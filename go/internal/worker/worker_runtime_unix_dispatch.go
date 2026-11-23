// Port of mj-worker/src/worker_runtime/unix/dispatch.rs.
//
// The relay coordinator: it records the ACP runtime's events into the
// durable relay and dispatches claimed commands to the runtime.

//go:build unix

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"runtime"
	"slices"
	"sort"
	"strings"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"
	acpunstable "github.com/BrokkAi/acp-go/schema/unstable"

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

// coordinatorClock is the clock the harness startup deadline runs on: the
// system clock, or a test's manual clock (Rust's tests pause tokio's).
type coordinatorClock interface {
	now() time.Time
	// at fires once now() reaches deadline; stop releases it unfired.
	at(deadline time.Time) (fired <-chan time.Time, stop func())
}

// systemCoordinatorClock is the real clock.
type systemCoordinatorClock struct{}

func (systemCoordinatorClock) now() time.Time { return time.Now() }

func (systemCoordinatorClock) at(deadline time.Time) (<-chan time.Time, func()) {
	timer := time.NewTimer(time.Until(deadline))
	return timer.C, func() { timer.Stop() }
}

// harnessStartup is the harness's startup deadline. Only the coordinator
// accepts readiness or expiry. A ready bridge removes its timer; a restart
// arms a new one, and expiry ends the worker with a typed startup failure.
type harnessStartup struct {
	// deadline is zero when no startup is pending.
	deadline time.Time
	clock    coordinatorClock
}

func newHarnessStartup() harnessStartup {
	return newHarnessStartupOn(systemCoordinatorClock{})
}

func newHarnessStartupOn(clock coordinatorClock) harnessStartup {
	return harnessStartup{deadline: clock.now().Add(core.NativeSessionStartupTimeout), clock: clock}
}

func (s *harnessStartup) observeEvent(restarting, ready bool) {
	if restarting {
		s.deadline = s.clock.now().Add(core.NativeSessionStartupTimeout)
	}
	if ready {
		s.deadline = time.Time{}
	}
}

// runRelayCoordinatorWithShells runs the coordinator of a started harness,
// with the completed-turn classifier this worker resolves for itself.
func runRelayCoordinatorWithShells(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, dispatchWakes *dispatchWakeChannel, commands *commandChannel, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, capture *WorkObserver, startup harnessStartup) error {
	verdict := verdictClientResolve(ctx, nil)
	return runRelayCoordinator(ctx, relay, events, dispatchWakes, commands, userShells, kimiTasks, verdict, capture, &startup)
}

// runRelayCoordinatorWithVerdict runs the coordinator with the given
// classifier and no startup deadline (Rust: cfg(test)). The coordinator owns
// one handle to each thing it serves; a struct of them would only rename the
// arguments.
func runRelayCoordinatorWithVerdict(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, dispatchWakes *dispatchWakeChannel, commands *commandChannel, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, verdict *VerdictClient, capture *WorkObserver) error {
	return runRelayCoordinator(ctx, relay, events, dispatchWakes, commands, userShells, kimiTasks, verdict, capture, nil)
}

// coordinatorTimer is a persisted wall deadline and the monotonic instant
// the coordinator wakes for it.
type coordinatorTimer struct {
	deadline int64
	wake     time.Time
}

// coordinatorStopTimer is a stop applied to a turn Claude Code started on
// its own, keyed by that turn's first ordinal, and when to end the turn if
// nothing answers it.
type coordinatorStopTimer struct {
	turn uint64
	wake time.Time
}

// verdictTaskResult is one finished completed-turn classification task.
type verdictTaskResult struct {
	generation uint64
	attempt    *VerdictAttempt
	answer     core.Probabilities
	err        error
	// cancelled is a task the coordinator aborted before it finished.
	cancelled bool
	// failure is a task that panicked (Rust: a JoinError that is not a
	// cancellation).
	failure error
}

// locatorTaskResult is one finished question locator task.
type locatorTaskResult struct {
	id      uint64
	ids     []string
	index   int
	located bool
	err     error
	failure error
}

// loginReadResult is the initial Claude login read.
type loginReadResult struct {
	revision uint64
	login    *string
	err      error
	failure  error
}

// coordinatorRemainingUntil is the monotonic instant at which the wall deadline
// deadlineMs falls, measured now.
func coordinatorRemainingUntil(deadlineMs int64) time.Time {
	remaining := max(deadlineMs-core.EpochMillis(), 0)
	return time.Now().Add(time.Duration(remaining) * time.Millisecond)
}

// coordinatorTimerAt fires at the monotonic instant at.
func coordinatorTimerAt(at time.Time) (<-chan time.Time, func()) {
	timer := time.NewTimer(time.Until(at))
	return timer.C, func() { timer.Stop() }
}

// runRelayCoordinator records runtime events and dispatches commands until
// the ACP runtime stops, the harness fails to start, or ctx is cancelled
// (Rust: the task is aborted).
func runRelayCoordinator(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, dispatchWakes *dispatchWakeChannel, commands *commandChannel, userShells *UserShellRegistry, kimiTasks *KimiTaskMonitor, verdict *VerdictClient, capture *WorkObserver, startup *harnessStartup) error {
	// The coordinator owns the receiving side of the dispatch wakes: once it
	// stops, a sender sees the closed channel.
	defer dispatchWakes.closeReceiver()
	// Rust moves the user shell registry into the coordinator, so it drops
	// (cancels the shells still running) when the coordinator returns.
	if userShells != nil {
		defer userShells.Close()
	}
	if verdict != nil {
		guard := relay.Lock()
		log, ok := guard.TurnContext().DecisionLog()
		guard.Unlock()
		var logged VerdictClient
		if ok {
			logged = verdict.withLog(&log)
		} else {
			logged = verdict.withLog(nil)
		}
		verdict = &logged
	}
	guard := relay.Lock()
	guard.handbackVerdictClient = verdict
	guard.Unlock()
	// Owned by this coordinator: cancelling it stops HTTP requests on every
	// exit path, and joining reports panics instead of losing background
	// errors.
	guard = relay.Lock()
	if !guard.classifiesTurns() {
		verdict = nil
	}
	guard.Unlock()

	// done tells background tasks nobody will collect their result.
	done := make(chan struct{})
	defer close(done)

	verdictResults := make(chan verdictTaskResult)
	var verdictCancels []context.CancelFunc
	verdictPending := 0
	abortVerdicts := func() {
		for _, cancel := range verdictCancels {
			cancel()
		}
		verdictCancels = nil
	}
	// shutdownVerdicts aborts the classifier tasks and waits for them, so
	// their cancellation is reported before the coordinator returns.
	shutdownVerdicts := func() {
		abortVerdicts()
		for ; verdictPending > 0; verdictPending-- {
			if result := <-verdictResults; result.attempt != nil {
				result.attempt.Close()
			}
		}
	}
	defer abortVerdicts()
	locatorResults := make(chan locatorTaskResult)
	locatorCompletions := map[uint64]uint64{}
	var locatorCancels []context.CancelFunc
	locatorPending := 0
	nextLocatorID := uint64(0)
	defer func() {
		for _, cancel := range locatorCancels {
			cancel()
		}
	}()
	var verdictGeneration *uint64
	var verdictPoll <-chan time.Time
	if verdict != nil {
		ticker := time.NewTicker(time.Second)
		defer ticker.Stop()
		verdictPoll = ticker.C
	}
	inFlight := map[string]core.RelayCommand{}
	configuration := sessionConfigurationOpening
	var kimiPoll <-chan time.Time
	if kimiTasks != nil {
		ticker := time.NewTicker(kimiTaskPollInterval)
		defer ticker.Stop()
		kimiPoll = ticker.C
	}
	loginReads := make(chan loginReadResult, 1)
	loginPending := false
	guard = relay.Lock()
	var initialLoginPath *string
	var initialRevision uint64
	if guard.authenticationLoginPath != nil {
		path := *guard.authenticationLoginPath
		initialLoginPath = &path
		initialRevision, _ = guard.loginIdentity.snapshot()
	}
	guard.Unlock()
	if initialLoginPath != nil {
		loginPending = true
		readCtx, cancelRead := context.WithCancel(ctx)
		defer cancelRead()
		go func(path string, revision uint64) {
			result := loginReadResult{revision: revision}
			func() {
				defer func() {
					if payload := recover(); payload != nil {
						result.failure = fmt.Errorf("task panicked: %v", payload)
					}
				}()
				result.login, result.err = readClaudeLoginFingerprint(readCtx, path)
			}()
			loginReads <- result
		}(*initialLoginPath, initialRevision)
	}
	if err := recoverAuthentication(relay); err != nil {
		return err
	}
	if err := dispatchPending(relay, commands, inFlight, configuration.isReady(), userShells); err != nil {
		return err
	}
	capturing := func() *DispatchCapture {
		if capture == nil {
			return nil
		}
		return &DispatchCapture{observer: capture, wakes: dispatchWakes}
	}
	dispatch := func() error {
		return dispatchPending(relay, commands, inFlight, configuration.isReady(), userShells)
	}
	// A monotonic deadline keeps unrelated events and wall-clock changes from
	// restarting the wait. Only the persisted wall deadline crosses restarts.
	var capacityTimer, mailboxLeaseTimer *coordinatorTimer
	var stopTimer *coordinatorStopTimer
	for {
		var startupDeadline time.Time
		if startup != nil {
			startupDeadline = startup.deadline
		}
		// While a checkpoint barrier holds its cut, or a Close has sealed
		// the relay at one, the worker writes nothing of its own: the
		// classifier's answer, its assessment bookkeeping and the retry and
		// stop timers all wait. A barrier ends through a relay request or a
		// disconnect, and both wake this loop, so the branches below open
		// again then. The open deadline waits only for a held cut: a pending
		// Close is claimed only once the session is configured, so waiting
		// for it would wait forever on a harness that never opens.
		guard := relay.Lock()
		cutHeld := guard.WorkerWritesHeld()
		held := cutHeld || guard.ClosePending()
		guard.Unlock()
		if !held {
			guard := relay.Lock()
			var err error
			if guard.classifiesTurns() {
				err = guard.preparePendingAssessment()
			}
			guard.Unlock()
			if err != nil {
				return err
			}
		}
		if verdictGeneration != nil {
			generation := *verdictGeneration
			guard := relay.Lock()
			current := guard.repliedVerdictIsCurrent(generation)
			session := guard.TurnContext().SessionID()
			guard.Unlock()
			if !current {
				slog.Info("Jev request invalidated", "target", "mj_jev", "generation", generation,
					"phase", "replied", "session", session,
					"reason", "evidence_or_lifecycle_changed", "outcome", "cancelled")
				abortVerdicts()
				verdictGeneration = nil
			}
		}
		guard = relay.Lock()
		var pending PendingRepliedVerdict
		var hasPending bool
		var facts core.ActivityFacts
		var native int
		if !held && guard.classifiesTurns() {
			if pending, hasPending = guard.PendingRepliedVerdict(); hasPending {
				facts, native = guard.glideRuntime()
			}
		}
		guard.Unlock()
		if hasPending {
			generation := pending.Generation
			if verdict != nil {
				abortVerdicts()
				verdictGeneration = &generation
				guard := relay.Lock()
				session := guard.TurnContext().SessionID()
				guard.Unlock()
				client := *verdict
				evidence := pending.Evidence
				taskCtx, cancel := context.WithCancel(ctx)
				verdictCancels = append(verdictCancels, cancel)
				verdictPending++
				go func() {
					result := verdictTaskResult{generation: generation}
					func() {
						defer func() {
							if payload := recover(); payload != nil {
								result.failure = fmt.Errorf("task panicked: %v", payload)
							}
						}()
						result.attempt, result.answer, result.err = client.askGlideLogged(taskCtx, session, generation, &evidence, &facts, native)
					}()
					if result.failure == nil && result.err != nil && taskCtx.Err() != nil {
						// An aborted request ends its attempt as cancelled. An
						// answer that arrived first is still delivered, as a
						// finished task's output is, and the coordinator
						// discards it when it is no longer current.
						if result.attempt != nil {
							result.attempt.Close()
						}
						result.cancelled = true
					}
					select {
					case verdictResults <- result:
					case <-done:
						if result.attempt != nil {
							result.attempt.Close()
						}
					}
				}()
			} else {
				guard := relay.Lock()
				err := guard.failTurnAssessment(generation, "classifier_unavailable")
				session := guard.TurnContext().SessionID()
				guard.Unlock()
				if err != nil {
					return err
				}
				slog.Info("Jev classification skipped", "target", "mj_jev", "generation", generation,
					"phase", "replied", "outcome", "skipped", "session", session,
					"reason", "classifier_unavailable")
			}
		}
		guard = relay.Lock()
		capacityDeadline, hasCapacityDeadline := guard.CapacityRetryDeadline()
		mailboxLeaseDeadline, hasMailboxLeaseDeadline := guard.MailboxHookLeaseDeadline()
		unansweredStop, hasUnansweredStop := guard.UnansweredHarnessTurnStop()
		guard.Unlock()
		if !hasCapacityDeadline {
			capacityTimer = nil
		} else if capacityTimer == nil || capacityTimer.deadline != capacityDeadline {
			capacityTimer = &coordinatorTimer{deadline: capacityDeadline, wake: coordinatorRemainingUntil(capacityDeadline)}
		}
		if !hasMailboxLeaseDeadline {
			mailboxLeaseTimer = nil
		} else if mailboxLeaseTimer == nil || mailboxLeaseTimer.deadline != mailboxLeaseDeadline {
			mailboxLeaseTimer = &coordinatorTimer{deadline: mailboxLeaseDeadline, wake: coordinatorRemainingUntil(mailboxLeaseDeadline)}
		}
		if !hasUnansweredStop {
			stopTimer = nil
		} else if stopTimer == nil || stopTimer.turn != unansweredStop {
			stopTimer = &coordinatorStopTimer{turn: unansweredStop, wake: time.Now().Add(HarnessTurnStopGrace)}
		}

		var stops []func()
		var startupC, mailboxC, capacityC, stopC <-chan time.Time
		if !startupDeadline.IsZero() && !cutHeld {
			fired, stop := startup.clock.at(startupDeadline)
			startupC, stops = fired, append(stops, stop)
		}
		if mailboxLeaseTimer != nil && !held {
			fired, stop := coordinatorTimerAt(mailboxLeaseTimer.wake)
			mailboxC, stops = fired, append(stops, stop)
		}
		if capacityTimer != nil && !held {
			fired, stop := coordinatorTimerAt(capacityTimer.wake)
			capacityC, stops = fired, append(stops, stop)
		}
		if stopTimer != nil && !held {
			fired, stop := coordinatorTimerAt(stopTimer.wake)
			stopC, stops = fired, append(stops, stop)
		}
		kimiC := kimiPoll
		if held {
			kimiC = nil
		}
		var verdictC <-chan verdictTaskResult
		if verdictPending > 0 && !held {
			verdictC = verdictResults
		}
		var locatorC <-chan locatorTaskResult
		if locatorPending > 0 && !held {
			locatorC = locatorResults
		}
		var loginC <-chan loginReadResult
		if loginPending {
			loginC = loginReads
		}

		onStartup := func() (bool, error) {
			// Already received readiness precedes an uncommitted expiry.
			// Bound the prefix by the actual queue so fresh chatter cannot
			// starve the startup decision.
			stopped, err := recordQueuedRuntimeEvents(ctx, relay, inFlight, events, &configuration, userShells, kimiTasks, len(events), capturing(), startup)
			if err != nil || stopped {
				return stopped, err
			}
			if startup.deadline.IsZero() || startup.clock.now().Before(startup.deadline) {
				if err := recoverAuthentication(relay); err != nil {
					return false, err
				}
				return false, dispatch()
			}
			cause := fmt.Sprintf("the harness did not open its session within %ds; use Restart agent to retry in the existing environment",
				int64(core.NativeSessionStartupTimeout/time.Second))
			guard := relay.Lock()
			// A control request may have acquired a cut since cutHeld was
			// observed. Decide and journal under its owner's lock.
			if guard.WorkerWritesHeld() {
				guard.Unlock()
				return false, nil
			}
			guard.setAcpReady(false)
			// A Close sealed the relay at its cut: nothing is journaled
			// behind the seal, and the next worker settles what is left, as
			// after any worker death.
			if !guard.ClosePending() {
				if err := interruptInFlightLocked(guard.DurableRelay, inFlight, cause); err != nil {
					guard.Unlock()
					return false, err
				}
				// As when the bridge faults before opening, clear any
				// question it asked while opening, so no invisible question
				// outlives this harness. Accepted lifecycle work stays queued
				// for the next worker.
				if _, err := guard.RecordObservation(core.RelayObservationElicitationsCleared{}); err != nil {
					guard.Unlock()
					return false, err
				}
			}
			guard.Unlock()
			// The worker ends here: the runtime owner stops ACP and the exit
			// record carries the typed failure, so the daemon stops
			// restarting this harness and reports why.
			shutdownVerdicts()
			return false, core.WorkerHarnessStartupFailure{Message: cause}
		}
		onWake := func() (bool, error) {
			// A wake only says durable work may now be available. Runtime
			// events already in the channel always belong before any newly
			// admitted checkpoint cut.
			stopped, err := recordQueuedRuntimeEvents(ctx, relay, inFlight, events, &configuration, userShells, kimiTasks, len(events), capturing(), startup)
			if err != nil || stopped {
				return stopped, err
			}
			if err := recoverAuthentication(relay); err != nil {
				return false, err
			}
			return false, dispatch()
		}
		onEvent := func(event core.RuntimeEvent, open bool) (bool, error) {
			if !open {
				if err := interruptInFlight(relay, inFlight, "ACP runtime stopped before the command completed"); err != nil {
					return false, err
				}
				return true, nil
			}
			stopped, err := recordRuntimeEventBatch(ctx, relay, inFlight, event, events, &configuration, userShells, kimiTasks, capturing(), startup)
			if err != nil || stopped {
				return stopped, err
			}
			if err := recoverAuthentication(relay); err != nil {
				return false, err
			}
			return false, dispatch()
		}
		onMailboxLease := func() (bool, error) {
			deadline := mailboxLeaseTimer.deadline
			guard := relay.Lock()
			expired, err := guard.ExpireMailboxHookLeaseAndPromote(core.EpochMillis())
			guard.Unlock()
			if err != nil {
				return false, err
			}
			if expired {
				return false, dispatch()
			}
			// A wall-clock adjustment can make a monotonic timer fire before
			// the persisted deadline. Retry with a bounded delay, not a spin.
			mailboxLeaseTimer = &coordinatorTimer{deadline: deadline, wake: time.Now().Add(time.Second)}
			return false, nil
		}
		onCapacity := func() (bool, error) {
			guard := relay.Lock()
			admitted, err := guard.SubmitDueCapacityRetry(max(core.EpochMillis(), capacityDeadline))
			guard.Unlock()
			if err != nil {
				return false, err
			}
			if !admitted {
				// Wait for readiness/control changes without spinning on an
				// overdue timer.
				capacityTimer = &coordinatorTimer{deadline: capacityDeadline, wake: time.Now().Add(time.Second)}
			}
			return false, dispatch()
		}
		onStop := func() (bool, error) {
			turn := stopTimer.turn
			stopTimer = nil
			// Ending the turn is a managed child's turn ending like any
			// other: observe its work first, then replay what arrived.
			var deferred []core.RuntimeEvent
			var observed *core.WorkFacts
			if capture != nil {
				facts := captureWhileOpen(ctx, capturing(), relay, events, &deferred)
				if err := ctx.Err(); err != nil {
					return false, err
				}
				observed = &facts
			}
			guard := relay.Lock()
			guard.OfferSettleFacts(observed)
			_, ended := guard.EndUnansweredHarnessTurnStop(turn)
			guard.OfferSettleFacts(nil)
			guard.Unlock()
			if ended != nil {
				return false, ended
			}
			stopped, err := recordEventsInOrder(ctx, relay, inFlight, deferred, events, &configuration, userShells, kimiTasks, capturing(), startup)
			if err != nil || stopped {
				return stopped, err
			}
			// A checkpoint barrier may have been waiting for this turn.
			return false, dispatch()
		}
		onKimiPoll := func() (bool, error) {
			return false, kimiTasks.refresh(ctx, relay, false)
		}
		onLoginRead := func(result loginReadResult) (bool, error) {
			loginPending = false
			switch {
			case result.failure != nil:
				return false, fmt.Errorf("Claude login reader failed: %w", result.failure)
			case result.err != nil:
				slog.Warn("could not initialize the Claude login identity", "error", result.err)
				return false, nil
			}
			guard := relay.Lock()
			guard.loginIdentity.observe(result.revision, result.login)
			guard.Unlock()
			if err := recoverAuthentication(relay); err != nil {
				return false, err
			}
			return false, dispatch()
		}
		onVerdict := func(result verdictTaskResult) (bool, error) {
			verdictPending--
			switch {
			case result.failure != nil:
				slog.Warn("completed-turn classifier task failed", "target", "mj_jev", "error", result.failure)
				if verdictGeneration != nil {
					generation := *verdictGeneration
					verdictGeneration = nil
					guard := relay.Lock()
					err := guard.failTurnAssessment(generation, "classifier_task_failed")
					guard.Unlock()
					if err != nil {
						return false, err
					}
				}
			case result.cancelled:
			default:
				if err := applyVerdictResult(ctx, relay, verdict, result, &verdictGeneration, locatorResults, done, &locatorCancels, &locatorPending, &nextLocatorID, locatorCompletions); err != nil {
					return false, err
				}
			}
			return false, recoverAuthentication(relay)
		}
		onLocator := func(result locatorTaskResult) (bool, error) {
			locatorPending--
			var target *string
			switch {
			case result.failure != nil:
				slog.Warn("question locator task failed", "target", "mj_jev", "error", result.failure)
			case result.err != nil:
				slog.Warn("question locator failed", "target", "mj_jev", "error", result.err)
			case result.located && result.index >= 0 && result.index < len(result.ids):
				if id, ok := core.TranscriptStableID(result.ids[result.index]); ok {
					target = &id
				}
			}
			completed, ok := locatorCompletions[result.id]
			if !ok {
				return false, nil
			}
			delete(locatorCompletions, result.id)
			guard := relay.Lock()
			_, err := guard.finishQuestionLocator(completed, target)
			guard.Unlock()
			return false, err
		}

		// Rust's select is biased: the startup deadline, then wakes, then
		// events come first when several are ready.
		var action func() (bool, error)
		if startupC != nil {
			select {
			case <-startupC:
				action = onStartup
			default:
			}
		}
		if action == nil {
			select {
			case <-dispatchWakes.receive():
				action = onWake
			default:
			}
		}
		if action == nil {
			select {
			case event, open := <-events:
				action = func() (bool, error) { return onEvent(event, open) }
			default:
			}
		}
		if action == nil {
			select {
			case <-ctx.Done():
				action = func() (bool, error) { return false, ctx.Err() }
			case <-startupC:
				action = onStartup
			case <-dispatchWakes.receive():
				action = onWake
			case event, open := <-events:
				action = func() (bool, error) { return onEvent(event, open) }
			case <-mailboxC:
				action = onMailboxLease
			case <-capacityC:
				action = onCapacity
			case <-stopC:
				action = onStop
			case <-kimiC:
				action = onKimiPoll
			case <-verdictPoll:
				action = func() (bool, error) { return false, nil }
			case result := <-loginC:
				action = func() (bool, error) { return onLoginRead(result) }
			case result := <-verdictC:
				action = func() (bool, error) { return onVerdict(result) }
			case result := <-locatorC:
				action = func() (bool, error) { return onLocator(result) }
			}
		}
		for _, stop := range stops {
			stop()
		}
		exit, err := action()
		if err != nil {
			return err
		}
		if exit {
			shutdownVerdicts()
			return nil
		}
	}
}

// applyVerdictResult applies one finished classification, starting a
// question locator when the answer asks the person for input.
func applyVerdictResult(ctx context.Context, relay *RelayCell, verdict *VerdictClient, result verdictTaskResult, verdictGeneration **uint64, locatorResults chan<- locatorTaskResult, done <-chan struct{}, locatorCancels *[]context.CancelFunc, locatorPending *int, nextLocatorID *uint64, locatorCompletions map[uint64]uint64) error {
	generation := result.generation
	attempt := result.attempt
	if attempt == nil {
		attempt = &VerdictAttempt{finished: true}
	}
	defer attempt.Close()
	if *verdictGeneration != nil && **verdictGeneration == generation {
		*verdictGeneration = nil
	}
	guard := relay.Lock()
	defer guard.Unlock()
	if !guard.repliedVerdictIsCurrent(generation) {
		attempt.finish("discarded", "activity_or_generation_changed")
		return nil
	}
	if result.err != nil {
		if err := guard.failTurnAssessment(generation, "request_failed"); err != nil {
			return err
		}
		attempt.finish("unchanged", "request_failed")
		return nil
	}
	reason, err := guard.applyGlideAssessment(generation, result.answer)
	if err != nil {
		return err
	}
	attempt.finish("applied", reason)
	if reason != "user_input_required" {
		return nil
	}
	completed, ok := guard.questionLocatorCompletion(generation)
	if !ok {
		return nil
	}
	request := guard.questionLocatorRequest(generation)
	switch {
	case request != nil && len(request.MessageIDs) == 1:
		var target *string
		if id, ok := core.TranscriptStableID(request.MessageIDs[0]); ok {
			target = &id
			if verdict != nil {
				verdict.logSingleQuestion(guard.TurnContext().SessionID(), request, id)
			}
		}
		_, err := guard.finishQuestionLocator(completed, target)
		return err
	case request != nil && verdict != nil:
		client := *verdict
		session := guard.TurnContext().SessionID()
		located := *request
		*nextLocatorID++
		id := *nextLocatorID
		locatorCompletions[id] = completed
		taskCtx, cancel := context.WithCancel(ctx)
		*locatorCancels = append(*locatorCancels, cancel)
		*locatorPending++
		go func() {
			outcome := locatorTaskResult{id: id, ids: located.MessageIDs}
			func() {
				defer func() {
					if payload := recover(); payload != nil {
						outcome.failure = fmt.Errorf("task panicked: %v", payload)
					}
				}()
				outcome.index, outcome.located, outcome.err = client.locateQuestion(taskCtx, session, &located)
			}()
			select {
			case locatorResults <- outcome:
			case <-done:
			}
		}()
		return nil
	default:
		_, err := guard.finishQuestionLocator(completed, nil)
		return err
	}
}

// recoverAuthentication consults the same identity that credential
// installation publishes. This is supplemental evidence; neither dispatch
// nor recovery waits on filesystem I/O.
func recoverAuthentication(relay *RelayCell) error {
	guard := relay.Lock()
	defer guard.Unlock()
	ordinal, _, ok := guard.authenticationRetryIntent()
	if !ok {
		return nil
	}
	if _, login := guard.loginIdentity.snapshot(); login != nil {
		_, err := guard.submitAuthenticationRetry(ordinal, *login)
		return err
	}
	return nil
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
	// The relay is asked on entry and on every wake, never only on a wake:
	// the wake of a command accepted just before this prompt's event was
	// drained is already consumed, and the command is still waiting for this
	// task.
	endsForCommand := func() bool {
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.UndeliveredCancellationOrClosePending()
	}
	const commandAccepted = "a cancellation or close was accepted while the turn's observation was being taken"
	if endsForCommand() {
		return core.WorkFactsUnavailable(commandAccepted)
	}
	observationCtx, stopObservation := context.WithCancel(ctx)
	defer stopObservation()
	observation := make(chan core.WorkFacts, 1)
	go func() { observation <- WorkFactsCapture(observationCtx, capture.observer) }()
	for {
		// The observation is biased first, as in Rust.
		select {
		case facts := <-observation:
			return facts
		default:
		}
		select {
		case facts := <-observation:
			return facts
		case <-ctx.Done():
			return core.WorkFactsUnavailable("the worker stopped while the turn's observation was being taken")
		case <-capture.wakes.receive():
			if endsForCommand() {
				return core.WorkFactsUnavailable(commandAccepted)
			}
		case event, open := <-events:
			if !open {
				return core.WorkFactsUnavailable("the harness exited while the turn's observation was being taken")
			}
			ends := false
			switch event.(type) {
			case core.RuntimeEventStopped, core.RuntimeEventHarnessRestarting:
				ends = true
			}
			*deferred = append(*deferred, event)
			if ends {
				return core.WorkFactsUnavailable("the harness stopped while the turn's observation was being taken")
			}
		}
	}
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
	_, finishingPrompt := event.(core.RuntimeEventPromptFinished)
	if !finishingPrompt {
		guard := relay.Lock()
		finishingPrompt = guard.EventSettlesAutonomousTurn(event)
		guard.Unlock()
	}
	var workFacts *core.WorkFacts
	if capture != nil && finishingPrompt {
		prepared := make(chan error, 1)
		go func() { prepared <- prepareKimiRuntimeEvent(ctx, kimiTasks, relay, &event) }()
		facts := captureWhileOpen(ctx, capture, relay, events, deferred)
		if err := <-prepared; err != nil {
			return false, err
		}
		if err := ctx.Err(); err != nil {
			return false, err
		}
		workFacts = &facts
	} else if err := prepareKimiRuntimeEvent(ctx, kimiTasks, relay, &event); err != nil {
		return false, err
	}
	trackUserShellCompletion(userShells, event)
	return recordRuntimeEventWithStartup(relay, inFlight, event, sessionConfiguration, workFacts, startup)
}

func recordRuntimeEventWithStartup(relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent, sessionConfiguration *sessionConfiguration, workFacts *core.WorkFacts, startup *harnessStartup) (bool, error) {
	restarting, configured := false, false
	switch event.(type) {
	case core.RuntimeEventHarnessRestarting, core.RuntimeEventContextClearing:
		restarting = true
	case core.RuntimeEventSessionConfigured:
		configured = true
	}
	var failure error
	if fault, ok := event.(core.RuntimeEventSessionFault); ok && startup != nil && *sessionConfiguration == sessionConfigurationOpening {
		cause, diagnostics, _ := strings.Cut(fault.Message, "\n")
		failure = core.WorkerHarnessStartupFailure{Message: cause}
		if diagnostics != "" {
			failure = fmt.Errorf("%s: %w", diagnostics, failure)
		}
	}
	stopped, err := recordRuntimeEventWithFacts(relay, inFlight, event, workFacts)
	if err != nil {
		return false, err
	}
	if failure != nil {
		// The worker exits before the runtime's Stopped is recorded. Clear
		// any question the bridge asked while opening, as Stopped would, so
		// no invisible question outlives this harness. In-flight commands
		// are left for the next boot's recovery.
		guard := relay.Lock()
		_, err := guard.RecordObservation(core.RelayObservationElicitationsCleared{})
		guard.Unlock()
		if err != nil {
			return false, err
		}
		return false, failure
	}
	if configured {
		*sessionConfiguration = sessionConfigurationReady
	} else if restarting && sessionConfiguration.isReady() {
		// Allow dispatch again only after the fresh bridge configures
		// itself. Its failure is no longer a failure to open this worker's
		// session.
		*sessionConfiguration = sessionConfigurationReopening
	}
	if startup != nil {
		ready := false
		if sessionConfiguration.isReady() {
			guard := relay.Lock()
			state := guard.OperationalState()
			guard.Unlock()
			ready = state.NativeSessionIsReady()
		}
		startup.observeEvent(restarting, ready)
	}
	return stopped, nil
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
	if finished, ok := event.(core.RuntimeEventUserShellFinished); ok {
		userShells.Completed(finished.RequestID)
	}
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
	_, stopped := event.(core.RuntimeEventStopped)
	guard := relay.Lock()
	defer guard.Unlock()
	// A prompt's facts ride in its own outcome below; for any other event
	// they are what a settlement of an autonomous turn takes. Every event
	// replaces them, so none outlives the event it was observed for.
	if _, prompt := event.(core.RuntimeEventPromptFinished); prompt {
		guard.OfferSettleFacts(nil)
	} else {
		guard.OfferSettleFacts(workFacts)
		workFacts = nil
	}
	if err := recordRuntimeEventLocked(guard, inFlight, event, workFacts); err != nil {
		return false, err
	}
	// An event that settled nothing leaves the observation unused.
	guard.OfferSettleFacts(nil)
	return stopped, nil
}

// recordRuntimeEventLocked journals one runtime event under the relay lock.
// Whether this event can wait behind a ready checkpoint barrier is the
// relay's decision, made where it journals (appendRelayEvent). An event that
// journals nothing leaves the cut held.
func recordRuntimeEventLocked(relay *RelayGuard, inFlight map[string]core.RelayCommand, event core.RuntimeEvent, workFacts *core.WorkFacts) error {
	observe := func(observations ...core.RelayObservation) error {
		for _, observation := range observations {
			if _, err := relay.RecordObservation(observation); err != nil {
				return err
			}
		}
		return nil
	}
	complete := func(requestID string, outcome core.RelayCommandOutcome) error {
		_, err := relay.RecordCommandCompleted(requestID, outcome)
		return err
	}
	switch event := event.(type) {
	case core.RuntimeEventConnected:
		if event.ProtocolVersion == nil || event.Capabilities == nil {
			return observe(core.RelayObservationWarning{Message: "ACP initialized without capability metadata"})
		}
		RecordStartupStep(relay.Root(), "acp-initialized")
		if err := observe(core.RelayObservationAgentInitialized{
			ProtocolVersion: *event.ProtocolVersion,
			Capabilities:    *event.Capabilities,
			AgentInfo:       event.AgentInfo,
		}); err != nil {
			return err
		}
		relay.SetSteeringSupported(event.SteeringSupported)
		// Every bridge that advertises the steering extension queues the
		// steer until the running turn can take it, at its next tool
		// boundary. Do not require a second capability: the extension is the
		// whole contract, and some bridges (Muse) implement it without
		// advertising `idleBehaviors`.
		relay.SetAutomaticSteering(event.SteeringSupported != nil && *event.SteeringSupported)
	case core.RuntimeEventBridgeRetired:
		return relay.retireAgentTerminalConnection(event.TerminalConnectionID)
	case core.RuntimeEventContextClearing:
		// The previous bridge has exited, including when opening the new
		// conversation fails and /clear rolls back to its native session.
		return relay.retireHarnessProcessState()
	case core.RuntimeEventContextCleared:
		if err := complete(event.RequestID, core.RelayCommandOutcomeContextCleared{
			NativeSessionID: event.NativeSessionID,
			Memory:          event.Memory,
		}); err != nil {
			return err
		}
		delete(inFlight, event.RequestID)
	case core.RuntimeEventSessionStarted:
		RecordStartupStep(relay.Root(), "acp-session-open")
		return observe(core.RelayObservationSessionOpened{
			NativeSessionID:               event.NativeSessionID,
			Resumed:                       event.Resumed,
			NativeContinuityLost:          event.NativeContinuityLost,
			ReplacedUnusedNativeSessionID: event.ReplacedUnusedNativeSessionID,
		})
	case core.RuntimeEventSessionConfigured:
		return observe(core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions})
	case core.RuntimeEventNativeSessionUsed:
		return relay.MarkNativeSessionUsed()
	case core.RuntimeEventSessionModesConfigured:
		return observe(core.RelayObservationSessionModesConfigured{Modes: event.Modes})
	case core.RuntimeEventNativeAgent:
		return observe(core.RelayObservationNativeAgent{Event: event.Event})
	case core.RuntimeEventNativeGoalResumeSent:
		relay.nativeGoalResumeSent(event.ResumeID, event.Goal)
		event.Applied.Acknowledge()
	case core.RuntimeEventNativeGoalResumeRejected:
		relay.nativeGoalResumeRejected(event.ResumeID)
	case core.RuntimeEventSessionUpdate:
		var update acpschema.SessionUpdate
		if err := json.Unmarshal(event.Update, &update); err != nil {
			decode := fmt.Errorf("decode ACP session update for relay journal: %w", err)
			if err := observe(core.RelayObservationWarning{Message: decode.Error()}); err != nil {
				return err
			}
			return decode
		}
		_, err := relay.RecordSessionUpdate(update)
		return err
	case core.RuntimeEventClaudeBackgroundTasksChanged, core.RuntimeEventClaudeAsyncTaskControlChanged:
		// Historical runtime payloads can still appear in old transcripts.
	case core.RuntimeEventAirTaskChanged:
		return relay.AirTaskChanged(event.SessionID, event.Update)
	case core.RuntimeEventClaudeTurnResult:
		return relay.ClaudeTurnResult(&event.ClaudeTurnResult)
	case core.RuntimeEventElicitationRequested:
		return observe(core.RelayObservationElicitationRequested{Request: event.Request})
	case core.RuntimeEventElicitationResolved:
		return observe(core.RelayObservationElicitationResolved{
			ElicitationID: event.ElicitationID,
			Action:        event.Action,
			Reply:         event.Reply,
		})
	case core.RuntimeEventPromptFinished:
		delete(inFlight, event.RequestID)
		return complete(event.RequestID, core.RelayCommandOutcomePrompt{
			StopReason: event.StopReason,
			Usage:      event.Usage,
			Diagnostic: event.Diagnostic,
			WorkFacts:  workFacts,
		})
	case core.RuntimeEventConfigApplied:
		if event.RequestID == "" {
			// Hel applied this selector for itself, recovering a session
			// whose saved value the harness dropped, so no relay command is
			// waiting to be completed. Publish the catalogue the value came
			// from first: the accepted pair is normalized against whatever
			// options are current when the value lands.
			return observe(
				core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions},
				core.RelayObservationConfigurationUpdated{Key: event.Key, Value: event.Value},
			)
		}
		if err := observe(
			core.RelayObservationConfigurationUpdated{Key: event.Key, Value: event.Value},
			core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions},
		); err != nil {
			return err
		}
		if err := complete(event.RequestID, core.RelayCommandOutcomeConfigured{}); err != nil {
			return err
		}
		delete(inFlight, event.RequestID)
	case core.RuntimeEventGoalControlApplied:
		if err := complete(event.RequestID, core.RelayCommandOutcomeGoalControlled{}); err != nil {
			return err
		}
		delete(inFlight, event.RequestID)
	case core.RuntimeEventSessionModeApplied:
		if err := observe(
			core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions},
			core.RelayObservationSessionModesConfigured{Modes: event.Modes},
		); err != nil {
			return err
		}
		if err := complete(event.RequestID, core.RelayCommandOutcomeSessionModeSet{}); err != nil {
			return err
		}
		delete(inFlight, event.RequestID)
	case core.RuntimeEventCommandRejected:
		delete(inFlight, event.RequestID)
		_, err := relay.RecordCommandRejected(event.RequestID, event.Reason, event.Message)
		return err
	case core.RuntimeEventCommandInterrupted:
		delete(inFlight, event.RequestID)
		_, err := relay.RecordCommandInterrupted(event.RequestID, event.Reason, event.Message)
		return err
	case core.RuntimeEventCancelApplied:
		delete(inFlight, event.RequestID)
		return complete(event.RequestID, core.RelayCommandOutcomeCancelled{})
	case core.RuntimeEventSteeringUnconfirmed:
		return observe(core.RelayObservationSteeringUnconfirmed{CommandID: event.RequestID, Message: event.Message})
	case core.RuntimeEventSteerApplied:
		delete(inFlight, event.RequestID)
		return complete(event.RequestID, core.RelayCommandOutcomeSteered{QueuedCommandID: event.QueuedCommandID})
	case core.RuntimeEventSteerReturned:
		delete(inFlight, event.RequestID)
		return complete(event.RequestID, core.RelayCommandOutcomeSteeringReturned{QueuedCommandID: event.QueuedCommandID})
	case core.RuntimeEventCloseApplied:
		if err := observe(core.RelayObservationNativeAgent{Event: core.NativeAgentEventDisconnected{}}); err != nil {
			return err
		}
		delete(inFlight, event.RequestID)
		if err := complete(event.RequestID, core.RelayCommandOutcomeClosed{}); err != nil {
			return err
		}
		return observe(core.RelayObservationClosed{})
	case core.RuntimeEventNotice:
		return observe(core.RelayObservationNotice{Message: event.Message})
	case core.RuntimeEventSessionFault:
		return observe(core.RelayObservationSessionFault{
			Reason:  core.OutcomeReasonRuntimeUnavailable,
			Message: event.Message,
		})
	case core.RuntimeEventWarning:
		return observe(core.RelayObservationWarning{Message: event.Message})
	case core.RuntimeEventHarnessRestarting:
		// Close admission before interrupting anything: interruption may
		// otherwise promote a replacement ahead of SessionRestarted.
		relay.ClearAcpReadiness()
		if err := observe(core.RelayObservationNativeAgent{Event: core.NativeAgentEventDisconnected{}}); err != nil {
			return err
		}
		if err := relay.retireHarnessProcessState(); err != nil {
			return err
		}
		if err := observe(core.RelayObservationWarning{Message: event.Message}); err != nil {
			return err
		}
		// Record the prompt before its controls: terminating a cancel
		// clears its target, which recovery needs to honor the user's stop.
		interrupted := takeInFlightCommands(inFlight)
		sort.SliceStable(interrupted, func(i, j int) bool {
			return core.RelayCommandKindOf(interrupted[i].command) == core.RelayCommandKindPrompt &&
				core.RelayCommandKindOf(interrupted[j].command) != core.RelayCommandKindPrompt
		})
		for _, entry := range interrupted {
			if _, err := relay.RecordCommandInterrupted(entry.commandID, core.OutcomeReasonWorkerRestarted, event.Message); err != nil {
				return err
			}
		}
		return observe(core.RelayObservationSessionRestarted{})
	case core.RuntimeEventTerminalClosed:
		// Cap here rather than letting clampObservation fire: that keeps the
		// head of a string, and a terminal's tail is what says how the
		// command ended.
		output := event.Output
		truncated := core.TruncateStartWithMarker(&output, core.TerminalJournalOutputBytes) || event.Truncated
		if err := observe(core.RelayObservationTerminalOutput{
			TerminalID: event.TerminalID,
			Output:     output,
			Truncated:  truncated,
			ExitCode:   event.ExitCode,
			Signal:     event.Signal,
		}); err != nil {
			return err
		}
		return relay.AgentTerminalClosed(event.TerminalID)
	case core.RuntimeEventTerminalStartFailed:
		return relay.agentTerminalStartFailed(event.TerminalID)
	case core.RuntimeEventTerminalStarted:
		return relay.admitAgentTerminal(core.ActiveAgentTerminal{
			TerminalID:  event.TerminalID,
			Command:     event.Command,
			StartedAtMs: event.StartedAtMs,
		})
	case core.RuntimeEventUserShellOutput:
		return observe(core.RelayObservationUserShellOutput{
			CommandID:       event.RequestID,
			Command:         event.Command,
			Stdout:          event.Stdout,
			Stderr:          event.Stderr,
			StdoutTruncated: event.StdoutTruncated,
			StderrTruncated: event.StderrTruncated,
		})
	case core.RuntimeEventUserShellFinished:
		return complete(event.RequestID, core.RelayCommandOutcomeUserShell{Result: event.Result})
	case core.RuntimeEventStopped:
		if err := observe(core.RelayObservationNativeAgent{Event: core.NativeAgentEventDisconnected{}}); err != nil {
			return err
		}
		relay.ClearAcpReadiness()
		if err := relay.retireHarnessProcessState(); err != nil {
			return err
		}
		if err := observe(core.RelayObservationElicitationsCleared{}); err != nil {
			return err
		}
		if relay.OperationalState().Execution != core.RelayExecutionStateClosed {
			if err := observe(core.RelayObservationWarning{Message: "ACP runtime stopped"}); err != nil {
				return err
			}
		}
		for _, entry := range takeInFlightCommands(inFlight) {
			if _, err := relay.RecordCommandInterrupted(entry.commandID, core.OutcomeReasonRuntimeStopped,
				"ACP runtime stopped before the command completed"); err != nil {
				return err
			}
		}
	default:
		return fmt.Errorf("unknown runtime event %T", event)
	}
	return nil
}

// inFlightCommand is one entry of the in-flight map, taken in command ID
// order (Rust iterates its BTreeMap).
type inFlightCommand struct {
	commandID string
	command   core.RelayCommand
}

// takeInFlightCommands empties inFlight and returns its entries in command ID order
// (Rust: std::mem::take of the BTreeMap).
func takeInFlightCommands(inFlight map[string]core.RelayCommand) []inFlightCommand {
	entries := make([]inFlightCommand, 0, len(inFlight))
	for commandID, command := range inFlight {
		entries = append(entries, inFlightCommand{commandID: commandID, command: command})
	}
	clear(inFlight)
	sort.Slice(entries, func(i, j int) bool { return entries[i].commandID < entries[j].commandID })
	return entries
}

func interruptInFlight(relay *RelayCell, inFlight map[string]core.RelayCommand, message string) error {
	guard := relay.Lock()
	defer guard.Unlock()
	return interruptInFlightLocked(guard.DurableRelay, inFlight, message)
}

func interruptInFlightLocked(relay *DurableRelay, inFlight map[string]core.RelayCommand, message string) error {
	if _, err := relay.RecordObservation(core.RelayObservationSessionFault{
		Reason:  core.OutcomeReasonRuntimeUnavailable,
		Message: message,
	}); err != nil {
		return err
	}
	for _, entry := range takeInFlightCommands(inFlight) {
		if _, err := relay.RecordCommandInterrupted(entry.commandID, core.OutcomeReasonRuntimeFailure, message); err != nil {
			return err
		}
	}
	return nil
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
	// Full means dispatch what fits now and reserve again on the next
	// runtime event or wake. Closed means the ACP runtime is gone, so claim
	// nothing: leaving the commands pending lets the next run dispatch them
	// instead of interrupting work that never started, and the closing
	// events channel is what ends this coordinator.
	var permits []commandPermit
	for {
		permit, ok := commands.tryReserve()
		if !ok {
			break
		}
		permits = append(permits, permit)
	}
	// Every permit not used for a send goes back, on every exit path (Rust
	// drops them).
	defer func() {
		for _, permit := range permits {
			permit.release()
		}
	}()
	if err := dispatchUserShells(relay, userShells); err != nil {
		return err
	}
	guard := relay.Lock()
	pending, err := guard.ClaimPendingCommandsUpTo(sessionConfigured, len(permits))
	guard.Unlock()
	if err != nil {
		return err
	}
	sort.SliceStable(pending, func(i, j int) bool { return pending[i].AcceptedOrdinal < pending[j].AcceptedOrdinal })
	// Hand back capacity the claim did not need before doing anything else.
	for _, permit := range permits[min(len(pending), len(permits)):] {
		permit.release()
	}
	permits = permits[:min(len(pending), len(permits))]
	attachmentRoot := func() string {
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.Root()
	}
	for _, claimed := range pending {
		if _, checkpoint := claimed.Command.(core.RelayCommandBeginCheckpoint); checkpoint {
			guard := relay.Lock()
			_, err := guard.RecordCheckpointReady(claimed.CommandID)
			guard.Unlock()
			if err != nil {
				return err
			}
			continue
		}
		command := acpCommand(&claimed)
		if command == nil {
			guard := relay.Lock()
			_, err := guard.RecordCommandRejected(claimed.CommandID, core.OutcomeReasonAdmissionRejected,
				"relay-local command was unexpectedly claimed for ACP dispatch")
			guard.Unlock()
			if err != nil {
				return err
			}
			continue
		}
		switch request := command.(type) {
		case CommandRequestPrompt:
			if dispatchPromptHasReferences(request.Prompt) {
				command = CommandRequestPromptAttachments{
					RequestID: request.RequestID,
					Prompt:    request.Prompt,
					Root:      attachmentRoot(),
				}
			}
		case CommandRequestSteer:
			root := attachmentRoot()
			request.SteeringPrompt.AttachmentRoot = &root
			command = request
		case CommandRequestCancel:
			if request.SteeringPrompt != nil {
				steering := *request.SteeringPrompt
				root := attachmentRoot()
				steering.AttachmentRoot = &root
				request.SteeringPrompt = &steering
			}
			command = request
		}
		if len(permits) == 0 {
			panic("every claimed command holds a reserved ACP command permit")
		}
		permits[0].send(command)
		permits = permits[1:]
		inFlight[claimed.CommandID] = claimed.Command
	}
	return nil
}

// dispatchPromptHasReferences is core.HasReferences for a stable-schema
// prompt. The attachment module reads prompts as the unstable ACP schema;
// the two schemas share the wire form, so the prompt crosses by its JSON.
func dispatchPromptHasReferences(prompt []acpschema.ContentBlock) bool {
	encoded, err := json.Marshal(prompt)
	if err != nil {
		return false
	}
	var unstable []acpunstable.ContentBlock
	if json.Unmarshal(encoded, &unstable) != nil {
		return false
	}
	return core.HasReferences(unstable)
}

func dispatchUserShells(relay *RelayCell, userShells *UserShellRegistry) error {
	guard := relay.Lock()
	claimed, err := guard.ClaimPendingUserShellCommandsUpTo(userShells.AvailableSlots())
	guard.Unlock()
	if err != nil {
		return err
	}
	for _, claim := range claimed {
		switch command := claim.Command.(type) {
		case core.RelayCommandRunUserShell:
			startErr := userShells.Start(claim.CommandID, command.Command)
			if startErr == nil {
				continue
			}
			message := startErr.Error()
			guard := relay.Lock()
			_, err := guard.RecordCommandCompleted(claim.CommandID, core.RelayCommandOutcomeUserShell{
				Result: core.UserShellResult{
					Command: command.Command,
					Status:  core.UserShellStatusFailed,
					Error:   &message,
				},
			})
			guard.Unlock()
			if err != nil {
				return err
			}
		case core.RelayCommandCancelUserShell:
			cancellation := userShells.Cancel(command.ShellCommandID)
			if err := func() error {
				guard := relay.Lock()
				defer guard.Unlock()
				if cancellation == UserShellCancelOutcomeNotRunning &&
					slices.ContainsFunc(guard.OperationalState().ActiveUserShells, func(shell core.ActiveUserShell) bool {
						return shell.CommandID == command.ShellCommandID
					}) {
					if _, err := guard.RecordCommandInterrupted(command.ShellCommandID, core.OutcomeReasonRequestedCancellation,
						"shell command was cancelled before it started"); err != nil {
						return err
					}
				}
				_, err := guard.RecordCommandCompleted(claim.CommandID, core.RelayCommandOutcomeUserShellCancelled{})
				return err
			}(); err != nil {
				return err
			}
		default:
			panic("only user shell commands are claimed here")
		}
	}
	return nil
}

// acpCommand is the ACP command a claimed relay command becomes; nil for a
// relay-local command.
func acpCommand(claimed *core.ClaimedRelayCommand) CommandRequest {
	requestID := claimed.CommandID
	switch command := claimed.Command.(type) {
	case core.RelayCommandClearContext:
		return CommandRequestClearContext{RequestID: requestID}
	case core.RelayCommandPrompt, core.RelayCommandHandbackReminder, core.RelayCommandHoldFollowUp,
		core.RelayCommandMailboxWake, core.RelayCommandContinueAuthorizedWork, core.RelayCommandResumeAfterQuota:
		blocks, ok := core.RelayCommandPromptBlocksForCommand(command, claimed.CommandID)
		if !ok {
			panic("prompt command")
		}
		prompt := slices.Clone(blocks)
		if claimed.HiddenPromptContext != nil {
			prompt = slices.Insert(prompt, 0, hiddenContextBlock(*claimed.HiddenPromptContext))
		}
		return CommandRequestPrompt{RequestID: requestID, Prompt: prompt}
	case core.RelayCommandSetConfig:
		return CommandRequestSetConfig{RequestID: requestID, Key: command.Key, Value: command.Value}
	case core.RelayCommandGoalControl:
		return CommandRequestGoalControl{RequestID: requestID, Action: command.Action}
	case core.RelayCommandSetSessionMode:
		return CommandRequestSetSessionMode{RequestID: requestID, ModeID: command.ModeID}
	case core.RelayCommandRestoreExecutionMode:
		return CommandRequestRestoreExecutionMode{RequestID: requestID}
	case core.RelayCommandSteer:
		if claimed.SteeringPrompt == nil {
			return nil
		}
		return CommandRequestSteer{
			RequestID:      requestID,
			ActivePromptID: command.ActivePromptID,
			SteeringPrompt: *claimed.SteeringPrompt,
		}
	case core.RelayCommandCancelTurnFor:
		return CommandRequestCancelTurnFor{RequestID: requestID, ActivePromptID: command.ActivePromptID}
	case core.RelayCommandApplyQueuedFor:
		return CommandRequestCancelTurnFor{RequestID: requestID, ActivePromptID: command.ActivePromptID}
	case core.RelayCommandCancelTurn:
		return CommandRequestCancel{RequestID: requestID}
	case core.RelayCommandCancel:
		var steering *core.ClaimedSteeringPrompt
		if claimed.SteeringPrompt != nil {
			copied := *claimed.SteeringPrompt
			steering = &copied
		}
		return CommandRequestCancel{RequestID: requestID, SteeringPrompt: steering}
	case core.RelayCommandClose:
		return CommandRequestClose{RequestID: requestID}
	}
	// BeginCheckpoint, the user shell commands, queue edits, checkpoint and
	// recovery controls, prompt context, notices, sub-agent waits,
	// assessment context, question and notification controls, quota
	// recovery, steering resolution and mailbox delivery stay in the relay.
	return nil
}

// selectResumeSession is which native thread this worker should try to
// resume: always the identity the journal recorded last, if there is one.
// The launch configuration can still name a thread this session already
// replaced. ACP decides how to open that identity using
// DurableRelay.nativeSessionHistory as evidence.
func selectResumeSession(config *core.WorkerLaunchConfig, relay *DurableRelay) *string {
	if recorded := relay.OperationalState().NativeSessionID; recorded != nil {
		return recorded
	}
	if config.NativeSessionID != nil {
		launched := *config.NativeSessionID
		return &launched
	}
	return nil
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
	if config.NativeSessionID != nil &&
		relay.OperationalState().NativeSessionID == nil &&
		!relay.RestoredNativeSessionUnused() {
		return relay.MarkNativeSessionUsed()
	}
	return nil
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
	barriers := make([]string, 0, len(checkpointBarriers))
	for commandID := range checkpointBarriers {
		barriers = append(barriers, commandID)
	}
	sort.Strings(barriers)
	released := false
	if err := func() error {
		guard := relay.Lock()
		defer guard.Unlock()
		for _, commandID := range barriers {
			_, ok, err := guard.CancelCheckpointBarrierOnDisconnect(commandID)
			if err != nil {
				return fmt.Errorf("release disconnected checkpoint barrier %s: %w", commandID, err)
			}
			released = released || ok
		}
		return nil
	}(); err != nil {
		return err
	}
	if released {
		if err := wakeDispatch(relay, dispatchWake); err != nil {
			return fmt.Errorf("wake relay after releasing checkpoint barrier: %w", err)
		}
	}
	return nil
}

// wakeDispatch tells the coordinator durable work may be available, or
// dispatches a checkpoint-only relay's work itself.
func wakeDispatch(relay *RelayCell, dispatchWake *dispatchWakeChannel) error {
	checkpointOnly, err := func() (bool, error) {
		guard := relay.Lock()
		defer guard.Unlock()
		if guard.OperationalState().CheckpointOnly {
			return true, guard.DispatchCheckpointOnly()
		}
		return false, nil
	}()
	if checkpointOnly {
		return err
	}
	if err := dispatchWake.trySend(); err == nil {
		return nil
	}
	guard := relay.Lock()
	closed := guard.OperationalState().Execution == core.RelayExecutionStateClosed
	guard.Unlock()
	if closed {
		return nil
	}
	return errors.New("relay coordinator stopped")
}
