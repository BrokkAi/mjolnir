use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much latitude the TUI takes with color.
///
/// There is deliberately no light/dark distinction any more. Body text is drawn
/// with the terminal's own foreground and secondary text is the same color
/// dimmed, so the palette is correct on a light terminal and a dark one without
/// being told which it is — and correct on themes we have never seen, which the
/// old hand-tuned light/dark palettes could not be.
///
/// What remains is a capability question, not an aesthetic one: may we blend
/// backgrounds against the terminal's measured background, or must we restrict
/// ourselves to the ANSI 16?
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalThemeKind {
    /// Use the terminal's foreground and background, ANSI-16 accents, and
    /// background fills blended against the real background when the terminal
    /// reports it.
    ///
    /// The legacy `light` and `dark` names deserialize here: both described a
    /// background this mode now measures instead of being told.
    #[default]
    #[serde(alias = "light", alias = "dark")]
    Adaptive,
    /// Never emit anything outside the ANSI 16 — no RGB, no indexed colors, no
    /// blended fills.
    ///
    /// For terminals that overstate their capabilities, and multiplexers that
    /// mangle OSC responses badly enough that a measured background would be
    /// worse than none. Diffs fall back to foreground-only styling here.
    #[serde(alias = "ansi-light", alias = "ansi-dark")]
    Ansi,
}

impl TerminalThemeKind {
    pub const ALL: [Self; 2] = [Self::Adaptive, Self::Ansi];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Adaptive => "adaptive",
            Self::Ansi => "ansi",
        }
    }

    /// One-line explanation for the appearance settings tab.
    pub fn description(self) -> &'static str {
        match self {
            Self::Adaptive => "follow the terminal's own colors; blend diff backgrounds",
            Self::Ansi => "strict 16-color; no blended backgrounds",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl fmt::Display for TerminalThemeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TerminalThemeKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "adaptive" => Ok(Self::Adaptive),
            "ansi" => Ok(Self::Ansi),
            // Configs written before the palette became adaptive still name the
            // old themes. Accept them permanently rather than failing to start:
            // the distinction they encoded is now measured at runtime.
            "light" | "dark" => Ok(Self::Adaptive),
            "ansi-light" | "ansi-dark" => Ok(Self::Ansi),
            _ => Err(format!(
                "unknown theme {value:?}; expected one of: {}",
                Self::ALL
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_names_round_trip() {
        for kind in TerminalThemeKind::ALL {
            assert_eq!(kind.as_str().parse::<TerminalThemeKind>(), Ok(kind));
        }
        assert!("solarized".parse::<TerminalThemeKind>().is_err());
    }

    #[test]
    fn legacy_theme_names_still_parse() {
        // Users have `theme = "dark"` in config.toml today; refusing it would
        // turn a cosmetic change into a failure to start.
        for name in ["light", "dark"] {
            assert_eq!(name.parse(), Ok(TerminalThemeKind::Adaptive));
        }
        for name in ["ansi-light", "ansi-dark"] {
            assert_eq!(name.parse(), Ok(TerminalThemeKind::Ansi));
        }
    }

    #[test]
    fn legacy_theme_names_still_deserialize() {
        #[derive(Deserialize)]
        struct Holder {
            theme: TerminalThemeKind,
        }

        let dark: Holder = toml::from_str(r#"theme = "dark""#).expect("legacy dark deserializes");
        assert_eq!(dark.theme, TerminalThemeKind::Adaptive);
        let ansi: Holder =
            toml::from_str(r#"theme = "ansi-dark""#).expect("legacy ansi-dark deserializes");
        assert_eq!(ansi.theme, TerminalThemeKind::Ansi);
    }

    #[test]
    fn adaptive_is_the_default_so_untouched_configs_stay_clean() {
        // `is_default` drives `skip_serializing_if`, so a config migrated off a
        // legacy name drops the key entirely rather than pinning a new one.
        assert!(TerminalThemeKind::Adaptive.is_default());
        assert!(!TerminalThemeKind::Ansi.is_default());
    }
}
