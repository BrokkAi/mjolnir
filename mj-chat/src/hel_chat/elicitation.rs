//! Modal editor for ACP form elicitations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use rat_event::ConsumedEvent;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

use crate::components::{
    ButtonRow, ChoiceList, ControlKind, FieldEdit, Form, Interaction, TextField,
};
use crate::hel_selection::{
    ContentPos, FrameSurfaces, SelectionRange, SurfaceFrame, SurfaceId, extract_rows,
};
use crate::hel_text_input::TextInput;
use hel::hel_elicitation::{
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

#[derive(Debug, Clone)]
enum FieldValue {
    Text(TextInput),
    Single(Option<usize>),
    Multi(BTreeSet<usize>),
    Boolean(bool),
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
}

/// A process-local copy of an unanswered form. The request is retained in the
/// snapshot deliberately: an id can be reused by a harness for a different
/// form, and local answers must never be applied to that new form.
#[derive(Debug, Clone)]
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
        let mut form = Form::new();
        for (index, display) in display_fields.iter().copied().enumerate() {
            form.declare(
                ElicitationControl::Field(index),
                display_control_kind(&request, &values, &option_cursors, display),
            );
        }
        form.declare(ElicitationControl::Submit, ControlKind::Button);
        form.declare(ElicitationControl::Skip, ControlKind::Button);
        form.declare(ElicitationControl::Cancel, ControlKind::Button);
        form.end_frame(if display_fields.is_empty() {
            ElicitationControl::Submit
        } else {
            ElicitationControl::Field(0)
        });
        Self {
            request,
            values,
            option_cursors,
            display_fields,
            active_custom_fields,
            form: RefCell::new(form),
            error: None,
            message_scroll: Cell::new(0),
            message_page_height: Cell::new(0),
            message_max_scroll: Cell::new(0),
            message_area: Cell::new(None),
            message_anchor: Cell::new(None),
            message_anchor_width: Cell::new(0),
            focus_scroll: Cell::new(0),
        }
    }

    pub(super) fn request(&self) -> &ElicitationRequest {
        &self.request
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
        for (index, display) in dialog.display_fields.iter().copied().enumerate() {
            dialog.form.borrow_mut().declare(
                ElicitationControl::Field(index),
                display_control_kind(
                    &dialog.request,
                    &dialog.values,
                    &dialog.option_cursors,
                    display,
                ),
            );
        }
        dialog.focus_control(draft.focus.min(dialog.display_fields.len() + 2));
        dialog.message_scroll = Cell::new(draft.message_scroll);
        dialog.message_anchor = Cell::new(draft.message_anchor);
        dialog.message_anchor_width = Cell::new(draft.message_anchor_width);
        dialog.focus_scroll = Cell::new(draft.focus_scroll);
        Some(dialog)
    }

    pub(super) fn paste(&mut self, text: &str) {
        let text = crate::hel_text_input::single_line_paste(&sanitize_terminal_text(text));
        let Some((field, custom)) = self.editable_field() else {
            return;
        };
        if custom {
            self.active_custom_fields.insert(field);
        }
        if let Some(FieldValue::Text(value)) = self.values.get_mut(field) {
            value.insert_str(&text);
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
        if self.is_plan_review() {
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
        }
        self.error = None;
        // Inline custom answers retain their parent question's focus. Text
        // editing and option navigation are distinct operations on that page.
        if key.kind != crossterm::event::KeyEventKind::Release
            && self.editable_field().is_some_and(|(_, custom)| custom)
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
        if result.outcome.is_consumed() {
            return None;
        }
        None
    }

    fn apply_interaction(
        &mut self,
        interaction: Interaction<ElicitationControl>,
    ) -> Option<ElicitationResponse> {
        match interaction {
            Interaction::Activate(ElicitationControl::Submit) => self.accept(),
            Interaction::Activate(ElicitationControl::Skip) => Some(ElicitationResponse::Decline),
            Interaction::Activate(ElicitationControl::Cancel) | Interaction::Cancel => {
                Some(ElicitationResponse::Cancel)
            }
            Interaction::Activate(ElicitationControl::Field(_)) => {
                self.advance_focus();
                None
            }
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
        if result.outcome.is_consumed() {
            return None;
        }
        if !self.is_plan_review()
            || !self
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

    pub(super) fn cancel_component_pointer(&self) {
        self.form.borrow_mut().cancel_pointer();
    }

    pub(super) fn reset_component_geometry(&self) {
        self.form.borrow_mut().reset_geometry();
    }

    fn focus_index(&self) -> usize {
        match self.form.borrow().focused() {
            Some(ElicitationControl::Field(index)) => index,
            Some(ElicitationControl::Submit) => self.display_fields.len(),
            Some(ElicitationControl::Skip) => self.display_fields.len() + 1,
            Some(ElicitationControl::Cancel) => self.display_fields.len() + 2,
            None => 0,
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
        self.form.borrow_mut().focus(control);
    }

    fn advance_focus(&mut self) {
        let event = crossterm::event::Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let _ = self.form.borrow_mut().handle(&event);
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
            *value = (selected < option_count).then_some(selected);
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
        hel::hel_acp::is_plan_review_id(&self.request.id)
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
        for line in self.request.message.split('\n') {
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
        self.message_scroll.set(next as u16);
        // A user scroll establishes a new logical anchor. Subsequent redraws
        // and resizes retain that source position instead of repeatedly
        // replacing it with whichever wrapped row happens to be at the top.
        if self.message_anchor_width.get() > 0 {
            self.message_anchor.set(Some(message_position_at_row(
                &self.request.message,
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
        if custom {
            self.active_custom_fields.insert(field);
        }
        let Some(FieldValue::Text(value)) = self.values.get_mut(field) else {
            unreachable!("editable fields contain text values")
        };
        value.handle_key(key);
    }

    fn accept(&mut self) -> Option<ElicitationResponse> {
        let mut content = BTreeMap::new();
        for (display_index, display) in self.display_fields.iter().copied().enumerate() {
            let active_custom = display
                .custom
                .filter(|custom| self.active_custom_fields.contains(custom));
            let field_indices = match (active_custom, display.custom_option) {
                (Some(custom), Some(_)) => vec![display.field, custom],
                (Some(custom), None) => vec![custom],
                (None, _) => vec![display.field],
            };
            for field_index in field_indices {
                let field = &self.request.fields[field_index];
                let value = &self.values[field_index];
                match validated_value(field, value) {
                    Ok(Some(value)) => {
                        content.insert(field.id.clone(), value);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.error = Some(error);
                        self.focus_control(display_index);
                        if field_index != display.field
                            && let Some(option_count) =
                                select_option_count(&self.request.fields[display.field])
                        {
                            self.option_cursors[display.field] =
                                display.custom_option.unwrap_or(option_count);
                        }
                        return None;
                    }
                }
            }
        }
        Some(ElicitationResponse::Accept { content })
    }

    fn editable_field(&self) -> Option<(usize, bool)> {
        let display = self.display_fields.get(self.focus_index())?;
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
            if value.is_empty() {
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
            if value.is_empty() {
                return if field.required { missing() } else { Ok(None) };
            }
            ElicitationValue::Integer(
                value
                    .parse::<i64>()
                    .map_err(|_| format!("{} must be an integer", field.title))?,
            )
        }
        (ElicitationFieldKind::Number { .. }, FieldValue::Text(value)) => {
            if value.is_empty() {
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
    let title = dialog.request.title.as_deref().unwrap_or("Agent question");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let focus = focus_content(dialog);
    let natural_focus_height = u16::try_from(
        Paragraph::new(focus.lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width),
    )
    .unwrap_or(u16::MAX)
    .max(1);
    let footer_height = inner.height.min(1);
    // In a compact question pane one button row is enough: the second row is
    // more valuable to the message and focused control, which can each scroll
    // their content independently.
    let button_rows = if inner.height <= 4 { 1 } else { 2 };
    let buttons_height = inner.height.saturating_sub(footer_height).min(button_rows);
    let body_height = inner.height.saturating_sub(footer_height + buttons_height);
    let (message_height, focus_height) = if dialog.is_plan_review() {
        let focus_height = natural_focus_height
            .min(body_height.saturating_sub(1).max(1))
            .min(body_height);
        (body_height.saturating_sub(focus_height), focus_height)
    } else {
        // Keep the answer controls inside tiny session areas too. The form
        // body receives whatever remains after the two button rows and the
        // footer; its own scroll keeps a focused option reachable.
        let message_height = body_height.min(3).min(body_height.saturating_sub(1));
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
    let message = Paragraph::new(dialog.request.message.as_str()).wrap(Wrap { trim: true });
    let total_lines = u16::try_from(message.line_count(chunks[0].width)).unwrap_or(u16::MAX);
    let maximum_scroll = total_lines.saturating_sub(chunks[0].height);
    let anchored_scroll = (dialog.message_anchor_width.get() != chunks[0].width)
        .then(|| dialog.message_anchor.get())
        .flatten()
        .map(|(line, row)| {
            let source = dialog.request.message.split('\n').nth(line).unwrap_or("");
            wrapped_prefix_rows(&dialog.request.message, chunks[0].width, line).saturating_add(
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
            &dialog.request.message,
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
    let focused_field = dialog.focus_index();
    let field_index = focused_field.min(dialog.display_fields.len().saturating_sub(1));
    {
        let mut form = dialog.form.borrow_mut();
        form.begin_frame();
        for (index, display) in dialog.display_fields.iter().copied().enumerate() {
            let field_area = if index == focused_field {
                chunks[1]
            } else {
                Rect::default()
            };
            form.register(
                ElicitationControl::Field(index),
                display_control_kind(
                    &dialog.request,
                    &dialog.values,
                    &dialog.option_cursors,
                    display,
                ),
                field_area,
                true,
            );
        }
        form.register(
            ElicitationControl::Submit,
            ControlKind::Button,
            chunks[2],
            true,
        );
        form.register(
            ElicitationControl::Skip,
            ControlKind::Button,
            chunks[2],
            true,
        );
        form.register(
            ElicitationControl::Cancel,
            ControlKind::Button,
            chunks[2],
            true,
        );
    }
    let focus_rows = u16::try_from(
        Paragraph::new(focus.lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(chunks[1].width),
    )
    .unwrap_or(u16::MAX)
    .max(1);
    let focus_max_scroll = focus_rows.saturating_sub(chunks[1].height);
    let target_row = focus.focused_row.map_or(0, |line| {
        let prefix_rows = Paragraph::new(focus.lines[..line].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(chunks[1].width);
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
                input_cursor_visual_position(&text, cursor_byte, usize::from(chunks[1].width)).1
            });
        prefix_rows.saturating_add(cursor_row)
    });
    let mut focus_scroll = dialog.focus_scroll.get().min(focus_max_scroll);
    let target_row = target_row.min(usize::from(u16::MAX)) as u16;
    if target_row < focus_scroll {
        focus_scroll = target_row;
    } else if target_row >= focus_scroll.saturating_add(chunks[1].height) {
        focus_scroll = target_row
            .saturating_sub(chunks[1].height.saturating_sub(1))
            .min(focus_max_scroll);
    }
    dialog.focus_scroll.set(focus_scroll);
    if let Some(display) = dialog.display_fields.get(dialog.focus_index()).copied() {
        let id = ElicitationControl::Field(field_index);
        let mut form = dialog.form.borrow_mut();
        if select_option_count(&dialog.request.fields[display.field]).is_some() {
            ChoiceList::render_wrapped(
                frame,
                chunks[1],
                &focus.lines,
                &focus.option_rows,
                dialog.option_cursors[display.field],
                focus_scroll,
                &mut form,
                id,
            );
        } else {
            render_focus(frame, chunks[1], &focus, focus_scroll, focused);
        }
        if let Some((line, _)) = focus.text_cursor {
            let prefix = Paragraph::new(focus.lines[..usize::from(line)].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(chunks[1].width);
            // The editor is a single row; scrolling belongs to the field,
            // while the surrounding option descriptions keep their wrapping.
            let row = prefix.saturating_sub(usize::from(focus_scroll));
            if row < usize::from(chunks[1].height) {
                let field = if matches!(dialog.values[display.field], FieldValue::Text(_)) {
                    display.field
                } else {
                    display
                        .custom
                        .expect("inline text belongs to a custom answer")
                };
                if let FieldValue::Text(input) = &dialog.values[field] {
                    let area = Rect::new(
                        chunks[1].x.saturating_add(2),
                        chunks[1].y.saturating_add(row as u16),
                        chunks[1].width.saturating_sub(2),
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
            let row = focus
                .focused_row
                .unwrap_or(0)
                .saturating_sub(usize::from(focus_scroll));
            let area = if row < usize::from(chunks[1].height) {
                Rect::new(
                    chunks[1].x,
                    chunks[1].y + row as u16,
                    chunks[1].width.min(5),
                    1,
                )
            } else {
                Rect::default()
            };
            form.register(id, ControlKind::Checkbox, area, true);
        }
    } else {
        render_focus(frame, chunks[1], &focus, focus_scroll, focused);
    }
    {
        let mut form = dialog.form.borrow_mut();
        ButtonRow::render(
            frame,
            chunks[2],
            &[
                (ElicitationControl::Submit, "Submit", true),
                (ElicitationControl::Skip, "Skip", true),
                (ElicitationControl::Cancel, "Cancel", true),
            ],
            &mut form,
        );
        form.end_frame(ElicitationControl::Field(0));
    }
    let scroll_help = if dialog.is_plan_review() && maximum_scroll > 0 {
        let start = message_scroll.saturating_add(1);
        let end = message_scroll
            .saturating_add(chunks[0].height)
            .min(total_lines);
        Some(format!(
            "F6/Shift-F6 panes · Plan {start}–{end}/{total_lines} · PgUp/PgDn or wheel scroll · Tab fields/buttons · ↑/↓ choose · Enter continue"
        ))
    } else {
        None
    };
    let footer = if let Some(error) = dialog.error.as_deref() {
        format!("F6/Shift-F6 panes · {error}")
    } else if let Some(scroll_help) = scroll_help {
        scroll_help
    } else {
        "F6/Shift-F6 panes · Tab fields/buttons · ↑/↓ choose · Space toggle · Enter continue · Esc cancel".to_owned()
    };
    frame.render_widget(
        Paragraph::new(if focused {
            footer.as_str()
        } else {
            "Click to answer · F6 to change pane"
        })
        .style(Style::default().fg(if dialog.error.is_some() && focused {
            Color::Red
        } else {
            Color::DarkGray
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
    lines: Vec<Line<'a>>,
    text_cursor: Option<(u16, usize)>,
    focused_row: Option<usize>,
    centered: bool,
    option_rows: Vec<Option<usize>>,
}

fn focus_content(dialog: &ElicitationDialog) -> FocusContent<'_> {
    let focus = dialog.focus_index();
    let Some(display) = dialog.display_fields.get(focus).copied() else {
        let label = match focus.saturating_sub(dialog.display_fields.len()) {
            0 => "Submit these answers",
            1 => "Skip this question and let the agent continue",
            _ => "Cancel this question",
        };
        return FocusContent {
            lines: vec![Line::from(label)],
            text_cursor: None,
            focused_row: None,
            centered: true,
            option_rows: vec![],
        };
    };
    let field = &dialog.request.fields[display.field];
    let required = if field.required { " (required)" } else { "" };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{}/{}  ", focus + 1, dialog.display_fields.len()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{}{}", field.title, required),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some(description) = &field.description {
        lines.push(Line::styled(
            description.as_str(),
            Style::default().fg(Color::Gray),
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
            lines.push(Line::styled(
                format!("> {shown}"),
                Style::default().fg(Color::Cyan),
            ));
            focused_row = Some(2 + usize::from(field.description.is_some()));
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
                        .fg(Color::Cyan)
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
                            Style::default().fg(Color::Gray),
                        ));
                    }
                    if let Some(preview) = &option.preview {
                        lines.push(Line::styled(
                            format!("    {preview}"),
                            Style::default().fg(Color::DarkGray),
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
                let marker = if !custom_active && selected.contains(&index) {
                    "☑"
                } else {
                    "☐"
                };
                let style = if dialog.option_cursors[display.field] == index {
                    Style::default()
                        .fg(Color::Cyan)
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
                "☐",
                "☑",
            );
        }
        (ElicitationFieldKind::Boolean { .. }, FieldValue::Boolean(selected)) => {
            focused_row = Some(lines.len());
            lines.push(Line::styled(
                if *selected { "☑ Yes" } else { "☐ No" },
                Style::default().fg(Color::Cyan),
            ));
        }
        _ => {}
    }
    let focused_row = focused_row.or_else(|| text_cursor.map(|(line, _)| usize::from(line)));
    FocusContent {
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
            .fg(Color::Cyan)
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
            Style::default().fg(Color::Gray),
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
        Style::default().fg(Color::Cyan),
    ));
    *text_cursor = Some((
        input_line,
        value.value()[..value.cursor()].graphemes(true).count(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_elicitation::ElicitationOption;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn request(kind: ElicitationFieldKind, required: bool) -> ElicitationRequest {
        ElicitationRequest {
            id: "ask-1".into(),
            message: "Choose an architecture".into(),
            title: None,
            description: None,
            fields: vec![ElicitationField {
                id: "question_0".into(),
                title: "Architecture".into(),
                description: None,
                required,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind,
            }],
        }
    }

    fn paired_request(question_count: usize, multi_select: bool) -> ElicitationRequest {
        let mut fields = Vec::new();
        for index in 0..question_count {
            let id = format!("question_{index}");
            let options = vec![
                ElicitationOption {
                    value: "alpha".into(),
                    title: "Alpha".into(),
                    description: Some("Choose alpha".into()),
                    preview: None,
                },
                ElicitationOption {
                    value: "beta".into(),
                    title: "Beta".into(),
                    description: Some("Choose beta".into()),
                    preview: None,
                },
            ];
            fields.push(ElicitationField {
                id: id.clone(),
                title: format!("Question {}", index + 1),
                description: Some(format!("Prompt {}", index + 1)),
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: if multi_select {
                    ElicitationFieldKind::MultiSelect {
                        options,
                        default: Vec::new(),
                        min_items: None,
                        max_items: None,
                    }
                } else {
                    ElicitationFieldKind::SingleSelect {
                        options,
                        default: None,
                    }
                },
            });
            fields.push(ElicitationField {
                id: format!("{id}__other"),
                title: "Other".into(),
                description: Some(
                    "Type your own answer instead of choosing an option above.".into(),
                ),
                required: false,
                secret: false,
                custom_answer_for: Some(id),
                custom_answer_option: None,
                kind: ElicitationFieldKind::Text {
                    default: None,
                    min_length: None,
                    max_length: None,
                    pattern: None,
                    format: None,
                },
            });
        }
        ElicitationRequest {
            id: "ask-paired".into(),
            message: "Input requested".into(),
            title: None,
            description: None,
            fields,
        }
    }

    fn rendered(dialog: &ElicitationDialog) -> String {
        rendered_with_surfaces(dialog, &mut FrameSurfaces::new())
    }

    fn rendered_in_pane(dialog: &ElicitationDialog, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_elicitation_in(frame, dialog, &mut FrameSurfaces::new(), area, true);
            })
            .expect("render question pane");
        terminal.backend().buffer().clone()
    }

    fn buffer_row(buffer: &Buffer, row: u16, start: u16, end: u16) -> String {
        (start..end)
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    #[test]
    fn pane_resize_preserves_the_visible_word_inside_an_indented_paragraph() {
        for (indent, old_width, new_width) in [
            ("", 42, 62),
            ("             ", 33, 57),
            ("                       ", 57, 37),
            (
                "                                                                                ",
                42,
                62,
            ),
        ] {
            let message = format!(
                "{indent}{}",
                (0..200)
                    .map(|n| format!("word{n:03}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let dialog = plan_review_message(&message);
            rendered_in_pane(&dialog, old_width, 18);
            dialog.scroll_message(8);
            let before = rendered_in_pane(&dialog, old_width, 18);
            let area = dialog.message_area.get().expect("message area");
            let before_row = buffer_row(&before, area.y, area.x, area.right());
            let anchor = before_row.split_whitespace().next().expect("visible word");
            let after = rendered_in_pane(&dialog, new_width, 18);
            let area = dialog.message_area.get().expect("resized message area");
            let after_row = buffer_row(&after, area.y, area.x, area.right());
            assert!(
                after_row.contains(anchor),
                "anchor {anchor:?} disappeared from top row {after_row:?}, indent length {}",
                indent.len()
            );
        }
    }

    #[test]
    fn smallest_question_pane_keeps_other_and_its_draft_visible() {
        let mut dialog = ElicitationDialog::new(paired_request(1, true));
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        dialog.paste("custom draft");
        let buffer = rendered_in_pane(&dialog, 60, 6);
        let text = (0..6)
            .map(|row| buffer_row(&buffer, row, 0, 60))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("custom draft"),
            "focused custom input is hidden: {text}"
        );
        assert!(text.contains("Submit"), "submit is hidden: {text}");
        assert!(
            text.contains("F6/Shift-F6"),
            "pane shortcut is hidden: {text}"
        );
    }

    #[test]
    fn six_row_plan_pane_keeps_text_and_page_down_actionable() {
        let message = (0..80)
            .map(|index| format!("plan-{index:03}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut dialog = plan_review_message(&message);
        let before = rendered_in_pane(&dialog, 60, 6);
        let before_text = (0..6)
            .map(|row| buffer_row(&before, row, 0, 60))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            before_text.contains("plan-000"),
            "plan text is hidden: {before_text}"
        );
        assert!(
            before_text.contains("Submit"),
            "submit is hidden: {before_text}"
        );
        assert!(
            before_text.contains("F6/Shift-F6"),
            "pane shortcut is hidden: {before_text}"
        );

        dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
        let after = rendered_in_pane(&dialog, 60, 6);
        let after_text = (0..6)
            .map(|row| buffer_row(&after, row, 0, 60))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !after_text.contains("plan-000"),
            "PageDown did not scroll: {after_text}"
        );
        assert!(
            after_text.contains("plan-"),
            "scrolled plan text is hidden: {after_text}"
        );
    }

    #[test]
    fn repeated_resizes_retain_one_logical_message_anchor() {
        let message = (0..400)
            .map(|index| format!("word-{index:03}"))
            .collect::<Vec<_>>()
            .join(" ");
        let dialog = plan_review_message(&message);
        rendered_in_pane(&dialog, 42, 18);
        dialog.scroll_message(8);
        let anchor = dialog.message_anchor.get().expect("scroll anchor");
        rendered_in_pane(&dialog, 34, 18);
        assert_eq!(dialog.message_anchor.get(), Some(anchor));
        rendered_in_pane(&dialog, 62, 18);
        assert_eq!(dialog.message_anchor.get(), Some(anchor));
        rendered_in_pane(&dialog, 42, 18);
        assert_eq!(dialog.message_anchor.get(), Some(anchor));
    }

    #[test]
    fn pane_resize_keeps_a_word_at_an_exact_wrap_boundary() {
        let message = (0..400).map(|n| format!("w{n:04}")).collect::<String>();
        let dialog = plan_review_message(&message);
        rendered_in_pane(&dialog, 42, 18);
        dialog.scroll_message(8);
        let before = rendered_in_pane(&dialog, 42, 18);
        let area = dialog.message_area.get().unwrap();
        let before_row = buffer_row(&before, area.y, area.x, area.right());
        let after = rendered_in_pane(&dialog, 34, 18);
        let area = dialog.message_area.get().unwrap();
        let after_row = buffer_row(&after, area.y, area.x, area.right());
        assert!(
            after_row.starts_with(&before_row[..8]),
            "reading position moved from {before_row:?} to {after_row:?}"
        );
    }

    fn rendered_with_surfaces(dialog: &ElicitationDialog, surfaces: &mut FrameSurfaces) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_elicitation_in(frame, dialog, surfaces, area, true);
            })
            .expect("render elicitation");
        let buffer = terminal.backend().buffer();
        (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn plan_review(line_count: usize) -> ElicitationDialog {
        plan_review_message(
            &(0..line_count)
                .map(|line| format!("plan-line-{line:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn plan_review_message(message: &str) -> ElicitationDialog {
        let mut request = request(
            ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: "implement".into(),
                        title: "Implement".into(),
                        description: Some("Approve and continue".into()),
                        preview: None,
                    },
                    ElicitationOption {
                        value: "revise".into(),
                        title: "Revise".into(),
                        description: None,
                        preview: None,
                    },
                ],
                default: Some("implement".into()),
            },
            true,
        );
        request.id = "plan-review-test".into();
        request.title = Some("Plan review".into());
        request.fields.push(ElicitationField {
            id: "feedback".into(),
            title: "Revision feedback".into(),
            description: Some("Describe what the agent should change".into()),
            required: false,
            secret: false,
            custom_answer_for: Some("question_0".into()),
            custom_answer_option: Some("revise".into()),
            kind: ElicitationFieldKind::Text {
                default: None,
                min_length: None,
                max_length: None,
                pattern: None,
                format: None,
            },
        });
        request.message = message.to_owned();
        ElicitationDialog::new(request)
    }

    /// The pane a plan-review dialog registered on its last frame.
    fn message_pane(dialog: &ElicitationDialog) -> SurfaceFrame {
        let mut surfaces = FrameSurfaces::new();
        rendered_with_surfaces(dialog, &mut surfaces);
        *surfaces
            .surface(SurfaceId::ElicitationMessage)
            .expect("the message pane is registered")
    }

    fn range(start: (usize, u16), end: (usize, u16)) -> SelectionRange {
        SelectionRange {
            start: ContentPos::new(start.0, start.1),
            end: ContentPos::new(end.0, end.1),
        }
    }

    #[test]
    fn plan_review_preserves_content_outside_scoped_bounds() {
        const WIDTH: u16 = 100;
        const HEIGHT: u16 = 30;
        let dialog = plan_review(20);
        let screen = Rect::new(0, 0, WIDTH, HEIGHT);
        let question_bounds = Rect::new(10, 5, 80, 20);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");

        terminal
            .draw(|frame| {
                let background = (0..HEIGHT)
                    .map(|_| Line::raw("X".repeat(usize::from(WIDTH))))
                    .collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(background), frame.area());
                render_elicitation_in(
                    frame,
                    &dialog,
                    &mut FrameSurfaces::new(),
                    question_bounds,
                    true,
                );
            })
            .expect("render plan review over background");

        let buffer = terminal.backend().buffer();
        for y in screen.y..screen.bottom() {
            for x in screen.x..screen.right() {
                let position = Position::new(x, y);
                if !question_bounds.contains(position) {
                    assert_eq!(
                        buffer[(x, y)].symbol(),
                        "X",
                        "scoped question changed content outside ({x}, {y})"
                    );
                }
            }
        }
        assert_eq!(buffer[(question_bounds.x, question_bounds.y)].symbol(), "┌");
        assert_eq!(
            buffer[(question_bounds.right() - 1, question_bounds.y)].symbol(),
            "┐"
        );
    }

    /// The extractor maps a selection through per-line row counts, so those
    /// counts have to add up to what the pane's own paragraph reports.
    #[test]
    fn wrapped_row_counts_of_source_lines_sum_to_the_paragraphs_line_count() {
        let message = concat!(
            "a short line\n",
            "\n",
            "a considerably longer line that the plan pane has to wrap over several rows ",
            "before it finally runs out of words to place\n",
            "tiny\n",
            "\n",
            "\n",
            "another long one, long enough that it also wraps more than once at any of ",
            "these widths"
        );

        for width in [17u16, 33, 80] {
            let composed = message
                .split('\n')
                .map(|line| wrapped_row_count(line, width))
                .sum::<usize>();
            let whole = Paragraph::new(message)
                .wrap(Wrap { trim: true })
                .line_count(width);
            assert_eq!(
                composed, whole,
                "per-line rows must compose at width {width}"
            );
        }
    }

    /// The point of the feature: a range over whole logical lines comes back
    /// as the plan wrote them, without the newlines word wrap introduced, even
    /// though most of those rows were scrolled out of the pane.
    #[test]
    fn copying_a_plan_range_past_the_viewport_returns_the_unwrapped_source_lines() {
        let paragraph = "This step is long enough that the plan pane wraps it over several rows, which is exactly the fugliness copying is meant to undo.";
        let message = (0..12)
            .map(|step| format!("Step {step}: {paragraph}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let dialog = plan_review_message(&message);
        let pane = message_pane(&dialog);
        assert!(
            pane.total_rows > usize::from(pane.rect.height),
            "the fixture has to outgrow the pane"
        );

        let selection = range((0, 0), (pane.total_rows - 1, pane.rect.width - 1));

        assert_eq!(dialog.selection_text(&selection, pane.rect.width), message);
    }

    #[test]
    fn partial_first_and_last_plan_lines_are_cut_at_the_selected_columns() {
        let dialog = plan_review_message("alpha beta gamma\n世界 wide row\nomega");
        // At this width the pane draws "alpha beta" / "gamma" / "世界 wide" /
        // "row" / "omega"; the wide graphemes take two cells each.
        let width = 10;

        assert_eq!(
            dialog.selection_text(&range((0, 6), (3, 2)), width),
            "beta gamma\n世界 wide row"
        );
    }

    #[test]
    fn a_range_inside_one_wrapped_line_rejoins_the_rows_word_wrap_split() {
        let dialog = plan_review_message("alpha beta gamma\n世界 wide row\nomega");

        assert_eq!(
            dialog.selection_text(&range((0, 6), (1, 2)), 10),
            "beta gam"
        );
    }

    #[test]
    fn plan_review_gives_unused_form_rows_to_the_plan_and_scrolls_to_its_end() {
        let mut dialog = plan_review(80);

        let first = rendered(&dialog);
        assert!(first.contains("plan-line-12"));
        assert!(!first.contains("plan-line-79"));
        assert!(first.contains("PgUp/PgDn or wheel scroll"));

        for _ in 0..10 {
            dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
            rendered(&dialog);
        }
        let last = rendered(&dialog);
        assert!(last.contains("plan-line-79"));
        assert_eq!(dialog.focus_index(), 0);
    }

    #[test]
    fn mouse_wheel_scrolls_the_plan_without_moving_the_decision() {
        let mut dialog = plan_review(80);
        rendered(&dialog);
        let area = dialog.message_area.get().expect("rendered plan area");

        dialog.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(dialog.message_scroll.get(), 3);
        assert_eq!(dialog.focus_index(), 0);
        assert!(rendered(&dialog).contains("plan-line-03"));
    }

    #[test]
    fn mouse_click_toggles_the_focused_boolean_through_the_form() {
        let mut dialog = ElicitationDialog::new(request(
            ElicitationFieldKind::Boolean {
                default: Some(false),
            },
            false,
        ));
        let buffer = rendered_in_pane(&dialog, 100, 30);
        let point = (0..100)
            .flat_map(|column| (0..30).map(move |row| (column, row)))
            .find(|&(column, row)| buffer[(column, row)].symbol() == "☐")
            .expect("boolean control has a hitbox");
        let mouse = |kind| MouseEvent {
            kind,
            column: point.0,
            row: point.1,
            modifiers: KeyModifiers::NONE,
        };
        dialog.handle_mouse(mouse(MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        )));
        dialog.handle_mouse(mouse(MouseEventKind::Up(
            crossterm::event::MouseButton::Left,
        )));
        assert!(matches!(dialog.values[0], FieldValue::Boolean(true)));
    }

    #[test]
    fn revise_edits_feedback_inline_and_submits_it_with_the_action() {
        let mut dialog = plan_review(4);

        assert_eq!(dialog.display_fields.len(), 1);
        assert!(!rendered(&dialog).contains("> "));

        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        let revise = rendered(&dialog);
        assert!(revise.contains("● Revise"));
        assert!(revise.contains("Describe what the agent should change"));
        assert!(revise.contains("> "));
        assert!(revise.contains("1/1"));

        dialog.paste("add a regression test");
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([
                    (
                        "feedback".into(),
                        ElicitationValue::String("add a regression test".into())
                    ),
                    (
                        "question_0".into(),
                        ElicitationValue::String("revise".into())
                    ),
                ])
            })
        );
    }

    #[test]
    fn leaving_revise_keeps_its_draft_out_of_the_answer() {
        let mut dialog = plan_review(4);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.paste("stale revision");
        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "question_0".into(),
                    ElicitationValue::String("implement".into())
                )])
            })
        );
    }

    #[test]
    fn selecting_an_option_returns_its_wire_value() {
        let mut dialog = ElicitationDialog::new(request(
            ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: "thin".into(),
                        title: "Thin callers".into(),
                        description: None,
                        preview: None,
                    },
                    ElicitationOption {
                        value: "dynamic".into(),
                        title: "Dynamic matrix".into(),
                        description: None,
                        preview: None,
                    },
                ],
                default: None,
            },
            true,
        ));
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "question_0".into(),
                    ElicitationValue::String("dynamic".into())
                )])
            })
        );
    }

    #[test]
    fn first_single_select_option_is_the_visible_and_submitted_default() {
        let mut dialog = ElicitationDialog::new(request(
            ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: "thin".into(),
                        title: "Thin callers".into(),
                        description: None,
                        preview: None,
                    },
                    ElicitationOption {
                        value: "dynamic".into(),
                        title: "Dynamic matrix".into(),
                        description: None,
                        preview: None,
                    },
                ],
                default: None,
            },
            false,
        ));

        let initial = rendered(&dialog);
        assert!(initial.contains("● Thin callers"));
        assert!(initial.contains("○ Dynamic matrix"));

        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "question_0".into(),
                    ElicitationValue::String("thin".into())
                )])
            })
        );
    }

    #[test]
    fn paired_custom_answers_share_their_question_page() {
        let mut dialog = ElicitationDialog::new(paired_request(3, false));

        assert_eq!(dialog.display_fields.len(), 3);
        let first = rendered(&dialog);
        assert!(first.contains("1/3"));
        assert!(first.contains("○ Other"));
        assert!(!first.contains("1/6"));

        dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        let second = rendered(&dialog);
        assert!(second.contains("2/3"));
        assert!(second.contains("Question 2"));
    }

    #[test]
    fn custom_answer_uses_the_adapter_field_instead_of_the_stale_selection() {
        let mut dialog = ElicitationDialog::new(paired_request(1, false));
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        for character in "custom answer".chars() {
            dialog.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
        }
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "question_0__other".into(),
                    ElicitationValue::String("custom answer".into())
                )])
            })
        );
    }

    #[test]
    fn choosing_an_option_after_typing_other_omits_the_custom_draft() {
        let mut dialog = ElicitationDialog::new(paired_request(1, false));
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        dialog.paste("custom draft");
        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "question_0".into(),
                    ElicitationValue::String("beta".into())
                )])
            })
        );
    }

    #[test]
    fn toggling_a_multi_select_option_deactivates_other() {
        let mut dialog = ElicitationDialog::new(paired_request(1, true));
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        dialog.paste("custom draft");
        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "question_0".into(),
                    ElicitationValue::StringArray(vec!["beta".into()])
                )])
            })
        );
    }

    #[test]
    fn dangling_custom_metadata_remains_a_standalone_page() {
        let mut request = paired_request(1, false);
        request.fields[1].custom_answer_for = Some("missing".into());
        let mut dialog = ElicitationDialog::new(request);

        assert_eq!(dialog.display_fields.len(), 2);
        dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(rendered(&dialog).contains("2/2"));
        assert!(rendered(&dialog).contains("Other"));
    }

    #[test]
    fn required_text_blocks_submit_until_answered() {
        let mut dialog = ElicitationDialog::new(request(
            ElicitationFieldKind::Text {
                default: None,
                min_length: None,
                max_length: None,
                pattern: None,
                format: None,
            },
            true,
        ));
        dialog.focus_control(1);
        assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(dialog.focus_index(), 0);
        assert_eq!(dialog.error.as_deref(), Some("Architecture is required"));
    }

    #[test]
    fn escape_cancels_the_elicitation() {
        let mut dialog = ElicitationDialog::new(request(
            ElicitationFieldKind::Boolean { default: None },
            false,
        ));
        assert_eq!(
            dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            Some(ElicitationResponse::Cancel)
        );
    }
}
