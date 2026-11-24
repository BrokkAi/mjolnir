// Port of mj-accounts/src/harness_login.rs.

// Harness login driver shared by the local CLI and daemon terminal sessions.
// The ACP sign-in it tries first is in harness_login_acp_login.go (Unix
// only, as in Rust); the platform parts of the driver are in
// harness_login_unix.go and harness_login_windows.go.

package accounts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LoginOptions are the options for the harness login driver. The zero value
// is Rust's Default.
type LoginOptions struct {
	SetupToken bool
	CLI        bool
	Method     *string
}

// Login runs login with the caller's terminal standard streams. Call it in
// a terminal-owning process, never directly on the daemon event loop. It
// returns the login command's exit code.
func Login(ctx context.Context, profileID string, profile *core.HarnessProfile, options LoginOptions) (int, error) {
	if options.SetupToken {
		if err := storeClaudeSetupToken(ctx, profileID, profile); err != nil {
			return 0, err
		}
		return 0, nil
	}
	// OpenCode keeps its logins in its database, so its fingerprint is the
	// logins read from there, and a live session reloads to take them up.
	_, stored := profile.AuthScheme().(core.AuthSchemeNativeLoginStore)
	before, err := profileCredentialFingerprint(profile)
	if err != nil {
		return 0, err
	}
	plan, signedIn, err := prepareLogin(ctx, profileID, profile, options, before, stored)
	if err != nil {
		return 0, err
	}
	if signedIn {
		return 0, nil
	}
	defer plan.release()

	configuredProgram, arguments, err := core.LoginCommand(profile)
	if err != nil {
		return 0, fmt.Errorf("profile %q: %w", profileID, err)
	}
	environment := plan.environment(profile)
	arguments = plan.arguments(profile, arguments, environment)
	program := plan.program(configuredProgram, environment)
	fmt.Printf("Running `%s %s` against %s.\n", program, strings.Join(arguments, " "), profile.Home)
	if core.PreparationCancellation(ctx).Err() != nil {
		return 0, errors.New("login cancelled")
	}
	var removed []string
	if !profile.Kind.KeepsLoginInHome(core.HarnessHostCurrent()) {
		removed = append(removed, profile.Kind.HomeEnv())
	}
	command := loginCommand(program, arguments, loginInheritedEnvironment(environment, removed))
	command.Stdin, command.Stdout, command.Stderr = os.Stdin, os.Stdout, os.Stderr
	if err := core.SpawnChild(command); err != nil {
		return 0, fmt.Errorf("run `%s %s` for profile %s: %w",
			program, strings.Join(arguments, " "), profileID, loginSpawnError(err, program, profile.Kind, profileID))
	}
	waited := make(chan error, 1)
	go func() { waited <- command.Wait() }()
	cancellation := core.PreparationCancellation(ctx)
	var waitErr error
	select {
	case waitErr = <-waited:
	case <-cancellation.Done():
		if err := command.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
			return 0, fmt.Errorf("stop login command: %w", err)
		}
		<-waited
		return 0, errors.New("login cancelled")
	case <-ctx.Done():
		_ = command.Process.Kill()
		<-waited
		return 0, ctx.Err()
	}
	var exit *exec.ExitError
	if waitErr != nil && !errors.As(waitErr, &exit) {
		return 0, waitErr
	}

	if err := reportLoginChange(profileID, profile, before, stored); err != nil {
		return 0, err
	}
	code := command.ProcessState.ExitCode()
	if code < 0 {
		code = 1
	}
	return code, nil
}

func configuredLoginEnvironment(profile *core.HarnessProfile) map[string]string {
	environment := maps.Clone(profile.Environment.Resolved())
	if environment == nil {
		environment = map[string]string{}
	}
	profile.Kind.ConfigureProfileHomeEnvironment(profile.Home, core.HarnessHostCurrent(), environment)
	return environment
}

// loginAmbientEnvironment is this process's environment, its variables that
// are not UTF-8 left out (Rust's `std::env::vars_os` filtered through
// `into_string`).
func loginAmbientEnvironment() map[string]string {
	environment := map[string]string{}
	for _, entry := range os.Environ() {
		name, value, ok := strings.Cut(entry, "=")
		if ok && utf8.ValidString(name) && utf8.ValidString(value) {
			environment[name] = value
		}
	}
	return environment
}

// loginEnvironmentList is environment as exec.Cmd.Env takes it, sorted. An
// empty map is a non-nil empty list, so nothing is inherited.
func loginEnvironmentList(environment map[string]string) []string {
	list := make([]string, 0, len(environment))
	for _, name := range slices.Sorted(maps.Keys(environment)) {
		list = append(list, name+"="+environment[name])
	}
	return list
}

// loginInheritedEnvironment is this process's environment with environment
// layered over it and removed taken out (Rust's `envs` and `env_remove` on
// an inheriting Command).
func loginInheritedEnvironment(environment map[string]string, removed []string) []string {
	var list []string
	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		if _, overridden := environment[name]; overridden || slices.Contains(removed, name) {
			continue
		}
		list = append(list, entry)
	}
	for _, name := range slices.Sorted(maps.Keys(environment)) {
		if !slices.Contains(removed, name) {
			list = append(list, name+"="+environment[name])
		}
	}
	if list == nil {
		list = []string{}
	}
	return list
}

// loginCommand is program run with args in exactly the environment list
// environment. A program named without a directory is looked up on that
// environment's PATH, as the child would look it up, not on this
// process's; one found nowhere fails to start with exec.ErrNotFound.
func loginCommand(program string, args []string, environment []string) *exec.Cmd {
	command := exec.Command(program, args...)
	command.Env = environment
	if strings.ContainsRune(program, '/') || strings.ContainsRune(program, filepath.Separator) {
		command.Path = program
		command.Err = nil
		return command
	}
	path := ""
	for _, entry := range environment {
		if value, ok := strings.CutPrefix(entry, "PATH="); ok {
			path = value
		}
	}
	for _, directory := range filepath.SplitList(path) {
		if directory == "" {
			directory = "."
		}
		if found, err := exec.LookPath(filepath.Join(directory, program)); err == nil {
			command.Path = found
			command.Err = nil
			return command
		}
	}
	command.Err = &exec.Error{Name: program, Err: exec.ErrNotFound}
	return command
}

// profileCredentialFingerprint is the credential fingerprint a login is
// expected to change; nil when the profile holds no login.
func profileCredentialFingerprint(profile *core.HarnessProfile) (*string, error) {
	if _, stored := profile.AuthScheme().(core.AuthSchemeNativeLoginStore); stored {
		logins, err := ProfileLogins(profile)
		if err != nil {
			return nil, err
		}
		if logins == nil {
			return nil, nil
		}
		fingerprint := core.CredentialFingerprint(logins)
		return &fingerprint, nil
	}
	snapshot, _, err := core.ReadCredentialFile(profile.Kind, profile.AuthenticationMarker())
	if err != nil {
		return nil, err
	}
	if !snapshot.Present {
		return nil, nil
	}
	return &snapshot.Fingerprint, nil
}

func reportLoginChange(profileID string, profile *core.HarnessProfile, before *string, stored bool) error {
	after, err := profileCredentialFingerprint(profile)
	if err != nil {
		return err
	}
	switch {
	case after != nil && !quotaEqualOptional(after, before) && stored:
		fmt.Printf("Credentials updated for profile %s. Live sessions reload with them within about a minute, once idle, while the Mjolnir daemon is running.\n", profileID)
	case after != nil && !quotaEqualOptional(after, before):
		fmt.Printf("Credentials updated for profile %s. Live sessions pick them up within about a minute while the Mjolnir daemon is running.\n", profileID)
	default:
		fmt.Printf("Credentials for profile %s are unchanged.\n", profileID)
	}
	return nil
}

// loginSpawnError says which program is missing and how to get it, rather
// than the bare "No such file or directory" the operating system reports.
func loginSpawnError(err error, program string, kind core.HarnessKind, profileID string) error {
	if !errors.Is(err, fs.ErrNotExist) && !errors.Is(err, exec.ErrNotFound) {
		return err
	}
	return fmt.Errorf(
		"`%s` is not installed or is not on PATH, so the %s login cannot run. %s, then run `mj login --profile %s` again.",
		program, kind.DisplayName(), kind.InstallAdvice(), profileID,
	)
}

// storeClaudeSetupToken mints a long-lived Claude subscription token and
// stores it for the profile.
//
// Claude Code has no early-refresh command, and its `/login` grant rotates:
// a container copy that reaches expiry at the same instant as the host
// spends a single-use refresh token the host has already spent, and the
// container's turn dies. `claude setup-token` mints a one-year token that
// never rotates, and Claude Code reads it from `CLAUDE_CODE_OAUTH_TOKEN`
// ahead of the credentials file, so there is nothing left to race.
func storeClaudeSetupToken(ctx context.Context, profileID string, profile *core.HarnessProfile) error {
	if profile.Kind != core.HarnessKindClaude {
		return fmt.Errorf("--setup-token applies to Claude profiles only; profile %s is a %s profile", profileID, profile.Kind.DisplayName())
	}
	tokenPath := core.ClaudeOauthTokenPath(profileID)

	fmt.Printf("Running `claude setup-token` against %s.\n", profile.Home)
	program, environment, release := setupTokenProgram(ctx, profile)
	defer release()
	command := loginCommand(program, []string{"setup-token"}, loginInheritedEnvironment(environment, nil))
	output, err := core.RunCapturingStdoutCancellable(ctx, command)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound) {
			return loginSpawnError(fs.ErrNotExist, program, core.HarnessKindClaude, profileID)
		}
		return err
	}
	if !output.Status.Success() {
		return fmt.Errorf("`claude setup-token` exited with %s for profile %s", loginExitStatus(output.Status), profileID)
	}

	stdout, err := loginFromUTF8(output.Stdout)
	if err != nil {
		return fmt.Errorf("`claude setup-token` printed non-UTF-8 output: %w", err)
	}
	token, ok := extractSetupToken(stdout)
	if !ok {
		return fmt.Errorf("`claude setup-token` printed no token for profile %s", profileID)
	}
	if err := core.WriteClaudeOauthToken(tokenPath, []byte(token)); err != nil {
		return err
	}

	verifyEnvironment := maps.Clone(environment)
	verifyEnvironment[core.ClaudeOauthTokenEnv] = token
	command = loginCommand(program, []string{"auth", "status"}, loginInheritedEnvironment(verifyEnvironment, nil))
	verified, err := core.RunCapturingStdoutCancellable(ctx, command)
	if err != nil {
		return fmt.Errorf("run `claude auth status`: %w", err)
	}
	if !verified.Status.Success() {
		return fmt.Errorf(
			"the stored token did not authenticate: `claude auth status` exited with %s. The token is at %s; remove it to go back to the synced credentials file.",
			loginExitStatus(verified.Status), tokenPath,
		)
	}
	if method, ok := reportedAuthMethod(verified.Stdout); ok && method != "oauth_token" {
		return fmt.Errorf(
			"Claude Code authenticated with %q rather than the stored token. The token is at %s; remove it to go back to the synced credentials file.",
			method, tokenPath,
		)
	}

	fmt.Printf("Stored a long-lived Claude token for profile %s at %s.\n", profileID, tokenPath)
	fmt.Printf("New and resumed sessions of this profile run with %s set, so they no longer race the host over a rotating login.\n", core.ClaudeOauthTokenEnv)
	return nil
}

// reportedAuthMethod is how `claude auth status` says it authenticated, when
// it says so.
//
// Exit code alone proves little: `claude auth status` reports success for
// any value of `CLAUDE_CODE_OAUTH_TOKEN`. What is worth confirming is that
// Claude Code read the variable and gave it precedence, which the reported
// method says. A build whose output carries no method is not a failure to
// report.
func reportedAuthMethod(stdout []byte) (string, bool) {
	var payload map[string]any
	if json.Unmarshal(stdout, &payload) != nil {
		return "", false
	}
	method, ok := payload["authMethod"].(string)
	return method, ok
}

// extractSetupToken is the token in `claude setup-token` output.
//
// The command prints instructions as well as the token, so the last
// whitespace-free `sk-ant-oat01-` line wins. A future format that stops
// using that prefix still leaves the token as the last thing printed, so the
// last non-empty line is the fallback.
func extractSetupToken(stdout string) (string, bool) {
	var candidates []string
	for _, line := range strings.Split(stdout, "\n") {
		if line = strings.TrimSpace(line); line != "" {
			candidates = append(candidates, line)
		}
	}
	for i := len(candidates) - 1; i >= 0; i-- {
		if strings.HasPrefix(candidates[i], "sk-ant-oat01-") && !strings.ContainsFunc(candidates[i], unicode.IsSpace) {
			return candidates[i], true
		}
	}
	if len(candidates) == 0 {
		return "", false
	}
	return candidates[len(candidates)-1], true
}

// loginFromUTF8 is Rust's `String::from_utf8`, failing with the text of
// Rust's Utf8Error ("invalid utf-8 sequence of 1 bytes from index 3",
// "incomplete utf-8 byte sequence from index 3").
func loginFromUTF8(text []byte) (string, error) {
	index := 0
	for index < len(text) {
		first := text[index]
		if first < 0x80 {
			index++
			continue
		}
		width := 0
		switch {
		case first >= 0xC2 && first <= 0xDF:
			width = 2
		case first >= 0xE0 && first <= 0xEF:
			width = 3
		case first >= 0xF0 && first <= 0xF4:
			width = 4
		}
		if width == 0 {
			return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		low, high := byte(0x80), byte(0xBF)
		switch first {
		case 0xE0:
			low = 0xA0
		case 0xED:
			high = 0x9F
		case 0xF0:
			low = 0x90
		case 0xF4:
			high = 0x8F
		}
		for offset := 1; offset < width; offset++ {
			if index+offset >= len(text) {
				return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
			}
			next := text[index+offset]
			if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
				return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", offset, index)
			}
		}
		index += width
	}
	return string(text), nil
}
