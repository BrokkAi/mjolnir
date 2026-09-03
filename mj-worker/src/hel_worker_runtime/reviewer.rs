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
//! A sidecar holds several *roles*, not one reviewer. Plan review uses the one
//! called `reviewer`; a turn review in the extended tier also runs an intent
//! analyst, a supervisor, and the specialist lanes the supervisor launches.
//! Each role is a separate harness process with its own relay, journal and
//! copy of the staged profile, so two roles can never share a config home or
//! be mistaken for one another. Each role also locks independently: launching
//! a lane must not block the controller polling another role's journal.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::unix::{ACP_EVENT_CHANNEL_CAPACITY, run_relay_coordinator};
use super::{
    AcpSupervisorSpec, REVIEWER_DIR, REVIEWER_PROFILE_DIR, REVIEWER_ROLES_DIR, ReviewerLaunchConfig,
};
use hel::hel_acp::{self, CommandRequest, LaunchSpec};
use hel::hel_worker::{
    DurableRelay, RelayCommand, RelayCursor, RelayEvent, RelayObservation, RelayOperationalState,
    RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponseEnvelope,
    RelayResponsePayload,
};

/// How long a reviewer may take to open its native session and advertise its
/// configuration. A harness that has to authenticate or warm a large profile
/// is slow, but a harness that never answers must not hang the controller.
const START_TIMEOUT: Duration = Duration::from_secs(180);
/// How long one configuration change may take to apply.
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a paused reviewer's runtime is given to terminate its harness
/// process group before the pause gives up and reports the leak.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a cancelled reviewer turn is given to leave the relay idle before
/// the pause stops waiting for it.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval between reads of the reviewer relay's durable state while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Specialist lanes admitted at once. Lower than mjolnir's six because these
/// run inside one resource-limited container beside the primary agent, and a
/// lane that waits a little costs the review nothing: the supervisor may not
/// conclude until every launched lane has reported anyway.
const MAX_PARALLEL_LANES: usize = 3;
/// The role plan review uses, and the one a request without a role means. It
/// is the turn review's own reviewer role, so the two features name the same
/// harness the same way.
pub use hel::hel_review::driver::REVIEWER_ROLE as DEFAULT_ROLE;
/// File inside a role's home naming the reviewer generation it was copied for.
const ROLE_GENERATION_MARKER: &str = ".hel-reviewer-generation";

/// Everything a reviewer inherits from the primary session it reviews for.
#[derive(Debug, Clone)]
pub struct ReviewerPlacement {
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
        self.root().join(REVIEWER_PROFILE_DIR)
    }

    /// Where one role lives. The default role keeps the original layout, so a
    /// worker that was staged before roles existed still finds its journal.
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
            self.profile_home()
        } else {
            self.role_root(role).join(REVIEWER_PROFILE_DIR)
        }
    }

    /// Relay session id for one role. It is derived from the primary's so
    /// worker logs name the pair, and it is distinct so the relays can never
    /// be mistaken for one another. The default role keeps the original
    /// `-reviewer` suffix, which the controller's projection already uses.
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
    dispatch_wake: mpsc::Sender<()>,
    acp: JoinHandle<Result<()>>,
    coordinator: JoinHandle<Result<()>>,
    /// A specialist lane's admission slot, held for as long as its harness
    /// runs. Roles that are not lanes hold none: the supervisor and the intent
    /// analyst are not what the cap is protecting the container from.
    _lane_slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// One reviewing agent: its harness process, its relay, and its own copy of
/// the staged profile.
struct ReviewerRole {
    role: String,
    placement: ReviewerPlacement,
    relay: Option<Arc<Mutex<DurableRelay>>>,
    running: Option<RunningReviewer>,
    /// Distinguishes the configuration commands this role submits itself.
    config_sequence: u64,
}

/// Owns every reviewing role beside one primary worker.
///
/// Roles are locked one at a time rather than behind a single sidecar lock:
/// launching a specialist lane takes seconds, and the controller polls other
/// roles' journals throughout.
pub struct ReviewerSidecar {
    placement: ReviewerPlacement,
    roles:
        std::sync::Mutex<std::collections::BTreeMap<String, Arc<tokio::sync::Mutex<ReviewerRole>>>>,
    /// Admission for specialist lanes, so a supervisor that dispatches the
    /// whole roster cannot put six harnesses in one container at once.
    lane_slots: Arc<tokio::sync::Semaphore>,
    /// Lanes the supervisor has asked for and the controller has not collected
    /// yet. The worker records them and answers the tool at once; starting a
    /// lane is the controller's job, because only it holds the diff, the job
    /// and the rendered prompts.
    pending_dispatches: std::sync::Mutex<Vec<hel::hel_review::lanes::ReviewSubagentRequest>>,
}

impl ReviewerSidecar {
    #[must_use]
    pub fn new(placement: ReviewerPlacement) -> Self {
        Self {
            placement,
            roles: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            lane_slots: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_LANES)),
            pending_dispatches: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Records one `call_review_subagents` dispatch from the supervisor's MCP
    /// tool, answering with the lanes it accepted.
    ///
    /// The reply says "started" because that is what the supervisor needs to
    /// know: these lanes are now the review's business, and it may not
    /// conclude until each has reported. A lane the controller then cannot
    /// launch reaches the supervisor as a failed report, exactly as a lane
    /// that dies mid-run does.
    #[must_use]
    pub fn record_dispatch(
        &self,
        dispatch: hel::hel_review::lanes::LaneDispatch,
    ) -> hel::hel_review::lanes::LaneDispatchReply {
        if let Err(message) = hel::hel_review::lanes::validate_dispatch(&dispatch.reviewers) {
            return hel::hel_review::lanes::LaneDispatchReply {
                started: Vec::new(),
                error: Some(message),
            };
        }
        let mut pending = self
            .pending_dispatches
            .lock()
            .expect("review dispatch queue lock poisoned");
        let mut started = Vec::new();
        for request in dispatch.reviewers {
            // A lane the supervisor already launched is not launched twice:
            // its report is still coming, and a second copy would double the
            // container's load for no new evidence.
            if pending
                .iter()
                .any(|queued| queued.agent_type == request.agent_type)
            {
                continue;
            }
            started.push(request.agent_type.clone());
            pending.push(request);
        }
        hel::hel_review::lanes::LaneDispatchReply {
            started,
            error: None,
        }
    }

    /// Hands the controller every dispatch recorded since it last asked.
    #[must_use]
    pub fn take_dispatches(&self) -> Vec<hel::hel_review::lanes::ReviewSubagentRequest> {
        std::mem::take(
            &mut *self
                .pending_dispatches
                .lock()
                .expect("review dispatch queue lock poisoned"),
        )
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
                    relay: None,
                    running: None,
                    config_sequence: 0,
                }))
            })
            .clone()
    }

    /// Every role this sidecar has touched, in a stable order.
    #[must_use]
    pub fn known_roles(&self) -> Vec<String> {
        self.roles
            .lock()
            .expect("reviewer role map lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Serves one reviewer request. Every variant answers with an ordinary
    /// relay response, so the controller decodes reviewer replies with the
    /// code it already uses for the primary.
    pub async fn handle(
        &self,
        envelope: RelayRequestEnvelope,
        role: Option<String>,
        request: hel::hel_worker::ReviewerRequest,
    ) -> RelayResponseEnvelope {
        let request_id = envelope.request_id;
        let protocol_version = envelope.protocol_version;
        let role = role.unwrap_or_else(|| DEFAULT_ROLE.to_owned());
        let body = match self.dispatch(&role, request).await {
            Ok(body) => body,
            Err(error) => reviewer_error(format!("{error:#}")),
        };
        RelayResponseEnvelope {
            request_id,
            protocol_version,
            body,
        }
    }

    /// Stops every running role. Called when the worker's session closes, so
    /// no reviewing harness outlives the session it was reviewing for.
    pub async fn pause_all(&self) {
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
        for role in roles {
            role.lock().await.pause().await;
        }
    }

    async fn dispatch(
        &self,
        role: &str,
        request: hel::hel_worker::ReviewerRequest,
    ) -> Result<RelayResponseBody> {
        if let hel::hel_worker::ReviewerRequest::TakeLaneDispatches = request {
            return Ok(RelayResponseBody::Ok {
                payload: RelayResponsePayload::LaneDispatches {
                    requests: self.take_dispatches(),
                },
            });
        }
        let handle = self.role(role);
        let lane_slots = self.lane_slots.clone();
        let mut role = handle.lock().await;
        role.dispatch(&lane_slots, request).await
    }
}

impl ReviewerRole {
    async fn dispatch(
        &mut self,
        lane_slots: &Arc<tokio::sync::Semaphore>,
        request: hel::hel_worker::ReviewerRequest,
    ) -> Result<RelayResponseBody> {
        use hel::hel_worker::ReviewerRequest;

        match request {
            ReviewerRequest::Start { config } => self.start(lane_slots, *config).await,
            ReviewerRequest::Pause => {
                self.pause().await;
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
                self.wake_dispatch();
                Ok(response)
            }
            ReviewerRequest::Status => self.forward(RelayRequest::Status),
            ReviewerRequest::RespondElicitation {
                elicitation_id,
                response,
            } => self.respond_elicitation(elicitation_id, response).await,
            ReviewerRequest::CaptureDelta { baselines } => self.capture_delta(baselines).await,
            ReviewerRequest::AdvanceBaseline { trees } => self.advance_baseline(trees).await,
            ReviewerRequest::AnalyzeDelta { repositories } => {
                self.analyze_delta(repositories).await
            }
            ReviewerRequest::TakeLaneDispatches => {
                unreachable!("lane dispatches are answered by the sidecar, not by one role")
            }
        }
    }

    /// The workspace repositories a review covers, discovered from the
    /// primary's working directory and additional roots.
    fn review_repositories(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.placement.cwd.clone()];
        roots.extend(self.placement.additional_directories.iter().cloned());
        hel::hel_review::delta::discover_repositories(&hel::hel_archive::SystemGit, &roots)
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
        let repositories = self.review_repositories();
        let repositories = tokio::task::spawn_blocking(move || {
            hel::hel_review::delta::capture_repository_deltas(
                &hel::hel_archive::SystemGit,
                &repositories,
                &baselines,
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
        tokio::task::spawn_blocking(move || {
            hel::hel_review::delta::advance_baselines(&hel::hel_archive::SystemGit, &trees)
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review baseline update stopped: {error}"))??;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewBaselineAdvanced,
        })
    }

    /// Runs Bifrost's semantic diff analysis over the captured trees.
    ///
    /// A repository with no recorded baseline is analyzed against its own
    /// empty tree, which is what "everything here is new" means to Bifrost.
    async fn analyze_delta(
        &mut self,
        repositories: Vec<hel::hel_worker::AnalyzeDeltaRepository>,
    ) -> Result<RelayResponseBody> {
        let mut requests = Vec::new();
        for repository in repositories {
            let base = match repository.baseline_tree {
                Some(tree) => tree,
                None => {
                    let root = repository.root.clone();
                    tokio::task::spawn_blocking(move || {
                        hel::hel_archive::empty_tree_id(&hel::hel_archive::SystemGit, &root)
                    })
                    .await
                    .map_err(|error| anyhow::anyhow!("reading the empty tree stopped: {error}"))??
                }
            };
            requests.push(hel::hel_review::bifrost::AnalyzeRequest {
                repository: repository.root,
                base_tree: base,
                target_tree: repository.current_tree,
            });
        }
        let packet = hel::hel_review::bifrost::changed_functions_packet(&requests)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewChangedFunctions { packet },
        })
    }

    /// Answers a form the reviewer's harness is waiting on.
    ///
    /// The answer goes straight to the reviewer's ACP runtime, never through
    /// its command queue: form content is the user's, and the primary's
    /// answers are kept out of the durable ledger for the same reason.
    async fn respond_elicitation(
        &mut self,
        elicitation_id: String,
        response: hel::hel_elicitation::ElicitationResponse,
    ) -> Result<RelayResponseBody> {
        let Some(running) = self.running.as_ref() else {
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
    async fn start(
        &mut self,
        lane_slots: &Arc<tokio::sync::Semaphore>,
        config: ReviewerLaunchConfig,
    ) -> Result<RelayResponseBody> {
        let profile_home = self.placement.profile_home();
        if !profile_home.is_dir() {
            bail!(
                "reviewer profile has not been staged at {}",
                profile_home.display()
            );
        }
        let reused = match self.running.as_ref() {
            Some(running) if running.config.reusable_for(&config) => true,
            Some(_) => {
                // A different profile or a new generation is a different
                // reviewer. Stop the old process group before its replacement
                // touches the same staged directory.
                self.pause().await;
                false
            }
            None => false,
        };
        if !reused {
            self.launch(lane_slots, &config).await?;
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
    async fn launch(
        &mut self,
        lane_slots: &Arc<tokio::sync::Semaphore>,
        config: &ReviewerLaunchConfig,
    ) -> Result<()> {
        let root = self.placement.role_root(&self.role);
        std::fs::create_dir_all(&root)
            .with_context(|| format!("create reviewer root {}", root.display()))?;
        // Every role but the default runs from its own copy of the staged
        // profile: two harnesses sharing one config home would race over the
        // same session files and credentials.
        let profile_home = self.role_profile_home(config.generation)?;
        // A lane waits for a slot before its harness starts, so a supervisor
        // that dispatches the whole roster cannot fill the container.
        let lane_slot = if is_lane(&self.role) {
            Some(
                lane_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .context("the reviewer lane admission semaphore closed")?,
            )
        } else {
            None
        };
        let relay = self.open_relay()?;

        let mut environment = config.environment.clone();
        // The worker fixes the harness home itself: a controller must never be
        // able to aim a reviewer at the primary's credentials.
        environment.insert(
            config.harness.home_env().into(),
            profile_home.to_string_lossy().into_owned(),
        );
        config
            .harness
            .configure_execution_environment(config.execution_policy, &mut environment);

        let supervisor_path = root.join("acp-supervisor.json");
        AcpSupervisorSpec {
            command: config.bridge_command.clone(),
            args: config.bridge_args.clone(),
            environment,
            cwd: self.placement.cwd.clone(),
        }
        .write_spec(&supervisor_path)?;

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
        let (resume_session, acp_activity, step_clock) = {
            let relay = relay.lock().expect("reviewer relay lock poisoned");
            (
                relay.operational_state().native_session_id,
                relay.acp_activity_clock(),
                relay.step_clock(),
            )
        };
        let spec = LaunchSpec {
            command: self.placement.worker_executable.clone(),
            args: vec![
                "worker".into(),
                "acp-supervisor".into(),
                "--spec".into(),
                supervisor_path.to_string_lossy().into_owned(),
            ],
            environment: Default::default(),
            cwd: self.placement.cwd.clone(),
            additional_directories: self.placement.additional_directories.clone(),
            // A reviewer reads the workspace; it never syncs project memory,
            // which belongs to the primary session alone.
            project_memory: None,
            extra_mcp_servers: config.mcp_servers.clone(),
            resume_session,
            harness: config.harness,
            execution_policy: config.execution_policy,
            acp_activity,
            step_clock,
        };

        let (commands_tx, commands_rx) = mpsc::channel(32);
        let (events_tx, events_rx) = mpsc::channel(ACP_EVENT_CHANNEL_CAPACITY);
        let (wake_tx, wake_rx) = mpsc::channel(1);
        let acp = tokio::spawn(hel_acp::run(spec, commands_rx, events_tx));
        let coordinator = tokio::spawn(run_relay_coordinator(
            relay.clone(),
            events_rx,
            wake_rx,
            commands_tx.clone(),
        ));
        self.running = Some(RunningReviewer {
            config: config.clone(),
            commands: commands_tx,
            dispatch_wake: wake_tx,
            acp,
            coordinator,
            _lane_slot: lane_slot,
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
            self.pause().await;
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
        let Ok(state) = self.state() else {
            return;
        };
        // The same harness-aware decision the primary's /plan uses, so a
        // reviewer asks for plan mode exactly the way a session does.
        let mut surface = hel::hel_acp::surface::AcpSessionSurface::default();
        surface.set_harness_kind(config.harness);
        surface.set_config_options(&state.config_options);
        surface.set_session_modes(state.modes.clone());
        let Ok(control) = surface.plan_control(true) else {
            return;
        };
        let command = match control {
            hel::hel_acp::PlanControl::SetConfig { key, value } => {
                RelayCommand::SetConfig { key, value }
            }
            hel::hel_acp::PlanControl::SetSessionMode { mode_id } => {
                RelayCommand::SetSessionMode { mode_id }
            }
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
        self.wake_dispatch();
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
            let Some(value) = value else {
                continue;
            };
            if self
                .state()?
                .config
                .get(key)
                .is_some_and(|current| current == value)
            {
                continue;
            }
            self.config_sequence += 1;
            let command_id = format!("reviewer-{key}-{}", self.config_sequence);
            let cursor = self.cursor()?;
            let body = self.forward(RelayRequest::Submit {
                command_id: command_id.clone(),
                command: RelayCommand::SetConfig {
                    key: key.to_owned(),
                    value: value.clone(),
                },
            })?;
            if let RelayResponseBody::Error { error } = body {
                bail!(
                    "reviewer could not accept {key} {value:?}: {}",
                    error.message
                );
            }
            self.wake_dispatch();
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
            let applied = self.state()?.config.get(key) == Some(value);
            if settled.is_err() || !applied {
                let failure = self
                    .failure_since(&cursor)
                    .unwrap_or_else(|| format!("the reviewer did not apply {key} {value:?}"));
                bail!("{failure}");
            }
        }
        Ok(())
    }

    /// Cancels any turn in flight and stops the reviewer's process group,
    /// keeping its staged profile, native session and journal.
    pub async fn pause(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        // Ask the harness to stop the turn first, so a paused reviewer is not
        // reloaded mid-answer next time.
        self.config_sequence += 1;
        let command_id = format!("reviewer-cancel-{}", self.config_sequence);
        if self
            .forward(RelayRequest::Submit {
                command_id,
                command: RelayCommand::Cancel,
            })
            .is_ok()
        {
            let _ = running.dispatch_wake.try_send(());
            let _ = self
                .wait_for(CANCEL_TIMEOUT, |state| state.active_prompt.is_none())
                .await;
        }

        // The coordinator holds the only other command sender. Stopping it
        // first is what lets the runtime see a closed channel, shut its bridge
        // down gracefully, and terminate the harness process group. Killing
        // the runtime instead would strand that group.
        running.coordinator.abort();
        let _ = running.coordinator.await;
        drop(running.commands);
        drop(running.dispatch_wake);
        match tokio::time::timeout(PAUSE_TIMEOUT, running.acp).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                tracing::debug!(
                    operation = "reviewer_pause",
                    error = format!("{error:#}"),
                    "the reviewer runtime reported a failure while stopping"
                );
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    operation = "reviewer_pause",
                    %error,
                    "the reviewer runtime task stopped abnormally"
                );
            }
            Err(_) => {
                // Reported rather than dropped: the harness process group may
                // still be alive, and only a report makes that visible.
                tracing::error!(
                    operation = "reviewer_pause",
                    "the reviewer runtime did not stop within {PAUSE_TIMEOUT:?}; \
                     its harness process group may still be running"
                );
            }
        }
    }

    /// This role's own harness home, refreshed from the staged profile when it
    /// is missing or belongs to an older reviewer generation.
    ///
    /// The controller stages one copy of the chosen profile; every role but
    /// the default runs from a copy of that copy, so concurrent harnesses
    /// never share a config home. The generation marker keeps a repeat review
    /// from re-copying a large profile on every lane launch while still
    /// refreshing it when the reviewer's lifetime changes.
    fn role_profile_home(&self, generation: u64) -> Result<PathBuf> {
        let home = self.placement.role_profile_home(&self.role);
        if self.role == DEFAULT_ROLE {
            return Ok(home);
        }
        let marker = home.join(ROLE_GENERATION_MARKER);
        let staged = std::fs::read_to_string(&marker).ok();
        if staged.as_deref() == Some(generation.to_string().as_str()) {
            return Ok(home);
        }
        if home.exists() {
            std::fs::remove_dir_all(&home)
                .with_context(|| format!("clear the reviewer role home {}", home.display()))?;
        }
        copy_tree(&self.placement.profile_home(), &home)
            .with_context(|| format!("copy the staged reviewer profile into {}", home.display()))?;
        std::fs::write(&marker, generation.to_string())
            .with_context(|| format!("record the reviewer role generation {}", marker.display()))?;
        Ok(home)
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
            protocol_version: hel::hel_worker::RELAY_PROTOCOL_VERSION,
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

    fn wake_dispatch(&self) {
        if let Some(running) = &self.running {
            let _ = running.dispatch_wake.try_send(());
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
            if ready(&self.state()?) {
                return Ok(());
            }
            if self
                .running
                .as_ref()
                .is_none_or(|running| running.acp.is_finished())
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
            if self
                .events_since(cursor)
                .iter()
                .any(|event| wanted(&event.observation))
            {
                return Ok(());
            }
            if self
                .running
                .as_ref()
                .is_none_or(|running| running.acp.is_finished())
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

/// Whether `role` is one of the specialist lanes the admission cap covers.
/// The default reviewer, the validator, the supervisor and the intent analyst
/// are single-instance roles and are not what the cap protects against.
fn is_lane(role: &str) -> bool {
    hel::hel_review::lanes::lane_by_id(role).is_some()
}

/// Recursively copies a staged profile into a role's own home.
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
        error: hel::hel_worker::RelayProtocolError {
            code: hel::hel_worker::RelayErrorCode::InvalidState,
            message,
            retryable: false,
            detail: None,
        },
    }
}
