//! Workspace tabs and the non-blocking F3 workspace manager.

use std::cell::RefCell;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use hel::hel_workspace::WorkspaceRecord;
use mj_chat::components::{ButtonRow, ChoiceList, ControlKind, Form, Interaction, TextField};
use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::widgets::centered_modal;
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
pub(crate) enum WorkspaceControl {
    List,
    Drafts,
    Name,
    Create,
    Rename,
    Delete,
    ForceDelete,
    Recover,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceMutation {
    Load,
    Create,
    Rename,
    Delete,
    Recover,
}

/// State for the F3 manager. It only stores a snapshot and drafts; all
/// filesystem/database work is requested through [`DashboardAction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceManager {
    pub(crate) generation: u64,
    pub(crate) entries: Vec<WorkspaceManagementEntry>,
    pub(crate) selected: usize,
    pub(crate) selected_draft: usize,
    pub(crate) confirming_delete: bool,
    pub(crate) name: TextInput,
    pub(crate) form: RefCell<Form<WorkspaceControl>>,
    pub(crate) loading: bool,
    pub(crate) busy: Option<WorkspaceMutation>,
    pub(crate) error: Option<String>,
}

fn manager_form() -> RefCell<Form<WorkspaceControl>> {
    let mut form = Form::new();
    form.declare(
        WorkspaceControl::List,
        ControlKind::ChoiceList {
            len: 0,
            selected: 0,
        },
    );
    form.declare(
        WorkspaceControl::Drafts,
        ControlKind::ChoiceList {
            len: 0,
            selected: 0,
        },
    );
    form.declare(WorkspaceControl::Name, ControlKind::TextField);
    form.declare(WorkspaceControl::Create, ControlKind::Button);
    form.declare(WorkspaceControl::Rename, ControlKind::Button);
    form.declare(WorkspaceControl::Delete, ControlKind::Button);
    form.declare(WorkspaceControl::ForceDelete, ControlKind::Button);
    form.declare(WorkspaceControl::Recover, ControlKind::Button);
    form.declare(WorkspaceControl::Close, ControlKind::Button);
    form.end_frame(WorkspaceControl::List);
    RefCell::new(form)
}

impl WorkspaceManager {
    pub(crate) fn loading(generation: u64) -> Self {
        Self {
            generation,
            entries: Vec::new(),
            selected: 0,
            selected_draft: 0,
            confirming_delete: false,
            name: TextInput::default(),
            form: manager_form(),
            loading: true,
            busy: Some(WorkspaceMutation::Load),
            error: None,
        }
    }

    pub(crate) fn selected_entry(&self) -> Option<&WorkspaceManagementEntry> {
        self.entries.get(self.selected)
    }

    pub(crate) fn selected_draft(&self) -> Option<&WorkspaceDraftEntry> {
        self.selected_entry()?.drafts.get(self.selected_draft)
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.entries.len().saturating_sub(1));
        self.selected_draft = 0;
        self.confirming_delete = false;
        if let Some(entry) = self.selected_entry() {
            self.name = TextInput::from_value(entry.workspace.name.clone()).with_max_chars(64);
        }
    }

    fn force_delete_ready(&self) -> bool {
        let Some(entry) = self.selected_entry() else {
            return false;
        };
        let typed = self.name.value().trim();
        self.confirming_delete
            && typed == entry.workspace.name
            && (!entry.drafts.is_empty() || entry.workspace.session_count > 0)
    }

    fn active_sessions_or_drafts(&self) -> bool {
        self.selected_entry()
            .is_some_and(|entry| entry.workspace.session_count > 0 || !entry.drafts.is_empty())
    }

    fn set_error(&mut self, error: String) {
        self.loading = false;
        self.busy = None;
        self.error = Some(error);
    }

    fn can_mutate(&self) -> bool {
        !self.loading && self.busy.is_none()
    }

    fn sync_form(&mut self) {
        let draft_len = self.selected_entry().map_or(0, |entry| entry.drafts.len());
        let form = self.form.get_mut();
        form.begin_update();
        form.declare(
            WorkspaceControl::List,
            ControlKind::ChoiceList {
                len: self.entries.len(),
                selected: self.selected,
            },
        );
        form.declare(
            WorkspaceControl::Drafts,
            ControlKind::ChoiceList {
                len: draft_len,
                selected: self.selected_draft,
            },
        );
        form.declare(WorkspaceControl::Name, ControlKind::TextField);
        form.declare(WorkspaceControl::Create, ControlKind::Button);
        form.declare(WorkspaceControl::Rename, ControlKind::Button);
        form.declare(WorkspaceControl::Delete, ControlKind::Button);
        form.declare(WorkspaceControl::ForceDelete, ControlKind::Button);
        form.declare(WorkspaceControl::Recover, ControlKind::Button);
        form.declare(WorkspaceControl::Close, ControlKind::Button);
        form.end_frame(WorkspaceControl::List);
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
        self.mode = Mode::WorkspaceManager(WorkspaceManager::loading(generation));
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
                let selected_id = manager
                    .selected_entry()
                    .map(|entry| entry.workspace.id.clone())
                    .or(active_workspace_id);
                manager.entries = entries;
                manager.selected = selected_id
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
                manager.selected_draft = 0;
                manager.confirming_delete = false;
                manager.loading = false;
                manager.busy = None;
                manager.error = None;
                if let Some(entry) = manager.selected_entry() {
                    manager.name =
                        TextInput::from_value(entry.workspace.name.clone()).with_max_chars(64);
                } else {
                    manager.name = TextInput::default().with_max_chars(64);
                }
                manager.sync_form();
                return foreground;
            }
            Err(error) => manager.set_error(error),
        }
        true
    }

    pub(crate) fn handle_workspace_manager_event(&mut self, event: Event) -> DashboardAction {
        let Mode::WorkspaceManager(manager) = &mut self.mode else {
            return DashboardAction::None;
        };

        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.modifiers.is_empty()
        {
            let focused = manager.form.borrow().focused();
            if focused == Some(WorkspaceControl::List) {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        manager.select(manager.selected.saturating_add(1));
                        return DashboardAction::None;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        manager.select(manager.selected.saturating_sub(1));
                        return DashboardAction::None;
                    }
                    KeyCode::Char('r') if manager.can_mutate() => {
                        return self.workspace_manager_mutation(WorkspaceMutation::Rename);
                    }
                    KeyCode::Char('d') if manager.can_mutate() => {
                        return self.workspace_manager_mutation(WorkspaceMutation::Delete);
                    }
                    KeyCode::Char('c') if manager.can_mutate() => {
                        manager.form.get_mut().focus(WorkspaceControl::Name);
                        return DashboardAction::None;
                    }
                    _ => {}
                }
            } else if focused == Some(WorkspaceControl::Drafts) {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        let len = manager
                            .selected_entry()
                            .map_or(0, |entry| entry.drafts.len());
                        manager.selected_draft = manager
                            .selected_draft
                            .saturating_add(1)
                            .min(len.saturating_sub(1));
                        return DashboardAction::None;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        manager.selected_draft = manager.selected_draft.saturating_sub(1);
                        return DashboardAction::None;
                    }
                    KeyCode::Char('r') if manager.can_mutate() => {
                        return self.workspace_manager_mutation(WorkspaceMutation::Recover);
                    }
                    _ => {}
                }
            }
        }

        let interaction = manager.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel | Interaction::Activate(WorkspaceControl::Close)) => {
                self.cancel_modal();
            }
            Some(Interaction::Edit(WorkspaceControl::Name, edit)) => {
                TextField::apply(&mut manager.name, edit);
            }
            Some(Interaction::Select(WorkspaceControl::List, index)) => manager.select(index),
            Some(Interaction::Select(WorkspaceControl::Drafts, index)) => {
                manager.selected_draft = index;
            }
            Some(Interaction::Activate(WorkspaceControl::List)) if manager.busy.is_none() => {
                let Some(workspace_id) = manager
                    .selected_entry()
                    .map(|entry| entry.workspace.id.clone())
                else {
                    return DashboardAction::None;
                };
                self.cancel_modal();
                return DashboardAction::SelectWorkspace { workspace_id };
            }
            Some(Interaction::Activate(WorkspaceControl::Drafts)) => {
                return self.workspace_manager_mutation(WorkspaceMutation::Recover);
            }
            Some(Interaction::Activate(WorkspaceControl::Name)) => {
                manager.form.get_mut().focus(WorkspaceControl::Create);
            }
            Some(Interaction::Activate(WorkspaceControl::Create)) => {
                return self.workspace_manager_mutation(WorkspaceMutation::Create);
            }
            Some(Interaction::Activate(WorkspaceControl::Rename)) => {
                return self.workspace_manager_mutation(WorkspaceMutation::Rename);
            }
            Some(Interaction::Activate(WorkspaceControl::Delete)) => {
                return self.workspace_manager_mutation(WorkspaceMutation::Delete);
            }
            Some(Interaction::Activate(WorkspaceControl::ForceDelete)) => {
                return self.workspace_manager_mutation(WorkspaceMutation::Delete);
            }
            Some(Interaction::Activate(WorkspaceControl::Recover)) => {
                return self.workspace_manager_mutation(WorkspaceMutation::Recover);
            }
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
                let Some(entry) = manager.selected_entry() else {
                    return DashboardAction::None;
                };
                let name = manager.name.trim().to_owned();
                if name.is_empty() {
                    manager.error = Some("Workspace name cannot be empty.".into());
                    return DashboardAction::None;
                }
                DashboardAction::RenameWorkspace {
                    generation,
                    workspace_id: entry.workspace.id.clone(),
                    name,
                }
            }
            WorkspaceMutation::Delete => {
                let Some((workspace_id, workspace_name)) = manager
                    .selected_entry()
                    .map(|entry| (entry.workspace.id.clone(), entry.workspace.name.clone()))
                else {
                    return DashboardAction::None;
                };
                let force = manager.active_sessions_or_drafts();
                if force && !manager.force_delete_ready() {
                    manager.confirming_delete = true;
                    manager.name = TextInput::default().with_max_chars(64);
                    manager.error = Some(format!(
                        "Type {:?} exactly to force delete this workspace.",
                        workspace_name
                    ));
                    manager.form.get_mut().focus(WorkspaceControl::Name);
                    return DashboardAction::None;
                }
                DashboardAction::DeleteWorkspace {
                    generation,
                    workspace_id,
                    force,
                }
            }
            WorkspaceMutation::Recover => {
                let Some(draft) = manager.selected_draft() else {
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
        action
    }
}

/// Handles clicks on the tab strip before row and pane hitboxes.
pub(crate) fn workspace_tab_click(
    dashboard: &DashboardState,
    column: u16,
    row: u16,
) -> Option<DashboardAction> {
    let workspace_id = dashboard
        .workspace_tab_areas
        .iter()
        .find(|(_, area)| {
            column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
        })
        .map(|(workspace_id, _)| workspace_id.clone())?;
    (dashboard.active_workspace_id() != Some(workspace_id.as_str()))
        .then_some(DashboardAction::SelectWorkspace { workspace_id })
}

/// Draws the tab strip and records its hitboxes for the next mouse event.
pub(crate) fn render_workspace_tabs(frame: &mut Frame, area: Rect, dashboard: &mut DashboardState) {
    dashboard.clear_workspace_tab_areas();
    if area.height == 0 || area.width == 0 {
        return;
    }
    let ids = dashboard.workspace_ids();
    let labels = ids
        .iter()
        .map(|id| format!(" {} ", dashboard.workspace_display_name(id)))
        .collect::<Vec<_>>();
    let widths = labels
        .iter()
        .map(|label| Line::raw(label.as_str()).width())
        .collect::<Vec<_>>();
    let selected = ids
        .iter()
        .position(|id| Some(id.as_str()) == dashboard.active_workspace_id())
        .unwrap_or(0);
    let mut first = 0;
    let mut selected_width = widths.iter().take(selected + 1).sum::<usize>();
    while first < selected && selected_width > usize::from(area.width) {
        selected_width = selected_width.saturating_sub(widths[first]);
        first += 1;
    }
    let mut x = area.x;
    for (index, id) in ids.iter().enumerate().skip(first) {
        let width = widths[index].min(usize::from(area.right().saturating_sub(x))) as u16;
        if width == 0 {
            break;
        }
        let tab_area = Rect::new(x, area.y, width, 1);
        dashboard.register_workspace_tab_area(id.clone(), tab_area);
        let label = crate::widgets::truncate_text(&labels[index], usize::from(width));
        let style = if index == selected {
            theme::selection(true)
        } else {
            theme::muted()
        };
        frame.render_widget(Paragraph::new(label).style(style), tab_area);
        x = x.saturating_add(width);
    }
}

/// The standard Form modal renderer used by the parent combined renderer.
pub(crate) fn render_workspace_manager(
    frame: &mut Frame,
    area: Rect,
    dialog: &WorkspaceManager,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 86, area.height.min(24), area);
    frame.render_widget(theme::modal().title(" Workspaces · F3 "), popup);
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
            Constraint::Min(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let message = if dialog.loading {
        "Loading workspaces…".to_owned()
    } else if let Some(error) = &dialog.error {
        error.clone()
    } else {
        "Enter selects a workspace · type a name for create, rename, or force delete".to_owned()
    };
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(if dialog.error.is_some() {
            theme::ERROR
        } else {
            theme::MUTED
        })),
        rows[0],
    );
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let list_items = dialog
        .entries
        .iter()
        .map(|entry| {
            let drafts = if entry.drafts.is_empty() {
                String::new()
            } else {
                format!(
                    " · {} draft{}",
                    entry.drafts.len(),
                    if entry.drafts.len() == 1 { "" } else { "s" }
                )
            };
            Line::from(format!(
                "{}  ({} active session{}){}",
                entry.workspace.name,
                entry.workspace.session_count,
                if entry.workspace.session_count == 1 {
                    ""
                } else {
                    "s"
                },
                drafts
            ))
        })
        .collect::<Vec<_>>();
    if list_items.is_empty() {
        frame.render_widget(Paragraph::new("No workspaces."), rows[1]);
        form.register(
            WorkspaceControl::List,
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
            &list_items,
            dialog.selected,
            &mut form,
            WorkspaceControl::List,
        );
    }
    let drafts = dialog.selected_entry().map(|entry| {
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
                        .map_or(String::new(), |session_id| format!("  · {session_id}"))
                ))
            })
            .collect::<Vec<_>>()
    });
    frame.render_widget(Paragraph::new("Detached drafts"), rows[2]);
    if let Some(drafts) = drafts.filter(|drafts| !drafts.is_empty()) {
        ChoiceList::render(
            frame,
            rows[3],
            &drafts,
            dialog.selected_draft,
            &mut form,
            WorkspaceControl::Drafts,
        );
    } else {
        frame.render_widget(Paragraph::new("No detached drafts."), rows[3]);
        form.register(
            WorkspaceControl::Drafts,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            rows[3],
            false,
        );
    }
    let name_label = Line::from(vec![Span::styled(
        if dialog.confirming_delete {
            "Type name: "
        } else {
            "Name: "
        },
        Style::default().add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(name_label, Rect::new(rows[4].x, rows[4].y, 11, 1));
    TextField::render(
        frame,
        Rect::new(
            rows[4].x.saturating_add(11),
            rows[4].y,
            rows[4].width.saturating_sub(11),
            1,
        ),
        &dialog.name,
        &mut form,
        WorkspaceControl::Name,
    );
    let force_enabled = dialog.force_delete_ready();
    ButtonRow::render(
        frame,
        rows[5],
        &[
            (WorkspaceControl::Create, "Create", dialog.can_mutate()),
            (
                WorkspaceControl::Rename,
                "Rename",
                dialog.can_mutate() && dialog.selected_entry().is_some(),
            ),
            (WorkspaceControl::Delete, "Delete", dialog.can_mutate()),
            (
                WorkspaceControl::ForceDelete,
                "Force delete",
                dialog.can_mutate() && force_enabled,
            ),
            (
                WorkspaceControl::Recover,
                "Recover draft",
                dialog.can_mutate() && dialog.selected_draft().is_some(),
            ),
            (WorkspaceControl::Close, "Close", dialog.busy.is_none()),
        ],
        &mut form,
    );
    let busy = dialog.busy.map(|mutation| format!("Working: {mutation:?}"));
    frame.render_widget(
        Paragraph::new(busy.unwrap_or_default()).style(Style::default().fg(theme::MUTED)),
        rows[6],
    );
    form.end_frame(WorkspaceControl::List);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{dashboard_with_session, running_session};
    use crossterm::event::{KeyEvent, KeyModifiers};
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
        let mut terminal = Terminal::new(TestBackend::new(16, 2)).unwrap();
        terminal
            .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 16, 1), &mut dashboard))
            .unwrap();
        assert_eq!(
            dashboard.workspace_tab_areas[0],
            ("b".into(), Rect::new(0, 0, 6, 1))
        );
        assert!(
            matches!(workspace_tab_click(&dashboard, 6, 0), Some(DashboardAction::SelectWorkspace { workspace_id }) if workspace_id == "c")
        );
        dashboard.set_active_workspace(Some("c".into()));
        terminal
            .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 16, 1), &mut dashboard))
            .unwrap();
        assert_eq!(dashboard.workspace_tab_areas[0].0, "c");
        let text = (0..16)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
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
