// Port of mj-worker/src/acp/permissions.rs.

package worker

import (
	"context"
	"fmt"
	"slices"
	"strings"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PlanImplementationSlot holds the current command's plan implementation
// sender. A permission callback captures the slot, so a late answer cannot
// attach an implementation to a subsequent prompt or bridge. Share it by
// pointer; the zero value is an empty slot.
type PlanImplementationSlot = Shared[*core.Unbounded[PlanImplementation]]

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
	panic("unported: mj-worker/src/acp/permissions.rs restore_plan_execution_mode")
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
	panic("unported: mj-worker/src/acp/permissions.rs offered_permission_choices")
}

func policyPlanPermissionAnswer(request *schema.RequestPermissionRequest, response core.ElicitationResponse, harness core.HarnessKind, enforcement *core.ExecutionEnforcement) (PlanPermissionAnswer, error) {
	panic("unported: mj-worker/src/acp/permissions.rs policy_plan_permission_answer")
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
	panic("unported: mj-worker/src/acp/permissions.rs is_plan_permission")
}

func permissionPlanResponse(request *schema.RequestPermissionRequest, response core.ElicitationResponse) schema.RequestPermissionResponse {
	panic("unported: mj-worker/src/acp/permissions.rs permission_plan_response")
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

// ghPermissionResponse authorizes only an execute tool with a single literal
// gh invocation. Titles and display summaries are never evidence of what
// will execute. input is nil when the call has none.
func ghPermissionResponse(request *schema.RequestPermissionRequest, input any) *schema.RequestPermissionResponse {
	panic("unported: mj-worker/src/acp/permissions.rs gh_permission_response")
}

func ghScript(script string) bool {
	panic("unported: mj-worker/src/acp/permissions.rs gh_script")
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
// its answer was dropped.
func awaitElicitationResponse(ctx context.Context, pending *PendingElicitations, id string, answer <-chan PendingAnswer, cancellation <-chan struct{}) PendingAnswer {
	panic("unported: mj-worker/src/acp/permissions.rs await_elicitation_response")
}

func resolvePendingElicitation(pending *PendingElicitations, elicitationID string, response core.ElicitationResponse) error {
	return pending.resolve(elicitationID, response)
}
