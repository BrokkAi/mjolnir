// Tests of mj-chat/src/spinner.rs.

package chat

import (
	"fmt"
	"strings"
	"testing"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// spinnerStyleDebug is SpinnerStyle's derived Debug: the variant name.
func spinnerStyleDebug(style SpinnerStyle) string {
	name := string(style)
	return strings.ToUpper(name[:1]) + name[1:]
}

// colorOptionDebug is Rust's Debug for a style's `Option<Color>`.
func colorOptionDebug(color Color) string {
	if !color.IsSet() {
		return "None"
	}
	return fmt.Sprintf("Some(%#v)", color)
}

func stringsDebug(values []string) string {
	parts := make([]string, 0, len(values))
	for _, value := range values {
		parts = append(parts, core.RustDebugString(value))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func runsDebug(runs []SpinnerRun) string {
	parts := make([]string, 0, len(runs))
	for _, run := range runs {
		parts = append(parts, fmt.Sprintf("(%s, %#v)", core.RustDebugString(run.Text), run.Ink))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func appendActivity(output *strings.Builder, label string, style SpinnerStyle, elapsed uint64, active bool, model string) {
	line := ActivityLine(style, elapsed, active)
	spans := make([]string, 0, len(line.Spans))
	for _, span := range line.Spans {
		spans = append(spans, fmt.Sprintf("%s fg=%s", core.RustDebugString(span.Content), colorOptionDebug(span.Style.Fg)))
	}
	buffer := uv.NewBuffer(SpinnerWidth, 1)
	NewParagraph(TextFrom(line)).Render(buffer.Bounds(), buffer)
	fmt.Fprintf(output, "=== %s (%dx1) ===\n", label, SpinnerWidth)
	fmt.Fprintf(output, "%s\nspans: %s\n", model, strings.Join(spans, " | "))
	output.WriteString(strings.Join(BufferLines(buffer), "\n"))
	output.WriteByte('\n')
}

func appendCompactSpan(output *strings.Builder, style SpinnerStyle) {
	span := CompactSpan(style, 0)
	buffer := uv.NewBuffer(1, 1)
	NewParagraph(TextFrom(LineFrom(span))).Render(buffer.Bounds(), buffer)
	fmt.Fprintf(output, "=== %s compact styled span at zero (1x1) ===\nwidth: %d; style: %#v\n%s\n\n",
		spinnerStyleDebug(style), span.Width(), span.Style, strings.Join(BufferLines(buffer), "\n"))
}

func TestGoldenActivitySpinner(t *testing.T) {
	var output strings.Builder
	WithTheme(core.UiThemeMidnight, func() {
		WithSymbols(core.SymbolSetUnicode, func() {
			for _, style := range core.SpinnerStyleAll {
				frames := spinnerStyleFrames(style)
				interval := spinnerStyleFrameIntervalMs(style)
				loopMs := uint64(len(frames)) * interval
				fmt.Fprintf(&output, "=== %s timing and compact frames (%dx1) ===\nframe interval: %dms; loop: %dms; compact: %s\n",
					spinnerStyleDebug(style), SpinnerWidth, interval, loopMs, stringsDebug(spinnerStyleCompactFrames(style)))
				appendCompactSpan(&output, style)
				for index := range frames {
					frame := &frames[index]
					elapsed := uint64(index) * interval
					appendActivity(&output,
						fmt.Sprintf("%s animated frame %d at %dms", spinnerStyleDebug(style), index, elapsed),
						style, elapsed, true,
						fmt.Sprintf("frame: %s; runs: %s", core.RustDebugString(frame.Text()), runsDebug(frame.Runs())))
				}
				appendActivity(&output, spinnerStyleDebug(style)+" first frame before cadence",
					style, max(interval, 1)-1, true, "active at interval minus one")
				appendActivity(&output, spinnerStyleDebug(style)+" frame after three intervals",
					style, interval*3, true, "active after three intervals")
				appendActivity(&output, spinnerStyleDebug(style)+" idle at zero",
					style, 0, false, "idle frame")
				appendActivity(&output, spinnerStyleDebug(style)+" idle at 100000ms",
					style, 100_000, false, "idle frame at a later clock")
				for index, frame := range spinnerStyleCompactFrames(style) {
					elapsed := uint64(index) * SpinnerFrameIntervalMs
					fmt.Fprintf(&output, "=== %s compact frame %d at %dms (1x1) ===\nsource: %s; rendered: %s\n",
						spinnerStyleDebug(style), index, elapsed, frame, CompactFrame(style, elapsed))
				}
				if style == core.SpinnerStyleScan {
					heads := scanHeads()
					headTexts := make([]string, 0, len(heads))
					for _, head := range heads {
						headTexts = append(headTexts, fmt.Sprint(head))
					}
					trail := make([]string, 0, SpinnerWidth)
					for distance := range int64(SpinnerWidth) {
						cell := scanCell(distance)
						trail = append(trail, fmt.Sprintf("('%c', %#v)", cell.glyph, cell.ink))
					}
					fmt.Fprintf(&output, "scan crossing: %dms; redraw interval: %dms; heads: [%s]; first trail: [%s]\n",
						uint64(SpinnerWidth-1)*spinnerStyleFrameIntervalMs(style), SpinnerRedrawIntervalMs,
						strings.Join(headTexts, ", "), strings.Join(trail, ", "))
				}
			}
		})
	})
	core.AssertGolden(t, "testdata", "activity-spinner", output.String())
}

func TestEveryFrameHasStableDisplayWidth(t *testing.T) {
	for _, style := range core.SpinnerStyleAll {
		frames := spinnerStyleFrames(style)
		if len(frames) == 0 {
			t.Fatalf("%s has no frames", style)
		}
		for _, frame := range frames {
			if width := displayWidth(frame.Text()); width != SpinnerWidth {
				t.Fatalf("%s frame %q wrong width %d", style, frame.Text(), width)
			}
		}
		if width := displayWidth(spinnerStyleIdleFrame(style).Text()); width != SpinnerWidth {
			t.Fatalf("%s idle wrong width %d", style, width)
		}
		for _, frame := range spinnerStyleCompactFrames(style) {
			if width := displayWidth(frame); width != 1 {
				t.Fatalf("%s compact frame %q must be one column, is %d", style, frame, width)
			}
		}
	}
}

func TestScanBouncesOffBothWallsWithoutWrapping(t *testing.T) {
	heads := scanHeads()
	// The light must actually reach both walls before turning around…
	reachesLeft, reachesRight := false, false
	for _, head := range heads {
		reachesLeft = reachesLeft || head == 0
		reachesRight = reachesRight || head == SpinnerWidth-1
	}
	if !reachesLeft {
		t.Fatal("scan never reaches the left wall")
	}
	if !reachesRight {
		t.Fatal("scan never reaches the right wall")
	}
	// …and travel there smoothly: a wrap like Pulse's would show up as a
	// near-full-width jump between consecutive frames.
	if heads[0] != 0 || heads[len(heads)-1] != 0 {
		t.Fatalf("scan starts at %d and ends at %d", heads[0], heads[len(heads)-1])
	}
	for index := 1; index < len(heads); index++ {
		head, next := heads[index-1], heads[index]
		if diff := head - next; diff != 1 && diff != -1 {
			t.Fatalf("scan head jumps from %d to %d", head, next)
		}
	}
}
