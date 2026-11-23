// Port of mj-chat/src/chat/transcript/render.rs.

package chat

import (
	"math"
	"strings"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	panic("unported: mj-chat/src/chat/transcript/render.rs render_transcript")
}

func transcriptTitle(chat *ChatState, nowEpochSeconds uint64) Line {
	panic("unported: mj-chat/src/chat/transcript/render.rs transcript_title")
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
	panic("unported: mj-chat/src/chat/transcript/render.rs preview_rows")
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
	lines := previewRows(source, width, maximumLines+1)
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

// inlineToolRows draws a tool call inline: its status mark and full command
// or title, then the head and tail of its output under an elbow. Every output
// row is cut to the width, so a call never takes more than its header and
// outputLines rows however long its output lines are. copies, when not nil,
// receives one rowCopy per row.
func inlineToolRows(entry *core.ChatEntry, width int, outputLines int, copies *[]rowCopy) []Line {
	panic("unported: mj-chat/src/chat/transcript/render.rs inline_tool_rows")
}

// inlineOutput is the rows an inline call shows: its diffstats and output,
// cut to the first and last few lines around a count of what was left out.
// Reads and searches show no output, as their header already says what they
// looked at, unless they failed.
func inlineOutput(entry *core.ChatEntry, limit int) []string {
	panic("unported: mj-chat/src/chat/transcript/render.rs inline_output")
}

// renderEntryTracking renders one entry, and records its link cells and row
// copies when track is not nil. Every caller shares this so what it records
// matches the drawn rows.
func renderEntryTracking(entry *core.ChatEntry, width int, mode client.TranscriptRenderMode, tool ToolView, track *RowTrack) []Line {
	panic("unported: mj-chat/src/chat/transcript/render.rs render_entry_tracking")
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
	panic("unported: mj-chat/src/chat/transcript/render.rs estimated_entry_rows")
}

// renderMailboxEntry renders event headers and report bodies together so
// clicks and selection use the exact row positions that were drawn. False
// when the entry is not a mailbox entry.
func renderMailboxEntry(entry *core.ChatEntry, width int, expanded map[MailboxDisclosureId]struct{}) ([]Line, RowTrack, bool) {
	panic("unported: mj-chat/src/chat/transcript/render.rs render_mailbox_entry")
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
	panic("unported: mj-chat/src/chat/transcript/render.rs entry_body_rows_inner")
}

// collectRowLinks turns one wrapped row's cell sources into runs of link
// cells, shifted past the role gutter. Adjacent links stay separate runs.
// cells holds the source span index of each cell, negative for a cell no
// span drew (Rust's None).
func collectRowLinks(links *[]RowLink, row int, cells []int, spans []linkSpan) {
	panic("unported: mj-chat/src/chat/transcript/render.rs collect_row_links")
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
	panic("unported: mj-chat/src/chat/transcript/render.rs entry_logical_lines")
}

func toolPatchLines(entry *core.ChatEntry, style Style) []logicalLine {
	panic("unported: mj-chat/src/chat/transcript/render.rs tool_patch_lines")
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

// entryBodySource is the text an entry's body is drawn from. Plans and an
// opened tool call with arguments draw their own bodies and do not use it.
func entryBodySource(entry *core.ChatEntry, mode client.TranscriptRenderMode, expandedTool bool) string {
	panic("unported: mj-chat/src/chat/transcript/render.rs entry_body_source")
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
	panic("unported: mj-chat/src/chat/transcript/render.rs tool_row_presentation")
}

func entryVisual(entry *core.ChatEntry) EntryVisual {
	panic("unported: mj-chat/src/chat/transcript/render.rs entry_visual")
}

func toolPresentation(status core.ToolStatus) (string, string, Style) {
	panic("unported: mj-chat/src/chat/transcript/render.rs tool_presentation")
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
