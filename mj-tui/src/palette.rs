//! The semantic palette the TUI draws with.
//!
//! Every foreground role is an [`Ink`], and most of them carry no color at all:
//! body text is the terminal's own foreground, secondary text is that same
//! foreground dimmed, headers are it in bold. Only roles that genuinely need to
//! be told apart mid-sentence — success, error, the agent versus the user —
//! spend one of the ANSI 16, which every terminal theme remaps to something
//! that contrasts with its own background.
//!
//! Rich colors are always derived rather than assumed: diff backgrounds and
//! the scan-light intensity ramp blend from the terminal's measured background
//! toward a hue. When that is unavailable, both fall back to ANSI-safe roles.

use ratatui::style::Color;

use crate::ink::Ink;
use crate::spinner::{SCAN_RED_LEVELS, SpinnerInk};
use crate::terminal_palette::{self, DefaultColors, StdoutColorLevel};
use crate::theme::TerminalThemeKind;

/// Hues the diff fills blend toward. These are never rendered directly — only
/// as a low-alpha composite over the terminal's background — so they are picked
/// for hue rather than for legibility on any particular backdrop.
const ADDED_TINT: (u8, u8, u8) = (46, 160, 67);
const REMOVED_TINT: (u8, u8, u8) = (248, 81, 73);
/// Keeps the resting rail visible while leaving enough range for the peak.
const SCAN_RED_MIN_ALPHA: f32 = 0.32;
/// Whole-row fill: present enough to band the row, faint enough to read text through.
const ROW_ALPHA: f32 = 0.16;
/// Changed-token fill, which must be distinguishable from the row it sits in.
const EMPH_ALPHA: f32 = 0.34;
/// Selection fill, blended from the background toward the foreground so it
/// inverts by the terminal's own contrast rather than a color we picked.
const SELECTION_ALPHA: f32 = 0.28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTheme {
    pub kind: TerminalThemeKind,
    pub text: Ink,
    pub muted: Ink,
    pub subtle: Ink,
    pub header: Ink,
    pub primary: Ink,
    pub secondary: Ink,
    pub accent: Ink,
    pub success: Ink,
    pub warning: Ink,
    pub error: Ink,
    pub selection_fg: Ink,
    pub selection_bg: Ink,
    pub user: Ink,
    pub agent: Ink,
    pub thought: Ink,
    pub tip: Ink,
    pub tool: Ink,
    pub code: Ink,
    pub terminal: Ink,
    pub quote: Ink,
    pub diff_added: Ink,
    pub diff_removed: Ink,
    pub diff_context: Ink,
    /// Row and changed-token background fills for diff rendering. `None` falls
    /// back to foreground-only styling, which happens whenever the terminal
    /// declined to report its background, cannot render more than 16 colors, or
    /// the user pinned [`TerminalThemeKind::Ansi`].
    pub diff_added_bg: Option<Color>,
    pub diff_removed_bg: Option<Color>,
    pub diff_added_emph_bg: Option<Color>,
    pub diff_removed_emph_bg: Option<Color>,
    pub permission: Ink,
    scan_red: [Ink; SCAN_RED_LEVELS],
}

pub trait TerminalThemeKindExt {
    fn palette(self) -> TerminalTheme;
    fn palette_with(
        self,
        terminal: Option<DefaultColors>,
        level: StdoutColorLevel,
    ) -> TerminalTheme;
}

impl TerminalThemeKindExt for TerminalThemeKind {
    /// The palette for this mode, using whatever the startup probe learned
    /// about the terminal.
    fn palette(self) -> TerminalTheme {
        self.palette_with(
            terminal_palette::default_colors(),
            terminal_palette::stdout_color_level(),
        )
    }

    /// The palette for explicitly supplied terminal colors and color level, so
    /// the adaptation can be tested without a real terminal.
    fn palette_with(
        self,
        terminal: Option<DefaultColors>,
        level: StdoutColorLevel,
    ) -> TerminalTheme {
        // Strict ANSI mode refuses every derived fill, so it behaves as though
        // the terminal never answered the color queries.
        let measured = match self {
            Self::Adaptive => terminal,
            Self::Ansi => None,
        };
        let tint = |hue: (u8, u8, u8), alpha: f32| -> Option<Color> {
            let bg = measured?.bg;
            terminal_palette::best_color_for_level(terminal_palette::blend(hue, bg, alpha), level)
        };
        let scan_red = scan_red_ramp(measured, level);

        TerminalTheme {
            kind: self,
            // Hierarchy lives on the modifier axis: same color, less intensity.
            // This is what removes the need for per-background palettes.
            text: Ink::terminal(),
            muted: Ink::dim(),
            subtle: Ink::dim(),
            header: Ink::bold(),
            // Accents are restricted to the ANSI 16 so the terminal theme keeps
            // control of the actual hue.
            primary: Ink::ansi(Color::Cyan),
            secondary: Ink::ansi(Color::Magenta),
            accent: Ink::ansi(Color::Blue),
            success: Ink::ansi(Color::Green),
            warning: Ink::ansi(Color::Yellow),
            error: Ink::ansi(Color::Red),
            selection_fg: selection_fg(measured),
            selection_bg: selection_bg(measured, level),
            user: Ink::ansi(Color::Cyan),
            agent: Ink::ansi(Color::Green),
            thought: Ink::dim(),
            tip: Ink::ansi(Color::Blue),
            tool: Ink::ansi(Color::Magenta),
            code: Ink::ansi(Color::Yellow),
            terminal: Ink::ansi(Color::Yellow),
            quote: Ink::dim(),
            diff_added: Ink::ansi(Color::Green),
            diff_removed: Ink::ansi(Color::Red),
            diff_context: Ink::dim(),
            diff_added_bg: tint(ADDED_TINT, ROW_ALPHA),
            diff_removed_bg: tint(REMOVED_TINT, ROW_ALPHA),
            diff_added_emph_bg: tint(ADDED_TINT, EMPH_ALPHA),
            diff_removed_emph_bg: tint(REMOVED_TINT, EMPH_ALPHA),
            permission: Ink::ansi(Color::Yellow),
            scan_red,
        }
    }
}

/// Text drawn on top of [`selection_bg`].
///
/// A selection is the one place a hardcoded black or white is right, and the
/// style guide's stated exception: the fill underneath is one *we* painted, so
/// the terminal's default foreground has no guaranteed contrast against it.
fn selection_fg(terminal: Option<DefaultColors>) -> Ink {
    match terminal {
        // The fill leans toward the terminal's foreground, so the text on it
        // has to lean back toward the background to stay legible.
        Some(colors) if terminal_palette::is_light(colors.bg) => Ink::ansi(Color::White),
        // Dark terminal (light fill) and the unmeasured case (a cyan fill,
        // which is bright in every terminal theme) both want dark text.
        _ => Ink::ansi(Color::Black),
    }
}

/// The selection fill.
///
/// Blended from the terminal's background toward its *own* foreground rather
/// than toward absolute white, so on a tinted theme the selection reads as a
/// shade of that theme instead of a gray we invented.
fn selection_bg(terminal: Option<DefaultColors>, level: StdoutColorLevel) -> Ink {
    let derived = terminal.and_then(|colors| {
        terminal_palette::best_color_for_level(
            terminal_palette::blend(colors.fg, colors.bg, SELECTION_ALPHA),
            level,
        )
    });
    match derived {
        Some(color) => Ink::ansi(color),
        // Cyan is the conventional selection accent and is legible in every
        // terminal theme, which a derived gray cannot be guaranteed to be when
        // we could not measure the background it came from.
        None => Ink::ansi(Color::Cyan),
    }
}

impl TerminalTheme {
    /// Resolve a spinner's semantic ink to a palette role.
    ///
    /// Every arm reuses a role the theme already declares rather than mixing a
    /// new color, which is what lets the animated styles keep their gradients
    /// on 16-color terminals as well as truecolor ones.
    pub fn spinner_ink(self, ink: SpinnerInk) -> Ink {
        match ink {
            // Faint is the resting rail and the cold end of every gradient. It
            // no longer needs the special-casing the old palettes required: DIM
            // is always a step below plain text, on every terminal, so the
            // gradient can never invert.
            SpinnerInk::Faint => self.subtle,
            SpinnerInk::Cool => self.primary,
            SpinnerInk::Bright => self.accent,
            SpinnerInk::Vivid => self.secondary,
            SpinnerInk::Calm => self.success,
            SpinnerInk::Warm => self.warning,
            SpinnerInk::Hot => self.error,
            SpinnerInk::Red(level) => self.scan_red[usize::from(level).min(SCAN_RED_LEVELS - 1)],
        }
    }
}

fn scan_red_ramp(
    terminal: Option<DefaultColors>,
    level: StdoutColorLevel,
) -> [Ink; SCAN_RED_LEVELS] {
    std::array::from_fn(|index| {
        let fallback = match index {
            0..=5 => Ink::ansi(Color::Red),
            6..=8 => Ink::ansi(Color::Red).with_bold(),
            9..=10 => Ink::ansi(Color::LightRed),
            _ => Ink::ansi(Color::LightRed).with_bold(),
        };
        let Some(background) = terminal.map(|colors| colors.bg) else {
            return fallback;
        };
        let progress = index as f32 / (SCAN_RED_LEVELS - 1) as f32;
        let alpha = SCAN_RED_MIN_ALPHA + progress * (1.0 - SCAN_RED_MIN_ALPHA);
        terminal_palette::best_color_for_level(
            terminal_palette::blend(REMOVED_TINT, background, alpha),
            level,
        )
        .map(Ink::ansi)
        .unwrap_or(fallback)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solarized dark, as the terminal would report it.
    const DARK: Option<DefaultColors> = Some(DefaultColors {
        fg: (131, 148, 150),
        bg: (0, 43, 54),
    });
    /// Solarized light.
    const LIGHT: Option<DefaultColors> = Some(DefaultColors {
        fg: (101, 123, 131),
        bg: (253, 246, 227),
    });

    fn adaptive(terminal: Option<DefaultColors>) -> TerminalTheme {
        TerminalThemeKind::Adaptive.palette_with(terminal, StdoutColorLevel::TrueColor)
    }

    #[test]
    fn body_text_never_pins_a_foreground_color() {
        // The entire point of the adaptive palette: on any terminal theme, body
        // text is that theme's foreground rather than our idea of white.
        for bg in [DARK, LIGHT, None] {
            assert_eq!(adaptive(bg).text.explicit_color(), None);
        }
        assert_eq!(
            TerminalThemeKind::Ansi
                .palette_with(DARK, StdoutColorLevel::TrueColor)
                .text
                .explicit_color(),
            None
        );
    }

    #[test]
    fn secondary_roles_are_dimmed_rather_than_recolored() {
        let theme = adaptive(DARK);
        for role in [
            theme.muted,
            theme.subtle,
            theme.thought,
            theme.quote,
            theme.diff_context,
        ] {
            assert_eq!(role.explicit_color(), None, "secondary role pinned a color");
            assert!(role.is_dim(), "secondary role is not dimmed");
        }
    }

    #[test]
    fn accent_roles_stay_within_the_ansi_sixteen() {
        // RGB foregrounds are what break on unfamiliar terminal themes, so no
        // role may introduce one regardless of the terminal's capabilities.
        for kind in TerminalThemeKind::ALL {
            let theme = kind.palette_with(DARK, StdoutColorLevel::TrueColor);
            for role in [
                theme.primary,
                theme.secondary,
                theme.accent,
                theme.success,
                theme.warning,
                theme.error,
                theme.user,
                theme.agent,
                theme.tool,
                theme.code,
                theme.terminal,
                theme.permission,
                theme.diff_added,
                theme.diff_removed,
            ] {
                let Some(color) = role.explicit_color() else {
                    continue;
                };
                assert!(
                    matches!(
                        color,
                        Color::Black
                            | Color::Red
                            | Color::Green
                            | Color::Yellow
                            | Color::Blue
                            | Color::Magenta
                            | Color::Cyan
                            | Color::White
                    ),
                    "{kind} uses off-palette foreground {color:?}"
                );
            }
        }
    }

    #[test]
    fn diff_fills_are_blended_from_the_measured_background() {
        // A dark terminal should get a dark green row, a light terminal a pale
        // one, from the same palette definition.
        let dark = adaptive(DARK);
        let light = adaptive(LIGHT);
        let (Some(Color::Rgb(_, dg, _)), Some(Color::Rgb(_, lg, _))) =
            (dark.diff_added_bg, light.diff_added_bg)
        else {
            panic!("expected blended diff fills on a truecolor terminal");
        };
        assert!(
            lg > dg,
            "light-terminal fill ({lg}) should be brighter than dark-terminal fill ({dg})"
        );
    }

    #[test]
    fn emphasis_fill_is_stronger_than_the_row_fill() {
        // Changed tokens have to be visible against the row they sit inside.
        let theme = adaptive(DARK);
        assert_ne!(theme.diff_added_bg, theme.diff_added_emph_bg);
        assert_ne!(theme.diff_removed_bg, theme.diff_removed_emph_bg);
    }

    #[test]
    fn diff_fills_are_dropped_when_the_background_is_unknown() {
        // Guessing a fill against an unmeasured background is how you get
        // unreadable diffs; foreground-only styling is the safe fallback.
        let theme = adaptive(None);
        assert_eq!(theme.diff_added_bg, None);
        assert_eq!(theme.diff_removed_bg, None);
        assert_eq!(theme.diff_added_emph_bg, None);
        assert_eq!(theme.diff_removed_emph_bg, None);
    }

    #[test]
    fn diff_fills_are_dropped_on_sixteen_color_terminals() {
        let theme = TerminalThemeKind::Adaptive.palette_with(DARK, StdoutColorLevel::Ansi16);
        assert_eq!(theme.diff_added_bg, None);
        assert_eq!(theme.diff_removed_bg, None);
    }

    #[test]
    fn strict_ansi_mode_refuses_derived_fills_even_when_measurable() {
        let theme = TerminalThemeKind::Ansi.palette_with(DARK, StdoutColorLevel::TrueColor);
        assert_eq!(theme.diff_added_bg, None);
        assert_eq!(theme.diff_removed_bg, None);
        assert_eq!(theme.selection_bg, Ink::ansi(Color::Cyan));
    }

    #[test]
    fn spinner_faint_is_distinguishable_from_body_text() {
        // Faint is the resting rail; if it matched body text the idle ornament
        // would read as loud and every gradient would invert.
        for kind in TerminalThemeKind::ALL {
            for bg in [DARK, LIGHT, None] {
                let palette = kind.palette_with(bg, StdoutColorLevel::TrueColor);
                assert_ne!(
                    palette.spinner_ink(SpinnerInk::Faint),
                    palette.text,
                    "{kind} faint ink is indistinguishable from body text"
                );
            }
        }
    }

    #[test]
    fn semantic_spinner_inks_are_drawn_from_the_active_palette() {
        for kind in TerminalThemeKind::ALL {
            let palette = kind.palette_with(DARK, StdoutColorLevel::TrueColor);
            let declared = [
                palette.subtle,
                palette.primary,
                palette.secondary,
                palette.accent,
                palette.success,
                palette.warning,
                palette.error,
            ];
            for ink in [
                SpinnerInk::Faint,
                SpinnerInk::Cool,
                SpinnerInk::Bright,
                SpinnerInk::Vivid,
                SpinnerInk::Calm,
                SpinnerInk::Warm,
                SpinnerInk::Hot,
            ] {
                assert!(
                    declared.contains(&palette.spinner_ink(ink)),
                    "{kind} resolves {ink:?} to an off-palette ink"
                );
            }
        }
    }

    #[test]
    fn adaptive_scan_ramp_has_a_distinct_color_for_every_level() {
        let palette = TerminalThemeKind::Adaptive.palette_with(DARK, StdoutColorLevel::TrueColor);
        let colors: Vec<(u8, u8, u8)> = (0..SCAN_RED_LEVELS)
            .map(|level| {
                match palette
                    .spinner_ink(SpinnerInk::Red(level as u8))
                    .explicit_color()
                    .expect("adaptive scan level should have an explicit color")
                {
                    Color::Rgb(r, g, b) => (r, g, b),
                    color => panic!("adaptive scan level used {color:?} instead of RGB"),
                }
            })
            .collect();

        assert!(colors.windows(2).all(|pair| pair[0] != pair[1]));
        let background = DARK.expect("test background").bg;
        let distances: Vec<f32> = colors
            .iter()
            .map(|color| terminal_palette::perceptual_distance(*color, background))
            .collect();
        assert!(
            distances.windows(2).all(|pair| pair[0] < pair[1]),
            "scan ramp should brighten monotonically: {distances:?}"
        );
    }

    #[test]
    fn ansi_scan_ramp_stays_within_the_terminal_red_slots() {
        let palette = TerminalThemeKind::Ansi.palette_with(DARK, StdoutColorLevel::TrueColor);
        for level in 0..SCAN_RED_LEVELS {
            assert!(matches!(
                palette
                    .spinner_ink(SpinnerInk::Red(level as u8))
                    .explicit_color(),
                Some(Color::Red | Color::LightRed)
            ));
        }
        assert!(
            !palette.spinner_ink(SpinnerInk::Red(0)).is_dim(),
            "the resting rail should remain visibly red"
        );
    }

    #[test]
    fn selection_text_leans_opposite_the_terminal_background() {
        // The fill moves toward the foreground, so the text on it has to move
        // back toward the background to stay legible.
        assert_eq!(adaptive(DARK).selection_fg, Ink::ansi(Color::Black));
        assert_eq!(adaptive(LIGHT).selection_fg, Ink::ansi(Color::White));
        // Unmeasured backgrounds fall back to a cyan fill, which is bright in
        // every terminal theme, so dark text is the safe pairing.
        assert_eq!(adaptive(None).selection_fg, Ink::ansi(Color::Black));
    }

    #[test]
    fn selection_always_has_contrast_between_its_foreground_and_fill() {
        for bg in [DARK, LIGHT, None] {
            let theme = adaptive(bg);
            assert_ne!(theme.selection_fg, theme.selection_bg);
            // The fill must name a real color; a Reset fill would leave the
            // selection foreground painted onto the ordinary background.
            assert!(theme.selection_bg.explicit_color().is_some());
        }
    }
}
