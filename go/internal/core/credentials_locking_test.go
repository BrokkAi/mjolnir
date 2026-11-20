// Port of the tests in mj-core/src/credentials/locking.rs.

package core

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

// ageKimiLock backdates the lock directory the way a holder that stopped
// heartbeating leaves it behind.
func ageKimiLock(t *testing.T, path string, age time.Duration) {
	t.Helper()
	if err := touch(path, time.Now().Add(-age)); err != nil {
		t.Fatalf("backdate lock directory: %v", err)
	}
}

func TestAKimiRefreshLockLeftByACrashedHolderIsBrokenAndReacquired(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1299: Windows file locking and directory times")
	}
	home := t.TempDir()
	lock := filepath.Join(home, "oauth", "kimi-code.lock")
	if err := os.MkdirAll(lock, 0o777); err != nil {
		t.Fatal(err)
	}
	ageKimiLock(t, lock, kimiLockStaleAfter+60*time.Second)

	started := time.Now()
	held, err := acquireKimiLock(context.Background(), home, 10*time.Second)
	if err != nil {
		t.Fatalf("an orphaned lock must not block a refresh: %v", err)
	}
	if waited := time.Since(started); waited >= 5*time.Second {
		t.Fatalf("acquisition waited %v", waited)
	}
	held.Release()
	if _, err := os.Stat(lock); err == nil {
		t.Fatal("the released lock must be gone")
	}
}

func TestAHeartbeatingKimiRefreshLockIsNotBrokenByAWaiter(t *testing.T) {
	home := t.TempDir()
	lock := filepath.Join(home, "oauth", "kimi-code.lock")
	if err := os.MkdirAll(lock, 0o777); err != nil {
		t.Fatal(err)
	}
	_, err := acquireKimiLock(context.Background(), home, 600*time.Millisecond)
	if err == nil {
		t.Fatal("a lock with a live holder must be waited out, not stolen")
	}
	if !strings.Contains(err.Error(), "kimi-code.lock") {
		t.Fatalf("the timeout must name the lock: %v", err)
	}
	if _, err := os.Stat(lock); err != nil {
		t.Fatal("a live holder's lock must survive a waiter")
	}
}

// The Kimi Code CLI breaks a lock whose modification time is more than five
// seconds old, so Mjolnir's beats have to be frequent enough that a stalled
// heartbeat still cannot cost it a live lock.
func TestAHeldKimiLockRepublishesItsMtimeSeveralTimesPerCLIBreakWindow(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1299: Windows file locking and directory times")
	}
	home := t.TempDir()
	held, err := acquireKimiLock(context.Background(), home, kimiLockWait)
	if err != nil {
		t.Fatal(err)
	}
	defer held.Release()
	lock := filepath.Join(home, "oauth", "kimi-code.lock")

	// Half the peer's break window: two beats have to land inside it, so
	// Mjolnir publishes at least four times per window and can miss several
	// in a row and still hold the lock.
	const kimiCLILockStaleAfter = 5 * time.Second
	watched := kimiCLILockStaleAfter / 2
	deadline := time.Now().Add(watched)
	first, ok, err := mtime(lock)
	if err != nil || !ok {
		t.Fatalf("the created lock: %v", err)
	}
	published := []time.Time{first}
	for time.Now().Before(deadline) {
		time.Sleep(50 * time.Millisecond)
		observed, ok, err := mtime(lock)
		if err != nil || !ok {
			t.Fatalf("a held lock: %v", err)
		}
		if !published[len(published)-1].Equal(observed) {
			published = append(published, observed)
		}
		entries, err := os.ReadDir(lock)
		if err != nil {
			t.Fatal(err)
		}
		if len(entries) != 0 {
			t.Fatal("the CLI releases this lock with a plain rmdir, so it must stay empty")
		}
	}
	if len(published) < 3 {
		t.Fatalf("the lock's modification time moved %d times in %v; the Kimi Code CLI breaks a lock after %v without a beat",
			len(published)-1, watched, kimiCLILockStaleAfter)
	}
}

// Child entry point for cross-process OS-lock compatibility and
// cancellation.
func TestCredentialLockPeer(t *testing.T) {
	root := os.Getenv("MJ_CREDENTIAL_LOCK_PEER")
	if root == "" {
		return
	}
	file, err := os.OpenFile(filepath.Join(root, os.Getenv("MJ_CREDENTIAL_LOCK_NAME")), os.O_CREATE|os.O_WRONLY, 0o666)
	if err != nil {
		t.Fatal(err)
	}
	for {
		locked, err := tryLockFile(file)
		if err != nil {
			t.Fatal(err)
		}
		if locked {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	lock := NewFileLock(file)
	defer lock.Drop()
	if err := os.WriteFile(filepath.Join(root, "ready"), []byte("ready"), 0o666); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(15 * time.Second)
	for {
		if _, err := os.Stat(filepath.Join(root, "release")); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("parent did not release peer")
		}
		time.Sleep(10 * time.Millisecond)
	}
	refreshed, err := os.ReadFile(filepath.Join(root, "refreshed.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "auth.json"), refreshed, 0o666); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: #1485: sync overwrote credentials refreshed while it waited.
func TestDestinationLockRereadsAfterProcessRefreshAndCancelledWaiter(t *testing.T) {
	for _, entry := range []struct {
		kind                          HarnessKind
		lockName, incoming, refreshed string
	}{
		{HarnessKindCodex, ".mj-auth.lock", `{"last_refresh":"2026-01-01T00:00:00Z"}`, `{"last_refresh":"2026-02-01T00:00:00Z"}`},
		{HarnessKindMuse, ".auth.json.lock", `{"providers":{"meta":{"expires_at":100}}}`, `{"providers":{"meta":{"expires_at":200}}}`},
	} {
		root := t.TempDir()
		if err := os.WriteFile(filepath.Join(root, "refreshed.json"), []byte(entry.refreshed), 0o666); err != nil {
			t.Fatal(err)
		}
		peer := exec.Command(os.Args[0], "-test.run", "^TestCredentialLockPeer$", "-test.v")
		peer.Env = append(os.Environ(), "MJ_CREDENTIAL_LOCK_PEER="+root, "MJ_CREDENTIAL_LOCK_NAME="+entry.lockName)
		child := SpawnBlocking(func() ProcessOutput {
			output, err := RunCapturingStdout(peer)
			if err != nil {
				panic(err)
			}
			return output
		})
		readyBy := time.Now().Add(10 * time.Second)
		for {
			if _, err := os.Stat(filepath.Join(root, "ready")); err == nil {
				break
			}
			select {
			case <-child.Done():
				t.Fatal("lock peer exited before acquiring lock")
			default:
			}
			if time.Now().After(readyBy) {
				t.Fatal("the lock peer never became ready")
			}
			time.Sleep(10 * time.Millisecond)
		}
		marker := filepath.Join(root, "auth.json")
		cancelledCtx, cancel := context.WithCancel(context.Background())
		cancelled := Spawn(func() error {
			guard, err := CredentialWriteGuardAcquire(cancelledCtx, entry.kind, marker)
			if guard != nil {
				guard.Release()
			}
			return err
		})
		select {
		case <-cancelled.Done():
			t.Fatal("the waiter took a lock the peer holds")
		case <-time.After(150 * time.Millisecond):
		}
		cancel()
		if err, _ := cancelled.Join(context.Background()); !errors.Is(err, context.Canceled) {
			t.Fatalf("the waiter was not cancelled: %v", err)
		}
		other := t.TempDir()
		independentCtx, stop := context.WithTimeout(context.Background(), 2*time.Second)
		independent, err := CredentialWriteGuardAcquire(independentCtx, entry.kind, filepath.Join(other, "auth.json"))
		stop()
		if err != nil {
			t.Fatal(err)
		}
		independent.Release()
		type installed struct {
			install CredentialInstall
			err     error
		}
		install := Spawn(func() installed {
			result, err := InstallCredentialsIfNewer(context.Background(), entry.kind, marker, []byte(entry.incoming))
			return installed{result, err}
		})
		select {
		case <-install.Done():
			t.Fatal("the install ran while the peer held the lock")
		case <-time.After(150 * time.Millisecond):
		}
		if err := os.WriteFile(filepath.Join(root, "release"), []byte("release"), 0o666); err != nil {
			t.Fatal(err)
		}
		output, err := child.Join(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if !output.Status.Success() {
			t.Fatalf("peer failed: %s", output.Stdout)
		}
		waitCtx, stopWait := context.WithTimeout(context.Background(), 5*time.Second)
		result, err := install.Join(waitCtx)
		stopWait()
		if err != nil || result.err != nil {
			t.Fatalf("install: %v %v", err, result.err)
		}
		if result.install.Installed {
			t.Fatal("a stale copy replaced the refreshed one")
		}
		if !result.install.Snapshot.Equal(CredentialSnapshotOf(entry.kind, []byte(entry.refreshed))) {
			t.Fatalf("snapshot %+v", result.install.Snapshot)
		}
		if body, _ := os.ReadFile(marker); string(body) != entry.refreshed {
			t.Fatalf("marker %s", body)
		}
	}
}

// Hard-won: #1485: aliases must share the same credential-slot owner.
func TestHomeAliasesContendWithoutReplacingAuthSymlinks(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	ctx := context.Background()
	root := t.TempDir()
	home := filepath.Join(root, "home")
	if err := os.Mkdir(home, 0o777); err != nil {
		t.Fatal(err)
	}
	alias := filepath.Join(root, "alias")
	if err := os.Symlink(home, alias); err != nil {
		t.Fatal(err)
	}
	held, err := CredentialWriteGuardAcquire(ctx, HarnessKindCodex, filepath.Join(home, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	contended, cancel := context.WithTimeout(ctx, 150*time.Millisecond)
	guard, err := CredentialWriteGuardAcquire(contended, HarnessKindCodex, filepath.Join(alias, "auth.json"))
	cancel()
	if err == nil {
		guard.Release()
		t.Fatal("an alias took the same slot's lock")
	}
	held.Release()
	target := filepath.Join(root, "target.json")
	if err := os.Symlink(target, filepath.Join(home, "auth.json")); err != nil {
		t.Fatal(err)
	}
	earlier := `{"last_refresh":"2026-01-01T00:00:00Z"}`
	later := `{"last_refresh":"2026-02-01T00:00:00Z"}`
	unknown := `{}`
	// Refuse the linked destination before reading, regardless of whether
	// freshness would select a write, equality, retention, or no decision.
	for _, pair := range [][2]string{{unknown, earlier}, {earlier, earlier}, {later, earlier}, {unknown, unknown}} {
		existing, incoming := pair[0], pair[1]
		if err := os.WriteFile(target, []byte(existing), 0o666); err != nil {
			t.Fatal(err)
		}
		_, err := InstallCredentialsIfNewer(ctx, HarnessKindCodex, filepath.Join(alias, "auth.json"), []byte(incoming))
		if err == nil || !strings.Contains(err.Error(), "symbolic link") {
			t.Fatalf("error %v", err)
		}
		if body, _ := os.ReadFile(target); string(body) != existing {
			t.Fatalf("target %s", body)
		}
		info, err := os.Lstat(filepath.Join(home, "auth.json"))
		if err != nil || info.Mode()&os.ModeSymlink == 0 {
			t.Fatal("the link stays")
		}
	}
}
