// Port of the tests in mj-worker/src/work_facts.rs that run on every
// platform. The Unix and Linux ones are in work_facts_unix_test.go and
// work_facts_linux_test.go.

package worker

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const workFactsWorker uint32 = 100

func workFactsRow(pid, ppid uint32, ageMs int64, marked bool) ProcRow {
	marker := WorkFactsMarkerAbsent
	if marked {
		marker = WorkFactsMarkerPresent
	}
	return ProcRow{Pid: pid, Ppid: ppid, AgeMs: ageMs, Marker: marker}
}

func workFactsDescribe(row *ProcRow) Described {
	var command, exe string
	switch {
	// The harness's own helpers, present when it was configured.
	case row.Pid >= 101 && row.Pid <= 104:
		command, exe = "hel worker acp-supervisor", "/bin/hel"
	case row.Pid == 105:
		command, exe = "node bridge.js", "/usr/bin/node"
	case row.Pid == 106:
		command, exe = "claude --output-format stream-json", "/usr/bin/claude"
	// The agent's own node: the very program the bridge runs on.
	case row.Pid == 107:
		command, exe = "node server.js", "/usr/bin/node"
	default:
		command, exe = fmt.Sprintf("cargo test --job %d", row.Pid), "/usr/bin/cargo"
	}
	return Described{Command: command, Exe: &exe, Identity: fmt.Sprintf("boot:%d", row.Pid)}
}

// workFactsBusyTable is a busy host: 300 unrelated processes, the harness
// helpers started before the session was configured, and the agent's own
// work: a still-parented build, the agent's own node, and a setsid process
// that has been handed to PID 1.
func workFactsBusyTable() []ProcRow {
	var rows []ProcRow
	for pid := uint32(1000); pid < 1300; pid++ {
		rows = append(rows, workFactsRow(pid, 1, 500_000, false))
	}
	rows = append(rows, workFactsRow(workFactsWorker, 1, 900_000, false))
	for pid := uint32(101); pid <= 104; pid++ {
		rows = append(rows, workFactsRow(pid, workFactsWorker, 800_000, true))
	}
	rows = append(rows, workFactsRow(105, 101, 800_000, true))
	rows = append(rows, workFactsRow(106, 105, 800_000, true))
	rows = append(rows, workFactsRow(107, 105, 30_000, true))
	for pid := uint32(200); pid < 210; pid++ {
		rows = append(rows, workFactsRow(pid, 105, 10_000+int64(pid), true))
	}
	rows = append(rows, workFactsRow(300, 1, 20_000, true))
	return rows
}

func workFactsContext(own string) ScanContext {
	age := int64(700_000)
	return ScanContext{
		WorkerPID:        workFactsWorker,
		HarnessAgeMs:     &age,
		ResolutionMs:     0,
		OwnExe:           &own,
		ObserverChildren: map[uint32]struct{}{},
	}
}

func TestProcessesExcludeHelpersAndKeepExactCountsBeyondWhatIsShown(t *testing.T) {
	scan := workFactsContext("/bin/hel")
	facts := selectProcesses(workFactsBusyTable(), &scan, workFactsDescribe)
	// The agent's node is reported beside ten builds and the reparented
	// process; the bridge, which runs the same node, is not, nor is any
	// other helper or unrelated process among the 300.
	if facts.Running != 12 || len(facts.Identities) != 12 {
		t.Fatalf("%+v", facts)
	}
	if len(facts.Shown) != core.ShownProcesses || facts.Omitted != 7 {
		t.Fatalf("%+v", facts)
	}
	// Longest running first: the node is 30 s old, the detached process
	// 20 s, the builds 10 s; only the detached one is marked so.
	if facts.Shown[0].Pid != 107 || facts.Shown[0].Command != "node server.js" {
		t.Fatalf("%+v", facts.Shown[0])
	}
	if facts.Shown[1].Pid != 300 || !facts.Shown[1].Detached {
		t.Fatalf("%+v", facts.Shown[1])
	}
	detached := 0
	for _, process := range facts.Shown {
		if process.Detached {
			detached++
		}
	}
	if detached != 1 {
		t.Fatal(detached)
	}
	for _, identity := range facts.Identities {
		if identity.Start != fmt.Sprintf("boot:%d", identity.Pid) {
			t.Fatalf("%+v", identity)
		}
		if identity.Pid == 105 {
			t.Fatal("the bridge was reported as work")
		}
	}
}

func TestNothingIsAgentWorkBeforeTheHarnessIsConfigured(t *testing.T) {
	scan := workFactsContext("/bin/hel")
	scan.HarnessAgeMs = nil
	facts := selectProcesses(workFactsBusyTable(), &scan, workFactsDescribe)
	// Only the process outside the harness tree remains.
	if facts.Running != 1 || facts.Shown[0].Pid != 300 {
		t.Fatalf("%+v", facts)
	}
}

func TestLongCommandsAreRedactedAndCut(t *testing.T) {
	secret := "ghp_" + strings.Repeat("a1B2", 12)
	text := commandText(fmt.Sprintf("curl -H token:%s\n  --data %s", secret, strings.Repeat("word ", 100)))
	if strings.Contains(text, secret) || strings.Contains(text, "\n") {
		t.Fatal(text)
	}
	if utf8.RuneCountInString(text) != core.CommandChars+1 || !strings.HasSuffix(text, "…") {
		t.Fatal(text)
	}
}

// workFactsGit runs Git in directory with a fixed identity.
func workFactsGit(t *testing.T, directory string, args ...string) {
	t.Helper()
	command := exec.Command("git", append([]string{"-C", directory, "-c", "user.name=t", "-c", "user.email=t@example.invalid"}, args...)...)
	output, err := core.RunWithInput(command, nil)
	if err != nil || !output.Status.Success() {
		t.Fatalf("git %v: %v %s", args, err, output.Stderr)
	}
}

func workFactsKnownTree(t *testing.T, observed core.Observed[core.WorktreeFacts]) core.WorktreeFacts {
	t.Helper()
	if observed.Value == nil {
		t.Fatalf("worktree unavailable: %s", observed.Reason)
	}
	return *observed.Value
}

func workFactsWrite(t *testing.T, path, contents string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(contents), 0o666); err != nil {
		t.Fatal(err)
	}
}

// Git writes more than a pipe holds when thousands of paths are dirty, so
// a collector that writes before it reads, or that stops at the first
// buffer, shows up here.
func TestWorktreeCountsThousandsOfDirtyPathsExactlyAndShowsEight(t *testing.T) {
	directory := t.TempDir()
	workFactsGit(t, directory, "init", "-q", "-b", "task")
	workFactsWrite(t, filepath.Join(directory, "tracked.txt"), "one\n")
	workFactsGit(t, directory, "add", "tracked.txt")
	workFactsGit(t, directory, "commit", "-qm", "Fix parser, with unicode: naïve")
	workFactsWrite(t, filepath.Join(directory, "tracked.txt"), "two\n")
	for index := 0; index < 5000; index++ {
		workFactsWrite(t, filepath.Join(directory, fmt.Sprintf("untracked-file-with-a-long-name-%05d.txt", index)), "")
	}
	facts := workFactsKnownTree(t, readWorktree(context.Background(), &ObserverChildren{}, "git", directory))
	if facts.Changed != 5001 || facts.Modified != 1 || facts.Untracked != 5000 {
		t.Fatalf("%+v", facts)
	}
	if len(facts.Shown) != core.ShownPaths || facts.Omitted != 5001-8 {
		t.Fatalf("%+v", facts)
	}
	if facts.Branch == nil || *facts.Branch != "task" {
		t.Fatal(facts.Branch)
	}
	head := facts.Head
	if head == nil || len(head.Sha) != 40 {
		t.Fatalf("%+v", head)
	}
	if subject := head.Subject.Known(); subject == nil || *subject != "Fix parser, with unicode: naïve" {
		t.Fatalf("%+v", head.Subject)
	}
}

func TestADirectoryThatIsNotARepositoryIsUnavailableNotClean(t *testing.T) {
	observed := readWorktree(context.Background(), &ObserverChildren{}, "git", t.TempDir())
	if observed.Value != nil {
		t.Fatalf("reported %+v for a directory outside any repository", *observed.Value)
	}
	if !strings.Contains(observed.Reason, "not a git repository") {
		t.Fatal(observed.Reason)
	}
}

func TestRenamesAndPathsWithSpacesAreOneChangeEach(t *testing.T) {
	directory := t.TempDir()
	workFactsGit(t, directory, "init", "-q")
	workFactsWrite(t, filepath.Join(directory, "old name.txt"), "content that is long enough to be a rename\n")
	workFactsGit(t, directory, "add", ".")
	workFactsGit(t, directory, "commit", "-qm", "first")
	workFactsGit(t, directory, "mv", "old name.txt", "new name.txt")
	facts := workFactsKnownTree(t, readWorktree(context.Background(), &ObserverChildren{}, "git", directory))
	if facts.Changed != 1 || facts.Shown[0].Status != "R" || facts.Shown[0].Path != "new name.txt" {
		t.Fatalf("%+v", facts)
	}
}

// workFactsTestObserver is an observer whose Git must never run.
func workFactsTestObserver(session string, worker uint32) WorkObserver {
	return WorkObserver{
		SessionID:      session,
		WorkerPID:      worker,
		Directory:      "/nonexistent",
		Git:            "/nonexistent/git-must-not-run",
		HarnessReadyMs: new(atomic.Int64),
		Children:       &ObserverChildren{},
	}
}

func workFactsShownPids(processes *core.ProcessFacts) []uint32 {
	var pids []uint32
	for _, process := range processes.Shown {
		pids = append(pids, process.Pid)
	}
	return pids
}

func workFactsNumberedReader() ProcessReader {
	return newProcessReader(func(row *ProcRow) Described {
		return Described{Command: fmt.Sprintf("process %d", row.Pid), Identity: fmt.Sprintf("boot:%d", row.Pid)}
	})
}

// Process ages and the harness's age are measured at one instant. A helper
// that started just before the harness was configured stays a helper
// however long the rest of the scan takes; the slow scan used to age the
// harness past the helper and report it as the agent's work.
func TestASlowScanDoesNotTurnAHelperIntoTheAgentsWork(t *testing.T) {
	worker := uint32(4_000_000)
	observer := workFactsTestObserver("one-sampling-instant", worker)
	sampledMs := core.EpochMillis()
	observer.HarnessReadyMs.Store(sampledMs - 1_000)
	row := func(pid uint32, ageMs int64) ProcRow {
		return ProcRow{Pid: pid, Ppid: worker, AgeMs: ageMs, Marker: WorkFactsMarkerAbsent}
	}
	facts := scanProcessesFrom(&observer, func(string) (ProcessTable, error) {
		// 11 started 50 ms before the harness was configured, 12 after.
		rows := []ProcRow{row(11, 1_050), row(12, 500)}
		// The rest of the scan: reading what the survivors are.
		time.Sleep(300 * time.Millisecond)
		return ProcessTable{Rows: rows, Reader: workFactsNumberedReader(), SampledMs: sampledMs, ResolutionMs: 0}, nil
	})
	if facts.Value == nil {
		t.Fatal(facts.Reason)
	}
	if pids := workFactsShownPids(facts.Value); !slices.Equal(pids, []uint32{12}) {
		t.Fatalf("%v %+v", pids, *facts.Value)
	}
}

func TestUntrackedDirectoriesAreExpandedWhateverTheRepositoryConfigures(t *testing.T) {
	directory := t.TempDir()
	workFactsGit(t, directory, "init", "-q")
	workFactsGit(t, directory, "config", "status.showUntrackedFiles", "no")
	if err := os.Mkdir(filepath.Join(directory, "newdir"), 0o777); err != nil {
		t.Fatal(err)
	}
	workFactsWrite(t, filepath.Join(directory, "newdir/a.txt"), "a\n")
	workFactsWrite(t, filepath.Join(directory, "newdir/b.txt"), "b\n")
	facts := workFactsKnownTree(t, readWorktree(context.Background(), &ObserverChildren{}, "git", directory))
	if facts.Changed != 2 || facts.Untracked != 2 {
		t.Fatalf("%+v", facts)
	}
	var paths []string
	for _, path := range facts.Shown {
		paths = append(paths, path.Path)
	}
	if !slices.Equal(paths, []string{"newdir/a.txt", "newdir/b.txt"}) {
		t.Fatal(paths)
	}
}

func TestAProcessWhoseEnvironmentCannotBeReadIsCountedNotOmitted(t *testing.T) {
	rows := workFactsBusyTable()
	unreadable := workFactsRow(400, 1, 15_000, false)
	unreadable.Marker = WorkFactsMarkerUnreadable
	rows = append(rows, unreadable)
	// One inside the harness's tree needs no marker to be attributed.
	inTree := workFactsRow(401, 105, 15_000, false)
	inTree.Marker = WorkFactsMarkerUnreadable
	rows = append(rows, inTree)
	scan := workFactsContext("/bin/hel")
	facts := selectProcesses(rows, &scan, workFactsDescribe)
	if facts.Unattributed != 1 || facts.Running != 13 {
		t.Fatalf("%+v", facts)
	}
	for _, identity := range facts.Identities {
		if identity.Pid == 400 {
			t.Fatal("the unreadable process outside the tree was attributed")
		}
	}
}

func TestARealLongPathIsStoredExactlyAsGitReportedIt(t *testing.T) {
	directory := t.TempDir()
	workFactsGit(t, directory, "init", "-q")
	long := "src/a-very-long-ordinary-file-name-with-more-than-twenty-four-characters.rs"
	spaced := "docs/notes  with   spaces.md"
	for _, dir := range []string{"src", "docs"} {
		if err := os.MkdirAll(filepath.Join(directory, dir), 0o777); err != nil {
			t.Fatal(err)
		}
	}
	workFactsWrite(t, filepath.Join(directory, long), "")
	workFactsWrite(t, filepath.Join(directory, spaced), "")
	facts := workFactsKnownTree(t, readWorktree(context.Background(), &ObserverChildren{}, "git", directory))
	var paths []string
	for _, path := range facts.Shown {
		paths = append(paths, path.Path)
	}
	if !slices.Equal(paths, []string{spaced, long}) {
		t.Fatal(paths)
	}
}

// The scan and the Git reads of a capture overlap: each waits on a barrier
// that only the other side releases, so the capture finishes only if both
// run at once. (The outer deadline only turns a regression into a failure
// instead of a hang.)
func TestACaptureRunsTheScanAndTheTreeReadTogether(t *testing.T) {
	var barrier sync.WaitGroup
	barrier.Add(2)
	meet := func() {
		barrier.Done()
		barrier.Wait()
	}
	finished := make(chan core.WorkFacts, 1)
	go func() {
		finished <- captureFrom(
			func() core.Observed[core.ProcessFacts] {
				meet()
				return core.ObservedUnavailable[core.ProcessFacts]("a fake scan")
			},
			func() core.Observed[core.WorktreeFacts] {
				meet()
				return core.ObservedUnavailable[core.WorktreeFacts]("a fake tree read")
			},
		)
	}()
	select {
	case facts := <-finished:
		if facts.Processes.Known() != nil || facts.Worktree.Known() != nil {
			t.Fatalf("%+v", facts)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("the two reads did not run together")
	}
}

// ps etime is whole seconds while the harness's readiness age is in
// milliseconds, so a process that started near readiness reads younger or
// older than it is. Uncertainty reports the process: one that started within
// the resolution of readiness counts as the agent's work, and only one that
// started clearly before it is a helper.
func TestAProcessNearReadinessCountsAsWorkAtWholeSecondResolution(t *testing.T) {
	worker := uint32(4_000_000)
	observer := workFactsTestObserver("whole-second-ages", worker)
	sampledMs := core.EpochMillis()
	observer.HarnessReadyMs.Store(sampledMs - 10_000)
	row := func(pid uint32, ageMs int64) ProcRow {
		return ProcRow{Pid: pid, Ppid: worker, AgeMs: ageMs, Marker: WorkFactsMarkerAbsent}
	}
	facts := scanProcessesFrom(&observer, func(string) (ProcessTable, error) {
		return ProcessTable{
			// Readiness was 10 s ago. 11 started 3 s before it, 12 started
			// 0.5 s after it (9.5 s old, shown as 9 s), 13 started 0.3 s
			// before it (10.3 s old, shown as 10 s).
			Rows:         []ProcRow{row(11, 13_000), row(12, 9_000), row(13, 10_000)},
			Reader:       workFactsNumberedReader(),
			SampledMs:    sampledMs,
			ResolutionMs: 1_000,
		}, nil
	})
	if facts.Value == nil {
		t.Fatal(facts.Reason)
	}
	pids := workFactsShownPids(facts.Value)
	slices.Sort(pids)
	if !slices.Equal(pids, []uint32{12, 13}) {
		t.Fatalf("%v %+v", pids, *facts.Value)
	}
}
