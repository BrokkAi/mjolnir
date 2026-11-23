// Port of mj-tui/src/welcome.rs.
//
// The first-run welcome remains dismissible while background discovery runs.

package tui

import (
	"fmt"
	"math"
	"strings"

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
	dialog := d.welcomeMut()
	if dialog == nil {
		for _, failure := range errors {
			d.SetFailureNotice(failure)
		}
		return
	}
	dialog.status = nil
	for _, failure := range errors {
		dialog.lines = append(dialog.lines, "")
		dialog.lines = append(dialog.lines, welcomeStrLines(failure)...)
	}
}

// welcomeStrLines splits text as Rust's str::lines does: on "\n", dropping
// one trailing "\r" per line and the empty piece after a final newline.
func welcomeStrLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for index, line := range lines {
		lines[index] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

func (d *DashboardState) WelcomeNotNeeded() {
	d.pendingWelcome = nil
	if _, welcome := d.mode.(ModeWelcome); welcome {
		d.CancelModal()
	}
}

// handleWelcomeEvent routes an event to the welcome, which the caller took
// out of d.mode.
func (d *DashboardState) handleWelcomeEvent(event tea.Msg, dialog *WelcomeDialog) DashboardAction {
	if key, ok := event.(tea.KeyPressMsg); ok {
		var scroll int
		scrolled := true
		switch chat.KeyPressCode(key) {
		case tea.KeyDown:
			scroll = dialog.scroll + 1
		case tea.KeyUp:
			scroll = max(dialog.scroll-1, 0)
		case tea.KeyPgDown:
			scroll = dialog.scroll + 5
		case tea.KeyPgUp:
			scroll = max(dialog.scroll-5, 0)
		case tea.KeyHome:
			scroll = 0
		case tea.KeyEnd:
			scroll = dialog.maxScroll
		default:
			scrolled = false
		}
		if scrolled {
			dialog.scroll = min(scroll, dialog.maxScroll)
			d.lastEventConsumed = true
			d.mode = ModeWelcome{WelcomeDialog: dialog}
			return DashboardActionNone{}
		}
	}
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch action := result.Action.(type) {
	case chat.InteractionActivate[WelcomeAction]:
		if action.ID == WelcomeActionManagedSetup {
			d.CancelModal()
			return DashboardActionOpenManagedSetup{}
		}
		d.CancelModal()
	case chat.InteractionCancel[WelcomeAction]:
		d.CancelModal()
	default:
		d.mode = ModeWelcome{WelcomeDialog: dialog}
	}
	return DashboardActionNone{}
}

func renderWelcome(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *WelcomeDialog, surfaces *chat.FrameSurfaces) {
	lines := make([]chat.Line, 0, len(dialog.lines)+3)
	for _, line := range dialog.lines {
		lines = append(lines, chat.RawLine(line))
	}
	if dialog.status != nil {
		lines = append(lines, chat.StyledLine(*dialog.status, chat.Muted()))
	}
	if dialog.agentsFound != nil {
		lines = append(lines, chat.RawLine(""))
		if *dialog.agentsFound {
			text := "Choose Create session from Commands to start."
			if key, ok := dashboard.FirstKeyLabel(CommandIdNewSessionWizard); ok {
				text = fmt.Sprintf("Press %s to create a session.", key)
			}
			lines = append(lines, chat.RawLine(text))
		} else {
			text := "Already have an account? Choose Continue and add it in Settings."
			if key, ok := dashboard.FirstKeyLabel(CommandIdOpenConfig); ok {
				text = fmt.Sprintf("Already have an account? Choose Continue and add it in Settings (%s).", key)
			}
			lines = append(lines, chat.StyledLine(text, chat.Muted()))
		}
	}
	paragraph := chat.NewParagraph(chat.TextFrom(lines...)).WithWrap(chat.Wrap{Trim: false})
	width := min(max(area.Dx()-4, 0), 78)
	height := min(paragraph.LineCount(max(width, 1))+4, math.MaxUint16)
	popup := centeredModal(frame, surfaces, 80, max(height, 7), area)
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 1, Vertical: 1})
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, " Welcome to Mjolnir ", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	body := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-2, 0))
	maxScroll := max(paragraph.LineCount(max(body.Dx(), 1))-body.Dy(), 0)
	dialog.maxScroll = maxScroll
	frame.RenderWidget(paragraph.WithScroll(min(dialog.scroll, maxScroll, math.MaxUint16), 0), body)
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), min(inner.Dy(), 1))
	var actions []chat.ButtonSpec[WelcomeAction]
	if dialog.agentsFound != nil && !*dialog.agentsFound {
		actions = append(actions, chat.ButtonSpec[WelcomeAction]{ID: WelcomeActionManagedSetup, Label: "Set up a managed agent", Enabled: true})
	}
	actions = append(actions, chat.ButtonSpec[WelcomeAction]{ID: WelcomeActionContinue, Label: "Continue", Enabled: true})
	form.RenderActions(frame, footer, actions)
	form.EndFrame(WelcomeActionContinue)
}
