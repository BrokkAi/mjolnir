//! Content-space text selection engine shared by Hel's terminal surfaces.
//!
//! The engine owns one global selection over a registry of selectable surfaces
//! that the render pass rebuilds every frame. Selections are stored in *content*
//! coordinates (a scroll-independent row plus a column inside the surface), so a
//! surface that scrolls or streams new rows under an active drag keeps the same
//! selection.
//!
//! The engine deliberately knows nothing about Hel's views. It maps screen cells
//! to content positions, highlights the visible part of a selection in a
//! [`Buffer`], and reports the selected range on release. Turning a range into
//! text is the caller's job for scrollable surfaces, which own the wrapped-row
//! cache; [`extract_rows`] covers surfaces whose content is fully on screen.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// Identifies a surface that can own a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceId {
    /// Chat transcript rows.
    Transcript,
    /// Reviewer transcript rows, beside the primary's during a second opinion.
    ReviewerTranscript,
    /// Scrollable message body of an elicitation dialog.
    ElicitationMessage,
    /// Prompt editor.
    PromptInput,
    /// Dashboard pane, numbered in render order.
    DashboardPane(u8),
    /// Session list inside the resume dialog.
    ResumeList,
    /// Autocomplete popup rows.
    AutocompletePopup,
    /// Body of a modal dialog or wizard step.
    ModalBody,
}

/// Where a selectable surface sits on screen for one frame.
///
/// `rect` is the content area *inside* any border. `top_row` is the content row
/// currently drawn at `rect.y`, and `total_rows` is how many content rows the
/// surface holds in total. A surface whose content is fully on screen registers
/// with `top_row = 0` and `total_rows = rect.height`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceFrame {
    /// Which surface this frame belongs to.
    pub id: SurfaceId,
    /// Content area inside the surface's border.
    pub rect: Rect,
    /// Content row drawn at `rect.y`.
    pub top_row: usize,
    /// Total content rows the surface holds.
    pub total_rows: usize,
}

impl SurfaceFrame {
    /// Registers a scrollable surface showing `total_rows` rows starting at `top_row`.
    #[must_use]
    pub const fn scrollable(id: SurfaceId, rect: Rect, top_row: usize, total_rows: usize) -> Self {
        Self {
            id,
            rect,
            top_row,
            total_rows,
        }
    }

    /// Registers a surface whose content is entirely on screen.
    #[must_use]
    pub const fn fixed(id: SurfaceId, rect: Rect) -> Self {
        Self {
            id,
            rect,
            top_row: 0,
            total_rows: rect.height as usize,
        }
    }

    /// Maps a screen cell to a content position, clamped to this surface.
    ///
    /// Columns clamp to `rect`'s horizontal span; rows clamp to the visible band
    /// and then to `[0, total_rows)`, so a pointer dragged above or below the
    /// rect lands on its first or last visible row.
    #[must_use]
    pub fn content_pos(&self, column: u16, row: u16) -> ContentPos {
        let last_column = self.rect.width.saturating_sub(1);
        let col = column.saturating_sub(self.rect.x).min(last_column);
        let last_visible = self.rect.height.saturating_sub(1);
        let offset = row.saturating_sub(self.rect.y).min(last_visible);
        let last_row = self.total_rows.saturating_sub(1);
        let content_row = self.top_row.saturating_add(offset as usize).min(last_row);
        ContentPos {
            row: content_row,
            col,
        }
    }

    /// Screen row showing `content_row`, or `None` when it is scrolled out.
    #[must_use]
    pub fn screen_row(&self, content_row: usize) -> Option<u16> {
        let offset = content_row.checked_sub(self.top_row)?;
        if offset >= self.rect.height as usize {
            return None;
        }
        Some(self.rect.y.saturating_add(offset as u16))
    }
}

/// Surfaces registered during the current frame, in render order.
#[derive(Clone, Debug, Default)]
pub struct FrameSurfaces {
    surfaces: Vec<SurfaceFrame>,
}

impl FrameSurfaces {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every registration; call once at the start of each frame.
    pub fn clear(&mut self) {
        self.surfaces.clear();
    }

    /// Registers a surface. Later registrations sit above earlier ones.
    pub fn push(&mut self, surface: SurfaceFrame) {
        self.surfaces.push(surface);
    }

    /// Appends every surface `other` registered, preserving render order.
    ///
    /// One frame can be drawn by more than one renderer; this is how the
    /// second renderer's hitboxes join the first's without either owning the
    /// other's registry.
    pub fn append(&mut self, other: &FrameSurfaces) {
        self.surfaces.extend_from_slice(&other.surfaces);
    }

    /// Replaces every registration with `other`'s.
    ///
    /// A modal owns the frame's interaction, so everything behind it stops
    /// being selectable rather than staying reachable underneath.
    pub fn replace_with(&mut self, other: &FrameSurfaces) {
        self.surfaces.clear();
        self.surfaces.extend_from_slice(&other.surfaces);
    }

    /// Returns true when no surface is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// Topmost surface containing the screen cell, or `None`.
    ///
    /// Render order is z-order, so the last registration wins where surfaces
    /// overlap and an overlay takes the hit from whatever it covers.
    #[must_use]
    pub fn surface_at(&self, column: u16, row: u16) -> Option<&SurfaceFrame> {
        let position = Position::new(column, row);
        self.surfaces
            .iter()
            .rev()
            .find(|surface| surface.rect.contains(position))
    }

    /// Most recent registration for `id`, or `None` when it is not on screen.
    #[must_use]
    pub fn surface(&self, id: SurfaceId) -> Option<&SurfaceFrame> {
        self.surfaces.iter().rev().find(|surface| surface.id == id)
    }
}

/// A position inside a surface's content: a content row and a column measured
/// from `rect.x`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContentPos {
    /// Content row, independent of scrolling.
    pub row: usize,
    /// Column offset from the surface's left edge.
    pub col: u16,
}

impl ContentPos {
    /// Creates a content position.
    #[must_use]
    pub const fn new(row: usize, col: u16) -> Self {
        Self { row, col }
    }
}

/// A normalized selection over one surface, inclusive of both endpoints.
///
/// Semantics are linear (stream-like), not rectangular: a single-row selection
/// covers `start.col..=end.col`; a multi-row selection covers the first row from
/// `start.col` to the surface's right edge, every middle row in full, and the
/// last row from column 0 to `end.col`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    /// Earlier endpoint in row-major order.
    pub start: ContentPos,
    /// Later endpoint in row-major order.
    pub end: ContentPos,
}

impl SelectionRange {
    /// Builds a range from an anchor and a cursor, ordering them row-major.
    #[must_use]
    pub fn new(anchor: ContentPos, cursor: ContentPos) -> Self {
        if cursor < anchor {
            Self {
                start: cursor,
                end: anchor,
            }
        } else {
            Self {
                start: anchor,
                end: cursor,
            }
        }
    }

    /// Inclusive column span selected on `row` for a surface `width` wide.
    ///
    /// Returns `None` when `row` lies outside the range or the surface has no
    /// width.
    #[must_use]
    pub fn columns_on(&self, row: usize, width: u16) -> Option<(u16, u16)> {
        if width == 0 || row < self.start.row || row > self.end.row {
            return None;
        }
        let last = width - 1;
        let first_col = if row == self.start.row {
            self.start.col.min(last)
        } else {
            0
        };
        let last_col = if row == self.end.row {
            self.end.col.min(last)
        } else {
            last
        };
        (first_col <= last_col).then_some((first_col, last_col))
    }
}

/// What the caller should do after a button release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionAction {
    /// Nothing to do.
    None,
    /// The button was released without ever dragging; forward it to the view's
    /// click handling.
    Click {
        /// Screen column of the release.
        column: u16,
        /// Screen row of the release.
        row: u16,
    },
    /// A drag finished. The caller extracts the text for `range` and copies it.
    CopyRequested {
        /// Surface the selection belongs to.
        surface: SurfaceId,
        /// Normalized selected range.
        range: SelectionRange,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    /// No button held and no selection to show.
    #[default]
    Idle,
    /// Button down, no movement yet: still a click candidate.
    Pressed,
    /// Button down and moved: a selection is being dragged.
    Dragging,
    /// Button released after dragging; the highlight persists.
    Completed,
}

/// The single global selection state machine.
///
/// Wheel events never reach the engine; the caller routes them to scrolling.
#[derive(Clone, Debug, Default)]
pub struct SelectionState {
    phase: Phase,
    surface: Option<SurfaceId>,
    anchor: ContentPos,
    cursor: ContentPos,
    press_cell: (u16, u16),
    pointer: (u16, u16),
}

impl SelectionState {
    /// Creates an idle state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops any selection and returns to idle.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Surface owning the in-flight or completed gesture, if any.
    #[must_use]
    pub fn active_surface(&self) -> Option<SurfaceId> {
        match self.phase {
            Phase::Idle => None,
            _ => self.surface,
        }
    }

    /// Normalized range while dragging or after a completed drag.
    #[must_use]
    pub fn range(&self) -> Option<SelectionRange> {
        matches!(self.phase, Phase::Dragging | Phase::Completed)
            .then(|| SelectionRange::new(self.anchor, self.cursor))
    }

    /// Starts a click candidate on the surface under the pointer.
    ///
    /// Any completed selection is dropped. A press that hits no surface leaves
    /// the engine idle.
    pub fn on_mouse_down(&mut self, column: u16, row: u16, surfaces: &FrameSurfaces) {
        self.clear();
        let Some(frame) = surfaces.surface_at(column, row) else {
            return;
        };
        let anchor = frame.content_pos(column, row);
        self.phase = Phase::Pressed;
        self.surface = Some(frame.id);
        self.anchor = anchor;
        self.cursor = anchor;
        self.press_cell = (column, row);
        self.pointer = (column, row);
    }

    /// Extends the selection while the button is held.
    ///
    /// The first movement to a different cell promotes the candidate to a drag.
    /// The cursor is resolved against the surface's *current* frame, so a drag
    /// that runs alongside auto-scrolling tracks the rows now under the pointer.
    pub fn on_mouse_drag(&mut self, column: u16, row: u16, surfaces: &FrameSurfaces) {
        if !matches!(self.phase, Phase::Pressed | Phase::Dragging) {
            return;
        }
        self.track(column, row, surfaces);
    }

    /// Finishes the gesture.
    ///
    /// A release that never moved is reported as a [`SelectionAction::Click`] for
    /// the view's own handlers; a release after movement yields
    /// [`SelectionAction::CopyRequested`] and leaves the highlight in place until
    /// the next press or [`SelectionState::clear`].
    pub fn on_mouse_up(
        &mut self,
        column: u16,
        row: u16,
        surfaces: &FrameSurfaces,
    ) -> SelectionAction {
        if !matches!(self.phase, Phase::Pressed | Phase::Dragging) {
            return SelectionAction::None;
        }
        self.track(column, row, surfaces);
        if self.phase == Phase::Dragging {
            self.phase = Phase::Completed;
            match (self.surface, self.range()) {
                (Some(surface), Some(range)) => SelectionAction::CopyRequested { surface, range },
                _ => SelectionAction::None,
            }
        } else {
            self.clear();
            SelectionAction::Click { column, row }
        }
    }

    /// Re-resolves the held pointer against the current frame, extending the
    /// selection after the caller auto-scrolled the surface under it.
    ///
    /// A stationary pointer at a scroll edge produces no new drag events, so
    /// the auto-scroll tick must call this after re-rendering to pick up the
    /// rows that moved under the pointer.
    pub fn retrack(&mut self, surfaces: &FrameSurfaces) {
        if self.phase == Phase::Dragging {
            self.track(self.pointer.0, self.pointer.1, surfaces);
        }
    }

    /// Direction to auto-scroll the dragged surface, if the pointer sits at or
    /// beyond one of its edges.
    ///
    /// `-1` scrolls up, `+1` scrolls down. Surfaces with nothing scrolled out
    /// never request a scroll.
    #[must_use]
    pub fn autoscroll_request(&self, surfaces: &FrameSurfaces) -> Option<(SurfaceId, i8)> {
        if self.phase != Phase::Dragging {
            return None;
        }
        let id = self.surface?;
        let frame = surfaces.surface(id)?;
        if frame.rect.height == 0 || frame.total_rows <= frame.rect.height as usize {
            return None;
        }
        let row = self.pointer.1;
        if row <= frame.rect.y {
            return Some((id, -1));
        }
        if row >= frame.rect.bottom().saturating_sub(1) {
            return Some((id, 1));
        }
        None
    }

    fn track(&mut self, column: u16, row: u16, surfaces: &FrameSurfaces) {
        self.pointer = (column, row);
        let Some(id) = self.surface else {
            return;
        };
        let Some(frame) = surfaces.surface(id) else {
            return;
        };
        let cursor = frame.content_pos(column, row);
        if self.phase == Phase::Pressed
            && ((column, row) != self.press_cell || cursor != self.anchor)
        {
            self.phase = Phase::Dragging;
        }
        self.cursor = cursor;
    }
}

/// Reverse-videos the visible part of `range` inside `frame`.
///
/// Cells outside `frame.rect` or outside the buffer are left alone, so the pass
/// is safe to run after any widget has drawn.
pub fn highlight(buffer: &mut Buffer, frame: &SurfaceFrame, range: &SelectionRange) {
    for span in visible_spans(frame, range) {
        for x in span.first_x..=span.last_x {
            if let Some(cell) = buffer.cell_mut(Position::new(x, span.screen_y)) {
                cell.modifier |= Modifier::REVERSED;
            }
        }
    }
}

/// Reads the selected text out of the frame buffer.
///
/// This is for surfaces whose content is entirely on screen. Rows are trimmed of
/// trailing whitespace and joined with `\n`. Scrollable surfaces extract from
/// their own row cache instead; the engine only reports the range.
#[must_use]
pub fn extract_rows(buffer: &Buffer, frame: &SurfaceFrame, range: &SelectionRange) -> String {
    let rows: Vec<String> = visible_spans(frame, range)
        .into_iter()
        .map(|span| extract_span(buffer, frame.rect.x, &span))
        .collect();
    rows.join("\n")
}

/// One visible selected row, in screen coordinates.
struct VisibleSpan {
    screen_y: u16,
    first_x: u16,
    last_x: u16,
}

fn visible_spans(frame: &SurfaceFrame, range: &SelectionRange) -> Vec<VisibleSpan> {
    let mut spans = Vec::new();
    if frame.rect.width == 0 || frame.rect.height == 0 || frame.total_rows == 0 {
        return spans;
    }
    let below_viewport = frame
        .top_row
        .saturating_add(frame.rect.height as usize)
        .saturating_sub(1);
    let first_row = range.start.row.max(frame.top_row);
    let last_row = range.end.row.min(below_viewport).min(frame.total_rows - 1);
    if first_row > last_row {
        return spans;
    }
    for row in first_row..=last_row {
        let Some((first, last)) = range.columns_on(row, frame.rect.width) else {
            continue;
        };
        let Some(screen_y) = frame.screen_row(row) else {
            continue;
        };
        spans.push(VisibleSpan {
            screen_y,
            first_x: frame.rect.x.saturating_add(first),
            last_x: frame.rect.x.saturating_add(last),
        });
    }
    spans
}

/// Reads one row span, emitting each grapheme once.
///
/// ratatui stores a wide grapheme in its leading cell and resets the cells it
/// covers, so those read back as a blank symbol. Scanning from the surface's
/// left edge keeps the wide/continuation alignment right even when the span
/// starts mid-row, and continuation cells are skipped instead of emitted as
/// spurious spaces.
fn extract_span(buffer: &Buffer, row_start_x: u16, span: &VisibleSpan) -> String {
    let mut text = String::new();
    let mut continuation = 0u16;
    let mut x = row_start_x;
    while x <= span.last_x {
        let symbol = buffer
            .cell(Position::new(x, span.screen_y))
            .map_or(" ", ratatui::buffer::Cell::symbol);
        if continuation > 0 {
            continuation -= 1;
        } else {
            if x >= span.first_x {
                text.push_str(symbol);
            }
            continuation = u16::try_from(symbol.width()).unwrap_or(1).saturating_sub(1);
        }
        let Some(next) = x.checked_add(1) else {
            break;
        };
        x = next;
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};

    fn registry(frames: &[SurfaceFrame]) -> FrameSurfaces {
        let mut surfaces = FrameSurfaces::new();
        for frame in frames {
            surfaces.push(*frame);
        }
        surfaces
    }

    fn transcript(top_row: usize, total_rows: usize) -> SurfaceFrame {
        SurfaceFrame::scrollable(
            SurfaceId::Transcript,
            Rect::new(2, 3, 10, 4),
            top_row,
            total_rows,
        )
    }

    fn buffer_with_rows(rows: &[&str], width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, u16::try_from(rows.len()).expect("rows"));
        let mut buffer = Buffer::empty(area);
        let lines: Vec<Line<'_>> = rows.iter().map(|row| Line::from(*row)).collect();
        Paragraph::new(lines).render(area, &mut buffer);
        buffer
    }

    #[test]
    fn surface_at_picks_the_last_registered_overlapping_surface() {
        let pane = SurfaceFrame::fixed(SurfaceId::DashboardPane(0), Rect::new(0, 0, 20, 10));
        let popup = SurfaceFrame::fixed(SurfaceId::ModalBody, Rect::new(4, 2, 8, 4));
        let surfaces = registry(&[pane, popup]);

        assert_eq!(
            surfaces.surface_at(5, 3).map(|frame| frame.id),
            Some(SurfaceId::ModalBody)
        );
        assert_eq!(
            surfaces.surface_at(1, 3).map(|frame| frame.id),
            Some(SurfaceId::DashboardPane(0))
        );
        assert_eq!(surfaces.surface_at(30, 30), None);
    }

    #[test]
    fn content_pos_maps_screen_rows_through_top_row() {
        let frame = transcript(40, 200);

        assert_eq!(frame.content_pos(2, 3), ContentPos::new(40, 0));
        assert_eq!(frame.content_pos(5, 5), ContentPos::new(42, 3));
        assert_eq!(frame.screen_row(42), Some(5));
        assert_eq!(frame.screen_row(39), None);
        assert_eq!(frame.screen_row(44), None);
    }

    #[test]
    fn press_and_release_in_the_same_cell_reports_a_click() {
        let surfaces = registry(&[transcript(0, 4)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(5, 4, &surfaces);
        state.on_mouse_drag(5, 4, &surfaces);
        let action = state.on_mouse_up(5, 4, &surfaces);

        assert_eq!(action, SelectionAction::Click { column: 5, row: 4 });
        assert_eq!(state.range(), None);
        assert_eq!(state.active_surface(), None);
    }

    #[test]
    fn drag_to_another_cell_reports_a_copy_request() {
        let surfaces = registry(&[transcript(10, 100)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(4, 4, &surfaces);
        state.on_mouse_drag(7, 5, &surfaces);
        assert_eq!(state.active_surface(), Some(SurfaceId::Transcript));
        assert_eq!(
            state.range(),
            Some(SelectionRange {
                start: ContentPos::new(11, 2),
                end: ContentPos::new(12, 5),
            })
        );

        let action = state.on_mouse_up(7, 5, &surfaces);

        assert_eq!(
            action,
            SelectionAction::CopyRequested {
                surface: SurfaceId::Transcript,
                range: SelectionRange {
                    start: ContentPos::new(11, 2),
                    end: ContentPos::new(12, 5),
                },
            }
        );
        // The highlight survives the release.
        assert!(state.range().is_some());
        assert_eq!(state.active_surface(), Some(SurfaceId::Transcript));
    }

    #[test]
    fn dragging_backwards_normalizes_the_range() {
        let surfaces = registry(&[transcript(0, 4)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(8, 5, &surfaces);
        let action = state.on_mouse_up(3, 3, &surfaces);

        assert_eq!(
            action,
            SelectionAction::CopyRequested {
                surface: SurfaceId::Transcript,
                range: SelectionRange {
                    start: ContentPos::new(0, 1),
                    end: ContentPos::new(2, 6),
                },
            }
        );
    }

    #[test]
    fn dragging_outside_the_rect_clamps_to_the_visible_rows() {
        let surfaces = registry(&[transcript(10, 100)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(5, 5, &surfaces);
        state.on_mouse_drag(0, 0, &surfaces);
        assert_eq!(
            state.range(),
            Some(SelectionRange {
                start: ContentPos::new(10, 0),
                end: ContentPos::new(12, 3),
            })
        );

        state.on_mouse_drag(60, 40, &surfaces);
        assert_eq!(
            state.range(),
            Some(SelectionRange {
                start: ContentPos::new(12, 3),
                end: ContentPos::new(13, 9),
            })
        );
    }

    #[test]
    fn autoscroll_request_reports_a_direction_only_at_the_edges() {
        let surfaces = registry(&[transcript(10, 100)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(5, 5, &surfaces);
        assert_eq!(state.autoscroll_request(&surfaces), None);

        state.on_mouse_drag(5, 4, &surfaces);
        assert_eq!(state.autoscroll_request(&surfaces), None);

        state.on_mouse_drag(5, 3, &surfaces);
        assert_eq!(
            state.autoscroll_request(&surfaces),
            Some((SurfaceId::Transcript, -1))
        );

        state.on_mouse_drag(5, 1, &surfaces);
        assert_eq!(
            state.autoscroll_request(&surfaces),
            Some((SurfaceId::Transcript, -1))
        );

        state.on_mouse_drag(5, 6, &surfaces);
        assert_eq!(
            state.autoscroll_request(&surfaces),
            Some((SurfaceId::Transcript, 1))
        );

        state.on_mouse_up(5, 6, &surfaces);
        assert_eq!(state.autoscroll_request(&surfaces), None);
    }

    #[test]
    fn retrack_extends_the_selection_as_autoscroll_moves_rows_under_the_pointer() {
        let mut state = SelectionState::new();
        let before = registry(&[transcript(10, 100)]);

        // Drag to the bottom edge and hold still: content row 13 is selected.
        state.on_mouse_down(5, 4, &before);
        state.on_mouse_drag(5, 6, &before);
        assert_eq!(state.range().expect("dragging").end, ContentPos::new(13, 3));
        assert_eq!(
            state.autoscroll_request(&before),
            Some((SurfaceId::Transcript, 1))
        );

        // The caller scrolls one row and re-renders; the same screen cell now
        // shows content row 14.
        let after = registry(&[transcript(11, 100)]);
        state.retrack(&after);

        assert_eq!(state.range().expect("dragging").end, ContentPos::new(14, 3));
    }

    #[test]
    fn autoscroll_request_ignores_surfaces_with_nothing_scrolled_out() {
        let surfaces = registry(&[transcript(0, 4)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(5, 5, &surfaces);
        state.on_mouse_drag(5, 9, &surfaces);

        assert_eq!(state.autoscroll_request(&surfaces), None);
    }

    #[test]
    fn pressing_outside_every_surface_clears_the_selection() {
        let surfaces = registry(&[transcript(0, 4)]);
        let mut state = SelectionState::new();

        state.on_mouse_down(3, 3, &surfaces);
        state.on_mouse_up(9, 5, &surfaces);
        assert!(state.range().is_some());

        state.on_mouse_down(40, 40, &surfaces);
        assert_eq!(state.range(), None);
        assert_eq!(state.active_surface(), None);
        assert_eq!(state.on_mouse_up(40, 40, &surfaces), SelectionAction::None);
    }

    #[test]
    fn highlight_reverses_exactly_the_selected_cells() {
        let mut buffer = buffer_with_rows(&["abcdefgh", "ijklmnop", "qrstuvwx"], 8);
        let frame = SurfaceFrame::fixed(SurfaceId::PromptInput, Rect::new(1, 0, 6, 3));
        let range = SelectionRange {
            start: ContentPos::new(0, 3),
            end: ContentPos::new(1, 1),
        };

        highlight(&mut buffer, &frame, &range);

        let reversed: Vec<(u16, u16)> = (0..3)
            .flat_map(|y| (0..8).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buffer
                    .cell(Position::new(x, y))
                    .expect("cell")
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect();

        // First row: columns 3..=5 of the rect (screen x 4..=6).
        // Second row: columns 0..=1 of the rect (screen x 1..=2).
        assert_eq!(reversed, vec![(4, 0), (5, 0), (6, 0), (1, 1), (2, 1)]);
    }

    #[test]
    fn highlight_skips_rows_scrolled_out_of_view() {
        let mut buffer = buffer_with_rows(&["aaaa", "bbbb"], 4);
        let frame = SurfaceFrame::scrollable(SurfaceId::Transcript, Rect::new(0, 0, 4, 2), 5, 20);
        let range = SelectionRange {
            start: ContentPos::new(0, 0),
            end: ContentPos::new(5, 2),
        };

        highlight(&mut buffer, &frame, &range);

        let reversed: Vec<(u16, u16)> = (0..2)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buffer
                    .cell(Position::new(x, y))
                    .expect("cell")
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect();

        assert_eq!(reversed, vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn highlight_preserves_the_styles_already_on_the_cells() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(Line::from("bold").style(Style::new().bold())).render(area, &mut buffer);
        let frame = SurfaceFrame::fixed(SurfaceId::ModalBody, area);
        let range = SelectionRange {
            start: ContentPos::new(0, 0),
            end: ContentPos::new(0, 3),
        };

        highlight(&mut buffer, &frame, &range);

        let modifier = buffer.cell(Position::new(0, 0)).expect("cell").modifier;
        assert!(modifier.contains(Modifier::BOLD));
        assert!(modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn extract_rows_trims_trailing_pad_and_joins_with_newlines() {
        let buffer = buffer_with_rows(&["hello", "hi", "there"], 10);
        let frame = SurfaceFrame::fixed(SurfaceId::ModalBody, Rect::new(0, 0, 10, 3));
        let range = SelectionRange {
            start: ContentPos::new(0, 0),
            end: ContentPos::new(2, 9),
        };

        assert_eq!(extract_rows(&buffer, &frame, &range), "hello\nhi\nthere");
    }

    #[test]
    fn extract_rows_reads_a_partial_first_and_last_row() {
        let buffer = buffer_with_rows(&["abcdefgh", "ijklmnop", "qrstuvwx"], 8);
        let frame = SurfaceFrame::fixed(SurfaceId::PromptInput, Rect::new(0, 0, 8, 3));
        let range = SelectionRange {
            start: ContentPos::new(0, 5),
            end: ContentPos::new(2, 2),
        };

        assert_eq!(extract_rows(&buffer, &frame, &range), "fgh\nijklmnop\nqrs");
    }

    #[test]
    fn extract_rows_emits_wide_graphemes_exactly_once() {
        // ratatui writes a wide grapheme into its leading cell and resets the
        // cell it covers, so the continuation reads back as a blank symbol.
        let buffer = buffer_with_rows(&["世界 ok"], 10);
        assert_eq!(
            buffer.cell(Position::new(0, 0)).expect("cell").symbol(),
            "世"
        );
        assert_eq!(
            buffer.cell(Position::new(1, 0)).expect("cell").symbol(),
            " "
        );
        assert_eq!(
            buffer.cell(Position::new(2, 0)).expect("cell").symbol(),
            "界"
        );

        let frame = SurfaceFrame::fixed(SurfaceId::ElicitationMessage, Rect::new(0, 0, 10, 1));
        let range = SelectionRange {
            start: ContentPos::new(0, 0),
            end: ContentPos::new(0, 9),
        };
        assert_eq!(extract_rows(&buffer, &frame, &range), "世界 ok");

        // A span that starts on a continuation cell drops the half-covered
        // grapheme rather than emitting a stray space for it.
        let tail = SelectionRange {
            start: ContentPos::new(0, 1),
            end: ContentPos::new(0, 9),
        };
        assert_eq!(extract_rows(&buffer, &frame, &tail), "界 ok");
    }

    #[test]
    fn extract_rows_offsets_by_the_surface_rect() {
        let buffer = buffer_with_rows(&["xxhello", "xxworld"], 7);
        let frame = SurfaceFrame::fixed(SurfaceId::ResumeList, Rect::new(2, 0, 5, 2));
        let range = SelectionRange {
            start: ContentPos::new(0, 0),
            end: ContentPos::new(1, 4),
        };

        assert_eq!(extract_rows(&buffer, &frame, &range), "hello\nworld");
    }
}
