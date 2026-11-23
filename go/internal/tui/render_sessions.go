// Port of mj-tui/src/render/sessions.rs.
package tui

import (
	"fmt"
	"iter"
	"math"
	"slices"
	"strings"
	"time"
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
	width := layout.width
	nowEpochSeconds := uint64(max(time.Now().Unix(), 0))
	animationMs := chat.ElapsedMs()
	sessions := dashboard.orderedSessions()
	order := dashboard.sessionOrder()
	targets := order.targets()
	config := dashboard.config.Get()
	inRange := func(position int) bool { return position >= rangeStart && position < rangeEnd }
	var rows []DrawnSessionRow
	type heading struct {
		key  string
		line chat.Line
	}
	var pendingHeading *heading
	position := 0
	for _, row := range order.rows() {
		switch row := row.(type) {
		case SessionsRowProjectHeading:
			if !inRange(position) {
				continue
			}
			var number *int
			if options.showProjectNumbers {
				number = row.Number
			}
			hotkey := ""
			if number != nil {
				hotkey = fmt.Sprintf("[%d] ", *number)
			}
			// A heading is drawn inside the row beneath it, so the table's
			// selection index keeps counting sessions and nothing else.
			spans := []chat.Span{
				chat.StyledSpan(hotkey, chat.KeyHint()),
				chat.StyledSpan(row.Label, chat.Actionable().Bold()),
			}
			// A folded project hides its rows, so the heading says what is
			// waiting inside it. An unfolded one shows every symbol.
			if _, collapsed := (*dashboard.collapsedProjectKeys.Get())[row.Key]; collapsed {
				if badge := attentionBadge(dashboard.projectAttentionSummary(row.Key)); badge != nil {
					spans = append(spans, *badge)
				}
			}
			used := 0
			for _, span := range spans {
				used += span.Width()
			}
			if remaining := max(width-used, 0); remaining > 2 {
				spans = append(spans, chat.StyledSpan(" "+strings.Repeat(chat.ThemeGlyphs().Rule, remaining-1), chat.Border(false)))
			}
			pendingHeading = &heading{key: row.Key, line: chat.LineFrom(spans...)}
		case SessionsRowSession:
			index := row.Index
			presentation := layout.presentation(position, options.summaryOnly)
			visible := inRange(position)
			position++
			if !visible {
				pendingHeading = nil
				continue
			}
			if index < 0 || index >= len(sessions) {
				continue
			}
			session := sessions[index]
			displayName := sessionName(session)
			if dashboard.goMode != nil {
				displayName = dashboard.GoConversationTitle(session.ID)
			}
			detail := (*dashboard.sessionDetails.Get())[session.ID]
			reviews := dashboard.sessionReviews(session.ID)
			_, unreachable := (*dashboard.unreachableSessions.Get())[session.ID]
			// An unreachable session on a full disk says why.
			diskFull := false
			if unreachable {
				_, diskFull = dashboard.sessionStorageProblem(session.ID)
			}
			attention := dashboard.AttentionLevel(session.ID)
			facts := SessionRowFacts{
				detail:          detail,
				unreachable:     unreachable,
				state:           session.State,
				nowEpochSeconds: nowEpochSeconds,
				attention:       attention,
			}
			primaryBusy := dashboard.sessionIsWorking(session.ID)
			// Review work is independent of the session's primary
			// lifecycle. A review can keep animating while the session is
			// stopped or unreachable, so do not gate it on either.
			busy := primaryBusy || client.AnyWorking(sessionsReviewViews(reviews))
			var spinner *string
			if busy {
				frame := chat.CompactFrame(config.Spinner, animationMs)
				spinner = &frame
			}
			operation := (*dashboard.sessionOperations.Get())[session.ID]
			target := ""
			if index < len(targets) {
				target = targets[index]
			}
			var git *string
			if text, ok := dashboard.gitRowText(session.ID); ok {
				git = &text
			}
			permission := sessionPermissionBadge(session, operation, config)
			// The selection drives which conversation is on screen, so the
			// caret marks it in both forms.
			selectedID, hasSelection := dashboard.SelectedSessionID()
			selected := options.showSelection && hasSelection && selectedID == session.ID
			glyphs := chat.ThemeGlyphs()
			var symbol string
			if form, ok := presentation.form.(SessionRowFormTransition); ok && !form.Failed {
				switch form.Kind {
				case core.SessionTransitionKindStarting:
					symbol = glyphs.Starting
				case core.SessionTransitionKindResuming:
					symbol = glyphs.Resuming
				case core.SessionTransitionKindMoving:
					symbol = glyphs.Moving
				case core.SessionTransitionKindSuspending, core.SessionTransitionKindStopping:
					symbol = glyphs.Stopping
				case core.SessionTransitionKindDestroying:
					symbol = glyphs.Destroying
				}
			} else {
				symbol = facts.statusSymbol()
			}
			caret := "  "
			if selected {
				caret = glyphs.Selected
			}
			prefix := caret + symbol + " "
			var headingKey *string
			var lines []chat.Line
			if pendingHeading != nil && presentation.heading {
				key := pendingHeading.key
				headingKey = &key
				lines = append(lines, pendingHeading.line)
			}
			pendingHeading = nil
			// The minimized pane keeps one terse row per session, so it
			// omits CPU.
			var cpu *CpuShare
			if !options.summaryOnly {
				if share, ok := dashboard.cpuShare(session.ID); ok {
					cpu = &share
				}
			}
			var pin *uint32
			if id, ok := dashboard.PinID(session.ID); ok {
				pin = &id
			}
			titleWidth := sessionTitleWidth(width, pin)
			var waitingChild *core.SubagentKind
			if kind, ok := dashboard.waitingChildKind(session.ID); ok {
				waitingChild = &kind
			}
			switch form := presentation.form.(type) {
			case SessionRowFormRepair:
				errorStyle := chat.Style{}.Foreground(chat.ThemePalette().SessionError)
				lines = append(lines,
					chat.StyledLine(prefix+displayName, errorStyle),
					chat.StyledLine("  Needs config repair", errorStyle))
				if form.Hint {
					hint := "  Enter for repair details"
					if key, ok := dashboard.FirstKeyLabel(CommandIdOpenConfig); ok {
						hint = fmt.Sprintf("  Enter for repair details · %s settings", key)
					}
					lines = append(lines, chat.RawLine(hint))
				}
			case SessionRowFormTransition:
				if !form.Failed {
					lines = append(lines, sessionTransitionLine(prefix, session, displayName, form.Kind,
						operation, nowEpochSeconds, target, width, nil))
				} else {
					lines = append(lines, sessionTransitionLine(prefix, session, displayName, form.Kind,
						nil, nowEpochSeconds, target, width, session.LastError))
				}
			case SessionRowFormExpanded:
				expandedSessionLines(&lines, session, displayName, detail, reviews, unreachable, diskFull,
					attention, waitingChild, operation, nowEpochSeconds, target, cpu, permission, width,
					titleWidth, prefix, spinner, config.Advanced.DetailedActivityClocks, git)
			case SessionRowFormCompact:
				compactSessionLines(&lines, prefix, session, displayName, detail, reviews, unreachable,
					diskFull, attention, waitingChild, operation, nowEpochSeconds, target, cpu, permission,
					spinner, width, titleWidth)
			}
			if presentation.outsideFilter {
				lines = append(lines, chat.StyledLine("  Outside filter", chat.Muted()))
			}
			if selected {
				skip := 0
				if headingKey != nil {
					skip = 1
				}
				for i := skip; i < len(lines); i++ {
					lines[i].Style = chat.Selection(false).Patch(lines[i].Style)
				}
			}
			sessionIndex := index
			rows = append(rows, DrawnSessionRow{
				session: &sessionIndex,
				heading: headingKey,
				lines:   lines,
				height:  presentation.height(),
				spacing: presentation.spacing,
			})
		}
	}
	return rows
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
	// nameFloor: below this many cells an elided name says nothing worth
	// the room.
	const nameFloor = 9
	// tail is the characters of the name's tail that survive the elision.
	const tail = 4

	if chat.RawLine(git).Width() <= free {
		return git, true
	}
	// The marker and the branch name are the first two words; a branch name
	// never contains a space. Whatever follows is the counts, kept with the
	// space that separates them.
	marker, rest, found := strings.Cut(git, " ")
	if !found {
		return "", false
	}
	name, counts, hasCounts := rest, "", false
	if at := strings.Index(rest, " "); at >= 0 {
		name, counts, hasCounts = rest[:at], rest[at:], true
	}
	markerRoom := chat.RawLine(marker).Width() + 1
	elided := func(room int) (string, bool) {
		if room < nameFloor {
			return "", false
		}
		if chat.RawLine(name).Width() <= room {
			return name, true
		}
		// The last characters are what tell two generated names apart, so
		// they are kept and the truncated head carries the ellipsis.
		characters := []rune(name)
		tailText := string(characters[max(len(characters)-tail, 0):])
		head := truncateToCells(name, max(room-chat.RawLine(tailText).Width(), 0), TruncatePlain)
		return head + tailText, true
	}
	// The counts are worth more than the middle of the name, so they are
	// what the elision makes room for.
	if hasCounts {
		if fitted, ok := elided(max(free-(markerRoom+chat.RawLine(counts).Width()), 0)); ok {
			return marker + " " + fitted + counts, true
		}
	}
	if fitted, ok := elided(max(free-markerRoom, 0)); ok {
		return marker + " " + fitted, true
	}
	return "", false
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
	theme := chat.Current()
	ascii := chat.ASCII()
	if preview := c.cached; preview != nil &&
		preview.label == label &&
		preview.width == width &&
		preview.theme == theme &&
		preview.ascii == ascii &&
		preview.message == message {
		return slices.Clone(preview.rows)
	}
	c.renders++
	// An agent's message is summarized as one paragraph; the user's prompt
	// keeps its lines after the label.
	text := label + message
	if label == "" {
		text = strings.ReplaceAll(message, "\n", " ")
	}
	rows := chat.RenderAgentMessageHead(text, width, 2)
	c.cached = &OutputPreview{
		label:   label,
		message: message,
		width:   width,
		theme:   theme,
		ascii:   ascii,
		rows:    rows,
	}
	return slices.Clone(rows)
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
	style := chat.Style{}.Foreground(sessionBandColor(attention, detail, session.State)).Bold()
	name := recoveryWarningName(session, displayName, nowEpochSeconds)
	// The pin and ellipsis actions occupy the right of the first line only.
	// Keep the activity and output lines at the full content width so a
	// running clock and queued count remain readable in a compact pane.
	*lines = append(*lines,
		chat.LineFrom(sessionTitleSpans(prefix, name, style, git, cpu, titleWidth)...),
		sessionActivityLine("  ", session, detail, reviews, unreachable, diskFull, attention, waitingChild,
			operation, nowEpochSeconds, target, permission, spinner, width, detailedActivityClocks))

	label, message, hasMessage, muted := "", "", false, false
	if detail != nil {
		if excerpt, ok := currentAgentExcerpt(detail); ok {
			message, hasMessage = excerpt, true
		} else if detail.lastUserMessage != nil {
			label, message, hasMessage, muted = "You: ", *detail.lastUserMessage, true, true
		}
	}
	outputPrefix := "  " + chat.ThemeGlyphs().RoleGutter
	outputWidth := max(width-chat.RawLine(outputPrefix).Width(), 0)
	var output []chat.Line
	if detail != nil && hasMessage {
		output = detail.outputPreview.rows(label, message, outputWidth)
	}
	if len(output) == 0 {
		output = append(output, chat.RawLine("No messages yet"))
	}
	for len(output) < 2 {
		output = append(output, chat.Line{})
	}
	for _, line := range output[:2] {
		spans := append([]chat.Span{chat.StyledSpan(outputPrefix, chat.Border(false))}, line.Spans...)
		row := chat.LineFrom(spans...)
		if muted {
			row.Style = chat.Muted()
		} else {
			row.Style = chat.Style{}.Foreground(chat.ThemePalette().Text)
		}
		*lines = append(*lines, row)
	}
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
	prefixWidth := chat.RawLine(prefix).Width()
	nameRoom := max(titleWidth-prefixWidth, 0)
	cpuText, hasCPU := cpuRowLabel(cpu)
	if hasCPU {
		cpuWidth := chat.RawLine(cpuText).Width() + 1
		nameWidth := chat.RawLine(name).Width()
		hasCPU = max(nameRoom-cpuWidth, 0) >= min(nameWidth, minNameCellsWithCPU)
	}
	cpuWidth := 0
	if hasCPU {
		cpuWidth = chat.RawLine(cpuText).Width() + 1
	}
	title := truncateToCells(name, max(nameRoom-cpuWidth, 0), TruncatePlain)
	titleCells := chat.RawLine(title).Width()
	spans := []chat.Span{chat.StyledSpan(prefix+title, style)}
	used := prefixWidth + titleCells
	// The branch follows the name when the line has room before the CPU.
	if branch != nil {
		if text, ok := fitGitRowText(*branch, max(max(titleWidth-(used+cpuWidth), 0)-2, 0)); ok {
			text = "  " + text
			used += chat.RawLine(text).Width()
			spans = append(spans, chat.StyledSpan(text, chat.Muted()))
		}
	}
	if hasCPU {
		pad := max(titleWidth-(used+chat.RawLine(cpuText).Width()), 0)
		spans = append(spans, chat.RawSpan(strings.Repeat(" ", pad)), chat.StyledSpan(cpuText, chat.Muted()))
	}
	return spans
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
	facts := SessionRowFacts{
		detail:          detail,
		unreachable:     unreachable,
		state:           session.State,
		nowEpochSeconds: nowEpochSeconds,
		attention:       attention,
	}
	lastKnownActivity := func(kind core.ActivityStateKind) bool {
		if detail == nil {
			return false
		}
		state := detail.activity.State()
		return state.LastKnownState().Kind == kind
	}
	var status string
	switch {
	case operation != nil:
		label, startedAt := operationStatus(operation)
		status = label + " " + client.FormatClock(saturatingSubUint64(nowEpochSeconds, startedAt))
	case facts.state == core.SessionStateError:
		status = "Error"
	case facts.state == core.SessionStateParked:
		// A sub-agent whose turn ended: idle, with its processes stopped
		// until its parent sends it more input.
		status = "Parked"
	case session.State == core.SessionStateProvisioning:
		startedAt, ok := sessionUpdatedAtEpochSeconds(session)
		if !ok {
			startedAt = nowEpochSeconds
		}
		status = "Launch " + client.FormatClock(saturatingSubUint64(nowEpochSeconds, startedAt))
	case facts.unreachable && diskFull:
		status = "Disk full"
	case facts.unreachable:
		status = "Unreachable"
	case lastKnownActivity(core.ActivityStateCheckingQuota):
		status = "Checking quota"
	case facts.needsInput() && waitingChild == nil:
		status = "Question"
	default:
		if label, ok := reviewStatusLabel(reviews); ok {
			status = label
			break
		}
		switch {
		case attention == AttentionLevelWaiting:
			// Waiting with no question or review of its own: one of its
			// Mjolnir children holds an open form (see
			// DashboardState.AttentionLevel). Reviewers live in their own
			// workspace, so the row says which.
			if waitingChild != nil && *waitingChild == core.SubagentKindReviewer {
				status = "Reviewer question"
			} else {
				status = "Sub-agent question"
			}
		case lastKnownActivity(core.ActivityStateExpecting):
			status = "expecting the agent to continue"
		case facts.state.IsActive():
			status = facts.clock(detailedActivityClocks)
		default:
			status = facts.state.GoString()
		}
	}
	var queue *string
	if detail != nil && len(detail.queuedPrompts) > 0 {
		count := len(detail.queuedPrompts)
		text := fmt.Sprintf("[Q %d]", count)
		if width <= 24 {
			text = fmt.Sprintf("Q%d", count)
		}
		queue = &text
	}
	// At the minimum sidebar width, retain both the attention marker and the
	// queue count while using a compact status word.
	if width <= 24 && status == "Unreachable" {
		status = "Offline"
	}
	available := width
	// A minimized cell's second line is reserved for activity and its
	// queue. Keep it two cells in from the pane edge so the summary reads as
	// a continuation of the title line while retaining enough room for
	// status.
	compact := width <= 24
	if compact {
		prefix = "  "
	}
	queueWidth := 0
	if queue != nil {
		queueWidth = chat.RawLine(*queue).Width() + 1
	}
	prefixWidth := chat.RawLine(prefix).Width()
	if spinner != nil && prefixWidth+chat.RawLine(*spinner).Width()+1+chat.RawLine(status).Width()+queueWidth > available {
		spinner = nil
	}
	spinnerWidth := 0
	if spinner != nil {
		spinnerWidth = chat.RawLine(*spinner).Width() + 1
	}
	status = truncateToCells(status, max(available-(prefixWidth+spinnerWidth+queueWidth), 0), TruncatePlain)
	statusWidth := chat.RawLine(status).Width() + queueWidth + 2
	identityWidth := 0
	if !compact {
		identityWidth = max(available-(prefixWidth+spinnerWidth+statusWidth), 0)
	}
	badgeText := ""
	if permission != nil {
		badgeText = " " + permission.Content
	}
	badgeWidth := chat.RawLine(badgeText).Width()
	showPermission := badgeWidth > 0 && identityWidth >= badgeWidth+1
	if showPermission {
		identityWidth = max(identityWidth-badgeWidth, 0)
	}
	profile := session.LastProfile
	if operation != nil && operation.resumeDestination != nil {
		profile = operation.resumeDestination.ProfileID
	}
	targetWidth, profileWidth := chat.RawLine(target).Width(), chat.RawLine(profile).Width()
	if targetWidth+2+profileWidth > identityWidth {
		half := max(identityWidth-2, 0) / 2
		targetWidth, profileWidth = half, max(max(identityWidth-2, 0)-half, 0)
	}
	target = truncateToCells(target, targetWidth, TruncatePlain)
	profile = truncateToCells(profile, profileWidth, TruncatePlain)
	var identity string
	switch {
	case target == "" && profile == "":
	case target == "":
		identity = profile
	case profile == "":
		identity = target
	default:
		identity = target + "  " + profile
	}
	spans := []chat.Span{chat.RawSpan(prefix)}
	if spinner != nil {
		spans = append(spans, chat.StyledSpan(*spinner+" ", facts.style()))
	}
	statusStyle := facts.style()
	if facts.needsInput() {
		statusStyle = statusStyle.Bold()
	}
	spans = append(spans, chat.StyledSpan(status, statusStyle))
	if queue != nil {
		spans = append(spans, chat.StyledSpan(" "+*queue, facts.style().Bold()))
	}
	if identity != "" || showPermission {
		spans = append(spans, chat.StyledSpan("  "+identity, chat.Muted()))
	}
	if showPermission {
		spans = append(spans, chat.RawSpan(" "), *permission)
	}
	return chat.LineFrom(spans...)
}

// saturatingSubUint64 is u64::saturating_sub.
func saturatingSubUint64(a, b uint64) uint64 {
	if b > a {
		return 0
	}
	return a - b
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
	facts := SessionRowFacts{
		detail:          detail,
		unreachable:     unreachable,
		state:           session.State,
		nowEpochSeconds: nowEpochSeconds,
		attention:       attention,
	}
	style := facts.style().Bold()
	name := recoveryWarningName(session, displayName, nowEpochSeconds)
	// The pin and ellipsis actions occupy the right of the first line only;
	// retain the full width for the status line below it.
	*lines = append(*lines,
		chat.LineFrom(sessionTitleSpans(prefix, name, style, nil, cpu, titleWidth)...),
		sessionActivityLine("  ", session, detail, reviews, unreachable, diskFull, attention, waitingChild,
			operation, nowEpochSeconds, target, permission, spinner, width, false))
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
	glyphs := chat.ThemeGlyphs()
	// The same scale the attention queue and the badges read, so a row can
	// never show a symbol the queue disagrees with.
	switch f.attention {
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
	}
	// What is left is a session that wants nothing: name the lifecycle
	// state it is in, or say how idle it is.
	switch f.state {
	case core.SessionStateStopped:
		return glyphs.Stopped
	case core.SessionStateProvisioning:
		return glyphs.Starting
	case core.SessionStateCheckpointing:
		return glyphs.Checkpointing
	case core.SessionStateClosing, core.SessionStateStartupCleanup:
		return glyphs.Stopping
	case core.SessionStateDestroying:
		return glyphs.Destroying
	case core.SessionStateParked:
		return glyphs.Idle
	default:
		if detail := f.detail; detail != nil &&
			(detail.materializedAppliedEventOrdinal != nil ||
				detail.activity.Execution != nil ||
				detail.activity.IdleSinceMs != nil) {
			return glyphs.Idle
		}
		return glyphs.Unknown
	}
}

func (f SessionRowFacts) style() chat.Style {
	return chat.Style{}.Foreground(sessionBandColor(f.attention, f.detail, f.state))
}

func (f SessionRowFacts) clock(detailed bool) string {
	activity := &emptyActivity
	var currentTurnStartedAt, currentStepStartedAtMs *uint64
	if f.detail != nil {
		activity = &f.detail.activity
		currentTurnStartedAt = f.detail.currentTurnStartedAt
		currentStepStartedAtMs = f.detail.currentStepStartedAtMs
	}
	return activity.DisplayClock(f.nowEpochSeconds, currentTurnStartedAt, currentStepStartedAtMs, detailed)
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
	var startedAt uint64
	hasStart := false
	switch {
	case operation != nil:
		_, startedAt = operationStatus(operation)
		hasStart = true
	case dashboard.TransitionKind(session.ID) != nil ||
		dashboard.TransitionFailureKind(session.ID) != nil ||
		session.State == core.SessionStateProvisioning:
		startedAt, hasStart = sessionUpdatedAtEpochSeconds(session)
	}
	if hasStart {
		return client.FormatClock(saturatingSubUint64(nowEpochSeconds, startedAt)), true
	}

	if _, reviewing := client.CombinedActivityLabel(sessionsReviewViews(reviews)); session.LastError != nil ||
		unreachable ||
		dashboard.AttentionLevel(session.ID).NeedsPerson() ||
		reviewing ||
		!session.State.IsActive() {
		return "", false
	}
	if detail == nil {
		return "", false
	}
	if detail.activity.IsIdle(detail.currentTurnStartedAt) ||
		(detail.activity.Execution != nil &&
			(*detail.activity.Execution == core.RelayExecutionStateClosing ||
				*detail.activity.Execution == core.RelayExecutionStateClosed)) {
		return "", false
	}
	detailed := dashboard.PaneSize(SupportPaneSessions) != core.PaneSizeMinimized &&
		dashboard.ProjectIsExpanded(session) &&
		dashboard.config.Get().Advanced.DetailedActivityClocks
	return SessionRowFacts{
		detail:          detail,
		unreachable:     unreachable,
		state:           session.State,
		nowEpochSeconds: nowEpochSeconds,
		attention:       dashboard.AttentionLevel(session.ID),
	}.clock(detailed), true
}

// currentAgentExcerpt selects only content authored by the agent for the
// expanded output rows. The user prompt is a fallback for the compact
// summary, never an agent excerpt with a misleading prefix.
func currentAgentExcerpt(detail *SessionDetail) (string, bool) {
	// A question waiting for an answer is what the session is doing now;
	// the tool call that raised it only carries the harness's internal tool
	// name.
	if len(detail.pendingElicitations) > 0 {
		return detail.pendingElicitations[0].Message, true
	}
	if detail.lastAgentMessageFollowsLastUser && detail.lastAgentMessage != nil {
		return *detail.lastAgentMessage, true
	}
	if detail.latestAgentActivityAfterLastUser != nil {
		return *detail.latestAgentActivityAfterLastUser, true
	}
	return "", false
}

// sessionDisplayTargets is the target label shown for each of sessions,
// given each one's project key in projects.
//
// A target repeated inside one project is ambiguous on its own, so repeats
// are numbered `[1]`, `[2]`, … in the order they appear. The Sessions order
// keeps these labels for ordered_sessions(), and every Sessions
// representation reads them from there so labels remain consistent.
func sessionDisplayTargets(dashboard *DashboardState, sessions []*core.SessionRecord, projects []string) []string {
	type projectTarget struct{ project, target string }
	count := min(len(sessions), len(projects))
	keys := make([]projectTarget, count)
	for i := range count {
		keys[i] = projectTarget{
			project: projects[i],
			target: sessionTargetLabel(dashboard.state.Get(), sessions[i],
				(*dashboard.sessionOperations.Get())[sessions[i].ID], dashboard.config.Get()),
		}
	}
	counts := map[projectTarget]int{}
	for _, key := range keys {
		counts[key]++
	}
	occurrences := map[projectTarget]int{}
	labels := make([]string, count)
	for i, key := range keys {
		occurrences[key]++
		if counts[key] > 1 {
			labels[i] = fmt.Sprintf("%s [%d]", key.target, occurrences[key])
		} else {
			labels[i] = key.target
		}
	}
	return labels
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
	var form SessionRowForm
	if _, issue := session.ConfigurationIssue(dashboard.config.Get()); issue {
		form = SessionRowFormRepair{Hint: expanded && !minimized}
	} else if kind := dashboard.TransitionKind(session.ID); kind != nil {
		form = SessionRowFormTransition{Kind: *kind, Failed: false}
	} else if kind := dashboard.TransitionFailureKind(session.ID); kind != nil {
		form = SessionRowFormTransition{Kind: *kind, Failed: true}
	} else if expanded && !minimized {
		form = SessionRowFormExpanded{}
	} else {
		form = SessionRowFormCompact{}
	}
	spacing := 0
	if expanded && !minimized {
		spacing = 1
	}
	return RowPresentation{
		form:          form,
		heading:       heading,
		outsideFilter: dashboard.sessionOutsideFilter(session),
		spacing:       spacing,
	}
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
	sessions := dashboard.orderedSessions()
	var shapes []RowShape
	heading := false
	for _, row := range dashboard.sessionOrder().rows() {
		switch row := row.(type) {
		case SessionsRowProjectHeading:
			if len(shapes) > 0 {
				last := &shapes[len(shapes)-1]
				last.full.spacing = 1
				last.minimized.spacing = 1
			}
			heading = true
		case SessionsRowSession:
			session := sessions[row.Index]
			shapes = append(shapes, RowShape{
				full:      rowPresentation(dashboard, session, row.Expanded, false, heading),
				minimized: rowPresentation(dashboard, session, row.Expanded, true, heading),
			})
			heading = false
		}
	}
	if len(shapes) > 0 {
		last := &shapes[len(shapes)-1]
		last.full.spacing = 0
		last.minimized.spacing = 0
	}
	minimizedHeights := make([]int, len(shapes))
	fullHeights := make([]int, len(shapes))
	for i, shape := range shapes {
		minimizedHeights[i] = shape.minimized.height() + shape.minimized.spacing
		fullHeights[i] = shape.full.height() + shape.full.spacing
	}
	return SessionsLayout{
		width:            width,
		shapes:           shapes,
		minimizedHeights: minimizedHeights,
		fullHeights:      fullHeights,
	}
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
	budget := paneTitleContentWidth(width, maximizeEnabled)
	if workspaceName == "" {
		// Padded like every other pane title when it fits; a narrow
		// minimized pane spends its few cells on letters instead.
		title := " Sessions "
		if len(title) > budget {
			title = truncateToCells("Sessions", budget, TruncateSummary)
		}
		return SessionsTitle{line: chat.RawLine(title)}
	}
	prefix := " S · "
	if labelKeepsFullPrefix(workspaceName, width, maximizeEnabled, clear) {
		prefix = fullTitlePrefix
	}
	end := titleEnd(clear)
	workspaceRoom := max(budget-(utf8.RuneCountInString(prefix)+utf8.RuneCountInString(end)), 0)
	label := chat.StyledSpan(truncateToCells(workspaceName, workspaceRoom, TruncateSummary),
		chat.Style{}.Foreground(chat.ThemePalette().Muted))
	chipStart := chat.RawSpan(prefix).Width() + label.Width()
	var clearChip *int
	if clear && chipStart+chat.RawSpan(end).Width() <= budget && chipStart <= math.MaxUint16 {
		clearChip = &chipStart
	}
	endSpan := chat.RawSpan(end)
	if clear {
		endSpan = chat.StyledSpan(end, chat.Muted())
	}
	return SessionsTitle{
		line:      chat.LineFrom(chat.RawSpan(prefix), label, endSpan),
		clearChip: clearChip,
	}
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
	base := sessionsTitle(workspaceName, width, maximizeEnabled, clear)
	if !hasBadge {
		return base
	}
	budget := paneTitleContentWidth(width, maximizeEnabled)
	style := chat.Style{}.Foreground(attentionColor(badgeLevel)).Bold()
	glyph := attentionGlyph(badgeLevel)
	suffix := chat.StyledSpan(fmt.Sprintf(" · Attention: %d", badgeCount), style)
	if base.line.Width()+suffix.Width() <= budget {
		spans := append(slices.Clone(base.line.Spans), suffix)
		return SessionsTitle{line: chat.LineFrom(spans...), clearChip: base.clearChip}
	}
	// The compact forms lead with the count. A filter still says it is on:
	// the label follows in whatever room is left, and the clear chip always
	// ends the title, so the label is what gives way first.
	compactForms := []string{
		fmt.Sprintf(" Sessions [%s%d]", glyph, badgeCount),
		fmt.Sprintf(" %s%d", glyph, badgeCount),
	}
	for _, form := range compactForms {
		lead := chat.StyledSpan(form, style)
		if !clear {
			if lead.Width() <= budget {
				return SessionsTitle{line: chat.LineFrom(lead)}
			}
			continue
		}
		end := titleEnd(true)
		endWidth := chat.RawSpan(end).Width()
		if lead.Width()+endWidth > budget {
			continue
		}
		room := budget - lead.Width() - endWidth
		spans := []chat.Span{lead}
		// A separating space, then at least one cell of label.
		if room >= 2 && workspaceName != "" {
			spans = append(spans, chat.StyledSpan(
				" "+truncateToCells(workspaceName, room-1, TruncateSummary),
				chat.Style{}.Foreground(chat.ThemePalette().Muted)))
		}
		chipStart := 0
		for _, span := range spans {
			chipStart += span.Width()
		}
		spans = append(spans, chat.StyledSpan(end, chat.Muted()))
		var clearChip *int
		if chipStart <= math.MaxUint16 {
			clearChip = &chipStart
		}
		return SessionsTitle{line: chat.LineFrom(spans...), clearChip: clearChip}
	}
	return base
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
	content := chat.RectInner(area, chat.Margin{Horizontal: 1, Vertical: 1})
	// Keep three cells at the right edge clear for the session ellipsis
	// control on each session's title line. The activity and output lines
	// use the full content width; a narrow sidebar must not hide their clock
	// or queued count behind the action button.
	actionsArea := uv.Rect(content.Min.X, content.Min.Y, content.Dx(), min(content.Dy(), sessionActionsHeight))
	syncLine, hasSyncLine := dashboard.sessionsSyncLine(content.Dx())
	headerHeight := sessionActionsHeight
	if hasSyncLine {
		headerHeight++
	}
	rowsArea := uv.Rect(content.Min.X, content.Min.Y+headerHeight, content.Dx(), max(content.Dy()-headerHeight, 0))
	// Rust debug-asserts here that layout.width == content.Dx(): the
	// Sessions layout must be made for this width.
	minimized := dashboard.SessionsMinimized()
	rowHeights := layout.heights(minimized)
	focused := dashboard.Focus() == FocusSessions
	maximizeEnabled := dashboard.paneMaximizeEnabled(SupportPaneSessions)
	filterLabel := dashboard.sessionsFilterLabel()
	// While a filter is in force, the title ends with a chip that clears it.
	clear := *dashboard.sessionsFilter.Get() != nil
	// The count is the first thing to give up its place. Below the width
	// that keeps both, the pane name is worth more than the number.
	if hidden := dashboard.sessionsHiddenCount(); hidden != 0 {
		// The count leads, so the label's tail is what a narrow title cuts.
		counted := fmt.Sprintf("%d hidden", hidden)
		if filterLabel != "" {
			counted = fmt.Sprintf("%d hidden · %s", hidden, filterLabel)
		}
		if labelKeepsFullPrefix(counted, area.Dx(), maximizeEnabled, clear) {
			filterLabel = counted
		}
	}
	badgeLevel, badgeCount, hasBadge := dashboard.sessionsAttentionSummary()
	block, clearChip := sessionsBlock(focused, filterLabel, area.Dx(), dashboard.PaneSize(SupportPaneSessions),
		badgeLevel, badgeCount, hasBadge, maximizeEnabled, clear)
	frame.RenderWidget(block, area)
	if clearChip != nil {
		// The title starts one cell in, after the border's corner.
		chipWidth := min(chat.RawLine(chat.ThemeGlyphs().Close).Width(), math.MaxUint16)
		renderSessionsFilterClear(frame, uv.Rect(area.Min.X+1+*clearChip, area.Min.Y, chipWidth, min(area.Dy(), 1)), dashboard)
	}
	renderSessionButtons(frame, actionsArea, dashboard)
	if hasSyncLine {
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(syncLine)), uv.Rect(
			content.Min.X,
			content.Min.Y+sessionActionsHeight,
			content.Dx(),
			min(max(content.Dy()-sessionActionsHeight, 0), 1)))
	}
	selected, hasSelected := dashboard.selectedVisibleIndex()
	hasSelected = hasSelected && selected < len(rowHeights)
	viewport := rowsArea.Dy()
	// When the last row is taller than the pane, no page fits it and the
	// clamp allows an offset past it. The list always keeps its last row.
	offset := min(chat.ClampOffsetToLastPage(dashboard.sessionsScroll, rowHeights, viewport), max(len(rowHeights)-1, 0))
	// A moved selection, or none at all, ends the wheel's hold on the
	// offset.
	selectedID, hasSelectedID := dashboard.SelectedSessionID()
	if held := dashboard.sessionsWheelScrolledFrom; !hasSelected || held == nil || !hasSelectedID || *held != selectedID {
		dashboard.sessionsWheelScrolledFrom = nil
	}
	wheelScrolled := dashboard.sessionsWheelScrolledFrom != nil
	revealSelected := true
	if hasSelected {
		contentHeight := layout.presentation(selected, minimized).height()
		switch {
		case takeSelectionRecenter(dashboard, FocusSessions):
			offset = chat.CenteredOffset(selected, rowHeights, viewport)
		case wheelScrolled:
			// The wheel moved the list, not the selection, so the selected
			// row may be off screen. The table below must not scroll to it
			// either: that would put its rows out of step with the hit
			// areas, so it only marks a selection that already fits, whole,
			// where it is.
			revealSelected = false
			if selected >= offset {
				above := 0
				for _, height := range rowHeights[offset:selected] {
					above += height
				}
				revealSelected = above+contentHeight <= viewport
			}
		case selected < offset:
			offset = selected
		default:
			offset = offsetShowingSelected(offset, selected, contentHeight, rowHeights, viewport)
		}
	}
	end, used := offset, 0
	for end < len(rowHeights) && used < viewport {
		used += rowHeights[end]
		end++
	}
	options := sessionRowsRenderOptionsDashboard
	if minimized {
		options = sessionRowsRenderOptionsMinimized
	}
	drawn := drawnSessionRowsInRange(dashboard, layout, options, offset, end)
	tableRows := make([]TableRow, len(drawn))
	for i := range drawn {
		row := &drawn[i]
		tableRows[i] = NewTableRow(NewTableCell(chat.TextFrom(slices.Clone(row.lines)...))).
			WithHeight(row.contentHeight()).
			WithBottomMargin(row.spacing)
	}
	table := NewTable(tableRows, []chat.Constraint{chat.ConstraintMin(1)})
	var tableSelected *int
	if hasSelected && revealSelected && selected < end && selected >= offset {
		index := selected - offset
		tableSelected = &index
	}
	state := TableState{}.WithSelected(tableSelected)
	table.RenderStateful(rowsArea, frame.Buffer(), &state)
	if len(drawn) == 0 && rowsArea.Dy() > 0 {
		var lines []chat.Line
		switch {
		case dashboard.sessionsText.Get().isPending():
			lines = []chat.Line{chat.RawLine("Searching conversations…")}
		case clear:
			lines = []chat.Line{chat.RawLine("No sessions match · Esc clears the filter")}
		default:
			lines = []chat.Line{
				chat.StyledLine("No sessions yet", chat.Title(false)),
				chat.RawLine("Create new work or open a session."),
			}
		}
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(lines...)).WithStyle(chat.Muted()).WithWrap(chat.Wrap{Trim: true}), rowsArea)
	}
	// The table scrolled only as far as it had to; remember where it
	// settled so the next frame does not scroll back to the top.
	dashboard.sessionsScroll = offset

	rowY := rowsArea.Min.Y
	visible := 0
	var rendered SessionRowsRendered
	for i := range drawn {
		row := &drawn[i]
		if rowY >= rowsArea.Max.Y {
			break
		}
		visible++
		headingRows := 0
		if row.heading != nil {
			headingRows = 1
			rendered.projectHeadingAreas = append(rendered.projectHeadingAreas, client.DaemonPair[string, uv.Rectangle]{
				First:  *row.heading,
				Second: uv.Rect(rowsArea.Min.X, rowY, rowsArea.Dx(), 1),
			})
		}
		if row.session != nil {
			sessionY := rowY + headingRows
			height := max(row.contentHeight()-headingRows, 0)
			rendered.sessionRowAreas = append(rendered.sessionRowAreas, client.DaemonPair[int, uv.Rectangle]{
				First:  *row.session,
				Second: uv.Rect(rowsArea.Min.X, sessionY, rowsArea.Dx(), min(height, max(rowsArea.Max.Y-sessionY, 0))),
			})
		}
		rowY += row.contentHeight() + row.spacing
	}
	renderSessionScrollbar(frame, area, len(rowHeights), offset, visible)
	return rendered
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
	startedAt := nowEpochSeconds
	if operation != nil {
		startedAt = operation.startedAtEpochSeconds
		for i, stage := range operation.activeStages {
			if i == 0 || stage.StartedAt < startedAt {
				startedAt = stage.StartedAt
			}
		}
	} else if updated, ok := sessionUpdatedAtEpochSeconds(session); ok {
		startedAt = updated
	}
	stages := "waiting"
	if operation != nil && len(operation.activeStages) > 0 {
		labels := make([]string, len(operation.activeStages))
		for i, stage := range operation.activeStages {
			labels[i] = stage.Stage.Label()
		}
		if joined := strings.Join(labels, ", "); joined != "" {
			stages = joined
		}
	}
	elapsed := client.FormatClock(saturatingSubUint64(nowEpochSeconds, startedAt))
	profile := session.LastProfile
	if operation != nil && operation.resumeDestination != nil {
		profile = operation.resumeDestination.ProfileID
	}
	// Lead with the name, as every other session row does. A transition row
	// is a single line, so anything after it is what truncation drops first;
	// sessions starting together on one target differ only by their names.
	// Keep the name to half the row so the stage and clock always survive.
	contentWidth := max(max(width-3, 0)-utf8.RuneCountInString(prefix), 0)
	name := truncateToCells(displayName, max(contentWidth/2, 12), TruncatePlain)
	failed := ""
	if failure != nil {
		failed = " · failed: " + *failure
	}
	line := fmt.Sprintf("%s%s  %s · %s · %s  %s · %s%s",
		prefix, name, transition.Label(), stages, elapsed, target, profile, failed)
	color := chat.ThemePalette().SessionActivity
	if failure != nil {
		color = chat.ThemePalette().SessionError
	}
	return chat.StyledLine(
		truncateToCells(line, max(width-3, 0), TruncateSummary),
		chat.Style{}.Foreground(color).Bold())
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
	return state.ProjectIdentitySession(session).ProjectTarget(config, sessionDestinationTarget(session, operation))
}

// sessionDestinationTarget is the target a session runs on, or the one an
// in-flight resume is moving it to.
func sessionDestinationTarget(session *core.SessionRecord, operation *SessionOperationDisplay) string {
	if operation != nil && operation.resumeDestination != nil {
		return operation.resumeDestination.TargetTemplateID
	}
	return session.TargetTemplateID
}

// sessionPermissionBadge is the permission badge of the target the session
// runs on, or is moving to.
func sessionPermissionBadge(session *core.SessionRecord, operation *SessionOperationDisplay, config *core.Config) *chat.Span {
	target, ok := config.Targets[sessionDestinationTarget(session, operation)]
	if !ok {
		return nil
	}
	return targetPermissionBadge(target)
}

// targetPermissionBadge is the badge for a configured target's permission
// mode.
func targetPermissionBadge(target core.TargetTemplate) *chat.Span {
	if mode, ok := target.PermissionMode(); ok {
		return permissionBadge(&mode)
	}
	return nil
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
	var listed []string
	for _, id := range targetIDs {
		if !dashboard.defaultEngineMissing(id) {
			listed = append(listed, id)
		}
	}
	return listed
}

// capacityTargetLabels is the Targets cell of a capacity row: the listed
// targets, each with its permission badge or marked unavailable.
func capacityTargetLabels(targetIDs []string, dashboard *DashboardState) chat.Line {
	config := dashboard.config.Get()
	var spans []chat.Span
	for index, targetID := range listedCapacityTargets(targetIDs, dashboard) {
		if index > 0 {
			spans = append(spans, chat.RawSpan(", "))
		}
		if _, missing := dashboard.targetMissingLine(targetID); missing {
			spans = append(spans, chat.StyledSpan(targetID+" (unavailable)",
				chat.Style{}.Foreground(chat.ThemePalette().Muted)))
			continue
		}
		spans = append(spans, chat.RawSpan(targetID))
		if target, ok := config.Targets[targetID]; ok {
			if badge := targetPermissionBadge(target); badge != nil {
				spans = append(spans, chat.RawSpan(" "), *badge)
			}
		}
	}
	return chat.LineFrom(spans...)
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
	switch operation.kind {
	case SessionOperationKindLaunching, SessionOperationKindResuming, SessionOperationKindMoving:
		if len(operation.activeStages) > 0 {
			labels := make([]string, len(operation.activeStages))
			startedAt := operation.activeStages[0].StartedAt
			for i, stage := range operation.activeStages {
				labels[i] = stage.Stage.Label()
				startedAt = min(startedAt, stage.StartedAt)
			}
			return strings.Join(labels, ", "), startedAt
		}
	}
	return operation.kind.Label(), operation.startedAtEpochSeconds
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
	if level == AttentionLevelIdle {
		// Without a detail projection there is nothing to call idle yet.
		if state == core.SessionStateRunning && detail != nil && detail.activity.IsIdle(detail.currentTurnStartedAt) {
			return chat.ThemePalette().SessionIdle
		}
		return chat.ThemePalette().SessionActivity
	}
	return attentionColor(level)
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
