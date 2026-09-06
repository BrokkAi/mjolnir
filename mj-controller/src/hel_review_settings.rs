//! Background discovery against the same workers and adapters that run review.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{StreamExt, stream};
use hel::hel_acp::{SessionConfigChoice, session_config_choices};
use hel::hel_review::bifrost::review_mcp_servers;
use hel::hel_targets::CancellableProcessExecutor;
use hel::hel_worker::{AnalyzeDeltaRepository, RepoDelta};
use hel::hel_worker_launch::ReviewerLaunchConfig;
use tokio::sync::mpsc::UnboundedSender;

use crate::hel_controller::Controller;
use crate::hel_review_host::{next_review_generation, validate_reviewer_assignment};
use crate::hel_session_manager::{
    ManagedSessionHandle, ReviewerAction, ReviewerOutcome, SessionManagerControl,
};
use crate::hel_worker_client::StartedReviewer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewProbeRequest {
    pub profile: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewCapabilityChoices {
    pub model_choices: Vec<SessionConfigChoice>,
    pub effort_choices: Vec<SessionConfigChoice>,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewReadinessReport {
    pub model_choices: Vec<SessionConfigChoice>,
    pub effort_choices: Vec<SessionConfigChoice>,
    /// Empty means unverified: no attached target was available to inspect.
    pub targets: Vec<ReviewTargetReadiness>,
}

#[derive(Debug, Clone)]
pub struct ReviewTargetReadiness {
    pub target: String,
    pub ready: bool,
    pub message: String,
}

/// Discover choices without prompting a model or provisioning a hidden session.
/// Each actual placement is checked independently: a template name alone does
/// not prove that two existing containers have the same adapter installed.
pub async fn probe_review_settings(
    control: SessionManagerControl,
    request: ReviewProbeRequest,
    cancelled: Arc<AtomicBool>,
) -> Result<ReviewReadinessReport, String> {
    let (progress, _progress_receiver) = tokio::sync::mpsc::unbounded_channel();
    probe_review_settings_with_progress(control, request, cancelled, progress).await
}

/// Discover review settings while reporting the first target's advertised
/// selectors as soon as capability probing completes. The final report still
/// waits for every target's readiness checks.
pub async fn probe_review_settings_with_progress(
    control: SessionManagerControl,
    request: ReviewProbeRequest,
    cancelled: Arc<AtomicBool>,
    progress: UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewReadinessReport, String> {
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
    let mut placements = Vec::new();
    let mut targets = Vec::new();
    for session in controller.state.sessions.values() {
        let Some(placement) = session.target.as_ref() else {
            continue;
        };
        if placements.contains(placement) {
            continue;
        }
        let Ok(handle) = control.session(&session.id).await else {
            continue;
        };
        if !handle.view().connected || handle.is_stopped() {
            continue;
        }
        placements.push(placement.clone());
        targets.push((
            session.id.clone(),
            format!("{} ({})", session.title, session.target_template_id),
            handle,
        ));
    }
    targets.sort_by(|left, right| left.0.cmp(&right.0));
    let reports = stream::iter(targets.into_iter().enumerate().map(
        |(index, (session, label, handle))| {
            let controller = Arc::clone(&controller);
            let request = request.clone();
            let cancelled = Arc::clone(&cancelled);
            let progress = (index == 0).then(|| progress.clone());
            async move {
                probe_target(
                    controller, session, label, handle, request, cancelled, progress,
                )
                .await
            }
        },
    ))
    .buffered(3)
    .collect::<Vec<_>>()
    .await;
    check_cancelled(&cancelled)?;
    let mut report = ReviewReadinessReport::default();
    let mut choices_selected = false;
    for (target, models, efforts) in reports {
        // The first adapter supplies the picker; every target independently
        // validates that selection and reports incompatibilities below it.
        if !choices_selected && (!models.is_empty() || !efforts.is_empty() || target.ready) {
            report.model_choices = models;
            report.effort_choices = efforts;
            choices_selected = true;
        }
        report.targets.push(target);
    }
    Ok(report)
}

async fn probe_target(
    controller: Arc<Controller>,
    session: String,
    target: String,
    handle: ManagedSessionHandle,
    request: ReviewProbeRequest,
    cancelled: Arc<AtomicBool>,
    progress: Option<UnboundedSender<ReviewCapabilityChoices>>,
) -> (
    ReviewTargetReadiness,
    Vec<SessionConfigChoice>,
    Vec<SessionConfigChoice>,
) {
    let mut models = Vec::new();
    let mut efforts = Vec::new();
    let generation = match next_review_generation() {
        Ok(generation) => generation,
        Err(message) => {
            return (
                ReviewTargetReadiness {
                    target,
                    ready: false,
                    message,
                },
                models,
                efforts,
            );
        }
    };
    let role = format!("readiness-{generation:016x}");
    let assignment = validate_reviewer_assignment(
        &session,
        controller.state.sessions.get(&session),
        &request.profile,
    );
    let result = async {
        check_cancelled(&cancelled)?;
        let repositories = match cancellable(
            &cancelled,
            call(
                &handle,
                &role,
                ReviewerAction::CaptureDelta {
                    baselines: Default::default(),
                },
            ),
        )
        .await?
        {
            ReviewerOutcome::Delta { repositories } => repositories,
            _ => {
                return Err(
                    "Worker returned an unexpected repository discovery response".to_owned(),
                );
            }
        };
        if repositories.is_empty() {
            return Err("No Git repositories are available to verify review tooling".to_owned());
        }
        check_cancelled(&cancelled)?;
        let roots = repositories
            .iter()
            .map(|repository| repository.root.clone())
            .collect::<Vec<_>>();
        let servers = review_mcp_servers(&roots, "core|slopcop");
        let flag = Arc::clone(&cancelled);
        let profile = request.profile.clone();
        let config = tokio::task::spawn_blocking(move || {
            let executor =
                CancellableProcessExecutor::new(flag).with_deadline(Duration::from_secs(90));
            controller.stage_reviewer_profile_controlled(
                &session, &profile, generation, &servers, &executor,
            )
        })
        .await
        .map_err(|error| format!("Reviewer staging task failed: {error}"))?
        .map_err(|error| format!("Stage reviewer: {error:#}"))?;
        let capabilities_result = probe_capabilities(
            &handle,
            &role,
            config,
            &request,
            &cancelled,
            &mut models,
            &mut efforts,
        )
        .await;
        let progress_result = match progress.as_ref() {
            Some(progress)
                if (capabilities_result.is_ok() || !models.is_empty() || !efforts.is_empty())
                    && !cancelled.load(Ordering::Acquire) =>
            {
                progress
                    .send(ReviewCapabilityChoices {
                        model_choices: models.clone(),
                        effort_choices: efforts.clone(),
                    })
                    .map_err(|_| "Review capability progress receiver closed".to_owned())
            }
            _ => Ok(()),
        };
        match (capabilities_result, progress_result) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) => return Err(error),
            (Ok(()), Err(error)) => return Err(error),
            (Err(error), Err(progress_error)) => {
                return Err(format!("{error}; {progress_error}"));
            }
        }
        assignment?;
        check_cancelled(&cancelled)?;
        cancellable(&cancelled, verify_tools(&handle, &role, repositories)).await?;
        check_cancelled(&cancelled)
    }
    .await;
    // Await cleanup even after cancellation or a rejected selector. A failed
    // Start may have launched a process before reporting configuration failure.
    let cleanup = call(&handle, &role, ReviewerAction::Pause).await;
    let result = match (result, cleanup) {
        (result, Ok(ReviewerOutcome::Paused)) => result,
        (Ok(()), Ok(_)) => Err("Unexpected response while stopping readiness probe".to_owned()),
        (Ok(()), Err(error)) => Err(format!("Could not stop readiness probe: {error}")),
        (Err(error), Ok(_)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}; could not stop readiness probe: {cleanup}"
        )),
    };
    let ready = result.is_ok();
    let message = result.err().unwrap_or_else(|| {
        "Reviewer starts, selected options apply, and Bifrost analysis works".to_owned()
    });
    (
        ReviewTargetReadiness {
            target,
            ready,
            message,
        },
        models,
        efforts,
    )
}

async fn probe_capabilities(
    handle: &ManagedSessionHandle,
    role: &str,
    mut config: ReviewerLaunchConfig,
    request: &ReviewProbeRequest,
    cancelled: &AtomicBool,
    models: &mut Vec<SessionConfigChoice>,
    efforts: &mut Vec<SessionConfigChoice>,
) -> Result<(), String> {
    config.model = None;
    config.effort = None;
    check_cancelled(cancelled)?;
    let started = cancellable(cancelled, start(handle, role, &config)).await?;
    *models = session_config_choices(&started.config_options, "model");
    *efforts = session_config_choices(&started.config_options, "effort");
    if let Some(model) = &request.model {
        validate_choice("model", model, models)?;
        check_cancelled(cancelled)?;
        config.model = Some(model.clone());
        let started = cancellable(cancelled, start(handle, role, &config)).await?;
        *efforts = session_config_choices(&started.config_options, "effort");
    }
    if let Some(effort) = &request.effort {
        validate_choice("effort", effort, efforts)?;
        check_cancelled(cancelled)?;
        config.effort = Some(effort.clone());
        cancellable(cancelled, start(handle, role, &config)).await?;
    }
    check_cancelled(cancelled)
}

fn validate_choice(key: &str, value: &str, choices: &[SessionConfigChoice]) -> Result<(), String> {
    if choices.iter().any(|choice| choice.value == value) {
        Ok(())
    } else if choices.is_empty() {
        Err(format!(
            "This adapter does not advertise configurable {key}; choose Profile default or update the adapter"
        ))
    } else {
        Err(format!(
            "This adapter does not advertise {key} {value:?}; choose an advertised value"
        ))
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

async fn verify_tools(
    handle: &ManagedSessionHandle,
    role: &str,
    repositories: Vec<RepoDelta>,
) -> Result<(), String> {
    let repositories = repositories
        .into_iter()
        .map(|repository| AnalyzeDeltaRepository {
            root: repository.root,
            baseline_tree: Some(repository.current_tree.clone()),
            current_tree: repository.current_tree,
        })
        .collect();
    match call(handle, role, ReviewerAction::AnalyzeDelta { repositories }).await? {
        ReviewerOutcome::ChangedFunctions { .. } => Ok(()),
        _ => Err("Worker returned an unexpected Bifrost analysis response".to_owned()),
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
        Err("Review readiness check cancelled".to_owned())
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
        } => Err("Review readiness check cancelled".to_owned()),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigSelectOption, SessionConfigSelectOptions,
    };
    use hel::hel_config::{
        ExecutionPolicy, HarnessKind, HarnessProfile, HelConfig, TargetTemplate,
    };
    use hel::hel_state::{HelState, SessionRecord, SessionState, TargetLocator};
    use hel::hel_targets::CommandSpec;
    use hel::hel_worker::{RelayExecutionState, RelayOperationalState, RepoDelta};
    use hel::hel_worker_launch::ReviewerLaunchConfig;
    use tokio::sync::watch;

    use super::*;
    use crate::hel_session_manager::{
        ManagedSessionView, RelaySessionTarget, RemoteSessionPublisher, RemoteSessionRequest,
        RemoteSessionRequests, ReviewerAction, ReviewerOutcome, SessionManagerControl,
        SessionManagerShutdown, spawn_remote_session_manager,
    };
    use crate::hel_worker_client::StartedReviewer;

    const SESSION: &str = "review-settings-test";

    /// The production remote manager is retained while this hand fake answers
    /// each reviewer action. This keeps the test on the same control path as a
    /// controller daemon without starting a worker or an adapter process.
    struct FakeManager {
        control: SessionManagerControl,
        requests: RemoteSessionRequests,
        publisher: RemoteSessionPublisher,
        _shutdown: SessionManagerShutdown,
        _targets: watch::Sender<Vec<RelaySessionTarget>>,
    }

    impl FakeManager {
        async fn new() -> (Self, crate::hel_session_manager::ManagedSessionHandle) {
            let channels = spawn_remote_session_manager().expect("remote manager");
            channels.targets.send_replace(vec![RelaySessionTarget {
                session_id: SESSION.to_owned(),
                spec: CommandSpec::new("true", Vec::<String>::new()),
                worker_recovery: None,
                project_memory: None,
            }]);
            let manager = Self {
                control: channels.control,
                requests: channels.requests,
                publisher: channels.publisher,
                _shutdown: channels.shutdown,
                _targets: channels.targets,
            };
            manager
                .publisher
                .publish(
                    SESSION.to_owned(),
                    ManagedSessionView {
                        connected: true,
                        ..ManagedSessionView::default()
                    },
                )
                .await
                .expect("publish the managed session view");
            let handle = manager
                .control
                .wait_for_session(SESSION, Duration::from_secs(5))
                .await
                .expect("the fake manager manages the session");
            (manager, handle)
        }

        async fn next(&mut self) -> RemoteSessionRequest {
            tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
                .await
                .expect("the settings helper makes a request")
                .expect("the remote manager remains alive")
        }
    }

    fn launch_config() -> ReviewerLaunchConfig {
        ReviewerLaunchConfig {
            profile_id: "reviewer".to_owned(),
            harness: HarnessKind::Claude,
            bridge_command: "/bin/false".into(),
            bridge_args: Vec::new(),
            environment: BTreeMap::new(),
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            model: Some("profile-default-model".to_owned()),
            effort: Some("profile-default-effort".to_owned()),
            generation: 1,
            mcp_servers: Vec::new(),
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
        let mut options = Vec::new();
        if !models.is_empty() {
            options.push(option("model", models));
        }
        if !efforts.is_empty() {
            options.push(option("effort", efforts));
        }
        options
    }

    fn started(options: Vec<SessionConfigOption>) -> Result<ReviewerOutcome, String> {
        Ok(ReviewerOutcome::Started(Box::new(StartedReviewer {
            native_session_id: Some("native-settings-test".to_owned()),
            config_options: options,
            reused: false,
            state: operational(),
        })))
    }

    fn operational() -> RelayOperationalState {
        RelayOperationalState {
            session_id: "native-settings-test".to_owned(),
            execution: RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            acknowledged_through: 0,
            acknowledged_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            native_session_id: Some("native-settings-test".to_owned()),
            agent_capabilities: None,
            agent_info: None,
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
            panic!("settings discovery must use reviewer start, not a prompt or config command");
        };
        (config, reply)
    }

    #[tokio::test]
    async fn cancellation_drops_an_inflight_start_without_waiting_for_the_adapter() {
        let (mut manager, handle) = FakeManager::new().await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let task = tokio::spawn(async move {
            cancellable(&flag, start(&handle, "cancel-live", &launch_config())).await
        });
        let (_, mut reply) = start_request(manager.next().await);
        cancelled.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), reply.closed())
            .await
            .expect("cancellation reaches the outstanding actor request promptly");
        assert!(task.await.unwrap().unwrap_err().contains("cancelled"));
    }

    #[tokio::test]
    async fn an_invalid_model_keeps_advertised_choices_without_a_second_start() {
        let (mut manager, handle) = FakeManager::new().await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = ReviewProbeRequest {
            profile: "reviewer".to_owned(),
            model: Some("missing".to_owned()),
            effort: None,
        };
        let task = tokio::spawn({
            let cancelled = Arc::clone(&cancelled);
            async move {
                let mut models = Vec::new();
                let mut efforts = Vec::new();
                let result = probe_capabilities(
                    &handle,
                    "invalid-model",
                    launch_config(),
                    &request,
                    &cancelled,
                    &mut models,
                    &mut efforts,
                )
                .await;
                (result, models, efforts)
            }
        });

        let (config, reply) = start_request(manager.next().await);
        assert_eq!(
            config.model, None,
            "profile defaults are not applied implicitly"
        );
        assert_eq!(
            config.effort, None,
            "profile defaults are not applied implicitly"
        );
        reply
            .send(started(advertised(&["fast", "deep"], &["low", "high"])))
            .expect("the probe is still waiting for startup");

        let (result, models, efforts) = task.await.expect("probe task");
        let error = result.expect_err("an unadvertised model must be rejected");
        assert!(error.contains("does not advertise model"), "{error}");
        assert_eq!(
            models
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["fast", "deep"]
        );
        assert_eq!(
            efforts
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
                .await
                .is_err(),
            "invalid choices must not trigger another start"
        );
    }

    #[tokio::test]
    async fn selecting_a_model_refreshes_efforts_before_applying_it() {
        let (mut manager, handle) = FakeManager::new().await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = ReviewProbeRequest {
            profile: "reviewer".to_owned(),
            model: Some("deep".to_owned()),
            effort: Some("high".to_owned()),
        };
        let task = tokio::spawn({
            let cancelled = Arc::clone(&cancelled);
            async move {
                let mut models = Vec::new();
                let mut efforts = Vec::new();
                let result = probe_capabilities(
                    &handle,
                    "refresh-effort",
                    launch_config(),
                    &request,
                    &cancelled,
                    &mut models,
                    &mut efforts,
                )
                .await;
                (result, models, efforts)
            }
        });

        let (first, reply) = start_request(manager.next().await);
        assert_eq!((first.model, first.effort), (None, None));
        reply
            .send(started(advertised(&["fast", "deep"], &["low"])))
            .expect("the probe is still waiting for startup");

        let (second, reply) = start_request(manager.next().await);
        assert_eq!(second.model.as_deref(), Some("deep"));
        assert_eq!(
            second.effort, None,
            "model selection refreshes before effort selection"
        );
        reply
            .send(started(advertised(&["fast", "deep"], &["low", "high"])))
            .expect("the probe is still waiting for the refreshed options");

        let (third, reply) = start_request(manager.next().await);
        assert_eq!(third.model.as_deref(), Some("deep"));
        assert_eq!(third.effort.as_deref(), Some("high"));
        reply
            .send(started(advertised(&["fast", "deep"], &["low", "high"])))
            .expect("the probe is still waiting for the selected startup");

        let (result, models, efforts) = task.await.expect("probe task");
        result.expect("advertised model and refreshed effort are accepted");
        assert_eq!(
            models
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["fast", "deep"]
        );
        assert_eq!(
            efforts
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
                .await
                .is_err(),
            "selector application must not send a separate config command"
        );
    }

    #[tokio::test]
    async fn profile_defaults_do_not_force_a_selector_start() {
        let (mut manager, handle) = FakeManager::new().await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = ReviewProbeRequest {
            profile: "reviewer".to_owned(),
            model: None,
            effort: None,
        };
        let task = tokio::spawn({
            let cancelled = Arc::clone(&cancelled);
            async move {
                let mut models = Vec::new();
                let mut efforts = Vec::new();
                let result = probe_capabilities(
                    &handle,
                    "profile-defaults",
                    launch_config(),
                    &request,
                    &cancelled,
                    &mut models,
                    &mut efforts,
                )
                .await;
                (result, models, efforts)
            }
        });

        let (config, reply) = start_request(manager.next().await);
        assert_eq!(config.model, None);
        assert_eq!(config.effort, None);
        reply
            .send(started(advertised(&["fast"], &["low"])))
            .expect("the probe is still waiting for startup");
        let (result, _, _) = task.await.expect("probe task");
        result.expect("profile defaults are a valid no-selector probe");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
                .await
                .is_err(),
            "profile defaults must not trigger an extra start"
        );
    }

    #[tokio::test]
    async fn cancellation_after_discovery_prevents_a_new_selector_start() {
        let (mut manager, handle) = FakeManager::new().await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = ReviewProbeRequest {
            profile: "reviewer".to_owned(),
            model: Some("deep".to_owned()),
            effort: None,
        };
        let task = tokio::spawn({
            let cancelled = Arc::clone(&cancelled);
            async move {
                let mut models = Vec::new();
                let mut efforts = Vec::new();
                let result = probe_capabilities(
                    &handle,
                    "cancel-before-selection",
                    launch_config(),
                    &request,
                    &cancelled,
                    &mut models,
                    &mut efforts,
                )
                .await;
                (result, models, efforts)
            }
        });

        let (_, reply) = start_request(manager.next().await);
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        reply
            .send(started(advertised(&["deep"], &["low"])))
            .expect("the probe is still waiting for startup");
        let (result, _, _) = task.await.expect("probe task");
        assert_eq!(
            result.expect_err("cancellation must stop selector application"),
            "Review readiness check cancelled"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
                .await
                .is_err(),
            "cancellation must prevent a second reviewer start"
        );
    }

    #[tokio::test]
    async fn verify_tools_uses_the_current_tree_for_both_analysis_endpoints() {
        let (mut manager, handle) = FakeManager::new().await;
        let task = tokio::spawn(async move {
            verify_tools(
                &handle,
                "verify-current-tree",
                vec![RepoDelta {
                    root: "/workspace/app".into(),
                    baseline_tree: Some("old-tree".to_owned()),
                    current_tree: "current-tree".to_owned(),
                    patch: "diff --git a/a b/a\n@@\n+change\n".to_owned(),
                    diffstat: "1 file changed, 1 insertion(+)".to_owned(),
                    changed_lines: 1,
                }],
            )
            .await
        });

        let RemoteSessionRequest::Reviewer {
            action: ReviewerAction::AnalyzeDelta { repositories },
            reply,
            ..
        } = manager.next().await
        else {
            panic!("tool verification must use analysis, not a prompt or baseline update");
        };
        assert_eq!(repositories.len(), 1);
        assert_eq!(
            repositories[0].baseline_tree.as_deref(),
            Some("current-tree")
        );
        assert_eq!(repositories[0].current_tree, "current-tree");
        reply
            .send(Ok(ReviewerOutcome::ChangedFunctions {
                packet: "verified".to_owned(),
            }))
            .expect("the verification task is still waiting");
        task.await
            .expect("verification task")
            .expect("the analysis response is accepted");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
                .await
                .is_err(),
            "tool verification must not advance the review baseline"
        );
    }

    #[tokio::test]
    async fn capability_progress_arrives_before_tool_verification() {
        let (mut manager, handle) = FakeManager::new().await;
        let profile_home = tempfile::tempdir().expect("profile home");
        fs::write(profile_home.path().join("settings.json"), b"{}").expect("profile settings");
        let worker_root_parent = tempfile::tempdir().expect("worker root");
        let worker_root = worker_root_parent.path().join(SESSION);
        fs::create_dir_all(&worker_root).expect("session worker root");
        let repository_root = tempfile::tempdir().expect("repository root");
        let mut config = HelConfig::default();
        config.profiles.insert(
            "reviewer".to_owned(),
            HarnessProfile {
                kind: HarnessKind::Claude,
                home: profile_home.path().to_owned(),
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        config
            .targets
            .insert("local".to_owned(), TargetTemplate::LocalBare);
        let mut state = HelState::default();
        state.sessions.insert(
            SESSION.to_owned(),
            SessionRecord {
                id: SESSION.to_owned(),
                workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                title: "review settings test".to_owned(),
                harness_kind: HarnessKind::Codex,
                last_profile: "primary".to_owned(),
                bundle_id: "project".to_owned(),
                project_directory: Some(repository_root.path().to_owned()),
                managed_worktree: None,
                target_template_id: "local".to_owned(),
                resource_allocation: None,
                additional_mounts: Vec::new(),
                container_cpus: None,
                container_memory: None,
                state: SessionState::Running,
                archived: false,
                target: Some(TargetLocator::LocalBare {
                    worker_root: worker_root.clone(),
                }),
                native_session_id: Some("native-settings-test".to_owned()),
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
        );
        let controller = Arc::new(Controller { config, state });
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = ReviewProbeRequest {
            profile: "reviewer".to_owned(),
            model: None,
            effort: None,
        };
        let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(probe_target(
            controller,
            SESSION.to_owned(),
            "review settings target".to_owned(),
            handle,
            request,
            cancelled,
            Some(progress),
        ));

        let RemoteSessionRequest::Reviewer {
            action: ReviewerAction::CaptureDelta { .. },
            reply,
            ..
        } = manager.next().await
        else {
            panic!("settings discovery must capture repositories first");
        };
        reply
            .send(Ok(ReviewerOutcome::Delta {
                repositories: vec![RepoDelta {
                    root: PathBuf::from(repository_root.path()),
                    baseline_tree: None,
                    current_tree: "current-tree".to_owned(),
                    patch: String::new(),
                    diffstat: String::new(),
                    changed_lines: 0,
                }],
            }))
            .expect("the probe is still waiting for repository discovery");

        let request = manager.next().await;
        let (config, reply) = match request {
            RemoteSessionRequest::Reviewer {
                action: ReviewerAction::Start { config },
                reply,
                ..
            } => (config, reply),
            RemoteSessionRequest::Reviewer {
                action: ReviewerAction::Pause,
                reply,
                ..
            } => {
                reply
                    .send(Ok(ReviewerOutcome::Paused))
                    .expect("the probe is still waiting for cleanup");
                let (target, _, _) = task.await.expect("probe task");
                panic!("profile staging failed before startup: {}", target.message);
            }
            _ => panic!("unexpected request before reviewer start"),
        };
        assert_eq!((config.model, config.effort), (None, None));
        reply
            .send(started(advertised(&["fast", "deep"], &["low", "high"])))
            .expect("the probe is still waiting for capability discovery");

        let choices = tokio::time::timeout(Duration::from_secs(5), progress_rx.recv())
            .await
            .expect("capability progress is emitted before slow verification")
            .expect("the progress sender remains alive");
        assert_eq!(
            choices
                .model_choices
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["fast", "deep"]
        );
        assert_eq!(
            choices
                .effort_choices
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"]
        );

        let RemoteSessionRequest::Reviewer {
            action: ReviewerAction::AnalyzeDelta { .. },
            reply,
            ..
        } = manager.next().await
        else {
            panic!("tool verification must follow capability progress");
        };
        reply
            .send(Ok(ReviewerOutcome::ChangedFunctions {
                packet: "verified".to_owned(),
            }))
            .expect("the probe is still waiting for tool verification");
        let RemoteSessionRequest::Reviewer {
            action: ReviewerAction::Pause,
            reply,
            ..
        } = manager.next().await
        else {
            panic!("the probe must clean up after tool verification");
        };
        reply
            .send(Ok(ReviewerOutcome::Paused))
            .expect("the probe is still waiting for cleanup");
        let (target, _, _) = task.await.expect("probe task");
        assert!(target.ready, "the full readiness check should still pass");
    }
}
