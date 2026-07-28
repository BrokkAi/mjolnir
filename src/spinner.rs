//! Prompt-activity spinner styles.
//!
//! A spinner style is a purely client-side visual preference, mirroring
//! [`crate::theme::TerminalThemeKind`]: it is persisted in `config.toml`,
//! chosen on first run, and changeable via the `/mjconfig` menu.
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

/// Wall-clock dwell per animation frame. Kept deliberately calmer than
/// streaming redraws so progress reads as steady activity without making
/// queued prompt typing feel visually noisy.
pub const SPINNER_FRAME_INTERVAL_MS: u128 = 250;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SpinnerStyle {
    /// A bright dot glides across a faint row (typing-indicator feel).
    Pulse,
    /// An undulating braille ribbon rolls across the strip.
    Wave,
    /// Vertical bars bounce like an audio equalizer.
    Bars,
    /// A pair of spinning hammers flies across the strip.
    Hammers,
    /// The whole row breathes brightness in unison (calmest).
    #[default]
    Shimmer,
}

impl SpinnerStyle {
    pub const ALL: [Self; 5] = [
        Self::Pulse,
        Self::Wave,
        Self::Bars,
        Self::Hammers,
        Self::Shimmer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Wave => "wave",
            Self::Bars => "bars",
            Self::Hammers => "hammers",
            Self::Shimmer => "shimmer",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn index(self) -> usize {
        // Derived from ALL so it cannot drift from FRAME_SETS (also ALL-ordered).
        Self::ALL
            .iter()
            .position(|style| *style == self)
            .unwrap_or(0)
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
            "hammers" => Ok(Self::Hammers),
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

fn frame_set_for(style: SpinnerStyle) -> FrameSet {
    match style {
        SpinnerStyle::Pulse => build_pulse(),
        SpinnerStyle::Wave => build_wave(),
        SpinnerStyle::Bars => build_bars(),
        SpinnerStyle::Hammers => build_hammers(),
        SpinnerStyle::Shimmer => build_shimmer(),
    }
}

/// All styles' frames, generated once and kept for the process lifetime. Built
/// by mapping over [`SpinnerStyle::ALL`], so `FRAME_SETS[style.index()]` is
/// always `style`'s frames — the array length and the exhaustive match in
/// `frame_set_for` force this to stay correct when a variant is added.
static FRAME_SETS: LazyLock<[FrameSet; 5]> = LazyLock::new(|| SpinnerStyle::ALL.map(frame_set_for));

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

/// Two hammers fly around a four-position track. A corner arc orbits each
/// visible hammer while the pair advances, suggesting rotation without blinking
/// the hammers out or changing the frame width.
fn build_hammers() -> FrameSet {
    const ROTATION: [&str; 4] = ["◜🔨", "🔨◝", "🔨◞", "◟🔨"];
    const SLOTS: usize = SPINNER_WIDTH / 3;
    let animated = (0..SLOTS * 2)
        .map(|phase| {
            let mut slots = ["   "; SLOTS];
            let first = (phase / 2) % SLOTS;
            let second = (first + 1) % SLOTS;
            slots[first] = ROTATION[phase % ROTATION.len()];
            slots[second] = ROTATION[(phase + 2) % ROTATION.len()];
            row(slots.concat())
        })
        .collect();

    let mut idle = ["   "; SLOTS];
    idle[0] = " 🔨";
    idle[2] = " 🔨";
    FrameSet {
        animated,
        idle: row(idle.concat()),
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
    fn loops_are_calm_progress_indicators() {
        // Each style should keep moving without reading as frantic activity.
        for style in SpinnerStyle::ALL {
            let loop_ms = style.frames().len() as u128 * SPINNER_FRAME_INTERVAL_MS;
            assert!(
                (1_500..=3_500).contains(&loop_ms),
                "{style} loop_ms = {loop_ms}"
            );
        }
    }

    #[test]
    fn each_style_maps_to_its_own_frames() {
        // Guards against a frame_set_for mis-mapping (e.g. two arms building the
        // same set) and against FRAME_SETS desyncing from ALL/index().
        for (i, a) in SpinnerStyle::ALL.iter().enumerate() {
            for b in &SpinnerStyle::ALL[i + 1..] {
                assert_ne!(a.frames(), b.frames(), "{a} and {b} share frames");
            }
        }
    }

    #[test]
    fn hammers_stay_visible_while_rotating_through_unique_throw_frames() {
        let frames = SpinnerStyle::Hammers.frames();
        assert_eq!(frames.len(), 8);
        let unique = frames.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), frames.len(), "throw frames must not repeat");

        let throw_positions = frames
            .iter()
            .map(|frame| {
                frame
                    .match_indices("🔨")
                    .map(|(byte, _)| UnicodeWidthStr::width(&frame[..byte]) / 3)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let expected_positions = vec![
            vec![0, 1],
            vec![0, 1],
            vec![1, 2],
            vec![1, 2],
            vec![2, 3],
            vec![2, 3],
            vec![0, 3],
            vec![0, 3],
        ];
        assert_eq!(
            throw_positions, expected_positions,
            "hammers must advance smoothly through every throw position"
        );

        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.matches("🔨").count(), 2, "frame {index}: {frame:?}");
            let rotation_marks = frame
                .chars()
                .filter(|c| matches!(c, '◜' | '◝' | '◞' | '◟'))
                .count();
            assert_eq!(rotation_marks, 2, "frame {index}: {frame:?}");
        }
    }
}
