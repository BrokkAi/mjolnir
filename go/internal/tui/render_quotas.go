// Port of mj-tui/src/render/quotas.rs.
package tui

import (
	"fmt"
	"iter"
	"math"
	"math/bits"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SummaryReading is one reading in a minimized pane: a name, its value, and
// how healthy the value is. A reading with no health to report draws in the
// ordinary foreground rather than claiming a colour it has not earned.
type SummaryReading struct {
	name  string
	value string
	// color is the reading's health colour; the zero Color is Rust's None.
	color chat.Color
}

// minimizedTargetsLine is one row summarising every target host and its CPU
// load, for the minimized Targets pane.
//
// A reading that cannot be trusted says so rather than showing a number: an
// unavailable probe, a sample that stopped refreshing, and a fleet reading
// that carries no CPU figure are all named explicitly.
func minimizedTargetsLine(dashboard *DashboardState, width int, focused bool) chat.Line {
	nowEpochSeconds := uint64(max(time.Now().Unix(), 0))
	var readings []SummaryReading
	for _, detail := range capacityDetailsInKeyOrder(dashboard) {
		// Same rule as the expanded table: a host whose listed targets are
		// all unavailable shows no reading.
		if allListedTargetsUnavailable(detail.Reading.TargetIDs, dashboard) {
			readings = append(readings, SummaryReading{name: detail.Reading.Host, value: "unavailable"})
			continue
		}
		if detail.Refreshing {
			readings = append(readings, SummaryReading{name: detail.Reading.Host, value: "refreshing" + chat.ThemeGlyphs().Ellipsis})
			continue
		}
		reading := &detail.Reading
		value := "unavailable"
		var cpuPercent *uint8
		switch {
		case reading.Kind == core.DeploymentCapacityKindHost && reading.HasReading:
			if reading.CPUPercent != nil {
				value = fmt.Sprintf("%d%%", *reading.CPUPercent)
				cpuPercent = reading.CPUPercent
			} else {
				value = "no CPU"
			}
		case reading.Kind == core.DeploymentCapacityKindAwsFleet && reading.FleetInstances != nil:
			value = reading.FleetMachinesLabel()
		}
		_, stale := reading.Staleness(nowEpochSeconds)
		summary := SummaryReading{name: detail.Reading.Host, value: value}
		if stale {
			summary.value = value + " (stale)"
		}
		// A busy host has little headroom, so the scale runs the other way
		// round from a quota's.
		if cpuPercent != nil && !stale {
			summary.color = headroomColor(100 - min(*cpuPercent, 100))
		}
		readings = append(readings, summary)
	}
	return summaryRow("Targets", readings, width, focused)
}

// minimizedQuotaLine is a weighted reading for each independently metered
// provider pool.
func minimizedQuotaLine(dashboard *DashboardState, width int, focused bool, now uint64) chat.Line {
	var readings, statuses []SummaryReading
	for _, provider := range providerPools(dashboard.config.Get(), dashboard.quotas, &dashboard.quotaProviders, now) {
		for p := range provider.Pools {
			pool := &provider.Pools[p]
			name := provider.Provider.DisplayName()
			if pool.Label != "" {
				name = provider.Provider.DisplayName() + " " + pool.Label
			}
			value := pool.percentage()
			if slices.ContainsFunc(pool.Members, func(member PoolMember) bool {
				_, refreshing := dashboard.quotaRefreshing[member.ID]
				return refreshing
			}) {
				value += " (refreshing)"
			}
			readings = append(readings, SummaryReading{name: name, value: value, color: providerAccent(provider.Provider)})
		}
		for _, text := range provider.Text {
			status := text.Text
			if _, refreshing := dashboard.quotaRefreshing[text.ID]; refreshing {
				status = "refreshing" + chat.ThemeGlyphs().Ellipsis
			} else if strings.HasPrefix(status, "quota check refused") {
				status = "check refused"
			}
			statuses = append(statuses, SummaryReading{name: text.ID, value: status})
		}
	}
	readings = append(readings, statuses...)
	return summaryRow("Profiles", readings, width, focused)
}

// summaryRow is a minimized pane's single row, drawn as the pane's own title
// so it keeps the rule the full pane has. Readings are comma-separated and
// truncated rather than wrapped, because the row is one row.
func summaryRow(label string, readings []SummaryReading, width int, focused bool) chat.Line {
	// A minimized pane is still a pane, so its one row opens the way a
	// bordered one does and the rule carries on between the label and the
	// readings: `─ Profiles ── claude-1 63% ────`.
	opening, divider := "─ ", " ── "
	spans := []chat.Span{chat.RawSpan(opening + label + divider)}
	used := utf8.RuneCountInString(opening) + utf8.RuneCountInString(label) + utf8.RuneCountInString(divider)
	// Leave room for the rule the title runs into, so the readings never
	// push it off the row and it never butts straight against a value.
	budget := max(width-4, 0)
	if len(readings) == 0 {
		spans = append(spans, chat.RawSpan("none configured "))
		return chat.LineFrom(spans...)
	}
	for index, reading := range readings {
		separator := ", "
		if index == 0 {
			separator = ""
		}
		text := separator + reading.name + " " + reading.value
		textWidth := utf8.RuneCountInString(text)
		if used+textWidth > budget {
			ellipsis := chat.ThemeGlyphs().Ellipsis
			if index == 0 {
				spans = append(spans, chat.RawSpan(ellipsis+" "))
			} else {
				spans = append(spans, chat.RawSpan(", "+ellipsis+" "))
			}
			return chat.LineFrom(spans...)
		}
		used += textWidth
		if separator != "" {
			spans = append(spans, chat.RawSpan(separator))
		}
		spans = append(spans, chat.RawSpan(reading.name+" "))
		if reading.color.IsSet() {
			spans = append(spans, chat.StyledSpan(reading.value, chat.Style{}.Foreground(reading.color)))
		} else {
			spans = append(spans, chat.RawSpan(reading.value))
		}
	}
	// The rule picks up where the title stops, so the title closes with a
	// space the way a bordered pane's does.
	spans = append(spans, chat.RawSpan(" "))
	return chat.LineFrom(spans...)
}

// quotaRemainingPercent is the share of a window left, from its reported
// percentage or else from its used and limit counts; false when neither is
// readable.
func quotaRemainingPercent(window *client.QuotaWindow) (uint8, bool) {
	if window.RemainingPercent != nil {
		return min(*window.RemainingPercent, 100), true
	}
	if window.Used == nil || window.Limit == nil {
		return 0, false
	}
	used, limit := *window.Used, *window.Limit
	if limit <= 0 {
		return 0, false
	}
	// limit.saturating_sub(used).clamp(0, limit), times 100 over limit in
	// 128 bits as Rust's i128 does.
	remaining := limit - used
	switch {
	case used < 0 && remaining < limit:
		// limit - used overflowed past i64::MAX; it saturates there.
		remaining = limit
	case remaining < 0:
		remaining = 0
	case remaining > limit:
		remaining = limit
	}
	hi, lo := bits.Mul64(uint64(remaining), 100)
	quotient, _ := bits.Div64(hi, lo, uint64(limit))
	return uint8(quotient), true
}

// weeklyQuotaExhausted reports whether the profile's weekly window has less
// than one percent left.
func weeklyQuotaExhausted(quota *client.ProfileQuota) bool {
	window := quota.WeeklyWindow()
	if window == nil {
		return false
	}
	remaining, ok := quotaRemainingPercent(window)
	return ok && remaining < 1
}

// quotaColumnWidth is the width of a column: its widest cell or its
// header, at most maximum.
func quotaColumnWidth(header string, contentWidths iter.Seq[int], maximum int) int {
	width := chat.RawLine(header).Width()
	for content := range contentWidths {
		width = max(width, content)
	}
	return min(width, math.MaxUint16, maximum)
}

// quotaPaneWidth is enough space for seven readable profile segments,
// without forcing short dashboards to reserve the width of expanded notes.
func quotaPaneWidth(dashboard *DashboardState) int {
	return 76
}

// quotaContentHeight is the lines the Quota pane wants at its own width.
func quotaContentHeight(dashboard *DashboardState) int {
	now := uint64(max(time.Now().Unix(), 0))
	lines := quotaLines(dashboard, max(quotaPaneWidth(dashboard)-2, 0), now, false, clockLabel)
	return max(len(lines.lines), 1)
}

// renderQuotas draws the Quota pane at the current time and in the local
// timezone.
func renderQuotas(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, size *core.PaneSize) {
	now := uint64(max(time.Now().Unix(), 0))
	renderQuotasAt(frame, area, dashboard, size, now, clockLabel)
}

// renderQuotasAt draws the Quota pane at now, formatting reset times with
// clock. size is set on the combined surface, where the pane carries its
// size controls and its title opens the pane's menu.
func renderQuotasAt(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, size *core.PaneSize, now uint64, clock func(epoch int64, format string) string) {
	// Provider discovery is polled once per frame by the combined renderer,
	// before pane sizing, so sizing and drawing use the same snapshot.
	var refreshStatus string
	if len(dashboard.quotaRefreshing) > 0 {
		refreshStatus = "refreshing" + chat.ThemeGlyphs().Ellipsis
	} else {
		var oldest *uint64
		for _, quota := range dashboard.quotas {
			if oldest == nil || quota.RefreshedAtEpochSeconds < *oldest {
				refreshed := quota.RefreshedAtEpochSeconds
				oldest = &refreshed
			}
		}
		if oldest != nil {
			refreshStatus = "refreshed " + refreshAge(now, *oldest)
		} else {
			refreshStatus = "not refreshed"
		}
	}
	// On the combined surface (size is set) the title opens the pane's
	// menu, and says so with a dropdown mark.
	label := paneTitleLabel("Profiles", size != nil)
	labelWidth := chat.RawLine(label).Width()
	titleBudget := max(area.Dx()-2, 0)
	if size != nil {
		titleBudget = paneTitleContentWidth(area.Dx(), dashboard.paneMaximizeEnabled(SupportPaneQuota))
	}
	statusBudget := max(titleBudget-labelWidth, 0)
	status := truncateToCells("("+refreshStatus+") ", statusBudget, TruncateSummary)
	pressed := size != nil && registerPaneTitleMenu(dashboard, SupportPaneQuota, uv.Rect(
		area.Min.X+1,
		area.Min.Y,
		min(labelWidth, math.MaxUint16, titleBudget),
		min(area.Dy(), 1)))
	labelSpan := chat.RawSpan(label)
	if pressed {
		labelSpan = chat.StyledSpan(label, chat.Selection(true))
	}
	title := chat.LineFrom(labelSpan, chat.StyledSpan(status, chat.Style{}.Foreground(chat.ThemePalette().Muted)))

	focused := dashboard.focus == FocusQuota
	block := chat.Panel(focused).WithTitle(title)
	if size != nil {
		block = block.WithTitle(paneSizeControls(*size, dashboard.paneMaximizeEnabled(SupportPaneQuota)))
	}
	inner := block.Inner(area)
	frame.RenderWidget(block, area)
	laid := quotaLines(dashboard, inner.Dx(), now, focused, clock)
	viewport := inner.Dy()
	dashboard.quotaViewport = viewport
	offset := min(dashboard.quotaScroll, max(len(laid.lines)-viewport, 0))
	selected := dashboard.selectedQuotaRow()
	var renderedSelection *quotaRenderedSelection
	if selected != nil {
		for _, poolRow := range laid.poolRows {
			if poolRow.Row != selected {
				continue
			}
			if renderedSelection == nil {
				renderedSelection = &quotaRenderedSelection{row: selected, first: poolRow.Line, last: poolRow.Line}
			} else {
				renderedSelection.last = poolRow.Line
			}
		}
	}
	previous := dashboard.quotaRenderedSelection
	moved := (previous == nil) != (renderedSelection == nil) ||
		(previous != nil && *previous != *renderedSelection)
	dashboard.quotaRenderedSelection = renderedSelection
	recenter := takeSelectionRecenter(dashboard, FocusQuota)
	if (recenter || (focused && moved)) && renderedSelection != nil {
		first, last := renderedSelection.first, renderedSelection.last
		if first < offset {
			offset = first
		}
		if last >= offset+viewport {
			offset = min(max(last+1-viewport, 0), first)
		}
	}
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(laid.lines...)).WithScroll(min(offset, math.MaxUint16), 0), inner)
	dashboard.quotaScroll = offset
	var poolAreas []client.DaemonPair[QuotaRow, uv.Rectangle]
	for _, poolRow := range laid.poolRows {
		if poolRow.Line >= offset && poolRow.Line < offset+viewport {
			poolAreas = append(poolAreas, client.DaemonPair[QuotaRow, uv.Rectangle]{
				First:  poolRow.Row,
				Second: uv.Rect(inner.Min.X, inner.Min.Y+(poolRow.Line-offset), inner.Dx(), 1),
			})
		}
	}
	dashboard.quotaPoolAreas = poolAreas
	var rowAreas []client.DaemonPair[string, uv.Rectangle]
	for _, profileRow := range laid.profileRows {
		if profileRow.Line >= offset && profileRow.Line < offset+viewport {
			rowAreas = append(rowAreas, client.DaemonPair[string, uv.Rectangle]{
				First:  profileRow.ID,
				Second: uv.Rect(inner.Min.X, inner.Min.Y+(profileRow.Line-offset), inner.Dx(), 1),
			})
		}
	}
	dashboard.quotaRowAreas = rowAreas
	renderSessionScrollbar(frame, area, len(laid.lines), offset, viewport)
}

// providerAccent is the colour a provider's pools draw in. Provider colours
// paint both foreground and background against the active theme;
// monochrome uses the theme palette.
func providerAccent(provider core.QuotaProvider) chat.Color {
	if chat.Current() == core.UiThemeMono {
		return chat.ThemePalette().Text
	}
	switch provider.(type) {
	case core.QuotaProviderAnthropic:
		return chat.ColorRGB(217, 119, 87)
	case core.QuotaProviderOpenAI:
		return chat.ColorRGB(16, 163, 127)
	case core.QuotaProviderGoogle:
		return chat.ColorRGB(66, 133, 244)
	case core.QuotaProviderXAI:
		return chat.ColorRGB(200, 200, 210)
	case core.QuotaProviderMoonshot:
		return chat.ColorRGB(150, 110, 230)
	case core.QuotaProviderDeepSeek:
		return chat.ColorRGB(77, 107, 254)
	case core.QuotaProviderZai:
		return chat.ColorRGB(60, 170, 200)
	case core.QuotaProviderMeta:
		return chat.ColorRGB(0, 129, 251)
	default:
		return chat.ThemePalette().Muted
	}
}

// tint blends accent towards the active theme's background, never an
// unknown terminal background; strength 1 keeps the accent. A colour that is
// not RGB cannot be blended and comes back unchanged.
func tint(accent chat.Color, strength float64) chat.Color {
	r, g, b, ok := accent.RGB()
	if !ok {
		return accent
	}
	br, bg, bb, ok := chat.ThemePalette().Background.RGB()
	if !ok {
		br, bg, bb = 30, 30, 30
	}
	mix := func(a, b uint8) uint8 {
		return uint8(math.Round(float64(b) + (float64(a)-float64(b))*strength))
	}
	return chat.ColorRGB(mix(r, br), mix(g, bg), mix(b, bb))
}

// readableOn is a label colour with legible contrast on a provider fill:
// dark text on light backgrounds, light text on dark ones. Non-RGB fills and
// monochrome keep the theme text. Contrast is computed from the fill's own
// RGB value, not from a theme colour.
func readableOn(background chat.Color) chat.Color {
	r, g, b, ok := background.RGB()
	if !ok {
		return chat.ThemePalette().Text
	}
	// ASCII still paints RGB provider fills, so it needs the same contrast.
	if chat.Current() == core.UiThemeMono {
		return chat.ThemePalette().Text
	}
	linear := func(c uint8) float64 {
		value := float64(c) / 255.0
		if value <= 0.04045 {
			return value / 12.92
		}
		return math.Pow((value+0.055)/1.055, 2.4)
	}
	luminance := 0.2126*linear(r) + 0.7152*linear(g) + 0.0722*linear(b)
	// Black text beats white above this luminance (equal contrast ratios).
	if luminance > 0.179 {
		return chat.ColorRGB(16, 16, 16)
	}
	return chat.ColorRGB(245, 245, 245)
}

// resetLabel is the countdown to a reset, falling back to the provider's
// reset text when the report carries no timestamp.
func resetLabel(reset *int64, text *string, now uint64) string {
	if reset == nil && text != nil {
		return *text
	}
	return resetDuration(reset, now)
}

// resetDuration is the time from now until reset, as `2d3h`, `4h05m`, `4h`
// or `12m`; `?` when the reset is unknown.
func resetDuration(reset *int64, now uint64) string {
	if reset == nil {
		return "?"
	}
	seconds := uint64(max(*reset, 0))
	seconds = max(seconds, now) - now
	days := seconds / 86400
	hours := seconds % 86400 / 3600
	minutes := seconds % 3600 / 60
	switch {
	case days > 0:
		return fmt.Sprintf("%dd%dh", days, hours)
	case hours > 0 && minutes > 0:
		return fmt.Sprintf("%dh%02dm", hours, minutes)
	case hours > 0:
		return fmt.Sprintf("%dh", hours)
	default:
		return fmt.Sprintf("%dm", minutes)
	}
}

// clockLabel formats epoch in the local timezone with a chrono (strftime)
// format string such as `%a %H:%M`; `unknown` when the time is out of range.
func clockLabel(epoch int64, format string) string {
	if epoch < chronoMinEpoch || epoch > chronoMaxEpoch {
		return "unknown"
	}
	return chronoFormat(time.Unix(epoch, 0).Local(), format)
}

// chronoMinEpoch and chronoMaxEpoch bound the timestamps chrono's
// DateTime::from_timestamp accepts (years -262143 through 262142).
var (
	chronoMinEpoch = time.Date(-262143, time.January, 1, 0, 0, 0, 0, time.UTC).Unix()
	chronoMaxEpoch = time.Date(262142, time.December, 31, 23, 59, 59, 0, time.UTC).Unix()
)

// chronoFormat formats t with a chrono (strftime) format string. It knows
// the specifiers the dashboard's clocks use; any other is written as it
// stands.
func chronoFormat(t time.Time, format string) string {
	var out strings.Builder
	for index := 0; index < len(format); index++ {
		if format[index] != '%' || index+1 == len(format) {
			out.WriteByte(format[index])
			continue
		}
		index++
		switch format[index] {
		case 'a':
			out.WriteString(t.Format("Mon"))
		case 'A':
			out.WriteString(t.Format("Monday"))
		case 'b':
			out.WriteString(t.Format("Jan"))
		case 'd':
			fmt.Fprintf(&out, "%02d", t.Day())
		case 'e':
			fmt.Fprintf(&out, "%2d", t.Day())
		case 'H':
			fmt.Fprintf(&out, "%02d", t.Hour())
		case 'M':
			fmt.Fprintf(&out, "%02d", t.Minute())
		case 'S':
			fmt.Fprintf(&out, "%02d", t.Second())
		case 'm':
			fmt.Fprintf(&out, "%02d", int(t.Month()))
		case 'Y':
			fmt.Fprintf(&out, "%d", t.Year())
		case '%':
			out.WriteByte('%')
		default:
			out.WriteByte('%')
			out.WriteByte(format[index])
		}
	}
	return out.String()
}

// overdueEntry is the line naming a profile whose reset passed without a
// new reading. Its banked resets are a separate balance and stay known.
func overdueEntry(member *PoolMember, ascii bool) string {
	entry := member.ID + " reset due, awaiting reading"
	if member.Banked != nil && *member.Banked > 0 {
		mark := "⟲"
		if ascii {
			mark = "B"
		}
		entry += fmt.Sprintf(" %s%d banked", mark, *member.Banked)
	}
	return entry
}

// poolBar is a pool's bar at width cells: the available share, then one
// refill segment per member that is not full, and the labels that did not
// fit on the bar, for the lines under it.
func poolBar(pool *QuotaPool, width int, now uint64, accent chat.Color, refreshing map[string]struct{}) ([]chat.Span, []string) {
	isRefreshing := func(id string) bool {
		_, ok := refreshing[id]
		return ok
	}
	bankedMark := func(ascii bool) string {
		if ascii {
			return "B"
		}
		return "⟲"
	}
	percentage := pool.percentage()
	padding := 4
	if pool.Missing > 0 {
		padding++
	}
	suffixText := fmt.Sprintf(" %*s", padding, percentage)
	suffixWidth := len(suffixText)
	suffix := func() chat.Span {
		return chat.StyledSpan(suffixText, chat.Style{}.Foreground(accent).Bold())
	}
	weight := pool.reportedWeight()
	if weight == 0 {
		var overdue []string
		for m := range pool.Members {
			if pool.Members[m].AwaitingReading {
				overdue = append(overdue, overdueEntry(&pool.Members[m], chat.ASCII()))
			}
		}
		return []chat.Span{chat.RawSpan(strings.Repeat(" ", max(width-suffixWidth, 0))), suffix()}, overdue
	}
	type refill struct {
		member   *PoolMember
		capacity float64
	}
	var refills []refill
	for _, member := range pool.reportedMembers() {
		if member.Remaining < 100 || isRefreshing(member.ID) {
			refills = append(refills, refill{member, member.Weight * float64(100-int(member.Remaining))})
		}
	}
	available := pool.availableCapacity()
	apportion := func(width int) []int {
		capacities := make([]float64, len(refills))
		for i, refill := range refills {
			capacities[i] = refill.capacity
		}
		return refillWidths(available, capacities, weight*100, width)
	}
	labelWidth := func(label string) int { return chat.RawSpan(label).Width() }
	percentageWidth := labelWidth(percentage)
	widths := apportion(width)
	hasRefill := slices.ContainsFunc(widths[1:], func(cells int) bool { return cells > 0 })
	reserved := 0
	if hasRefill {
		reserved = 1
	}
	inline := percentageWidth <= max(widths[0]-reserved, 0)
	if !inline {
		widths = apportion(max(width-suffixWidth, 0))
	}
	lastSegment := 0
	for index := len(widths) - 1; index >= 0; index-- {
		if widths[index] > 0 {
			lastSegment = index
			break
		}
	}
	ascii := chat.ASCII()
	pane := chat.ThemePalette().Surface
	_, _, _, accentRGB := accent.RGB()
	mono := chat.Current() == core.UiThemeMono
	useGlyphs := ascii || mono || !accentRGB
	refillColor := tint(accent, 0.24)
	// Text or a glyph on a segment colour. readableOn can only contrast with
	// an RGB colour; for a theme colour (and in monochrome) it returns the
	// theme text colour, which may be the segment colour itself, so draw in
	// the segment colour on the pane instead.
	on := func(colour chat.Color) chat.Style {
		if _, _, _, rgb := colour.RGB(); rgb && !mono {
			return chat.Style{}.Foreground(readableOn(colour)).Background(colour)
		}
		return chat.Style{}.Foreground(colour).Background(pane)
	}
	divider := func(colour chat.Color) chat.Span {
		if ascii {
			return chat.StyledSpan("|", on(colour))
		}
		// The segment keeps this cell; its right eighth exposes the pane.
		return chat.StyledSpan("▉", chat.Style{}.Foreground(colour).Background(pane))
	}
	fillGlyph := " "
	switch {
	case ascii:
		fillGlyph = "#"
	case useGlyphs:
		fillGlyph = "█"
	}
	// A full block draws in the foreground colour, so in Unicode glyph mode
	// the fill is the block itself; reversing it would paint the background
	// over the whole cell. ASCII `#` keeps reverse video to stay solid.
	// ASCII fills in reverse video so a percentage printed over the whole
	// fill keeps the fill's look; `on` keeps its two colours distinct.
	var fillStyle chat.Style
	switch {
	case ascii:
		fillStyle = on(accent).Add(chat.ModifierReversed)
	case useGlyphs:
		fillStyle = chat.Style{}.Foreground(accent).Background(pane)
	default:
		fillStyle = chat.Style{}.Foreground(readableOn(accent)).Background(accent)
	}
	var spans []chat.Span
	if widths[0] > 0 {
		split := lastSegment > 0
		cells := widths[0]
		if split {
			cells--
		}
		if inline {
			var labelStyle chat.Style
			switch {
			case useGlyphs && !ascii:
				labelStyle = chat.Style{}.Foreground(readableOn(pane)).Background(pane).Add(chat.ModifierReversed)
			case ascii:
				labelStyle = fillStyle
			default:
				labelStyle = on(accent)
			}
			spans = append(spans, chat.StyledSpan(percentage, labelStyle))
			cells -= percentageWidth
		}
		if cells > 0 {
			spans = append(spans, chat.StyledSpan(strings.Repeat(fillGlyph, cells), fillStyle))
		}
		if split {
			spans = append(spans, divider(accent))
		}
	}
	var fallback []string
	for index, refill := range refills {
		member, capacity := refill.member, refill.capacity
		cells := widths[index+1]
		memberRefreshing := isRefreshing(member.ID)
		status := resetLabel(member.Reset, member.ResetText, now)
		if memberRefreshing {
			status = "refreshing" + chat.ThemeGlyphs().Ellipsis
		}
		var banked *string
		if member.Banked != nil && *member.Banked > 0 {
			text := fmt.Sprintf("%s%d", bankedMark(ascii), *member.Banked)
			banked = &text
		}
		split := index+1 < lastSegment
		// Keep a divider cell at a boundary, and a refill glyph at the end
		// of a glyph bar when text would otherwise cover the segment.
		labelBudget := cells
		if split || useGlyphs {
			labelBudget = max(cells-1, 0)
		}
		candidates := []string{status}
		if memberRefreshing {
			candidates = append(candidates, "refresh", "check")
		}
		shortStatus, hasShort := "", false
		for _, candidate := range candidates {
			if labelWidth(candidate) <= labelBudget {
				shortStatus, hasShort = candidate, true
				break
			}
		}
		label := ""
		if hasShort {
			label = shortStatus
			if banked != nil {
				if fullLabel := shortStatus + " " + *banked; labelWidth(fullLabel) <= labelBudget {
					label = fullLabel
				} else {
					fallback = append(fallback, fmt.Sprintf("%s %s banked", member.ID, *banked))
				}
			}
		} else {
			entry := member.ID
			if capacity > 0 {
				share := capacity / weight
				amount := "<1"
				if share >= 1 {
					amount = fmt.Sprintf("%.0f", share)
				}
				entry += fmt.Sprintf(" +%s%%", amount)
			}
			entry += " " + status
			if banked != nil {
				entry += " " + *banked
			}
			fallback = append(fallback, entry)
		}
		if cells == 0 {
			continue
		}
		body := cells
		if split {
			body--
		}
		if body > 0 {
			glyph := " "
			if useGlyphs {
				glyph = "="
			}
			spans = append(spans, chat.StyledSpan(strings.Repeat(glyph, body-labelWidth(label))+label, on(refillColor)))
		}
		if split {
			spans = append(spans, divider(refillColor))
		}
	}
	if !inline {
		spans = append(spans, suffix())
	}
	// A profile whose reset passed without a new reading adds nothing to the
	// bar; name it so the pane says which profile is waiting.
	for m := range pool.Members {
		if pool.Members[m].AwaitingReading {
			fallback = append(fallback, overdueEntry(&pool.Members[m], ascii))
		}
	}
	// A full profile has no refill chunk, but its banked resets still
	// matter.
	for _, member := range pool.reportedMembers() {
		if member.Remaining >= 100 && !isRefreshing(member.ID) && member.Banked != nil && *member.Banked > 0 {
			fallback = append(fallback, fmt.Sprintf("%s %s%d banked", member.ID, bankedMark(ascii), *member.Banked))
		}
	}
	return spans, fallback
}

// lane is one member's 24-hour timeline at cells cells: when it is usable,
// blocked, and where its resets fall.
func lane(member *PoolMember, period PoolPeriod, cells int, now uint64, accent chat.Color) []chat.Span {
	var spans []chat.Span
	palette := chat.ThemePalette()
	// A reading older than its own reset (weekly or five-hour) cannot say
	// whether the profile is usable now, so draw no availability; reset
	// markers from readings that are still current stay.
	unknown := member.AwaitingReading || member.FiveHourAwaitingReading
	for cell := range cells {
		t0 := saturatingAddUint64(now, uint64(cell)*86400/uint64(cells))
		t1 := saturatingAddUint64(now, uint64(cell+1)*86400/uint64(cells))
		contains := func(reset *int64) bool {
			return reset != nil && *reset >= 0 && t0 <= uint64(*reset) && uint64(*reset) < t1
		}
		var weeklyReset *int64
		if !member.AwaitingReading {
			weeklyReset = member.Reset
		}
		var fiveReset *int64
		if member.FiveHour != nil && !member.FiveHourAwaitingReading {
			fiveReset = member.FiveHour.ResetsAtEpochSeconds
		}
		resetAfter := func(reset *int64) bool { return reset == nil || *reset > int64(t0) }
		blocked := (member.OtherLongBlockedUntil != nil && *member.OtherLongBlockedUntil > int64(t0)) ||
			(member.Remaining == 0 && resetAfter(member.Reset))
		if member.FiveHour != nil {
			if remaining, ok := quotaRemainingPercent(member.FiveHour); ok && remaining == 0 && resetAfter(member.FiveHour.ResetsAtEpochSeconds) {
				blocked = true
			}
		}
		// A five-hour reading without a reset time is still the active
		// balance; only its end is unknown.
		var five *client.QuotaWindow
		if member.FiveHour != nil && resetAfter(member.FiveHour.ResetsAtEpochSeconds) {
			five = member.FiveHour
		}
		var marker rune
		hasMarker := true
		switch {
		case contains(weeklyReset) && contains(fiveReset):
			marker = '*'
		case contains(weeklyReset):
			marker = period.marker()
		case contains(fiveReset):
			marker = '5'
		default:
			hasMarker = false
		}
		foreground := palette.Muted
		if hasMarker {
			foreground = palette.Text
		}
		if unknown {
			symbol := marker
			if !hasMarker {
				symbol = '·'
				if chat.ASCII() {
					symbol = '?'
				}
			}
			spans = append(spans, chat.StyledSpan(string(symbol),
				chat.Style{}.Foreground(foreground).Background(palette.Background)))
			continue
		}
		symbol := marker
		if !hasMarker {
			switch {
			case blocked && chat.ASCII():
				symbol = '.'
			case blocked:
				symbol = '·'
			case chat.ASCII() || chat.Current() == core.UiThemeMono:
				symbol = '-'
				if five != nil {
					symbol = '='
				}
			default:
				symbol = ' '
			}
		}
		var background chat.Color
		switch {
		case blocked:
			background = palette.Background
		case five != nil:
			remaining, _ := quotaRemainingPercent(five)
			background = tint(accent, 0.25+0.75*float64(remaining)/100.0)
		default:
			background = tint(accent, 0.18)
		}
		spans = append(spans, chat.StyledSpan(string(symbol), chat.Style{}.Foreground(foreground).Background(background)))
	}
	return spans
}

// saturatingAddUint64 is u64::saturating_add.
func saturatingAddUint64(a, b uint64) uint64 {
	if sum, carry := bits.Add64(a, b, 0); carry == 0 {
		return sum
	}
	return math.MaxUint64
}

// laneNote is the text beside a member's lane: its share, when it resets,
// and anything else its reading says.
func laneNote(member *PoolMember, period PoolPeriod, now uint64, clock func(epoch int64, format string) string, refreshing bool) string {
	// A collapsed segment can be too narrow to say so; the lane always does.
	refresh := ""
	if refreshing {
		refresh = "refreshing" + chat.ThemeGlyphs().Ellipsis + " · "
	}
	var note string
	switch {
	case member.AwaitingReading:
		note = period.label() + " reset due, awaiting reading"
	case member.Remaining == 0:
		back := "unknown"
		if member.Reset != nil {
			back = clock(*member.Reset, "%a %H:%M")
		}
		note = fmt.Sprintf("%s back %s (%s)", period.label(), back, resetLabel(member.Reset, member.ResetText, now))
	default:
		note = fmt.Sprintf("%s %d%%, resets in %s", period.label(), member.Remaining, resetLabel(member.Reset, member.ResetText, now))
	}
	_, week := period.(PoolPeriodWeek)
	switch {
	case member.FiveHourAwaitingReading:
		note += " · 5H reset due, awaiting reading"
	case member.FiveHour != nil:
		remaining := "?"
		if percent, ok := quotaRemainingPercent(member.FiveHour); ok {
			remaining = fmt.Sprintf("%d", percent)
		}
		note += fmt.Sprintf(" · 5H %s%%, resets in %s", remaining,
			resetLabel(member.FiveHour.ResetsAtEpochSeconds, member.FiveHour.Resets, now))
	case member.Remaining > 0 && !member.AwaitingReading && member.FiveHourIdle && week:
		note += " · 5H idle: a fresh window starts on first use"
	}
	if member.Remaining > 0 && !member.AwaitingReading && member.Fable != nil {
		note += fmt.Sprintf(" · Fable %d%%", *member.Fable)
	}
	if member.Note != nil {
		note += " · " + *member.Note
	}
	if member.Banked != nil && *member.Banked > 0 {
		mark := "⟲"
		if chat.ASCII() {
			mark = "B"
		}
		note += fmt.Sprintf(" · %s%d banked", mark, *member.Banked)
	}
	return refresh + note
}

// wrapNote wraps text to width cells, one string per row.
func wrapNote(text string, width int) []string {
	lines := chat.WrapLines([]chat.Line{chat.RawLine(text)}, min(max(width, 1), math.MaxUint16))
	rows := make([]string, len(lines))
	for i, line := range lines {
		rows[i] = line.String()
	}
	return rows
}

// refillLines lays labels out under a bar, joined while they fit on a row
// and wrapped when one alone does not.
func refillLines(labels []string, width int) []chat.Line {
	var lines []chat.Line
	row := ""
	separator := chat.ThemeGlyphs().FooterSeparator
	width = max(width-2, 0)
	for _, label := range labels {
		if row != "" {
			joined := row + separator + label
			if chat.RawSpan(joined).Width() <= width {
				row = joined
				continue
			}
			lines = append(lines, chat.StyledLine("  "+row, chat.Muted()))
		}
		row = label
		if chat.RawSpan(row).Width() > width {
			for _, text := range wrapNote(row, width) {
				lines = append(lines, chat.StyledLine("  "+text, chat.Muted()))
			}
			row = ""
		}
	}
	if row != "" {
		lines = append(lines, chat.StyledLine("  "+row, chat.Muted()))
	}
	return lines
}

// QuotaLines is what quotaLines lays out: the pane's lines, the line each
// selectable row starts on (a row can span several), and the lines that
// belong to each profile.
type QuotaLines struct {
	lines       []chat.Line
	poolRows    []QuotaRowLine
	profileRows []ProfileRowLine
}

// QuotaRowLine is a selectable row and one line it occupies (Rust's
// `(QuotaRow, usize)`).
type QuotaRowLine struct {
	Row  QuotaRow
	Line int
}

// ProfileRowLine is a profile id and one line that belongs to it (Rust's
// `(String, usize)`).
type ProfileRowLine struct {
	ID   string
	Line int
}

// quotaLines lays out the Quota pane's content at width cells.
func quotaLines(dashboard *DashboardState, width int, now uint64, focused bool, clock func(epoch int64, format string) string) QuotaLines {
	var laid QuotaLines
	isRefreshing := func(id string) bool {
		_, ok := dashboard.quotaRefreshing[id]
		return ok
	}
	rowStyle := func(selected bool) chat.Style {
		if selected {
			return chat.Selection(true)
		}
		return chat.Style{}
	}
	providers := providerPools(dashboard.config.Get(), dashboard.quotas, &dashboard.quotaProviders, now)
	selection := dashboard.selectedQuotaRow()
	isSelected := func(row QuotaRow) bool { return focused && selection != nil && selection == row }
	for _, provider := range providers {
		accent := providerAccent(provider.Provider)
		laid.lines = append(laid.lines, chat.StyledLine(
			strings.ToUpper(provider.Provider.DisplayName()),
			chat.Style{}.Foreground(accent).Bold()))
		for p := range provider.Pools {
			pool := &provider.Pools[p]
			var row QuotaRow = QuotaRowPool{Key: pool.Key}
			selected := isSelected(row)
			laid.poolRows = append(laid.poolRows, QuotaRowLine{Row: row, Line: len(laid.lines)})
			expanded := slices.Contains(dashboard.quotaExpanded, pool.Key)
			prefix := " "
			if selected {
				prefix = ">"
			}
			if expanded {
				prefix += "- "
			} else {
				prefix += "+ "
			}
			if pool.Label != "" {
				prefix += pool.Label + " "
			}
			barWidth := max(width-utf8.RuneCountInString(prefix), 0)
			spans := []chat.Span{chat.StyledSpan(prefix, rowStyle(selected))}
			bar, refillLabels := poolBar(pool, barWidth, now, accent, dashboard.quotaRefreshing)
			spans = append(spans, bar...)
			if len(pool.Members) == 1 {
				laid.profileRows = append(laid.profileRows, ProfileRowLine{ID: pool.Members[0].ID, Line: len(laid.lines)})
			}
			laid.lines = append(laid.lines, chat.LineFrom(spans...))
			if pool.Missing > 0 {
				plural := "s"
				if pool.Missing == 1 {
					plural = ""
				}
				laid.lines = append(laid.lines, chat.StyledLine(
					fmt.Sprintf("  %d profile%s not reporting", pool.Missing, plural), chat.Muted()))
			}
			// Expanded lanes already give each profile's reset and banked
			// resets; the list only supplies what a collapsed bar cannot
			// fit.
			if !expanded {
				laid.lines = append(laid.lines, refillLines(refillLabels, width)...)
			}
			// Warnings (a refused check, unknown windows) stay visible while
			// collapsed; each line names one member and opens its menu.
			for m := range pool.Members {
				member := &pool.Members[m]
				if member.Note == nil {
					continue
				}
				share := fmt.Sprintf("%d%%", member.Remaining)
				if member.AwaitingReading {
					share = "?"
				}
				for _, text := range wrapNote(fmt.Sprintf("%s %s · %s", member.ID, share, *member.Note), max(width-2, 0)) {
					laid.profileRows = append(laid.profileRows, ProfileRowLine{ID: member.ID, Line: len(laid.lines)})
					laid.lines = append(laid.lines, chat.StyledLine("  "+text, chat.Muted()))
				}
			}
			if !expanded {
				continue
			}
			nameWidth := 0
			for m := range pool.Members {
				nameWidth = max(nameWidth, utf8.RuneCountInString(pool.Members[m].ID))
			}
			nameWidth += 3
			nameWidth = min(nameWidth, max(width-12, 3))
			notes := make([]string, len(pool.Members))
			longestNote := 0
			for m := range pool.Members {
				member := &pool.Members[m]
				notes[m] = laneNote(member, pool.Key.Period, now, clock, isRefreshing(member.ID))
				longestNote = max(longestNote, chat.RawSpan(notes[m]).Width())
			}
			laneWidth := min(max(min(max(width-(nameWidth+longestNote+2), 0), 48), 12), max(width-nameWidth, 0))
			axis := []rune(strings.Repeat(" ", laneWidth))
			for cell := 0; cell < laneWidth; cell += max(laneWidth/6, 6) {
				label := clock(int64(now)+int64(cell*86400/laneWidth), "%H:%M")
				if cell+len(label) <= laneWidth {
					for i, ch := range []rune(label) {
						axis[cell+i] = ch
					}
				}
			}
			laid.lines = append(laid.lines, chat.StyledLine(strings.Repeat(" ", nameWidth)+string(axis), chat.Muted()))
			for m := range pool.Members {
				member, note := &pool.Members[m], notes[m]
				laid.profileRows = append(laid.profileRows, ProfileRowLine{ID: member.ID, Line: len(laid.lines)})
				var row QuotaRow = QuotaRowLane{Key: pool.Key, ID: member.ID}
				selected := isSelected(row)
				laid.poolRows = append(laid.poolRows, QuotaRowLine{Row: row, Line: len(laid.lines)})
				marker := " "
				if selected {
					marker = ">"
				}
				spans := []chat.Span{chat.StyledSpan(
					fmt.Sprintf("%s %-*s ", marker, nameWidth-3, truncateToCells(member.ID, nameWidth-3, TruncatePlain)),
					rowStyle(selected))}
				timeline := lane(member, pool.Key.Period, laneWidth, now, accent)
				if slices.ContainsFunc(timeline, func(span chat.Span) bool { return span.Content == "*" }) {
					note += fmt.Sprintf(" · *=%c+5 (both resets)", pool.Key.Period.marker())
				}
				spans = append(spans, timeline...)
				if nameWidth+laneWidth+2+chat.RawSpan(note).Width() <= width {
					spans = append(spans, chat.StyledSpan("  "+note, chat.Muted()))
					laid.lines = append(laid.lines, chat.LineFrom(spans...))
					continue
				}
				laid.lines = append(laid.lines, chat.LineFrom(spans...))
				for _, text := range wrapNote(note, max(width-4, 0)) {
					laid.poolRows = append(laid.poolRows, QuotaRowLine{Row: row, Line: len(laid.lines)})
					laid.profileRows = append(laid.profileRows, ProfileRowLine{ID: member.ID, Line: len(laid.lines)})
					laid.lines = append(laid.lines, chat.StyledLine("    "+text, chat.Muted()))
				}
			}
		}
		for _, text := range provider.Text {
			id, label := text.ID, text.Text
			if isRefreshing(id) {
				label = "refreshing" + chat.ThemeGlyphs().Ellipsis
			}
			laid.profileRows = append(laid.profileRows, ProfileRowLine{ID: id, Line: len(laid.lines)})
			var row QuotaRow = QuotaRowProfile{ID: id}
			selected := isSelected(row)
			for index, wrapped := range wrapNote(id+": "+label, max(width-2, 0)) {
				laid.poolRows = append(laid.poolRows, QuotaRowLine{Row: row, Line: len(laid.lines)})
				laid.profileRows = append(laid.profileRows, ProfileRowLine{ID: id, Line: len(laid.lines)})
				marker := " "
				if selected && index == 0 {
					marker = ">"
				}
				style := chat.Muted()
				if selected {
					style = chat.Selection(true)
				}
				laid.lines = append(laid.lines, chat.StyledLine(marker+" "+wrapped, style))
			}
		}
	}
	if len(laid.lines) == 0 {
		laid.lines = append(laid.lines, chat.StyledLine("Add an agent profile in Settings.", chat.Muted()))
	}
	return laid
}
