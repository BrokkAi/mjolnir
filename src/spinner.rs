//! Prompt-activity spinner styles.
//!
//! A spinner style is a purely client-side visual preference, mirroring
//! [`crate::theme::TerminalThemeKind`]: it is persisted in `config.toml`,
//! chosen on first run, and changeable via `/spinner` or `/mjconfig`.
//!
//! Every style renders to frames of exactly [`SPINNER_WIDTH`] display columns
//! (including its idle frame) so the prompt title never reflows when a turn
//! starts, ends, or the style changes. Frames are generated once on first use.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Display width (terminal columns) of every spinner frame, for every style.
pub const SPINNER_WIDTH: usize = 12;

/// Wall-clock dwell per animation frame. Matches the streaming redraw budget so
/// the spinner advances one frame per repaint (no skipped or aliased frames).
pub const SPINNER_FRAME_INTERVAL_MS: u128 = 125;

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
    #[default]
    Shimmer,
}

impl SpinnerStyle {
    pub const ALL: [Self; 4] = [Self::Pulse, Self::Wave, Self::Bars, Self::Shimmer];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Wave => "wave",
            Self::Bars => "bars",
            Self::Shimmer => "shimmer",
        }
    }

    /// Short human description, shown in pickers and `/spinner` listings.
    pub fn description(self) -> &'static str {
        match self {
            Self::Pulse => "a bright dot glides across a faint row",
            Self::Wave => "an undulating braille ribbon",
            Self::Bars => "bouncing equalizer bars",
            Self::Shimmer => "the whole row breathes in unison",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn index(self) -> usize {
        match self {
            Self::Pulse => 0,
            Self::Wave => 1,
            Self::Bars => 2,
            Self::Shimmer => 3,
        }
    }

    /// Animated frames for this style. Always non-empty; index with the
    /// wall-clock tick (`now / SPINNER_FRAME_INTERVAL_MS % frames.len()`).
    pub fn frames(self) -> &'static [String] {
        &FRAME_SETS[self.index()].animated
    }

    /// Resting frame shown when no turn is in flight.
    pub fn idle_frame(self) -> &'static str {
        FRAME_SETS[self.index()].idle.as_str()
    }

    /// Animation frame for the current wall-clock instant. Driven purely by real
    /// time so the spinner advances at a steady rate regardless of redraw cadence
    /// and stays in sync across every place it is shown.
    pub fn current_frame(self) -> &'static str {
        let frames = self.frames();
        let idx = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| (elapsed.as_millis() / SPINNER_FRAME_INTERVAL_MS) as usize)
            .unwrap_or(0)
            % frames.len();
        frames[idx].as_str()
    }
}

impl fmt::Display for SpinnerStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpinnerStyle {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pulse" => Ok(Self::Pulse),
            "wave" => Ok(Self::Wave),
            "bars" => Ok(Self::Bars),
            "shimmer" => Ok(Self::Shimmer),
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

struct FrameSet {
    animated: Vec<String>,
    idle: String,
}

/// All styles' frames, generated once and kept for the process lifetime.
/// Ordered to match [`SpinnerStyle::index`].
static FRAME_SETS: LazyLock<[FrameSet; 4]> =
    LazyLock::new(|| [build_pulse(), build_wave(), build_bars(), build_shimmer()]);

fn row(s: String) -> String {
    debug_assert_eq!(
        unicode_width::UnicodeWidthStr::width(s.as_str()),
        SPINNER_WIDTH,
        "spinner frame {s:?} must be {SPINNER_WIDTH} columns wide"
    );
    s
}

/// A bright dot glides left-to-right and wraps, with a symmetric brightness
/// falloff on either side so it reads as a soft pulse rather than a hard pip.
fn build_pulse() -> FrameSet {
    let w = SPINNER_WIDTH;
    let glyph = |dist: usize| match dist {
        0 => '●',
        1 => '•',
        2 => '∙',
        _ => '·',
    };
    let animated = (0..w)
        .map(|head| {
            let cells: String = (0..w)
                .map(|x| {
                    // ring distance from the head, so the pulse wraps seamlessly
                    let d = ((x + w - head) % w).min((head + w - x) % w);
                    glyph(d)
                })
                .collect();
            row(cells)
        })
        .collect();
    FrameSet {
        animated,
        idle: "·".repeat(w),
    }
}

/// An undulating braille ribbon that scrolls one full wavelength per loop.
fn build_wave() -> FrameSet {
    let w = SPINNER_WIDTH;
    // Horizontal braille bars (both columns lit) at four vertical heights,
    // top to bottom: ⠉ ⠒ ⠤ ⣀.
    let levels = [
        char::from_u32(0x2800 + 0x09).unwrap(),
        char::from_u32(0x2800 + 0x12).unwrap(),
        char::from_u32(0x2800 + 0x24).unwrap(),
        char::from_u32(0x2800 + 0xC0).unwrap(),
    ];
    const N: usize = 8;
    let animated = (0..N)
        .map(|i| {
            let cells: String = (0..w)
                .map(|x| {
                    let phase = std::f64::consts::TAU * (x as f64 / w as f64)
                        - std::f64::consts::TAU * (i as f64 / N as f64);
                    let v = phase.sin();
                    let lvl = (((1.0 - (v + 1.0) / 2.0) * 3.0).round() as i64).clamp(0, 3) as usize;
                    levels[lvl]
                })
                .collect();
            row(cells)
        })
        .collect();
    FrameSet {
        animated,
        idle: levels[1].to_string().repeat(w),
    }
}

/// Vertical eighth-block bars whose heights ripple like an equalizer.
fn build_bars() -> FrameSet {
    let w = SPINNER_WIDTH;
    let bars = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const N: usize = 10;
    let animated = (0..N)
        .map(|i| {
            let cells: String = (0..w)
                .map(|x| {
                    let phase = std::f64::consts::TAU * (x as f64 / 6.0)
                        + std::f64::consts::TAU * (i as f64 / N as f64);
                    let v = (phase.sin() + 1.0) / 2.0; // 0..1
                    let h = 1 + (v * 7.0).round() as usize; // 1..8 (never blank)
                    bars[h]
                })
                .collect();
            row(cells)
        })
        .collect();
    FrameSet {
        animated,
        idle: "▁".repeat(w),
    }
}

/// The whole row brightens and dims together — a calm, confident "working…".
fn build_shimmer() -> FrameSet {
    let w = SPINNER_WIDTH;
    let ramp = ['·', '·', '∙', '•', '●', '●', '•', '∙'];
    let animated = ramp.iter().map(|c| row(c.to_string().repeat(w))).collect();
    FrameSet {
        animated,
        idle: "·".repeat(w),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn style_names_round_trip() {
        for style in SpinnerStyle::ALL {
            assert_eq!(style.as_str().parse::<SpinnerStyle>(), Ok(style));
        }
        assert!("spiral".parse::<SpinnerStyle>().is_err());
    }

    #[test]
    fn default_is_shimmer_and_only_default_is_default() {
        assert_eq!(SpinnerStyle::default(), SpinnerStyle::Shimmer);
        for style in SpinnerStyle::ALL {
            assert_eq!(style.is_default(), style == SpinnerStyle::Shimmer);
        }
    }

    #[test]
    fn every_frame_has_stable_display_width() {
        for style in SpinnerStyle::ALL {
            assert!(!style.frames().is_empty(), "{style} has no frames");
            for frame in style.frames() {
                assert_eq!(
                    UnicodeWidthStr::width(frame.as_str()),
                    SPINNER_WIDTH,
                    "{style} frame {frame:?} wrong width"
                );
            }
            assert_eq!(
                UnicodeWidthStr::width(style.idle_frame()),
                SPINNER_WIDTH,
                "{style} idle wrong width"
            );
        }
    }

    #[test]
    fn loops_are_brisk() {
        // Each style is one short, lively loop at the streaming cadence.
        for style in SpinnerStyle::ALL {
            let loop_ms = style.frames().len() as u128 * SPINNER_FRAME_INTERVAL_MS;
            assert!(
                (700..=2_000).contains(&loop_ms),
                "{style} loop_ms = {loop_ms}"
            );
        }
    }
}
