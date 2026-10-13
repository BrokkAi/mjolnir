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

/// `count` and the noun that agrees with it. It lives in mj-core so the
/// daemon's and the CLI's messages use it too (launch finding R5-10).
pub(crate) use mj_core::text::counted;

pub(crate) fn config_choice_label(
    value: Option<&str>,
    choices: &[mj_core::acp::SessionConfigChoice],
    capabilities_discovered: bool,
) -> String {
    let Some(value) = value else {
        return "Profile default".to_owned();
    };
    choices
        .iter()
        .find(|choice| choice.value == value)
        .map(|choice| choice.name.clone())
        .unwrap_or_else(|| {
            let state = if capabilities_discovered {
                "unavailable"
            } else {
                "unverified"
            };
            format!("{value} ({state})")
        })
}

pub(crate) fn config_choice_values(
    value: Option<&str>,
    choices: &[mj_core::acp::SessionConfigChoice],
) -> Vec<Option<String>> {
    let mut values = vec![None];
    values.extend(choices.iter().map(|choice| Some(choice.value.clone())));
    if let Some(value) = value
        && !values
            .iter()
            .any(|candidate| candidate.as_deref() == Some(value))
    {
        // Keep an invalid value in the form until the user explicitly
        // changes it. A refresh must never silently pick a new model.
        values.push(Some(value.to_owned()));
    }
    values
}
