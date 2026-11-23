// Port of mj-chat/src/golden.rs.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// bufferLinesTestFilter is applied to every row BufferLines returns. It is
// nil in the program; this package's tests install normalizeClockedActivity,
// as Rust applies it under #[cfg(test)] only (other packages' tests see the
// rows unchanged, as other crates do in Rust).
var bufferLinesTestFilter func(string) string

// BufferLines converts a cell buffer to text rows without its right-edge
// padding.
func BufferLines(buffer *uv.Buffer) []string {
	panic("unported: mj-chat/src/golden.rs buffer_lines")
}

// normalizeClockedActivity replaces the animated activity dots in the
// composer's top border and in "Reviewing" rows with a fixed pattern, so
// goldens do not depend on the animation frame. Test-only in Rust.
func normalizeClockedActivity(line string) string {
	panic("unported: mj-chat/src/golden.rs normalize_clocked_activity")
}
