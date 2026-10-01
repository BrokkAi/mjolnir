//! A linear-light pixel buffer at two pixels per cell, and the single place
//! where splash colors become terminal colors.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

pub(super) type Rgb = [f32; 3];

/// Ordered-dither thresholds. They depend only on position, so dithered
/// pixels stay put between frames and the terminal diff stays small.
const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Output levels are this far apart, which hides slow drift in the clouds
/// from the cell diff without visible banding once dithered.
const QUANTUM: f32 = 6.0;

pub(super) fn bayer(x: usize, y: usize) -> f32 {
    (f32::from(BAYER[y % 4][x % 4]) + 0.5) / 16.0
}

#[allow(
    clippy::disallowed_methods,
    reason = "The splash paints every cell's foreground and background itself, so its truecolor scene does not depend on the terminal palette."
)]
pub(crate) fn color([red, green, blue]: [u8; 3]) -> Color {
    Color::Rgb(red, green, blue)
}

pub(super) struct Dissolve {
    pub(super) progress: f32,
    pub(super) background: Color,
}

pub(super) struct Canvas {
    pub(super) width: usize,
    pub(super) height: usize,
    pixels: Vec<Rgb>,
}

impl Canvas {
    pub(super) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![[0.0; 3]; width * height],
        }
    }

    pub(super) fn get(&self, x: usize, y: usize) -> Rgb {
        self.pixels[y * self.width + x]
    }

    pub(super) fn set(&mut self, x: usize, y: usize, color: Rgb) {
        self.pixels[y * self.width + x] = color;
    }

    /// Adds light at a pixel; positions off the canvas are ignored.
    pub(super) fn add(&mut self, x: isize, y: isize, color: Rgb) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        let pixel = &mut self.pixels[y as usize * self.width + x as usize];
        for channel in 0..3 {
            pixel[channel] += color[channel];
        }
    }

    /// Adds light at a sub-pixel position, shared among the four nearest
    /// pixels so moving points glide instead of jumping.
    pub(super) fn splat(&mut self, (x, y): (f32, f32), color: Rgb) {
        let (fx, fy) = (x - 0.5, y - 0.5);
        let (x0, y0) = (fx.floor(), fy.floor());
        let (dx, dy) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);
        for (ox, oy, weight) in [
            (0, 0, (1.0 - dx) * (1.0 - dy)),
            (1, 0, dx * (1.0 - dy)),
            (0, 1, (1.0 - dx) * dy),
            (1, 1, dx * dy),
        ] {
            self.add(x0 + ox, y0 + oy, super::scaled(color, weight));
        }
    }

    pub(super) fn add_all(&mut self, color: Rgb) {
        for pixel in &mut self.pixels {
            for channel in 0..3 {
                pixel[channel] += color[channel];
            }
        }
    }

    /// Tone-maps and dithers one pixel to display bytes.
    fn display(&self, x: usize, y: usize, boost: f32) -> [u8; 3] {
        let pixel = self.get(x, y);
        let threshold = bayer(x, y);
        pixel.map(|value| {
            let value = (value * boost).max(0.0);
            // Narkowicz's ACES fit: bright light rolls off instead of clipping.
            let mapped = (value * (2.51 * value + 0.03)) / (value * (2.43 * value + 0.59) + 0.14);
            let level = (mapped.clamp(0.0, 1.0) * 255.0 / QUANTUM + threshold).floor() * QUANTUM;
            level.min(255.0) as u8
        })
    }

    /// Writes the canvas into `area`, one cell per pixel pair. During the
    /// dissolve, cells give way to the background in dithered order and
    /// glow briefly just before they go.
    pub(super) fn blit(&self, area: Rect, buf: &mut Buffer, dissolve: Option<&Dissolve>) {
        for row in 0..usize::from(area.height) {
            for column in 0..usize::from(area.width) {
                let cell = &mut buf[(area.x + column as u16, area.y + row as u16)];
                cell.reset();
                let mut boost = 1.0;
                if let Some(dissolve) = dissolve {
                    let remaining = bayer(column, row) - dissolve.progress;
                    if remaining < 0.0 {
                        cell.set_bg(dissolve.background);
                        continue;
                    }
                    boost += 2.5 * (1.0 - remaining / 0.15).max(0.0);
                }
                let top = self.display(column, row * 2, boost);
                let bottom = self.display(column, row * 2 + 1, boost);
                if top == bottom {
                    cell.set_bg(color(bottom));
                } else {
                    cell.set_symbol("▀");
                    cell.set_fg(color(top));
                    cell.set_bg(color(bottom));
                }
            }
        }
    }
}

/// A scalar light field with max blending, for strokes that cross
/// themselves (lightning, cracks) without bright knots at the joints.
pub(super) struct Field {
    width: usize,
    height: usize,
    values: Vec<f32>,
}

impl Field {
    pub(super) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            values: vec![0.0; width * height],
        }
    }

    pub(super) fn get(&self, x: usize, y: usize) -> f32 {
        self.values[y * self.width + x]
    }

    /// Draws a glowing segment between two pixel positions: a solid core
    /// of `core` pixels and an exponential halo of `glow` pixels.
    pub(super) fn segment(
        &mut self,
        a: (f32, f32),
        b: (f32, f32),
        core: f32,
        glow: f32,
        intensity: f32,
    ) {
        let reach = core + glow * 3.0;
        let left = (a.0.min(b.0) - reach).floor().max(0.0) as usize;
        let top = (a.1.min(b.1) - reach).floor().max(0.0) as usize;
        let right = ((a.0.max(b.0) + reach).ceil().max(0.0) as usize).min(self.width);
        let bottom = ((a.1.max(b.1) + reach).ceil().max(0.0) as usize).min(self.height);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let length_squared = (dx * dx + dy * dy).max(1e-6);
        for y in top..bottom {
            for x in left..right {
                let (px, py) = (x as f32 + 0.5 - a.0, y as f32 + 0.5 - a.1);
                let along = ((px * dx + py * dy) / length_squared).clamp(0.0, 1.0);
                let distance = (px - dx * along).hypot(py - dy * along);
                let value = intensity
                    * (super::smoothstep(core + 0.8, core * 0.5, distance)
                        + 0.45 * (-distance / glow).exp());
                let slot = &mut self.values[y * self.width + x];
                *slot = slot.max(value);
            }
        }
    }
}
