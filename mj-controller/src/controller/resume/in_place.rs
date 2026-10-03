//! Replacing only the harness of a session that keeps its environment.
//!
//! A move whose destination target, attached resources, and resource
//! allocation are unchanged does not rebuild anything: the source is
//! checkpointed and sealed but its target is kept (the record stays `Closing`
//! with its target), and this module then takes the old harness out of that
//! target and puts the new one in. The container or worker root, the
//! workspace, untracked files, and any build caches survive.

use anyhow::{Context, Result, ensure};
use tokio_util::sync::CancellationToken;

use mj_core::state::{MaterializedSession, SessionState};
use mj_transcript::projection::materialized_session_from_canonical;

use super::{
    Controller, RestoreIntoTarget, WorkerRootReset, backend_locator, native_continuity_preserved,
    now, projection_rebuild_required, restore_projection_after_failed_resume,
    utility_handoff_while_cancellable, verify_resume_checkpoint,
};
use crate::controller::removable_profile_root;
use crate::targets::CommandExecutor;

/// What an in-place swap verified before it changes the record.
struct InPlaceRestorePlan {
    verified_archive: super::VerifiedResumeArchive,
    profile: mj_core::config::HarnessProfile,
    target_template: mj_core::config::TargetTemplate,
    previous_profile_root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InPlaceRestoreMode {
    Move,
    Restart,
}

pub(crate) enum InPlaceRestartError {
    /// The target was only read or probed; fallback can still destroy it.
    Preflight(anyhow::Error),
    /// A requested cancellation or daemon handoff stopped the operation.
    Cancelled(anyhow::Error),
    /// The session record or worker root changed; keep the target for retry.
    Restore(anyhow::Error),
}

impl std::fmt::Debug for InPlaceRestartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight(error) => formatter.debug_tuple("Preflight").field(error).finish(),
            Self::Cancelled(error) => formatter.debug_tuple("Cancelled").field(error).finish(),
            Self::Restore(error) => formatter.debug_tuple("Restore").field(error).finish(),
        }
    }
}

impl std::fmt::Display for InPlaceRestartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight(error) => {
                write!(formatter, "in-place restart preflight failed: {error:#}")
            }
            Self::Cancelled(error) => write!(formatter, "in-place restart cancelled: {error:#}"),
            Self::Restore(error) => write!(
                formatter,
                "in-place restart failed after restore began: {error:#}"
            ),
        }
    }
}

impl std::error::Error for InPlaceRestartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Preflight(error) | Self::Cancelled(error) | Self::Restore(error) => {
                Some(error.as_ref())
            }
        }
    }
}

impl Controller {
    /// Replace the harness of a sealed session without rebuilding its target.
    ///
    /// The session must be the one [`Controller::suspend_session_for_move`] left
    /// behind for an in-place swap: `Closing`, with its verified checkpoint and
    /// its target still on the record, or, on a retry, the `Error` record a
    /// failed attempt retained. On success the session is `Running` on
    /// `profile_id` in the same environment, recorded under
    /// `target_template_id`. Every failure stops only the worker and leaves
    /// the record `Error`, keeping the environment and checkpoint for an
    /// explicit retry.
    pub(in crate::controller) async fn restore_session_in_place(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_template_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        match self
            .restore_session_in_place_with_mode(
                session_id,
                profile_id,
                target_template_id,
                executor,
                InPlaceRestoreMode::Move,
            )
            .await?
        {
            Ok(session) => Ok(session),
            Err(error) => Err(anyhow::Error::msg(error)),
        }
    }

    pub(crate) async fn restore_session_in_place_for_restart(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_template_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<std::result::Result<MaterializedSession, InPlaceRestartError>> {
        self.restore_session_in_place_with_mode(
            session_id,
            profile_id,
            target_template_id,
            executor,
            InPlaceRestoreMode::Restart,
        )
        .await
    }

    #[allow(
        clippy::await_holding_lock,
        reason = "the target gate deliberately spans the whole in-place restore"
    )]
    async fn restore_session_in_place_with_mode(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_template_id: &str,
        executor: &(impl CommandExecutor + Sync),
        mode: InPlaceRestoreMode,
    ) -> Result<std::result::Result<MaterializedSession, InPlaceRestartError>> {
        crate::worker_lifecycle::run(session_id, "restore session in place", executor, async {
            if let Err(error) = crate::worker_lifecycle::require(session_id)
                .and_then(|owner| owner.verify_cached_target(&self.state))
            {
                if mode == InPlaceRestoreMode::Restart {
                    return Ok(Err(InPlaceRestartError::Preflight(error)));
                }
                return Err(error);
            }
            let previous = self
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?
                .clone();
            let move_operation = if mode == InPlaceRestoreMode::Move {
                crate::database::load_move_operation(session_id)?
            } else {
                None
            };
            // A first attempt finds the source sealed (`Closing`); a retry finds
            // the environment its failed attempt retained (`Error`).
            let retained_state = previous.state == SessionState::Closing
                || (mode == InPlaceRestoreMode::Restart
                    && matches!(
                        previous.state,
                        SessionState::Error | SessionState::Stopped | SessionState::Provisioning
                    ))
                || (previous.state == SessionState::Error
                    && move_operation
                        .as_ref()
                        .is_some_and(mj_core::state::MoveOperation::holds_source_environment));
            if !retained_state || previous.target.is_none() {
                let error = anyhow::anyhow!(
                    "session {session_id} is not ready for an in-place harness replacement"
                );
                if mode == InPlaceRestoreMode::Restart {
                    return Ok(Err(InPlaceRestartError::Preflight(error)));
                }
                return Err(error);
            }
            // From here the environment belongs to this swap. Every failure,
            // before or after the record transition below, stops only the worker
            // and leaves the record `Error` with the environment retained, so no
            // failure can leave the session suspending without an owner.
            let plan = match self.plan_in_place_restore(
                &previous,
                move_operation.as_ref(),
                profile_id,
                target_template_id,
                executor,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    if mode == InPlaceRestoreMode::Restart {
                        return Ok(Err(InPlaceRestartError::Preflight(error)));
                    }
                    return Err(self.retain_failed_in_place_move(session_id, &previous, error)?);
                }
            };
            let InPlaceRestorePlan {
                verified_archive,
                profile,
                target_template,
                previous_profile_root,
            } = plan;

            let archive_manifest = &verified_archive.manifest;
            let canonical_session = std::sync::Arc::clone(&verified_archive.canonical_session);
            let native_continuity =
                native_continuity_preserved(profile.kind, archive_manifest.session.harness_kind);
            let (discard_queued_prompts, replay_queue) = match mode {
                // A move admits its queue only after destination readiness.
                InPlaceRestoreMode::Move => (true, false),
                // Restart follows ordinary same-harness resume semantics.
                InPlaceRestoreMode::Restart => (false, true),
            };
            let context_bytes = crate::handoff::profile_handoff_bytes(Some(&profile));
            let utility_config = (!native_continuity).then(|| self.config.clone());
            let stored_frontier = crate::database::materialized_event_frontier(session_id)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not read the stored projection frontier; rebuilding it from the archive"
                );
                None
            });
            let rebuild_projection = projection_rebuild_required(
                stored_frontier
                    .as_ref()
                    .map(|(ordinal, digest)| (*ordinal, digest.as_str())),
                canonical_session.event_frontier,
                &canonical_session.event_frontier_digest,
            );
            let projection_build = rebuild_projection.then(|| {
                let canonical = std::sync::Arc::clone(&canonical_session);
                let session_id = session_id.to_owned();
                tokio::task::spawn_blocking(move || {
                    materialized_session_from_canonical(session_id, &canonical)
                })
            });

            if mode == InPlaceRestoreMode::Restart && executor.cancellation_requested() {
                return Ok(Err(InPlaceRestartError::Cancelled(anyhow::anyhow!(
                    "restart cancelled before the worker reset"
                ))));
            }

            // One record transition, and the crash boundary of the whole swap:
            // before it, recovery finishes the interrupted close; after it,
            // recovery stops the partial worker and records a retryable error. The
            // target stays on the record because the environment is being kept.
            {
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.harness_kind = profile.kind;
                record.last_profile = profile_id.to_string();
                // Another bare target on the same machine is the same environment;
                // the record only names it differently afterwards.
                record.target_template_id = target_template_id.to_string();
                record.target_runtime = Some((&target_template).into());
                record.native_session_id =
                    native_continuity.then(|| archive_manifest.session.native_session_id.clone());
                record.state = SessionState::Provisioning;
                record.updated_at = now();
                record.last_error = None;
            }
            crate::database::save_resumed_session(&self.state.sessions[session_id], None)?;

            let result = async {
                // A cross-harness swap has no provisioning to overlap with, so the
                // handoff is compacted here, before the target is touched. It
                // watches the executor for cancellation itself.
                let utility_handoff = match utility_config.as_ref() {
                    Some(config) => Some(
                        utility_handoff_while_cancellable(
                            session_id,
                            config,
                            &canonical_session,
                            context_bytes,
                            executor,
                            CancellationToken::new(),
                        )
                        .await
                        .context("prepare the cross-harness handoff")?,
                    ),
                    None => None,
                };
                // The reset that empties the target of the old harness runs inside
                // `restore_into_target`, immediately before the new worker binary
                // is installed. Retain worker ownership across the whole call so no
                // background recovery can act on the worker while it has neither
                // harness installed.
                self.restore_into_target(
                    session_id,
                    RestoreIntoTarget {
                        profile: &profile,
                        archive: &verified_archive,
                        restored_archive: &verified_archive.archive_path,
                        resumed_project_directory: previous.project_directory.clone(),
                        resumed_container_workspace: previous.container_workspace.clone(),
                        // The repositories are already in the workspace, untouched
                        // by the swap; only the harness state is restored.
                        restore_repositories: false,
                        native_continuity,
                        discard_queued_prompts,
                        replay_queue,
                        utility_handoff,
                        projection_build,
                        resume_notices: Vec::new(),
                        // EC2-only, and in-place eligibility requires unchanged
                        // attached resources, so they are already on the instance.
                        install_attached_resources: false,
                        worker_root_reset: WorkerRootReset::InPlace {
                            previous_profile_root,
                        },
                        retire_after_ready: None,
                    },
                    executor,
                )
                .await
            }
            .await;
            match result {
                Ok(materialized) => Ok(Ok(materialized)),
                Err(error) => {
                    // Put back whatever this swap wrote to the durable projection,
                    // including the failed worker's own lines.
                    restore_projection_after_failed_resume(
                        session_id,
                        &canonical_session,
                        discard_queued_prompts,
                    );
                    if mode == InPlaceRestoreMode::Restart {
                        let error =
                            self.retain_failed_in_place_restart(session_id, &previous, error)?;
                        Ok(Err(InPlaceRestartError::Restore(error)))
                    } else {
                        Err(self.retain_failed_in_place_move(session_id, &previous, error)?)
                    }
                }
            }
        })
        .await
    }

    /// Everything an in-place swap checks before it changes the record. It
    /// reads the target but changes nothing on it.
    fn plan_in_place_restore(
        &self,
        previous: &mj_core::state::SessionRecord,
        move_operation: Option<&mj_core::state::MoveOperation>,
        profile_id: &str,
        target_template_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<InPlaceRestorePlan> {
        let session_id = previous.id.as_str();
        let locator = previous
            .target
            .as_ref()
            .context("an in-place harness replacement has no target to replace it in")?;
        let checkpoint = move_operation
            .and_then(|op| op.handoff.as_ref())
            .or(previous.checkpoint.as_ref())
            .context("session has no checkpoint")?;
        let verified_archive = verify_resume_checkpoint(session_id, checkpoint)?;
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .with_context(|| format!("unknown profile {profile_id:?}"))?
            .clone();
        ensure!(profile.enabled, "profile {profile_id:?} is disabled");
        let target_template = self
            .config
            .targets
            .get(target_template_id)
            .with_context(|| format!("unknown target template {target_template_id:?}"))?
            .clone();
        self.validate_muse_resume_destination(previous, profile.kind, target_template_id)?;
        ensure!(
            profile.kind != mj_core::config::HarnessKind::Muse
                || previous.additional_mounts.is_empty(),
            "Muse Code ACP supports one workspace root; attached directories are unsupported"
        );
        // Resolving the worker binary is local and costs microseconds. A swap
        // that could never install a worker fails before the old harness is
        // removed from the target.
        crate::controller::worker_binary::preflight_worker_binary(&target_template, executor)?;
        // The directory the *source* profile owns inside the target. It is
        // read from the record's own profile, before the record names the
        // destination one.
        let backend = backend_locator(locator, previous, &self.config)?;
        let worker_root = crate::targets::worker_root(&backend, session_id)?;
        super::super::execute_checked(
            executor,
            crate::targets::command_on_locator(
                &backend,
                session_id,
                vec!["test".into(), "-d".into(), worker_root],
                "verify retained worker root",
            )?,
        )?;
        if let Some(checkout) = &previous.managed_worktree {
            ensure!(
                super::managed_worktree_checkout_exists(executor, checkout)?,
                "retained checkout is missing; refusing to recreate it"
            );
        } else if let Some(path) = &previous.project_directory {
            self.validate_project_directory(target_template_id, path, executor)?;
        }
        let source_profile = self
            .config
            .profiles
            .get(&previous.last_profile)
            .with_context(|| {
                format!(
                    "session profile {:?} is missing; it names the profile home to remove",
                    previous.last_profile
                )
            })?;
        let previous_profile_root = removable_profile_root(&backend, session_id, source_profile);
        Ok(InPlaceRestorePlan {
            verified_archive,
            profile,
            target_template,
            previous_profile_root,
        })
    }

    /// The Move owns this environment; cleanup may stop its worker, never retire
    /// its checkout or destroy the container. Error keeps ordinary recovery out.
    pub(in crate::controller) fn retain_failed_in_place_move(
        &mut self,
        session_id: &str,
        previous: &mj_core::state::SessionRecord,
        error: anyhow::Error,
    ) -> Result<anyhow::Error> {
        self.retain_failed_in_place_restore(session_id, previous, error, "Move")
    }

    pub(crate) fn retain_failed_in_place_restart(
        &mut self,
        session_id: &str,
        previous: &mj_core::state::SessionRecord,
        error: anyhow::Error,
    ) -> Result<anyhow::Error> {
        self.retain_failed_in_place_restore(session_id, previous, error, "Restart")
    }

    fn retain_failed_in_place_restore(
        &mut self,
        session_id: &str,
        previous: &mj_core::state::SessionRecord,
        error: anyhow::Error,
        owner: &'static str,
    ) -> Result<anyhow::Error> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "retain failed in-place restore",
            &crate::targets::ProcessExecutor,
            || {
                let current = self
                    .state
                    .sessions
                    .get(session_id)
                    .context("session missing")?;
                ensure!(
                    current.target.is_some() && current.target == previous.target,
                    "retained {owner} target changed; refusing cleanup"
                );
                let backend =
                    backend_locator(current.target.as_ref().unwrap(), current, &self.config)?;
                let root = crate::targets::worker_root(&backend, session_id)?;
                let cleanup = super::super::execute_checked(
                    &crate::targets::CancellableProcessExecutor::with_timeout(
                        std::time::Duration::from_secs(15),
                    ),
                    crate::targets::command_on_locator(
                        &backend,
                        session_id,
                        vec![
                            "sh".into(),
                            "-c".into(),
                            crate::targets::stop_worker_daemon_script(&root),
                        ],
                        "stop failed in-place worker while retaining its environment",
                    )?,
                );
                let mut retained = previous.clone();
                retained.state = SessionState::Error;
                retained.updated_at = now();
                retained.last_error =
                    Some(format!("{error:#}; environment retained for {owner} retry"));
                if let Err(cleanup_error) = &cleanup {
                    retained.last_error = Some(format!(
                        "{error:#}; stopping retained worker failed: {cleanup_error:#}"
                    ));
                }
                crate::database::save_resumed_session(&retained, None)?;
                self.state.sessions.insert(session_id.to_owned(), retained);
                cleanup.context("stop retained worker before retry")?;
                Ok(error)
            },
        )
    }
}
