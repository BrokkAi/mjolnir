//! Focus, event routing, and interaction state shared by the controls.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use rat_event::{ConsumedEvent, Outcome};
use rat_focus::{Focus, FocusBuilder, FocusFlag, HasFocus, Navigation};
use ratatui::layout::Rect;
use std::fmt;

use crate::hel_text_input::{EditOutcome, TextInput};

/// A user-visible edit delivered by a [`TextField`](super::TextField).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldEdit {
    /// A key event to pass to the readline editor.
    Key(KeyEvent),
    /// Text pasted into the field.
    Paste(String),
    /// A byte offset selected by clicking in the field.
    Cursor(usize),
}

/// The common interaction vocabulary emitted by a [`Form`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interaction<K> {
    /// Activate a button or submit a text field.
    Activate(K),
    /// Toggle a checkbox or a multiple-choice row.
    Toggle(K),
    /// Select a row or tab.
    Select(K, usize),
    /// Accept the currently highlighted combobox option.
    ComboBoxCommit(K, usize),
    /// Dismiss an expanded combobox without changing its value.
    ComboBoxDismiss(K),
    /// Edit a text field.
    Edit(K, FieldEdit),
    /// Escape requests dismissal of the active form.
    Cancel,
}

/// A small result which preserves both repaint information and a typed action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventResult<A> {
    /// The state change caused by the event.
    pub outcome: Outcome,
    /// The screen-level action, when one was produced.
    pub action: Option<A>,
}

impl<A> EventResult<A> {
    /// An event which was not recognized by this form.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            outcome: Outcome::Continue,
            action: None,
        }
    }

    /// A consumed event which did not produce a visible state change.
    #[must_use]
    pub const fn handled() -> Self {
        Self {
            outcome: Outcome::Unchanged,
            action: None,
        }
    }

    /// A consumed event which carries an action but does not itself repaint
    /// the form. The action is deliberately retained even when the outcome is
    /// unchanged; callers may need to apply a domain operation such as a
    /// button activation or a no-op cursor request.
    #[must_use]
    pub const fn unchanged(action: Option<A>) -> Self {
        Self {
            outcome: Outcome::Unchanged,
            action,
        }
    }

    /// A consumed event which should cause a repaint.
    #[must_use]
    pub const fn changed(action: Option<A>) -> Self {
        Self {
            outcome: Outcome::Changed,
            action,
        }
    }

    /// Replaces the action while preserving this result's repaint outcome.
    #[must_use]
    pub fn map_action<B>(self, map: impl FnOnce(A) -> B) -> EventResult<B> {
        EventResult {
            outcome: self.outcome,
            action: self.action.map(map),
        }
    }

    /// Replaces the outcome while preserving this result's action.
    #[must_use]
    pub const fn with_outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Combines a second repaint outcome with this result, retaining the
    /// stronger of the two while preserving the action.
    #[must_use]
    pub fn merge_outcome(self, outcome: Outcome) -> Self {
        let merged = self.outcome.max(outcome);
        self.with_outcome(merged)
    }
}

impl<A> ConsumedEvent for EventResult<A> {
    fn is_consumed(&self) -> bool {
        self.outcome.is_consumed()
    }
}

/// The kind of behavior associated with a registered control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    /// A push button.
    Button,
    /// A readline text field.
    TextField,
    /// A boolean checkbox.
    Checkbox,
    /// A vertically navigable list.
    ChoiceList {
        /// Number of rows.
        len: usize,
        /// Current row.
        selected: usize,
    },
    /// A scalar field with a popup list of choices.
    ComboBox {
        /// Number of options.
        len: usize,
        /// Current popup cursor.
        selected: usize,
        /// Whether the popup is expanded.
        expanded: bool,
    },
    /// A horizontally navigable tab strip.
    Tabs {
        /// Number of tabs.
        len: usize,
        /// Current tab.
        selected: usize,
    },
}

impl ControlKind {
    fn is_button(self) -> bool {
        matches!(self, Self::Button)
    }

    fn is_field(self) -> bool {
        matches!(self, Self::TextField)
    }

    fn is_checkbox(self) -> bool {
        matches!(self, Self::Checkbox)
    }

    fn is_choice_list(self) -> bool {
        matches!(self, Self::ChoiceList { .. })
    }

    fn is_combo_box(self) -> bool {
        matches!(self, Self::ComboBox { .. })
    }

    fn is_tab_strip(self) -> bool {
        matches!(self, Self::Tabs { .. })
    }

    fn selected(self) -> Option<usize> {
        match self {
            Self::ChoiceList { selected, .. }
            | Self::ComboBox { selected, .. }
            | Self::Tabs { selected, .. } => Some(selected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Control<K> {
    id: K,
    flag: FocusFlag,
    area: Rect,
    enabled: bool,
    active: bool,
    kind: ControlKind,
    cursor_map: Vec<(u16, usize)>,
    multiline_cursor_map: Vec<(u16, u16, usize)>,
    editor_area: Rect,
    region_map: Vec<(u16, u16, usize)>,
    list_offset: usize,
    row_map: Vec<Option<usize>>,
    row_enabled: Vec<bool>,
    popup_area: Rect,
    popup_row_map: Vec<Option<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerOwner<K> {
    Control(K),
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerHit<K> {
    Control(K),
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormVisualState<K> {
    focused: Option<K>,
    focused_kind: Option<ControlKind>,
    armed: Option<K>,
    dismiss_armed: bool,
}

impl<K> Control<K> {
    fn new(id: K, kind: ControlKind) -> Self {
        Self {
            id,
            flag: FocusFlag::default(),
            area: Rect::default(),
            enabled: true,
            active: true,
            kind,
            cursor_map: Vec::new(),
            multiline_cursor_map: Vec::new(),
            editor_area: Rect::default(),
            region_map: Vec::new(),
            list_offset: 0,
            row_map: Vec::new(),
            row_enabled: Vec::new(),
            popup_area: Rect::default(),
            popup_row_map: Vec::new(),
        }
    }
}

impl<K> HasFocus for Control<K> {
    fn build(&self, builder: &mut FocusBuilder) {
        builder.leaf_widget(self);
    }

    fn focus(&self) -> FocusFlag {
        self.flag.clone()
    }

    fn area(&self) -> Rect {
        self.area
    }

    fn navigable(&self) -> Navigation {
        if self.active && self.enabled {
            Navigation::Regular
        } else {
            Navigation::None
        }
    }
}

/// A persistent, screen-local collection of interactive controls.
///
/// Controls retain their identity when a screen redraws. Call [`begin_frame`](Self::begin_frame),
/// register controls in visual order while rendering, and finish with [`end_frame`](Self::end_frame).
/// The focus tree is rebuilt from the registrations and keeps the focused control when possible.
pub struct Form<K: Copy + Eq> {
    controls: Vec<Control<K>>,
    order: Vec<K>,
    last_order: Vec<K>,
    focus_tree: Focus,
    pointer_owner: Option<PointerOwner<K>>,
    dismiss_area: Rect,
    dismiss_enabled: bool,
    dismiss_active: bool,
    pending_focus: Option<K>,
}

impl<K: Copy + Eq> Default for Form<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq> Form<K> {
    /// Creates an empty form.
    #[must_use]
    pub fn new() -> Self {
        Self {
            controls: Vec::new(),
            order: Vec::new(),
            last_order: Vec::new(),
            focus_tree: Focus::default(),
            pointer_owner: None,
            dismiss_area: Rect::default(),
            dismiss_enabled: false,
            dismiss_active: false,
            pending_focus: None,
        }
    }

    /// Begins a metadata update between frames, preserving the drawn hitboxes.
    pub fn begin_update(&mut self) {
        self.order.clear();
        self.dismiss_active = false;
        for control in &mut self.controls {
            control.active = false;
        }
    }

    /// Marks the start of a render pass. Registrations from the previous pass are hidden.
    pub fn begin_frame(&mut self) {
        self.begin_update();
        for control in &mut self.controls {
            control.cursor_map.clear();
            control.multiline_cursor_map.clear();
            control.editor_area = Rect::default();
        }
    }

    /// Registers a control with its exact drawn hitbox.
    pub fn register(&mut self, id: K, kind: ControlKind, area: Rect, enabled: bool) {
        self.register_with_cursor_map(id, kind, area, enabled, Vec::new());
    }

    /// Registers the non-focusable upper-left dismissal target for a modal.
    ///
    /// The target is independent of regular controls so a footer Cancel button
    /// can retain its own hitbox and keyboard focus.
    pub fn register_dismiss(&mut self, area: Rect, enabled: bool) {
        self.dismiss_area = area;
        self.dismiss_enabled = enabled;
        self.dismiss_active = true;
    }

    /// Registers a control and the screen columns corresponding to field cursors.
    pub(crate) fn register_with_cursor_map(
        &mut self,
        id: K,
        kind: ControlKind,
        area: Rect,
        enabled: bool,
        cursor_map: Vec<(u16, usize)>,
    ) {
        let index = self.ensure_control(id, kind);
        let control = &mut self.controls[index];
        control.area = area;
        control.enabled = enabled;
        control.active = true;
        control.kind = kind;
        control.cursor_map = cursor_map;
        control.multiline_cursor_map.clear();
        control.editor_area = if kind.is_field() {
            area
        } else {
            Rect::default()
        };
        control.region_map.clear();
        control.list_offset = 0;
        control.row_map.clear();
        control.row_enabled.clear();
        control.popup_area = Rect::default();
        control.popup_row_map.clear();
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    pub(crate) fn register_with_regions(
        &mut self,
        id: K,
        kind: ControlKind,
        area: Rect,
        enabled: bool,
        region_map: Vec<(u16, u16, usize)>,
    ) {
        let index = self.ensure_control(id, kind);
        let control = &mut self.controls[index];
        control.area = area;
        control.enabled = enabled;
        control.active = true;
        control.kind = kind;
        control.cursor_map.clear();
        control.multiline_cursor_map.clear();
        control.editor_area = Rect::default();
        control.region_map = region_map;
        control.list_offset = 0;
        control.row_map.clear();
        control.row_enabled.clear();
        control.popup_area = Rect::default();
        control.popup_row_map.clear();
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    pub(crate) fn register_with_rows(
        &mut self,
        id: K,
        kind: ControlKind,
        area: Rect,
        enabled: bool,
        row_map: Vec<Option<usize>>,
        row_enabled: Vec<bool>,
    ) {
        let index = self.ensure_control(id, kind);
        let control = &mut self.controls[index];
        control.area = area;
        control.enabled = enabled;
        control.active = true;
        control.kind = kind;
        control.cursor_map.clear();
        control.multiline_cursor_map.clear();
        control.editor_area = Rect::default();
        control.region_map.clear();
        control.list_offset = 0;
        control.row_map = row_map;
        control.row_enabled = row_enabled;
        control.popup_area = Rect::default();
        control.popup_row_map.clear();
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Registers the editor within a compound control without adding a tab stop.
    pub(crate) fn register_inline_editor(
        &mut self,
        id: K,
        area: Rect,
        cursor_map: Vec<(u16, usize)>,
    ) {
        if let Some(control) = self.control_mut(id) {
            control.editor_area = area;
            control.cursor_map = cursor_map;
            control.multiline_cursor_map.clear();
        }
    }

    /// Registers a scalar combobox field and, when expanded, its popup hitbox.
    pub(crate) fn register_combobox(
        &mut self,
        id: K,
        kind: ControlKind,
        area: Rect,
        enabled: bool,
        popup_area: Rect,
        popup_row_map: Vec<Option<usize>>,
    ) {
        let index = self.ensure_control(id, kind);
        let control = &mut self.controls[index];
        control.area = area;
        control.enabled = enabled;
        control.active = true;
        control.kind = kind;
        control.cursor_map.clear();
        control.multiline_cursor_map.clear();
        control.editor_area = Rect::default();
        control.region_map.clear();
        control.list_offset = 0;
        control.row_map.clear();
        control.row_enabled.clear();
        control.popup_area = popup_area;
        control.popup_row_map = popup_row_map;
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Registers screen cells corresponding to cursors in a wrapped field.
    pub(crate) fn register_with_multiline_cursor_map(
        &mut self,
        id: K,
        kind: ControlKind,
        area: Rect,
        enabled: bool,
        cursor_map: Vec<(u16, u16, usize)>,
    ) {
        let index = self.ensure_control(id, kind);
        let control = &mut self.controls[index];
        control.area = area;
        control.enabled = enabled;
        control.active = true;
        control.kind = kind;
        control.cursor_map.clear();
        control.multiline_cursor_map = cursor_map;
        control.editor_area = if kind.is_field() {
            area
        } else {
            Rect::default()
        };
        control.region_map.clear();
        control.list_offset = 0;
        control.row_map.clear();
        control.row_enabled.clear();
        control.popup_area = Rect::default();
        control.popup_row_map.clear();
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Declares a control for event handling before its first render.
    ///
    /// The declaration uses a zero geometry; a later render registration should replace it.
    pub fn declare(&mut self, id: K, kind: ControlKind) {
        self.declare_with_enabled(id, kind, true);
    }

    /// Declares a control for event handling with an explicit enabled state.
    pub fn declare_with_enabled(&mut self, id: K, kind: ControlKind, enabled: bool) {
        let index = self.ensure_control(id, kind);
        let control = &mut self.controls[index];
        if std::mem::discriminant(&control.kind) != std::mem::discriminant(&kind)
            || matches!(
                (control.kind, kind),
                (ControlKind::ChoiceList { len: before, .. }, ControlKind::ChoiceList { len: after, .. }) if before != after
            )
            || matches!(
                (control.kind, kind),
                (ControlKind::ComboBox { len: before, .. }, ControlKind::ComboBox { len: after, .. }) if before != after
            )
        {
            control.area = Rect::default();
            control.row_map.clear();
            control.row_enabled.clear();
            control.cursor_map.clear();
            control.multiline_cursor_map.clear();
            control.editor_area = Rect::default();
            control.list_offset = 0;
            control.popup_area = Rect::default();
            control.popup_row_map.clear();
        }
        control.active = true;
        control.enabled = enabled;
        control.kind = kind;
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Removes all active controls while retaining their stable identities for future frames.
    pub fn clear(&mut self) {
        self.order.clear();
        self.last_order.clear();
        self.pending_focus = None;
        self.pointer_owner = None;
        self.dismiss_area = Rect::default();
        self.dismiss_enabled = false;
        self.dismiss_active = false;
        for control in &mut self.controls {
            control.active = false;
            control.flag.clear();
            control.popup_area = Rect::default();
            control.popup_row_map.clear();
        }
        self.focus_tree.none();
    }

    /// Clears hitboxes and cursor maps while retaining declarations and focus identities.
    pub fn reset_geometry(&mut self) {
        for control in &mut self.controls {
            control.area = Rect::default();
            control.cursor_map.clear();
            control.multiline_cursor_map.clear();
            control.editor_area = Rect::default();
            control.popup_area = Rect::default();
            control.popup_row_map.clear();
        }
        self.dismiss_area = Rect::default();
    }

    /// Rebuilds the focus tree and repairs focus after controls appeared or disappeared.
    pub fn end_frame(&mut self, initial: K) {
        let previous_focus = self.focused();
        let previous_order = self.last_order.clone();
        let old_focus = std::mem::take(&mut self.focus_tree);
        let mut builder = FocusBuilder::new(Some(old_focus));
        for id in &self.order {
            if let Some(control) = self.active_control(*id) {
                builder.leaf_widget(control);
            }
        }
        self.focus_tree = builder.build();
        self.last_order.clone_from(&self.order);
        // Hosts clear geometry before rendering. Metadata reconciliation can run
        // in that interval; only removal or disabling cancels a captured press.
        self.pointer_owner = self
            .pointer_owner
            .filter(|owner| self.pointer_owner_is_eligible(*owner));

        let preserved = previous_focus.filter(|id| self.is_eligible(*id));
        let wanted = self
            .pending_focus
            .take()
            .filter(|id| self.is_eligible(*id))
            .or(preserved)
            .or_else(|| (previous_focus.is_none() && self.is_eligible(initial)).then_some(initial))
            .or_else(|| self.fallback(previous_focus, &previous_order));
        if let Some(id) = wanted.filter(|id| self.is_eligible(*id)) {
            self.focus(id);
        } else {
            self.focus_tree.none();
            self.pending_focus = None;
        }
    }

    /// Returns the focused control, if any.
    #[must_use]
    pub fn focused(&self) -> Option<K> {
        if let Some(id) = self.pending_focus.filter(|id| self.is_eligible(*id)) {
            return Some(id);
        }
        if let Some(flag) = self.focus_tree.focused() {
            return self
                .controls
                .iter()
                .find(|control| control.flag == flag)
                .map(|control| control.id);
        }
        self.pending_focus.filter(|id| self.is_eligible(*id))
    }

    /// Requests focus for a stable control id.
    pub fn focus(&mut self, id: K) {
        if let Some(control) = self.active_control(id).filter(|control| control.enabled) {
            if self.focus_tree.is_valid_widget(control) {
                self.focus_tree.focus(control);
                self.pending_focus = None;
            } else {
                self.pending_focus = Some(id);
            }
        } else {
            self.pending_focus = Some(id);
        }
    }

    /// Returns whether the given point is in any active control hitbox.
    #[must_use]
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.hit(x, y).is_some()
    }

    /// Handles keyboard and mouse input for the form.
    pub fn handle(&mut self, event: &Event) -> EventResult<Interaction<K>> {
        let before = self.visual_state();
        let result = self.handle_inner(event);
        self.report_result(before, result)
    }

    fn handle_inner(&mut self, event: &Event) -> EventResult<Interaction<K>> {
        if let Some(result) = self.handle_pointer(event) {
            return result;
        }
        if matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press) {
            self.cancel_pointer();
        }
        if let Some(result) = self.handle_key(event) {
            return result;
        }
        if let Event::Paste(text) = event
            && let Some(id) = self.focused().filter(|id| self.kind(*id).is_field())
        {
            return EventResult::changed(Some(Interaction::Edit(
                id,
                FieldEdit::Paste(text.clone()),
            )));
        }
        EventResult::ignored()
    }

    fn visual_state(&self) -> FormVisualState<K> {
        let focused = self.focused();
        FormVisualState {
            focused,
            focused_kind: focused.and_then(|id| self.control(id).map(|control| control.kind)),
            armed: match self.pointer_owner {
                Some(PointerOwner::Control(id)) => Some(id).filter(|id| {
                    self.active_control(*id)
                        .is_some_and(|control| control.enabled && !control.kind.is_field())
                }),
                Some(PointerOwner::Dismiss) | None => None,
            },
            dismiss_armed: self.dismiss_is_armed(),
        }
    }

    fn report_result(
        &self,
        before: FormVisualState<K>,
        mut result: EventResult<Interaction<K>>,
    ) -> EventResult<Interaction<K>> {
        let visible_changed = before != self.visual_state();
        if visible_changed {
            result.outcome = Outcome::Changed;
        } else if result.outcome == Outcome::Changed {
            // Form handlers also emit actions for domain operations whose
            // result is not visible until the owner applies them (or which is
            // intentionally a no-op). Keep those actions consumed, but do not
            // force a repaint for the form itself.
            result.outcome = Outcome::Unchanged;
        }
        result
    }

    /// Returns true while a mouse control owns a press/release gesture.
    #[must_use]
    pub fn captures_pointer(&self) -> bool {
        self.pointer_owner.is_some()
    }

    /// Cancels a captured mouse gesture.
    pub fn cancel_pointer(&mut self) {
        self.pointer_owner = None;
    }

    /// Whether the control is declared and accepts keyboard focus.
    #[must_use]
    pub fn is_enabled(&self, id: K) -> bool {
        self.is_eligible(id)
    }

    /// Returns whether a control currently has input focus.
    #[must_use]
    pub fn is_focused(&self, id: K) -> bool {
        self.is_eligible(id) && self.focused() == Some(id)
    }

    /// Returns whether a control currently owns a mouse press/release gesture.
    #[must_use]
    pub fn is_armed(&self, id: K) -> bool {
        self.pointer_owner == Some(PointerOwner::Control(id))
    }

    /// Returns whether the modal dismissal target owns an active press.
    #[must_use]
    pub fn dismiss_is_armed(&self) -> bool {
        self.pointer_owner == Some(PointerOwner::Dismiss)
    }

    /// Returns the current selection metadata for a list, combobox, or tab strip.
    #[must_use]
    pub fn selected(&self, id: K) -> Option<usize> {
        self.kind(id).selected()
    }

    /// Returns the first display row currently visible for a choice list.
    #[must_use]
    pub fn list_offset(&self, id: K) -> usize {
        self.control(id).map_or(0, |control| control.list_offset)
    }

    /// Updates selection metadata immediately, allowing batched key events before redraw.
    pub fn set_selected(&mut self, id: K, selected: usize) {
        if let Some(control) = self.control_mut(id) {
            control.kind = match control.kind {
                ControlKind::ChoiceList { len, .. } => ControlKind::ChoiceList {
                    len,
                    selected: selected.min(len.saturating_sub(1)),
                },
                ControlKind::Tabs { len, .. } => ControlKind::Tabs {
                    len,
                    selected: selected.min(len.saturating_sub(1)),
                },
                ControlKind::ComboBox { len, expanded, .. } => ControlKind::ComboBox {
                    len,
                    selected: selected.min(len.saturating_sub(1)),
                    expanded,
                },
                kind => kind,
            };
        }
    }

    pub(crate) fn set_list_offset(&mut self, id: K, offset: usize) {
        if let Some(control) = self.control_mut(id) {
            control.list_offset = offset;
        }
    }

    fn ensure_control(&mut self, id: K, kind: ControlKind) -> usize {
        if let Some(index) = self.controls.iter().position(|control| control.id == id) {
            index
        } else {
            self.controls.push(Control::new(id, kind));
            self.controls.len() - 1
        }
    }

    fn control(&self, id: K) -> Option<&Control<K>> {
        self.controls.iter().find(|control| control.id == id)
    }

    fn control_mut(&mut self, id: K) -> Option<&mut Control<K>> {
        self.controls.iter_mut().find(|control| control.id == id)
    }

    fn active_control(&self, id: K) -> Option<&Control<K>> {
        self.control(id).filter(|control| control.active)
    }

    fn kind(&self, id: K) -> ControlKind {
        self.control(id)
            .map_or(ControlKind::Button, |control| control.kind)
    }

    fn is_eligible(&self, id: K) -> bool {
        self.active_control(id)
            .is_some_and(|control| control.enabled)
    }

    fn dismiss_is_eligible(&self) -> bool {
        self.dismiss_active && self.dismiss_enabled
    }

    fn pointer_owner_is_eligible(&self, owner: PointerOwner<K>) -> bool {
        match owner {
            PointerOwner::Control(id) => self.is_eligible(id),
            PointerOwner::Dismiss => self.dismiss_is_eligible(),
        }
    }

    fn fallback(&self, previous_focus: Option<K>, previous_order: &[K]) -> Option<K> {
        let start = previous_focus
            .and_then(|id| previous_order.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        self.order
            .iter()
            .skip(if previous_focus.is_some() { start } else { 0 })
            .chain(self.order.iter())
            .copied()
            .find(|id| self.is_eligible(*id))
    }

    fn hit(&self, x: u16, y: u16) -> Option<PointerHit<K>> {
        if self.dismiss_active && self.dismiss_area.contains((x, y).into()) {
            return Some(PointerHit::Dismiss);
        }
        self.order
            .iter()
            .rev()
            .filter_map(|id| self.active_control(*id))
            .find(|control| {
                control.area.contains((x, y).into()) || control.popup_area.contains((x, y).into())
            })
            .map(|control| PointerHit::Control(control.id))
    }

    fn handle_key(&mut self, event: &Event) -> Option<EventResult<Interaction<K>>> {
        let Event::Key(key) = event else { return None };
        if matches!(key.kind, KeyEventKind::Release) {
            return Some(if self.focused().is_some() {
                EventResult::handled()
            } else {
                EventResult::ignored()
            });
        }
        let is_press = matches!(key.kind, KeyEventKind::Press);
        let ordinary = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        let focused = self.focused().filter(|id| self.is_eligible(*id));
        if ordinary && is_tab(*key) {
            if let Some(id) = focused
                && let ControlKind::ComboBox {
                    selected,
                    expanded: true,
                    ..
                } = self.kind(id)
            {
                return Some(EventResult::changed(Some(Interaction::ComboBoxCommit(
                    id, selected,
                ))));
            }
            let changed = if is_back_tab(*key) {
                self.focus_pending_sibling(false)
            } else {
                self.focus_pending_sibling(true)
            };
            return Some(if changed {
                EventResult::changed(None)
            } else {
                EventResult::handled()
            });
        }
        if is_press && ordinary && key.code == KeyCode::Esc {
            if let Some(id) = focused
                && matches!(self.kind(id), ControlKind::ComboBox { expanded: true, .. })
            {
                return Some(EventResult::changed(Some(Interaction::ComboBoxDismiss(id))));
            }
            return Some(EventResult::changed(Some(Interaction::Cancel)));
        }

        let id = focused?;
        let kind = self.kind(id);
        if !is_press
            && ordinary
            && (key.code == KeyCode::Enter
                || key.code == KeyCode::Esc
                || (key.code == KeyCode::Char(' ') && !kind.is_field()))
        {
            return Some(EventResult::handled());
        }
        if kind.is_button() {
            if is_press
                && ordinary
                && (key.code == KeyCode::Enter || key.code == KeyCode::Char(' '))
            {
                return Some(EventResult::changed(Some(Interaction::Activate(id))));
            }
            if ordinary && (key.code == KeyCode::Left || key.code == KeyCode::Right) {
                let changed = self.focus_sibling(id, key.code == KeyCode::Right, true);
                return Some(if changed {
                    EventResult::changed(None)
                } else {
                    EventResult::handled()
                });
            }
            return None;
        }
        if kind.is_field() {
            if is_press && ordinary && key.code == KeyCode::Enter {
                return Some(EventResult::changed(Some(Interaction::Activate(id))));
            }
            return Some(EventResult::changed(Some(Interaction::Edit(
                id,
                FieldEdit::Key(*key),
            ))));
        }
        if is_press
            && kind.is_checkbox()
            && ordinary
            && (key.code == KeyCode::Enter || key.code == KeyCode::Char(' '))
        {
            return Some(EventResult::changed(Some(Interaction::Toggle(id))));
        }
        if ordinary
            && let ControlKind::ComboBox {
                len,
                selected,
                expanded,
            } = kind
        {
            if is_press && matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
                return Some(if expanded {
                    EventResult::changed(Some(Interaction::ComboBoxCommit(id, selected)))
                } else {
                    EventResult::changed(Some(Interaction::Activate(id)))
                });
            }
            if expanded && let Some(next) = list_selection(key.code, selected, len) {
                self.set_selected(id, next);
                return Some(EventResult::changed(Some(Interaction::Select(id, next))));
            }
        }
        if ordinary && let ControlKind::ChoiceList { len, selected } = kind {
            if is_press && matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
                let control = self.control(id)?;
                let allowed = if control.row_map.is_empty() {
                    selected < len
                } else {
                    control
                        .row_map
                        .iter()
                        .position(|row| *row == Some(selected))
                        .is_some_and(|row| control.row_enabled.get(row).copied().unwrap_or(true))
                };
                return Some(if allowed {
                    EventResult::changed(Some(if key.code == KeyCode::Enter {
                        Interaction::Activate(id)
                    } else {
                        Interaction::Toggle(id)
                    }))
                } else {
                    EventResult::handled()
                });
            }
            if let Some(next) = self.list_selection(id, key.code, selected, len) {
                self.set_selected(id, next);
                return Some(EventResult::changed(Some(Interaction::Select(id, next))));
            }
        }
        if ordinary
            && let ControlKind::Tabs { len, selected } = kind
            && let Some(next) = tab_selection(key.code, selected, len)
        {
            self.set_selected(id, next);
            return Some(EventResult::changed(Some(Interaction::Select(id, next))));
        }
        if is_press
            && ordinary
            && matches!(kind, ControlKind::Tabs { .. })
            && key.code == KeyCode::Enter
        {
            return Some(EventResult::changed(Some(Interaction::Activate(id))));
        }
        None
    }

    fn focus_sibling(&mut self, current: K, forward: bool, buttons_only: bool) -> bool {
        let Some(index) = self.order.iter().position(|id| *id == current) else {
            return false;
        };
        let indices = if forward {
            (index + 1..self.order.len())
                .chain(0..index)
                .collect::<Vec<_>>()
        } else {
            (0..index)
                .rev()
                .chain((index + 1..self.order.len()).rev())
                .collect::<Vec<_>>()
        };
        for candidate in indices {
            let id = self.order[candidate];
            if self.is_eligible(id) && (!buttons_only || self.kind(id).is_button()) {
                self.focus(id);
                return true;
            }
        }
        false
    }

    fn focus_pending_sibling(&mut self, forward: bool) -> bool {
        if self.focus_tree.focused().is_some() {
            return if forward {
                self.focus_tree.next()
            } else {
                self.focus_tree.prev()
            };
        }
        let current = self.pending_focus;
        let current_index =
            current.and_then(|id| self.order.iter().position(|candidate| *candidate == id));
        let candidates = if forward {
            current_index
                .map(|index| {
                    (index + 1..self.order.len())
                        .chain(0..=index)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| (0..self.order.len()).collect())
        } else {
            current_index
                .map(|index| {
                    (0..index)
                        .rev()
                        .chain((index..self.order.len()).rev())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| (0..self.order.len()).rev().collect())
        };
        if let Some(index) = candidates
            .into_iter()
            .find(|index| self.is_eligible(self.order[*index]))
        {
            self.pending_focus = Some(self.order[index]);
            return true;
        }
        false
    }

    fn list_selection(&self, id: K, code: KeyCode, selected: usize, len: usize) -> Option<usize> {
        let control = self.control(id)?;
        if control.row_map.is_empty() {
            return list_selection(code, selected, len);
        }
        let len = control.row_map.len();
        let current = control
            .row_map
            .iter()
            .position(|row| *row == Some(selected))
            .unwrap_or(0);
        let target = match code {
            KeyCode::Up => (0..current).rev().find(|row| {
                control.row_enabled.get(*row).copied().unwrap_or(true)
                    && control.row_map[*row].is_some()
                    && control.row_map[*row] != Some(selected)
            }),
            KeyCode::Down => ((current + 1)..len).find(|row| {
                control.row_enabled.get(*row).copied().unwrap_or(true)
                    && control.row_map[*row].is_some()
                    && control.row_map[*row] != Some(selected)
            }),
            KeyCode::Home => (0..len).find(|row| {
                control.row_enabled.get(*row).copied().unwrap_or(true)
                    && control.row_map[*row].is_some()
            }),
            KeyCode::End => (0..len).rev().find(|row| {
                control.row_enabled.get(*row).copied().unwrap_or(true)
                    && control.row_map[*row].is_some()
            }),
            KeyCode::PageUp => page_target(control, current, false),
            KeyCode::PageDown => page_target(control, current, true),
            _ => None,
        }?;
        control.row_map[target]
    }

    fn handle_pointer(&mut self, event: &Event) -> Option<EventResult<Interaction<K>>> {
        match event {
            Event::Mouse(mouse) => {
                let (x, y) = (mouse.column, mouse.row);
                if self.pointer_owner.is_some() {
                    match mouse.kind {
                        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                            let owner = self.pointer_owner?;
                            if !self.pointer_owner_contains(owner, x, y) {
                                self.pointer_owner = None;
                                return Some(EventResult::changed(None));
                            }
                            return Some(EventResult::handled());
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            let owner = self.pointer_owner.take()?;
                            if !self.pointer_owner_contains(owner, x, y) {
                                return Some(EventResult::changed(None));
                            }
                            let interaction = match owner {
                                PointerOwner::Control(id) => self.pointer_interaction(id, x, y),
                                PointerOwner::Dismiss => Some(Interaction::Cancel),
                            };
                            return Some(match interaction {
                                Some(action) => EventResult::changed(Some(action)),
                                None => EventResult::handled(),
                            });
                        }
                        _ => return None,
                    }
                }
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let (id, len, selected) = self.hit(x, y).and_then(|hit| match hit {
                        PointerHit::Control(id) => {
                            self.control(id).and_then(|control| match control.kind {
                                ControlKind::ChoiceList { len, selected } if control.enabled => {
                                    Some((id, len, selected))
                                }
                                ControlKind::ComboBox {
                                    len,
                                    selected,
                                    expanded: true,
                                } if control.enabled => Some((id, len, selected)),
                                _ => None,
                            })
                        }
                        PointerHit::Dismiss => None,
                    })?;
                    let code = if mouse.kind == MouseEventKind::ScrollUp {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    };
                    self.focus(id);
                    if let Some(next) = self.list_selection(id, code, selected, len) {
                        self.set_selected(id, next);
                        return Some(EventResult::changed(Some(Interaction::Select(id, next))));
                    }
                    return Some(EventResult::handled());
                }
                if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                    match self.hit(x, y)? {
                        PointerHit::Dismiss => {
                            if !self.dismiss_enabled {
                                return Some(EventResult::handled());
                            }
                            self.pointer_owner = Some(PointerOwner::Dismiss);
                            return Some(EventResult::changed(None));
                        }
                        PointerHit::Control(id) => {
                            let control = self.control(id)?;
                            let (enabled, kind, cursor, editor) = (
                                control.enabled,
                                control.kind,
                                cursor_at(control, x, y),
                                control
                                    .editor_area
                                    .contains(ratatui::layout::Position::new(x, y)),
                            );
                            if !enabled {
                                return Some(EventResult::handled());
                            }
                            let in_popup =
                                kind.is_combo_box() && control.popup_area.contains((x, y).into());
                            if in_popup {
                                let row = usize::from(y.saturating_sub(control.popup_area.y));
                                if control.popup_row_map.get(row).copied().flatten().is_none() {
                                    return Some(EventResult::handled());
                                }
                            }
                            if kind.is_choice_list() && !editor {
                                let row = usize::from(y.saturating_sub(control.area.y))
                                    + control.list_offset;
                                if !control.row_enabled.get(row).copied().unwrap_or(true)
                                    || (!control.row_map.is_empty()
                                        && control.row_map.get(row).copied().flatten().is_none())
                                {
                                    return Some(EventResult::handled());
                                }
                            }
                            self.focus(id);
                            if kind.is_field() || editor {
                                return Some(EventResult::changed(Some(Interaction::Edit(
                                    id,
                                    FieldEdit::Cursor(cursor),
                                ))));
                            }
                            if kind.is_button()
                                || kind.is_checkbox()
                                || kind.is_choice_list()
                                || kind.is_combo_box()
                                || kind.is_tab_strip()
                            {
                                self.pointer_owner = Some(PointerOwner::Control(id));
                                return Some(EventResult::changed(None));
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn pointer_owner_contains(&self, owner: PointerOwner<K>, x: u16, y: u16) -> bool {
        match owner {
            PointerOwner::Control(id) => self.control(id).is_some_and(|control| {
                control.active
                    && control.enabled
                    && (control.area.contains((x, y).into())
                        || control.popup_area.contains((x, y).into()))
            }),
            PointerOwner::Dismiss => {
                self.dismiss_is_eligible() && self.dismiss_area.contains((x, y).into())
            }
        }
    }

    fn pointer_interaction(&mut self, id: K, x: u16, y: u16) -> Option<Interaction<K>> {
        let control = self.control(id)?;
        match control.kind {
            ControlKind::Button => Some(Interaction::Activate(id)),
            ControlKind::Checkbox => Some(Interaction::Toggle(id)),
            ControlKind::ChoiceList { len, .. } => {
                let row = usize::from(y.saturating_sub(control.area.y)) + control.list_offset;
                if (control.row_map.is_empty() && row >= len)
                    || !control.row_enabled.get(row).copied().unwrap_or(true)
                {
                    return None;
                }
                let selected = if control.row_map.is_empty() {
                    row
                } else {
                    control.row_map.get(row).copied().flatten()?
                };
                self.set_selected(id, selected);
                Some(Interaction::Select(id, selected))
            }
            ControlKind::Tabs { len, .. } => {
                let index = control
                    .region_map
                    .iter()
                    .find(|(start, end, _)| x >= *start && x < *end)
                    .map(|(_, _, index)| *index)
                    .filter(|index| *index < len);
                index.map(|index| {
                    self.set_selected(id, index);
                    Interaction::Select(id, index)
                })
            }
            ControlKind::ComboBox { expanded, .. } => {
                if expanded && control.popup_area.contains((x, y).into()) {
                    let row = usize::from(y.saturating_sub(control.popup_area.y));
                    let index = control.popup_row_map.get(row).copied().flatten()?;
                    self.set_selected(id, index);
                    Some(Interaction::ComboBoxCommit(id, index))
                } else if expanded {
                    Some(Interaction::ComboBoxDismiss(id))
                } else {
                    Some(Interaction::Activate(id))
                }
            }
            ControlKind::TextField => None,
        }
    }
}

impl<K: Copy + Eq> Clone for Form<K> {
    fn clone(&self) -> Self {
        let focused = self.focused();
        let mut clone = Self::new();
        clone.order.clone_from(&self.order);
        clone.last_order.clone_from(&self.last_order);
        clone.pending_focus = focused.or(self.pending_focus);
        clone.dismiss_area = self.dismiss_area;
        clone.dismiss_enabled = self.dismiss_enabled;
        clone.dismiss_active = self.dismiss_active;
        // A physical pointer gesture belongs to the original view, never a draft copy.
        clone.pointer_owner = None;
        clone.controls = self
            .controls
            .iter()
            .map(|control| {
                let mut cloned = Control::new(control.id, control.kind);
                cloned.area = control.area;
                cloned.enabled = control.enabled;
                cloned.active = control.active;
                cloned.cursor_map.clone_from(&control.cursor_map);
                cloned
                    .multiline_cursor_map
                    .clone_from(&control.multiline_cursor_map);
                cloned.editor_area = control.editor_area;
                cloned.region_map.clone_from(&control.region_map);
                cloned.list_offset = control.list_offset;
                cloned.row_map.clone_from(&control.row_map);
                cloned.row_enabled.clone_from(&control.row_enabled);
                cloned.popup_area = control.popup_area;
                cloned.popup_row_map.clone_from(&control.popup_row_map);
                cloned
            })
            .collect();
        clone.rebuild_without_previous_focus();
        if let Some(id) = focused.filter(|id| clone.is_eligible(*id)) {
            clone.focus(id);
        }
        clone
    }
}

impl<K: Copy + Eq> PartialEq for Form<K> {
    fn eq(&self, other: &Self) -> bool {
        self.controls
            .iter()
            .filter(|control| control.active)
            .map(|control| (control.id, control.area, control.enabled, control.kind))
            .eq(other
                .controls
                .iter()
                .filter(|control| control.active)
                .map(|control| (control.id, control.area, control.enabled, control.kind)))
            && self.focused() == other.focused()
    }
}

impl<K: Copy + Eq> Eq for Form<K> {}

impl<K: Copy + Eq> fmt::Debug for Form<K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Form")
            .field("control_count", &self.controls.len())
            .field("active_count", &self.order.len())
            .field("has_focus", &self.focused().is_some())
            .field("captures_pointer", &self.captures_pointer())
            .finish()
    }
}

impl<K: Copy + Eq> Form<K> {
    fn rebuild_without_previous_focus(&mut self) {
        let old_focus = std::mem::take(&mut self.focus_tree);
        let mut builder = FocusBuilder::new(Some(old_focus));
        for id in &self.order {
            if let Some(control) = self.active_control(*id) {
                builder.leaf_widget(control);
            }
        }
        self.focus_tree = builder.build();
    }
}

fn is_tab(key: KeyEvent) -> bool {
    key.code == KeyCode::Tab || key.code == KeyCode::BackTab
}

fn is_back_tab(key: KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn list_selection(code: KeyCode, selected: usize, rows: usize) -> Option<usize> {
    if rows == 0 {
        return None;
    }
    let last = rows - 1;
    match code {
        KeyCode::Up => Some(selected.saturating_sub(1)),
        KeyCode::Down => Some((selected + 1).min(last)),
        KeyCode::Home => Some(0),
        KeyCode::End => Some(last),
        KeyCode::PageUp => Some(selected.saturating_sub(5)),
        KeyCode::PageDown => Some((selected + 5).min(last)),
        _ => None,
    }
}

fn page_target<K>(control: &Control<K>, current: usize, forward: bool) -> Option<usize> {
    let len = control.row_map.len();
    let area_rows = usize::from(control.area.height).max(1);
    let target = if forward {
        current.saturating_add(area_rows).min(len.saturating_sub(1))
    } else {
        current.saturating_sub(area_rows)
    };
    let mut range: Box<dyn Iterator<Item = usize>> = if forward {
        Box::new(target..len)
    } else {
        Box::new((0..=target).rev())
    };
    range.find(|row| {
        control.row_enabled.get(*row).copied().unwrap_or(true) && control.row_map[*row].is_some()
    })
}

fn tab_selection(code: KeyCode, selected: usize, tabs: usize) -> Option<usize> {
    if tabs == 0 {
        return None;
    }
    let last = tabs - 1;
    match code {
        KeyCode::Left => Some(selected.saturating_sub(1)),
        KeyCode::Right => Some((selected + 1).min(last)),
        KeyCode::Home => Some(0),
        KeyCode::End => Some(last),
        _ => None,
    }
}

fn cursor_at<K>(control: &Control<K>, x: u16, y: u16) -> usize {
    if !control.multiline_cursor_map.is_empty() {
        let row_distance = control
            .multiline_cursor_map
            .iter()
            .map(|(_, row, _)| row.abs_diff(y))
            .min()
            .unwrap_or(0);
        let column_distance = control
            .multiline_cursor_map
            .iter()
            .filter(|(_, row, _)| row.abs_diff(y) == row_distance)
            .map(|(column, _, _)| column.abs_diff(x))
            .min()
            .unwrap_or(0);
        return control
            .multiline_cursor_map
            .iter()
            .filter(|(column, row, _)| {
                row.abs_diff(y) == row_distance && column.abs_diff(x) == column_distance
            })
            .max_by_key(|(_, _, offset)| *offset)
            .map_or(0, |(_, _, offset)| *offset);
    }
    control
        .cursor_map
        .iter()
        .min_by_key(|(column, _)| column.abs_diff(x))
        .map_or(0, |(_, offset)| *offset)
}

/// Applies a field edit to the existing readline editor.
pub fn apply_field_edit(input: &mut TextInput, edit: FieldEdit) -> Outcome {
    match edit {
        FieldEdit::Key(key) => {
            let cursor = input.cursor();
            let outcome = input.handle_key(key);
            if input.cursor() != cursor {
                Outcome::Changed
            } else {
                match outcome {
                    EditOutcome::Unhandled => Outcome::Continue,
                    EditOutcome::Handled => Outcome::Unchanged,
                    EditOutcome::Changed => Outcome::Changed,
                }
            }
        }
        FieldEdit::Paste(text) => input
            .insert_str(&crate::hel_text_input::single_line_paste(&text))
            .into(),
        FieldEdit::Cursor(offset) => {
            let before = input.cursor();
            input.set_cursor(offset);
            if before == input.cursor() {
                Outcome::Unchanged
            } else {
                Outcome::Changed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn form() -> Form<u8> {
        let mut form = Form::new();
        form.register(1, ControlKind::TextField, Rect::new(0, 0, 5, 1), true);
        form.register(2, ControlKind::Button, Rect::new(0, 1, 5, 1), true);
        form.end_frame(1);
        form
    }

    #[test]
    fn focus_wraps_and_skips_disabled_controls() {
        let mut form = Form::new();
        form.register(1, ControlKind::Button, Rect::new(0, 0, 5, 1), true);
        form.register(2, ControlKind::Button, Rect::new(0, 1, 5, 1), false);
        form.register(3, ControlKind::Button, Rect::new(0, 2, 5, 1), true);
        form.end_frame(1);
        assert_eq!(form.focused(), Some(1));
        assert!(form.handle(&key(KeyCode::Tab)).outcome.is_consumed());
        assert_eq!(form.focused(), Some(3));
        form.handle(&key(KeyCode::Tab));
        assert_eq!(form.focused(), Some(1));
    }

    #[test]
    fn button_releases_activate_once_and_release_outside_cancels() {
        let mut form = Form::new();
        form.register(1, ControlKind::Button, Rect::new(0, 0, 5, 1), true);
        form.end_frame(1);
        let down = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        let up = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(form.handle(&down).action.is_none());
        assert!(form.captures_pointer());
        assert_eq!(form.handle(&up).action, Some(Interaction::Activate(1)));
        assert!(!form.captures_pointer());
    }

    #[test]
    fn field_space_is_editing_and_unicode_cursor_is_applied_by_editor() {
        let mut form = form();
        let space = key(KeyCode::Char(' '));
        assert_eq!(
            form.handle(&space).action,
            Some(Interaction::Edit(
                1,
                FieldEdit::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE,))
            ))
        );
        let mut input = TextInput::from_value("a👩‍💻b");
        assert_eq!(
            apply_field_edit(&mut input, FieldEdit::Cursor(2)),
            Outcome::Changed
        );
        assert_eq!(input.cursor(), 1);
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn requested_initial_focus_and_both_reverse_tab_encodings_work() {
        let mut form = Form::new();
        for id in 1..=3 {
            form.register(id, ControlKind::Button, Rect::default(), true);
        }
        form.end_frame(3);
        assert_eq!(form.focused(), Some(3));
        form.handle(&key(KeyCode::BackTab));
        assert_eq!(form.focused(), Some(2));
        form.handle(&Event::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(form.focused(), Some(1));
        form.handle(&key(KeyCode::BackTab));
        assert_eq!(form.focused(), Some(3));
    }

    #[test]
    fn focus_repairs_after_removal_and_disabling_every_control() {
        let mut form = form();
        form.focus(2);
        form.begin_frame();
        form.register(1, ControlKind::Button, Rect::default(), true);
        form.register(3, ControlKind::Button, Rect::default(), true);
        form.end_frame(1);
        assert_eq!(form.focused(), Some(3));
        form.begin_frame();
        form.register(1, ControlKind::Button, Rect::default(), false);
        form.register(3, ControlKind::Button, Rect::default(), false);
        form.end_frame(1);
        assert_eq!(form.focused(), None);
        assert_eq!(form.handle(&key(KeyCode::Enter)).action, None);
        form.begin_frame();
        form.register(1, ControlKind::Button, Rect::default(), true);
        form.end_frame(1);
        assert_eq!(form.focused(), Some(1));
    }

    #[test]
    fn pointer_release_outside_or_after_disappearance_never_activates() {
        let mut form = form();
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 1);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 1, 1);
        form.handle(&down);
        assert_eq!(form.focused(), Some(2));
        assert!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 20, 20))
                .action
                .is_none()
        );
        assert!(!form.captures_pointer());
        assert!(form.handle(&up).action.is_none());
        form.handle(&down);
        form.begin_frame();
        form.register(1, ControlKind::TextField, Rect::default(), true);
        form.end_frame(1);
        assert!(!form.captures_pointer());
        assert!(form.handle(&up).action.is_none());
    }

    #[test]
    fn metadata_updates_preserve_cursor_maps_and_pressed_controls() {
        let mut form = form();
        form.register_with_cursor_map(
            1,
            ControlKind::TextField,
            Rect::new(0, 0, 5, 1),
            true,
            vec![(0, 0), (3, 4)],
        );
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        form.begin_update();
        form.declare(1, ControlKind::TextField);
        form.declare(2, ControlKind::Button);
        form.end_frame(1);
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 1))
                .action,
            Some(Interaction::Activate(2))
        );
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 3, 0))
                .action,
            Some(Interaction::Edit(1, FieldEdit::Cursor(4)))
        );
    }

    #[test]
    fn capture_survives_metadata_reconciliation_before_redraw() {
        let mut form = form();
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        form.reset_geometry();
        form.begin_update();
        form.declare(1, ControlKind::TextField);
        form.declare(2, ControlKind::Button);
        form.end_frame(1);
        assert!(form.captures_pointer());
        form.begin_frame();
        form.register(1, ControlKind::TextField, Rect::new(0, 0, 5, 1), true);
        form.register(2, ControlKind::Button, Rect::new(0, 1, 5, 1), true);
        form.end_frame(1);
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 1))
                .action,
            Some(Interaction::Activate(2))
        );
    }

    #[test]
    fn changed_list_metadata_drops_obsolete_row_mappings() {
        let mut form = Form::new();
        form.register_with_rows(
            1,
            ControlKind::ChoiceList {
                len: 2,
                selected: 0,
            },
            Rect::new(0, 0, 4, 2),
            true,
            vec![Some(0), Some(1)],
            vec![true, true],
        );
        form.end_frame(1);
        form.begin_update();
        form.declare(
            1,
            ControlKind::ChoiceList {
                len: 4,
                selected: 0,
            },
        );
        form.end_frame(1);
        assert_eq!(
            form.handle(&key(KeyCode::End)).action,
            Some(Interaction::Select(1, 3))
        );
        assert!(!form.contains(1, 1));
    }

    #[test]
    fn disabled_click_is_consumed_without_changing_focus() {
        let mut form = form();
        form.declare_with_enabled(2, ControlKind::Button, false);
        form.end_frame(1);
        let result = form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        assert!(result.is_consumed());
        assert_eq!(result.action, None);
        assert_eq!(form.focused(), Some(1));
    }

    #[test]
    fn activation_ignores_repeat_release_and_modified_space() {
        let mut form = form();
        form.focus(2);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(
                form.handle(&Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                    kind
                )))
                .action
                .is_none()
            );
        }
        assert!(
            form.handle(&Event::Key(KeyEvent::new(
                KeyCode::Char(' '),
                KeyModifiers::CONTROL
            )))
            .action
            .is_none()
        );
        assert_eq!(
            form.handle(&key(KeyCode::Char(' '))).action,
            Some(Interaction::Activate(2))
        );
    }

    #[test]
    fn activation_keeps_its_action_when_the_form_does_not_repaint() {
        let mut form = form();
        form.focus(2);

        let result = form.handle(&key(KeyCode::Enter));

        assert_eq!(result.outcome, Outcome::Unchanged);
        assert_eq!(result.action, Some(Interaction::Activate(2)));
    }

    #[test]
    fn clamped_navigation_and_repeated_focus_are_consumed_without_changed() {
        let mut form = Form::new();
        form.register(
            1,
            ControlKind::ChoiceList {
                len: 2,
                selected: 1,
            },
            Rect::new(0, 0, 5, 2),
            true,
        );
        form.end_frame(1);

        let result = form.handle(&key(KeyCode::Down));
        assert_eq!(result.outcome, Outcome::Unchanged);
        assert_eq!(result.action, Some(Interaction::Select(1, 1)));

        let result = form.handle(&key(KeyCode::Up));
        assert_eq!(result.outcome, Outcome::Changed);
        assert_eq!(result.action, Some(Interaction::Select(1, 0)));
        assert_eq!(form.selected(1), Some(0));

        let mut only = Form::new();
        only.register(1, ControlKind::Button, Rect::new(0, 0, 5, 1), true);
        only.end_frame(1);
        let result = only.handle(&key(KeyCode::Tab));
        assert_eq!(result.outcome, Outcome::Unchanged);
        assert_eq!(only.focused(), Some(1));
    }

    #[test]
    fn pointer_press_and_release_report_only_the_visible_armed_change() {
        let mut form = form();
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 1);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 20, 20);

        assert_eq!(form.handle(&down).outcome, Outcome::Changed);
        assert!(form.captures_pointer());
        let result = form.handle(&up);
        assert_eq!(result.outcome, Outcome::Changed);
        assert_eq!(result.action, None);
        assert!(!form.captures_pointer());
    }

    #[test]
    fn disabled_click_and_noop_editing_remain_consumed_without_repaint() {
        let mut form = form();
        form.declare_with_enabled(2, ControlKind::Button, false);
        form.end_frame(1);
        let disabled = form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        assert_eq!(disabled.outcome, Outcome::Unchanged);

        let edit = form.handle(&key(KeyCode::Left));
        assert_eq!(edit.outcome, Outcome::Unchanged);
        assert_eq!(
            edit.action,
            Some(Interaction::Edit(
                1,
                FieldEdit::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            ))
        );
    }

    #[test]
    fn cloned_scopes_do_not_share_focus_flags() {
        let original = form();
        let mut clone = original.clone();
        clone.handle(&key(KeyCode::Tab));
        assert_eq!(clone.focused(), Some(2));
        assert_eq!(original.focused(), Some(1));
        clone.clear();
        assert_eq!(original.focused(), Some(1));
    }

    #[test]
    fn mapped_list_skips_headings_and_disabled_choices_in_keys_and_mouse() {
        let mut form = Form::new();
        form.register_with_rows(
            1,
            ControlKind::ChoiceList {
                len: 4,
                selected: 0,
            },
            Rect::new(0, 0, 12, 4),
            true,
            vec![None, Some(0), Some(1), Some(2)],
            vec![true, true, false, true],
        );
        form.end_frame(1);
        assert_eq!(
            form.handle(&key(KeyCode::Down)).action,
            Some(Interaction::Select(1, 2))
        );
        for y in [0, 2] {
            form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, y));
            assert_eq!(
                form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, y))
                    .action,
                None
            );
        }
        form.set_selected(1, 1);
        assert_eq!(form.handle(&key(KeyCode::Enter)).action, None);
        assert_eq!(
            form.handle(&mouse(MouseEventKind::ScrollDown, 1, 1)).action,
            Some(Interaction::Select(1, 2))
        );
    }

    #[test]
    fn dismiss_target_is_not_a_focus_stop_and_cancel_coexists_with_footer_button() {
        let mut form = Form::new();
        form.register(1, ControlKind::Button, Rect::new(10, 0, 8, 1), true);
        form.register_dismiss(Rect::new(1, 0, 3, 1), true);
        form.end_frame(1);
        assert_eq!(form.focused(), Some(1));
        form.handle(&key(KeyCode::Tab));
        assert_eq!(form.focused(), Some(1));

        assert!(
            form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0))
                .action
                .is_none()
        );
        assert!(form.dismiss_is_armed());
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
                .action,
            Some(Interaction::Cancel)
        );
        assert!(!form.dismiss_is_armed());
    }

    #[test]
    fn dismiss_drag_or_move_outside_disarms_without_cancel() {
        let mut form = Form::<u8>::new();
        form.register_dismiss(Rect::new(1, 0, 3, 1), true);
        form.end_frame(1);
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
        assert!(form.dismiss_is_armed());
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Moved, 20, 20)).action,
            None
        );
        assert!(!form.dismiss_is_armed());
        assert!(!form.captures_pointer());
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
                .action,
            None
        );
    }

    #[test]
    fn disabled_dismiss_target_is_visible_to_hit_testing_but_inert() {
        let mut form = Form::<u8>::new();
        form.register_dismiss(Rect::new(1, 0, 3, 1), false);
        form.end_frame(1);
        assert!(form.contains(2, 0));
        let result = form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
        assert_eq!(result.action, None);
        assert!(!form.captures_pointer());
    }

    #[test]
    fn dismiss_geometry_does_not_leave_a_stale_capture_across_frames() {
        let mut form = Form::<u8>::new();
        form.register_dismiss(Rect::new(1, 0, 3, 1), true);
        form.end_frame(1);
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
        form.reset_geometry();
        assert!(!form.contains(2, 0));
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
                .action,
            None
        );
        form.begin_frame();
        form.end_frame(1);
        assert!(!form.contains(2, 0));
        assert!(!form.captures_pointer());
    }

    #[test]
    fn combobox_keys_preview_without_commit_until_acceptance() {
        let mut form = Form::new();
        form.register(
            1,
            ControlKind::ComboBox {
                len: 3,
                selected: 0,
                expanded: false,
            },
            Rect::new(0, 0, 8, 1),
            true,
        );
        form.end_frame(1);
        assert_eq!(
            form.handle(&key(KeyCode::Enter)).action,
            Some(Interaction::Activate(1))
        );

        form.begin_frame();
        form.register(
            1,
            ControlKind::ComboBox {
                len: 3,
                selected: 0,
                expanded: true,
            },
            Rect::new(0, 0, 8, 1),
            true,
        );
        form.end_frame(1);
        assert_eq!(
            form.handle(&key(KeyCode::Down)).action,
            Some(Interaction::Select(1, 1))
        );
        assert_eq!(form.selected(1), Some(1));
        assert_eq!(
            form.handle(&key(KeyCode::Tab)).action,
            Some(Interaction::ComboBoxCommit(1, 1))
        );
    }

    #[test]
    fn expanded_combobox_escape_is_local_and_popup_rows_commit() {
        let mut form = Form::new();
        form.register_combobox(
            1,
            ControlKind::ComboBox {
                len: 3,
                selected: 0,
                expanded: true,
            },
            Rect::new(0, 0, 8, 1),
            true,
            Rect::new(0, 2, 10, 5),
            vec![None, Some(0), Some(1), Some(2), None],
        );
        form.end_frame(1);
        assert_eq!(
            form.handle(&mouse(MouseEventKind::ScrollDown, 1, 3)).action,
            Some(Interaction::Select(1, 1))
        );
        assert_eq!(
            form.handle(&key(KeyCode::Esc)).action,
            Some(Interaction::ComboBoxDismiss(1))
        );

        let mut form = Form::new();
        form.register_combobox(
            1,
            ControlKind::ComboBox {
                len: 3,
                selected: 0,
                expanded: true,
            },
            Rect::new(0, 0, 8, 1),
            true,
            Rect::new(0, 2, 10, 5),
            vec![None, Some(0), Some(1), Some(2), None],
        );
        form.end_frame(1);
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 4));
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 4))
                .action,
            Some(Interaction::ComboBoxCommit(1, 1))
        );
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 0))
                .action,
            Some(Interaction::ComboBoxDismiss(1))
        );
    }
}
