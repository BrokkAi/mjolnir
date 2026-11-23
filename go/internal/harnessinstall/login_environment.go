// Port of mj-harness-install/src/login_environment.rs.

// Target-account shell exports, isolated from the process that launched
// Mjolnir.
//
// Discovery starts the account's shell as an interactive login shell, the way
// a terminal starts it, so the version managers people actually install (nvm,
// pyenv, asdf, mise, volta, sdkman, Homebrew on Linux) put their tools on
// `PATH`. The shell still starts from a cleared, seeded environment, so
// nothing the launcher exported can leak into a session.

package harnessinstall

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"iter"
	"log/slog"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LoginEnvironmentEnvironment is Rust's `login_environment::Environment`
// (prefixed: `Environment` is config/secrets' name).
type LoginEnvironmentEnvironment = map[string]string

// loginEnvironmentCache is Rust's static `ENVIRONMENT`: successful discovery,
// shared by session launches in this process. mu is held across discovery,
// as OnceCell::get_or_try_init makes concurrent callers wait for one
// initialization.
var loginEnvironmentCache struct {
	mu    sync.Mutex
	value LoginEnvironmentEnvironment
	set   bool
}

// InitializeFromParent: the worker's internal re-exec passes a resolved
// environment explicitly. Seed the cache before starting its runtime, without
// another shell startup.
func InitializeFromParent() error {
	environment := LoginEnvironmentEnvironment{}
	for _, entry := range os.Environ() {
		// As Rust's vars_os, a leading '=' (a Windows drive directory entry)
		// belongs to the name.
		index := strings.IndexByte(entry[min(1, len(entry)):], '=')
		name, value := entry, ""
		if index >= 0 {
			index += min(1, len(entry))
			name, value = entry[:index], entry[index+1:]
		}
		if !utf8.ValidString(name) {
			return errors.New("login environment name is not UTF-8")
		}
		if !utf8.ValidString(value) {
			return errors.New("login environment value is not UTF-8")
		}
		environment[name] = value
	}
	loginEnvironmentCache.mu.Lock()
	defer loginEnvironmentCache.mu.Unlock()
	if loginEnvironmentCache.set {
		return errors.New("login environment already initialized")
	}
	loginEnvironmentCache.value = environment
	loginEnvironmentCache.set = true
	return nil
}

// Resolve: successful discovery is shared by session launches in this
// process. Failed startup is reported and may be retried; it never authorizes
// ambient inheritance. The returned map is shared; callers must not modify
// it.
func Resolve(ctx context.Context) (LoginEnvironmentEnvironment, error) {
	loginEnvironmentCache.mu.Lock()
	defer loginEnvironmentCache.mu.Unlock()
	if loginEnvironmentCache.set {
		return loginEnvironmentCache.value, nil
	}
	type discovered struct {
		environment LoginEnvironmentEnvironment
		err         error
	}
	cancellation, cancellable := core.ScopedPreparationCancellation(ctx)
	result, err := core.SpawnBlocking(func() discovered {
		if cancellable {
			environment, err := discoverCancellable(cancellation)
			return discovered{environment, err}
		}
		environment, err := Discover()
		return discovered{environment, err}
	}).Join(ctx)
	if err != nil {
		return nil, fmt.Errorf("target login environment task failed: %w", err)
	}
	if result.err != nil {
		return nil, result.err
	}
	loginEnvironmentCache.value = result.environment
	loginEnvironmentCache.set = true
	return result.environment, nil
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
	if runtime.GOOS == "windows" {
		return nil, errors.New("target login environment requires a Unix account")
	}
	account, err := accountCurrent()
	if err != nil {
		return nil, err
	}
	return account.environment()
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
	if runtime.GOOS == "windows" {
		return discoverWindowsAccount()
	}
	started := time.Now()
	account, err := accountCurrent()
	if err != nil {
		return nil, err
	}
	shell := account.shell
	environment, err := discoverAccount(account, LoginEnvironmentProbeTimeout)
	if err != nil {
		return nil, err
	}
	pathEntries := 0
	if path, ok := environment["PATH"]; ok {
		pathEntries = len(filepath.SplitList(path))
	}
	slog.Info("resolved the account's shell environment",
		"shell", shell,
		"elapsed_ms", uint64(time.Since(started).Milliseconds()),
		"path_entries", pathEntries)
	return environment, nil
}

func discoverCancellable(cancellation context.Context) (LoginEnvironmentEnvironment, error) {
	if runtime.GOOS == "windows" {
		return Discover()
	}
	executor := core.CancellableProcessExecutorWithTimeout(LoginEnvironmentProbeTimeout).
		WithCancellation(cancellation).
		WithForegroundTerminal()
	account, err := accountCurrent()
	if err != nil {
		return nil, err
	}
	return discoverAccountWithExecutor(account, executor)
}

// AccountShell is the shell the probe starts, for diagnostics that explain
// where a session's environment came from.
func AccountShell() (string, error) {
	if runtime.GOOS == "windows" {
		return "", errors.New("target shell environment requires a Unix account")
	}
	account, err := accountCurrent()
	if err != nil {
		return "", err
	}
	return account.shell, nil
}

// AccountName is the effective user's account name from the account
// database, as Node's `os.userInfo().username` reports it.
func AccountName() (string, error) {
	if runtime.GOOS == "windows" {
		return "", errors.New("account names come from the Unix account database")
	}
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
	if !utf8.ValidString(a.shell) {
		return nil, errors.New("target shell path is not UTF-8")
	}
	if !utf8.ValidString(a.home) {
		return nil, errors.New("target home path is not UTF-8")
	}
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
//
// Rust calls getpwuid_r, which consults every name service the system is
// configured with. Go has no such call without cgo, so the entry comes from
// the system's own account-database tool: `getent passwd` (NSS) or, on
// macOS, Directory Services through `dscl`.
func accountCurrent() (Account, error) {
	var account Account
	var err error
	if runtime.GOOS == "darwin" {
		account, err = accountFromDirectoryServices()
	} else {
		account, err = accountFromNameService()
	}
	if err != nil {
		return Account{}, err
	}
	if account.shell == "" {
		account.shell = "/bin/sh"
	}
	// Let a subprocess fixture substitute a deliberately slow shell.
	if core.TestHooksEnabled() {
		if shell, ok := os.LookupEnv("MJ_TEST_ACCOUNT_SHELL"); ok {
			account.shell = shell
		}
	}
	return account, nil
}

// accountFromNameService reads `getent passwd <euid>`:
// name:password:uid:gid:gecos:home:shell.
func accountFromNameService() (Account, error) {
	output, err := core.Capture(context.Background(), exec.Command("getent", "passwd", strconv.Itoa(os.Geteuid())))
	if err != nil {
		return Account{}, fmt.Errorf("read target account: %w", err)
	}
	// getent exits 2 when the key is absent from the database.
	if output.Status != nil && output.Status.ExitCode() == 2 {
		return Account{}, errors.New("target account is absent from the account database")
	}
	if output.Status == nil || !output.Status.Success() {
		return Account{}, fmt.Errorf("read target account: getent exited with %v", output.Status)
	}
	line, _, _ := strings.Cut(string(output.Stdout), "\n")
	fields := strings.Split(line, ":")
	if len(fields) != 7 {
		return Account{}, errors.New("read target account: malformed account database entry")
	}
	if !utf8.ValidString(line) {
		return Account{}, errors.New("read target account: entry is not UTF-8")
	}
	return Account{user: fields[0], home: fields[5], shell: fields[6]}, nil
}

// accountFromDirectoryServices reads the account's name and home through the
// system library (os/user uses getpwuid_r on macOS) and its shell from
// Directory Services.
func accountFromDirectoryServices() (Account, error) {
	current, err := user.LookupId(strconv.Itoa(os.Geteuid()))
	if err != nil {
		var unknown user.UnknownUserIdError
		if errors.As(err, &unknown) {
			return Account{}, errors.New("target account is absent from the account database")
		}
		return Account{}, fmt.Errorf("read target account: %w", err)
	}
	output, err := core.Capture(context.Background(), exec.Command("/usr/bin/dscl", ".", "-read", "/Users/"+current.Username, "UserShell"))
	if err != nil {
		return Account{}, fmt.Errorf("read target account: %w", err)
	}
	if output.Status == nil || !output.Status.Success() {
		return Account{}, fmt.Errorf("read target account: dscl exited with %v", output.Status)
	}
	shell := ""
	for _, line := range strings.Split(string(output.Stdout), "\n") {
		if value, ok := strings.CutPrefix(line, "UserShell:"); ok {
			shell = strings.TrimSpace(value)
		}
	}
	return Account{user: current.Username, home: current.HomeDir, shell: shell}, nil
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
	return discoverAccountWithExecutor(account, core.NewBoundedProcessExecutor(timeout))
}

func discoverAccountWithExecutor(account Account, executor core.CommandExecutor) (LoginEnvironmentEnvironment, error) {
	var nonceBytes [16]byte
	if _, err := rand.Read(nonceBytes[:]); err != nil {
		return nil, fmt.Errorf("create login capture marker: %v", err)
	}
	nonce := hex.EncodeToString(nonceBytes[:])
	marker := "\x00MJ_LOGIN_" + nonce + "\x00"
	// Absolute env prevents a profile alias or PATH change from replacing the
	// exporter. exec avoids logout hooks being appended to the captured records.
	script := "printf '\\000MJ_LOGIN_" + nonce + "\\000'; exec /usr/bin/env -0"
	probe, err := loginShellCommand(account.shell, script)
	if err != nil {
		return nil, err
	}
	probe.command.ClearEnv = true
	home := account.home
	probe.command.Cwd = &home
	if probe.command.Env, err = account.environment(); err != nil {
		return nil, err
	}
	var output core.CommandOutput
	if probe.scriptOnStdin {
		output, err = executor.ExecuteWithStdin(&probe.command, strings.NewReader(script))
	} else {
		output, err = executor.Execute(&probe.command)
	}
	if err != nil {
		return nil, err
	}
	// Startup output can contain secrets. Never include it in errors or logs.
	if output.Status != 0 {
		return nil, fmt.Errorf("target login shell exited with status %d", output.Status)
	}
	return loginEnvironmentParse(output.Stdout, []byte(marker))
}

type LoginShellProbe struct {
	command       core.CommandSpec
	scriptOnStdin bool
}

func loginShellCommand(shell, script string) (LoginShellProbe, error) {
	if !utf8.ValidString(shell) {
		return LoginShellProbe{}, errors.New("target shell path is not UTF-8")
	}
	var command core.CommandSpec
	scriptOnStdin := false
	if arg0, ok := cshLoginArgv0(shell); ok {
		// `-c` leaves `$?prompt` unset while startup files run, even with
		// `-i`. C-shell files commonly guard terminal setup with that flag.
		command = core.NewCommandSpec(shell, []string{"-i", "-s"}).Arg0(arg0)
		scriptOnStdin = true
	} else {
		command = core.NewCommandSpec(shell, []string{"-l", "-i", "-c", script})
	}
	return LoginShellProbe{
		command:       command.WithPurpose("initialize target shell environment"),
		scriptOnStdin: scriptOnStdin,
	}, nil
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
	position := bytes.Index(output, marker)
	if position < 0 {
		return nil, errors.New("target login shell did not produce an environment frame")
	}
	records := output[position+len(marker):]
	if !bytes.HasSuffix(records, []byte{0}) {
		return nil, errors.New("target login environment was truncated")
	}
	environment := LoginEnvironmentEnvironment{}
	for _, record := range bytes.Split(records[:len(records)-1], []byte{0}) {
		if !utf8.Valid(record) {
			return nil, errors.New("target login environment is not UTF-8")
		}
		name, value, ok := strings.Cut(string(record), "=")
		if !ok {
			return nil, errors.New("invalid target login environment record")
		}
		if name == "" {
			return nil, errors.New("empty target login environment name")
		}
		environment[name] = value
	}
	if environment["PATH"] == "" {
		return nil, errors.New("target login environment has no PATH")
	}
	return environment, nil
}
