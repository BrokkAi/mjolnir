use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalThemeKind {
    Light,
    #[default]
    Dark,
    AnsiLight,
    AnsiDark,
}

impl TerminalThemeKind {
    pub const ALL: [Self; 4] = [Self::Light, Self::Dark, Self::AnsiLight, Self::AnsiDark];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::AnsiLight => "ansi-light",
            Self::AnsiDark => "ansi-dark",
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
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            "ansi-light" => Ok(Self::AnsiLight),
            "ansi-dark" => Ok(Self::AnsiDark),
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
}
