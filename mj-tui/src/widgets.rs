//! Small drawing primitives shared by the dashboard, dialogs, and wizards.

use mj_chat::hel_selection::{FrameSurfaces, SurfaceFrame, SurfaceId};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BorderType, Paragraph};

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

/// The drawn text inside a full border, which is the part of a widget a
/// selection may cover.
pub(crate) fn bordered_content(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    })
}

/// Centers a modal popup and registers its body as a selectable surface.
///
/// Modals draw over the dashboard, and the registry is z-ordered by render
/// order, so registering here makes the body win the cells it covers.
pub(crate) fn centered_modal(
    surfaces: &mut FrameSurfaces,
    width_percent: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_rect(width_percent, height, area);
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        bordered_content(popup),
    ));
    popup
}

/// Empty cells kept between any modal and the screen edge on every side.
pub(crate) const MODAL_SCREEN_MARGIN: u16 = 2;

/// The region a modal may occupy: `area` inset by [`MODAL_SCREEN_MARGIN`] on
/// every side, so dialogs never butt against the terminal border. On a terminal
/// too small to hold the margin it degrades to the full area rather than vanish.
pub(crate) fn modal_area(area: Rect) -> Rect {
    let margin = MODAL_SCREEN_MARGIN;
    if area.width > margin * 2 && area.height > margin * 2 {
        Rect::new(
            area.x + margin,
            area.y + margin,
            area.width - margin * 2,
            area.height - margin * 2,
        )
    } else {
        area
    }
}

/// Centers a modal of an absolute cell width and registers its body as a
/// selectable surface. Use when the content has a natural width — a QR code, a
/// fixed table — that should hug its content instead of scaling with the
/// terminal. `width` and `height` include the border and are clamped to `area`.
pub(crate) fn centered_modal_fixed(
    surfaces: &mut FrameSurfaces,
    width: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_rect_fixed(width, height, area);
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        bordered_content(popup),
    ));
    popup
}

/// Centers a rectangle of an absolute cell size within `area`, clamped to fit
/// inside the [`modal_area`] margin.
pub(crate) fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let area = modal_area(area);
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

pub(crate) fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let area = modal_area(area);
    let vertical_margin = area.height.saturating_sub(height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(vertical_margin),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Min(0),
        ])
        .split(vertical[1]);
    horizontal[1]
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
