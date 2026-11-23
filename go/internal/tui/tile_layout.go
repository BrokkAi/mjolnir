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
	"cmp"
	"math"
	"slices"

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
	if first == second {
		return false
	}
	ids := l.PaneIDs()
	if !slices.Contains(ids, first) || !slices.Contains(ids, second) {
		return false
	}
	var swap func(node Node) Node
	swap = func(node Node) Node {
		switch node := node.(type) {
		case NodePane:
			switch node.ID {
			case first:
				return NodePane{ID: second}
			case second:
				return NodePane{ID: first}
			}
			return node
		case NodeSplit:
			node.First = swap(node.First)
			node.Second = swap(node.Second)
			return node
		}
		return node
	}
	l.root = swap(l.root)
	return true
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
	var targetRect uv.Rectangle
	found := false
	for _, pane := range l.Panes(area) {
		if pane.ID == target {
			targetRect, found = pane.Rect, true
			break
		}
	}
	if !found {
		return false
	}
	first, second := SplitRect(targetRect, direction, 0.5)
	for _, half := range []uv.Rectangle{first, second} {
		if half.Dx() < MinPaneWidth || half.Dy() < MinPaneHeight {
			return false
		}
	}
	return true
}

// SplitPane splits target without moving focus. It returns the new pane's
// id, or false when target is not in the layout or [TileLayout.CanSplit]
// refuses the split.
func (l *TileLayout) SplitPane(target PaneId, direction chat.Direction, ratio float32, area uv.Rectangle) (PaneId, bool) {
	if !l.CanSplit(target, direction, area) {
		return 0, false
	}
	newID := l.allocateID()
	l.root = splitAt(l.root, target, direction, newID, validSplitRatio(ratio))
	return newID, true
}

// CloseFocused closes the focused pane, moving focus to the pane it was
// opened from when that pane still exists, or to a tree-order neighbor
// otherwise. It returns false when it is the last pane.
func (l *TileLayout) CloseFocused() bool {
	if l.PaneCount() <= 1 {
		return false
	}
	target := l.focus
	ids := l.PaneIDs()
	position := slices.Index(ids, target)
	if position < 0 {
		panic("focused pane is in the layout")
	}
	var neighbor PaneId
	if position+1 < len(ids) {
		neighbor = ids[position+1]
	} else {
		neighbor = ids[position-1]
	}
	newFocus := neighbor
	if prev := l.prevFocus; prev != nil && *prev != target && slices.Contains(ids, *prev) {
		newFocus = *prev
	}
	newRoot := removePane(l.root, target)
	if newRoot == nil {
		return false
	}
	l.root = newRoot
	l.focus = newFocus
	l.prevFocus = nil
	return true
}

// ClosePane closes any pane. Focus and its history are untouched unless id
// was the focused pane, in which case this behaves like CloseFocused.
func (l *TileLayout) ClosePane(id PaneId) bool {
	if l.focus == id {
		return l.CloseFocused()
	}
	if l.PaneCount() <= 1 || !slices.Contains(l.PaneIDs(), id) {
		return false
	}
	newRoot := removePane(l.root, id)
	if newRoot == nil {
		return false
	}
	l.root = newRoot
	if l.prevFocus != nil && *l.prevFocus == id {
		l.prevFocus = nil
	}
	return true
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
	panes := l.Panes(area)
	var focused *PaneInfo
	for index := range panes {
		if panes[index].IsFocused {
			focused = &panes[index]
			break
		}
	}
	if focused == nil {
		return
	}
	focusedRect := focused.Rect
	splits := l.Splits(area)

	targetDirection := chat.DirectionVertical
	if nav == NavDirectionLeft || nav == NavDirectionRight {
		targetDirection = chat.DirectionHorizontal
	}
	grows := nav == NavDirectionRight || nav == NavDirectionDown

	chosen := nearestResizeSplit(splits, targetDirection, focusedRect, nav)
	if chosen == nil {
		chosen = nearestResizeSplit(splits, targetDirection, focusedRect, oppositeDirection(nav))
	}
	if chosen != nil {
		path := slices.Clone(chosen.Path)
		currentRatio, ok := getRatioAt(l.root, path)
		if !ok {
			currentRatio = 0.5
		}
		adjustment := -delta
		if grows {
			adjustment = delta
		}
		l.SetRatioAt(path, currentRatio+adjustment)
	}
}

// ResizePane resizes paneID as if it were focused, without disturbing the
// real focus. It returns whether any ratio changed.
func (l *TileLayout) ResizePane(paneID PaneId, nav NavDirection, delta float32, area uv.Rectangle) bool {
	if !slices.Contains(l.PaneIDs(), paneID) {
		return false
	}
	before := splitRatios(l.root)
	previousFocus := l.focus
	l.focus = paneID
	l.ResizeFocused(nav, delta, area)
	l.focus = previousFocus
	after := splitRatios(l.root)
	return !slices.EqualFunc(before, after, func(a, b splitRatio) bool {
		return a.ratio == b.ratio && slices.Equal(a.path, b.path)
	})
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
	saved := make(map[uint32]string, len(sessions))
	for pane, session := range sessions {
		saved[pane.Raw()] = session
	}
	return core.ConversationLayout{
		Browse:   nil,
		Pins:     map[string]uint32{},
		Root:     savedNode(l.root),
		Focus:    l.focus.Raw(),
		Sessions: saved,
	}
}

// TileLayoutFromConversationLayout rebuilds a layout and its pane sessions
// from a stored arrangement. A stored focus that is not in the tree falls
// back to the first pane, so a damaged record still opens.
func TileLayoutFromConversationLayout(layout *core.ConversationLayout) (TileLayout, map[PaneId]string) {
	root := liveNode(layout.Root)
	var ids []PaneId
	collectIDs(root, &ids)
	focus := PaneId(layout.Focus)
	if !slices.Contains(ids, focus) {
		if len(ids) == 0 {
			panic("a saved layout has at least one pane")
		}
		focus = ids[0]
	}
	sessions := map[PaneId]string{}
	for pane, session := range layout.Sessions {
		if slices.Contains(ids, PaneId(pane)) {
			sessions[PaneId(pane)] = session
		}
	}
	return TileLayoutFromSaved(root, focus), sessions
}

func savedNode(node Node) core.LayoutNode {
	switch node := node.(type) {
	case NodeSplit:
		axis := core.SplitAxisVertical
		if node.Direction == chat.DirectionHorizontal {
			axis = core.SplitAxisHorizontal
		}
		return core.LayoutNodeSplit{
			Axis:   axis,
			Ratio:  node.Ratio,
			First:  savedNode(node.First),
			Second: savedNode(node.Second),
		}
	case NodePane:
		return core.LayoutNodePane{ID: node.ID.Raw()}
	}
	panic("tile layout node is a pane or a split")
}

func liveNode(node core.LayoutNode) Node {
	switch node := node.(type) {
	case core.LayoutNodeSplit:
		direction := chat.DirectionVertical
		if node.Axis == core.SplitAxisHorizontal {
			direction = chat.DirectionHorizontal
		}
		return NodeSplit{
			Direction: direction,
			Ratio:     node.Ratio,
			First:     liveNode(node.First),
			Second:    liveNode(node.Second),
		}
	case core.LayoutNodePane:
		return NodePane{ID: PaneId(node.ID)}
	}
	panic("saved layout node is a pane or a split")
}

// --- Directional pane navigation ---

// FindInDirection finds the nearest pane in direction from focused,
// preferring the closest edge, then the largest overlap along the shared
// axis, then layout order.
func FindInDirection(focused *PaneInfo, direction NavDirection, panes []PaneInfo) (PaneId, bool) {
	from := focused.Rect
	type candidate struct {
		edgeDistance   int
		overlap        int
		centerDistance int
		index          int
		id             PaneId
	}
	var best *candidate
	for index, pane := range panes {
		if pane.ID == focused.ID {
			continue
		}
		r := pane.Rect
		var inDirection bool
		switch direction {
		case NavDirectionLeft:
			inDirection = r.Min.X+r.Dx() <= from.Min.X && rangesOverlap(r.Min.Y, r.Dy(), from.Min.Y, from.Dy())
		case NavDirectionRight:
			inDirection = r.Min.X >= from.Min.X+from.Dx() && rangesOverlap(r.Min.Y, r.Dy(), from.Min.Y, from.Dy())
		case NavDirectionUp:
			inDirection = r.Min.Y+r.Dy() <= from.Min.Y && rangesOverlap(r.Min.X, r.Dx(), from.Min.X, from.Dx())
		case NavDirectionDown:
			inDirection = r.Min.Y >= from.Min.Y+from.Dy() && rangesOverlap(r.Min.X, r.Dx(), from.Min.X, from.Dx())
		}
		if !inDirection {
			continue
		}
		next := candidate{index: index, id: pane.ID}
		switch direction {
		case NavDirectionLeft:
			next.edgeDistance = max(from.Min.X-(r.Min.X+r.Dx()), 0)
		case NavDirectionRight:
			next.edgeDistance = max(r.Min.X-(from.Min.X+from.Dx()), 0)
		case NavDirectionUp:
			next.edgeDistance = max(from.Min.Y-(r.Min.Y+r.Dy()), 0)
		case NavDirectionDown:
			next.edgeDistance = max(r.Min.Y-(from.Min.Y+from.Dy()), 0)
		}
		if direction == NavDirectionLeft || direction == NavDirectionRight {
			next.overlap = rangeOverlapAmount(r.Min.Y, r.Dy(), from.Min.Y, from.Dy())
			next.centerDistance = rangeCenterDistance(r.Min.Y, r.Dy(), from.Min.Y, from.Dy())
		} else {
			next.overlap = rangeOverlapAmount(r.Min.X, r.Dx(), from.Min.X, from.Dx())
			next.centerDistance = rangeCenterDistance(r.Min.X, r.Dx(), from.Min.X, from.Dx())
		}
		// Rust's min_by_key over (edge distance, Reverse(overlap), center
		// distance, index), where the first minimum wins.
		if best == nil || cmp.Or(
			cmp.Compare(next.edgeDistance, best.edgeDistance),
			cmp.Compare(best.overlap, next.overlap),
			cmp.Compare(next.centerDistance, best.centerDistance),
			cmp.Compare(next.index, best.index),
		) < 0 {
			best = &next
		}
	}
	if best == nil {
		return 0, false
	}
	return best.id, true
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
	var best *SplitBorder
	var bestDistance uint32
	for index := range splits {
		split := &splits[index]
		if split.Direction != targetDirection ||
			!splitAreaOverlapsFocusedPane(split, focused, nav) ||
			!splitOnRequestedEdge(split, focused, nav) {
			continue
		}
		distance := splitEdgeDistance(split, focused, nav)
		if best == nil || distance < bestDistance {
			best, bestDistance = split, distance
		}
	}
	return best
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
	var difference int
	switch nav {
	case NavDirectionLeft:
		difference = split.Pos - focused.Min.X
	case NavDirectionRight:
		difference = split.Pos - (focused.Min.X + focused.Dx())
	case NavDirectionUp:
		difference = split.Pos - focused.Min.Y
	default:
		difference = split.Pos - (focused.Min.Y + focused.Dy())
	}
	if difference < 0 {
		difference = -difference
	}
	return uint32(difference)
}

func rangeOverlapAmount(aStart, aLen, bStart, bLen int) int {
	aEnd := tileSaturatingCell(aStart + aLen)
	bEnd := tileSaturatingCell(bStart + bLen)
	return max(min(aEnd, bEnd)-max(aStart, bStart), 0)
}

func rangeCenterDistance(aStart, aLen, bStart, bLen int) int {
	aCenter := tileSaturatingCell(tileSaturatingCell(aStart*2) + aLen)
	bCenter := tileSaturatingCell(tileSaturatingCell(bStart*2) + bLen)
	if aCenter > bCenter {
		return aCenter - bCenter
	}
	return bCenter - aCenter
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
	split, ok := node.(NodeSplit)
	if !ok {
		return
	}
	first, second := SplitRect(area, split.Direction, split.Ratio)
	pos := first.Min.Y + first.Dy()
	if split.Direction == chat.DirectionHorizontal {
		pos = first.Min.X + first.Dx()
	}
	*result = append(*result, SplitBorder{
		Pos:       pos,
		Direction: split.Direction,
		Ratio:     split.Ratio,
		Area:      area,
		Path:      slices.Clone(path),
	})
	collectSplits(split.First, first, append(slices.Clone(path), false), result)
	collectSplits(split.Second, second, append(slices.Clone(path), true), result)
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
	var out []splitRatio
	var walk func(node Node, path []bool)
	walk = func(node Node, path []bool) {
		split, ok := node.(NodeSplit)
		if !ok {
			return
		}
		out = append(out, splitRatio{path: slices.Clone(path), ratio: split.Ratio})
		walk(split.First, append(slices.Clone(path), false))
		walk(split.Second, append(slices.Clone(path), true))
	}
	walk(node, nil)
	return out
}

func splitAt(node Node, target PaneId, direction chat.Direction, newID PaneId, ratio float32) Node {
	switch current := node.(type) {
	case NodePane:
		if current.ID == target {
			return NodeSplit{
				Direction: direction,
				Ratio:     ratio,
				First:     NodePane{ID: current.ID},
				Second:    NodePane{ID: newID},
			}
		}
		return node
	case NodeSplit:
		return NodeSplit{
			Direction: current.Direction,
			Ratio:     current.Ratio,
			First:     splitAt(current.First, target, direction, newID, ratio),
			Second:    splitAt(current.Second, target, direction, newID, ratio),
		}
	}
	return node
}

func validSplitRatio(ratio float32) float32 {
	if math.IsNaN(float64(ratio)) || math.IsInf(float64(ratio), 0) {
		return 0.5
	}
	return min(max(ratio, 0.1), 0.9)
}

// removePane drops target from the tree; nil when nothing is left.
func removePane(node Node, target PaneId) Node {
	switch current := node.(type) {
	case NodePane:
		if current.ID == target {
			return nil
		}
		return node
	case NodeSplit:
		first := removePane(current.First, target)
		second := removePane(current.Second, target)
		switch {
		case first == nil && second != nil:
			return second
		case first != nil && second == nil:
			return first
		case first != nil && second != nil:
			return NodeSplit{
				Direction: current.Direction,
				Ratio:     current.Ratio,
				First:     first,
				Second:    second,
			}
		}
		return nil
	}
	return node
}

// setRatioAt sets the ratio of the split at path below the subtree in slot.
// Nodes are values, so the split is replaced in its slot.
func setRatioAt(slot *Node, path []bool, newRatio float32) bool {
	split, ok := (*slot).(NodeSplit)
	if !ok {
		return false
	}
	var changed bool
	switch {
	case len(path) == 0:
		split.Ratio = newRatio
		changed = true
	case path[0]:
		changed = setRatioAt(&split.Second, path[1:], newRatio)
	default:
		changed = setRatioAt(&split.First, path[1:], newRatio)
	}
	*slot = split
	return changed
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
	x, y := area.Min.X, area.Min.Y
	if direction == chat.DirectionHorizontal {
		firstWidth := tileRoundCell(float32(area.Dx()) * ratio)
		secondWidth := max(area.Dx()-firstWidth, 0)
		return uv.Rect(x, y, firstWidth, area.Dy()),
			uv.Rect(tileSaturatingCell(x+firstWidth), y, secondWidth, area.Dy())
	}
	firstHeight := tileRoundCell(float32(area.Dy()) * ratio)
	secondHeight := max(area.Dy()-firstHeight, 0)
	return uv.Rect(x, y, area.Dx(), firstHeight),
		uv.Rect(x, tileSaturatingCell(y+firstHeight), area.Dx(), secondHeight)
}

// tileRoundCell is Rust's `value.round() as u16`: rounded half away from zero,
// with the cast saturating at 0 and u16::MAX (NaN becomes 0).
func tileRoundCell(value float32) int {
	rounded := math.Round(float64(value))
	if math.IsNaN(rounded) || rounded <= 0 {
		return 0
	}
	return int(min(rounded, math.MaxUint16))
}

// tileSaturatingCell clamps a cell coordinate to u16's range, as Rust's
// saturating u16 arithmetic does.
func tileSaturatingCell(value int) int {
	return min(max(value, 0), math.MaxUint16)
}
