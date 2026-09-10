//! Reusable controls and focus-aware form routing for Hel TUIs.

mod controls;
mod layout;
mod scope;
pub mod scrollbar;
pub(crate) mod text_layout;

pub use controls::{
    Button, ButtonRow, Checkbox, ChoiceList, ComboBox, ComboBoxState, TabStrip, TextField,
};
pub use layout::{
    AutocompletePopup, FormViewport, PopupSide, dialog_content, dialog_rect, form_area,
    form_columns, form_rows,
};
pub use rat_event::{ConsumedEvent, Outcome};
pub use scope::{ControlKind, EventResult, FieldEdit, Form, Interaction, apply_field_edit};
pub use scrollbar::{ScrollbarGeometry, render_scrollbar, scrollbar_geometry};

/// A path field with the standard readline editing and cursor behavior.
pub struct PathField;
impl PathField {
    pub fn render<K: Copy + Eq>(
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        input: &crate::hel_path_input::PathInput,
        form: &mut Form<K>,
        id: K,
    ) {
        TextField::render(frame, area, input, form, id);
    }
    pub fn apply(input: &mut crate::hel_path_input::PathInput, edit: FieldEdit) -> Outcome {
        TextField::apply(input, edit)
    }
}
