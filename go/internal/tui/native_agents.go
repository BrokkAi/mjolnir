// Port of mj-tui/src/native_agents.rs.

package tui

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NativeAgentPane is a harness-native sub-agent's read-only conversation.
type NativeAgentPane struct {
	Agent          core.NativeAgent
	transcript     *chat.TranscriptSnapshot
	projection     core.MaterializedSession
	generation     uint64
	loadingHistory bool
	hasMore        bool
	scroll         int
	Stopping       bool
}

func (d *DashboardState) IsNativeAgent(id string) bool {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::is_native_agent")
}

func (d *DashboardState) subagentParentFor(id string) (string, bool) {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::subagent_parent_for")
}

func (d *DashboardState) WorkingSubagentCountFor(parent string) int {
	return d.verdict(parent).WorkingChildren
}

func (d *DashboardState) SubagentCountFor(parent string) int {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::subagent_count_for")
}

func (d *DashboardState) SetNativeAgents(views []core.NativeAgentView) {
	snapshot := core.NewSnapshotMap[string, core.NativeAgentView]()
	for _, view := range views {
		snapshot.Insert(view.Agent.ViewID(), view)
	}
	d.SetNativeAgentSnapshot(snapshot)
}

func (d *DashboardState) SetNativeAgentSnapshot(views core.SnapshotMap[string, core.NativeAgentView]) {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::set_native_agent_snapshot")
}

// setNativeRowExcerpt gives a native child's row the excerpt other rows get
// from their stored summary. A native child has no summary of its own, so
// without this its row read "No messages yet" whatever it had said (R10-2).
func (d *DashboardState) setNativeRowExcerpt(id string, transcript []*core.TranscriptItem) {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::set_native_row_excerpt")
}

// NativeAgentHistoryLoaded applies a page of older native-agent history; err
// is Rust's Err(String), nil for Ok(page).
func (d *DashboardState) NativeAgentHistoryLoaded(owner string, child string, page core.NativeAgentHistoryPage, err error) {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::native_agent_history_loaded")
}

// NativeAgentStopFinished applies the result of a native-agent stop; err is
// Rust's Err(String), nil for Ok.
func (d *DashboardState) NativeAgentStopFinished(owner string, child string, err error) {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::native_agent_stop_finished")
}

// nativeAgentKey handles a key for a native agent's row or pane; nil when the
// key is not the pane's.
func (d *DashboardState) nativeAgentKey(key tea.KeyMsg) DashboardAction {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::native_agent_key")
}

func (d *DashboardState) renderNativeAgent(frame *chat.Frame, paneID PaneId, id string, transcriptArea uv.Rectangle, promptArea uv.Rectangle) {
	panic("unported: mj-tui/src/native_agents.rs DashboardState::render_native_agent")
}

// mergeNativeItems merges older transcript items into the projection, by
// stable id, ordered by position then stable id.
func mergeNativeItems(projection *core.MaterializedSession, older []*core.TranscriptItem) {
	panic("unported: mj-tui/src/native_agents.rs merge_native_items")
}
