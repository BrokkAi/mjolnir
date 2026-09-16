//! The optional fast-start workflow. All IO remains in the CLI supervisor.

use std::path::PathBuf;

use mj_core::config::{Config, TargetTemplate, raw_project_context_id};
use mj_core::go::GoRecipe;

use crate::{DashboardAction, DashboardState};

#[derive(Debug, Clone)]
pub struct GoMode {
    pub workspace_id: Option<String>,
    pub last_session_id: Option<String>,
    pub directory: PathBuf,
    pub recipe: Option<GoRecipe>,
    pub save_as_default: bool,
}

impl DashboardState {
    pub(crate) fn go_command_allowed(&self, id: crate::CommandId) -> bool {
        use crate::CommandId;
        self.go.is_none()
            || !matches!(
                id,
                CommandId::Workspaces
                    | CommandId::SelectWorkspacePrevious
                    | CommandId::SelectWorkspaceNext
                    | CommandId::TogglePanePreset
                    | CommandId::CycleFocusedPaneSize
                    | CommandId::TargetActions
                    | CommandId::EditProfile
            )
    }
    pub fn set_go_context(
        &mut self,
        session_id: String,
        result: Result<(PathBuf, String), String>,
    ) {
        self.go_contexts.insert(session_id, result);
        self.mark_render_changed();
    }
    pub fn begin_go(&mut self, mode: GoMode, setup: bool) -> DashboardAction {
        let needs_remote_path = mode.recipe.as_ref().is_some_and(|recipe| {
            matches!(
                self.config.targets.get(&recipe.target_id),
                Some(TargetTemplate::SshBare { .. })
            ) && recipe.project_directory.is_none()
        });
        self.go = Some(mode);
        self.focus_prompt();
        if setup || needs_remote_path {
            self.change_go_setup()
        } else if let Some(session_id) = self.go_startup_session() {
            self.select_active_session(&session_id);
            if self.state.sessions[&session_id].state == mj_core::state::SessionState::Stopped {
                self.begin_resume_for(&session_id)
            } else {
                self.open_selected_session()
            }
        } else {
            self.begin_new()
        }
    }

    fn go_startup_session(&self) -> Option<String> {
        let eligible = |session: &&mj_core::state::SessionRecord| {
            self.active_workspace_id.as_deref() == Some(session.workspace_id.as_str())
                && !session.archived
                && !self.state.subagents.contains_key(&session.id)
                && session.state != mj_core::state::SessionState::DestroyedWithDataLoss
        };
        self.go
            .as_ref()
            .and_then(|go| go.last_session_id.as_ref())
            .and_then(|id| self.state.sessions.get(id))
            .filter(eligible)
            .or_else(|| {
                self.state
                    .sessions
                    .values()
                    .filter(eligible)
                    .max_by_key(|session| &session.updated_at)
            })
            .map(|session| session.id.clone())
    }

    pub fn go_conversation_title(&self, session_id: &str) -> String {
        let Some(session) = self.state.sessions.get(session_id) else {
            return "Conversation".into();
        };
        if session.display_title() != session.id {
            return session.display_title().to_owned();
        }
        let mut sessions = self
            .state
            .sessions
            .values()
            .filter(|other| other.workspace_id == session.workspace_id)
            .collect::<Vec<_>>();
        sessions.sort_by_cached_key(|session| session.creation_order_key());
        let number = sessions
            .iter()
            .position(|other| other.id == session_id)
            .unwrap_or(0)
            + 1;
        format!("Conversation {number}")
    }

    pub fn go_mode(&self) -> Option<&GoMode> {
        self.go.as_ref()
    }

    pub(crate) fn change_go_setup(&mut self) -> DashboardAction {
        let action = self.begin_new_wizard();
        let Some(go) = &self.go else {
            return action;
        };
        if let crate::Mode::New(wizard) = &mut self.mode {
            // The local source is explicit. A remote bare path is requested
            // separately after target selection, never guessed from this path.
            wizard.project_directory = go.directory.to_string_lossy().into_owned().into();
            if let Some(recipe) = &go.recipe {
                wizard.profile = self
                    .config
                    .enabled_profiles()
                    .position(|(id, _)| id == recipe.profile_id)
                    .unwrap_or(0);
                wizard.target = self
                    .config
                    .targets
                    .keys()
                    .position(|id| id == &recipe.target_id)
                    .unwrap_or(0);
                wizard.create_managed_worktree = recipe.create_managed_worktree.unwrap_or(false);
                wizard.mjolnir_subagents = recipe
                    .mjolnir_subagents
                    .unwrap_or(self.config.subagents.enabled);
                wizard.resource_allocation = recipe.resource_allocation.clone();
                wizard.mounts.mounts = recipe.additional_mounts.clone();
            }
        }
        action
    }

    pub fn go_launch_action(&mut self, config: Config, recipe: GoRecipe) -> DashboardAction {
        self.set_config(config);
        if let Some(go) = &mut self.go {
            go.recipe = Some(recipe.clone());
        }
        DashboardAction::CreateSession {
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            profile_id: recipe.profile_id,
            target_template_id: recipe.target_id,
            bundle_id: recipe.bundle_id.unwrap_or_else(|| {
                raw_project_context_id(
                    &recipe
                        .project_directory
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            }),
            project_directory: recipe.project_directory,
            create_managed_worktree: recipe.create_managed_worktree,
            mjolnir_subagents: recipe.mjolnir_subagents,
            additional_mounts: recipe.additional_mounts,
            resource_allocation: recipe.resource_allocation,
            allow_dirty_local: false,
        }
    }

    pub fn remember_go_launch(
        &mut self,
        action: &DashboardAction,
    ) -> Option<(PathBuf, GoRecipe, bool)> {
        let DashboardAction::CreateSession {
            profile_id,
            target_template_id,
            bundle_id,
            project_directory,
            create_managed_worktree,
            mjolnir_subagents,
            additional_mounts,
            resource_allocation,
            ..
        } = action
        else {
            return None;
        };
        let go = self.go.as_mut()?;
        let recipe = GoRecipe {
            profile_id: profile_id.clone(),
            target_id: target_template_id.clone(),
            bundle_id: project_directory.is_none().then(|| bundle_id.clone()),
            project_directory: project_directory.clone(),
            create_managed_worktree: *create_managed_worktree,
            mjolnir_subagents: *mjolnir_subagents,
            additional_mounts: additional_mounts.clone(),
            resource_allocation: resource_allocation.clone(),
        };
        go.recipe = Some(recipe.clone());
        Some((go.directory.clone(), recipe, go.save_as_default))
    }

    pub(crate) fn go_context(&self) -> Vec<String> {
        let Some(go) = &self.go else {
            return Vec::new();
        };
        let name = mj_core::go::GoPreferences::directory_label(&go.directory);
        let mut lines = vec![name, format!("Source: {}", go.directory.display())];
        if let Some(session) = self.selected_session() {
            lines[0] = format!(
                "{} · {} · {}",
                lines[0], session.last_profile, session.target_template_id
            );
            let sharing = if session.managed_worktree.is_some() {
                "separate checkout"
            } else if session.project_directory.is_some() {
                "shared folder"
            } else {
                "isolated checkout"
            };
            match self.go_contexts.get(&session.id) {
                Some(Ok((directory, branch))) => lines.push(format!(
                    "Working: {} · branch: {branch} · {sharing}",
                    directory.display()
                )),
                Some(Err(error)) => lines.push(format!("Working location unavailable: {error}")),
                None => lines.push(format!("Checking working location… · {sharing}")),
            }
        } else if let Some(recipe) = &go.recipe {
            lines[0] = format!(
                "{} · {} · {}",
                lines[0], recipe.profile_id, recipe.target_id
            );
        }
        lines
    }
}

pub(crate) fn render_conversations(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    dashboard: &DashboardState,
) -> crate::render::SessionRowsRendered {
    use mj_chat::theme;
    use ratatui::layout::Rect;
    use ratatui::widgets::Paragraph;
    frame.render_widget(
        theme::panel(dashboard.focus() == crate::Focus::Sessions).title(" Conversations "),
        area,
    );
    let inner = crate::widgets::bordered_content(area);
    crate::surface_controls::render_session_buttons(
        frame,
        Rect::new(inner.x, inner.y, inner.width, inner.height.min(1)),
        dashboard,
    );
    let rows = inner.height.saturating_sub(2) as usize;
    let sessions = dashboard.ordered_sessions();
    let selected = sessions
        .iter()
        .position(|session| Some(session.id.as_str()) == dashboard.selected_session_id())
        .unwrap_or(0);
    let start = dashboard
        .sessions_scroll
        .get()
        .min(selected)
        .max(selected.saturating_add(1).saturating_sub(rows));
    dashboard.sessions_scroll.set(start);
    let mut session_row_areas = Vec::new();
    for (index, session) in sessions.iter().enumerate().skip(start).take(rows) {
        let rect = Rect::new(
            inner.x,
            inner.y + 2 + (index - start) as u16,
            inner.width,
            1,
        );
        let selected = Some(session.id.as_str()) == dashboard.selected_session_id();
        let text = format!(
            "{} {}",
            if selected { "›" } else { " " },
            dashboard.go_conversation_title(&session.id)
        );
        let style = if selected {
            theme::selection(true)
        } else {
            theme::base()
        };
        frame.render_widget(Paragraph::new(text).style(style), rect);
        session_row_areas.push((index, rect));
    }
    crate::render::SessionRowsRendered {
        session_row_areas,
        project_heading_areas: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{alt_key, buffer_lines, dashboard_with_session, running_session};
    use crate::{CommandId, Mode};

    fn mode() -> GoMode {
        GoMode {
            workspace_id: None,
            last_session_id: None,
            directory: "/projects/current".into(),
            save_as_default: false,
            recipe: Some(GoRecipe {
                profile_id: "codex-1".into(),
                target_id: "podman".into(),
                bundle_id: Some("hel".into()),
                project_directory: None,
                create_managed_worktree: Some(false),
                mjolnir_subagents: Some(true),
                additional_mounts: Vec::new(),
                resource_allocation: None,
            }),
        }
    }

    #[test]
    fn new_reuses_the_recipe_without_stopping_the_existing_session() {
        let mut dashboard = dashboard_with_session(running_session());
        let expected = mode().recipe.unwrap();
        assert_eq!(
            dashboard.begin_go(mode(), false),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        let before = dashboard.state.clone();
        let command = crate::global_chord(&alt_key('n')).unwrap();
        assert!(dashboard.global_chord_allowed(command));
        assert_eq!(
            dashboard.dispatch_command(command),
            DashboardAction::GoLaunch { recipe: expected }
        );
        assert_eq!(dashboard.state, before);
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn reopening_prefers_the_remembered_conversation_and_ignores_other_workspaces() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut newer = running_session();
        newer.id = "newer".into();
        newer.updated_at = "2026-09-01T00:00:00Z".into();
        dashboard.state.sessions.insert(newer.id.clone(), newer);
        let mut other = running_session();
        other.id = "other-project".into();
        other.workspace_id = "other-workspace".into();
        other.updated_at = "2026-09-02T00:00:00Z".into();
        dashboard.state.sessions.insert(other.id.clone(), other);
        let mut go = mode();
        go.last_session_id = Some("session-1".into());
        assert_eq!(
            dashboard.begin_go(go, false),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        let mut go = mode();
        go.last_session_id = Some("other-project".into());
        assert_eq!(
            dashboard.begin_go(go, false),
            DashboardAction::Open {
                session_id: "newer".into()
            }
        );
    }

    #[test]
    fn hidden_dashboard_commands_cannot_escape_the_focused_screen() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_go(mode(), false);
        for command in [
            CommandId::Workspaces,
            CommandId::SelectWorkspaceNext,
            CommandId::TogglePanePreset,
            CommandId::TargetActions,
        ] {
            assert!(!dashboard.global_chord_allowed(command));
            assert_eq!(dashboard.dispatch_command(command), DashboardAction::None);
            assert!(matches!(dashboard.mode, Mode::Dashboard));
        }
        dashboard.dispatch_command(CommandId::Palette);
        assert!(!matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn change_setup_is_explicit_and_cancelling_keeps_the_previous_recipe() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_go(mode(), false);
        dashboard.dispatch_command(CommandId::ChangeGoSetup);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        dashboard.cancel_modal();
        assert_eq!(
            dashboard.dispatch_command(CommandId::NewSessionWizard),
            DashboardAction::GoLaunch {
                recipe: mode().recipe.unwrap()
            }
        );
    }

    #[test]
    fn ordinary_new_still_opens_the_existing_wizard() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.dispatch_command(CommandId::NewSessionWizard);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        assert!(dashboard.go_mode().is_none());
    }

    #[test]
    fn banner_displays_the_selected_sessions_actual_checkout_and_branch() {
        let mut session = running_session();
        session.acp_session_title = None;
        session.project_directory = Some("/actual/checkout".into());
        let id = session.id.clone();
        let mut dashboard = dashboard_with_session(session);
        dashboard.begin_go(mode(), false);
        dashboard.set_go_context(id, Ok(("/actual/checkout".into(), "feature-x".into())));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 45)).unwrap();
        terminal
            .draw(|frame| crate::render_combined(frame, &mut dashboard, None, false))
            .unwrap();
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Source: /projects/current"));
        assert!(rendered.contains("Working: /actual/checkout"));
        assert!(rendered.contains("branch: feature-x"));
        assert!(rendered.contains(" Menu "));
        for hidden in [
            "Workspaces",
            "Targets",
            "Quota",
            "Change setup",
            "session-1",
            "Alt-G",
        ] {
            assert!(
                !rendered.contains(hidden),
                "unexpected dashboard detail: {hidden}"
            );
        }
        assert!(rendered.contains("Conversation 1"));
        assert!(dashboard.workspace_pane_area.is_none());
        assert!(dashboard.pane_size_control_areas.is_empty());
        if let Some(path) = std::env::var_os("MJ_GO_CAPTURE_PATH") {
            std::fs::write(
                path,
                crate::docs_screenshots::buffer_svg(
                    terminal.backend().buffer(),
                    "Focused project workspace",
                    "Fast mode with conversations, New, Menu and the selected working context",
                ),
            )
            .unwrap();
        }
        assert!(rendered.contains(" New "));
        let rows = buffer_lines(terminal.backend().buffer());
        let (row, line) = rows
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains(" New "))
            .unwrap();
        let column = crate::test_support::cell_column(line, " New ") + 1;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mouse = |kind| MouseEvent {
            kind,
            column,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        };
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left))),
            DashboardAction::GoLaunch {
                recipe: mode().recipe.unwrap()
            }
        );
    }

    #[test]
    fn failed_context_probe_replaces_old_location_instead_of_claiming_it_is_current() {
        let session = running_session();
        let id = session.id.clone();
        let mut dashboard = dashboard_with_session(session);
        dashboard.begin_go(mode(), false);
        dashboard.set_go_context(id.clone(), Ok(("/old".into(), "main".into())));
        dashboard.set_go_context(id, Err("target disconnected".into()));
        let text = dashboard.go_context().join("\n");
        assert!(text.contains("target disconnected"));
        assert!(!text.contains("/old"));
    }
}
