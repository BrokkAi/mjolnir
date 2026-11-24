// Port of the Linux-only tests in mj-cli/src/daemon.rs
// (`#[cfg(target_os = "linux")]`).

package cli

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestDetachedDaemonCanLaunchAfterItsClientBinaryIsRemoved(t *testing.T) {
	const stage = "MJ_TEST_REMOVED_DAEMON_EXECUTABLE"
	if directory, ok := os.LookupEnv(stage); ok {
		if pathExists(filepath.Join(directory, "client")) {
			check(t, os.Remove(filepath.Join(directory, "client")))
			executable, err := daemonLaunchExecutable()
			check(t, err)
			command := exec.Command(executable, "-test.run=^"+t.Name()+"$", "-test.count=1")
			_, err = core.SpawnDetached(command, filepath.Join(directory, "child.log"))
			check(t, err)
			deadline := time.Now().Add(10 * time.Second)
			for !pathExists(filepath.Join(directory, "launched")) {
				if !time.Now().Before(deadline) {
					t.Fatal("detached child did not launch")
				}
				time.Sleep(10 * time.Millisecond)
			}
		} else {
			check(t, os.WriteFile(filepath.Join(directory, "launched"), []byte("ok"), 0o644))
		}
		return
	}
	// Hard-linked beside the test binary rather than copied: a copy's write
	// descriptor is inherited by children other tests fork until they exec,
	// and exec of the copy in that window fails with ETXTBSY.
	testBinary, err := os.Executable()
	check(t, err)
	directory, err := os.MkdirTemp(filepath.Dir(testBinary), "removed-client")
	check(t, err)
	defer os.RemoveAll(directory)
	executable := filepath.Join(directory, "client")
	check(t, os.Link(testBinary, executable))
	command := exec.Command(executable, "-test.run=^"+t.Name()+"$", "-test.count=1")
	command.Env = append(os.Environ(), stage+"="+directory)
	output, err := core.RunWithInput(command, nil)
	check(t, err)
	if !output.Status.Success() {
		t.Fatalf("%s\n%s", output.Stdout, output.Stderr)
	}
	if !pathExists(filepath.Join(directory, "launched")) {
		t.Fatal("the detached daemon did not launch")
	}
}

func TestCancelledStartupWaitDoesNotRetainTheLock(t *testing.T) {
	path := filepath.Join(t.TempDir(), "daemon-start.lock")
	owner, err := acquireStartGuard(context.Background(), path)
	check(t, err)
	waiter := startAcquiring(exclusive(path))
	if finished, _ := waiter.within(50 * time.Millisecond); finished {
		t.Fatal("the waiter took a lock that was held")
	}
	waiter.cancel()
	if err := <-waiter.done; !errors.Is(err, context.Canceled) {
		t.Fatalf("the cancelled waiter ended with %v", err)
	}
	owner.release()
	replacement := startAcquiring(exclusive(path))
	finished, err := replacement.within(2 * time.Second)
	if !finished {
		t.Fatal("the lock stayed held after the waiter was cancelled")
	}
	check(t, err)
	replacement.guard()
}

// Clients that only use the running daemon check it side by side, while a
// client starting or replacing the daemon excludes all of them, and they
// it. This is what lets a burst of reconnecting clients proceed together yet
// still wait for a handoff to publish its daemon.
func TestSharedHoldsOverlapAndExcludeAReplacingClient(t *testing.T) {
	path := filepath.Join(t.TempDir(), "daemon-start.lock")
	first, err := acquireSharedStartGuard(context.Background(), path)
	check(t, err)
	second := startAcquiring(shared(path))
	finished, err := second.within(2 * time.Second)
	if !finished {
		t.Fatal("a second shared hold waited for the first")
	}
	check(t, err)
	replacing := startAcquiring(exclusive(path))
	if finished, _ := replacing.within(150 * time.Millisecond); finished {
		t.Fatal("an exclusive hold was granted beside shared ones")
	}
	first.release()
	second.guard()
	finished, err = replacing.within(2 * time.Second)
	if !finished {
		t.Fatal("the exclusive hold waited after the shared ones ended")
	}
	check(t, err)
	queued := startAcquiring(shared(path))
	if finished, _ := queued.within(150 * time.Millisecond); finished {
		t.Fatal("a shared hold was granted while a client was replacing the daemon")
	}
	queued.cancel()
	<-queued.done
	replacing.guard()
	last := startAcquiring(shared(path))
	finished, err = last.within(2 * time.Second)
	if !finished {
		t.Fatal("a shared hold waited after the replacement ended")
	}
	check(t, err)
	last.guard()
}

// The restart holds one guard across its stop and its start. That is only
// safe because the lock is not reentrant: a second acquisition inside the
// same process blocks exactly as another process would, so a restart that
// called ConnectOrStart again would wait on itself until the deadline.
func TestTheStartupLockIsNotReentrantWithinOneProcess(t *testing.T) {
	path := filepath.Join(t.TempDir(), "daemon-start.lock")
	held, err := acquireStartGuard(context.Background(), path)
	check(t, err)
	second := startAcquiring(exclusive(path))
	if finished, _ := second.within(100 * time.Millisecond); finished {
		t.Fatal("a second acquisition in this process must block while the first is held")
	}
	second.cancel()
	<-second.done
	held.release()
	third := startAcquiring(exclusive(path))
	finished, err := third.within(2 * time.Second)
	if !finished {
		t.Fatal("the lock stayed held after its release")
	}
	check(t, err)
	third.guard()
}

// The keep-alive is a timer, not a user action. It used to call
// ConnectOrStart, which is how a daemon stopped by a restart came back on
// the attached client's older build before the restart could start its own.
// Hard-won: cbbc2e0d: the unattended keep-alive restarted a daemon from a stale client executable.
func TestTheAttachmentKeepAliveReportsAMissingDaemonWithoutStartingOne(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_KEEP_ALIVE_NEVER_STARTS_A_DAEMON") {
		return
	}
	if pathExists(client.MetadataPath()) {
		t.Fatal("this isolated store starts without a daemon")
	}
	cancellation, cancel := context.WithCancel(context.Background())
	attachment := MaintainAttachment(cancellation, "keep-alive-test")
	// tokio's watch receiver sees a change made before it waits; asking for
	// the change channel before reading the value does the same here.
	changed := attachment.Presence.Changed()
	if attachment.Presence.Borrow() == (DaemonPresenceAttached{}) {
		select {
		case <-changed:
		case <-time.After(20 * time.Second):
			t.Fatal("the keep-alive reports within a few ticks")
		}
	}
	if _, missing := attachment.Presence.Borrow().(DaemonPresenceMissing); !missing {
		t.Fatalf("a missing daemon must be reported, not replaced: %#v", attachment.Presence.Borrow())
	}
	cancel()
	_, err := attachment.Task.Join(context.Background())
	check(t, err)
	// Starting a daemon begins by taking the startup lock, which creates
	// this file. Its absence is the proof that no start was attempted; the
	// missing metadata alone would only prove none succeeded.
	if pathExists(filepath.Join(core.DataDir(), "daemon-start.lock")) {
		t.Fatal("the keep-alive must never attempt to start a daemon")
	}
	if pathExists(client.MetadataPath()) {
		t.Fatal("the keep-alive must never start a daemon")
	}
}
