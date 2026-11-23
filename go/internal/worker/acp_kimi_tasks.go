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
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
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
	current, err := fileStamp(f.wirePath)
	if err != nil {
		return nil, err
	}
	if f.stamp != nil {
		if current.requiresRescan(f.stamp, f.offset) {
			return KimiWireRefreshRescanRequired{}, nil
		}
	} else {
		if _, err := f.Rescan(); err != nil {
			return nil, err
		}
		return KimiWireRefreshUpdated{Value: f.Snapshot()}, nil
	}

	file, err := openWireFile(f.wirePath)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	metadata, err := file.Stat()
	if err != nil {
		return nil, err
	}
	opened, err := fileStampFromMetadata(f.wirePath, metadata)
	if err != nil {
		return nil, err
	}
	if opened.requiresRescan(&current, f.offset) || opened.requiresRescan(f.stamp, f.offset) {
		return KimiWireRefreshRescanRequired{}, nil
	}
	if _, err := file.Seek(int64(f.offset), io.SeekStart); err != nil {
		return nil, fmt.Errorf("seek Kimi wire stream %s: %w", f.wirePath, err)
	}
	appended, err := io.ReadAll(file)
	if err != nil {
		return nil, fmt.Errorf("read Kimi wire stream %s: %w", f.wirePath, err)
	}

	candidate := f.tracker.clone()
	candidatePartial := slices.Clone(f.partial)
	candidateLineNumber := f.nextLineNumber
	if err := parseAppend(&candidate, &candidatePartial, &candidateLineNumber, appended, f.wirePath); err != nil {
		return nil, err
	}

	offset := f.offset + uint64(len(appended))
	if offset < f.offset {
		return nil, errors.New("Kimi wire stream offset overflow")
	}
	updatedStamp, err := fileStamp(f.wirePath)
	if err != nil {
		return nil, err
	}
	// The writer can append while the read is in progress. Keeping the
	// current offset, rather than the metadata length, lets the next refresh
	// catch up with those bytes.
	f.tracker = candidate
	f.partial = candidatePartial
	f.nextLineNumber = candidateLineNumber
	f.offset = offset
	f.stamp = &updatedStamp
	return KimiWireRefreshUpdated{Value: f.Snapshot()}, nil
}

// Rescan discards incremental state and parses the complete stream again.
func (f *KimiWireFollower) Rescan() (KimiTaskSnapshot, error) {
	bytes, err := os.ReadFile(f.wirePath)
	if err != nil {
		return KimiTaskSnapshot{}, fmt.Errorf("read Kimi wire stream for full scan %s: %w", f.wirePath, err)
	}
	var tracker TaskTracker
	var partial []byte
	var lineCount uint64
	if err := parseAppend(&tracker, &partial, &lineCount, bytes, f.wirePath); err != nil {
		return KimiTaskSnapshot{}, err
	}
	stamp, err := fileStamp(f.wirePath)
	if err != nil {
		return KimiTaskSnapshot{}, err
	}
	f.tracker = tracker
	f.nextLineNumber = lineCount
	f.partial = partial
	f.offset = uint64(len(bytes))
	f.stamp = &stamp
	return f.Snapshot(), nil
}

// ResolveSessionDir resolves the final index entry for sessionID, then
// validates its native directory and state identity before returning its
// canonical path. (acp.rs re-exports it as resolve_kimi_session_dir.)
func ResolveSessionDir(kimiHome string, sessionID string) (string, error) {
	if sessionID == "" {
		return "", errors.New("Kimi session id must not be empty")
	}
	canonicalHome, err := kimiCanonicalize(kimiHome)
	if err != nil {
		return "", fmt.Errorf("canonicalize Kimi home while resolving session %s: %s: %w",
			sessionConfigDebugString(sessionID), kimiHome, err)
	}
	if info, err := os.Stat(canonicalHome); err != nil || !info.IsDir() {
		return "", errors.New("Kimi home is not a directory")
	}

	indexPath := filepath.Join(canonicalHome, sessionIndexFile)
	index, err := os.ReadFile(indexPath)
	if err != nil {
		return "", fmt.Errorf("read Kimi session index %s: %w", indexPath, err)
	}
	var selectedSessionDir *string
	lines, err := completeLines(index, indexPath)
	if err != nil {
		return "", err
	}
	for _, line := range lines {
		if kimiAllASCIIWhitespace(line.line) {
			continue
		}
		entry, err := jsonvalue.Parse(line.line)
		if err != nil {
			return "", fmt.Errorf("parse Kimi session index %s line %d: %w", indexPath, line.number+1, err)
		}
		object, ok := entry.(jsonvalue.Object)
		if !ok {
			return "", fmt.Errorf("Kimi session index %s line %d is not an object", indexPath, line.number+1)
		}
		if id, isText := acpFeaturesString(object, "sessionId"); !isText || id != sessionID {
			continue
		}
		sessionDir, ok := acpFeaturesString(object, "sessionDir")
		if !ok {
			return "", fmt.Errorf("Kimi session index %s line %d matching %s lacks string sessionDir",
				indexPath, line.number+1, sessionConfigDebugString(sessionID))
		}
		selectedSessionDir = &sessionDir
	}

	if selectedSessionDir == nil {
		return "", fmt.Errorf("Kimi session index %s has no session %s", indexPath, sessionConfigDebugString(sessionID))
	}
	sessionDir := *selectedSessionDir
	if !filepath.IsAbs(sessionDir) {
		return "", fmt.Errorf("Kimi session directory is not absolute: %s", sessionDir)
	}
	canonicalSession, err := kimiCanonicalize(sessionDir)
	if err != nil {
		return "", fmt.Errorf("canonicalize Kimi session directory %s: %w", sessionDir, err)
	}
	if canonicalSession == canonicalHome || !kimiPathStartsWith(canonicalSession, canonicalHome) {
		return "", fmt.Errorf("Kimi session directory escapes Kimi home: %s", canonicalSession)
	}
	if info, err := os.Stat(canonicalSession); err != nil || !info.IsDir() {
		return "", fmt.Errorf("Kimi session directory is not a directory: %s", canonicalSession)
	}

	statePath := filepath.Join(canonicalSession, stateFile)
	canonicalState, err := kimiCanonicalize(statePath)
	if err != nil {
		return "", fmt.Errorf("canonicalize Kimi session state %s: %w", statePath, err)
	}
	if !kimiPathStartsWith(canonicalState, canonicalSession) {
		return "", fmt.Errorf("Kimi session state escapes session directory: %s", canonicalState)
	}
	stateBytes, err := os.ReadFile(canonicalState)
	if err != nil {
		return "", fmt.Errorf("read Kimi session state %s: %w", canonicalState, err)
	}
	state, err := jsonvalue.Parse(stateBytes)
	if err != nil {
		return "", fmt.Errorf("parse Kimi session state %s: %w", canonicalState, err)
	}
	idValue, ok := acpFeaturesGet(state, "id")
	if !ok {
		idValue, ok = acpFeaturesGet(state, "sessionId")
	}
	stateID, isText := idValue.(string)
	if !ok || !isText {
		return "", fmt.Errorf("Kimi session state %s lacks string id", canonicalState)
	}
	if stateID != sessionID {
		return "", fmt.Errorf("Kimi session state id %s does not match requested session %s",
			sessionConfigDebugString(stateID), sessionConfigDebugString(sessionID))
	}
	return canonicalSession, nil
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
	clone := TaskTracker{
		nativeAgents:    maps.Clone(t.nativeAgents),
		active:          maps.Clone(t.active),
		providerToolIDs: maps.Clone(t.providerToolIDs),
		observedTaskIDs: maps.Clone(t.observedTaskIDs),
	}
	if t.turn != nil {
		turn := *t.turn
		turn.usage = maps.Clone(t.turn.usage)
		if t.turn.diagnostic != nil {
			diagnostic := *t.turn.diagnostic
			turn.diagnostic = &diagnostic
		}
		clone.turn = &turn
	}
	return clone
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
	var total NativeUsage
	modelUsage := map[string]core.TokenUsage{}
	models := slices.Sorted(maps.Keys(byModel))
	for _, model := range models {
		usage := byModel[model]
		total.add(usage)
		modelUsage[model] = usage.tokenUsage()
	}
	usage := total.tokenUsage()
	if unreadable {
		usage.Scope = core.UsageScopeUnspecified
		for model, tokens := range modelUsage {
			tokens.Scope = core.UsageScopeUnspecified
			modelUsage[model] = tokens
		}
	}
	usage.ProviderDetails = &core.ProviderTurnUsage{ModelUsage: modelUsage}
	return usage
}

// parseUsageRecord reads one `usage.record` payload; ok is false when this
// build cannot read it.
func parseUsageRecord(record any) (model string, usage NativeUsage, ok bool) {
	model, ok = acpFeaturesString(record, "model")
	if !ok {
		return "", NativeUsage{}, false
	}
	value, _ := acpFeaturesGet(record, "usage")
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return "", NativeUsage{}, false
	}
	var counters NativeUsage
	for _, field := range []struct {
		name    string
		counter *uint64
	}{
		{"inputOther", &counters.inputOther},
		{"output", &counters.output},
		{"inputCacheRead", &counters.cacheRead},
		{"inputCacheCreation", &counters.cacheCreation},
	} {
		value, present := object.Get(field.name)
		if !present || value == nil {
			*field.counter = 0
			continue
		}
		number, ok := acpFeaturesAsU64(value)
		if !ok {
			return "", NativeUsage{}, false
		}
		*field.counter = number
	}
	return model, counters, true
}

func (t *TaskTracker) snapshot() KimiTaskSnapshot {
	tasks := []KimiBackgroundTask{}
	for _, id := range slices.Sorted(maps.Keys(t.active)) {
		tasks = append(tasks, t.active[id])
	}
	snapshot := KimiTaskSnapshot{
		NativeAgents:    maps.Clone(t.nativeAgents),
		Tasks:           tasks,
		ProviderToolIDs: maps.Clone(t.providerToolIDs),
		ObservedTaskIDs: maps.Clone(t.observedTaskIDs),
	}
	if snapshot.NativeAgents == nil {
		snapshot.NativeAgents = map[string]KimiNativeAgent{}
	}
	if snapshot.ProviderToolIDs == nil {
		snapshot.ProviderToolIDs = map[string]struct{}{}
	}
	if snapshot.ObservedTaskIDs == nil {
		snapshot.ObservedTaskIDs = map[string]struct{}{}
	}
	return snapshot
}

func (t *TaskTracker) apply(event TaskEvent) {
	if t.nativeAgents == nil {
		t.nativeAgents = map[string]KimiNativeAgent{}
	}
	if t.active == nil {
		t.active = map[string]KimiBackgroundTask{}
	}
	if t.providerToolIDs == nil {
		t.providerToolIDs = map[string]struct{}{}
	}
	if t.observedTaskIDs == nil {
		t.observedTaskIDs = map[string]struct{}{}
	}
	if event.parentToolCallID != nil {
		t.providerToolIDs[*event.parentToolCallID] = struct{}{}
	}
	switch kind := event.kind.(type) {
	case TaskEventKindStarted:
		task := kind.Value
		t.observedTaskIDs[task.TaskID] = struct{}{}
		if task.IsAgent {
			t.nativeAgents[task.TaskID] = KimiNativeAgent{Task: task, State: core.NativeAgentStateRunning}
		}
		t.active[task.TaskID] = task
	case TaskEventKindTerminated:
		if agent, ok := t.nativeAgents[kind.TaskID]; ok {
			agent.State = kind.State
			t.nativeAgents[kind.TaskID] = agent
		}
		t.observedTaskIDs[kind.TaskID] = struct{}{}
		delete(t.active, kind.TaskID)
	}
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
	metadata, err := os.Lstat(path)
	if err != nil {
		return nil, fmt.Errorf("stat Kimi wire stream %s: %w", path, err)
	}
	if err := ensureRegularFile(metadata, path, "Kimi wire stream"); err != nil {
		return nil, err
	}
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open Kimi wire stream %s: %w", path, err)
	}
	return file, nil
}

func ensureRegularFile(metadata os.FileInfo, path string, label string) error {
	if metadata.Mode()&os.ModeSymlink != 0 || !metadata.Mode().IsRegular() {
		return fmt.Errorf("%s is not a regular file: %s", label, path)
	}
	return nil
}

func fileStamp(path string) (FileStamp, error) {
	metadata, err := os.Lstat(path)
	if err != nil {
		return FileStamp{}, fmt.Errorf("stat Kimi wire stream %s: %w", path, err)
	}
	if err := ensureRegularFile(metadata, path, "Kimi wire stream"); err != nil {
		return FileStamp{}, err
	}
	return fileStampFromMetadata(path, metadata)
}

func fileStampFromMetadata(path string, metadata os.FileInfo) (FileStamp, error) {
	file, err := os.Open(path)
	if err != nil {
		return FileStamp{}, fmt.Errorf("open Kimi wire stream for fingerprint %s: %w", path, err)
	}
	defer file.Close()
	prefix := make([]byte, min(fileIDSampleBytes, int(max(metadata.Size(), 0))))
	read := 0
	for read < len(prefix) {
		count, err := file.Read(prefix[read:])
		if count == 0 {
			if err != nil && !errors.Is(err, io.EOF) {
				return FileStamp{}, fmt.Errorf("read Kimi wire stream fingerprint %s: %w", path, err)
			}
			prefix = prefix[:read]
			break
		}
		read += count
	}
	modified := metadata.ModTime()
	device, inode := kimiFileIdentity(metadata)
	return FileStamp{
		len:      uint64(max(metadata.Size(), 0)),
		modified: &modified,
		prefix:   prefix,
		device:   device,
		inode:    inode,
	}, nil
}

// kimiWireLine is one line of a wire stream and its zero-based number (Rust
// `(u64, &[u8])`).
type kimiWireLine struct {
	number uint64
	line   []byte
}

func completeLines(bytes []byte, path string) ([]kimiWireLine, error) {
	var lines []kimiWireLine
	start := 0
	var lineNumber uint64
	for index, b := range bytes {
		if b != '\n' {
			continue
		}
		line := bytes[start:index]
		if err := ensureLineSize(line, path, lineNumber); err != nil {
			return nil, err
		}
		lines = append(lines, kimiWireLine{number: lineNumber, line: trimCR(line)})
		lineNumber++
		start = index + 1
	}
	if start < len(bytes) {
		line := bytes[start:]
		if err := ensureLineSize(line, path, lineNumber); err != nil {
			return nil, err
		}
		lines = append(lines, kimiWireLine{number: lineNumber, line: trimCR(line)})
	}
	return lines, nil
}

func parseAppend(tracker *TaskTracker, partial *[]byte, nextLineNumber *uint64, appended []byte, path string) error {
	*partial = append(*partial, appended...)
	start := 0
	lineNumber := *nextLineNumber
	for index, b := range *partial {
		if b != '\n' {
			continue
		}
		line := (*partial)[start:index]
		if err := ensureLineSize(line, path, lineNumber); err != nil {
			return err
		}
		if err := parseLine(tracker, trimCR(line), lineNumber, path); err != nil {
			return err
		}
		lineNumber++
		start = index + 1
	}
	if start == 0 {
		if err := ensureLineSize(*partial, path, lineNumber); err != nil {
			return err
		}
	} else {
		tail := slices.Clone((*partial)[start:])
		if err := ensureLineSize(tail, path, lineNumber); err != nil {
			return err
		}
		*partial = tail
	}
	*nextLineNumber = lineNumber
	return nil
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
	if kimiAllASCIIWhitespace(line) {
		return nil
	}
	record, err := jsonvalue.Parse(line)
	if err != nil {
		return fmt.Errorf("parse Kimi wire stream %s line %d: %w", path, lineNumber+1, err)
	}
	eventType, ok := acpFeaturesString(record, "type")
	if !ok {
		return nil
	}
	// Kimi Code 2.0.2 opens a main-agent turn in its journal with
	// `turn.prompt`: its `turn.started` event is observable only and never
	// written to `wire.jsonl` (#1132). `turn.started` is still read in case a
	// build journals it.
	if eventType == "turn.prompt" || eventType == "turn.started" || eventType == "turn.ended" {
		// The main agent's records say `agentId: "main"`; an earlier Kimi
		// wrote no `agentId` into the main agent's own stream at all.
		if agent, present := acpFeaturesGet(record, "agentId"); present {
			if text, isText := agent.(string); !isText || text != "main" {
				return nil
			}
		}
		// An earlier Kimi wrote no `turnId` on the prompt record either.
		var id any
		if value, present := acpFeaturesGet(record, "turnId"); present {
			if _, isU64 := acpFeaturesAsU64(value); isU64 {
				id = value
			} else if _, isText := value.(string); isText {
				id = value
			}
		}
		if eventType != "turn.ended" {
			// A second opener of the turn already open adds nothing.
			if id != nil && tracker.turn != nil && !tracker.turn.ended && jsonvalue.Equal(tracker.turn.id, id) {
				return nil
			}
			// An opener always invalidates the preceding failure, even if
			// malformed.
			tracker.turn = nil
			if value, present := acpFeaturesGet(record, "time"); present {
				if time, ok := acpFeaturesAsI64(value); ok {
					tracker.turn = &NativeTurn{
						id:          id,
						startedAtMs: time,
						usage:       map[string]NativeUsage{},
					}
				}
			}
		} else if turn := tracker.turn; turn != nil && (turn.id == nil || id == nil || jsonvalue.Equal(turn.id, id)) {
			turn.ended = true
			turn.diagnostic = nil
			if reason, _ := acpFeaturesString(record, "reason"); reason == "failed" {
				if failure, present := acpFeaturesGet(record, "error"); present {
					turn.diagnostic = core.TurnDiagnosticFromProvider(failure)
				}
			}
		}
		return nil
	}
	if eventType == usageRecord {
		// The main agent's own stream; a record written for another agent, or
		// one Kimi scoped to the session rather than the turn, is not this
		// turn's consumption.
		agentID, isText := acpFeaturesString(record, "agentId")
		scope, _ := acpFeaturesString(record, "usageScope")
		if (isText && agentID != "main") || scope != "turn" {
			return nil
		}
		// A malformed counter must not fail the follower: background-task
		// tracking blocks worker replacement, and losing that over an
		// accounting field would be a worse outcome than an unreadable total.
		if turn := tracker.turn; turn != nil && !turn.ended {
			if model, usage, ok := parseUsageRecord(record); ok {
				if turn.usage == nil {
					turn.usage = map[string]NativeUsage{}
				}
				total := turn.usage[model]
				total.add(usage)
				turn.usage[model] = total
			} else {
				turn.usageUnreadable = true
			}
		}
		return nil
	}
	eventKind, ok := taskEventKind(eventType)
	if !ok {
		return nil
	}
	event, err := parseTaskEvent(record, eventKind)
	if err != nil {
		return fmt.Errorf("parse Kimi task lifecycle record in %s line %d: %w", path, lineNumber+1, err)
	}
	if event != nil {
		tracker.apply(*event)
	}
	return nil
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
	recordAgent, recordHasAgent := acpFeaturesGet(record, "agentId")
	if recordHasAgent {
		agentID, ok := recordAgent.(string)
		if !ok {
			return nil, errors.New("lifecycle record agentId is not a string")
		}
		if agentID != "main" {
			return nil, nil
		}
	}
	infoValue, _ := acpFeaturesGet(record, "info")
	info, ok := infoValue.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("lifecycle record lacks object info")
	}
	if infoAgent, present := info.Get("agentId"); present {
		if _, ok := infoAgent.(string); !ok {
			return nil, errors.New("lifecycle record info.agentId is not a string")
		}
	} else if !recordHasAgent {
		return nil, errors.New("lifecycle record lacks agentId")
	}

	// Kimi journals detached agents and detached shell processes the same
	// way; both are background work a turn is no longer waiting on.
	if kindValue, present := info.Get("kind"); present {
		kind, ok := kindValue.(string)
		if !ok {
			return nil, errors.New("lifecycle record kind is not a string")
		}
		if kind != "agent" && kind != "process" {
			return nil, nil
		}
	} else if eventKind == TaskEventTypeStarted {
		return nil, errors.New("task lifecycle record lacks kind")
	}
	if detachedValue, present := info.Get("detached"); present {
		detached, ok := detachedValue.(bool)
		if !ok {
			return nil, errors.New("lifecycle record detached is not a boolean")
		}
		if !detached {
			return nil, nil
		}
	} else if eventKind == TaskEventTypeStarted {
		return nil, errors.New("task lifecycle record lacks detached")
	}

	taskID, ok := acpFeaturesString(info, "taskId")
	if !ok || taskID == "" {
		return nil, errors.New("task lifecycle record lacks non-empty taskId")
	}
	parentToolCallID, err := optionalString(info, "parentToolCallId")
	if err != nil {
		return nil, err
	}
	checkStatus := func() error {
		if status, present := info.Get("status"); present {
			if _, ok := status.(string); !ok {
				return errors.New("task lifecycle record status is not a string")
			}
		}
		return nil
	}
	switch eventKind {
	case TaskEventTypeStarted:
		description, ok := acpFeaturesString(info, "description")
		if !ok {
			return nil, errors.New("task start record lacks string description")
		}
		startedValue, _ := info.Get("startedAt")
		startedAtMs, ok := acpFeaturesAsI64(startedValue)
		if !ok {
			return nil, errors.New("task start record lacks integer startedAt")
		}
		if err := checkStatus(); err != nil {
			return nil, err
		}
		kind, _ := acpFeaturesString(info, "kind")
		var parent *string
		if parentToolCallID != nil {
			copied := *parentToolCallID
			parent = &copied
		}
		return &TaskEvent{
			kind: TaskEventKindStarted{Value: KimiBackgroundTask{
				IsAgent:          kind == "agent",
				TaskID:           taskID,
				Description:      description,
				StartedAtMs:      startedAtMs,
				ParentToolCallID: parent,
			}},
			parentToolCallID: parentToolCallID,
		}, nil
	default:
		if err := checkStatus(); err != nil {
			return nil, err
		}
		state := core.NativeAgentStateDisconnected
		switch status, _ := acpFeaturesString(info, "status"); status {
		case "completed", "done":
			state = core.NativeAgentStateCompleted
		case "failed", "error":
			state = core.NativeAgentStateFailed
		case "cancelled", "canceled", "stopped":
			state = core.NativeAgentStateCancelled
		}
		return &TaskEvent{
			kind:             TaskEventKindTerminated{TaskID: taskID, State: state},
			parentToolCallID: parentToolCallID,
		}, nil
	}
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

// kimiAllASCIIWhitespace is `line.iter().all(u8::is_ascii_whitespace)`.
func kimiAllASCIIWhitespace(line []byte) bool {
	for _, b := range line {
		switch b {
		case ' ', '\t', '\n', '\x0c', '\r':
		default:
			return false
		}
	}
	return true
}

// kimiCanonicalize is Rust's std::fs::canonicalize: absolute, with every
// link resolved, failing when the path does not exist.
func kimiCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// kimiPathStartsWith is Rust's Path::starts_with for two canonical paths:
// whether base is a component prefix of path.
func kimiPathStartsWith(path, base string) bool {
	relative, err := filepath.Rel(base, path)
	if err != nil {
		return false
	}
	return relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator)) && !filepath.IsAbs(relative)
}
