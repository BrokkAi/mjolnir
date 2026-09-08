//! Shared terminal colors and chrome for the dashboard and conversation.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub const BACKGROUND: Color = rgb(11, 18, 32);
pub const SURFACE: Color = rgb(17, 29, 45);
pub const SURFACE_RAISED: Color = rgb(27, 43, 64);
pub const SELECTION: Color = rgb(39, 57, 79);
pub const TEXT: Color = rgb(223, 235, 244);
pub const MUTED: Color = rgb(133, 150, 173);
pub const BORDER: Color = rgb(52, 70, 94);
pub const ACCENT: Color = rgb(99, 216, 229);
pub const SECONDARY: Color = rgb(181, 164, 245);
pub const SUCCESS: Color = rgb(135, 214, 176);
pub const WARNING: Color = rgb(240, 195, 123);
pub const ERROR: Color = rgb(242, 143, 156);

/// Semantic colors used by the dashboard's session summaries. These are
/// deliberately separate from the general-purpose palette: changing a panel
/// or dialog color must not change what a session's state means.
pub const SESSION_ERROR: Color = ERROR;
pub const SESSION_ACTIVITY: Color = WARNING;
pub const SESSION_ATTENTION: Color = rgb(255, 220, 96);
pub const SESSION_IDLE: Color = rgb(111, 177, 255);

#[allow(
    clippy::disallowed_methods,
    reason = "The shared theme paints both foreground and background, so its RGB contrast does not depend on the terminal palette."
)]
const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

/// The canvas beneath panels and modal halos.
pub fn base() -> Style {
    Style::default().fg(TEXT).bg(BACKGROUND)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn border(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(BORDER)
    }
}

pub fn title(focused: bool) -> Style {
    Style::default()
        .fg(if focused { ACCENT } else { TEXT })
        .add_modifier(Modifier::BOLD)
}

pub fn selection(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(ACCENT)
            .bg(SELECTION)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT).bg(SELECTION)
    }
}

/// Rounded panels keep identical content geometry regardless of focus.
pub fn panel(focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(TEXT).bg(SURFACE))
        .border_style(border(focused))
        .title_style(title(focused))
}

pub fn modal() -> Block<'static> {
    panel(true).style(Style::default().fg(TEXT).bg(SURFACE_RAISED))
}

/// Distinguishes keys from their descriptions without changing hint spacing.
pub fn hints(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    for hint in text.split_inclusive(['·', '│']) {
        let leading = hint.len() - hint.trim_start().len();
        let key_end = hint[leading..]
            .find(char::is_whitespace)
            .map_or(hint.len(), |offset| leading + offset);
        spans.push(Span::styled(hint[..leading].to_owned(), muted()));
        spans.push(Span::styled(
            hint[leading..key_end].to_owned(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(hint[key_end..].to_owned(), muted()));
    }
    Line::from(spans)
}

pub const FOOTER_SEPARATOR: &str = " · ";
pub const FOOTER_GROUP_SEPARATOR: &str = " │ ";

/// Keep complete footer hints within the terminal width. Pane hints give way
/// before global chords, then function keys; palette and help survive longest.
pub fn fit_footer(pane: &[&str], chords: &[&str], functions: &[&str], width: u16) -> String {
    let mut pane = pane.to_vec();
    let mut chords = chords.to_vec();
    let mut functions = functions.to_vec();
    loop {
        let text = [&pane, &chords, &functions]
            .into_iter()
            .filter(|group| !group.is_empty())
            .map(|group| group.join(FOOTER_SEPARATOR))
            .collect::<Vec<_>>()
            .join(FOOTER_GROUP_SEPARATOR);
        if unicode_width::UnicodeWidthStr::width(text.as_str()) <= usize::from(width) {
            return text;
        }
        if pane.pop().is_some() || chords.pop().is_some() {
            continue;
        }
        let removable = functions
            .iter()
            .rposition(|hint| !hint.starts_with("F1 ") && !hint.starts_with("F2 "))
            .or_else(|| functions.iter().rposition(|hint| !hint.starts_with("F1 ")))
            .or_else(|| functions.len().checked_sub(1));
        if let Some(index) = removable {
            functions.remove(index);
        } else {
            return String::new();
        }
    }
}
