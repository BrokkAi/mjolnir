// Port of mj-core/src/credentials/locking.rs.

// Credential-slot ownership shared by sync and quota refreshers.

package core

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
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
	parent, ok := credentialParent(marker)
	if !ok {
		return nil, errors.New("credential marker has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return nil, err
	}
	home, err := credentialsCanonicalize(parent)
	if err != nil {
		return nil, fmt.Errorf("resolve credential home: %w", err)
	}
	if kind == HarnessKindKimi {
		above := filepath.Dir(home)
		if above == home {
			return nil, errors.New("Kimi credentials have no home")
		}
		home = above
	}
	guard := &CredentialWriteGuard{}
	switch kind {
	case HarnessKindClaude:
		// Claude 2.1.312 takes these proper-lockfile directory locks in this
		// order during refresh (explicit auth login does not). Preserve the
		// refresh protocol's 60s stale / 5s heartbeat and Mjolnir's existing
		// shared acquisition budget.
		deadline := time.Now().Add(claudeWait)
		for _, path := range []string{filepath.Join(home, ".oauth_refresh.lock"), home + ".lock"} {
			lock, err := oauthRefreshLockAcquire(ctx, path, max(time.Until(deadline), 0), 60*time.Second, 5*time.Second)
			if err != nil {
				guard.Release()
				return nil, err
			}
			guard.directories = append(guard.directories, lock)
		}
	case HarnessKindKimi:
		lock, err := acquireKimiLock(ctx, home, kimiLockWait)
		if err != nil {
			return nil, err
		}
		guard.directories = append(guard.directories, lock)
	case HarnessKindMuse:
		// Muse serializes auth.json writes with an exclusive OS lock on
		// .auth.json.lock (native acquisition waits up to 5s). Preserve
		// Mjolnir's existing 10s acquisition budget.
		wait := 10 * time.Second
		lock, err := acquireFileLock(ctx, filepath.Join(home, ".auth.json.lock"), &wait)
		if err != nil {
			return nil, err
		}
		guard.file = lock
	default:
		lock, err := acquireFileLock(ctx, filepath.Join(home, ".mj-auth.lock"), nil)
		if err != nil {
			return nil, err
		}
		guard.file = lock
	}
	return guard, nil
}

// Release gives up every lock the guard holds, in the order Rust's Drop of
// the fields gives them up: the directory locks as acquired, then the file.
func (g *CredentialWriteGuard) Release() {
	for _, directory := range g.directories {
		directory.Release()
	}
	g.directories = nil
	if g.file != nil {
		file := g.file.File()
		g.file.Drop()
		_ = file.Close()
		g.file = nil
	}
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
	if err := ValidateCredentialPayload(kind, bytes); err != nil {
		return CredentialInstall{}, err
	}
	guard, err := CredentialWriteGuardAcquire(ctx, kind, path)
	if err != nil {
		return CredentialInstall{}, err
	}
	// Blocking writes cannot be cancelled: ownership follows the write.
	defer guard.Release()
	if err := refuseSymlinkedToken("credential", path); err != nil {
		return CredentialInstall{}, err
	}
	current, currentBytes, err := ReadCredentialFile(kind, path)
	if err != nil {
		return CredentialInstall{}, err
	}
	incoming := CredentialSnapshotOf(kind, bytes)
	if replaces(policy, kind, &current, currentBytes, &incoming, bytes) {
		if err := WriteCredentialFile(kind, path, bytes); err != nil {
			return CredentialInstall{}, err
		}
		return CredentialInstall{Snapshot: incoming, Installed: true}, nil
	}
	return CredentialInstall{Snapshot: current, Installed: false}, nil
}

// acquireFileLock takes an exclusive OS lock on path, polling every 100 ms;
// a nil wait waits indefinitely.
func acquireFileLock(ctx context.Context, path string, wait *time.Duration) (*FileLock, error) {
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0o666)
	if err != nil {
		return nil, fmt.Errorf("open credential lock %s: %w", path, err)
	}
	var deadline time.Time
	if wait != nil {
		deadline = time.Now().Add(*wait)
	}
	for {
		locked, err := tryLockFile(file)
		if err != nil {
			_ = file.Close()
			return nil, fmt.Errorf("lock credential %s: %w", path, err)
		}
		if locked {
			return NewFileLock(file), nil
		}
		if wait != nil && !time.Now().Before(deadline) {
			_ = file.Close()
			return nil, fmt.Errorf("timed out waiting for credential lock %s", path)
		}
		select {
		case <-ctx.Done():
			_ = file.Close()
			return nil, ctx.Err()
		case <-time.After(100 * time.Millisecond):
		}
	}
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
	oauthDir := filepath.Join(home, "oauth")
	if err := os.MkdirAll(oauthDir, 0o777); err != nil {
		return nil, fmt.Errorf("prepare Kimi Code OAuth lock: %w", err)
	}
	// proper-lockfile requires the sentinel to exist before taking its lock.
	sentinel, err := os.OpenFile(filepath.Join(oauthDir, "kimi-code"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o666)
	if err != nil {
		return nil, fmt.Errorf("prepare Kimi Code OAuth lock sentinel: %w", err)
	}
	_ = sentinel.Close()
	return oauthRefreshLockAcquire(ctx, filepath.Join(oauthDir, "kimi-code.lock"), wait, kimiLockStaleAfter, kimiLockHeartbeatInterval)
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
	deadline := time.Now().Add(wait)
	for {
		err := os.Mkdir(path, 0o777)
		if err == nil {
			heartbeat, stop := context.WithCancel(context.Background())
			lock := &OAuthRefreshLock{path: path, stopHeartbeat: stop}
			go lock.heartbeat(heartbeat, heartbeatInterval)
			return lock, nil
		}
		if !errors.Is(err, fs.ErrExist) {
			return nil, fmt.Errorf("acquire OAuth refresh lock: %w", err)
		}
		if !time.Now().Before(deadline) {
			return nil, fmt.Errorf("timed out waiting for OAuth refresh lock %s", path)
		}
		if !breakStaleLock(ctx, path, staleAfter) {
			select {
			case <-ctx.Done():
				return nil, ctx.Err()
			case <-time.After(100 * time.Millisecond):
			}
		}
	}
}

// Release stops the heartbeat and removes the lock directory.
func (l *OAuthRefreshLock) Release() {
	if l.stopHeartbeat != nil {
		l.stopHeartbeat()
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.path == "" {
		return
	}
	path := l.path
	l.path = ""
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		slog.Warn("release OAuth refresh lock", "path", path, "error", err)
	}
}

// mtime is path's modification time, or false when it does not exist.
func mtime(path string) (time.Time, bool, error) {
	info, err := os.Stat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return time.Time{}, false, nil
	}
	if err != nil {
		return time.Time{}, false, err
	}
	return info.ModTime(), true, nil
}

// touch sets path's modification time; path may be a directory.
func touch(path string, modified time.Time) error {
	// A zero access time leaves it unchanged, as Rust's FileTimes does.
	return os.Chtimes(path, time.Time{}, modified)
}

// breakStaleLock removes the lock directory at path when its holder stopped
// heartbeating more than staleAfter ago. True when the lock is gone.
func breakStaleLock(ctx context.Context, path string, staleAfter time.Duration) bool {
	modified, exists, err := mtime(path)
	if err != nil {
		slog.Warn("inspect OAuth refresh lock", "path", path, "error", err)
		return false
	}
	if !exists {
		return true
	}
	// proper-lockfile can round its writes into the future.
	age := time.Since(modified)
	if age < 0 || age < staleAfter {
		return false
	}
	if err := os.Remove(path); err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return true
		}
		slog.Warn("remove stale OAuth refresh lock", "path", path, "error", err)
		return false
	}
	slog.Warn("removed an abandoned OAuth refresh lock", "path", path, "age_seconds", int64(age/time.Second))
	return true
}

// heartbeat republishes the lock directory's modification time every
// interval until the lock is released.
func (l *OAuthRefreshLock) heartbeat(ctx context.Context, interval time.Duration) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-time.After(interval):
		}
		l.mu.Lock()
		if l.path == "" {
			l.mu.Unlock()
			return
		}
		if err := touch(l.path, time.Now()); err != nil {
			slog.Warn("heartbeat OAuth refresh lock", "path", l.path, "error", err)
		}
		l.mu.Unlock()
	}
}

// credentialsCanonicalize is Rust's std::fs::canonicalize: the absolute path
// with every symbolic link resolved.
func credentialsCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}
