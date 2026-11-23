// Port of mj-worker/src/acp/plan_tests.rs, continued: tool permissions,
// questions and operator-away answers through the plan probe.

package worker

import (
	"fmt"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestUnconstrainedToolPermissionsAreAutoApprovedForEveryHarness(t *testing.T) {
	once := map[string]any{"optionId": "one-time-id", "name": "Approve", "kind": "allow_once"}
	always := map[string]any{"optionId": "persistent-id", "name": "Approve", "kind": "allow_always"}
	reject := map[string]any{"optionId": "reject", "name": "No", "kind": "reject_once"}
	for _, harness := range core.HarnessKindAll {
		probe := planProbeWithHarness(t, core.ExecutionPolicyUnconstrained, false, harness)
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
		for _, scenario := range []struct {
			options  []any
			expected string
		}{
			{[]any{always, reject, once}, "one-time-id"},
			{[]any{once, always}, "one-time-id"},
			{[]any{reject, always}, "persistent-id"},
		} {
			// Claude's live requests both included wildcard deletion in
			// /tmp/snip. Display names deliberately agree: approval must use
			// kind and ID.
			probe.send(map[string]any{
				"jsonrpc": "2.0", "id": "delete-ask", "method": "session/request_permission",
				"params": map[string]any{
					"sessionId": "plan-session",
					"toolCall": map[string]any{
						"toolCallId": "delete-tool", "kind": "execute",
						"title":    "cd /tmp/snip && rm -f *",
						"rawInput": map[string]any{"command": "cd /tmp/snip && rm -f *"},
					},
					"options": scenario.options,
				},
			})
			answer := probe.message()
			if answer["id"] != "delete-ask" || jsonAt(answer, "result", "outcome", "outcome") != "selected" ||
				jsonAt(answer, "result", "outcome", "optionId") != scenario.expected {
				t.Fatalf("%s: answer = %v", harness, answer)
			}
		}
		// Published form events below reveal any accidental pending approval;
		// answering an invented ID would only prove that ID is absent.
		probe.close()
		for _, event := range probe.drainEvents() {
			switch event.(type) {
			case core.RuntimeEventElicitationRequested, core.RuntimeEventWarning:
				t.Fatalf("unexpected permission event: %#v", event)
			}
		}
	}
}

func TestUnconstrainedPermissionWithoutAllowReportsErrorWithoutWaiting(t *testing.T) {
	for _, options := range []any{
		[]any{},
		[]any{map[string]any{"optionId": "reject", "name": "No", "kind": "reject_once"}},
	} {
		probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
		probe.send(map[string]any{
			"jsonrpc": "2.0", "id": "no-allow", "method": "session/request_permission",
			"params": map[string]any{
				"sessionId": "plan-session",
				"toolCall":  map[string]any{"toolCallId": "no-allow-tool", "kind": "execute", "title": "Run command"},
				"options":   options,
			},
		})
		answer := probe.message()
		if answer["id"] != "no-allow" || fmt.Sprint(jsonAt(answer, "error", "code")) != "-32602" ||
			!strings.Contains(jsonString(jsonAt(answer, "error", "data")), "without offering an allow response") {
			t.Fatalf("answer = %v", answer)
		}
		probe.close()
		warned := false
		for _, event := range probe.drainEvents() {
			switch event := event.(type) {
			case core.RuntimeEventWarning:
				if !strings.Contains(event.Message, "Claude Code") || !strings.Contains(event.Message, "plan-session") ||
					!strings.Contains(event.Message, "no-allow-tool") {
					t.Fatal(event.Message)
				}
				warned = true
			case core.RuntimeEventElicitationRequested:
				t.Fatal("an unanswerable request must not leave a form")
			}
		}
		if !warned {
			t.Fatal("the malformed request must be reported")
		}
	}
}

func (p *acpProbe) spawnNativeChild(id string) {
	p.t.Helper()
	p.send(map[string]any{
		"jsonrpc": "2.0", "method": "session/update",
		"params": map[string]any{"sessionId": "plan-session", "update": map[string]any{
			"sessionUpdate": "subagent_spawned", "subagentSessionId": id,
			"name": "worker", "task": "inspect", "capabilities": map[string]any{},
		}},
	})
	for {
		if _, ok := p.event().(core.RuntimeEventNativeAgent); ok {
			return
		}
	}
}

func TestUnconstrainedNativeChildToolPermissionsAreAutoApproved(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
	probe.spawnNativeChild("child")
	probe.send(map[string]any{
		"jsonrpc": "2.0", "id": "child-ask", "method": "session/request_permission",
		"params": map[string]any{
			"sessionId": "child",
			"toolCall":  map[string]any{"toolCallId": "child-tool", "kind": "execute", "title": "Run command"},
			"options":   []any{map[string]any{"optionId": "allow-once", "name": "Yes", "kind": "allow_once"}},
		},
	})
	answer := probe.message()
	if answer["id"] != "child-ask" || jsonAt(answer, "result", "outcome", "optionId") != "allow-once" {
		t.Fatalf("answer = %v", answer)
	}
	probe.close()
	for _, event := range probe.drainEvents() {
		switch event.(type) {
		case core.RuntimeEventElicitationRequested, core.RuntimeEventWarning:
			t.Fatalf("unexpected child permission event: %#v", event)
		}
	}
}

func TestUnconstrainedPlanPermissionsStillWaitForApprovalForEveryHarness(t *testing.T) {
	for _, harness := range core.HarnessKindAll {
		probe := planProbeWithHarness(t, core.ExecutionPolicyUnconstrained, false, harness)
		probe.send(map[string]any{
			"jsonrpc": "2.0", "id": "plan-ask", "method": "session/request_permission",
			"params": planPermission("Implement the parser.", []string{"bypassPermissions", "reject"}),
		})
		var request core.ElicitationRequest
		for found := false; !found; {
			switch event := probe.event().(type) {
			case core.RuntimeEventElicitationRequested:
				request, found = event.Request, true
			case core.RuntimeEventWarning:
				t.Fatalf("plan approval must not warn: %s", event.Message)
			}
		}
		if !strings.HasPrefix(request.ID, "plan-review-") {
			t.Fatal(request.ID)
		}
		probe.noMessage()
		if err := probe.resolve(request.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err != nil {
			t.Fatal(err)
		}
		answer := probe.message()
		if answer["id"] != "plan-ask" || jsonAt(answer, "result", "outcome", "optionId") != "reject" {
			t.Fatalf("%s: answer = %v", harness, answer)
		}
		probe.close()
	}
}

func TestUnconstrainedUserQuestionsStillWaitForAnAnswer(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	probe.send(map[string]any{
		"jsonrpc": "2.0", "id": "question", "method": "elicitation/create",
		"params": map[string]any{
			"sessionId": "plan-session", "mode": "form", "message": "Which file?",
			"requestedSchema": map[string]any{"type": "object", "required": []any{"file"}, "properties": map[string]any{
				"file": map[string]any{"type": "string", "title": "File", "enum": []any{"a.c", "b.c"}},
			}},
		},
	})
	request := probe.elicitationRequested()
	if request.Message != "Which file?" {
		t.Fatal(request.Message)
	}
	probe.noMessage()
	if err := probe.resolve(request.ID, core.ElicitationResponse{Action: core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"file": core.ElicitationValueString("b.c")}}); err != nil {
		t.Fatal(err)
	}
	answer := probe.message()
	if answer["id"] != "question" || jsonAt(answer, "result", "action") != "accept" ||
		jsonAt(answer, "result", "content", "file") != "b.c" {
		t.Fatalf("answer = %v", answer)
	}
	probe.close()
}

// A tool permission request that is not a plan review must reach the user
// as a form instead of being cancelled, which the adapter reports to the
// agent as "Tool use aborted".
// Hard-won: c607200: Guardian Claude tool asks were cancelled because no execution mode was selected.
func TestANonPlanPermissionRequestIsAnsweredByTheUser(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	probe.send(map[string]any{
		"jsonrpc": "2.0", "id": "permission-2", "method": "session/request_permission",
		"params": map[string]any{
			"sessionId": "plan-session",
			"toolCall": map[string]any{
				"toolCallId": "fetch-1", "kind": "fetch",
				"title": "Fetch https://example.com/docs",
			},
			"options": []any{
				map[string]any{"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
				map[string]any{"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
				map[string]any{"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
			},
		},
	})
	request := probe.elicitationRequested()
	if !strings.HasPrefix(request.ID, "tool-permission-") {
		t.Fatalf("a generic permission form, not a plan review: %s", request.ID)
	}
	if request.Message != "Claude Code requests permission:\nFetch https://example.com/docs" {
		t.Fatal(request.Message)
	}
	field := request.Fields[0]
	if field.ID != "choice" {
		t.Fatal(field.ID)
	}
	selectKind, ok := field.Kind.(core.ElicitationFieldKindSingleSelect)
	if !ok {
		t.Fatal("a permission form offers the harness options as a select")
	}
	var values []string
	for _, option := range selectKind.Options {
		values = append(values, option.Value)
	}
	if !reflect.DeepEqual(values, []string{"allow_once", "allow_always", "reject_once"}) {
		t.Fatalf("values = %v", values)
	}
	if err := probe.resolve(request.ID, core.ElicitationResponse{Action: core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"choice": core.ElicitationValueString("allow_once")}}); err != nil {
		t.Fatal(err)
	}
	answer := probe.message()
	if answer["id"] != "permission-2" || jsonAt(answer, "result", "outcome", "outcome") != "selected" ||
		jsonAt(answer, "result", "outcome", "optionId") != "allow_once" {
		t.Fatalf("answer = %v", answer)
	}
	probe.close()
}

// Hard-won: ebb2e9611: a late old bridge resolution cancelled a reused permission ID.
func TestPermissionIDsSurviveBridgeRestartWithoutConsumingTheNewQuestion(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), "permission-restart", "test")
	if err != nil {
		t.Fatal(err)
	}
	commandID := "original-permission-turn"
	if _, err := relay.appendRelayEvent(&commandID, core.RelayObservationCommandQueued{
		CommandID:   "original-permission-turn",
		Command:     core.RelayCommandPrompt{Prompt: textPrompt("run tests")},
		CreatedAtMs: 1,
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationCommandStarted{
		CommandID: "original-permission-turn", StartedAtMs: 2,
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.ClaimPendingCommands(true); err != nil {
		t.Fatal(err)
	}
	ask := func(probe *planProbe) core.ElicitationRequest {
		probe.send(map[string]any{
			"jsonrpc": "2.0", "id": "permission-ask", "method": "session/request_permission",
			"params": map[string]any{"sessionId": "plan-session", "toolCall": map[string]any{
				"toolCallId": "execute-test", "kind": "execute", "title": "cargo test",
			}, "options": []any{map[string]any{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
				map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}}},
		})
		return probe.elicitationRequested()
	}
	first := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	old := ask(first)
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: old}); err != nil {
		t.Fatal(err)
	}
	first.cancel()
	if _, err := first.outcome(); err == nil {
		t.Fatal("the driver is cancelled")
	}
	if _, err := relay.RecordCommandInterrupted("original-permission-turn", core.OutcomeReasonWorkerRestarted, "bridge died"); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationSessionRestarted{}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []schema.SessionConfigOption{}}); err != nil {
		t.Fatal(err)
	}
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil || len(claimed) == 0 {
		t.Fatalf("claimed = %#v, err = %v", claimed, err)
	}
	continued := claimed[0].CommandID

	second := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	fresh := ask(second)
	if old.ID == fresh.ID {
		t.Fatal("each bridge's first permission is a different journal identity")
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: fresh}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationResolved{
		ElicitationID: old.ID, Action: "cancel",
	}); err != nil {
		t.Fatal(err)
	}
	if relay.OperationalState().CancellingPromptID != nil {
		t.Fatal("the old resolution cancels nothing")
	}
	if second.resolve(old.ID, core.ElicitationResponse{Action: core.ElicitationResponseCancel}) == nil {
		t.Fatal("the old id is not the new form's")
	}
	// The old response did not consume the new durable question either.
	if _, err := relay.RecordObservation(core.RelayObservationElicitationResolved{
		ElicitationID: fresh.ID, Action: "cancel",
	}); err != nil {
		t.Fatal(err)
	}
	if cancelling := relay.OperationalState().CancellingPromptID; cancelling == nil || *cancelling != continued {
		t.Fatalf("cancelling = %v, want %s", cancelling, continued)
	}
	if err := second.resolve(fresh.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err != nil {
		t.Fatal(err)
	}
	if answer := second.message(); answer["id"] != "permission-ask" {
		t.Fatalf("answer = %v", answer)
	}
	second.close()
}

// ACP keeps `cancelled` for a cancelled turn. Skip rejects the tool call
// alone: it selects the first one-time rejection, here codex-acp's "No,
// continue without running it", and the turn keeps running. A request with
// no one-time rejection offers no Skip. Cancel stops the turn, so the agent
// receives `session/cancel` before its request is answered `cancelled`.
// Hard-won: #1313: Skip and Cancel on a permission form both told the agent its turn was cancelled.
// Hard-won: 00050db89: user permission Cancel and bridge withdrawal need distinct durable actions.
func TestSkipRejectsOnlyTheToolCallAndCancelCancelsTheTurn(t *testing.T) {
	probe := planProbeWithHarness(t, core.ExecutionPolicyConfiguredApprovals, false, core.HarnessKindCodex)
	probe.command(CommandRequestPrompt{RequestID: "original-prompt", Prompt: textPrompt("Run the tests")})
	prompt := probe.message()
	if prompt["method"] != "session/prompt" {
		t.Fatalf("prompt = %v", prompt)
	}
	// The options codex-acp 1.13.7 builds for a command approval that offers
	// both of Codex's refusals, sorted allow first.
	codexOptions := []any{
		map[string]any{"optionId": "approved", "name": "Yes, proceed", "kind": "allow_once"},
		map[string]any{"optionId": "approved-for-session", "name": "Yes, and don't ask again for this command in this session", "kind": "allow_always"},
		map[string]any{"optionId": "decline", "name": "No, continue without running it", "kind": "reject_once"},
		map[string]any{"optionId": "abort", "name": "No, and tell Codex what to do differently", "kind": "reject_once"},
	}
	networkOptions := []any{
		map[string]any{"optionId": "network_policy_amendment:0", "name": "Yes, and allow this host in the future", "kind": "allow_always"},
		map[string]any{"optionId": "network_policy_amendment:1", "name": "No, and block this host in the future", "kind": "reject_always"},
	}
	ask := func(id string, options []any) core.ElicitationRequest {
		probe.send(map[string]any{
			"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
			"params": map[string]any{
				"sessionId": "plan-session",
				"toolCall":  map[string]any{"toolCallId": id, "kind": "execute", "title": "cargo test"},
				"options":   options,
			},
		})
		return probe.elicitationRequested()
	}
	permissionForm := func(skip bool, toolCallID string) *core.ToolPermissionForm {
		return &core.ToolPermissionForm{Skip: skip, ToolCallID: &toolCallID}
	}

	form := ask("skip-ask", codexOptions)
	if !reflect.DeepEqual(form.ToolPermission, permissionForm(true, "skip-ask")) {
		t.Fatalf("tool permission = %+v", form.ToolPermission)
	}
	if err := probe.resolve(form.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err != nil {
		t.Fatal(err)
	}
	answer := probe.message()
	if answer["id"] != "skip-ask" || !jsonEqual(jsonAt(answer, "result", "outcome"),
		map[string]any{"outcome": "selected", "optionId": "decline"}) {
		t.Fatalf("answer = %v", answer)
	}
	probe.noFinish()

	form = ask("network-ask", networkOptions)
	if !reflect.DeepEqual(form.ToolPermission, permissionForm(false, "network-ask")) {
		t.Fatalf("tool permission = %+v", form.ToolPermission)
	}
	if probe.resolve(form.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline}) == nil {
		t.Fatal("Skip must never select reject_always, which persists a rule")
	}
	probe.noMessage()
	if err := probe.resolve(form.ID, core.ElicitationResponse{Action: core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"choice": core.ElicitationValueString("network_policy_amendment:1")}}); err != nil {
		t.Fatal(err)
	}
	answer = probe.message()
	if answer["id"] != "network-ask" || jsonAt(answer, "result", "outcome", "outcome") != "selected" {
		t.Fatalf("answer = %v", answer)
	}

	// A harness withdrawal is not a user stop and must not poison restart
	// recovery.
	withdrawn := ask("withdraw-ask", codexOptions)
	probe.send(map[string]any{"jsonrpc": "2.0", "method": "$/cancel_request", "params": map[string]any{"requestId": "withdraw-ask"}})
	answer = probe.message()
	if answer["id"] != "withdraw-ask" || !jsonEqual(jsonAt(answer, "result", "outcome"), map[string]any{"outcome": "cancelled"}) {
		t.Fatalf("answer = %v", answer)
	}
	for {
		if resolved, ok := probe.event().(core.RuntimeEventElicitationResolved); ok && resolved.ElicitationID == withdrawn.ID {
			if resolved.Action != "closed" {
				t.Fatalf("action = %s", resolved.Action)
			}
			break
		}
	}
	probe.noFinish()

	form = ask("cancel-ask", codexOptions)
	if err := probe.resolve(form.ID, core.ElicitationResponse{Action: core.ElicitationResponseCancel}); err != nil {
		t.Fatal(err)
	}
	if cancel := probe.message(); cancel["method"] != "session/cancel" {
		t.Fatalf("cancel = %v", cancel)
	}
	answer = probe.message()
	if answer["id"] != "cancel-ask" || !jsonEqual(jsonAt(answer, "result", "outcome"), map[string]any{"outcome": "cancelled"}) {
		t.Fatalf("answer = %v", answer)
	}
	for {
		if resolved, ok := probe.event().(core.RuntimeEventElicitationResolved); ok && resolved.ElicitationID == form.ID {
			if resolved.Action != "cancel" || resolved.Reply != nil {
				t.Fatalf("resolved = %+v", resolved)
			}
			break
		}
	}
	probe.result(prompt, map[string]any{"stopReason": "cancelled"})
	if stopReason := probe.finished(); stopReason != "Cancelled" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

// bridgeExitPlanRequest is Claude's ExitPlanMode request exactly as
// claude-agent-acp 0.84.0 sends it to Mjolnir, an AIR client, when Claude
// wrote no plan file: the tool call has no kind and the input no plan, so it
// is not a plan review. The Guardian shape without a plan was captured from a
// live worker.log. The options are what `buildExitPlanModePermissionOptions`
// builds, stably sorted by kind (allow once, allow always, reject): the mode
// the session was in before Plan leads the elevated ones (Auto for Guardian,
// bypass for YOLO), and a plan text adds a clear-context option for that
// mode before them.
func bridgeExitPlanRequest(policy core.ExecutionPolicy, plan bool) map[string]any {
	options := []any{
		map[string]any{"optionId": "exit-plan-default", "name": "Yes, manually approve edits", "kind": "allow_once"},
	}
	if plan {
		if policy.IsUnconstrained() {
			options = append(options, map[string]any{"optionId": "exit-plan-clear-bypass", "name": "Yes, clear context (12% used) and bypass permissions", "kind": "allow_always"})
		} else {
			options = append(options, map[string]any{"optionId": "exit-plan-clear-auto", "name": "Yes, clear context (12% used) and use auto mode", "kind": "allow_always"})
		}
	}
	auto := map[string]any{"optionId": "exit-plan-auto", "name": "Yes, and use auto mode", "kind": "allow_always"}
	bypass := map[string]any{"optionId": "exit-plan-bypass", "name": "Yes, and bypass permissions", "kind": "allow_always"}
	if policy.IsUnconstrained() {
		options = append(options, bypass, auto)
	} else {
		options = append(options, auto, bypass)
	}
	options = append(options, map[string]any{"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"})
	rawInput := map[string]any{}
	if plan {
		rawInput = map[string]any{"plan": "Add one line."}
	}
	return map[string]any{
		"sessionId": "plan-session",
		"toolCall": map[string]any{
			"toolCallId": "toolu_exit_plan",
			"title":      "Approve Plan",
			"rawInput":   rawInput,
		},
		"options": options,
		"_meta":   map[string]any{"jetbrains": map[string]any{"air": map[string]any{"version": 1, "permission": map[string]any{"version": 1, "title": "Ready to code?"}}}},
	}
}

func offeredChoices(t *testing.T, request any, policy core.ExecutionPolicy) [][2]string {
	t.Helper()
	decoded := decodeAs[schema.RequestPermissionRequest](t, request)
	var choices [][2]string
	for _, choice := range permissionChoices(&decoded, core.HarnessKindClaude, claudeEnforcement(policy)).choices {
		choices = append(choices, [2]string{string(choice.optionID), choice.title})
	}
	return choices
}

// Hard-won: 42d0e02: Guardian plan approval exposed an option that switched Claude to bypassPermissions.
func TestClaudePlanApprovalOffersYesUnderTheSessionPolicyAndNoBypassInGuardian(t *testing.T) {
	guardian := core.ExecutionPolicyConfiguredApprovals
	yolo := core.ExecutionPolicyUnconstrained
	request := decodeAs[schema.RequestPermissionRequest](t, bridgeExitPlanRequest(guardian, false))
	if isPlanPermission(&request) {
		t.Fatal("claude-agent-acp's exit request is not a plan review")
	}
	expect := func(got [][2]string, want ...[2]string) {
		t.Helper()
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("offered %v, want %v", got, want)
		}
	}
	// The policy owns the mode, so approving a plan offers only the policy's
	// own option, with and without clearing the context.
	expect(offeredChoices(t, bridgeExitPlanRequest(guardian, false), guardian),
		[2]string{"exit-plan-auto", "Yes"}, [2]string{"reject", "No, keep planning"})
	expect(offeredChoices(t, bridgeExitPlanRequest(guardian, true), guardian),
		[2]string{"exit-plan-auto", "Yes"}, [2]string{"exit-plan-clear-auto", "Yes, clear context"},
		[2]string{"reject", "No, keep planning"})
	expect(offeredChoices(t, bridgeExitPlanRequest(yolo, false), yolo),
		[2]string{"exit-plan-bypass", "Yes"}, [2]string{"reject", "No, keep planning"})
	expect(offeredChoices(t, bridgeExitPlanRequest(yolo, true), yolo),
		[2]string{"exit-plan-bypass", "Yes"}, [2]string{"exit-plan-clear-bypass", "Yes, clear context"},
		[2]string{"reject", "No, keep planning"})
	// Only the policy's clear-context option is on offer: it still rules out
	// every other mode.
	clearOnly := bridgeExitPlanRequest(guardian, true)
	var kept []any
	for _, option := range clearOnly["options"].([]any) {
		if option.(map[string]any)["optionId"] != "exit-plan-auto" {
			kept = append(kept, option)
		}
	}
	clearOnly["options"] = kept
	expect(offeredChoices(t, clearOnly, guardian),
		[2]string{"exit-plan-clear-auto", "Yes, clear context"}, [2]string{"reject", "No, keep planning"})
	// A Guardian session whose bridge led with bypass still gets no option
	// above its policy.
	expect(offeredChoices(t, bridgeExitPlanRequest(yolo, true), guardian),
		[2]string{"exit-plan-auto", "Yes"}, [2]string{"reject", "No, keep planning"})
}

// The published form and the accepted answers come from the same decision:
// Yes selects the bridge option for the session's policy, and an answer
// naming the hidden bypass option is refused without reaching Claude.
// Hard-won: 42d0e02: A crafted Guardian answer could select an unoffered bypass mode.
func TestClaudePlanApprovalFormSelectsThePolicyModeAndRefusesHiddenBypass(t *testing.T) {
	for _, scenario := range []struct {
		policy core.ExecutionPolicy
		yes    string
	}{
		{core.ExecutionPolicyConfiguredApprovals, "exit-plan-auto"},
		{core.ExecutionPolicyUnconstrained, "exit-plan-bypass"},
	} {
		probe := newPlanProbe(t, scenario.policy)
		probe.send(map[string]any{
			"jsonrpc": "2.0", "id": "exit-plan", "method": "session/request_permission",
			"params": bridgeExitPlanRequest(scenario.policy, false),
		})
		var request core.ElicitationRequest
		for found := false; !found; {
			switch event := probe.event().(type) {
			case core.RuntimeEventElicitationRequested:
				request, found = event.Request, true
			case core.RuntimeEventWarning:
				t.Fatalf("plan approval must not warn: %s", event.Message)
			}
		}
		selectKind, ok := request.Fields[0].Kind.(core.ElicitationFieldKindSingleSelect)
		if !ok {
			t.Fatal("the approval is a select")
		}
		options := selectKind.Options
		if options[0].Value != scenario.yes || options[0].Title != "Yes" {
			t.Fatalf("first option = %+v", options[0])
		}
		for _, option := range options {
			if strings.Contains(option.Title, "bypass") {
				t.Fatalf("option = %+v", option)
			}
		}
		choose := func(value string) core.ElicitationResponse {
			return core.ElicitationResponse{Action: core.ElicitationResponseAccept,
				Content: map[string]core.ElicitationValue{"choice": core.ElicitationValueString(value)}}
		}
		if !scenario.policy.IsUnconstrained() {
			if probe.resolve(request.ID, choose("exit-plan-bypass")) == nil {
				t.Fatal("the hidden bypass option is refused")
			}
			probe.noMessage()
		}
		if err := probe.resolve(request.ID, choose(scenario.yes)); err != nil {
			t.Fatal(err)
		}
		answer := probe.message()
		if answer["id"] != "exit-plan" || jsonAt(answer, "result", "outcome", "optionId") != scenario.yes {
			t.Fatalf("answer = %v", answer)
		}
		probe.close()
	}
}

// Exercise the actual ACP callbacks, including the harness-specific plan
// envelopes, while keeping question ownership independent of harness kind.
// A parent agent answers questions with a message but cannot grant
// approvals, so tool permissions and plan reviews stay open for a person,
// and so do secrets, which a message would keep in plain text.
func TestManagedChildrenCancelQuestionsAndKeepApprovalsForAPerson(t *testing.T) {
	for _, harness := range core.HarnessKindAll {
		for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
			probe := planProbeWithOwnership(t, policy, false, harness, true)
			probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
			question := map[string]any{
				"jsonrpc": "2.0", "id": "ask", "method": "elicitation/create", "params": map[string]any{
					"sessionId": "plan-session", "mode": "form", "message": "Which file?",
					"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"file": map[string]any{"type": "string", "title": "File"}}},
				},
			}
			secret := map[string]any{
				"jsonrpc": "2.0", "id": "secret", "method": "elicitation/create", "params": map[string]any{
					"sessionId": "plan-session", "mode": "form", "message": "Paste the token",
					"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"token": map[string]any{
						"type": "string", "title": "Secret token", "_meta": map[string]any{"codex": map[string]any{"isSecret": true}}}}},
				},
			}
			toolAsk := map[string]any{
				"jsonrpc": "2.0", "id": "permission", "method": "session/request_permission", "params": map[string]any{
					"sessionId": "plan-session", "toolCall": map[string]any{"toolCallId": "tool", "kind": "execute", "title": "Run command"},
					"options": []any{map[string]any{"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
						map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}},
				},
			}
			plan := map[string]any{"jsonrpc": "2.0", "id": "plan", "method": "session/request_permission",
				"params": planPermission("The proposed plan", []string{"bypassPermissions", "reject"})}
			approvals := []map[string]any{plan, secret}
			if policy.IsUnconstrained() {
				probe.send(toolAsk)
				if answer := probe.message(); jsonAt(answer, "result", "outcome", "optionId") != "allow" {
					t.Fatalf("full-access children retain automatic tool approval: %v", answer)
				}
				probe.send(map[string]any{
					"jsonrpc": "2.0", "id": "no-allow", "method": "session/request_permission", "params": map[string]any{
						"sessionId": "plan-session", "toolCall": map[string]any{"toolCallId": "no-allow", "kind": "execute", "title": "Run command"},
						"options": []any{map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}},
					},
				})
				if _, ok := probe.message()["error"].(map[string]any); !ok {
					t.Fatal("an unanswerable full-access request keeps its original error path")
				}
				for done := false; !done; {
					switch event := probe.event().(type) {
					case core.RuntimeEventWarning:
						if !strings.Contains(event.Message, "without offering an allow response") {
							t.Fatal(event.Message)
						}
						done = true
					case core.RuntimeEventElicitationRequested:
						t.Fatal("automatic approval must not publish a human form")
					}
				}
			} else {
				approvals = append(approvals, toolAsk)
			}
			if harness == core.HarnessKindGrok {
				approvals = append(approvals, map[string]any{"jsonrpc": "2.0", "id": "grok-plan", "method": "x.ai/exit_plan_mode",
					"params": map[string]any{"sessionId": "plan-session", "plan": "Proposed plan"}})
			}
			{
				id := question["id"]
				probe.send(question)
				var request core.ElicitationRequest
				for found := false; !found; {
					switch event := probe.event().(type) {
					case core.RuntimeEventElicitationRequested:
						request, found = event.Request, true
					case core.RuntimeEventWarning:
						t.Fatalf("%s: %s", harness, event.Message)
					}
				}
				answer := probe.message()
				if answer["id"] != id {
					t.Fatalf("%s: answer belongs to the original request: %v", harness, answer)
				}
				if jsonAt(answer, "result", "action") != "cancel" {
					t.Fatalf("answer = %v", answer)
				}
				for {
					if resolved, ok := probe.event().(core.RuntimeEventElicitationResolved); ok {
						if resolved.ElicitationID != request.ID || resolved.Action != "cancel" || resolved.Reply != nil {
							t.Fatalf("cancellation is not a parent answer: %+v", resolved)
						}
						break
					}
				}
				if probe.resolve(request.ID, core.ElicitationResponse{Action: core.ElicitationResponseCancel}) == nil {
					t.Fatal("the form must no longer be externally answerable")
				}
			}
			// With the person present, nothing answers an approval but them.
			probe.operatorAway.set(core.OperatorPresence{Away: false, Revision: 2})
			for _, ask := range approvals {
				id := ask["id"]
				probe.send(ask)
				var request core.ElicitationRequest
				for found := false; !found; {
					switch event := probe.event().(type) {
					case core.RuntimeEventElicitationRequested:
						request, found = event.Request, true
					case core.RuntimeEventWarning:
						t.Fatalf("%s: %s", harness, event.Message)
					}
				}
				if request.GoesToParent() {
					t.Fatalf("%s: %v stays with a person", harness, id)
				}
				if err := probe.resolve(request.ID, core.ElicitationResponse{Action: core.ElicitationResponseCancel}); err != nil {
					t.Fatalf("%s: a person must be able to answer %v: %v", harness, id, err)
				}
				if answer := probe.message(); answer["id"] != id {
					t.Fatalf("%s: the person's answer reaches the original request: %v", harness, answer)
				}
			}
			probe.close()
		}
	}
}

func (p *planProbe) awayRefusal(ask map[string]any, expected any) {
	p.t.Helper()
	id := ask["id"]
	p.send(ask)
	question := p.elicitationRequested()
	switch event := p.event().(type) {
	case core.RuntimeEventElicitationResolved:
		if event.ElicitationID != question.ID || event.Action != core.OperatorAwayAction || event.Reply != nil {
			p.t.Fatalf("resolved = %+v", event)
		}
	default:
		p.t.Fatalf("away refusal must resolve immediately: %#v", event)
	}
	answer := p.message()
	if answer["id"] != id || !jsonEqual(answer["result"], expected) {
		p.t.Fatalf("answer = %v, want %v", answer, expected)
	}
	if p.resolve(question.ID, core.ElicitationResponse{Action: core.ElicitationResponseCancel}) == nil {
		p.t.Fatal("an away refusal must not leave an externally answerable form")
	}
}

func (p *planProbe) awayRepeat(ask map[string]any, expected any) {
	p.t.Helper()
	id := ask["id"]
	p.send(ask)
	var question core.ElicitationRequest
	for found := false; !found; {
		switch event := p.event().(type) {
		case core.RuntimeEventElicitationRequested:
			question, found = event.Request, true
		case core.RuntimeEventElicitationResolved:
			p.t.Fatalf("unexpected resolution: %s", event.Action)
		}
	}
	if err := p.resolve(question.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err != nil {
		p.t.Fatal(err)
	}
	answer := p.message()
	if answer["id"] != id || !jsonEqual(answer["result"], expected) {
		p.t.Fatalf("answer = %v, want %v", answer, expected)
	}
	for {
		if resolved, ok := p.event().(core.RuntimeEventElicitationResolved); ok {
			if resolved.ElicitationID != question.ID || resolved.Action != "decline" {
				p.t.Fatalf("resolved = %+v", resolved)
			}
			return
		}
	}
}

func (p *planProbe) awayPendingRefusal(ask map[string]any, revision uint64, expected any) {
	p.t.Helper()
	id := ask["id"]
	p.send(ask)
	var pending core.ElicitationRequest
	for found := false; !found; {
		switch event := p.event().(type) {
		case core.RuntimeEventElicitationRequested:
			pending, found = event.Request, true
		case core.RuntimeEventWarning:
			p.t.Fatalf("request failed before AFK: %s", event.Message)
		}
	}
	p.operatorAway.set(core.OperatorPresence{Away: true, Revision: revision})
	for {
		if resolved, ok := p.event().(core.RuntimeEventElicitationResolved); ok {
			if resolved.ElicitationID != pending.ID || resolved.Action != core.OperatorAwayAction || resolved.Reply != nil {
				p.t.Fatalf("resolved = %+v", resolved)
			}
			break
		}
	}
	response := p.message()
	if response["id"] != id || !jsonEqual(response["result"], expected) {
		p.t.Fatalf("response = %v, want %v", response, expected)
	}
	if p.resolve(pending.ID, core.ElicitationResponse{Action: core.ElicitationResponseCancel}) == nil {
		p.t.Fatal("AFK must remove the unanswered form from the externally answerable set")
	}
}

// cloneJSONObject is a deep copy of a generic JSON object.
func cloneJSONObject(t *testing.T, value map[string]any) map[string]any {
	t.Helper()
	return decodeJSONValue(t, []byte(toValueJSON(t, value))).(map[string]any)
}

func toValueJSON(t *testing.T, value any) string {
	t.Helper()
	encoded, err := jsonMarshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

// Verify requests stay answerable until their pending owner observes AFK.
// Hard-won: 08e09d915: enabling AFK left already-open forms pending.
func TestOperatorAwayResolvesQuestionsAlreadyPendingAcrossHarnesses(t *testing.T) {
	for _, harness := range core.HarnessKindAll {
		probe := planProbeWithHarness(t, core.ExecutionPolicyConfiguredApprovals, false, harness)
		humanFirst := map[string]any{"jsonrpc": "2.0", "id": "question-human-first", "method": "elicitation/create", "params": map[string]any{
			"sessionId": "plan-session", "mode": "form", "message": "Which file?",
			"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"answer": map[string]any{"type": "string"}}},
		}}
		probe.send(humanFirst)
		var humanPending core.ElicitationRequest
		for found := false; !found; {
			switch event := probe.event().(type) {
			case core.RuntimeEventElicitationRequested:
				humanPending, found = event.Request, true
			case core.RuntimeEventWarning:
				t.Fatalf("question failed before AFK: %s", event.Message)
			}
		}
		if err := probe.resolve(humanPending.ID, core.ElicitationResponse{Action: core.ElicitationResponseAccept,
			Content: map[string]core.ElicitationValue{"answer": core.ElicitationValueString("before AFK")}}); err != nil {
			t.Fatal(err)
		}
		humanResponse := probe.message()
		if humanResponse["id"] != "question-human-first" || jsonAt(humanResponse, "result", "action") != "accept" ||
			jsonAt(humanResponse, "result", "content", "answer") != "before AFK" {
			t.Fatalf("response = %v", humanResponse)
		}
		for {
			if resolved, ok := probe.event().(core.RuntimeEventElicitationResolved); ok {
				if resolved.ElicitationID != humanPending.ID || resolved.Action != "accept" {
					t.Fatalf("resolved = %+v", resolved)
				}
				break
			}
		}
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
		probe.operatorAway.set(core.OperatorPresence{Away: false, Revision: 2})

		question := map[string]any{"jsonrpc": "2.0", "id": "question-pending", "method": "elicitation/create", "params": map[string]any{
			"sessionId": "plan-session", "mode": "form", "message": "Which file?",
			"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"answer": map[string]any{"type": "string"}}},
		}}
		rejected := map[string]any{"outcome": map[string]any{"outcome": "selected", "optionId": "reject"}}
		tool := map[string]any{"jsonrpc": "2.0", "id": "permission-pending", "method": "session/request_permission", "params": map[string]any{
			"sessionId": "plan-session", "toolCall": map[string]any{"toolCallId": "tool", "kind": "execute", "title": "Run command"},
			"options": []any{map[string]any{"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
				map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}},
		}}
		plan := map[string]any{"jsonrpc": "2.0", "id": "plan-pending", "method": "session/request_permission",
			"params": planPermission("Proposed plan", []string{"bypassPermissions", "reject"})}
		type asked struct {
			ask      map[string]any
			expected any
		}
		asks := []asked{
			{question, map[string]any{"action": "decline"}},
			{tool, rejected},
			{plan, rejected},
		}
		if harness == core.HarnessKindGrok {
			asks = append(asks, asked{
				map[string]any{"jsonrpc": "2.0", "id": "grok-plan-pending", "method": "x.ai/exit_plan_mode",
					"params": map[string]any{"sessionId": "plan-session", "plan": "Proposed plan"}},
				map[string]any{"outcome": "cancelled"},
			})
		}
		revision := uint64(2)
		for _, item := range asks {
			revision++
			probe.operatorAway.set(core.OperatorPresence{Away: false, Revision: revision})
			revision++
			probe.awayPendingRefusal(cloneJSONObject(t, item.ask), revision, item.expected)
			repeated := cloneJSONObject(t, item.ask)
			repeated["id"] = jsonString(item.ask["id"]) + "-repeat"
			probe.awayRepeat(repeated, item.expected)
		}
		probe.close()
	}

	probe := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	probe.command(CommandRequestPrompt{RequestID: "original-prompt", Prompt: textPrompt("Inspect the repository")})
	prompt := probe.message()
	probe.spawnNativeChild("child-question")
	childQuestion := map[string]any{"jsonrpc": "2.0", "id": "child-question", "method": "elicitation/create", "params": map[string]any{
		"sessionId": "child-question", "mode": "form", "message": "Which file?",
		"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"answer": map[string]any{"type": "string"}}},
	}}
	probe.awayPendingRefusal(childQuestion, 1, map[string]any{"action": "decline"})
	probe.answered(prompt)
	if stopReason := probe.finished(); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

// Real ACP ingress/response envelopes exercise the gate before form
// registration.
// Hard-won: AFK review #1/#2: stale writes and missed transitions corrupted refusal memory.
func TestOperatorAwayRefusesOnceThenExposesRepeatsAcrossACPPaths(t *testing.T) {
	rejected := map[string]any{"outcome": map[string]any{"outcome": "selected", "optionId": "reject"}}
	for _, harness := range core.HarnessKindAll {
		probe := planProbeWithHarness(t, core.ExecutionPolicyConfiguredApprovals, false, harness)
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
		tool := map[string]any{
			"jsonrpc": "2.0", "id": "tool-first", "method": "session/request_permission", "params": map[string]any{
				"sessionId": "plan-session", "toolCall": map[string]any{"toolCallId": "tool", "kind": "execute", "title": "Run command",
					"rawInput": map[string]any{"command": "ls", "cwd": "/workspace"}},
				"options": []any{map[string]any{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
					map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}},
			},
		}
		toolCall := func() map[string]any { return jsonAt(tool, "params", "toolCall").(map[string]any) }
		probe.awayRefusal(cloneJSONObject(t, tool), rejected)
		tool["id"] = "tool-repeat"
		toolCall()["toolCallId"] = "new-call-id"
		// Insertion order and transient tool-call ids do not change the
		// action.
		toolCall()["rawInput"] = map[string]any{"cwd": "/workspace", "command": "ls"}
		probe.awayRepeat(cloneJSONObject(t, tool), rejected)
		toolCall()["rawInput"].(map[string]any)["command"] = "pwd"
		probe.awayRefusal(cloneJSONObject(t, tool), rejected)
		toolCall()["rawInput"].(map[string]any)["command"] = "ls"
		probe.awayRepeat(cloneJSONObject(t, tool), rejected)
		probe.operatorAway.set(core.OperatorPresence{Away: false, Revision: 2})
		// A stale away writer cannot resurrect a superseded absence.
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
		probe.awayRepeat(cloneJSONObject(t, tool), rejected)
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 3})
		// A delayed writer from the old period cannot undo the newer
		// presence.
		probe.operatorAway.set(core.OperatorPresence{Away: false, Revision: 2})
		probe.awayRefusal(cloneJSONObject(t, tool), rejected)
		// Equal revisions cannot clear refusal memory or change availability.
		probe.operatorAway.set(core.OperatorPresence{Away: false, Revision: 3})
		probe.awayRepeat(cloneJSONObject(t, tool), rejected)
		// Revision 4 (off) was missed. The newer away transition still starts
		// a period.
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 5})
		probe.awayRefusal(cloneJSONObject(t, tool), rejected)
		probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 3})
		probe.awayRepeat(cloneJSONObject(t, tool), rejected)
		// When no reject-once exists, cancel only this ACP responder.
		toolCall()["rawInput"].(map[string]any)["command"] = "no-rejection-option"
		params := jsonAt(tool, "params").(map[string]any)
		options := params["options"].([]any)
		params["options"] = options[:len(options)-1]
		probe.awayRefusal(cloneJSONObject(t, tool), map[string]any{"outcome": map[string]any{"outcome": "cancelled"}})

		question := map[string]any{"jsonrpc": "2.0", "id": "question", "method": "elicitation/create", "params": map[string]any{
			"sessionId": "plan-session", "mode": "form", "message": "Which file?",
			"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"answer": map[string]any{"type": "string"}}},
		}}
		probe.awayRefusal(cloneJSONObject(t, question), map[string]any{"action": "decline"})
		jsonAt(question, "params").(map[string]any)["message"] = "Rephrased: which directory instead?"
		probe.awayRepeat(cloneJSONObject(t, question), map[string]any{"action": "decline"})

		plan := map[string]any{"jsonrpc": "2.0", "id": "plan", "method": "session/request_permission",
			"params": planPermission("Proposed plan", []string{"bypassPermissions", "reject"})}
		if harness == core.HarnessKindGrok {
			grok := map[string]any{"jsonrpc": "2.0", "id": "grok-plan", "method": "x.ai/exit_plan_mode",
				"params": map[string]any{"sessionId": "plan-session", "plan": "First draft"}}
			probe.awayRefusal(cloneJSONObject(t, grok), map[string]any{"outcome": "cancelled"})
			probe.awayRepeat(cloneJSONObject(t, grok), map[string]any{"outcome": "cancelled"})
			// Both plan transports share the same per-agent refusal memory.
			probe.awayRepeat(cloneJSONObject(t, plan), rejected)
		} else {
			probe.awayRefusal(cloneJSONObject(t, plan), rejected)
		}
		jsonAt(plan, "params", "toolCall", "rawInput").(map[string]any)["plan"] = "Entirely rewritten plan"
		probe.awayRepeat(cloneJSONObject(t, plan), rejected)
		probe.close()
	}
}

func TestOperatorAwayNativeChildrenHaveIndependentRefusalMemory(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	probe.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
	probe.command(CommandRequestPrompt{RequestID: "original-prompt", Prompt: textPrompt("Inspect the repository")})
	prompt := probe.message()
	if prompt["method"] != "session/prompt" {
		t.Fatalf("prompt = %v", prompt)
	}
	for _, child := range []string{"child-a", "child-b"} {
		probe.spawnNativeChild(child)
	}
	for _, agent := range []string{"plan-session", "child-a", "child-b"} {
		question := map[string]any{"jsonrpc": "2.0", "id": "question-" + agent, "method": "elicitation/create", "params": map[string]any{
			"sessionId": agent, "mode": "form", "message": "Which file?",
			"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"answer": map[string]any{"type": "string"}}},
		}}
		probe.awayRefusal(cloneJSONObject(t, question), map[string]any{"action": "decline"})
		probe.awayRepeat(cloneJSONObject(t, question), map[string]any{"action": "decline"})
		plan := planPermission("Inspect the repository", []string{"bypassPermissions", "reject"})
		plan["sessionId"] = agent
		ask := map[string]any{"jsonrpc": "2.0", "id": "plan-" + agent, "method": "session/request_permission", "params": plan}
		rejected := map[string]any{"outcome": map[string]any{"outcome": "selected", "optionId": "reject"}}
		probe.awayRefusal(cloneJSONObject(t, ask), rejected)
		probe.awayRepeat(cloneJSONObject(t, ask), rejected)
		tool := map[string]any{"jsonrpc": "2.0", "id": "tool-" + agent, "method": "session/request_permission", "params": map[string]any{
			"sessionId": agent, "toolCall": map[string]any{"toolCallId": "tool", "kind": "execute", "title": "Run command",
				"rawInput": map[string]any{"command": "pwd"}},
			"options": []any{map[string]any{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
				map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}},
		}}
		probe.awayRefusal(cloneJSONObject(t, tool), rejected)
		probe.awayRepeat(cloneJSONObject(t, tool), rejected)
	}
	probe.answered(prompt)
	if stopReason := probe.finished(); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

func TestGhExecutePermissionIsApprovedAcrossHarnesses(t *testing.T) {
	for _, harness := range core.HarnessKindAll {
		probe := planProbeWithHarness(t, core.ExecutionPolicyConfiguredApprovals, false, harness)
		probe.command(CommandRequestPrompt{RequestID: "gh-prompt", Prompt: textPrompt("Check GitHub")})
		prompt := probe.message()
		if prompt["method"] != "session/prompt" {
			t.Fatalf("prompt = %v", prompt)
		}
		for _, scenario := range []struct {
			shape    string
			rawInput any
		}{
			{"argv", map[string]any{"command": []any{"/bin/bash", "-lc", "gh issue list"}}},
			// Pinned DSH run_command passes executable and args separately.
			{"dsh", map[string]any{"command": "gh", "args": []any{"issue", "view", "123"}}},
			{"kimi-stream", nil},
		} {
			id := "gh-" + scenario.shape
			toolCall := map[string]any{"toolCallId": id, "kind": "execute", "title": "Bash"}
			if scenario.rawInput != nil {
				toolCall["rawInput"] = scenario.rawInput
			} else {
				// Recorded Kimi sequence: initial tool call, then cumulative
				// JSON text updates; permission itself carries no input.
				probe.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
					"sessionId": "plan-session", "update": map[string]any{
						"sessionUpdate": "tool_call", "toolCallId": id,
						"title": "Bash", "kind": "execute",
						"content": []any{map[string]any{"type": "content", "content": map[string]any{"type": "text", "text": ""}}},
					},
				}})
				for _, text := range []string{"{", "{\"command\": \"gh", "{\"command\": \"gh issue list\"}"} {
					probe.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
						"sessionId": "plan-session", "update": map[string]any{
							"sessionUpdate": "tool_call_update", "toolCallId": id,
							"status":  "in_progress",
							"content": []any{map[string]any{"type": "content", "content": map[string]any{"type": "text", "text": text}}},
						},
					}})
				}
				if _, ok := toolCall["rawInput"]; ok {
					t.Fatal("the permission carries no input")
				}
				if _, ok := toolCall["content"]; ok {
					t.Fatal("the permission carries no content")
				}
			}
			probe.send(map[string]any{"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": map[string]any{
				"sessionId": "plan-session", "toolCall": toolCall,
				"options": []any{map[string]any{"optionId": "always", "name": "Always", "kind": "allow_always"},
					map[string]any{"optionId": "once", "name": "Once", "kind": "allow_once"}},
			}})
			answer := probe.message()
			if answer["id"] != id || jsonAt(answer, "result", "outcome", "optionId") != "once" {
				t.Fatalf("%s %s: answer = %v", harness, scenario.shape, answer)
			}
		}
		probe.answered(prompt)
	}
}

func TestGhPermissionSecurityBoundary(t *testing.T) {
	value := map[string]any{"sessionId": "s", "toolCall": map[string]any{"toolCallId": "t", "kind": "execute", "title": "gh issue list"},
		"options": []any{map[string]any{"optionId": "yes", "name": "Yes", "kind": "allow_once"}}}
	input := func(t *testing.T, value any) any {
		t.Helper()
		parsed, err := jsonvalueParse(toValueJSON(t, value))
		if err != nil {
			t.Fatal(err)
		}
		return parsed
	}
	for _, scenario := range []struct {
		input   any
		allowed bool
	}{
		{map[string]any{"command": "gh issue list"}, true},
		{map[string]any{"cmd": "gh issue view 'a b'"}, true},
		{map[string]any{"command": []any{"gh", "issue", "a; echo bad"}}, true},
		{map[string]any{"cmd": []any{"sh", "-c", "gh issue list"}}, true},
		{map[string]any{"command": []any{"bash", "-lc", "gh issue list", "extra"}}, false},
		{map[string]any{"command": []any{"gh-evil", "issue"}}, false},
		{map[string]any{"command": "gh"}, true},
		{map[string]any{"command": "gh issue view 123"}, true},
		// DSH v0.7.0 run_command forwards executable and args unchanged.
		{map[string]any{"command": "gh", "args": []any{"issue", "view", "123"}}, true},
		{map[string]any{"command": "/bin/bash", "args": []any{"-lc", "gh issue list"}}, true},
		{map[string]any{"command": "gh issue list", "args": []any{}}, false},
		{map[string]any{"command": "gh", "args": []any{42}}, false},
		{map[string]any{"command": "gh issue list; echo bad"}, false},
		{map[string]any{"command": "gh issue list && echo bad"}, false},
		{map[string]any{"command": "gh issue list | cat"}, false},
		{map[string]any{"command": "gh issue $(echo bad)"}, false},
		{map[string]any{"command": "gh issue `echo bad`"}, false},
		{map[string]any{"command": "gh issue > file"}, false},
		{map[string]any{"command": "gh issue list;"}, false},
		{map[string]any{"command": "gh issue list &"}, false},
		{map[string]any{"command": "gh issue list 2>/dev/null"}, false},
		{map[string]any{"command": "gh issue \"$(echo bad)\""}, false},
		{map[string]any{"command": []any{"bash", "--noprofile", "-c", "gh issue list"}}, false},
		{map[string]any{"command": "X=bad gh issue list"}, false},
		{map[string]any{"command": "gh issue $TOKEN"}, false},
		{map[string]any{"command": "gh issue list\necho bad"}, false},
		{map[string]any{"nested": map[string]any{"command": "gh issue list"}}, false},
		{map[string]any{"command": "gh issue list", "cmd": "echo bad"}, false},
		{map[string]any{"command": strings.Repeat("gh issue list", 2000)}, false},
	} {
		request := decodeAs[schema.RequestPermissionRequest](t, value)
		if got := ghPermissionResponse(&request, input(t, scenario.input)) != nil; got != scenario.allowed {
			t.Fatalf("%v: allowed = %v", scenario.input, got)
		}
	}
	for _, kind := range []any{"read", "switch_mode", nil} {
		value["toolCall"].(map[string]any)["kind"] = kind
		request := decodeAs[schema.RequestPermissionRequest](t, value)
		if ghPermissionResponse(&request, input(t, map[string]any{"command": "gh issue list"})) != nil {
			t.Fatalf("kind %v must not be approved", kind)
		}
	}
	value["toolCall"].(map[string]any)["kind"] = "execute"
	value["options"] = []any{}
	request := decodeAs[schema.RequestPermissionRequest](t, value)
	if ghPermissionResponse(&request, input(t, map[string]any{"command": "gh issue list"})) != nil {
		t.Fatal("no allow option, no approval")
	}
}

// jsonMarshal writes a generic JSON value as serde_json does.
func jsonMarshal(value any) ([]byte, error) {
	return jsonvalue.Marshal(value)
}

// jsonvalueParse reads text as a serde_json::Value.
func jsonvalueParse(text string) (any, error) {
	return jsonvalue.Parse([]byte(text))
}
