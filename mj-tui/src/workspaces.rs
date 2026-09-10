//! Workspace tabs and the non-blocking workspace manager.

use std::cell::RefCell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use hel::hel_workspace::WorkspaceRecord;
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, TextField};
use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::widgets::{centered_modal, dismissible_modal_title};
use crate::{DashboardAction, DashboardState, Mode};

/// A detached composer draft shown by the workspace manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDraftEntry {
    pub id: String,
    pub session_id: Option<String>,
    pub source: String,
    pub saved_at: String,
    pub owner_pid: Option<u32>,
}

/// The workspace and its detached drafts returned by the controller's
/// background management query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceManagementEntry {
    pub workspace: WorkspaceRecord,
    pub drafts: Vec<WorkspaceDraftEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceControlFocus {
    Tabs,
    Menu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceManagerView {
    List,
    Create,
    Rename {
        workspace_id: String,
    },
    Delete {
        workspace_id: String,
        workspace_name: String,
        session_count: u64,
        draft_count: usize,
    },
    Drafts {
        workspace_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceControl {
    List,
    DraftList,
    New,
    Open,
    Rename,
    Save,
    Delete,
    ConfirmDelete,
    Drafts,
    Name,
    Create,
    ForceDelete,
    Recover,
    Cancel,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceMutation {
    Load,
    Create,
    Rename,
    Delete,
    Recover,
}

/// State for the workspace manager. It only stores a snapshot and drafts; all
/// filesystem/database work is requested through [`DashboardAction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceManager {
    pub(crate) generation: u64,
    pub(crate) active_workspace_id: Option<String>,
    pub(crate) entries: Vec<WorkspaceManagementEntry>,
    pub(crate) selected: usize,
    pub(crate) selected_draft: usize,
    pub(crate) view: WorkspaceManagerView,
    pub(crate) name: TextInput,
    pub(crate) form: RefCell<Dialog<WorkspaceControl>>,
    pub(crate) loading: bool,
    pub(crate) busy: Option<WorkspaceMutation>,
    pub(crate) error: Option<String>,
    pub(crate) success: Option<String>,
}

fn manager_form() -> RefCell<Dialog<WorkspaceControl>> {
    RefCell::new(Dialog::new())
}

impl WorkspaceManager {
    pub(crate) fn prepare_dialog_state(&mut self) {
        self.sync_form();
        use WorkspaceControl::*;
        let form = self.form.get_mut();
        form.set_dismissal_enabled(
            self.busy
                .is_none_or(|operation| operation == WorkspaceMutation::Load),
        );
        form.set_action_role(Cancel, mj_chat::components::ActionRole::Cancel);
        form.set_action_role(Back, mj_chat::components::ActionRole::Back);
        match self.view {
            WorkspaceManagerView::Create | WorkspaceManagerView::Rename { .. } => {
                form.track_draft(vec![self.name.to_string()]);
                form.set_dismiss_actions(&[Cancel, Back]);
                let primary = if matches!(self.view, WorkspaceManagerView::Create) {
                    Create
                } else {
                    Save
                };
                form.set_default_action(primary);
                form.set_submit(Name, primary);
            }
            _ => {
                form.reset_draft();
                form.set_dismiss_actions(&[Cancel]);
            }
        }
    }

    pub(crate) fn loading(generation: u64, active_workspace_id: Option<String>) -> Self {
        Self {
            generation,
            active_workspace_id,
            entries: Vec::new(),
            selected: 0,
            selected_draft: 0,
            view: WorkspaceManagerView::List,
            name: TextInput::default(),
            form: manager_form(),
            loading: true,
            busy: Some(WorkspaceMutation::Load),
            error: None,
            success: None,
        }
    }

    pub(crate) fn selected_entry(&self) -> Option<&WorkspaceManagementEntry> {
        self.entries.get(self.selected)
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.entries.len().saturating_sub(1));
        self.selected_draft = 0;
        if matches!(self.view, WorkspaceManagerView::List)
            && let Some(entry) = self.selected_entry()
        {
            self.name = TextInput::from_value(entry.workspace.name.clone()).with_max_chars(64);
        }
    }

    fn view_workspace_id(&self) -> Option<&str> {
        match &self.view {
            WorkspaceManagerView::List | WorkspaceManagerView::Create => None,
            WorkspaceManagerView::Rename { workspace_id }
            | WorkspaceManagerView::Delete { workspace_id, .. }
            | WorkspaceManagerView::Drafts { workspace_id } => Some(workspace_id),
        }
    }

    fn viewed_entry(&self) -> Option<&WorkspaceManagementEntry> {
        let workspace_id = self.view_workspace_id()?;
        self.entries
            .iter()
            .find(|entry| entry.workspace.id == workspace_id)
    }

    fn force_delete_ready(&self) -> bool {
        let WorkspaceManagerView::Delete {
            workspace_name,
            session_count,
            draft_count,
            ..
        } = &self.view
        else {
            return false;
        };
        let typed = self.name.value().trim();
        typed == workspace_name && (*draft_count > 0 || *session_count > 0)
    }

    fn delete_is_destructive(&self) -> bool {
        matches!(
            self.view,
            WorkspaceManagerView::Delete {
                session_count,
                draft_count,
                ..
            } if session_count > 0 || draft_count > 0
        )
    }

    fn set_error(&mut self, error: String) {
        self.loading = false;
        self.busy = None;
        self.error = Some(error);
        self.success = None;
    }

    fn can_mutate(&self) -> bool {
        !self.loading && self.busy.is_none()
    }

    fn reset_to_list(&mut self) {
        self.view = WorkspaceManagerView::List;
        self.selected_draft = 0;
        self.error = None;
        self.success = None;
        if let Some(entry) = self.selected_entry() {
            self.name = TextInput::from_value(entry.workspace.name.clone()).with_max_chars(64);
        }
        self.form.get_mut().focus(WorkspaceControl::List);
    }

    fn actions(&self) -> Vec<(WorkspaceControl, &'static str, bool)> {
        use WorkspaceControl::*;
        let ready = self.can_mutate();
        let dismiss = self.busy.is_none();
        match &self.view {
            WorkspaceManagerView::List => {
                let selected = self.selected_entry();
                let mut actions = vec![
                    (Cancel, "Close", dismiss),
                    (Rename, "Rename", ready && selected.is_some()),
                    (Delete, "Delete", ready && selected.is_some()),
                ];
                if selected.is_some_and(|entry| !entry.drafts.is_empty()) {
                    actions.push((Drafts, "Drafts", ready));
                }
                actions.push((Open, "Open", ready && selected.is_some()));
                actions
            }
            WorkspaceManagerView::Create => vec![
                (Cancel, "Cancel", dismiss),
                (Back, "Back", dismiss),
                (Create, "Create", ready),
            ],
            WorkspaceManagerView::Rename { .. } => vec![
                (Cancel, "Cancel", dismiss),
                (Back, "Back", dismiss),
                (Save, "Save", ready),
            ],
            WorkspaceManagerView::Delete { .. } if self.delete_is_destructive() => vec![
                (Cancel, "Cancel", dismiss),
                (
                    ForceDelete,
                    "Force delete",
                    ready && self.force_delete_ready(),
                ),
            ],
            WorkspaceManagerView::Delete { .. } => vec![
                (Cancel, "Cancel", dismiss),
                (ConfirmDelete, "Delete", ready),
            ],
            WorkspaceManagerView::Drafts { .. } => vec![
                (Back, "Back", dismiss),
                (
                    Recover,
                    "Recover",
                    ready
                        && self
                            .viewed_entry()
                            .is_some_and(|entry| !entry.drafts.is_empty()),
                ),
            ],
        }
    }

    fn sync_form(&mut self) {
        use WorkspaceControl::*;
        use mj_chat::components::{ActionRole, DialogAction};
        let actions = self.actions();
        let draft_len = self.viewed_entry().map_or(0, |entry| entry.drafts.len());
        let ready = self.can_mutate();
        let destructive = self.delete_is_destructive();
        let form = self.form.get_mut();
        form.begin_update();
        let initial = match self.view {
            WorkspaceManagerView::List => {
                form.declare_with_enabled(New, ControlKind::Button, ready);
                form.declare_with_enabled(
                    List,
                    ControlKind::ChoiceList {
                        len: self.entries.len(),
                        selected: self.selected,
                    },
                    !self.entries.is_empty(),
                );
                if self.entries.is_empty() { New } else { List }
            }
            WorkspaceManagerView::Create | WorkspaceManagerView::Rename { .. } => {
                form.declare(Name, ControlKind::TextField);
                Name
            }
            WorkspaceManagerView::Delete { .. } => {
                if destructive {
                    form.declare(Name, ControlKind::TextField);
                }
                Cancel
            }
            WorkspaceManagerView::Drafts { .. } => {
                form.declare_with_enabled(
                    DraftList,
                    ControlKind::ChoiceList {
                        len: draft_len,
                        selected: self.selected_draft,
                    },
                    draft_len > 0,
                );
                form.set_list_identity(DraftList, format!("{:?}", self.view));
                DraftList
            }
        };
        let actions = actions
            .iter()
            .map(|(id, label, enabled)| DialogAction {
                id: *id,
                label,
                enabled: *enabled,
                role: match id {
                    Cancel => ActionRole::Cancel,
                    Back => ActionRole::Back,
                    Open | Create | Save | ConfirmDelete | ForceDelete | Recover => {
                        ActionRole::Primary
                    }
                    _ => ActionRole::Secondary,
                },
            })
            .collect::<Vec<_>>();
        form.declare_actions(&actions);
        form.end_frame(initial);
    }
}

fn workspace_manager_generation(mode: &Mode) -> Option<u64> {
    match mode {
        Mode::WorkspaceManager(manager) => Some(manager.generation),
        Mode::Help(overlay) => workspace_manager_generation(&overlay.return_to),
        _ => None,
    }
}

fn workspace_manager_in_mode(mode: &mut Mode) -> Option<&mut WorkspaceManager> {
    match mode {
        Mode::WorkspaceManager(manager) => Some(manager),
        Mode::Help(overlay) => workspace_manager_in_mode(overlay.return_to.as_mut()),
        _ => None,
    }
}

impl DashboardState {
    /// Handles the two keyboard stops inside the workspace pane. Tabs own
    /// left/right selection; the pinned menu is a second local stop before
    /// the ordinary dashboard Tab ring continues to Sessions.
    pub(crate) fn handle_workspace_pane_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if self.focus != crate::Focus::Workspaces
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        let back_tab = key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        match (self.workspace_control_focus, key.code, back_tab) {
            (WorkspaceControlFocus::Tabs, KeyCode::Tab, false) => {
                self.workspace_control_focus = WorkspaceControlFocus::Menu;
                self.surface_form
                    .borrow_mut()
                    .focus(crate::surface_controls::SurfaceControl::WorkspaceMenu);
                self.mark_render_changed();
                self.record_event_handled();
                Some(DashboardAction::None)
            }
            (WorkspaceControlFocus::Menu, KeyCode::Tab, false) => {
                self.cycle_focus(false);
                self.record_event_handled();
                Some(DashboardAction::None)
            }
            (WorkspaceControlFocus::Menu, _, true) => {
                self.workspace_control_focus = WorkspaceControlFocus::Tabs;
                self.mark_render_changed();
                self.record_event_handled();
                Some(DashboardAction::None)
            }
            (WorkspaceControlFocus::Tabs, _, true) => {
                self.cycle_focus(true);
                self.record_event_handled();
                Some(DashboardAction::None)
            }
            (WorkspaceControlFocus::Menu, KeyCode::Left, false) => {
                self.workspace_control_focus = WorkspaceControlFocus::Tabs;
                self.mark_render_changed();
                self.record_event_handled();
                Some(DashboardAction::None)
            }
            (WorkspaceControlFocus::Menu, KeyCode::Enter | KeyCode::Char(' '), false) => {
                self.record_event_handled();
                Some(self.dispatch_command(crate::CommandId::Workspaces))
            }
            (WorkspaceControlFocus::Tabs, KeyCode::Right, false) => {
                let ids = self.workspace_ids();
                let at_end = self
                    .active_workspace_id()
                    .and_then(|active| ids.iter().position(|id| id == active))
                    .is_some_and(|index| index + 1 >= ids.len());
                if at_end {
                    self.workspace_control_focus = WorkspaceControlFocus::Menu;
                    self.surface_form
                        .borrow_mut()
                        .focus(crate::surface_controls::SurfaceControl::WorkspaceMenu);
                    self.mark_render_changed();
                    self.record_event_handled();
                    Some(DashboardAction::None)
                } else {
                    self.record_event_handled();
                    Some(self.select_adjacent_workspace(1))
                }
            }
            (WorkspaceControlFocus::Tabs, KeyCode::Left, false) => {
                self.record_event_handled();
                Some(self.select_adjacent_workspace(-1))
            }
            _ => None,
        }
    }

    pub(crate) fn select_adjacent_workspace(&self, delta: isize) -> DashboardAction {
        let ids = self.workspace_ids();
        let Some(active) = self.active_workspace_id() else {
            return DashboardAction::None;
        };
        let Some(index) = ids.iter().position(|id| id == active) else {
            return DashboardAction::None;
        };
        let next = if delta < 0 {
            index.checked_sub(delta.unsigned_abs())
        } else {
            Some(index.saturating_add(delta as usize))
        };
        let Some(next) = next.filter(|next| *next < ids.len()) else {
            return DashboardAction::None;
        };
        DashboardAction::SelectWorkspace {
            workspace_id: ids[next].clone(),
        }
    }

    /// Opens the manager and requests its first snapshot off the UI loop.
    pub fn begin_workspace_manager(&mut self) -> DashboardAction {
        self.workspace_management_generation = self.workspace_management_generation.wrapping_add(1);
        let generation = self.workspace_management_generation;
        self.mode = Mode::WorkspaceManager(WorkspaceManager::loading(
            generation,
            self.active_workspace_id.clone(),
        ));
        self.mark_render_changed();
        DashboardAction::LoadWorkspaceManagement { generation }
    }

    /// Whether a background workspace query or mutation may still update the
    /// visible manager. This has the same mode and generation guard as
    /// [`Self::finish_workspace_management`].
    pub fn workspace_management_is_current(&self, generation: u64) -> bool {
        generation == self.workspace_management_generation
            && workspace_manager_generation(&self.mode) == Some(generation)
    }

    /// Installs a manager snapshot, ignoring replies from an older modal or
    /// mutation. An error remains visible in the responsive modal.
    pub fn finish_workspace_management(
        &mut self,
        generation: u64,
        result: Result<Vec<WorkspaceManagementEntry>, String>,
    ) -> bool {
        if !self.workspace_management_is_current(generation) {
            return false;
        }
        let active_workspace_id = self.active_workspace_id.clone();
        let foreground = matches!(self.mode, Mode::WorkspaceManager(_));
        let Some(manager) = workspace_manager_in_mode(&mut self.mode) else {
            return false;
        };
        match result {
            Ok(entries) => {
                let previous_busy = manager.busy;
                let was_loading = manager.loading;
                let previous_error = manager.error.clone();
                let previous_selected = manager.selected;
                let previous_view_id = manager.view_workspace_id().map(str::to_owned);
                let previous_selected_id = manager
                    .selected_entry()
                    .map(|entry| entry.workspace.id.clone());
                let previous_entries = std::mem::replace(&mut manager.entries, entries);
                let workspace_id = previous_view_id
                    .or(previous_selected_id)
                    .or(active_workspace_id);
                let selected = workspace_id
                    .as_deref()
                    .and_then(|id| {
                        manager
                            .entries
                            .iter()
                            .position(|entry| entry.workspace.id == id)
                    })
                    .unwrap_or_else(|| {
                        manager
                            .selected
                            .min(manager.entries.len().saturating_sub(1))
                    });
                let previous_draft_id = previous_entries
                    .get(manager.selected)
                    .and_then(|entry| entry.drafts.get(manager.selected_draft))
                    .map(|draft| draft.id.clone());
                let old_view = manager.view.clone();
                manager.selected = selected;
                manager.selected_draft = previous_draft_id
                    .as_deref()
                    .and_then(|id| {
                        manager
                            .selected_entry()
                            .and_then(|entry| entry.drafts.iter().position(|draft| draft.id == id))
                    })
                    .unwrap_or(0);
                if manager
                    .view_workspace_id()
                    .is_some_and(|id| !manager.entries.iter().any(|entry| entry.workspace.id == id))
                {
                    manager.view = WorkspaceManagerView::List;
                }
                if matches!(
                    previous_busy,
                    Some(
                        WorkspaceMutation::Create
                            | WorkspaceMutation::Rename
                            | WorkspaceMutation::Delete
                    )
                ) {
                    manager.view = WorkspaceManagerView::List;
                }
                if matches!(previous_busy, Some(WorkspaceMutation::Recover)) {
                    manager.success = Some("Draft recovered.".into());
                } else {
                    manager.success = None;
                }
                manager.loading = false;
                manager.busy = None;
                manager.error = None;
                if matches!(manager.view, WorkspaceManagerView::List) {
                    manager.name = manager
                        .selected_entry()
                        .map(|entry| {
                            TextInput::from_value(entry.workspace.name.clone()).with_max_chars(64)
                        })
                        .unwrap_or_else(|| TextInput::default().with_max_chars(64));
                }
                manager.sync_form();
                let changed = manager.entries != previous_entries
                    || manager.selected != previous_selected
                    || was_loading
                    || previous_busy.is_some()
                    || manager.view != old_view
                    || previous_error.is_some();
                if changed {
                    self.mark_render_changed();
                }
                return foreground;
            }
            Err(error) => {
                let changed = manager.loading
                    || manager.busy.is_some()
                    || manager.error.as_deref() != Some(error.as_str());
                manager.set_error(error);
                if changed {
                    self.mark_render_changed();
                }
            }
        }
        true
    }

    pub(crate) fn handle_workspace_manager_event(&mut self, event: Event) -> DashboardAction {
        let Mode::WorkspaceManager(manager) = &mut self.mode else {
            return DashboardAction::None;
        };
        let result = manager.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        match result.action {
            Some(Interaction::Cancel) => {
                let locked = manager
                    .busy
                    .is_some_and(|mutation| mutation != WorkspaceMutation::Load);
                if !locked {
                    if matches!(manager.view, WorkspaceManagerView::List) {
                        self.cancel_modal();
                    } else {
                        manager.reset_to_list();
                        self.mark_render_changed();
                    }
                }
            }
            Some(Interaction::Edit(WorkspaceControl::Name, edit)) => {
                if TextField::apply(&mut manager.name, edit)
                    == mj_chat::components::Outcome::Changed
                {
                    manager.error = None;
                    manager.success = None;
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                }
            }
            Some(Interaction::Select(WorkspaceControl::List, index)) => {
                let previous = manager.selected;
                manager.select(index);
                if manager.selected != previous {
                    manager.error = None;
                    manager.success = None;
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                }
            }
            Some(Interaction::Select(WorkspaceControl::DraftList, index)) => {
                if manager.selected_draft != index {
                    manager.selected_draft = index;
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                }
            }
            Some(Interaction::Activate(control)) => match control {
                WorkspaceControl::List | WorkspaceControl::Open => {
                    if manager.busy.is_none()
                        && let Some(workspace_id) = manager
                            .selected_entry()
                            .map(|entry| entry.workspace.id.clone())
                    {
                        self.cancel_modal();
                        return DashboardAction::SelectWorkspace { workspace_id };
                    }
                }
                WorkspaceControl::New if manager.can_mutate() => {
                    manager.view = WorkspaceManagerView::Create;
                    manager.name = TextInput::default().with_max_chars(64);
                    manager.error = None;
                    manager.success = None;
                    manager.form.get_mut().focus(WorkspaceControl::Name);
                    self.mark_render_changed();
                }
                WorkspaceControl::Rename if manager.can_mutate() => {
                    if let Some((workspace_id, workspace_name)) = manager
                        .selected_entry()
                        .map(|entry| (entry.workspace.id.clone(), entry.workspace.name.clone()))
                    {
                        manager.view = WorkspaceManagerView::Rename { workspace_id };
                        manager.name = TextInput::from_value(workspace_name).with_max_chars(64);
                        manager.error = None;
                        manager.success = None;
                        manager.form.get_mut().focus(WorkspaceControl::Name);
                        self.mark_render_changed();
                    }
                }
                WorkspaceControl::Delete if manager.can_mutate() => {
                    if let Some(entry) = manager.selected_entry() {
                        manager.view = WorkspaceManagerView::Delete {
                            workspace_id: entry.workspace.id.clone(),
                            workspace_name: entry.workspace.name.clone(),
                            session_count: entry.workspace.session_count,
                            draft_count: entry.drafts.len(),
                        };
                        manager.name = TextInput::default().with_max_chars(64);
                        manager.error = None;
                        manager.success = None;
                        if manager.delete_is_destructive() {
                            manager.form.get_mut().focus(WorkspaceControl::Name);
                        } else {
                            manager.form.get_mut().focus(WorkspaceControl::Cancel);
                        }
                        self.mark_render_changed();
                    }
                }
                WorkspaceControl::Drafts if manager.can_mutate() => {
                    if let Some(entry) = manager.selected_entry() {
                        manager.view = WorkspaceManagerView::Drafts {
                            workspace_id: entry.workspace.id.clone(),
                        };
                        manager.selected_draft = 0;
                        manager.error = None;
                        manager.success = None;
                        manager.form.get_mut().focus(WorkspaceControl::DraftList);
                        self.mark_render_changed();
                    }
                }
                WorkspaceControl::Back => {
                    if matches!(manager.view, WorkspaceManagerView::List) {
                        self.cancel_modal();
                    } else {
                        manager.reset_to_list();
                        self.mark_render_changed();
                    }
                }
                WorkspaceControl::Name => {
                    let next = match manager.view {
                        WorkspaceManagerView::Create => WorkspaceControl::Create,
                        WorkspaceManagerView::Rename { .. } => WorkspaceControl::Save,
                        WorkspaceManagerView::Delete { .. } if manager.delete_is_destructive() => {
                            WorkspaceControl::ForceDelete
                        }
                        WorkspaceManagerView::Delete { .. } => WorkspaceControl::ConfirmDelete,
                        _ => WorkspaceControl::Cancel,
                    };
                    manager.form.get_mut().focus(next);
                }
                WorkspaceControl::Create => {
                    return self.workspace_manager_mutation(WorkspaceMutation::Create);
                }
                WorkspaceControl::Save => {
                    return self.workspace_manager_mutation(WorkspaceMutation::Rename);
                }
                WorkspaceControl::ConfirmDelete | WorkspaceControl::ForceDelete => {
                    return self.workspace_manager_mutation(WorkspaceMutation::Delete);
                }
                WorkspaceControl::DraftList | WorkspaceControl::Recover => {
                    return self.workspace_manager_mutation(WorkspaceMutation::Recover);
                }
                WorkspaceControl::Cancel => {
                    if matches!(manager.view, WorkspaceManagerView::Delete { .. }) {
                        manager.reset_to_list();
                        self.mark_render_changed();
                    } else {
                        self.cancel_modal();
                    }
                }
                _ => {}
            },
            _ => {}
        }
        DashboardAction::None
    }

    fn workspace_manager_mutation(&mut self, mutation: WorkspaceMutation) -> DashboardAction {
        let Mode::WorkspaceManager(manager) = &mut self.mode else {
            return DashboardAction::None;
        };
        if !manager.can_mutate() {
            return DashboardAction::None;
        }
        let generation = manager.generation;
        let action = match mutation {
            WorkspaceMutation::Create => {
                let name = manager.name.trim().to_owned();
                if name.is_empty() {
                    manager.error = Some("Workspace name cannot be empty.".into());
                    self.mark_render_changed();
                    return DashboardAction::None;
                }
                DashboardAction::CreateWorkspace { generation, name }
            }
            WorkspaceMutation::Rename => {
                let WorkspaceManagerView::Rename { workspace_id } = &manager.view else {
                    return DashboardAction::None;
                };
                let name = manager.name.trim().to_owned();
                if name.is_empty() {
                    manager.error = Some("Workspace name cannot be empty.".into());
                    self.mark_render_changed();
                    return DashboardAction::None;
                }
                DashboardAction::RenameWorkspace {
                    generation,
                    workspace_id: workspace_id.clone(),
                    name,
                }
            }
            WorkspaceMutation::Delete => {
                let WorkspaceManagerView::Delete {
                    workspace_id,
                    session_count,
                    draft_count,
                    ..
                } = &manager.view
                else {
                    return DashboardAction::None;
                };
                let destructive = *session_count > 0 || *draft_count > 0;
                if destructive && !manager.force_delete_ready() {
                    manager.error =
                        Some("Type the workspace name exactly to enable Force delete.".into());
                    manager.form.get_mut().focus(WorkspaceControl::Name);
                    self.mark_render_changed();
                    return DashboardAction::None;
                }
                DashboardAction::DeleteWorkspace {
                    generation,
                    workspace_id: workspace_id.clone(),
                    force: destructive,
                }
            }
            WorkspaceMutation::Recover => {
                let Some(draft) = manager
                    .viewed_entry()
                    .and_then(|entry| entry.drafts.get(manager.selected_draft))
                else {
                    manager.error = Some("This workspace has no detached drafts.".into());
                    self.mark_render_changed();
                    return DashboardAction::None;
                };
                DashboardAction::RecoverWorkspaceDraft {
                    generation,
                    draft_id: draft.id.clone(),
                }
            }
            WorkspaceMutation::Load => return DashboardAction::None,
        };
        manager.busy = Some(mutation);
        manager.error = None;
        manager.success = None;
        self.mark_render_changed();
        action
    }
}

/// Handles clicks on the workspace pane before row and pane hitboxes.
pub(crate) fn workspace_tab_click(
    dashboard: &mut DashboardState,
    column: u16,
    row: u16,
) -> Option<DashboardAction> {
    let workspace_id = dashboard
        .workspace_tab_areas
        .iter()
        .find(|(_, area)| {
            column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
        })
        .map(|(workspace_id, _)| workspace_id.clone());
    if let Some(area) = dashboard.workspace_pane_area
        && column >= area.x
        && column < area.right()
        && row >= area.y
        && row < area.bottom()
        && dashboard.focus != crate::Focus::Workspaces
    {
        dashboard.focus = crate::Focus::Workspaces;
        dashboard.mark_render_changed();
        dashboard.set_session_action_focus(None);
    }
    let workspace_id = workspace_id?;
    if dashboard.focus != crate::Focus::Workspaces {
        dashboard.focus = crate::Focus::Workspaces;
        dashboard.mark_render_changed();
    }
    if dashboard.workspace_control_focus != WorkspaceControlFocus::Tabs {
        dashboard.workspace_control_focus = WorkspaceControlFocus::Tabs;
        dashboard.mark_render_changed();
    }
    (dashboard.active_workspace_id() != Some(workspace_id.as_str()))
        .then_some(DashboardAction::SelectWorkspace { workspace_id })
}

/// Draws the bordered workspace list and records its hitboxes for the next
/// mouse event. The selected tab is kept visible when the list is wider than
/// the pane, and every visible label retains one cell of padding on either
/// side.
pub(crate) fn render_workspace_tabs(frame: &mut Frame, area: Rect, dashboard: &mut DashboardState) {
    dashboard.clear_workspace_tab_areas();
    dashboard.workspace_pane_area = (area.height > 0 && area.width > 0).then_some(area);
    if area.height == 0 || area.width == 0 {
        return;
    }
    let focused = dashboard.focus() == crate::Focus::Workspaces;
    let block = theme::panel(focused).title(" Workspaces ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let menu_width = 3u16.min(inner.width);
    let menu_area = Rect::new(
        inner.right().saturating_sub(menu_width),
        inner.y,
        menu_width,
        1,
    );
    if menu_width == 3 {
        dashboard.workspace_hamburger_area = Some(menu_area);
        crate::surface_controls::render_workspace_menu(frame, menu_area, dashboard);
    }
    let tabs_width = inner.width.saturating_sub(menu_width);
    let tabs_area = Rect::new(inner.x, inner.y, tabs_width, 1);
    let ids = dashboard.workspace_ids();
    let labels = ids
        .iter()
        .map(|id| dashboard.workspace_display_name(id).to_owned())
        .collect::<Vec<_>>();
    let widths = labels
        .iter()
        .map(|label| Line::raw(format!(" {label} ")).width())
        .collect::<Vec<_>>();
    let selected = ids
        .iter()
        .position(|id| Some(id.as_str()) == dashboard.active_workspace_id())
        .unwrap_or(0);
    let mut first = 0;
    let mut selected_width = widths.iter().take(selected + 1).sum::<usize>();
    while first < selected && selected_width > usize::from(tabs_area.width) {
        selected_width = selected_width.saturating_sub(widths[first]);
        first += 1;
    }
    let mut x = tabs_area.x;
    for (index, id) in ids.iter().enumerate().skip(first) {
        let width = widths[index].min(usize::from(tabs_area.right().saturating_sub(x))) as u16;
        if width == 0 {
            break;
        }
        let tab_area = Rect::new(x, inner.y, width, 1);
        dashboard.register_workspace_tab_area(id.clone(), tab_area);
        let label = workspace_label(&labels[index], width);
        let style = if index == selected {
            theme::selection(true)
        } else {
            theme::muted()
        };
        frame.render_widget(Paragraph::new(label).style(style), tab_area);
        x = x.saturating_add(width);
    }
}

fn workspace_label(name: &str, width: u16) -> String {
    if width <= 2 {
        return " ".repeat(usize::from(width));
    }
    let content_width = usize::from(width.saturating_sub(2));
    let full = Line::raw(name).width();
    let content = if full <= content_width {
        name.to_owned()
    } else if content_width == 0 {
        String::new()
    } else if content_width == 1 {
        "…".to_owned()
    } else {
        let mut clipped = String::new();
        for character in name.chars() {
            let candidate = format!("{clipped}{character}…");
            if Line::raw(candidate.as_str()).width() > content_width {
                break;
            }
            clipped.push(character);
        }
        clipped.push('…');
        clipped
    };
    format!(" {content} ")
}

/// The standard Dialog modal renderer used by the parent combined renderer.
pub(crate) fn render_workspace_manager(
    frame: &mut Frame,
    area: Rect,
    dialog: &WorkspaceManager,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 72, area.height.min(24), area);
    let title = match &dialog.view {
        WorkspaceManagerView::List => " Workspaces ",
        WorkspaceManagerView::Create => " Workspaces · New ",
        WorkspaceManagerView::Rename { .. } => " Workspaces · Rename ",
        WorkspaceManagerView::Delete { .. } => " Workspaces · Delete ",
        WorkspaceManagerView::Drafts { .. } => " Workspaces · Drafts ",
    };
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height < 5 {
        dialog.form.borrow_mut().reset_geometry();
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let message = if dialog.loading {
        "Loading workspaces…".to_owned()
    } else if let Some(error) = &dialog.error {
        error.clone()
    } else if let Some(success) = &dialog.success {
        success.clone()
    } else {
        match &dialog.view {
            WorkspaceManagerView::List => {
                "Enter opens the selected workspace · Tab moves between controls".into()
            }
            WorkspaceManagerView::Create => {
                "Enter creates the workspace · Esc closes manager".into()
            }
            WorkspaceManagerView::Rename { .. } => {
                "Enter saves the new name · Esc closes manager".into()
            }
            WorkspaceManagerView::Delete { .. } => {
                "Every deletion requires confirmation · Esc closes manager".into()
            }
            WorkspaceManagerView::Drafts { .. } => {
                "Enter recovers the selected draft · Esc closes manager".into()
            }
        }
    };
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(if dialog.error.is_some() {
            theme::palette().error
        } else if dialog.success.is_some() {
            theme::palette().success
        } else {
            theme::palette().muted
        })),
        rows[0],
    );
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let dismiss_enabled = dialog
        .busy
        .is_none_or(|mutation| mutation == WorkspaceMutation::Load);
    let title_line = dismissible_modal_title(
        &mut form,
        popup,
        title.trim(),
        theme::title(true),
        dismiss_enabled,
    );
    frame.render_widget(theme::modal().title(title_line), popup);
    match &dialog.view {
        WorkspaceManagerView::List => render_manager_list(frame, &rows, dialog, &mut form),
        WorkspaceManagerView::Create => render_manager_name_view(
            frame,
            &rows,
            dialog,
            &mut form,
            "New workspace name:",
            WorkspaceControl::Create,
            "Create",
        ),
        WorkspaceManagerView::Rename { .. } => render_manager_name_view(
            frame,
            &rows,
            dialog,
            &mut form,
            "Workspace name:",
            WorkspaceControl::Save,
            "Save",
        ),
        WorkspaceManagerView::Delete {
            workspace_name,
            session_count,
            draft_count,
            ..
        } => render_manager_delete(
            frame,
            &rows,
            dialog,
            &mut form,
            workspace_name,
            *session_count,
            *draft_count,
        ),
        WorkspaceManagerView::Drafts { .. } => {
            render_manager_drafts(frame, &rows, dialog, &mut form)
        }
    }
    let busy = dialog.busy.map(|mutation| format!("Working: {mutation:?}"));
    frame.render_widget(
        Paragraph::new(busy.unwrap_or_default()).style(Style::default().fg(theme::palette().muted)),
        rows[3],
    );
    let initial = match &dialog.view {
        WorkspaceManagerView::List if dialog.entries.is_empty() => WorkspaceControl::New,
        WorkspaceManagerView::List => WorkspaceControl::List,
        WorkspaceManagerView::Create | WorkspaceManagerView::Rename { .. } => {
            WorkspaceControl::Name
        }
        WorkspaceManagerView::Delete { .. } if dialog.delete_is_destructive() => {
            WorkspaceControl::Name
        }
        WorkspaceManagerView::Delete { .. } => WorkspaceControl::Cancel,
        WorkspaceManagerView::Drafts { .. } => WorkspaceControl::DraftList,
    };
    form.end_frame(initial);
}

fn render_manager_list(
    frame: &mut Frame,
    rows: &[Rect],
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
) {
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(rows[1]);
    Dialog::render_actions(
        frame,
        body[0],
        &[(WorkspaceControl::New, "New workspace", dialog.can_mutate())],
        form,
    );
    let list_items = dialog
        .entries
        .iter()
        .map(|entry| {
            let current =
                if dialog.active_workspace_id.as_deref() == Some(entry.workspace.id.as_str()) {
                    "  Current"
                } else {
                    ""
                };
            let sessions = format!(
                "{} session{}",
                entry.workspace.session_count,
                if entry.workspace.session_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
            let drafts = if entry.drafts.is_empty() {
                String::new()
            } else {
                format!(
                    "  {} draft{}",
                    entry.drafts.len(),
                    if entry.drafts.len() == 1 { "" } else { "s" }
                )
            };
            Line::from(format!(
                "{}{}  {}{}",
                entry.workspace.name, current, sessions, drafts
            ))
        })
        .collect::<Vec<_>>();
    if list_items.is_empty() {
        frame.render_widget(Paragraph::new("No workspaces."), body[1]);
        form.register(
            WorkspaceControl::List,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            body[1],
            false,
        );
    } else {
        ChoiceList::render(
            frame,
            body[1],
            &list_items,
            dialog.selected,
            form,
            WorkspaceControl::List,
        );
    }
    Dialog::render_actions(frame, rows[2], &dialog.actions(), form);
}

fn render_manager_name_view(
    frame: &mut Frame,
    rows: &[Rect],
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
    label: &str,
    _submit: WorkspaceControl,
    _submit_label: &str,
) {
    let field = Rect::new(rows[1].x, rows[1].y, rows[1].width, 1);
    let label_width = Line::raw(label).width() as u16 + 1;
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            label,
            Style::default().add_modifier(Modifier::BOLD),
        )])),
        Rect::new(field.x, field.y, label_width.min(field.width), 1),
    );
    TextField::render(
        frame,
        Rect::new(
            field.x.saturating_add(label_width),
            field.y,
            field.width.saturating_sub(label_width),
            1,
        ),
        &dialog.name,
        form,
        WorkspaceControl::Name,
    );
    Dialog::render_actions(frame, rows[2], &dialog.actions(), form);
}

fn render_manager_delete(
    frame: &mut Frame,
    rows: &[Rect],
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
    workspace_name: &str,
    session_count: u64,
    draft_count: usize,
) {
    let destructive = session_count > 0 || draft_count > 0;
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if destructive { 2 } else { 1 }),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(rows[1]);
    let explanation = if destructive {
        format!(
            "Deleting {workspace_name:?} destroys {session_count} session{} and discards {draft_count} draft{}.",
            if session_count == 1 { "" } else { "s" },
            if draft_count == 1 { "" } else { "s" },
        )
    } else {
        format!("Delete empty workspace {workspace_name:?}?")
    };
    frame.render_widget(Paragraph::new(explanation), body[0]);
    if destructive {
        frame.render_widget(
            Paragraph::new("Type the exact workspace name to confirm:"),
            body[1],
        );
        TextField::render(frame, body[2], &dialog.name, form, WorkspaceControl::Name);
    }
    Dialog::render_actions(frame, rows[2], &dialog.actions(), form);
}

fn render_manager_drafts(
    frame: &mut Frame,
    rows: &[Rect],
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
) {
    let drafts = dialog
        .viewed_entry()
        .map(|entry| {
            entry
                .drafts
                .iter()
                .map(|draft| {
                    Line::from(format!(
                        "{}  · {}{}",
                        draft.source,
                        draft.saved_at,
                        draft
                            .session_id
                            .as_deref()
                            .map_or(String::new(), |id| format!("  · {id}")),
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if drafts.is_empty() {
        frame.render_widget(Paragraph::new("No detached drafts."), rows[1]);
        form.register(
            WorkspaceControl::DraftList,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            rows[1],
            false,
        );
    } else {
        ChoiceList::render(
            frame,
            rows[1],
            &drafts,
            dialog.selected_draft,
            form,
            WorkspaceControl::DraftList,
        );
    }
    Dialog::render_actions(frame, rows[2], &dialog.actions(), form);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{buffer_lines, cell_column, dashboard_with_session, running_session};
    use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use hel::hel_workspace::WorkspaceRecord;

    fn entry(id: &str, name: &str) -> WorkspaceManagementEntry {
        WorkspaceManagementEntry {
            workspace: WorkspaceRecord {
                id: id.into(),
                name: name.into(),
                created_at: String::new(),
                last_opened_at: String::new(),
                session_count: 0,
            },
            drafts: Vec::new(),
        }
    }

    fn draw_manager(dashboard: &DashboardState) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        let mut surfaces = FrameSurfaces::new();
        terminal
            .draw(|frame| {
                if let Mode::WorkspaceManager(manager) = &dashboard.mode {
                    render_workspace_manager(frame, frame.area(), manager, &mut surfaces);
                }
            })
            .unwrap();
        buffer_lines(terminal.backend().buffer())
    }

    fn manager_click(dashboard: &mut DashboardState, lines: &[String], label: &str) {
        let (row, line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains(label))
            .unwrap_or_else(|| panic!("missing {label:?}: {lines:#?}"));
        let column = cell_column(line, label) + 1;
        let mouse = |kind| MouseEvent {
            kind,
            column,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left))),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left))),
            DashboardAction::None
        );
    }

    #[test]
    fn manager_ignores_stale_results_and_selects_workspace_on_enter() {
        let mut dashboard = dashboard_with_session(running_session());
        let load = dashboard.begin_workspace_manager();
        let DashboardAction::LoadWorkspaceManagement { generation } = load else {
            panic!("manager did not request a load");
        };
        dashboard.finish_workspace_management(
            generation.wrapping_sub(1),
            Ok(vec![entry("stale", "Stale")]),
        );
        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager) if manager.loading
        ));
        dashboard.finish_workspace_management(generation, Ok(vec![entry("other", "Other")]));
        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager) if !manager.loading
        ));
        let action = dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            DashboardAction::SelectWorkspace {
                workspace_id: "other".into()
            }
        );
    }

    #[test]
    fn tabs_scroll_to_the_selected_workspace_and_mouse_hits_unicode_labels() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_workspace_names(std::collections::BTreeMap::from([
            ("a".into(), "First workspace".into()),
            ("b".into(), "界界".into()),
            ("c".into(), "Last workspace".into()),
        ]));
        dashboard.set_active_workspace(Some("b".into()));
        let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
        terminal
            .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 24, 3), &mut dashboard))
            .unwrap();
        assert_eq!(
            dashboard.workspace_tab_areas[0],
            ("b".into(), Rect::new(1, 1, 6, 1))
        );
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1)].symbol(), " ");
        assert_eq!(buffer[(2, 1)].symbol(), "界");
        assert_eq!(buffer[(4, 1)].symbol(), "界");
        assert_eq!(buffer[(6, 1)].symbol(), " ");
        assert_eq!(buffer[(1, 1)].bg, buffer[(6, 1)].bg);
        assert!(
            matches!(workspace_tab_click(&mut dashboard, 8, 1), Some(DashboardAction::SelectWorkspace { workspace_id }) if workspace_id == "c")
        );
        assert_eq!(dashboard.focus(), crate::Focus::Workspaces);
        dashboard.set_active_workspace(Some("c".into()));
        terminal
            .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 24, 3), &mut dashboard))
            .unwrap();
        assert!(
            dashboard
                .workspace_tab_areas
                .iter()
                .any(|(id, _)| id == "c")
        );
        let text = (0..24)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(text.contains("Last workspace"), "{text}");
    }

    #[test]
    fn manager_initial_selection_follows_the_current_tab() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_active_workspace(Some("b".into()));
        let DashboardAction::LoadWorkspaceManagement { generation } =
            dashboard.begin_workspace_manager()
        else {
            panic!("load");
        };
        dashboard
            .finish_workspace_management(generation, Ok(vec![entry("a", "A"), entry("b", "B")]));
        assert!(
            matches!(dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), DashboardAction::SelectWorkspace { workspace_id } if workspace_id == "b")
        );
    }

    #[test]
    fn hamburger_is_pinned_and_opens_manager_on_mouse_release() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.workspace_names.insert(
            hel::hel_workspace::DEFAULT_WORKSPACE_ID.into(),
            "A very long workspace name".into(),
        );
        let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
        terminal
            .draw(|frame| render_workspace_tabs(frame, frame.area(), &mut dashboard))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(20, 1)].symbol(), " ");
        assert_eq!(buffer[(21, 1)].symbol(), "☰");
        assert_eq!(buffer[(22, 1)].symbol(), " ");
        assert!(dashboard.surface_form.borrow().contains(21, 1));

        let mouse = |kind| MouseEvent {
            kind,
            column: 21,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left))),
            DashboardAction::None
        );
        assert!(matches!(
            dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left))),
            DashboardAction::LoadWorkspaceManagement { generation: 1 }
        ));
        assert!(matches!(dashboard.mode, Mode::WorkspaceManager(_)));
    }

    #[test]
    fn workspace_menu_has_a_keyboard_stop_before_sessions() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = crate::Focus::Workspaces;
        assert_eq!(
            dashboard.workspace_control_focus,
            WorkspaceControlFocus::Tabs
        );
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.workspace_control_focus,
            WorkspaceControlFocus::Menu
        );
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            DashboardAction::None
        );
        assert_eq!(dashboard.focus, crate::Focus::Sessions);

        dashboard.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(dashboard.focus, crate::Focus::Workspaces);
        assert_eq!(
            dashboard.workspace_control_focus,
            WorkspaceControlFocus::Menu
        );
        dashboard.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(dashboard.focus, crate::Focus::Workspaces);
        assert_eq!(
            dashboard.workspace_control_focus,
            WorkspaceControlFocus::Tabs
        );

        dashboard.focus = crate::Focus::Workspaces;
        dashboard.workspace_control_focus = WorkspaceControlFocus::Menu;
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::LoadWorkspaceManagement { generation: 1 }
        );
    }

    #[test]
    fn manager_views_keep_rename_identity_across_reordered_refresh() {
        let mut dashboard = dashboard_with_session(running_session());
        let DashboardAction::LoadWorkspaceManagement { generation } =
            dashboard.begin_workspace_manager()
        else {
            panic!("load");
        };
        dashboard
            .finish_workspace_management(generation, Ok(vec![entry("a", "A"), entry("b", "B")]));
        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.form.get_mut().focus(WorkspaceControl::Rename);
        }
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::None
        );
        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager)
                if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "a")
        ));
        dashboard.finish_workspace_management(
            generation,
            Ok(vec![entry("b", "B"), entry("a", "A renamed")]),
        );
        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager)
                if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "a")
        ));
    }

    #[test]
    fn manager_actions_activate_on_mouse_release() {
        let mut dashboard = dashboard_with_session(running_session());
        let DashboardAction::LoadWorkspaceManagement { generation } =
            dashboard.begin_workspace_manager()
        else {
            panic!("load");
        };
        dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "A")]));
        let lines = draw_manager(&dashboard);

        manager_click(&mut dashboard, &lines, "Rename");

        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager)
                if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "a")
        ));
    }

    #[test]
    fn destructive_delete_requires_exact_name_and_uses_force_action() {
        let mut dashboard = dashboard_with_session(running_session());
        let DashboardAction::LoadWorkspaceManagement { generation } =
            dashboard.begin_workspace_manager()
        else {
            panic!("load");
        };
        let mut busy_entry = entry("a", "Project alpha");
        busy_entry.workspace.session_count = 2;
        busy_entry.drafts.push(WorkspaceDraftEntry {
            id: "draft-a".into(),
            session_id: Some("session-a".into()),
            source: "composer".into(),
            saved_at: "now".into(),
            owner_pid: None,
        });
        dashboard.finish_workspace_management(generation, Ok(vec![busy_entry]));
        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.form.get_mut().focus(WorkspaceControl::Delete);
        }
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager) if matches!(&manager.view, WorkspaceManagerView::Delete { .. })
        ));
        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.form.get_mut().focus(WorkspaceControl::ForceDelete);
        }
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::None
        );
        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.name = TextInput::from_value("Project alpha").with_max_chars(64);
            manager.form.get_mut().focus(WorkspaceControl::ForceDelete);
        }
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::DeleteWorkspace {
                generation,
                workspace_id: "a".into(),
                force: true,
            }
        );
    }

    #[test]
    fn manager_renders_distinct_create_and_drafts_views() {
        let mut dashboard = dashboard_with_session(running_session());
        let DashboardAction::LoadWorkspaceManagement { generation } =
            dashboard.begin_workspace_manager()
        else {
            panic!("load");
        };
        let mut workspace = entry("workspace-a", "Project alpha");
        workspace.drafts.push(WorkspaceDraftEntry {
            id: "draft-a".into(),
            session_id: Some("session-a".into()),
            source: "composer".into(),
            saved_at: "today".into(),
            owner_pid: None,
        });
        dashboard.finish_workspace_management(generation, Ok(vec![workspace]));
        let list = draw_manager(&dashboard).join("\n");
        assert!(list.contains("New workspace"), "{list}");
        assert!(list.contains("Open"), "{list}");
        assert!(list.contains("Drafts"), "{list}");

        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.form.get_mut().focus(WorkspaceControl::New);
        }
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let create = draw_manager(&dashboard).join("\n");
        assert!(create.contains("Workspaces · New"), "{create}");
        assert!(create.contains("Create"), "{create}");
        assert!(create.contains("Cancel"), "{create}");
        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.form.get_mut().focus(WorkspaceControl::Back);
        }
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::None
        );
        let _ = draw_manager(&dashboard);

        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.form.get_mut().focus(WorkspaceControl::Drafts);
        }
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let drafts = draw_manager(&dashboard).join("\n");
        assert!(drafts.contains("Workspaces · Drafts"), "{drafts}");
        assert!(drafts.contains("composer"), "{drafts}");
        assert!(drafts.contains("Recover"), "{drafts}");
        assert!(drafts.contains("Back"), "{drafts}");
    }

    #[test]
    fn manager_load_finishes_behind_help_and_is_restored_when_help_closes() {
        let mut dashboard = dashboard_with_session(running_session());
        let DashboardAction::LoadWorkspaceManagement { generation } =
            dashboard.begin_workspace_manager()
        else {
            panic!("manager did not request a load");
        };
        assert_eq!(
            dashboard.dispatch_command(crate::CommandId::Help),
            DashboardAction::None
        );
        assert!(matches!(&dashboard.mode, Mode::Help(_)));

        assert!(!dashboard.finish_workspace_management(
            generation,
            Ok(vec![entry("workspace-a", "Workspace A")]),
        ));
        assert!(matches!(
            &dashboard.mode,
            Mode::Help(overlay)
                if matches!(overlay.return_to.as_ref(), Mode::WorkspaceManager(manager) if !manager.loading)
        ));

        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            DashboardAction::None
        );
        assert!(matches!(
            &dashboard.mode,
            Mode::WorkspaceManager(manager)
                if !manager.loading && manager.entries[0].workspace.id == "workspace-a"
        ));
    }
}
