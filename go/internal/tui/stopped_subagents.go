// Port of mj-tui/src/stopped_subagents.rs.
//
// Read-only view of a Mjolnir sub-agent that has stopped.
//
// A closed child has no worker to attach to, so its conversation cannot open
// the ordinary way: an attach waits out its timeout and leaves the pane
// empty. Its transcript is durable, so the host loads the stored tail in the
// background and this draws it, the way a native agent's pane is drawn.

package tui

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// StoppedSubagentPane is a stopped sub-agent's stored conversation.
type StoppedSubagentPane struct {
	// transcript is nil until the stored tail arrives.
	transcript *chat.TranscriptSnapshot
	empty      bool
	// errorText is why the stored tail could not be read (Rust's `error`),
	// nil when it could.
	errorText *string
	scroll    int
}

// IsStoppedSubagent reports whether id is a Mjolnir sub-agent whose session
// has stopped, so its conversation can only be read.
func (d *DashboardState) IsStoppedSubagent(id string) bool {
	return d.state.Get().IsStoppedSubagent(id) && d.TransitionKind(id) == nil
}

// BeginStoppedSubagent starts showing a stopped sub-agent. It returns
// whether the host has to load its stored transcript: not when it is loaded
// or loading already, and again after a load that failed.
func (d *DashboardState) BeginStoppedSubagent(id string) bool {
	pane, ok := d.stoppedSubagents[id]
	switch {
	case ok && pane.errorText == nil:
		return false
	case ok:
		pane.errorText = nil
		return true
	default:
		d.stoppedSubagents[id] = &StoppedSubagentPane{}
		return true
	}
}

// SetStoppedSubagentTranscript records the stored tail of a stopped
// sub-agent's conversation (session, nil when none is stored), or why it
// could not be read (err, Rust's Err(String)).
func (d *DashboardState) SetStoppedSubagentTranscript(id string, session *core.MaterializedSession, err error) {
	pane, ok := d.stoppedSubagents[id]
	if !ok {
		return
	}
	switch {
	case err != nil:
		message := err.Error()
		pane.errorText = &message
	case session != nil:
		pane.empty = len(session.Transcript) == 0
		pane.transcript = chat.TranscriptSnapshotFromMaterialized(session)
	default:
		pane.empty = true
		pane.transcript = chat.TranscriptSnapshotFromEntries(nil)
	}
}

func (d *DashboardState) renderStoppedSubagent(frame *chat.Frame, paneID PaneId, id string, transcriptArea uv.Rectangle, promptArea uv.Rectangle) {
	name := id
	if session := d.state.Get().Sessions.GetShared(id); session != nil {
		name = session.ListedTitle()
	}
	title := childPaneTitle(d, paneID, transcriptArea.Dx(), name, []string{"stopped"})
	tools := chat.ToolDisplayFromConfig(&d.config.Get().Advanced)
	pane, ok := d.stoppedSubagents[id]
	if !ok {
		return
	}
	block := chat.NewBlock().WithBorders(chat.BordersAll).WithTitle(title)
	inner := block.Inner(transcriptArea)
	frame.RenderWidget(block, transcriptArea)
	switch {
	case pane.errorText != nil:
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(
				"Could not read this sub-agent's transcript: "+*pane.errorText+"\nOpen it again to retry.",
			)).WithWrap(chat.Wrap{Trim: false}),
			inner,
		)
	case pane.transcript == nil:
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText("Loading this sub-agent's transcript"+chat.ThemeGlyphs().Ellipsis)),
			inner,
		)
	case pane.empty:
		frame.RenderWidget(chat.NewParagraph(chat.RawText("This sub-agent left no transcript.")), inner)
	default:
		lines, scroll := pane.transcript.RichTailScrolled(inner.Dx(), inner.Dy(), pane.scroll, tools)
		pane.scroll = scroll
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(lines...)), inner)
	}
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText(
			"This sub-agent has stopped; its conversation is read-only.\n"+
				"PgUp/PgDn: scroll · End: latest",
		)).WithBlock(chat.NewBlock().WithBorders(chat.BordersAll).WithTitle(chat.RawLine("Stopped sub-agent"))),
		promptArea,
	)
}

// stoppedSubagentKey scrolls a stopped sub-agent's read-only conversation;
// nil leaves every other key its ordinary meaning.
func (d *DashboardState) stoppedSubagentKey(key tea.KeyMsg) DashboardAction {
	var id string
	var ok bool
	switch {
	case d.focus == FocusSessions && *d.sessionsFilter.Get() == nil:
		id, ok = d.SelectedSessionID()
	case d.focus == FocusPrompt:
		id, ok = d.CurrentSessionID()
	}
	if !ok {
		return nil
	}
	pane, ok := d.stoppedSubagents[id]
	if !ok {
		return nil
	}
	switch key.Key().Code {
	case tea.KeyPgUp:
		pane.scroll += 20
	case tea.KeyPgDown:
		pane.scroll = max(pane.scroll-20, 0)
	case tea.KeyEnd:
		pane.scroll = 0
	default:
		return nil
	}
	return DashboardActionNone{}
}
