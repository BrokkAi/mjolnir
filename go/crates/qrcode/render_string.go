// Port of qrcode 0.14.1 src/render/string.rs

package qrcode

import (
	"strings"
	"unicode/utf8"
)

// String rendering support.

// RenderStringElement is qrcode::render::string::Element: the two pixel
// types a string renders from, `char` (a rune) and `&str` (a string).
type RenderStringElement interface {
	rune | string
}

// renderStringDefaultColor is Element::default_color.
func renderStringDefaultColor[P RenderStringElement](color Color) P {
	var pixel P
	switch any(pixel).(type) {
	case rune:
		return P(ColorSelect[rune](color, '█', ' '))
	default:
		return renderStringPixelFromString[P](ColorSelect(color, "█", " "))
	}
}

// renderStringPixelFromString makes a string pixel of P from s.
func renderStringPixelFromString[P RenderStringElement](s string) P {
	var pixel P
	p := any(&pixel)
	*(p.(*string)) = s
	return pixel
}

// renderStringStrlen is Element::strlen: `len_utf8` of a char, `len` of a
// str.
func renderStringStrlen[P RenderStringElement](pixel P) int {
	switch v := any(pixel).(type) {
	case rune:
		return utf8.RuneLen(v)
	case string:
		return len(v)
	}
	panic("unreachable pixel type")
}

// renderStringAppend is Element::append_to_string.
func renderStringAppend[P RenderStringElement](pixel P, out *strings.Builder) {
	switch v := any(pixel).(type) {
	case rune:
		out.WriteRune(v)
	case string:
		out.WriteString(v)
	}
}

// renderStringDefaultUnitSize is `Pixel::default_unit_size` of an Element.
func renderStringDefaultUnitSize() (uint32, uint32) {
	return 1, 1
}

// renderStringCanvas is qrcode::render::string::Canvas.
type renderStringCanvas[P RenderStringElement] struct {
	buffer     []P
	width      int
	darkPixel  P
	darkCapInc int
	capacity   int
}

func renderStringCanvasNew[P RenderStringElement](width uint32, height uint32, darkPixel P, lightPixel P) *renderStringCanvas[P] {
	widthUsize := asUsizeU32(width)
	heightIsize := asIsizeU32(height)
	darkCap := asIsizeUsize(renderStringStrlen(darkPixel))
	lightCap := asIsizeUsize(renderStringStrlen(lightPixel))
	buffer := make([]P, widthUsize*asUsizeIsize(heightIsize))
	for i := range buffer {
		buffer[i] = lightPixel
	}
	return &renderStringCanvas[P]{
		buffer:     buffer,
		width:      widthUsize,
		darkPixel:  darkPixel,
		darkCapInc: darkCap - lightCap,
		capacity:   lightCap*asIsizeUsize(widthUsize)*heightIsize + (heightIsize - 1),
	}
}

func (c *renderStringCanvas[P]) drawDarkPixel(x uint32, y uint32) {
	ux := asUsizeU32(x)
	uy := asUsizeU32(y)
	c.capacity += c.darkCapInc
	c.buffer[ux+uy*c.width] = c.darkPixel
}

// drawDarkRect is the default `Canvas::draw_dark_rect` of render/mod.rs.
func (c *renderStringCanvas[P]) drawDarkRect(left uint32, top uint32, width uint32, height uint32) {
	for y := top; y < top+height; y++ {
		for x := left; x < left+width; x++ {
			c.drawDarkPixel(x, y)
		}
	}
}

func (c *renderStringCanvas[P]) intoImage() string {
	var result strings.Builder
	if c.capacity > 0 {
		result.Grow(c.capacity)
	}
	for i, pixel := range c.buffer {
		if i != 0 && i%c.width == 0 {
			result.WriteByte('\n')
		}
		renderStringAppend(pixel, &result)
	}
	return result.String()
}
