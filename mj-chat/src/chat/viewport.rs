//! A scroll position over a list of rows, shared by the secondary panes.
//!
//! The primary transcript does not use this: its own `TranscriptViewport`
//! freezes row estimates for the duration of a scrollbar drag, which is a
//! different job.

/// Where a scrollable pane is looking: the first content row it draws, and
/// whether new rows keep it pinned to the end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowViewport {
    /// First content row drawn.
    pub(super) top_row: usize,
    /// Whether new rows keep the pane pinned to the end.
    pub(super) follow: bool,
}

impl RowViewport {
    /// Scrolls by `delta` rows within `total_rows` shown `height` rows at a
    /// time, and reports whether the view moved. A negative `delta` scrolls
    /// towards the start. Following is left on only at the very end of the
    /// content, so any scroll away from the end stops it.
    pub(super) fn scroll_by(&mut self, delta: isize, total_rows: usize, height: usize) -> bool {
        let before = *self;
        let maximum = total_rows.saturating_sub(height);
        let top = if delta.is_negative() {
            self.top_row.saturating_sub(delta.unsigned_abs())
        } else {
            self.top_row.saturating_add(delta as usize)
        };
        self.top_row = top.min(maximum);
        self.follow = self.top_row >= maximum;
        *self != before
    }

    /// Keeps the view inside content that has changed size, and re-derives
    /// whether it is sitting at the end.
    pub(super) fn clamp(&mut self, total_rows: usize, height: usize) {
        let maximum = total_rows.saturating_sub(height);
        self.top_row = self.top_row.min(maximum);
        self.follow = self.top_row >= maximum;
    }
}
