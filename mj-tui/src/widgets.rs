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

pub(crate) use mj_core::text::format_resource_bytes;

/// `count` and the noun that agrees with it. It lives in mj-core so the
/// daemon's and the CLI's messages use it too (launch finding R5-10).
pub(crate) use mj_core::text::counted;
