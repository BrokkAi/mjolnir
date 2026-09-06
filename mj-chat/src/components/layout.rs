//! Layout helpers for forms and dialogs.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::hel_modal::{bordered_content, centered_rect, modal_area};

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
