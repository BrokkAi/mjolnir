// Port of mj-chat/src/modal.rs.

// Geometry and background clearing every modal shares: where a dialog sits
// on screen and how much empty space it keeps around itself.
//
// The dashboard and the chat view both draw dialogs, so the rule lives here,
// in the crate they both depend on, rather than once per view. The rendering
// helpers clear the same margin that the geometry reserves.
//
// Rectangles are ultraviolet's (image.Rectangle), standing in for ratatui's
// u16 Rect: sizes never go negative here, where Rust saturates at zero.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// ModalScreenMargin is the number of blank cells kept outside every modal
// border, on every side.
const ModalScreenMargin = 2

// ModalArea is the region a modal may occupy: area inset by
// ModalScreenMargin on every side, so dialogs never butt against the
// terminal border. On a terminal too small to hold the margin it degrades to
// the full area rather than vanish.
func ModalArea(area uv.Rectangle) uv.Rectangle {
	margin := ModalScreenMargin
	if area.Dx() > margin*2 && area.Dy() > margin*2 {
		return uv.Rect(area.Min.X+margin, area.Min.Y+margin, area.Dx()-margin*2, area.Dy()-margin*2)
	}
	return area
}

// BorderedContent is the drawn text inside a full border, which is the part
// of a widget a selection may cover (ratatui's `inner` with a one-cell
// margin: an area too small for the border has no content).
func BorderedContent(area uv.Rectangle) uv.Rectangle {
	if area.Dx() < 2 || area.Dy() < 2 {
		return uv.Rectangle{}
	}
	return uv.Rect(area.Min.X+1, area.Min.Y+1, area.Dx()-2, area.Dy()-2)
}

// DismissibleModalTitle builds the shared upper-left modal title and
// registers its mouse target.
//
// The leading ` × ` occupies the three title cells immediately inside the
// popup's left border. It is deliberately not a form control, so it never
// enters keyboard focus traversal; Escape remains the keyboard equivalent.
func DismissibleModalTitle[K comparable](form *Form[K], popup uv.Rectangle, title string, titleStyle Style, enabled bool) Line {
	hitbox := uv.Rect(popup.Min.X+1, popup.Min.Y, min(max(popup.Dx()-2, 0), 3), min(popup.Dy(), 1))
	form.RegisterDismiss(hitbox, enabled)
	var dismissStyle Style
	switch {
	case !enabled:
		dismissStyle = Muted().Patch(Raised())
	case form.DismissIsArmed():
		dismissStyle = FocusControl()
	default:
		dismissStyle = ActionableChip()
	}
	return LineFrom(
		StyledSpan(ThemeGlyphs().Close, dismissStyle),
		StyledSpan(title, titleStyle),
		StyledSpan(" ", titleStyle),
	)
}

// clearModal clears a modal and the empty cells it keeps around its border.
//
// bounds is the area the modal overlays. Clipping the expanded rectangle to
// it keeps a pane-local modal from erasing a neighboring pane and keeps tiny
// terminal coordinates valid.
func clearModal(frame *Frame, popup, bounds uv.Rectangle) {
	area := modalClearArea(popup, bounds)
	frame.RenderWidget(Clear{}, area)
	frame.RenderWidget(NewBlock().WithStyle(Base()), area)
}

// modalClearArea is popup grown by ModalScreenMargin on every side (ratatui's
// `outer`) and clipped to bounds (ratatui's `intersection`, which keeps the
// corner of an empty overlap).
func modalClearArea(popup, bounds uv.Rectangle) uv.Rectangle {
	x := max(popup.Min.X-ModalScreenMargin, 0)
	y := max(popup.Min.Y-ModalScreenMargin, 0)
	right := popup.Max.X + ModalScreenMargin
	bottom := popup.Max.Y + ModalScreenMargin
	x1 := max(x, bounds.Min.X)
	y1 := max(y, bounds.Min.Y)
	x2 := min(right, bounds.Max.X)
	y2 := min(bottom, bounds.Max.Y)
	return uv.Rect(x1, y1, max(x2-x1, 0), max(y2-y1, 0))
}

// CenteredRect centers a rectangle whose width is a percentage of area and
// whose height is an absolute row count, keeping the ModalScreenMargin
// floor.
func CenteredRect(widthPercent, height int, area uv.Rectangle) uv.Rectangle {
	return place(percentOf(area.Dx(), widthPercent), height, area)
}

// CenteredRectPercent centers a rectangle sized as a percentage of area in
// both directions, keeping the ModalScreenMargin floor. Use when the content
// has no natural height and should scale with the terminal.
func CenteredRectPercent(widthPercent, heightPercent int, area uv.Rectangle) uv.Rectangle {
	return place(percentOf(area.Dx(), widthPercent), percentOf(area.Dy(), heightPercent), area)
}

// CenteredRectFixed centers a rectangle of an absolute cell size, keeping the
// ModalScreenMargin floor. Use when the content has a natural size — a QR
// code, a fixed table — that should hug its content instead of scaling.
func CenteredRectFixed(width, height int, area uv.Rectangle) uv.Rectangle {
	return place(width, height, area)
}

// centeredModalRect centers a percentage-width modal and clears its exterior
// halo.
func centeredModalRect(frame *Frame, widthPercent, height int, area uv.Rectangle) uv.Rectangle {
	popup := CenteredRect(widthPercent, height, area)
	clearModal(frame, popup, area)
	return popup
}

// centeredModalRectFixed centers a fixed-size modal and clears its exterior
// halo.
func centeredModalRectFixed(frame *Frame, width, height int, area uv.Rectangle) uv.Rectangle {
	popup := CenteredRectFixed(width, height, area)
	clearModal(frame, popup, area)
	return popup
}

// place centers a modal of the requested size in area, shrinking it only as
// far as the margin requires.
//
// The size is asked for against the whole area, not against the inset one,
// so the margin is a floor rather than a second inset. A dialog that already
// leaves more than ModalScreenMargin free — most percentage-sized ones do —
// keeps the size its caller chose.
func place(width, height int, area uv.Rectangle) uv.Rectangle {
	bounds := ModalArea(area)
	width = min(width, bounds.Dx())
	height = min(height, bounds.Dy())
	return uv.Rect(
		bounds.Min.X+max(bounds.Dx()-width, 0)/2,
		bounds.Min.Y+max(bounds.Dy()-height, 0)/2,
		width,
		height,
	)
}

// percentOf is percent of cells, clamped to the u16 range Rust's Rect holds.
func percentOf(cells, percent int) int {
	return min(cells*percent/100, 0xFFFF)
}

// CenteredModal centers and clears a modal popup, then registers its body as
// selectable.
//
// Modals draw over the view beneath them, and the registry is z-ordered by
// render order, so registering here makes the body win the cells it covers.
func CenteredModal(frame *Frame, surfaces *FrameSurfaces, widthPercent, height int, area uv.Rectangle) uv.Rectangle {
	popup := centeredModalRect(frame, widthPercent, height, area)
	surfaces.Push(SurfaceFrameFixed(SurfaceIdModalBody{}, BorderedContent(popup)))
	return popup
}

// CenteredModalFixed centers and clears a modal of an absolute cell width,
// then registers its body as selectable. Use when the content has a natural
// width — a QR code, a fixed table — that should hug its content instead of
// scaling with the terminal. width and height include the border and are
// clamped to area.
func CenteredModalFixed(frame *Frame, surfaces *FrameSurfaces, width, height int, area uv.Rectangle) uv.Rectangle {
	popup := centeredModalRectFixed(frame, width, height, area)
	surfaces.Push(SurfaceFrameFixed(SurfaceIdModalBody{}, BorderedContent(popup)))
	return popup
}
