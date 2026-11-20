// Port of mj-core/src/jev.rs.

// Bounded, local Jev diagnostics. No transcript or activity state is mutated
// here.

package core

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"weak"
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
	panic("unported: mj-core/src/jev.rs request_body")
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

type DecisionPage struct {
	Decisions []JevDecision `json:"decisions"`
	Warnings  []string      `json:"warnings"`
}

func (p *DecisionPage) Merge(other DecisionPage) {
	panic("unported: mj-core/src/jev.rs DecisionPage::merge")
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

type jevWriter struct {
	tx chan<- jevMessage
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
	panic("unported: mj-core/src/jev.rs DecisionLog::open")
}

func (l DecisionLog) append(decision *JevDecision) {
	panic("unported: mj-core/src/jev.rs DecisionLog::append")
}

func (l DecisionLog) Start(session, kind, checked, scope string) Attempt {
	panic("unported: mj-core/src/jev.rs DecisionLog::start")
}

// RecordAssessment publishes the owner's durable state, including later
// admission or supersession. Request tasks must not overwrite this record
// with their process-local outcome.
func (l DecisionLog) RecordAssessment(session string, assessment *TurnAssessment) {
	panic("unported: mj-core/src/jev.rs DecisionLog::record_assessment")
}

func (l DecisionLog) startWithID(session, kind, checked, scope, id string) Attempt {
	panic("unported: mj-core/src/jev.rs DecisionLog::start_with_id")
}

// Attempt is shared across classification and application; the last owner
// records cancellation. Rust records it when the last clone drops; Go has
// no drop, so the bodies phase must give the owners an explicit end (or a
// runtime cleanup) that appends the cancelled record.
type Attempt struct {
	state *attemptState
}

type attemptState struct {
	mu       sync.Mutex
	log      DecisionLog
	record   JevDecision
	finished bool
}

// drop is Rust's Drop for AttemptState: an unfinished attempt records its
// cancellation.
func (s *attemptState) drop() {
	panic("unported: mj-core/src/jev.rs AttemptState::drop")
}

func (a Attempt) ID() string {
	a.state.mu.Lock()
	defer a.state.mu.Unlock()
	return a.state.record.ID
}

func (a Attempt) Update(answer *string, technical json.RawMessage) {
	panic("unported: mj-core/src/jev.rs Attempt::update")
}

func (a Attempt) Finish(status, action string) {
	a.state.mu.Lock()
	defer a.state.mu.Unlock()
	if a.state.finished {
		return
	}
	a.state.finished = true
	a.state.record.Status = status
	a.state.record.Action = action
	a.state.record.UpdatedAtMs = EpochMillis()
	a.state.log.append(&a.state.record)
}

func jevSegment(directory string, n int) string {
	return filepath.Join(directory, fmt.Sprintf("decisions.%d.jsonl", n))
}

func jevAppend(directory string, record *JevDecision, limit uint64) error {
	panic("unported: mj-core/src/jev.rs append")
}

// JevRead is a blocking read; callers must use a background task. Live reads
// are ordered after writes. (Rust: `jev::read`.)
func JevRead(directory, session string, id *string) (DecisionPage, error) {
	panic("unported: mj-core/src/jev.rs read")
}

func jevReadFiles(directory, session string, id *string) (DecisionPage, error) {
	panic("unported: mj-core/src/jev.rs read_files")
}
