# `go/crates/vte` (vte 0.15.0 via vt100, 0.14.1 direct) and `go/crates/vt100` (vt100 0.16.2)

Oracle module `go/port/oracle/src/vt100_oracle.rs`, feature `vt100`.

Mjolnir embeds terminals in its dashboard: bytes from a child process go
through a `vt100::Parser` and the `Screen` is drawn through `tui-term`
(ported later on top of this). Mjolnir also uses `vte::Parser` with its own
`Perform` once and `vt100::Callbacks` once (grep `vt100::\|vte::`), and
`tui-term-0.3.4/src/` shows what is read from a `Screen` and `Cell`. Screens
must equal Rust's for any byte stream, so port both crates completely.

- `vte`: the state machine (transition table, `params.rs`), UTF-8 handling of
  partial and invalid sequences, parameter and intermediate limits,
  OSC/DCS/SOS/PM/APC strings and terminators, `Perform` as a Go interface,
  `Params` with subparameters. Say in `doc.go` which version each entry point
  reproduces.
- `vt100`: `Parser` (`new`, `process`, `screen`, `screen_mut`, `set_size`,
  scrollback), `Screen` (every content, formatting, diff, cursor, size,
  scrollback, title, mode and colour accessor), `Cell`, `Color`, `Callbacks`,
  the grid with wrapping, wide and combining characters (widths from
  `go/crates/unicodewidth`), tab stops, scroll regions, origin mode,
  insert/delete, erase, save/restore, alternate screen, resize, scrollback.

Tests: both crates' tests (vt100's data-driven fixtures; copy them into
`testdata/` with a note of origin if large), and differential tests: generated
byte streams (text in several scripts with wide and combining characters,
every control and escape sequence with random parameters, malformed and
truncated sequences, random bytes, streams split at arbitrary chunk
boundaries, resizes in between), comparing every cell's contents and
attributes, cursor, modes, title, scrollback, and the bytes of
`contents_formatted()` and `contents_diff()`. Several thousand streams.
