//! Oracle module `vt100`: the `vte` 0.15.0 parser and the `vt100` 0.16.2
//! terminal, for go/crates/vte and go/crates/vt100.
//!
//! Requests are objects with an `"op"` member:
//!
//! - `{"op":"vte","cases":[case..],"full":bool}` runs every case on a new
//!   `vte::Parser` and replies with the total number of `Perform` calls and,
//!   for each case, eight hex digits of a digest of the log of its calls. A
//!   case is a list of base64 chunks, each passed to `advance`, or
//!   `{"c":[b64..],"t":n}`, whose chunks go through `advance_until_terminated`
//!   with a performer that reports itself terminated after every `n` calls.
//! - `{"op":"vte","matrix":{"prefix":b64,"alphabet":b64,"len":n,"suffix":b64,"split":bool}}`
//!   runs every sequence of 1 to `n` bytes of the alphabet, each on a new
//!   parser between the prefix and the suffix and, with `split`, cut into
//!   chunks in every possible way. The reply holds a digest for each first
//!   byte.
//! - `{"op":"vt100","cases":[case..],"full":bool}` runs every case on a new
//!   `vt100::Parser`. A case is `{"r":rows,"c":cols,"sb":scrollback,"z":bool,"s":[step..]}`
//!   and a step is a base64 string (`process`), a pair `[rows, cols]`
//!   (`set_size`) or a number (`set_scrollback`). Before the first step and
//!   after every step the whole observable state is written to a dump, in
//!   seven sections (cursor and modes, cells, text, formatted output, diffs,
//!   scrollback, callbacks). The reply holds, for each case, eight hex digits
//!   of a digest of each section, or `{"s":digests,"panic":message,"at":step}`
//!   when a step panicked. `z` makes the `resize` callback apply the requested
//!   size, as a terminal emulator does.
//! - `{"op":"vt100","matrix":{"r":rows,"c":cols,"sb":n,"z":bool,"setup":b64,"tail":b64,"csi":{..},"seqs":[b64..]}}`
//!   runs many short sequences from one prepared screen: for each, a new
//!   parser processes `setup`, the sequence and `tail`, and the whole dump is
//!   hashed. `csi` is `{"prefixes":[..],"params":[..],"inter":[..],"lo":n,"hi":n}`
//!   and stands for `ESC [ prefix params intermediate final` with every final
//!   byte from `lo` to `hi`; the reply has one digest for each (prefix,
//!   params, intermediate). The sequences of `seqs` get a digest each.
//! - `{"op":"width","from":a,"to":b}` prints every code point in the range on a
//!   2x8 screen and replies with where the cursor ended, run-length encoded.
//!   That is the width `unicode-width` gave the character, as `vt100` uses it.
//!
//! With `"full": true` a reply also carries the dump text, which is how a
//! difference in a digest is tracked down. The Go side builds the same text
//! (go/crates/vt100/oracle_test.go, go/crates/vte/oracle_test.go); the two
//! must be kept in step line for line.

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    // Panics in the code under test are results here, and there are many of
    // them: keep the default hook from writing each one to standard error.
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));

    match crate::text(request, "op") {
        "vte" => vte_op(request),
        "vt100" => vt100_op(request),
        "width" => width_op(request),
        _ => crate::unknown(request),
    }
}

// --- shared helpers ---------------------------------------------------------

/// FNV-1a, 64 bits.
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// Standard base64 with padding, which is how Go's encoding/json writes a
/// byte slice.
fn b64(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => panic!("oracle: request holds bad base64"),
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Bytes as printable ASCII: everything outside 0x20..=0x7e, the backslash
/// and the separators `|` and `;` are written `\xNN`.
fn esc(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        match *byte {
            b'\\' | b'|' | b';' => write!(out, "\\x{byte:02x}").unwrap(),
            0x20..=0x7e => out.push(char::from(*byte)),
            _ => write!(out, "\\x{byte:02x}").unwrap(),
        }
    }
    out
}

fn bit(value: bool) -> char {
    if value { '1' } else { '0' }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

/// The value of `f`, or `PANIC <message>` when it panics.
fn guard(f: impl FnOnce() -> String) -> String {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(text) => text,
        Err(payload) => format!("PANIC {}", panic_message(payload)),
    }
}

fn params_text(params: &vte::Params) -> String {
    let mut out = String::new();
    for (i, param) in params.iter().enumerate() {
        if i != 0 {
            out.push(';');
        }
        for (j, subparam) in param.iter().enumerate() {
            if j != 0 {
                out.push(':');
            }
            write!(out, "{subparam}").unwrap();
        }
    }
    out
}

// --- vte --------------------------------------------------------------------

struct VteLog {
    lines: Vec<String>,
    /// Calls since the driver last cleared the count.
    since: usize,
    /// `terminated()` holds once `since` reaches this; 0 means never.
    term: usize,
}

impl VteLog {
    fn push(&mut self, line: String) {
        self.lines.push(line);
        self.since += 1;
    }
}

impl vte::Perform for VteLog {
    fn print(&mut self, c: char) {
        self.push(format!("p {:x}", u32::from(c)));
    }

    fn execute(&mut self, byte: u8) {
        self.push(format!("x {byte:02x}"));
    }

    fn hook(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        self.push(format!(
            "h [{}] len={} empty={} {} {} {:x}",
            params_text(params),
            params.len(),
            bit(params.is_empty()),
            esc(intermediates),
            bit(ignore),
            u32::from(action)
        ));
    }

    fn put(&mut self, byte: u8) {
        self.push(format!("u {byte:02x}"));
    }

    fn unhook(&mut self) {
        self.push("U".to_owned());
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let joined = params.iter().map(|param| esc(param)).collect::<Vec<_>>().join("|");
        self.push(format!("o {} {} {}", params.len(), joined, bit(bell_terminated)));
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        self.push(format!(
            "c [{}] len={} empty={} {} {} {:x}",
            params_text(params),
            params.len(),
            bit(params.is_empty()),
            esc(intermediates),
            bit(ignore),
            u32::from(action)
        ));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.push(format!("e {} {} {byte:02x}", esc(intermediates), bit(ignore)));
    }

    fn terminated(&self) -> bool {
        self.term != 0 && self.since >= self.term
    }
}

/// Runs one case: a list of chunks, or `{"c": chunks, "t": term}`.
fn vte_case(case: &Value) -> Vec<String> {
    let (chunks, term) = match case {
        Value::Object(fields) => (
            fields.get("c").and_then(Value::as_array),
            fields.get("t").and_then(Value::as_u64).unwrap_or(0) as usize,
        ),
        other => (other.as_array(), 0),
    };
    let chunks: Vec<Vec<u8>> = chunks
        .map(|chunks| chunks.iter().map(|chunk| b64(chunk.as_str().unwrap_or_default())).collect())
        .unwrap_or_default();

    let mut parser = vte::Parser::new();
    let mut log = VteLog { lines: Vec::new(), since: 0, term };
    for chunk in &chunks {
        if term == 0 {
            parser.advance(&mut log, chunk);
            continue;
        }
        let mut offset = 0;
        loop {
            let read = parser.advance_until_terminated(&mut log, &chunk[offset..]);
            log.lines.push(format!("r {read}"));
            offset += read;
            if offset >= chunk.len() {
                break;
            }
            log.since = 0;
        }
    }
    log.lines
}

/// Runs every sequence of 1 to `len` bytes of `alphabet` between `prefix` and
/// `suffix`, each on a new parser, and with `split` once for every way of
/// cutting the sequence into chunks. The reply holds one digest for each
/// first byte, over the logs of all the sequences that start with it.
fn vte_matrix(spec: &Value) -> Value {
    let field = |key: &str| b64(spec.get(key).and_then(Value::as_str).unwrap_or_default());
    let prefix = field("prefix");
    let alphabet = field("alphabet");
    let suffix = field("suffix");
    let max_len = spec.get("len").and_then(Value::as_u64).unwrap_or(1) as usize;
    let split = spec.get("split").and_then(Value::as_bool).unwrap_or(false);

    let mut each = String::new();
    let mut calls = 0;
    for first in &alphabet {
        let mut hash = Fnv::new();
        for len in 1..=max_len {
            let combinations = alphabet.len().pow((len - 1) as u32);
            for index in 0..combinations {
                let mut sequence = vec![*first; len];
                let mut rest = index;
                for position in (1..len).rev() {
                    sequence[position] = alphabet[rest % alphabet.len()];
                    rest /= alphabet.len();
                }
                let masks = if split { 1u32 << (len - 1) } else { 1 };
                for mask in 0..masks {
                    let mut parser = vte::Parser::new();
                    let mut log = VteLog { lines: Vec::new(), since: 0, term: 0 };
                    parser.advance(&mut log, &prefix);
                    let mut start = 0;
                    for gap in 1..len {
                        if mask & (1 << (gap - 1)) != 0 {
                            parser.advance(&mut log, &sequence[start..gap]);
                            start = gap;
                        }
                    }
                    parser.advance(&mut log, &sequence[start..]);
                    parser.advance(&mut log, &suffix);
                    for line in &log.lines {
                        hash.write(line.as_bytes());
                        hash.write(b"\n");
                    }
                    hash.write(b"--\n");
                    calls += log.lines.len();
                }
            }
        }
        write!(each, "{:08x}", hash.0 as u32).unwrap();
    }
    json!({ "n": calls, "each": each })
}

fn vte_op(request: &Value) -> Value {
    if let Some(spec) = request.get("matrix") {
        return vte_matrix(spec);
    }
    let full = request.get("full").and_then(Value::as_bool).unwrap_or(false);
    let no_cases = Vec::new();
    let cases = request.get("cases").and_then(Value::as_array).unwrap_or(&no_cases);

    let mut each = String::new();
    let mut calls = 0;
    let mut logs = Vec::new();
    for case in cases {
        let lines = vte_case(case);
        let mut hash = Fnv::new();
        for line in &lines {
            hash.write(line.as_bytes());
            hash.write(b"\n");
        }
        write!(each, "{:08x}", hash.0 as u32).unwrap();
        calls += lines.len();
        if full {
            logs.push(Value::String(lines.join("\n")));
        }
    }
    let mut reply = json!({ "n": calls, "each": each });
    if full {
        reply["logs"] = Value::Array(logs);
    }
    reply
}

// --- vt100 ------------------------------------------------------------------

/// Records every callback, with the cursor position the screen had when it
/// was called.
struct Recorder {
    lines: Vec<String>,
    apply_resize: bool,
}

impl Recorder {
    fn push(&mut self, screen: &vt100::Screen, line: String) {
        let (row, col) = screen.cursor_position();
        self.lines.push(format!("{line} @{row},{col}"));
    }
}

fn option_byte(value: Option<u8>) -> String {
    value.map_or_else(|| "-".to_owned(), |byte| format!("{byte:02x}"))
}

impl vt100::Callbacks for Recorder {
    fn audible_bell(&mut self, screen: &mut vt100::Screen) {
        self.push(screen, "bell".to_owned());
    }

    fn visual_bell(&mut self, screen: &mut vt100::Screen) {
        self.push(screen, "vbell".to_owned());
    }

    fn resize(&mut self, screen: &mut vt100::Screen, (rows, cols): (u16, u16)) {
        self.push(screen, format!("resize {rows} {cols}"));
        // A real terminal resizes; sizes are bounded so that a request for
        // 65535 columns does not allocate gigabytes.
        if self.apply_resize && rows <= 200 && cols <= 200 {
            screen.set_size(rows, cols);
        }
    }

    fn set_window_icon_name(&mut self, screen: &mut vt100::Screen, icon_name: &[u8]) {
        self.push(screen, format!("icon {}", esc(icon_name)));
    }

    fn set_window_title(&mut self, screen: &mut vt100::Screen, title: &[u8]) {
        self.push(screen, format!("title {}", esc(title)));
    }

    fn copy_to_clipboard(&mut self, screen: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        self.push(screen, format!("copy {}|{}", esc(ty), esc(data)));
    }

    fn paste_from_clipboard(&mut self, screen: &mut vt100::Screen, ty: &[u8]) {
        self.push(screen, format!("paste {}", esc(ty)));
    }

    fn unhandled_char(&mut self, screen: &mut vt100::Screen, c: char) {
        self.push(screen, format!("char {:x}", u32::from(c)));
    }

    fn unhandled_control(&mut self, screen: &mut vt100::Screen, b: u8) {
        self.push(screen, format!("control {b:02x}"));
    }

    fn unhandled_escape(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, b: u8) {
        self.push(screen, format!("escape {} {} {b:02x}", option_byte(i1), option_byte(i2)));
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let params = params
            .iter()
            .map(|param| param.iter().map(u16::to_string).collect::<Vec<_>>().join(":"))
            .collect::<Vec<_>>()
            .join(";");
        self.push(
            screen,
            format!("csi {} {} [{params}] {:x}", option_byte(i1), option_byte(i2), u32::from(c)),
        );
    }

    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        let joined = params.iter().map(|param| esc(param)).collect::<Vec<_>>().join("|");
        self.push(screen, format!("osc {} {joined}", params.len()));
    }
}

const SECTIONS: [&str; 7] = ["cur", "cells", "text", "fmt", "diff", "sb", "cb"];
const CUR: usize = 0;
const CELLS: usize = 1;
const TEXT: usize = 2;
const FMT: usize = 3;
const DIFF: usize = 4;
const SB: usize = 5;
const CB: usize = 6;

struct Dump {
    /// Every line, in the order written.
    all: Fnv,
    sections: [Fnv; 7],
    full: Option<String>,
}

impl Dump {
    fn new(full: bool) -> Self {
        Self { all: Fnv::new(), sections: [Fnv::new(); 7], full: full.then(String::new) }
    }

    fn line(&mut self, section: usize, text: &str) {
        let line = format!("{} {text}\n", SECTIONS[section]);
        self.all.write(line.as_bytes());
        self.sections[section].write(line.as_bytes());
        if let Some(full) = &mut self.full {
            full.push_str(&line);
        }
    }
}

fn color_text(color: vt100::Color) -> String {
    match color {
        vt100::Color::Default => "d".to_owned(),
        vt100::Color::Idx(i) => format!("i{i}"),
        vt100::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

fn mouse_mode_text(mode: vt100::MouseProtocolMode) -> &'static str {
    match mode {
        vt100::MouseProtocolMode::None => "none",
        vt100::MouseProtocolMode::Press => "press",
        vt100::MouseProtocolMode::PressRelease => "press_release",
        vt100::MouseProtocolMode::ButtonMotion => "button_motion",
        vt100::MouseProtocolMode::AnyMotion => "any_motion",
    }
}

fn mouse_encoding_text(encoding: vt100::MouseProtocolEncoding) -> &'static str {
    match encoding {
        vt100::MouseProtocolEncoding::Default => "default",
        vt100::MouseProtocolEncoding::Utf8 => "utf8",
        vt100::MouseProtocolEncoding::Sgr => "sgr",
    }
}

fn cell_text(cell: Option<&vt100::Cell>) -> String {
    let Some(cell) = cell else {
        return "-".to_owned();
    };
    let plain = !cell.has_contents()
        && !cell.is_wide()
        && !cell.is_wide_continuation()
        && cell.fgcolor() == vt100::Color::Default
        && cell.bgcolor() == vt100::Color::Default
        && !cell.bold()
        && !cell.dim()
        && !cell.italic()
        && !cell.underline()
        && !cell.inverse();
    if plain {
        return ".".to_owned();
    }
    format!(
        "{}/{}{}{}/{}/{}/{}{}{}{}{}",
        esc(cell.contents().as_bytes()),
        bit(cell.has_contents()),
        bit(cell.is_wide()),
        bit(cell.is_wide_continuation()),
        color_text(cell.fgcolor()),
        color_text(cell.bgcolor()),
        bit(cell.bold()),
        bit(cell.dim()),
        bit(cell.italic()),
        bit(cell.underline()),
        bit(cell.inverse()),
    )
}

fn row_text(screen: &vt100::Screen, row: u16, cols: u16) -> String {
    let mut out = String::new();
    for col in 0..cols {
        if col != 0 {
            out.push(';');
        }
        out.push_str(&cell_text(screen.cell(row, col)));
    }
    out
}

fn joined_rows(rows: impl Iterator<Item = Vec<u8>>) -> String {
    rows.map(|row| esc(&row)).collect::<Vec<_>>().join("|")
}

/// Writes everything a caller can observe of `screen` to the dump. `prev` is
/// the screen before the last step and `base` the screen the parser started
/// with.
fn dump_screen(dump: &mut Dump, screen: &vt100::Screen, prev: &vt100::Screen, base: &vt100::Screen) {
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();

    dump.line(CUR, &format!("size {rows} {cols}"));
    dump.line(CUR, &format!("cursor {cursor_row} {cursor_col}"));
    dump.line(
        CUR,
        &format!(
            "modes alt={} keypad={} appcursor={} hide={} paste={} mouse={} enc={}",
            bit(screen.alternate_screen()),
            bit(screen.application_keypad()),
            bit(screen.application_cursor()),
            bit(screen.hide_cursor()),
            bit(screen.bracketed_paste()),
            mouse_mode_text(screen.mouse_protocol_mode()),
            mouse_encoding_text(screen.mouse_protocol_encoding()),
        ),
    );
    dump.line(
        CUR,
        &format!(
            "attrs fg={} bg={} bold={} dim={} italic={} underline={} inverse={}",
            color_text(screen.fgcolor()),
            color_text(screen.bgcolor()),
            bit(screen.bold()),
            bit(screen.dim()),
            bit(screen.italic()),
            bit(screen.underline()),
            bit(screen.inverse()),
        ),
    );
    dump.line(CUR, &format!("scrollback {}", screen.scrollback()));

    for row in 0..rows {
        let text = guard(|| format!("{} {}", bit(screen.row_wrapped(row)), row_text(screen, row, cols)));
        dump.line(CELLS, &format!("row {row} {text}"));
    }
    dump.line(
        CELLS,
        &format!(
            "outside {} {} {}",
            cell_text(screen.cell(rows, 0)),
            cell_text(screen.cell(0, cols)),
            bit(screen.row_wrapped(rows)),
        ),
    );

    let part_start = cols / 3;
    let part_width = cols / 2 + 1;
    let last_row = rows.saturating_sub(1);
    dump.line(TEXT, &format!("contents {}", guard(|| esc(screen.contents().as_bytes()))));
    dump.line(
        TEXT,
        &format!(
            "rows {}",
            guard(|| screen.rows(0, cols).map(|row| esc(row.as_bytes())).collect::<Vec<_>>().join("|"))
        ),
    );
    dump.line(
        TEXT,
        &format!(
            "rows_part {}",
            guard(|| screen
                .rows(part_start, part_width)
                .map(|row| esc(row.as_bytes()))
                .collect::<Vec<_>>()
                .join("|"))
        ),
    );
    dump.line(
        TEXT,
        &format!(
            "rows_past {}",
            guard(|| screen.rows(1, cols).map(|row| esc(row.as_bytes())).collect::<Vec<_>>().join("|"))
        ),
    );
    let between = [
        (0, 0, last_row, cols),
        (cursor_row, cursor_col.min(cols), last_row, cols / 2),
        (rows / 2, part_start, rows / 2, cols),
        (rows / 2, part_start, rows / 2, part_start),
        (1, 0, 0, 0),
        (0, cols.saturating_sub(1), last_row.min(1), 1),
        (0, part_start, rows, part_width),
    ];
    for (start_row, start_col, end_row, end_col) in between {
        dump.line(
            TEXT,
            &format!(
                "between {start_row} {start_col} {end_row} {end_col} {}",
                guard(|| esc(screen.contents_between(start_row, start_col, end_row, end_col).as_bytes()))
            ),
        );
    }

    dump.line(FMT, &format!("contents_formatted {}", guard(|| esc(&screen.contents_formatted()))));
    dump.line(FMT, &format!("state_formatted {}", guard(|| esc(&screen.state_formatted()))));
    dump.line(FMT, &format!("input_mode_formatted {}", guard(|| esc(&screen.input_mode_formatted()))));
    dump.line(FMT, &format!("attributes_formatted {}", guard(|| esc(&screen.attributes_formatted()))));
    dump.line(FMT, &format!("cursor_state_formatted {}", guard(|| esc(&screen.cursor_state_formatted()))));
    dump.line(FMT, &format!("rows_formatted {}", guard(|| joined_rows(screen.rows_formatted(0, cols)))));
    dump.line(
        FMT,
        &format!("rows_formatted_part {}", guard(|| joined_rows(screen.rows_formatted(part_start, part_width)))),
    );

    dump.line(DIFF, &format!("contents_diff {}", guard(|| esc(&screen.contents_diff(prev)))));
    dump.line(DIFF, &format!("state_diff {}", guard(|| esc(&screen.state_diff(prev)))));
    dump.line(DIFF, &format!("input_mode_diff {}", guard(|| esc(&screen.input_mode_diff(prev)))));
    dump.line(DIFF, &format!("rows_diff {}", guard(|| joined_rows(screen.rows_diff(prev, 0, cols)))));
    dump.line(
        DIFF,
        &format!("rows_diff_part {}", guard(|| joined_rows(screen.rows_diff(prev, part_start, part_width)))),
    );
    dump.line(DIFF, &format!("contents_diff_base {}", guard(|| esc(&screen.contents_diff(base)))));
    dump.line(DIFF, &format!("contents_diff_back {}", guard(|| esc(&prev.contents_diff(screen)))));
    dump.line(DIFF, &format!("contents_diff_self {}", guard(|| esc(&screen.contents_diff(screen)))));

    // The scrollback is read through a copy, one offset at a time: at offset
    // k the top visible row is the k-th newest row that scrolled off.
    let mut probe = screen.clone();
    probe.set_scrollback(usize::MAX);
    let depth = probe.scrollback();
    dump.line(SB, &format!("depth {depth}"));
    if depth > 0 {
        dump.line(SB, &format!("oldest_contents {}", guard(|| esc(probe.contents().as_bytes()))));
        dump.line(SB, &format!("oldest_formatted {}", guard(|| esc(&probe.contents_formatted()))));
        dump.line(SB, &format!("oldest_diff {}", guard(|| esc(&probe.contents_diff(screen)))));
    }
    for offset in 1..=depth {
        probe.set_scrollback(offset);
        let text = guard(|| format!("{} {}", bit(probe.row_wrapped(0)), row_text(&probe, 0, cols)));
        dump.line(SB, &format!("at {offset} {text}"));
    }
}

/// One step of a case.
enum Step {
    Process(Vec<u8>),
    SetSize(u16, u16),
    SetScrollback(usize),
}

/// Runs the steps on a new parser, writing the state to the dump before the
/// first step and after every step. Returns the step that panicked (counted
/// from 1, with 0 for the creation of the parser) and the panic message.
fn vt100_run(
    dump: &mut Dump,
    rows: u16,
    cols: u16,
    scrollback: usize,
    apply_resize: bool,
    steps: &[Step],
) -> Option<(usize, String)> {
    let created = catch_unwind(AssertUnwindSafe(|| {
        vt100::Parser::new_with_callbacks(rows, cols, scrollback, Recorder { lines: Vec::new(), apply_resize })
    }));
    let mut parser = match created {
        Err(payload) => return Some((0, panic_message(payload))),
        Ok(parser) => parser,
    };
    let base = parser.screen().clone();
    dump.line(CUR, "start");
    dump_screen(dump, parser.screen(), &base, &base);
    for (index, step) in steps.iter().enumerate() {
        let prev = parser.screen().clone();
        let ran = catch_unwind(AssertUnwindSafe(|| match step {
            Step::Process(bytes) => parser.process(bytes),
            Step::SetSize(rows, cols) => parser.screen_mut().set_size(*rows, *cols),
            Step::SetScrollback(offset) => parser.screen_mut().set_scrollback(*offset),
        }));
        // What the callbacks saw is reported even when the step then
        // panicked.
        let lines = std::mem::take(&mut parser.callbacks_mut().lines);
        dump.line(CUR, &format!("step {index}"));
        for line in &lines {
            dump.line(CB, line);
        }
        if let Err(payload) = ran {
            return Some((index + 1, panic_message(payload)));
        }
        dump_screen(dump, parser.screen(), &prev, &base);
    }
    None
}

/// Runs one case and returns the digests of its dump, where it panicked and,
/// when asked for, the dump.
fn vt100_case(case: &Value, full: bool) -> (String, Option<(usize, String)>, Option<String>) {
    let number = |key: &str| case.get(key).and_then(Value::as_u64).unwrap_or(0);
    let no_steps = Vec::new();
    let steps: Vec<Step> = case
        .get("s")
        .and_then(Value::as_array)
        .unwrap_or(&no_steps)
        .iter()
        .map(|step| match step {
            Value::String(chunk) => Step::Process(b64(chunk)),
            Value::Array(size) => Step::SetSize(
                size.first().and_then(Value::as_u64).unwrap_or(0) as u16,
                size.get(1).and_then(Value::as_u64).unwrap_or(0) as u16,
            ),
            Value::Number(offset) => Step::SetScrollback(offset.as_u64().unwrap_or(0) as usize),
            _ => panic!("oracle: unknown step"),
        })
        .collect();

    let mut dump = Dump::new(full);
    let failure = vt100_run(
        &mut dump,
        number("r") as u16,
        number("c") as u16,
        number("sb") as usize,
        case.get("z").and_then(Value::as_bool).unwrap_or(false),
        &steps,
    );
    let digests = dump.sections.iter().map(|hash| format!("{:08x}", hash.0 as u32)).collect::<String>();
    (digests, failure, dump.full)
}

/// Runs many short sequences from one prepared screen: for each sequence a
/// new parser processes `setup`, the sequence and `tail`, and everything
/// written to the dump is hashed. The control sequences of `csi` are
/// `ESC [ prefix params intermediate final` for every prefix, parameter
/// string, intermediate and final byte from `lo` to `hi`; the reply holds one
/// digest for each (prefix, params, intermediate), over all the finals. The
/// sequences of `seqs` are given whole and get a digest each.
fn vt100_matrix(spec: &Value) -> Value {
    let number = |key: &str| spec.get(key).and_then(Value::as_u64).unwrap_or(0);
    let rows = number("r") as u16;
    let cols = number("c") as u16;
    let scrollback = number("sb") as usize;
    let apply_resize = spec.get("z").and_then(Value::as_bool).unwrap_or(false);
    let setup = b64(crate::text(spec, "setup"));
    let tail = b64(crate::text(spec, "tail"));

    let run = |hash: &mut Fnv, sequence: Vec<u8>| {
        let mut dump = Dump::new(false);
        let steps = [Step::Process(setup.clone()), Step::Process(sequence), Step::Process(tail.clone())];
        let failure = vt100_run(&mut dump, rows, cols, scrollback, apply_resize, &steps);
        hash.write(format!("{:016x}\n", dump.all.0).as_bytes());
        if let Some((at, message)) = failure {
            hash.write(format!("panic {at} {message}\n").as_bytes());
        }
    };

    let mut reply = json!({});
    if let Some(csi) = spec.get("csi") {
        let strings = |key: &str| -> Vec<String> {
            csi.get(key)
                .and_then(Value::as_array)
                .map(|values| values.iter().map(|value| value.as_str().unwrap_or_default().to_owned()).collect())
                .unwrap_or_default()
        };
        let lo = csi.get("lo").and_then(Value::as_u64).unwrap_or(0x40) as u8;
        let hi = csi.get("hi").and_then(Value::as_u64).unwrap_or(0x7e) as u8;
        let mut each = String::new();
        for prefix in strings("prefixes") {
            for params in strings("params") {
                for intermediate in strings("inter") {
                    let mut hash = Fnv::new();
                    for last in lo..=hi {
                        let mut sequence = b"\x1b[".to_vec();
                        sequence.extend_from_slice(prefix.as_bytes());
                        sequence.extend_from_slice(params.as_bytes());
                        sequence.extend_from_slice(intermediate.as_bytes());
                        sequence.push(last);
                        run(&mut hash, sequence);
                    }
                    write!(each, "{:08x}", hash.0 as u32).unwrap();
                }
            }
        }
        reply["csi"] = Value::String(each);
    }
    if let Some(sequences) = spec.get("seqs").and_then(Value::as_array) {
        let mut each = String::new();
        for sequence in sequences {
            let mut hash = Fnv::new();
            run(&mut hash, b64(sequence.as_str().unwrap_or_default()));
            write!(each, "{:08x}", hash.0 as u32).unwrap();
        }
        reply["seqs"] = Value::String(each);
    }
    reply
}

fn vt100_op(request: &Value) -> Value {
    if let Some(spec) = request.get("matrix") {
        return vt100_matrix(spec);
    }
    let full = request.get("full").and_then(Value::as_bool).unwrap_or(false);
    let no_cases = Vec::new();
    let cases = request.get("cases").and_then(Value::as_array).unwrap_or(&no_cases);

    let mut each = Vec::new();
    let mut dumps = Vec::new();
    for case in cases {
        let (digests, failure, dump) = vt100_case(case, full);
        each.push(match failure {
            None => Value::String(digests),
            Some((at, message)) => json!({ "s": digests, "panic": message, "at": at }),
        });
        if let Some(dump) = dump {
            dumps.push(Value::String(dump));
        }
    }
    let mut reply = json!({ "each": each });
    if full {
        reply["dumps"] = Value::Array(dumps);
    }
    reply
}

// --- width ------------------------------------------------------------------

/// Where the cursor is after `c` is printed at row 0, column 6 of a 2x8
/// screen, as `row * 100 + col`, plus 1000 when the character was added to
/// the cell before the cursor. Widths 0, 1, 2 and 3 give 1006, 7, 8 and 102.
fn width_probe(c: char) -> u32 {
    let mut parser = vt100::Parser::new(2, 8, 0);
    parser.process(b"\x1b[1;7H");
    let mut buffer = [0; 4];
    parser.process(c.encode_utf8(&mut buffer).as_bytes());
    let (row, col) = parser.screen().cursor_position();
    let appended = parser.screen().cell(0, 5).is_some_and(vt100::Cell::has_contents);
    u32::from(row) * 100 + u32::from(col) + if appended { 1000 } else { 0 }
}

fn width_op(request: &Value) -> Value {
    let from = request.get("from").and_then(Value::as_u64).unwrap_or(0) as u32;
    let to = request.get("to").and_then(Value::as_u64).unwrap_or(0) as u32;
    let mut runs = String::new();
    let mut run: Option<(u32, u32, u32)> = None;
    for code_point in from..=to {
        let Some(c) = char::from_u32(code_point) else {
            continue;
        };
        let code = width_probe(c);
        match &mut run {
            Some((_, last, current)) if *current == code && *last + 1 == code_point => *last = code_point,
            _ => {
                if let Some((first, last, current)) = run {
                    write!(runs, "{first:x}-{last:x}:{current},").unwrap();
                }
                run = Some((code_point, code_point, code));
            }
        }
    }
    if let Some((first, last, current)) = run {
        write!(runs, "{first:x}-{last:x}:{current},").unwrap();
    }
    json!({ "runs": runs })
}
