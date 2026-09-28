//! Modal editor for ACP form elicitations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget, Wrap};

use crate::components::{
    ButtonRow, Checkbox, ChoiceList, ControlKind, FieldEdit, Form, Interaction, TextField,
};
use crate::selection::{
    ContentPos, FrameSurfaces, SelectionRange, SurfaceFrame, SurfaceId, extract_rows,
};
use crate::text_input::TextInput;
use mj_core::elicitation::{
    ElicitationField, ElicitationFieldKind, ElicitationRequest, ElicitationResponse,
    ElicitationValue, validate_field_value,
};

use super::input::{
    grapheme_offset_for_wrapped_row, input_cursor_visual_position, wrapped_row_for_grapheme_offset,
};
use super::rendering::sanitize_terminal_text;
use unicode_segmentation::UnicodeSegmentation;

/// How many wrapped rows of one logical line the extractor is willing to
/// render off screen when a selection cuts it. A plan line long enough to wrap
/// past this is pathological; the rows beyond it are dropped rather than
/// allowed to allocate an unbounded buffer.
const MAXIMUM_OFFSCREEN_ROWS: usize = 4_096;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum FieldValue {
    Text(#[serde(with = "draft_text")] TextInput),
    Single(Option<usize>),
    Multi(BTreeSet<usize>),
    Boolean(bool),
}

/// Carry user-authored form text across a binary upgrade without coupling the
/// handoff format to readline's internal history, undo, and cursor state.
mod draft_text {
    use super::TextInput;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(input: &TextInput, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(input.value())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<TextInput, D::Error> {
        String::deserialize(deserializer).map(TextInput::from_value)
    }
}

#[derive(Debug, Clone, Copy)]
struct DisplayField {
    field: usize,
    custom: Option<usize>,
    custom_option: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElicitationControl {
    Field(usize),
    Submit,
    Skip,
    Cancel,
    Previous,
    Next,
    GoBack,
    SubmitAnyway,
}

/// A process-local copy of an unanswered form. The request is retained in the
/// snapshot deliberately: an id can be reused by a harness for a different
/// form, and local answers must never be applied to that new form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ElicitationDraft {
    request: ElicitationRequest,
    values: Vec<FieldValue>,
    option_cursors: Vec<usize>,
    active_custom_fields: BTreeSet<usize>,
    focus: usize,
    message_scroll: u16,
    /// `(source line, grapheme offset)` at the top of the viewport. Both
    /// components are logical source coordinates; the visual row is
    /// recomputed by the renderer for the current width.
    message_anchor: Option<(usize, usize)>,
    message_anchor_width: u16,
    focus_scroll: u16,
    #[serde(default)]
    current_question: Option<usize>,
    #[serde(default)]
    confirmed: BTreeSet<usize>,
}

impl ElicitationDraft {
    /// Whether this snapshot belongs to exactly this pending request.
    #[must_use]
    pub fn matches(&self, request: &ElicitationRequest) -> bool {
        self.request == *request
    }

    pub(super) fn request(&self) -> &ElicitationRequest {
        &self.request
    }
}

#[derive(Debug, Clone)]
pub(super) struct ElicitationDialog {
    request: ElicitationRequest,
    values: Vec<FieldValue>,
    option_cursors: Vec<usize>,
    display_fields: Vec<DisplayField>,
    active_custom_fields: BTreeSet<usize>,
    /// The form is the sole focus and pointer authority for this modal.
    form: RefCell<Form<ElicitationControl>>,
    error: Option<String>,
    /// Page navigation is independent of control focus; only an explicit
    /// answer action confirms a page, and edits invalidate that confirmation.
    current_question: usize,
    confirmed: BTreeSet<usize>,
    confirming_unanswered: bool,
    message_scroll: Cell<u16>,
    message_page_height: Cell<u16>,
    message_max_scroll: Cell<u16>,
    message_area: Cell<Option<Rect>>,
    /// The source line and grapheme offset at the top of the last rendered
    /// message viewport. Keeping this anchor lets a resize recompute wrapped
    /// rows at the new width without moving the reader to an unrelated part
    /// of a long source line.
    message_anchor: Cell<Option<(usize, usize)>>,
    message_anchor_width: Cell<u16>,
    /// The complete question panel bounds from the most recent frame. This
    /// lets mouse routing distinguish its chrome from the transcript above
    /// without making the message pane a component hitbox (message text is
    /// still selectable).
    rendered_area: Cell<Option<Rect>>,
    /// Wrapped rows to skip in the form body when a long option list is
    /// taller than the space left above the answer controls.
    focus_scroll: Cell<u16>,
}

impl ElicitationDialog {
    pub(super) fn new(request: ElicitationRequest) -> Self {
        let mut values = request.fields.iter().map(default_value).collect::<Vec<_>>();
        let mut option_cursors = values
            .iter()
            .map(|value| match value {
                FieldValue::Single(Some(index)) => *index,
                FieldValue::Multi(selected) => selected.first().copied().unwrap_or(0),
                _ => 0,
            })
            .collect::<Vec<_>>();
        let (display_fields, active_custom_fields) = display_fields(&request, &values);
        for display in &display_fields {
            let Some(custom) = display.custom else {
                continue;
            };
            if active_custom_fields.contains(&custom)
                && let Some(option_count) = select_option_count(&request.fields[display.field])
            {
                let cursor = display.custom_option.unwrap_or(option_count);
                option_cursors[display.field] = cursor;
                if let FieldValue::Single(selected) = &mut values[display.field] {
                    *selected = Some(cursor);
                }
            }
        }
        let mut dialog = Self {
            request,
            values,
            option_cursors,
            display_fields,
            active_custom_fields,
            form: RefCell::new(Form::new()),
            error: None,
            current_question: 0,
            confirmed: BTreeSet::new(),
            confirming_unanswered: false,
            message_scroll: Cell::new(0),
            message_page_height: Cell::new(0),
            message_max_scroll: Cell::new(0),
            message_area: Cell::new(None),
            message_anchor: Cell::new(None),
            message_anchor_width: Cell::new(0),
            rendered_area: Cell::new(None),
            focus_scroll: Cell::new(0),
        };
        dialog.reset_controls();
        dialog
    }

    pub(super) fn request(&self) -> &ElicitationRequest {
        &self.request
    }

    /// The natural height of the current question page, including its panel
    /// border, message, focused answer content, action row, and footer.
    ///
    /// Prompts use their complete wrapped height, capped by the caller to
    /// the available pane. Overflow scrolls independently of the controls.
    pub(super) fn natural_height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(2).max(1);
        let focus_rows = focus_content(self).wrapped_height(content_width);
        let message_rows = u16::try_from(
            Paragraph::new(self.prompt_text())
                .wrap(Wrap { trim: true })
                .line_count(content_width),
        )
        .unwrap_or(u16::MAX)
        .max(1);

        // Two border rows, one footer row, and the two action rows match the
        // full-size renderer. The renderer collapses actions to one row only
        // when the capped pane itself is exceptionally small.
        2u16.saturating_add(1)
            .saturating_add(2)
            .saturating_add(message_rows)
            .saturating_add(focus_rows)
    }

    pub(super) fn draft(&self) -> ElicitationDraft {
        ElicitationDraft {
            request: self.request.clone(),
            values: self.values.clone(),
            option_cursors: self.option_cursors.clone(),
            active_custom_fields: self.active_custom_fields.clone(),
            focus: self.focus_index(),
            message_scroll: self.message_scroll.get(),
            message_anchor: self.message_anchor.get(),
            message_anchor_width: self.message_anchor_width.get(),
            focus_scroll: self.focus_scroll.get(),
            current_question: Some(self.current_question),
            confirmed: self.confirmed.clone(),
        }
    }

    pub(super) fn from_draft(request: ElicitationRequest, draft: ElicitationDraft) -> Option<Self> {
        if draft.request != request
            || draft.values.len() != request.fields.len()
            || draft.option_cursors.len() != request.fields.len()
        {
            return None;
        }
        let mut dialog = Self::new(request);
        dialog.values = draft.values;
        dialog.option_cursors = draft.option_cursors;
        dialog.active_custom_fields = draft.active_custom_fields;
        dialog.current_question = draft
            .current_question
            .unwrap_or(draft.focus)
            .min(dialog.display_fields.len().saturating_sub(1));
        dialog.confirmed = draft
            .confirmed
            .into_iter()
            .filter(|index| *index < dialog.display_fields.len())
            .collect();
        dialog.reset_controls();
        dialog.focus_control(draft.focus.min(dialog.display_fields.len() + 2));
        dialog.message_scroll = Cell::new(draft.message_scroll);
        dialog.message_anchor = Cell::new(draft.message_anchor);
        dialog.message_anchor_width = Cell::new(draft.message_anchor_width);
        dialog.focus_scroll = Cell::new(draft.focus_scroll);
        Some(dialog)
    }

    pub(super) fn paste(&mut self, text: &str) {
        let text = crate::text_input::single_line_paste(&sanitize_terminal_text(text));
        if text.is_empty() {
            return;
        }
        let Some((field, custom)) = self.editable_field() else {
            return;
        };
        if custom {
            self.active_custom_fields.insert(field);
        }
        if let Some(FieldValue::Text(value)) = self.values.get_mut(field) {
            value.insert_str(&text);
            self.confirmed.remove(&self.current_question);
            self.error = None;
        }
    }

    #[cfg(test)]
    pub(super) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<ElicitationResponse> {
        self.handle_key_event(KeyEvent::new(code, modifiers))
    }

    pub(super) fn handle_key_event(&mut self, key: KeyEvent) -> Option<ElicitationResponse> {
        let (code, modifiers) = super::normalize_key(key.code, key.modifiers);
        let key = KeyEvent::new_with_kind_and_state(code, modifiers, key.kind, key.state);
        if key.kind == crossterm::event::KeyEventKind::Release {
            return None;
        }
        if key.kind == crossterm::event::KeyEventKind::Press {
            if self.confirming_unanswered && code == KeyCode::Esc {
                self.go_back();
                return None;
            }
            if !self.confirming_unanswered {
                if modifiers == KeyModifiers::ALT && matches!(code, KeyCode::Left | KeyCode::Right)
                {
                    self.navigate(code == KeyCode::Right);
                    return None;
                }
                if code == KeyCode::Enter
                    && modifiers.is_empty()
                    && matches!(
                        self.form.borrow().focused(),
                        Some(ElicitationControl::Field(_))
                    )
                {
                    return self.answer_and_next();
                }
            }
        }
        match code {
            KeyCode::PageUp => {
                let page = isize::try_from(self.message_page_step()).unwrap_or(isize::MAX);
                self.scroll_message(-page);
                return None;
            }
            KeyCode::PageDown => {
                let page = isize::try_from(self.message_page_step()).unwrap_or(isize::MAX);
                self.scroll_message(page);
                return None;
            }
            _ => {}
        }
        self.error = None;
        // Inline custom answers retain their parent question's focus. Text
        // editing and option navigation are distinct operations on that page.
        if self.editable_field().is_some_and(|(_, custom)| custom)
            && !matches!(
                code,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter | KeyCode::Esc
            )
            && (!matches!(code, KeyCode::Up | KeyCode::Down) || !modifiers.is_empty())
        {
            self.form.borrow_mut().cancel_pointer();
            self.edit_text(key);
            return None;
        }
        let event = crossterm::event::Event::Key(key);
        let result = self.form.borrow_mut().handle(&event);
        if let Some(interaction) = result.action {
            return self.apply_interaction(interaction);
        }
        None
    }

    fn apply_interaction(
        &mut self,
        interaction: Interaction<ElicitationControl>,
    ) -> Option<ElicitationResponse> {
        match interaction {
            Interaction::Activate(ElicitationControl::Submit) => self.answer_and_next(),
            Interaction::Activate(ElicitationControl::Previous) => {
                self.navigate(false);
                None
            }
            Interaction::Activate(ElicitationControl::Next) => {
                self.navigate(true);
                None
            }
            Interaction::Activate(ElicitationControl::GoBack) => {
                self.go_back();
                None
            }
            Interaction::Activate(ElicitationControl::SubmitAnyway) => self.accept(),
            Interaction::Activate(ElicitationControl::Skip) => Some(ElicitationResponse::Decline),
            Interaction::Activate(ElicitationControl::Cancel) | Interaction::Cancel => {
                Some(ElicitationResponse::Cancel)
            }
            Interaction::Activate(ElicitationControl::Field(_)) => self.answer_and_next(),
            Interaction::Toggle(ElicitationControl::Field(index)) => {
                self.focus_control(index);
                self.toggle_current();
                None
            }
            Interaction::Select(ElicitationControl::Field(index), selected) => {
                self.focus_control(index);
                self.select_option(selected);
                None
            }
            Interaction::Edit(ElicitationControl::Field(index), edit) => {
                self.focus_control(index);
                match edit {
                    FieldEdit::Key(key) => self.edit_text(key),
                    FieldEdit::Paste(text) => self.paste(&text),
                    FieldEdit::Cursor(offset) => {
                        if let Some((field, _)) = self.editable_field()
                            && let FieldValue::Text(value) = &mut self.values[field]
                        {
                            value.set_cursor(offset);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<ElicitationResponse> {
        let event = crossterm::event::Event::Mouse(mouse);
        let result = self.form.borrow_mut().handle(&event);
        if let Some(interaction) = result.action {
            let toggle = matches!(
                mouse.kind,
                MouseEventKind::Up(crossterm::event::MouseButton::Left)
            ) && matches!(interaction, Interaction::Select(ElicitationControl::Field(index), _) if matches!(self.values[self.display_fields[index].field], FieldValue::Multi(_)));
            let response = self.apply_interaction(interaction);
            if toggle {
                if let Some((custom, true)) = self.editable_field() {
                    self.active_custom_fields.insert(custom);
                } else {
                    self.toggle_current();
                }
            }
            return response;
        }
        if result.consumed {
            return None;
        }
        if !self
            .message_area
            .get()
            .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
        {
            return None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_message(-3),
            MouseEventKind::ScrollDown => self.scroll_message(3),
            _ => {}
        }
        None
    }

    pub(super) fn component_handles_mouse_at(&self, column: u16, row: u16) -> bool {
        let form = self.form.borrow();
        form.captures_pointer() || form.contains(column, row)
    }

    pub(super) fn rendered_area_contains(&self, column: u16, row: u16) -> bool {
        self.rendered_area
            .get()
            .is_some_and(|area| area.contains(Position::new(column, row)))
    }

    pub(super) fn cancel_component_pointer(&self) {
        self.form.borrow_mut().cancel_pointer();
    }

    pub(super) fn reset_component_geometry(&self) {
        self.form.borrow_mut().reset_geometry();
        self.rendered_area.set(None);
    }

    fn focus_index(&self) -> usize {
        match self.form.borrow().focused() {
            Some(ElicitationControl::Field(index)) => index,
            Some(ElicitationControl::Submit) => self.display_fields.len(),
            Some(ElicitationControl::Skip) => self.display_fields.len() + 1,
            Some(ElicitationControl::Cancel) => self.display_fields.len() + 2,
            _ => self.current_question,
        }
    }

    fn focus_control(&mut self, index: usize) {
        let field_count = self.display_fields.len();
        let control = match index.cmp(&field_count) {
            std::cmp::Ordering::Less => ElicitationControl::Field(index),
            std::cmp::Ordering::Equal => ElicitationControl::Submit,
            std::cmp::Ordering::Greater if index == field_count + 1 => ElicitationControl::Skip,
            _ => ElicitationControl::Cancel,
        };
        if index < field_count && index != self.current_question {
            self.current_question = index;
            self.reset_page();
        }
        self.form.borrow_mut().focus(control);
    }

    fn actions(&self) -> Vec<(ElicitationControl, &'static str, bool)> {
        if self.confirming_unanswered {
            return vec![
                (ElicitationControl::GoBack, "Go back", true),
                (ElicitationControl::SubmitAnyway, "Submit anyway", true),
            ];
        }
        let mut actions = vec![(
            ElicitationControl::Submit,
            if self.current_question + 1 < self.display_fields.len() {
                "Answer and next"
            } else {
                "Submit all"
            },
            true,
        )];
        if self.display_fields.len() > 1 {
            actions.push((
                ElicitationControl::Previous,
                "Previous",
                self.current_question > 0,
            ));
            actions.push((
                ElicitationControl::Next,
                "Next",
                self.current_question + 1 < self.display_fields.len(),
            ));
        }
        actions.extend([
            (ElicitationControl::Skip, "Skip", true),
            (ElicitationControl::Cancel, "Cancel", true),
        ]);
        actions
    }

    fn reset_controls(&mut self) {
        let mut form = Form::new();
        let first = if self.confirming_unanswered {
            ElicitationControl::GoBack
        } else if let Some(display) = self.display_fields.get(self.current_question).copied() {
            let id = ElicitationControl::Field(self.current_question);
            form.declare(
                id,
                display_control_kind(&self.request, &self.values, &self.option_cursors, display),
            );
            id
        } else {
            ElicitationControl::Submit
        };
        for (id, _, enabled) in self.actions() {
            form.declare_with_enabled(id, ControlKind::Button, enabled);
        }
        form.end_frame(first);
        self.form = RefCell::new(form);
    }

    fn reset_page(&mut self) {
        self.message_scroll.set(0);
        self.message_anchor.set(None);
        self.message_anchor_width.set(0);
        self.focus_scroll.set(0);
        self.reset_controls();
    }

    fn navigate(&mut self, forward: bool) {
        let next = if forward {
            (self.current_question + 1).min(self.display_fields.len().saturating_sub(1))
        } else {
            self.current_question.saturating_sub(1)
        };
        if next != self.current_question {
            self.current_question = next;
            self.error = None;
            self.reset_page();
        }
    }

    fn unanswered(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.display_fields.len()).filter(|index| !self.confirmed.contains(index))
    }

    fn go_back(&mut self) {
        let first = self.unanswered().next();
        self.confirming_unanswered = false;
        self.current_question = first.unwrap_or(self.current_question);
        self.reset_page();
    }

    fn prompt_text(&self) -> String {
        if self.confirming_unanswered {
            let mut text = format!(
                "Submit with {} unanswered questions?\n\n",
                self.unanswered().count()
            );
            for index in self.unanswered() {
                text.push_str(&format!(
                    "{}. {}\n",
                    index + 1,
                    self.request.fields[self.display_fields[index].field].title
                ));
            }
            return text;
        }
        if !self.is_plan_review()
            && let Some(display) = self.display_fields.get(self.current_question)
        {
            let field = &self.request.fields[display.field];
            let mut text = field.title.clone();
            if field.required {
                text.push_str(" (required)");
            }
            if let Some(description) = &field.description {
                text.push('\n');
                text.push_str(description);
            }
            text.push_str("\n\n");
            text.push_str(&self.request.message);
            return text;
        }
        self.request.message.clone()
    }

    fn select_option(&mut self, selected: usize) {
        let index = self.focus_index();
        let Some(display) = self.display_fields.get(index).copied() else {
            return;
        };
        let option_count = select_option_count(&self.request.fields[display.field]).unwrap_or(0);
        let count =
            option_count + usize::from(display.custom.is_some() && display.custom_option.is_none());
        if selected >= count {
            return;
        }
        self.option_cursors[display.field] = selected;
        if let FieldValue::Single(value) = &mut self.values[display.field] {
            let next = (selected < option_count).then_some(selected);
            if *value != next {
                self.confirmed.remove(&self.current_question);
            }
            *value = next;
            if let Some(custom) = display.custom {
                if display.custom_option == Some(selected) || selected == option_count {
                    self.active_custom_fields.insert(custom);
                } else {
                    self.active_custom_fields.remove(&custom);
                }
            }
        }
    }

    fn is_plan_review(&self) -> bool {
        mj_core::acp::is_plan_review_id(&self.request.id)
    }

    /// The message pane's content area, recorded by the last frame.
    pub(super) fn message_area(&self) -> Option<Rect> {
        self.message_area.get()
    }

    /// The message text a selection covers, reconstructed as source lines.
    ///
    /// Wrapping is a rendering artifact, so a logical line the range covers
    /// whole comes back exactly as the message wrote it, without the newlines
    /// word wrap introduced. Only the range's partial endpoints are cut, and
    /// those go back through the same `Paragraph` pipeline the pane renders
    /// with, so wide characters are sliced on the cells they actually occupy.
    pub(super) fn selection_text(&self, range: &SelectionRange, width: u16) -> String {
        if width == 0 {
            return String::new();
        }
        let mut selected = Vec::new();
        let mut base = 0usize;
        for line in self.prompt_text().split('\n') {
            let rows = wrapped_row_count(line, width);
            let last_row = base.saturating_add(rows.saturating_sub(1));
            if rows == 0 || base > range.end.row || last_row < range.start.row {
                base = base.saturating_add(rows);
                continue;
            }
            let first = range.start.row.max(base);
            let last = range.end.row.min(last_row);
            let whole = first == base
                && last == last_row
                && (first..=last).all(|row| range.columns_on(row, width) == Some((0, width - 1)));
            selected.push(if whole {
                line.to_owned()
            } else {
                partial_line_text(line, width, base, (first, last), range)
            });
            base = base.saturating_add(rows);
            if base > range.end.row {
                break;
            }
        }
        selected.join("\n")
    }

    fn message_page_step(&self) -> u16 {
        self.message_page_height.get().saturating_sub(1).max(1)
    }

    pub(super) fn scroll_message(&self, delta: isize) {
        let current = usize::from(self.message_scroll.get());
        let maximum = usize::from(self.message_max_scroll.get());
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(maximum)
        };
        if next == current {
            return;
        }
        self.message_scroll.set(next as u16);
        // A user scroll establishes a new logical anchor. Subsequent redraws
        // and resizes retain that source position instead of repeatedly
        // replacing it with whichever wrapped row happens to be at the top.
        if self.message_anchor_width.get() > 0 {
            self.message_anchor.set(Some(message_position_at_row(
                &self.prompt_text(),
                self.message_anchor_width.get(),
                next,
            )));
        }
    }

    fn toggle_current(&mut self) {
        if self.editable_field().is_some() {
            self.edit_text(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            return;
        }
        let Some(display) = self.display_fields.get(self.focus_index()).copied() else {
            return;
        };
        self.confirmed.remove(&self.current_question);
        let value = &mut self.values[display.field];
        match value {
            FieldValue::Single(selected) => {
                *selected = Some(self.option_cursors[display.field]);
                if let Some(custom) = display.custom {
                    self.active_custom_fields.remove(&custom);
                }
            }
            FieldValue::Multi(selected) => {
                let index = self.option_cursors[display.field];
                if !selected.remove(&index) {
                    selected.insert(index);
                }
                if let Some(custom) = display.custom {
                    self.active_custom_fields.remove(&custom);
                }
            }
            FieldValue::Boolean(selected) => *selected = !*selected,
            FieldValue::Text(_) => unreachable!("text fields are handled above"),
        }
    }

    fn edit_text(&mut self, key: KeyEvent) {
        let Some((field, custom)) = self.editable_field() else {
            return;
        };
        let activated = custom && self.active_custom_fields.insert(field);
        let Some(FieldValue::Text(value)) = self.values.get_mut(field) else {
            unreachable!("editable fields contain text values")
        };
        let before = value.to_string();
        value.handle_key(key);
        if activated || value.value() != before {
            self.confirmed.remove(&self.current_question);
        }
    }

    fn question_content(
        &self,
        display_index: usize,
    ) -> Result<BTreeMap<String, ElicitationValue>, String> {
        let display = self.display_fields[display_index];
        let active_custom = display
            .custom
            .filter(|custom| self.active_custom_fields.contains(custom));
        let indices = match (active_custom, display.custom_option) {
            (Some(custom), Some(_)) => vec![display.field, custom],
            (Some(custom), None) => vec![custom],
            (None, _) => vec![display.field],
        };
        let mut content = BTreeMap::new();
        for index in indices {
            let field = &self.request.fields[index];
            if let Some(value) = validated_value(field, &self.values[index])? {
                content.insert(field.id.clone(), value);
            }
        }
        if self.request.fields[display.field].required && content.is_empty() {
            return Err(format!(
                "{} is required",
                self.request.fields[display.field].title
            ));
        }
        Ok(content)
    }

    fn answer_and_next(&mut self) -> Option<ElicitationResponse> {
        if !self.display_fields.is_empty() {
            match self.question_content(self.current_question) {
                Ok(content) => {
                    if content.is_empty() {
                        self.confirmed.remove(&self.current_question);
                    } else {
                        self.confirmed.insert(self.current_question);
                    }
                }
                Err(error) => {
                    self.error = Some(error);
                    self.focus_control(self.current_question);
                    return None;
                }
            }
        }
        if self.current_question + 1 < self.display_fields.len() {
            self.navigate(true);
            return None;
        }
        let missing = self
            .unanswered()
            .find(|index| self.request.fields[self.display_fields[*index].field].required);
        if let Some(index) = missing {
            self.focus_control(index);
            self.error = Some(format!(
                "{} is required",
                self.request.fields[self.display_fields[index].field].title
            ));
            return None;
        }
        if self.unanswered().next().is_some() {
            self.confirming_unanswered = true;
            self.reset_page();
            return None;
        }
        self.accept()
    }

    fn accept(&mut self) -> Option<ElicitationResponse> {
        let mut content = BTreeMap::new();
        for index in self.confirmed.iter().copied() {
            match self.question_content(index) {
                Ok(answer) => content.extend(answer),
                Err(error) => {
                    self.error = Some(error);
                    return None;
                }
            }
        }
        if let Err(error) = self
            .request
            .validate_response(&ElicitationResponse::Accept {
                content: content.clone(),
            })
        {
            self.error = Some(error);
            return None;
        }
        Some(ElicitationResponse::Accept { content })
    }

    fn editable_field(&self) -> Option<(usize, bool)> {
        let Some(ElicitationControl::Field(index)) = self.form.borrow().focused() else {
            return None;
        };
        let display = self.display_fields.get(index)?;
        if matches!(self.values[display.field], FieldValue::Text(_)) {
            return Some((display.field, false));
        }
        let custom = display.custom?;
        let option_count = select_option_count(&self.request.fields[display.field])?;
        let custom_cursor = display.custom_option.unwrap_or(option_count);
        (self.option_cursors[display.field] == custom_cursor).then_some((custom, true))
    }
}

/// Rows one source line takes when the message pane wraps it.
///
/// ratatui wraps each input line on its own, so these counts compose: their
/// prefix sums are the visual rows of the whole message.
fn wrapped_row_count(line: &str, width: u16) -> usize {
    Paragraph::new(line)
        .wrap(Wrap { trim: true })
        .line_count(width)
}

/// The part of one logical line a range covers, cut on cell boundaries.
///
/// The line is re-rendered alone, scrolled to the first covered row, so the
/// engine can slice the same cells the pane drew. Word wrap consumed the
/// spaces it broke on, so the covered rows rejoin with one space.
fn partial_line_text(
    line: &str,
    width: u16,
    base: usize,
    covered: (usize, usize),
    range: &SelectionRange,
) -> String {
    let (first, mut last) = covered;
    // Clamping `last` rather than the height keeps the row the range ends on
    // and the rows rendered for it the same rows, so a capped line takes the
    // full-width branch below instead of cutting a row it never drew.
    last = last.min(first.saturating_add(MAXIMUM_OFFSCREEN_ROWS - 1));
    let Ok(height) = u16::try_from(last.saturating_sub(first).saturating_add(1)) else {
        return String::new();
    };
    let skip = u16::try_from(first.saturating_sub(base)).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(line)
        .wrap(Wrap { trim: true })
        .scroll((skip, 0))
        .render(area, &mut buffer);
    let cut = SelectionRange {
        start: ContentPos::new(
            0,
            if first == range.start.row {
                range.start.col
            } else {
                0
            },
        ),
        end: ContentPos::new(
            usize::from(height - 1),
            if last == range.end.row {
                range.end.col
            } else {
                width - 1
            },
        ),
    };
    let frame = SurfaceFrame::fixed(SurfaceId::ElicitationMessage, area);
    extract_rows(&buffer, &frame, &cut)
        .split('\n')
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_fields(
    request: &ElicitationRequest,
    values: &[FieldValue],
) -> (Vec<DisplayField>, BTreeSet<usize>) {
    let fields_by_id = request
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| (field.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut custom_by_owner = BTreeMap::new();
    let mut attached_custom = BTreeSet::new();
    for (custom, field) in request.fields.iter().enumerate() {
        let Some(owner) = field
            .custom_answer_for
            .as_deref()
            .and_then(|owner| fields_by_id.get(owner))
            .copied()
        else {
            continue;
        };
        if !matches!(field.kind, ElicitationFieldKind::Text { .. })
            || select_option_count(&request.fields[owner]).is_none()
            || custom_by_owner.contains_key(&owner)
        {
            continue;
        }
        custom_by_owner.insert(owner, custom);
        attached_custom.insert(custom);
    }
    let display_fields = request
        .fields
        .iter()
        .enumerate()
        .filter(|(index, _)| !attached_custom.contains(index))
        .map(|(field, _)| DisplayField {
            field,
            custom: custom_by_owner.get(&field).copied(),
            custom_option: custom_by_owner.get(&field).and_then(|custom| {
                request.fields[*custom]
                    .custom_answer_option
                    .as_deref()
                    .and_then(|value| select_option_index(&request.fields[field], value))
            }),
        })
        .collect::<Vec<_>>();
    let active_custom_fields = attached_custom
        .into_iter()
        .filter(|index| {
            matches!(
                &values[*index],
                FieldValue::Text(value) if !value.is_empty()
            )
        })
        .collect();
    (display_fields, active_custom_fields)
}

fn select_option_count(field: &ElicitationField) -> Option<usize> {
    match &field.kind {
        ElicitationFieldKind::SingleSelect { options, .. }
        | ElicitationFieldKind::MultiSelect { options, .. } => Some(options.len()),
        _ => None,
    }
}

fn select_option_index(field: &ElicitationField, value: &str) -> Option<usize> {
    match &field.kind {
        ElicitationFieldKind::SingleSelect { options, .. } => {
            options.iter().position(|option| option.value == value)
        }
        _ => None,
    }
}

fn display_control_kind(
    request: &ElicitationRequest,
    values: &[FieldValue],
    cursors: &[usize],
    display: DisplayField,
) -> ControlKind {
    match &request.fields[display.field].kind {
        ElicitationFieldKind::SingleSelect { options, .. }
        | ElicitationFieldKind::MultiSelect { options, .. } => ControlKind::ChoiceList {
            len: options.len()
                + usize::from(display.custom.is_some() && display.custom_option.is_none()),
            selected: cursors[display.field],
        },
        _ if matches!(values[display.field], FieldValue::Boolean(_)) => ControlKind::Checkbox,
        _ => ControlKind::TextField,
    }
}

fn default_value(field: &ElicitationField) -> FieldValue {
    match &field.kind {
        ElicitationFieldKind::Text { default, .. } => {
            FieldValue::Text(default.clone().unwrap_or_default().into())
        }
        ElicitationFieldKind::SingleSelect { options, default } => {
            let selected = match default {
                Some(default) => options.iter().position(|option| option.value == *default),
                None => (!options.is_empty()).then_some(0),
            };
            FieldValue::Single(selected)
        }
        ElicitationFieldKind::MultiSelect {
            options, default, ..
        } => FieldValue::Multi(
            options
                .iter()
                .enumerate()
                .filter_map(|(index, option)| default.contains(&option.value).then_some(index))
                .collect(),
        ),
        ElicitationFieldKind::Boolean { default } => FieldValue::Boolean(default.unwrap_or(false)),
        ElicitationFieldKind::Integer { default, .. } => FieldValue::Text(
            default
                .map(|value| value.to_string())
                .unwrap_or_default()
                .into(),
        ),
        ElicitationFieldKind::Number { default, .. } => FieldValue::Text(
            default
                .map(|value| value.to_string())
                .unwrap_or_default()
                .into(),
        ),
    }
}

fn validated_value(
    field: &ElicitationField,
    value: &FieldValue,
) -> Result<Option<ElicitationValue>, String> {
    let missing = || Err(format!("{} is required", field.title));
    let answered = match (&field.kind, value) {
        (ElicitationFieldKind::Text { .. }, FieldValue::Text(value)) => {
            if value.value().trim().is_empty() {
                return if field.required { missing() } else { Ok(None) };
            }
            ElicitationValue::String(value.to_string())
        }
        (ElicitationFieldKind::SingleSelect { options, .. }, FieldValue::Single(selected)) => {
            let Some(index) = selected else {
                return if field.required { missing() } else { Ok(None) };
            };
            ElicitationValue::String(options[*index].value.clone())
        }
        (ElicitationFieldKind::MultiSelect { options, .. }, FieldValue::Multi(selected)) => {
            if selected.is_empty() && field.required {
                return missing();
            }
            let value = ElicitationValue::StringArray(
                selected
                    .iter()
                    .map(|index| options[*index].value.clone())
                    .collect(),
            );
            // An empty optional multi-select still has to satisfy `minItems`,
            // so the constraints are checked before the answer is dropped.
            validate_field_value(field, &value)?;
            if selected.is_empty() {
                return Ok(None);
            }
            value
        }
        (ElicitationFieldKind::Boolean { .. }, FieldValue::Boolean(value)) => {
            ElicitationValue::Boolean(*value)
        }
        (ElicitationFieldKind::Integer { .. }, FieldValue::Text(value)) => {
            if value.value().trim().is_empty() {
                return if field.required { missing() } else { Ok(None) };
            }
            ElicitationValue::Integer(
                value
                    .parse::<i64>()
                    .map_err(|_| format!("{} must be an integer", field.title))?,
            )
        }
        (ElicitationFieldKind::Number { .. }, FieldValue::Text(value)) => {
            if value.value().trim().is_empty() {
                return if field.required { missing() } else { Ok(None) };
            }
            ElicitationValue::Number(
                value
                    .parse::<f64>()
                    .map_err(|_| format!("{} must be a number", field.title))?,
            )
        }
        _ => return Err(format!("{} has an incompatible value", field.title)),
    };
    validate_field_value(field, &answered)?;
    Ok(Some(answered))
}

/// Draws an elicitation in the bounds owned by the chat surface. This is used
/// by the combined dashboard so the question cannot cover its navigator or
/// the other support panes.
pub(super) fn render_elicitation_in(
    frame: &mut Frame,
    dialog: &ElicitationDialog,
    surfaces: &mut FrameSurfaces,
    bounds: Rect,
    focused: bool,
) {
    render_elicitation_at(frame, dialog, surfaces, bounds, focused);
}

fn render_elicitation_at(
    frame: &mut Frame,
    dialog: &ElicitationDialog,
    surfaces: &mut FrameSurfaces,
    area: Rect,
    focused: bool,
) {
    // The question replaces the session content rectangle. Clear exactly that
    // rectangle so hidden transcript text cannot show through it, while the
    // navigator and every neighboring pane remain untouched.
    frame.render_widget(Clear, area);
    dialog.rendered_area.set(Some(area));
    let title = if dialog.confirming_unanswered {
        "Unanswered questions".to_owned()
    } else if dialog.display_fields.is_empty() {
        dialog
            .request
            .title
            .clone()
            .unwrap_or_else(|| "Agent question".into())
    } else {
        format!(
            "Question {}/{} · {} unanswered",
            dialog.current_question + 1,
            dialog.display_fields.len(),
            dialog.unanswered().count()
        )
    };
    let focus = focus_content(dialog);
    let focused_field = dialog.current_question;
    let inner = {
        let mut form = dialog.form.borrow_mut();
        form.begin_frame();
        let title_line = crate::modal::dismissible_modal_title(
            &mut form,
            area,
            &title,
            theme::title(focused),
            true,
        );
        let mut block = theme::panel(focused).title(title_line);
        // Compact panes reserve their content rows for the labeled control
        // and actions; keep validation failures visible in the bottom border.
        if area.height < 6
            && let Some(error) = dialog.error.as_deref()
        {
            block = block.title_bottom(Line::styled(
                error,
                Style::default().fg(theme::palette().error),
            ));
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };
    render_elicitation_body(
        frame,
        dialog,
        surfaces,
        inner,
        focused,
        focus,
        focused_field,
    );
}

fn render_elicitation_body(
    frame: &mut Frame,
    dialog: &ElicitationDialog,
    surfaces: &mut FrameSurfaces,
    inner: Rect,
    focused: bool,
    focus: FocusContent<'_>,
    focused_field: usize,
) {
    let natural_focus_height = focus.wrapped_height(inner.width);
    let prompt = dialog.prompt_text();
    let natural_message_height = u16::try_from(
        Paragraph::new(prompt.as_str())
            .wrap(Wrap { trim: true })
            .line_count(inner.width),
    )
    .unwrap_or(u16::MAX)
    .max(1);
    // Keep a field's title, control, and action row before spending a row on
    // keyboard hints. A five-row bordered pane has only three content rows.
    let footer_height = u16::from(inner.height >= 4);
    // In a compact question pane one button row is enough: the second row is
    // more valuable to the message and focused control, which can each scroll
    // their content independently.
    let button_rows = if inner.height <= 4 { 1 } else { 2 };
    let buttons_height = inner.height.saturating_sub(footer_height).min(button_rows);
    let body_height = inner.height.saturating_sub(footer_height + buttons_height);
    let (message_height, focus_height) = if dialog.confirming_unanswered {
        (body_height, 0)
    } else if dialog.is_plan_review() {
        let focus_height = natural_focus_height
            .min(body_height.saturating_sub(1).max(1))
            .min(body_height);
        (body_height.saturating_sub(focus_height), focus_height)
    } else {
        let minimum_answer = natural_focus_height.min(2);
        let message_height = natural_message_height
            .min(
                body_height
                    .saturating_sub(minimum_answer)
                    .max(body_height / 2),
            )
            .min(body_height.saturating_sub(1));
        (message_height, body_height.saturating_sub(message_height))
    };
    let constraints = [
        Constraint::Length(message_height),
        Constraint::Length(focus_height),
        Constraint::Length(buttons_height),
        Constraint::Length(footer_height),
    ];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    let message = Paragraph::new(prompt.as_str()).wrap(Wrap { trim: true });
    let total_lines = u16::try_from(message.line_count(chunks[0].width)).unwrap_or(u16::MAX);
    let maximum_scroll = total_lines.saturating_sub(chunks[0].height);
    let anchored_scroll = (dialog.message_anchor_width.get() != chunks[0].width)
        .then(|| dialog.message_anchor.get())
        .flatten()
        .map(|(line, row)| {
            let source = prompt.split('\n').nth(line).unwrap_or("");
            wrapped_prefix_rows(&prompt, chunks[0].width, line).saturating_add(
                u16::try_from(wrapped_row_for_grapheme_offset(
                    source,
                    usize::from(chunks[0].width),
                    row,
                ))
                .unwrap_or(u16::MAX),
            )
        });
    let message_scroll = anchored_scroll
        .unwrap_or_else(|| dialog.message_scroll.get())
        .min(maximum_scroll);
    dialog.message_scroll.set(message_scroll);
    if dialog.message_anchor.get().is_none() {
        dialog.message_anchor.set(Some(message_position_at_row(
            &prompt,
            chunks[0].width,
            usize::from(message_scroll),
        )));
    }
    dialog.message_anchor_width.set(chunks[0].width);
    dialog.message_page_height.set(chunks[0].height);
    dialog.message_max_scroll.set(maximum_scroll);
    dialog.message_area.set(Some(chunks[0]));
    frame.render_widget(message.scroll((message_scroll, 0)), chunks[0]);
    // The message pane scrolls its own rows, so it registers in content space:
    // a drag can reach the plan text above and below the viewport.
    surfaces.push(SurfaceFrame::scrollable(
        SurfaceId::ElicitationMessage,
        chunks[0],
        usize::from(message_scroll),
        usize::from(total_lines),
    ));
    surfaces.push(SurfaceFrame::fixed(SurfaceId::ModalBody, chunks[1]));
    // The field title stays outside the scrolled field body, so scrolling to a
    // control or a distant option can never hide the question it answers. A
    // single remaining row belongs to the control itself.
    let focus_area = match focus.title.as_ref().filter(|_| chunks[1].height >= 2) {
        Some(title) => {
            frame.render_widget(
                Paragraph::new(title.clone()),
                Rect {
                    height: 1,
                    ..chunks[1]
                },
            );
            Rect {
                y: chunks[1].y.saturating_add(1),
                height: chunks[1].height.saturating_sub(1),
                ..chunks[1]
            }
        }
        None => chunks[1],
    };
    let field_index = focused_field.min(dialog.display_fields.len().saturating_sub(1));
    {
        let mut form = dialog.form.borrow_mut();
        if !dialog.confirming_unanswered
            && let Some(display) = dialog.display_fields.get(focused_field).copied()
        {
            form.register(
                ElicitationControl::Field(focused_field),
                display_control_kind(
                    &dialog.request,
                    &dialog.values,
                    &dialog.option_cursors,
                    display,
                ),
                focus_area,
                true,
            );
        }
    }

    let focus_rows = u16::try_from(
        Paragraph::new(focus.lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(focus_area.width),
    )
    .unwrap_or(u16::MAX)
    .max(1);
    let focus_max_scroll = focus_rows.saturating_sub(focus_area.height);
    let target_row = focus.focused_row.map_or(0, |line| {
        let prefix_rows = Paragraph::new(focus.lines[..line].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(focus_area.width);
        let cursor_row = focus
            .text_cursor
            .filter(|(cursor_line, _)| usize::from(*cursor_line) == line)
            .map_or(0, |(_, column)| {
                let text = focus.lines[line].to_string();
                let cursor_grapheme = 2usize.saturating_add(column);
                let cursor_byte = text
                    .grapheme_indices(true)
                    .nth(cursor_grapheme)
                    .map_or(text.len(), |(offset, _)| offset);
                input_cursor_visual_position(&text, cursor_byte, usize::from(focus_area.width)).1
            });
        prefix_rows.saturating_add(cursor_row)
    });
    let mut focus_scroll = dialog.focus_scroll.get().min(focus_max_scroll);
    let target_row = target_row.min(usize::from(u16::MAX)) as u16;
    if target_row < focus_scroll {
        focus_scroll = target_row;
    } else if target_row >= focus_scroll.saturating_add(focus_area.height) {
        focus_scroll = target_row
            .saturating_sub(focus_area.height.saturating_sub(1))
            .min(focus_max_scroll);
    }
    dialog.focus_scroll.set(focus_scroll);
    if !dialog.confirming_unanswered
        && let Some(display) = dialog.display_fields.get(focused_field).copied()
    {
        let id = ElicitationControl::Field(field_index);
        let mut form = dialog.form.borrow_mut();
        if select_option_count(&dialog.request.fields[display.field]).is_some() {
            ChoiceList::render_wrapped(
                frame,
                focus_area,
                &focus.lines,
                &focus.option_rows,
                dialog.option_cursors[display.field],
                focus_scroll,
                &mut form,
                id,
            );
        } else {
            render_focus(
                frame,
                focus_area,
                &focus,
                focus_scroll,
                focused && form.is_focused(id),
            );
        }
        if let Some((line, _)) = focus.text_cursor {
            let prefix = Paragraph::new(focus.lines[..usize::from(line)].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(focus_area.width);
            // The editor is a single row; scrolling belongs to the field,
            // while the surrounding option descriptions keep their wrapping.
            let row = prefix.saturating_sub(usize::from(focus_scroll));
            if row < usize::from(focus_area.height) {
                let field = if matches!(dialog.values[display.field], FieldValue::Text(_)) {
                    display.field
                } else {
                    display
                        .custom
                        .expect("inline text belongs to a custom answer")
                };
                if let FieldValue::Text(input) = &dialog.values[field] {
                    let area = Rect::new(
                        focus_area.x.saturating_add(2),
                        focus_area.y.saturating_add(row as u16),
                        focus_area.width.saturating_sub(2),
                        1,
                    );
                    if select_option_count(&dialog.request.fields[display.field]).is_none() {
                        form.register(id, ControlKind::TextField, area, true);
                    }
                    TextField::render_inline(
                        frame,
                        area,
                        input,
                        dialog.request.fields[field].secret,
                        focused,
                        &mut form,
                        id,
                    );
                }
            }
        } else if let FieldValue::Boolean(_) = dialog.values[display.field] {
            // The title and description label this boolean too: clicking
            // any of them toggles the same control as clicking its mark.
            let area = if dialog.is_plan_review() {
                chunks[1]
            } else {
                chunks[0].union(chunks[1])
            };
            form.register(id, ControlKind::Checkbox, area, true);
        }
    } else {
        render_focus(frame, focus_area, &focus, focus_scroll, focused);
    }
    {
        let mut form = dialog.form.borrow_mut();
        ButtonRow::render(frame, chunks[2], &dialog.actions(), &mut form);
        form.end_frame(if dialog.confirming_unanswered {
            ElicitationControl::GoBack
        } else {
            ElicitationControl::Field(dialog.current_question)
        });
    }
    let scroll_help = if maximum_scroll > 0 {
        let start = message_scroll.saturating_add(1);
        let end = message_scroll
            .saturating_add(chunks[0].height)
            .min(total_lines);
        Some(format!(
            "Text {start}–{end}/{total_lines} · PgUp/PgDn or wheel scroll · Space toggle · Enter answer · Alt+←/→ questions"
        ))
    } else {
        None
    };
    let footer = if let Some(error) = dialog.error.as_deref() {
        error.to_owned()
    } else if let Some(scroll_help) = scroll_help {
        scroll_help
    } else if dialog.confirming_unanswered {
        "Enter confirm · Esc go back".to_owned()
    } else {
        "Tab controls · ↑/↓ choose · Space toggle · Enter answer · Alt+←/→ questions".to_owned()
    };
    frame.render_widget(
        Paragraph::new(if focused {
            footer.as_str()
        } else {
            "Click to answer"
        })
        .style(Style::default().fg(if dialog.error.is_some() && focused {
            theme::palette().error
        } else {
            theme::palette().muted
        })),
        chunks[3],
    );
}

fn wrapped_prefix_rows(message: &str, width: u16, line_count: usize) -> u16 {
    let mut rows = 0usize;
    for line in message.split('\n').take(line_count) {
        rows = rows.saturating_add(wrapped_row_count(line, width));
    }
    u16::try_from(rows).unwrap_or(u16::MAX)
}

fn message_position_at_row(message: &str, width: u16, row: usize) -> (usize, usize) {
    let mut rows = 0usize;
    for (line_index, line) in message.split('\n').enumerate() {
        let line_rows = wrapped_row_count(line, width).max(1);
        if row < rows.saturating_add(line_rows) {
            return (
                line_index,
                grapheme_offset_for_wrapped_row(line, usize::from(width), row.saturating_sub(rows)),
            );
        }
        rows = rows.saturating_add(line_rows);
    }
    (message.split('\n').count().saturating_sub(1), 0)
}

struct FocusContent<'a> {
    /// The field position and title. It is pinned above the scrolling field
    /// body so a control can never be shown without the question it answers.
    title: Option<Line<'a>>,
    lines: Vec<Line<'a>>,
    text_cursor: Option<(u16, usize)>,
    focused_row: Option<usize>,
    centered: bool,
    option_rows: Vec<Option<usize>>,
}

impl FocusContent<'_> {
    /// Rows the pinned title and the field body occupy at this width.
    fn wrapped_height(&self, width: u16) -> u16 {
        let body = u16::try_from(
            Paragraph::new(self.lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(width),
        )
        .unwrap_or(u16::MAX)
        .max(1);
        body.saturating_add(u16::from(self.title.is_some()))
    }
}

fn focus_content(dialog: &ElicitationDialog) -> FocusContent<'_> {
    let focus = dialog.current_question;
    let Some(display) = dialog
        .display_fields
        .get(focus)
        .copied()
        .filter(|_| !dialog.confirming_unanswered)
    else {
        let label = match focus.saturating_sub(dialog.display_fields.len()) {
            0 => "Submit these answers",
            1 => "Skip this question and let the agent continue",
            _ => "Cancel this question",
        };
        return FocusContent {
            title: None,
            lines: vec![Line::from(label)],
            text_cursor: None,
            focused_row: None,
            centered: true,
            option_rows: vec![],
        };
    };
    let field = &dialog.request.fields[display.field];
    let required = if field.required { " (required)" } else { "" };
    let title = Line::from(vec![
        Span::styled(
            format!("{}/{}  ", focus + 1, dialog.display_fields.len()),
            Style::default().fg(theme::palette().muted),
        ),
        Span::styled(
            format!("{}{}", field.title, required),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);
    let mut lines = Vec::new();
    if dialog.is_plan_review()
        && let Some(description) = &field.description
    {
        lines.push(Line::styled(
            description.as_str(),
            Style::default().fg(theme::palette().text),
        ));
    }
    let mut option_rows = vec![];
    let mut text_cursor = None;
    let mut focused_row = None;
    match (&field.kind, &dialog.values[display.field]) {
        (_, FieldValue::Text(value)) => {
            let shown = if field.secret {
                "•".repeat(value.chars().count())
            } else {
                value.to_string()
            };
            lines.push(Line::raw(""));
            let input_line = lines.len() as u16;
            focused_row = Some(lines.len());
            lines.push(Line::styled(
                format!("> {shown}"),
                Style::default().fg(theme::palette().accent),
            ));
            text_cursor = Some((
                input_line,
                value.value()[..value.cursor()].graphemes(true).count(),
            ));
        }
        (ElicitationFieldKind::SingleSelect { options, .. }, FieldValue::Single(selected)) => {
            let custom_active = display
                .custom
                .is_some_and(|custom| dialog.active_custom_fields.contains(&custom));
            for (index, option) in options.iter().enumerate() {
                let cursor = dialog.option_cursors[display.field] == index;
                let custom_replaces_selection = display.custom_option.is_none();
                let marker =
                    if (!custom_active || !custom_replaces_selection) && *selected == Some(index) {
                        "●"
                    } else {
                        "○"
                    };
                let style = if cursor {
                    Style::default()
                        .fg(theme::palette().accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                if cursor {
                    focused_row = Some(lines.len());
                }
                option_rows.resize(lines.len(), None);
                option_rows.push(Some(index));
                lines.push(Line::styled(format!("{marker} {}", option.title), style));
                if cursor {
                    if let Some(description) = &option.description {
                        lines.push(Line::styled(
                            format!("    {description}"),
                            Style::default().fg(theme::palette().text),
                        ));
                    }
                    if let Some(preview) = &option.preview {
                        lines.push(Line::styled(
                            format!("    {preview}"),
                            Style::default().fg(theme::palette().muted),
                        ));
                    }
                    if display.custom_option == Some(index)
                        && let Some(custom) = display.custom
                    {
                        render_custom_text(&mut lines, &mut text_cursor, dialog, custom);
                    }
                }
            }
            if display.custom.is_some() && display.custom_option.is_none() {
                option_rows.resize(lines.len(), None);
                option_rows.push(Some(options.len()));
            }
            render_custom_answer(
                &mut lines,
                &mut text_cursor,
                dialog,
                display,
                options.len(),
                "○",
                "●",
            );
        }
        (ElicitationFieldKind::MultiSelect { options, .. }, FieldValue::Multi(selected)) => {
            let custom_active = display
                .custom
                .is_some_and(|custom| dialog.active_custom_fields.contains(&custom));
            for (index, option) in options.iter().enumerate() {
                if dialog.option_cursors[display.field] == index {
                    focused_row = Some(lines.len());
                }
                let marker = Checkbox::marker(!custom_active && selected.contains(&index));
                let style = if dialog.option_cursors[display.field] == index {
                    Style::default()
                        .fg(theme::palette().accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                option_rows.resize(lines.len(), None);
                option_rows.push(Some(index));
                lines.push(Line::styled(format!("{marker} {}", option.title), style));
            }
            if display.custom.is_some() && display.custom_option.is_none() {
                option_rows.resize(lines.len(), None);
                option_rows.push(Some(options.len()));
            }
            render_custom_answer(
                &mut lines,
                &mut text_cursor,
                dialog,
                display,
                options.len(),
                Checkbox::marker(false),
                Checkbox::marker(true),
            );
        }
        (ElicitationFieldKind::Boolean { .. }, FieldValue::Boolean(selected)) => {
            focused_row = Some(lines.len());
            lines.push(Line::styled(
                format!(
                    "{} {}",
                    Checkbox::marker(*selected),
                    if *selected { "Yes" } else { "No" }
                ),
                Style::default().fg(theme::palette().accent),
            ));
        }
        _ => {}
    }
    let focused_row = focused_row.or_else(|| text_cursor.map(|(line, _)| usize::from(line)));
    FocusContent {
        title: dialog.is_plan_review().then_some(title),
        lines,
        text_cursor,
        focused_row,
        centered: false,
        option_rows,
    }
}

fn render_focus(
    frame: &mut Frame,
    area: Rect,
    content: &FocusContent<'_>,
    scroll: u16,
    focused: bool,
) {
    let paragraph = Paragraph::new(content.lines.clone())
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .alignment(if content.centered {
            Alignment::Center
        } else {
            Alignment::Left
        });
    frame.render_widget(paragraph, area);
    if focused
        && let Some((line, column)) = content.text_cursor
        && area.width > 2
        && area.height > 0
    {
        let line = usize::from(line);
        let prefix_rows = Paragraph::new(content.lines[..line].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(area.width);
        let text = content.lines[line].to_string();
        let cursor_grapheme = 2usize.saturating_add(column);
        let cursor_byte = text
            .grapheme_indices(true)
            .nth(cursor_grapheme)
            .map_or(text.len(), |(offset, _)| offset);
        let (cursor_column, cursor_row) =
            input_cursor_visual_position(&text, cursor_byte, usize::from(area.width));
        let row = prefix_rows
            .saturating_add(cursor_row)
            .saturating_sub(usize::from(scroll));
        if row < usize::from(area.height) {
            frame.set_cursor_position((
                area.x
                    + u16::try_from(cursor_column)
                        .unwrap_or(u16::MAX)
                        .min(area.width - 1),
                area.y + u16::try_from(row).unwrap_or(u16::MAX),
            ));
        }
    }
}

fn render_custom_answer(
    lines: &mut Vec<Line<'_>>,
    text_cursor: &mut Option<(u16, usize)>,
    dialog: &ElicitationDialog,
    display: DisplayField,
    option_count: usize,
    unselected_marker: &str,
    selected_marker: &str,
) {
    let Some(custom_index) = display.custom else {
        return;
    };
    if display.custom_option.is_some() {
        return;
    }
    let custom = &dialog.request.fields[custom_index];
    let focused = dialog.option_cursors[display.field] == option_count;
    let active = dialog.active_custom_fields.contains(&custom_index);
    let marker = if active {
        selected_marker
    } else {
        unselected_marker
    };
    let style = if focused {
        Style::default()
            .fg(theme::palette().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    lines.push(Line::styled(format!("{marker} {}", custom.title), style));
    if !focused {
        return;
    }
    render_custom_text(lines, text_cursor, dialog, custom_index);
}

fn render_custom_text(
    lines: &mut Vec<Line<'_>>,
    text_cursor: &mut Option<(u16, usize)>,
    dialog: &ElicitationDialog,
    custom_index: usize,
) {
    let custom = &dialog.request.fields[custom_index];
    let FieldValue::Text(value) = &dialog.values[custom_index] else {
        unreachable!("custom answer fields contain text values")
    };
    if let Some(description) = &custom.description {
        lines.push(Line::styled(
            format!("    {description}"),
            Style::default().fg(theme::palette().text),
        ));
    }
    let shown = if custom.secret {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let input_line = lines.len() as u16;
    lines.push(Line::styled(
        format!("> {shown}"),
        Style::default().fg(theme::palette().accent),
    ));
    *text_cursor = Some((
        input_line,
        value.value()[..value.cursor()].graphemes(true).count(),
    ));
}

#[cfg(test)]
mod tests;
