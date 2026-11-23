// Port of the tests in mj-targets/src/target_storage.rs.

package targets

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func storageSshBare(host string) *core.TargetsTargetLocator {
	return &core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "jonathan@" + host},
		Workspace: "/home/jonathan/Projects/app/.mj/clones/abc",
	}
}

const (
	storageWorker = ".local/share/hel/workers/abc/hel.prepared-x.next"
	storageClone  = "/home/jonathan/Projects/app/.mj/clones/abc/restore.hel.zip"
)

func storageSample(host string, rootFree, projectsFree uint64) []core.HostStorageSample {
	return []core.HostStorageSample{TestStorageSample(host, rootFree, projectsFree)}
}

func storageMustHaveRoom(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

// The board is shared by every test in this process, so each test names
// its own host.
func TestEachWriteIsJudgedByTheFilesystemItLandsOn(t *testing.T) {
	host := "precision-3260"
	target := storageSshBare(host)
	// Not measured yet: unknown is not a reason to refuse.
	storageMustHaveRoom(t, EnsureRoom(target, storageWorker, 100<<20, "stage the worker"))

	// A full ~/Projects refuses a restore into a clone there, but does not
	// block worker staging on a healthy /.
	RecordSamples(storageSample(host, 40<<30, 0), nowEpochSeconds())
	storageMustHaveRoom(t, EnsureRoom(target, storageWorker, 100<<20, "stage the worker"))
	err := EnsureRoom(target, storageClone, 100<<20, "restore the checkpoint")
	if err == nil {
		t.Fatal("a restore into a full filesystem was allowed")
	}
	if !IsStorageFull(err) {
		t.Fatalf("not a storage-full error: %v", err)
	}
	text := err.Error()
	if !strings.Contains(text, "precision-3260 has 0 B free on /home/jonathan/Projects") {
		t.Fatal(text)
	}
	if !strings.Contains(text, "free space on precision-3260 to continue") {
		t.Fatal(text)
	}

	// And the other way round: a full / blocks staging, not the restore.
	RecordSamples(storageSample(host, 0, 40<<30), nowEpochSeconds())
	err = EnsureRoom(target, storageWorker, 100<<20, "stage the worker")
	if err == nil {
		t.Fatal("staging onto a full filesystem was allowed")
	}
	if text := err.Error(); !strings.Contains(text, "precision-3260 has 0 B free on / (the filesystem reserves 26.84 GB more for root)") {
		t.Fatal(text)
	}
	storageMustHaveRoom(t, EnsureRoom(target, storageClone, 100<<20, "restore the checkpoint"))

	// Room for the write, but not for the reserve beside it.
	RecordSamples(storageSample(host, core.WriteReserveBytes+(50<<20), 40<<30), nowEpochSeconds())
	if EnsureRoom(target, storageWorker, 100<<20, "stage the worker") == nil {
		t.Fatal("a write into the reserve was allowed")
	}
	storageMustHaveRoom(t, EnsureRoom(target, storageWorker, 10<<20, "write a config"))
}

func TestAFailedWriteMarksTheFilesystemItNamesUntilALaterMeasurement(t *testing.T) {
	host := "observed-3260"
	target := storageSshBare(host)
	now := nowEpochSeconds()
	RecordSamples(storageSample(host, 20<<30, 20<<30), now)
	changes := Subscribe().Changed()

	ObserveNoSpace(host, `scp: write remote "/home/jonathan/Projects/app/.mj/clones/abc/x": No space left on device`)
	select {
	case <-changes:
	default:
		t.Fatal("the failure did not change the board")
	}
	if EnsureRoom(target, storageClone, 0, "restore") == nil {
		t.Fatal("a restore onto the failed filesystem was allowed")
	}
	storageMustHaveRoom(t, EnsureRoom(target, storageWorker, 0, "stage the worker"))

	// A probe that may have started before the failure does not clear it.
	RecordSamples(storageSample(host, 20<<30, 20<<30), nowEpochSeconds())
	if EnsureRoom(target, storageClone, 0, "restore") == nil {
		t.Fatal("an early probe cleared the failure")
	}
	RecordSamples(storageSample(host, 20<<30, 20<<30), nowEpochSeconds()+noSpaceHoldSeconds)
	storageMustHaveRoom(t, EnsureRoom(target, storageClone, 0, "restore"))
}

func TestAFailureNamingNoPathHoldsTheHostUntilMeasured(t *testing.T) {
	host := "unattributed-3260"
	target := storageSshBare(host)
	RecordSamples(storageSample(host, 20<<30, 20<<30), nowEpochSeconds())
	ObserveNoSpace(host, "Error response from daemon: ENOSPC: no space left on device")
	if EnsureRoom(target, storageWorker, 0, "stage the worker") == nil {
		t.Fatal("staging was allowed while the host is held")
	}
	if EnsureRoom(target, storageClone, 0, "restore") == nil {
		t.Fatal("a restore was allowed while the host is held")
	}
	// The measurement it asked for decides: here, both have room.
	RecordSamples(storageSample(host, 20<<30, 20<<30), nowEpochSeconds()+noSpaceHoldSeconds)
	storageMustHaveRoom(t, EnsureRoom(target, storageWorker, 0, "stage the worker"))
}

func TestASessionIsDiskFullOnlyWhenOneOfItsOwnFilesystemsIs(t *testing.T) {
	host := "session-3260"
	session := core.TargetLocatorSshBare{Host: host, Workspace: "/home/jonathan/Projects/app/.mj/clones/abc"}
	elsewhere := core.TargetLocatorSshBare{Host: host, Workspace: "/srv/work/abc"}
	RecordSamples(storageSample(host, 40<<30, 0), nowEpochSeconds())
	_, problem, ok := WorkerRestartProblem(session, "abc", nil)
	if !ok {
		t.Fatal("the session's full filesystem was not reported")
	}
	if !strings.HasPrefix(problem, "disk full: session-3260 has 0 B free on /home/jonathan/Projects") {
		t.Fatal(problem)
	}
	if _, problem, ok := WorkerRestartProblem(elsewhere, "abc", nil); ok {
		t.Fatalf("a session elsewhere is reported full: %s", problem)
	}
}
