//! Session provisioning, rollback, and worker-side Git bootstrap.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{TargetTemplate, atomic_write, data_dir};
use mj_core::state::{SessionState, State, TargetLocator};

use crate::targets::{
    self, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, ProvisionStage,
    ProvisionStageGuard,
};

use super::backend::{
    ContainerOverrides, backend_bundle, backend_locator, backend_target,
    configure_github_token_environment, controller_github_token, locator_after_provision,
    preflight_target, use_github_https_urls,
};
use super::git_cache;
use super::readiness::{connect_started_worker, wait_for_native_session_in_stage};
use super::worker_binary::{bridge_readiness_stage, start_worker, worker_probe_diagnosis};
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
/// container is expensive: the reviewer sidecar already caps its own
/// specialist lanes at three for the same reason. Measured on a local Podman
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
fn container_start_gate(locator: &targets::TargetLocator) -> Option<Arc<tokio::sync::Semaphore>> {
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
        let github_token = controller_github_token();
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
                self.connect_and_start_worker(session_id, executor, &backend, &worker_root, true)
                    .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(native_session_id) => {
                if let Err(error) = grant_commit() {
                    return Err(self.rollback_failed_new_session(session_id, error, executor)?);
                }
                self.mark_worker_connected(session_id, native_session_id)
            }
            Err(error) => Err(self.rollback_failed_new_session(session_id, error, executor)?),
        }
    }

    /// Start a child worker inside an already-provisioned parent target.
    /// Repository, target, and mount setup belong exclusively to the parent.
    pub async fn provision_subagent_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        // One retry, and only for a worker that provably never published a
        // control socket. Such a worker has no relay, no durable journal and
        // no harness, so starting another over the same root cannot duplicate
        // or corrupt anything. A spawn is issued by a model that cannot see
        // the target, so a transient start failure it could have retried by
        // hand is better retried here.
        let mut attempts: Vec<String> = Vec::new();
        let (result, placement) = loop {
            let attempt = self.attempt_subagent_start(session_id, executor).await;
            let (result, placement) = attempt;
            let Err(error) = &result else {
                break (result, placement);
            };
            if attempts.len() == 1 || !subagent_start_is_retryable(error) {
                if !attempts.is_empty() {
                    let combined = attempts
                        .iter()
                        .enumerate()
                        .map(|(index, attempt)| format!("attempt {}: {attempt}", index + 1))
                        .chain(std::iter::once(format!(
                            "attempt {}: {error:#}",
                            attempts.len() + 1
                        )))
                        .collect::<Vec<_>>()
                        .join("; ");
                    break (Err(anyhow::anyhow!("{combined}")), placement);
                }
                break (result, placement);
            }
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "sub-agent worker never started; retrying once"
            );
            attempts.push(format!("{error:#}"));
            // The next attempt reinstalls the worker files, so stop whatever
            // the failed one may have left behind first.
            if let Some((backend, worker_root)) = &placement
                && let Err(stop_error) =
                    super::worker_binary::stop_worker(executor, backend, worker_root)
            {
                tracing::debug!(
                    session_id,
                    error = format!("{stop_error:#}"),
                    "could not stop the worker of a retried sub-agent start"
                );
            }
        };
        match result {
            Ok(native_session_id) => self.mark_worker_connected(session_id, native_session_id),
            Err(error) => {
                // Without placement there is no worker to stop.
                if let Some((backend, worker_root)) = placement
                    && let Err(stop_error) =
                        super::worker_binary::stop_worker(executor, &backend, &worker_root)
                {
                    tracing::warn!(
                        session_id,
                        error = format!("{stop_error:#}"),
                        "failed sub-agent worker could not be stopped cleanly"
                    );
                }
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "sub-agent startup failed"
                );
                let record = self
                    .state
                    .sessions
                    .get_mut(session_id)
                    .context("failed sub-agent session disappeared")?;
                record.state = SessionState::Error;
                record.updated_at = super::now();
                record.last_error = Some(format!("sub-agent startup failed: {error:#}"));
                crate::database::save_lifecycle_session(record)?;
                Err(error)
            }
        }
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
                let prepared =
                    self.prepare_worker_files(session_id, &backend, &worker_root, syncing);
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
        executor: &impl CommandExecutor,
    ) -> Result<anyhow::Error> {
        self.rollback_failed_new_session_with(
            session_id,
            error,
            executor,
            // Rollback must remain possible after the foreground operation's
            // cancellation token has been set.
            &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
        )
    }

    fn rollback_failed_new_session_with(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
        executor: &impl CommandExecutor,
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
        let worktree_cleanup =
            self.cleanup_new_session_worktree_after_failure(session_id, executor);
        let cleanup_error = [target_cleanup, worktree_cleanup]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect::<Vec<_>>()
            .join("; ");
        if !cleanup_error.is_empty() {
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
            (!cleanup_error.is_empty()).then_some(cleanup_error),
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
                Err(self.rollback_failed_new_session(session_id, error, executor)?)
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
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if session.state != SessionState::Provisioning {
            bail!("session {session_id} is not provisioning");
        }
        let preparation = (|| {
            let selected = self
                .config
                .targets
                .get(&session.target_template_id)
                .context("target template disappeared before provisioning")?;
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
            super::worker_binary::preflight_worker_binary(template)?;
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
            // The image's user is a property of the host's copy of the image,
            // so it is read here, once per image per daemon, and handed to the
            // plan rather than stored on the session.
            let image_user = podman_image_user(&target, executor);
            let mut bundle = if session.project_directory.is_some() {
                None
            } else if failure_disposition == ProvisioningFailureDisposition::Preserve {
                Some(super::network_git::checkpoint_bundle(&session)?)
            } else {
                Some(backend_bundle(
                    self.config
                        .bundles
                        .get(&session.bundle_id)
                        .context("session bundle is missing")?,
                    executor,
                )?)
            };
            let container_github_token =
                github_token.filter(|_| configure_github_token_environment(&mut target));
            if container_github_token.is_some()
                && let Some(bundle) = bundle.as_mut()
            {
                use_github_https_urls(bundle);
            }
            preflight_target(template, executor)?;
            let prepared_cache = bundle.as_mut().and_then(|bundle| {
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
                &self.config.build_cache,
                &session,
                bundle.as_ref(),
                prepared_cache.as_ref(),
                &mut runtime_mounts,
                executor,
            );
            let provision = if let Some(project_directory) = &session.project_directory {
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
                        targets::provision_plan(
                            &target,
                            session_id,
                            bundle,
                            &runtime_mounts,
                            image_user,
                            session.container_workspace.as_deref(),
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
            let result =
                provision_target_creation(&provision, &target, session_id, executor, |outputs| {
                    locator_after_provision(
                        template,
                        &target,
                        session_id,
                        outputs.first(),
                        executor,
                    )
                })
                .map(|(locator, remainder)| (locator, remainder, bundle, build_cache));
            if result.is_err()
                && let Some(cache) = &prepared_cache
            {
                if let Some(locator) = provisioned_locator(&target, session_id, None) {
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
                return Err(self.rollback_failed_new_session(session_id, error, executor)?);
            }
            result => result,
        };
        if result.is_ok()
            && let Some(session) = self.state.sessions.get(session_id)
            && let Some(directory) = session
                .managed_worktree
                .as_ref()
                .map(|worktree| worktree.source_project_directory.clone())
                .or_else(|| session.project_directory.clone())
            && let Some(template) = self.config.targets.get(&session.target_template_id)
        {
            let host = match template {
                TargetTemplate::LocalBare => Some("local"),
                TargetTemplate::SshBare { ssh, .. } => Some(ssh.host.as_str()),
                _ => None,
            };
            if let Some(host) = host {
                self.state.remember_project_directory(host, &directory);
                crate::database::remember_project_directory(host, &directory)?;
            }
        }
        self.persist_session_state(session_id)?;
        result
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
                start_worker(executor, backend, worker_root)?;
                connect_started_worker(reconnect, session_id, executor, backend, worker_root)
                    .await?
            };
            let native_session_id =
                wait_for_native_session_in_stage(&mut relay, executor, readiness_stage).await?;
            Ok(Some(native_session_id))
        }
        .await;
        match readiness {
            Ok(native_session_id) => Ok(native_session_id),
            Err(error) => Err(worker_probe_diagnosis(
                executor,
                backend,
                worker_root,
                error,
            )),
        }
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
        "Hel session launch failure\nsession: {session_id}\nat: {}\n\n{detail}\n",
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
    record.state = SessionState::Error;
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
            record.state = SessionState::Error;
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
        let second = scope.spawn(second);
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
fn provision_target_creation(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
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
            match cleanup_failed_provision(target, session_id, outputs.first(), executor) {
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
fn cleanup_failed_provision(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
    executor: &impl CommandExecutor,
) -> Option<String> {
    let locator = provisioned_locator(target, session_id, create_output)?;
    let leak = format!(
        "the resource may still exist; find it via its dev.mj.session={session_id} label/tag"
    );
    let plan = match targets::close_plan(&locator, session_id) {
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
fn provisioned_locator(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
) -> Option<targets::TargetLocator> {
    let container_id = || targets::resource_name(session_id).ok();
    Some(match target {
        // A bare project directory belongs to the user: provisioning creates
        // nothing that a failure could leak.
        targets::TargetTemplate::LocalBare => return None,
        targets::TargetTemplate::LocalPodman(container) => targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: container_id()?,
            workspace_storage: targets::podman_workspace_locator(container, session_id).ok()?,
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
                workspace_storage: targets::podman_workspace_locator(container, session_id).ok()?,
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

/// The image users already probed, keyed by container host and image
/// reference. An image's
/// configured user does not change under a fixed reference, and reading it
/// costs a container start, so each daemon asks a host once.
static IMAGE_USERS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, targets::ImageUser>>> =
    std::sync::LazyLock::new(std::sync::Mutex::default);

/// The uid and gid a Podman session container maps onto the host user.
///
/// Only Podman is asked: Docker and Apple's `container` engine are left with
/// their own defaults. A probe that cannot answer is not a launch failure —
/// the container falls back to plain `--userns=keep-id`, which maps the
/// image's default user, and the user is told what happened.
pub(super) fn podman_image_user(
    target: &targets::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Option<targets::ImageUser> {
    let (ssh, container) = match target {
        targets::TargetTemplate::LocalPodman(container) => (None, container),
        targets::TargetTemplate::SshPodman { ssh, container } => (Some(ssh), container),
        _ => return None,
    };
    let image = container.image.as_str();
    let key = format!(
        "{}|{image}",
        ssh.map_or("local", |ssh| ssh.destination.as_str())
    );
    if let Some(cached) = IMAGE_USERS.lock().expect("image user cache").get(&key) {
        return Some(*cached);
    }
    // The probe starts a container, so on Podman this is where a missing image
    // is actually downloaded. Wait for the daemon's own download instead of
    // starting a second one.
    match crate::image_pull_gate::with_image_ready(target, executor, || {
        targets::probe_image_user(ssh, container, executor)
    }) {
        Ok(user) => {
            IMAGE_USERS
                .lock()
                .expect("image user cache")
                .insert(key, user);
            Some(user)
        }
        Err(error) => {
            tracing::warn!(
                image,
                error = format!("{error:#}"),
                "could not read the container image user; keeping Podman's default user mapping"
            );
            executor.notify_notice(&format!(
                "Could not read the user of image {image}, so the container runs with Podman's \
                 default user mapping and may not be able to write to an attached directory: \
                 {error:#}"
            ));
            None
        }
    }
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
            stage,
            _guard: ProvisionStageGuard::new(inner, stage),
        }
    }

    fn staged(&self, command: &CommandSpec) -> CommandSpec {
        if command.stage.is_some() {
            return command.clone();
        }
        command.clone().stage(self.stage)
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
            targets::command_on_locator(
                locator,
                session_id,
                vec![
                    "git".into(),
                    "config".into(),
                    "--global".into(),
                    "--replace-all".into(),
                    "--".into(),
                    key.clone(),
                    value,
                ],
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
