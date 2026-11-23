// Port of mj-tui/src/render.rs.
//
// Dashboard rendering: pane layout, session tables, capacity, quotas, footer.
// The submodules (render/capacity.rs, render/footer.rs, render/quota_pools.rs,
// render/quotas.rs, render/sessions.rs) are the render_*.go files beside this
// one; Rust re-exports their items into `crate::render`, which in package tui
// they already are. Rust's ratatui Table is render_table_widget.go.
//
// The test-only `render` (draws the combined surface with no conversation)
// is declared with the tests, as renderRender, because it calls the combined
// surface's test-only entry point.
package tui

import (
	"fmt"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// sessionTableChromeHeight is the rows a bordered table spends outside its
// body: two borders and the header row.
const sessionTableChromeHeight = 3

// sessionActionsHeight is one fixed row at the top of Sessions for Create
// and Resume.
const sessionActionsHeight = 1

// takeSelectionRecenter reports whether this pane requested that its
// selected row be visible again, and consumes the request. Sessions and
// Targets request this on keyboard movement; Quota also uses it when its
// selected identity moves during a refresh.
func takeSelectionRecenter(dashboard *DashboardState, focus Focus) bool {
	if dashboard.recenterOnSelection != nil && *dashboard.recenterOnSelection == focus {
		dashboard.recenterOnSelection = nil
		return true
	}
	return false
}

// renderModal draws the active modal over the dashboard already on the
// frame. Each modal clears its own centered rect, so the panes stay visible
// around it.
//
// The registry moves out for the call because the modal renderers read the
// rest of the dashboard while they register their own surfaces.
func renderModal(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	panic("unported: mj-tui/src/render.rs render_modal")
}

// minimumTerminalWidth is the width from which the Sessions sidebar sits
// beside the conversation. Below it, down to [narrowTerminalWidth], the
// sidebar stacks above.
const minimumTerminalWidth = 80

// narrowTerminalWidth is the narrowest frame the dashboard draws at all.
const narrowTerminalWidth = 60

// TerminalSizeRequirement is which dimension of the terminal is too small,
// and the size it needs.
type TerminalSizeRequirement interface {
	isTerminalSizeRequirement()
}

// TerminalSizeRequirementWidth needs at least Value columns.
type TerminalSizeRequirementWidth struct{ Value int }

// TerminalSizeRequirementHeight needs at least Value rows.
type TerminalSizeRequirementHeight struct{ Value int }

func (TerminalSizeRequirementWidth) isTerminalSizeRequirement()  {}
func (TerminalSizeRequirementHeight) isTerminalSizeRequirement() {}

// renderTerminalTooSmall replaces the frame with a centered notice saying
// how much larger the terminal must be.
func renderTerminalTooSmall(frame *chat.Frame, area uv.Rectangle, requirement TerminalSizeRequirement) {
	var instructions []chat.Line
	switch requirement := requirement.(type) {
	case TerminalSizeRequirementWidth:
		instructions = []chat.Line{
			chat.RawLine(fmt.Sprintf("Need at least %d columns.", requirement.Value)),
			chat.RawLine(fmt.Sprintf("Current width: %d.", area.Dx())),
		}
	case TerminalSizeRequirementHeight:
		instructions = []chat.Line{chat.RawLine(fmt.Sprintf(
			"Increase height to at least %d rows (currently %d).", requirement.Value, area.Dy()))}
	}
	frame.RenderWidget(chat.Clear{}, area)
	lines := append([]chat.Line{chat.StyledLine("Terminal too small", chat.Style{}.Bold())}, instructions...)
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(lines...)).WithAlignment(chat.AlignmentCenter), area)
}
