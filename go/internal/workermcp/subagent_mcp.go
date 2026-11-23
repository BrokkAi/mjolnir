// Port of mj-worker-mcp/src/subagent_mcp.rs.
//
// Mjolnir-owned delegation tools for Claude, Codex, DSH, and OpenCode
// sessions.
//
// JSON values (serde_json::Value) are github.com/BrokkAi/acp-go/jsonvalue
// values, as mcp_stdio's are: the Rust build's serde_json keeps object
// members in insertion order (preserve_order, which the ACP crate turns on),
// and jsonvalue.Object does the same, so the tools and answers this server
// writes have Rust's member order.

package workermcp

import (
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
	daemon, ok := subagentMCPGet(reply, "daemon")
	if !ok {
		return nil
	}
	data, err := jsonvalue.Marshal(daemon)
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
	case core.SubagentToolActionListAgents, core.SubagentToolActionListTargets, core.SubagentToolActionListProfiles:
		return "It changes nothing, so call it again later."
	case core.SubagentToolActionSpawn, core.SubagentToolActionSpawnOrchestrator:
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
			return subagentObject(
				"request_id", requestID,
				"accepted", true,
				"status", "not_picked_up",
				"error", reason+" "+notPickedUpAdvice(action),
			), true
		}
	}
	status := "Mjolnir has not answered this request yet."
	if daemon != nil {
		status = "Mjolnir picked up this request but has not finished it."
	}
	return subagentObject(
		"request_id", requestID,
		"accepted", true,
		"note", status+" "+unansweredAdvice(action),
	), false
}

// ReplyTimeout is how long an action may take to be answered. The daemon
// must notice the queued request, run the action (a spawn may provision a
// child session) and complete it back to the worker. The worker answers a
// little sooner by itself, so it can say whether the daemon picked it up.
const ReplyTimeout = 120 * time.Second

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
func stillClosingReply(requestID, childSessionID, reason string) jsonvalue.Object {
	return subagentObject(
		"request_id", requestID,
		"child_session_id", childSessionID,
		"closed", false,
		"status", "still_closing",
		"note", reason,
		"next_action", "Use list_agents to observe all of your children. This child may remain in state stopping until teardown reaches stopped. Do not spawn a replacement child before then.",
	)
}

// replyTimeoutFor is Rust's reply_timeout (renamed so it does not read as
// the constant REPLY_TIMEOUT, Go's ReplyTimeout): calls use the same bounded
// reply window.
func replyTimeoutFor(_ core.SubagentToolAction, _ *core.HarnessKind) time.Duration {
	return ReplyTimeout
}

// unansweredReply explains an unconfirmed close or a request with no reply.
func unansweredReply(requestID string, action core.SubagentToolAction, waited time.Duration) (any, bool) {
	seconds := uint64(waited / time.Second)
	if closing, ok := action.(core.SubagentToolActionCloseAgent); ok {
		return stillClosingReply(requestID, closing.ChildSessionID, fmt.Sprintf(
			"Mjolnir did not confirm this close within %d seconds. The close may still be running, so this child is not known to be gone.",
			seconds)), false
	}
	return subagentObject(
		"request_id", requestID,
		"error", fmt.Sprintf("Mjolnir did not answer this request within %d seconds. %s", seconds, unansweredAdvice(action)),
	), true
}

// modelFacing is what the model reads for one answer. The daemon and the
// worker both put the answer's own JSON into SubagentToolResult.Message, so
// handing the envelope back would leave the model parsing JSON out of a
// string inside a wrapper. Unwrap it, keeping request_id alongside the
// answer.
func modelFacing(requestID string, result any) any {
	messageValue, _ := subagentMCPGet(result, "message")
	message, ok := messageValue.(string)
	if !ok {
		return result
	}
	payload, err := jsonvalue.Parse([]byte(message))
	answer, isObject := payload.(jsonvalue.Object)
	if err != nil || !isObject {
		return result
	}
	if _, ok := answer.Get("request_id"); !ok {
		answer = append(answer, jsonvalue.Member{Key: "request_id", Value: requestID})
	}
	return answer
}

// ParentTools is what a parent's tool set offers, from the worker's launch
// configuration.
type ParentTools struct {
	Oracle bool
	// SubtleSubagents is whether spawn offers the subtle task type. Without
	// it, spawn has no task_type and every child uses the straightforward
	// choice.
	SubtleSubagents bool
	// OrchestratorsEnabled is whether a top-level parent can start
	// standalone orchestrators.
	OrchestratorsEnabled bool
}

// DefaultParentTools is Rust's ParentTools::default().
func DefaultParentTools() ParentTools {
	return ParentTools{Oracle: false, SubtleSubagents: true, OrchestratorsEnabled: false}
}

// childReportBudget: every harness already supplies the worker socket. Its
// adjacent launch file carries controller-authored settings even when the
// harness filters its env.
type childReportBudget struct {
	directory string
	limit     uint64
}

// subagentMCPLaunchFile is Rust's socket.with_file_name("launch.json").
func subagentMCPLaunchFile(socket string) string {
	return filepath.Join(filepath.Dir(socket), "launch.json")
}

// childReportBudgetFromLaunch reads the budget from the launch settings;
// nil when an older controller configured no budget.
func childReportBudgetFromLaunch(launch any) (*childReportBudget, error) {
	environment, _ := subagentMCPGet(launch, "environment")
	directoryValue, _ := subagentMCPGet(environment, core.ChildReportDirEnv)
	limitValue, _ := subagentMCPGet(environment, core.ChildReportLimitEnv)
	directory, hasDirectory := directoryValue.(string)
	limitText, hasLimit := limitValue.(string)
	if !hasDirectory || !hasLimit {
		// Older controllers did not configure a report budget.
		return nil, nil
	}
	limit, err := strconv.ParseUint(strings.TrimPrefix(limitText, "+"), 10, 64)
	if err != nil {
		return nil, fmt.Errorf("parse child report byte limit: %w", err)
	}
	return &childReportBudget{directory: directory, limit: limit}, nil
}

func (b *childReportBudget) note() string {
	return fmt.Sprintf("Report directory limit: %d bytes.", b.limit)
}

// check refuses a handback whose report directory holds more than the
// limit, naming the largest files.
func (b *childReportBudget) check() error {
	type sizedFile struct {
		size uint64
		path string
	}
	pending := []string{b.directory}
	total := uint64(0)
	var largest []sizedFile
	for len(pending) > 0 {
		path := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		info, err := os.Lstat(path)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return fmt.Errorf("inspect report %s: %w", path, err)
		}
		switch {
		case info.IsDir():
			entries, err := os.ReadDir(path)
			if err != nil {
				return fmt.Errorf("read report directory %s: %w", path, err)
			}
			for _, entry := range entries {
				pending = append(pending, filepath.Join(path, entry.Name()))
			}
		case info.Mode().IsRegular():
			size := uint64(info.Size())
			total = subagentSaturatingAdd(total, size)
			largest = append(largest, sizedFile{size: size, path: path})
			slices.SortFunc(largest, func(a, b sizedFile) int {
				if order := cmp.Compare(b.size, a.size); order != 0 {
					return order
				}
				return slices.Compare(subagentPathComponents(a.path), subagentPathComponents(b.path))
			})
			largest = largest[:min(len(largest), 10)]
		}
		// Lstat never follows links, including directory links.
	}
	if total <= b.limit {
		return nil
	}
	lines := make([]string, 0, len(largest))
	for _, file := range largest {
		shown := file.path
		if relative, err := filepath.Rel(b.directory, file.path); err == nil && subagentPathStartsWith(file.path, b.directory) {
			shown = relative
		}
		lines = append(lines, fmt.Sprintf("  %d bytes: %s", file.size, shown))
	}
	return fmt.Errorf("Handback refused: report directory %s holds %d bytes; limit is %d bytes. Largest files:\n%s\nDelete build outputs, binaries and downloads; keep only text evidence (logs, tables, findings), then call handback again.",
		b.directory, total, b.limit, strings.Join(lines, "\n"))
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
	role = core.SubagentMcpRole(role.ID())
	launch, hasLaunch, err := readLaunchSettings(socket)
	if err != nil {
		return err
	}
	var reportBudget *childReportBudget
	if hasLaunch && (role == core.SubagentMcpRoleChild || role == core.SubagentMcpRoleOrchestrator) {
		budget, err := childReportBudgetFromLaunch(launch)
		if err != nil {
			return err
		}
		reportBudget = budget
	}
	if hasLaunch {
		if oracle, _ := subagentMCPGet(launch, "oracle"); oracle == true {
			options.Oracle = true
		}
		if enabled, _ := subagentMCPGet(launch, "supervisors_enabled"); enabled == true {
			options.OrchestratorsEnabled = true
		}
	}
	var reviewRole *core.ReviewChildRole
	if reviewer, ok := subagentMCPGet(launch, "reviewer_launch"); ok && reviewer != nil {
		chosen := core.ReviewChildRoleReviewer
		if value, ok := subagentMCPGet(reviewer, "role"); ok {
			data, err := jsonvalue.Marshal(value)
			if err == nil {
				err = json.Unmarshal(data, &chosen)
			}
			if err != nil {
				return fmt.Errorf("read review child MCP output contract: %w", err)
			}
		}
		reviewRole = &chosen
	}
	tools := roleToolDefinitions(role, options)
	if index := slices.IndexFunc(tools, func(tool jsonvalue.Object) bool {
		name, _ := tool.Get("name")
		return name == "handback"
	}); index >= 0 {
		if reportBudget != nil {
			description, _ := tools[index].Get("description")
			text, _ := description.(string)
			tools[index].Set("description", text+" "+reportBudget.note())
		}
		if reviewRole != nil {
			instructions := core.ReviewChildOutputContract(*reviewRole)
			tools[index].Set("description", instructions)
			tools[index] = subagentSetPath(tools[index], instructions, "inputSchema", "properties", "message", "description")
		}
	}
	listed := make([]any, len(tools))
	for index, tool := range tools {
		listed[index] = tool
	}
	budget := func(action core.SubagentToolAction) time.Duration { return replyTimeoutFor(action, harness) }
	return Serve(reader, writer, MCPServer{
		Name:             core.SubagentMCPServer,
		Tools:            listed,
		ProgressInterval: ProgressInterval,
		Call: func(params any, present bool, progress *Progress) (any, bool, error) {
			raw, err := subagentMCPParams(params, present)
			if err != nil {
				return nil, false, err
			}
			return callWithBudgetAndOptions(socket, role, raw, progress, budget, options, reportBudget)
		},
	})
}

// subagentMCPParams is a tools/call request's params as JSON text; nil
// when the request had none.
func subagentMCPParams(params any, present bool) (json.RawMessage, error) {
	if !present {
		return nil, nil
	}
	return jsonvalue.Marshal(params)
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

// arguments is the call's arguments as JSON text, null when absent.
func (p *callParams) arguments() json.RawMessage {
	if len(p.Arguments) == 0 {
		return json.RawMessage("null")
	}
	return p.Arguments
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
	TaskType         *core.WorkerTaskType `json:"task_type"`
	WorkingDirectory string               `json:"working_directory" serde:"default"`
}

func (a *spawnArgs) UnmarshalJSON(data []byte) error {
	type plain spawnArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

// spawnOrchestratorArgs: Target and Repos are nil when not given.
type spawnOrchestratorArgs struct {
	TaskName            string    `json:"task_name"`
	Instructions        string    `json:"instructions"`
	Target              *string   `json:"target"`
	Repos               *[]string `json:"repos"`
	NoGithubCredentials bool      `json:"no_github_credentials" serde:"default"`
}

func (a *spawnOrchestratorArgs) UnmarshalJSON(data []byte) error {
	type plain spawnOrchestratorArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

// listTargetsArgs takes no arguments and refuses any.
type listTargetsArgs struct{}

func (a *listTargetsArgs) UnmarshalJSON(data []byte) error {
	type plain listTargetsArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type childArgs struct {
	ChildSessionID string `json:"child_session_id"`
}

func (a *childArgs) UnmarshalJSON(data []byte) error {
	type plain childArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
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

// callWithBudgetAndOptions answers one mj-agents tool call, waiting for the
// worker as long as budget allows for the call's action. params is nil when
// the request had none; reportBudget is nil when no report budget applies.
func callWithBudgetAndOptions(socket string, role core.SubagentMcpRole, params json.RawMessage, progress *Progress, budget func(core.SubagentToolAction) time.Duration, options ParentTools, reportBudget *childReportBudget) (any, bool, error) {
	if params == nil {
		return nil, false, errors.New("missing params")
	}
	var call callParams
	if err := json.Unmarshal(params, &call); err != nil {
		return nil, false, err
	}
	role = core.SubagentMcpRole(role.ID())
	if !slices.Contains(role.ToolNames(options.Oracle, options.OrchestratorsEnabled), call.Name) {
		return nil, false, fmt.Errorf("unknown sub-agent tool %s for %s", strconv.Quote(call.Name), role.ID())
	}
	arguments := call.arguments()
	var action core.SubagentToolAction
	switch call.Name {
	case "fetch":
		var args fetchArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		action = core.SubagentToolActionFetch{SessionID: args.SessionID, Path: args.Path, Ls: args.Ls}
	case "spawn_worker":
		for _, key := range []string{"files", "context"} {
			if subagentMCPHasMember(arguments, key) {
				return nil, false, errors.New("spawn no longer accepts files or context; put context and file, symbol, line-range or earlier-report pointers in instructions")
			}
		}
		var args spawnArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		instructions := strings.TrimSpace(args.Instructions)
		if instructions == "" {
			return nil, false, errors.New("spawn instructions cannot be empty")
		}
		// An absolute path stands alone, so it still works after the
		// parent's own directory was deleted or renamed.
		resolved := args.WorkingDirectory
		if !filepath.IsAbs(resolved) {
			parent, err := os.Getwd()
			if err != nil {
				return nil, false, fmt.Errorf("the parent's working directory is gone; pass an absolute working_directory: %w", err)
			}
			if args.WorkingDirectory == "" {
				resolved = parent
			} else {
				resolved = core.Join(parent, args.WorkingDirectory)
			}
		}
		if info, err := os.Stat(resolved); err != nil || !info.IsDir() {
			return nil, false, fmt.Errorf("spawn working directory %s does not exist or is not a directory; pass an existing directory (relative paths resolve against the parent's working directory)", resolved)
		}
		// A subtle request without a subtle choice still reaches the
		// daemon: the configuration may have gained one since launch.
		var taskType core.WorkerTaskType
		switch {
		case args.TaskType != nil:
			taskType = *args.TaskType
		case !options.SubtleSubagents:
			taskType = core.WorkerTaskTypeStraightforward
		default:
			return nil, false, errors.New("missing field `task_type`")
		}
		action = core.SubagentToolActionSpawn{
			NoGithubCredentials: args.NoGithubCredentials,
			TaskName:            args.TaskName,
			Instructions:        instructions,
			TaskType:            taskType.SubagentTaskType(),
			WorkingDirectory:    args.WorkingDirectory,
			// Legacy accepted requests keep context and files on the wire;
			// new calls omit them.
		}
	case "spawn_orchestrator":
		var args spawnOrchestratorArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		instructions := strings.TrimSpace(args.Instructions)
		if instructions == "" {
			return nil, false, errors.New("spawn_orchestrator instructions cannot be empty")
		}
		action = core.SubagentToolActionSpawnOrchestrator{
			TaskName:            args.TaskName,
			Instructions:        instructions,
			Target:              args.Target,
			Repos:               args.Repos,
			NoGithubCredentials: args.NoGithubCredentials,
		}
	case "ask_oracle":
		var args askOracleArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		action = core.SubagentToolActionAskOracle{Message: args.Message}
	case "list_agents":
		action = core.SubagentToolActionListAgents{}
	case "list_targets":
		if !bytes.Equal(bytes.TrimSpace(arguments), []byte("null")) {
			var args listTargetsArgs
			if err := json.Unmarshal(arguments, &args); err != nil {
				return nil, false, err
			}
		}
		action = core.SubagentToolActionListTargets{}
	case "send_message":
		if subagentMCPHasMember(arguments, "child_session_id") {
			return nil, false, errors.New("unknown parameter `child_session_id`; use `session_id`")
		}
		var args sendMessageArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		action = core.SubagentToolActionSendMessage{ChildSessionID: args.SessionID, Message: args.Message}
	case "close":
		var args childArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		action = core.SubagentToolActionCloseAgent{ChildSessionID: args.ChildSessionID}
	case "handback":
		if reportBudget != nil {
			if err := reportBudget.check(); err != nil {
				return subagentObject("error", err.Error()), true, nil
			}
		}
		var args handbackArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, false, err
		}
		action = core.SubagentToolActionHandback{Message: args.Message}
	default:
		return nil, false, fmt.Errorf("unknown sub-agent tool %s", strconv.Quote(call.Name))
	}
	return subagentMCPExecute(socket, action, progress, budget)
}

// subagentMCPExecute is Rust's subagent_mcp::execute (prefixed:
// move_workspace's execute keeps Execute). It sends one action to the
// worker's request queue and turns its answer into what the model reads.
// Both MCP servers use it, so every tool shares one path to the daemon.
func subagentMCPExecute(socket string, action core.SubagentToolAction, progress *Progress, budget func(core.SubagentToolAction) time.Duration) (any, bool, error) {
	// Each call is its own request. The id keeps the worker's queue and the
	// daemon from running one request twice; it is never the model's to
	// set.
	requestID, err := core.NewSessionID()
	if err != nil {
		return nil, false, err
	}
	timeout := budget(action)
	request := core.SubagentToolRequest{
		RequestID:   requestID,
		CreatedAtMs: core.EpochMillis(),
		Action:      action,
	}
	reply, answered, err := send(socket, &request, timeout, progress)
	if err != nil {
		return nil, false, err
	}
	if !answered {
		value, isError := unansweredReply(requestID, request.Action, timeout)
		return value, isError, nil
	}
	if result, ok := subagentMCPGet(reply, "result"); ok && result != nil {
		isError := false
		if flag, ok := subagentMCPGet(result, "is_error"); ok {
			isError, _ = flag.(bool)
		}
		return modelFacing(requestID, result), isError, nil
	}
	daemon := daemonContactFromReply(reply)
	if closing, ok := request.Action.(core.SubagentToolActionCloseAgent); ok {
		reason := "Mjolnir took this close but has not confirmed it, so this child is not known to be gone."
		if daemon != nil {
			if notPicked, ok := daemon.notPickedUp(); ok {
				reason = notPicked + " The close is saved and runs when the daemon picks it up, so this child is not known to be gone."
			}
		}
		return stillClosingReply(requestID, closing.ChildSessionID, reason), false, nil
	}
	value, isError := pendingReply(requestID, request.Action, daemon)
	return value, isError, nil
}

// send sends the request and waits for the worker's answer, reporting
// progress while it is outstanding. answered is false when the worker did
// not answer within timeout.
//
// The exchange runs on its own goroutine so this one can keep reporting: a
// harness can abandon a silent request before a long operation finishes.
func send(socket string, request *core.SubagentToolRequest, timeout time.Duration, progress *Progress) (reply any, answered bool, err error) {
	started := time.Now()
	finished := make(chan subagentExchange, 1)
	go func() {
		var result subagentExchange
		defer func() {
			if recovered := recover(); recovered != nil {
				result = subagentExchange{err: errors.New("the sub-agent socket thread panicked")}
				slog.Error("the sub-agent socket exchange panicked", "panic", recovered)
			}
			finished <- result
		}()
		result = subagentExchangeRequest(socket, request, timeout)
	}()
	for {
		wait := time.NewTimer(progress.Interval())
		select {
		case result := <-finished:
			wait.Stop()
			return result.reply, result.answered, result.err
		case <-wait.C:
			progress.Notify(uint64(time.Since(started)/time.Second), nil, "Mjolnir is processing this request")
		}
	}
}

// subagentExchange is what one exchange with the worker gave: its answer,
// whether it answered in time, or why the exchange failed.
type subagentExchange struct {
	reply    any
	answered bool
	err      error
}

// subagentExchangeRequest is one request to the worker and, when the worker asks
// for it, the acknowledgment of its durable reply.
func subagentExchangeRequest(socket string, request *core.SubagentToolRequest, timeout time.Duration) (result subagentExchange) {
	raw, err := SocketRequest[json.RawMessage](socket, request, "sub-agent", timeout)
	if err != nil {
		result.err = err
		return result
	}
	if raw == nil {
		return result
	}
	reply, err := jsonvalue.Parse(*raw)
	if err != nil {
		result.err = err
		return result
	}
	result.reply, result.answered = reply, true
	if required, _ := subagentMCPGet(reply, "acknowledgment_required"); required == true {
		acknowledged, err := SocketRequest[json.RawMessage](socket,
			subagentObject("acknowledge_result", request.RequestID),
			"sub-agent reply acknowledgment", 2*time.Second)
		switch {
		case err != nil:
			slog.Warn("could not acknowledge sub-agent reply; its body remains durable", "error", err)
		case acknowledged == nil || !subagentMCPAccepted(*acknowledged):
			slog.Warn("sub-agent reply acknowledgment was not confirmed; its body remains durable")
		}
	}
	return result
}

// subagentMCPAccepted is whether a worker answer says accepted: true.
func subagentMCPAccepted(raw json.RawMessage) bool {
	reply, err := jsonvalue.Parse(raw)
	if err != nil {
		return false
	}
	accepted, _ := subagentMCPGet(reply, "accepted")
	return accepted == true
}

// spawnTwoTaskTypes is how spawn opens when the parent chooses between two
// task types.
const spawnTwoTaskTypes = "Start a child in your target and filesystem using the task type's configured model and effort. Use `task_type` `straightforward` for bounded work with one clear path: mapping code, running exact build, test, or gate commands, and small mechanical edits. Use `subtle` for multi-file implementation (even from a settled design), multi-stage operational runs or troubleshooting, ambiguous diagnosis, design choices, and work with a high risk of subtle bugs. A subtle task gets a smarter, more expensive agent. Judge by scope and branching, not brief length or command runtime. Escalate stalled or incomplete `straightforward` work to `subtle` after one correction."

// spawnOneTaskType is how spawn opens when only the straightforward type is
// configured: the parent delegates straightforward work and keeps work
// needing judgment.
const spawnOneTaskType = "Start a child in your target and filesystem using the configured model and effort. Delegate straightforward work: bounded tasks with a single clear path, including locating and mapping code, running exact build, test, or gate commands, and small mechanical edits. Keep multi-file implementation, troubleshooting, ambiguous diagnosis, design choices, and changes with a high risk of hard-to-detect bugs yourself. Judge by scope and branching, not brief length or command runtime."

// spawnTool is the parent's spawn tool. With only the straightforward type
// configured there is no choice to make, so it has no task_type.
func spawnTool(subtleSubagents bool) jsonvalue.Object {
	lead, selection := spawnOneTaskType, "the configured model and effort"
	if subtleSubagents {
		lead, selection = spawnTwoTaskTypes, "the task type's configured model and effort"
	}
	properties := subagentObject(
		"no_github_credentials", subagentObject("type", "boolean", "description", "Withhold GitHub credentials from this child and its descendants. A parent's credential opt-out is always inherited."),
		"task_name", subagentObject("type", "string"),
		"instructions", subagentObject("type", "string", "description", "The assignment, context, constraints and required evidence. Point to relevant files, symbols, line ranges and earlier reports; the child reads them in the shared filesystem."),
		"working_directory", subagentObject("type", "string", "description", "Launch directory on the parent's target. Absolute paths are used as-is; relative paths resolve against the parent's working directory. The directory must exist. Defaults to the parent's working directory."),
	)
	required := []any{"task_name", "instructions"}
	if subtleSubagents {
		// serde_json's `properties["task_type"] = ...` with preserve_order
		// appends the new member.
		properties.Set("task_type", subagentObject(
			"type", "string",
			"enum", []any{"straightforward", "subtle"},
			"description", "`straightforward` is for bounded work with a single clear path, such as locating and mapping code, running exact build, test, or gate commands, and small mechanical edits. `subtle` gets a smarter, more expensive agent for multi-file implementation, multi-stage operational runs or troubleshooting, ambiguous diagnosis, design choices, or work with a high risk of subtle bugs.",
		))
		required = append(required, "task_type")
	}
	return tool(
		"spawn_worker",
		lead+" Returns child_session_id and report_dir immediately; registration does not mean startup succeeded. Reports and startup errors arrive as messages from Mjolnir; use list_agents for status. Reports are short and point to files in report_dir. Supply the assignment and file, symbol, line-range or earlier-report pointers in instructions. Use send_message for follow-up work when an idle child's context helps. Mjolnir selects an enabled profile with the most quota, API-priced profiles last, supporting "+selection+". An unavailable model, effort or login is an error, never replaced by another selection. Only children holding processes count against the live children limit; finished children are parked. Close children you no longer need, including failed children after reading the error. Failed-start cleanup must finish before a replacement can use its slot. A child whose record is in error, lost or stopped cannot be re-prompted; a child whose last turn failed can: send_message resumes it. A login the provider has refused stays unavailable until repaired with `mj login`.",
		subagentObject(
			"type", "object",
			"properties", properties,
			"required", required,
			"additionalProperties", false,
		),
	)
}

// roleToolDefinitions lists the tools role offers with options, in the
// order core's ToolNames gives them.
func roleToolDefinitions(role core.SubagentMcpRole, options ParentTools) []jsonvalue.Object {
	names := role.ToolNames(options.Oracle, options.OrchestratorsEnabled)
	tools := make([]jsonvalue.Object, 0, len(names))
	for _, name := range names {
		var definition jsonvalue.Object
		switch name {
		case "spawn_worker":
			definition = spawnTool(options.SubtleSubagents)
		case "spawn_orchestrator":
			definition = tool(
				name,
				"Start an orchestrator in its own environment. Use list_targets to choose an enabled target. Omit target to use your target. Omit repos to inherit your project's bundle, image and tooling. Supply repos to create or reuse a project bundle for those URLs. Bare targets require an existing directory, so choose a container target for URL-only repos. Use list_agents for startup status and the report directory, then fetch remote files before closing. Close checkpoints tracked commits, discards untracked files, and tears down the container and report directory.",
				subagentObject(
					"type", "object",
					"properties", subagentObject(
						"task_name", subagentObject("type", "string"),
						"instructions", subagentObject("type", "string"),
						"target", subagentObject("type", "string"),
						"repos", subagentObject("type", "array", "items", subagentObject("type", "string")),
						"no_github_credentials", subagentObject("type", "boolean"),
					),
					"required", []any{"task_name", "instructions"},
					"additionalProperties", false,
				),
			)
		case "list_agents":
			definition = readOnly(tool(
				"list_agents",
				"List this parent's Mjolnir child sessions and status, including each resolved effort, pending_messages and recent message_deliveries. Each delivery says whether it used mailbox or turn routing. A failed delivery does not change the child's state. A child whose last turn failed and whose entry has promptable true can be resumed with send_message. A finished child has parked true when its worker was stopped; parking stops the harness and the processes under it, and a detached process may keep running. An entry has facts when Mjolnir recorded what the child's worker observed: facts.finish lists the processes the child's agent had left running and its working tree when its turn ended. The facts are Mjolnir's observation, not the child's claim; read them against its report. A refused login reports failure kind login_invalid; its output names `mj login` and is not about the task.",
				subagentObject("type", "object", "additionalProperties", false),
			))
		case "list_targets":
			definition = readOnly(tool(
				name,
				"List enabled execution targets, their default container sizes, and recent and hourly host capacity observations.",
				subagentObject("type", "object", "properties", subagentObject(), "additionalProperties", false),
			))
		case "close":
			definition = tool(
				name,
				"Stop a child by its full ID or unique hexadecimal prefix of at least 8 characters, retaining its conversation. Closing a standalone orchestrator checkpoints tracked commits, discards untracked files, and tears down the container and report directory. A borrowed worker's report directory is kept. Closed children cannot receive messages. If cleanup is pending, use list_agents to confirm the child is stopped before replacing it.",
				subagentObject(
					"type", "object",
					"properties", subagentObject("child_session_id", subagentObject("type", "string")),
					"required", []any{"child_session_id"},
					"additionalProperties", false,
				),
			)
		case "fetch":
			definition = fetchTool()
		case "send_message":
			definition = sendMessageTool()
		case "handback":
			definition = handbackTool()
		case "ask_oracle":
			definition = askOracleTool()
		default:
			panic("unknown capability " + name)
		}
		tools = append(tools, definition)
	}
	return tools
}

// readLaunchSettings reads the launch file beside socket as plain JSON, so
// fields from older or newer launch files are optional. present is false
// when there is no launch file.
func readLaunchSettings(socket string) (launch any, present bool, err error) {
	path := subagentMCPLaunchFile(socket)
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("read %s: %w", path, err)
	}
	launch, err = jsonvalue.Parse(data)
	if err != nil {
		return nil, false, fmt.Errorf("parse %s: %w", path, err)
	}
	return launch, true, nil
}

func askOracleTool() jsonvalue.Object {
	return tool(
		"ask_oracle",
		"Consult a strong read-only advisor about a decision before you commit to it. Consult when a choice is hard to reverse or affects more than your change, and you cannot state what it must satisfy, why your choice satisfies each requirement (beyond \"tests pass\"), and what would fail if you were wrong. Also consult before giving up, escalating to a human, or a third attempt at something that failed twice. Check facts yourself first; never use it for routine work. Send the decision, options, and evidence. The oracle remembers your earlier questions and its answers, so a follow-up can build on them.",
		subagentObject(
			"type", "object",
			"properties", subagentObject("message", subagentObject("type", "string", "description", "The decision, the options and your evidence, with file, symbol and line pointers. The oracle reads the shared workspace but not your conversation.")),
			"required", []any{"message"},
			"additionalProperties", false,
		),
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
func reviewToolDefinitions() []jsonvalue.Object {
	return []jsonvalue.Object{
		tool(
			"request_review",
			"Request review of committed work before pushing. Reviews HEAD of each worktree in `paths` together, in one review (default: the repository that contains your working directory). The verdict arrives later as a Mjolnir message.",
			subagentObject(
				"type", "object",
				"properties", subagentObject("paths", subagentObject(
					"type", "array",
					"minItems", 1,
					"items", subagentObject("type", "string"),
					"description", "Worktrees of workspace repositories to review together.",
				)),
				"additionalProperties", false,
			),
		),
		tool(
			"override_review",
			"Record before-push review findings you will not fix. Pass one entry per finding: the finding in your words and why you will not fix it. Call it as soon as the verdict arrives, before committing any change; it applies only to the exact reviewed commits and is refused once they change. Later reviews are told not to report these findings again. It allows pushing the reviewed commits, but does not move the review baseline. When more than one verdict is outstanding, pass `path`, a worktree that verdict covers.",
			subagentObject(
				"type", "object",
				"properties", subagentObject(
					"overrides", subagentObject(
						"type", "array",
						"minItems", 1,
						"items", subagentObject(
							"type", "object",
							"properties", subagentObject(
								"finding", subagentObject("type", "string", "description", "The finding, in enough words that a later reviewer recognizes it."),
								"reason", subagentObject("type", "string", "description", "Why this finding does not need fixing."),
							),
							"required", []any{"finding", "reason"},
							"additionalProperties", false,
						),
					),
					"path", subagentObject("type", "string", "description", "A worktree the outstanding verdict covers. Needed only when more than one verdict is outstanding."),
				),
				"required", []any{"overrides"},
				"additionalProperties", false,
			),
		),
		tool(
			"set_baseline",
			"Mark commits through `commit` as reviewed, so later reviews cover only later changes. Accepted only for a commit whose tree a review has passed, clean or by override_review. A clean review already does this. Pass `path` for another worktree.",
			subagentObject(
				"type", "object",
				"properties", subagentObject(
					"commit", subagentObject("type", "string", "description", "The reviewed commit; commits through it are marked reviewed."),
					"path", subagentObject("type", "string", "description", "A worktree of a workspace repository (default: the session working directory)."),
				),
				"required", []any{"commit"},
				"additionalProperties", false,
			),
		),
	}
}

type requestReviewArgs struct {
	Paths *[]string `json:"paths"`
}

func (a *requestReviewArgs) UnmarshalJSON(data []byte) error {
	type plain requestReviewArgs
	return subagentMCPDecodeStrict(data, (*plain)(a), true)
}

type overrideReviewArgs struct {
	Overrides []overrideEntry `json:"overrides"`
	Path      *string         `json:"path"`
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
	if len(arguments) == 0 {
		arguments = json.RawMessage("null")
	}
	switch name {
	case "request_review":
		if subagentMCPHasMember(arguments, "path") {
			return nil, errors.New("request_review now takes `paths`, a list of worktrees to review together")
		}
		var args requestReviewArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, err
		}
		if args.Paths != nil && len(*args.Paths) == 0 {
			return nil, errors.New("request_review paths cannot be empty; omit it for the repository of your working directory")
		}
		var paths []string
		if args.Paths != nil {
			paths = *args.Paths
		}
		if slices.ContainsFunc(paths, func(path string) bool { return strings.TrimSpace(path) == "" }) {
			return nil, errors.New("request_review paths cannot contain an empty path")
		}
		return core.SubagentToolActionRequestReview{Paths: paths}, nil
	case "override_review":
		if subagentMCPHasMember(arguments, "reason") && !subagentMCPHasMember(arguments, "overrides") {
			return nil, errors.New("override_review now takes `overrides`, a list of {finding, reason} entries, one per finding you will not fix")
		}
		var args overrideReviewArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, err
		}
		if len(args.Overrides) == 0 {
			return nil, errors.New("override_review needs at least one entry in `overrides`")
		}
		overrides := make([]core.ReviewOverride, 0, len(args.Overrides))
		for index, entry := range args.Overrides {
			number := index + 1
			finding := strings.TrimSpace(entry.Finding)
			if finding == "" {
				return nil, fmt.Errorf("override_review overrides[%d].finding cannot be empty", number)
			}
			reason := strings.TrimSpace(entry.Reason)
			if reason == "" {
				return nil, fmt.Errorf("override_review overrides[%d].reason cannot be empty", number)
			}
			overrides = append(overrides, core.ReviewOverride{Finding: finding, Reason: reason})
		}
		if args.Path != nil && strings.TrimSpace(*args.Path) == "" {
			return nil, errors.New("override_review path cannot be empty; omit it when one verdict is outstanding")
		}
		return core.SubagentToolActionOverrideReview{Overrides: overrides, Path: args.Path}, nil
	case "set_baseline":
		var args setReviewBaselineArgs
		if err := json.Unmarshal(arguments, &args); err != nil {
			return nil, err
		}
		commit := strings.TrimSpace(args.Commit)
		if commit == "" {
			return nil, errors.New("set_baseline commit cannot be empty")
		}
		if args.Path != nil && strings.TrimSpace(*args.Path) == "" {
			return nil, errors.New("set_baseline path cannot be empty; omit it for the session working directory")
		}
		return core.SubagentToolActionSetReviewBaseline{Commit: commit, Path: args.Path}, nil
	}
	return nil, fmt.Errorf("unknown review tool %s", strconv.Quote(name))
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
	action, err := reviewAction(call.Name, call.arguments())
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
	definitions := reviewToolDefinitions()
	tools := make([]any, len(definitions))
	for index, definition := range definitions {
		tools[index] = definition
	}
	return Serve(reader, writer, MCPServer{
		Name:             core.ReviewMCPServer,
		Tools:            tools,
		ProgressInterval: ProgressInterval,
		Call: func(params any, present bool, progress *Progress) (any, bool, error) {
			raw, err := subagentMCPParams(params, present)
			if err != nil {
				return nil, false, err
			}
			return callReview(socket, raw, progress, func(action core.SubagentToolAction) time.Duration {
				return replyTimeoutFor(action, harness)
			})
		},
	})
}

func sendMessageTool() jsonvalue.Object {
	return tool(
		"send_message",
		"Send a message to another Mjolnir session, including managed children, by `session_id`, using its full ID or a unique case-insensitive hexadecimal prefix of at least 8 characters. Mjolnir refuses your own session, an unknown or destroyed target, and a stopped (suspended) target. A parked child is restarted before delivery. The parked child starts a turn and can take tens of seconds. Peer mailbox delivery requires a protocol-36 worker. Supported mid-turn harnesses, including OpenAI-backed Codex, deliver to a busy recipient at the next tool boundary without turn cancellation. Codex using the DeepSeek API receives the message after the current turn ends to preserve provider tool-call ordering. An idle recipient wakes. When mailbox delivery is unavailable, Mjolnir queues a turn. A session that is still starting accepts the message and queues it until startup ends. If startup fails, you are told the message was not delivered. The response includes the resolved full session ID and delivery timing when the worker reports it. The recipient sees your full session ID and a reply instruction; they can use send_message with that ID or its unique prefix. Do not poll, and do not resend an acknowledged message.",
		subagentObject(
			"type", "object",
			"properties", subagentObject(
				"session_id", subagentObject("type", "string", "description", "Full ID or unique case-insensitive hexadecimal prefix (at least 8 characters) of a Mjolnir session, including managed children."),
				"message", subagentObject("type", "string"),
			),
			"required", []any{"session_id", "message"},
			"additionalProperties", false,
		),
	)
}

func fetchTool() jsonvalue.Object {
	return readOnly(tool(
		"fetch",
		"Copy a file or directory from a Mjolnir session's target into your target's temporary directory, like `mj fetch`. Returns the local path, source path, kind and byte count; directories are saved as zip archives. Set ls to true to return a structured listing with sizes and modification times instead. Relative paths resolve in the source session's working directory; absolute paths must lie in its workspace, sub-agent reports or temporary directory. Symlinks cannot escape those boundaries.",
		subagentObject(
			"type", "object",
			"properties", subagentObject(
				"session_id", subagentObject("type", "string", "description", "Source session's full ID."),
				"path", subagentObject("type", "string", "description", "File or directory on the source session's target."),
				"ls", subagentObject("type", "boolean", "description", "List the path instead of copying its content.", "default", false),
			),
			"required", []any{"session_id", "path"},
			"additionalProperties", false,
		),
	))
}

func handbackTool() jsonvalue.Object {
	message := fmt.Sprintf("Your report, at most %d characters for workers or %d for oracles and reviewers; details go in files in your report directory.",
		core.SubagentKindWorker.MaxHandbackChars(), core.SubagentKindOracle.MaxHandbackChars())
	return readOnly(tool(
		"handback",
		"Hand back once, as your last action. Repeats in one turn are refused. "+core.HandbackReportRules,
		subagentObject(
			"type", "object",
			"properties", subagentObject("message", subagentObject("type", "string", "description", message)),
			"required", []any{"message"},
			"additionalProperties", false,
		),
	))
}

func tool(name, description string, inputSchema jsonvalue.Object) jsonvalue.Object {
	return subagentObject("name", name, "description", description, "inputSchema", inputSchema)
}

// readOnly marks a tool that changes nothing. Claude's plan mode asks before
// every MCP tool without this hint, even one its allowed tools list names.
func readOnly(tool jsonvalue.Object) jsonvalue.Object {
	tool.Set("annotations", subagentObject("readOnlyHint", true))
	return tool
}

// subagentObject is serde_json's json!({...}) with preserve_order: an object
// whose members are the key and value pairs in order.
func subagentObject(pairs ...any) jsonvalue.Object {
	if len(pairs)%2 != 0 {
		panic("subagentObject needs key and value pairs")
	}
	object := make(jsonvalue.Object, 0, len(pairs)/2)
	for index := 0; index < len(pairs); index += 2 {
		object = append(object, jsonvalue.Member{Key: pairs[index].(string), Value: pairs[index+1]})
	}
	return object
}

// subagentSetPath is serde_json's `value[a][b]... = new`: it sets the member at
// keys inside object, creating objects along the way, and returns object.
func subagentSetPath(object jsonvalue.Object, value any, keys ...string) jsonvalue.Object {
	if len(keys) == 1 {
		object.Set(keys[0], value)
		return object
	}
	inner, _ := object.Get(keys[0])
	child, _ := inner.(jsonvalue.Object)
	if child == nil {
		child = jsonvalue.Object{}
	}
	object.Set(keys[0], subagentSetPath(child.Clone(), value, keys[1:]...))
	return object
}

// subagentMCPGet is serde_json's Value::get with a key: the member of an
// object, or false for any other value.
func subagentMCPGet(value any, key string) (any, bool) {
	switch object := value.(type) {
	case jsonvalue.Object:
		return object.Get(key)
	case map[string]any:
		member, ok := object[key]
		return member, ok
	}
	return nil, false
}

// subagentMCPHasMember is `arguments.get(key).is_some()` on JSON text.
func subagentMCPHasMember(arguments json.RawMessage, key string) bool {
	value, err := jsonvalue.Parse(arguments)
	if err != nil {
		return false
	}
	_, ok := subagentMCPGet(value, key)
	return ok
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

// subagentSaturatingAdd is u64::saturating_add.
func subagentSaturatingAdd(a, b uint64) uint64 {
	if sum := a + b; sum >= a {
		return sum
	}
	return ^uint64(0)
}

// subagentPathStartsWith is Rust's Path::starts_with: base's components are
// the leading components of path.
func subagentPathStartsWith(path, base string) bool {
	components, prefix := subagentPathComponents(path), subagentPathComponents(base)
	return len(prefix) <= len(components) && slices.Equal(components[:len(prefix)], prefix)
}

// subagentPathComponents is a path's components as Rust's Path::components
// yields them: the root as "/", "." dropped except at the start, ".." kept.
func subagentPathComponents(path string) []string {
	separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
	var components []string
	if path != "" && separator(rune(path[0])) {
		components = append(components, "/")
	}
	for index, part := range strings.FieldsFunc(path, separator) {
		if part == "." && !(index == 0 && components == nil && strings.HasPrefix(path, ".")) {
			continue
		}
		components = append(components, part)
	}
	return components
}
