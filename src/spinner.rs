//! Prompt-activity spinner styles.
//!
//! A spinner style is a purely client-side visual preference, mirroring
//! [`crate::theme::TerminalThemeKind`]: it is persisted in `config.toml`,
//! chosen on first run, and changeable via the `/mjconfig` menu.
//!
//! Every style renders to frames of exactly [`SPINNER_WIDTH`] display columns
//! (including its idle frame) so the prompt title never reflows when a turn
//! starts, ends, or the style changes. Frames are generated once on first use.
//!
//! Frames carry color as [`SpinnerInk`] slots rather than concrete colors. Two
//! of the four themes are 16-color ANSI palettes that cannot express an RGB
//! ramp, so the palette resolves each slot to a color it already defines and
//! every style stays legible everywhere.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Display width (terminal columns) of every spinner frame, for every style.
pub const SPINNER_WIDTH: usize = 12;

/// Resting ornament shown when no turn is in flight.
const IDLE_GLYPH: char = '─';

/// Wall-clock dwell per animation frame. Kept deliberately calmer than
/// streaming redraws so progress reads as steady activity without making
/// queued prompt typing feel visually noisy.
pub const SPINNER_FRAME_INTERVAL_MS: u128 = 250;

/// Color slot for one spinner cell, resolved against the active palette by
/// [`crate::palette::TerminalTheme::spinner_ink`]. Styles emit slots rather
/// than colors so one frame set serves both the truecolor and the 16-color
/// ANSI themes.
///
/// The first four are a cold-to-hot energy ramp used by the motion styles; the
/// last three are the metered green/amber/red ramp `Bars` reads as an audio
/// meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinnerInk {
    /// Resting rail and the coldest cells; recedes toward the border.
    Faint,
    /// The body of the motion.
    Cool,
    /// The leading edge.
    Bright,
    /// The single hottest cell of a frame.
    Vivid,
    /// Low end of a metered ramp.
    Calm,
    /// Middle of a metered ramp.
    Warm,
    /// Peak of a metered ramp.
    Hot,
}

/// One rendered frame: the glyph row together with the ink each glyph takes.
/// Built as a unit from a single `(char, ink)` sequence, so a style cannot ship
/// colors that disagree with the glyphs they are meant to shade.
///
/// Both representations are computed once, at construction: like the frames
/// themselves they never change, and they are read on every redraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinnerFrame {
    text: String,
    runs: Vec<(String, SpinnerInk)>,
}

impl SpinnerFrame {
    /// The frame's glyphs, without color. Used where a plain string is all the
    /// surface can carry (the web `/mjconfig` preview, width assertions).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The frame split into maximal same-ink runs, one styled span each.
    /// Merging keeps a twelve-cell strip down to a handful of spans, and
    /// borrowing the run text keeps a redraw from allocating per span.
    pub fn runs(&self) -> &[(String, SpinnerInk)] {
        &self.runs
    }
}

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
    /// A lit sphere rotates in place, carrying its dark side into view.
    Globe,
}

impl SpinnerStyle {
    pub const ALL: [Self; 5] = [
        Self::Pulse,
        Self::Wave,
        Self::Bars,
        Self::Shimmer,
        Self::Globe,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Wave => "wave",
            Self::Bars => "bars",
            Self::Shimmer => "shimmer",
            Self::Globe => "globe",
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
    pub fn frames(self) -> &'static [SpinnerFrame] {
        &FRAME_SETS[self.index()].animated
    }

    /// Resting frame shown when no turn is in flight.
    pub fn idle_frame(self) -> &'static SpinnerFrame {
        &FRAME_SETS[self.index()].idle
    }

    /// Animation frame for the current wall-clock instant. Driven purely by real
    /// time so the spinner advances at a steady rate regardless of redraw cadence
    /// and stays in sync across every place it is shown.
    pub fn current_frame(self) -> &'static SpinnerFrame {
        let frames = self.frames();
        &frames[current_frame_index(frames.len())]
    }

    /// Single-column animation frame for compact progress surfaces. This is a
    /// separate contract from the twelve-column prompt ornament: callers never
    /// need to infer a usable glyph from the internals of a wide frame.
    pub fn compact_frame(self) -> &'static str {
        let frames = self.compact_frames();
        frames[current_frame_index(frames.len())]
    }

    fn compact_frames(self) -> &'static [&'static str] {
        match self {
            Self::Pulse => &["·", "∙", "•", "●", "•", "∙"],
            Self::Wave => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            Self::Bars => &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█", "▅", "▃"],
            Self::Shimmer => &["·", "∙", "•", "●", "•", "∙"],
            // Moon glyphs are double-width, so the compact slot spins the same
            // sphere with single-column circles instead. On a dark terminal the
            // filled half is the lit half, so this tracks GLOBE_PHASES' sweep:
            // dark → lit on the right → full → lit on the left.
            Self::Globe => &["○", "◑", "●", "◐"],
        }
    }
}

fn current_frame_index(frame_count: usize) -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_millis() / SPINNER_FRAME_INTERVAL_MS) as usize)
        .unwrap_or(0)
        % frame_count
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
            "globe" => Ok(Self::Globe),
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
    animated: Vec<SpinnerFrame>,
    idle: SpinnerFrame,
}

fn frame_set_for(style: SpinnerStyle) -> FrameSet {
    match style {
        SpinnerStyle::Pulse => build_pulse(),
        SpinnerStyle::Wave => build_wave(),
        SpinnerStyle::Bars => build_bars(),
        SpinnerStyle::Shimmer => build_shimmer(),
        SpinnerStyle::Globe => build_globe(),
    }
}

/// All styles' frames, generated once and kept for the process lifetime. Built
/// by mapping over [`SpinnerStyle::ALL`], so `FRAME_SETS[style.index()]` is
/// always `style`'s frames — the array length and the exhaustive match in
/// `frame_set_for` force this to stay correct when a variant is added.
static FRAME_SETS: LazyLock<[FrameSet; 5]> = LazyLock::new(|| SpinnerStyle::ALL.map(frame_set_for));

/// Assemble one frame from its `(glyph, ink)` cells, checking the width
/// contract at the single point where every style's frames are born.
fn row(cells: Vec<(char, SpinnerInk)>) -> SpinnerFrame {
    let text: String = cells.iter().map(|(glyph, _)| *glyph).collect();
    debug_assert_eq!(
        unicode_width::UnicodeWidthStr::width(text.as_str()),
        SPINNER_WIDTH,
        "spinner frame {text:?} must be {SPINNER_WIDTH} columns wide"
    );
    let mut runs: Vec<(String, SpinnerInk)> = Vec::new();
    for (glyph, ink) in cells {
        match runs.last_mut() {
            Some((run, run_ink)) if *run_ink == ink => run.push(glyph),
            _ => runs.push((glyph.to_string(), ink)),
        }
    }
    SpinnerFrame { text, runs }
}

/// The rule every style rests on. Faint on purpose: the resting ornament
/// should recede into the border so an active turn's color reads as a change.
fn idle_row() -> SpinnerFrame {
    row(vec![(IDLE_GLYPH, SpinnerInk::Faint); SPINNER_WIDTH])
}

/// A bright dot glides left-to-right and wraps, with a symmetric brightness
/// falloff on either side so it reads as a soft pulse rather than a hard pip.
/// Color follows the same falloff, giving the dot a hot core and a cold tail.
fn build_pulse() -> FrameSet {
    let w = SPINNER_WIDTH;
    // Distance from the head picks the glyph and its ink together, so the
    // comet's core can never end up shaded like its tail.
    let cell = |dist: usize| match dist {
        0 => ('●', SpinnerInk::Vivid),
        1 => ('•', SpinnerInk::Bright),
        2 => ('∙', SpinnerInk::Cool),
        _ => ('·', SpinnerInk::Faint),
    };
    let animated = (0..w)
        .map(|head| {
            row((0..w)
                .map(|x| {
                    // ring distance from the head, so the pulse wraps seamlessly
                    let d = ((x + w - head) % w).min((head + w - x) % w);
                    cell(d)
                })
                .collect())
        })
        .collect();
    FrameSet {
        animated,
        idle: idle_row(),
    }
}

/// An undulating braille ribbon that scrolls one full wavelength per loop.
/// Height also drives color, so crests catch the light and troughs sink into
/// the rail — the strip reads as a lit surface rather than a flat scroll.
fn build_wave() -> FrameSet {
    let w = SPINNER_WIDTH;
    // Horizontal braille bars (both columns lit) at four vertical heights,
    // top to bottom: ⠉ ⠒ ⠤ ⣀.
    let levels = [
        (char::from_u32(0x2800 + 0x09).unwrap(), SpinnerInk::Bright),
        (char::from_u32(0x2800 + 0x12).unwrap(), SpinnerInk::Cool),
        (char::from_u32(0x2800 + 0x24).unwrap(), SpinnerInk::Calm),
        (char::from_u32(0x2800 + 0xC0).unwrap(), SpinnerInk::Faint),
    ];
    const N: usize = 8;
    let animated = (0..N)
        .map(|i| {
            row((0..w)
                .map(|x| {
                    let phase = std::f64::consts::TAU * (x as f64 / w as f64)
                        - std::f64::consts::TAU * (i as f64 / N as f64);
                    let v = phase.sin();
                    let lvl = (((1.0 - (v + 1.0) / 2.0) * 3.0).round() as i64).clamp(0, 3) as usize;
                    levels[lvl]
                })
                .collect())
        })
        .collect();
    FrameSet {
        animated,
        idle: idle_row(),
    }
}

/// Vertical eighth-block bars whose heights ripple like an equalizer, shaded
/// like one too: green through the low range, amber as it climbs, and red only
/// on the single tallest step, so peaks flash rather than glow.
fn build_bars() -> FrameSet {
    let w = SPINNER_WIDTH;
    let bars = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    // Indexed by the same 1..=8 height as `bars`, so a bar's color and its
    // height are read off one number.
    let ink = |height: usize| match height {
        ..=4 => SpinnerInk::Calm,
        5..=7 => SpinnerInk::Warm,
        _ => SpinnerInk::Hot,
    };
    const N: usize = 10;
    let animated = (0..N)
        .map(|i| {
            row((0..w)
                .map(|x| {
                    let phase = std::f64::consts::TAU * (x as f64 / 6.0)
                        + std::f64::consts::TAU * (i as f64 / N as f64);
                    let v = (phase.sin() + 1.0) / 2.0; // 0..1
                    let h = 1 + (v * 7.0).round() as usize; // 1..8 (never blank)
                    (bars[h], ink(h))
                })
                .collect())
        })
        .collect();
    FrameSet {
        animated,
        idle: idle_row(),
    }
}

/// The whole row brightens and dims together — a calm, confident "working…".
/// Size and color breathe on the same ramp, so the row swells and warms as one.
fn build_shimmer() -> FrameSet {
    let w = SPINNER_WIDTH;
    let ramp = [
        ('·', SpinnerInk::Faint),
        ('·', SpinnerInk::Faint),
        ('∙', SpinnerInk::Cool),
        ('•', SpinnerInk::Bright),
        ('●', SpinnerInk::Vivid),
        ('●', SpinnerInk::Vivid),
        ('•', SpinnerInk::Bright),
        ('∙', SpinnerInk::Cool),
    ];
    let animated = ramp.iter().map(|cell| row(vec![*cell; w])).collect();
    FrameSet {
        animated,
        idle: idle_row(),
    }
}

/// One rotation of a lit sphere, ordered so the terminator sweeps steadily
/// across the disc: fully dark, lit growing in from the right edge, fully lit,
/// then shrinking off the left edge. Reading them in order is what makes the
/// ball look like it is spinning rather than fading in and out.
const GLOBE_PHASES: [char; 8] = ['🌑', '🌒', '🌓', '🌔', '🌕', '🌖', '🌗', '🌘'];

/// A single sphere spins in place at the centre of the idle rule, so the strip
/// keeps its resting shape and only the ball moves. The sphere is inked
/// `Bright` for terminals that render the moons as monochrome text; those that
/// use emoji presentation supply their own color and ignore it.
fn build_globe() -> FrameSet {
    let w = SPINNER_WIDTH;
    let rail_cell = (IDLE_GLYPH, SpinnerInk::Faint);
    let animated = GLOBE_PHASES
        .iter()
        .map(|&phase| {
            // Measured rather than assumed: the moon glyphs are double-width,
            // and the rule has to absorb exactly whatever they occupy for the
            // frame to stay SPINNER_WIDTH columns.
            let rail = w.saturating_sub(unicode_width::UnicodeWidthChar::width(phase).unwrap_or(1));
            let mut cells = vec![rail_cell; rail / 2];
            cells.push((phase, SpinnerInk::Bright));
            cells.resize(rail + 1, rail_cell);
            row(cells)
        })
        .collect();
    FrameSet {
        animated,
        idle: idle_row(),
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
                    UnicodeWidthStr::width(frame.text()),
                    SPINNER_WIDTH,
                    "{style} frame {frame:?} wrong width"
                );
            }
            assert_eq!(
                UnicodeWidthStr::width(style.idle_frame().text()),
                SPINNER_WIDTH,
                "{style} idle wrong width"
            );
            for frame in style.compact_frames() {
                assert_eq!(
                    UnicodeWidthStr::width(*frame),
                    1,
                    "{style} compact frame {frame:?} must be one column"
                );
            }
        }
    }

    #[test]
    fn every_style_uses_a_solid_horizontal_line_when_idle() {
        let expected = IDLE_GLYPH.to_string().repeat(SPINNER_WIDTH);
        for style in SpinnerStyle::ALL {
            assert_eq!(style.idle_frame().text(), expected, "{style} idle frame");
            // A single muted run: the resting ornament must never look active.
            assert_eq!(
                style.idle_frame().runs(),
                [(expected.clone(), SpinnerInk::Faint)],
                "{style} idle ink"
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
    fn globe_spins_one_centred_sphere_through_every_phase() {
        let frames = SpinnerStyle::Globe.frames();
        assert_eq!(frames.len(), GLOBE_PHASES.len());
        for (frame, phase) in frames.iter().zip(GLOBE_PHASES) {
            // Exactly one sphere per frame, centred, riding the idle rule.
            let (left, rest) = frame
                .text()
                .split_once(phase)
                .expect("frame shows its phase");
            assert!(
                !rest.contains(phase),
                "{frame:?} shows more than one sphere"
            );
            assert!(
                left.chars().all(|c| c == IDLE_GLYPH) && rest.chars().all(|c| c == IDLE_GLYPH),
                "{frame:?} should be a sphere on the idle rule"
            );
            assert!(
                left.chars().count().abs_diff(rest.chars().count()) <= 1,
                "{frame:?} sphere is off-centre"
            );
        }
    }

    #[test]
    fn globe_shows_both_a_fully_dark_and_a_fully_lit_face() {
        // The point of the style: the sphere carries a dark side around with
        // it, so a full rotation must pass through both extremes.
        let frames = SpinnerStyle::Globe.frames();
        for face in ['🌑', '🌕'] {
            assert!(
                frames.iter().any(|frame| frame.text().contains(face)),
                "globe never shows {face}"
            );
        }
        assert!(
            SpinnerStyle::Globe.compact_frames().contains(&"○")
                && SpinnerStyle::Globe.compact_frames().contains(&"●"),
            "compact globe never reaches both extremes"
        );
    }

    #[test]
    fn every_frame_inks_every_cell() {
        // `runs()` merges same-ink neighbours, so the only way to check that no
        // cell was left uncolored is to confirm the runs reassemble the frame.
        for style in SpinnerStyle::ALL {
            for frame in style.frames().iter().chain([style.idle_frame()]) {
                let reassembled: String =
                    frame.runs().iter().map(|(text, _)| text.as_str()).collect();
                assert_eq!(reassembled, frame.text(), "{style} frame {frame:?}");
            }
        }
    }

    #[test]
    fn animated_frames_reach_past_the_resting_ink() {
        // Every style has to actually light up while a turn is in flight;
        // an all-Faint animation would be indistinguishable from idle.
        for style in SpinnerStyle::ALL {
            assert!(
                style
                    .frames()
                    .iter()
                    .flat_map(|frame| frame.runs())
                    .any(|(_, ink)| *ink != SpinnerInk::Faint),
                "{style} never brightens past its idle rail"
            );
        }
    }

    #[test]
    fn runs_merge_neighbours_that_share_an_ink() {
        // Shimmer inks the whole row identically, so it must collapse to one
        // span; pulse's falloff must not.
        let shimmer = &SpinnerStyle::Shimmer.frames()[0];
        assert_eq!(shimmer.runs().len(), 1, "{shimmer:?} should be one run");
        assert!(
            SpinnerStyle::Pulse.frames()[0].runs().len() > 1,
            "pulse's falloff should span several inks"
        );
    }

    #[test]
    fn bars_reserves_its_hottest_ink_for_the_tallest_bar() {
        // The meter reads as an audio meter only if red means "peak" — if a
        // mid-height bar ever went Hot the ramp would look like an error.
        let tallest = '█';
        for frame in SpinnerStyle::Bars.frames() {
            for (run, ink) in frame.runs() {
                if *ink == SpinnerInk::Hot {
                    assert!(
                        run.chars().all(|glyph| glyph == tallest),
                        "{frame:?} inked a short bar Hot"
                    );
                }
            }
        }
        assert!(
            SpinnerStyle::Bars
                .frames()
                .iter()
                .flat_map(|frame| frame.runs())
                .any(|(_, ink)| *ink == SpinnerInk::Hot),
            "bars never peaks"
        );
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
}
