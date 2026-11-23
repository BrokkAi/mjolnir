// Tests ported from mj-chat/src/components/scrollbar.rs.

package chat

import (
	"fmt"
	"math"
	"strings"
	"testing"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func scrollbarTrack() uv.Rectangle {
	return uv.Rect(7, 11, 1, 20)
}

func unitHeights(count int) []int {
	heights := make([]int, count)
	for i := range heights {
		heights[i] = 1
	}
	return heights
}

func TestOffsetNeverHidesRowsThatFitTheViewport(t *testing.T) {
	for _, test := range []struct {
		offset   int
		heights  []int
		viewport int
		want     int
	}{
		// Content shrank below the viewport: show it all from the top.
		{7, unitHeights(3), 10, 0},
		// The viewport grew past the content.
		{2, unitHeights(6), 6, 0},
		// Exact fit.
		{1, unitHeights(5), 5, 0},
		// One more row than fits: only the last page start is allowed.
		{4, unitHeights(6), 5, 1},
		{1, unitHeights(6), 5, 1},
		{0, unitHeights(6), 5, 0},
		// Variable heights: the last page starts where the tail first fits.
		{9, []int{3, 3, 2, 2}, 5, 2},
		{1, nil, 5, 0},
	} {
		if got := ClampOffsetToLastPage(test.offset, test.heights, test.viewport); got != test.want {
			t.Errorf("ClampOffsetToLastPage(%d, %v, %d) = %d, want %d", test.offset, test.heights, test.viewport, got, test.want)
		}
	}
}

func centeredView(heights []int, selected int, viewport int) (int, string) {
	offset := CenteredOffset(selected, heights, viewport)
	var lines []Line
	for index := offset; index < len(heights); index++ {
		for row := range heights[index] {
			if len(lines) == viewport {
				break
			}
			marker := " "
			if index == selected && row == 0 {
				marker = ">"
			}
			lines = append(lines, RawLine(fmt.Sprintf("%s%02d %s", marker, index, strings.Repeat("row", 2))))
		}
	}
	terminal := newComponentTerminal(12, viewport)
	terminal.draw(func(frame *Frame) {
		frame.RenderWidget(NewParagraph(TextFrom(lines...)), frame.Area())
	})
	return offset, strings.Join(BufferLines(terminal.buffer), "\n")
}

func appendCenteredState(output *strings.Builder, label string, heights []int, selected int, viewport int) {
	offset, surface := centeredView(heights, selected, viewport)
	fmt.Fprintf(output, "=== %s (12x%d) ===\n", label, viewport)
	fmt.Fprintf(output, "offset: %d\n", offset)
	output.WriteString(surface)
	output.WriteString("\n")
}

func TestGoldenCenteredListViewport(t *testing.T) {
	var output strings.Builder
	heights := unitHeights(40)
	for selected := range 40 {
		appendCenteredState(&output, fmt.Sprintf("unit rows selected %d", selected), heights, selected, 9)
		fmt.Fprintf(&output, "unit offset: %d\n", CenteredUnitOffset(selected, 40, 9))
	}
	fitting := unitHeights(6)
	for selected := range 6 {
		appendCenteredState(&output, fmt.Sprintf("fit viewport 6 selected %d", selected), fitting, selected, 6)
		appendCenteredState(&output, fmt.Sprintf("fit viewport 20 selected %d", selected), fitting, selected, 20)
		list := []Line{
			RawLine("Alpha"),
			RawLine("Beta"),
			RawLine("Gamma"),
			RawLine("Delta"),
			RawLine("Epsilon"),
			RawLine("Zeta"),
		}
		terminal := newComponentTerminal(12, 6)
		form := NewForm[uint8]()
		form.Declare(1, ControlKindChoiceList{Len: 6, Selected: selected})
		form.EndFrame(1)
		terminal.draw(func(frame *Frame) {
			form.BeginFrame()
			ChoiceListRender(frame, frame.Area(), list, selected, &form, 1)
			form.EndFrame(1)
		})
		var styles []string
		for x := range terminal.width {
			cell := terminal.buffer.CellAt(x, selected)
			styles = append(styles, fmt.Sprintf(
				"%s:%s/%s/%s",
				cellSymbol(cell),
				debugCellColor(cell.Style.Fg),
				debugCellColor(cell.Style.Bg),
				debugCellModifier(cell.Style),
			))
		}
		fmt.Fprintf(
			&output,
			"unit offsets at viewport 6/20: %d/%d; unit offset viewport 6: %d; actual ChoiceList selected %d:\n%s\nselected row style: %s\n",
			CenteredOffset(selected, fitting, 6),
			CenteredOffset(selected, fitting, 20),
			CenteredUnitOffset(selected, 6, 6),
			selected,
			strings.Join(BufferLines(terminal.buffer), "\n"),
			strings.Join(styles, " "),
		)
	}
	appendCenteredState(&output, "mixed rows fit", []int{2, 3, 2}, 2, 7)
	mixed := []int{2, 3, 4, 2, 3, 2, 4, 3}
	for selected := range mixed {
		appendCenteredState(&output, fmt.Sprintf("mixed heights selected %d", selected), mixed, selected, 9)
	}
	core.AssertGolden(t, "testdata", "centered-list-viewport", output.String())
}

func TestZeroSizedTracksHaveNoGeometry(t *testing.T) {
	if _, ok := NewScrollbarGeometry(uv.Rect(0, 0, 0, 20), 100, 0, 10); ok {
		t.Error("a zero-width track has geometry")
	}
	if _, ok := NewScrollbarGeometry(uv.Rect(0, 0, 1, 0), 100, 0, 10); ok {
		t.Error("a zero-height track has geometry")
	}
}

// debugRect is ratatui's Debug of a Rect.
func debugRect(r uv.Rectangle) string {
	return fmt.Sprintf("Rect { x: %d, y: %d, width: %d, height: %d }", r.Min.X, r.Min.Y, r.Dx(), r.Dy())
}

func renderedGeometry(t *testing.T, label string, content, position, viewport int) string {
	track := uv.Rect(1, 0, 1, 20)
	geometry, ok := NewScrollbarGeometry(track, content, position, viewport)
	if !ok {
		t.Fatal("geometry")
	}
	terminal := newComponentTerminal(3, 20)
	terminal.draw(func(frame *Frame) { RenderScrollbar(frame, geometry) })
	return fmt.Sprintf(
		"=== %s (3x20) ===\ngeometry: max=%d thumb=%s\n%s\n",
		label,
		geometry.MaxScroll,
		debugRect(geometry.Thumb),
		strings.Join(BufferLines(terminal.buffer), "\n"),
	)
}

func TestGoldenScrollbarGeometry(t *testing.T) {
	var output strings.Builder
	output.WriteString(renderedGeometry(t, "full viewport", 10, 0, 10))
	output.WriteString(renderedGeometry(t, "proportional thumb at start", 100, 0, 25))
	output.WriteString(renderedGeometry(t, "proportional thumb at end", 100, 75, 25))
	core.AssertGolden(t, "testdata", "scrollbar-geometry", output.String())
}

func TestPositionsPastTheEndClampToTheEnd(t *testing.T) {
	end, ok := NewScrollbarGeometry(scrollbarTrack(), 100, math.MaxInt, 25)
	if !ok {
		t.Fatal("track geometry")
	}
	if end.Thumb.Max.Y != end.Track.Max.Y {
		t.Fatalf("thumb bottom %d, track bottom %d", end.Thumb.Max.Y, end.Track.Max.Y)
	}
}

func TestHeldThumbSeeksAcrossTheTrackAndReleasesOutsideIt(t *testing.T) {
	var drag ScrollbarDrag
	drag.SetGeometry(NewScrollbarGeometry(scrollbarTrack(), 100, 0, 25))
	if got := drag.HandleMouse(formMouseDown(7, 12)); got != (ScrollbarPointerConsumed{}) {
		t.Fatalf("press on thumb = %#v", got)
	}
	if !drag.IsDragging() {
		t.Fatal("press on thumb does not hold it")
	}
	if got := drag.HandleMouse(formMouseDrag(7, math.MaxUint16)); got != (ScrollbarPointerScrollTo{Offset: 75}) {
		t.Fatalf("drag past the track = %#v", got)
	}
	if got := drag.HandleMouse(formMouseUp(7, math.MaxUint16)); got != (ScrollbarPointerConsumed{}) {
		t.Fatalf("release = %#v", got)
	}
	if drag.IsDragging() {
		t.Fatal("release keeps the thumb held")
	}
}

func TestTrackClickSeeksAndAResizedTrackReleasesTheDrag(t *testing.T) {
	var drag ScrollbarDrag
	drag.SetGeometry(NewScrollbarGeometry(scrollbarTrack(), 100, 0, 25))
	if _, ok := drag.HandleMouse(formMouseDown(7, 27)).(ScrollbarPointerScrollTo); !ok {
		t.Fatal("track click does not seek")
	}
	drag.SetGeometry(NewScrollbarGeometry(uv.Rect(8, 11, 1, 20), 100, 0, 25))
	if drag.IsDragging() {
		t.Fatal("a moved track keeps the drag")
	}
}
