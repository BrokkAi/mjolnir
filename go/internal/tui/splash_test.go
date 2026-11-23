// Port of mj-tui/src/splash.rs tests.

package tui

import (
	"fmt"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// splashMs is the Rust tests' `MS`: Duration::from_millis.
func splashMs(millis int64) time.Duration {
	return time.Duration(millis) * time.Millisecond
}

func splashRendered(width, height int, frame *SplashFrame) *uv.Buffer {
	area := uv.Rect(0, 0, width, height)
	buf := uv.NewBuffer(width, height)
	RenderSplash(area, buf, frame)
	return buf
}

func splashTestFrame(elapsed time.Duration, timeline SplashTimeline) SplashFrame {
	return SplashFrame{
		Elapsed:    elapsed,
		Timeline:   timeline,
		Status:     nil,
		Background: chat.ColorReset,
	}
}

func TestTheSplashWaitsForLoadingBeforeDissolving(t *testing.T) {
	var timeline SplashTimeline
	if _, ok := timeline.DissolveStart(); ok {
		t.Fatal("dissolve start before loading is ready")
	}
	if timeline.Finished(splashMs(10_000)) {
		t.Fatal("finished before loading is ready")
	}
	if !timeline.Holding(splashMs(5_000)) {
		t.Fatal("the animation holds while loading runs")
	}

	timeline.MarkReady(splashMs(4_000))
	if start, ok := timeline.DissolveStart(); !ok || start != splashMs(4_000) {
		t.Fatalf("dissolve start = %v, %v", start, ok)
	}
	if timeline.Finished(splashMs(4_100)) {
		t.Fatal("finished during the dissolve")
	}
	if !timeline.Finished(splashMs(4_250)) {
		t.Fatal("not finished after the dissolve")
	}
}

func TestFastLoadingStillPlaysTheWholeAnimation(t *testing.T) {
	var timeline SplashTimeline
	timeline.MarkReady(splashMs(300))
	if start, ok := timeline.DissolveStart(); !ok || start != holdStart {
		t.Fatalf("dissolve start = %v, %v", start, ok)
	}
	if timeline.Finished(holdStart + dissolve - splashMs(1)) {
		t.Fatal("finished a millisecond early")
	}
	if !timeline.Finished(holdStart + dissolve) {
		t.Fatal("not finished at the end of the dissolve")
	}
}

func TestEverySizeAndInstantRendersWithinItsArea(t *testing.T) {
	var ready SplashTimeline
	ready.MarkReady(splashMs(1_200))
	status := "Waiting for the daemon"
	for _, size := range [][2]int{
		{0, 0},
		{1, 1},
		{59, 30},
		{60, 1},
		{60, 10},
		{80, 24},
		{300, 100},
	} {
		width, height := size[0], size[1]
		for _, timeline := range []SplashTimeline{{}, ready} {
			for elapsed := int64(0); elapsed < 4_000; elapsed += 37 {
				// A larger buffer catches writes outside the splash area.
				outer := uv.Rect(0, 0, width+4, height+4)
				area := uv.Rect(2, 2, width, height)
				buf := uv.NewBuffer(outer.Dx(), outer.Dy())
				frame := splashTestFrame(splashMs(elapsed), timeline)
				frame.Status = &status
				RenderSplash(area, buf, &frame)
				for y := outer.Min.Y; y < outer.Max.Y; y++ {
					for x := outer.Min.X; x < outer.Max.X; x++ {
						if !uv.Pos(x, y).In(area) && !buf.CellAt(x, y).Equal(&uv.EmptyCell) {
							t.Fatalf("%dx%d at %dms wrote (%d, %d) outside its area: %#v", width, height, elapsed, x, y, buf.CellAt(x, y))
						}
					}
				}
			}
		}
	}
}

func TestGoldenSplashFrames(t *testing.T) {
	var output strings.Builder

	var timeline SplashTimeline
	timeline.MarkReady(splashMs(100))
	background := splashColor([3]uint8{15, 18, 20})
	frame := splashTestFrame(holdStart+dissolve, timeline)
	frame.Background = background
	dissolved := splashRendered(100, 30, &frame)
	output.WriteString("=== dashboard background after dissolve (100x30) ===\n")
	output.WriteString(strings.Join(bufferLines(dissolved), "\n"))
	output.WriteByte('\n')
	cells := 0
	for y := range dissolved.Height() {
		for x := range dissolved.Width() {
			cell := dissolved.CellAt(x, y)
			cells++
			if cell.Content != " " || chat.CellStyle(cell).Bg != background {
				t.Fatalf("dissolved cell (%d, %d) is %#v", x, y, cell)
			}
		}
	}
	fmt.Fprintf(&output, "dissolved cells: %d; background: %#v\n", cells, chat.CellStyle(dissolved.CellAt(0, 0)).Bg)

	heldFrame := splashTestFrame(splashMs(1_800), SplashTimeline{})
	held := splashRendered(120, 40, &heldFrame)
	output.WriteString("\n=== held hammer and title (120x40) ===\n")
	heldLines := bufferLines(held)
	output.WriteString(strings.Join(heldLines, "\n"))
	output.WriteByte('\n')
	rgbSum := func(color chat.Color) (int, bool) {
		r, g, b, ok := color.RGB()
		if !ok {
			return 0, false
		}
		return int(r) + int(g) + int(b), true
	}
	brightness := func(x, y int) int {
		style := chat.CellStyle(held.CellAt(x, y))
		brightest := 0
		for _, color := range []chat.Color{style.Fg, style.Bg} {
			sum, _ := rgbSum(color)
			brightest = max(brightest, sum)
		}
		return brightest
	}
	ground, rows, half := groundY, float32(40.0), float32(2.0)
	headRow := 20 + asUsize(ground*rows/2.1/half) - 2
	lit := func(from, to int) int {
		count := 0
		for x := from; x < to; x++ {
			if brightness(x, headRow) > 150 {
				count++
			}
		}
		return count
	}
	left := lit(30, 60)
	right := lit(61, 91)
	titleRows := 0
	for y := headRow + 3; y < 40; y++ {
		bright := 0
		for x := range 120 {
			if brightness(x, y) > 300 {
				bright++
			}
		}
		if bright > 20 {
			titleRows++
		}
	}
	titleColors := map[string]struct{}{}
	for y := headRow + 3; y < 40; y++ {
		for x := range 120 {
			style := chat.CellStyle(held.CellAt(x, y))
			for _, color := range []chat.Color{style.Fg, style.Bg} {
				if sum, ok := rgbSum(color); ok && sum > 300 {
					titleColors[color.GoString()] = struct{}{}
				}
			}
		}
	}
	sortedColors := make([]string, 0, len(titleColors))
	for color := range titleColors {
		sortedColors = append(sortedColors, color)
	}
	slices.Sort(sortedColors)
	quoted := make([]string, len(sortedColors))
	for index, color := range sortedColors {
		quoted[index] = strconv.Quote(color)
	}
	fmt.Fprintf(
		&output,
		"hammer head row: %d; center brightness: %d; left/right lit cells: %d/%d; title rows: %d; bright title colors: {%s}\n",
		headRow, brightness(60, headRow), left, right, titleRows, strings.Join(quoted, ", "),
	)
	fmt.Fprintf(&output, "status row: %s\n", heldLines[39])

	core.AssertGolden(t, "testdata", "splash-frames", output.String())
}
