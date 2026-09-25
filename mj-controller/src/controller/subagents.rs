//! Registration and ownership rules for child sessions on a parent's target.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::{Controller, now};
use mj_core::config::HarnessKind;
use mj_core::state::{SessionRecord, SessionState, new_session_id};
use mj_core::subagent::SubagentRecord;

#[derive(Debug, Clone)]
pub struct RegisterSubagentRequest {
    pub parent_session_id: String,
    pub task_name: String,
    pub profile_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Empty means the parent's working directory. An absolute path is used
    /// as-is; a relative path is resolved against the parent's working
    /// directory. The path is interpreted on the parent's target and must
    /// exist there; no other restriction applies.
    pub working_directory: PathBuf,
    pub initial_prompt: String,
    pub request_key: String,
}

impl Controller {
    /// Register a child without provisioning another target or checkout.
    pub fn register_subagent(
        &mut self,
        request: RegisterSubagentRequest,
    ) -> Result<SubagentRecord> {
        if let Some(existing) = crate::database::lookup_subagent_request(
            &request.parent_session_id,
            &request.request_key,
        )? {
            return Ok(existing);
        }
        ensure!(
            !request.request_key.trim().is_empty(),
            "sub-agent request key cannot be empty"
        );
        ensure!(
            !request.task_name.trim().is_empty(),
            "sub-agent task name cannot be empty"
        );
        ensure!(
            !request.initial_prompt.trim().is_empty(),
            "sub-agent instructions cannot be empty"
        );
        let parent = self
            .state
            .sessions
            .get(&request.parent_session_id)
            .with_context(|| format!("unknown parent session {}", request.parent_session_id))?
            .clone();
        ensure!(
            matches!(
                parent.harness_kind,
                HarnessKind::Claude | HarnessKind::Codex
            ),
            "only Claude and Codex sessions can spawn sub-agents"
        );
        ensure_parent_may_delegate(&parent)?;
        ensure!(parent.state.is_active(), "parent session is not active");
        ensure!(parent.target.is_some(), "parent session has no live target");
        ensure!(
            crate::database::load_subagent(&parent.id)?.is_none(),
            "sub-agents cannot spawn other sub-agents"
        );
        ensure!(
            self.config
                .subagents
                .profile_is_eligible(&parent.last_profile, &request.profile_id),
            "profile {:?} is not eligible for sub-agent use",
            request.profile_id
        );
        let profile = self
            .config
            .enabled_profile(&request.profile_id)
            .with_context(|| {
                format!("sub-agent profile {:?} is unavailable", request.profile_id)
            })?;
        if profile.kind == HarnessKind::Muse {
            let multiple_roots = !parent.additional_mounts.is_empty()
                || (parent.project_directory.is_none()
                    && self
                        .config
                        .bundles
                        .get(&parent.bundle_id)
                        .is_some_and(|bundle| bundle.repositories.len() > 1));
            ensure!(
                !multiple_roots,
                "{} ACP supports one workspace root; this parent exposes multiple roots",
                profile.kind.display_name()
            );
        }
        let occupied = crate::database::list_subagents(&parent.id)?
            .into_iter()
            .filter(|child| {
                self.subagent_occupies_slot(&child.child_session_id)
                    .unwrap_or(true)
            })
            .count();
        ensure!(
            occupied < self.config.subagents.max_concurrent,
            "parent session already has the maximum {} active sub-agents",
            self.config.subagents.max_concurrent
        );

        let child_id = new_session_id()?;
        let target = borrowed_locator(
            parent.target.as_ref().expect("live target checked above"),
            &parent.id,
            &child_id,
        )?;
        let created_at = now();
        let session = SessionRecord {
            target_runtime: Some(parent.target_runtime_settings(&self.config)?.into_owned()),
            launch_base: None,
            launch_branch: None,
            publication: None,
            // A child shares its parent's container, so it shares the build
            // cache that container was created with.
            build_cache: parent.build_cache.clone(),
            // A child never receives the Mjolnir sub-agent tools, so it can
            // never spawn a grandchild.
            mjolnir_subagents: Some(false),
            create_managed_worktree: Some(false),
            archived: false,
            container_cpus: None,
            container_memory: None,
            // A child runs inside its parent's container, so it works in the
            // parent's workspace, including the legacy shared one.
            container_workspace: parent.container_workspace.clone(),
            id: child_id.clone(),
            workspace_id: parent.workspace_id.clone(),
            title: request.task_name.clone(),
            harness_kind: profile.kind,
            last_profile: request.profile_id.clone(),
            bundle_id: parent.bundle_id.clone(),
            project_directory: parent.project_directory.clone(),
            managed_worktree: None,
            target_template_id: parent.target_template_id.clone(),
            resource_allocation: parent.resource_allocation.clone(),
            additional_mounts: parent.additional_mounts.clone(),
            state: SessionState::Provisioning,
            target: Some(target),
            native_session_id: None,
            acp_session_title: None,
            session_title_override: Some(request.task_name.clone()),
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        let handback_tool = child_gets_handback_tool(profile.kind);
        // The first prompt names the tool only when the child will have it.
        let initial_prompt = if handback_tool {
            format!(
                "{}\n\n{}",
                mj_core::subagent::HANDBACK_PROMPT_NOTE,
                request.initial_prompt
            )
        } else {
            request.initial_prompt
        };
        let relation = SubagentRecord {
            child_session_id: child_id.clone(),
            parent_session_id: parent.id,
            task_name: request.task_name,
            profile_id: request.profile_id,
            model: request.model,
            effort: request.effort,
            working_directory: request.working_directory,
            initial_prompt,
            request_key: request.request_key,
            created_at,
            noticed_turn: None,
            handback_tool,
        };
        crate::database::save_subagent_session(&session, &relation)?;
        self.state.sessions.insert(child_id, session);
        self.state
            .subagents
            .insert(relation.child_session_id.clone(), relation.clone());
        Ok(relation)
    }

    pub fn ensure_subagent_slot_available(
        &self,
        parent_session_id: &str,
        child_id: &str,
    ) -> Result<()> {
        let occupied = crate::database::list_subagents(parent_session_id)?
            .into_iter()
            .filter(|child| child.child_session_id != child_id)
            .filter(|child| {
                self.subagent_occupies_slot(&child.child_session_id)
                    .unwrap_or(true)
            })
            .count();
        ensure!(
            occupied < self.config.subagents.max_concurrent,
            "parent session already has the maximum {} active sub-agents",
            self.config.subagents.max_concurrent
        );
        Ok(())
    }

    fn subagent_occupies_slot(&self, child_id: &str) -> Result<bool> {
        let Some(session) = self.state.sessions.get(child_id) else {
            return Ok(false);
        };
        if matches!(
            session.state,
            SessionState::Provisioning | SessionState::Closing | SessionState::Checkpointing
        ) {
            return Ok(true);
        }
        if !session.state.is_active() {
            return Ok(false);
        }
        Ok(
            crate::database::load_materialized_session_summary(child_id)?.is_none_or(|summary| {
                !matches!(
                    summary.execution,
                    mj_core::state::MaterializedExecutionState::Idle
                )
            }),
        )
    }
}

fn sibling_path(path: &Path, parent_id: &str, child_id: &str) -> Result<PathBuf> {
    ensure!(
        path.ends_with(parent_id),
        "parent target path does not end in its session id"
    );
    Ok(path
        .parent()
        .context("parent target path has no parent")?
        .join(child_id))
}

fn borrowed_locator(
    target: &mj_core::state::TargetLocator,
    parent_id: &str,
    child_id: &str,
) -> Result<mj_core::state::TargetLocator> {
    use mj_core::state::TargetLocator;
    Ok(match target {
        TargetLocator::LocalBare { worker_root } => TargetLocator::LocalBare {
            worker_root: sibling_path(worker_root, parent_id, child_id)?,
        },
        TargetLocator::SshBare {
            host,
            workspace,
            worker_id: _,
        } => TargetLocator::SshBare {
            host: host.clone(),
            workspace: workspace.clone(),
            worker_id: Some(child_id.to_owned()),
        },
        // A container child runs its own worker inside the parent's container
        // and records the parent as the container's owner, so cleanup and
        // whole-target operations stay with the parent.
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
            ..
        } => TargetLocator::LocalPodman {
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::LocalDocker { container_id, .. } => TargetLocator::LocalDocker {
            container_id: container_id.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::AppleContainer { container_id, .. } => TargetLocator::AppleContainer {
            container_id: container_id.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::SshPodman {
            host,
            container_id,
            workspace_storage,
            ..
        } => TargetLocator::SshPodman {
            host: host.clone(),
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::SshDocker {
            host, container_id, ..
        } => TargetLocator::SshDocker {
            host: host.clone(),
            container_id: container_id.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        // EC2 children keep the parent's locator unchanged, as they did before
        // container borrowing was recorded.
        other @ TargetLocator::AwsEc2 { .. } => other.clone(),
    })
}

/// Whether a child can be given the `handback` tool. Codex takes Mjolnir's MCP
/// servers over ACP; Claude reads them from its staged profile, which every
/// session has. Other harnesses keep reporting through their last message.
fn child_gets_handback_tool(harness: HarnessKind) -> bool {
    matches!(harness, HarnessKind::Codex | HarnessKind::Claude)
}

/// A parent may delegate to Mjolnir children only if its own stored choice
/// says so; `None` means native sub-agents, same as `Some(false)`. A parent
/// using its harness's native delegation never received the Mjolnir tools, so
/// a request from it is stale.
fn ensure_parent_may_delegate(parent: &SessionRecord) -> Result<()> {
    match parent.mjolnir_subagents {
        Some(true) => Ok(()),
        _ => bail!("this session uses native sub-agents"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_child_borrows_its_parents_container() {
        use mj_core::state::{PodmanWorkspaceLocator, TargetLocator};

        let parent_id = "0123456789abcdef0123456789abcdef";
        let child_id = "fedcba9876543210fedcba9876543210";
        let container = mj_core::targets::resource_name(parent_id).unwrap();

        let local = borrowed_locator(
            &TargetLocator::LocalPodman {
                container_id: container.clone(),
                workspace_storage: PodmanWorkspaceLocator::Volume {
                    name: "parent-volume".to_owned(),
                },
                borrowed_from: None,
            },
            parent_id,
            child_id,
        )
        .unwrap();
        assert_eq!(
            local,
            TargetLocator::LocalPodman {
                container_id: container.clone(),
                workspace_storage: PodmanWorkspaceLocator::Volume {
                    name: "parent-volume".to_owned(),
                },
                borrowed_from: Some(parent_id.to_owned()),
            }
        );

        let remote = borrowed_locator(
            &TargetLocator::SshPodman {
                host: "builder".to_owned(),
                container_id: container.clone(),
                workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
                borrowed_from: None,
            },
            parent_id,
            child_id,
        )
        .unwrap();
        assert_eq!(
            remote,
            TargetLocator::SshPodman {
                host: "builder".to_owned(),
                container_id: container,
                workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
                borrowed_from: Some(parent_id.to_owned()),
            }
        );
    }

    #[test]
    fn a_parent_using_native_delegation_cannot_spawn_mjolnir_children() {
        let parent = |choice| {
            let mut session = crate::controller::test_support::checkpoint_test_session("parent");
            session.mjolnir_subagents = choice;
            session
        };

        assert_eq!(
            ensure_parent_may_delegate(&parent(Some(false)))
                .unwrap_err()
                .to_string(),
            "this session uses native sub-agents"
        );
        assert_eq!(
            ensure_parent_may_delegate(&parent(None))
                .unwrap_err()
                .to_string(),
            "this session uses native sub-agents"
        );
        assert!(ensure_parent_may_delegate(&parent(Some(true))).is_ok());
    }

    #[test]
    fn borrowed_bare_locator_gets_a_private_worker_identity() {
        let locator = mj_core::state::TargetLocator::LocalBare {
            worker_root: PathBuf::from("/workers/parent"),
        };
        assert_eq!(
            borrowed_locator(&locator, "parent", "child").unwrap(),
            mj_core::state::TargetLocator::LocalBare {
                worker_root: PathBuf::from("/workers/child")
            }
        );
    }

    #[test]
    fn borrowed_ssh_locator_keeps_parent_workspace_with_private_worker_identity() {
        let locator = mj_core::state::TargetLocator::SshBare {
            host: "builder".into(),
            workspace: PathBuf::from(".local/share/hel/workspaces/parent-session"),
            worker_id: None,
        };
        assert_eq!(
            borrowed_locator(&locator, "parent-session", "child-session").unwrap(),
            mj_core::state::TargetLocator::SshBare {
                host: "builder".into(),
                workspace: PathBuf::from(".local/share/hel/workspaces/parent-session"),
                worker_id: Some("child-session".into()),
            }
        );
    }
}
