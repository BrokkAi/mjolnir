//! Shared terminal colors and chrome for the dashboard and conversation.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

use std::cell::Cell;
use std::sync::OnceLock;

pub use mj_core::config::SymbolSet;
pub use mj_core::config::UiTheme;

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

/// No colors: every slot is the terminal's own default, and the style
/// functions below carry focus and selection with bold and reverse video.
const MONO: Palette = Palette {
    background: Color::Reset,
    surface_raised: Color::Reset,
    surface: Color::Reset,
    selection: Color::Reset,
    text: Color::Reset,
    muted: Color::Reset,
    border: Color::Reset,
    accent: Color::Reset,
    secondary: Color::Reset,
    success: Color::Reset,
    warning: Color::Reset,
    error: Color::Reset,
    session_error: Color::Reset,
    session_activity: Color::Reset,
    session_attention: Color::Reset,
    session_idle: Color::Reset,
    activity_dim: Color::Reset,
};

thread_local! {
    static CURRENT: Cell<UiTheme> = const { Cell::new(UiTheme::Midnight) };
    static SYMBOLS: Cell<SymbolSet> = const { Cell::new(SymbolSet::Unicode) };
}

pub fn current() -> UiTheme {
    CURRENT.get()
}

/// Whether the palette in force paints no colors, so styles have to say
/// everything with modifiers.
pub fn is_mono() -> bool {
    current() == UiTheme::Mono
}

/// Whether `NO_COLOR` (https://no-color.org) is set to a non-empty value.
/// Read once: the answer cannot change while the process runs.
pub fn no_color_requested() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()))
}

/// The theme to draw with: the configured one, unless `NO_COLOR` asks for
/// none, which wins over every configured choice.
pub fn effective_theme(configured: UiTheme) -> UiTheme {
    theme_for(configured, no_color_requested())
}

/// [`effective_theme`] with the environment answer passed in, for tests.
pub fn theme_for(configured: UiTheme, no_color: bool) -> UiTheme {
    if no_color { UiTheme::Mono } else { configured }
}

pub fn palette_for(theme: UiTheme) -> &'static Palette {
    match theme {
        UiTheme::Midnight => &MIDNIGHT,
        UiTheme::Light => &LIGHT,
        UiTheme::Darcula => &DARCULA,
        UiTheme::HighContrast => &HIGH_CONTRAST,
        UiTheme::Mono => &MONO,
    }
}

/// The glyphs one symbol set draws with. Every site that draws a status
/// mark, a control, a rule, or a separator reads these through [`glyphs`],
/// so switching the set changes all of them together.
#[derive(Debug)]
pub struct Glyphs {
    pub working: &'static str,
    pub waiting: &'static str,
    pub unread: &'static str,
    pub idle: &'static str,
    pub unknown: &'static str,
    pub unreachable: &'static str,
    pub failed: &'static str,
    pub starting: &'static str,
    pub resuming: &'static str,
    pub moving: &'static str,
    pub checkpointing: &'static str,
    pub stopping: &'static str,
    pub stopped: &'static str,
    pub destroying: &'static str,
    /// The caret before the selected row.
    pub selected: &'static str,
    pub ellipsis: &'static str,
    pub row_menu: &'static str,
    pub workspace_menu: &'static str,
    pub close: &'static str,
    pub size_minimized: &'static str,
    pub size_standard: &'static str,
    pub size_maximized: &'static str,
    pub warning: &'static str,
    pub spark: &'static str,
    pub rule: &'static str,
    pub none: &'static str,
    pub footer_separator: &'static str,
    pub footer_group_separator: &'static str,
    pub bullet: &'static str,
    pub check: &'static str,
    pub running: &'static str,
    pub pending: &'static str,
    pub dropdown: &'static str,
    pub scroll_track: &'static str,
    pub scroll_thumb: &'static str,
    pub bar_full: &'static str,
    pub bar_left: &'static str,
    pub bar_right: &'static str,
    pub arrows_vertical: &'static str,
    pub role_gutter: &'static str,
}

pub const UNICODE_GLYPHS: Glyphs = Glyphs {
    working: "◐",
    waiting: "!",
    unread: "✓",
    idle: "○",
    unknown: "·",
    unreachable: "?",
    failed: "×",
    starting: "↑",
    resuming: "↻",
    moving: "⇄",
    checkpointing: "▣",
    stopping: "↓",
    stopped: "■",
    destroying: "⊗",
    selected: "› ",
    ellipsis: "…",
    row_menu: " ⋯ ",
    workspace_menu: " ☰ ",
    close: " × ",
    size_minimized: "▁",
    size_standard: "▪",
    size_maximized: "□",
    warning: "⚠",
    spark: "✦",
    rule: "─",
    none: "—",
    footer_separator: " · ",
    footer_group_separator: " │ ",
    bullet: "•",
    check: "✓",
    running: "●",
    pending: "○",
    dropdown: "▾",
    scroll_track: "│",
    scroll_thumb: "▐",
    bar_full: "█",
    bar_left: "▕",
    bar_right: "▏",
    arrows_vertical: "↑↓",
    role_gutter: "│ ",
};

pub const ASCII_GLYPHS: Glyphs = Glyphs {
    working: "*",
    waiting: "!",
    unread: "+",
    idle: "-",
    unknown: ".",
    unreachable: "?",
    failed: "x",
    starting: "^",
    resuming: "~",
    moving: "<>",
    checkpointing: "#",
    stopping: "v",
    stopped: "=",
    destroying: "X",
    selected: "> ",
    ellipsis: "..",
    row_menu: " . ",
    workspace_menu: " = ",
    close: " x ",
    size_minimized: "-",
    size_standard: "=",
    size_maximized: "+",
    warning: "!",
    spark: "*",
    rule: "-",
    none: "-",
    footer_separator: " - ",
    footer_group_separator: " | ",
    bullet: "*",
    check: "x",
    running: "*",
    pending: "o",
    dropdown: "v",
    scroll_track: "|",
    scroll_thumb: "#",
    bar_full: "#",
    bar_left: "[",
    bar_right: "]",
    arrows_vertical: "^v",
    role_gutter: "| ",
};

/// The glyphs in force on this thread.
pub fn glyphs() -> &'static Glyphs {
    match SYMBOLS.get() {
        SymbolSet::Unicode => &UNICODE_GLYPHS,
        SymbolSet::Ascii => &ASCII_GLYPHS,
    }
}

/// Whether the ASCII set is in force.
pub fn ascii() -> bool {
    SYMBOLS.get() == SymbolSet::Ascii
}

/// Select a symbol set for synchronous rendering, restoring it even on panic.
pub fn with_symbols<R>(symbols: SymbolSet, render: impl FnOnce() -> R) -> R {
    struct Restore(SymbolSet);
    impl Drop for Restore {
        fn drop(&mut self) {
            SYMBOLS.set(self.0);
        }
    }
    let _restore = Restore(SYMBOLS.replace(symbols));
    render()
}

/// The symbol set to draw with: the configured one, or a guess from the
/// terminal when nothing is configured. The Linux console and a locale
/// without UTF-8 cannot show the Unicode set.
pub fn symbols_for(configured: Option<SymbolSet>) -> SymbolSet {
    configured.unwrap_or_else(|| {
        static DETECTED: OnceLock<SymbolSet> = OnceLock::new();
        *DETECTED.get_or_init(|| {
            symbols_for_environment(
                std::env::var("TERM").ok().as_deref(),
                ["LC_ALL", "LC_CTYPE", "LANG"]
                    .into_iter()
                    .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
                    .as_deref(),
            )
        })
    })
}

/// [`symbols_for`]'s guess, from the terminal name and the locale.
pub fn symbols_for_environment(term: Option<&str>, locale: Option<&str>) -> SymbolSet {
    if term == Some("linux") {
        return SymbolSet::Ascii;
    }
    match locale {
        Some(locale) if !locale.to_ascii_lowercase().contains("utf") => SymbolSet::Ascii,
        _ => SymbolSet::Unicode,
    }
}

/// An all-ASCII border for terminals that cannot draw box characters.
const ASCII_BORDER: ratatui::symbols::border::Set = ratatui::symbols::border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

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
    if is_mono() {
        // With no colors, reverse video is the selection.
        let style = Style::default().add_modifier(Modifier::REVERSED);
        return if focused {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
    }
    if focused {
        Style::default()
            .fg(palette().accent)
            .bg(palette().selection)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette().text).bg(palette().selection)
    }
}

/// The background of a raised surface: a selected row, an armed control, a
/// modal's title bar. Reverse video without colors.
pub fn raised() -> Style {
    if is_mono() {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().bg(palette().surface_raised)
    }
}

/// The style of a control that is switched on, such as the active pane size
/// chip: a raised surface in a colored theme, reverse video without colors.
pub fn active_control() -> Style {
    if is_mono() {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette().accent)
            .bg(palette().surface_raised)
            .add_modifier(Modifier::BOLD)
    }
}

/// Emphasize a key name without painting it as a selected control.
pub fn key_hint() -> Style {
    Style::default()
        .fg(palette().text)
        .add_modifier(Modifier::BOLD)
}

/// Description text beside a key hint. Panel titles are bold, so the
/// description must remove BOLD explicitly; patching a plain muted style
/// over a bold title style cannot clear the modifier.
pub fn hint_description() -> Style {
    muted().remove_modifier(Modifier::BOLD)
}

/// Rounded panels keep identical content geometry regardless of focus.
pub fn panel(focused: bool) -> Block<'static> {
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(palette().text).bg(palette().surface))
        .border_style(border(focused))
        .title_style(title(focused));
    if ascii() {
        block.border_set(ASCII_BORDER)
    } else {
        block.border_type(BorderType::Rounded)
    }
}

pub fn modal() -> Block<'static> {
    panel(true).style(
        Style::default()
            .fg(palette().text)
            .bg(palette().surface_raised),
    )
}

/// Distinguishes keys from their descriptions without changing hint spacing.
///
/// The text is cut at the footer separators in force, so the ASCII set's
/// ` - ` never splits a hyphenated key such as `Shift-Enter`.
pub fn hints(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let separators = [footer_group_separator(), footer_separator()];
    let mut rest = text;
    while !rest.is_empty() {
        let cut = separators
            .iter()
            .filter_map(|separator| rest.find(separator).map(|at| (at, separator.len())))
            .min();
        let (hint, separator) = match cut {
            Some((at, len)) => (&rest[..at], &rest[at..at + len]),
            None => (rest, ""),
        };
        let leading = hint.len() - hint.trim_start().len();
        let key_end = hint[leading..]
            .find(char::is_whitespace)
            .map_or(hint.len(), |offset| leading + offset);
        spans.push(Span::styled(hint[..leading].to_owned(), muted()));
        spans.push(Span::styled(hint[leading..key_end].to_owned(), key_hint()));
        spans.push(Span::styled(
            format!("{}{separator}", &hint[key_end..]),
            muted(),
        ));
        rest = &rest[hint.len() + separator.len()..];
    }
    Line::from(spans)
}

/// The text between two hints of one footer group.
pub fn footer_separator() -> &'static str {
    glyphs().footer_separator
}

/// The text between two footer groups.
pub fn footer_group_separator() -> &'static str {
    glyphs().footer_group_separator
}

/// The footer row drawn while the prefix key is waiting for the key that
/// completes a chord. `prefix` is the resolved prefix label and `help_key` the
/// key that lists the bindings.
pub fn prefix_banner(prefix: &str, help_key: &str) -> Line<'static> {
    let badge = if is_mono() {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette().background)
            .bg(palette().accent)
            .add_modifier(Modifier::BOLD)
    };
    let separator = footer_separator();
    Line::from(vec![
        Span::styled(" PREFIX ".to_owned(), badge),
        Span::styled(
            format!(" esc cancel{separator}{prefix} send{separator}{help_key} keys"),
            muted(),
        ),
    ])
}

/// Keep complete footer hints within the terminal width. Pane hints give way
/// before the prefix chords; `protected` names the hints that survive longest.
pub fn fit_footer(
    pane: &[&str],
    chords: &[&str],
    functions: &[&str],
    width: u16,
    protected: impl Fn(&&str) -> bool,
) -> String {
    footer_items_text(
        &fit_footer_items(
            [pane.to_vec(), chords.to_vec(), functions.to_vec()],
            width,
            |text| *text,
            protected,
        ),
        |text| *text,
    )
}

/// Fit structured command hints while retaining their identities for hit testing.
///
/// Whole segments are dropped rather than truncated, because half a hint names
/// a key that does not exist. They give way from the left-hand group first, and
/// from the right within a group, so the reader loses what the pane offers
/// before what answers from anywhere. A hint `protected` accepts is dropped
/// only once nothing else is left, which is how the palette and the help key
/// stay visible on the narrowest terminal.
pub fn fit_footer_items<T>(
    groups: [Vec<T>; 3],
    width: u16,
    label: impl Fn(&T) -> &str,
    protected: impl Fn(&T) -> bool,
) -> [Vec<T>; 3] {
    let mut groups = groups;
    loop {
        let text = footer_items_text(&groups, &label);
        if unicode_width::UnicodeWidthStr::width(text.as_str()) <= usize::from(width) {
            return groups;
        }
        let victim = groups
            .iter()
            .enumerate()
            .find_map(|(group, hints)| {
                hints
                    .iter()
                    .rposition(|hint| !protected(hint))
                    .map(|index| (group, index))
            })
            // Only protected hints are left: give up the leading one, so the
            // last hint standing is the one the table ranked last.
            .or_else(|| {
                groups
                    .iter()
                    .position(|hints| !hints.is_empty())
                    .map(|group| (group, 0))
            });
        match victim {
            Some((group, index)) => {
                groups[group].remove(index);
            }
            None => return groups,
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
                .join(footer_separator())
        })
        .collect::<Vec<_>>()
        .join(footer_group_separator())
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
    fn no_color_selects_the_monochrome_theme_and_mono_uses_reverse_video() {
        assert_eq!(theme_for(UiTheme::Light, true), UiTheme::Mono);
        assert_eq!(theme_for(UiTheme::Light, false), UiTheme::Light);
        with_theme(UiTheme::Mono, || {
            assert_eq!(base().bg, Some(Color::Reset));
            assert!(selection(true).add_modifier.contains(Modifier::REVERSED));
            assert!(active_control().add_modifier.contains(Modifier::REVERSED));
        });
    }

    #[test]
    fn ascii_symbols_follow_the_console_and_the_locale_and_swap_every_glyph() {
        assert_eq!(
            symbols_for_environment(Some("linux"), Some("en_US.UTF-8")),
            SymbolSet::Ascii
        );
        assert_eq!(
            symbols_for_environment(Some("xterm"), Some("C")),
            SymbolSet::Ascii
        );
        assert_eq!(
            symbols_for_environment(Some("xterm"), Some("en_US.utf8")),
            SymbolSet::Unicode
        );
        assert_eq!(
            symbols_for_environment(Some("xterm"), None),
            SymbolSet::Unicode
        );
        assert_eq!(symbols_for(Some(SymbolSet::Ascii)), SymbolSet::Ascii);
        with_symbols(SymbolSet::Ascii, || {
            assert!(glyphs().working.is_ascii());
            assert_eq!(footer_separator(), " - ");
            // A hyphenated key survives the ASCII separator.
            let line = hints("Shift-Enter newline - Tab pane | ctrl+b then: c create");
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(
                text,
                "Shift-Enter newline - Tab pane | ctrl+b then: c create"
            );
            assert_eq!(line.spans[1].content, "Shift-Enter");
        });
        assert_eq!(footer_separator(), " · ");
    }

    #[test]
    fn palette_text_is_legible_on_its_painted_surfaces() {
        for theme in UiTheme::ALL {
            if theme == UiTheme::Mono {
                // No colors to measure: the terminal's own are in force.
                continue;
            }
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
