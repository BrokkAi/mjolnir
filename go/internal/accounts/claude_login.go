// Port of mj-accounts/src/claude_login.rs.

// Writing the host's Claude login on macOS (#1491).
//
// The Keychain item `Claude Code-credentials` is the only host login for
// Claude there (ReadClaudeKeychainLogin reads it). The daemon is the one
// refresher, but a pre-upgrade container or remote session (or an old
// worker during a rollout) can still hold a full copy with the refresh
// token. When that session refreshes, Anthropic rotates the shared
// single-use token and the newer one exists only in the session. The sync
// pulls such a copy back into the Keychain, as it does into the profile's
// file on other hosts, and hands the session a stripped copy again.

package accounts

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// securityInteractiveLineLimit is the longest command line `security -i`
// accepts.
const securityInteractiveLineLimit = 4032

// WriteClaudeKeychainLogin replaces Claude Code's Keychain login, as Claude
// Code does: the command goes to `security -i` on stdin, so the secret is
// never in an argument list another process could read. The account is
// Claude Code's own, so this updates its item instead of adding a second
// one. Reads the item back, because an interactive `security` does not
// report a failed command in its exit status.
func WriteClaudeKeychainLogin(executor core.CommandExecutor, login []byte) error {
	line := fmt.Sprintf("add-generic-password -U -a %s -s \"%s\" -X %s\n",
		ClaudeKeychainAccount(), core.ClaudeKeychainService, core.LowerHex(login))
	// `security -i` reads at most this many bytes per command line (Claude
	// Code 2.1.321 checks the same 4032 before using it). A longer line would
	// be cut short and could store a truncated login. Claude Code then passes
	// the login as an argument, where other processes can read it; Mjolnir
	// reports the failure instead (the sync step fails visibly).
	if len(line) > securityInteractiveLineLimit {
		return fmt.Errorf("Claude Code's login is too large (%d bytes) to write to the macOS Keychain through `security -i`", len(login))
	}
	command := core.NewCommandSpec("security", []string{"-i"}).
		WithPurpose("write Claude Code authentication to the macOS Keychain").
		WithSensitiveStdin([]byte(line))
	command.SensitiveOutput = true
	output, err := executor.Execute(&command)
	if err != nil {
		return fmt.Errorf("write Claude Code's login to the macOS Keychain: %w", err)
	}
	if output.Status != 0 {
		return fmt.Errorf("the macOS Keychain refused Claude Code's login (security exited with status %d)", output.Status)
	}
	stored, err := ReadClaudeKeychainLogin(executor)
	if err != nil {
		return err
	}
	switch stored := stored.(type) {
	case KeychainLoginPresent:
		if bytes.Equal(trimASCIIEnd(stored.Login), trimASCIIEnd(login)) {
			return nil
		}
		return errors.New("the macOS Keychain does not hold the login that was just written")
	case KeychainLoginMissing:
		return errors.New("the macOS Keychain has no login after the write")
	case KeychainLoginUnreadable:
		return fmt.Errorf("the macOS Keychain could not be read back after the write (security exited with status %d)", stored.Status)
	}
	panic("unreachable KeychainLogin")
}

// trimASCIIEnd is Rust's `<[u8]>::trim_ascii_end`: trailing ASCII
// whitespace (space, tab, newline, form feed, carriage return) removed.
func trimASCIIEnd(value []byte) []byte {
	return bytes.TrimRight(value, " \t\n\f\r")
}

// InstallKeychainLoginIfNewer installs a session's fresher login into the
// Keychain, only when it is strictly newer and does not lose a refresh
// token the current one holds (the rule of
// core.InstallCredentialsIfNewer). Holds the locks Claude Code takes for
// `~/.claude` (lockMarker is its credential file,
// core.HarnessLoginStore.LockMarker), rereads the Keychain under them, and
// writes under them, so Claude Code's own refresh and this install cannot
// interleave. A Keychain that cannot be read or written fails; it never
// falls back to a file.
func InstallKeychainLoginIfNewer(ctx context.Context, lockMarker string, security core.CommandExecutor, bytes []byte) (core.CredentialInstall, error) {
	kind := core.HarnessKindClaude
	if err := core.ValidateCredentialPayload(kind, bytes); err != nil {
		return core.CredentialInstall{}, err
	}
	guard, err := core.CredentialWriteGuardAcquire(ctx, kind, lockMarker)
	if err != nil {
		return core.CredentialInstall{}, err
	}
	// Blocking writes cannot be cancelled: ownership follows the write.
	defer guard.Release()
	bytes = append([]byte(nil), bytes...)
	var (
		snapshot core.CredentialSnapshot
		current  []byte
	)
	login, err := ReadClaudeKeychainLogin(security)
	if err != nil {
		return core.CredentialInstall{}, err
	}
	switch login := login.(type) {
	case KeychainLoginPresent:
		snapshot, current = core.CredentialSnapshotOf(kind, login.Login), login.Login
	case KeychainLoginMissing:
		snapshot, current = core.CredentialSnapshotAbsent(), []byte{}
	case KeychainLoginUnreadable:
		return core.CredentialInstall{}, fmt.Errorf("the macOS Keychain could not be read (security exited with status %d)", login.Status)
	}
	incoming := core.CredentialSnapshotOf(kind, bytes)
	if !core.CanonicalLoginIsReplacedBy(kind, &snapshot, current, &incoming, bytes) {
		return core.CredentialInstall{Snapshot: snapshot, Installed: false}, nil
	}
	if err := WriteClaudeKeychainLogin(security, bytes); err != nil {
		return core.CredentialInstall{}, err
	}
	return core.CredentialInstall{Snapshot: incoming, Installed: true}, nil
}

// FakeKeychain is a hand-written macOS Keychain for tests: `security` as
// Claude Code's item answers it and as `security -i` writes it (Rust's
// `claude_login::fake` module).
type FakeKeychain struct {
	core.CommandExecutorDefaults
	mu    sync.Mutex
	state fakeKeychainState
}

var _ core.CommandExecutor = (*FakeKeychain)(nil)

// FakeKeychainCall is one call: its arguments, and what it was given on
// stdin (nil when nothing).
type FakeKeychainCall struct {
	Args  []string
	Stdin []byte
}

type fakeKeychainState struct {
	login []byte
	// locked: every call fails as `security` does on a locked Keychain.
	locked bool
	// refuseWrites: writes fail while reads work, as when the Keychain
	// refuses to add.
	refuseWrites bool
	// lockDir is a directory whose Claude Code refresh lock a write must
	// hold.
	lockDir *string
	calls   []FakeKeychainCall
}

// NewFakeKeychain is an empty Keychain (Rust's `FakeKeychain::default`).
func NewFakeKeychain() *FakeKeychain { return &FakeKeychain{} }

// FakeKeychainHolding is a Keychain whose item holds login.
func FakeKeychainHolding(login []byte) *FakeKeychain {
	keychain := &FakeKeychain{}
	keychain.state.login = append([]byte{}, login...)
	return keychain
}

// Locked makes every call fail as on a locked Keychain.
func (k *FakeKeychain) Locked() *FakeKeychain {
	k.mu.Lock()
	defer k.mu.Unlock()
	k.state.locked = true
	return k
}

// RequiringLockIn fails any write made without Claude Code's refresh lock
// in dir. Unix only in Rust.
func (k *FakeKeychain) RequiringLockIn(dir string) *FakeKeychain {
	canonical, err := filepath.EvalSymlinks(dir)
	if err == nil {
		canonical, err = filepath.Abs(canonical)
	}
	if err != nil {
		panic(err)
	}
	k.mu.Lock()
	defer k.mu.Unlock()
	k.state.lockDir = &canonical
	return k
}

// RefuseWrites makes writes fail while reads work.
func (k *FakeKeychain) RefuseWrites() {
	k.mu.Lock()
	defer k.mu.Unlock()
	k.state.refuseWrites = true
}

// Login is the item's login, nil when there is none.
func (k *FakeKeychain) Login() []byte {
	k.mu.Lock()
	defer k.mu.Unlock()
	if k.state.login == nil {
		return nil
	}
	return append([]byte{}, k.state.login...)
}

// Calls is every call so far.
func (k *FakeKeychain) Calls() []FakeKeychainCall {
	k.mu.Lock()
	defer k.mu.Unlock()
	return append([]FakeKeychainCall(nil), k.state.calls...)
}

func (k *FakeKeychain) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (k *FakeKeychain) CancellationRequestedWithoutDeadline() bool {
	return k.CancellationRequested()
}

func (k *FakeKeychain) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return k.Execute(command)
}

func (k *FakeKeychain) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return k.ExecuteWithStdin(command, input)
}

func (k *FakeKeychain) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return k.Execute(command)
}

func (k *FakeKeychain) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return k.ExecuteCleanup(command)
}

// Execute answers `security find-generic-password` from the item and
// stores what `security -i` writes, panicking on any other command as
// Rust's assertions do.
func (k *FakeKeychain) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	fakeKeychainAssert(command.Program == "security", "program %q is security", command.Program)
	fakeKeychainAssert(command.SensitiveOutput, "a login passes through here")
	var stdin []byte
	if input, ok := command.SensitiveStdin(); ok {
		stdin = append([]byte{}, input...)
	}
	k.mu.Lock()
	defer k.mu.Unlock()
	k.state.calls = append(k.state.calls, FakeKeychainCall{
		Args:  append([]string(nil), command.Args...),
		Stdin: stdin,
	})
	status := func(status int) (core.CommandOutput, error) {
		return core.CommandOutput{Status: status, Stdout: []byte{}, Stderr: []byte{}}, nil
	}
	if k.state.locked {
		return status(36)
	}
	if len(command.Args) > 0 && command.Args[0] == "-i" {
		fakeKeychainAssert(reflect.DeepEqual(command.Args, []string{"-i"}), "the secret is never an argument")
		if k.state.refuseWrites {
			return status(36)
		}
		if k.state.lockDir != nil {
			info, err := os.Stat(filepath.Join(*k.state.lockDir, ".oauth_refresh.lock"))
			fakeKeychainAssert(err == nil && info.IsDir(), "the Keychain is written under Claude Code's refresh lock")
		}
		fakeKeychainAssert(stdin != nil, "the command comes on stdin")
		words := strings.Fields(string(stdin))
		want := []string{"add-generic-password", "-U", "-a", ClaudeKeychainAccount(), "-s", "\"Claude"}
		fakeKeychainAssert(len(words) >= 9 && reflect.DeepEqual(words[:6], want), "words %q", words)
		fakeKeychainAssert(words[6] == "Code-credentials\"", "words[6] = %q", words[6])
		fakeKeychainAssert(words[7] == "-X", "words[7] = %q", words[7])
		hex := words[8]
		login := make([]byte, 0, len(hex)/2)
		for at := 0; at < len(hex); at += 2 {
			value, err := strconv.ParseUint(hex[at:at+2], 16, 8)
			if err != nil {
				panic(err)
			}
			login = append(login, byte(value))
		}
		k.state.login = login
		return status(0)
	}
	fakeKeychainAssert(reflect.DeepEqual(command.Args, []string{
		"find-generic-password", "-a", ClaudeKeychainAccount(), "-s", core.ClaudeKeychainService, "-w",
	}), "args %q", command.Args)
	if k.state.login == nil {
		return status(44)
	}
	return core.CommandOutput{Status: 0, Stdout: append([]byte{}, k.state.login...), Stderr: []byte{}}, nil
}

// fakeKeychainAssert panics with the message when condition fails, as the
// Rust fake's assertions do.
func fakeKeychainAssert(condition bool, format string, args ...any) {
	if !condition {
		panic(fmt.Sprintf("assertion failed: "+format, args...))
	}
}
