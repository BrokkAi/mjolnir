// Port of mj-tui/src/session_usage_report.rs.

// Profiles' cancellable, read-only API cost comparison.

package tui

import (
	"fmt"
	"maps"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"

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
	d.usageRequestGeneration++
	form := chat.NewDialog[UsageControl]()
	form.Focus(UsageControlProfile)
	d.mode = ModeSessionUsageReport{&SessionUsageDialog{
		generation: d.usageRequestGeneration,
		query:      core.SessionUsageQuery{Profile: profile},
		profiles:   slices.Sorted(maps.Keys(d.config.Get().Profiles)),
		form:       form,
	}}
}

// SessionUsageRequest is the identity the terminal task owner uses to cancel
// obsolete reads, while the dialog is open.
func (d *DashboardState) SessionUsageRequest() (uint64, core.SessionUsageQuery, bool) {
	dialog, ok := d.mode.(ModeSessionUsageReport)
	if !ok {
		return 0, core.SessionUsageQuery{}, false
	}
	return dialog.generation, usageCloneQuery(dialog.query), true
}

// usageCloneQuery is SessionUsageQuery's Clone: the copy shares no pointer
// with the dialog's query.
func usageCloneQuery(query core.SessionUsageQuery) core.SessionUsageQuery {
	if query.Profile != nil {
		profile := *query.Profile
		query.Profile = &profile
	}
	if query.HourStartMs != nil {
		hour := *query.HourStartMs
		query.HourStartMs = &hour
	}
	return query
}

// ApplySessionUsageResult applies a finished read (err set for Rust's Err)
// when it still belongs to the open dialog.
func (d *DashboardState) ApplySessionUsageResult(generation uint64, report core.SessionUsageReport, err error) {
	if current, _, ok := d.SessionUsageRequest(); !ok || current != generation {
		return
	}
	if err != nil {
		d.SetFailureNotice(fmt.Sprintf("Usage by session: %s", err))
	}
	dialog, ok := d.mode.(ModeSessionUsageReport)
	if !ok {
		return
	}
	result := &SessionUsageResult{Report: report}
	if err != nil {
		text := err.Error()
		result = &SessionUsageResult{Error: &text}
	} else {
		dialog.hours = slices.Clone(report.Hours)
		profiles := map[string]struct{}{}
		for _, profile := range dialog.profiles {
			profiles[profile] = struct{}{}
		}
		for _, profile := range report.Profiles {
			profiles[profile] = struct{}{}
		}
		dialog.profiles = slices.Sorted(maps.Keys(profiles))
	}
	dialog.report = result
}

func (d *DashboardState) handleSessionUsageReportEvent(event tea.Msg, dialog SessionUsageDialog) DashboardAction {
	// Arrow keys at either end of the graph repeat the selected hour; only a
	// click on the selected bar goes back to all hours.
	_, clicked := event.(tea.MouseMsg)
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	action := dialog.combo.Route(result.Action)
	reload := false
	profileIndex := func() int {
		if dialog.query.Profile != nil {
			if index := slices.Index(dialog.profiles, *dialog.query.Profile); index >= 0 {
				return index + 1
			}
		}
		return 0
	}
	switch action := action.(type) {
	case chat.InteractionCancel[UsageControl]:
		d.CancelModal()
		return DashboardActionNone{}
	case chat.InteractionActivate[UsageControl]:
		switch action.ID {
		case UsageControlClose:
			d.CancelModal()
			return DashboardActionNone{}
		case UsageControlProfile:
			dialog.combo.Open(UsageControlProfile, profileIndex())
		case UsageControlHours:
			reload = dialog.query.HourStartMs != nil
			dialog.query.HourStartMs = nil
		case UsageControlRefresh:
			reload = true
		}
	case chat.InteractionComboBoxCommit[UsageControl]:
		if action.ID == UsageControlProfile {
			dialog.query.Profile = nil
			if index := action.Index - 1; action.Index > 0 && index < len(dialog.profiles) {
				profile := dialog.profiles[index]
				dialog.query.Profile = &profile
			}
			reload = true
		}
	case chat.InteractionSelect[UsageControl]:
		switch action.ID {
		// Clicking the hour that is already shown goes back to all hours.
		case UsageControlHours:
			if position := dialog.hiddenHours + action.Index; position >= 0 && position < len(dialog.hours) {
				hour := dialog.hours[position].StartMs
				if clicked && dialog.query.HourStartMs != nil && *dialog.query.HourStartMs == hour {
					dialog.query.HourStartMs = nil
				} else {
					dialog.query.HourStartMs = &hour
				}
				reload = true
			}
		case UsageControlRows:
			dialog.selectedKey = nil
			if action.Index >= 0 && action.Index < len(dialog.rowKeys) {
				key := dialog.rowKeys[action.Index]
				dialog.selectedKey = &key
			}
			dialog.selectedRow = action.Index
		}
	case chat.InteractionToggle[UsageControl]:
		if action.ID == UsageControlSuspended {
			dialog.query.IncludeSuspended = !dialog.query.IncludeSuspended
			reload = true
		}
	}
	if reload {
		d.usageRequestGeneration++
		dialog.generation = d.usageRequestGeneration
		dialog.report = nil
	}
	d.mode = ModeSessionUsageReport{&dialog}
	return DashboardActionNone{}
}

// dollars is a cost, or `?` when it cannot be known. `+` marks a known cost
// that running or unreported turns may add to.
func dollars(cost core.CostUsdTicks, partial bool) string {
	if cost.Unknown {
		return "?"
	}
	ticks := cost.Ticks
	plus := ""
	if partial {
		plus = "+"
	}
	if ticks > 0 && ticks < 10_000_000 {
		return "<$0.001" + plus
	}
	return fmt.Sprintf("$%.3f%s", float64(ticks)/10_000_000_000.0, plus)
}

// sessionUsageReportRender is the module's render (named apart from
// render.go's).
func sessionUsageReportRender(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *SessionUsageDialog, surfaces *chat.FrameSurfaces) {
	popup := centeredModal(frame, surfaces, 92, 32, area)
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 1, Vertical: 1})
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Usage by session", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	if inner.Dy() < 9 {
		form.EndFrame(UsageControlClose)
		return
	}
	// A short terminal drops the blank lines between the controls.
	compact := inner.Dy() < 14
	ifCompact := func(compactValue, roomy int) int {
		if compact {
			return compactValue
		}
		return roomy
	}
	selectedProfile := 0
	if dialog.query.Profile != nil {
		if index := slices.Index(dialog.profiles, *dialog.query.Profile); index >= 0 {
			selectedProfile = index + 1
		}
	}
	form.Declare(UsageControlProfile, chat.ControlKindComboBox{
		Len:      len(dialog.profiles) + 1,
		Selected: dialog.combo.Selection(UsageControlProfile, selectedProfile),
		Expanded: dialog.combo.IsOpen(UsageControlProfile),
	})
	frame.RenderWidget(chat.NewParagraph(chat.RawText("Profile")), uv.Rect(inner.Min.X, inner.Min.Y, min(8, inner.Dx()), 1))
	chat.CheckboxRender(
		frame,
		uv.Rect(inner.Min.X, inner.Min.Y+ifCompact(1, 2), inner.Dx(), 1),
		"Include suspended sessions",
		dialog.query.IncludeSuspended,
		true,
		&form.Form,
		UsageControlSuspended,
	)
	window := "last 24 clock hours"
	if dialog.query.HourStartMs != nil {
		ms := *dialog.query.HourStartMs
		zone := func(ms int64) time.Time { return time.UnixMilli(ms).In(displayZone()) }
		window = fmt.Sprintf("%s–%s", zone(ms).Format("Mon Jan 2 15:04"), zone(ms+core.HourMs).Format("15:04"))
	}
	// A short terminal keeps the rows and drops the graph.
	barRows := 2
	switch {
	case inner.Dy() >= 24:
		barRows = 5
	case compact:
		barRows = 1
	}
	chartY := inner.Min.Y + ifCompact(3, 5)
	renderHourChart(frame, uv.Rect(inner.Min.X, chartY, inner.Dx(), barRows+1), dialog, barRows, form)
	rowsY := chartY + barRows + ifCompact(1, 2)
	rowsArea := uv.Rect(inner.Min.X, rowsY, inner.Dx(), max(inner.Max.Y-(rowsY+ifCompact(2, 3)), 0))
	switch {
	case dialog.report == nil:
		frame.RenderWidget(chat.NewParagraph(chat.RawText("Loading usage…")).WithStyle(chat.Muted()), rowsArea)
	case dialog.report.Error != nil:
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(fmt.Sprintf("Usage load failed: %s\nUse Refresh to retry.", *dialog.report.Error))).
				WithWrap(chat.Wrap{Trim: false}).
				WithStyle(chat.Muted()),
			rowsArea,
		)
	default:
		report := &dialog.report.Report
		partial := false
		for index := range report.Sessions {
			if report.Sessions[index].IsPartial() {
				partial = true
				break
			}
		}
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(fmt.Sprintf("Total %s · %s · estimated API USD", dollars(report.CostUsdTicks, partial), window))),
			uv.Rect(inner.Min.X, inner.Min.Y+ifCompact(2, 4), inner.Dx(), 1),
		)
		var lines []chat.Line
		var rowMap []*int
		dialog.rowKeys = dialog.rowKeys[:0]
		pushRow := func(line chat.Line, key *UsageRow) {
			lines = append(lines, line)
			if key == nil {
				rowMap = append(rowMap, nil)
				return
			}
			index := len(dialog.rowKeys)
			rowMap = append(rowMap, &index)
			dialog.rowKeys = append(dialog.rowKeys, *key)
		}
		modelName := func(model *string) string {
			if model == nil {
				return "Unknown model"
			}
			return *model
		}
		usageModel := func(model *string) (string, bool) {
			if model == nil {
				return "", false
			}
			return *model, true
		}
		if len(report.Sessions) > 0 {
			for _, agents := range report.AgentUsage {
				pushRow(
					chat.StyledLine(fmt.Sprintf("%s  %s", agents.Role.Label(), dollars(agents.CostUsdTicks, partial)), chat.Title(false)),
					&UsageRow{kind: usageRowRole, role: agents.Role},
				)
				for _, model := range agents.Models {
					cost := dollars(model.CostUsdTicks, partial)
					name := truncateToCells(modelName(model.Model), max(inner.Dx()-(len(cost)+4), 0), TruncatePlain)
					modelID, hasModel := usageModel(model.Model)
					pushRow(
						chat.RawLine(fmt.Sprintf("  %s  %s", name, cost)),
						&UsageRow{kind: usageRowModelTotal, role: agents.Role, model: modelID, hasModel: hasModel},
					)
				}
				pushRow(chat.Line{}, nil)
			}
		}
		for index := range report.Sessions {
			session := &report.Sessions[index]
			cost := dollars(session.CostUsdTicks, session.IsPartial())
			if session.Suspended {
				cost += " · suspended"
			}
			name := fmt.Sprintf("%s  [%s]", session.Title, session.Profile)
			name = truncateToCells(name, max(inner.Dx()-(len(cost)+2), 0), TruncateSummary)
			pushRow(
				chat.LineFrom(chat.StyledSpan(name, chat.Title(false)), chat.RawSpan("  "+cost)),
				&UsageRow{kind: usageRowSession, sessionID: session.SessionID},
			)
			for _, model := range session.Models {
				tokens := "tokens unknown"
				if total, ok := model.Totals["total_tokens"]; ok {
					tokens = fmt.Sprintf("%d tokens", total.Tokens)
				}
				detail := fmt.Sprintf("%s  %s", dollars(model.CostUsdTicks, false), tokens)
				name := truncateToCells(modelName(model.Model), max(inner.Dx()-(len(detail)+6), 0), TruncatePlain)
				modelID, hasModel := usageModel(model.Model)
				pushRow(
					chat.RawLine(fmt.Sprintf("    %s  %s", name, detail)),
					&UsageRow{kind: usageRowModel, sessionID: session.SessionID, model: modelID, hasModel: hasModel},
				)
			}
			coverage := &session.Coverage
			unfinished := ""
			if coverage.UnfinishedTurns > 0 {
				unfinished = fmt.Sprintf(" · %d unfinished", coverage.UnfinishedTurns)
			}
			pushRow(
				chat.StyledLine(fmt.Sprintf(
					"    %d sub-agents · %d/%d turns reported%s",
					session.SubagentCount,
					coverage.FullTurnReports,
					coverage.RecordedTurns,
					unfinished,
				), chat.Muted()),
				&UsageRow{kind: usageRowCoverage, sessionID: session.SessionID},
			)
			pushRow(chat.Line{}, nil)
		}
		selected := -1
		if dialog.selectedKey != nil {
			selected = slices.Index(dialog.rowKeys, *dialog.selectedKey)
		}
		if selected < 0 {
			selected = min(dialog.selectedRow, max(len(dialog.rowKeys)-1, 0))
		}
		dialog.selectedRow = selected
		dialog.selectedKey = nil
		if selected < len(dialog.rowKeys) {
			key := dialog.rowKeys[selected]
			dialog.selectedKey = &key
		}
		chat.ChoiceListRenderWithRows(frame, rowsArea, lines, selected, rowMap, nil, &form.Form, UsageControlRows)
		if len(lines) == 0 {
			frame.RenderWidget(
				chat.NewParagraph(chat.RawText("No sessions match these filters.")).WithStyle(chat.Muted()),
				rowsArea,
			)
		}
	}
	hints := []string{
		"Click a bar or ←/→ for an hour, Enter for all 24 · ░ unpriced models",
		"Sub-agents included · profile filters turns · + partial · base rates",
	}
	shownHints := hints[ifCompact(1, 0):]
	for row := range shownHints {
		hint := shownHints[len(shownHints)-1-row]
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(hint)).WithStyle(chat.Muted()),
			uv.Rect(inner.Min.X, inner.Max.Y-2-row, inner.Dx(), 1),
		)
	}
	form.RenderActions(
		frame,
		uv.Rect(inner.Min.X, inner.Max.Y-1, inner.Dx(), 1),
		[]chat.ButtonSpec[UsageControl]{
			{ID: UsageControlRefresh, Label: "Refresh", Enabled: true},
			{ID: UsageControlClose, Label: "Close", Enabled: true},
		},
	)
	// Dropdown popups draw last so they own the cells and pointer surface
	// beneath them.
	profileOptions := []chat.Line{chat.RawLine("All")}
	for _, profile := range dialog.profiles {
		profileOptions = append(profileOptions, chat.RawLine(profile))
	}
	value := "All"
	if dialog.query.Profile != nil {
		value = *dialog.query.Profile
	}
	chat.ComboBoxRender(
		frame,
		area,
		uv.Rect(inner.Min.X+8, inner.Min.Y, min(max(inner.Dx()-8, 0), 40), 1),
		value,
		profileOptions,
		dialog.combo.Selection(UsageControlProfile, selectedProfile),
		dialog.combo.IsOpen(UsageControlProfile),
		true,
		"Profile",
		chat.PopupSideBelow,
		chat.ComboBoxDefaultMaxVisibleRows,
		&form.Form,
		UsageControlProfile,
	)
	form.EndFrame(UsageControlClose)
}

// axisTopDollars is the graph's top value: the next 1, 2, or 5 times a
// power of ten above the tallest bar, in dollars, so the axis labels read
// cleanly.
func axisTopDollars(maxTicks uint64) float64 {
	dollars := float64(maxTicks) / 10_000_000_000.0
	if dollars <= 0.0 {
		return 1.0
	}
	magnitude := math.Pow(10, math.Floor(math.Log10(dollars)))
	for _, step := range []float64{1.0, 2.0, 5.0, 10.0} {
		if top := step * magnitude; top >= dollars*0.999_999 {
			return top
		}
	}
	return 10.0 * magnitude
}

func axisLabel(dollars float64) string {
	if dollars <= 0.0 {
		return "$0"
	}
	// Keep two significant digits below a cent so tiny costs do not read $0.
	places := min(max(int(max(math.Ceil(-math.Log10(dollars)), 0.0))+1, 3), 12)
	text := strconv.FormatFloat(dollars, 'f', places, 64)
	return "$" + strings.TrimRight(strings.TrimRight(text, "0"), ".")
}

// renderHourChart draws one bar per hour in the report, tallest at the axis
// top. Each is a column of the Hours control, so a click or arrow selects
// that hour.
func renderHourChart(frame *chat.Frame, area uv.Rectangle, dialog *SessionUsageDialog, barRows int, form *chat.Dialog[UsageControl]) {
	blocks := [8]rune{'▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'}
	maxTicks := uint64(0)
	for _, hour := range dialog.hours {
		maxTicks = max(maxTicks, hour.KnownCostUsdTicks)
	}
	top := axisTopDollars(maxTicks)
	labels := []string{axisLabel(top), axisLabel(top / 2.0), "$0"}
	gutter := 0
	for _, label := range labels {
		gutter = max(gutter, len(label))
	}
	gutter++
	available := max(area.Dx()-gutter, 0)
	// Draw the newest hours that fit. Bars have a one-cell gap when there is
	// room; at 24 columns they touch, and only below that are hours dropped.
	pitch := max(available/max(len(dialog.hours), 1), 1)
	shown := min(len(dialog.hours), available/pitch)
	hidden := len(dialog.hours) - shown
	dialog.hiddenHours = hidden
	hours := dialog.hours[hidden:]
	selected := -1
	if dialog.query.HourStartMs != nil {
		selected = slices.IndexFunc(hours, func(hour core.HourlyUsage) bool { return hour.StartMs == *dialog.query.HourStartMs })
	}
	cursor := selected
	if cursor < 0 {
		cursor = max(len(hours)-1, 0)
	}
	focused := form.IsFocused(UsageControlHours)
	eighths := func(hour core.HourlyUsage) int {
		if hour.KnownCostUsdTicks == 0 {
			// Usage with no known price still marks the hour.
			if hour.HasUnknownCost {
				return 1
			}
			return 0
		}
		full := float64(barRows) * 8.0
		scaled := float64(hour.KnownCostUsdTicks) / 10_000_000_000.0 / top * full
		rounded := math.Round(scaled)
		if math.IsNaN(rounded) {
			rounded = 0
		}
		// Rust's `as u16` saturates.
		return min(max(int(min(max(rounded, 0), math.MaxUint16)), 1), barRows*8)
	}
	barWidth := max(pitch-1, 1)
	var lines []chat.Line
	for row := range barRows {
		axis := -1
		switch {
		case row == 0:
			axis = 0
		case row == barRows-1:
			axis = 2
		case row == barRows/2:
			axis = 1
		}
		label := ""
		if axis >= 0 {
			label = labels[axis]
		}
		spans := []chat.Span{chat.StyledSpan(fmt.Sprintf("%*s ", gutter-1, label), chat.Muted())}
		levelBelow := (barRows - 1 - row) * 8
		for index, hour := range hours {
			level := min(max(eighths(hour)-levelBelow, 0), 8)
			glyph := ' '
			switch {
			case level == 0:
			case hour.HasUnknownCost:
				glyph = '░'
			default:
				glyph = blocks[level-1]
			}
			style := chat.Actionable()
			if selected == index {
				style = chat.ActiveControl()
			}
			spans = append(spans, chat.StyledSpan(strings.Repeat(string(glyph), barWidth), style))
			if pitch > 1 {
				spans = append(spans, chat.RawSpan(" "))
			}
		}
		lines = append(lines, chat.LineFrom(spans...))
	}
	// A two-digit hour needs two cells; with one-cell bars, label every
	// other bar.
	stride := 1
	if barWidth < 2 {
		stride = (3 + pitch - 1) / pitch
	}
	labelWidth := pitch * stride
	labelRow := []chat.Span{chat.RawSpan(strings.Repeat(" ", gutter))}
	for index := 0; index < len(hours); index += stride {
		local := time.UnixMilli(hours[index].StartMs).In(displayZone())
		covers := func(bar int) bool { return bar >= 0 && bar >= index && bar < index+stride }
		style := chat.Muted()
		if covers(selected) {
			style = chat.ActiveControl()
		}
		if focused && covers(cursor) {
			style = style.Underlined()
		}
		labelRow = append(labelRow, chat.StyledSpan(fmt.Sprintf("%-*s", labelWidth, local.Format("15")), style))
	}
	lines = append(lines, chat.LineFrom(labelRow...))
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(lines...)), area)
	columns := make([][2]int, shown)
	for index := range shown {
		start := area.Min.X + gutter + index*pitch
		columns[index] = [2]int{start, start + pitch}
	}
	form.RegisterColumns(
		UsageControlHours,
		uv.Rect(area.Min.X+gutter, area.Min.Y, shown*pitch, area.Dy()),
		cursor,
		columns,
		shown > 0,
	)
}
