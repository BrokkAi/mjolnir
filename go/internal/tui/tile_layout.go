// Port of mj-tui/src/tile_layout.rs.

// Binary-space-partition layout tree for tiling conversation panes.
//
// A [TileLayout] is a binary tree: each leaf is a pane holding one
// conversation, and each inner node is a split that divides its rectangle
// into a first and a second half along a ratio. [TileLayout.Panes] and
// [TileLayout.Splits] turn the tree into rectangles for a given area;
// [TileLayout.CanSplit] keeps a split from producing a pane smaller than a
// conversation can usefully show.
//
// Rust's u16 cell coordinates are ints here, as in uv.Rectangle; the
// arithmetic saturates where Rust's does.

package tui

import (
	"math"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MinPaneWidth is the narrowest a pane may be after a split, in columns.
const MinPaneWidth = 40

// MinPaneHeight is the shortest a pane may be after a split, in rows.
const MinPaneHeight = 8

// PaneId identifies one pane within a single [TileLayout]. Ids are
// allocated by that layout and are not reused while the pane exists.
type PaneId uint32

// Raw is the id's raw value, for saving a layout.
func (p PaneId) Raw() uint32 {
	return uint32(p)
}

// PaneIdFromRaw rebuilds a PaneId from a value a layout previously saved.
func PaneIdFromRaw(id uint32) PaneId {
	return PaneId(id)
}

// Node is one node of the binary-space-partition tree: a [NodePane] leaf or
// a [NodeSplit].
type Node interface {
	isNode()
}

// NodePane is a leaf holding one pane.
type NodePane struct {
	ID PaneId
}

// NodeSplit is an inner node dividing First and Second along Direction.
type NodeSplit struct {
	Direction chat.Direction
	// Ratio is the share of the split's area given to First, kept in
	// 0.1..=0.9.
	Ratio  float32
	First  Node
	Second Node
}

func (NodePane) isNode()  {}
func (NodeSplit) isNode() {}

// PaneInfo is a pane's rectangle and focus state, as computed for one area.
type PaneInfo struct {
	ID        PaneId
	Rect      uv.Rectangle
	IsFocused bool
}

// SplitBorder is one split boundary within a laid-out area, for resize.
type SplitBorder struct {
	// Pos is the column (horizontal split) or row (vertical split) of the
	// divider.
	Pos       int
	Direction chat.Direction
	// Ratio is the share of Area given to the split's first child.
	Ratio float32
	// Area is the split node's own area, before dividing it.
	Area uv.Rectangle
	// Path is the steps from the root to this split: false into first,
	// true into second.
	Path []bool
}

// NavDirection is a direction to move focus or grow a pane toward.
type NavDirection int

const (
	NavDirectionLeft NavDirection = iota
	NavDirectionRight
	NavDirectionUp
	NavDirectionDown
)

// TileLayout is a binary-space-partition tree of panes, with a focused leaf.
type TileLayout struct {
	root  Node
	focus PaneId
	// prevFocus is the pane focused before focus, so CloseFocused can return
	// to it. Only a real focus change sets this; tree edits that take a
	// target pane directly (SplitPane, ClosePane) leave it alone.
	prevFocus *PaneId
	// nextID is the next id SplitPane will hand out.
	nextID uint32
}

// SwapPanes exchanges leaf positions without changing pane identities or
// split ratios.
func (l *TileLayout) SwapPanes(first, second PaneId) bool {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::swap_panes")
}

// NewTileLayout starts a layout with a single pane, id 1. It returns the
// layout and that pane's id so the caller can set up what it shows.
func NewTileLayout() (TileLayout, PaneId) {
	rootID := PaneId(1)
	return TileLayout{root: NodePane{ID: rootID}, focus: rootID, nextID: 2}, rootID
}

func (l *TileLayout) allocateID() PaneId {
	id := PaneId(l.nextID)
	l.nextID++
	return id
}

// setFocus moves focus, recording the pane being left. It does nothing when
// id is already focused.
func (l *TileLayout) setFocus(id PaneId) {
	if id != l.focus {
		previous := l.focus
		l.prevFocus = &previous
		l.focus = id
	}
}

func (l TileLayout) Focused() PaneId {
	return l.focus
}

// PreviousFocus is the pane focused before this one, while it is still in
// the layout. Closing a pane forgets it rather than leaving a stale id
// behind.
func (l TileLayout) PreviousFocus() (PaneId, bool) {
	if l.prevFocus == nil {
		return 0, false
	}
	return *l.prevFocus, true
}

func (l TileLayout) PaneCount() int {
	return countPanes(l.root)
}

// Panes computes every pane's rectangle within area.
func (l TileLayout) Panes(area uv.Rectangle) []PaneInfo {
	var result []PaneInfo
	collectPanes(l.root, area, l.focus, &result)
	return result
}

// Splits computes every split boundary within area, for drag or key resize.
func (l TileLayout) Splits(area uv.Rectangle) []SplitBorder {
	var result []SplitBorder
	collectSplits(l.root, area, nil, &result)
	return result
}

// CanSplit reports whether splitting target along direction would leave
// both halves at least [MinPaneWidth] columns wide and [MinPaneHeight] rows
// tall. False when target is not in the layout.
func (l TileLayout) CanSplit(target PaneId, direction chat.Direction, area uv.Rectangle) bool {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::can_split")
}

// SplitPane splits target without moving focus. It returns the new pane's
// id, or false when target is not in the layout or [TileLayout.CanSplit]
// refuses the split.
func (l *TileLayout) SplitPane(target PaneId, direction chat.Direction, ratio float32, area uv.Rectangle) (PaneId, bool) {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::split_pane")
}

// CloseFocused closes the focused pane, moving focus to the pane it was
// opened from when that pane still exists, or to a tree-order neighbor
// otherwise. It returns false when it is the last pane.
func (l *TileLayout) CloseFocused() bool {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::close_focused")
}

// ClosePane closes any pane. Focus and its history are untouched unless id
// was the focused pane, in which case this behaves like CloseFocused.
func (l *TileLayout) ClosePane(id PaneId) bool {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::close_pane")
}

func (l *TileLayout) FocusPane(id PaneId) {
	for _, existing := range l.PaneIDs() {
		if existing == id {
			l.setFocus(id)
			return
		}
	}
}

// SetRatioAt sets the ratio of the split at path, clamped to 0.1..=0.9.
func (l *TileLayout) SetRatioAt(path []bool, ratio float32) bool {
	return setRatioAt(&l.root, path, min(max(ratio, 0.1), 0.9))
}

// ResizeFocused grows or shrinks the focused pane toward nav by delta,
// adjusting the nearest matching split.
func (l *TileLayout) ResizeFocused(nav NavDirection, delta float32, area uv.Rectangle) {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::resize_focused")
}

// ResizePane resizes paneID as if it were focused, without disturbing the
// real focus. It returns whether any ratio changed.
func (l *TileLayout) ResizePane(paneID PaneId, nav NavDirection, delta float32, area uv.Rectangle) bool {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::resize_pane")
}

func (l TileLayout) PaneIDs() []PaneId {
	var ids []PaneId
	collectIDs(l.root, &ids)
	return ids
}

// Root is the tree's root, for saving the layout.
func (l TileLayout) Root() Node {
	return l.root
}

// TileLayoutFromSaved rebuilds a layout from a saved tree. It allocates new
// ids starting one above the highest id already present, so a later split
// never collides with a restored one.
func TileLayoutFromSaved(root Node, focus PaneId) TileLayout {
	var ids []PaneId
	collectIDs(root, &ids)
	highest := uint32(0)
	for _, id := range ids {
		highest = max(highest, uint32(id))
	}
	return TileLayout{root: root, focus: focus, nextID: highest + 1}
}

// ToConversationLayout exports the tree, its focus, and the session each
// pane shows in the form the workspace store keeps.
func (l TileLayout) ToConversationLayout(sessions map[PaneId]string) core.ConversationLayout {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::to_conversation_layout")
}

// TileLayoutFromConversationLayout rebuilds a layout and its pane sessions
// from a stored arrangement. A stored focus that is not in the tree falls
// back to the first pane, so a damaged record still opens.
func TileLayoutFromConversationLayout(layout *core.ConversationLayout) (TileLayout, map[PaneId]string) {
	panic("unported: mj-tui/src/tile_layout.rs TileLayout::from_conversation_layout")
}

func savedNode(node Node) core.LayoutNode {
	panic("unported: mj-tui/src/tile_layout.rs saved_node")
}

func liveNode(node core.LayoutNode) Node {
	panic("unported: mj-tui/src/tile_layout.rs live_node")
}

// --- Directional pane navigation ---

// FindInDirection finds the nearest pane in direction from focused,
// preferring the closest edge, then the largest overlap along the shared
// axis, then layout order.
func FindInDirection(focused *PaneInfo, direction NavDirection, panes []PaneInfo) (PaneId, bool) {
	panic("unported: mj-tui/src/tile_layout.rs find_in_direction")
}

func rangesOverlap(aStart, aLen, bStart, bLen int) bool {
	return aStart < bStart+bLen && aStart+aLen > bStart
}

func splitOnRequestedEdge(split *SplitBorder, focused uv.Rectangle, nav NavDirection) bool {
	return splitEdgeDistance(split, focused, nav) <= 1
}

func splitAreaOverlapsFocusedPane(split *SplitBorder, focused uv.Rectangle, nav NavDirection) bool {
	switch nav {
	case NavDirectionLeft, NavDirectionRight:
		return rangesOverlap(split.Area.Min.Y, split.Area.Dy(), focused.Min.Y, focused.Dy())
	default:
		return rangesOverlap(split.Area.Min.X, split.Area.Dx(), focused.Min.X, focused.Dx())
	}
}

// nearestResizeSplit is the split closest to the edge of focused that nav
// points at, among splits running the right way and overlapping the focused
// pane's other axis.
func nearestResizeSplit(splits []SplitBorder, targetDirection chat.Direction, focused uv.Rectangle, nav NavDirection) *SplitBorder {
	panic("unported: mj-tui/src/tile_layout.rs nearest_resize_split")
}

func oppositeDirection(nav NavDirection) NavDirection {
	switch nav {
	case NavDirectionLeft:
		return NavDirectionRight
	case NavDirectionRight:
		return NavDirectionLeft
	case NavDirectionUp:
		return NavDirectionDown
	default:
		return NavDirectionUp
	}
}

func splitEdgeDistance(split *SplitBorder, focused uv.Rectangle, nav NavDirection) uint32 {
	panic("unported: mj-tui/src/tile_layout.rs split_edge_distance")
}

func rangeOverlapAmount(aStart, aLen, bStart, bLen int) int {
	panic("unported: mj-tui/src/tile_layout.rs range_overlap_amount")
}

func rangeCenterDistance(aStart, aLen, bStart, bLen int) int {
	panic("unported: mj-tui/src/tile_layout.rs range_center_distance")
}

// --- Tree operations ---

func countPanes(node Node) int {
	if split, ok := node.(NodeSplit); ok {
		return countPanes(split.First) + countPanes(split.Second)
	}
	return 1
}

func collectPanes(node Node, area uv.Rectangle, focus PaneId, result *[]PaneInfo) {
	switch node := node.(type) {
	case NodePane:
		*result = append(*result, PaneInfo{ID: node.ID, Rect: area, IsFocused: node.ID == focus})
	case NodeSplit:
		first, second := SplitRect(area, node.Direction, node.Ratio)
		collectPanes(node.First, first, focus, result)
		collectPanes(node.Second, second, focus, result)
	}
}

func collectSplits(node Node, area uv.Rectangle, path []bool, result *[]SplitBorder) {
	panic("unported: mj-tui/src/tile_layout.rs collect_splits")
}

func collectIDs(node Node, ids *[]PaneId) {
	switch node := node.(type) {
	case NodePane:
		*ids = append(*ids, node.ID)
	case NodeSplit:
		collectIDs(node.First, ids)
		collectIDs(node.Second, ids)
	}
}

// splitRatio is one split's path and ratio.
type splitRatio struct {
	path  []bool
	ratio float32
}

// splitRatios lists every split's path and ratio, for detecting whether a
// resize changed anything.
func splitRatios(node Node) []splitRatio {
	panic("unported: mj-tui/src/tile_layout.rs split_ratios")
}

func splitAt(node Node, target PaneId, direction chat.Direction, newID PaneId, ratio float32) Node {
	panic("unported: mj-tui/src/tile_layout.rs split_at")
}

func validSplitRatio(ratio float32) float32 {
	if math.IsNaN(float64(ratio)) || math.IsInf(float64(ratio), 0) {
		return 0.5
	}
	return min(max(ratio, 0.1), 0.9)
}

// removePane drops target from the tree; nil when nothing is left.
func removePane(node Node, target PaneId) Node {
	panic("unported: mj-tui/src/tile_layout.rs remove_pane")
}

// setRatioAt sets the ratio of the split at path below the subtree in slot.
// Nodes are values, so the split is replaced in its slot.
func setRatioAt(slot *Node, path []bool, newRatio float32) bool {
	panic("unported: mj-tui/src/tile_layout.rs set_ratio_at")
}

func getRatioAt(node Node, path []bool) (float32, bool) {
	split, ok := node.(NodeSplit)
	if !ok {
		return 0, false
	}
	if len(path) == 0 {
		return split.Ratio, true
	}
	if path[0] {
		return getRatioAt(split.Second, path[1:])
	}
	return getRatioAt(split.First, path[1:])
}

// SplitRect divides area into two rects along direction, giving ratio of it
// to the first.
func SplitRect(area uv.Rectangle, direction chat.Direction, ratio float32) (uv.Rectangle, uv.Rectangle) {
	panic("unported: mj-tui/src/tile_layout.rs split_rect")
}
