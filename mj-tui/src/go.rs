//! The optional fast-start workflow. All IO remains in the CLI supervisor.

use std::path::PathBuf;

use mj_core::config::{Config, TargetTemplate, raw_project_context_id};
use mj_core::go::GoRecipe;

use crate::{DashboardAction, DashboardState, PaneSize, SupportPane};

#[derive(Debug, Clone)]
pub struct GoMode {
    pub directory: PathBuf,
    pub recipe: Option<GoRecipe>,
    pub save_as_default: bool,
}

impl DashboardState {
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
        self.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        self.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
        self.focus_prompt();
        if setup || needs_remote_path {
            self.change_go_setup()
        } else {
            self.begin_new()
        }
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
        let mut lines = vec![format!("New sessions from: {}", go.directory.display())];
        if let Some(session) = self.selected_session() {
            let context = self.go_contexts.get(&session.id);
            let location = match context {
                Some(Ok((directory, _))) => directory.display().to_string(),
                Some(Err(error)) => format!("unavailable: {error}"),
                None => "checking actual working location…".into(),
            };
            let sharing = if session.managed_worktree.is_some() {
                "Separate checkout"
            } else if session.project_directory.is_some() {
                "Shared folder"
            } else {
                "Isolated checkout"
            };
            lines.push(format!("{sharing}: {location}"));
            if let Some(target) = &session.target {
                use mj_core::state::TargetLocator;
                let environment = match target {
                    TargetLocator::LocalBare { .. } => "this machine".into(),
                    TargetLocator::LocalPodman { container_id, .. } => {
                        format!("local Podman · {container_id}")
                    }
                    TargetLocator::LocalDocker { container_id } => {
                        format!("local Docker · {container_id}")
                    }
                    TargetLocator::AppleContainer { container_id } => {
                        format!("Apple container · {container_id}")
                    }
                    TargetLocator::SshBare { host, .. } => format!("SSH · {host}"),
                    TargetLocator::SshPodman {
                        host, container_id, ..
                    } => format!("Podman · {host} · {container_id}"),
                    TargetLocator::SshDocker { host, container_id } => {
                        format!("Docker · {host} · {container_id}")
                    }
                    TargetLocator::AwsEc2 {
                        instance_id,
                        address,
                    } => format!(
                        "EC2 · {instance_id} · {}",
                        address.as_deref().unwrap_or("address pending")
                    ),
                };
                lines.push(format!("Running on: {environment}"));
            }
            lines.push(format!(
                "{} · {} · {} · {}",
                session.display_title(),
                session.last_profile,
                session.target_template_id,
                context
                    .and_then(|result| result.as_ref().ok())
                    .map(|(_, branch)| format!("branch: {branch}"))
                    .unwrap_or_else(|| "branch: checking…".into())
            ));
        } else if let Some(recipe) = &go.recipe {
            let target = self.config.targets.get(&recipe.target_id);
            let location = if matches!(target, Some(TargetTemplate::LocalBare)) {
                "shared local folder"
            } else {
                "preparing target workspace"
            };
            lines.push(format!(
                "{} · {} · {location}",
                recipe.profile_id, recipe.target_id
            ));
        } else {
            lines.push("Choose your account and target once; New will reuse them.".into());
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{alt_key, buffer_lines, dashboard_with_session, running_session};
    use crate::{CommandId, Mode};

    fn mode() -> GoMode {
        GoMode {
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
            DashboardAction::GoLaunch {
                recipe: expected.clone()
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
        assert!(rendered.contains("New sessions from: /projects/current"));
        assert!(rendered.contains("Shared folder: /actual/checkout"));
        assert!(rendered.contains("branch: feature-x"));
        assert!(rendered.contains("Change setup"));
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
