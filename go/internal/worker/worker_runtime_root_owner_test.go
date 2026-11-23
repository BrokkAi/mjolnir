// Port of the tests in mj-worker/src/worker_runtime/root_owner.rs.

package worker

import (
	"os"
	"path/filepath"
	"testing"
)

// Hard-won: #1257: a forked pre-exec child keeps the flock after the Rust owner drops.
func TestRootIsExclusiveBeforeAControlSocketExistsAndReleasedOnDrop(t *testing.T) {
	root := t.TempDir()
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatal(err)
	}
	if contender, err := WorkerRootOwnerAcquire(root); err == nil {
		contender.Release()
		t.Fatal("a second owner must be refused")
	}
	other := t.TempDir()
	independent, err := WorkerRootOwnerAcquire(other)
	if err != nil {
		t.Fatal(err)
	}
	defer independent.Release()
	// Another process holds a copy of the lock's open file description, as
	// a child forked before exec does.
	release := holdForkedLockCopy(t, owner)
	owner.Release()
	acquired, err := WorkerRootOwnerAcquire(root)
	release()
	if err != nil {
		t.Fatal(err)
	}
	acquired.Release()
}

func TestARefusedOwnerCannotModifyTheIncumbentsFiles(t *testing.T) {
	root := t.TempDir()
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatal(err)
	}
	defer owner.Release()
	names := []string{"worker.log", "worker-startup.json", "worker-exit.json", "worker.pid"}
	for _, name := range names {
		if err := os.WriteFile(filepath.Join(root, name), []byte("incumbent"), 0o666); err != nil {
			t.Fatal(err)
		}
	}
	if contender, err := WorkerRootOwnerAcquire(root); err == nil {
		contender.Release()
		t.Fatal("a second owner must be refused")
	}
	for _, name := range names {
		body, err := os.ReadFile(filepath.Join(root, name))
		if err != nil {
			t.Fatal(err)
		}
		if string(body) != "incumbent" {
			t.Fatalf("%s = %q", name, body)
		}
	}
}
