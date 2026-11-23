// Port of mj-tui/src/tile_layout.rs (test-only items).

package tui

import (
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// splitFocused splits the focused pane with an even ratio: SplitPane
// followed by moving focus to the new pane.
func (l *TileLayout) splitFocused(direction chat.Direction, area uv.Rectangle) PaneId {
	return l.splitFocusedWithRatio(direction, 0.5, area)
}

// splitFocusedWithRatio splits the focused pane with a chosen first-child
// ratio, then focuses the new pane.
func (l *TileLayout) splitFocusedWithRatio(direction chat.Direction, ratio float32, area uv.Rectangle) PaneId {
	newID, ok := l.SplitPane(l.focus, direction, ratio, area)
	if !ok {
		panic("test area accepts the split")
	}
	l.setFocus(newID)
	return newID
}
