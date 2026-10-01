//! Mjölnir as a signed distance field: a flared Uru head with a knotwork
//! band, a short leather-wrapped haft, a pommel and its lanyard loop.

use std::f32::consts::TAU;

use super::canvas::{Canvas, Rgb};
use super::{DESCENT_START, IMPACT, View, hash2, mix, rotate, scaled, smoothstep};

/// Where the head's centre rests once it has struck.
const REST_Y: f32 = 0.20;
/// The point the hammer spins around: its centre of mass, inside the head.
const PIVOT: (f32, f32) = (0.0, -0.15);
const DROP_FROM: f32 = -2.4;
const TURNS: f32 = 2.25;
/// Earlier instants averaged into each falling frame, for motion blur.
const BLUR: [f32; 3] = [0.0, 0.012, 0.024];
/// Light comes from the upper left.
const TOWARD_LIGHT: (f32, f32) = (-0.55, -0.835);
const RIM: Rgb = [0.35, 0.65, 1.25];
/// Everything drawn lies within this distance of the pivot.
const REACH: f32 = 1.05;

/// The local point that lightning strikes: the top of the lanyard loop.
pub(super) const CROWN: (f32, f32) = (0.0, -1.045);
/// The head's half extents, for arcs that crawl over it.
pub(super) const HEAD: (f32, f32) = (0.42, 0.2);

#[derive(Clone, Copy)]
pub(super) struct Pose {
    origin: (f32, f32),
    angle: f32,
}

impl Pose {
    /// The hammer's placement at scene time `t`, once it has appeared. It
    /// falls under gravity while its spin unwinds, and lands upright.
    pub(super) fn at(t: f32) -> Option<Self> {
        if t < DESCENT_START {
            return None;
        }
        let progress = ((t - DESCENT_START) / (IMPACT - DESCENT_START)).min(1.0);
        let y = if progress < 1.0 {
            DROP_FROM + (REST_Y - DROP_FROM) * progress * progress
        } else {
            let since = t - IMPACT;
            REST_Y - 0.04 * (since * 30.0).sin().abs() * (-since * 10.0).exp()
        };
        Some(Self {
            origin: (0.0, y),
            angle: TURNS * TAU * (1.0 - progress).powf(1.7),
        })
    }

    pub(super) fn to_world(self, local: (f32, f32)) -> (f32, f32) {
        let (x, y) = rotate((local.0 - PIVOT.0, local.1 - PIVOT.1), self.angle);
        (self.origin.0 + PIVOT.0 + x, self.origin.1 + PIVOT.1 + y)
    }

    fn to_local(self, world: (f32, f32)) -> (f32, f32) {
        let (x, y) = rotate(
            (
                world.0 - self.origin.0 - PIVOT.0,
                world.1 - self.origin.1 - PIVOT.1,
            ),
            -self.angle,
        );
        (x + PIVOT.0, y + PIVOT.1)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Head,
    Haft,
    Metal,
}

fn round_box((x, y): (f32, f32), (half_x, half_y): (f32, f32), radius: f32) -> f32 {
    let (dx, dy) = (x.abs() - half_x + radius, y.abs() - half_y + radius);
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius
}

fn sdf((x, y): (f32, f32)) -> (f32, Part) {
    let body = round_box((x, y), (0.34, 0.185), 0.03);
    let faces = round_box((x.abs() - 0.375, y), (0.05, 0.205), 0.02);
    let head = body.min(faces);
    let haft = round_box((x, y + 0.51), (0.048, 0.31), 0.02);
    let pommel = x.hypot(y + 0.86) - 0.068;
    let lanyard = (x.hypot(y + 0.975) - 0.055).abs() - 0.014;
    let metal = pommel.min(lanyard);
    if head <= haft && head <= metal {
        (head, Part::Head)
    } else if haft <= metal {
        (haft, Part::Haft)
    } else {
        (metal, Part::Metal)
    }
}

fn normal(point: (f32, f32)) -> (f32, f32) {
    const EPSILON: f32 = 0.004;
    let dx = sdf((point.0 + EPSILON, point.1)).0 - sdf((point.0 - EPSILON, point.1)).0;
    let dy = sdf((point.0, point.1 + EPSILON)).0 - sdf((point.0, point.1 - EPSILON)).0;
    let length = dx.hypot(dy).max(1e-6);
    (dx / length, dy / length)
}

/// Brushed Uru metal: cool steel, lighter toward the top, with fine streaks.
fn uru((_, y): (f32, f32)) -> Rgb {
    let streak = hash2((y * 90.0).floor() as i32, 7) * 0.12;
    let tone = 0.30 + 0.28 * (0.5 - y * 0.9).clamp(0.0, 1.0) + streak;
    [tone * 0.92, tone * 0.98, tone * 1.08]
}

/// The knotwork band round the middle of the head: a diagonal lattice of
/// grooves framed by two rings.
fn knotwork((x, y): (f32, f32)) -> f32 {
    if x.abs() > 0.165 || y.abs() > 0.17 {
        return 1.0;
    }
    let frame = (x.abs() - 0.15).abs();
    let lattice = |value: f32| (value - value.floor() - 0.5).abs();
    let groove = lattice((x + y) * 9.0)
        .min(lattice((x - y) * 9.0))
        .min(frame * 4.0);
    0.5 + 0.5 * smoothstep(0.02, 0.09, groove)
}

fn leather((x, y): (f32, f32)) -> Rgb {
    let wrap = y * 16.0 + x * 5.0;
    let stripe = smoothstep(0.42, 0.58, wrap - wrap.floor());
    let base = mix([0.42, 0.24, 0.12], [0.20, 0.11, 0.05], stripe);
    let across = (x / 0.05).clamp(-1.0, 1.0);
    let roundness = (across * std::f32::consts::FRAC_PI_2).cos();
    scaled(base, 0.45 + 0.75 * roundness - 0.15 * across)
}

fn shade(local: (f32, f32), distance: f32, part: Part, pose: Pose, charge: f32) -> Rgb {
    let depth = -distance;
    let lit = {
        let (nx, ny) = rotate(normal(local), pose.angle);
        nx * TOWARD_LIGHT.0 + ny * TOWARD_LIGHT.1
    };
    let base = match part {
        Part::Head => scaled(uru(local), knotwork(local)),
        Part::Haft => leather(local),
        Part::Metal => uru(local),
    };
    let bevel = smoothstep(0.045, 0.0, depth);
    let lit_color = scaled(base, 1.0 + bevel * lit * 0.9);
    let rim = smoothstep(0.03, 0.0, depth) * (0.25 + charge);
    mix(lit_color, RIM, (rim * 0.6).min(1.0))
}

/// Paints the hammer over the scene, with motion blur while it falls and a
/// blue halo whose strength follows `charge`.
pub(super) fn draw(canvas: &mut Canvas, view: &View, t: f32, charge: f32) {
    let blur: &[f32] = if t < IMPACT { &BLUR } else { &BLUR[..1] };
    let poses: Vec<Pose> = blur
        .iter()
        .filter_map(|delay| Pose::at(t - delay))
        .collect();
    if poses.is_empty() {
        return;
    }
    let mut left = f32::MAX;
    let mut top = f32::MAX;
    let mut right = f32::MIN;
    let mut bottom = f32::MIN;
    for pose in &poses {
        let (cx, cy) = view.pixel(pose.to_world(PIVOT));
        let reach = REACH * view.scale + 2.0;
        left = left.min(cx - reach);
        top = top.min(cy - reach);
        right = right.max(cx + reach);
        bottom = bottom.max(cy + reach);
    }
    let columns = (left.max(0.0) as usize)..(right.max(0.0) as usize).min(canvas.width);
    let rows = (top.max(0.0) as usize)..(bottom.max(0.0) as usize).min(canvas.height);
    let samples = poses.len() as f32;
    for y in rows {
        for x in columns.clone() {
            let point = view.scene(x, y);
            let mut color = [0.0; 3];
            let mut coverage = 0.0;
            let mut halo = 0.0;
            for (index, pose) in poses.iter().enumerate() {
                let local = pose.to_local(point);
                let (distance, part) = sdf(local);
                if index == 0 && distance > 0.0 {
                    halo = (-distance * 16.0).exp();
                }
                let cover = (0.5 - distance * view.scale).clamp(0.0, 1.0);
                if cover > 0.0 {
                    let shaded = shade(local, distance, part, *pose, charge);
                    for channel in 0..3 {
                        color[channel] += shaded[channel] * cover;
                    }
                    coverage += cover;
                }
            }
            let alpha = coverage / samples;
            let under = canvas.get(x, y);
            let glow = halo * charge * 0.5 * (1.0 - alpha);
            let mut out = [0.0; 3];
            for channel in 0..3 {
                out[channel] =
                    under[channel] * (1.0 - alpha) + color[channel] / samples + RIM[channel] * glow;
            }
            canvas.set(x, y, out);
        }
    }
}
