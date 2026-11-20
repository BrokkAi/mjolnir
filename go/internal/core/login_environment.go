// Port of mj-core/src/login_environment.rs.

// Target-account shell exports, isolated from the process that launched
// Mjolnir.
//
// Discovery starts the account's shell as an interactive login shell, the way
// a terminal starts it, so the version managers people actually install (nvm,
// pyenv, asdf, mise, volta, sdkman, Homebrew on Linux) put their tools on
// `PATH`. The shell still starts from a cleared, seeded environment, so
// nothing the launcher exported can leak into a session.

package core

import (
	"context"
	"iter"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// LoginEnvironmentEnvironment is Rust's `login_environment::Environment`
// (prefixed: `Environment` is config/secrets' name).
type LoginEnvironmentEnvironment = map[string]string

// loginEnvironmentCache is Rust's static `ENVIRONMENT`: successful discovery,
// shared by session launches in this process.
var loginEnvironmentCache struct {
	mu    sync.Mutex
	value LoginEnvironmentEnvironment
	set   bool
}

// InitializeFromParent: the worker's internal re-exec passes a resolved
// environment explicitly. Seed the cache before starting its runtime, without
// another shell startup.
func InitializeFromParent() error {
	panic("unported: mj-core/src/login_environment.rs initialize_from_parent")
}

// Resolve: successful discovery is shared by session launches in this
// process. Failed startup is reported and may be retried; it never authorizes
// ambient inheritance. The returned map is shared; callers must not modify
// it.
func Resolve(ctx context.Context) (LoginEnvironmentEnvironment, error) {
	panic("unported: mj-core/src/login_environment.rs resolve")
}

func WithOverrides(ctx context.Context, overrides LoginEnvironmentEnvironment) (LoginEnvironmentEnvironment, error) {
	resolved, err := Resolve(ctx)
	if err != nil {
		return nil, err
	}
	environment := make(LoginEnvironmentEnvironment, len(resolved)+len(overrides))
	for name, value := range resolved {
		environment[name] = value
	}
	for name, value := range overrides {
		environment[name] = value
	}
	return environment, nil
}

// OverlayImageEnvironment restores the container image's declared
// environment onto a discovered login environment.
//
// A login shell started with a minimal seed reproduces only what a profile
// script exports. An image that declares variables with `ENV` (for example
// `RUSTUP_HOME` or `PLAYWRIGHT_BROWSERS_PATH`) rather than through
// `/etc/profile.d` therefore loses them, and processes the worker launches
// resolve their toolchains against the wrong locations. In a fresh container
// the worker's own process environment is exactly the image's `ENV` plus the
// values the controller passed at `run`, with no host pollution, because the
// container is a clean namespace. This carries the keys discovered lacks from
// that ambient environment: keys the login shell already produced (such as
// `PATH` and `HOME`) are left untouched, launcher and shell internals are
// never carried, and credentials are left to the controller's own token
// mechanism. The caller applies its deliberate `target_environment` on top,
// so those still win.
//
// This must only run for container targets, where the ambient environment is
// the image's. On a bare or localhost target the ambient environment is the
// user's shell, and carrying it would defeat the isolation Discover exists to
// provide.
func OverlayImageEnvironment(discovered LoginEnvironmentEnvironment, ambient iter.Seq2[string, string]) {
	for name, value := range ambient {
		if _, ok := discovered[name]; ok || !carriesImageVariable(name) {
			continue
		}
		discovered[name] = value
	}
}

// carriesImageVariable reports whether an ambient variable is part of the
// image's contract rather than launcher noise or a credential handled
// elsewhere.
func carriesImageVariable(name string) bool {
	switch name {
	case "",
		// Shell bookkeeping the login shell already re-derives for itself.
		"_", "SHLVL", "PWD", "OLDPWD",
		// Tokens flow through the session's token file, not the image
		// contract: the worker keeps the container's token there before its
		// re-exec, and the controller's credential sync updates it.
		"GH_TOKEN", "GITHUB_TOKEN":
		return false
	}
	// The worker's own launcher and re-exec internals.
	return !strings.HasPrefix(name, "MJ_")
}

// Bootstrap is the minimal account identity for recovery that must not
// depend on shell startup. It requires a Unix account.
func Bootstrap() (LoginEnvironmentEnvironment, error) {
	panic("unported: mj-core/src/login_environment.rs bootstrap")
}

// LoginEnvironmentProbeTimeout is how long the login-shell probe may run. The
// probe reproduces everything a terminal session pays at shell startup —
// version managers, profile scripts, prompt setup — so on a loaded host it
// can take far longer than a profile read, and any fixed budget smaller than
// the worker-startup budget fails spawns that the controller's own wait would
// have outlasted.
//
// This budget is the controller's worker-startup ceiling
// (`WORKER_STARTUP_CONNECT_CEILING`, 300s) minus 60s the launch itself can
// spend reaching the probe on a loaded machine, so the probe deadline always
// fires before the startup wait would give up on a living worker. While the
// probe runs, the worker records the WorkerStartupStepLoginEnvironment
// startup step, and the startup wait defers to this deadline for exactly
// that step; the test
// `login_probe_deadline_fits_inside_the_worker_startup_ceiling` pins the
// ordering so the two cannot silently disagree. The probe records its own
// exit when the deadline passes, so the worker's own sentence, not the wait
// giving up, is what a too-slow startup reports. The 240s comes from that
// measured ceiling; it is not an independent guess at how long a shell may
// take.
const LoginEnvironmentProbeTimeout = 240 * time.Second

// Discover reproduces the account's terminal environment: on Unix through
// its login shell, on Windows through the account's registry-backed
// environment block (CreateEnvironmentBlock with bInherit = FALSE).
func Discover() (LoginEnvironmentEnvironment, error) {
	panic("unported: mj-core/src/login_environment.rs discover")
}

func discoverCancellable(cancellation context.Context) (LoginEnvironmentEnvironment, error) {
	panic("unported: mj-core/src/login_environment.rs discover_cancellable")
}

// AccountShell is the shell the probe starts, for diagnostics that explain
// where a session's environment came from.
func AccountShell() (string, error) {
	account, err := accountCurrent()
	if err != nil {
		return "", err
	}
	return account.shell, nil
}

// AccountName is the effective user's account name from the account
// database, as Node's `os.userInfo().username` reports it.
func AccountName() (string, error) {
	account, err := accountCurrent()
	if err != nil {
		return "", err
	}
	return account.user, nil
}

// Account is Unix-only in Rust.
type Account struct {
	user  string
	home  string
	shell string
}

func (a Account) environment() (LoginEnvironmentEnvironment, error) {
	return LoginEnvironmentEnvironment{
		"HOME":    a.home,
		"USER":    a.user,
		"LOGNAME": a.user,
		"SHELL":   a.shell,
		"PATH":    "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
	}, nil
}

// accountCurrent reads the effective user's entry from the account
// database. With the test-hooks build, `MJ_TEST_ACCOUNT_SHELL` lets a
// subprocess fixture substitute a deliberately slow shell.
func accountCurrent() (Account, error) {
	panic("unported: mj-core/src/login_environment.rs Account::current")
}

// discoverAccount reproduces the environment a terminal gives this account.
//
// POSIX shells use `-l` to read login startup files (`/etc/profile`,
// `~/.zprofile`). C shells instead identify a login shell by a leading dash
// in `argv[0]` (`-tcsh` or `-csh`) and reject `-l`. In both families, `-i`
// reads the interactive startup files (`~/.zshrc`, `~/.bashrc`,
// `~/.tcshrc`) where version managers install themselves. A terminal starts
// a single shell that is both, so the probe starts the account's shell the
// same way.
func discoverAccount(account Account, timeout time.Duration) (LoginEnvironmentEnvironment, error) {
	return discoverAccountWithExecutor(account, NewBoundedProcessExecutor(timeout))
}

func discoverAccountWithExecutor(account Account, executor CommandExecutor) (LoginEnvironmentEnvironment, error) {
	panic("unported: mj-core/src/login_environment.rs discover_account_with_executor")
}

type LoginShellProbe struct {
	command       CommandSpec
	scriptOnStdin bool
}

func loginShellCommand(shell, script string) (LoginShellProbe, error) {
	panic("unported: mj-core/src/login_environment.rs login_shell_command")
}

func cshLoginArgv0(shell string) (string, bool) {
	switch filepath.Base(shell) {
	case "tcsh":
		return "-tcsh", true
	case "csh":
		return "-csh", true
	}
	return "", false
}

// loginEnvironmentParse is Rust's private `parse` (prefixed: the name is
// activity/question_locator's).
func loginEnvironmentParse(output, marker []byte) (LoginEnvironmentEnvironment, error) {
	panic("unported: mj-core/src/login_environment.rs parse")
}
