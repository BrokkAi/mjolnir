use super::*;

use crate::server_runtime::profile_catalog::{ProfileCatalog, counting_probe, test_config};
use mj_core::config::HarnessKind;
use mj_core::state::SessionRecord;

fn quota(profile_id: &str, harness: HarnessKind, remaining: &[u8]) -> ProfileQuota {
    ProfileQuota {
        profile_id: profile_id.into(),
        harness,
        windows: remaining
            .iter()
            .map(|remaining_percent| crate::quota::QuotaWindow {
                label: "window".into(),
                remaining_percent: Some(*remaining_percent),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            })
            .collect(),
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
    }
}

/// A pay-per-use profile's report: no windows, the API label instead.
fn usage_priced(profile_id: &str) -> ProfileQuota {
    ProfileQuota {
        profile_id: profile_id.into(),
        harness: HarnessKind::Codex,
        windows: Vec::new(),
        extra: Some(crate::quota::API_LABEL.to_owned()),
        error: None,
        refreshed_at_epoch_seconds: 0,
    }
}

/// A probe that answers each profile with the models given for it and the
/// efforts `low` and `high`, and fails for a profile it has no models for.
fn models_probe(offers: &[(&str, &[&str])]) -> Arc<crate::server_runtime::profile_catalog::Probe> {
    let offers = offers
        .iter()
        .map(|(profile, models)| {
            (
                (*profile).to_owned(),
                models.iter().map(|model| (*model).to_owned()).collect(),
            )
        })
        .collect::<BTreeMap<String, Vec<String>>>();
    let choice = |value: &str| mj_core::acp::SessionConfigChoice {
        value: value.to_owned(),
        name: value.to_owned(),
        description: None,
    };
    Arc::new(move |profile: String| {
        let models = offers.get(&profile).cloned();
        Box::pin(async move {
            let models = models.with_context(|| format!("{profile} cannot be discovered"))?;
            Ok(mj_core::worker_launch::ProfileConfig {
                model: models.first().cloned(),
                models: models.iter().map(|model| choice(model)).collect(),
                efforts: vec![choice("low"), choice("high")],
                observed_at: 1,
            })
        })
    })
}

/// The parent's durable record, and the registration a spawn asks for, which
/// it keeps and then refuses so a test can read what the spawn chose.
struct RecordingExports {
    parent: SessionRecord,
    registered: std::sync::Mutex<Option<crate::controller::RegisterSubagentRequest>>,
}

impl ExportRuntime for RecordingExports {
    fn session_record(&self, session_id: &str) -> Option<SessionRecord> {
        (session_id == self.parent.id).then(|| self.parent.clone())
    }
    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
        Box::pin(async move { bail!("session {session_id} cannot be checkpointed in a test") })
    }
    fn spawn_subagent(
        self: Arc<Self>,
        request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async move {
            *self.registered.lock().unwrap() = Some(request);
            bail!("registration recorded")
        })
    }
}

impl RecordingExports {
    fn registered(&self) -> crate::controller::RegisterSubagentRequest {
        self.registered
            .lock()
            .unwrap()
            .clone()
            .expect("the spawn reached registration")
    }
}

/// The situation from the report that prompted profile selection: the parent
/// runs on `parent`, nearly out of its 5-hour window; `codex-high` offers the
/// same models with plenty left; `deepseek` is pay-per-use, so it ranks as
/// full, but offers a different model.
async fn selection_backend(
    offers: &[(&str, &[&str])],
    parent_view: Option<ManagedSessionView>,
) -> (Arc<ApiBackend>, Arc<RecordingExports>) {
    selection_backend_refusing(offers, parent_view, Default::default()).await
}

/// [`selection_backend`], with logins the credential sync found refused.
async fn selection_backend_refusing(
    offers: &[(&str, &[&str])],
    parent_view: Option<ManagedSessionView>,
    rejected_logins: mj_core::credentials::RejectedLogins,
) -> (Arc<ApiBackend>, Arc<RecordingExports>) {
    let catalog = ProfileCatalog::with_probe(models_probe(offers));
    catalog
        .sync_now(&test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("codex-high", HarnessKind::Codex),
                ("deepseek", HarnessKind::Codex),
            ],
            &["codex-high", "deepseek"],
        ))
        .await;
    let quotas = BTreeMap::from([
        (
            "parent".to_owned(),
            quota("parent", HarnessKind::Codex, &[80, 3]),
        ),
        (
            "codex-high".to_owned(),
            quota("codex-high", HarnessKind::Codex, &[60, 55]),
        ),
        ("deepseek".to_owned(), usage_priced("deepseek")),
    ]);
    let exports = Arc::new(RecordingExports {
        parent: parent_record("parent-1", "parent"),
        registered: std::sync::Mutex::new(None),
    });
    let backend = Arc::new(
        ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "parent-1".into(),
                accepted_ordinal: 1,
                submitted: mpsc::unbounded_channel().0,
                view: parent_view,
            })),
            running_states(),
            exports.clone(),
        )
        .with_profile_catalog(catalog)
        .with_quota_reports(Arc::new(std::sync::Mutex::new(quotas)))
        .with_rejected_logins(Arc::new(std::sync::Mutex::new(rejected_logins))),
    );
    (backend, exports)
}

const SAME_MODELS: &[(&str, &[&str])] = &[
    ("parent", &["luna", "nova"]),
    ("codex-high", &["luna", "nova"]),
    ("deepseek", &["flash"]),
];

/// A parent whose live harness runs `model` at `effort`.
fn parent_running(model: &str, effort: &str) -> ManagedSessionView {
    let mut view = ready_view(model);
    let config = &mut view
        .snapshot
        .as_mut()
        .expect("a ready view has a snapshot")
        .operational
        .config;
    config.insert("model".into(), model.into());
    config.insert("effort".into(), effort.into());
    view
}

fn spawn_with(
    profile_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> mj_core::subagent::SubagentToolRequest {
    mj_core::subagent::SubagentToolRequest {
        request_id: "request-1".into(),
        created_at_ms: 0,
        action: mj_core::subagent::SubagentToolAction::Spawn {
            task_name: "audit deps".into(),
            instructions: "check the lockfile".into(),
            profile_id: profile_id.map(str::to_owned),
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
            working_directory: Default::default(),
            context: None,
            files: Vec::new(),
        },
    }
}

#[tokio::test]
async fn spawn_runs_the_child_on_the_profile_with_the_most_quota_that_offers_the_model() {
    let (backend, exports) = selection_backend(SAME_MODELS, None).await;

    let result = backend
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(None, Some("luna"), Some("low")),
        )
        .await;

    assert!(
        result.message.contains("registration recorded"),
        "{}",
        result.message
    );
    let registered = exports.registered();
    assert_eq!(
        registered.profile_id, "codex-high",
        "the parent's own profile has 3% left and the pay-per-use one lacks the model"
    );
    assert_eq!(registered.model.as_deref(), Some("luna"));
    assert_eq!(registered.effort.as_deref(), Some("low"));
}

#[tokio::test]
async fn a_pinned_profile_is_honored_even_with_less_quota() {
    let (backend, exports) = selection_backend(SAME_MODELS, None).await;

    backend
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(Some("parent"), Some("luna"), Some("low")),
        )
        .await;

    assert_eq!(exports.registered().profile_id, "parent");
}

#[tokio::test]
async fn a_spawn_without_a_model_is_refused() {
    let (backend, exports) = selection_backend(SAME_MODELS, None).await;

    let result = backend
        .execute_subagent_tool("parent-1".into(), spawn_with(None, None, Some("low")))
        .await;

    assert!(result.is_error);
    assert!(
        result.message.contains("spawn needs a model"),
        "{}",
        result.message
    );
    assert!(exports.registered.lock().unwrap().is_none());
}

#[tokio::test]
async fn current_names_the_parents_live_model_and_its_effort_follows_when_offered() {
    let (backend, exports) =
        selection_backend(SAME_MODELS, Some(parent_running("nova", "high"))).await;

    backend
        .execute_subagent_tool("parent-1".into(), spawn_with(None, Some("current"), None))
        .await;

    let registered = exports.registered();
    assert_eq!(registered.profile_id, "codex-high");
    assert_eq!(registered.model.as_deref(), Some("nova"));
    assert_eq!(registered.effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn an_inherited_effort_the_chosen_profile_lacks_is_left_to_the_harness() {
    let (backend, exports) =
        selection_backend(SAME_MODELS, Some(parent_running("nova", "xhigh"))).await;

    backend
        .execute_subagent_tool("parent-1".into(), spawn_with(None, Some("current"), None))
        .await;

    assert_eq!(exports.registered().effort, None);
}

#[tokio::test]
async fn current_without_a_live_parent_model_is_refused() {
    let (backend, exports) = selection_backend(SAME_MODELS, None).await;

    let result = backend
        .execute_subagent_tool("parent-1".into(), spawn_with(None, Some("current"), None))
        .await;

    assert!(result.is_error);
    assert!(
        result.message.contains("current model is unknown"),
        "{}",
        result.message
    );
    assert!(exports.registered.lock().unwrap().is_none());
}

/// Profiles offering the same models are one choice for the parent, shown as
/// the one with the most quota left; a profile offering other models is never
/// hidden behind a same-harness profile with more quota.
#[tokio::test]
async fn list_profiles_merges_profiles_offering_the_same_models() {
    let (backend, _) = selection_backend(SAME_MODELS, None).await;

    let result = backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                request_id: "request-1".into(),
                created_at_ms: 0,
                action: mj_core::subagent::SubagentToolAction::ListProfiles,
            },
        )
        .await;

    assert!(!result.is_error, "{}", result.message);
    let answer: serde_json::Value = serde_json::from_str(&result.message).unwrap();
    let listed = answer["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|profile| profile["profile_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(listed, vec!["deepseek", "codex-high"]);
    assert!(answer.get("unavailable").is_none(), "{answer}");
}

/// One login that cannot be discovered drops out on its own; it neither fails
/// the answer nor stops a spawn from using the others.
#[tokio::test]
async fn a_profile_that_cannot_be_discovered_is_left_out_not_fatal() {
    let offers: &[(&str, &[&str])] = &[("parent", &["luna"]), ("deepseek", &["flash"])];
    let (backend, exports) = selection_backend(offers, None).await;

    let listed = backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                request_id: "request-1".into(),
                created_at_ms: 0,
                action: mj_core::subagent::SubagentToolAction::ListProfiles,
            },
        )
        .await;
    assert!(!listed.is_error, "{}", listed.message);
    let answer: serde_json::Value = serde_json::from_str(&listed.message).unwrap();
    assert_eq!(answer["unavailable"][0]["profile_id"], "codex-high");
    assert_eq!(answer["profiles"].as_array().unwrap().len(), 2);

    backend
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(None, Some("luna"), Some("low")),
        )
        .await;
    assert_eq!(exports.registered().profile_id, "parent");
}

#[test]
fn failed_subagent_followup_is_terminal_error_with_its_cause() {
    let status = StartStatus::Failed {
        message: "model is unavailable".into(),
    };
    assert_eq!(
        subagent_status(
            None,
            None,
            Some(&status),
            None,
            false,
            &ChildProgress::settled(ReportState::Fallback)
        ),
        ("error".into(), Some("model is unavailable".into()), true)
    );
}

/// A close is accepted long before the child is gone: it cancels whatever owned
/// the session, checkpoints, seals the relay and tears the process tree down,
/// and the record only says `Closing` once that is under way. A child that had
/// finished its turn therefore reported `completed` the moment its close was
/// admitted, so a parent that closed a child and spawned its replacement stacked
/// both process trees inside one container (#1087). A wait must follow the close
/// instead, the way a session-level wait does.
#[test]
fn a_child_whose_close_is_running_is_not_finished_until_the_close_is() {
    let mut record = crate::controller::test_support::checkpoint_test_session("child");
    record.state = SessionState::Running;
    let summary = mj_core::state::MaterializedSessionSummary {
        session_id: "child".into(),
        applied_event_ordinal: 4,
        last_activity_at_ms: None,
        execution: MaterializedExecutionState::Idle,
        session_title: None,
        last_agent_message: Some("the child's report".into()),
        last_user_message: None,
        last_agent_message_follows_last_user: true,
        agent_message_latest_content_ordinals: Vec::new(),
        interruption_event_ordinals: Vec::new(),
    };

    assert_eq!(
        subagent_status(
            Some(&record),
            Some(&summary),
            None,
            None,
            false,
            &ChildProgress::settled(ReportState::Fallback)
        ),
        ("completed".into(), Some("the child's report".into()), true),
        "an idle child nobody is closing is finished"
    );

    let (state, output, finished) = subagent_status(
        Some(&record),
        Some(&summary),
        None,
        None,
        true,
        &ChildProgress::settled(ReportState::Fallback),
    );
    assert_eq!(state, "stopping");
    assert_eq!(output, None);
    assert!(
        !finished,
        "a wait must keep following a child whose close is still running"
    );

    // A child whose start failed is terminal, but it is not gone either while
    // its close runs; its cause was already reported to the parent before it
    // asked for the close.
    let failed = StartStatus::Failed {
        message: "model is unavailable".into(),
    };
    assert!(
        !subagent_status(
            Some(&record),
            Some(&summary),
            Some(&failed),
            None,
            true,
            &ChildProgress::settled(ReportState::Fallback)
        )
        .2,
        "a failed child is still being torn down while its close runs"
    );

    // The close finished: the record settled, and the flag the daemon clears
    // just after it is no longer allowed to hold the wait open.
    record.state = SessionState::Stopped;
    assert_eq!(
        subagent_status(
            Some(&record),
            Some(&summary),
            None,
            None,
            true,
            &ChildProgress::settled(ReportState::Fallback)
        ),
        ("stopped".into(), None, true),
        "a record that already settled to stopped ends the wait"
    );
}

/// When a child's startup failed, the record holds the reason and the
/// follow-up holds only the symptom: that the session would not take a first
/// prompt. The parent model must read the reason, which is what #1065 could
/// not do.
#[test]
fn a_failed_child_reports_the_startup_cause_rather_than_the_symptom() {
    let mut record = crate::controller::test_support::checkpoint_test_session("child");
    record.state = SessionState::Error;
    record.last_error = Some(
        "sub-agent startup failed: the worker process is gone; it reached the startup step \
         \"review-baseline\""
            .into(),
    );
    let status = StartStatus::Failed {
        message: "session child is Error and will not take a first prompt".into(),
    };

    let (state, output, terminal) = subagent_status(
        Some(&record),
        None,
        Some(&status),
        None,
        false,
        &ChildProgress::settled(ReportState::Fallback),
    );

    assert_eq!(state, "error");
    assert!(terminal);
    assert_eq!(output.as_deref(), record.last_error.as_deref());
}
use mj_client::session::{
    ManagedSessionView, PendingRelaySubmit, PendingRelaySync, SessionControlBackend,
    SessionHandleBackend,
};
use tokio::sync::mpsc;

/// A session actor that records what it was asked to submit and accepts it
/// at a fixed ordinal. Hand-written rather than mocked so the test proves
/// the exact relay command the API sends.
#[derive(Clone)]
struct FakeSession {
    session_id: String,
    accepted_ordinal: u64,
    submitted: mpsc::UnboundedSender<(String, RelayCommand)>,
    /// What `view()` reports. `None` is the default empty view, which is
    /// what a session that has not connected yet looks like.
    view: Option<ManagedSessionView>,
}

impl SessionHandleBackend for FakeSession {
    fn search_prompts(
        &self,
        _bundle_id: String,
        _scope: mj_core::storage::HistoryScope,
        _query: String,
    ) -> mj_client::session::BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn review_state(
        &self,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::ReviewState>> {
        Box::pin(async { Ok(Default::default()) })
    }

    fn config_result(&self, _command_id: String) -> BoxFuture<'_, Result<Option<Option<String>>>> {
        Box::pin(async { Ok(Some(None)) })
    }
    fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
        Box::new(self.clone())
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn view(&self) -> ManagedSessionView {
        self.view.clone().unwrap_or_default()
    }
    fn is_stopped(&self) -> bool {
        false
    }
    fn has_changed(&self) -> Result<bool> {
        Ok(false)
    }
    fn changed(&mut self) -> BoxFuture<'_, Result<ManagedSessionView>> {
        Box::pin(std::future::pending())
    }
    fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> BoxFuture<'_, Result<PendingRelaySubmit>> {
        let _ = self.submitted.send((command_id, command));
        let ordinal = self.accepted_ordinal;
        Box::pin(async move {
            Ok(PendingRelaySubmit::new(Box::pin(
                async move { Ok(ordinal) },
            )))
        })
    }
    fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
        Box::pin(async move { Ok(PendingRelaySync::new(Box::pin(async { Ok(()) }))) })
    }
    fn respond_elicitation(
        &self,
        _elicitation_id: String,
        _response: mj_core::elicitation::ElicitationResponse,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop_background_task(&self, _background_task_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn reviewer(
        &self,
        _role: Option<String>,
        _action: mj_client::session::ReviewerAction,
    ) -> BoxFuture<'_, Result<mj_client::session::ReviewerOutcome>> {
        Box::pin(async { anyhow::bail!("no reviewer in this fake") })
    }
}

/// Every session this daemon is asked about is up and running.
fn running_states() -> SessionStateSource {
    Arc::new(|_| Some(SessionState::Running))
}

#[test]
fn checkpoint_export_retains_typed_deferrals_and_real_failures() {
    let error = anyhow!("disk failed");
    assert!(matches!(
        checkpoint_export_error(error),
        ExportError::Failed(_)
    ));
    let error = anyhow::Error::new(crate::controller::CheckpointDeferred::harness_busy())
        .context("capture bundle");
    let ExportError::Refused(message) = checkpoint_export_error(error) else {
        panic!("expected a deferred export");
    };
    assert!(message.contains("capture bundle"));
    assert!(message.contains("agent is working"));
}

/// An export runtime with nothing in it. The follow-up tests never export;
/// the export path's own behavior is proved by the API handler tests.
struct NoExports;

impl ExportRuntime for NoExports {
    fn session_record(&self, _session_id: &str) -> Option<mj_core::state::SessionRecord> {
        None
    }
    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
        Box::pin(async move { bail!("session {session_id} cannot be checkpointed in a test") })
    }
}

/// A connected view whose harness is ready and offers one model.
fn ready_view(model: &str) -> ManagedSessionView {
    let materialized = mj_core::state::MaterializedSession::empty("session-1");
    let option: agent_client_protocol::schema::v1::SessionConfigOption =
        serde_json::from_value(serde_json::json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": model,
            "options": [{"value": model, "name": model}],
        }))
        .expect("the fixture describes a select the schema accepts");
    let operational = mj_core::relay::RelayOperationalState {
        continuation: Default::default(),
        relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
        native_agents: Vec::new(),
        steering: None,
        cancelling_prompt_id: None,
        clear_context: false,
        clear_context_started_at_ms: None,
        native_agent_count: 0,
        expected_continuation: None,
        inferred_idle_since_ms: None,
        goal: Default::default(),
        capacity_retry: None,
        retry_assessment_pending: false,
        activity_turn_started_at_ms: None,
        idle_since_ms: None,
        store_id: None,
        session_id: "session-1".into(),
        execution: mj_core::relay::RelayExecutionState::Idle,
        latest_ordinal: 0,
        latest_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        acknowledged_through: 0,
        acknowledged_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        recovery_floor_ordinal: 0,
        recovery_floor_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        native_session_id: Some("native-1".into()),
        native_continuity_lost: false,
        replaced_unused_native_session_id: None,
        checkpoint_only: false,
        acp_ready: Some(true),
        agent_capabilities: None,
        agent_info: None,
        steering_supported: None,
        config_options: vec![option],
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
        tools_in_flight: Vec::new(),
        activity: None,
        harness_turn: None,
        last_harness_turn_started_ordinal: None,
        background_commands: Vec::new(),
        background_work_known: None,
    };
    ManagedSessionView {
        snapshot: Some(mj_core::state::ManagedSessionSnapshot {
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational,
            latest_credential_sync_signal: None,
            worker_build: None,
        }),
        connected: true,
        error: None,
    }
}

/// The same ready view, but also offering the `fast-mode` selector Codex
/// exposes, so a follow-up can turn it on.
fn ready_view_offering_fast_mode(model: &str) -> ManagedSessionView {
    let mut view = ready_view(model);
    let option: agent_client_protocol::schema::v1::SessionConfigOption =
        serde_json::from_value(serde_json::json!({
            "id": "fast-mode",
            "name": "Fast mode",
            "type": "select",
            "currentValue": "off",
            "options": [{"value": "off", "name": "Off"}, {"value": "on", "name": "On"}],
        }))
        .expect("the fixture describes a select the schema accepts");
    view.snapshot
        .as_mut()
        .expect("ready_view has a snapshot")
        .operational
        .config_options
        .push(option);
    view
}

struct FakeControl(FakeSession);

impl SessionControlBackend for FakeControl {
    fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>> {
        let session = self.0.clone();
        Box::pin(async move {
            anyhow::ensure!(session_id == session.session_id, "unknown session");
            Ok(SessionHandle::new(session))
        })
    }
}

/// A running parent session, as the durable record the tool path reads it
/// from. Only the fields the tool path uses carry meaning.
struct ParentExports(SessionRecord);

impl ExportRuntime for ParentExports {
    fn session_record(&self, session_id: &str) -> Option<SessionRecord> {
        (session_id == self.0.id).then(|| self.0.clone())
    }
    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
        Box::pin(async move { bail!("session {session_id} cannot be checkpointed in a test") })
    }
}

fn parent_record(id: &str, profile: &str) -> SessionRecord {
    SessionRecord {
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        publication: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        container_workspace: None,
        build_cache: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: id.to_owned(),
        title: "parent".into(),
        harness_kind: HarnessKind::Codex,
        last_profile: profile.to_owned(),
        bundle_id: "hel".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Running,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-09T12:00:00Z".into(),
        updated_at: "2026-08-09T12:01:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

/// The behaviour the whole change exists for: once the background pass has
/// discovered the profiles, the tool call itself discovers nothing.
#[tokio::test]
async fn list_profiles_answers_from_the_warm_catalogue_without_probing_again() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
    let config = test_config(
        &[
            ("parent", HarnessKind::Codex),
            ("helper", HarnessKind::Claude),
        ],
        &["helper"],
    );
    catalog.sync_now(&config).await;
    let warmed = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(warmed, 2, "the pass discovers every enabled profile once");

    let backend = Arc::new(
        ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "parent-1".into(),
                accepted_ordinal: 1,
                submitted: mpsc::unbounded_channel().0,
                view: None,
            })),
            running_states(),
            Arc::new(ParentExports(parent_record("parent-1", "parent"))),
        )
        .with_profile_catalog(catalog),
    );

    let result = backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                request_id: "request-1".into(),
                created_at_ms: 0,
                action: mj_core::subagent::SubagentToolAction::ListProfiles,
            },
        )
        .await;

    assert!(!result.is_error, "the tool call failed: {}", result.message);
    let answer: serde_json::Value =
        serde_json::from_str(&result.message).expect("the answer is JSON");
    let profiles = answer["profiles"]
        .as_array()
        .expect("the answer names profiles")
        .clone();
    assert_eq!(
        profiles
            .iter()
            .map(|profile| (
                profile["profile_id"].as_str().unwrap().to_owned(),
                profile["harness"].as_str().unwrap().to_owned(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("parent".to_owned(), "codex".to_owned()),
            ("helper".to_owned(), "claude".to_owned()),
        ],
        "the parent's own profile and the eligible one are offered"
    );
    for profile in &profiles {
        let id = profile["profile_id"].as_str().unwrap();
        assert_eq!(
            profile["default_model"].as_str(),
            Some(format!("{id}-model").as_str()),
            "the answer quotes the discovered default model"
        );
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        warmed,
        "the call must not discover anything the background pass already did"
    );
}

/// A backend whose parent session is the `parent` profile, over the given
/// catalogue, so a spawn can be driven against a warm or a cold one.
fn spawn_backend(catalog: Arc<ProfileCatalog>) -> Arc<ApiBackend> {
    Arc::new(
        ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "parent-1".into(),
                accepted_ordinal: 1,
                submitted: mpsc::unbounded_channel().0,
                view: None,
            })),
            running_states(),
            Arc::new(ParentExports(parent_record("parent-1", "parent"))),
        )
        .with_profile_catalog(catalog),
    )
}

#[tokio::test]
async fn spawn_refuses_a_model_no_eligible_profile_offers() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
    catalog
        .sync_now(&test_config(&[("parent", HarnessKind::Codex)], &[]))
        .await;
    let warmed = calls.load(std::sync::atomic::Ordering::SeqCst);

    let result = spawn_backend(catalog)
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(None, Some("no-such-model"), None),
        )
        .await;

    assert!(result.is_error, "the spawn should have been refused");
    assert!(
        result
            .message
            .contains("no eligible profile offers model \"no-such-model\""),
        "{}",
        result.message
    );
    assert!(
        result.message.contains("parent (parent-model)"),
        "the refusal names what is offered: {}",
        result.message
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        warmed,
        "a warm catalogue answers without launching a discovery harness"
    );
}

/// A spawn that arrives before the background pass has finished waits for
/// that pass's discovery instead of choosing blind, and starts none of its
/// own: the one harness launch is shared.
#[tokio::test]
async fn spawn_over_a_cold_catalogue_waits_for_the_background_discovery() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
    catalog.sync(&test_config(&[("parent", HarnessKind::Codex)], &[]));

    let result = spawn_backend(catalog)
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(None, Some("no-such-model"), None),
        )
        .await;

    assert!(
        result.message.contains("no eligible profile offers"),
        "the spawn should have checked the model once discovered: {}",
        result.message
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the spawn shares the background pass's discovery"
    );
}

#[tokio::test]
async fn prompt_submits_one_text_block_and_returns_its_acceptance_ordinal() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 42,
            submitted: submitted_tx,
            view: None,
        })),
        running_states(),
        Arc::new(NoExports),
    );

    let turn_id = backend
        .prompt("session-1".into(), "add a README line".into())
        .await
        .unwrap();
    assert_eq!(turn_id, 42);

    let (command_id, command) = submitted.recv().await.unwrap();
    assert!(
        command_id.starts_with("api-"),
        "command id {command_id} should name the API as its source"
    );
    let RelayCommand::Prompt { prompt } = command else {
        panic!("the API must submit a prompt command");
    };
    assert_eq!(prompt.len(), 1);
    let ContentBlock::Text(text) = &prompt[0] else {
        panic!("the API must submit the prompt as one text block");
    };
    assert_eq!(text.text, "add a README line");
}

#[tokio::test]
async fn a_finished_subagent_is_recorded_as_a_notice_not_a_prompt() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "parent-1".into(),
            accepted_ordinal: 7,
            submitted: submitted_tx,
            view: None,
        })),
        running_states(),
        Arc::new(NoExports),
    );

    // Launch finding R11-3: the notice read `finished turn 46 (completed {
    // stop_reason: "endturn" })`, Rust's debug form of the outcome and the
    // turn's completion ordinal, where the child's own `mj wait` said turn
    // 16. It names the turn as `mj wait` does, and the outcome in words.
    let outcome = mj_core::state::MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: "prompt-1".into(),
        accepted_ordinal: Some(16),
        turn_start_position: Some(17),
        completed_ordinal: 46,
        completed_at_ms: 0,
        outcome: mj_core::state::TurnOutcomeKind::Completed {
            stop_reason: "EndTurn".into(),
        },
    };
    backend
        .record_subagent_completion_notice(
            "parent-1".into(),
            "child-abcdef012345",
            "Answer project codename",
            &outcome,
        )
        .await
        .unwrap();

    let (command_id, command) = submitted.recv().await.unwrap();
    assert!(
        command_id.starts_with("subagent-"),
        "notice command id {command_id} should name the sub-agent path"
    );
    let RelayCommand::RecordNotice { text } = command else {
        panic!("a finished sub-agent must be a notice, not {command:?}");
    };
    assert_eq!(
        text,
        "Subagent \"Answer project codename\" (child-ab) finished turn 16 (completed, end of turn)."
    );
    assert!(
        !text.contains("full child output"),
        "the notice must stay terse, not paste the child output: {text}"
    );
}

#[tokio::test]
async fn the_start_follow_up_configures_the_model_before_it_prompts() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 12,
            submitted: submitted_tx,
            view: Some(ready_view("gpt-5-codex")),
        })),
        running_states(),
        Arc::new(NoExports),
    );

    backend
        .start_followup(
            "session-1".into(),
            StartFollowup {
                model: Some("gpt-5-codex".into()),
                effort: None,
                prompt: Some("add a README line".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let (_, first) = submitted.recv().await.unwrap();
    assert_eq!(
        first,
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "gpt-5-codex".into(),
        },
        "the model must be set before the prompt runs under the old one"
    );
    let (_, second) = submitted.recv().await.unwrap();
    assert!(matches!(second, RelayCommand::Prompt { .. }));

    // The status the follow-up records is what a wait blocks on, so it has
    // to name the turn the prompt was accepted as.
    let status = loop {
        match backend.start_status("session-1".into()).await.unwrap() {
            Some(StartStatus::Pending) | None => tokio::task::yield_now().await,
            Some(status) => break status,
        }
    };
    assert_eq!(status, StartStatus::Submitted { turn_id: 12 });
}

#[tokio::test]
async fn the_start_follow_up_turns_on_fast_mode_after_model_when_offered() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 12,
            submitted: submitted_tx,
            view: Some(ready_view_offering_fast_mode("gpt-5.10-luna")),
        })),
        running_states(),
        Arc::new(NoExports),
    );

    backend
        .start_followup(
            "session-1".into(),
            StartFollowup {
                model: Some("gpt-5.10-luna".into()),
                effort: None,
                prompt: Some("add a README line".into()),
                fast_mode: true,
            },
        )
        .await
        .unwrap();

    let (_, first) = submitted.recv().await.unwrap();
    assert_eq!(
        first,
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "gpt-5.10-luna".into(),
        }
    );
    let (_, second) = submitted.recv().await.unwrap();
    assert_eq!(
        second,
        RelayCommand::SetConfig {
            key: "fast-mode".into(),
            value: "on".into(),
        },
        "fast mode must be turned on after the model, before the prompt"
    );
    let (_, third) = submitted.recv().await.unwrap();
    assert!(matches!(third, RelayCommand::Prompt { .. }));
}

#[tokio::test]
async fn the_start_follow_up_skips_fast_mode_silently_when_not_offered() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 12,
            submitted: submitted_tx,
            // This agent offers a model but not fast mode.
            view: Some(ready_view("gpt-5.10-luna")),
        })),
        running_states(),
        Arc::new(NoExports),
    );

    backend
        .start_followup(
            "session-1".into(),
            StartFollowup {
                model: Some("gpt-5.10-luna".into()),
                effort: None,
                prompt: Some("add a README line".into()),
                fast_mode: true,
            },
        )
        .await
        .unwrap();

    let (_, first) = submitted.recv().await.unwrap();
    assert_eq!(
        first,
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "gpt-5.10-luna".into(),
        }
    );
    // No fast-mode SetConfig: the option is not offered, so it is skipped
    // silently and the prompt still goes out.
    let (_, second) = submitted.recv().await.unwrap();
    assert!(
        matches!(second, RelayCommand::Prompt { .. }),
        "a spawn must not fail or stall just because fast mode is unavailable"
    );
}

#[tokio::test]
async fn the_start_follow_up_refuses_a_model_the_agent_does_not_offer() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 12,
            submitted: submitted_tx,
            view: Some(ready_view("gpt-5-codex")),
        })),
        running_states(),
        Arc::new(NoExports),
    );

    backend
        .start_followup(
            "session-1".into(),
            StartFollowup {
                model: Some("no-such-model".into()),
                effort: None,
                prompt: Some("add a README line".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let status = loop {
        match backend.start_status("session-1".into()).await.unwrap() {
            Some(StartStatus::Pending) | None => tokio::task::yield_now().await,
            Some(status) => break status,
        }
    };
    let StartStatus::Failed { message } = status else {
        panic!("an unavailable model must fail the start, not submit the prompt");
    };
    assert!(message.contains("no-such-model"), "unexpected: {message}");
    assert!(
        submitted.try_recv().is_err(),
        "nothing may be submitted once the configuration is refused"
    );
    backend
        .set_config("session-1".into(), "model".into(), "gpt-5-codex".into())
        .await
        .unwrap();
    assert!(
        backend
            .start_status("session-1".into())
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        submitted.recv().await.unwrap().1,
        RelayCommand::SetConfig { .. }
    ));
    assert!(
        submitted.try_recv().is_err(),
        "repair must not replay the abandoned initial prompt"
    );
    assert_eq!(
        backend
            .prompt("session-1".into(), "repaired prompt".into())
            .await
            .unwrap(),
        12
    );
    assert!(matches!(
        submitted.recv().await.unwrap().1,
        RelayCommand::Prompt { .. }
    ));
}

#[tokio::test]
async fn closing_cancels_the_supervised_start_before_any_prompt() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 1,
            submitted: submitted_tx,
            view: None,
        })),
        running_states(),
        Arc::new(NoExports),
    );
    backend
        .start_followup(
            "session-1".into(),
            StartFollowup {
                prompt: Some("must not run".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let task = backend
        .starts
        .lock()
        .unwrap()
        .get_mut("session-1")
        .unwrap()
        .task
        .take()
        .unwrap();
    backend.cancel_start("session-1".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(submitted.try_recv().is_err());
    assert!(
        backend
            .start_status("session-1".into())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn prompt_reports_a_session_the_manager_does_not_hold() {
    let (submitted_tx, _submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 1,
            submitted: submitted_tx,
            view: None,
        })),
        running_states(),
        Arc::new(NoExports),
    );
    let error = backend
        .prompt("session-2".into(), "hello".into())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("session-2 is not running"),
        "unexpected error: {error:#}"
    );
}

/// A checkpoint that finishes only after its requester has given up, so a
/// dropped request can be told apart from a cancelled checkpoint.
struct SlowCheckpoint {
    started: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl ExportRuntime for SlowCheckpoint {
    fn session_record(&self, _session_id: &str) -> Option<SessionRecord> {
        None
    }
    fn checkpoint_now(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
        use std::sync::atomic::Ordering;
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.finished.store(true, Ordering::Release);
            bail!("the archive is not built in this test")
        })
    }
}

/// A client that times out drops the request future. The checkpoint behind a
/// bundle export must run to its end anyway: it has already marked the session
/// `Checkpointing` and holds a barrier on the worker, so abandoning it midway
/// left the session busy with nothing to finish or fail it, and every retry
/// refused for minutes (#1010).
#[tokio::test]
async fn a_dropped_bundle_export_request_does_not_abandon_its_checkpoint() {
    use std::sync::atomic::Ordering;

    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exports = Arc::new(SlowCheckpoint {
        started: started.clone(),
        finished: finished.clone(),
    });

    let request = supervised_checkpoint(exports, "session-1".into());
    // Let the checkpoint start, then abandon the request the way a timed-out
    // client does.
    let abandoned = tokio::time::timeout(Duration::from_millis(5), request).await;
    assert!(abandoned.is_err(), "the checkpoint should still be running");
    assert!(started.load(Ordering::Acquire), "the checkpoint started");
    assert!(
        !finished.load(Ordering::Acquire),
        "the checkpoint was still running when the request was dropped"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        finished.load(Ordering::Acquire),
        "the supervised checkpoint ran to its end without its requester"
    );
}

/// A relative export path resolves against the directory the agent runs in,
/// not the workspace root above it. For a bare project session those differ by
/// one level, which is why a file the agent had just written was refused as
/// "not in the session workspace" (#1079).
#[test]
fn a_file_export_resolves_a_relative_path_against_the_agents_directory() {
    use mj_checkpoint::checkpoint::{CheckpointRepositoryCapture, CheckpointRepositorySpec};

    // The shape `session_export_layout` builds for a bare project session whose
    // agent is launched in `/home/dev/project`.
    let bare = SessionExportLayout {
        backend: targets::TargetLocator::LocalBare {
            worker_root: "/home/dev/.local/share/hel/workers/session".into(),
        },
        workspace_root: "/home/dev".into(),
        primary_repository: "project".into(),
        repositories: vec![CheckpointRepositorySpec {
            id: "project".into(),
            relative_destination: PathBuf::from("project"),
            capture: CheckpointRepositoryCapture::MetadataOnly,
            origin_override: None,
        }],
        managed_worktree: None,
    };
    assert_eq!(
        agent_working_directory(&bare).unwrap(),
        "/home/dev/project",
        "the workspace root is the project's parent, not where the agent runs"
    );

    // The shape it builds for a bundle session, whose agent is launched in the
    // primary repository under the target's workspace directory.
    let bundle = SessionExportLayout {
        backend: targets::TargetLocator::LocalPodman {
            container_id: "hel-session".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        },
        workspace_root: "/workspace".into(),
        primary_repository: "app".into(),
        repositories: vec![
            CheckpointRepositorySpec {
                id: "app".into(),
                relative_destination: PathBuf::from("app"),
                capture: CheckpointRepositoryCapture::RemoteWorkspace,
                origin_override: None,
            },
            CheckpointRepositorySpec {
                id: "lib".into(),
                relative_destination: PathBuf::from("lib"),
                capture: CheckpointRepositoryCapture::RemoteWorkspace,
                origin_override: None,
            },
        ],
        managed_worktree: None,
    };
    assert_eq!(agent_working_directory(&bundle).unwrap(), "/workspace/app");

    // What the target is actually asked to read. The one-repository layout is
    // bounded by the agent's own directory, so the path it is handed is the one
    // the caller typed.
    assert_eq!(
        export_root_and_path(&bare, Path::new("secret.txt")).unwrap(),
        ("/home/dev/project".to_owned(), "secret.txt".to_owned())
    );
    // The bundle is bounded by the workspace root the repositories share, so
    // the path is rewritten to start at the primary repository.
    assert_eq!(
        export_root_and_path(&bundle, Path::new("src/main.rs")).unwrap(),
        ("/workspace".to_owned(), "app/src/main.rs".to_owned())
    );

    // A secondary repository sits beside the primary one under the workspace
    // root, so `..` reaches it. Before paths resolved in the agent's directory
    // this was `lib/README.md`; it must not have become unreachable (#1079).
    assert_eq!(
        export_root_and_path(&bundle, Path::new("../lib/README.md")).unwrap(),
        ("/workspace".to_owned(), "lib/README.md".to_owned())
    );
    assert_eq!(
        export_root_and_path(&bundle, Path::new("./src/../src/main.rs")).unwrap(),
        ("/workspace".to_owned(), "app/src/main.rs".to_owned())
    );

    // Above the workspace root is refused, and the refusal names both the
    // boundary and the directory the path was resolved in.
    for escape in ["../../etc/passwd", "../.."] {
        let ExportError::Refused(message) =
            export_root_and_path(&bundle, Path::new(escape)).unwrap_err()
        else {
            panic!("{escape} must be refused, not attempted");
        };
        assert!(
            message.contains("/workspace") && message.contains("/workspace/app"),
            "the refusal names the boundary and the directory searched: {message}"
        );
    }

    // A one-repository layout has no sibling to reach, and its workspace root
    // is the parent directory holding the user's other projects, not a boundary
    // Hel owns. `..` stops at the agent's directory there.
    let ExportError::Refused(message) =
        export_root_and_path(&bare, Path::new("../other-project/.env")).unwrap_err()
    else {
        panic!("a single-repository layout must not reach outside its own directory");
    };
    assert!(
        message.contains("/home/dev/project"),
        "the refusal names the boundary: {message}"
    );
    assert!(
        !message.contains("/home/dev/other-project"),
        "the refusal does not suggest the path was looked for: {message}"
    );

    // The boundary itself is not a file inside it.
    for (layout, path) in [(&bundle, ".."), (&bare, ".")] {
        assert!(matches!(
            export_root_and_path(layout, Path::new(path)),
            Err(ExportError::Refused(_))
        ));
    }
}

#[test]
fn a_worker_refusal_is_read_without_the_log_lines_beside_it() {
    // F-13: with `RUST_LOG=debug` the worker's log shared standard error with
    // its refusal, and the 409 began with a DEBUG line.
    let refused = |stdout: &str, stderr: &str| match worker_output(
        CommandOutput {
            status: mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        },
        "branch export",
    ) {
        Err(ExportError::Refused(message)) => message,
        other => panic!("a refusal, not {other:?}"),
    };
    let log = "2026-09-23T19:52:35Z DEBUG mj_core::targets: target command finished\n";
    assert_eq!(
        refused(
            "no push remote configured\n",
            &format!("{log}no push remote configured\n")
        ),
        "no push remote configured"
    );
    // A worker from before the change says it only on standard error.
    assert_eq!(
        refused("", "no push remote configured\n"),
        "no push remote configured"
    );
    assert_eq!(refused("", ""), "branch export was refused by the target");
}

fn finished_turn(command_id: &str) -> mj_core::state::MaterializedTurnOutcome {
    mj_core::state::MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: command_id.into(),
        accepted_ordinal: Some(3),
        turn_start_position: Some(3),
        completed_ordinal: 9,
        completed_at_ms: 1,
        outcome: mj_core::state::TurnOutcomeKind::Completed {
            stop_reason: "end_turn".into(),
        },
    }
}

/// An idle child that still owes its report is not finished: Mjolnir is about
/// to remind it. A delivered report is the child's output, not its last
/// message, and the answer says which one it is.
#[test]
fn a_childs_report_decides_whether_an_idle_child_is_finished() {
    let mut record = crate::controller::test_support::checkpoint_test_session("child");
    record.state = SessionState::Running;
    let summary = mj_core::state::MaterializedSessionSummary {
        session_id: "child".into(),
        applied_event_ordinal: 4,
        last_activity_at_ms: None,
        execution: MaterializedExecutionState::Idle,
        session_title: None,
        last_agent_message: Some("Done.".into()),
        last_user_message: None,
        last_agent_message_follows_last_user: true,
        agent_message_latest_content_ordinals: Vec::new(),
        interruption_event_ordinals: Vec::new(),
    };
    let status = |report: &ReportState| {
        subagent_status(
            Some(&record),
            Some(&summary),
            None,
            Some("Done."),
            false,
            &ChildProgress::settled(report.clone()),
        )
    };

    let (state, _, finished) = status(&ReportState::Pending { remind: true });
    assert_eq!((state.as_str(), finished), ("running", false));
    assert_eq!(
        report_source(&state, &ReportState::Pending { remind: true }),
        None
    );

    let delivered = ReportState::Delivered("the full report".into());
    let (state, output, finished) = status(&delivered);
    assert_eq!(
        (state.as_str(), output.as_deref(), finished),
        ("completed", Some("the full report"), true)
    );
    assert_eq!(report_source(&state, &delivered), Some("handback"));

    let (state, output, _) = status(&ReportState::Fallback);
    assert_eq!(output.as_deref(), Some("Done."));
    assert_eq!(
        report_source(&state, &ReportState::Fallback),
        Some("last_message")
    );
}

/// Nothing to remind: the child has no tool, or its next task is already
/// queued. Neither case reaches the store or the child.
#[tokio::test]
async fn a_child_without_the_tool_or_with_queued_work_is_not_reminded() {
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 1,
            submitted: submitted_tx,
            view: None,
        })),
        running_states(),
        Arc::new(NoExports),
    );
    let turn = finished_turn("task");
    assert!(
        !backend
            .remind_subagent_to_hand_back("child-1", false, &turn, &[])
            .await
            .unwrap()
    );
    assert!(
        !backend
            .remind_subagent_to_hand_back("child-1", true, &turn, &["api-next".into()])
            .await
            .unwrap()
    );
    assert!(
        submitted.try_recv().is_err(),
        "no prompt may reach the child"
    );
}

const HANDBACK_TEST_CHILD: &str = "MJ_HANDBACK_TEST_CHILD";

/// A child that ended its turn without a report is reminded once, with a
/// prompt its transcript shows, and the reminder is recorded so the next look
/// at the same turn does not send another.
#[tokio::test]
async fn a_child_that_owes_its_report_is_reminded_once() {
    if std::env::var_os(HANDBACK_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        crate::controller::test_support::IsolatedTest::new(
            crate::controller::test_support::test_name(
                module_path!(),
                "a_child_that_owes_its_report_is_reminded_once",
            ),
        )
        .env(HANDBACK_TEST_CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
    let backend = ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 1,
            submitted: submitted_tx,
            view: None,
        })),
        running_states(),
        Arc::new(NoExports),
    );
    let turn = finished_turn("task");

    assert!(
        backend
            .remind_subagent_to_hand_back("child-1", true, &turn, &[])
            .await
            .unwrap()
    );
    let (command_id, command) = submitted.recv().await.unwrap();
    assert!(
        mj_core::subagent::is_handback_reminder(&command_id),
        "{command_id}"
    );
    let RelayCommand::Prompt { prompt } = command else {
        panic!("the reminder is an ordinary prompt, not {command:?}");
    };
    let [ContentBlock::Text(text)] = prompt.as_slice() else {
        panic!("the reminder is one text block");
    };
    assert_eq!(text.text, mj_core::subagent::HANDBACK_REMINDER_TEXT);
    let recorded = crate::database::load_subagent_report("child-1").unwrap();
    assert_eq!(
        recorded.reminder.as_ref().map(|reminder| (
            reminder.command_id.as_str(),
            reminder.for_command_id.as_str()
        )),
        Some((command_id.as_str(), "task"))
    );

    assert!(
        !backend
            .remind_subagent_to_hand_back("child-1", true, &turn, &[])
            .await
            .unwrap(),
        "one reminder per turn"
    );
    assert!(submitted.try_recv().is_err());
}

/// A child hands back one report per turn. The report is recorded against the
/// turn that is running, a repeat in the same turn is refused without being an
/// error, and a session that is not such a child cannot hand anything back.
#[tokio::test]
async fn a_child_hands_back_one_report_per_turn() {
    if std::env::var_os(HANDBACK_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        crate::controller::test_support::IsolatedTest::new(
            crate::controller::test_support::test_name(
                module_path!(),
                "a_child_hands_back_one_report_per_turn",
            ),
        )
        .env(HANDBACK_TEST_CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::save_session(&parent_record("parent-1", "parent")).unwrap();
    let mut child = parent_record("child-1", "helper");
    child.title = "child".into();
    crate::database::save_subagent_session(
        &child,
        &mj_core::subagent::SubagentRecord {
            child_session_id: "child-1".into(),
            parent_session_id: "parent-1".into(),
            task_name: "audit deps".into(),
            profile_id: "helper".into(),
            model: None,
            effort: None,
            working_directory: Default::default(),
            initial_prompt: "check the lockfile".into(),
            request_key: "request-1".into(),
            created_at: "2026-09-24T00:00:00Z".into(),
            noticed_turn: None,
            handback_tool: true,
        },
    )
    .unwrap();
    let mut view = ready_view("model");
    view.snapshot.as_mut().unwrap().materialized.active_turn =
        Some(mj_core::state::MaterializedTurn {
            command_id: "task".into(),
            accepted_ordinal: Some(3),
            turn_start_position: 3,
            started_at_ms: 1,
            steered_into: None,
        });
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 1,
            submitted: mpsc::unbounded_channel().0,
            view: Some(view),
        })),
        running_states(),
        Arc::new(NoExports),
    ));
    let hand_back = |session: &str, message: &str| {
        let backend = backend.clone();
        let session = session.to_owned();
        let message = message.to_owned();
        async move {
            backend
                .execute_subagent_tool(
                    session,
                    mj_core::subagent::SubagentToolRequest {
                        request_id: "request".into(),
                        created_at_ms: 0,
                        action: mj_core::subagent::SubagentToolAction::Handback { message },
                    },
                )
                .await
        }
    };

    let first = hand_back("child-1", "the full report").await;
    assert!(!first.is_error, "{}", first.message);
    assert!(
        first.message.contains("\"delivered\": true"),
        "{}",
        first.message
    );
    let recorded = crate::database::load_subagent_report("child-1").unwrap();
    assert_eq!(
        recorded
            .handback
            .map(|handback| (handback.command_id, handback.message)),
        Some(("task".to_owned(), "the full report".to_owned()))
    );

    let second = hand_back("child-1", "a second report").await;
    assert!(!second.is_error, "{}", second.message);
    assert!(
        second.message.contains("already delivered"),
        "{}",
        second.message
    );

    let empty = hand_back("child-1", "  ").await;
    assert!(
        empty.is_error && empty.message.contains("cannot be empty"),
        "{}",
        empty.message
    );

    // A report past the cap is refused, and the refusal says where the
    // details go, so the child can retry with a short report.
    let long = hand_back(
        "child-1",
        &"x".repeat(mj_core::subagent::MAX_HANDBACK_CHARS + 1),
    )
    .await;
    assert!(long.is_error, "{}", long.message);
    assert!(
        long.message.contains("report directory") && long.message.contains("4000"),
        "{}",
        long.message
    );

    let stranger = hand_back("parent-1", "a report").await;
    assert!(stranger.is_error, "{}", stranger.message);
    assert!(
        stranger.message.contains("only for a Mjolnir sub-agent"),
        "{}",
        stranger.message
    );
}

/// Found in a live run: a parent waited right after spawning, the store had
/// not seen the child's first turn yet, and the idle child read as completed
/// with no output. A child is not done until a finished turn reaches the
/// parent's newest prompt, and a turn that failed says so and why.
#[test]
fn a_child_is_done_only_when_its_newest_prompt_is_answered_and_says_how_it_failed() {
    let mut record = crate::controller::test_support::checkpoint_test_session("child");
    record.state = SessionState::Running;
    let summary = mj_core::state::MaterializedSessionSummary {
        session_id: "child".into(),
        applied_event_ordinal: 4,
        last_activity_at_ms: None,
        execution: MaterializedExecutionState::Idle,
        session_title: None,
        last_agent_message: None,
        last_user_message: None,
        last_agent_message_follows_last_user: false,
        agent_message_latest_content_ordinals: Vec::new(),
        interruption_event_ordinals: Vec::new(),
    };
    let progress =
        |awaited: Option<u64>, answered: Option<u64>, failed: Option<(&'static str, &str)>| {
            ChildProgress {
                report: ReportState::Fallback,
                awaited_ordinal: awaited,
                answered_ordinal: answered,
                failed_turn: failed.map(|(state, reason)| (state, reason.to_owned())),
                login_failure: None,
                report_dir: None,
            }
        };
    let status = |start: Option<&StartStatus>, progress: &ChildProgress| {
        subagent_status(Some(&record), Some(&summary), start, None, false, progress)
    };

    // The first prompt was accepted as turn 20; no turn has finished yet.
    let submitted = StartStatus::Submitted { turn_id: 20 };
    assert_eq!(
        status(Some(&submitted), &progress(None, None, None)),
        ("running".into(), None, false)
    );
    // The same, known from the store after the start follow-up is forgotten.
    assert_eq!(
        status(None, &progress(Some(20), None, None)),
        ("running".into(), None, false)
    );
    // A follow-up prompt the finished turn is older than.
    assert!(!status(None, &progress(Some(45), Some(20), None)).2);
    // Answered, and the turn hit a usage limit.
    assert_eq!(
        status(
            None,
            &progress(
                Some(20),
                Some(20),
                Some(("failed", "You've hit your usage limit."))
            )
        ),
        (
            "failed".into(),
            Some("You've hit your usage limit.".into()),
            true
        )
    );
    assert_eq!(report_source("failed", &ReportState::Fallback), None);
}

/// #1160: a Codex child whose profile could not sign in died on its first
/// request, and `wait` handed its parent the error text as the child's report.
/// The parent concluded the profile was dead. The answer now says the turn
/// failed, which profile's login was refused, and what fixes it.
#[test]
fn a_child_whose_login_was_refused_fails_naming_its_profile_and_the_fix() {
    let mut record = crate::controller::test_support::checkpoint_test_session("child");
    record.state = SessionState::Running;
    let summary = mj_core::state::MaterializedSessionSummary {
        session_id: "child".into(),
        applied_event_ordinal: 4,
        last_activity_at_ms: None,
        execution: MaterializedExecutionState::Idle,
        session_title: None,
        last_agent_message: Some(
            "Your access token could not be refreshed. Please log out and sign in again.".into(),
        ),
        last_user_message: None,
        last_agent_message_follows_last_user: true,
        agent_message_latest_content_ordinals: Vec::new(),
        interruption_event_ordinals: Vec::new(),
    };
    let progress = ChildProgress {
        report: ReportState::Fallback,
        awaited_ordinal: Some(20),
        answered_ordinal: Some(20),
        failed_turn: Some((
            "failed",
            "Your access token could not be refreshed. Please log out and sign in again.".into(),
        )),
        login_failure: Some("codex4".into()),
        report_dir: None,
    };
    let expected = "profile codex4: the login is no longer valid; run `mj login --profile codex4` and spawn again";

    let (state, output, finished) =
        subagent_status(Some(&record), Some(&summary), None, None, false, &progress);
    assert_eq!(
        (state.as_str(), output.as_deref(), finished),
        ("failed", Some(expected), true)
    );
    let entry = super::wait_agent_entry("child", &state, output, finished, &progress);
    assert_eq!(entry["state"], "failed", "{entry}");
    assert_eq!(entry["output"], expected, "{entry}");
    assert_eq!(entry["report_source"], serde_json::Value::Null, "{entry}");
    assert_eq!(
        entry["failure"],
        serde_json::json!({"kind": "login_invalid", "profile_id": "codex4"}),
        "{entry}"
    );

    // A report the child did hand back is still its report.
    let delivered = ChildProgress {
        report: ReportState::Delivered("done".into()),
        ..progress
    };
    assert_eq!(
        subagent_status(Some(&record), Some(&summary), None, None, false, &delivered),
        ("completed".into(), Some("done".into()), true)
    );
}

/// Once the credential sync has found a profile's login refused, a spawn on it
/// is refused at once instead of starting a child that cannot sign in. An
/// unpinned spawn goes to another profile that offers the model.
#[tokio::test]
async fn spawn_refuses_a_profile_whose_login_is_known_to_be_refused() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        r#"{"auth_mode":"chatgpt","last_refresh":"2026-09-25T20:00:00.000Z"}"#,
    )
    .unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: Default::default(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    let mut rejected = mj_core::credentials::RejectedLogins::default();
    rejected.observe(
        &mj_core::credentials::CredentialSyncResult {
            profile_id: "codex-high".into(),
            trigger: Some(mj_core::credentials::CredentialSyncCause {
                session_id: "child-1".into(),
                reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
            }),
            failure: None,
            outcomes: vec![mj_core::credentials::CredentialSyncOutcome {
                session_id: "child-1".into(),
                outcome: Ok(Vec::new()),
            }],
        },
        &profile,
    );
    let (backend, exports) = selection_backend_refusing(SAME_MODELS, None, rejected).await;

    let pinned = backend
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(Some("codex-high"), Some("luna"), Some("low")),
        )
        .await;
    assert!(pinned.is_error, "{}", pinned.message);
    assert!(
        pinned.message.contains(
            "the login is no longer valid; run `mj login --profile codex-high` and spawn again"
        ),
        "{}",
        pinned.message
    );
    assert!(exports.registered.lock().unwrap().is_none());

    let listed = backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                request_id: "request-2".into(),
                created_at_ms: 0,
                action: mj_core::subagent::SubagentToolAction::ListProfiles,
            },
        )
        .await;
    let answer: serde_json::Value = serde_json::from_str(&listed.message).unwrap();
    assert_eq!(
        answer["unavailable"][0]["profile_id"], "codex-high",
        "{answer}"
    );

    backend
        .execute_subagent_tool(
            "parent-1".into(),
            spawn_with(None, Some("luna"), Some("low")),
        )
        .await;
    assert_eq!(exports.registered().profile_id, "parent");
}

/// A last message that stands in for a missing handback has no bound, so the
/// wait cuts it and tells the parent how to get the rest; every entry names
/// the child's report directory.
#[test]
fn a_wait_entry_bounds_its_output_and_names_the_report_directory() {
    let mut progress = ChildProgress::settled(ReportState::Fallback);
    progress.report_dir = Some("/workspace/p/.mj-agents/c1".into());
    let long = "y".repeat(mj_core::subagent::MAX_HANDBACK_CHARS + 10);
    let entry = super::wait_agent_entry("c1", "completed", Some(long), true, &progress);
    assert_eq!(entry["truncated"], true, "{entry}");
    assert_eq!(entry["report_dir"], "/workspace/p/.mj-agents/c1");
    assert_eq!(entry["report_source"], "last_message");
    let output = entry["output"].as_str().unwrap();
    assert!(output.contains("10 more characters") && output.contains("send_input"));

    let short = super::wait_agent_entry(
        "c1",
        "completed",
        Some("done".into()),
        true,
        &ChildProgress::settled(ReportState::Delivered("done".into())),
    );
    assert_eq!(short["output"], "done");
    assert!(short.get("truncated").is_none(), "{short}");
    assert_eq!(short["report_source"], "handback");
}
