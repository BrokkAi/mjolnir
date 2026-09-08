//! Shared slim terminal scrollbars.

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
            .set_symbol(if is_thumb { THUMB_SYMBOL } else { TRACK_SYMBOL })
            .set_style(Style::default().fg(if is_thumb {
                theme::ACCENT
            } else {
                theme::BORDER
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
