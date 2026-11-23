// Port of mj-worker/src/acp/goal.rs.

// Native goal controls and recovery through the saved-session question
// surface.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"sync"
	"sync/atomic"
	"time"
	"weak"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// goalNextResume numbers native goal resumes, from 1 (Rust NEXT_RESUME).
var goalNextResume atomic.Uint64

// errGoalChannelClosed is tokio's oneshot RecvError, as anyhow shows it.
var errGoalChannelClosed = errors.New("channel closed")

// errGoalDeadlineElapsed is tokio's Elapsed, as anyhow shows it.
var errGoalDeadlineElapsed = errors.New("deadline has elapsed")

// recordResumeSent publishes a native resume to the relay that owns the
// session's cut and returns its id; nil when the launch has no relay owner
// (a nil owner, LaunchSpec.TerminalOwner).
func recordResumeSent(ctx context.Context, owner *weak.Pointer[RelayCell], events chan<- core.RuntimeEvent, goal *core.GoalSnapshot) (*uint64, error) {
	if owner == nil {
		return nil, nil
	}
	if owner.Value() == nil {
		return nil, errors.New("native resume relay owner disappeared")
	}
	resumeID := goalNextResume.Add(1)
	applied, appliedRx := core.NewNativeGoalResumeAckPair()
	// Send through the worker FIFO so every earlier queued observation
	// applies first (cannot prematurely clear this reservation), then await
	// the relay apply acknowledgement before dispatching `_session/goal`.
	if err := emitRuntimeEvent(ctx, events, core.RuntimeEventNativeGoalResumeSent{
		ResumeID: resumeID,
		Goal:     goal,
		Applied:  applied,
	}); err != nil {
		return nil, err
	}
	select {
	case <-appliedRx:
	case <-ctx.Done():
		return nil, fmt.Errorf("native resume relay event loop closed before recording reservation: %w", errGoalChannelClosed)
	}
	return &resumeID, nil
}

// recordResumeRejection releases a resume the adapter provably rejected.
func recordResumeRejection(ctx context.Context, events chan<- core.RuntimeEvent, resumeID *uint64, rejection *acp.RPCError) error {
	// SDK outgoing _session/goal publication and lost response failures use
	// INTERNAL_ERROR (5.0.0 jsonrpc PreparedRequest/block_task). Its local
	// INVALID_REQUEST constructors handle initialize rewriting/incoming
	// batches, not this object-valued goal request; these rejections came
	// from the peer.
	if rejection == nil || resumeID == nil {
		return nil
	}
	switch rejection.Code {
	case rpcCodeInvalidRequest, rpcCodeInvalidParams, rpcCodeMethodNotFound:
		return emitRuntimeEvent(ctx, events, core.RuntimeEventNativeGoalResumeRejected{ResumeID: *resumeID})
	}
	return nil
}

// goalRPCError is the ACP error a request failed with; nil when it failed
// without the peer's answer.
func goalRPCError(err error) *acp.RPCError {
	var rpc *acp.RPCError
	if errors.As(err, &rpc) {
		return rpc
	}
	return nil
}

// PendingControl is one goal control request in flight.
type PendingControl struct {
	// sent is closed once the request has been written, or once the control
	// ended without sending one and its outcome was taken; a later control
	// does not start before it.
	sent     chan struct{}
	markSent func()
}

// PendingControls are the goal controls in flight, polled by both command
// loops. Ending the session ends all request waits; the relay reports their
// commands interrupted rather than replaying them. The zero value is empty
// (Rust Default).
type PendingControls struct {
	mu       sync.Mutex
	controls []*PendingControl
	// ready carries each finished control's outcome to the command loop. A
	// control stays in controls until its outcome has been taken, so
	// isEmpty never hides an outcome still to be delivered.
	ready chan core.RuntimeEvent
}

func (c *PendingControls) isEmpty() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return len(c.controls) == 0
}

// readyChannel is the outcome channel, created on first use. The caller
// holds c.mu.
func (c *PendingControls) readyChannel() chan core.RuntimeEvent {
	if c.ready == nil {
		c.ready = make(chan core.RuntimeEvent)
	}
	return c.ready
}

// forget removes a control whose outcome was taken or abandoned.
func (c *PendingControls) forget(control *PendingControl) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.controls = slices.DeleteFunc(c.controls, func(other *PendingControl) bool { return other == control })
}

// start sends one goal control. Explicit native resume may answer only when
// its turn finishes, so no acknowledgement timeout applies. ctx ends the
// request wait with the session.
func (c *PendingControls) start(
	ctx context.Context,
	spec *LaunchSpec,
	events chan<- core.RuntimeEvent,
	connection *acp.Connection,
	session schema.SessionId,
	requestID string,
	action core.GoalControlAction,
	expected *core.GoalSnapshot,
) {
	owner := spec.TerminalOwner
	recovery, unlock := spec.GoalRecovery.Lock()
	resumeGoal := recovery.State.Snapshot
	unlock()
	control := &PendingControl{sent: make(chan struct{})}
	control.markSent = sync.OnceFunc(func() { close(control.sent) })
	c.mu.Lock()
	// Keep requests ordered until each is sent: a later control does not
	// start before the one ahead of it has sent its request.
	var previous chan struct{}
	if len(c.controls) > 0 {
		previous = c.controls[len(c.controls)-1].sent
	}
	c.controls = append(c.controls, control)
	ready := c.readyChannel()
	c.mu.Unlock()
	go func() {
		defer c.forget(control)
		if previous != nil {
			select {
			case <-previous:
			case <-ctx.Done():
				control.markSent()
				return
			}
		}
		event := c.run(ctx, events, connection, session, requestID, action, expected, owner, resumeGoal, control)
		select {
		case ready <- event:
		case <-ctx.Done():
		}
		// A control that ended before sending keeps later ones waiting
		// until its outcome was taken.
		control.markSent()
	}()
}

// run is one control's work: its resume admission, its request, and the
// outcome to report.
func (c *PendingControls) run(
	ctx context.Context,
	events chan<- core.RuntimeEvent,
	connection *acp.Connection,
	session schema.SessionId,
	requestID string,
	action core.GoalControlAction,
	expected *core.GoalSnapshot,
	owner *weak.Pointer[RelayCell],
	resumeGoal *core.GoalSnapshot,
	control *PendingControl,
) core.RuntimeEvent {
	// Explicit native resume may answer only when its turn finishes. Do not
	// impose recovery's short acknowledgement timeout here.
	var resumeID *uint64
	var failure error
	if action == core.GoalControlActionResume {
		resumeID, failure = recordResumeSent(ctx, owner, events, resumeGoal)
	}
	if failure == nil {
		method, params := controlRequest(session, action, expected)
		control.markSent()
		var result json.RawMessage
		if err := connection.Call(ctx, method, params, &result); err != nil {
			display := acpErrorDisplay(err)
			if recording := recordResumeRejection(ctx, events, resumeID, goalRPCError(err)); recording != nil {
				failure = fmt.Errorf("goal request rejected: %s: %w", display, recording)
			} else {
				failure = errors.New(display)
			}
		}
	}
	if failure != nil {
		return core.RuntimeEventCommandRejected{
			Reason:    core.OutcomeReasonCommandFailed,
			RequestID: requestID,
			Message:   fmt.Sprintf("/goal %s failed: %s", action.AsStr(), failure),
		}
	}
	return core.RuntimeEventGoalControlApplied{RequestID: requestID}
}

// next yields the next finished control's event, keeping requests ordered
// until each has been sent: a full event FIFO must not let a later control
// overtake the resume whose ordered marker is still being enqueued. The
// channel never yields while no control is in flight.
func (c *PendingControls) next() <-chan core.RuntimeEvent {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.readyChannel()
}

// controlIdentity is the identity an automatic quota goal resume sends, so
// the adapter restarts only the goal the usage limit stopped and never one
// whose budget is spent. A user's own `/goal resume` carries none (nil).
func controlIdentity(spec *LaunchSpec, requestID string) *core.GoalSnapshot {
	if !core.IsQuotaGoalResume(requestID) {
		return nil
	}
	recovery, unlock := spec.GoalRecovery.Lock()
	defer unlock()
	if recovery.State.Snapshot == nil {
		return nil
	}
	snapshot := *recovery.State.Snapshot
	return &snapshot
}

// controlRequest is the `_session/goal` request: its method and params.
func controlRequest(session schema.SessionId, action core.GoalControlAction, expected *core.GoalSnapshot) (string, jsonvalue.Object) {
	params := jsonvalue.Object{
		{Key: "sessionId", Value: string(session)},
		{Key: "action", Value: action.AsStr()},
	}
	if expected != nil {
		var createdAt any
		if expected.CreatedAt != nil {
			createdAt = *expected.CreatedAt
		}
		params.Set("expectedGoal", jsonvalue.Object{
			{Key: "objective", Value: expected.Objective},
			{Key: "createdAt", Value: createdAt},
		})
	}
	return "_session/goal", params
}

// goalInfoUpdate is the session_info_update that carries meta.
func goalInfoUpdate(meta any) (schema.SessionUpdate, error) {
	encoded, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "sessionUpdate", Value: "session_info_update"},
		{Key: "_meta", Value: meta},
	})
	if err != nil {
		return schema.SessionUpdate{}, err
	}
	var update schema.SessionUpdate
	if err := json.Unmarshal(encoded, &update); err != nil {
		return schema.SessionUpdate{}, err
	}
	return update, nil
}

// publish applies a goal metadata update to the session's goal state and
// publishes it as a session update.
func publish(ctx context.Context, spec *LaunchSpec, events chan<- core.RuntimeEvent, meta any) error {
	update, err := goalInfoUpdate(meta)
	if err != nil {
		return err
	}
	recovery, unlock := spec.GoalRecovery.Lock()
	_, err = recovery.State.Apply(&update)
	unlock()
	if err != nil {
		return err
	}
	encoded, err := json.Marshal(update)
	if err != nil {
		return err
	}
	return emitRuntimeEvent(ctx, events, core.RuntimeEventSessionUpdate{Update: encoded})
}

// decisionUpdate persists a goal decision update through the goal journal,
// or publishes it when the launch has none.
func decisionUpdate(ctx context.Context, spec *LaunchSpec, events chan<- core.RuntimeEvent, meta any) error {
	recovery, unlock := spec.GoalRecovery.Lock()
	journal := recovery.Journal
	unlock()
	if journal == nil {
		return publish(ctx, spec, events, meta)
	}
	update, err := goalInfoUpdate(meta)
	if err != nil {
		return err
	}
	persisted, err := goalInfoUpdate(meta)
	if err != nil {
		return err
	}
	if err := journal(persisted); err != nil {
		return err
	}
	recovery, unlock = spec.GoalRecovery.Lock()
	defer unlock()
	_, err = recovery.State.Apply(&update)
	return err
}

// goalRecoveryContext is a copy of the launch's goal recovery context (Rust
// clones it out of its lock). GoalState.Apply replaces what it changes, so a
// shallow copy is a snapshot.
func goalRecoveryContext(spec *LaunchSpec) core.GoalRecoveryContext {
	recovery, unlock := spec.GoalRecovery.Lock()
	defer unlock()
	return *recovery
}

// prepareControl lets explicit controls supersede any saved restart answer
// before native dispatch. False when the control was refused, which it has
// already reported.
func prepareControl(
	ctx context.Context,
	spec *LaunchSpec,
	events chan<- core.RuntimeEvent,
	pending **Question,
	modelPending bool,
	requestID string,
	action core.GoalControlAction,
) (bool, error) {
	recovery := goalRecoveryContext(spec)
	var refusal string
	switch {
	case !recovery.State.Supports(action):
		refusal = fmt.Sprintf("/goal %s is not supported by this adapter", action.AsStr())
	case core.IsQuotaGoalResume(requestID) && !recovery.State.ResumableAfterQuota():
		refusal = "the goal is no longer stopped by the usage limit"
	case modelPending && action == core.GoalControlActionResume:
		refusal = "Choose the replacement model before resuming the goal"
	}
	if refusal != "" {
		if err := emitRuntimeEvent(ctx, events, core.RuntimeEventCommandRejected{
			Reason:    core.OutcomeReasonAdmissionRejected,
			RequestID: requestID,
			Message:   refusal,
		}); err != nil {
			return false, err
		}
		return false, nil
	}
	if *pending != nil || recovery.State.Decision != nil || recovery.Asking() {
		meta := jsonvalue.Object{{Key: "mjGoalDecision", Value: nil}}
		if intent, ok := recovery.RequestID(); ok {
			meta.Set("mjGoalResumeAnswered", intent)
		}
		if err := decisionUpdate(ctx, spec, events, meta); err != nil {
			if err := emitRuntimeEvent(ctx, events, core.RuntimeEventCommandRejected{
				Reason:    core.OutcomeReasonCommandFailed,
				RequestID: requestID,
				Message:   fmt.Sprintf("Could not save goal control intent: %s", err),
			}); err != nil {
				return false, err
			}
			return false, nil
		}
		if question := *pending; question != nil {
			question.request.Close()
			*pending = nil
			if err := emitRuntimeEvent(ctx, events, core.RuntimeEventElicitationResolved{
				ElicitationID: core.RecoveryID,
				Action:        "cancel",
			}); err != nil {
				return false, err
			}
		}
	}
	return true, nil
}

// Question is the open goal recovery question.
type Question struct {
	request *CommandLoopQuestion
	goal    core.GoalSnapshot
	intent  *string
}

// Close withdraws the question (Rust Drop of its CommandLoopQuestion).
func (q *Question) Close() {
	q.request.Close()
}

// control pauses or resumes goal through the adapter.
func control(
	ctx context.Context,
	spec *LaunchSpec,
	events chan<- core.RuntimeEvent,
	connection *acp.Connection,
	session schema.SessionId,
	goal *core.GoalSnapshot,
	resume bool,
) error {
	if goal.ControlMethod == nil || *goal.ControlMethod != "_session/goal" {
		return errors.New("goal resume is not supported by this harness")
	}
	if goal.CreatedAt == nil {
		return errors.New("goal identity is unavailable")
	}
	action := core.GoalControlActionPause
	if resume {
		action = core.GoalControlActionResume
	}
	var resumeID *uint64
	if resume {
		copied := *goal
		var err error
		if resumeID, err = recordResumeSent(ctx, spec.TerminalOwner, events, &copied); err != nil {
			return err
		}
	}
	// Timeout and SDK internal errors cannot prove rejection.
	deadline, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	method, params := controlRequest(session, action, goal)
	var result json.RawMessage
	err := connection.Call(deadline, method, params, &result)
	if err != nil && ctx.Err() == nil && errors.Is(deadline.Err(), context.DeadlineExceeded) {
		return fmt.Errorf("goal resume acknowledgement timed out; current execution must be reconciled before retrying: %w",
			errGoalDeadlineElapsed)
	}
	if err != nil {
		if err := recordResumeRejection(ctx, events, resumeID, goalRPCError(err)); err != nil {
			return err
		}
		return sessionConfigCallError(err)
	}
	return nil
}

// goalRecoverable reports whether snapshot is still the goal a question or
// decision was about, active or paused.
func goalRecoverable(snapshot *core.GoalSnapshot, goal *core.GoalSnapshot) bool {
	return snapshot != nil && snapshot.SameGoal(goal) && (snapshot.Status == "active" || snapshot.Status == "paused")
}

// goalRecover resumes or asks about a restored session's goal; the open
// question when it asks. (Rust goal::recover; renamed because recover is a
// Go builtin.)
func goalRecover(
	ctx context.Context,
	recoverySupported bool,
	connection *acp.Connection,
	session schema.SessionId,
	spec *LaunchSpec,
	events chan<- core.RuntimeEvent,
	pendingElicitations PendingElicitations,
	modelQuestion bool,
) (*Question, error) {
	recovery := goalRecoveryContext(spec)
	if !recoverySupported || spec.ResumeSession == nil {
		return nil, nil
	}
	// A crash can leave the prior question in the durable projection after
	// its decision was saved. Rebuild this session-owned question from
	// intent.
	if err := emitRuntimeEvent(ctx, events, core.RuntimeEventElicitationResolved{
		ElicitationID: core.RecoveryID,
		Action:        "cancel",
	}); err != nil {
		return nil, err
	}
	if !recovery.State.Synchronized() {
		if err := emitRuntimeEvent(ctx, events, core.RuntimeEventWarning{
			Message: "The adapter did not synchronize goal execution state; automatic goal recovery and worker replacement are deferred",
		}); err != nil {
			return nil, err
		}
		return nil, nil
	}
	clearDecision := jsonvalue.Object{{Key: "mjGoalDecision", Value: nil}}
	if decision := recovery.State.Decision; decision != nil && !recovery.Asking() && !modelQuestion {
		if goalRecoverable(recovery.State.Snapshot, &decision.Goal) {
			if err := control(ctx, spec, events, connection, session, &decision.Goal, decision.Resume); err != nil {
				return nil, err
			}
		}
		if err := decisionUpdate(ctx, spec, events, clearDecision); err != nil {
			return nil, err
		}
		return nil, nil
	}
	if recovery.State.Decision != nil {
		// A new explicit open or model choice supersedes an older answer.
		if err := decisionUpdate(ctx, spec, events, clearDecision); err != nil {
			return nil, err
		}
	}
	snapshot := recovery.State.Snapshot
	if snapshot == nil || (snapshot.Status != "active" && snapshot.Status != "paused") {
		if id, ok := recovery.RequestID(); recovery.Asking() && ok {
			if err := publish(ctx, spec, events, jsonvalue.Object{{Key: "mjGoalResumeAnswered", Value: id}}); err != nil {
				return nil, err
			}
		}
		return nil, nil
	}
	goal := *snapshot
	if modelQuestion && goal.Active() {
		if err := control(ctx, spec, events, connection, session, &goal, false); err != nil {
			return nil, err
		}
	}
	var explanation *string
	if !recovery.Asking() && !modelQuestion {
		if !goal.Active() || recovery.State.Running() {
			return nil, nil
		}
		err := control(ctx, spec, events, connection, session, &goal, true)
		if err == nil {
			return nil, nil
		}
		message := fmt.Sprintf("Goal recovery needs attention: %s", err)
		if err := emitRuntimeEvent(ctx, events, core.RuntimeEventWarning{Message: message}); err != nil {
			return nil, err
		}
		explanation = &message
	}
	title := "Resume this goal?"
	description := "Resume the existing goal with its remaining budget, or keep it paused."
	if explanation != nil {
		description = *explanation
	}
	preselected := "resume"
	request := pendingElicitations.registerCommandLoop(core.ElicitationRequest{
		ID:          core.RecoveryID,
		Title:       &title,
		Message:     goal.Objective,
		Description: &description,
		Fields: []core.ElicitationField{{
			ID:       "action",
			Title:    "Goal",
			Required: true,
			Kind: core.ElicitationFieldKindSingleSelect{
				Options: []core.ElicitationOption{
					{Value: "resume", Title: "Resume goal"},
					{Value: "pause", Title: "Keep paused"},
				},
				Default: &preselected,
			},
		}},
	})
	if err := emitRuntimeEvent(ctx, events, core.RuntimeEventElicitationRequested{Request: request.ElicitationRequest}); err != nil {
		request.Close()
		return nil, err
	}
	var intent *string
	if id, ok := recovery.RequestID(); ok {
		intent = &id
	}
	return &Question{request: request, goal: goal, intent: intent}, nil
}

// resolve answers the goal recovery question.
//
// handled is false when the id is not this question's. Otherwise answer is
// what the caller reports back to whoever submitted the form: nil when the
// answer was taken, or the refusal (Rust `Option<Result<(), String>>`).
func resolve(
	ctx context.Context,
	connection *acp.Connection,
	session schema.SessionId,
	spec *LaunchSpec,
	events chan<- core.RuntimeEvent,
	pending **Question,
	modelPending bool,
	id string,
	response *core.ElicitationResponse,
) (answer error, handled bool, err error) {
	question := *pending
	if question == nil || question.request.ID != id {
		return nil, false, nil
	}
	if err := question.request.ValidateResponse(response); err != nil {
		return err, true, nil
	}
	chosen, _ := response.Content["action"].(core.ElicitationValueString)
	resume := response.Action == core.ElicitationResponseAccept && chosen == "resume"
	if resume && modelPending {
		return errors.New("Choose the replacement model before resuming the goal"), true, nil
	}
	recovery := goalRecoveryContext(spec)
	current := goalRecoverable(recovery.State.Snapshot, &question.goal)
	var decision any
	if current {
		value, err := acpFeaturesValue(core.GoalDecision{Goal: question.goal, Resume: resume})
		if err != nil {
			return nil, true, err
		}
		decision = value
	}
	meta := jsonvalue.Object{{Key: "mjGoalDecision", Value: decision}}
	if question.intent != nil {
		meta.Set("mjGoalResumeAnswered", *question.intent)
	}
	if err := decisionUpdate(ctx, spec, events, meta); err != nil {
		return nil, true, err
	}
	if current {
		if err := control(ctx, spec, events, connection, session, &question.goal, resume); err != nil {
			return err, true, nil
		}
		if err := decisionUpdate(ctx, spec, events, jsonvalue.Object{{Key: "mjGoalDecision", Value: nil}}); err != nil {
			return nil, true, err
		}
	}
	reply := question.request.ReplyText(response)
	question.Close()
	*pending = nil
	if err := emitRuntimeEvent(ctx, events, core.RuntimeEventElicitationResolved{
		ElicitationID: id,
		Action:        response.ActionName(),
		Reply:         &reply,
	}); err != nil {
		return nil, true, err
	}
	return nil, true, nil
}
