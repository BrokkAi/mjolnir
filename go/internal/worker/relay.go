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
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// HarnessTurnPolicy is whether this relay models turns the harness starts on
// its own.
//
// Only Claude Code's adapter marks the end of such a turn today (an origin
// key on the settling usage_update), so every other harness keeps the
// behaviour it had before harness turns existed. See
// .agents/docs/claude-autonomous-turns.md.
type HarnessTurnPolicy int

const (
	HarnessTurnPolicyDisabled HarnessTurnPolicy = iota
	HarnessTurnPolicyClaudeAdapter
	HarnessTurnPolicyCodexAdapter
)

// SteeringDeliveryMode is how this harness receives an accepted steer.
type SteeringDeliveryMode int

const (
	SteeringDeliveryModeAcp SteeringDeliveryMode = iota
	// SteeringDeliveryModeAcpWithMailbox: the harness has no mailbox hook;
	// carry waking events through its ordinary durable prompt queue and
	// safe-boundary steering instead.
	SteeringDeliveryModeAcpWithMailbox
	SteeringDeliveryModeClaudePostToolBatch
	SteeringDeliveryModeClaudeTurnEnd
)

// steeringDeliveryModeForHarness is SteeringDeliveryMode::for_harness.
func steeringDeliveryModeForHarness(harness core.HarnessKind, claudeHook bool) SteeringDeliveryMode {
	switch {
	case harness == core.HarnessKindDsh:
		return SteeringDeliveryModeAcpWithMailbox
	case harness == core.HarnessKindClaude && claudeHook:
		return SteeringDeliveryModeClaudePostToolBatch
	case harness == core.HarnessKindClaude:
		return SteeringDeliveryModeClaudeTurnEnd
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
	return &RelayCell{relay: relay, released: &relayReleaseWatch{changed: make(chan struct{})}}
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
	pendingLiveBaselines  []PendingLiveBaseline
	pendingNativeResumes  []PendingNativeResume
	acpActivity           core.AcpActivityClock
	stepClock             *core.StepClock
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
	verdictHarness  *core.HarnessKind
	// authenticationLoginPath is read only by the Unix worker control runtime.
	authenticationLoginPath *string
	loginIdentity           *ClaudeLoginIdentity
	repliedVerdict          RepliedVerdictState
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
	// kimiBackgroundTasks are Kimi detached agents and processes confirmed by
	// its native journal.
	kimiBackgroundTasks map[string]KimiTaskEntry
	// kimiProvisionalTasks is positive ACP launch evidence retained until the
	// native journal correlates it through native task identity or
	// parentToolCallId.
	kimiProvisionalTasks map[string]KimiProvisionalTask
	kimiObservedTaskIDs  map[string]struct{}
	kimiObservedToolIDs  map[string]struct{}
	// backgroundWorkKnown is whether Kimi's provider-owned background work is
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
	panic("unported: mj-worker/src/relay.rs service_live_baselines")
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
	panic("unported: mj-worker/src/relay.rs adoption_blockers")
}

func (r *DurableRelay) LiveSessionBaseline() (core.LiveSessionBaseline, error) {
	panic("unported: mj-worker/src/relay.rs live_session_baseline")
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
	panic("unported: mj-worker/src/relay.rs discard_queued_prompts_before_start")
}

func durableRelayOpenWithHarness(root, sessionID, relayVersion string, checkpointOnly bool, harness *core.HarnessKind, initialCommands []core.InitialWorkerCommand) (*DurableRelay, error) {
	panic("unported: mj-worker/src/relay.rs open_with_harness")
}

// seedInitialCommands completes the creation prefix before publishing any
// control endpoint. The final prompt's receipt proves the entire prefix was
// journaled. Once a turn has run, receipt compaction must never cause
// initialization again.
func (r *DurableRelay) seedInitialCommands() error {
	panic("unported: mj-worker/src/relay.rs seed_initial_commands")
}

// ensureInitialCommandsSucceeded: a failed required creation command cannot
// release the original brief or any follow-up. Its journaled failure also
// stops a restarted worker.
func (r *DurableRelay) ensureInitialCommandsSucceeded() error {
	panic("unported: mj-worker/src/relay.rs ensure_initial_commands_succeeded")
}

// adoptUnqueuedQueueCommands adopts queueable commands that an earlier relay
// format accepted outside the durable queue. Configuration changes used to
// dispatch through the control path, so a command accepted just before an
// upgrade would otherwise stay accepted forever without ever being promoted.
func (r *DurableRelay) adoptUnqueuedQueueCommands() error {
	panic("unported: mj-worker/src/relay.rs adopt_unqueued_queue_commands")
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
	panic("unported: mj-worker/src/relay.rs operational_state")
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

// SetAutomaticSteering allows the relay to steer queued prompts into the
// running turn itself.
func (r *DurableRelay) SetAutomaticSteering(enabled bool) {
	r.automaticSteering = enabled
}

// SetSteeringDeliveryMode selects steering delivery for the configured
// harness and staged profile. The safe Claude turn-end mode is selected
// before worker admission opens; preparation upgrades it only after
// verifying a usable hook marker.
func (r *DurableRelay) SetSteeringDeliveryMode(harness core.HarnessKind, claudePostToolBatchHook bool) {
	r.steeringDeliveryMode = steeringDeliveryModeForHarness(harness, claudePostToolBatchHook)
}

// SetMailboxDeliveryMode sets when a Codex session's mailbox messages may
// reach it, from the provider in its staged configuration. The PostToolUse
// drain and the published operational state both read this one field.
func (r *DurableRelay) SetMailboxDeliveryMode(mode core.MailboxDeliveryMode) {
	r.mailboxDeliveryMode = &mode
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
	panic("unported: mj-worker/src/relay.rs activity_facts")
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
	return r.suppressChildAssessment()
}

// watchedWork is the latest finished turn's work processes while some may
// still be alive, with the identities the worker's watcher recounts.
func (r *DurableRelay) watchedWork() (core.WorkLiveness, []core.ProcessIdentity, bool) {
	panic("unported: mj-worker/src/relay.rs watched_work")
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
	panic("unported: mj-worker/src/relay.rs set_turn_verdict_harness")
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
	panic("unported: mj-worker/src/relay.rs native_session_history")
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
	panic("unported: mj-worker/src/relay.rs record_observation")
}

func (r *DurableRelay) RecordSessionUpdate(update acpschema.SessionUpdate) (uint64, error) {
	panic("unported: mj-worker/src/relay.rs record_session_update")
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
	panic("unported: mj-worker/src/relay.rs event_settles_autonomous_turn")
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
	if r.harnessTurns != HarnessTurnPolicyClaudeAdapter {
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
	slog.Warn("Claude Code did not answer a stop of the turn it started on its own; ending the turn",
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
	panic("unported: mj-worker/src/relay.rs settle_harness_turn")
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
	panic("unported: mj-worker/src/relay.rs seed_legacy_open_request")
}

func seedLegacyNotificationStack(snapshot *core.RelaySnapshot) {
	panic("unported: mj-worker/src/relay.rs seed_legacy_notification_stack")
}

// seedMissingNotificationTime: older stack snapshots omitted the time. The
// matching assessment retains the original completion event's time even
// when its journal was collected.
func seedMissingNotificationTime(snapshot *core.RelaySnapshot) {
	panic("unported: mj-worker/src/relay.rs seed_missing_notification_time")
}
