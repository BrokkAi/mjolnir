// Port of mj-worker/src/work_facts.rs.
//
// Worker-owned observation of what a session's agent left running and of
// its working tree, captured when a managed child's turn ends and journaled
// with the turn's outcome. WatchLiveness then republishes how many of those
// processes are still alive.
//
// The processes of a session are those that descend from this worker (the
// harness and everything its tools start) plus any process anywhere that
// carries the session's MJ_SESSION_ID. The second set matters: a process
// started with setsid, or whose launching shell exited, has been handed to
// PID 1 and no walk of parent links finds it. The controller exports the
// variable to every harness, and tool shells pass it on.
//
// Helpers the harness needs are not work, and they are identified by which
// processes they are, never by which program they run: the harness's bridge
// and its helper processes (an MCP server, a language runtime the bridge
// runs on) are the descendants of this worker that already existed when the
// harness session was configured, and Mjolnir's own commands are processes
// of its own executable. A descendant that started after that moment was
// started by the agent, even when it runs the very node the bridge runs on.
//
// Names that collide with other mj-worker modules carry this module's
// prefix: WorkFactsCapture (Rust capture) and WorkFactsMarker (Rust
// Marker). The platform readers are readTable in work_facts_linux.go,
// work_facts_macos_darwin.go and work_facts_other.go.

package worker

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"os"
	"os/exec"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WorkObserver is everything the worker knows that the observation needs.
// Copied into each relay connection; the timestamp and the children
// registry are shared by pointer, as Rust shares them through Arc.
type WorkObserver struct {
	// SessionID is the session whose MJ_SESSION_ID marks its processes.
	SessionID string
	// WorkerPID is the worker's own process: its descendants are the
	// harness tree.
	WorkerPID uint32
	// Directory is the agent's working directory.
	Directory string
	// Git is the Git program. Only a test names another than "git".
	Git string
	// HarnessReadyMs is epoch milliseconds at which the harness session was
	// last configured, or 0 while no configured harness exists. The relay
	// writes it.
	HarnessReadyMs *atomic.Int64
	// Children are the processes this worker has started to collect facts
	// (its Git reads), whichever request started them, while they run.
	// Shared by every connection's copy of the observer, so a scan for one
	// request never reports the subprocess of another as the agent's work.
	Children *ObserverChildren
}

// registeredChild is one recorded subprocess of the observer, removed from
// the registry by release (Rust: Drop) when the read ended or its request
// went away.
type registeredChild struct {
	children *ObserverChildren
	pid      uint32
}

// release removes the child from the registry. Unlike Rust's Drop it can
// wait: a scan holding the registry only delays the removal.
func (c *registeredChild) release() {
	c.children.mu.Lock()
	defer c.children.mu.Unlock()
	delete(c.children.pids, c.pid)
}

// ObserverChildren is the registry of the observer's own subprocesses. A
// scan holds it for the whole scan and a spawn holds it from before the
// spawn until the new process is recorded, so a scan sees every such
// process that exists, or none that is not yet recorded. The zero value is
// an empty registry (Rust: Default); share it by pointer.
type ObserverChildren struct {
	mu sync.Mutex
	// pids is nil until the first child is recorded.
	pids map[uint32]struct{}
}

// ProcRow is one process as the platform reader sees it. Cheap fields only;
// the text of the command line and the start identity are read for the
// survivors.
type ProcRow struct {
	Pid  uint32
	Ppid uint32
	// AgeMs is the milliseconds the process has existed, on the host's
	// monotonic clock, rounded down to the table's resolution.
	AgeMs  int64
	Zombie bool
	Marker WorkFactsMarker
}

// WorkFactsMarker is Rust's work_facts::Marker (prefixed: review/hook's
// MARKER keeps the name): whether a process's environment carries this
// session's MJ_SESSION_ID.
type WorkFactsMarker int

const (
	WorkFactsMarkerPresent WorkFactsMarker = iota
	WorkFactsMarkerAbsent
	// WorkFactsMarkerUnreadable: the environment could not be read although
	// the process is the same user's (a process that forbids it, such as
	// one that is not dumpable), so whether it is this session's cannot be
	// known.
	WorkFactsMarkerUnreadable
)

type Described struct {
	// Command is the raw command line; arguments separated by one space.
	Command  string
	Exe      *string
	Identity string
}

type ScanContext struct {
	WorkerPID uint32
	// HarnessAgeMs is the age of the harness's configured session in ms;
	// nil before it exists.
	HarnessAgeMs *int64
	// ResolutionMs is the resolution of the ages in the table
	// (ProcessTable.ResolutionMs).
	ResolutionMs int64
	// OwnExe is Mjolnir's own executable: its commands are never the
	// agent's work.
	OwnExe *string
	// ObserverChildren are the observer's own subprocesses; they and their
	// descendants are never the agent's work.
	ObserverChildren map[uint32]struct{}
}

// selectProcesses decides which rows are the agent's work. Pure: the
// platform readers feed it.
func selectProcesses(rows []ProcRow, scan *ScanContext, describe func(*ProcRow) Described) core.ProcessFacts {
	children := map[uint32][]uint32{}
	for _, row := range rows {
		children[row.Ppid] = append(children[row.Ppid], row.Pid)
	}
	// descend adds every descendant of pending to set.
	descend := func(set map[uint32]struct{}, pending []uint32) {
		for len(pending) > 0 {
			pid := pending[len(pending)-1]
			pending = pending[:len(pending)-1]
			for _, child := range children[pid] {
				if _, seen := set[child]; !seen {
					set[child] = struct{}{}
					pending = append(pending, child)
				}
			}
		}
	}
	tree := map[uint32]struct{}{}
	descend(tree, []uint32{scan.WorkerPID})
	// The observer's own subprocesses, with everything they started.
	own := map[uint32]struct{}{}
	pending := make([]uint32, 0, len(scan.ObserverChildren))
	for pid := range scan.ObserverChildren {
		own[pid] = struct{}{}
		pending = append(pending, pid)
	}
	descend(own, pending)
	type workItem struct {
		row       *ProcRow
		described Described
		detached  bool
	}
	var work []workItem
	unattributed := uint32(0)
	for index := range rows {
		row := &rows[index]
		if _, mine := own[row.Pid]; row.Pid == scan.WorkerPID || row.Zombie || mine {
			continue
		}
		_, inTree := tree[row.Pid]
		if !inTree && row.Marker == WorkFactsMarkerUnreadable {
			// Not the harness's descendant and not readable: it may be work
			// the agent detached, and it is counted rather than left out.
			if unattributed < math.MaxUint32 {
				unattributed++
			}
			continue
		}
		if !inTree && row.Marker != WorkFactsMarkerPresent {
			continue
		}
		// A descendant that existed when the harness session was configured
		// is part of the harness. Before that moment nothing is the agent's.
		// A process is taken to be a helper only when its age puts its start
		// before readiness by more than the ages' resolution (whole seconds
		// on macOS): one that may have started on either side of it is
		// reported as work, so uncertainty shows a process and never hides
		// one.
		if inTree && (scan.HarnessAgeMs == nil || row.AgeMs >= workFactsSaturatingAdd(*scan.HarnessAgeMs, scan.ResolutionMs)) {
			continue
		}
		described := describe(row)
		if scan.OwnExe != nil && described.Exe != nil &&
			slices.Equal(workFactsPathComponents(*described.Exe), workFactsPathComponents(*scan.OwnExe)) {
			continue
		}
		work = append(work, workItem{row: row, described: described, detached: !inTree})
	}
	// Longest running first; the process ID makes the order total.
	slices.SortStableFunc(work, func(a, b workItem) int {
		if order := cmp.Compare(b.row.AgeMs, a.row.AgeMs); order != 0 {
			return order
		}
		return cmp.Compare(a.row.Pid, b.row.Pid)
	})
	running := uint32(min(len(work), math.MaxUint32))
	shown := []core.RunningProcess{}
	for _, item := range work[:min(len(work), core.ShownProcesses)] {
		shown = append(shown, core.RunningProcess{
			Pid:         item.row.Pid,
			ElapsedSecs: uint64(max(item.row.AgeMs/1000, 0)),
			Command:     commandText(item.described.Command),
			Detached:    item.detached,
		})
	}
	identities := []core.ProcessIdentity{}
	for _, item := range work {
		identities = append(identities, core.ProcessIdentity{Pid: item.row.Pid, Start: item.described.Identity})
	}
	return core.ProcessFacts{
		Unattributed: unattributed,
		Running:      running,
		Omitted:      running - uint32(len(shown)),
		Shown:        shown,
		Identities:   identities,
	}
}

// workFactsSaturatingAdd is Rust's i64::saturating_add.
func workFactsSaturatingAdd(a, b int64) int64 {
	sum := a + b
	switch {
	case a > 0 && b > 0 && sum < 0:
		return math.MaxInt64
	case a < 0 && b < 0 && sum >= 0:
		return math.MinInt64
	}
	return sum
}

// workFactsSaturatingSub is Rust's i64::saturating_sub.
func workFactsSaturatingSub(a, b int64) int64 {
	if b == math.MinInt64 {
		if a >= 0 {
			return math.MaxInt64
		}
		return a - b
	}
	return workFactsSaturatingAdd(a, -b)
}

// workFactsBlocking runs blocking work as Rust's spawn_blocking does: a
// panic in it becomes an error instead of ending the worker.
func workFactsBlocking[T any](work func() T) (result T, err error) {
	defer func() {
		if recovered := recover(); recovered != nil {
			err = fmt.Errorf("task panicked: %v", recovered)
		}
	}()
	return work(), nil
}

// commandText is one line, free of terminal controls and secrets, cut for
// reading.
func commandText(raw string) string {
	clean := core.SanitizeTerminalText(raw)
	clean = strings.Join(strings.Fields(clean), " ")
	clean = core.RedactSecrets(clean)
	if utf8.RuneCountInString(clean) <= core.CommandChars {
		return clean
	}
	runes := []rune(clean)
	return string(runes[:core.CommandChars]) + "…"
}

// ProcessTable is what a platform reader returns for one scan.
type ProcessTable struct {
	Rows   []ProcRow
	Reader ProcessReader
	// SampledMs is the wall-clock instant the ages in Rows were measured at.
	// The harness's age is taken at this instant too: the scan after it can
	// take as long as it likes without moving the line between a helper and
	// work.
	SampledMs int64
	// ResolutionMs is the resolution of the ages in Rows, in milliseconds:
	// the true age of a process is somewhere within it of the age given. ps
	// on macOS reports whole seconds; Linux gives the clock tick.
	ResolutionMs int64
}

func scanProcesses(observer *WorkObserver) core.Observed[core.ProcessFacts] {
	return scanProcessesFrom(observer, readTable)
}

func scanProcessesFrom(observer *WorkObserver, readTable func(sessionID string) (ProcessTable, error)) core.Observed[core.ProcessFacts] {
	// Held for the whole scan: a Git read that starts meanwhile waits to be
	// recorded, and one already running is in the registry.
	observer.Children.mu.Lock()
	defer observer.Children.mu.Unlock()
	var ownExe *string
	if exe, err := os.Executable(); err == nil {
		// A replaced executable reads back with this suffix.
		exe = strings.TrimSuffix(exe, " (deleted)")
		ownExe = &exe
	}
	readyMs := observer.HarnessReadyMs.Load()
	table, err := readTable(observer.SessionID)
	if err != nil {
		return core.ObservedUnavailable[core.ProcessFacts](err.Error())
	}
	scan := ScanContext{
		WorkerPID:        observer.WorkerPID,
		ResolutionMs:     table.ResolutionMs,
		OwnExe:           ownExe,
		ObserverChildren: observer.Children.pids,
	}
	// The configured moment is wall-clock; ages are monotonic, so convert
	// through the instant the ages were sampled at, not the present.
	if readyMs > 0 {
		age := workFactsSaturatingSub(table.SampledMs, readyMs)
		scan.HarnessAgeMs = &age
	}
	return core.ObservedKnown(selectProcesses(table.Rows, &scan, table.Reader.describe))
}

// ProcessReader reads the text and identity of a process the scan kept.
type ProcessReader struct {
	describeRow func(*ProcRow) Described
}

func newProcessReader(describe func(*ProcRow) Described) ProcessReader {
	return ProcessReader{describeRow: describe}
}

func (r ProcessReader) describe(row *ProcRow) Described {
	return r.describeRow(row)
}

// readWorktree is what `git status` and `git log` say about directory. The
// two reads run together, so the tree costs the slower of them and not
// their sum. The log read asks for HEAD with its id (%H), and its subject is
// used only when that id is the one status reported: a commit made between
// the two reads would otherwise give the id of one commit the subject of
// another, and the subject is then unavailable, with that reason.
func readWorktree(ctx context.Context, children *ObserverChildren, git string, directory string) core.Observed[core.WorktreeFacts] {
	var status []byte
	var statusErr error
	var reads sync.WaitGroup
	reads.Go(func() {
		status, statusErr = runGit(ctx, children, git, directory, []string{
			"status",
			"--porcelain=v2",
			"-z",
			"--branch",
			// Whatever `status.showUntrackedFiles` says, and with untracked
			// directories expanded to their files: a configured `no` would
			// report a tree with new files as clean.
			"--untracked-files=all",
		})
	})
	head, headErr := runGit(ctx, children, git, directory, []string{
		"log", "-1", "--format=%H%x00%s", "--no-show-signature", "HEAD", "--",
	})
	reads.Wait()
	if statusErr != nil {
		return core.ObservedUnavailable[core.WorktreeFacts](statusErr.Error())
	}
	facts := parseStatus(fromUTF8Lossy(status), directory)
	if commit := facts.Head; commit != nil {
		switch sha, subject, ok := strings.Cut(fromUTF8Lossy(head), "\x00"); {
		case headErr != nil:
			commit.Subject = core.ObservedUnavailable[string](headErr.Error())
		// Exactly as Git reported it: redaction and cutting are for the
		// rendering, which cannot tell a long branch name from a secret.
		case ok && sha == commit.Sha:
			commit.Subject = core.ObservedKnown(strings.TrimRight(subject, "\n"))
		case ok:
			commit.Subject = core.ObservedUnavailable[string]("HEAD moved while the working tree was being read")
		default:
			commit.Subject = core.ObservedUnavailable[string]("git log gave no subject")
		}
	}
	return core.ObservedKnown(facts)
}

// runGit runs Git as a child the observation owns: in a process group of
// its own, killed and reaped when ctx ends (the requester disconnected, or
// the worker is stopping), so a stalled Git never outlives its question and
// repeated requests cannot pile scans up. While the caller stays connected
// it waits with no deadline and no limit on the output, because neither has
// a source: a git status takes as long as the tree needs. The error's text
// is the reason the read is unavailable.
func runGit(ctx context.Context, children *ObserverChildren, git string, directory string, args []string) ([]byte, error) {
	command := exec.Command(git, append([]string{"-C", directory}, args...)...)
	// GIT_OPTIONAL_LOCKS=0 keeps this read from taking the index lock a
	// concurrent `git commit` of the agent's would need.
	command.Env = append(os.Environ(), "GIT_OPTIONAL_LOCKS=0", "GIT_TERMINAL_PROMPT=0")
	// Recorded as the child starts, under the registry's lock, which a scan
	// holds for its whole duration: a scan sees a Git read that exists, or
	// the read waits to be recorded. Removed when the read ends, however it
	// ends, including when ctx ends on a disconnect.
	children.mu.Lock()
	locked := true
	var registered *registeredChild
	output, err := core.RunBoundedObserved(ctx, command, math.MaxInt, time.Duration(math.MaxInt64), func(pid int) {
		if children.pids == nil {
			children.pids = map[uint32]struct{}{}
		}
		children.pids[uint32(pid)] = struct{}{}
		registered = &registeredChild{children: children, pid: uint32(pid)}
		locked = false
		children.mu.Unlock()
	})
	if locked {
		children.mu.Unlock()
	}
	if registered != nil {
		registered.release()
	}
	if err != nil {
		return nil, fmt.Errorf("could not run git: %w", err)
	}
	if output.Status.Success() {
		return output.Stdout, nil
	}
	detail := ""
	if lines := rustLines(fromUTF8Lossy(output.Stderr)); len(lines) > 0 {
		detail = strings.TrimSpace(lines[0])
	}
	if detail == "" {
		return nil, fmt.Errorf("git failed with %s", workFactsExitStatus(output.Status))
	}
	return nil, errors.New(detail)
}

// parseStatus parses `git status --porcelain=v2 -z --branch`: header
// records beginning "# ", then NUL-separated change records. A rename
// record is followed by one more field, the original path.
func parseStatus(text string, directory string) core.WorktreeFacts {
	var headSha, branch *string
	var changed, modified, untracked uint32
	shown := []core.ChangedPath{}
	fields := strings.Split(text, "\x00")
	for index := 0; index < len(fields); index++ {
		field := fields[index]
		if header, ok := strings.CutPrefix(field, "# "); ok {
			name, value, ok := strings.Cut(header, " ")
			switch {
			case ok && name == "branch.oid" && value != "(initial)":
				headSha = &value
			case ok && name == "branch.head" && value != "(detached)":
				branch = &value
			}
			continue
		}
		if len(field) < 2 {
			continue
		}
		var status, path string
		switch record, rest := field[:2], field[2:]; record {
		case "? ":
			status, path = "?", rest
		// `1 XY sub mH mI mW hH hI path`, `2 XY ... score path` and
		// `u XY ... path` put the path after the eighth, ninth and tenth
		// space-separated fields; paths may contain spaces.
		case "1 ":
			status, path = changeRecord(rest, 8)
		case "2 ":
			// The original path of a rename is its own field.
			index++
			status, path = changeRecord(rest, 9)
		case "u ":
			status, path = changeRecord(rest, 10)
		default:
			continue
		}
		changed = workFactsSaturatingIncrement(changed)
		switch status {
		case "?":
			untracked = workFactsSaturatingIncrement(untracked)
		case "M":
			modified = workFactsSaturatingIncrement(modified)
		}
		if len(shown) < core.ShownPaths {
			// Exactly as Git reported it; the rendering sanitizes and cuts.
			shown = append(shown, core.ChangedPath{Status: status, Path: path})
		}
	}
	facts := core.WorktreeFacts{
		Directory: directory,
		Branch:    branch,
		Changed:   changed,
		Modified:  modified,
		Untracked: untracked,
		Omitted:   changed - min(changed, uint32(len(shown))),
		Shown:     shown,
	}
	if headSha != nil {
		facts.Head = &core.HeadCommit{
			Sha:     *headSha,
			Subject: core.ObservedUnavailable[string]("the subject was not read"),
		}
	}
	return facts
}

func workFactsSaturatingIncrement(count uint32) uint32 {
	if count == math.MaxUint32 {
		return count
	}
	return count + 1
}

// changeRecord is the status letter and path of a 1, 2 or u record whose
// text after the record type is rest; leading fields precede the path.
func changeRecord(rest string, leading int) (string, string) {
	parts := strings.SplitN(rest, " ", leading)
	xy, path := "..", ""
	if len(parts) > 0 {
		xy = parts[0]
	}
	if len(parts) > 1 {
		path = parts[len(parts)-1]
	}
	// Missing letters read as '.', as Rust's unwrap_or('.') does.
	letters := append([]rune(xy), '.', '.')
	index, tree := letters[0], letters[1]
	letter := tree
	switch {
	case index == 'U' || tree == 'U':
		letter = 'U'
	case index != '.':
		letter = index
	}
	return string(letter), path
}

// WorkFactsCapture is Rust's work_facts::capture (prefixed: move_workspace's
// capture keeps Capture): what a sub-agent's worker observes of its agent's
// work when a turn ends: the processes under the worker or carrying the
// session's marker, and the working tree. The two are read together. A scan
// never reports the observer's own Git children (they are registered as
// they start). The Git reads have no fixed duration; the caller that waits
// for them cancels by ending ctx, which kills them.
func WorkFactsCapture(ctx context.Context, observer *WorkObserver) core.WorkFacts {
	scan := func() core.Observed[core.ProcessFacts] {
		facts, err := workFactsBlocking(func() core.Observed[core.ProcessFacts] { return scanProcesses(observer) })
		if err != nil {
			return core.ObservedUnavailable[core.ProcessFacts](fmt.Sprintf("process scan failed: %v", err))
		}
		return facts
	}
	// Read here, not handed to another goroutine to own: the Git children
	// belong to this call and are killed when ctx ends.
	return captureFrom(scan, func() core.Observed[core.WorktreeFacts] {
		return readWorktree(ctx, observer.Children, observer.Git, observer.Directory)
	})
}

// captureFrom runs the two observations of a capture together: the scan of
// the processes beside the Git reads. The time added to a turn's end is the
// slower of the two, not their sum.
func captureFrom(processes func() core.Observed[core.ProcessFacts], worktree func() core.Observed[core.WorktreeFacts]) core.WorkFacts {
	var scanned core.Observed[core.ProcessFacts]
	var done sync.WaitGroup
	done.Go(func() { scanned = processes() })
	tree := worktree()
	done.Wait()
	return core.WorkFacts{
		ObservedAtMs: uint64(max(core.EpochMillis(), 0)),
		Processes:    scanned,
		Worktree:     tree,
	}
}

// LivenessInterval is how often the watcher recounts live work processes.
// This is the latency with which the daemon learns that the last process
// exited, not a limit on anything: a recount reads the process table once.
const LivenessInterval = 5 * time.Second

// countAlive is how many of identities are still alive: the ones a fresh
// scan finds with the same pid and start identity, so a reused pid does not
// count.
func countAlive(observer *WorkObserver, identities []core.ProcessIdentity) core.Observed[uint32] {
	now := scanProcesses(observer)
	if now.Value == nil {
		return core.ObservedUnavailable[uint32](now.Reason)
	}
	alive := uint32(0)
	for _, wanted := range identities {
		if slices.Contains(now.Value.Identities, wanted) {
			alive++
		}
	}
	return core.ObservedKnown(alive)
}

// WatchLiveness keeps work_liveness in the relay snapshot true while the
// last finished turn's work processes run. It follows the snapshot, so it
// resumes after a worker restart and stops by itself when the count reaches
// zero or the next prompt replaces the turn. A scan that fails leaves the
// published count as it was, and the failure is logged once until a scan
// succeeds. Returns only on a durable relay failure, which it reports, or
// when ctx ends (Rust: the task is dropped).
func WatchLiveness(ctx context.Context, relay *RelayCell, observer WorkObserver, interval time.Duration) error {
	releases := relay.Releases()
	failing := false
	for {
		guard := relay.Observe()
		liveness, identities, watched := guard.watchedWork()
		guard.Unlock()
		if !watched {
			// Nothing to watch until a turn leaves processes running.
			if err := releases.Changed(ctx); err != nil {
				return err
			}
			continue
		}
		pause := time.NewTimer(interval)
		select {
		case <-pause.C:
		case <-ctx.Done():
			pause.Stop()
			return ctx.Err()
		}
		counted, err := workFactsBlocking(func() core.Observed[uint32] { return countAlive(&observer, identities) })
		if err != nil {
			if !failing {
				slog.Warn("work liveness scan task failed", "session_id", observer.SessionID, "error", err)
			}
			failing = true
			continue
		}
		if counted.Value == nil {
			if !failing {
				slog.Warn("work liveness scan failed", "session_id", observer.SessionID, "reason", counted.Reason)
			}
			failing = true
			continue
		}
		failing = false
		locked := relay.Lock()
		_, err = locked.recordWorkLiveness(core.WorkLiveness{
			CompletedOrdinal: liveness.CompletedOrdinal,
			Alive:            *counted.Value,
			CheckedAtMs:      uint64(max(core.EpochMillis(), 0)),
		})
		locked.Unlock()
		if err != nil {
			return fmt.Errorf("recording work liveness: %w", err)
		}
	}
}

// workFactsExitStatus formats a process exit as Rust's ExitStatus Display
// does: "exit status: N", or "signal: N (name)" for a signal. Go names the
// signal by its description ("killed") where Rust prints "SIGKILL".
func workFactsExitStatus(state *os.ProcessState) string {
	if state == nil {
		return "unknown status"
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
		return fmt.Sprintf("signal: %d (%s)", int(status.Signal()), status.Signal())
	}
	return fmt.Sprintf("exit status: %d", state.ExitCode())
}

// workFactsPathComponents is a path's components as Rust's Path::components
// yields them, for comparing paths as Rust's PathBuf equality does: the root
// as "/", "." dropped except at the start, ".." kept.
func workFactsPathComponents(path string) []string {
	separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
	var components []string
	if path != "" && separator(rune(path[0])) {
		components = append(components, "/")
	}
	for index, part := range strings.FieldsFunc(path, separator) {
		if part == "." && !(index == 0 && components == nil && strings.HasPrefix(path, ".")) {
			continue
		}
		components = append(components, part)
	}
	return components
}
