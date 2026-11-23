// Port of mj-worker/src/acp/plan_tests.rs: plan reviews, policy modes and
// permission forms through a real ACP transport whose agent side the test
// controls.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func planImplement() core.ElicitationResponse {
	return core.ElicitationResponse{Action: core.ElicitationResponseAccept, Content: map[string]core.ElicitationValue{
		core.PlanReviewAction: core.ElicitationValueString("implement"),
	}}
}

func planPermission(plan string, options []string) map[string]any {
	offered := []any{}
	for _, id := range options {
		kind := "allow_always"
		if id == "reject" {
			kind = "reject_once"
		}
		offered = append(offered, map[string]any{"optionId": id, "name": id, "kind": kind})
	}
	return map[string]any{
		"sessionId": "plan-session",
		"toolCall": map[string]any{
			"toolCallId": "plan-tool", "kind": "switch_mode", "title": "Ready to code?",
			"rawInput": map[string]any{"plan": plan, "planFilePath": "/workspace/plan.md"},
		},
		"options": offered,
	}
}

func modeConfig(current string) []any {
	return []any{map[string]any{
		"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": current,
		"options": []any{
			map[string]any{"value": "default", "name": "Default"}, map[string]any{"value": "plan", "name": "Plan"},
			map[string]any{"value": "auto", "name": "Auto"}, map[string]any{"value": "acceptEdits", "name": "Accept edits"},
			map[string]any{"value": "bypassPermissions", "name": "Bypass"},
		},
	}}
}

// decodeAs decodes a generic JSON value into a typed one, as
// serde_json::from_value does.
func decodeAs[T any](t *testing.T, value any) T {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var decoded T
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatalf("decode %s: %v", encoded, err)
	}
	return decoded
}

// toValue is serde_json::to_value of a typed value, read back generically.
func toValue(t *testing.T, value any) any {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return decodeJSONValue(t, encoded)
}

// planProbe is a real ACP transport with the agent side controlled by the
// test. No fake depends on runtime implementation order beyond the protocol
// under test.
type planProbe struct {
	*acpProbe
}

func newPlanProbe(t *testing.T, policy core.ExecutionPolicy) *planProbe {
	t.Helper()
	return planProbeWithConfig(t, policy, false)
}

func planProbeWithConfig(t *testing.T, policy core.ExecutionPolicy, config bool) *planProbe {
	t.Helper()
	return planProbeWithHarness(t, policy, config, core.HarnessKindClaude)
}

func planProbeWithHarness(t *testing.T, policy core.ExecutionPolicy, config bool, harness core.HarnessKind) *planProbe {
	t.Helper()
	return planProbeWithOwnership(t, policy, config, harness, false)
}

func planProbeWithOwnership(t *testing.T, policy core.ExecutionPolicy, config bool, harness core.HarnessKind, managedSubagent bool) *planProbe {
	t.Helper()
	return openPlanProbe(t, policy, config, harness, managedSubagent, false, true)
}

// planProbeConfigOnly is a Claude probe whose bridge carries its modes only
// on the `mode` config option, with no legacy mode list.
func planProbeConfigOnly(t *testing.T, policy core.ExecutionPolicy) *planProbe {
	t.Helper()
	return openPlanProbe(t, policy, true, core.HarnessKindClaude, false, false, false)
}

// planProbeRefusingPolicyMode is a probe whose bridge answers the startup
// request for the policy mode with a refusal, as Claude Code does when a
// model cannot use Auto.
func planProbeRefusingPolicyMode(t *testing.T, policy core.ExecutionPolicy, config bool, harness core.HarnessKind) *planProbe {
	t.Helper()
	return openPlanProbe(t, policy, config, harness, false, true, true)
}

func openPlanProbe(t *testing.T, policy core.ExecutionPolicy, config bool, harness core.HarnessKind, managedSubagent, refuseMode, legacyModes bool) *planProbe {
	t.Helper()
	spec := acpDriveSpec("plan-probe", harness, policy, "/workspace")
	spec.ManagedSubagent = managedSubagent
	probe := &planProbe{startACPProbe(t, spec, 4096)}
	initialize := probe.message()
	if initialize["method"] != "initialize" {
		t.Fatalf("first message = %v", initialize)
	}
	probe.result(initialize, map[string]any{
		"protocolVersion": 1,
		"_meta": map[string]any{"jetbrains": map[string]any{"air": map[string]any{
			"version": 1, "capabilities": []any{"nativeSubagentSessions", "backgroundSubagents"}}}},
	})
	enforcement, enforced := harness.ExecutionEnforcement(policy)
	// Muse advertises its approval policy the way muse-acp does:
	// `approval_mode` plus the `auto_review` selector. Every other harness
	// in this probe uses the legacy mode list.
	var options []any
	switch {
	case harness == core.HarnessKindMuse:
		options = musePolicyOptions()
	case config:
		options = modeConfig("plan")
	default:
		options = []any{}
	}
	// dsh-acp carries its permission policy on its own selector.
	if harness == core.HarnessKindDsh {
		options = append(options, map[string]any{"id": "permission_mode", "name": "Tool permissions", "type": "select",
			"currentValue": "ask", "options": []any{
				map[string]any{"value": "ask", "name": "Ask before sensitive tools"},
				map[string]any{"value": "allow", "name": "Allow sensitive tools"},
				map[string]any{"value": "deny", "name": "Deny sensitive tools"},
			}})
	}
	opening := probe.message()
	if opening["method"] != "session/new" {
		t.Fatalf("open = %v", opening)
	}
	// Hard-won: 49d62f2d8: the all-harness fixture omitted Antigravity's default mode.
	if legacyModes {
		modes := []any{
			map[string]any{"id": "plan", "name": "Plan"}, map[string]any{"id": "auto", "name": "Auto"},
			map[string]any{"id": "bypassPermissions", "name": "Bypass"},
			map[string]any{"id": "agent", "name": "Agent"},
			map[string]any{"id": "agent-full-access", "name": "Full access"},
			map[string]any{"id": "allowAll", "name": "Allow all"},
		}
		if harness == core.HarnessKindAntigravity {
			modes = append(modes, map[string]any{"id": "default", "name": "Default"})
		}
		probe.result(opening, map[string]any{"sessionId": "plan-session", "configOptions": options, "modes": map[string]any{
			"currentModeId": "plan", "availableModes": modes,
		}})
	} else {
		probe.result(opening, map[string]any{"sessionId": "plan-session", "configOptions": options})
	}
	// A harness that carries its approval policy on a config selector (Muse
	// on `approval_mode`) is driven through `set_config_option` whatever the
	// probe's `config` flag says; the rest use the flag.
	var mode string
	hasMode := false
	if enforced {
		mode, hasMode = enforcement.ACPMode()
	}
	var selector string
	hasSelector := false
	if enforced {
		selector, hasSelector = enforcement.ACPModeSelector()
	}
	switch {
	case hasMode && hasSelector:
		request := probe.message()
		if request["method"] != "session/set_config_option" || jsonAt(request, "params", "configId") != selector ||
			jsonAt(request, "params", "value") != mode {
			t.Fatalf("mode request = %v", request)
		}
		if refuseMode {
			probe.refuse(request)
		} else {
			selectOption(options, map[string]any{"configId": selector, "value": mode})
			probe.result(request, map[string]any{"configOptions": options})
		}
	case hasMode && config:
		request := probe.message()
		if request["method"] != "session/set_config_option" || jsonAt(request, "params", "value") != mode {
			t.Fatalf("mode request = %v", request)
		}
		if refuseMode {
			probe.refuse(request)
			// Claude answers a refusal by taking its default mode.
			if harness == core.HarnessKindClaude {
				fallback := probe.message()
				if fallback["method"] != "session/set_config_option" || jsonAt(fallback, "params", "value") != "default" {
					t.Fatalf("default request = %v", fallback)
				}
				probe.result(fallback, map[string]any{"configOptions": modeConfig("default")})
			}
		} else {
			probe.result(request, map[string]any{"configOptions": modeConfig(mode)})
		}
	case hasMode:
		request := probe.message()
		if request["method"] != "session/set_mode" || jsonAt(request, "params", "modeId") != mode {
			t.Fatalf("mode request = %v", request)
		}
		probe.result(request, map[string]any{})
	}
	// Muse's auto-review is off in every session the bridge opens, and
	// dsh-acp keeps a session's permission policy in its record, so the
	// worker selects each on every open that enforces it.
	if enforced {
		if key, value, ok := enforcement.ACPSetting(); ok {
			setting := probe.message()
			if setting["method"] != "session/set_config_option" || jsonAt(setting, "params", "configId") != key ||
				jsonAt(setting, "params", "value") != value {
				t.Fatalf("setting request = %v", setting)
			}
			selectOption(options, map[string]any{"configId": key, "value": value})
			probe.result(setting, map[string]any{"configOptions": options})
		}
	}
	probe.opened()
	return probe
}

// answered finishes a prompt the way a working harness does: one line of
// answer, then the result. A turn that produces nothing at all is reported
// as unanswered rather than finished (#970).
func (p *planProbe) answered(request map[string]any) {
	p.t.Helper()
	p.send(map[string]any{
		"jsonrpc": "2.0",
		"method":  "session/update",
		"params": map[string]any{
			"sessionId": "plan-session",
			"update": map[string]any{
				"sessionUpdate": "agent_message_chunk",
				"content":       map[string]any{"type": "text", "text": "done"},
			},
		},
	})
	p.result(request, map[string]any{"stopReason": "end_turn"})
}

func (p *planProbe) approve(plan string, options []string) (map[string]any, string, map[string]any) {
	p.t.Helper()
	p.command(CommandRequestPrompt{RequestID: "original-prompt", Prompt: textPrompt("Make a plan")})
	prompt := p.message()
	if prompt["method"] != "session/prompt" {
		p.t.Fatalf("prompt = %v", prompt)
	}
	p.send(map[string]any{"jsonrpc": "2.0", "id": "permission-1", "method": "session/request_permission",
		"params": planPermission(plan, options)})
	id := p.elicitationRequested().ID
	if err := p.answer(id); err != nil {
		p.t.Fatal(err)
	}
	answer := p.message()
	if answer["id"] != "permission-1" {
		p.t.Fatalf("answer = %v", answer)
	}
	return prompt, id, answer
}

// elicitationRequested waits for the next form the runtime publishes.
func (p *acpProbe) elicitationRequested() core.ElicitationRequest {
	p.t.Helper()
	for {
		if requested, ok := p.event().(core.RuntimeEventElicitationRequested); ok {
			return requested.Request
		}
	}
}

func (p *planProbe) answer(id string) error {
	p.t.Helper()
	return p.resolve(id, planImplement())
}

func (p *planProbe) noMessage() {
	p.t.Helper()
	p.acpProbe.noMessage(30 * time.Millisecond)
}

func (p *planProbe) noFinish() {
	p.t.Helper()
	p.acpProbe.noFinish(100 * time.Millisecond)
}

func (p *planProbe) finished() string {
	p.t.Helper()
	for {
		if finished, ok := p.event().(core.RuntimeEventPromptFinished); ok {
			if finished.RequestID != "original-prompt" {
				p.t.Fatalf("finished = %s", finished.RequestID)
			}
			return finished.StopReason
		}
	}
}

func (p *planProbe) sdkResult(message any) {
	p.t.Helper()
	p.send(map[string]any{
		"jsonrpc": "2.0",
		"method":  "_claude/sdkMessage",
		"params":  map[string]any{"sessionId": "plan-session", "message": message},
	})
}

func (p *planProbe) claudeResult() core.ClaudeTurnResult {
	p.t.Helper()
	for {
		if result, ok := p.event().(core.RuntimeEventClaudeTurnResult); ok {
			return result.ClaudeTurnResult
		}
	}
}

func (p *planProbe) close() {
	p.t.Helper()
	p.command(CommandRequestClose{RequestID: "close"})
	closing := p.message()
	if closing["method"] == "session/cancel" {
		closing = p.message()
	}
	if closing["method"] != "session/close" {
		p.t.Fatalf("close = %v", closing)
	}
	p.result(closing, map[string]any{})
	restart, err := p.outcome()
	if err != nil || restart != nil {
		p.t.Fatalf("restart = %#v, err = %v", restart, err)
	}
}

// drainEvents is the events already sent: Rust's try_recv loop.
func (p *acpProbe) drainEvents() []core.RuntimeEvent {
	var events []core.RuntimeEvent
	for {
		select {
		case event := <-p.events:
			events = append(events, event)
		default:
			return events
		}
	}
}

func TestCodexYoloSuppressesOnlyTheBundledBubblewrapStartupWarning(t *testing.T) {
	warning := "Codex could not find bubblewrap on PATH. Install bubblewrap with your OS package manager. See the sandbox prerequisites: https://developers.openai.com/codex/concepts/sandboxing#prerequisites. Codex will use the bundled bubblewrap in the meantime."
	for _, scenario := range []struct {
		harness              core.HarnessKind
		policy               core.ExecutionPolicy
		suppressPrerequisite bool
	}{
		{core.HarnessKindCodex, core.ExecutionPolicyUnconstrained, true},
		{core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, false},
		{core.HarnessKindClaude, core.ExecutionPolicyUnconstrained, false},
	} {
		probe := planProbeWithHarness(t, scenario.policy, false, scenario.harness)
		for _, notice := range []struct {
			severity   string
			title      string
			details    any
			suppressed bool
		}{
			{"warning", warning, nil, scenario.suppressPrerequisite},
			{"error", warning, nil, false},
			{"warning", warning, "Additional sandbox failure", false},
			{"warning", "bubblewrap could not create a user namespace", nil, false},
		} {
			failure := map[string]any{
				"id": "startup-warning", "revision": 1, "category": "unknown",
				"severity": notice.severity, "title": notice.title, "details": notice.details, "actions": []any{},
			}
			update := map[string]any{
				"sessionUpdate": "session_info_update", "title": "Session title",
				"_meta": map[string]any{"jetbrains": map[string]any{"air": map[string]any{"version": 1, "sessionFailure": failure}}},
			}
			probe.send(map[string]any{
				"jsonrpc": "2.0", "method": "session/update",
				"params": map[string]any{"sessionId": "plan-session", "update": update},
			})
			var received any
			for {
				if relayed, ok := probe.event().(core.RuntimeEventSessionUpdate); ok {
					value := decodeJSONValue(t, relayed.Update)
					if jsonAt(value, "title") == "Session title" {
						received = value
						break
					}
				}
			}
			if notice.suppressed {
				delete(jsonAt(update, "_meta", "jetbrains", "air").(map[string]any), "sessionFailure")
			}
			if !jsonEqual(received, update) {
				t.Fatalf("%s %s %s %s: received %v, want %v", scenario.harness, scenario.policy, notice.severity, notice.title, received, update)
			}
		}
	}
}

func setClaudeConfigMode(probe *planProbe, requestID, mode string) {
	probe.t.Helper()
	probe.command(CommandRequestSetConfig{RequestID: requestID, Key: "mode", Value: mode})
	request := probe.message()
	if request["method"] != "session/set_config_option" || jsonAt(request, "params", "value") != mode {
		probe.t.Fatalf("request = %v", request)
	}
	probe.result(request, map[string]any{"configOptions": modeConfig(mode)})
	for {
		if applied, ok := probe.event().(core.RuntimeEventConfigApplied); ok {
			if applied.Key != "mode" || applied.Value != mode {
				probe.t.Fatalf("applied = %+v", applied)
			}
			return
		}
	}
}

// Hard-won: f8bb425: Leaving Claude Plan mode forced bypassPermissions under Guardian.
func TestClaudePlanModeStaysActiveAndExitRestoresThePolicyMode(t *testing.T) {
	for _, scenario := range []struct {
		policy  core.ExecutionPolicy
		desired string
	}{
		{core.ExecutionPolicyConfiguredApprovals, "auto"},
		{core.ExecutionPolicyUnconstrained, "bypassPermissions"},
	} {
		for _, config := range []bool{false, true} {
			probe := planProbeWithConfig(t, scenario.policy, config)
			if config {
				setClaudeConfigMode(probe, "enter-plan", "plan")
			} else {
				probe.command(CommandRequestSetSessionMode{RequestID: "enter-plan", ModeID: "plan"})
				entering := probe.message()
				if jsonAt(entering, "params", "modeId") != "plan" {
					t.Fatalf("entering = %v", entering)
				}
				probe.result(entering, map[string]any{})
				for {
					if applied, ok := probe.event().(core.RuntimeEventSessionModeApplied); ok {
						if applied.RequestID != "enter-plan" {
							t.Fatalf("applied = %+v", applied)
						}
						break
					}
				}
			}
			// Entering Plan must not immediately send another request that
			// restores the launch policy.
			probe.noMessage()
			probe.command(CommandRequestRestoreExecutionMode{RequestID: "exit-plan"})
			restoring := probe.message()
			if config {
				if restoring["method"] != "session/set_config_option" || jsonAt(restoring, "params", "value") != scenario.desired {
					t.Fatalf("restoring = %v", restoring)
				}
			} else if restoring["method"] != "session/set_mode" || jsonAt(restoring, "params", "modeId") != scenario.desired {
				t.Fatalf("restoring = %v", restoring)
			}
			for _, event := range probe.drainEvents() {
				if _, applied := event.(core.RuntimeEventConfigApplied); applied {
					t.Fatal("the exit is not applied before Claude answers")
				}
			}
			if config {
				probe.result(restoring, map[string]any{"configOptions": modeConfig(scenario.desired)})
			} else {
				probe.result(restoring, map[string]any{})
			}
			for {
				if applied, ok := probe.event().(core.RuntimeEventConfigApplied); ok {
					if applied.RequestID != "exit-plan" || applied.Key != "mode" || applied.Value != scenario.desired {
						t.Fatalf("applied = %+v", applied)
					}
					break
				}
			}
			probe.noMessage()
			probe.close()
		}
	}
}

// The worker owns the mode it enforces: a caller outside it, such as an
// older controller, cannot select another mode, and nothing reaches Claude.
// Plan stays available.
// Hard-won: 9249142: a Guardian session put in bypassPermissions stayed there when Claude refused Auto
func TestACallerOutsideTheWorkerCannotMoveAPolicyOwnedMode(t *testing.T) {
	probe := planProbeWithConfig(t, core.ExecutionPolicyConfiguredApprovals, true)
	for _, change := range []struct{ requestID, value string }{
		{"accept-edits", "acceptEdits"},
		{"bypass", "bypassPermissions"},
	} {
		probe.command(CommandRequestSetConfig{RequestID: change.requestID, Key: "mode", Value: change.value})
		for refused := false; !refused; {
			switch event := probe.event().(type) {
			case core.RuntimeEventCommandRejected:
				if event.RequestID != change.requestID || !strings.Contains(event.Message, "permission policy") {
					t.Fatalf("rejected = %+v", event)
				}
				refused = true
			case core.RuntimeEventConfigApplied:
				t.Fatalf("%s must be refused", change.value)
			}
		}
		probe.noMessage()
	}
	setClaudeConfigMode(probe, "enter-plan", "plan")
	probe.close()
}

// `mode` reaches a bridge's Mode selector whatever its id, so the policy's
// ownership follows the same resolution.
// Hard-won: 853cd39: the `mode` alias reached a differently named policy selector unchecked
func TestAModeAliasIsCheckedAgainstTheSelectorItReaches(t *testing.T) {
	options := decodeAs[[]schema.SessionConfigOption](t, []any{map[string]any{
		"id": "interaction_mode", "name": "Mode", "category": "mode", "type": "select",
		"currentValue": "auto", "options": []any{
			map[string]any{"value": "auto", "name": "Auto"}, map[string]any{"value": "plan", "name": "Plan"},
			map[string]any{"value": "bypassPermissions", "name": "Bypass"},
		},
	}})
	guardian := claudeEnforcement(core.ExecutionPolicyConfiguredApprovals)
	for _, key := range []string{"mode", "interaction_mode"} {
		if policyPermits(guardian, options, &key, "bypassPermissions") {
			t.Fatal(key)
		}
		if !policyPermits(guardian, options, &key, "plan") {
			t.Fatal(key)
		}
	}
}

// claudeEnforcement is HarnessKind::Claude.execution_enforcement(policy).
func claudeEnforcement(policy core.ExecutionPolicy) *core.ExecutionEnforcement {
	enforcement, ok := core.HarnessKindClaude.ExecutionEnforcement(policy)
	if !ok {
		return nil
	}
	return &enforcement
}

// On a model without Auto the bridge does not list it, and the mode selector
// must stay the policy's: no other mode can be chosen then either.
// Hard-won: 49cecf8: without Auto on offer the mode selector stopped being owned
func TestAGuardianModeSelectorStaysOwnedWithoutAutoOnOffer(t *testing.T) {
	options := decodeAs[[]schema.SessionConfigOption](t, []any{map[string]any{
		"id": "mode", "name": "Mode", "category": "mode", "type": "select",
		"currentValue": "default", "options": []any{
			map[string]any{"value": "default", "name": "Default"}, map[string]any{"value": "plan", "name": "Plan"},
			map[string]any{"value": "bypassPermissions", "name": "Bypass"},
		},
	}})
	guardian := claudeEnforcement(core.ExecutionPolicyConfiguredApprovals)
	if keys := guardian.OwnedConfigKeys(options); !reflect.DeepEqual(keys, []string{"mode"}) {
		t.Fatalf("owned = %v", keys)
	}
	key := "mode"
	if policyPermits(guardian, options, &key, "bypassPermissions") {
		t.Fatal("bypass must be refused")
	}
	if !policyPermits(guardian, options, &key, "plan") {
		t.Fatal("plan must be permitted")
	}
}

// A `mode` selector without the optional Mode category is still the one a
// SetConfig for `mode` reaches, so the policy owns it.
// Hard-won: 9888b76: a `mode` selector without the Mode category was not owned
func TestAnUncategorizedModeSelectorIsPolicyOwned(t *testing.T) {
	options := decodeAs[[]schema.SessionConfigOption](t, []any{map[string]any{
		"id": "mode", "name": "Mode", "type": "select", "currentValue": "auto",
		"options": []any{
			map[string]any{"value": "auto", "name": "Auto"}, map[string]any{"value": "plan", "name": "Plan"},
			map[string]any{"value": "bypassPermissions", "name": "Bypass"},
		},
	}})
	guardian := claudeEnforcement(core.ExecutionPolicyConfiguredApprovals)
	if keys := guardian.OwnedConfigKeys(options); !reflect.DeepEqual(keys, []string{"mode"}) {
		t.Fatalf("owned = %v", keys)
	}
	key := "mode"
	if policyPermits(guardian, options, &key, "bypassPermissions") {
		t.Fatal("bypass must be refused")
	}
}

// A model whose catalogue omits Auto has refused it as plainly as an error
// answer: leaving Plan takes Claude's default mode with a warning.
// Hard-won: d33f5a5: an unadvertised policy mode failed the command instead of keeping the session usable
func TestLeavingPlanWithoutAutoOnOfferTakesClaudesDefaultMode(t *testing.T) {
	withoutAuto := func(current string) []any {
		return []any{map[string]any{
			"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": current,
			"options": []any{
				map[string]any{"value": "default", "name": "Default"}, map[string]any{"value": "plan", "name": "Plan"},
				map[string]any{"value": "acceptEdits", "name": "Accept edits"},
			},
		}}
	}
	probe := planProbeConfigOnly(t, core.ExecutionPolicyConfiguredApprovals)
	probe.command(CommandRequestSetConfig{RequestID: "enter-plan", Key: "mode", Value: "plan"})
	entering := probe.message()
	probe.result(entering, map[string]any{"configOptions": withoutAuto("plan")})
	probe.command(CommandRequestRestoreExecutionMode{RequestID: "exit-plan"})
	fallback := probe.message()
	if jsonAt(fallback, "params", "value") != "default" {
		t.Fatalf("fallback = %v", fallback)
	}
	probe.result(fallback, map[string]any{"configOptions": withoutAuto("default")})
	warned := false
	for done := false; !done; {
		switch event := probe.event().(type) {
		case core.RuntimeEventWarning:
			if strings.Contains(event.Message, "does not offer") {
				warned = true
			}
		case core.RuntimeEventConfigApplied:
			if event.RequestID == "exit-plan" {
				if event.Value != "default" {
					t.Fatalf("applied = %+v", event)
				}
				done = true
			}
		case core.RuntimeEventCommandRejected:
			t.Fatal(event.Message)
		case core.RuntimeEventStopped:
			t.Fatal("leaving Plan ended the session")
		}
	}
	if !warned {
		t.Fatal("leaving Plan for another mode must say why")
	}
	probe.noMessage()
	probe.close()
}

// Kimi carries its execution mode on the Plan selector, like Claude, so it
// leaves Plan through the worker, which restores the policy's own mode.
// Hard-won: ac3b724: refusing every mode but Plan and the policy's left Kimi stuck in Plan
func TestKimiLeavesPlanForThePolicyMode(t *testing.T) {
	probe := planProbeWithHarness(t, core.ExecutionPolicyUnconstrained, true, core.HarnessKindKimi)
	setClaudeConfigMode(probe, "enter-plan", "plan")
	var surface core.AcpSessionSurface
	surface.SetHarnessKind(core.HarnessKindKimi)
	surface.SetConfigOptions(decodeAs[[]schema.SessionConfigOption](t, modeConfig("plan")))
	control, controlErr := surface.PlanControl(false)
	if controlErr != nil {
		t.Fatalf("plan control refused: %v", *controlErr)
	}
	if _, ok := control.(core.PlanControlRestoreExecutionMode); !ok {
		t.Fatalf("plan control = %#v", control)
	}
	probe.command(CommandRequestRestoreExecutionMode{RequestID: "exit-plan"})
	restoring := probe.message()
	if restoring["method"] != "session/set_config_option" || jsonAt(restoring, "params", "value") != "auto" {
		t.Fatalf("restoring = %v", restoring)
	}
	// Hard-won: ce521dc: Kimi answers with the configuration it held before
	// the change, so a strict confirmation failed every Plan exit.
	probe.result(restoring, map[string]any{"configOptions": modeConfig("plan")})
	for done := false; !done; {
		switch event := probe.event().(type) {
		case core.RuntimeEventConfigApplied:
			if event.RequestID == "exit-plan" {
				if event.Value != "auto" {
					t.Fatalf("applied = %+v", event)
				}
				done = true
			}
		case core.RuntimeEventCommandRejected:
			t.Fatal(event.Message)
		}
	}
	probe.noMessage()
	probe.close()
}

// Hard-won: 734385a: a refused approval mode skipped Muse's guardian auto-review
func TestMuseGuardianKeepsAutoReviewWhenTheBridgeRefusesItsApprovalMode(t *testing.T) {
	// The probe's handshake asserts that `auto_review on` follows the refused
	// approval mode; the session then reports the refusal and keeps serving.
	probe := planProbeRefusingPolicyMode(t, core.ExecutionPolicyConfiguredApprovals, false, core.HarnessKindMuse)
	for {
		if warning, ok := probe.event().(core.RuntimeEventWarning); ok {
			if !strings.Contains(warning.Message, "this bridge refuses it") {
				t.Fatal(warning.Message)
			}
			break
		}
	}
	probe.noMessage()
	probe.close()
}

// Hard-won: 734385a: leaving Plan only retried a refused Auto, so Plan could not be left
func TestLeavingPlanTakesClaudesDefaultModeWhenAutoIsRefused(t *testing.T) {
	probe := planProbeRefusingPolicyMode(t, core.ExecutionPolicyConfiguredApprovals, true, core.HarnessKindClaude)
	setClaudeConfigMode(probe, "enter-plan", "plan")
	probe.command(CommandRequestRestoreExecutionMode{RequestID: "exit-plan"})
	auto := probe.message()
	if jsonAt(auto, "params", "value") != "auto" {
		t.Fatalf("auto = %v", auto)
	}
	probe.refuse(auto)
	fallback := probe.message()
	if fallback["method"] != "session/set_config_option" || jsonAt(fallback, "params", "value") != "default" {
		t.Fatalf("fallback = %v", fallback)
	}
	probe.result(fallback, map[string]any{"configOptions": modeConfig("default")})
	warned := false
	for done := false; !done; {
		switch event := probe.event().(type) {
		case core.RuntimeEventWarning:
			if strings.Contains(event.Message, "this bridge refuses it") {
				warned = true
			}
		case core.RuntimeEventConfigApplied:
			if event.RequestID == "exit-plan" {
				if event.Value != "default" {
					t.Fatalf("applied = %+v", event)
				}
				done = true
			}
		case core.RuntimeEventCommandRejected:
			t.Fatal(event.Message)
		}
	}
	if !warned {
		t.Fatal("leaving Plan for another mode must say why")
	}
	probe.noMessage()
	probe.close()
}

func TestARefusedClaudePlanExitReportsFailureWithoutContinuing(t *testing.T) {
	probe := planProbeWithConfig(t, core.ExecutionPolicyConfiguredApprovals, true)
	setClaudeConfigMode(probe, "enter-plan", "plan")
	probe.command(CommandRequestRestoreExecutionMode{RequestID: "exit-plan"})
	restoring := probe.message()
	// An acknowledgement that still reports Plan is not a successful exit.
	probe.result(restoring, map[string]any{"configOptions": modeConfig("plan")})
	for done := false; !done; {
		switch event := probe.event().(type) {
		case core.RuntimeEventConfigApplied:
			t.Fatal("unconfirmed mode must not succeed")
		case core.RuntimeEventCommandRejected:
			if event.RequestID != "exit-plan" || !strings.Contains(event.Message, "reports") {
				t.Fatalf("rejected = %+v", event)
			}
			done = true
		}
	}
	probe.noMessage()
}

func TestClaudePlanApprovalSelectsTheDeploymentModeWithoutClearingContext(t *testing.T) {
	for _, ids := range [][3]string{
		{"auto", "bypassPermissions", "default"},
		{"exit-plan-auto", "exit-plan-bypass", "exit-plan-default"},
	} {
		for _, reverse := range []bool{false, true} {
			options := []string{"exit-plan-clear-auto", "exit-plan-clear-bypass", ids[2], ids[0], ids[1]}
			if reverse {
				for i, j := 0, len(options)-1; i < j; i, j = i+1, j-1 {
					options[i], options[j] = options[j], options[i]
				}
			}
			request := decodeAs[schema.RequestPermissionRequest](t, planPermission("Approved plan", options))
			for _, scenario := range []struct {
				policy   core.ExecutionPolicy
				expected string
			}{
				{core.ExecutionPolicyConfiguredApprovals, ids[0]},
				{core.ExecutionPolicyUnconstrained, ids[1]},
			} {
				answer, err := policyPlanPermissionAnswer(&request, planImplement(), core.HarnessKindClaude, claudeEnforcement(scenario.policy))
				if err != nil {
					t.Fatal(err)
				}
				native, ok := answer.(PlanPermissionAnswerNative)
				if !ok {
					t.Fatal("offered mode must be selected directly")
				}
				if got := jsonAt(toValue(t, native.Value), "outcome", "optionId"); got != scenario.expected {
					t.Fatalf("selected %v, want %s", got, scenario.expected)
				}
			}
		}
	}
}

func TestClaudeMissingAutoIsAnErrorAndMissingBypassRequiresAContinuation(t *testing.T) {
	request := decodeAs[schema.RequestPermissionRequest](t, planPermission("Plan",
		[]string{"exit-plan-clear-auto", "default", "acceptEdits", "reject"}))
	_, err := policyPlanPermissionAnswer(&request, planImplement(), core.HarnessKindClaude,
		claudeEnforcement(core.ExecutionPolicyConfiguredApprovals))
	if err == nil || !strings.Contains(err.Error(), "required auto") {
		t.Fatalf("err = %v", err)
	}
	answer, err := policyPlanPermissionAnswer(&request, planImplement(), core.HarnessKindClaude,
		claudeEnforcement(core.ExecutionPolicyUnconstrained))
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := answer.(PlanPermissionAnswerContinueInBypass); !ok {
		t.Fatalf("answer = %#v", answer)
	}
	answer, err = policyPlanPermissionAnswer(&request, core.ElicitationResponse{Action: core.ElicitationResponseDecline},
		core.HarnessKindClaude, claudeEnforcement(core.ExecutionPolicyUnconstrained))
	if err != nil {
		t.Fatal(err)
	}
	native, ok := answer.(PlanPermissionAnswerNative)
	if !ok {
		t.Fatal("decline is native")
	}
	if got := jsonAt(toValue(t, native.Value), "outcome", "optionId"); got != "reject" {
		t.Fatalf("selected %v", got)
	}
}

func TestApprovedPlanWaitsForTurnAndModeAckThenContinuesOnceInSameSession(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	plan := strings.Repeat("Implement the parser and its behavior tests.\n", 2048)
	if len(plan) <= 65536 {
		t.Fatal("the plan crosses the pipe buffer")
	}
	prompt, id, answer := probe.approve(plan, []string{"exit-plan-clear-auto", "exit-plan-default", "exit-plan-auto", "reject"})
	if jsonAt(answer, "result", "outcome", "outcome") != "cancelled" {
		t.Fatalf("answer = %v", answer)
	}
	if probe.answer(id) == nil {
		t.Fatal("a duplicate answer must not schedule another continuation")
	}
	probe.noMessage()
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	mode := probe.message()
	if mode["method"] != "session/set_mode" || jsonAt(mode, "params", "modeId") != "bypassPermissions" {
		t.Fatalf("mode = %v", mode)
	}
	probe.noMessage()
	for _, event := range probe.drainEvents() {
		if _, finished := event.(core.RuntimeEventPromptFinished); finished {
			t.Fatal("the relay command must stay active through the transition")
		}
	}
	probe.result(mode, map[string]any{})
	continuation := probe.message()
	if continuation["method"] != "session/prompt" || jsonAt(continuation, "params", "sessionId") != "plan-session" {
		t.Fatalf("continuation = %v", continuation)
	}
	blocks, _ := jsonAt(continuation, "params", "prompt").([]any)
	text := jsonString(jsonAt(blocks[0], "text"))
	if !strings.HasPrefix(text, "The user approved") || !strings.HasSuffix(text, plan) {
		t.Fatalf("continuation text = %.80q", text)
	}
	probe.answered(continuation)
	if stopReason := probe.finished(); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.noMessage()
	probe.close()
}

// Claude answers a cancelled plan review with "Tool use aborted", so the
// planning cycle can end with an ordinary result rather than an interruption
// report. Neither that result nor a late relay of it may end the prompt the
// approved plan continues; the implementation prompt's response ends it.
func TestAPlanningResultNeverEndsThePromptAnApprovedPlanContinues(t *testing.T) {
	result := map[string]any{
		"type": "result", "subtype": "success", "is_error": false, "num_turns": 3,
		"stop_reason": "end_turn", "result": "done",
		"usage":  map[string]any{"input_tokens": 10, "output_tokens": 5},
		"origin": map[string]any{"kind": "human"},
	}
	for _, relayedLate := range []bool{false, true} {
		probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
		prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
		probe.sdkResult(result)
		probe.claudeResult()
		if !relayedLate {
			probe.sdkResult(result)
			probe.noFinish()
		}
		probe.result(prompt, map[string]any{"stopReason": "end_turn"})
		mode := probe.message()
		if mode["method"] != "session/set_mode" {
			t.Fatalf("mode = %v", mode)
		}
		probe.result(mode, map[string]any{})
		continuation := probe.message()
		if continuation["method"] != "session/prompt" {
			t.Fatalf("continuation = %v", continuation)
		}
		if relayedLate {
			probe.sdkResult(result)
			probe.noFinish()
		}
		probe.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
			"sessionId": "plan-session",
			"update":    map[string]any{"sessionUpdate": "agent_message_chunk", "content": map[string]any{"type": "text", "text": "implemented"}},
		}})
		probe.sdkResult(result)
		probe.claudeResult()
		probe.noFinish()
		probe.result(continuation, map[string]any{"stopReason": "end_turn"})
		if stopReason := probe.finished(); stopReason != "EndTurn" {
			t.Fatalf("%v: stop reason = %s", relayedLate, stopReason)
		}
		probe.noMessage()
		probe.noFinish()
		probe.close()
	}
}

func TestGuardianApprovalUsesAutoWithoutAFollowupPrompt(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	prompt, _, answer := probe.approve("Plan", []string{"exit-plan-default", "exit-plan-clear-auto", "exit-plan-auto", "reject"})
	if jsonAt(answer, "result", "outcome", "optionId") != "exit-plan-auto" {
		t.Fatalf("answer = %v", answer)
	}
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	if stopReason := probe.finished(); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.noMessage()
	probe.close()
}

func TestRejectedModeChangeNeverSubmitsTheApprovedPlan(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
	probe.result(prompt, map[string]any{"stopReason": "cancelled"})
	mode := probe.message()
	probe.send(map[string]any{"jsonrpc": "2.0", "id": mode["id"], "error": map[string]any{"code": -32603, "message": "mode unavailable"}})
	warned := false
	for done := false; !done; {
		switch event := probe.event().(type) {
		case core.RuntimeEventWarning:
			if strings.Contains(event.Message, "could not restore the policy mode") {
				warned = true
			}
		case core.RuntimeEventPromptFinished:
			if event.StopReason != "error" {
				t.Fatalf("finished = %+v", event)
			}
			done = true
		}
	}
	if !warned {
		t.Fatal("the failed restoration is reported")
	}
	probe.noMessage()
	probe.close()
}

func TestCancellingDuringModeRestorationDiscardsTheContinuation(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	mode := probe.message()
	probe.command(CommandRequestCancel{RequestID: "cancel"})
	if message := probe.message(); message["method"] != "session/cancel" {
		t.Fatalf("message = %v", message)
	}
	if stopReason := probe.finished(); stopReason != "Cancelled" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	cancelledRequest := probe.message()
	if cancelledRequest["method"] != "$/cancel_request" || !jsonEqual(jsonAt(cancelledRequest, "params", "requestId"), mode["id"]) {
		t.Fatalf("cancelled request = %v", cancelledRequest)
	}
	probe.result(mode, map[string]any{})
	probe.noMessage()
	probe.close()
}

func TestClosingDuringModeRestorationDiscardsTheContinuation(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	if message := probe.message(); message["method"] != "session/set_mode" {
		t.Fatalf("message = %v", message)
	}
	probe.close()
}

func TestFailedPlanningTurnDoesNotRestoreModeOrContinue(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
	probe.send(map[string]any{"jsonrpc": "2.0", "id": prompt["id"], "error": map[string]any{"code": -32603, "message": "planning failed"}})
	if stopReason := probe.finished(); stopReason != "error" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.noMessage()
	probe.close()
}

func TestCancellingWhileWaitingForThePlanningTurnPreventsModeRestoration(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
	probe.command(CommandRequestCancel{RequestID: "cancel"})
	if message := probe.message(); message["method"] != "session/cancel" {
		t.Fatalf("message = %v", message)
	}
	probe.result(prompt, map[string]any{"stopReason": "cancelled"})
	if stopReason := probe.finished(); stopReason != "Cancelled" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.noMessage()
	probe.close()
}

func TestPlanTransitionTimeoutRequestsARestartWithoutReplayingImplementation(t *testing.T) {
	// Rust pauses the clock and advances it past the bound; the bound here is
	// short instead.
	bound := cancelAckTimeout
	cancelAckTimeout = 300 * time.Millisecond
	t.Cleanup(func() { cancelAckTimeout = bound })
	for _, awaitingMode := range []bool{false, true} {
		probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
		prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
		if awaitingMode {
			probe.result(prompt, map[string]any{"stopReason": "end_turn"})
			if message := probe.message(); message["method"] != "session/set_mode" {
				t.Fatalf("message = %v", message)
			}
		}
		for {
			if warning, ok := probe.event().(core.RuntimeEventWarning); ok && strings.Contains(warning.Message, "waiting for Claude") {
				break
			}
		}
		warned := false
		for done := false; !done; {
			switch event := probe.event().(type) {
			case core.RuntimeEventWarning:
				if strings.Contains(event.Message, "Plan implementation timed out") {
					warned = true
				}
			case core.RuntimeEventCommandInterrupted:
				if event.RequestID != "original-prompt" {
					t.Fatalf("interrupted = %+v", event)
				}
				done = true
			case core.RuntimeEventPromptFinished:
				t.Fatal("timeout must interrupt the command")
			}
		}
		if !warned {
			t.Fatal("the timeout is reported")
		}
		restart, err := probe.outcome()
		if err != nil || !reflect.DeepEqual(restart, SessionRestart(SessionRestartResume{Value: "plan-session"})) {
			t.Fatalf("restart = %#v, err = %v", restart, err)
		}
		for line := range probe.remainingLines() {
			if decodeJSONValue(t, []byte(line)).(map[string]any)["method"] == "session/prompt" {
				t.Fatal("the implementation is never replayed")
			}
		}
	}
}

// remainingLines is every frame the runtime still sends until its stream
// ends.
func (p *acpProbe) remainingLines() <-chan string {
	lines := make(chan string)
	go func() {
		defer close(lines)
		for {
			select {
			case line := <-p.input:
				lines <- line
			case <-p.inputClosed:
				for {
					select {
					case line := <-p.input:
						lines <- line
					default:
						return
					}
				}
			case <-time.After(5 * time.Second):
				return
			}
		}
	}()
	return lines
}

func TestDroppingBridgeConnectionDiscardsAPendingImplementation(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, _ := probe.approve("Plan", []string{"exit-plan-auto", "reject"})
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	if message := probe.message(); message["method"] != "session/set_mode" {
		t.Fatalf("message = %v", message)
	}
	// runBridge drops the driver when its child exits; exercise that same
	// teardown while the mode acknowledgement is still outstanding.
	probe.cancel()
	if _, err := probe.outcome(); !errors.Is(err, context.Canceled) {
		t.Fatalf("the driver must be cancelled: %v", err)
	}
	for line := range probe.remainingLines() {
		if decodeJSONValue(t, []byte(line)).(map[string]any)["method"] == "session/prompt" {
			t.Fatal("the implementation is never submitted")
		}
	}
}

func TestGuardianWithoutAutoReportsFailureInsteadOfSelectingManualMode(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
	prompt, _, answer := probe.approve("Plan", []string{"exit-plan-default", "exit-plan-clear-auto", "reject"})
	if jsonAt(answer, "result", "outcome", "outcome") != "cancelled" {
		t.Fatalf("answer = %v", answer)
	}
	for {
		if warning, ok := probe.event().(core.RuntimeEventWarning); ok && strings.Contains(warning.Message, "required auto") {
			break
		}
	}
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	probe.finished()
	probe.noMessage()
	probe.close()
}

func TestOfferedBypassIsSelectedWithoutCancellingThePlanTurn(t *testing.T) {
	probe := newPlanProbe(t, core.ExecutionPolicyUnconstrained)
	prompt, _, answer := probe.approve("Plan",
		[]string{"exit-plan-clear-bypass", "exit-plan-default", "exit-plan-bypass", "exit-plan-auto", "reject"})
	if jsonAt(answer, "result", "outcome", "optionId") != "exit-plan-bypass" {
		t.Fatalf("answer = %v", answer)
	}
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	probe.finished()
	probe.noMessage()
	probe.close()
}

func TestConfigModeRestorationChecksTheModeReturnedByClaude(t *testing.T) {
	for _, returnedMode := range []string{"bypassPermissions", "auto"} {
		probe := planProbeWithConfig(t, core.ExecutionPolicyUnconstrained, true)
		prompt, _, _ := probe.approve("The approved plan", []string{"exit-plan-auto", "reject"})
		probe.result(prompt, map[string]any{"stopReason": "end_turn"})
		mode := probe.message()
		if mode["method"] != "session/set_config_option" || jsonAt(mode, "params", "value") != "bypassPermissions" {
			t.Fatalf("mode = %v", mode)
		}
		probe.noMessage()
		probe.result(mode, map[string]any{"configOptions": modeConfig(returnedMode)})
		if returnedMode == "bypassPermissions" {
			continuation := probe.message()
			if continuation["method"] != "session/prompt" {
				t.Fatalf("continuation = %v", continuation)
			}
			probe.answered(continuation)
			if stopReason := probe.finished(); stopReason != "EndTurn" {
				t.Fatalf("stop reason = %s", stopReason)
			}
		} else if stopReason := probe.finished(); stopReason != "error" {
			t.Fatalf("stop reason = %s", stopReason)
		}
		probe.noMessage()
		probe.close()
	}
}

// Hard-won: b81228f: ZCode close retries left the session permanently stuck in closing when session/close was unsupported.
func TestCloseIsAppliedWhenTheHarnessLacksSessionClose(t *testing.T) {
	for _, scenario := range []struct {
		code    int
		applied bool
	}{{-32601, true}, {-32603, false}} {
		probe := newPlanProbe(t, core.ExecutionPolicyConfiguredApprovals)
		probe.command(CommandRequestClose{RequestID: "close"})
		closing := probe.message()
		if closing["method"] == "session/cancel" {
			closing = probe.message()
		}
		if closing["method"] != "session/close" {
			t.Fatalf("close = %v", closing)
		}
		probe.send(map[string]any{
			"jsonrpc": "2.0", "id": closing["id"],
			"error": map[string]any{"code": scenario.code, "message": "Method not found"},
		})
		warned := false
		var outcome core.RuntimeEvent
		for outcome == nil {
			switch event := probe.event().(type) {
			case core.RuntimeEventWarning:
				if strings.Contains(event.Message, "session/close") {
					warned = true
				}
			case core.RuntimeEventCloseApplied, core.RuntimeEventCommandRejected:
				outcome = event
			}
		}
		switch event := outcome.(type) {
		case core.RuntimeEventCloseApplied:
			if !scenario.applied {
				t.Fatalf("error %d must not report the close as applied", scenario.code)
			}
			if event.RequestID != "close" || !warned {
				t.Fatalf("applied = %+v, warned = %v", event, warned)
			}
		case core.RuntimeEventCommandRejected:
			if scenario.applied {
				t.Fatal("method not found must apply the close")
			}
			if event.RequestID != "close" {
				t.Fatalf("rejected = %+v", event)
			}
		}
		restart, err := probe.outcome()
		if err != nil || restart != nil {
			t.Fatalf("restart = %#v, err = %v", restart, err)
		}
	}
}
