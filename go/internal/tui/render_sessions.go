// Port of mj-tui/src/render/sessions.rs.
package tui

import (
	"fmt"
	"iter"
	"math"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SessionRowsRendered is the session-row rendering results that the caller
// folds back into the combined surface's mouse hitboxes once the borrow of
// the session list has ended.
//
// The pairs are Rust's `(usize, Rect)` (an index into ordered_sessions())
// and `(String, Rect)` (a project key), in the shape DashboardState keeps
// them.
type SessionRowsRendered struct {
	sessionRowAreas     []client.DaemonPair[int, uv.Rectangle]
	projectHeadingAreas []client.DaemonPair[string, uv.Rectangle]
}

// paneSizeControlsWidthWithMaximum is the width of the three size controls.
// Rust's PANE_SIZE_CONTROLS_WIDTH; renamed because its camelCase form is the
// function paneSizeControlsWidth.
const paneSizeControlsWidthWithMaximum = 11

// paneSizeControlsWidthWithoutMaximum is the width of the controls without
// the maximize chip.
const paneSizeControlsWidthWithoutMaximum = 7

// paneSizeControlWidth is the width of one padded control chip.
const paneSizeControlWidth = 3

// paneSizeControlsWidth is the width a pane's visible size controls take.
func paneSizeControlsWidth(maximizeEnabled bool) int {
	if maximizeEnabled {
		return paneSizeControlsWidthWithMaximum
	}
	return paneSizeControlsWidthWithoutMaximum
}

// paneTitleContentWidth is the width left for a pane's left title after
// preserving the border, a gap, and the currently visible right-aligned size
// controls.
func paneTitleContentWidth(width int, maximizeEnabled bool) int {
	return max(width-(2+1+paneSizeControlsWidth(maximizeEnabled)), 0)
}

// displayedPaneSize is the size the controls mark as active: a maximized
// pane whose maximize control is hidden shows as standard.
func displayedPaneSize(active core.PaneSize, maximizeEnabled bool) core.PaneSize {
	if active == core.PaneSizeMaximized && !maximizeEnabled {
		return core.PaneSizeStandard
	}
	return active
}

// paneSizeControls is the title-bar controls. Their padded backgrounds are
// the buttons; the unstyled cells between them keep inactive controls
// visually distinct.
func paneSizeControls(active core.PaneSize, maximizeEnabled bool) chat.Line {
	active = displayedPaneSize(active, maximizeEnabled)
	glyphs := chat.ThemeGlyphs()
	controls := []struct {
		size  core.PaneSize
		glyph string
	}{
		{core.PaneSizeMinimized, glyphs.SizeMinimized},
		{core.PaneSizeStandard, glyphs.SizeStandard},
		{core.PaneSizeMaximized, glyphs.SizeMaximized},
	}
	var spans []chat.Span
	for _, control := range controls {
		if control.size == core.PaneSizeMaximized && !maximizeEnabled {
			continue
		}
		if len(spans) > 0 {
			spans = append(spans, chat.RawSpan(" "))
		}
		style := chat.Actionable().Background(chat.ThemePalette().Surface)
		if control.size == active {
			style = chat.ActiveControl()
		}
		spans = append(spans, chat.StyledSpan(" "+control.glyph+" ", style))
	}
	return chat.LineFrom(spans...).RightAligned()
}

// minimizedPaneSizeControls: minimized panes have no right border, but keep
// its column as horizontal rule so their controls line up with those in a
// fully bordered pane.
func minimizedPaneSizeControls(focused bool, maximizeEnabled bool) chat.Line {
	controls := paneSizeControls(core.PaneSizeMinimized, maximizeEnabled)
	controls.Spans = append(controls.Spans, chat.RawSpan(chat.ThemeGlyphs().Rule))
	return controls
}

// PaneSizeControlArea is one size control chip on screen (Rust's
// `(SupportPane, PaneSize, Rect)`).
type PaneSizeControlArea struct {
	Pane SupportPane
	Size core.PaneSize
	Area uv.Rectangle
}

// paneSizeControlAreas is the screen rectangles occupied by the padded
// control chips in a bordered pane's right-aligned title.
func paneSizeControlAreas(area uv.Rectangle, pane SupportPane, maximizeEnabled bool) []PaneSizeControlArea {
	start := max(area.Max.X-1-paneSizeControlsWidth(maximizeEnabled), 0)
	var areas []PaneSizeControlArea
	for _, size := range []core.PaneSize{core.PaneSizeMinimized, core.PaneSizeStandard, core.PaneSizeMaximized} {
		if size == core.PaneSizeMaximized && !maximizeEnabled {
			continue
		}
		x := start + len(areas)*4
		areas = append(areas, PaneSizeControlArea{Pane: pane, Size: size, Area: uv.Rect(x, area.Min.Y, paneSizeControlWidth, 1)})
	}
	return areas
}

// DrawnSessionRow is one table row of the Sessions pane, already laid out.
type DrawnSessionRow struct {
	// session is the index into ordered_sessions() for the session this row
	// draws.
	session *int
	// heading is the project key of the heading this row carries, if it
	// opens a group.
	heading *string
	lines   []chat.Line
	height  int
	// spacing is the blank rows drawn under this one, to separate groups.
	spacing int
}

// SessionRowsRenderOptions says which form of the Sessions rows to draw.
type SessionRowsRenderOptions struct {
	summaryOnly        bool
	showProjectNumbers bool
	showSelection      bool
}

// sessionRowsRenderOptionsDashboard is SessionRowsRenderOptions::DASHBOARD,
// the full pane.
var sessionRowsRenderOptionsDashboard = SessionRowsRenderOptions{
	summaryOnly:        false,
	showProjectNumbers: true,
	showSelection:      true,
}

// sessionRowsRenderOptionsMinimized is SessionRowsRenderOptions::MINIMIZED,
// one terse row per session.
var sessionRowsRenderOptionsMinimized = SessionRowsRenderOptions{
	summaryOnly:        true,
	showProjectNumbers: true,
	showSelection:      true,
}

func (r *DrawnSessionRow) contentHeight() int {
	return r.height
}

// drawnSessionRows lays the pane out without drawing it, so the layout can
// ask how tall it wants to be before it has any rows to give it. Only tests
// use this.
func drawnSessionRows(dashboard *DashboardState, width int) []DrawnSessionRow {
	return drawnSessionRowsWithOptions(dashboard, width, sessionRowsRenderOptionsDashboard)
}

// drawnSessionRowsWithOptions is drawnSessionRows in the given form. Only
// tests use this.
func drawnSessionRowsWithOptions(dashboard *DashboardState, width int, options SessionRowsRenderOptions) []DrawnSessionRow {
	layout := newSessionsLayout(dashboard, width)
	return drawnSessionRowsInRange(dashboard, &layout, options, 0, math.MaxInt)
}

// drawnSessionRowsInRange builds the rows at positions [rangeStart,
// rangeEnd) of the layout (Rust's `range: Range<usize>`).
func drawnSessionRowsInRange(dashboard *DashboardState, layout *SessionsLayout, options SessionRowsRenderOptions, rangeStart, rangeEnd int) []DrawnSessionRow {
	panic("unported: mj-tui/src/render/sessions.rs drawn_session_rows_in_range")
}

// fitGitRowText is the most of the git row text — `⎇ main ↑1 ↓2 ±4` — that
// fits free cells, or false when not even a marker and a readable name fit.
//
// Every session created with a managed worktree gets an `mj/<32 hex>`
// branch, which is wider than the sidebar on its own. Giving up the whole
// marker there also gave up the counts, which are the part that says the
// checkout moved, so the name gives up its middle first:
// `⎇ mj/d45dc…0580 ±2`. The middle of a generated name is what nobody reads;
// its start and its last few characters are what tells two branches apart.
func fitGitRowText(git string, free int) (string, bool) {
	panic("unported: mj-tui/src/render/sessions.rs fit_git_row_text")
}

// OutputPreviewCache is the output rows an expanded session row shows for
// its message, kept with the session's detail until that message, or what
// its rendering depends on, changes.
//
// Rendering a reply's markdown is the costly part of a Sessions frame, and a
// session's message changes far less often than the pane is drawn. Each
// frame compares the message with the one these rows were made from and
// reuses them when nothing changed. The zero value is empty.
type OutputPreviewCache struct {
	cached *OutputPreview
	// renders is how many times the rows were rendered, for tests to count.
	renders int
}

// OutputPreview is the rows rendered for one message and what they depend
// on.
type OutputPreview struct {
	label   string
	message string
	width   int
	theme   core.UiTheme
	ascii   bool
	rows    []chat.Line
}

// rows is at most two rows of message, after label, at width cells.
func (c *OutputPreviewCache) rows(label string, message string, width int) []chat.Line {
	panic("unported: mj-tui/src/render/sessions.rs OutputPreviewCache::rows")
}

// expandedSessionLines appends the four rows an expanded session draws:
// name, status and identity, and two wrapped rows of the current output. The
// output block is always two rows, even with nothing to say, so every
// expanded session is the same height and the layout can be computed from a
// count.
func expandedSessionLines(
	lines *[]chat.Line,
	session *core.SessionRecord,
	displayName string,
	detail *SessionDetail,
	reviews []client.RuntimeReviewView,
	unreachable bool,
	diskFull bool,
	attention AttentionLevel,
	waitingChild *core.SubagentKind,
	operation *SessionOperationDisplay,
	nowEpochSeconds uint64,
	target string,
	cpu *CpuShare,
	permission *chat.Span,
	width int,
	titleWidth int,
	prefix string,
	spinner *string,
	detailedActivityClocks bool,
	git *string,
) {
	panic("unported: mj-tui/src/render/sessions.rs expanded_session_lines")
}

// minNameCellsWithCPU is the fewest name cells worth showing next to a CPU
// figure. A name that is shorter than this needs only its own width.
const minNameCellsWithCPU = 8

// cpuRowLabel is the CPU text a row shows, or false below the display
// threshold (1.0%).
func cpuRowLabel(cpu *CpuShare) (string, bool) {
	if cpu == nil || !(cpu.included || cpu.permille >= 10) {
		return "", false
	}
	return cpu.label(), true
}

// sessionTitleSpans is the first row of a session: the name, the branch
// when there is room, and the CPU figure right-justified to titleWidth, the
// edge the row's controls leave clear. A narrow row truncates the name before
// it gives up the CPU; the CPU goes only when the name would get fewer than
// [minNameCellsWithCPU] cells (or its own width, if shorter).
func sessionTitleSpans(prefix string, name string, style chat.Style, branch *string, cpu *CpuShare, titleWidth int) []chat.Span {
	panic("unported: mj-tui/src/render/sessions.rs session_title_spans")
}

// sessionActivityLine is the second row of a session. Actionable state and
// queued work come first so a narrow pane cannot hide them behind the target
// or profile identity.
func sessionActivityLine(
	prefix string,
	session *core.SessionRecord,
	detail *SessionDetail,
	reviews []client.RuntimeReviewView,
	unreachable bool,
	diskFull bool,
	attention AttentionLevel,
	waitingChild *core.SubagentKind,
	operation *SessionOperationDisplay,
	nowEpochSeconds uint64,
	target string,
	permission *chat.Span,
	spinner *string,
	width int,
	detailedActivityClocks bool,
) chat.Line {
	panic("unported: mj-tui/src/render/sessions.rs session_activity_line")
}

// compactSessionLines appends a folded or minimized session's fixed
// two-line summary: identity on the first line, then status, clock, and
// queued work on the second.
func compactSessionLines(
	lines *[]chat.Line,
	prefix string,
	session *core.SessionRecord,
	displayName string,
	detail *SessionDetail,
	reviews []client.RuntimeReviewView,
	unreachable bool,
	diskFull bool,
	attention AttentionLevel,
	waitingChild *core.SubagentKind,
	operation *SessionOperationDisplay,
	nowEpochSeconds uint64,
	target string,
	cpu *CpuShare,
	permission *chat.Span,
	spinner *string,
	width int,
	titleWidth int,
) {
	panic("unported: mj-tui/src/render/sessions.rs compact_session_lines")
}

// SessionRowFacts is the shared state used to derive a session's status,
// activity clock, and colour.
type SessionRowFacts struct {
	detail          *SessionDetail
	unreachable     bool
	state           core.SessionState
	nowEpochSeconds uint64
	// attention is how much this session needs a person, from the one shared
	// ladder. The symbol and the band colour both read it, so a row cannot
	// say one thing with its glyph and another with its colour.
	attention AttentionLevel
}

// statusSymbol is the one status symbol used in expanded, compact, and
// minimized rows so lifecycle and attention state remains visible at every
// width.
func (f SessionRowFacts) statusSymbol() string {
	panic("unported: mj-tui/src/render/sessions.rs SessionRowFacts::status_symbol")
}

func (f SessionRowFacts) style() chat.Style {
	return chat.Style{}.Foreground(sessionBandColor(f.attention, f.detail, f.state))
}

func (f SessionRowFacts) clock(detailed bool) string {
	panic("unported: mj-tui/src/render/sessions.rs SessionRowFacts::clock")
}

func (f SessionRowFacts) needsInput() bool {
	return f.attention == AttentionLevelWaiting
}

// sessionDisplayClock returns the moving value currently used in a
// session's activity line. Keeping this decision beside the renderer
// prevents invalidation from inventing a second precedence order for
// operation, lifecycle, and activity clocks.
func sessionDisplayClock(
	dashboard *DashboardState,
	session *core.SessionRecord,
	detail *SessionDetail,
	reviews []client.RuntimeReviewView,
	unreachable bool,
	operation *SessionOperationDisplay,
	nowEpochSeconds uint64,
) (string, bool) {
	panic("unported: mj-tui/src/render/sessions.rs session_display_clock")
}

// currentAgentExcerpt selects only content authored by the agent for the
// expanded output rows. The user prompt is a fallback for the compact
// summary, never an agent excerpt with a misleading prefix.
func currentAgentExcerpt(detail *SessionDetail) (string, bool) {
	panic("unported: mj-tui/src/render/sessions.rs current_agent_excerpt")
}

// sessionDisplayTargets is the target label shown for each of sessions,
// given each one's project key in projects.
//
// A target repeated inside one project is ambiguous on its own, so repeats
// are numbered `[1]`, `[2]`, … in the order they appear. The Sessions order
// keeps these labels for ordered_sessions(), and every Sessions
// representation reads them from there so labels remain consistent.
func sessionDisplayTargets(dashboard *DashboardState, sessions []*core.SessionRecord, projects []string) []string {
	panic("unported: mj-tui/src/render/sessions.rs session_display_targets")
}

// SessionRowForm is one decision about which lines a session row contains.
type SessionRowForm interface {
	isSessionRowForm()
}

// SessionRowFormRepair is a session whose configuration needs repair; Hint
// adds the line saying how.
type SessionRowFormRepair struct{ Hint bool }

// SessionRowFormTransition is the single row of a session a lifecycle owns,
// or whose lifecycle failed.
type SessionRowFormTransition struct {
	Kind   core.SessionTransitionKind
	Failed bool
}

// SessionRowFormExpanded is the four-row form.
type SessionRowFormExpanded struct{}

// SessionRowFormCompact is the two-row form.
type SessionRowFormCompact struct{}

func (SessionRowFormRepair) isSessionRowForm()     {}
func (SessionRowFormTransition) isSessionRowForm() {}
func (SessionRowFormExpanded) isSessionRowForm()   {}
func (SessionRowFormCompact) isSessionRowForm()    {}

// RowPresentation is a session row's form and what surrounds it.
type RowPresentation struct {
	form          SessionRowForm
	heading       bool
	outsideFilter bool
	spacing       int
}

// height is the row's lines, its heading and filter notice included and
// its spacing excluded.
func (p RowPresentation) height() int {
	body := 0
	switch form := p.form.(type) {
	case SessionRowFormRepair:
		body = 2
		if form.Hint {
			body++
		}
	case SessionRowFormTransition:
		body = 1
	case SessionRowFormExpanded:
		body = 4
	case SessionRowFormCompact:
		body = 2
	}
	if p.heading {
		body++
	}
	if p.outsideFilter {
		body++
	}
	return body
}

func rowPresentation(dashboard *DashboardState, session *core.SessionRecord, expanded bool, minimized bool, heading bool) RowPresentation {
	panic("unported: mj-tui/src/render/sessions.rs row_presentation")
}

// RowShape is a row's presentation in both forms of the pane.
type RowShape struct {
	full      RowPresentation
	minimized RowPresentation
}

// SessionsLayout is the Sessions pane's row decisions for both forms in one
// frame. Only the form actually drawn builds text, and only for visible
// rows.
type SessionsLayout struct {
	width            int
	shapes           []RowShape
	minimizedHeights []int
	fullHeights      []int
}

// newSessionsLayout is SessionsLayout::new: decide each row's form, height,
// and spacing before the pane size is known.
func newSessionsLayout(dashboard *DashboardState, width int) SessionsLayout {
	panic("unported: mj-tui/src/render/sessions.rs SessionsLayout::new")
}

// minimizedContentHeight is the content rows the minimized form wants,
// excluding its border.
func (l *SessionsLayout) minimizedContentHeight() int {
	total := 0
	for _, height := range l.minimizedHeights {
		total = min(total+min(height, math.MaxUint16), math.MaxUint16)
	}
	return total
}

// contentHeight is the content rows the full form wants, including actions
// and excluding its border.
func (l *SessionsLayout) contentHeight() int {
	total := sessionActionsHeight
	for _, height := range l.fullHeights {
		total = min(total+min(height, math.MaxUint16), math.MaxUint16)
	}
	return total
}

func (l *SessionsLayout) heights(minimized bool) []int {
	if minimized {
		return l.minimizedHeights
	}
	return l.fullHeights
}

func (l *SessionsLayout) presentation(position int, minimized bool) RowPresentation {
	shape := l.shapes[position]
	if minimized {
		return shape.minimized
	}
	return shape.full
}

// offsetShowingSelected is the first row to draw so that the selected row,
// at or below offset, is on screen. heights include each row's trailing
// spacing; content is the selected row's height without it.
//
// This is ratatui's Table rule, which the pane used while it handed the
// table every row: a selected row whose content fits below the rows above it
// stays put, and otherwise the pane scrolls only until the rows through it,
// spacing included, fit. One difference: when the selected row and its
// spacing are taller than the pane, ratatui scrolls past it and hides it,
// and here it stays at the top.
func offsetShowingSelected(offset int, selected int, content int, heights []int, viewport int) int {
	above := 0
	for _, height := range heights[offset:selected] {
		above += height
	}
	if above+content <= viewport {
		return offset
	}
	occupied := above + heights[selected]
	for offset < selected && occupied > viewport {
		occupied -= heights[offset]
		offset++
	}
	return offset
}

// fullTitlePrefix is the pane name the title carries when there is room for
// it beside a label.
const fullTitlePrefix = " Sessions · "

// titleEnd is what ends a Sessions title after its label: a closing space,
// or, while a filter is in force, the chip that clears it (` × `, or ` x `
// with ASCII symbols), whose own padding takes the place of that space.
func titleEnd(clear bool) string {
	if clear {
		return chat.ThemeGlyphs().Close
	}
	return " "
}

// labelKeepsFullPrefix reports whether a label is short enough for the pane
// to keep its full name rather than falling back to `S · `. Callers that want
// to add to the label ask this first, so the pane name never loses its place
// to something optional. clear says whether the title ends with the filter's
// clear chip.
func labelKeepsFullPrefix(label string, width int, maximizeEnabled bool, clear bool) bool {
	budget := paneTitleContentWidth(width, maximizeEnabled)
	return utf8.RuneCountInString(fullTitlePrefix)+utf8.RuneCountInString(label)+
		utf8.RuneCountInString(titleEnd(clear)) <= budget
}

// SessionsTitle is a Sessions pane title, and where its filter clear chip
// starts, counted in cells from the title's first cell, when the title shows
// one.
type SessionsTitle struct {
	line      chat.Line
	clearChip *int
}

// sessionsTitle: the Sessions title keeps the workspace ahead of the long
// pane label when the screen is narrow, while visible size controls retain
// their cells. With clear, the label ends with the chip that clears the
// filter, and the label gives up its cells to it first.
func sessionsTitle(workspaceName string, width int, maximizeEnabled bool, clear bool) SessionsTitle {
	panic("unported: mj-tui/src/render/sessions.rs sessions_title")
}

// sessionsTitleWithAttention reserves a title suffix for the sessions that
// want a person. At narrow widths a compact form keeps that count visible
// while preserving the Sessions label; the narrowest form is the most urgent
// level's own glyph. The compact forms give up the filter label before they
// give up its clear chip.
//
// The badge (Rust's `Option<(AttentionLevel, usize)>`) is badgeLevel and
// badgeCount when hasBadge.
func sessionsTitleWithAttention(workspaceName string, width int, badgeLevel AttentionLevel, badgeCount int, hasBadge bool, maximizeEnabled bool, clear bool) SessionsTitle {
	panic("unported: mj-tui/src/render/sessions.rs sessions_title_with_attention")
}

// sessionsBlock is the Sessions pane's frame, and where its title's filter
// clear chip starts, as [SessionsTitle].clearChip counts it. The badge is
// shown only while the pane is minimized.
func sessionsBlock(focused bool, workspaceName string, width int, size core.PaneSize, badgeLevel AttentionLevel, badgeCount int, hasBadge bool, maximizeEnabled bool, clear bool) (chat.Block, *int) {
	title := sessionsTitleWithAttention(workspaceName, width, badgeLevel, badgeCount,
		hasBadge && size == core.PaneSizeMinimized, maximizeEnabled, clear)
	block := chat.Panel(focused).WithTitle(title.line).WithTitle(paneSizeControls(size, maximizeEnabled))
	return block, title.clearChip
}

// attentionGlyph is the glyph that stands for an attention level wherever it
// is named: a row symbol, a badge, or a pane title.
func attentionGlyph(level AttentionLevel) string {
	glyphs := chat.ThemeGlyphs()
	switch level {
	case AttentionLevelFailed:
		return glyphs.Failed
	case AttentionLevelUnreachable:
		return glyphs.Unreachable
	case AttentionLevelWaiting:
		return glyphs.Waiting
	case AttentionLevelImportant:
		return glyphs.Important
	case AttentionLevelUnread:
		return glyphs.Unread
	case AttentionLevelWorking:
		return glyphs.Working
	default:
		return glyphs.Idle
	}
}

// attentionColor is the colour an attention level carries: red for
// something broken, the attention colour for something that is only waiting
// on a person.
func attentionColor(level AttentionLevel) chat.Color {
	palette := chat.ThemePalette()
	switch level {
	case AttentionLevelFailed, AttentionLevelUnreachable:
		return palette.SessionError
	case AttentionLevelWaiting, AttentionLevelImportant, AttentionLevelUnread:
		return palette.SessionAttention
	case AttentionLevelWorking, AttentionLevelInactive:
		return palette.SessionActivity
	default:
		return palette.SessionIdle
	}
}

// attentionBadge is the ` ×1` or ` ✓3` a folded heading, a workspace tab, or
// the footer carries: the most urgent unseen level's glyph and its session
// count. The arguments are Rust's `Option<(AttentionLevel, usize)>`, so a
// summary returning (level, count, ok) passes straight in; nil when !ok.
func attentionBadge(level AttentionLevel, count int, ok bool) *chat.Span {
	if !ok {
		return nil
	}
	span := chat.StyledSpan(fmt.Sprintf(" %s%d", attentionGlyph(level), count),
		chat.Style{}.Foreground(attentionColor(level)).Bold())
	return &span
}

// renderSessions draws the Sessions pane from the frame's layout of it, and
// reports the per-row mouse hitboxes. The layout must be for this area's
// content width.
func renderSessions(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, layout *SessionsLayout) SessionRowsRendered {
	panic("unported: mj-tui/src/render/sessions.rs render_sessions")
}

// sessionTransitionLine is the single row used while a lifecycle owns a
// session. It deliberately contains no transcript excerpt: the identity,
// operation, active stages, and elapsed time remain stable across expanded,
// collapsed, and minimized layouts while another session can still be
// selected and used.
func sessionTransitionLine(
	prefix string,
	session *core.SessionRecord,
	displayName string,
	transition core.SessionTransitionKind,
	operation *SessionOperationDisplay,
	nowEpochSeconds uint64,
	target string,
	width int,
	failure *string,
) chat.Line {
	panic("unported: mj-tui/src/render/sessions.rs session_transition_line")
}

// emptyActivity is a session the dashboard has heard nothing operational
// about yet (SessionActivity::default()).
var emptyActivity client.SessionActivity

// sessionTargetLabel is the target a session row names, as
// `<target>/<project>` where the target runs the project directly.
//
// The target id is the session's own, but the project comes from whichever
// session owns the project identity: a sub-agent child works in its
// parent's checkout, so naming it from its own record would label every
// child with the parent's session id.
func sessionTargetLabel(state *core.StateState, session *core.SessionRecord, operation *SessionOperationDisplay, config *core.Config) string {
	panic("unported: mj-tui/src/render/sessions.rs session_target_label")
}

// sessionPermissionBadge is the permission badge of the target the session
// runs on, or is moving to.
func sessionPermissionBadge(session *core.SessionRecord, operation *SessionOperationDisplay, config *core.Config) *chat.Span {
	panic("unported: mj-tui/src/render/sessions.rs session_permission_badge")
}

// permissionBadge is `[G]` for guardian and `[Y]` for yolo; nil for a
// target with no permission mode.
func permissionBadge(mode *core.PermissionMode) *chat.Span {
	if mode == nil {
		return nil
	}
	var span chat.Span
	switch *mode {
	case core.PermissionModeGuardian:
		span = chat.StyledSpan("[G]", chat.Style{}.Foreground(chat.ThemePalette().Success).Bold())
	case core.PermissionModeYolo:
		span = chat.StyledSpan("[Y]", chat.Style{}.Foreground(chat.ThemePalette().Error).Bold())
	default:
		return nil
	}
	return &span
}

// listedCapacityTargets is the targets a Targets row lists. A standard
// runtime whose engine was never installed is not listed: the user never
// configured it. Any other missing runtime stays, marked unavailable.
func listedCapacityTargets(targetIDs []string, dashboard *DashboardState) []string {
	panic("unported: mj-tui/src/render/sessions.rs listed_capacity_targets")
}

// capacityTargetLabels is the Targets cell of a capacity row: the listed
// targets, each with its permission badge or marked unavailable.
func capacityTargetLabels(targetIDs []string, dashboard *DashboardState) chat.Line {
	panic("unported: mj-tui/src/render/sessions.rs capacity_target_labels")
}

// renderSessionScrollbar draws a pane's scrollbar on its right border when
// the content overflows the viewport.
func renderSessionScrollbar(frame *chat.Frame, area uv.Rectangle, contentLength int, position int, viewportContentLength int) {
	if geometry, ok := sessionScrollbarGeometry(area, contentLength, position, viewportContentLength); ok {
		chat.RenderScrollbar(frame, geometry)
	}
}

// sessionScrollbarGeometry is the scrollbar on a pane's right border, inside
// its corners; false when the content fits.
func sessionScrollbarGeometry(area uv.Rectangle, contentLength int, position int, viewportContentLength int) (chat.ScrollbarGeometry, bool) {
	if area.Dx() == 0 || contentLength <= viewportContentLength {
		return chat.ScrollbarGeometry{}, false
	}
	track := uv.Rect(max(area.Max.X-1, 0), area.Min.Y+1, 1, max(area.Dy()-2, 0))
	return chat.NewScrollbarGeometry(track, contentLength, position, viewportContentLength)
}

// operationStatus is the label an operation's row shows and when that work
// started: the active launch stages while a launch, resume or move has any,
// otherwise the operation's own label.
func operationStatus(operation *SessionOperationDisplay) (string, uint64) {
	panic("unported: mj-tui/src/render/sessions.rs operation_status")
}

// sessionUpdatedAtEpochSeconds is when the session record last changed;
// false when its timestamp is unreadable or before the epoch.
func sessionUpdatedAtEpochSeconds(session *core.SessionRecord) (uint64, bool) {
	updated, ok := core.ParseChronoRFC3339(session.UpdatedAt)
	if !ok || updated.Unix() < 0 {
		return 0, false
	}
	return uint64(updated.Unix()), true
}

// sessionName is the name a session row shows.
func sessionName(session *core.SessionRecord) string {
	return session.ListedTitle()
}

// NoticeNameCells is how many cells a notice gives a session's name.
const NoticeNameCells = 40

// FitSessionName is a session's name as one line of at most room cells, cut
// with the theme's ellipsis. A session named by its initial prompt can have
// a title of many paragraphs; the Sessions row shows the start of it, and so
// does every dialog and notice that names the session.
func FitSessionName(name string, room int) string {
	return chat.TruncateToCells(name, room, chat.TruncateSummary)
}

// reviewStatusLabel maps the controller's review projection to the short
// overlay that fits in every session row. The controller owns the detailed
// wording and verdict; the TUI only compresses that authoritative view for
// the list.
func reviewStatusLabel(reviews []client.RuntimeReviewView) (string, bool) {
	return client.CombinedActivityLabel(sessionsReviewViews(reviews))
}

// sessionsReviewViews is the reviews as the sequence the client's review
// helpers read.
func sessionsReviewViews(reviews []client.RuntimeReviewView) iter.Seq[*client.RuntimeReviewView] {
	return func(yield func(*client.RuntimeReviewView) bool) {
		for i := range reviews {
			if !yield(&reviews[i]) {
				return
			}
		}
	}
}

// sessionBandColor is the colour a session's summary rows carry.
//
// A session that wants a person takes that level's colour, so the band and
// the row symbol always agree: red for a failure or an unreachable worker,
// the attention colour for a question or unread output. What is left is a
// live session: blue when it is genuinely idle, amber while it or its
// lifecycle is busy.
func sessionBandColor(level AttentionLevel, detail *SessionDetail, state core.SessionState) chat.Color {
	panic("unported: mj-tui/src/render/sessions.rs session_band_color")
}

// checkpointAge is how long ago checkpointedAt was, as `42s`, `5m`, `3h` or
// `2d`; `unknown` when the timestamp is unreadable.
func checkpointAge(nowEpochSeconds uint64, checkpointedAt string) string {
	parsed, ok := core.ParseChronoRFC3339(checkpointedAt)
	if !ok {
		return "unknown"
	}
	at := uint64(max(parsed.Unix(), 0))
	age := max(nowEpochSeconds, at) - at
	switch {
	case age < 60:
		return fmt.Sprintf("%ds", age)
	case age < 3_600:
		return fmt.Sprintf("%dm", age/60)
	case age < 86_400:
		return fmt.Sprintf("%dh", age/3_600)
	default:
		return fmt.Sprintf("%dd", age/86_400)
	}
}

// recoveryWarningName is name with a warning when the session's last
// checkpoint failed: how old its recovery copy is, or that it has none.
func recoveryWarningName(session *core.SessionRecord, name string, nowEpochSeconds uint64) string {
	if session.LastCheckpointError == nil {
		return name
	}
	warning := chat.ThemeGlyphs().Warning
	if session.Checkpoint != nil {
		return fmt.Sprintf("%s  %s Recovery copy %s old", name, warning,
			checkpointAge(nowEpochSeconds, session.Checkpoint.CreatedAt))
	}
	return fmt.Sprintf("%s  %s Recovery unavailable", name, warning)
}
