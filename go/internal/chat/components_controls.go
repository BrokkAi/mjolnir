// Port of mj-chat/src/components/controls.rs.

package chat

import (
	"strings"

	uv "github.com/charmbracelet/ultraviolet"
)

// Small terminal controls backed by [Form].

func focusStyle() Style {
	return FocusControl()
}

func normalStyle() Style {
	return ActionableChip()
}

func disabledStyle() Style {
	return Style{}.Foreground(ThemePalette().Muted).Background(ThemePalette().SurfaceRaised)
}

func controlStyle[K comparable](form *Form[K], id K, enabled bool) Style {
	switch {
	case !enabled:
		return disabledStyle()
	case form.IsFocused(id) || form.IsArmed(id):
		return focusStyle()
	case form.IsDefaultAction(id):
		style := normalStyle().Foreground(ThemePalette().Accent).Add(ModifierBold)
		// Bold reverse video is reserved for actual focus without painted
		// surfaces.
		if ReverseVideo() {
			return style.Remove(ModifierReversed)
		}
		return style
	default:
		return normalStyle()
	}
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
	form.Register(id, ControlKindButton{}, area, enabled)
	paragraph := NewParagraph(TextFrom(LineFrom(RawSpan("  " + label + "  ")))).
		WithStyle(controlStyle(form, id, enabled)).
		WithAlignment(alignment)
	frame.RenderWidget(paragraph, area)
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
	return min(displayWidth(label)+4, 0xFFFF)
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
	if len(buttons) == 0 {
		return
	}
	if area.Dx() == 0 || area.Dy() == 0 {
		for _, button := range buttons {
			form.Register(button.ID, ControlKindButton{}, uv.Rectangle{}, button.Enabled)
		}
		return
	}
	for _, button := range buttons {
		form.DeclareWithEnabled(button.ID, ControlKindButton{}, button.Enabled)
	}
	widths := make([]int, len(buttons))
	total := 0
	for i, button := range buttons {
		widths[i] = buttonWidth(button.Label)
		total += widths[i]
	}
	// Only a row that fits can be pushed to the right edge; when it
	// overflows the offset is zero and the scroll below behaves as before.
	total += len(widths) - 1
	offset := 0
	if align == RowAlignRight {
		offset = max(area.Dx()-total, 0)
	}
	start := offset
	scroll := 0
	for i, button := range buttons {
		if form.IsFocused(button.ID) {
			scroll = min(max(start+widths[i]-area.Dx(), 0), start)
		}
		start += widths[i] + 1
	}
	start = offset
	for i, button := range buttons {
		end := start + widths[i]
		visibleStart := max(start, scroll)
		visibleEnd := min(end, scroll+area.Dx())
		rect := uv.Rectangle{}
		if visibleStart < visibleEnd {
			rect = uv.Rect(area.Min.X+(visibleStart-scroll), area.Min.Y, visibleEnd-visibleStart, area.Dy())
		}
		ButtonRender(frame, rect, button.Label, button.Enabled, form, button.ID)
		start = end + 1
	}
}

// ButtonRowRenderInert draws the row as inert text, registering no
// hitboxes.
//
// A page that stays visible behind a popup draws this mirror so its
// controls keep their places, while a click on the overlay covering them
// cannot reach a control behind it.
func ButtonRowRenderInert(frame *Frame, area uv.Rectangle, labels []string) {
	if area.Dx() == 0 || area.Dy() == 0 {
		return
	}
	start := 0
	for _, label := range labels {
		if start >= area.Dx() {
			return
		}
		width := buttonWidth(label)
		visible := min(width, area.Dx()-start)
		frame.RenderWidget(
			NewParagraph(TextFrom(LineFrom(StyledSpan("  "+label+"  ", Muted())))),
			uv.Rect(area.Min.X+start, area.Min.Y, visible, 1),
		)
		start += width + 1
	}
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
	if len(buttons) == 0 {
		return
	}
	if area.Dx() == 0 || area.Dy() == 0 {
		for _, button := range buttons {
			form.Register(button.ID, ControlKindButton{}, uv.Rectangle{}, button.Enabled)
		}
		return
	}
	for _, button := range buttons {
		form.DeclareWithEnabled(button.ID, ControlKindButton{}, button.Enabled)
	}
	width := min(ButtonColumnWidth(buttons), area.Dx())
	x := area.Min.X
	if align == ColumnAlignRight {
		x = area.Min.X + max(area.Dx()-width, 0)
	}
	scroll := 0
	for row, button := range buttons {
		if form.IsFocused(button.ID) {
			scroll = min(max(row+1-area.Dy(), 0), row)
		}
	}
	for row, button := range buttons {
		rect := uv.Rectangle{}
		if row >= scroll && row-scroll < area.Dy() {
			rect = uv.Rect(x, area.Min.Y+(row-scroll), width, 1)
		}
		// Every row shares one width, so labels line up on the left the way
		// the rows of a table align.
		ButtonRenderAligned(frame, rect, button.Label, button.Enabled, form, button.ID, AlignmentLeft)
	}
}

// ButtonColumnRenderInert draws the stack as inert text, registering no
// hitboxes.
//
// A page that stays visible behind a popup draws this mirror so its
// controls keep their places, while a click on the overlay covering them
// cannot reach a control behind it.
func ButtonColumnRenderInert(frame *Frame, area uv.Rectangle, labels []string, align ColumnAlign) {
	if len(labels) == 0 || area.Dx() == 0 || area.Dy() == 0 {
		return
	}
	width := 0
	for _, label := range labels {
		width = max(width, buttonWidth(label))
	}
	width = min(width, area.Dx())
	x := area.Min.X
	if align == ColumnAlignRight {
		x = area.Min.X + max(area.Dx()-width, 0)
	}
	for row, label := range labels {
		if row >= area.Dy() {
			return
		}
		frame.RenderWidget(
			NewParagraph(TextFrom(LineFrom(StyledSpan("  "+label+"  ", Muted())))),
			uv.Rect(x, area.Min.Y+row, width, 1),
		)
	}
}

// TextField is a readline text field using Hel's existing [TextInput]
// editor.
type TextField struct{}

// TextFieldRenderUnderlined draws an editor whose whole field remains
// underlined before focus.
func TextFieldRenderUnderlined[K comparable](frame *Frame, area uv.Rectangle, input *TextInput, form *Form[K], id K) {
	TextFieldRender(frame, area, input, form, id)
	BufferSetStyle(frame.Buffer(), area, Style{}.Add(ModifierUnderlined))
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
	value := input.Value()
	rows, cursorColumn, cursorRow := multilineRows(value, input.Cursor(), area.Dx())
	scroll := max(cursorRow+1-area.Dy(), 0)
	var cursorMap []multilineCursorStop
	for row := scroll; row < len(rows) && row < scroll+area.Dy(); row++ {
		visual := rows[row]
		screenRow := area.Min.Y + (row - scroll)
		column := 0
		cursorMap = append(cursorMap, multilineCursorStop{x: area.Min.X, y: screenRow, offset: visual.start})
		for _, grapheme := range visual.graphemes {
			column += grapheme.width
			cursorMap = append(cursorMap, multilineCursorStop{x: area.Min.X + column, y: screenRow, offset: grapheme.end})
		}
	}
	if cursorRow >= len(rows) && cursorRow >= scroll && cursorRow < scroll+area.Dy() {
		cursorMap = append(cursorMap, multilineCursorStop{x: area.Min.X, y: area.Min.Y + (cursorRow - scroll), offset: len(value)})
	}
	form.registerWithMultilineCursorMap(id, ControlKindTextField{}, area, enabled, cursorMap)
	form.SetFieldUsesVerticalKeys(id, input.UsesVerticalKeys())
	style := Field(false).Foreground(ThemePalette().Muted)
	if enabled {
		style = Field(focused && form.IsFocused(id))
	}
	frame.RenderWidget(
		NewParagraph(RawText(value)).
			WithStyle(style).
			WithWrap(Wrap{Trim: false}).
			WithScroll(min(scroll, 0xFFFF), 0),
		area,
	)
	if enabled && focused && form.IsFocused(id) && area.Dx() > 0 && area.Dy() > 0 {
		row := max(cursorRow-scroll, 0)
		if row < area.Dy() {
			frame.SetCursorPosition(uv.Pos(
				area.Min.X+min(cursorColumn, max(area.Dx()-1, 0)),
				area.Min.Y+row,
			))
		}
	}
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
	graphemeWidth := func(text string) int {
		if secret {
			return 1
		}
		return displayWidth(text)
	}
	content := area
	width := content.Dx()
	value := input.Value()
	cursorWidth := 0
	for _, grapheme := range componentGraphemes(value[:input.Cursor()]) {
		cursorWidth += graphemeWidth(grapheme)
	}
	scroll := max(cursorWidth-max(width-1, 0), 0)
	startByte, consumed := 0, 0
	for _, grapheme := range componentGraphemes(value) {
		if consumed >= scroll {
			break
		}
		consumed += graphemeWidth(grapheme)
		startByte += len(grapheme)
	}
	scroll = consumed

	var visible strings.Builder
	cursorMap := []cursorStop{{x: content.Min.X, offset: startByte}}
	displayed := 0
	offset := startByte
	for _, grapheme := range componentGraphemes(value[startByte:]) {
		cells := graphemeWidth(grapheme)
		if displayed+cells > width {
			break
		}
		if secret {
			visible.WriteString(ThemeGlyphs().Bullet)
		} else {
			visible.WriteString(grapheme)
		}
		displayed += cells
		offset += len(grapheme)
		cursorMap = append(cursorMap, cursorStop{x: content.Min.X + min(displayed, 0xFFFF), offset: offset})
	}
	if inline {
		form.registerInlineEditor(id, area, cursorMap)
	} else {
		form.registerWithCursorMap(id, kind, area, true, cursorMap)
	}
	form.SetFieldUsesVerticalKeys(id, input.UsesVerticalKeys())
	style := Field(focused && form.IsFocused(id))
	frame.RenderWidget(NewParagraph(RawText(visible.String())).WithStyle(style), area)
	if focused && form.IsFocused(id) && content.Dx() > 0 && content.Dy() > 0 {
		cursor := max(cursorWidth-scroll, 0)
		frame.SetCursorPosition(uv.Pos(content.Min.X+min(cursor, max(width-1, 0)), content.Min.Y))
	}
}

// TextFieldCaretWindow cuts a single-line field down to width cells around
// cursor for a host that draws its own caret. It returns the text before
// the caret, the caret cell (the grapheme under the cursor, or a space at
// the end), and the text after it. One cell is always kept for the caret,
// so the field scrolls horizontally to keep the cursor in view.
func TextFieldCaretWindow(value string, cursor int, width int) (before string, caret string, after string) {
	cursor = min(cursor, len(value))
	cursorWidth := displayWidth(value[:cursor])
	scroll := max(cursorWidth-max(width-1, 0), 0)
	start, consumed := 0, 0
	for _, grapheme := range componentGraphemes(value[:cursor]) {
		if consumed >= scroll {
			break
		}
		consumed += displayWidth(grapheme)
		start += len(grapheme)
	}
	before = value[start:cursor]
	beforeWidth := displayWidth(before)
	rest := componentGraphemes(value[cursor:])
	caret = " "
	if len(rest) > 0 && beforeWidth+displayWidth(rest[0]) <= width {
		caret = rest[0]
	}
	var afterText strings.Builder
	if len(rest) > 0 && rest[0] == caret {
		used := beforeWidth + displayWidth(caret)
		for _, grapheme := range rest[1:] {
			cells := displayWidth(grapheme)
			if used+cells > width {
				break
			}
			used += cells
			afterText.WriteString(grapheme)
		}
	}
	return before, caret, afterText.String()
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
	glyphs := ThemeGlyphs()
	if checked {
		return glyphs.CheckboxChecked
	}
	return glyphs.CheckboxUnchecked
}

// CheckboxRender draws and registers a checkbox.
func CheckboxRender[K comparable](frame *Frame, area uv.Rectangle, label string, checked bool, enabled bool, form *Form[K], id K) {
	form.Register(id, ControlKindCheckbox{}, area, enabled)
	mark := CheckboxMarker(checked)
	frame.RenderWidget(
		NewParagraph(RawText(mark+" "+label)).WithStyle(controlStyle(form, id, enabled)),
		area,
	)
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
	s.close()
	return true
}

// close forgets the open popup and its pending cursor.
func (s *ComboBoxState[K]) close() {
	var none K
	s.openID, s.hasOpen = none, false
	s.pending, s.hasPending = 0, false
}

// Route routes interactions produced by a combobox form control.
//
// Preview selections are consumed locally. Accepted selections are returned
// to the screen as [InteractionComboBoxCommit]. Interactions for other
// controls pass through unchanged, so callers can feed a form's action
// stream through this helper without special casing every screen. A nil
// interaction is no interaction.
func (s *ComboBoxState[K]) Route(interaction Interaction[K]) Interaction[K] {
	if interaction == nil {
		return nil
	}
	if !s.hasOpen {
		return interaction
	}
	open := s.openID
	switch routed := interaction.(type) {
	case InteractionSelect[K]:
		if routed.ID == open {
			s.Preview(routed.ID, routed.Index)
			return nil
		}
	case InteractionComboBoxCommit[K]:
		if routed.ID == open {
			s.close()
			return routed
		}
	case InteractionComboBoxDismiss[K]:
		if routed.ID == open {
			s.Dismiss(routed.ID)
			return routed
		}
	}
	s.close()
	return interaction
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
	return ThemeGlyphs().Dropdown
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
	selected = min(selected, max(len(options)-1, 0))
	kind := ControlKindComboBox{Len: len(options), Selected: selected, Expanded: expanded}
	form.registerCombobox(id, kind, area, enabled, uv.Rectangle{}, nil)
	display := clippedDisplay(value, area.Dx())
	frame.RenderWidget(NewParagraph(RawText(display)).WithStyle(controlStyle(form, id, enabled)), area)

	if !expanded {
		return uv.Rectangle{}, uv.Rectangle{}, false
	}
	optionWidth := 0
	for _, option := range options {
		optionWidth = max(optionWidth, option.Width())
	}
	width := max(optionWidth+4, displayWidth(title)+2)
	outer, inner, ok = AutocompletePopupRender(
		frame,
		bounds,
		area,
		min(width, 0xFFFF),
		len(options),
		maxVisibleRows,
		title,
		preferredSide,
	)
	if !ok {
		return uv.Rectangle{}, uv.Rectangle{}, false
	}
	items := make([]ListItem, 0, len(options))
	for _, option := range options {
		items = append(items, NewListItem(TextFrom(option)))
	}
	var selectedItem *int
	if len(options) > 0 {
		selectedItem = &selected
	}
	state := ListState{}.
		WithOffset(CenteredUnitOffset(selected, len(options), inner.Dy())).
		WithSelected(selectedItem)
	overflow := len(options) > inner.Dy() && inner.Dx() > 1
	// Keep the last column for the scrollbar so it never covers row text.
	listArea := inner
	if overflow {
		listArea = uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx()-1, inner.Dy())
	}
	RenderStatefulWidget(frame, NewList(items).WithHighlightStyle(Selection(form.IsFocused(id))), listArea, &state)
	offset := state.Offset()
	if overflow {
		track := uv.Rect(inner.Max.X-1, inner.Min.Y, 1, inner.Dy())
		if geometry, ok := NewScrollbarGeometry(track, len(options), offset, inner.Dy()); ok {
			RenderScrollbar(frame, geometry)
		}
	}
	form.registerCombobox(id, kind, area, enabled, outer, popupRowMap(outer, inner, offset, len(options)))
	return outer, inner, true
}

// popupRowMap maps each row of a popup's frame to the option shown there:
// rows inside the border show options from offset on, and the border rows
// and rows past the last option show none.
func popupRowMap(outer, inner uv.Rectangle, offset, options int) []*int {
	rows := make([]*int, outer.Dy())
	for row := 1; row < len(rows) && row-1 < inner.Dy(); row++ {
		if index := offset + row - 1; index < options {
			rows[row] = &index
		}
	}
	return rows
}

func clippedDisplay(value string, width int) string {
	glyph := ComboBoxCurrentGlyph()
	suffix := " " + glyph
	if width == 0 {
		return ""
	}
	suffixWidth := displayWidth(suffix)
	if width < suffixWidth {
		return glyph
	}
	if width == suffixWidth {
		var result strings.Builder
		used := 0
		for _, grapheme := range componentGraphemes(suffix) {
			next := used + displayWidth(grapheme)
			if next > width {
				break
			}
			used = next
			result.WriteString(grapheme)
		}
		return result.String()
	}
	available := width - suffixWidth
	var result strings.Builder
	used := 0
	for _, grapheme := range componentGraphemes(value) {
		next := used + displayWidth(grapheme)
		if next > available {
			break
		}
		result.WriteString(grapheme)
		used = next
	}
	result.WriteString(suffix)
	return result.String()
}

// ChoiceList is a vertically navigable list.
type ChoiceList struct{}

// ChoiceListRender draws and registers a list. The selected row remains
// metadata in the form until the screen applies the emitted [Interaction].
func ChoiceListRender[K comparable](frame *Frame, area uv.Rectangle, rows []Line, selected int, form *Form[K], id K) {
	ChoiceListRenderWithRows(frame, area, rows, selected, nil, nil, form, id)
}

// lineContents is each line's text, the list contents a form compares to
// tell lists apart.
func lineContents(rows []Line) []string {
	contents := make([]string, 0, len(rows))
	for _, row := range rows {
		contents = append(contents, row.String())
	}
	return contents
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
	var mapped []*int
	for index, line := range rows {
		height := NewParagraph(TextFrom(line)).WithWrap(Wrap{Trim: false}).LineCount(area.Dx())
		var item *int
		if index < len(rowMap) {
			item = rowMap[index]
		}
		for range height {
			mapped = append(mapped, item)
		}
	}
	form.registerWithRows(
		id,
		ControlKindChoiceList{Len: len(mapped), Selected: selected},
		area,
		true,
		mapped,
		nil,
	)
	form.SetListContents(id, lineContents(rows))
	form.setListOffset(id, scroll)
	frame.RenderWidget(
		NewParagraph(TextFrom(rows...)).WithWrap(Wrap{Trim: false}).WithScroll(scroll, 0),
		area,
	)
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
	mapped := rowMap
	if len(rowMap) != len(rows) {
		mapped = make([]*int, len(rows))
		for index := range rows {
			item := index
			mapped[index] = &item
		}
	}
	enabled := rowEnabled
	if len(rowEnabled) != len(rows) {
		enabled = make([]bool, len(rows))
		for index := range enabled {
			enabled[index] = true
		}
	}
	// `len` counts items, not display rows: the host declares the list
	// between frames with its item count, and a length that disagreed would
	// read as a different list and drop the registered geometry.
	items := 0
	for _, item := range mapped {
		if item != nil {
			items++
		}
	}
	form.registerWithRows(
		id,
		ControlKindChoiceList{Len: items, Selected: selected},
		area,
		true,
		append([]*int(nil), mapped...),
		append([]bool(nil), enabled...),
	)
	form.SetListContents(id, lineContents(rows))
	listItems := make([]ListItem, 0, len(rows))
	for index, row := range rows {
		item := NewListItem(TextFrom(row))
		if !enabled[index] {
			item = item.WithStyle(disabledStyle())
		}
		listItems = append(listItems, item)
	}
	var selectedRow *int
	for index, option := range mapped {
		if option != nil && *option == selected {
			row := index
			selectedRow = &row
			break
		}
	}
	stateOffset := 0
	if selectedRow != nil {
		stateOffset = CenteredUnitOffset(*selectedRow, len(rows), area.Dy())
	}
	state := ListState{}.WithOffset(stateOffset).WithSelected(selectedRow)
	highlight := Selection(form.IsFocused(id))
	if selectedRow != nil && !enabled[*selectedRow] {
		highlight = disabledStyle()
	}
	RenderStatefulWidget(frame, NewList(listItems).WithHighlightStyle(highlight), area, &state)
	form.setListOffset(id, state.Offset())
}

// TabStrip is a horizontally navigable tab strip.
type TabStrip struct{}

// TabStripRender draws and registers tabs.
func TabStripRender[K comparable](frame *Frame, area uv.Rectangle, tabs []string, selected int, form *Form[K], id K) {
	TabStripRenderEnabled(frame, area, tabs, selected, true, form, id)
}

// TabStripRenderEnabled draws a tab strip with explicit enabled state.
func TabStripRenderEnabled[K comparable](frame *Frame, area uv.Rectangle, tabs []string, selected int, enabled bool, form *Form[K], id K) {
	form.DeclareWithEnabled(id, ControlKindTabs{Len: len(tabs), Selected: selected}, enabled)
	starts := make([]int, 0, len(tabs))
	x := 0
	for _, tab := range tabs {
		starts = append(starts, x)
		x += displayWidth(tab) + 1
	}
	selected = min(selected, max(len(tabs)-1, 0))
	selectedStart, selectedEnd := 0, 0
	if selected < len(starts) {
		selectedStart = starts[selected]
		selectedEnd = selectedStart + displayWidth(tabs[selected])
	}
	available := area.Dx()
	scroll := min(max(selectedEnd-available, 0), selectedStart)

	var regions []regionSpan
	// The strip owns its row: blank it, so text drawn there before (the
	// turn review's role strip spaces its labels more widely) cannot show
	// through between or after the tabs (R4-13, "General done Verdictt").
	// Spaces keep the row's existing style, unlike `Clear`.
	blank := RawLine(strings.Repeat(" ", area.Dx()))
	blanks := make([]Line, area.Dy())
	for row := range blanks {
		blanks[row] = blank
	}
	frame.RenderWidget(NewParagraph(TextFrom(blanks...)), area)
	for index, tab := range tabs {
		tabStart := starts[index]
		tabEnd := tabStart + displayWidth(tab)
		visibleStart := max(tabStart, scroll)
		visibleEnd := min(tabEnd, scroll+available)
		if visibleStart >= visibleEnd {
			continue
		}
		var style Style
		switch {
		case !enabled:
			style = disabledStyle()
		case index == selected:
			// The active tab is always highlighted; underline marks keyboard
			// focus.
			style = focusStyle()
			if form.IsFocused(id) {
				style = style.Add(ModifierUnderlined)
			}
		default:
			style = normalStyle()
		}
		skip := visibleStart - tabStart
		end := visibleEnd - tabStart
		var rendered strings.Builder
		width := 0
		for _, grapheme := range componentGraphemes(tab) {
			next := width + displayWidth(grapheme)
			clipped := max(min(next, end)-max(width, skip), 0)
			if clipped > 0 {
				if width >= skip && next <= end {
					rendered.WriteString(grapheme)
				} else {
					rendered.WriteString(strings.Repeat(" ", clipped))
				}
			}
			width = next
		}
		tabX := area.Min.X + (visibleStart - scroll)
		tabWidth := visibleEnd - visibleStart
		frame.RenderWidget(NewParagraph(RawText(rendered.String())).WithStyle(style), uv.Rect(tabX, area.Min.Y, tabWidth, area.Dy()))
		regions = append(regions, regionSpan{start: tabX, end: tabX + tabWidth, index: index})
	}
	form.registerWithRegions(id, ControlKindTabs{Len: len(tabs), Selected: selected}, area, enabled, regions)
}
