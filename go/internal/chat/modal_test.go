// Port of mj-chat/src/modal.rs (tests).

package chat

import (
	"fmt"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

var modalTestScreen = uv.Rect(0, 0, 130, 40)

// modalRectDebug is ratatui's Debug for a Rect.
func modalRectDebug(rect uv.Rectangle) string {
	return fmt.Sprintf("Rect { x: %d, y: %d, width: %d, height: %d }", rect.Min.X, rect.Min.Y, rect.Dx(), rect.Dy())
}

func assertKeepsModalMargin(t *testing.T, popup, area uv.Rectangle) {
	t.Helper()
	margin := ModalScreenMargin
	if !(popup.Min.X >= area.Min.X+margin &&
		popup.Min.Y >= area.Min.Y+margin &&
		popup.Max.X+margin <= area.Max.X &&
		popup.Max.Y+margin <= area.Max.Y) {
		t.Fatalf("%s does not keep %d cells inside %s", modalRectDebug(popup), margin, modalRectDebug(area))
	}
}

// Hard-won: 4e98115d: chat dialogs shipped without the two-cell screen margin.
func TestEveryCenteringHelperKeepsTheScreenMargin(t *testing.T) {
	assertKeepsModalMargin(t, CenteredRect(82, 30, modalTestScreen), modalTestScreen)
	assertKeepsModalMargin(t, CenteredRectPercent(82, 78, modalTestScreen), modalTestScreen)
	assertKeepsModalMargin(t, CenteredRectFixed(72, 14, modalTestScreen), modalTestScreen)
}

func TestAnOversizedModalIsClampedToTheMarginRatherThanOverflowing(t *testing.T) {
	assertKeepsModalMargin(t, CenteredRect(100, 400, modalTestScreen), modalTestScreen)
	assertKeepsModalMargin(t, CenteredRectPercent(100, 100, modalTestScreen), modalTestScreen)
	assertKeepsModalMargin(t, CenteredRectFixed(400, 400, modalTestScreen), modalTestScreen)
}

func centeredModalRender(label string, place func(uv.Rectangle) uv.Rectangle) string {
	const width, height = 90, 32
	pane := uv.Rect(20, 5, 60, 20)
	popup := place(pane)
	buffer := uv.NewBuffer(width, height)
	frame := NewFrame(buffer)
	frame.RenderWidget(BorderedBlock().WithTitle(RawLine(label)), popup)
	output := fmt.Sprintf("=== %s (%dx%d) ===\npane: %s\npopup: %s\n", label, width, height, modalRectDebug(pane), modalRectDebug(popup))
	output += strings.Join(BufferLines(buffer), "\n")
	output += "\n"
	return output
}

func TestGoldenModalLayout(t *testing.T) {
	pane := uv.Rect(20, 5, 60, 20)
	states := []struct {
		label string
		popup uv.Rectangle
	}{
		{"percentage width and fixed height", CenteredRect(80, 10, pane)},
		{"percentage width and height", CenteredRectPercent(80, 50, pane)},
		{"fixed dimensions", CenteredRectFixed(30, 10, pane)},
	}
	var output strings.Builder
	for _, state := range states {
		assertKeepsModalMargin(t, state.popup, pane)
		var rendered string
		switch state.label {
		case "percentage width and fixed height":
			rendered = centeredModalRender(state.label, func(area uv.Rectangle) uv.Rectangle { return CenteredRect(80, 10, area) })
		case "percentage width and height":
			rendered = centeredModalRender(state.label, func(area uv.Rectangle) uv.Rectangle { return CenteredRectPercent(80, 50, area) })
		default:
			rendered = centeredModalRender(state.label, func(area uv.Rectangle) uv.Rectangle { return CenteredRectFixed(30, 10, area) })
		}
		output.WriteString(rendered)
	}
	output.WriteString("=== end of rendered states ===\n")
	core.AssertGolden(t, "testdata", "modal-layout", output.String())
}

func TestATerminalTooSmallForTheMarginStillYieldsAVisibleModal(t *testing.T) {
	tiny := uv.Rect(0, 0, 4, 3)
	for _, popup := range []uv.Rectangle{
		CenteredRect(100, 3, tiny),
		CenteredRectPercent(100, 100, tiny),
		CenteredRectFixed(4, 3, tiny),
	} {
		if !(popup.Dx() > 0 && popup.Dy() > 0) {
			t.Fatalf("%s vanished", modalRectDebug(popup))
		}
	}
}

func TestClearingAModalBlanksItsTwoCellHaloAndNothingBeyondIt(t *testing.T) {
	const width, height = 30, 15
	bounds := uv.Rect(3, 2, 20, 10)
	popup := uv.Rect(8, 5, 6, 3)
	expectedClear := uv.Rect(6, 3, 10, 7)
	if got := modalClearArea(popup, bounds); got != expectedClear {
		t.Fatalf("modalClearArea = %s, want %s", modalRectDebug(got), modalRectDebug(expectedClear))
	}

	buffer := uv.NewBuffer(width, height)
	frame := NewFrame(buffer)
	rows := make([]Line, 0, height)
	for range height {
		rows = append(rows, RawLine(strings.Repeat("X", width)))
	}
	frame.RenderWidget(
		NewParagraph(TextFrom(rows...)).WithStyle(Style{}.Foreground(ColorCyan).Background(ColorBlue)),
		frame.Area(),
	)
	clearModal(frame, popup, bounds)

	text := ThemePalette().Text.TerminalColor()
	background := ThemePalette().Background.TerminalColor()
	for y := range height {
		for x := range width {
			cell := buffer.CellAt(x, y)
			if uv.Pos(x, y).In(expectedClear) {
				if cell.Content != " " {
					t.Fatalf("cell (%d, %d) was not blank", x, y)
				}
				if cell.Style.Fg != text {
					t.Fatalf("cell (%d, %d) kept its foreground", x, y)
				}
				if cell.Style.Bg != background {
					t.Fatalf("cell (%d, %d) kept its background", x, y)
				}
			} else if cell.Content != "X" {
				t.Fatalf("cell (%d, %d) outside the halo changed", x, y)
			}
		}
	}
}

func modalDismissState(label string, form *Form[uint8], popup uv.Rectangle, enabled bool) string {
	titleStyle := Style{}.Foreground(ColorYellow).Add(ModifierItalic)
	title := DismissibleModalTitle(form, popup, "Settings", titleStyle, enabled)
	spans := make([]string, 0, len(title.Spans))
	for _, span := range title.Spans {
		spans = append(spans, fmt.Sprintf("%q=%#v", span.Content, span.Style))
	}
	targets := make([]string, 0, 3)
	for x := popup.Min.X + 1; x < popup.Min.X+4; x++ {
		targets = append(targets, fmt.Sprintf("%d:%t", x, form.Contains(x, popup.Min.Y)))
	}
	buffer := uv.NewBuffer(40, 16)
	NewFrame(buffer).RenderWidget(BorderedBlock().WithTitle(title), popup)
	return fmt.Sprintf(
		"=== %s (40x16) ===\nenabled: %t\ntitle spans: %s\nhit cells: %s\noutside: %t %t\n%s\n",
		label,
		enabled,
		strings.Join(spans, " | "),
		strings.Join(targets, " "),
		form.Contains(popup.Min.X, popup.Min.Y),
		form.Contains(popup.Min.X+4, popup.Min.Y),
		strings.Join(BufferLines(buffer), "\n"),
	)
}

func TestGoldenModalDismissControl(t *testing.T) {
	popup := uv.Rect(4, 2, 30, 8)
	form := NewForm[uint8]()
	output := modalDismissState("available close affordance", &form, popup, true)
	form.Handle(tea.MouseClickMsg{X: popup.Min.X + 2, Y: popup.Min.Y, Button: tea.MouseLeft})
	output += modalDismissState("armed close affordance", &form, popup, true)
	output += modalDismissState("disabled close affordance", &form, popup, false)
	output += "=== end of rendered states ===\n"
	core.AssertGolden(t, "testdata", "modal-dismiss-control", output)
}
