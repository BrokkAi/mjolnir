// Port of mj-tui/src/tile_layout.rs (test-only items and tests).

package tui

import (
	"math"
	"testing"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// splitFocused splits the focused pane with an even ratio: SplitPane
// followed by moving focus to the new pane.
func (l *TileLayout) splitFocused(direction chat.Direction, area uv.Rectangle) PaneId {
	return l.splitFocusedWithRatio(direction, 0.5, area)
}

// splitFocusedWithRatio splits the focused pane with a chosen first-child
// ratio, then focuses the new pane.
func (l *TileLayout) splitFocusedWithRatio(direction chat.Direction, ratio float32, area uv.Rectangle) PaneId {
	newID, ok := l.SplitPane(l.focus, direction, ratio, area)
	if !ok {
		panic("test area accepts the split")
	}
	l.setFocus(newID)
	return newID
}

// tileArea is big enough that a split never trips the minimum pane size,
// even after several nested splits shrink an individual leaf.
func tileArea() uv.Rectangle {
	return uv.Rect(0, 0, 400, 100)
}

func tilePane(id uint32) PaneId {
	return PaneIdFromRaw(id)
}

func tileSampleLayout() TileLayout {
	return TileLayoutFromSaved(
		NodeSplit{
			Direction: chat.DirectionHorizontal,
			Ratio:     0.3,
			First:     NodePane{ID: tilePane(1)},
			Second: NodeSplit{
				Direction: chat.DirectionVertical,
				Ratio:     0.6,
				First:     NodePane{ID: tilePane(2)},
				Second: NodeSplit{
					Direction: chat.DirectionHorizontal,
					Ratio:     0.4,
					First:     NodePane{ID: tilePane(3)},
					Second:    NodePane{ID: tilePane(4)},
				},
			},
		},
		tilePane(2),
	)
}

type tileSplitSnapshotEntry struct {
	direction chat.Direction
	ratio     float32
}

func tileSplitSnapshot(layout *TileLayout) []tileSplitSnapshotEntry {
	var out []tileSplitSnapshotEntry
	var walk func(node Node)
	walk = func(node Node) {
		if split, ok := node.(NodeSplit); ok {
			out = append(out, tileSplitSnapshotEntry{split.Direction, split.Ratio})
			walk(split.First)
			walk(split.Second)
		}
	}
	walk(layout.Root())
	return out
}

// tileFloat32Epsilon is Rust's f32::EPSILON.
const tileFloat32Epsilon = 1.1920929e-07

func TestResizeUsesSplitInSameBranchWhenBordersShareCoordinate(t *testing.T) {
	layout := TileLayoutFromSaved(
		NodeSplit{
			Direction: chat.DirectionVertical,
			Ratio:     0.5,
			First: NodeSplit{
				Direction: chat.DirectionHorizontal,
				Ratio:     0.5,
				First:     NodePane{ID: tilePane(1)},
				Second:    NodePane{ID: tilePane(2)},
			},
			Second: NodeSplit{
				Direction: chat.DirectionHorizontal,
				Ratio:     0.5,
				First:     NodePane{ID: tilePane(3)},
				Second:    NodePane{ID: tilePane(4)},
			},
		},
		tilePane(3),
	)

	if !layout.ResizePane(tilePane(3), NavDirectionRight, 0.05, tileArea()) {
		t.Fatal("the resize changes a ratio")
	}

	splits := tileSplitSnapshot(&layout)
	if splits[0] != (tileSplitSnapshotEntry{chat.DirectionVertical, 0.5}) {
		t.Fatalf("splits[0] = %+v", splits[0])
	}
	if splits[1] != (tileSplitSnapshotEntry{chat.DirectionHorizontal, 0.5}) {
		t.Fatalf("splits[1] = %+v", splits[1])
	}
	if splits[2].direction != chat.DirectionHorizontal {
		t.Fatalf("splits[2] = %+v", splits[2])
	}
	if math.Abs(float64(splits[2].ratio-0.55)) >= tileFloat32Epsilon {
		t.Fatalf("splits[2] ratio = %v", splits[2].ratio)
	}
}

func TestFindInDirectionTiebreaksByLargerOverlapBeforeLayoutOrder(t *testing.T) {
	focused := PaneInfo{ID: tilePane(1), Rect: uv.Rect(10, 10, 10, 10), IsFocused: true}
	smallOverlapFirst := PaneInfo{ID: tilePane(2), Rect: uv.Rect(0, 10, 10, 2)}
	largerOverlapSecond := PaneInfo{ID: tilePane(3), Rect: uv.Rect(0, 10, 10, 8)}
	panes := []PaneInfo{focused, smallOverlapFirst, largerOverlapSecond}

	found, ok := FindInDirection(&focused, NavDirectionLeft, panes)
	if !ok || found != tilePane(3) {
		t.Fatalf("FindInDirection = %v, %v", found, ok)
	}
}

func TestClosingABackgroundPaneKeepsTheFocusedPaneHistory(t *testing.T) {
	layout := tileSampleLayout()
	layout.FocusPane(tilePane(4))

	if !layout.ClosePane(tilePane(1)) {
		t.Fatal("close pane 1")
	}
	if layout.Focused() != tilePane(4) {
		t.Fatalf("focused = %v", layout.Focused())
	}

	if !layout.CloseFocused() {
		t.Fatal("close focused")
	}
	if layout.Focused() != tilePane(2) {
		t.Fatalf("focused = %v", layout.Focused())
	}
}

func TestClosingTheRememberedPaneDropsTheFocusHistory(t *testing.T) {
	layout := tileSampleLayout()
	layout.FocusPane(tilePane(4))

	if !layout.ClosePane(tilePane(2)) {
		t.Fatal("close pane 2")
	}

	if !layout.CloseFocused() {
		t.Fatal("close focused")
	}
	if layout.Focused() != tilePane(3) {
		t.Fatalf("focused = %v", layout.Focused())
	}
}

func TestCloseFocusedDoesNotReuseHistoryAfterItIsConsumed(t *testing.T) {
	layout := tileSampleLayout()
	layout.FocusPane(tilePane(4))

	if !layout.CloseFocused() {
		t.Fatal("close focused")
	}
	if layout.Focused() != tilePane(2) {
		t.Fatalf("focused = %v", layout.Focused())
	}

	if !layout.CloseFocused() {
		t.Fatal("close focused again")
	}
	if layout.Focused() != tilePane(3) {
		t.Fatalf("focused = %v", layout.Focused())
	}
}

func TestSplitIsRefusedWhenALeafWouldFallUnderTheMinimum(t *testing.T) {
	layout, root := NewTileLayout()

	// 70 columns split in half gives two 35-column halves, under
	// MinPaneWidth.
	narrowArea := uv.Rect(0, 0, 70, 20)
	if layout.CanSplit(root, chat.DirectionHorizontal, narrowArea) {
		t.Fatal("a narrow area refuses a horizontal split")
	}
	if _, ok := layout.SplitPane(root, chat.DirectionHorizontal, 0.5, narrowArea); ok {
		t.Fatal("the narrow split is refused")
	}
	if layout.PaneCount() != 1 {
		t.Fatalf("pane count = %d", layout.PaneCount())
	}

	// 16 rows split in half gives two 8-row halves... but 15 rows gives two
	// halves under MinPaneHeight.
	shortArea := uv.Rect(0, 0, 100, 15)
	if layout.CanSplit(root, chat.DirectionVertical, shortArea) {
		t.Fatal("a short area refuses a vertical split")
	}
	if _, ok := layout.SplitPane(root, chat.DirectionVertical, 0.5, shortArea); ok {
		t.Fatal("the short split is refused")
	}
	if layout.PaneCount() != 1 {
		t.Fatalf("pane count = %d", layout.PaneCount())
	}

	// A generous area still allows the split.
	if !layout.CanSplit(root, chat.DirectionHorizontal, tileArea()) {
		t.Fatal("a generous area allows the split")
	}
	if _, ok := layout.SplitPane(root, chat.DirectionHorizontal, 0.5, tileArea()); !ok {
		t.Fatal("the generous split happens")
	}
}

func TestRestoredLayoutsAllocateIdsAboveTheSavedOnes(t *testing.T) {
	layout := TileLayoutFromSaved(
		NodeSplit{
			Direction: chat.DirectionHorizontal,
			Ratio:     0.5,
			First:     NodePane{ID: tilePane(1)},
			Second:    NodePane{ID: tilePane(5)},
		},
		tilePane(5),
	)

	newID, ok := layout.SplitPane(tilePane(5), chat.DirectionVertical, 0.5, tileArea())
	if !ok {
		t.Fatal("target pane accepts the split")
	}
	if newID != tilePane(6) {
		t.Fatalf("new id = %v", newID)
	}
}
