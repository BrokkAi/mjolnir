//! Geometry every modal shares: where a dialog sits on screen and how much
//! empty space it keeps around itself.
//!
//! The dashboard and the chat view both draw dialogs, so the rule lives here,
//! in the crate they both depend on, rather than once per view. Centering is
//! the only way to obtain a modal rectangle, and centering applies
//! [`MODAL_SCREEN_MARGIN`], so a new dialog cannot forget the margin.

use ratatui::layout::{Margin, Rect};

use crate::hel_selection::{FrameSurfaces, SurfaceFrame, SurfaceId};

/// Empty cells kept between any modal and the edge of the area it is centered
/// in, on every side.
pub const MODAL_SCREEN_MARGIN: u16 = 2;

/// The region a modal may occupy: `area` inset by [`MODAL_SCREEN_MARGIN`] on
/// every side, so dialogs never butt against the terminal border. On a terminal
/// too small to hold the margin it degrades to the full area rather than vanish.
pub fn modal_area(area: Rect) -> Rect {
    let margin = MODAL_SCREEN_MARGIN;
    if area.width > margin * 2 && area.height > margin * 2 {
        Rect::new(
            area.x + margin,
            area.y + margin,
            area.width - margin * 2,
            area.height - margin * 2,
        )
    } else {
        area
    }
}

/// The drawn text inside a full border, which is the part of a widget a
/// selection may cover.
pub fn bordered_content(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    })
}

/// Centers a rectangle whose width is a percentage of `area` and whose height
/// is an absolute row count, keeping the [`MODAL_SCREEN_MARGIN`] floor.
pub fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    place(percent_of(area.width, width_percent), height, area)
}

/// Centers a rectangle sized as a percentage of `area` in both directions,
/// keeping the [`MODAL_SCREEN_MARGIN`] floor. Use when the content has no
/// natural height and should scale with the terminal.
pub fn centered_rect_percent(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    place(
        percent_of(area.width, width_percent),
        percent_of(area.height, height_percent),
        area,
    )
}

/// Centers a rectangle of an absolute cell size, keeping the
/// [`MODAL_SCREEN_MARGIN`] floor. Use when the content has a natural size — a QR
/// code, a fixed table — that should hug its content instead of scaling.
pub fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    place(width, height, area)
}

/// Centers a modal of the requested size in `area`, shrinking it only as far as
/// the margin requires.
///
/// The size is asked for against the whole `area`, not against the inset one,
/// so the margin is a floor rather than a second inset. A dialog that already
/// leaves more than [`MODAL_SCREEN_MARGIN`] free — most percentage-sized ones
/// do — keeps the size its caller chose.
fn place(width: u16, height: u16, area: Rect) -> Rect {
    let bounds = modal_area(area);
    let width = width.min(bounds.width);
    let height = height.min(bounds.height);
    Rect::new(
        bounds.x + bounds.width.saturating_sub(width) / 2,
        bounds.y + bounds.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn percent_of(cells: u16, percent: u16) -> u16 {
    u16::try_from(u32::from(cells) * u32::from(percent) / 100).unwrap_or(u16::MAX)
}

/// Centers a modal popup and registers its body as a selectable surface.
///
/// Modals draw over the view beneath them, and the registry is z-ordered by
/// render order, so registering here makes the body win the cells it covers.
pub fn centered_modal(
    surfaces: &mut FrameSurfaces,
    width_percent: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_rect(width_percent, height, area);
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        bordered_content(popup),
    ));
    popup
}

/// Centers a modal of an absolute cell width and registers its body as a
/// selectable surface. Use when the content has a natural width — a QR code, a
/// fixed table — that should hug its content instead of scaling with the
/// terminal. `width` and `height` include the border and are clamped to `area`.
pub fn centered_modal_fixed(
    surfaces: &mut FrameSurfaces,
    width: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_rect_fixed(width, height, area);
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        bordered_content(popup),
    ));
    popup
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: 130,
        height: 40,
    };

    fn assert_keeps_margin(popup: Rect, area: Rect) {
        let margin = MODAL_SCREEN_MARGIN;
        assert!(
            popup.x >= area.x + margin
                && popup.y >= area.y + margin
                && popup.right() + margin <= area.right()
                && popup.bottom() + margin <= area.bottom(),
            "{popup:?} does not keep {margin} cells inside {area:?}"
        );
    }

    #[test]
    fn every_centering_helper_keeps_the_screen_margin() {
        assert_keeps_margin(centered_rect(82, 30, SCREEN), SCREEN);
        assert_keeps_margin(centered_rect_percent(82, 78, SCREEN), SCREEN);
        assert_keeps_margin(centered_rect_fixed(72, 14, SCREEN), SCREEN);
    }

    #[test]
    fn an_oversized_modal_is_clamped_to_the_margin_rather_than_overflowing() {
        assert_keeps_margin(centered_rect(100, 400, SCREEN), SCREEN);
        assert_keeps_margin(centered_rect_percent(100, 100, SCREEN), SCREEN);
        assert_keeps_margin(centered_rect_fixed(400, 400, SCREEN), SCREEN);
    }

    #[test]
    fn centering_offsets_by_the_origin_of_the_area_it_is_given() {
        let pane = Rect::new(20, 5, 60, 20);
        assert_keeps_margin(centered_rect(80, 10, pane), pane);
        assert_keeps_margin(centered_rect_percent(80, 50, pane), pane);
        assert_keeps_margin(centered_rect_fixed(30, 10, pane), pane);
    }

    #[test]
    fn a_terminal_too_small_for_the_margin_still_yields_a_visible_modal() {
        let tiny = Rect::new(0, 0, 4, 3);
        for popup in [
            centered_rect(100, 3, tiny),
            centered_rect_percent(100, 100, tiny),
            centered_rect_fixed(4, 3, tiny),
        ] {
            assert!(popup.width > 0 && popup.height > 0, "{popup:?} vanished");
        }
    }
}
