// Tests ported from mj-chat/src/components/layout.rs.

package chat

import (
	"testing"

	uv "github.com/charmbracelet/ultraviolet"
)

func TestAnchoredPopupPrefersBelowAndFallsBackAboveWithClipping(t *testing.T) {
	bounds := uv.Rect(2, 2, 30, 16)
	terminal := newComponentTerminal(40, 24)
	terminal.draw(func(frame *Frame) {
		below, _, ok := AutocompletePopupRender(
			frame,
			bounds,
			uv.Rect(8, 4, 1, 1),
			14,
			2,
			ComboBoxDefaultMaxVisibleRows,
			" choices ",
			PopupSideBelow,
		)
		if !ok {
			t.Fatal("below popup")
		}
		if below.Min.Y != 5 || below.Dy() != 4 {
			t.Fatalf("below popup = %v", below)
		}

		above, _, ok := AutocompletePopupRender(
			frame,
			bounds,
			uv.Rect(8, 15, 1, 1),
			40,
			20,
			ComboBoxDefaultMaxVisibleRows,
			" choices ",
			PopupSideBelow,
		)
		if !ok {
			t.Fatal("clipped popup")
		}
		if above.Min.Y < bounds.Min.Y || above.Max.Y > bounds.Max.Y {
			t.Fatalf("clipped popup leaves bounds vertically: %v", above)
		}
		if above.Dy() != 10 {
			t.Fatalf("clipped popup height = %d, want 10", above.Dy())
		}
		if above.Min.X < bounds.Min.X || above.Max.X > bounds.Max.X {
			t.Fatalf("clipped popup leaves bounds horizontally: %v", above)
		}
	})
}
