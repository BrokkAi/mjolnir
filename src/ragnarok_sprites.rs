//! Pixel-art viking sprites for the Ragnarok arena.
//!
//! Sprites are 14×14 pixel grids encoded as char maps (one palette key per
//! pixel) and rendered with terminal half-blocks: each cell shows two
//! vertically stacked pixels (`▀` with foreground = top pixel, background =
//! bottom pixel), so a sprite occupies 14 columns × 7 rows. Beard and tabard
//! trim take the fighter's arena color so champions stay tellable apart; the
//! `M` accent pixels take a per-action color (sparks, lightning, a scrying
//! orb, song notes…).
//!
//! Every frame is validated by tests: exactly [`SPRITE_H`] rows of
//! [`SPRITE_W`] chars, all from the palette. Misaligned art fails CI instead
//! of rendering a mangled viking.

// Pixel art, not UI chrome. Every RGB below is a texture — skin, wood, leather,
// steel — where the exact value *is* the artwork. Snapping these onto the ANSI
// 16 the way the rest of the TUI does would not adapt the sprites to a terminal
// theme, it would destroy them. The palette-driven colors (`hero`, `accent`)
// still arrive from the caller, so the parts that should follow the theme do.
#![allow(clippy::disallowed_methods)]

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub const SPRITE_W: usize = 14;
pub const SPRITE_H: usize = 14;

/// One animation frame: `SPRITE_H` rows of `SPRITE_W` palette chars.
pub type Frame = [&'static str; SPRITE_H];

/// Which animation a fighter is currently playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpriteKind {
    /// At ease, axe on shoulder, breathing.
    Idle,
    /// Marching into the arena (summoned / forging camp / connecting).
    March,
    /// Swinging the axe (forging code, hurling shell).
    Swing,
    /// Arm raised, channeling an orb (scrying, pondering, chanting).
    Cast,
    /// Staggered by a failing rune.
    Wound,
    /// Axe aloft, crowned.
    Victor,
    /// A heap on the arena floor.
    Slain,
}

// Palette keys:
//   ' ' transparent   H helmet steel    W horn bone      S skin
//   O   eye           B beard (hero)    P trim (hero)    T tunic
//   L   belt leather  D boots           X axe haft       A axe head
//   M   accent (per action)             R blood          G gold

const IDLE: [Frame; 2] = [
    [
        "          AA  ",
        "  W    W AAA  ",
        "  WWHHHHWW AA ",
        "   HHHHHH X   ",
        "   HHHHHH X   ",
        "   SOSSOS X   ",
        "   SSSSSS X   ",
        "   BBBBBBSX   ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
    [
        "          AA  ",
        "  W    W AAA  ",
        "  WWHHHHWW AA ",
        "   HHHHHH X   ",
        "   HHHHHH X   ",
        "   SOSSOS X   ",
        "   SSSSSS X   ",
        "   BBBBBBSX   ",
        "   BBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
];

const MARCH: [Frame; 2] = [
    [
        "          AA  ",
        "  W    W AAA  ",
        "  WWHHHHWW AA ",
        "   HHHHHH X   ",
        "   HHHHHH X   ",
        "   SOSSOS X   ",
        "   SSSSSS X   ",
        "   BBBBBBSX   ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT   TT    ",
        "  DDD    DDD  ",
    ],
    [
        "          AA  ",
        "  W    W AAA  ",
        "  WWHHHHWW AA ",
        "   HHHHHH X   ",
        "   HHHHHH X   ",
        "   SOSSOS X   ",
        "   SSSSSS X   ",
        "   BBBBBBSX   ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "    TT TT     ",
        "   DDD DDD    ",
    ],
];

const SWING: [Frame; 4] = [
    // Windup: axe lifted high on the right.
    [
        "         AAA  ",
        "  W    W AAA  ",
        "  WWHHHHWWX   ",
        "   HHHHHH X   ",
        "   HHHHHHSX   ",
        "   SOSSOS     ",
        "   SSSSSS     ",
        "   BBBBBB     ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
    // Overhead: blade above the helmet.
    [
        "     AAA      ",
        "     AAAX     ",
        "  W     X W   ",
        "  WWHHHHXWW   ",
        "   HHHHHS     ",
        "   SOSSOS     ",
        "   SSSSSS     ",
        "   BBBBBB     ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
    // Strike: blade buried low-left, sparks flying.
    [
        "  W      W    ",
        "  WWHHHHWW    ",
        "   HHHHHH     ",
        "   SOSSOS     ",
        "   SSSSSS     ",
        "  SBBBBBB     ",
        "  XBBBBBBB    ",
        " AX PBBPTT    ",
        "AAX PTTPTT    ",
        "MAA LLLLLL    ",
        " M  TT  TT    ",
        "M  DDD  DDD   ",
        "              ",
        "              ",
    ],
    // Follow-through: sparks everywhere.
    [
        "  W      W    ",
        "  WWHHHHWW    ",
        "   HHHHHH     ",
        "   SOSSOS     ",
        "   SSSSSS     ",
        "  SBBBBBB M   ",
        "  XBBBBBBB    ",
        " AX PBBPTT M  ",
        "AAX PTTPTT    ",
        " AAMLLLLLL    ",
        "M M TT  TT    ",
        " M DDD  DDD   ",
        "  M           ",
        "              ",
    ],
];

const CAST: [Frame; 2] = [
    // Orb held high in the left hand, axe grounded at the right.
    [
        " MM       AA  ",
        "MMMM     AAA  ",
        " MM  HHH  X   ",
        "  S HHHHH X   ",
        "  S HHHHH X   ",
        "  S SOSOS X   ",
        "   SSSSSS X   ",
        "   BBBBBBSX   ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
    // The orb pulses.
    [
        "  M       AA  ",
        " MMM     AAA  ",
        "  M  HHH  X   ",
        "  S HHHHH X   ",
        "  S HHHHH X   ",
        "  S SOSOS X   ",
        "   SSSSSS X   ",
        "   BBBBBBSX   ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
];

const WOUND: [Frame; 2] = [
    // Staggering, blood flecks, axe drooping.
    [
        "   W      W   ",
        "   WWHHHHWW   ",
        "    HHHHHH  R ",
        "    OSSSOS    ",
        "    SSSSSS R  ",
        "   RBBBBBB    ",
        "    BBBBBBB   ",
        "   TTPBBPTTR  ",
        "   TTPTTPTT   ",
        "  R LLLLLL    ",
        "    TT  TT X  ",
        "   DDD  DDDXA ",
        "           AA ",
        "              ",
    ],
    [
        "  W      W    ",
        "  WWHHHHWW    ",
        "   HHHHHH R   ",
        "   OSSSOS     ",
        "  RSSSSSS     ",
        "   BBBBBBR    ",
        "   BBBBBBB    ",
        "  TTPBBPTT    ",
        " RTTPTTPTT    ",
        "   LLLLLL R   ",
        "   TT  TT  X  ",
        "  DDD  DDD XA ",
        "            A ",
        "              ",
    ],
];

const VICTOR: [Frame; 2] = [
    // Axe thrust skyward, crowned in gold.
    [
        "          AAA ",
        "   GGGG   AAA ",
        "  WGGGGW   X  ",
        "  WWHHHHWW X  ",
        "   HHHHHH SX  ",
        "   SOSSOS S   ",
        "   SSSSSS     ",
        "   BBBBBB     ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
    [
        "     G    AAA ",
        "   GGGG   AAA ",
        "  WGGGGW   X  ",
        "  WWHHHHWW X  ",
        "   HHHHHH SX  ",
        "   SOSSOS S   ",
        "   SSSSSS     ",
        "   BBBBBB     ",
        "  BBBBBBBB    ",
        "  TTPBBPTT    ",
        "  TTPTTPTT    ",
        "   LLLLLL     ",
        "   TT  TT     ",
        "  DDD  DDD    ",
    ],
];

const SLAIN: [Frame; 1] = [[
    "              ",
    "              ",
    "              ",
    "              ",
    "              ",
    "              ",
    "              ",
    "        X     ",
    "       XA     ",
    "      XAA     ",
    "              ",
    " WHHOSSBBTTDD ",
    " WHHSSSBBTTDD ",
    "  RR   RR     ",
]];

/// The frames for one animation.
pub fn frames(kind: SpriteKind) -> &'static [Frame] {
    match kind {
        SpriteKind::Idle => &IDLE,
        SpriteKind::March => &MARCH,
        SpriteKind::Swing => &SWING,
        SpriteKind::Cast => &CAST,
        SpriteKind::Wound => &WOUND,
        SpriteKind::Victor => &VICTOR,
        SpriteKind::Slain => &SLAIN,
    }
}

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
        let lines = render(&IDLE[0], Color::Rgb(1, 2, 3), Color::Yellow);
        let uses_hero = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Rgb(1, 2, 3)))
        });
        assert!(uses_hero, "beard/trim must carry the fighter color");
    }
}
