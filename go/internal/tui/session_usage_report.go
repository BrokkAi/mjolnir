// Port of mj-tui/src/session_usage_report.rs.

// Profiles' cancellable, read-only API cost comparison.

package tui

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// UsageControl is a control of the usage dialog.
type UsageControl int

const (
	UsageControlProfile UsageControl = iota
	UsageControlSuspended
	// UsageControlHours is the hourly bar graph; its columns are the
	// selectable hours.
	UsageControlHours
	UsageControlRows
	UsageControlRefresh
	UsageControlClose
)

// usageRowKind says which of Rust's UsageRow variants a UsageRow is.
type usageRowKind int

const (
	usageRowRole usageRowKind = iota
	usageRowModelTotal
	usageRowSession
	usageRowModel
	usageRowCoverage
)

// UsageRow identifies one selectable row of the report, so the selection
// survives a reload: Role(role), ModelTotal(role, model), Session(id),
// Model(id, model), or Coverage(id). It is comparable with ==; an absent
// model is hasModel false.
type UsageRow struct {
	kind      usageRowKind
	role      core.UsageAgentRole
	sessionID string
	model     string
	hasModel  bool
}

// SessionUsageResult is the dialog's `Result<SessionUsageReport, String>`:
// Error is set for Err.
type SessionUsageResult struct {
	Report core.SessionUsageReport
	Error  *string
}

// SessionUsageDialog is the usage-by-session dialog.
type SessionUsageDialog struct {
	generation uint64
	query      core.SessionUsageQuery
	// report is nil while a read is in flight.
	report   *SessionUsageResult
	profiles []string
	// hours are the latest bars, kept while a new read is in flight so the
	// graph the user clicked stays on screen.
	hours []core.HourlyUsage
	// hiddenHours is how many of the oldest hours the last frame had no room
	// to draw.
	hiddenHours int
	selectedKey *UsageRow
	rowKeys     []UsageRow
	selectedRow int
	combo       chat.ComboBoxState[UsageControl]
	form        chat.Dialog[UsageControl]
}

func (d *DashboardState) beginSessionUsageReport(profile *string) {
	panic("unported: mj-tui/src/session_usage_report.rs DashboardState::begin_session_usage_report")
}

// SessionUsageRequest is the identity the terminal task owner uses to cancel
// obsolete reads, while the dialog is open.
func (d *DashboardState) SessionUsageRequest() (uint64, core.SessionUsageQuery, bool) {
	panic("unported: mj-tui/src/session_usage_report.rs DashboardState::session_usage_request")
}

// ApplySessionUsageResult applies a finished read (err set for Rust's Err)
// when it still belongs to the open dialog.
func (d *DashboardState) ApplySessionUsageResult(generation uint64, report core.SessionUsageReport, err error) {
	panic("unported: mj-tui/src/session_usage_report.rs DashboardState::apply_session_usage_result")
}

func (d *DashboardState) handleSessionUsageReportEvent(event tea.Msg, dialog SessionUsageDialog) DashboardAction {
	panic("unported: mj-tui/src/session_usage_report.rs DashboardState::handle_session_usage_report_event")
}

// dollars is a cost, or `?` when it cannot be known. `+` marks a known cost
// that running or unreported turns may add to.
func dollars(cost core.CostUsdTicks, partial bool) string {
	panic("unported: mj-tui/src/session_usage_report.rs dollars")
}

// sessionUsageReportRender is the module's render (named apart from
// render.go's).
func sessionUsageReportRender(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *SessionUsageDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/session_usage_report.rs render")
}

// axisTopDollars is the graph's top value: the next 1, 2, or 5 times a
// power of ten above the tallest bar, in dollars, so the axis labels read
// cleanly.
func axisTopDollars(maxTicks uint64) float64 {
	panic("unported: mj-tui/src/session_usage_report.rs axis_top_dollars")
}

func axisLabel(dollars float64) string {
	panic("unported: mj-tui/src/session_usage_report.rs axis_label")
}

// renderHourChart draws one bar per hour in the report, tallest at the axis
// top. Each is a column of the Hours control, so a click or arrow selects
// that hour.
func renderHourChart(frame *chat.Frame, area uv.Rectangle, dialog *SessionUsageDialog, barRows int, form *chat.Dialog[UsageControl]) {
	panic("unported: mj-tui/src/session_usage_report.rs render_hour_chart")
}
