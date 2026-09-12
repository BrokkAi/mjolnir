//! Layout helpers for forms and dialogs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::Clear;

use crate::modal::{bordered_content, centered_rect, modal_area};
use crate::theme;

/// Which side of an anchor an inline popup should prefer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupSide {
    /// Place the popup below the anchor when there is room.
    Below,
    /// Place the popup above the anchor when there is room.
    Above,
}

/// The shared framed shell for compact, anchored choice popups.
pub struct AutocompletePopup;

impl AutocompletePopup {
    /// Draws an anchored popup shell and returns its outer and inner regions.
    ///
    /// The caller owns the rows and selection. This helper only chooses a
    /// clipped rectangle, clears it, and draws the modal frame. At most eight
    /// rows are visible, matching chat autocomplete's compact surface.
    #[must_use]
    pub fn render(
        frame: &mut Frame<'_>,
        bounds: Rect,
        anchor: Rect,
        width: u16,
        rows: usize,
        title: &str,
        preferred: PopupSide,
    ) -> Option<(Rect, Rect)> {
        let visible = rows.min(8);
        if visible == 0 || bounds.width == 0 || bounds.height == 0 {
            return None;
        }
        let height = u16::try_from(visible)
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(bounds.height);
        let width = width.max(4).min(bounds.width);
        let x = anchor
            .x
            .min(bounds.right().saturating_sub(width))
            .max(bounds.x);
        let below = anchor.bottom().min(bounds.bottom());
        let above = anchor.y.saturating_sub(height);
        let fits_below = below.saturating_add(height) <= bounds.bottom();
        let fits_above = anchor.y >= bounds.y.saturating_add(height);
        let y = match preferred {
            PopupSide::Below if fits_below => below,
            PopupSide::Above if fits_above => above,
            _ if fits_below => below,
            _ if fits_above => above,
            _ => bounds
                .y
                .saturating_add(bounds.height.saturating_sub(height) / 2),
        };
        let outer = Rect::new(x, y, width, height);
        frame.render_widget(Clear, outer);
        let block = theme::modal().title(title);
        let inner = block.inner(outer);
        frame.render_widget(block, outer);
        Some((outer, inner))
    }
}

/// Returns a centered dialog rectangle using the shared modal margin rules.
#[must_use]
pub fn dialog_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
    centered_rect(width_percent, height, area)
}

/// Returns the content area inside a dialog border.
#[must_use]
pub fn dialog_content(area: Rect) -> Rect {
    bordered_content(area)
}

/// Insets a form region by the shared modal screen margin.
#[must_use]
pub fn form_area(area: Rect) -> Rect {
    modal_area(area)
}

/// Splits a region into horizontal form rows.
#[must_use]
pub fn form_rows(area: Rect, rows: &[u16]) -> Vec<Rect> {
    let constraints = rows
        .iter()
        .copied()
        .map(Constraint::Length)
        .collect::<Vec<_>>();
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

/// Splits a region into equal-width columns.
#[must_use]
pub fn form_columns(area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, count as u32); count])
        .split(area)
        .to_vec()
}

/// A vertically clipped form body which reveals the focused row.
/// Keep its offset in the screen draft between frames; fixed footer buttons stay outside it.
#[derive(Debug, Clone, Copy)]
pub struct FormViewport {
    area: Rect,
    offset: u16,
}

impl FormViewport {
    /// Computes the scroll position for the current body height and focused content row.
    #[must_use]
    pub fn new(area: Rect, content_height: u16, previous: u16, focused_row: Option<u16>) -> Self {
        let mut offset = previous.min(content_height.saturating_sub(area.height));
        if let Some(row) = focused_row {
            if row < offset {
                offset = row;
            }
            if area.height > 0 && row >= offset.saturating_add(area.height) {
                offset = row.saturating_add(1).saturating_sub(area.height);
            }
        }
        Self { area, offset }
    }

    /// The offset to retain for the next frame.
    #[must_use]
    pub fn offset(self) -> u16 {
        self.offset
    }

    /// Clips a content row or list to this frame's visible body.
    #[must_use]
    pub fn row(self, start: u16, height: u16) -> Rect {
        let top = start.max(self.offset);
        let bottom = start
            .saturating_add(height)
            .min(self.offset.saturating_add(self.area.height));
        if top >= bottom {
            return Rect::default();
        }
        Rect::new(
            self.area.x,
            self.area.y.saturating_add(top - self.offset),
            self.area.width,
            bottom - top,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn anchored_popup_prefers_below_and_falls_back_above_with_clipping() {
        let bounds = Rect::new(2, 2, 30, 16);
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                let below = AutocompletePopup::render(
                    frame,
                    bounds,
                    Rect::new(8, 4, 1, 1),
                    14,
                    2,
                    " choices ",
                    PopupSide::Below,
                )
                .expect("below popup");
                assert_eq!(below.0.y, 5);
                assert_eq!(below.0.height, 4);

                let above = AutocompletePopup::render(
                    frame,
                    bounds,
                    Rect::new(8, 15, 1, 1),
                    40,
                    20,
                    " choices ",
                    PopupSide::Below,
                )
                .expect("clipped popup");
                assert!(above.0.y >= bounds.y);
                assert!(above.0.bottom() <= bounds.bottom());
                assert_eq!(above.0.height, 10);
                assert!(above.0.x >= bounds.x);
                assert!(above.0.right() <= bounds.right());
            })
            .expect("draw popup");
    }
}
