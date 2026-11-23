// Port of mj-chat/src/components/controls/tests.rs.

package chat

import (
	"fmt"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func click(form *Form[uint8], x, y int) Interaction[uint8] {
	form.Handle(formMouseDown(x, y))
	return form.Handle(formMouseUp(x, y)).Action
}

func columnLines(width, height int, focused uint8) []string {
	form := NewForm[uint8]()
	form.Declare(1, ControlKindButton{})
	form.Declare(2, ControlKindButton{})
	form.Declare(3, ControlKindButton{})
	form.EndFrame(focused)
	terminal := newComponentTerminal(width, height)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		ButtonColumnRender(frame, frame.Area(), []ButtonSpec[uint8]{
			{ID: 1, Label: "First", Enabled: true},
			{ID: 2, Label: "Widest", Enabled: true},
			{ID: 3, Label: "Last", Enabled: true},
		}, &form)
		form.EndFrame(focused)
	})
	return terminal.rows()
}

func TestAColumnTooShortForItsButtonsScrollsToTheFocusedOne(t *testing.T) {
	got := columnLines(16, 1, 3)
	want := []string{strings.Repeat(" ", 6) + "  Last    "}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("column = %q, want %q", got, want)
	}
}

func TestTabbingToAClippedButtonScrollsItIntoView(t *testing.T) {
	form := NewForm[uint8]()
	form.Declare(1, ControlKindButton{})
	form.Declare(2, ControlKindButton{})
	form.EndFrame(1)
	terminal := newComponentTerminal(10, 1)
	for _, selected := range []uint8{1, 2} {
		terminal.draw(func(frame *Frame) {
			form.BeginFrame()
			ButtonRowRender(frame, frame.Area(), []ButtonSpec[uint8]{
				{ID: 1, Label: "First", Enabled: true},
				{ID: 2, Label: "Last", Enabled: true},
			}, &form)
			form.EndFrame(1)
		})
		requireFocused(t, &form, selected)
		requireAction[uint8](t, click(&form, 5, 0), InteractionActivate[uint8]{ID: selected})
		form.Handle(key(tea.KeyTab))
	}
	if text := terminal.content(); !strings.Contains(text, "Last") {
		t.Fatalf("Last missing from %q", text)
	}
}

func TestWrappedChoiceRowsKeepNavigationAndInlineEditorHitboxesDistinct(t *testing.T) {
	form := NewForm[uint8]()
	form.Declare(1, ControlKindChoiceList{Len: 2, Selected: 0})
	form.EndFrame(1)
	terminal := newComponentTerminal(12, 7)
	input := TextInputFromValue("a界z")
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		ChoiceListRenderWrapped(
			frame,
			uv.Rect(0, 0, 12, 7),
			[]Line{
				RawLine("Heading"),
				RawLine("First option has long text"),
				RawLine("Second"),
				RawLine(""),
			},
			[]*int{nil, intPtr(0), intPtr(1), nil},
			0,
			0,
			&form,
			1,
		)
		TextFieldRenderInline(frame, uv.Rect(2, 5, 8, 1), &input, false, true, &form, 1)
		form.EndFrame(1)
	})
	requireAction[uint8](t, form.Handle(key(tea.KeyDown)).Action, InteractionSelect[uint8]{ID: 1, Index: 1})
	requireAction[uint8](t, click(&form, 1, 4), InteractionSelect[uint8]{ID: 1, Index: 1})
	requireAction(t, click(&form, 1, 0), nil)
	edit := form.Handle(formMouseDown(5, 5)).Action
	requireAction[uint8](t, edit, InteractionEdit[uint8]{ID: 1, Edit: FieldEditCursor{Offset: 4}})
}

func TestDrawnUnicodeFieldClickUsesDisplayCellsAndGraphemeBoundaries(t *testing.T) {
	form := NewForm[uint8]()
	input := TextInputFromValue("a界éz")
	input.SetCursor(0)
	form.Declare(1, ControlKindTextField{})
	form.EndFrame(1)
	terminal := newComponentTerminal(10, 2)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		TextFieldRender(frame, uv.Rect(1, 0, 8, 1), &input, &form, 1)
		form.EndFrame(1)
	})
	if got := terminal.symbol(2, 0); got != "界" {
		t.Fatalf("cell (2, 0) = %q, want 界", got)
	}
	result := form.Handle(formMouseDown(4, 0))
	requireAction[uint8](t, result.Action, InteractionEdit[uint8]{ID: 1, Edit: FieldEditCursor{Offset: 4}})
	TextFieldApply(&input, FieldEditCursor{Offset: 4})
	if input.Cursor() != len("a界") {
		t.Fatalf("cursor = %d, want %d", input.Cursor(), len("a界"))
	}
}

func TestMultilineFieldClickTracksWrappedNewlinesAndUnicode(t *testing.T) {
	form := NewForm[int32]()
	input := TextInputMultiline()
	input.SetValue("abcdefghi\nab界éz\nfinal")
	input.SetCursor(len(input.Value()))
	form.Declare(1, ControlKindTextField{})
	form.EndFrame(1)
	area := uv.Rect(2, 1, 8, 2)
	terminal := newComponentTerminal(14, 5)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		TextFieldRenderMultiline(frame, area, &input, true, true, &form, 1)
		form.EndFrame(1)
	})

	// The caret is on the final line, so the field scrolls to show the
	// second logical line and the final hard-newline-delimited line.
	clickField := func(column, row int) Interaction[int32] {
		return form.Handle(formMouseDown(column, row)).Action
	}
	first := clickField(area.Min.X+4, area.Min.Y)
	requireAction[int32](t, first, InteractionEdit[int32]{ID: 1, Edit: FieldEditCursor{Offset: len("abcdefghi\n") + len("ab界")}})
	firstEdit, ok := first.(InteractionEdit[int32])
	if !ok || firstEdit.ID != 1 {
		t.Fatal("wrapped row click should edit the field")
	}
	TextFieldApply(&input, firstEdit.Edit)
	TextFieldApply(&input, FieldEditKey{Key: key('X')})

	input.SetCursor(len(input.Value()))
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		TextFieldRenderMultiline(frame, area, &input, true, true, &form, 1)
		form.EndFrame(1)
	})

	second := clickField(area.Min.X, area.Min.Y+1)
	requireAction[int32](t, second, InteractionEdit[int32]{ID: 1, Edit: FieldEditCursor{Offset: len("abcdefghi\nab界Xéz\n")}})
	secondEdit, ok := second.(InteractionEdit[int32])
	if !ok || secondEdit.ID != 1 {
		t.Fatal("hard-newline row click should edit the field")
	}
	TextFieldApply(&input, secondEdit.Edit)
	TextFieldApply(&input, FieldEditKey{Key: key('Y')})
	if want := "abcdefghi\nab界Xéz\nYfinal"; input.Value() != want {
		t.Fatalf("value = %q, want %q", input.Value(), want)
	}
}

func TestMultilineFieldClickAfterAZeroWidthTabKeepsTheByteOffset(t *testing.T) {
	form := NewForm[int32]()
	input := TextInputMultiline()
	input.SetValue("a\t界")
	input.SetCursor(len(input.Value()))
	form.Declare(1, ControlKindTextField{})
	form.EndFrame(1)
	area := uv.Rect(1, 0, 8, 1)
	terminal := newComponentTerminal(10, 2)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		TextFieldRenderMultiline(frame, area, &input, true, true, &form, 1)
		form.EndFrame(1)
	})

	// Ratatui skips the tab control character, so the visible wide glyph
	// starts in the same cell as the tab's end boundary.
	result := form.Handle(formMouseDown(area.Min.X+1, area.Min.Y))
	requireAction[int32](t, result.Action, InteractionEdit[int32]{ID: 1, Edit: FieldEditCursor{Offset: len("a\t")}})
	edit, ok := result.Action.(InteractionEdit[int32])
	if !ok || edit.ID != 1 {
		t.Fatal("tab click should edit the field")
	}
	TextFieldApply(&input, edit.Edit)
	TextFieldApply(&input, FieldEditKey{Key: key('X')})
	if want := "a\tX界"; input.Value() != want {
		t.Fatalf("value = %q, want %q", input.Value(), want)
	}
}

func TestScrollingTabsHitTheDrawnLabelAndIgnoreSeparator(t *testing.T) {
	form := NewForm[uint8]()
	form.Declare(1, ControlKindTabs{Len: 3, Selected: 2})
	form.EndFrame(1)
	terminal := newComponentTerminal(10, 1)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		TabStripRender(frame, uv.Rect(0, 0, 10, 1), []string{"Long first", "界", "Last"}, 2, &form, 1)
		form.EndFrame(1)
	})
	if got := terminal.symbol(3, 0); got != "界" {
		t.Fatalf("cell (3, 0) = %q, want 界", got)
	}
	if got := terminal.symbol(6, 0); got != "L" {
		t.Fatalf("cell (6, 0) = %q, want L", got)
	}
	requireAction(t, click(&form, 5, 0), nil)
	requireAction[uint8](t, click(&form, 3, 0), InteractionSelect[uint8]{ID: 1, Index: 1})
	requireAction[uint8](t, click(&form, 6, 0), InteractionSelect[uint8]{ID: 1, Index: 2})
}

func TestClippedListClickSelectsTheVisibleRowAfterScrolling(t *testing.T) {
	form := NewForm[uint8]()
	form.Declare(1, ControlKindChoiceList{Len: 20, Selected: 19})
	form.EndFrame(1)
	lines := make([]Line, 0, 20)
	for index := range 20 {
		lines = append(lines, RawLine(fmt.Sprintf("row %d", index)))
	}
	terminal := newComponentTerminal(15, 3)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		ChoiceListRender(frame, frame.Area(), lines, 19, &form, 1)
		form.EndFrame(1)
	})
	if got := form.ListOffset(1); got != 17 {
		t.Fatalf("list offset = %d, want 17", got)
	}
	requireAction[uint8](t, click(&form, 1, 0), InteractionSelect[uint8]{ID: 1, Index: 17})
}

// comboboxGoldenBuffer draws the combobox, and optionally a button beside
// it, the way each golden state renders.
func comboboxGoldenBuffer(
	form *Form[uint8],
	value string,
	options []Line,
	selected int,
	expanded bool,
	width, height int,
	focus uint8,
	withButton bool,
) *componentTerminal {
	terminal := newComponentTerminal(width, height)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		fieldY := 1
		if height <= 1 {
			fieldY = 0
		}
		ComboBoxRender(
			frame,
			frame.Area(),
			uv.Rect(0, fieldY, 12, 1),
			value,
			options,
			selected,
			expanded,
			true,
			" choices ",
			PopupSideBelow,
			ComboBoxDefaultMaxVisibleRows,
			form,
			1,
		)
		if withButton {
			ButtonRender(frame, uv.Rect(14, 1, 14, 1), "Continue", true, form, 2)
		}
		form.EndFrame(focus)
	})
	return terminal
}

func appendComponentGoldenState(output *strings.Builder, label string, width, height int, terminal *componentTerminal, details []string) {
	if output.Len() > 0 {
		output.WriteString("\n")
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(BufferLines(terminal.buffer), "\n"))
	output.WriteString("\n")
	for _, detail := range details {
		output.WriteString(detail + "\n")
	}
}

func modelOptions(count int) []Line {
	options := make([]Line, 0, count)
	for index := range count {
		options = append(options, RawLine(fmt.Sprintf("model-%02d", index)))
	}
	return options
}

func TestGoldenComboboxPopup(t *testing.T) {
	var output strings.Builder
	options := []Line{RawLine("one"), RawLine("two"), RawLine("three")}

	form := NewForm[uint8]()
	var state ComboBoxState[uint8]
	committed := 0
	buffer := comboboxGoldenBuffer(&form, "one", options, committed, false, 20, 10, 1, true)
	opened := form.Handle(key(tea.KeyEnter))
	state.Open(1, committed)
	appendComponentGoldenState(&output, "collapsed field opens", 20, 10, buffer, []string{
		"action: " + debugInteraction(opened.Action),
	})
	buffer = comboboxGoldenBuffer(&form, "one", options, state.Selection(1, committed), state.IsOpen(1), 20, 10, 1, true)
	appendComponentGoldenState(&output, "popup opened", 20, 10, buffer, nil)
	preview := form.Handle(key(tea.KeyDown))
	routedPreview := state.Route(preview.Action)
	selected := state.Selection(1, committed)
	buffer = comboboxGoldenBuffer(&form, "one", options, selected, true, 20, 10, 1, true)
	appendComponentGoldenState(&output, "keyboard preview leaves committed value", 20, 10, buffer, []string{
		"routed preview: " + debugInteraction(routedPreview),
		fmt.Sprintf("committed selection: %d", committed),
		fmt.Sprintf("preview selection: %d", selected),
	})
	commit := form.Handle(key(tea.KeyTab))
	routedCommit := state.Route(commit.Action)
	if action, ok := routedCommit.(InteractionComboBoxCommit[uint8]); ok {
		committed = action.Index
	}
	buffer = comboboxGoldenBuffer(&form, "two", options, committed, false, 20, 10, 1, true)
	appendComponentGoldenState(&output, "tab commits highlighted option", 20, 10, buffer, []string{
		"routed action: " + debugInteraction(routedCommit),
		fmt.Sprintf("committed selection: %d", committed),
	})
	form.Focus(2)
	button := comboboxGoldenBuffer(&form, "two", options, committed, false, 30, 10, 2, true)
	action := form.Handle(key(tea.KeyEnter))
	routed := state.Route(action.Action)
	appendComponentGoldenState(&output, "unrelated button action passes through", 30, 10, button, []string{
		"routed action: " + debugInteraction(routed),
	})

	tinyForm := NewForm[uint8]()
	tiny := comboboxGoldenBuffer(&tinyForm, "long value", options, 0, false, 6, 1, 1, false)
	appendComponentGoldenState(&output, "collapsed glyph at minimum width", 6, 1, tiny, nil)

	form = NewForm[uint8]()
	form.Handle(key(tea.KeyEnter))
	popup := comboboxGoldenBuffer(&form, "one", options, 0, true, 20, 10, 1, false)
	clicked := click(&form, 1, 4)
	appendComponentGoldenState(&output, "mouse click commits popup row", 20, 10, popup, []string{
		"action: " + debugInteraction(clicked),
	})

	form = NewForm[uint8]()
	buffer = comboboxGoldenBuffer(&form, "model", options, 0, true, 20, 10, 1, false)
	scrolled := form.Handle(formScrollDown(1, 3))
	appendComponentGoldenState(&output, "popup scroll selects an option", 20, 10, buffer, []string{
		"action: " + debugInteraction(scrolled.Action),
	})
	dismissed := form.Handle(key(tea.KeyEscape))
	dismissedBuffer := comboboxGoldenBuffer(&form, "model", options, 1, false, 20, 10, 1, false)
	appendComponentGoldenState(&output, "escape dismisses expanded popup", 20, 10, dismissedBuffer, []string{
		"action: " + debugInteraction(dismissed.Action),
	})

	form = NewForm[uint8]()
	comboboxGoldenBuffer(&form, "one", options, 0, true, 20, 10, 1, false)
	outsideClick := click(&form, 1, 1)
	collapsed := comboboxGoldenBuffer(&form, "one", options, 0, false, 20, 10, 1, false)
	appendComponentGoldenState(&output, "outside click dismisses expanded popup", 20, 10, collapsed, []string{
		"action: " + debugInteraction(outsideClick),
	})

	largeOptions := modelOptions(30)
	for selected := range 30 {
		form := NewForm[uint8]()
		buffer := comboboxGoldenBuffer(&form, "model", largeOptions, selected, true, 30, 14, 1, false)
		appendComponentGoldenState(&output, fmt.Sprintf("selection centered or list pinned (%d)", selected), 30, 14, buffer, nil)
	}
	for selected := range 6 {
		form := NewForm[uint8]()
		buffer := comboboxGoldenBuffer(&form, "model", modelOptions(6), selected, true, 30, 14, 1, false)
		appendComponentGoldenState(&output, fmt.Sprintf("fitting list selection (%d)", selected), 30, 14, buffer, nil)
	}

	WithSymbols(core.SymbolSetAscii, func() {
		for _, selected := range []int{0, 19} {
			form := NewForm[uint8]()
			buffer := comboboxGoldenBuffer(&form, "model", modelOptions(20), selected, true, 30, 14, 1, false)
			appendComponentGoldenState(&output, fmt.Sprintf("ASCII scrollbar thumb (%d)", selected), 30, 14, buffer, nil)
		}
	})

	form = NewForm[uint8]()
	buffer = comboboxGoldenBuffer(&form, "model", modelOptions(5), 2, true, 30, 14, 1, false)
	appendComponentGoldenState(&output, "no scrollbar without overflow", 30, 14, buffer, nil)

	WithSymbols(core.SymbolSetUnicode, func() {
		for _, selected := range []int{0, 10, 19} {
			form := NewForm[uint8]()
			buffer := comboboxGoldenBuffer(&form, "model", modelOptions(20), selected, true, 30, 14, 1, false)
			appendComponentGoldenState(&output, fmt.Sprintf("Unicode scrollbar thumb (%d)", selected), 30, 14, buffer, []string{
				fmt.Sprintf("selected option: model-%02d", selected),
			})
		}
	})

	core.AssertGolden(t, "testdata", "combobox-popup", output.String())
}
