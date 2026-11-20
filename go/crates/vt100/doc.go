// Package vt100 stands in for the Rust crate vt100 0.16.2, which parses a
// terminal byte stream and keeps the screen it draws: the cells with their
// text and attributes, the cursor, the modes, the scrollback. Mjolnir's
// dashboard runs the output of a child process through a vt100::Parser and
// draws the Screen with tui-term (go/crates/tuiterm, built on this
// package), and its login pane (mj-tui/src/login_terminal.rs) answers the
// child's terminal queries from Callbacks::unhandled_csi.
//
// The whole crate is ported, file for file: attrs.rs, callbacks.rs,
// cell.rs, grid.rs, lib.rs, parser.rs, perform.rs, row.rs, screen.rs and
// term.rs. It is built on go/crates/vte (vte 0.15.0, which vt100 0.16.2
// uses) and go/crates/unicodewidth (unicode-width 0.2.2, the version
// Mjolnir's Cargo.lock gives vt100); unicode_width.go is the one place that
// asks for a character's width. rsstd.go holds the operations of Rust's
// standard library whose panics are part of what the crate does.
//
// # Names
//
//	vt100::Parser                 Parser[CB]: ParserNew, ParserNewWithCallbacks,
//	                              ParserDefault (Parser::default), Process,
//	                              Screen, ScreenMut, Callbacks, CallbacksMut,
//	                              Write and Flush (impl std::io::Write)
//	vt100::Screen                 Screen: SetSize, Size, SetScrollback,
//	                              Scrollback, Contents, Rows, ContentsBetween,
//	                              StateFormatted, StateDiff, ContentsFormatted,
//	                              RowsFormatted, ContentsDiff, RowsDiff,
//	                              InputModeFormatted, InputModeDiff,
//	                              AttributesFormatted, CursorPosition,
//	                              CursorStateFormatted, Cell, RowWrapped,
//	                              AlternateScreen, ApplicationKeypad,
//	                              ApplicationCursor, HideCursor,
//	                              BracketedPaste, MouseProtocolMode,
//	                              MouseProtocolEncoding, Fgcolor, Bgcolor,
//	                              Bold, Dim, Italic, Underline, Inverse, Clone
//	vt100::Cell                   Cell: Contents, HasContents, IsWide,
//	                              IsWideContinuation, Fgcolor, Bgcolor, Bold,
//	                              Dim, Italic, Underline, Inverse, Equal, Clone
//	vt100::Color                  Color: *Color_Default, *Color_Idx,
//	                              *Color_Rgb; ColorDefault
//	vt100::Callbacks              Callbacks; CallbacksDefault<Method> holds
//	                              each default body
//	impl Callbacks for ()         CallbacksUnit
//	vt100::MouseProtocolMode      MouseProtocolMode_None, _Press,
//	                              _PressRelease, _ButtonMotion, _AnyMotion
//	vt100::MouseProtocolEncoding  MouseProtocolEncoding_Default, _Utf8, _Sgr
//
// `Parser<CB>` is Parser[CB] with CB the type that implements Callbacks.
// Callbacks take `&mut self` in Rust, so a type with state implements them
// on its pointer and the parser holds the pointer: `vt100::Parser<Replies>`
// is `vt100.Parser[*Replies]`, and Callbacks and CallbacksMut return that
// pointer. `vt100::Parser` without a type argument, `Parser<()>`, is
// `vt100.Parser[vt100.CallbacksUnit]`. Default is registered (rs.Default)
// for Color and for Parser[CallbacksUnit].
//
// Screen.Cell returns a pointer into the screen, valid until the screen next
// changes, as the Rust borrow is. Rows, RowsFormatted and RowsDiff return
// iter.Seq values that produce each row when it is asked for, as Rust's
// iterators do. The slices a callback receives belong to the parser and are
// valid only during the call.
//
// # Behaviour product code must know
//
// The crate computes positions in u16, and its arithmetic overflows on some
// sizes and byte streams (a screen of zero rows or columns, columns near
// 65535). Mjolnir is built in Rust's release profile, where the overflow
// wraps; the port wraps the same way, and where the crate then indexes out
// of bounds or unwraps None, the port panics with the same message. In
// Rust's development profile those overflows panic earlier, with "attempt to
// subtract with overflow"; the port does not reproduce that.
//
// The crate panics on input a terminal can receive, and so does the port:
//
//   - On a screen of one row, any character that wraps to a new line panics
//     ("called `Option::unwrap()` on a `None` value"): Grid::col_wrap moves
//     the row it marks as wrapped up by the one line it scrolled, from row 0.
//     The login pane gives its screen one row when the terminal is at most
//     eight rows high.
//   - On a screen of one column, a wide character panics the same way.
//   - A resize that leaves the first half of a wide character in the last
//     column leaves a cell that the next erase of that cell panics on
//     ("index out of bounds: the len is N but the index is N").
//
// Inserting n cells (`CSI n @`) grows the row one cell at a time before it
// cuts it back to the screen's width, which takes time in proportion to n²:
// for n = 65535, 0.6 s in Rust and 1.7 s here, Go's overlapping copies
// being slower than the C library's.
//
// U+17D8 is three columns wide to unicode-width 0.2.2; the crate draws it in
// two cells.
//
// # Tests
//
// The crate's own tests, from the tests/ directory of its repository (the
// crate on crates.io does not ship them), are the it_*_test.go files, with
// tests/helpers/ in it_helpers_test.go and it_helpers_fixtures_test.go.
// Their data, the screen fixtures, the crawl recording and the weechat
// typescript, is in testdata/vt100-rust/ (see ORIGIN there). The quickcheck
// tests draw their inputs as quickcheck 1.0.3 does, from a fixed seed. The
// tests Rust marks #[ignore] run with MJ_RUN_IGNORED=1.
//
// oracle_test.go and oracle_gen_test.go compare the port with vt100 0.16.2
// run by go/port/oracle (module vt100). Each case writes everything a caller
// can observe after every step to a dump: size, cursor, modes and
// attributes; every cell; Contents, Rows, ContentsBetween; the bytes of
// ContentsFormatted, StateFormatted, RowsFormatted and the other formatted
// forms; ContentsDiff, StateDiff and RowsDiff against the previous step, the
// first screen and the screen itself; every row of the scrollback; and
// every callback with the cursor at the time, titles and clipboard included.
// The recorded corpus holds hand-written cases, the crate's own inputs, a
// matrix run from 12 prepared screens (every final byte of a control
// sequence with up to 30 parameter strings, the prefixes none, `?` and `>`
// and an intermediate, and 669 other sequences: every SGR and private mode,
// every escape and control character, OSC, DCS and text), and 4,000
// generated streams: text in several scripts with wide and combining
// characters, every control and escape sequence with random parameters,
// OSC and DCS strings, malformed and truncated sequences and random bytes,
// cut into chunks at random places with resizes and scrollback changes
// between them. Every code point is printed through both to compare widths.
// MJ_ORACLE=live adds the full matrix on every screen and 60,000 fresh
// streams.
package vt100
