// Port of mj-worker/src/worker_runtime/unix/dispatch.rs.
//
// The relay coordinator: it records the ACP runtime's events into the
// durable relay and dispatches claimed commands to the runtime.

//go:build unix

package worker

import (
	"context"
	"log/slog"
	"runtime"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sessionConfiguration is where the harness is in configuring its session.
type sessionConfiguration int

const (
	sessionConfigurationOpening sessionConfiguration = iota
	sessionConfigurationReady
	sessionConfigurationReopening
)

func (c sessionConfiguration) isReady() bool { return c == sessionConfigurationReady }

// harnessStartup is the harness's startup deadline. Only the coordinator
// accepts readiness or expiry. A ready bridge removes its timer; a restart
// arms a new one, and expiry ends the worker with a typed startup failure.
type harnessStartup struct {
	// deadline is zero when no startup is pending.
	deadline time.Time
}

func newHarnessStartup() harnessStartup {
	return harnessStartup{deadline: time.Now().Add(core.NativeSessionStartupTimeout)}
}

func (s *harnessStartup) observeEvent(restarting, ready bool) {
	if restarting {
		s.deadline = time.Now().Add(core.NativeSessionStartupTimeout)
	}
	if ready {
		s.deadline = time.Time{}
	}
}

// runRelayCoordinatorWithShells runs the coordinator of a started harness,
// with the completed-turn classifier this worker resolves for itself.
func runRelayCoordinatorWithShells(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, dispatchWakes *dispatchWakeChannel, commands *commandChannel, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, capture *WorkObserver, startup harnessStartup) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs run_relay_coordinator_with_shells")
}

// runRelayCoordinatorWithVerdict runs the coordinator with the given
// classifier and no startup deadline (Rust: cfg(test)). The coordinator owns
// one handle to each thing it serves; a struct of them would only rename the
// arguments.
func runRelayCoordinatorWithVerdict(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, dispatchWakes *dispatchWakeChannel, commands *commandChannel, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, verdict *VerdictClient, capture *WorkObserver) error {
	return runRelayCoordinator(ctx, relay, events, dispatchWakes, commands, userShells, kimiTasks, verdict, capture, nil)
}

// runRelayCoordinator records runtime events and dispatches commands until
// the ACP runtime stops, the harness fails to start, or ctx is cancelled
// (Rust: the task is aborted).
func runRelayCoordinator(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, dispatchWakes *dispatchWakeChannel, commands *commandChannel, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, verdict *VerdictClient, capture *WorkObserver, startup *harnessStartup) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs run_relay_coordinator")
}

// recoverAuthentication consults the same identity that credential
// installation publishes. This is supplemental evidence; neither dispatch
// nor recovery waits on filesystem I/O.
func recoverAuthentication(relay *RelayCell) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs recover_authentication")
}

// DispatchCapture is what observing a sub-agent's finishing prompt needs
// from its coordinator: the observer, and the channel that says a command
// has been accepted. (Rust: Capture, renamed because work_facts and
// move_workspace also declare a Capture.)
type DispatchCapture struct {
	observer *WorkObserver
	wakes    *dispatchWakeChannel
}

// captureWhileOpen observes a sub-agent's work for the prompt that is
// finishing, together with whatever else must happen before its outcome is
// published (for Kimi, waiting for the turn's native records to settle): the
// time added is the slowest of them, not their sum. The next turn cannot
// start meanwhile: the next prompt is dispatched by this task after the
// outcome is recorded.
//
// There is no timeout of ours. The observation ends by itself, or is
// cancelled by what really ends a turn early: the harness stopping or
// restarting, its event channel closing, or the relay holding a cancellation
// or a Close it has not yet delivered (this task is what dispatches those,
// so they would otherwise wait behind a Git read that does not return;
// dropping the observation kills its Git children). A park waits for the
// report, which waits for the outcome. The outcome is then published with
// the facts unavailable and the reason. Events that arrive while it runs
// belong after the outcome, and are kept, in order, in deferred. Wakes
// consumed here need no replay: the caller dispatches pending commands once
// the outcome is recorded.
func captureWhileOpen(ctx context.Context, capture *DispatchCapture, relay *RelayCell, events <-chan core.RuntimeEvent, deferred *[]core.RuntimeEvent) core.WorkFacts {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs capture_while_open")
}

// recordOneRuntimeEvent prepares, observes (for a sub-agent's finishing
// prompt) and records one runtime event, then the events that arrived while
// it was observed.
func recordOneRuntimeEvent(ctx context.Context, relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent, events <-chan core.RuntimeEvent, sessionConfiguration *sessionConfiguration, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, capture *DispatchCapture, startup *harnessStartup) (bool, error) {
	return recordEventsInOrder(ctx, relay, inFlight, []core.RuntimeEvent{event}, events, sessionConfiguration, userShells, kimiTasks, capture, startup)
}

// recordEventsInOrder records queue in order. What arrives while one event
// is observed belongs after it, and is itself a later event of the same
// kind: it is judged by the same rule, so an autonomous turn that ends
// inside an observation is observed too.
func recordEventsInOrder(ctx context.Context, relay *RelayCell, inFlight map[string]core.RelayCommand, queue []core.RuntimeEvent, events <-chan core.RuntimeEvent, sessionConfiguration *sessionConfiguration, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, capture *DispatchCapture, startup *harnessStartup) (bool, error) {
	for len(queue) > 0 {
		event := queue[0]
		queue = queue[1:]
		var deferred []core.RuntimeEvent
		stopped, err := observeAndRecord(ctx, relay, inFlight, event, events, &deferred, sessionConfiguration, userShells, kimiTasks, capture, startup)
		if err != nil {
			return false, err
		}
		if stopped {
			return true, nil
		}
		queue = append(queue, deferred...)
	}
	return false, nil
}

// observeAndRecord applies one rule for "a managed child's turn ended": a
// prompt of ours finishing, or the harness ending a turn it started on its
// own. Such an event is observed before it is recorded.
func observeAndRecord(ctx context.Context, relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent, events <-chan core.RuntimeEvent, deferred *[]core.RuntimeEvent, sessionConfiguration *sessionConfiguration, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, capture *DispatchCapture, startup *harnessStartup) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs observe_and_record")
}

func recordRuntimeEventWithStartup(relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent, sessionConfiguration *sessionConfiguration, workFacts *core.WorkFacts, startup *harnessStartup) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs record_runtime_event_with_startup")
}

// recordRuntimeEventBatch records the complete batch already emitted by the
// ACP runtime before admitting a checkpoint barrier. A command event may
// itself materialize several durable observations, and queued notification
// events belong to the cut ahead of any waiting barrier.
func recordRuntimeEventBatch(ctx context.Context, relay *RelayCell, inFlight map[string]core.RelayCommand, first core.RuntimeEvent, events <-chan core.RuntimeEvent, sessionConfiguration *sessionConfiguration, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, capture *DispatchCapture, startup *harnessStartup) (bool, error) {
	stopped, err := recordOneRuntimeEvent(ctx, relay, inFlight, first, events, sessionConfiguration, userShells, kimiTasks, capture, startup)
	if err != nil || stopped {
		return stopped, err
	}
	return recordQueuedRuntimeEvents(ctx, relay, inFlight, events, sessionConfiguration, userShells, kimiTasks, len(events), capture, startup)
}

// recordQueuedRuntimeEvents records at most maximum events already queued,
// without waiting for more. A closed event channel interrupts the commands
// in flight and stops the coordinator.
func recordQueuedRuntimeEvents(ctx context.Context, relay *RelayCell, inFlight map[string]core.RelayCommand, events <-chan core.RuntimeEvent, sessionConfiguration *sessionConfiguration, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, maximum int, capture *DispatchCapture, startup *harnessStartup) (bool, error) {
	for recorded := 0; recorded < maximum; recorded++ {
		select {
		case event, open := <-events:
			if !open {
				if err := interruptInFlight(relay, inFlight, "ACP runtime stopped before the command completed"); err != nil {
					return false, err
				}
				return true, nil
			}
			stopped, err := recordOneRuntimeEvent(ctx, relay, inFlight, event, events, sessionConfiguration, userShells, kimiTasks, capture, startup)
			if err != nil || stopped {
				return stopped, err
			}
			if (recorded+1)%256 == 0 {
				runtime.Gosched()
			}
		default:
			return false, nil
		}
	}
	return false, nil
}

func trackUserShellCompletion(userShells *UserShellRegistry, event core.RuntimeEvent) {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs track_user_shell_completion")
}

// recordRuntimeEvent records one runtime event with no work facts (Rust:
// cfg(test)).
func recordRuntimeEvent(relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent) (bool, error) {
	return recordRuntimeEventWithFacts(relay, inFlight, event, nil)
}

// recordRuntimeEventWithFacts records one runtime event. workFacts is what
// the worker observed of its agent's work for a finishing prompt, published
// with that prompt's outcome. Reports whether the runtime stopped.
func recordRuntimeEventWithFacts(relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent, workFacts *core.WorkFacts) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs record_runtime_event_with_facts")
}

func interruptInFlight(relay *RelayCell, inFlight map[string]core.RelayCommand, message string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs interrupt_in_flight")
}

func interruptInFlightLocked(relay *DurableRelay, inFlight map[string]core.RelayCommand, message string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs interrupt_in_flight_locked")
}

// dispatchPending hands durable work to the ACP runtime through capacity
// reserved before the claim, so dispatch never waits. The command channel is
// shared with out-of-band senders (compaction, elicitation answers); they
// compete for the same permits instead of stealing capacity a claim already
// counted on. That matters because a coordinator parked on a send stops
// draining ACP's bounded event channel, which stops the runtime that would
// have drained these commands: a cycle nothing breaks.
//
// This function deliberately never waits. A reserved permit carries no
// durable state, so reserve -> durable claim -> permit send keeps the
// claim-before-dispatch contract: a crash between the claim and the send
// leaves the command in flight for restart interruption.
func dispatchPending(relay *RelayCell, commands *commandChannel, inFlight map[string]core.RelayCommand, sessionConfigured bool, userShells *UserShellRegistry) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs dispatch_pending")
}

func dispatchUserShells(relay *RelayCell, userShells *UserShellRegistry) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs dispatch_user_shells")
}

// acpCommand is the ACP command a claimed relay command becomes; nil for a
// relay-local command.
func acpCommand(claimed *core.ClaimedRelayCommand) CommandRequest {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs acp_command")
}

// selectResumeSession is which native thread this worker should try to
// resume: always the identity the journal recorded last, if there is one.
// The launch configuration can still name a thread this session already
// replaced. ACP decides how to open that identity using
// DurableRelay.nativeSessionHistory as evidence.
func selectResumeSession(config *core.WorkerLaunchConfig, relay *DurableRelay) *string {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs select_resume_session")
}

// recordImportedNativeIdentity records that a native identity which arrived
// with the launch configuration was created somewhere other than this
// journal, which therefore cannot show what the thread contains. It is
// recorded durably before the thread is used again, so a later resume
// failure can never be answered by replacing it. The one exception is a
// relay restored from a checkpoint with no visible prompt on the thread
// (I2-7). That hint allows the typed missing-session fallback after a reload
// attempt.
func recordImportedNativeIdentity(config *core.WorkerLaunchConfig, relay *DurableRelay) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs record_imported_native_identity")
}

// CheckpointChange is how a request changes the checkpoint barriers a
// connection owns.
type CheckpointChange interface{ isCheckpointChange() }

// CheckpointChangeBegin is a barrier this connection now owns.
type CheckpointChangeBegin struct{ Value string }

// CheckpointChangeEnded is a barrier that no longer belongs to this
// connection, whether it ended through a full completion or an early
// dispatch release.
type CheckpointChangeEnded struct{ Value string }

func (CheckpointChangeBegin) isCheckpointChange() {}
func (CheckpointChangeEnded) isCheckpointChange() {}

// checkpointChange is the change request makes, or nil.
func checkpointChange(request core.RelayRequest) CheckpointChange {
	switch request := request.(type) {
	case core.RelayRequestReserveIdle:
		return CheckpointChangeBegin{Value: request.CommandID}
	case core.RelayRequestSubmit:
		switch command := request.Command.(type) {
		case core.RelayCommandBeginCheckpoint:
			return CheckpointChangeBegin{Value: request.CommandID}
		case core.RelayCommandCompleteCheckpoint:
			return CheckpointChangeEnded{Value: command.BarrierCommandID}
		case core.RelayCommandReleaseCheckpoint:
			return CheckpointChangeEnded{Value: command.BarrierCommandID}
		}
	}
	return nil
}

// reportFatal logs a durable failure and hands it to the daemon, which
// stops.
func reportFatal(fatal chan<- error, err error, sessionID, reason string) {
	slog.Error("relay daemon reported a fatal failure", "session_id", sessionID, "reason", reason, "error", err.Error())
	select {
	case fatal <- err:
	default:
		slog.Error("could not deliver relay fatal failure to daemon", "session_id", sessionID, "reason", reason, "error", "no available capacity")
	}
}

// releaseCheckpointBarriers cancels the checkpoint barriers a disconnected
// connection still owned.
func releaseCheckpointBarriers(relay *RelayCell, dispatchWake *dispatchWakeChannel, checkpointBarriers map[string]struct{}) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs release_checkpoint_barriers")
}

// wakeDispatch tells the coordinator durable work may be available, or
// dispatches a checkpoint-only relay's work itself.
func wakeDispatch(relay *RelayCell, dispatchWake *dispatchWakeChannel) error {
	panic("unported: mj-worker/src/worker_runtime/unix/dispatch.rs wake_dispatch")
}
