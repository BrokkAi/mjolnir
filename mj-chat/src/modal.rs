//! Geometry and background clearing every modal shares: where a dialog sits
//! on screen and how much empty space it keeps around itself.
//!
//! The dashboard and the chat view both draw dialogs, so the rule lives here,
//! in the crate they both depend on, rather than once per view. The rendering
//! helpers clear the same margin that the geometry reserves.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear};

use crate::components::Form;
use crate::selection::{FrameSurfaces, SurfaceFrame, SurfaceId};
use crate::theme;

/// Blank cells kept outside every modal border, on every side.
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

/// Builds the shared upper-left modal title and registers its mouse target.
///
/// The leading ` × ` occupies the three title cells immediately inside the
/// popup's left border. It is deliberately not a form control, so it never
/// enters keyboard focus traversal; Escape remains the keyboard equivalent.
pub fn dismissible_modal_title<K: Copy + Eq>(
    form: &mut Form<K>,
    popup: Rect,
    title: impl Into<String>,
    title_style: Style,
    enabled: bool,
) -> Line<'static> {
    let hitbox = Rect::new(
        popup.x.saturating_add(1),
        popup.y,
        popup.width.saturating_sub(2).min(3),
        popup.height.min(1),
    );
    form.register_dismiss(hitbox, enabled);
    let dismiss_style = if !enabled {
        theme::muted().bg(theme::palette().surface_raised)
    } else if form.dismiss_is_armed() {
        theme::selection(true)
    } else {
        theme::selection(false)
    };
    Line::from(vec![
        Span::styled(" × ", dismiss_style),
        Span::styled(title.into(), title_style),
        Span::styled(" ", title_style),
    ])
}

/// Clears a modal and the empty cells it keeps around its border.
///
/// `bounds` is the area the modal overlays. Clipping the expanded rectangle to
/// it keeps a pane-local modal from erasing a neighboring pane and keeps tiny
/// terminal coordinates valid.
fn clear_modal(frame: &mut Frame, popup: Rect, bounds: Rect) {
    let area = modal_clear_area(popup, bounds);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(crate::theme::base()), area);
}

fn modal_clear_area(popup: Rect, bounds: Rect) -> Rect {
    popup
        .outer(Margin {
            vertical: MODAL_SCREEN_MARGIN,
            horizontal: MODAL_SCREEN_MARGIN,
        })
        .intersection(bounds)
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

/// Centers a percentage-width modal and clears its exterior halo.
pub(crate) fn centered_modal_rect(
    frame: &mut Frame,
    width_percent: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_rect(width_percent, height, area);
    clear_modal(frame, popup, area);
    popup
}

/// Centers a fixed-size modal and clears its exterior halo.
pub(crate) fn centered_modal_rect_fixed(
    frame: &mut Frame,
    width: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_rect_fixed(width, height, area);
    clear_modal(frame, popup, area);
    popup
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

/// Centers and clears a modal popup, then registers its body as selectable.
///
/// Modals draw over the view beneath them, and the registry is z-ordered by
/// render order, so registering here makes the body win the cells it covers.
pub fn centered_modal(
    frame: &mut Frame,
    surfaces: &mut FrameSurfaces,
    width_percent: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_modal_rect(frame, width_percent, height, area);
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        bordered_content(popup),
    ));
    popup
}

/// Centers and clears a modal of an absolute cell width, then registers its
/// body as selectable. Use when the content has a natural width — a QR code, a
/// fixed table — that should hug its content instead of scaling with the
/// terminal. `width` and `height` include the border and are clamped to `area`.
pub fn centered_modal_fixed(
    frame: &mut Frame,
    surfaces: &mut FrameSurfaces,
    width: u16,
    height: u16,
    area: Rect,
) -> Rect {
    let popup = centered_modal_rect_fixed(frame, width, height, area);
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        bordered_content(popup),
    ));
    popup
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Form;
    use crossterm::event::Event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::style::Modifier;
    use ratatui::style::{Color, Style};
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;

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

    #[test]
    fn clearing_a_modal_blanks_its_two_cell_halo_and_nothing_beyond_it() {
        const WIDTH: u16 = 30;
        const HEIGHT: u16 = 15;
        let bounds = Rect::new(3, 2, 20, 10);
        let popup = Rect::new(8, 5, 6, 3);
        let expected_clear = Rect::new(6, 3, 10, 7);
        assert_eq!(modal_clear_area(popup, bounds), expected_clear);

        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
        terminal
            .draw(|frame| {
                let rows = (0..HEIGHT)
                    .map(|_| Line::raw("X".repeat(usize::from(WIDTH))))
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(rows).style(Style::default().fg(Color::Cyan).bg(Color::Blue)),
                    frame.area(),
                );
                clear_modal(frame, popup, bounds);
            })
            .expect("draw modal clear");

        let buffer = terminal.backend().buffer();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let cell = &buffer[(x, y)];
                if expected_clear.contains(Position::new(x, y)) {
                    assert_eq!(cell.symbol(), " ", "cell ({x}, {y}) was not blank");
                    assert_eq!(
                        cell.fg,
                        crate::theme::palette().text,
                        "cell ({x}, {y}) kept its foreground"
                    );
                    assert_eq!(
                        cell.bg,
                        crate::theme::palette().background,
                        "cell ({x}, {y}) kept its background"
                    );
                } else {
                    assert_eq!(
                        cell.symbol(),
                        "X",
                        "cell ({x}, {y}) outside the halo changed"
                    );
                }
            }
        }
    }

    #[test]
    fn dismissible_title_registers_the_three_cell_border_target_and_preserves_title_style() {
        let popup = Rect::new(10, 6, 30, 12);
        let title_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC);
        let mut form = Form::<u8>::new();
        let title = dismissible_modal_title(&mut form, popup, "Settings", title_style, true);

        assert_eq!(title.spans.len(), 3);
        assert_eq!(title.spans[0].content, " × ");
        assert_eq!(title.spans[1].content, "Settings");
        assert_eq!(title.spans[1].style, title_style);
        assert_eq!(title.spans[2].content, " ");
        assert_eq!(title.spans[2].style, title_style);
        assert!(form.contains(popup.x + 1, popup.y));
        assert!(form.contains(popup.x + 3, popup.y));
        assert!(!form.contains(popup.x, popup.y));
        assert!(!form.contains(popup.x + 4, popup.y));
    }

    #[test]
    fn dismissible_title_reports_available_armed_and_disabled_glyph_styles() {
        let popup = Rect::new(10, 6, 30, 12);
        let mut form = Form::<u8>::new();
        let available = dismissible_modal_title(&mut form, popup, "Title", Style::default(), true);
        assert_eq!(available.spans[0].style, theme::selection(false));

        let mouse = |kind, column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        form.handle(&mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            popup.x + 2,
            popup.y,
        ));
        let armed = dismissible_modal_title(&mut form, popup, "Title", Style::default(), true);
        assert_eq!(armed.spans[0].style, theme::selection(true));

        let disabled = dismissible_modal_title(&mut form, popup, "Title", Style::default(), false);
        assert_eq!(
            disabled.spans[0].style,
            theme::muted().bg(theme::palette().surface_raised)
        );
    }
}
