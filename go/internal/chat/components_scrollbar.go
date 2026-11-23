// Port of mj-chat/src/components/scrollbar.rs.

package chat

import (
	"math"

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
	if !ok {
		geometry = ScrollbarGeometry{}
	}
	sameTrack := d.hasGeometry == ok && (!ok || d.geometry.Track == geometry.Track)
	if d.dragging && !sameTrack {
		d.dragging = false
	}
	d.geometry, d.hasGeometry = geometry, ok
}

// HandleMouse offers a mouse event to the scrollbar.
func (d *ScrollbarDrag) HandleMouse(mouse tea.MouseMsg) ScrollbarPointer {
	kind, button, x, y := classifyMouse(mouse)
	if d.dragging {
		switch {
		case kind == mouseKindDrag && button == tea.MouseLeft:
			return d.seek(y)
		case kind == mouseKindUp && button == tea.MouseLeft:
			d.dragging = false
			return ScrollbarPointerConsumed{}
		case kind == mouseKindDown:
			d.dragging = false
		default:
			return ScrollbarPointerConsumed{}
		}
	}
	if kind != mouseKindDown || button != tea.MouseLeft {
		return ScrollbarPointerIgnored{}
	}
	if !d.hasGeometry {
		return ScrollbarPointerIgnored{}
	}
	geometry := d.geometry
	if geometry.MaxScroll == 0 ||
		x != geometry.Track.Min.X ||
		y < geometry.Track.Min.Y ||
		y >= geometry.Track.Max.Y {
		return ScrollbarPointerIgnored{}
	}
	onThumb := y >= geometry.Thumb.Min.Y && y < geometry.Thumb.Max.Y
	if onThumb {
		d.grabOffset = y - geometry.Thumb.Min.Y
	} else {
		d.grabOffset = geometry.Thumb.Dy() / 2
	}
	d.dragging = true
	if onThumb {
		return ScrollbarPointerConsumed{}
	}
	return d.seek(y)
}

func (d ScrollbarDrag) seek(pointerRow int) ScrollbarPointer {
	if !d.hasGeometry {
		return ScrollbarPointerConsumed{}
	}
	geometry := d.geometry
	travel := max(geometry.Track.Dy()-geometry.Thumb.Dy(), 0)
	thumbTop := min(max(max(pointerRow-geometry.Track.Min.Y, 0)-d.grabOffset, 0), travel)
	target := 0
	if travel != 0 {
		numerator := saturatingMul(thumbTop, geometry.MaxScroll)
		target = min(saturatingAdd(numerator, travel/2)/travel, geometry.MaxScroll)
	}
	return ScrollbarPointerScrollTo{Offset: target}
}

// saturatingMul is a * b for non-negative operands, saturating at
// math.MaxInt as Rust's `saturating_mul` does.
func saturatingMul(a, b int) int {
	if a == 0 || b == 0 {
		return 0
	}
	if a > math.MaxInt/b {
		return math.MaxInt
	}
	return a * b
}

// saturatingAdd is a + b for non-negative operands, saturating at
// math.MaxInt.
func saturatingAdd(a, b int) int {
	if a > math.MaxInt-b {
		return math.MaxInt
	}
	return a + b
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
	first := len(rowHeights)
	used := 0
	for first > 0 {
		used = saturatingAdd(used, rowHeights[first-1])
		if used > viewportHeight {
			break
		}
		first--
	}
	return min(offset, first)
}

// CenteredOffset is the offset that centres selected in the viewport,
// pinned to the list ends.
//
// Near the top the list starts at its first row, near the bottom it ends at
// its last row, and a list that fits never scrolls. With variable row
// heights the selected row is centred as closely as whole rows allow.
func CenteredOffset(selected int, rowHeights []int, viewportHeight int) int {
	if selected >= len(rowHeights) {
		return ClampOffsetToLastPage(math.MaxInt, rowHeights, viewportHeight)
	}
	roomAbove := max(viewportHeight-rowHeights[selected], 0) / 2
	offset := selected
	above := 0
	for offset > 0 && saturatingAdd(above, rowHeights[offset-1]) <= roomAbove {
		offset--
		above += rowHeights[offset]
	}
	return ClampOffsetToLastPage(offset, rowHeights, viewportHeight)
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
	if track.Dx() == 0 || track.Dy() == 0 {
		return ScrollbarGeometry{}, false
	}
	viewportContentLength = max(viewportContentLength, 1)
	totalLength := max(contentLength, viewportContentLength)
	maxScroll := totalLength - viewportContentLength
	scroll := min(position, maxScroll)
	trackHeight := track.Dy()
	thumbHeight := trackHeight
	if maxScroll != 0 {
		proportional := max(saturatingMul(trackHeight, viewportContentLength)/totalLength, 1)
		thumbHeight = min(proportional, trackHeight)
	}
	travel := max(trackHeight-thumbHeight, 0)
	thumbOffset := 0
	if maxScroll != 0 {
		thumbOffset = saturatingMul(travel, scroll) / maxScroll
	}
	return ScrollbarGeometry{
		Track:     track,
		Thumb:     uv.Rect(track.Min.X, track.Min.Y+thumbOffset, track.Dx(), thumbHeight),
		MaxScroll: maxScroll,
	}, true
}

// ScrollbarMarkRow is the track row for a content offset, for a mark drawn
// on the scrollbar.
//
// contentLength is the length the geometry was calculated from. Offsets map
// in proportion to the whole track, so a mark for a visible row lands on or
// next to the thumb. Offsets past the end land on the last row.
func ScrollbarMarkRow(geometry ScrollbarGeometry, contentLength int, offset int) int {
	height := geometry.Track.Dy()
	row := saturatingMul(offset, height) / max(contentLength, 1)
	row = min(row, max(height-1, 0))
	return geometry.Track.Min.Y + row
}

// RenderScrollbar paints a scrollbar from previously calculated geometry.
func RenderScrollbar(frame *Frame, geometry ScrollbarGeometry) {
	glyphs := ThemeGlyphs()
	palette := ThemePalette()
	for row := geometry.Track.Min.Y; row < geometry.Track.Max.Y; row++ {
		isThumb := row >= geometry.Thumb.Min.Y && row < geometry.Thumb.Max.Y
		symbol, color := glyphs.ScrollTrack, palette.Border
		if isThumb {
			symbol, color = glyphs.ScrollThumb, palette.Muted
		}
		BufferSetCell(frame.Buffer(), geometry.Track.Min.X, row, symbol, Style{}.Foreground(color))
	}
}

// RenderMarkedScrollbar paints a scrollbar with a mark on each of the given
// track rows. The thumb covers any mark under it, and rows outside the
// track are ignored.
func RenderMarkedScrollbar(frame *Frame, geometry ScrollbarGeometry, marks []int, markStyle Style) {
	RenderScrollbar(frame, geometry)
	for _, row := range marks {
		onTrack := row >= geometry.Track.Min.Y && row < geometry.Track.Max.Y
		onThumb := row >= geometry.Thumb.Min.Y && row < geometry.Thumb.Max.Y
		if onTrack && !onThumb {
			BufferSetCell(frame.Buffer(), geometry.Track.Min.X, row, ThemeGlyphs().ScrollMark, markStyle)
		}
	}
}
