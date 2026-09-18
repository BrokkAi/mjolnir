//! Terminal appearance settings.

use anyhow::Result;

use super::is_false;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SpinnerStyle {
    /// A bright dot glides across a faint row (typing-indicator feel).
    Pulse,
    /// An undulating braille ribbon rolls across the strip.
    Wave,
    /// Vertical bars bounce like an audio equalizer.
    Bars,
    /// The whole row breathes brightness in unison (calmest).
    Shimmer,
    /// A lit sphere rotates in place, carrying its dark side into view.
    Globe,
    /// A lit head sweeps to one wall and back, trailing a fading tail.
    #[default]
    Scan,
}

impl SpinnerStyle {
    pub const ALL: [Self; 6] = [
        Self::Pulse,
        Self::Wave,
        Self::Bars,
        Self::Shimmer,
        Self::Globe,
        Self::Scan,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Wave => "wave",
            Self::Bars => "bars",
            Self::Shimmer => "shimmer",
            Self::Globe => "globe",
            Self::Scan => "scan",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Next animation in the command palette's stable cycle.
    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|style| *style == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

impl std::fmt::Display for SpinnerStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SpinnerStyle {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pulse" => Ok(Self::Pulse),
            "wave" => Ok(Self::Wave),
            "bars" => Ok(Self::Bars),
            "shimmer" => Ok(Self::Shimmer),
            "globe" => Ok(Self::Globe),
            "scan" => Ok(Self::Scan),
            _ => Err(format!(
                "unknown spinner {value:?}; expected one of: {}",
                Self::ALL
                    .iter()
                    .map(|style| style.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Color palette for the terminal dashboard and conversation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiTheme {
    #[default]
    Midnight,
    Light,
    #[serde(rename = "darcula", alias = "dracula")]
    Darcula,
    HighContrast,
}

impl UiTheme {
    pub const ALL: [Self; 4] = [
        Self::Midnight,
        Self::Light,
        Self::Darcula,
        Self::HighContrast,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Midnight => "Midnight",
            Self::Light => "Light",
            Self::Darcula => "Darcula",
            Self::HighContrast => "High Contrast",
        }
    }

    pub(super) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionsSide {
    #[default]
    Left,
    Right,
}

impl SessionsSide {
    pub(super) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Settings that are useful while diagnosing or tuning the client surface.
///
/// The section is optional on disk so configurations written before it was
/// introduced retain their existing representation and behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    #[serde(skip_serializing_if = "is_false")]
    pub detailed_activity_clocks: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub show_stopped_sessions: bool,
    #[serde(skip_serializing_if = "SessionOrder::is_default")]
    pub session_order: SessionOrder,
}

/// How the Sessions pane orders its rows.
///
/// `Project` groups sessions under a heading per project, in creation order,
/// which keeps related work together. `Priority` drops the headings and lists
/// the sessions that need a person first: waiting for input, then failed,
/// then unread, then working, then idle, newest activity first within a level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOrder {
    #[default]
    Project,
    Priority,
}

impl SessionOrder {
    pub(super) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl AdvancedConfig {
    pub(super) fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
