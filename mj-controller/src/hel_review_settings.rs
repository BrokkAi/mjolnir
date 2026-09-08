//! Background discovery against the same workers and adapters that run review.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hel::hel_acp::{SessionConfigChoice, session_config_choices};
use hel::hel_targets::CancellableProcessExecutor;
use hel::hel_worker_launch::ReviewerLaunchConfig;
use tokio::sync::mpsc::UnboundedSender;

use crate::hel_controller::Controller;
use crate::hel_session_manager::{
    ManagedSessionHandle, ReviewerAction, ReviewerOutcome, SessionManagerControl,
};
use crate::hel_worker_client::StartedReviewer;

const REVIEW_DISCOVERY_CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
const REVIEW_DISCOVERY_STAGING_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiscoveryRequest {
    pub profile: String,
    pub model: Option<String>,
    pub preferred_session: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewCapabilityChoices {
    pub model_choices: Vec<SessionConfigChoice>,
    pub effort_choices: Vec<SessionConfigChoice>,
    /// Whether effort choices were obtained for the selected model. An
    /// explicit model that the adapter does not advertise leaves this false,
    /// even when the initial startup advertised generic effort choices.
    pub effort_capabilities_discovered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiscoveryOutcome {
    Available {
        choices: ReviewCapabilityChoices,
        cleanup_warning: Option<String>,
    },
    Unavailable,
}

/// Discover review selectors from one already connected active worker.
///
/// The selected dashboard session is preferred when it is an eligible
/// worker. Otherwise eligible sessions are considered by session id. The
/// reviewer is staged without MCP servers and started only to read its ACP
/// configuration choices; no review prompt, repository inspection, or tool
/// verification is performed.
pub async fn discover_review_settings(
    control: SessionManagerControl,
    request: ReviewDiscoveryRequest,
    cancelled: Arc<AtomicBool>,
    progress: UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewDiscoveryOutcome, String> {
    check_cancelled(&cancelled)?;
    let controller = Arc::new(
        tokio::task::spawn_blocking(Controller::load)
            .await
            .map_err(|error| format!("load review settings task failed: {error}"))?
            .map_err(|error| format!("load review settings: {error:#}"))?,
    );
    if !controller.config.profiles.contains_key(&request.profile) {
        return Err(format!("Unknown reviewer profile {:?}", request.profile));
    }
    if !controller.config.profiles[&request.profile]
        .kind
        .supports_injected_mcp()
    {
        return Err("Muse Code cannot be a reviewer because muse-acp does not accept the required MCP tools".into());
    }

    let Some((session_id, handle)) = select_worker(
        &control,
        &controller,
        request.preferred_session.as_deref(),
        &cancelled,
    )
    .await?
    else {
        return Ok(ReviewDiscoveryOutcome::Unavailable);
    };

    // Once a worker has been selected, every path through the attempt runs a
    // bounded cleanup. This includes cancellation and a failed Start: a
    // worker can launch its process before returning a configuration error.
    let generation = crate::hel_review_host::next_review_generation()?;
    discover_selected_worker(
        Arc::clone(&controller),
        session_id,
        generation,
        handle,
        &request,
        &cancelled,
        &progress,
    )
    .await
}

async fn discover_selected_worker(
    controller: Arc<Controller>,
    session_id: String,
    generation: u64,
    handle: ManagedSessionHandle,
    request: &ReviewDiscoveryRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewDiscoveryOutcome, String> {
    let role = format!("settings-{generation:016x}");
    let discovery = discover_on_worker(
        controller,
        &session_id,
        generation,
        &handle,
        request,
        cancelled,
        progress,
    )
    .await;
    let cleanup = cleanup_worker(&handle, &role).await;
    if let Err(error) = &cleanup {
        tracing::warn!(
            session_id = %session_id,
            role = %role,
            error = %error,
            "review settings discovery cleanup failed"
        );
    }

    match (discovery, cleanup) {
        (Ok(choices), Ok(())) => Ok(ReviewDiscoveryOutcome::Available {
            choices,
            cleanup_warning: None,
        }),
        (Ok(choices), Err(warning)) => Ok(ReviewDiscoveryOutcome::Available {
            choices,
            cleanup_warning: Some(warning),
        }),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
    }
}

async fn select_worker(
    control: &SessionManagerControl,
    controller: &Controller,
    preferred_session: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<Option<(String, ManagedSessionHandle)>, String> {
    let mut session_ids = controller
        .state
        .sessions
        .iter()
        .filter(|(_, session)| session.target.is_some() && session.state.is_active())
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();
    session_ids.sort();
    if let Some(preferred) = preferred_session
        && let Some(index) = session_ids
            .iter()
            .position(|session_id| session_id == preferred)
    {
        let selected = session_ids.remove(index);
        session_ids.insert(0, selected);
    }

    for session_id in session_ids {
        check_cancelled(cancelled)?;
        let handle = match cancellable(cancelled, async {
            control
                .session(session_id.clone())
                .await
                .map_err(|error| format!("{error:#}"))
        })
        .await
        {
            Ok(handle) => handle,
            Err(_) => {
                check_cancelled(cancelled)?;
                continue;
            }
        };
        let view = handle.view();
        if view.connected && !handle.is_stopped() {
            return Ok(Some((session_id, handle)));
        }
    }
    check_cancelled(cancelled)?;
    Ok(None)
}

async fn discover_on_worker(
    controller: Arc<Controller>,
    session_id: &str,
    generation: u64,
    handle: &ManagedSessionHandle,
    request: &ReviewDiscoveryRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewCapabilityChoices, String> {
    let role = format!("settings-{generation:016x}");
    check_cancelled(cancelled)?;
    let session_id_for_stage = session_id.to_owned();
    let profile = request.profile.clone();
    let flag = Arc::clone(cancelled);
    let config = tokio::task::spawn_blocking(move || {
        let executor =
            CancellableProcessExecutor::new(flag).with_deadline(REVIEW_DISCOVERY_STAGING_TIMEOUT);
        controller.stage_reviewer_profile_controlled(
            &session_id_for_stage,
            &profile,
            generation,
            &[],
            &executor,
        )
    })
    .await
    .map_err(|error| format!("Reviewer staging task failed: {error}"))?
    .map_err(|error| format!("Stage reviewer: {error:#}"))?;

    let mut config = config;
    config.model = None;
    config.effort = None;
    let started = cancellable(cancelled, start(handle, &role, &config)).await?;
    let model_choices = session_config_choices(&started.config_options, "model");
    let initial_effort_choices = session_config_choices(&started.config_options, "effort");
    let mut choices = ReviewCapabilityChoices {
        model_choices,
        effort_choices: initial_effort_choices,
        effort_capabilities_discovered: true,
    };

    if let Some(model) = &request.model {
        if !choices
            .model_choices
            .iter()
            .any(|choice| choice.value == *model)
        {
            // The initial Start still gave us useful model choices. Its
            // generic effort choices cannot be claimed for an unsupported
            // explicit model.
            choices.effort_choices.clear();
            choices.effort_capabilities_discovered = false;
        } else {
            check_cancelled(cancelled)?;
            config.model = Some(model.clone());
            config.effort = None;
            let started = cancellable(cancelled, start(handle, &role, &config)).await?;
            choices.effort_choices = session_config_choices(&started.config_options, "effort");
            choices.effort_capabilities_discovered = true;
        }
    }

    check_cancelled(cancelled)?;
    progress
        .send(choices.clone())
        .map_err(|_| "Review capability progress receiver closed".to_owned())?;
    Ok(choices)
}

async fn cleanup_worker(handle: &ManagedSessionHandle, role: &str) -> Result<(), String> {
    match tokio::time::timeout(
        REVIEW_DISCOVERY_CLEANUP_TIMEOUT,
        call(handle, role, ReviewerAction::Pause),
    )
    .await
    {
        Ok(Ok(ReviewerOutcome::Paused)) => Ok(()),
        Ok(Ok(_)) => Err("Unexpected response while stopping review settings discovery".to_owned()),
        Ok(Err(error)) => Err(format!("Could not stop review settings discovery: {error}")),
        Err(_) => Err(format!(
            "Could not stop review settings discovery within {} seconds",
            REVIEW_DISCOVERY_CLEANUP_TIMEOUT.as_secs()
        )),
    }
}

async fn start(
    handle: &ManagedSessionHandle,
    role: &str,
    config: &ReviewerLaunchConfig,
) -> Result<StartedReviewer, String> {
    match call(
        handle,
        role,
        ReviewerAction::Start {
            config: Box::new(config.clone()),
        },
    )
    .await?
    {
        ReviewerOutcome::Started(started) => Ok(*started),
        _ => Err("Worker returned an unexpected reviewer startup response".to_owned()),
    }
}

async fn call(
    handle: &ManagedSessionHandle,
    role: &str,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    handle
        .reviewer_as(Some(role.to_owned()), action)
        .await
        .map_err(|error| format!("{error:#}"))
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("Review settings discovery cancelled".to_owned())
    } else {
        Ok(())
    }
}

async fn cancellable<T>(
    cancelled: &AtomicBool,
    operation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = async {
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            while !cancelled.load(Ordering::Acquire) {
                interval.tick().await;
            }
        } => Err("Review settings discovery cancelled".to_owned()),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigSelectOption, SessionConfigSelectOptions,
    };
    use hel::hel_config::{HarnessKind, HarnessProfile, HelConfig, TargetTemplate};
    use hel::hel_state::{HelState, SessionRecord, SessionState, TargetLocator};
    use hel::hel_targets::CommandSpec;
    use hel::hel_worker::{RelayExecutionState, RelayOperationalState};
    use tokio::sync::watch;

    use super::*;
    use crate::hel_session_manager::{
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
                        git_broker: None,
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
            store_id: None,
            idle_since_ms: None,
            session_id: session_id.to_owned(),
            execution: RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            acknowledged_through: 0,
            acknowledged_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            native_session_id: Some(session_id.to_owned()),
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
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
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
        let mut config = HelConfig::default();
        config.profiles.insert(
            "reviewer".to_owned(),
            HarnessProfile {
                kind: HarnessKind::Claude,
                home: profile_home,
                environment: BTreeMap::new(),
                context_window_bytes: None,
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
                        id: (*session_id).to_owned(),
                        workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
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
            state: HelState {
                sessions,
                ..HelState::default()
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
        let controller =
            controller_fixture(directory.path(), &["worker-z", "worker-a", "worker-m"]);
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
}
