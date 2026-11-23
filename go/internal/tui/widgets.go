// Port of mj-tui/src/widgets.rs.

// Small drawing primitives shared by the dashboard, dialogs, and wizards.
//
// Modal geometry is shared with the chat view, so it lives in package chat.
// Rust re-exports those helpers here so `crate::widgets` stays the single
// import site for dashboard code; Go has no re-exports, so these are thin
// wrappers (and a type alias) over chat's functions, and calling chat's
// directly is equally correct.

package tui

import (
	"math"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Truncate is chat's Truncate (`mj_chat::components::Truncate`).
type Truncate = chat.Truncate

// TruncateSummary and TruncatePlain are Rust's `Truncate::SUMMARY` and
// `Truncate::PLAIN`, re-exported with the type.
var (
	TruncateSummary = chat.TruncateSummary
	TruncatePlain   = chat.TruncatePlain
)

// truncateToCells is chat's TruncateToCells.
func truncateToCells(text string, width int, options Truncate) string {
	return chat.TruncateToCells(text, width, options)
}

// borderedContent is chat's BorderedContent.
func borderedContent(area uv.Rectangle) uv.Rectangle {
	return chat.BorderedContent(area)
}

// centeredModal is chat's CenteredModal.
func centeredModal(frame *chat.Frame, surfaces *chat.FrameSurfaces, widthPercent, height int, area uv.Rectangle) uv.Rectangle {
	return chat.CenteredModal(frame, surfaces, widthPercent, height, area)
}

// centeredModalFixed is chat's CenteredModalFixed.
func centeredModalFixed(frame *chat.Frame, surfaces *chat.FrameSurfaces, width, height int, area uv.Rectangle) uv.Rectangle {
	return chat.CenteredModalFixed(frame, surfaces, width, height, area)
}

// centeredRect is chat's CenteredRect.
func centeredRect(widthPercent, height int, area uv.Rectangle) uv.Rectangle {
	return chat.CenteredRect(widthPercent, height, area)
}

// dismissibleModalTitle is chat's DismissibleModalTitle.
func dismissibleModalTitle[K comparable](form *chat.Form[K], popup uv.Rectangle, title string, titleStyle chat.Style, enabled bool) chat.Line {
	return chat.DismissibleModalTitle(form, popup, title, titleStyle, enabled)
}

// modalArea is chat's ModalArea.
func modalArea(area uv.Rectangle) uv.Rectangle {
	return chat.ModalArea(area)
}

// popupHeight is the popup height that keeps every wrapped line of
// paragraph visible, never shrinking below the dialog's nominal height.
func popupHeight(paragraph *chat.Paragraph, widthPercent, nominal int, area uv.Rectangle) int {
	innerWidth := max(centeredRect(widthPercent, 1, area).Dx()-2, 0)
	// Rust's height is a u16 and saturates there.
	wrapped := min(paragraph.LineCount(innerWidth), math.MaxUint16)
	return max(nominal, wrapped)
}

// formatResourceBytes is core's FormatResourceBytes.
func formatResourceBytes(bytes uint64) string {
	return core.FormatResourceBytes(bytes)
}

// widgetsCountedNumber is the integer types counted accepts, the same set
// as core's Counted.
type widgetsCountedNumber interface {
	~int | ~int8 | ~int16 | ~int32 | ~int64 |
		~uint | ~uint8 | ~uint16 | ~uint32 | ~uint64 | ~uintptr
}

// counted is count and the noun that agrees with it. It lives in core so the
// daemon's and the CLI's messages use it too (launch finding R5-10).
func counted[T widgetsCountedNumber](count T, singular, plural string) string {
	return core.Counted(count, singular, plural)
}
