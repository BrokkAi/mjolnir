// Port of mj-worker/src/acp/drive.rs.

package worker

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"math"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// cancelAckTimeout is how long a `session/cancel` may take to settle
// `session/prompt` before Hel kills the bridge and reloads the native
// session. A cooperative cancel can flush thinking; this bound is for the
// case that never acks. A variable only so a test can stand in for Rust's
// paused clock.
var cancelAckTimeout = 60 * time.Second

const cancelUnackedWarning = "cancel was not acknowledged within 60s; restarting the harness"

// rapidBridgeRestartLimit: give up if a freshly opened session dies this
// many times in a row before it has lived for rapidBridgeWindow. A later
// crash of a healthy session resets the count.
const rapidBridgeRestartLimit uint32 = 3

const rapidBridgeWindow = 5 * time.Second

// BridgeOwned is what a bridge's protocol driver uses but runBridge owns,
// because it must outlive the driver until the bridge's process teardown:
// the connection's terminals, and the guardian whose reviewer is stopped
// after the bridge on every exit path.
type BridgeOwned struct {
	terminals *TerminalRegistry
	guardian  *Guardian
}

// bridgeDriver is the state one bridge's ACP handlers share: Rust clones each
// handle into every handler closure; here the handlers are methods on one
// value.
type bridgeDriver struct {
	spec *LaunchSpec
	// events is the relay's event FIFO.
	events chan<- core.RuntimeEvent
	// bridgeCtx ends when the driver ends: the handlers that run on the
	// connection's reader stop waiting for room in events then, as Rust's
	// dispatch loop is dropped with its connection.
	bridgeCtx context.Context
	// taskCtx is what the work Rust spawns off the dispatch loop (the
	// responders of permission forms and questions) waits for room in
	// events with: it outlives the connection, as those tasks do.
	taskCtx context.Context

	terminals          *TerminalRegistry
	guardian           *Guardian
	agentOutputCount   *AgentOutputCount
	lastAgentMessage   *LastAgentMessage
	grokUsage          *Collector
	nativeSessionUsed  *atomic.Bool
	liveHistoryEnabled *atomic.Bool
	liveToolCalls      *Shared[map[string]struct{}]
	toolContent        *ToolCallContentInput
	pending            *PendingElicitations
	permissionIDPrefix string
	// permissionReviewIDs, extReviewIDs and nextElicitationID number the
	// questions this bridge raises, from 1.
	permissionReviewIDs atomic.Uint64
	extReviewIDs        atomic.Uint64
	nextElicitationID   atomic.Uint64
	enforcement         *core.ExecutionEnforcement
	ownedMcp            OwnedMcpApprovals
	planSlot            *PlanImplementationSlot
	nativeAgents        *Shared[NativeAgentRouter]
}

// isChild says whether id is one of the native child sessions this bridge
// routes.
func (d *bridgeDriver) isChild(id string) bool {
	router, unlock := d.nativeAgents.Lock()
	defer unlock()
	return router.IsChild(id)
}

// markAgentRequest records a request the agent made of Mjolnir: activity,
// parent activity unless a native child asked, and agent output.
func (d *bridgeDriver) markAgentRequest(sessionID string) {
	d.spec.ACPActivity.Mark()
	if !d.isChild(sessionID) {
		d.spec.TurnContext.MarkParentActivity()
	}
	d.agentOutputCount.mark()
}

// observeRequest records each request the agent makes in wire order, on the
// connection's reader, before its handler starts. Rust's handlers do this on
// the dispatch loop, which handles no later message until they return, so a
// prompt's response that follows a question already counts the question as
// the agent's output. acp-go runs every handler in its own goroutine; marking
// there would let the response settle first and report a turn that asked a
// question as unanswered (#970, #1265).
func (d *bridgeDriver) observeRequest(method string, params json.RawMessage) {
	var typed any
	switch method {
	case "session/request_permission":
		// Rust marks a permission request inside the task it spawns.
		return
	case "terminal/create":
		typed = &schema.CreateTerminalRequest{}
	case "terminal/output":
		typed = &schema.TerminalOutputRequest{}
	case "terminal/wait_for_exit":
		typed = &schema.WaitForTerminalExitRequest{}
	case "terminal/kill":
		typed = &schema.KillTerminalRequest{}
	case "terminal/release":
		typed = &schema.ReleaseTerminalRequest{}
	default:
		// The catch-all (extRequest) answers every other request.
		sessionID, _ := sessionIDParam(params)
		d.markAgentRequest(sessionID)
		d.spec.StepClock.BeginClientWork()
		return
	}
	// A typed request whose params do not decode reaches no handler in Rust
	// and marks nothing.
	if json.Unmarshal(params, typed) != nil {
		return
	}
	sessionID, _ := sessionIDParam(params)
	d.markAgentRequest(sessionID)
	if method == "terminal/create" {
		d.spec.StepClock.BeginClientWork()
	}
}

func driveWithTerminals(ctx context.Context, transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], sessionOpen BridgeSessionOpen, owned BridgeOwned) (SessionRestart, error) {
	bridgeCtx, endBridge := context.WithCancel(ctx)
	defer endBridge()
	pendingElicitations := spec.OperatorAway.pendingForBridgeWithSource(spec.Verdict, spec.TurnContext)
	bridgeLifetime := pendingElicitations.bridgeLifetime()
	defer bridgeLifetime.Close()
	// A bridge-local counter alone repeats after reload (and worker restart).
	// Use the same unique identity source as sessions, keeping the namespace.
	identity, err := core.NewSessionID()
	if err != nil {
		return nil, err
	}
	liveHistoryEnabled := &atomic.Bool{}
	// A provider may replay the native transcript as `session/update`
	// notifications while answering `session/load`. Hel already owns that
	// history in its durable relay, so accepting the replay would duplicate
	// every old turn on every restart. State announcements still pass
	// through. New sessions have no old history. acceptLiveHistory turns
	// them on however the session opens, including a resume that falls back
	// to `session/new`.
	liveHistoryEnabled.Store(spec.ResumeSession == nil)
	driver := &bridgeDriver{
		spec:      &spec,
		events:    events,
		bridgeCtx: bridgeCtx,
		taskCtx:   context.WithoutCancel(ctx),
		terminals: owned.terminals,
		guardian:  owned.guardian,
		// How many updates the agent produced by working, over the whole
		// connection. A turn reads it before and after to learn whether the
		// harness answered the prompt at all (#970).
		agentOutputCount: &AgentOutputCount{},
		lastAgentMessage: &LastAgentMessage{},
		grokUsage:        &Collector{},
		// Codex writes a thread's rollout, and Claude Code a session's
		// transcript, only when a model turn runs, so the evidence is a
		// prompt sent (session.go) or a Claude Code result. Agent output alone
		// is not: the Claude adapter publishes notices as agent text while it
		// answers a configuration request, and no turn runs for them (R4-3).
		nativeSessionUsed:  &atomic.Bool{},
		liveHistoryEnabled: liveHistoryEnabled,
		// Codex can finish dispatching old tool updates after `session/load`
		// has already returned. Track only creations observed after the load
		// boundary, so those delayed updates cannot reintroduce historical
		// tool state into the durable relay. A live tool always announces its
		// creation before its updates on the same ACP connection.
		liveToolCalls: NewShared(map[string]struct{}{}),
		// What an open tool call spelled out as its content, for the
		// permission form of a harness that sends no `rawInput` (R4-10).
		toolContent:        &ToolCallContentInput{},
		pending:            &pendingElicitations,
		permissionIDPrefix: toolPermissionIDPrefix + identity + "-",
		enforcement:        sessionEnforcement(&spec),
		ownedMcp:           ownedMcpApprovalsForLaunch(&spec),
		planSlot:           &PlanImplementationSlot{},
		nativeAgents:       NewShared(NativeAgentRouter{}),
	}
	boundary := newResponseTap(transport)
	connection := acp.Connect(boundary.incoming(), boundary.outgoing(), driver.handle, driver.notify,
		acp.WithRequestObserver(driver.observeRequest))
	type driven struct {
		restart SessionRestart
		err     error
	}
	done := make(chan driven, 1)
	go func() {
		restart, err := driveConnection(bridgeCtx, connection, &spec, requests, events, driver.terminals, driver.pending,
			driver.planSlot, opened, driver.agentOutputCount, driver.lastAgentMessage, driver.liveHistoryEnabled,
			driver.nativeSessionUsed, sessionOpen, driver.grokUsage, boundary)
		done <- driven{restart: restart, err: err}
	}()
	var outcome driven
	select {
	case outcome = <-done:
	case <-ctx.Done():
		// The runtime is shutting down: Rust drops the driver. Closing the
		// connection fails every request it waits for.
		go connection.Close()
		<-done
		return nil, ctx.Err()
	}
	// The handlers that run on the reader stop waiting for the relay now;
	// what Rust spawns off the dispatch loop keeps its own lifetime.
	endBridge()
	if outcome.err == nil {
		// Rust's connection flushes what is queued once the foreground
		// succeeds, then closes its output, before the driver returns. The
		// handlers Rust spawned keep running; only the flush is waited for.
		go func() {
			if err := connection.Shutdown(context.WithoutCancel(ctx)); err != nil {
				slog.Debug("ACP connection shutdown", "error", err)
			}
		}()
		select {
		case <-boundary.outputClosed:
		case <-ctx.Done():
			go connection.Close()
		}
		return outcome.restart, nil
	}
	go connection.Close()
	// Set when the session itself failed (an agent error answer, for
	// example), as opposed to the connection: only the latter may be stray
	// bridge output. The connection failed when it stopped on a broken
	// transport rather than at the bridge's end of output.
	connectionFailed := false
	select {
	case <-connection.Done():
		if failure := connection.Err(); failure != nil && !errors.Is(failure, io.EOF) && !errors.Is(failure, io.ErrClosedPipe) {
			connectionFailed = true
			return nil, protocolFailure(acpInternalError(failure.Error()), connectionFailed)
		}
	default:
	}
	return nil, protocolFailure(acpInternalError(outcome.err.Error()), connectionFailed)
}

// handle serves every request the agent sends. The typed handlers come
// first; anything else reaches the catch-all, because Hel answers every
// incoming request, always: the ACP crate parks an unhandled request that
// carries a session id instead of rejecting it, so an agent that sends an
// ext request Hel does not know would wait for a reply that never comes,
// and its turn would never end.
func (d *bridgeDriver) handle(ctx context.Context, method string, params json.RawMessage) (any, error) {
	decode := func(target any) error {
		if err := json.Unmarshal(params, target); err != nil {
			return rpcErrorWithData(rpcCodeInvalidParams, "Invalid params", err.Error())
		}
		return nil
	}
	switch method {
	case "session/request_permission":
		var request schema.RequestPermissionRequest
		if err := decode(&request); err != nil {
			return nil, err
		}
		return d.permission(ctx, request)
	case "terminal/create":
		var request schema.CreateTerminalRequest
		if err := decode(&request); err != nil {
			return nil, err
		}
		return d.createTerminal(request)
	case "terminal/output":
		var request schema.TerminalOutputRequest
		if err := decode(&request); err != nil {
			return nil, err
		}
		terminalID := string(request.TerminalID)
		snapshot, ok := d.terminals.Output(terminalID)
		if !ok {
			return nil, unknownTerminalError(terminalID)
		}
		response := schema.TerminalOutputResponse{Output: snapshot.Output, Truncated: snapshot.Truncated}
		if snapshot.Exit != nil {
			status := terminalExitStatus(snapshot.Exit)
			response.ExitStatus = &status
		}
		return response, nil
	case "terminal/wait_for_exit":
		var request schema.WaitForTerminalExitRequest
		if err := decode(&request); err != nil {
			return nil, err
		}
		terminalID := string(request.TerminalID)
		exit, ok := d.terminals.ExitReceiver(terminalID)
		if !ok {
			return nil, unknownTerminalError(terminalID)
		}
		// Each request has its own goroutine, so waiting for the child here
		// stops no other message.
		status := WaitForExit(ctx, exit)
		return schema.WaitForTerminalExitResponse{ExitCode: status.ExitCode, Signal: status.Signal}, nil
	case "terminal/kill":
		var request schema.KillTerminalRequest
		if err := decode(&request); err != nil {
			return nil, err
		}
		terminalID := string(request.TerminalID)
		// The terminal stays valid: output and wait_for_exit still answer for
		// it until the agent releases it.
		if !d.terminals.Kill(terminalID) {
			return nil, unknownTerminalError(terminalID)
		}
		return schema.KillTerminalResponse{}, nil
	case "terminal/release":
		var request schema.ReleaseTerminalRequest
		if err := decode(&request); err != nil {
			return nil, err
		}
		terminalID := string(request.TerminalID)
		supervisor, ok := d.terminals.Release(terminalID)
		if !ok {
			return nil, unknownTerminalError(terminalID)
		}
		// Reap off the dispatch loop: the supervisor still has to watch the
		// killed child exit before it reports the terminal closed.
		go func() {
			if _, err := supervisor.Join(context.Background()); err != nil {
				slog.Warn("released terminal supervisor failed",
					"terminal_id", terminalID, "operation", "terminal_release_reap", "error", err)
			}
		}()
		return schema.ReleaseTerminalResponse{}, nil
	}
	return d.extRequest(ctx, method, params)
}

// cancelledPermissionResponse answers a permission request `cancelled`.
func cancelledPermissionResponse() schema.RequestPermissionResponse {
	return schema.RequestPermissionResponse{Outcome: schema.RequestPermissionOutcome{
		Kind:      schema.RequestPermissionOutcomeKindCancelled,
		Cancelled: &schema.RequestPermissionOutcomeCancelled{},
	}}
}

// permissionOutcomeDebug is the selected option of a permission answer, for
// the decision log.
func permissionOutcomeDebug(response *schema.RequestPermissionResponse) string {
	if response.Outcome.Selected != nil {
		return "Selected(" + string(response.Outcome.Selected.OptionID) + ")"
	}
	return "Cancelled"
}

func (d *bridgeDriver) permission(ctx context.Context, request schema.RequestPermissionRequest) (any, error) {
	spec := d.spec
	harness := spec.Harness
	policy := spec.ExecutionPolicy
	sessionID := string(request.SessionID)
	toolCallID := string(request.ToolCall.ToolCallID)
	spec.ACPActivity.Mark()
	if !d.isChild(sessionID) {
		spec.TurnContext.MarkParentActivity()
	}
	d.agentOutputCount.mark()
	spec.StepClock.BeginClientWork()
	planPermission := isPlanPermission(&request)
	claudePlanApproval := isClaudePlanApproval(&request, harness)
	// The worker's policy owns tool approval, including provider asks that
	// survive full-access mode. Plans still need consent.
	if policy.IsUnconstrained() && !planPermission && !claudePlanApproval {
		response := allowPermissionResponse(&request)
		if response == nil {
			message := fmt.Sprintf("%s requested tool permission in YOLO mode without offering an allow response (session %s, tool %s).",
				harness.DisplayName(), sessionID, toolCallID)
			if emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventWarning{Message: message}) != nil {
				return nil, relayEventChannelError()
			}
			return nil, rpcErrorWithData(rpcCodeInvalidParams, "Invalid params", message)
		}
		slog.Debug("automatically approved tool permission in YOLO mode", "harness", harness.DisplayName(),
			"session_id", sessionID, "tool_call_id", toolCallID, "selected_option", permissionOutcomeDebug(response))
		return *response, nil
	}
	if response := d.ownedMcp.response(&request); response != nil {
		slog.Debug("automatically approved a Mjolnir-owned MCP tool", "harness", harness.DisplayName(),
			"session_id", sessionID, "tool_call_id", toolCallID, "selected_option", permissionOutcomeDebug(response))
		return *response, nil
	}
	input, _ := permissionToolInput(&request.ToolCall, d.toolContent)
	if !planPermission && !claudePlanApproval {
		if response := ghPermissionResponse(&request, input); response != nil {
			slog.Debug("automatically approved a gh invocation", "harness", harness.DisplayName(),
				"session_id", sessionID, "tool_call_id", toolCallID, "selected_option", permissionOutcomeDebug(response))
			return *response, nil
		}
	}
	// Review permissions that reached ACP after the existing native and
	// owned-tool approvals.
	guardianPreface := ""
	if !planPermission && !claudePlanApproval && d.guardian != nil {
		answer, preface, answered, err := d.guardianReview(ctx, &request, input)
		if answered || err != nil {
			return answer, err
		}
		guardianPreface = preface
	}
	// Preserve Muse's Guardian forms; YOLO plans use plan review.
	preferFormOverPlanReview := harness == core.HarnessKindMuse && !policy.IsUnconstrained()
	nativeChild := d.isChild(sessionID)
	if !nativeChild && !preferFormOverPlanReview && planPermission {
		return d.planReview(ctx, &request)
	}
	// What the call would run with, as streamed by now.
	input, _ = permissionToolInput(&request.ToolCall, d.toolContent)
	// A permission request that isPlanPermission did not classify is shown
	// as a generic permission form below. Log its raw shape so an agent
	// whose request form we do not yet recognize stays diagnosable from
	// worker.log.
	if raw, err := json.Marshal(request); err == nil {
		slog.Debug("permission request not classified as a plan review; raw payload follows",
			"target", "acp::plan_diag", "operation", "unclassified_permission_request", "request", string(raw))
	} else {
		slog.Debug("permission request not classified as a plan review and could not be serialized",
			"target", "acp::plan_diag", "operation", "unclassified_permission_request", "error", err)
	}
	id := fmt.Sprintf("%s%d", d.permissionIDPrefix, d.permissionReviewIDs.Add(1))
	// The worker alone decides which answers this form offers, and accepts
	// only those (see permissionChoices).
	answers := permissionChoices(&request, harness, d.enforcement)
	options := make([]any, 0, len(answers.choices))
	for _, choice := range answers.choices {
		options = append(options, jsonvalue.Object{
			{Key: "const", Value: string(choice.optionID)},
			{Key: "title", Value: choice.title},
		})
	}
	// Prefer the tool call's own title so the card reads like the action
	// being approved; fall back to the raw payload when a harness sends no
	// title.
	var message string
	if title := trimmedTitle(request.ToolCall.Title); title != "" {
		message = permissionTitleWithCommand(title, input)
	} else {
		raw, err := json.Marshal(request.ToolCall)
		if err != nil {
			return nil, acpInternalErrorWithoutData()
		}
		value, err := jsonvalue.Parse(raw)
		if err != nil {
			return nil, acpInternalErrorWithoutData()
		}
		message = acpPrettyJSON(value)
	}
	if nativeChild {
		message = fmt.Sprintf("Native agent %s: %s", sessionID, message)
	}
	formParams, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "mode", Value: "form"},
		{Key: "sessionId", Value: sessionID},
		{Key: "message", Value: fmt.Sprintf("%s%s requests permission:\n%s", guardianPreface, harness.DisplayName(), message)},
		{Key: "requestedSchema", Value: jsonvalue.Object{
			{Key: "type", Value: "object"},
			{Key: "required", Value: []any{"choice"}},
			{Key: "properties", Value: jsonvalue.Object{
				{Key: "choice", Value: jsonvalue.Object{
					{Key: "type", Value: "string"},
					{Key: "title", Value: "Permission"},
					{Key: "oneOf", Value: options},
				}},
			}},
		}},
	})
	if err != nil {
		return nil, acpInternalErrorWithoutData()
	}
	form, err := core.ElicitationRequestFromACPParams(id, formParams)
	if err != nil {
		return nil, &acp.RPCError{Code: rpcCodeInvalidParams, Message: "Invalid params"}
	}
	// ACP call IDs are session-local. A native child's calls are absent from
	// the transcript that carries this form.
	permissionForm := &core.ToolPermissionForm{Skip: answers.skip != nil}
	if !nativeChild {
		permissionForm.ToolCallID = &toolCallID
	}
	form.ToolPermission = permissionForm
	var identity AwayRequest
	if planPermission || claudePlanApproval {
		identity = AwayRequestPlan{}
	} else {
		identity = awayPermissionIdentity(&request, input)
	}
	requestingAgent := ""
	if nativeChild {
		requestingAgent = sessionID
	}
	answerRx := d.pending.register(form, requestingAgent, identity)
	var response PendingAnswer
	if emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationRequested{Request: form}) == nil {
		response = awaitElicitationResponse(ctx, d.pending, id, answerRx, ctx.Done())
	} else {
		d.pending.remove(id)
		answerRx.Close()
	}
	// ACP keeps `cancelled` for a cancelled turn. Skip is a rejection of
	// this call alone, so it selects the request's one-time rejection.
	// Cancel reaches here only after the prompt loop cancelled the turn, or
	// when no turn is running.
	var selected *schema.PermissionOptionId
	if ctx.Err() == nil {
		switch answer := response.(type) {
		case PendingAnswerHuman:
			switch answer.Value.Action {
			case core.ElicitationResponseAccept:
				if value, ok := answer.Value.Content["choice"].(core.ElicitationValueString); ok {
					for _, choice := range answers.choices {
						if string(choice.optionID) == string(value) {
							option := choice.optionID
							selected = &option
							break
						}
					}
				}
			case core.ElicitationResponseDecline:
				selected = answers.skip
			}
		case PendingAnswerOperatorAway:
			selected = answers.skip
		}
	}
	var harnessAnswer schema.RequestPermissionResponse
	_, away := response.(PendingAnswerOperatorAway)
	switch {
	case away && (planPermission || claudePlanApproval):
		harnessAnswer = permissionPlanResponse(&request, core.ElicitationResponse{Action: core.ElicitationResponseDecline})
	case selected != nil:
		harnessAnswer = selectedPermissionResponse(*selected)
	default:
		harnessAnswer = cancelledPermissionResponse()
	}
	action := "closed"
	var reply *string
	if response != nil {
		action = response.actionName()
		if human := response.human(); human != nil && human.Action != core.ElicitationResponseCancel {
			text := form.ReplyText(human)
			reply = &text
		}
	}
	if err := emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationResolved{
		ElicitationID: id, Action: action, Reply: reply,
	}); err != nil {
		slog.Debug("harness permission result receiver closed", "error", err)
	}
	return harnessAnswer, nil
}

// acpInternalErrorWithoutData is agent_client_protocol::Error::internal_error().
func acpInternalErrorWithoutData() *acp.RPCError {
	return &acp.RPCError{Code: rpcCodeInternalError, Message: "Internal error"}
}

// trimmedTitle is a tool call's title, trimmed; empty when it has none.
func trimmedTitle(title *string) string {
	if title == nil {
		return ""
	}
	return strings.TrimSpace(*title)
}

// guardianReview has the Mjolnir guardian review a tool permission request.
// answered is true when the guardian's verdict (or the turn's withdrawal)
// answers the request; otherwise preface heads the form the person gets.
func (d *bridgeDriver) guardianReview(ctx context.Context, request *schema.RequestPermissionRequest, input any) (answer any, preface string, answered bool, err error) {
	nativeChild := d.isChild(string(request.SessionID))
	// Registered like an open form, so withdrawing the turn reaches the
	// review too; a turn already withdrawn answers `cancelled` without one.
	ticket := d.pending.beginReview()
	if ticket == nil {
		return cancelledPermissionResponse(), "", true, nil
	}
	settled := false
	defer func() {
		if !settled {
			ticket.Close()
		}
	}()
	reviewCtx, stopReview := context.WithCancel(ctx)
	defer stopReview()
	decided := make(chan GuardianDecision, 1)
	go func() {
		decided <- d.guardian.review(reviewCtx, request, input, nativeChild)
	}()
	var decision GuardianDecision
	select {
	case <-ticket.withdrawn():
	case <-ctx.Done():
	case decision = <-decided:
		// Biased as Rust's select: a withdrawal or cancellation already
		// there wins over the verdict.
		select {
		case <-ticket.withdrawn():
			decision = nil
		case <-ctx.Done():
			decision = nil
		default:
		}
	}
	stopReview()
	title := trimmedTitle(request.ToolCall.Title)
	if title == "" {
		title = "a tool call"
	}
	subject := permissionTitleWithCommand(title, input)
	switch decision := decision.(type) {
	case nil:
		return cancelledPermissionResponse(), "", true, nil
	case GuardianDecisionAllow:
		// Once only: an allow the harness remembers would let later calls
		// skip review.
		response := allowOnceResponse(request)
		if response == nil {
			return nil, askPersonPreface("the guardian allowed it, but the harness offered no way to allow it once"), false, nil
		}
		// Settled under the withdrawal lock: a verdict never follows a cancel
		// the agent was told of.
		var result schema.RequestPermissionResponse
		settled = true
		ticket.settle(func(withdrawn bool) {
			if withdrawn {
				result = cancelledPermissionResponse()
			} else {
				result = *response
			}
		})
		return result, "", true, nil
	case GuardianDecisionDeny:
		var result schema.RequestPermissionResponse
		withdrawnTurn := false
		settled = true
		ticket.settle(func(withdrawn bool) {
			withdrawnTurn = withdrawn
			if withdrawn {
				result = cancelledPermissionResponse()
			} else {
				result = rejectionResponse(request)
			}
		})
		if !withdrawnTurn {
			notice := core.RuntimeEventNotice{Message: denialNotice(subject, decision.Rationale)}
			if emitRuntimeEvent(d.taskCtx, d.events, notice) != nil {
				return nil, "", true, relayEventChannelError()
			}
		}
		return result, "", true, nil
	case GuardianDecisionAskPerson:
		// A turn withdrawn from here on still reaches the form: registering
		// it answers `cancelled`.
		return nil, askPersonPreface(decision.Reason), false, nil
	}
	panic(fmt.Sprintf("unknown guardian decision %T", decision))
}

// planReview shows a plan permission request as a plan review and answers
// it as the person decides.
func (d *bridgeDriver) planReview(ctx context.Context, request *schema.RequestPermissionRequest) (any, error) {
	spec := d.spec
	id := fmt.Sprintf("plan-review-%d", d.permissionReviewIDs.Add(1))
	raw, err := json.Marshal(request)
	if err != nil {
		return nil, acpInternalErrorWithoutData()
	}
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return nil, acpInternalErrorWithoutData()
	}
	review := core.NormalizedPlanReview(id, value)
	approvedPlan, _ := core.PlanReviewProposal(&review)
	slot, unlock := d.planSlot.Lock()
	implementation := *slot
	unlock()
	answerRx := d.pending.register(review, "", AwayRequestPlan{})
	if emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationRequested{Request: review}) != nil {
		d.pending.remove(id)
		answerRx.Close()
		return nil, relayEventChannelError()
	}
	response := awaitElicitationResponse(ctx, d.pending, id, answerRx, ctx.Done())
	action := "cancel"
	var reply *string
	if response != nil {
		action = response.actionName()
		if human := response.human(); human != nil {
			text := review.ReplyText(human)
			reply = &text
		}
	}
	if err := emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationResolved{
		ElicitationID: id, Action: action, Reply: reply,
	}); err != nil {
		slog.Debug("could not report permission response to relay coordinator",
			"id", id, "operation", "elicitation_resolved", "error", err)
	}
	if ctx.Err() != nil {
		response = nil
	}
	var handoffCompletion chan bool
	answer, err := func() (schema.RequestPermissionResponse, error) {
		if response == nil {
			return cancelledPermissionResponse(), nil
		}
		selection, err := policyPlanPermissionAnswer(request, response.harnessResponse(), spec.Harness, d.enforcement)
		if err != nil {
			return schema.RequestPermissionResponse{}, err
		}
		switch selection := selection.(type) {
		case PlanPermissionAnswerNative:
			return selection.Value, nil
		case PlanPermissionAnswerContinueInBypass:
			if implementation == nil {
				return schema.RequestPermissionResponse{}, errors.New("Cannot resume the approved plan without an active prompt; select bypassPermissions and submit the implementation instruction.")
			}
			completion := make(chan bool, 1)
			if implementation.send(PlanImplementation{plan: approvedPlan, permissionSent: completion}) != nil {
				return schema.RequestPermissionResponse{}, errors.New("Plan implementation was cancelled because its prompt is no longer active.")
			}
			handoffCompletion = completion
			return cancelledPermissionResponse(), nil
		}
		panic(fmt.Sprintf("unknown plan permission answer %T", selection))
	}()
	if err != nil {
		if emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventWarning{Message: err.Error()}) != nil {
			slog.Debug("could not report failed plan implementation", "error", err)
		}
		answer = cancelledPermissionResponse()
	}
	if handoffCompletion != nil {
		// The answer is written as this handler returns; a connection that
		// already ended cannot deliver it.
		handoffCompletion <- ctx.Err() == nil
		close(handoffCompletion)
	}
	return answer, nil
}

func (d *bridgeDriver) createTerminal(request schema.CreateTerminalRequest) (any, error) {
	spec := d.spec
	// observeRequest marked the request in wire order.
	startedAtMs := core.EpochMillis()
	environment := maps.Clone(spec.Environment)
	if environment == nil {
		environment = map[string]string{}
	}
	for _, variable := range request.Env {
		environment[variable.Name] = variable.Value
	}
	names := slices.Sorted(maps.Keys(environment))
	env := make([][2]string, 0, len(names))
	for _, name := range names {
		env = append(env, [2]string{name, environment[name]})
	}
	// A terminal runs where the session runs unless the agent names a
	// directory of its own.
	cwd := spec.Cwd
	if request.Cwd != nil {
		cwd = *request.Cwd
	}
	outputByteLimit := DefaultTerminalOutputBytes
	if limit := request.OutputByteLimit; limit != nil && *limit <= uint64(math.MaxInt) {
		outputByteLimit = int(*limit)
	}
	spawn := TerminalSpawn{
		Command:         request.Command,
		Args:            slices.Clone(request.Args),
		Env:             env,
		Cwd:             cwd,
		OutputByteLimit: outputByteLimit,
	}
	command := spawn.DisplayCommand()
	terminalID := d.terminals.allocateID()
	var owner *RelayCell
	var ownerErr error
	if spec.TerminalOwner != nil {
		if owner = spec.TerminalOwner.Value(); owner == nil {
			ownerErr = errors.New("the relay owner is gone; refusing to spawn an unregistered terminal")
		}
	}
	registered := false
	announced := false
	spawned, err := func() (string, error) {
		if ownerErr != nil {
			return "", ownerErr
		}
		if owner != nil {
			registered = true
			relay := owner.Lock()
			err := relay.AgentTerminalStarted(core.ActiveAgentTerminal{
				TerminalID: terminalID, Command: command, StartedAtMs: startedAtMs,
			})
			relay.Unlock()
			if err != nil {
				return "", err
			}
		}
		if emitRuntimeEvent(d.bridgeCtx, d.events, core.RuntimeEventTerminalStarted{
			TerminalID: terminalID, Command: command, StartedAtMs: startedAtMs,
		}) != nil {
			return "", acpDisplayError{relayEventChannelError()}
		}
		announced = true
		if registered {
			if err := core.ReachTestHook("adoption_terminal_admitted_before_spawn"); err != nil {
				return "", err
			}
		}
		return d.terminals.createWithID(terminalID, spawn, d.events)
	}()
	if err == nil {
		return schema.CreateTerminalResponse{TerminalID: schema.TerminalId(spawned)}, nil
	}
	// Keep ordinary failure behind the same FIFO as the start card. A closed
	// FIFO cannot apply it, so withdraw only the inventory then; no process
	// was created.
	failureQueued := announced &&
		emitRuntimeEvent(d.bridgeCtx, d.events, core.RuntimeEventTerminalStartFailed{TerminalID: terminalID}) == nil
	if registered && !failureQueued && owner != nil {
		relay := owner.Lock()
		cleanup := relay.AgentTerminalClosed(terminalID)
		relay.Unlock()
		if cleanup != nil {
			err = fmt.Errorf("could not withdraw failed terminal inventory: %v: %w", cleanup, err)
		}
	}
	if emitRuntimeEvent(d.bridgeCtx, d.events, core.RuntimeEventWarning{
		Message: "a client terminal failed to start: " + err.Error(),
	}) != nil {
		return nil, acpInternalError(fmt.Sprintf("terminal spawn failed: %s; worker event channel closed", err))
	}
	return nil, acpInternalError(err.Error())
}

// sessionIDParam is the `sessionId` string of a request's params, when it
// has one.
func sessionIDParam(params json.RawMessage) (string, bool) {
	var members map[string]json.RawMessage
	if json.Unmarshal(params, &members) != nil {
		return "", false
	}
	var sessionID string
	if raw, ok := members["sessionId"]; !ok || json.Unmarshal(raw, &sessionID) != nil {
		return "", false
	}
	return sessionID, true
}

// extRequest is the catch-all: form elicitations, Grok's plan review, and a
// method-not-found answer for everything else.
func (d *bridgeDriver) extRequest(ctx context.Context, method string, params json.RawMessage) (any, error) {
	spec := d.spec
	// observeRequest marked the request in wire order.
	sessionID, hasSessionID := sessionIDParam(params)
	child := ""
	if hasSessionID && d.isChild(sessionID) {
		child = sessionID
	}
	if method == "elicitation/create" {
		id := fmt.Sprintf("elicitation-%d", d.nextElicitationID.Add(1))
		request, err := core.ElicitationRequestFromACPParams(id, params)
		if err != nil {
			return nil, rpcErrorWithData(rpcCodeInvalidParams, "Invalid params", "invalid ACP form elicitation: "+err.Error())
		}
		if child != "" {
			request.Message = fmt.Sprintf("Native agent %s: %s", child, request.Message)
		}
		// A parent agent can answer a question with a message, but it cannot
		// grant an approval, so approvals stay open for a person.
		if spec.ManagedSubagent && request.GoesToParent() {
			if err := recordDelegatedQuestion(d.bridgeCtx, d.events, request); err != nil {
				return nil, err
			}
			return jsonvalue.Object{{Key: "action", Value: "cancel"}}, nil
		}
		answerRx := d.pending.register(request, child, AwayRequestQuestion{})
		if emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationRequested{Request: request}) != nil {
			d.pending.remove(id)
			answerRx.Close()
			return nil, relayEventChannelError()
		}
		response := awaitElicitationResponse(ctx, d.pending, id, answerRx, ctx.Done())
		action := "cancel"
		var reply *string
		if response != nil {
			action = response.actionName()
			if human := response.human(); human != nil {
				text := request.ReplyText(human)
				reply = &text
			}
		}
		if err := emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationResolved{
			ElicitationID: id, Action: action, Reply: reply,
		}); err != nil {
			slog.Debug("could not report elicitation response to relay coordinator",
				"id", id, "operation", "elicitation_resolved", "error", err)
		}
		if response == nil {
			return nil, &acp.RPCError{Code: rpcCodeRequestCancelled, Message: "Request cancelled"}
		}
		encoded, err := json.Marshal(response.harnessResponse())
		if err != nil {
			return nil, acpInternalError("serialize elicitation response: " + err.Error())
		}
		return json.RawMessage(encoded), nil
	}
	if core.GrokHandlesExitPlanMode(spec.Harness, method) {
		id := core.GrokPlanReviewID(d.extReviewIDs.Add(1))
		var value any = jsonvalue.Object{}
		if parsed, err := jsonvalue.Parse(params); err == nil {
			value = parsed
		}
		review := core.NormalizedPlanReview(id, value)
		answerRx := d.pending.register(review, child, AwayRequestPlan{})
		if emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationRequested{Request: review}) != nil {
			d.pending.remove(id)
			answerRx.Close()
			return nil, relayEventChannelError()
		}
		response := awaitElicitationResponse(ctx, d.pending, id, answerRx, ctx.Done())
		action := "cancel"
		var reply *string
		if response != nil {
			action = response.actionName()
			if human := response.human(); human != nil {
				text := review.ReplyText(human)
				reply = &text
			}
		}
		if err := emitRuntimeEvent(d.taskCtx, d.events, core.RuntimeEventElicitationResolved{
			ElicitationID: id, Action: action, Reply: reply,
		}); err != nil {
			slog.Debug("could not report plan review response to relay coordinator",
				"id", id, "operation", "plan_review_resolved", "error", err)
		}
		if response == nil {
			return jsonvalue.Object{{Key: "outcome", Value: "cancelled"}}, nil
		}
		return core.GrokPlanResponse(response.harnessResponse()), nil
	}
	if emitRuntimeEvent(d.bridgeCtx, d.events, core.RuntimeEventWarning{Message: unsupportedClientRequestReport(method)}) != nil {
		return nil, relayEventChannelError()
	}
	return nil, rpcErrorWithData(rpcCodeMethodNotFound, "Method not found", method)
}

// notify serves the notifications the agent sends, in wire order on the
// connection's reader.
func (d *bridgeDriver) notify(method string, params json.RawMessage) error {
	switch method {
	case rawSessionNotificationMethod:
		var notification RawSessionNotification
		if err := json.Unmarshal(params, &notification); err != nil {
			return err
		}
		if err := d.sessionUpdate(notification); err != nil {
			return err
		}
		return nil
	case claudeSdkMessageNotificationMethod:
		var notification ClaudeSdkMessageNotification
		if err := json.Unmarshal(params, &notification); err != nil {
			return err
		}
		return d.claudeSdkMessage(notification)
	case grokUsageNotificationMethod:
		var notification GrokUsageNotification
		if err := json.Unmarshal(params, &notification); err != nil {
			return err
		}
		if d.spec.Harness == core.HarnessKindGrok {
			// Grok reports a turn it ran here rather than through the standard
			// updates, so this is evidence the agent worked.
			d.agentOutputCount.mark()
			if err := d.grokUsage.Observe(string(notification.SessionID), notification.Update); err != nil {
				warning := core.RuntimeEventWarning{Message: "Grok usage report was not recorded: " + err.Error()}
				if emitRuntimeEvent(d.bridgeCtx, d.events, warning) != nil {
					return relayEventChannelError()
				}
			}
		}
	}
	return nil
}

func (d *bridgeDriver) emit(event core.RuntimeEvent) error {
	if emitRuntimeEvent(d.bridgeCtx, d.events, event) != nil {
		return relayEventChannelError()
	}
	return nil
}

func (d *bridgeDriver) sessionUpdate(notification RawSessionNotification) error {
	spec := d.spec
	harness := spec.Harness
	sessionID := string(notification.SessionID)
	spec.ACPActivity.Mark()
	airHarness := harness == core.HarnessKindClaude || harness == core.HarnessKindCodex
	if airHarness {
		change, handled, err := airTaskUpdate(notification.Update)
		if err != nil {
			return d.emit(core.RuntimeEventWarning{Message: "ignored malformed AIR async task update: " + err.Error()})
		}
		if handled {
			if change != nil {
				if spawned, ok := change.(core.AirTaskUpdateTurnSpawned); ok {
					spawned.Replayed = !d.liveHistoryEnabled.Load()
					change = spawned
				}
				return d.emit(core.RuntimeEventAirTaskChanged{SessionID: sessionID, Update: change})
			}
			return nil
		}
		router, unlock := d.nativeAgents.Lock()
		routed, err := router.Route(sessionID, notification.Update)
		unlock()
		if err != nil {
			return d.emit(core.RuntimeEventWarning{Message: "native agent update: " + err.Error()})
		}
		if event, ok := routed.(RoutedEvent); ok {
			return d.emit(core.RuntimeEventNativeAgent{Event: event.Value})
		}
	}
	spec.TurnContext.MarkParentActivity()
	// A single tool card carrying a status or shape outside the ACP v1
	// vocabulary must not discard the whole notification and strand the
	// tracked tool item in_progress forever. Coerce an out-of-spec status to
	// `failed` so the item settles, and, if the update is still
	// unrepresentable, salvage a minimal settle for the named tool rather
	// than dropping everything.
	rawUpdate := notification.Update
	if replaced, ok := coerceToolCallStatus(&rawUpdate); ok {
		slog.Debug("coerced an out-of-spec ACP tool status to failed", "replaced_status", replaced)
	}
	var update schema.SessionUpdate
	encoded, err := jsonvalue.Marshal(rawUpdate)
	if err == nil {
		err = json.Unmarshal(encoded, &update)
	}
	if err != nil {
		salvaged := salvageToolCallUpdate(rawUpdate)
		if salvaged == nil {
			return rpcErrorWithData(rpcCodeInvalidParams, "Invalid params", "decode ACP session update: "+err.Error())
		}
		slog.Warn("salvaged an unrepresentable ACP tool update as a failed settle", "error", err)
		update = *salvaged
	}
	if !d.liveHistoryEnabled.Load() {
		if !core.SessionUpdateIsSessionState(&update) {
			return nil
		}
		// Session state can carry historical failures alongside current
		// metadata. Do not timestamp those as new errors.
		removeSessionFailure(&update)
	}
	suppressCodexSandboxWarning(harness, spec.ExecutionPolicy, &update)
	goal, unlockGoal := spec.GoalRecovery.Lock()
	_, err = goal.State.Apply(&update)
	unlockGoal()
	if err != nil {
		return rpcErrorWithData(rpcCodeInvalidParams, "Invalid params", err.Error())
	}
	spec.StepClock.Observe(&update)
	d.toolContent.observe(&update)
	if !sessionUpdateIsRelayVisible(&update, d.liveToolCalls, sessionID) {
		return nil
	}
	// Count only what the agent produced by working, so a turn carrying
	// nothing but the harness's own announcements is still recognized as
	// unanswered (#970).
	if core.SessionUpdateIsAgentOutput(&update) {
		d.lastAgentMessage.observe(&update)
		d.agentOutputCount.mark()
	}
	relayed, err := json.Marshal(update)
	if err != nil {
		return acpInternalError("serialize ACP session update for relay: " + err.Error())
	}
	return d.emit(core.RuntimeEventSessionUpdate{Update: relayed})
}

func (d *bridgeDriver) claudeSdkMessage(notification ClaudeSdkMessageNotification) error {
	if d.spec.Harness != core.HarnessKindClaude {
		return nil
	}
	// Sent on the same stream as the session updates, so the coordinator
	// records everything the adapter sent before the result ahead of it.
	result, err := core.ClaudeTurnResultFromSDKMessage(notification.Message)
	if err != nil {
		return d.emit(core.RuntimeEventWarning{Message: "ignored malformed Claude SDK result: " + err.Error()})
	}
	if result == nil {
		return nil
	}
	// A result ends a model cycle, and Claude Code has written that cycle
	// to the session's transcript. Report the transition once, so the worker
	// can persist that this session must never be replaced.
	if !d.nativeSessionUsed.Swap(true) {
		if err := d.emit(core.RuntimeEventNativeSessionUsed{}); err != nil {
			return err
		}
	}
	return d.emit(core.RuntimeEventClaudeTurnResult{ClaudeTurnResult: *result})
}

// responseTap is the connection's transport with one ordered hook Rust's SDK
// has and acp-go does not: a callback that runs where a request's response
// sits in the connection's incoming message order (Rust's
// `on_receiving_result`). acp-go's reader handles one frame before it reads
// the next, so the tap hands it one frame per read and runs the callback
// at the read that follows the response: after the response has been
// routed and before any later notification is handled.
type responseTap struct {
	in  *bufio.Reader
	raw io.ReadCloser
	out io.WriteCloser

	// mu guards what the session, the writer and the reader share.
	mu sync.Mutex
	// expecting is the method of the next outgoing request whose response
	// the tap watches, and onResponse its callback.
	expecting  string
	onResponse func(success bool)
	// watching are the watched requests by their id.
	watching map[string]func(success bool)

	// The rest belongs to the connection's reader, its only caller.
	// rest is what is left of the frame being handed out, and after the
	// callback its end triggers.
	rest  []byte
	after func()
	// ready is the callback to run before the next frame is read.
	ready func()
	err   error

	// outputClosed is closed once the connection has closed its output: a
	// connection shut down in order has written everything it queued by
	// then.
	outputClosed chan struct{}
	closeOutput  sync.Once
}

func newResponseTap(transport ByteStreams) *responseTap {
	return &responseTap{
		in:           bufio.NewReaderSize(transport.Incoming, 64*1024),
		raw:          transport.Incoming,
		out:          transport.Outgoing,
		watching:     map[string]func(bool){},
		outputClosed: make(chan struct{}),
	}
}

// expect watches the response to the next request sent with method.
func (t *responseTap) expect(method string, onResponse func(success bool)) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.expecting = method
	t.onResponse = onResponse
}

// canonicalID is a JSON-RPC id's compact text.
func canonicalID(raw json.RawMessage) string {
	var compact bytes.Buffer
	if json.Compact(&compact, raw) != nil {
		return string(raw)
	}
	return compact.String()
}

type tapIncoming struct{ tap *responseTap }

func (r tapIncoming) Read(p []byte) (int, error) {
	t := r.tap
	if ready := t.ready; ready != nil {
		t.ready = nil
		ready()
	}
	if len(t.rest) == 0 {
		if t.err != nil {
			return 0, t.err
		}
		line, err := t.in.ReadBytes('\n')
		t.err = err
		if len(line) == 0 {
			return 0, err
		}
		t.rest = line
		t.mu.Lock()
		watched := len(t.watching) > 0
		t.mu.Unlock()
		if watched {
			var frame struct {
				ID     json.RawMessage `json:"id"`
				Method *string         `json:"method"`
				Error  json.RawMessage `json:"error"`
			}
			if json.Unmarshal(line, &frame) == nil && frame.Method == nil && len(frame.ID) > 0 {
				id := canonicalID(frame.ID)
				t.mu.Lock()
				callback, ok := t.watching[id]
				delete(t.watching, id)
				t.mu.Unlock()
				if ok {
					success := acpRawValueAbsent(frame.Error)
					t.after = func() { callback(success) }
				}
			}
		}
	}
	n := copy(p, t.rest)
	t.rest = t.rest[n:]
	if len(t.rest) == 0 && t.after != nil {
		t.ready = t.after
		t.after = nil
	}
	return n, nil
}

func (r tapIncoming) Close() error { return r.tap.raw.Close() }

type tapOutgoing struct{ tap *responseTap }

func (w tapOutgoing) Write(p []byte) (int, error) {
	t := w.tap
	t.mu.Lock()
	if t.expecting != "" {
		var frame struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
		}
		if json.Unmarshal(p, &frame) == nil && frame.Method == t.expecting && len(frame.ID) > 0 {
			t.watching[canonicalID(frame.ID)] = t.onResponse
			t.expecting, t.onResponse = "", nil
		}
	}
	t.mu.Unlock()
	return t.out.Write(p)
}

func (w tapOutgoing) Close() error {
	err := w.tap.out.Close()
	w.tap.closeOutput.Do(func() { close(w.tap.outputClosed) })
	return err
}

func (t *responseTap) incoming() io.ReadCloser  { return tapIncoming{tap: t} }
func (t *responseTap) outgoing() io.WriteCloser { return tapOutgoing{tap: t} }

// ToolCallContentInput is the input a tool call carried as text content, by
// tool call id, while the call is open. Kimi sends no `rawInput` for a shell
// call: the input exists only as a text block it streams as the call's
// content, growing to `{"command": "..."}` (R4-10). The latest text replaces
// the earlier one, and a finished call is forgotten. Share it by pointer; the
// zero value is ready.
type ToolCallContentInput struct {
	mu    sync.Mutex
	texts map[string]string
}

// toolCallContentInputOpenCalls is the number of open calls remembered at
// once; the oldest id is dropped past this.
const toolCallContentInputOpenCalls = 64

func (t *ToolCallContentInput) observe(update *schema.SessionUpdate) {
	var id string
	var content []schema.ToolCallContent
	hasContent := false
	var status *schema.ToolCallStatus
	switch {
	case update.ToolCall != nil:
		id = string(update.ToolCall.ToolCallID)
		content, hasContent = update.ToolCall.Content, true
		status = update.ToolCall.Status
		if status == nil {
			// A tool call without a status is pending (Rust's default).
			pending := schema.ToolCallStatusPending
			status = &pending
		}
	case update.ToolCallUpdate != nil:
		id = string(update.ToolCallUpdate.ToolCallID)
		content, hasContent = update.ToolCallUpdate.Content, update.ToolCallUpdate.Content != nil
		status = update.ToolCallUpdate.Status
	default:
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	if status != nil && (*status == schema.ToolCallStatusCompleted || *status == schema.ToolCallStatusFailed) {
		delete(t.texts, id)
		return
	}
	if !hasContent {
		return
	}
	text, ok := singleTextContent(content)
	if !ok {
		return
	}
	if t.texts == nil {
		t.texts = map[string]string{}
	}
	if _, known := t.texts[id]; !known && len(t.texts) >= toolCallContentInputOpenCalls {
		// Rust's BTreeMap::pop_first: the smallest id goes.
		delete(t.texts, slices.Min(slices.Collect(maps.Keys(t.texts))))
	}
	t.texts[id] = text
}

// input is the JSON object the call's content spelled out, if it did.
func (t *ToolCallContentInput) input(toolCallID string) (any, bool) {
	t.mu.Lock()
	defer t.mu.Unlock()
	text, ok := t.texts[toolCallID]
	if !ok {
		return nil, false
	}
	return jsonObjectText(text)
}

func singleTextContent(content []schema.ToolCallContent) (string, bool) {
	if len(content) != 1 || content[0].Content == nil || content[0].Content.Content.Text == nil {
		return "", false
	}
	return content[0].Content.Content.Text.Text, true
}

func jsonObjectText(text string) (any, bool) {
	value, err := jsonvalue.Parse([]byte(strings.TrimSpace(text)))
	if err != nil {
		return nil, false
	}
	if _, isObject := value.(jsonvalue.Object); !isObject {
		return nil, false
	}
	return value, true
}

// permissionToolInput is what a permission request's tool call would run
// with: its `rawInput`, or else the JSON its content spells out, in the
// request itself or streamed for the same call before it.
func permissionToolInput(toolCall *schema.ToolCallUpdate, streamed *ToolCallContentInput) (any, bool) {
	if !acpRawValueAbsent(toolCall.RawInput) {
		if value, err := jsonvalue.Parse(toolCall.RawInput); err == nil {
			return value, true
		}
	}
	if toolCall.Content != nil {
		if text, ok := singleTextContent(toolCall.Content); ok {
			if value, ok := jsonObjectText(text); ok {
				return value, true
			}
		}
	}
	return streamed.input(string(toolCall.ToolCallID))
}

// permissionTitleWithCommand is a permission form's text: the tool call's
// title, and the command it would run when the title does not already show
// it. Kimi titles a shell request just "Bash", which left the person
// approving a command they could not see (I2-5). rawInput is nil when the
// call has no input.
func permissionTitleWithCommand(title string, rawInput any) string {
	command := ""
	found := false
	if input, ok := rawInput.(jsonvalue.Object); ok {
		value, ok := input.Get("command")
		if !ok {
			value, ok = input.Get("cmd")
		}
		if ok {
			switch value := value.(type) {
			case string:
				command, found = strings.TrimSpace(value), true
			case []any:
				var parts []string
				for _, part := range value {
					if text, ok := part.(string); ok {
						parts = append(parts, text)
					}
				}
				if len(parts) > 0 {
					command, found = strings.Join(parts, " "), true
				}
			}
		}
	}
	if found && command != "" && !strings.Contains(title, command) {
		return title + "\n$ " + command
	}
	return title
}

// cancelPromptTurn cancels the session's prompt turn: stops its terminals,
// sends `session/cancel`, then answers every pending tool permission request
// `cancelled`. ACP requires that order of a client that cancels a turn, and
// reserves `cancelled` for it.
func cancelPromptTurn(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, terminals *TerminalRegistry, pending *PendingElicitations) error {
	terminals.KillLive()
	pending.beginWithdrawal()
	sent := connection.Notify(context.WithoutCancel(ctx), "session/cancel", schema.CancelNotification{SessionID: sessionID})
	// Some agents (Kimi, I2-15) never withdraw the request themselves, which
	// left the form open on an idle session.
	withdrawToolPermissions(pending)
	return sent
}

// withdrawToolPermissions withdraws the turn's tool permission requests:
// every open form and every Mjolnir guardian review in flight is answered
// `cancelled`, and so is one that arrives later in the same turn (see
// PendingElicitations.withdrawToolPermissions).
func withdrawToolPermissions(pending *PendingElicitations) {
	pending.withdrawToolPermissions()
}

// toolPermissionIDPrefix is the id prefix of a permission request shown as a
// form.
const toolPermissionIDPrefix = core.ToolPermissionIDPrefix

// protocolFailure describes a failed ACP connection. The hint about stray
// bridge output only helps when the connection itself broke or could not
// parse what the bridge wrote; an error the agent answered (such as a missing
// thread, I2-7) says nothing about bridge stdout, and the hint would send the
// person the wrong way.
func protocolFailure(err *acp.RPCError, connectionFailed bool) error {
	if connectionFailed || err.Code == rpcCodeParseError {
		return fmt.Errorf("ACP protocol failed: %s; %s", acpErrorText(err), bridgeStdoutRule)
	}
	return fmt.Errorf("ACP protocol failed: %s", acpErrorText(err))
}

// promptErrorStopReason is the stop reason reported for a turn the bridge
// rejected instead of finishing.
const promptErrorStopReason = "error"

// acpToolCallStatuses are the tool-call statuses ACP v1 defines. An adapter
// that sends anything else (a Muse `cancelled`, say) makes the whole
// `session/update` unparseable.
var acpToolCallStatuses = [4]string{"pending", "in_progress", "completed", "failed"}

// coerceToolCallStatus: if update is a `tool_call`/`tool_call_update` whose
// `status` is outside the ACP v1 vocabulary, rewrite it to `failed` (the
// nearest legal terminal) in place so the update parses and the tracked tool
// item settles instead of stranding in_progress. Returns the replaced status
// when it coerced one.
func coerceToolCallStatus(update *any) (string, bool) {
	object, ok := (*update).(jsonvalue.Object)
	if !ok {
		return "", false
	}
	kind, _ := object.Get("sessionUpdate")
	if kind != "tool_call" && kind != "tool_call_update" {
		return "", false
	}
	statusValue, _ := object.Get("status")
	status, ok := statusValue.(string)
	if !ok {
		return "", false
	}
	if slices.Contains(acpToolCallStatuses[:], status) {
		return "", false
	}
	// The object may be shared with the notification's other readers.
	object = object.Clone()
	object.Set("status", "failed")
	*update = object
	return status, true
}

// salvageToolCallUpdate is the last resort when a tool update cannot be
// represented at all (for example an unknown nested content type): build a
// minimal `tool_call_update` that settles the named tool as `failed`, so a
// later completion the client can no longer parse does not leave the card
// running forever.
func salvageToolCallUpdate(update any) *schema.SessionUpdate {
	object, ok := update.(jsonvalue.Object)
	if !ok {
		return nil
	}
	kind, _ := object.Get("sessionUpdate")
	if kind != "tool_call" && kind != "tool_call_update" {
		return nil
	}
	idValue, _ := object.Get("toolCallId")
	toolCallID, ok := idValue.(string)
	if !ok {
		return nil
	}
	encoded, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "sessionUpdate", Value: "tool_call_update"},
		{Key: "toolCallId", Value: toolCallID},
		{Key: "status", Value: "failed"},
	})
	if err != nil {
		return nil
	}
	var salvaged schema.SessionUpdate
	if json.Unmarshal(encoded, &salvaged) != nil {
		return nil
	}
	return &salvaged
}

// turnStallTimeoutVariable is the name of the silence bound's override, in
// one place.
const turnStallTimeoutVariable = "MJ_TURN_STALL_TIMEOUT_MS"

// toolCallStallTimeoutVariable is the name of the tool-call bound's
// override, in one place.
//
// It was spelled one way here and another way in the message that tells a
// user to set it, so the documented variable did nothing and the bound was
// always the default. Everything that names it now reads this constant, and
// `the_tool_call_bound_reads_the_variable_its_message_advertises` proves the
// message and the lookup agree.
const toolCallStallTimeoutVariable = "MJ_TURN_TOOL_STALL_TIMEOUT_MS"

// parseStallTimeout is a stall bound from the text of its environment
// variable.
//
// Both bounds are opt-in. Unset, empty, unparseable and `0` all mean "no
// bound"; only a positive number of milliseconds arms one. The permissive
// parse is deliberate: a typo must not silently arm a watchdog that ends
// turns. Pure so the rule can be tested without touching process-wide state.
func parseStallTimeout(value *string) *time.Duration {
	if value == nil {
		return nil
	}
	// Rust's u64 parse accepts one leading '+'.
	millis, err := strconv.ParseUint(strings.TrimPrefix(strings.TrimSpace(*value), "+"), 10, 64)
	if err != nil || millis == 0 {
		return nil
	}
	// Rust's Duration holds any u64 of milliseconds; a Go Duration saturates.
	timeout := time.Duration(math.MaxInt64)
	if millis <= uint64(math.MaxInt64/int64(time.Millisecond)) {
		timeout = time.Duration(millis) * time.Millisecond
	}
	return &timeout
}

func timeoutFromEnvironment(name string) *time.Duration {
	value, ok := os.LookupEnv(name)
	if !ok {
		return nil
	}
	return parseStallTimeout(&value)
}

// turnStallPolicy is the bounds a running turn is held to, both off unless
// configured.
//
// Mjolnir does not guess that a quiet turn is a dead turn. Silence is not
// evidence: a turn waiting on a slow first token or a twenty-minute build
// sends nothing at all, and failing it destroys real work (#1020). The
// optional turn classifier can identify an explicit request for user input;
// the silence age is published as a fact for a person or an orchestrator to
// act on (core.ActivityState's silent-for age).
//
// An operator who wants a fixed timeout opts in per session, through the
// worker's environment, by setting turnStallTimeoutVariable or
// toolCallStallTimeoutVariable to a positive number of milliseconds. The
// bound then applies to every harness: none of them ends a turn Mjolnir
// reports without the `session/prompt` reply, so none of them is a safe
// exception.
func turnStallPolicy() core.StallPolicy {
	return core.StallPolicy{
		Silence:  turnStallTimeout(),
		ToolCall: timeoutFromEnvironment(toolCallStallTimeoutVariable),
	}
}

func turnStallTimeout() *time.Duration {
	return timeoutFromEnvironment(turnStallTimeoutVariable)
}

// turnStallFacts is what the watchdog knows about the running turn right
// now.
//
// Only two facts bear on it, and both are shared handles the relay fills:
// when anything last arrived, and which tool calls are open.
func turnStallFacts(spec *LaunchSpec) core.ActivityFacts {
	facts := core.DefaultActivityFacts()
	facts.BackgroundCommands, facts.QueuedCommands = spec.TurnContext.Counts()
	if at, ok := spec.ACPActivity.LastAtMs(); ok {
		facts.LastAcpActivityAtMs = &at
	}
	facts.ToolsInFlight = spec.ToolsInFlight.Snapshot()
	return facts
}

// turnStalledStopReason is the stop reason recorded for a turn the watchdog
// failed.
//
// A reason of its own rather than the bare word "error", so `mj wait`, the
// session summary and the recorded events all name what happened. Any stop
// reason that is not a known completion already classifies as an error, so
// nothing has to learn this string to keep working.
const turnStalledStopReason = "harness_inactive"

// turnStallMessage is the transcript message shown when a turn is failed for
// going silent. It says what happened and what the user can do, because the
// work may already be finished in the container even though mj never
// received it.
func turnStallMessage(harness core.HarnessKind, verdict core.StallVerdict) string {
	var reason string
	switch verdict := verdict.(type) {
	case core.StallVerdictSilent:
		reason = fmt.Sprintf("mj received no activity from the harness for %s while a turn was running and no "+
			"tool call was open, so it failed the turn", core.DescribeDuration(verdict.SilentMs))
	case core.StallVerdictToolCall:
		reason = fmt.Sprintf("the tool call %s ran for %s, with no activity from the harness for %s, "+
			"which is past the limit on a single tool call, so mj failed the turn. Raise or "+
			"remove that limit with %s (milliseconds, 0 removes it)",
			verdict.ToolCallID, core.DescribeDuration(verdict.RunningMs), core.DescribeDuration(verdict.SilentMs),
			toolCallStallTimeoutVariable)
	default:
		reason = "mj stopped waiting for the harness"
	}
	return turnStallTranscriptMessage(harness, reason)
}

func turnStallTranscriptMessage(harness core.HarnessKind, reason string) string {
	return fmt.Sprintf("The %s turn stopped responding: %s. The work may already "+
		"be finished inside the container even though mj did not receive it.\n"+
		"- Inspect the workspace before discarding it: check `git status` and `git log` for edits "+
		"or a commit the model made.\n"+
		"- Resend your prompt to continue; the relay reconnects on the next prompt and clears any "+
		"tool card left running.\n"+
		"- If this keeps happening it is a known harness relay stall (mjolnir #1007).",
		harness.DisplayName(), reason)
}

// promptErrorOutcome is the stop reason and the conversation warning for a
// prompt the harness failed with a JSON-RPC error.
//
// An exhausted usage limit ends the turn as `QuotaLimit`, the stop reason
// that puts the session in the quota-blocked state with its notice, for Kimi
// and for Codex, whose bridge reports it as an internal error naming
// `usageLimitExceeded` (launch finding J-25).
//
// There is no warning when it would only repeat the turn's last agent
// message: codex-acp streams Codex's own account of a failed turn as agent
// text and then fails the prompt with the same sentence, which showed it
// twice (R14-1). The turn's diagnostic still carries the error, and
// credential sync reads it there when the warning is left out.
func promptErrorOutcome(harness core.HarnessKind, err *acp.RPCError, diagnostic *core.TurnDiagnostic, lastAgentMessage string) (string, *string) {
	stopReason := failedTurnStopReason(harness, diagnostic)
	warning := promptFailureWarning(harness, err)
	if line, ok := readablePromptError(err); ok &&
		warning == "prompt failed: "+line && strings.Contains(singleSpaced(lastAgentMessage), line) {
		return stopReason, nil
	}
	return stopReason, &warning
}

// failedTurnStopReason is the stop reason of a turn the bridge failed: a
// usage limit puts Kimi and Codex sessions in the quota-blocked state,
// anything else is an error.
func failedTurnStopReason(harness core.HarnessKind, diagnostic *core.TurnDiagnostic) string {
	if (harness == core.HarnessKindKimi || harness == core.HarnessKindCodex) && diagnostic.IsUsageLimit() {
		return core.QuotaStopReason
	}
	return promptErrorStopReason
}

// sessionFailureOutcome is the stop reason and warning of a turn the bridge
// answered as an ordinary end of turn but with a typed failure record
// (Claude and Codex do this once Mjolnir asks for `sessionFailure`; issue
// 1217). The bridge no longer streams the error as agent text, so the
// warning line is the only place the conversation shows it.
func sessionFailureOutcome(diagnostic *core.TurnDiagnostic) (string, string) {
	if diagnostic.IsUsageLimit() {
		return core.QuotaStopReason, "prompt failed (usage limit reached): " + singleSpaced(diagnostic.Message)
	}
	return promptErrorStopReason, "prompt failed: " + singleSpaced(diagnostic.Message)
}

// promptFailureWarning is the warning a failed prompt leaves in the
// conversation.
//
// A bridge that puts its own sentence in the error's data (Codex does, with
// `message` and `codexErrorInfo`) gets that sentence on one line rather than
// "Internal error: " and the pretty-printed JSON; the caller logs the raw
// error. When the raw text carries an authentication code and that line does
// not, the warning keeps the raw text, because credential sync reads the
// code from this warning.
func promptFailureWarning(harness core.HarnessKind, err *acp.RPCError) string {
	// A usage limit is named as one before anything else: Kimi sends its
	// weekly limit as ACP `auth_required` with a 403 message (R4-7), and the
	// credential marker would both mislabel it and ask for a credential sync.
	if err.Code == acp.ErrorCodeAuthRequired {
		diagnostic := core.TurnDiagnosticFromACP(err)
		if diagnostic.IsUsageLimit() {
			return "prompt failed (usage limit reached): " + acpErrorText(err)
		}
		return fmt.Sprintf("prompt failed (%s): %s", core.PromptAuthRequiredMarker, acpErrorText(err))
	}
	raw := acpErrorText(err)
	// The raw text is kept only for an authentication code the readable line
	// would drop.
	if line, ok := readablePromptError(err); ok &&
		(core.AuthFailureSignature(harness, line) || !core.AuthFailureSignature(harness, raw)) {
		return "prompt failed: " + line
	}
	return "prompt failed: " + raw
}

// readablePromptError is one line for an error whose data says what went
// wrong: its `message`, or the error's own message with the Codex error
// kind. False when the data says nothing readable.
func readablePromptError(err *acp.RPCError) (string, bool) {
	if acpRawValueAbsent(err.Data) {
		return "", false
	}
	data, parseErr := jsonvalue.Parse(err.Data)
	if parseErr != nil {
		return "", false
	}
	var line string
	message, _ := jsonGetString(data, "message")
	if message != nil {
		line = *message
	} else {
		kind, ok := core.CodexErrorKind(data)
		if !ok {
			return "", false
		}
		line = fmt.Sprintf("%s (%s)", err.Message, kind)
	}
	line = singleSpaced(line)
	if line == "" {
		return "", false
	}
	return line, true
}

// jsonGetString is serde_json's `value.get(key).and_then(Value::as_str)`:
// the string member key of an object.
func jsonGetString(value any, key string) (*string, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	member, ok := object.Get(key)
	if !ok {
		return nil, false
	}
	text, ok := member.(string)
	if !ok {
		return nil, false
	}
	return &text, true
}

// singleSpaced is text with every run of whitespace, line breaks included,
// as one space.
func singleSpaced(text string) string {
	return strings.Join(strings.Fields(text), " ")
}

// AgentOutputCount is how much the agent has produced by working, over one
// ACP connection.
//
// A turn reads it before and after to learn whether the harness did anything
// the prompt asked for (#970). It is marked wherever the agent acts: the
// session updates that carry its messages, thoughts, plans and tool calls,
// and every request it makes of Mjolnir — a permission, a terminal, an
// elicitation. Traffic a harness emits on its own schedule is excluded, so a
// turn carrying only a command catalogue, a usage figure or a compaction
// banner counts as having produced nothing. Share it by pointer; the zero
// value is ready.
type AgentOutputCount struct {
	count atomic.Uint64
}

func (c *AgentOutputCount) mark() {
	c.count.Add(1)
}

func (c *AgentOutputCount) get() uint64 {
	return c.count.Load()
}

// LastAgentMessage is the text of the agent message the harness is streaming
// in this turn.
//
// A failed prompt's warning is left out when it only repeats this message
// (R14-1). A thought or a tool call ends the message, as it ends the agent's
// row in the transcript, and each prompt starts with none. Share it by
// pointer; the zero value is ready.
type LastAgentMessage struct {
	mu      sync.Mutex
	message strings.Builder
}

func (m *LastAgentMessage) observe(update *schema.SessionUpdate) {
	m.mu.Lock()
	defer m.mu.Unlock()
	switch {
	case update.AgentMessageChunk != nil:
		if text := update.AgentMessageChunk.Content.Text; text != nil {
			m.message.WriteString(text.Text)
		}
	case update.AgentThoughtChunk != nil, update.ToolCall != nil:
		m.message.Reset()
	}
}

func (m *LastAgentMessage) clear() {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.message.Reset()
}

func (m *LastAgentMessage) text() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.message.String()
}

// promptUnansweredMessage is what the person is told when the harness ended
// a turn without answering.
//
// It leads with the stable marker core's credentials matching looks for,
// then says in plain language what happened and what to do. It deliberately
// does not claim the prompt was dropped: Mjolnir cannot tell a prompt the
// harness never acted on from one it acted on silently, and resending is the
// person's decision because the harness may have done the work already.
func promptUnansweredMessage(harness core.HarnessKind) string {
	return fmt.Sprintf("%s: %s ended the turn without producing any message, "+
		"thought or tool call, so this prompt may never have been acted on. Check the workspace "+
		"before resending it, in case the work was done without being reported.",
		core.PromptEmptyResponseMarker, harness.DisplayName())
}

// promptReturnedWithoutUpdates says whether the harness ended this turn
// without doing anything the prompt asked for.
//
// Only a turn the harness reported as finished is judged. A cancelled,
// refused, token-limited or errored turn already reports its own ending, and
// relabelling those would hide the reason they really ended. The counters
// count agent output only (core.SessionUpdateIsAgentOutput), so a turn that
// ran a tool and said nothing counts as answered, while a turn carrying only
// the harness's own banners counts as unanswered (#970).
func promptReturnedWithoutUpdates(stopReason schema.StopReason, updatesBefore, updatesAfter uint64) bool {
	return stopReason == schema.StopReasonEndTurn && updatesBefore == updatesAfter
}

func driveConnection(ctx context.Context, connection *acp.Connection, spec *LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, terminals *TerminalRegistry, pendingElicitations *PendingElicitations, planImplementationSlot *PlanImplementationSlot, opened *Shared[*OpenedSession], agentOutputCount *AgentOutputCount, lastAgentMessage *LastAgentMessage, liveHistoryEnabled *atomic.Bool, nativeSessionUsed *atomic.Bool, sessionOpen BridgeSessionOpen, grokUsage *Collector, boundary *responseTap) (SessionRestart, error) {
	// Terminals belong to the connection. However the session ends —
	// closed, failed, or with its command channel dropped — their process
	// groups must not outlive it.
	restart, err := serveSession(ctx, connection, spec, requests, events, terminals, pendingElicitations,
		planImplementationSlot, opened, agentOutputCount, lastAgentMessage, liveHistoryEnabled,
		nativeSessionUsed, sessionOpen, grokUsage, boundary)
	pendingElicitations.close()
	terminals.Shutdown(context.WithoutCancel(ctx), events)
	return restart, err
}

func applyCancel(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, cancelID string, events chan<- core.RuntimeEvent, terminals *TerminalRegistry, pending *PendingElicitations) error {
	if err := cancelPromptTurn(ctx, connection, sessionID, terminals, pending); err != nil {
		return emitRuntimeEvent(ctx, events, core.RuntimeEventCommandRejected{
			Reason:    core.OutcomeReasonCommandFailed,
			RequestID: cancelID,
			Message:   "cancel ACP prompt: " + acpErrorDisplay(err),
		})
	}
	return emitRuntimeEvent(ctx, events, core.RuntimeEventCancelApplied{RequestID: cancelID})
}

const sessionSteeringMethod = "_session/steering"

func steeringSupportedFromMeta(meta schema.Meta) bool {
	steering, ok := meta.Get("steering")
	if !ok {
		return false
	}
	object, ok := steering.(jsonvalue.Object)
	if !ok {
		return false
	}
	supported, _ := object.Get("supported")
	enabled, _ := supported.(bool)
	return enabled
}

// PendingSteer is a steering request waiting for the bridge's
// acknowledgement.
type PendingSteer struct {
	requestID       string
	queuedCommandID string
	response        <-chan asyncResult[any]
	// drop drops the steering request, cancelling it while it still waits
	// for its answer (Rust drops the pinned future).
	drop context.CancelFunc
}

// resolveAttachments loads the attached images a prompt references. The
// attachment store reads prompts as the unstable ACP schema; the two schemas
// share the wire form, so the prompt crosses by its JSON.
func resolveAttachments(root string, prompt []schema.ContentBlock) ([]schema.ContentBlock, error) {
	encoded, err := json.Marshal(prompt)
	if err != nil {
		return nil, err
	}
	var unstable []acpUnstableContentBlock
	if err := json.Unmarshal(encoded, &unstable); err != nil {
		return nil, err
	}
	store := core.AttachmentStoreWorker(root)
	if err := store.Resolve(unstable); err != nil {
		return nil, err
	}
	encoded, err = json.Marshal(unstable)
	if err != nil {
		return nil, err
	}
	var resolved []schema.ContentBlock
	if err := json.Unmarshal(encoded, &resolved); err != nil {
		return nil, err
	}
	return resolved, nil
}

func startSteer(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, requestID string, steeringPrompt core.ClaimedSteeringPrompt) PendingSteer {
	response := make(chan asyncResult[any], 1)
	callCtx, drop := context.WithCancel(context.WithoutCancel(ctx))
	go func() {
		defer drop()
		prompt := steeringPrompt.Prompt
		if steeringPrompt.AttachmentRoot != nil {
			resolved, err := resolveAttachments(*steeringPrompt.AttachmentRoot, prompt)
			if err != nil {
				response <- asyncResult[any]{err: acpDisplayError{acpInternalError(err.Error())}}
				return
			}
			prompt = resolved
		}
		if prompt == nil {
			prompt = []schema.ContentBlock{}
		}
		params := jsonvalue.Object{
			{Key: "sessionId", Value: string(sessionID)},
			{Key: "prompt", Value: prompt},
			{Key: "_meta", Value: jsonvalue.Object{
				{Key: "steering", Value: jsonvalue.Object{{Key: "idleBehavior", Value: "promptRequired"}}},
			}},
		}
		var raw json.RawMessage
		err := acpCallError(connection.Call(callCtx, sessionSteeringMethod, params, &raw))
		if err != nil {
			response <- asyncResult[any]{err: err}
			return
		}
		value, parseErr := jsonvalue.Parse(raw)
		if parseErr != nil {
			response <- asyncResult[any]{err: acpDisplayError{acpInternalError(parseErr.Error())}}
			return
		}
		response <- asyncResult[any]{value: value}
	}()
	return PendingSteer{requestID: requestID, queuedCommandID: steeringPrompt.QueuedCommandID, response: response, drop: drop}
}

// steerOutcomeDebug is Rust's `{outcome:?}` of a steering result.
func steerOutcomeDebug(outcome asyncResult[any]) string {
	if outcome.err != nil {
		rpc := acpRPCError(outcome.err)
		data := "None"
		if !acpRawValueAbsent(rpc.Data) {
			if value, err := jsonvalue.Parse(rpc.Data); err == nil {
				data = "Some(" + jsonValueDebug(value) + ")"
			}
		}
		return fmt.Sprintf("Err(Error { code: %d, message: %s, data: %s })",
			rpc.Code, sessionConfigDebugString(rpc.Message), data)
	}
	return "Ok(" + jsonValueDebug(outcome.value) + ")"
}

// jsonValueDebug is serde_json::Value's Debug form.
func jsonValueDebug(value any) string {
	switch value := value.(type) {
	case nil:
		return "Null"
	case bool:
		return fmt.Sprintf("Bool(%t)", value)
	case string:
		return "String(" + sessionConfigDebugString(value) + ")"
	case int64, uint64, float64:
		encoded, _ := jsonvalue.Marshal(value)
		return "Number(" + string(encoded) + ")"
	case []any:
		items := make([]string, len(value))
		for i, item := range value {
			items[i] = jsonValueDebug(item)
		}
		return "Array [" + strings.Join(items, ", ") + "]"
	case jsonvalue.Object:
		members := make([]string, len(value))
		for i, member := range value {
			members[i] = sessionConfigDebugString(member.Key) + ": " + jsonValueDebug(member.Value)
		}
		return "Object {" + strings.Join(members, ", ") + "}"
	}
	return fmt.Sprint(value)
}

// settleSteer reports a steering request's outcome: the bridge's answer, or
// the error it failed with.
func settleSteer(ctx context.Context, events chan<- core.RuntimeEvent, pending PendingSteer, outcome asyncResult[any]) error {
	var status *string
	if outcome.err == nil {
		status, _ = jsonGetString(outcome.value, "outcome")
	}
	switch {
	case status != nil && *status == "injected":
		return emitRuntimeEvent(ctx, events, core.RuntimeEventSteerApplied{
			RequestID: pending.requestID, QueuedCommandID: pending.queuedCommandID,
		})
	// Sent with `idleBehavior: "promptRequired"`: the turn ended before the
	// bridge could inject, so the prompt stays queued for the next one.
	case status != nil && *status == "promptRequired":
		return emitRuntimeEvent(ctx, events, core.RuntimeEventSteerReturned{
			RequestID: pending.requestID, QueuedCommandID: pending.queuedCommandID,
		})
	}
	unexpected := false
	if outcome.err != nil {
		switch acpRPCError(outcome.err).Code {
		case rpcCodeInvalidRequest, rpcCodeInvalidParams, rpcCodeMethodNotFound, acp.ErrorCodeAuthRequired:
		default:
			unexpected = true
		}
	} else if status == nil || *status != "failed" {
		unexpected = true
	}
	if unexpected {
		return emitRuntimeEvent(ctx, events, core.RuntimeEventCommandInterrupted{
			Reason:    core.OutcomeReasonRuntimeFailure,
			RequestID: pending.requestID,
			Message:   "Unexpected steering response; delivery is unconfirmed: " + steerOutcomeDebug(outcome),
		})
	}
	var message string
	if outcome.err == nil {
		encoded, _ := jsonvalue.Marshal(outcome.value)
		message = fmt.Sprintf("Steering was not confirmed: %s. The prompt remains queued.", encoded)
	} else {
		message = fmt.Sprintf("Steering failed: %s. The prompt remains queued.", acpErrorDisplay(outcome.err))
	}
	return emitRuntimeEvent(ctx, events, core.RuntimeEventCommandRejected{
		Reason:    core.OutcomeReasonCommandFailed,
		RequestID: pending.requestID,
		Message:   message,
	})
}

// settleSteerAtTurnEnd settles a steer still waiting for its acknowledgement
// when the prompt ends. The acknowledgement normally follows at once;
// without it within two seconds, delivery is reported as unconfirmed and the
// queued input is held. pendingSteer is Rust's `&mut Option<PendingSteer>`:
// it is taken.
func settleSteerAtTurnEnd(ctx context.Context, events chan<- core.RuntimeEvent, pendingSteer **PendingSteer) error {
	pending := *pendingSteer
	if pending == nil {
		return nil
	}
	*pendingSteer = nil
	timer := time.NewTimer(2 * time.Second)
	defer timer.Stop()
	select {
	case outcome := <-pending.response:
		return settleSteer(ctx, events, *pending, outcome)
	case <-timer.C:
		pending.drop()
		return emitRuntimeEvent(ctx, events, core.RuntimeEventCommandInterrupted{
			Reason:    core.OutcomeReasonRuntimeFailure,
			RequestID: pending.requestID,
			Message:   "Steering delivery unconfirmed after the turn ended; queued input is held for review",
		})
	case <-ctx.Done():
		return errRelayEventCoordinatorStopped
	}
}

// drainRequestsFromThePreviousBridge discards requests left in the channel
// by the bridge that just restarted. See the call site in serveSession for
// why nothing is reported back.
func drainRequestsFromThePreviousBridge(requests <-chan CommandRequest) {
	for {
		var request CommandRequest
		select {
		case received, ok := <-requests:
			if !ok {
				return
			}
			request = received
		default:
			return
		}
		variant, requestID := "", "-"
		switch request := request.(type) {
		case CommandRequestPrompt:
			variant, requestID = "Prompt", request.RequestID
		case CommandRequestPromptAttachments:
			variant, requestID = "Prompt", request.RequestID
		case CommandRequestClearContext:
			variant, requestID = "ClearContext", request.RequestID
		case CommandRequestSetConfig:
			variant, requestID = "SetConfig", request.RequestID
		case CommandRequestGoalControl:
			variant, requestID = "GoalControl", request.RequestID
		case CommandRequestSetSessionMode:
			variant, requestID = "SetSessionMode", request.RequestID
		case CommandRequestRestoreExecutionMode:
			variant, requestID = "RestoreExecutionMode", request.RequestID
		case CommandRequestCancelTurnFor:
			variant, requestID = "CancelTurnFor", request.RequestID
		case CommandRequestSteer:
			variant, requestID = "Steer", request.RequestID
		case CommandRequestCancel:
			variant, requestID = "Cancel", request.RequestID
		case CommandRequestClose:
			variant, requestID = "Close", request.RequestID
		case CommandRequestResolveElicitation:
			variant = "ResolveElicitation"
		case CommandRequestStopBackgroundTask:
			sendResolution(request.Resolved, errors.New("ACP bridge restarted before stopping task"))
			variant = "StopBackgroundTask"
		case CommandRequestReloadLaunchConfig:
			sendResolution(request.Resolved, errors.New("ACP bridge restarted before applying launch configuration"))
			releaseReloadLeases(request.Leases)
			variant = "ReloadLaunchConfig"
		}
		slog.Debug("dropping a request queued for the previous ACP bridge",
			"operation", "acp_bridge_restart", "variant", variant, "request_id", requestID)
	}
}

// harnessReportsMissingNativeSession says whether a failed `session/resume`
// or `session/load` failed because the harness has no such native session.
// Only harnesses that defer writing a session to disk until its first user
// message can report a session Mjolnir believes it created; every other
// harness's reload failure means something else, and the classifier answers
// false for them.
func harnessReportsMissingNativeSession(spec *LaunchSpec, err error) bool {
	if spec.ResumeSession == nil {
		return false
	}
	return core.ErrorReportsMissingNativeSession(spec.Harness, *spec.ResumeSession, err)
}

// recordDelegatedQuestion: the original ACP responder closes the form; no
// later RPC can cancel a reused id.
func recordDelegatedQuestion(ctx context.Context, events chan<- core.RuntimeEvent, request core.ElicitationRequest) error {
	return recordRefusedQuestion(ctx, events, request, "cancel")
}

func recordRefusedQuestion(ctx context.Context, events chan<- core.RuntimeEvent, request core.ElicitationRequest, action string) error {
	elicitationID := request.ID
	if emitRuntimeEvent(ctx, events, core.RuntimeEventElicitationRequested{Request: request}) != nil {
		return relayEventChannelError()
	}
	resolved := core.RuntimeEventElicitationResolved{ElicitationID: elicitationID, Action: action}
	if emitRuntimeEvent(ctx, events, resolved) != nil {
		return relayEventChannelError()
	}
	return nil
}
