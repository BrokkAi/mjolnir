// Port of vt100 0.16.2 src/attrs.rs

package vt100

import "github.com/BrokkAi/mjolnir/go/rs"

// Represents a foreground or background color for cells.
type Color interface{ isColor() }

// The default terminal color.
type Color_Default struct{}

// An indexed terminal color.
type Color_Idx struct{ F0 uint8 }

// An RGB terminal color. The parameters are (red, green, blue).
type Color_Rgb struct {
	F0 uint8
	F1 uint8
	F2 uint8
}

func (*Color_Default) isColor() {}
func (*Color_Idx) isColor()     {}
func (*Color_Rgb) isColor()     {}

var _ = rs.RegisterEnum[Color]("Color",
	rs.Variant[Color_Default]("Default", rs.ShapeUnit),
	rs.Variant[Color_Idx]("Idx", rs.ShapeTuple),
	rs.Variant[Color_Rgb]("Rgb", rs.ShapeTuple),
)

// ColorDefault is `Color::default()`: the default terminal color.
func ColorDefault() Color {
	return &Color_Default{}
}

// color is a Color as the screen stores it. Color is Copy in Rust and every
// cell holds two; storing the variant by value keeps a cell the 32 bytes it
// is in Rust, lets cells and attributes be compared with ==, and means a
// Color handed to a caller never aliases a cell.
type color struct {
	// tag is the variant: colorDefault, colorIdx or colorRgb.
	tag uint8
	// r is the index of an indexed color and the red of an RGB one.
	r uint8
	g uint8
	b uint8
}

const (
	colorDefault uint8 = iota
	colorIdx
	colorRgb
)

func colorOfIdx(i uint8) color {
	return color{tag: colorIdx, r: i, g: 0, b: 0}
}

func colorOfRgb(r, g, b uint8) color {
	return color{tag: colorRgb, r: r, g: g, b: b}
}

// colorFrom is the stored form of a Color.
func colorFrom(c Color) color {
	switch c := c.(type) {
	case *Color_Default:
		return color{}
	case *Color_Idx:
		return colorOfIdx(c.F0)
	case *Color_Rgb:
		return colorOfRgb(c.F0, c.F1, c.F2)
	default:
		panic(rs.Unreachable(c))
	}
}

// color returns the Color a caller sees: a new value each time.
func (self color) color() Color {
	switch self.tag {
	case colorIdx:
		return &Color_Idx{F0: self.r}
	case colorRgb:
		return &Color_Rgb{F0: self.r, F1: self.g, F2: self.b}
	default:
		return &Color_Default{}
	}
}

// FmtDebug prints the stored color as the Color it stands for, which is what
// the derived Debug of Attrs prints in Rust.
func (self color) FmtDebug(f *rs.Formatter) error {
	return rs.DebugFmt(self.color(), f)
}

const textModeIntensity uint8 = 0b0000_0011
const textModeBold uint8 = 0b0000_0001
const textModeDim uint8 = 0b0000_0010
const textModeItalic uint8 = 0b0000_0100
const textModeUnderline uint8 = 0b0000_1000
const textModeInverse uint8 = 0b0001_0000

type attrsAttrs struct {
	fgcolor color `rs:"fgcolor"`
	bgcolor color `rs:"bgcolor"`
	mode    uint8 `rs:"mode"`
}

var _ = rs.RegisterStruct[attrsAttrs]("Attrs")

func (self attrsAttrs) bold() bool {
	return self.mode&textModeBold != 0
}

func (self attrsAttrs) dim() bool {
	return self.mode&textModeDim != 0
}

func (self attrsAttrs) intensity() uint8 {
	return self.mode & textModeIntensity
}

func (self *attrsAttrs) setBold() {
	self.mode &= ^textModeIntensity
	self.mode |= textModeBold
}

func (self *attrsAttrs) setDim() {
	self.mode &= ^textModeIntensity
	self.mode |= textModeDim
}

func (self *attrsAttrs) setNormalIntensity() {
	self.mode &= ^textModeIntensity
}

func (self attrsAttrs) italic() bool {
	return self.mode&textModeItalic != 0
}

func (self *attrsAttrs) setItalic(italic bool) {
	if italic {
		self.mode |= textModeItalic
	} else {
		self.mode &= ^textModeItalic
	}
}

func (self attrsAttrs) underline() bool {
	return self.mode&textModeUnderline != 0
}

func (self *attrsAttrs) setUnderline(underline bool) {
	if underline {
		self.mode |= textModeUnderline
	} else {
		self.mode &= ^textModeUnderline
	}
}

func (self attrsAttrs) inverse() bool {
	return self.mode&textModeInverse != 0
}

func (self *attrsAttrs) setInverse(inverse bool) {
	if inverse {
		self.mode |= textModeInverse
	} else {
		self.mode &= ^textModeInverse
	}
}

func (self attrsAttrs) writeEscapeCodeDiff(contents *[]uint8, other *attrsAttrs) {
	if self != *other && self == (attrsAttrs{}) {
		termClearAttrs{}.writeBuf(contents)
		return
	}

	attrs := termAttrs{}

	if self.fgcolor != other.fgcolor {
		attrs = attrs.fgcolorMethod(self.fgcolor)
	}
	if self.bgcolor != other.bgcolor {
		attrs = attrs.bgcolorMethod(self.bgcolor)
	}
	if self.intensity() != other.intensity() {
		var intensity termIntensity
		switch self.intensity() {
		case 0:
			intensity = termIntensity_Normal
		case textModeBold:
			intensity = termIntensity_Bold
		case textModeDim:
			intensity = termIntensity_Dim
		default:
			panic(rs.Unreachable())
		}
		attrs = attrs.intensityMethod(intensity)
	}
	if self.italic() != other.italic() {
		attrs = attrs.italicMethod(self.italic())
	}
	if self.underline() != other.underline() {
		attrs = attrs.underlineMethod(self.underline())
	}
	if self.inverse() != other.inverse() {
		attrs = attrs.inverseMethod(self.inverse())
	}

	attrs.writeBuf(contents)
}
