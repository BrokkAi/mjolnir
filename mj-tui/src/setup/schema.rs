//! The shared settings schema, plus the theme report only a terminal can
//! make: what Auto resolved to against this terminal's background, and
//! whether `NO_COLOR` overrides the configured theme.
use mj_chat::theme;
pub(super) use mj_client::settings::schema::*;
use serde_json::Value;

/// [`mj_client::settings::schema::section_summary`] with the theme reported
/// as this terminal draws it.
pub(super) fn section_summary(key: &str, draft: &Value) -> Option<String> {
    mj_client::settings::schema::section_summary(key, draft, &draft_theme_report)
}

/// The theme as Setup reports it: what is on screen and why. `NO_COLOR` is
/// an override, not a theme: [`theme::effective_theme`] draws in monochrome
/// whatever is configured, and the configured choice is kept and comes back
/// once `NO_COLOR` is unset, so the report names the choice that is waiting.
/// Auto names the palette it picked and what the terminal reported.
pub(super) fn theme_report(
    configured: theme::ThemeChoice,
    no_color: bool,
    background: Option<theme::Rgb>,
) -> String {
    let resolution = theme::resolve(configured, no_color, background);
    let label = resolution.theme.label();
    match resolution.reason {
        theme::ThemeReason::Configured => label.to_owned(),
        theme::ThemeReason::NoColor => {
            format!("Monochrome (NO_COLOR; configured: {})", configured.label())
        }
        theme::ThemeReason::TerminalBackground((red, green, blue)) => {
            format!("Auto: {label} (terminal background #{red:02x}{green:02x}{blue:02x})")
        }
        theme::ThemeReason::Unreported => {
            format!("Auto: {label} (terminal did not report its background)")
        }
    }
}

/// [`theme_report`] for the draft's theme value, as this process sees it.
pub(super) fn draft_theme_report(value: &Value, draft: &Value) -> String {
    match serde_json::from_value::<theme::ThemeChoice>(value.clone()) {
        Ok(configured) => theme_report(
            configured,
            crate::no_color_requested(),
            theme::terminal_background(),
        ),
        Err(_) => choice_label(&["theme".to_owned()], value, draft),
    }
}
