//! Safe, stateful normalization for untrusted terminal byte streams.
//!
//! ACP terminals can emit a terminal control plane rather than ordinary log
//! lines. This module consumes that control plane into a bounded text screen;
//! callers only receive the resulting printable text, never escape sequences.

use std::collections::VecDeque;

const MIN_SCREEN_ROWS: usize = 256;
const MAX_SCREEN_ROWS: usize = 16_384;
const MIN_SCREEN_COLUMNS: usize = 256;
const MAX_SCREEN_COLUMNS: usize = 8_192;
const MAX_SCREEN_CELLS: usize = 4 * 1024 * 1024;
const MAX_CONTROL_SEQUENCE_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    String,
    StringEscape,
}

/// Incremental terminal parser backed by a bounded virtual text screen.
#[derive(Debug)]
pub struct TerminalText {
    state: ParserState,
    csi: String,
    utf8_pending: Vec<u8>,
    lines: VecDeque<Vec<char>>,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    max_rows: usize,
    max_columns: usize,
    max_cells: usize,
    cells: usize,
    truncated: bool,
}

impl TerminalText {
    pub fn new(output_limit: usize) -> Self {
        let max_rows = (output_limit / 32).clamp(MIN_SCREEN_ROWS, MAX_SCREEN_ROWS);
        let max_columns = output_limit.clamp(MIN_SCREEN_COLUMNS, MAX_SCREEN_COLUMNS);
        Self {
            state: ParserState::Ground,
            csi: String::new(),
            utf8_pending: Vec::with_capacity(4),
            lines: VecDeque::from([Vec::new()]),
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            max_rows,
            max_columns,
            max_cells: output_limit.clamp(MIN_SCREEN_COLUMNS, MAX_SCREEN_CELLS),
            cells: 0,
            truncated: false,
        }
    }

    pub fn reset(&mut self) {
        self.state = ParserState::Ground;
        self.csi.clear();
        self.utf8_pending.clear();
        self.lines.clear();
        self.lines.push_back(Vec::new());
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.saved_cursor = None;
        self.cells = 0;
        self.truncated = false;
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.push_byte(byte);
        }
    }

    /// Finish a complete stream. Partial UTF-8 becomes one replacement
    /// character; partial terminal control sequences are discarded safely.
    pub fn finish(&mut self) {
        self.drain_utf8(true);
        self.state = ParserState::Ground;
        self.csi.clear();
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn render(&self) -> String {
        let mut lines = self
            .lines
            .iter()
            .map(|line| {
                let end = line
                    .iter()
                    .rposition(|ch| *ch != ' ')
                    .map_or(0, |index| index + 1);
                line[..end].iter().collect::<String>()
            })
            .collect::<Vec<_>>();
        let first = lines.iter().position(|line| !line.is_empty());
        let last = lines.iter().rposition(|line| !line.is_empty());
        match (first, last) {
            (Some(first), Some(last)) => lines.drain(first..=last).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if !self.utf8_pending.is_empty() {
            self.utf8_pending.push(byte);
            self.drain_utf8(false);
            return;
        }
        match byte {
            0x00..=0x7f => self.push_char(char::from(byte)),
            // Raw 8-bit C1 controls occur in terminal streams even though
            // they are not valid standalone UTF-8 bytes.
            0x80..=0x9f => self.push_char(char::from_u32(u32::from(byte)).unwrap_or('\u{fffd}')),
            _ => {
                self.utf8_pending.push(byte);
                self.drain_utf8(false);
            }
        }
    }

    fn drain_utf8(&mut self, finalize: bool) {
        loop {
            if self.utf8_pending.is_empty() {
                return;
            }
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(text) => {
                    let text = text.to_string();
                    self.utf8_pending.clear();
                    for ch in text.chars() {
                        self.push_char(ch);
                    }
                    return;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        let valid =
                            String::from_utf8_lossy(&self.utf8_pending[..valid_up_to]).into_owned();
                        self.utf8_pending.drain(..valid_up_to);
                        for ch in valid.chars() {
                            self.push_char(ch);
                        }
                        continue;
                    }
                    if let Some(error_len) = error.error_len() {
                        self.utf8_pending.drain(..error_len);
                        self.push_char('\u{fffd}');
                        continue;
                    }
                    if finalize {
                        self.utf8_pending.clear();
                        self.push_char('\u{fffd}');
                    }
                    return;
                }
            }
        }
    }

    fn push_char(&mut self, ch: char) {
        match self.state {
            ParserState::Ground => self.ground(ch),
            ParserState::Escape => self.escape(ch),
            ParserState::EscapeIntermediate => {
                if ch == '\u{1b}' {
                    self.state = ParserState::Escape;
                } else if ('\u{30}'..='\u{7e}').contains(&ch) {
                    self.state = ParserState::Ground;
                }
            }
            ParserState::Csi => self.csi(ch),
            ParserState::Osc => self.control_string(ch, true, false),
            ParserState::OscEscape => self.control_string(ch, true, true),
            ParserState::String => self.control_string(ch, false, false),
            ParserState::StringEscape => self.control_string(ch, false, true),
        }
    }

    fn ground(&mut self, ch: char) {
        match ch {
            '\u{1b}' => self.state = ParserState::Escape,
            '\u{9b}' => self.start_csi(),
            '\u{9d}' => self.state = ParserState::Osc,
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => self.state = ParserState::String,
            '\u{84}' => self.index(),
            '\u{85}' => self.newline(),
            '\u{8d}' => self.reverse_index(),
            '\u{00}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}' => self.execute_control(ch),
            _ if ch.is_control() => {}
            _ => self.put(ch),
        }
    }

    fn execute_control(&mut self, ch: char) {
        match ch {
            '\u{08}' => self.cursor_col = self.cursor_col.saturating_sub(1),
            '\u{09}' => {
                self.cursor_col = ((self.cursor_col / 8) + 1)
                    .saturating_mul(8)
                    .min(self.max_columns.saturating_sub(1));
            }
            '\u{0a}' | '\u{0b}' | '\u{0c}' => self.newline(),
            '\u{0d}' => self.cursor_col = 0,
            _ => {}
        }
    }

    fn escape(&mut self, ch: char) {
        match ch {
            '\u{1b}' => {}
            '[' => self.start_csi(),
            ']' => self.state = ParserState::Osc,
            'P' | 'X' | '^' | '_' => self.state = ParserState::String,
            '7' => {
                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
                self.state = ParserState::Ground;
            }
            '8' => {
                self.restore_cursor();
                self.state = ParserState::Ground;
            }
            'D' => {
                self.index();
                self.state = ParserState::Ground;
            }
            'E' => {
                self.newline();
                self.state = ParserState::Ground;
            }
            'M' => {
                self.reverse_index();
                self.state = ParserState::Ground;
            }
            'c' => self.reset(),
            '\u{20}'..='\u{2f}' => self.state = ParserState::EscapeIntermediate,
            '\u{30}'..='\u{7e}' => self.state = ParserState::Ground,
            _ => self.state = ParserState::Ground,
        }
    }

    fn start_csi(&mut self) {
        self.csi.clear();
        self.state = ParserState::Csi;
    }

    fn csi(&mut self, ch: char) {
        match ch {
            '\u{1b}' => {
                self.csi.clear();
                self.state = ParserState::Escape;
            }
            '\u{9c}' => {
                self.csi.clear();
                self.state = ParserState::Ground;
            }
            '\u{40}'..='\u{7e}' => {
                let sequence = std::mem::take(&mut self.csi);
                self.state = ParserState::Ground;
                self.execute_csi(&sequence, ch);
            }
            '\u{20}'..='\u{3f}' if self.csi.len() < MAX_CONTROL_SEQUENCE_CHARS => {
                self.csi.push(ch);
            }
            '\u{00}'..='\u{1f}' => self.execute_control(ch),
            _ => {}
        }
    }

    fn control_string(&mut self, ch: char, osc: bool, after_escape: bool) {
        if ch == '\u{9c}' || (osc && ch == '\u{07}') || (after_escape && ch == '\\') {
            self.state = ParserState::Ground;
        } else if ch == '\u{1b}' {
            self.state = if osc {
                ParserState::OscEscape
            } else {
                ParserState::StringEscape
            };
        } else {
            self.state = if osc {
                ParserState::Osc
            } else {
                ParserState::String
            };
        }
    }

    fn execute_csi(&mut self, sequence: &str, final_char: char) {
        let params = csi_params(sequence);
        let count = movement_param(&params, 0);
        match final_char {
            'A' => self.cursor_row = self.cursor_row.saturating_sub(count),
            'B' | 'e' => self.cursor_row = self.clamp_row(self.cursor_row.saturating_add(count)),
            'C' | 'a' => {
                self.cursor_col = self.clamp_col(self.cursor_col.saturating_add(count));
            }
            'D' => self.cursor_col = self.cursor_col.saturating_sub(count),
            'E' => {
                self.cursor_row = self.clamp_row(self.cursor_row.saturating_add(count));
                self.cursor_col = 0;
            }
            'F' => {
                self.cursor_row = self.cursor_row.saturating_sub(count);
                self.cursor_col = 0;
            }
            'G' | '`' => self.cursor_col = self.clamp_col(position_param(&params, 0) - 1),
            'H' | 'f' => {
                self.cursor_row = self.clamp_row(position_param(&params, 0) - 1);
                self.cursor_col = self.clamp_col(position_param(&params, 1) - 1);
            }
            'd' => self.cursor_row = self.clamp_row(position_param(&params, 0) - 1),
            'J' => self.erase_display(raw_param(&params, 0, 0)),
            'K' => self.erase_line(raw_param(&params, 0, 0)),
            'X' => self.erase_chars(count),
            '@' => self.insert_chars(count),
            'P' => self.delete_chars(count),
            'L' => self.insert_lines(count),
            'M' => self.delete_lines(count),
            'S' => self.scroll_up(count),
            'T' => self.scroll_down(count),
            's' => self.saved_cursor = Some((self.cursor_row, self.cursor_col)),
            'u' => self.restore_cursor(),
            // SGR, mode changes, device queries, window operations, and
            // unsupported extensions intentionally have no text effect.
            _ => {}
        }
    }

    fn put(&mut self, ch: char) {
        if self.cursor_col >= self.max_columns {
            self.newline();
        }
        self.ensure_row(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        let old_len = line.len();
        if line.len() <= self.cursor_col {
            line.resize(self.cursor_col + 1, ' ');
        }
        line[self.cursor_col] = ch;
        self.cells += line.len() - old_len;
        self.cursor_col = self.cursor_col.saturating_add(1);
        self.bound_cells();
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.index();
    }

    fn index(&mut self) {
        self.cursor_row = self.cursor_row.saturating_add(1);
        if self.cursor_row >= self.max_rows {
            if let Some(line) = self.lines.pop_front() {
                self.cells = self.cells.saturating_sub(line.len());
            }
            self.lines.push_back(Vec::new());
            self.cursor_row = self.max_rows - 1;
            if let Some((row, col)) = self.saved_cursor {
                self.saved_cursor = Some((row.saturating_sub(1), col));
            }
            self.truncated = true;
        } else {
            self.ensure_row(self.cursor_row);
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
        } else {
            self.lines.push_front(Vec::new());
            if self.lines.len() > self.max_rows
                && let Some(line) = self.lines.pop_back()
            {
                self.cells = self.cells.saturating_sub(line.len());
            }
        }
    }

    fn ensure_row(&mut self, row: usize) {
        let row = self.clamp_row(row);
        while self.lines.len() <= row {
            self.lines.push_back(Vec::new());
        }
    }

    fn clamp_row(&self, row: usize) -> usize {
        row.min(self.max_rows - 1)
    }

    fn clamp_col(&self, col: usize) -> usize {
        col.min(self.max_columns - 1)
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.cursor_row = self.clamp_row(row);
            self.cursor_col = self.clamp_col(col);
        }
    }

    fn erase_line(&mut self, mode: usize) {
        self.ensure_row(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        let old_len = line.len();
        match mode {
            1 => {
                let end = self.cursor_col.min(line.len().saturating_sub(1));
                for ch in line.iter_mut().take(end.saturating_add(1)) {
                    *ch = ' ';
                }
            }
            2 => line.clear(),
            _ => line.truncate(self.cursor_col.min(line.len())),
        }
        self.cells = self.cells.saturating_sub(old_len - line.len());
    }

    fn erase_display(&mut self, mode: usize) {
        self.ensure_row(self.cursor_row);
        match mode {
            1 => {
                for row in 0..self.cursor_row {
                    self.cells = self.cells.saturating_sub(self.lines[row].len());
                    self.lines[row].clear();
                }
                let line = &mut self.lines[self.cursor_row];
                let end = self.cursor_col.min(line.len().saturating_sub(1));
                for ch in line.iter_mut().take(end.saturating_add(1)) {
                    *ch = ' ';
                }
            }
            2 | 3 => {
                for line in &mut self.lines {
                    self.cells = self.cells.saturating_sub(line.len());
                    line.clear();
                }
            }
            _ => {
                let line = &mut self.lines[self.cursor_row];
                let old_len = line.len();
                line.truncate(self.cursor_col.min(line.len()));
                self.cells = self.cells.saturating_sub(old_len - line.len());
                for row in self.cursor_row + 1..self.lines.len() {
                    self.cells = self.cells.saturating_sub(self.lines[row].len());
                    self.lines[row].clear();
                }
            }
        }
    }

    fn erase_chars(&mut self, count: usize) {
        self.ensure_row(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        for index in self.cursor_col..self.cursor_col.saturating_add(count).min(line.len()) {
            line[index] = ' ';
        }
    }

    fn insert_chars(&mut self, count: usize) {
        self.ensure_row(self.cursor_row);
        let max_columns = self.max_columns;
        let count = count.min(max_columns);
        let line = &mut self.lines[self.cursor_row];
        let old_len = line.len();
        let position = self.cursor_col.min(line.len());
        line.splice(position..position, std::iter::repeat_n(' ', count));
        line.truncate(max_columns);
        self.cells = self.cells.saturating_add(line.len() - old_len);
        self.bound_cells();
    }

    fn delete_chars(&mut self, count: usize) {
        self.ensure_row(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        let old_len = line.len();
        let start = self.cursor_col.min(line.len());
        let end = start.saturating_add(count).min(line.len());
        line.drain(start..end);
        self.cells = self.cells.saturating_sub(old_len - line.len());
    }

    fn insert_lines(&mut self, count: usize) {
        self.ensure_row(self.cursor_row);
        let count = count.min(self.max_rows);
        let mut tail = self.lines.split_off(self.cursor_row);
        self.lines
            .extend(std::iter::repeat_with(Vec::new).take(count));
        self.lines.append(&mut tail);
        while self.lines.len() > self.max_rows {
            if let Some(line) = self.lines.pop_back() {
                self.cells = self.cells.saturating_sub(line.len());
            }
        }
    }

    fn delete_lines(&mut self, count: usize) {
        self.ensure_row(self.cursor_row);
        let end = self.cursor_row.saturating_add(count).min(self.lines.len());
        let removed = end.saturating_sub(self.cursor_row);
        for line in self.lines.drain(self.cursor_row..end) {
            self.cells = self.cells.saturating_sub(line.len());
        }
        self.lines
            .extend(std::iter::repeat_with(Vec::new).take(removed));
    }

    fn scroll_up(&mut self, count: usize) {
        for _ in 0..count.min(self.lines.len()) {
            if let Some(line) = self.lines.pop_front() {
                self.cells = self.cells.saturating_sub(line.len());
            }
            self.lines.push_back(Vec::new());
        }
    }

    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count.min(self.max_rows) {
            self.lines.push_front(Vec::new());
            if self.lines.len() > self.max_rows
                && let Some(line) = self.lines.pop_back()
            {
                self.cells = self.cells.saturating_sub(line.len());
            }
        }
    }

    fn bound_cells(&mut self) {
        while self.cells > self.max_cells && self.lines.len() > 1 {
            let line = self.lines.pop_front().expect("terminal has a front row");
            self.cells = self.cells.saturating_sub(line.len());
            self.cursor_row = self.cursor_row.saturating_sub(1);
            if let Some((row, col)) = self.saved_cursor {
                self.saved_cursor = Some((row.saturating_sub(1), col));
            }
            self.truncated = true;
        }
        if self.cells > self.max_cells {
            let overflow = self.cells - self.max_cells;
            let line = &mut self.lines[0];
            let drain_end = overflow.min(line.len());
            line.drain(..drain_end);
            self.cells -= drain_end;
            self.cursor_col = self.cursor_col.saturating_sub(drain_end);
            if let Some((row, col)) = self.saved_cursor {
                self.saved_cursor = Some((row, col.saturating_sub(drain_end)));
            }
            self.truncated = true;
        }
    }
}

fn csi_params(sequence: &str) -> Vec<Option<usize>> {
    let parameters = sequence
        .trim_start_matches(['?', '>', '<', '='])
        .split_once(|ch: char| ('\u{20}'..='\u{2f}').contains(&ch))
        .map_or(
            sequence.trim_start_matches(['?', '>', '<', '=']),
            |(head, _)| head,
        );
    if parameters.is_empty() {
        return Vec::new();
    }
    parameters
        .split(';')
        .map(|value| value.split(':').next().and_then(|value| value.parse().ok()))
        .collect()
}

fn raw_param(params: &[Option<usize>], index: usize, default: usize) -> usize {
    params
        .get(index)
        .and_then(|value| *value)
        .unwrap_or(default)
}

fn movement_param(params: &[Option<usize>], index: usize) -> usize {
    raw_param(params, index, 1).max(1)
}

fn position_param(params: &[Option<usize>], index: usize) -> usize {
    raw_param(params, index, 1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(chunks: &[&[u8]]) -> (String, bool) {
        let mut terminal = TerminalText::new(4096);
        for chunk in chunks {
            terminal.push(chunk);
        }
        terminal.finish();
        (terminal.render(), terminal.truncated())
    }

    fn assert_safe(text: &str) {
        assert!(!text.contains('\u{1b}'), "escape leaked: {text:?}");
        assert!(
            text.chars().all(|ch| ch == '\n' || !ch.is_control()),
            "control leaked: {text:?}"
        );
    }

    #[test]
    fn strips_complete_sgr_osc_dcs_and_private_mode_sequences() {
        let (text, _) = normalized(&[concat!(
            "plain\x1b[1;31m red\x1b[0m",
            "\x1b]0;hostile title\x07",
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            "\x1bPignored payload\x1b\\",
            "\x1b[?2004h\x1b[?25l done\x1b[?25h"
        )
        .as_bytes()]);

        assert_eq!(text, "plain redlink done");
        assert_safe(&text);
    }

    #[test]
    fn split_sequences_and_split_utf8_are_stateful() {
        let (text, _) = normalized(&[
            b"safe \x1b[3",
            b"1mred\x1b[0",
            b"m ",
            &[0xc3],
            &[0xa9],
            b"\x1b]0;split",
            b" title\x1b",
            b"\\ tail",
        ]);

        assert_eq!(text, "safe red é tail");
        assert_safe(&text);
    }

    #[test]
    fn cursor_addressing_carriage_return_backspace_and_erase_resolve_to_text() {
        let (text, _) = normalized(&[concat!(
            "progress 10%\rprogress 90%",
            "\nold value\rnew\x1b[K",
            "\nabcXX\x08\x08de",
            "\x1b[5;3Hplaced",
            "\x1b[4;1Habove"
        )
        .as_bytes()]);

        assert_eq!(text, "progress 90%\nnew\nabcde\nabove\n  placed");
        assert_safe(&text);
    }

    #[test]
    fn raw_and_utf8_encoded_c1_controls_are_consumed_atomically() {
        let (text, _) = normalized(&[b"a\x9b31mb", "\u{9b}32mc\u{9d}title\u{9c}d".as_bytes()]);

        assert_eq!(text, "abcd");
        assert_safe(&text);
    }

    #[test]
    fn incomplete_and_malformed_sequences_fail_closed() {
        let (text, _) = normalized(&[b"before\xffafter\x1b[31"]);

        assert_eq!(text, "before�after");
        assert_safe(&text);
    }

    #[test]
    fn reset_clears_prior_screen_and_parser_state() {
        let mut terminal = TerminalText::new(4096);
        terminal.push(b"old\x1b[31");
        terminal.reset();
        terminal.push(b"new");
        terminal.finish();

        assert_eq!(terminal.render(), "new");
    }

    #[test]
    fn bounded_screen_reports_scrollback_truncation() {
        let mut terminal = TerminalText::new(1);
        for _ in 0..300 {
            terminal.push(b"line\n");
        }
        terminal.finish();

        assert!(terminal.truncated());
        assert_eq!(terminal.render().lines().count(), MIN_SCREEN_COLUMNS / 4);
    }

    #[test]
    fn sparse_cursor_writes_stay_within_the_cell_budget() {
        let mut terminal = TerminalText::new(256);
        for row in 1..=300 {
            terminal.push(format!("\x1b[{row};256HX").as_bytes());
        }
        terminal.finish();

        assert!(terminal.truncated());
        assert!(terminal.cells <= terminal.max_cells);
        assert!(terminal.render().contains('X'));
    }

    #[test]
    fn huge_csi_counts_do_not_expand_or_loop_without_bound() {
        let (text, truncated) = normalized(&[concat!(
            "start",
            "\x1b[18446744073709551615@",
            "X",
            "\x1b[18446744073709551615L",
            "\x1b[18446744073709551615M",
            "end"
        )
        .as_bytes()]);

        assert!(text.ends_with("end"));
        assert!(!truncated);
        assert_safe(&text);
    }
}
