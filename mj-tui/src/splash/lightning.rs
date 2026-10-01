//! Lightning answering the hammer: branched bolts from the clouds into its
//! crown, and short arcs crawling over the head. Each bolt holds its shape
//! for one flicker interval and is redrawn from a new seed in the next, so
//! the sky crackles.

use super::canvas::{Canvas, Field, Rgb};
use super::hammer::{CROWN, HEAD, Pose};
use super::{IMPACT, Rng, View, mix, smoothstep};

const CORE: Rgb = [0.95, 0.97, 1.0];
const GLOW: Rgb = [0.45, 0.52, 1.0];
const THUNDER_FLICKER: f32 = 0.075;
const HELD_FLICKER: f32 = 0.11;

/// One flicker interval in which lightning may strike.
pub(super) struct Strike {
    seed: u64,
    /// How bright the strike is now; it also lights the clouds.
    pub(super) brightness: f32,
}

impl Strike {
    /// The strike at scene time `t`, if one is lit. The first one lands with
    /// the hammer; later ones come less often while the splash waits.
    pub(super) fn at(t: f32, holding: bool) -> Option<Self> {
        let since = t - IMPACT;
        if since < 0.0 {
            return None;
        }
        let (length, chance, salt) = if holding {
            (HELD_FLICKER, 0.4, 0x4E1D)
        } else {
            (THUNDER_FLICKER, 0.8, 0x7E0D)
        };
        let interval = (since / length).floor();
        let within = since / length - interval;
        let seed = salt ^ (interval as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
        let mut rng = Rng::new(seed);
        let first = !holding && interval == 0.0;
        if !first && rng.unit() > chance {
            return None;
        }
        let level = if first { 1.4 } else { rng.range(0.6, 1.1) };
        // A strike flares, fades, and sometimes flares again as it re-forms.
        let restrike = if rng.unit() > 0.5 {
            0.6 * (1.0 - ((within - 0.55) / 0.1).abs()).max(0.0)
        } else {
            0.0
        };
        Some(Self {
            seed,
            brightness: level * ((1.0 - within).powf(0.7) + restrike),
        })
    }
}

/// Midpoint displacement: split every segment, nudge the midpoint sideways,
/// halve the nudge, repeat.
fn bolt(
    rng: &mut Rng,
    from: (f32, f32),
    to: (f32, f32),
    depth: u32,
    roughness: f32,
) -> Vec<(f32, f32)> {
    let mut points = vec![from, to];
    let mut amplitude = roughness * (to.0 - from.0).hypot(to.1 - from.1);
    for _ in 0..depth {
        let mut next = Vec::with_capacity(points.len() * 2);
        for pair in points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let length = dx.hypot(dy).max(1e-6);
            let offset = rng.range(-1.0, 1.0) * amplitude;
            next.push(a);
            next.push((
                (a.0 + b.0) / 2.0 - dy / length * offset,
                (a.1 + b.1) / 2.0 + dx / length * offset,
            ));
        }
        next.push(to);
        points = next;
        amplitude *= 0.5;
    }
    points
}

fn stroke(field: &mut Field, view: &View, points: &[(f32, f32)], core: f32, intensity: f32) {
    let glow = (view.scale * 0.05).max(1.6);
    for pair in points.windows(2) {
        field.segment(
            view.pixel(pair[0]),
            view.pixel(pair[1]),
            core,
            glow,
            intensity,
        );
    }
}

pub(super) fn draw(canvas: &mut Canvas, view: &View, strike: &Strike, pose: Pose) {
    let mut rng = Rng::new(strike.seed ^ 0xB017);
    let mut field = Field::new(canvas.width, canvas.height);
    let (half_width, half_height) = view.extent();
    let crown = pose.to_world(CROWN);
    let bolts = if rng.unit() > 0.6 { 2 } else { 1 };
    for _ in 0..bolts {
        let from = (
            rng.range(-half_width, half_width) * 0.8,
            -half_height - 0.05,
        );
        let target = (crown.0 + rng.range(-0.01, 0.01), crown.1);
        let path = bolt(&mut rng, from, target, 6, 0.22);
        stroke(&mut field, view, &path, 0.7, 1.0);
        let branches = 2 + (rng.unit() * 3.0) as usize;
        for _ in 0..branches {
            let start = path[(rng.range(0.1, 0.75) * path.len() as f32) as usize];
            let heading = (target.1 - from.1).atan2(target.0 - from.0)
                + rng.range(0.4, 0.95) * if rng.unit() > 0.5 { 1.0 } else { -1.0 };
            let length = rng.range(0.25, 0.6);
            let end = (
                start.0 + heading.cos() * length,
                start.1 + heading.sin() * length,
            );
            let branch = bolt(&mut rng, start, end, 4, 0.25);
            stroke(&mut field, view, &branch, 0.35, 0.55);
        }
    }
    // Arcs from edge to edge of the head.
    for _ in 0..3 {
        let edge = |rng: &mut Rng| {
            let along = rng.range(-1.0, 1.0);
            let side = if rng.unit() > 0.5 { 1.0 } else { -1.0 };
            pose.to_world(if rng.unit() > 0.4 {
                (along * HEAD.0, side * HEAD.1)
            } else {
                (side * HEAD.0, along * HEAD.1)
            })
        };
        let (a, b) = (edge(&mut rng), edge(&mut rng));
        let arc = bolt(&mut rng, a, b, 3, 0.3);
        stroke(&mut field, view, &arc, 0.25, 0.6);
    }
    let brightness = strike.brightness.min(1.4);
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            let value = field.get(x, y) * brightness;
            if value > 0.004 {
                let color = mix(GLOW, CORE, smoothstep(0.55, 1.0, value));
                canvas.add(
                    x as isize,
                    y as isize,
                    color.map(|channel| channel * value * 1.3),
                );
            }
        }
    }
}
