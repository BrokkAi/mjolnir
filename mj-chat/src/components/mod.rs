//! Reusable controls and focus-aware form routing for Hel TUIs.

mod controls;
mod layout;
mod scope;
pub mod scrollbar;
pub(crate) mod text_layout;

pub use controls::{Button, ButtonRow, Checkbox, ChoiceList, TabStrip, TextField};
pub use layout::{FormViewport, dialog_content, dialog_rect, form_area, form_columns, form_rows};
pub use rat_event::{ConsumedEvent, Outcome};
pub use scope::{ControlKind, EventResult, FieldEdit, Form, Interaction, apply_field_edit};
pub use scrollbar::{ScrollbarGeometry, render_scrollbar, scrollbar_geometry};
