// Port of mj-worker/src/relay.rs.
//
// Persistent, transport-neutral protocol core for a Hel target worker.
//
// The worker never listens on a network port. Controllers speak newline-
// delimited JSON through `mj worker proxy`, which can itself be carried over
// SSH or a container exec stream.
//
// This file is the root of the relay implementation. It keeps the
// DurableRelay type itself: opening and recovering a session, the
// operational-state and activity clocks, and recording observations and
// session updates. The rest of the implementation sits in sibling files:
// relay_requests.go answers relay envelopes, relay_commands.go owns the
// command queue and the claim scheduler, relay_background.go tracks
// agent-owned background work and terminals, relay_replay.go serves replay
// pages and cursor validation, and relay_journal.go does durable journal I/O.
// The wire protocol and the deterministic event/snapshot state machine are
// defined in package core and used by both runtime owners.

package worker

import (
	"cmp"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// InPlaceHarnessRestartDecision is whether an in-place harness restart
// still has to run for its operation.
type InPlaceHarnessRestartDecision int

const (
	InPlaceHarnessRestartProceed InPlaceHarnessRestartDecision = iota
	InPlaceHarnessRestartAlreadyRestarted
)

// HarnessTurnPolicy is whether this relay models turns the harness starts on
// its own.
//
// The harness-specific policy chooses which signals open and settle a turn.
// See .agents/docs/claude-autonomous-turns.md.
type HarnessTurnPolicy int

const (
	HarnessTurnPolicyDisabled HarnessTurnPolicy = iota
	HarnessTurnPolicyClaudeAdapter
	HarnessTurnPolicyCodexAdapter
	HarnessTurnPolicyAntigravity
)

// SteeringDeliveryMode is how this harness receives an accepted steer.
type SteeringDeliveryMode int

const (
	SteeringDeliveryModeAcp SteeringDeliveryMode = iota
	// SteeringDeliveryModeAcpWithMailbox: the harness has no mailbox hook;
	// carry waking events through its ordinary durable prompt queue and
	// safe-boundary steering instead.
	SteeringDeliveryModeAcpWithMailbox
	SteeringDeliveryModeHookPostToolBatch
	SteeringDeliveryModeHookTurnEnd
)

// steeringDeliveryModeForHarness is SteeringDeliveryMode::for_harness.
func steeringDeliveryModeForHarness(harness core.HarnessKind, boundaryHook bool) SteeringDeliveryMode {
	hookHarness := harness == core.HarnessKindClaude || harness == core.HarnessKindAntigravity
	switch {
	case harness == core.HarnessKindDsh:
		return SteeringDeliveryModeAcpWithMailbox
	case hookHarness && boundaryHook:
		return SteeringDeliveryModeHookPostToolBatch
	case hookHarness:
		return SteeringDeliveryModeHookTurnEnd
	default:
		return SteeringDeliveryModeAcp
	}
}

func (m SteeringDeliveryMode) usesAcp() bool {
	return m == SteeringDeliveryModeAcp || m == SteeringDeliveryModeAcpWithMailbox
}

// HarnessTurnStopGrace is how long a stop sent during a turn Claude Code
// started on its own waits for the result that ends an interrupted cycle
// before the relay ends the turn itself.
const HarnessTurnStopGrace = 5 * time.Second

// BackgroundWorkPolicy is where this relay learns about commands the agent
// left running: the authoritative source of agent-owned background commands.
type BackgroundWorkPolicy int

const (
	// BackgroundWorkPolicyHostedTerminals: live terminals Hel spawned for the
	// agent.
	BackgroundWorkPolicyHostedTerminals BackgroundWorkPolicy = iota
	// BackgroundWorkPolicyAirTasks: AIR tasks announced by Claude or Codex,
	// together with hosted terminals.
	BackgroundWorkPolicyAirTasks
	// BackgroundWorkPolicyKimiTasks: Kimi's native detached-agent set,
	// together with hosted terminals.
	BackgroundWorkPolicyKimiTasks
	BackgroundWorkPolicyAntigravityTasks
)

// LiveBaselineResult is what a live baseline reservation receives: the cut,
// or the error that refused it (Rust's Result<LiveSessionBaseline, String>).
type LiveBaselineResult struct {
	Baseline core.LiveSessionBaseline
	Err      error
}

// liveBaselineReply is the sending half of a reservation: Rust's oneshot
// sender. closed is set when the receiver gives up, as a dropped oneshot
// receiver closes the channel.
type liveBaselineReply struct {
	result chan LiveBaselineResult
	closed atomic.Bool
}

// LiveBaselineReceiver receives the live recovery cut a reservation asked
// for. Close it when the wait ends without one; the relay then drops the
// reservation, as it does for a dropped Rust oneshot receiver.
type LiveBaselineReceiver struct {
	reply *liveBaselineReply
}

// C delivers the cut once the worker is quiescent.
func (r *LiveBaselineReceiver) C() <-chan LiveBaselineResult { return r.reply.result }

// TryRecv returns the cut if it has been sent.
func (r *LiveBaselineReceiver) TryRecv() (LiveBaselineResult, bool) {
	select {
	case result := <-r.reply.result:
		return result, true
	default:
		return LiveBaselineResult{}, false
	}
}

// Close withdraws the reservation.
func (r *LiveBaselineReceiver) Close() { r.reply.closed.Store(true) }

// PendingLiveBaseline is one connection's queue hold until its cut or
// disconnection. reply is nil once the cut was sent.
type PendingLiveBaseline struct {
	reply *liveBaselineReply
}

type PendingNativeResume struct {
	resumeID uint64
	goal     *core.GoalSnapshot
}

// RelayCell is the relay behind its lock. Every change to the relay takes
// this lock through Lock, so each such release is counted as a possible
// change: a connection waiting for one (see core.RelayRequestWaitForChange)
// wakes and compares what it is waiting on, without the relay listing its
// mutations. A waiting connection reads through Observe, which cannot change
// the relay and is not counted, so waits never wake one another.
//
// Go has no lock poisoning, so Lock and Observe return the guard alone.
type RelayCell struct {
	mu       sync.Mutex
	relay    *DurableRelay
	released *relayReleaseWatch
	// guardian holds the worker's pending guardian reviews for the daemon,
	// across relay connections. Rust keeps it on Unix only, where the live
	// guardian relay is served.
	guardian *GuardianEndpoint
}

// relayReleaseWatch is the Go form of the tokio watch channel carrying the
// release count: changed is closed and replaced on every release.
type relayReleaseWatch struct {
	mu      sync.Mutex
	count   uint64
	changed chan struct{}
}

func (w *relayReleaseWatch) bump() {
	w.mu.Lock()
	w.count++
	close(w.changed)
	w.changed = make(chan struct{})
	w.mu.Unlock()
}

func NewRelayCell(relay *DurableRelay) *RelayCell {
	return &RelayCell{relay: relay, released: &relayReleaseWatch{changed: make(chan struct{})}, guardian: &GuardianEndpoint{}}
}

// Lock takes the relay for a possible change. Unlock the guard to release
// it; the release is counted.
func (c *RelayCell) Lock() *RelayGuard {
	c.mu.Lock()
	return &RelayGuard{DurableRelay: c.relay, cell: c}
}

// Observe reads the relay without counting as a possible change. The guard
// only reads, so nothing a change wait must see can happen under it.
func (c *RelayCell) Observe() *RelayObserveGuard {
	c.mu.Lock()
	return &RelayObserveGuard{DurableRelay: c.relay, cell: c}
}

// Releases changes on every release of a Lock guard.
func (c *RelayCell) Releases() *RelayReleases {
	c.released.mu.Lock()
	defer c.released.mu.Unlock()
	return &RelayReleases{watch: c.released, seen: c.released.count}
}

// RelayGuard is the relay held for a possible change (Rust's RelayGuard,
// whose Deref the embedded pointer stands in for).
type RelayGuard struct {
	*DurableRelay
	cell *RelayCell
}

// Unlock releases the relay and counts the release (Rust's Drop). A woken
// waiter takes the lock to look, so it sees this holder's writes once the
// mutex below is released.
func (g *RelayGuard) Unlock() {
	g.cell.released.bump()
	g.DurableRelay = nil
	g.cell.mu.Unlock()
}

// RelayObserveGuard is the relay held for reading only (Rust's MutexGuard
// returned by observe).
type RelayObserveGuard struct {
	*DurableRelay
	cell *RelayCell
}

// Unlock releases the relay without counting a change.
func (g *RelayObserveGuard) Unlock() {
	g.DurableRelay = nil
	g.cell.mu.Unlock()
}

// RelayReleases is the receiving side of the release count (Rust's
// tokio::sync::watch::Receiver<u64>).
type RelayReleases struct {
	watch *relayReleaseWatch
	seen  uint64
}

// BorrowAndUpdate marks the current count seen and returns it.
func (r *RelayReleases) BorrowAndUpdate() uint64 {
	r.watch.mu.Lock()
	defer r.watch.mu.Unlock()
	r.seen = r.watch.count
	return r.seen
}

// Changed waits for a release after the last one seen, then marks it seen.
func (r *RelayReleases) Changed(ctx context.Context) error {
	for {
		r.watch.mu.Lock()
		count, changed := r.watch.count, r.watch.changed
		r.watch.mu.Unlock()
		if count != r.seen {
			r.seen = count
			return nil
		}
		select {
		case <-changed:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// NativeSessionHistory is relay evidence about the native thread named at
// launch.
type NativeSessionHistory int

const (
	// NativeSessionHistoryMayHaveHistory: the thread may contain history and
	// must not be replaced.
	NativeSessionHistoryMayHaveHistory NativeSessionHistory = iota
	// NativeSessionHistoryLoadBeforeReplace: the thread may be replaced only
	// if the harness reports it missing.
	NativeSessionHistoryLoadBeforeReplace
	// NativeSessionHistoryCreatedHereUnused: durable evidence proves this
	// relay created the thread and never used it.
	NativeSessionHistoryCreatedHereUnused
)

// relayOrdinalDigest is one retained replay cursor (Rust's (u64, String)).
type relayOrdinalDigest struct {
	ordinal uint64
	digest  string
}

// KimiNativeAgentEntry is one value of the map kimi_native_agents_changed
// takes (Rust's (KimiBackgroundTask, NativeAgentState) tuple).
type KimiNativeAgentEntry struct {
	Task  KimiBackgroundTask
	State core.NativeAgentState
}

// DurableRelay is the durable, session-side ACP store-and-forward relay.
//
// core.RelaySnapshot is the canonical operational state. Journal spans retain
// observations newer than the durable controller acknowledgement.
// Operation-owned command receipts survive separately until their consumers
// finish.
type DurableRelay struct {
	// initialCommands is immutable input supplied when this worker was
	// created; execution and completion live exclusively in the ordinary
	// durable command queue.
	initialCommands []core.InitialWorkerCommand
	// operatorAway is updated only by the Unix worker runtime from
	// SetOperatorAway.
	operatorAway *OperatorAway
	root         string
	relayVersion string
	// workerBuild is the content address of the executable serving this
	// relay, reported in hello. The crate version cannot tell two builds of
	// the same release apart, and a controller has to know whether the
	// worker it is talking to is the binary it would install today.
	workerBuild *string
	// harnessReadyMs is the epoch milliseconds at which the harness session
	// was last configured, or 0 while it is not: the one owner of ACP process
	// readiness, which is never recovered from the journal. isAcpReady
	// derives the readiness flag from it, and the work-facts scan reads the
	// instant to tell the harness's own processes, which exist by then, from
	// work the agent started, so the two cannot disagree.
	harnessReadyMs *atomic.Int64
	// sessionSetup: a bridge has connected and its session is not open yet:
	// the load or new-session request and the model and mode applied after
	// it. Output in this window answers no prompt (R8-4). Never recovered
	// from the journal: every bridge start records its own AgentInitialized.
	sessionSetup   bool
	checkpointOnly bool
	// inPlaceHarnessRestartInFlight is the active in-place harness restart
	// operation ID, serialized with the hold.
	inPlaceHarnessRestartInFlight *string
	// reviewerAdmissions: live reviewer leases share this owner's atomic
	// checkpoint decision.
	reviewerAdmissions    map[uint64]string
	nextReviewerAdmission uint64
	// steeringSupported is the optional extension advertised by the current
	// ACP process.
	steeringSupported *bool
	// automaticSteering: the connected bridge advertises steering, so the
	// relay may steer queued prompts on its own; the bridge queues them to
	// its next safe boundary. Belongs to the bridge, like readiness.
	automaticSteering bool
	// steeringDeliveryMode is how this harness receives an accepted steer.
	// Claude never uses ACP steering because its bridge can cancel an
	// in-flight tool hook.
	steeringDeliveryMode SteeringDeliveryMode
	// mailboxDeliveryMode is when this worker delivers mailbox messages to a
	// working Codex session, resolved from its staged provider. Nil until
	// preparation resolves it, and for harnesses whose delivery is not
	// classified here; nil never permits a PostToolUse delivery.
	mailboxDeliveryMode *core.MailboxDeliveryMode
	snapshot            core.RelaySnapshot
	// journalSpans are canonical, non-overlapping slices of the durable
	// journal. Event bodies stay on disk; only enough metadata to locate a
	// requested ordinal is retained in memory.
	journalSpans []RelayJournalSpan
	// hotEvents is a small optimization for recent cursor validation, oldest
	// first (Rust's VecDeque). This is never the source of truth and is
	// deliberately fixed-size (core.RelayHotEventCapacity).
	hotEvents []core.RelayEvent
	// replayCursors are digests proven while serving older replay pages,
	// oldest first. Controllers normally return the previous page's
	// frontier, so retaining those sparse cursors avoids decompressing that
	// segment again merely to validate the next request. Event bodies remain
	// on disk and authoritative.
	replayCursors []relayOrdinalDigest
	// unpersistedJournalBytes is the journal bytes appended since
	// relay-state.json last matched this snapshot. Zero means the durable
	// file is exactly this state.
	unpersistedJournalBytes int
	// journalGeneration is bumped whenever a captured RelayReplayPlan stops
	// describing the journal on disk: sealing moves the active segment's
	// events into a new file, and garbage collection rewrites and prunes
	// segments. Plain appends never bump it; they only extend the active
	// segment, which a lock-free reader already tolerates.
	journalGeneration uint64
	// journalCleanupPending retries physical prefix cleanup once after
	// reopen, or after a failed GC.
	journalCleanupPending bool
	// journalAppendFailure: a failed rollback left the on-disk append
	// boundary unknown.
	journalAppendFailure *string
	// journalAppendFault is a test-only injected append failure.
	journalAppendFault   *journalAppendFault
	pendingLiveBaselines []PendingLiveBaseline
	pendingNativeResumes []PendingNativeResume
	acpActivity          core.AcpActivityClock
	stepClock            *core.StepClock
	// harnessTurns is whether harness announcements or output with no prompt
	// open a turn. The runtime sets this from the configured harness right
	// after open.
	harnessTurns HarnessTurnPolicy
	// backgroundWork is where commands the agent left running are read from.
	backgroundWork BackgroundWorkPolicy
	// foregroundTools are tool calls that still report pending or
	// in-progress, with the start of their current status. This is stronger
	// foreground evidence than a harness-neutral step clock, whose prose
	// steps have no portable ending.
	//
	// A shared handle rather than a private map: the turn stall watchdog
	// reads the same tool calls, and when it could not, a turn blocked in a
	// long build was failed as if the harness had died (#1020).
	foregroundTools *core.ToolsInFlight
	turnContext     *transcript.TurnContext
	// decisionLog is the Jev decision log this relay opened in its root and
	// owns; Shutdown closes it. The turn context lends it to classifiers.
	decisionLog    *core.DecisionLog
	verdictHarness *core.HarnessKind
	jevDisabled    bool
	// authenticationLoginPath is read only by the Unix worker control runtime.
	authenticationLoginPath *string
	loginIdentity           *ClaudeLoginIdentity
	repliedVerdict          RepliedVerdictState
	recoversChildQuota      bool
	// airTasks are adapter-owned tasks, keyed by their owning session and
	// task identity (airTaskKey).
	airTasks map[string]*AirTaskEntry
	// pendingSettleFacts are work facts the coordinator observed for the
	// runtime event it is about to apply (see EventSettlesAutonomousTurn).
	// Only a settlement of a turn the harness started on its own takes them;
	// every event application replaces them, so a stale observation never
	// reaches a later turn.
	pendingSettleFacts *core.WorkFacts
	// claudePendingStops are Claude tasks whose stop this process requested
	// and the adapter has not yet acknowledged, by session then task key. The
	// adapter answers a stop with a plain agent chunk and no model cycle, so
	// no result would settle a harness turn opened for that chunk.
	claudePendingStops map[string]map[string]struct{}
	antigravityWork    antigravityWorkState
	// kimiBackgroundTasks are Kimi detached agents and processes confirmed by
	// its native journal.
	kimiBackgroundTasks map[string]KimiTaskEntry
	// kimiProvisionalTasks is positive ACP launch evidence retained until the
	// native journal correlates it through native task identity or
	// parentToolCallId.
	kimiProvisionalTasks map[string]KimiProvisionalTask
	kimiObservedTaskIDs  map[string]struct{}
	kimiObservedToolIDs  map[string]struct{}
	// backgroundWorkKnown is whether provider-owned background work is
	// synchronized. Other policies leave this nil.
	backgroundWorkKnown *bool
	// handbackVerdictClient is the classifier the sub-agent handback check
	// asks. Only the Unix worker sets and reads it.
	handbackVerdictClient *VerdictClient
	// taskSettledAtMs is when a Claude background task last completed or
	// failed, until the harness turn that answers it opens. Claude Code
	// always follows such a settle with a task-notification turn; publishing
	// the settle time lets every quiet check treat the gap as a turn in
	// flight.
	taskSettledAtMs *int64
	// activeAgentTerminals: ACP terminals are connection-owned and disappear
	// when that connection is torn down, so they belong in memory rather
	// than the durable relay snapshot or transcript journal.
	activeAgentTerminals map[string]core.ActiveAgentTerminal
	// closedAgentTerminals is a short-lived guard against a fast child
	// reporting close before the create handler publishes its started event
	// on the shared channel.
	closedAgentTerminals map[string]struct{}
	// agentTerminalToolCalls is the hosted terminal each agent tool call
	// embedded, keyed by the native tool call id. Kimi runs a detached shell
	// in one of our terminals, so this is what ties its native task record to
	// the terminal doing the work.
	agentTerminalToolCalls map[string]string
	// heldWrites are the worker's own writes waiting for the ready checkpoint
	// barrier to end, in arrival order. See relay_checkpoint_hold.go.
	heldWrites []HeldWrite
	// holdEndedFor is the barrier whose hold something that could not wait
	// already ended.
	holdEndedFor *string
	// cutSpoiledFor is the barrier whose cut background work started after.
	cutSpoiledFor *string
	// knownBackgroundWork is the background work the relay last saw, to tell
	// work that starts from work already running.
	knownBackgroundWork map[string]struct{}
	// stageSnapshotEveryAppend is a benchmark aid (Rust cfg(test)): stage and
	// persist a snapshot on every append, the way the relay did before
	// transcript appends were amortized, so both policies can be timed in one
	// process.
	stageSnapshotEveryAppend bool
}

// ReserveLiveBaseline reserves a live recovery cut. The connection owns this
// queue hold until its cut or disconnection.
func (r *DurableRelay) ReserveLiveBaseline() *LiveBaselineReceiver {
	reply := &liveBaselineReply{result: make(chan LiveBaselineResult, 1)}
	r.pendingLiveBaselines = append(r.pendingLiveBaselines, PendingLiveBaseline{reply: reply})
	r.serviceLiveBaselines()
	return &LiveBaselineReceiver{reply: reply}
}

func (r *DurableRelay) resumeAfterAdoptionWait() error {
	_, _, err := r.promoteNextQueuedCommand()
	return err
}

func (r *DurableRelay) liveBaselinePending() bool {
	r.serviceLiveBaselines()
	for _, request := range r.pendingLiveBaselines {
		if request.reply != nil {
			return true
		}
	}
	return false
}

func (r *DurableRelay) serviceLiveBaselines() {
	kept := r.pendingLiveBaselines[:0]
	for _, request := range r.pendingLiveBaselines {
		if request.reply != nil && !request.reply.closed.Load() {
			kept = append(kept, request)
		}
	}
	clear(r.pendingLiveBaselines[len(kept):])
	r.pendingLiveBaselines = kept
	waiting := false
	for _, request := range r.pendingLiveBaselines {
		if request.reply != nil {
			waiting = true
			break
		}
	}
	if !waiting || !r.quiescentForAdoption() {
		return
	}
	baseline, err := r.LiveSessionBaseline()
	cut := LiveBaselineResult{Baseline: baseline, Err: err}
	for i := range r.pendingLiveBaselines {
		reply := r.pendingLiveBaselines[i].reply
		if reply == nil {
			continue
		}
		r.pendingLiveBaselines[i].reply = nil
		// The reply channel holds the one result; a receiver that gave up
		// leaves it unread, as a send to a dropped oneshot is ignored.
		select {
		case reply.result <- cut:
		default:
		}
	}
}

func (r *DurableRelay) nativeGoalResumeSent(resumeID uint64, goal *core.GoalSnapshot) {
	if goal == nil && r.snapshot.Goal.Snapshot != nil {
		snapshot := *r.snapshot.Goal.Snapshot
		goal = &snapshot
	}
	r.pendingNativeResumes = append(r.pendingNativeResumes, PendingNativeResume{resumeID: resumeID, goal: goal})
}

func (r *DurableRelay) nativeGoalResumeRejected(resumeID uint64) {
	kept := r.pendingNativeResumes[:0]
	for _, pending := range r.pendingNativeResumes {
		if pending.resumeID != resumeID {
			kept = append(kept, pending)
		}
	}
	r.pendingNativeResumes = kept
	r.serviceLiveBaselines()
}

func (r *DurableRelay) quiescentForAdoption() bool {
	return len(r.adoptionBlockers()) == 0
}

func (r *DurableRelay) adoptionBlockers() []string {
	var blockers []string
	if r.turnInProgress() || r.hasHeldClaudeTurn() {
		if len(r.snapshot.AssessmentQuestions) == 0 {
			blockers = append(blockers, "the worker's current turn to finish")
		} else {
			blockers = append(blockers, "the worker's current turn, blocked on a pending question or approval, to finish")
		}
	}
	if r.turnCancellationPending() {
		blockers = append(blockers, "the pending turn cancellation to finish")
	}
	if r.snapshot.ClearContextInProgress() {
		blockers = append(blockers, "the worker's context reset (/clear) to finish")
	}
	goalResume := len(r.pendingNativeResumes) > 0
	for _, dispatch := range r.snapshot.Dispatches {
		control, ok := dispatch.Command.(core.RelayCommandGoalControl)
		if ok && control.Action == core.GoalControlActionResume &&
			(dispatch.State == core.RelayDispatchStatePending || dispatch.State == core.RelayDispatchStateInFlight) {
			goalResume = true
			break
		}
	}
	if goalResume {
		blockers = append(blockers, "the accepted native goal resume to finish")
	}
	if r.configurationDispatchInProgress() {
		blockers = append(blockers, "the in-flight configuration change to finish")
	}
	for _, tool := range r.foregroundTools.Snapshot() {
		name := tool.ToolCallID
		if tool.Title != nil {
			name = *tool.Title
		}
		blockers = append(blockers, fmt.Sprintf("foreground tool %s to finish", name))
	}
	for _, id := range slices.Sorted(maps.Keys(r.snapshot.ActiveUserShells)) {
		shell := r.snapshot.ActiveUserShells[id]
		if shell.StartedAtMs != nil {
			blockers = append(blockers, fmt.Sprintf("running shell %s to finish", core.RustDebugString(shell.Command)))
		}
	}
	for _, id := range slices.Sorted(maps.Keys(r.activeAgentTerminals)) {
		blockers = append(blockers, fmt.Sprintf("live harness terminal %s to exit", core.RustDebugString(r.activeAgentTerminals[id].Command)))
	}
	slices.Sort(blockers)
	return blockers
}

func (r *DurableRelay) LiveSessionBaseline() (core.LiveSessionBaseline, error) {
	state := r.OperationalState()
	if !r.quiescentForAdoption() {
		return core.LiveSessionBaseline{}, errors.New("live recovery cut requires a quiescent worker")
	}
	state.Execution = core.RelayExecutionStateIdle
	state.ActivityTurnStartedAtMs = nil
	queue := make([]core.LiveQueuedCommand, 0, len(r.snapshot.QueuedPrompts))
	for _, queued := range r.snapshot.QueuedPrompts {
		dispatch, ok := r.snapshot.Dispatches[queued.CommandID]
		if !ok {
			return core.LiveSessionBaseline{}, fmt.Errorf("queued command %s has no dispatch", queued.CommandID)
		}
		var accepted *uint64
		if receipt, ok := r.snapshot.HandledCommands[queued.CommandID]; ok {
			ordinal := receipt.AcceptedOrdinal
			accepted = &ordinal
		}
		queue = append(queue, core.LiveQueuedCommand{
			CommandID:       queued.CommandID,
			Command:         dispatch.Command,
			CreatedAtMs:     queued.CreatedAtMs,
			AcceptedOrdinal: accepted,
		})
	}
	delivered := []core.LiveQueuedCommand{}
	for _, initial := range r.initialCommands {
		if _, prompt := initial.Command.(core.RelayCommandPrompt); !prompt ||
			!relaySortedSetContains(r.snapshot.InitialCommandIDs, initial.CommandID) ||
			slices.ContainsFunc(r.snapshot.QueuedPrompts, func(queued core.StoredQueuedRelayCommand) bool {
				return queued.CommandID == initial.CommandID
			}) {
			continue
		}
		receipt, received := r.snapshot.HandledCommands[initial.CommandID]
		var wasDelivered bool
		switch {
		case !received:
			// A legacy pin prevents reseeding, but acceptance alone cannot
			// distinguish delivery from removal before dispatch.
			wasDelivered = false
		case receipt.Failure != nil && *receipt.Failure == "queued command was removed":
			wasDelivered = false
		default:
			_, promptOutcome := receipt.Outcome.(core.RelayCommandOutcomePrompt)
			// Receipts do not distinguish failure before/after start.
			// Preserve the delivered interpretation for every failure
			// except queue removal.
			wasDelivered = promptOutcome || receipt.Failure != nil
		}
		if !wasDelivered {
			continue
		}
		var createdAtMs int64
		if active := state.ActivePrompt; active != nil && active.CommandID == initial.CommandID {
			createdAtMs = active.CreatedAtMs
		}
		var accepted *uint64
		if received {
			ordinal := receipt.AcceptedOrdinal
			accepted = &ordinal
		}
		delivered = append(delivered, core.LiveQueuedCommand{
			CommandID: initial.CommandID,
			// Startup owns the original brief even after terminal receipt
			// compaction. Receipts decide delivery, never display content.
			Command:         initial.Command,
			CreatedAtMs:     createdAtMs,
			AcceptedOrdinal: accepted,
		})
	}
	liveQuestions := r.operatorAway.liveQuestions()
	questions := []core.ElicitationRequest{}
	unavailable := 0
	for _, id := range slices.Sorted(maps.Keys(r.snapshot.PendingElicitations)) {
		body := r.snapshot.PendingElicitations[id]
		live, isLive := liveQuestions[id]
		if body != nil && isLive && reflect.DeepEqual(live, *body) {
			questions = append(questions, *body)
		}
		if isLive && body == nil {
			unavailable++
		}
	}
	var nativeAgentReplay *[]core.NativeAgent
	if r.snapshot.NativeAgentReplay != nil {
		agents := make([]core.NativeAgent, 0, len(r.snapshot.NativeAgentReplay))
		for _, id := range slices.Sorted(maps.Keys(r.snapshot.NativeAgentReplay)) {
			agents = append(agents, r.snapshot.NativeAgentReplay[id])
		}
		nativeAgentReplay = &agents
	}
	var evidence *core.NativeSessionUseEvidence
	if r.snapshot.NativeUseEvidence != nil {
		copied := *r.snapshot.NativeUseEvidence
		evidence = &copied
	}
	return core.LiveSessionBaseline{
		DeliveredInitialPrompts: delivered,
		State:                   state,
		Queue:                   queue,
		Questions:               questions,
		UnavailableQuestions:    unavailable,
		NativeAgentReplay:       nativeAgentReplay,
		NativeUseEvidence:       evidence,
	}, nil
}

// relaySortedSetContains is BTreeSet::contains on a sorted, unique slice.
func relaySortedSetContains(values []string, value string) bool {
	_, found := slices.BinarySearch(values, value)
	return found
}

// relaySortedSetInsert is BTreeSet::insert on a sorted, unique slice.
func relaySortedSetInsert(values []string, value string) []string {
	index, found := slices.BinarySearch(values, value)
	if found {
		return values
	}
	return slices.Insert(values, index, value)
}

// DurableRelayOpen is DurableRelay::open. The Unix worker is the only
// production caller.
func DurableRelayOpen(root, sessionID, relayVersion string) (*DurableRelay, error) {
	return durableRelayOpenWithHarness(root, sessionID, relayVersion, false, nil, nil)
}

// DurableRelayOpenForHarness opens a live relay with its harness delivery
// mode established before restart recovery can settle interrupted turns.
func DurableRelayOpenForHarness(root, sessionID, relayVersion string, harness core.HarnessKind, initialCommands []core.InitialWorkerCommand) (*DurableRelay, error) {
	return durableRelayOpenWithHarness(root, sessionID, relayVersion, false, &harness, initialCommands)
}

// DurableRelayOpenForCheckpoint opens existing durable state without
// promoting work onto a harness.
func DurableRelayOpenForCheckpoint(root, sessionID, relayVersion string) (*DurableRelay, error) {
	return DurableRelayOpenForCheckpointWithInitialCommands(root, sessionID, relayVersion, nil)
}

// DurableRelayOpenForCheckpointWithInitialCommands preserves launch receipt
// owners during checkpoint-only upgrades without seeding work.
func DurableRelayOpenForCheckpointWithInitialCommands(root, sessionID, relayVersion string, initialCommands []core.InitialWorkerCommand) (*DurableRelay, error) {
	if info, err := os.Stat(filepath.Join(root, core.RelayStateFile)); err != nil || !info.Mode().IsRegular() {
		return nil, errors.New("checkpoint recovery requires existing relay state")
	}
	return durableRelayOpenWithHarness(root, sessionID, relayVersion, true, nil, initialCommands)
}

func durableRelayDiscardQueuedPromptsBeforeStart(root, sessionID, commandID string) error {
	relay, err := durableRelayOpenWithHarness(root, sessionID, core.PackageVersion, true, nil, nil)
	if err != nil {
		return err
	}
	_, refusal, err := relay.submitCommand(commandID, core.RelayCommandClearQueuedPrompts{})
	if err != nil {
		return err
	}
	if refusal != nil {
		return fmt.Errorf("discard queued prompts before startup: %s", refusal.Message)
	}
	return nil
}

// readLegacyRelaySnapshot reads relay-state.json. Legacy snapshots stored
// pretty JSON and duplicated terminal bodies, so the read is bounded
// independently of the compact write budget.
func readLegacyRelaySnapshot(statePath string) (core.RelaySnapshot, error) {
	file, err := os.Open(statePath)
	if err != nil {
		return core.RelaySnapshot{}, err
	}
	defer file.Close()
	bytes, err := io.ReadAll(io.LimitReader(file, int64(core.RelaySnapshotByteBudget*4+1)))
	if err != nil {
		return core.RelaySnapshot{}, fmt.Errorf("read %s: %w", statePath, err)
	}
	if err := core.EnsureByteBudget(len(bytes), core.RelaySnapshotByteBudget*4, "legacy relay snapshot"); err != nil {
		return core.RelaySnapshot{}, err
	}
	var snapshot core.RelaySnapshot
	if err := json.Unmarshal(bytes, &snapshot); err != nil {
		return core.RelaySnapshot{}, fmt.Errorf("parse %s: %w", statePath, err)
	}
	return snapshot, nil
}

// restoredRelaySnapshot is the first snapshot of a relay restored from a
// checkpoint, or a fresh one when nothing was restored.
func restoredRelaySnapshot(root, sessionID string) (core.RelaySnapshot, error) {
	snapshot := core.NewRelaySnapshot(sessionID)
	restored, err := readRestoredRelaySeed(root)
	if err != nil || restored == nil {
		return snapshot, err
	}
	snapshot.AssessmentContext = nil
	if len(restored.AssessmentState) > 0 && string(restored.AssessmentState) != "null" {
		state, err := core.CheckpointDecode(restored.AssessmentState)
		if err != nil {
			return snapshot, err
		}
		snapshot.AssessmentContext = state.Context
		snapshot.Assessment = state.Assessment
		snapshot.Continuation = state.Continuation
		snapshot.CapacityRetry = state.CapacityRetry
		snapshot.TurnCompletion = state.TurnCompletion
		if open := state.OpenRequest; open != nil {
			snapshot.Request = open.Request
			snapshot.PersonInputThrough = open.PersonInputThrough
		} else {
			seedLegacyOpenRequest(&snapshot)
		}
	}
	snapshot.LatestOrdinal = restored.EventFrontier
	if restored.NotificationStack != nil {
		snapshot.NotificationStack = *restored.NotificationStack
		seedMissingNotificationTime(&snapshot)
	} else {
		seedLegacyNotificationStack(&snapshot)
	}
	snapshot.LatestDigest = restored.EventFrontierDigest
	snapshot.AcknowledgedThrough = restored.EventFrontier
	snapshot.AcknowledgedDigest = restored.EventFrontierDigest
	snapshot.RecoveryFloorOrdinal = restored.EventFrontier
	snapshot.RecoveryFloorDigest = restored.EventFrontierDigest
	// The accepted model and effort the worker pins on its first bridge
	// start, exactly as it does after a restart.
	for key, value := range restored.AcceptedConfig {
		snapshot.Config[key] = value
	}
	snapshot.RestoredNativeSessionUnused = restored.NativeSessionUnused
	if evidence := restored.NativeUseEvidence; evidence != nil {
		snapshot.NativeSessionUsed = evidence.Usage == core.NativeSessionUseUsed
		copied := *evidence
		snapshot.NativeUseEvidence = &copied
	} else if restored.NativeSessionUnused {
		snapshot.RecordNativeUse(core.NativeSessionUseUnused)
	} else {
		snapshot.RecordNativeUse(core.NativeSessionUseUnknown)
	}
	for _, queued := range restored.QueuedPrompts {
		if !core.ValidCommandID(queued.CommandID) {
			return snapshot, errors.New("invalid restored queued command ID")
		}
		if len(queued.Content) == 0 {
			return snapshot, fmt.Errorf("restored queued command %s is empty", queued.CommandID)
		}
		if _, duplicate := snapshot.HandledCommands[queued.CommandID]; duplicate {
			return snapshot, fmt.Errorf("restored canonical session contains duplicate queued command %s", queued.CommandID)
		}
		var payload core.StoredQueuedRelayPayload
		var command core.RelayCommand
		switch queued.Kind.Kind {
		case core.CanonicalQueuedCommandKindSetConfig:
			payload = core.StoredQueuedRelayPayloadSetConfig{Key: queued.Kind.Key, Value: queued.Kind.Value}
			command = core.RelayCommandSetConfig{Key: queued.Kind.Key, Value: queued.Kind.Value}
		default:
			prompt := make([]acpschema.ContentBlock, 0, len(queued.Content))
			for _, content := range queued.Content {
				var block acpschema.ContentBlock
				if err := json.Unmarshal(content, &block); err != nil {
					return snapshot, fmt.Errorf("decode ACP content for restored queued command %s: %w", queued.CommandID, err)
				}
				prompt = append(prompt, block)
			}
			payload = core.StoredQueuedRelayPayloadPrompt{Prompt: prompt}
			command = core.RelayCommandPrompt{Prompt: slices.Clone(prompt)}
		}
		snapshot.HandledCommands[queued.CommandID] = core.HandledRelayCommand{
			Command:         command,
			AcceptedOrdinal: restored.EventFrontier,
		}
		snapshot.Dispatches[queued.CommandID] = core.RelayDispatchRecord{
			Command: command,
			State:   core.RelayDispatchStateQueued,
		}
		snapshot.QueuedPrompts = append(snapshot.QueuedPrompts, core.StoredQueuedRelayCommand{
			CommandID:   queued.CommandID,
			Payload:     payload,
			CreatedAtMs: queued.QueuedAtMs,
		})
	}
	// The archived worker's mail, as the restore policy selected it. A
	// claimed context may name a prompt the controller admits later under
	// the same command ID; dispatch finds it there before that prompt starts.
	if mailbox := restored.Mailbox; mailbox != nil {
		snapshot.PendingMailboxEvents = mailbox.PendingEvents
		snapshot.DeliveredMailboxEventKeys = mailbox.DeliveredEventKeys
		snapshot.MailboxPromptContexts = mailbox.PromptContexts
	}
	return snapshot, nil
}

// upgradeRelaySnapshot upgrades an older snapshot in place. The stored
// frontier digests stay valid: each is recomputed with the formula matching
// the record's format. Newly owned state is seeded before the schema marker
// advances; the next persist writes the current version.
func upgradeRelaySnapshot(snapshot *core.RelaySnapshot) {
	if snapshot.FormatVersion < 23 {
		seedLegacyOpenRequest(snapshot)
	}
	if snapshot.FormatVersion < 24 && snapshot.HarnessTurn != nil {
		started := snapshot.Continuation.AutonomousTurnStarted
		snapshot.HarnessTurn.Autonomous = started != nil && *started == snapshot.HarnessTurn.FirstOrdinal
	}
	// An older worker recorded a withdrawn continuation only as the quota
	// flag, so a retry stays blocked for a session already in that state at
	// upgrade.
	if snapshot.FormatVersion < 25 && snapshot.Continuation.QuotaSuppressed && snapshot.Continuation.WithdrawnOrdinal == nil {
		withdrawn := snapshot.LatestOrdinal
		snapshot.Continuation.WithdrawnOrdinal = &withdrawn
	}
	if snapshot.FormatVersion < 27 {
		seedLegacyNotificationStack(snapshot)
	}
	if snapshot.FormatVersion < 28 {
		// Old Moves had no explicit pins. Keep their acceptance receipts
		// until the daemon has reconciled its durable operation owner.
		snapshot.LegacyCommandRetention = true
	}
	seedMissingNotificationTime(snapshot)
	snapshot.FormatVersion = core.RelayStateVersion
	// Older snapshots can still identify a live turn, even though they did
	// not retain its start through subsequent background work.
	switch {
	case snapshot.ActivePrompt != nil:
		started := snapshot.ActivePrompt.StartedAtMs
		snapshot.ActivityTurnStartedAtMs = &started
	case snapshot.HarnessTurn != nil:
		started := snapshot.HarnessTurn.StartedAtMs
		snapshot.ActivityTurnStartedAtMs = &started
	}
}

// relayPromptText is the text a prompt command carries, its text blocks
// joined by newlines; false for a command without a prompt.
func relayPromptText(command core.RelayCommand) (string, bool) {
	prompt, ok := core.RelayCommandPromptBlocks(command)
	if !ok {
		return "", false
	}
	var texts []string
	for _, block := range prompt {
		if block.Kind == acpschema.ContentBlockKindText && block.Text != nil {
			texts = append(texts, block.Text.Text)
		}
	}
	return strings.Join(texts, "\n"), true
}

func durableRelayOpenWithHarness(root, sessionID, relayVersion string, checkpointOnly bool, harness *core.HarnessKind, initialCommands []core.InitialWorkerCommand) (*DurableRelay, error) {
	if err := validateIdentifier(sessionID, "session ID"); err != nil {
		return nil, err
	}
	if err := core.CreateOwnerOnlyDirectory(filepath.Join(root, core.RelayJournalDir)); err != nil {
		return nil, fmt.Errorf("create relay state directory %s: %w", root, err)
	}

	statePath := filepath.Join(root, core.RelayStateFile)
	_, statErr := os.Stat(statePath)
	stateExisted := statErr == nil
	var snapshot core.RelaySnapshot
	if stateExisted {
		var err error
		snapshot, err = readLegacyRelaySnapshot(statePath)
		if err != nil {
			return nil, err
		}
		if snapshot.FormatVersion > core.RelayStateVersion {
			return nil, fmt.Errorf("relay state schema %d is newer than supported schema %d", snapshot.FormatVersion, core.RelayStateVersion)
		}
		upgradeRelaySnapshot(&snapshot)
		if snapshot.SessionID != sessionID {
			return nil, fmt.Errorf("relay state belongs to session %s, not %s", snapshot.SessionID, sessionID)
		}
	} else {
		var err error
		snapshot, err = restoredRelaySnapshot(root, sessionID)
		if err != nil {
			return nil, err
		}
	}

	// Startup pins live until destroy. Upgrade from the lifetime prompt
	// watermark, which /clear never resets: startup's final brief is the
	// first accepted prompt, so that watermark proves the whole prefix.
	// Pre-watermark states use their legacy native-use proof instead.
	// Without it, retain only accepted receipt IDs and complete a crashed
	// selector prefix normally. Missing selectors are idempotent settings.
	// This also finishes migration first opened without launch arguments.
	startupOwnerCount := len(snapshot.InitialCommandIDs)
	for _, initial := range initialCommands {
		_, handled := snapshot.HandledCommands[initial.CommandID]
		if snapshot.LatestPromptAcceptedOrdinal != nil || snapshot.NativeSessionUsed || handled {
			snapshot.InitialCommandIDs = relaySortedSetInsert(snapshot.InitialCommandIDs, initial.CommandID)
		}
	}
	registeredStartupOwners := len(snapshot.InitialCommandIDs) != startupOwnerCount
	compactedTerminalCommands, err := snapshot.CompactTerminalCommands()
	if err != nil {
		return nil, err
	}
	assignedStoreID := snapshot.StoreID == nil
	if assignedStoreID {
		var random [16]byte
		if _, err := rand.Read(random[:]); err != nil {
			return nil, fmt.Errorf("generate worker store identity: %w", err)
		}
		// Rust formats u128::from_le_bytes(random) as 32 hex digits.
		slices.Reverse(random[:])
		storeID := hex.EncodeToString(random[:])
		snapshot.StoreID = &storeID
	}
	if err := core.ValidateRelaySnapshotFrontiers(&snapshot); err != nil {
		return nil, err
	}
	// Upgrade native-use evidence before discarding the old retained prefix.
	upgradingNative := snapshot.NativeSessionID != nil && snapshot.NativeUseEvidence == nil
	upgradingQuestions := false
	for _, id := range snapshot.AssessmentQuestions {
		if _, ok := snapshot.PendingElicitations[id]; !ok {
			upgradingQuestions = true
			break
		}
	}
	upgradingHistory := upgradingNative || upgradingQuestions
	retainedThrough := snapshot.RetainedThrough()
	if upgradingHistory {
		retainedThrough = min(snapshot.AcknowledgedThrough, snapshot.RecoveryFloorOrdinal)
	}
	retainedDigest := snapshot.RetainedDigest()
	if upgradingHistory && snapshot.RecoveryFloorOrdinal < snapshot.AcknowledgedThrough {
		retainedDigest = snapshot.RecoveryFloorDigest
	}
	snapshotOrdinal := snapshot.LatestOrdinal
	missingNativeHistory := upgradingNative
	journalSpans, hotEvents, err := openRelayJournal(
		filepath.Join(root, core.RelayJournalDir),
		retainedThrough,
		retainedDigest,
		snapshotOrdinal,
		&snapshot,
	)
	if err != nil {
		return nil, err
	}
	// Shipped retry records were command-bound. Read them forward once; all
	// new transitions use the turn-owned assessment state machine.
	migratedAssessment := snapshot.Assessment == nil && snapshot.RetryAssessment != nil
	if migratedAssessment {
		old := snapshot.RetryAssessment
		snapshot.RetryAssessment = nil
		var completion core.CompletionEvidence
		if old.Evidence.Completion != nil {
			completion = *old.Evidence.Completion
		} else {
			completion = core.CompletionEvidenceBounded("legacy_completed_turn", nil)
		}
		assessment := core.TurnAssessmentPending(old.CommandID, old.Ordinal, core.EpochMillis(), completion)
		evidence := old.Evidence
		assessment.Evidence = &evidence
		snapshot.Assessment = &assessment
	}
	if err := core.EnsureSerializedBudget(snapshot.OperationalState(), core.RelayStateByteBudget, "relay operational state"); err != nil {
		return nil, err
	}

	steeringDeliveryMode := SteeringDeliveryModeAcp
	if harness != nil {
		steeringDeliveryMode = steeringDeliveryModeForHarness(*harness, false)
	}
	var startupCommands []core.InitialWorkerCommand
	if !checkpointOnly {
		startupCommands = slices.Clone(initialCommands)
	}
	workerBuild := core.WorkerBuildID()
	relay := &DurableRelay{
		operatorAway:           &OperatorAway{},
		root:                   root,
		relayVersion:           relayVersion,
		workerBuild:            &workerBuild,
		harnessReadyMs:         new(atomic.Int64),
		checkpointOnly:         checkpointOnly,
		reviewerAdmissions:     map[uint64]string{},
		initialCommands:        startupCommands,
		steeringDeliveryMode:   steeringDeliveryMode,
		snapshot:               snapshot,
		journalSpans:           journalSpans,
		hotEvents:              hotEvents,
		journalCleanupPending:  true,
		acpActivity:            core.NewAcpActivityClock(),
		stepClock:              &core.StepClock{},
		foregroundTools:        &core.ToolsInFlight{},
		turnContext:            &transcript.TurnContext{},
		jevDisabled:            core.DisabledByEnvironment(),
		loginIdentity:          &ClaudeLoginIdentity{},
		airTasks:               map[string]*AirTaskEntry{},
		claudePendingStops:     map[string]map[string]struct{}{},
		kimiBackgroundTasks:    map[string]KimiTaskEntry{},
		kimiProvisionalTasks:   map[string]KimiProvisionalTask{},
		kimiObservedTaskIDs:    map[string]struct{}{},
		kimiObservedToolIDs:    map[string]struct{}{},
		activeAgentTerminals:   map[string]core.ActiveAgentTerminal{},
		closedAgentTerminals:   map[string]struct{}{},
		agentTerminalToolCalls: map[string]string{},
		knownBackgroundWork:    map[string]struct{}{},
	}
	recoveredNativeHistory := missingNativeHistory && relay.recoverNativeHistoryEvidence()
	recoveredQuestions := upgradingQuestions && relay.recoverPendingElicitations()
	// Startup already reads the active journal into the bounded hot window.
	// Reuse it: old sealed history must not make worker startup slow or fail.
	if !checkpointOnly {
		relay.restoreTurnContext()
	}
	relay.turnContext.SetAuthorizationContext(relay.snapshot.AuthorizationContext())
	// Live-only work cannot be reconstructed on reopen. Nor can replay prove
	// an idle transition that was not yet saved with the snapshot.
	replayed := relay.snapshot.LatestOrdinal > snapshotOrdinal
	idle := relay.activityIsIdle()
	if relay.snapshot.ActivityWasIdle == nil || *relay.snapshot.ActivityWasIdle != idle {
		relay.snapshot.IdleSinceMs = nil
		relay.snapshot.ActivityWasIdle = &idle
	}
	if idle {
		relay.snapshot.ActivityTurnStartedAtMs = nil
	}
	_, statErr = os.Stat(statePath)
	if statErr != nil || replayed || assignedStoreID || recoveredNativeHistory || recoveredQuestions ||
		migratedAssessment || compactedTerminalCommands || registeredStartupOwners {
		if err := relay.persistSnapshot(); err != nil {
			if recoveredQuestions {
				ids := slices.Sorted(maps.Keys(relay.snapshot.PendingElicitations))
				quoted := make([]string, len(ids))
				for i, id := range ids {
					quoted[i] = core.RustDebugString(id)
				}
				return nil, fmt.Errorf("save upgraded worker pending questions [%s]: %w", strings.Join(quoted, ", "), err)
			}
			return nil, fmt.Errorf("save recovered worker snapshot: %w", err)
		}
	}
	// Complete an ACK's interrupted file collection before any startup append
	// can try sealing an acknowledged active file with no live span. Evidence
	// upgrades above are durable before their old files disappear.
	if err := relay.garbageCollectRelayHistory(); err != nil {
		return nil, err
	}
	if !checkpointOnly {
		if err := relay.returnMailboxHookLease(core.MailboxHookLeaseReturnReasonWorkerRestart); err != nil {
			return nil, err
		}
	}
	if err := relay.adoptUnqueuedQueueCommands(); err != nil {
		return nil, err
	}
	if err := relay.ensureInitialCommandsSucceeded(); err != nil {
		return nil, err
	}
	if err := relay.seedInitialCommands(); err != nil {
		return nil, err
	}
	if err := relay.recoverNonterminalCommands(); err != nil {
		return nil, err
	}
	if err := relay.ensureInitialCommandsSucceeded(); err != nil {
		return nil, err
	}
	if _, _, err := relay.promoteNextQueuedCommand(); err != nil {
		return nil, err
	}
	if !checkpointOnly {
		// No locator task survives a worker process. Record the final
		// no-result observation for any request interrupted by the restart.
		var pending []uint64
		for _, entry := range relay.snapshot.NotificationStack.Entries {
			if _, locating := entry.Message.(core.EntryMessageLocating); locating {
				pending = append(pending, entry.CompletedOrdinal)
			}
		}
		for _, completed := range pending {
			if _, err := relay.finishQuestionLocator(completed, nil); err != nil {
				return nil, err
			}
		}
	}
	return relay, nil
}

// restoreTurnContext rebuilds the process-local turn context from the hot
// window of the journal startup already read.
func (r *DurableRelay) restoreTurnContext() {
	// The queue observation may be collected before delivery. The durable
	// dispatch still owns the active prompt and its boundary. A nil value is
	// a prompt whose body is unavailable, distinct from an empty prompt.
	prompts := map[string]*string{}
	for id, dispatch := range r.snapshot.Dispatches {
		if text, ok := relayPromptText(dispatch.Command); ok {
			prompts[id] = &text
		}
	}
	ensurePrompt := func(commandID string) {
		if _, ok := prompts[commandID]; !ok {
			prompts[commandID] = nil
		}
	}
	// The relay owner persists autonomous identity in the open turn and its
	// settled record; native starts inside prompts are not assistant-message
	// boundaries.
	autonomousStarts := map[uint64]struct{}{}
	if turn := r.snapshot.AutonomousHarnessTurn(); turn != nil {
		autonomousStarts[turn.FirstOrdinal] = struct{}{}
	}
	// Terminal records retain command kinds after dispatch bodies are gone.
	// Their starts still establish boundaries; an unavailable body stays
	// distinct from an empty prompt.
	for _, event := range r.hotEvents {
		switch observation := event.Observation.(type) {
		case core.RelayObservationHarnessTurnSettled:
			if observation.Autonomous != nil {
				autonomousStarts[observation.Autonomous.StartOrdinal] = struct{}{}
			}
		case core.RelayObservationCommandCompleted:
			_, promptOutcome := observation.Outcome.(core.RelayCommandOutcomePrompt)
			if (observation.Command != nil && *observation.Command == core.RelayCommandKindPrompt) || promptOutcome {
				ensurePrompt(observation.CommandID)
			}
		case core.RelayObservationCommandRejected:
			if observation.Command == core.RelayCommandKindPrompt {
				ensurePrompt(observation.CommandID)
			}
		case core.RelayObservationCommandInterrupted:
			if observation.Command == core.RelayCommandKindPrompt {
				ensurePrompt(observation.CommandID)
			}
			if observation.ContinuationID != nil {
				ensurePrompt(*observation.ContinuationID)
			}
		}
	}
	earlierHistoryOmitted := (len(r.hotEvents) == 0 || r.hotEvents[0].Ordinal > 1) && r.snapshot.LatestOrdinal > 0
	if earlierHistoryOmitted {
		r.turnContext.MarkEarlierHistoryOmitted()
	}
	for _, event := range r.hotEvents {
		if _, ok := autonomousStarts[event.Ordinal]; ok {
			r.turnContext.ObserveAutonomousTurnStart()
		}
		if queued, ok := event.Observation.(core.RelayObservationCommandQueued); ok {
			if text, ok := relayPromptText(queued.Command); ok {
				prompts[queued.CommandID] = &text
			}
		}
		var prompt *string
		delivered := false
		if commandID, ok := transcript.DeliveredPromptCommandID(event.Observation); ok {
			prompt, delivered = prompts[commandID]
			delete(prompts, commandID)
		} else if completed, ok := event.Observation.(core.RelayObservationCommandCompleted); ok {
			delete(prompts, completed.CommandID)
		}
		if delivered {
			commandID, _ := transcript.DeliveredPromptCommandID(event.Observation)
			r.turnContext.ObservePromptDelivery(&commandID, prompt)
		} else {
			r.turnContext.ObserveRelay(event.Observation, nil)
		}
	}
	// The latest persisted human instructions govern every turn, including
	// mailbox wakes and restart continuations. Use what remains in that
	// history, without matching an executing request ID.
	if history := r.snapshot.AuthorizationContext(); history != nil {
		r.turnContext.RestoreUserPromptTail(history)
	}
}

// seedInitialCommands completes the creation prefix before publishing any
// control endpoint. The final prompt's receipt proves the entire prefix was
// journaled. Once a turn has run, receipt compaction must never cause
// initialization again.
func (r *DurableRelay) seedInitialCommands() error {
	if len(r.initialCommands) == 0 {
		return nil
	}
	last := r.initialCommands[len(r.initialCommands)-1]
	if relaySortedSetContains(r.snapshot.InitialCommandIDs, last.CommandID) {
		return nil
	}
	if _, prompt := last.Command.(core.RelayCommandPrompt); !prompt || last.Optional {
		return errors.New("initial input must end with the original prompt")
	}
	commands := slices.Clone(r.initialCommands)
	for position, initial := range commands {
		if _, setConfig := initial.Command.(core.RelayCommandSetConfig); position+1 != len(commands) && !setConfig {
			return errors.New("initial input before the brief must be configuration")
		}
		if relaySortedSetContains(r.snapshot.InitialCommandIDs, initial.CommandID) {
			continue
		}
		_, refusal, err := r.submitCommand(initial.CommandID, initial.Command)
		if err != nil {
			return err
		}
		if refusal != nil {
			return fmt.Errorf("initialize worker command %s: %s", initial.CommandID, refusal.Message)
		}
		r.snapshot.InitialCommandIDs = relaySortedSetInsert(r.snapshot.InitialCommandIDs, initial.CommandID)
	}
	// No endpoint is published during seeding. A crash before this write
	// recovers the accepted IDs from their ordinary journal receipts.
	return r.persistSnapshot()
}

// ensureInitialCommandsSucceeded: a failed required creation command cannot
// release the original brief or any follow-up. Its journaled failure also
// stops a restarted worker.
func (r *DurableRelay) ensureInitialCommandsSucceeded() error {
	for _, initial := range r.initialCommands {
		if _, setConfig := initial.Command.(core.RelayCommandSetConfig); initial.Optional || !setConfig {
			continue
		}
		receipt, ok := r.snapshot.HandledCommands[initial.CommandID]
		if ok && receipt.Failure != nil {
			return fmt.Errorf("initial worker command %s (%s) failed: %s",
				initial.CommandID, relayCommandKindDebug(core.RelayCommandKindOf(initial.Command)), *receipt.Failure)
		}
	}
	return nil
}

// relayCommandKindDebug renders a command kind as Rust's {:?} does: the
// variant name.
func relayCommandKindDebug(kind core.RelayCommandKind) string {
	var name strings.Builder
	for _, word := range strings.Split(string(kind), "_") {
		if word != "" {
			name.WriteString(strings.ToUpper(word[:1]) + word[1:])
		}
	}
	return name.String()
}

// adoptUnqueuedQueueCommands adopts queueable commands that an earlier relay
// format accepted outside the durable queue. Configuration changes used to
// dispatch through the control path, so a command accepted just before an
// upgrade would otherwise stay accepted forever without ever being promoted.
func (r *DurableRelay) adoptUnqueuedQueueCommands() error {
	type acceptedQueued struct {
		accepted uint64
		queued   core.StoredQueuedRelayCommand
	}
	var adopted []acceptedQueued
	for _, commandID := range slices.Sorted(maps.Keys(r.snapshot.Dispatches)) {
		dispatch := r.snapshot.Dispatches[commandID]
		if dispatch.State != core.RelayDispatchStateQueued ||
			!core.RelayCommandIsQueueEntry(dispatch.Command) ||
			slices.ContainsFunc(r.snapshot.QueuedPrompts, func(queued core.StoredQueuedRelayCommand) bool {
				return queued.CommandID == commandID
			}) {
			continue
		}
		handled, ok := r.snapshot.HandledCommands[commandID]
		if !ok {
			continue
		}
		var payload core.StoredQueuedRelayPayload
		switch command := dispatch.Command.(type) {
		case core.RelayCommandPrompt, core.RelayCommandHandbackReminder, core.RelayCommandHoldFollowUp,
			core.RelayCommandMailboxWake, core.RelayCommandContinueAuthorizedWork, core.RelayCommandResumeAfterQuota:
			prompt, _ := core.RelayCommandPromptBlocks(command)
			payload = core.StoredQueuedRelayPayloadPrompt{Prompt: slices.Clone(prompt)}
		case core.RelayCommandSetConfig:
			payload = core.StoredQueuedRelayPayloadSetConfig{Key: command.Key, Value: command.Value}
		default:
			continue
		}
		adopted = append(adopted, acceptedQueued{
			accepted: handled.AcceptedOrdinal,
			queued: core.StoredQueuedRelayCommand{
				CommandID:   commandID,
				Payload:     payload,
				CreatedAtMs: core.EpochMillis(),
			},
		})
	}
	if len(adopted) == 0 {
		return nil
	}
	ordered := make([]acceptedQueued, 0, len(r.snapshot.QueuedPrompts)+len(adopted))
	for _, queued := range r.snapshot.QueuedPrompts {
		var accepted uint64
		if handled, ok := r.snapshot.HandledCommands[queued.CommandID]; ok {
			accepted = handled.AcceptedOrdinal
		}
		ordered = append(ordered, acceptedQueued{accepted: accepted, queued: queued})
	}
	ordered = append(ordered, adopted...)
	slices.SortStableFunc(ordered, func(a, b acceptedQueued) int {
		return cmp.Compare(a.accepted, b.accepted)
	})
	queue := make([]core.StoredQueuedRelayCommand, 0, len(ordered))
	for _, entry := range ordered {
		queue = append(queue, entry.queued)
	}
	r.snapshot.QueuedPrompts = queue
	return r.persistSnapshot()
}

// originatingCommandID is called while the worker owner is held through
// durable tool admission.
func (r *DurableRelay) originatingCommandID() (string, bool) {
	if prompt := r.snapshot.ActivePrompt; prompt != nil {
		return prompt.CommandID, true
	}
	if turn := r.snapshot.AutonomousHarnessTurn(); turn != nil {
		return core.HarnessTurnID(turn.FirstOrdinal), true
	}
	return "", false
}

func (r *DurableRelay) OperationalState() core.RelayOperationalState {
	state := r.snapshot.OperationalState()
	if kind, limited := core.CurrentRateLimit(); limited {
		state.JevRateLimit = &kind
	}
	ready := r.isAcpReady()
	state.ACPReady = &ready
	state.CheckpointOnly = r.checkpointOnly
	state.SteeringSupported = r.EffectiveSteeringSupported()
	if r.mailboxDeliveryMode != nil {
		mode := *r.mailboxDeliveryMode
		state.MailboxDelivery = &mode
	}
	// Harness-initiated turns have no owned prompt ID. Keep their existing
	// explicit interrupt surface; queue steering always needs an owned turn.
	if r.snapshot.ActivePrompt != nil || r.snapshot.HarnessTurn == nil {
		controls := r.turnControls()
		state.TurnControls = &controls
	}
	if at, ok := r.acpActivity.LastAtMs(); ok {
		state.LastACPActivityAtMs = &at
	}
	state.CurrentStepStartedAtMs = r.stepClock.StartedAtMs()
	state.ToolsInFlight = r.foregroundTools.Snapshot()
	state.ForegroundToolStartedAtMs = r.foregroundTools.NewestStartedAtMs()
	terminals := make([]core.ActiveAgentTerminal, 0, len(r.activeAgentTerminals))
	for _, id := range slices.Sorted(maps.Keys(r.activeAgentTerminals)) {
		terminals = append(terminals, r.activeAgentTerminals[id])
	}
	state.ActiveAgentTerminals = terminals
	state.NativeAgentCount = r.nativeAgentCount()
	state.ClearContext = r.supportsClearContext()
	state.BackgroundCommands = r.backgroundCommands()
	if r.backgroundWorkKnown != nil {
		known := *r.backgroundWorkKnown
		state.BackgroundWorkKnown = &known
	}
	// Answer the activity question once, here, where every fact is in hand.
	// Consumers read this instead of each deriving their own.
	facts := r.ActivityFacts()
	state.ExpectedContinuation = facts.ExpectedContinuation
	state.InferredIdleSinceMs = facts.InferredIdleSinceMs
	state.TaskSettledAtMs = facts.TaskSettledAtMs
	state.BackgroundNeeded = facts.BackgroundNeeded
	state.Activity = core.Classify(&facts)
	// The worker alone decides whether it still owes a retry of a failed
	// turn (the durable intent, apart from its submit gate); the daemon only
	// reads the answer. authentication_retry_waiting is retired.
	if r.verdictHarness != nil && *r.verdictHarness == core.HarnessKindClaude {
		_, state.HeldLoginFingerprint = r.loginIdentity.snapshot()
	} else {
		state.HeldLoginFingerprint = r.loginIdentity.fileFingerprint()
	}
	if _, rejected, ok := r.authenticationRetryIntent(); ok {
		state.AuthenticationRetryIntent = &rejected
	}
	return state
}

// isAcpReady is whether the ACP process is configured: derived from the
// readiness instant, never stored beside it.
func (r *DurableRelay) isAcpReady() bool {
	return r.harnessReadyMs.Load() != 0
}

func (r *DurableRelay) setAcpReady(ready bool) {
	// A second configuration of an open session is not a new harness: the
	// instant stays that of the first, or work started in between would read
	// as part of the harness.
	if ready && !r.isAcpReady() {
		r.harnessReadyMs.Store(max(core.EpochMillis(), 1))
	} else if !ready {
		r.harnessReadyMs.Store(0)
	}
}

// HarnessReadyClock is the instant the harness session was last configured,
// shared with connections that observe work.
func (r *DurableRelay) HarnessReadyClock() *atomic.Int64 {
	return r.harnessReadyMs
}

// ClearAcpReadiness: a stopped bridge cannot become ready again until its
// replacement configures.
func (r *DurableRelay) ClearAcpReadiness() {
	r.setAcpReady(false)
	r.steeringSupported = nil
	r.automaticSteering = false
}

// SetSteeringSupported publishes the current harness's steering support
// without changing the journal.
func (r *DurableRelay) SetSteeringSupported(supported *bool) {
	r.steeringSupported = supported
}

// EffectiveSteeringSupported is the steering capability combining ACP
// support with hook-based delivery.
func (r *DurableRelay) EffectiveSteeringSupported() *bool {
	if r.steeringSupported == nil {
		return nil
	}
	var supported bool
	switch r.steeringDeliveryMode {
	case SteeringDeliveryModeHookPostToolBatch:
		supported = true
	case SteeringDeliveryModeHookTurnEnd:
		supported = false
	default:
		supported = *r.steeringSupported
	}
	return &supported
}

func (r *DurableRelay) supportsClearContext() bool {
	if r.verdictHarness == nil {
		return false
	}
	switch *r.verdictHarness {
	case core.HarnessKindCodex, core.HarnessKindClaude, core.HarnessKindKimi, core.HarnessKindAntigravity:
		return true
	}
	return false
}

// SetAutomaticSteering allows the relay to steer queued prompts into the
// running turn itself.
func (r *DurableRelay) SetAutomaticSteering(enabled bool) {
	r.automaticSteering = enabled
}

// SetSteeringDeliveryMode selects steering delivery for the configured
// harness and staged profile. The safe turn-end mode is selected before
// worker admission opens; preparation upgrades it only after verifying a
// usable hook marker.
func (r *DurableRelay) SetSteeringDeliveryMode(harness core.HarnessKind, boundaryHook bool) {
	r.steeringDeliveryMode = steeringDeliveryModeForHarness(harness, boundaryHook)
}

// SetMailboxDeliveryMode sets when a Codex session's mailbox messages may
// reach it, from the provider in its staged configuration. The PostToolUse
// drain and the published operational state both read this one field.
func (r *DurableRelay) SetMailboxDeliveryMode(mode core.MailboxDeliveryMode) {
	r.mailboxDeliveryMode = &mode
}

func (r *DurableRelay) updateMailboxDeliveryMode(mode *core.MailboxDeliveryMode) {
	r.mailboxDeliveryMode = mode
}

func (r *DurableRelay) inPlaceMoveHoldActive() bool {
	return r.snapshot.InPlaceMoveHold != nil
}

// turnInProgress is the relay-owned turn boundary shared by command
// promotion, checkpoint admission, and background-work accounting. Activity
// classification also presents an in-progress /clear as a turn, but that
// command must be allowed to promote itself when the session is otherwise
// idle.
func (r *DurableRelay) turnInProgress() bool {
	return r.snapshot.ActivePrompt != nil || r.snapshot.HarnessTurn != nil
}

// ActivityFacts is every fact that bears on whether this session is working.
//
// The idle clock runs on every journal append, so this assembles the facts
// directly rather than through a whole operational state, which would clone
// the session's configuration once per streamed chunk. It must stay
// identical to RelayOperationalState.Facts, which is what the daemon reads;
// worker_facts_match_the_published_state pins them together.
func (r *DurableRelay) ActivityFacts() core.ActivityFacts {
	backgroundCommands := r.backgroundCommands()
	backgroundCount := len(backgroundCommands) + r.nativeAgentCount()
	r.turnContext.SetCounts(backgroundCount, len(r.snapshot.QueuedPrompts))
	var promptStartedAtMs *int64
	if prompt := r.snapshot.ActivePrompt; prompt != nil {
		started := prompt.StartedAtMs
		promptStartedAtMs = &started
	} else {
		promptStartedAtMs = r.snapshot.ClearContextStartedAtMs()
	}
	var harnessTurnStartedAtMs *int64
	if turn := r.snapshot.HarnessTurn; turn != nil {
		started := turn.StartedAtMs
		harnessTurnStartedAtMs = &started
	}
	var backgroundStartedAtMs *int64
	noteStart := func(started int64) {
		if backgroundStartedAtMs == nil || started < *backgroundStartedAtMs {
			backgroundStartedAtMs = &started
		}
	}
	for _, command := range backgroundCommands {
		noteStart(command.StartedAtMs)
	}
	for _, shell := range r.snapshot.ActiveUserShells {
		if shell.StartedAtMs != nil {
			noteStart(*shell.StartedAtMs)
		}
	}
	var backgroundWorkKnown *bool
	if r.backgroundWorkKnown != nil {
		known := *r.backgroundWorkKnown
		backgroundWorkKnown = &known
	}
	acpReady := r.isAcpReady()
	var lastACPActivityAtMs *int64
	if at, ok := r.acpActivity.LastAtMs(); ok {
		lastACPActivityAtMs = &at
	}
	// A settle only counts until the turn it promised has started: no turn
	// open now, and none started since the settle.
	var taskSettledAtMs *int64
	if settled := r.taskSettledAtMs; settled != nil &&
		r.snapshot.ActivePrompt == nil &&
		r.snapshot.HarnessTurn == nil &&
		(r.snapshot.ActivityTurnStartedAtMs == nil || *settled >= *r.snapshot.ActivityTurnStartedAtMs) {
		at := *settled
		taskSettledAtMs = &at
	}
	facts := core.ActivityFacts{
		Execution:                  r.snapshot.Execution,
		PromptStartedAtMs:          promptStartedAtMs,
		HarnessTurnStartedAtMs:     harnessTurnStartedAtMs,
		TurnStartedAtMs:            r.snapshot.ActivityTurnStartedAtMs,
		QueuedCommands:             len(r.snapshot.QueuedPrompts),
		ToolsInFlight:              r.foregroundTools.Snapshot(),
		BackgroundStartedAtMs:      backgroundStartedAtMs,
		BackgroundCommands:         backgroundCount,
		AntigravityTaskStartedAtMs: core.BackgroundCommandOldestAntigravityTaskStart(backgroundCommands),
		ActiveUserShells:           len(r.snapshot.ActiveUserShells),
		ActiveAgentTerminals:       len(r.activeAgentTerminals),
		GoalActive:                 r.snapshot.Goal.Active(),
		GoalRunning:                r.snapshot.Goal.Running(),
		GoalPendingResume:          r.snapshot.Goal.PendingResume != nil,
		GoalDecision:               r.snapshot.Goal.Decision != nil,
		GoalSynchronized:           r.snapshot.Goal.Synchronized(),
		BackgroundWorkKnown:        backgroundWorkKnown,
		AcpReady:                   &acpReady,
		CheckpointOnly:             r.checkpointOnly,
		CheckpointBarrier:          r.snapshot.CheckpointBarrier != nil,
		CapacityRetryArmed:         r.snapshot.CapacityRetry != nil && !r.snapshot.CapacityRetry.Submitted,
		LastAcpActivityAtMs:        lastACPActivityAtMs,
		CurrentStepStartedAtMs:     r.stepClock.StartedAtMs(),
		IdleSinceMs:                r.snapshot.IdleSinceMs,
		TaskSettledAtMs:            taskSettledAtMs,
	}
	runningAgents := []string{}
	for _, id := range slices.Sorted(maps.Keys(r.snapshot.NativeAgents)) {
		if r.snapshot.NativeAgents[id].State == core.NativeAgentStateRunning {
			runningAgents = append(runningAgents, id)
		}
	}
	r.turnContext.SetBackgroundInventory(backgroundCommands, runningAgents)
	facts.ExpectedContinuation, facts.InferredIdleSinceMs, facts.BackgroundNeeded =
		r.repliedVerdict.inference(r.turnContext.Generation(), &facts)
	return facts
}

// activityIsIdle is whether the session's idle clock should be running: the
// same classification every other part of Mjolnir uses.
func (r *DurableRelay) activityIsIdle() bool {
	facts := r.ActivityFacts()
	state := core.Classify(&facts)
	return state.IsIdle()
}

// refreshIdleClock updates only at activity mutations, never when a viewer
// reads status.
func (r *DurableRelay) refreshIdleClock(nowMs int64) bool {
	idle := r.activityIsIdle()
	previous := r.snapshot.ActivityWasIdle
	if previous != nil && *previous == idle {
		return false
	}
	r.snapshot.IdleSinceMs = nil
	if idle && previous != nil && !*previous {
		r.snapshot.IdleSinceMs = &nowMs
	}
	if idle {
		r.snapshot.ActivityTurnStartedAtMs = nil
	}
	r.snapshot.ActivityWasIdle = &idle
	return true
}

func (r *DurableRelay) persistActivityTransition() error {
	// The one place background work gains members: every change to it ends
	// here, journaled or not.
	if err := r.noteBackgroundWork(); err != nil {
		return err
	}
	if r.refreshIdleClock(core.EpochMillis()) {
		if err := r.persistSnapshot(); err != nil {
			return fmt.Errorf("persist session idle transition: %w", err)
		}
	}
	r.serviceLiveBaselines()
	return nil
}

// SetWorkerBuild sets the worker-input compatibility identity served in
// hello.
func (r *DurableRelay) SetWorkerBuild(build *string) {
	r.workerBuild = build
}

// SetHarnessTurnPolicy chooses whether agent output with no prompt in flight
// opens a turn.
func (r *DurableRelay) SetHarnessTurnPolicy(policy HarnessTurnPolicy) {
	r.harnessTurns = policy
}

func (r *DurableRelay) usesClaudeHarnessTurns() bool {
	return r.harnessTurns == HarnessTurnPolicyClaudeAdapter
}

// setManagedSubagent: launch metadata owns this fact, including older
// environment-only launches.
func (r *DurableRelay) setManagedSubagent(managed bool) error {
	if r.snapshot.ManagedSubagent != managed {
		if _, err := r.RecordObservation(core.RelayObservationManagedSubagentConfigured{Managed: managed}); err != nil {
			return err
		}
	}
	if err := r.settleLocalQuota(); err != nil {
		return err
	}
	return r.suppressChildAssessment()
}

// watchedWork is the latest finished turn's work processes while some may
// still be alive, with the identities the worker's watcher recounts.
func (r *DurableRelay) watchedWork() (core.WorkLiveness, []core.ProcessIdentity, bool) {
	liveness := r.snapshot.WorkLiveness
	if liveness == nil || liveness.Alive == 0 {
		return core.WorkLiveness{}, nil, false
	}
	completion := r.snapshot.TurnCompletion
	if completion == nil || completion.CompletedOrdinal != liveness.CompletedOrdinal {
		return core.WorkLiveness{}, nil, false
	}
	// A prompted turn's facts are in its handled command; an autonomous
	// harness turn's are on its settlement.
	var facts *core.WorkFacts
	handled, ok := r.snapshot.HandledCommands[completion.CommandID]
	if prompt, isPrompt := handled.Outcome.(core.RelayCommandOutcomePrompt); ok && isPrompt {
		if prompt.WorkFacts == nil {
			return core.WorkLiveness{}, nil, false
		}
		facts = prompt.WorkFacts
	} else {
		work := r.snapshot.HarnessTurnWork
		if work == nil || work.CompletedOrdinal != liveness.CompletedOrdinal {
			return core.WorkLiveness{}, nil, false
		}
		facts = &work.Facts
	}
	processes := facts.Processes.Known()
	if processes == nil {
		return core.WorkLiveness{}, nil, false
	}
	return *liveness, slices.Clone(processes.Identities), true
}

// recordWorkLiveness publishes a recount, unless the turn it counts is no
// longer the current one or the count has not changed. Returns whether it
// was recorded.
func (r *DurableRelay) recordWorkLiveness(liveness core.WorkLiveness) (bool, error) {
	current := r.snapshot.WorkLiveness
	changed := current != nil && current.CompletedOrdinal == liveness.CompletedOrdinal && current.Alive != liveness.Alive
	if changed {
		if _, err := r.RecordObservation(core.RelayObservationWorkLivenessChanged{Liveness: liveness}); err != nil {
			return false, err
		}
	}
	return changed, nil
}

func (r *DurableRelay) SetTurnVerdictHarness(harness core.HarnessKind) {
	r.verdictHarness = &harness
	if _, ok := r.turnContext.DecisionLog(); !ok {
		log, err := core.DecisionLogOpen(filepath.Join(r.root, "jev-decisions"))
		if err != nil {
			slog.Warn("Jev diagnostic log unavailable", "error", err)
		} else {
			r.decisionLog = &log
			r.turnContext.SetDecisionLog(log)
		}
	}
	r.turnContext.SetSessionID(r.snapshot.SessionID)
	r.publishAssessmentDiagnostic()
}

// Shutdown ends the relay's ownership of the Jev decision log it opened:
// every record queued so far is written before it returns, and tasks still
// holding the log can write no more, so nothing lands in the root after its
// owner is done with it. Rust drops the log with the relay. Calling it
// again has no further effect.
func (r *DurableRelay) Shutdown() {
	if r.decisionLog != nil {
		r.decisionLog.Close()
	}
}

func (r *DurableRelay) TurnContext() *transcript.TurnContext {
	return r.turnContext
}

func (r *DurableRelay) AcpActivityClock() core.AcpActivityClock {
	return r.acpActivity
}

func (r *DurableRelay) StepClock() *core.StepClock {
	return r.stepClock
}

// ToolsInFlight is the tool calls the agent has open. Shared, not copied:
// the ACP driver's stall watchdog reads exactly what this relay records.
func (r *DurableRelay) ToolsInFlight() *core.ToolsInFlight {
	return r.foregroundTools
}

// Root is the directory holding this relay's durable state.
func (r *DurableRelay) Root() string {
	return r.root
}

// NewestAcceptedWorkOrdinal is the journal ordinal of the newest work this
// worker accepted: a prompt, or a turn the harness started on its own. Zero
// before any. The worker's own answer to whether work arrived after a
// finish.
func (r *DurableRelay) NewestAcceptedWorkOrdinal() uint64 {
	var newest uint64
	if ordinal := r.snapshot.LatestPromptAcceptedOrdinal; ordinal != nil {
		newest = *ordinal
	}
	if ordinal := r.snapshot.LastHarnessTurnStartedOrdinal; ordinal != nil {
		newest = max(newest, *ordinal)
	}
	return newest
}

func (r *DurableRelay) LatestOrdinal() uint64 {
	return r.snapshot.LatestOrdinal
}

func (r *DurableRelay) LatestDigest() string {
	return r.snapshot.LatestDigest
}

func (r *DurableRelay) AcknowledgedThrough() uint64 {
	return r.snapshot.AcknowledgedThrough
}

func (r *DurableRelay) AcknowledgedDigest() string {
	return r.snapshot.AcknowledgedDigest
}

func (r *DurableRelay) EventsAfter(afterOrdinal uint64, afterDigest string) ([]core.RelayEvent, error) {
	plan := r.replayPlan()
	if err := plan.validateCursor(afterOrdinal, afterDigest); err != nil {
		return nil, err
	}
	page, err := plan.readEventsAfter(afterOrdinal, afterDigest, math.MaxInt)
	if err != nil {
		return nil, err
	}
	return page.events, nil
}

func (r *DurableRelay) NativeSessionMayHaveHistory() bool {
	return r.NativeSessionHistory() == NativeSessionHistoryMayHaveHistory
}

// NativeSessionHistory classifies native history using the durable evidence,
// including after ACK pruning.
func (r *DurableRelay) NativeSessionHistory() NativeSessionHistory {
	dispatched := false
	for _, dispatch := range r.snapshot.Dispatches {
		if dispatch.PromptMayHaveReachedAgent() {
			dispatched = true
			break
		}
	}
	if evidence := r.snapshot.NativeUseEvidence; evidence != nil {
		// Before the restored worker opens its first harness session,
		// native_session_id is still nil while the seed's archived evidence
		// is already present. Honor RestoredNativeSessionUnused so an unused
		// restored session can still take LoadBeforeReplace.
		if r.snapshot.NativeSessionID == nil {
			if evidence.Version == 1 && evidence.Usage == core.NativeSessionUseUnused &&
				r.snapshot.RestoredNativeSessionUnused && !dispatched {
				return NativeSessionHistoryLoadBeforeReplace
			}
			return NativeSessionHistoryMayHaveHistory
		}
		if evidence.Version != 1 ||
			r.snapshot.NativeSessionID == nil || evidence.NativeSessionID != *r.snapshot.NativeSessionID ||
			evidence.Usage != core.NativeSessionUseUnused ||
			dispatched {
			return NativeSessionHistoryMayHaveHistory
		}
		switch {
		case evidence.OpenedOrdinal != nil:
			return NativeSessionHistoryCreatedHereUnused
		case r.snapshot.RestoredNativeSessionUnused:
			return NativeSessionHistoryLoadBeforeReplace
		default:
			return NativeSessionHistoryMayHaveHistory
		}
	}
	if r.snapshot.NativeSessionUsed || dispatched {
		return NativeSessionHistoryMayHaveHistory
	}
	opened := r.snapshot.NativeSessionOpenedOrdinal
	switch {
	// History below the recovery floor was archived. A thread opened above
	// it could never have contributed to that history.
	case opened != nil && *opened > r.snapshot.RecoveryFloorOrdinal:
		return NativeSessionHistoryCreatedHereUnused
	// The checkpoint hint only reflects visible user prompts. The harness
	// can still have history from a mailbox wake.
	case opened == nil && r.snapshot.RestoredNativeSessionUnused:
		return NativeSessionHistoryLoadBeforeReplace
	default:
		return NativeSessionHistoryMayHaveHistory
	}
}

// RestoredNativeSessionUnused is whether the checkpoint this relay was
// restored from shows that the native session it continues never received a
// prompt, while no session has opened here yet.
func (r *DurableRelay) RestoredNativeSessionUnused() bool {
	return r.snapshot.RestoredNativeSessionUnused && r.snapshot.NativeSessionOpenedOrdinal == nil
}

// MarkNativeSessionUsed records that the native thread has been used and can
// never be replaced. Persisted immediately: the evidence is live ACP
// traffic, and a worker that dies right after it must not come back
// believing the thread empty.
func (r *DurableRelay) MarkNativeSessionUsed() error {
	if evidence := r.snapshot.NativeUseEvidence; evidence != nil &&
		evidence.Version == 1 &&
		r.snapshot.NativeSessionID != nil && evidence.NativeSessionID == *r.snapshot.NativeSessionID &&
		evidence.Usage == core.NativeSessionUseUsed {
		return nil
	}
	r.snapshot.NativeSessionUsed = true
	r.snapshot.RecordNativeUse(core.NativeSessionUseUsed)
	return r.persistSnapshot()
}

// JournalGeneration is how many times the journal's files stopped matching a
// captured RelayReplayPlan. A lock-free reader compares this before and
// after its read to tell a stale plan from a real desynchronization.
func (r *DurableRelay) JournalGeneration() uint64 {
	return r.journalGeneration
}

// RecordObservation journals an observation. While a checkpoint barrier
// holds its cut, one that can wait is held until the barrier ends and the
// current frontier is returned (see relay_checkpoint_hold.go).
func (r *DurableRelay) RecordObservation(observation core.RelayObservation) (uint64, error) {
	observation, err := r.holdObservation(observation)
	if err != nil {
		return 0, err
	}
	if observation == nil {
		return r.snapshot.LatestOrdinal, nil
	}
	if native, ok := observation.(core.RelayObservationNativeAgent); ok {
		if update, ok := native.Event.(core.NativeAgentEventUpdate); ok {
			sessionID := update.SessionID
			r.consumeClaudeStopAcknowledgement(&sessionID, &update.Update)
		}
	}
	var acpReady, sessionSetup *bool
	ready, notReady := true, false
	switch observation.(type) {
	case core.RelayObservationSessionConfigured:
		acpReady, sessionSetup = &ready, &notReady
	case core.RelayObservationAgentInitialized:
		acpReady, sessionSetup = &notReady, &ready
	case core.RelayObservationSessionRestarted, core.RelayObservationClosing, core.RelayObservationClosed:
		acpReady, sessionSetup = &notReady, &notReady
		// A restart or a close ends the harness process that owned whatever
		// it had left running, so nothing it reported is still alive.
		r.forgetHarnessProcesses()
	case core.RelayObservationSessionOpened:
		sessionSetup = &notReady
	}
	ordinal, err := r.appendRelayEvent(nil, observation)
	if err != nil {
		return 0, err
	}
	if acpReady != nil {
		r.setAcpReady(*acpReady)
	}
	if sessionSetup != nil {
		r.sessionSetup = *sessionSetup
	}
	return ordinal, nil
}

func (r *DurableRelay) RecordSessionUpdate(update acpschema.SessionUpdate) (uint64, error) {
	// An ACP file edit arrives as the whole file before and the whole file
	// after. Store the patch between them instead: nothing downstream
	// reconstructs a file from those copies, and the patch is proportional
	// to the edit rather than to the file. See core diff.go.
	switch update.Kind {
	case acpschema.SessionUpdateKindToolCall:
		if update.ToolCall != nil {
			call := *update.ToolCall
			call.Content = slices.Clone(call.Content)
			core.CompactToolCallContent(call.Content)
			update.ToolCall = &call
		}
	case acpschema.SessionUpdateKindToolCallUpdate:
		if update.ToolCallUpdate != nil && update.ToolCallUpdate.Content != nil {
			call := *update.ToolCallUpdate
			call.Content = slices.Clone(call.Content)
			core.CompactToolCallContent(call.Content)
			update.ToolCallUpdate = &call
		}
	}
	// A notification that changes no agent work waits for a ready checkpoint
	// barrier to end; see relay_checkpoint_hold.go.
	held, err := r.holdSessionUpdate(update)
	if err != nil {
		return 0, err
	}
	if held == nil {
		return r.snapshot.LatestOrdinal, nil
	}
	update = *held
	nativeBefore := r.snapshot.Goal.Running()
	nativeAfter := r.snapshot.Goal
	if _, err := nativeAfter.Apply(&update); err != nil {
		return 0, err
	}
	resumeRunning := false
	if update.Kind == acpschema.SessionUpdateKindSessionInfoUpdate && update.SessionInfoUpdate != nil &&
		update.SessionInfoUpdate.Meta != nil {
		if value, ok := update.SessionInfoUpdate.Meta.Get("execution"); ok {
			encoded, err := jsonvalue.Marshal(value)
			if err != nil {
				return 0, err
			}
			var execution core.GoalExecution
			if err := json.Unmarshal(encoded, &execution); err != nil {
				return 0, err
			}
			resumeRunning = execution.Status == "running" &&
				nativeAfter.Execution != nil && reflect.DeepEqual(*nativeAfter.Execution, execution)
		}
	}
	resumesGoal := func(pending PendingNativeResume) bool {
		return pending.goal == nil ||
			(nativeAfter.Snapshot != nil && nativeAfter.Snapshot.SameGoal(pending.goal))
	}
	resumedNative := resumeRunning && slices.ContainsFunc(r.pendingNativeResumes, resumesGoal)
	codex := r.harnessTurns == HarnessTurnPolicyCodexAdapter
	if (codex && !nativeBefore && nativeAfter.Running()) || (resumedNative && r.snapshot.HarnessTurn == nil) {
		if _, err := r.appendRelayEvent(nil, core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()}); err != nil {
			return 0, err
		}
	}
	// Arrival after the marker does not associate an idle update with this
	// resume. Only running proof or an explicit goal change settles it.
	var goalReport any
	goalReported := false
	if update.Kind == acpschema.SessionUpdateKindSessionInfoUpdate && update.SessionInfoUpdate != nil &&
		update.SessionInfoUpdate.Meta != nil {
		goalReport, goalReported = core.GoalMeta(update.SessionInfoUpdate.Meta)
	}
	explicitlyCleared := goalReported && goalReport == nil
	kept := r.pendingNativeResumes[:0]
	for _, pending := range r.pendingNativeResumes {
		settled := explicitlyCleared ||
			(goalReported && pending.goal != nil &&
				(nativeAfter.Snapshot == nil ||
					!nativeAfter.Snapshot.SameGoal(pending.goal) ||
					(pending.goal.Status != nativeAfter.Snapshot.Status && !nativeAfter.Snapshot.Active())))
		if !(settled || (resumedNative && r.snapshot.HarnessTurn != nil && resumesGoal(pending))) {
			kept = append(kept, pending)
		}
	}
	clear(r.pendingNativeResumes[len(kept):])
	r.pendingNativeResumes = kept
	claude := r.harnessTurns == HarnessTurnPolicyClaudeAdapter
	// A stop we requested is acknowledged with a plain chunk and nothing
	// else: no model cycle runs, so no result would ever settle a turn
	// opened for it. Consume the expectation and keep the line.
	var owner *string
	if r.snapshot.NativeSessionID != nil {
		id := *r.snapshot.NativeSessionID
		owner = &id
	}
	ack := claude && r.consumeClaudeStopAcknowledgement(owner, &update)
	// The adapter publishes notices as agent text at times no model cycle
	// runs. Such text is a notice row of its own, so it neither opens a turn
	// that nothing would end nor joins the previous reply.
	if claude && !ack {
		if message, ok := r.noticeOutsideAnyTurn(&update); ok {
			return r.RecordObservation(core.RelayObservationNotice{Message: message})
		}
	}
	if claude && !ack && r.opensHarnessTurn(&update) {
		if err := r.openHarnessTurnIfAllowed(); err != nil {
			return 0, err
		}
	}
	started := core.EpochMillis()
	if step := r.stepClock.StartedAtMs(); step != nil {
		started = *step
	}
	r.foregroundTools.ObserveWithStart(&update, started)
	if r.backgroundWork == BackgroundWorkPolicyKimiTasks {
		r.trackAgentTerminalToolCall(&update)
		r.trackKimiBackgroundAgent(&update)
	}
	ordinal, err := r.RecordObservation(core.RelayObservationSessionUpdate{Update: update})
	if err != nil {
		return 0, err
	}
	// A Claude turn settles on its SDK result instead, in ClaudeTurnResult:
	// the adapter's origin marker is left out when a cycle produced no
	// assistant usage.
	if codex && nativeBefore && !nativeAfter.Running() && r.snapshot.HarnessTurn != nil {
		origin := "codex"
		if err := r.settleHarnessTurn(&origin, "end_turn"); err != nil {
			return 0, err
		}
	}
	return ordinal, nil
}

// claudeResultSettlesTurn is whether a Claude cycle result ends the open
// harness turn. Since claude-agent-acp 0.101.1 (#1455), an announced task
// owns its boundary: raw results can precede its final output or belong to a
// previous cycle, and only that task's terminal edge may release its turn.
func (r *DurableRelay) claudeResultSettlesTurn() bool {
	return r.harnessTurns == HarnessTurnPolicyClaudeAdapter &&
		r.snapshot.ActivePrompt == nil &&
		r.snapshot.HarnessTurn != nil &&
		!r.announcedHarnessTurn()
}

// EventSettlesAutonomousTurn is whether applying event would end a turn the
// harness started on its own. The coordinator asks before it applies the
// event, so it can observe the worker's processes first and hand the facts
// over with OfferSettleFacts: the same observation, taken at the same point,
// as for a prompt that is finishing. Each arm names the condition its
// settling site tests.
func (r *DurableRelay) EventSettlesAutonomousTurn(event core.RuntimeEvent) bool {
	if r.snapshot.ActivePrompt != nil || r.snapshot.AutonomousHarnessTurn() == nil {
		return false
	}
	switch event := event.(type) {
	case core.RuntimeEventClaudeTurnResult:
		return r.claudeResultSettlesTurn()
	case core.RuntimeEventAirTaskChanged:
		return r.airUpdateSettlesTurn(event.SessionID, event.Update)
	case core.RuntimeEventSessionUpdate:
		if r.harnessTurns != HarnessTurnPolicyCodexAdapter || !r.snapshot.Goal.Running() {
			return false
		}
		var update acpschema.SessionUpdate
		if json.Unmarshal(event.Update, &update) != nil {
			return false
		}
		after := r.snapshot.Goal
		_, err := after.Apply(&update)
		return err == nil && !after.Running()
	}
	return false
}

// OfferSettleFacts hands the facts observed for the event about to be
// applied to the relay. Replaces whatever an earlier event was given.
func (r *DurableRelay) OfferSettleFacts(facts *core.WorkFacts) {
	r.pendingSettleFacts = facts
}

// ClaudeTurnResult: Claude Code ended a model cycle that did not end one of
// Mjolnir's prompts. If a turn Claude Code started on its own is open, this
// result is its boundary, whatever started the cycle.
func (r *DurableRelay) ClaudeTurnResult(result *core.ClaudeTurnResult) error {
	if !r.claudeResultSettlesTurn() {
		return nil
	}
	origin := "human"
	if result.OriginKind != nil {
		origin = *result.OriginKind
	}
	stopReason := "end_turn"
	if result.IsError {
		stopReason = promptErrorStopReason
	}
	return r.settleHarnessTurn(&origin, stopReason)
}

// announcedHarnessTurn: the announced task owns completion unless an
// unanswered user stop overrides it.
func (r *DurableRelay) announcedHarnessTurn() bool {
	turn := r.snapshot.HarnessTurn
	if turn == nil {
		return false
	}
	for _, entry := range r.airTasks {
		if ordinal, ok := entry.turnOrdinal(); ok && ordinal == turn.FirstOrdinal {
			return true
		}
	}
	return false
}

// UnansweredHarnessTurnStop is the first ordinal of the open Claude harness
// turn a stop was applied to, while nothing has answered that stop yet.
// Claude Code answers a stop of a running cycle with the cycle's result; a
// turn it never ran gets no answer, so the coordinator ends it after
// HarnessTurnStopGrace. The snapshot owns the stop: a journal event records
// it, so it is the same on every path and after a restart.
func (r *DurableRelay) UnansweredHarnessTurnStop() (uint64, bool) {
	if r.harnessTurns != HarnessTurnPolicyClaudeAdapter && r.harnessTurns != HarnessTurnPolicyAntigravity {
		return 0, false
	}
	turn := r.snapshot.AutonomousHarnessTurn()
	if turn == nil {
		return 0, false
	}
	stop := r.snapshot.CancellingHarnessTurn
	if stop == nil || !stop.Applied || stop.FirstOrdinal != turn.FirstOrdinal {
		return 0, false
	}
	return turn.FirstOrdinal, true
}

// EndUnansweredHarnessTurnStop ends the harness turn a stop was applied to,
// if it is still open. A user stop overrides even an announced task that
// never reports its end. Returns whether a turn was ended.
func (r *DurableRelay) EndUnansweredHarnessTurnStop(firstOrdinal uint64) (bool, error) {
	if ordinal, ok := r.UnansweredHarnessTurnStop(); !ok || ordinal != firstOrdinal {
		return false, nil
	}
	slog.Warn("The harness did not answer a stop of its autonomous turn; ending the turn",
		"session_id", r.snapshot.SessionID, "first_ordinal", firstOrdinal)
	origin := "stop_unanswered"
	if err := r.settleHarnessTurn(&origin, "cancelled"); err != nil {
		return false, err
	}
	return true, nil
}

// configurationRequestInFlight is whether a request that configures the
// session, rather than a prompt, is waiting for the harness's answer. The
// Claude adapter publishes some notices as agent text while it answers such
// a request: a switch to a model without Auto mode sends "Auto mode
// unavailable". No model cycle runs for it, so no result would ever end a
// turn opened for that text.
func (r *DurableRelay) configurationRequestInFlight() bool {
	for _, dispatch := range r.snapshot.Dispatches {
		if dispatch.State != core.RelayDispatchStateInFlight {
			continue
		}
		switch dispatch.Command.(type) {
		case core.RelayCommandSetConfig, core.RelayCommandSetSessionMode, core.RelayCommandRestoreExecutionMode:
			return true
		}
	}
	return false
}

func (r *DurableRelay) settleHarnessTurn(origin *string, stopReason string) error {
	var settledOrdinal *uint64
	if turn := r.snapshot.HarnessTurn; turn != nil {
		first := turn.FirstOrdinal
		settledOrdinal = &first
	}
	workFacts := r.pendingSettleFacts
	r.pendingSettleFacts = nil
	var autonomous *core.SettledHarnessTurn
	if turn := r.snapshot.AutonomousHarnessTurn(); turn != nil && r.snapshot.ActivePrompt == nil {
		// claude-agent-acp 0.99.0 / Claude Code 2.1.308 (2026-11-06) forwards
		// SDK results, including error-shaped interrupted cycles. Its
		// interrupt has no yield deadline: a result can arrive after
		// stop_unanswered. The snapshot's recorded stop owns the cancellation
		// reason.
		reason := stopReason
		if stop := r.snapshot.CancellingHarnessTurn; stop != nil && stop.FirstOrdinal == turn.FirstOrdinal {
			reason = "cancelled"
		}
		autonomous = &core.SettledHarnessTurn{
			StartOrdinal: turn.FirstOrdinal,
			StopReason:   reason,
			WorkFacts:    workFacts,
		}
	}
	if _, err := r.appendRelayEvent(nil, core.RelayObservationHarnessTurnSettled{
		Autonomous: autonomous,
		Origin:     origin,
		// The projection cannot see active_prompt, so the event has to carry
		// whether a prompt of ours is still running.
		PromptInFlight: r.snapshot.ActivePrompt != nil,
	}); err != nil {
		return err
	}
	// Settlement and detachment share the relay owner lock. Keep task IDs as
	// tombstones so late terminal updates cannot claim a successor. Late
	// output may open an unannounced turn after its raw result; a fresh Stop
	// still ends that turn through the same grace fallback.
	for _, entry := range r.airTasks {
		if entry.state.Kind != AirTaskStateKindTurn {
			continue
		}
		ordinal := entry.state.TurnOrdinal
		if (ordinal == nil) == (settledOrdinal == nil) && (ordinal == nil || *ordinal == *settledOrdinal) {
			entry.state.TurnOrdinal = nil
		}
	}
	return r.finishTurnActivity()
}

// finishTurnActivity forgets provisional tool statuses at a boundary the
// harness itself has confirmed. Independently tracked background work
// survives into idle.
func (r *DurableRelay) finishTurnActivity() error {
	r.foregroundTools.Clear()
	return r.persistActivityTransition()
}

// outputOutsideAnyTurn is whether agent output arriving now belongs to no
// turn: no prompt runs, no harness turn is open, and the session is either
// still being set up or answering a configuration request. The Claude
// adapter reports its Auto mode fallback as agent text at both times: while
// a resumed session loads and its model is applied (R8-4), and while it
// answers a live model change (R4-2, R8-5).
func (r *DurableRelay) outputOutsideAnyTurn() bool {
	return (r.sessionSetup || r.configurationRequestInFlight()) &&
		r.snapshot.ActivePrompt == nil &&
		r.snapshot.HarnessTurn == nil
}

// noticeOutsideAnyTurn is the notice row for agent text that belongs to no
// turn, without the adapter's Markdown emphasis, which a notice row would
// show literally.
func (r *DurableRelay) noticeOutsideAnyTurn(update *acpschema.SessionUpdate) (string, bool) {
	if !r.outputOutsideAnyTurn() {
		return "", false
	}
	text, ok := agentChunkText(update)
	if !ok {
		return "", false
	}
	message := strings.TrimSpace(strings.ReplaceAll(text, "**", ""))
	return message, message != ""
}

// opensHarnessTurn is whether this update reveals the harness working with
// nothing of Hel's in flight, which is what opens a harness-initiated turn.
// Output that arrives while a configuration request waits for its answer
// belongs to that request, and output while the session is set up belongs
// to no turn.
func (r *DurableRelay) opensHarnessTurn(update *acpschema.SessionUpdate) bool {
	// A tool update refines a call the adapter already announced. When a
	// cycle ends, the adapter fails its unfinished calls ("Claude stopped
	// this tool call before it ran.") after the result, and no cycle would
	// end a turn opened for that status. Checked against claude-agent-acp
	// 0.94.0 dist/tool-calls/renderer.js (unfinished) on 2026-10-23; the file
	// is unchanged in 0.97.0. Our terminal inventory/card is not evidence of
	// a parent model cycle: a child may request it while the parent remains
	// idle.
	return isAgentOutput(update) &&
		update.Kind != acpschema.SessionUpdateKindToolCallUpdate &&
		!(update.Kind == acpschema.SessionUpdateKindToolCall && core.IsFallbackTerminalToolCall(update.ToolCall)) &&
		r.canOpenHarnessTurn()
}

// canOpenHarnessTurn is the shared admission decision for Claude output and
// autonomous spawns.
func (r *DurableRelay) canOpenHarnessTurn() bool {
	return r.snapshot.ActivePrompt == nil &&
		r.snapshot.HarnessTurn == nil &&
		!r.sessionSetup &&
		!r.configurationRequestInFlight() &&
		r.snapshot.Execution != core.RelayExecutionStateClosing &&
		r.snapshot.Execution != core.RelayExecutionStateClosed
}

// openHarnessTurnIfAllowed opens the one durable boundary shared by output
// and turn announcements.
func (r *DurableRelay) openHarnessTurnIfAllowed() error {
	if !r.canOpenHarnessTurn() {
		return nil
	}
	// New work invalidates a ready checkpoint cut. Replaying earlier held
	// writes may itself open a turn, so ask the owner again afterwards.
	if err := r.EndCheckpointHold(); err != nil {
		return err
	}
	if r.canOpenHarnessTurn() {
		r.taskSettledAtMs = nil
		if _, err := r.appendRelayEvent(nil, core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()}); err != nil {
			return err
		}
	}
	return nil
}

func (r *DurableRelay) digestAt(ordinal uint64) (string, bool, error) {
	plan := r.replayPlan()
	return plan.digestAt(ordinal)
}

// seedLegacyOpenRequest seeds once at the legacy persisted-format boundary,
// never during publication.
func seedLegacyOpenRequest(snapshot *core.RelaySnapshot) {
	assessment := snapshot.Assessment
	if assessment == nil || assessment.QuestionDismissed() {
		return
	}
	question := assessment.InferredQuestion()
	if question == nil && core.ClassifyPromptCompletion(assessment.Completion.StopReason) == core.PromptCompletionInputRequired {
		question = &core.InferredQuestion{
			TurnID:           assessment.TurnID,
			CompletedOrdinal: assessment.CompletedOrdinal,
		}
	}
	if question == nil {
		return
	}
	snapshot.PersonInputThrough = 0
	if accepted := snapshot.LatestPromptAcceptedOrdinal; accepted != nil && *accepted > question.CompletedOrdinal {
		snapshot.PersonInputThrough = *accepted
	}
	message := assessment.QuestionMessage
	if message == nil {
		message = snapshot.CompletedReplyMessage
	}
	if message != nil {
		copied := *message
		message = &copied
	}
	snapshot.Request = &core.OpenRequest{Question: *question, QuestionMessage: message}
}

func seedLegacyNotificationStack(snapshot *core.RelaySnapshot) {
	var entries []core.NotificationStackEntry
	if request := snapshot.OpenRequest(); request != nil {
		var completedAtMs int64
		if assessment := snapshot.Assessment; assessment != nil && assessment.CompletedOrdinal == request.Question.CompletedOrdinal {
			completedAtMs = assessment.CompletedAtMs
		}
		var message core.EntryMessage = core.EntryMessageNone{}
		if request.QuestionMessage != nil {
			message = core.EntryMessageFound{Value: *request.QuestionMessage}
		}
		entries = append(entries, core.NotificationStackEntry{
			CompletedAtMs:    completedAtMs,
			CompletedOrdinal: request.Question.CompletedOrdinal,
			Kind:             core.NotificationKindNeedsInput,
			Message:          message,
		})
	}
	if assessment := snapshot.Assessment; assessment != nil &&
		assessment.Current() &&
		assessment.Attention != nil && *assessment.Attention == core.AssessmentAttentionImportant &&
		assessment.CompletedOrdinal > snapshot.PersonInputThrough &&
		(snapshot.TurnCompletion == nil ||
			(snapshot.TurnCompletion.CompletedOrdinal == assessment.CompletedOrdinal &&
				snapshot.TurnCompletion.CommandID == assessment.TurnID)) &&
		!slices.ContainsFunc(entries, func(entry core.NotificationStackEntry) bool {
			return entry.CompletedOrdinal == assessment.CompletedOrdinal
		}) {
		var message core.EntryMessage = core.EntryMessageNone{}
		if snapshot.CompletedReplyMessage != nil {
			message = core.EntryMessageFound{Value: *snapshot.CompletedReplyMessage}
		}
		entries = append(entries, core.NotificationStackEntry{
			CompletedAtMs:    assessment.CompletedAtMs,
			CompletedOrdinal: assessment.CompletedOrdinal,
			Kind:             core.NotificationKindImportant,
			Message:          message,
		})
		slices.SortStableFunc(entries, func(a, b core.NotificationStackEntry) int {
			return cmp.Compare(a.CompletedOrdinal, b.CompletedOrdinal)
		})
	}
	if len(entries) > 0 {
		snapshot.NotificationStack = core.NotificationStack{
			Revision: snapshot.LatestOrdinal,
			Entries:  entries,
		}
	}
}

// seedMissingNotificationTime: older stack snapshots omitted the time. The
// matching assessment retains the original completion event's time even
// when its journal was collected.
func seedMissingNotificationTime(snapshot *core.RelaySnapshot) {
	assessment := snapshot.Assessment
	if assessment == nil {
		return
	}
	index := slices.IndexFunc(snapshot.NotificationStack.Entries, func(entry core.NotificationStackEntry) bool {
		return entry.CompletedAtMs == 0 && entry.CompletedOrdinal == assessment.CompletedOrdinal
	})
	if index < 0 {
		return
	}
	// The stack's entries may be shared with an earlier snapshot copy, so
	// they are copied before the edit (Rust's Arc::make_mut).
	entries := slices.Clone(snapshot.NotificationStack.Entries)
	entries[index].CompletedAtMs = assessment.CompletedAtMs
	snapshot.NotificationStack.Entries = entries
}
