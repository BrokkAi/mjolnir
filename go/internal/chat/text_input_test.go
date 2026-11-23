// Tests of mj-chat/src/text_input.rs.

package chat

import (
	"fmt"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func appendInputState(output *strings.Builder, label string, input *TextInput, multiline bool) {
	const width = 40
	const height = 4
	buffer := uv.NewBuffer(width, height)
	frame := NewFrame(buffer)
	form := NewForm[uint8]()
	form.Declare(1, ControlKindTextField{})
	form.EndFrame(1)
	form.BeginFrame()
	if multiline {
		TextFieldRenderMultiline(frame, uv.Rect(0, 0, width, height), input, true, true, &form, uint8(1))
	} else {
		TextFieldRender(frame, uv.Rect(0, 0, width, 1), input, &form, uint8(1))
	}
	form.EndFrame(1)
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	fmt.Fprintf(output, "value: %s; cursor-byte: %d; kill-buffer: %s\n",
		core.RustDebugString(input.Value()), input.Cursor(), core.RustDebugString(input.killBuffer))
	output.WriteString(strings.Join(BufferLines(buffer), "\n"))
	output.WriteByte('\n')
}

type namedKey struct {
	name  string
	event tea.KeyPressMsg
}

func traceKeys(output *strings.Builder, label string, input *TextInput, multiline bool, keys []namedKey) {
	appendInputState(output, label+" initial", input, multiline)
	for _, named := range keys {
		outcome := input.HandleKey(named.event)
		appendInputState(output, fmt.Sprintf("%s: %s => %#v", label, named.name, outcome), input, multiline)
	}
}

func TestReadlineEditsAtUnicodeGraphemeBoundaries(t *testing.T) {
	input := TextInputFromValue("one 👩‍💻 two")
	input.HandleKey(ctrl('a'))
	input.HandleKey(key(tea.KeyRight))
	input.HandleKey(key(tea.KeyRight))
	input.HandleKey(key(tea.KeyRight))
	input.HandleKey(key(tea.KeyRight))
	input.HandleKey(key(tea.KeyDelete))
	if input.Value() != "one  two" {
		t.Fatalf("value = %q", input.Value())
	}

	// Word edits step over a whole grapheme cluster, not a char.
	input = TextInputFromValue("one two 👩‍💻")
	input.HandleKey(key(tea.KeyLeft))
	if rest := input.Value()[input.Cursor():]; rest != "👩‍💻" {
		t.Fatalf("after Left the caret precedes %q", rest)
	}
	input.HandleKey(ctrl('w'))
	if input.Value() != "one 👩‍💻" {
		t.Fatalf("after Ctrl-W value = %q", input.Value())
	}
	input.HandleKey(ctrl('y'))
	if input.Value() != "one two 👩‍💻" {
		t.Fatalf("after Ctrl-Y value = %q", input.Value())
	}
	input.HandleKey(alt('b'))
	if rest := input.Value()[input.Cursor():]; rest != "two 👩‍💻" {
		t.Fatalf("after Alt-B the caret precedes %q", rest)
	}
}

func TestGoldenTextInputEditing(t *testing.T) {
	var output strings.Builder
	input := TextInputFromValue("alpha beta")
	traceKeys(&output, "readline word and line kill/yank", &input, false, []namedKey{
		{"Ctrl-W", ctrl('w')},
		{"Ctrl-Y", ctrl('y')},
		{"Ctrl-A", ctrl('a')},
		{"Ctrl-K", ctrl('k')},
		{"Ctrl-Y", ctrl('y')},
	})

	input = TextInputMultiline()
	input.SetValue("alpha beta\ngamma")
	traceKeys(&output, "multiline line motion and kill/yank", &input, true, []namedKey{
		{"Ctrl-A to line start", ctrl('a')},
		{"Ctrl-A to input start", ctrl('a')},
		{"Ctrl-E", ctrl('e')},
		{"Ctrl-K", ctrl('k')},
		{"Ctrl-Y", ctrl('y')},
	})

	input = TextInputFromValue("alpha beta")
	traceKeys(&output, "single-line whole-value motions", &input, false, []namedKey{
		{"Ctrl-A", ctrl('a')},
		{"Ctrl-E", ctrl('e')},
		{"Ctrl-U", ctrl('u')},
	})

	input = TextInputFromValue("draft")
	input.SetHistory([]string{"first", "second"})
	traceKeys(&output, "optional history restores the draft", &input, false, []namedKey{
		{"Up", key(tea.KeyUp)},
		{"Down", key(tea.KeyDown)},
	})
	output.WriteString("=== end of rendered states ===\n")
	core.AssertGolden(t, "testdata", "text-input-editing", output.String())
}

// Hard-won: a3fd1514: consecutive Ctrl-K presses discarded earlier killed lines.
func TestSequentialControlKAccumulatesOneYankableBlock(t *testing.T) {
	input := TextInputMultiline()
	input.SetValue("line1\nline2")
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('k'))
	input.HandleKey(ctrl('k'))
	if input.Value() != "line2" {
		t.Fatalf("value = %q", input.Value())
	}
	input.HandleKey(ctrl('y'))
	if input.Value() != "line1\nline2" {
		t.Fatalf("after yank value = %q", input.Value())
	}
}

func TestAnyKeyBetweenControlKPressesRestartsTheKillBuffer(t *testing.T) {
	input := TextInputMultiline()
	input.SetValue("line1\nline2")
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('k'))
	input.HandleKey(key(tea.KeyRight))
	input.HandleKey(key(tea.KeyLeft))
	input.HandleKey(ctrl('k'))
	if input.killBuffer != "\n" {
		t.Fatalf("kill buffer = %q", input.killBuffer)
	}

	input.SetValue("line1\nline2")
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('k'))
	input.HandleKey(key('x'))
	input.HandleKey(ctrl('a'))
	input.HandleKey(ctrl('k'))
	if input.killBuffer != "x" {
		t.Fatalf("kill buffer = %q", input.killBuffer)
	}
}

func TestVerticalMotionKeepsThePreferredColumnAcrossAShortLine(t *testing.T) {
	input := TextInputMultiline()
	input.SetValue("abcdef\nxy\nghijkl")
	input.SetCursor(5)

	input.HandleKey(key(tea.KeyDown))
	if input.Cursor() != 9 {
		t.Fatalf("clamped to the end of the short line: cursor = %d", input.Cursor())
	}
	input.HandleKey(key(tea.KeyDown))
	if input.Cursor() != 15 {
		t.Fatalf("back to the remembered column: cursor = %d", input.Cursor())
	}

	// Any other motion forgets the column, so the next Down starts over.
	input.HandleKey(key(tea.KeyHome))
	input.HandleKey(key(tea.KeyUp))
	if input.Cursor() != 7 {
		t.Fatalf("cursor = %d", input.Cursor())
	}
}
