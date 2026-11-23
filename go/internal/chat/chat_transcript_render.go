// Port of mj-chat/src/chat/transcript/render.rs.

package chat

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// renderTranscript draws the conversation pane.
//
// titleControls is the number of columns the host reserves at the right of
// the title row for its own chips; the title stops short of them. titleLead
// is the number of columns the host draws its own label into at the left of
// the title row; the title starts after them. paneFocused draws the border in
// the focused style, which is how a host with several panes shows which one
// has the keyboard.
func renderTranscript(frame *Frame, area uv.Rectangle, chat *ChatState, gestureActive bool, titleControls int, titleLead int, paneFocused bool) {
	renderStarted := time.Now()
	viewportHeight := max(area.Dy()-2, 0)
	chat.lastViewportHeight = viewportHeight
	block := Panel(paneFocused).
		WithStyle(Base()).
		WithPadding(Padding{Left: 1, Right: 1})
	inner := block.Inner(area)
	contentWidth := inner.Dx()
	window := chat.viewport(contentWidth, viewportHeight)
	// The window resolves and clamps the anchor: an anchor inside the last
	// screenful snaps back to following the tail.
	chat.anchor = window.anchor
	atTail := anchorIsBottom(window.anchor)
	top := window.top
	title := transcriptTitle(chat, core.EpochSeconds())
	if titleLead > 0 {
		title.Spans = slices.Insert(title.Spans, 0, RawSpan(strings.Repeat(" ", titleLead)))
	}
	block = block.WithTitle(TruncateLineToWidth(
		title,
		max(max(area.Dx()-2, 0)-titleControls, 0),
	))
	frame.RenderWidget(block, area)
	visible := window.rows[:min(len(window.rows), inner.Dy())]
	visibleRows := len(visible)
	frame.RenderWidget(
		NewParagraph(TextFrom(visible...)).WithStyle(Style{}.Foreground(ThemePalette().Text)),
		inner,
	)
	for _, render := range chat.submissionRenders {
		slog.Debug("authoritative submission frame prepared",
			"target", "mj_chat::latency",
			"command_id", render.commandID,
			"render_ms", float64(time.Since(renderStarted))/float64(time.Millisecond),
			"elapsed_ms", float64(time.Since(render.at))/float64(time.Millisecond))
	}
	chat.submissionRenders = chat.submissionRenders[:0]
	if _, restoring := chat.anchor.(TranscriptAnchorRestoring); restoring {
		chat.transcriptClickTargets = chat.transcriptClickTargets[:0]
		chat.transcriptLinkFrame = nil
		chat.transcriptSelection = nil
		chat.transcriptSelectionInvalid = true
		chat.transcriptScrollbar.clearGeometry()
		return
	}
	chat.rebuildTranscriptClickTargets(inner, top, visibleRows)
	chat.recordTranscriptLinkFrame(inner, top, visibleRows)
	chat.registerTranscriptSurface(inner, top, visibleRows, atTail, gestureActive)
	trackWidth := 0
	if inner.Dx() > 0 {
		trackWidth = 1
	}
	track := uv.Rect(inner.Max.X, inner.Min.Y, trackWidth, inner.Dy())
	if geometry, ok := chat.updateTranscriptScrollbar(
		track,
		contentWidth,
		viewportHeight,
		top,
		chat.anchor,
	); ok {
		marks := make([]int, 0, len(chat.transcriptScrollbar.marks))
		for _, mark := range chat.transcriptScrollbar.marks {
			marks = append(marks, mark.trackRow)
		}
		RenderMarkedScrollbar(
			frame,
			geometry,
			marks,
			Style{}.Foreground(ThemePalette().Secondary),
		)
	}
	if chat.renderMode == client.TranscriptRenderModeRaw {
		chat.queueVisibleRawToolDetails(top, visibleRows)
	}
}

func transcriptTitle(chat *ChatState, nowEpochSeconds uint64) Line {
	separator := ThemeGlyphs().FooterSeparator
	reviewActivity, hasReviewActivity := client.CombinedActivityLabel(chat.reviewViews())
	var summary string
	switch {
	case chat.headerTarget == "" || chat.headerProfile == "":
		if hasReviewActivity {
			summary = "Conversation" + separator + reviewActivity
		} else {
			summary = "Conversation"
		}
	case hasReviewActivity:
		columns := []string{chat.headerTarget}
		if len(chat.queuedPrompts) > 0 {
			columns = append(columns, fmt.Sprintf("[Q %d]", len(chat.queuedPrompts)))
		}
		columns = append(columns, "["+reviewActivity+"]", chat.headerProfile)
		summary = strings.Join(columns, "  ")
	default:
		columns := []string{chat.headerTarget}
		if len(chat.queuedPrompts) > 0 {
			columns = append(columns, fmt.Sprintf("[Q %d]", len(chat.queuedPrompts)))
		}
		var activity string
		switch {
		case !chat.activityReachable:
			activity = "Unreachable"
		case chat.phase == core.WorkerPhaseClosed:
			activity = "Agent ended"
		case chat.phase == core.WorkerPhaseClosing:
			activity = "Agent shutdown"
		case len(chat.pendingElicitations) > 0:
			activity = "Question"
		default:
			activity = chat.sessionActivity.DisplayClock(
				nowEpochSeconds,
				chat.turnStartedAtEpochSeconds,
				chat.currentStepStartedAtMs,
				chat.detailedActivityClocks,
			)
		}
		columns = append(columns, activity, chat.headerProfile)
		summary = strings.Join(columns, "  ")
	}
	style := HintDescription()
	if chat.headerTarget == "" || chat.headerProfile == "" {
		style = Title(false)
	}
	spans := []Span{StyledSpan(" "+summary, style)}
	if chat.headerTitle != "" {
		spans = append(spans, StyledSpan("  ", style), StyledSpan(chat.headerTitle, Title(false)))
	}
	var suffix string
	switch anchor := chat.anchor.(type) {
	case TranscriptAnchorRestoring:
		suffix = separator + "Loading… "
	case TranscriptAnchorRow:
		suffix = fmt.Sprintf(
			"%smessage %d of %d%sEnd to follow ",
			separator,
			transcriptSaturatingAdd(anchor.Entry, 1),
			len(chat.entries),
			separator,
		)
	default:
		if chat.renderMode == client.TranscriptRenderModeRaw {
			suffix = separator + "raw source "
		} else {
			suffix = " "
		}
	}
	spans = append(spans, StyledSpan(suffix, style))
	return Line{Spans: spans}
}

// roleGutter is the gutter before every transcript body row, in the symbol
// set in force.
func roleGutter() string {
	return ThemeGlyphs().RoleGutter
}

const roleGutterWidth = 2

// previewRows is the body rows of an agent message for a summary viewport:
// the conversation's own rendering, minus the parts that only make sense
// inside it.
//
// No header row, blank rows dropped, and no role gutter. The gutter marks
// continuation rows underneath a role header; a summary row has no header
// and carries its own prefix, so a gutter there is a second marker for
// nothing. The wrap width is widened by the gutter it drops, so the caller
// gets the width columns of text it asked for.
//
// Only the first limit rows are produced: wrapping stops there, so a summary
// of a long message costs what its first rows cost.
func previewRows(source string, width int, limit int) []Line {
	entry := core.ChatEntryPlain(0, core.ChatRoleAgent, source)
	mode := client.TranscriptRenderModeRich
	// The same rows entryBodyRows gives for the widened width.
	visual := entryVisual(&entry)
	contentWidth := max(width, 1)
	var rows []Line
	for _, logical := range entryLogicalLines(&entry, mode, &visual, contentWidth, false) {
		if len(rows) >= limit {
			break
		}
		wrapStyledLineUntil(logical.line, contentWidth, logical.continuationIndent, func(row Line) bool {
			row = withRoleGutter(row, visual.railStyle)
			if !lineIsEmpty(row) {
				rows = append(rows, withoutRoleGutter(row))
			}
			return len(rows) < limit
		})
	}
	return rows
}

// RenderAgentMessageTail is the last rows of an agent message for a small
// preview viewport, rendered by the same pipeline as the conversation view.
func RenderAgentMessageTail(source string, width int, maximumLines int) []Line {
	if width == 0 || maximumLines == 0 {
		return nil
	}
	lines := previewRows(source, width, math.MaxInt)
	start := max(len(lines)-maximumLines, 0)
	return lines[start:]
}

// RenderAgentMessageHead is the first rows of an agent message for
// session-list summaries. Rich formatting is retained, but the final visible
// row announces omitted content.
func RenderAgentMessageHead(source string, width int, maximumLines int) []Line {
	if width == 0 || maximumLines == 0 {
		return nil
	}
	// One row past the limit says whether anything was left out.
	lines := previewRows(source, width, transcriptSaturatingAdd(maximumLines, 1))
	truncated := len(lines) > maximumLines
	lines = lines[:min(len(lines), maximumLines)]
	if truncated && len(lines) > 0 {
		appendTrimmedEllipsis(&lines[len(lines)-1], 0)
	}
	return lines
}

func emptyTranscriptRow(loading bool) Line {
	text := "No messages yet — send a prompt to begin."
	if loading {
		text = "Loading…"
	}
	return Line{Spans: []Span{{
		Content: text,
		Style:   Style{}.Foreground(ThemePalette().Muted).Add(ModifierItalic),
	}}}
}

func renderTranscriptEntry(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode) []Line {
	return renderTranscriptEntryWithOptions(entry, width, mode, ToolViewSummary{})
}

// renderTranscriptEntryExpanded renders a completed tool after the user
// opens it from a compact Rich row. The header keeps the active transcript
// mode, while the body uses the full provider title and attached details
// just like Raw mode.
func renderTranscriptEntryExpanded(entry *core.ChatEntry, width int) []Line {
	return renderTranscriptEntryWithOptions(entry, width, client.TranscriptRenderModeRich, ToolViewExpanded{})
}

func renderTranscriptEntryWithOptions(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, tool ToolView) []Line {
	return renderEntryTracking(entry, width, mode, tool, nil)
}

// trackTranscriptEntry is what rendering one entry with the given options
// records about its rows, in the row coordinates of
// renderTranscriptEntryWithOptions.
func trackTranscriptEntry(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, tool ToolView) RowTrack {
	var track RowTrack
	renderEntryTracking(entry, width, mode, tool, &track)
	return track
}

// RowTrack is what a tracking render records about the rows it draws.
type RowTrack struct {
	Links []RowLink
	// Copies has one element per rendered row.
	Copies []rowCopy
	// MailboxEvents is the rendered row range and durable identity for each
	// independently clickable event.
	MailboxEvents []MailboxEventRows
}

// MailboxEventRows is one element of RowTrack.MailboxEvents, Rust's
// (Range<usize>, MailboxDisclosureId): rows Start..End draw event ID.
type MailboxEventRows struct {
	Start int
	End   int
	ID    MailboxDisclosureId
}

// MailboxDisclosureId identifies an open mailbox event or incoming message.
// Messages delivered in one batch share an ordinal, so use their durable ID.
// Every variant is comparable, so identities work as map keys.
type MailboxDisclosureId interface{ isMailboxDisclosureId() }

// MailboxDisclosureIdEvent is event Index of the mailbox entry starting at
// StartSeq.
type MailboxDisclosureIdEvent struct {
	StartSeq uint64
	Index    int
}

// MailboxDisclosureIdMessage is an incoming message by its durable message ID.
type MailboxDisclosureIdMessage struct{ MessageID string }

func (MailboxDisclosureIdEvent) isMailboxDisclosureId()   {}
func (MailboxDisclosureIdMessage) isMailboxDisclosureId() {}

// RowLink says cells Start..End of rendered row Row draw text of a link to
// URL.
type RowLink struct {
	Row   int
	Start int
	End   int
	URL   string
}

// ToolView is how a tool entry presents its call. Every variant is
// comparable, so views compare with ==.
type ToolView interface{ isToolView() }

// ToolViewSummary is a status header over the call's short summary.
type ToolViewSummary struct{}

// ToolViewExpanded is opened by a click: the provider title and every
// detail.
type ToolViewExpanded struct{}

// ToolViewInline is the status mark and the full call on one row, over at
// most OutputLines rows of what it printed.
type ToolViewInline struct{ OutputLines int }

func (ToolViewSummary) isToolView()  {}
func (ToolViewExpanded) isToolView() {}
func (ToolViewInline) isToolView()   {}

// emptyRow is Line::from(""): a row with one empty span.
func emptyRow() Line {
	return Line{Spans: []Span{RawSpan("")}}
}

// inlineToolRows draws a tool call inline: its status mark and full command
// or title, then the head and tail of its output under an elbow. Every output
// row is cut to the width, so a call never takes more than its header and
// outputLines rows however long its output lines are. copies, when not nil,
// receives one rowCopy per row.
func inlineToolRows(entry *core.ChatEntry, width int, outputLines int, copies *[]rowCopy) []Line {
	glyph, label, statusStyle := toolRowPresentation(entry)
	presentation := entry.ToolPresentation
	var source string
	if presentation == nil {
		source = transcript.ToolDisplayTitle(entry.Text)
	} else {
		source = presentation.Source.Text()
	}
	source = sanitizeTerminalText(source)
	var sourceLines []string
	for _, line := range rustStrLines(source) {
		if strings.TrimSpace(line) != "" {
			sourceLines = append(sourceLines, line)
		}
	}
	command := ""
	if len(sourceLines) > 0 {
		command = strings.TrimSpace(sourceLines[0])
	}
	if len(sourceLines) > 1 {
		command += " " + ThemeGlyphs().Ellipsis
	}
	header := []Span{StyledSpan(glyph+" ", statusStyle.Add(ModifierBold))}
	if label == client.ToolAwaitingApproval {
		header = append(header, StyledSpan(label+": ", statusStyle))
	} else if presentation != nil && presentation.ToolKind == acp.ToolKindExecute {
		status := core.ToolStatusPending
		if entry.ToolStatus != nil {
			status = *entry.ToolStatus
		}
		verb := "Running "
		if status == core.ToolStatusCompleted || status == core.ToolStatusFailed {
			verb = "Ran "
		}
		header = append(header, StyledSpan(verb, Style{}.Add(ModifierBold)))
	}
	header = append(header, RawSpan(command))
	rows, _, headerCopies := wrapStyledLineForCopy(Line{Spans: header}, width, roleGutterWidth)

	elbow := ThemeGlyphs().ToolOutput
	indent := strings.Repeat(" ", roleGutterWidth)
	follow := strings.Repeat(" ", displayWidth(elbow))
	textWidth := max(width-(roleGutterWidth+displayWidth(elbow)), 1)
	bodyStyle := entryVisual(entry).bodyStyle
	for index, text := range inlineOutput(entry, outputLines) {
		lead := follow
		if index == 0 {
			lead = elbow
		}
		truncated := TruncateLineToWidth(Line{Spans: []Span{StyledSpan(text, bodyStyle)}}, textWidth)
		spans := []Span{RawSpan(indent), StyledSpan(lead, Muted())}
		spans = append(spans, truncated.Spans...)
		rows = append(rows, Line{Spans: spans})
	}
	if copies != nil {
		// Output rows are cut to the width, never wrapped.
		outputRows := len(rows) - len(headerCopies)
		*copies = append(*copies, headerCopies...)
		*copies = append(*copies, make([]rowCopy, outputRows)...)
	}
	return rows
}

// inlineOutput is the rows an inline call shows: its diffstats and output,
// cut to the first and last few lines around a count of what was left out.
// Reads and searches show no output, as their header already says what they
// looked at, unless they failed.
func inlineOutput(entry *core.ChatEntry, limit int) []string {
	explored := entry.ToolPresentation != nil &&
		(entry.ToolPresentation.ToolKind == acp.ToolKindRead || entry.ToolPresentation.ToolKind == acp.ToolKindSearch)
	failed := entry.ToolStatus != nil && *entry.ToolStatus == core.ToolStatusFailed
	lines := slices.Clone(entry.ToolDiffstats)
	if failed || !explored {
		output := strings.Join(toolContentWithoutError(entry), "\n")
		lines = append(lines, rustStrLines(strings.Trim(output, "\n"))...)
	}
	if failed && entry.ToolError != nil {
		lines = append(lines, rustStrLines("Error: "+*entry.ToolError)...)
	}
	if len(lines) <= limit {
		return lines
	}
	if limit == 0 {
		return nil
	}
	kept := limit - 1
	head := kept / 2
	tail := kept - head
	omitted := len(lines) - kept
	shown := slices.Clone(lines[:head])
	shown = append(shown, fmt.Sprintf("%s +%d lines", ThemeGlyphs().Ellipsis, omitted))
	shown = append(shown, lines[len(lines)-tail:]...)
	return shown
}

// renderEntryTracking renders one entry, and records its link cells and row
// copies when track is not nil. Every caller shares this so what it records
// matches the drawn rows.
func renderEntryTracking(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, tool ToolView, track *RowTrack) []Line {
	if mode == client.TranscriptRenderModeRich {
		if rows, mailboxTrack, ok := renderMailboxEntry(entry, width, nil); ok {
			if track != nil {
				*track = mailboxTrack
			}
			return rows
		}
	}
	if inline, ok := tool.(ToolViewInline); ok &&
		mode == client.TranscriptRenderModeRich &&
		entry.Role == core.ChatRoleTool {
		var copies *[]rowCopy
		if track != nil {
			copies = &track.Copies
		}
		out := inlineToolRows(entry, width, inline.OutputLines, copies)
		out = append(out, emptyRow())
		if track != nil {
			track.Copies = append(track.Copies, rowCopy{})
		}
		return out
	}
	visual := entryVisual(entry)
	timeText, hasTime := "", false
	switch entry.Role {
	case core.ChatRoleUser, core.ChatRoleAgent, core.ChatRoleMessage, core.ChatRoleSystem:
		timeText, hasTime = FormatEventTime(entry.RecordedAtMs)
	case core.ChatRoleTool:
		if tool == (ToolViewExpanded{}) {
			timeText, hasTime = FormatEventRange(entry.RecordedAtMs, entry.ToolEndedAtMs)
		}
	}
	header := []Span{
		StyledSpan(visual.glyph+" ", visual.headerStyle.Add(ModifierBold)),
		StyledSpan(visual.label, visual.headerStyle.Add(ModifierBold)),
	}
	if hasTime {
		header = append(header, StyledSpan(ThemeGlyphs().FooterSeparator+timeText, Muted()))
	}
	headerRows, _, headerCopies := wrapStyledLineForCopy(Line{Spans: header}, width, roleGutterWidth)
	out := make([]Line, 0, len(headerRows)+1)
	for _, line := range headerRows {
		if entry.Role == core.ChatRoleUser {
			line = line.WithStyle(Raised())
		}
		out = append(out, line)
	}
	if track != nil {
		track.Copies = append(track.Copies, headerCopies...)
	}
	headerRowCount := len(out)
	out = append(out, entryBodyRowsInner(
		entry,
		width,
		mode,
		tool == (ToolViewExpanded{}),
		track,
		headerRowCount,
	)...)
	out = append(out, emptyRow())
	if track != nil {
		track.Copies = append(track.Copies, rowCopy{})
	}
	return out
}

// estimatedEntryRows is the rows an entry is likely to take at width, worked
// out from its text without rendering it. The scrollbar places entries that
// have not been drawn yet with this, and the real count replaces it once one
// is drawn.
//
// It follows the layout renderEntryTracking draws: a header row, the body
// wrapped beside the role gutter, and a blank row. Markdown and word
// wrapping can make the real count differ by a few rows.
func estimatedEntryRows(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, tool ToolView) int {
	if mode == client.TranscriptRenderModeRich {
		if entry.Role == core.ChatRoleMessage {
			return 2
		}
		events := client.MailboxEventLines(entry.Role, rustStrLines(entry.Text))
		if len(events) > 0 {
			// One header per closed event, then the blank row.
			return len(events) + 1
		}
		if inline, ok := tool.(ToolViewInline); ok && entry.Role == core.ChatRoleTool {
			// The call on one row, its clipped output, then the blank row.
			return len(inlineOutput(entry, inline.OutputLines)) + 2
		}
	}
	// The body is wrapped as plain text by the same wrapper that draws it, so
	// only Markdown formatting is left out of the count.
	contentWidth := max(width-roleGutterWidth, 1)
	wrapped := func(logical logicalLine) int {
		return len(WrapStyledLine(logical.line, contentWidth, logical.continuationIndent))
	}
	body := 0
	if entry.Role == core.ChatRolePlan {
		for _, item := range entry.Plan {
			body += wrapped(logicalLine{
				line:               RawLine(ThemeGlyphs().Pending + " " + item.Text),
				continuationIndent: 2,
			})
		}
	} else {
		source := entryBodySource(entry, mode, false)
		if mode == client.TranscriptRenderModeRich {
			// Markdown drops the newline that ends the last paragraph.
			source = strings.TrimRight(source, "\n")
		}
		for _, logical := range rawLines(source, Style{}) {
			body += wrapped(logical)
		}
	}
	return body + 2
}

// renderMailboxEntry renders event headers and report bodies together so
// clicks and selection use the exact row positions that were drawn. False
// when the entry is not a mailbox entry.
func renderMailboxEntry(entry *core.ChatEntry, width int, expanded map[MailboxDisclosureId]struct{}) ([]Line, RowTrack, bool) {
	message := entry.Role == core.ChatRoleMessage
	var events [][]string
	if message {
		events = [][]string{{entryVisual(entry).label}}
	} else {
		events = client.MailboxEventLines(entry.Role, rustStrLines(entry.Text))
	}
	if len(events) == 0 {
		return nil, RowTrack{}, false
	}
	var rows []Line
	var track RowTrack
	for index, lines := range events {
		var id MailboxDisclosureId
		if message {
			if entry.MessageID == nil {
				panic("message entry identity")
			}
			id = MailboxDisclosureIdMessage{MessageID: *entry.MessageID}
		} else {
			id = MailboxDisclosureIdEvent{StartSeq: entry.StartSeq, Index: index}
		}
		_, open := expanded[id]
		arrow := "▸"
		if open {
			arrow = ThemeGlyphs().Dropdown
		}
		style := Muted()
		if message {
			style = entryVisual(entry).headerStyle.Add(ModifierBold)
		}
		header := []Span{StyledSpan(arrow+" "+lines[0], style)}
		if eventTime, ok := FormatEventTime(entry.RecordedAtMs); ok {
			header = append(header, StyledSpan(ThemeGlyphs().FooterSeparator+eventTime, Muted()))
		}
		eventStart := len(rows)
		if open {
			headerRows, _, copies := wrapStyledLineForCopy(Line{Spans: header}, width, roleGutterWidth)
			rows = append(rows, headerRows...)
			track.Copies = append(track.Copies, copies...)
		} else {
			rows = append(rows, TruncateLineToWidth(Line{Spans: header}, width))
			track.Copies = append(track.Copies, rowCopy{})
		}
		if open {
			firstRow := len(rows)
			if message {
				rows = append(rows, entryBodyRowsInner(
					entry,
					width,
					client.TranscriptRenderModeRich,
					false,
					&track,
					firstRow,
				)...)
			} else if len(lines) > 1 {
				detail := core.ChatEntryPlain(entry.StartSeq, core.ChatRoleSystem, strings.Join(lines[1:], "\n"))
				rows = append(rows, entryBodyRowsInner(
					&detail,
					width,
					client.TranscriptRenderModeRaw,
					false,
					&track,
					firstRow,
				)...)
			}
		}
		track.MailboxEvents = append(track.MailboxEvents, MailboxEventRows{Start: eventStart, End: len(rows), ID: id})
	}
	rows = append(rows, emptyRow())
	track.Copies = append(track.Copies, rowCopy{})
	return rows, track, true
}

// entryBodyRows is the gutter-prefixed body rows of one entry, shared
// between the full conversation view and the dashboard preview tail.
func entryBodyRows(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode) []Line {
	return entryBodyRowsWithOptions(entry, width, mode, false)
}

func entryBodyRowsWithOptions(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, expandedTool bool) []Line {
	return entryBodyRowsInner(entry, width, mode, expandedTool, nil, 0)
}

// entryBodyRowsInner is the body rows, and with track what each row records,
// numbered from firstRow (Rust's Option<(&mut RowTrack, usize)>; firstRow is
// unused when track is nil).
func entryBodyRowsInner(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, expandedTool bool, track *RowTrack, firstRow int) []Line {
	visual := entryVisual(entry)
	contentWidth := max(width-roleGutterWidth, 1)
	var rows []Line
	for _, logical := range entryLogicalLines(entry, mode, &visual, contentWidth, expandedTool) {
		if track == nil {
			rows = append(rows, WrapStyledLine(logical.line, contentWidth, logical.continuationIndent)...)
			continue
		}
		wrapped, sources, copies := wrapStyledLineForCopy(logical.line, contentWidth, logical.continuationIndent)
		for offset, cells := range sources {
			collectRowLinks(&track.Links, firstRow+len(rows)+offset, cells, logical.links)
		}
		// Each body row is drawn after the role gutter.
		for _, copy := range copies {
			copy.lead += roleGutterWidth
			track.Copies = append(track.Copies, copy)
		}
		rows = append(rows, wrapped...)
	}
	for index := range rows {
		rows[index] = withRoleGutter(rows[index], visual.railStyle)
	}
	return rows
}

// collectRowLinks turns one wrapped row's cell sources into runs of link
// cells, shifted past the role gutter. Adjacent links stay separate runs.
// cells holds the source span index of each cell, negative for a cell no
// span drew (Rust's None).
func collectRowLinks(links *[]RowLink, row int, cells []int, spans []linkSpan) {
	var run *RowLink
	for cell, source := range cells {
		url, hasURL := "", false
		if source >= 0 {
			for _, link := range spans {
				if link.span == source {
					url, hasURL = link.url, true
					break
				}
			}
		}
		column := roleGutterWidth + cell
		if run != nil && hasURL && run.URL == url && run.End == column {
			run.End = column + 1
			continue
		}
		if run != nil {
			*links = append(*links, *run)
			run = nil
		}
		if hasURL {
			run = &RowLink{Row: row, Start: column, End: column + 1, URL: url}
		}
	}
	if run != nil {
		*links = append(*links, *run)
	}
}

// The date range chrono's DateTime::from_timestamp_millis accepts (chrono
// 0.4.44: years -262143 through 262142); outside it Rust has no time to show.
var (
	eventTimeMinMillis = time.Date(-262143, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli()
	eventTimeEndMillis = time.Date(262143, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli()
)

// FormatEventTime formats an optional transcript event timestamp as local
// 24-hour time.
func FormatEventTime(recordedAtMs *int64) (string, bool) {
	if recordedAtMs == nil || *recordedAtMs < eventTimeMinMillis || *recordedAtMs >= eventTimeEndMillis {
		return "", false
	}
	return time.UnixMilli(*recordedAtMs).Local().Format("15:04"), true
}

// FormatEventRange formats when a tool call ran as local 24-hour times. A
// call that took less than a minute, or has not ended, shows only when it
// started.
func FormatEventRange(startedAtMs *int64, endedAtMs *int64) (string, bool) {
	started, ok := FormatEventTime(startedAtMs)
	if !ok {
		return "", false
	}
	if endedAtMs != nil && *endedAtMs-*startedAtMs >= 60_000 {
		ended, ok := FormatEventTime(endedAtMs)
		if !ok {
			return "", false
		}
		return started + "–" + ended, true
	}
	return started, true
}

func entryLogicalLines(entry *core.ChatEntry, mode client.TranscriptRenderMode, visual *EntryVisual, width int, expandedTool bool) []logicalLine {
	if entry.Role == core.ChatRolePlan {
		lines := make([]logicalLine, 0, len(entry.Plan))
		for _, item := range entry.Plan {
			var glyph string
			var style Style
			switch item.Status {
			case core.PlanStatusRunning:
				glyph, style = ThemeGlyphs().Running, Style{}.Foreground(ThemePalette().Warning)
			case core.PlanStatusCompleted:
				glyph, style = ThemeGlyphs().Check, Style{}.Foreground(ThemePalette().Success)
			default:
				glyph, style = ThemeGlyphs().Pending, Style{}.Foreground(ThemePalette().Muted)
			}
			lines = append(lines, logicalLine{
				line: Line{Spans: []Span{
					StyledSpan(glyph+" ", style),
					StyledSpan(item.Text, visual.bodyStyle),
				}},
				continuationIndent: 2,
			})
		}
		return lines
	}

	if expandedTool &&
		mode == client.TranscriptRenderModeRich &&
		entry.Role == core.ChatRoleTool &&
		(entry.ToolInput != nil || entry.ToolError != nil) {
		title := transcript.ToolDisplayTitle(entry.Text)
		if entry.LeadingOmitted {
			title = "[… earlier content omitted …]\n" + title
		}
		lines := markdownLines(title, visual.bodyStyle, visual.headerStyle, width)
		if entry.ToolInput != nil {
			input := "Input arguments\n" + transcript.ToolInputText(entry.ToolInput)
			lines = append(lines, rawLines(input, visual.bodyStyle)...)
		}
		if details := strings.Join(toolDetails(entry), "\n"); details != "" {
			lines = append(lines, markdownLines(details, visual.bodyStyle, visual.headerStyle, width)...)
		}
		if entry.ToolError != nil {
			toolError := "Error\n" + core.SanitizeTerminalText(*entry.ToolError)
			lines = append(lines, rawLines(toolError, visual.bodyStyle)...)
		}
		lines = append(lines, toolPatchLines(entry, visual.bodyStyle)...)
		return lines
	}
	source := entryBodySource(entry, mode, expandedTool)
	var lines []logicalLine
	switch mode {
	case client.TranscriptRenderModeRaw:
		lines = rawLines(source, visual.bodyStyle)
	default:
		lines = markdownLines(source, visual.bodyStyle, visual.headerStyle, width)
	}
	if expandedTool || mode == client.TranscriptRenderModeRaw {
		lines = append(lines, toolPatchLines(entry, visual.bodyStyle)...)
	}
	return lines
}

func toolPatchLines(entry *core.ChatEntry, style Style) []logicalLine {
	if entry.ToolDetailTrimmed || len(entry.ToolDiffstats) == 0 {
		return nil
	}
	item := entry.Source.Item
	if item == nil {
		return nil
	}
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return nil
	}
	call, err := body.Call.Decode()
	if err != nil {
		return nil
	}
	var lines []logicalLine
	for _, part := range call.Content {
		// Live projection already stored the patch. Legacy copies can supply
		// their counts through the existing background diff job.
		if part.Kind != acp.ToolCallContentKindDiff || part.Diff == nil {
			continue
		}
		diff := part.Diff
		patch := core.StoredPatch(&schema.Diff{Path: diff.Path, OldText: diff.OldText, NewText: diff.NewText, Meta: diff.Meta})
		if patch == nil {
			continue
		}
		lines = append(lines, rawLines(sanitizeTerminalText(patch.Text), style)...)
	}
	return lines
}

// toolDetails is the tool output, diff stats, and locations an entry's
// details show.
func toolDetails(entry *core.ChatEntry) []string {
	details := toolContentWithoutError(entry)
	details = append(details, entry.ToolDiffstats...)
	return append(details, entry.ToolLocations...)
}

// toolContentWithoutError: ordinary content must not duplicate the
// separately rendered raw-output diagnostic, whose literal rendering is
// authoritative for the error.
func toolContentWithoutError(entry *core.ChatEntry) []string {
	var content []string
	for _, text := range entry.ToolContent {
		if entry.ToolError == nil || *entry.ToolError != text {
			content = append(content, text)
		}
	}
	return content
}

// toolInputPretty is serde_json::to_string_pretty of the tool's arguments:
// members in their stored order, two-space indentation.
func toolInputPretty(input json.RawMessage) string {
	value, err := jsonvalue.Parse(input)
	if err != nil {
		panic("tool arguments are serializable JSON values")
	}
	compact, err := jsonvalue.Marshal(value)
	if err != nil {
		panic("tool arguments are serializable JSON values")
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, compact, "", "  "); err != nil {
		panic("tool arguments are serializable JSON values")
	}
	return pretty.String()
}

// entryBodySource is the text an entry's body is drawn from. Plans and an
// opened tool call with arguments draw their own bodies and do not use it.
func entryBodySource(entry *core.ChatEntry, mode client.TranscriptRenderMode, expandedTool bool) string {
	details := toolDetails(entry)
	if mode == client.TranscriptRenderModeRaw && entry.ToolInput != nil {
		input := core.SanitizeTerminalText(toolInputPretty(entry.ToolInput))
		details = slices.Insert(details, 0, "Input arguments\n"+input)
	}
	if mode == client.TranscriptRenderModeRaw && entry.ToolError != nil {
		details = append(details, "Error\n"+core.SanitizeTerminalText(*entry.ToolError))
	}
	text := entry.Text
	if entry.Role == core.ChatRoleTool {
		text = transcript.ToolDisplayTitle(entry.Text)
	}
	var source string
	switch {
	case mode == client.TranscriptRenderModeRich && expandedTool && entry.Role == core.ChatRoleTool:
		if len(details) == 0 {
			source = text
		} else {
			source = text + "\n" + strings.Join(details, "\n")
		}
	case mode == client.TranscriptRenderModeRaw && len(details) > 0:
		source = text + "\n" + strings.Join(details, "\n")
	case mode == client.TranscriptRenderModeRich && len(entry.ToolDiffstats) > 0:
		summary := text
		if entry.ToolSummary != nil {
			summary = *entry.ToolSummary
		}
		source = summary + "\n" + strings.Join(entry.ToolDiffstats, "\n")
	case mode == client.TranscriptRenderModeRich && entry.Role == core.ChatRoleTool:
		source = text
		if entry.ToolSummary != nil {
			source = *entry.ToolSummary
		}
	default:
		source = text
	}
	if entry.LeadingOmitted {
		source = "[… earlier content omitted …]\n" + source
	}
	return source
}

// roleGlyph is the mark a transcript header draws for one entry, in the
// symbol set in force. client.EntryGlyph keeps the Unicode marks the browser
// projection sends; a terminal has to be able to fall back to ASCII.
func roleGlyph(entry *core.ChatEntry) string {
	glyphs := ThemeGlyphs()
	switch entry.Role {
	case core.ChatRoleUser:
		return glyphs.RoleUser
	case core.ChatRoleMessage:
		return glyphs.RoleMessage
	case core.ChatRoleAgent:
		return glyphs.RoleAgent
	case core.ChatRoleThought:
		return glyphs.RoleThought
	case core.ChatRolePlan:
		return glyphs.RolePlan
	case core.ChatRolePlanProposal:
		return glyphs.RolePlanProposal
	case core.ChatRoleSystem:
		return glyphs.Rule
	default:
		glyph, _, _ := toolRowPresentation(entry)
		return glyph
	}
}

// toolRowPresentation is a tool row's status mark, label and header style. A
// call that ended after its turn was interrupted says so instead of "done"
// (R10-1).
func toolRowPresentation(entry *core.ChatEntry) (string, string, Style) {
	if entry.EndedAfterInterrupt {
		return ThemeGlyphs().Stopped,
			client.ToolEndedAfterInterrupt,
			Style{}.Foreground(ThemePalette().Muted)
	}
	if entry.AwaitingApproval {
		return ThemeGlyphs().Waiting,
			client.ToolAwaitingApproval,
			Style{}.Foreground(ThemePalette().SessionAttention).Add(ModifierBold)
	}
	status := core.ToolStatusPending
	if entry.ToolStatus != nil {
		status = *entry.ToolStatus
	}
	return toolPresentation(status)
}

func entryVisual(entry *core.ChatEntry) EntryVisual {
	palette := ThemePalette()
	switch entry.Role {
	case core.ChatRoleUser:
		style := Style{}.Foreground(palette.Secondary)
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       client.UserLabel(entry),
			headerStyle: style,
			bodyStyle:   Style{},
			railStyle:   style,
		}
	case core.ChatRoleAgent:
		style := Style{}.Foreground(palette.Accent)
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       "Agent",
			headerStyle: style,
			bodyStyle:   Style{},
			railStyle:   Style{}.Foreground(palette.Border),
		}
	case core.ChatRoleMessage:
		style := Style{}.Foreground(palette.Secondary)
		if entry.IncomingMessage == nil {
			panic("message entry metadata")
		}
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       entry.IncomingMessage.MessageLabel(),
			headerStyle: style,
			bodyStyle:   Style{},
			railStyle:   style,
		}
	case core.ChatRoleThought:
		style := Style{}.Foreground(palette.Muted).Add(ModifierItalic)
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       "Thinking",
			headerStyle: style,
			bodyStyle:   style,
			railStyle:   Style{}.Foreground(palette.Border),
		}
	case core.ChatRoleTool:
		status := core.ToolStatusPending
		if entry.ToolStatus != nil {
			status = *entry.ToolStatus
		}
		_, label, style := toolRowPresentation(entry)
		bodyStyle := Style{}
		if status == core.ToolStatusCompleted {
			bodyStyle = Style{}.Foreground(palette.Muted)
		}
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       "Tool" + ThemeGlyphs().FooterSeparator + label,
			headerStyle: style,
			bodyStyle:   bodyStyle,
			railStyle:   Style{}.Foreground(palette.Border),
		}
	case core.ChatRolePlan:
		style := Style{}.Foreground(palette.Secondary)
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       "Plan",
			headerStyle: style,
			bodyStyle:   Style{},
			railStyle:   style,
		}
	case core.ChatRolePlanProposal:
		style := Style{}.Foreground(palette.Secondary)
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       "Proposed plan",
			headerStyle: style,
			bodyStyle:   Style{},
			railStyle:   style,
		}
	default:
		style := Style{}.Foreground(palette.Muted)
		return EntryVisual{
			glyph:       roleGlyph(entry),
			label:       "Mjolnir",
			headerStyle: style,
			bodyStyle:   style,
			railStyle:   style,
		}
	}
}

func toolPresentation(status core.ToolStatus) (string, string, Style) {
	switch status {
	case core.ToolStatusCompleted:
		return ThemeGlyphs().Check, "done", Style{}.Foreground(ThemePalette().Muted)
	case core.ToolStatusFailed:
		return strings.TrimSpace(ThemeGlyphs().Failed), "failed", Style{}.Foreground(ThemePalette().Error)
	default:
		return ThemeGlyphs().Running, "running", Style{}.Foreground(ThemePalette().Warning)
	}
}

func withRoleGutter(line Line, style Style) Line {
	spans := make([]Span, 0, len(line.Spans)+1)
	spans = append(spans, Span{Content: roleGutter(), Style: style})
	spans = append(spans, line.Spans...)
	return Line{Spans: spans}
}

// withoutRoleGutter drops the leading role gutter from a rendered row, for
// viewports that do not draw the rail it belongs to.
func withoutRoleGutter(line Line) Line {
	if len(line.Spans) > 0 && line.Spans[0].Content == roleGutter() {
		line.Spans = line.Spans[1:]
	}
	return line
}

func lineIsEmpty(line Line) bool {
	gutter := roleGutter()
	for _, span := range line.Spans {
		if strings.TrimSpace(span.Content) != "" && span.Content != gutter {
			return false
		}
	}
	return true
}
