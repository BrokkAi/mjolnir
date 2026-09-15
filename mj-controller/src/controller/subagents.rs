//! Registration and ownership rules for child sessions on a parent's target.

use std::path::{Component, Path, PathBuf};

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
    /// Empty means the parent's working directory; otherwise this is relative.
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
        ensure_safe_relative_directory(&request.working_directory)?;

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
        ensure_parent_may_delegate(&parent, self.config.subagents.enabled)?;
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
            // A child never receives the Mjolnir sub-agent tools, so it can
            // never spawn a grandchild.
            mjolnir_subagents: Some(false),
            create_managed_worktree: Some(false),
            archived: false,
            container_cpus: None,
            container_memory: None,
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
        let relation = SubagentRecord {
            child_session_id: child_id.clone(),
            parent_session_id: parent.id,
            task_name: request.task_name,
            profile_id: request.profile_id,
            model: request.model,
            effort: request.effort,
            working_directory: request.working_directory,
            initial_prompt: request.initial_prompt,
            request_key: request.request_key,
            created_at,
            noticed_turn: None,
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

fn ensure_safe_relative_directory(path: &Path) -> Result<()> {
    if path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        bail!("sub-agent working directory must be relative to the parent's workspace");
    }
    Ok(())
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
        other => other.clone(),
    })
}

/// A parent may delegate to Mjolnir children only if its own stored choice
/// says so; `None` follows the global `[subagents] enabled` setting. A parent
/// using its harness's native delegation never received the Mjolnir tools, so
/// a request from it is stale.
fn ensure_parent_may_delegate(parent: &SessionRecord, global_enabled: bool) -> Result<()> {
    match parent.mjolnir_subagents {
        Some(false) => bail!("this session uses native sub-agents"),
        None if !global_enabled => bail!("sub-agents are disabled"),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parent_using_native_delegation_cannot_spawn_mjolnir_children() {
        let parent = |choice| {
            let mut session = crate::controller::test_support::checkpoint_test_session("parent");
            session.mjolnir_subagents = choice;
            session
        };

        assert_eq!(
            ensure_parent_may_delegate(&parent(Some(false)), true)
                .unwrap_err()
                .to_string(),
            "this session uses native sub-agents"
        );
        assert_eq!(
            ensure_parent_may_delegate(&parent(None), false)
                .unwrap_err()
                .to_string(),
            "sub-agents are disabled"
        );
        // An explicit opt-in outlives the global setting being turned off.
        assert!(ensure_parent_may_delegate(&parent(Some(true)), false).is_ok());
        assert!(ensure_parent_may_delegate(&parent(None), true).is_ok());
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
