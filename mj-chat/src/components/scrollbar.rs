//! Shared slim terminal scrollbars.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::theme;

/// The dim one-cell rail used by every terminal scrollbar.
pub const TRACK_SYMBOL: &str = "│";
/// The accent one-cell thumb used by every terminal scrollbar.
pub const THUMB_SYMBOL: &str = "▐";

/// Geometry for a slim vertical scrollbar.
///
/// `max_scroll` is the greatest content offset represented by the thumb. It is
/// kept with the geometry so pointer dragging and rendering use the same
/// endpoint mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub track: Rect,
    pub thumb: Rect,
    pub max_scroll: usize,
}

/// Result of offering a pointer event to a scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPointer {
    Ignored,
    Consumed,
    ScrollTo(usize),
}

/// Mouse state shared by every draggable vertical scrollbar. The caller owns
/// the content offset; this only maps track clicks and held-thumb motion to it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarDrag {
    geometry: Option<ScrollbarGeometry>,
    dragging: bool,
    grab_offset: u16,
}

impl ScrollbarDrag {
    pub fn geometry(self) -> Option<ScrollbarGeometry> {
        self.geometry
    }
    pub fn is_dragging(self) -> bool {
        self.dragging
    }
    pub fn grab_offset(self) -> u16 {
        self.grab_offset
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn set_geometry(&mut self, geometry: Option<ScrollbarGeometry>) {
        if self.dragging && self.geometry.map(|old| old.track) != geometry.map(|new| new.track) {
            self.dragging = false;
        }
        self.geometry = geometry;
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> ScrollbarPointer {
        if self.dragging {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => return self.seek(mouse.row),
                MouseEventKind::Up(MouseButton::Left) => {
                    self.dragging = false;
                    return ScrollbarPointer::Consumed;
                }
                MouseEventKind::Down(_) => self.dragging = false,
                _ => return ScrollbarPointer::Consumed,
            }
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return ScrollbarPointer::Ignored;
        }
        let Some(geometry) = self.geometry else {
            return ScrollbarPointer::Ignored;
        };
        if geometry.max_scroll == 0
            || mouse.column != geometry.track.x
            || mouse.row < geometry.track.y
            || mouse.row >= geometry.track.bottom()
        {
            return ScrollbarPointer::Ignored;
        }
        let on_thumb = mouse.row >= geometry.thumb.y && mouse.row < geometry.thumb.bottom();
        self.grab_offset = if on_thumb {
            mouse.row - geometry.thumb.y
        } else {
            geometry.thumb.height / 2
        };
        self.dragging = true;
        if on_thumb {
            ScrollbarPointer::Consumed
        } else {
            self.seek(mouse.row)
        }
    }

    fn seek(self, pointer_row: u16) -> ScrollbarPointer {
        let Some(geometry) = self.geometry else {
            return ScrollbarPointer::Consumed;
        };
        let travel = usize::from(geometry.track.height.saturating_sub(geometry.thumb.height));
        let thumb_top = usize::from(pointer_row.saturating_sub(geometry.track.y))
            .saturating_sub(usize::from(self.grab_offset))
            .min(travel);
        let target = if travel == 0 {
            0
        } else {
            let numerator = u64::try_from(thumb_top)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::try_from(geometry.max_scroll).unwrap_or(u64::MAX));
            usize::try_from(
                numerator.saturating_add(u64::try_from(travel / 2).unwrap_or(0))
                    / u64::try_from(travel).unwrap_or(1),
            )
            .unwrap_or(geometry.max_scroll)
            .min(geometry.max_scroll)
        };
        ScrollbarPointer::ScrollTo(target)
    }
}

/// Calculate scrollbar geometry using a proportional thumb and inclusive
/// endpoints. A zero-height or zero-width track has no drawable geometry.
/// Content shorter than the viewport produces a full-track thumb so callers
/// that need a hidden scrollbar can apply their existing overflow check.
pub fn scrollbar_geometry(
    track: Rect,
    content_length: usize,
    position: usize,
    viewport_content_length: usize,
) -> Option<ScrollbarGeometry> {
    if track.width == 0 || track.height == 0 {
        return None;
    }

    let viewport_content_length = viewport_content_length.max(1);
    let total_length = content_length.max(viewport_content_length);
    let max_scroll = total_length.saturating_sub(viewport_content_length);
    let scroll = position.min(max_scroll);
    let track_height = usize::from(track.height);
    let thumb_height = if max_scroll == 0 {
        track_height
    } else {
        let proportional = (u64::from(track.height)
            .saturating_mul(u64::try_from(viewport_content_length).unwrap_or(u64::MAX))
            / u64::try_from(total_length).unwrap_or(u64::MAX))
        .max(1);
        usize::try_from(proportional)
            .unwrap_or(track_height)
            .min(track_height)
    };
    let travel = track_height.saturating_sub(thumb_height);
    let thumb_offset = if max_scroll == 0 {
        0
    } else {
        (u64::try_from(travel)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(scroll).unwrap_or(u64::MAX))
            / u64::try_from(max_scroll).unwrap_or(u64::MAX)) as usize
    };

    Some(ScrollbarGeometry {
        track,
        thumb: Rect::new(
            track.x,
            track
                .y
                .saturating_add(u16::try_from(thumb_offset).unwrap_or(u16::MAX)),
            track.width,
            u16::try_from(thumb_height).unwrap_or(track.height),
        ),
        max_scroll,
    })
}

/// Paint a scrollbar from previously calculated geometry.
pub fn render_scrollbar(frame: &mut Frame, geometry: ScrollbarGeometry) {
    for row in geometry.track.y..geometry.track.bottom() {
        let is_thumb = row >= geometry.thumb.y && row < geometry.thumb.bottom();
        frame.buffer_mut()[(geometry.track.x, row)]
            .set_symbol(if is_thumb {
                theme::glyphs().scroll_thumb
            } else {
                theme::glyphs().scroll_track
            })
            .set_style(Style::default().fg(if is_thumb {
                theme::palette().muted
            } else {
                theme::palette().border
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn track() -> Rect {
        Rect::new(7, 11, 1, 20)
    }

    #[test]
    fn zero_sized_tracks_have_no_geometry() {
        assert_eq!(scrollbar_geometry(Rect::new(0, 0, 0, 20), 100, 0, 10), None);
        assert_eq!(scrollbar_geometry(Rect::new(0, 0, 1, 0), 100, 0, 10), None);
    }

    #[test]
    fn full_viewport_uses_the_whole_track() {
        let geometry = scrollbar_geometry(track(), 10, 0, 10).expect("track geometry");
        assert_eq!(geometry.max_scroll, 0);
        assert_eq!(geometry.thumb, geometry.track);
    }

    #[test]
    fn proportional_thumb_reaches_both_endpoints() {
        let start = scrollbar_geometry(track(), 100, 0, 25).expect("track geometry");
        let end = scrollbar_geometry(track(), 100, 75, 25).expect("track geometry");

        assert_eq!(start.max_scroll, 75);
        assert_eq!(start.thumb.y, start.track.y);
        assert_eq!(start.thumb.height, 5);
        assert_eq!(end.thumb.bottom(), end.track.bottom());
        assert_eq!(end.thumb.height, start.thumb.height);
    }

    #[test]
    fn positions_past_the_end_clamp_to_the_end() {
        let end = scrollbar_geometry(track(), 100, usize::MAX, 25).expect("track geometry");
        assert_eq!(end.thumb.bottom(), end.track.bottom());
    }

    fn mouse(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 7,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn held_thumb_seeks_across_the_track_and_releases_outside_it() {
        let mut drag = ScrollbarDrag::default();
        drag.set_geometry(scrollbar_geometry(track(), 100, 0, 25));
        assert_eq!(
            drag.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 12)),
            ScrollbarPointer::Consumed
        );
        assert!(drag.is_dragging());
        assert_eq!(
            drag.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX)),
            ScrollbarPointer::ScrollTo(75)
        );
        assert_eq!(
            drag.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), u16::MAX)),
            ScrollbarPointer::Consumed
        );
        assert!(!drag.is_dragging());
    }

    #[test]
    fn track_click_seeks_and_a_resized_track_releases_the_drag() {
        let mut drag = ScrollbarDrag::default();
        drag.set_geometry(scrollbar_geometry(track(), 100, 0, 25));
        assert!(matches!(
            drag.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 27)),
            ScrollbarPointer::ScrollTo(_)
        ));
        drag.set_geometry(scrollbar_geometry(Rect::new(8, 11, 1, 20), 100, 0, 25));
        assert!(!drag.is_dragging());
    }
}
