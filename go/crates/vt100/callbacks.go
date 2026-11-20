// Port of vt100 0.16.2 src/callbacks.rs

package vt100

import "github.com/BrokkAi/mjolnir/go/rs"

// This trait is used by the parser to handle extra escape sequences that
// don't have an impact on the terminal screen directly.
//
// Every method has a default in Rust that does nothing. An implementation
// that relies on one calls the matching CallbacksDefault function from a
// one-line method.
//
// The byte and parameter slices a method receives belong to the parser and
// are only valid until the method returns, as the borrows are in Rust: copy
// what is kept.
type Callbacks interface {
	// This callback is called when the terminal requests an audible bell
	// (typically with `^G`).
	AudibleBell(screen *Screen)
	// This callback is called when the terminal requests a visual bell
	// (typically with `\eg`).
	VisualBell(screen *Screen)
	// This callback is called when the terminal requests a resize
	// (typically with `\e[8;<rows>;<cols>t`).
	Resize(screen *Screen, request rs.Tuple2[uint16, uint16])
	// This callback is called when the terminal requests the window title
	// to be set (typically with `\e]1;<icon_name>\a`)
	SetWindowIconName(screen *Screen, iconName []uint8)
	// This callback is called when the terminal requests the window title
	// to be set (typically with `\e]2;<title>\a`)
	SetWindowTitle(screen *Screen, title []uint8)
	// This callback is called when the terminal requests data to be copied
	// to the system clipboard (typically with `\e]52;<ty>;<data>\a`). Note
	// that `data` will be encoded as base64.
	CopyToClipboard(screen *Screen, ty []uint8, data []uint8)
	// This callback is called when the terminal requests data to be pasted
	// from the system clipboard (typically with `\e]52;<ty>;?\a`).
	PasteFromClipboard(screen *Screen, ty []uint8)
	// This callback is called when the terminal receives an escape sequence
	// which is otherwise not implemented.
	UnhandledChar(screen *Screen, c rune)
	// This callback is called when the terminal receives a control
	// character which is otherwise not implemented.
	UnhandledControl(screen *Screen, b uint8)
	// This callback is called when the terminal receives an escape sequence
	// which is otherwise not implemented.
	UnhandledEscape(screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], b uint8)
	// This callback is called when the terminal receives a CSI sequence
	// (`\e[`) which is otherwise not implemented.
	UnhandledCsi(screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], params [][]uint16, c rune)
	// This callback is called when the terminal receives a OSC sequence
	// (`\e]`) which is otherwise not implemented.
	UnhandledOsc(screen *Screen, params [][]uint8)
}

// CallbacksDefaultAudibleBell is the default body of Callbacks::audible_bell.
func CallbacksDefaultAudibleBell(self Callbacks, screen *Screen) {}

// CallbacksDefaultVisualBell is the default body of Callbacks::visual_bell.
func CallbacksDefaultVisualBell(self Callbacks, screen *Screen) {}

// CallbacksDefaultResize is the default body of Callbacks::resize.
func CallbacksDefaultResize(self Callbacks, screen *Screen, request rs.Tuple2[uint16, uint16]) {}

// CallbacksDefaultSetWindowIconName is the default body of
// Callbacks::set_window_icon_name.
func CallbacksDefaultSetWindowIconName(self Callbacks, screen *Screen, iconName []uint8) {}

// CallbacksDefaultSetWindowTitle is the default body of
// Callbacks::set_window_title.
func CallbacksDefaultSetWindowTitle(self Callbacks, screen *Screen, title []uint8) {}

// CallbacksDefaultCopyToClipboard is the default body of
// Callbacks::copy_to_clipboard.
func CallbacksDefaultCopyToClipboard(self Callbacks, screen *Screen, ty []uint8, data []uint8) {}

// CallbacksDefaultPasteFromClipboard is the default body of
// Callbacks::paste_from_clipboard.
func CallbacksDefaultPasteFromClipboard(self Callbacks, screen *Screen, ty []uint8) {}

// CallbacksDefaultUnhandledChar is the default body of
// Callbacks::unhandled_char.
func CallbacksDefaultUnhandledChar(self Callbacks, screen *Screen, c rune) {}

// CallbacksDefaultUnhandledControl is the default body of
// Callbacks::unhandled_control.
func CallbacksDefaultUnhandledControl(self Callbacks, screen *Screen, b uint8) {}

// CallbacksDefaultUnhandledEscape is the default body of
// Callbacks::unhandled_escape.
func CallbacksDefaultUnhandledEscape(self Callbacks, screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], b uint8) {
}

// CallbacksDefaultUnhandledCsi is the default body of
// Callbacks::unhandled_csi.
func CallbacksDefaultUnhandledCsi(self Callbacks, screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], params [][]uint16, c rune) {
}

// CallbacksDefaultUnhandledOsc is the default body of
// Callbacks::unhandled_osc.
func CallbacksDefaultUnhandledOsc(self Callbacks, screen *Screen, params [][]uint8) {}

// CallbacksUnit is `()` as callbacks, which is `impl Callbacks for () {}`:
// every event is ignored. It is the callbacks type of the parser that
// ParserNew returns, as `()` is the default of `Parser<CB>`.
type CallbacksUnit struct{}

func (self CallbacksUnit) AudibleBell(screen *Screen) { CallbacksDefaultAudibleBell(self, screen) }
func (self CallbacksUnit) VisualBell(screen *Screen)  { CallbacksDefaultVisualBell(self, screen) }
func (self CallbacksUnit) Resize(screen *Screen, request rs.Tuple2[uint16, uint16]) {
	CallbacksDefaultResize(self, screen, request)
}
func (self CallbacksUnit) SetWindowIconName(screen *Screen, iconName []uint8) {
	CallbacksDefaultSetWindowIconName(self, screen, iconName)
}
func (self CallbacksUnit) SetWindowTitle(screen *Screen, title []uint8) {
	CallbacksDefaultSetWindowTitle(self, screen, title)
}
func (self CallbacksUnit) CopyToClipboard(screen *Screen, ty []uint8, data []uint8) {
	CallbacksDefaultCopyToClipboard(self, screen, ty, data)
}
func (self CallbacksUnit) PasteFromClipboard(screen *Screen, ty []uint8) {
	CallbacksDefaultPasteFromClipboard(self, screen, ty)
}
func (self CallbacksUnit) UnhandledChar(screen *Screen, c rune) {
	CallbacksDefaultUnhandledChar(self, screen, c)
}
func (self CallbacksUnit) UnhandledControl(screen *Screen, b uint8) {
	CallbacksDefaultUnhandledControl(self, screen, b)
}
func (self CallbacksUnit) UnhandledEscape(screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], b uint8) {
	CallbacksDefaultUnhandledEscape(self, screen, i1, i2, b)
}
func (self CallbacksUnit) UnhandledCsi(screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], params [][]uint16, c rune) {
	CallbacksDefaultUnhandledCsi(self, screen, i1, i2, params, c)
}
func (self CallbacksUnit) UnhandledOsc(screen *Screen, params [][]uint8) {
	CallbacksDefaultUnhandledOsc(self, screen, params)
}
