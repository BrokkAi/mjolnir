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
	"fmt"
	"sync"
	"sync/atomic"
	"time"

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
	panic("unported: mj-github-relevance/src/github_student.rs Inner::fail")
}

// completed records an inference result from engine (err nil for a
// score). A late result from a replaced engine cannot change the new epoch.
func (i *inner) completed(engine *engine, score float64, err error) {
	panic("unported: mj-github-relevance/src/github_student.rs Inner::completed")
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
	panic("unported: mj-github-relevance/src/github_student.rs Student::set_use_gpu")
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
	panic("unported: mj-github-relevance/src/github_student.rs Student::score_segments")
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
	panic("unported: mj-github-relevance/src/github_student.rs device_transition")
}

func run(inner *inner, engine *engine, work work, item, session string) (float64, error) {
	panic("unported: mj-github-relevance/src/github_student.rs run")
}

// probePair produces one outcome on every ordinary exit; the reservation
// guard supplies ControlFailed if this computation panics instead of
// returning an outcome.
func probePair(inner *inner, engine *engine, gpu *gpu, item, session string) (probeOutcome, float64, error) {
	panic("unported: mj-github-relevance/src/github_student.rs probe_pair")
}

// supervise is the owner's preparation loop; it returns when ctx (the
// student's cancellation) ends.
func supervise(ctx context.Context, inner *inner, root string, options preparationOptions) {
	panic("unported: mj-github-relevance/src/github_student.rs supervise")
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
