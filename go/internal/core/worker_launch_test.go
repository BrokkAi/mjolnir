// Port of the tests in mj-core/src/worker_launch.rs.

package core

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// workerLaunchJSON is the minimal launch file every test starts from.
func workerLaunchJSON() map[string]any {
	return map[string]any{
		"session_id":       "session",
		"harness":          "codex",
		"bridge_command":   "codex-acp",
		"bridge_args":      []any{},
		"environment":      map[string]any{},
		"cwd":              "/workspace/project",
		"execution_policy": "configured_approvals",
	}
}

// workerLaunchToValue is serde_json::to_value: a generic JSON value.
func workerLaunchToValue(t *testing.T, value any) map[string]any {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	return decoded
}

// workerLaunchFromValue is serde_json::from_value into T.
func workerLaunchFromValue[T any](t *testing.T, value any) (T, error) {
	t.Helper()
	var decoded T
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	err = json.Unmarshal(encoded, &decoded)
	return decoded, err
}

func workerLaunchMustDecode(t *testing.T, value any) WorkerLaunchConfig {
	t.Helper()
	launch, err := workerLaunchFromValue[WorkerLaunchConfig](t, value)
	if err != nil {
		t.Fatal(err)
	}
	return launch
}

func TestLegacyCPUWireAndLaunchDefaultToProcessTree(t *testing.T) {
	legacy := map[string]any{
		"recent_permille": 10, "hourly_permille": 20,
		"hourly_covered_secs": 30, "online_cpus": 4,
	}
	usage, err := workerLaunchFromValue[SessionCpuUsage](t, legacy)
	if err != nil {
		t.Fatal(err)
	}
	if usage.Scope != CpuSampleScopeProcessTree {
		t.Fatalf("scope %q", usage.Scope)
	}
	legacy["scope"] = "container"
	usage, err = workerLaunchFromValue[SessionCpuUsage](t, legacy)
	if err != nil || usage.Scope != CpuSampleScopeContainer {
		t.Fatalf("container scope %q %v", usage.Scope, err)
	}
	launch := workerLaunchMustDecode(t, workerLaunchJSON())
	if launch.CPUAccounting != CpuAccountingModeProcessTree {
		t.Fatalf("accounting %q", launch.CPUAccounting)
	}
	if _, ok := workerLaunchToValue(t, launch)["cpu_accounting"]; ok {
		t.Fatal("process tree accounting was written")
	}
	launch.CPUAccounting = CpuAccountingModeContainer
	saved := workerLaunchToValue(t, launch)
	if workerLaunchMustDecode(t, saved).CPUAccounting != CpuAccountingModeContainer {
		t.Fatal("container accounting lost")
	}
}

func TestWorkerLaunchRenamedOrchestratorFieldsKeepTheStrictLegacyWireShape(t *testing.T) {
	launch := workerLaunchMustDecode(t, workerLaunchJSON())
	baseline := workerLaunchToValue(t, launch)
	for _, flags := range [][2]bool{{true, false}, {false, true}} {
		orchestrator, enabled := flags[0], flags[1]
		launch.Orchestrator = orchestrator
		launch.OrchestratorsEnabled = enabled
		payload := workerLaunchToValue(t, launch)
		fields := map[string]any{}
		for key, value := range payload {
			if previous, ok := baseline[key]; !ok || !reflect.DeepEqual(previous, value) {
				fields[key] = value
			}
		}
		// The only changed fields are the retained legacy names, with the
		// strictness of #[serde(deny_unknown_fields)].
		retained := map[string]bool{"supervisor": false, "supervisors_enabled": false}
		for key, value := range fields {
			if _, ok := retained[key]; !ok {
				t.Fatalf("unexpected field %s", key)
			}
			flag, ok := value.(bool)
			if !ok {
				t.Fatalf("%s = %v", key, value)
			}
			retained[key] = flag
		}
		if retained["supervisor"] != orchestrator || retained["supervisors_enabled"] != enabled {
			t.Fatalf("retained %v for (%v, %v)", retained, orchestrator, enabled)
		}

		decoded := workerLaunchMustDecode(t, payload)
		if decoded.Orchestrator != orchestrator || decoded.OrchestratorsEnabled != enabled {
			t.Fatalf("decoded (%v, %v), want (%v, %v)", decoded.Orchestrator, decoded.OrchestratorsEnabled, orchestrator, enabled)
		}
	}
}

// A launch file written before per-turn review was removed still names
// `"turn"`. It must keep parsing, and reads as the push gate.
func TestALaunchFileWrittenForTurnReviewStillParsesAsPush(t *testing.T) {
	value := workerLaunchJSON()
	value["review_trigger"] = "turn"
	value["review_capture"] = true
	launch := workerLaunchMustDecode(t, value)
	if launch.ReviewTrigger != ReviewTriggerPush {
		t.Fatalf("trigger %q", launch.ReviewTrigger)
	}
	if !launch.ReviewGate() {
		t.Fatal("no review gate")
	}
	rewritten := workerLaunchToValue(t, launch)
	if rewritten["review_trigger"] != "push" {
		t.Fatalf("rewritten trigger %v", rewritten["review_trigger"])
	}
	if _, ok := rewritten["review_capture"]; ok {
		t.Fatal("review_capture was written")
	}
}

// Hard-won: 32d96d199: push reviews exposed delegation tools with subagents=None.
func TestPersistedReviewTriggerControlsPrimaryMCPWithoutLeakingToChildren(t *testing.T) {
	for _, harness := range []HarnessKind{
		HarnessKindClaude,
		HarnessKindCodex,
		HarnessKindDsh,
		HarnessKindKimi,
		HarnessKindMuse,
		// Delegates through Mjolnir but has no push review gate.
		HarnessKindOpenCode,
	} {
		for _, trigger := range []ReviewTrigger{ReviewTriggerOff, ReviewTriggerPush} {
			value := workerLaunchJSON()
			value["harness"] = harness
			value["review_trigger"] = trigger
			launch := workerLaunchMustDecode(t, value)
			read := workerLaunchMustDecode(t, workerLaunchToValue(t, launch))
			if read.ReviewTrigger != trigger {
				t.Fatalf("%s: trigger %q, want %q", harness, read.ReviewTrigger, trigger)
			}
			expected := trigger == ReviewTriggerPush && harness != HarnessKindMuse && harness != HarnessKindOpenCode
			if launch.ReviewGate() != expected {
				t.Fatalf("%s %s: review gate %v", harness, trigger, launch.ReviewGate())
			}
			for _, policy := range []SubagentPolicy{
				{Mode: SubagentPolicyNone},
				{Mode: SubagentPolicyNative},
				{Mode: SubagentPolicyMjolnir},
			} {
				launch.Subagents = policy
				read := workerLaunchMustDecode(t, workerLaunchToValue(t, launch))
				role, ok := read.SubagentMCPRole()
				switch {
				case policy.Mode == SubagentPolicyMjolnir:
					if !ok || role != SubagentMcpRoleParent {
						t.Fatalf("%s %s %s: role %q %v", harness, trigger, policy.Mode, role, ok)
					}
				case expected:
					if !ok || role != SubagentMcpRoleMessageOnly {
						t.Fatalf("%s %s %s: role %q %v", harness, trigger, policy.Mode, role, ok)
					}
				default:
					if ok {
						t.Fatalf("%s %s %s: role %q", harness, trigger, policy.Mode, role)
					}
				}
			}
			launch.Subagents = SubagentPolicy{Mode: SubagentPolicyNone}
			launch.Environment[SessionManagedSubagentEnv] = "1"
			// Managed identity does not suppress an owning launch's gate.
			if launch.ReviewGate() != expected {
				t.Fatalf("%s %s: managed review gate %v", harness, trigger, launch.ReviewGate())
			}
			if role, ok := launch.SubagentMCPRole(); !ok || role != SubagentMcpRoleFetchOnly {
				t.Fatalf("sub-agent role %q %v", role, ok)
			}
			launch.HandbackTool = true
			if role, ok := launch.SubagentMCPRole(); !ok || role != SubagentMcpRoleChild {
				t.Fatalf("handback role %q %v", role, ok)
			}
		}
	}
}

func TestUnknownLaunchFieldsExplainTheWorkerDaemonBuildMismatch(t *testing.T) {
	path := filepath.Join(t.TempDir(), "launch.json")
	launch := workerLaunchJSON()
	launch["future_daemon_field"] = true
	body, err := json.Marshal(launch)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, body, 0o644); err != nil {
		t.Fatal(err)
	}
	_, err = WorkerLaunchConfigRead(path)
	if err == nil {
		t.Fatal("unknown field accepted")
	}
	message := err.Error()
	if !strings.Contains(message, "older than the daemon") || !strings.Contains(message, "future_daemon_field") || !strings.Contains(message, WorkerBuildID()) {
		t.Fatal(message)
	}
	if err := os.WriteFile(path, []byte("not JSON"), 0o644); err != nil {
		t.Fatal(err)
	}
	_, err = WorkerLaunchConfigRead(path)
	if err == nil || strings.Contains(err.Error(), "older than the daemon") {
		t.Fatal(err)
	}
}

// Hard-won: 5fc728617: a brief writer must never silently receive the reviewer contract.
func TestReviewerLaunchRolesKeepOldReviewersReadableAndRejectUnknownContracts(t *testing.T) {
	value := map[string]any{
		"profile_id": "reviewer", "harness": "codex", "bridge_command": "bridge",
		"bridge_args": []any{}, "execution_policy": "configured_approvals",
	}
	// Build from an existing launch fixture's policy representation.
	value["execution_policy"] = ExecutionPolicyConfiguredApprovals
	reviewer, err := workerLaunchFromValue[ReviewerLaunchConfig](t, value)
	if err != nil {
		t.Fatal(err)
	}
	if reviewer.Role != ReviewChildRoleReviewer {
		t.Fatalf("role %v", reviewer.Role)
	}
	if _, ok := workerLaunchToValue(t, reviewer)["role"]; ok {
		t.Fatal("reviewer role was written")
	}
	value["role"] = "brief_writer"
	writer, err := workerLaunchFromValue[ReviewerLaunchConfig](t, value)
	if err != nil {
		t.Fatal(err)
	}
	if writer.Role != ReviewChildRoleBriefWriter {
		t.Fatalf("role %v", writer.Role)
	}
	if role := workerLaunchToValue(t, writer)["role"]; role != "brief_writer" {
		t.Fatalf("written role %v", role)
	}
	value["role"] = "future_role"
	if _, err := workerLaunchFromValue[ReviewerLaunchConfig](t, value); err == nil {
		t.Fatal("future role accepted")
	}
	delete(value, "role")
	value["future_role"] = true
	if _, err := workerLaunchFromValue[ReviewerLaunchConfig](t, value); err == nil || !strings.Contains(err.Error(), "unknown field") {
		t.Fatal(err)
	}
}

func workerLaunchReadRewrite(t *testing.T, field string, fieldValue any) (WorkerLaunchConfig, map[string]any) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "launch.json")
	value := workerLaunchJSON()
	value[field] = fieldValue
	body, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, body, 0o644); err != nil {
		t.Fatal(err)
	}
	launch, err := WorkerLaunchConfigRead(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := launch.Write(path); err != nil {
		t.Fatal(err)
	}
	saved, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var written map[string]any
	if err := json.Unmarshal(saved, &written); err != nil {
		t.Fatal(err)
	}
	return launch, written
}

func TestSavedLaunchDropsTheRetiredRuntimeConstraint(t *testing.T) {
	launch, saved := workerLaunchReadRewrite(t, "expected_runtime_identity", "mj-runtime-v1:retired")
	if launch.SessionID != "session" || launch.BridgeCommand != "codex-acp" {
		t.Fatalf("launch %q %q", launch.SessionID, launch.BridgeCommand)
	}
	if _, ok := saved["expected_runtime_identity"]; ok {
		t.Fatal("retired runtime constraint was written")
	}
}

func TestSavedLaunchDropsTheRetiredBifrostBinary(t *testing.T) {
	_, saved := workerLaunchReadRewrite(t, "bifrost_binary", "/usr/local/bin/bifrost")
	if _, ok := saved["bifrost_binary"]; ok {
		t.Fatal("retired Bifrost binary was written")
	}
}

func TestOldLaunchConfigsDefaultToAmbientHarnesses(t *testing.T) {
	launch := workerLaunchMustDecode(t, workerLaunchJSON())
	if launch.HarnessRuntime != HarnessRuntimePolicyAmbient || !launch.AgentMailboxesEnabled {
		t.Fatalf("runtime %v mailboxes %v", launch.HarnessRuntime, launch.AgentMailboxesEnabled)
	}

	disabled := workerLaunchJSON()
	disabled["agent_mailboxes_enabled"] = false
	if workerLaunchMustDecode(t, disabled).AgentMailboxesEnabled {
		t.Fatal("mailboxes stayed enabled")
	}
}

func TestBuiltInContainerLaunchersArePreparedBeforeStart(t *testing.T) {
	for _, harness := range HarnessKindAll {
		command, args := FallbackLaunch(harness, ExecutionPolicyUnconstrained)
		value := workerLaunchJSON()
		value["harness"] = harness
		value["bridge_command"] = command
		value["bridge_args"] = args
		value["execution_policy"] = "unconstrained"
		launch := workerLaunchMustDecode(t, value)
		if !launch.RequiresHarnessPreparation() {
			t.Fatalf("%s must decide between its target launcher and managed installation", harness.DisplayName())
		}
	}

	custom := workerLaunchJSON()
	custom["bridge_command"] = "/opt/custom-agent"
	customLaunch := workerLaunchMustDecode(t, custom)
	if customLaunch.RequiresHarnessPreparation() {
		t.Fatal("custom launcher prepared")
	}
}

// Older controllers required both task types, so a launch without the field
// offers subtle. Only the straightforward-only case is written, so a launch
// offering both stays readable by an older worker.
func TestSubtleSubagentsDefaultsOnAndIsWrittenOnlyWhenOff(t *testing.T) {
	launch := workerLaunchMustDecode(t, workerLaunchJSON())
	if !launch.SubtleSubagents {
		t.Fatal("subtle off by default")
	}
	if _, ok := workerLaunchToValue(t, launch)["subtle_subagents"]; ok {
		t.Fatal("default subtle_subagents was written")
	}

	onlyStraightforward := workerLaunchJSON()
	onlyStraightforward["subtle_subagents"] = false
	launch = workerLaunchMustDecode(t, onlyStraightforward)
	if launch.SubtleSubagents {
		t.Fatal("subtle stayed on")
	}
	if written := workerLaunchToValue(t, launch)["subtle_subagents"]; written != false {
		t.Fatalf("subtle_subagents %v", written)
	}
}

func TestOracleDefaultsOffAndIsWrittenOnlyWhenOn(t *testing.T) {
	launch := workerLaunchMustDecode(t, workerLaunchJSON())
	if launch.Oracle || launch.SubagentKind != nil {
		t.Fatalf("oracle %v kind %v", launch.Oracle, launch.SubagentKind)
	}
	if _, ok := workerLaunchToValue(t, launch)["subagent_kind"]; ok {
		t.Fatal("absent subagent_kind was written")
	}
	for _, kind := range []SubagentKind{SubagentKindWorker, SubagentKindReviewer, SubagentKindOracle} {
		wire := workerLaunchJSON()
		wire["subagent_kind"] = kind
		decoded := workerLaunchMustDecode(t, wire)
		if decoded.SubagentKind == nil || *decoded.SubagentKind != kind {
			t.Fatalf("kind %v", decoded.SubagentKind)
		}
		if written := workerLaunchToValue(t, decoded)["subagent_kind"]; written != string(kind) {
			t.Fatalf("written kind %v", written)
		}
	}
	if _, ok := workerLaunchToValue(t, launch)["oracle"]; ok {
		t.Fatal("oracle off was written")
	}
	on := workerLaunchJSON()
	on["oracle"] = true
	launch = workerLaunchMustDecode(t, on)
	if !launch.Oracle {
		t.Fatal("oracle off")
	}
	if written := workerLaunchToValue(t, launch)["oracle"]; written != true {
		t.Fatalf("oracle %v", written)
	}
}

// Hard-won: f9a3130c: upgraded workers on older launches demanded an unstaged Claude registration.
func TestALaunchFromAnOlderControllerGetsNoMessageOnlyServer(t *testing.T) {
	// An older controller staged no `mj-agents` server for a top-level
	// session and wrote no marker; mailboxes alone must not imply one.
	launch := workerLaunchMustDecode(t, workerLaunchJSON())
	if !launch.AgentMailboxesEnabled {
		t.Fatal("mailboxes off")
	}
	if role, ok := launch.SubagentMCPRole(); ok {
		t.Fatalf("role %q", role)
	}

	staged := workerLaunchJSON()
	staged["environment"] = map[string]any{SessionMessageMCPEnv: "1"}
	launch = workerLaunchMustDecode(t, staged)
	if role, ok := launch.SubagentMCPRole(); !ok || role != SubagentMcpRoleMessageOnly {
		t.Fatalf("role %q %v", role, ok)
	}
}

func TestALaunchFromADaemonWithObserveGuardrailsStillLoadsAndDropsThem(t *testing.T) {
	// Older daemons wrote Brokk Observe settings under `observe_policy`.
	// The struct rejects unknown fields, so the name must stay accepted.
	value := workerLaunchJSON()
	value["observe_policy"] = map[string]any{
		"url": "http://observe.invalid", "token": "secret", "timeout_ms": 3000, "fail_closed": true,
	}
	launch := workerLaunchMustDecode(t, value)
	if launch.ObservePolicy == nil {
		t.Fatal("observe policy not recorded")
	}
	if _, ok := workerLaunchToValue(t, launch)["observe_policy"]; ok {
		t.Fatal("observe policy was written")
	}
}

func TestManagedPolicyAcceptsNewAndLegacyWireNames(t *testing.T) {
	value := workerLaunchJSON()
	value["harness_runtime"] = "managed_remote"
	launch := workerLaunchMustDecode(t, value)
	if launch.HarnessRuntime != HarnessRuntimePolicyManaged {
		t.Fatalf("runtime %v", launch.HarnessRuntime)
	}
	if written := workerLaunchToValue(t, launch)["harness_runtime"]; written != "managed_remote" {
		t.Fatalf("written runtime %v", written)
	}

	value = workerLaunchJSON()
	value["harness_runtime"] = "managed"
	if workerLaunchMustDecode(t, value).HarnessRuntime != HarnessRuntimePolicyManaged {
		t.Fatal("legacy managed name rejected")
	}
}

func TestLaunchPolicyAcceptsLegacySelectorsAndBooleans(t *testing.T) {
	effort := "max"
	luna := &SubagentChoice{Model: "gpt-6-luna", Effort: &effort}
	allModels := SubagentPolicy{Mode: SubagentPolicyMjolnir}
	for index, c := range []struct {
		policy   any
		expected SubagentPolicy
	}{
		{map[string]any{"mode": "mjolnir", "model": "gpt-6-luna", "effort": "max"},
			SubagentPolicy{Mode: SubagentPolicyMjolnir, Straightforward: luna, Subtle: luna}},
		{map[string]any{"mode": "all_models"}, allModels},
		{map[string]any{"mode": "single_model", "model": "chosen", "effort": "high"}, allModels},
		{map[string]any{"mode": "single_model", "model": "chosen", "effort": "adaptive"}, allModels},
		{true, allModels},
		{false, SubagentPolicy{Mode: SubagentPolicyNative}},
	} {
		value := workerLaunchJSON()
		value["subagents"] = c.policy
		launch := workerLaunchMustDecode(t, value)
		if !reflect.DeepEqual(launch.Subagents, c.expected) {
			t.Fatalf("case %d: %s, want %s", index, fmt.Sprint(launch.Subagents), fmt.Sprint(c.expected))
		}
	}
}
