// Port of mj-tui/src/render/capacity.rs.
package tui

import (
	"cmp"
	"fmt"
	"maps"
	"math"
	"slices"
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
	details := capacityDetailsInKeyOrder(dashboard)
	rows := make([]CapacityTableRow, 0, len(details))
	for _, detail := range details {
		// Every listed target is unavailable: say so once for the host and
		// draw nothing else (no capacity, no Disks button).
		if allListedTargetsUnavailable(detail.Reading.TargetIDs, dashboard) {
			rows = append(rows, CapacityTableRow{
				host:    detail.Reading.Host,
				targets: chat.StyledLine("unavailable", chat.Muted()),
			})
			continue
		}
		staleness, stale := detail.Reading.Staleness(nowEpochSeconds)
		cpu, memory := capacityCPUAndMemory(detail, stale)
		row := CapacityTableRow{
			host:    detail.Reading.Host,
			targets: capacityTargetLabels(detail.Reading.TargetIDs, dashboard),
			cpu:     cpu,
			memory:  memory,
			disks:   capacityDisksCell(dashboard, detail),
		}
		if stale {
			row.staleness = &staleness
		}
		rows = append(rows, row)
	}
	return rows
}

// capacityDetailsInKeyOrder is the dashboard's capacity details in key
// order, the order Rust's BTreeMap values() yields and capacityIndex counts
// in.
func capacityDetailsInKeyOrder(dashboard *DashboardState) []*client.RuntimeHostCapacity {
	keys := slices.Sorted(maps.Keys(dashboard.capacityDetails))
	details := make([]*client.RuntimeHostCapacity, len(keys))
	for i, key := range keys {
		detail := dashboard.capacityDetails[key]
		details[i] = &detail
	}
	return details
}

// allListedTargetsUnavailable reports whether the row lists targets and
// every one of them is unavailable.
func allListedTargetsUnavailable(targetIDs []string, dashboard *DashboardState) bool {
	listed := listedCapacityTargets(targetIDs, dashboard)
	if len(listed) == 0 {
		return false
	}
	for _, id := range listed {
		if _, missing := dashboard.targetMissingLine(id); !missing {
			return false
		}
	}
	return true
}

// capacityCPUAndMemory is the CPU and RAM cells of one row. A host reports
// how much of each is in use; a fleet has no percentage of its own and
// reports what it is running.
func capacityCPUAndMemory(detail *client.RuntimeHostCapacity, stale bool) (chat.Line, chat.Line) {
	if detail.Refreshing {
		return chat.StyledLine("refreshing"+chat.ThemeGlyphs().Ellipsis, chat.Muted()), chat.Line{}
	}
	unavailable := func() chat.Line { return chat.StyledLine("unavailable", chat.Muted()) }
	reading := &detail.Reading
	switch {
	case reading.Kind == core.DeploymentCapacityKindHost && reading.HasReading:
		percentCell := func(usedPercent uint8, ok bool) chat.Line {
			if !ok {
				return unavailable()
			}
			color := chat.ThemePalette().Muted
			if !stale {
				color = headroomColor(100 - min(usedPercent, 100))
			}
			return chat.StyledLine(fmt.Sprintf("%d%%", usedPercent), chat.Style{}.Foreground(color))
		}
		var cpu chat.Line
		if reading.CPUPercent != nil {
			cpu = percentCell(*reading.CPUPercent, true)
		} else {
			cpu = percentCell(0, false)
		}
		return cpu, percentCell(reading.MemoryPercent())
	case reading.Kind == core.DeploymentCapacityKindAwsFleet && reading.FleetInstances != nil:
		cpu, memory := reading.CPUAndMemory()
		return chat.RawLine(cpu), chat.RawLine(memory)
	default:
		return unavailable(), chat.Line{}
	}
}

// capacityDisksCell is the Disks cell of one row. Free space comes from the
// daemon's storage owner, the same verdict that refuses writes and holds
// recovery back.
//
// The cell is one line: `Disks ▾` while every filesystem is fine, or the
// filesystem in the worst condition, as `/srv low ▾`. Details are in the
// anchored dropdown.
func capacityDisksCell(dashboard *DashboardState, detail *client.RuntimeHostCapacity) chat.Line {
	reading := &detail.Reading
	if len(reading.Filesystems) == 0 {
		return chat.Line{}
	}
	mark := chat.ThemeGlyphs().Dropdown
	if dashboard.capacityDisksMenuOpen(reading.ID) {
		mark = chat.ThemeGlyphs().Dropup
	}
	// The worst condition, then the least space left; Rust's max_by_key
	// keeps the last of equal elements.
	var worst *core.HostFilesystemRow
	for i := range reading.Filesystems {
		filesystem := &reading.Filesystems[i]
		if filesystem.Condition == core.StorageConditionOk {
			continue
		}
		if worst == nil {
			worst = filesystem
			continue
		}
		order := filesystem.Condition.Compare(worst.Condition)
		if order == 0 {
			order = cmp.Compare(worst.AvailableBytes, filesystem.AvailableBytes)
		}
		if order >= 0 {
			worst = filesystem
		}
	}
	if worst == nil {
		return chat.RawLine("Disks " + mark)
	}
	return chat.StyledLine(
		fmt.Sprintf("%s%s %s", reading.FilesystemPlace(worst), core.StorageConditionFlag(worst.Condition), mark),
		storageConditionStyle(worst.Condition))
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
	reading := &detail.Reading
	lines := make([]chat.Line, len(reading.Filesystems))
	for i := range reading.Filesystems {
		filesystem := &reading.Filesystems[i]
		lines[i] = chat.StyledLine(
			fmt.Sprintf("%s %s free of %s%s",
				reading.FilesystemPlace(filesystem),
				formatResourceBytes(filesystem.AvailableBytes),
				formatResourceBytes(filesystem.TotalBytes),
				core.StorageConditionFlag(filesystem.Condition)),
			storageConditionStyle(filesystem.Condition))
	}
	return lines
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
	nowEpochSeconds := uint64(max(time.Now().Unix(), 0))
	rows := capacityTableRows(dashboard, nowEpochSeconds)
	columnWidths := capacityColumnWidths(rows)
	focused := dashboard.focus == FocusTargets
	// On the combined surface (size is set) the title opens the pane's
	// menu, and says so with a dropdown mark.
	label := paneTitleLabel("Targets", size != nil)
	pressed := false
	if size != nil {
		titleBudget := paneTitleContentWidth(area.Dx(), dashboard.paneMaximizeEnabled(SupportPaneTargets))
		pressed = registerPaneTitleMenu(dashboard, SupportPaneTargets, uv.Rect(
			area.Min.X+1,
			area.Min.Y,
			min(chat.RawLine(label).Width(), math.MaxUint16, titleBudget),
			min(area.Dy(), 1)))
	}
	title := chat.RawSpan(label)
	if pressed {
		title = chat.StyledSpan(label, chat.Selection(true))
	}
	block := chat.Panel(focused).WithTitle(chat.LineFrom(title))
	if size != nil {
		block = block.WithTitle(paneSizeControls(*size, dashboard.paneMaximizeEnabled(SupportPaneTargets)))
	}
	// A row never asks for more lines than the pane has room for. The table
	// scrolls a selected row into view by dropping whole rows from the top
	// of the page, and a row taller than the content area makes it drop the
	// selected row itself, so a notice that does not fit waits for room
	// instead of costing the readings their line.
	viewport := max(area.Dy()-sessionTableChromeHeight, 0)
	rowHeights := make([]int, len(rows))
	disksButtons := make([]int, len(rows))
	notices := make([]string, len(rows))
	tableRows := make([]TableRow, len(rows))
	for i := range rows {
		row := &rows[i]
		rowHeights[i] = min(row.height(), max(viewport, 1))
		disksButtons[i] = row.disksButtonWidth()
		if rowHeights[i] > 1 && row.staleness != nil {
			notices[i] = *row.staleness
		}
		tableRows[i] = NewTableRow(
			TableCellFromString(row.host).WithStyle(chat.Style{}.Bold()),
			TableCellFromLine(row.targets).WithStyle(chat.Muted()),
			TableCellFromLine(row.cpu),
			TableCellFromLine(row.memory),
			TableCellFromLine(row.disks),
		).WithHeight(min(rowHeights[i], math.MaxUint16))
	}
	constraints := make([]chat.Constraint, len(columnWidths))
	for i, width := range columnWidths {
		constraints[i] = chat.ConstraintLength(width)
	}
	highlightStyle := chat.Style{}
	highlightSymbol := "  "
	if focused {
		highlightStyle = chat.Selection(true)
		highlightSymbol = chat.ThemeGlyphs().Selected
	}
	table := NewTable(tableRows, constraints).
		WithColumnSpacing(capacityColumnSpacing).
		WithHeader(NewTableRowFromStrings(capacityHeaders[:]...).WithStyle(chat.Muted().Patch(chat.Raised()))).
		WithRowHighlightStyle(highlightStyle).
		WithHighlightSymbol(highlightSymbol).
		WithHighlightSpacing(TableHighlightSpacingAlways).
		WithBlock(block)
	offset := chat.ClampOffsetToLastPage(dashboard.targetsScroll, rowHeights, viewport)
	if takeSelectionRecenter(dashboard, FocusTargets) {
		offset = chat.CenteredOffset(dashboard.capacityIndex, rowHeights, viewport)
	}
	var selected *int
	if len(dashboard.capacityDetails) > 0 {
		index := dashboard.capacityIndex
		selected = &index
	}
	state := TableState{}.WithOffset(offset).WithSelected(selected)
	table.RenderStateful(area, frame.Buffer(), &state)
	// The notice lines are drawn after the table, which laid out the rows
	// they belong to.
	columns := capacityColumns(area, columnWidths)
	registerDisksButtons(dashboard, &columns, disksButtons, rowHeights, state.Offset())
	renderCapacityNotices(frame, &columns, notices, rowHeights, state.Offset())
	if len(dashboard.capacityDetails) == 0 {
		message := "Waiting for target readings."
		if len(dashboard.config.Get().Targets) == 0 {
			message = "Add a target in Settings."
		}
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(message)).WithStyle(chat.Muted()).WithWrap(chat.Wrap{Trim: true}),
			uv.Rect(area.Min.X+3, area.Min.Y+2, max(area.Dx()-4, 0), max(area.Dy()-3, 0)))
	}
	dashboard.targetsScroll = state.Offset()
	total, above := 0, 0
	for i, height := range rowHeights {
		total += height
		if i < state.Offset() {
			above += height
		}
	}
	renderSessionScrollbar(frame, area, total, above, max(area.Dy()-sessionTableChromeHeight, 0))
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
	targets := columns.targets()
	width := max(columns.inner.Max.X-targets.Min.X, 0)
	if width == 0 {
		return
	}
	// The first row sits under the border and the heading row.
	y := columns.inner.Min.Y + 1
	for index := offset; index < min(len(notices), len(rowHeights)); index++ {
		if y >= columns.inner.Max.Y {
			break
		}
		noticeY := y + 1
		if notice := notices[index]; notice != "" && noticeY < columns.inner.Max.Y {
			text := truncateToCells(notice, width, TruncateSummary)
			frame.RenderWidget(
				chat.NewParagraph(chat.TextFrom(chat.StyledLine(text, chat.Muted()))),
				uv.Rect(targets.Min.X, noticeY, width, 1))
		}
		y += min(rowHeights[index], math.MaxUint16)
	}
}

// registerDisksButtons registers each visible row's Disks summary as the
// button that opens its filesystem dropdown.
func registerDisksButtons(dashboard *DashboardState, columns *CapacityColumns, buttons []int, rowHeights []int, offset int) {
	disks := columns.disks()
	y := columns.inner.Min.Y + 1
	areas := make([]*uv.Rectangle, len(buttons))
	for index := offset; index < min(len(buttons), len(rowHeights)); index++ {
		if y >= columns.inner.Max.Y {
			break
		}
		if width := min(buttons[index], disks.Dx()); width > 0 {
			buttonArea := uv.Rect(disks.Min.X, y, width, 1)
			dashboard.surfaceForm.Register(SurfaceControlCapacityDisks(index), chat.ControlKindButton{}, buttonArea, true)
			areas[index] = &buttonArea
		}
		y += min(rowHeights[index], math.MaxUint16)
	}
	dashboard.capacityDisksAreas = areas
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
