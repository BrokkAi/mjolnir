// Port of mj-tui/src/splash/font.rs.
//
// The title, MJÖLNIR, in gold block letters revealed by a white-hot sweep.
package tui

import "math"

const glyphWidth = 5

// glyphHeight leaves two rows above the capitals for the umlaut.
const glyphHeight = 9

// glyphGap is Rust's font GAP, fontWidth its WIDTH and fontReveal its
// REVEAL: package tui is flat, so these generic names carry the module's.
const (
	glyphGap  = 2
	fontWidth = 7*glyphWidth + 6*glyphGap
	// fontReveal is typed so titleStart+fontReveal rounds as Rust's f32
	// sum does.
	fontReveal float32 = 0.45
)

var fontGlyphs = [7][glyphHeight]string{
	{"", "", "#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"},
	{"", "", "..###", "...#.", "...#.", "...#.", "...#.", "#..#.", ".##.."},
	{".#.#.", "", ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."},
	{"", "", "#....", "#....", "#....", "#....", "#....", "#....", "#####"},
	{"", "", "#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"},
	{"", "", "#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"},
	{"", "", "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"},
}

func lit(letter, row, column int) bool {
	glyph := fontGlyphs[letter][row]
	return column < len(glyph) && glyph[column] == '#'
}

// Title is where the title sits on the canvas and how large it is drawn.
type Title struct {
	x     int
	y     int
	scale int
}

// titleLayout centres the title in the space between the horizon and the
// status row, as large as fits. A terminal too short for it shows the
// scene alone.
func titleLayout(view *View) (Title, bool) {
	horizon := float32(view.height)/2.0 + groundY*view.scale
	top := int(horizon + max(view.scale*0.1, 2.0))
	bottom := max(view.height-2, 0)
	room := max(bottom-top, 0)
	scale := min(view.width*17/20/fontWidth, room/glyphHeight)
	if scale <= 0 {
		return Title{}, false
	}
	return Title{
		x:     (view.width - fontWidth*scale) / 2,
		y:     top + (room-glyphHeight*scale)/2,
		scale: scale,
	}, true
}

// sweep is how far the reveal has swept, in font columns.
func sweep(t float32) float32 {
	return -6.0 + smoothstep(titleStart, titleStart+fontReveal, t)*(float32(fontWidth)+12.0)
}

// glint is a highlight that crosses the finished title while the splash
// waits.
func glint(t float32, holding bool) (float32, bool) {
	if !holding {
		return 0, false
	}
	// Rust's f32 `%` is an exact fmod of the f32 operands.
	cycle := float32(math.Mod(float64(t-durationSecsF32(holdStart)), float64(float32(1.8)))) / 0.7
	if cycle >= 1.0 {
		return 0, false
	}
	return -10.0 + cycle*(float32(fontWidth)+30.0), true
}

// fontDraw is Rust's font::draw; the three splash `draw` functions carry
// their module's name in the flat package.
func fontDraw(canvas *Canvas, placement Title, t float32, holding bool) {
	panic("unported: mj-tui/src/splash/font.rs draw")
}

func fillBlock(canvas *Canvas, placement Title, letter, row, column, offset int, paint func(Rgb) Rgb) {
	scale := placement.scale
	left := placement.x + (letter*(glyphWidth+glyphGap)+column)*scale + offset
	top := placement.y + row*scale + offset
	for y := top; y < min(top+scale, canvas.height); y++ {
		for x := left; x < min(left+scale, canvas.width); x++ {
			under := canvas.get(x, y)
			canvas.set(x, y, paint(under))
		}
	}
}
