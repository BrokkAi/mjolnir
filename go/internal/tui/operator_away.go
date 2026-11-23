// Port of mj-tui/src/operator_away.rs.
//
// The daemon owns presence; a pending return suppresses stale away snapshots.

package tui

import (
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
	panic("unported: mj-tui/src/operator_away.rs DashboardState::set_operator_presence")
}

// OperatorPresenceRevision associates background completions with the
// presence they mutated.
func (d *DashboardState) OperatorPresenceRevision() uint64 {
	return d.operatorPresence.revision()
}

// OperatorAwayRequestFinished applies the result of an away request; err is
// Rust's Err(String), nil for Ok.
func (d *DashboardState) OperatorAwayRequestFinished(away bool, revision uint64, err error) {
	panic("unported: mj-tui/src/operator_away.rs DashboardState::operator_away_request_finished")
}

func (d *DashboardState) awayDialog() *AwayDialog {
	if away, ok := d.operatorPresence.(OperatorPresenceAway); ok {
		return away.Dialog
	}
	return nil
}

func (d *DashboardState) handleAwayEvent(event tea.Msg) DashboardAction {
	panic("unported: mj-tui/src/operator_away.rs DashboardState::handle_away_event")
}

// operatorAwayRender is Rust's operator_away::render (prefixed:
// notifications_pane also declares render).
func operatorAwayRender(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *AwayDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/operator_away.rs render")
}
