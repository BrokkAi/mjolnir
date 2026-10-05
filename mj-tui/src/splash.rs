//! The startup splash: Mjölnir falls out of a storm, strikes the ground and
//! calls down lightning while the dashboard loads behind it.
//!
//! A frame is a pure function of [`SplashFrame`], so any instant can be
//! rendered again or tested. [`SplashTimeline`] alone decides the phases; the
//! driver asks it when the splash is over instead of keeping its own clock.
//!
//! The scene is painted into a floating-point canvas at two pixels per cell
//! (upper half block: foreground on top, background below), tone-mapped,
//! and written to the buffer once.

mod canvas;
mod font;
mod fx;
mod hammer;
mod lightning;

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use canvas::{Canvas, Dissolve, Rgb};

/// Scene times, in seconds. The splash opens with the hammer already
/// falling above the screen; it comes into view about 0.16s later.
const DESCENT_START: f32 = -0.16;
const IMPACT: f32 = 0.44;
const TITLE_START: f32 = 0.61;
/// The finished title stays up for a moment before the splash may end.
const HOLD_START: Duration = Duration::from_millis(1340);
const DISSOLVE: Duration = Duration::from_millis(250);

/// The horizon, in scene units below the centre of the screen.
const GROUND_Y: f32 = 0.40;

/// When the splash may end. Loading readiness is the only input; every
/// phase decision is derived here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SplashTimeline {
    ready_at: Option<Duration>,
}

impl SplashTimeline {
    /// Loading finished at `at`. Only the first report counts.
    pub fn mark_ready(&mut self, at: Duration) {
        self.ready_at.get_or_insert(at);
    }

    pub fn is_ready(&self) -> bool {
        self.ready_at.is_some()
    }

    /// When the dissolve into the dashboard begins, once loading is ready.
    pub fn dissolve_start(&self) -> Option<Duration> {
        Some(self.ready_at?.max(HOLD_START))
    }

    /// Whether the last frame has been shown and the dashboard can draw.
    pub fn finished(&self, elapsed: Duration) -> bool {
        self.dissolve_start()
            .is_some_and(|start| elapsed >= start + DISSOLVE)
    }

    /// Whether the animation has played out and is waiting for loading.
    pub fn holding(&self, elapsed: Duration) -> bool {
        self.scene_time(elapsed) >= HOLD_START.as_secs_f32()
            && self.dissolve_progress(elapsed) == 0.0
    }

    fn scene_time(&self, elapsed: Duration) -> f32 {
        elapsed.as_secs_f32()
    }

    fn dissolve_progress(&self, elapsed: Duration) -> f32 {
        self.dissolve_start().map_or(0.0, |start| {
            (elapsed.saturating_sub(start).as_secs_f32() / DISSOLVE.as_secs_f32()).min(1.0)
        })
    }
}

/// Whether a terminal of this width shows the splash. The splash asks no
/// more of the terminal than the dashboard behind it does.
pub fn fits(width: u16) -> bool {
    width >= crate::render::NARROW_TERMINAL_WIDTH
}

/// Everything one splash frame depends on.
#[derive(Clone, Copy, Debug)]
pub struct SplashFrame<'a> {
    pub elapsed: Duration,
    pub timeline: SplashTimeline,
    /// The latest startup notice, shown while the splash waits for loading.
    pub status: Option<&'a str>,
    /// The dashboard's background, which the dissolve reveals.
    pub background: Color,
}

pub fn render_splash(area: Rect, buf: &mut Buffer, frame: &SplashFrame) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let dissolve = frame.timeline.dissolve_progress(frame.elapsed);
    if dissolve >= 1.0 || !fits(area.width) {
        fill(area, buf, frame.background);
        return;
    }
    let t = frame.timeline.scene_time(frame.elapsed);
    let holding = frame.timeline.holding(frame.elapsed);

    let mut canvas = Canvas::new(usize::from(area.width), usize::from(area.height) * 2);
    let view = View::new(canvas.width, canvas.height, t);
    let strike = lightning::Strike::at(t, holding);
    // Only a strong strike lights the whole sky, and then all at once: a
    // continuous fade would repaint every cloud cell on every frame.
    let flash = strike.as_ref().map_or(
        0.0,
        |strike| if strike.brightness > 1.0 { 1.0 } else { 0.0 },
    );
    let pose = hammer::Pose::at(t);

    fx::sky(&mut canvas, &view, t, flash);
    fx::ground(&mut canvas, &view, t, flash);
    fx::cracks(&mut canvas, &view, t);
    fx::shockwave(&mut canvas, &view, t);
    let charge = if t < IMPACT {
        0.12
    } else {
        0.35 + strike.as_ref().map_or(0.0, |strike| strike.brightness)
    };
    hammer::draw(&mut canvas, &view, t, charge);
    if let (Some(strike), Some(pose)) = (&strike, pose) {
        lightning::draw(&mut canvas, &view, strike, pose);
    }
    fx::sparks(&mut canvas, &view, t);
    if let Some(title) = font::Title::layout(&view) {
        font::draw(&mut canvas, title, t, holding);
    }
    fx::impact_flash(&mut canvas, t);

    let dissolve = (dissolve > 0.0).then_some(Dissolve {
        progress: dissolve,
        background: frame.background,
    });
    canvas.blit(area, buf, dissolve.as_ref());
    if dissolve.is_none() && holding {
        draw_status(area, buf, frame.status, t);
    }
}

fn fill(area: Rect, buf: &mut Buffer, background: Color) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.reset();
            cell.set_bg(background);
        }
    }
}

/// The startup notice, or a quiet default, centred on the bottom row.
fn draw_status(area: Rect, buf: &mut Buffer, status: Option<&str>, t: f32) {
    let dots = ((t * 3.0) as usize) % 4;
    let text = format!(
        "{}{:<3}",
        status.unwrap_or("Starting Mjolnir"),
        ".".repeat(dots)
    );
    let width = text.chars().count().min(usize::from(area.width));
    let x = area.x + (area.width - width as u16) / 2;
    let y = area.bottom() - 1;
    for (offset, character) in text.chars().take(width).enumerate() {
        let cell = &mut buf[(x + offset as u16, y)];
        let background = cell.bg;
        cell.reset();
        cell.set_char(character);
        cell.set_fg(canvas::color([140, 152, 184]));
        cell.set_bg(background);
    }
}

/// Maps canvas pixels to scene units: the origin is the screen centre, `y`
/// grows downward, and one unit is about half the screen height.
struct View {
    width: usize,
    height: usize,
    scale: f32,
    shake: (f32, f32),
}

impl View {
    fn new(width: usize, height: usize, t: f32) -> Self {
        let scale = (width as f32 / 2.6).min(height as f32 / 2.1);
        let since = t - IMPACT;
        let amplitude = 0.035 * (-since.max(0.0) * 9.0).exp();
        // Shaking by less than half a pixel only shimmers every edge.
        let shake = if since >= 0.0 && amplitude * scale > 0.5 {
            (
                amplitude * (since * 95.0).sin(),
                amplitude * 0.7 * (since * 71.0).cos(),
            )
        } else {
            (0.0, 0.0)
        };
        Self {
            width,
            height,
            scale,
            shake,
        }
    }

    /// The scene point under a pixel's centre.
    fn scene(&self, x: usize, y: usize) -> (f32, f32) {
        (
            (x as f32 + 0.5 - self.width as f32 / 2.0) / self.scale - self.shake.0,
            (y as f32 + 0.5 - self.height as f32 / 2.0) / self.scale - self.shake.1,
        )
    }

    /// A scene point in continuous pixel coordinates.
    fn pixel(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (
            (x + self.shake.0) * self.scale + self.width as f32 / 2.0,
            (y + self.shake.1) * self.scale + self.height as f32 / 2.0,
        )
    }

    /// Half the visible scene, in scene units.
    fn extent(&self) -> (f32, f32) {
        (
            self.width as f32 / 2.0 / self.scale,
            self.height as f32 / 2.0 / self.scale,
        )
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scaled(color: Rgb, factor: f32) -> Rgb {
    [color[0] * factor, color[1] * factor, color[2] * factor]
}

fn rotate((x, y): (f32, f32), angle: f32) -> (f32, f32) {
    let (sin, cos) = angle.sin_cos();
    (x * cos - y * sin, x * sin + y * cos)
}

/// SplitMix64: small, seedable, and identical on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.unit()
    }
}

/// A stable pseudo-random value in `[0, 1)` for a lattice point.
fn hash2(x: i32, y: i32) -> f32 {
    Rng::new(((x as u32 as u64) << 32) | y as u32 as u64).unit()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn rendered(width: u16, height: u16, frame: &SplashFrame) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render_splash(area, &mut buf, frame);
        buf
    }

    fn frame(elapsed: Duration, timeline: SplashTimeline) -> SplashFrame<'static> {
        SplashFrame {
            elapsed,
            timeline,
            status: None,
            background: Color::Reset,
        }
    }

    #[test]
    fn the_splash_waits_for_loading_before_dissolving() {
        let mut timeline = SplashTimeline::default();
        assert_eq!(timeline.dissolve_start(), None);
        assert!(!timeline.finished(MS(10_000)));
        assert!(timeline.holding(MS(5_000)));

        timeline.mark_ready(MS(4_000));
        assert_eq!(timeline.dissolve_start(), Some(MS(4_000)));
        assert!(!timeline.finished(MS(4_100)));
        assert!(timeline.finished(MS(4_250)));
    }

    #[test]
    fn fast_loading_still_plays_the_whole_animation() {
        let mut timeline = SplashTimeline::default();
        timeline.mark_ready(MS(300));
        assert_eq!(timeline.dissolve_start(), Some(HOLD_START));
        assert!(!timeline.finished(HOLD_START + DISSOLVE - MS(1)));
        assert!(timeline.finished(HOLD_START + DISSOLVE));
    }

    #[test]
    fn every_size_and_instant_renders_within_its_area() {
        let mut ready = SplashTimeline::default();
        ready.mark_ready(MS(1_200));
        for (width, height) in [
            (0, 0),
            (1, 1),
            (59, 30),
            (60, 1),
            (60, 10),
            (80, 24),
            (300, 100),
        ] {
            for timeline in [SplashTimeline::default(), ready] {
                for elapsed in (0..4_000).step_by(37) {
                    // A larger buffer catches writes outside the splash area.
                    let outer = Rect::new(0, 0, width + 4, height + 4);
                    let area = Rect::new(2, 2, width, height);
                    let mut buf = Buffer::empty(outer);
                    render_splash(
                        area,
                        &mut buf,
                        &SplashFrame {
                            status: Some("Waiting for the daemon"),
                            ..frame(MS(elapsed), timeline)
                        },
                    );
                    for y in outer.top()..outer.bottom() {
                        for x in outer.left()..outer.right() {
                            if !area.contains((x, y).into()) {
                                assert_eq!(buf[(x, y)], ratatui::buffer::Cell::EMPTY);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn golden_splash_frames() {
        use std::fmt::Write as _;

        let mut output = String::new();

        let mut timeline = SplashTimeline::default();
        timeline.mark_ready(MS(100));
        let background = canvas::color([15, 18, 20]);
        let dissolved = rendered(
            100,
            30,
            &SplashFrame {
                background,
                ..frame(HOLD_START + DISSOLVE, timeline)
            },
        );
        writeln!(
            output,
            "=== dashboard background after dissolve (100x30) ==="
        )
        .expect("write state header");
        output.push_str(&crate::test_support::buffer_lines(&dissolved).join("\n"));
        output.push('\n');
        assert!(
            dissolved
                .content()
                .iter()
                .all(|cell| cell.symbol() == " " && cell.bg == background)
        );
        writeln!(
            output,
            "dissolved cells: {}; background: {:?}",
            dissolved.content().len(),
            dissolved.content()[0].bg
        )
        .expect("write dissolved surface");

        let held = rendered(120, 40, &frame(MS(1_800), SplashTimeline::default()));
        writeln!(output, "\n=== held hammer and title (120x40) ===").expect("write state header");
        output.push_str(&crate::test_support::buffer_lines(&held).join("\n"));
        output.push('\n');
        let brightness = |x: u16, y: u16| {
            let cell = &held[(x, y)];
            [cell.fg, cell.bg]
                .into_iter()
                .map(|color| match color {
                    Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
        };
        let head_row = 20 + (GROUND_Y * 40.0 / 2.1 / 2.0) as u16 - 2;
        let lit =
            |range: std::ops::Range<u16>| range.filter(|x| brightness(*x, head_row) > 150).count();
        let left = lit(30..60);
        let right = lit(61..91);
        let title_rows = (head_row + 3..40)
            .filter(|y| (0..120).filter(|x| brightness(*x, *y) > 300).count() > 20)
            .count();
        let title_colors = (head_row + 3..40)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .flat_map(|(x, y)| [held[(x, y)].fg, held[(x, y)].bg])
            .filter(|color| match color {
                Color::Rgb(r, g, b) => u32::from(*r) + u32::from(*g) + u32::from(*b) > 300,
                _ => false,
            })
            .map(|color| format!("{color:?}"))
            .collect::<std::collections::BTreeSet<_>>();
        writeln!(
            output,
            "hammer head row: {head_row}; center brightness: {}; left/right lit cells: {left}/{right}; title rows: {title_rows}; bright title colors: {title_colors:?}",
            brightness(60, head_row)
        )
        .expect("write rendered art observations");
        writeln!(
            output,
            "status row: {}",
            crate::test_support::buffer_lines(&held)[39]
        )
        .expect("write status line");

        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "splash-frames", &output);
    }
}
