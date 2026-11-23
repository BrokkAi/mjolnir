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
	panic("unported: mj-tui/src/stopped_subagents.rs DashboardState::is_stopped_subagent")
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
	panic("unported: mj-tui/src/stopped_subagents.rs DashboardState::render_stopped_subagent")
}

// stoppedSubagentKey scrolls a stopped sub-agent's read-only conversation;
// nil leaves every other key its ordinary meaning.
func (d *DashboardState) stoppedSubagentKey(key tea.KeyMsg) DashboardAction {
	panic("unported: mj-tui/src/stopped_subagents.rs DashboardState::stopped_subagent_key")
}
