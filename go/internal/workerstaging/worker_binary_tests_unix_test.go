//go:build unix

// Port of mj-worker-staging/src/worker_binary/tests.rs: the tests and
// helpers under `#[cfg(unix)]` that stage and run control workers.

package workerstaging

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func fileInode(t *testing.T, path string) uint64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Sys().(*syscall.Stat_t).Ino
}

func fileLinkCount(t *testing.T, path string) uint64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return uint64(info.Sys().(*syscall.Stat_t).Nlink)
}

// writeStampedWorkerScript writes an executable worker stand-in: script,
// then this build's worker stamp after it.
func writeStampedWorkerScript(t *testing.T, path, script string) {
	t.Helper()
	body := append([]byte(script), core.WorkerBuildStamp()...)
	writeFixture(t, path, body)
	if err := os.Chmod(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func dirEntryCount(t *testing.T, path string) int {
	t.Helper()
	entries, err := os.ReadDir(path)
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func workerTestPermit(t *testing.T, session string) *store.WorkerPermit {
	t.Helper()
	owner, err := store.WorkerTestPermit(context.Background(), session, "test stop")
	if err != nil {
		t.Fatal(err)
	}
	if owner == nil {
		t.Fatal("the test gate refused a worker permit")
	}
	return owner
}

// Hard-won: 816ca1d77: the controller ran an old root binary that lacks the worker control subcommands
func TestProbingStoppingAndClosingARootWithAnOldWorkerRunTheRootsControlWorker(t *testing.T) {
	const child = "MJ_OLD_ROOT_WORKER_CHILD"
	const session = "17171717171717171717171717171717"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		// The current worker: answers the control subcommands, then carries
		// this build's stamp after its `exit`.
		current := filepath.Join(directory, "current-worker")
		writeStampedWorkerScript(t, current, fmt.Sprintf(
			"#!/bin/sh\ncase \"$2\" in build-id) echo %s ;; liveness) echo dead ;; esac\nexit 0\n", core.WorkerBuildID()))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestProbingStoppingAndClosingARootWithAnOldWorkerRunTheRootsControlWorker")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-816ca1d77-old-root").
			Env(child, "1").
			Env("MJ_WORKER_BINARY", current).
			Run()
		return
	}
	directory := t.TempDir()
	root := filepath.Join(directory, "workers", session)
	makeFixtureDir(t, root)
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	// A build from before `worker stop|liveness|holder`: clap's own
	// rejection.
	const old = "#!/bin/sh\necho \"error: unrecognized subcommand '$2'\" >&2\nexit 2\n"
	hel := filepath.Join(root, "hel")
	writeFixture(t, hel, []byte(old))
	if err := os.Chmod(hel, 0o755); err != nil {
		t.Fatal(err)
	}
	helInode := fileInode(t, hel)
	unchanged := func() {
		t.Helper()
		if readFixture(t, hel) != old || fileInode(t, hel) != helInode {
			t.Fatal("the root's own hel changed")
		}
	}

	liveness, err := ProbeWorkerLiveness(core.ProcessExecutor{}, &locator, session, root)
	if err != nil {
		t.Fatal(err)
	}
	if liveness.Status != 0 || strings.TrimSpace(lossyText(liveness.Stdout)) != "dead" {
		t.Fatalf("liveness %d %q", liveness.Status, liveness.Stdout)
	}
	// The control worker is a hard link to this build's store entry, in the
	// root itself, so the entry's link count went up and the root owns it.
	control := targets.ControlWorkerPath(root)
	if filepath.Dir(control) != root {
		t.Fatalf("control worker %s is outside the root", control)
	}
	digest, err := core.WorkerExecutableDigest(os.Getenv("MJ_WORKER_BINARY"))
	if err != nil {
		t.Fatal(err)
	}
	entry := filepath.Join(directory, "workers/worker-binaries", digest, "hel")
	if fileInode(t, control) != fileInode(t, entry) {
		t.Fatal("the control worker is not the store entry")
	}
	if fileLinkCount(t, entry) != 2 {
		t.Fatal("the store entry and the root's link")
	}
	unchanged()

	owner := workerTestPermit(t, session)
	if err := StopWorker(owner, core.ProcessExecutor{}, &locator, root); err != nil {
		t.Fatal(err)
	}
	unchanged()

	// The close plan stops the worker (embedding the same control binary)
	// and then removes the root, and the control worker with it.
	plan, err := targets.ClosePlan(&locator, session)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := plan.Execute(core.ProcessExecutor{}, ControlWorker{}); err != nil {
		t.Fatal(err)
	}
	if fixtureExists(root) {
		t.Fatal("the close left the root")
	}
}

// A plan that embeds the control worker installs it when it runs, so a
// cleanup with no earlier probe or stop (a failed provisioning) still works.
func TestAClosePlanStagesTheControlWorkerItselfOnAFreshBareTarget(t *testing.T) {
	const child = "MJ_FRESH_BARE_CLOSE_CHILD"
	const session = "18181818181818181818181818181818"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		current := filepath.Join(directory, "current-worker")
		writeStampedWorkerScript(t, current, fmt.Sprintf(
			"#!/bin/sh\ncase \"$2\" in build-id) echo %s ;; esac\nexit 0\n", core.WorkerBuildID()))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestAClosePlanStagesTheControlWorkerItselfOnAFreshBareTarget")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-816ca1d77-fresh-close").
			Env(child, "1").
			Env("MJ_WORKER_BINARY", current).
			Run()
		return
	}
	directory := t.TempDir()
	root := filepath.Join(directory, "worker's space", session)
	makeFixtureDir(t, root)
	hel := filepath.Join(root, "hel")
	writeFixture(t, hel, []byte("#!/bin/sh\nexit 2\n"))
	helInode := fileInode(t, hel)
	control := targets.ControlWorkerPath(root)
	if fixtureExists(control) {
		t.Fatal("nothing is staged yet")
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	plan, err := targets.ClosePlan(&locator, session)
	if err != nil {
		t.Fatal(err)
	}
	if !plan.StagesControlWorker() {
		t.Fatal("the close plan does not stage the control worker")
	}
	// A caller's deadline bounds the command itself: with the deadline
	// already past, nothing runs, nothing is staged and nothing is removed.
	expired := core.CancellableProcessExecutorWithTimeout(0)
	_, err = plan.Execute(expired, ControlWorker{})
	requireErrorContains(t, err, "timed out")
	if fixtureExists(control) {
		t.Fatal("the check ignored the caller's deadline")
	}
	if !fixtureExists(root) {
		t.Fatal("the timed-out close removed something")
	}
	// Stage once so the store entry exists, then install a `hel` that
	// carries the interface: the root's control worker has nothing left to
	// do, and its link must not pin the store entry.
	stop := StopWorkerCommand(&locator, root).UnstagedCommand()
	if _, err := RunControlCommand(core.ProcessExecutor{}, &locator, session, &stop); err != nil {
		t.Fatal(err)
	}
	if !fixtureExists(control) {
		t.Fatal("the control worker was not staged")
	}
	// A control file staged under an earlier release's hashed name goes
	// too.
	legacy := filepath.Join(root, "hel-control-0123456789abcdef")
	if err := os.Link(control, legacy); err != nil {
		t.Fatal(err)
	}
	digest, err := core.WorkerExecutableDigest(os.Getenv("MJ_WORKER_BINARY"))
	if err != nil {
		t.Fatal(err)
	}
	storeRoot, err := StoreRoot(&locator, session)
	if err != nil {
		t.Fatal(err)
	}
	entry := filepath.Join(storeRoot, digest, "hel")
	if fileLinkCount(t, entry) != 3 {
		t.Fatalf("store entry links: %d", fileLinkCount(t, entry))
	}
	worker, err := WorkerInstallForTarget(&locator, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if err := InstallWorkerBinary(core.ProcessExecutor{}, &locator, session, worker, "hel"); err != nil {
		t.Fatal(err)
	}
	if fixtureExists(control) || fixtureExists(legacy) {
		t.Fatal("the control workers survived the install")
	}
	if fileLinkCount(t, entry) != 2 {
		t.Fatal("the store entry and the root's hel")
	}
	if fileInode(t, hel) == helInode {
		t.Fatal("hel was not replaced")
	}
	// With the interface in `hel`, closing needs no staging at all.
	if _, err := plan.Execute(core.ProcessExecutor{}, ControlWorker{}); err != nil {
		t.Fatal(err)
	}
	if fixtureExists(root) {
		t.Fatal("the close left the root")
	}
}

// Hard-won: #1573: a sub-agent suspend queued behind another upload of this
// build held daemon admission for the whole wait, so a handoff waited 440 s on
// it. The wait now releases admission and a handoff's cancellation ends it
// without taking admission back.
func TestAControlWorkerInstallWaitsWithoutHoldingAdmissionAndACancelEndsIt(t *testing.T) {
	const child = "MJ_CONTROL_INSTALL_ADMISSION_CHILD"
	const session = "19191919191919191919191919191919"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		current := filepath.Join(directory, "current-worker")
		writeStampedWorkerScript(t, current, fmt.Sprintf(
			"#!/bin/sh\ncase \"$2\" in build-id) echo %s ;; esac\nexit 0\n", core.WorkerBuildID()))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestAControlWorkerInstallWaitsWithoutHoldingAdmissionAndACancelEndsIt")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-1573-control-admission").
			Env(child, "1").
			Env("MJ_WORKER_BINARY", current).
			Run()
		return
	}
	directory := t.TempDir()
	root := filepath.Join(directory, "workers", session)
	makeFixtureDir(t, root)
	writeFixture(t, filepath.Join(root, "hel"), []byte("#!/bin/sh\nexit 2\n"))
	control := targets.ControlWorkerPath(root)
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	digest, err := core.WorkerExecutableDigest(os.Getenv("MJ_WORKER_BINARY"))
	if err != nil {
		t.Fatal(err)
	}
	storeRoot, err := StoreRoot(&locator, session)
	if err != nil {
		t.Fatal(err)
	}
	directoryKey := core.Text(storeRoot) + "/" + digest
	// Another upload of this build owns the publication.
	gate := StoreGateFor(&locator, directoryKey)
	otherUpload := gate.TryLock("other-session", "publish a worker build")
	if otherUpload == nil {
		t.Fatal("the gate was already held")
	}
	cancelled := new(atomic.Bool)
	var held atomic.Bool
	held.Store(true)
	var reacquired atomic.Int64
	inner := core.NewCancellableProcessExecutor(cancelled)
	admission := &workerTestExecutor{
		execute:                         inner.Execute,
		cancelled:                       cancelled.Load,
		cancelledWithoutDeadline:        cancelled.Load,
		pauseDeadline:                   inner.PauseDeadline,
		executeWithoutDeadline:          inner.ExecuteWithoutDeadline,
		executeWithStdinWithoutDeadline: inner.ExecuteWithStdinWithoutDeadline,
		yieldAdmission:                  func() bool { return held.Swap(false) },
		endResumable: func(string) error {
			reacquired.Add(1)
			held.Store(true)
			return nil
		},
	}
	stop := StopWorkerCommand(&locator, root).UnstagedCommand()
	type result struct {
		output core.CommandOutput
		err    error
	}
	waiter := make(chan result, 1)
	go func() {
		output, err := RunControlCommand(admission, &locator, session, &stop)
		waiter <- result{output, err}
	}()
	deadline := time.Now().Add(30 * time.Second)
	for held.Load() {
		if time.Now().After(deadline) {
			t.Fatal("the wait never released admission")
		}
		select {
		case <-waiter:
			t.Fatal("the install finished behind a held gate")
		default:
		}
		time.Sleep(10 * time.Millisecond)
	}
	// A handoff cancels the wait; admission stays released.
	cancelled.Store(true)
	waited := <-waiter
	requireErrorContains(t, waited.err, "cancelled")
	if reacquired.Load() != 0 || held.Load() || fixtureExists(control) {
		t.Fatal("the cancelled wait took admission back or staged the worker")
	}
	// Once the other upload is gone, the same install completes and takes
	// admission back before returning.
	otherUpload.Release()
	cancelled.Store(false)
	held.Store(true)
	output, err := RunControlCommand(admission, &locator, session, &stop)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 || !fixtureExists(control) {
		t.Fatal("the install did not complete")
	}
	if reacquired.Load() != 1 || !held.Load() {
		t.Fatal("the install did not take admission back")
	}
}

// countingExecutor counts the commands a control run executes.
func countingExecutor(count *int) *workerTestExecutor {
	process := core.ProcessExecutor{}
	return &workerTestExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			*count++
			return process.Execute(command)
		},
		cancelledWithoutDeadline:        func() bool { return false },
		pauseDeadline:                   process.PauseDeadline,
		executeWithoutDeadline:          process.ExecuteWithoutDeadline,
		executeWithStdinWithoutDeadline: process.ExecuteWithStdinWithoutDeadline,
	}
}

// inIsolatedStoreWithWorker runs test in a child process whose worker store
// is isolated and whose current worker is a stamped shell script of
// workerScript. The parent returns true after the child passes.
func inIsolatedStoreWithWorker(t *testing.T, test, workerScript string) bool {
	t.Helper()
	const child = "MJ_CONTROL_STAGING_CHILD"
	if _, ok := os.LookupEnv(child); ok {
		return false
	}
	directory := t.TempDir()
	current := filepath.Join(directory, "current-worker")
	writeStampedWorkerScript(t, current, "#!/bin/sh\n"+workerScript+"\n")
	targets.NewIsolatedTest(targets.TestName(testsModule, test)).
		IsolatedStore(directory).
		Env("MJ_INSTANCE", "control-staging").
		Env(child, "1").
		Env("MJ_WORKER_BINARY", current).
		Run()
	return true
}

// A root whose own `hel` serves the subcommand runs it there: one command,
// and nothing is published to the store or staged in the root.
func TestARootWhoseHelServesTheCommandStopsWithoutStaging(t *testing.T) {
	const session = "21212121212121212121212121212121"
	if inIsolatedStoreWithWorker(t, "TestARootWhoseHelServesTheCommandStopsWithoutStaging", "exit 0") {
		return
	}
	directory := t.TempDir()
	root := filepath.Join(directory, "workers", session)
	makeFixtureDir(t, root)
	hel := filepath.Join(root, "hel")
	writeFixture(t, hel, []byte("#!/bin/sh\nexit 0\n"))
	if err := os.Chmod(hel, 0o755); err != nil {
		t.Fatal(err)
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	count := 0
	executor := countingExecutor(&count)
	owner := workerTestPermit(t, session)
	if err := StopWorker(owner, executor, &locator, root); err != nil {
		t.Fatal(err)
	}
	if _, err := ProbeWorkerLiveness(executor, &locator, session, root); err != nil {
		t.Fatal(err)
	}
	if count != 2 {
		t.Fatalf("each control command ran exactly once, not %d", count)
	}
	if dirEntryCount(t, root) != 1 {
		t.Fatal("the root gained a file")
	}
	if fixtureExists(filepath.Join(directory, "workers/worker-binaries")) {
		t.Fatal("a build was published to the store")
	}
}

// A root nothing in serves gets the control worker staged on the 78, and the
// command rerun exactly once. When the staged build still does not serve, the
// second 78 is an error rather than a loop.
func TestStatus78StagesTheControlWorkerAndRerunsOnce(t *testing.T) {
	const session = "22222222222222222222222222222222"
	const serves = "MJ_CONTROL_STAGING_SERVES"
	for _, servesCommand := range []bool{true, false} {
		const test = "TestStatus78StagesTheControlWorkerAndRerunsOnce"
		// The staged build answers `--help` with 0, or rejects it as clap
		// does.
		worker := "exit 2"
		marker := "0"
		if servesCommand {
			worker, marker = "exit 0", "1"
		}
		if _, ok := os.LookupEnv("MJ_CONTROL_STAGING_CHILD"); !ok {
			// Parent: run the whole body once per worker behaviour.
			directory := t.TempDir()
			current := filepath.Join(directory, "current-worker")
			writeStampedWorkerScript(t, current, "#!/bin/sh\n"+worker+"\n")
			targets.NewIsolatedTest(targets.TestName(testsModule, test)).
				IsolatedStore(directory).
				Env("MJ_INSTANCE", "control-staging-78").
				Env("MJ_CONTROL_STAGING_CHILD", "1").
				Env(serves, marker).
				Env("MJ_WORKER_BINARY", current).
				Run()
			continue
		}
		if os.Getenv(serves) != marker {
			continue
		}
		directory := t.TempDir()
		root := filepath.Join(directory, "workers", session)
		makeFixtureDir(t, root)
		writeFixture(t, filepath.Join(root, "hel"), []byte("#!/bin/sh\nexit 2\n"))
		control := targets.ControlWorkerPath(root)
		locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
		count := 0
		executor := countingExecutor(&count)
		command := StopWorkerCommand(&locator, root).UnstagedCommand()
		output, err := RunControlCommand(executor, &locator, session, &command)
		if count != 2 {
			t.Fatalf("one run, the staging, one rerun: %d", count)
		}
		if !fixtureExists(control) {
			t.Fatal("the control worker was not staged")
		}
		if servesCommand {
			if err != nil || output.Status != 0 {
				t.Fatalf("%v %d", err, output.Status)
			}
		} else {
			requireErrorContains(t, err, "does not serve")
		}
	}
}

// A diagnosis reports and installs nothing: a root with no control worker
// for this build gets a finding that says so, and no file appears.
func TestADiagnosisReportsAMissingControlWorkerAndInstallsNothing(t *testing.T) {
	const session = "20202020202020202020202020202020"
	directory := t.TempDir()
	root := filepath.Join(directory, "workers", session)
	makeFixtureDir(t, root)
	writeFixture(t, filepath.Join(root, "hel"), []byte("#!/bin/sh\nexit 2\n"))
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	before := dirEntryCount(t, filepath.Join(directory, "workers"))
	_, err := DiagnoseWorkerReadOnly(core.ProcessExecutor{}, &locator, root)
	requireErrorContains(t, err, fmt.Sprintf("no worker in %s serves 'worker holder'", root))
	if fixtureExists(targets.ControlWorkerPath(root)) {
		t.Fatal("the diagnosis staged a control worker")
	}
	if dirEntryCount(t, filepath.Join(directory, "workers")) != before {
		t.Fatal("the diagnosis created something beside the root")
	}
	if dirEntryCount(t, root) != 1 {
		t.Fatal("the diagnosis created something in the root")
	}
}

// The readiness probe is a decision, so like every command that embeds the
// control worker it installs it first: a startup wait on a root with none
// must not fail with exit 78.
func TestProbingAWorkerInstallsTheControlWorkerItself(t *testing.T) {
	const child = "MJ_PROBE_STAGES_CHILD"
	const session = "19191919191919191919191919191919"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		current := filepath.Join(directory, "current-worker")
		writeStampedWorkerScript(t, current, fmt.Sprintf(
			"#!/bin/sh\ncase \"$2\" in build-id) echo %s ;; esac\nexit 0\n", core.WorkerBuildID()))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestProbingAWorkerInstallsTheControlWorkerItself")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-816ca1d77-probe-stages").
			Env(child, "1").
			Env("MJ_WORKER_BINARY", current).
			Run()
		return
	}
	directory := t.TempDir()
	root := filepath.Join(directory, "workers", session)
	makeFixtureDir(t, root)
	writeFixture(t, filepath.Join(root, "hel"), []byte("#!/bin/sh\nexit 2\n"))
	control := targets.ControlWorkerPath(root)
	if fixtureExists(control) {
		t.Fatal("nothing is staged yet")
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	probe, err := ProbeWorker(core.ProcessExecutor{}, &locator, session, root)
	if err != nil {
		t.Fatal(err)
	}
	if !fixtureExists(control) {
		t.Fatal("the probe did not stage the control worker")
	}
	if len(probe.Pids) != 0 {
		t.Fatalf("%v", probe)
	}
}

var _ io.Reader = (*strings.Reader)(nil)
