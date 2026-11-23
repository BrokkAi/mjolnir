// Port of mj-tui/src/dashboard_input.rs (test-only items).

package tui

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// focusedTranscriptArea is where the focused pane's transcript sat on the
// last frame.
func (d *DashboardState) focusedTranscriptArea() (uv.Rectangle, bool) {
	transcript, _, ok := d.paneBands(d.FocusedPane())
	return transcript, ok
}

// focusedPromptArea is where the focused pane's composer sat on the last
// frame.
func (d *DashboardState) focusedPromptArea() (uv.Rectangle, bool) {
	_, prompt, ok := d.paneBands(d.FocusedPane())
	return prompt, ok
}

// paneBands are the transcript and composer rectangles one pane drew into.
func (d *DashboardState) paneBands(pane PaneId) (uv.Rectangle, uv.Rectangle, bool) {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::pane_bands")
}
