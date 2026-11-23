// Port of mj-tui/src/conversation_navigation.rs.
//
// One owner for the active conversation and the session in each pane.

package tui

import (
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PaneAssignment is the identity of a pane assignment, independent of
// background attachment attempts. Reassigning A to B to A produces a
// different identity even before I/O runs.
type PaneAssignment uint64

// ConversationNavigation owns the tiled conversation layout and the session
// each pane shows.
//
// The layout and sessions fields carry the suffix Field because Go cannot
// give a field and a method the same name.
type ConversationNavigation struct {
	layoutField    TileLayout
	sessionsField  map[PaneId]string
	assignments    map[PaneId]PaneAssignment
	nextAssignment uint64
}

// newConversationNavigation is Rust's ConversationNavigation::default: one
// empty pane.
func newConversationNavigation() ConversationNavigation {
	layout, _ := NewTileLayout()
	return ConversationNavigation{
		layoutField:   layout,
		sessionsField: map[PaneId]string{},
		assignments:   map[PaneId]PaneAssignment{},
	}
}

func (n *ConversationNavigation) layout() *TileLayout {
	return &n.layoutField
}

func (n *ConversationNavigation) sessions() map[PaneId]string {
	return n.sessionsField
}

func (n *ConversationNavigation) selected() (string, bool) {
	session, ok := n.sessionsField[n.layoutField.Focused()]
	return session, ok
}

func (n *ConversationNavigation) assignment(pane PaneId) (PaneAssignment, bool) {
	assignment, ok := n.assignments[pane]
	return assignment, ok
}

// assign shows session in pane, or empties the pane for nil. It reports
// whether anything changed.
func (n *ConversationNavigation) assign(pane PaneId, session *string) bool {
	panic("unported: mj-tui/src/conversation_navigation.rs ConversationNavigation::assign")
}

func (n *ConversationNavigation) retainSessions(keep func(string) bool) bool {
	before := len(n.sessionsField)
	for pane, session := range n.sessionsField {
		if !keep(session) {
			delete(n.sessionsField, pane)
		}
	}
	for pane := range n.assignments {
		if _, shown := n.sessionsField[pane]; !shown {
			delete(n.assignments, pane)
		}
	}
	return len(n.sessionsField) != before
}

func (n *ConversationNavigation) focusPane(pane PaneId) {
	n.layoutField.FocusPane(pane)
}

func (n *ConversationNavigation) splitPane(pane PaneId, direction chat.Direction, ratio float32, area uv.Rectangle) (PaneId, bool) {
	return n.layoutField.SplitPane(pane, direction, ratio, area)
}

func (n *ConversationNavigation) closePane(pane PaneId) (string, bool) {
	if !n.layoutField.ClosePane(pane) {
		return "", false
	}
	delete(n.assignments, pane)
	session, ok := n.sessionsField[pane]
	delete(n.sessionsField, pane)
	return session, ok
}

func (n *ConversationNavigation) swapPanes(first, second PaneId) bool {
	return n.layoutField.SwapPanes(first, second)
}

func (n *ConversationNavigation) resizeFocused(nav NavDirection, delta float32, area uv.Rectangle) {
	n.layoutField.ResizeFocused(nav, delta, area)
}

func (n *ConversationNavigation) restore(saved *core.ConversationLayout, keep func(string) bool) {
	panic("unported: mj-tui/src/conversation_navigation.rs ConversationNavigation::restore")
}

func (n *ConversationNavigation) reset() {
	layout, _ := NewTileLayout()
	n.layoutField = layout
	clear(n.sessionsField)
	clear(n.assignments)
}
