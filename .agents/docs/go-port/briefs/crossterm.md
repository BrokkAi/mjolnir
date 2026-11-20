# Terminal input and output: `go/crates/crossterm` for crossterm 0.29.0

Features `event-stream`, `bracketed-paste`, `osc52`, `serde`. Oracle module
`crossterm_oracle.rs`, feature `crossterm`.

Mjolnir's dashboard (`mj-tui`, `mj-chat`, `mj-cli`) reads the keyboard and
mouse and drives the terminal through crossterm: about 70 files, mostly
`crossterm::event` (`KeyEvent`, `KeyCode`, `KeyModifiers`, `KeyEventKind`,
`KeyEventState`, `MouseEvent`, `Event`, `EventStream`, `poll`, `read`,
keyboard enhancement flags, bracketed paste, focus), plus `terminal`,
`cursor`, `style`, `execute!`/`queue!`, `clipboard`. The Go ratatui backend is
built on this package, so the commands `ratatui-crossterm-0.1.0/src/lib.rs`
queues must be here.

Inventory what Mjolnir and `ratatui-crossterm` use into `doc.go`, then port
from `crossterm-0.29.0/src/`:

1. Input parsing, exactly (`event/sys/unix/parse.rs`): every byte sequence
   yields the same `Event`: characters, control codes, UTF-8, the Alt prefix,
   CSI keys with modifiers, the kitty protocol (`CSI u` with alternate keys,
   event types, text, and the flag responses), legacy, SGR and rxvt mouse
   reports, bracketed paste, focus, status reports consumed internally,
   partial sequences that need more bytes, and invalid sequences. `KeyEvent`
   equality and hashing as crossterm normalises them, the `Display`/`Debug`
   forms used, and the serde form of every event type.
2. The event source: `poll(timeout)`, `read()`, `EventStream` (design it so
   the async runtime `go/crates/tokio` can await it: a reader goroutine and a
   readiness method; say in the report what the integration needs), the Unix
   implementation over the tty (`/dev/tty` or stdin, `SIGWINCH` to
   `Event::Resize`, the wake mechanism) and the Windows implementation over
   console input records with the quirks of `event/sys/windows/parse.rs`.
3. Commands and their escape sequences, byte for byte: all of `terminal.rs`,
   `cursor.rs`, `style.rs` (colors in every form, `NO_COLOR` handling,
   attributes, `SetStyle`, `PrintStyledContent`, `Print`), the `event.rs`
   commands, `clipboard.rs` (OSC 52), synchronized update, scrolling,
   `SetSize`, `SetTitle`. `Execute(w, commands...)` and `Queue(w,
   commands...)`; on Windows the WinAPI fallback crossterm uses when ANSI is
   unsupported.
4. Terminal state: `enable_raw_mode`/`disable_raw_mode` (the exact termios
   changes, saved and restored as crossterm does, through
   `golang.org/x/sys/unix`; console modes on Windows), `is_raw_mode_enabled`,
   `size()` with its fallbacks, `window_size()`,
   `supports_keyboard_enhancement()`, `cursor::position()`.

Choose a consistent, compact spelling for the event types, since product code
names them in hundreds of places, and list the names in `doc.go`.

Tests: crossterm's own tests (the parser tests are extensive). Differential
tests: the parser is not public API, so vendor `parse.rs` and the types it
needs into the oracle module's directory as a test fixture (note origin,
version and licence; change only visibility), and compare parse results for a
large generated corpus (every key with every modifier in every protocol, mouse
reports, pastes, random bytes, truncated and concatenated sequences). For
commands, compare the bytes written for generated parameters.
