// Port of mj-worker/src/worker_runtime/unix.rs.
//
// The Unix worker daemon: it recovers the durable relay, publishes the
// control socket, prepares and starts the harness under its own supervisor,
// and serves relay connections until the session closes or the worker stops.
//
// Rust's tokio primitives map onto Go as follows in this module and its
// submodules: a CancellationToken is a context.Context (observed) together
// with its context.CancelFunc (cancelled); a JoinHandle is a
// *core.JoinHandle, aborted through the cancel function of the context its
// task runs under; a watch channel is a *core.Watch; a TaskTracker is a
// *sync.WaitGroup. The ACP command channel and the dispatch wake channel are
// commandChannel and dispatchWakeChannel below, because their senders must
// see a receiver that is gone, and dispatch must reserve command capacity
// before it claims durable work.

//go:build unix

package worker

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// acpEventChannelCapacity is the ACP runtime's event channel capacity.
const acpEventChannelCapacity = 256

// localPreparationStepTimeout bounds one local preparation step. Cold local
// filesystems can take minutes to traverse under host pressure. Subagent
// profile, project memory, ACP setup, and bridge start each get three
// minutes, with no local preparation stage exceeding five minutes.
const localPreparationStepTimeout = 3 * time.Minute

// acpCommandChannelCapacity is the ACP command channel's capacity (Rust:
// mpsc::channel(32) in build_acp_setup).
const acpCommandChannelCapacity = 32

// preparationSnapshot is what the preparation supervisor publishes to the
// connection handlers: where preparation is, and once Started, the services
// the prepared harness offers.
type preparationSnapshot struct {
	// state is nil (Rust's None) only for handlers with no primary
	// preparation supervisor.
	state    core.HarnessPreparation
	services *preparedConnectionServices
	// stepDeadline is the current step's own bound, which also bounds the
	// threads it starts; zero when there is none.
	stepDeadline time.Time
}

// isPending is whether preparation has not yet published Started.
func (s *preparationSnapshot) isPending() bool {
	_, started := s.state.(core.HarnessPreparationStarted)
	return !started
}

// preparedConnectionServices are the live services a prepared harness offers
// each relay connection.
type preparedConnectionServices struct {
	commands  *commandChannel
	reviewer  *reviewCapture
	subagents *SubagentEndpoint
}

// runningHarness is a started bridge and its relay coordinator.
type runningHarness struct {
	acpTask *core.JoinHandle[error]
	// eventTask is the relay coordinator; eventAbort aborts it.
	eventTask           *core.JoinHandle[error]
	eventAbort          context.CancelFunc
	acpShutdown         context.CancelFunc
	commands            *commandChannel
	reviewer            *reviewCapture
	subagentSocketGuard *subagentSocketGuard
	subagents           *SubagentEndpoint
	// harnessGC aborts the managed harness garbage collector; nil when none
	// runs.
	harnessGC    context.CancelFunc
	shellCleanup *sync.WaitGroup
	// managedHarness and managedNode are runtime leases kept in long-lived
	// ACP state so they cover supervisor replacement gaps as well as the
	// lifetime of any individual supervisor.
	managedHarness *core.ManagedHarness
	managedNode    *core.ManagedNodeRuntime
	// projectTools is stopped when the harness ends, before a closed session
	// is checkpointed.
	projectTools *backgroundInstall
}

// preparationOutcome is how a harness preparation ended. There is no failed
// state to serve: a harness that cannot start ends the worker.
type preparationOutcome interface{ isPreparationOutcome() }

// preparationOutcomeStarted is a started harness.
type preparationOutcomeStarted struct{ Value *runningHarness }

// preparationOutcomeCancelled is a Close that sealed the relay, or a worker
// stopping, before the bridge started. Value keeps request-side dispatch
// wakes deliverable while the sealed relay is served without a harness; nil
// when preparation no longer owned them.
type preparationOutcomeCancelled struct{ Value *dispatchWakeChannel }

// preparationOutcomeFailed is a harness that cannot start. The error carries
// a core.WorkerHarnessStartupFailure for the exit record.
type preparationOutcomeFailed struct{ Value error }

func (preparationOutcomeStarted) isPreparationOutcome()   {}
func (preparationOutcomeCancelled) isPreparationOutcome() {}
func (preparationOutcomeFailed) isPreparationOutcome()    {}

// preparationStop is why supervisePreparation did not return a prepared
// value; nil means it did.
type preparationStop interface{ isPreparationStop() }

type preparationStopCancelled struct{}

type preparationStopFailed struct{ Value error }

func (preparationStopCancelled) isPreparationStop() {}
func (preparationStopFailed) isPreparationStop()    {}

// harnessPreparationSetup is everything the harness preparation owns.
type harnessPreparationSetup struct {
	root                    string
	config                  core.WorkerLaunchConfig
	relay                   *RelayCell
	sessionGitConfigInclude *string
	credentials             credentialEndpointResult
	kimiTaskHome            *kimiHome
	resumeSession           *string
	nativeSessionHistory    NativeSessionHistory
	// capture is what a sub-agent's worker observes at the end of each turn.
	capture *WorkObserver
}

// credentialEndpointResult is Rust's Result<CredentialEndpoint, String>: the
// endpoint this relay's credential and skills requests use, or why the launch
// config cannot locate harness credentials.
type credentialEndpointResult struct {
	endpoint CredentialEndpoint
	// err is set when there is no endpoint; its text is the reason.
	err error
}

// kimiHome is Rust's Result<PathBuf, String> for the Kimi home the task
// monitor reads: the path, or why it cannot be located.
type kimiHome struct {
	path string
	// err is set when there is no path; its text is the reason.
	err error
}

// preparationStepBudget is one preparation step's deadline.
type preparationStepBudget struct {
	step     string
	deadline time.Time
	timeout  time.Duration
}

func newPreparationStepBudget(step string, timeout time.Duration) preparationStepBudget {
	return preparationStepBudget{step: step, deadline: time.Now().Add(timeout), timeout: timeout}
}

func (b *preparationStepBudget) remaining() time.Duration {
	return max(time.Until(b.deadline), 0)
}

// deadlineError reports the step exceeding its deadline, with the timeout in
// Rust's Duration Debug form.
func (b *preparationStepBudget) deadlineError() error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs PreparationStepBudget::deadline_error")
}

// acpPreparationSetup is what building the ACP setup consumes.
type acpPreparationSetup struct {
	root                 string
	config               core.WorkerLaunchConfig
	sessionEnvironment   map[string]string
	supervisorSpec       AcpSupervisorSpec
	relay                *RelayCell
	resumeSession        *string
	nativeSessionHistory NativeSessionHistory
	profileRegistration  bool
	// managedSubagent is read from the launch before preparation strips the
	// marker from the harness environment, so the ACP runtime knows the
	// session is a child.
	managedSubagent bool
	subagentRole    *core.SubagentMcpRole
	// runtime is Rust's tokio runtime Handle the bridge tasks are spawned on.
	runtime          *core.Runtime
	managedCacheRoot *string
}

// preparedAcpSetup is a built ACP setup, ready for its bridge to start.
type preparedAcpSetup struct {
	commandsTx          *commandChannel
	commandsRx          <-chan CommandRequest
	eventsTx            chan<- core.RuntimeEvent
	eventsRx            <-chan core.RuntimeEvent
	userShells          *UserShellRegistry
	reviewer            *reviewCapture
	subagents           *SubagentEndpoint
	subagentSocketGuard *subagentSocketGuard
	acpSpec             LaunchSpec
	shellCleanup        *sync.WaitGroup
	managedCacheRoot    *string
	harness             core.HarnessKind
}

// startedBridgeTasks holds a bridge's tasks while it starts. Close (Rust's
// Drop) stops whatever intoRunning has not taken.
type startedBridgeTasks struct {
	acpShutdown         context.CancelFunc
	acpTask             *core.JoinHandle[error]
	eventTask           *core.JoinHandle[error]
	eventAbort          context.CancelFunc
	harnessGC           context.CancelFunc
	commands            *commandChannel
	reviewer            *reviewCapture
	subagentSocketGuard *subagentSocketGuard
	subagents           *SubagentEndpoint
	shellCleanup        *sync.WaitGroup
}

// intoRunning hands the started tasks to a runningHarness.
func (t *startedBridgeTasks) intoRunning() *runningHarness {
	if t.acpShutdown == nil || t.acpTask == nil {
		panic("ACP task was started")
	}
	if t.eventTask == nil {
		panic("relay coordinator was started")
	}
	if t.commands == nil {
		panic("ACP command sender was prepared")
	}
	if t.reviewer == nil {
		panic("reviewer was prepared")
	}
	if t.shellCleanup == nil {
		panic("shell tracker was prepared")
	}
	running := &runningHarness{
		acpShutdown: t.acpShutdown, acpTask: t.acpTask,
		eventTask: t.eventTask, eventAbort: t.eventAbort,
		harnessGC: t.harnessGC, commands: t.commands, reviewer: t.reviewer,
		subagentSocketGuard: t.subagentSocketGuard, subagents: t.subagents,
		shellCleanup: t.shellCleanup,
	}
	*t = startedBridgeTasks{}
	return running
}

// Close is the Drop of startedBridgeTasks: it stops every task not yet handed
// to a runningHarness.
func (t *startedBridgeTasks) Close() {
	if t.acpShutdown != nil {
		t.acpShutdown()
	}
	if t.eventAbort != nil {
		t.eventAbort()
	}
	if t.harnessGC != nil {
		t.harnessGC()
	}
	t.acpTask, t.eventTask, t.eventAbort, t.harnessGC = nil, nil, nil, nil
}

// buildACPSetup writes the ACP supervisor spec and builds the launch the
// bridge starts from.
func buildACPSetup(setup acpPreparationSetup) (*preparedAcpSetup, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs build_acp_setup")
}

// publishStarted hands relay-local lifecycle work from the request handler
// to the coordinator about to start, or refuses because a Close sealed the
// relay first. Returns the dispatch wakes the coordinator will own.
//
// The relay lock is the one decision point between this and a Close the
// request handler accepts during preparation (see serveClientWithMemory).
// Started is published inside it, and only while preparation is not
// cancelled and the relay is not sealed. A handler that saw Preparing
// therefore completed the Close and cancelled a preparation that never
// starts a bridge, and one that sees Started leaves the Close to the
// coordinator.
//
// A seal found here cancels the preparation itself, under the same lock: the
// handler that accepted the Close may not have cancelled it yet, and a worker
// that booted with a closing relay has no handler to do it. The cancelled
// token (ctx, cancelled through cancel) is then the one fact that makes this
// refusal a cancellation rather than a startup failure. On refusal the wakes
// stay with the preparation for the sealed relay.
func publishStarted(ctx context.Context, cancel context.CancelFunc, relay *RelayCell, status *core.Watch[preparationSnapshot], dispatchWakes **dispatchWakeChannel, services preparedConnectionServices) (*dispatchWakeChannel, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs publish_started")
}

// startBridge starts the bridge and its coordinator once publishStarted
// accepts the handoff. Started is published before any bridge process
// exists, and nothing after the publication can fail, so a Started worker
// always has a coordinator. ctx and cancel are the preparation's
// cancellation token.
func startBridge(ctx context.Context, cancel context.CancelFunc, setup *preparedAcpSetup, relay *RelayCell, dispatchWakes **dispatchWakeChannel, kimiTaskHome *kimiHome, capture *WorkObserver, runtime *core.Runtime, status *core.Watch[preparationSnapshot]) (*startedBridgeTasks, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs start_bridge")
}

// abortTaskOnDrop aborts a task when closed (Rust's Drop).
type abortTaskOnDrop context.CancelFunc

func (a abortTaskOnDrop) Close() { a() }

// cancelPreparationOnDrop cancels a preparation when closed (Rust's Drop).
type cancelPreparationOnDrop context.CancelFunc

func (c cancelPreparationOnDrop) Close() { c() }

// preparationStep publishes a new preparation step and records its startup
// breadcrumb within the step's budget. ctx is the preparation's cancellation
// token.
func preparationStep(ctx context.Context, root string, status *core.Watch[preparationSnapshot], step string, timeout time.Duration) (preparationStepBudget, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs preparation_step")
}

// preparationError is why operation is unavailable while the harness
// prepares; false when it is available.
func preparationError(snapshot *preparationSnapshot, operation string) (string, bool) {
	// Standalone/reviewer socket handlers have no primary preparation
	// supervisor (nil); their explicit services are already available. This
	// worker never reports Failed: a harness that cannot start ends the
	// worker instead.
	preparing, ok := snapshot.state.(core.HarnessPreparationPreparing)
	if !ok {
		return "", false
	}
	return fmt.Sprintf("harness is preparing at %s; %s is not available yet", preparing.Step, operation), true
}

// waitForChange holds a WaitForChange until the relay's latest event or
// operational state, or this connection's sub-agent queue, differs from what
// the controller names, or until core.RelayChangeWaitBound passes. The state
// is built as an attach builds it, preparation included, so its digest is
// the one the controller holds.
func waitForChange(ctx context.Context, relay *RelayCell, preparation *core.Watch[preparationSnapshot], subagents *SubagentEndpoint, feed *subagentFeed, afterOrdinal uint64, stateDigest *string) (core.RelayResponseBody, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs wait_for_change")
}

// overlayPreparationState puts the preparation into a status reply. Attach
// replies carry the preparation already; see handleRequest.
func overlayPreparationState(response *core.RelayResponseEnvelope, snapshot *preparationSnapshot) {
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		return
	}
	status, isStatus := ok.Payload.(core.RelayResponsePayloadStatus)
	if !isStatus {
		return
	}
	status.State.HarnessPreparation = snapshot.state
	response.Body = core.RelayResponseBodyOk{Payload: status}
}

func unavailableRequest(envelope core.RelayRequestEnvelope, message string) core.RelayResponseEnvelope {
	return core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            compactionError(core.RelayErrorCodeInvalidState, message),
	}
}

// projectMemoryEndpoint is a connection's project memory and history
// service.
type projectMemoryEndpoint struct {
	config *core.ProjectMemoryLaunchConfig
	// io serializes project memory I/O: a one-slot semaphore, held by
	// sending into it.
	io chan struct{}
	// ioRequested, ioDisconnected and ioBlock are test instrumentation
	// (Rust: cfg(test) fields). The two notifications hold one pending
	// signal each, as tokio's Notify::notify_one does.
	ioRequested    chan struct{}
	ioDisconnected chan struct{}
	ioBlock        *projectMemoryIoBlock
	history        *historyEndpoint
	cwd            string
}

// projectMemoryIoBlock holds a project memory operation in tests (Rust:
// cfg(test)).
type projectMemoryIoBlock struct {
	// started holds one pending signal once the blocked operation starts.
	started chan struct{}
	// release lets the blocked operation go on, or fail with the error sent.
	release           <-chan error
	panicAfterRelease bool
}

func newProjectMemoryEndpoint(config *core.ProjectMemoryLaunchConfig) projectMemoryEndpoint {
	return projectMemoryEndpoint{
		config:         config,
		io:             make(chan struct{}, 1),
		ioRequested:    make(chan struct{}, 1),
		ioDisconnected: make(chan struct{}, 1),
		history:        defaultHistoryEndpoint(),
	}
}

func defaultProjectMemoryEndpoint() projectMemoryEndpoint {
	return newProjectMemoryEndpoint(nil)
}

// socketGuard removes the control socket it names when closed (Rust's Drop).
type socketGuard string

func (g socketGuard) Close() {
	if err := removeControlSocket(string(g), "shutdown"); err != nil {
		slog.Warn("control socket shutdown cleanup failed", "error", err, "socket", string(g))
	}
}

// logControlSocket logs a control socket operation with both sides of it:
// overlay filesystems can report a successful operation while subsequent
// path lookup still exposes an old inode.
func logControlSocket(path, operation string, err error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs log_control_socket")
}

func removeControlSocket(path, phase string) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs remove_control_socket")
}

// publishControlSocket binds `control.sock`, restricts it to the owner, and
// records the `bind-socket` and `serving` startup steps. The caller must
// accept on the listener promptly: the daemon reads the socket's existence
// as "this worker answers".
func publishControlSocket(root, socket string) (*net.UnixListener, socketGuard, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs publish_control_socket")
}

// writeWorkerPidfile records this daemon's PID where session teardown can
// find it. Teardown must stop the daemon before deleting the worker root;
// without this file it can only guess from process command lines.
func writeWorkerPidfile(root string, pid uint32) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs write_worker_pidfile")
}

// RunDaemon acquires the worker root and runs the worker daemon in it.
func RunDaemon(ctx context.Context, root string, config core.WorkerLaunchConfig) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs run_daemon")
}

// RunDaemonOwned runs the worker daemon in a root it owns.
func RunDaemonOwned(ctx context.Context, owner *WorkerRootOwner, config core.WorkerLaunchConfig) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs run_daemon_owned")
}

// acceptWorkerClients serves every relay connection on listener until
// accepting fails.
func acceptWorkerClients(ctx context.Context, listener *net.UnixListener, socketGuard socketGuard, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, runtime connectionRuntime, fatal chan<- error) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs accept_worker_clients")
}

// boundedPreparationStep runs operation within the step's remaining budget.
func boundedPreparationStep[T any](ctx context.Context, budget *preparationStepBudget, operation func(ctx context.Context) (T, error)) (T, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs bounded_preparation_step")
}

// boundedBlockingPreparationStep runs blocking operation on its own
// goroutine within the step's budget. ctx is the preparation's cancellation
// token; operation gets a child of it that is cancelled when the step is
// cancelled or times out.
//
// Synchronous filesystem/socket calls cannot be forcibly interrupted. On
// timeout or cancellation this stops waiting for the goroutine; the
// preparation's thread tracker (see supervisePreparation) still counts it,
// so a cancelled preparation waits for it and a failed one ends the worker,
// which ends the goroutine.
func boundedBlockingPreparationStep[T any](ctx context.Context, budget *preparationStepBudget, operation func(stepCtx context.Context) (T, error)) (T, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs bounded_blocking_preparation_step")
}

// superviseHarnessPreparation prepares and starts the harness, owning the
// dispatch wakes until a coordinator takes them. ctx and cancel are the
// preparation's cancellation token.
func superviseHarnessPreparation(ctx context.Context, cancel context.CancelFunc, setup harnessPreparationSetup, dispatchWakes *dispatchWakeChannel, status *core.Watch[preparationSnapshot]) preparationOutcome {
	panic("unported: mj-worker/src/worker_runtime/unix.rs supervise_harness_preparation")
}

// supervisePreparation runs one harness preparation to an end it owns:
// prepared, cancelled, or failed with the typed startup failure the worker
// exits with. ctx is the preparation's cancellation token.
//
// Commands the preparation runs through the shared subprocess helpers stop
// when it is cancelled or ends, including those on blocking threads, and
// every thread it hands to the blocking pool is counted until it returns. A
// failed preparation lets those threads stop their commands, within the
// failed step's own bound, before the worker exits; the exit ends any thread
// still blocked. A cancelled one returns at once: either the worker is
// stopping, or it keeps serving a sealed relay while the cancelled threads
// finish under their step's bound, and the controller stops the worker
// before it removes any of its files.
func supervisePreparation[T any](ctx context.Context, root string, status *core.Watch[preparationSnapshot], preparation func(ctx context.Context) (T, error)) (T, preparationStop) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs supervise_preparation")
}

// catchPreparationPanic runs preparation, reporting a panic as an error.
func catchPreparationPanic[T any](preparation func() T) (value T, err error) {
	defer func() {
		if payload := recover(); payload != nil {
			err = fmt.Errorf("harness preparation panicked: %s", panicPayloadMessage(payload))
		}
	}()
	return preparation(), nil
}

func panicPayloadMessage(payload any) string {
	switch message := payload.(type) {
	case string:
		return message
	case error:
		return message.Error()
	default:
		return "non-string panic payload"
	}
}

// prepareProjectTools is the fast part of project tools. Whatever mise
// reports missing is installed by the returned projectToolsInstall in the background;
// the agent has already been told, so the harness starts at once. Returns
// whether the project tools environment was applied.
func prepareProjectTools(ctx context.Context, root string, roots []string, config *core.WorkerLaunchConfig, baseEnvironment map[string]string, status *core.Watch[preparationSnapshot], relay *RelayCell) (bool, *projectToolsInstall, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs prepare_project_tools")
}

// prepareAndStartHarness prepares the session's harness and starts its
// bridge. ctx and cancel are the preparation's cancellation token.
func prepareAndStartHarness(ctx context.Context, cancel context.CancelFunc, setup harnessPreparationSetup, dispatchWakes **dispatchWakeChannel, status *core.Watch[preparationSnapshot]) (*runningHarness, error) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs prepare_and_start_harness")
}

// stopPreparation stops a preparation the worker no longer wants, including
// a harness it started just before the stop.
func stopPreparation(ctx context.Context, cancel context.CancelFunc, task *core.JoinHandle[preparationOutcome]) {
	cancel()
	outcome, err := task.Join(context.Background())
	if err != nil {
		return
	}
	if started, ok := outcome.(preparationOutcomeStarted); ok {
		shutdownRunningHarness(ctx, started.Value)
	}
}

func shutdownRunningHarness(ctx context.Context, running *runningHarness) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs shutdown_running_harness")
}

// PrepareManagedHarness installs or validates the managed harness named by a
// proposed launch config without starting, stopping, or otherwise touching
// the session worker.
//
// The preparation owns the runtime lease for the whole call, so preparation
// and the supervisor that later uses the same runtime never overlap.
func PrepareManagedHarness(ctx context.Context, config core.WorkerLaunchConfig) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs prepare_managed_harness")
}

// serveTerminalRelay serves a sealed or checkpoint-only relay, which has no
// harness, until a durable failure is reported.
func serveTerminalRelay(ctx context.Context, listener *net.UnixListener, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, runtime connectionRuntime, fatal chan<- error, fatalReports <-chan error) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs serve_terminal_relay")
}

// connectionRuntime is the live services one relay connection can reach
// beyond the durable relay: everything answered on the connection instead of
// being journaled.
type connectionRuntime struct {
	projectMemory projectMemoryEndpoint
	// commands is the ACP coordinator's command channel, or nil once the
	// session is sealed and no ACP runtime is left to serve scratch prompts.
	commands *commandChannel
	// reviewer is capture state prepared with the primary harness. A sealed
	// session has none.
	reviewer    *reviewCapture
	subagents   *SubagentEndpoint
	cpu         *core.Watch[cpuRead]
	preparation *core.Watch[preparationSnapshot]
	// preparationCancel cancels the harness preparation; nil without one.
	preparationCancel context.CancelFunc
	hasSubagentTools  bool
}

func defaultConnectionRuntime() connectionRuntime {
	return connectionRuntime{projectMemory: defaultProjectMemoryEndpoint()}
}

// serveClient serves one connection with only an ACP command channel (Rust:
// cfg(test)).
func serveClient(ctx context.Context, stream *net.UnixConn, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, commands *commandChannel, fatal chan<- error) error {
	runtime := defaultConnectionRuntime()
	runtime.commands = commands
	return serveClientWithMemory(ctx, stream, relay, dispatchWake, credentials, runtime, fatal)
}

// serveClientWithMemory serves one relay connection until it closes.
func serveClientWithMemory(ctx context.Context, stream *net.UnixConn, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, runtime connectionRuntime, fatal chan<- error) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs serve_client_with_memory")
}

// dispatchWakeChannel is the relay's dispatch wake channel (Rust:
// mpsc::channel::<()>(1)). A wake only says durable work may now be
// available, so a full channel already carries it. The coordinator owns the
// receiving side and closes it when it stops, which senders observe.
type dispatchWakeChannel struct {
	wakes     chan struct{}
	closed    chan struct{}
	closeOnce sync.Once
}

// errDispatchWakeClosed is a wake sent after the receiver closed (Rust:
// TrySendError::Closed).
var errDispatchWakeClosed = errors.New("channel closed")

func newDispatchWakeChannel() *dispatchWakeChannel {
	return &dispatchWakeChannel{wakes: make(chan struct{}, 1), closed: make(chan struct{})}
}

// trySend wakes the receiver without waiting. A full channel is success: the
// pending wake says the same thing.
func (w *dispatchWakeChannel) trySend() error {
	select {
	case <-w.closed:
		return errDispatchWakeClosed
	default:
	}
	select {
	case w.wakes <- struct{}{}:
	default:
	}
	return nil
}

// receive is the receiving side.
func (w *dispatchWakeChannel) receive() <-chan struct{} { return w.wakes }

// closeReceiver is Rust's drop of the receiver: later sends fail.
func (w *dispatchWakeChannel) closeReceiver() {
	w.closeOnce.Do(func() { close(w.closed) })
}

// commandChannel is the ACP command channel (Rust: mpsc::channel of
// CommandRequest with capacity acpCommandChannelCapacity). Every sender is in
// this runtime and the ACP runtime receives from receiver(). Dispatch
// reserves capacity before it claims durable work, so out-of-band senders
// (compaction, elicitation answers) compete for the same slots instead of
// stealing one a claim already counted on. A command counts against the
// capacity until the receiver takes it.
type commandChannel struct {
	mu       sync.Mutex
	capacity int
	// queue is the accepted commands not yet taken by the receiver.
	queue []CommandRequest
	// reserved counts outstanding permits.
	reserved int
	// changed is closed and replaced whenever a slot frees, a command is
	// queued, or the receiver closes.
	changed chan struct{}
	// out delivers the queue's head to the receiver.
	out chan CommandRequest
	// closed is closed when the receiver is gone.
	closed    chan struct{}
	closeOnce sync.Once
}

// commandPermit is one reserved slot of a commandChannel (Rust:
// mpsc::Permit). Exactly one of send or release must be called.
type commandPermit struct {
	channel *commandChannel
}

var (
	// errCommandChannelFull is Rust's TrySendError::Full.
	errCommandChannelFull = errors.New("no available capacity")
	// errCommandChannelClosed is Rust's TrySendError::Closed and SendError.
	errCommandChannelClosed = errors.New("channel closed")
)

func newCommandChannel(capacity int) *commandChannel {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandChannel (tokio mpsc)")
}

// receiver is the receiving side, which the ACP runtime drains.
func (c *commandChannel) receiver() <-chan CommandRequest { return c.out }

// closeReceiver is Rust's drop of the receiver: queued commands are dropped
// and later sends fail.
func (c *commandChannel) closeReceiver() {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandChannel::close_receiver")
}

// availableCapacity is how many commands can be sent without waiting (Rust:
// Sender::capacity).
func (c *commandChannel) availableCapacity() int {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandChannel::capacity")
}

// tryReserve reserves one slot; false when the channel is full or closed.
func (c *commandChannel) tryReserve() (commandPermit, bool) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandChannel::try_reserve")
}

// trySend sends without waiting: errCommandChannelFull or
// errCommandChannelClosed when it cannot.
func (c *commandChannel) trySend(request CommandRequest) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandChannel::try_send")
}

// send waits for a slot; errCommandChannelClosed once the receiver is gone.
func (c *commandChannel) send(ctx context.Context, request CommandRequest) error {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandChannel::send")
}

// send queues request in the reserved slot.
func (p commandPermit) send(request CommandRequest) {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandPermit::send")
}

// release gives the reserved slot back unused.
func (p commandPermit) release() {
	panic("unported: mj-worker/src/worker_runtime/unix.rs commandPermit::release")
}
