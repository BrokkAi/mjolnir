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
//! Frames carry color as [`SpinnerInk`] slots rather than concrete colors, so
//! the palette can adapt gradients to the active terminal capabilities.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Display width (terminal columns) of every spinner frame, for every style.
pub const SPINNER_WIDTH: usize = 12;

/// Number of intensity levels in the scan-light gradient.
pub(crate) const SCAN_RED_LEVELS: usize = SPINNER_WIDTH;
const SCAN_RED_MAX: u8 = (SCAN_RED_LEVELS - 1) as u8;

/// Resting ornament shown when no turn is in flight.
const IDLE_GLYPH: char = '─';

/// Wall-clock dwell per animation frame. Kept deliberately calmer than
/// streaming redraws so progress reads as steady activity without making
/// queued prompt typing feel visually noisy.
pub const SPINNER_FRAME_INTERVAL_MS: u128 = 250;

/// Fastest frame interval any spinner uses. The UI redraw timer follows this
/// value while individual styles retain their own wall-clock cadence.
pub const SPINNER_REDRAW_INTERVAL_MS: u128 = SCAN_FRAME_INTERVAL_MS;

/// Dwell between adjacent scan-light positions. Across the full-width rail,
/// this produces an edge-to-edge sweep of roughly one second.
const SCAN_FRAME_INTERVAL_MS: u128 = 90;

/// Color slot for one spinner cell, resolved against the active palette by
/// [`crate::palette::TerminalTheme::spinner_ink`]. Styles emit slots rather
/// than colors so one frame set serves both the truecolor and the 16-color
/// ANSI themes.
///
/// The first four are a cold-to-hot energy ramp used by the motion styles.
/// `Calm`, `Warm`, and `Hot` form the metered ramp used by `Bars`; `Red` carries
/// one level of the scan-light gradient.
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
    /// Red gradient level, from the dim rail at zero to the bright head.
    Red(u8),
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
    /// A lit head sweeps to one wall and back, trailing a fading tail.
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

    fn index(self) -> usize {
        // Derived from ALL so it cannot drift from FRAME_SETS (also ALL-ordered).
        Self::ALL
            .iter()
            .position(|style| *style == self)
            .unwrap_or(0)
    }

    /// Animated frames for this style. Always non-empty; index with the
    /// wall-clock tick and this style's frame interval.
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
        &frames[current_frame_index(frames.len(), self.frame_interval_ms())]
    }

    /// Single-column animation frame for compact progress surfaces. This is a
    /// separate contract from the twelve-column prompt ornament: callers never
    /// need to infer a usable glyph from the internals of a wide frame.
    pub fn compact_frame(self) -> &'static str {
        let frames = self.compact_frames();
        frames[current_frame_index(frames.len(), SPINNER_FRAME_INTERVAL_MS)]
    }

    fn frame_interval_ms(self) -> u128 {
        match self {
            Self::Scan => SCAN_FRAME_INTERVAL_MS,
            _ => SPINNER_FRAME_INTERVAL_MS,
        }
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
            // One column can't sweep sideways, so the compact slot keeps the
            // style's identity — a light bouncing between two walls — by
            // ping-ponging a braille dot vertically instead.
            Self::Scan => &["⠁", "⠂", "⠄", "⡀", "⠄", "⠂"],
        }
    }
}

fn current_frame_index(frame_count: usize, frame_interval_ms: u128) -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_millis() / frame_interval_ms) as usize)
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
        SpinnerStyle::Scan => build_scan(),
    }
}

/// All styles' frames, generated once and kept for the process lifetime. Built
/// by mapping over [`SpinnerStyle::ALL`], so `FRAME_SETS[style.index()]` is
/// always `style`'s frames — the array length and the exhaustive match in
/// `frame_set_for` force this to stay correct when a variant is added.
static FRAME_SETS: LazyLock<[FrameSet; 6]> = LazyLock::new(|| SpinnerStyle::ALL.map(frame_set_for));

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
const GLOBE_CLEARANCE: usize = 2;

/// A single sphere spins at the centre of the idle rule with a small gap around
/// it. The sphere is inked `Bright` for terminals that render the moons as
/// monochrome text; those that use emoji presentation supply their own color
/// and ignore it.
fn build_globe() -> FrameSet {
    let w = SPINNER_WIDTH;
    let rail_cell = (IDLE_GLYPH, SpinnerInk::Faint);
    let empty_cell = (' ', SpinnerInk::Faint);
    let animated = GLOBE_PHASES
        .iter()
        .map(|&phase| {
            let available =
                w.saturating_sub(unicode_width::UnicodeWidthChar::width(phase).unwrap_or(1));
            let clearance = GLOBE_CLEARANCE.min(available / 2);
            let rail = available.saturating_sub(clearance * 2);
            let left_rail = rail / 2;
            let right_rail = rail - left_rail;

            let mut cells = vec![rail_cell; left_rail];
            cells.extend(std::iter::repeat_n(empty_cell, clearance));
            cells.push((phase, SpinnerInk::Bright));
            cells.extend(std::iter::repeat_n(empty_cell, clearance));
            cells.extend(std::iter::repeat_n(rail_cell, right_rail));
            row(cells)
        })
        .collect();
    FrameSet {
        animated,
        idle: idle_row(),
    }
}

/// Head positions for two complete one-way sweeps. Each movement advances one
/// adjacent cell, and both journeys include their destination wall.
fn scan_heads() -> Vec<i64> {
    let w = SPINNER_WIDTH as i64;
    let mut heads: Vec<i64> = (0..w).collect();
    heads.extend((0..w - 1).rev());
    heads
}

fn scan_cell(distance: i64) -> (char, SpinnerInk) {
    if !(0..SPINNER_WIDTH as i64).contains(&distance) {
        return ('·', SpinnerInk::Red(0));
    }

    let level = SCAN_RED_MAX - distance as u8;
    let glyph = match distance {
        0 => '●',
        1..=4 => '•',
        5..=8 => '∙',
        _ => '·',
    };
    (glyph, SpinnerInk::Red(level))
}

/// A lit head sweeps to one wall, reverses, and sweeps back, dragging a short
/// fading tail. The tail always trails the direction of travel — it swaps
/// sides on the frame after each bounce — which is what makes the light read
/// as bouncing between the walls rather than wrapping around like `Pulse`.
/// Every active cell keeps a low red glow so motion comes from the brighter
/// peak and afterglow rather than cells switching fully off.
fn build_scan() -> FrameSet {
    let heads = scan_heads();
    let count = heads.len();
    // One full bounce supplies two uninterrupted one-way sweeps. The dim rail
    // then holds for one one-way sweep before the next bounce begins.
    let animated = (0..count + SPINNER_WIDTH)
        .map(|i| {
            if i >= count {
                return row(vec![('·', SpinnerInk::Red(0)); SPINNER_WIDTH]);
            }
            let head = heads[i];
            let prev = heads[(i + count - 1) % count];
            let dir = if head >= prev { 1 } else { -1 };
            row((0..SPINNER_WIDTH as i64)
                .map(|x| {
                    // Signed distance behind the head; cells ahead go negative
                    // and fall through to the low, always-on glow.
                    scan_cell((head - x) * dir)
                })
                .collect())
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
    fn animation_loops_stay_within_their_intended_cadence() {
        for style in SpinnerStyle::ALL {
            let loop_ms = style.frames().len() as u128 * style.frame_interval_ms();
            assert!(
                (1_000..=4_250).contains(&loop_ms),
                "{style} loop_ms = {loop_ms}"
            );
        }
    }

    #[test]
    fn scan_crosses_the_rail_in_about_one_second() {
        let crossing_ms = (SPINNER_WIDTH as u128 - 1) * SpinnerStyle::Scan.frame_interval_ms();
        assert!(
            (900..=1_100).contains(&crossing_ms),
            "crossing_ms = {crossing_ms}"
        );
        assert_eq!(SPINNER_REDRAW_INTERVAL_MS, SCAN_FRAME_INTERVAL_MS);
    }

    #[test]
    fn globe_spins_one_centred_sphere_through_every_phase() {
        let frames = SpinnerStyle::Globe.frames();
        assert_eq!(frames.len(), GLOBE_PHASES.len());
        for (frame, phase) in frames.iter().zip(GLOBE_PHASES) {
            // Exactly one sphere per frame, centred with a small clear gap.
            let (left, rest) = frame
                .text()
                .split_once(phase)
                .expect("frame shows its phase");
            assert!(
                !rest.contains(phase),
                "{frame:?} shows more than one sphere"
            );
            let left: Vec<char> = left.chars().collect();
            let right: Vec<char> = rest.chars().collect();
            assert!(
                left.ends_with(&[' '; GLOBE_CLEARANCE])
                    && right.starts_with(&[' '; GLOBE_CLEARANCE]),
                "{frame:?} should leave two spaces around the sphere"
            );
            assert!(
                left[..left.len() - GLOBE_CLEARANCE]
                    .iter()
                    .chain(&right[GLOBE_CLEARANCE..])
                    .all(|c| *c == IDLE_GLYPH),
                "{frame:?} should retain the idle rule outside the gap"
            );
            assert!(
                left.len().abs_diff(right.len()) <= 1,
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
    fn scan_bounces_off_both_walls_without_wrapping() {
        let heads: Vec<usize> = scan_heads().into_iter().map(|head| head as usize).collect();
        // The light must actually reach both walls before turning around…
        assert!(heads.contains(&0), "scan never reaches the left wall");
        assert!(
            heads.contains(&(SPINNER_WIDTH - 1)),
            "scan never reaches the right wall"
        );
        // …and travel there smoothly: a wrap like Pulse's would show up as a
        // near-full-width jump between consecutive frames.
        assert_eq!(heads.first(), Some(&0));
        assert_eq!(heads.last(), Some(&0));
        for pair in heads.windows(2) {
            let [head, next] = pair else {
                unreachable!("windows of two always contain two positions")
            };
            assert!(
                head.abs_diff(*next) == 1,
                "scan head jumps from {head} to {next}"
            );
        }
    }

    #[test]
    fn scan_sweeps_twice_then_holds_the_dim_rail_for_one_pass() {
        let frames = SpinnerStyle::Scan.frames();
        let sweep_ticks = scan_heads().len();
        assert_eq!(frames.len(), sweep_ticks + SPINNER_WIDTH);
        assert!(
            frames[..sweep_ticks]
                .iter()
                .all(|frame| frame.text().contains('●')),
            "both sweeps should stay continuously lit"
        );
        assert!(
            frames[sweep_ticks..].len() == SPINNER_WIDTH
                && frames[sweep_ticks..].iter().all(|frame| frame
                    .text()
                    .chars()
                    .all(|glyph| glyph == '·')
                    && frame
                        .runs()
                        .iter()
                        .all(|(_, ink)| *ink == SpinnerInk::Red(0))),
            "the quiet pass should hold only the dim red rail"
        );
    }

    #[test]
    fn scan_trail_drops_one_red_level_per_cell() {
        let trail: Vec<(char, SpinnerInk)> = (0..SPINNER_WIDTH as i64).map(scan_cell).collect();
        let levels: Vec<u8> = trail
            .iter()
            .map(|(_, ink)| match ink {
                SpinnerInk::Red(level) => *level,
                _ => panic!("scan cell is not red"),
            })
            .collect();

        assert_eq!(levels, (0..=SCAN_RED_MAX).rev().collect::<Vec<_>>());
        assert!(trail[1..=4].iter().all(|(glyph, _)| *glyph == '•'));
        assert_eq!(levels.iter().filter(|level| **level > 0).count(), 11);
    }

    #[test]
    fn scan_brightness_peak_trails_the_head_over_an_always_lit_rail() {
        let mut longest_tail = 0;
        for frame in SpinnerStyle::Scan
            .frames()
            .iter()
            .filter(|frame| frame.text().contains('●'))
        {
            let cells: Vec<char> = frame.text().chars().collect();
            let head = cells.iter().position(|c| *c == '●').expect("head");
            let tail: Vec<usize> = cells
                .iter()
                .enumerate()
                .filter(|(_, c)| ['•', '∙'].contains(c))
                .map(|(x, _)| x)
                .collect();
            longest_tail = longest_tail.max(tail.len());
            // The comet is contiguous and entirely on one side of the head,
            // so the tail reads as dragged behind rather than haloing it.
            assert!(
                tail.iter().all(|x| x.abs_diff(head) <= 8)
                    && (tail.iter().all(|x| *x < head) || tail.iter().all(|x| *x > head)),
                "{frame:?} tail {tail:?} should trail one side of head {head}"
            );
            assert!(
                cells.iter().all(|c| ['●', '•', '∙', '·'].contains(c)),
                "{frame:?} should keep every active cell visibly lit"
            );
        }
        assert_eq!(longest_tail, 8, "scan should show eight shaped tail cells");
    }

    #[test]
    fn scan_uses_red_ink_for_every_lit_cell() {
        let mut seen_inks = Vec::new();
        for frame in SpinnerStyle::Scan.frames() {
            for (run, ink) in frame.runs() {
                assert!(
                    matches!(ink, SpinnerInk::Red(0..=SCAN_RED_MAX)),
                    "scan run {run:?} in {frame:?} is not red"
                );
                seen_inks.push(*ink);
            }
        }
        assert!(
            seen_inks.contains(&SpinnerInk::Red(0)),
            "scan has no dim glow"
        );
        assert!(
            seen_inks.contains(&SpinnerInk::Red(SCAN_RED_MAX)),
            "scan has no bright peak"
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
