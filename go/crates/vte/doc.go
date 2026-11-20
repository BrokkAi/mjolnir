// Package vte stands in for the Rust crate vte, the parser of terminal byte
// streams (Paul Williams' ANSI state machine with UTF-8 input) that hands
// what it parses to a Perform implementation. go/crates/vt100 is built on
// it, and Mjolnir's PTY tests (mj-cli/tests/pty_screen) use it directly.
//
// # Versions
//
// Mjolnir's build holds two versions. vt100 0.16.2 uses vte 0.15.0 with its
// default `std` feature; mj-cli's tests (and strip-ansi-escapes) use vte
// 0.14.1 with `default-features = false`, which in 0.14 is also the std
// build, because the feature 0.14 turns on by default is `no_std`. The files
// here follow the 0.15.0 sources. In the std build, src/lib.rs and
// src/params.rs of the two versions differ only in the names of features,
// so every entry point below reproduces both 0.14.1 and 0.15.0. The versions
// otherwise differ only in the `ansi` module (0.15.0 adds `set_tabs`, for
// `CSI ? 5 W`), which this package does not cover.
//
// # Covered
//
//   - vte::Parser: ParserNew (`Parser::new`), ParserDefault and the zero
//     value (`Parser::default`), Advance (`advance`) and
//     AdvanceUntilTerminated (`advance_until_terminated`): the whole state
//     machine, with every transition of every state; parameters and
//     subparameters (at most 32 in all, each saturating at 65535, the
//     `ignore` flag once the list is full); at most two intermediates;
//     OSC strings split at `;` into at most 16 parameters, kept in a buffer
//     without limit as the std build keeps them, and ended by BEL, ST
//     (`ESC \`), CAN or SUB; DCS strings through Hook, Put and Unhook;
//     SOS, PM and APC strings, which are ignored; UTF-8 decoding in the
//     ground state, where a character cut by the end of a call is completed
//     by the next one, an invalid sequence prints U+FFFD, and C1 controls
//     (0x80 to 0x9F, raw or UTF-8 encoded) are executed. Where Rust's
//     `str::from_utf8` stops and how many bytes it blames decides what is
//     printed, so utf8_validation.go is a copy of Rust's validation.
//   - vte::Perform: the interface Perform. Every method has a default in
//     Rust that does nothing; its body is the function PerformDefault<Name>,
//     which an implementation calls from a one-line method.
//   - vte::Params and vte::ParamsIter: Len, IsEmpty, Iter, IntoIter and
//     Debug (`[1;38:2:255:0:255]`); Next and SizeHint.
//
// The slices and the *Params a Perform method receives belong to the parser
// and are valid only until the method returns, as the borrows are in Rust.
//
// # Not covered
//
//   - The no_std build: `Parser<const OSC_RAW_BUF_SIZE>` and
//     `Parser::new_with_size`, whose OSC buffer has a fixed size. Mjolnir
//     builds both versions with std. The crate's three tests that exist only
//     in the no_std build (build_with_fixed_size, exceed_fixed_osc_buffer_size,
//     fixed_size_osc_containing_string_terminator) are not ported.
//   - The `ansi` module and the `serde` feature, which nothing in Mjolnir
//     enables.
//
// # Tests
//
// lib_test.go is the crate's own tests from src/lib.rs (31 of its 34; the
// three above need no_std). oracle_test.go compares the logs of Perform
// calls with vte 0.15.0 run by go/port/oracle (module vt100, operation vte):
// fixed cases, every byte and every pair of bytes from the ground state,
// every byte and pairs of class bytes from 32 prepared parser states, the
// UTF-8 edge bytes in sequences of up to four cut into chunks every possible
// way, and 3,000 generated streams cut at random places, some driven through
// AdvanceUntilTerminated. MJ_ORACLE=live adds larger matrices and 40,000
// fresh streams.
package vte
