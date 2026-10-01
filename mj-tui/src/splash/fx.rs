//! The storm, the ground, and everything the impact throws off.

use std::f32::consts::{FRAC_PI_2, PI};

use super::canvas::{Canvas, Field, Rgb};
use super::{GROUND_Y, IMPACT, Rng, View, hash2, mix, scaled, smoothstep};

const SPARKS: u64 = 90;
const GRAVITY: f32 = 5.5;
const CRACKS: u64 = 9;

fn value_noise(x: f32, y: f32) -> f32 {
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let top = hash2(ix, iy) + (hash2(ix + 1, iy) - hash2(ix, iy)) * sx;
    let bottom = hash2(ix, iy + 1) + (hash2(ix + 1, iy + 1) - hash2(ix, iy + 1)) * sx;
    top + (bottom - top) * sy
}

fn clouds(x: f32, y: f32) -> f32 {
    let mut total = 0.0;
    let mut amplitude = 0.55;
    let mut frequency = 1.0;
    for _ in 0..3 {
        total += amplitude * value_noise(x * frequency, y * frequency);
        amplitude *= 0.5;
        frequency *= 2.1;
    }
    total
}

/// Sheet lightning inside the clouds before the hammer lands: a soft bloom
/// at a random place in some flicker intervals.
fn storm_glow(t: f32) -> Option<((f32, f32), f32)> {
    if t >= IMPACT {
        return None;
    }
    let interval = (t / 0.11).floor();
    let mut rng = Rng::new(0x5707 ^ interval as u64);
    if rng.unit() < 0.55 {
        return None;
    }
    let within = t / 0.11 - interval;
    let centre = (rng.range(-1.4, 1.4), rng.range(-0.95, -0.45));
    Some((centre, rng.range(0.4, 0.9) * (1.0 - within)))
}

/// A night sky: scrolling storm clouds, stars in the gaps, and the clouds
/// lit from within by `flash`.
pub(super) fn sky(canvas: &mut Canvas, view: &View, t: f32, flash: f32) {
    let glow = storm_glow(t);
    for py in 0..canvas.height {
        for px in 0..canvas.width {
            let (x, y) = view.scene(px, py);
            if y > GROUND_Y {
                continue;
            }
            let mut color = mix(
                [0.010, 0.008, 0.030],
                [0.045, 0.030, 0.090],
                smoothstep(-1.05, GROUND_Y, y),
            );
            let reach = smoothstep(0.15, -0.6, y);
            let density = if reach > 0.0 {
                smoothstep(0.42, 0.85, clouds(x * 1.3 + t * 0.12, y * 2.6 - t * 0.03)) * reach
            } else {
                0.0
            };
            let mut light = 0.18 + flash * 0.9;
            if let Some(((gx, gy), strength)) = glow {
                let distance = (x - gx).powi(2) + (y - gy).powi(2) * 2.0;
                light += strength * 2.2 * (-distance / 0.18).exp();
            }
            let cloud = [0.55, 0.42, 1.0];
            for channel in 0..3 {
                color[channel] += cloud[channel] * density * light;
            }
            if density < 0.15 && y < 0.25 {
                let seed = hash2(px as i32, py as i32);
                if seed > 0.994 {
                    let twinkle = 0.55 + 0.45 * (t * 7.0 + seed * 1000.0).sin();
                    let star = twinkle * (1.0 - density * 6.0) * 0.9;
                    for (channel, tint) in [0.75, 0.8, 1.0].into_iter().enumerate() {
                        color[channel] += tint * star;
                    }
                }
            }
            canvas.set(px, py, color);
        }
    }
}

/// How hot the ground is where the hammer struck: a flash of heat that
/// settles into a glowing bed of embers.
fn heat(t: f32) -> f32 {
    let since = t - IMPACT;
    if since < 0.0 {
        0.0
    } else {
        (-since * 2.2).exp() + 0.18 + 0.05 * (t * 9.0).sin()
    }
}

/// Dark rock below the horizon, a rim of storm light along it, and the
/// glow of the impact.
pub(super) fn ground(canvas: &mut Canvas, view: &View, t: f32, flash: f32) {
    let heat = heat(t);
    for py in 0..canvas.height {
        for px in 0..canvas.width {
            let (x, y) = view.scene(px, py);
            if y <= GROUND_Y {
                continue;
            }
            let depth = y - GROUND_Y;
            let grain = value_noise(x * 9.0, depth * 26.0) * 0.02;
            let mut color = [0.030 + grain, 0.026 + grain, 0.042 + grain];
            let rim = (-depth * 55.0).exp() * (0.25 + flash);
            for (channel, tint) in [0.16, 0.14, 0.34].into_iter().enumerate() {
                color[channel] += tint * rim;
            }
            let pool = (-(x * x + (depth * 3.0).powi(2)) * 5.0).exp() * heat;
            for (channel, tint) in [1.0, 0.42, 0.12].into_iter().enumerate() {
                color[channel] += tint * pool * 0.9;
            }
            canvas.set(px, py, color);
        }
    }
}

/// Cracks that race outward from the impact, glowing white-hot and cooling
/// to a dull red.
pub(super) fn cracks(canvas: &mut Canvas, view: &View, t: f32) {
    let since = t - IMPACT;
    if since < 0.0 {
        return;
    }
    let grown = smoothstep(0.0, 0.2, since);
    let mut field = Field::new(canvas.width, canvas.height);
    for crack in 0..CRACKS {
        let mut rng = Rng::new(0xC4AC ^ crack.wrapping_mul(0x9E37));
        let spread = (crack as f32 + rng.range(0.2, 0.8)) / CRACKS as f32;
        let mut heading = -0.12 * PI + spread * 1.24 * PI;
        let mut point = (heading.cos() * 0.3, GROUND_Y + 0.006);
        let segments = 6;
        let visible = grown * segments as f32;
        for segment in 0..segments {
            heading += rng.range(-0.45, 0.45);
            let length = 0.14 * rng.range(0.7, 1.3);
            let next = (
                point.0 + heading.cos() * length,
                (point.1 + heading.sin() * length * 0.45).max(GROUND_Y + 0.004),
            );
            let shown = (visible - segment as f32).clamp(0.0, 1.0);
            if shown > 0.0 {
                let end = (
                    point.0 + (next.0 - point.0) * shown,
                    point.1 + (next.1 - point.1) * shown,
                );
                let taper = 1.0 - segment as f32 / segments as f32 * 0.6;
                field.segment(view.pixel(point), view.pixel(end), 0.35, 1.4, taper);
            }
            point = next;
        }
    }
    let cooling = (-since * 2.8).exp();
    let color = mix([0.55, 0.08, 0.02], [1.0, 0.85, 0.5], cooling);
    let strength = 0.5 + 1.2 * cooling + 0.1 * (t * 11.0).sin();
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            let value = field.get(x, y);
            if value > 0.004 {
                canvas.add(x as isize, y as isize, scaled(color, value * strength));
            }
        }
    }
}

/// Two shock rings with split color fringes: one along the ground in
/// perspective, one through the air around the head.
pub(super) fn shockwave(canvas: &mut Canvas, view: &View, t: f32) {
    let since = t - IMPACT;
    if !(0.0..0.9).contains(&since) {
        return;
    }
    let ground_radius = since * 2.8;
    let ground_fade = (-since * 3.5).exp() * 1.2;
    let air_radius = since * 3.6;
    let air_fade = (-since * 5.0).exp() * 0.6;
    let fringe = [1.05, 1.0, 0.95];
    for py in 0..canvas.height {
        for px in 0..canvas.width {
            let (x, y) = view.scene(px, py);
            let mut light = [0.0; 3];
            for channel in 0..3 {
                let radius = ground_radius * fringe[channel];
                let squash = (y - GROUND_Y) / 0.28;
                let ring = (x.hypot(squash) - radius).abs();
                light[channel] += (-(ring / 0.045).powi(2)).exp() * ground_fade;
                let radius = air_radius * fringe[channel];
                let ring = (x.hypot(y - 0.2) - radius).abs();
                light[channel] += (-(ring / 0.025).powi(2)).exp() * air_fade;
            }
            canvas.add(
                px as isize,
                py as isize,
                [light[0], light[1] * 0.9, light[2] * 1.1],
            );
        }
    }
}

fn ember(heat: f32) -> Rgb {
    if heat > 0.7 {
        mix([1.0, 0.8, 0.3], [1.0, 0.97, 0.85], (heat - 0.7) / 0.3)
    } else if heat > 0.35 {
        mix([1.0, 0.42, 0.08], [1.0, 0.8, 0.3], (heat - 0.35) / 0.35)
    } else {
        mix([0.0, 0.0, 0.0], [1.0, 0.42, 0.08], heat / 0.35)
    }
}

/// Sparks thrown up by the impact, falling under gravity with short
/// streaks behind them.
pub(super) fn sparks(canvas: &mut Canvas, view: &View, t: f32) {
    let since = t - IMPACT;
    if !(0.0..1.1).contains(&since) {
        return;
    }
    for spark in 0..SPARKS {
        let mut rng = Rng::new(0x5BA4 ^ spark.wrapping_mul(0x2545_F491));
        let heading = -FRAC_PI_2 + rng.range(-1.25, 1.25);
        let speed = rng.range(1.0, 3.4);
        let life = rng.range(0.35, 1.05);
        let start = rng.range(-0.38, 0.38);
        if since > life {
            continue;
        }
        let heat = 1.0 - since / life;
        let (vx, vy) = (heading.cos() * speed, heading.sin() * speed);
        for (step, weight) in [(0.0, 1.0), (0.012, 0.6), (0.024, 0.3)] {
            let age = since - step;
            if age < 0.0 {
                break;
            }
            let position = (
                start + vx * age,
                GROUND_Y - 0.02 + vy * age + 0.5 * GRAVITY * age * age,
            );
            canvas.splat(view.pixel(position), scaled(ember(heat), 2.2 * weight));
        }
    }
}

/// The white flash of the strike, cooling to blue over three frames.
pub(super) fn impact_flash(canvas: &mut Canvas, t: f32) {
    let since = t - IMPACT;
    if !(0.0..0.07).contains(&since) {
        return;
    }
    let strength = (1.0 - since / 0.07).powi(2) * 1.6;
    canvas.add_all([strength * 0.85, strength * 0.9, strength * 1.15]);
}
