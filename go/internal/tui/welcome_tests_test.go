// Port of mj-tui/src/welcome/tests.rs.

package tui

import (
	"fmt"
	"reflect"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func welcomeDashboard() *DashboardState {
	return NewDashboardState(core.DefaultConfig().WithLocalTargets(), core.NewStateState(), nil)
}

func TestWelcomeCanBeDismissedDuringDiscoveryAndLateErrorsBecomeNotices(t *testing.T) {
	for _, code := range []rune{tea.KeyEnter, tea.KeyEscape} {
		dashboard := welcomeDashboard()
		dashboard.BeginWelcome()
		drawn(dashboard, 100, 30)
		if action := dashboard.HandleKey(key(code)); !reflect.DeepEqual(action, DashboardActionNone{}) {
			t.Fatalf("key %q: action = %#v, want None", code, action)
		}
		if _, ok := dashboard.mode.(ModeDashboard); !ok {
			t.Fatalf("key %q: mode = %#v, want the dashboard", code, dashboard.mode)
		}
		dashboard.WelcomeConfigured([]string{"Agents: Claude Code."}, true)
		dashboard.WelcomeChecked([]string{"Run mj login --profile claude"})
		if _, ok := dashboard.mode.(ModeDashboard); !ok {
			t.Fatalf("key %q: mode = %#v, want the dashboard", code, dashboard.mode)
		}
		found := false
		for _, notice := range dashboard.notices.History() {
			if notice.Failure && strings.Contains(notice.Text, "mj login") {
				found = true
			}
		}
		if !found {
			t.Fatalf("key %q: no failure notice mentions mj login: %#v", code, dashboard.notices.History())
		}
	}
}

func TestFirstRunDoesNotReplaceAnOpenSettingsDraft(t *testing.T) {
	dashboard := welcomeDashboard()
	dashboard.BeginSetup()
	dashboard.BeginWelcome()
	dashboard.WelcomeConfigured([]string{"Agents: Codex."}, true)
	dashboard.WelcomeChecked(nil)
	if _, ok := dashboard.mode.(ModeSetup); !ok {
		t.Fatalf("mode = %#v, want Settings", dashboard.mode)
	}
	dashboard.CancelModal()
	text := strings.Join(drawn(dashboard, 100, 30), "\n")
	if !strings.Contains(text, "Welcome to Mjolnir") {
		t.Fatalf("the welcome is not drawn:\n%s", text)
	}
	if !strings.Contains(text, "Agents: Codex.") {
		t.Fatalf("the agents line is not drawn:\n%s", text)
	}
}

// Hard-won: 5238411cd: a no-setup result left an automatic Welcome open or pending.
func TestExistingSetupCompletionRetiresWelcomeWithoutClosingAnotherDialog(t *testing.T) {
	for _, settingsOpen := range []bool{false, true} {
		dashboard := welcomeDashboard()
		if settingsOpen {
			dashboard.BeginSetup()
		}
		dashboard.BeginWelcome()
		dashboard.WelcomeNotNeeded()
		if settingsOpen {
			if _, ok := dashboard.mode.(ModeSetup); !ok {
				t.Fatalf("mode = %#v, want Settings", dashboard.mode)
			}
			dashboard.CancelModal()
		}
		if _, ok := dashboard.mode.(ModeDashboard); !ok {
			t.Fatalf("settings open %v: mode = %#v, want the dashboard", settingsOpen, dashboard.mode)
		}
		text := strings.Join(drawn(dashboard, 100, 30), "\n")
		if strings.Contains(text, "Welcome to Mjolnir") {
			t.Fatalf("settings open %v: the welcome is still drawn:\n%s", settingsOpen, text)
		}
	}
}

func TestWelcomeOffersManagedSetupWhenNoAgentWasFound(t *testing.T) {
	dashboard := welcomeDashboard()
	dashboard.BeginWelcome()
	dashboard.WelcomeConfigured(
		[]string{"No coding agent found yet. Mjolnir can install one and sign you in."},
		false,
	)
	dashboard.WelcomeChecked(nil)
	lines := drawn(dashboard, 110, 32)
	found := false
	for _, line := range lines {
		if strings.Contains(line, "No coding agent found yet") {
			found = true
		}
	}
	if !found {
		t.Fatalf("the no-agent line is not drawn:\n%s", strings.Join(lines, "\n"))
	}
	at := point(lines, "Set up a managed agent")
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, at))
	action := dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, at))
	if !reflect.DeepEqual(action, DashboardActionOpenManagedSetup{}) {
		t.Fatalf("action = %#v, want OpenManagedSetup", action)
	}
}

func TestGoldenFirstRunWelcome(t *testing.T) {
	var output strings.Builder
	for _, run := range []struct {
		label  string
		agents bool
	}{{"no agent found", false}, {"agents found", true}} {
		dashboard := welcomeDashboard()
		dashboard.BeginWelcome()
		line := "No coding agent found yet. Mjolnir can install one and sign you in."
		if run.agents {
			line = "Agents: Codex, Claude Code."
		}
		dashboard.WelcomeConfigured([]string{line}, run.agents)
		dashboard.WelcomeChecked(nil)
		lines := drawn(dashboard, 100, 24)
		fmt.Fprintf(&output, "=== %s (100x24) ===\n%s\n", run.label, strings.Join(lines, "\n"))
	}
	core.AssertGolden(t, "testdata", "first-run-welcome", strings.TrimRight(output.String(), "\n"))
}
