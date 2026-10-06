//! The terminal backend the dashboard draws through.
//!
//! Ratatui measures every grapheme with `unicode-width` and writes runs of
//! adjacent cells without moving the cursor between them, trusting the
//! terminal to advance by the same width. Terminals do not all agree with that
//! table. Windows Terminal draws `🎙︎` (a pictograph outside the Basic
//! Multilingual Plane with a text variation selector) two columns wide where
//! `unicode-width` says one, and it disagrees about emoji sequences in agent
//! text the same way. Every cell after such a grapheme then lands one column
//! to the right: a row that reaches the right edge wraps onto the next row,
//! and because ratatui only rewrites cells it believes changed, the shifted
//! text stays on screen over the neighbouring pane.
//!
//! [`PositionedBackend`] ends a run after any grapheme whose width terminals
//! may disagree about, so the next cell is placed with an absolute cursor
//! move. A disagreement can then only affect the grapheme's own cell and the
//! one beside it; it can no longer shift the rest of the row.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub(crate) struct PositionedBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> PositionedBackend<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
        }
    }
}

/// Whether terminals may draw `symbol` at a width other than the one ratatui
/// gave it: a grapheme of more than one code point (variation selectors,
/// zero-width joiners, skin tones, flags, combining marks), or a code point
/// outside the Basic Multilingual Plane, where emoji width tables differ.
fn width_uncertain(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    match (chars.next(), chars.next()) {
        (Some(character), None) => u32::from(character) > 0xFFFF,
        (None, _) => false,
        (Some(_), Some(_)) => true,
    }
}

impl<W: Write> Backend for PositionedBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        while content.peek().is_some() {
            // The crossterm backend moves the cursor at the start of every
            // `draw` call, so each run begins at an absolute position.
            let mut run_ended = false;
            let run = std::iter::from_fn(|| {
                if run_ended {
                    return None;
                }
                let item = content.next()?;
                run_ended = width_uncertain(item.2.symbol());
                Some(item)
            });
            self.inner.draw(run)?;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

/// Escape sequences the host writes itself (title, bell, clipboard, input
/// modes) go straight to the terminal.
impl<W: Write> Write for PositionedBackend<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    const COLUMNS: usize = 24;
    const ROWS: usize = 2;

    /// A screen that measures characters the way Windows Terminal does for
    /// this case: variation selectors take no column, and pictographs outside
    /// the Basic Multilingual Plane take two whatever selector follows them.
    struct WideEmojiScreen {
        cells: Vec<Vec<char>>,
        row: usize,
        column: usize,
    }

    impl WideEmojiScreen {
        fn rows(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| row.iter().collect::<String>().trim_end().to_owned())
                .collect()
        }
    }

    impl vte::Perform for WideEmojiScreen {
        fn print(&mut self, character: char) {
            if matches!(character, '\u{FE0E}' | '\u{FE0F}') {
                return;
            }
            let width = if u32::from(character) > 0xFFFF { 2 } else { 1 };
            if self.column + width > COLUMNS {
                self.row = (self.row + 1).min(ROWS - 1);
                self.column = 0;
            }
            self.cells[self.row][self.column] = character;
            if width == 2 {
                self.cells[self.row][self.column + 1] = ' ';
            }
            self.column += width;
        }

        fn csi_dispatch(&mut self, params: &vte::Params, _: &[u8], _: bool, action: char) {
            if action == 'H' {
                let mut values = params.iter().map(|value| usize::from(value[0]));
                self.row = values.next().unwrap_or(1).max(1) - 1;
                self.column = values.next().unwrap_or(1).max(1) - 1;
            }
        }
    }

    fn draw_on_wide_emoji_screen(row: &str) -> Vec<String> {
        let area = Rect::new(0, 0, COLUMNS as u16, ROWS as u16);
        let blank = Buffer::empty(area);
        let mut frame = Buffer::empty(area);
        frame.set_string(0, 0, row, ratatui::style::Style::default());
        frame.set_string(0, 1, "next row", ratatui::style::Style::default());
        let mut output = Vec::new();
        let mut backend = PositionedBackend::new(&mut output);
        backend.draw(blank.diff(&frame).into_iter()).unwrap();
        Backend::flush(&mut backend).unwrap();
        drop(backend);
        let mut screen = WideEmojiScreen {
            cells: vec![vec![' '; COLUMNS]; ROWS],
            row: 0,
            column: 0,
        };
        vte::Parser::new().advance(&mut screen, &output);
        screen.rows()
    }

    // Hard-won: #1243: the composer's 🎙︎ button shifted its border row on
    // Windows Terminal, wrapping the row's last cell into the pane below.
    #[test]
    fn a_grapheme_the_terminal_draws_wider_cannot_shift_the_rest_of_its_row() {
        // Ratatui gives the text-presentation microphone one column; this
        // screen gives it two, so the space after it is overdrawn and
        // everything from the name onward must still land in its own column.
        let rows = draw_on_wide_emoji_screen("╭🎙︎ muse-spark ─────────╮");
        assert_eq!(rows, ["╭🎙 muse-spark ─────────╮", "next row"]);
    }
}
