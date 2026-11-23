// Port of mj-tui/src/wizards/picker.rs.

package tui

import (
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
	panic("unported: mj-tui/src/wizards/picker.rs PickerChoice::line")
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
	panic("unported: mj-tui/src/wizards/picker.rs render_picker")
}
