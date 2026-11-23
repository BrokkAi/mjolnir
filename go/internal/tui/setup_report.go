// Port of mj-tui/src/setup/report.rs.

// The Detection results view: what a runtimes detection turned down, as one
// block per runtime — name, reason, and an indented fix — in a scrollable
// panel, so a status line never truncates a remediation again.

package tui

import (
	"math"
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
	report := dialog.report
	if report == nil {
		return false
	}
	var step int
	switch msg := event.(type) {
	case tea.KeyPressMsg:
		switch msg.Key().Code {
		case tea.KeyDown:
			step = 1
		case tea.KeyUp:
			step = -1
		case tea.KeyPgDown:
			step = 10
		case tea.KeyPgUp:
			step = -10
		case tea.KeyHome:
			report.scrollTo(0)
			return true
		case tea.KeyEnd:
			report.scrollTo(report.maxScroll)
			return true
		case tea.KeyEscape, tea.KeyBackspace:
			dialog.report = nil
			return true
		default:
			return false
		}
	case tea.MouseWheelMsg:
		switch msg.Mouse().Button {
		case tea.MouseWheelDown:
			step = 3
		case tea.MouseWheelUp:
			step = -3
		default:
			return false
		}
	default:
		return false
	}
	scroll := report.scroll + step
	report.scrollTo(min(max(scroll, 0), math.MaxUint16))
	return true
}

func renderReport(frame *chat.Frame, popup uv.Rectangle, inner uv.Rectangle, dialog *SetupDialog) {
	report := dialog.report
	if report == nil {
		return
	}
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Settings", chat.Title(true), !dialog.saving)
	frame.RenderWidget(
		chat.Modal().
			WithTitle(title).
			WithTitle(chat.StyledLine(" Detection results ", chat.Muted()).RightAligned()).
			WithTitleBottom(chat.RawLine(" ↑/↓ scroll · PgUp/PgDn · Home/End · Esc back ")),
		popup,
	)
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText("Why the last Detect runtimes left each one out. Esc returns to Runtimes.")).
			WithWrap(chat.Wrap{Trim: false}).
			WithStyle(chat.Muted()),
		uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), 2),
	)
	footerRow := chat.DialogShellLayout(inner, 0).Actions
	top := inner.Min.Y + 3
	body := uv.Rect(inner.Min.X, top, inner.Dx(), max(footerRow.Min.Y-top, 1))
	blocks := chat.NewParagraph(chat.TextFrom(setupReportLines(dialog.detectionRejected)...)).
		WithWrap(chat.Wrap{Trim: false})
	total := min(blocks.LineCount(max(body.Dx(), 1)), math.MaxUint16)
	maxScroll := max(total-body.Dy(), 0)
	report.maxScroll = maxScroll
	frame.RenderWidget(blocks.WithScroll(min(report.scroll, maxScroll), 0), body)
	form.RenderActions(frame, footerRow, dialog.footerActions())
	form.EndFrame(SetupControlBack)
}
