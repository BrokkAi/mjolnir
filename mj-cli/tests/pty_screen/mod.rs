//! The text a terminal user sees, projected from the bytes a PTY carries.
//!
//! Shared by the Unix PTY tests and the Windows ConPTY smoke. ConPTY re-renders
//! the console instead of passing the child's bytes through, so tests on both
//! platforms match what is visible rather than particular escape sequences.

// Each test binary that includes this module uses a different part of it.
#![allow(dead_code)]

pub const PTY_ROWS: usize = 24;
pub const PTY_COLUMNS: usize = 80;

/// Keep the terminal's current cells independently of the captured byte log.
/// Ratatui can retain an unchanged space and move the cursor over it, so the
/// words visible together on screen need not be contiguous in the PTY stream.
pub struct PtyOutput {
    raw: Vec<u8>,
    parser: vte::Parser,
    pub screen: PtyScreen,
}

impl PtyOutput {
    pub fn new() -> Self {
        Self {
            raw: Vec::new(),
            parser: vte::Parser::new(),
            screen: PtyScreen::new(),
        }
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        self.parser.advance(&mut self.screen, bytes);
    }

    /// Start a new raw capture without forgetting unchanged screen cells.
    pub fn clear(&mut self) {
        self.raw.clear();
    }
}

impl std::ops::Deref for PtyOutput {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

pub struct PtyScreen {
    cells: Vec<Vec<String>>,
    row: usize,
    column: usize,
    saved_cursor: (usize, usize),
}

impl PtyScreen {
    fn new() -> Self {
        Self {
            cells: vec![vec![" ".into(); PTY_COLUMNS]; PTY_ROWS],
            row: 0,
            column: 0,
            saved_cursor: (0, 0),
        }
    }

    pub fn contents(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn newline(&mut self) {
        if self.row + 1 == PTY_ROWS {
            self.cells.remove(0);
            self.cells.push(vec![" ".into(); PTY_COLUMNS]);
        } else {
            self.row += 1;
        }
    }
}

/// Interpret the cursor and erasure operations emitted by the crossterm
/// backend. VTE owns escape/UTF-8 parsing, including sequences split between
/// reads; styles and input-mode changes do not affect the text projection.
impl vte::Perform for PtyScreen {
    fn print(&mut self, character: char) {
        let text = character.to_string();
        let width = ratatui::text::Span::raw(text.clone()).width();
        if width == 0 {
            if self.column > 0 {
                self.cells[self.row][self.column.min(PTY_COLUMNS) - 1].push(character);
            }
            return;
        }
        if self.column + width > PTY_COLUMNS {
            self.newline();
            self.column = 0;
        }
        self.cells[self.row][self.column] = text;
        for column in self.column + 1..self.column + width {
            self.cells[self.row][column].clear();
        }
        self.column += width;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.column = 0,
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\x08' => self.column = self.column.saturating_sub(1),
            b'\t' => self.column = ((self.column / 8 + 1) * 8).min(PTY_COLUMNS - 1),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _: bool, action: char) {
        let values = params
            .iter()
            .map(|value| value[0] as usize)
            .collect::<Vec<_>>();
        let value = |index: usize| values.get(index).copied().unwrap_or(0);
        let distance = value(0).max(1);
        if !intermediates.is_empty() {
            if intermediates == b"?" && matches!(action, 'h' | 'l') && values.contains(&1049) {
                *self = Self::new();
            }
            return;
        }
        match action {
            'H' | 'f' => {
                self.row = value(0).max(1).saturating_sub(1).min(PTY_ROWS - 1);
                self.column = value(1).max(1).saturating_sub(1).min(PTY_COLUMNS - 1);
            }
            'A' => self.row = self.row.saturating_sub(distance),
            'B' | 'e' => self.row = (self.row + distance).min(PTY_ROWS - 1),
            'C' | 'a' => self.column = (self.column + distance).min(PTY_COLUMNS - 1),
            'D' => self.column = self.column.saturating_sub(distance),
            'E' | 'F' => {
                self.row = if action == 'E' {
                    (self.row + distance).min(PTY_ROWS - 1)
                } else {
                    self.row.saturating_sub(distance)
                };
                self.column = 0;
            }
            'G' | '`' => self.column = distance.saturating_sub(1).min(PTY_COLUMNS - 1),
            'd' => self.row = distance.saturating_sub(1).min(PTY_ROWS - 1),
            'J' | 'K' => {
                let cursor = self.row * PTY_COLUMNS + self.column.min(PTY_COLUMNS - 1);
                for (row_index, row) in self.cells.iter_mut().enumerate() {
                    for (column, cell) in row.iter_mut().enumerate() {
                        let position = row_index * PTY_COLUMNS + column;
                        let erase = match value(0) {
                            0 => position >= cursor,
                            1 => position <= cursor,
                            2 | 3 => true,
                            _ => false,
                        };
                        if erase && (action == 'J' || row_index == self.row) {
                            *cell = " ".into();
                        }
                    }
                }
            }
            'X' => {
                let start = self.column.min(PTY_COLUMNS - 1);
                self.cells[self.row][start..(start + distance).min(PTY_COLUMNS)].fill(" ".into());
            }
            's' => self.saved_cursor = (self.row, self.column),
            'u' => (self.row, self.column) = self.saved_cursor,
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _: &[u8], _: bool, byte: u8) {
        match byte {
            b'7' => self.saved_cursor = (self.row, self.column),
            b'8' => (self.row, self.column) = self.saved_cursor,
            b'c' => *self = Self::new(),
            _ => {}
        }
    }
}
