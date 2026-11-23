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
	"context"
	"io"
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
	panic("unported: mj-accounts/src/claude_login.rs write_claude_keychain_login")
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
	panic("unported: mj-accounts/src/claude_login.rs install_keychain_login_if_newer")
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
	panic("unported: mj-accounts/src/claude_login.rs FakeKeychain::requiring_lock_in")
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
	panic("unported: mj-accounts/src/claude_login.rs FakeKeychain::execute")
}
