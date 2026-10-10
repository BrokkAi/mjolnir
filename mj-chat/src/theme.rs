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
    /// The colors pinned sessions cycle through.
    pub pins: [Color; 8],
    /// The palette leaves every background to the terminal, so selection,
    /// focus, and filled chips are drawn with reverse video, bold, and
    /// underline instead of painted surfaces.
    pub reverse_video: bool,
}

const DARK_PINS: [Color; 8] = [
    rgb(100, 215, 235),
    rgb(190, 160, 250),
    rgb(130, 210, 180),
    rgb(235, 160, 205),
    rgb(145, 175, 250),
    rgb(205, 195, 135),
    rgb(125, 215, 215),
    rgb(215, 175, 175),
];

const LIGHT_PINS: [Color; 8] = [
    rgb(0, 105, 135),
    rgb(112, 65, 165),
    rgb(40, 110, 95),
    rgb(155, 65, 115),
    rgb(65, 85, 165),
    rgb(115, 95, 40),
    rgb(0, 110, 115),
    rgb(125, 75, 80),
];

const MIDNIGHT: Palette = Palette {
    // Neutral graphite leaves color to actions, authorship, and session state.
    background: rgb(15, 18, 20),
    surface_raised: rgb(32, 39, 43),
    surface: rgb(22, 27, 30),
    selection: rgb(42, 58, 57),
    text: rgb(239, 237, 232),
    muted: rgb(151, 163, 163),
    border: rgb(62, 74, 78),
    accent: rgb(157, 229, 202),
    secondary: rgb(158, 194, 238),
    success: rgb(157, 229, 202),
    warning: rgb(232, 189, 123),
    error: rgb(244, 154, 165),
    session_error: rgb(244, 154, 165),
    session_activity: rgb(232, 189, 123),
    session_attention: rgb(240, 205, 145),
    session_idle: rgb(158, 194, 238),
    activity_dim: rgb(67, 44, 48),
    pins: DARK_PINS,
    reverse_video: false,
};

const LIGHT: Palette = Palette {
    background: rgb(242, 241, 237),
    surface_raised: rgb(233, 231, 226),
    surface: rgb(251, 250, 247),
    selection: rgb(214, 229, 220),
    text: rgb(38, 51, 46),
    muted: rgb(89, 101, 95),
    border: rgb(179, 186, 180),
    accent: rgb(35, 104, 77),
    secondary: rgb(54, 95, 146),
    success: rgb(40, 108, 77),
    warning: rgb(133, 83, 19),
    error: rgb(169, 61, 80),
    session_error: rgb(169, 61, 80),
    session_activity: rgb(133, 83, 19),
    session_attention: rgb(133, 83, 19),
    session_idle: rgb(54, 95, 146),
    activity_dim: rgb(220, 183, 187),
    pins: LIGHT_PINS,
    reverse_video: false,
};

const DARCULA: Palette = Palette {
    // IntelliJ Darcula's charcoal editor and panel anchors.
    background: rgb(43, 43, 43),
    surface_raised: rgb(60, 63, 65),
    surface: rgb(49, 51, 53),
    selection: rgb(39, 55, 78),
    text: rgb(169, 183, 198),
    muted: rgb(159, 170, 183),
    border: rgb(126, 135, 143),
    // Brighter variants preserve contrast over the blue-gray selection surface.
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
    pins: DARK_PINS,
    reverse_video: false,
};

const HIGH_CONTRAST: Palette = Palette {
    background: rgb(0, 0, 0),
    surface_raised: rgb(32, 32, 32),
    surface: rgb(0, 0, 0),
    // Selected rows retain status colors, so their surface must support red too.
    selection: rgb(38, 38, 38),
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
    pins: DARK_PINS,
    reverse_video: false,
};

// The Windows Terminal palettes keep the scheme's own background so the
// window padding the terminal paints matches the dashboard canvas. Scheme
// colors that fall under 4.5:1 on a surface are lifted or deepened in hue.

const CAMPBELL: Palette = Palette {
    background: rgb(12, 12, 12),
    surface_raised: rgb(34, 34, 34),
    surface: rgb(19, 19, 19),
    // Campbell's blue, darkened to carry every status color.
    selection: rgb(23, 42, 84),
    text: rgb(204, 204, 204),
    muted: rgb(152, 152, 152),
    border: rgb(78, 78, 78),
    accent: rgb(97, 214, 214),
    secondary: rgb(97, 156, 255),
    success: rgb(22, 198, 12),
    warning: rgb(193, 156, 0),
    error: rgb(240, 108, 118),
    session_error: rgb(240, 108, 118),
    session_activity: rgb(193, 156, 0),
    session_attention: rgb(249, 241, 165),
    session_idle: rgb(97, 156, 255),
    activity_dim: rgb(60, 20, 24),
    pins: DARK_PINS,
    reverse_video: false,
};

const ONE_HALF_DARK: Palette = Palette {
    background: rgb(40, 44, 52),
    surface_raised: rgb(50, 56, 66),
    surface: rgb(44, 49, 58),
    selection: rgb(52, 64, 84),
    text: rgb(220, 223, 228),
    muted: rgb(166, 173, 184),
    border: rgb(90, 99, 116),
    accent: rgb(94, 186, 197),
    secondary: rgb(105, 179, 240),
    success: rgb(152, 195, 121),
    warning: rgb(229, 192, 123),
    error: rgb(233, 149, 156),
    session_error: rgb(233, 149, 156),
    session_activity: rgb(229, 192, 123),
    session_attention: rgb(240, 208, 150),
    session_idle: rgb(105, 179, 240),
    activity_dim: rgb(74, 48, 56),
    pins: DARK_PINS,
    reverse_video: false,
};

const ONE_HALF_LIGHT: Palette = Palette {
    background: rgb(250, 250, 250),
    surface_raised: rgb(240, 240, 241),
    surface: rgb(255, 255, 255),
    selection: rgb(222, 230, 242),
    text: rgb(56, 58, 66),
    muted: rgb(99, 101, 110),
    border: rgb(160, 161, 167),
    accent: rgb(1, 108, 154),
    secondary: rgb(166, 38, 164),
    success: rgb(56, 113, 55),
    warning: rgb(135, 92, 1),
    error: rgb(173, 65, 55),
    session_error: rgb(173, 65, 55),
    session_activity: rgb(135, 92, 1),
    session_attention: rgb(135, 92, 1),
    session_idle: rgb(7, 110, 131),
    activity_dim: rgb(236, 200, 196),
    pins: LIGHT_PINS,
    reverse_video: false,
};

// Solarized keeps its base tones and accent hues. Its accents sit near 3:1 on
// its own background, so each is lifted (dark) or deepened (light) to 4.5:1.

const SOLARIZED_DARK: Palette = Palette {
    background: rgb(0, 43, 54),
    surface_raised: rgb(7, 54, 66),
    surface: rgb(3, 47, 59),
    selection: rgb(12, 66, 80),
    text: rgb(238, 232, 213),
    muted: rgb(159, 171, 171),
    border: rgb(88, 110, 117),
    accent: rgb(91, 183, 176),
    secondary: rgb(105, 175, 224),
    success: rgb(162, 177, 61),
    warning: rgb(199, 165, 61),
    error: rgb(236, 142, 141),
    session_error: rgb(236, 142, 141),
    session_activity: rgb(199, 165, 61),
    session_attention: rgb(225, 151, 120),
    session_idle: rgb(105, 175, 224),
    activity_dim: rgb(58, 42, 50),
    pins: DARK_PINS,
    reverse_video: false,
};

const SOLARIZED_LIGHT: Palette = Palette {
    background: rgb(253, 246, 227),
    surface_raised: rgb(238, 232, 213),
    surface: rgb(255, 250, 236),
    selection: rgb(229, 222, 196),
    text: rgb(7, 54, 66),
    muted: rgb(81, 98, 105),
    border: rgb(147, 161, 161),
    accent: rgb(28, 108, 102),
    secondary: rgb(28, 101, 153),
    success: rgb(89, 103, 0),
    warning: rgb(121, 92, 0),
    error: rgb(183, 42, 39),
    session_error: rgb(183, 42, 39),
    session_activity: rgb(121, 92, 0),
    session_attention: rgb(168, 62, 18),
    session_idle: rgb(28, 101, 153),
    activity_dim: rgb(240, 205, 195),
    pins: LIGHT_PINS,
    reverse_video: false,
};

// The Terminal palettes name slots of the terminal's 16-color table, so the
// user's scheme decides the actual shades. Each slot takes the variant that
// stays readable across the common schemes: Campbell's normal blue and
// magenta are too dark on black, and Solarized maps several bright colors to
// its grays. Muted text and borders take fixed grays from the 256-color ramp,
// which no scheme redefines: bright black, the usual choice, is Solarized's
// background highlight and too dim on One Half Dark and Tango. See [`ansi`]
// for why every slot is a 256-color index.

const TERMINAL_DARK: Palette = Palette {
    background: Color::Reset,
    surface_raised: Color::Reset,
    surface: Color::Reset,
    selection: Color::Reset,
    text: Color::Reset,
    muted: ansi(246),
    border: ansi(243),
    accent: ansi(6),
    secondary: ansi(12),
    success: ansi(2),
    warning: ansi(3),
    error: ansi(9),
    session_error: ansi(9),
    session_activity: ansi(3),
    session_attention: ansi(3),
    session_idle: ansi(12),
    activity_dim: ansi(1),
    pins: [
        ansi(6),
        ansi(13),
        ansi(2),
        ansi(3),
        ansi(12),
        ansi(9),
        ansi(14),
        ansi(7),
    ],
    reverse_video: true,
};

const TERMINAL_LIGHT: Palette = Palette {
    background: Color::Reset,
    surface_raised: Color::Reset,
    surface: Color::Reset,
    selection: Color::Reset,
    text: Color::Reset,
    muted: ansi(242),
    border: ansi(248),
    // Light schemes keep their bright colors pale, so only the normal eight
    // and the two blacks carry text.
    accent: ansi(4),
    secondary: ansi(5),
    success: ansi(2),
    warning: ansi(3),
    error: ansi(1),
    session_error: ansi(1),
    session_activity: ansi(3),
    session_attention: ansi(3),
    session_idle: ansi(4),
    activity_dim: ansi(1),
    pins: [
        ansi(4),
        ansi(5),
        ansi(2),
        ansi(1),
        ansi(6),
        ansi(3),
        ansi(0),
        ansi(8),
    ],
    reverse_video: true,
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
    pins: [Color::Reset; 8],
    reverse_video: true,
};

thread_local! {
    static CURRENT: Cell<UiTheme> = const { Cell::new(UiTheme::Midnight) };
    static SYMBOLS: Cell<SymbolSet> = const { Cell::new(SymbolSet::Unicode) };
}

pub fn current() -> UiTheme {
    CURRENT.get()
}

/// Whether the palette in force leaves backgrounds to the terminal, so
/// styles carry selection and focus with reverse video, bold, and underline.
pub fn reverse_video() -> bool {
    palette().reverse_video
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
        UiTheme::Campbell => &CAMPBELL,
        UiTheme::OneHalfDark => &ONE_HALF_DARK,
        UiTheme::OneHalfLight => &ONE_HALF_LIGHT,
        UiTheme::SolarizedDark => &SOLARIZED_DARK,
        UiTheme::SolarizedLight => &SOLARIZED_LIGHT,
        UiTheme::TerminalDark => &TERMINAL_DARK,
        UiTheme::TerminalLight => &TERMINAL_LIGHT,
        UiTheme::Mono => &MONO,
    }
}

/// The glyphs one symbol set draws with. Every site that draws a status
/// mark, a control, a rule, or a separator reads these through [`glyphs`],
/// so switching the set changes all of them together.
#[derive(Debug)]
pub struct Glyphs {
    pub pin: &'static str,
    pub pinned: &'static str,
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
    /// Closes what a `dropdown` mark opened.
    pub dropup: &'static str,
    /// Opens another view rather than a choice menu.
    pub navigate: &'static str,
    pub scroll_track: &'static str,
    pub scroll_thumb: &'static str,
    pub bar_full: &'static str,
    pub bar_left: &'static str,
    pub bar_right: &'static str,
    pub arrows_vertical: &'static str,
    pub role_gutter: &'static str,
    /// The microphone chip on the composer's top border.
    pub microphone: &'static str,
    /// The marks a transcript header draws for each kind of message. A system
    /// message uses `rule` and a tool row uses the tool-status glyphs, so only
    /// the kinds without a mark of their own are listed here.
    pub role_user: &'static str,
    pub role_message: &'static str,
    pub role_agent: &'static str,
    pub role_thought: &'static str,
    pub role_plan: &'static str,
    pub role_plan_proposal: &'static str,
    /// Leads the first output row under an inline tool call.
    pub tool_output: &'static str,
}

pub const UNICODE_GLYPHS: Glyphs = Glyphs {
    pin: "◇",
    pinned: "◆",
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
    dropup: "▴",
    navigate: "›",
    scroll_track: "│",
    scroll_thumb: "▐",
    bar_full: "█",
    bar_left: "▕",
    bar_right: "▏",
    arrows_vertical: "↑↓",
    role_gutter: "│ ",
    // The text variation selector is intentional: a terminal should keep this
    // as a compact text button rather than an emoji of a different width.
    microphone: "🎙︎",
    role_user: "❯",
    role_message: "←",
    role_agent: "●",
    role_thought: "○",
    role_plan: "◇",
    role_plan_proposal: "◈",
    tool_output: "└ ",
};

pub const ASCII_GLYPHS: Glyphs = Glyphs {
    pin: "+",
    pinned: "*",
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
    dropup: "^",
    navigate: ">",
    scroll_track: "|",
    scroll_thumb: "#",
    bar_full: "#",
    bar_left: "[",
    bar_right: "]",
    arrows_vertical: "^v",
    role_gutter: "| ",
    microphone: "mic",
    role_user: ">",
    role_message: "<",
    role_agent: "*",
    role_thought: "o",
    role_plan: "-",
    role_plan_proposal: "+",
    tool_output: "`- ",
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

/// The one-cell ASCII stand-in for a Unicode mark that dialog titles, hints,
/// notices and Settings fields spell as literal text. The glyph table covers
/// the marks the dashboard draws itself; prose such as "Enter opens · Esc
/// closes" or "Saving…" cannot reach it without every string asking for its
/// separator, so the frame is folded once after drawing instead. The
/// replacements are one cell wide so no layout changes.
pub fn ascii_fallback(mark: char) -> Option<&'static str> {
    Some(match mark {
        '·' | '—' | '–' | '−' | '─' | '▁' => "-",
        '…' | '⋯' => ".",
        '▾' | '↓' => ASCII_GLYPHS.stopping,
        '↑' => ASCII_GLYPHS.starting,
        '→' | '›' | '❯' | '▸' => ">",
        '←' | '‹' => "<",
        '✓' => ASCII_GLYPHS.unread,
        '×' => ASCII_GLYPHS.failed,
        '▪' => ASCII_GLYPHS.size_standard,
        '□' | '◇' | '◈' => "+",
        '◆' | '•' | '●' | '◐' => "*",
        '○' => ASCII_GLYPHS.pending,
        '⚠' => "!",
        '│' | '▕' | '▏' => "|",
        '█' | '▐' => "#",
        '■' => "=",
        _ => return None,
    })
}

/// Replace every Unicode mark in `buffer` that has an ASCII stand-in. Called
/// after a frame is drawn while the ASCII symbol set is in force.
pub fn fold_buffer_to_ascii(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        let mut marks = cell.symbol().chars();
        if let (Some(mark), None) = (marks.next(), marks.next())
            && let Some(replacement) = ascii_fallback(mark)
        {
            cell.set_symbol(replacement);
        }
    }
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

/// A 256-color index (`38;5;n`). Indexes 0-15 are the terminal's own scheme
/// colors, written this way rather than as SGR 30-37 because Windows
/// Terminal, xterm and Alacritty draw bold text in the bright variant of a
/// 30-37 color; on a light scheme that turns a bold blue title pale.
#[allow(
    clippy::disallowed_methods,
    reason = "The terminal palettes name the user's own scheme colors and leave the background to that scheme."
)]
const fn ansi(index: u8) -> Color {
    Color::Indexed(index)
}

/// The canvas beneath panels and modal halos.
pub fn base() -> Style {
    Style::default().fg(palette().text).bg(palette().background)
}

/// Neutral enabled-control text; shape and surface convey clickability.
pub fn actionable() -> Style {
    Style::default().fg(palette().text)
}

/// A clickable chip that retains its surface and monochrome affordance.
pub fn actionable_chip() -> Style {
    selection(false).patch(actionable())
}

pub fn muted() -> Style {
    Style::default().fg(palette().muted)
}

pub fn border(focused: bool) -> Style {
    match (focused, current()) {
        (true, UiTheme::HighContrast) => Style::default()
            .fg(palette().accent)
            .add_modifier(Modifier::BOLD),
        // The terminal's own colors have no edge quieter than the border, so
        // focus is the foreground in bold.
        (true, _) if reverse_video() => Style::default()
            .fg(palette().text)
            .add_modifier(Modifier::BOLD),
        // Titles carry the accent. A neutral edge keeps large panes quiet.
        (true, _) => Style::default().fg(palette().muted),
        (false, _) => Style::default().fg(palette().border),
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
    if reverse_video() {
        // Without a painted surface, reverse video is the selection.
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
/// modal's title bar. Reverse video without painted surfaces.
pub fn raised() -> Style {
    if reverse_video() {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().bg(palette().surface_raised)
    }
}

/// The style of a control that is switched on, such as the active pane size
/// chip: a selection surface in a painted theme, reverse video otherwise.
pub fn active_control() -> Style {
    if reverse_video() {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette().accent)
            .bg(palette().selection)
            .add_modifier(Modifier::BOLD)
    }
}

/// A focused or armed action. Filled color is reserved for a direct action;
/// selected content uses the quieter [`selection`] surface.
pub fn focus_control() -> Style {
    filled(palette().accent).add_modifier(Modifier::BOLD)
}

/// A chip filled with `color` and lettered in the canvas color. Without a
/// painted canvas, reverse video swaps the terminal's own background in as
/// the lettering, which keeps the chip readable on any scheme.
pub fn filled(color: Color) -> Style {
    if reverse_video() {
        Style::default().fg(color).add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(palette().background).bg(color)
    }
}

/// An inset text field, with an uninterrupted focus surface for legibility.
/// Without painted surfaces an underline keeps editable text distinct.
pub fn field(focused: bool) -> Style {
    if reverse_video() {
        return if focused {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        };
    }
    Style::default().fg(palette().text).bg(if focused {
        palette().selection
    } else {
        palette().background
    })
}

/// Emphasize a key name without painting it as a selected control.
pub fn key_hint() -> Style {
    Style::default()
        .fg(palette().secondary)
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
    let separator = footer_separator();
    Line::from(vec![
        Span::styled(" PREFIX ".to_owned(), focus_control()),
        Span::styled(
            format!(" esc cancel{separator}{prefix} send{separator}{help_key} keys"),
            muted(),
        ),
    ])
}

/// Keep complete footer hints within the terminal width. The prefix chords
/// give way before the pane's own hints; `protected` names the hints that
/// survive longest.
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
/// a key that does not exist. They give way from the right-hand group first,
/// and from the right within a group, so the reader keeps the few keys that
/// work right here and loses the long list that answers from anywhere — a list
/// the palette holds in full. A hint `protected` accepts is dropped only once
/// nothing else is left, which is how the palette and the help key stay
/// visible on the narrowest terminal.
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
            .rev()
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

/// Fit footer hints whose chord group (the middle one) is read after a prefix.
///
/// `prefix` — `ctrl+b then ` — is prepended to the chord group's first hint.
/// That hint is the first unprotected chord the fitter drops, and once it is
/// gone the survivors would read `: palette · ? keys`, as if `:` alone opened
/// the palette. So when the labeled hint did not survive, the label moves to
/// the first chord that did, and the fit runs again so the label's width is
/// paid for. A row too narrow for the label and the palette together keeps
/// the unlabeled palette: a reachable key beats a complete sentence.
///
/// Both the dashboard footer and the composer footer go through this one
/// function, so the two cannot disagree about when the prefix is named.
pub fn fit_prefixed_footer_items<K: Clone>(
    groups: [Vec<(K, String)>; 3],
    width: u16,
    prefix: &str,
    protected: impl Fn(&K) -> bool,
) -> [Vec<(K, String)>; 3] {
    let mut groups = groups;
    if let Some((_, text)) = groups[1].first_mut() {
        *text = format!("{prefix}{text}");
    }
    let labeled = |groups: &[Vec<(K, String)>; 3]| {
        groups[1]
            .first()
            .is_none_or(|(_, text)| text.starts_with(prefix))
    };
    let fitted = fit_footer_items(
        groups,
        width,
        |(_, text)| text.as_str(),
        |(key, _)| protected(key),
    );
    if labeled(&fitted) {
        return fitted;
    }
    let mut relabeled = fitted.clone();
    if let Some((_, text)) = relabeled[1].first_mut() {
        *text = format!("{prefix}{text}");
    }
    let relabeled = fit_footer_items(
        relabeled,
        width,
        |(_, text)| text.as_str(),
        |(key, _)| protected(key),
    );
    if labeled(&relabeled) && !relabeled[1].is_empty() {
        relabeled
    } else {
        fitted
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use std::fmt::Write as _;

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

    fn rendered_theme_rows(
        label: &str,
        width: u16,
        rows: Vec<Line<'static>>,
        metadata: &str,
    ) -> String {
        let height = u16::try_from(rows.len()).expect("theme rows fit");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(rows), frame.area()))
            .expect("render theme samples");
        let mut output = format!("=== {label} ({width}x{height}) ===\n{metadata}\n");
        output.push_str(&crate::golden::buffer_lines(terminal.backend().buffer()).join("\n"));
        output.push('\n');
        output
    }

    #[test]
    fn golden_theme_rendering() {
        let mut output = String::new();

        let mono = theme_for(UiTheme::Light, true);
        let mono_styles = with_theme(mono, || {
            format!(
                "NO_COLOR theme: {mono:?}; color-enabled light remains: {:?}; base: {:?}; selection: {:?}; active control: {:?}",
                theme_for(UiTheme::Light, false),
                base(),
                selection(true),
                active_control()
            )
        });
        let mono_rows = with_theme(mono, || {
            vec![Line::from(vec![
                Span::styled("selected", selection(true)),
                Span::raw(" "),
                Span::styled("active", active_control()),
                Span::raw(" "),
                Span::styled("canvas", base()),
            ])]
        });
        output.push_str(&rendered_theme_rows(
            "NO_COLOR monochrome controls",
            40,
            mono_rows,
            &mono_styles,
        ));

        // A terminal palette's contrast belongs to the user's scheme, so its
        // record is the styles that carry selection and focus.
        for theme in [UiTheme::TerminalDark, UiTheme::TerminalLight] {
            let (rows, styles) = with_theme(theme, || {
                let styles = format!(
                    "selection: {:?}; raised: {:?}; active control: {:?}; focus control: {:?}; field: {:?}; focused border: {:?}",
                    selection(true),
                    raised(),
                    active_control(),
                    focus_control(),
                    field(true),
                    border(true),
                );
                let palette = palette();
                let rows = vec![Line::from(vec![
                    Span::styled("selected", selection(true)),
                    Span::raw(" "),
                    Span::styled("focus", focus_control()),
                    Span::raw(" "),
                    Span::styled("muted", muted()),
                    Span::raw(" "),
                    Span::styled("error", Style::default().fg(palette.error)),
                ])];
                (rows, format!("{styles}\n{palette:?}"))
            });
            output.push_str(&rendered_theme_rows(
                &format!("{theme:?} controls"),
                40,
                rows,
                &styles,
            ));
        }

        for theme in UiTheme::ALL {
            if palette_for(theme).reverse_video {
                continue;
            }
            let palette = palette_for(theme);
            let foregrounds = [
                ("text", palette.text),
                ("muted", palette.muted),
                ("accent", palette.accent),
                ("secondary", palette.secondary),
                ("success", palette.success),
                ("warning", palette.warning),
                ("error", palette.error),
                ("session_error", palette.session_error),
                ("session_activity", palette.session_activity),
                ("session_attention", palette.session_attention),
                ("session_idle", palette.session_idle),
            ];
            let backgrounds = [
                ("background", palette.background),
                ("surface", palette.surface),
                ("surface_raised", palette.surface_raised),
                ("selection", palette.selection),
            ];
            let mut rows = Vec::new();
            let mut details = String::new();
            writeln!(
                details,
                "landmarks: background={:?}; surface={:?}; raised={:?}; selection={:?}",
                palette.background, palette.surface, palette.surface_raised, palette.selection
            )
            .expect("write palette landmarks");
            for (foreground_name, foreground) in foregrounds {
                for (background_name, background) in backgrounds {
                    rows.push(Line::from(Span::styled(
                        format!("{foreground_name} on {background_name}: Aa"),
                        Style::default().fg(foreground).bg(background),
                    )));
                    writeln!(
                        details,
                        "{foreground_name}/{background_name}: fg={foreground:?} bg={background:?} contrast={:.2}",
                        contrast(foreground, background)
                    )
                    .expect("write palette contrast");
                }
            }
            output.push_str(&rendered_theme_rows(
                &format!("{theme:?} palette and contrast surfaces"),
                56,
                rows,
                &details,
            ));
        }
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "theme-rendering", &output);
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
        // The derived Debug prints every field, so a glyph added without an
        // ASCII column is caught here instead of on a console without UTF-8.
        let table = format!("{ASCII_GLYPHS:?}");
        assert!(table.is_ascii(), "{table}");
        with_symbols(SymbolSet::Ascii, || {
            assert!(glyphs().working.is_ascii());
            assert_eq!(footer_separator(), " - ");
            // A hyphenated key survives the ASCII separator.
            let line = hints("Shift-Enter newline - Tab pane | ctrl+b then c create");
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(
                text,
                "Shift-Enter newline - Tab pane | ctrl+b then c create"
            );
            assert_eq!(line.spans[1].content, "Shift-Enter");
        });
        assert_eq!(footer_separator(), " · ");
    }
}

/// Stable pin names remain distinct when colors repeat or color is disabled.
pub fn pin_label(mut id: u32) -> String {
    let mut label = Vec::new();
    loop {
        label.push((b'A' + (id % 26) as u8) as char);
        if id < 26 {
            break;
        }
        id = id / 26 - 1;
    }
    label.into_iter().rev().collect()
}

pub fn pin_color(id: u32) -> Color {
    palette().pins[(id % 8) as usize]
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn pin_labels_remain_unique_after_the_color_palette_repeats() {
        assert_eq!(pin_label(0), "A");
        assert_eq!(pin_label(25), "Z");
        assert_eq!(pin_label(26), "AA");
        assert_eq!(pin_label(51), "AZ");
        assert_eq!(pin_label(52), "BA");
        for selected in [UiTheme::Light, UiTheme::Midnight, UiTheme::Mono] {
            with_theme(selected, || {
                assert_eq!(pin_color(0), pin_color(8));
                if selected == UiTheme::Mono {
                    assert_eq!(pin_color(0), palette().text);
                }
            });
        }
    }
}
