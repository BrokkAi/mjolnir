//! Terminal backend wrapper that tracks the cursor position locally so
//! ratatui never needs a CPR (`ESC[6n`) roundtrip while the UI is running.
//!
//! crossterm's `cursor::position()` blocks on the same internal reader lock
//! the async `EventStream` holds while it waits for input. With the stream
//! idle in a blocking poll, every CPR query times out after two seconds and
//! the reply is only delivered once a real key or mouse event releases the
//! lock, which blanks the view until the next input event.
//!
//! The wrapper mirrors ratatui's own `last_known_cursor_pos` bookkeeping:
//! every cursor movement ratatui performs goes through `set_cursor_position`,
//! `draw`, or `append_lines`, so the tracked position stays accurate without
//! asking the terminal. The first `get_cursor_position` on an unseeded
//! backend still performs one real query (during terminal setup, before the
//! event stream contends for the lock); afterwards the answer always comes
//! from memory.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};

/// Center a `width` x `height` rectangle within `area`, clamping each dimension
/// to fit. Shared by the modal overlays and the first-run pickers.
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

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
    /// moved the cursor themselves.
    #[cfg(test)]
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
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn centered_rect_centers_and_clamps_to_offset_area() {
        let area = Rect::new(10, 20, 80, 40);
        assert_eq!(centered_rect(area, 20, 10), Rect::new(40, 35, 20, 10));
        assert_eq!(centered_rect(area, 100, 50), area);
        assert_eq!(
            centered_rect(Rect::new(3, 4, 0, 0), 10, 10),
            Rect::new(3, 4, 0, 0)
        );
    }

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

    #[test]
    fn empty_draw_keeps_the_previous_cursor_position() {
        let initial = Position { x: 4, y: 6 };
        let mut backend = TrackedBackend::with_cursor_position(Vec::new(), initial);
        backend
            .draw(std::iter::empty::<(u16, u16, &Cell)>())
            .expect("empty draw");
        assert_eq!(backend.get_cursor_position().unwrap(), initial);
    }

    #[test]
    fn append_lines_advances_known_cursor_and_zero_is_a_noop() {
        let mut backend = TrackedBackend::with_cursor_position(Vec::new(), Position { x: 3, y: 7 });

        backend.append_lines(0).expect("append zero lines");
        assert_eq!(
            backend.get_cursor_position().unwrap(),
            Position { x: 3, y: 7 }
        );

        backend.append_lines(5).expect("append lines");
        assert_eq!(backend.get_cursor_position().unwrap().x, 3);
        assert!(backend.get_cursor_position().unwrap().y >= 7);
    }

    #[test]
    fn append_lines_saturates_and_clamps_to_terminal_height() {
        let mut backend = TrackedBackend::with_cursor_position(
            Vec::new(),
            Position {
                x: 2,
                y: u16::MAX - 1,
            },
        );
        let expected_y = Backend::size(&backend)
            .map(|size| size.height.saturating_sub(1))
            .unwrap_or(u16::MAX);

        backend.append_lines(10).expect("append lines");

        assert_eq!(backend.get_cursor_position().unwrap().x, 2);
        assert_eq!(backend.get_cursor_position().unwrap().y, expected_y);
    }

    #[test]
    fn write_and_backend_commands_reach_the_inner_writer() {
        let writer = SharedWriter::default();
        let mut backend = TrackedBackend::with_cursor_position(writer.clone(), Position::ORIGIN);
        Write::write_all(&mut backend, b"literal").expect("write");
        Write::flush(&mut backend).expect("writer flush");
        backend.hide_cursor().expect("hide cursor");
        backend.show_cursor().expect("show cursor");
        backend.clear().expect("clear screen");
        backend
            .clear_region(ClearType::AfterCursor)
            .expect("clear region");
        Backend::flush(&mut backend).expect("backend flush");

        let output = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
        assert!(output.starts_with("literal"));
        assert!(output.contains("\x1b[?25l"));
        assert!(output.contains("\x1b[?25h"));
        assert!(output.contains("\x1b[2J"));
        assert!(output.contains("\x1b[J"));
    }
}
