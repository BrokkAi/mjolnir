// Port of mj-core/src/work_facts.rs.
//
// Facts about a child session's live work that its own worker observed:
// which processes the agent left running, and the state of its working tree.
//
// The worker owns these facts (it owns the harness process tree and the
// working directory). The daemon asks for them once per finish, records the
// answer, and every surface (the finish report, the park message and
// list_agents) renders the recorded value through WorkFacts.RenderLines, so
// the surfaces cannot disagree. A part that could not be observed says so
// with a reason; it is never rendered as zero.

package core

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"unicode/utf8"
)

// These three limits shape what a reader sees. Counts stay exact whatever
// they are, and no failure is guarded by them.

// ShownProcesses is the processes listed in a rendering; the rest are
// counted as omitted.
const ShownProcesses = 5

// ShownPaths is the changed paths listed in a rendering; the rest are
// counted as omitted.
const ShownPaths = 8

// CommandChars is the characters of a command line kept in a rendering.
const CommandChars = 160

// Observed is one part of the facts: observed, or not observable and why.
// Serialized adjacently tagged, snake_case:
// {"status":"known","value":...} or
// {"status":"unavailable","value":{"reason":...}}.
type Observed[T any] struct {
	// Value is set when the part was observed (Known).
	Value *T
	// Reason is why the part is unavailable, when Value is nil.
	Reason string
}

// ObservedKnown is Observed::Known.
func ObservedKnown[T any](value T) Observed[T] { return Observed[T]{Value: &value} }

// ObservedUnavailable is Observed::unavailable.
func ObservedUnavailable[T any](reason string) Observed[T] { return Observed[T]{Reason: reason} }

// Known is the observed value, or nil when unavailable.
func (o Observed[T]) Known() *T { return o.Value }

func (o Observed[T]) MarshalJSON() ([]byte, error) {
	if o.Value != nil {
		return stateMarshalAdjacent("status", "known", "value", *o.Value)
	}
	return stateMarshalAdjacent("status", "unavailable", "value", struct {
		Reason string `json:"reason"`
	}{o.Reason})
}

func (o *Observed[T]) UnmarshalJSON(data []byte) error {
	var head struct {
		Status *string         `json:"status"`
		Value  json.RawMessage `json:"value"`
	}
	if err := json.Unmarshal(data, &head); err != nil {
		return err
	}
	if head.Status == nil {
		return fmt.Errorf("missing field `status`")
	}
	switch *head.Status {
	case "known":
		if head.Value == nil {
			return fmt.Errorf("missing field `value`")
		}
		var value T
		if err := json.Unmarshal(head.Value, &value); err != nil {
			return err
		}
		*o = Observed[T]{Value: &value}
		return nil
	case "unavailable":
		var body struct {
			Reason *string `json:"reason"`
		}
		if head.Value == nil || string(bytes.TrimSpace(head.Value)) == "null" {
			return fmt.Errorf("missing field `value`")
		}
		if err := json.Unmarshal(head.Value, &body); err != nil {
			return err
		}
		if body.Reason == nil {
			return fmt.Errorf("missing field `reason`")
		}
		*o = Observed[T]{Reason: *body.Reason}
		return nil
	}
	return fmt.Errorf("unknown variant `%s`", *head.Status)
}

type WorkFacts struct {
	// ObservedAtMs is the wall-clock time of the observation, for
	// list_agents ages. Renderings of a report never print it, so they stay
	// stable.
	ObservedAtMs uint64                  `json:"observed_at_ms"`
	Processes    Observed[ProcessFacts]  `json:"processes"`
	Worktree     Observed[WorktreeFacts] `json:"worktree"`
}

type ProcessFacts struct {
	// Running is the exact number of work processes found.
	Running uint32 `json:"running"`
	// Shown is the first ShownProcesses, longest running first.
	Shown []RunningProcess `json:"shown"`
	// Omitted is Running minus len(Shown).
	Omitted uint32 `json:"omitted"`
	// Identities is the identity of every counted process, for the worker's
	// liveness watcher, which republishes how many are still alive (see
	// WorkLiveness).
	Identities []ProcessIdentity `json:"identities"`
	// Unattributed is processes of the same user outside the harness's tree
	// whose environment could not be read, so that whether they carry this
	// session's marker, and are work the agent detached, is unknown. They
	// are not in Running.
	Unattributed uint32 `json:"unattributed"`
}

type RunningProcess struct {
	Pid         uint32 `json:"pid"`
	ElapsedSecs uint64 `json:"elapsed_secs"`
	// Command is the redacted, terminal-safe command line, cut at CommandChars.
	Command string `json:"command"`
	// Detached: not a descendant of the harness: its parent exited and it
	// was handed to PID 1 or another process outside the harness tree.
	Detached bool `json:"detached"`
}

// ProcessIdentity is a process ID together with its start identity: an ID
// alone can be reused.
type ProcessIdentity struct {
	Pid   uint32 `json:"pid"`
	Start string `json:"start"`
}

type WorktreeFacts struct {
	Directory string      `json:"directory"`
	Head      *HeadCommit `json:"head"`
	// Branch is nil when HEAD is detached.
	Branch *string `json:"branch"`
	// Changed is the exact number of changed paths, tracked or not.
	Changed   uint32 `json:"changed"`
	Modified  uint32 `json:"modified"`
	Untracked uint32 `json:"untracked"`
	// Shown is the first ShownPaths changed paths.
	Shown []ChangedPath `json:"shown"`
	// Omitted is Changed minus len(Shown).
	Omitted uint32 `json:"omitted"`
}

type HeadCommit struct {
	Sha string `json:"sha"`
	// Subject is the commit's subject, or why it is unavailable: it is read
	// beside the status and is the subject of this commit only if Git still
	// says HEAD is it.
	Subject Observed[string] `json:"subject"`
}

type ChangedPath struct {
	// Status is one status character as git status prints it: M, A, D, R, ?
	// for untracked, U for a conflict.
	Status string `json:"status"`
	Path   string `json:"path"`
}

// WorkLiveness reports whether the work processes a finished turn left
// running are still alive.
//
// The worker owns this fact: it watches the process identities recorded in
// that turn's WorkFacts and republishes the count as they exit, because
// only the worker can see its own process namespace (a remote host or a
// container). The daemon reads it from the relay snapshot to hold a child
// that is waiting on its own background work, and to delay a park that
// would stop those processes.
type WorkLiveness struct {
	// CompletedOrdinal of the turn whose work facts are watched.
	CompletedOrdinal uint64 `json:"completed_ordinal"`
	// Alive is how many of that turn's process identities are still alive.
	Alive uint32 `json:"alive"`
	// CheckedAtMs is the wall-clock time of the check that produced Alive.
	CheckedAtMs uint64 `json:"checked_at_ms"`
}

// WorkLivenessFromTurnFacts is the one rule for starting to watch a
// finished turn's work: facts that list live process identities, however
// the turn was started.
func WorkLivenessFromTurnFacts(completedOrdinal uint64, facts *WorkFacts) *WorkLiveness {
	if facts == nil || facts.Processes.Value == nil || len(facts.Processes.Value.Identities) == 0 {
		return nil
	}
	alive := uint32(min(len(facts.Processes.Value.Identities), int(^uint32(0))))
	return &WorkLiveness{CompletedOrdinal: completedOrdinal, Alive: alive, CheckedAtMs: facts.ObservedAtMs}
}

// WorkFactsUnavailable is facts when nothing could be observed, with the
// reason for both parts.
func WorkFactsUnavailable(reason string) WorkFacts {
	observedAt := EpochMillis()
	if observedAt < 0 {
		observedAt = 0
	}
	return WorkFacts{
		ObservedAtMs: uint64(observedAt),
		Processes:    ObservedUnavailable[ProcessFacts](reason),
		Worktree:     ObservedUnavailable[WorktreeFacts](reason),
	}
}

// RenderLines is the observation lines of a report, without the label that
// says who observed them. Free of wall-clock times so renderings are stable.
func (f *WorkFacts) RenderLines() []string {
	var lines []string
	if f.Processes.Value != nil {
		lines = append(lines, renderProcesses(f.Processes.Value)...)
	} else {
		lines = append(lines, fmt.Sprintf("Running processes: unavailable (%s)", f.Processes.Reason))
	}
	return append(lines, RenderWorktreePart(&f.Worktree)...)
}

// RunningProcesses is the number of running processes, when they were observed.
func (f *WorkFacts) RunningProcesses() (uint32, bool) {
	if f.Processes.Value == nil {
		return 0, false
	}
	return f.Processes.Value.Running, true
}

// RenderWorktreePart is the working-tree lines of a report, or the reason
// they are missing.
func RenderWorktreePart(worktree *Observed[WorktreeFacts]) []string {
	if worktree.Value != nil {
		return renderWorktree(worktree.Value)
	}
	return []string{fmt.Sprintf("Working tree: unavailable (%s)", worktree.Reason)}
}

// RenderElapsed renders 4m12s, 1h02m, 35s.
func RenderElapsed(secs uint64) string {
	switch {
	case secs >= 3600:
		return fmt.Sprintf("%dh%02dm", secs/3600, secs%3600/60)
	case secs >= 60:
		return fmt.Sprintf("%dm%02ds", secs/60, secs%60)
	}
	return fmt.Sprintf("%ds", secs)
}

// RenderProcess renders "pid 41022, up 4m12s, detached from the harness: command".
func RenderProcess(process *RunningProcess) string {
	detached := ""
	if process.Detached {
		detached = ", detached from the harness"
	}
	return fmt.Sprintf("pid %d, up %s%s: %s", process.Pid, RenderElapsed(process.ElapsedSecs), detached, process.Command)
}

// processScope is what the process scan looks at. A process outside it (a
// job started with env -i setsid whose launching shell has exited, say) is
// not seen, so the lines say what was searched and an empty result says
// none were found there, not that none are running.
const processScope = "in the worker's process tree or carrying this session's marker"

func renderProcesses(processes *ProcessFacts) []string {
	var lines []string
	if processes.Running == 0 {
		lines = []string{fmt.Sprintf("No running processes found %s", processScope)}
	} else {
		lines = []string{fmt.Sprintf("Running processes found %s: %d", processScope, processes.Running)}
		for i := range processes.Shown {
			lines = append(lines, "  "+RenderProcess(&processes.Shown[i]))
		}
		if processes.Omitted > 0 {
			lines = append(lines, fmt.Sprintf("  and %d more", processes.Omitted))
		}
	}
	if processes.Unattributed > 0 {
		lines = append(lines, fmt.Sprintf("  %d other processes of this user could not be attributed to this session (their environment is unreadable)", processes.Unattributed))
	}
	return lines
}

// displayText is text from the machine as a reader sees it: terminal
// controls removed and a long value cut. Paths, branch names and subjects
// are stored exactly as Git reported them; only this rendering changes them.
func displayText(text string) string {
	clean := SanitizeTerminalText(text)
	clean = strings.NewReplacer("\n", " ", "\t", " ").Replace(clean)
	if utf8.RuneCountInString(clean) <= CommandChars {
		return clean
	}
	return string([]rune(clean)[:CommandChars]) + "…"
}

func renderWorktree(tree *WorktreeFacts) []string {
	var head string
	if tree.Head != nil {
		short := tree.Head.Sha
		if runes := []rune(short); len(runes) > 7 {
			short = string(runes[:7])
		}
		if tree.Head.Subject.Value != nil {
			head = fmt.Sprintf("HEAD %s %s", short, debugString(displayText(*tree.Head.Subject.Value)))
		} else {
			head = fmt.Sprintf("HEAD %s (subject unavailable: %s)", short, displayText(tree.Head.Subject.Reason))
		}
	} else {
		head = "no commits yet"
	}
	branch := "detached HEAD"
	if tree.Branch != nil {
		branch = "branch " + displayText(*tree.Branch)
	}
	lines := []string{fmt.Sprintf("Working tree %s: %s; %s", displayText(tree.Directory), head, branch)}
	if tree.Changed == 0 {
		return append(lines, "Uncommitted changes: none")
	}
	plural := "paths"
	if tree.Changed == 1 {
		plural = "path"
	}
	var kinds []string
	if tree.Modified > 0 {
		kinds = append(kinds, fmt.Sprintf("%d modified", tree.Modified))
	}
	if tree.Untracked > 0 {
		kinds = append(kinds, fmt.Sprintf("%d untracked", tree.Untracked))
	}
	counted := uint64(tree.Modified) + uint64(tree.Untracked)
	if other := uint64(tree.Changed); other > counted {
		kinds = append(kinds, fmt.Sprintf("%d other", other-counted))
	}
	lines = append(lines, fmt.Sprintf("Uncommitted changes: %d %s (%s)", tree.Changed, plural, strings.Join(kinds, ", ")))
	for _, path := range tree.Shown {
		lines = append(lines, fmt.Sprintf("  %s %s", path.Status, displayText(path.Path)))
	}
	if tree.Omitted > 0 {
		lines = append(lines, fmt.Sprintf("  and %d more", tree.Omitted))
	}
	return lines
}

// MarshalJSON writes absent lists as empty arrays, as serde writes a Vec.
func (f ProcessFacts) MarshalJSON() ([]byte, error) {
	type plain ProcessFacts
	if f.Shown == nil {
		f.Shown = []RunningProcess{}
	}
	if f.Identities == nil {
		f.Identities = []ProcessIdentity{}
	}
	return json.Marshal(plain(f))
}

// MarshalJSON writes an absent list as an empty array, as serde writes a Vec.
func (f WorktreeFacts) MarshalJSON() ([]byte, error) {
	type plain WorktreeFacts
	if f.Shown == nil {
		f.Shown = []ChangedPath{}
	}
	return json.Marshal(plain(f))
}
