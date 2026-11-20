// Port of mj-worker/src/acp/goal.rs.

// Native goal controls and recovery through the saved-session question
// surface.

package worker

import (
	"context"
	"sync"
	"sync/atomic"
	"weak"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// goalNextResume numbers native goal resumes, from 1 (Rust NEXT_RESUME).
var goalNextResume atomic.Uint64

// recordResumeSent publishes a native resume to the relay that owns the
// session's cut and returns its id; nil when the launch has no relay owner
// (a nil owner, LaunchSpec.TerminalOwner).
func recordResumeSent(ctx context.Context, owner *weak.Pointer[RelayCell], events chan<- core.RuntimeEvent, goal *core.GoalSnapshot) (*uint64, error) {
	panic("unported: mj-worker/src/acp/goal.rs record_resume_sent")
}

// recordResumeRejection releases a resume the adapter provably rejected.
func recordResumeRejection(ctx context.Context, events chan<- core.RuntimeEvent, resumeID *uint64, rejection *acp.RPCError) error {
	panic("unported: mj-worker/src/acp/goal.rs record_resume_rejection")
}

// PendingControl is one goal control request in flight (Rust a pinned
// future of its RuntimeEvent).
type PendingControl struct {
	// sent is closed once the request has been written; a later control does
	// not start before it.
	sent chan struct{}
	// event delivers the control's outcome once the adapter answers.
	event chan core.RuntimeEvent
}

// PendingControls are the goal controls in flight, polled by both command
// loops. Ending the session ends all request waits; the relay reports their
// commands interrupted rather than replaying them. The zero value is empty
// (Rust Default).
type PendingControls struct {
	mu       sync.Mutex
	controls []PendingControl
}

func (c *PendingControls) isEmpty() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return len(c.controls) == 0
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
	panic("unported: mj-worker/src/acp/goal.rs PendingControls::start")
}

// next yields the next finished control's event, keeping requests ordered
// until each has been sent: a full event FIFO must not let a later control
// overtake the resume whose ordered marker is still being enqueued. The
// channel never yields while no control is in flight.
func (c *PendingControls) next() <-chan core.RuntimeEvent {
	panic("unported: mj-worker/src/acp/goal.rs PendingControls::next")
}

// controlIdentity is the identity an automatic quota goal resume sends, so
// the adapter restarts only the goal the usage limit stopped and never one
// whose budget is spent. A user's own `/goal resume` carries none (nil).
func controlIdentity(spec *LaunchSpec, requestID string) *core.GoalSnapshot {
	panic("unported: mj-worker/src/acp/goal.rs control_identity")
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

// publish applies a goal metadata update to the session's goal state and
// publishes it as a session update.
func publish(ctx context.Context, spec *LaunchSpec, events chan<- core.RuntimeEvent, meta any) error {
	panic("unported: mj-worker/src/acp/goal.rs publish")
}

// decisionUpdate persists a goal decision update through the goal journal,
// or publishes it when the launch has none.
func decisionUpdate(ctx context.Context, spec *LaunchSpec, events chan<- core.RuntimeEvent, meta any) error {
	panic("unported: mj-worker/src/acp/goal.rs decision_update")
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
	panic("unported: mj-worker/src/acp/goal.rs prepare_control")
}

// Question is the open goal recovery question.
type Question struct {
	request *CommandLoopQuestion
	goal    core.GoalSnapshot
	intent  *string
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
	panic("unported: mj-worker/src/acp/goal.rs control")
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
	panic("unported: mj-worker/src/acp/goal.rs recover")
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
	panic("unported: mj-worker/src/acp/goal.rs resolve")
}
