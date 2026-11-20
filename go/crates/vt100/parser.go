// Port of vt100 0.16.2 src/parser.rs

package vt100

import "github.com/BrokkAi/mjolnir/go/crates/vte"

// A parser for terminal output which produces an in-memory representation of
// the terminal contents.
//
// CB is the type of the callbacks. The methods of Callbacks take `&mut self`
// in Rust, so a type that keeps state implements them on its pointer, and it
// is the pointer type that is given here: `vt100::Parser<Replies>` is
// `vt100.Parser[*Replies]`. Rust's default, `Parser<()>`, is
// `vt100.Parser[vt100.CallbacksUnit]`.
type Parser[CB Callbacks] struct {
	parser vte.Parser
	screen performWrappedScreen[CB]
}

// Creates a new terminal parser of the given size and with the given
// amount of scrollback.
func ParserNew(rows uint16, cols uint16, scrollbackLen int) Parser[CallbacksUnit] {
	return Parser[CallbacksUnit]{
		parser: vte.ParserNew(),
		screen: performWrappedScreenNew(
			rows,
			cols,
			scrollbackLen,
		),
	}
}

// Creates a new terminal parser of the given size and with the given
// amount of scrollback. Terminal events will be reported via method
// calls on the provided [`Callbacks`](crate::callbacks::Callbacks)
// implementation.
func ParserNewWithCallbacks[CB Callbacks](
	rows uint16,
	cols uint16,
	scrollbackLen int,
	callbacks CB,
) Parser[CB] {
	return Parser[CB]{
		parser: vte.ParserNew(),
		screen: performWrappedScreenNewWithCallbacks(
			rows,
			cols,
			scrollbackLen,
			callbacks,
		),
	}
}

// Processes the contents of the given byte string, and updates the
// in-memory terminal state.
func (self *Parser[CB]) Process(bytes []uint8) {
	self.parser.Advance(&self.screen, bytes)
}

// Returns a reference to a [`Screen`](crate::Screen) object containing
// the terminal state.
func (self *Parser[CB]) Screen() *Screen {
	return &self.screen.screen
}

// Returns a mutable reference to a [`Screen`](crate::Screen) object
// containing the terminal state.
func (self *Parser[CB]) ScreenMut() *Screen {
	return &self.screen.screen
}

// Returns a reference to the [`Callbacks`](crate::callbacks::Callbacks)
// state object passed into the constructor.
//
// CB is the pointer the constructor was given, which is the reference.
func (self *Parser[CB]) Callbacks() CB {
	return self.screen.callbacks
}

// Returns a mutable reference to the
// [`Callbacks`](crate::callbacks::Callbacks) state object passed into
// the constructor.
func (self *Parser[CB]) CallbacksMut() CB {
	return self.screen.callbacks
}

// ParserDefault is `Parser::default()`.
//
// Returns a parser with dimensions 80x24 and no scrollback.
func ParserDefault() Parser[CallbacksUnit] {
	return ParserNew(24, 80, 0)
}

// Write is `impl std::io::Write for Parser`: it processes buf and reports
// all of it written. Rust has it only for the parser without callbacks.
func (self *Parser[CB]) Write(buf []uint8) (int, error) {
	self.Process(buf)
	return len(buf), nil
}

// Flush is `std::io::Write::flush`, which does nothing.
func (self *Parser[CB]) Flush() error {
	return nil
}
