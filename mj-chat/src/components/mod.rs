//! Reusable controls and focus-aware form routing for Hel TUIs.

mod controls;
mod dialog;
pub use dialog::{ActionRole, Dialog, DialogAction, DialogLayout, DialogShell};
mod layout;
mod scope;
pub mod scrollbar;
pub(crate) mod text_layout;

#[cfg(test)]
mod test_support;

pub use crate::text_input::EditOutcome;
pub use controls::{
    Button, ButtonColumn, ButtonRow, Checkbox, ChoiceList, ColumnAlign, ColumnSplit, ComboBox,
    ComboBoxState, RowAlign, TabStrip, TextField,
};
pub use layout::{
    AutocompletePopup, FormViewport, PopupSide, dialog_content, dialog_rect, form_area,
    form_columns, form_rows,
};
pub use scope::{
    ControlKind, DOUBLE_CLICK_INTERVAL, EventResult, FieldEdit, Form, Interaction, ListActivation,
    apply_field_edit,
};
pub use scrollbar::{
    ScrollbarDrag, ScrollbarGeometry, ScrollbarPointer, render_scrollbar, scrollbar_geometry,
};
pub use text_layout::{
    Truncate, input_cursor_visual_position, input_visual_rows, set_input_cursor, truncate_to_cells,
};

/// A path field with the standard readline editing and cursor behavior, plus
/// an anchored popup of filesystem completions.
pub struct PathField;
impl PathField {
    /// The popup is at most this many rows tall, matching the shared popup shell.
    const VISIBLE_ROWS: usize = 8;

    /// The rows the popup needs beneath the field, its border included, or
    /// zero when nothing is open. A dialog reserves these so a popup never
    /// lands on the button row and never grows past its frame.
    #[must_use]
    pub fn popup_rows(input: &crate::path_input::PathInput) -> u16 {
        let rows = if input.is_completing() {
            input.completions().len().min(Self::VISIBLE_ROWS)
        } else if input.completion_pending().is_some() {
            1
        } else {
            return 0;
        };
        u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2)
    }

    pub fn render<K: Copy + Eq>(
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        input: &crate::path_input::PathInput,
        form: &mut Form<K>,
        id: K,
    ) {
        let bounds = frame.area();
        Self::render_within(frame, bounds, area, input, form, id);
    }

    /// Draws the field with its popup confined to `bounds`.
    ///
    /// The popup belongs to the surface that owns the field, so that surface
    /// says which rows it can spare: pass the region above a dialog's button
    /// row and the popup opens upward, or shrinks, instead of painting over
    /// the buttons, the frame, or the screen behind the dialog.
    pub fn render_within<K: Copy + Eq>(
        frame: &mut ratatui::Frame<'_>,
        bounds: ratatui::layout::Rect,
        area: ratatui::layout::Rect,
        input: &crate::path_input::PathInput,
        form: &mut Form<K>,
        id: K,
    ) {
        use ratatui::widgets::{List, ListItem, ListState, Paragraph};
        use unicode_width::UnicodeWidthStr;

        TextField::render_with_kind(frame, area, input, input.control_kind(), form, id);
        if !form.is_focused(id) {
            return;
        }
        if input.is_completing() {
            let candidates = input.completions();
            let title = if input.completion_truncated() {
                " first 50 matches \u{b7} keep typing "
            } else {
                " matches \u{b7} \u{2191}/\u{2193} select \u{b7} Enter accept "
            };
            let longest = candidates
                .iter()
                .map(|text| text.width())
                .max()
                .unwrap_or(0);
            let width = longest
                .saturating_add(4)
                .max(usize::from(area.width))
                .max(title.width().saturating_add(2));
            let Some((outer, inner)) = AutocompletePopup::render(
                frame,
                bounds,
                area,
                u16::try_from(width).unwrap_or(u16::MAX),
                candidates.len().min(Self::VISIBLE_ROWS),
                title,
                PopupSide::Below,
            ) else {
                return;
            };
            let items = candidates
                .iter()
                .map(|candidate| ListItem::new(candidate.as_str()))
                .collect::<Vec<_>>();
            let mut state = ListState::default();
            state.select(Some(input.completion_selected()));
            frame.render_stateful_widget(
                List::new(items).highlight_style(crate::theme::selection(true)),
                inner,
                &mut state,
            );
            let offset = state.offset();
            let mut popup_row_map = vec![None; usize::from(outer.height)];
            for (row, candidate) in popup_row_map
                .iter_mut()
                .enumerate()
                .skip(1)
                .take(usize::from(inner.height))
            {
                *candidate = offset
                    .checked_add(row.saturating_sub(1))
                    .filter(|index| *index < candidates.len());
            }
            form.register_popup(id, outer, popup_row_map);
        } else if input.completion_pending().is_some() {
            const WAITING: &str = "Completing\u{2026}";
            let width = WAITING
                .width()
                .saturating_add(4)
                .max(usize::from(area.width));
            if let Some((_, inner)) = AutocompletePopup::render(
                frame,
                bounds,
                area,
                u16::try_from(width).unwrap_or(u16::MAX),
                1,
                " completing ",
                PopupSide::Below,
            ) {
                frame.render_widget(Paragraph::new(WAITING), inner);
            }
        }
    }

    pub fn apply(input: &mut crate::path_input::PathInput, edit: FieldEdit) -> EditOutcome {
        let outcome = TextField::apply(input, edit);
        if outcome.changed() {
            input.dismiss_completion();
        }
        outcome
    }
}
