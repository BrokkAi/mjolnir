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
    agent: SelectedAgent,
    roster: Option<roster::Roster>,
    config_hash: Option<u64>,
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
        Self {
            launch: RwLock::new(ServerSessionLaunch {
                agent: selected_agent_for_roster(&roster),
                roster: Some(roster),
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
            if !refresh_requested && launch.config_hash == hash {
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
                launch.agent = selected_agent_for_roster(&roster);
                launch.roster = Some(roster.clone());
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
        let session = start_server_agent_session(
            launch.agent,
            launch.roster,
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
            cwd: Some(cwd.clone()),
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
            quota_gate,
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
    let (subagent_reports, subagent_report_rx) = subagent::SubagentReportBus::channel();
    // Shared with the orchestrator so every wake can ask the still-running
    // subagents for progress.
    let subagent_runs = subagent::SubagentRegistry::default();
    // The discrete review's specialist lanes run on the subagent pool and share
    // the primary's workspace roots, so both have to be cloned before they move
    // into the subagent config and the runtime config respectively.
    let review_workers = subagent_pool.clone();
    let review_additional_directories = additional_directories.clone();
    let subagents = subagent_pool
        .map(|subagent_pool| {
            subagent::Config::new(subagent_pool, None)
                .with_subagent_handoff_counter(subagent_handoffs.clone())
                .with_id_allocator(subagent_ids.clone())
                .with_active_implementation_workers(active_implementation_workers.clone())
                .with_max_parallel(app_config.subagents.max_parallel)
                .with_debrief(app_config.subagents.debrief)
                .with_permission_mode(app_config.subagents.permission)
                .with_reports(subagent_reports.clone())
                .with_run_registry(subagent_runs.clone())
                .with_prewarm(subagent::RunContext {
                    cwd: cwd.clone(),
                    additional_directories: additional_directories.clone(),
                    snapshot_exclusions: snapshot_exclusions.clone(),
                    fs_max_text_bytes,
                    access_mode: mj_core::acp::RuntimeAccessMode::Full,
                })
        })
        .map(subagent::runtime_service);
    let provenance_primary = roster.as_ref().map(|resolved| resolved.primary.clone());
    let provenance_cwd = cwd.clone();
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
            max_correction_rounds: app_config.agent.max_correction_rounds,
            primary_model: roster
                .as_ref()
                .map(|resolved| resolved.primary.model.model.clone()),
            review_root: provenance_cwd.clone(),
            review_fanout: review_workers
                .zip(
                    roster
                        .as_ref()
                        .and_then(|resolved| resolved.review_supervisor.clone()),
                )
                .map(|(workers, supervisor)| {
                    crate::discrete_review::live_spawner(crate::discrete_review::FanoutConfig {
                        workers,
                        supervisor,
                        cwd: provenance_cwd.clone(),
                        additional_directories: review_additional_directories,
                        session_tag: Some(format!("remote-{}", std::process::id())),
                        agent_stderr: None,
                        snapshot_exclusions: snapshot_exclusions.clone(),
                        fs_max_text_bytes,
                        permission: app_config.review.permission,
                        id_allocator: subagent_ids.clone(),
                    })
                }),
        },
    );
    let primary_orchestrator = orchestrated.handle.clone();

    let task = tokio::spawn(async move {
        let _subagent_homes = subagent_codex_home;
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
                    if let UiCommand::RunReview { target } = command {
                        primary_orchestrator.request_review(target);
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
    async fn refresh_for_config(
        &self,
        config_path: &Path,
    ) -> std::result::Result<Option<roster::Roster>, String> {
        RootServerSessionManager::refresh_for_config(self, config_path).await
    }
}
