// Port of mj-worker/src/acp/operator_away.rs.

// Operator availability: questions the operator-away policy answers, the
// refusals it remembers, and the tool permission requests a turn withdrawal
// reaches.

package worker

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
	// A turn withdrawal marks the turn before sending session/cancel and
	// removes its forms after. AFK must not slip an answer through that gap.
	// Leave the form for the withdrawal to remove.
	if _, withdrawn := s.withdrawnTurns[entry.bridge]; withdrawn && strings.HasPrefix(entry.question.ID, core.ToolPermissionIDPrefix) {
		s.pending = append(s.pending, entry)
		return
	}
	responder, ok := entry.responder.(PendingResponderAcp)
	if !ok {
		// Command-loop recovery choices are never answered by AFK policy.
		s.pending = append(s.pending, entry)
		return
	}
	var evidence any
	_, isQuestion := responder.identity.request.(AwayRequestQuestion)
	if isQuestion {
		evidence = questionEvidence(&entry.question)
	}
	if responder.answer.send(PendingAnswerOperatorAway{}) {
		history := s.refused[responder.identity]
		if isQuestion {
			history = append(history, evidence)
			s.refused[responder.identity] = history
			s.changed.SendReplace(struct{}{})
		} else {
			s.refused[responder.identity] = history
		}
	}
}

// questionEvidence retains answer constraints, excluding transient identity,
// secret defaults and the required flag. The user ruled (2026-11-13) that a
// form differing only by required versus optional repeats the earlier
// question, so the flag never reaches the classifier and cannot make two
// forms differ.
func questionEvidence(question *core.ElicitationRequest) any {
	value, err := acpFeaturesValue(question)
	if err != nil {
		panic("elicitation serializes: " + err.Error())
	}
	evidence, ok := value.(jsonvalue.Object)
	if !ok {
		panic("elicitation is an object")
	}
	evidence.SwapDelete("id")
	fieldsValue, _ := evidence.Get("fields")
	fields, ok := fieldsValue.([]any)
	if !ok {
		panic("elicitation fields are an array")
	}
	for index := range fields {
		if index >= len(question.Fields) {
			break
		}
		field, ok := fields[index].(jsonvalue.Object)
		if !ok {
			panic("elicitation field is an object")
		}
		field.SwapDelete("required")
		if question.FieldIsSecret(&question.Fields[index]) {
			field.SwapDelete("default")
			field.Set("secret", true)
		}
		fields[index] = field
	}
	evidence.Set("fields", fields)
	return evidence
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

// keepFixtureQuestionLive registers question on a bridge of its own and
// returns what keeps it live: the bridge and its answer receiver (Rust
// `Box<dyn Send>`). The question stays live while the caller holds it.
func (o *OperatorAway) keepFixtureQuestionLive(question core.ElicitationRequest) any {
	bridge := o.pendingForBridgeWithSource(VerdictSourceDisabled{}, nil)
	answer := bridge.register(question, "", AwayRequestQuestion{})
	return []any{bridge, answer}
}

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
	state := o.lockState()
	defer o.mu.Unlock()
	if presence.Revision > state.presence.Revision {
		state.presence = presence
		clear(state.refused)
		if presence.Away {
			// Slice order is registration order, including across bridge
			// scopes.
			pending := state.pending
			state.pending = nil
			for _, entry := range pending {
				identity, ok := entry.responder.acpIdentity()
				if _, refused := state.refused[identity]; ok && !refused {
					state.answerAway(entry)
				} else {
					state.pending = append(state.pending, entry)
				}
			}
		}
		state.changed.SendReplace(struct{}{})
	}
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
	state := b.owner.lockState()
	defer b.owner.mu.Unlock()
	delete(state.bridges, b.id)
	state.dropPending(func(entry *PendingElicitation) bool { return entry.bridge == b.id })
	state.forgetReviews(b.id)
}

// forgetReviews withdraws a bridge's reviews in flight and forgets its turn.
func (s *OperatorAwayState) forgetReviews(bridge uint64) {
	s.reviews = slices.DeleteFunc(s.reviews, func(review PendingReview) bool {
		if review.bridge == bridge {
			review.withdraw()
			return true
		}
		return false
	})
	delete(s.withdrawnTurns, bridge)
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
	state := t.owner.lockState()
	_, turnWithdrawn := state.withdrawnTurns[t.bridge]
	withdrawn := t.withdrawnCtx.Err() != nil || turnWithdrawn ||
		!slices.ContainsFunc(state.reviews, func(review PendingReview) bool { return review.id == t.id })
	answer(withdrawn)
	t.owner.mu.Unlock()
	t.Close()
}

// Close unregisters the review (Rust Drop).
func (t *PendingReviewTicket) Close() {
	state := t.owner.lockState()
	defer t.owner.mu.Unlock()
	state.reviews = slices.DeleteFunc(state.reviews, func(review PendingReview) bool { return review.id == t.id })
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
	lifetime := &commandLoopLifetime{}
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	if _, live := state.bridges[p.bridge.id]; live {
		state.pending = append(state.pending, &PendingElicitation{
			bridge:    p.bridge.id,
			question:  request,
			responder: PendingResponderCommandLoop{lifetime: lifetime},
		})
	}
	return &CommandLoopQuestion{
		ElicitationRequest: request,
		owner:              p.bridge.owner,
		bridge:             p.bridge.id,
		lifetime:           lifetime,
	}
}

// register registers a form. Both immediate and later AFK decisions reach
// the same responder task. It publishes Requested before receiving this
// answer, so AFK cannot resolve a form before the relay has observed the
// question.
func (p PendingElicitations) register(question core.ElicitationRequest, agent string, request AwayRequest) *PendingAnswerReceiver {
	answer := newPendingAnswerSlot()
	received := &PendingAnswerReceiver{slot: answer}
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	_, live := state.bridges[p.bridge.id]
	_, withdrawn := state.withdrawnTurns[p.bridge.id]
	if !live || (withdrawn && strings.HasPrefix(question.ID, core.ToolPermissionIDPrefix)) {
		// Dropping the answer channel answers the request `cancelled`.
		answer.drop()
		return received
	}
	identity := awayIdentity{agent: agent, request: request}
	entry := &PendingElicitation{
		bridge:    p.bridge.id,
		question:  question,
		responder: PendingResponderAcp{identity: identity, answer: answer},
	}
	if _, refused := state.refused[identity]; state.presence.Away && !refused {
		state.answerAway(entry)
	} else {
		state.pending = append(state.pending, entry)
	}
	return received
}

// refineQuestion asks the classifier whether question id repeats one already
// declined while away. The responder owns this call: human answers and
// cancellation end ctx, which drops its HTTP work; only the presence owner
// may accept the resulting answer.
func (p PendingElicitations) refineQuestion(ctx context.Context, id string) {
	owner := p.bridge.owner
	state := owner.lockState()
	changed := state.changed
	owner.mu.Unlock()
	var client *VerdictClient
	for {
		// Mark the current value seen: a change after this point wakes the
		// waits below.
		changes := changed.Changed()
		type afkSnapshot struct {
			revision uint64
			agent    string
			question any
			history  []any
		}
		var snapshot *afkSnapshot
		state := owner.lockState()
		index := slices.IndexFunc(state.pending, func(entry *PendingElicitation) bool {
			return entry.bridge == p.bridge.id && entry.question.ID == id
		})
		if index < 0 {
			owner.mu.Unlock()
			return
		}
		entry := state.pending[index]
		identity, ok := entry.responder.acpIdentity()
		if !ok {
			owner.mu.Unlock()
			return
		}
		if _, isQuestion := identity.request.(AwayRequestQuestion); !isQuestion {
			owner.mu.Unlock()
			return
		}
		if state.presence.Away {
			snapshot = &afkSnapshot{
				revision: state.presence.Revision,
				agent:    identity.agent,
				question: questionEvidence(&entry.question),
				history:  slices.Clone(state.refused[identity]),
			}
		}
		owner.mu.Unlock()
		if snapshot != nil {
			type classified struct {
				client *VerdictClient
				same   bool
				err    error
			}
			classifyCtx, cancelClassify := context.WithCancel(ctx)
			result := make(chan classified, 1)
			// Dropping the classification (Rust drops its future) ends it
			// before this returns: cancelled mid-request it records its
			// cancellation, and past the request it records its outcome.
			drop := func() {
				cancelClassify()
				<-result
			}
			go func(client *VerdictClient) {
				if client == nil {
					client = verdictClientResolve(classifyCtx, p.bridge.source)
				}
				// Read at classification time: the log opens after the bridge
				// starts.
				session := ""
				var log *core.DecisionLog
				if turnContext := p.bridge.turnContext; turnContext != nil {
					session = turnContext.SessionID()
					if decisions, ok := turnContext.DecisionLog(); ok {
						log = &decisions
					}
				}
				if client == nil {
					result <- classified{err: errors.New("the classifier is disabled or unavailable")}
					return
				}
				same, err := client.withLog(log).sameAfkQuestion(classifyCtx, session, snapshot.question, snapshot.history)
				result <- classified{client: client, same: same, err: err}
			}(client)
			var verdict classified
			select {
			case verdict = <-result:
				cancelClassify()
			case <-changes:
				drop()
				continue
			case <-ctx.Done():
				drop()
				return
			}
			if verdict.client != nil {
				client = verdict.client
			}
			switch {
			case verdict.err == nil && !verdict.same:
				state := owner.lockState()
				// Compare and consume under the same owner lock. Concurrent new
				// questions must reclassify against the accepted history.
				current, known := state.refused[awayIdentity{agent: snapshot.agent, request: AwayRequestQuestion{}}]
				if state.presence.Away && state.presence.Revision == snapshot.revision && known &&
					operatorAwayHistoryEqual(current, snapshot.history) {
					index := slices.IndexFunc(state.pending, func(entry *PendingElicitation) bool {
						_, acp := entry.responder.acpIdentity()
						return entry.bridge == p.bridge.id && entry.question.ID == id && acp
					})
					if index >= 0 {
						entry := state.pending[index]
						state.pending = slices.Delete(state.pending, index, index+1)
						state.answerAway(entry)
						owner.mu.Unlock()
						slog.Info("Jev identified a new AFK question", "target", "mj_jev", "id", id, "revision", snapshot.revision)
						return
					}
				}
				owner.mu.Unlock()
				continue
			case verdict.err == nil:
				slog.Info("Jev identified a repeated AFK question", "target", "mj_jev", "id", id, "revision", snapshot.revision)
			default:
				slog.Warn("AFK question classification unavailable; retaining existing behavior",
					"target", "mj_jev", "id", id, "error", verdict.err.Error())
			}
		}
		select {
		case <-changes:
		case <-ctx.Done():
			return
		}
	}
}

func (p PendingElicitations) remove(id string) *PendingElicitation {
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	index := slices.IndexFunc(state.pending, func(entry *PendingElicitation) bool {
		_, acp := entry.responder.acpIdentity()
		return entry.bridge == p.bridge.id && entry.question.ID == id && acp
	})
	if index < 0 {
		return nil
	}
	entry := state.pending[index]
	state.pending = slices.Delete(state.pending, index, index+1)
	return entry
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
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	_, live := state.bridges[p.bridge.id]
	_, withdrawn := state.withdrawnTurns[p.bridge.id]
	if !live || withdrawn {
		return nil
	}
	id := state.nextReview
	if id+1 < id {
		panic("guardian review identity exhausted")
	}
	state.nextReview = id + 1
	withdrawnCtx, withdraw := context.WithCancel(context.Background())
	state.reviews = append(state.reviews, PendingReview{
		bridge:    p.bridge.id,
		id:        id,
		withdrawn: withdrawnCtx,
		withdraw:  withdraw,
	})
	return &PendingReviewTicket{
		owner:        p.bridge.owner,
		bridge:       p.bridge.id,
		id:           id,
		withdrawnCtx: withdrawnCtx,
	}
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
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	state.dropPending(func(entry *PendingElicitation) bool {
		_, acp := entry.responder.acpIdentity()
		return entry.bridge == p.bridge.id && acp && strings.HasPrefix(entry.question.ID, core.ToolPermissionIDPrefix)
	})
	state.reviews = slices.DeleteFunc(state.reviews, func(review PendingReview) bool {
		if review.bridge == p.bridge.id {
			review.withdraw()
			return true
		}
		return false
	})
	state.withdrawnTurns[p.bridge.id] = struct{}{}
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
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	return slices.ContainsFunc(state.reviews, func(review PendingReview) bool { return review.bridge == p.bridge.id }) &&
		!state.hasAcpEntry(p.bridge.id)
}

func (p PendingElicitations) isEmpty() bool {
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	return !state.hasAcpEntry(p.bridge.id) &&
		!slices.ContainsFunc(state.reviews, func(review PendingReview) bool { return review.bridge == p.bridge.id })
}

func (p PendingElicitations) close() {
	p.bridge.Close()
}

// resolve gives the person's answer to form id; the error is the refusal to
// report to whoever answered (Rust `Result<(), String>`).
func (p PendingElicitations) resolve(id string, response core.ElicitationResponse) error {
	state := p.bridge.owner.lockState()
	defer p.bridge.owner.mu.Unlock()
	index := slices.IndexFunc(state.pending, func(entry *PendingElicitation) bool {
		_, acp := entry.responder.acpIdentity()
		return entry.bridge == p.bridge.id && entry.question.ID == id && acp
	})
	if index < 0 {
		return fmt.Errorf("elicitation %s is no longer pending", sessionConfigDebugString(id))
	}
	answer, err := state.pending[index].question.HarnessResponse(response)
	if err != nil {
		return err
	}
	entry := state.pending[index]
	state.pending = slices.Delete(state.pending, index, index+1)
	switch responder := entry.responder.(type) {
	case PendingResponderAcp:
		if !responder.answer.send(PendingAnswerHuman{Value: answer}) {
			return fmt.Errorf("elicitation %s was cancelled before it was answered", sessionConfigDebugString(id))
		}
		return nil
	default:
		return fmt.Errorf("elicitation %s belongs to the command loop", sessionConfigDebugString(id))
	}
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
	q.lifetime.closed.Store(true)
	state := q.owner.lockState()
	defer q.owner.mu.Unlock()
	state.pending = slices.DeleteFunc(state.pending, func(entry *PendingElicitation) bool {
		responder, ok := entry.responder.(PendingResponderCommandLoop)
		return entry.bridge == q.bridge && ok && responder.lifetime == q.lifetime
	})
}

// cancel answers the form Cancel, preserving explicit user intent instead of
// conflating it with a dropped bridge.
func (e *PendingElicitation) cancel() {
	if responder, ok := e.responder.(PendingResponderAcp); ok {
		if !responder.answer.send(PendingAnswerHuman{Value: core.ElicitationResponse{Action: core.ElicitationResponseCancel}}) {
			slog.Debug("permission responder closed before cancellation was answered", "elicitation_id", e.question.ID)
		}
	}
	e.Close()
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
	var kind, title any
	if request.ToolCall.Kind != nil {
		kind = string(*request.ToolCall.Kind)
	}
	if request.ToolCall.Title != nil {
		title = *request.ToolCall.Title
	}
	// ACP metadata may enable serde_json's preserve_order feature elsewhere.
	identity := operatorAwaySortObjects(jsonvalue.Object{
		{Key: "kind", Value: kind},
		{Key: "title", Value: title},
		{Key: "input", Value: input},
	})
	encoded, err := jsonvalue.Marshal(identity)
	if err != nil {
		panic("tool permission identity serializes: " + err.Error())
	}
	return AwayRequestTool{Value: string(encoded)}
}

// Close drops the form's answer channel (Rust Drop of a PendingElicitation):
// an ACP responder still waiting answers its request `cancelled`. A form
// already answered is unaffected.
func (e *PendingElicitation) Close() {
	if responder, ok := e.responder.(PendingResponderAcp); ok {
		responder.answer.drop()
	}
}

// dropPending removes the entries matches selects, dropping each: Rust's
// `pending.retain` drops what it removes. The caller holds the owner's lock.
func (s *OperatorAwayState) dropPending(matches func(entry *PendingElicitation) bool) {
	s.pending = slices.DeleteFunc(s.pending, func(entry *PendingElicitation) bool {
		if matches(entry) {
			entry.Close()
			return true
		}
		return false
	})
}

// hasAcpEntry reports whether bridge has an ACP form outstanding. The caller
// holds the owner's lock.
func (s *OperatorAwayState) hasAcpEntry(bridge uint64) bool {
	return slices.ContainsFunc(s.pending, func(entry *PendingElicitation) bool {
		_, acp := entry.responder.acpIdentity()
		return entry.bridge == bridge && acp
	})
}

// operatorAwayHistoryEqual is `Vec<serde_json::Value>` equality.
func operatorAwayHistoryEqual(a, b []any) bool {
	return slices.EqualFunc(a, b, acpFeaturesJSONEqual)
}

// operatorAwaySortObjects is serde_json's Value::sort_all_objects: every
// object's members in key order, at any depth.
func operatorAwaySortObjects(value any) any {
	switch typed := value.(type) {
	case jsonvalue.Object:
		keys := slices.Sorted(slices.Values(typed.Keys()))
		sorted := make(jsonvalue.Object, 0, len(keys))
		for _, key := range keys {
			member, _ := typed.Get(key)
			sorted = append(sorted, jsonvalue.Member{Key: key, Value: operatorAwaySortObjects(member)})
		}
		return sorted
	case map[string]any:
		return operatorAwaySortObjects(jsonvalue.ObjectOf(typed))
	case []any:
		items := make([]any, len(typed))
		for index, item := range typed {
			items[index] = operatorAwaySortObjects(item)
		}
		return items
	}
	return value
}
