// Port of mj-github-relevance/src/guardian_classifier.rs.

// Daemon-local Wolf Defender Jev inference for the Guardian's approve-only
// stage.
//
// State, Machine, Inner, Engine, supervise and prepare collide with
// github_student.rs's and inference.rs's names in the flat package, so they
// carry the guardianClassifier prefix here.

package githubrelevance

import (
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const modelFile = "onnx/onnx_fp32/model.onnx"

const (
	tokCase     int64 = 7
	tokQuestion int64 = 8
	tokOption   int64 = 9
)

//go:embed data/guardian_classifier/manifest.json
var guardianClassifierManifestJSON []byte

// GuardianStage1Result is the approve-only stage's verdict on one review.
type GuardianStage1Result interface {
	isGuardianStage1Result()
}

type GuardianStage1ResultScored struct {
	UnsafeProb float64
	Tokens     int
	ElapsedMs  uint64
}

type GuardianStage1ResultEscalatedActionTooLong struct{}

type GuardianStage1ResultOverflow struct {
	Tokens int
}

type GuardianStage1ResultUnavailable struct {
	Reason string
}

func (GuardianStage1ResultScored) isGuardianStage1Result()                 {}
func (GuardianStage1ResultEscalatedActionTooLong) isGuardianStage1Result() {}
func (GuardianStage1ResultOverflow) isGuardianStage1Result()               {}
func (GuardianStage1ResultUnavailable) isGuardianStage1Result()            {}

type guardianClassifierStateKind uint8

const (
	guardianClassifierStateDormant guardianClassifierStateKind = iota
	guardianClassifierStateLoading
	guardianClassifierStateReady
	guardianClassifierStateFailed
)

// guardianClassifierState is State<E>: Ready holds engine, Failed retryAt.
type guardianClassifierState[E any] struct {
	kind    guardianClassifierStateKind
	engine  *E
	retryAt time.Time
}

// guardianClassifierMachine is Mutex<Machine<E>>: mu guards the fields
// below it.
type guardianClassifierMachine[E any] struct {
	mu       sync.Mutex
	state    guardianClassifierState[E]
	failures uint32
}

// request admits a request against the state: true when the engine is
// ready, otherwise the reason it is unavailable. A dormant machine, or a
// failed one past its backoff, starts loading and wakes the supervisor.
// The caller holds mu.
func (m *guardianClassifierMachine[E]) request(wake chan struct{}) (string, bool) {
	switch m.state.kind {
	case guardianClassifierStateReady:
		return "", true
	case guardianClassifierStateLoading:
		return "loading", false
	case guardianClassifierStateFailed:
		if time.Now().Before(m.state.retryAt) {
			return "load failed", false
		}
	}
	m.state = guardianClassifierState[E]{kind: guardianClassifierStateLoading}
	notifyOne(wake)
	return "loading", false
}

// loaded records a load's outcome: engine when err is nil. The caller
// holds mu.
func (m *guardianClassifierMachine[E]) loaded(engine *E, err error) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs Machine::loaded")
}

// notifyOne is tokio's Notify::notify_one on a one-slot channel: at most one
// pending wake is kept.
func notifyOne(wake chan struct{}) {
	select {
	case wake <- struct{}{}:
	default:
	}
}

type guardianClassifierInner struct {
	machine guardianClassifierMachine[guardianClassifierEngine]
	// wake is the supervisor's Notify, holding at most one pending wake.
	wake chan struct{}
	// permits is the Semaphore of one inference slot: ORT requires exclusive
	// access to Run on this session. Never queue behind an active inference:
	// a busy collision escalates immediately to Stage 2 so slot-wait time
	// never counts against a review deadline.
	permits chan struct{}
}

// GuardianClassifier is the daemon's Wolf Defender Jev stage.
type GuardianClassifier struct {
	inner *guardianClassifierInner
	// ctx and cancel are the classifier's CancellationToken.
	ctx    context.Context
	cancel context.CancelFunc
	// supervisorMu guards supervisor until Shutdown takes it.
	supervisorMu sync.Mutex
	supervisor   *core.JoinHandle[struct{}]
}

func NewGuardianClassifier(dataDirectory string, useGPU bool) *GuardianClassifier {
	owner := &guardianClassifierInner{
		wake:    make(chan struct{}, 1),
		permits: make(chan struct{}, 1),
	}
	ctx, cancel := context.WithCancel(context.Background())
	supervisor := core.Spawn(func() struct{} {
		guardianClassifierSupervise(ctx, owner, dataDirectory, useGPU)
		return struct{}{}
	})
	return &GuardianClassifier{inner: owner, ctx: ctx, cancel: cancel, supervisor: supervisor}
}

// Classify scores request, or says why it cannot.
func (c *GuardianClassifier) Classify(ctx context.Context, request *core.GuardianReviewRequest) GuardianStage1Result {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs GuardianClassifier::classify")
}

func (c *GuardianClassifier) Shutdown(ctx context.Context) error {
	c.cancel()
	c.supervisorMu.Lock()
	task := c.supervisor
	c.supervisor = nil
	c.supervisorMu.Unlock()
	if task != nil {
		if _, err := task.Join(ctx); err != nil {
			return fmt.Errorf("Wolf classifier supervisor failed: %w", err)
		}
	}
	c.releaseEngine()
	return nil
}

// releaseEngine drops the engine. Releasing native weights runs in the
// session's cleanup, off the caller's goroutine.
func (c *GuardianClassifier) releaseEngine() {
	c.inner.machine.mu.Lock()
	c.inner.machine.state = guardianClassifierState[guardianClassifierEngine]{kind: guardianClassifierStateDormant}
	c.inner.machine.mu.Unlock()
}

// Drop is Rust's Drop for GuardianClassifier.
func (c *GuardianClassifier) Drop() {
	c.cancel()
	c.releaseEngine()
}

// guardianPermit is an admitted inference slot (OwnedSemaphorePermit);
// release returns it.
type guardianPermit struct {
	permits chan struct{}
}

func (p guardianPermit) release() {
	<-p.permits
}

// admitInference admits one inference without queuing: the permit and the
// ready engine, or the reason the request is refused ("stopped", "loading",
// "load failed" or "busy").
func admitInference[E any](machine *guardianClassifierMachine[E], wake chan struct{}, permits chan struct{}, cancellation context.Context) (guardianPermit, *E, string, bool) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs admit_inference")
}

// guardianClassifierSupervise loads the engine whenever a request wakes
// it, until ctx (the classifier's cancellation) ends.
func guardianClassifierSupervise(ctx context.Context, inner *guardianClassifierInner, dataDirectory string, useGPU bool) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs supervise")
}

// guardianClassifierPrepare returns the verified Wolf model directory and
// ORT library path.
func guardianClassifierPrepare(ctx context.Context, dataDirectory string, useGPU bool) (string, string, error) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs prepare")
}

type modelConfig struct {
	MaxCaseTokens     int `json:"max_case_tokens"`
	MaxQuestionTokens int `json:"max_question_tokens"`
	MaxOptionTokens   int `json:"max_option_tokens"`
}

func (c *modelConfig) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "max_case_tokens", "max_question_tokens", "max_option_tokens"); err != nil {
		return err
	}
	type wire modelConfig
	return json.Unmarshal(data, (*wire)(c))
}

// specialToken is a special token as tokenizer_config.json writes it:
// untagged, either its text or an added token object with its content.
type specialToken struct {
	content string
}

func (t *specialToken) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err == nil {
		t.content = text
		return nil
	}
	var added struct {
		Content *string `json:"content"`
	}
	if err := json.Unmarshal(data, &added); err != nil || added.Content == nil {
		return errors.New("data did not match any variant of untagged enum SpecialToken")
	}
	t.content = *added.Content
	return nil
}

func (t *specialToken) text() string {
	return t.content
}

type tokenizerConfig struct {
	BosToken *specialToken `json:"bos_token"`
	EosToken *specialToken `json:"eos_token"`
}

type prompt struct {
	bos        int64
	eos        int64
	question   []int64
	options    *ortValue
	optionMask *ortValue
}

// loadPrompt is Prompt::load: the question and the padded option batch.
func loadPrompt(tokenizer hfTokenizer, config modelConfig, specials tokenizerConfig) (*prompt, error) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs Prompt::load")
}

// caseInput is the model input for a case and the question marker's
// position, false when the case exceeds core.WolfStage1MaxCaseTokens.
func caseInput(bos, eos int64, caseIDs, question []int64) ([]int64, int64, bool) {
	if len(caseIDs) > core.WolfStage1MaxCaseTokens {
		return nil, 0, false
	}
	ids := make([]int64, 0, len(caseIDs)+len(question)+5)
	ids = append(ids, bos, tokCase)
	ids = append(ids, caseIDs...)
	ids = append(ids, eos, tokQuestion)
	ids = append(ids, question...)
	ids = append(ids, eos)
	return ids, int64(len(caseIDs)) + 3, true
}

func encode(tokenizer hfTokenizer, text string) ([]int64, error) {
	encoded, err := tokenizer.encode(text, nil, false)
	if err != nil {
		return nil, fmt.Errorf("Wolf tokenization failed: %w", err)
	}
	ids := make([]int64, len(encoded.ids))
	for i, id := range encoded.ids {
		ids[i] = int64(id)
	}
	return ids, nil
}

// unsafeProbability is the softmax probability of the misaligned logit.
func unsafeProbability(logits []float32) (float64, error) {
	if len(logits) != 2 {
		return 0, fmt.Errorf("Wolf returned %d logits; expected two", len(logits))
	}
	aligned, misaligned := float64(logits[0]), float64(logits[1])
	if math.IsNaN(aligned) || math.IsInf(aligned, 0) || math.IsNaN(misaligned) || math.IsInf(misaligned, 0) {
		return 0, errors.New("Wolf returned a nonfinite logit")
	}
	maximum := math.Max(aligned, misaligned)
	aligned = math.Exp(aligned - maximum)
	misaligned = math.Exp(misaligned - maximum)
	return misaligned / (aligned + misaligned), nil
}

type guardianClassifierEngine struct {
	tokenizer hfTokenizer
	prompt    *prompt
	// sessionMu guards session (Rust's Mutex<Session>).
	sessionMu sync.Mutex
	session   *ortSession
}

// loadGuardianClassifierEngine is Engine::load.
func loadGuardianClassifierEngine(directory, runtimeLib string, useGPU bool) (*guardianClassifierEngine, error) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs Engine::load")
}

func (e *guardianClassifierEngine) score(request *core.GuardianReviewRequest) (GuardianStage1Result, error) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs Engine::score")
}

// loadSession loads model, on the selected GPU provider when useGPU asks
// for one and it loads, otherwise on the CPU.
func loadSession(environment *ortEnvironment, model string, useGPU bool) (*ortSession, error) {
	panic("unported: mj-github-relevance/src/guardian_classifier.rs load_session")
}
