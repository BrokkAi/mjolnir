// Port of mj-worker/src/acp/permissions.rs.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
	"sync"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	sitter "github.com/tree-sitter/go-tree-sitter"
	bash "github.com/tree-sitter/tree-sitter-bash/bindings/go"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PlanImplementationSlot holds the current command's plan implementation
// sender. A permission callback captures the slot, so a late answer cannot
// attach an implementation to a subsequent prompt or bridge. Share it by
// pointer; the zero value is an empty slot.
type PlanImplementationSlot = Shared[*planImplementationQueue]

// planImplementationQueue is Rust's unbounded mpsc channel of plan
// implementations: a send never blocks and fails once the prompt closed the
// channel, and the prompt's loop can wait for one in a select.
type planImplementationQueue struct {
	mu     sync.Mutex
	queue  []PlanImplementation
	closed bool
	// signal holds one token while the queue may have an implementation.
	signal chan struct{}
}

func newPlanImplementationQueue() *planImplementationQueue {
	return &planImplementationQueue{signal: make(chan struct{}, 1)}
}

// errPlanImplementationClosed is a send after the prompt closed its
// channel.
var errPlanImplementationClosed = errors.New("plan implementation channel closed")

func (q *planImplementationQueue) send(implementation PlanImplementation) error {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.closed {
		return errPlanImplementationClosed
	}
	q.queue = append(q.queue, implementation)
	select {
	case q.signal <- struct{}{}:
	default:
	}
	return nil
}

// close stops further sends; implementations already sent can still be
// taken, as tokio's Receiver::close leaves them.
func (q *planImplementationQueue) close() {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.closed = true
}

// ready yields when an implementation may be waiting; take then takes it.
func (q *planImplementationQueue) ready() <-chan struct{} {
	return q.signal
}

func (q *planImplementationQueue) take() (PlanImplementation, bool) {
	q.mu.Lock()
	defer q.mu.Unlock()
	if len(q.queue) == 0 {
		return PlanImplementation{}, false
	}
	implementation := q.queue[0]
	q.queue = q.queue[1:]
	if len(q.queue) > 0 {
		select {
		case q.signal <- struct{}{}:
		default:
		}
	}
	return implementation, true
}

// PlanImplementation is an approved plan on its way to the prompt that
// implements it. permissionSent delivers whether the plan permission answer
// reached the bridge; it is closed without a value when the answer was
// dropped.
type PlanImplementation struct {
	plan           string
	permissionSent <-chan bool
}

// ActivePlanImplementation is the current command's claim on the slot;
// release empties it (Rust's Drop).
type ActivePlanImplementation struct {
	slot *PlanImplementationSlot
}

// RestoredPlanMode is the session's modes once Plan was left for the
// session's execution policy.
type RestoredPlanMode struct {
	configOptions []schema.SessionConfigOption
	modes         *schema.SessionModeState
	plan          string
	// warning is set when Claude refused the policy mode and Plan was left
	// for its default mode instead.
	warning *string
}

func currentSessionMode(configOptions []schema.SessionConfigOption, modes *schema.SessionModeState) *string {
	for _, option := range configOptions {
		if option.Category != nil && *option.Category == schema.SessionConfigOptionCategoryMode {
			if value := core.ConfigCurrentValue(configOptions, string(option.ID)); value != nil {
				return value
			}
			break
		}
	}
	if value := core.ConfigCurrentValue(configOptions, "mode"); value != nil {
		return value
	}
	if modes != nil {
		mode := string(modes.CurrentModeID)
		return &mode
	}
	return nil
}

// PlanModeRestoration is a plan-mode restoration in flight.
type PlanModeRestoration = <-chan asyncResult[RestoredPlanMode]

func restorePlanExecutionMode(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, state RestoredPlanMode, permissionSent <-chan bool, enforcement *core.ExecutionEnforcement) (RestoredPlanMode, error) {
	sent := false
	select {
	case delivered, ok := <-permissionSent:
		sent = ok && delivered
	case <-ctx.Done():
		return RestoredPlanMode{}, ctx.Err()
	}
	if !sent {
		return RestoredPlanMode{}, errors.New("Claude's plan permission response could not be delivered")
	}
	warning, hasWarning, err := leavePlanForPolicy(ctx, connection, sessionID, core.HarnessKindClaude, enforcement,
		&state.configOptions, state.modes)
	if err != nil {
		return RestoredPlanMode{}, err
	}
	if hasWarning {
		state.warning = &warning
	} else {
		state.warning = nil
	}
	return state, nil
}

// PlanPermissionAnswer is how a plan permission request is answered.
type PlanPermissionAnswer interface {
	isPlanPermissionAnswer()
}

type PlanPermissionAnswerNative struct {
	Value schema.RequestPermissionResponse
}

type PlanPermissionAnswerContinueInBypass struct{}

func (PlanPermissionAnswerNative) isPlanPermissionAnswer()           {}
func (PlanPermissionAnswerContinueInBypass) isPlanPermissionAnswer() {}

// claudePolicyMode is the Claude permission mode the session's enforcement
// selects: the profile's `claude_permission_mode` (Auto by default) for
// Guardian, bypassPermissions for YOLO. It remains the fallback for a plan
// approval that did not enter through a recorded Mjolnir Plan control.
func claudePolicyMode(enforcement *core.ExecutionEnforcement) string {
	if enforcement != nil {
		if mode, ok := enforcement.ACPMode(); ok {
			return mode
		}
	}
	panic("Claude enforces an ACP mode under every execution policy")
}

// claudePlanOptionMode is the permission mode a Claude plan-approval option
// continues in, and whether it first clears the context. claude-agent-acp
// 0.84 names the options `exit-plan-*`; earlier bridges named them by the
// mode itself.
func claudePlanOptionMode(optionID string) (mode string, clearsContext bool, ok bool) {
	switch optionID {
	case "exit-plan-bypass", "bypassPermissions":
		return "bypassPermissions", false, true
	case "exit-plan-auto", "auto":
		return "auto", false, true
	case "exit-plan-accept-edits", "acceptEdits":
		return "acceptEdits", false, true
	case "exit-plan-default", "default":
		return "default", false, true
	case "exit-plan-clear-bypass":
		return "bypassPermissions", true, true
	case "exit-plan-clear-auto":
		return "auto", true, true
	case "exit-plan-clear-accept-edits":
		return "acceptEdits", true, true
	}
	return "", false, false
}

// claudePolicyPlanOption is the option that approves a Claude plan and
// continues under the session's own execution policy. Plan review's
// Implement and the approval form's Yes both select it.
func claudePolicyPlanOption(request *schema.RequestPermissionRequest, enforcement *core.ExecutionEnforcement) *schema.PermissionOption {
	policy := claudePolicyMode(enforcement)
	for i := range request.Options {
		option := &request.Options[i]
		mode, clearsContext, ok := claudePlanOptionMode(string(option.OptionID))
		allows := option.Kind == schema.PermissionOptionKindAllowOnce || option.Kind == schema.PermissionOptionKindAllowAlways
		if ok && mode == policy && !clearsContext && allows {
			return option
		}
	}
	return nil
}

// isClaudePlanApproval says whether this is Claude's own ExitPlanMode
// approval in the claude-agent-acp 0.84 shape. Its tool call carries no kind
// for an AIR client, so the option ids are what identify it.
func isClaudePlanApproval(request *schema.RequestPermissionRequest, harness core.HarnessKind) bool {
	return harness == core.HarnessKindClaude && slices.ContainsFunc(request.Options, func(option schema.PermissionOption) bool {
		return strings.HasPrefix(string(option.OptionID), "exit-plan-")
	})
}

// PermissionChoice is one answer a permission form offers, and the harness
// option it selects.
type PermissionChoice struct {
	optionID schema.PermissionOptionId
	title    string
}

// PermissionAnswers is the answers a person may give to a permission request
// shown as a form.
type PermissionAnswers struct {
	choices []PermissionChoice
	// skip is the option Skip selects: the first offered one-time rejection,
	// so the agent is refused this tool call and its turn continues. Skip
	// never selects `reject_always`, which would persist a rule. Without a
	// one-time rejection the form offers no Skip.
	skip *schema.PermissionOptionId
}

// permissionChoices is the answers a person may give to a permission
// request shown as a form.
//
// This is the only place that decides them: the worker publishes exactly
// these choices and whether Skip is offered, and accepts only these answers,
// so every surface (terminal, web, `mj respond`, automation) sees and can
// select the same set.
//
// Claude's own plan approval (claude-agent-acp 0.84 `exit-plan-*` options,
// which reach this form when Claude wrote no plan file) must not change the
// session's execution policy. The option that continues under that policy
// becomes a plain "Yes", and no option for another mode is offered. When
// Claude does not offer the policy's mode, lower modes stay so the plan can
// still be approved; bypassPermissions is never offered to a Guardian
// session.
func permissionChoices(request *schema.RequestPermissionRequest, harness core.HarnessKind, enforcement *core.ExecutionEnforcement) PermissionAnswers {
	choices := offeredPermissionChoices(request, harness, enforcement)
	var skip *schema.PermissionOptionId
	for _, option := range request.Options {
		offered := slices.ContainsFunc(choices, func(choice PermissionChoice) bool { return choice.optionID == option.OptionID })
		if option.Kind == schema.PermissionOptionKindRejectOnce && offered {
			id := option.OptionID
			skip = &id
			break
		}
	}
	return PermissionAnswers{choices: choices, skip: skip}
}

func offeredPermissionChoices(request *schema.RequestPermissionRequest, harness core.HarnessKind, enforcement *core.ExecutionEnforcement) []PermissionChoice {
	if !isClaudePlanApproval(request, harness) {
		choices := make([]PermissionChoice, 0, len(request.Options))
		for _, option := range request.Options {
			choices = append(choices, PermissionChoice{optionID: option.OptionID, title: option.Name})
		}
		return choices
	}
	policyMode := claudePolicyMode(enforcement)
	yes := claudePolicyPlanOption(request, enforcement)
	// Any option in the policy's mode, its clear-context one included, lets
	// the plan be approved without another mode.
	policyOffered := slices.ContainsFunc(request.Options, func(option schema.PermissionOption) bool {
		mode, _, ok := claudePlanOptionMode(string(option.OptionID))
		return ok && mode == policyMode
	})
	choices := []PermissionChoice{}
	if yes != nil {
		choices = append(choices, PermissionChoice{optionID: yes.OptionID, title: "Yes"})
	}
	for _, option := range request.Options {
		if yes != nil && yes.OptionID == option.OptionID {
			continue
		}
		title := option.Name
		if mode, clearsContext, ok := claudePlanOptionMode(string(option.OptionID)); ok {
			switch {
			// The policy owns the mode: with its own mode on offer, no other
			// mode is. Without it, lower modes stay so the plan can still be
			// approved.
			case policyOffered && mode != policyMode:
				continue
			case mode == "bypassPermissions" && policyMode != "bypassPermissions":
				continue
			case clearsContext && mode == policyMode:
				title = "Yes, clear context"
			}
		}
		choices = append(choices, PermissionChoice{optionID: option.OptionID, title: title})
	}
	return choices
}

func policyPlanPermissionAnswer(request *schema.RequestPermissionRequest, response core.ElicitationResponse, harness core.HarnessKind, enforcement *core.ExecutionEnforcement) (PlanPermissionAnswer, error) {
	if action, _ := core.PlanReviewAnswer(response); harness != core.HarnessKindClaude || action != "implement" {
		return PlanPermissionAnswerNative{Value: permissionPlanResponse(request, response)}, nil
	}
	mode := claudePolicyMode(enforcement)
	if option := claudePolicyPlanOption(request, enforcement); option != nil {
		return PlanPermissionAnswerNative{Value: selectedPermissionResponse(option.OptionID)}, nil
	}
	if mode == "bypassPermissions" {
		return PlanPermissionAnswerContinueInBypass{}, nil
	}
	return nil, fmt.Errorf("Cannot implement the approved plan: Claude did not offer the required %s mode. Update the Claude bridge, use a model supporting that mode, or choose another Claude permission mode in the profile's settings.", mode)
}

// nestedStringMatches says whether any string under one of keys, anywhere in
// value, satisfies predicate. value is a decoded JSON value.
func nestedStringMatches(value any, keys []string, predicate func(string) bool) bool {
	switch value := value.(type) {
	case jsonvalue.Object:
		for _, member := range value {
			if text, ok := member.Value.(string); ok && slices.Contains(keys, member.Key) && predicate(text) {
				return true
			}
			if nestedStringMatches(member.Value, keys, predicate) {
				return true
			}
		}
	case []any:
		for _, item := range value {
			if nestedStringMatches(item, keys, predicate) {
				return true
			}
		}
	}
	return false
}

func isPlanPermission(request *schema.RequestPermissionRequest) bool {
	encoded, err := json.Marshal(request)
	if err != nil {
		return false
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		return false
	}
	// Claude Code's ExitPlanMode approval arrives as a `switch_mode` tool call
	// whose rawInput carries the plan text and a `planFilePath`; its title is
	// "Ready to code?" and its options are generic permission-mode ids
	// (`default`, `acceptEdits`, `plan`, ...). None of those match a title or
	// option-id heuristic, so key on the tool kind and the plan payload.
	if nestedStringMatches(value, []string{"kind"}, func(kind string) bool {
		return kind == "plan_review" || kind == "switch_mode"
	}) {
		return true
	}
	if _, ok := core.NestedString(value, []string{"planFilePath", "plan_file_path"}); ok {
		return true
	}
	if nestedStringMatches(value, []string{"title", "name"}, func(name string) bool {
		normalized := strings.NewReplacer(" ", "", "_", "").Replace(asciiLower(name))
		return strings.Contains(normalized, "implementthisplan") || strings.Contains(normalized, "exitplanmode")
	}) {
		return true
	}
	return slices.ContainsFunc(request.Options, func(option schema.PermissionOption) bool {
		id := asciiLower(string(option.OptionID))
		return strings.Contains(id, "plan_approve") || strings.Contains(id, "implement_plan") ||
			strings.Contains(id, "plan_revise") || strings.Contains(id, "reject_and_exit")
	})
}

// asciiLower is Rust's to_ascii_lowercase: only ASCII letters change.
func asciiLower(text string) string {
	var lowered strings.Builder
	lowered.Grow(len(text))
	for i := 0; i < len(text); i++ {
		c := text[i]
		if c >= 'A' && c <= 'Z' {
			c += 'a' - 'A'
		}
		lowered.WriteByte(c)
	}
	return lowered.String()
}

func permissionPlanResponse(request *schema.RequestPermissionRequest, response core.ElicitationResponse) schema.RequestPermissionResponse {
	action, _ := core.PlanReviewAnswer(response)
	var needles []string
	switch action {
	case "implement":
		needles = []string{"implement_plan", "plan_approve", "default", "approve"}
	case "revise":
		needles = []string{"plan_revise", "revise"}
	case "exit":
		needles = []string{"reject_and_exit", "exit"}
	}
	firstOfKind := func(kind schema.PermissionOptionKind) *schema.PermissionOption {
		for i := range request.Options {
			if request.Options[i].Kind == kind {
				return &request.Options[i]
			}
		}
		return nil
	}
	var selected *schema.PermissionOption
	for i := range request.Options {
		option := &request.Options[i]
		id := asciiLower(string(option.OptionID))
		name := asciiLower(option.Name)
		if slices.ContainsFunc(needles, func(needle string) bool {
			return strings.Contains(id, needle) || strings.Contains(name, needle)
		}) {
			selected = option
			break
		}
	}
	if selected == nil {
		// No harness-specific option id matched. Claude's "Ready to code?"
		// exposes only generic kinds, so fall back by intent: implement takes
		// an allow option; every decline (revise, keep_planning, exit) takes a
		// reject option to stay in plan mode rather than cancelling the turn.
		if action == "implement" {
			// Prefer the least-privileged approval so an unmatched harness
			// never silently escalates to a bypass-permissions option.
			if selected = firstOfKind(schema.PermissionOptionKindAllowOnce); selected == nil {
				selected = firstOfKind(schema.PermissionOptionKindAllowAlways)
			}
		} else if selected = firstOfKind(schema.PermissionOptionKindRejectOnce); selected == nil {
			selected = firstOfKind(schema.PermissionOptionKindRejectAlways)
		}
	}
	if selected == nil {
		return cancelledPermissionResponse()
	}
	return selectedPermissionResponse(selected.OptionID)
}

// selectedPermissionResponse answers a permission request with the option
// optionID.
func selectedPermissionResponse(optionID schema.PermissionOptionId) schema.RequestPermissionResponse {
	return schema.RequestPermissionResponse{Outcome: schema.RequestPermissionOutcome{
		Kind:     schema.RequestPermissionOutcomeKindSelected,
		Selected: &schema.SelectedPermissionOutcome{OptionID: optionID},
	}}
}

// allowPermissionResponse is the answer that allows a tool call the worker
// has already authorized, preferably for this call alone. Harnesses can
// still ask for tool approval after their full-access mode was selected, and
// YOLO authorizes those requests; OwnedMcpApprovals authorizes calls to
// Mjolnir's own servers.
func allowPermissionResponse(request *schema.RequestPermissionRequest) *schema.RequestPermissionResponse {
	for _, kind := range []schema.PermissionOptionKind{schema.PermissionOptionKindAllowOnce, schema.PermissionOptionKindAllowAlways} {
		for _, option := range request.Options {
			if option.Kind == kind {
				response := selectedPermissionResponse(option.OptionID)
				return &response
			}
		}
	}
	return nil
}

// ghPermissionLimit is the largest script or argument list, in bytes, a gh
// invocation is authorized for.
const ghPermissionLimit = 16 * 1024

// ghPermissionResponse authorizes only an execute tool with a single literal
// gh invocation. Titles and display summaries are never evidence of what
// will execute. input is nil when the call has none.
func ghPermissionResponse(request *schema.RequestPermissionRequest, input any) *schema.RequestPermissionResponse {
	if request.ToolCall.Kind == nil || *request.ToolCall.Kind != schema.ToolKindExecute {
		return nil
	}
	object, ok := input.(jsonvalue.Object)
	if !ok {
		return nil
	}
	// Conflicting command representations are ambiguous.
	command, hasCommand := object.Get("command")
	cmd, hasCmd := object.Get("cmd")
	switch {
	case hasCommand && !hasCmd:
	case hasCmd && !hasCommand:
		command = cmd
	default:
		return nil
	}
	// DSH's run_command transports the executable separately from argv.
	if argsValue, ok := object.Get("args"); ok {
		executable, ok := command.(string)
		if !ok {
			return nil
		}
		args, ok := argsValue.([]any)
		if !ok {
			return nil
		}
		if len(args) > 1023 || len(executable) > ghPermissionLimit {
			return nil
		}
		size := len(executable)
		for _, arg := range args {
			text, ok := arg.(string)
			if !ok {
				return nil
			}
			size += len(text)
			if size > ghPermissionLimit {
				return nil
			}
		}
		command = append([]any{executable}, args...)
	}
	authorized := false
	switch command := command.(type) {
	case string:
		authorized = ghScript(command)
	case []any:
		if len(command) > 1024 {
			break
		}
		argv := make([]string, 0, len(command))
		size := 0
		for _, arg := range command {
			text, ok := arg.(string)
			if !ok {
				return nil
			}
			argv = append(argv, text)
			size += len(text)
		}
		if size > ghPermissionLimit {
			return nil
		}
		switch {
		case len(argv) > 0 && argv[0] == "gh":
			authorized = true
		case len(argv) == 3 && (argv[1] == "-c" || argv[1] == "-lc") &&
			slices.Contains([]string{"bash", "sh", "/bin/bash", "/bin/sh", "/usr/bin/bash", "/usr/bin/sh"}, argv[0]):
			authorized = ghScript(argv[2])
		}
	}
	if !authorized {
		return nil
	}
	return allowPermissionResponse(request)
}

// ghBashLanguage is tree-sitter-bash's grammar, as gh_script parses with it.
var ghBashLanguage = sync.OnceValue(func() *sitter.Language {
	return sitter.NewLanguage(bash.Language())
})

func ghScript(script string) bool {
	if len(script) > ghPermissionLimit {
		return false
	}
	parser := sitter.NewParser()
	defer parser.Close()
	if err := parser.SetLanguage(ghBashLanguage()); err != nil {
		return false
	}
	tree := parser.Parse([]byte(script), nil)
	if tree == nil {
		return false
	}
	defer tree.Close()
	root := tree.RootNode()
	if root.HasError() || root.NamedChildCount() != 1 {
		return false
	}
	command := root.NamedChild(0)
	if command == nil {
		return false
	}
	if command.Kind() != "command" || command.NamedChildCount() < 1 {
		return false
	}
	// Reject terminators too: the complete script is exactly the command.
	if strings.TrimSpace(script) != script[command.StartByte():command.EndByte()] {
		return false
	}
	name := command.ChildByFieldName("name")
	if name == nil {
		return false
	}
	if script[name.StartByte():name.EndByte()] != "gh" {
		return false
	}
	var literal func(node *sitter.Node, depth int) bool
	literal = func(node *sitter.Node, depth int) bool {
		if depth > 16 {
			return false
		}
		switch node.Kind() {
		case "command", "command_name", "word", "number", "string", "string_content", "raw_string", "concatenation":
		default:
			return false
		}
		cursor := node.Walk()
		defer cursor.Close()
		for _, child := range node.NamedChildren(cursor) {
			if !literal(&child, depth+1) {
				return false
			}
		}
		return true
	}
	return literal(command, 0)
}

// OwnedMcpApprovals is calls to Mjolnir's own MCP servers, for a harness
// that cannot be told to trust them. Under configured approvals DSH asks
// before every MCP tool call and has no per-server rule, so the worker
// allows the tools the servers this launch registered itself list, as
// Claude's allowed tools and Codex's server approval mode do for those
// harnesses. Every other request still reaches a person. A title is matched
// against each owned tool's full name, never a server prefix, and DSH's
// double separator keeps server and tool apart.
//
// OpenCode gets no such answers. It titles a call `<server>_<tool>`, and
// server names may contain `_`, so another server (for example one a
// project's own configuration adds, named `mj-agents_send` with a `message`
// tool) can produce exactly an owned title. Nothing in the request says
// which server a call belongs to, so its person decides, and can allow
// always.
//
// Antigravity's `mcp_<server>_<tool>` titles are ambiguous in the same way,
// so it gets no answers here either. agy has a grant that names the server,
// which the worker stages instead (stageAntigravityGrants).
type OwnedMcpApprovals struct {
	// titles is the permission title of each owned server's tool calls,
	// without any argument the harness appends.
	titles []string
	// appendsArgument says whether the harness follows the title with a
	// space and an argument.
	appendsArgument bool
}

func ownedMcpApprovalsForLaunch(spec *LaunchSpec) OwnedMcpApprovals {
	if spec.ExecutionPolicy != core.ExecutionPolicyConfiguredApprovals {
		return OwnedMcpApprovals{}
	}
	// dsh-acp names an MCP tool `mcp__<server>__<tool>` and titles its call
	// with that name, underscores shown as spaces, then any `path` argument.
	if spec.Harness != core.HarnessKindDsh {
		return OwnedMcpApprovals{}
	}
	var titles []string
	for _, owned := range ownedACPMCPTools(spec) {
		for _, tool := range owned.tools {
			titles = append(titles, strings.ReplaceAll(fmt.Sprintf("mcp__%s__%s", owned.server, tool), "_", " "))
		}
	}
	return OwnedMcpApprovals{titles: titles, appendsArgument: true}
}

// response is the allowing answer when request is a call to an owned tool.
func (o *OwnedMcpApprovals) response(request *schema.RequestPermissionRequest) *schema.RequestPermissionResponse {
	if request.ToolCall.Title == nil {
		return nil
	}
	title := *request.ToolCall.Title
	for _, owned := range o.titles {
		if title == owned {
			return allowPermissionResponse(request)
		}
		// One space then the argument; more spaces mean a longer server or
		// tool name that merely begins with an owned one.
		rest, prefixed := strings.CutPrefix(title, owned)
		argument, spaced := strings.CutPrefix(rest, " ")
		if o.appendsArgument && prefixed && spaced && argument != "" && !strings.HasPrefix(argument, " ") {
			return allowPermissionResponse(request)
		}
	}
	return nil
}

// awaitElicitationResponse waits for an answer to the elicitation id.
// Taking the pending entry decides whether answering or withdrawal wins.
// Once a human or AFK answer was accepted, cancellation still receives it so
// its resolution is recorded even when both notifications are ready. A nil
// cancellation never fires. Returns nil when the question was withdrawn or
// its answer was dropped. It consumes answer (Rust takes the oneshot
// receiver by value), so it closes answer before returning.
func awaitElicitationResponse(ctx context.Context, pending *PendingElicitations, id string, answer *PendingAnswerReceiver, cancellation <-chan struct{}) PendingAnswer {
	defer answer.Close()
	// The classifier's refinement belongs to this wait: an answer or a
	// cancellation ends it and drops its work.
	refining, stopRefining := context.WithCancel(ctx)
	defer stopRefining()
	go pending.refineQuestion(refining, id)
	receive := func() PendingAnswer {
		value, ok := <-answer.C()
		if !ok {
			return nil
		}
		return value
	}
	// Biased as Rust's select: a cancellation already there wins.
	select {
	case <-cancellation:
		if pending.remove(id) != nil {
			return nil
		}
		return receive()
	default:
	}
	select {
	case <-cancellation:
		if pending.remove(id) != nil {
			return nil
		}
		return receive()
	case value, ok := <-answer.C():
		if !ok {
			return nil
		}
		return value
	}
}

func resolvePendingElicitation(pending *PendingElicitations, elicitationID string, response core.ElicitationResponse) error {
	return pending.resolve(elicitationID, response)
}
