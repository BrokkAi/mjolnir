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
	"log/slog"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
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
	if err == nil {
		m.failures = 0
		m.state = guardianClassifierState[E]{kind: guardianClassifierStateReady, engine: engine}
		return
	}
	if m.failures < math.MaxUint32 {
		m.failures++
	}
	// Same backoff as the GitHub student, including HTTP Retry-After.
	delay := time.Second << min(m.failures-1, 8)
	var failure *preparationFailure
	if errors.As(err, &failure) && failure.retryAfter != nil {
		delay = max(delay, *failure.retryAfter)
	}
	slog.Warn("Wolf Defender Jev preparation failed", "target", "mj_guardian", "error", err.Error(), "retry_seconds", uint64(delay/time.Second))
	m.state = guardianClassifierState[E]{kind: guardianClassifierStateFailed, retryAt: time.Now().Add(delay)}
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
	if core.WolfActionExceedsStage1Limit(request) {
		return GuardianStage1ResultEscalatedActionTooLong{}
	}
	permit, engine, reason, ok := admitInference(&c.inner.machine, c.inner.wake, c.inner.permits, c.ctx)
	if !ok {
		return GuardianStage1ResultUnavailable{Reason: reason}
	}
	cloned := *request
	cloned.UserInstructions = slices.Clone(request.UserInstructions)
	cancellation := c.ctx
	task := core.SpawnBlocking(func() GuardianStage1Result {
		// The permit stays with the native Run if the requester times out.
		defer permit.release()
		if cancellation.Err() != nil {
			return GuardianStage1ResultUnavailable{Reason: "stopped"}
		}
		verdict, err := engine.score(&cloned)
		if err != nil {
			slog.Warn("Wolf Defender Jev inference failed", "target", "mj_guardian", "error", err.Error())
			return GuardianStage1ResultUnavailable{Reason: "inference failed"}
		}
		return verdict
	})
	select {
	case <-c.ctx.Done():
		return GuardianStage1ResultUnavailable{Reason: "stopped"}
	case <-ctx.Done():
		// The requester gave up; Rust drops the future here.
		return GuardianStage1ResultUnavailable{Reason: "stopped"}
	case <-task.Done():
	}
	verdict, err := task.Join(context.Background())
	if err != nil {
		slog.Warn("Wolf Defender Jev inference task failed", "target", "mj_guardian", "error", err.Error())
		return GuardianStage1ResultUnavailable{Reason: "inference task failed"}
	}
	return verdict
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
	machine.mu.Lock()
	defer machine.mu.Unlock()
	if cancellation.Err() != nil {
		return guardianPermit{}, nil, "stopped", false
	}
	if reason, ready := machine.request(wake); !ready {
		return guardianPermit{}, nil, reason, false
	}
	if machine.state.kind != guardianClassifierStateReady {
		return guardianPermit{}, nil, "loading", false
	}
	select {
	case permits <- struct{}{}:
	default:
		return guardianPermit{}, nil, "busy", false
	}
	return guardianPermit{permits: permits}, machine.state.engine, "", true
}

// guardianClassifierSupervise loads the engine whenever a request wakes
// it, until ctx (the classifier's cancellation) ends.
func guardianClassifierSupervise(ctx context.Context, inner *guardianClassifierInner, dataDirectory string, useGPU bool) {
	for {
		// Cancellation first, as Rust's biased select checks it first.
		if ctx.Err() != nil {
			return
		}
		select {
		case <-ctx.Done():
			return
		case <-inner.wake:
		}
		done := make(chan error, 1)
		go func() {
			done <- func() error {
				directory, runtimeLibrary, err := guardianClassifierPrepare(ctx, dataDirectory, useGPU)
				if err != nil {
					return err
				}
				// Publish on the blocking work so a cancelled load releases
				// its native weights there, even when the supervisor has
				// already gone.
				task := core.SpawnBlocking(func() struct{} {
					if ctx.Err() != nil {
						return struct{}{}
					}
					engine, err := loadGuardianClassifierEngine(directory, runtimeLibrary, useGPU)
					inner.machine.mu.Lock()
					defer inner.machine.mu.Unlock()
					if ctx.Err() == nil {
						inner.machine.loaded(engine, err)
					}
					return struct{}{}
				})
				if _, err := task.Join(context.Background()); err != nil {
					return fmt.Errorf("Wolf Defender Jev loading task failed: %w", err)
				}
				return nil
			}()
		}()
		var err error
		select {
		case <-ctx.Done():
			return
		case err = <-done:
		}
		if ctx.Err() != nil {
			return
		}
		if err != nil {
			inner.machine.mu.Lock()
			if ctx.Err() == nil {
				inner.machine.loaded(nil, err)
			}
			inner.machine.mu.Unlock()
		}
	}
}

// guardianClassifierPrepare returns the verified Wolf model directory and
// ORT library path.
func guardianClassifierPrepare(ctx context.Context, dataDirectory string, useGPU bool) (string, string, error) {
	if err := validatePlatform(); err != nil {
		return "", "", err
	}
	var pinned manifest
	if err := json.Unmarshal(guardianClassifierManifestJSON, &pinned); err != nil {
		return "", "", err
	}
	client := &http.Client{}
	notice := func(notice StudentNotice) {
		slog.Warn("Wolf Defender Jev runtime preparation notice", "target", "mj_guardian", "notice", fmt.Sprintf("%#v", notice))
	}
	var directory, runtimeLibrary string
	err := tryJoinAll(ctx, []func(ctx context.Context) error{
		func(ctx context.Context) error {
			var err error
			directory, err = prepare(ctx, filepath.Join(dataDirectory, "models", "wolf-defender-jev"), pinned, "https://huggingface.co", client)
			return err
		},
		func(ctx context.Context) error {
			var err error
			runtimeLibrary, err = prepareSelected(ctx, filepath.Join(dataDirectory, "github-student-native"), useGPU, notice, client)
			return err
		},
	})
	if err != nil {
		return "", "", err
	}
	return directory, runtimeLibrary, nil
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
	if config.MaxCaseTokens != core.WolfStage1MaxCaseTokens {
		return nil, errors.New("Wolf case token limit disagrees with the pinned configuration")
	}
	specialID := func(token *specialToken, fallback string) (int64, error) {
		text := fallback
		if token != nil {
			text = token.text()
		}
		id, ok := tokenizer.tokenToID(text)
		if !ok {
			return 0, fmt.Errorf("Wolf tokenizer has no %s token", text)
		}
		return int64(id), nil
	}
	bos, err := specialID(specials.BosToken, "<bos>")
	if err != nil {
		return nil, err
	}
	eos, err := specialID(specials.EosToken, "<eos>")
	if err != nil {
		return nil, err
	}
	question, err := encode(tokenizer, core.WolfStage1Question)
	if err != nil {
		return nil, err
	}
	question = question[:min(len(question), config.MaxQuestionTokens)]
	var options [][]int64
	for _, option := range core.WolfStage1Options {
		ids, err := encode(tokenizer, option)
		if err != nil {
			return nil, err
		}
		ids = ids[:min(len(ids), config.MaxOptionTokens)]
		options = append(options, slices.Concat([]int64{bos, tokOption}, ids, []int64{eos}))
	}
	width := 0
	for _, ids := range options {
		width = max(width, len(ids))
	}
	if len(options) == 0 {
		return nil, errors.New("no Wolf options")
	}
	shape := []int64{int64(len(core.WolfStage1Options)), int64(width)}
	var optionIDs, optionMask []int64
	for _, ids := range options {
		for range ids {
			optionMask = append(optionMask, 1)
		}
		optionIDs = append(optionIDs, ids...)
		paddedEnd := (len(optionIDs) + width - 1) / width * width
		for len(optionIDs) < paddedEnd {
			optionIDs = append(optionIDs, 0)
		}
		for len(optionMask) < paddedEnd {
			optionMask = append(optionMask, 0)
		}
	}
	optionsTensor, err := newOrtTensorInt64(shape, optionIDs)
	if err != nil {
		return nil, err
	}
	maskTensor, err := newOrtTensorInt64(shape, optionMask)
	if err != nil {
		return nil, err
	}
	return &prompt{bos: bos, eos: eos, question: question, options: optionsTensor, optionMask: maskTensor}, nil
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
	environment, err := environment(runtimeLib)
	if err != nil {
		return nil, err
	}
	tokenizer, err := loadHFTokenizer(filepath.Join(directory, "tokenizer.json"), nil)
	if err != nil {
		return nil, fmt.Errorf("could not load Wolf tokenizer: %w", err)
	}
	configData, err := os.ReadFile(filepath.Join(directory, "config.json"))
	if err != nil {
		return nil, err
	}
	var config modelConfig
	if err := json.Unmarshal(configData, &config); err != nil {
		return nil, err
	}
	specialsData, err := os.ReadFile(filepath.Join(directory, "tokenizer_config.json"))
	if err != nil {
		return nil, err
	}
	var specials tokenizerConfig
	if err := json.Unmarshal(specialsData, &specials); err != nil {
		return nil, err
	}
	prompt, err := loadPrompt(tokenizer, config, specials)
	if err != nil {
		return nil, err
	}
	session, err := loadSession(environment, filepath.Join(directory, filepath.FromSlash(modelFile)), useGPU)
	if err != nil {
		return nil, err
	}
	return &guardianClassifierEngine{tokenizer: tokenizer, prompt: prompt, session: session}, nil
}

func (e *guardianClassifierEngine) score(request *core.GuardianReviewRequest) (GuardianStage1Result, error) {
	started := time.Now()
	caseIDs, err := encode(e.tokenizer, core.WolfScenePacket(request))
	if err != nil {
		return nil, err
	}
	tokens := len(caseIDs)
	ids, questionPos, ok := caseInput(e.prompt.bos, e.prompt.eos, caseIDs, e.prompt.question)
	if !ok {
		return GuardianStage1ResultOverflow{Tokens: tokens}, nil
	}
	length := int64(len(ids))
	mask := make([]int64, len(ids))
	for i := range mask {
		mask[i] = 1
	}
	e.sessionMu.Lock()
	defer e.sessionMu.Unlock()
	inputIDs, err := newOrtTensorInt64([]int64{1, length}, ids)
	if err != nil {
		return nil, err
	}
	defer inputIDs.release()
	attentionMask, err := newOrtTensorInt64([]int64{1, length}, mask)
	if err != nil {
		return nil, err
	}
	defer attentionMask.release()
	position, err := newOrtTensorInt64([]int64{1}, []int64{questionPos})
	if err != nil {
		return nil, err
	}
	defer position.release()
	outputs, err := e.session.run([]ortInput{
		{name: "input_ids", value: inputIDs},
		{name: "attention_mask", value: attentionMask},
		{name: "question_pos", value: position},
		{name: "option_input_ids", value: e.prompt.options},
		{name: "option_attention_mask", value: e.prompt.optionMask},
	}, nil)
	if err != nil {
		return nil, err
	}
	defer outputs.release()
	logitsValue, ok := outputs.get("logits")
	if !ok || logitsValue == nil {
		return nil, errors.New("Wolf graph has no logits output")
	}
	shape, logits, err := logitsValue.tensorFloat32()
	if err != nil {
		return nil, err
	}
	if !slices.Equal(shape, []int64{1, 2}) {
		dimensions := make([]string, len(shape))
		for i, dimension := range shape {
			dimensions[i] = strconv.FormatInt(dimension, 10)
		}
		return nil, fmt.Errorf("Wolf returned unexpected logits shape [%s]", strings.Join(dimensions, ", "))
	}
	probability, err := unsafeProbability(logits)
	if err != nil {
		return nil, err
	}
	return GuardianStage1ResultScored{UnsafeProb: probability, Tokens: tokens, ElapsedMs: uint64(time.Since(started).Milliseconds())}, nil
}

// loadSession loads model, on the selected GPU provider when useGPU asks
// for one and it loads, otherwise on the CPU.
func loadSession(environment *ortEnvironment, model string, useGPU bool) (*ortSession, error) {
	builder := func() (*ortSessionBuilder, error) {
		builder, err := newOrtSessionBuilder(environment)
		if err != nil {
			return nil, err
		}
		if err := builder.withIntraThreads(min(runtime.NumCPU(), 4)); err != nil {
			return nil, err
		}
		if err := builder.withInterThreads(1); err != nil {
			return nil, err
		}
		return builder, nil
	}
	if useGPU {
		// Use only providers that support Wolf's dynamic input shapes.
		var provider *ortExecutionProvider
		switch {
		case cudaSelected():
			provider = &ortExecutionProvider{name: ortCUDAProvider, options: []ortOption{{key: "use_tf32", value: "0"}}}
		case directmlSelected():
			provider = &ortExecutionProvider{name: ortDirectMLProvider}
		}
		if provider != nil {
			provider.errorOnFailure = true
			session, err := func() (*ortSession, error) {
				builder, err := builder()
				if err != nil {
					return nil, err
				}
				if err := builder.withMemoryPattern(false); err != nil {
					return nil, err
				}
				if err := builder.withParallelExecution(false); err != nil {
					return nil, err
				}
				if err := builder.withExecutionProviders(*provider, ortExecutionProvider{name: ortCPUProvider}); err != nil {
					return nil, err
				}
				return builder.commitFromFile(model)
			}()
			if err == nil {
				return session, nil
			}
			slog.Warn("Wolf GPU session failed to load; using CPU", "target", "mj_guardian", "error", err.Error())
		}
	}
	cpu, err := builder()
	if err != nil {
		return nil, err
	}
	return cpu.commitFromFile(model)
}
