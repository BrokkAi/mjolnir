// Port of mj-tui/src/operator_away.rs.
//
// The daemon owns presence; a pending return suppresses stale away snapshots.

package tui

import (
	"fmt"
	"math"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// operatorAwayExplanation is Rust's EXPLANATION (prefixed: dialogs/git_identity
// also declares EXPLANATION).
const operatorAwayExplanation = "While you are away, an agent that asks a question or needs permission is told that you are away and asked to do other useful work first. If it asks the same thing again, the request waits for you."

// AwayArtRow is one row of the AFK art: hammer, wordmark, bolt.
type AwayArtRow [3]string

var unicodeArt = [5]AwayArtRow{
	{" ▟█████▙ ", "▄████▄  ██████  ██   ██", "    ╱╱ "},
	{" ▜█████▛ ", "██  ██  ██      ██  ██ ", "   ╱╱  "},
	{"    ║    ", "██████  █████   █████  ", "  ╱━━  "},
	{"    ║    ", "██  ██  ██      ██  ██ ", "   ╱╱  "},
	{"    ▀    ", "██  ██  ██      ██   ██", "  ╱╱   "},
}

var asciiArt = [5]AwayArtRow{
	{` .=====. `, `/####\  ######  ##   ##`, "    // "},
	{` '==+==' `, "##  ##  ##      ##  ## ", "   //  "},
	{"    |    ", "######  #####   #####  ", "  /==  "},
	{"    |    ", "##  ##  ##      ##  ## ", "   //  "},
	{"   [_]   ", "##  ##  ##      ##   ##", "  //   "},
}

// AwayControl is the AFK dialog's one control.
type AwayControl uint8

const (
	AwayControlReturn AwayControl = iota
)

// OperatorPresence is this client's view of the operator's presence: here,
// away behind the AFK dialog, or returning while the daemon confirms. The
// variants carry the daemon presence revision they reflect.
type OperatorPresence interface {
	revision() uint64
}

type OperatorPresenceHere struct {
	Revision uint64
}

type OperatorPresenceAway struct {
	Revision uint64
	Dialog   *AwayDialog
}

type OperatorPresenceReturning struct {
	Revision uint64
}

// defaultOperatorPresence is Rust's Default: here, at revision 0.
func defaultOperatorPresence() OperatorPresence {
	return OperatorPresenceHere{}
}

func (p OperatorPresenceHere) revision() uint64      { return p.Revision }
func (p OperatorPresenceAway) revision() uint64      { return p.Revision }
func (p OperatorPresenceReturning) revision() uint64 { return p.Revision }

// AwayDialog is the AFK overlay's form and scroll state.
type AwayDialog struct {
	form      chat.Dialog[AwayControl]
	scroll    int
	maxScroll int
}

func newAwayDialog() *AwayDialog {
	form := chat.NewDialog[AwayControl]()
	form.SetDismissActions([]AwayControl{AwayControlReturn})
	form.SetDefaultAction(AwayControlReturn)
	return &AwayDialog{form: form}
}

// dialog and dialogMut implement DialogModal for AwayDialog.
func (d *AwayDialog) dialog() *chat.Dialog[AwayControl] {
	return &d.form
}

func (d *AwayDialog) dialogMut() *chat.Dialog[AwayControl] {
	return &d.form
}

// textInputFocused, prepare and layerDetail are DialogModal's default
// methods, which Go cannot inherit.
func (d *AwayDialog) textInputFocused() bool { return false }

func (d *AwayDialog) prepare() {}

func (d *AwayDialog) layerDetail() string { return "" }

func (d *DashboardState) SetOperatorPresence(presence core.OperatorPresence) {
	revision := d.operatorPresence.revision()
	_, returning := d.operatorPresence.(OperatorPresenceReturning)
	if presence.Revision < revision || (presence.Revision == revision && returning) {
		return
	}
	_, here := d.operatorPresence.(OperatorPresenceHere)
	if !presence.Away {
		d.operatorPresence = OperatorPresenceHere{Revision: presence.Revision}
	} else if presence.Revision > revision || here {
		d.CancelComponentPointer()
		d.paneMenu = nil
		d.operatorPresence = OperatorPresenceAway{Revision: presence.Revision, Dialog: newAwayDialog()}
	}
}

// OperatorPresenceRevision associates background completions with the
// presence they mutated.
func (d *DashboardState) OperatorPresenceRevision() uint64 {
	return d.operatorPresence.revision()
}

// OperatorAwayRequestFinished applies the result of an away request; err is
// Rust's Err(String), nil for Ok.
func (d *DashboardState) OperatorAwayRequestFinished(away bool, revision uint64, err error) {
	if err != nil {
		if current, returning := d.operatorPresence.(OperatorPresenceReturning); !away && returning && current.Revision == revision {
			d.operatorPresence = OperatorPresenceAway{Revision: revision, Dialog: newAwayDialog()}
		}
		verb := "leave"
		if away {
			verb = "enter"
		}
		d.SetFailureNotice(fmt.Sprintf("Could not %s AFK mode: %v", verb, err))
	}
	// Replies cannot replace the versioned feed: a later presence revision
	// may already describe another client's new AFK period.
}

func (d *DashboardState) awayDialog() *AwayDialog {
	if away, ok := d.operatorPresence.(OperatorPresenceAway); ok {
		return away.Dialog
	}
	return nil
}

func (d *DashboardState) handleAwayEvent(event tea.Msg) DashboardAction {
	away, ok := d.operatorPresence.(OperatorPresenceAway)
	if !ok {
		return DashboardActionNone{}
	}
	dialog := away.Dialog
	scroll, scrolled := 0, false
	switch event := event.(type) {
	case tea.KeyPressMsg:
		scrolled = true
		switch chat.KeyPressCode(event) {
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
	case tea.MouseWheelMsg:
		scrolled = true
		switch event.Button {
		case tea.MouseWheelDown:
			scroll = dialog.scroll + 3
		case tea.MouseWheelUp:
			scroll = max(dialog.scroll-3, 0)
		default:
			scrolled = false
		}
	}
	if scrolled {
		dialog.scroll = min(scroll, dialog.maxScroll)
		d.lastEventConsumed = true
		return DashboardActionNone{}
	}
	result := dialog.form.Handle(event)
	// The overlay owns all input; nothing may edit the preserved draft.
	d.lastEventConsumed = true
	switch action := result.Action.(type) {
	case chat.InteractionCancel[AwayControl]:
	case chat.InteractionActivate[AwayControl]:
		if action.ID != AwayControlReturn {
			return DashboardActionNone{}
		}
	default:
		return DashboardActionNone{}
	}
	d.operatorPresence = OperatorPresenceReturning{Revision: away.Revision}
	return DashboardActionSetOperatorAway{Away: false}
}

// operatorAwayRender is Rust's operator_away::render (prefixed:
// notifications_pane also declares render).
func operatorAwayRender(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *AwayDialog, surfaces *chat.FrameSurfaces) {
	var popup uv.Rectangle
	if area.Dx() < 16 || area.Dy() < 8 {
		// The normal two-cell modal margin would leave no readable body.
		frame.RenderWidget(chat.Clear{}, area)
		surfaces.Push(chat.SurfaceFrameFixed(chat.SurfaceIdModalBody{}, borderedContent(area)))
		popup = area
	} else {
		popup = centeredModal(frame, surfaces, 80, 21, area)
	}
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 2, Vertical: 1})
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, " AFK ", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	art := &unicodeArt
	if chat.SymbolsFor(dashboard.config.Get().Advanced.Symbols) == core.SymbolSetAscii {
		art = &asciiArt
	}
	palette := chat.ThemePalette()
	artLines := make([]chat.Line, 0, len(art))
	artWidth := 0
	for _, row := range art {
		line := chat.LineFrom(
			chat.StyledSpan(row[0], chat.Style{}.Foreground(palette.Secondary).Bold()),
			chat.RawSpan(" "),
			chat.StyledSpan(row[1], chat.Style{}.Foreground(palette.Warning).Bold()),
			chat.RawSpan(" "),
			chat.StyledSpan(row[2], chat.Style{}.Foreground(palette.Text).Bold()),
		)
		artWidth = max(artWidth, line.Width())
		artLines = append(artLines, line)
	}
	lines := []chat.Line{chat.RawLine("")}
	if inner.Dx() >= artWidth && inner.Dy() >= 15 {
		lines = append(lines, artLines...)
	} else {
		lines = append(lines, chat.StyledLine("AFK", chat.Title(true)))
	}
	lines = append(lines,
		chat.RawLine(""),
		chat.StyledLine(operatorAwayExplanation, chat.Muted()),
		chat.RawLine(""),
	)
	var waiting []*core.SessionRecord
	for _, entry := range dashboard.AttentionQueue() {
		if entry.Level != AttentionLevelWaiting {
			continue
		}
		if session := dashboard.state.Get().Sessions.GetShared(entry.SessionID); session != nil {
			waiting = append(waiting, session)
		}
	}
	if len(waiting) == 0 {
		lines = append(lines, chat.StyledLine("No sessions are waiting for you.", chat.Muted()))
	} else {
		// Left-align the list within itself, then let the centered paragraph
		// center the block: pad every row to the widest row's width.
		block := []chat.Line{chat.StyledLine("Waiting for you:", chat.Muted())}
		limit := min(max(inner.Dy()/4, 1), 5)
		for _, session := range waiting[:min(limit, len(waiting))] {
			block = append(block, chat.StyledLine(
				truncateToCells("  "+session.ListedTitle(), inner.Dx(), TruncatePlain),
				chat.Muted(),
			))
		}
		if len(waiting) > limit {
			block = append(block, chat.StyledLine(fmt.Sprintf("  and %d more", len(waiting)-limit), chat.Muted()))
		}
		// Padding past the body width would wrap a short row onto a blank line.
		blockWidth := 0
		for _, line := range block {
			blockWidth = max(blockWidth, line.Width())
		}
		blockWidth = min(blockWidth, inner.Dx())
		for _, line := range block {
			padding := max(blockWidth-line.Width(), 0)
			line.Spans = append(line.Spans, chat.RawSpan(strings.Repeat(" ", padding)))
			lines = append(lines, line)
		}
	}
	lines = append(lines,
		chat.RawLine(""),
		chat.StyledLine("Close this window to come back.", chat.Muted()),
	)
	paragraph := chat.NewParagraph(chat.TextFrom(lines...)).
		WithAlignment(chat.AlignmentCenter).
		WithWrap(chat.Wrap{Trim: false})
	maxScroll := max(paragraph.LineCount(max(inner.Dx(), 1))-inner.Dy(), 0)
	dialog.maxScroll = maxScroll
	frame.RenderWidget(paragraph.WithScroll(min(dialog.scroll, maxScroll, math.MaxUint16), 0), inner)
	form.EndFrame(AwayControlReturn)
}
