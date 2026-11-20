// Port of mj-worker/src/acp/operator_away.rs.

// Operator availability: questions the operator-away policy answers, the
// refusals it remembers, and the tool permission requests a turn withdrawal
// reaches.

package worker

import (
	"context"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// OperatorAway is process-local operator availability and refusal memory,
// retained across bridge restarts. Share it as *OperatorAway (Rust clones an
// Arc); the zero value is ready to use (Rust Default).
type OperatorAway struct {
	mu    sync.Mutex
	state OperatorAwayState
}

// awayIdentity is who asked and what: the agent and the kind of request
// (Rust `(String, AwayRequest)`).
type awayIdentity struct {
	agent   string
	request AwayRequest
}

type OperatorAwayState struct {
	presence core.OperatorPresence
	// refused: the accepted answers are both the fallback identity and Jev's
	// evidence.
	refused    map[awayIdentity][]any
	nextBridge uint64
	bridges    map[uint64]struct{}
	pending    []*PendingElicitation
	// reviews are the Mjolnir guardian reviews in flight. They are
	// outstanding tool permission requests like the forms in pending, so the
	// same lock decides what is outstanding and withdraws both at once.
	reviews    []PendingReview
	nextReview uint64
	// withdrawnTurns are the bridges whose current turn's tool permission
	// requests were withdrawn. A request that arrives later in that turn is
	// answered `cancelled` too, until the turn ends.
	withdrawnTurns map[uint64]struct{}
	changed        *core.Watch[struct{}]
}

type PendingReview struct {
	bridge uint64
	id     uint64
	// withdrawn is done once the review is withdrawn; withdraw cancels it
	// (Rust CancellationToken).
	withdrawn context.Context
	withdraw  context.CancelFunc
}

// DefaultOperatorAwayState is OperatorAwayState::default().
func DefaultOperatorAwayState() OperatorAwayState {
	return OperatorAwayState{
		refused:        map[awayIdentity][]any{},
		bridges:        map[uint64]struct{}{},
		withdrawnTurns: map[uint64]struct{}{},
		changed:        core.NewWatch(struct{}{}),
	}
}

// lockState locks the owner and returns its state, initialized on first use
// (Rust Default). The caller unlocks o.mu.
func (o *OperatorAway) lockState() *OperatorAwayState {
	o.mu.Lock()
	if o.state.changed == nil {
		o.state = DefaultOperatorAwayState()
	}
	return &o.state
}

func (s *OperatorAwayState) answerAway(entry *PendingElicitation) {
	panic("unported: mj-worker/src/acp/operator_away.rs OperatorAwayState::answer_away")
}

// questionEvidence retains answer constraints, excluding transient identity,
// secret defaults and the required flag. The user ruled (2026-11-13) that a
// form differing only by required versus optional repeats the earlier
// question, so the flag never reaches the classifier and cannot make two
// forms differ.
func questionEvidence(question *core.ElicitationRequest) any {
	panic("unported: mj-worker/src/acp/operator_away.rs question_evidence")
}

// AwayRequest is the kind of request the operator-away policy answers.
type AwayRequest interface {
	isAwayRequest()
}

type AwayRequestQuestion struct{}

type AwayRequestPlan struct{}

type AwayRequestTool struct{ Value string }

func (AwayRequestQuestion) isAwayRequest() {}
func (AwayRequestPlan) isAwayRequest()     {}
func (AwayRequestTool) isAwayRequest()     {}

// liveQuestions are the forms still waiting on a live bridge and responder,
// by id.
func (o *OperatorAway) liveQuestions() map[string]core.ElicitationRequest {
	// Recovery calls this with the relay held: Relay -> OperatorAway is the
	// lock order. This owner never takes the relay lock while held;
	// answering only wakes a responder, whose FIFO work runs afterwards.
	state := o.lockState()
	defer o.mu.Unlock()
	questions := map[string]core.ElicitationRequest{}
	for _, entry := range state.pending {
		if _, live := state.bridges[entry.bridge]; live && entry.responder.isAlive() {
			questions[entry.question.ID] = entry.question
		}
	}
	return questions
}

// set records the operator's presence. Only the Unix worker runtime receives
// SetOperatorAway; other targets carry the default state so ACP request
// handling has one type.
func (o *OperatorAway) set(presence core.OperatorPresence) {
	panic("unported: mj-worker/src/acp/operator_away.rs OperatorAway::set")
}

// pendingForBridgeWithSource opens a bridge scope. A nil source is the
// default classifier source; a nil turnContext is none.
func (o *OperatorAway) pendingForBridgeWithSource(source VerdictSource, turnContext *transcript.TurnContext) PendingElicitations {
	state := o.lockState()
	defer o.mu.Unlock()
	id := state.nextBridge
	if id+1 < id {
		panic("ACP bridge identity exhausted")
	}
	state.nextBridge = id + 1
	state.bridges[id] = struct{}{}
	return PendingElicitations{bridge: &PendingBridge{owner: o, id: id, source: source, turnContext: turnContext}}
}

// PendingElicitations is a bridge's handle on the presence owner: its live
// responders share the owner's serialized decisions. Copies share the
// bridge (Rust clones an Arc).
type PendingElicitations struct {
	bridge *PendingBridge
}

type PendingBridge struct {
	owner  *OperatorAway
	id     uint64
	source VerdictSource
	// turnContext owns the session id and the decision log the AFK check
	// records into.
	turnContext *transcript.TurnContext
}

// Close retires the bridge (Rust Drop of the last handle).
func (b *PendingBridge) Close() {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingBridge::drop")
}

// forgetReviews withdraws a bridge's reviews in flight and forgets its turn.
func (s *OperatorAwayState) forgetReviews(bridge uint64) {
	panic("unported: mj-worker/src/acp/operator_away.rs OperatorAwayState::forget_reviews")
}

// PendingReviewTicket is a Mjolnir guardian review registered as an
// outstanding tool permission request. Close it, when the review ends, to
// unregister it (Rust Drop).
type PendingReviewTicket struct {
	owner  *OperatorAway
	bridge uint64
	id     uint64
	// withdrawnCtx is done once the turn's tool permission requests are
	// withdrawn.
	withdrawnCtx context.Context
}

// withdrawn is closed when the turn's tool permission requests are
// withdrawn.
func (t *PendingReviewTicket) withdrawn() <-chan struct{} {
	return t.withdrawnCtx.Done()
}

// settle answers the request under the lock that withdraws turns: answer
// learns whether the turn was withdrawn, so no verdict is sent after a
// cancel the agent has already been told about. answer must not take that
// lock itself; an ACP responder only queues a message. settle ends the
// ticket (Rust takes self), so the caller captures any result in answer.
func (t *PendingReviewTicket) settle(answer func(withdrawn bool)) {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingReviewTicket::settle")
}

// Close unregisters the review (Rust Drop).
func (t *PendingReviewTicket) Close() {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingReviewTicket::drop")
}

// PendingBridgeLifetime makes the connection, rather than its cloned
// responder tasks, own bridge life. Closing it when a cancelled driver ends
// retires responders before terminal teardown (Rust Drop).
type PendingBridgeLifetime struct {
	pending PendingElicitations
}

func (l PendingBridgeLifetime) Close() {
	l.pending.close()
}

func (p PendingElicitations) bridgeLifetime() PendingBridgeLifetime {
	return PendingBridgeLifetime{pending: p}
}

func (p PendingElicitations) registerCommandLoop(request core.ElicitationRequest) *CommandLoopQuestion {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::register_command_loop")
}

// register registers a form. Both immediate and later AFK decisions reach
// the same responder task. It publishes Requested before receiving this
// answer, so AFK cannot resolve a form before the relay has observed the
// question.
func (p PendingElicitations) register(question core.ElicitationRequest, agent string, request AwayRequest) *PendingAnswerReceiver {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::register")
}

// refineQuestion asks the classifier whether question id repeats one already
// declined while away. The responder owns this call: human answers and
// cancellation end ctx, which drops its HTTP work; only the presence owner
// may accept the resulting answer.
func (p PendingElicitations) refineQuestion(ctx context.Context, id string) {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::refine_question")
}

func (p PendingElicitations) remove(id string) *PendingElicitation {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::remove")
}

// takeTurnCancel claims Cancel before stopping the turn, so a resolved form
// cannot stop it. The caller keeps the sender until after sending
// session/cancel.
func (p PendingElicitations) takeTurnCancel(id string, response *core.ElicitationResponse) *PendingElicitation {
	if response.Action != core.ElicitationResponseCancel || !strings.HasPrefix(id, core.ToolPermissionIDPrefix) {
		return nil
	}
	return p.remove(id)
}

// beginReview registers a Mjolnir guardian review of a tool permission
// request, so the turn's withdrawal reaches it as it reaches an open form.
// nil when the bridge is gone or its turn was already withdrawn: answer
// `cancelled`.
func (p PendingElicitations) beginReview() *PendingReviewTicket {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::begin_review")
}

// beginWithdrawal marks the turn withdrawn before the agent is told of the
// cancel, so a guardian review that settles in between answers `cancelled`,
// and no request arriving later in the turn is reviewed or shown.
func (p PendingElicitations) beginWithdrawal() {
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	state.withdrawnTurns[p.bridge.id] = struct{}{}
}

// withdrawToolPermissions withdraws the turn's tool permission requests in
// one decision: drop the answer channel of every open form (its task answers
// the agent `cancelled` and closes the form), cancel every guardian review
// in flight, and answer any that arrives later in this turn `cancelled`.
func (p PendingElicitations) withdrawToolPermissions() {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::withdraw_tool_permissions")
}

// endWithdrawal: the withdrawn turn is over, its prompt answered, or a new
// one began.
func (p PendingElicitations) endWithdrawal() {
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	delete(state.withdrawnTurns, p.bridge.id)
}

// onlyReviews reports whether the only outstanding requests are guardian
// reviews, which end on their own within the review deadline.
func (p PendingElicitations) onlyReviews() bool {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::only_reviews")
}

func (p PendingElicitations) isEmpty() bool {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::is_empty")
}

func (p PendingElicitations) close() {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::close")
}

// resolve gives the person's answer to form id; the error is the refusal to
// report to whoever answered (Rust `Result<(), String>`).
func (p PendingElicitations) resolve(id string, response core.ElicitationResponse) error {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitations::resolve")
}

type PendingElicitation struct {
	bridge    uint64
	question  core.ElicitationRequest
	responder PendingResponder
}

// PendingResponder is who answers a pending form.
type PendingResponder interface {
	isPendingResponder()
	acpIdentity() (awayIdentity, bool)
	isAlive() bool
}

// PendingResponderAcp is an ACP request's responder task, waiting on answer.
type PendingResponderAcp struct {
	identity awayIdentity
	answer   *pendingAnswerSlot
}

// PendingResponderCommandLoop is a command-loop question, alive until its
// CommandLoopQuestion is closed (Rust holds a Weak of its lifetime).
type PendingResponderCommandLoop struct {
	lifetime *commandLoopLifetime
}

func (PendingResponderAcp) isPendingResponder()         {}
func (PendingResponderCommandLoop) isPendingResponder() {}

func (r PendingResponderAcp) acpIdentity() (awayIdentity, bool) { return r.identity, true }

func (PendingResponderCommandLoop) acpIdentity() (awayIdentity, bool) { return awayIdentity{}, false }

func (r PendingResponderAcp) isAlive() bool { return !r.answer.isClosed() }

func (r PendingResponderCommandLoop) isAlive() bool { return !r.lifetime.closed.Load() }

// commandLoopLifetime is a command-loop question's lifetime: its identity,
// and whether its owner has closed it (Rust `Arc<()>`).
type commandLoopLifetime struct {
	closed atomic.Bool
}

// CommandLoopQuestion is a recovery choice answered by the command loop, not
// ACP or AFK policy. Its actual local owner keeps this wrapper until
// resolution or cancellation, then closes it (Rust Drop). It reads as its
// request (Rust Deref).
type CommandLoopQuestion struct {
	core.ElicitationRequest
	owner    *OperatorAway
	bridge   uint64
	lifetime *commandLoopLifetime
}

// Close withdraws the question from the owner (Rust Drop).
func (q *CommandLoopQuestion) Close() {
	panic("unported: mj-worker/src/acp/operator_away.rs CommandLoopQuestion::drop")
}

// cancel answers the form Cancel, preserving explicit user intent instead of
// conflating it with a dropped bridge.
func (e *PendingElicitation) cancel() {
	panic("unported: mj-worker/src/acp/operator_away.rs PendingElicitation::cancel")
}

// PendingAnswer is how a pending form was answered.
type PendingAnswer interface {
	isPendingAnswer()
	actionName() string
	// human is the person's answer; nil when the operator-away policy
	// answered.
	human() *core.ElicitationResponse
	harnessResponse() core.ElicitationResponse
}

type PendingAnswerHuman struct{ Value core.ElicitationResponse }

type PendingAnswerOperatorAway struct{}

func (PendingAnswerHuman) isPendingAnswer()        {}
func (PendingAnswerOperatorAway) isPendingAnswer() {}

func (a PendingAnswerHuman) actionName() string { return a.Value.ActionName() }

func (PendingAnswerOperatorAway) actionName() string { return core.OperatorAwayAction }

func (a PendingAnswerHuman) human() *core.ElicitationResponse { return &a.Value }

func (PendingAnswerOperatorAway) human() *core.ElicitationResponse { return nil }

func (a PendingAnswerHuman) harnessResponse() core.ElicitationResponse { return a.Value }

func (PendingAnswerOperatorAway) harnessResponse() core.ElicitationResponse {
	return core.ElicitationResponse{Action: core.ElicitationResponseDecline}
}

// pendingAnswerSlot is one form's answer channel (Rust's oneshot channel of
// a PendingAnswer). The sending side is the presence owner; the receiving
// side is the form's PendingAnswerReceiver.
type pendingAnswerSlot struct {
	mu sync.Mutex
	// answer carries at most one answer. It is closed without one when the
	// sender is dropped unanswered, which answers the request `cancelled`.
	answer chan PendingAnswer
	// done: the sender has answered or been dropped.
	done bool
	// receiverGone is closed once the receiver stops waiting.
	receiverGone chan struct{}
	gone         bool
}

func newPendingAnswerSlot() *pendingAnswerSlot {
	return &pendingAnswerSlot{answer: make(chan PendingAnswer, 1), receiverGone: make(chan struct{})}
}

// send delivers answer; false when the receiver is gone or the slot was
// already answered (Rust Sender::send failing).
func (s *pendingAnswerSlot) send(answer PendingAnswer) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.done || s.gone {
		return false
	}
	s.done = true
	s.answer <- answer
	close(s.answer)
	return true
}

// drop drops the sender: an unanswered form is answered `cancelled`.
func (s *pendingAnswerSlot) drop() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.done {
		s.done = true
		close(s.answer)
	}
}

// isClosed reports whether the receiver has stopped waiting.
func (s *pendingAnswerSlot) isClosed() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.gone
}

// PendingAnswerReceiver is the waiting half of one form's answer (Rust's
// oneshot::Receiver<PendingAnswer>). Close it when the responder stops
// waiting, so the presence owner sees the question is no longer live.
type PendingAnswerReceiver struct {
	slot *pendingAnswerSlot
}

// C yields the answer, or is closed without one when the request was
// dropped, which answers it `cancelled`.
func (r *PendingAnswerReceiver) C() <-chan PendingAnswer { return r.slot.answer }

// Close stops waiting (Rust drops the receiver).
func (r *PendingAnswerReceiver) Close() {
	r.slot.mu.Lock()
	defer r.slot.mu.Unlock()
	if !r.slot.gone {
		r.slot.gone = true
		close(r.slot.receiverGone)
	}
}

// awayPermissionIdentity is the identity of a tool permission request: its
// kind, title and input, with every object's keys sorted. A nil input is no
// input.
func awayPermissionIdentity(request *schema.RequestPermissionRequest, input any) AwayRequest {
	panic("unported: mj-worker/src/acp/operator_away.rs away_permission_identity")
}
