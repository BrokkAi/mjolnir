// Port of mj-worker/src/subagent_mcp.rs.
//
// Mjolnir-owned delegation tools for Claude, Codex, DSH, and OpenCode
// sessions.
//
// JSON values (serde_json::Value) are decoded with UseNumber into any:
// map[string]any, []any, json.Number, string, bool or nil. Go's encoder
// sorts map keys as serde_json does without preserve_order.

package worker

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// handbackUnansweredAdvice is a child's advice when Mjolnir has not
// confirmed its report. Calling again is safe: a report that did arrive is
// refused as already delivered.
const handbackUnansweredAdvice = "It may still be recorded; call handback again with the same report. An answer that it was already delivered means the first call arrived."

// reviewUnansweredAdvice: a review whose preparation outlasts the reply
// window keeps preparing; no deadline stops it (#1503). Calling again is
// safe: Mjolnir refuses it while that review prepares or runs, and starts
// one if that review did not start.
const reviewUnansweredAdvice = "The review may still be preparing. Call request_review again to find out: it answers that a review is already open while this one prepares or runs, and starts a new review if this one did not start."

// unansweredAdviceText is Rust's UNANSWERED_ADVICE (renamed because the
// function unanswered_advice takes the Go name unansweredAdvice): what the
// model is told when Mjolnir has not answered a request. It may still run,
// so repeating it blindly could spawn a second child or send the same input
// twice. list_agents shows what actually happened.
const unansweredAdviceText = "It may still run; call list_agents to see the children's state before repeating this call."

// unansweredAdvice is what to do about a request Mjolnir has not answered,
// for this action.
func unansweredAdvice(action core.SubagentToolAction) string {
	switch action.(type) {
	case core.SubagentToolActionHandback:
		return handbackUnansweredAdvice
	case core.SubagentToolActionFetch:
		return "The fetch is safe to repeat; call fetch again for its result."
	case core.SubagentToolActionRequestReview:
		return reviewUnansweredAdvice
	}
	return unansweredAdviceText
}

// DaemonContact is what the worker can say about a request the daemon has
// not answered. The worker sends it with an answer that has no result.
// Unknown fields are refused.
type DaemonContact struct {
	// PickedUp is whether the daemon has read the worker's queue since this
	// request joined it.
	PickedUp bool `json:"picked_up"`
	// LastCollectedSecondsAgo is seconds since the daemon last read the
	// queue; nil when it has not read it since the worker started.
	LastCollectedSecondsAgo *uint64 `json:"last_collected_seconds_ago,omitempty"`
}

// UnmarshalJSON refuses unknown fields and requires picked_up, as serde's
// deny_unknown_fields does.
func (c *DaemonContact) UnmarshalJSON(data []byte) error {
	type plain DaemonContact
	return subagentMCPDecodeStrict(data, (*plain)(c), true)
}

// daemonContactFromReply reads the contact from a worker answer. An older
// worker does not send it.
func daemonContactFromReply(reply any) *DaemonContact {
	object, ok := reply.(map[string]any)
	if !ok {
		return nil
	}
	daemon, ok := object["daemon"]
	if !ok {
		return nil
	}
	data, err := json.Marshal(daemon)
	if err != nil {
		return nil
	}
	var contact DaemonContact
	if json.Unmarshal(data, &contact) != nil {
		return nil
	}
	return &contact
}

// notPickedUp is one sentence naming a daemon that has not picked the
// request up; false when it has.
func (c DaemonContact) notPickedUp() (string, bool) {
	if c.PickedUp {
		return "", false
	}
	last := "Its daemon has not collected this session's requests since this session's worker started"
	if c.LastCollectedSecondsAgo != nil {
		last = fmt.Sprintf("Its daemon last collected this session's requests %d seconds ago", *c.LastCollectedSecondsAgo)
	}
	return fmt.Sprintf("Mjolnir has not picked up this request. %s, so the daemon is probably restarting, stalled, or unreachable.", last), true
}

// notPickedUpAdvice is what to do about a request the daemon has not picked
// up. It stays saved and runs once a daemon reads the queue, which can be
// long after this call.
func notPickedUpAdvice(action core.SubagentToolAction) string {
	switch action.(type) {
	case core.SubagentToolActionHandback:
		return handbackUnansweredAdvice
	case core.SubagentToolActionFetch:
		return "The fetch is safe to repeat; call fetch again later."
	case core.SubagentToolActionListAgents, core.SubagentToolActionListProfiles:
		return "It changes nothing, so call it again later."
	case core.SubagentToolActionSpawn:
		return "The request is saved and runs when the daemon picks it up, which can be much later, so do not repeat it. Continue without this child or call list_agents later to see whether it started; close it if you no longer need it."
	}
	return "The request is saved and runs when the daemon picks it up, which can be much later, so do not repeat it. Call list_agents later to see whether it ran."
}

// pendingReply is the degraded answer when the worker has taken the request
// but the daemon has not completed it in time. The request stays queued; the
// model must check on it itself, because nothing is ever pushed into its
// conversation. When the worker says the daemon has not even picked the
// request up, the answer says so and is an error: nothing is working on it.
func pendingReply(requestID string, action core.SubagentToolAction, daemon *DaemonContact) (any, bool) {
	if daemon != nil {
		if reason, ok := daemon.notPickedUp(); ok {
			return map[string]any{
				"request_id": requestID,
				"accepted":   true,
				"status":     "not_picked_up",
				"error":      reason + " " + notPickedUpAdvice(action),
			}, true
		}
	}
	status := "Mjolnir has not answered this request yet."
	if daemon != nil {
		status = "Mjolnir picked up this request but has not finished it."
	}
	return map[string]any{"request_id": requestID, "accepted": true, "note": status + " " + unansweredAdvice(action)}, false
}

// replyTimeout is how long an action may take to be answered. The daemon
// must notice the queued request, run the action (a spawn may provision a
// child session) and complete it back to the worker. The worker answers a
// little sooner by itself, so it can say whether the daemon picked it up.
const replyTimeout = 120 * time.Second

// stillClosingReply is the answer to a close Mjolnir has not confirmed
// within the call's budget.
//
// A close is finished when the child's process tree is gone, which is later
// than the moment the request was taken: it cancels whatever owned the
// session, checkpoints, seals the relay and tears the target down. Calling
// that "accepted" told a parent nothing it could act on, and a parent that
// then spawned the replacement child stacked both process trees inside one
// container until it ran out of process slots (#1087). So say what is true
// - the close may still be running - and name the one tool that can observe
// the child being gone.
func stillClosingReply(requestID, childSessionID, reason string) map[string]any {
	return map[string]any{
		"request_id":       requestID,
		"child_session_id": childSessionID,
		"closed":           false,
		"status":           "still_closing",
		"note":             reason,
		"next_action":      "Use list_agents to observe all of your children. This child may remain in state stopping until teardown reaches stopped. Do not spawn a replacement child before then.",
	}
}

// replyTimeoutFor is Rust's reply_timeout (renamed because the constant
// REPLY_TIMEOUT takes the Go name replyTimeout): calls use the same bounded
// reply window.
func replyTimeoutFor(_ core.SubagentToolAction, _ *core.HarnessKind) time.Duration {
	return replyTimeout
}

// unansweredReply explains an unconfirmed close or a request with no reply.
func unansweredReply(requestID string, action core.SubagentToolAction, waited time.Duration) (any, bool) {
	seconds := uint64(waited / time.Second)
	if closing, ok := action.(core.SubagentToolActionCloseAgent); ok {
		return stillClosingReply(requestID, closing.ChildSessionID, fmt.Sprintf(
			"Mjolnir did not confirm this close within %d seconds. The close may still be running, so this child is not known to be gone.",
			seconds)), false
	}
	return map[string]any{
		"request_id": requestID,
		"error":      fmt.Sprintf("Mjolnir did not answer this request within %d seconds. %s", seconds, unansweredAdvice(action)),
	}, true
}

// modelFacing is what the model reads for one answer. The daemon and the
// worker both put the answer's own JSON into SubagentToolResult.Message, so
// handing the envelope back would leave the model parsing JSON out of a
// string inside a wrapper. Unwrap it, keeping request_id alongside the
// answer.
func modelFacing(requestID string, result any) any {
	object, _ := result.(map[string]any)
	message, ok := object["message"].(string)
	if !ok {
		return result
	}
	payload, err := subagentMCPDecodeValue([]byte(message))
	answer, isObject := payload.(map[string]any)
	if err != nil || !isObject {
		return result
	}
	if _, ok := answer["request_id"]; !ok {
		answer["request_id"] = requestID
	}
	return answer
}

// ParentTools is what a parent's tool set offers, from the worker's launch
// configuration.
type ParentTools struct {
	Oracle bool
	// AgentMailboxesEnabled is a legacy option accepted for older launch
	// arguments; it has no effect.
	AgentMailboxesEnabled bool
	// SubtleSubagents is whether spawn offers the subtle task type. Without
	// it, spawn has no task_type and every child uses the straightforward
	// choice.
	SubtleSubagents bool
}

// DefaultParentTools is Rust's ParentTools::default().
func DefaultParentTools() ParentTools {
	return ParentTools{Oracle: false, AgentMailboxesEnabled: true, SubtleSubagents: true}
}

// childReportBudget: every harness already supplies the worker socket. Its
// adjacent launch file carries controller-authored settings even when the
// harness filters its env.
type childReportBudget struct {
	directory string
	limit     uint64
}

// childReportBudgetFromSocket reads the budget from the launch file beside
// socket; nil when the file is absent or an older controller configured no
// budget.
func childReportBudgetFromSocket(socket string) (*childReportBudget, error) {
	panic("unported: mj-worker/src/subagent_mcp.rs ChildReportBudget::from_socket")
}

func (b *childReportBudget) note() string {
	return fmt.Sprintf("Report directory limit: %d bytes.", b.limit)
}

// check refuses a handback whose report directory holds more than the
// limit, naming the largest files.
func (b *childReportBudget) check() error {
	panic("unported: mj-worker/src/subagent_mcp.rs ChildReportBudget::check")
}

// SubagentMCPRunMCPStdio is Rust's subagent_mcp::run_mcp_stdio (prefixed:
// memory_mcp's run_mcp_stdio keeps RunMCPStdio). It serves the mj-agents
// tools over standard input and output.
func SubagentMCPRunMCPStdio(socket string, harness *core.HarnessKind, role core.SubagentMcpRole, options ParentTools) error {
	return runWithOptions(os.Stdin, os.Stdout, socket, harness, role, options)
}

// runWithOptions serves MCP over reader/writer against the worker socket.
// Calls are dispatched concurrently, each on its own socket connection, so
// startup cannot block a later status request. harness selects the
// instructions. role picks the tool set: a parent delegates, a child hands
// back.
func runWithOptions(reader io.Reader, writer io.Writer, socket string, harness *core.HarnessKind, role core.SubagentMcpRole, options ParentTools) error {
	panic("unported: mj-worker/src/subagent_mcp.rs run_with_options")
}

// callParams is one tools/call request's params.
type callParams struct {
	Name string `json:"name"`
	// Arguments is nil when absent (serde default: Value::Null).
	Arguments json.RawMessage `json:"arguments" serde:"default"`
}

// UnmarshalJSON requires name, as serde does.
func (p *callParams) UnmarshalJSON(data []byte) error {
	type plain callParams
	return subagentMCPDecodeStrict(data, (*plain)(p), false)
}

// spawnArgs rejects obsolete arguments rather than silently discarding
// assignment text.
type spawnArgs struct {
	NoGithubCredentials bool   `json:"no_github_credentials" serde:"default"`
	TaskName            string `json:"task_name"`
	Instructions        string `json:"instructions"`
	// TaskType is required when the subtle type is offered. Without it, the
	// spawn tool has no task type and a call that omits one is
	// straightforward.
	TaskType         *core.SubagentTaskType `json:"task_type"`
	WorkingDirectory string                 `json:"working_directory" serde:"default"`
}

func (a *spawnArgs) UnmarshalJSON(data []byte) error {
	type plain spawnArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type childArgs struct {
	ChildSessionID string  `json:"child_session_id"`
	Message        *string `json:"message"`
}

func (a *childArgs) UnmarshalJSON(data []byte) error {
	type plain childArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), false)
}

type sendMessageArgs struct {
	SessionID string `json:"session_id"`
	Message   string `json:"message"`
}

func (a *sendMessageArgs) UnmarshalJSON(data []byte) error {
	type plain sendMessageArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type fetchArgs struct {
	SessionID string `json:"session_id"`
	Path      string `json:"path"`
	Ls        bool   `json:"ls" serde:"default"`
}

func (a *fetchArgs) UnmarshalJSON(data []byte) error {
	type plain fetchArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type handbackArgs struct {
	Message string `json:"message"`
}

func (a *handbackArgs) UnmarshalJSON(data []byte) error {
	type plain handbackArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

// callWithOptions answers one mj-agents tool call. params is nil when the
// request had none.
func callWithOptions(socket string, harness *core.HarnessKind, role core.SubagentMcpRole, params json.RawMessage, progress *Progress, options ParentTools) (any, bool, error) {
	options.Oracle = options.Oracle || oracleEnabled(socket)
	budget := func(action core.SubagentToolAction) time.Duration { return replyTimeoutFor(action, harness) }
	return callWithBudgetAndOptions(socket, harness, role, params, progress, budget, options)
}

// callWithBudgetAndOptions answers one tool call, waiting for the worker as
// long as budget allows for the call's action.
func callWithBudgetAndOptions(socket string, harness *core.HarnessKind, role core.SubagentMcpRole, params json.RawMessage, progress *Progress, budget func(core.SubagentToolAction) time.Duration, options ParentTools) (any, bool, error) {
	panic("unported: mj-worker/src/subagent_mcp.rs call_with_budget_and_options")
}

// subagentMCPExecute is Rust's subagent_mcp::execute (prefixed:
// move_workspace's execute keeps Execute). It sends one action to the
// worker's request queue and turns its answer into what the model reads.
// Both MCP servers use it, so every tool shares one path to the daemon.
func subagentMCPExecute(socket string, action core.SubagentToolAction, progress *Progress, budget func(core.SubagentToolAction) time.Duration) (any, bool, error) {
	panic("unported: mj-worker/src/subagent_mcp.rs execute")
}

// send sends the request and waits for the worker's answer, reporting
// progress while it is outstanding. answered is false when the worker did
// not answer within timeout.
//
// The exchange runs on its own goroutine so this one can keep reporting: a
// harness can abandon a silent request before a long operation finishes.
func send(socket string, request *core.SubagentToolRequest, timeout time.Duration, progress *Progress) (reply any, answered bool, err error) {
	panic("unported: mj-worker/src/subagent_mcp.rs send")
}

// spawnTwoTaskTypes is how spawn opens when the parent chooses between two
// task types.
const spawnTwoTaskTypes = "Start a child in your target and filesystem using the task type's configured model and effort. Set task_type to `straightforward` for work with a clear method, such as locating code, running commands or test suites, or implementing a design you have settled. Use `subtle` for work that needs judgment, such as an ambiguous diagnosis, a design choice, or a change with high risk of subtle bugs. A subtle task gets a smarter, more expensive agent. Judge by difficulty, not by the length of the brief. The user configures the model and effort for each type."

// spawnOneTaskType is how spawn opens when only the straightforward type is
// configured: the parent delegates straightforward work and keeps work
// needing judgment.
const spawnOneTaskType = "Start a child in your target and filesystem using the configured model and effort. Delegate straightforward work: work with a clear method, such as locating code, running commands or test suites, or implementing a design you have settled. Do work that needs judgment yourself, such as an ambiguous diagnosis, a design choice, or a change with high risk of subtle bugs. Judge by difficulty, not by the length of the brief. The user configures the model and effort."

// spawnTool is the parent's spawn tool. With only the straightforward type
// configured there is no choice to make, so it has no task_type.
func spawnTool(subtleSubagents bool) map[string]any {
	panic("unported: mj-worker/src/subagent_mcp.rs spawn_tool")
}

// toolDefinitionsWithOptions lists a parent's tools.
func toolDefinitionsWithOptions(_ *core.HarnessKind, options ParentTools) []map[string]any {
	panic("unported: mj-worker/src/subagent_mcp.rs tool_definitions_with_options")
}

// oracleEnabled is whether the controller offered this session the oracle.
// Like the other target-side checks, it reads the launch file as plain
// JSON, so neither a field it does not know nor one an older file still
// carries can hide the tool.
func oracleEnabled(socket string) bool {
	data, err := os.ReadFile(filepath.Join(filepath.Dir(socket), "launch.json"))
	if err != nil {
		return false
	}
	launch, err := subagentMCPDecodeValue(data)
	if err != nil {
		return false
	}
	object, _ := launch.(map[string]any)
	return object["oracle"] == true
}

func askOracleTool() map[string]any {
	return tool(
		"ask_oracle",
		"Consult a strong read-only advisor about a decision before you commit to it. Consult when a choice is hard to reverse or affects more than your change, and you cannot state what it must satisfy, why your choice satisfies each requirement (beyond \"tests pass\"), and what would fail if you were wrong. Also consult before giving up, escalating to a human, or a third attempt at something that failed twice. Check facts yourself first; never use it for routine work. Send the decision, options, and evidence. The oracle remembers your earlier questions and its answers, so a follow-up can build on them.",
		map[string]any{
			"type":                 "object",
			"properties":           map[string]any{"message": map[string]any{"type": "string", "description": "The decision, the options and your evidence, with file, symbol and line pointers. The oracle reads the shared workspace but not your conversation."}},
			"required":             []any{"message"},
			"additionalProperties": false,
		},
	)
}

type askOracleArgs struct {
	Message string `json:"message"`
}

func (a *askOracleArgs) UnmarshalJSON(data []byte) error {
	type plain askOracleArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

// reviewToolDefinitions are the tools of the separate mj-review server.
func reviewToolDefinitions() []map[string]any {
	panic("unported: mj-worker/src/subagent_mcp.rs review_tool_definitions")
}

type requestReviewArgs struct {
	Path *string `json:"path"`
}

func (a *requestReviewArgs) UnmarshalJSON(data []byte) error {
	type plain requestReviewArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type overrideReviewArgs struct {
	Overrides []overrideEntry `json:"overrides"`
}

func (a *overrideReviewArgs) UnmarshalJSON(data []byte) error {
	type plain overrideReviewArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type overrideEntry struct {
	Finding string `json:"finding"`
	Reason  string `json:"reason"`
}

func (e *overrideEntry) UnmarshalJSON(data []byte) error {
	type plain overrideEntry
	return subagentMCPDecodeStrict(data, (*plain)(e), true)
}

type setReviewBaselineArgs struct {
	Commit string  `json:"commit"`
	Path   *string `json:"path"`
}

func (a *setReviewBaselineArgs) UnmarshalJSON(data []byte) error {
	type plain setReviewBaselineArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

// reviewAction builds the action for one review tool call, refusing empty
// values.
func reviewAction(name string, arguments json.RawMessage) (core.SubagentToolAction, error) {
	panic("unported: mj-worker/src/subagent_mcp.rs review_action")
}

// callReview answers one call to the review server.
func callReview(socket string, params json.RawMessage, progress *Progress, budget func(core.SubagentToolAction) time.Duration) (any, bool, error) {
	if params == nil {
		return nil, false, errors.New("missing params")
	}
	var call callParams
	if err := json.Unmarshal(params, &call); err != nil {
		return nil, false, err
	}
	if !slices.Contains(core.ReviewMCPTools, call.Name) {
		return nil, false, fmt.Errorf("unknown review tool %s", strconv.Quote(call.Name))
	}
	action, err := reviewAction(call.Name, call.Arguments)
	if err != nil {
		return nil, false, err
	}
	return subagentMCPExecute(socket, action, progress, budget)
}

// RunReviewMCPStdio serves the before-push review tools over MCP stdio as
// mj-review. The tools reach the controller through the same worker queue
// as mj-agents.
func RunReviewMCPStdio(socket string, harness *core.HarnessKind, role core.SubagentMcpRole) error {
	return runReview(os.Stdin, os.Stdout, socket, harness, role)
}

func runReview(reader io.Reader, writer io.Writer, socket string, harness *core.HarnessKind, _ core.SubagentMcpRole) error {
	panic("unported: mj-worker/src/subagent_mcp.rs run_review")
}

func sendMessageTool() map[string]any {
	return tool(
		"send_message",
		"Send a message to another Mjolnir session, including managed children, by `session_id`, using its full ID or a unique case-insensitive hexadecimal prefix of at least 8 characters. Mjolnir refuses your own session, an unknown or destroyed target, and a stopped (suspended) target. A parked child is restarted before delivery. The parked child starts a turn and can take tens of seconds. Peer mailbox delivery requires a protocol-36 worker. Supported mid-turn harnesses, including OpenAI-backed Codex, deliver to a busy recipient at the next tool boundary without turn cancellation. Codex using the DeepSeek API receives the message after the current turn ends to preserve provider tool-call ordering. An idle recipient wakes. When mailbox delivery is unavailable, Mjolnir queues a turn. A session that is still starting accepts the message and queues it until startup ends. If startup fails, you are told the message was not delivered. The response includes the resolved full session ID and delivery timing when the worker reports it. The recipient sees your full session ID and a reply instruction; they can use send_message with that ID or its unique prefix. Do not poll, and do not resend an acknowledged message.",
		map[string]any{
			"type": "object",
			"properties": map[string]any{
				"session_id": map[string]any{"type": "string", "description": "Full ID or unique case-insensitive hexadecimal prefix (at least 8 characters) of a Mjolnir session, including managed children."},
				"message":    map[string]any{"type": "string"},
			},
			"required":             []any{"session_id", "message"},
			"additionalProperties": false,
		},
	)
}

func fetchTool() map[string]any {
	return readOnly(tool(
		"fetch",
		"Copy a file or directory from a Mjolnir session's target into your target's temporary directory, like `mj fetch`. Returns the local path, source path, kind and byte count; directories are saved as zip archives. Set ls to true to return a structured listing with sizes and modification times instead. Relative paths resolve in the source session's working directory; absolute paths must lie in its workspace, sub-agent reports or temporary directory. Symlinks cannot escape those boundaries.",
		map[string]any{"type": "object", "properties": map[string]any{
			"session_id": map[string]any{"type": "string", "description": "Source session's full ID."},
			"path":       map[string]any{"type": "string", "description": "File or directory on the source session's target."},
			"ls":         map[string]any{"type": "boolean", "description": "List the path instead of copying its content.", "default": false},
		}, "required": []any{"session_id", "path"}, "additionalProperties": false},
	))
}

// childToolDefinitions keeps handback first so the report budget can
// annotate it at launch.
func childToolDefinitions() []map[string]any {
	message := fmt.Sprintf("Your report, at most %d characters for workers or %d for oracles and reviewers; details go in files in your report directory.",
		core.SubagentKindWorker.MaxHandbackChars(), core.SubagentKindOracle.MaxHandbackChars())
	return []map[string]any{
		readOnly(tool(
			"handback",
			"Hand back once, as your last action. Repeats in one turn are refused. "+core.HandbackReportRules,
			map[string]any{"type": "object", "properties": map[string]any{"message": map[string]any{"type": "string", "description": message}}, "required": []any{"message"}, "additionalProperties": false},
		)),
		fetchTool(),
		sendMessageTool(),
	}
}

func tool(name, description string, inputSchema map[string]any) map[string]any {
	return map[string]any{"name": name, "description": description, "inputSchema": inputSchema}
}

// readOnly marks a tool that changes nothing. Claude's plan mode asks before
// every MCP tool without this hint, even one its allowed tools list names.
func readOnly(tool map[string]any) map[string]any {
	tool["annotations"] = map[string]any{"readOnlyHint": true}
	return tool
}

// subagentMCPDecodeValue is serde_json::from_slice::<Value>: one JSON value
// with nothing after it, numbers kept exact as json.Number.
func subagentMCPDecodeValue(data []byte) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil, err
	}
	if _, err := decoder.Token(); err != io.EOF {
		return nil, errors.New("trailing characters")
	}
	return value, nil
}

// subagentMCPDecodeStrict decodes the JSON object data into target (a
// pointer to a struct without its own UnmarshalJSON) as serde's derived
// Deserialize does: with deny, the first member in input order that is not
// a field is refused; then the first missing required field in declaration
// order. A field is optional when it is a pointer, omitempty, or tagged
// serde:"default".
func subagentMCPDecodeStrict(data []byte, target any, deny bool) error {
	known, required := subagentMCPFields(reflect.TypeOf(target))
	decoder := json.NewDecoder(bytes.NewReader(data))
	open, err := decoder.Token()
	if err != nil {
		return err
	}
	if delim, ok := open.(json.Delim); !ok || delim != '{' {
		return fmt.Errorf("invalid type: %s, expected a map", strings.TrimSpace(string(data)))
	}
	present := map[string]bool{}
	for decoder.More() {
		key, err := decoder.Token()
		if err != nil {
			return err
		}
		name, _ := key.(string)
		if deny && !slices.Contains(known, name) {
			return fmt.Errorf("unknown field `%s`, %s", name, subagentMCPExpected(known))
		}
		present[name] = true
		var skipped json.RawMessage
		if err := decoder.Decode(&skipped); err != nil {
			return err
		}
	}
	for _, name := range required {
		if !present[name] {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return json.Unmarshal(data, target)
}

// subagentMCPFields lists a struct type's JSON member names and the
// required ones, following the struct-tag rules of subagentMCPDecodeStrict.
func subagentMCPFields(t reflect.Type) (known, required []string) {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	for i := 0; i < t.NumField(); i++ {
		field := t.Field(i)
		name, options, _ := strings.Cut(field.Tag.Get("json"), ",")
		if name == "-" || !field.IsExported() {
			continue
		}
		if name == "" {
			name = field.Name
		}
		known = append(known, name)
		optional := field.Type.Kind() == reflect.Pointer || strings.Contains(options, "omitempty") || field.Tag.Get("serde") == "default"
		if !optional {
			required = append(required, name)
		}
	}
	return known, required
}

// subagentMCPExpected is serde's list of the fields an unknown one is not.
func subagentMCPExpected(names []string) string {
	quoted := make([]string, len(names))
	for i, name := range names {
		quoted[i] = "`" + name + "`"
	}
	switch len(quoted) {
	case 0:
		return "there are no fields"
	case 1:
		return "expected " + quoted[0]
	case 2:
		return "expected " + quoted[0] + " or " + quoted[1]
	}
	return "expected one of " + strings.Join(quoted, ", ")
}
