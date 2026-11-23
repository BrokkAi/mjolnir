// Port of mj-chat/src/components/text_layout.rs.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// Word-wrapped text layout shared by multiline controls and the composer.

// WrapLines wraps display text with the same wrapping for measurement and
// rendering.
func WrapLines(lines []Line, width int) []Line {
	panic("unported: mj-chat/src/components/text_layout.rs WrapLines")
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
	panic("unported: mj-chat/src/components/text_layout.rs trimBeforeEllipsis")
}

// TruncateToCells shortens text to at most width terminal cells, marking a
// cut with a trailing ellipsis.
//
// Width is measured in display cells, so a full-width glyph costs two. Text
// that already fits comes back unchanged apart from the requested
// whitespace collapsing. A width of zero yields an empty string and a width
// of one yields the ellipsis alone, because nothing else fits beside it.
func TruncateToCells(text string, width int, options Truncate) string {
	panic("unported: mj-chat/src/components/text_layout.rs TruncateToCells")
}

// multilineRows returns the wrapped rows and the visual cursor position
// (column, row) for a text value.
//
// The wrapping policy mirrors ratatui's `WordWrapper` with `trim = false`,
// which is the policy used by the composer and multiline text fields.
func multilineRows(input string, cursor int, width int) (rows []visualRow, cursorColumn int, cursorRow int) {
	panic("unported: mj-chat/src/components/text_layout.rs multilineRows")
}

func wrappedRowForGraphemeOffset(line string, width int, graphemeOffset int) int {
	panic("unported: mj-chat/src/components/text_layout.rs wrappedRowForGraphemeOffset")
}

func graphemeOffsetForWrappedRow(line string, width int, row int) int {
	panic("unported: mj-chat/src/components/text_layout.rs graphemeOffsetForWrappedRow")
}

// InputCursorVisualPosition returns the (column, row) of the cursor over
// word-wrapped input at width.
func InputCursorVisualPosition(input string, cursor int, width int) (column int, row int) {
	panic("unported: mj-chat/src/components/text_layout.rs InputCursorVisualPosition")
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
	panic("unported: mj-chat/src/components/text_layout.rs wrapInputLineWithTrim")
}

// textLayoutDisplayWidth is the display width of a grapheme, zero for
// control text (Rust's private `display_width` in text_layout.rs).
func textLayoutDisplayWidth(text string) int {
	panic("unported: mj-chat/src/components/text_layout.rs textLayoutDisplayWidth")
}

// SetInputCursor places the terminal cursor over word-wrapped composer
// input, queueRows below the top of area and adjusted by the paragraph's
// scroll.
func SetInputCursor(frame *Frame, area uv.Rectangle, input string, cursor int, queueRows int, scroll int) {
	panic("unported: mj-chat/src/components/text_layout.rs SetInputCursor")
}

// InputVisualRows is the number of rows the word-wrapped input occupies at
// width.
func InputVisualRows(input string, width int) int {
	_, row := InputCursorVisualPosition(input, len(input), width)
	return row + 1
}
