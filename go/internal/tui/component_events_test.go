// Port of the tests in mj-tui/src/component_events.rs.

package tui

import (
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

func componentEventsDraw(dashboard *DashboardState) []string {
	return drawn(dashboard, 120, 35)
}

// componentEventsClick presses and releases the left button at position and
// returns what the release did.
func componentEventsClick(dashboard *DashboardState, position uv.Position) DashboardAction {
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, position))
	return dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, position))
}

func TestDoubleClickOnNextCannotSkipAWizardStep(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.beginNew()
	position := point(componentEventsDraw(dashboard), "Next")
	componentEventsClick(dashboard, position)
	if wizard, ok := dashboard.mode.(ModeNew); !ok || wizard.step != WizardStepTarget {
		t.Fatalf("mode = %#v, want the wizard on its Target step", dashboard.mode)
	}
	componentEventsDraw(dashboard)
	componentEventsClick(dashboard, position)
	if wizard, ok := dashboard.mode.(ModeNew); !ok || wizard.step != WizardStepTarget {
		t.Fatalf("mode = %#v, want the wizard still on its Target step", dashboard.mode)
	}
}

// Hard-won: 0e37e4b79b: click coordinates missed profile rows below the heading.
func TestClickingAProfileRowBelowTheHeadingSelectsThatProfile(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.beginNew()
	// The heading row sits above the profiles, so the third profile is the
	// fourth display row; the click has to land on the profile, not the row.
	position := point(componentEventsDraw(dashboard), "codex-2")
	componentEventsClick(dashboard, position)
	if wizard, ok := dashboard.mode.(ModeNew); !ok || wizard.profile != 2 {
		t.Fatalf("mode = %#v: the click did not select the third profile", dashboard.mode)
	}
}
