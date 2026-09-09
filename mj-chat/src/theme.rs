//! Shared terminal colors and chrome for the dashboard and conversation.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

use std::cell::Cell;

pub use hel::hel_config::UiTheme;

/// Semantic colors always paired with the palette's painted surfaces.
#[derive(Debug)]
pub struct Palette {
    pub background: Color,
    pub surface_raised: Color,
    pub surface: Color,
    pub selection: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub secondary: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub session_error: Color,
    pub session_activity: Color,
    pub session_attention: Color,
    pub session_idle: Color,
    pub activity_dim: Color,
}

const MIDNIGHT: Palette = Palette {
    background: rgb(11, 18, 32),
    surface_raised: rgb(27, 43, 64),
    surface: rgb(17, 29, 45),
    selection: rgb(39, 57, 79),
    text: rgb(223, 235, 244),
    muted: rgb(133, 150, 173),
    border: rgb(52, 70, 94),
    accent: rgb(99, 216, 229),
    secondary: rgb(181, 164, 245),
    success: rgb(135, 214, 176),
    warning: rgb(240, 195, 123),
    error: rgb(242, 143, 156),
    session_error: rgb(242, 143, 156),
    session_activity: rgb(240, 195, 123),
    session_attention: rgb(255, 220, 96),
    session_idle: rgb(111, 177, 255),
    activity_dim: rgb(78, 37, 55),
};

const LIGHT: Palette = Palette {
    background: rgb(242, 245, 250),
    surface_raised: rgb(232, 237, 245),
    surface: rgb(255, 255, 255),
    selection: rgb(211, 228, 241),
    text: rgb(30, 41, 59),
    muted: rgb(83, 100, 121),
    border: rgb(161, 174, 192),
    accent: rgb(0, 103, 124),
    secondary: rgb(105, 65, 166),
    success: rgb(28, 112, 74),
    warning: rgb(142, 83, 8),
    error: rgb(179, 42, 65),
    session_error: rgb(179, 42, 65),
    session_activity: rgb(142, 83, 8),
    session_attention: rgb(145, 96, 0),
    session_idle: rgb(31, 98, 183),
    activity_dim: rgb(220, 178, 186),
};

const DARCULA: Palette = Palette {
    // IntelliJ Darcula's charcoal editor and panel anchors.
    background: rgb(43, 43, 43),
    surface_raised: rgb(60, 63, 65),
    surface: rgb(49, 51, 53),
    selection: rgb(33, 66, 131),
    text: rgb(169, 183, 198),
    muted: rgb(159, 170, 183),
    border: rgb(126, 135, 143),
    // Brighter variants preserve contrast over the blue selection surface.
    accent: rgb(112, 183, 255),
    secondary: rgb(205, 169, 255),
    success: rgb(136, 231, 155),
    warning: rgb(255, 198, 109),
    error: rgb(255, 155, 155),
    session_error: rgb(255, 155, 155),
    session_activity: rgb(255, 198, 109),
    session_attention: rgb(255, 220, 96),
    session_idle: rgb(145, 220, 255),
    activity_dim: rgb(104, 56, 76),
};

const HIGH_CONTRAST: Palette = Palette {
    background: rgb(0, 0, 0),
    surface_raised: rgb(32, 32, 32),
    surface: rgb(0, 0, 0),
    selection: rgb(51, 51, 255),
    text: rgb(255, 255, 255),
    muted: rgb(190, 190, 190),
    border: rgb(230, 230, 230),
    accent: rgb(26, 235, 255),
    secondary: rgb(255, 150, 255),
    success: rgb(80, 166, 97),
    warning: rgb(255, 191, 102),
    error: rgb(255, 80, 80),
    session_error: rgb(255, 80, 80),
    session_activity: rgb(255, 191, 102),
    session_attention: rgb(255, 220, 96),
    session_idle: rgb(140, 220, 255),
    activity_dim: rgb(64, 0, 32),
};

thread_local! {
    static CURRENT: Cell<UiTheme> = const { Cell::new(UiTheme::Midnight) };
}

pub fn current() -> UiTheme {
    CURRENT.get()
}

pub fn palette_for(theme: UiTheme) -> &'static Palette {
    match theme {
        UiTheme::Midnight => &MIDNIGHT,
        UiTheme::Light => &LIGHT,
        UiTheme::Darcula => &DARCULA,
        UiTheme::HighContrast => &HIGH_CONTRAST,
    }
}

pub fn palette() -> &'static Palette {
    palette_for(current())
}

/// Select a palette for synchronous rendering, restoring it even on panic.
/// Keep asynchronous work outside this scope: colors belong to this thread.
pub fn with_theme<R>(theme: UiTheme, render: impl FnOnce() -> R) -> R {
    struct Restore(UiTheme);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.set(self.0);
        }
    }
    let _restore = Restore(CURRENT.replace(theme));
    render()
}

#[allow(
    clippy::disallowed_methods,
    reason = "The shared theme paints both foreground and background, so its RGB contrast does not depend on the terminal palette."
)]
const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

/// The canvas beneath panels and modal halos.
pub fn base() -> Style {
    Style::default().fg(palette().text).bg(palette().background)
}

pub fn muted() -> Style {
    Style::default().fg(palette().muted)
}

pub fn border(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(palette().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette().border)
    }
}

pub fn title(focused: bool) -> Style {
    Style::default()
        .fg(if focused {
            palette().accent
        } else {
            palette().text
        })
        .add_modifier(Modifier::BOLD)
}

pub fn selection(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(palette().accent)
            .bg(palette().selection)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette().text).bg(palette().selection)
    }
}

/// Rounded panels keep identical content geometry regardless of focus.
pub fn panel(focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(palette().text).bg(palette().surface))
        .border_style(border(focused))
        .title_style(title(focused))
}

pub fn modal() -> Block<'static> {
    panel(true).style(
        Style::default()
            .fg(palette().text)
            .bg(palette().surface_raised),
    )
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
            Style::default()
                .fg(palette().text)
                .add_modifier(Modifier::BOLD),
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
    footer_items_text(
        &fit_footer_items(
            [pane.to_vec(), chords.to_vec(), functions.to_vec()],
            width,
            |text| *text,
        ),
        |text| *text,
    )
}

/// Fit structured command hints while retaining their identities for hit testing.
pub fn fit_footer_items<T>(
    [mut pane, mut chords, mut functions]: [Vec<T>; 3],
    width: u16,
    label: impl Fn(&T) -> &str,
) -> [Vec<T>; 3] {
    loop {
        let groups = [pane, chords, functions];
        let text = footer_items_text(&groups, &label);
        if unicode_width::UnicodeWidthStr::width(text.as_str()) <= usize::from(width) {
            return groups;
        }
        [pane, chords, functions] = groups;
        if pane.pop().is_some() || chords.pop().is_some() {
            continue;
        }
        let removable = functions
            .iter()
            .rposition(|hint| !label(hint).starts_with("F1 ") && !label(hint).starts_with("F2 "))
            .or_else(|| {
                functions
                    .iter()
                    .rposition(|hint| !label(hint).starts_with("F1 "))
            })
            .or_else(|| functions.len().checked_sub(1));
        if let Some(index) = removable {
            functions.remove(index);
        } else {
            return [pane, chords, functions];
        }
    }
}

/// Render the same complete segments used to register footer controls.
pub fn footer_items_text<T>(groups: &[Vec<T>; 3], label: impl Fn(&T) -> &str) -> String {
    groups
        .iter()
        .filter(|group| !group.is_empty())
        .map(|group| {
            group
                .iter()
                .map(&label)
                .collect::<Vec<_>>()
                .join(FOOTER_SEPARATOR)
        })
        .collect::<Vec<_>>()
        .join(FOOTER_GROUP_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_scopes_restore_colors_after_nested_rendering_and_panics() {
        let original = current();
        with_theme(UiTheme::Light, || {
            assert_eq!(base().bg, Some(LIGHT.background));
            let result = std::panic::catch_unwind(|| {
                with_theme(UiTheme::Darcula, || {
                    assert_eq!(base().fg, Some(DARCULA.text));
                    panic!("interrupted render");
                });
            });
            assert!(result.is_err());
            assert_eq!(base().bg, Some(LIGHT.background));
        });
        assert_eq!(current(), original);
    }

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("palettes must define explicit RGB colors");
        };
        [r, g, b]
            .into_iter()
            .zip([0.2126, 0.7152, 0.0722])
            .map(|(channel, weight)| {
                let value = f64::from(channel) / 255.0;
                weight
                    * if value <= 0.04045 {
                        value / 12.92
                    } else {
                        ((value + 0.055) / 1.055).powf(2.4)
                    }
            })
            .sum()
    }

    fn contrast(foreground: Color, background: Color) -> f64 {
        let a = luminance(foreground);
        let b = luminance(background);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn palette_text_is_legible_on_its_painted_surfaces() {
        for theme in UiTheme::ALL {
            let colors = palette_for(theme);
            for foreground in [
                colors.text,
                colors.muted,
                colors.accent,
                colors.secondary,
                colors.success,
                colors.warning,
                colors.error,
                colors.session_error,
                colors.session_activity,
                colors.session_attention,
                colors.session_idle,
            ] {
                for background in [colors.background, colors.surface, colors.surface_raised] {
                    assert!(
                        contrast(foreground, background) >= 4.5,
                        "{theme:?}: {foreground:?} on {background:?} has insufficient contrast"
                    );
                }
            }
            for foreground in [colors.text, colors.accent] {
                assert!(contrast(foreground, colors.selection) >= 4.5);
            }
        }
    }

    #[test]
    fn high_contrast_uses_black_canvas_and_distinct_raised_controls() {
        let colors = palette_for(UiTheme::HighContrast);
        assert_eq!(colors.background, rgb(0, 0, 0));
        assert_eq!(colors.surface, rgb(0, 0, 0));
        assert_eq!(colors.surface_raised, rgb(32, 32, 32));
        assert!(contrast(colors.text, colors.surface_raised) >= 7.0);
        assert!(contrast(colors.accent, colors.selection) >= 4.5);
    }
}
