//! Session provisioning, rollback, and worker-side Git bootstrap.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{TargetTemplate, atomic_write, data_dir};
use mj_core::state::{SessionState, State, TargetLocator};

use crate::targets::{
    self, CommandExecutor, CommandOutput, CommandSpec, ProvisionStage, ProvisionStageGuard,
};

use super::backend::{
    ContainerOverrides, TargetCheck, backend_locator, backend_session_bundle, backend_target,
    configure_github_token_environment, preflight_target, use_github_https_urls,
};
use super::git_cache;
use super::readiness::{connect_started_worker, wait_for_native_session_in_stage};
use super::worker_binary::{bridge_readiness_stage, start_worker_durably, worker_probe_diagnosis};
use super::{Controller, execute_checked, now};

const INHERITED_GIT_SETTINGS: &[&str] = &[
    "diff.algorithm",
    "fetch.prune",
    "fetch.prunetags",
    "init.defaultbranch",
    "merge.conflictstyle",
    "pull.ff",
    "pull.rebase",
    "push.autosetupremote",
    "push.default",
    "rebase.autostash",
    "rerere.autoupdate",
    "rerere.enabled",
    "user.email",
    "user.name",
];

/// How many sub-agent children may be brought up inside one container at the
/// same time.
///
/// Starting a child means starting a harness, and a harness start inside a
/// container is expensive, especially while its reviewer is active. Measured
/// on a local Podman
/// target, ten children started one after another each reached their harness
/// in about seven seconds, while four started at once left two or three of
/// them past the 300-second harness-startup wait. Admitting two at a time
/// keeps a burst slower but finished, instead of fast and failed.
const CONTAINER_START_ADMISSION: usize = 2;

/// The admission gate for one container, created on first use.
///
/// Keyed by the container the children share. A bare target has no gate: a
/// child there is an ordinary process on a whole machine, and twenty
/// sequential and four concurrent starts measured 2.8 seconds each.
pub(super) fn container_start_gate(
    locator: &targets::TargetLocator,
) -> Option<Arc<tokio::sync::Semaphore>> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>> = OnceLock::new();
    let container = match locator {
        targets::TargetLocator::LocalPodman { container_id, .. }
        | targets::TargetLocator::LocalDocker { container_id, .. }
        | targets::TargetLocator::AppleContainer { container_id, .. }
        | targets::TargetLocator::SshPodman { container_id, .. }
        | targets::TargetLocator::SshDocker { container_id, .. } => container_id.clone(),
        targets::TargetLocator::LocalBare { .. }
        | targets::TargetLocator::SshBare { .. }
        | targets::TargetLocator::AwsEc2 { .. } => return None,
    };
    let gates = GATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut gates = gates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(Arc::clone(gates.entry(container).or_insert_with(|| {
        Arc::new(tokio::sync::Semaphore::new(CONTAINER_START_ADMISSION))
    })))
}

/// Whether starting a child worker can be tried again.
///
/// Only a worker that provably never published its control socket qualifies:
/// it owns no relay, no journal and no harness, so a second start cannot
/// duplicate or corrupt work. A refusal is never retried, because it names a
/// precondition that a second attempt would meet in exactly the same way, and
/// a cancelled operation is not retried either.
fn subagent_start_is_retryable(error: &anyhow::Error) -> bool {
    if mj_core::refusal::Refusal::of(error).is_some() {
        return false;
    }
    if format!("{error:#}").contains("operation cancelled") {
        return false;
    }
    error
        .downcast_ref::<super::readiness::WorkerStartupFailure>()
        .is_some_and(|failure| !failure.reached_socket)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProvisioningFailureDisposition {
    /// A freshly registered session has no durable history to retain.
    Discard,
    /// Resume owns rollback to the archived record and checkpoint lineage.
    Preserve,
}

impl Controller {
    pub async fn provision_session_controlled_with_commit(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        grant_commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        crate::worker_lifecycle::run(
            session_id,
            "provision session controlled with commit",
            executor,
            async {
                crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
                let github_token = self
                    .github_token_for_session(session_id)
                    .await
                    .context("resolve GitHub credentials for session")?;
                let repositories = self
                    .provision_session_target_with_failure_disposition(
                        session_id,
                        executor,
                        github_token.as_deref(),
                        ProvisioningFailureDisposition::Discard,
                    )
                    .await?;
                let setup = execute_concurrent_lanes(
                    || execute_repository_setup(&repositories, executor),
                    || self.install_worker_payload(session_id, executor),
                );
                let result = match setup {
                    Ok(((), (backend, worker_root))) => {
                        self.connect_and_start_worker(
                            session_id,
                            executor,
                            &backend,
                            &worker_root,
                            true,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(native_session_id) => {
                        if let Err(error) = grant_commit() {
                            return Err(self.rollback_failed_new_session(session_id, error)?);
                        }
                        self.mark_worker_connected(session_id, native_session_id)
                    }
                    Err(error) => Err(self.rollback_failed_new_session(session_id, error)?),
                }
            },
        )
        .await
    }

    /// Start a child worker inside an already-provisioned parent target.
    /// Repository, target, and mount setup belong exclusively to the parent.
    pub async fn provision_subagent_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        crate::worker_lifecycle::run(
            session_id,
            "provision subagent session controlled",
            executor,
            async {
                crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
                // One retry, and only for a worker that provably never published a
                // control socket. Such a worker has no relay, no durable journal and
                // no harness, so starting another over the same root cannot duplicate
                // or corrupt anything. A spawn is issued by a model that cannot see
                // the target, so a transient start failure it could have retried by
                // hand is better retried here.
                let mut attempts: Vec<String> = Vec::new();
                loop {
                    let (result, placement) =
                        self.attempt_subagent_start(session_id, executor).await;
                    let error = match result {
                        Ok(native_session_id) => {
                            return self.mark_worker_connected(session_id, native_session_id);
                        }
                        Err(error) => error,
                    };
                    let retry = attempts.is_empty() && subagent_start_is_retryable(&error);
                    let error = match &placement {
                        Some((backend, _)) => super::subagent_park::explain_process_exhaustion(
                            error, backend, session_id,
                        ),
                        None => error,
                    };
                    attempts.push(format!("{error:#}"));
                    let error = if attempts.len() > 1 {
                        anyhow::anyhow!(
                            "{}",
                            attempts
                                .iter()
                                .enumerate()
                                .map(|(index, error)| format!("attempt {}: {error}", index + 1))
                                .collect::<Vec<_>>()
                                .join("; ")
                        )
                    } else {
                        error
                    };
                    let diagnostic = note_new_session_launch_failure(session_id, &error);
                    let previous = self
                        .state
                        .sessions
                        .get(session_id)
                        .context("failed child disappeared")?
                        .clone();
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::StartupCleanup;
                    record.updated_at = now();
                    record.last_error = Some(format!("sub-agent startup failed: {diagnostic}"));
                    self.persist_session_transition_or_restore(
                        session_id,
                        &previous,
                        "record failed startup before teardown",
                    )?;
                    let cleanup = super::failed_launch_cleanup_executor();
                    if let Err(cleanup_error) =
                        self.finish_failed_startup_controlled(session_id, &cleanup, retry)
                    {
                        return Err(error.context(format!(
                            "startup cleanup remains pending: {cleanup_error:#}"
                        )));
                    }
                    if !retry {
                        return Err(error);
                    }
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "sub-agent worker never started and was stopped; retrying once"
                    );
                }
            },
        )
        .await
    }

    /// Finish a failed startup without reopening its relay or replaying work.
    pub(crate) fn cleanup_failed_startup_controlled(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        self.finish_failed_startup_controlled(session_id, executor, false)
    }

    fn finish_failed_startup_controlled(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
        retry_after_stop: bool,
    ) -> Result<()> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "finish failed startup controlled",
            executor,
            || {
                let previous = self
                    .state
                    .sessions
                    .get(session_id)
                    .context("cleanup session disappeared")?
                    .clone();
                ensure!(
                    previous.state == SessionState::StartupCleanup,
                    "session is not awaiting startup cleanup"
                );
                let child = self.state.subagents.contains_key(session_id);
                ensure!(
                    !retry_after_stop || child,
                    "only unaccepted child startup may retry"
                );
                let outcome = (|| -> Result<()> {
                    if let Some(locator) = &previous.target {
                        let backend = backend_locator(locator, &previous, &self.config)?;
                        if child {
                            let root = targets::worker_root(&backend, session_id)?;
                            super::worker_binary::stop_worker(
                                &crate::worker_lifecycle::require(session_id)?,
                                executor,
                                &backend,
                                &root,
                            )?;
                        } else {
                            targets::close_plan(&backend, session_id)?.execute(executor)?;
                        }
                    }
                    if !child {
                        self.cleanup_new_session_worktree_after_failure(session_id, executor)?;
                    }
                    Ok(())
                })();
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.updated_at = now();
                let cause = previous
                    .last_error
                    .as_deref()
                    .unwrap_or("startup failed")
                    .split("; startup cleanup failed:")
                    .next()
                    .unwrap()
                    .to_owned();
                match &outcome {
                    Ok(()) => {
                        record.state = if retry_after_stop {
                            SessionState::Provisioning
                        } else {
                            SessionState::Error
                        };
                        record.last_error = (!retry_after_stop).then_some(cause);
                        if !child {
                            record.target = None;
                        }
                    }
                    Err(error) => {
                        record.last_error =
                            Some(format!("{cause}; startup cleanup failed: {error:#}"));
                    }
                }
                let record = self.state.sessions.get_mut(session_id).unwrap();
                if outcome.is_ok() && child && !retry_after_stop {
                    record.archived = true;
                }
                if let Err(error) = crate::database::save_startup_cleanup_outcome(
                    record,
                    outcome.is_ok() && child && !retry_after_stop,
                ) {
                    self.state.sessions.insert(session_id.to_owned(), previous);
                    return Err(error).context("record startup cleanup outcome");
                }
                outcome
            },
        )
    }

    /// One start of a child worker: place it, install its files, and wait for
    /// its relay. The placement is returned even on failure, because the
    /// caller needs it to stop a worker that may be half up.
    async fn attempt_subagent_start(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> (
        Result<Option<String>>,
        Option<(targets::TargetLocator, String)>,
    ) {
        // Placement failures must reach the same failure arm as startup
        // failures; otherwise the child record stays `Provisioning` forever.
        match self.worker_placement(session_id) {
            Ok((backend, worker_root)) => {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                let prepared = self
                    .prepare_worker_files(session_id, &backend, &worker_root, syncing)
                    .and_then(|()| self.prepare_subagent_report_dir(session_id, &backend, syncing));
                let result = match prepared {
                    Ok(()) => {
                        // Held across the harness startup wait, which is the
                        // part that does not survive a crowd.
                        let gate = container_start_gate(&backend);
                        let _admitted = match &gate {
                            Some(gate) => gate.acquire().await.ok(),
                            None => None,
                        };
                        self.connect_and_start_worker(
                            session_id,
                            executor,
                            &backend,
                            &worker_root,
                            false,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                (result, Some((backend, worker_root)))
            }
            Err(error) => (Err(error), None),
        }
    }

    fn rollback_failed_new_session(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
    ) -> Result<anyhow::Error> {
        self.rollback_failed_new_session_with(
            session_id,
            error,
            // Rollback must remain possible after the foreground operation's
            // cancellation token has been set.
            &super::failed_launch_cleanup_executor(),
        )
    }

    fn rollback_failed_new_session_with(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
        target_cleanup_executor: &impl CommandExecutor,
    ) -> Result<anyhow::Error> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        // The failure goes on record before anything the record points at is
        // removed. Removing the worker and its checkout can outlast a daemon
        // that is stopping, and a daemon that exits partway would otherwise
        // leave a record naming a worker that no longer exists, which every
        // later daemon keeps reconnecting to. A failed record that still
        // names its target is one Destroy knows how to clean up.
        let original = note_new_session_launch_failure(session_id, &error);
        apply_failed_new_session_launch(&mut self.state, session_id, &original);
        self.persist_session_transition_or_restore(
            session_id,
            &session,
            "record the failed launch before removing its target",
        )
        .map_err(|persist_error| {
            persist_error.context(format!(
                "{original}; its target was left in place because the failure could not be recorded"
            ))
        })?;
        let target_cleanup = match session.target.as_ref() {
            Some(locator) => (|| -> Result<()> {
                let backend = backend_locator(locator, &session, &self.config)?;
                targets::close_plan(&backend, session_id)?
                    .execute(target_cleanup_executor)
                    .map(|_| ())
            })(),
            None => Ok(()),
        };
        // A failed stop leaves the harness able to write its checkout. Keep
        // that checkout until a later cleanup confirms process termination.
        let cleanup_error = target_cleanup
            .and_then(|()| {
                self.cleanup_new_session_worktree_after_failure(session_id, target_cleanup_executor)
            })
            .err()
            .map(|error| format!("{error:#}"));
        if let Some(cleanup_error) = &cleanup_error {
            tracing::warn!(
                session_id,
                error = %cleanup_error,
                "new-session rollback cleanup reported failures"
            );
        }
        let failure = apply_failed_new_session_rollback(
            &mut self.state,
            session_id,
            &original,
            cleanup_error,
        );
        self.persist_session_state(session_id)?;
        Ok(failure)
    }

    pub(super) async fn provision_session_with_failure_disposition(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        github_token: Option<&str>,
        failure_disposition: ProvisioningFailureDisposition,
    ) -> Result<()> {
        let repositories = self
            .provision_session_target_with_failure_disposition(
                session_id,
                executor,
                github_token,
                failure_disposition,
            )
            .await?;
        match execute_repository_setup(&repositories, executor) {
            Ok(()) => Ok(()),
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                Err(self.rollback_failed_new_session(session_id, error)?)
            }
            Err(error) => Err(error),
        }
    }

    async fn provision_session_target_with_failure_disposition(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        github_token: Option<&str>,
        failure_disposition: ProvisioningFailureDisposition,
    ) -> Result<targets::CommandPlan> {
        crate::worker_lifecycle::run(session_id, "provision session target with failure disposition", executor, async {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if session.state != SessionState::Provisioning {
            bail!("session {session_id} is not provisioning");
        }
        if let Some(plan) = self.adopt_prepared_ec2_destination(session_id)? {
            return Ok(plan);
        }
        let preparation = (|| {
            let selected = self
                .config
                .targets
                .get(&session.target_template_id)
                .context("target template disappeared before provisioning")?;
            // Before the runtime is recorded, so a container never starts
            // without an entry whose secret is missing.
            selected.ensure_ready(&session.target_template_id)?;
            self.config
                .profiles
                .get(&session.last_profile)
                .context("harness profile disappeared before provisioning")?
                .ensure_ready(&session.last_profile)?;
            let runtime = mj_core::state::TargetRuntimeSettings::from(selected);
            if let Some(recorded) = &session.target_runtime {
                ensure!(
                    recorded == &runtime,
                    "target access settings changed before provisioning; retry with the selected target"
                );
            } else {
                self.state
                    .sessions
                    .get_mut(session_id)
                    .unwrap()
                    .target_runtime = Some(runtime);
                self.persist_session_state(session_id)?;
            }
            let template = self
                .config
                .targets
                .get(&session.target_template_id)
                .context("target template disappeared during provisioning")?;
            let profile = self
                .config
                .profiles
                .get(&session.last_profile)
                .context("harness profile disappeared during provisioning")?;
            super::worker_binary::preflight_worker_binary(template, executor)?;
            super::worker_binary::preflight_harness(template, profile, executor)?;
            self.prepare_managed_raw_worktree(session_id, executor)
        })();
        let created_worktree = match preparation {
            Ok(created) => created,
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                return Err(self.fail_new_session_with_cleanup(session_id, error, executor)?);
            }
            Err(error) => return Err(error),
        };
        let session = self
            .state
            .sessions
            .get(session_id)
            .expect("session retained after managed worktree preparation")
            .clone();
        let checkout = self.state.checkout(session_id)?;
        let project_directory = checkout.project_directory();
        // Keep planning, preflight, creation, and locator discovery in one
        // result so the caller's failure disposition applies to every error.
        let result = (|| {
            let template = self
                .config
                .targets
                .get(&session.target_template_id)
                .context("target template disappeared during provisioning")?;
            if matches!(template, TargetTemplate::AwsEc2 { .. }) {
                for resource in &session.additional_mounts {
                    ensure!(
                        resource.source.is_dir(),
                        "attached resource source is not a directory: {}",
                        resource.source.display()
                    );
                }
            }
            let mut target = backend_target(
                template,
                session.resource_allocation.as_ref(),
                ContainerOverrides::for_session(&session),
            )?;
            let mut runtime_mounts = if matches!(target, targets::TargetTemplate::AwsEc2(_)) {
                Vec::new()
            } else {
                session.additional_mounts.clone()
            };
            // The mounts this container runs with, not the ones the session
            // stores: a forced downgrade belongs to the host the container
            // lands on, so it is decided here every time and never written
            // over the user's choice.
            for notice in enforce_overlay_capable_mounts(&target, &mut runtime_mounts, executor) {
                executor.notify_notice(&notice);
            }
            let mut bundle = if project_directory.is_some() {
                None
            } else if let Some(bundle) = self.move_destination_bundle(session_id)? {
                Some(bundle)
            } else if failure_disposition == ProvisioningFailureDisposition::Preserve {
                Some(super::network_git::checkpoint_bundle(&session)?)
            } else {
                Some(backend_session_bundle(&session, &self.config, executor)?)
            };
            let container_github_token =
                github_token.filter(|_| configure_github_token_environment(&mut target));
            if container_github_token.is_some()
                && let Some(bundle) = bundle.as_mut()
            {
                use_github_https_urls(bundle);
            }
            preflight_target(template, executor, TargetCheck::Launch)?;
            let resource_name = crate::database::load_move_operation(session_id)?
                .filter(|op| {
                    op.workspace_transfer.is_some()
                        && op.phase == mj_core::state::MovePhase::ResumingDestination
                })
                .map(|op| targets::move_resource_name(session_id, &op.operation_id))
                .unwrap_or_else(|| targets::resource_name(session_id))?;
            let moving_workspace = resource_name != targets::resource_name(session_id)?;
            let prepared_cache = bundle
                .as_mut()
                .filter(|_| !moving_workspace)
                .and_then(|bundle| {
                    git_cache::prepare(
                        &target,
                        session_id,
                        bundle,
                        &mut runtime_mounts,
                        container_github_token,
                        executor,
                    )
                });
            // Mounts are fixed when the container is created, so the build
            // cache is decided here, before the provisioning plan is built.
            let build_cache = super::mbx::prepare(
                &target,
                &session,
                bundle.as_ref(),
                prepared_cache.as_ref(),
                &mut runtime_mounts,
                executor,
            );
            let provision = if let Some(project_directory) = project_directory {
                targets::provision_bare_project_plan(
                    &target,
                    session_id,
                    &project_directory.to_string_lossy(),
                )
            } else {
                bundle
                    .as_ref()
                    .context("project bundle disappeared during provisioning")
                    .and_then(|bundle| {
                        targets::provision_plan_named(
                            &target,
                            session_id,
                            bundle,
                            &runtime_mounts,
                            session.container_workspace.as_deref(),
                            &resource_name,
                        )
                    })
            };
            let mut provision = match provision {
                Ok(provision) => provision,
                Err(error) => {
                    if let Some(cache) = &prepared_cache {
                        let _ = cache.cleanup(executor);
                    }
                    return Err(error);
                }
            };
            if let Some(token) = container_github_token
                && let Err(error) =
                    provision.provide_target_environment_secret(&target, "GH_TOKEN", token)
            {
                if let Some(cache) = &prepared_cache {
                    let _ = cache.cleanup(executor);
                }
                return Err(error);
            }

            let started = Instant::now();
            let result = provision_target_creation_named(
                &provision,
                &target,
                session_id,
                executor,
                &resource_name,
                |outputs| {
                    super::backend::locator_after_provision_named(
                        template,
                        &target,
                        session_id,
                        outputs.first(),
                        executor,
                        &resource_name,
                    )
                },
            )
            .map(|(locator, remainder)| (locator, remainder, bundle, build_cache));
            if result.is_err()
                && let Some(cache) = &prepared_cache
            {
                if let Some(locator) =
                    provisioned_locator_named(&target, session_id, None, &resource_name)
                {
                    let _ = targets::close_plan(&locator, session_id)
                        .and_then(|plan| plan.execute(executor).map(|_| ()));
                } else {
                    let _ = cache.cleanup(executor);
                }
            }
            tracing::debug!(
                session_id,
                elapsed_ms = started.elapsed().as_millis(),
                "provisioning plan execution completed"
            );
            result
        })();
        drop(checkout);
        let result = match result {
            Err(error)
                if created_worktree
                    && failure_disposition == ProvisioningFailureDisposition::Discard =>
            {
                return Err(self.fail_new_session_with_cleanup(session_id, error, executor)?);
            }
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Preserve => {
                Err(error)
            }
            Err(error) => {
                // This arm (no managed worktree to unwind) is the one a
                // provisioning failure such as a dropped target connection
                // hits; record the diagnostic and the session-id log here too.
                let detail = note_new_session_launch_failure(session_id, &error);
                {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::Error;
                    record.target = None;
                    record.updated_at = super::now();
                    record.last_error = Some(format!("session provisioning failed: {detail}"));
                }
                return match self.persist_session_state(session_id) {
                    Ok(()) => Err(error),
                    Err(persistence_error) => Err(error.context(format!(
                        "persist removal of failed provisioning session {session_id}: {persistence_error:#}"
                    ))),
                };
            }
            Ok((locator, remainder, bundle, build_cache)) => {
                apply_new_session_provisioning_result(&mut self.state, session_id, Ok(locator))?;
                self.state
                    .sessions
                    .get_mut(session_id)
                    .expect("session retained after provisioning")
                    .build_cache = build_cache;
                let session = &self.state.sessions[session_id];
                let backend = backend_locator(
                    session
                        .target
                        .as_ref()
                        .context("provisioned target disappeared")?,
                    session,
                    &self.config,
                )?;
                if matches!(backend, targets::TargetLocator::AwsEc2 { .. }) {
                    targets::provision_on_locator_plan(
                        &backend,
                        session_id,
                        bundle
                            .as_ref()
                            .context("AWS provisioning requires a project bundle")?,
                    )
                } else {
                    Ok(remainder)
                }
            }
        };
        let result = match result {
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                return Err(self.rollback_failed_new_session(session_id, error)?);
            }
            result => result,
        };
        if result.is_ok() {
            let target_template_id = self
                .state
                .sessions
                .get(session_id)
                .map(|session| session.target_template_id.clone());
            let directory = match self.state.checkout(session_id)?.effective() {
                mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                    Some(worktree.source_project_directory.clone())
                }
                mj_core::state::Checkout::Attached { path } => Some(path.to_path_buf()),
                mj_core::state::Checkout::ManagedWorkspace => None,
                mj_core::state::Checkout::Borrowed { .. } => {
                    unreachable!("effective checkout resolves borrowing")
                }
            };
            let host = target_template_id
                .as_deref()
                .and_then(|id| self.config.targets.get(id))
                .and_then(|template| match template {
                    TargetTemplate::LocalBare => Some("local".to_owned()),
                    TargetTemplate::SshBare { ssh, .. } => Some(ssh.host.clone()),
                    _ => None,
                });
            if let (Some(directory), Some(host)) = (directory, host) {
                self.state.remember_project_directory(&host, &directory);
                crate::database::remember_project_directory(&host, &directory)?;
            }
        }
        self.persist_session_state(session_id)?;
        result

        }).await
    }

    pub fn mark_worker_connected(
        &mut self,
        session_id: &str,
        native_session_id: Option<String>,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.target.is_none() {
            bail!("session {session_id} has no provisioned target");
        }
        let updated_at = now();
        crate::database::mark_session_worker_connected(
            session_id,
            native_session_id.as_deref(),
            &updated_at,
        )?;
        let session = self
            .state
            .sessions
            .get_mut(session_id)
            .expect("session disappeared after its worker connection was saved");
        session.state = SessionState::Running;
        if native_session_id.is_some() {
            session.native_session_id = native_session_id;
        }
        session.updated_at = updated_at;
        session.last_error = None;
        Ok(())
    }

    fn install_worker_payload(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<(targets::TargetLocator, String)> {
        // Worker/profile installation is independent of repository cloning.
        let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
        let (backend, worker_root) = self.worker_placement(session_id)?;
        self.prepare_worker_files(session_id, &backend, &worker_root, syncing)?;
        install_attached_resources(&self.state, session_id, &backend, &worker_root, syncing)?;
        Ok((backend, worker_root))
    }

    async fn connect_and_start_worker(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
        backend: &targets::TargetLocator,
        worker_root: &str,
        initialize_workspace: bool,
    ) -> Result<Option<String>> {
        crate::worker_lifecycle::run(session_id, "connect and start worker", executor, async {
            let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
            if initialize_workspace {
                install_inherited_git_settings(executor, backend, session_id)?;
                self.initialize_network_workspaces(session_id, backend, syncing)?;
            }
            let session = self
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?;
            let profile = self
                .config
                .profiles
                .get(&session.last_profile)
                .with_context(|| format!("unknown profile {}", session.last_profile))?;
            let readiness_stage = bridge_readiness_stage(profile);
            let reconnect = &targets::reconnect_plan(backend, session_id)?.commands[0];
            let readiness = async {
                let mut relay = {
                    let _starting = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
                    start_worker_durably(
                        &crate::worker_lifecycle::require(session_id)?,
                        self.state.sessions[session_id]
                            .target
                            .as_ref()
                            .context("worker start has no durable target")?,
                        executor,
                        backend,
                        worker_root,
                    )?;
                    connect_started_worker(reconnect, session_id, executor, backend, worker_root)
                        .await?
                };
                let native_session_id = wait_for_native_session_in_stage(
                    &mut relay,
                    executor,
                    readiness_stage,
                    profile.kind,
                )
                .await?;
                let owner = crate::worker_lifecycle::require(session_id)?;
                crate::database::finish_worker_restart(session_id, owner.operation_id())?;
                Ok(Some(native_session_id))
            }
            .await;
            match readiness {
                Ok(native_session_id) => Ok(native_session_id),
                Err(error) => {
                    // The diagnosis reads the worker's state, so it runs before the
                    // worker is stopped.
                    let error = worker_probe_diagnosis(executor, backend, worker_root, error);
                    Err(error)
                }
            }
        })
        .await
    }
}

const MAX_LAUNCH_DIAGNOSTIC_BYTES: usize = 64 * 1024;

const RETAINED_LAUNCH_DIAGNOSTICS: usize = 20;

/// Record a failed new-session launch consistently across every failure arm:
/// log the failure with the session id (so `logs/mj-*.log` names the session)
/// and save the local diagnostic file. Returns the underlying error chain
/// annotated with the diagnostic path, which the caller stores in `last_error`
/// so the reason travels to `mj sessions`, `mj wait`, and `mj events`.
pub(super) fn note_new_session_launch_failure(session_id: &str, error: &anyhow::Error) -> String {
    note_new_session_launch_failure_in(&data_dir().join("diagnostics"), session_id, error)
}

fn note_new_session_launch_failure_in(
    directory: &Path,
    session_id: &str,
    error: &anyhow::Error,
) -> String {
    let original = format!("{error:#}");
    tracing::warn!(session_id, error = %original, "session launch failed");
    match persist_launch_failure_to(directory, session_id, &original) {
        Ok(path) => format!("{original}; full diagnostic saved to {}", path.display()),
        Err(save_error) => {
            format!("{original}; saving the local diagnostic failed: {save_error:#}")
        }
    }
}

fn persist_launch_failure_to(directory: &Path, session_id: &str, detail: &str) -> Result<PathBuf> {
    mj_core::config::validate_id("session", session_id)?;
    std::fs::create_dir_all(directory).with_context(|| {
        format!(
            "create launch diagnostics directory {}",
            directory.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join(format!("{session_id}-launch-error.txt"));
    let detail = bounded_launch_diagnostic(detail);
    let body = format!(
        "Mjolnir session launch failure\nsession: {session_id}\nat: {}\n\n{detail}\n",
        now()
    );
    atomic_write(&path, body.as_bytes())?;
    prune_launch_diagnostics(directory)?;
    Ok(path)
}

fn bounded_launch_diagnostic(detail: &str) -> String {
    if detail.len() <= MAX_LAUNCH_DIAGNOSTIC_BYTES {
        return detail.to_owned();
    }
    let mut head_end = MAX_LAUNCH_DIAGNOSTIC_BYTES / 4;
    while !detail.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let tail_bytes = MAX_LAUNCH_DIAGNOSTIC_BYTES - head_end;
    let mut tail_start = detail.len() - tail_bytes;
    while !detail.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n\n[... launch diagnostic truncated ...]\n\n{}",
        &detail[..head_end],
        &detail[tail_start..]
    )
}

fn prune_launch_diagnostics(directory: &Path) -> Result<()> {
    let mut diagnostics = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with("-launch-error.txt"))
        {
            continue;
        }
        diagnostics.push((entry.metadata()?.modified()?, entry.path()));
    }
    diagnostics.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in diagnostics.into_iter().skip(RETAINED_LAUNCH_DIAGNOSTICS) {
        std::fs::remove_file(&path)
            .with_context(|| format!("prune old launch diagnostic {}", path.display()))?;
    }
    Ok(())
}

fn apply_new_session_provisioning_result(
    state: &mut State,
    session_id: &str,
    result: Result<TargetLocator>,
) -> Result<()> {
    match result {
        Ok(locator) => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.target = Some(locator);
            // Provisioning has completed, but Running is reserved for a
            // successful worker handshake.
            record.state = SessionState::Disconnected;
            record.updated_at = now();
            record.last_error = None;
            Ok(())
        }
        Err(error) => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Error;
            record.target = None;
            record.updated_at = now();
            record.last_error = Some(format!("session provisioning failed: {error:#}"));
            Err(error)
        }
    }
}

/// Mark a new session's launch failed while it still names its target, so the
/// record is true while the rollback removes that target.
fn apply_failed_new_session_launch(state: &mut State, session_id: &str, original_error: &str) {
    let record = state.sessions.get_mut(session_id).unwrap();
    record.state = SessionState::StartupCleanup;
    record.updated_at = now();
    record.last_error = Some(format!("worker bootstrap failed: {original_error}"));
}

pub(super) fn apply_failed_new_session_rollback(
    state: &mut State,
    session_id: &str,
    original_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    match cleanup_error {
        None => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Error;
            record.target = None;
            record.updated_at = now();
            record.last_error = Some(format!("worker bootstrap failed: {original_error}"));
            anyhow::anyhow!("{original_error}; partial target removed and failed session retained")
        }
        Some(cleanup_error) => {
            let failure = format!(
                "{original_error}; cleanup of the failed session target failed: {cleanup_error}"
            );
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::StartupCleanup;
            record.updated_at = now();
            record.last_error = Some(format!("worker bootstrap failed: {failure}"));
            anyhow::anyhow!(failure)
        }
    }
}

pub(super) fn install_attached_resources(
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let targets::TargetLocator::AwsEc2 { .. } = backend else {
        return Ok(());
    };
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    if session.additional_mounts.is_empty() {
        return Ok(());
    }
    for resource in &session.additional_mounts {
        let install = targets::command_on_locator(
            backend,
            session_id,
            vec![
                format!("{worker_root}/hel"),
                "worker".into(),
                "install-resource".into(),
                "--destination".into(),
                resource.destination.to_string_lossy().into_owned(),
            ],
            "stream attached resource",
        )?;
        mj_checkpoint::resources::stream_resource(&resource.source, |stream| {
            execute_checked_with_stdin(executor, &install, stream).map(|_| ())
        })
        .with_context(|| format!("stream attached resource {}", resource.source.display()))?;
    }
    Ok(())
}

/// Run two independent target setup lanes at the same time and wait for both.
/// The first lane's failure wins deterministically when both fail, and neither
/// lane is abandoned while it may still own a transfer or subprocess.
pub(super) fn execute_concurrent_lanes<A: Send, B: Send>(
    first: impl FnOnce() -> Result<A> + Send,
    second: impl FnOnce() -> Result<B> + Send,
) -> Result<(A, B)> {
    std::thread::scope(|scope| {
        let owner = crate::worker_lifecycle::capture();
        let second = scope.spawn(move || match owner {
            Some(owner) => owner.scope_blocking(second),
            None => second(),
        });
        let first = first();
        let second = second.join().unwrap_or_else(|panic| {
            Err(anyhow::anyhow!(
                "concurrent target lane panicked: {}",
                targets::command_thread_panic_message(panic.as_ref())
            ))
        });
        match (first, second) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(first), Ok(second)) => Ok((first, second)),
        }
    })
}

fn execute_repository_setup(
    plan: &targets::CommandPlan,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    if plan.commands.is_empty() {
        return Ok(());
    }
    let _cloning = ProvisionStageGuard::new(executor, ProvisionStage::Cloning);
    plan.execute_concurrent(executor).map(|_| ())
}

/// Run a provisioning plan and discover the locator it produced, tearing the
/// target down again if anything after its creation fails.
///
/// Creation is the boundary that matters. A step that fails before the target
/// exists has left nothing behind; every failure after it — a later plan step
/// or locator discovery — owns a target no session record will point at.
#[cfg(test)]
fn provision_target(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    discover: impl FnOnce(&[CommandOutput]) -> Result<TargetLocator>,
) -> Result<TargetLocator> {
    let Some((creation, remainder)) = plan.split_at_target_creation() else {
        // Nothing this plan runs can leave a target behind.
        return discover(&plan.execute_concurrent(executor)?);
    };
    let mut outputs = creation.execute_concurrent(executor)?;
    let result = match remainder.execute_concurrent(executor) {
        Ok(rest) => {
            outputs.extend(rest);
            discover(&outputs)
        }
        Err(error) => Err(error),
    };
    result.map_err(|error| {
        match cleanup_failed_provision(target, session_id, outputs.first(), executor) {
            Some(note) => error.context(note),
            None => error,
        }
    })
}

/// Bring the target into existence and return the commands that populate its
/// repositories. The caller may overlap that remainder with worker/profile
/// installation once it has persisted the discovered locator.
#[cfg(test)]
fn provision_target_creation(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    discover: impl FnOnce(&[CommandOutput]) -> Result<TargetLocator>,
) -> Result<(TargetLocator, targets::CommandPlan)> {
    provision_target_creation_named(
        plan,
        target,
        session_id,
        executor,
        &targets::resource_name(session_id)?,
        discover,
    )
}

fn provision_target_creation_named(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    name: &str,
    discover: impl FnOnce(&[CommandOutput]) -> Result<TargetLocator>,
) -> Result<(TargetLocator, targets::CommandPlan)> {
    let Some((creation, remainder)) = plan.split_at_target_creation() else {
        // Nothing this plan runs can leave a target behind, so its commands
        // must still finish before the locator is usable.
        let outputs = crate::image_pull_gate::with_image_ready(target, executor, || {
            plan.execute_concurrent(executor)
        })?;
        return discover(&outputs).map(|locator| {
            (
                locator,
                targets::CommandPlan {
                    description: plan.description.clone(),
                    commands: Vec::new(),
                },
            )
        });
    };
    // Creating the container is what downloads the image on Docker, and the
    // probe just before it is what downloads it on Podman. Either way, a
    // background download of the same image must finish first.
    let outputs = crate::image_pull_gate::with_image_ready(target, executor, || {
        creation.execute_concurrent(executor)
    })?;
    discover(&outputs)
        .map(|locator| (locator, remainder))
        .map_err(|error| {
            match cleanup_failed_provision_named(
                target,
                session_id,
                outputs.first(),
                executor,
                name,
            ) {
                Some(note) => error.context(note),
                None => error,
            }
        })
}

/// Best-effort teardown of a target whose creation succeeded but whose
/// provisioning failed before a locator was recorded. Returns a note
/// describing what happened for inclusion in the session error.
///
/// The teardown is the session's own close plan, so a failed launch and an
/// ordinary close can never disagree about what removing a target means.
#[cfg(test)]
fn cleanup_failed_provision(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
    executor: &impl CommandExecutor,
) -> Option<String> {
    cleanup_failed_provision_named(
        target,
        session_id,
        create_output,
        executor,
        &targets::resource_name(session_id).ok()?,
    )
}

fn cleanup_failed_provision_named(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
    executor: &impl CommandExecutor,
    name: &str,
) -> Option<String> {
    let locator = provisioned_locator_named(target, session_id, create_output, name)?;

    let leak = format!(
        "the resource may still exist; find it via its dev.mj.session={session_id} label/tag"
    );
    let cleanup = if name == targets::resource_name(session_id).ok()? {
        targets::close_plan(&locator, session_id)
    } else {
        targets::retire_move_target_plan(&locator, session_id)
    };
    let plan = match cleanup {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not build provisioning cleanup plan"
            );
            return Some(format!("cleanup FAILED: {error:#}; {leak}"));
        }
    };
    let purpose = plan
        .commands
        .iter()
        .map(|command| command.purpose.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let Err(error) = plan.execute(executor) else {
        return Some(format!("cleanup succeeded: {purpose}"));
    };
    match targets::cleanup_target_is_confirmed_absent(&locator, session_id, executor) {
        Ok(true) => Some(format!("cleanup succeeded: {purpose}")),
        Ok(false) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "provisioning cleanup failed and the target may still exist"
            );
            Some(format!("cleanup FAILED ({purpose}): {error:#}; {leak}"))
        }
        Err(confirm_error) => {
            tracing::warn!(
                session_id,
                error = format!("{confirm_error:#}"),
                "could not confirm whether the failed provisioning target was removed"
            );
            Some(format!(
                "cleanup FAILED ({purpose}): {error:#}; checking whether it was removed also failed: {confirm_error:#}; {leak}"
            ))
        }
    }
}

/// The locator a provisioning plan's creating command brought into existence.
///
/// Every target but AWS is named before its plan runs; an EC2 instance
/// reports its own ID in the launch response.
fn provisioned_locator_named(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
    name: &str,
) -> Option<targets::TargetLocator> {
    let container_id = || Some(name.to_owned());

    Some(match target {
        // A bare project directory belongs to the user: provisioning creates
        // nothing that a failure could leak.
        targets::TargetTemplate::LocalBare => return None,
        targets::TargetTemplate::LocalPodman(container) => targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: container_id()?,
            workspace_storage: targets::podman_workspace_locator_named(container, name).ok()?,
        },
        targets::TargetTemplate::LocalDocker(_) => targets::TargetLocator::LocalDocker {
            borrowed_from: None,
            container_id: container_id()?,
        },
        targets::TargetTemplate::AppleContainer(_) => targets::TargetLocator::AppleContainer {
            borrowed_from: None,
            container_id: container_id()?,
        },
        targets::TargetTemplate::SshPodman { ssh, container } => {
            targets::TargetLocator::SshPodman {
                borrowed_from: None,
                ssh: ssh.clone(),
                container_id: container_id()?,
                workspace_storage: targets::podman_workspace_locator_named(container, name).ok()?,
            }
        }
        targets::TargetTemplate::SshDocker { ssh, .. } => targets::TargetLocator::SshDocker {
            borrowed_from: None,
            ssh: ssh.clone(),
            container_id: container_id()?,
        },
        targets::TargetTemplate::SshBare { ssh, .. } => targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: targets::workspace_for(target, session_id).ok()?,
            worker_id: None,
        },
        targets::TargetTemplate::AwsEc2(aws) => targets::TargetLocator::AwsEc2 {
            profile: aws.profile.clone(),
            region: aws.region.clone(),
            instance_id: serde_json::from_slice::<serde_json::Value>(&create_output?.stdout)
                .ok()?
                .pointer("/Instances/0/InstanceId")?
                .as_str()?
                .to_owned(),
            ssh: aws.ssh.clone(),
            workspace: targets::workspace_for(target, session_id).ok()?,
        },
    })
}

/// Attach read-only whatever the selected container overlay cannot hold, and
/// say so.
///
/// The filesystem is probed on the host that runs the container, because that
/// is where the overlay would be built. A probe that cannot answer leaves the
/// overlay alone: a failed probe is no evidence of an unsupported filesystem,
/// and refusing to provision over one would cost the user their session.
///
/// Apple's `container` engine already mounts every extra directory read-only,
/// and EC2 copies the directory instead of mounting it, so neither is probed.
pub(super) fn enforce_overlay_capable_mounts(
    target: &targets::TargetTemplate,
    mounts: &mut [targets::AdditionalMount],
    executor: &impl CommandExecutor,
) -> Vec<String> {
    let ssh = match target {
        targets::TargetTemplate::LocalPodman(_) | targets::TargetTemplate::LocalDocker(_) => None,
        targets::TargetTemplate::SshPodman { ssh, .. }
        | targets::TargetTemplate::SshDocker { ssh, .. } => Some(ssh),
        _ => return Vec::new(),
    };
    let overlaid = mounts
        .iter()
        .filter(|mount| mount.access == targets::MountAccess::Cow)
        .map(|mount| mount.source.clone())
        .collect::<Vec<_>>();
    if overlaid.is_empty() {
        return Vec::new();
    }
    // `stat` on this host cannot see what a VM-hosted Docker daemon mounts.
    if matches!(target, targets::TargetTemplate::LocalDocker(_)) {
        match targets::local_docker_vm_share(executor) {
            Ok(Some(reason)) => {
                return mounts
                    .iter_mut()
                    .filter(|mount| mount.access == targets::MountAccess::Cow)
                    .map(|mount| {
                        mount.access = mount.access.without_overlay();
                        format!(
                            "Mounted {} read-only: {reason}, which cannot back the \
                             copy-on-write overlay.",
                            mount.source.display()
                        )
                    })
                    .collect();
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                error = format!("{error:#}"),
                "could not identify the Docker daemon platform; probing this host's filesystems"
            ),
        }
    }
    let filesystems = match targets::probe_filesystem_types(ssh, &overlaid, executor) {
        Ok(filesystems) => filesystems,
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not probe attached-directory filesystems; preserving overlay mounts"
            );
            return vec![format!(
                "Could not read the filesystem under the attached directories, so they keep the \
                 copy-on-write overlay: {error:#}"
            )];
        }
    };
    let mut notices = Vec::new();
    for (mount, filesystem) in mounts
        .iter_mut()
        .filter(|mount| mount.access == targets::MountAccess::Cow)
        .zip(filesystems)
    {
        let Some(reason) = targets::overlay_unsupported_filesystem(&filesystem) else {
            continue;
        };
        mount.access = mount.access.without_overlay();
        notices.push(format!(
            "Mounted {} read-only: the overlay is unreliable on {filesystem} ({reason}).",
            mount.source.display()
        ));
    }
    notices
}

/// Reports every command an installer issues as one launch stage, so progress
/// stays accurate without threading the stage through each `CommandSpec`.
/// A command that already names a stage keeps it.
pub(super) struct StagedExecutor<'a, E: CommandExecutor> {
    inner: &'a E,
    stage: ProvisionStage,
    _guard: ProvisionStageGuard<'a, E>,
}

impl<'a, E: CommandExecutor> StagedExecutor<'a, E> {
    pub(crate) fn new(inner: &'a E, stage: ProvisionStage) -> Self {
        Self {
            inner,
            stage: stage.clone(),
            _guard: ProvisionStageGuard::new(inner, stage.clone()),
        }
    }

    fn staged(&self, command: &CommandSpec) -> CommandSpec {
        if command.stage.is_some() {
            return command.clone();
        }
        command.clone().stage(self.stage.clone())
    }
}

impl<E: CommandExecutor> CommandExecutor for StagedExecutor<'_, E> {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.inner.execute(&self.staged(command))
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        self.inner.stage_started(stage);
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        self.inner.stage_finished(stage);
    }

    fn notify_notice(&self, notice: &str) {
        self.inner.notify_notice(notice);
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        self.inner.execute_with_stdin(&self.staged(command), input)
    }
}

fn execute_checked_with_stdin(
    executor: &impl CommandExecutor,
    command: &CommandSpec,
    input: &mut (dyn std::io::Read + Send),
) -> Result<CommandOutput> {
    let output = executor.execute_with_stdin(command, input)?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

pub(super) fn install_inherited_git_settings(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
) -> Result<()> {
    let settings = if inherits_controller_git_settings(locator) {
        controller_git_settings()?
    } else {
        BTreeMap::new()
    };
    for command in inherited_git_setting_commands(locator, session_id, settings)? {
        execute_checked(executor, command)?;
    }
    Ok(())
}

fn inherits_controller_git_settings(locator: &targets::TargetLocator) -> bool {
    !matches!(
        locator,
        targets::TargetLocator::LocalBare { .. } | targets::TargetLocator::SshBare { .. }
    )
}

fn inherited_git_setting_commands(
    locator: &targets::TargetLocator,
    session_id: &str,
    settings: BTreeMap<String, String>,
) -> Result<Vec<CommandSpec>> {
    if matches!(locator, targets::TargetLocator::SshBare { .. }) {
        return Ok(Vec::new());
    }
    settings
        .into_iter()
        .map(|(key, value)| {
            let args = vec![
                "git".into(),
                "config".into(),
                "--global".into(),
                "--replace-all".into(),
                "--".into(),
                key.clone(),
                value,
            ];
            targets::command_on_locator(
                locator,
                session_id,
                args,
                format!("inherit Git setting {key}"),
            )
        })
        .collect()
}

fn controller_git_settings() -> Result<BTreeMap<String, String>> {
    let output = match Command::new("git")
        .args(["config", "--global", "--includes", "--null", "--list"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).context("read controller Git configuration"),
    };
    if !output.status.success() {
        bail!(
            "read controller Git configuration failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_inherited_git_settings(&output.stdout)
}

fn parse_inherited_git_settings(output: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut settings = BTreeMap::new();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = std::str::from_utf8(entry).context("decode controller Git configuration")?;
        let (key, value) = entry
            .split_once('\n')
            .with_context(|| format!("controller Git returned malformed entry {entry:?}"))?;
        let key = key.to_ascii_lowercase();
        if INHERITED_GIT_SETTINGS.contains(&key.as_str()) {
            settings.insert(key, value.to_owned());
        }
    }
    Ok(settings)
}

#[cfg(test)]
mod tests;
