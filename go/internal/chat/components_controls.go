// Port of mj-chat/src/components/controls.rs.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// Small terminal controls backed by [Form].

func focusStyle() Style {
	panic("unported: mj-chat/src/components/controls.rs focusStyle")
}

func normalStyle() Style {
	panic("unported: mj-chat/src/components/controls.rs normalStyle")
}

func disabledStyle() Style {
	panic("unported: mj-chat/src/components/controls.rs disabledStyle")
}

func controlStyle[K comparable](form *Form[K], id K, enabled bool) Style {
	panic("unported: mj-chat/src/components/controls.rs controlStyle")
}

// ButtonSpec is one button of a row or column: its id, label, and whether it
// is enabled (Rust's `(K, &str, bool)` tuples).
type ButtonSpec[K comparable] struct {
	ID      K
	Label   string
	Enabled bool
}

// Button is a push button.
type Button struct{}

// ButtonRender draws and registers one button.
func ButtonRender[K comparable](frame *Frame, area uv.Rectangle, label string, enabled bool, form *Form[K], id K) {
	ButtonRenderAligned(frame, area, label, enabled, form, id, AlignmentCenter)
}

// ButtonRenderAligned draws and registers one button with an explicit label
// alignment.
func ButtonRenderAligned[K comparable](frame *Frame, area uv.Rectangle, label string, enabled bool, form *Form[K], id K, alignment Alignment) {
	panic("unported: mj-chat/src/components/controls.rs ButtonRenderAligned")
}

// RowAlign is which edge of its area a [ButtonRow] packs against when it
// fits.
//
// A row wider than its area ignores this and stays left-anchored so the
// focus-following scroll keeps working.
type RowAlign int

const (
	RowAlignLeft RowAlign = iota
	RowAlignRight
)

func buttonWidth(label string) int {
	panic("unported: mj-chat/src/components/controls.rs buttonWidth")
}

// ButtonRow is a row of equally aligned buttons with content-sized hitboxes.
type ButtonRow struct{}

// ButtonRowRender draws and registers buttons from left to right.
func ButtonRowRender[K comparable](frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], form *Form[K]) {
	ButtonRowRenderAligned(frame, area, buttons, form, RowAlignLeft)
}

// ButtonRowRenderAligned draws and registers buttons in order, packed
// against align.
func ButtonRowRenderAligned[K comparable](frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], form *Form[K], align RowAlign) {
	panic("unported: mj-chat/src/components/controls.rs ButtonRowRenderAligned")
}

// ButtonRowRenderInert draws the row as inert text, registering no
// hitboxes.
//
// A page that stays visible behind a popup draws this mirror so its
// controls keep their places, while a click on the overlay covering them
// cannot reach a control behind it.
func ButtonRowRenderInert(frame *Frame, area uv.Rectangle, labels []string) {
	panic("unported: mj-chat/src/components/controls.rs ButtonRowRenderInert")
}

// ColumnAlign is which edge of its area a [ButtonColumn] packs against when
// it fits.
type ColumnAlign int

const (
	ColumnAlignLeft ColumnAlign = iota
	ColumnAlignRight
)

// ButtonColumn is a vertical stack of buttons sharing one width, one button
// per row.
//
// The stack is the column counterpart of [ButtonRow]: callers set it beside
// the page body so the dialog's controls sit in a right-hand column. Every
// row is as wide as the longest label, so the buttons line up on both
// edges, and a stack taller than its area scrolls to keep the focused
// button visible.
type ButtonColumn struct{}

// ColumnSplit is the page body and the [ButtonColumn] set beside it.
//
// Page content renders into Body; Actions takes the column, which is packed
// against the edge of the area that was split.
type ColumnSplit struct {
	Body    uv.Rectangle
	Actions uv.Rectangle
}

// ButtonColumnBodyGap is the gap between a stacked action column and the
// body beside it.
const ButtonColumnBodyGap = 2

// ButtonColumnWidth is the width a column of buttons needs, including
// button padding.
func ButtonColumnWidth[K comparable](buttons []ButtonSpec[K]) int {
	width := 0
	for _, button := range buttons {
		width = max(width, buttonWidth(button.Label))
	}
	return width
}

// ButtonColumnSplit splits area into the page body and the action column
// beside it.
//
// The column is content-sized and packed against the right edge of area;
// the body keeps what is left, minus [ButtonColumnBodyGap].
func ButtonColumnSplit[K comparable](area uv.Rectangle, buttons []ButtonSpec[K]) ColumnSplit {
	width := min(ButtonColumnWidth(buttons), area.Dx())
	return ColumnSplit{
		Body: uv.Rect(
			area.Min.X,
			area.Min.Y,
			max(area.Dx()-(width+ButtonColumnBodyGap), 0),
			area.Dy(),
		),
		Actions: uv.Rect(area.Min.X+max(area.Dx()-width, 0), area.Min.Y, width, area.Dy()),
	}
}

// ButtonColumnRender draws and registers buttons from top to bottom, packed
// to the right.
func ButtonColumnRender[K comparable](frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], form *Form[K]) {
	ButtonColumnRenderAligned(frame, area, buttons, form, ColumnAlignRight)
}

// ButtonColumnRenderAligned draws and registers buttons in order, packed
// against align.
func ButtonColumnRenderAligned[K comparable](frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], form *Form[K], align ColumnAlign) {
	panic("unported: mj-chat/src/components/controls.rs ButtonColumnRenderAligned")
}

// ButtonColumnRenderInert draws the stack as inert text, registering no
// hitboxes.
//
// A page that stays visible behind a popup draws this mirror so its
// controls keep their places, while a click on the overlay covering them
// cannot reach a control behind it.
func ButtonColumnRenderInert(frame *Frame, area uv.Rectangle, labels []string, align ColumnAlign) {
	panic("unported: mj-chat/src/components/controls.rs ButtonColumnRenderInert")
}

// TextField is a readline text field using Hel's existing [TextInput]
// editor.
type TextField struct{}

// TextFieldRenderUnderlined draws an editor whose whole field remains
// underlined before focus.
func TextFieldRenderUnderlined[K comparable](frame *Frame, area uv.Rectangle, input *TextInput, form *Form[K], id K) {
	panic("unported: mj-chat/src/components/controls.rs TextFieldRenderUnderlined")
}

// TextFieldRender draws a horizontally scrolling field and registers its
// cursor map.
func TextFieldRender[K comparable](frame *Frame, area uv.Rectangle, input *TextInput, form *Form[K], id K) {
	textFieldRenderWithKind(frame, area, input, ControlKindTextField{}, form, id)
}

// textFieldRenderWithKind draws the same field but registers it as another
// kind of control, so a path field can carry its completion state without a
// second editor.
func textFieldRenderWithKind[K comparable](frame *Frame, area uv.Rectangle, input *TextInput, kind ControlKind, form *Form[K], id K) {
	textFieldRenderEditor(frame, area, input, false, false, true, kind, form, id)
}

// TextFieldRenderMultiline draws a vertically scrolling field and registers
// its two-dimensional cursor map. The map is built from the same
// grapheme-aware layout used by the composer, so mouse clicks follow
// wrapped Unicode text.
func TextFieldRenderMultiline[K comparable](frame *Frame, area uv.Rectangle, input *TextInput, enabled bool, focused bool, form *Form[K], id K) {
	panic("unported: mj-chat/src/components/controls.rs TextFieldRenderMultiline")
}

// TextFieldRenderInline draws an inline editor owned by an existing
// compound control.
func TextFieldRenderInline[K comparable](frame *Frame, area uv.Rectangle, input *TextInput, secret bool, focused bool, form *Form[K], id K) {
	textFieldRenderEditor(frame, area, input, secret, true, focused, ControlKindTextField{}, form, id)
}

func textFieldRenderEditor[K comparable](
	frame *Frame,
	area uv.Rectangle,
	input *TextInput,
	secret bool,
	inline bool,
	focused bool,
	kind ControlKind,
	form *Form[K],
	id K,
) {
	panic("unported: mj-chat/src/components/controls.rs textFieldRenderEditor")
}

// TextFieldCaretWindow cuts a single-line field down to width cells around
// cursor for a host that draws its own caret. It returns the text before
// the caret, the caret cell (the grapheme under the cursor, or a space at
// the end), and the text after it. One cell is always kept for the caret,
// so the field scrolls horizontally to keep the cursor in view.
func TextFieldCaretWindow(value string, cursor int, width int) (before string, caret string, after string) {
	panic("unported: mj-chat/src/components/controls.rs TextFieldCaretWindow")
}

// TextFieldApply applies an edit emitted by a form to a text input.
func TextFieldApply(input *TextInput, edit FieldEdit) EditOutcome {
	return ApplyFieldEdit(input, edit)
}

// Checkbox is a checkbox with a text label.
type Checkbox struct{}

// CheckboxMarker is the shared checked/unchecked symbol, respecting ASCII
// terminals.
func CheckboxMarker(checked bool) string {
	panic("unported: mj-chat/src/components/controls.rs CheckboxMarker")
}

// CheckboxRender draws and registers a checkbox.
func CheckboxRender[K comparable](frame *Frame, area uv.Rectangle, label string, checked bool, enabled bool, form *Form[K], id K) {
	panic("unported: mj-chat/src/components/controls.rs CheckboxRender")
}

// ComboBoxState is the state for one screen-local combobox interaction.
//
// The state deliberately keeps only one pending cursor. A screen can render
// several comboboxes, but opening a second one replaces the first so popup
// interactions cannot leak into another field. The zero value has no popup
// open.
type ComboBoxState[K comparable] struct {
	openID     K
	hasOpen    bool
	pending    int
	hasPending bool
}

// OpenID returns the currently expanded field, if any.
func (s *ComboBoxState[K]) OpenID() (K, bool) {
	return s.openID, s.hasOpen
}

// IsOpen reports whether id currently owns the popup.
func (s *ComboBoxState[K]) IsOpen(id K) bool {
	return s.hasOpen && s.openID == id
}

// Open opens id, taking a snapshot of its committed selection.
func (s *ComboBoxState[K]) Open(id K, selected int) {
	s.openID, s.hasOpen = id, true
	s.pending, s.hasPending = selected, true
}

// Selection returns the selection to render for a field.
func (s *ComboBoxState[K]) Selection(id K, committed int) int {
	if s.IsOpen(id) && s.hasPending {
		return s.pending
	}
	return committed
}

// Preview records an uncommitted popup cursor movement.
func (s *ComboBoxState[K]) Preview(id K, selected int) bool {
	if !s.IsOpen(id) {
		return false
	}
	changed := !s.hasPending || s.pending != selected
	s.pending, s.hasPending = selected, true
	return changed
}

// Accept accepts and closes the active popup, returning its pending
// selection.
func (s *ComboBoxState[K]) Accept(id K) (int, bool) {
	if !s.IsOpen(id) {
		return 0, false
	}
	selected, ok := s.pending, s.hasPending
	s.pending, s.hasPending = 0, false
	var none K
	s.openID, s.hasOpen = none, false
	return selected, ok
}

// Dismiss closes the active popup without changing its committed value.
func (s *ComboBoxState[K]) Dismiss(id K) bool {
	if !s.IsOpen(id) {
		return false
	}
	var none K
	s.openID, s.hasOpen = none, false
	s.pending, s.hasPending = 0, false
	return true
}

// Route routes interactions produced by a combobox form control.
//
// Preview selections are consumed locally. Accepted selections are returned
// to the screen as [InteractionComboBoxCommit]. Interactions for other
// controls pass through unchanged, so callers can feed a form's action
// stream through this helper without special casing every screen. A nil
// interaction is no interaction.
func (s *ComboBoxState[K]) Route(interaction Interaction[K]) Interaction[K] {
	panic("unported: mj-chat/src/components/controls.rs ComboBoxState.Route")
}

// ComboBox is a compact scalar field with an anchored list of choices.
type ComboBox struct{}

// ComboBoxGlyph is the compact affordance appended to a collapsed choice
// value.
const ComboBoxGlyph = "▾"

// ComboBoxDefaultMaxVisibleRows is how many options a dropdown shows at
// once, matching chat autocomplete's compact surface. A caller with less
// room to spare passes its own cap to [ComboBoxRender].
const ComboBoxDefaultMaxVisibleRows = 8

// ComboBoxCurrentGlyph is [ComboBoxGlyph] in the symbol set in force (Rust's
// `ComboBox::glyph()`).
func ComboBoxCurrentGlyph() string {
	panic("unported: mj-chat/src/components/controls.rs ComboBoxCurrentGlyph")
}

// ComboBoxDisplayValue returns a collapsed field value with the dropdown
// affordance.
func ComboBoxDisplayValue(value string) string {
	return value + " " + ComboBoxGlyph
}

// ComboBoxRender draws a combobox and, when expanded, its anchored popup.
// maxVisibleRows caps how many options the popup shows at once. It returns
// the popup's outer and inner regions when one was drawn.
func ComboBoxRender[K comparable](
	frame *Frame,
	bounds uv.Rectangle,
	area uv.Rectangle,
	value string,
	options []Line,
	selected int,
	expanded bool,
	enabled bool,
	title string,
	preferredSide PopupSide,
	maxVisibleRows int,
	form *Form[K],
	id K,
) (outer uv.Rectangle, inner uv.Rectangle, ok bool) {
	panic("unported: mj-chat/src/components/controls.rs ComboBoxRender")
}

func clippedDisplay(value string, width int) string {
	panic("unported: mj-chat/src/components/controls.rs clippedDisplay")
}

// ChoiceList is a vertically navigable list.
type ChoiceList struct{}

// ChoiceListRender draws and registers a list. The selected row remains
// metadata in the form until the screen applies the emitted [Interaction].
func ChoiceListRender[K comparable](frame *Frame, area uv.Rectangle, rows []Line, selected int, form *Form[K], id K) {
	ChoiceListRenderWithRows(frame, area, rows, selected, nil, nil, form, id)
}

// ChoiceListRenderWrapped draws wrapped descriptions and options with exact
// option hitboxes.
func ChoiceListRenderWrapped[K comparable](
	frame *Frame,
	area uv.Rectangle,
	rows []Line,
	rowMap []*int,
	selected int,
	scroll int,
	form *Form[K],
	id K,
) {
	panic("unported: mj-chat/src/components/controls.rs ChoiceListRenderWrapped")
}

// ChoiceListRenderWithRows draws a list with a display-to-option map and
// per-row enabled state.
//
// rowMap may contain nil for headings or separators. A missing entry in
// rowEnabled is treated as enabled.
func ChoiceListRenderWithRows[K comparable](
	frame *Frame,
	area uv.Rectangle,
	rows []Line,
	selected int,
	rowMap []*int,
	rowEnabled []bool,
	form *Form[K],
	id K,
) {
	panic("unported: mj-chat/src/components/controls.rs ChoiceListRenderWithRows")
}

// TabStrip is a horizontally navigable tab strip.
type TabStrip struct{}

// TabStripRender draws and registers tabs.
func TabStripRender[K comparable](frame *Frame, area uv.Rectangle, tabs []string, selected int, form *Form[K], id K) {
	TabStripRenderEnabled(frame, area, tabs, selected, true, form, id)
}

// TabStripRenderEnabled draws a tab strip with explicit enabled state.
func TabStripRenderEnabled[K comparable](frame *Frame, area uv.Rectangle, tabs []string, selected int, enabled bool, form *Form[K], id K) {
	panic("unported: mj-chat/src/components/controls.rs TabStripRenderEnabled")
}
