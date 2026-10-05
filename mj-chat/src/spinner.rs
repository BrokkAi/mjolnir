//! Prompt-activity spinner styles.
//!
//! A spinner style is a purely client-side visual preference: it is persisted
//! as `spinner` in `config.toml` and changeable from the command palette.
//!
//! Every style renders to frames of exactly [`SPINNER_WIDTH`] display columns
//! (including its idle frame) so the activity row never reflows when a turn
//! starts, ends, or the style changes. Frames are generated once on first use.
//!
//! Frames carry color as [`SpinnerInk`] slots rather than concrete colors, so
//! the palette can adapt gradients to the active terminal capabilities.

use std::sync::LazyLock;

pub use mj_core::config::SpinnerStyle;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::theme;

/// Display width (terminal columns) of every spinner frame, for every style.
pub const SPINNER_WIDTH: usize = 12;

/// Number of intensity levels in the scan-light gradient.
pub const SCAN_RED_LEVELS: usize = SPINNER_WIDTH;
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

/// Color slot for one spinner cell, resolved against the shared terminal
/// palette by the rendering helpers.
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
    /// surface can carry (plain-text previews and width assertions).
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

trait SpinnerFrames {
    fn index(self) -> usize;
    fn frames(self) -> &'static [SpinnerFrame];
    fn idle_frame(self) -> &'static SpinnerFrame;
    fn frame_interval_ms(self) -> u128;
    fn compact_frames(self) -> &'static [&'static str];
}

impl SpinnerFrames for SpinnerStyle {
    fn index(self) -> usize {
        // Derived from ALL so it cannot drift from FRAME_SETS (also ALL-ordered).
        Self::ALL
            .iter()
            .position(|style| *style == self)
            .unwrap_or(0)
    }

    /// Animated frames for this style. Always non-empty; index with the
    /// wall-clock tick and this style's frame interval.
    fn frames(self) -> &'static [SpinnerFrame] {
        &FRAME_SETS[self.index()].animated
    }

    /// Resting frame shown when no turn is in flight.
    fn idle_frame(self) -> &'static SpinnerFrame {
        &FRAME_SETS[self.index()].idle
    }

    /// Wall-clock dwell for this style's full-width animation frames.
    ///
    /// Remote viewers receive the same frames as the TUI and use this value to
    /// keep the animation cadence in sync as well.
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

/// Milliseconds on a monotonic animation clock, independent of input redraws.
pub fn elapsed_ms() -> u128 {
    static STARTED: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
    STARTED.elapsed().as_millis()
}

fn frame_index(elapsed_ms: u128, count: usize, interval_ms: u128) -> usize {
    ((elapsed_ms / interval_ms) % count as u128) as usize
}

/// The frames of every style when only ASCII can be drawn.
const ASCII_COMPACT_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// Colored twelve-column activity ornament. Idle always rests on a quiet rule.
pub fn activity_line(style: SpinnerStyle, elapsed_ms: u128, active: bool) -> Line<'static> {
    if theme::ascii() {
        // A marker sweeping a dashed rail: the same width as every style.
        let rail = "-".repeat(SPINNER_WIDTH);
        if !active {
            return Line::styled(rail, Style::default().fg(ink_color(SpinnerInk::Faint)));
        }
        let at = frame_index(elapsed_ms, SPINNER_WIDTH, SPINNER_FRAME_INTERVAL_MS);
        return Line::from(vec![
            Span::styled(
                "-".repeat(at),
                Style::default().fg(ink_color(SpinnerInk::Faint)),
            ),
            Span::styled("#", Style::default().fg(ink_color(SpinnerInk::Bright))),
            Span::styled(
                "-".repeat(SPINNER_WIDTH - at - 1),
                Style::default().fg(ink_color(SpinnerInk::Faint)),
            ),
        ]);
    }
    let frame = if active {
        let frames = style.frames();
        &frames[frame_index(elapsed_ms, frames.len(), style.frame_interval_ms())]
    } else {
        style.idle_frame()
    };
    Line::from(
        frame
            .runs()
            .iter()
            .map(|(text, ink)| Span::styled(text.as_str(), Style::default().fg(ink_color(*ink))))
            .collect::<Vec<_>>(),
    )
}

/// Single-column activity frame for session lists and loading states.
pub fn compact_frame(style: SpinnerStyle, elapsed_ms: u128) -> &'static str {
    let frames = if theme::ascii() {
        &ASCII_COMPACT_FRAMES[..]
    } else {
        style.compact_frames()
    };
    frames[frame_index(elapsed_ms, frames.len(), SPINNER_FRAME_INTERVAL_MS)]
}

/// Compact loading indicator with the same palette as the full ornament.
pub fn compact_span(style: SpinnerStyle, elapsed_ms: u128) -> Span<'static> {
    Span::styled(
        compact_frame(style, elapsed_ms),
        Style::default().fg(theme::palette().accent),
    )
}

#[allow(
    clippy::disallowed_methods,
    reason = "The scan gradient is drawn over the shared theme's painted surfaces, not an unknown terminal background."
)]
fn ink_color(ink: SpinnerInk) -> Color {
    match ink {
        SpinnerInk::Faint => theme::palette().border,
        SpinnerInk::Cool => theme::palette().secondary,
        SpinnerInk::Bright => theme::palette().accent,
        SpinnerInk::Vivid => theme::palette().text,
        SpinnerInk::Calm => theme::palette().success,
        SpinnerInk::Warm => theme::palette().warning,
        SpinnerInk::Hot => theme::palette().error,
        SpinnerInk::Red(level) => {
            let level = u16::from(level.min(SCAN_RED_MAX));
            let max = u16::from(SCAN_RED_MAX);
            let Color::Rgb(start_r, start_g, start_b) = theme::palette().activity_dim else {
                return theme::palette().error;
            };
            let Color::Rgb(end_r, end_g, end_b) = theme::palette().error else {
                return theme::palette().error;
            };
            let blend = |start: u8, end: u8| {
                ((u16::from(start) * (max - level) + u16::from(end) * level) / max) as u8
            };
            Color::Rgb(
                blend(start_r, end_r),
                blend(start_g, end_g),
                blend(start_b, end_b),
            )
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
    use crate::theme::{SymbolSet, UiTheme};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;
    use std::fmt::Write as _;
    use unicode_width::UnicodeWidthStr;

    fn append_activity(
        output: &mut String,
        label: &str,
        style: SpinnerStyle,
        elapsed: u128,
        active: bool,
        model: &str,
    ) {
        let line = activity_line(style, elapsed, active);
        let spans = line
            .spans
            .iter()
            .map(|span| format!("{:?} fg={:?}", span.content.as_ref(), span.style.fg))
            .collect::<Vec<_>>()
            .join(" | ");
        let mut terminal =
            Terminal::new(TestBackend::new(SPINNER_WIDTH as u16, 1)).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(line.clone()), frame.area()))
            .expect("render activity line");
        writeln!(output, "=== {label} ({}x1) ===", SPINNER_WIDTH).expect("write heading");
        writeln!(output, "{model}\nspans: {spans}").expect("write frame state");
        output.push_str(&crate::golden::buffer_lines(terminal.backend().buffer()).join("\n"));
        output.push('\n');
    }

    fn append_compact_span(output: &mut String, style: SpinnerStyle) {
        let span = compact_span(style, 0);
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(Line::from(span.clone())), frame.area())
            })
            .expect("render compact activity span");
        writeln!(
            output,
            "=== {style:?} compact styled span at zero (1x1) ===\nwidth: {}; style: {:?}\n{}\n",
            span.width(),
            span.style,
            crate::golden::buffer_lines(terminal.backend().buffer()).join("\n")
        )
        .expect("write compact activity span");
    }

    #[test]
    fn golden_activity_spinner() {
        let mut output = String::new();
        crate::theme::with_theme(UiTheme::Midnight, || {
            crate::theme::with_symbols(SymbolSet::Unicode, || {
                for style in SpinnerStyle::ALL {
                    let frames = style.frames();
                    let interval = style.frame_interval_ms();
                    let loop_ms = frames.len() as u128 * interval;
                    writeln!(
                        output,
                        "=== {:?} timing and compact frames ({}x1) ===\nframe interval: {interval}ms; loop: {loop_ms}ms; compact: {:?}",
                        style,
                        SPINNER_WIDTH,
                        style.compact_frames()
                    )
                    .expect("write timing");
                    append_compact_span(&mut output, style);
                    for (index, frame) in frames.iter().enumerate() {
                        let elapsed = index as u128 * interval;
                        append_activity(
                            &mut output,
                            &format!("{:?} animated frame {index} at {elapsed}ms", style),
                            style,
                            elapsed,
                            true,
                            &format!("frame: {:?}; runs: {:?}", frame.text(), frame.runs()),
                        );
                    }
                    append_activity(
                        &mut output,
                        &format!("{:?} first frame before cadence", style),
                        style,
                        interval.saturating_sub(1),
                        true,
                        "active at interval minus one",
                    );
                    append_activity(
                        &mut output,
                        &format!("{:?} frame after three intervals", style),
                        style,
                        interval * 3,
                        true,
                        "active after three intervals",
                    );
                    append_activity(
                        &mut output,
                        &format!("{:?} idle at zero", style),
                        style,
                        0,
                        false,
                        "idle frame",
                    );
                    append_activity(
                        &mut output,
                        &format!("{:?} idle at 100000ms", style),
                        style,
                        100_000,
                        false,
                        "idle frame at a later clock",
                    );
                    for (index, frame) in style.compact_frames().iter().enumerate() {
                        let elapsed = index as u128 * SPINNER_FRAME_INTERVAL_MS;
                        writeln!(
                            output,
                            "=== {:?} compact frame {index} at {elapsed}ms (1x1) ===\nsource: {frame}; rendered: {}",
                            style,
                            compact_frame(style, elapsed)
                        )
                        .expect("write compact frame");
                    }
                    if style == SpinnerStyle::Scan {
                        let heads = scan_heads();
                        let trail = (0..SPINNER_WIDTH as i64).map(scan_cell).collect::<Vec<_>>();
                        writeln!(
                            output,
                            "scan crossing: {}ms; redraw interval: {}ms; heads: {heads:?}; first trail: {trail:?}",
                            (SPINNER_WIDTH as u128 - 1) * style.frame_interval_ms(),
                            SPINNER_REDRAW_INTERVAL_MS
                        )
                        .expect("write scan timing");
                    }
                }
            });
        });
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "activity-spinner", &output);
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
}
