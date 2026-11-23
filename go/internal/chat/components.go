// Port of mj-chat/src/components/mod.rs.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// Reusable controls and focus-aware form routing for Hel TUIs.
//
// Rust's `components` module re-exports its submodules' items; in Go they
// all live in package chat, in the components_*.go files.

// PathField is a path field with the standard readline editing and cursor
// behavior, plus an anchored popup of filesystem completions.
type PathField struct{}

// pathFieldVisibleRows is the most rows the popup is tall, matching the
// shared popup shell.
const pathFieldVisibleRows = 8

// PathFieldPopupRows is the rows the popup needs beneath the field, its
// border included, or zero when nothing is open. A dialog reserves these so
// a popup never lands on the button row and never grows past its frame.
func PathFieldPopupRows(input *PathInput) int {
	var rows int
	if input.IsCompleting() {
		rows = min(len(input.Completions()), pathFieldVisibleRows)
	} else if _, pending := input.CompletionPending(); pending {
		rows = 1
	} else {
		return 0
	}
	return min(rows, 0xFFFF) + 2
}

// PathFieldRender draws the field with its popup confined to the frame.
func PathFieldRender[K comparable](frame *Frame, area uv.Rectangle, input *PathInput, form *Form[K], id K) {
	PathFieldRenderWithin(frame, frame.Area(), area, input, form, id)
}

// PathFieldRenderWithin draws the field with its popup confined to bounds.
//
// The popup belongs to the surface that owns the field, so that surface
// says which rows it can spare: pass the region above a dialog's button row
// and the popup opens upward, or shrinks, instead of painting over the
// buttons, the frame, or the screen behind the dialog.
func PathFieldRenderWithin[K comparable](frame *Frame, bounds uv.Rectangle, area uv.Rectangle, input *PathInput, form *Form[K], id K) {
	textFieldRenderWithKind(frame, area, &input.TextInput, input.ControlKind(), form, id)
	if !form.IsFocused(id) {
		return
	}
	if input.IsCompleting() {
		candidates := input.Completions()
		title := " matches "
		if input.CompletionTruncated() {
			title = " first 50 matches · keep typing "
		}
		longest := 0
		for _, candidate := range candidates {
			longest = max(longest, displayWidth(candidate))
		}
		width := max(longest+4, area.Dx(), displayWidth(title)+2)
		outer, inner, ok := AutocompletePopupRender(
			frame,
			bounds,
			area,
			min(width, 0xFFFF),
			len(candidates),
			pathFieldVisibleRows,
			title,
			PopupSideBelow,
		)
		if !ok {
			return
		}
		items := make([]ListItem, 0, len(candidates))
		for _, candidate := range candidates {
			items = append(items, NewListItem(RawText(candidate)))
		}
		selected := input.CompletionSelected()
		state := ListState{}.
			WithOffset(CenteredUnitOffset(selected, len(candidates), inner.Dy())).
			WithSelected(&selected)
		RenderStatefulWidget(frame, NewList(items).WithHighlightStyle(Selection(true)), inner, &state)
		form.registerPopup(id, outer, popupRowMap(outer, inner, state.Offset(), len(candidates)))
	} else if _, pending := input.CompletionPending(); pending {
		const waiting = "Completing…"
		width := max(displayWidth(waiting)+4, area.Dx())
		if _, inner, ok := AutocompletePopupRender(
			frame,
			bounds,
			area,
			min(width, 0xFFFF),
			1,
			pathFieldVisibleRows,
			" completing ",
			PopupSideBelow,
		); ok {
			frame.RenderWidget(NewParagraph(RawText(waiting)), inner)
		}
	}
}

// PathFieldApply applies a form edit to the path input, dismissing the
// completion popup when the text changed.
func PathFieldApply(input *PathInput, edit FieldEdit) EditOutcome {
	outcome := TextFieldApply(&input.TextInput, edit)
	if outcome.Changed() {
		input.DismissCompletion()
	}
	return outcome
}
