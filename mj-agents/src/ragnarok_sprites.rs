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

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: &str = " HWSOBPTLDXAMRG";

    fn all_frames() -> [(SpriteKind, &'static [Frame]); 7] {
        [
            (SpriteKind::Idle, frames(SpriteKind::Idle)),
            (SpriteKind::March, frames(SpriteKind::March)),
            (SpriteKind::Swing, frames(SpriteKind::Swing)),
            (SpriteKind::Cast, frames(SpriteKind::Cast)),
            (SpriteKind::Wound, frames(SpriteKind::Wound)),
            (SpriteKind::Victor, frames(SpriteKind::Victor)),
            (SpriteKind::Slain, frames(SpriteKind::Slain)),
        ]
    }

    #[test]
    fn every_frame_is_exactly_sprite_sized() {
        for (kind, set) in all_frames() {
            assert!(!set.is_empty(), "{kind:?} has no frames");
            for frame in set {
                assert_eq!(frame.len(), SPRITE_H);
                assert!(frame.iter().all(|row| row.chars().count() == SPRITE_W));
            }
        }
    }

    #[test]
    fn every_pixel_is_a_known_palette_key() {
        for (_, set) in all_frames() {
            for frame in set {
                assert!(
                    frame
                        .iter()
                        .flat_map(|row| row.chars())
                        .all(|key| PALETTE.contains(key))
                );
            }
        }
    }
}
