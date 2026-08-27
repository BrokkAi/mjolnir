use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::{self, SelectedAgent};
use crate::event::{UiCommand, UiEvent};
use crate::remote::TrackerStatusSeed;
use crate::remote::{self, ServerSessionLaunchState, remote::RemoteSessionTracker};
use crate::roster;
use crate::subagent;
use mj_core::acp::{self, AcpRuntimeConfig};

#[derive(Debug)]
struct ServerAgentSession {
    session_id: Arc<Mutex<Option<String>>>,
    command_tx: mpsc::UnboundedSender<UiCommand>,
    task: JoinHandle<()>,
}

/// The agent binding new server-owned sessions launch with, plus the hash of
/// the config file it was resolved from so a `/mjconfig` save (or any other
/// config edit) triggers a re-resolve before the next session starts.
#[derive(Debug, Clone)]
struct ServerSessionLaunch {
    binding: ServerSessionBinding,
    config_hash: Option<u64>,
}

/// Whether server-owned sessions can launch at all. A server that starts on a
/// machine with no launchable model runs `Unbound` — serving the viewer so the
/// user can finish setup — and upgrades to `Bound` on the first successful
/// roster re-resolve.
#[derive(Debug, Clone)]
enum ServerSessionBinding {
    Bound(Box<BoundSession>),
    Unbound { reason: String },
}

/// The launch binding a resolved roster produced.
#[derive(Debug, Clone)]
struct BoundSession {
    agent: SelectedAgent,
    roster: Option<roster::Roster>,
}

impl ServerSessionBinding {
    fn bound(agent: SelectedAgent, roster: Option<roster::Roster>) -> Self {
        Self::Bound(Box::new(BoundSession { agent, roster }))
    }

    fn is_bound(&self) -> bool {
        matches!(self, Self::Bound(_))
    }
}

/// How far one requested session launch has got.
///
/// `POST /api/server-sessions` returns as soon as the launch has been
/// *requested* — the agent is spawned on a detached task — so without this the
/// only failure the viewer could ever report was its own timeout. A session
/// that dies on startup never publishes a snapshot, so there is nothing in the
/// session list to carry the error either.
/// Launch outcomes, keyed by the id handed back to the client that asked for
/// the launch.
#[derive(Debug, Default)]
struct ServerSessionLaunchRegistry {
    next_id: AtomicU64,
    launches: Mutex<BTreeMap<u64, ServerSessionLaunchState>>,
}

/// Retained launch records. Each is a handful of bytes and only a viewer
/// actively waiting on one ever reads it, so this only has to stop an
/// long-lived server from growing a record per session forever.
const MAX_RETAINED_LAUNCHES: usize = 64;

impl ServerSessionLaunchRegistry {
    fn begin(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut launches) = self.launches.lock() {
            while launches.len() >= MAX_RETAINED_LAUNCHES {
                let Some(oldest) = launches.keys().next().copied() else {
                    break;
                };
                launches.remove(&oldest);
            }
            launches.insert(id, ServerSessionLaunchState::Starting);
        }
        id
    }

    fn resolve(&self, id: u64, state: ServerSessionLaunchState) {
        if let Ok(mut launches) = self.launches.lock()
            && let Some(slot) = launches.get_mut(&id)
        {
            // First outcome wins: a session that started and later failed is a
            // session failure, which the transcript carries, not a launch
            // failure.
            if matches!(slot, ServerSessionLaunchState::Starting) {
                *slot = state;
            }
        }
    }

    fn get(&self, id: u64) -> Option<ServerSessionLaunchState> {
        self.launches.lock().ok()?.get(&id).cloned()
    }
}

/// Records one launch's outcome. Held by the session task, which is the only
/// place that learns whether the agent actually came up.
#[derive(Debug, Clone)]
struct ServerSessionLaunchReporter {
    registry: Arc<ServerSessionLaunchRegistry>,
    launch_id: u64,
}

#[derive(Debug, Clone)]
struct ServerSessionStart {
    resume_session: Option<String>,
    reporter: Option<ServerSessionLaunchReporter>,
}

impl ServerSessionLaunchReporter {
    fn started(&self, session_id: &str) {
        self.registry.resolve(
            self.launch_id,
            ServerSessionLaunchState::Started {
                session_id: session_id.to_string(),
            },
        );
    }

    fn failed(&self, error: impl Into<String>) {
        self.registry.resolve(
            self.launch_id,
            ServerSessionLaunchState::Failed {
                error: error.into(),
            },
        );
    }
}

#[derive(Debug)]
pub(crate) struct RootServerSessionManager {
    launch: RwLock<ServerSessionLaunch>,
    /// Serialize the check-resolve-publish sequence so concurrent session
    /// requests cannot launch from the old roster while another refreshes it.
    roster_refresh_lock: tokio::sync::Mutex<()>,
    /// Credentials and ACP capabilities can change without touching the
    /// config file. Force the next server-owned session through resolution
    /// when discovery observes such a change.
    roster_refresh_requested: AtomicBool,
    /// Directory the startup roster was resolved against; config-change
    /// re-resolves reuse it. `None` disables re-resolving (tests).
    resolve_cwd: Option<PathBuf>,
    additional_directories: Vec<PathBuf>,
    snapshot_exclusions: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    sessions: Mutex<Vec<ServerAgentSession>>,
    launches: Arc<ServerSessionLaunchRegistry>,
}

fn selected_agent_for_roster(roster: &roster::Roster) -> SelectedAgent {
    let primary = &roster.primary;
    SelectedAgent {
        source_id: format!("roster:{}", primary.model.model),
        program: primary.launch.command.clone(),
        args: primary.launch.args.clone(),
        env: primary.launch.env.clone(),
    }
}

/// Display-only seat bindings for the `/mjconfig` role panels, mirroring what
/// the roster bound for each seat.
/// Content hash of the saved config file; `None` when it cannot be read.
pub(crate) fn config_file_hash(config_path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let contents = std::fs::read(config_path).ok()?;
    let mut hasher = std::hash::DefaultHasher::new();
    contents.hash(&mut hasher);
    Some(hasher.finish())
}

impl RootServerSessionManager {
    pub(crate) fn new_roster(
        roster: roster::Roster,
        config_hash: Option<u64>,
        resolve_cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        snapshot_exclusions: Vec<PathBuf>,
        fs_max_text_bytes: u64,
    ) -> Self {
        Self::with_binding(
            ServerSessionBinding::bound(selected_agent_for_roster(&roster), Some(roster)),
            config_hash,
            resolve_cwd,
            additional_directories,
            snapshot_exclusions,
            fs_max_text_bytes,
        )
    }

    /// A manager for a server that started with no launchable model. Session
    /// launches fail with `reason` until a re-resolve binds a roster; every
    /// refresh attempt re-resolves regardless of the config hash, because the
    /// missing piece (credentials, an installed agent) changes outside the
    /// config file.
    pub(crate) fn new_unresolved(
        reason: String,
        config_hash: Option<u64>,
        resolve_cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        snapshot_exclusions: Vec<PathBuf>,
        fs_max_text_bytes: u64,
    ) -> Self {
        Self::with_binding(
            ServerSessionBinding::Unbound { reason },
            config_hash,
            resolve_cwd,
            additional_directories,
            snapshot_exclusions,
            fs_max_text_bytes,
        )
    }

    #[cfg(test)]
    pub(crate) fn is_bound(&self) -> bool {
        self.launch
            .read()
            .is_ok_and(|launch| launch.binding.is_bound())
    }

    fn with_binding(
        binding: ServerSessionBinding,
        config_hash: Option<u64>,
        resolve_cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        snapshot_exclusions: Vec<PathBuf>,
        fs_max_text_bytes: u64,
    ) -> Self {
        Self {
            launch: RwLock::new(ServerSessionLaunch {
                binding,
                config_hash,
            }),
            roster_refresh_lock: tokio::sync::Mutex::new(()),
            roster_refresh_requested: AtomicBool::new(false),
            resolve_cwd: Some(resolve_cwd),
            additional_directories,
            snapshot_exclusions,
            fs_max_text_bytes,
            sessions: Mutex::new(Vec::new()),
            launches: Arc::new(ServerSessionLaunchRegistry::default()),
        }
    }

    /// Re-resolve the roster when the saved config or detected capabilities
    /// changed since the last resolution, so a new session does not use the
    /// binding frozen at server startup.
    /// Returns the fresh roster when a re-resolve happened, and an error when
    /// the saved config cannot bind a roster at all — callers must refuse to
    /// start the session rather than silently launching the stale binding.
    async fn refresh_for_config(
        &self,
        config_path: &Path,
    ) -> std::result::Result<Option<roster::Roster>, String> {
        self.refresh_for_config_with(config_path, |config, cwd| async move {
            roster::resolve(&config, &cwd).await
        })
        .await
    }

    async fn refresh_for_config_with<F, Fut>(
        &self,
        config_path: &Path,
        resolve: F,
    ) -> std::result::Result<Option<roster::Roster>, String>
    where
        F: FnOnce(config::Config, PathBuf) -> Fut,
        Fut: std::future::Future<Output = Result<roster::Roster>>,
    {
        let Some(resolve_cwd) = self.resolve_cwd.clone() else {
            return Ok(None);
        };
        let _refresh_guard = self.roster_refresh_lock.lock().await;
        let refresh_requested = self.roster_refresh_requested.swap(false, Ordering::AcqRel);
        let hash = config_file_hash(config_path);
        {
            let launch = self.launch.read().expect("server launch lock");
            // An unbound server re-resolves on every refresh: what it lacks
            // (credentials, an installed agent) appears without touching the
            // config file, so the hash alone can never clear the state.
            if !refresh_requested && launch.config_hash == hash && launch.binding.is_bound() {
                return Ok(None);
            }
        }
        let Ok(config) = config::Config::load(config_path) else {
            if refresh_requested {
                self.roster_refresh_requested.store(true, Ordering::Release);
            }
            return Ok(None);
        };
        match resolve(config, resolve_cwd).await {
            Ok(roster) => {
                let mut launch = self.launch.write().expect("server launch lock");
                launch.binding = ServerSessionBinding::bound(
                    selected_agent_for_roster(&roster),
                    Some(roster.clone()),
                );
                launch.config_hash = hash;
                Ok(Some(roster))
            }
            Err(error) => {
                if refresh_requested {
                    self.roster_refresh_requested.store(true, Ordering::Release);
                }
                warn!("roster re-resolve failed: {error:#}");
                Err(format!("{error:#}"))
            }
        }
    }

    fn request_roster_refresh(&self) {
        self.roster_refresh_requested.store(true, Ordering::Release);
    }

    fn resolve_cwd(&self) -> Option<PathBuf> {
        self.resolve_cwd.clone()
    }

    fn launch_state(&self, launch_id: u64) -> Option<ServerSessionLaunchState> {
        self.launches.get(launch_id)
    }

    /// Request a session launch. Returns the id the caller polls for the
    /// outcome: the agent starts on a detached task, so the launch has not
    /// succeeded merely because this returned.
    fn start_session(&self, cwd: PathBuf) -> u64 {
        self.start_session_with_resume(cwd, None)
    }

    fn resume_session(&self, cwd: PathBuf, session_id: String) -> u64 {
        self.start_session_with_resume(cwd, Some(session_id))
    }

    fn start_session_with_resume(&self, cwd: PathBuf, resume_session: Option<String>) -> u64 {
        let launch = self.launch.read().expect("server launch lock").clone();
        let launch_id = self.launches.begin();
        let reporter = ServerSessionLaunchReporter {
            registry: Arc::clone(&self.launches),
            launch_id,
        };
        let (agent, roster) = match launch.binding {
            ServerSessionBinding::Bound(bound) => (bound.agent, bound.roster),
            ServerSessionBinding::Unbound { reason } => {
                reporter.failed(reason);
                return launch_id;
            }
        };
        let session = start_server_agent_session(
            agent,
            roster,
            cwd,
            self.additional_directories.clone(),
            self.snapshot_exclusions.clone(),
            self.fs_max_text_bytes,
            ServerSessionStart {
                resume_session,
                reporter: Some(reporter.clone()),
            },
        );
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.push(session);
        } else {
            session.task.abort();
            reporter.failed("server is shutting down");
        }
        launch_id
    }

    fn owns_session(&self, session_id: &str) -> bool {
        self.sessions.lock().is_ok_and(|sessions| {
            sessions.iter().any(|session| {
                !session.task.is_finished()
                    && session
                        .session_id
                        .lock()
                        .is_ok_and(|current| current.as_deref() == Some(session_id))
            })
        })
    }

    async fn archive_session(&self, session_id: &str) -> bool {
        let session = self.sessions.lock().ok().and_then(|mut sessions| {
            sessions.retain(|session| !session.task.is_finished());
            let index = sessions.iter().position(|session| {
                session
                    .session_id
                    .lock()
                    .is_ok_and(|current| current.as_deref() == Some(session_id))
            })?;
            Some(sessions.swap_remove(index))
        });
        let Some(session) = session else {
            return false;
        };
        session.shutdown().await;
        true
    }

    async fn shutdown_all(&self) {
        let sessions = self
            .sessions
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default();
        for session in sessions {
            session.shutdown().await;
        }
    }

    async fn reload_auxiliary_agents(&self) {
        let commands = self
            .sessions
            .lock()
            .map(|mut sessions| {
                sessions.retain(|session| !session.task.is_finished());
                sessions
                    .iter()
                    .map(|session| session.command_tx.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for command_tx in commands {
            let _ = command_tx.send(UiCommand::ReloadAuxiliaryAgents);
        }
    }
}

fn start_server_agent_session(
    agent: SelectedAgent,
    roster: Option<roster::Roster>,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    snapshot_exclusions: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    start: ServerSessionStart,
) -> ServerAgentSession {
    let ServerSessionStart {
        resume_session,
        reporter: launch_reporter,
    } = start;
    let side_agent = agent.clone();
    let side_cwd = cwd.clone();
    let side_additional_directories = additional_directories.clone();
    let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();
    let auxiliary_event_tx = runtime_event_tx.clone();
    let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();
    let (server_cmd_tx, mut server_cmd_rx) = mpsc::unbounded_channel();
    let (remote_event_tx, mut remote_event_rx) = mpsc::unbounded_channel();
    let (side_event_tx, mut side_event_rx) = mpsc::unbounded_channel();
    let session_id = Arc::new(Mutex::new(resume_session.clone()));
    let published_session_id = Arc::clone(&session_id);
    // The adapter source id ("codex-acp", ...) — not the synthetic
    // `roster:{model}` launch id — so saved session options load from and
    // accepted live values persist to the same buckets the TUI uses.
    let agent_source_id = roster.as_ref().map_or_else(
        || agent.source_id.clone(),
        |resolved| resolved.primary.launch.source_id.clone(),
    );
    let config_path = config::default_config_path();
    let app_config = config::Config::load(&config_path).unwrap_or_default();
    let saved_session_config = roster.as_ref().map_or_else(HashMap::new, |resolved| {
        config::load_saved_session_config(
            &config_path,
            &resolved.primary.launch.source_id,
            &resolved.primary.model.model,
            config::SessionConfigSeat::Primary,
        )
    });
    let project_label = mj_core::paths::project_label_from_cwd(&cwd);
    let worktree_label = mj_core::paths::worktree_name_from_cwd(&cwd);
    // With a roster the session has a real primary model; align the published
    // identity with what a TUI session publishes (model + adapter source)
    // instead of the adapter display label alone.
    let (model_label, model_source) = match roster.as_ref() {
        Some(resolved) => (
            resolved.primary.model.model.clone(),
            Some(resolved.primary.launch.source_id.clone()),
        ),
        None => (remote::agent_display_label(&agent), None),
    };
    let reasoning_effort = roster
        .as_ref()
        .and_then(|resolved| resolved.primary.reasoning_effort.clone());
    let tracker = RemoteSessionTracker::new(
        project_label,
        worktree_label,
        model_label,
        TrackerStatusSeed {
            model_source,
            reasoning_effort,
            model_choices: roster
                .as_ref()
                .map(|resolved| resolved.choices.clone())
                .unwrap_or_default(),
            cwd: Some(cwd.clone()),
            runtime_stall_minutes: app_config.agent.runtime_stall_minutes,
        },
        Some(server_cmd_tx.clone()),
        Some(remote_event_tx),
        false,
    );
    if let Some(resolved) = roster.as_ref() {
        for warning in &resolved.warnings {
            tracker.observe_event(&UiEvent::Warning(warning.clone()));
        }
    }
    let mut roster_setup_error = None;
    let (subagent_roles, subagent_codex_home) = match roster.as_ref() {
        Some(resolved) => {
            match crate::isolated_subagent_roles(
                mj_core::roster::subagent_failover_roles(resolved),
                "subagent",
            ) {
                Ok(pair) => pair,
                Err(error) => {
                    roster_setup_error = Some(format!("prepare subagents: {error:#}"));
                    (Vec::new(), None)
                }
            }
        }
        None => (Vec::new(), None),
    };
    let quota_gate = crate::quota::Gate::new(cwd.clone(), runtime_event_tx.clone());
    let subagent_pool = (!subagent_roles.is_empty()).then(|| {
        crate::quota::RolePool::new(
            subagent_roles,
            quota_gate.clone(),
            app_config.subagents.auto_failover,
            "subagents",
            runtime_event_tx.clone(),
        )
    });
    let role_config = roster.as_ref().map(|resolved| acp::RuntimeRoleConfig {
        label: "primary".to_string(),
        model_id: resolved.primary.model.model.clone(),
        model_value: resolved.primary.model_value.clone(),
        adapter_source_id: resolved.primary.launch.source_id.clone(),
        permission: None,
        session_tag: None,
        reasoning_effort: resolved.primary.reasoning_effort.clone(),
    });
    let subagent_handoffs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Shared with the review fan-out so lane ids never collide with pool ids.
    let subagent_ids = subagent::SubagentIdAllocator::default();
    let active_implementation_workers = subagent::ActiveSubagentWorkers::default();
    let (review_checkpoint, review_checkpoints) = subagent::ReviewCheckpointClient::channel();
    let (subagent_reports, subagent_report_rx) = subagent::SubagentReportBus::channel();
    // Shared with the orchestrator so every wake can ask the still-running
    // subagents for progress.
    let subagent_runs = subagent::SubagentRegistry::default();
    // The discrete review's specialist lanes run on the subagent pool and share
    // the primary's workspace roots, so both have to be cloned before they move
    // into the subagent config and the runtime config respectively.
    let review_workers = subagent_pool.clone();
    let review_additional_directories = additional_directories.clone();
    let auxiliary_session_tag = format!("remote-{}", std::process::id());
    let live_subagent_options = crate::LiveSubagentOptions {
        agent_stderr: None,
        snapshot_exclusions: snapshot_exclusions.clone(),
        cwd: cwd.clone(),
        additional_directories: additional_directories.clone(),
        fs_max_text_bytes,
        session_tag: auxiliary_session_tag.clone(),
        handoff_counter: subagent_handoffs.clone(),
        id_allocator: subagent_ids.clone(),
        active_workers: active_implementation_workers.clone(),
        review_checkpoint,
        reports: subagent_reports.clone(),
        runs: subagent_runs.clone(),
    };
    let live_subagent_service = match subagent_pool.clone() {
        Some(pool) => subagent::LiveRuntimeService::new(crate::configured_subagent_service(
            pool,
            &live_subagent_options,
            &app_config.subagents,
            app_config.agent.mcp_discrete_review,
        )),
        None => subagent::LiveRuntimeService::unconfigured(),
    };
    let subagents = Some(Arc::new(live_subagent_service.clone()) as Arc<dyn acp::RuntimeService>);
    let provenance_primary = roster.as_ref().map(|resolved| resolved.primary.clone());
    let provenance_cwd = cwd.clone();
    let command_primary = provenance_primary.clone();
    let command_config_path = config_path.clone();
    let command_quota_gate = quota_gate;
    let command_auxiliary_event_tx = auxiliary_event_tx;
    let command_live_subagent_service = live_subagent_service.clone();
    let command_live_subagent_options = live_subagent_options.clone();
    let command_review_additional_directories = side_additional_directories.clone();
    let command_snapshot_exclusions = snapshot_exclusions.clone();
    let command_subagent_codex_homes = subagent_codex_home.into_iter().collect::<Vec<_>>();
    let session_memory = mj_core::memory::SessionMemory::from_config(
        &app_config.memory,
        &cwd,
        roster.as_ref().map(|resolved| resolved.primary.launch.kind),
    );
    let mut workspace_roots = Vec::with_capacity(1 + additional_directories.len());
    workspace_roots.push(cwd.clone());
    workspace_roots.extend(additional_directories.iter().cloned());
    let runtime_cfg = AcpRuntimeConfig {
        command: agent.program,
        args: agent.args,
        cwd,
        additional_directories,
        mcp_servers: Vec::new(),
        resume_session: resume_session.clone(),
        session_restore_mode: if resume_session.is_some() {
            acp::SessionRestoreMode::Replay
        } else {
            acp::SessionRestoreMode::Continue
        },
        env: agent.env,
        agent_stderr: None,
        fs_max_text_bytes,
        access_mode: mj_core::acp::RuntimeAccessMode::Full,
        agent_source_id: Some(agent_source_id),
        config_path: Some(config_path),
        saved_session_config,
        role_config,
        subagents,
        memory: session_memory,
        side_prompt_policy: false,
        termination: None,
    };
    let command_tx = server_cmd_tx.clone();
    let shutdown_tx = runtime_cmd_tx.clone();
    let orchestrated = mj_core::orchestrator::spawn(
        runtime_event_rx,
        mj_core::orchestrator::Config {
            runtime_commands: runtime_cmd_tx.clone(),
            active_subagent_workers: active_implementation_workers.clone(),
            subagent_reports: subagent_report_rx,
            subagent_report_bus: subagent_reports,
            subagent_runs: mj_core::orchestrator::SubagentProgressService::new(subagent_runs),
            progress_wake: mj_core::orchestrator::progress_wake_interval(
                app_config.subagents.progress_wake_minutes,
            ),
            discrete_review: app_config.agent.discrete_review,
            review_tier: app_config.agent.review_tier,
            correction_threshold: app_config.agent.correction_threshold,
            max_correction_rounds: app_config.agent.max_correction_rounds,
            primary_model: roster
                .as_ref()
                .map(|resolved| resolved.primary.model.model.clone()),
            review_root: provenance_cwd.clone(),
            review_checkpoints,
            review_fanout: match (
                review_workers,
                roster
                    .as_ref()
                    .and_then(|resolved| resolved.review_supervisor.clone()),
            ) {
                (Some(workers), Some(supervisor)) => {
                    mj_core::orchestrator::ReviewFanout::available(
                        crate::discrete_review::live_spawner(
                            crate::discrete_review::FanoutConfig {
                                workers,
                                supervisor,
                                cwd: provenance_cwd.clone(),
                                additional_directories: review_additional_directories,
                                session_tag: Some(auxiliary_session_tag.clone()),
                                agent_stderr: None,
                                snapshot_exclusions: snapshot_exclusions.clone(),
                                fs_max_text_bytes,
                                bifrost_analysis: app_config.agent.bifrost_analysis,
                                permission: app_config.review.permission,
                                bifrost_version: app_config.review.bifrost_version.clone(),
                                id_allocator: subagent_ids.clone(),
                            },
                        ),
                    )
                }
                (workers, supervisor) => {
                    let error = roster.as_ref().map_or_else(
                        || {
                            roster_setup_error.clone().unwrap_or_else(|| {
                                "the remote session has no resolved review roster".to_string()
                            })
                        },
                        |resolved| {
                            crate::review_fanout_error(
                                workers.is_some(),
                                supervisor.is_some(),
                                &app_config.subagents.model,
                                app_config.agent.needs_review_route(),
                                &resolved.warnings,
                            )
                        },
                    );
                    mj_core::orchestrator::ReviewFanout::unavailable(error)
                }
            },
        },
    );
    let primary_orchestrator = orchestrated.handle.clone();

    let task = tokio::spawn(async move {
        if let Some(error) = roster_setup_error {
            // The session never comes up, so nothing it could publish will
            // ever carry this. Tell whoever asked for the launch directly.
            if let Some(reporter) = launch_reporter.as_ref() {
                reporter.failed(error.clone());
            }
            tracker.observe_event(&UiEvent::Fatal(error));
            tracker.shutdown().await;
            return;
        }
        let runtime = {
            let launch_reporter = launch_reporter.clone();
            tokio::spawn(async move {
                if let Err(error) = acp::run(runtime_cfg, runtime_event_tx, runtime_cmd_rx).await {
                    // A spawn failure lands here — a missing adapter binary,
                    // a bad command line, a handshake that never completes.
                    // It used to stop at `debug!`, which is below the default
                    // level, so the viewer saw a session that simply never
                    // appeared.
                    if let Some(reporter) = launch_reporter.as_ref() {
                        reporter.failed(format!("{error:#}"));
                    }
                    debug!("server agent session exited: {error:#}");
                }
            })
        };
        let command_proxy = {
            let tracker = tracker.clone();
            let runtime_cmd_tx = runtime_cmd_tx.clone();
            let handoffs = subagent_handoffs.clone();
            let primary_orchestrator = primary_orchestrator.clone();
            let side_event_tx = side_event_tx.clone();
            let mut command_subagent_codex_homes = command_subagent_codex_homes;
            // Only the newest read may publish; the refresher enforces that
            // for every session owner.
            let workspace_diff_refresher = mj_core::acp::WorkspaceHeadDiffRefresher::new(
                workspace_roots.clone(),
                snapshot_exclusions.clone(),
                fs_max_text_bytes,
            );
            tokio::spawn(async move {
                let mut side_runtime: Option<crate::side::Runtime> = None;
                let mut local_epoch = 0_u64;
                while let Some(command) = server_cmd_rx.recv().await {
                    if let UiCommand::StartSide { initial_prompt } = command {
                        if side_runtime.is_some() {
                            let _ = side_event_tx.send(UiEvent::Warning(
                                "a side conversation is already active".to_string(),
                            ));
                            continue;
                        }
                        tracker.begin_side_start(initial_prompt.is_some());
                        let launch = crate::side::Launch {
                            agent: &side_agent,
                            cwd: side_cwd.clone(),
                            additional_directories: side_additional_directories.clone(),
                            agent_stderr: None,
                            fs_max_text_bytes,
                        };
                        let side = match crate::side::start(
                            launch,
                            &runtime_cmd_tx,
                            side_event_tx.clone(),
                        )
                        .await
                        {
                            Ok(side) => side,
                            Err(message) => {
                                let _ = side_event_tx.send(UiEvent::SideStartFailed { message });
                                continue;
                            }
                        };
                        if let Some(text) = initial_prompt {
                            let prompt = UiCommand::SendPrompt {
                                text,
                                images: Vec::new(),
                                resources: Vec::new(),
                            };
                            tracker.observe_side_command(&prompt);
                            let _ = side.send(prompt);
                        }
                        side_runtime = Some(side);
                        continue;
                    }
                    if matches!(command, UiCommand::ExitSide) {
                        tracker.finish_side_exit();
                        if let Some(side) = side_runtime.take()
                            && let Some(message) =
                                crate::side::discard(side, &side_agent, None).await
                        {
                            let _ = side_event_tx.send(UiEvent::Warning(message));
                        }
                        continue;
                    }
                    // Answered before side forwarding, exactly as the TUI does:
                    // the worktree is shared with any side conversation, so
                    // routing this into a side runtime would only lose it.
                    if matches!(command, UiCommand::RefreshWorkspaceDiff) {
                        workspace_diff_refresher.spawn(side_event_tx.clone());
                        continue;
                    }
                    let (command, force_main) = match command {
                        UiCommand::Main(command) => (*command, true),
                        command @ UiCommand::ReloadAuxiliaryAgents => (command, true),
                        command => (command, false),
                    };
                    if !force_main && side_runtime.is_some() {
                        if matches!(command, UiCommand::Shutdown) {
                            if let Some(side) = side_runtime.take() {
                                tracker.finish_side_exit();
                                let _ = crate::side::discard(side, &side_agent, None).await;
                            }
                        } else {
                            let side = side_runtime.as_ref().expect("checked side runtime");
                            tracker.observe_side_command(&command);
                            let _ = side.send(command);
                            continue;
                        }
                    }
                    if matches!(command, UiCommand::ReloadAuxiliaryAgents) {
                        let Some(command_primary) = command_primary.as_ref() else {
                            tracker.observe_event(&UiEvent::Warning(
                                "the active server session has no resolved primary route"
                                    .to_string(),
                            ));
                            continue;
                        };
                        let updated_config = match config::Config::load(&command_config_path) {
                            Ok(config) => config,
                            Err(error) => {
                                tracker.observe_event(&UiEvent::Warning(format!(
                                    "could not apply the saved reviewer configuration: {error:#}"
                                )));
                                continue;
                            }
                        };
                        let updated_roster = match roster::resolve(&updated_config, &side_cwd).await
                        {
                            Ok(roster) => roster,
                            Err(error) => {
                                tracker.observe_event(&UiEvent::Warning(format!(
                                    "the primary session kept its current reviewer configuration because the saved configuration could not be resolved: {error:#}"
                                )));
                                continue;
                            }
                        };
                        if !crate::primary_route_matches(command_primary, &updated_roster.primary) {
                            tracker.observe_event(&UiEvent::Info(
                                "primary agent changed; start a new server session to apply that route"
                                    .to_string(),
                            ));
                            continue;
                        }
                        let (roles, codex_home) = match crate::isolated_subagent_roles(
                            roster::subagent_failover_roles(&updated_roster),
                            "subagent",
                        ) {
                            Ok(result) => result,
                            Err(error) => {
                                tracker.observe_event(&UiEvent::Warning(format!(
                                    "could not prepare the saved subagent configuration: {error:#}"
                                )));
                                continue;
                            }
                        };
                        let pool = (!roles.is_empty()).then(|| {
                            crate::quota::RolePool::new(
                                roles,
                                command_quota_gate.clone(),
                                updated_config.subagents.auto_failover,
                                "subagents",
                                command_auxiliary_event_tx.clone(),
                            )
                        });
                        if let Some(pool) = pool.as_ref() {
                            command_live_subagent_service
                                .replace(crate::configured_subagent_service(
                                    pool.clone(),
                                    &command_live_subagent_options,
                                    &updated_config.subagents,
                                    updated_config.agent.mcp_discrete_review,
                                ))
                                .await;
                        } else {
                            command_live_subagent_service.clear();
                        }
                        if let Some(home) = codex_home {
                            command_subagent_codex_homes.push(home);
                        }
                        let review_fanout = match (pool, updated_roster.review_supervisor.clone()) {
                            (Some(workers), Some(supervisor)) => {
                                mj_core::orchestrator::ReviewFanout::available(
                                    crate::discrete_review::live_spawner(
                                        crate::discrete_review::FanoutConfig {
                                            workers,
                                            supervisor,
                                            cwd: side_cwd.clone(),
                                            additional_directories:
                                                command_review_additional_directories.clone(),
                                            session_tag: Some(
                                                command_live_subagent_options.session_tag.clone(),
                                            ),
                                            agent_stderr: None,
                                            snapshot_exclusions: command_snapshot_exclusions
                                                .clone(),
                                            fs_max_text_bytes,
                                            bifrost_analysis: updated_config.agent.bifrost_analysis,
                                            permission: updated_config.review.permission,
                                            bifrost_version: updated_config
                                                .review
                                                .bifrost_version
                                                .clone(),
                                            id_allocator: command_live_subagent_options
                                                .id_allocator
                                                .clone(),
                                        },
                                    ),
                                )
                            }
                            (workers, supervisor) => {
                                mj_core::orchestrator::ReviewFanout::unavailable(
                                    crate::review_fanout_error(
                                        workers.is_some(),
                                        supervisor.is_some(),
                                        &updated_config.subagents.model,
                                        updated_config.agent.needs_review_route(),
                                        &updated_roster.warnings,
                                    ),
                                )
                            }
                        };
                        primary_orchestrator.set_review_fanout(review_fanout);
                        primary_orchestrator
                            .set_review_policy_from_agent_config(&updated_config.agent);
                        tracker.observe_event(&UiEvent::Info(
                            "reviewer and subagent configuration is active for this server session"
                                .to_string(),
                        ));
                        continue;
                    }
                    if let UiCommand::RunReview { request } = command {
                        primary_orchestrator.request_review(request);
                        continue;
                    }
                    if matches!(command, UiCommand::CancelReview) {
                        primary_orchestrator.cancel_review();
                        continue;
                    }
                    if matches!(command, UiCommand::CompactPrimary)
                        || matches!(&command, UiCommand::SendPrompt { text, images, resources } if text == "/compact" && images.is_empty() && resources.is_empty())
                    {
                        primary_orchestrator.compact_manual().await;
                        continue;
                    }
                    tracker.observe_command(&command);
                    if let UiCommand::SendPrompt { text, images, .. } = &command {
                        local_epoch = local_epoch.saturating_add(1);
                        handoffs.store(0, std::sync::atomic::Ordering::Release);
                        let snapshot =
                            mj_core::workspace_snapshot::WorkspaceSnapshot::capture_excluding(
                                &workspace_roots,
                                &snapshot_exclusions,
                            )
                            .await;
                        primary_orchestrator
                            .begin_turn(local_epoch, text.clone(), images.clone(), snapshot)
                            .await;
                    }
                    if matches!(command, UiCommand::CancelPrompt) {
                        primary_orchestrator.cancel_review();
                    }
                    let shutdown = matches!(command, UiCommand::Shutdown);
                    if runtime_cmd_tx.send(command).is_err() || shutdown {
                        break;
                    }
                }
                if let Some(side) = side_runtime.take() {
                    tracker.finish_side_exit();
                    if let Some(message) = crate::side::discard(side, &side_agent, None).await {
                        let _ = side_event_tx.send(UiEvent::Warning(message));
                    }
                }
            })
        };
        tokio::pin!(runtime);
        tokio::pin!(command_proxy);
        let mut pending_permissions = std::collections::HashMap::new();
        let mut runtime_done = false;
        let mut runtime_events = orchestrated.events;
        let orchestrator_task = orchestrated.task;

        loop {
            tokio::select! {
                event = runtime_events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    if let UiEvent::SessionStarted { session_id, .. } = &event
                        && let Some(reporter) = launch_reporter.as_ref()
                    {
                        reporter.started(session_id);
                    }
                    if let UiEvent::SessionStarted { session_id, .. } = &event
                        && let Ok(mut current) = published_session_id.lock()
                    {
                        *current = Some(session_id.clone());
                    }
                    if let (Some(primary), UiEvent::SessionStarted { session_id, .. }) =
                        (provenance_primary.as_ref(), &event)
                    {
                        mj_core::session_provenance::record(mj_core::session_provenance::Record {
                            session_id: session_id.clone(),
                            cwd: provenance_cwd.clone(),
                            adapter_source_id: primary.launch.source_id.clone(),
                            model: primary.model.model.clone(),
                            model_value: primary.model_value.clone(),
                        });
                    }
                    remote::handle_server_agent_event(event, &tracker, &mut pending_permissions);
                }
                event = remote_event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    remote::handle_server_remote_event(event, &mut pending_permissions);
                }
                event = side_event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    remote::handle_server_side_event(event, &tracker, &mut pending_permissions);
                }
                joined = &mut runtime => {
                    if let Err(error) = joined {
                        debug!("server agent runtime task join failed: {error}");
                    }
                    runtime_done = true;
                    break;
                }
                joined = &mut command_proxy => {
                    if let Err(error) = joined {
                        debug!("server agent command proxy task join failed: {error}");
                    }
                    break;
                }
            }
        }
        if !runtime_done {
            let _ = shutdown_tx.send(UiCommand::Shutdown);
            let abort_handle = runtime.as_ref().abort_handle();
            match tokio::time::timeout(Duration::from_secs(2), &mut runtime).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => debug!("server agent runtime task join failed: {error}"),
                Err(_) => {
                    debug!("server agent runtime did not exit within 2s; aborting");
                    abort_handle.abort();
                }
            }
        }
        pending_permissions.clear();
        let _ = tokio::time::timeout(Duration::from_secs(2), orchestrator_task).await;
        tracker.shutdown().await;
    });

    ServerAgentSession {
        session_id,
        command_tx,
        task,
    }
}

impl ServerAgentSession {
    async fn shutdown(self) {
        let _ = self.command_tx.send(UiCommand::Shutdown);
        let abort_handle = self.task.abort_handle();
        match tokio::time::timeout(Duration::from_secs(2), self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!("server agent session task join failed: {error}"),
            Err(_) => {
                warn!("server agent session did not exit within 2s; aborting");
                abort_handle.abort();
            }
        }
    }
}

#[async_trait::async_trait]
impl remote::ServerSessionManager for RootServerSessionManager {
    fn resolve_cwd(&self) -> Option<PathBuf> {
        RootServerSessionManager::resolve_cwd(self)
    }
    fn request_roster_refresh(&self) {
        RootServerSessionManager::request_roster_refresh(self)
    }
    fn launch_state(&self, launch_id: u64) -> Option<ServerSessionLaunchState> {
        RootServerSessionManager::launch_state(self, launch_id)
    }
    fn start_session(&self, cwd: PathBuf) -> u64 {
        RootServerSessionManager::start_session(self, cwd)
    }
    fn resume_session(&self, cwd: PathBuf, session_id: String) -> u64 {
        RootServerSessionManager::resume_session(self, cwd, session_id)
    }
    fn owns_session(&self, session_id: &str) -> bool {
        RootServerSessionManager::owns_session(self, session_id)
    }
    async fn archive_session(&self, session_id: &str) -> bool {
        RootServerSessionManager::archive_session(self, session_id).await
    }
    async fn shutdown_all(&self) {
        RootServerSessionManager::shutdown_all(self).await
    }
    async fn reload_auxiliary_agents(&self) {
        RootServerSessionManager::reload_auxiliary_agents(self).await
    }
    async fn refresh_for_config(
        &self,
        config_path: &Path,
    ) -> std::result::Result<Option<roster::Roster>, String> {
        RootServerSessionManager::refresh_for_config(self, config_path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_roster() -> roster::Roster {
        let agent = roster::ResolvedAgent {
            model: crate::deepswe::Row {
                model: "test-model".to_string(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: "test-model".to_string(),
            launch: roster::AdapterLaunch {
                kind: roster::AdapterKind::Claude,
                source_id: "claude-acp".to_string(),
                command: PathBuf::from("false"),
                args: Vec::new(),
                env: Default::default(),
            },
            ranked: true,
            reasoning_effort: None,
        };
        roster::Roster {
            primary: agent.clone(),
            review_supervisor: None,
            subagent_default: None,
            available: vec![agent],
            choices: Vec::new(),
            warnings: Vec::new(),
            inventory: roster::AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
        }
    }

    fn unresolved_manager(reason: &str, config_path: &Path) -> RootServerSessionManager {
        RootServerSessionManager::new_unresolved(
            reason.to_string(),
            config_file_hash(config_path),
            config_path.parent().expect("parent dir").to_path_buf(),
            Vec::new(),
            Vec::new(),
            0,
        )
    }

    #[tokio::test]
    async fn unresolved_manager_fails_launches_with_the_setup_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = unresolved_manager("no model is launchable", &dir.path().join("config.toml"));
        let launch_id = manager.start_session(dir.path().to_path_buf());
        match manager.launch_state(launch_id) {
            Some(ServerSessionLaunchState::Failed { error }) => {
                assert_eq!(error, "no model is launchable");
            }
            other => panic!("expected a failed launch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn active_server_session_receives_an_auxiliary_route_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = unresolved_manager("setup pending", &dir.path().join("config.toml"));
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending::<()>());
        manager
            .sessions
            .lock()
            .expect("sessions lock")
            .push(ServerAgentSession {
                session_id: Arc::new(Mutex::new(Some("server-session".to_string()))),
                command_tx,
                task,
            });

        manager.reload_auxiliary_agents().await;

        assert!(matches!(
            command_rx.try_recv(),
            Ok(UiCommand::ReloadAuxiliaryAgents)
        ));
        let session = manager
            .sessions
            .lock()
            .expect("sessions lock")
            .pop()
            .expect("test session");
        session.task.abort();
    }

    #[tokio::test]
    async fn unbound_manager_rebinds_on_refresh_despite_an_unchanged_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let manager = unresolved_manager("setup pending", &config_path);
        // The config file has not changed since startup (both hashes are for
        // the missing file); only the unbound state forces the re-resolve.
        let refreshed = manager
            .refresh_for_config_with(&config_path, |_, _| async { Ok(stub_roster()) })
            .await;
        assert!(matches!(refreshed, Ok(Some(_))), "{refreshed:?}");
        let launch = manager.launch.read().expect("launch lock");
        assert!(launch.binding.is_bound());
    }

    #[tokio::test]
    async fn bound_manager_skips_refresh_for_an_unchanged_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let manager = RootServerSessionManager::new_roster(
            stub_roster(),
            config_file_hash(&config_path),
            dir.path().to_path_buf(),
            Vec::new(),
            Vec::new(),
            0,
        );
        let refreshed = manager
            .refresh_for_config_with(&config_path, |_, _| async {
                panic!("a bound manager must not re-resolve an unchanged config")
            })
            .await;
        assert!(matches!(refreshed, Ok(None)), "{refreshed:?}");
    }

    #[tokio::test]
    async fn unbound_manager_stays_unbound_when_resolution_still_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let manager = unresolved_manager("setup pending", &config_path);
        let refreshed = manager
            .refresh_for_config_with(&config_path, |_, _| async {
                Err(anyhow::anyhow!("still no model"))
            })
            .await;
        assert!(
            matches!(refreshed, Err(ref error) if error == "still no model"),
            "{refreshed:?}"
        );
        let launch_id = manager.start_session(dir.path().to_path_buf());
        match manager.launch_state(launch_id) {
            Some(ServerSessionLaunchState::Failed { error }) => {
                assert_eq!(error, "setup pending");
            }
            other => panic!("expected a failed launch, got {other:?}"),
        }
    }
}
