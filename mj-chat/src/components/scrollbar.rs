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

/// Clamp a list offset so the viewport never opens on a hidden head.
///
/// The largest useful offset is the first row from which every remaining row
/// fits (the last page). A larger offset leaves blank space below the content
/// while earlier rows stay hidden. For unit-height rows this is
/// `max(0, len - viewport)`, and a list that fits has offset 0. Row heights are
/// in terminal rows.
#[must_use]
pub fn clamp_offset_to_last_page(
    offset: usize,
    row_heights: &[usize],
    viewport_height: usize,
) -> usize {
    let mut first = row_heights.len();
    let mut used = 0usize;
    while first > 0 {
        used = used.saturating_add(row_heights[first - 1]);
        if used > viewport_height {
            break;
        }
        first -= 1;
    }
    offset.min(first)
}

/// The offset that centres `selected` in the viewport, pinned to the list ends.
///
/// Near the top the list starts at its first row, near the bottom it ends at
/// its last row, and a list that fits never scrolls. With variable row heights
/// the selected row is centred as closely as whole rows allow.
#[must_use]
pub fn centered_offset(selected: usize, row_heights: &[usize], viewport_height: usize) -> usize {
    if selected >= row_heights.len() {
        return clamp_offset_to_last_page(usize::MAX, row_heights, viewport_height);
    }
    let room_above = viewport_height.saturating_sub(row_heights[selected]) / 2;
    let mut offset = selected;
    let mut above = 0usize;
    while offset > 0 && above.saturating_add(row_heights[offset - 1]) <= room_above {
        offset -= 1;
        above += row_heights[offset];
    }
    clamp_offset_to_last_page(offset, row_heights, viewport_height)
}

/// [`centered_offset`] for `len` rows that are one terminal row each.
#[must_use]
pub fn centered_unit_offset(selected: usize, len: usize, viewport_height: usize) -> usize {
    let max = len.saturating_sub(viewport_height);
    selected.saturating_sub(viewport_height / 2).min(max)
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
    use crate::components::{ChoiceList, Form};
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;
    use std::fmt::Write as _;

    fn track() -> Rect {
        Rect::new(7, 11, 1, 20)
    }

    #[test]
    fn offset_never_hides_rows_that_fit_the_viewport() {
        let clamp = clamp_offset_to_last_page;
        // Content shrank below the viewport: show it all from the top.
        assert_eq!(clamp(7, &[1; 3], 10), 0);
        // The viewport grew past the content.
        assert_eq!(clamp(2, &[1; 6], 6), 0);
        // Exact fit.
        assert_eq!(clamp(1, &[1; 5], 5), 0);
        // One more row than fits: only the last page start is allowed.
        assert_eq!(clamp(4, &[1; 6], 5), 1);
        assert_eq!(clamp(1, &[1; 6], 5), 1);
        assert_eq!(clamp(0, &[1; 6], 5), 0);
        // Variable heights: the last page starts where the tail first fits.
        assert_eq!(clamp(9, &[3, 3, 2, 2], 5), 2);
        assert_eq!(clamp(1, &[], 5), 0);
    }

    fn centered_view(heights: &[usize], selected: usize, viewport: u16) -> (usize, String) {
        let offset = centered_offset(selected, heights, usize::from(viewport));
        let mut lines = Vec::new();
        for (index, height) in heights.iter().enumerate().skip(offset) {
            for row in 0..*height {
                if lines.len() == usize::from(viewport) {
                    break;
                }
                let marker = if index == selected && row == 0 {
                    ">"
                } else {
                    " "
                };
                lines.push(Line::raw(format!("{marker}{index:02} {}", "row".repeat(2))));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(12, viewport)).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
            .expect("render scrolling list");
        (
            offset,
            crate::golden::buffer_lines(terminal.backend().buffer()).join("\n"),
        )
    }

    fn append_centered_state(
        output: &mut String,
        label: &str,
        heights: &[usize],
        selected: usize,
        viewport: u16,
    ) {
        let (offset, surface) = centered_view(heights, selected, viewport);
        writeln!(output, "=== {label} (12x{viewport}) ===").expect("write heading");
        writeln!(output, "offset: {offset}").expect("write offset");
        output.push_str(&surface);
        output.push('\n');
    }

    #[test]
    fn golden_centered_list_viewport() {
        let mut output = String::new();
        let heights = [1; 40];
        for selected in 0..40 {
            append_centered_state(
                &mut output,
                &format!("unit rows selected {selected}"),
                &heights,
                selected,
                9,
            );
            writeln!(
                output,
                "unit offset: {}",
                centered_unit_offset(selected, 40, 9)
            )
            .expect("write unit offset");
        }
        let fitting = [1; 6];
        for selected in 0..6 {
            append_centered_state(
                &mut output,
                &format!("fit viewport 6 selected {selected}"),
                &fitting,
                selected,
                6,
            );
            append_centered_state(
                &mut output,
                &format!("fit viewport 20 selected {selected}"),
                &fitting,
                selected,
                20,
            );
            let list = [
                Line::raw("Alpha"),
                Line::raw("Beta"),
                Line::raw("Gamma"),
                Line::raw("Delta"),
                Line::raw("Epsilon"),
                Line::raw("Zeta"),
            ];
            let mut terminal = Terminal::new(TestBackend::new(12, 6)).expect("terminal");
            let mut form = Form::<u8>::new();
            form.declare(
                1,
                crate::components::ControlKind::ChoiceList { len: 6, selected },
            );
            form.end_frame(1);
            terminal
                .draw(|frame| {
                    form.begin_frame();
                    ChoiceList::render(frame, frame.area(), &list, selected, &mut form, 1);
                    form.end_frame(1);
                })
                .expect("render choice list");
            let buffer = terminal.backend().buffer();
            let selected_style = (0..buffer.area.width)
                .map(|x| {
                    let cell = &buffer[(x, selected as u16)];
                    format!(
                        "{}:{:?}/{:?}/{:?}",
                        cell.symbol(),
                        cell.fg,
                        cell.bg,
                        cell.modifier
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(
                output,
                "unit offsets at viewport 6/20: {}/{}; unit offset viewport 6: {}; actual ChoiceList selected {selected}:\n{}\nselected row style: {selected_style}",
                centered_offset(selected, &fitting, 6),
                centered_offset(selected, &fitting, 20),
                centered_unit_offset(selected, 6, 6),
                crate::golden::buffer_lines(buffer).join("\n")
            )
            .expect("write fit list rendering");
        }
        let mixed_fit = [2, 3, 2];
        append_centered_state(&mut output, "mixed rows fit", &mixed_fit, 2, 7);
        let mixed = [2, 3, 4, 2, 3, 2, 4, 3];
        for selected in 0..mixed.len() {
            append_centered_state(
                &mut output,
                &format!("mixed heights selected {selected}"),
                &mixed,
                selected,
                9,
            );
        }
        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "centered-list-viewport",
            &output,
        );
    }

    #[test]
    fn zero_sized_tracks_have_no_geometry() {
        assert_eq!(scrollbar_geometry(Rect::new(0, 0, 0, 20), 100, 0, 10), None);
        assert_eq!(scrollbar_geometry(Rect::new(0, 0, 1, 0), 100, 0, 10), None);
    }

    fn rendered_geometry(label: &str, content: usize, position: usize, viewport: usize) -> String {
        let track = Rect::new(1, 0, 1, 20);
        let geometry = scrollbar_geometry(track, content, position, viewport).expect("geometry");
        let mut terminal = Terminal::new(TestBackend::new(3, 20)).expect("terminal");
        terminal
            .draw(|frame| render_scrollbar(frame, geometry))
            .expect("render scrollbar");
        format!(
            "=== {label} (3x20) ===\ngeometry: max={} thumb={:?}\n{}\n",
            geometry.max_scroll,
            geometry.thumb,
            crate::golden::buffer_lines(terminal.backend().buffer()).join("\n")
        )
    }

    #[test]
    fn golden_scrollbar_geometry() {
        let mut output = String::new();
        output.push_str(&rendered_geometry("full viewport", 10, 0, 10));
        output.push_str(&rendered_geometry(
            "proportional thumb at start",
            100,
            0,
            25,
        ));
        output.push_str(&rendered_geometry("proportional thumb at end", 100, 75, 25));
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "scrollbar-geometry", &output);
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
