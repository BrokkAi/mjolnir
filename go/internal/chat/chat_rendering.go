// Port of mj-chat/src/chat/rendering.rs.
//
// Markdown and width-aware transcript rendering.
package chat

import (
	"bytes"
	"fmt"
	"iter"
	"math"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"
	"github.com/yuin/goldmark"
	"github.com/yuin/goldmark/ast"
	"github.com/yuin/goldmark/extension"
	extast "github.com/yuin/goldmark/extension/ast"
	mdtext "github.com/yuin/goldmark/text"
	"github.com/yuin/goldmark/util"

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
	if w.link != nil {
		w.pushText(text, style)
		return
	}
	for _, part := range linkifySpans(text) {
		if part.link {
			url := part.text
			w.link = &url
			w.pushText(part.text, style.Add(ModifierUnderlined))
			w.link = nil
		} else {
			w.pushText(part.text, style)
		}
	}
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
	quote := strings.Repeat("> ", w.quoteDepth)
	item := ""
	if w.itemPrefix != nil {
		item = *w.itemPrefix
		w.itemPrefix = nil
	}
	continuationIndent := displayWidth(quote) + displayWidth(item)
	spans := make([]Span, 0, len(w.spans)+2)
	if quote != "" {
		spans = append(spans, StyledSpan(quote, Style{}.Foreground(ThemePalette().Muted)))
	}
	if item != "" {
		spans = append(spans, StyledSpan(item, Style{}.Foreground(ThemePalette().Muted)))
	}
	prefixSpans := len(spans)
	spans = append(spans, w.spans...)
	w.spans = nil
	var links []linkSpan
	for _, link := range w.lineLinks {
		links = append(links, linkSpan{span: link.span + prefixSpans, url: link.url})
	}
	w.lineLinks = nil
	w.lines = append(w.lines, logicalLine{line: LineFrom(spans...), continuationIndent: continuationIndent, links: links})
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

const (
	tableCellPadding    = 1
	tableColumnGap      = 2
	tableMinColumnWidth = 3
)

func (w *markdownWriter) renderTable(table tableState) {
	if !table.cell.isEmpty() || len(table.row) > 0 {
		table.row = append(table.row, table.cell)
		table.cell = tableCell{}
	}
	if len(table.row) > 0 {
		table.rows = append(table.rows, table.row)
		table.row = nil
	}
	if len(table.rows) == 0 {
		return
	}
	header := make([]string, len(table.rows[0]))
	for i, cell := range table.rows[0] {
		header[i] = cell.text()
	}
	columns := max(len(table.alignments), len(header), 1)
	for i, row := range table.rows {
		row = row[:min(len(row), columns)]
		for len(row) < columns {
			row = append(row, tableCell{})
		}
		table.rows[i] = row
	}
	for len(table.alignments) < columns {
		table.alignments = append(table.alignments, tableAlignmentNone)
	}
	table.alignments = table.alignments[:columns]
	texts := make([][]string, len(table.rows))
	for i, row := range table.rows {
		texts[i] = make([]string, len(row))
		for j, cell := range row {
			texts[i][j] = cell.text()
		}
	}

	reserved := columns*tableCellPadding*2 + max(columns-1, 0)*tableColumnGap
	available := max(w.width-reserved, 0)
	columnWidths := make([]int, columns)
	for column := range columns {
		width := 0
		for _, row := range texts {
			width = max(width, displayWidth(row[column]))
		}
		columnWidths[column] = max(width, tableMinColumnWidth)
	}
	sum := func() int {
		total := 0
		for _, width := range columnWidths {
			total += width
		}
		return total
	}
	for sum() > available {
		// The widest column above the minimum gives up a cell; Rust's
		// max_by_key keeps the last of equal widths.
		widest := -1
		for column, width := range columnWidths {
			if width > tableMinColumnWidth && (widest < 0 || width >= columnWidths[widest]) {
				widest = column
			}
		}
		if widest < 0 {
			break
		}
		columnWidths[widest]--
	}
	gridFits := sum() <= available
	fragmentsTokens := false
	for _, row := range texts[1:] {
		for column, cell := range row {
			width := columnWidths[column]
			if width >= 12 {
				continue
			}
			for _, token := range strings.Fields(cell) {
				if displayWidth(token) > width {
					fragmentsTokens = true
				}
			}
		}
	}
	if gridFits && !fragmentsTokens {
		rows := table.rows
		w.renderTableRow(rows[0], columnWidths, table.alignments, Style{}.Add(ModifierBold))
		separators := make([]string, columns)
		for i, width := range columnWidths {
			separators[i] = strings.Repeat(ThemeGlyphs().Rule, width+tableCellPadding*2)
		}
		w.lines = append(w.lines, logicalLine{
			line: LineFrom(StyledSpan(strings.Join(separators, strings.Repeat(" ", tableColumnGap)), Style{}.Foreground(ThemePalette().Muted))),
		})
		for _, row := range rows[1:] {
			w.renderTableRow(row, columnWidths, table.alignments, Style{})
		}
		return
	}
	for rowIndex, row := range table.rows[1:] {
		if rowIndex > 0 {
			w.lines = append(w.lines, logicalLine{
				line: LineFrom(StyledSpan("────────────────────", Style{}.Foreground(ThemePalette().Muted))),
			})
		}
		for column, value := range row {
			label := fmt.Sprintf("Column %d", column+1)
			if column < len(header) && header[column] != "" {
				label = header[column]
			}
			spans := append([]Span{StyledSpan(label+": ", Style{}.Add(ModifierBold))}, value.spans...)
			var links []linkSpan
			for _, link := range value.links {
				links = append(links, linkSpan{span: link.span + 1, url: link.url})
			}
			w.lines = append(w.lines, logicalLine{
				line:               LineFrom(spans...),
				continuationIndent: displayWidth(label) + 2,
				links:              links,
			})
		}
	}
}

func (w *markdownWriter) renderTableRow(row []tableCell, columnWidths []int, alignments []tableAlignment, emphasis Style) {
	type wrappedCell struct {
		rows    []Line
		sources cellSources
	}
	cells := make([]wrappedCell, 0, min(len(row), len(columnWidths)))
	for column := range min(len(row), len(columnWidths)) {
		spans := make([]Span, len(row[column].spans))
		for i, span := range row[column].spans {
			spans[i] = StyledSpan(span.Content, span.Style.Patch(emphasis))
		}
		rows, sources := wrapStyledLineWithSources(LineFrom(spans...), columnWidths[column], 0)
		cells = append(cells, wrappedCell{rows: rows, sources: sources})
	}
	height := 1
	if len(cells) > 0 {
		height = 0
		for _, cell := range cells {
			height = max(height, len(cell.rows))
		}
	}
	for lineIndex := range height {
		var spans []Span
		var links []linkSpan
		for column, width := range columnWidths {
			if column > 0 {
				spans = append(spans, RawSpan(strings.Repeat(" ", tableColumnGap)))
			}
			cell := cells[column]
			var value Line
			if lineIndex < len(cell.rows) {
				value = cell.rows[lineIndex]
			}
			remaining := max(width-value.Width(), 0)
			left, right := 0, remaining
			switch alignments[column] {
			case tableAlignmentCenter:
				left, right = remaining/2, remaining-remaining/2
			case tableAlignmentRight:
				left, right = remaining, 0
			}
			spans = append(spans, RawSpan(strings.Repeat(" ", tableCellPadding+left)))
			var sources []int
			if lineIndex < len(cell.sources) {
				sources = cell.sources[lineIndex]
			}
			offset := 0
			for _, span := range value.Spans {
				// Each wrapped span comes from one source span of the cell.
				source := noCellSource
				if offset < len(sources) {
					source = sources[offset]
				}
				for _, link := range row[column].links {
					if source != noCellSource && link.span == source {
						links = append(links, linkSpan{span: len(spans), url: link.url})
						break
					}
				}
				offset += displayWidth(span.Content)
				spans = append(spans, span)
			}
			spans = append(spans, RawSpan(strings.Repeat(" ", right+tableCellPadding)))
		}
		w.lines = append(w.lines, logicalLine{line: LineFrom(spans...), links: links})
	}
}

// sanitizeTerminalText is re-exported from core's transcript.
func sanitizeTerminalText(text string) string { return core.SanitizeTerminalText(text) }

func markdownLines(source string, bodyStyle, accentStyle Style, width int) []logicalLine {
	writer := newMarkdownWriter(width, bodyStyle)
	var styleStack []Style
	pop := func() {
		if n := len(styleStack); n > 0 {
			writer.style = styleStack[n-1]
			styleStack = styleStack[:n-1]
		} else {
			writer.style = bodyStyle
		}
	}
	push := func(style Style) {
		styleStack = append(styleStack, writer.style)
		writer.style = style
	}
	walkMarkdown(source, func(event markdownEvent) {
		if event.kind == markdownText {
			writer.pendingText += event.text
			return
		}
		writer.flushText()
		switch event.kind {
		case markdownStart:
			switch event.tag {
			case markdownTagHeading:
				writer.spans = append(writer.spans, StyledSpan(strings.Repeat("#", event.level)+" ", Style{}.Foreground(ThemePalette().Muted)))
				push(accentStyle.Add(ModifierBold))
			case markdownTagBlockQuote:
				writer.quoteDepth++
			case markdownTagCodeBlock:
				writer.finishBlock()
				language := "code"
				if event.fenced && event.text != "" {
					language = "code · " + event.text
				}
				writer.lines = append(writer.lines, logicalLine{
					line: LineFrom(StyledSpan(language, Style{}.Foreground(ThemePalette().Muted).Add(ModifierBold))),
				})
				push(Style{}.Foreground(ThemePalette().Text).Patch(Raised()))
			case markdownTagList:
				writer.lists = append(writer.lists, listState{next: event.start})
			case markdownTagItem:
				depth := max(len(writer.lists)-1, 0)
				marker := "• "
				if n := len(writer.lists); n > 0 && writer.lists[n-1].next != nil {
					next := writer.lists[n-1].next
					marker = fmt.Sprintf("%d. ", *next)
					*next++
				}
				prefix := strings.Repeat("  ", depth) + marker
				writer.itemPrefix = &prefix
			case markdownTagEmphasis:
				push(writer.style.Add(ModifierItalic))
			case markdownTagStrong:
				push(writer.style.Add(ModifierBold))
			case markdownTagStrikethrough:
				push(writer.style.Add(ModifierCrossedOut))
			case markdownTagLink:
				url := event.text
				writer.link = &url
				push(writer.style.Foreground(ThemePalette().Text).Add(ModifierUnderlined))
			case markdownTagTable:
				writer.table = &tableState{alignments: event.alignments}
			}
		case markdownEnd:
			switch event.tag {
			case markdownTagParagraph:
				writer.finishBlock()
			case markdownTagHeading:
				pop()
				writer.finishBlock()
			case markdownTagBlockQuote:
				writer.finishBlock()
				writer.quoteDepth = max(writer.quoteDepth-1, 0)
			case markdownTagCodeBlock:
				if len(writer.spans) > 0 {
					writer.finishLine()
				}
				pop()
				writer.finishBlock()
			case markdownTagList:
				writer.finishBlock()
				if n := len(writer.lists); n > 0 {
					writer.lists = writer.lists[:n-1]
				}
			case markdownTagItem:
				writer.finishBlock()
			case markdownTagEmphasis, markdownTagStrong, markdownTagStrikethrough:
				pop()
			case markdownTagLink:
				writer.link = nil
				pop()
			case markdownTagTableCell:
				if writer.table != nil {
					writer.table.row = append(writer.table.row, writer.table.cell)
					writer.table.cell = tableCell{}
				}
			case markdownTagTableHead, markdownTagTableRow:
				if writer.table != nil {
					writer.table.rows = append(writer.table.rows, writer.table.row)
					writer.table.row = nil
				}
			case markdownTagTable:
				if table := writer.table; table != nil {
					writer.table = nil
					writer.renderTable(*table)
				}
				writer.finishBlock()
			}
		case markdownCode:
			writer.pushLinkified(event.text, writer.style.Foreground(ThemePalette().Secondary).Patch(Raised()))
		case markdownBreak:
			writer.finishLine()
		case markdownRule:
			writer.finishBlock()
			writer.lines = append(writer.lines, logicalLine{
				line: LineFrom(StyledSpan("────────────────────", Style{}.Foreground(ThemePalette().Muted))),
			})
		}
		// Rich mode intentionally does not interpret or display raw HTML. The
		// raw transcript mode still exposes it as sanitized source when
		// needed. Footnotes and task lists are not enabled, as in Rust.
	})
	writer.flushText()
	return writer.finish()
}

// markdownEventKind is the kind of one pulldown-cmark event.
type markdownEventKind uint8

const (
	markdownStart markdownEventKind = iota
	markdownEnd
	markdownText
	markdownCode
	// markdownBreak is a soft or hard line break.
	markdownBreak
	markdownRule
)

// markdownTag is the element a start or end event opens or closes.
type markdownTag uint8

const (
	markdownTagParagraph markdownTag = iota
	markdownTagHeading
	markdownTagBlockQuote
	markdownTagCodeBlock
	markdownTagList
	markdownTagItem
	markdownTagEmphasis
	markdownTagStrong
	markdownTagStrikethrough
	markdownTagLink
	markdownTagImage
	markdownTagTable
	markdownTagTableHead
	markdownTagTableRow
	markdownTagTableCell
	// markdownTagOther is an element the transcript does not render.
	markdownTagOther
)

// markdownEvent is one event of the stream pulldown-cmark produces for a
// document: the transcript's renderer is written against that stream.
type markdownEvent struct {
	kind markdownEventKind
	tag  markdownTag
	// text is a text or code event's text, a link's destination, or a fenced
	// code block's info string.
	text       string
	level      int
	fenced     bool
	start      *uint64
	alignments []tableAlignment
}

// markdownParser parses CommonMark with GitHub's tables and strikethrough,
// the two extensions the Rust renderer enables in pulldown-cmark.
var markdownParser = goldmark.New(goldmark.WithExtensions(extension.Table, extension.Strikethrough)).Parser()

// walkMarkdown parses source and hands emit the events pulldown-cmark
// produces for it. goldmark builds a tree; the walk turns it into the same
// start, end and text events. goldmark keeps backslash escapes and entity
// references in its text and resolves them as it renders; this resolves them
// as pulldown-cmark does before handing the text on.
func walkMarkdown(source string, emit func(markdownEvent)) {
	data := []byte(source)
	document := markdownParser.Parse(mdtext.NewReader(data))
	var walk func(node ast.Node)
	children := func(node ast.Node) {
		for child := node.FirstChild(); child != nil; child = child.NextSibling() {
			walk(child)
		}
	}
	element := func(node ast.Node, event markdownEvent) {
		event.kind = markdownStart
		emit(event)
		children(node)
		emit(markdownEvent{kind: markdownEnd, tag: event.tag})
	}
	walk = func(node ast.Node) {
		switch node := node.(type) {
		case *ast.Document, *ast.TextBlock:
			// Tight list items hold their text without a paragraph, as
			// pulldown-cmark emits no paragraph for them.
			children(node)
		case *ast.Paragraph:
			element(node, markdownEvent{tag: markdownTagParagraph})
		case *ast.Heading:
			element(node, markdownEvent{tag: markdownTagHeading, level: node.Level})
		case *ast.Blockquote:
			element(node, markdownEvent{tag: markdownTagBlockQuote})
		case *ast.CodeBlock:
			emit(markdownEvent{kind: markdownStart, tag: markdownTagCodeBlock})
			if code := markdownLinesText(node, data); code != "" {
				emit(markdownEvent{kind: markdownText, text: code})
			}
			emit(markdownEvent{kind: markdownEnd, tag: markdownTagCodeBlock})
		case *ast.FencedCodeBlock:
			info := ""
			if node.Info != nil {
				info = markdownUnescape(node.Info.Segment.Value(data))
			}
			emit(markdownEvent{kind: markdownStart, tag: markdownTagCodeBlock, fenced: true, text: info})
			if code := markdownLinesText(node, data); code != "" {
				emit(markdownEvent{kind: markdownText, text: code})
			}
			emit(markdownEvent{kind: markdownEnd, tag: markdownTagCodeBlock})
		case *ast.List:
			var start *uint64
			if node.IsOrdered() {
				value := uint64(node.Start)
				start = &value
			}
			element(node, markdownEvent{tag: markdownTagList, start: start})
		case *ast.ListItem:
			element(node, markdownEvent{tag: markdownTagItem})
		case *ast.ThematicBreak:
			emit(markdownEvent{kind: markdownRule})
		case *ast.HTMLBlock, *ast.RawHTML:
		case *ast.Text:
			value := node.Segment.Value(data)
			if !node.IsRaw() {
				value = []byte(markdownUnescape(value))
			}
			if len(value) > 0 {
				emit(markdownEvent{kind: markdownText, text: string(value)})
			}
			if node.HardLineBreak() || node.SoftLineBreak() {
				emit(markdownEvent{kind: markdownBreak})
			}
		case *ast.String:
			value := node.Value
			if !node.IsRaw() && !node.IsCode() {
				value = []byte(markdownUnescape(value))
			}
			if len(value) > 0 {
				emit(markdownEvent{kind: markdownText, text: string(value)})
			}
		case *ast.CodeSpan:
			var code strings.Builder
			for child := node.FirstChild(); child != nil; child = child.NextSibling() {
				var value []byte
				switch child := child.(type) {
				case *ast.Text:
					value = child.Segment.Value(data)
				case *ast.String:
					value = child.Value
				}
				// A line ending inside a code span reads as a space.
				if trimmed, ok := bytes.CutSuffix(value, []byte("\n")); ok {
					code.Write(trimmed)
					code.WriteByte(' ')
				} else {
					code.Write(value)
				}
			}
			emit(markdownEvent{kind: markdownCode, text: code.String()})
		case *ast.Emphasis:
			tag := markdownTagEmphasis
			if node.Level >= 2 {
				tag = markdownTagStrong
			}
			element(node, markdownEvent{tag: tag})
		case *ast.Link:
			element(node, markdownEvent{tag: markdownTagLink, text: markdownUnescape(node.Destination)})
		case *ast.AutoLink:
			label := string(node.Label(data))
			emit(markdownEvent{kind: markdownStart, tag: markdownTagLink, text: label})
			emit(markdownEvent{kind: markdownText, text: label})
			emit(markdownEvent{kind: markdownEnd, tag: markdownTagLink})
		case *ast.Image:
			element(node, markdownEvent{tag: markdownTagImage})
		case *extast.Strikethrough:
			element(node, markdownEvent{tag: markdownTagStrikethrough})
		case *extast.Table:
			alignments := make([]tableAlignment, len(node.Alignments))
			for i, alignment := range node.Alignments {
				switch alignment {
				case extast.AlignLeft:
					alignments[i] = tableAlignmentLeft
				case extast.AlignCenter:
					alignments[i] = tableAlignmentCenter
				case extast.AlignRight:
					alignments[i] = tableAlignmentRight
				}
			}
			element(node, markdownEvent{tag: markdownTagTable, alignments: alignments})
		case *extast.TableHeader:
			element(node, markdownEvent{tag: markdownTagTableHead})
		case *extast.TableRow:
			element(node, markdownEvent{tag: markdownTagTableRow})
		case *extast.TableCell:
			element(node, markdownEvent{tag: markdownTagTableCell})
		default:
			element(node, markdownEvent{tag: markdownTagOther})
		}
	}
	walk(document)
}

// linkifyPart is a run of text that is or is not a URL (linkify's Span).
type linkifyPart struct {
	text string
	link bool
}

// linkifySpans splits text into URLs and the text between them, as the
// linkify crate's LinkFinder does with only URL links enabled: a URL needs a
// scheme, may be an IRI, and is found from each `:` in the text.
func linkifySpans(text string) []linkifyPart {
	var parts []linkifyPart
	position, rewind := 0, 0
	for {
		start, end, ok := linkifyNextURL(text, rewind)
		if !ok {
			break
		}
		if position < start {
			parts = append(parts, linkifyPart{text: text[position:start]})
		}
		parts = append(parts, linkifyPart{text: text[start:end], link: true})
		position, rewind = end, end
	}
	if position < len(text) {
		parts = append(parts, linkifyPart{text: text[position:]})
	}
	return parts
}

// linkifyNextURL is the next URL in text after offset rewind.
func linkifyNextURL(text string, rewind int) (int, int, bool) {
	slice := text[rewind:]
	for findFrom := 0; ; {
		i := strings.IndexByte(slice[findFrom:], ':')
		if i < 0 {
			return 0, 0, false
		}
		if start, end, ok := linkifyScanURL(slice, findFrom+i); ok {
			return rewind + start, rewind + end, true
		}
		findFrom += i + 1
	}
}

// linkifyScanURL is linkify's UrlScanner::scan for the `:` at separator.
func linkifyScanURL(s string, separator int) (int, int, bool) {
	if separator == 0 || !strings.HasPrefix(s[separator:], "://") {
		return 0, 0, false
	}
	afterSeparator := separator + len("://")
	if afterSeparator >= len(s) {
		return 0, 0, false
	}
	start, quote, ok := linkifySchemeStart(s[:separator])
	if !ok {
		return 0, 0, false
	}
	scheme := s[start:separator]
	rest := s[afterSeparator:]
	requireHost := scheme == "https" || scheme == "http" || scheme == "ftp" || scheme == "ssh"
	afterAuthority, ok := linkifyAuthorityEnd(rest, requireHost)
	if !ok {
		return 0, 0, false
	}
	end, ok := linkifyURLEnd(rest[afterAuthority:], quote)
	if !ok || (afterAuthority == 0 && end == 0) {
		return 0, 0, false
	}
	return start, afterSeparator + afterAuthority + end, true
}

// linkifySchemeStart finds where the scheme before a `://` begins, and the
// quote before it, if any (linkify's find_scheme_start).
func linkifySchemeStart(s string) (int, rune, bool) {
	first, special := -1, -1
	var quote rune
scan:
	for i := len(s); i > 0; {
		c, size := utf8.DecodeLastRuneInString(s[:i])
		i -= size
		switch {
		case c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z':
			first = i
		case c >= '0' && c <= '9':
			special = i
		case c == '+' || c == '-' || c == '.':
		case c == '@':
			return 0, 0, false
		case c == '\'' || c == '"':
			quote = c
			break scan
		default:
			break scan
		}
	}
	if first >= 0 && special >= 0 && first > 0 && first-1 == special {
		return 0, quote, false
	}
	return first, quote, first >= 0
}

// linkifyAuthorityEnd finds where the authority of a URL ends (linkify's
// find_authority_end with userinfo and ports allowed, IRIs enabled).
func linkifyAuthorityEnd(s string, requireHost bool) (int, bool) {
	userinfoAllowed := true
	end := 0
	maybeLastDot, lastDot := -1, -1
	numberDots := 0
	dotAllowed, hyphenAllowed := false, false
	allNumeric, maybeHost, hostEnded := true, true, false
scan:
	for i, c := range s {
		canBeLast := false
		switch {
		case c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= 0x80:
			// Can start or end a domain label, but not numeric.
			dotAllowed, hyphenAllowed = true, true
			lastDot = maybeLastDot
			allNumeric = false
			if hostEnded {
				maybeHost = false
			}
			canBeLast = !requireHost || !hostEnded
		case c >= '0' && c <= '9':
			dotAllowed, hyphenAllowed = true, true
			if lastDot != maybeLastDot {
				lastDot = maybeLastDot
				numberDots++
			}
			if hostEnded {
				maybeHost = false
			}
			canBeLast = !requireHost || !hostEnded
		case c == '-':
			// A hyphen cannot start or end a label.
			if !hyphenAllowed {
				maybeHost = false
			}
			dotAllowed = false
			allNumeric = false
			canBeLast = !requireHost
		case c == '.':
			if !dotAllowed {
				// Labels cannot be empty.
				hostEnded = true
			}
			dotAllowed, hyphenAllowed = false, false
			maybeLastDot = i
		case c == '_' || c == '~':
			maybeHost = false
		case strings.ContainsRune("!$&'()*+,;=", c):
			// Not in host names, but treated as delimiters.
			hostEnded = true
			if !userinfoAllowed && requireHost {
				break scan
			}
		case c == ':':
			// Userinfo, or a port; the last dot does not advance into ports.
			maybeLastDot = lastDot
		case c == '@':
			if !userinfoAllowed {
				return 0, false
			}
			// Everything before this was userinfo.
			userinfoAllowed = false
			maybeLastDot, lastDot = -1, -1
			dotAllowed, hyphenAllowed = false, false
			allNumeric, maybeHost, hostEnded = true, true, false
		case c == '/':
			if !requireHost {
				end = i
			}
			break scan
		default:
			break scan
		}
		if canBeLast {
			end = i + utf8.RuneLen(c)
		}
	}
	if !requireHost {
		return end, true
	}
	if !maybeHost {
		return 0, false
	}
	if allNumeric {
		// IPv4 addresses need four numbers.
		if numberDots != 3 {
			return 0, false
		}
	} else if lastDot >= 0 {
		// The top-level domain must start with two ASCII letters.
		letters := 0
		for _, c := range s[lastDot+1:] {
			if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z') || letters == 2 {
				break
			}
			letters++
		}
		if letters < 2 {
			return 0, false
		}
	}
	return end, true
}

// linkifyURLEnd finds where a URL's path, query and fragment end (linkify's
// find_url_end with IRIs enabled).
func linkifyURLEnd(s string, quote rune) (int, bool) {
	round, square, curly := 0, 0, 0
	singleQuote := false
	previousIsURLChar := true
	end := 0
	if !strings.HasPrefix(s, "/") && !strings.HasPrefix(s, "?") {
		return 0, true
	}
scan:
	for i, c := range s {
		canBeLast := true
		switch {
		case c <= 0x1F || c == ' ' || c == '|' || c == '"' || c == '<' || c == '>' || c == '`' || (c >= 0x7F && c <= 0x9F):
			break scan
		case strings.ContainsRune("?!.,:;*", c):
			canBeLast = false
		case c == '/':
			canBeLast = previousIsURLChar
		case c == '(':
			round++
			canBeLast = false
		case c == ')':
			round--
			if round < 0 {
				break scan
			}
		case c == '[':
			square++
			canBeLast = false
		case c == ']':
			square--
			if square < 0 {
				break scan
			}
		case c == '{':
			curly++
			canBeLast = false
		case c == '}':
			curly--
			if curly < 0 {
				break scan
			}
		case quote != 0 && c == quote:
			break scan
		case c == '\'':
			singleQuote = !singleQuote
			canBeLast = !singleQuote
		}
		if canBeLast {
			end = i + utf8.RuneLen(c)
		}
		// IRIs are enabled, so every character counts as a URL character.
		previousIsURLChar = true
	}
	return end, true
}

// markdownLinesText is a code block's text, its lines joined as written.
func markdownLinesText(node ast.Node, source []byte) string {
	var code strings.Builder
	lines := node.Lines()
	for i := range lines.Len() {
		segment := lines.At(i)
		code.Write(segment.Value(source))
	}
	return code.String()
}

// markdownUnescape resolves the backslash escapes and the entity and numeric
// character references of Markdown text, as goldmark's HTML writer does
// while rendering, without HTML-escaping the result.
func markdownUnescape(source []byte) string {
	var out strings.Builder
	escaped := false
	n := 0
	for i := 0; i < len(source); i++ {
		c := source[i]
		if escaped && util.IsPunct(c) {
			out.Write(source[n : i-1])
			n = i
			escaped = false
			continue
		}
		if c == 0 {
			out.Write(source[n:i])
			out.WriteRune(utf8.RuneError)
			n = i + 1
			escaped = false
			continue
		}
		if c == '&' {
			if value, end, ok := markdownCharacterReference(source, i); ok {
				out.Write(source[n:i])
				out.WriteString(value)
				n = end
				i = end - 1
				escaped = false
				continue
			}
		}
		escaped = c == '\\' && !escaped
	}
	out.Write(source[n:])
	return out.String()
}

// markdownCharacterReference reads the entity or numeric character
// reference at source[at] (which is `&`): its text and where it ends.
func markdownCharacterReference(source []byte, at int) (string, int, bool) {
	limit := len(source)
	next := at + 1
	if next < limit && source[next] == '#' {
		digits := next + 1
		if digits >= limit {
			return "", 0, false
		}
		base, start, isDigit, maxDigits := 10, digits, util.IsNumeric, 8
		if source[digits] == 'x' || source[digits] == 'X' {
			base, start, isDigit, maxDigits = 16, digits+1, util.IsHexDecimal, 7
		}
		end := start
		for end < limit && isDigit(source[end]) {
			end++
		}
		if end == start || end >= limit || source[end] != ';' || end-start >= maxDigits {
			return "", 0, false
		}
		value, _ := strconv.ParseUint(string(source[start:end]), base, 32)
		return string(util.ToValidRune(rune(value))), end + 1, true
	}
	end := next
	for end < limit && util.IsAlphaNumeric(source[end]) {
		end++
	}
	if end == next || end >= limit || source[end] != ';' {
		return "", 0, false
	}
	entity, ok := util.LookUpHTML5EntityByName(string(source[next:end]))
	if !ok {
		return "", 0, false
	}
	return string(entity.Characters), end + 1, true
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
	spanCount := len(line.Spans)
	var rows []Line
	wrapGraphemes(&line, width, continuationIndent, func(buffer *styledBuffer, row []grapheme, copy rowCopy) bool {
		if copies != nil {
			*copies = append(*copies, copy)
		}
		if sources != nil {
			// Style indexes past the line's spans belong to the indent.
			cells := []int{}
			for _, grapheme := range row {
				source := noCellSource
				if grapheme.style < spanCount {
					source = grapheme.style
				}
				for range grapheme.width {
					cells = append(cells, source)
				}
			}
			*sources = append(*sources, cells)
		}
		rows = append(rows, buffer.line(row))
		return true
	})
	return rows
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
	capacity := 0
	for _, span := range line.Spans {
		capacity += len(span.Content)
	}
	var text strings.Builder
	text.Grow(capacity + 1)
	styles := make([]Style, 0, len(line.Spans)+1)
	spans := make([][2]int, 0, len(line.Spans))
	for _, span := range line.Spans {
		styles = append(styles, line.Style.Patch(span.Style))
		base := text.Len()
		text.WriteString(span.Content)
		spans = append(spans, [2]int{base, text.Len()})
	}
	// Continuation indents reuse one space rather than allocating their own.
	space := text.Len()
	text.WriteByte(' ')
	indentStyle := Style{}
	if len(line.Spans) > 0 {
		indentStyle = line.Spans[0].Style
	}
	styles = append(styles, indentStyle)
	return &styledBuffer{text: text.String(), styles: styles, spans: spans, space: space}
}

// graphemes is the line's graphemes in order. Segmentation is the costly
// part of wrapping, so it goes only as far as the caller reads.
func (b *styledBuffer) graphemes() iter.Seq[grapheme] {
	return func(yield func(grapheme) bool) {
		for style, span := range b.spans {
			for offset := span[0]; offset < span[1]; {
				cluster, _ := ansi.FirstGraphemeCluster(b.text[offset:span[1]], ansi.GraphemeWidth)
				if cluster == "" {
					break
				}
				whitespace := true
				for _, character := range cluster {
					if !unicode.IsSpace(character) {
						whitespace = false
						break
					}
				}
				// Graphemes render as at most two columns.
				next := grapheme{
					start:      offset,
					end:        offset + len(cluster),
					style:      style,
					width:      uint8(min(displayWidth(cluster), math.MaxUint8)),
					whitespace: whitespace,
				}
				offset += len(cluster)
				if !yield(next) {
					return
				}
			}
		}
	}
}

func (b *styledBuffer) indent() grapheme {
	return grapheme{start: b.space, end: b.space + 1, style: len(b.styles) - 1, width: 1, whitespace: true}
}

// line joins row into spans, merging runs that share a style.
func (b *styledBuffer) line(row []grapheme) Line {
	var spans []Span
	style := -1
	var text strings.Builder
	for _, grapheme := range row {
		if style != grapheme.style {
			if style >= 0 {
				spans = append(spans, StyledSpan(text.String(), b.styles[style]))
				text.Reset()
			}
			style = grapheme.style
		}
		text.WriteString(b.text[grapheme.start:grapheme.end])
	}
	if style >= 0 {
		spans = append(spans, StyledSpan(text.String(), b.styles[style]))
	}
	return LineFrom(spans...)
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
	tokenWidth := 0
	for _, grapheme := range token {
		tokenWidth += int(grapheme.width)
	}
	isWhitespace := len(token) > 0 && token[0].whitespace
	switch {
	case f.currentWidth+tokenWidth <= f.width:
		f.current = append(f.current, token...)
		f.currentWidth += tokenWidth
	case isWhitespace:
		trimTrailingWhitespace(&f.current)
		if len(f.current) > 0 && !f.finishRow(buffer, emit) {
			return false
		}
		f.brokeAtSpace = true
		f.startContinuation()
	case tokenWidth+f.continuationIndent <= f.width:
		if len(f.current) > f.continuationIndent {
			trimmed := trimTrailingWhitespace(&f.current)
			if !f.finishRow(buffer, emit) {
				return false
			}
			f.brokeAtSpace = trimmed
		}
		f.startContinuation()
		f.current = append(f.current, token...)
		f.currentWidth += tokenWidth
	default:
		for _, grapheme := range token {
			graphemeWidth := int(grapheme.width)
			if f.currentWidth+graphemeWidth > f.width && len(f.current) > 0 {
				trimmed := trimTrailingWhitespace(&f.current)
				if !f.finishRow(buffer, emit) {
					return false
				}
				f.brokeAtSpace = trimmed
				f.startContinuation()
			}
			f.current = append(f.current, grapheme)
			f.currentWidth += graphemeWidth
		}
	}
	return true
}

// wrapGraphemes wraps line, handing each finished row to emit until it
// returns false.
func wrapGraphemes(line *Line, width, continuationIndent int, emit rowEmitter) {
	width = max(width, 1)
	continuationIndent = min(continuationIndent, max(width-1, 0))
	buffer := newStyledBuffer(line)
	fill := rowFill{width: width, continuationIndent: continuationIndent, indent: buffer.indent()}
	// Rows are filled a run of whitespace or non-whitespace graphemes at a
	// time, each run as soon as the next one begins.
	var token []grapheme
	for grapheme := range buffer.graphemes() {
		if len(token) > 0 && token[0].whitespace != grapheme.whitespace {
			if !fill.place(token, buffer, emit) {
				return
			}
			token = token[:0]
		}
		token = append(token, grapheme)
	}
	if len(token) > 0 && !fill.place(token, buffer, emit) {
		return
	}
	trimTrailingWhitespace(&fill.current)
	if len(fill.current) > 0 || fill.emitted == 0 {
		// The last row; there is nothing left for a break to stop.
		fill.finishRow(buffer, emit)
	}
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
//
// Clusters are measured with x/ansi's grapheme widths, corrected where
// unicode-width measures differently: a control character counts one cell,
// as unicode-width counts it inside a string ("\r\n" counts one), where
// x/ansi would count none; and U+FE0F widens only a character that starts an
// emoji presentation sequence, where x/ansi widens any character before it.
func displayWidth(text string) int {
	width := 0
	for rest := text; rest != ""; {
		cluster, clusterWidth := ansi.FirstGraphemeCluster(rest, ansi.GraphemeWidth)
		if cluster == "" {
			break
		}
		rest = rest[len(cluster):]
		if first, _ := utf8.DecodeRuneInString(cluster); isControl(first) {
			if cluster == "\r\n" {
				width++
			} else {
				width += utf8.RuneCountInString(cluster)
			}
			continue
		}
		if at := strings.IndexRune(cluster, '️'); at > 0 {
			base, _ := utf8.DecodeLastRuneInString(cluster[:at])
			if !startsEmojiPresentationSeq(base) {
				// The selector is ignored: measure the cluster without it.
				_, clusterWidth = ansi.FirstGraphemeCluster(strings.ReplaceAll(cluster, "️", ""), ansi.GraphemeWidth)
			}
		}
		width += clusterWidth
	}
	return width
}

// startsEmojiPresentationSeq reports whether c forms an emoji presentation
// sequence when followed by U+FE0F (unicode-width's
// starts_emoji_presentation_seq, with its table).
func startsEmojiPresentationSeq(c rune) bool {
	var leaf int
	switch c >> 10 {
	case 0x0:
		leaf = 0
	case 0x8:
		leaf = 1
	case 0x9:
		leaf = 2
	case 0xA:
		leaf = 3
	case 0xC:
		leaf = 4
	case 0x7C:
		leaf = 5
	case 0x7D:
		leaf = 6
	default:
		return false
	}
	return emojiPresentationLeaves[leaf][(c>>3)&0x7F]>>(c&7)&1 == 1
}

// emojiPresentationLeaves is unicode-width 0.2.2's EMOJI_PRESENTATION_LEAVES
// (Unicode 16): one bit per code point that starts an emoji presentation
// sequence.
var emojiPresentationLeaves = [7][128]uint8{
	{
		0, 0, 0, 0, 8, 4, 255, 3, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	},
	{
		0, 0, 0, 0, 0, 0, 0, 16, 0, 2, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 4, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 240, 3, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 12, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 0, 0, 0, 254, 15, 7,
	},
	{
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 12, 64, 0, 1, 0, 0, 0, 0, 0, 0, 120,
		31, 64, 50, 33, 77, 196, 0, 7, 5, 255, 15, 128, 105, 1, 0, 200,
		0, 0, 252, 26, 131, 12, 3, 96, 48, 193, 26, 0, 0, 6, 191, 39,
		36, 191, 84, 32, 2, 1, 24, 0, 144, 80, 184, 0, 24, 0, 0, 0,
		0, 0, 224, 0, 2, 0, 1, 128, 0, 0, 0, 0, 0, 0, 0, 0,
	},
	{
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 48, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		224, 0, 0, 24, 0, 0, 0, 0, 0, 0, 33, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	},
	{
		0, 0, 0, 0, 0, 0, 1, 32, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 128, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	},
	{
		16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 192,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		4, 0, 0, 4, 0, 128, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 224, 32, 16, 242, 31, 64, 0, 0, 0, 0, 0, 0, 0, 0, 33,
		0, 0, 200, 206, 128, 112, 0, 0, 84, 124, 240, 255, 1, 32, 168, 0,
	},
	{
		0, 1, 32, 128, 64, 0, 0, 128, 198, 99, 8, 0, 0, 4, 0, 32,
		0, 0, 0, 0, 8, 0, 9, 136, 0, 8, 0, 132, 112, 60, 128, 46,
		0, 33, 12, 0, 0, 0, 0, 0, 0, 6, 255, 255, 255, 128, 249, 3,
		128, 60, 1, 0, 32, 1, 6, 16, 28, 0, 14, 112, 10, 129, 8, 4,
		0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		128, 32, 18, 1, 0, 32, 4, 22, 0, 232, 0, 0, 63, 2, 9, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	},
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
	total := 0
	for _, span := range line.Spans {
		total += displayWidth(span.Content)
	}
	if total <= width {
		return line
	}
	budget := max(width-1, 0)
	var spans []Span
	used := 0
	for _, span := range line.Spans {
		if used >= budget {
			if width > 0 {
				trimSpansBeforeEllipsis(&spans, 0)
				spans = append(spans, StyledSpan(ThemeGlyphs().Ellipsis, span.Style))
			}
			return LineFrom(spans...)
		}
		count := displayWidth(span.Content)
		if used+count <= budget {
			used += count
			spans = append(spans, span)
			continue
		}
		var kept strings.Builder
		for _, grapheme := range splitGraphemes(span.Content) {
			next := used + displayWidth(grapheme.text)
			if next > budget {
				break
			}
			used = next
			kept.WriteString(grapheme.text)
		}
		if kept.Len() > 0 {
			spans = append(spans, StyledSpan(kept.String(), span.Style))
		}
		if width > 0 {
			trimSpansBeforeEllipsis(&spans, 0)
			spans = append(spans, StyledSpan(ThemeGlyphs().Ellipsis, span.Style))
		}
		return LineFrom(spans...)
	}
	return LineFrom(spans...)
}

func truncateToWidth(text string, width int) string {
	return TruncateLineToWidth(RawLine(text), width).String()
}
