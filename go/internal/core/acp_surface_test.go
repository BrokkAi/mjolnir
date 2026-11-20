// Port of the tests in mj-core/src/acp/surface.rs.

package core

import (
	"testing"

	"github.com/BrokkAi/acp-go/schema"
)

func surfaceSelect(key, name, current string, options ...schema.SessionConfigSelectOption) schema.SessionConfigOption {
	return schema.SessionConfigOption{
		Kind: schema.SessionConfigOptionKindSelect,
		ID:   schema.SessionConfigId(key),
		Name: name,
		Select: &schema.SessionConfigSelect{
			CurrentValue: schema.SessionConfigValueId(current),
			Options:      options,
		},
	}
}

func surfaceChoice(value, name string) schema.SessionConfigSelectOption {
	return schema.SessionConfigSelectOption{Value: schema.SessionConfigValueId(value), Name: name}
}

func surfaceModeOption(key, current string) schema.SessionConfigOption {
	return surfaceSelect(key, "Mode", current,
		surfaceChoice("default", "Default"),
		surfaceChoice("plan", "Plan"),
		surfaceChoice("bypassPermissions", "Bypass permissions"),
	)
}

func surfaceModes(current string) *schema.SessionModeState {
	return &schema.SessionModeState{
		CurrentModeID: schema.SessionModeId(current),
		AvailableModes: []schema.SessionMode{
			{ID: "default", Name: "Default"},
			{ID: "plan", Name: "Plan"},
			{ID: "bypassPermissions", Name: "Bypass permissions"},
		},
	}
}

func surfaceCurrentMode(t *testing.T, surface *AcpSessionSurface, want string) {
	t.Helper()
	if mode, ok := surface.CurrentMode(); !ok || mode != want {
		t.Fatalf("current mode = %q, %v; want %q", mode, ok, want)
	}
}

func TestConfigChurnDoesNotRevertAnInFlightPlanChange(t *testing.T) {
	var surface AcpSessionSurface
	surface.SetHarnessKind(HarnessKindClaude)
	surface.SetConfigOptions([]schema.SessionConfigOption{surfaceModeOption("mode", "default")})
	surface.BeginPlanModeChange(true)

	churned := surfaceModeOption("mode", "default")
	description := "metadata changed"
	churned.Description = &description
	surface.SetConfigOptions([]schema.SessionConfigOption{churned})

	surfaceCurrentMode(t, &surface, "plan")
	surface.FinishPlanModeChange(true)
	surfaceCurrentMode(t, &surface, "plan")
}

// Hard-won: f8bb425: a late Claude mode update reverted the user-confirmed Plan selector to permission mode.
func TestClaudeModeUpdateDoesNotRevertPlanConfigToPermissionMode(t *testing.T) {
	var surface AcpSessionSurface
	surface.SetHarnessKind(HarnessKindClaude)
	surface.SetConfigOptions([]schema.SessionConfigOption{surfaceModeOption("mode", "bypassPermissions")})
	surface.SetSessionModes(surfaceModes("bypassPermissions"))

	surface.BeginPlanModeChange(true)
	surface.SetConfigOptions([]schema.SessionConfigOption{surfaceModeOption("mode", "plan")})
	surface.FinishPlanModeChange(true)

	// A delayed mode notification must not override the selector state.
	surface.ApplyCurrentModeUpdate("bypassPermissions")

	surfaceCurrentMode(t, &surface, "plan")
	if !surface.PlanModeActive() {
		t.Fatal("plan mode is not active")
	}
}

func TestClaudeModeUpdateRemainsAuthoritativeWithoutAConfigSelector(t *testing.T) {
	var surface AcpSessionSurface
	surface.SetHarnessKind(HarnessKindClaude)
	surface.SetSessionModes(surfaceModes("bypassPermissions"))

	surface.ApplyCurrentModeUpdate("plan")

	surfaceCurrentMode(t, &surface, "plan")
}

// Hard-won: 60145fd: leaving Claude Plan mode discarded the prior worker policy and confirmed user mode.
func TestClaudePlanExitRestoresWorkerPolicyAndKeepsTheConfirmedMode(t *testing.T) {
	for _, restored := range []string{"auto", "bypassPermissions"} {
		var surface AcpSessionSurface
		surface.SetHarnessKind(HarnessKindClaude)
		surface.SetConfigOptions([]schema.SessionConfigOption{surfaceModeOption("mode", "plan")})
		if control, failure := surface.PlanControl(false); failure != nil || control != (PlanControlRestoreExecutionMode{}) {
			t.Fatalf("plan control = %#v, %v", control, failure)
		}
		surface.BeginPlanModeChange(false)
		surface.SetConfigOptions([]schema.SessionConfigOption{surfaceModeOption("mode", restored)})
		surface.FinishPlanModeChange(false)
		surfaceCurrentMode(t, &surface, restored)
		if surface.PlanModeActive() {
			t.Fatalf("%s: plan mode is active", restored)
		}
	}
}

func TestCodexUsesOneModeKeyForSnapshotsOptionsAndProjection(t *testing.T) {
	surface := AcpSessionSurfaceFromConfiguration(map[string]any{
		"mode":               "default",
		"collaboration_mode": "plan",
	})
	surface.SetHarnessKind(HarnessKindCodex)
	surfaceCurrentMode(t, &surface, "plan")

	surface.SetConfigOptions([]schema.SessionConfigOption{surfaceModeOption("collaboration_mode", "default")})
	surfaceCurrentMode(t, &surface, "default")
	surface.ApplyProjectedConfiguration(map[string]any{"collaboration_mode": "plan"})
	surfaceCurrentMode(t, &surface, "plan")
}

// Leaving Plan selects the normal mode each harness's `mode` select offers:
// OpenCode's `build` and dsh-acp's `agent`.
func TestPlanExitSelectsEachHarnessOwnNormalMode(t *testing.T) {
	for _, row := range []struct {
		harness HarnessKind
		normal  string
	}{
		{HarnessKindOpenCode, "build"},
		{HarnessKindDsh, "agent"},
	} {
		var surface AcpSessionSurface
		surface.SetHarnessKind(row.harness)
		surface.SetConfigOptions([]schema.SessionConfigOption{surfaceSelect("mode", "Mode", row.normal,
			surfaceChoice(row.normal, "Normal"),
			surfaceChoice("plan", "Plan"),
		)})

		for _, step := range []struct {
			active bool
			value  string
		}{{true, "plan"}, {false, row.normal}} {
			control, failure := surface.PlanControl(step.active)
			want := PlanControlSetConfig{Key: "mode", Value: step.value}
			if failure != nil || control != want {
				t.Fatalf("%v active=%v: %#v, %v", row.harness, step.active, control, failure)
			}
		}
	}
}
