//! Ratatui renderer for Ragnarok sprite data owned by `mj-agents`.

#![allow(clippy::disallowed_methods)]

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub use mj_agents::ragnarok_sprites::*;

/// Fixed sprite palette (hero + accent are injected per fighter/action).
fn pixel_color(key: char, hero: Color, accent: Color) -> Option<Color> {
    match key {
        ' ' => None,
        'H' => Some(Color::Rgb(150, 156, 168)),
        'W' => Some(Color::Rgb(230, 224, 200)),
        'S' => Some(Color::Rgb(224, 172, 126)),
        'O' => Some(Color::Rgb(38, 40, 48)),
        'B' | 'P' => Some(hero),
        'T' => Some(Color::Rgb(96, 84, 60)),
        'L' => Some(Color::Rgb(126, 86, 46)),
        'D' => Some(Color::Rgb(72, 60, 50)),
        'X' => Some(Color::Rgb(139, 101, 54)),
        'A' => Some(Color::Rgb(192, 202, 214)),
        'M' => Some(accent),
        'R' => Some(Color::Rgb(202, 44, 44)),
        'G' => Some(Color::Rgb(240, 196, 60)),
        _ => None,
    }
}

/// Render one frame into `SPRITE_H / 2` half-block lines. Each terminal cell
/// carries two vertically stacked pixels; transparent pixels leave the
/// terminal background untouched.
pub fn render(frame: &Frame, hero: Color, accent: Color) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(SPRITE_H / 2);
    for pair in frame.chunks(2) {
        let top_row: Vec<char> = pair[0].chars().collect();
        let bottom_row: Vec<char> = pair
            .get(1)
            .map(|row| row.chars().collect())
            .unwrap_or_else(|| vec![' '; SPRITE_W]);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(SPRITE_W);
        for col in 0..SPRITE_W {
            let top = top_row
                .get(col)
                .and_then(|&key| pixel_color(key, hero, accent));
            let bottom = bottom_row
                .get(col)
                .and_then(|&key| pixel_color(key, hero, accent));
            let span = match (top, bottom) {
                (None, None) => Span::raw(" "),
                (Some(t), None) => Span::styled("▀", Style::default().fg(t)),
                (None, Some(b)) => Span::styled("▄", Style::default().fg(b)),
                (Some(t), Some(b)) if t == b => Span::styled("█", Style::default().fg(t)),
                (Some(t), Some(b)) => Span::styled("▀", Style::default().fg(t).bg(b)),
            };
            spans.push(span);
        }
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: &str = " HWSOBPTLDXAMRG";

    fn all_frames() -> Vec<(&'static str, &'static [Frame])> {
        vec![
            ("idle", frames(SpriteKind::Idle)),
            ("march", frames(SpriteKind::March)),
            ("swing", frames(SpriteKind::Swing)),
            ("cast", frames(SpriteKind::Cast)),
            ("wound", frames(SpriteKind::Wound)),
            ("victor", frames(SpriteKind::Victor)),
            ("slain", frames(SpriteKind::Slain)),
        ]
    }

    #[test]
    fn every_frame_is_exactly_sprite_sized() {
        for (name, set) in all_frames() {
            assert!(!set.is_empty(), "{name} has no frames");
            for (fi, frame) in set.iter().enumerate() {
                assert_eq!(frame.len(), SPRITE_H, "{name}[{fi}] row count");
                for (ri, row) in frame.iter().enumerate() {
                    assert_eq!(
                        row.chars().count(),
                        SPRITE_W,
                        "{name}[{fi}] row {ri} is {} chars: {row:?}",
                        row.chars().count()
                    );
                }
            }
        }
    }

    #[test]
    fn every_pixel_is_a_known_palette_key() {
        for (name, set) in all_frames() {
            for (fi, frame) in set.iter().enumerate() {
                for (ri, row) in frame.iter().enumerate() {
                    for (ci, key) in row.chars().enumerate() {
                        assert!(
                            PALETTE.contains(key),
                            "{name}[{fi}] row {ri} col {ci}: unknown palette key {key:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn render_produces_half_height_full_width_lines() {
        let hero = Color::Cyan;
        let accent = Color::Yellow;
        for (name, set) in all_frames() {
            for frame in set {
                let lines = render(frame, hero, accent);
                assert_eq!(lines.len(), SPRITE_H / 2, "{name} line count");
                for line in &lines {
                    let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                    assert_eq!(width, SPRITE_W, "{name} line width");
                }
            }
        }
    }

    #[test]
    fn render_maps_pixel_pairs_to_half_blocks() {
        let mut frame: Frame = ["              "; SPRITE_H];
        frame[0] = "HS  H         ";
        frame[1] = "H S S         ";
        let lines = render(&frame, Color::Cyan, Color::Yellow);
        let cells: Vec<&Span> = lines[0].spans.iter().collect();
        // Both pixels set and equal → solid block.
        assert_eq!(cells[0].content, "█");
        // Top only → upper half block, no background.
        assert_eq!(cells[1].content, "▀");
        assert_eq!(cells[1].style.bg, None);
        // Bottom only → lower half block.
        assert_eq!(cells[2].content, "▄");
        // Neither → plain space.
        assert_eq!(cells[3].content, " ");
        // Both set, different colors → upper half with bg fill.
        assert_eq!(cells[4].content, "▀");
        assert!(cells[4].style.bg.is_some());
    }

    #[test]
    fn heroic_pixels_take_the_fighter_color() {
        let lines = render(
            &frames(SpriteKind::Idle)[0],
            Color::Rgb(1, 2, 3),
            Color::Yellow,
        );
        let uses_hero = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Rgb(1, 2, 3)))
        });
        assert!(uses_hero, "beard/trim must carry the fighter color");
    }
}
