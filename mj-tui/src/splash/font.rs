//! The title, MJÖLNIR, in gold block letters revealed by a white-hot sweep.

use super::canvas::{Canvas, Rgb};
use super::{GROUND_Y, HOLD_START, TITLE_START, View, mix, smoothstep};

const GLYPH_WIDTH: usize = 5;
/// Two rows above the capitals hold the umlaut.
const GLYPH_HEIGHT: usize = 9;
const GAP: usize = 2;
const WIDTH: usize = 7 * GLYPH_WIDTH + 6 * GAP;
const REVEAL: f32 = 0.45;

#[rustfmt::skip]
const GLYPHS: [[&str; GLYPH_HEIGHT]; 7] = [
    ["", "", "#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"],
    ["", "", "..###", "...#.", "...#.", "...#.", "...#.", "#..#.", ".##.."],
    [".#.#.", "", ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
    ["", "", "#....", "#....", "#....", "#....", "#....", "#....", "#####"],
    ["", "", "#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"],
    ["", "", "#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
    ["", "", "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
];

fn lit(letter: usize, row: usize, column: usize) -> bool {
    GLYPHS[letter][row].as_bytes().get(column) == Some(&b'#')
}

#[derive(Clone, Copy)]
pub(super) struct Title {
    x: usize,
    y: usize,
    scale: usize,
}

impl Title {
    /// Centres the title in the space between the horizon and the status
    /// row, as large as fits. A terminal too short for it shows the scene
    /// alone.
    pub(super) fn layout(view: &View) -> Option<Self> {
        let horizon = view.height as f32 / 2.0 + GROUND_Y * view.scale;
        let top = (horizon + (view.scale * 0.1).max(2.0)) as usize;
        let bottom = view.height.saturating_sub(2);
        let room = bottom.saturating_sub(top);
        let scale = (view.width * 17 / 20 / WIDTH).min(room / GLYPH_HEIGHT);
        (scale > 0).then(|| Self {
            x: (view.width - WIDTH * scale) / 2,
            y: top + (room - GLYPH_HEIGHT * scale) / 2,
            scale,
        })
    }
}

/// How far the reveal has swept, in font columns.
fn sweep(t: f32) -> f32 {
    -6.0 + smoothstep(TITLE_START, TITLE_START + REVEAL, t) * (WIDTH as f32 + 12.0)
}

/// A highlight that crosses the finished title while the splash waits.
fn glint(t: f32, holding: bool) -> Option<f32> {
    if !holding {
        return None;
    }
    let cycle = ((t - HOLD_START.as_secs_f32()) % 1.8) / 0.7;
    (cycle < 1.0).then(|| -10.0 + cycle * (WIDTH as f32 + 30.0))
}

pub(super) fn draw(canvas: &mut Canvas, placement: Title, t: f32, holding: bool) {
    let front = sweep(t);
    if front <= -6.0 {
        return;
    }
    let glint = glint(t, holding);
    let breath = if holding {
        0.9 + 0.1 * (t * 4.0).sin()
    } else {
        1.0
    };
    let cells = (0..7).flat_map(|letter| {
        (0..GLYPH_HEIGHT).flat_map(move |row| {
            (0..GLYPH_WIDTH)
                .filter(move |column| lit(letter, row, *column))
                .map(move |column| (letter, row, column))
        })
    });
    let shown = |letter: usize, column: usize| {
        let x = letter * (GLYPH_WIDTH + GAP) + column;
        (x as f32) < front
    };
    // A drop shadow first, so the letters stand off the rock.
    for (letter, row, column) in cells.clone() {
        if shown(letter, column) {
            fill_block(canvas, placement, letter, row, column, 1, |under| {
                under.map(|channel| channel * 0.2)
            });
        }
    }
    for (letter, row, column) in cells {
        if !shown(letter, column) {
            continue;
        }
        let x = (letter * (GLYPH_WIDTH + GAP) + column) as f32;
        let mut gold = mix(
            [1.0, 0.86, 0.45],
            [0.85, 0.42, 0.10],
            row.saturating_sub(2) as f32 / 6.0,
        );
        if row == 0 || !lit(letter, row - 1, column) {
            gold = gold.map(|channel| channel * 1.3);
        }
        let hot = 2.0 * (-(front - x) / 2.5).exp();
        let shine = glint.map_or(0.0, |position| {
            1.5 * (-((x + row as f32 * 0.6 - position).powi(2)) / 4.0).exp()
        });
        let lit_color: Rgb = gold.map(|channel| channel * breath + hot + shine);
        fill_block(canvas, placement, letter, row, column, 0, |_| lit_color);
    }
}

fn fill_block(
    canvas: &mut Canvas,
    placement: Title,
    letter: usize,
    row: usize,
    column: usize,
    offset: usize,
    paint: impl Fn(Rgb) -> Rgb,
) {
    let scale = placement.scale;
    let left = placement.x + (letter * (GLYPH_WIDTH + GAP) + column) * scale + offset;
    let top = placement.y + row * scale + offset;
    for y in top..(top + scale).min(canvas.height) {
        for x in left..(left + scale).min(canvas.width) {
            let under = canvas.get(x, y);
            canvas.set(x, y, paint(under));
        }
    }
}
