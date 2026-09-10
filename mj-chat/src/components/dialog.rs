//! Shared dialog interaction, draft protection, and presentation.

use std::ops::{Deref, DerefMut};

use crossterm::event::Event;
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};

use super::{ButtonRow, ControlKind, EventResult, Form, Interaction, Outcome};
use crate::{hel_modal, hel_selection::FrameSurfaces, theme};

/// The meaning of an action, independent of its label and position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRole {
    Cancel,
    Back,
    Secondary,
    Primary,
}

/// A dialog action declared once for keyboard behavior and presentation.
#[derive(Debug, Clone, Copy)]
pub struct DialogAction<'a, K> {
    pub id: K,
    pub label: &'a str,
    pub role: ActionRole,
    pub enabled: bool,
}

/// Persistent dialog state. Domain drafts and I/O remain with the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog<K: Copy + Eq> {
    form: Form<K>,
    dismiss_actions: Vec<K>,
    action_roles: Vec<(K, ActionRole)>,
    escape_action: Option<K>,
    dismissal_scopes: Vec<(K, String)>,
    submit: Vec<(K, K)>,
    baselines: Vec<(String, Vec<String>, bool)>,
    dirty: bool,
    dismissal_enabled: bool,
    submission_pending: bool,
    pending_dismissal: Option<Interaction<K>>,
    confirmation: Form<bool>,
    menu: bool,
    bounds: Rect,
}

impl<K: Copy + Eq> Default for Dialog<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq> Dialog<K> {
    pub fn new() -> Self {
        Self {
            form: Form::new(),
            dismiss_actions: Vec::new(),
            action_roles: Vec::new(),
            escape_action: None,
            dismissal_scopes: Vec::new(),
            submit: Vec::new(),
            baselines: Vec::new(),
            dirty: false,
            dismissal_enabled: true,
            submission_pending: false,
            pending_dismissal: None,
            confirmation: Form::new(),
            menu: false,
            bounds: Rect::default(),
        }
    }

    /// Compares editable values only. Call before handling edits and after rendering.
    pub fn track_draft(&mut self, values: Vec<String>) {
        self.track_draft_part("draft", values);
    }

    /// Tracks lazily populated editor fields without treating navigation as an edit.
    pub fn track_draft_part(&mut self, name: &str, values: Vec<String>) {
        if let Some((_, baseline, changed)) =
            self.baselines.iter_mut().find(|(key, _, _)| key == name)
        {
            *changed = *baseline != values;
        } else {
            self.baselines.push((name.to_owned(), values, false));
        }
        self.dirty = self.baselines.iter().any(|(_, _, changed)| *changed);
    }

    /// Chooses the innermost editor action for Escape and its close control.
    pub fn set_escape_action(&mut self, action: Option<K>) {
        self.escape_action = action;
    }

    pub fn set_dismissal_scope(&mut self, action: K, part: &str) {
        self.dismissal_scopes.retain(|(id, _)| *id != action);
        self.dismissal_scopes.push((action, part.to_owned()));
    }

    pub fn forget_draft_part(&mut self, part: &str) {
        self.baselines.retain(|(name, _, _)| name != part);
        self.dirty = self.baselines.iter().any(|(_, _, changed)| *changed);
    }

    /// Prevents duplicate submission while a supervised operation is pending.
    pub fn set_submission_pending(&mut self, pending: bool) {
        self.submission_pending = pending;
    }

    pub fn set_dirty(&mut self, dirty: bool) {
        self.dirty = dirty;
    }

    /// Starts a new editor baseline after a successful save or a change of task.
    pub fn reset_draft(&mut self) {
        self.baselines.clear();
        self.dirty = false;
    }

    /// Locks dismissal only while the owning operation cannot be rolled back.
    pub fn set_dismissal_enabled(&mut self, enabled: bool) {
        self.dismissal_enabled = enabled;
    }

    pub fn set_dismiss_actions(&mut self, actions: &[K]) {
        self.dismiss_actions = actions.to_vec();
    }

    pub fn set_action_role(&mut self, id: K, role: ActionRole) {
        self.action_roles.retain(|(control, _)| *control != id);
        self.action_roles.push((id, role));
        if role == ActionRole::Primary {
            self.form.set_default_action(id);
        }
    }

    /// Renders actions in the declared semantic order with the shared button geometry.
    pub fn render_actions(
        frame: &mut Frame<'_>,
        area: Rect,
        buttons: &[(K, &str, bool)],
        dialog: &mut Self,
    ) {
        let mut buttons = buttons.to_vec();
        if dialog.submission_pending {
            for (id, label, enabled) in &mut buttons {
                if dialog.form.is_default_action(*id) {
                    *label = "Working…";
                    *enabled = false;
                }
            }
        }
        buttons.sort_by_key(|(id, _, _)| {
            let role = dialog
                .action_roles
                .iter()
                .find(|(control, _)| control == id)
                .map(|(_, role)| *role)
                .unwrap_or_else(|| {
                    if dialog.form.is_default_action(*id) {
                        ActionRole::Primary
                    } else if dialog.dismiss_actions.contains(id) {
                        ActionRole::Cancel
                    } else {
                        ActionRole::Secondary
                    }
                });
            match role {
                ActionRole::Cancel => 0,
                ActionRole::Back => 1,
                ActionRole::Secondary => 2,
                ActionRole::Primary => 3,
            }
        });
        ButtonRow::render(frame, area, &buttons, &mut dialog.form);
    }

    /// Declares which action Enter in a single-line field invokes.
    pub fn set_submit(&mut self, field: K, action: K) {
        self.submit.retain(|(id, _)| *id != field);
        self.submit.push((field, action));
    }

    /// A command menu closes when clicked outside its frame; forms do not.
    pub fn set_menu(&mut self, menu: bool) {
        self.menu = menu;
    }
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }
    pub fn cancel_pointer(&mut self) {
        self.form.cancel_pointer();
        self.confirmation.cancel_pointer();
    }

    pub fn confirmation_open(&self) -> bool {
        self.pending_dismissal.is_some()
    }

    /// Both explicit Cancel actions and Escape pass through this protection.
    pub fn request_dismissal(&mut self, action: Interaction<K>) -> EventResult<Interaction<K>> {
        if !self.dismissal_enabled {
            return EventResult::handled();
        }
        let scope = match &action {
            Interaction::Activate(id) => self
                .dismissal_scopes
                .iter()
                .find(|(control, _)| control == id)
                .map(|(_, scope)| scope),
            _ => None,
        };
        let dirty = scope.map_or(self.dirty, |scope| {
            self.baselines
                .iter()
                .any(|(name, _, changed)| name == scope && *changed)
        });
        if !dirty {
            return EventResult::unchanged(Some(action));
        }
        self.form.cancel_pointer();
        self.pending_dismissal = Some(action);
        self.confirmation = Form::new();
        self.confirmation.declare(false, ControlKind::Button);
        self.confirmation.declare(true, ControlKind::Button);
        self.confirmation.end_frame(false);
        EventResult {
            outcome: Outcome::Changed,
            action: None,
        }
    }

    pub fn handle(&mut self, event: &Event) -> EventResult<Interaction<K>> {
        if self.confirmation_open() {
            let result = self.confirmation.handle(event);
            return match result.action {
                Some(Interaction::Activate(true)) => {
                    self.dirty = false;
                    EventResult::changed(self.pending_dismissal.take())
                }
                Some(Interaction::Cancel | Interaction::Activate(false)) => {
                    self.pending_dismissal = None;
                    EventResult::changed(None)
                }
                _ => EventResult {
                    outcome: result.outcome.max(Outcome::Unchanged),
                    action: None,
                },
            };
        }
        if let Event::Mouse(mouse) = event
            && self.menu
            && self.bounds.width > 0
            && !self.bounds.contains((mouse.column, mouse.row).into())
            && matches!(
                mouse.kind,
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            )
        {
            self.form.cancel_pointer();
            return EventResult::changed(Some(Interaction::Cancel));
        }
        let mut result = self.form.handle(event);
        if matches!(result.action, Some(Interaction::Cancel))
            && let Some(action) = self.escape_action
        {
            result.action = Some(Interaction::Activate(action));
        }
        if let Some(Interaction::Activate(id)) = result.action
            && let Some((_, target)) = self.submit.iter().find(|(field, _)| *field == id)
        {
            result.action = self
                .form
                .is_enabled(*target)
                .then_some(Interaction::Activate(*target));
        }
        if matches!(&result.action, Some(Interaction::Cancel))
            || matches!(&result.action, Some(Interaction::Activate(id)) if self.dismiss_actions.contains(id))
        {
            return self.request_dismissal(result.action.expect("dismissal action"));
        }
        if self.submission_pending && matches!(result.action, Some(Interaction::Activate(_))) {
            return EventResult::handled();
        }
        if matches!(
            result.action,
            Some(Interaction::Edit(..) | Interaction::Select(..) | Interaction::Toggle(..))
        ) {
            self.submission_pending = false;
        }
        result
    }

    /// Draws the reusable discard prompt above the owning dialog without losing its focus.
    pub fn render_confirmation(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        surfaces: &mut FrameSurfaces,
    ) {
        if !self.confirmation_open() {
            return;
        }
        let popup = hel_modal::centered_modal_fixed(frame, surfaces, 56, 7, area);
        let block = theme::modal().title(" Discard unsaved changes? ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let layout = DialogShell::layout(inner, 1);
        frame.render_widget(
            Paragraph::new("Your changes have not been saved.").wrap(Wrap { trim: false }),
            layout.body,
        );
        self.confirmation.begin_frame();
        ButtonRow::render(
            frame,
            layout.actions,
            &[(false, "Keep editing", true), (true, "Discard", true)],
            &mut self.confirmation,
        );
        self.confirmation.end_frame(false);
    }

    pub fn declare_actions(&mut self, actions: &[DialogAction<'_, K>]) {
        self.dismiss_actions.clear();
        for action in actions {
            self.set_action_role(action.id, action.role);
            self.form
                .declare_with_enabled(action.id, ControlKind::Button, action.enabled);
            if action.role == ActionRole::Cancel {
                self.dismiss_actions.push(action.id);
            }
            if action.role == ActionRole::Primary {
                self.form.set_default_action(action.id);
            }
        }
    }
}

impl<K: Copy + Eq> Deref for Dialog<K> {
    type Target = Form<K>;
    fn deref(&self) -> &Self::Target {
        &self.form
    }
}
impl<K: Copy + Eq> DerefMut for Dialog<K> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.form
    }
}

/// Common modal layout keeps actions out of the scrolling body.
pub struct DialogShell;
pub struct DialogLayout {
    pub body: Rect,
    pub status: Rect,
    pub actions: Rect,
}
impl DialogShell {
    pub fn layout(inner: Rect, status_rows: u16) -> DialogLayout {
        let footer = inner.height.min(1);
        let status = status_rows.min(inner.height.saturating_sub(footer));
        let body_height = inner.height.saturating_sub(footer + status);
        DialogLayout {
            body: Rect::new(inner.x, inner.y, inner.width, body_height),
            status: Rect::new(inner.x, inner.y + body_height, inner.width, status),
            actions: Rect::new(inner.x, inner.y + body_height + status, inner.width, footer),
        }
    }

    pub fn hints(commands: bool) -> Line<'static> {
        Line::styled(
            if commands {
                " ↑↓ browse · Click/Enter runs · Tab moves · Esc closes "
            } else {
                " ↑↓ select · Double-click/Enter opens · Tab moves · Esc back "
            },
            theme::muted(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn editor() -> Dialog<u8> {
        let mut dialog = Dialog::new();
        dialog.declare(1, ControlKind::TextField);
        dialog.declare_actions(&[
            DialogAction {
                id: 2,
                label: "Cancel",
                role: ActionRole::Cancel,
                enabled: true,
            },
            DialogAction {
                id: 3,
                label: "Save",
                role: ActionRole::Primary,
                enabled: true,
            },
        ]);
        dialog.set_submit(1, 3);
        dialog.end_frame(1);
        dialog.track_draft(vec!["original".into()]);
        dialog
    }

    #[test]
    fn dismissal_protects_draft_and_restores_field_focus() {
        let mut dialog = editor();
        dialog.track_draft(vec!["edited".into()]);
        assert!(dialog.handle(&key(KeyCode::Esc)).action.is_none());
        assert!(dialog.confirmation_open());
        assert!(dialog.handle(&key(KeyCode::Enter)).action.is_none());
        assert!(!dialog.confirmation_open());
        assert_eq!(dialog.focused(), Some(1));
        dialog.handle(&key(KeyCode::Esc));
        dialog.handle(&key(KeyCode::Right));
        assert_eq!(
            dialog.handle(&key(KeyCode::Enter)).action,
            Some(Interaction::Cancel)
        );
    }

    #[test]
    fn reverting_changes_closes_without_a_prompt_and_cancel_button_uses_same_guard() {
        let mut dialog = editor();
        dialog.track_draft(vec!["edited".into()]);
        dialog.focus(2);
        dialog.handle(&key(KeyCode::Enter));
        assert!(dialog.confirmation_open());
        dialog.handle(&key(KeyCode::Esc));
        dialog.track_draft(vec!["original".into()]);
        assert_eq!(
            dialog.handle(&key(KeyCode::Enter)).action,
            Some(Interaction::Activate(2))
        );
    }

    #[test]
    fn disabled_submit_never_activates_a_fallback_button() {
        let mut dialog = editor();
        dialog.declare_with_enabled(3, ControlKind::Button, false);
        assert_eq!(dialog.handle(&key(KeyCode::Enter)).action, None);
        assert_eq!(dialog.focused(), Some(1));
        dialog.focus(3);
        dialog.end_frame(1);
        assert_eq!(dialog.handle(&key(KeyCode::Enter)).action, None);
        dialog.handle(&key(KeyCode::Tab));
        assert!(!dialog.confirmation_open());
    }

    #[test]
    fn child_dismissal_does_not_discard_the_parent_draft() {
        let mut dialog = editor();
        dialog.declare(4, ControlKind::Button);
        dialog.set_dismiss_actions(&[2, 4]);
        dialog.set_escape_action(Some(4));
        dialog.set_dismissal_scope(4, "child");
        dialog.track_draft_part("child", vec!["unchanged".into()]);
        dialog.track_draft(vec!["edited parent".into()]);
        assert_eq!(
            dialog.handle(&key(KeyCode::Esc)).action,
            Some(Interaction::Activate(4))
        );
        dialog.set_escape_action(None);
        dialog.handle(&key(KeyCode::Esc));
        assert!(dialog.confirmation_open());
    }

    #[test]
    fn discard_actions_remain_visible_on_a_small_terminal() {
        let mut dialog = editor();
        dialog.track_draft(vec!["changed".into()]);
        dialog.handle(&key(KeyCode::Esc));
        let mut terminal = Terminal::new(TestBackend::new(45, 12)).unwrap();
        terminal
            .draw(|frame| {
                dialog.render_confirmation(frame, frame.area(), &mut FrameSurfaces::default())
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Keep editing"));
        assert!(text.contains("Discard"));
    }
}
