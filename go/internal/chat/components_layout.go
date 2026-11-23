// Port of mj-chat/src/components/layout.rs.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// Layout helpers for forms and dialogs.

// PopupSide is which side of an anchor an inline popup should prefer.
type PopupSide int

const (
	// PopupSideBelow places the popup below the anchor when there is room.
	PopupSideBelow PopupSide = iota
	// PopupSideAbove places the popup above the anchor when there is room.
	PopupSideAbove
)

// AutocompletePopup is the shared framed shell for compact, anchored choice
// popups.
type AutocompletePopup struct{}

// AutocompletePopupRender draws an anchored popup shell and returns its
// outer and inner regions (Rust's `AutocompletePopup::render`).
//
// The caller owns the rows and selection, and says how many are visible at
// once. This helper only chooses a clipped rectangle, clears it, and draws
// the modal frame. ok is false when nothing could be drawn.
func AutocompletePopupRender(
	frame *Frame,
	bounds uv.Rectangle,
	anchor uv.Rectangle,
	width int,
	rows int,
	maxRows int,
	title string,
	preferred PopupSide,
) (outer uv.Rectangle, inner uv.Rectangle, ok bool) {
	visible := min(rows, maxRows)
	if visible == 0 || bounds.Dx() == 0 || bounds.Dy() == 0 {
		return uv.Rectangle{}, uv.Rectangle{}, false
	}
	height := min(min(visible, 0xFFFF)+2, bounds.Dy())
	width = min(max(width, 4), bounds.Dx())
	x := max(min(anchor.Min.X, max(bounds.Max.X-width, 0)), bounds.Min.X)
	below := min(anchor.Max.Y, bounds.Max.Y)
	above := max(anchor.Min.Y-height, 0)
	fitsBelow := below+height <= bounds.Max.Y
	fitsAbove := anchor.Min.Y >= bounds.Min.Y+height
	var y int
	switch {
	case preferred == PopupSideBelow && fitsBelow:
		y = below
	case preferred == PopupSideAbove && fitsAbove:
		y = above
	case fitsBelow:
		y = below
	case fitsAbove:
		y = above
	default:
		y = bounds.Min.Y + max(bounds.Dy()-height, 0)/2
	}
	outer = uv.Rect(x, y, width, height)
	frame.RenderWidget(Clear{}, outer)
	block := Modal().WithTitle(RawLine(title))
	inner = block.Inner(outer)
	frame.RenderWidget(block, outer)
	return outer, inner, true
}

// DialogRect returns a centered dialog rectangle using the shared modal
// margin rules.
func DialogRect(area uv.Rectangle, widthPercent int, height int) uv.Rectangle {
	return CenteredRect(widthPercent, height, area)
}

// DialogContent returns the content area inside a dialog border.
func DialogContent(area uv.Rectangle) uv.Rectangle {
	return BorderedContent(area)
}

// FormArea insets a form region by the shared modal screen margin.
func FormArea(area uv.Rectangle) uv.Rectangle {
	return ModalArea(area)
}

// FormRows splits a region into horizontal form rows.
func FormRows(area uv.Rectangle, rows []int) []uv.Rectangle {
	constraints := make([]Constraint, 0, len(rows))
	for _, row := range rows {
		constraints = append(constraints, ConstraintLength(row))
	}
	return NewLayout(DirectionVertical, constraints).Split(area)
}

// FormColumns splits a region into equal-width columns.
func FormColumns(area uv.Rectangle, count int) []uv.Rectangle {
	if count == 0 {
		return nil
	}
	constraints := make([]Constraint, count)
	for i := range constraints {
		constraints[i] = ConstraintRatio{Num: 1, Den: count}
	}
	return NewLayout(DirectionHorizontal, constraints).Split(area)
}

// RowRange is a half-open block of content rows [Start, End), Rust's
// `std::ops::Range<u16>` in the form viewport.
type RowRange struct {
	Start int
	End   int
}

// FormViewport is a vertically clipped form body, managed by
// [Form.Viewport]. Fixed footer buttons stay outside it.
type FormViewport struct {
	area   uv.Rectangle
	offset int
}

// NewFormViewport computes clipped layout, optionally revealing a half-open
// content row block.
func NewFormViewport(area uv.Rectangle, contentHeight int, previous int, focusedBlock *RowRange) FormViewport {
	maxOffset := max(contentHeight-area.Dy(), 0)
	offset := min(previous, maxOffset)
	if block := focusedBlock; block != nil && block.Start < block.End {
		if block.Start < offset || block.End-block.Start > area.Dy() {
			offset = block.Start
		} else if block.End > offset+area.Dy() {
			offset = max(block.End-area.Dy(), 0)
		}
	}
	return FormViewport{area: area, offset: min(offset, maxOffset)}
}

func (v FormViewport) areaRect() uv.Rectangle {
	return v.area
}

// Offset is the offset to retain for the next frame.
func (v FormViewport) Offset() int {
	return v.offset
}

// Row clips a content row or list to this frame's visible body.
func (v FormViewport) Row(start int, height int) uv.Rectangle {
	top := max(start, v.offset)
	bottom := min(start+height, v.offset+v.area.Dy())
	if top >= bottom {
		return uv.Rectangle{}
	}
	return uv.Rect(v.area.Min.X, v.area.Min.Y+(top-v.offset), v.area.Dx(), bottom-top)
}
