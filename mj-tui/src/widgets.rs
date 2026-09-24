//! Small drawing primitives shared by the dashboard, dialogs, and wizards.

use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

// Modal geometry is shared with the chat view, so it lives in `mj-chat`. These
// re-exports keep `crate::widgets` the single import site for dashboard code.
pub(crate) use mj_chat::components::{Truncate, truncate_to_cells};
pub(crate) use mj_chat::modal::{
    bordered_content, centered_modal, centered_modal_fixed, centered_rect, dismissible_modal_title,
    modal_area,
};

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

/// `count` and the noun that agrees with it: `1 command`, `2 commands`,
/// `0 entries`. Counts the dashboard prints go through this, so none reads
/// "1 commands".
pub(crate) fn counted<T>(count: T, singular: &str, plural: &str) -> String
where
    T: std::fmt::Display + PartialEq + From<u8>,
{
    let noun = if count == T::from(1) {
        singular
    } else {
        plural
    };
    format!("{count} {noun}")
}

#[cfg(test)]
mod tests {
    use super::counted;

    #[test]
    fn counted_agrees_the_noun_with_the_count() {
        assert_eq!(counted(0_usize, "entry", "entries"), "0 entries");
        assert_eq!(counted(1_usize, "entry", "entries"), "1 entry");
        assert_eq!(counted(2_i64, "command", "commands"), "2 commands");
    }
}
