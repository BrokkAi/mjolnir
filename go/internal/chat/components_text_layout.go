// Port of mj-chat/src/components/text_layout.rs.

package chat

import (
	"strings"
	"unicode"

	uv "github.com/charmbracelet/ultraviolet"
	"github.com/clipperhouse/uax29/v2/graphemes"
)

// Word-wrapped text layout shared by multiline controls and the composer.

// graphemeIndex is one grapheme cluster of a string and its byte offset
// (an item of unicode-segmentation's `grapheme_indices(true)`).
type graphemeIndex struct {
	start int
	text  string
}

// componentGraphemeIndices splits text into extended grapheme clusters with
// their byte offsets, as unicode-segmentation's `grapheme_indices(true)`
// does.
func componentGraphemeIndices(text string) []graphemeIndex {
	var clusters []graphemeIndex
	iterator := graphemes.FromString(text)
	for iterator.Next() {
		clusters = append(clusters, graphemeIndex{start: iterator.Start(), text: iterator.Value()})
	}
	return clusters
}

// componentGraphemes splits text into extended grapheme clusters, as
// unicode-segmentation's `graphemes(true)` does.
func componentGraphemes(text string) []string {
	var clusters []string
	iterator := graphemes.FromString(text)
	for iterator.Next() {
		clusters = append(clusters, iterator.Value())
	}
	return clusters
}

// charWidth is unicode-width's `UnicodeWidthChar::width(..).unwrap_or(0)`:
// control characters have no width.
func charWidth(character rune) int {
	if unicode.IsControl(character) {
		return 0
	}
	return displayWidth(string(character))
}

// WrapLines wraps display text with the same wrapping for measurement and
// rendering.
func WrapLines(lines []Line, width int) []Line {
	var wrapped []Line
	for _, line := range lines {
		spans := make([]Span, 0, len(line.Spans))
		for _, span := range line.Spans {
			spans = append(spans, StyledSpan(span.Content, line.Style.Patch(span.Style)))
		}
		wrapped = append(wrapped, WrapStyledLine(LineFrom(spans...), width, 0)...)
	}
	return wrapped
}

// inputGrapheme is one grapheme of an input value: its byte range, display
// width, and whether it counts as whitespace for word wrapping.
type inputGrapheme struct {
	start      int
	end        int
	width      int
	whitespace bool
}

// visualRow is one wrapped row of an input value and the byte offset it
// starts at.
type visualRow struct {
	start     int
	graphemes []inputGrapheme
}

// Truncate says how [TruncateToCells] treats the text it keeps.
type Truncate struct {
	// CollapseWhitespace folds every run of whitespace, newlines included,
	// into one space before measuring, so a multi-line value reads as one
	// row.
	CollapseWhitespace bool
	// TrimPunctuation drops trailing whitespace and punctuation before the
	// ellipsis, so a cut in the middle of a phrase does not read as
	// `alpha,…`.
	TrimPunctuation bool
}

var (
	// TruncatePlain cuts the text where it runs out of room and keeps
	// everything before it.
	TruncatePlain = Truncate{CollapseWhitespace: false, TrimPunctuation: false}
	// TruncateSummary folds the value onto one row and cuts it where a
	// reader would: no dangling comma, dash, or space in front of the
	// ellipsis.
	TruncateSummary = Truncate{CollapseWhitespace: true, TrimPunctuation: true}
)

// trimBeforeEllipsis reports the characters a cut swallows before its
// ellipsis: whitespace, ASCII punctuation, and the typographic marks that
// read as punctuation.
func trimBeforeEllipsis(character rune) bool {
	if unicode.IsSpace(character) || isASCIIPunctuation(character) {
		return true
	}
	switch character {
	case '…', '–', '—', '‘', '’', '“', '”', '•', '·':
		return true
	}
	return false
}

// isASCIIPunctuation is Rust's `char::is_ascii_punctuation`.
func isASCIIPunctuation(character rune) bool {
	return (character >= '!' && character <= '/') ||
		(character >= ':' && character <= '@') ||
		(character >= '[' && character <= '`') ||
		(character >= '{' && character <= '~')
}

// TruncateToCells shortens text to at most width terminal cells, marking a
// cut with a trailing ellipsis.
//
// Width is measured in display cells, so a full-width glyph costs two. Text
// that already fits comes back unchanged apart from the requested
// whitespace collapsing. A width of zero yields an empty string and a width
// of one yields the ellipsis alone, because nothing else fits beside it.
func TruncateToCells(text string, width int, options Truncate) string {
	if options.CollapseWhitespace {
		text = strings.Join(strings.Fields(text), " ")
	}
	if displayWidth(text) <= width {
		return text
	}
	if width == 0 {
		return ""
	}
	ellipsis := ThemeGlyphs().Ellipsis
	ellipsisWidth := max(displayWidth(ellipsis), 1)
	if width <= ellipsisWidth {
		runes := []rune(ellipsis)
		return string(runes[:min(width, len(runes))])
	}
	// The ellipsis's own cells are reserved.
	budget := width - ellipsisWidth
	var kept strings.Builder
	used := 0
	for _, character := range text {
		cells := charWidth(character)
		if used+cells > budget {
			break
		}
		used += cells
		kept.WriteRune(character)
	}
	result := kept.String()
	if options.TrimPunctuation {
		result = strings.TrimRightFunc(result, trimBeforeEllipsis)
	}
	return result + ellipsis
}

// multilineRows returns the wrapped rows and the visual cursor position
// (column, row) for a text value.
//
// The wrapping policy mirrors ratatui's `WordWrapper` with `trim = false`,
// which is the policy used by the composer and multiline text fields.
func multilineRows(input string, cursor int, width int) (rows []visualRow, cursorColumn int, cursorRow int) {
	width = max(width, 1)
	lineOffset := 0
	for _, line := range strings.Split(input, "\n") {
		for _, wrapped := range wrapInputLine(line, lineOffset, width) {
			start := lineOffset
			if len(wrapped) > 0 {
				start = wrapped[0].start
			}
			rows = append(rows, visualRow{start: start, graphemes: wrapped})
		}
		lineOffset += len(line) + 1
	}
	cursorColumn, cursorRow = InputCursorVisualPosition(input, cursor, width)
	return rows, cursorColumn, cursorRow
}

func wrappedRowForGraphemeOffset(line string, width int, graphemeOffset int) int {
	wrapped := wrapInputLineWithTrim(line, 0, max(width, 1), true)
	graphemeByte := len(line)
	if clusters := componentGraphemeIndices(line); graphemeOffset < len(clusters) {
		graphemeByte = clusters[graphemeOffset].start
	}
	for row, graphemes := range wrapped {
		last := graphemeByte
		if len(graphemes) > 0 {
			last = graphemes[len(graphemes)-1].end
		}
		if graphemeByte < last {
			return row
		}
		// `last` is an exclusive source offset. When a hard wrap starts at
		// that exact offset, the anchor belongs to the following row; using
		// `<=` here moved it back one visual row on every resize.
		if graphemeByte == last {
			nextStartsHere := row+1 < len(wrapped) && len(wrapped[row+1]) > 0 && wrapped[row+1][0].start == graphemeByte
			if !nextStartsHere {
				return row
			}
		}
	}
	return max(len(wrapped)-1, 0)
}

func graphemeOffsetForWrappedRow(line string, width int, row int) int {
	wrapped := wrapInputLineWithTrim(line, 0, max(width, 1), true)
	if row >= len(wrapped) || len(wrapped[row]) == 0 {
		return len(componentGraphemes(line))
	}
	return len(componentGraphemes(line[:wrapped[row][0].start]))
}

// InputCursorVisualPosition returns the (column, row) of the cursor over
// word-wrapped input at width.
func InputCursorVisualPosition(input string, cursor int, width int) (column int, row int) {
	width = max(width, 1)
	cursor = min(cursor, len(input))
	lineOffset := 0
	rowOffset := 0
	for _, line := range strings.Split(input, "\n") {
		lineEnd := lineOffset + len(line)
		wrapped := wrapInputLine(line, lineOffset, width)
		if cursor <= lineEnd {
			previousColumn, previousRow := 0, rowOffset
			for wrappedRow, graphemes := range wrapped {
				row := rowOffset + wrappedRow
				column := 0
				for _, grapheme := range graphemes {
					if cursor <= grapheme.start {
						return column, row
					}
					column += grapheme.width
					endColumn, endRow := column, row
					if column >= width {
						endColumn, endRow = 0, row+1
					}
					if cursor <= grapheme.end {
						return endColumn, endRow
					}
					previousColumn, previousRow = endColumn, endRow
				}
			}
			return previousColumn, previousRow
		}
		rowOffset += len(wrapped)
		lineOffset = lineEnd + 1
	}
	return 0, rowOffset
}

// wrapInputLine mirrors ratatui's `WordWrapper` layout so the terminal
// cursor follows the word-wrapped `Paragraph`, including whitespace
// discarded at wrap points.
func wrapInputLine(line string, offset int, width int) [][]inputGrapheme {
	return wrapInputLineWithTrim(line, offset, width, false)
}

// wrapInputLineWithTrim word-wraps one input line with the same trim option
// as ratatui's `Paragraph`. Composer input uses `trim = false`; question
// messages use `trim = true`, which is why both callers share this
// implementation instead of trying to infer one policy from the other.
func wrapInputLineWithTrim(line string, offset int, width int, trim bool) [][]inputGrapheme {
	var wrapped [][]inputGrapheme
	var pendingLine, pendingWord, pendingWhitespace []inputGrapheme
	lineWidth, wordWidth, whitespaceWidth := 0, 0, 0
	previousWasNonWhitespace := false

	for _, cluster := range componentGraphemeIndices(line) {
		symbol := cluster.text
		symbolWidth := textLayoutDisplayWidth(symbol)
		if symbolWidth > width {
			continue
		}
		// Ratatui filters control graphemes before wrapping and rendering.
		// Keep them as zero-width map anchors so their byte offsets survive
		// mouse editing, while excluding them from word-boundary decisions.
		control := strings.ContainsFunc(symbol, unicode.IsControl)
		whitespace := !control &&
			(symbol == "​" || (isAllWhitespace(symbol) && symbol != " "))
		grapheme := inputGrapheme{
			start:      offset + cluster.start,
			end:        offset + cluster.start + len(symbol),
			width:      symbolWidth,
			whitespace: whitespace,
		}
		wordFound := previousWasNonWhitespace && whitespace
		trimmedOverflow := len(pendingLine) == 0 && trim && wordWidth+symbolWidth > width
		whitespaceOverflow := len(pendingLine) == 0 && trim && whitespaceWidth+symbolWidth > width
		untrimmedOverflow := len(pendingLine) == 0 && !trim && wordWidth+whitespaceWidth+symbolWidth > width

		if wordFound || trimmedOverflow || whitespaceOverflow || untrimmedOverflow {
			if len(pendingLine) > 0 || !trim {
				pendingLine = append(pendingLine, pendingWhitespace...)
				pendingWhitespace = nil
				lineWidth += whitespaceWidth
			}
			pendingLine = append(pendingLine, pendingWord...)
			pendingWord = nil
			lineWidth += wordWidth
			whitespaceWidth = 0
			wordWidth = 0
		}

		lineFull := lineWidth >= width
		pendingWordOverflow := symbolWidth > 0 && lineWidth+whitespaceWidth+wordWidth >= width
		if lineFull || pendingWordOverflow {
			remainingWidth := max(width-lineWidth, 0)
			wrapped = append(wrapped, pendingLine)
			pendingLine = nil
			lineWidth = 0

			for len(pendingWhitespace) > 0 {
				pending := pendingWhitespace[0]
				if pending.width > remainingWidth {
					break
				}
				whitespaceWidth -= pending.width
				remainingWidth -= pending.width
				pendingWhitespace = pendingWhitespace[1:]
			}
			if whitespace && len(pendingWhitespace) == 0 {
				previousWasNonWhitespace = false
				continue
			}
		}

		if grapheme.whitespace {
			whitespaceWidth += grapheme.width
			pendingWhitespace = append(pendingWhitespace, grapheme)
		} else {
			wordWidth += grapheme.width
			pendingWord = append(pendingWord, grapheme)
		}
		previousWasNonWhitespace = !whitespace
	}

	if len(pendingLine) == 0 && len(pendingWord) == 0 && len(pendingWhitespace) > 0 && trim {
		wrapped = append(wrapped, []inputGrapheme{})
	}
	if len(pendingLine) > 0 || !trim {
		pendingLine = append(pendingLine, pendingWhitespace...)
	}
	pendingLine = append(pendingLine, pendingWord...)
	if len(pendingLine) > 0 {
		wrapped = append(wrapped, pendingLine)
	}
	if len(wrapped) == 0 {
		wrapped = append(wrapped, []inputGrapheme{})
	}
	return wrapped
}

// isAllWhitespace is Rust's `str::chars().all(char::is_whitespace)`.
func isAllWhitespace(text string) bool {
	for _, character := range text {
		if !unicode.IsSpace(character) {
			return false
		}
	}
	return true
}

// textLayoutDisplayWidth is the display width of a grapheme, zero for
// control text (Rust's private `display_width` in text_layout.rs).
func textLayoutDisplayWidth(text string) int {
	if strings.ContainsFunc(text, unicode.IsControl) {
		return 0
	}
	return displayWidth(text)
}

// SetInputCursor places the terminal cursor over word-wrapped composer
// input, queueRows below the top of area and adjusted by the paragraph's
// scroll.
func SetInputCursor(frame *Frame, area uv.Rectangle, input string, cursor int, queueRows int, scroll int) {
	if area.Dx() == 0 || area.Dy() == 0 {
		return
	}
	width := area.Dx()
	column, inputRow := InputCursorVisualPosition(input, cursor, width)
	row := max(queueRows+inputRow-scroll, 0)
	if row < area.Dy() {
		frame.SetCursorPosition(uv.Pos(area.Min.X+min(column, max(width-1, 0)), area.Min.Y+row))
	}
}

// InputVisualRows is the number of rows the word-wrapped input occupies at
// width.
func InputVisualRows(input string, width int) int {
	_, row := InputCursorVisualPosition(input, len(input), width)
	return row + 1
}
