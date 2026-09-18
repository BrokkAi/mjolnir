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
    now, projection_rebuild_required, utility_handoff_while_cancellable, verify_resume_checkpoint,
};
use crate::controller::removable_profile_root;
use crate::targets::CommandExecutor;

impl Controller {
    /// Replace the harness of a sealed session without rebuilding its target.
    ///
    /// The session must be the one [`Controller::close_session_for_move`] left
    /// behind for an in-place swap: `Closing`, with its verified checkpoint and
    /// its target still on the record. On success the session is `Running` on
    /// `profile_id` in the same target. On any failure the session is torn down
    /// to `Stopped` with its verified checkpoint, because a half-written worker
    /// root is never retried in place.
    // The target gate below is an ordinary lock held across the restore on
    // purpose; see the comment where it is taken.
    #[allow(
        clippy::await_holding_lock,
        reason = "the target gate deliberately spans the whole in-place restore"
    )]
    pub(in crate::controller) async fn restore_session_in_place(
        &mut self,
        session_id: &str,
        profile_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        ensure!(
            previous.state == SessionState::Closing,
            "session {session_id} is not sealed for an in-place harness replacement"
        );
        let locator = previous
            .target
            .as_ref()
            .context("an in-place harness replacement has no target to replace it in")?;
        let checkpoint = previous
            .checkpoint
            .as_ref()
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
            .get(&previous.target_template_id)
            .with_context(|| format!("unknown target template {:?}", previous.target_template_id))?
            .clone();
        self.validate_muse_resume_destination(
            &previous,
            profile.kind,
            &previous.target_template_id,
        )?;
        ensure!(
            profile.kind != mj_core::config::HarnessKind::Muse
                || previous.additional_mounts.is_empty(),
            "Muse Code ACP supports one workspace root; attached directories are unsupported"
        );
        // Resolving the worker binary is local and costs microseconds. A swap
        // that could never install a worker fails before the old harness is
        // removed from the target.
        crate::controller::worker_binary::preflight_worker_binary(&target_template)?;
        // The directory the *source* profile owns inside the target. It is
        // read from the record's own profile, before the record names the
        // destination one.
        let backend = backend_locator(locator, &previous, &self.config)?;
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

        let archive_manifest = &verified_archive.manifest;
        let canonical_session = std::sync::Arc::clone(&verified_archive.canonical_session);
        let native_continuity =
            native_continuity_preserved(profile.kind, archive_manifest.session.harness_kind);
        // A move always seals its source behind a barrier and never replays the
        // interrupted prompt itself; queued work is admitted afterwards by
        // `admit_move_queue`.
        let discard_queued_prompts = true;
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

        // One record transition, and the crash boundary of the whole swap:
        // before it, recovery finishes the interrupted close; after it,
        // recovery rolls the resume back to `Stopped`. The target stays on the
        // record, because it is the environment being kept.
        {
            let record = self.state.sessions.get_mut(session_id).unwrap();
            record.harness_kind = profile.kind;
            record.last_profile = profile_id.to_string();
            record.native_session_id =
                native_continuity.then(|| archive_manifest.session.native_session_id.clone());
            record.state = SessionState::Provisioning;
            record.updated_at = now();
            record.last_error = None;
        }
        crate::database::save_session(&self.state.sessions[session_id])?;

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
            // is installed. Hold the target lock across the whole call so no
            // background recovery can act on the worker while it has neither
            // harness installed.
            let target_mutex = crate::recovery_gate::worker_target_mutex(session_id);
            // Holding this ordinary lock across the restore is the point: it is
            // the same gate a destroy holds, and the swap is exactly the window
            // where a background worker recovery would act on a worker root
            // that has no harness in it. A move runs on its own runtime through
            // `block_on`, and every other holder takes the lock in synchronous
            // code, so nothing here can park the lock on an unscheduled task.
            let _target_guard = target_mutex.lock().map_err(|_| {
                anyhow::anyhow!("worker target ownership lock poisoned for {session_id}")
            })?;
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
                    primary_repository_root_from_conversion: false,
                    native_continuity,
                    discard_queued_prompts,
                    replay_queue: false,
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
            Ok(materialized) => Ok(materialized),
            Err(error) => {
                // Put back whatever this swap could have written to the durable
                // projection. Both branches restore archived content, so they
                // are correct whether or not the write had happened.
                if rebuild_projection {
                    match materialized_session_from_canonical(session_id, &canonical_session) {
                        Ok(previous_projection) => {
                            if let Err(restore_error) =
                                crate::database::save_materialized_session(&previous_projection)
                            {
                                tracing::error!(
                                    session_id,
                                    error = format!("{restore_error:#}"),
                                    "could not restore the durable projection after an in-place move failed"
                                );
                            }
                        }
                        Err(restore_error) => tracing::error!(
                            session_id,
                            error = format!("{restore_error:#}"),
                            "could not rebuild the durable projection after an in-place move failed"
                        ),
                    }
                } else if let Err(restore_error) =
                    crate::database::replace_materialized_queued_prompts(
                        session_id,
                        &mj_transcript::projection::materialized_queued_prompts_from_canonical(
                            &canonical_session.queued_prompts,
                        ),
                    )
                {
                    tracing::error!(
                        session_id,
                        error = format!("{restore_error:#}"),
                        "could not restore queued prompts after an in-place move failed"
                    );
                }
                // Never retry in place: the rollback tears the target down and
                // leaves the session `Stopped` with its verified checkpoint,
                // which is what the fresh path resumes from. The managed
                // checkout is still present and still owned by the session, so
                // the rollback retires it exactly as a recreated one.
                Err(self.rollback_failed_resume(session_id, &previous, true, error, executor)?)
            }
        }
    }
}
