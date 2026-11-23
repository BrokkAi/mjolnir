// Port of the tests in mj-worker/src/authorized_keys.rs.

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"
)

const authorizedKeysSamplePubkey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl"

func TestConcurrentInstallsAndRemovalsPreserveGrantsAndPermissions(t *testing.T) {
	dir := t.TempDir()
	sshDir := filepath.Join(dir, ".ssh")
	authKeys := filepath.Join(sshDir, "authorized_keys")

	// Pre-populate with an unrelated user key.
	if err := createSSHDir(sshDir); err != nil {
		t.Fatalf("create .ssh: %v", err)
	}
	userKey := "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB user@host"
	if err := os.WriteFile(authKeys, []byte(userKey+"\n"), 0o600); err != nil {
		t.Fatalf("write user key: %v", err)
	}
	if runtime.GOOS != "windows" {
		if err := os.Chmod(authKeys, 0o600); err != nil {
			t.Fatalf("chmod 0600: %v", err)
		}
	}

	// Pre-install 8 keys that will be removed concurrently while 8 new keys
	// are installed concurrently.
	for i := range 8 {
		if err := InstallKey(authKeys, fmt.Sprintf("remove-%d", i), "10.0.0.1", "mj-worker worker receive-move-stream", authorizedKeysSamplePubkey); err != nil {
			t.Fatalf("pre-install: %v", err)
		}
	}

	var start, done sync.WaitGroup
	start.Add(16)
	errs := make(chan error, 16)
	for i := range 8 {
		done.Add(1)
		go func() {
			defer done.Done()
			start.Done()
			start.Wait()
			if err := InstallKey(authKeys, fmt.Sprintf("keep-%d", i), "10.0.0.2", "mj-worker worker receive-move-stream", authorizedKeysSamplePubkey); err != nil {
				errs <- fmt.Errorf("concurrent install: %w", err)
			}
		}()
	}
	for i := range 8 {
		done.Add(1)
		go func() {
			defer done.Done()
			start.Done()
			start.Wait()
			removed, err := RemoveKey(authKeys, fmt.Sprintf("remove-%d", i))
			if err != nil {
				errs <- fmt.Errorf("concurrent remove: %w", err)
				return
			}
			if !removed {
				errs <- fmt.Errorf("expected remove-%d to be removed", i)
			}
		}()
	}
	done.Wait()
	close(errs)
	for err := range errs {
		t.Fatal(err)
	}

	contents, err := os.ReadFile(authKeys)
	if err != nil {
		t.Fatalf("read final authorized_keys: %v", err)
	}
	lines := authorizedKeysLines(string(contents))
	if !slices.Contains(lines, userKey) {
		t.Fatal("unrelated user key must survive")
	}
	for i := range 8 {
		keepComment := fmt.Sprintf("mj-move-keep-%d", i)
		if !slices.ContainsFunc(lines, func(l string) bool { return hasManagedComment(l, keepComment) }) {
			t.Fatalf("concurrently installed key %s was lost", keepComment)
		}
		removeComment := fmt.Sprintf("mj-move-remove-%d", i)
		if slices.ContainsFunc(lines, func(l string) bool { return hasManagedComment(l, removeComment) }) {
			t.Fatalf("removed key %s was resurrected", removeComment)
		}
	}

	if runtime.GOOS != "windows" {
		info, err := os.Stat(authKeys)
		if err != nil {
			t.Fatalf("metadata: %v", err)
		}
		if mode := info.Mode().Perm(); mode != 0o600 {
			t.Fatalf("authorized_keys must stay 0600, got %o", mode)
		}
	}

	// Reconcile keeps only keep-0 and keep-1, leaving userKey untouched.
	active := map[string]struct{}{"keep-0": {}, "keep-1": {}}
	reconciled, err := ReconcileKeys(authKeys, active)
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if reconciled != 6 {
		t.Fatalf("reconciled = %d, want 6", reconciled)
	}
	finalContents, err := os.ReadFile(authKeys)
	if err != nil {
		t.Fatalf("read reconciled: %v", err)
	}
	finalLines := authorizedKeysLines(string(finalContents))
	if !slices.Contains(finalLines, userKey) {
		t.Fatal("user key must survive reconcile")
	}
	if len(finalLines) != 3 {
		t.Fatalf("final line count = %d, want 3", len(finalLines))
	}
}

func TestInstallRejectsControlCharsAndUnsafeQuoting(t *testing.T) {
	authKeys := filepath.Join(t.TempDir(), ".ssh", "authorized_keys")

	if InstallKey(authKeys, "op1", "10.0.0.1\",command=\"evil", "mj-worker", authorizedKeysSamplePubkey) == nil {
		t.Fatal("quote in source address must be rejected")
	}
	if InstallKey(authKeys, "op1", "10.0.0.1", "mj-worker\nssh-ed25519 AAAA", authorizedKeysSamplePubkey) == nil {
		t.Fatal("newline in command must be rejected")
	}
	if InstallKey(authKeys, "op1", "10.0.0.1", "mj-worker", authorizedKeysSamplePubkey+"\nextra") == nil {
		t.Fatal("newline in public key must be rejected")
	}
	if InstallKey(authKeys, "op 1", "10.0.0.1", "mj-worker", authorizedKeysSamplePubkey) == nil {
		t.Fatal("space in operation id must be rejected")
	}
}

func TestLeavesUnrelatedKeyWhoseCommentEndsWithMjMoveUntouched(t *testing.T) {
	sshDir := filepath.Join(t.TempDir(), ".ssh")
	authKeys := filepath.Join(sshDir, "authorized_keys")
	if err := createSSHDir(sshDir); err != nil {
		t.Fatalf("create .ssh: %v", err)
	}

	unrelated := authorizedKeysSamplePubkey + " alice laptop mj-move-op1"
	if err := os.WriteFile(authKeys, []byte(unrelated+"\n"), 0o600); err != nil {
		t.Fatalf("write unrelated key: %v", err)
	}

	// Install, remove, and reconcile for `op1` must never touch `unrelated`.
	if err := InstallKey(authKeys, "op1", "10.0.0.1", "mj-worker", authorizedKeysSamplePubkey); err != nil {
		t.Fatalf("install: %v", err)
	}
	removed, err := RemoveKey(authKeys, "op1")
	if err != nil {
		t.Fatalf("remove: %v", err)
	}
	if !removed {
		t.Fatal("the managed op1 key must be removed")
	}
	reconciled, err := ReconcileKeys(authKeys, map[string]struct{}{})
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if reconciled != 0 {
		t.Fatalf("reconciled = %d, want 0", reconciled)
	}

	contents, err := os.ReadFile(authKeys)
	if err != nil {
		t.Fatalf("read final: %v", err)
	}
	if got := strings.TrimSpace(string(contents)); got != unrelated {
		t.Fatalf("contents = %q, want %q", got, unrelated)
	}
}
