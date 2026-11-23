// Port of mj-chat/src/chat/rendering.rs.
//
// Markdown and width-aware transcript rendering.
package chat

import (
	"iter"
	"math"
	"strings"
	"unicode"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// voiceButtonGlyph is the microphone glyph in the symbol set in force, so a
// console without UTF-8 gets the plain button rather than mojibake in the
// composer's border.
func voiceButtonGlyph() string { return ThemeGlyphs().Microphone }

// voiceButtonArea is the microphone chip in the upper-left of the prompt's
// top border. Like other border titles, it leaves one blank cell after the
// corner. Its geometry is derived from the same Unicode width a Line is laid
// out with.
func voiceButtonArea(promptArea uv.Rectangle) (uv.Rectangle, bool) {
	if promptArea.Dx() == 0 || promptArea.Dy() == 0 {
		return uv.Rectangle{}, false
	}
	buttonWidth := displayWidth(" " + voiceButtonGlyph() + " ")
	if buttonWidth > math.MaxUint16 {
		return uv.Rectangle{}, false
	}
	x := promptArea.Min.X + 2
	if x+buttonWidth < promptArea.Max.X {
		return uv.Rect(x, promptArea.Min.Y, buttonWidth, 1), true
	}
	return uv.Rectangle{}, false
}

// voiceButtonLine renders the microphone chip that belongs to the prompt's
// top border.
func voiceButtonLine(voiceAvailable, voiceActive bool) Line {
	var style Style
	switch {
	case voiceActive:
		style = Filled(ThemePalette().Error).Add(ModifierBold)
	case voiceAvailable:
		style = Filled(ThemePalette().Accent).Add(ModifierBold)
	default:
		style = Style{}.Foreground(ThemePalette().Muted).Patch(Raised())
	}
	return LineFrom(StyledSpan(" "+voiceButtonGlyph()+" ", style)).LeftAligned()
}

// TranscriptRenderMode is re-exported from client.
type TranscriptRenderMode = client.TranscriptRenderMode

const (
	TranscriptRenderModeRich = client.TranscriptRenderModeRich
	TranscriptRenderModeRaw  = client.TranscriptRenderModeRaw
)

// logicalLine is one source line before wrapping.
type logicalLine struct {
	line               Line
	continuationIndent int
	// links are the spans of line that render a Markdown link, and where
	// each points.
	links []linkSpan
}

// linkSpan is one span of a rendered line that belongs to a Markdown link.
type linkSpan struct {
	// span is an index into the line's spans.
	span int
	url  string
}

type listState struct {
	next *uint64
}

// tableAlignment is a Markdown table column's alignment (pulldown-cmark's
// Alignment).
type tableAlignment uint8

const (
	tableAlignmentNone tableAlignment = iota
	tableAlignmentLeft
	tableAlignmentCenter
	tableAlignmentRight
)

type tableState struct {
	alignments []tableAlignment
	rows       [][]tableCell
	row        []tableCell
	cell       tableCell
}

// tableCell is one table cell's styled text, laid out once the whole table
// is known.
type tableCell struct {
	spans []Span
	// links are link spans, indexed into spans.
	links []linkSpan
}

func (c *tableCell) isEmpty() bool { return len(c.spans) == 0 }

func (c *tableCell) text() string {
	var text strings.Builder
	for _, span := range c.spans {
		text.WriteString(span.Content)
	}
	return text.String()
}

type markdownWriter struct {
	lines      []logicalLine
	spans      []Span
	style      Style
	quoteDepth int
	lists      []listState
	itemPrefix *string
	table      *tableState
	width      int
	// link is the destination of the link whose text is being written, if
	// any.
	link *string
	// lineLinks are link spans of the line in progress, indexed into spans.
	lineLinks []linkSpan
	// pendingText is consecutive text events not yet written. The parser can
	// split one run of text into several events, so URLs are found in the
	// whole run.
	pendingText string
}

func newMarkdownWriter(width int, style Style) *markdownWriter {
	return &markdownWriter{style: style, width: max(width, 1)}
}

// pushSpan adds an inline span to the table cell or line in progress,
// recording it as link text inside a link.
func (w *markdownWriter) pushSpan(span Span) {
	spans, links := &w.spans, &w.lineLinks
	if w.table != nil {
		spans, links = &w.table.cell.spans, &w.table.cell.links
	}
	if w.link != nil {
		*links = append(*links, linkSpan{span: len(*spans), url: *w.link})
	}
	*spans = append(*spans, span)
}

func (w *markdownWriter) pushText(text string, style Style) {
	if w.table != nil {
		// A table cell is one row of source text.
		w.pushSpan(StyledSpan(strings.ReplaceAll(text, "\n", " "), style))
		return
	}
	parts := strings.Split(text, "\n")
	for i, part := range parts {
		if part != "" {
			w.pushSpan(StyledSpan(part, style))
		}
		if i+1 < len(parts) {
			w.finishLine()
		}
	}
}

// pushLinkified writes text, turning each URL written out in it into a link
// to itself. Text that is already a link's label stays one link.
func (w *markdownWriter) pushLinkified(text string, style Style) {
	panic("unported: mj-chat/src/chat/rendering.rs MarkdownWriter::push_linkified")
}

func (w *markdownWriter) flushText() {
	if w.pendingText == "" {
		return
	}
	text := w.pendingText
	w.pendingText = ""
	w.pushLinkified(text, w.style)
}

func (w *markdownWriter) finishLine() {
	panic("unported: mj-chat/src/chat/rendering.rs MarkdownWriter::finish_line")
}

func (w *markdownWriter) finishBlock() {
	if len(w.spans) > 0 || w.itemPrefix != nil {
		w.finishLine()
	}
	if n := len(w.lines); n > 0 && len(w.lines[n-1].line.Spans) > 0 {
		w.lines = append(w.lines, logicalLine{})
	}
}

func (w *markdownWriter) finish() []logicalLine {
	if len(w.spans) > 0 || w.itemPrefix != nil {
		w.finishLine()
	}
	for n := len(w.lines); n > 0 && len(w.lines[n-1].line.Spans) == 0; n = len(w.lines) {
		w.lines = w.lines[:n-1]
	}
	return w.lines
}

func (w *markdownWriter) renderTable(table tableState) {
	panic("unported: mj-chat/src/chat/rendering.rs MarkdownWriter::render_table")
}

func (w *markdownWriter) renderTableRow(row []tableCell, columnWidths []int, alignments []tableAlignment, emphasis Style) {
	panic("unported: mj-chat/src/chat/rendering.rs MarkdownWriter::render_table_row")
}

// sanitizeTerminalText is re-exported from core's transcript.
func sanitizeTerminalText(text string) string { return core.SanitizeTerminalText(text) }

func markdownLines(source string, bodyStyle, accentStyle Style, width int) []logicalLine {
	panic("unported: mj-chat/src/chat/rendering.rs markdown_lines")
}

func rawLines(source string, style Style) []logicalLine {
	pieces := strings.Split(source, "\n")
	lines := make([]logicalLine, len(pieces))
	for i, line := range pieces {
		lines[i] = logicalLine{
			line:               LineFrom(StyledSpan(line, style)),
			continuationIndent: displayWidth(line[:len(line)-len(strings.TrimLeftFunc(line, unicode.IsSpace))]),
		}
	}
	return lines
}

// WrapStyledLine wraps a styled line to width terminal cells, keeping each
// span's style.
//
// Wrapping is grapheme-aware and breaks on word boundaries, cutting a word
// only when it cannot fit a row on its own. Continuation rows are indented
// by continuationIndent cells; pass 0 for no indent. The returned rows are
// exactly the rows a renderer draws, so a caller can count them for a scroll
// clamp or a scrollbar.
func WrapStyledLine(line Line, width, continuationIndent int) []Line {
	return wrapLine(line, width, continuationIndent, nil, nil)
}

// rowCopy is how copying one rendered row gives back the text it was drawn
// from.
type rowCopy struct {
	// lead is the leading cells that only decorate the row, such as a role
	// gutter or a wrap indent.
	lead int
	// joins is, on a row that wrapping split from the row above, the text
	// the break removed between them: a space, or nothing where a word was
	// cut. It points at rowCopyJoinSpace or rowCopyJoinCut, so rowCopy
	// values compare with == as Rust's do.
	joins *string
}

var (
	rowCopyJoinSpace = " "
	rowCopyJoinCut   = ""
)

// cellSources gives, for each wrapped row, the index of the source span
// drawn in each cell, or noCellSource for a continuation indent (Rust's
// `Vec<Vec<Option<usize>>>`; a sentinel keeps the per-cell table free of
// allocations).
type cellSources = [][]int

// noCellSource marks a cell drawn by a continuation indent.
const noCellSource = -1

// wrapStyledLineWithSources wraps line exactly as WrapStyledLine does, and
// also reports which of its spans each cell of each row came from.
func wrapStyledLineWithSources(line Line, width, continuationIndent int) ([]Line, cellSources) {
	var sources cellSources
	rows := wrapLine(line, width, continuationIndent, &sources, nil)
	return rows, sources
}

// wrapStyledLineForCopy wraps line exactly as WrapStyledLine does, and also
// reports its cell sources and how copying each row gives back the line's
// text.
func wrapStyledLineForCopy(line Line, width, continuationIndent int) ([]Line, cellSources, []rowCopy) {
	var sources cellSources
	var copies []rowCopy
	rows := wrapLine(line, width, continuationIndent, &sources, &copies)
	return rows, sources, copies
}

func wrapLine(line Line, width, continuationIndent int, sources *cellSources, copies *[]rowCopy) []Line {
	panic("unported: mj-chat/src/chat/rendering.rs wrap_line")
}

// wrapStyledLineUntil wraps line exactly as WrapStyledLine does, handing
// each row to emit as soon as it is complete. Wrapping stops when emit
// returns false, so a caller that needs only the first rows of a long line
// never segments the rest of it.
func wrapStyledLineUntil(line Line, width, continuationIndent int, emit func(Line) bool) {
	wrapGraphemes(&line, width, continuationIndent, func(buffer *styledBuffer, row []grapheme, _ rowCopy) bool {
		return emit(buffer.line(row))
	})
}

// grapheme is one grapheme of a line being wrapped, kept as a byte range into
// a shared buffer so wrapping a long transcript does not allocate per
// grapheme.
type grapheme struct {
	start int
	end   int
	// style is an index into the wrapper's style table.
	style      int
	width      uint8
	whitespace bool
}

// styledBuffer is the text and styles of a logical line, flattened for
// wrapping.
type styledBuffer struct {
	text   string
	styles []Style
	// spans are each span's byte range in text. A span's index is its
	// style's.
	spans [][2]int
	// space is the single trailing space that continuation indents point at.
	space int
}

func newStyledBuffer(line *Line) *styledBuffer {
	panic("unported: mj-chat/src/chat/rendering.rs StyledBuffer::new")
}

// graphemes is the line's graphemes in order. Segmentation is the costly
// part of wrapping, so it goes only as far as the caller reads.
func (b *styledBuffer) graphemes() iter.Seq[grapheme] {
	panic("unported: mj-chat/src/chat/rendering.rs StyledBuffer::graphemes")
}

func (b *styledBuffer) indent() grapheme {
	return grapheme{start: b.space, end: b.space + 1, style: len(b.styles) - 1, width: 1, whitespace: true}
}

// line joins row into spans, merging runs that share a style.
func (b *styledBuffer) line(row []grapheme) Line {
	panic("unported: mj-chat/src/chat/rendering.rs StyledBuffer::line")
}

// rowEmitter receives each finished row; returning false stops wrapping.
// row is only valid during the call.
type rowEmitter func(buffer *styledBuffer, row []grapheme, copy rowCopy) bool

// rowFill is the row being filled while a line wraps, and how many rows it
// has handed on so far.
type rowFill struct {
	width              int
	continuationIndent int
	indent             grapheme
	current            []grapheme
	currentWidth       int
	emitted            int
	// brokeAtSpace is whether the latest break dropped whitespace, rather
	// than cutting a word.
	brokeAtSpace bool
}

// finishRow hands the current row on and leaves it empty.
func (f *rowFill) finishRow(buffer *styledBuffer, emit rowEmitter) bool {
	// Every row after a line's first is one that wrapping split off.
	rowCopyValue := rowCopy{}
	if f.emitted > 0 {
		joins := &rowCopyJoinCut
		if f.brokeAtSpace {
			joins = &rowCopyJoinSpace
		}
		rowCopyValue = rowCopy{lead: f.continuationIndent, joins: joins}
	}
	f.emitted++
	flow := emit(buffer, f.current, rowCopyValue)
	f.current = f.current[:0]
	return flow
}

func (f *rowFill) startContinuation() {
	f.current = f.current[:0]
	for range f.continuationIndent {
		f.current = append(f.current, f.indent)
	}
	f.currentWidth = f.continuationIndent
}

// place places one run of whitespace or non-whitespace graphemes, breaking
// on word boundaries and cutting a word only when it cannot fit a row on its
// own.
func (f *rowFill) place(token []grapheme, buffer *styledBuffer, emit rowEmitter) bool {
	panic("unported: mj-chat/src/chat/rendering.rs RowFill::place")
}

// wrapGraphemes wraps line, handing each finished row to emit until it
// returns false.
func wrapGraphemes(line *Line, width, continuationIndent int, emit rowEmitter) {
	panic("unported: mj-chat/src/chat/rendering.rs wrap_graphemes")
}

// trimTrailingWhitespace drops trailing whitespace, reporting whether there
// was any.
func trimTrailingWhitespace(graphemes *[]grapheme) bool {
	length := len(*graphemes)
	for n := len(*graphemes); n > 0 && (*graphemes)[n-1].whitespace; n = len(*graphemes) {
		*graphemes = (*graphemes)[:n-1]
	}
	return len(*graphemes) < length
}

// displayWidth is text's width in terminal cells, as the unicode-width
// crate's UnicodeWidthStr::width measures it.
func displayWidth(text string) int {
	panic("unported: mj-chat/src/chat/rendering.rs display_width")
}

func trimSpansBeforeEllipsis(spans *[]Span, preservedSpans int) {
	for len(*spans) > preservedSpans {
		last := &(*spans)[len(*spans)-1]
		trimmed := strings.TrimRightFunc(last.Content, trimBeforeEllipsis)
		if trimmed == "" {
			*spans = (*spans)[:len(*spans)-1]
		} else {
			last.Content = trimmed
			break
		}
	}
}

func appendTrimmedEllipsis(line *Line, preservedSpans int) {
	style := Style{}
	if n := len(line.Spans); n > 0 {
		style = line.Spans[n-1].Style
	}
	trimSpansBeforeEllipsis(&line.Spans, preservedSpans)
	line.Spans = append(line.Spans, StyledSpan(ThemeGlyphs().Ellipsis, style))
}

// TruncateLineToWidth truncates a styled line to width terminal cells,
// keeping each span's style and marking the cut with `…` in the style of the
// span it landed in.
func TruncateLineToWidth(line Line, width int) Line {
	panic("unported: mj-chat/src/chat/rendering.rs truncate_line_to_width")
}

func truncateToWidth(text string, width int) string {
	return TruncateLineToWidth(RawLine(text), width).String()
}
