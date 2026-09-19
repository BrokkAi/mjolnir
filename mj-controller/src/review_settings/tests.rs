use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigSelectOption, SessionConfigSelectOptions,
};
use mj_core::config::{Config, HarnessKind, HarnessProfile, TargetTemplate};
use mj_core::state::{SessionRecord, SessionState, State, TargetLocator};

use crate::targets::CommandSpec;
use mj_core::relay::{RelayExecutionState, RelayOperationalState};

use tokio::sync::watch;

use super::*;
use crate::session_manager::{
    ManagedSessionView, RelaySessionTarget, RemoteSessionPublisher, RemoteSessionRequest,
    RemoteSessionRequests, SessionManagerShutdown, spawn_remote_session_manager,
};

const SESSION: &str = "0123456789abcdef0123456789abcdef";

struct FakeManager {
    control: SessionManagerControl,
    requests: RemoteSessionRequests,
    publisher: RemoteSessionPublisher,
    _shutdown: SessionManagerShutdown,
    _targets: watch::Sender<Vec<RelaySessionTarget>>,
}

impl FakeManager {
    async fn new(session_ids: &[&str]) -> Self {
        let channels = spawn_remote_session_manager().expect("remote manager");
        channels.targets.send_replace(
            session_ids
                .iter()
                .map(|session_id| RelaySessionTarget {
                    session_id: (*session_id).to_owned(),
                    spec: CommandSpec::new("true", Vec::<String>::new()),
                    worker_recovery: None,
                    project_memory: None,
                })
                .collect(),
        );
        let manager = Self {
            control: channels.control,
            requests: channels.requests,
            publisher: channels.publisher,
            _shutdown: channels.shutdown,
            _targets: channels.targets,
        };
        for session_id in session_ids {
            manager
                .publisher
                .publish(
                    (*session_id).to_owned(),
                    ManagedSessionView {
                        connected: true,
                        ..ManagedSessionView::default()
                    },
                )
                .await
                .expect("publish the managed session view");
        }
        for session_id in session_ids {
            manager.wait_connected(session_id, true).await;
        }
        manager
    }

    async fn wait_connected(&self, session_id: &str, connected: bool) {
        let mut handle = self.handle(session_id).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.view().connected != connected {
                handle.changed().await.expect("published view");
            }
        })
        .await
        .expect("connection update applied");
    }

    async fn handle(&self, session_id: &str) -> ManagedSessionHandle {
        self.control
            .wait_for_session(session_id, Duration::from_secs(5))
            .await
            .expect("the fake manager manages the session")
    }

    async fn next(&mut self) -> RemoteSessionRequest {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("the discovery makes a request")
            .expect("the remote manager remains alive")
    }
}

fn option(key: &str, values: &[&str]) -> SessionConfigOption {
    SessionConfigOption::select(
        key.to_owned(),
        key.to_owned(),
        values[0].to_owned(),
        SessionConfigSelectOptions::Ungrouped(
            values
                .iter()
                .map(|value| {
                    SessionConfigSelectOption::new((*value).to_owned(), (*value).to_owned())
                })
                .collect(),
        ),
    )
}

fn advertised(models: &[&str], efforts: &[&str]) -> Vec<SessionConfigOption> {
    let mut choices = Vec::new();
    if !models.is_empty() {
        choices.push(option("model", models));
    }
    if !efforts.is_empty() {
        choices.push(option("effort", efforts));
    }
    choices
}

fn operational(session_id: &str) -> RelayOperationalState {
    RelayOperationalState {
        native_agent_count: 0,
        expected_continuation: None,
        goal: Default::default(),
        capacity_retry: None,
        store_id: None,
        idle_since_ms: None,
        session_id: session_id.to_owned(),
        execution: RelayExecutionState::Idle,
        latest_ordinal: 0,
        latest_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        acknowledged_through: 0,
        acknowledged_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        recovery_floor_ordinal: 0,
        recovery_floor_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        native_session_id: Some(session_id.to_owned()),
        native_continuity_lost: false,
        checkpoint_only: false,
        acp_ready: None,
        agent_capabilities: None,
        agent_info: None,
        steering_supported: None,
        config_options: Vec::new(),
        modes: None,
        available_commands: Vec::new(),
        config: BTreeMap::new(),
        active_prompt: None,
        queued_prompts: Vec::new(),
        active_user_shells: Vec::new(),
        active_agent_terminals: Vec::new(),
        checkpoint_barrier: None,
        checkpoint_ready: None,
        last_acp_activity_at_ms: None,
        activity_turn_started_at_ms: None,
        current_step_started_at_ms: None,
        foreground_tool_started_at_ms: None,
        tools_in_flight: Vec::new(),
        activity: None,
        harness_turn: None,
        last_harness_turn_started_ordinal: None,
        background_commands: Vec::new(),
        background_work_known: None,
    }
}

fn started(options: Vec<SessionConfigOption>) -> Result<ReviewerOutcome, String> {
    Ok(ReviewerOutcome::Started(Box::new(StartedReviewer {
        native_session_id: Some("native-settings-test".to_owned()),
        config_options: options,
        reused: false,
        state: operational("native-settings-test"),
    })))
}

fn controller_fixture(directory: &Path, session_ids: &[&str]) -> Controller {
    let profile_home = directory.join("reviewer");
    fs::create_dir_all(&profile_home).expect("profile home");
    fs::write(profile_home.join("settings.json"), b"{}").expect("profile settings");
    let mut config = Config::default();
    config.profiles.insert(
        "reviewer".to_owned(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Claude,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    config
        .targets
        .insert("local".to_owned(), TargetTemplate::LocalBare);
    let sessions = session_ids
        .iter()
        .map(|session_id| {
            let worker_root = directory.join(session_id);
            fs::create_dir_all(&worker_root).expect("worker root");
            (
                (*session_id).to_owned(),
                SessionRecord {
                    build_cache: None,
                    container_workspace: None,
                    mjolnir_subagents: None,
                    create_managed_worktree: None,
                    id: (*session_id).to_owned(),
                    workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                    title: "review settings test".to_owned(),
                    harness_kind: HarnessKind::Codex,
                    last_profile: "primary".to_owned(),
                    bundle_id: "project".to_owned(),
                    project_directory: None,
                    managed_worktree: None,
                    target_template_id: "local".to_owned(),
                    resource_allocation: None,
                    additional_mounts: Vec::new(),
                    container_cpus: None,
                    container_memory: None,
                    state: SessionState::Running,
                    archived: false,
                    target: Some(TargetLocator::LocalBare { worker_root }),
                    native_session_id: Some("native-primary".to_owned()),
                    acp_session_title: None,
                    session_title_override: None,
                    created_at: "2026-08-12T00:00:00Z".to_owned(),
                    updated_at: "2026-08-12T00:00:00Z".to_owned(),
                    viewed_through_event_ordinal: 0,
                    draft_input: String::new(),
                    last_error: None,
                    last_checkpoint_error: None,
                    checkpoint: None,
                },
            )
        })
        .collect();
    Controller {
        config,
        state: State {
            sessions,
            ..State::default()
        },
    }
}

fn start_request(
    request: RemoteSessionRequest,
) -> (
    Box<ReviewerLaunchConfig>,
    tokio::sync::oneshot::Sender<Result<ReviewerOutcome, String>>,
) {
    let RemoteSessionRequest::Reviewer {
        action: ReviewerAction::Start { config },
        reply,
        ..
    } = request
    else {
        panic!("discovery must use reviewer Start");
    };
    (config, reply)
}

fn pause_request(
    request: RemoteSessionRequest,
) -> tokio::sync::oneshot::Sender<Result<ReviewerOutcome, String>> {
    let RemoteSessionRequest::Reviewer {
        action: ReviewerAction::Pause,
        reply,
        ..
    } = request
    else {
        panic!("discovery cleanup must use reviewer Pause");
    };
    reply
}

fn request(model: Option<&str>) -> ReviewDiscoveryRequest {
    ReviewDiscoveryRequest {
        profile: "reviewer".to_owned(),
        model: model.map(str::to_owned),
        preferred_session: Some(SESSION.to_owned()),
    }
}

async fn discover_for_test(
    controller: Arc<Controller>,
    handle: ManagedSessionHandle,
    request: ReviewDiscoveryRequest,
    cancelled: Arc<AtomicBool>,
    progress: UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewDiscoveryOutcome, String> {
    discover_selected_worker(
        controller,
        SESSION.to_owned(),
        1,
        handle,
        &request,
        &cancelled,
        &progress,
    )
    .await
}

#[tokio::test]
async fn discovery_uses_empty_start_and_pauses_after_publishing_choices() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let controller = Arc::new(controller_fixture(directory.path(), &[SESSION]));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(discover_for_test(
        controller,
        handle,
        request(None),
        cancelled,
        progress,
    ));

    let (config, reply) = start_request(manager.next().await);
    assert_eq!(config.model, None);
    assert_eq!(config.effort, None);
    assert!(config.mcp_servers.is_empty());
    reply
        .send(started(advertised(&["fast", "deep"], &["low", "high"])))
        .expect("startup reply");
    let choices = progress_rx.recv().await.expect("progress before cleanup");
    assert_eq!(choices.model_choices.len(), 2);
    assert_eq!(choices.effort_choices.len(), 2);
    assert!(choices.effort_capabilities_discovered);

    let reply = pause_request(manager.next().await);
    reply
        .send(Ok(ReviewerOutcome::Paused))
        .expect("cleanup reply");
    let ReviewDiscoveryOutcome::Available {
        choices: result,
        cleanup_warning,
    } = task.await.expect("discovery task").expect("choices")
    else {
        panic!("discovery should be available");
    };
    assert_eq!(result, choices);
    assert!(cleanup_warning.is_none());
}

#[tokio::test]
async fn supported_model_starts_again_without_applying_effort() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let controller = Arc::new(controller_fixture(directory.path(), &[SESSION]));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(discover_for_test(
        controller,
        handle,
        request(Some("deep")),
        cancelled,
        progress,
    ));

    let (_, reply) = start_request(manager.next().await);
    reply
        .send(started(advertised(&["fast", "deep"], &["low"])))
        .expect("initial startup reply");
    let (config, reply) = start_request(manager.next().await);
    assert_eq!(config.model.as_deref(), Some("deep"));
    assert_eq!(config.effort, None);
    assert!(config.mcp_servers.is_empty());
    reply
        .send(started(advertised(&["fast", "deep"], &["low", "high"])))
        .expect("model startup reply");
    let choices = progress_rx.recv().await.expect("progress");
    assert_eq!(choices.effort_choices.len(), 2);
    assert!(choices.effort_capabilities_discovered);
    let reply = pause_request(manager.next().await);
    reply
        .send(Ok(ReviewerOutcome::Paused))
        .expect("cleanup reply");
    task.await.expect("discovery task").expect("choices");
}

#[tokio::test]
async fn unsupported_model_keeps_models_without_claiming_efforts() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let controller = Arc::new(controller_fixture(directory.path(), &[SESSION]));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(discover_for_test(
        controller,
        handle,
        request(Some("missing")),
        cancelled,
        progress,
    ));

    let (_, reply) = start_request(manager.next().await);
    reply
        .send(started(advertised(&["fast", "deep"], &["low", "high"])))
        .expect("startup reply");
    let choices = progress_rx.recv().await.expect("progress");
    assert_eq!(choices.model_choices.len(), 2);
    assert!(choices.effort_choices.is_empty());
    assert!(!choices.effort_capabilities_discovered);
    // Pause must be the very next request: no attempt applies the
    // unsupported model or its effort before cleanup.
    let reply = pause_request(manager.next().await);
    reply
        .send(Ok(ReviewerOutcome::Paused))
        .expect("cleanup reply");
    task.await.expect("discovery task").expect("choices");
}

#[tokio::test]
async fn cleanup_failure_keeps_choices_as_a_warning() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let controller = Arc::new(controller_fixture(directory.path(), &[SESSION]));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(discover_for_test(
        controller,
        handle,
        request(None),
        cancelled,
        progress,
    ));
    let (_, reply) = start_request(manager.next().await);
    reply
        .send(started(advertised(&["fast"], &["low"])))
        .expect("startup reply");
    progress_rx.recv().await.expect("choices before cleanup");
    let reply = pause_request(manager.next().await);
    reply
        .send(Err("worker vanished".to_owned()))
        .expect("cleanup reply");
    let outcome = task.await.expect("discovery task").expect("outcome");
    let ReviewDiscoveryOutcome::Available {
        choices,
        cleanup_warning,
    } = outcome
    else {
        panic!("choices remain available after cleanup failure");
    };
    assert_eq!(choices.model_choices.len(), 1);
    assert!(cleanup_warning.unwrap().contains("worker vanished"));
}

#[tokio::test]
async fn cancellation_and_start_error_still_pause_without_progress() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let controller = Arc::new(controller_fixture(directory.path(), &[SESSION]));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(discover_for_test(
        controller,
        handle,
        request(None),
        cancelled.clone(),
        progress,
    ));
    let (_, reply) = start_request(manager.next().await);
    cancelled.store(true, Ordering::Release);
    drop(reply);
    let reply = pause_request(manager.next().await);
    reply
        .send(Ok(ReviewerOutcome::Paused))
        .expect("cleanup reply");
    assert!(task.await.expect("discovery task").is_err());
    assert!(progress_rx.try_recv().is_err());

    let directory = tempfile::tempdir().expect("fixture directory");
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let controller = Arc::new(controller_fixture(directory.path(), &[SESSION]));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(discover_for_test(
        controller,
        handle,
        request(None),
        cancelled,
        progress,
    ));
    let (_, reply) = start_request(manager.next().await);
    reply
        .send(Err("adapter failed".to_owned()))
        .expect("startup reply");
    let reply = pause_request(manager.next().await);
    reply
        .send(Ok(ReviewerOutcome::Paused))
        .expect("cleanup reply");
    assert!(task.await.expect("discovery task").is_err());
    assert!(progress_rx.try_recv().is_err());
}

#[tokio::test]
async fn selected_worker_precedes_sorted_connected_active_workers() {
    let manager = FakeManager::new(&["worker-z", "worker-a", "worker-m"]).await;
    let directory = tempfile::tempdir().expect("fixture directory");
    let controller = controller_fixture(directory.path(), &["worker-z", "worker-a", "worker-m"]);
    let selected = select_worker(
        &manager.control,
        &controller,
        Some("worker-z"),
        &AtomicBool::new(false),
    )
    .await
    .expect("worker selection")
    .expect("selected worker");
    assert_eq!(selected.0, "worker-z");

    let fallback = select_worker(
        &manager.control,
        &controller,
        Some("missing"),
        &AtomicBool::new(false),
    )
    .await
    .expect("worker selection")
    .expect("fallback worker");
    assert_eq!(fallback.0, "worker-a");
}

#[tokio::test]
async fn unavailable_workers_are_skipped_and_no_worker_is_reported_honestly() {
    let manager = FakeManager::new(&["worker-a", "worker-b", "worker-c"]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut controller =
        controller_fixture(directory.path(), &["worker-a", "worker-b", "worker-c"]);
    controller.state.sessions.get_mut("worker-a").unwrap().state = SessionState::Stopped;
    manager
        .publisher
        .publish("worker-b".to_owned(), ManagedSessionView::default())
        .await
        .unwrap();
    manager.wait_connected("worker-b", false).await;
    let selected = select_worker(
        &manager.control,
        &controller,
        Some("worker-b"),
        &AtomicBool::new(false),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(selected.0, "worker-c");
    controller
        .state
        .sessions
        .get_mut("worker-c")
        .unwrap()
        .target = None;
    assert!(
        select_worker(&manager.control, &controller, None, &AtomicBool::new(false))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cleanup_is_bounded_when_the_worker_never_replies() {
    let mut manager = FakeManager::new(&[SESSION]).await;
    let handle = manager.handle(SESSION).await;
    let task = tokio::spawn(async move { cleanup_worker(&handle, "settings-test").await });
    let reply = pause_request(manager.next().await);
    tokio::time::pause();
    tokio::time::advance(REVIEW_DISCOVERY_CLEANUP_TIMEOUT + Duration::from_secs(1)).await;
    assert!(
        task.await
            .unwrap()
            .unwrap_err()
            .contains("within 15 seconds")
    );
    assert!(reply.is_closed());
}
