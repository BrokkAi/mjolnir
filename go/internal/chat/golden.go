// Port of mj-chat/src/golden.rs.

package chat

import (
	"strings"

	uv "github.com/charmbracelet/ultraviolet"
)

// bufferLinesTestFilter is applied to every row BufferLines returns. It is
// nil in the program; this package's tests install normalizeClockedActivity,
// as Rust applies it under #[cfg(test)] only (other packages' tests see the
// rows unchanged, as other crates do in Rust).
var bufferLinesTestFilter func(string) string

// BufferLines converts a cell buffer to text rows without its right-edge
// padding.
//
// A cell a wide grapheme hides reads as one space, as ratatui resets the
// cells after a wide symbol; ultraviolet marks them as zero-width
// placeholders instead.
func BufferLines(buffer *uv.Buffer) []string {
	area := buffer.Bounds()
	lines := make([]string, 0, area.Dy())
	for y := area.Min.Y; y < area.Max.Y; y++ {
		var row strings.Builder
		for x := area.Min.X; x < area.Max.X; x++ {
			cell := buffer.CellAt(x, y)
			if cell == nil || cell.Content == "" {
				row.WriteByte(' ')
				continue
			}
			row.WriteString(cell.Content)
		}
		line := strings.TrimRight(row.String(), " ")
		if bufferLinesTestFilter != nil {
			line = bufferLinesTestFilter(line)
		}
		lines = append(lines, line)
	}
	return lines
}

// isActivityDot is one of the animated activity dots.
func isActivityDot(character rune) bool {
	switch character {
	case '·', '∙', '•', '●':
		return true
	}
	return false
}

// normalizeClockedActivity replaces the animated activity dots in the
// composer's top border and in "Reviewing" rows with a fixed pattern, so
// goldens do not depend on the animation frame. Test-only in Rust.
func normalizeClockedActivity(line string) string {
	// The composer's top border starts with the microphone chip.
	composerBorder := "╭  " + UnicodeGlyphs.Microphone + " "
	if !strings.Contains(line, composerBorder) && !strings.Contains(line, "Reviewing") {
		return line
	}

	characters := []rune(line)
	var normalized strings.Builder
	index := 0
	for index < len(characters) {
		if isActivityDot(characters[index]) {
			start := index
			for index < len(characters) && isActivityDot(characters[index]) {
				index++
			}
			runLen := index - start
			if runLen > 1 {
				normalized.WriteRune('●')
				for range runLen - 1 {
					normalized.WriteRune('·')
				}
			} else {
				normalized.WriteRune('·')
			}
		} else {
			normalized.WriteRune(characters[index])
			index++
		}
	}
	return normalized.String()
}
