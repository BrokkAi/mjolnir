// Port of mj-tui/src/render/quotas.rs.
package tui

import (
	"fmt"
	"iter"
	"math"
	"time"

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
	panic("unported: mj-tui/src/render/quotas.rs minimized_targets_line")
}

// minimizedQuotaLine is a weighted reading for each independently metered
// provider pool.
func minimizedQuotaLine(dashboard *DashboardState, width int, focused bool, now uint64) chat.Line {
	panic("unported: mj-tui/src/render/quotas.rs minimized_quota_line")
}

// summaryRow is a minimized pane's single row, drawn as the pane's own title
// so it keeps the rule the full pane has. Readings are comma-separated and
// truncated rather than wrapped, because the row is one row.
func summaryRow(label string, readings []SummaryReading, width int, focused bool) chat.Line {
	panic("unported: mj-tui/src/render/quotas.rs summary_row")
}

// quotaRemainingPercent is the share of a window left, from its reported
// percentage or else from its used and limit counts; false when neither is
// readable.
func quotaRemainingPercent(window *client.QuotaWindow) (uint8, bool) {
	panic("unported: mj-tui/src/render/quotas.rs quota_remaining_percent")
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
	panic("unported: mj-tui/src/render/quotas.rs render_quotas_at")
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
	panic("unported: mj-tui/src/render/quotas.rs clock_label")
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
	panic("unported: mj-tui/src/render/quotas.rs pool_bar")
}

// lane is one member's 24-hour timeline at cells cells: when it is usable,
// blocked, and where its resets fall.
func lane(member *PoolMember, period PoolPeriod, cells int, now uint64, accent chat.Color) []chat.Span {
	panic("unported: mj-tui/src/render/quotas.rs lane")
}

// laneNote is the text beside a member's lane: its share, when it resets,
// and anything else its reading says.
func laneNote(member *PoolMember, period PoolPeriod, now uint64, clock func(epoch int64, format string) string, refreshing bool) string {
	panic("unported: mj-tui/src/render/quotas.rs lane_note")
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
	panic("unported: mj-tui/src/render/quotas.rs refill_lines")
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
	panic("unported: mj-tui/src/render/quotas.rs quota_lines")
}
