//! Inline (non-alt-screen) menu prompts.
//!
//! Renders a small arrow-key selector directly into the normal terminal
//! scrollback, for prompts that run after the ratatui alt-screen has been
//! torn down (e.g. post-session worktree cleanup). Returns `None` when
//! stdio is not an interactive terminal so callers can fall back to a
//! plain line-based prompt.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::cursor;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate_text_to_width;

pub struct MenuOption {
    /// Short action name shown in the selector row.
    pub label: &'static str,
    /// Dimmed explanation rendered after the label.
    pub hint: String,
    /// Keys that select this option (matched case-insensitively).
    pub shortcuts: &'static [char],
}

/// What a key press does to the menu; factored out of the event loop so
/// the mapping is unit-testable without a terminal.
#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    Ignored,
    Select(usize),
    Choose(usize),
    Cancel,
}

/// Show `question` with one selectable row per option plus a dimmed
/// `footer` hint line. Arrows/Tab move, Enter confirms, Esc/Ctrl-C picks
/// `initial` (the safe default), and option shortcut keys or digits move the
/// selection. Enter is the only key that confirms a choice. The menu erases
/// itself before returning so callers can print
/// a plain outcome line in its place. Returns `None` without drawing
/// anything when stdin/stdout is not an interactive terminal.
pub fn select_inline(
    question: &str,
    footer: &str,
    options: &[MenuOption],
    initial: usize,
) -> Result<Option<usize>> {
    if options.is_empty() || initial >= options.len() {
        bail!("select_inline needs options and a valid initial index");
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(None);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _guard = RawModeGuard::enable(&mut out)?;

    let result = run_menu(&mut out, question, footer, options, initial);

    // Erase the menu block whether selection succeeded or failed, so the
    // scrollback is left clean for the caller's outcome line.
    let height = menu_height(options.len());
    let _ = queue!(
        out,
        cursor::MoveUp(height),
        Clear(ClearType::FromCursorDown)
    );
    let _ = out.flush();

    result.map(Some)
}

fn run_menu(
    out: &mut impl Write,
    question: &str,
    footer: &str,
    options: &[MenuOption],
    initial: usize,
) -> Result<usize> {
    let mut selected = initial;
    let mut width = terminal::size().map(|(w, _)| w).unwrap_or(80);
    draw(out, question, footer, options, selected, width, false)?;

    loop {
        match read().context("read menu key event")? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key_outcome(key.code, key.modifiers, selected, options) {
                    KeyOutcome::Ignored => continue,
                    KeyOutcome::Select(i) => selected = i,
                    KeyOutcome::Choose(i) => return Ok(i),
                    KeyOutcome::Cancel => return Ok(initial),
                }
            }
            Event::Resize(w, _) => width = w,
            _ => continue,
        }
        draw(out, question, footer, options, selected, width, true)?;
    }
}

fn key_outcome(
    code: KeyCode,
    modifiers: KeyModifiers,
    selected: usize,
    options: &[MenuOption],
) -> KeyOutcome {
    let len = options.len();
    match code {
        KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
            KeyOutcome::Select(selected.checked_sub(1).unwrap_or(len - 1))
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Tab => KeyOutcome::Select((selected + 1) % len),
        KeyCode::Home => KeyOutcome::Select(0),
        KeyCode::End => KeyOutcome::Select(len - 1),
        KeyCode::Enter => KeyOutcome::Choose(selected),
        KeyCode::Esc => KeyOutcome::Cancel,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => KeyOutcome::Cancel,
        KeyCode::Char(c) => {
            if let Some(i) = c.to_digit(10).map(|d| d as usize)
                && (1..=len).contains(&i)
            {
                return KeyOutcome::Select(i - 1);
            }
            let c = c.to_ascii_lowercase();
            for (i, option) in options.iter().enumerate() {
                if option.shortcuts.iter().any(|s| s.to_ascii_lowercase() == c) {
                    return KeyOutcome::Select(i);
                }
            }
            KeyOutcome::Ignored
        }
        _ => KeyOutcome::Ignored,
    }
}

fn draw(
    out: &mut impl Write,
    question: &str,
    footer: &str,
    options: &[MenuOption],
    selected: usize,
    width: u16,
    redraw: bool,
) -> Result<()> {
    let height = menu_height(options.len());
    if redraw {
        queue!(out, cursor::MoveUp(height))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))?;

    let inner_width = width.saturating_sub(4).max(1);
    let title_width = inner_width.saturating_sub(2);
    queue!(
        out,
        SetForegroundColor(Color::DarkGrey),
        Print("╭"),
        Print(rule(title_width as usize)),
        Print("╮"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("│ "),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print(pad_to_width(
            &truncate_text_to_width(question.to_string(), title_width),
            title_width as usize
        )),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(Color::DarkGrey),
        Print(" │"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("├"),
        Print(rule(title_width as usize)),
        Print("┤"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
    )?;

    let label_width = options
        .iter()
        .map(|o| UnicodeWidthStr::width(o.label))
        .max()
        .unwrap_or(0);
    for (i, option) in options.iter().enumerate() {
        let marker = if i == selected { "❯" } else { " " };
        let label = pad_to_width(option.label, label_width);
        let prefix = format!("{marker} {label}");
        let prefix_width = UnicodeWidthStr::width(prefix.as_str());
        let hint_width = title_width.saturating_sub((prefix_width + 3) as u16);
        let hint = truncate_text_to_width(option.hint.clone(), hint_width);
        let hint_width = UnicodeWidthStr::width(hint.as_str());
        queue!(out, SetForegroundColor(Color::DarkGrey), Print("│ "))?;
        if i == selected {
            queue!(
                out,
                SetForegroundColor(Color::Cyan),
                SetAttribute(Attribute::Bold),
                Print(prefix),
                SetAttribute(Attribute::Reset),
            )?;
        } else {
            queue!(out, SetForegroundColor(Color::Grey), Print(prefix))?;
        }
        let used = prefix_width + 3 + hint_width;
        queue!(
            out,
            SetAttribute(Attribute::Reset),
            SetAttribute(Attribute::Dim),
            Print("   "),
            Print(hint),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(Color::DarkGrey),
            Print(pad_to_width(
                "",
                title_width.saturating_sub(used as u16) as usize
            )),
            Print(" │"),
            SetAttribute(Attribute::Reset),
            Print("\r\n"),
        )?;
    }

    queue!(
        out,
        SetForegroundColor(Color::DarkGrey),
        Print("├"),
        Print(rule(title_width as usize)),
        Print("┤"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("│ "),
        SetAttribute(Attribute::Dim),
        Print(pad_to_width(
            &truncate_text_to_width(footer.to_string(), title_width),
            title_width as usize
        )),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(Color::DarkGrey),
        Print(" │"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("╰"),
        Print(rule(title_width as usize)),
        Print("╯"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
    )?;
    out.flush().context("flush inline menu")
}

fn menu_height(option_count: usize) -> u16 {
    option_count as u16 + 5
}

fn rule(width: usize) -> String {
    "─".repeat(width)
}

fn pad_to_width(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    if used >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - used))
    }
}

/// Puts the terminal in raw mode with the cursor hidden, restoring both on
/// drop so the menu cannot leave the shell in raw mode on error paths.
struct RawModeGuard;

impl RawModeGuard {
    fn enable(out: &mut impl Write) -> Result<Self> {
        enable_raw_mode().context("enable raw mode for inline menu")?;
        if let Err(e) = execute!(out, cursor::Hide) {
            let _ = disable_raw_mode();
            return Err(e).context("hide cursor for inline menu");
        }
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), cursor::Show);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<MenuOption> {
        vec![
            MenuOption {
                label: "Keep",
                hint: "leave it".to_string(),
                shortcuts: &['n', 'k'],
            },
            MenuOption {
                label: "Remove",
                hint: "delete it".to_string(),
                shortcuts: &['y', 'r'],
            },
        ]
    }

    #[test]
    fn arrows_move_and_wrap() {
        let opts = options();
        assert_eq!(
            key_outcome(KeyCode::Down, KeyModifiers::NONE, 0, &opts),
            KeyOutcome::Select(1)
        );
        assert_eq!(
            key_outcome(KeyCode::Down, KeyModifiers::NONE, 1, &opts),
            KeyOutcome::Select(0)
        );
        assert_eq!(
            key_outcome(KeyCode::Up, KeyModifiers::NONE, 0, &opts),
            KeyOutcome::Select(1)
        );
    }

    #[test]
    fn enter_chooses_selected_and_esc_cancels() {
        let opts = options();
        assert_eq!(
            key_outcome(KeyCode::Enter, KeyModifiers::NONE, 1, &opts),
            KeyOutcome::Choose(1)
        );
        assert_eq!(
            key_outcome(KeyCode::Esc, KeyModifiers::NONE, 1, &opts),
            KeyOutcome::Cancel
        );
        assert_eq!(
            key_outcome(KeyCode::Char('c'), KeyModifiers::CONTROL, 1, &opts),
            KeyOutcome::Cancel
        );
    }

    #[test]
    fn shortcuts_and_digits_only_move_selection() {
        let opts = options();
        assert_eq!(
            key_outcome(KeyCode::Char('y'), KeyModifiers::NONE, 0, &opts),
            KeyOutcome::Select(1)
        );
        assert_eq!(
            key_outcome(KeyCode::Char('N'), KeyModifiers::NONE, 1, &opts),
            KeyOutcome::Select(0)
        );
        assert_eq!(
            key_outcome(KeyCode::Char('2'), KeyModifiers::NONE, 0, &opts),
            KeyOutcome::Select(1)
        );
        assert_eq!(
            key_outcome(KeyCode::Char('x'), KeyModifiers::NONE, 0, &opts),
            KeyOutcome::Ignored
        );
    }

    #[test]
    fn enter_is_the_only_key_that_confirms_a_selection() {
        let opts = options();
        let non_confirming_keys = [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Char(' '),
            KeyCode::Char('y'),
            KeyCode::Char('r'),
            KeyCode::Char('2'),
        ];

        for key in non_confirming_keys {
            let outcome = key_outcome(key, KeyModifiers::NONE, 0, &opts);
            assert!(
                !matches!(outcome, KeyOutcome::Choose(_)),
                "{key:?} must not confirm any option, got {outcome:?}"
            );
        }
        assert_eq!(
            key_outcome(KeyCode::Down, KeyModifiers::NONE, 0, &opts),
            KeyOutcome::Select(1)
        );
        assert_eq!(
            key_outcome(KeyCode::Enter, KeyModifiers::NONE, 1, &opts),
            KeyOutcome::Choose(1)
        );
    }
}
