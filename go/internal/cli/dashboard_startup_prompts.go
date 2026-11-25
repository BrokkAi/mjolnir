// Port of mj-cli/src/dashboard/startup_prompts.rs.
//
// Ordered host requests and durable receipts for prompts entered before
// attachment.

package cli

import (
	"context"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// finishNotificationReply is the one outcome owner for detached replies,
// including admission failures and durable refusals. The card may no longer
// exist when either arrives. result is nil when the reply was accepted;
// otherwise its text is the failure.
func finishNotificationReply(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat, sessionID string, prompt *chat.StartupPrompt, result error) bool {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs finish_notification_reply")
}

// retryAdmission runs attempt until it reports done or settlement ends,
// backing off from one second, doubling to at most thirty, between
// attempts. attempt's context ends with settlement.
func retryAdmission(settlement context.Context, attempt func(ctx context.Context) bool) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs retry_admission")
}

// admissionFailureIsDefinite reports whether err proves the request was
// not accepted. A conflict describes another immutable request under the
// original ID; it cannot establish that the original request was
// unaccepted.
func admissionFailureIsDefinite(err error) bool {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs admission_failure_is_definite")
}

// saveJob is one ordered startup request (Rust's pinned boxed future).
type saveJob func(ctx context.Context)

// sessionMailboxCapacity is the Rust mailbox channel's bound.
const sessionMailboxCapacity = 64

// sessionMailbox runs one session's startup requests in order on a worker
// of its own, which stops when the mailbox is closed (Rust's
// AbortOnDropHandle).
type sessionMailbox struct {
	sender chan saveJob
	stop   context.CancelFunc
}

func newSessionMailbox() *sessionMailbox {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs SessionMailbox::new")
}

// admit queues work without waiting, or fails when the queue is full or
// gone.
func (m *sessionMailbox) admit(work saveJob) error {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs SessionMailbox::admit")
}

// startupPrompts tracks every prompt queued before its session attached,
// from admission to a durable outcome. The zero value is Rust's Default.
type startupPrompts struct {
	mailboxes          map[string]*sessionMailbox
	pending            map[string]map[string]trackedPrompt
	polls              map[string]*outcomePoll
	nextOrder          uint64
	moves              map[string]moveAdmission
	nextMoveGeneration uint64
}

// heldSave keeps a quit waiting while a prompt is held for its move's
// origin.
type heldSave struct {
	guard *client.CriticalOperationGuard
}

// moveAdmission is the move a session's held prompts wait for.
type moveAdmission struct {
	generation uint64
	phase      moveAdmissionPhase
}

// moveAdmissionPhase is how far a session's move has come.
type moveAdmissionPhase interface{ isMoveAdmissionPhase() }

type moveAdmissionPhasePreparing struct{ id *string }

type moveAdmissionPhaseReady struct{ id string }

type moveAdmissionPhaseCompleted struct{ id string }

type moveAdmissionPhaseFailed struct{ id *string }

func (moveAdmissionPhasePreparing) isMoveAdmissionPhase() {}
func (moveAdmissionPhaseReady) isMoveAdmissionPhase()     {}
func (moveAdmissionPhaseCompleted) isMoveAdmissionPhase() {}
func (moveAdmissionPhaseFailed) isMoveAdmissionPhase()    {}

// originAdmission is what a prompt naming a move origin may do now.
type originAdmission uint8

const (
	originAdmissionHold originAdmission = iota
	originAdmissionSend
	originAdmissionRefuse
)

// trackedPrompt is one queued prompt: waiting for its outcome, or settled.
type trackedPrompt interface{ isTrackedPrompt() }

// trackedPromptWaiting is a prompt still waiting for a durable outcome; the
// map holds it by pointer so it is updated in place.
type trackedPromptWaiting struct {
	prompt         chat.StartupPrompt
	inheritedDraft *string
	recalled       bool
	admissionError *string
	order          uint64
	held           *heldSave
	// settlement ends once the prompt settles (Rust's CancellationToken);
	// settle ends it.
	settlement context.Context
	settle     context.CancelFunc
}

type trackedPromptSettled struct{}

func (*trackedPromptWaiting) isTrackedPrompt() {}
func (trackedPromptSettled) isTrackedPrompt()  {}

// outcomePoll is one session's outcome query and its backoff.
type outcomePoll struct {
	// task stops the query in flight (Rust's AbortOnDropHandle); nil when
	// none runs.
	task    context.CancelFunc
	next    time.Time
	backoff time.Duration
	offset  int
}

// newOutcomePoll is Rust's Default: due now, with a one-second backoff.
func newOutcomePoll() *outcomePoll {
	return &outcomePoll{next: time.Now(), backoff: time.Second}
}

func (p *startupPrompts) setMovePhase(sessionID string, phase moveAdmissionPhase) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::set_move_phase")
}

func (p *startupPrompts) beginMove(sessionID string, id *string) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::begin_move")
}

func (p *startupPrompts) prepareMove(sessionID string, id *string) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::prepare_move")
}

// moveOrigin is the generation and operation id of a move still preparing
// or ready; false otherwise.
func (p *startupPrompts) moveOrigin(sessionID string) (generation uint64, id *string, ok bool) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::move_origin")
}

// terminalMove is the generation, operation id and success of a move that
// completed or failed; ok is false otherwise.
func (p *startupPrompts) terminalMove(sessionID string) (generation uint64, id *string, completed bool, ok bool) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::terminal_move")
}

// observeMove records a move the daemon reported; terminal is nil while it
// runs, and whether it completed once it ended.
func (p *startupPrompts) observeMove(sessionID string, id string, terminal *bool) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::observe_move")
}

func (p *startupPrompts) finishMove(sessionID string, completed *string) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::finish_move")
}

func (p *startupPrompts) originAdmission(sessionID string, origin *string) originAdmission {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::origin_admission")
}

func (p *startupPrompts) flushHeld(state *core.StateState, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::flush_held")
}

func (p *startupPrompts) retainUnconfirmed(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::retain_unconfirmed")
}

func (p *startupPrompts) cancelUnadmitted(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::cancel_unadmitted")
}

func (p *startupPrompts) enqueue(session namedSession, prompt chat.StartupPrompt, inheritedDraft *string, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	p.enqueueInner(session, prompt, inheritedDraft, updates, tracker, false)
}

func (p *startupPrompts) resume(session namedSession, prompt chat.StartupPrompt, inheritedDraft *string, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::resume")
}

func (p *startupPrompts) enqueueInner(session namedSession, prompt chat.StartupPrompt, inheritedDraft *string, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker, recovering bool) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::enqueue_inner")
}

func (p *startupPrompts) withdraw(session namedSession, prompt chat.StartupPrompt, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::withdraw")
}

func (p *startupPrompts) isSettled(sessionID, commandID string) bool {
	_, settled := p.pending[sessionID][commandID].(trackedPromptSettled)
	return settled
}

// admissionFailed records a failed admission. False means admission is
// uncertain or already settled: never create a resendable duplicate.
func (p *startupPrompts) admissionFailed(sessionID, commandID, failure string, definite bool) bool {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::admission_failed")
}

func (p *startupPrompts) withdrawalNeedsReceipt(sessionID, commandID string) bool {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::withdrawal_needs_receipt")
}

func (p *startupPrompts) poll(updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::poll")
}

func (p *startupPrompts) outcomes(sessionID string, succeeded bool) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::outcomes")
}

func (p *startupPrompts) applyOutcomes(sessionID string, outcomes map[string]client.StartupPromptOutcome, dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::apply_outcomes")
}

func (p *startupPrompts) orderOf(sessionID, commandID string) uint64 {
	if waiting, ok := p.pending[sessionID][commandID].(*trackedPromptWaiting); ok {
		return waiting.order
	}
	return ^uint64(0)
}

// knownPrompt is a waiting prompt and whether it was recalled; ok is false
// when it is unknown or settled.
func (p *startupPrompts) knownPrompt(sessionID, commandID string) (prompt chat.StartupPrompt, recalled bool, ok bool) {
	if waiting, waits := p.pending[sessionID][commandID].(*trackedPromptWaiting); waits {
		return waiting.prompt, waiting.recalled, true
	}
	return chat.StartupPrompt{}, false, false
}

func (p *startupPrompts) settle(sessionID, commandID string) {
	panic("unported: mj-cli/src/dashboard/startup_prompts.rs StartupPrompts::settle")
}
