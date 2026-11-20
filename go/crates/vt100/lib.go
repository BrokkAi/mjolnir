// Port of vt100 0.16.2 src/lib.rs

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
// names of this package, without a module prefix. The crate's own
// documentation is the package comment in doc.go.
