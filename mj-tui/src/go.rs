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
    /// Record the repository `mj` was started in, so a new local session
    /// starts with it as its project directory.
    pub fn set_launch_project_directory(&mut self, directory: Option<PathBuf>) {
        self.launch_project_directory = directory;
    }

    pub fn register_go_workspaces(&mut self, modes: impl IntoIterator<Item = GoMode>) {
        for mode in modes {
            if let Some(id) = &mode.workspace_id {
                self.go_workspaces.insert(id.clone(), mode);
            }
        }
    }

    pub(crate) fn switch_go_workspace(&mut self, workspace_id: Option<&str>) {
        if let Some(mut mode) = self.go.take()
            && let Some(id) = mode.workspace_id.clone()
        {
            mode.last_session_id = self.selected_session_id.clone();
            self.go_workspaces.insert(id, mode);
        }
        self.go = workspace_id.and_then(|id| self.go_workspaces.get(id).cloned());
    }
    pub fn set_go_context(
        &mut self,
        session_id: String,
        result: Result<(PathBuf, String), String>,
    ) {
        self.go_contexts.insert(session_id, result);
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
                && !self.state.is_subagent_session(&session.id)
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

    pub fn go_launch_action(
        &mut self,
        workspace_id: String,
        config: Config,
        recipe: GoRecipe,
    ) -> DashboardAction {
        self.set_config(config);
        DashboardAction::CreateSession {
            workspace_id,
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
        }
    }

    pub fn remember_go_launch(
        &mut self,
        action: &DashboardAction,
    ) -> Option<(PathBuf, GoRecipe, bool)> {
        let DashboardAction::CreateSession {
            workspace_id,
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
        let go = if self
            .go
            .as_ref()
            .is_some_and(|go| go.workspace_id.as_ref() == Some(workspace_id))
        {
            self.go.as_mut()?
        } else {
            self.go_workspaces.get_mut(workspace_id)?
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{buffer_lines, chord, dashboard_with_session, running_session};
    use crate::{CommandId, Mode};

    fn mode() -> GoMode {
        GoMode {
            workspace_id: Some(mj_core::workspace::DEFAULT_WORKSPACE_ID.into()),
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
        assert_eq!(
            chord(&mut dashboard, CommandId::NewSessionWizard),
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
    fn reopening_skips_a_remembered_harness_owned_sub_agent() {
        let mut dashboard = dashboard_with_session(running_session());
        let agent = mj_core::native_agent::NativeAgent {
            owner_session_id: "session-1".into(),
            session_id: "child".into(),
            parent_session_id: None,
            name: "Explore".into(),
            task: "Map the code".into(),
            capabilities: Default::default(),
            state: mj_core::native_agent::NativeAgentState::Completed,
            availability: Default::default(),
            availability_reason: None,
            stable_id: None,
        };
        let child = agent.view_id();
        dashboard.set_native_agents(vec![mj_core::native_agent::NativeAgentView {
            generation_ordinal: 1,
            projection: mj_core::state::MaterializedSession::empty(child.clone()),
            agent,
        }]);
        let mut go = mode();
        go.last_session_id = Some(child);
        assert_eq!(
            dashboard.begin_go(go, false),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
    }

    #[test]
    fn workspace_management_remains_available_from_go() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_go(mode(), false);
        assert!(dashboard.command_allowed_now(CommandId::Workspaces));
        chord(&mut dashboard, CommandId::Workspaces);
        assert!(matches!(dashboard.mode, Mode::WorkspaceManager(_)));
    }

    #[test]
    fn switching_workspaces_changes_new_and_context_without_rebinding_the_launch_directory() {
        let mut dashboard = dashboard_with_session(running_session());
        let first = mode();
        let first_id = first.workspace_id.clone().unwrap();
        let mut second = mode();
        second.workspace_id = Some("project-b".into());
        second.directory = "/projects/second".into();
        second.recipe.as_mut().unwrap().target_id = "other-runtime".into();
        dashboard.register_go_workspaces([second.clone()]);
        dashboard.begin_go(first.clone(), false);
        dashboard.set_active_workspace(second.workspace_id.clone());
        assert!(
            dashboard
                .go_context()
                .join("\n")
                .contains("/projects/second")
        );
        assert!(
            !dashboard
                .go_context()
                .join("\n")
                .contains("/projects/current")
        );
        assert_eq!(
            dashboard.dispatch_command(CommandId::NewSessionWizard),
            DashboardAction::GoLaunch {
                recipe: second.recipe.clone().unwrap()
            }
        );

        // Preparation from A may finish while B is selected: never overwrite B's recipe.
        let mut prepared = first.recipe.clone().unwrap();
        prepared.bundle_id = Some("prepared-a".into());
        let action = dashboard.go_launch_action(
            first_id.clone(),
            dashboard.config.clone(),
            prepared.clone(),
        );
        let saved = dashboard.remember_go_launch(&action).unwrap();
        assert_eq!(saved.0, first.directory);
        assert_eq!(dashboard.go_mode().unwrap().recipe, second.recipe);

        dashboard.set_active_workspace(Some("unbound-workspace".into()));
        assert!(dashboard.go_mode().is_none());
        assert!(dashboard.go_context().is_empty());
        dashboard.dispatch_command(CommandId::NewSessionWizard);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        dashboard.cancel_modal();
        dashboard.set_active_workspace(Some(first_id));
        assert_eq!(dashboard.go_mode().unwrap().directory, first.directory);
        assert_eq!(
            dashboard.dispatch_command(CommandId::NewSessionWizard),
            DashboardAction::GoLaunch { recipe: prepared }
        );
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
            .draw(|frame| {
                crate::render_combined(
                    frame,
                    &mut dashboard,
                    &mut std::collections::BTreeMap::new(),
                    &std::collections::BTreeMap::new(),
                    false,
                );
            })
            .unwrap();
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Source: /projects/current"));
        assert!(rendered.contains("Working: /actual/checkout"));
        assert!(rendered.contains("branch: feature-x"));
        assert!(rendered.contains(" Menu "));
        for visible in ["Workspaces", "Targets", "Quota", "b panes"] {
            assert!(
                rendered.contains(visible),
                "missing dashboard detail: {visible}"
            );
        }
        assert!(!rendered.contains("session-1"));
        assert!(rendered.contains("Conversation 1"));
        assert!(dashboard.workspace_pane_area.is_some());
        assert!(!dashboard.pane_size_control_areas.is_empty());
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
