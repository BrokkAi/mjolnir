// Port of mj-tui/src/render/capacity.rs.
package tui

import (
	"math"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CapacityTableRow is one host or fleet in the Targets table.
type CapacityTableRow struct {
	host    string
	targets chat.Line
	cpu     chat.Line
	memory  chat.Line
	// disks is the one-line Disks button, which opens filesystem details in
	// a popup.
	disks chat.Line
	// staleness says why the row's reading cannot be trusted, when it
	// cannot. It is drawn on a second line of the row rather than in a
	// column of its own, so a notice never changes how wide the table has to
	// be.
	staleness *string
}

// disksButtonWidth is the width of the Disks button. Zero when the row has
// no storage reading.
func (r *CapacityTableRow) disksButtonWidth() int {
	return r.disks.Width()
}

// height is the lines this target wants: its readings, plus one for the
// notice when the reading cannot be trusted. The pane's content height
// bounds it, so a short pane keeps the readings and waits for room for the
// notice.
func (r *CapacityTableRow) height() int {
	if r.staleness != nil {
		return 2
	}
	return 1
}

// capacityTableRows is one row per host or fleet the dashboard has a
// capacity reading for.
func capacityTableRows(dashboard *DashboardState, nowEpochSeconds uint64) []CapacityTableRow {
	panic("unported: mj-tui/src/render/capacity.rs capacity_table_rows")
}

// allListedTargetsUnavailable reports whether the row lists targets and
// every one of them is unavailable.
func allListedTargetsUnavailable(targetIDs []string, dashboard *DashboardState) bool {
	panic("unported: mj-tui/src/render/capacity.rs all_listed_targets_unavailable")
}

// capacityCPUAndMemory is the CPU and RAM cells of one row. A host reports
// how much of each is in use; a fleet has no percentage of its own and
// reports what it is running.
func capacityCPUAndMemory(detail *client.RuntimeHostCapacity, stale bool) (chat.Line, chat.Line) {
	panic("unported: mj-tui/src/render/capacity.rs capacity_cpu_and_memory")
}

// capacityDisksCell is the Disks cell of one row. Free space comes from the
// daemon's storage owner, the same verdict that refuses writes and holds
// recovery back.
//
// The cell is one line: `Disks ▾` while every filesystem is fine, or the
// filesystem in the worst condition, as `/srv low ▾`. Details are in the
// anchored dropdown.
func capacityDisksCell(dashboard *DashboardState, detail *client.RuntimeHostCapacity) chat.Line {
	panic("unported: mj-tui/src/render/capacity.rs capacity_disks_cell")
}

// storageConditionStyle is the colour a filesystem's condition draws in.
func storageConditionStyle(condition core.StorageCondition) chat.Style {
	switch condition {
	case core.StorageConditionFull:
		return chat.Style{}.Foreground(chat.ThemePalette().Error)
	case core.StorageConditionLow:
		return chat.Style{}.Foreground(chat.ThemePalette().Warning)
	default:
		return chat.Style{}
	}
}

// capacityDisksMenuLines is the filesystem details for the dropdown, styled
// like the former expanded rows.
func capacityDisksMenuLines(detail *client.RuntimeHostCapacity) []chat.Line {
	panic("unported: mj-tui/src/render/capacity.rs capacity_disks_menu_lines")
}

// capacityTableLines is the lines the Targets table draws: one line per
// target, plus one line under each target whose reading cannot be trusted
// and says why.
func capacityTableLines(dashboard *DashboardState) int {
	lines := 0
	rows := capacityTableRows(dashboard, uint64(max(time.Now().Unix(), 0)))
	for i := range rows {
		lines += rows[i].height()
	}
	return lines
}

// capacityHeaders is the Targets table's headings. The Disks column has no
// heading: its cell already says `Disks ▾`, or names the filesystem it is
// about.
var capacityHeaders = [5]string{"Host / fleet", "Targets", "CPU", "RAM", ""}

// capacityColumnWidths is the column widths of the Targets table, one per
// heading. A notice line is deliberately absent from this list: it spans the
// columns under Targets instead, so a stale reading cannot make the pane ask
// for more width.
func capacityColumnWidths(rows []CapacityTableRow) []int {
	column := func(header string, cell func(row *CapacityTableRow) int) int {
		return quotaColumnWidth(header, func(yield func(int) bool) {
			for i := range rows {
				if !yield(cell(&rows[i])) {
					return
				}
			}
		}, math.MaxUint16)
	}
	return []int{
		column(capacityHeaders[0], func(row *CapacityTableRow) int { return chat.RawLine(row.host).Width() }),
		column(capacityHeaders[1], func(row *CapacityTableRow) int { return row.targets.Width() }),
		column(capacityHeaders[2], func(row *CapacityTableRow) int { return row.cpu.Width() }),
		column(capacityHeaders[3], func(row *CapacityTableRow) int { return row.memory.Width() }),
		column(capacityHeaders[4], func(row *CapacityTableRow) int { return row.disks.Width() }),
	}
}

// capacityColumnSpacing is how many cells separate the columns.
const capacityColumnSpacing = 2

// capacityTableWidth is the width needed to draw the complete Targets
// table, including its table spacing, border, and always-present selection
// marker. A notice line needs no width of its own: it shares the row's
// second line.
func capacityTableWidth(dashboard *DashboardState) int {
	widths := capacityColumnWidths(capacityTableRows(dashboard, uint64(max(time.Now().Unix(), 0))))
	total := 0
	for _, width := range widths {
		total = min(total+width, math.MaxUint16)
	}
	gaps := max(len(widths)-1, 0)
	// Two borders and two highlight cells.
	return min(total+gaps*capacityColumnSpacing+4, math.MaxUint16)
}

// renderCapacity draws the Targets pane. size is set on the combined
// surface, where the pane carries its size controls and its title opens the
// pane's menu.
func renderCapacity(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, size *core.PaneSize) {
	panic("unported: mj-tui/src/render/capacity.rs render_capacity")
}

// CapacityColumns is the inner area of the Targets table and the columns
// inside it, laid out the way the table lays them out, so hit regions and
// notice lines follow the rendered text even when columns shrink to fit the
// pane.
type CapacityColumns struct {
	inner   uv.Rectangle
	columns []uv.Rectangle
}

// targets is the Targets column, under which every notice line starts.
func (c *CapacityColumns) targets() uv.Rectangle {
	return c.columns[1]
}

func (c *CapacityColumns) disks() uv.Rectangle {
	return c.columns[len(capacityHeaders)-1]
}

// capacitySelectionWidth is the width of the selection marker a table row
// reserves, always present. It is the width of chat.ThemeGlyphs().Selected,
// the glyph the table highlights a row with.
const capacitySelectionWidth = 2

func capacityColumns(area uv.Rectangle, columnWidths []int) CapacityColumns {
	inner := chat.RectInner(area, chat.Margin{Horizontal: 1, Vertical: 1})
	areas := chat.HorizontalLayout(chat.ConstraintLength(capacitySelectionWidth), chat.ConstraintFill(0)).Split(inner)
	constraints := make([]chat.Constraint, len(columnWidths))
	for i, width := range columnWidths {
		constraints[i] = chat.ConstraintLength(width)
	}
	columns := chat.HorizontalLayout(constraints...).
		WithFlex(chat.FlexStart).
		WithSpacing(capacityColumnSpacing).
		Split(areas[1])
	return CapacityColumns{inner: inner, columns: columns}
}

// renderCapacityNotices draws each row's notice across the columns under
// Targets, on the second line the row reserves for it. The line runs to the
// pane's inner edge and is cut with an ellipsis, so a long notice costs width
// the table already has instead of widening the pane.
func renderCapacityNotices(frame *chat.Frame, columns *CapacityColumns, notices []string, rowHeights []int, offset int) {
	panic("unported: mj-tui/src/render/capacity.rs render_capacity_notices")
}

// registerDisksButtons registers each visible row's Disks summary as the
// button that opens its filesystem dropdown.
func registerDisksButtons(dashboard *DashboardState, columns *CapacityColumns, buttons []int, rowHeights []int, offset int) {
	panic("unported: mj-tui/src/render/capacity.rs register_disks_buttons")
}

// headroomColor is the colour the quota bar gives a percentage of headroom
// left.
//
// Both minimized panes read the same scale, which is why it lives in one
// place: a quota reports the headroom it has left directly, and a CPU
// reading is the inverse — a busy host has little left.
func headroomColor(headroomPercent uint8) chat.Color {
	switch {
	case headroomPercent <= 20:
		return chat.ThemePalette().Error
	case headroomPercent <= 50:
		return chat.ThemePalette().Warning
	default:
		return chat.ThemePalette().Success
	}
}
