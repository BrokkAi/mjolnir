// Port of the test-only items of mj-worker-staging/src/worker_binary/store.rs
// (`#[cfg(all(test, unix))]`) and of its tests,
// mj-worker-staging/src/worker_binary/store/tests.rs.

//go:build unix

package workerstaging

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"maps"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// ForgetCollections clears every store's collection claim, so a test's
// install collects as a fresh daemon's would.
func ForgetCollections() {
	collections.mu.Lock()
	defer collections.mu.Unlock()
	clear(collections.last)
}

// storeTestSource writes a stamped worker named name under root.
func storeTestSource(t *testing.T, root, name string) string {
	t.Helper()
	path := filepath.Join(root, name)
	body := bytes.Repeat([]byte{'x'}, 256*1024)
	body = append(body, name...)
	body = append(body, core.WorkerBuildStamp()...)
	if err := os.WriteFile(path, body, 0o666); err != nil {
		t.Fatal(err)
	}
	return path
}

// storeTestTarget is a local bare session whose worker root is root/id.
func storeTestTarget(t *testing.T, root, id string) core.TargetsTargetLocator {
	t.Helper()
	workerRoot := filepath.Join(root, id)
	if err := os.MkdirAll(workerRoot, 0o777); err != nil {
		t.Fatal(err)
	}
	return core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}
}

// storeTestInstall installs source as the session's `hel`.
func storeTestInstall(executor core.CommandExecutor, locator core.TargetsTargetLocator, id, source string) error {
	return InstallWorkerBinary(executor, &locator, id, WorkerInstallHost{Path: source}, "hel")
}

// storeMust fails the test on err; for setup steps Rust unwraps.
func storeMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func storeRead(t *testing.T, path string) []byte {
	t.Helper()
	body, err := os.ReadFile(path)
	storeMust(t, err)
	return body
}

func storeInode(t *testing.T, path string) uint64 {
	t.Helper()
	info, err := os.Stat(path)
	storeMust(t, err)
	return uint64(info.Sys().(*syscall.Stat_t).Ino)
}

func storeLinks(t *testing.T, path string) uint64 {
	t.Helper()
	info, err := os.Stat(path)
	storeMust(t, err)
	return uint64(info.Sys().(*syscall.Stat_t).Nlink)
}

func storeExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// storeTestExecutor is a hand-written CommandExecutor standing in for the
// Rust tests' one-off executor types. Unset hooks keep the Rust trait's
// defaults, and like those types it runs without a deadline.
type storeTestExecutor struct {
	core.CommandExecutorDefaults
	execute                         func(command *core.CommandSpec) (core.CommandOutput, error)
	executeWithoutDeadline          func(command *core.CommandSpec) (core.CommandOutput, error)
	executeWithStdinWithoutDeadline func(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error)
	cancelled                       func() bool
	cancelledWithoutDeadline        func() bool
	stageStarted                    func(stage core.ProvisionStage)
	stageFinished                   func(stage core.ProvisionStage)
	pauseDeadline                   func() *core.DeadlinePause
}

func (e *storeTestExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.execute(command)
}

func (e *storeTestExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *storeTestExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e *storeTestExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	if e.executeWithoutDeadline != nil {
		return e.executeWithoutDeadline(command)
	}
	return e.Execute(command)
}

func (e *storeTestExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	if e.executeWithStdinWithoutDeadline != nil {
		return e.executeWithStdinWithoutDeadline(command, input)
	}
	return e.ExecuteWithStdin(command, input)
}

func (e *storeTestExecutor) CancellationRequested() bool {
	return e.cancelled != nil && e.cancelled()
}

func (e *storeTestExecutor) CancellationRequestedWithoutDeadline() bool {
	if e.cancelledWithoutDeadline != nil {
		return e.cancelledWithoutDeadline()
	}
	return e.CancellationRequested()
}

func (e *storeTestExecutor) PauseDeadline() *core.DeadlinePause {
	if e.pauseDeadline != nil {
		return e.pauseDeadline()
	}
	return core.DeadlinePauseNone()
}

func (e *storeTestExecutor) StageStarted(stage core.ProvisionStage) {
	if e.stageStarted != nil {
		e.stageStarted(stage)
	}
}

func (e *storeTestExecutor) StageFinished(stage core.ProvisionStage) {
	if e.stageFinished != nil {
		e.stageFinished(stage)
	}
}

// storeProcess runs a command as a local process.
func storeProcess(command *core.CommandSpec) (core.CommandOutput, error) {
	return core.ProcessExecutor{}.Execute(command)
}

// storeTransfers is Rust's Transfers: local processes, counting worker
// transfers.
func storeTransfers(transfers *atomic.Int64) *storeTestExecutor {
	return &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if command.Purpose == "stage replacement Mjolnir worker" {
			transfers.Add(1)
		}
		return storeProcess(command)
	}}
}

// storeWithPath runs command with bin first on its PATH.
func storeWithPath(command *core.CommandSpec, bin string) (core.CommandOutput, error) {
	local := *command
	local.Env = maps.Clone(command.Env)
	if local.Env == nil {
		local.Env = map[string]string{}
	}
	local.Env["PATH"] = bin + ":" + os.Getenv("PATH")
	return storeProcess(&local)
}

func TestConcurrentSessionsShareOneVerifiedUploadAndSwapsLeaveOtherLinksUnchanged(t *testing.T) {
	temp := t.TempDir()
	first := "11111111111111111111111111111111"
	second := "22222222222222222222222222222222"
	a := storeTestTarget(t, temp, first)
	b := storeTestTarget(t, temp, second)
	old := storeTestSource(t, temp, "old")
	replacement := storeTestSource(t, temp, "new")
	var transfers atomic.Int64
	executor := storeTransfers(&transfers)
	type session struct {
		locator core.TargetsTargetLocator
		id      string
	}
	var others []session
	for n := range 2 {
		id := fmt.Sprintf("%032d", n)
		others = append(others, session{storeTestTarget(t, temp, id), id})
	}
	var group sync.WaitGroup
	for _, install := range append([]session{{a, first}, {b, second}}, others...) {
		group.Go(func() {
			if err := storeTestInstall(executor, install.locator, install.id, old); err != nil {
				t.Error(err)
			}
		})
	}
	group.Wait()
	if t.Failed() {
		t.FailNow()
	}
	// A repeated install and a normal promotion can both name the inode
	// that is already installed. Plain mv reports these two hard links as an
	// error.
	storeMust(t, storeTestInstall(executor, a, first, old))
	staged, err := StageWorkerBinaryForUpgrade(executor, &a, first, WorkerInstallHost{Path: old})
	storeMust(t, err)
	owner, err := store.WorkerTestPermit(context.Background(), first, "test current worker promotion")
	storeMust(t, err)
	if owner == nil {
		t.Fatal("the test worker gate refused the promotion")
	}
	storeMust(t, staged.Install(owner))
	staged.Close()
	owner.Release()
	aPath := filepath.Join(temp, first, "hel")
	bPath := filepath.Join(temp, second, "hel")
	digest, err := core.WorkerExecutableDigest(old)
	storeMust(t, err)
	root, err := StoreRoot(&a, first)
	storeMust(t, err)
	cached := filepath.Join(root, digest, "hel")
	if got := transfers.Load(); got != 1 {
		t.Fatalf("transfers = %d, want 1", got)
	}
	if storeInode(t, aPath) != storeInode(t, bPath) {
		t.Fatal("the sessions do not share the installed inode")
	}
	if storeInode(t, aPath) != storeInode(t, cached) {
		t.Fatal("the session does not share the store's inode")
	}
	// Initial install, not just upgrade promotion, must replace the
	// directory entry when the old executable shares its inode with another
	// session.
	profile := filepath.Join(temp, "empty-profile")
	storeMust(t, os.Mkdir(profile, 0o777))
	config := filepath.Join(temp, "config")
	storeMust(t, os.WriteFile(config, []byte("{}"), 0o666))
	storeMust(t, InstallWorkerFiles(executor, &a, first, filepath.Dir(aPath), filepath.Join(temp, "profile"),
		WorkerInstallHost{Path: replacement}, config, config, profile, nil, nil))
	if !bytes.Equal(storeRead(t, aPath), storeRead(t, replacement)) {
		t.Fatal("the session does not run the new worker")
	}
	if !bytes.Equal(storeRead(t, bPath), storeRead(t, old)) {
		t.Fatal("the other session's worker changed")
	}
	if !bytes.Equal(storeRead(t, cached), storeRead(t, old)) {
		t.Fatal("the store entry changed")
	}
	storeMust(t, os.RemoveAll(filepath.Dir(bPath)))
	if !storeExists(cached) {
		t.Fatal("session teardown removed the shared entry")
	}
}

// A gate wait inside WithoutDeadline must not end at the wrapped executor's
// deadline: queueing behind another upload does not count against a
// timeout. A real cancellation still ends it.
// Hard-won: #1550: staging time consumed the caller's deadline, so the following stop never ran
func TestAGateWaitOutlastsTheCallersDeadlineButNotACancel(t *testing.T) {
	temp := t.TempDir()
	id := "66666666666666666666666666666666"
	locator := storeTestTarget(t, temp, id)
	root, err := StoreRoot(&locator, id)
	storeMust(t, err)
	gate := StoreGateFor(&locator, root)
	held := gate.TryLock("holder", "test hold")
	if held == nil {
		t.Fatal("a free gate")
	}
	caller := core.CancellableProcessExecutorWithTimeout(time.Second)
	var waiterDone atomic.Bool
	ready := make(chan struct{})
	result := make(chan error, 1)
	go func() {
		close(ready)
		guard, err := LockGate(caller, gate, id, "wait behind the holder")
		guard.Release()
		waiterDone.Store(true)
		result <- err
	}()
	<-ready
	time.Sleep(1500 * time.Millisecond)
	if waiterDone.Load() {
		t.Fatal("the wait ended at the caller's deadline")
	}
	held.Release()
	if err := <-result; err != nil {
		t.Fatalf("the wait proceeds once the gate frees: %v", err)
	}

	// The control command gets the budget that staging did not use. A later
	// long command still times out: dropping the scope really resumes it.
	control := core.NewCommandSpec("sh", []string{"-c", "sleep 0.2; printf stopped"}).
		WithPurpose("stop worker after staging")
	output, err := caller.Execute(&control)
	if err != nil {
		t.Fatalf("stop still has its budget: %v", err)
	}
	if output.Status != 0 || string(output.Stdout) != "stopped" {
		t.Fatalf("stop output = %d %q", output.Status, output.Stdout)
	}
	long := core.NewCommandSpec("sh", []string{"-c", "sleep 30"})
	if _, err := caller.Execute(&long); err == nil || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("the long command did not time out: %v", err)
	}

	// A real cancellation ends the wait.
	var cancelled atomic.Bool
	canceller := core.NewCancellableProcessExecutor(&cancelled)
	held = gate.TryLock("holder", "test hold")
	if held == nil {
		t.Fatal("a free gate")
	}
	go func() {
		untimed := core.NewWithoutDeadline(canceller)
		defer untimed.End()
		guard, err := LockGate(untimed, gate, id, "wait behind the holder")
		guard.Release()
		result <- err
	}()
	time.Sleep(100 * time.Millisecond)
	cancelled.Store(true)
	if err := <-result; err == nil {
		t.Fatal("a cancelled wait succeeded")
	}
	held.Release()
	if _, err := canceller.Execute(&control); err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("a cancelled executor ran the command: %v", err)
	}
	untimed := core.NewWithoutDeadline(canceller)
	_, err = untimed.Execute(&control)
	untimed.End()
	if err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("a cancelled untimed executor ran the command: %v", err)
	}
	// Cancellation also interrupts a running staging command while paused.
	cancelled.Store(false)
	go func() {
		time.Sleep(100 * time.Millisecond)
		cancelled.Store(true)
	}()
	untimed = core.NewWithoutDeadline(canceller)
	_, err = untimed.Execute(&long)
	untimed.End()
	if err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("cancellation did not interrupt the paused command: %v", err)
	}
}

// Hard-won: #1577: the upgrade deadline killed a slow worker upload, and the next session restarted it from zero
func TestASlowWorkerByteStreamOutlivesTheOperationDeadlineButShortCommandsKeepIt(t *testing.T) {
	// Record which commands consume the operation deadline.
	var mu sync.Mutex
	var recorded []string
	record := func(entry string) {
		mu.Lock()
		defer mu.Unlock()
		recorded = append(recorded, entry)
	}
	executor := &storeTestExecutor{
		cancelledWithoutDeadline: func() bool { return false },
		executeWithoutDeadline: func(command *core.CommandSpec) (core.CommandOutput, error) {
			panic("only the byte stream may ignore the deadline: " + command.Purpose)
		},
		executeWithStdinWithoutDeadline: func(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
			body, err := io.ReadAll(input)
			if err != nil {
				return core.CommandOutput{}, err
			}
			if !bytes.Equal(body, bytes.Repeat([]byte{'x'}, 128*1024)) {
				t.Errorf("streamed %d bytes, not the worker", len(body))
			}
			record("untimed " + command.Purpose)
			return core.CommandOutput{}, nil
		},
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			record("timed " + command.Purpose)
			return core.CommandOutput{}, nil
		},
	}
	temp := t.TempDir()
	file := filepath.Join(temp, "worker")
	storeMust(t, os.WriteFile(file, bytes.Repeat([]byte{'x'}, 128*1024), 0o666))
	locator := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "user@example.test"},
		Workspace: "workspace",
	}
	storeMust(t, transferBinary(executor, &locator, "session", file, "store/.upload/hel"))
	want := []string{
		"untimed stage replacement Mjolnir worker",
		"timed make replaced Mjolnir worker executable",
	}
	if !slices.Equal(recorded, want) {
		t.Fatalf("recorded = %q, want %q", recorded, want)
	}
}

func TestStoreCollectionReclaimsOnlyOldUnlinkedEntriesAndCorruptionIsReplacedPrivately(t *testing.T) {
	temp := t.TempDir()
	id := "33333333333333333333333333333333"
	locator := storeTestTarget(t, temp, id)
	worker := storeTestSource(t, temp, "worker")
	process := &storeTestExecutor{execute: storeProcess}
	storeMust(t, storeTestInstall(process, locator, id, worker))
	root, err := StoreRoot(&locator, id)
	storeMust(t, err)
	digest, err := core.WorkerExecutableDigest(worker)
	storeMust(t, err)
	linked := filepath.Join(root, digest, "hel")
	unlinked := filepath.Join(root, strings.Repeat("a", 64), "hel")
	young := filepath.Join(root, strings.Repeat("b", 64), "hel")
	abandoned := filepath.Join(root, ".upload-abandoned")
	storeMust(t, os.Mkdir(abandoned, 0o777))
	storeMust(t, os.WriteFile(filepath.Join(abandoned, "hel.next"), []byte("partial"), 0o666))
	for _, path := range []string{unlinked, young} {
		storeMust(t, os.MkdirAll(filepath.Dir(path), 0o777))
		storeMust(t, os.WriteFile(path, []byte("unused"), 0o666))
	}
	_, err = targets.ExecuteChecked(process, core.NewCommandSpec("touch",
		[]string{"-t", "200001010000", linked, unlinked, abandoned}))
	storeMust(t, err)
	storeMust(t, collect(process, &locator, root, nil))
	if !storeExists(linked) {
		t.Fatal("collection removed a linked entry")
	}
	if storeExists(unlinked) {
		t.Fatal("collection kept an old unlinked entry")
	}
	if storeExists(abandoned) {
		t.Fatal("collection kept an abandoned upload")
	}
	if !storeExists(young) {
		t.Fatal("collection removed a young entry")
	}
	// An old transport-cache entry may have no session links just before an
	// installer needs it. Collection must preserve that in-progress build.
	storeMust(t, os.Remove(filepath.Join(temp, id, "hel")))
	var cachedTransfers atomic.Int64
	storeMust(t, storeTestInstall(storeTransfers(&cachedTransfers), locator, id, worker))
	if got := cachedTransfers.Load(); got != 0 {
		t.Fatalf("a cached install transferred %d times", got)
	}
	// Simulate external corruption via replacement; a repair must not write
	// through any existing inode or trust an unchecked content-addressed
	// name.
	storeMust(t, os.Remove(linked))
	storeMust(t, os.WriteFile(linked, []byte("corrupt"), 0o666))
	var transfers atomic.Int64
	storeMust(t, storeTestInstall(storeTransfers(&transfers), locator, id, worker))
	if !bytes.Equal(storeRead(t, linked), storeRead(t, worker)) {
		t.Fatal("the corrupt entry was not replaced")
	}
	if got := transfers.Load(); got != 1 {
		t.Fatalf("transfers = %d, want 1", got)
	}
}

// Rust: `#[cfg(target_os = "linux")]`.
func TestCrossFilesystemLinkUsesPrivateCopyWithoutChangingExistingLinks(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("Linux only, as in Rust: /dev/shm is a separate filesystem there")
	}
	temp := t.TempDir()
	other, err := os.MkdirTemp("/dev/shm", ".tmp")
	storeMust(t, err)
	t.Cleanup(func() { os.RemoveAll(other) })
	tempInfo, err := os.Stat(temp)
	storeMust(t, err)
	otherInfo, err := os.Stat(other)
	storeMust(t, err)
	if tempInfo.Sys().(*syscall.Stat_t).Dev == otherInfo.Sys().(*syscall.Stat_t).Dev {
		t.Fatal("/dev/shm is on the test directory's filesystem")
	}
	id := "44444444444444444444444444444444"
	// The session root may be a mount while its sibling store is on the host
	// filesystem. A symlink reproduces that boundary without mount
	// privileges.
	storeMust(t, os.Symlink(other, filepath.Join(temp, id)))
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: filepath.Join(temp, id)}
	worker := storeTestSource(t, temp, "worker")
	storeMust(t, storeTestInstall(&storeTestExecutor{execute: storeProcess}, locator, id, worker))
	if !bytes.Equal(storeRead(t, filepath.Join(other, "hel")), storeRead(t, worker)) {
		t.Fatal("the session did not get the worker")
	}
}

func TestAFilesystemWithoutHardLinksStillInstallsVerifiedPrivateCopies(t *testing.T) {
	temp := t.TempDir()
	bin := filepath.Join(temp, "bin")
	storeMust(t, os.Mkdir(bin, 0o777))
	core.InstallFakeCommand(bin, "ln", "#!/bin/sh\nexit 1\n")
	id := "66666666666666666666666666666666"
	locator := storeTestTarget(t, temp, id)
	worker := storeTestSource(t, temp, "worker")
	noLinks := &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		return storeWithPath(command, bin)
	}}
	storeMust(t, storeTestInstall(noLinks, locator, id, worker))
	if !bytes.Equal(storeRead(t, filepath.Join(temp, id, "hel")), storeRead(t, worker)) {
		t.Fatal("the session did not get the worker")
	}
}

// Hard-won: 7f871fd03: the wait deadline expired during setup before the installer reached the held store gate.
func TestAWaitingInstallerObservesCancellationWithoutCancellingTheStoreOwner(t *testing.T) {
	reached := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	owner := &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if command.Purpose == "stage replacement Mjolnir worker" {
			reached <- struct{}{}
			// Keep the original deadlock watchdog, but fail on expiry:
			// never release the gate as though the waiter had finished.
			select {
			case <-release:
			case <-time.After(10 * time.Second):
				panic("cancellation test waiter did not finish before the deadlock watchdog")
			}
		}
		return storeProcess(command)
	}}
	// Rust's CancelAtWait: cancelled only once queued, without racing setup
	// or a clock.
	var canceller atomic.Pointer[core.CancellableProcessExecutor]
	cancelAtWait := &storeTestExecutor{
		pauseDeadline: func() *core.DeadlinePause {
			if executor := canceller.Load(); executor != nil {
				return executor.PauseDeadline()
			}
			return core.DeadlinePauseNone()
		},
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			if executor := canceller.Load(); executor != nil {
				return executor.Execute(command)
			}
			return storeProcess(command)
		},
		cancelled: func() bool {
			executor := canceller.Load()
			return executor != nil && executor.IsCancelled()
		},
		stageStarted: func(stage core.ProvisionStage) {
			if stage.Kind == core.ProvisionStageWaitingForWorkerStore {
				cancelled := new(atomic.Bool)
				cancelled.Store(true)
				executor := core.NewCancellableProcessExecutor(cancelled)
				canceller.CompareAndSwap(nil, &executor)
			}
		},
	}
	temp := t.TempDir()
	first := "77777777777777777777777777777777"
	second := "88888888888888888888888888888888"
	a := storeTestTarget(t, temp, first)
	b := storeTestTarget(t, temp, second)
	worker := storeTestSource(t, temp, "worker")
	running := make(chan error, 1)
	go func() { running <- storeTestInstall(owner, a, first, worker) }()
	select {
	case <-reached:
	case err := <-running:
		t.Fatalf("the owner finished before its upload: %v", err)
	}
	result := InstallWorkerBinary(cancelAtWait, &b, second, WorkerInstallHost{Path: worker}, "hel")
	release <- struct{}{}
	storeMust(t, <-running)
	if result == nil || !strings.Contains(result.Error(), "cancelled while wait to publish a worker build") {
		t.Fatalf("the waiting installer was not cancelled at its wait: %v", result)
	}
	if !storeExists(filepath.Join(temp, first, "hel")) {
		t.Fatal("the store owner did not install")
	}
	if storeExists(filepath.Join(temp, second, "hel")) {
		t.Fatal("the cancelled installer installed")
	}
}

func TestPublishedBuildsInstallWithoutWaitingOnEachOtherOrOnAnUploadOrCollection(t *testing.T) {
	const sessions = 6
	temp := t.TempDir()
	published := storeTestSource(t, temp, "published")
	uploading := storeTestSource(t, temp, "uploading")
	seed := "cccccccccccccccccccccccccccccccc"
	storeMust(t, storeTestInstall(&storeTestExecutor{execute: storeProcess}, storeTestTarget(t, temp, seed), seed, published))
	type session struct {
		locator core.TargetsTargetLocator
		id      string
	}
	var installs []session
	for n := range sessions {
		id := fmt.Sprintf("d%031d", n)
		installs = append(installs, session{storeTestTarget(t, temp, id), id})
	}
	uploader := "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
	uploaderTarget := storeTestTarget(t, temp, uploader)
	publishedDigest, err := core.WorkerExecutableDigest(published)
	storeMust(t, err)
	reached := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	var arrived, overlapped, collects atomic.Int64
	executor := &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		switch {
		// Another build's upload stays in flight for the whole test.
		case command.Purpose == "stage replacement Mjolnir worker":
			reached <- struct{}{}
			select {
			case <-release:
			case <-time.After(20 * time.Second):
			}
		// Every link of the published build must be in flight at once;
		// serialized installs would leave each one alone here until its
		// deadline.
		case command.Purpose == "link shared Mjolnir worker into session" &&
			slices.ContainsFunc(command.Args, func(arg string) bool { return strings.Contains(arg, publishedDigest) }):
			arrived.Add(1)
			deadline := time.Now().Add(10 * time.Second)
			for arrived.Load() < sessions && time.Now().Before(deadline) {
				time.Sleep(5 * time.Millisecond)
			}
			if arrived.Load() >= sessions {
				overlapped.Add(1)
			}
		case command.Purpose == "collect unused Mjolnir worker binaries":
			collects.Add(1)
		}
		return storeProcess(command)
	}}
	upload := make(chan error, 1)
	go func() { upload <- storeTestInstall(executor, uploaderTarget, uploader, uploading) }()
	select {
	case <-reached:
	case <-time.After(5 * time.Second):
		t.Fatal("the upload did not start")
	}
	var group sync.WaitGroup
	for _, install := range installs {
		group.Go(func() {
			if err := storeTestInstall(executor, install.locator, install.id, published); err != nil {
				t.Error(err)
			}
		})
	}
	group.Wait()
	// The upload is still held: nobody queued behind it.
	release <- struct{}{}
	storeMust(t, <-upload)
	if got := overlapped.Load(); got != sessions {
		t.Fatalf("overlapped = %d, want %d", got, sessions)
	}
	// Only the upload collects; cached installs skip while it owns the
	// store.
	if got := collects.Load(); got != 1 {
		t.Fatalf("collects = %d, want 1", got)
	}
}

// Hard-won: shared-store review: a foreign collector can unlink a fallback source before cp opens it.
func TestFallbackReuploadsASourceRetiredBetweenLinkAndCopyButPreservesOtherErrors(t *testing.T) {
	temp := t.TempDir()
	id := "99999999999999999999999999999999"
	locator := storeTestTarget(t, temp, id)
	worker := storeTestSource(t, temp, "worker")
	storeMust(t, storeTestInstall(&storeTestExecutor{execute: storeProcess}, locator, id, worker))
	storeMust(t, os.Remove(filepath.Join(temp, id, "hel")))
	bin := filepath.Join(temp, "bin")
	storeMust(t, os.Mkdir(bin, 0o777))
	core.InstallFakeCommand(bin, "ln", "#!/bin/sh\nexit 1\n")
	// Emulate the other collector after the shell has checked source
	// existence, immediately before cp opens it. Subsequent retries use real
	// cp.
	core.InstallFakeCommand(bin, "cp", fmt.Sprintf(`#!/bin/sh
case "$2" in */worker-binaries/*/hel)
    if [ ! -f '%[1]s' ]; then
        touch '%[1]s'
        rm -f -- "$2"
    fi;;
esac
exec /bin/cp "$@"
`, filepath.Join(temp, "retired")))
	var transfers atomic.Int64
	retiringCopy := &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if command.Purpose == "stage replacement Mjolnir worker" {
			transfers.Add(1)
		}
		return storeWithPath(command, bin)
	}}
	storeMust(t, storeTestInstall(retiringCopy, locator, id, worker))
	if got := transfers.Load(); got != 1 {
		t.Fatalf("transfers = %d, want 1", got)
	}
	if !bytes.Equal(storeRead(t, filepath.Join(temp, id, "hel")), storeRead(t, worker)) {
		t.Fatal("the session did not get the worker")
	}
	// A failing copy with its source still present must fail, not retry
	// uploads.
	core.InstallFakeCommand(bin, "cp", "#!/bin/sh\nexit 1\n")
	err := storeTestInstall(retiringCopy, locator, id, worker)
	if err == nil || !strings.Contains(err.Error(), "install shared Mjolnir worker") {
		t.Fatalf("a failing copy did not fail the install: %v", err)
	}
	if got := transfers.Load(); got != 1 {
		t.Fatalf("transfers = %d, want 1", got)
	}
}

// Hard-won: shared-store review: SIGKILL skips shell EXIT traps and used to leak a permanent store reference.
func TestKilledStorePinsAreSweptOnTheNextInstallWhileLivePinsSurvive(t *testing.T) {
	killLink := &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		local := *command
		if local.Purpose == "link shared Mjolnir worker into session" {
			local.Args = slices.Clone(command.Args)
			local.Args[1] = strings.Replace(local.Args[1], "# The private link pins",
				"kill -KILL $$\n# The private link pins", 1)
		}
		return storeProcess(&local)
	}}
	temp := t.TempDir()
	id := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	locator := storeTestTarget(t, temp, id)
	worker := storeTestSource(t, temp, "worker")
	root := filepath.Join(temp, id)
	err := storeTestInstall(killLink, locator, id, worker)
	if err == nil || !strings.Contains(err.Error(), "install shared Mjolnir worker") {
		t.Fatalf("a killed link did not fail the install: %v", err)
	}
	entries, err := os.ReadDir(root)
	storeMust(t, err)
	var stale []string
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "hel.store-") {
			stale = append(stale, filepath.Join(root, entry.Name()))
		}
	}
	if len(stale) != 1 {
		t.Fatalf("stale pins = %q, want one", stale)
	}
	if links := storeLinks(t, stale[0]); links != 2 {
		t.Fatalf("the stale pin has %d links, want 2", links)
	}
	liveName := "hel.store-live"
	process := &storeTestExecutor{execute: storeProcess}
	live, err := LiveStagingRegisterAndSweep(process, &locator, id, root, liveName)
	storeMust(t, err)
	if storeExists(stale[0]) {
		t.Fatal("registration kept a stale pin")
	}
	digest, err := core.WorkerExecutableDigest(worker)
	storeMust(t, err)
	storeRoot, err := StoreRoot(&locator, id)
	storeMust(t, err)
	cached := filepath.Join(storeRoot, digest, "hel")
	livePath := filepath.Join(root, liveName)
	storeMust(t, os.Link(cached, livePath))
	storeMust(t, storeTestInstall(process, locator, id, worker))
	if !storeExists(livePath) {
		t.Fatal("the next install swept a registered pin")
	}
	live.Release()
	storeMust(t, storeTestInstall(process, locator, id, worker))
	if storeExists(livePath) {
		t.Fatal("an unregistered pin survived the next install")
	}
	if links := storeLinks(t, cached); links != 2 {
		t.Fatalf("the store entry has %d links, want 2", links)
	}
}

// Hard-won: shared-store review: copying capacity was required even when a cached hard link needed no bytes.
func TestCachedInstallsCollectAndLinkOnFullTargetsButUploadsAndFallbackCopiesAreRefused(t *testing.T) {
	if _, child := os.LookupEnv("MJ_WORKER_STORE_CAPACITY_CHILD"); !child {
		temp := t.TempDir()
		targets.NewIsolatedTest(targets.TestName("workerstaging::worker_binary::store::tests",
			"TestCachedInstallsCollectAndLinkOnFullTargetsButUploadsAndFallbackCopiesAreRefused")).
			IsolatedStore(temp).Env("MJ_WORKER_STORE_CAPACITY_CHILD", "1").Run()
		return
	}
	temp := t.TempDir()
	id := "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	locator := storeTestTarget(t, temp, id)
	worker := storeTestSource(t, temp, "worker")
	other := storeTestSource(t, temp, "new")
	process := &storeTestExecutor{execute: storeProcess}
	storeMust(t, storeTestInstall(process, locator, id, worker))
	root, err := StoreRoot(&locator, id)
	storeMust(t, err)
	garbage := filepath.Join(root, strings.Repeat("c", 64), "hel")
	storeMust(t, os.MkdirAll(filepath.Dir(garbage), 0o777))
	storeMust(t, os.WriteFile(garbage, []byte("unused"), 0o666))
	_, err = targets.ExecuteChecked(process, core.NewCommandSpec("touch", []string{"-t", "200001010000", garbage}))
	storeMust(t, err)
	sample := targets.TestStorageSample("local", 0, 0)
	sample.Filesystems = sample.Filesystems[:1]
	sample.Filesystems[0].Paths = []string{temp}
	targets.RecordSamples([]core.HostStorageSample{sample}, 1)
	// The first install above claimed this interval's collection.
	ForgetCollections()
	var transfers atomic.Int64
	executor := storeTransfers(&transfers)
	storeMust(t, storeTestInstall(executor, locator, id, worker))
	profile := filepath.Join(temp, "empty-profile")
	storeMust(t, os.Mkdir(profile, 0o777))
	config := filepath.Join(temp, "config")
	storeMust(t, os.WriteFile(config, []byte("{}"), 0o666))
	storeMust(t, InstallWorkerFiles(executor, &locator, id, filepath.Join(temp, id), filepath.Join(temp, "profile"),
		WorkerInstallHost{Path: worker}, config, config, profile, nil, nil))
	unchecked := core.NewCommandSpec("true", nil)
	_, err = ReplaceTargetWorkerBinaryIfStale(executor, &locator, id, &unchecked, worker)
	storeMust(t, err)
	prepared, err := StageWorkerBinaryForUpgrade(executor, &locator, id, WorkerInstallHost{Path: worker})
	storeMust(t, err)
	prepared.Close()
	if storeExists(garbage) {
		t.Fatal("capacity refusal skipped collection")
	}
	if got := transfers.Load(); got != 0 {
		t.Fatalf("transfers = %d, want 0", got)
	}
	err = storeTestInstall(executor, locator, id, other)
	if !targets.IsStorageFull(err) {
		t.Fatalf("an upload to a full target was not refused for space: %v", err)
	}
	noLinks := &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if command.Purpose == "link shared Mjolnir worker into session" {
			return core.CommandOutput{Status: 76}, nil
		}
		return storeProcess(command)
	}}
	err = storeTestInstall(noLinks, locator, id, worker)
	if !targets.IsStorageFull(err) {
		t.Fatalf("a fallback copy on a full target was not refused for space: %v", err)
	}
}

// A wait behind a held gate is its own stage, open from the first failed
// attempt until the gate is taken, so a lifecycle watching for stalls can
// tell a queue from a stuck command. A free gate reports nothing.
func TestAWaitBehindAHeldStoreGateIsReportedAsAQueueStage(t *testing.T) {
	type report struct {
		started bool
		stage   core.ProvisionStage
	}
	var mu sync.Mutex
	var reports []report
	recorded := func() []report {
		mu.Lock()
		defer mu.Unlock()
		return slices.Clone(reports)
	}
	stages := &storeTestExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			panic(fmt.Sprintf("waiting for a gate runs no command: %+v", *command))
		},
		stageStarted: func(stage core.ProvisionStage) {
			mu.Lock()
			defer mu.Unlock()
			reports = append(reports, report{true, stage})
		},
		stageFinished: func(stage core.ProvisionStage) {
			mu.Lock()
			defer mu.Unlock()
			reports = append(reports, report{false, stage})
		},
	}
	temp := t.TempDir()
	locator := storeTestTarget(t, temp, "33333333333333333333333333333333")
	gate := StoreGateFor(&locator, "queue-stage-test")
	guard, err := LockGate(stages, gate, "free", "take a free gate")
	storeMust(t, err)
	guard.Release()
	if len(recorded()) != 0 {
		t.Fatalf("a free gate reported %+v", recorded())
	}

	held := gate.TryLock("holder", "hold the gate")
	if held == nil {
		t.Fatal("a free gate")
	}
	waiter := make(chan error, 1)
	go func() {
		guard, err := LockGate(stages, gate, "waiter", "wait for the gate")
		guard.Release()
		waiter <- err
	}()
	for len(recorded()) == 0 {
		time.Sleep(5 * time.Millisecond)
	}
	queued := core.ProvisionStage{Kind: core.ProvisionStageWaitingForWorkerStore}
	if got := recorded(); !slices.Equal(got, []report{{true, queued}}) {
		t.Fatalf("the stage is open while the gate is held: %+v", got)
	}
	held.Release()
	storeMust(t, <-waiter)
	if got := recorded(); !slices.Equal(got, []report{{true, queued}, {false, queued}}) {
		t.Fatalf("stages = %+v", got)
	}
}

// Hard-won: #1626: concurrent publication after a failed ln misclassified missing source as no hard-link support
func TestConcurrentPublicationAfterInitialLinkMissHardLinksInsteadOfCopying(t *testing.T) {
	withPathPrefix := func(bin string, copyCalls *atomic.Int64) *storeTestExecutor {
		return &storeTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			if command.Purpose == "link shared Mjolnir worker into session" {
				if len(command.Args) > 0 && command.Args[len(command.Args)-1] == "copy" {
					copyCalls.Add(1)
				}
				return storeWithPath(command, bin)
			}
			return storeProcess(command)
		}}
	}

	temp := t.TempDir()
	idRace := "44444444444444444444444444444444"
	locatorRace := storeTestTarget(t, temp, idRace)
	binary := storeTestSource(t, temp, "worker-race")
	digest, err := core.WorkerExecutableDigest(binary)
	storeMust(t, err)
	root, err := StoreRoot(&locatorRace, idRace)
	storeMust(t, err)
	cached := filepath.Join(root, digest, "hel")

	// Wrapper `ln`: on the first call, let real `ln` fail on the missing
	// source, then publish the source before returning the failure so the
	// script's post-failure existence check sees it present. Subsequent
	// calls use real `ln`.
	raceBin := filepath.Join(temp, "race-bin")
	storeMust(t, os.Mkdir(raceBin, 0o777))
	marker := filepath.Join(temp, "ln-first-call")
	realLn := "/bin/ln"
	if !storeExists(realLn) {
		realLn = "/usr/bin/ln"
	}
	raceLn := filepath.Join(raceBin, "ln")
	storeMust(t, os.WriteFile(raceLn, []byte(fmt.Sprintf("#!/bin/sh\n"+
		"if [ ! -e '%[1]s' ]; then\n"+
		": > '%[1]s'\n"+
		"'%[2]s' \"$@\" 2>/dev/null\n"+
		"status=$?\n"+
		"mkdir -p -- '%[3]s'\n"+
		"cp -- '%[4]s' '%[5]s'\n"+
		"exit $status\n"+
		"fi\n"+
		"exec '%[2]s' \"$@\"\n",
		marker, realLn, filepath.Dir(cached), binary, cached)), 0o666))
	storeMust(t, os.Chmod(raceLn, 0o755))

	var raceCopies atomic.Int64
	storeMust(t, storeTestInstall(withPathPrefix(raceBin, &raceCopies), locatorRace, idRace, binary))
	destRace := filepath.Join(temp, idRace, "hel")
	if got := raceCopies.Load(); got != 0 {
		t.Fatalf("concurrent publication should hard-link on retry instead of copying: %d copies", got)
	}
	if storeInode(t, destRace) != storeInode(t, cached) {
		t.Fatal("installed worker must share the published store inode")
	}

	// Genuine no-hard-link fallback: `ln` always fails even though the
	// source is present.
	idNolink := "55555555555555555555555555555555"
	locatorNolink := storeTestTarget(t, temp, idNolink)
	nolinkBin := filepath.Join(temp, "nolink-bin")
	storeMust(t, os.Mkdir(nolinkBin, 0o777))
	nolinkLn := filepath.Join(nolinkBin, "ln")
	storeMust(t, os.WriteFile(nolinkLn, []byte("#!/bin/sh\nexit 1\n"), 0o666))
	storeMust(t, os.Chmod(nolinkLn, 0o755))

	var nolinkCopies atomic.Int64
	storeMust(t, storeTestInstall(withPathPrefix(nolinkBin, &nolinkCopies), locatorNolink, idNolink, binary))
	destNolink := filepath.Join(temp, idNolink, "hel")
	if got := nolinkCopies.Load(); got != 1 {
		t.Fatalf("genuine hard-link failure must still fall back to copy: %d copies", got)
	}
	if storeInode(t, destNolink) == storeInode(t, cached) {
		t.Fatal("copied fallback must have its own inode")
	}
	if !bytes.Equal(storeRead(t, destNolink), storeRead(t, binary)) {
		t.Fatal("the copied worker differs")
	}
}
