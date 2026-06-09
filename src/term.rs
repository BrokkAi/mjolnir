//! Terminal backend wrapper that tracks the cursor position locally so
//! ratatui never needs a CPR (`ESC[6n`) roundtrip while the UI is running.
//!
//! crossterm's `cursor::position()` blocks on the same internal reader lock
//! the async `EventStream` holds while it waits for input. With the stream
//! idle in a blocking poll, every CPR query times out after two seconds and
//! the reply is only delivered once a real key or mouse event releases the
//! lock. ratatui queries the cursor position whenever an inline viewport is
//! created or resized, so resizing the inline viewport (for example when a
//! permission modal opens) blanked the view until the next input event.
//!
//! The wrapper mirrors ratatui's own `last_known_cursor_pos` bookkeeping:
//! every cursor movement ratatui performs goes through `set_cursor_position`,
//! `draw`, or `append_lines`, so the tracked position stays accurate without
//! asking the terminal. The first `get_cursor_position` on an unseeded
//! backend still performs one real query (initial inline setup, before the
//! event stream contends for the lock); afterwards the answer always comes
//! from memory.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

#[derive(Debug)]
pub struct TrackedBackend<W: Write> {
    inner: CrosstermBackend<W>,
    cursor_pos: Option<Position>,
}

impl<W: Write> TrackedBackend<W> {
    /// Backend with an unknown cursor position; the first
    /// `get_cursor_position` call queries the terminal once.
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            cursor_pos: None,
        }
    }

    /// Backend seeded with a known cursor position, for callers that just
    /// moved the cursor themselves (e.g. an inline viewport resize).
    pub fn with_cursor_position(writer: W, position: Position) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            cursor_pos: Some(position),
        }
    }
}

impl<W: Write> Write for TrackedBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for TrackedBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut last: Option<Position> = None;
        let result = self
            .inner
            .draw(content.inspect(|(x, y, _)| last = Some(Position { x: *x, y: *y })));
        if result.is_ok()
            && let Some(pos) = last
        {
            self.cursor_pos = Some(pos);
        }
        result
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        if let Some(pos) = self.cursor_pos {
            return Ok(pos);
        }
        let pos = self.inner.get_cursor_position()?;
        self.cursor_pos = Some(pos);
        Ok(pos)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.cursor_pos = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)?;
        // Line feeds move the cursor down (clamped at the bottom row, where
        // the screen scrolls instead) and leave the column unchanged in raw
        // mode. Without the screen height, advance unclamped; ratatui's
        // inline-size math only compares rows that fit on screen.
        if n > 0
            && let Some(pos) = self.cursor_pos.as_mut()
        {
            let next = pos.y.saturating_add(n);
            pos.y = match self.inner.size() {
                Ok(size) => next.min(size.height.saturating_sub(1)),
                Err(_) => next,
            };
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_cursor_position_is_returned_without_terminal_query() {
        let mut backend =
            TrackedBackend::with_cursor_position(Vec::new(), Position { x: 0, y: 40 });
        let pos = backend.get_cursor_position().expect("tracked position");
        assert_eq!(pos, Position { x: 0, y: 40 });
    }

    #[test]
    fn set_cursor_position_updates_tracked_position() {
        let mut backend = TrackedBackend::with_cursor_position(Vec::new(), Position::ORIGIN);
        backend
            .set_cursor_position(Position { x: 3, y: 17 })
            .expect("set cursor");
        let pos = backend.get_cursor_position().expect("tracked position");
        assert_eq!(pos, Position { x: 3, y: 17 });
    }

    #[test]
    fn draw_tracks_last_drawn_cell_like_ratatui() {
        let mut backend = TrackedBackend::with_cursor_position(Vec::new(), Position::ORIGIN);
        let cell = Cell::new("x");
        let content = [(2u16, 5u16, &cell), (7u16, 9u16, &cell)];
        backend.draw(content.into_iter()).expect("draw");
        let pos = backend.get_cursor_position().expect("tracked position");
        assert_eq!(pos, Position { x: 7, y: 9 });
    }
}
