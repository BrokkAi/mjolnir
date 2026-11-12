//! Login terminal rendering and terminal keyboard encoding.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    widgets::{Block, Paragraph, Wrap},
};

#[derive(Default)]
struct Replies(Vec<u8>);
impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        prefix: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        let parameter = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (prefix, command, parameter) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.0
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (None, 'n', 5) => self.0.extend_from_slice(b"\x1b[0n"),
            (None, 'c', 0) => self.0.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

/// A terminal pane owns emulation; the caller owns its disposable connection.
pub struct LoginPane {
    parser: vt100::Parser<Replies>,
    profile: String,
    status: String,
}
impl LoginPane {
    pub fn new(profile: &str, cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows.max(1),
                cols.max(1),
                0,
                Replies::default(),
            ),
            profile: profile.into(),
            status: "Connecting to the daemon…".into(),
        }
    }
    /// The bordered pane reserves three footer lines for status and controls.
    pub fn terminal_size(width: u16, height: u16) -> (u16, u16) {
        (
            width.saturating_sub(2).max(1),
            height.saturating_sub(5).max(1),
        )
    }
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }
    pub fn status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }
    pub fn output(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.process(bytes);
        std::mem::take(&mut self.parser.callbacks_mut().0)
    }
    pub fn paste(&self, text: &str) -> Vec<u8> {
        if self.parser.screen().bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.as_bytes().to_vec()
        }
    }
    pub fn key(&self, key: KeyEvent) -> Option<Vec<u8>> {
        let modifier = 1
            + u8::from(key.modifiers.contains(KeyModifiers::SHIFT))
            + 2 * u8::from(key.modifiers.contains(KeyModifiers::ALT))
            + 4 * u8::from(key.modifiers.contains(KeyModifiers::CONTROL));
        let cursor = |end: char, ss3: bool| {
            if modifier > 1 {
                format!("\x1b[1;{modifier}{end}").into_bytes()
            } else {
                format!("\x1b{}{end}", if ss3 { 'O' } else { '[' }).into_bytes()
            }
        };
        let tilde = |number: u8| {
            if modifier > 1 {
                format!("\x1b[{number};{modifier}~").into_bytes()
            } else {
                format!("\x1b[{number}~").into_bytes()
            }
        };
        let mut bytes = match key.code {
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && (c.is_ascii_alphabetic()
                        || matches!(c, '@' | '[' | '\\' | ']' | '^' | '_' | ' ' | '?')) =>
            {
                vec![if c == '?' { 127 } else { (c as u8) & 31 }]
            }
            KeyCode::Char(c) => c.to_string().into_bytes(),
            KeyCode::Enter => vec![b'\r'],
            KeyCode::Tab => vec![b'\t'],
            KeyCode::BackTab => b"\x1b[Z".to_vec(),
            KeyCode::Backspace => vec![127],
            KeyCode::Esc => vec![27],
            KeyCode::Null => vec![0],
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Right
            | KeyCode::Left
            | KeyCode::Home
            | KeyCode::End => {
                let end = match key.code {
                    KeyCode::Up => 'A',
                    KeyCode::Down => 'B',
                    KeyCode::Right => 'C',
                    KeyCode::Left => 'D',
                    KeyCode::Home => 'H',
                    _ => 'F',
                };
                return Some(cursor(end, self.parser.screen().application_cursor()));
            }
            KeyCode::Insert => return Some(tilde(2)),
            KeyCode::Delete => return Some(tilde(3)),
            KeyCode::PageUp => return Some(tilde(5)),
            KeyCode::PageDown => return Some(tilde(6)),
            KeyCode::F(n @ 1..=4) => return Some(cursor(char::from(b'P' + n - 1), true)),
            KeyCode::F(n @ 5..=12) => {
                return Some(tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]));
            }
            _ => return None,
        };
        if key.modifiers.contains(KeyModifiers::ALT) {
            bytes.insert(0, 27);
        }
        Some(bytes)
    }
    pub fn render(&self, frame: &mut Frame<'_>) {
        let block = Block::bordered().title(format!(" Login · {} ", self.profile));
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());
        let areas = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
        frame.render_widget(
            tui_term::widget::PseudoTerminal::new(self.parser.screen()),
            areas[0],
        );
        frame.render_widget(
            Paragraph::new(self.status.as_str()).wrap(Wrap { trim: false }),
            areas[1],
        );
        frame.render_widget(Paragraph::new("Ctrl+] closes · Ctrl+Q quits"), areas[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn login_terminal_rendering_preserves_fragmented_ansi_and_reports_completion() {
        let mut terminal = Terminal::new(TestBackend::new(76, 12)).unwrap();
        let (cols, rows) = LoginPane::terminal_size(76, 12);
        let mut pane = LoginPane::new("fixture", cols, rows);
        let mut rendered = String::new();
        terminal.draw(|frame| pane.render(frame)).unwrap();
        rendered
            .push_str(&crate::test_support::buffer_lines(terminal.backend().buffer()).join("\n"));
        rendered.push_str("\n\n");
        pane.status("Signed in. Enter returns to Settings.");
        for chunk in [
            b"Open https://example.test/login\r\n\x1b[3".as_slice(),
            b"2mCode: \x1b[0m\xe2\x9c",
            b"\x93\r\nSaved credentials.\x1b[6n",
        ] {
            pane.output(chunk);
        }
        terminal.draw(|frame| pane.render(frame)).unwrap();
        rendered
            .push_str(&crate::test_support::buffer_lines(terminal.backend().buffer()).join("\n"));
        rendered.push_str("\n\n");
        pane.status("The daemon connection ended during login or harness preparation; retry Login. Enter returns to Settings.");
        terminal.draw(|frame| pane.render(frame)).unwrap();
        rendered
            .push_str(&crate::test_support::buffer_lines(terminal.backend().buffer()).join("\n"));
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "login-terminal", &rendered);
    }
}
