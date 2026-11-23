// Port of mj-github-relevance/src/github_student.rs.

// One daemon-owned model state machine. Downloads, retry scheduling and device
// transitions are decided here; readers never inspect the cache for readiness.
//
// Rust drops replaced engines, GPU sessions and preparation results on
// blocking threads because their destructors free native memory. In Go the
// native handles are released by cleanups attached to their owners
// (onnxruntime.go), which run off the caller's goroutine once the last
// reference is gone, so an engine still held by an in-flight inference
// outlives its replacement exactly as Rust's Arc keeps it alive.

package githubrelevance

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"net/http"
	"path/filepath"
	"sync"
	"sync/atomic"
	"time"
	"weak"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type StudentFailureKind string

const (
	StudentFailureKindDownload              StudentFailureKind = "Download"
	StudentFailureKindVerification          StudentFailureKind = "Verification"
	StudentFailureKindLoad                  StudentFailureKind = "Load"
	StudentFailureKindInference             StudentFailureKind = "Inference"
	StudentFailureKindWindowsRuntimeMissing StudentFailureKind = "WindowsRuntimeMissing"
)

// StudentState is the model state readers see. String is Rust's Debug
// rendering, which diagnostics quote.
type StudentState interface {
	isStudentState()
	String() string
}

type StudentStateLoading struct{}

type StudentStateReady struct{}

type StudentStateFailed struct {
	Kind          StudentFailureKind
	Cause         string
	NextRetryAtMs int64
}

func (StudentStateLoading) isStudentState() {}
func (StudentStateReady) isStudentState()   {}
func (StudentStateFailed) isStudentState()  {}

func (StudentStateLoading) String() string { return "Loading" }
func (StudentStateReady) String() string   { return "Ready" }
func (s StudentStateFailed) String() string {
	return fmt.Sprintf("Failed { kind: %s, cause: %s, next_retry_at_ms: %d }", s.Kind, core.RustDebugString(s.Cause), s.NextRetryAtMs)
}

// StudentError is the error policy routes on, never on diagnostic text
// (Rust's downcastable StudentError). Find it in an error chain with
// errors.As.
type StudentError interface {
	error
	isStudentError()
}

type StudentErrorUnavailable struct {
	State StudentState
}

type StudentErrorInference struct {
	Cause string
}

func (StudentErrorUnavailable) isStudentError() {}
func (StudentErrorInference) isStudentError()   {}

func (e StudentErrorUnavailable) Error() string {
	return "GitHub student unavailable: " + e.State.String()
}

func (e StudentErrorInference) Error() string {
	return "GitHub student inference failed: " + e.Cause
}

// StudentDevice is where the student runs; the values are Rust's Debug names.
type StudentDevice string

const (
	StudentDeviceCPU      StudentDevice = "Cpu"
	StudentDeviceDirectML StudentDevice = "DirectMl"
	StudentDeviceCoreML   StudentDevice = "CoreMl"
	StudentDeviceCUDA     StudentDevice = "Cuda"
)

type StudentDeviceFallbackReason interface {
	isStudentDeviceFallbackReason()
}

type StudentDeviceFallbackReasonSlower struct {
	GPUMs uint64
	CPUMs uint64
}

type StudentDeviceFallbackReasonFailed struct {
	Kind StudentFailureKind
}

func (StudentDeviceFallbackReasonSlower) isStudentDeviceFallbackReason() {}
func (StudentDeviceFallbackReasonFailed) isStudentDeviceFallbackReason() {}

type StudentNotice interface {
	isStudentNotice()
}

type StudentNoticeDeviceFallback struct {
	From   StudentDevice
	Reason StudentDeviceFallbackReason
}

type StudentNoticePlatformUnsupported struct{}

type StudentNoticeGPUAfterRestart struct{}

type StudentNoticeWindowsRuntimeMissing struct {
	Missing []string
}

type StudentNoticeCUDALibrariesMissing struct {
	Missing []string
}

type StudentNoticeInferenceFailing struct {
	Cause string
}

type StudentNoticeInferenceRecovered struct{}

func (StudentNoticeDeviceFallback) isStudentNotice()        {}
func (StudentNoticePlatformUnsupported) isStudentNotice()   {}
func (StudentNoticeGPUAfterRestart) isStudentNotice()       {}
func (StudentNoticeWindowsRuntimeMissing) isStudentNotice() {}
func (StudentNoticeCUDALibrariesMissing) isStudentNotice()  {}
func (StudentNoticeInferenceFailing) isStudentNotice()      {}
func (StudentNoticeInferenceRecovered) isStudentNotice()    {}

// Notice receives the student's typed transitions on background work
// (crate-private in Rust, exported here because NewStudent takes it).
type Notice = func(StudentNotice)

// cancellationToken is a tokio_util CancellationToken shared by Arc: ctx
// observes it, cancel cancels it, and its pointer is its identity.
type cancellationToken struct {
	ctx    context.Context
	cancel context.CancelFunc
}

func newCancellationToken() *cancellationToken {
	ctx, cancel := context.WithCancel(context.Background())
	return &cancellationToken{ctx: ctx, cancel: cancel}
}

// device is the ready engine's private Device choice. Variants holding a
// *gpu or *cancellationToken compare by pointer (Rust's Arc::ptr_eq).
type device interface {
	isDevice()
}

type deviceCPU struct{}

type deviceNeeded struct{}

type devicePreparing struct {
	request *cancellationToken
}

type devicePending struct {
	gpu *gpu
}

type deviceBenchmarking struct {
	gpu *gpu
}

type deviceGPU struct {
	gpu *gpu
}

func (deviceCPU) isDevice()          {}
func (deviceNeeded) isDevice()       {}
func (devicePreparing) isDevice()    {}
func (devicePending) isDevice()      {}
func (deviceBenchmarking) isDevice() {}
func (deviceGPU) isDevice()          {}

type ready struct {
	engine *engine
	// permits is the Arc<Semaphore> admitting inference: a buffered channel
	// holding one token per admitted call, its capacity from parallelism().
	permits chan struct{}
	device  device
	failing bool
}

type failure struct {
	kind      StudentFailureKind
	cause     string
	retryAt   time.Time
	retryAtMs int64
}

// state is the owner's private State; public is how readers see it.
type state interface {
	public() StudentState
}

type stateDormant struct{}

type stateLoading struct{}

// stateReady holds the ready engine by pointer, so the owner changes its
// device and health in place (Rust's `&mut State::Ready`).
type stateReady struct {
	ready *ready
}

type stateFailed struct {
	failure failure
}

func (stateDormant) public() StudentState { return StudentStateLoading{} }
func (stateLoading) public() StudentState { return StudentStateLoading{} }
func (stateReady) public() StudentState   { return StudentStateReady{} }
func (s stateFailed) public() StudentState {
	return StudentStateFailed{Kind: s.failure.kind, Cause: s.failure.cause, NextRetryAtMs: s.failure.retryAtMs}
}

type machine struct {
	state    state
	failures uint32
	useGPU   bool
}

type inner struct {
	// probeControlFailures makes that many CPU controls of a GPU probe fail
	// (a #[cfg(test)] field in Rust; always zero outside tests).
	probeControlFailures atomic.Int64
	// mu guards machine (Rust's Mutex<Machine>).
	mu      sync.Mutex
	machine machine
	// changed is the Notify woken on state changes. Waiters subscribe
	// before they read the state, so a change between their decision and
	// their wait is never lost; every waiter rereads the state, so waking
	// all of them (notify_waiters) also serves notify_one.
	changed *core.Watch[struct{}]
	notice  Notice
}

// innerFail is Inner::fail: count the failure and schedule the automatic
// retry. The caller holds the inner's mu.
func innerFail(machine *machine, kind StudentFailureKind, err error) {
	if machine.failures < math.MaxUint32 {
		machine.failures++
	}
	var detail *preparationFailure
	if errors.As(err, &detail) {
		kind = detail.kind
	}
	// Same retry progression as the existing GitHub credential lookup: one
	// second, doubling up to its eighth exponent, with server hints.
	delay := time.Second << min(machine.failures-1, 8)
	if detail != nil && detail.retryAfter != nil {
		delay = max(delay, *detail.retryAfter)
	}
	cause := err.Error()
	slog.Warn("GitHub student failed; automatic retry scheduled", "error", cause, "kind", string(kind), "retry_seconds", uint64(delay/time.Second))
	now := time.Now()
	machine.state = stateFailed{failure: failure{
		kind:      kind,
		cause:     cause,
		retryAt:   now.Add(delay),
		retryAtMs: now.UnixMilli() + delay.Milliseconds(),
	}}
}

// completed records an inference result from engine (err nil for a
// score). A late result from a replaced engine cannot change the new epoch.
func (i *inner) completed(engine *engine, score float64, err error) {
	i.mu.Lock()
	current, ok := i.machine.state.(stateReady)
	if !ok || current.ready.engine != engine {
		i.mu.Unlock()
		return
	}
	var notice StudentNotice
	switch {
	case err != nil && !current.ready.failing:
		current.ready.failing = true
		slog.Warn("GitHub student inference failing", "error", err.Error())
		notice = StudentNoticeInferenceFailing{Cause: err.Error()}
	case err == nil && current.ready.failing:
		current.ready.failing = false
		notice = StudentNoticeInferenceRecovered{}
	}
	i.mu.Unlock()
	if notice != nil {
		i.notice(notice)
	}
}

// notifyChanged wakes the state's waiters (Notify's notify_one and
// notify_waiters).
func (i *inner) notifyChanged() {
	i.changed.SendReplace(struct{}{})
}

type preparationOptions struct {
	modelBase     string
	modelManifest manifest
	// loadFailures makes that many model loads fail (a #[cfg(test)] field
	// in Rust; nil outside tests).
	loadFailures *atomic.Int64
}

func defaultPreparationOptions() preparationOptions {
	return preparationOptions{
		modelBase:     "https://huggingface.co",
		modelManifest: pinnedManifest().clone(),
	}
}

// Student is the daemon's handle on the local relevance model. Keep one
// handle per daemon. The notice callback receives typed transitions and is
// called on background work; public wording belongs to the notice consumer.
type Student struct {
	inner *inner
	// ctx and cancel are the student's CancellationToken.
	ctx    context.Context
	cancel context.CancelFunc
	// taskMu guards task, the supervisor until Shutdown takes it.
	taskMu sync.Mutex
	task   *core.JoinHandle[struct{}]
}

func NewStudent(dataDirectory string, useGPU bool, notice Notice) *Student {
	return startStudent(dataDirectory, useGPU, notice, defaultPreparationOptions())
}

func startStudent(dataDirectory string, useGPU bool, notice Notice, options preparationOptions) *Student {
	owner := &inner{
		machine: machine{state: stateDormant{}, useGPU: useGPU},
		changed: core.NewWatch(struct{}{}),
		notice:  notice,
	}
	ctx, cancel := context.WithCancel(context.Background())
	task := core.Spawn(func() struct{} {
		supervise(ctx, owner, dataDirectory, options)
		return struct{}{}
	})
	return &Student{inner: owner, ctx: ctx, cancel: cancel, task: task}
}

// SetUseGPU applies the GPU preference. A preference change starts one
// fresh probe. Repeated configuration reloads preserve a measured fallback;
// disabling retains the CPU engine.
func (s *Student) SetUseGPU(enabled bool) {
	s.inner.mu.Lock()
	if s.inner.machine.useGPU == enabled {
		s.inner.mu.Unlock()
		return
	}
	s.inner.machine.useGPU = enabled
	var old device
	if current, ok := s.inner.machine.state.(stateReady); ok {
		old = current.ready.device
		if enabled {
			current.ready.device = deviceNeeded{}
		} else {
			current.ready.device = deviceCPU{}
		}
	}
	s.inner.mu.Unlock()
	if preparing, ok := old.(devicePreparing); ok {
		preparing.request.cancel()
	}
	if enabled && gpuRequiresRestart() {
		s.inner.notice(StudentNoticeGPUAfterRestart{})
	}
	s.inner.notifyChanged()
}

func (s *Student) State() StudentState {
	s.inner.mu.Lock()
	defer s.inner.mu.Unlock()
	return s.inner.machine.state.public()
}

func (s *Student) ManifestRevision() string {
	return pinnedManifest().Revision
}

// scoreSegments is Student::score_segments, the inherent method; the
// StudentOwner method ScoreSegments (github_relevance.go) wraps it. The
// error carries a StudentError, findable with errors.As.
func (s *Student) scoreSegments(ctx context.Context, item, session string) (float64, error) {
	owner := s.inner
	owner.mu.Lock()
	if _, ok := owner.machine.state.(stateDormant); ok {
		owner.machine.state = stateLoading{}
		owner.mu.Unlock()
		owner.notifyChanged()
		return 0, StudentErrorUnavailable{State: StudentStateLoading{}}
	}
	// Queue time is outside every measured inference duration. Reserve the
	// actual device only after admission, using the owner's state.
	current, ok := owner.machine.state.(stateReady)
	if !ok {
		state := owner.machine.state.public()
		owner.mu.Unlock()
		return 0, StudentErrorUnavailable{State: state}
	}
	permits := current.ready.permits
	waitingEpoch := weak.Make(current.ready.engine)
	owner.mu.Unlock()
	select {
	case <-s.ctx.Done():
		return 0, StudentErrorUnavailable{State: StudentStateLoading{}}
	case <-ctx.Done():
		return 0, ctx.Err()
	case permits <- struct{}{}:
	}
	release := func() { <-permits }
	owner.mu.Lock()
	current, ok = owner.machine.state.(stateReady)
	if !ok || weak.Make(current.ready.engine) != waitingEpoch {
		state := owner.machine.state.public()
		owner.mu.Unlock()
		release()
		return 0, StudentErrorUnavailable{State: state}
	}
	var assigned work
	switch reserved := current.ready.device.(type) {
	case devicePending:
		current.ready.device = deviceBenchmarking{gpu: reserved.gpu}
		assigned = workBenchmark{probe: &probe{
			inner:   owner,
			engine:  current.ready.engine,
			gpu:     reserved.gpu,
			outcome: probeOutcomeControlFailed{},
		}}
	case deviceGPU:
		assigned = workGPU{gpu: reserved.gpu}
	default:
		assigned = workCPU{}
	}
	engine := current.ready.engine
	owner.mu.Unlock()
	epoch := weak.Make(engine)
	type outcome struct {
		score float64
		err   error
	}
	task := core.SpawnBlocking(func() outcome {
		// The admission permit returns after the work, including when it
		// panics.
		defer release()
		score, err := run(owner, engine, assigned, item, session)
		owner.completed(engine, score, err)
		return outcome{score: score, err: err}
	})
	select {
	case <-s.ctx.Done():
		return 0, StudentErrorUnavailable{State: StudentStateLoading{}}
	case <-ctx.Done():
		return 0, ctx.Err()
	case <-task.Done():
	}
	result, err := task.Join(context.Background())
	if err != nil {
		failed := fmt.Errorf("GitHub student inference task failed: %w", err)
		if alive := epoch.Value(); alive != nil {
			owner.completed(alive, 0, failed)
		}
		return 0, StudentErrorInference{Cause: failed.Error()}
	}
	if result.err != nil {
		return 0, StudentErrorInference{Cause: result.err.Error()}
	}
	return result.score, nil
}

// Shutdown cancels preparation promptly; in-flight inference owns no daemon
// handoff admission and its supervised blocking work reports its own
// outcome.
func (s *Student) Shutdown(ctx context.Context) error {
	s.cancel()
	s.taskMu.Lock()
	task := s.task
	s.task = nil
	s.taskMu.Unlock()
	if task != nil {
		if _, err := task.Join(ctx); err != nil {
			return fmt.Errorf("student supervisor failed: %w", err)
		}
	}
	s.releaseState()
	return nil
}

// releaseState drops the engine. Native graph/session destruction can
// release hundreds of MB; it runs in the native handles' cleanups, off the
// caller's goroutine, once in-flight inference has let go of the engine.
func (s *Student) releaseState() {
	s.inner.mu.Lock()
	s.inner.machine.state = stateLoading{}
	s.inner.mu.Unlock()
}

// Drop is Rust's Drop for Student: cancel preparation and release the
// engine.
func (s *Student) Drop() {
	s.cancel()
	s.releaseState()
}

type work interface {
	isWork()
}

type workCPU struct{}

type workBenchmark struct {
	probe *probe
}

type workGPU struct {
	gpu *gpu
}

func (workCPU) isWork()       {}
func (workBenchmark) isWork() {}
func (workGPU) isWork()       {}

// probeOutcome is a GPU benchmark's decision. A control failure makes no
// device decision. Pending is consumed only by a later real pair; neither
// the supervisor nor this probe retries it in a loop.
type probeOutcome interface {
	isProbeOutcome()
}

type probeOutcomeGPUWins struct{}

type probeOutcomeCPUWins struct {
	gpuMs uint64
	cpuMs uint64
}

type probeOutcomeGPUFailed struct{}

type probeOutcomeControlFailed struct{}

func (probeOutcomeGPUWins) isProbeOutcome()       {}
func (probeOutcomeCPUWins) isProbeOutcome()       {}
func (probeOutcomeGPUFailed) isProbeOutcome()     {}
func (probeOutcomeControlFailed) isProbeOutcome() {}

// probe owns the benchmark reservation from admission through blocking
// execution, including cancellation before execution and panics. Exactly
// one drop resolves it: callers defer drop, and complete drops with its
// outcome.
type probe struct {
	inner   *inner
	engine  *engine
	gpu     *gpu
	outcome probeOutcome
	// resolved makes drop resolve the reservation once.
	resolved atomic.Bool
}

func (p *probe) complete(outcome probeOutcome) {
	p.outcome = outcome
	p.drop()
}

// drop is Rust's Drop for Probe: resolve the reservation with the recorded
// outcome, ControlFailed unless complete supplied one.
func (p *probe) drop() {
	if p.resolved.CompareAndSwap(false, true) {
		outcome := p.outcome
		p.outcome = probeOutcomeControlFailed{}
		deviceTransition(p.inner, p.engine, p.gpu, outcome)
	}
}

func deviceTransition(inner *inner, engine *engine, gpu *gpu, outcome probeOutcome) {
	// Reservation cleanup must work even while another failure unwinds; a
	// Go mutex is not poisoned by a panic.
	inner.mu.Lock()
	current, ok := inner.machine.state.(stateReady)
	if !ok || current.ready.engine != engine || !reservedFor(current.ready.device, gpu) {
		inner.mu.Unlock()
		return
	}
	var reason StudentDeviceFallbackReason
	switch outcome := outcome.(type) {
	case probeOutcomeGPUWins:
		current.ready.device = deviceGPU{gpu: gpu}
	case probeOutcomeControlFailed:
		current.ready.device = devicePending{gpu: gpu}
	case probeOutcomeCPUWins:
		current.ready.device = deviceCPU{}
		reason = StudentDeviceFallbackReasonSlower{GPUMs: outcome.gpuMs, CPUMs: outcome.cpuMs}
	case probeOutcomeGPUFailed:
		current.ready.device = deviceCPU{}
		reason = StudentDeviceFallbackReasonFailed{Kind: StudentFailureKindInference}
	}
	inner.mu.Unlock()
	if reason != nil {
		inner.notice(StudentNoticeDeviceFallback{From: gpu.device, Reason: reason})
	}
}

// reservedFor reports whether device is gpu's selection or benchmark.
func reservedFor(device device, gpu *gpu) bool {
	switch current := device.(type) {
	case deviceGPU:
		return current.gpu == gpu
	case deviceBenchmarking:
		return current.gpu == gpu
	}
	return false
}

// gpuSelected reports, under the owner's lock, whether engine is still
// the ready engine and device is its device.
func gpuSelected(inner *inner, engine *engine, matches func(device) bool) bool {
	inner.mu.Lock()
	defer inner.mu.Unlock()
	current, ok := inner.machine.state.(stateReady)
	return ok && current.ready.engine == engine && matches(current.ready.device)
}

func run(inner *inner, engine *engine, work work, item, session string) (float64, error) {
	switch work := work.(type) {
	case workBenchmark:
		defer work.probe.drop()
		outcome, score, err := probePair(inner, engine, work.probe.gpu, item, session)
		work.probe.complete(outcome)
		return score, err
	case workGPU:
		ids, mask, err := engine.tokens(item, session)
		if err != nil {
			return 0, err
		}
		score, selected, err := func() (float64, bool, error) {
			// DirectML permits one Run per session. Check selection after
			// that admission so queued calls cannot undo CPU selection.
			work.gpu.sessionMu.Lock()
			defer work.gpu.sessionMu.Unlock()
			if !gpuSelected(inner, engine, func(current device) bool {
				selected, ok := current.(deviceGPU)
				return ok && selected.gpu == work.gpu
			}) {
				return 0, false, nil
			}
			score, err := work.gpu.session.score(ids, mask)
			return score, true, err
		}()
		switch {
		case err != nil:
			slog.Warn("GPU student inference failed; retaining CPU", "error", err.Error(), "device", string(work.gpu.device))
			deviceTransition(inner, engine, work.gpu, probeOutcomeGPUFailed{})
			return engine.scoreTokens(ids, mask)
		case selected:
			return score, nil
		default:
			return engine.scoreTokens(ids, mask)
		}
	default:
		ids, mask, err := engine.tokens(item, session)
		if err != nil {
			return 0, err
		}
		return engine.scoreTokens(ids, mask)
	}
}

// probePair produces one outcome on every ordinary exit; the reservation
// guard supplies ControlFailed if this computation panics instead of
// returning an outcome.
func probePair(inner *inner, engine *engine, gpu *gpu, item, session string) (probeOutcome, float64, error) {
	ids, mask, err := engine.tokens(item, session)
	if err != nil {
		return probeOutcomeControlFailed{}, 0, err
	}
	// Compare warm executions on both sessions. A first CPU run may
	// allocate and initialize kernels just as the discarded GPU warmup does.
	if _, err := engine.scoreTokens(ids, mask); err != nil {
		return probeOutcomeControlFailed{}, 0, err
	}
	start := time.Now()
	for {
		remaining := inner.probeControlFailures.Load()
		if remaining <= 0 {
			break
		}
		if inner.probeControlFailures.CompareAndSwap(remaining, remaining-1) {
			return probeOutcomeControlFailed{}, 0, errors.New("recorded CPU-control inference failure")
		}
	}
	cpu, err := engine.scoreTokens(ids, mask)
	if err != nil {
		return probeOutcomeControlFailed{}, 0, err
	}
	cpuTime := time.Since(start)
	gpu.sessionMu.Lock()
	defer gpu.sessionMu.Unlock()
	// A preference or engine change has already released the reservation.
	// Its epoch check also prevents this stale outcome from changing that
	// choice.
	if !gpuSelected(inner, engine, func(current device) bool {
		benchmarking, ok := current.(deviceBenchmarking)
		return ok && benchmarking.gpu == gpu
	}) {
		return probeOutcomeControlFailed{}, cpu, nil
	}
	// Discard cold GPU compilation/allocation before comparing warm latency.
	if err := gpu.session.warmup(ids, mask); err != nil {
		slog.Warn("GPU warmup failed; retaining CPU", "error", err.Error())
		return probeOutcomeGPUFailed{}, cpu, nil
	}
	start = time.Now()
	score, err := gpu.session.score(ids, mask)
	gpuTime := time.Since(start)
	switch {
	case err != nil:
		slog.Warn("GPU student inference failed; retaining CPU", "error", err.Error(), "device", string(gpu.device))
		return probeOutcomeGPUFailed{}, cpu, nil
	case math.Abs(cpu-score) > 0.005:
		slog.Warn("GPU student parity failed; retaining CPU", "cpu", cpu, "score", score, "device", string(gpu.device))
		return probeOutcomeGPUFailed{}, cpu, nil
	case gpuTime >= cpuTime:
		return probeOutcomeCPUWins{gpuMs: uint64(gpuTime.Milliseconds()), cpuMs: uint64(cpuTime.Milliseconds())}, cpu, nil
	default:
		return probeOutcomeGPUWins{}, score, nil
	}
}

// supervisorAction is supervise's decision for one turn of its loop.
type supervisorAction interface{ isSupervisorAction() }

type actionPrepare struct{ useGPU bool }

type actionGPU struct {
	engine  *engine
	request *cancellationToken
}

type actionRetry struct{ at time.Time }

type actionWait struct{}

func (actionPrepare) isSupervisorAction() {}
func (actionGPU) isSupervisorAction()     {}
func (actionRetry) isSupervisorAction()   {}
func (actionWait) isSupervisorAction()    {}

// supervise is the owner's preparation loop; it returns when ctx (the
// student's cancellation) ends.
func supervise(ctx context.Context, inner *inner, root string, options preparationOptions) {
	for {
		// Subscribe before reading state so an off/on transition cannot be
		// lost between the decision and waiting for its notification.
		changed, stop := inner.changed.Subscribe()
		var action supervisorAction
		inner.mu.Lock()
		enabled := inner.machine.useGPU
		switch state := inner.machine.state.(type) {
		case stateDormant:
			action = actionWait{}
		case stateLoading:
			action = actionPrepare{useGPU: enabled}
		case stateFailed:
			action = actionRetry{at: state.failure.retryAt}
		case stateReady:
			if _, needed := state.ready.device.(deviceNeeded); needed {
				request := newCancellationToken()
				state.ready.device = devicePreparing{request: request}
				action = actionGPU{engine: state.ready.engine, request: request}
			} else {
				action = actionWait{}
			}
		}
		inner.mu.Unlock()
		switch action := action.(type) {
		case actionWait:
			select {
			case <-ctx.Done():
				stop()
				return
			case <-changed:
			}
			stop()
		case actionRetry:
			stop()
			timer := time.NewTimer(time.Until(action.at))
			select {
			case <-ctx.Done():
				timer.Stop()
				return
			case <-timer.C:
			}
			inner.mu.Lock()
			if failed, ok := inner.machine.state.(stateFailed); ok && failed.failure.retryAt.Equal(action.at) {
				inner.machine.state = stateLoading{}
				inner.mu.Unlock()
				inner.notifyChanged()
			} else {
				inner.mu.Unlock()
			}
		case actionGPU:
			stop()
			if !superviseGPU(ctx, inner, root, options, action) {
				return
			}
		case actionPrepare:
			stop()
			if !supervisePrepare(ctx, inner, root, options, action.useGPU) {
				return
			}
		}
	}
}

// superviseGPU prepares the GPU candidate the owner asked for; false when
// the student's cancellation ended it.
func superviseGPU(ctx context.Context, inner *inner, root string, options preparationOptions, action actionGPU) bool {
	type prepared struct {
		gpu *gpu
		err error
	}
	preparationCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	stopRequest := context.AfterFunc(action.request.ctx, cancel)
	defer stopRequest()
	done := make(chan prepared, 1)
	go func() {
		candidate, err := func() (*gpu, error) {
			client := &http.Client{}
			device, ok := action.engine.preferredDevice()
			if !ok {
				return nil, nil
			}
			directory, err := prepare(preparationCtx, filepath.Join(root, "github-student"),
				options.modelManifest.selected(device == StudentDeviceCoreML), options.modelBase, client)
			if err != nil {
				return nil, err
			}
			loaded := core.SpawnBlocking(func() prepared {
				candidate, err := action.engine.loadGPU(directory, device)
				return prepared{gpu: candidate, err: err}
			})
			result, err := loaded.Join(context.Background())
			if err != nil {
				return nil, err
			}
			return result.gpu, result.err
		}()
		done <- prepared{gpu: candidate, err: err}
	}()
	var result prepared
	select {
	case <-ctx.Done():
		return false
	case <-action.request.ctx.Done():
		return true
	case result = <-done:
	}
	inner.mu.Lock()
	current, ok := inner.machine.state.(stateReady)
	preparing, isPreparing := devicePreparing{}, false
	if ok {
		preparing, isPreparing = current.ready.device.(devicePreparing)
	}
	if !ok || current.ready.engine != action.engine || !isPreparing || preparing.request != action.request {
		// The result is dropped; native handles go with their cleanups.
		inner.mu.Unlock()
		return true
	}
	var notice StudentNotice
	if result.err != nil {
		slog.Warn("GPU preparation failed; retaining CPU", "error", result.err.Error())
		if from, ok := action.engine.preferredDevice(); ok {
			notice = StudentNoticeDeviceFallback{From: from, Reason: StudentDeviceFallbackReasonFailed{Kind: StudentFailureKindLoad}}
		}
	}
	if result.err == nil && result.gpu != nil {
		current.ready.device = devicePending{gpu: result.gpu}
	} else {
		current.ready.device = deviceCPU{}
	}
	inner.mu.Unlock()
	if notice != nil {
		inner.notice(notice)
	}
	return true
}

// supervisePrepare loads the model and runtime; false when the student's
// cancellation ended it.
func supervisePrepare(ctx context.Context, inner *inner, root string, options preparationOptions, useGPU bool) bool {
	type prepared struct {
		ready *ready
		err   error
	}
	done := make(chan prepared, 1)
	go func() {
		loaded, err := func() (*ready, error) {
			// Decide unsupported hosts before starting any model bytes.
			if err := validatePlatform(); err != nil {
				return nil, err
			}
			client := &http.Client{}
			var directory, runtimeLibrary string
			err := tryJoinAll(ctx, []func(ctx context.Context) error{
				func(ctx context.Context) error {
					var err error
					directory, err = prepare(ctx, filepath.Join(root, "github-student"), options.modelManifest.selected(false), options.modelBase, client)
					return err
				},
				func(ctx context.Context) error {
					var err error
					runtimeLibrary, err = prepareSelected(ctx, filepath.Join(root, "github-student-native"), useGPU, inner.notice, client)
					return err
				},
			})
			if err != nil {
				var failure *preparationFailure
				if !errors.As(err, &failure) {
					err = preparationFailureError(StudentFailureKindDownload, err, nil)
				}
				return nil, err
			}
			loading := core.SpawnBlocking(func() prepared {
				if failures := options.loadFailures; failures != nil {
					for {
						remaining := failures.Load()
						if remaining <= 0 {
							break
						}
						if failures.CompareAndSwap(remaining, remaining-1) {
							return prepared{err: errors.New("recorded transient loader failure")}
						}
					}
				}
				engine, err := loadEngine(directory, runtimeLibrary)
				if err != nil {
					return prepared{err: err}
				}
				capacity, err := parallelism()
				if err != nil {
					return prepared{err: err}
				}
				return prepared{ready: &ready{
					engine:  engine,
					permits: make(chan struct{}, capacity),
					device:  deviceCPU{},
				}}
			})
			result, err := loading.Join(context.Background())
			if err != nil {
				return nil, fmt.Errorf("student loading task failed: %w", err)
			}
			return result.ready, result.err
		}()
		done <- prepared{ready: loaded, err: err}
	}()
	var result prepared
	select {
	case <-ctx.Done():
		return false
	case result = <-done:
	}
	inner.mu.Lock()
	if ctx.Err() != nil {
		inner.mu.Unlock()
		return false
	}
	var notice StudentNotice
	if result.err == nil {
		if inner.machine.useGPU {
			result.ready.device = deviceNeeded{}
		} else {
			result.ready.device = deviceCPU{}
		}
		inner.machine.failures = 0
		inner.machine.state = stateReady{ready: result.ready}
	} else {
		var failure *preparationFailure
		if errors.As(result.err, &failure) {
			notice = failure.notice
		}
		innerFail(&inner.machine, StudentFailureKindLoad, result.err)
	}
	inner.mu.Unlock()
	inner.notifyChanged()
	if notice != nil {
		inner.notice(notice)
	}
	return true
}

type preparationFailure struct {
	kind       StudentFailureKind
	cause      string
	retryAfter *time.Duration
	// notice is announced when the failure is recorded; nil for none.
	notice StudentNotice
}

// preparationFailureError is PreparationFailure::error.
func preparationFailureError(kind StudentFailureKind, err error, retryAfter *time.Duration) error {
	return &preparationFailure{kind: kind, cause: err.Error(), retryAfter: retryAfter}
}

func (f *preparationFailure) Error() string {
	return f.cause
}
