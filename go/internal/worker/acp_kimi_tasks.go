// Port of mj-worker/src/acp/kimi_tasks.rs.

// Native Kimi background-task tracking.
//
// Kimi does not report its detached background work through ACP. The
// durable task lifecycle records in the native session's main wire stream
// are the source of truth instead. Both detached agents (`kind: "agent"`)
// and detached shell processes (`kind: "process"`) are tracked. This module
// deliberately contains only file parsing and state tracking; the caller
// decides where a snapshot is published.

package worker

import (
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	sessionIndexFile  = "session_index.jsonl"
	stateFile         = "state.json"
	maxWireLineBytes  = 1024 * 1024
	fileIDSampleBytes = 4096

	usageRecord          = "usage.record"
	taskStarted          = "task.started"
	taskTerminated       = "task.terminated"
	legacyTaskStarted    = "background.task.started"
	legacyTaskTerminated = "background.task.terminated"
)

// KimiBackgroundTask is a detached Kimi agent or process which is active
// according to the durable wire log.
type KimiBackgroundTask struct {
	IsAgent          bool    `json:"is_agent"`
	TaskID           string  `json:"task_id"`
	Description      string  `json:"description"`
	StartedAtMs      int64   `json:"started_at_ms"`
	ParentToolCallID *string `json:"parent_tool_call_id"`
}

func (t KimiBackgroundTask) MarshalJSON() ([]byte, error) {
	type plain KimiBackgroundTask
	return acpFeaturesMarshalJSON(plain(t))
}

func (t *KimiBackgroundTask) UnmarshalJSON(data []byte) error {
	if err := acpFeaturesRequireFields(data, "task_id", "description", "started_at_ms"); err != nil {
		return err
	}
	type plain KimiBackgroundTask
	return json.Unmarshal(data, (*plain)(t))
}

// KimiNativeAgent is a detached Kimi agent and its native state (Rust
// `(KimiBackgroundTask, NativeAgentState)`).
type KimiNativeAgent struct {
	Task  KimiBackgroundTask
	State core.NativeAgentState
}

// KimiTaskSnapshot is the materialized native task level and provider IDs
// observed in its event history. ProviderToolIDs intentionally retains IDs
// after termination: a relay may see the ACP tool call before it catches up
// with this stream.
type KimiTaskSnapshot struct {
	NativeAgents    map[string]KimiNativeAgent
	Tasks           []KimiBackgroundTask
	ProviderToolIDs map[string]struct{}
	// ObservedTaskIDs are native task identities, retained after termination
	// to reconcile late ACP queries.
	ObservedTaskIDs map[string]struct{}
}

func (s *KimiTaskSnapshot) IsEmpty() bool {
	return len(s.Tasks) == 0
}

func (s *KimiTaskSnapshot) Task(taskID string) *KimiBackgroundTask {
	for i := range s.Tasks {
		if s.Tasks[i].TaskID == taskID {
			return &s.Tasks[i]
		}
	}
	return nil
}

// KimiWireRefresh is the result of an append catch-up. A replacement or
// truncation invalidates the incremental offset and previously materialized
// level; the caller should invoke KimiWireFollower.Rescan before accepting
// another update.
type KimiWireRefresh interface {
	isKimiWireRefresh()
}

type KimiWireRefreshUpdated struct{ Value KimiTaskSnapshot }

type KimiWireRefreshRescanRequired struct{}

func (KimiWireRefreshUpdated) isKimiWireRefresh()        {}
func (KimiWireRefreshRescanRequired) isKimiWireRefresh() {}

// KimiWireFollower incrementally follows one Kimi main-agent wire stream.
type KimiWireFollower struct {
	wirePath       string
	offset         uint64
	nextLineNumber uint64
	partial        []byte
	tracker        TaskTracker
	stamp          *FileStamp
}

// NewKimiWireFollower constructs an unopened follower. The first Refresh
// performs an initial scan; KimiWireFollowerOpen is convenient when the
// caller wants that initial scan to be explicit. A live trailing line
// without a newline is retained as partial input.
func NewKimiWireFollower(wirePath string) *KimiWireFollower {
	return &KimiWireFollower{wirePath: wirePath}
}

// KimiWireFollowerOpen opens and fully scans a wire stream into a follower.
func KimiWireFollowerOpen(wirePath string) (*KimiWireFollower, error) {
	follower := NewKimiWireFollower(wirePath)
	if _, err := follower.Rescan(); err != nil {
		return nil, err
	}
	return follower, nil
}

func (f *KimiWireFollower) WirePath() string {
	return f.wirePath
}

// Snapshot returns the latest materialized level without touching the
// filesystem.
func (f *KimiWireFollower) Snapshot() KimiTaskSnapshot {
	return f.tracker.snapshot()
}

// TurnDiagnosticSince is the current main-agent turn's failure only; older
// failures cannot describe a new prompt.
func (f *KimiWireFollower) TurnDiagnosticSince(startedAtMs int64) *core.TurnDiagnostic {
	turn := f.turnSince(startedAtMs)
	if turn == nil || turn.diagnostic == nil {
		return nil
	}
	diagnostic := *turn.diagnostic
	return &diagnostic
}

// TurnUsageSince is the token consumption Kimi journalled for the current
// main-agent turn.
//
// Kimi's ACP server never puts a `usage` on its prompt response, so the
// durable wire records are the only place a turn's consumption is stated
// (#1064). One `usage.record` is written per model request; the ones scoped
// to the turn are exactly what Kimi itself sums into its own "current turn"
// figure. Session-scoped records (compaction, title generation) and work a
// detached background agent does in its own wire stream belong to no turn
// and are left out.
func (f *KimiWireFollower) TurnUsageSince(startedAtMs int64) *core.TokenUsage {
	turn := f.turnSince(startedAtMs)
	if turn == nil || len(turn.usage) == 0 {
		return nil
	}
	usage := turnTokenUsage(turn.usage, turn.usageUnreadable)
	return &usage
}

// TurnSettledSince reports whether Kimi has written the end of the current
// main-agent turn.
//
// known is false when no journalled turn belongs to a prompt started at or
// after startedAtMs, so a caller can tell "not written yet" from "this wire
// stream does not journal turns at all".
func (f *KimiWireFollower) TurnSettledSince(startedAtMs int64) (settled bool, known bool) {
	turn := f.turnSince(startedAtMs)
	if turn == nil {
		return false, false
	}
	return turn.ended, true
}

func (f *KimiWireFollower) turnSince(startedAtMs int64) *NativeTurn {
	if f.tracker.turn == nil || f.tracker.turn.startedAtMs < startedAtMs {
		return nil
	}
	return f.tracker.turn
}

// Refresh reads complete appended lines, retaining a trailing partial line.
//
// A malformed complete line is an error. The follower commits no state from
// that refresh when parsing fails, so a caller can fail closed.
func (f *KimiWireFollower) Refresh() (KimiWireRefresh, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs KimiWireFollower::refresh")
}

// Rescan discards incremental state and parses the complete stream again.
func (f *KimiWireFollower) Rescan() (KimiTaskSnapshot, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs KimiWireFollower::rescan")
}

// ResolveSessionDir resolves the final index entry for sessionID, then
// validates its native directory and state identity before returning its
// canonical path. (acp.rs re-exports it as resolve_kimi_session_dir.)
func ResolveSessionDir(kimiHome string, sessionID string) (string, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs resolve_session_dir")
}

type TaskTracker struct {
	nativeAgents map[string]KimiNativeAgent
	turn         *NativeTurn
	// active is ordered by task id when snapshotted (Rust BTreeMap).
	active          map[string]KimiBackgroundTask
	providerToolIDs map[string]struct{}
	observedTaskIDs map[string]struct{}
}

// clone is a deep copy, so a failed refresh commits nothing.
func (t *TaskTracker) clone() TaskTracker {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs TaskTracker::clone")
}

type NativeTurn struct {
	// id is the turn's `turnId`, when the record that opened it had one: a
	// JSON integer or string, nil when absent.
	id          any
	startedAtMs int64
	ended       bool
	diagnostic  *core.TurnDiagnostic
	// usage is the turn-scoped consumption per model, as Kimi journalled it.
	usage map[string]NativeUsage
	// usageUnreadable: a usage record this build cannot read; the turn total
	// is then a floor, not a whole-turn report, and must not enter session
	// totals.
	usageUnreadable bool
}

// NativeUsage is one Kimi `usage.record` payload.
type NativeUsage struct {
	inputOther    uint64
	output        uint64
	cacheRead     uint64
	cacheCreation uint64
}

func (u *NativeUsage) add(other NativeUsage) {
	u.inputOther = kimiSaturatingAdd(u.inputOther, other.inputOther)
	u.output = kimiSaturatingAdd(u.output, other.output)
	u.cacheRead = kimiSaturatingAdd(u.cacheRead, other.cacheRead)
	u.cacheCreation = kimiSaturatingAdd(u.cacheCreation, other.cacheCreation)
}

// tokenUsage converts the payload: Kimi counts cache reads and cache writes
// outside `inputOther`; every other harness reports one input figure that
// contains them.
func (u NativeUsage) tokenUsage() core.TokenUsage {
	input := kimiSaturatingAdd(kimiSaturatingAdd(u.inputOther, u.cacheRead), u.cacheCreation)
	cacheRead, cacheCreation := u.cacheRead, u.cacheCreation
	return core.TokenUsage{
		Scope:        core.UsageScopeTurn,
		TotalTokens:  kimiSaturatingAdd(input, u.output),
		InputTokens:  input,
		OutputTokens: u.output,
		// Kimi counts thinking inside its output tokens and reports no
		// separate figure.
		CachedReadTokens:  &cacheRead,
		CachedWriteTokens: &cacheCreation,
	}
}

// kimiSaturatingAdd is u64::saturating_add.
func kimiSaturatingAdd(a, b uint64) uint64 {
	if a+b < a {
		return ^uint64(0)
	}
	return a + b
}

func turnTokenUsage(byModel map[string]NativeUsage, unreadable bool) core.TokenUsage {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs turn_token_usage")
}

// parseUsageRecord reads one `usage.record` payload; ok is false when this
// build cannot read it.
func parseUsageRecord(record any) (model string, usage NativeUsage, ok bool) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs parse_usage_record")
}

func (t *TaskTracker) snapshot() KimiTaskSnapshot {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs TaskTracker::snapshot")
}

func (t *TaskTracker) apply(event TaskEvent) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs TaskTracker::apply")
}

type TaskEvent struct {
	kind             TaskEventKind
	parentToolCallID *string
}

type TaskEventKind interface {
	isTaskEventKind()
}

type TaskEventKindStarted struct{ Value KimiBackgroundTask }

type TaskEventKindTerminated struct {
	TaskID string
	State  core.NativeAgentState
}

func (TaskEventKindStarted) isTaskEventKind()    {}
func (TaskEventKindTerminated) isTaskEventKind() {}

type FileStamp struct {
	len uint64
	// modified is nil where the platform reports no modification time.
	modified *time.Time
	prefix   []byte
	// device and inode are the file identity, compared only where
	// kimiFileIdentityKnown (Rust reads them on Unix only).
	device uint64
	inode  uint64
}

func (s *FileStamp) requiresRescan(previous *FileStamp, offset uint64) bool {
	if s.len < offset {
		return true
	}
	if kimiFileIdentityKnown && (s.device != previous.device || s.inode != previous.inode) {
		return true
	}
	common := min(len(s.prefix), len(previous.prefix))
	if string(s.prefix[:common]) != string(previous.prefix[:common]) {
		return true
	}
	// On platforms without a file identity API, a same-length rewrite is
	// still detectable when its modification time changes.
	return s.len == offset && !kimiSameTime(s.modified, previous.modified)
}

// kimiSameTime compares two optional modification times.
func kimiSameTime(a, b *time.Time) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return a.Equal(*b)
}

func openWireFile(path string) (*os.File, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs open_wire_file")
}

func ensureRegularFile(metadata os.FileInfo, path string, label string) error {
	if metadata.Mode()&os.ModeSymlink != 0 || !metadata.Mode().IsRegular() {
		return fmt.Errorf("%s is not a regular file: %s", label, path)
	}
	return nil
}

func fileStamp(path string) (FileStamp, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs file_stamp")
}

func fileStampFromMetadata(path string, metadata os.FileInfo) (FileStamp, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs file_stamp_from_metadata")
}

// kimiWireLine is one line of a wire stream and its zero-based number (Rust
// `(u64, &[u8])`).
type kimiWireLine struct {
	number uint64
	line   []byte
}

func completeLines(bytes []byte, path string) ([]kimiWireLine, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs complete_lines")
}

func parseAppend(tracker *TaskTracker, partial *[]byte, nextLineNumber *uint64, appended []byte, path string) error {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs parse_append")
}

func ensureLineSize(line []byte, path string, lineNumber uint64) error {
	if len(line) > maxWireLineBytes {
		return fmt.Errorf("Kimi wire stream %s line %d exceeds %d bytes", path, lineNumber+1, maxWireLineBytes)
	}
	return nil
}

func trimCR(line []byte) []byte {
	if len(line) > 0 && line[len(line)-1] == '\r' {
		return line[:len(line)-1]
	}
	return line
}

func parseLine(tracker *TaskTracker, line []byte, lineNumber uint64, path string) error {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs parse_line")
}

type TaskEventType int

const (
	TaskEventTypeStarted TaskEventType = iota
	TaskEventTypeTerminated
)

func taskEventKind(eventType string) (TaskEventType, bool) {
	switch eventType {
	case taskStarted, legacyTaskStarted:
		return TaskEventTypeStarted, true
	case taskTerminated, legacyTaskTerminated:
		return TaskEventTypeTerminated, true
	}
	return 0, false
}

// parseTaskEvent reads one task lifecycle record; nil when the record is not
// a detached main-agent task.
func parseTaskEvent(record any, eventKind TaskEventType) (*TaskEvent, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs parse_task_event")
}

func optionalString(object jsonvalue.Object, field string) (*string, error) {
	value, ok := object.Get(field)
	if !ok || value == nil {
		return nil, nil
	}
	text, ok := value.(string)
	if !ok {
		return nil, fmt.Errorf("lifecycle record %s is not a string", field)
	}
	return &text, nil
}
