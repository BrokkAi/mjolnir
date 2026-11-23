// Tests of mj-chat/src/chat/rendering.rs.

package chat

import (
	"fmt"
	"reflect"
	"strings"
	"testing"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func renderingText(lines []Line) []string {
	texts := make([]string, len(lines))
	for i, line := range lines {
		texts[i] = line.String()
	}
	return texts
}

func TestSanitizerRemovesTerminalControlsAndNormalizesCarriageReturns(t *testing.T) {
	if got := sanitizeTerminalText("safe\x1b[31mred\x1b[0m\rnext\u0007"); got != "safered\nnext" {
		t.Fatalf("sanitized = %q", got)
	}
}

// Hard-won: 6c06003: OSC window-title payloads leaked into rendered transcripts
func TestSanitizerConsumesOscPayloadsAndTwoByteEscapes(t *testing.T) {
	cases := []struct{ input, want string }{
		// A build tool setting the window title, terminated by BEL and by ST.
		{"\x1b]0;make: build\x07done", "done"},
		{"\x1b]2;cargo test\x1b\\done", "done"},
		// Charset selection and cursor save/restore.
		{"\x1b(B\x1b)0plain\x1b7saved\x1b8", "plainsaved"},
		// An OSC that is never terminated stops at the line break instead of
		// eating the rest of the transcript.
		{"\x1b]0;title\nnext line", "\nnext line"},
		// An OSC ended by another escape does not swallow that escape's own
		// sequence, and nesting cannot recurse without bound.
		{"\x1b]0;title\x1b[31mred", "red"},
		{strings.Repeat("\x1b]", 50_000), ""},
	}
	for _, c := range cases {
		if got := sanitizeTerminalText(c.input); got != c.want {
			t.Errorf("sanitizeTerminalText(%.40q) = %q, want %q", c.input, got, c.want)
		}
	}
}

// Each row after a line's first says what its break removed, so copying can
// join the rows back: a space between words, nothing inside a word.
func TestWrappedRowsRecordWhatEachBreakRemoved(t *testing.T) {
	rows, _, copies := wrapStyledLineForCopy(RawLine("alpha beta abcdefghijkl"), 8, 2)
	if got, want := renderingText(rows), []string{"alpha", "  beta a", "  bcdefg", "  hijkl"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("rows = %q, want %q", got, want)
	}
	want := []rowCopy{
		{},
		{lead: 2, joins: &rowCopyJoinSpace},
		{lead: 2, joins: &rowCopyJoinCut},
		{lead: 2, joins: &rowCopyJoinCut},
	}
	if !reflect.DeepEqual(copies, want) {
		t.Fatalf("copies = %+v, want %+v", copies, want)
	}
}

func TestGraphemeWrapperNeverSplitsJoinedOrCombiningCharacters(t *testing.T) {
	rendered := renderingText(WrapStyledLine(RawLine("a 👩‍💻 é ｶﾞ z"), 4, 0))
	for _, cluster := range []string{"👩‍💻", "é", "ｶﾞ"} {
		found := false
		for _, line := range rendered {
			if strings.Contains(line, cluster) {
				found = true
			}
		}
		if !found {
			t.Errorf("no row holds %q whole: %q", cluster, rendered)
		}
	}
}

func appendRenderedMarkdown(output *strings.Builder, label string, buffer *uv.Buffer, details []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, buffer.Width(), buffer.Height())
	output.WriteString(strings.Join(BufferLines(buffer), "\n"))
	output.WriteByte('\n')
	for _, detail := range details {
		output.WriteString(detail + "\n")
	}
}

func drawMarkdownLines(lines []Line, width int) *uv.Buffer {
	buffer := uv.NewBuffer(width, max(len(lines), 1))
	frame := NewFrame(buffer)
	frame.RenderWidget(NewParagraph(TextFrom(lines...)), frame.Area())
	return buffer
}

// optionColorDebug is Rust's Debug of a style's Option<Color>.
func optionColorDebug(c Color) string {
	if !c.IsSet() {
		return "None"
	}
	return "Some(" + c.GoString() + ")"
}

func markdownStyleDetails(lines []logicalLine) []string {
	details := make([]string, len(lines))
	for row, logical := range lines {
		spans := make([]string, len(logical.line.Spans))
		for i, span := range logical.line.Spans {
			spans[i] = fmt.Sprintf("%s:fg=%s:mod=%#v", core.RustDebugString(span.Content), optionColorDebug(span.Style.Fg), span.Style.AddModifier)
		}
		if len(spans) == 0 {
			details[row] = fmt.Sprintf("row %d styles: <empty>", row)
		} else {
			details[row] = fmt.Sprintf("row %d styles: %s", row, strings.Join(spans, " | "))
		}
	}
	return details
}

func renderedSpanStyles(lines []Line) string {
	var colors []Color
	for _, line := range lines {
		for _, span := range line.Spans {
			if len(colors) == 0 || colors[len(colors)-1] != span.Style.Fg {
				colors = append(colors, span.Style.Fg)
			}
		}
	}
	names := make([]string, len(colors))
	for i, c := range colors {
		names[i] = optionColorDebug(c)
	}
	return strings.Join(names, " -> ")
}

func logicalLineLines(parsed []logicalLine) []Line {
	lines := make([]Line, len(parsed))
	for i, logical := range parsed {
		lines[i] = logical.line
	}
	return lines
}

func TestGoldenTranscriptMarkdownRender(t *testing.T) {
	var output strings.Builder

	parsed := markdownLines(
		"# Heading\n\n- **bold** and `code`\n\n```rust\nfn main() {}",
		Style{},
		Style{}.Foreground(ThemePalette().Success),
		40,
	)
	appendRenderedMarkdown(&output, "heading list and incomplete fenced code",
		drawMarkdownLines(logicalLineLines(parsed), 40), markdownStyleDetails(parsed))

	parsed = markdownLines(
		"| Name | Description |\n| --- | --- |\n| alpha | a long explanation |",
		Style{}, Style{}, 18,
	)
	appendRenderedMarkdown(&output, "narrow-width table fallback records",
		drawMarkdownLines(logicalLineLines(parsed), 32), markdownStyleDetails(parsed))

	parsed = markdownLines(
		"| Name | Score |\n| :--- | ---: |\n| alpha | 7 |",
		Style{}, Style{}, 40,
	)
	appendRenderedMarkdown(&output, "aligned Markdown table and header rule",
		drawMarkdownLines(logicalLineLines(parsed), 40), markdownStyleDetails(parsed))

	prefix := "bifrost2 · 2 repositories "
	path := "/workspace/project/run-1000/session-9275cf63/scratchpad/replay-with-a-long-stable-name"
	plain := RawLine(prefix + path)
	styled := LineFrom(
		StyledSpan(prefix, Style{}.Foreground(ColorYellow)),
		StyledSpan(path, Style{}.Foreground(ColorBlue)),
	)
	for _, width := range []int{1, 12, 40, 80} {
		plainRows := WrapStyledLine(plain, width, 0)
		appendRenderedMarkdown(&output, fmt.Sprintf("plain long path wrap at width %d", width),
			drawMarkdownLines(plainRows, width), []string{fmt.Sprintf("rows=%d", len(plainRows))})

		styledRows, sources := wrapStyledLineWithSources(styled, width, 0)
		aligned := true
		for i, row := range styledRows {
			if i >= len(sources) || row.Width() != len(sources[i]) {
				aligned = false
			}
		}
		details := []string{
			fmt.Sprintf("rows=%d source rows=%d widths aligned=%t", len(styledRows), len(sources), aligned),
			fmt.Sprintf("style runs=%s", renderedSpanStyles(styledRows)),
		}
		appendRenderedMarkdown(&output, fmt.Sprintf("styled long path wrap at width %d", width),
			drawMarkdownLines(styledRows, width), details)
	}

	plainText := truncateToWidth("alpha, beta", 7)
	styledLine := LineFrom(
		StyledSpan("alpha,", Style{}.Foreground(ThemePalette().Error)),
		StyledSpan(" beta", Style{}.Foreground(ColorBlue)),
	)
	truncated := TruncateLineToWidth(styledLine, 7)
	appendRenderedMarkdown(&output, "ellipsis trims cutoff punctuation and whitespace",
		drawMarkdownLines([]Line{RawLine(plainText)}, 7), nil)
	var style Color
	if n := len(truncated.Spans); n > 0 {
		style = truncated.Spans[n-1].Style.Fg
	}
	appendRenderedMarkdown(&output, "styled ellipsis preserves its trailing span style",
		drawMarkdownLines([]Line{truncated}, 7), []string{fmt.Sprintf("ellipsis fg=%s", optionColorDebug(style))})

	core.AssertGolden(t, "testdata", "transcript-markdown-render", output.String())
}

func TestTruncationRespectsWideGlyphsAndKeepsCombiningSequencesIntact(t *testing.T) {
	if got := truncateToWidth("界éabc", 4); got != "界é…" {
		t.Errorf("truncated = %q", got)
	}
	if got := truncateToWidth("👩‍💻 abc", 3); got != "👩‍💻…" {
		t.Errorf("truncated = %q", got)
	}
	for width := range 8 {
		text := truncateToWidth("界é👩‍💻 abc", width)
		if displayWidth(text) > width {
			t.Errorf("%q exceeds %d cells", text, width)
		}
	}
}

// The ASCII set reaches the composer's border, where the microphone was the
// last glyph a console without UTF-8 could not draw.
//
// Hard-won: 57f76ac: ASCII mode still rendered the microphone button as Unicode
func TestTheASCIIMicrophoneButtonIsPlainText(t *testing.T) {
	WithSymbols(core.SymbolSetAscii, func() {
		text := voiceButtonLine(true, false).String()
		for _, character := range text {
			if character >= utf8.RuneSelf {
				t.Fatalf("%q is not ASCII", text)
			}
		}
		area, ok := voiceButtonArea(uv.Rect(0, 0, 40, 3))
		if !ok {
			t.Fatal("a chip fits")
		}
		if displayWidth(text) != area.Dx() {
			t.Fatalf("the chip's width follows the glyph: %d != %d", displayWidth(text), area.Dx())
		}
	})
}
