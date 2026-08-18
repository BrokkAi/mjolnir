//! Semantic foreground intents that defer to the user's terminal theme.
//!
//! The TUI used to name an explicit color for every role, which meant every
//! supported background needed its own hand-tuned palette and any terminal
//! theme we had not anticipated (solarized, gruvbox, nord) rendered wrong with
//! no recourse but a manual setting. An [`Ink`] instead records *intent*: most
//! roles carry no color at all and lean on `Color::Reset` plus `DIM`/`BOLD`, so
//! the terminal supplies the actual pixels and stays internally consistent.
//!
//! Colors that survive are normally restricted to the ANSI 16, which every
//! terminal theme remaps against its own background. Richer visual ramps are
//! derived from the *measured* terminal background and retain an ANSI fallback
//! (see [`crate::terminal_palette`]).

use ratatui::style::{Color, Modifier, Style};

/// A foreground intent: an optional explicit color plus text modifiers.
///
/// `color: None` is the important case — it renders as `Color::Reset`, which
/// emits SGR 39 and hands the decision back to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    color: Option<Color>,
    modifier: Modifier,
}

impl Ink {
    /// The terminal's own foreground color, unmodified. Body text.
    pub const fn terminal() -> Self {
        Self {
            color: None,
            modifier: Modifier::empty(),
        }
    }

    /// The terminal's foreground, dimmed. Secondary text.
    ///
    /// `DIM` is how a 16-color-safe hierarchy is expressed: the terminal keeps
    /// ownership of the hue and only the intensity changes, so secondary text
    /// stays legible on backgrounds we never tested.
    pub const fn dim() -> Self {
        Self {
            color: None,
            modifier: Modifier::DIM,
        }
    }

    /// The terminal's foreground, bold. Headers.
    pub const fn bold() -> Self {
        Self {
            color: None,
            modifier: Modifier::BOLD,
        }
    }

    /// An explicit color, for roles that must be told apart at a glance.
    ///
    /// Callers normally pass one of the ANSI 16. Richer colors must come from
    /// the measured-background helpers; `clippy.toml` bans direct RGB and
    /// indexed constructors so fixed off-palette colors cannot enter here.
    pub const fn ansi(color: Color) -> Self {
        Self {
            color: Some(color),
            modifier: Modifier::empty(),
        }
    }

    /// An explicit ANSI color, dimmed.
    pub const fn dim_ansi(color: Color) -> Self {
        Self {
            color: Some(color),
            modifier: Modifier::DIM,
        }
    }

    /// Add `DIM` to an existing ink without changing its color.
    pub const fn with_dim(self) -> Self {
        Self {
            color: self.color,
            modifier: self.modifier.union(Modifier::DIM),
        }
    }

    /// Add `BOLD` to an existing ink.
    pub const fn with_bold(self) -> Self {
        Self {
            color: self.color,
            modifier: self.modifier.union(Modifier::BOLD),
        }
    }

    /// The color to hand ratatui, resolving "terminal default" to `Reset`.
    pub fn color(self) -> Color {
        self.color.unwrap_or(Color::Reset)
    }

    /// The explicit color, if this ink names one. `None` means the terminal
    /// decides, which callers comparing inks for distinctness need to know.
    pub fn explicit_color(self) -> Option<Color> {
        self.color
    }

    pub fn modifier(self) -> Modifier {
        self.modifier
    }

    pub fn is_dim(self) -> bool {
        self.modifier.contains(Modifier::DIM)
    }

    /// This ink as a standalone foreground style.
    pub fn style(self) -> Style {
        Style::default()
            .fg(self.color())
            .add_modifier(self.modifier)
    }
}

impl Default for Ink {
    fn default() -> Self {
        Self::terminal()
    }
}

/// `Span::styled(text, ink)` and friends accept anything `Into<Style>`, so this
/// impl keeps those call sites unchanged.
///
/// Deliberately absent: `From<Ink> for Color`. It would make the old
/// `.fg(theme.muted)` call sites compile again while silently dropping the
/// modifier that carries the hierarchy — a wrong-but-quiet migration. Forcing
/// them through [`InkStyle::ink`] makes the compiler enumerate the work.
impl From<Ink> for Style {
    fn from(ink: Ink) -> Self {
        ink.style()
    }
}

/// Applies an [`Ink`] to a [`Style`], carrying the modifier along with the color.
pub trait InkStyle {
    /// Set the foreground from `ink` and add its modifiers.
    fn ink(self, ink: Ink) -> Self;

    /// Set the background from `ink`, ignoring its modifiers.
    ///
    /// `DIM` and `BOLD` describe glyphs, not fills, so applying them to a
    /// background would leak the modifier onto whatever text sits on top.
    fn ink_bg(self, ink: Ink) -> Self;
}

impl InkStyle for Style {
    fn ink(self, ink: Ink) -> Self {
        self.fg(ink.color()).add_modifier(ink.modifier())
    }

    fn ink_bg(self, ink: Ink) -> Self {
        self.bg(ink.color())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_ink_defers_to_the_terminal_foreground() {
        // Reset is what makes a single palette work on every terminal theme:
        // it emits SGR 39 rather than committing to white or black.
        assert_eq!(Ink::terminal().color(), Color::Reset);
        assert_eq!(Ink::terminal().modifier(), Modifier::empty());
        assert_eq!(Ink::terminal().explicit_color(), None);
    }

    #[test]
    fn dim_ink_keeps_the_terminal_color_and_only_lowers_intensity() {
        let ink = Ink::dim();
        assert_eq!(ink.color(), Color::Reset);
        assert!(ink.is_dim());
    }

    #[test]
    fn colored_ink_can_be_dimmed_without_losing_its_hue() {
        let ink = Ink::ansi(Color::Red).with_dim();
        assert_eq!(ink.explicit_color(), Some(Color::Red));
        assert!(ink.is_dim());
    }

    #[test]
    fn ink_applied_to_style_carries_both_color_and_modifier() {
        let style = Style::default().ink(Ink::dim_ansi(Color::Cyan));
        assert_eq!(style.fg, Some(Color::Cyan));
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn ink_bg_drops_modifiers_so_they_do_not_leak_onto_overlaid_text() {
        let style = Style::default().ink_bg(Ink::dim_ansi(Color::Yellow));
        assert_eq!(style.bg, Some(Color::Yellow));
        assert!(!style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn ink_preserves_modifiers_already_on_the_style() {
        let style = Style::default()
            .add_modifier(Modifier::ITALIC)
            .ink(Ink::bold());
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }
}
