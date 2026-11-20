// Port of qrcode 0.14.1 src/lib.rs

package qrcode

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// QrCode is qrcode::QrCode: the encoded QR code symbol.
type QrCode struct {
	content []Color
	version Version
	ecLevel EcLevel
	width   int
}

// QrCodeNew is QrCode::new: constructs a new QR code which automatically
// encodes the given data.
//
// This method uses the "medium" error correction level and automatically
// chooses the smallest QR code.
//
//	code, err := QrCodeNew([]byte("Some data"))
//
// # Errors
//
// Returns error if the QR code cannot be constructed, e.g. when the data is
// too long.
func QrCodeNew(data []byte) (*QrCode, error) {
	return QrCodeWithErrorCorrectionLevel(data, EcLevel_M)
}

// QrCodeWithErrorCorrectionLevel is QrCode::with_error_correction_level:
// constructs a new QR code which automatically encodes the given data at a
// specific error correction level.
//
// This method automatically chooses the smallest QR code.
//
//	code, err := QrCodeWithErrorCorrectionLevel([]byte("Some data"), EcLevel_H)
//
// # Errors
//
// Returns error if the QR code cannot be constructed, e.g. when the data is
// too long.
func QrCodeWithErrorCorrectionLevel(data []byte, ecLevel EcLevel) (*QrCode, error) {
	bits, err := BitsEncodeAuto(data, ecLevel)
	if err != nil {
		return nil, err
	}
	return QrCodeWithBits(bits, ecLevel)
}

// QrCodeWithVersion is QrCode::with_version: constructs a new QR code for
// the given version and error correction level.
//
//	code, err := QrCodeWithVersion([]byte("Some data"), &Version_Normal{F0: 5}, EcLevel_M)
//
// This method can also be used to generate Micro QR code.
//
//	microCode, err := QrCodeWithVersion([]byte("123"), &Version_Micro{F0: 1}, EcLevel_L)
//
// # Errors
//
// Returns error if the QR code cannot be constructed, e.g. when the data is
// too long, or when the version and error correction level are incompatible.
func QrCodeWithVersion(data []byte, version Version, ecLevel EcLevel) (*QrCode, error) {
	bits := BitsBitsNew(version)
	if err := bits.PushOptimalData(data); err != nil {
		return nil, err
	}
	if err := bits.PushTerminator(ecLevel); err != nil {
		return nil, err
	}
	return QrCodeWithBits(bits, ecLevel)
}

// QrCodeWithBits is QrCode::with_bits: constructs a new QR code with encoded
// bits.
//
// Use this method only if there are very special need to manipulate the raw
// bits before encoding. Some examples are:
//
//   - Encode data using specific character set with ECI
//   - Use the FNC1 modes
//   - Avoid the optimal segmentation algorithm
//
// See the `BitsBits` structure for detail.
//
//	bits := BitsBitsNew(&Version_Normal{F0: 1})
//	bits.PushEciDesignator(9)
//	bits.PushByteData([]byte("\xca\xfe\xe4\xe9\xea\xe1\xf2 QR"))
//	bits.PushTerminator(EcLevel_L)
//	qrcode, err := QrCodeWithBits(bits, EcLevel_L)
//
// # Errors
//
// Returns error if the QR code cannot be constructed, e.g. when the bits are
// too long, or when the version and error correction level are incompatible.
func QrCodeWithBits(bits *BitsBits, ecLevel EcLevel) (*QrCode, error) {
	version := bits.Version()
	data := bits.IntoBytes()
	encodedData, ecData, err := EcConstructCodewords(data, version, ecLevel)
	if err != nil {
		return nil, err
	}
	canvas := CanvasCanvasNew(version, ecLevel)
	canvas.DrawAllFunctionalPatterns()
	canvas.DrawData(encodedData, ecData)
	canvas = canvas.ApplyBestMask()
	return &QrCode{content: canvas.IntoColors(), version: version, ecLevel: ecLevel, width: asUsizeI16(version.Width())}, nil
}

// Version is QrCode::version: gets the version of this QR code.
func (q *QrCode) Version() Version {
	return q.version
}

// ErrorCorrectionLevel is QrCode::error_correction_level: gets the error
// correction level of this QR code.
func (q *QrCode) ErrorCorrectionLevel() EcLevel {
	return q.ecLevel
}

// Width is QrCode::width: gets the number of modules per side, i.e. the
// width of this QR code.
//
// The width here does not contain the quiet zone paddings.
func (q *QrCode) Width() int {
	return q.width
}

// MaxAllowedErrors is QrCode::max_allowed_errors: gets the maximum number of
// allowed erratic modules can be introduced before the data becomes
// corrupted. Note that errors should not be introduced to functional
// modules.
func (q *QrCode) MaxAllowedErrors() int {
	errors, err := EcMaxAllowedErrors(q.version, q.ecLevel)
	if err != nil {
		rs.PanicAt(1, "invalid version or ec_level: "+rs.DebugString(err))
	}
	return errors
}

// IsFunctional is QrCode::is_functional: checks whether a module at
// coordinate (x, y) is a functional module or not.
//
// # Panics
//
// Panics if `x` or `y` is beyond the size of the QR code.
func (q *QrCode) IsFunctional(x int, y int) bool {
	if x < 0 || x > 0x7fff || y < 0 || y > 0x7fff {
		rs.PanicAt(1, "coordinate is too large for QR code: TryFromIntError(())")
	}
	return CanvasIsFunctional(q.version, q.version.Width(), int16(x), int16(y))
}

// ToDebugStr is QrCode::to_debug_str: converts the QR code into a
// human-readable string. This is mainly for debugging only.
func (q *QrCode) ToDebugStr(onChar rune, offChar rune) string {
	renderer := QrCodeRender[rune](q)
	return renderer.QuietZone(false).DarkColor(onChar).LightColor(offChar).Build()
}

// ToVec is QrCode::to_vec: converts the QR code to a vector of booleans.
// Each entry represents the color of the module, with "true" means dark and
// "false" means light.
//
// Deprecated: since qrcode 0.4.0, use ToColors.
func (q *QrCode) ToVec() []bool {
	bools := make([]bool, len(q.content))
	for i, c := range q.content {
		bools[i] = c != Color_Light
	}
	return bools
}

// IntoVec is QrCode::into_vec: converts the QR code to a vector of booleans.
// Each entry represents the color of the module, with "true" means dark and
// "false" means light.
//
// Deprecated: since qrcode 0.4.0, use IntoColors.
func (q *QrCode) IntoVec() []bool {
	return q.ToVec()
}

// ToColors is QrCode::to_colors: converts the QR code to a vector of colors.
func (q *QrCode) ToColors() []Color {
	colors := make([]Color, len(q.content))
	copy(colors, q.content)
	return colors
}

// IntoColors is QrCode::into_colors: converts the QR code to a vector of
// colors.
func (q *QrCode) IntoColors() []Color {
	return q.content
}

// QrCodeRender is QrCode::render: renders the QR code into a string. The
// result is a renderer builder, which you may do some additional
// configuration before building the text. The pixel type is a rune (`char`)
// or a string (`&str`); the image, svg and pic pixels are not covered.
func QrCodeRender[P RenderStringElement](q *QrCode) *RenderRenderer[P] {
	quietZone := uint32(4)
	if q.version.IsMicro() {
		quietZone = 2
	}
	return RenderRendererNew[P](q.content, q.width, quietZone)
}

// Index is `impl Index<(usize, usize)> for QrCode`: `qr[(x, y)]`, the color
// of the module at (x, y).
func (q *QrCode) Index(x int, y int) Color {
	index := y*q.width + x
	if index < 0 || index >= len(q.content) {
		rs.PanicAt(1, "index out of bounds: the len is "+strconv.Itoa(len(q.content))+" but the index is "+strconv.Itoa(index))
	}
	return q.content[index]
}
