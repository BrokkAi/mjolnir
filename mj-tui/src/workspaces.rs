//! Workspace tabs and the non-blocking workspace manager.

use std::cell::RefCell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use mj_chat::components::{
    ChoiceList, ColumnAlign, ColumnSplit, ControlKind, Dialog, Interaction, TextField,
};
use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;
use mj_chat::theme;
use mj_core::workspace::WorkspaceRecord;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::widgets::{Truncate, centered_modal, dismissible_modal_title, truncate_to_cells};
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
                    (New, "New workspace", ready),
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
        let destructive = self.delete_is_destructive();
        let form = self.form.get_mut();
        form.begin_update();
        let initial = match self.view {
            WorkspaceManagerView::List => {
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
    /// left/right selection (with up/down as vertical mirrors of the same
    /// moves); the pinned menu is a second local stop before the ordinary
    /// dashboard Tab ring continues to Sessions.
    pub(crate) fn handle_workspace_pane_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if self.focus != crate::Focus::Workspaces
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        if self.subagent_parent_id().is_some()
            && matches!(key.code, KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Esc)
        {
            self.record_event_handled();
            return Some(DashboardAction::ExitSubagentWorkspace);
        }
        // The tab row is horizontal, so Up/Down carry the same meaning as
        // Left/Right everywhere in this pane.
        let key = match key.code {
            KeyCode::Up => KeyEvent {
                code: KeyCode::Left,
                ..key
            },
            KeyCode::Down => KeyEvent {
                code: KeyCode::Right,
                ..key
            },
            _ => key,
        };
        let back_tab = key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        match (self.workspace_control_focus, key.code, back_tab) {
            (WorkspaceControlFocus::Tabs, KeyCode::Tab, false) => {
                self.workspace_control_focus = WorkspaceControlFocus::Menu;
                self.surface_form
                    .borrow_mut()
                    .focus(crate::surface_controls::SurfaceControl::WorkspaceMenu);
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

    /// Selects the workspace in slot `index`, counting from zero, or does
    /// nothing when there is no tab there. This is the numbered-key sibling of
    /// [`DashboardState::select_adjacent_workspace`].
    pub fn select_workspace_index(&self, index: usize) -> DashboardAction {
        let ids = self.workspace_ids();
        match ids.get(index) {
            Some(workspace_id) => DashboardAction::SelectWorkspace {
                workspace_id: workspace_id.clone(),
            },
            None => DashboardAction::None,
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
                return foreground;
            }
            Err(error) => {
                manager.set_error(error);
            }
        }
        true
    }

    pub(crate) fn handle_workspace_manager_event(&mut self, event: Event) -> DashboardAction {
        let Mode::WorkspaceManager(manager) = &mut self.mode else {
            return DashboardAction::None;
        };
        let result = manager.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
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
                    }
                }
            }
            Some(Interaction::Edit(WorkspaceControl::Name, edit)) => {
                if TextField::apply(&mut manager.name, edit)
                    == mj_chat::components::EditOutcome::Changed
                {
                    manager.error = None;
                    manager.success = None;
                }
            }
            Some(Interaction::Select(WorkspaceControl::List, index)) => {
                let previous = manager.selected;
                manager.select(index);
                if manager.selected != previous {
                    manager.error = None;
                    manager.success = None;
                }
            }
            Some(Interaction::Select(WorkspaceControl::DraftList, index)) => {
                if manager.selected_draft != index {
                    manager.selected_draft = index;
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
                    }
                }
                WorkspaceControl::Back => {
                    if matches!(manager.view, WorkspaceManagerView::List) {
                        self.cancel_modal();
                    } else {
                        manager.reset_to_list();
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
        action
    }
}

/// Handles clicks on the workspace pane before row and pane hitboxes.
pub(crate) fn workspace_tab_click(
    dashboard: &mut DashboardState,
    column: u16,
    row: u16,
) -> Option<DashboardAction> {
    if dashboard
        .subagent_workspace_close_area
        .is_some_and(|area| area.contains(ratatui::layout::Position::new(column, row)))
    {
        return Some(DashboardAction::ExitSubagentWorkspace);
    }
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
        dashboard.set_session_action_focus(None);
    }
    let workspace_id = workspace_id?;
    if dashboard.focus != crate::Focus::Workspaces {
        dashboard.focus = crate::Focus::Workspaces;
    }
    if dashboard.workspace_control_focus != WorkspaceControlFocus::Tabs {
        dashboard.workspace_control_focus = WorkspaceControlFocus::Tabs;
    }
    (dashboard.active_workspace_id() != Some(workspace_id.as_str()))
        .then_some(DashboardAction::SelectWorkspace { workspace_id })
}

const WORKSPACES_TITLE: &str = " Workspaces ";
/// The running Mjolnir build, drawn on the workspace pane's border.
const VERSION_TITLE: &str = concat!(" v", env!("CARGO_PKG_VERSION"), " ");
/// The two rounded corners a bordered title row cannot draw into.
const BORDER_CORNER_CELLS: usize = 2;

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
    let mut block = theme::panel(focused).title(WORKSPACES_TITLE);
    // The pane sits at the top of every dashboard, so its border is where the
    // build number costs nothing and is always in view. A sidebar too narrow
    // to hold both drops it rather than overlap the pane's own title.
    if usize::from(area.width) >= WORKSPACES_TITLE.len() + VERSION_TITLE.len() + BORDER_CORNER_CELLS
    {
        // The pane's own title style is bold; a build number is a stamp, not a
        // heading, so it drops back out of bold here.
        let style = theme::muted().remove_modifier(Modifier::BOLD);
        block = block.title(Line::styled(VERSION_TITLE, style).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if let Some(parent_id) = dashboard.subagent_parent_id().map(str::to_owned) {
        let title = dashboard
            .state
            .sessions
            .get(&parent_id)
            .map(|session| session.display_title())
            .unwrap_or(&parent_id);
        let close_width = 3u16.min(inner.width);
        let close_area = Rect::new(
            inner.right().saturating_sub(close_width),
            inner.y,
            close_width,
            1,
        );
        dashboard.subagent_workspace_close_area = Some(close_area);
        let label_area = Rect::new(inner.x, inner.y, inner.width.saturating_sub(close_width), 1);
        frame.render_widget(
            Paragraph::new(workspace_label(title, label_area.width)).style(theme::selection(true)),
            label_area,
        );
        frame.render_widget(
            Paragraph::new(" X ").style(theme::selection(false)),
            close_area,
        );
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
    let content = truncate_to_cells(name, usize::from(width - 2), Truncate::PLAIN);
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
    // The actions form a column beside the page body rather than a footer row,
    // so the body gives up exactly the width that column needs.
    let columns = form.split_actions(rows[1], &dialog.actions());
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
        WorkspaceManagerView::List => render_manager_list(frame, columns, dialog, &mut form),
        WorkspaceManagerView::Create => render_manager_name_view(
            frame,
            columns,
            dialog,
            &mut form,
            "New workspace name:",
            WorkspaceControl::Create,
            "Create",
        ),
        WorkspaceManagerView::Rename { .. } => render_manager_name_view(
            frame,
            columns,
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
            columns,
            dialog,
            &mut form,
            workspace_name,
            *session_count,
            *draft_count,
        ),
        WorkspaceManagerView::Drafts { .. } => {
            render_manager_drafts(frame, columns, dialog, &mut form)
        }
    }
    let busy = dialog.busy.map(|mutation| format!("Working: {mutation:?}"));
    frame.render_widget(
        Paragraph::new(busy.unwrap_or_default()).style(Style::default().fg(theme::palette().muted)),
        rows[2],
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
    columns: ColumnSplit,
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
) {
    let ColumnSplit { body, actions } = columns;
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
        frame.render_widget(Paragraph::new("No workspaces."), body);
        form.register(
            WorkspaceControl::List,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            body,
            false,
        );
    } else {
        ChoiceList::render(
            frame,
            body,
            &list_items,
            dialog.selected,
            form,
            WorkspaceControl::List,
        );
    }
    Dialog::render_actions_stacked(frame, actions, &dialog.actions(), form, ColumnAlign::Right);
}

fn render_manager_name_view(
    frame: &mut Frame,
    columns: ColumnSplit,
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
    label: &str,
    _submit: WorkspaceControl,
    _submit_label: &str,
) {
    let ColumnSplit { body, actions } = columns;
    let field = Rect::new(body.x, body.y, body.width, 1);
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
    Dialog::render_actions_stacked(frame, actions, &dialog.actions(), form, ColumnAlign::Right);
}

fn render_manager_delete(
    frame: &mut Frame,
    columns: ColumnSplit,
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
    workspace_name: &str,
    session_count: u64,
    draft_count: usize,
) {
    let ColumnSplit {
        body: body_area,
        actions,
    } = columns;
    let destructive = session_count > 0 || draft_count > 0;
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if destructive { 2 } else { 1 }),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(body_area);
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
    Dialog::render_actions_stacked(frame, actions, &dialog.actions(), form, ColumnAlign::Right);
}

fn render_manager_drafts(
    frame: &mut Frame,
    columns: ColumnSplit,
    dialog: &WorkspaceManager,
    form: &mut Dialog<WorkspaceControl>,
) {
    let ColumnSplit { body, actions } = columns;
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
        frame.render_widget(Paragraph::new("No detached drafts."), body);
        form.register(
            WorkspaceControl::DraftList,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            body,
            false,
        );
    } else {
        ChoiceList::render(
            frame,
            body,
            &drafts,
            dialog.selected_draft,
            form,
            WorkspaceControl::DraftList,
        );
    }
    Dialog::render_actions_stacked(frame, actions, &dialog.actions(), form, ColumnAlign::Right);
}

#[cfg(test)]
mod tests;
