//! Reusable controls and focus-aware form routing for Hel TUIs.

mod controls;
mod dialog;
pub use dialog::{ActionRole, Dialog, DialogAction, DialogLayout, DialogShell};
mod layout;
mod scope;
pub mod scrollbar;
pub(crate) mod text_layout;

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
pub use scrollbar::{ScrollbarGeometry, render_scrollbar, scrollbar_geometry};
pub use text_layout::{
    Truncate, input_cursor_visual_position, input_visual_rows, set_input_cursor, truncate_to_cells,
};

/// A path field with the standard readline editing and cursor behavior.
pub struct PathField;
impl PathField {
    pub fn render<K: Copy + Eq>(
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        input: &crate::path_input::PathInput,
        form: &mut Form<K>,
        id: K,
    ) {
        TextField::render(frame, area, input, form, id);
    }
    pub fn apply(input: &mut crate::path_input::PathInput, edit: FieldEdit) -> EditOutcome {
        TextField::apply(input, edit)
    }
}
