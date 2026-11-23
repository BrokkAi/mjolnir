// Tests of mj-chat/src/selection.rs.

package chat

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func registry(frames ...SurfaceFrame) FrameSurfaces {
	surfaces := NewFrameSurfaces()
	for _, frame := range frames {
		surfaces.Push(frame)
	}
	return surfaces
}

func selectionTranscript(topRow, totalRows int) SurfaceFrame {
	return SurfaceFrameScrollable(SurfaceIdTranscript{}, uv.Rect(2, 3, 10, 4), topRow, totalRows)
}

func bufferWithRows(rows []string, width int) *uv.Buffer {
	area := uv.Rect(0, 0, width, len(rows))
	buffer := uv.NewBuffer(width, len(rows))
	lines := make([]Line, 0, len(rows))
	for _, row := range rows {
		lines = append(lines, RawLine(row))
	}
	NewParagraph(TextFrom(lines...)).Render(area, buffer)
	return buffer
}

func transcriptBuffer(topRow int) (*uv.Buffer, SurfaceFrame) {
	buffer := uv.NewBuffer(14, 10)
	rows := make([]Line, 0, 4)
	for row := topRow; row < topRow+4; row++ {
		rows = append(rows, RawLine(fmt.Sprintf("row%02d abcde", row)))
	}
	NewParagraph(TextFrom(rows...)).Render(uv.Rect(2, 3, 10, 4), buffer)
	return buffer, SurfaceFrameScrollable(SurfaceIdTranscript{}, uv.Rect(2, 3, 10, 4), topRow, topRow+20)
}

func appendSelectionState(output *strings.Builder, label string, buffer *uv.Buffer, details string) {
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, buffer.Width(), buffer.Height())
	fmt.Fprintf(output, "%s\n", details)
	output.WriteString(strings.Join(BufferLines(buffer), "\n"))
	output.WriteByte('\n')
}

// screenRowDebug is Rust's Debug for `Option<u16>`.
func screenRowDebug(row int, ok bool) string {
	if !ok {
		return "None"
	}
	return fmt.Sprintf("Some(%d)", row)
}

// autoscrollDebug is Rust's Debug for `Option<(SurfaceId, i8)>`.
func autoscrollDebug(id SurfaceId, direction int8, ok bool) string {
	if !ok {
		return "None"
	}
	return fmt.Sprintf("Some((%#v, %d))", id, direction)
}

// modifierDebug is ratatui's Debug for a cell's Modifier, read from the
// attributes a uv cell carries.
func modifierDebug(cell *uv.Cell) string {
	var names []string
	if cell.Style.Attrs&uv.AttrBold != 0 {
		names = append(names, "BOLD")
	}
	if cell.Style.Attrs&uv.AttrFaint != 0 {
		names = append(names, "DIM")
	}
	if cell.Style.Attrs&uv.AttrItalic != 0 {
		names = append(names, "ITALIC")
	}
	if cell.Style.Underline != uv.UnderlineNone {
		names = append(names, "UNDERLINED")
	}
	if cell.Style.Attrs&uv.AttrBlink != 0 {
		names = append(names, "SLOW_BLINK")
	}
	if cell.Style.Attrs&uv.AttrRapidBlink != 0 {
		names = append(names, "RAPID_BLINK")
	}
	if cell.Style.Attrs&uv.AttrReverse != 0 {
		names = append(names, "REVERSED")
	}
	if cell.Style.Attrs&uv.AttrConceal != 0 {
		names = append(names, "HIDDEN")
	}
	if cell.Style.Attrs&uv.AttrStrikethrough != 0 {
		names = append(names, "CROSSED_OUT")
	}
	if len(names) == 0 {
		return "NONE"
	}
	return strings.Join(names, " | ")
}

func reversedCells(buffer *uv.Buffer, width, height int) []uv.Position {
	var reversed []uv.Position
	for y := range height {
		for x := range width {
			cell := buffer.CellAt(x, y)
			if cell == nil {
				panic("cell")
			}
			if cell.Style.Attrs&uv.AttrReverse != 0 {
				reversed = append(reversed, uv.Pos(x, y))
			}
		}
	}
	return reversed
}

func positionsDebug(positions []uv.Position) string {
	parts := make([]string, 0, len(positions))
	for _, position := range positions {
		parts = append(parts, fmt.Sprintf("(%d, %d)", position.X, position.Y))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func TestGoldenTextSelection(t *testing.T) {
	var output strings.Builder

	frame := selectionTranscript(40, 200)
	row42, ok42 := frame.ScreenRow(42)
	row39, ok39 := frame.ScreenRow(39)
	row44, ok44 := frame.ScreenRow(44)
	fmt.Fprintf(&output,
		"=== content coordinates through a scrolled transcript (10x4) ===\nscreen (2,3) -> %#v; screen (5,5) -> %#v; content row 42 -> %s; rows 39 and 44 -> %s, %s\n",
		frame.ContentPos(2, 3), frame.ContentPos(5, 5),
		screenRowDebug(row42, ok42), screenRowDebug(row39, ok39), screenRowDebug(row44, ok44))

	surfaces := registry(selectionTranscript(0, 4))
	state := NewSelectionState()
	state.OnMouseDown(5, 4, &surfaces)
	state.OnMouseDrag(5, 4, &surfaces)
	action := state.OnMouseUp(5, 4, &surfaces)
	buffer, _ := transcriptBuffer(0)
	selected, hasSelection := state.Range()
	appendSelectionState(&output, "press and release in the same cell", buffer,
		fmt.Sprintf("action: %#v; range: %s; active surface: %s",
			action, selectionRangeDebug(selected, hasSelection), surfaceIdDebug(state.ActiveSurface())))

	surfaces = registry(selectionTranscript(10, 100))
	state = NewSelectionState()
	buffer, frame = transcriptBuffer(10)
	state.OnMouseDown(4, 4, &surfaces)
	state.OnMouseDrag(7, 5, &surfaces)
	action = state.OnMouseUp(7, 5, &surfaces)
	selected, hasSelection = state.Range()
	if !hasSelection {
		t.Fatal("selection range")
	}
	Highlight(buffer, &frame, &selected)
	copied := ExtractRows(buffer, &frame, &selected)
	appendSelectionState(&output, "drag, highlight, and copy request", buffer,
		fmt.Sprintf("action: %#v; range: %#v; active surface: %s; copied: %s",
			action, selected, surfaceIdDebug(state.ActiveSurface()), core.RustDebugString(copied)))

	surfaces = registry(selectionTranscript(0, 4))
	state = NewSelectionState()
	buffer, frame = transcriptBuffer(0)
	state.OnMouseDown(8, 5, &surfaces)
	action = state.OnMouseUp(3, 3, &surfaces)
	selected, hasSelection = state.Range()
	if !hasSelection {
		t.Fatal("reverse selection range")
	}
	Highlight(buffer, &frame, &selected)
	appendSelectionState(&output, "backward drag normalized to content order", buffer,
		fmt.Sprintf("action: %#v; normalized range: %#v", action, selected))

	surfaces = registry(selectionTranscript(10, 100))
	state = NewSelectionState()
	state.OnMouseDown(5, 5, &surfaces)
	requests := []string{"inside: " + autoscrollDebug(state.AutoscrollRequest(&surfaces))}
	state.OnMouseDrag(5, 4, &surfaces)
	requests = append(requests, "one row below top: "+autoscrollDebug(state.AutoscrollRequest(&surfaces)))
	state.OnMouseDrag(5, 3, &surfaces)
	requests = append(requests, "top edge: "+autoscrollDebug(state.AutoscrollRequest(&surfaces)))
	state.OnMouseDrag(5, 1, &surfaces)
	requests = append(requests, "above top edge: "+autoscrollDebug(state.AutoscrollRequest(&surfaces)))
	state.OnMouseDrag(5, 6, &surfaces)
	requests = append(requests, "bottom edge: "+autoscrollDebug(state.AutoscrollRequest(&surfaces)))
	state.OnMouseUp(5, 6, &surfaces)
	requests = append(requests, "after release: "+autoscrollDebug(state.AutoscrollRequest(&surfaces)))
	buffer, _ = transcriptBuffer(10)
	appendSelectionState(&output, "autoscroll direction at viewport edges", buffer, strings.Join(requests, "; "))

	surfaces = registry(selectionTranscript(0, 4))
	state = NewSelectionState()
	state.OnMouseDown(5, 5, &surfaces)
	state.OnMouseDrag(5, 9, &surfaces)
	request := autoscrollDebug(state.AutoscrollRequest(&surfaces))
	buffer, _ = transcriptBuffer(0)
	appendSelectionState(&output, "no autoscroll when the whole surface fits", buffer, "request: "+request)

	surfaces = registry(selectionTranscript(0, 4))
	state = NewSelectionState()
	state.OnMouseDown(3, 3, &surfaces)
	state.OnMouseUp(9, 5, &surfaces)
	before := selectionRangeDebug(state.Range())
	state.OnMouseDown(40, 40, &surfaces)
	after := selectionRangeDebug(state.Range())
	action = state.OnMouseUp(40, 40, &surfaces)
	buffer, _ = transcriptBuffer(0)
	appendSelectionState(&output, "outside press clears the selection", buffer,
		fmt.Sprintf("before: %s; after: %s; active: %s; action: %#v",
			before, after, surfaceIdDebug(state.ActiveSurface()), action))

	buffer = bufferWithRows([]string{"abcdefgh", "ijklmnop", "qrstuvwx"}, 8)
	frame = SurfaceFrameFixed(SurfaceIdPromptInput{}, uv.Rect(1, 0, 6, 3))
	selected = SelectionRange{Start: NewContentPos(0, 3), End: NewContentPos(1, 1)}
	Highlight(buffer, &frame, &selected)
	appendSelectionState(&output, "highlight only selected cells", buffer,
		"reverse-video cells: "+positionsDebug(reversedCells(buffer, 8, 3)))

	area := uv.Rect(0, 0, 4, 1)
	buffer = uv.NewBuffer(4, 1)
	NewParagraph(TextFrom(RawLine("bold").WithStyle(Style{}.Bold()))).Render(area, buffer)
	frame = SurfaceFrameFixed(SurfaceIdModalBody{}, area)
	selected = SelectionRange{Start: NewContentPos(0, 0), End: NewContentPos(0, 3)}
	Highlight(buffer, &frame, &selected)
	var modifiers []string
	for x := range 4 {
		modifiers = append(modifiers, modifierDebug(buffer.CellAt(x, 0)))
	}
	appendSelectionState(&output, "selection keeps existing bold style", buffer,
		"cell modifiers: "+strings.Join(modifiers, " | "))

	buffer = bufferWithRows([]string{"hello", "hi", "there"}, 10)
	frame = SurfaceFrameFixed(SurfaceIdModalBody{}, uv.Rect(0, 0, 10, 3))
	selected = SelectionRange{Start: NewContentPos(0, 0), End: NewContentPos(2, 9)}
	copied = ExtractRows(buffer, &frame, &selected)
	appendSelectionState(&output, "copy trims pad and joins rows", buffer,
		"copied: "+core.RustDebugString(copied))

	buffer = bufferWithRows([]string{"abcdefgh", "ijklmnop", "qrstuvwx"}, 8)
	frame = SurfaceFrameFixed(SurfaceIdPromptInput{}, uv.Rect(0, 0, 8, 3))
	selected = SelectionRange{Start: NewContentPos(0, 5), End: NewContentPos(2, 2)}
	copied = ExtractRows(buffer, &frame, &selected)
	appendSelectionState(&output, "copy reads partial first and last rows", buffer,
		"copied: "+core.RustDebugString(copied))

	buffer = bufferWithRows([]string{"xxhello", "xxworld"}, 7)
	frame = SurfaceFrameFixed(SurfaceIdResumeList{}, uv.Rect(2, 0, 5, 2))
	selected = SelectionRange{Start: NewContentPos(0, 0), End: NewContentPos(1, 4)}
	copied = ExtractRows(buffer, &frame, &selected)
	appendSelectionState(&output, "copy is relative to the surface origin", buffer,
		"copied: "+core.RustDebugString(copied))

	core.AssertGolden(t, "testdata", "text-selection", output.String())
}

func TestSurfaceAtPicksTheLastRegisteredOverlappingSurface(t *testing.T) {
	pane := SurfaceFrameFixed(SurfaceIdDashboardPane(0), uv.Rect(0, 0, 20, 10))
	popup := SurfaceFrameFixed(SurfaceIdModalBody{}, uv.Rect(4, 2, 8, 4))
	surfaces := registry(pane, popup)

	if frame := surfaces.SurfaceAt(5, 3); frame == nil || frame.ID != (SurfaceIdModalBody{}) {
		t.Fatalf("surface at (5, 3) = %#v", frame)
	}
	if frame := surfaces.SurfaceAt(1, 3); frame == nil || frame.ID != SurfaceIdDashboardPane(0) {
		t.Fatalf("surface at (1, 3) = %#v", frame)
	}
	if frame := surfaces.SurfaceAt(30, 30); frame != nil {
		t.Fatalf("surface at (30, 30) = %#v", frame)
	}
}

func TestVisualStateChangesOnlyWhenAHighlightChanges(t *testing.T) {
	surfaces := registry(selectionTranscript(0, 4))
	state := NewSelectionState()
	if state.VisualState() != (SelectionVisualState{}) || state.VisualRevision() != 0 {
		t.Fatalf("fresh state = %#v, revision %d", state.VisualState(), state.VisualRevision())
	}

	// A click candidate has no highlight, so pressing and releasing it do not
	// ask the outer loop to repaint for selection alone.
	state.OnMouseDown(5, 4, &surfaces)
	if state.VisualState() != (SelectionVisualState{}) || state.VisualRevision() != 0 {
		t.Fatalf("pressed state = %#v, revision %d", state.VisualState(), state.VisualRevision())
	}
	if action := state.OnMouseUp(5, 4, &surfaces); action != (SelectionActionClick{Column: 5, Row: 4}) {
		t.Fatalf("release action = %#v", action)
	}
	if state.VisualRevision() != 0 {
		t.Fatalf("revision after click = %d", state.VisualRevision())
	}

	state.OnMouseDown(5, 4, &surfaces)
	state.OnMouseDrag(6, 4, &surfaces)
	want := SelectionVisualState{
		Surface:  SurfaceIdTranscript{},
		Range:    SelectionRange{Start: NewContentPos(1, 3), End: NewContentPos(1, 4)},
		HasRange: true,
	}
	if state.VisualState() != want {
		t.Fatalf("dragging state = %#v", state.VisualState())
	}
	if state.VisualRevision() != 1 {
		t.Fatalf("revision after drag = %d", state.VisualRevision())
	}

	// Repeating the same drag and completing it leave the painted range
	// alone, even though the phase changes from Dragging to Completed.
	state.OnMouseDrag(6, 4, &surfaces)
	if state.VisualRevision() != 1 {
		t.Fatalf("revision after repeated drag = %d", state.VisualRevision())
	}
	if _, ok := state.OnMouseUp(6, 4, &surfaces).(SelectionActionCopyRequested); !ok {
		t.Fatal("release after drag did not request a copy")
	}
	if state.VisualRevision() != 1 {
		t.Fatalf("revision after completion = %d", state.VisualRevision())
	}

	state.Clear()
	if state.VisualState() != (SelectionVisualState{}) || state.VisualRevision() != 2 {
		t.Fatalf("cleared state = %#v, revision %d", state.VisualState(), state.VisualRevision())
	}
	state.Clear()
	if state.VisualRevision() != 2 {
		t.Fatalf("revision after second clear = %d", state.VisualRevision())
	}
}

func TestRetrackReportsAChangedVisibleRangeAfterScrolling(t *testing.T) {
	before := registry(selectionTranscript(10, 100))
	after := registry(selectionTranscript(11, 100))
	state := NewSelectionState()
	state.OnMouseDown(5, 4, &before)
	state.OnMouseDrag(5, 6, &before)
	revision := state.VisualRevision()

	state.Retrack(&after)

	if state.VisualRevision() != revision+1 {
		t.Fatalf("revision = %d, want %d", state.VisualRevision(), revision+1)
	}
	visual := state.VisualState()
	if !visual.HasRange || visual.Range.End.Row != 14 {
		t.Fatalf("drag range = %#v", visual)
	}
	state.Retrack(&after)
	if state.VisualRevision() != revision+1 {
		t.Fatalf("revision after second retrack = %d", state.VisualRevision())
	}
}

func TestDraggingOutsideTheRectClampsToTheVisibleRows(t *testing.T) {
	surfaces := registry(selectionTranscript(10, 100))
	state := NewSelectionState()

	state.OnMouseDown(5, 5, &surfaces)
	state.OnMouseDrag(0, 0, &surfaces)
	want := SelectionRange{Start: NewContentPos(10, 0), End: NewContentPos(12, 3)}
	if got, ok := state.Range(); !ok || got != want {
		t.Fatalf("range = %s", selectionRangeDebug(got, ok))
	}

	state.OnMouseDrag(60, 40, &surfaces)
	want = SelectionRange{Start: NewContentPos(12, 3), End: NewContentPos(13, 9)}
	if got, ok := state.Range(); !ok || got != want {
		t.Fatalf("range = %s", selectionRangeDebug(got, ok))
	}
}

func TestRetrackExtendsTheSelectionAsAutoscrollMovesRowsUnderThePointer(t *testing.T) {
	state := NewSelectionState()
	before := registry(selectionTranscript(10, 100))

	// Drag to the bottom edge and hold still: content row 13 is selected.
	state.OnMouseDown(5, 4, &before)
	state.OnMouseDrag(5, 6, &before)
	if got, ok := state.Range(); !ok || got.End != NewContentPos(13, 3) {
		t.Fatalf("dragging range = %s", selectionRangeDebug(got, ok))
	}
	if id, direction, ok := state.AutoscrollRequest(&before); !ok || id != (SurfaceIdTranscript{}) || direction != 1 {
		t.Fatalf("autoscroll = %s", autoscrollDebug(id, direction, ok))
	}

	// The caller scrolls one row and re-renders; the same screen cell now
	// shows content row 14.
	after := registry(selectionTranscript(11, 100))
	state.Retrack(&after)

	if got, ok := state.Range(); !ok || got.End != NewContentPos(14, 3) {
		t.Fatalf("retracked range = %s", selectionRangeDebug(got, ok))
	}
}

func TestHighlightSkipsRowsScrolledOutOfView(t *testing.T) {
	buffer := bufferWithRows([]string{"aaaa", "bbbb"}, 4)
	frame := SurfaceFrameScrollable(SurfaceIdTranscript{}, uv.Rect(0, 0, 4, 2), 5, 20)
	selected := SelectionRange{Start: NewContentPos(0, 0), End: NewContentPos(5, 2)}

	Highlight(buffer, &frame, &selected)

	reversed := reversedCells(buffer, 4, 2)
	if want := []uv.Position{uv.Pos(0, 0), uv.Pos(1, 0), uv.Pos(2, 0)}; !slices.Equal(reversed, want) {
		t.Fatalf("reversed cells = %s", positionsDebug(reversed))
	}
}

func TestExtractRowsEmitsWideGraphemesExactlyOnce(t *testing.T) {
	// The buffer writes a wide grapheme into its leading cell and resets the
	// cell it covers, so the continuation reads back as a blank symbol.
	buffer := bufferWithRows([]string{"世界 ok"}, 10)
	if symbol := buffer.CellAt(0, 0).Content; symbol != "世" {
		t.Fatalf("cell 0 = %q", symbol)
	}
	// ratatui resets the covered cell to a space; ultraviolet leaves an
	// empty placeholder there. Either reads back blank.
	if symbol := buffer.CellAt(1, 0).Content; symbol != " " && symbol != "" {
		t.Fatalf("cell 1 = %q", symbol)
	}
	if symbol := buffer.CellAt(2, 0).Content; symbol != "界" {
		t.Fatalf("cell 2 = %q", symbol)
	}

	frame := SurfaceFrameFixed(SurfaceIdElicitationMessage{}, uv.Rect(0, 0, 10, 1))
	whole := SelectionRange{Start: NewContentPos(0, 0), End: NewContentPos(0, 9)}
	if text := ExtractRows(buffer, &frame, &whole); text != "世界 ok" {
		t.Fatalf("whole row = %q", text)
	}

	// A span that starts on a continuation cell drops the half-covered
	// grapheme rather than emitting a stray space for it.
	tail := SelectionRange{Start: NewContentPos(0, 1), End: NewContentPos(0, 9)}
	if text := ExtractRows(buffer, &frame, &tail); text != "界 ok" {
		t.Fatalf("tail = %q", text)
	}
}
