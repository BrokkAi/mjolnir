//! Parking a sub-agent whose turn has ended, and starting it again.
//!
//! A sub-agent child runs inside its parent's target, and in a container every
//! live child's harness holds hundreds of threads against the container's pids
//! limit (#1161). A child that has finished its turn, and whose parent has
//! been told so, therefore gives its processes back: its worker is stopped and
//! its record says [`SessionState::Parked`]. Everything else stays: the
//! record, the relation to the parent, the borrowed target locator, and the
//! worker root with its relay journal and the harness's native session id. A
//! parked child is started again in place, with the same sequence a worker
//! restart uses, when its parent gives it more input.
//!
//! The daemon runs both as lifecycle operations of the child, so they are
//! serialized with its close, suspend, destroy and each other.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

use mj_core::relay::WorkerHarnessStartupFailureKind;
use mj_core::state::SessionState;

use super::worker_binary::{
    probe_worker_liveness, refresh_installed_worker_binary, start_worker_durably,
    stop_worker_after_target_recovery,
};
use super::worker_restart::{InstalledWorkerRestart, WorkerRestartMessages};
use super::{Controller, IdleWorkspaceLease};
use crate::database::SubagentUnparkFailure;
use crate::session_manager::SessionManagerControl;
use crate::targets::{self, CommandExecutor};

/// How long a park waits for the child's session actor to exist.
const PARK_ACTOR_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a park waits, after recording `Parked`, for the session manager
/// to drop the child. The manager rereads the store twice a second.
const PARK_RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// What an unpark tells the operator at each step of the restart it runs.
const RESTART_FROM_PARKED: WorkerRestartMessages = WorkerRestartMessages {
    stop: "stop the parked sub-agent's worker",
    replace: "install the current Mjolnir worker binary for the parked sub-agent",
    start: "start the parked sub-agent's worker",
    connect: "connect to the parked sub-agent's worker after starting it",
    project_memory: "project memory will not be synchronized for the restarted sub-agent",
    native_session: "wait for the parked sub-agent's harness to load its conversation",
};

/// How one park attempt ended. Only [`ParkOutcome::Parked`] changed anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    /// The worker was stopped and the record says `Parked`.
    Parked,
    /// The child had work in flight or queued, such as a prompt its parent
    /// sent after the turn ended, so it was left running.
    Busy,
    /// The child is not a running sub-agent any more (it is closing, already
    /// parked, or gone), so there was nothing to park.
    NotRunning,
}

/// An executor for stopping what a failed unpark started. The unpark's own
/// executor may be the reason it failed, cancelled by a close of the child.
pub const FAILED_STARTUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

pub fn failed_launch_cleanup_executor() -> targets::CancellableProcessExecutor {
    targets::CancellableProcessExecutor::with_timeout(FAILED_STARTUP_CLEANUP_TIMEOUT)
}

/// Say plainly that a sub-agent could not start because its target ran out of
/// process slots, when `error` shows that, with the container's pid counts
/// when they can be read; return any other error unchanged.
///
/// The read is best effort, bounded by [`targets::PIDS_USAGE_READ_TIMEOUT`],
/// and itself fails in a container that is completely full, which the
/// message then says.
pub(super) fn explain_process_exhaustion(
    error: anyhow::Error,
    backend: &targets::TargetLocator,
    session_id: &str,
) -> anyhow::Error {
    if !targets::shows_process_exhaustion(&error) {
        return error;
    }
    let usage = targets::is_container(backend).then(|| {
        let executor =
            targets::CancellableProcessExecutor::with_timeout(targets::PIDS_USAGE_READ_TIMEOUT);
        targets::read_pids_usage(&executor, backend, session_id)
    });
    anyhow::anyhow!(targets::process_exhaustion_message(usage.as_ref(), &error))
}

impl Controller {
    /// Stop a running sub-agent's worker while it is idle, and record it as
    /// parked.
    ///
    /// The worker is reserved the way an idle worker upgrade reserves it: the
    /// actor's connection is leased only while the worker reports nothing
    /// running or queued, and the worker holds an idle barrier until it is
    /// stopped. A prompt that reaches the actor meanwhile waits behind the
    /// lease. The lease is kept until the session manager has dropped the
    /// child, which it does once the store says `Parked`, so such a prompt is
    /// rejected as undelivered rather than sent to a stopped worker; the
    /// caller that sent it can start the child again and resend it.
    ///
    /// Any failure before the record changes leaves the child running: the
    /// lease is dropped and the actor reconnects, restarting the worker if the
    /// stop got that far.
    pub async fn park_subagent_worker(
        &self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        quota_ordinal: Option<u64>,
    ) -> Result<ParkOutcome> {
        self.stop_idle_subagent_worker(
            admission,
            session_id,
            executor,
            manager,
            None,
            quota_ordinal,
        )
        .await
    }

    /// Stop a sub-agent whose first prompt can never run and record it as
    /// failed with `cause`, so every surface shows the failure instead of an
    /// idle session holding a worker (I1-2). It is stopped exactly as a park
    /// stops a child, behind the same idle reservation, and keeps its record,
    /// relation and target; its close settles without a checkpoint (see
    /// [`super::has_nothing_to_checkpoint`]).
    ///
    /// A child that took work meanwhile is left running and
    /// [`ParkOutcome::Busy`] is returned. [`ParkOutcome::Parked`] means the
    /// worker was stopped and the record now says `Error`.
    pub async fn fail_subagent_start_worker(
        &self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        cause: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<ParkOutcome> {
        self.stop_idle_subagent_worker(admission, session_id, executor, manager, Some(cause), None)
            .await
    }

    /// The stop behind a park and a failed start: the record becomes
    /// `Parked`, or `Error` with `failure` as its cause.
    async fn stop_idle_subagent_worker(
        &self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        failure: Option<&str>,
        quota_ordinal: Option<u64>,
    ) -> Result<ParkOutcome> {
        crate::worker_lifecycle::run_admitted(admission, session_id, async {
            crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
            ensure!(
                self.state.subagents.contains_key(session_id),
                "session {session_id} is not a sub-agent"
            );
            let Some(session) = self.state.sessions.get(session_id) else {
                return Ok(ParkOutcome::NotRunning);
            };
            if failure.is_some() && session.state == SessionState::Error {
                if session.target.is_some() {
                    let (backend, worker_root) = self.worker_placement(session_id)?;
                    let liveness =
                        probe_worker_liveness(executor, &backend, session_id, &worker_root)?;
                    if liveness.status != 0
                        || String::from_utf8_lossy(&liveness.stdout).trim() != "dead"
                    {
                        stop_worker_after_target_recovery(
                            executor,
                            &backend,
                            session_id,
                            &worker_root,
                        )
                        .context("stop the failed sub-agent's worker")?;
                    }
                }
                return Ok(ParkOutcome::Parked);
            }
            if session.state != SessionState::Running {
                return Ok(ParkOutcome::NotRunning);
            }
            let (backend, worker_root) = self.worker_placement(session_id)?;
            let handle = manager
                .wait_for_session(session_id, PARK_ACTOR_TIMEOUT)
                .await?;
            let Some(mut lease) = IdleWorkspaceLease::acquire_for_replacement(
                admission,
                &handle,
                session.harness_kind,
                mj_core::activity::WorkerReplacement::Other,
            )
            .await?
            else {
                return Ok(ParkOutcome::Busy);
            };
            let Some(snapshot) = lease.reserved_snapshot().await? else {
                return Ok(ParkOutcome::Busy);
            };
            if let Some(ordinal) = quota_ordinal
                && !snapshot.operational.assessment.as_ref().is_some_and(|a| {
                    use mj_core::assessment::{Action, Status};
                    a.status == Status::Deferred
                        && a.action == Some(Action::RecoverQuota)
                        && a.completed_ordinal == ordinal
                })
            {
                return Ok(ParkOutcome::Busy);
            }
            stop_worker_after_target_recovery(executor, &backend, session_id, &worker_root)
                .context("stop the sub-agent's worker")?;
            self.harvest_parked_login(session_id, session, &backend, executor)
                .await;
            let mut record = session.clone();
            record.state = if failure.is_some() {
                SessionState::Error
            } else {
                SessionState::Parked
            };
            record.last_error = failure.map(str::to_owned);
            record.updated_at = super::now();
            crate::database::save_lifecycle_session(&record)
                .context("record the stopped sub-agent")?;
            let released = tokio::time::timeout(PARK_RELEASE_TIMEOUT, async {
                while manager.session(session_id.to_owned()).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            if released.is_err() {
                tracing::warn!(
                    session_id,
                    "the session manager still held the stopped sub-agent; releasing it anyway"
                );
            }
            drop(lease);
            Ok(ParkOutcome::Parked)
        })
        .await
    }

    /// Adopt a copied native login after its writer has stopped. Shared links
    /// and push-only exports are excluded by the core login descriptor.
    pub(in crate::controller) async fn harvest_stopped_login(
        &self,
        session_id: &str,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<bool> {
        #[derive(serde::Deserialize)]
        struct StagedOwnershipProfile {
            profile_id: String,
        }

        let kind = session.harness_kind;
        let Some(relative_login) = mj_core::credentials::copied_login_relative_path(kind) else {
            return Ok(false);
        };
        let Some(profile) = self.config.profiles.get(&session.last_profile) else {
            return Ok(false);
        };
        if !profile.auth_scheme().uses_native_login_file() {
            return Ok(false);
        }
        let launch = self
            .current_worker_launch_config(session_id, backend)
            .context("resolve launch config to pull staged native login")?;
        let worker_root = crate::targets::worker_root(backend, session_id)?;
        let canonical_marker = profile.authentication_marker();
        let expected_profile = session.last_profile.clone();
        let bytes = match backend {
            targets::TargetLocator::LocalBare { .. } => {
                let ownership_path = std::path::PathBuf::from(&worker_root).join("ownership.json");
                let staged_path =
                    mj_core::config::harness_authentication_marker(kind, &launch.harness_home);
                if staged_path == canonical_marker {
                    return Ok(false);
                }
                let expected = expected_profile.clone();
                let maybe_bytes =
                    mj_core::runtime::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
                        match std::fs::read(&ownership_path) {
                            Ok(ownership_bytes) => {
                                let ownership: StagedOwnershipProfile =
                                    serde_json::from_slice(&ownership_bytes).with_context(|| {
                                        format!(
                                            "parse worker ownership {}",
                                            ownership_path.display()
                                        )
                                    })?;
                                if ownership.profile_id != expected {
                                    tracing::warn!(
                                        expected_profile = %expected,
                                        staged_profile = %ownership.profile_id,
                                        "skipping staged native login pull because ownership.json names another profile"
                                    );
                                    return Ok(None);
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(error).with_context(|| {
                                    format!("read worker ownership {}", ownership_path.display())
                                });
                            }
                        }
                        mj_core::credentials::read_harvestable_copied_login(kind, &launch.harness_home)
                    })
                    .await
                    .context("staged native login read task failed")??;
                match maybe_bytes {
                    Some(bytes) => bytes,
                    None => return Ok(false),
                }
            }
            _ => {
                let ownership_path =
                    format!("{}/ownership.json", worker_root.trim_end_matches('/'));
                let staged_path = format!(
                    "{}/{}",
                    launch.harness_home.to_string_lossy().trim_end_matches('/'),
                    relative_login.to_string_lossy()
                );
                let command = targets::locator_command(
                    targets::SshTrafficClass::Control,
                    backend,
                    vec![
                        "sh".into(),
                        "-c".into(),
                        "set -e; if [ -e \"$1\" ] || [ -L \"$1\" ]; then cat -- \"$1\"; fi; printf '\\0'; if [ ! -L \"$3\" ] && [ ! -L \"$4\" ] && [ ! -L \"$2\" ] && [ -f \"$2\" ]; then cat -- \"$2\"; fi".into(),
                        "mj-pull-native-login".into(),
                        ownership_path,
                        staged_path.clone(),
                        launch.harness_home.to_string_lossy().into_owned(),
                        std::path::Path::new(&staged_path).parent().context("staged login parent")?.to_string_lossy().into_owned(),
                    ],
                )
                .purpose("read staged native login");
                let output = executor
                    .execute(&command)
                    .context("execute staged native login read")?;
                if output.status != 0 {
                    anyhow::bail!(
                        "read staged native login from target exited with status {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                let Some(sep) = output.stdout.iter().position(|&byte| byte == 0) else {
                    anyhow::bail!("unexpected staged native login probe output without separator");
                };
                let ownership_bytes = &output.stdout[..sep];
                let auth_bytes = &output.stdout[sep + 1..];
                if !ownership_bytes.is_empty() {
                    let ownership: StagedOwnershipProfile = serde_json::from_slice(ownership_bytes)
                        .context("parse remote worker ownership.json")?;
                    if ownership.profile_id != expected_profile {
                        tracing::warn!(
                            session_id,
                            expected_profile = %expected_profile,
                            staged_profile = %ownership.profile_id,
                            "skipping staged native login pull because ownership.json names another profile"
                        );
                        return Ok(false);
                    }
                }
                if auth_bytes.is_empty() {
                    return Ok(false);
                }
                auth_bytes.to_vec()
            }
        };
        if !mj_core::credentials::harvestable_login_bytes(kind, &bytes)? {
            return Ok(false);
        }
        crate::worker_client::adopt_copied_login(profile, &bytes).await
    }

    /// Pull a refreshed native login from a stopped sub-agent's staged home back
    /// into the canonical profile store before `Parked` is published (#1633).
    ///
    /// Keyed by `session.last_profile`, which is the profile the stopped home
    /// was staged for. Failures are logged rather than aborting `Parked`
    /// publication because the worker is already stopped.
    async fn harvest_parked_login(
        &self,
        session_id: &str,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        executor: &(impl CommandExecutor + Sync),
    ) {
        if let Err(error) = self
            .harvest_stopped_login(session_id, session, backend, executor)
            .await
        {
            tracing::warn!(
                session_id,
                profile = %session.last_profile,
                error = format!("{error:#}"),
                "could not pull parked native login"
            );
        }
    }

    /// Accept a durable wake, launch the worker, and observe native readiness.
    /// Preparation and readiness hold no daemon admission. Once the wake is
    /// accepted, a proved dead worker settles with its failure; uncertain
    /// liveness leaves the active record and boot intent for recovery.
    /// It never stops a worker that may have resumed accepted work.
    // A Move stages a destination while preserving the parked source for rollback.
    pub async fn unpark_subagent_worker(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        move_destination: Option<(&mj_core::state::SessionRecord, Option<String>)>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        crate::worker_lifecycle::run_admitted(admission, session_id, async {
            let owner = crate::worker_lifecycle::require(session_id)?;
            owner.verify_cached_target(&self.state)?;
            ensure!(
                self.state.subagents.contains_key(session_id),
                "session {session_id} is not a sub-agent"
            );
            let session = self
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?;
            if session.state == SessionState::Running {
                return Ok(());
            }
            ensure!(
                session.state == SessionState::Parked,
                "sub-agent {session_id} is {} and cannot be started again",
                session.state.as_str()
            );
            let (backend, worker_root) = self.worker_placement(session_id)?;
            let reconnect = targets::reconnect_plan(&backend, session_id)?
                .commands
                .into_iter()
                .next()
                .context("reconnect plan is empty")?;
            // An older failed wake may have left a parked record and boot
            // intent. Only proved death permits retiring that intent.
            if crate::database::load_worker_restart(session_id)?.is_some() {
                let output = super::worker_binary::probe_worker_liveness(executor, &backend, session_id, &worker_root)?;
                ensure!(
                    output.status == 0 && String::from_utf8_lossy(&output.stdout).trim() == "dead",
                    "parked sub-agent still has a live worker boot"
                );
                owner.settle_dead_restart(
                    session
                        .target
                        .as_ref()
                        .context("parked child has no target")?,
                )?;
            }
            // Prepare using a local view of the chosen profile. The record
            // still names the source if staging fails or is interrupted.
            let previous_session = session.clone();
            let mut staging = Controller { config: self.config.clone(), state: self.state.clone() };
            if let Some((destination, _)) = move_destination.as_ref() {
                staging.state.sessions.insert(session_id.to_owned(), (*destination).clone());
            }
            // Stage the current login even when the parked child kept its profile.
            let staged = staging.prepare_worker_files_for(
                session_id,
                &backend,
                &worker_root,
                executor,
                super::worker_binary::WorkerFilePreparation::RefreshProfileOnly,
                super::worker_binary::LaunchHistory::Stored,
            )
            .context("stage the current profile login for the parked sub-agent")?;
            staging.state.sessions.get_mut(session_id).unwrap().staged_aws_profile.clone_from(&staged.aws_profile);
            let mut launch = staging.current_worker_launch_config(session_id, &backend)?;
            launch.discard_native_session_id = move_destination.as_ref().and_then(|(_, discard)| discard.clone());
            refresh_installed_worker_binary(executor, &backend, session_id)
                .context(RESTART_FROM_PARKED.replace)?;
            staging.install_launch_config(session_id, &backend, &launch, executor)?;
            // Keep container startup concurrency bounded through native readiness,
            // while cancellation releases this observation without stopping a worker.
            let gate = super::provisioning::container_start_gate(&backend);
            let _admitted = match &gate {
                Some(gate) => {
                    let acquiring = gate.acquire();
                    tokio::pin!(acquiring);
                    loop {
                        ensure!(!executor.cancellation_requested(), "sub-agent wake cancelled");
                        tokio::select! {
                            permit = &mut acquiring => break Some(permit.context("container start admission closed")?),
                            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                        }
                    }
                }
                None => None,
            };
            ensure!(
                !executor.cancellation_requested(),
                "sub-agent wake cancelled"
            );
            executor.end_resumable_work("subagent unpark launch")?;
            let target = session
                .target
                .as_ref()
                .context("parked child has no target")?;
            crate::database::begin_subagent_unpark(
                session_id,
                &crate::database::WorkerRestartIntent {
                    operation_id: owner.operation_id().to_owned(),
                    target: *target.clone(),
                    desired_build: String::new(),
                },
                move_destination.as_ref().map(|(destination, _)| *destination),
                Some(staged.aws_profile.as_deref()),
            )?;
            let observed = async {
                start_worker_durably(&owner, target, executor, &backend, &worker_root, &self.config.github)
                    .context(RESTART_FROM_PARKED.start)
                    .map_err(|error| explain_process_exhaustion(error, &backend, session_id))?;
                // The active record and boot intent are durable. Handoff may cancel
                // this wait; the next daemon attaches to the same live worker.
                executor.begin_resumable_work()?;
                staging.observe_started_worker(
                    session_id,
                    executor,
                    InstalledWorkerRestart {
                        backend: &backend,
                        worker_root: &worker_root,
                        reconnect: &reconnect,
                        launch: Some(&launch),
                        prepared: true,
                        discard_queued_prompts: None,
                        staged_aws_profile: Some(staged.aws_profile.as_deref()),
                        swap_admission: None,
                        messages: &RESTART_FROM_PARKED,
                    },
                )
                .await
            }.await;
            if let Err(error) = observed {
                // Probe liveness first: after the worker releases its root
                // lock its exit record is final. A failed connection alone
                // cannot authorize a retry.
                let liveness = super::worker_binary::probe_worker_liveness(executor, &backend, session_id, &worker_root).context("check whether the failed unpark worker exited")?;
                // The working-directory failure is written before the relay
                // opens. If the marker is already present while the worker is
                // still exiting, it is safe to settle without waiting for death.
                let command = super::worker_binary::worker_exit_record_command(&backend, &worker_root);
                let exited = executor.execute(&command)
                    .and_then(|output| mj_core::targets::checked_command_output(&command, output))
                    .and_then(|output| super::worker_binary::read_worker_exit_record(&output.stdout));
                match exited {
                    Ok(Some(exit)) => {
                        if let Some(failure) = exit.harness_startup_failure
                            && failure.kind == Some(WorkerHarnessStartupFailureKind::WorkingDirectoryUnavailable)
                        {
                            crate::database::fail_subagent_unpark(
                                session_id,
                                owner.operation_id(),
                                SubagentUnparkFailure::WorkingDirectoryUnavailable(&failure.message),
                            )?;
                            return Err(anyhow::Error::new(failure)
                                .context(mj_core::subagent::FAILED_CHILD_CLOSE_GUIDANCE));
                        }
                    }
                    Ok(None) => {}
                    Err(probe_error) => tracing::warn!(
                        session_id,
                        error = format!("{probe_error:#}"),
                        "could not read the failed wake's worker exit record"
                    ),
                }
                if liveness.status == 0 && String::from_utf8_lossy(&liveness.stdout).trim() == "dead" {
                    let detail = format!("{error:#}");
                    let evidence = staged.login
                        .filter(|_| mj_core::credentials::text_reports_auth_failure(&detail))
                        .map(|staged_login| mj_core::subagent::WakeFailure::Authentication {
                            profile_id: staging.state.sessions[session_id].last_profile.clone(),
                            staged_login,
                        });
                    crate::database::fail_subagent_unpark_with_rollback(
                        session_id, owner.operation_id(),
                        SubagentUnparkFailure::Retryable { detail: &detail, evidence: evidence.as_ref() },
                        move_destination.as_ref().map(|_| &previous_session),
                    )?;
                    anyhow::bail!("{detail}");
                }
                return Err(error);
            }
            crate::database::activate_worker_restart(session_id, owner.operation_id())?;
            let active = crate::database::load_session_record(session_id)?
                .context("activated sub-agent disappeared")?;
            self.state.sessions.insert(session_id.to_owned(), active);
            Ok(())
        })
        .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::v1::schema::ContentBlock;
    use mj_core::relay::RelayCommand;
    use mj_core::state::TargetLocator;

    use super::*;
    use crate::controller::checkpoint::tests::{
        LATCH_RELAY_SESSION, ReleaseSupport, latch_relay_target,
    };
    use crate::controller::test_support::{IsolatedTest, checkpoint_test_session, test_name};
    use crate::targets::{CommandOutput, CommandSpec};

    const MARKER: &str = "MJ_TEST_SUBAGENT_PARK_CHILD";

    /// Run the named test alone, with a store of its own. Returns whether
    /// this process is that run.
    fn isolated(test: &str) -> bool {
        if std::env::var_os(MARKER).is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("worker-source");
        std::fs::write(&worker, mj_core::worker_build::worker_build_stamp()).unwrap();
        IsolatedTest::new(test_name(module_path!(), test))
            .env(MARKER, "1")
            .env("MJ_WORKER_BINARY", worker)
            .env("MJ_INSTANCE", "durable-unpark-test")
            .isolated_store(directory.path())
            .run();
        false
    }

    /// Stands in for the target: every command succeeds, and the first one,
    /// which is the park's stop, also sends the child a prompt through its
    /// actor, the way a parent's `send_message` can race a park.
    #[derive(Default)]
    struct RacingStop {
        purposes: Mutex<Vec<String>>,
        racer: Mutex<
            Option<(
                crate::session_manager::ManagedSessionHandle,
                tokio::runtime::Handle,
            )>,
        >,
        raced: Mutex<Option<tokio::task::JoinHandle<Result<u64>>>>,
    }

    impl CommandExecutor for RacingStop {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if let Some((handle, runtime)) = self.racer.lock().unwrap().take() {
                *self.raced.lock().unwrap() = Some(runtime.spawn(async move {
                    handle
                        .submit(
                            "raced-prompt".into(),
                            RelayCommand::Prompt {
                                prompt: vec![ContentBlock::from("one more thing")],
                            },
                        )
                        .await
                }));
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// A parent, and the stand-in relay's session registered as its child on
    /// a bare target under `root`, with a report it handed back.
    fn register_child(root: &std::path::Path) {
        crate::database::save_session(&checkpoint_test_session("parent-1")).unwrap();
        let mut child = checkpoint_test_session(LATCH_RELAY_SESSION);
        child.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: root.join(LATCH_RELAY_SESSION),
        }));
        crate::database::save_subagent_session(
            &child,
            &mj_core::subagent::SubagentRecord {
                resume_with_parent: false,
                kind: mj_core::subagent::SubagentKind::Worker,
                reviewer_launch: None,
                child_session_id: LATCH_RELAY_SESSION.into(),
                parent_session_id: "parent-1".into(),
                task_name: "map the parser".into(),
                profile_id: "codex".into(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                initial_prompt: "map the parser".into(),
                request_key: "request-1".into(),
                created_at: "2026-09-25T00:00:00Z".into(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: true,
                orchestrator: false,
            },
        )
        .unwrap();
        assert!(
            crate::database::record_subagent_handback(
                LATCH_RELAY_SESSION,
                &mj_core::subagent::SubagentHandback {
                    command_id: "task-1".into(),
                    message: "The parser has three entry points.".into(),
                    recorded_at_ms: 1,
                },
            )
            .unwrap()
        );
    }

    fn loaded_controller() -> Controller {
        Controller {
            config: mj_core::config::Config::default(),
            state: crate::database::load_state().unwrap(),
        }
    }

    /// Target command boundaries are controlled; the store and relay journal
    /// are real. A detached launch must already have its active record and intent.
    struct WakeTarget {
        cancelled: AtomicBool,
        purposes: Mutex<Vec<String>>,
        admission: Mutex<Option<crate::upgrade::Work>>,
        gate: Arc<crate::upgrade::Gate>,
        observing: tokio::sync::Notify,
    }

    impl WakeTarget {
        fn new() -> Self {
            Self {
                cancelled: AtomicBool::new(false),
                purposes: Mutex::new(Vec::new()),
                admission: Mutex::new(None),
                gate: Arc::new(crate::upgrade::Gate::default()),
                observing: tokio::sync::Notify::new(),
            }
        }
    }

    impl CommandExecutor for WakeTarget {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if command.purpose == "start detached Mjolnir worker" {
                assert!(!self.gate.try_close(), "launch must hold admission");
                assert_eq!(
                    crate::database::load_session_state(LATCH_RELAY_SESSION)?,
                    Some(SessionState::Running)
                );
                assert!(crate::database::load_worker_restart(LATCH_RELAY_SESSION)?.is_some());
                crate::database::test_fixtures::seed_session_draft_input(
                    LATCH_RELAY_SESSION,
                    "newer draft",
                )?;
            }
            let stdout = if command.purpose == "detect target platform" {
                format!(
                    "{} {}\n",
                    if cfg!(target_os = "macos") {
                        "Darwin"
                    } else {
                        "Linux"
                    },
                    std::env::consts::ARCH
                )
                .into_bytes()
            } else if command.purpose == "probe Mjolnir worker daemon liveness" {
                b"alive\n".to_vec()
            } else {
                Vec::new()
            };
            Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }

        fn cancellation_requested(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }

        fn begin_resumable_work(&self) -> Result<()> {
            if self.admission.lock().unwrap().take().is_some() {
                self.observing.notify_one();
            }
            Ok(())
        }

        fn end_resumable_work(&self, label: &'static str) -> Result<()> {
            *self.admission.lock().unwrap() = Some(self.gate.enter(label)?);
            Ok(())
        }
    }

    fn waking_controller(root: &std::path::Path, startup_delay_ms: u64) -> Controller {
        use std::os::unix::fs::PermissionsExt;
        register_child(root);
        let mut parent = crate::database::read_durable_session_record("parent-1")
            .unwrap()
            .unwrap();
        parent.project_directory = Some(root.to_path_buf());
        parent.target_template_id = "local-bare".into();
        parent.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: root.join("parent-1"),
        }));
        parent.no_project_memory = true;
        crate::database::save_session(&parent).unwrap();
        let mut child = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        child.state = SessionState::Parked;
        child.target_template_id = "local-bare".into();
        child.project_directory = Some(root.to_path_buf());
        child.no_project_memory = true;
        crate::database::save_session(&child).unwrap();
        let worker_root = root.join(LATCH_RELAY_SESSION);
        std::fs::create_dir_all(&worker_root).unwrap();
        let relay = latch_relay_target(root, None, ReleaseSupport::Supported, true);
        let mut script = String::from("#!/bin/sh\n");
        for (key, value) in &relay.spec.env {
            script.push_str(&format!("export {key}={}\n", targets::posix_quote(value)));
        }
        script.push_str(&format!(
            "export MJ_TEST_LATCH_STARTUP_DELAY_MS={startup_delay_ms}\nexec {}\n",
            targets::join_remote_command(
                &[relay.spec.program.clone(),]
                    .into_iter()
                    .chain(relay.spec.args.iter().cloned())
                    .collect::<Vec<_>>()
            ),
        ));
        let worker = worker_root.join("hel");
        std::fs::write(&worker, script).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut controller = loaded_controller();
        controller.config = crate::controller::test_support::resume_compatibility_config();
        let home = root.join("profile");
        std::fs::create_dir_all(&home).unwrap();
        controller.config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                environment: Default::default(),
                context_window_bytes: None,
                models: Default::default(),
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                codex_profile: None,
                legacy_muse_auto_review: None,
                load_balancing_may_consume_credits: false,
                claude_permission_mode: None,
                mjolnir_guardian: true,
                aws: None,
                ..mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Codex, home)
            },
        );
        controller
    }

    // Hard-won: #1508: a refused unpark left a running record and endless reconnects.
    #[tokio::test]
    async fn dead_unpark_returns_to_parked_and_releases_its_boot_atomically() {
        if !isolated("dead_unpark_returns_to_parked_and_releases_its_boot_atomically") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let controller = waking_controller(root.path(), 0);
        let target = controller.state.sessions[LATCH_RELAY_SESSION]
            .target
            .clone()
            .unwrap();
        let owner =
            crate::controller::test_support::worker_test_permit(LATCH_RELAY_SESSION, "test unpark")
                .unwrap()
                .unwrap();
        owner
            .scope(async {
                let intent = crate::database::WorkerRestartIntent {
                    operation_id: owner.operation_id().to_owned(),
                    target: *target,
                    desired_build: String::new(),
                };
                let previous = controller.state.sessions[LATCH_RELAY_SESSION].clone();
                let mut destination = previous.clone();
                destination.last_profile = "moved-profile".into();
                destination.harness_kind = mj_core::config::HarnessKind::Claude;
                destination.native_session_id = None;
                crate::database::begin_subagent_unpark(
                    LATCH_RELAY_SESSION,
                    &intent,
                    Some(&destination),
                    None,
                )
                .unwrap();
                let detail = "workspace routing discovery unauthorized (401)";
                let evidence = mj_core::subagent::WakeFailure::Authentication {
                    profile_id: "codex".into(),
                    staged_login: mj_core::credentials::LoginFingerprint::File("staged".into()),
                };
                // A stale completion must leave the current boot and state intact.
                assert!(
                    crate::database::fail_subagent_unpark(
                        LATCH_RELAY_SESSION,
                        "stale-owner",
                        SubagentUnparkFailure::Retryable {
                            detail,
                            evidence: Some(&evidence),
                        }
                    )
                    .is_err()
                );
                assert_eq!(
                    crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
                    Some(SessionState::Running)
                );
                crate::database::fail_subagent_unpark_with_rollback(
                    LATCH_RELAY_SESSION,
                    owner.operation_id(),
                    SubagentUnparkFailure::Retryable {
                        detail,
                        evidence: Some(&evidence),
                    },
                    Some(&previous),
                )
                .unwrap();
                let record = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
                    .unwrap()
                    .unwrap();
                assert_eq!(record.state, SessionState::Parked);
                assert_eq!(record.last_profile, previous.last_profile);
                assert_eq!(record.harness_kind, previous.harness_kind);
                assert_eq!(record.native_session_id, previous.native_session_id);
                let saved = record.last_error.as_deref().unwrap();
                assert!(saved.starts_with(detail), "{saved}");
                assert_eq!(
                    mj_core::subagent::WakeFailure::decode(saved),
                    (detail, Some(evidence))
                );
                assert!(
                    crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                        .unwrap()
                        .is_none()
                );
                // The same child can accept a later wake after login repair.
                crate::database::begin_subagent_unpark(LATCH_RELAY_SESSION, &intent, None, None)
                    .unwrap();
                assert_eq!(
                    crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
                    Some(SessionState::Running)
                );
            })
            .await;
    }

    // Hard-won: a6ad3cc3c: unpark waited for a resumed provider retry to finish and then stopped it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpark_accepts_native_readiness_while_durable_work_is_running() {
        if !isolated("unpark_accepts_native_readiness_while_durable_work_is_running") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let mut controller = waking_controller(root.path(), 0);
        let executor = WakeTarget::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            controller.unpark_subagent_worker(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                None,
                &executor,
            ),
        )
        .await
        .expect("a running turn must not delay readiness")
        .unwrap();
        assert!(executor.gate.try_close());
        let record = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Running);
        assert_eq!(record.draft_input, "newer draft");
        assert!(
            crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                .unwrap()
                .is_none()
        );
        let snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            snapshot.active_prompt.unwrap().command_id,
            "seed-running-prompt"
        );
        assert!(
            !executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "stop Mjolnir worker daemon")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parked_move_starts_destination_in_the_existing_environment() {
        if !isolated("parked_move_starts_destination_in_the_existing_environment") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let mut controller = waking_controller(root.path(), 0);
        let previous = controller.state.sessions[LATCH_RELAY_SESSION].clone();
        controller.config.profiles.insert(
            "destination".into(),
            controller.config.profiles["codex"].clone(),
        );
        let mut operation =
            super::super::move_session::test_fixtures::source_recovery_operation(&previous);
        operation.in_place = true;
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        controller
            .execute_move(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                &mut operation,
                None,
                &WakeTarget::new(),
                &channels.control,
                super::super::move_session::MoveSourceRelay::default(),
            )
            .await
            .unwrap();
        channels.shutdown.shutdown().await.unwrap();
        assert_eq!(operation.phase, mj_core::state::MovePhase::Completed);
        let active = crate::database::load_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert_eq!(active.last_profile, "destination");
        assert_eq!(active.target, previous.target);
        assert_eq!(active.state, SessionState::Running);
    }

    // Hard-won: #1547: a woken child started with the login it parked with, which a renewal had revoked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpark_starts_the_worker_with_the_profiles_current_login() {
        if !isolated("unpark_starts_the_worker_with_the_profiles_current_login") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let mut controller = waking_controller(root.path(), 0);
        let profile = controller.config.profiles["codex"].clone();
        let (backend, _) = controller.worker_placement(LATCH_RELAY_SESSION).unwrap();
        let staged = std::path::PathBuf::from(crate::targets::target_profile_home(
            &backend,
            LATCH_RELAY_SESSION,
            &profile,
        ))
        .join(profile.kind.credential_file_name());
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"{\"login\":\"the one the child parked with\"}").unwrap();
        std::fs::write(
            profile.authentication_marker(),
            b"{\"login\":\"renewed while it was parked\"}",
        )
        .unwrap();
        controller
            .unpark_subagent_worker(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                None,
                &WakeTarget::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            b"{\"login\":\"renewed while it was parked\"}"
        );
    }

    // Hard-won: a6ad3cc3c: readiness observation held daemon admission and cancellation stopped accepted worker work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_unpark_releases_handoff_and_its_next_actor_preserves_the_live_boot() {
        if !isolated("slow_unpark_releases_handoff_and_its_next_actor_preserves_the_live_boot") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let mut controller = waking_controller(root.path(), 60_000);
        let executor = Arc::new(WakeTarget::new());
        let waking = mj_core::runtime::spawn({
            let executor = executor.clone();
            async move {
                controller
                    .unpark_subagent_worker(
                        &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                        LATCH_RELAY_SESSION,
                        None,
                        &*executor,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), executor.observing.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if std::fs::read(root.path().join(mj_core::relay::RELAY_STATE_FILE))
                    .ok()
                    .and_then(|bytes| {
                        serde_json::from_slice::<mj_core::relay::RelaySnapshot>(&bytes).ok()
                    })
                    .is_some_and(|snapshot| snapshot.active_prompt.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            executor.gate.try_close(),
            "harness readiness must not hold handoff"
        );
        executor.cancelled.store(true, Ordering::SeqCst);
        let error = tokio::time::timeout(Duration::from_secs(10), waking)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert_eq!(
            crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
            Some(SessionState::Running)
        );
        assert_eq!(
            crate::database::worker_restart_phase(LATCH_RELAY_SESSION).unwrap(),
            Some(crate::database::WorkerRestartPhase::AwaitingReadiness)
        );
        let source_target = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap()
            .target
            .unwrap();
        let snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            snapshot.active_prompt.unwrap().command_id,
            "seed-running-prompt"
        );
        let mut target = latch_relay_target(root.path(), None, ReleaseSupport::Supported, false);
        target.worker_recovery = Some(crate::session_manager::WorkerRecoveryPlan {
            source_target: *source_target,
            target: None,
            workspace: None,
            exit_record: None,
            liveness_probe: CommandSpec::new("printf", ["alive\n"])
                .purpose("probe Mjolnir worker daemon liveness")
                .into(),
            binary_refresh: None,
            launch_refresh: None,
            restart: targets::CommandPlan {
                description: "a live wake must not be restarted".into(),
                commands: vec![CommandSpec::new("false", std::iter::empty::<&str>())],
            }
            .into(),
        });
        executor.cancelled.store(false, Ordering::SeqCst);
        assert_eq!(
            crate::session_manager::recover_worker_controlled(
                target.worker_recovery.clone().unwrap(),
                true,
                Some(LATCH_RELAY_SESSION),
                &*executor,
            )
            .unwrap(),
            crate::session_manager::WorkerRecoveryOutcome::Starting
        );
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        channels.targets.send(vec![target]).unwrap();
        let handle = channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        handle.sync_now().await.unwrap();
        assert!(
            crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                .unwrap()
                .is_none()
        );
        assert!(
            handle
                .view()
                .snapshot
                .unwrap()
                .operational
                .native_session_is_ready()
        );
        assert_eq!(
            executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .filter(|p| *p == "start detached Mjolnir worker")
                .count(),
            1
        );
        assert!(
            !executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "stop Mjolnir worker daemon")
        );
        channels.shutdown.shutdown().await.unwrap();
    }

    #[test]
    fn failed_wake_publication_rolls_back_its_boot_intent() {
        if !isolated("failed_wake_publication_rolls_back_its_boot_intent") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let controller = waking_controller(root.path(), 0);
        let connection = rusqlite::Connection::open(crate::database::database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_wake BEFORE UPDATE OF state ON sessions
             WHEN NEW.state='running' AND OLD.state='parked'
             BEGIN SELECT RAISE(ABORT, 'wake publication failed'); END;",
            )
            .unwrap();
        let owner = crate::controller::test_support::worker_test_permit(
            LATCH_RELAY_SESSION,
            "test wake transaction",
        )
        .unwrap()
        .unwrap();
        let error = owner
            .scope_blocking(|| {
                crate::database::begin_subagent_unpark(
                    LATCH_RELAY_SESSION,
                    &crate::database::WorkerRestartIntent {
                        operation_id: owner.operation_id().into(),
                        target: *controller.state.sessions[LATCH_RELAY_SESSION]
                            .target
                            .clone()
                            .unwrap(),
                        desired_build: String::new(),
                    },
                    None,
                    None,
                )
            })
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("wake publication failed"),
            "{error:#}"
        );
        assert_eq!(
            crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
            Some(SessionState::Parked)
        );
        assert!(
            crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parking_stops_an_idle_child_keeps_its_record_and_turns_away_a_racing_prompt() {
        if !isolated("parking_stops_an_idle_child_keeps_its_record_and_turns_away_a_racing_prompt")
        {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let crate::session_manager::SessionManagerChannels {
            session_cpu: _,
            targets,
            control,
            updates: _updates,
            shutdown,
        } = crate::session_manager::spawn_session_manager().unwrap();
        targets
            .send(vec![latch_relay_target(
                root.path(),
                None,
                ReleaseSupport::Supported,
                false,
            )])
            .unwrap();
        let handle = control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let executor = RacingStop::default();
        *executor.racer.lock().unwrap() = Some((handle, tokio::runtime::Handle::current()));
        // What the daemon's target refresher does: once the store says the
        // child is parked, the session manager no longer holds it.
        let refresher = mj_core::runtime::spawn(async move {
            while crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap()
                != Some(SessionState::Parked)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            targets.send_replace(Vec::new());
            targets
        });

        let outcome = loaded_controller()
            .park_subagent_worker(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                &executor,
                &control,
                None,
            )
            .await
            .unwrap();

        assert_eq!(outcome, ParkOutcome::Parked);
        assert!(
            executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|purpose| purpose == "stop Mjolnir worker daemon"),
            "the child's worker was stopped: {:?}",
            executor.purposes.lock().unwrap()
        );
        // The prompt that arrived during the park never reached the stopped
        // worker, and its sender is told so for certain, so it can start the
        // child again and send it once more.
        let raced = executor.raced.lock().unwrap().take();
        let raced = raced
            .expect("the stop raced a prompt")
            .await
            .unwrap()
            .expect_err("a prompt that arrived during the park is turned away");
        assert!(
            raced
                .downcast_ref::<mj_client::session::DeliveryUnconfirmed>()
                .is_none(),
            "a turned-away prompt is known not to be delivered: {raced:#}"
        );
        // Everything but the worker stays.
        let stored = crate::database::load_state().unwrap();
        let child = &stored.sessions[LATCH_RELAY_SESSION];
        assert_eq!(child.state, SessionState::Parked);
        assert!(child.target.is_some(), "a parked child keeps its target");
        assert!(stored.subagents.contains_key(LATCH_RELAY_SESSION));
        assert_eq!(
            crate::database::load_subagent_report(LATCH_RELAY_SESSION)
                .unwrap()
                .handback
                .map(|handback| handback.message)
                .as_deref(),
            Some("The parser has three entry points.")
        );
        assert!(control.session(LATCH_RELAY_SESSION).await.is_err());
        let _targets = refresher.await.unwrap();
        shutdown.shutdown().await.unwrap();
    }

    /// I1-2: a child whose first prompt is refused for good has its worker
    /// stopped and is recorded as failed with the cause, keeping its record,
    /// relation and target, instead of staying a live idle session.
    // Hard-won: ba6c34276ced: a refused first prompt left a live idle child whose parent had already seen the error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_whose_start_failed_is_stopped_and_recorded_as_failed() {
        if !isolated("a_child_whose_start_failed_is_stopped_and_recorded_as_failed") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let crate::session_manager::SessionManagerChannels {
            session_cpu: _,
            targets,
            control,
            updates: _updates,
            shutdown,
        } = crate::session_manager::spawn_session_manager().unwrap();
        targets
            .send(vec![latch_relay_target(
                root.path(),
                None,
                ReleaseSupport::Supported,
                false,
            )])
            .unwrap();
        control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let refresher = mj_core::runtime::spawn(async move {
            while crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap()
                != Some(SessionState::Error)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            targets.send_replace(Vec::new());
            targets
        });
        let executor = RacingStop::default();
        let cause = "this agent does not offer high as a effort";

        let outcome = loaded_controller()
            .fail_subagent_start_worker(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                cause,
                &executor,
                &control,
            )
            .await
            .unwrap();

        assert_eq!(outcome, ParkOutcome::Parked);
        assert!(
            executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|purpose| purpose == "stop Mjolnir worker daemon"),
            "the child's worker was stopped: {:?}",
            executor.purposes.lock().unwrap()
        );
        let stored = crate::database::load_state().unwrap();
        let child = &stored.sessions[LATCH_RELAY_SESSION];
        assert_eq!(child.state, SessionState::Error);
        assert_eq!(child.last_error.as_deref(), Some(cause));
        assert!(child.target.is_some(), "the failed child keeps its target");
        assert!(stored.subagents.contains_key(LATCH_RELAY_SESSION));
        assert!(control.session(LATCH_RELAY_SESSION).await.is_err());
        let _targets = refresher.await.unwrap();
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_with_work_in_flight_is_not_parked() {
        if !isolated("a_child_with_work_in_flight_is_not_parked") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        channels
            .targets
            .send(vec![latch_relay_target(
                root.path(),
                None,
                ReleaseSupport::Supported,
                true,
            )])
            .unwrap();
        channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let executor = RacingStop::default();

        let outcome = loaded_controller()
            .park_subagent_worker(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                &executor,
                &channels.control,
                None,
            )
            .await
            .unwrap();

        assert_eq!(outcome, ParkOutcome::Busy);
        assert!(
            executor.purposes.lock().unwrap().is_empty(),
            "nothing stopped"
        );
        assert_eq!(
            crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
            Some(SessionState::Running)
        );
        channels.shutdown.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_failure_stops_the_live_worker_of_an_error_child() {
        if !isolated("startup_failure_stops_the_live_worker_of_an_error_child") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let id = LATCH_RELAY_SESSION;
        let child = crate::database::read_durable_session_record(id)
            .unwrap()
            .unwrap();
        let failed = crate::database::fail_unready_session(
            id,
            "original readiness error",
            child.state,
            &child.updated_at,
        )
        .unwrap()
        .unwrap();
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let target = WakeTarget::new();

        let outcome = loaded_controller()
            .fail_subagent_start_worker(
                &crate::controller::test_support::session_admission(id),
                id,
                "later startup rejection",
                &target,
                &channels.control,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ParkOutcome::Parked);
        assert_eq!(
            *target.purposes.lock().unwrap(),
            [
                "probe Mjolnir worker daemon liveness",
                "stop Mjolnir worker daemon"
            ]
        );
        let persisted = crate::database::read_durable_session_record(id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.state, SessionState::Error);
        assert_eq!(persisted.last_error, failed.last_error);
        channels.shutdown.shutdown().await.unwrap();
    }

    // Hard-won: 6927da2ba976: Issue #1161 exhausted a shipped container's process slots and left users with a Cannot fork failure.
    #[test]
    fn only_a_full_target_is_rewritten_and_a_bare_one_reads_no_container_counts() {
        let backend = targets::TargetLocator::LocalBare {
            worker_root: "/tmp/workers/child".into(),
        };
        let unrelated =
            explain_process_exhaustion(anyhow::anyhow!("the harness exited"), &backend, "child");
        assert_eq!(format!("{unrelated:#}"), "the harness exited");

        let full = explain_process_exhaustion(
            anyhow::anyhow!("sh: 1: Cannot fork").context("start the parked sub-agent's worker"),
            &backend,
            "child",
        );
        let message = format!("{full:#}");
        assert!(
            message.starts_with("the target machine ran out of process slots"),
            "{message}"
        );
        assert!(
            message.contains("Close sub-agents you no longer need"),
            "{message}"
        );
        assert!(
            message.contains("Cannot fork"),
            "the original error stays: {message}"
        );
    }

    // Hard-won: #1633: parking a Codex sub-agent discarded a refreshed login in its staged home before the next unpark overwrote it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parking_codex_subagent_pulls_refreshed_login_keyed_by_staged_profile() {
        if !isolated("parking_codex_subagent_pulls_refreshed_login_keyed_by_staged_profile") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let canonical_a = root.path().join("canonical-a");
        let canonical_b = root.path().join("canonical-b");
        std::fs::create_dir_all(&canonical_a).unwrap();
        std::fs::create_dir_all(&canonical_b).unwrap();

        let stale_a = serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "stale-access-a",
                "refresh_token": "stale-refresh-a",
                "id_token": "id-a",
                "account_id": "acct-a"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let refreshed_a = serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "fresh-access-a",
                "refresh_token": "fresh-refresh-a",
                "id_token": "id-a",
                "account_id": "acct-a"
            },
            "last_refresh": "2026-02-01T00:00:00Z"
        }))
        .unwrap();
        let stale_b = serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "stale-access-b",
                "refresh_token": "stale-refresh-b",
                "id_token": "id-b",
                "account_id": "acct-b"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        std::fs::write(canonical_a.join("auth.json"), &stale_a).unwrap();
        std::fs::write(canonical_b.join("auth.json"), &stale_b).unwrap();

        let mut config = crate::controller::test_support::resume_compatibility_config();
        let profile_a = mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            canonical_a.clone(),
        );
        let profile_b = mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            canonical_b.clone(),
        );
        config.profiles.insert("codex-a".into(), profile_a);
        config.profiles.insert("codex-b".into(), profile_b);

        let mut parent = checkpoint_test_session("parent-1");
        parent.last_profile = "codex-a".into();
        parent.project_directory = Some(root.path().to_path_buf());
        parent.target_template_id = "local-bare".into();
        parent.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: root.path().join("parent-1"),
        }));
        crate::database::save_session(&parent).unwrap();
        let mut child = checkpoint_test_session(LATCH_RELAY_SESSION);
        child.harness_kind = mj_core::config::HarnessKind::Codex;
        child.last_profile = "codex-a".into();
        child.project_directory = Some(root.path().to_path_buf());
        child.target_template_id = "local-bare".into();
        child.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: root.path().join(LATCH_RELAY_SESSION),
        }));
        crate::database::save_subagent_session(
            &child,
            &mj_core::subagent::SubagentRecord {
                resume_with_parent: false,
                kind: mj_core::subagent::SubagentKind::Worker,
                reviewer_launch: None,
                child_session_id: LATCH_RELAY_SESSION.into(),
                parent_session_id: "parent-1".into(),
                task_name: "refresh test".into(),
                profile_id: "codex-b".into(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                initial_prompt: "refresh test".into(),
                request_key: "request-refresh".into(),
                created_at: "2026-09-25T00:00:00Z".into(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: true,
                orchestrator: false,
            },
        )
        .unwrap();

        let mut controller = Controller {
            config,
            state: crate::database::load_state().unwrap(),
        };
        let (backend, _) = controller.worker_placement(LATCH_RELAY_SESSION).unwrap();
        let launch = controller
            .current_worker_launch_config(LATCH_RELAY_SESSION, &backend)
            .unwrap();
        std::fs::create_dir_all(&launch.harness_home).unwrap();
        std::fs::write(launch.harness_home.join("auth.json"), &refreshed_a).unwrap();

        let crate::session_manager::SessionManagerChannels {
            session_cpu: _,
            targets,
            control,
            updates: _updates,
            shutdown,
        } = crate::session_manager::spawn_session_manager().unwrap();
        targets
            .send(vec![latch_relay_target(
                root.path(),
                None,
                ReleaseSupport::Supported,
                false,
            )])
            .unwrap();
        control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();

        let refresher = mj_core::runtime::spawn(async move {
            while crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap()
                != Some(SessionState::Parked)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            targets.send_replace(Vec::new());
            targets
        });

        let executor = RacingStop::default();
        let outcome = controller
            .park_subagent_worker(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                &executor,
                &control,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ParkOutcome::Parked);
        assert_eq!(
            std::fs::read(canonical_a.join("auth.json")).unwrap(),
            refreshed_a,
            "refreshed login must be pulled into the staged profile's canonical store"
        );
        assert_eq!(
            std::fs::read(canonical_b.join("auth.json")).unwrap(),
            stale_b,
            "unrelated profile's canonical store must not be touched"
        );
        // The same stopped-home ownership path also harvests nested non-Codex grants.
        controller.config.profiles.get_mut("codex-a").unwrap().kind =
            mj_core::config::HarnessKind::Kimi;
        let mut stopped = child.clone();
        stopped.harness_kind = mj_core::config::HarnessKind::Kimi;
        controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap()
            .harness_kind = mj_core::config::HarnessKind::Kimi;
        let canonical = canonical_a.join("credentials/kimi-code.json");
        let staged = launch.harness_home.join("credentials/kimi-code.json");
        std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(
            &canonical,
            b"{\"expires_at\":100,\"refresh_token\":\"old\"}",
        )
        .unwrap();
        let refreshed = b"{\"expires_at\":200,\"refresh_token\":\"new\"}";
        std::fs::write(&staged, refreshed).unwrap();
        assert!(
            controller
                .harvest_stopped_login(LATCH_RELAY_SESSION, &stopped, &backend, &executor)
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read(canonical).unwrap(), refreshed);
        let _targets = refresher.await.unwrap();
        shutdown.shutdown().await.unwrap();
    }
}
