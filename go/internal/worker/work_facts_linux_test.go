// Port of the Linux-only tests in mj-worker/src/work_facts.rs.

package worker

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// workFactsLaunchOrphan starts `sleep <seconds>` with setsid from a shell
// that exits, carrying session's marker.
func workFactsLaunchOrphan(t *testing.T, session string, seconds int) {
	t.Helper()
	command := exec.Command("sh", "-c", fmt.Sprintf("setsid sleep %d >/dev/null 2>&1 &", seconds))
	command.Env = append(os.Environ(), "MJ_SESSION_ID="+session)
	output, err := core.RunWithInput(command, nil)
	if err != nil || !output.Status.Success() {
		t.Fatalf("launch: %v", err)
	}
}

func workFactsKill(pid uint32) {
	_ = syscall.Kill(int(pid), syscall.SIGKILL)
}

func workFactsRealObserver(session string) WorkObserver {
	return WorkObserver{
		SessionID:      session,
		WorkerPID:      uint32(os.Getpid()),
		Directory:      os.TempDir(),
		Git:            "git",
		HarnessReadyMs: new(atomic.Int64),
		Children:       &ObserverChildren{},
	}
}

// The case issue 1350 turned on: the agent's process has no parent in the
// harness tree. A real process started with setsid from a shell that has
// exited is found through its environment alone.
func TestARealSetsidOrphanIsFoundByItsSessionMarkerAndIdentifiedLikeTheParkScript(t *testing.T) {
	session := fmt.Sprintf("work-facts-test-%d", os.Getpid())
	workFactsLaunchOrphan(t, session, 61)
	observer := workFactsRealObserver(session)
	// The orphan carries the marker from the moment the shell forks it, but
	// it is sleep only after sh and then setsid have exec'd. A scan between
	// the fork and the last exec rightly reports the forked shell.
	var found *core.ProcessFacts
	last := "<none>"
	for range 100 {
		if facts := scanProcesses(&observer); facts.Value != nil && facts.Value.Running == 1 {
			if facts.Value.Shown[0].Command == "sleep 61" {
				found = facts.Value
				break
			}
			last = facts.Value.Shown[0].Command
		}
		time.Sleep(50 * time.Millisecond)
	}
	if found == nil {
		t.Fatalf("the orphan was not found as `sleep 61`; last seen as %s", last)
	}
	process := found.Shown[0]
	identity, err := core.ProcessBirthIdentity(process.Pid)
	// Stop it before asserting so a failure leaves nothing behind.
	workFactsKill(process.Pid)
	if err != nil {
		t.Fatal(err)
	}
	if !process.Detached {
		t.Fatal("the orphan is not marked detached")
	}
	if found.Identities[0].Start != identity {
		t.Fatalf("%s != %s", found.Identities[0].Start, identity)
	}
}

// The watcher recounts real processes: a killed one stops counting, and an
// identity whose pid is alive but whose start differs (a reused pid) never
// counts.
func TestTheWatcherCountsRealProcessesAndIgnoresAReusedPid(t *testing.T) {
	session := fmt.Sprintf("liveness-test-%d", os.Getpid())
	for range 2 {
		workFactsLaunchOrphan(t, session, 62)
	}
	observer := workFactsRealObserver(session)
	var facts *core.ProcessFacts
	for range 100 {
		if scanned := scanProcesses(&observer); scanned.Value != nil && scanned.Value.Running == 2 {
			facts = scanned.Value
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if facts == nil {
		t.Fatal("both orphans were not found")
	}
	var pids []uint32
	for _, identity := range facts.Identities {
		pids = append(pids, identity.Pid)
	}
	defer func() {
		for _, pid := range pids {
			workFactsKill(pid)
		}
	}()
	// The third identity names a live pid with a start it never had.
	facts.Identities = append(facts.Identities, core.ProcessIdentity{Pid: pids[0], Start: "not-the-start-of-that-process"})
	relay, err := DurableRelayOpen(t.TempDir(), session, "test")
	if err != nil {
		t.Fatal(err)
	}
	submitRelay(relay, "subagent-spawn-task", prompt("work"))
	if _, err := relay.ClaimPendingCommands(true); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordCommandCompleted("subagent-spawn-task", core.RelayCommandOutcomePrompt{
		WorkFacts: &core.WorkFacts{
			ObservedAtMs: 1,
			Processes:    core.ObservedKnown(*facts),
			Worktree:     core.ObservedUnavailable[core.WorktreeFacts]("not read"),
		},
		StopReason: "end_turn",
	}); err != nil {
		t.Fatal(err)
	}
	completed := relay.OperationalState().WorkLiveness
	if completed == nil || completed.Alive != 3 {
		t.Fatalf("%+v", completed)
	}
	cell := NewRelayCell(relay)
	ctx, cancel := context.WithCancel(context.Background())
	watcher := make(chan error, 1)
	go func() { watcher <- WatchLiveness(ctx, cell, observer, 50*time.Millisecond) }()
	defer func() {
		cancel()
		<-watcher
	}()
	alive := func() core.WorkLiveness {
		guard := cell.Observe()
		defer guard.Unlock()
		return *guard.OperationalState().WorkLiveness
	}
	waitFor := func(want uint32) {
		t.Helper()
		for range 200 {
			if alive().Alive == want {
				return
			}
			time.Sleep(50 * time.Millisecond)
		}
		t.Fatalf("alive never reached %d: %d", want, alive().Alive)
	}
	waitFor(2)
	workFactsKill(pids[0])
	waitFor(1)
	workFactsKill(pids[1])
	waitFor(0)
	if got := alive().CompletedOrdinal; got != completed.CompletedOrdinal {
		t.Fatalf("%d != %d", got, completed.CompletedOrdinal)
	}
}

func TestTheObserversOwnGitIsNotReportedAsTheAgentsWork(t *testing.T) {
	directory := t.TempDir()
	slowGit := filepath.Join(directory, "fake-git-for-the-observer-test")
	if err := os.WriteFile(slowGit, []byte("#!/bin/sh\nsleep 1\nexit 1\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	observer := WorkObserver{
		SessionID: fmt.Sprintf("observer-git-%d", os.Getpid()),
		// This test process stands in for the worker, so a git the collector
		// starts is one of its descendants, as in a real worker, and it
		// started after the harness was configured.
		WorkerPID:      uint32(os.Getpid()),
		Directory:      directory,
		Git:            slowGit,
		HarnessReadyMs: new(atomic.Int64),
		Children:       &ObserverChildren{},
	}
	observer.HarnessReadyMs.Store(core.EpochMillis() - 1)
	facts := WorkFactsCapture(context.Background(), &observer)
	if facts.Processes.Value == nil {
		t.Fatal(facts.Processes.Reason)
	}
	for _, process := range facts.Processes.Value.Shown {
		if strings.Contains(process.Command, "fake-git-for-the-observer-test") {
			t.Fatalf("%+v", *facts.Processes.Value)
		}
	}
	// The Git read ran, and failed, and says so; it is not an empty tree.
	if facts.Worktree.Known() != nil {
		t.Fatalf("%+v", *facts.Worktree.Value)
	}
}

// A same-user process that forbids reading its environment (a process that
// is not dumpable), found by the real reader.
func TestTheLinuxReaderTellsUnreadableFromAbsent(t *testing.T) {
	child := exec.Command("python3", "-I", "-c", "import ctypes, time; ctypes.CDLL(None).prctl(4, 0); time.sleep(30)")
	child.Env = append(os.Environ(), "MJ_SESSION_ID=unreadable-test")
	if err := child.Start(); err != nil {
		t.Fatalf("start a process that is not dumpable: %v", err)
	}
	wanted := []byte("MJ_SESSION_ID=unreadable-test")
	seen := WorkFactsMarkerAbsent
	for range 100 {
		seen = sessionMarker(uint32(child.Process.Pid), wanted)
		if seen == WorkFactsMarkerUnreadable {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	own := sessionMarker(uint32(os.Getpid()), []byte("MJ_NO_SUCH_VARIABLE=1"))
	if err := child.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	_ = child.Wait()
	if seen != WorkFactsMarkerUnreadable {
		t.Fatalf("seen %d", seen)
	}
	if own != WorkFactsMarkerAbsent {
		t.Fatalf("own %d", own)
	}
}
