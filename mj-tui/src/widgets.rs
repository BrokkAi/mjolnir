//! Small drawing primitives shared by the dashboard, dialogs, and wizards.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BorderType, Paragraph};

// Modal geometry is shared with the chat view, so it lives in `mj-chat`. These
// re-exports keep `crate::widgets` the single import site for dashboard code.
pub(crate) use mj_chat::hel_modal::{
    bordered_content, centered_modal, centered_modal_fixed, centered_rect, modal_area,
};

pub(crate) fn truncate_text(text: &str, width: usize) -> String {
    let text = collapse_whitespace(text);
    if text.chars().count() <= width {
        return text;
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut truncated = text.chars().take(width - 1).collect::<String>();
    truncated.truncate(
        truncated
            .trim_end_matches(|character: char| {
                character.is_whitespace()
                    || character.is_ascii_punctuation()
                    || matches!(
                        character,
                        '…' | '–' | '—' | '‘' | '’' | '“' | '”' | '•' | '·'
                    )
            })
            .len(),
    );
    truncated.push('…');
    truncated
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Popup height that keeps every wrapped line of `paragraph` visible, never
/// shrinking below the dialog's nominal height.
pub(crate) fn popup_height(
    paragraph: &Paragraph,
    width_percent: u16,
    nominal: u16,
    area: Rect,
) -> u16 {
    let inner_width = centered_rect(width_percent, 1, area)
        .width
        .saturating_sub(2);
    let wrapped = u16::try_from(paragraph.line_count(inner_width)).unwrap_or(u16::MAX);
    nominal.max(wrapped)
}

pub(crate) fn focus_border(focused: bool) -> BorderType {
    if focused {
        BorderType::Double
    } else {
        BorderType::Plain
    }
}

pub(crate) fn format_resource_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;

    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / KIB)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / MIB)
    } else if bytes < 1024_u64.pow(4) {
        format!("{:.1}G", bytes as f64 / GIB)
    } else {
        format!("{:.1}T", bytes as f64 / TIB)
    }
}

pub(crate) fn action_buttons(buttons: &[(&str, bool)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (label, focused)) in buttons.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if *focused {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    Line::from(spans).alignment(Alignment::Center)
}

/// Renders a button row from its label list, highlighting the focused index.
pub(crate) fn focused_buttons(labels: &[&'static str], focus: usize) -> Line<'static> {
    let buttons = labels
        .iter()
        .enumerate()
        .map(|(index, label)| (*label, index == focus))
        .collect::<Vec<_>>();
    action_buttons(&buttons)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_widget_text_removes_cutoff_whitespace_and_punctuation() {
        assert_eq!(truncate_text("alpha, beta", 7), "alpha…");
        assert_eq!(truncate_text("alpha - beta", 8), "alpha…");
        assert_eq!(truncate_text("alpha beta", 20), "alpha beta");
    }
}
