//! The second-opinion reviewer that runs beside a primary session.
//!
//! A reviewer is a sidecar, not a session. It shares the primary's target and
//! working directory and owns nothing else: its harness home is a fresh copy
//! of the chosen profile, staged under the primary worker root, and it keeps
//! its own native session and durable relay there. Hel never gives it a
//! session record, a target, a repository checkout, or a lifecycle operation.
//!
//! Reusing [`DurableRelay`] for the reviewer is deliberate. It makes the
//! reviewer's conversation journaled, replayable and recoverable on exactly
//! the terms the primary's is, so the controller projects and renders it with
//! the code it already has instead of a parallel transcript pipeline.
//!
//! The sidecar owns independent roles. Plan review and turn review use the
//! default `reviewer` role; review-settings discovery uses a named role so it
//! cannot interrupt a live review. Each role has its own harness home, relay,
//! journal and lock.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::unix::{ACP_EVENT_CHANNEL_CAPACITY, run_relay_coordinator};
use super::{
    AcpSupervisorSpec, REVIEWER_DIR, REVIEWER_PROFILE_DIR, REVIEWER_ROLES_DIR, ReviewerLaunchConfig,
};
use crate::acp::{self, CommandRequest, LaunchSpec};
use crate::relay::{
    DurableRelay, RelayCommand, RelayCursor, RelayEvent, RelayObservation, RelayOperationalState,
    RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponseEnvelope,
    RelayResponsePayload, ReviewerAdmission,
};
use mj_core::config::HarnessKind;
use mj_core::worker_launch::HarnessRuntimePolicy;

/// How long a reviewer may take to open its native session and advertise its
/// configuration. A harness that has to authenticate or warm a large profile
/// is slow, but a harness that never answers must not hang the controller.
const START_TIMEOUT: Duration = Duration::from_secs(180);
/// How long one configuration change may take to apply.
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a paused reviewer's runtime is given to terminate its harness
/// process group before pause reports that it is still stopping.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a cancelled reviewer turn is given to leave the relay idle before
/// the pause stops waiting for it.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval between reads of the reviewer relay's durable state while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// The default role. Requests without a role mean this role.
pub use mj_core::review::driver::REVIEWER_ROLE as DEFAULT_ROLE;
/// File inside a role root naming the reviewer generation it was copied for.
const ROLE_GENERATION_MARKER: &str = ".hel-reviewer-generation";
/// The default role keeps its relay at the historical role root, but its
/// harness home must be separate from the controller's staged profile.
const DEFAULT_RUNTIME_PROFILE_DIR: &str = "runtime-profile";
/// Old reviewer relay files are retained here when a new generation starts.
/// Keeping them out of the live root lets [`DurableRelay::open`] create a
/// genuinely new native conversation while preserving forensic history.
const RELAY_ARCHIVE_DIR: &str = "relay-archive";

/// Why the client-side operation cancellation watcher fired.
///
/// This is an in-process signal, not a relay protocol variant. A relay client
/// that disconnects while a reviewer action is still running must stop that
/// action, while a client that disconnects after the action's response has been
/// produced must leave the reviewer running for its next connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewerCancellation {
    ClientDisconnected,
}

impl ReviewerCancellation {
    fn message(self) -> &'static str {
        match self {
            Self::ClientDisconnected => {
                "reviewer operation cancelled because the client disconnected"
            }
        }
    }
}

/// Everything a reviewer inherits from the primary session it reviews for.
#[derive(Debug, Clone)]
pub struct ReviewerPlacement {
    pub target_environment: std::collections::BTreeMap<String, String>,
    /// The primary worker root. The reviewer lives in a subdirectory of it.
    pub worker_root: PathBuf,
    /// Primary session id, used to name the reviewer's relay session.
    pub session_id: String,
    /// The primary's working directory. The reviewer reads the same tree.
    pub cwd: PathBuf,
    /// The primary's additional workspace roots.
    pub additional_directories: Vec<PathBuf>,
    /// This worker's own executable, which supervises the reviewer's bridge
    /// exactly as it supervises the primary's.
    pub worker_executable: PathBuf,
    /// Reviewers inherit the primary worker's harness ownership policy.
    pub harness_runtime: HarnessRuntimePolicy,
    /// Whether this session took a review baseline when it started. A session
    /// started without one cannot be reviewed: with no record of what the tree
    /// held before the turn, every file would be reported as new work.
    pub review_capture: bool,
    /// Which untracked paths each repository already held at the point the
    /// current baseline was taken. A review needs this to tell a file the turn
    /// created from one that was already lying in the working tree, without
    /// having read any of their contents at startup. It moves forward with the
    /// baseline, so it is shared rather than copied.
    pub untracked_at_start:
        Arc<Mutex<BTreeMap<PathBuf, Vec<crate::review::capture::UntrackedEntry>>>>,
}

impl ReviewerPlacement {
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.worker_root.join(REVIEWER_DIR)
    }

    /// Where the controller stages the chosen profile. Every role's own home
    /// is a copy of this one.
    #[must_use]
    pub fn profile_home(&self) -> PathBuf {
        mj_core::worker_launch::reviewer_staging_profile_home(&self.worker_root, 0)
    }

    /// Where the controller stages one immutable profile snapshot. Generation
    /// zero retains the original path for workers upgraded in place; later
    /// generations get their own path so concurrent role launches never
    /// replace a snapshot another launch is reading.
    #[must_use]
    fn staged_profile_home(&self, generation: u64) -> PathBuf {
        if generation == 0 {
            self.profile_home()
        } else {
            mj_core::worker_launch::reviewer_staging_profile_home(&self.worker_root, generation)
        }
    }

    /// Where one role lives. The default role keeps the original layout, so a
    /// worker staged before roles existed still finds its journal.
    #[must_use]
    fn role_root(&self, role: &str) -> PathBuf {
        if role == DEFAULT_ROLE {
            self.root()
        } else {
            self.root().join(REVIEWER_ROLES_DIR).join(role)
        }
    }

    /// The harness home one role runs under: its own copy of the staged
    /// profile, so concurrent roles never share a config home.
    #[must_use]
    fn role_profile_home(&self, role: &str) -> PathBuf {
        if role == DEFAULT_ROLE {
            self.root().join(DEFAULT_RUNTIME_PROFILE_DIR)
        } else {
            self.role_root(role).join(REVIEWER_PROFILE_DIR)
        }
    }

    /// Relay session id for one role. The default role keeps the historical
    /// suffix used by the controller's projection.
    #[must_use]
    fn relay_session_id(&self, role: &str) -> String {
        if role == DEFAULT_ROLE {
            format!("{}-reviewer", self.session_id)
        } else {
            format!("{}-review-{role}", self.session_id)
        }
    }
}

/// The reviewer's live process and the tasks driving it.
struct RunningReviewer {
    config: ReviewerLaunchConfig,
    commands: mpsc::Sender<CommandRequest>,
    coordinator_wake: mpsc::Sender<()>,
    runtime: JoinHandle<Result<()>>,
    shutdown: CancellationToken,
    _admission: ReviewerAdmission,
}

impl Drop for RunningReviewer {
    fn drop(&mut self) {
        // A sidecar dropped during exceptional worker teardown still asks its
        // runtime to reap the supervisor and harness group cooperatively.
        self.shutdown.cancel();
    }
}

enum ReviewerLifecycle {
    Stopped,
    Preparing { generation: u64 },
    Running(RunningReviewer),
    Stopping(RunningReviewer),
}

impl ReviewerLifecycle {
    fn generation(&self) -> Option<u64> {
        match self {
            Self::Preparing { generation } => Some(*generation),
            Self::Running(running) | Self::Stopping(running) => Some(running.config.generation),
            Self::Stopped => None,
        }
    }

    fn running(&self) -> Option<&RunningReviewer> {
        match self {
            Self::Running(running) => Some(running),
            _ => None,
        }
    }
}

/// The reviewer's harness process, relay and private copy of the staged profile.
struct ReviewerRole {
    role: String,
    placement: ReviewerPlacement,
    primary_relay: Arc<Mutex<DurableRelay>>,
    relay: Option<Arc<Mutex<DurableRelay>>>,
    lifecycle: ReviewerLifecycle,
    /// Cancellation belongs to the admitted operation, not its socket future.
    request_cancel: CancellationToken,
    /// Distinguishes the configuration commands this role submits itself.
    config_sequence: u64,
    #[cfg(test)]
    preparation_pause: Option<(
        tokio::sync::oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
}

/// Owns every role beside one primary worker.
pub struct ReviewerSidecar {
    placement: ReviewerPlacement,
    primary_relay: Arc<Mutex<DurableRelay>>,
    roles:
        std::sync::Mutex<std::collections::BTreeMap<String, Arc<tokio::sync::Mutex<ReviewerRole>>>>,
    /// Admitted role operations outlive their requesting socket. A role lock
    /// moves into its task, so cancellation cannot release it over a live
    /// blocking mutation. Only one operation per role is spawned at a time.
    operations: Mutex<JoinSet<()>>,
    shutdown: CancellationToken,
}

impl ReviewerSidecar {
    #[must_use]
    pub fn new(placement: ReviewerPlacement, primary_relay: Arc<Mutex<DurableRelay>>) -> Self {
        Self {
            placement,
            primary_relay,
            roles: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            operations: Mutex::new(JoinSet::new()),
            shutdown: CancellationToken::new(),
        }
    }

    #[cfg(test)]
    pub(super) async fn pause_preparation_for_test(
        &self,
        role: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (proceed, blocked) = std::sync::mpsc::channel();
        self.role(role).lock().await.preparation_pause = Some((entered, blocked));
        (observed, proceed)
    }

    /// The roles this sidecar has touched, in a stable order.
    #[must_use]
    pub fn known_roles(&self) -> Vec<String> {
        self.roles
            .lock()
            .expect("reviewer role map lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// The role's own state, created on first use.
    fn role(&self, role: &str) -> Arc<tokio::sync::Mutex<ReviewerRole>> {
        self.roles
            .lock()
            .expect("reviewer role map lock poisoned")
            .entry(role.to_owned())
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(ReviewerRole {
                    role: role.to_owned(),
                    placement: self.placement.clone(),
                    primary_relay: self.primary_relay.clone(),
                    relay: None,
                    lifecycle: ReviewerLifecycle::Stopped,
                    request_cancel: CancellationToken::new(),
                    config_sequence: 0,
                    #[cfg(test)]
                    preparation_pause: None,
                }))
            })
            .clone()
    }

    /// Serves one reviewer request. Every variant answers with an ordinary
    /// relay response, so the controller decodes reviewer replies with the
    /// code it already uses for the primary.
    #[cfg(test)]
    pub async fn handle(
        &self,
        envelope: RelayRequestEnvelope,
        role: Option<String>,
        request: mj_core::relay::ReviewerRequest,
    ) -> RelayResponseEnvelope {
        self.handle_cancellable(
            envelope,
            role,
            request,
            std::future::pending::<ReviewerCancellation>(),
        )
        .await
    }

    /// The connection may stop waiting, but the admitted operation retains
    /// ownership until every blocking mutation and process cleanup settles.
    pub(super) async fn handle_cancellable<F>(
        &self,
        envelope: RelayRequestEnvelope,
        role: Option<String>,
        request: mj_core::relay::ReviewerRequest,
        disconnected: F,
    ) -> RelayResponseEnvelope
    where
        F: std::future::Future<Output = ReviewerCancellation>,
    {
        let request_id = envelope.request_id;
        let protocol_version = envelope.protocol_version;
        let role = role.unwrap_or_else(|| DEFAULT_ROLE.to_owned());
        let body = self.dispatch(&role, request, disconnected).await;
        let body = match body {
            Ok(body) => body,
            Err(error) => reviewer_error(format!("{error:#}")),
        };
        RelayResponseEnvelope {
            request_id,
            protocol_version,
            body,
        }
    }

    /// Stops every role. Called when the worker's session closes, so
    /// no reviewing harness outlives the session it was reviewing for.
    pub async fn pause_all(&self) {
        self.shutdown.cancel();
        let names = self.known_roles();
        if !names.is_empty() {
            tracing::debug!(roles = ?names, "stopping every reviewing role");
        }
        let roles = self
            .roles
            .lock()
            .expect("reviewer role map lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut stops = JoinSet::new();
        for role in roles {
            stops.spawn(async move {
                let mut role = role.lock().await;
                if let Err(error) = role.pause().await {
                    tracing::error!(%error, "reviewer shutdown did not complete before its response deadline");
                }
                role.finish_shutdown().await
            });
        }
        while let Some(result) = stops.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "reviewer final cleanup failed"),
                Err(error) => tracing::error!(%error, "reviewer final cleanup task failed"),
            }
        }
        let mut operations = self
            .operations
            .lock()
            .expect("reviewer operations lock poisoned");
        while let Some(joined) = operations.try_join_next() {
            if let Err(error) = joined {
                tracing::error!(%error, "reviewer operation failed during shutdown");
            }
        }
    }

    async fn dispatch(
        &self,
        role: &str,
        request: mj_core::relay::ReviewerRequest,
        disconnected: impl std::future::Future<Output = ReviewerCancellation>,
    ) -> Result<RelayResponseBody> {
        use mj_core::relay::ReviewerRequest;
        tokio::pin!(disconnected);
        let handle = self.role(role);
        let mut role = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => bail!("reviewer sidecar is stopping"),
            reason = &mut disconnected => bail!("{} before admission", reason.message()),
            role = handle.lock_owned() => role,
        };
        let operation_admission = match &request {
            ReviewerRequest::Pause
            | ReviewerRequest::PauseGeneration { .. }
            | ReviewerRequest::Attach { .. }
            | ReviewerRequest::Acknowledge { .. }
            | ReviewerRequest::Status
            | ReviewerRequest::RespondElicitation { .. } => None,
            _ => Some(ReviewerAdmission::acquire(
                self.primary_relay.clone(),
                format!("reviewer operation {}", role.role),
            )?),
        };
        let cancellation = self.shutdown.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        role.request_cancel = cancellation.clone();
        let (respond, response) = tokio::sync::oneshot::channel();
        // Journal reads and already accepted mutations never own the live
        // reviewer's lifetime. Losing their reply cannot cancel its turn.
        let owns_runtime = matches!(request, ReviewerRequest::Start { .. });
        {
            let mut operations = self
                .operations
                .lock()
                .expect("reviewer operations lock poisoned");
            anyhow::ensure!(
                !self.shutdown.is_cancelled(),
                "reviewer sidecar is stopping"
            );
            while let Some(joined) = operations.try_join_next() {
                if let Err(error) = joined {
                    tracing::error!(%error, "reviewer operation task failed");
                }
            }
            operations.spawn(async move {
                let _operation_admission = operation_admission;
                let result = role.dispatch(request).await;
                if owns_runtime && role.request_cancel.is_cancelled() {
                    if let Err(error) = role.pause().await {
                        tracing::error!(%error, "cancelled reviewer operation remains stopping");
                    }
                } else if matches!(role.lifecycle, ReviewerLifecycle::Preparing { .. }) {
                    role.lifecycle = ReviewerLifecycle::Stopped;
                }
                if let Err(result) = respond.send(result)
                    && let Err(error) = result
                {
                    tracing::warn!(%error, "reviewer operation failed after its client disconnected");
                }
            });
        }
        tokio::select! {
            biased;
            result = response => result.context("reviewer operation task stopped")?,
            reason = &mut disconnected => {
                cancellation.cancel();
                bail!("{}; admitted cleanup continues", reason.message());
            }
        }
    }
}

impl ReviewerRole {
    async fn dispatch(
        &mut self,
        request: mj_core::relay::ReviewerRequest,
    ) -> Result<RelayResponseBody> {
        use mj_core::relay::ReviewerRequest;

        match request {
            ReviewerRequest::Start { config } => self.start(*config).await,
            ReviewerRequest::PauseGeneration { generation } => {
                if self.lifecycle.generation() == Some(generation) {
                    self.pause().await?;
                }
                Ok(RelayResponseBody::Ok {
                    payload: RelayResponsePayload::ReviewerPaused,
                })
            }
            ReviewerRequest::Pause => {
                self.pause().await?;
                Ok(RelayResponseBody::Ok {
                    payload: RelayResponsePayload::ReviewerPaused,
                })
            }
            ReviewerRequest::Attach {
                after_ordinal,
                after_digest,
            } => self.forward(RelayRequest::Attach {
                after_ordinal,
                after_digest,
            }),
            ReviewerRequest::Acknowledge {
                through_ordinal,
                through_digest,
            } => self.forward(RelayRequest::Acknowledge {
                through_ordinal,
                through_digest,
            }),
            ReviewerRequest::Submit {
                command_id,
                command,
            } => {
                let response = self.forward(RelayRequest::Submit {
                    command_id,
                    command,
                })?;
                self.wake_coordinator();
                Ok(response)
            }
            ReviewerRequest::Status => self.forward(RelayRequest::Status),
            ReviewerRequest::RespondElicitation {
                elicitation_id,
                response,
            } => self.respond_elicitation(elicitation_id, response).await,
            ReviewerRequest::CaptureDelta { baselines } => self.capture_delta(baselines).await,
            ReviewerRequest::AdvanceBaseline { trees } => self.advance_baseline(trees).await,
        }
    }

    /// The workspace repositories a review covers, discovered from the
    /// primary's working directory and additional roots.
    fn review_repositories(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.placement.cwd.clone()];
        roots.extend(self.placement.additional_directories.iter().cloned());
        crate::review::capture::discover_repositories(&mj_checkpoint::archive::SystemGit, &roots)
    }

    /// Reports what every workspace repository changed since `baselines`.
    ///
    /// Git work is blocking and can take a moment on a large tree, so it runs
    /// on the blocking pool rather than on the runtime that also serves the
    /// primary session's relay.
    async fn capture_delta(
        &mut self,
        baselines: std::collections::BTreeMap<PathBuf, String>,
    ) -> Result<RelayResponseBody> {
        anyhow::ensure!(
            self.placement.review_capture,
            "this session was started without a review baseline, so a review \
             cannot tell its work from what the working tree already held; \
             sub-agent children are never reviewed, and an ordinary session \
             needs an eligible reviewer in Settings before it starts; resume or restart the session after configuring one"
        );
        let repositories = self.review_repositories();
        let untracked_at_start = self
            .placement
            .untracked_at_start
            .lock()
            .expect("untracked-at-start lock poisoned")
            .clone();
        let repositories = tokio::task::spawn_blocking(move || {
            crate::review::capture::capture_repository_deltas(
                &mj_checkpoint::archive::SystemGit,
                &repositories,
                &baselines,
                &untracked_at_start,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review capture stopped: {error}"))??;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewDelta { repositories },
        })
    }

    /// Records the trees a completed review reviewed through.
    async fn advance_baseline(
        &mut self,
        trees: std::collections::BTreeMap<PathBuf, String>,
    ) -> Result<RelayResponseBody> {
        // The untracked list has to move with the tree it belongs to, or the
        // next review would report every file this turn created as new work
        // all over again. Re-reading the state is one stat-walk per completed
        // review, which is what the new capture costs anyway.
        let repositories = self.review_repositories();
        let refreshed = tokio::task::spawn_blocking(move || {
            crate::review::capture::advance_baselines(&mj_checkpoint::archive::SystemGit, &trees)?;
            crate::review::capture::read_untracked_at_start(
                &mj_checkpoint::archive::SystemGit,
                &repositories,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review baseline update stopped: {error}"))??;
        *self
            .placement
            .untracked_at_start
            .lock()
            .expect("untracked-at-start lock poisoned") = refreshed;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewBaselineAdvanced,
        })
    }
}

impl ReviewerRole {
    /// Answers a form the reviewer's harness is waiting on.
    ///
    /// The answer goes straight to the reviewer's ACP runtime, never through
    /// its command queue. Only the rendered, masked reply enters the ledger.
    async fn respond_elicitation(
        &mut self,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) -> Result<RelayResponseBody> {
        let Some(running) = self.lifecycle.running() else {
            bail!("no reviewer is running to answer that form");
        };
        let (resolved, resolution) = tokio::sync::oneshot::channel();
        running
            .commands
            .send(CommandRequest::ResolveElicitation {
                elicitation_id: elicitation_id.clone(),
                response,
                resolved,
            })
            .await
            .map_err(|_| anyhow::anyhow!("the reviewer runtime stopped before it could answer"))?;
        match resolution.await {
            Ok(Ok(())) => Ok(RelayResponseBody::Ok {
                payload: RelayResponsePayload::ElicitationResolved { elicitation_id },
            }),
            Ok(Err(message)) => bail!("{message}"),
            Err(_) => bail!("the reviewer runtime stopped before it answered"),
        }
    }

    /// Starts the reviewer, or reports the running one when it already matches
    /// `config`. A configuration that names a different profile or a newer
    /// generation replaces the running reviewer rather than reusing it.
    async fn start(&mut self, config: ReviewerLaunchConfig) -> Result<RelayResponseBody> {
        let profile_home = self.placement.staged_profile_home(config.generation);
        if !profile_home.is_dir() {
            bail!(
                "reviewer profile has not been staged at {}",
                profile_home.display()
            );
        }
        if matches!(self.lifecycle, ReviewerLifecycle::Stopping(_)) {
            self.pause().await?;
        }
        let reused = match self.lifecycle.running() {
            Some(running) if running.config.reusable_for(&config) => true,
            Some(_) => {
                // A different profile or a new generation is a different
                // reviewer. Stop the old process group before its replacement
                // touches the same staged directory.
                self.pause().await?;
                false
            }
            None => false,
        };
        if !reused {
            self.launch(&config).await?;
        }
        self.request_plan_mode(&config).await;
        self.apply_configuration(&config).await?;
        let state = self.state()?;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewerStarted {
                native_session_id: state.native_session_id.clone(),
                config_options: state.config_options.clone(),
                reused,
                state: Box::new(state),
            },
        })
    }

    /// Spawns the reviewer's harness and waits for it to open a session and
    /// advertise its configuration.
    async fn launch(&mut self, config: &ReviewerLaunchConfig) -> Result<()> {
        let admission = ReviewerAdmission::acquire(
            self.primary_relay.clone(),
            format!("reviewer runtime {}", self.role),
        )?;
        self.lifecycle = ReviewerLifecycle::Preparing {
            generation: config.generation,
        };
        let root = self.placement.role_root(&self.role);
        std::fs::create_dir_all(&root)
            .with_context(|| format!("create reviewer root {}", root.display()))?;
        // The staged profile is a source snapshot. It is never the harness's
        // home: replacing it for another role or generation therefore cannot
        // delete files from a running harness.
        let generation_changed = self.prepare_generation(&root, config).await?;
        self.check_cancelled()?;
        let profile_home = self
            .role_profile_home(config.harness, config.generation, generation_changed)
            .await?;
        self.check_cancelled()?;
        if generation_changed {
            Self::commit_generation(&root, config).await?;
        }
        let relay = self.open_relay()?;

        let mut environment = self.placement.target_environment.clone();
        environment.extend(config.environment.clone());
        // The worker fixes the harness home itself: a controller must never be
        // able to aim a reviewer at the primary's credentials.
        config
            .harness
            .configure_home_environment(&profile_home, &mut environment);
        config
            .harness
            .configure_execution_environment(config.execution_policy, &mut environment)?;
        let prepared_harness = super::prepare_harness_launch(
            config.harness,
            self.placement.harness_runtime,
            config.execution_policy,
            AcpSupervisorSpec {
                command: config.bridge_command.clone(),
                args: config.bridge_args.clone(),
                environment,
                excluded_environment: config.excluded_environment.clone(),
                cwd: self.placement.cwd.clone(),
                harness_lease: None,
            },
        )
        .await
        .with_context(|| format!("prepare reviewer {}", config.harness.display_name()))?;
        let session_environment = prepared_harness.environment.clone();
        // A Codex reviewer needs its recorded model at launch for the same
        // reason the primary session does. The ACP runtime further down pins
        // it into the spec below before every bridge start; this value is what
        // that runtime starts from.
        let accepted_config = {
            let mut relay = relay.lock().expect("reviewer relay lock poisoned");
            relay.set_turn_verdict_harness(config.harness);
            let state = relay.operational_state();
            acp::AcceptedSessionConfig::from_configuration(&state.config, &state.config_options)
        };
        let supervisor_path = root.join("acp-supervisor.json");
        prepared_harness.spec.write_spec(&supervisor_path)?;

        // Captured before the harness starts, so a later scan sees only what
        // this launch produced and never an earlier one's events.
        let cursor = {
            let relay = relay.lock().expect("reviewer relay lock poisoned");
            RelayCursor {
                ordinal: relay.latest_ordinal(),
                digest: relay.latest_digest().to_owned(),
            }
        };
        // One acquisition: a guard taken inside the struct literal below
        // would live until the literal ends and deadlock the next one.
        let (
            resume_session,
            native_session_may_have_history,
            acp_activity,
            step_clock,
            tools_in_flight,
            turn_context,
            accepted_config,
        ) = {
            let relay = relay.lock().expect("reviewer relay lock poisoned");
            let state = relay.operational_state();
            (
                state.native_session_id,
                relay.native_session_may_have_history(),
                relay.acp_activity_clock(),
                relay.step_clock(),
                relay.tools_in_flight(),
                relay.turn_context(),
                Arc::new(Mutex::new(accepted_config)),
            )
        };
        let spec = LaunchSpec {
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: self.placement.worker_executable.clone(),
            args: vec![
                "--login-environment-ready".into(),
                "worker".into(),
                "acp-supervisor".into(),
                "--spec".into(),
                supervisor_path.to_string_lossy().into_owned(),
            ],
            environment: session_environment,
            bridge_spec_path: Some(supervisor_path.clone()),
            cwd: self.placement.cwd.clone(),
            additional_directories: self.placement.additional_directories.clone(),
            // A reviewer reads the workspace; it never syncs project memory,
            // which belongs to the primary session alone.
            project_memory: None,
            extra_mcp_servers: config
                .mcp_servers
                .iter()
                .cloned()
                .map(crate::acp::ReviewerMcpServer::new)
                .collect(),
            subagent_policy: mj_core::subagent::SubagentPolicy::Native,
            subagent_mcp_socket: None,
            resume_session,
            native_session_may_have_history,
            accepted_config,
            // The reviewer's chosen model, which it opens on rather than
            // switching to after the session starts.
            initial_model: config.model.clone(),
            harness: config.harness,
            execution_policy: config.execution_policy,
            acp_activity,
            step_clock,
            tools_in_flight,
            turn_context,
            verdict: None,
            stall_policy: None,
        };

        let (commands_tx, commands_rx) = mpsc::channel(32);
        let (events_tx, events_rx) = mpsc::channel(ACP_EVENT_CHANNEL_CAPACITY);
        let (wake_tx, wake_rx) = mpsc::channel(1);
        self.check_cancelled()?;
        let shutdown = CancellationToken::new();
        let runtime_shutdown = shutdown.clone();
        let coordinator_commands = commands_tx.clone();
        let runtime = tokio::spawn(async move {
            let mut acp = tokio::spawn(acp::run_with_shutdown(
                spec,
                commands_rx,
                events_tx,
                runtime_shutdown.clone(),
            ));
            let mut coordinator = tokio::spawn(run_relay_coordinator(
                relay,
                events_rx,
                wake_rx,
                coordinator_commands,
            ));
            // Join handles turn a coordinator panic into a supervised exit:
            // cleanup must still run in the ACP owner instead of unwinding it.
            let (acp_result, coordinator_result) = tokio::select! {
                result = &mut acp => (result, coordinator.await),
                result = &mut coordinator => {
                    runtime_shutdown.cancel();
                    (acp.await, result)
                }
            };
            let result = (|| -> Result<()> {
                acp_result
                    .context("reviewer ACP task stopped")?
                    .context("reviewer ACP runtime stopped")?;
                coordinator_result
                    .context("reviewer coordinator task stopped")?
                    .context("reviewer coordinator stopped")
            })();
            if let Err(error) = &result {
                tracing::error!(error = %format!("{error:#}"), "reviewer runtime failed");
            }
            result
        });
        self.lifecycle = ReviewerLifecycle::Running(RunningReviewer {
            config: config.clone(),
            commands: commands_tx,
            coordinator_wake: wake_tx,
            runtime,
            shutdown,
            _admission: admission,
        });

        // The relay is the durable truth about the harness, so readiness is
        // read from it rather than from a side channel that a restart would
        // not reproduce. `SessionConfigured` is the event to wait for, not a
        // non-empty option list: a harness that advertises no selectors is
        // ready too, and the waterfall offers it a harness default.
        let ready = self
            .wait_for_observation(START_TIMEOUT, &cursor, |observation| {
                matches!(observation, RelayObservation::SessionConfigured { .. })
            })
            .await;
        if ready.is_err() {
            let failure = self.failure_since(&cursor).unwrap_or_else(|| {
                "the reviewer harness did not open a session in time".to_owned()
            });
            self.pause().await?;
            bail!("{failure}");
        }
        Ok(())
    }

    /// Asks the reviewer's harness for plan mode when it has one.
    ///
    /// This is a request, not a guarantee: Hel does not claim the reviewer is
    /// read-only, and its prompt says not to implement for the same reason. A
    /// harness with no plan mode simply keeps the one it has.
    async fn request_plan_mode(&mut self, config: &ReviewerLaunchConfig) {
        // Muse's Plan mode disables the shell, and the reviewer reads the
        // change with `git diff`.
        if config.harness == mj_core::config::HarnessKind::Muse {
            return;
        }
        let Ok(state) = self.state() else {
            return;
        };
        // The same harness-aware decision the primary's /plan uses, so a
        // reviewer asks for plan mode exactly the way a session does.
        let mut surface = crate::acp::surface::AcpSessionSurface::default();
        surface.set_harness_kind(config.harness);
        surface.set_config_options(&state.config_options);
        surface.set_session_modes(state.modes.clone());
        let Ok(control) = surface.plan_control(true) else {
            return;
        };
        let command = match control {
            crate::acp::PlanControl::SetConfig { key, value } => {
                RelayCommand::SetConfig { key, value }
            }
            crate::acp::PlanControl::SetSessionMode { mode_id } => {
                RelayCommand::SetSessionMode { mode_id }
            }
            crate::acp::PlanControl::RestoreExecutionMode => RelayCommand::RestoreExecutionMode,
        };
        self.config_sequence += 1;
        let command_id = format!("reviewer-plan-mode-{}", self.config_sequence);
        let cursor = match self.cursor() {
            Ok(cursor) => cursor,
            Err(_) => return,
        };
        if self
            .forward(RelayRequest::Submit {
                command_id: command_id.clone(),
                command,
            })
            .is_err()
        {
            return;
        }
        self.wake_coordinator();
        // A harness that refuses plan mode is not a failure: the review still
        // runs, and the prompt is what actually asks the reviewer not to act.
        let _ = self
            .wait_for_observation(CONFIGURE_TIMEOUT, &cursor, |observation| {
                matches!(
                    observation,
                    RelayObservation::CommandCompleted { command_id: done, .. }
                        | RelayObservation::CommandRejected { command_id: done, .. }
                        | RelayObservation::CommandInterrupted { command_id: done, .. }
                    if *done == command_id
                )
            })
            .await;
    }

    /// Applies the chosen model and effort on the live reviewer. A `None`
    /// choice means the harness advertises no such selector, so nothing is
    /// sent: the reviewer keeps whatever its profile configures.
    async fn apply_configuration(&mut self, config: &ReviewerLaunchConfig) -> Result<()> {
        for (key, value) in [("model", &config.model), ("effort", &config.effort)] {
            if let Some(value) = value {
                self.apply_setting(key, value).await?;
            }
        }
        if let Some(enabled) = config.fast_mode {
            let state = self.state()?;
            let facts = mj_core::acp::AcpSessionFacts::from_operational(
                config.harness,
                &state.config,
                &state.config_options,
                state.modes.as_ref(),
            );
            if facts.supports_fast_mode()
                && let Err(error) = self
                    .apply_setting("fast-mode", if enabled { "on" } else { "off" })
                    .await
            {
                tracing::warn!(role = %self.role, %error, "reviewer fast mode unavailable; continuing at standard speed");
            }
        }
        Ok(())
    }

    async fn apply_setting(&mut self, key: &str, value: &str) -> Result<()> {
        if self
            .state()?
            .config
            .get(key)
            .is_some_and(|current| current == value)
        {
            return Ok(());
        }
        self.config_sequence += 1;
        let command_id = format!("reviewer-{key}-{}", self.config_sequence);
        let cursor = self.cursor()?;
        let body = self.forward(RelayRequest::Submit {
            command_id: command_id.clone(),
            command: RelayCommand::SetConfig {
                key: key.to_owned(),
                value: value.to_owned(),
            },
        })?;
        if let RelayResponseBody::Error { error } = body {
            bail!(
                "reviewer could not accept {key} {value:?}: {}",
                error.message
            );
        }
        self.wake_coordinator();
        // Waiting for the command's own completion, not for the value in
        // the relay's configuration map, is what makes the refreshed
        // option list part of the answer: the runtime records the value,
        // then the refreshed options, then the completion.
        let settled = self
            .wait_for_observation(CONFIGURE_TIMEOUT, &cursor, |observation| {
                matches!(
                    observation,
                    RelayObservation::CommandCompleted { command_id: done, .. }
                        | RelayObservation::CommandRejected { command_id: done, .. }
                        | RelayObservation::CommandInterrupted { command_id: done, .. }
                    if *done == command_id
                )
            })
            .await;
        let applied = self.state()?.config.get(key) == Some(&value.to_owned());
        if settled.is_err() || !applied {
            let failure = self
                .failure_since(&cursor)
                .unwrap_or_else(|| format!("the reviewer did not apply {key} {value:?}"));
            bail!("{failure}");
        }
        Ok(())
    }

    /// Cancels any turn in flight and stops the reviewer's process group,
    /// keeping its staged profile, native session and journal.
    pub async fn pause(&mut self) -> Result<()> {
        if matches!(self.lifecycle, ReviewerLifecycle::Running(_)) {
            self.config_sequence += 1;
            let command_id = format!("reviewer-cancel-{}", self.config_sequence);
            if self
                .forward(RelayRequest::Submit {
                    command_id,
                    command: RelayCommand::Cancel,
                })
                .is_ok()
            {
                self.wake_coordinator();
                if !self.request_cancel.is_cancelled() {
                    let _ = self
                        .wait_for(CANCEL_TIMEOUT, |state| state.active_prompt.is_none())
                        .await;
                }
            }
            let ReviewerLifecycle::Running(running) =
                std::mem::replace(&mut self.lifecycle, ReviewerLifecycle::Stopped)
            else {
                unreachable!("running lifecycle was checked")
            };
            running.shutdown.cancel();
            self.lifecycle = ReviewerLifecycle::Stopping(running);
        }
        let ReviewerLifecycle::Stopping(running) = &mut self.lifecycle else {
            self.lifecycle = ReviewerLifecycle::Stopped;
            return Ok(());
        };
        // Borrow the handle: a timeout leaves the task and process ownership
        // in Stopping. No replacement may touch this role's files
        // until a later pause observes the runtime's actual completion.
        let joined = tokio::time::timeout(PAUSE_TIMEOUT, &mut running.runtime)
            .await
            .context("reviewer is still stopping; its files remain reserved")?;
        self.lifecycle = ReviewerLifecycle::Stopped;
        joined.context("reviewer runtime task stopped abnormally")??;
        Ok(())
    }

    /// Worker exit cannot abandon a retained cleanup owner just because a
    /// client's pause deadline elapsed. Admission and files remain held until
    /// the already-cancelled runtime has actually reaped its processes.
    async fn finish_shutdown(&mut self) -> Result<()> {
        let ReviewerLifecycle::Stopping(running) = &mut self.lifecycle else {
            return Ok(());
        };
        let result = (&mut running.runtime).await;
        self.lifecycle = ReviewerLifecycle::Stopped;
        result.context("reviewer final runtime task stopped")??;
        Ok(())
    }

    fn check_cancelled(&self) -> Result<()> {
        anyhow::ensure!(
            !self.request_cancel.is_cancelled(),
            "reviewer operation cancelled"
        );
        Ok(())
    }

    /// This role's own harness home, refreshed from the staged profile when it
    /// is missing or belongs to an older reviewer generation.
    ///
    /// The controller stages one immutable snapshot of the chosen profile;
    /// every role runs from a copy of that snapshot, so concurrent harnesses
    /// never share a config home. The role marker keeps a compatible resume
    /// from re-copying a large profile while still refreshing it when the
    /// reviewer's lifetime changes. The role's profile directory is what the
    /// harness's home variable names, so a Muse home is its `muse` child; the
    /// whole directory is replaced, so nothing an earlier reviewer's harness
    /// left there survives into the next one's home variable.
    async fn role_profile_home(
        &mut self,
        harness: HarnessKind,
        generation: u64,
        generation_changed: bool,
    ) -> Result<PathBuf> {
        let root = self.placement.role_profile_home(&self.role);
        let home = harness.home_from_environment(&root);
        let source = self.placement.staged_profile_home(generation);
        let result_home = home.clone();
        #[cfg(test)]
        let preparation_pause = self.preparation_pause.take();
        tokio::task::spawn_blocking(move || -> Result<()> {
            #[cfg(test)]
            if let Some((entered, proceed)) = preparation_pause {
                let _ = entered.send(());
                proceed
                    .recv()
                    .context("release reviewer preparation fixture")?;
            }
            if !generation_changed && home.is_dir() {
                return Ok(());
            }
            let parent = root.parent().context("reviewer home has no parent")?;
            std::fs::create_dir_all(parent)?;
            let staging = tempfile::Builder::new()
                .prefix(".reviewer-profile-")
                .tempdir_in(parent)?;
            copy_tree(&source, &harness.home_from_environment(staging.path()))
                .with_context(|| format!("stage the reviewer profile for {}", home.display()))?;
            if root.exists() {
                std::fs::remove_dir_all(&root)
                    .with_context(|| format!("clear the reviewer role home {}", root.display()))?;
            }
            std::fs::rename(staging.path(), &root)
                .with_context(|| format!("publish the reviewer profile {}", root.display()))?;
            Ok(())
        })
        .await
        .map_err(|error| anyhow::anyhow!("reviewer profile copy stopped: {error}"))??;
        Ok(result_home)
    }

    /// Starts a new relay conversation when the role's profile or generation
    /// changes. The running process is already paused by `start` before this
    /// method is reached, so moving its files cannot race a live writer.
    async fn prepare_generation(
        &mut self,
        root: &std::path::Path,
        config: &ReviewerLaunchConfig,
    ) -> Result<bool> {
        let marker = root.join(ROLE_GENERATION_MARKER);
        let identity = format!(
            "{}:{}:{:?}",
            config.generation, config.profile_id, config.harness
        );
        let previous = tokio::task::spawn_blocking({
            let marker = marker.clone();
            move || std::fs::read_to_string(marker).ok()
        })
        .await
        .map_err(|error| anyhow::anyhow!("read reviewer generation stopped: {error}"))?;
        if previous.as_deref() == Some(identity.as_str()) {
            return Ok(false);
        }

        // Do not keep an in-memory relay pointing at files that are about to
        // move. Dropping it before the archive is what makes the next open
        // use the fresh state on disk.
        self.relay.take();
        let root = root.to_owned();
        let archive_identity = previous.clone().unwrap_or_else(|| "legacy".to_owned());
        tokio::task::spawn_blocking(move || -> Result<()> {
            archive_relay(&root, &archive_identity)?;
            Ok(())
        })
        .await
        .map_err(|error| anyhow::anyhow!("archive reviewer generation stopped: {error}"))??;
        Ok(true)
    }

    /// Marks a generation only after its private profile home was copied
    /// successfully. If the copy fails, the next attempt still sees the old
    /// marker and retries the archive and copy instead of trusting a partial
    /// home.
    fn commit_generation(
        root: &std::path::Path,
        config: &ReviewerLaunchConfig,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let marker = root.join(ROLE_GENERATION_MARKER);
        let identity = format!(
            "{}:{}:{:?}",
            config.generation, config.profile_id, config.harness
        );
        async move {
            tokio::task::spawn_blocking(move || {
                mj_core::config::atomic_write(&marker, identity.as_bytes())
                    .with_context(|| format!("record reviewer generation at {}", marker.display()))
            })
            .await
            .map_err(|error| anyhow::anyhow!("record reviewer generation stopped: {error}"))??;
            Ok(())
        }
    }

    /// Opens the reviewer's relay, creating its journal on first use.
    fn open_relay(&mut self) -> Result<Arc<Mutex<DurableRelay>>> {
        if let Some(relay) = &self.relay {
            return Ok(relay.clone());
        }
        let relay = DurableRelay::open(
            self.placement.role_root(&self.role),
            self.placement.relay_session_id(&self.role),
            env!("CARGO_PKG_VERSION"),
        )
        .context("open the reviewer relay")?;
        let relay = Arc::new(Mutex::new(relay));
        self.relay = Some(relay.clone());
        Ok(relay)
    }

    /// Hands one request to the reviewer's own relay.
    fn forward(&mut self, request: RelayRequest) -> Result<RelayResponseBody> {
        let relay = self.open_relay()?;
        let envelope = RelayRequestEnvelope {
            request_id: format!("reviewer-{}", self.role),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request,
        };
        let response = relay
            .lock()
            .expect("reviewer relay lock poisoned")
            .handle(envelope);
        Ok(response.body)
    }

    fn state(&mut self) -> Result<RelayOperationalState> {
        let relay = self.open_relay()?;
        let state = relay
            .lock()
            .expect("reviewer relay lock poisoned")
            .operational_state();
        Ok(state)
    }

    fn wake_coordinator(&self) {
        if let Some(running) = self.lifecycle.running() {
            let _ = running.coordinator_wake.try_send(());
        }
    }

    /// Waits until the reviewer's durable state satisfies `ready`, or until
    /// the runtime stops or the deadline passes.
    async fn wait_for(
        &mut self,
        timeout: Duration,
        ready: impl Fn(&RelayOperationalState) -> bool,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.check_cancelled()?;
            if ready(&self.state()?) {
                return Ok(());
            }
            if self
                .lifecycle
                .running()
                .is_none_or(|running| running.runtime.is_finished())
            {
                bail!("the reviewer runtime stopped");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("timed out waiting for the reviewer");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// A cursor at the reviewer relay's current frontier, for scanning only
    /// the events an operation is about to produce.
    fn cursor(&mut self) -> Result<RelayCursor> {
        let relay = self.open_relay()?;
        let relay = relay.lock().expect("reviewer relay lock poisoned");
        Ok(RelayCursor {
            ordinal: relay.latest_ordinal(),
            digest: relay.latest_digest().to_owned(),
        })
    }

    /// Everything the reviewer journaled after `cursor`.
    fn events_since(&mut self, cursor: &RelayCursor) -> Vec<RelayEvent> {
        let Some(relay) = self.relay.as_ref().cloned() else {
            return Vec::new();
        };
        let relay = relay.lock().expect("reviewer relay lock poisoned");
        relay
            .events_after(cursor.ordinal, &cursor.digest)
            .unwrap_or_default()
    }

    /// Why the operation that started at `cursor` failed, as the reviewer's
    /// own runtime recorded it. Reporting the harness's words beats reporting
    /// that Hel gave up waiting.
    fn failure_since(&mut self, cursor: &RelayCursor) -> Option<String> {
        self.events_since(cursor)
            .iter()
            .rev()
            .find_map(|event| match &event.observation {
                RelayObservation::Warning { message } => Some(message.clone()),
                RelayObservation::CommandRejected { message, .. }
                | RelayObservation::CommandInterrupted { message, .. } => Some(message.clone()),
                _ => None,
            })
    }

    /// Waits until the reviewer journals an observation matching `wanted`
    /// after `cursor`, or until the runtime stops or the deadline passes.
    async fn wait_for_observation(
        &mut self,
        timeout: Duration,
        cursor: &RelayCursor,
        wanted: impl Fn(&RelayObservation) -> bool,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.check_cancelled()?;
            if self
                .events_since(cursor)
                .iter()
                .any(|event| wanted(&event.observation))
            {
                return Ok(());
            }
            if self
                .lifecycle
                .running()
                .is_none_or(|running| running.runtime.is_finished())
            {
                bail!("the reviewer runtime stopped");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("timed out waiting for the reviewer");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

/// Moves a role's old relay files out of the live root. A relay journal is
/// append-only while it is live, so deleting it would lose the previous
/// conversation and leave a stale native id available for a new generation.
fn archive_relay(root: &std::path::Path, identity: &str) -> Result<()> {
    let state = root.join(mj_core::relay::RELAY_STATE_FILE);
    let journal = root.join(mj_core::relay::RELAY_JOURNAL_DIR);
    if !state.exists() && !journal.exists() {
        return Ok(());
    }

    let archive_root = root.join(RELAY_ARCHIVE_DIR);
    std::fs::create_dir_all(&archive_root)
        .with_context(|| format!("create reviewer relay archive {}", archive_root.display()))?;
    let component = identity
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let component = if component.is_empty() {
        "legacy".to_owned()
    } else {
        component
    };
    let mut destination = archive_root.join(&component);
    let mut suffix = 0_u64;
    while destination.exists() {
        suffix = suffix.saturating_add(1);
        destination = archive_root.join(format!("{component}-{suffix}"));
    }
    std::fs::create_dir_all(&destination)
        .with_context(|| format!("create reviewer relay archive {}", destination.display()))?;
    if state.exists() {
        std::fs::rename(&state, destination.join(mj_core::relay::RELAY_STATE_FILE))
            .with_context(|| format!("archive reviewer relay state {}", state.display()))?;
    }
    if journal.exists() {
        std::fs::rename(
            &journal,
            destination.join(mj_core::relay::RELAY_JOURNAL_DIR),
        )
        .with_context(|| format!("archive reviewer relay journal {}", journal.display()))?;
    }
    Ok(())
}

/// Recursively copies a staged profile into the reviewer's private home.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_tree(&source, &destination)?;
        } else if metadata.is_symlink() {
            // A staged profile's symlink is copied as its target's content:
            // the role's home must not point back at a directory another role
            // owns.
            let resolved = std::fs::canonicalize(&source)?;
            if resolved.is_dir() {
                copy_tree(&resolved, &destination)?;
            } else {
                std::fs::copy(&resolved, &destination)?;
            }
        } else {
            std::fs::copy(&source, &destination).with_context(|| {
                format!("copy {} to {}", source.display(), destination.display())
            })?;
        }
    }
    Ok(())
}

fn reviewer_error(message: String) -> RelayResponseBody {
    RelayResponseBody::Error {
        error: mj_core::relay::RelayProtocolError {
            code: mj_core::relay::RelayErrorCode::InvalidState,
            message,
            retryable: false,
            detail: None,
        },
    }
}

#[cfg(test)]
#[path = "reviewer/lifecycle_tests.rs"]
mod lifecycle_tests;
