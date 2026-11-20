// Port of mj-core/src/credentials/locking.rs.

// Credential-slot ownership shared by sync and quota refreshers.

package core

import (
	"context"
	"sync"
	"time"
)

// ClaudeRefreshLockWait is the shared Claude lock-acquisition budget, also
// included in quota query timeouts.
const ClaudeRefreshLockWait = 10 * time.Second

// CredentialWriteGuard owns the destination's native refresh locks, or
// Mjolnir's sidecar lock. Codex processes outside Mjolnir do not participate
// in its sidecar protocol. Release (Rust's Drop) gives the locks up.
type CredentialWriteGuard struct {
	directories []*OAuthRefreshLock
	file        *FileLock
}

func CredentialWriteGuardAcquire(ctx context.Context, kind HarnessKind, marker string) (*CredentialWriteGuard, error) {
	return CredentialWriteGuardAcquireWithin(ctx, kind, marker, ClaudeRefreshLockWait)
}

// CredentialWriteGuardAcquireWithin is CredentialWriteGuardAcquire with
// claudeWait as the budget for Claude's native locks. Other harnesses keep
// their own budgets.
func CredentialWriteGuardAcquireWithin(ctx context.Context, kind HarnessKind, marker string, claudeWait time.Duration) (*CredentialWriteGuard, error) {
	panic("unported: mj-core/src/credentials/locking.rs CredentialWriteGuard::acquire_within")
}

// Release gives up every lock the guard holds, in the reverse of acquisition
// order (Rust's Drop of the fields).
func (g *CredentialWriteGuard) Release() {
	panic("unported: mj-core/src/credentials/locking.rs CredentialWriteGuard drop")
}

// CredentialInstall is the snapshot actually retained by the destination
// after convergence.
type CredentialInstall struct {
	Snapshot  CredentialSnapshot
	Installed bool
}

// installPolicy is who owns the destination file, which decides what may
// replace it.
type installPolicy int

const (
	// installPolicyCanonical is the profile's own login: only a strictly
	// newer copy replaces it, and a copy without a refresh token never
	// replaces one that has it, because that would drop the grant the daemon
	// alone refreshes. Where copies omit the refresh token (Claude), only a
	// copy holding one replaces it at all, so a host without a login (after a
	// logout) is not handed a stripped session copy nothing can renew.
	installPolicyCanonical installPolicy = iota
	// installPolicySessionCopy is a harness's copy of the login. Also
	// replaced by a same-expiry copy without a refresh token, which is how a
	// copy staged before the daemon became the only refresher converges
	// (#1490).
	installPolicySessionCopy
)

func replaces(policy installPolicy, kind HarnessKind, current *CredentialSnapshot, currentBytes []byte, incoming *CredentialSnapshot, incomingBytes []byte) bool {
	if Reconcile(current, incoming) == SyncActionPull {
		return policy == installPolicySessionCopy ||
			CredentialHoldsRefreshToken(kind, incomingBytes) ||
			(!kind.CopiesOmitRefreshToken() && !CredentialHoldsRefreshToken(kind, currentBytes))
	}
	return policy == installPolicySessionCopy &&
		kind == HarnessKindClaude &&
		!CredentialHoldsRefreshToken(kind, incomingBytes) &&
		current.Present &&
		current.FreshnessEpochMs != nil &&
		incoming.FreshnessEpochMs != nil &&
		*current.FreshnessEpochMs == *incoming.FreshnessEpochMs &&
		current.Fingerprint != incoming.Fingerprint
}

// CanonicalLoginIsReplacedBy is whether the profile's own login current
// gives way to incoming: the rule InstallCredentialsIfNewer applies, for a
// login whose store does the writing itself (the macOS Keychain).
func CanonicalLoginIsReplacedBy(kind HarnessKind, current *CredentialSnapshot, currentBytes []byte, incoming *CredentialSnapshot, incomingBytes []byte) bool {
	return replaces(installPolicyCanonical, kind, current, currentBytes, incoming, incomingBytes)
}

// InstallCredentialsIfNewer installs a copy into the profile's own login,
// only when it is newer and does not lose the refresh token the current one
// holds.
func InstallCredentialsIfNewer(ctx context.Context, kind HarnessKind, path string, bytes []byte) (CredentialInstall, error) {
	return installCredentials(ctx, kind, path, bytes, installPolicyCanonical)
}

// InstallSessionCredentials installs a delivered copy into a harness's own
// credential file: newer copies replace it, and so does a same-expiry copy
// without a refresh token. A worker applies this to the controller's pushes.
func InstallSessionCredentials(ctx context.Context, kind HarnessKind, path string, bytes []byte) (CredentialInstall, error) {
	return installCredentials(ctx, kind, path, bytes, installPolicySessionCopy)
}

func installCredentials(ctx context.Context, kind HarnessKind, path string, bytes []byte, policy installPolicy) (CredentialInstall, error) {
	panic("unported: mj-core/src/credentials/locking.rs install_credentials")
}

// acquireFileLock takes an exclusive OS lock on path, polling every 100 ms;
// a nil wait waits indefinitely.
func acquireFileLock(ctx context.Context, path string, wait *time.Duration) (*FileLock, error) {
	panic("unported: mj-core/src/credentials/locking.rs acquire_file_lock")
}

// Kimi Code uses proper-lockfile on oauth/kimi-code (native stale: 5s).
// Preserve Mjolnir's existing 10s abandonment threshold and 1s heartbeat;
// frequent beats keep our live lease inside the CLI's shorter break window.
const (
	kimiLockHeartbeatInterval = 1 * time.Second
	kimiLockStaleAfter        = 10 * time.Second
	kimiLockWait              = 60 * time.Second
)

func acquireKimiLock(ctx context.Context, home string, wait time.Duration) (*OAuthRefreshLock, error) {
	panic("unported: mj-core/src/credentials/locking.rs acquire_kimi_lock")
}

// OAuthRefreshLock is a proper-lockfile directory lock held with a
// heartbeat. Release (Rust's Drop) stops the heartbeat and removes the
// directory.
type OAuthRefreshLock struct {
	// mu serializes heartbeat and release so an in-flight beat cannot touch
	// a directory acquired by the next owner after our release. path is
	// empty once released.
	mu   sync.Mutex
	path string
	// stopHeartbeat ends the heartbeat goroutine (Rust's JoinHandle::abort).
	stopHeartbeat context.CancelFunc
}

func oauthRefreshLockAcquire(ctx context.Context, path string, wait, staleAfter, heartbeatInterval time.Duration) (*OAuthRefreshLock, error) {
	panic("unported: mj-core/src/credentials/locking.rs OAuthRefreshLock::acquire")
}

// Release stops the heartbeat and removes the lock directory.
func (l *OAuthRefreshLock) Release() {
	panic("unported: mj-core/src/credentials/locking.rs OAuthRefreshLock drop")
}

// mtime is path's modification time, or false when it does not exist.
func mtime(path string) (time.Time, bool, error) {
	panic("unported: mj-core/src/credentials/locking.rs mtime")
}

// touch sets path's modification time; path may be a directory.
func touch(path string, modified time.Time) error {
	panic("unported: mj-core/src/credentials/locking.rs touch")
}

// breakStaleLock removes the lock directory at path when its holder stopped
// heartbeating more than staleAfter ago. True when the lock is gone.
func breakStaleLock(ctx context.Context, path string, staleAfter time.Duration) bool {
	panic("unported: mj-core/src/credentials/locking.rs break_stale_lock")
}
