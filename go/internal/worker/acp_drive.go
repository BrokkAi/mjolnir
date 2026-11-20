// Port of mj-worker/src/acp/drive.rs.

package worker

import (
	"context"
	"fmt"
	"math"
	"os"
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
// case that never acks.
const cancelAckTimeout = 60 * time.Second

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

func driveWithTerminals(ctx context.Context, transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], sessionOpen BridgeSessionOpen, owned BridgeOwned) (SessionRestart, error) {
	panic("unported: mj-worker/src/acp/drive.rs drive_with_terminals")
}

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
	panic("unported: mj-worker/src/acp/drive.rs ToolCallContentInput::observe")
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
	panic("unported: mj-worker/src/acp/drive.rs permission_tool_input")
}

// permissionTitleWithCommand is a permission form's text: the tool call's
// title, and the command it would run when the title does not already show
// it. Kimi titles a shell request just "Bash", which left the person
// approving a command they could not see (I2-5). rawInput is nil when the
// call has no input.
func permissionTitleWithCommand(title string, rawInput any) string {
	panic("unported: mj-worker/src/acp/drive.rs permission_title_with_command")
}

// cancelPromptTurn cancels the session's prompt turn: stops its terminals,
// sends `session/cancel`, then answers every pending tool permission request
// `cancelled`. ACP requires that order of a client that cancels a turn, and
// reserves `cancelled` for it.
func cancelPromptTurn(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, terminals *TerminalRegistry, pending *PendingElicitations) error {
	terminals.KillLive()
	pending.beginWithdrawal()
	sent := connection.Notify(ctx, "session/cancel", schema.CancelNotification{SessionID: sessionID})
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
	panic("unported: mj-worker/src/acp/drive.rs coerce_tool_call_status")
}

// salvageToolCallUpdate is the last resort when a tool update cannot be
// represented at all (for example an unknown nested content type): build a
// minimal `tool_call_update` that settles the named tool as `failed`, so a
// later completion the client can no longer parse does not leave the card
// running forever.
func salvageToolCallUpdate(update any) *schema.SessionUpdate {
	panic("unported: mj-worker/src/acp/drive.rs salvage_tool_call_update")
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
	panic("unported: mj-worker/src/acp/drive.rs turn_stall_message")
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
	panic("unported: mj-worker/src/acp/drive.rs prompt_failure_warning")
}

// readablePromptError is one line for an error whose data says what went
// wrong: its `message`, or the error's own message with the Codex error
// kind. False when the data says nothing readable.
func readablePromptError(err *acp.RPCError) (string, bool) {
	panic("unported: mj-worker/src/acp/drive.rs readable_prompt_error")
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

func driveConnection(ctx context.Context, connection *acp.Connection, spec *LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, terminals *TerminalRegistry, pendingElicitations *PendingElicitations, planImplementationSlot *PlanImplementationSlot, opened *Shared[*OpenedSession], agentOutputCount *AgentOutputCount, lastAgentMessage *LastAgentMessage, liveHistoryEnabled *atomic.Bool, nativeSessionUsed *atomic.Bool, sessionOpen BridgeSessionOpen, grokUsage *Collector) (SessionRestart, error) {
	// Terminals belong to the connection. However the session ends —
	// closed, failed, or with its command channel dropped — their process
	// groups must not outlive it.
	restart, err := serveSession(ctx, connection, spec, requests, events, terminals, pendingElicitations,
		planImplementationSlot, opened, agentOutputCount, lastAgentMessage, liveHistoryEnabled,
		nativeSessionUsed, sessionOpen, grokUsage)
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
}

func startSteer(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, requestID string, steeringPrompt core.ClaimedSteeringPrompt) PendingSteer {
	panic("unported: mj-worker/src/acp/drive.rs start_steer")
}

// settleSteer reports a steering request's outcome: the bridge's answer, or
// the error it failed with.
func settleSteer(ctx context.Context, events chan<- core.RuntimeEvent, pending PendingSteer, outcome asyncResult[any]) error {
	panic("unported: mj-worker/src/acp/drive.rs settle_steer")
}

// settleSteerAtTurnEnd settles a steer still waiting for its acknowledgement
// when the prompt ends. The acknowledgement normally follows at once;
// without it within two seconds, delivery is reported as unconfirmed and the
// queued input is held. pendingSteer is Rust's `&mut Option<PendingSteer>`:
// it is taken.
func settleSteerAtTurnEnd(ctx context.Context, events chan<- core.RuntimeEvent, pendingSteer **PendingSteer) error {
	panic("unported: mj-worker/src/acp/drive.rs settle_steer_at_turn_end")
}

// drainRequestsFromThePreviousBridge discards requests left in the channel
// by the bridge that just restarted. See the call site in serveSession for
// why nothing is reported back.
func drainRequestsFromThePreviousBridge(requests <-chan CommandRequest) {
	panic("unported: mj-worker/src/acp/drive.rs drain_requests_from_the_previous_bridge")
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
