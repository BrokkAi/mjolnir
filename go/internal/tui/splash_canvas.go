// Port of mj-tui/src/splash/canvas.rs.
//
// A linear-light pixel buffer at two pixels per cell, and the single place
// where splash colors become terminal colors.
package tui

import (
	"math"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// Rgb is one linear-light pixel.
type Rgb = [3]float32

// bayerMatrix holds the ordered-dither thresholds (Rust's BAYER, renamed
// because the bayer function shares its Go name). They depend only on
// position, so dithered pixels stay put between frames and the terminal
// diff stays small.
var bayerMatrix = [4][4]uint8{{0, 8, 2, 10}, {12, 4, 14, 6}, {3, 11, 1, 9}, {15, 7, 13, 5}}

// quantum is how far apart output levels are, which hides slow drift in
// the clouds from the cell diff without visible banding once dithered.
const quantum float32 = 6.0

func bayer(x, y int) float32 {
	return (float32(bayerMatrix[y%4][x%4]) + 0.5) / 16.0
}

// splashColor is Rust's canvas `color`, renamed so it cannot collide with
// an image/color import elsewhere in the package. The splash paints every
// cell's foreground and background itself, so its truecolor scene does not
// depend on the terminal palette.
func splashColor(rgb [3]uint8) chat.Color {
	return chat.ColorRGB(rgb[0], rgb[1], rgb[2])
}

// Dissolve is how far the dissolve into the dashboard has gone and the
// background it reveals.
type Dissolve struct {
	progress   float32
	background chat.Color
}

// Canvas is the splash's linear-light pixel buffer.
type Canvas struct {
	width  int
	height int
	pixels []Rgb
}

func newCanvas(width, height int) Canvas {
	return Canvas{width: width, height: height, pixels: make([]Rgb, width*height)}
}

func (c *Canvas) get(x, y int) Rgb {
	return c.pixels[y*c.width+x]
}

func (c *Canvas) set(x, y int, color Rgb) {
	c.pixels[y*c.width+x] = color
}

// add adds light at a pixel; positions off the canvas are ignored.
func (c *Canvas) add(x, y int, color Rgb) {
	if x < 0 || y < 0 || x >= c.width || y >= c.height {
		return
	}
	pixel := &c.pixels[y*c.width+x]
	for channel := range 3 {
		pixel[channel] += color[channel]
	}
}

// splat adds light at a sub-pixel position, shared among the four nearest
// pixels so moving points glide instead of jumping.
func (c *Canvas) splat(point [2]float32, color Rgb) {
	fx, fy := point[0]-0.5, point[1]-0.5
	x0, y0 := float32(math.Floor(float64(fx))), float32(math.Floor(float64(fy)))
	dx, dy := fx-x0, fy-y0
	ix, iy := int(x0), int(y0)
	for _, share := range [4]struct {
		ox, oy int
		weight float32
	}{
		{0, 0, (1.0 - dx) * (1.0 - dy)},
		{1, 0, dx * (1.0 - dy)},
		{0, 1, (1.0 - dx) * dy},
		{1, 1, dx * dy},
	} {
		c.add(ix+share.ox, iy+share.oy, scaled(color, share.weight))
	}
}

func (c *Canvas) addAll(color Rgb) {
	for index := range c.pixels {
		for channel := range 3 {
			c.pixels[index][channel] += color[channel]
		}
	}
}

// display tone-maps and dithers one pixel to display bytes.
func (c *Canvas) display(x, y int, boost float32) [3]uint8 {
	pixel := c.get(x, y)
	threshold := bayer(x, y)
	var out [3]uint8
	for channel, value := range pixel {
		value = max(value*boost, 0.0)
		// Narkowicz's ACES fit: bright light rolls off instead of clipping.
		mapped := (value * (2.51*value + 0.03)) / (value*(2.43*value+0.59) + 0.14)
		level := float32(math.Floor(float64(min(max(mapped, 0.0), 1.0)*255.0/quantum+threshold))) * quantum
		out[channel] = uint8(min(level, 255.0))
	}
	return out
}

// blit writes the canvas into area, one cell per pixel pair. During the
// dissolve, cells give way to the background in dithered order and glow
// briefly just before they go.
func (c *Canvas) blit(area uv.Rectangle, buf *uv.Buffer, dissolve *Dissolve) {
	for row := range area.Dy() {
		for column := range area.Dx() {
			cell := uv.EmptyCell
			var boost float32 = 1.0
			if dissolve != nil {
				remaining := bayer(column, row) - dissolve.progress
				if remaining < 0.0 {
					chat.Style{Bg: dissolve.background}.ApplyTo(&cell.Style)
					buf.SetCell(area.Min.X+column, area.Min.Y+row, &cell)
					continue
				}
				boost += 2.5 * max(1.0-remaining/0.15, 0.0)
			}
			top := c.display(column, row*2, boost)
			bottom := c.display(column, row*2+1, boost)
			if top == bottom {
				chat.Style{Bg: splashColor(bottom)}.ApplyTo(&cell.Style)
			} else {
				cell.Content = "▀"
				chat.Style{Fg: splashColor(top), Bg: splashColor(bottom)}.ApplyTo(&cell.Style)
			}
			buf.SetCell(area.Min.X+column, area.Min.Y+row, &cell)
		}
	}
}

// Field is a scalar light field with max blending, for strokes that cross
// themselves (lightning, cracks) without bright knots at the joints.
type Field struct {
	width  int
	height int
	values []float32
}

func newField(width, height int) Field {
	return Field{width: width, height: height, values: make([]float32, width*height)}
}

func (f *Field) get(x, y int) float32 {
	return f.values[y*f.width+x]
}

// segment draws a glowing segment between two pixel positions: a solid
// core of core pixels and an exponential halo of glow pixels.
func (f *Field) segment(a, b [2]float32, core, glow, intensity float32) {
	reach := core + glow*3.0
	left := asUsize(max(splashFloor(min(a[0], b[0])-reach), 0.0))
	top := asUsize(max(splashFloor(min(a[1], b[1])-reach), 0.0))
	right := min(asUsize(max(splashCeil(max(a[0], b[0])+reach), 0.0)), f.width)
	bottom := min(asUsize(max(splashCeil(max(a[1], b[1])+reach), 0.0)), f.height)
	dx, dy := b[0]-a[0], b[1]-a[1]
	lengthSquared := max(dx*dx+dy*dy, 1e-6)
	for y := top; y < bottom; y++ {
		for x := left; x < right; x++ {
			px, py := float32(x)+0.5-a[0], float32(y)+0.5-a[1]
			along := splashClamp((px*dx+py*dy)/lengthSquared, 0.0, 1.0)
			distance := splashHypot(px-dx*along, py-dy*along)
			value := intensity *
				(smoothstep(core+0.8, core*0.5, distance) +
					0.45*splashExp(-distance/glow))
			slot := &f.values[y*f.width+x]
			*slot = max(*slot, value)
		}
	}
}
