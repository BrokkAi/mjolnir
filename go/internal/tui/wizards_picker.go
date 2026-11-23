// Port of mj-tui/src/wizards/picker.rs.

package tui

import (
	"math"
	"slices"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// PickerNavigation is how a picker step's list and buttons behave.
type PickerNavigation struct {
	// resources is nil when the step offers no resource fields.
	resources ResourcePicker
	hasBack   bool
	// selected is the row the keyboard is on, highlighted while the content
	// has focus.
	selected    int
	control     WizardControl
	nextEnabled bool
	// sideActions are actions on the listed items, e.g. the project step's
	// Add and Remove, stacked in a column to the right of the list as the
	// Workspaces manager stacks its actions.
	sideActions []chat.ButtonSpec[WizardControl]
	// emptyHint is the muted line drawn in place of an empty list, so the
	// step never renders a blank picker. Nil for none.
	emptyHint *string
}

// PickerCell is one cell of a picker table row.
type PickerCell struct {
	text  string
	style chat.Style
}

func pickerCellText(text string) PickerCell {
	return PickerCell{text: text}
}

func pickerCellStyled(text string, style chat.Style) PickerCell {
	return PickerCell{text: text, style: style}
}

// pickerCellBlank is an empty cell that keeps the columns after it aligned
// on rows that need no entry here.
func pickerCellBlank() PickerCell {
	return pickerCellText("")
}

func (c PickerCell) width() int {
	return chat.RawLine(c.text).Width()
}

// PickerChoice is one picker row. A disabled row stays in the list so row
// numbers keep matching the underlying map order; it is greyed out and
// refuses Enter.
//
// Rows of several cells draw as a table: every cell but the row's last is
// padded to its column, so the columns line up down the list.
type PickerChoice struct {
	cells    []PickerCell
	disabled bool
	// heading rows name the columns and carry no item, so the picker leaves
	// them out of its row map and item indexes keep naming their own rows.
	heading bool
}

// pickerChoiceTable is a row of table cells padded into aligned columns.
func pickerChoiceTable(cells []PickerCell) PickerChoice {
	return PickerChoice{cells: cells}
}

// intoDisabled greys out a row that keeps its place in the list but refuses
// Enter.
func (c PickerChoice) intoDisabled() PickerChoice {
	c.disabled = true
	return c
}

// pickerChoiceHeading is a non-selectable heading row.
func pickerChoiceHeading(cells []PickerCell) PickerChoice {
	choice := pickerChoiceTable(cells)
	choice.heading = true
	return choice
}

// withNote appends a trailing note, e.g. the resume step's lossy-transcript
// marker, after the padded columns.
func (c PickerChoice) withNote(note string) PickerChoice {
	c.cells = append(c.cells, pickerCellText(note))
	return c
}

// line is the row as one line, each cell padded to its column. The last cell
// is cut with an ellipsis where the row would pass maxWidth, so a long note
// such as a target's unavailability reason shows it was cut (launch finding
// C-10).
func (c PickerChoice) line(widths []int, maxWidth int) chat.Line {
	var spans []chat.Span
	used := 0
	for index, cell := range c.cells {
		if index+1 == len(c.cells) {
			room := max(maxWidth-used, 0)
			text := cell.text
			if cell.width() > room {
				text = truncateToCells(cell.text, room, TruncateSummary)
			}
			spans = append(spans, chat.StyledSpan(text, cell.style))
			break
		}
		text := truncateToCells(cell.text, widths[index], TruncatePlain)
		cellWidth := chat.RawLine(text).Width()
		padding := max(widths[index]+columnGap-cellWidth, 0)
		spans = append(spans, chat.StyledSpan(text, cell.style))
		if padding > 0 {
			spans = append(spans, chat.RawSpan(strings.Repeat(" ", padding)))
		}
		used += cellWidth + padding
	}
	return chat.LineFrom(spans...)
}

// columnGap is the blank cells between adjacent table columns.
const columnGap = 2

// pickerColumns is the widest cell of every column across the rows, so each
// cell can be padded to its column.
func pickerColumns(choices []PickerChoice) []int {
	var widths []int
	for _, choice := range choices {
		for index, cell := range choice.cells {
			width := cell.width()
			if index < len(widths) {
				widths[index] = max(widths[index], width)
			} else {
				widths = append(widths, width)
			}
		}
	}
	return widths
}

// profileHeadings is the heading of the profile tables, in the same columns
// as their rows.
func profileHeadings() PickerChoice {
	style := chat.Muted().Bold()
	return pickerChoiceHeading([]PickerCell{
		pickerCellBlank(),
		pickerCellStyled("PROFILE", style),
		pickerCellStyled("HARNESS", style),
		pickerCellStyled("WEEKLY", style),
		pickerCellStyled("5H", style),
	})
}

// targetTable is the target tables of the new-session and resume wizards:
// the column headings followed by rows. The blank first column indents the
// table as it indents the profile table.
func targetTable(rows []PickerChoice) []PickerChoice {
	style := chat.Muted().Bold()
	table := make([]PickerChoice, 0, len(rows)+1)
	table = append(table, pickerChoiceHeading([]PickerCell{
		pickerCellBlank(),
		pickerCellStyled("TARGET", style),
		pickerCellStyled("KIND", style),
		pickerCellStyled("CPU", style),
		pickerCellStyled("MEM (GiB)", style),
		pickerCellStyled("STATUS", style),
	}))
	return append(table, rows...)
}

// newWizardProfileTable is the profile table of the new-session wizard:
// PROVIDER, PROFILE, WEEKLY, 5H.
func newWizardProfileTable(rows []PickerChoice) []PickerChoice {
	if len(rows) == 0 {
		return rows
	}
	style := chat.Muted().Bold()
	table := make([]PickerChoice, 0, len(rows)+1)
	table = append(table, pickerChoiceHeading([]PickerCell{
		pickerCellBlank(),
		pickerCellStyled("PROVIDER", style),
		pickerCellStyled("PROFILE", style),
		pickerCellStyled("WEEKLY", style),
		pickerCellStyled("5H", style),
	}))
	return append(table, rows...)
}

// profileTable is the profile tables of the new-session and resume wizards:
// the column headings followed by rows. An empty list keeps its hint instead
// of rendering a bare heading.
func profileTable(rows []PickerChoice) []PickerChoice {
	if len(rows) == 0 {
		return rows
	}
	table := make([]PickerChoice, 0, len(rows)+1)
	table = append(table, profileHeadings())
	return append(table, rows...)
}

// pickerHelp is the muted help row of a picker step.
func pickerHelp(text string) chat.Line {
	return chat.StyledLine(text, chat.Muted())
}

// renderPicker draws a picker step. The form and surface registry are
// distinct rendering owners.
func renderPicker(
	frame *chat.Frame,
	area uv.Rectangle,
	title string,
	choices []PickerChoice,
	help []chat.Line,
	navigation PickerNavigation,
	form *chat.Dialog[WizardControl],
	surfaces *chat.FrameSurfaces,
) {
	popup := wizardModal(frame, surfaces, area)
	container, hasContainer := navigation.resources.(ResourcePickerContainer)
	containerOptionRows := 0
	if hasContainer && container.SaveAsDefault != nil {
		containerOptionRows = 1
	}
	content := chat.DialogShellPaddedInner(popup)
	layout := chat.DialogShellLayout(content, 1)
	buttonArea := layout.Actions
	columns := form.SplitActions(layout.Body, navigation.sideActions)
	body := columns.Body
	// Wrapped to the width it is drawn in, so a long reason wraps instead of
	// being cut off at the dialog's padding.
	help = chat.WrapLines(help, body.Dx())
	helpGap := 0
	if len(help) > 0 {
		helpGap = 1
	}
	emptyRow := 0
	if len(choices) == 0 && navigation.emptyHint != nil {
		emptyRow = 1
	}
	listHeight := min(
		max(min(len(choices), math.MaxUint16), emptyRow),
		min(max(max(body.Dy()-(len(help)+helpGap), 0)-containerOptionRows, 1), body.Dy()),
	)
	listArea := uv.Rect(body.Min.X, body.Min.Y, body.Dx(), listHeight)
	widths := pickerColumns(choices)
	if navigation.control == WizardControlProfileList && len(widths) == 5 && len(choices) > 0 &&
		choices[0].heading && len(choices[0].cells) > 1 && choices[0].cells[1].text == "PROVIDER" {
		// Bound the Provider column so a long custom endpoint hostname never
		// pushes the Profile and quota columns off-screen.
		maxProvider := max(listArea.Dx()/3, 14)
		widths[1] = min(widths[1], maxProvider)
	}
	if navigation.control == WizardControlTargetList && len(widths) == 6 {
		widths[0] = 0
		widths[3] = 6
		widths[4] = 12
		widths[5] = min(max(widths[5], 6), 11)
		remaining := max(listArea.Dx()-(6+12+widths[5]+5*columnGap), 0)
		widths[1] = min(widths[1], remaining/2)
		widths[2] = min(widths[2], max(remaining-widths[1], 0))
	}
	rows := make([]chat.Line, 0, len(choices))
	for _, choice := range choices {
		rows = append(rows, choice.line(widths, listArea.Dx()))
	}
	rowMap := make([]*int, 0, len(choices))
	items := 0
	for _, choice := range choices {
		if choice.heading {
			rowMap = append(rowMap, nil)
		} else {
			item := items
			rowMap = append(rowMap, &item)
			items++
		}
	}
	rowEnabled := make([]bool, 0, len(choices))
	for _, choice := range choices {
		rowEnabled = append(rowEnabled, !choice.disabled)
	}
	helpY := listArea.Max.Y + helpGap
	helpHeight := min(len(help), max(body.Max.Y-helpY, 0))
	helpArea := uv.Rect(body.Min.X, helpY, body.Dx(), helpHeight)
	titleLine := dismissibleModalTitle(&form.Form, popup, strings.TrimSpace(title), chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	chat.ChoiceListRenderWithRows(
		frame,
		listArea,
		rows,
		navigation.selected,
		rowMap,
		rowEnabled,
		&form.Form,
		navigation.control,
	)
	if len(choices) == 0 && navigation.emptyHint != nil {
		frame.RenderWidget(
			chat.NewParagraph(chat.TextFrom(chat.StyledLine(
				*navigation.emptyHint,
				chat.Style{}.Foreground(chat.ThemePalette().Muted),
			))),
			listArea,
		)
	}
	declareResourceControls(form, navigation.resources)
	if hasContainer {
		selectedDisplayRow := slices.IndexFunc(rowMap, func(row *int) bool {
			return row != nil && *row == navigation.selected
		})
		offset := form.ListOffset(navigation.control)
		if selectedDisplayRow >= offset && selectedDisplayRow-offset < listArea.Dy() {
			y := listArea.Min.Y + (selectedDisplayRow - offset)
			x := listArea.Min.X
			for column, width := range widths {
				var input *chat.TextInput
				var id WizardControl
				switch column {
				case 3:
					input, id = &container.Editor.cpu, WizardControlResourceCpu
				case 4:
					input, id = &container.Editor.memory, WizardControlResourceMemory
				}
				if input != nil {
					field := uv.Rect(x, y, min(width, max(listArea.Max.X-x, 0)), 1)
					chat.TextFieldRenderUnderlined(frame, field, input, &form.Form, id)
				}
				x += width + columnGap
			}
		}
		if container.SaveAsDefault != nil {
			chat.CheckboxRender(
				frame,
				uv.Rect(body.Min.X, listArea.Max.Y, body.Dx(), 1).Intersect(body),
				"Save as default sizing",
				*container.SaveAsDefault,
				true,
				&form.Form,
				WizardControlRememberContainerSize,
			)
		}
	}
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(help...)), helpArea)
	buttons := []chat.ButtonSpec[WizardControl]{{ID: WizardControlCancel, Label: "Cancel", Enabled: true}}
	if navigation.hasBack {
		buttons = append(buttons, chat.ButtonSpec[WizardControl]{ID: WizardControlBack, Label: "Back", Enabled: true})
	}
	buttons = append(buttons, chat.ButtonSpec[WizardControl]{ID: WizardControlNext, Label: "Next", Enabled: navigation.nextEnabled})
	form.RenderActions(frame, buttonArea, buttons)
	if len(navigation.sideActions) > 0 {
		form.RenderActionsStacked(frame, columns.Actions, navigation.sideActions, chat.ColumnAlignRight)
	}
}
