//! Session provisioning, rollback, and worker-side Git bootstrap.

use mj_core::targets::SshTrafficClass;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};

use mj_checkpoint::archive::ArchiveManifest;
use mj_core::config::{TargetTemplate, atomic_write, data_dir};
use mj_core::state::{SessionState, State, TargetLocator};
use mj_session_runtime::host_git_config::controller_git_settings;

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
use crate::launch_timing::LaunchPhaseTimer;

#[derive(Clone, Copy)]
pub(super) enum RepositorySeed<'a> {
    ConfiguredBundle,
    Restore(Option<&'a ArchiveManifest>),
}

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
        | targets::TargetLocator::SshDocker { container_id, .. }
        | targets::TargetLocator::SshAppleContainer { container_id, .. }
        | targets::TargetLocator::SshNitro { container_id, .. } => container_id.clone(),
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

/// Take a start slot of the container's gate. A child that has to queue shows
/// the wait as its own stage, from the moment it begins waiting until it is
/// admitted, so the queue is visible and is not mistaken for a stalled stage.
async fn admit_container_start<'a>(
    gate: &'a tokio::sync::Semaphore,
    executor: &impl CommandExecutor,
) -> Option<tokio::sync::SemaphorePermit<'a>> {
    match gate.try_acquire() {
        Ok(permit) => Some(permit),
        Err(tokio::sync::TryAcquireError::Closed) => None,
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            let _waiting = ProvisionStageGuard::new(executor, ProvisionStage::WaitingForStartSlot);
            gate.acquire().await.ok()
        }
    }
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

struct RepositorySetup {
    plan: targets::CommandPlan,
    managed_checkout: Option<super::worktree::PreparedManagedCheckout>,
}

struct PreparedWorkerPayload {
    backend: targets::TargetLocator,
    worker_root: String,
    launch: Option<mj_core::worker_launch::WorkerLaunchConfig>,
}

impl PreparedWorkerPayload {
    fn prepare_harness(
        self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<(targets::TargetLocator, String)> {
        let _timing = LaunchPhaseTimer::new(session_id, "prepare harness after checkout");
        if let Some(launch) = self.launch {
            let syncing = StagedExecutor::new(executor, ProvisionStage::Syncing);
            super::worker_binary::prepare_installed_managed_harness(
                &syncing,
                &self.backend,
                &self.worker_root,
                &launch,
            )?;
        }
        Ok((self.backend, self.worker_root))
    }
}

impl Controller {
    /// Run one create to its end. `recovering` is set when a restarted
    /// daemon finishes a create its predecessor's shutdown interrupted: the
    /// attempt's leftovers are retired first and the record returns to its
    /// registered form.
    ///
    /// `grant_commit` is called once, after the worker is up and before the
    /// session is written `Running`; a refusal is a cancel that won, which
    /// rolls the create back. `interrupted` says whether daemon shutdown
    /// stopped the create, in which case nothing is rolled back and the next
    /// daemon finishes it from the durable create intent.
    pub async fn run_create(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        remember_launch_choices: bool,
        recovering: bool,
        grant_commit: impl FnOnce() -> Result<()>,
        interrupted: &(dyn Fn() -> bool + Sync),
    ) -> Result<()> {
        crate::worker_lifecycle::run(session_id, "run create", executor, async {
            let owns_environment = self
                .state
                .owns_environment(session_id)
                .context("create has no environment binding")?;
            crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
            let child = self.state.subagents.contains_key(session_id);
            let mut previous_failure = None;
            let start = async {
                if recovering {
                    self.reset_interrupted_create(session_id, owns_environment)?;
                }
                loop {
                    let attempt = if owns_environment {
                        self.prepare_owned_create(
                            session_id,
                            executor,
                            child,
                            remember_launch_choices,
                        )
                        .await
                    } else {
                        self.prepare_borrowed_create(session_id, executor)
                    };
                    let result = match attempt {
                        Ok((backend, worker_root)) => {
                            let gate = (!owns_environment)
                                .then(|| container_start_gate(&backend))
                                .flatten();
                            let _admitted = match &gate {
                                Some(gate) => admit_container_start(gate, executor).await,
                                None => None,
                            };
                            self.connect_and_start_worker(
                                session_id,
                                executor,
                                &backend,
                                &worker_root,
                                owns_environment,
                            )
                            .await
                            .map_err(|error| {
                                if child {
                                    super::subagent_park::explain_process_exhaustion(
                                        error, &backend, session_id,
                                    )
                                } else {
                                    error
                                }
                            })
                        }
                        Err(error) => Err(error),
                    };
                    match result {
                        Err(error)
                            if !owns_environment
                                && previous_failure.is_none()
                                && !interrupted()
                                && subagent_start_is_retryable(&error) =>
                        {
                            previous_failure = Some(format!("{error:#}"));
                            self.record_child_startup_failure(session_id, &error, true)?;
                        }
                        result => break result,
                    }
                }
            }
            .await;
            let committed = start.and_then(|native_session_id| {
                grant_commit()?;
                Ok(native_session_id)
            });
            match committed {
                Ok(native_session_id) => self.mark_worker_connected(session_id, native_session_id),
                Err(error) if child && !interrupted() => {
                    let error = match previous_failure {
                        Some(first) => anyhow::anyhow!("attempt 1: {first}; attempt 2: {error:#}"),
                        None => error,
                    };
                    if let Err(cleanup) =
                        self.record_child_startup_failure(session_id, &error, false)
                    {
                        return Err(
                            error.context(format!("startup cleanup remains pending: {cleanup:#}"))
                        );
                    }
                    Err(error)
                }
                Err(error) if child => {
                    Err(error.context("the Mjolnir daemon stopped during this launch"))
                }
                Err(error) => {
                    Err(self.rollback_unless_interrupted(session_id, error, interrupted)?)
                }
            }
        })
        .await
    }

    async fn prepare_owned_create(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        child: bool,
        remember_launch_choices: bool,
    ) -> Result<(targets::TargetLocator, String)> {
        let _total = LaunchPhaseTimer::new(session_id, "provision session");
        let credentials = LaunchPhaseTimer::new(session_id, "resolve GitHub credentials");
        let github_token = self
            .github_token_for_session(session_id)
            .await
            .context("resolve GitHub credentials for session")?;
        drop(credentials);
        let repositories = self
            .provision_session_target_with_failure_disposition(
                session_id,
                executor,
                github_token.as_deref(),
                ProvisioningFailureDisposition::Preserve,
                RepositorySeed::ConfiguredBundle,
                remember_launch_choices,
                &|| false,
            )
            .await?;
        let payload = if child {
            // Report paths under the checkout must wait for the clone to finish.
            execute_repository_setup(session_id, &repositories, executor)?;
            let (backend, _) = self.worker_placement(session_id)?;
            self.prepare_subagent_report_dir(session_id, &backend, executor)?;
            self.install_worker_payload(session_id, executor)?
        } else {
            let ((), payload) = execute_concurrent_lanes(
                || execute_repository_setup(session_id, &repositories, executor),
                || self.install_worker_payload(session_id, executor),
            )?;
            payload
        };
        payload.prepare_harness(session_id, executor)
    }

    fn prepare_borrowed_create(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<(targets::TargetLocator, String)> {
        let (backend, worker_root) = self.worker_placement(session_id)?;
        targets::verify_locator(&backend, session_id)?;
        let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
        self.prepare_subagent_report_dir(session_id, &backend, syncing)?;
        self.prepare_worker_files(
            session_id,
            &backend,
            &worker_root,
            syncing,
            super::worker_binary::LaunchHistory::Stored,
        )?;
        mj_core::test_hooks::reach_test_hook("subagent_create_before_worker_start")?;
        Ok((backend, worker_root))
    }

    fn record_child_startup_failure(
        &mut self,
        session_id: &str,
        error: &anyhow::Error,
        retry: bool,
    ) -> Result<()> {
        let diagnostic = note_new_session_launch_failure(session_id, error);
        let previous = self.state.sessions[session_id].clone();
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::StartupCleanup;
        record.updated_at = now();
        record.last_error = Some(format!("sub-agent startup failed: {diagnostic}"));
        self.persist_session_transition_or_restore(
            session_id,
            &previous,
            "record failed startup before teardown",
        )?;
        self.finish_failed_startup_controlled(
            session_id,
            &super::failed_launch_cleanup_executor(),
            retry,
        )
    }

    /// Return an interrupted create's record to its registered form after
    /// retiring what the attempt built.
    ///
    /// Retire-then-provision is used because several provisioning steps are
    /// not safe to re-enter (a persisted managed worktree is never planned
    /// again, `podman run --name` fails when the container exists, a clone
    /// refuses a populated workspace). Retirement is idempotent, so an attempt
    /// that is interrupted again is reset again by the next daemon.
    fn reset_interrupted_create(&mut self, session_id: &str, owns_environment: bool) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        ensure!(
            // The states an unfinished create's record can be in.
            crate::database::holds_create_intent(session.state),
            "session {session_id} is {}, not an unfinished create",
            session.state.as_str()
        );
        if owns_environment {
            self.retire_failed_create_environment(
                session_id,
                &super::failed_launch_cleanup_executor(),
            )?;
        } else if let Some(locator) = &session.target {
            let backend = backend_locator(locator, &session, &self.config)?;
            let root = targets::worker_root(&backend, session_id)?;
            super::worker_binary::stop_worker(
                &crate::worker_lifecycle::require(session_id)?,
                &super::failed_launch_cleanup_executor(),
                &backend,
                &root,
            )
            .context("stop the interrupted attempt's worker")?;
        }
        let record = self.state.sessions.get_mut(session_id).unwrap();
        if owns_environment {
            // With no environment there is no checkout to remove: planning
            // only records `managed_worktree`, and `execute_repository_setup`
            // creates the checkout after the environment is reserved.
            // Forgetting the plan is therefore enough when the stop came
            // before the reservation.
            if let Some(managed) = record.managed_worktree.take() {
                record.project_directory = Some(managed.source_project_directory.clone());
            }
            record.target = None;
        }
        record.state = SessionState::Provisioning;
        record.last_error = None;
        record.updated_at = now();
        crate::database::reset_session_for_create_retry(record)
    }

    fn retire_failed_create_environment(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        // A target reservation can be durable before the session placement is published.
        if let Some(environment) = crate::database::session_environment(session_id)? {
            ensure!(
                environment.owner_session_id == session_id,
                "cannot retire a borrowed environment"
            );
            super::environment::retire_owned_environment(
                environment,
                &self.config,
                executor,
                super::BranchDisposition::Delete,
                super::CheckoutDisposition::Remove,
            )
            .context("retire the failed create environment")?;
        }
        Ok(())
    }

    /// Finish a failed startup without reopening its relay or replaying work.
    pub fn cleanup_failed_startup_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.finish_failed_startup_controlled(session_id, executor, false)
    }

    fn finish_failed_startup_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
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
                let owns_environment = self
                    .state
                    .owns_environment(session_id)
                    .context("failed startup has no environment binding")?;
                let outcome = (|| -> Result<()> {
                    if owns_environment {
                        self.retire_failed_create_environment(session_id, executor)?;
                    } else if let Some(locator) = &previous.target {
                        let backend = backend_locator(locator, &previous, &self.config)?;
                        targets::verify_locator(&backend, session_id)?;
                        let root = targets::worker_root(&backend, session_id)?;
                        super::worker_binary::stop_worker(
                            &crate::worker_lifecycle::require(session_id)?,
                            executor,
                            &backend,
                            &root,
                        )?;
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
                        if owns_environment {
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

    /// A create the daemon's shutdown interrupted before it committed keeps
    /// its record and everything it built: the durable create intent makes the
    /// next daemon retire that and provision again. Any other failure rolls
    /// back. The decision is read at the moment of failure from the one owner
    /// of the fact, the create's lifecycle control.
    fn rollback_unless_interrupted(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
        interrupted: &(dyn Fn() -> bool + Sync),
    ) -> Result<anyhow::Error> {
        if interrupted() {
            tracing::info!(
                session_id,
                error = format!("{error:#}"),
                "session creation interrupted by daemon shutdown; the next daemon finishes it"
            );
            return Ok(error.context("the daemon stopped during session creation"));
        }
        self.rollback_failed_new_session(session_id, error)
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
        target_cleanup_executor: &(impl CommandExecutor + Sync),
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
        let recorded = if error.is::<super::readiness::WorkerStartupFailure>()
            || error.is::<super::readiness::HarnessPreparationFailure>()
            || error.is::<mj_core::relay::WorkerHarnessStartupFailure>()
        {
            launch_failure_text(
                &self.config,
                &session.last_profile,
                &original,
                "retry the launch",
            )
        } else {
            original
        };
        apply_failed_new_session_launch(&mut self.state, session_id, &recorded);
        self.persist_session_transition_or_restore(
            session_id,
            &session,
            "record the failed launch before removing its target",
        )
        .map_err(|persist_error| {
            persist_error.context(format!(
                "{recorded}; its target was left in place because the failure could not be recorded"
            ))
        })?;
        let cleanup_error = self
            .retire_failed_create_environment(session_id, target_cleanup_executor)
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
            &recorded,
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
        repository_seed: RepositorySeed<'_>,
    ) -> Result<()> {
        let repositories = self
            .provision_session_target_with_failure_disposition(
                session_id,
                executor,
                github_token,
                failure_disposition,
                repository_seed,
                // Resume is the only production caller; it reopens a
                // directory the user already chose, so it keeps the history.
                true,
                // Resume is cancelled and rolled back to its archived record.
                &|| false,
            )
            .await?;
        match execute_repository_setup(session_id, &repositories, executor) {
            Ok(()) => Ok(()),
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                Err(self.rollback_failed_new_session(session_id, error)?)
            }
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn provision_session_target_with_failure_disposition(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        github_token: Option<&str>,
        failure_disposition: ProvisioningFailureDisposition,
        repository_seed: RepositorySeed<'_>,
        remember_launch_choices: bool,
        interrupted: &(dyn Fn() -> bool + Sync),
    ) -> Result<RepositorySetup> {
        crate::worker_lifecycle::run(session_id, "provision session target with failure disposition", executor, async {
        let _target_timing = LaunchPhaseTimer::new(session_id, "plan checkout and provision target");
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
            // Before the runtime is recorded, so a container never starts
            // without an entry whose secret is missing.
            selected.ensure_ready(&session.target_template_id)?;
            self.config
                .profiles
                .get(&session.last_profile)
                .context("harness profile disappeared before provisioning")?
                .ensure_ready(&session.last_profile)?;
            let mut runtime = mj_core::state::TargetRuntimeSettings::from(selected);
            if let Some(recorded) = &session.target_runtime {
                runtime.tool_cache = recorded.tool_cache.clone();
                runtime.container_settings = recorded.container_settings.clone();
                ensure!(
                    **recorded == runtime,
                    "target access settings changed before provisioning; retry with the selected target"
                );
            } else {
                self.state
                    .sessions
                    .get_mut(session_id)
                    .unwrap()
                    .target_runtime = Some(runtime.into());
                self.persist_session_state(session_id)?;
            }
            let resolved_template = super::backend::session_target(&self.config, &session.bundle_id, &session.target_template_id, executor)?;
            let template = &resolved_template;
            let profile = self
                .config
                .profiles
                .get(&session.last_profile)
                .context("harness profile disappeared during provisioning")?;
            {
                let _timing = LaunchPhaseTimer::new(session_id, "preflight worker binary");
                super::worker_binary::preflight_worker_binary(template, executor)?;
            }
            {
                let _timing = LaunchPhaseTimer::new(session_id, "preflight harness");
                super::worker_binary::preflight_harness(template, profile, executor)?;
            }
            let _timing = LaunchPhaseTimer::new(session_id, "plan managed checkout");
            let managed_checkout = self.plan_managed_raw_worktree(session_id, executor)?;
            Ok(managed_checkout)
        })();
        let managed_checkout = match preparation {
            Ok(created) => created,
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                return Err(self.rollback_unless_interrupted(session_id, error, interrupted)?);
            }
            Err(error) => return Err(error),
        };
        let session = self
            .state
            .sessions
            .get(session_id)
            .expect("session retained after managed worktree preparation")
            .clone();
        let project_directory = self.state.checkout(session_id)?.project_directory().map(Path::to_path_buf);
        // Keep planning, preflight, creation, and locator discovery in one
        // result so the caller's failure disposition applies to every error.
        let result = (|| {
            let resolved_template = super::backend::session_target(&self.config, &session.bundle_id, &session.target_template_id, executor)?;
            let template = &resolved_template;

            let mut target = backend_target(
                template,
                session.resource_allocation.as_ref(),
                ContainerOverrides::for_session(&session),
            )?;
            let nitro_cache = nitro_build_cache(template, session.project_bundle(&self.config))?;
            let mut runtime_mounts = session.additional_mounts.clone();
            // The mounts this container runs with, not the ones the session
            // stores: a forced downgrade belongs to the host the container
            // lands on, so it is decided here every time and never written
            // over the user's choice.
            for notice in enforce_overlay_capable_mounts(&target, &mut runtime_mounts, executor) {
                executor.notify_notice(&notice);
            }
            for notice in drop_nitro_host_directories(&target, &mut runtime_mounts) {
                executor.notify_notice(&notice);
            }
            if matches!(nitro_cache, NitroBuildCache::GuestLocal) {
                executor.notify_notice("This Nitro sandbox keeps its mbx build cache inside the VM, so its first build starts cold. Set `build_cache.remote` on the host to share CI's cache (see Project environments).");
            }
            let mut bundle = if project_directory.is_some() {
                None
            } else if let RepositorySeed::Restore(move_manifest) = repository_seed {
                // Move's verified snapshot owns the checkout being restored.
                // The session's ordinary checkpoint may predate this checkout.
                let history_cutoff_origin_delta =
                    super::backend::session_history_cutoff(&session)?.is_some()
                        && match move_manifest {
                            Some(manifest) => super::network_git::manifest_has_history_cutoff_origin_delta(manifest),
                            None => super::network_git::checkpoint_has_history_cutoff_origin_delta(&session)?,
                        };
                if history_cutoff_origin_delta {
                    Some(backend_session_bundle(&session, &self.config, executor)?)
                } else if let Some(manifest) = move_manifest {
                    Some(super::network_git::bundle_from_manifest_for_session(
                        manifest, Some(&session), Some(&self.config),
                    )?)
                } else {
                    Some(super::network_git::checkpoint_bundle(&session, &self.config)?)
                }
            } else {
                Some(backend_session_bundle(&session, &self.config, executor)?)
            };
            if let Some(bundle) = bundle.as_mut() {
                super::backend::apply_session_history_cutoff(
                    &session,
                    &self.config,
                    bundle,
                )?;
            }
            let container_github_token =
                github_token.filter(|_| {
                    !session.no_github_credentials && configure_github_token_environment(&mut target)
                });
            if (container_github_token.is_some() || session.no_github_credentials)
                && let Some(bundle) = bundle.as_mut()
            {
                use_github_https_urls(bundle);
            }
            preflight_target(template, executor, TargetCheck::Launch)?;
            for notice in enforce_cgroup_capable_limits(&mut target, executor) {
                executor.notify_notice(&notice);
            }
            let resource_name = crate::database::load_move_operation(session_id)?
                .filter(|op| {
                    !op.in_place
                        && matches!(
                            op.phase,
                            mj_core::state::MovePhase::Restoring
                                | mj_core::state::MovePhase::DestinationStarting
                        )
                })
                .map(|op| targets::move_resource_name(session_id, &op.operation_id))
                .unwrap_or_else(|| targets::resource_name(session_id))?;
            let moving_workspace = resource_name != targets::resource_name(session_id)?;
            // Reserve physical ownership before creation, including a target
            // whose creation or locator discovery later fails. Rollback uses
            // the same owner and writer inventory as ordinary teardown.
            let reserved_target = super::backend::locator_after_provision_named(
                template,&target,session_id,None,executor,&resource_name)?;
            self.state.sessions.get_mut(session_id).context("session disappeared before target reservation")?.target = Some(Box::new(reserved_target));
            self.persist_session_state(session_id)?;

            let tool_cache = super::tool_cache::prepare(
                &self.config,
                &session.target_template_id,
                &mut runtime_mounts,
                executor,
            );
            // Record placement before creating fixed container mounts. A crash
            // after creation must not leave a mount whose location was lost.
            let runtime = self.state.sessions.get_mut(session_id)
                .expect("session retained during provisioning")
                .target_runtime.as_mut().context("selected runtime disappeared")?;
            std::sync::Arc::make_mut(runtime).tool_cache = tool_cache;

            self.persist_session_state(session_id)?;
            let prepared_cache = bundle
                .as_mut()
                .filter(|_| !moving_workspace)
                .map(|bundle| {
                    let exact = session.checkout.as_ref()
                        .filter(|checkout| !checkout.no_later_history)
                        .map(|checkout| {
                            super::backend::session_checkout_destination(
                                &session, &self.config, &checkout.repository_id,
                            )
                            .map(|destination| (destination.to_string_lossy(), checkout.commit.as_str()))
                        })
                        .transpose()?;
                    git_cache::prepare(
                        &target,
                        session_id,
                        bundle,
                        &mut runtime_mounts,
                        github_token,
                        exact.as_ref().map(|(destination, commit)| (destination.as_ref(), *commit)),
                        executor,
                    )
                })
                .transpose()?
                .flatten();
            // Mounts are fixed when the container is created, so the build
            // cache is decided here, before the provisioning plan is built.
            let build_cache = super::mbx::prepare(
                &target,
                &session,
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
                        if matches!(target, targets::TargetTemplate::LocalBare) {
                            let (_, worker_root) = self.worker_placement(session_id)?;
                            return targets::provision_local_bundle_plan(bundle, &worker_root, github_token);
                        }
                        targets::provision_plan_named(
                            &target,
                            session_id,
                            bundle,
                            &runtime_mounts,
                            session.container_workspace.as_deref(),
                            &resource_name,
                            targets::ProvisionOptions {
                                github_token: session.no_github_credentials.then_some(github_token).flatten(),
                                nitro_mbx_remote: match &nitro_cache {
                                    NitroBuildCache::Remote(remote) => Some(remote),
                                    _ => None,
                                },
                            },
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
            // Values that must stay off the command line: the GitHub token,
            // and, for a sandbox whose engine runs under `sudo`, every
            // environment value that came from a reference.
            let mut secrets = match template {
                TargetTemplate::SshNitro { container, .. } => {
                    super::backend::container_environment(
                        container,
                        ContainerOverrides::for_session(&session),
                        true,
                    )
                    .secrets
                }
                _ => Vec::new(),
            };
            if let Some(token) = container_github_token {
                secrets.retain(|(name, _)| name != "GH_TOKEN");
                secrets.push(("GH_TOKEN".to_owned(), token.to_owned()));
            }
            if !secrets.is_empty()
                && let Err(error) = provision.provide_target_environment_secrets(&target, &secrets)
            {
                if let Some(cache) = &prepared_cache {
                    let _ = cache.cleanup(executor);
                }
                return Err(error);
            }

            let started = Instant::now();
            let result = provision_target_creation_plan(
                &provision,
                &target,
                executor,
                |_| None,
                |outputs| {
                    // The engine's run command is one step of the creation plan
                    // (Git installation follows it); its warning names this VM.
                    if matches!(nitro_cache, NitroBuildCache::Remote(_)) {
                        let warning = format!(
                            "warning: {resource_name} starts without the remote mbx cache: "
                        );
                        if let Some(reason) = outputs.iter().find_map(|output| {
                            String::from_utf8_lossy(&output.stderr)
                                .lines()
                                .find_map(|line| line.strip_prefix(&warning).map(str::to_owned))
                        }) {
                            executor.notify_notice(&format!(
                                "This Nitro sandbox started without the shared mbx build cache: {reason}. Builds use the cache inside the VM."
                            ));
                        }
                    }
                    let locator = super::backend::locator_after_provision_named(
                        template,
                        &target,
                        session_id,
                        outputs.first(),
                        executor,
                        &resource_name,
                    )?;
                    // Creation owns HOME readiness before checkout and worker
                    // staging split into independent background operations.
                    targets::initialize_container_home(
                        &backend_locator(&locator, &session, &self.config)?, executor,
                    )?;
                    Ok(locator)
                },
            )
            .map(|(locator, remainder)| (locator, remainder, bundle, build_cache));
            tracing::debug!(
                session_id,
                elapsed_ms = started.elapsed().as_millis(),
                "provisioning plan execution completed"
            );
            result
        })();
        let result = match result {
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Preserve => {
                Err(error)
            }
            // Every failure from here, before or after the target was
            // reserved, goes through the one rollback: failure on record
            // with the target still named, retirement on an executor the
            // create's cancellation cannot stop, and only then `Error`.
            Err(error) => {
                return Err(self.rollback_unless_interrupted(session_id, error, interrupted)?);
            }
            Ok((locator, remainder, bundle, build_cache)) => {
                let resolved_template = super::backend::session_target(&self.config, &session.bundle_id, &session.target_template_id, executor)?;
                let image = resolved_template.container().map(|container| container.image.as_str());
                let container_settings = backend_locator(&locator, &self.state.sessions[session_id], &self.config)?
                    .container_engine()
                    .map(|_| match image {
                        Some(image) => mj_core::state::ContainerSettings::applied(&session, image),
                        None => mj_core::state::ContainerSettings::requested(&session),
                    });
                apply_new_session_provisioning_result(&mut self.state, session_id, Ok(locator))?;
                // Holds a create after its target exists, so a test can restart
                // the daemon in the middle of it.
                mj_core::test_hooks::reach_test_hook("create_after_target_provisioned")?;
                if mj_core::test_hooks::test_hooks_enabled()
                    && std::env::var("MJ_TEST_HOOK").as_deref() == Ok("create_after_target_provisioned")
                    && let Some(directory) = std::env::var_os("MJ_TEST_HOOK_DIR")
                {
                    let marker = PathBuf::from(directory).join("create_after_target_provisioned.fail");
                    match std::fs::remove_file(marker) {
                        Ok(()) => {
                            let failure = anyhow::anyhow!("test requested a one-shot launch failure after target provisioning");
                            return Err(failure);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error).context("read the launch failure test marker"),
                    }
                }
                let runtime = self.state.sessions.get_mut(session_id)
                    .expect("session retained after target creation")
                    .target_runtime.as_mut().context("selected runtime disappeared")?;
                std::sync::Arc::make_mut(runtime).container_settings = container_settings;
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
                        github_token,
                    )
                } else {
                    Ok(remainder)
                }
            }
        };
        let result = match result {
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                return Err(self.rollback_unless_interrupted(session_id, error, interrupted)?);
            }
            result => result,
        };
        if result.is_ok() && remember_launch_choices {
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
        result.map(|plan| RepositorySetup { plan, managed_checkout })

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
    ) -> Result<PreparedWorkerPayload> {
        let _timing = LaunchPhaseTimer::new(session_id, "prepare worker payload");
        // Worker/profile installation is independent of repository cloning.
        let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let (launch, _) = self.stage_worker_files_for(
            session_id,
            &backend,
            &worker_root,
            syncing,
            super::worker_binary::WorkerFilePreparation::Install,
            super::worker_binary::LaunchHistory::Stored,
        )?;
        install_attached_resources(&self.state, session_id, &backend, &worker_root, syncing)?;
        Ok(PreparedWorkerPayload {
            backend,
            worker_root,
            launch,
        })
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
                let _timing = LaunchPhaseTimer::new(session_id, "initialize workspace");
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
                let _timing =
                    LaunchPhaseTimer::new(session_id, "start worker and await native session");
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
                        &self.config.github,
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
            record.target = Some(Box::new(locator));
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

/// Add credential guidance when the ACP startup response identified an auth
/// failure. The original diagnostic is retained, including any log path.
pub(super) fn launch_failure_text(
    config: &mj_core::config::Config,
    profile_id: &str,
    detail: &str,
    retry: &str,
) -> String {
    use mj_core::acp::PROMPT_AUTH_REQUIRED_MARKER;
    use mj_core::config::AuthScheme;
    use mj_core::credentials::{ClaudeEnvironmentAuth, claude_environment_auth};

    let Some(profile) = config.profiles.get(profile_id) else {
        return detail.to_owned();
    };
    // The worker puts the failure before the bridge's stderr tail.
    let cause = detail.lines().next().unwrap_or(detail);
    let typed = cause.contains(&format!(
        "initialize ACP bridge ({PROMPT_AUTH_REQUIRED_MARKER}):"
    )) || cause.contains(&format!(
        "create ACP session ({PROMPT_AUTH_REQUIRED_MARKER}):"
    ));
    // Older workers flatten ACP responses without a code. Only the known
    // signatures from the ACP error itself merit credential guidance.
    let reported = [
        "initialize ACP bridge: ",
        "create ACP session: ",
        "ACP protocol failed: ",
    ]
    .into_iter()
    .find_map(|context| cause.split_once(context))
    .is_some_and(|(_, cause)| {
        mj_core::credentials::text_reports_auth_failure(cause)
            || (profile.kind == mj_core::config::HarnessKind::Muse
                && cause.starts_with("Muse Code is not logged in"))
    });
    if !typed && !reported {
        return detail.to_owned();
    }
    let update_key = |env_key: &str| {
        format!(
            "Update {env_key} in Settings > Agent Profiles > {profile_id} > Environment variables"
        )
    };
    let remedy = match profile.auth_scheme() {
        AuthScheme::ApiKey { env_key } => update_key(&env_key),
        AuthScheme::InlineApiKey => format!(
            "Update the selected provider's experimental_bearer_token in {}",
            mj_core::codex_provider::describe_config(
                &profile.home,
                profile.codex_profile.as_deref()
            )
        ),
        AuthScheme::AwsCredentialChain => {
            "Refresh the AWS credentials used by the profile".to_owned()
        }
        AuthScheme::NoAuthentication => return detail.to_owned(),
        AuthScheme::NativeLogin | AuthScheme::NativeLoginStore => {
            let claude_auth = (profile.kind == mj_core::config::HarnessKind::Claude)
                .then(|| claude_environment_auth(profile.environment.resolved()))
                .flatten();
            let mut stored_login =
                format!("For a stored login, run `mj login --profile {profile_id}`");
            if profile.kind == mj_core::config::HarnessKind::Claude {
                stored_login.push_str(&format!(
                    ". For a stored setup token, run `mj login --profile {profile_id} --setup-token`"
                ));
            }
            match claude_auth {
                Some(ClaudeEnvironmentAuth::Token(env_key)) => format!(
                    "If the launch uses {env_key}, refresh it in Settings > Agent Profiles > {profile_id} > Environment variables. {stored_login}. If the target selects a Claude cloud provider, refresh that provider's credentials"
                ),
                Some(ClaudeEnvironmentAuth::ExternalProvider) => {
                    "Refresh the credentials for the configured Claude cloud provider".to_owned()
                }
                _ => match profile.kind {
                    mj_core::config::HarnessKind::Claude => format!(
                        "{stored_login}. For credentials supplied by the profile or target, update ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN or CLAUDE_CODE_OAUTH_TOKEN where it is set, or refresh the selected Claude cloud provider's credentials"
                    ),
                    mj_core::config::HarnessKind::Codex => format!(
                        "{stored_login}. If the profile or target supplies credentials, update OPENAI_API_KEY, CODEX_API_KEY or CODEX_ACCESS_TOKEN where it is set"
                    ),
                    mj_core::config::HarnessKind::Kimi => format!(
                        "{stored_login}. If the profile or target supplies KIMI_API_KEY, update it where it is set"
                    ),
                    _ => format!("Run `mj login --profile {profile_id}`"),
                },
            }
        }
    };
    format!(
        "Authentication failed while starting profile {profile_id}. {remedy}. Then {retry}.\n{detail}"
    )
}

/// Mark a new session's launch failed while it still names its target, so the
/// record is true while the rollback removes that target.
fn apply_failed_new_session_launch(state: &mut State, session_id: &str, recorded_error: &str) {
    let record = state.sessions.get_mut(session_id).unwrap();
    record.state = SessionState::StartupCleanup;
    record.updated_at = now();
    record.last_error = Some(format!("worker bootstrap failed: {recorded_error}"));
}

pub(super) fn apply_failed_new_session_rollback(
    state: &mut State,
    session_id: &str,
    recorded_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    match cleanup_error {
        None => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Error;
            record.target = None;
            record.updated_at = now();
            record.last_error = Some(format!("worker bootstrap failed: {recorded_error}"));
            anyhow::anyhow!("{recorded_error}; partial target removed and failed session retained")
        }
        Some(cleanup_error) => {
            let cleanup = format!("; cleanup of the failed session target failed: {cleanup_error}");
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::StartupCleanup;
            record.updated_at = now();
            record.last_error = Some(format!(
                "worker bootstrap failed: {recorded_error}{cleanup}"
            ));
            anyhow::anyhow!("{recorded_error}{cleanup}")
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
            SshTrafficClass::Bulk,
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
        // The second lane runs on its own thread, which starts outside the
        // runtime the first lane runs in; enter it so runtime-backed waits
        // (mj_core::runtime::block_on, downloads) work in both lanes.
        let runtime = tokio::runtime::Handle::try_current().ok();
        let second = scope.spawn(move || {
            let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
            match owner {
                Some(owner) => owner.scope_blocking(second),
                None => second(),
            }
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
    session_id: &str,
    setup: &RepositorySetup,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    let _timing = LaunchPhaseTimer::new(session_id, "materialize checkout");
    if let Some(checkout) = &setup.managed_checkout {
        let _cloning = ProvisionStageGuard::new(executor, ProvisionStage::Cloning);
        checkout.execute(executor)?;
    }
    let plan = &setup.plan;
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
    provision_target_creation_plan(
        plan,
        target,
        executor,
        |outputs| cleanup_failed_provision(target, session_id, outputs.first(), executor),
        discover,
    )
}

fn provision_target_creation_plan(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    executor: &(impl CommandExecutor + Sync),
    cleanup: impl FnOnce(&[CommandOutput]) -> Option<String>,
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
        .map_err(|error| match cleanup(&outputs) {
            Some(note) => error.context(note),
            None => error,
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

#[cfg(test)]
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
        .scripts()
        .iter()
        .map(|command| command.purpose.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let Err(error) = plan.execute(executor, &crate::controller::ControlWorker) else {
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
#[cfg(test)]
fn provisioned_locator_named(
    target: &targets::TargetTemplate,
    session_id: &str,
    _create_output: Option<&CommandOutput>,
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
        targets::TargetTemplate::SshAppleContainer { ssh, .. } => {
            targets::TargetLocator::SshAppleContainer {
                borrowed_from: None,
                ssh: ssh.clone(),
                container_id: container_id()?,
            }
        }
        targets::TargetTemplate::SshNitro { ssh, .. } => targets::TargetLocator::SshNitro {
            borrowed_from: None,
            ssh: ssh.clone(),
            container_id: container_id()?,
        },
        targets::TargetTemplate::SshBare { ssh, .. } => targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: targets::workspace_for(target, session_id).ok()?,
            worker_id: None,
        },
    })
}

pub(super) enum NitroBuildCache<'a> {
    Disabled,
    GuestLocal,
    Remote(targets::NitroMbxRemote<'a>),
}

/// Resolve the guest's cache placement from the fused target and accepted bundle.
pub(super) fn nitro_build_cache<'a>(
    template: &'a TargetTemplate,
    bundle: Option<&mj_core::config::ProjectBundle>,
) -> Result<NitroBuildCache<'a>> {
    let TargetTemplate::SshNitro { container, .. } = template else {
        return Ok(NitroBuildCache::Disabled);
    };
    let Some(cache) = &container.build_cache else {
        return Ok(NitroBuildCache::GuestLocal);
    };
    if cache.enabled == Some(false) {
        return Ok(NitroBuildCache::Disabled);
    }
    let (Some(url), Some(bundle)) = (cache.remote.as_deref(), bundle) else {
        return Ok(NitroBuildCache::GuestLocal);
    };
    let namespaces = bundle.mbx_namespaces()?;
    if namespaces.is_empty() {
        return Ok(NitroBuildCache::GuestLocal);
    }
    Ok(NitroBuildCache::Remote(targets::NitroMbxRemote {
        url,
        region: cache.remote_region.as_deref(),
        budget: cache.max_total_size.as_deref(),
        namespaces,
    }))
}

/// Drop host directories from the runtime mounts for a Firecracker guest.
pub(super) fn drop_nitro_host_directories(
    target: &targets::TargetTemplate,
    mounts: &mut Vec<targets::AdditionalMount>,
) -> Vec<String> {
    if !matches!(target, targets::TargetTemplate::SshNitro { .. }) {
        return Vec::new();
    }
    let mut notices = Vec::new();
    if !mounts.is_empty() {
        let attached = mounts
            .drain(..)
            .map(|mount| mount.source.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        notices.push(format!(
            "Nitro sandboxes cannot attach host directories, so this session starts without \
             {attached}. Copy what it needs into the workspace, or use a Podman target."
        ));
    }
    notices
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

/// Drop the CPU and memory limits a Podman host cannot enforce. Like the
/// overlay downgrade, this belongs to the host the container lands on, so it
/// is decided at every creation and never written over the session's sizes.
pub(super) fn enforce_cgroup_capable_limits(
    target: &mut targets::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Vec<String> {
    let (ssh, container) = match target {
        targets::TargetTemplate::LocalPodman(container) => (None, container),
        targets::TargetTemplate::SshPodman { ssh, container } => (Some(&*ssh), container),
        _ => return Vec::new(),
    };
    let sets = |limit: targets::CgroupLimit, args: &[String]| {
        args.iter()
            .any(|argument| argument.starts_with(limit.run_flag_prefix()))
    };
    if !targets::CgroupLimit::ALL
        .into_iter()
        .any(|limit| sets(limit, &container.extra_run_args))
    {
        return Vec::new();
    }
    let unenforceable = match targets::probe_podman_unenforceable_limits(ssh, executor) {
        Ok(limits) => limits,
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not read Podman's cgroup controllers; keeping session limits"
            );
            return vec![format!(
                "Could not read Podman's cgroup controllers, so the session keeps its CPU and \
                 memory limits: {error:#}"
            )];
        }
    };
    let dropped = unenforceable
        .into_iter()
        .filter(|limit| sets(*limit, &container.extra_run_args))
        .collect::<Vec<_>>();
    if dropped.is_empty() {
        return Vec::new();
    }
    container.extra_run_args.retain(|argument| {
        !dropped
            .iter()
            .any(|limit| argument.starts_with(limit.run_flag_prefix()))
    });
    let scope = match ssh {
        Some(ssh) => format!("On {}: ", ssh.destination),
        None => String::new(),
    };
    vec![format!(
        "{} {scope}{}",
        targets::unenforceable_limits_detail(&dropped),
        targets::CGROUP_DELEGATION_REMEDIATION
    )]
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
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        self.inner.pause_deadline()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.inner.cancellation_requested_without_deadline()
    }

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

    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.inner.execute_without_deadline(&self.staged(command))
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        self.inner
            .execute_with_stdin_without_deadline(&self.staged(command), input)
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
    if settings.is_empty() {
        return Ok(Vec::new());
    }
    let steps = settings.into_iter().map(|(key, value)| {
        targets::CommandStep::new(
            format!("inherit Git setting {key}"),
            vec![
                "git".into(),
                "config".into(),
                "--global".into(),
                "--replace-all".into(),
                "--".into(),
                key,
                value,
            ],
        )
    });
    Ok(vec![targets::command_on_locator(
        SshTrafficClass::Control,
        locator,
        session_id,
        targets::step_script(steps),
        "inherit session Git settings",
    )?])
}

#[cfg(test)]
mod tests;
