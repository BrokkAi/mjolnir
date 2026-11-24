//go:build unix

// Port of mj-accounts/src/harness_login/acp_login.rs.

// Sign in to a profile through the harness's own ACP server.
//
// Mjolnir already installs an exact pinned harness for sessions. This module
// starts that same harness as an ACP agent, reads the authentication methods
// it advertises, and performs the one the person picks. A `terminal` method
// names a command the client is expected to run (Muse does this); a method
// without a command is either handled by ACP `authenticate` (Codex) or by a
// known terminal fallback for bridges that only describe the command in text
// (OpenCode).
//
// The command runs with inherited standard input and output because it is an
// interactive browser or device-code flow. That is why this belongs to the
// terminal owner rather than to the detached worker.

package accounts

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// initializeTimeout is how long a freshly installed bridge gets to answer
// `initialize`.
const initializeTimeout = 180 * time.Second

// authenticateTimeout is how long an ACP-driven browser/device-code flow may
// wait for the person.
const authenticateTimeout = 10 * time.Minute

// acpLoginOutcome is the result of an ACP sign-in (Rust's
// `acp_login::Outcome`).
type acpLoginOutcome interface {
	isAcpLoginOutcome()
}

type acpLoginOutcomeDeviceAuth struct{}

type acpLoginOutcomeSignedIn struct{ Method string }

// acpLoginOutcomeUnavailable: ACP login cannot run for this profile or
// target. The caller may use the harness CLI's own login command instead.
type acpLoginOutcomeUnavailable struct{ Value string }

// acpLoginOutcomeNoLogin: this profile authenticates without an interactive
// harness login.
type acpLoginOutcomeNoLogin struct{ Value string }

func (acpLoginOutcomeDeviceAuth) isAcpLoginOutcome()  {}
func (acpLoginOutcomeSignedIn) isAcpLoginOutcome()    {}
func (acpLoginOutcomeUnavailable) isAcpLoginOutcome() {}
func (acpLoginOutcomeNoLogin) isAcpLoginOutcome()     {}

// pinnedCLI is a pinned harness CLI and the environment prepared for it
// (Rust's `PinnedCli`). A managed harness keeps the installation lease alive
// while the child command runs; release gives it up (Rust's Drop).
type pinnedCLI struct {
	Command     string
	Environment map[string]string
	located     harnessinstall.Located
}

func (p *pinnedCLI) release() {
	if p.located.Managed != nil {
		p.located.Managed.Release()
		p.located.Managed = nil
	}
}

func usesAmbientCLI(profile *core.HarnessProfile) bool {
	return profile.Kind.NestedHome() && profile.Kind.IsOwnNestedHome(profile.Home)
}

// acpLogin signs in profile through the pinned harness's ACP methods (Rust's
// `acp_login::login`).
func acpLogin(ctx context.Context, profile *core.HarnessProfile, requestedMethod *string) (acpLoginOutcome, error) {
	scheme := profile.AuthScheme()
	if !scheme.SyncsLogin() {
		return acpLoginOutcomeNoLogin{Value: describeAuth(scheme)}, nil
	}
	name := profile.Kind.DisplayName()
	// A person's own installation keeps its config in their own nested home
	// (OpenCode/Muse). Pointing the pinned harness at it could migrate a
	// database Mjolnir does not own. Use their CLI for that case.
	if usesAmbientCLI(profile) {
		return acpLoginOutcomeUnavailable{Value: fmt.Sprintf(
			"this profile uses your own %s installation; Mjolnir signs in through that harness's installed CLI", name,
		)}, nil
	}
	fmt.Printf("Preparing %s…\n", name)
	located, launchEnvironment, err := locatedLogin(ctx, profile)
	if err != nil {
		return acpLoginOutcomeUnavailable{Value: fmt.Sprintf("the pinned %s could not be prepared: %v", name, err)}, nil
	}
	defer func() {
		if located.Managed != nil {
			located.Managed.Release()
		}
	}()
	if located.Entrypoint == nil {
		return acpLoginOutcomeUnavailable{Value: fmt.Sprintf("Mjolnir does not install %s", name)}, nil
	}
	entrypoint := *located.Entrypoint
	fmt.Printf("Using %s %s at %s.\n", sourceLabel(), name, entrypoint)
	cancellation := core.PreparationCancellation(ctx)
	signCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	type signedIn struct {
		outcome acpLoginOutcome
		err     error
	}
	done := make(chan signedIn, 1)
	go func() {
		outcome, err := signIn(signCtx, profile.Kind, profile.Home, located.Command, located.Args, entrypoint, launchEnvironment, requestedMethod)
		done <- signedIn{outcome: outcome, err: err}
	}()
	select {
	case result := <-done:
		return result.outcome, result.err
	case <-cancellation.Done():
		cancel()
		<-done
		return nil, errors.New("login cancelled")
	}
}

// sourceLabel is how a login names where its harness comes from.
func sourceLabel() string {
	if core.LocalHarnessRuntime() == core.HarnessRuntimePolicyImage {
		return "the image's"
	}
	return "the pinned"
}

// locatePinnedCLI locates the same harness a local session uses and returns
// its CLI with the environment that makes its shebangs and child tools use
// the matching Node version (Rust's `acp_login::pinned_cli`).
func locatePinnedCLI(ctx context.Context, profile *core.HarnessProfile) (pinnedCLI, error) {
	name := profile.Kind.DisplayName()
	if usesAmbientCLI(profile) {
		return pinnedCLI{}, fmt.Errorf("this profile uses your own %s installation; Mjolnir signs in through that harness's installed CLI", name)
	}
	located, environment, err := locatedLogin(ctx, profile)
	if err != nil {
		return pinnedCLI{}, err
	}
	if located.CLIEntrypoint == nil {
		if located.Managed != nil {
			located.Managed.Release()
		}
		return pinnedCLI{}, fmt.Errorf("Mjolnir does not install %s", name)
	}
	return pinnedCLI{Command: *located.CLIEntrypoint, Environment: environment, located: located}, nil
}

func locatedLogin(ctx context.Context, profile *core.HarnessProfile) (harnessinstall.Located, map[string]string, error) {
	_, statErr := os.Stat(profile.Home)
	createdHome := statErr != nil
	if err := os.MkdirAll(profile.Home, 0o777); err != nil {
		return harnessinstall.Located{}, nil, fmt.Errorf("create the harness profile home %s for sign-in: %w", profile.Home, err)
	}
	if createdHome {
		if err := os.Chmod(profile.Home, 0o700); err != nil {
			return harnessinstall.Located{}, nil, fmt.Errorf("restrict permissions on the harness profile home %s: %w", profile.Home, err)
		}
	}
	environment := configuredLoginEnvironment(profile)
	// A login runs in the person's terminal, not in an isolated worker. Its
	// ambient display, browser, provider-key and tool environment is already
	// the one the CLI fallback inherits, so layer profile overrides directly
	// on it. Starting the account's interactive login shell here would
	// repeat potentially expensive startup before every sign-in without
	// producing a second fact the login path needs.
	launchEnvironment := loginAmbientEnvironment()
	maps.Copy(launchEnvironment, environment)
	policy := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(profile.Kind, policy)
	located, err := harnessinstall.Locate(ctx, core.LocalHarnessRuntime(), profile.Kind, policy, command, args, launchEnvironment)
	if err != nil {
		return harnessinstall.Located{}, nil, err
	}
	maps.Copy(launchEnvironment, located.Environment)
	return located, launchEnvironment, nil
}

// signIn is the core of ACP sign-in. Tests drive this directly with a
// scripted bridge.
func signIn(ctx context.Context, kind core.HarnessKind, home string, command string, args []string, entrypoint string, environment map[string]string, requestedMethod *string) (acpLoginOutcome, error) {
	name := kind.DisplayName()
	fmt.Println("Starting sign-in…")
	session, err := acpSessionStart(ctx, kind, home, command, args, environment)
	if err != nil {
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		return acpLoginOutcomeUnavailable{Value: fmt.Sprintf("the %s ACP bridge could not start: %v", name, err)}, nil
	}
	// Every early return still takes the bridge's process group with it.
	defer session.close()
	var method authMethod
	switch {
	case len(session.methods) == 0:
		session.shutdown()
		return acpLoginOutcomeUnavailable{Value: "the ACP bridge advertised no login methods"}, nil
	case kind == core.HarnessKindCodex && requestedMethod == nil && !hasDesktopSession(environment) && apiKeyPresent(environment):
		// Headless with an exported key: use the key method rather than
		// waiting for a browser that cannot appear.
		index := slices.IndexFunc(session.methods, func(method authMethod) bool { return method.id == "api-key" })
		if index >= 0 {
			method = session.methods[index]
		} else if selected, err := selectAuthMethod(session.methods, requestedMethod); err == nil {
			method = *selected
		} else {
			return nil, errors.New("the ACP bridge advertised no usable login method")
		}
	default:
		selected, err := selectAuthMethod(session.methods, requestedMethod)
		if err != nil {
			session.shutdown()
			return nil, err
		}
		method = *selected
	}
	if kind == core.HarnessKindCodex && !hasDesktopSession(environment) &&
		quotaASCIILower(method.id) == "chat-gpt" && !apiKeyPresent(environment) {
		session.shutdown()
		return acpLoginOutcomeDeviceAuth{}, nil
	}
	fmt.Printf("Signing in with %s (%s).\n", method.name, method.id)
	terminal, err := method.terminalCommand(kind, home, entrypoint, environment)
	if err != nil {
		session.shutdown()
		return nil, err
	}
	if terminal != nil {
		session.shutdown()
		restoreDefaultSIGINT()
		rendered := renderCommand(terminal)
		fmt.Printf("Running `%s`.\n", rendered)
		childEnvironment := maps.Clone(environment)
		if childEnvironment == nil {
			childEnvironment = map[string]string{}
		}
		maps.Copy(childEnvironment, terminal.env)
		login := loginCommand(terminal.command, terminal.args, loginEnvironmentList(childEnvironment))
		login.Dir = home
		login.Stdin, login.Stdout, login.Stderr = os.Stdin, os.Stdout, os.Stderr
		if err := core.SpawnChild(login); err != nil {
			return nil, fmt.Errorf("run `%s` for the %s login: %w", rendered, name, err)
		}
		waited := make(chan error, 1)
		go func() { waited <- login.Wait() }()
		select {
		case err := <-waited:
			var exit *exec.ExitError
			if err != nil && !errors.As(err, &exit) {
				return nil, fmt.Errorf("run `%s` for the %s login: %w", rendered, name, err)
			}
		case <-ctx.Done():
			// The login command is the person's: it is left to finish.
			return nil, ctx.Err()
		}
		if !login.ProcessState.Success() {
			return nil, fmt.Errorf("`%s` exited with %s; the %s login did not complete", rendered, loginExitStatus(login.ProcessState), name)
		}
		return acpLoginOutcomeSignedIn{Method: method.id}, nil
	}
	if len(method.envVars) != 0 {
		var missing []string
		for _, variable := range method.envVars {
			if value, ok := environment[variable.name]; !variable.optional && (!ok || strings.TrimSpace(value) == "") {
				missing = append(missing, variable.name)
			}
		}
		if len(missing) != 0 {
			session.shutdown()
			return nil, fmt.Errorf("the %s sign-in method %s needs %s in the environment before it can run", name, method.id, strings.Join(missing, ", "))
		}
	}
	authenticated := make(chan error, 1)
	go func() { authenticated <- session.authenticate(ctx, method.id) }()
	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt)
	timer := time.NewTimer(authenticateTimeout)
	var result error
	select {
	case result = <-authenticated:
		signal.Stop(interrupt)
		timer.Stop()
	case <-timer.C:
		signal.Stop(interrupt)
		session.shutdown()
		<-authenticated
		return nil, errors.New("the ACP sign-in flow timed out")
	case <-interrupt:
		signal.Stop(interrupt)
		timer.Stop()
		session.shutdown()
		<-authenticated
		return nil, errors.New("the ACP sign-in flow was cancelled")
	case <-ctx.Done():
		signal.Stop(interrupt)
		timer.Stop()
		session.close()
		<-authenticated
		return nil, ctx.Err()
	}
	session.shutdown()
	if result != nil {
		if isHeadlessBrowserError(result) {
			return acpLoginOutcomeUnavailable{Value: "no browser is available for ACP sign-in"}, nil
		}
		return nil, fmt.Errorf("complete the %s ACP sign-in method %s: %w", name, method.id, result)
	}
	return acpLoginOutcomeSignedIn{Method: method.id}, nil
}

func describeAuth(scheme core.AuthScheme) string {
	switch scheme := scheme.(type) {
	case core.AuthSchemeApiKey:
		return fmt.Sprintf("this profile authenticates with %s, so it has no interactive login", scheme.EnvKey)
	case core.AuthSchemeInlineApiKey:
		return "this profile inlines its provider API key, so it has no interactive login"
	case core.AuthSchemeAwsCredentialChain:
		return "this profile authenticates with the AWS credential chain, so it has no interactive login"
	case core.AuthSchemeNoAuthentication:
		return "this profile's built-in provider does not require an interactive login"
	default:
		return "this profile has no ACP login method"
	}
}

type authMethod struct {
	id          string
	name        string
	description *string
	terminal    *terminalHint
	// envVars are the variables an `env_var` method needs the agent to
	// receive.
	envVars []authEnvVar
}

type authEnvVar struct {
	name     string
	optional bool
}

// terminalHint is what a bridge says about a `terminal` auth method.
type terminalHint struct {
	command *string
	args    []string
	env     map[string]string
}

type terminalCommand struct {
	command string
	args    []string
	env     map[string]string
}

// terminalCommand is the command that performs this method interactively,
// or nil when the method is completed by ACP `authenticate`.
func (m *authMethod) terminalCommand(kind core.HarnessKind, home string, entrypoint string, environment map[string]string) (*terminalCommand, error) {
	var resolved *terminalCommand
	if hint := m.terminal; hint != nil {
		switch {
		case hint.command != nil:
			resolved = &terminalCommand{command: *hint.command, args: slices.Clone(hint.args), env: maps.Clone(hint.env)}
		default:
			if fallback := knownTerminalFallback(kind, m.id, entrypoint, environment); fallback != nil {
				resolved = fallback
			} else if len(hint.args) == 0 {
				return nil, fmt.Errorf("the %s ACP bridge advertised a terminal login method %s without a command", kind.DisplayName(), m.id)
			} else {
				resolved = &terminalCommand{command: entrypoint, args: slices.Clone(hint.args), env: maps.Clone(hint.env)}
			}
		}
	} else {
		resolved = knownTerminalFallback(kind, m.id, entrypoint, environment)
	}
	if resolved != nil && kind == core.HarnessKindAntigravity &&
		!slices.ContainsFunc(resolved.args, func(arg string) bool { return arg == "--gemini-dir" || arg == "--gemini_dir" }) {
		resolved.args = append(resolved.args, kind.HomeCLIArgs(home)...)
	}
	return resolved, nil
}

// knownTerminalFallback covers bridges whose terminal action is named only
// in the method description.
//
// OpenCode 2.1.2 advertises `opencode-login` with the description "Run
// `opencode auth login` in the terminal" and no `terminal-auth` metadata.
func knownTerminalFallback(kind core.HarnessKind, method string, entrypoint string, environment map[string]string) *terminalCommand {
	var args []string
	switch {
	case kind == core.HarnessKindOpenCode && method == "opencode-login":
		args = []string{"auth", "login", "--standalone"}
	case kind == core.HarnessKindMuse && method == "muse-login":
		args = []string{"login"}
	// Grok's ACP bridge names an agent method, but its usable sign-in is the
	// CLI's own login. Running it is safer than leaving `authenticate` to
	// open a browser the target cannot show.
	case kind == core.HarnessKindGrok && method == "grok.com":
		if hasDesktopSession(environment) {
			args = []string{"login"}
		} else {
			args = []string{"login", "--device-auth"}
		}
	default:
		return nil
	}
	return &terminalCommand{command: entrypoint, args: args, env: map[string]string{}}
}

// selectAuthMethod is Rust's `acp_login::select` (a Go keyword).
func selectAuthMethod(methods []authMethod, requested *string) (*authMethod, error) {
	if requested != nil {
		for i := range methods {
			if methods[i].id == *requested {
				return &methods[i], nil
			}
		}
		return nil, fmt.Errorf("the ACP bridge has no login method %q; available: %s", *requested, available(methods))
	}
	for i := range methods {
		if methods[i].terminal != nil {
			return &methods[i], nil
		}
	}
	for i := range methods {
		if looksInteractive(methods[i].id) {
			return &methods[i], nil
		}
	}
	if len(methods) == 0 {
		return nil, errors.New("the ACP bridge advertised no login methods; this harness has no ACP sign-in")
	}
	return &methods[0], nil
}

func available(methods []authMethod) string {
	if len(methods) == 0 {
		return "none"
	}
	ids := make([]string, len(methods))
	for i := range methods {
		ids[i] = methods[i].id
	}
	return strings.Join(ids, ", ")
}

func looksInteractive(id string) bool {
	id = quotaASCIILower(id)
	for _, needle := range []string{"login", "chat-gpt", "chatgpt", "oauth", "browser"} {
		if strings.Contains(id, needle) {
			return true
		}
	}
	return false
}

// hasDesktopSession is whether this login has a desktop session for a
// browser callback or opener.
func hasDesktopSession(environment map[string]string) bool {
	set := func(name string) bool {
		value, ok := environment[name]
		return ok && strings.TrimSpace(value) != ""
	}
	if runtime.GOOS == "darwin" || runtime.GOOS == "windows" {
		return !set("SSH_CONNECTION") && !set("SSH_TTY") && !set("SSH_CLIENT")
	}
	return set("DISPLAY") || set("WAYLAND_DISPLAY")
}

// isHeadlessBrowserError: agy refuses `authenticate` with a generic -32603
// when it cannot reach a browser; its message is the only signal it gives
// (agy 1.5.8).
func isHeadlessBrowserError(err error) bool {
	return strings.Contains(quotaASCIILower(err.Error()), "no browser is available")
}

func apiKeyPresent(environment map[string]string) bool {
	for _, name := range []string{"CODEX_API_KEY", "OPENAI_API_KEY"} {
		if value, ok := environment[name]; ok && strings.TrimSpace(value) != "" {
			return true
		}
	}
	return false
}

// restoreDefaultSIGINT undoes any SIGINT handling this process installed
// for a sign-in. A terminal login command and the CLI fallback expect Ctrl+C
// to terminate this process, so restore the default before handing over to
// them.
func restoreDefaultSIGINT() {
	signal.Reset(os.Interrupt)
}

func renderCommand(command *terminalCommand) string {
	return strings.Join(append([]string{command.command}, command.args...), " ")
}

type acpSession struct {
	ownsGroup bool
	child     *exec.Cmd
	stdin     io.WriteCloser
	stdout    *bufio.Reader
	nextID    uint64
	methods   []authMethod
	// stdoutFile is the read end of the bridge's standard output, closed on
	// shutdown so a read blocked on it returns.
	stdoutFile *os.File
	// exited is closed once the bridge has exited and been reaped.
	exited chan struct{}
}

func acpSessionStart(ctx context.Context, kind core.HarnessKind, cwd string, command string, args []string, environment map[string]string) (*acpSession, error) {
	name := kind.DisplayName()
	child := loginCommand(command, args, loginEnvironmentList(environment))
	child.Dir = cwd
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return nil, fmt.Errorf("start the %s ACP bridge %s: %w", name, command, err)
	}
	stdoutRead, stdoutWrite, err := os.Pipe()
	if err != nil {
		stdinRead.Close()
		stdinWrite.Close()
		return nil, fmt.Errorf("start the %s ACP bridge %s: %w", name, command, err)
	}
	child.Stdin, child.Stdout, child.Stderr = stdinRead, stdoutWrite, os.Stderr
	// A shell bootstrap or npx can leave a native server in the bridge's
	// process group. Own that group so shutdown reaches it.
	_, terminal := os.LookupEnv("MJ_LOGIN_TERMINAL")
	ownsGroup := !terminal
	if ownsGroup {
		child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	}
	err = core.SpawnChild(child)
	stdinRead.Close()
	stdoutWrite.Close()
	if err != nil {
		stdinWrite.Close()
		stdoutRead.Close()
		return nil, fmt.Errorf("start the %s ACP bridge %s: %w", name, command, err)
	}
	session := &acpSession{
		ownsGroup:  ownsGroup,
		child:      child,
		stdin:      stdinWrite,
		stdout:     bufio.NewReader(stdoutRead),
		nextID:     1,
		stdoutFile: stdoutRead,
		exited:     make(chan struct{}),
	}
	go func() {
		_ = child.Wait()
		close(session.exited)
	}()
	initialized := make(chan error, 1)
	go func() { initialized <- session.initialize(ctx) }()
	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt)
	defer signal.Stop(interrupt)
	timer := time.NewTimer(initializeTimeout)
	defer timer.Stop()
	select {
	case err := <-initialized:
		if err == nil {
			return session, nil
		}
		session.shutdown()
		return nil, err
	case <-timer.C:
		session.shutdown()
		<-initialized
		return nil, fmt.Errorf("the %s ACP bridge did not answer initialize within %d seconds", name, int(initializeTimeout/time.Second))
	case <-interrupt:
		session.shutdown()
		<-initialized
		return nil, fmt.Errorf("the %s ACP bridge initialization was cancelled", name)
	case <-ctx.Done():
		session.close()
		<-initialized
		return nil, ctx.Err()
	}
}

func (s *acpSession) initialize(ctx context.Context) error {
	params := jsonvalue.Object{
		{Key: "protocolVersion", Value: int64(1)},
		{Key: "clientCapabilities", Value: jsonvalue.Object{}},
		{Key: "clientInfo", Value: jsonvalue.Object{
			{Key: "name", Value: "mj"},
			{Key: "title", Value: "Mjolnir"},
			{Key: "version", Value: core.PackageVersion},
		}},
	}
	result, err := s.request(ctx, "initialize", params)
	if err != nil {
		return fmt.Errorf("initialize the ACP bridge: %w", err)
	}
	s.methods = parseMethods(result)
	return nil
}

func (s *acpSession) authenticate(ctx context.Context, methodID string) error {
	_, err := s.request(ctx, "authenticate", jsonvalue.Object{{Key: "methodId", Value: methodID}})
	return err
}

func (s *acpSession) request(ctx context.Context, method string, params any) (any, error) {
	id := s.nextID
	s.nextID++
	request := jsonvalue.Object{
		{Key: "jsonrpc", Value: "2.0"},
		{Key: "id", Value: id},
		{Key: "method", Value: method},
		{Key: "params", Value: params},
	}
	line, err := jsonvalue.Marshal(request)
	if err != nil {
		return nil, err
	}
	if _, err := s.stdin.Write(append(line, '\n')); err != nil {
		return nil, fmt.Errorf("send an ACP request: %w", err)
	}
	for {
		line, err := s.stdout.ReadString('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return nil, fmt.Errorf("read an ACP response: %w", err)
		}
		if line == "" {
			return nil, fmt.Errorf("the ACP bridge exited before answering %s", method)
		}
		trimmed := strings.TrimSpace(line)
		message, parseErr := jsonvalue.Parse([]byte(trimmed))
		if parseErr != nil {
			// ACP reserves standard output for protocol frames. A malformed
			// line is therefore the bridge's bug; report it instead of
			// guessing which frame it meant.
			return nil, fmt.Errorf("the ACP bridge wrote a non-protocol line to standard output: %s", trimmed)
		}
		if serverMethod, ok := quotaJSONGetString(message, "method"); ok {
			// A request from the agent has its own id space. Check this
			// before matching our response id: both sides commonly start at
			// 1, and treating the request as the response would report a
			// successful login that never happened.
			if serverID, ok := quotaJSONGet(message, "id"); ok {
				response, err := jsonvalue.Marshal(jsonvalue.Object{
					{Key: "jsonrpc", Value: "2.0"},
					{Key: "id", Value: serverID},
					{Key: "error", Value: jsonvalue.Object{
						{Key: "code", Value: int64(-32601)},
						{Key: "message", Value: fmt.Sprintf("Mjolnir sign-in cannot provide the client method %s", serverMethod)},
					}},
				})
				if err != nil {
					return nil, err
				}
				if _, err := s.stdin.Write(append(response, '\n')); err != nil {
					return nil, err
				}
				return nil, fmt.Errorf("the ACP bridge asked for client method %s during sign-in", serverMethod)
			}
			// A notification is progress or logging; keep reading.
			continue
		}
		if responseID, ok := quotaJSONU64(quotaJSONIndex(message, "id")); ok && responseID == id {
			if failure, ok := quotaJSONGet(message, "error"); ok {
				text, err := jsonvalue.Marshal(failure)
				if err != nil {
					return nil, err
				}
				return nil, fmt.Errorf("the ACP bridge rejected %s: %s", method, text)
			}
			result, _ := quotaJSONGet(message, "result")
			return result, nil
		}
	}
}

// hasExited is whether the bridge has exited and been reaped.
func (s *acpSession) hasExited() bool {
	select {
	case <-s.exited:
		return true
	default:
		return false
	}
}

func (s *acpSession) shutdown() {
	// Closing the protocol stream gives the bridge a chance to finish a
	// credential write before the group is signaled.
	_ = s.stdin.Close()
	pid := s.child.Process.Pid
	if s.ownsGroup {
		// The group outlives its leader: a descendant left in it is
		// signaled even when the bridge itself has exited.
		core.TerminateProcessGroup(pid, syscall.SIGTERM)
	} else if !s.hasExited() {
		// The daemon owns this entire terminal group; leave its descendants
		// in that group for connection teardown.
		_ = s.child.Process.Signal(syscall.SIGTERM)
	}
	timer := time.NewTimer(5 * time.Second)
	defer timer.Stop()
	select {
	case <-s.exited:
	case <-timer.C:
		if s.ownsGroup {
			core.TerminateProcessGroup(pid, syscall.SIGKILL)
		}
		_ = s.child.Process.Kill()
		<-s.exited
	}
	_ = s.stdoutFile.Close()
}

// close takes the bridge's process group with it on every early return
// (Rust's Drop). It distinguishes a live child from one already reaped, so a
// recycled pid is never signaled.
func (s *acpSession) close() {
	if !s.hasExited() {
		if s.ownsGroup {
			core.TerminateProcessGroup(s.child.Process.Pid, syscall.SIGKILL)
		}
		_ = s.child.Process.Kill()
	}
	_ = s.stdin.Close()
	_ = s.stdoutFile.Close()
}

func parseMethods(result any) []authMethod {
	values, ok := quotaJSONIndex(result, "authMethods").([]any)
	if !ok {
		return nil
	}
	var methods []authMethod
	for _, value := range values {
		if method := parseMethod(value); method != nil {
			methods = append(methods, *method)
		}
	}
	return methods
}

func parseMethod(value any) *authMethod {
	id, ok := quotaJSONGetString(value, "id")
	if !ok {
		return nil
	}
	name, ok := quotaJSONGetString(value, "name")
	if !ok {
		name = id
	}
	method := &authMethod{id: id, name: name}
	if description, ok := quotaJSONGetString(value, "description"); ok {
		method.description = &description
	}
	kind, _ := quotaJSONGetString(value, "type")
	if kind == "terminal" {
		hint := &terminalHint{env: map[string]string{}}
		meta, hasMeta := quotaJSONPointer(value, "/_meta/terminal-auth")
		if hasMeta {
			if command, ok := quotaJSONGetString(meta, "command"); ok {
				hint.command = &command
			}
		}
		args, hasArgs := any(nil), false
		if hasMeta {
			args, hasArgs = quotaJSONGet(meta, "args")
		}
		if !hasArgs {
			args, hasArgs = quotaJSONGet(value, "args")
		}
		if items, ok := args.([]any); hasArgs && ok {
			for _, item := range items {
				if arg, ok := item.(string); ok {
					hint.args = append(hint.args, arg)
				}
			}
		}
		if env, ok := quotaJSONIndex(value, "env").(jsonvalue.Object); ok {
			for _, member := range env {
				if text, ok := member.Value.(string); ok {
					hint.env[member.Key] = text
				}
			}
		}
		method.terminal = hint
	}
	if kind == "env_var" {
		if vars, ok := quotaJSONIndex(value, "vars").([]any); ok {
			for _, variable := range vars {
				name, ok := quotaJSONGetString(variable, "name")
				if !ok {
					continue
				}
				optional, _ := quotaJSONIndex(variable, "optional").(bool)
				method.envVars = append(method.envVars, authEnvVar{name: name, optional: optional})
			}
		}
	}
	return method
}
