// Port of mj-chat/src/lib.rs.

// Package chat is Hel's conversation view state: the chat transcript, its
// input surfaces, and the usage formatting the surfaces render. It is the Go
// port of the mj-chat crate; its modules (components, speech, spinner,
// theme, chat, clipboard, golden, modal, selection, text_input, path_input)
// are files of this one package, named as the brief describes
// (chat/input.rs is chat_input.go).
//
// The terminal surfaces draw with Charm's libraries in place of ratatui and
// crossterm: Bubble Tea v2 messages stand for terminal events, an
// ultraviolet cell buffer for ratatui's Buffer and Rect, and Lip Gloss for
// styles.
//
// Rust's test builds enable mj-core's shared test fixtures at startup
// (enable_test_fixtures_on_startup!); a Go test binary is recognised by
// testing.Testing() where core needs it.
//
// Golden files copied unchanged from mj-chat/tests/golden live under
// testdata/tests/golden and are compared with core.AssertGolden(t,
// "testdata", name, output).
package chat
