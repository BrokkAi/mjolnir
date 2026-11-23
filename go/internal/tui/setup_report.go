// Port of mj-tui/src/setup/report.rs.

// The Detection results view: what a runtimes detection turned down, as one
// block per runtime — name, reason, and an indented fix — in a scrollable
// panel, so a status line never truncates a remediation again.

package tui

import (
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// ReportState is the open Detection results view: how far the blocks are
// scrolled, and the last rendered maximum so End can reach the bottom.
type ReportState struct {
	scroll int
	// maxScroll is Rust's Cell<u16>, written by the renderer.
	maxScroll int
}

func (r *ReportState) scrollTo(target int) {
	r.scroll = min(target, r.maxScroll)
}

// setupReportLines is each refusal as its own block (Rust's
// setup::report::report_lines). The reason stands alone and the fix sits on
// an indented `Fix:` line of its own, never folded back into the reason.
func setupReportLines(rejected []RejectedRuntime) []chat.Line {
	var lines []chat.Line
	for index, runtime := range rejected {
		if index > 0 {
			lines = append(lines, chat.RawLine(""))
		}
		lines = append(lines, chat.StyledLine(runtime.Label, chat.Style{}.Add(chat.ModifierBold)))
		lines = append(lines, chat.RawLine(strings.TrimSpace(runtime.Detail)))
		if runtime.Remediation != nil {
			lines = append(lines, chat.LineFrom(
				chat.StyledSpan("  Fix: ", chat.Muted()),
				chat.RawSpan(strings.TrimSpace(*runtime.Remediation)),
			))
		}
	}
	return lines
}

// handleReportEvent applies a scroll key or wheel step to the open report,
// and reports whether the event was the report's: scrolling, or leaving it.
func handleReportEvent(dialog *SetupDialog, event tea.Msg) bool {
	panic("unported: mj-tui/src/setup/report.rs handle_report_event")
}

func renderReport(frame *chat.Frame, popup uv.Rectangle, inner uv.Rectangle, dialog *SetupDialog) {
	panic("unported: mj-tui/src/setup/report.rs render_report")
}
