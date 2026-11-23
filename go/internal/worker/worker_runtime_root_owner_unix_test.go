// Port of the Unix tests in mj-worker/src/worker_runtime/root_owner.rs:
// ForkedPreExecChild, acquired_worker_root_is_owner_only and process_tests.

//go:build unix

package worker

import (
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"golang.org/x/sys/unix"
)

// holdForkedLockCopy starts a process that holds a copy of the owner's lock
// descriptor, as Rust's ForkedPreExecChild holds the forked copy until it
// execs. The returned function ends it. Go cannot run code between fork and
// exec, so the copy is passed to a child that keeps it until it is stopped.
func holdForkedLockCopy(t *testing.T, owner *WorkerRootOwner) func() {
	t.Helper()
	child := exec.Command("/bin/sh", "-c", "exec sleep 60")
	child.ExtraFiles = []*os.File{owner.file.File()}
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	return func() {
		_ = child.Process.Kill()
		_, _ = child.Process.Wait()
	}
}

// The worker root holds the session transcript and harness homes; it must
// never be traversable by other local users (#1409).
func TestAcquiredWorkerRootIsOwnerOnly(t *testing.T) {
	parent := t.TempDir()
	root := filepath.Join(parent, "worker-root")
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatal(err)
	}
	defer owner.Release()
	info, err := os.Stat(root)
	if err != nil {
		t.Fatal(err)
	}
	if mode := info.Mode().Perm(); mode != 0o700 {
		t.Fatalf("mode = %o", mode)
	}
}

const rootOwnerChildEnv = "MJ_ROOT_OWNER_CHILD"

// TestWorkerRootProcessChild is the child process the process tests run;
// without the child variable it does nothing.
func TestWorkerRootProcessChild(t *testing.T) {
	root, ok := os.LookupEnv(rootOwnerChildEnv)
	if !ok {
		return
	}
	switch mode := os.Getenv("MJ_ROOT_OWNER_MODE"); mode {
	case "contender":
		if owner, err := WorkerRootOwnerAcquire(root); err == nil {
			owner.Release()
			t.Fatal("the contender must be refused")
		}
	case "reexec":
		owner, err := WorkerRootOwnerAcquire(root)
		if err != nil {
			t.Fatal(err)
		}
		fd, err := owner.SetReexecInheritance(true)
		if err != nil {
			t.Fatal(err)
		}
		// exec(2) takes the environment as written: the first of two
		// entries with one name would win, so the mode is replaced.
		var environment []string
		for _, entry := range os.Environ() {
			if !strings.HasPrefix(entry, "MJ_ROOT_OWNER_MODE=") && !strings.HasPrefix(entry, "MJ_ROOT_OWNER_FD=") {
				environment = append(environment, entry)
			}
		}
		environment = append(environment, "MJ_ROOT_OWNER_MODE=inherited", "MJ_ROOT_OWNER_FD="+strconv.Itoa(fd))
		err = syscall.Exec(os.Args[0], []string{os.Args[0], "-test.run=^TestWorkerRootProcessChild$"}, environment)
		t.Fatalf("re-exec failed: %v", err)
	case "exit-without-drop":
		owner, err := WorkerRootOwnerAcquire(root)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(owner.Root(), "unclean-exit"), []byte("owned"), 0o666); err != nil {
			t.Fatal(err)
		}
		os.Exit(0)
	case "inherited":
		fd, err := strconv.Atoi(os.Getenv("MJ_ROOT_OWNER_FD"))
		if err != nil {
			t.Fatal(err)
		}
		owner, err := WorkerRootOwnerFromReexec(root, fd)
		if err != nil {
			t.Fatal(err)
		}
		if contender, err := WorkerRootOwnerAcquire(root); err == nil {
			contender.Release()
			t.Fatal("the inherited owner must keep the root")
		}
		// Harness subprocesses must not retain ownership after the worker
		// exits. The adopted descriptor is close-on-exec.
		flags, err := unix.FcntlInt(uintptr(fd), unix.F_GETFD, 0)
		if err != nil {
			t.Fatal(err)
		}
		if flags&unix.FD_CLOEXEC != unix.FD_CLOEXEC {
			t.Fatalf("flags = %#x", flags)
		}
		if err := os.WriteFile(filepath.Join(owner.Root(), "inherited"), []byte("owned"), 0o666); err != nil {
			t.Fatal(err)
		}
	default:
		t.Fatalf("unknown root owner child mode %s", mode)
	}
}

func rootOwnerChild(t *testing.T, root, mode string) {
	t.Helper()
	command := exec.Command(os.Args[0], "-test.run=^TestWorkerRootProcessChild$")
	command.Env = append(os.Environ(), rootOwnerChildEnv+"="+root, "MJ_ROOT_OWNER_MODE="+mode)
	output, err := core.RunCapturingStdout(command)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("child %s failed: %s", mode, output.Stdout)
	}
}

func TestASeparateProcessCannotTouchTheIncumbentBeforeSocketBind(t *testing.T) {
	root := t.TempDir()
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatal(err)
	}
	defer owner.Release()
	journal := filepath.Join(root, "relay-events.jsonl")
	if err := os.WriteFile(journal, []byte("incumbent journal"), 0o666); err != nil {
		t.Fatal(err)
	}
	rootOwnerChild(t, root, "contender")
	body, err := os.ReadFile(journal)
	if err != nil {
		t.Fatal(err)
	}
	if string(body) != "incumbent journal" {
		t.Fatalf("journal = %q", body)
	}
}

func TestProcessExitReleasesOwnershipWithoutRustCleanup(t *testing.T) {
	root := t.TempDir()
	rootOwnerChild(t, root, "exit-without-drop")
	if _, err := os.Stat(filepath.Join(root, "unclean-exit")); err != nil {
		t.Fatal(err)
	}
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatal(err)
	}
	owner.Release()
}

func TestRootOwnershipSurvivesReexecAndIsReleasedWhenTheProcessExits(t *testing.T) {
	root := t.TempDir()
	rootOwnerChild(t, root, "reexec")
	body, err := os.ReadFile(filepath.Join(root, "inherited"))
	if err != nil {
		t.Fatal(err)
	}
	if string(body) != "owned" {
		t.Fatalf("inherited = %q", body)
	}
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatal(err)
	}
	owner.Release()
}
