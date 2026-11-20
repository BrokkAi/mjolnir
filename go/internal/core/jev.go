// Port of mj-core/src/jev.rs.

// Bounded, local Jev diagnostics. No transcript or activity state is mutated
// here.

package core

import (
	"bufio"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"unicode/utf8"
	"weak"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// RequestBody is the body of a Jev classification request: the one place
// classifier evidence is prepared to leave the machine. The hosted proxy
// receives the evidence alone and adds the questions itself; TypeSafe,
// called directly, receives the evidence beside directQuestions. Either way
// every evidence string has its token-like runs redacted: prompts, replies,
// command lines and provider errors can carry API keys, and the classifier
// has no use for them. The questions are Mjolnir's own text and are sent as
// written. A nil directQuestions means the proxy form.
func RequestBody(evidence any, directQuestions json.RawMessage) json.RawMessage {
	encoded, err := agentMarshalJSON(evidence)
	if err != nil {
		panic("classifier evidence serializes: " + err.Error())
	}
	state, err := jsonvalue.Parse(encoded)
	if err != nil {
		panic("classifier evidence serializes: " + err.Error())
	}
	RedactJSONStrings(&state)
	var body any = state
	if directQuestions != nil {
		body = jsonvalue.Object{
			{Key: "model", Value: "jev-latest"},
			{Key: "state", Value: state},
			{Key: "questions", Value: directQuestions},
		}
	}
	out, err := jsonvalue.Marshal(body)
	if err != nil {
		panic("classifier request serializes: " + err.Error())
	}
	return out
}

// DisabledEnvironment is set to `1` in a worker's launch environment when
// `[jev] enabled = false`. Workers re-exec with a cleared environment and
// may run on another machine, so the switch travels in the launch
// configuration.
const DisabledEnvironment = "MJ_JEV_DISABLED"

// DisabledByEnvironment reports whether this process was launched with Jev
// turned off.
func DisabledByEnvironment() bool {
	value, ok := os.LookupEnv(DisabledEnvironment)
	return ok && value == "1"
}

const jevSegmentBytes uint64 = 8 * 1024 * 1024
const jevSegments = 4

type JevDecision struct {
	Version     uint32          `json:"version"`
	ID          string          `json:"id"`
	SessionID   string          `json:"session_id"`
	Kind        string          `json:"kind"`
	StartedAtMs int64           `json:"started_at_ms"`
	UpdatedAtMs int64           `json:"updated_at_ms"`
	Status      string          `json:"status"`
	Checked     string          `json:"checked"`
	Answer      string          `json:"answer"`
	Action      string          `json:"action"`
	Scope       string          `json:"scope"`
	Owner       string          `json:"owner"`
	Technical   json.RawMessage `json:"technical,omitempty"`
}

// UnmarshalJSON requires every field serde requires, and reads a null
// technical as absent, as serde's Option does.
func (d *JevDecision) UnmarshalJSON(data []byte) error {
	if err := agentRequireFields(data, "version", "id", "session_id", "kind", "started_at_ms",
		"updated_at_ms", "status", "checked", "answer", "action", "scope", "owner"); err != nil {
		return err
	}
	type plain JevDecision
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	if string(decoded.Technical) == "null" {
		decoded.Technical = nil
	}
	*d = JevDecision(decoded)
	return nil
}

type DecisionPage struct {
	Decisions []JevDecision `json:"decisions"`
	Warnings  []string      `json:"warnings"`
}

// MarshalJSON writes nil lists as empty ones, as serde writes a Vec.
func (p DecisionPage) MarshalJSON() ([]byte, error) {
	type plain DecisionPage
	if p.Decisions == nil {
		p.Decisions = []JevDecision{}
	}
	if p.Warnings == nil {
		p.Warnings = []string{}
	}
	return agentMarshalJSON(plain(p))
}

func (p *DecisionPage) Merge(other DecisionPage) {
	p.Decisions = append(p.Decisions, other.Decisions...)
	p.Warnings = append(p.Warnings, other.Warnings...)
	jevSortNewestFirst(p.Decisions)
	if len(p.Decisions) > 100 {
		p.Decisions = p.Decisions[:100]
	}
}

// jevSortNewestFirst orders decisions by (started_at_ms, id) descending.
func jevSortNewestFirst(decisions []JevDecision) {
	slices.SortStableFunc(decisions, func(left, right JevDecision) int {
		if order := cmp.Compare(right.StartedAtMs, left.StartedAtMs); order != 0 {
			return order
		}
		return strings.Compare(right.ID, left.ID)
	})
}

// jevMessage is a request to the diagnostic writer goroutine.
type jevMessage interface {
	isJevMessage()
}

type jevAppendMessage struct {
	decision JevDecision
}

type jevReadMessage struct {
	session string
	id      *string
	reply   chan<- jevReadResult
}

type jevReadResult struct {
	page DecisionPage
	err  error
}

func (jevAppendMessage) isJevMessage() {}
func (jevReadMessage) isJevMessage()   {}

// jevQueue is the writer's unbounded inbox, as Rust's mpsc channel is: an
// append never blocks its caller. It is closed when the last handle on the
// writer goes away, and the writer goroutine then ends.
type jevQueue struct {
	mu       sync.Mutex
	messages []jevMessage
	closed   bool
	wake     chan struct{}
}

func (q *jevQueue) send(message jevMessage) bool {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.closed {
		return false
	}
	q.messages = append(q.messages, message)
	select {
	case q.wake <- struct{}{}:
	default:
	}
	return true
}

func (q *jevQueue) close() {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.closed = true
	select {
	case q.wake <- struct{}{}:
	default:
	}
}

// receive returns the next message, or false once the queue is closed and
// drained.
func (q *jevQueue) receive() (jevMessage, bool) {
	for {
		q.mu.Lock()
		if len(q.messages) > 0 {
			message := q.messages[0]
			q.messages[0] = nil
			q.messages = q.messages[1:]
			q.mu.Unlock()
			return message, true
		}
		closed := q.closed
		q.mu.Unlock()
		if closed {
			return nil, false
		}
		<-q.wake
	}
}

type jevWriter struct {
	queue *jevQueue
}

// DecisionLog appends Jev decisions through one writer per directory.
type DecisionLog struct {
	writer *jevWriter
}

// jevRegistryState maps a log directory to its live writer.
var jevRegistryState = struct {
	sync.Mutex
	writers map[string]weak.Pointer[jevWriter]
}{writers: map[string]weak.Pointer[jevWriter]{}}

// jevOwner names this process in the records it writes.
var jevOwner = sync.OnceValue(func() string {
	return fmt.Sprintf("%d-%d", os.Getpid(), EpochMillis())
})

func ControllerLogDir() string {
	return filepath.Join(DataDir(), "jev-decisions")
}

// DecisionLogOpen starts a dedicated writer. Directory creation and all file
// I/O happen there.
func DecisionLogOpen(directory string) (DecisionLog, error) {
	jevRegistryState.Lock()
	defer jevRegistryState.Unlock()
	if pointer, ok := jevRegistryState.writers[directory]; ok {
		if writer := pointer.Value(); writer != nil {
			return DecisionLog{writer: writer}, nil
		}
	}
	queue := &jevQueue{wake: make(chan struct{}, 1)}
	writer := &jevWriter{queue: queue}
	// Rust's writer thread ends when the last sender drops; here the last
	// handle's collection closes the queue.
	runtime.AddCleanup(writer, func(queue *jevQueue) { queue.close() }, queue)
	go jevServe(directory, queue)
	jevRegistryState.writers[directory] = weak.Make(writer)
	return DecisionLog{writer: writer}, nil
}

// jevServe is the writer goroutine: it owns every file operation on
// directory, so reads are ordered after the writes sent before them.
func jevServe(directory string, queue *jevQueue) {
	var failure *string
	for {
		message, ok := queue.receive()
		if !ok {
			return
		}
		switch message := message.(type) {
		case jevAppendMessage:
			if err := jevAppend(directory, &message.decision, jevSegmentBytes); err != nil {
				slog.Warn("Jev diagnostic write failed", "error", err)
				text := fmt.Sprintf("Jev diagnostic write failed: %v", err)
				failure = &text
			}
		case jevReadMessage:
			page, err := jevReadFiles(directory, message.session, message.id)
			if err == nil && failure != nil {
				page.Warnings = append(page.Warnings, *failure)
			}
			// The reply channel is buffered, so a reader that gave up does
			// not block the writer.
			message.reply <- jevReadResult{page: page, err: err}
		}
	}
}

func (l DecisionLog) append(decision *JevDecision) {
	copied := *decision
	copied.Technical = slices.Clone(decision.Technical)
	if !l.writer.queue.send(jevAppendMessage{decision: copied}) {
		slog.Warn("Jev diagnostic writer stopped", "decision_id", decision.ID)
	}
}

// jevNextAttempt numbers the attempts this process starts.
var jevNextAttempt atomic.Uint64

func (l DecisionLog) Start(session, kind, checked, scope string) Attempt {
	return l.startWithID(session, kind, checked, scope,
		fmt.Sprintf("%s-%020d", jevOwner(), jevNextAttempt.Add(1)))
}

// RecordAssessment publishes the owner's durable state, including later
// admission or supersession. Request tasks must not overwrite this record
// with their process-local outcome.
func (l DecisionLog) RecordAssessment(session string, assessment *TurnAssessment) {
	encodedAssessment, err := agentMarshalJSON(assessment)
	if err != nil {
		panic("assessment serializes: " + err.Error())
	}
	var technical jsonvalue.Object
	if assessment.Glide != nil {
		questions, err := jsonvalue.Parse([]byte(Questions))
		if err != nil {
			panic("bundled GLiDE turn questions are valid JSON: " + err.Error())
		}
		technical = jsonvalue.Object{
			{Key: "contract", Value: "glide-turn-v1"},
			{Key: "assessment", Value: json.RawMessage(encodedAssessment)},
			{Key: "questions", Value: questions},
		}
	} else {
		technical = jsonvalue.Object{
			{Key: "contract", Value: "turn-verdict-v7"},
			{Key: "assessment", Value: json.RawMessage(encodedAssessment)},
			{Key: "automation_threshold", Value: float64(AutomationConfidence)},
			{Key: "required_input_probability", Value: RequiredInputProbability},
			{Key: "required_input_ratio", Value: RequiredInputRatio},
			{Key: "finished_work_probability", Value: FinishedWorkProbability},
		}
	}
	encodedTechnical, err := technical.MarshalJSON()
	if err != nil {
		panic("Jev technical record serializes: " + err.Error())
	}
	var answer string
	switch {
	case assessment.Glide != nil:
		action := "None"
		if assessment.Action != nil {
			action = "Some(" + jevDebugVariant(string(*assessment.Action)) + ")"
		}
		answer = fmt.Sprintf("GLiDE: %s.", action)
	case assessment.Verdict != nil:
		answer = fmt.Sprintf("Failure: %s; input: %s; work: %s.",
			jevDebugVariant(string(assessment.Verdict.Failure.Choice)),
			jevDebugVariant(string(assessment.Verdict.Input.Choice)),
			jevDebugVariant(string(assessment.Verdict.Work.Choice)))
	default:
		answer = "No usable Jev answer yet."
	}
	l.append(&JevDecision{
		Version:     1,
		ID:          fmt.Sprintf("assessment-%s-%d", session, assessment.Revision),
		SessionID:   session,
		Kind:        "assessment",
		StartedAtMs: assessment.CompletedAtMs,
		UpdatedAtMs: EpochMillis(),
		Status:      string(assessment.Status),
		Checked:     "What failed, is input required, and what authorized work remains?",
		Answer:      answer,
		Action:      strings.ReplaceAll(assessment.Reason, "_", " "),
		Scope:       "Whole retained authorization messages and current completion evidence; omitted context is explicit.",
		Owner:       jevOwner(),
		Technical:   encodedTechnical,
	})
}

// jevDebugVariant renders a snake_case serde enum value as Rust's Debug
// prints the variant: `transient_provider` is `TransientProvider`.
func jevDebugVariant(value string) string {
	var builder strings.Builder
	for _, word := range strings.Split(value, "_") {
		if word == "" {
			continue
		}
		first, width := utf8.DecodeRuneInString(word)
		builder.WriteString(strings.ToUpper(string(first)))
		builder.WriteString(word[width:])
	}
	return builder.String()
}

func (l DecisionLog) startWithID(session, kind, checked, scope, id string) Attempt {
	now := EpochMillis()
	record := JevDecision{
		Version:     1,
		ID:          id,
		SessionID:   session,
		Kind:        kind,
		StartedAtMs: now,
		UpdatedAtMs: now,
		Status:      "pending",
		Checked:     checked,
		Scope:       scope,
		Answer:      "Waiting for Jev.",
		Action:      "No action yet.",
		Owner:       jevOwner(),
		Technical:   json.RawMessage("{}"),
	}
	l.append(&record)
	return newAttempt(&attemptState{log: l, record: record})
}

// Attempt is shared across classification and application; the last owner
// records cancellation.
//
// Rust records the cancellation when the last clone drops. Go has no drop, so
// each owner holds its own handle: Clone makes another, and Release gives
// one back. When the last handle is released, an attempt that never
// finished records that it was cancelled. A handle that is never released
// is released when it is garbage collected, so a leaked owner still ends
// the record, only later.
type Attempt struct {
	handle *attemptHandle
}

// attemptHandle is one owner's share of an attempt.
type attemptHandle struct {
	state    *attemptState
	released atomic.Bool
}

type attemptState struct {
	mu       sync.Mutex
	log      DecisionLog
	record   JevDecision
	finished bool
	owners   int
}

func newAttempt(state *attemptState) Attempt {
	state.mu.Lock()
	state.owners++
	state.mu.Unlock()
	handle := &attemptHandle{state: state}
	runtime.AddCleanup(handle, func(state *attemptState) { state.release() }, state)
	return Attempt{handle: handle}
}

// Clone is another owner's handle on the same attempt.
func (a Attempt) Clone() Attempt {
	return newAttempt(a.handle.state)
}

// Release gives this owner's handle back. Releasing a handle twice has no
// further effect.
func (a Attempt) Release() {
	if a.handle.released.CompareAndSwap(false, true) {
		runtime.KeepAlive(a.handle)
		a.handle.state.release()
	}
}

func (s *attemptState) release() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.owners--
	if s.owners == 0 {
		s.drop()
	}
}

// drop is Rust's Drop for AttemptState: an unfinished attempt records its
// cancellation. The caller holds mu.
func (s *attemptState) drop() {
	if !s.finished {
		s.finished = true
		s.record.Status = "cancelled"
		s.record.Action = "Check cancelled before its outcome was confirmed."
		s.record.UpdatedAtMs = EpochMillis()
		s.log.append(&s.record)
	}
}

func (a Attempt) ID() string {
	state := a.handle.state
	state.mu.Lock()
	defer state.mu.Unlock()
	return state.record.ID
}

func (a Attempt) Update(answer *string, technical json.RawMessage) {
	state := a.handle.state
	state.mu.Lock()
	defer state.mu.Unlock()
	if state.finished {
		return
	}
	if answer != nil {
		state.record.Answer = *answer
	}
	existing, existingErr := jsonvalue.Parse(state.record.Technical)
	fieldsValue, fieldsErr := jsonvalue.Parse(technical)
	existingObject, existingIsObject := existing.(jsonvalue.Object)
	fields, fieldsAreObject := fieldsValue.(jsonvalue.Object)
	if len(state.record.Technical) > 0 && existingErr == nil && fieldsErr == nil && existingIsObject && fieldsAreObject {
		// serde_json's Map::extend: a known key keeps its place.
		existingObject = existingObject.Clone()
		for _, field := range fields {
			existingObject.Set(field.Key, field.Value)
		}
		if encoded, err := existingObject.MarshalJSON(); err == nil {
			state.record.Technical = encoded
		}
	}
	state.record.UpdatedAtMs = EpochMillis()
	state.log.append(&state.record)
}

func (a Attempt) Finish(status, action string) {
	state := a.handle.state
	state.mu.Lock()
	defer state.mu.Unlock()
	if state.finished {
		return
	}
	state.finished = true
	state.record.Status = status
	state.record.Action = action
	state.record.UpdatedAtMs = EpochMillis()
	state.log.append(&state.record)
}

func jevSegment(directory string, n int) string {
	return filepath.Join(directory, fmt.Sprintf("decisions.%d.jsonl", n))
}

func jevAppend(directory string, record *JevDecision, limit uint64) error {
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return err
	}
	encoded, err := agentMarshalJSON(record)
	if err != nil {
		return err
	}
	encoded = append(encoded, '\n')
	current := jevSegment(directory, 0)
	var size uint64
	if info, err := os.Stat(current); err == nil {
		size = uint64(info.Size())
	}
	if size+uint64(len(encoded)) > limit {
		for n := jevSegments - 1; n >= 1; n-- {
			previous := jevSegment(directory, n-1)
			destination := jevSegment(directory, n)
			if jevExists(destination) {
				if err := os.Remove(destination); err != nil {
					return err
				}
			}
			if jevExists(previous) {
				if err := os.Rename(previous, destination); err != nil {
					return err
				}
			}
		}
	}
	file, err := os.OpenFile(current, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	if _, err := file.Write(encoded); err != nil {
		file.Close()
		return err
	}
	return file.Close()
}

// jevExists is Rust's Path::exists: false for any error.
func jevExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// JevRead is a blocking read; callers must use a background task. Live reads
// are ordered after writes. (Rust: `jev::read`.)
func JevRead(directory, session string, id *string) (DecisionPage, error) {
	jevRegistryState.Lock()
	var writer *jevWriter
	if pointer, ok := jevRegistryState.writers[directory]; ok {
		writer = pointer.Value()
	}
	jevRegistryState.Unlock()
	if writer != nil {
		reply := make(chan jevReadResult, 1)
		if !writer.queue.send(jevReadMessage{session: session, id: id, reply: reply}) {
			return DecisionPage{}, errors.New("Jev diagnostic writer stopped")
		}
		runtime.KeepAlive(writer)
		result := <-reply
		return result.page, result.err
	}
	return jevReadFiles(directory, session, id)
}

func jevReadFiles(directory, session string, id *string) (DecisionPage, error) {
	records := map[string]JevDecision{}
	page := DecisionPage{Decisions: []JevDecision{}, Warnings: []string{}}
	const unreadable = "An unreadable diagnostic record was skipped."
	for n := jevSegments - 1; n >= 0; n-- {
		file, err := os.Open(jevSegment(directory, n))
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return DecisionPage{}, fmt.Errorf("read Jev diagnostics: %w", err)
		}
		reader := bufio.NewReader(io.LimitReader(file, int64(jevSegmentBytes+1024*1024)))
		readErr := func() error {
			for {
				line, err := reader.ReadString('\n')
				if err != nil && !errors.Is(err, io.EOF) {
					return err
				}
				if line == "" {
					return nil
				}
				if !utf8.ValidString(line) {
					return errors.New("stream did not contain valid UTF-8")
				}
				if !strings.HasSuffix(line, "\n") {
					page.Warnings = append(page.Warnings, "An incomplete diagnostic write was skipped.")
					return nil
				}
				var record JevDecision
				if json.Unmarshal([]byte(line), &record) != nil {
					if !slices.Contains(page.Warnings, unreadable) {
						page.Warnings = append(page.Warnings, unreadable)
					}
					continue
				}
				if record.Version != 1 || record.SessionID != session || (id != nil && record.ID != *id) {
					continue
				}
				if record.Status == "pending" && record.Owner != jevOwner() {
					record.Status = "interrupted"
					record.Action = "The previous process ended before recording an outcome."
				}
				if id == nil {
					record.Technical = nil
				}
				records[record.ID] = record
			}
		}()
		file.Close()
		if readErr != nil {
			return DecisionPage{}, readErr
		}
	}
	for _, record := range records {
		page.Decisions = append(page.Decisions, record)
	}
	jevSortNewestFirst(page.Decisions)
	if len(page.Decisions) > 100 {
		page.Decisions = page.Decisions[:100]
	}
	return page, nil
}
