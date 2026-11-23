// Port of mj-tui/src/render/sessions.rs tests (row_height_tests).

package tui

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestEachSessionFormDrawsItsDecidedHeight(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	forms := []SessionRowForm{
		SessionRowFormRepair{Hint: false},
		SessionRowFormRepair{Hint: true},
		SessionRowFormTransition{Kind: core.SessionTransitionKindMoving, Failed: false},
		SessionRowFormTransition{Kind: core.SessionTransitionKindMoving, Failed: true},
		SessionRowFormExpanded{},
		SessionRowFormCompact{},
	}
	for _, minimized := range []bool{false, true} {
		for _, outsideFilter := range []bool{false, true} {
			for _, form := range forms {
				layout := newSessionsLayout(dashboard, 60)
				presentation := &layout.shapes[0].full
				if minimized {
					presentation = &layout.shapes[0].minimized
				}
				presentation.form = form
				presentation.outsideFilter = outsideFilter
				expected := presentation.height()
				options := sessionRowsRenderOptionsDashboard
				if minimized {
					options = sessionRowsRenderOptionsMinimized
				}
				rows := drawnSessionRowsInRange(dashboard, &layout, options, 0, 1)
				if len(rows[0].lines) != expected {
					t.Fatalf("minimized=%v, outside_filter=%v, form %#v: %d lines, want %d",
						minimized, outsideFilter, form, len(rows[0].lines), expected)
				}
			}
		}
	}
}

// The pane now chooses its own offset, so it must still scroll exactly as
// ratatui's Table did when it was given every row, exact fits and trailing
// spacing included.
func TestSelectionScrollsTheWayTheTableDid(t *testing.T) {
	type shape struct{ content, spacing int }
	// Every row of up to three lines, with or without spacing beneath.
	shapes := []shape{{1, 0}, {1, 1}, {2, 0}, {2, 1}, {3, 0}, {3, 1}}
	cases := [][]shape{{}}
	for range 4 {
		var next [][]shape
		for _, prefix := range cases {
			for _, added := range shapes {
				extended := append(append([]shape(nil), prefix...), added)
				next = append(next, extended)
			}
		}
		cases = next
		for _, rowsCase := range cases {
			// The last row's spacing is not content, as in the pane.
			rows := append([]shape(nil), rowsCase...)
			rows[len(rows)-1].spacing = 0
			heights := make([]int, len(rows))
			for i, row := range rows {
				heights[i] = row.content + row.spacing
			}
			for viewport := 3; viewport <= 8; viewport++ {
				for start := range rows {
					for selected := start; selected < len(rows); selected++ {
						// Taller than the pane, the table would hide the
						// selection; the pane keeps it at the top.
						if heights[selected] > viewport {
							continue
						}
						tableRows := make([]TableRow, len(rows))
						for i, row := range rows {
							tableRows[i] = NewTableRowFromStrings("x").WithHeight(row.content).WithBottomMargin(row.spacing)
						}
						table := NewTable(tableRows, []chat.Constraint{chat.ConstraintMin(1)})
						chosen := selected
						state := TableState{}.WithOffset(start).WithSelected(&chosen)
						terminal := newRenderTerminal(4, viewport)
						terminal.draw(func(frame *chat.Frame) {
							table.RenderStateful(frame.Area(), frame.Buffer(), &state)
						})
						if got := offsetShowingSelected(start, selected, rows[selected].content, heights, viewport); got != state.Offset() {
							t.Fatalf("rows=%v viewport=%d start=%d selected=%d: offset %d, table %d",
								rows, viewport, start, selected, got, state.Offset())
						}
					}
				}
			}
		}
	}
}
