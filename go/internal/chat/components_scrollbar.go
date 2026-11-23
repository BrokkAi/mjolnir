// Port of mj-chat/src/components/scrollbar.rs.

package chat

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

// Shared slim terminal scrollbars.

// TrackSymbol is the dim one-cell rail used by every terminal scrollbar.
const TrackSymbol = "│"

// ThumbSymbol is the accent one-cell thumb used by every terminal scrollbar.
const ThumbSymbol = "▐"

// ScrollbarGeometry is the geometry for a slim vertical scrollbar.
//
// MaxScroll is the greatest content offset represented by the thumb. It is
// kept with the geometry so pointer dragging and rendering use the same
// endpoint mapping.
type ScrollbarGeometry struct {
	Track     uv.Rectangle
	Thumb     uv.Rectangle
	MaxScroll int
}

// ScrollbarPointer is the result of offering a pointer event to a
// scrollbar.
type ScrollbarPointer interface {
	isScrollbarPointer()
}

// ScrollbarPointerIgnored means the scrollbar did not take the event.
type ScrollbarPointerIgnored struct{}

// ScrollbarPointerConsumed means the scrollbar took the event without
// moving the content.
type ScrollbarPointerConsumed struct{}

// ScrollbarPointerScrollTo asks the owner to scroll the content to Offset.
type ScrollbarPointerScrollTo struct {
	Offset int
}

func (ScrollbarPointerIgnored) isScrollbarPointer()  {}
func (ScrollbarPointerConsumed) isScrollbarPointer() {}
func (ScrollbarPointerScrollTo) isScrollbarPointer() {}

// ScrollbarDrag is the mouse state shared by every draggable vertical
// scrollbar. The caller owns the content offset; this only maps track
// clicks and held-thumb motion to it. The zero value has no geometry.
type ScrollbarDrag struct {
	geometry    ScrollbarGeometry
	hasGeometry bool
	dragging    bool
	grabOffset  int
}

// Geometry returns the geometry the scrollbar was last drawn with, if any.
func (d ScrollbarDrag) Geometry() (ScrollbarGeometry, bool) {
	return d.geometry, d.hasGeometry
}

// IsDragging reports whether the thumb is held.
func (d ScrollbarDrag) IsDragging() bool {
	return d.dragging
}

// GrabOffset is the row within the thumb where it was grabbed.
func (d ScrollbarDrag) GrabOffset() int {
	return d.grabOffset
}

// CancelPointer releases a gesture while keeping the currently displayed
// geometry.
func (d *ScrollbarDrag) CancelPointer() {
	d.dragging = false
	d.grabOffset = 0
}

// Clear forgets the geometry and any gesture.
func (d *ScrollbarDrag) Clear() {
	*d = ScrollbarDrag{}
}

// SetGeometry records the geometry drawn this frame; ok false hides the
// scrollbar. A drag ends when the track moves. It takes
// [NewScrollbarGeometry]'s results directly.
func (d *ScrollbarDrag) SetGeometry(geometry ScrollbarGeometry, ok bool) {
	panic("unported: mj-chat/src/components/scrollbar.rs ScrollbarDrag.SetGeometry")
}

// HandleMouse offers a mouse event to the scrollbar.
func (d *ScrollbarDrag) HandleMouse(mouse tea.MouseMsg) ScrollbarPointer {
	panic("unported: mj-chat/src/components/scrollbar.rs ScrollbarDrag.HandleMouse")
}

func (d ScrollbarDrag) seek(pointerRow int) ScrollbarPointer {
	panic("unported: mj-chat/src/components/scrollbar.rs ScrollbarDrag.seek")
}

// ClampOffsetToLastPage clamps a list offset so the viewport never opens on
// a hidden head.
//
// The largest useful offset is the first row from which every remaining row
// fits (the last page). A larger offset leaves blank space below the content
// while earlier rows stay hidden. For unit-height rows this is
// `max(0, len - viewport)`, and a list that fits has offset 0. Row heights
// are in terminal rows.
func ClampOffsetToLastPage(offset int, rowHeights []int, viewportHeight int) int {
	panic("unported: mj-chat/src/components/scrollbar.rs ClampOffsetToLastPage")
}

// CenteredOffset is the offset that centres selected in the viewport,
// pinned to the list ends.
//
// Near the top the list starts at its first row, near the bottom it ends at
// its last row, and a list that fits never scrolls. With variable row
// heights the selected row is centred as closely as whole rows allow.
func CenteredOffset(selected int, rowHeights []int, viewportHeight int) int {
	panic("unported: mj-chat/src/components/scrollbar.rs CenteredOffset")
}

// CenteredUnitOffset is [CenteredOffset] for length rows that are one
// terminal row each.
func CenteredUnitOffset(selected int, length int, viewportHeight int) int {
	maxOffset := max(length-viewportHeight, 0)
	return min(max(selected-viewportHeight/2, 0), maxOffset)
}

// NewScrollbarGeometry calculates scrollbar geometry using a proportional
// thumb and inclusive endpoints (Rust's `scrollbar_geometry`). A
// zero-height or zero-width track has no drawable geometry. Content shorter
// than the viewport produces a full-track thumb so callers that need a
// hidden scrollbar can apply their existing overflow check.
func NewScrollbarGeometry(track uv.Rectangle, contentLength int, position int, viewportContentLength int) (ScrollbarGeometry, bool) {
	panic("unported: mj-chat/src/components/scrollbar.rs NewScrollbarGeometry")
}

// ScrollbarMarkRow is the track row for a content offset, for a mark drawn
// on the scrollbar.
//
// contentLength is the length the geometry was calculated from. Offsets map
// in proportion to the whole track, so a mark for a visible row lands on or
// next to the thumb. Offsets past the end land on the last row.
func ScrollbarMarkRow(geometry ScrollbarGeometry, contentLength int, offset int) int {
	panic("unported: mj-chat/src/components/scrollbar.rs ScrollbarMarkRow")
}

// RenderScrollbar paints a scrollbar from previously calculated geometry.
func RenderScrollbar(frame *Frame, geometry ScrollbarGeometry) {
	panic("unported: mj-chat/src/components/scrollbar.rs RenderScrollbar")
}

// RenderMarkedScrollbar paints a scrollbar with a mark on each of the given
// track rows. The thumb covers any mark under it, and rows outside the
// track are ignored.
func RenderMarkedScrollbar(frame *Frame, geometry ScrollbarGeometry, marks []int, markStyle Style) {
	panic("unported: mj-chat/src/components/scrollbar.rs RenderMarkedScrollbar")
}
