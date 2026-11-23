// Tests ported from mj-chat/src/components/text_layout.rs.

package chat

import (
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func appendTruncatedRender(output *strings.Builder, label, input string, width int, options Truncate) {
	rendered := TruncateToCells(input, width, options)
	terminal := newComponentTerminal(width, 1)
	terminal.draw(func(frame *Frame) {
		frame.RenderWidget(NewParagraph(RawText(rendered)), frame.Area())
	})
	fmt.Fprintf(output, "=== %s (%dx1) ===\n", label, width)
	fmt.Fprintf(output, "source: %s\n", rustDebugString(input))
	output.WriteString(strings.Join(BufferLines(terminal.buffer), "\n"))
	output.WriteString("\n")
}

// rustDebugString is Rust's `{:?}` of a string: quoted, with quotes,
// backslashes and control characters escaped and other characters kept.
func rustDebugString(text string) string {
	var quoted strings.Builder
	quoted.WriteByte('"')
	for _, character := range text {
		switch character {
		case '"':
			quoted.WriteString(`\"`)
		case '\\':
			quoted.WriteString(`\\`)
		case '\n':
			quoted.WriteString(`\n`)
		case '\r':
			quoted.WriteString(`\r`)
		case '\t':
			quoted.WriteString(`\t`)
		default:
			if character < 0x20 || character == 0x7f {
				fmt.Fprintf(&quoted, `\u{%x}`, character)
			} else {
				quoted.WriteRune(character)
			}
		}
	}
	quoted.WriteByte('"')
	return quoted.String()
}

func TestTruncationMeasuresDisplayCellsNotCharacters(t *testing.T) {
	plain := TruncatePlain
	// Each ideograph is two cells wide, so only two fit beside the ellipsis.
	for _, test := range []struct {
		text  string
		width int
		want  string
	}{
		{"一二三四", 5, "一二…"},
		{"一二", 4, "一二"},
		{"alpha", 1, "…"},
		{"alpha", 0, ""},
		{"", 0, ""},
	} {
		if got := TruncateToCells(test.text, test.width, plain); got != test.want {
			t.Errorf("TruncateToCells(%q, %d) = %q, want %q", test.text, test.width, got, test.want)
		}
	}
}

func TestGoldenTextTruncation(t *testing.T) {
	var output strings.Builder
	appendTruncatedRender(&output, "summary cuts dangling comma", "alpha, beta", 7, TruncateSummary)
	appendTruncatedRender(&output, "summary cuts dangling dash", "alpha - beta", 8, TruncateSummary)
	appendTruncatedRender(&output, "summary keeps full phrase", "alpha beta", 20, TruncateSummary)
	appendTruncatedRender(&output, "summary folds newlines", "alpha\n beta", 20, TruncateSummary)
	appendTruncatedRender(&output, "plain keeps cut punctuation", "alpha, beta", 7, TruncatePlain)
	appendTruncatedRender(&output, "plain preserves repeated spaces", "alpha  beta", 20, TruncatePlain)
	core.AssertGolden(t, "testdata", "text-truncation", output.String())
}
