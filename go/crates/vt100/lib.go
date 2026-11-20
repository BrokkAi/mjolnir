// Port of vt100 0.16.2 src/lib.rs

// This crate parses a terminal byte stream and provides an in-memory
// representation of the rendered contents.
//
// # Overview
//
// This is essentially the terminal parser component of a graphical terminal
// emulator pulled out into a separate crate. Although you can use this crate
// to build a graphical terminal emulator, it also contains functionality
// necessary for implementing terminal applications that want to run other
// terminal applications - programs like `screen` or `tmux` for example.
//
// # Synopsis
//
//	let mut parser = vt100::Parser::new(24, 80, 0);
//
//	let screen = parser.screen().clone();
//	parser.process(b"this text is \x1b[31mRED\x1b[m");
//	assert_eq!(
//	    parser.screen().cell(0, 13).unwrap().fgcolor(),
//	    vt100::Color::Idx(1),
//	);
//
//	let screen = parser.screen().clone();
//	parser.process(b"\x1b[3D\x1b[32mGREEN");
//	assert_eq!(
//	    parser.screen().contents_formatted(),
//	    &b"\x1b[?25h\x1b[m\x1b[H\x1b[Jthis text is \x1b[32mGREEN"[..],
//	);
//	assert_eq!(
//	    parser.screen().contents_diff(&screen),
//	    &b"\x1b[1;14H\x1b[32mGREEN"[..],
//	);
//
// (lib_test.go runs this example, as Rust runs it as a test.)

package vt100

// The crate root declares the modules and re-exports their public types:
//
//	pub use attrs::Color;
//	pub use callbacks::Callbacks;
//	pub use cell::Cell;
//	pub use parser::Parser;
//	pub use screen::{MouseProtocolEncoding, MouseProtocolMode, Screen};
//
// Those are the names Rust code writes (`vt100::Parser`), so they are the
// names of this package, without a module prefix. What the package covers is
// described in doc.go.
