// Port of mj-tui/src/welcome.rs.
//
// The first-run welcome remains dismissible while background discovery runs.

package tui

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// WelcomeAction is a button of the first-run welcome.
type WelcomeAction uint8

const (
	WelcomeActionManagedSetup WelcomeAction = iota
	WelcomeActionContinue
)

// WelcomeDialog is the first-run welcome's text, discovery status and form.
type WelcomeDialog struct {
	lines []string
	// status is the discovery step in progress, nil once it is done.
	status *string
	// agentsFound is nil until discovery has looked.
	agentsFound *bool
	scroll      int
	maxScroll   int
	form        chat.Dialog[WelcomeAction]
}

func openWelcome(mode Mode) *WelcomeDialog {
	if welcome, ok := mode.(ModeWelcome); ok {
		return welcome.WelcomeDialog
	}
	return nil
}

func (d *DashboardState) BeginWelcome() {
	status := "Finding your agents…"
	d.pendingWelcome = &WelcomeDialog{
		status: &status,
		form:   chat.NewDialog[WelcomeAction](),
	}
	d.showPendingWelcome()
}

func (d *DashboardState) showPendingWelcome() {
	// Discovery must not replace a Settings draft opened while it started.
	if _, dashboard := d.mode.(ModeDashboard); dashboard && d.pendingWelcome != nil {
		d.mode = ModeWelcome{WelcomeDialog: d.pendingWelcome}
		d.pendingWelcome = nil
	}
}

func (d *DashboardState) welcomeMut() *WelcomeDialog {
	if dialog := openWelcome(d.mode); dialog != nil {
		return dialog
	}
	return d.pendingWelcome
}

func (d *DashboardState) WelcomeConfigured(lines []string, agentsFound bool) {
	if dialog := d.welcomeMut(); dialog != nil {
		status := "Checking prerequisites…"
		dialog.lines = lines
		dialog.agentsFound = &agentsFound
		dialog.status = &status
	}
}

func (d *DashboardState) WelcomeChecked(errors []string) {
	panic("unported: mj-tui/src/welcome.rs DashboardState::welcome_checked")
}

func (d *DashboardState) WelcomeNotNeeded() {
	d.pendingWelcome = nil
	if _, welcome := d.mode.(ModeWelcome); welcome {
		d.cancelModal()
	}
}

// handleWelcomeEvent routes an event to the welcome, which the caller took
// out of d.mode.
func (d *DashboardState) handleWelcomeEvent(event tea.Msg, dialog *WelcomeDialog) DashboardAction {
	panic("unported: mj-tui/src/welcome.rs DashboardState::handle_welcome_event")
}

func renderWelcome(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *WelcomeDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/welcome.rs render_welcome")
}
