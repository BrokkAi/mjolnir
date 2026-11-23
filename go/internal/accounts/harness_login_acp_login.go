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
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"strings"
	"time"

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
	}
}

func usesAmbientCLI(profile *core.HarnessProfile) bool {
	return profile.Kind.NestedHome() && profile.Kind.IsOwnNestedHome(profile.Home)
}

// acpLogin signs in profile through the pinned harness's ACP methods (Rust's
// `acp_login::login`).
func acpLogin(ctx context.Context, profile *core.HarnessProfile, requestedMethod *string) (acpLoginOutcome, error) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs login")
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
	panic("unported: mj-accounts/src/harness_login/acp_login.rs pinned_cli")
}

func locatedLogin(ctx context.Context, profile *core.HarnessProfile) (harnessinstall.Located, map[string]string, error) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs located_login")
}

// signIn is the core of ACP sign-in. Tests drive this directly with a
// scripted bridge.
func signIn(ctx context.Context, kind core.HarnessKind, home string, command string, args []string, entrypoint string, environment map[string]string, requestedMethod *string) (acpLoginOutcome, error) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs sign_in")
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
	panic("unported: mj-accounts/src/harness_login/acp_login.rs AuthMethod::terminal_command")
}

// knownTerminalFallback covers bridges whose terminal action is named only
// in the method description.
//
// OpenCode 2.1.2 advertises `opencode-login` with the description "Run
// `opencode auth login` in the terminal" and no `terminal-auth` metadata.
func knownTerminalFallback(kind core.HarnessKind, method string, entrypoint string, environment map[string]string) *terminalCommand {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs known_terminal_fallback")
}

// selectAuthMethod is Rust's `acp_login::select` (a Go keyword).
func selectAuthMethod(methods []authMethod, requested *string) (*authMethod, error) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs select")
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
}

func acpSessionStart(ctx context.Context, kind core.HarnessKind, cwd string, command string, args []string, environment map[string]string) (*acpSession, error) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs AcpSession::start")
}

func (s *acpSession) initialize(ctx context.Context) error {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs AcpSession::initialize")
}

func (s *acpSession) authenticate(ctx context.Context, methodID string) error {
	_, err := s.request(ctx, "authenticate", map[string]any{"methodId": methodID})
	return err
}

func (s *acpSession) request(ctx context.Context, method string, params any) (any, error) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs AcpSession::request")
}

func (s *acpSession) shutdown(ctx context.Context) {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs AcpSession::shutdown")
}

// close takes the bridge's process group with it on every early return
// (Rust's Drop). It distinguishes a live child from one already reaped, so a
// recycled pid is never signaled.
func (s *acpSession) close() {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs AcpSession drop")
}

func parseMethods(result any) []authMethod {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs parse_methods")
}

func parseMethod(value any) *authMethod {
	panic("unported: mj-accounts/src/harness_login/acp_login.rs parse_method")
}
