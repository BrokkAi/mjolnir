// Port of qrcode 0.14.1 src/canvas.rs

package qrcode

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The `canvas` module puts raw bits into the QR code canvas.
//
//	c := CanvasCanvasNew(&Version_Normal{F0: 1}, EcLevel_L)
//	c.DrawAllFunctionalPatterns()
//	c.DrawData([]byte("data_here"), []byte("ec_code_here"))
//	c.ApplyMask(CanvasMaskPattern_Checkerboard)
//	bools := c.ToBools()

//------------------------------------------------------------------------------
//{{{ Modules

// CanvasModule is qrcode::canvas::Module: the color of a module (pixel) in
// the QR code. A value is a pointer to one of the variant structs; compare
// two with Equal (the derived PartialEq).
type CanvasModule interface {
	isCanvasModule()

	// IsDark is Module::is_dark: checks whether a module is dark.
	IsDark() bool

	// Mask is Module::mask: apply a mask to the unmasked modules.
	//
	//	Unmasked(Light).Mask(true) == Masked(Dark)
	//	Unmasked(Dark).Mask(true) == Masked(Light)
	//	Unmasked(Light).Mask(false) == Masked(Light)
	//	Masked(Dark).Mask(true) == Masked(Dark)
	//	Masked(Dark).Mask(false) == Masked(Dark)
	Mask(shouldInvert bool) CanvasModule

	// Equal is the derived PartialEq.
	Equal(other CanvasModule) bool
}

// CanvasModule_Empty is Module::Empty: the module is empty.
type CanvasModule_Empty struct{}

// CanvasModule_Masked is Module::Masked: the module is of functional
// patterns which cannot be masked, or pixels which have been masked.
type CanvasModule_Masked struct{ F0 Color }

// CanvasModule_Unmasked is Module::Unmasked: the module is of data and error
// correction bits before masking.
type CanvasModule_Unmasked struct{ F0 Color }

var _ = rs.RegisterEnum[CanvasModule]("Module",
	rs.Variant[CanvasModule_Empty]("Empty", rs.ShapeUnit),
	rs.Variant[CanvasModule_Masked]("Masked", rs.ShapeTuple),
	rs.Variant[CanvasModule_Unmasked]("Unmasked", rs.ShapeTuple),
)

func (*CanvasModule_Empty) isCanvasModule()    {}
func (*CanvasModule_Masked) isCanvasModule()   {}
func (*CanvasModule_Unmasked) isCanvasModule() {}

// The canvas holds these shared values rather than allocating a variant for
// every module. A Module is Copy in Rust and never changed in place, so the
// sharing cannot be observed.
var (
	canvasModuleEmpty         CanvasModule = &CanvasModule_Empty{}
	canvasModuleMaskedLight   CanvasModule = &CanvasModule_Masked{F0: Color_Light}
	canvasModuleMaskedDark    CanvasModule = &CanvasModule_Masked{F0: Color_Dark}
	canvasModuleUnmaskedLight CanvasModule = &CanvasModule_Unmasked{F0: Color_Light}
	canvasModuleUnmaskedDark  CanvasModule = &CanvasModule_Unmasked{F0: Color_Dark}
)

// canvasMasked is `Module::Masked(color)`.
func canvasMasked(color Color) CanvasModule {
	if color == Color_Dark {
		return canvasModuleMaskedDark
	}
	return canvasModuleMaskedLight
}

// canvasUnmasked is `Module::Unmasked(color)`.
func canvasUnmasked(color Color) CanvasModule {
	if color == Color_Dark {
		return canvasModuleUnmaskedDark
	}
	return canvasModuleUnmaskedLight
}

// ColorFromCanvasModule is `impl From<Module> for Color`.
func ColorFromCanvasModule(module CanvasModule) Color {
	switch m := module.(type) {
	case *CanvasModule_Empty:
		return Color_Light
	case *CanvasModule_Masked:
		return m.F0
	case *CanvasModule_Unmasked:
		return m.F0
	default:
		panic(rs.Unreachable(module))
	}
}

func (m *CanvasModule_Empty) IsDark() bool    { return ColorFromCanvasModule(m) == Color_Dark }
func (m *CanvasModule_Masked) IsDark() bool   { return ColorFromCanvasModule(m) == Color_Dark }
func (m *CanvasModule_Unmasked) IsDark() bool { return ColorFromCanvasModule(m) == Color_Dark }

func (*CanvasModule_Empty) Mask(shouldInvert bool) CanvasModule {
	if shouldInvert {
		return canvasModuleMaskedDark
	}
	return canvasModuleMaskedLight
}

func (m *CanvasModule_Unmasked) Mask(shouldInvert bool) CanvasModule {
	if shouldInvert {
		return canvasMasked(m.F0.Not())
	}
	return canvasMasked(m.F0)
}

func (m *CanvasModule_Masked) Mask(bool) CanvasModule {
	return canvasMasked(m.F0)
}

func (*CanvasModule_Empty) Equal(other CanvasModule) bool {
	_, ok := other.(*CanvasModule_Empty)
	return ok
}

func (m *CanvasModule_Masked) Equal(other CanvasModule) bool {
	o, ok := other.(*CanvasModule_Masked)
	return ok && m.F0 == o.F0
}

func (m *CanvasModule_Unmasked) Equal(other CanvasModule) bool {
	o, ok := other.(*CanvasModule_Unmasked)
	return ok && m.F0 == o.F0
}

//}}}
//------------------------------------------------------------------------------
//{{{ Canvas

// CanvasCanvas is qrcode::canvas::Canvas: `Canvas` is an intermediate helper
// structure to render error-corrected data into a QR code.
type CanvasCanvas struct {
	// The width and height of the canvas (cached as it is needed frequently).
	width int16

	// The version of the QR code.
	version Version

	// The error correction level of the QR code.
	ecLevel EcLevel

	// The modules of the QR code. Modules are arranged in left-to-right, then
	// top-to-bottom order.
	modules []CanvasModule
}

// CanvasCanvasNew is Canvas::new: constructs a new canvas big enough for a
// QR code of the given version.
func CanvasCanvasNew(version Version, ecLevel EcLevel) *CanvasCanvas {
	width := version.Width()
	modules := make([]CanvasModule, asUsizeI16(width*width))
	for i := range modules {
		modules[i] = canvasModuleEmpty
	}
	return &CanvasCanvas{width: width, version: version, ecLevel: ecLevel, modules: modules}
}

// Clone is the derived Clone. The modules are shared values (see
// canvasMasked), so copying the slice copies the canvas.
func (c *CanvasCanvas) Clone() *CanvasCanvas {
	modules := make([]CanvasModule, len(c.modules))
	copy(modules, c.modules)
	return &CanvasCanvas{width: c.width, version: c.version, ecLevel: c.ecLevel, modules: modules}
}

func (c *CanvasCanvas) coordsToIndex(x int16, y int16) int {
	if x < 0 {
		x += c.width
	}
	if y < 0 {
		y += c.width
	}
	ux := asUsizeI16(x)
	uy := asUsizeI16(y)
	index := uy*asUsizeI16(c.width) + ux
	if index < 0 || index >= len(c.modules) {
		rs.PanicAt(2, "index out of bounds: the len is "+strconv.Itoa(len(c.modules))+" but the index is "+strconv.Itoa(index))
	}
	return index
}

// Get is Canvas::get: obtains a module at the given coordinates. For
// convenience, negative coordinates will wrap around.
func (c *CanvasCanvas) Get(x int16, y int16) CanvasModule {
	return c.modules[c.coordsToIndex(x, y)]
}

// GetMut is Canvas::get_mut: obtains a mutable module at the given
// coordinates. For convenience, negative coordinates will wrap around.
func (c *CanvasCanvas) GetMut(x int16, y int16) *CanvasModule {
	index := c.coordsToIndex(x, y)
	return &c.modules[index]
}

// Put is Canvas::put: sets the color of a functional module at the given
// coordinates. For convenience, negative coordinates will wrap around.
func (c *CanvasCanvas) Put(x int16, y int16, color Color) {
	*c.GetMut(x, y) = canvasMasked(color)
}

//}}}
//------------------------------------------------------------------------------
//{{{ Finder patterns

// drawFinderPatternAt draws a single finder pattern with the center at
// (x, y).
func (c *CanvasCanvas) drawFinderPatternAt(x int16, y int16) {
	dxLeft, dxRight := int16(-3), int16(4)
	if x < 0 {
		dxLeft, dxRight = -4, 3
	}
	dyTop, dyBottom := int16(-3), int16(4)
	if y < 0 {
		dyTop, dyBottom = -4, 3
	}
	for j := dyTop; j <= dyBottom; j++ {
		for i := dxLeft; i <= dxRight; i++ {
			var color Color
			switch {
			case i == 4 || i == -4 || j == 4 || j == -4:
				color = Color_Light
			case i == 3 || i == -3 || j == 3 || j == -3:
				color = Color_Dark
			case i == 2 || i == -2 || j == 2 || j == -2:
				color = Color_Light
			default:
				color = Color_Dark
			}
			c.Put(x+i, y+j, color)
		}
	}
}

// drawFinderPatterns draws the finder patterns.
//
// The finder patterns is are 7×7 square patterns appearing at the three
// corners of a QR code. They allows scanner to locate the QR code and
// determine the orientation.
func (c *CanvasCanvas) drawFinderPatterns() {
	c.drawFinderPatternAt(3, 3)

	switch c.version.(type) {
	case *Version_Micro:
	case *Version_Normal:
		c.drawFinderPatternAt(-4, 3)
		c.drawFinderPatternAt(3, -4)
	default:
		panic(rs.Unreachable(c.version))
	}
}

//}}}
//------------------------------------------------------------------------------
//{{{ Alignment patterns

// drawAlignmentPatternAt draws a alignment pattern with the center at
// (x, y).
func (c *CanvasCanvas) drawAlignmentPatternAt(x int16, y int16) {
	if !c.Get(x, y).Equal(canvasModuleEmpty) {
		return
	}
	for j := int16(-2); j <= 2; j++ {
		for i := int16(-2); i <= 2; i++ {
			color := Color_Light
			if i == 2 || i == -2 || j == 2 || j == -2 || (i == 0 && j == 0) {
				color = Color_Dark
			}
			c.Put(x+i, y+j, color)
		}
	}
}

// drawAlignmentPatterns draws the alignment patterns.
//
// The alignment patterns are 5×5 square patterns inside the QR code symbol
// to help the scanner create the square grid.
func (c *CanvasCanvas) drawAlignmentPatterns() {
	switch v := c.version.(type) {
	case *Version_Micro:
	case *Version_Normal:
		switch a := v.F0; {
		case a == 1:
		case 2 <= a && a <= 6:
			c.drawAlignmentPatternAt(-7, -7)
		default:
			positions := canvasAlignmentPatternPositions[asUsizeI16(a-7)]
			for _, x := range positions {
				for _, y := range positions {
					c.drawAlignmentPatternAt(x, y)
				}
			}
		}
	default:
		panic(rs.Unreachable(c.version))
	}
}

// canvasAlignmentPatternPositions is ALIGNMENT_PATTERN_POSITIONS:
// `ALIGNMENT_PATTERN_POSITIONS` describes the x- and y-coordinates of the
// center of the alignment patterns. Since the QR code is symmetric, only one
// coordinate is needed.
var canvasAlignmentPatternPositions = [34][]int16{
	{6, 22, 38},
	{6, 24, 42},
	{6, 26, 46},
	{6, 28, 50},
	{6, 30, 54},
	{6, 32, 58},
	{6, 34, 62},
	{6, 26, 46, 66},
	{6, 26, 48, 70},
	{6, 26, 50, 74},
	{6, 30, 54, 78},
	{6, 30, 56, 82},
	{6, 30, 58, 86},
	{6, 34, 62, 90},
	{6, 28, 50, 72, 94},
	{6, 26, 50, 74, 98},
	{6, 30, 54, 78, 102},
	{6, 28, 54, 80, 106},
	{6, 32, 58, 84, 110},
	{6, 30, 58, 86, 114},
	{6, 34, 62, 90, 118},
	{6, 26, 50, 74, 98, 122},
	{6, 30, 54, 78, 102, 126},
	{6, 26, 52, 78, 104, 130},
	{6, 30, 56, 82, 108, 134},
	{6, 34, 60, 86, 112, 138},
	{6, 30, 58, 86, 114, 142},
	{6, 34, 62, 90, 118, 146},
	{6, 30, 54, 78, 102, 126, 150},
	{6, 24, 50, 76, 102, 128, 154},
	{6, 28, 54, 80, 106, 132, 158},
	{6, 32, 58, 84, 110, 136, 162},
	{6, 26, 54, 82, 110, 138, 166},
	{6, 30, 58, 86, 114, 142, 170},
}

//}}}
//------------------------------------------------------------------------------
//{{{ Timing patterns

// drawLine draws a line from (x1, y1) to (x2, y2), inclusively.
//
// The line must be either horizontal or vertical, i.e. `x1 == x2 || y1 ==
// y2`. Additionally, the first coordinates must be less then the second
// ones.
//
// On even coordinates, `colorEven` will be plotted; on odd coordinates,
// `colorOdd` will be plotted instead. Thus the timing pattern can be drawn
// using this method.
func (c *CanvasCanvas) drawLine(x1 int16, y1 int16, x2 int16, y2 int16, colorEven Color, colorOdd Color) {
	if debugAssertions && !(x1 == x2 || y1 == y2) {
		rs.PanicAt(1, "assertion failed: x1 == x2 || y1 == y2")
	}

	if y1 == y2 {
		// Horizontal line.
		for x := x1; x <= x2; x++ {
			color := colorOdd
			if x%2 == 0 {
				color = colorEven
			}
			c.Put(x, y1, color)
		}
	} else {
		// Vertical line.
		for y := y1; y <= y2; y++ {
			color := colorOdd
			if y%2 == 0 {
				color = colorEven
			}
			c.Put(x1, y, color)
		}
	}
}

// drawTimingPatterns draws the timing patterns.
//
// The timing patterns are checkboard-colored lines near the edge of the QR
// code symbol, to establish the fine-grained module coordinates when
// scanning.
func (c *CanvasCanvas) drawTimingPatterns() {
	width := c.width
	var y, x1, x2 int16
	switch c.version.(type) {
	case *Version_Micro:
		y, x1, x2 = 0, 8, width-1
	case *Version_Normal:
		y, x1, x2 = 6, 8, width-9
	default:
		panic(rs.Unreachable(c.version))
	}
	c.drawLine(x1, y, x2, y, Color_Dark, Color_Light)
	c.drawLine(y, x1, y, x2, Color_Dark, Color_Light)
}

//}}}
//------------------------------------------------------------------------------
//{{{ Format info & Version info

// canvasCoord is an `(i16, i16)` coordinate of the tables below.
type canvasCoord struct{ x, y int16 }

// drawNumber draws a big-endian integer onto the canvas with the given
// coordinates.
//
// The 1 bits will be plotted with `onColor` and the 0 bits with `offColor`.
// The coordinates will be extracted from the `coords` iterator. It will
// start from the most significant bits first, so *trailing* zeros will be
// ignored.
func (c *CanvasCanvas) drawNumber(number uint32, bits uint32, onColor Color, offColor Color, coords []canvasCoord) {
	mask := uint32(1) << (bits - 1)
	for _, coord := range coords {
		color := onColor
		if mask&number == 0 {
			color = offColor
		}
		c.Put(coord.x, coord.y, color)
		mask >>= 1
	}
}

// drawFormatInfoPatternsWithNumber draws the format info patterns for an
// encoded number.
func (c *CanvasCanvas) drawFormatInfoPatternsWithNumber(formatInfo16 uint16) {
	formatInfo := uint32(formatInfo16)
	switch c.version.(type) {
	case *Version_Micro:
		c.drawNumber(formatInfo, 15, Color_Dark, Color_Light, canvasFormatInfoCoordsMicroQr[:])
	case *Version_Normal:
		c.drawNumber(formatInfo, 15, Color_Dark, Color_Light, canvasFormatInfoCoordsQrMain[:])
		c.drawNumber(formatInfo, 15, Color_Dark, Color_Light, canvasFormatInfoCoordsQrSide[:])
		c.Put(8, -8, Color_Dark) // Dark module.
	default:
		panic(rs.Unreachable(c.version))
	}
}

// drawReservedFormatInfoPatterns reserves area to put in the format
// information.
func (c *CanvasCanvas) drawReservedFormatInfoPatterns() {
	c.drawFormatInfoPatternsWithNumber(0)
}

// drawVersionInfoPatterns draws the version information patterns.
func (c *CanvasCanvas) drawVersionInfoPatterns() {
	switch v := c.version.(type) {
	case *Version_Micro:
	case *Version_Normal:
		if 1 <= v.F0 && v.F0 <= 6 {
			return
		}
		versionInfo := canvasVersionInfos[asUsizeI16(v.F0-7)]
		c.drawNumber(versionInfo, 18, Color_Dark, Color_Light, canvasVersionInfoCoordsBl[:])
		c.drawNumber(versionInfo, 18, Color_Dark, Color_Light, canvasVersionInfoCoordsTr[:])
	default:
		panic(rs.Unreachable(c.version))
	}
}

var canvasVersionInfoCoordsBl = [18]canvasCoord{
	{5, -9},
	{5, -10},
	{5, -11},
	{4, -9},
	{4, -10},
	{4, -11},
	{3, -9},
	{3, -10},
	{3, -11},
	{2, -9},
	{2, -10},
	{2, -11},
	{1, -9},
	{1, -10},
	{1, -11},
	{0, -9},
	{0, -10},
	{0, -11},
}

var canvasVersionInfoCoordsTr = [18]canvasCoord{
	{-9, 5},
	{-10, 5},
	{-11, 5},
	{-9, 4},
	{-10, 4},
	{-11, 4},
	{-9, 3},
	{-10, 3},
	{-11, 3},
	{-9, 2},
	{-10, 2},
	{-11, 2},
	{-9, 1},
	{-10, 1},
	{-11, 1},
	{-9, 0},
	{-10, 0},
	{-11, 0},
}

var canvasFormatInfoCoordsQrMain = [15]canvasCoord{
	{0, 8},
	{1, 8},
	{2, 8},
	{3, 8},
	{4, 8},
	{5, 8},
	{7, 8},
	{8, 8},
	{8, 7},
	{8, 5},
	{8, 4},
	{8, 3},
	{8, 2},
	{8, 1},
	{8, 0},
}

var canvasFormatInfoCoordsQrSide = [15]canvasCoord{
	{8, -1},
	{8, -2},
	{8, -3},
	{8, -4},
	{8, -5},
	{8, -6},
	{8, -7},
	{-8, 8},
	{-7, 8},
	{-6, 8},
	{-5, 8},
	{-4, 8},
	{-3, 8},
	{-2, 8},
	{-1, 8},
}

var canvasFormatInfoCoordsMicroQr = [15]canvasCoord{
	{1, 8},
	{2, 8},
	{3, 8},
	{4, 8},
	{5, 8},
	{6, 8},
	{7, 8},
	{8, 8},
	{8, 7},
	{8, 6},
	{8, 5},
	{8, 4},
	{8, 3},
	{8, 2},
	{8, 1},
}

var canvasVersionInfos = [34]uint32{
	0x07c94, 0x085bc, 0x09a99, 0x0a4d3, 0x0bbf6, 0x0c762, 0x0d847, 0x0e60d, 0x0f928, 0x10b78, 0x1145d, 0x12a17,
	0x13532, 0x149a6, 0x15683, 0x168c9, 0x177ec, 0x18ec4, 0x191e1, 0x1afab, 0x1b08e, 0x1cc1a, 0x1d33f, 0x1ed75,
	0x1f250, 0x209d5, 0x216f0, 0x228ba, 0x2379f, 0x24b0b, 0x2542e, 0x26a64, 0x27541, 0x28c69,
}

//}}}
//------------------------------------------------------------------------------
//{{{ All functional patterns before data placement

// DrawAllFunctionalPatterns is Canvas::draw_all_functional_patterns: draw
// all functional patterns, before data placement.
//
// All functional patterns (e.g. the finder pattern) *except* the format info
// pattern will be filled in. The format info pattern will be filled with
// light modules instead. Data bits can then put in the empty modules. with
// `.DrawData()`.
func (c *CanvasCanvas) DrawAllFunctionalPatterns() {
	c.drawFinderPatterns()
	c.drawAlignmentPatterns()
	c.drawReservedFormatInfoPatterns()
	c.drawTimingPatterns()
	c.drawVersionInfoPatterns()
}

// CanvasIsFunctional is canvas::is_functional: gets whether the module at
// the given coordinates represents a functional module.
func CanvasIsFunctional(version Version, width int16, x int16, y int16) bool {
	if debugAssertions && width != version.Width() {
		rs.PanicAt(1, "assertion failed: width == version.width()")
	}

	if x < 0 {
		x += width
	}
	if y < 0 {
		y += width
	}

	switch v := version.(type) {
	case *Version_Micro:
		return x == 0 || y == 0 || (x < 9 && y < 9)
	case *Version_Normal:
		a := v.F0
		nonAlignmentTest := x == 6 || y == 6 || // Timing patterns
			(x < 9 && y < 9) || // Top-left finder pattern
			(x < 9 && y >= width-8) || // Bottom-left finder pattern
			(x >= width-8 && y < 9) // Top-right finder pattern
		switch {
		case nonAlignmentTest:
			return true
		case a == 1:
			return false
		case 2 <= a && a <= 6:
			return abs16(width-7-x) <= 2 && abs16(width-7-y) <= 2
		default:
			positions := canvasAlignmentPatternPositions[asUsizeI16(a-7)]
			last := len(positions) - 1
			for i, alignX := range positions {
				for j, alignY := range positions {
					if i == 0 && (j == 0 || j == last) || (i == last && j == 0) {
						continue
					}
					if abs16(alignX-x) <= 2 && abs16(alignY-y) <= 2 {
						return true
					}
				}
			}
			return false
		}
	default:
		panic(rs.Unreachable(version))
	}
}

// abs16 is i16::abs.
func abs16(v int16) int16 {
	if v < 0 {
		if debugAssertions && v == -32768 {
			rs.PanicAt(2, "attempt to negate with overflow")
		}
		return -v
	}
	return v
}

//}}}
//------------------------------------------------------------------------------
//{{{ Data placement iterator

type canvasDataModuleIter struct {
	x                   int16
	y                   int16
	width               int16
	timingPatternColumn int16
}

func canvasDataModuleIterNew(version Version) *canvasDataModuleIter {
	width := version.Width()
	var timingPatternColumn int16
	switch version.(type) {
	case *Version_Micro:
		timingPatternColumn = 0
	case *Version_Normal:
		timingPatternColumn = 6
	default:
		panic(rs.Unreachable(version))
	}
	return &canvasDataModuleIter{
		x:                   width - 1,
		y:                   width - 1,
		width:               width,
		timingPatternColumn: timingPatternColumn,
	}
}

// next is Iterator::next: the next (x, y), or false at the end.
func (it *canvasDataModuleIter) next() (int16, int16, bool) {
	adjustedRefCol := it.x
	if it.x <= it.timingPatternColumn {
		adjustedRefCol = it.x + 1
	}
	if adjustedRefCol <= 0 {
		return 0, 0, false
	}

	resX, resY := it.x, it.y
	columnType := (it.width - adjustedRefCol) % 4

	switch {
	case columnType == 2 && it.y > 0:
		it.y--
		it.x++
	case columnType == 0 && it.y < it.width-1:
		it.y++
		it.x++
	case (columnType == 0 || columnType == 2) && it.x == it.timingPatternColumn+1:
		it.x -= 2
	default:
		it.x--
	}

	return resX, resY, true
}

//}}}
//------------------------------------------------------------------------------
//{{{ Data placement

func (c *CanvasCanvas) drawCodewords(codewords []byte, isHalfCodewordAtEnd bool, coords *canvasDataModuleIter) {
	length := len(codewords)
	lastWord := length
	if isHalfCodewordAtEnd {
		if debugAssertions && length == 0 {
			rs.PanicAt(1, panicSubOverflow)
		}
		lastWord = length - 1
	}
	for i, b := range codewords {
		bitsEnd := 0
		if i == lastWord {
			bitsEnd = 4
		}
	outside:
		for j := 7; j >= bitsEnd; j-- {
			color := Color_Dark
			if b&(1<<j) == 0 {
				color = Color_Light
			}
			for {
				x, y, ok := coords.next()
				if !ok {
					break
				}
				r := c.GetMut(x, y)
				if (*r).Equal(canvasModuleEmpty) {
					*r = canvasUnmasked(color)
					continue outside
				}
			}
			return
		}
	}
}

// DrawData is Canvas::draw_data: draws the encoded data and error correction
// codes to the empty modules.
func (c *CanvasCanvas) DrawData(data []byte, ec []byte) {
	isHalfCodewordAtEnd := false
	if micro, ok := c.version.(*Version_Micro); ok {
		isHalfCodewordAtEnd = (micro.F0 == 1 && c.ecLevel == EcLevel_L) || (micro.F0 == 3 && c.ecLevel == EcLevel_M)
	}
	coords := canvasDataModuleIterNew(c.version)
	c.drawCodewords(data, isHalfCodewordAtEnd, coords)
	c.drawCodewords(ec, false, coords)
}

//}}}
//------------------------------------------------------------------------------
//{{{ Masking

// CanvasMaskPattern is qrcode::canvas::MaskPattern: the mask patterns. Since
// QR code and Micro QR code do not use the same pattern number, we name them
// according to their shape instead of the number.
type CanvasMaskPattern uint8

const (
	// QR code pattern 000: `(x + y) % 2 == 0`.
	CanvasMaskPattern_Checkerboard CanvasMaskPattern = 0b000

	// QR code pattern 001: `y % 2 == 0`.
	CanvasMaskPattern_HorizontalLines CanvasMaskPattern = 0b001

	// QR code pattern 010: `x % 3 == 0`.
	CanvasMaskPattern_VerticalLines CanvasMaskPattern = 0b010

	// QR code pattern 011: `(x + y) % 3 == 0`.
	CanvasMaskPattern_DiagonalLines CanvasMaskPattern = 0b011

	// QR code pattern 100: `((x/3) + (y/2)) % 2 == 0`.
	CanvasMaskPattern_LargeCheckerboard CanvasMaskPattern = 0b100

	// QR code pattern 101: `(x*y)%2 + (x*y)%3 == 0`.
	CanvasMaskPattern_Fields CanvasMaskPattern = 0b101

	// QR code pattern 110: `((x*y)%2 + (x*y)%3) % 2 == 0`.
	CanvasMaskPattern_Diamonds CanvasMaskPattern = 0b110

	// QR code pattern 111: `((x+y)%2 + (x*y)%3) % 2 == 0`.
	CanvasMaskPattern_Meadow CanvasMaskPattern = 0b111
)

var _ = rs.RegisterEnum[CanvasMaskPattern]("MaskPattern",
	rs.Const(CanvasMaskPattern_Checkerboard, "Checkerboard"),
	rs.Const(CanvasMaskPattern_HorizontalLines, "HorizontalLines"),
	rs.Const(CanvasMaskPattern_VerticalLines, "VerticalLines"),
	rs.Const(CanvasMaskPattern_DiagonalLines, "DiagonalLines"),
	rs.Const(CanvasMaskPattern_LargeCheckerboard, "LargeCheckerboard"),
	rs.Const(CanvasMaskPattern_Fields, "Fields"),
	rs.Const(CanvasMaskPattern_Diamonds, "Diamonds"),
	rs.Const(CanvasMaskPattern_Meadow, "Meadow"),
)

// The mask functions (mod mask_functions). The products and sums are of
// coordinates below 177, which i16 holds.

func canvasMaskCheckerboard(x int16, y int16) bool {
	return (x+y)%2 == 0
}

func canvasMaskHorizontalLines(_ int16, y int16) bool {
	return y%2 == 0
}

func canvasMaskVerticalLines(x int16, _ int16) bool {
	return x%3 == 0
}

func canvasMaskDiagonalLines(x int16, y int16) bool {
	return (x+y)%3 == 0
}

func canvasMaskLargeCheckerboard(x int16, y int16) bool {
	return ((y/2)+(x/3))%2 == 0
}

func canvasMaskFields(x int16, y int16) bool {
	return int32(x)*int32(y)%2+int32(x)*int32(y)%3 == 0
}

func canvasMaskDiamonds(x int16, y int16) bool {
	return (int32(x)*int32(y)%2+int32(x)*int32(y)%3)%2 == 0
}

func canvasMaskMeadow(x int16, y int16) bool {
	return ((x+y)%2+int16(int32(x)*int32(y)%3))%2 == 0
}

func canvasGetMaskFunction(pattern CanvasMaskPattern) func(int16, int16) bool {
	switch pattern {
	case CanvasMaskPattern_Checkerboard:
		return canvasMaskCheckerboard
	case CanvasMaskPattern_HorizontalLines:
		return canvasMaskHorizontalLines
	case CanvasMaskPattern_VerticalLines:
		return canvasMaskVerticalLines
	case CanvasMaskPattern_DiagonalLines:
		return canvasMaskDiagonalLines
	case CanvasMaskPattern_LargeCheckerboard:
		return canvasMaskLargeCheckerboard
	case CanvasMaskPattern_Fields:
		return canvasMaskFields
	case CanvasMaskPattern_Diamonds:
		return canvasMaskDiamonds
	case CanvasMaskPattern_Meadow:
		return canvasMaskMeadow
	default:
		panic(rs.Unreachable(pattern))
	}
}

// ApplyMask is Canvas::apply_mask: applies a mask to the canvas. This method
// will also draw the format info patterns.
func (c *CanvasCanvas) ApplyMask(pattern CanvasMaskPattern) {
	maskFn := canvasGetMaskFunction(pattern)
	for x := int16(0); x < c.width; x++ {
		for y := int16(0); y < c.width; y++ {
			module := c.GetMut(x, y)
			*module = (*module).Mask(maskFn(x, y))
		}
	}

	c.drawFormatInfoPatterns(pattern)
}

// drawFormatInfoPatterns draws the format information to encode the error
// correction level and mask pattern.
//
// If the error correction level or mask pattern is not supported in the
// current QR code version, this method will fail.
func (c *CanvasCanvas) drawFormatInfoPatterns(pattern CanvasMaskPattern) {
	var formatNumber uint16
	switch v := c.version.(type) {
	case *Version_Normal:
		simpleFormatNumber := (int(c.ecLevel)^1)<<3 | int(pattern)
		formatNumber = canvasFormatInfosQr[simpleFormatNumber]
	case *Version_Micro:
		var microPatternNumber int
		switch pattern {
		case CanvasMaskPattern_HorizontalLines:
			microPatternNumber = 0b00
		case CanvasMaskPattern_LargeCheckerboard:
			microPatternNumber = 0b01
		case CanvasMaskPattern_Diamonds:
			microPatternNumber = 0b10
		case CanvasMaskPattern_Meadow:
			microPatternNumber = 0b11
		default:
			rs.PanicAt(1, "Unsupported mask pattern in Micro QR code")
		}
		var symbolNumber int
		switch {
		case v.F0 == 1 && c.ecLevel == EcLevel_L:
			symbolNumber = 0b000
		case v.F0 == 2 && c.ecLevel == EcLevel_L:
			symbolNumber = 0b001
		case v.F0 == 2 && c.ecLevel == EcLevel_M:
			symbolNumber = 0b010
		case v.F0 == 3 && c.ecLevel == EcLevel_L:
			symbolNumber = 0b011
		case v.F0 == 3 && c.ecLevel == EcLevel_M:
			symbolNumber = 0b100
		case v.F0 == 4 && c.ecLevel == EcLevel_L:
			symbolNumber = 0b101
		case v.F0 == 4 && c.ecLevel == EcLevel_M:
			symbolNumber = 0b110
		case v.F0 == 4 && c.ecLevel == EcLevel_Q:
			symbolNumber = 0b111
		default:
			rs.PanicAt(1, "Unsupported version/ec_level combination in Micro QR code")
		}
		simpleFormatNumber := symbolNumber<<2 | microPatternNumber
		formatNumber = canvasFormatInfosMicroQr[simpleFormatNumber]
	default:
		panic(rs.Unreachable(c.version))
	}
	c.drawFormatInfoPatternsWithNumber(formatNumber)
}

var canvasFormatInfosQr = [32]uint16{
	0x5412, 0x5125, 0x5e7c, 0x5b4b, 0x45f9, 0x40ce, 0x4f97, 0x4aa0, 0x77c4, 0x72f3, 0x7daa, 0x789d, 0x662f, 0x6318,
	0x6c41, 0x6976, 0x1689, 0x13be, 0x1ce7, 0x19d0, 0x0762, 0x0255, 0x0d0c, 0x083b, 0x355f, 0x3068, 0x3f31, 0x3a06,
	0x24b4, 0x2183, 0x2eda, 0x2bed,
}

var canvasFormatInfosMicroQr = [32]uint16{
	0x4445, 0x4172, 0x4e2b, 0x4b1c, 0x55ae, 0x5099, 0x5fc0, 0x5af7, 0x6793, 0x62a4, 0x6dfd, 0x68ca, 0x7678, 0x734f,
	0x7c16, 0x7921, 0x06de, 0x03e9, 0x0cb0, 0x0987, 0x1735, 0x1202, 0x1d5b, 0x186c, 0x2508, 0x203f, 0x2f66, 0x2a51,
	0x34e3, 0x31d4, 0x3e8d, 0x3bba,
}

//}}}
//------------------------------------------------------------------------------
//{{{ Penalty score

// computeAdjacentPenaltyScore computes the penalty score for having too many
// adjacent modules with the same color.
//
// Every 5+N adjacent modules in the same column/row having the same color
// will contribute 3+N points.
func (c *CanvasCanvas) computeAdjacentPenaltyScore(isHorizontal bool) uint16 {
	totalScore := uint16(0)

	for i := int16(0); i < c.width; i++ {
		mapFn := func(j int16) CanvasModule {
			if isHorizontal {
				return c.Get(j, i)
			}
			return c.Get(i, j)
		}

		lastColor := canvasModuleEmpty
		consecutiveLen := uint16(1)

		// The modules of the row or column, then one Module::Empty.
		for j := int16(0); j <= c.width; j++ {
			color := canvasModuleEmpty
			if j < c.width {
				color = mapFn(j)
			}
			if color.Equal(lastColor) {
				consecutiveLen = addU16(consecutiveLen, 1)
			} else {
				lastColor = color
				if consecutiveLen >= 5 {
					totalScore = addU16(totalScore, consecutiveLen-2)
				}
				consecutiveLen = 1
			}
		}
	}

	return totalScore
}

// computeBlockPenaltyScore computes the penalty score for having too many
// rectangles with the same color.
//
// Every 2×2 blocks (with overlapping counted) having the same color will
// contribute 3 points.
func (c *CanvasCanvas) computeBlockPenaltyScore() uint16 {
	totalScore := uint16(0)

	for i := int16(0); i < c.width-1; i++ {
		for j := int16(0); j < c.width-1; j++ {
			this := c.Get(i, j)
			right := c.Get(i+1, j)
			bottom := c.Get(i, j+1)
			bottomRight := c.Get(i+1, j+1)
			if this.Equal(right) && right.Equal(bottom) && bottom.Equal(bottomRight) {
				totalScore = addU16(totalScore, 3)
			}
		}
	}

	return totalScore
}

// canvasFinderPenaltyPattern is the PATTERN of compute_finder_penalty_score.
var canvasFinderPenaltyPattern = [7]Color{Color_Dark, Color_Light, Color_Dark, Color_Dark, Color_Dark, Color_Light, Color_Dark}

// computeFinderPenaltyScore computes the penalty score for having a pattern
// similar to the finder pattern in the wrong place.
//
// Every pattern that looks like `#.###.#....` in any orientation will add 40
// points.
func (c *CanvasCanvas) computeFinderPenaltyScore(isHorizontal bool) uint16 {
	totalScore := uint16(0)

	for i := int16(0); i < c.width; i++ {
		for j := int16(0); j < c.width-6; j++ {
			// TODO a ref to a closure should be enough?
			get := func(k int16) Color {
				if isHorizontal {
					return ColorFromCanvasModule(c.Get(k, i))
				}
				return ColorFromCanvasModule(c.Get(i, k))
			}

			matches := true
			for k := j; k < j+7; k++ {
				if get(k) != canvasFinderPenaltyPattern[k-j] {
					matches = false
					break
				}
			}
			if !matches {
				continue
			}

			check := func(k int16) bool { return 0 <= k && k < c.width && get(k) != Color_Light }
			anyLeft := false
			for k := j - 4; k < j; k++ {
				if check(k) {
					anyLeft = true
					break
				}
			}
			anyRight := false
			if anyLeft {
				for k := j + 7; k < j+11; k++ {
					if check(k) {
						anyRight = true
						break
					}
				}
			}
			if !anyLeft || !anyRight {
				totalScore = addU16(totalScore, 40)
			}
		}
	}

	return subU16(totalScore, 360)
}

// computeBalancePenaltyScore computes the penalty score for having an
// unbalanced dark/light ratio.
//
// The score is given linearly by the deviation from a 50% ratio of dark
// modules. The highest possible score is 100.
//
// Note that this algorithm differs slightly from the standard we do not
// round the result every 5%, but the difference should be negligible and
// should not affect which mask is chosen.
func (c *CanvasCanvas) computeBalancePenaltyScore() uint16 {
	darkModules := 0
	for _, m := range c.modules {
		if m.IsDark() {
			darkModules++
		}
	}
	totalModules := len(c.modules)
	ratio := darkModules * 200 / totalModules
	if ratio >= 100 {
		return asU16Usize(ratio - 100)
	}
	return asU16Usize(100 - ratio)
}

// computeLightSidePenaltyScore computes the penalty score for having too
// many light modules on the sides.
//
// This penalty score is exclusive to Micro QR code.
//
// Note that the standard gives the formula for *efficiency* score, which has
// the inverse meaning of this method, but it is very easy to convert between
// the two (this score is (16×width − standard-score)).
func (c *CanvasCanvas) computeLightSidePenaltyScore() uint16 {
	h := 0
	for j := int16(1); j < c.width; j++ {
		if !c.Get(j, -1).IsDark() {
			h++
		}
	}
	v := 0
	for j := int16(1); j < c.width; j++ {
		if !c.Get(-1, j).IsDark() {
			v++
		}
	}

	return asU16Usize(h + v + 15*max(h, v))
}

// computeTotalPenaltyScores computes the total penalty scores. A QR code
// having higher points is less desirable.
func (c *CanvasCanvas) computeTotalPenaltyScores() uint16 {
	switch c.version.(type) {
	case *Version_Normal:
		s1A := c.computeAdjacentPenaltyScore(true)
		s1B := c.computeAdjacentPenaltyScore(false)
		s2 := c.computeBlockPenaltyScore()
		s3A := c.computeFinderPenaltyScore(true)
		s3B := c.computeFinderPenaltyScore(false)
		s4 := c.computeBalancePenaltyScore()
		return addU16(addU16(addU16(addU16(addU16(s1A, s1B), s2), s3A), s3B), s4)
	case *Version_Micro:
		return c.computeLightSidePenaltyScore()
	default:
		panic(rs.Unreachable(c.version))
	}
}

//}}}
//------------------------------------------------------------------------------
//{{{ Select mask with lowest penalty score

var canvasAllPatternsQr = [8]CanvasMaskPattern{
	CanvasMaskPattern_Checkerboard,
	CanvasMaskPattern_HorizontalLines,
	CanvasMaskPattern_VerticalLines,
	CanvasMaskPattern_DiagonalLines,
	CanvasMaskPattern_LargeCheckerboard,
	CanvasMaskPattern_Fields,
	CanvasMaskPattern_Diamonds,
	CanvasMaskPattern_Meadow,
}

var canvasAllPatternsMicroQr = [4]CanvasMaskPattern{
	CanvasMaskPattern_HorizontalLines, CanvasMaskPattern_LargeCheckerboard, CanvasMaskPattern_Diamonds, CanvasMaskPattern_Meadow,
}

// ApplyBestMask is Canvas::apply_best_mask: construct a new canvas and apply
// the best masking that gives the lowest penalty score. Of patterns with the
// same score the first wins, as `min_by_key` keeps the first minimum.
func (c *CanvasCanvas) ApplyBestMask() *CanvasCanvas {
	var patterns []CanvasMaskPattern
	switch c.version.(type) {
	case *Version_Normal:
		patterns = canvasAllPatternsQr[:]
	case *Version_Micro:
		patterns = canvasAllPatternsMicroQr[:]
	default:
		panic(rs.Unreachable(c.version))
	}
	var best *CanvasCanvas
	bestScore := uint16(0)
	for _, ptn := range patterns {
		candidate := c.Clone()
		candidate.ApplyMask(ptn)
		score := candidate.computeTotalPenaltyScores()
		if best == nil || score < bestScore {
			best, bestScore = candidate, score
		}
	}
	if best == nil {
		rs.PanicAt(1, "at least one pattern")
	}
	return best
}

// ToBools is Canvas::to_bools: convert the modules into a vector of
// booleans.
//
// Deprecated: since qrcode 0.4.0, use IntoColors.
func (c *CanvasCanvas) ToBools() []bool {
	bools := make([]bool, len(c.modules))
	for i, m := range c.modules {
		bools[i] = m.IsDark()
	}
	return bools
}

// IntoColors is Canvas::into_colors: convert the modules into a vector of
// colors.
func (c *CanvasCanvas) IntoColors() []Color {
	colors := make([]Color, len(c.modules))
	for i, m := range c.modules {
		colors[i] = ColorFromCanvasModule(m)
	}
	return colors
}

//}}}
