use super::*;

use crate::server_runtime::profile_catalog::{ProfileCatalog, counting_probe, test_config};
use mj_core::config::HarnessKind;
use mj_core::state::SessionRecord;

#[test]
fn child_progress_uses_the_published_turn_and_report_together() {
    let mut committed = crate::database::CommittedState {
        sequence: 0,
        state: Default::default(),
        moves: Default::default(),
        native_agents: Default::default(),
        startup_groups: Default::default(),
        subagent_reports: Default::default(),
        turns: Default::default(),
        wait_revisions: Default::default(),
    };
    let turn = mj_core::state::MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: "task".into(),
        accepted_ordinal: Some(7),
        turn_start_position: Some(8),
        completed_ordinal: 12,
        completed_at_ms: 10,
        outcome: mj_core::state::TurnOutcomeKind::Interrupted {
            reason: None,
            message: "worker stopped".into(),
        },
    };
    committed.turns.insert_shared(
        "child".into(),
        crate::database::CommittedTurn {
            state: (MaterializedExecutionState::Idle, None, Some(turn)),
            failed_message: Some("owned failure details".into()),
        },
    );
    committed.subagent_reports.insert_shared(
        "child".into(),
        mj_core::subagent::SubagentReport {
            awaited_ordinal: Some(15),
            report_dir: Some("/reports/child".into()),
            ..Default::default()
        },
    );
    let progress = child_progress(&committed, "child");
    assert_eq!(progress.finished_span, Some((8, 12)));
    assert_eq!(progress.answered_ordinal, Some(7));
    assert!(
        progress.awaiting_prompt(None),
        "an older completed turn cannot answer the new prompt"
    );
    assert_eq!(
        progress.failed_turn.as_ref().map(|(state, _)| *state),
        Some("interrupted")
    );
    assert_eq!(progress.report_dir.as_deref(), Some("/reports/child"));
    assert_eq!(child_progress(&committed, "other").failed_turn, None);
}

fn quota(profile_id: &str, harness: HarnessKind, remaining: &[u8]) -> ProfileQuota {
    ProfileQuota {
        banked_resets: None,
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
        rate_limited_until_epoch_seconds: None,
    }
}

/// A pay-per-use profile's report: no windows, the API label instead.
fn usage_priced(profile_id: &str) -> ProfileQuota {
    ProfileQuota {
        banked_resets: None,
        profile_id: profile_id.into(),
        harness: HarnessKind::Codex,
        windows: Vec::new(),
        extra: Some(crate::quota::API_LABEL.to_owned()),
        error: None,
        refreshed_at_epoch_seconds: 0,
        rate_limited_until_epoch_seconds: None,
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

fn spawn_with(
    profile_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> mj_core::subagent::SubagentToolRequest {
    mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
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

// Hard-won: 435b4ed9: model selection sent a child to a nearly exhausted profile despite an eligible profile with quota.
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

/// Profiles offering the same models are one choice for the parent, shown as
/// the one with the most quota left; a profile offering other models is never
/// hidden behind a same-harness profile with more quota.
// Hard-won: 435b4ed9: one same-harness profile hid models offered by another profile.
#[tokio::test]
async fn list_profiles_merges_profiles_offering_the_same_models() {
    let (backend, _) = selection_backend(SAME_MODELS, None).await;

    let result = backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
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
                originating_command_id: None,
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

/// A close is accepted long before the child is gone: it cancels whatever owned
/// the session, checkpoints, seals the relay and tears the process tree down,
/// and the record only says `Closing` once that is under way. A child that had
/// finished its turn therefore reported `completed` the moment its close was
/// admitted, so a parent that closed a child and spawned its replacement stacked
/// both process trees inside one container (#1087). A wait must follow the close
/// instead, the way a session-level wait does.
// Hard-won: 8308bd0c: wait reported close complete before teardown released the child process tree.
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
// Hard-won: d8501891: wait and list_agents replaced the recorded startup cause with a misleading symptom.
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
        assessment: None,
        assessment_context: None,
        turn_completion: None,
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
        task_settled_at_ms: None,
        background_needed: None,
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
        harness_preparation: None,
        agent_capabilities: None,
        agent_info: None,
        runtime: None,
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

/// A parent actor that applies prompt queue commands to a shared view, so
/// reconciliation tests can observe queueing, coalescing, and withdrawal.
#[derive(Clone)]
struct WaitPromptSession {
    inner: FakeSession,
    view: Arc<std::sync::Mutex<ManagedSessionView>>,
}

impl SessionHandleBackend for WaitPromptSession {
    fn search_prompts(
        &self,
        bundle_id: String,
        scope: mj_core::storage::HistoryScope,
        query: String,
    ) -> BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>> {
        self.inner.search_prompts(bundle_id, scope, query)
    }
    fn review_state(&self) -> BoxFuture<'_, Result<mj_client::session::ReviewState>> {
        self.inner.review_state()
    }
    fn config_result(&self, command_id: String) -> BoxFuture<'_, Result<Option<Option<String>>>> {
        self.inner.config_result(command_id)
    }
    fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
        Box::new(self.clone())
    }
    fn session_id(&self) -> &str {
        self.inner.session_id()
    }
    fn view(&self) -> ManagedSessionView {
        self.view.lock().unwrap().clone()
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
        let _ = self
            .inner
            .submitted
            .send((command_id.clone(), command.clone()));
        let view = self.view.clone();
        let accepted_ordinal = self.inner.accepted_ordinal;
        Box::pin(async move {
            let mut view = view.lock().unwrap();
            let Some(snapshot) = view.snapshot.as_mut() else {
                anyhow::bail!("the test parent has no snapshot")
            };
            match command {
                RelayCommand::Prompt { prompt } => {
                    let content = prompt
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    if snapshot.materialized.active_turn.is_some() {
                        snapshot.materialized.queued_prompts.push(
                            mj_core::state::MaterializedQueuedPrompt {
                                command_id,
                                kind: Default::default(),
                                content,
                                queued_at_ms: 1,
                                accepted_ordinal: Some(accepted_ordinal),
                            },
                        );
                    } else {
                        snapshot.materialized.execution =
                            MaterializedExecutionState::Running { started_at_ms: 1 };
                        snapshot.materialized.active_turn =
                            Some(mj_core::state::MaterializedTurn {
                                command_id: command_id.clone(),
                                accepted_ordinal: Some(accepted_ordinal),
                                turn_start_position: accepted_ordinal,
                                started_at_ms: 1,
                                steered_into: None,
                            });
                        snapshot.materialized.transcript.push(Arc::new(
                            mj_core::transcript::TranscriptItem {
                                stable_id: format!("user-{command_id}"),
                                position: accepted_ordinal,
                                latest_content_event_ordinal: None,
                                created_at_ms: 1,
                                last_changed_at_ms: 1,
                                body: mj_core::transcript::TranscriptBody::User { content },
                            },
                        ));
                    }
                }
                RelayCommand::RemoveQueuedPrompt { queued_command_id } => {
                    snapshot
                        .materialized
                        .queued_prompts
                        .retain(|prompt| prompt.command_id != queued_command_id);
                }
                _ => {}
            }
            Ok(PendingRelaySubmit::new(Box::pin(async move {
                Ok(accepted_ordinal)
            })))
        })
    }
    fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
        Box::pin(async { Ok(PendingRelaySync::new(Box::pin(async { Ok(()) }))) })
    }
    fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) -> BoxFuture<'_, Result<()>> {
        self.inner.respond_elicitation(elicitation_id, response)
    }
    fn stop_background_task(&self, background_task_id: String) -> BoxFuture<'_, Result<()>> {
        self.inner.stop_background_task(background_task_id)
    }
    fn reviewer(
        &self,
        role: Option<String>,
        action: mj_client::session::ReviewerAction,
    ) -> BoxFuture<'_, Result<mj_client::session::ReviewerOutcome>> {
        self.inner.reviewer(role, action)
    }
}

struct WaitPromptControl(WaitPromptSession);

impl SessionControlBackend for WaitPromptControl {
    fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>> {
        let session = self.0.clone();
        Box::pin(async move {
            anyhow::ensure!(session_id == session.session_id(), "unknown session");
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
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        subagents: Some(mj_core::subagent::SubagentPolicy::AllModels),
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
        review: None,
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

// The behaviour the whole change exists for: once the background pass has
// discovered the profiles, the tool call itself discovers nothing.

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

// Hard-won: 131223a9: a child-finish notice was forged into a user turn instead of returned as a tool result.
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

#[derive(Clone)]
struct ConfigSession {
    inner: FakeSession,
}

impl SessionHandleBackend for ConfigSession {
    fn search_prompts(
        &self,
        bundle: String,
        scope: mj_core::storage::HistoryScope,
        query: String,
    ) -> BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>> {
        self.inner.search_prompts(bundle, scope, query)
    }
    fn review_state(&self) -> BoxFuture<'_, Result<mj_client::session::ReviewState>> {
        self.inner.review_state()
    }
    fn config_result(&self, id: String) -> BoxFuture<'_, Result<Option<Option<String>>>> {
        self.inner.config_result(id)
    }
    fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
        Box::new(self.clone())
    }
    fn session_id(&self) -> &str {
        self.inner.session_id()
    }
    fn view(&self) -> ManagedSessionView {
        self.inner.view()
    }
    fn is_stopped(&self) -> bool {
        false
    }
    fn has_changed(&self) -> Result<bool> {
        Ok(false)
    }
    fn changed(&mut self) -> BoxFuture<'_, Result<ManagedSessionView>> {
        self.inner.changed()
    }
    fn enqueue_submit(
        &self,
        id: String,
        command: RelayCommand,
    ) -> BoxFuture<'_, Result<PendingRelaySubmit>> {
        self.inner.enqueue_submit(id, command)
    }
    fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
        self.inner.enqueue_sync()
    }
    fn respond_elicitation(
        &self,
        id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) -> BoxFuture<'_, Result<()>> {
        self.inner.respond_elicitation(id, response)
    }
    fn stop_background_task(&self, id: String) -> BoxFuture<'_, Result<()>> {
        self.inner.stop_background_task(id)
    }
    fn reviewer(
        &self,
        role: Option<String>,
        action: mj_client::session::ReviewerAction,
    ) -> BoxFuture<'_, Result<mj_client::session::ReviewerOutcome>> {
        self.inner.reviewer(role, action)
    }
}

fn config_session(
    view: ManagedSessionView,
) -> (
    SessionHandle,
    mpsc::UnboundedReceiver<(String, RelayCommand)>,
) {
    let (submitted, received) = mpsc::unbounded_channel();
    (
        SessionHandle::new(ConfigSession {
            inner: FakeSession {
                session_id: "session-1".into(),
                accepted_ordinal: 12,
                submitted,
                view: Some(view),
            },
        }),
        received,
    )
}

#[tokio::test]
async fn startup_configuration_retries_under_the_same_command_id() {
    // Deduplication lives in the worker: a submit whose id it has already
    // accepted answers with the original ordinal. The daemon's part is to
    // retry a step with the step's own id, never a fresh one.
    let (handle, mut submitted) = config_session(ready_view("gpt-5-codex"));
    for _ in 0..2 {
        assert_eq!(
            crate::daemon::startup_followup::configure_startup(
                &handle,
                "startup:model",
                "model",
                "gpt-5-codex",
                false,
            )
            .await
            .unwrap(),
            Some(12)
        );
    }
    for _ in 0..2 {
        assert_eq!(
            submitted.recv().await.unwrap(),
            (
                "startup:model".into(),
                RelayCommand::SetConfig {
                    key: "model".into(),
                    value: "gpt-5-codex".into(),
                }
            )
        );
    }
    assert!(submitted.try_recv().is_err());
}

#[tokio::test]
async fn startup_status_survives_backend_recreation_and_keeps_acceptance_ordinal() {
    if !isolated_parked_test(
        "startup_status_survives_backend_recreation_and_keeps_acceptance_ordinal",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    crate::database::enqueue_startup_deliveries(vec![crate::database::StartupDelivery {
        session_id: "child-1".into(),
        command_id: "startup:prompt".into(),
        step_json: serde_json::to_string(&crate::daemon::StartupStep::ApiPrompt {
            text: "first prompt".into(),
        })
        .unwrap(),
        phase: "pending".into(),
        group_id: Some("startup".into()),
        accepted_ordinal: None,
        error: None,
    }])
    .unwrap();
    assert_eq!(
        load_startup_status("child-1".into()).await.unwrap(),
        Some(StartStatus::Pending)
    );
    crate::database::set_startup_delivery_phase("startup:prompt", "delivering", None).unwrap();
    crate::database::set_startup_delivery_accepted("startup:prompt", Some(73)).unwrap();
    crate::database::set_startup_delivery_phase("startup:prompt", "done", None).unwrap();
    let exports = ParkingExports::new(SessionState::Running, None);
    let (backend, _) = parking_backend(exports, &[], Arc::new(|| {}));
    assert_eq!(
        backend.start_status("child-1".into()).await.unwrap(),
        Some(StartStatus::Submitted { turn_id: 73 })
    );
    crate::database::cancel_startup_groups("child-1").unwrap();
    assert_eq!(backend.start_status("child-1".into()).await.unwrap(), None);
}

#[tokio::test]
async fn dismissing_observed_startup_preserves_a_newer_followup() {
    if !isolated_parked_test("dismissing_observed_startup_preserves_a_newer_followup") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    for group in ["old", "new"] {
        crate::database::enqueue_startup_deliveries(vec![crate::database::StartupDelivery {
            session_id: "child-1".into(),
            command_id: format!("{group}:prompt"),
            step_json: serde_json::to_string(&crate::daemon::StartupStep::ApiPrompt {
                text: group.into(),
            })
            .unwrap(),
            phase: "pending".into(),
            group_id: Some(group.into()),
            accepted_ordinal: None,
            error: None,
        }])
        .unwrap();
        if group == "old" {
            crate::database::set_startup_delivery_phase("old:prompt", "delivering", None).unwrap();
            crate::database::set_startup_delivery_accepted("old:prompt", Some(1)).unwrap();
            crate::database::set_startup_delivery_phase("old:prompt", "done", None).unwrap();
        }
    }
    crate::database::dismiss_startup_group("child-1", "old").unwrap();
    assert_eq!(
        load_startup_status("child-1".into()).await.unwrap(),
        Some(StartStatus::Pending)
    );
    assert_eq!(
        crate::database::next_startup_delivery("child-1")
            .unwrap()
            .unwrap()
            .command_id,
        "new:prompt"
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
// Hard-won: 4a7c9a62: dropping a timed-out export handler abandoned its active checkpoint.
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
// Hard-won: f870729d: relative export paths were searched one directory above the agent working directory.
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

// Hard-won: 983f5078: DEBUG stderr lines contaminated the worker refusal returned to the caller.
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
    assert_eq!(
        command,
        RelayCommand::HandbackReminder {
            completed_command_id: "task".into(),
            completed_ordinal: turn.completed_ordinal,
        }
    );
    let recorded = crate::database::load_subagent_report("child-1").unwrap();
    assert_eq!(
        recorded.reminder.as_ref().map(|reminder| (
            reminder.command_id.as_str(),
            reminder.for_command_id.as_str()
        )),
        Some((command_id.as_str(), "task"))
    );

    assert!(
        backend
            .remind_subagent_to_hand_back("child-1", true, &turn, &[])
            .await
            .unwrap(),
        "the already admitted reminder still owns the pending report"
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
            reported_finish: None,
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
                        originating_command_id: None,
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
// Hard-won: a91cfd04: a live child was reported completed before its first turn reached the store.
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
                finished_span: None,
                last_completed_ordinal: None,
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
// Hard-won: 1b077959: refused child credentials were reported as task output without naming the broken profile.
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
        finished_span: None,
        last_completed_ordinal: None,
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
// Hard-won: 1b077959: refused credentials allowed ten more doomed children to spawn on the same profile.
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
        subagents: Default::default(),
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
                originating_command_id: None,
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
// Hard-won: cc7e6a2a: large handbacks were inlined into wait and caused repeated context-heavy polling.
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
    assert!(output.contains("10 more characters") && output.contains("send_message"));

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

/// The daemon side of a parent and one child, as the sub-agent tools read
/// it: the child's record, and a restart that either succeeds, making the
/// record running, or fails with the given reason and leaves it parked.
struct ParkingExports {
    records: std::sync::Mutex<BTreeMap<String, SessionRecord>>,
    unparks: std::sync::atomic::AtomicUsize,
    unpark_failure: Option<String>,
    mailboxes_enabled: bool,
}

impl ParkingExports {
    fn new(child_state: SessionState, unpark_failure: Option<&str>) -> Arc<Self> {
        Self::with_mailboxes_enabled(child_state, unpark_failure, false)
    }

    fn with_mailboxes_enabled(
        child_state: SessionState,
        unpark_failure: Option<&str>,
        mailboxes_enabled: bool,
    ) -> Arc<Self> {
        let mut child = parent_record("child-1", "helper");
        child.state = child_state;
        let records = [parent_record("parent-1", "parent"), child]
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        Arc::new(Self {
            records: std::sync::Mutex::new(records),
            unparks: Default::default(),
            unpark_failure: unpark_failure.map(str::to_owned),
            mailboxes_enabled,
        })
    }

    fn set_child_state(&self, state: SessionState) {
        self.records
            .lock()
            .unwrap()
            .get_mut("child-1")
            .unwrap()
            .state = state;
    }

    fn child_state(&self) -> SessionState {
        self.records.lock().unwrap()["child-1"].state
    }

    fn unparks(&self) -> usize {
        self.unparks.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ExportRuntime for ParkingExports {
    fn agent_mailboxes_enabled(&self) -> bool {
        self.mailboxes_enabled
    }

    fn startup_status(&self, session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>> {
        Box::pin(load_startup_status(session_id))
    }

    fn session_record(&self, session_id: &str) -> Option<SessionRecord> {
        self.records.lock().unwrap().get(session_id).cloned()
    }
    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
        Box::pin(async move { bail!("session {session_id} cannot be checkpointed in a test") })
    }
    fn unpark_subagent(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.unparks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(failure) = &self.unpark_failure {
                bail!("{failure}");
            }
            if self.records.lock().unwrap()[&session_id].state == SessionState::Parked {
                self.set_child_state(SessionState::Running);
            }
            Ok(())
        })
    }
}

type WaitPromptBackend = (
    Arc<ApiBackend>,
    Arc<ParkingExports>,
    Arc<std::sync::Mutex<ManagedSessionView>>,
    mpsc::UnboundedReceiver<(String, RelayCommand)>,
);

fn wait_prompt_backend(view: ManagedSessionView) -> WaitPromptBackend {
    let exports = ParkingExports::new(SessionState::Parked, None);
    let shared_view = Arc::new(std::sync::Mutex::new(view));
    let (submitted, received) = mpsc::unbounded_channel();
    let session = WaitPromptSession {
        inner: FakeSession {
            session_id: "parent-1".into(),
            accepted_ordinal: 30,
            submitted,
            view: None,
        },
        view: shared_view.clone(),
    };
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(WaitPromptControl(session)),
        running_states(),
        exports.clone(),
    ));
    (backend, exports, shared_view, received)
}

fn active_parent_view(text: &str) -> ManagedSessionView {
    let mut view = ready_view("model");
    let snapshot = view.snapshot.as_mut().unwrap();
    snapshot.materialized.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    snapshot.materialized.active_turn = Some(mj_core::state::MaterializedTurn {
        command_id: "parent-turn".into(),
        accepted_ordinal: Some(2),
        turn_start_position: 3,
        started_at_ms: 1,
        steered_into: None,
    });
    if !text.is_empty() {
        snapshot
            .materialized
            .transcript
            .push(Arc::new(mj_core::transcript::TranscriptItem {
                stable_id: "parent-user".into(),
                position: 3,
                latest_content_event_ordinal: None,
                created_at_ms: 1,
                last_changed_at_ms: 1,
                body: mj_core::transcript::TranscriptBody::User {
                    content: vec![serde_json::json!({"type":"text", "text":text})],
                },
            }));
    }
    view
}

/// A child's session actor whose next submissions fail as scripted before
/// it delivers again: `false` is a definite rejection, what a park that
/// finished around the prompt answers, and `true` a failure that may have
/// delivered it. Each failure first runs `on_failure`.
#[derive(Clone)]
struct ScriptedSession {
    inner: FakeSession,
    failures: Arc<std::sync::Mutex<std::collections::VecDeque<bool>>>,
    on_failure: Arc<dyn Fn() + Send + Sync>,
    /// Syncs refused the way the session actor refuses while a lifecycle
    /// operation such as a park holds it.
    reserved_syncs: Arc<std::sync::atomic::AtomicUsize>,
}

impl SessionHandleBackend for ScriptedSession {
    fn search_prompts(
        &self,
        bundle_id: String,
        scope: mj_core::storage::HistoryScope,
        query: String,
    ) -> BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>> {
        self.inner.search_prompts(bundle_id, scope, query)
    }
    fn review_state(&self) -> BoxFuture<'_, Result<mj_client::session::ReviewState>> {
        self.inner.review_state()
    }
    fn config_result(&self, command_id: String) -> BoxFuture<'_, Result<Option<Option<String>>>> {
        self.inner.config_result(command_id)
    }
    fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
        Box::new(self.clone())
    }
    fn session_id(&self) -> &str {
        self.inner.session_id()
    }
    fn view(&self) -> ManagedSessionView {
        self.inner.view()
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
        let failure = self.failures.lock().unwrap().pop_front();
        let Some(unconfirmed) = failure else {
            return self.inner.enqueue_submit(command_id, command);
        };
        (self.on_failure)();
        Box::pin(async move {
            Ok(PendingRelaySubmit::new(Box::pin(async move {
                let error = anyhow!("session target is changing");
                Err(if unconfirmed {
                    error.context(mj_client::session::DeliveryUnconfirmed)
                } else {
                    error
                })
            })))
        })
    }
    fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
        use std::sync::atomic::Ordering;
        if self
            .reserved_syncs
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Box::pin(async {
                Ok(PendingRelaySync::new(Box::pin(async {
                    anyhow::bail!("session is reserved for a lifecycle operation")
                })))
            });
        }
        self.inner.enqueue_sync()
    }
    fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) -> BoxFuture<'_, Result<()>> {
        self.inner.respond_elicitation(elicitation_id, response)
    }
    fn stop_background_task(&self, background_task_id: String) -> BoxFuture<'_, Result<()>> {
        self.inner.stop_background_task(background_task_id)
    }
    fn reviewer(
        &self,
        role: Option<String>,
        action: mj_client::session::ReviewerAction,
    ) -> BoxFuture<'_, Result<mj_client::session::ReviewerOutcome>> {
        self.inner.reviewer(role, action)
    }
}

struct ScriptedControl(ScriptedSession);

impl SessionControlBackend for ScriptedControl {
    fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>> {
        let session = self.0.clone();
        Box::pin(async move {
            anyhow::ensure!(session_id == session.inner.session_id, "unknown session");
            Ok(SessionHandle::new(session))
        })
    }
}

/// Store a parent and its child `child_id`, so the sub-agent tools accept
/// the child as the parent's.
fn store_parent_and_child(child_id: &str) {
    crate::database::save_session(&parent_record("parent-1", "parent")).unwrap();
    crate::database::save_subagent_session(
        &parent_record(child_id, "helper"),
        &mj_core::subagent::SubagentRecord {
            child_session_id: child_id.into(),
            parent_session_id: "parent-1".into(),
            task_name: "audit deps".into(),
            profile_id: "helper".into(),
            model: None,
            effort: None,
            working_directory: Default::default(),
            initial_prompt: "check the lockfile".into(),
            request_key: format!("request-{child_id}"),
            created_at: "2026-09-24T00:00:00Z".into(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: true,
        },
    )
    .unwrap();
}

/// A parent's backend over `exports` and a child actor scripted as given.
/// Returns the backend and the receiver of what reached the child's relay.
fn parking_backend(
    exports: Arc<ParkingExports>,
    failures: &[bool],
    on_failure: Arc<dyn Fn() + Send + Sync>,
) -> (
    Arc<ApiBackend>,
    mpsc::UnboundedReceiver<(String, RelayCommand)>,
) {
    parking_backend_with_protocol(
        exports,
        failures,
        on_failure,
        mj_core::relay::RELAY_PROTOCOL_VERSION,
    )
}

fn parking_backend_with_protocol(
    exports: Arc<ParkingExports>,
    failures: &[bool],
    on_failure: Arc<dyn Fn() + Send + Sync>,
    protocol: u32,
) -> (
    Arc<ApiBackend>,
    mpsc::UnboundedReceiver<(String, RelayCommand)>,
) {
    let (submitted, delivered) = mpsc::unbounded_channel();
    let mut view = ready_view("model");
    view.snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(protocol);
    let session = ScriptedSession {
        inner: FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 9,
            submitted,
            view: Some(view),
        },
        failures: Arc::new(std::sync::Mutex::new(failures.iter().copied().collect())),
        on_failure,
        reserved_syncs: Arc::default(),
    };
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(ScriptedControl(session)),
        running_states(),
        exports,
    ));
    (backend, delivered)
}

/// Like `parking_backend`, with the child's actor refusing its first
/// `reserved_syncs` syncs because a park still holds it.
fn reserved_parking_backend(
    exports: Arc<ParkingExports>,
    reserved_syncs: usize,
) -> (
    Arc<ApiBackend>,
    mpsc::UnboundedReceiver<(String, RelayCommand)>,
) {
    let (submitted, delivered) = mpsc::unbounded_channel();
    let session = ScriptedSession {
        inner: FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 9,
            submitted,
            view: Some(ready_view("model")),
        },
        failures: Arc::default(),
        on_failure: Arc::new(|| {}),
        reserved_syncs: Arc::new(std::sync::atomic::AtomicUsize::new(reserved_syncs)),
    };
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(ScriptedControl(session)),
        running_states(),
        exports,
    ));
    (backend, delivered)
}

#[tokio::test]
async fn send_message_to_parked_child_unparks_once_and_legacy_request_uses_the_same_route() {
    if !isolated_parked_test(
        "send_message_to_parked_child_unparks_once_and_legacy_request_uses_the_same_route",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::with_mailboxes_enabled(SessionState::Parked, None, true);
    let (backend, _) = parking_backend(exports.clone(), &[], Arc::new(|| {}));
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "first-request".into(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action: mj_core::subagent::SubagentToolAction::SendMessage {
            child_session_id: "child-1".into(),
            message: "keep going".into(),
        },
    };

    for _ in 0..2 {
        let answer = backend
            .execute_subagent_tool("parent-1".into(), request.clone())
            .await;
        assert!(!answer.is_error, "{}", answer.message);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&answer.message).unwrap()["status"],
            "queued"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&answer.message).unwrap()["via"],
            "mailbox"
        );
    }
    let pending = crate::database::pending_mailbox_events(10).unwrap();
    assert_eq!(pending.len(), 1, "a request retry inserts one outbox event");
    assert_eq!(pending[0].target_session_id, "child-1");
    assert!(pending[0].unpark);
    let event: mj_core::mailbox::MailboxEvent =
        serde_json::from_str(&pending[0].event_json).unwrap();
    assert_eq!(event.source, "parent");
    assert!(event.wake);
    assert_eq!(
        event.body,
        mj_core::mailbox::MailboxEventBody::ParentMessage {
            text: "keep going".into()
        }
    );
    assert_eq!(event.key, "subagent-message-first-request");

    let mut remote = crate::session_manager::spawn_remote_session_manager().unwrap();
    remote
        .targets
        .send_replace(vec![crate::session_manager::RelaySessionTarget {
            session_id: "child-1".into(),
            spec: crate::targets::CommandSpec::new("true", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        }]);
    let mut view = ready_view("model");
    let snapshot = view.snapshot.as_mut().unwrap();
    snapshot.materialized.session_id = "child-1".into();
    snapshot.operational.session_id = "child-1".into();
    remote
        .publisher
        .publish("child-1".into(), view)
        .await
        .unwrap();
    remote
        .control
        .wait_for_session("child-1", Duration::from_secs(5))
        .await
        .unwrap();

    let state: Arc<dyn ExportRuntime> = exports.clone();
    let sessions = remote.control.clone();
    let row = pending[0].clone();
    let delivery = tokio::spawn(async move {
        crate::mailbox_outbox::deliver_mailbox_event(
            state,
            &sessions,
            &row.target_session_id,
            event,
            row.unpark,
        )
        .await
    });
    let command = tokio::time::timeout(Duration::from_secs(2), remote.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::session_manager::RemoteSessionRequest::Submit {
        command_id,
        command,
        reply,
        ..
    } = command
    else {
        panic!("mailbox delivery must submit to the child relay")
    };
    assert_eq!(
        exports.unparks(),
        1,
        "the parked child starts before delivery"
    );
    assert_eq!(
        command_id,
        crate::mailbox_outbox::mailbox_command_id("subagent-message-first-request")
    );
    assert_eq!(
        command,
        RelayCommand::DeliverMailboxEvent {
            event: mj_core::mailbox::MailboxEvent {
                key: "subagent-message-first-request".into(),
                source: "parent".into(),
                wake: true,
                created_at_ms: request.created_at_ms.max(0) as u64,
                body: mj_core::mailbox::MailboxEventBody::ParentMessage {
                    text: "keep going".into(),
                },
            }
        }
    );
    reply.send(Ok(17)).unwrap();
    assert_eq!(
        delivery.await.unwrap().unwrap(),
        crate::mailbox_outbox::MailboxEventDelivery::Accepted(17)
    );
    crate::database::mark_mailbox_event_accepted(&pending[0].event_key, &command_id, 17).unwrap();

    let retried = backend
        .execute_subagent_tool("parent-1".into(), request.clone())
        .await;
    assert!(!retried.is_error, "{}", retried.message);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&retried.message).unwrap()["via"],
        "mailbox"
    );
    assert!(
        crate::database::pending_mailbox_events(10)
            .unwrap()
            .is_empty()
    );
    let messages = crate::database::subagent_mailbox_messages("parent-1").unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].accepted);
    assert_eq!(
        messages[0].accepted_command_id.as_deref(),
        Some(command_id.as_str())
    );

    // A daemon retry keeps the durable route even if mailbox settings change.
    let retry_exports = ParkingExports::with_mailboxes_enabled(SessionState::Parked, None, false);
    let (retry_backend, mut retry_delivered) = parking_backend(retry_exports, &[], Arc::new(|| {}));
    let retried = retry_backend
        .execute_subagent_tool("parent-1".into(), request)
        .await;
    assert!(!retried.is_error, "{}", retried.message);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&retried.message).unwrap()["via"],
        "mailbox"
    );
    assert!(retry_delivered.try_recv().is_err());
    assert_eq!(
        crate::database::subagent_mailbox_messages("parent-1")
            .unwrap()
            .len(),
        1
    );

    // A request recovered from an older parent queue still takes the same
    // mailbox route, retaining its existing request identity.
    let legacy: mj_core::subagent::SubagentToolRequest =
        serde_json::from_value(serde_json::json!({
            "originating_command_id":null,
            "request_id":"legacy-message-request",
            "created_at_ms":mj_core::clock::epoch_millis(),
            "action":{"action":"send_message","params":{
                "child_session_id":"child-1","message":"legacy follow-up"
            }}
        }))
        .unwrap();
    let legacy_answer = backend
        .execute_subagent_tool("parent-1".into(), legacy)
        .await;
    assert!(!legacy_answer.is_error, "{}", legacy_answer.message);
    let legacy_result: serde_json::Value = serde_json::from_str(&legacy_answer.message).unwrap();
    assert_eq!(legacy_result["via"], "mailbox");
    let pending = crate::database::pending_mailbox_events(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].event_key,
        "subagent-message-legacy-message-request"
    );

    let cached_alias = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "cached-input-alias".into(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action: mj_core::subagent::SubagentToolAction::SendInput {
            child_session_id: "child-1".into(),
            message: "follow-up from a cached tool list".into(),
        },
    };
    let alias_answer = backend
        .execute_subagent_tool("parent-1".into(), cached_alias)
        .await;
    assert!(!alias_answer.is_error, "{}", alias_answer.message);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&alias_answer.message).unwrap()["via"],
        "mailbox"
    );
    let pending = crate::database::pending_mailbox_events(10).unwrap();
    assert!(
        pending
            .iter()
            .any(|row| row.event_key == "subagent-message-cached-input-alias")
    );
    remote.shutdown.shutdown().await.unwrap();
}

// Hard-won: 193f015e: a stopped child was reported queued although it could never receive the message.
#[tokio::test]
async fn send_message_refuses_a_stopped_child_without_queueing() {
    if !isolated_parked_test("send_message_refuses_a_stopped_child_without_queueing") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::new(SessionState::Stopped, None);
    let (backend, _) = parking_backend(exports, &[], Arc::new(|| {}));
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "message-to-stopped-child".into(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action: mj_core::subagent::SubagentToolAction::SendMessage {
            child_session_id: "child-1".into(),
            message: "this cannot be delivered".into(),
        },
    };

    let answer = backend
        .execute_subagent_tool("parent-1".into(), request)
        .await;
    assert!(answer.is_error, "{}", answer.message);
    let result: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
    assert_eq!(result["status"], "failed");
    assert!(result["error"].as_str().unwrap().contains("Stopped"));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("queued message was not delivered")
    );
    assert!(
        crate::database::pending_mailbox_events(10)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn send_message_uses_a_turn_for_a_protocol_33_child() {
    if !isolated_parked_test("send_message_uses_a_turn_for_a_protocol_33_child") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::with_mailboxes_enabled(SessionState::Running, None, true);
    let (backend, mut delivered) = parking_backend_with_protocol(
        exports,
        &[],
        Arc::new(|| {}),
        mj_core::relay::RELAY_LEGACY_MAILBOX_PROTOCOL,
    );
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "message-to-old-worker".into(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action: mj_core::subagent::SubagentToolAction::SendMessage {
            child_session_id: "child-1".into(),
            message: "continue the task".into(),
        },
    };

    let answer = backend
        .execute_subagent_tool("parent-1".into(), request)
        .await;
    assert!(!answer.is_error, "{}", answer.message);
    let result: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
    assert_eq!(result["status"], "submitted");
    assert_eq!(result["via"], "turn");
    assert_eq!(
        delivered.try_recv().unwrap(),
        (
            "subagent-input-message-to-old-worker".into(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("continue the task"))],
            }
        )
    );
    assert!(
        crate::database::pending_mailbox_events(10)
            .unwrap()
            .is_empty(),
        "an older worker receives a queued turn instead of a mailbox row"
    );
}

#[tokio::test]
async fn send_message_uses_a_turn_when_mailboxes_are_disabled() {
    if !isolated_parked_test("send_message_uses_a_turn_when_mailboxes_are_disabled") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::with_mailboxes_enabled(SessionState::Running, None, false);
    let (backend, mut delivered) = parking_backend(exports, &[], Arc::new(|| {}));
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "mailboxes-disabled".into(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action: mj_core::subagent::SubagentToolAction::SendMessage {
            child_session_id: "child-1".into(),
            message: "continue the task".into(),
        },
    };

    let answer = backend
        .execute_subagent_tool("parent-1".into(), request)
        .await;
    assert!(!answer.is_error, "{}", answer.message);
    let result: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
    assert_eq!(result["status"], "submitted");
    assert_eq!(result["via"], "turn");
    assert!(matches!(
        delivered.try_recv().unwrap().1,
        RelayCommand::Prompt { .. }
    ));
    assert!(
        crate::database::pending_mailbox_events(10)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn outbox_fails_queued_parent_message_for_protocol_33_and_reports_it() {
    if !isolated_parked_test("outbox_fails_queued_parent_message_for_protocol_33_and_reports_it") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let event = mj_core::mailbox::MailboxEvent {
        key: "subagent-message-queued-before-worker-version".into(),
        source: "parent".into(),
        wake: true,
        created_at_ms: 123,
        body: mj_core::mailbox::MailboxEventBody::ParentMessage {
            text: "trusted parent message".into(),
        },
    };
    crate::database::enqueue_mailbox_event(
        &event.key,
        "child-1",
        &serde_json::to_string(&event).unwrap(),
        true,
        true,
    )
    .unwrap();

    let mut remote = crate::session_manager::spawn_remote_session_manager().unwrap();
    remote
        .targets
        .send_replace(vec![crate::session_manager::RelaySessionTarget {
            session_id: "child-1".into(),
            spec: crate::targets::CommandSpec::new("true", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        }]);
    let mut child_view = ready_view("model");
    let snapshot = child_view.snapshot.as_mut().unwrap();
    snapshot.materialized.session_id = "child-1".into();
    snapshot.operational.session_id = "child-1".into();
    snapshot.operational.relay_protocol_version =
        Some(mj_core::relay::RELAY_LEGACY_MAILBOX_PROTOCOL);
    remote
        .publisher
        .publish("child-1".into(), child_view)
        .await
        .unwrap();
    remote
        .control
        .wait_for_session("child-1", Duration::from_secs(5))
        .await
        .unwrap();

    let exports = ParkingExports::new(SessionState::Running, None);
    let exports_trait: Arc<dyn ExportRuntime> = exports.clone();
    let pending = crate::database::pending_mailbox_events(10).unwrap();
    crate::mailbox_outbox::deliver_session_events(
        exports_trait,
        remote.control.clone(),
        tokio_util::sync::CancellationToken::new(),
        "child-1".into(),
        pending,
    )
    .await;
    let reason = "This queued parent message reached an older mj worker that cannot receive structured mailbox events; it was not delivered.";
    assert!(
        crate::database::pending_mailbox_events(10)
            .unwrap()
            .is_empty()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), remote.requests.recv())
            .await
            .is_err(),
        "no legacy submit is allowed"
    );
    let messages = crate::database::subagent_mailbox_messages("parent-1").unwrap();
    assert_eq!(messages.len(), 1);
    assert!(!messages[0].accepted);
    assert_eq!(messages[0].failure.as_deref(), Some(reason));

    let (submitted, _) = mpsc::unbounded_channel();
    let mut parent_view = ready_view("model");
    parent_view
        .snapshot
        .as_mut()
        .unwrap()
        .materialized
        .session_id = "parent-1".into();
    let parent = FakeSession {
        session_id: "parent-1".into(),
        accepted_ordinal: 1,
        submitted,
        view: Some(parent_view),
    };
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(FakeControl(parent)),
        running_states(),
        exports,
    ));
    for (request_id, action) in [
        (
            "list-after-message-failure",
            mj_core::subagent::SubagentToolAction::ListAgents,
        ),
        (
            "wait-after-message-failure",
            mj_core::subagent::SubagentToolAction::WaitAgents,
        ),
    ] {
        let answer = backend
            .execute_subagent_tool(
                "parent-1".into(),
                mj_core::subagent::SubagentToolRequest {
                    originating_command_id: None,
                    request_id: request_id.into(),
                    created_at_ms: mj_core::clock::epoch_millis(),
                    action,
                },
            )
            .await;
        assert!(!answer.is_error, "{}", answer.message);
        let response: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
        let child = &response["agents"][0];
        assert_eq!(child["state"], "failed", "{response}");
        assert_eq!(
            child["message_deliveries"][0],
            serde_json::json!({
                "request_id":"queued-before-worker-version",
                "created_at_ms":123,
                "status":"failed",
                "via":"mailbox",
                "error":reason
            }),
            "{response}"
        );
        assert_eq!(child["input_deliveries"], child["message_deliveries"]);
    }
    remote.shutdown.shutdown().await.unwrap();
}

// Hard-won: 193f015e: pending parent messages for stopped children accumulated forever in the outbox.
#[tokio::test]
async fn outbox_prunes_pending_messages_for_stopped_children_only() {
    if !isolated_parked_test("outbox_prunes_pending_messages_for_stopped_children_only") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let mut stopped_child = parent_record("child-1", "helper");
    stopped_child.state = SessionState::Stopped;
    crate::database::save_session(&stopped_child).unwrap();
    let mut stopped_parent = parent_record("parent-1", "parent");
    stopped_parent.state = SessionState::Stopped;
    crate::database::save_session(&stopped_parent).unwrap();

    let enqueue = |key: &str, target: &str, source: &str, unpark: bool| {
        let event = mj_core::mailbox::MailboxEvent {
            key: key.into(),
            source: source.into(),
            wake: true,
            created_at_ms: 1,
            body: mj_core::mailbox::MailboxEventBody::PlainText {
                text: format!("event {key}"),
            },
        };
        crate::database::enqueue_mailbox_event(
            key,
            target,
            &serde_json::to_string(&event).unwrap(),
            true,
            unpark,
        )
        .unwrap();
    };
    enqueue("subagent-message-pending", "child-1", "parent", true);
    enqueue("subagent-message-delivered", "child-1", "parent", true);
    crate::database::mark_mailbox_event_accepted(
        "subagent-message-delivered",
        "mailbox-delivered",
        1,
    )
    .unwrap();
    enqueue("api:parent-1:keep-for-resume", "parent-1", "api", false);

    assert_eq!(
        crate::database::prune_pending_messages_for_stopped_children().unwrap(),
        1
    );
    let pending = crate::database::pending_mailbox_events(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_key, "api:parent-1:keep-for-resume");
    let history = crate::database::subagent_mailbox_messages("parent-1").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].event_key, "subagent-message-delivered");
    assert!(history[0].accepted);
}

async fn send_input(
    backend: &Arc<ApiBackend>,
    message: &str,
) -> mj_core::subagent::SubagentToolResult {
    backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
                request_id: "request".into(),
                created_at_ms: mj_core::clock::epoch_millis(),
                action: mj_core::subagent::SubagentToolAction::SendInput {
                    child_session_id: "child-1".into(),
                    message: message.into(),
                },
            },
        )
        .await
}

fn hold_child_start(_backend: &ApiBackend) -> String {
    let command_id = new_command_id("held-startup").unwrap();
    crate::database::enqueue_startup_deliveries(vec![crate::database::StartupDelivery {
        session_id: "child-1".into(),
        command_id: command_id.clone(),
        step_json: serde_json::to_string(&crate::daemon::StartupStep::ApiPrompt {
            text: "initial".into(),
        })
        .unwrap(),
        phase: "pending".into(),
        group_id: Some(command_id.clone()),
        accepted_ordinal: None,
        error: None,
    }])
    .unwrap();
    command_id
}

#[tokio::test]
async fn queued_input_waits_for_initial_prompt_when_mailboxes_are_disabled() {
    if !isolated_parked_test("queued_input_waits_for_initial_prompt_when_mailboxes_are_disabled") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::new(SessionState::Running, None);
    let (backend, mut delivered) = parking_backend(exports, &[], Arc::new(|| {}));
    let startup_id = hold_child_start(&backend);
    let mut pending = tokio::spawn({
        let backend = backend.clone();
        async move { send_input(&backend, "follow-up").await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    assert!(delivered.try_recv().is_err());
    assert!(
        !crate::upgrade::active_labels()
            .iter()
            .any(|label| label.starts_with("subagent input delivery"))
    );
    assert!(!pending.is_finished());
    let handle = backend.sessions.session("child-1").await.unwrap();
    let turn = submit_prompt(&handle, "initial".into()).await.unwrap();
    crate::database::set_startup_delivery_phase(&startup_id, "delivering", None).unwrap();
    crate::database::set_startup_delivery_accepted(&startup_id, Some(turn)).unwrap();
    crate::database::set_startup_delivery_phase(&startup_id, "done", None).unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(!answer.is_error, "{}", answer.message);
    let result: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
    assert_eq!(result["via"], "turn");
    assert_eq!(delivered_prompts(&mut delivered), ["initial", "follow-up"]);
}

#[tokio::test]
async fn queued_input_reports_startup_failure_and_never_delivers_after_close() {
    if !isolated_parked_test("queued_input_reports_startup_failure_and_never_delivers_after_close")
    {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    for closed in [false, true] {
        let exports = ParkingExports::new(SessionState::Running, None);
        let (backend, mut delivered) = parking_backend(exports.clone(), &[], Arc::new(|| {}));
        let startup_id = hold_child_start(&backend);
        let mut pending = tokio::spawn({
            let backend = backend.clone();
            async move { send_input(&backend, "must not run").await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut pending)
                .await
                .is_err()
        );
        if closed {
            exports.set_child_state(SessionState::Stopped);
        } else {
            crate::database::fail_startup_group("child-1", &startup_id, "provider login refused")
                .unwrap();
        }
        let answer = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(answer.is_error, "{}", answer.message);
        let value: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
        assert_eq!(value["child_session_id"], "child-1");
        assert_eq!(value["status"], "failed");
        assert!(value["error"].as_str().unwrap().contains(if closed {
            "Stopped"
        } else {
            "provider login refused"
        }));
        assert!(delivered.try_recv().is_err());
    }
}

/// I1-2: once a child's refused startup has also recorded it as failed, the
/// parent still reads the startup's cause from `wait` and `list_agents`, and
/// `send_input` is refused with it.
// Hard-won: ba6c3427: a refused first prompt left an idle live child while hiding the startup cause.
#[tokio::test]
async fn a_child_recorded_failed_after_a_refused_startup_reports_its_cause() {
    if !isolated_parked_test("a_child_recorded_failed_after_a_refused_startup_reports_its_cause") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let cause = "this agent does not offer high as a effort";
    let exports = ParkingExports::new(SessionState::Error, None);
    exports
        .records
        .lock()
        .unwrap()
        .get_mut("child-1")
        .unwrap()
        .last_error = Some(cause.into());
    let (backend, mut delivered) = parking_backend(exports.clone(), &[], Arc::new(|| {}));
    let startup_id = hold_child_start(&backend);
    crate::database::fail_startup_group("child-1", &startup_id, cause).unwrap();

    let answer = send_input(&backend, "must not run").await;
    assert!(answer.is_error, "{}", answer.message);
    let value: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
    assert_eq!(
        value["error"],
        format!("child startup failed: {cause}"),
        "{value}"
    );
    assert!(delivered.try_recv().is_err());
    assert_eq!(exports.unparks(), 0);

    let call = |action| {
        let backend = backend.clone();
        async move {
            let answer = backend
                .execute_subagent_tool(
                    "parent-1".into(),
                    mj_core::subagent::SubagentToolRequest {
                        originating_command_id: None,
                        request_id: "request".into(),
                        created_at_ms: mj_core::clock::epoch_millis(),
                        action,
                    },
                )
                .await;
            assert!(!answer.is_error, "{}", answer.message);
            serde_json::from_str::<serde_json::Value>(&answer.message).unwrap()
        }
    };
    let waited = call(mj_core::subagent::SubagentToolAction::WaitAgents).await;
    assert_eq!(waited["agents"][0]["state"], "error", "{waited}");
    assert_eq!(waited["agents"][0]["output"], cause, "{waited}");
    let listed = call(mj_core::subagent::SubagentToolAction::ListAgents).await;
    assert_eq!(listed["agents"][0]["state"], "error", "{listed}");
}

#[tokio::test]
async fn replayed_child_input_reuses_durable_acceptance_without_a_second_prompt() {
    if !isolated_parked_test(
        "replayed_child_input_reuses_durable_acceptance_without_a_second_prompt",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    // Recreate the backend each time, as daemon replacement does. There is no
    // live command ledger in this fake: the receipt must come from the store.
    for active in [true, false] {
        let mut conversation = mj_core::state::MaterializedSession::empty("child-1");
        conversation.applied_event_ordinal = 9;
        conversation.applied_event_digest = format!("{:064x}", 9);
        if active {
            conversation.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
            conversation.active_turn = Some(mj_core::state::MaterializedTurn {
                command_id: "subagent-input-request".into(),
                accepted_ordinal: Some(3),
                turn_start_position: 4,
                started_at_ms: 1,
                steered_into: None,
            });
        } else {
            conversation.last_turn_outcome = Some(finished_turn("subagent-input-request"));
        }
        crate::database::save_materialized_session(&conversation).unwrap();
        let exports = ParkingExports::new(SessionState::Running, None);
        let (backend, mut delivered) = parking_backend(exports, &[], Arc::new(|| {}));
        let answer = send_input(&backend, "already delivered").await;
        assert!(!answer.is_error, "{}", answer.message);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&answer.message).unwrap()["turn_id"],
            3
        );
        assert!(delivered.try_recv().is_err());
    }
    // A later turn replaces last_turn_outcome. The historical receipt must
    // still reconcile the input even without any retained worker ledger.
    for (ordinal, command) in [(10, "subagent-input-request"), (11, "newer-turn")] {
        crate::database::apply_projection_event(
            "child-1",
            ordinal,
            &format!("{:064x}", ordinal - 1),
            &format!("{ordinal:064x}"),
            &mj_core::storage::MaterializedSessionMutation {
                last_turn_outcome: Some(finished_turn(command)),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let (backend, mut delivered) = parking_backend(
        ParkingExports::new(SessionState::Parked, None),
        &[],
        Arc::new(|| {}),
    );
    let answer = send_input(&backend, "already completed and collected").await;
    assert!(!answer.is_error, "{}", answer.message);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer.message).unwrap()["turn_id"],
        3
    );
    assert!(delivered.try_recv().is_err());
}

#[test]
fn pending_child_inputs_hide_old_reports_and_delivery_failures_are_observable() {
    use super::subagent_input::InputProgress;
    let mut snapshot = ready_view("model").snapshot.unwrap();
    snapshot
        .subagent_requests
        .push(mj_core::subagent::SubagentToolRequest {
            originating_command_id: None,
            request_id: "input".into(),
            created_at_ms: 1,
            action: mj_core::subagent::SubagentToolAction::SendInput {
                child_session_id: "child".into(),
                message: "new work".into(),
            },
        });
    let old_report = || ("completed".into(), Some("old report".into()), true);
    let pending = InputProgress::from_snapshot(&snapshot);
    assert_eq!(
        pending.status("child", old_report()),
        ("running".into(), None, false)
    );
    let mut entry = serde_json::json!({});
    pending.annotate("child", &mut entry);
    assert_eq!(entry["pending_messages"], serde_json::json!(["input"]));
    assert_eq!(entry["pending_inputs"], entry["pending_messages"]);
    snapshot.subagent_requests.clear();
    snapshot.subagent_results.push(mj_core::subagent::SubagentToolResult {
        request_id: "input".into(), completed_at_ms: 2, is_error: true,
        message: serde_json::json!({"child_session_id":"child","status":"failed","created_at_ms":1,"error":"login refused"}).to_string(),
    });
    let failed = InputProgress::from_snapshot(&snapshot);
    assert_eq!(
        failed.status("child", old_report()),
        (
            "failed".into(),
            Some("Message input: login refused".into()),
            true
        )
    );
    failed.annotate("child", &mut entry);
    assert_eq!(entry["message_deliveries"][0]["error"], "login refused");
    assert_eq!(entry["message_deliveries"][0]["via"], "turn");
    assert_eq!(entry["input_deliveries"], entry["message_deliveries"]);
    snapshot.subagent_results.push(mj_core::subagent::SubagentToolResult {
        request_id: "later".into(), completed_at_ms: 4, is_error: false,
        message: serde_json::json!({"child_session_id":"child","status":"submitted","created_at_ms":3,"turn_id":12}).to_string(),
    });
    assert_eq!(
        InputProgress::from_snapshot(&snapshot).status("child", old_report()),
        old_report()
    );
}

#[tokio::test]
async fn queued_input_is_visible_through_wait_and_list_agents() {
    if !isolated_parked_test("queued_input_is_visible_through_wait_and_list_agents") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let mailbox_event = |request_id: &str, created_at_ms| mj_core::mailbox::MailboxEvent {
        key: format!("subagent-message-{request_id}"),
        source: "parent".into(),
        wake: true,
        created_at_ms,
        body: mj_core::mailbox::MailboxEventBody::ParentMessage {
            text: format!("message {request_id}"),
        },
    };
    for (request_id, accepted) in [("pending-message", false), ("delivered-message", true)] {
        let event = mailbox_event(request_id, if accepted { 2 } else { 1 });
        let key = event.key.clone();
        crate::database::enqueue_mailbox_event(
            &key,
            "child-1",
            &serde_json::to_string(&event).unwrap(),
            true,
            true,
        )
        .unwrap();
        if accepted {
            crate::database::mark_mailbox_event_accepted(&key, "mailbox-delivered", 12).unwrap();
        }
    }
    let mut conversation = mj_core::state::MaterializedSession::empty("child-1");
    conversation.applied_event_ordinal = 9;
    conversation.applied_event_digest = format!("{:064x}", 9);
    conversation.last_turn_outcome = Some(finished_turn("old-turn"));
    crate::database::save_materialized_session(&conversation).unwrap();
    crate::database::record_subagent_handback(
        "child-1",
        &mj_core::subagent::SubagentHandback {
            command_id: "old-turn".into(),
            message: "old result".into(),
            recorded_at_ms: 1,
        },
    )
    .unwrap();
    for failed in [false, true] {
        let mut parent = ready_view("model");
        let snapshot = parent.snapshot.as_mut().unwrap();
        if failed {
            snapshot.subagent_results.push(mj_core::subagent::SubagentToolResult {
                request_id: "follow-up".into(), completed_at_ms: 2, is_error: true,
                message: serde_json::json!({"child_session_id":"child-1","created_at_ms":1,"status":"failed","error":"restart refused"}).to_string(),
            });
        } else {
            snapshot
                .subagent_requests
                .push(mj_core::subagent::SubagentToolRequest {
                    originating_command_id: None,
                    request_id: "follow-up".into(),
                    created_at_ms: 1,
                    action: mj_core::subagent::SubagentToolAction::SendInput {
                        child_session_id: "child-1".into(),
                        message: "new work".into(),
                    },
                });
        }
        for request_id in ["pending-message", "delivered-message"] {
            snapshot
                .subagent_requests
                .push(mj_core::subagent::SubagentToolRequest {
                    originating_command_id: None,
                    request_id: request_id.into(),
                    created_at_ms: 1,
                    action: mj_core::subagent::SubagentToolAction::SendMessage {
                        child_session_id: "child-1".into(),
                        message: format!("message {request_id}"),
                    },
                });
        }
        let backend = Arc::new(ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "parent-1".into(),
                accepted_ordinal: 1,
                submitted: mpsc::unbounded_channel().0,
                view: Some(parent),
            })),
            running_states(),
            ParkingExports::new(SessionState::Parked, None),
        ));
        for wait in [false, true] {
            let action = if wait {
                mj_core::subagent::SubagentToolAction::WaitAgents
            } else {
                mj_core::subagent::SubagentToolAction::ListAgents
            };
            let answer = backend
                .execute_subagent_tool(
                    "parent-1".into(),
                    mj_core::subagent::SubagentToolRequest {
                        originating_command_id: None,
                        request_id: "observe".into(),
                        created_at_ms: 0,
                        action,
                    },
                )
                .await;
            assert!(!answer.is_error, "{}", answer.message);
            let value: serde_json::Value = serde_json::from_str(&answer.message).unwrap();
            let child = &value["agents"][0];
            // A pending waking input supersedes an older failed input and
            // keeps the parked child active for the queued delivery.
            assert_eq!(child["state"], "running");
            if failed {
                assert_eq!(child["message_deliveries"][0]["error"], "restart refused");
                assert_eq!(child["message_deliveries"][0]["via"], "turn");
                assert_eq!(
                    child["pending_messages"],
                    serde_json::json!(["pending-message"])
                );
            } else {
                assert_eq!(
                    child["pending_messages"],
                    serde_json::json!(["follow-up", "pending-message"])
                );
            }
            assert_eq!(
                child["message_deliveries"][if failed { 1 } else { 0 }],
                serde_json::json!({
                    "request_id":"delivered-message",
                    "created_at_ms":2,
                    "status":"delivered",
                    "via":"mailbox",
                    "command_id":"mailbox-delivered",
                    "accepted_ordinal":12
                })
            );
            assert_eq!(child["pending_inputs"], child["pending_messages"]);
            assert_eq!(child["input_deliveries"], child["message_deliveries"]);
            if wait {
                assert_eq!(
                    value["status"],
                    mj_core::subagent::WAIT_STATUS_STILL_RUNNING
                );
                assert_ne!(child["output"], "old result");
            }
        }
    }
}

#[tokio::test]
async fn legacy_child_interrupt_is_rejected_without_cancelling_a_turn() {
    if !isolated_parked_test("legacy_child_interrupt_is_rejected_without_cancelling_a_turn") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let mut view = ready_view("model");
    view.snapshot.as_mut().unwrap().materialized.active_turn =
        Some(mj_core::state::MaterializedTurn {
            command_id: "original-turn".into(),
            accepted_ordinal: Some(1),
            turn_start_position: 2,
            started_at_ms: 1,
            steered_into: None,
        });
    let (submitted, mut delivered) = mpsc::unbounded_channel();
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 3,
            submitted,
            view: Some(view),
        })),
        running_states(),
        ParkingExports::new(SessionState::Running, None),
    ));
    let answer = backend
        .execute_subagent_tool(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
                request_id: "legacy-interrupt".into(),
                created_at_ms: 1,
                action: mj_core::subagent::SubagentToolAction::LegacyInterruptAgent {
                    child_session_id: "child-1".into(),
                },
            },
        )
        .await;
    assert!(answer.is_error);
    assert!(answer.message.contains("use send_message"));
    assert!(delivered.try_recv().is_err());
}

#[tokio::test]
async fn recovered_legacy_interrupt_is_rejected_and_cached_without_cancelling_a_turn() {
    if !isolated_parked_test(
        "recovered_legacy_interrupt_is_rejected_and_cached_without_cancelling_a_turn",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "restart-legacy-interrupt".into(),
        created_at_ms: 1,
        action: mj_core::subagent::SubagentToolAction::LegacyInterruptAgent {
            child_session_id: "child-1".into(),
        },
    };
    crate::database::prepare_delegation(
        "parent-1".into(),
        crate::database::PreparedDelegation {
            request: request.clone(),
            turn_target: Some("original-turn".into()),
            spawn: None,
        },
    )
    .unwrap();
    crate::database::delegation_delivering("parent-1".into(), request.request_id.clone()).unwrap();
    let mut view = ready_view("model");
    view.snapshot.as_mut().unwrap().materialized.active_turn =
        Some(mj_core::state::MaterializedTurn {
            command_id: "newer-turn".into(),
            accepted_ordinal: Some(10),
            turn_start_position: 11,
            started_at_ms: 10,
            steered_into: None,
        });
    let (submitted, mut delivered) = mpsc::unbounded_channel();
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 12,
            submitted,
            view: Some(view),
        })),
        running_states(),
        ParkingExports::new(SessionState::Running, None),
    ));
    let result = backend
        .execute_subagent_tool_durable("parent-1".into(), request.clone())
        .await
        .unwrap();
    assert!(result.result.is_error);
    assert!(result.result.message.contains("use send_message"));
    assert!(delivered.try_recv().is_err());
    let cached = backend
        .execute_subagent_tool_durable("parent-1".into(), request)
        .await
        .unwrap();
    assert_eq!(cached.result.message, result.result.message);
    assert!(delivered.try_recv().is_err());
}

#[tokio::test]
async fn delayed_handback_uses_worker_origin_instead_of_current_turn() {
    if !isolated_parked_test("delayed_handback_uses_worker_origin_instead_of_current_turn") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let mut view = ready_view("model");
    view.snapshot.as_mut().unwrap().materialized.active_turn =
        Some(mj_core::state::MaterializedTurn {
            command_id: "newer-turn".into(),
            accepted_ordinal: Some(10),
            turn_start_position: 11,
            started_at_ms: 10,
            steered_into: None,
        });
    let (submitted, _) = mpsc::unbounded_channel();
    let backend = Arc::new(ApiBackend::new(
        SessionControl::new(FakeControl(FakeSession {
            session_id: "child-1".into(),
            accepted_ordinal: 12,
            submitted,
            view: Some(view),
        })),
        running_states(),
        ParkingExports::new(SessionState::Running, None),
    ));
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: Some("original-turn".into()),
        request_id: "delayed-report".into(),
        created_at_ms: 1,
        action: mj_core::subagent::SubagentToolAction::Handback {
            message: "original report".into(),
        },
    };
    let result = backend
        .execute_subagent_tool_durable("child-1".into(), request)
        .await
        .unwrap();
    assert!(!result.result.is_error, "{}", result.result.message);
    let report = crate::database::load_subagent_report("child-1").unwrap();
    assert_eq!(report.handback.unwrap().command_id, "original-turn");
}

/// The prompts that reached the child's relay, by text.
fn delivered_prompts(
    delivered: &mut mpsc::UnboundedReceiver<(String, RelayCommand)>,
) -> Vec<String> {
    std::iter::from_fn(|| delivered.try_recv().ok())
        .filter_map(|(_, command)| match command {
            RelayCommand::Prompt { prompt } => Some(
                prompt
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

const PARKED_TEST_CHILD: &str = "MJ_PARKED_SUBAGENT_TEST_CHILD";

/// Run the named test alone, with a store of its own. Returns whether this
/// process is that run.
fn isolated_parked_test(test: &str) -> bool {
    if std::env::var_os(PARKED_TEST_CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    crate::controller::test_support::IsolatedTest::new(crate::controller::test_support::test_name(
        module_path!(),
        test,
    ))
    .env(PARKED_TEST_CHILD, "1")
    .env("MJ_INSTANCE", "queued-subagent-input")
    .isolated_store(directory.path())
    .run();
    false
}

/// #1161: a child that handed back is parked, and the parent's next
/// `send_input` has to start it again before the prompt can run. A prompt
/// that a finishing park turned away is known not to have been delivered, so
/// it is sent again once; one that may have landed is never sent twice.
#[tokio::test]
async fn send_input_starts_a_parked_child_again_and_resends_only_a_prompt_a_park_turned_away() {
    if !isolated_parked_test(
        "send_input_starts_a_parked_child_again_and_resends_only_a_prompt_a_park_turned_away",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");

    // A parked child is started again, then given the prompt.
    let exports = ParkingExports::new(SessionState::Parked, None);
    let (backend, mut delivered) = parking_backend(exports.clone(), &[], Arc::new(|| {}));
    let answer = send_input(&backend, "check the tests too").await;
    assert!(!answer.is_error, "{}", answer.message);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer.message).unwrap()["turn_id"],
        9
    );
    assert_eq!(exports.unparks(), 1);
    assert_eq!(exports.child_state(), SessionState::Running);
    assert_eq!(delivered_prompts(&mut delivered), ["check the tests too"]);

    // A park finishes as the prompt arrives and turns it away: the child is
    // started again and the prompt delivered exactly once.
    let exports = ParkingExports::new(SessionState::Running, None);
    let parking = exports.clone();
    let (backend, mut delivered) = parking_backend(
        exports.clone(),
        &[false],
        Arc::new(move || parking.set_child_state(SessionState::Parked)),
    );
    let answer = send_input(&backend, "and the docs").await;
    assert!(!answer.is_error, "{}", answer.message);
    assert_eq!(exports.unparks(), 1, "the child was started again once");
    assert_eq!(exports.child_state(), SessionState::Running);
    assert_eq!(delivered_prompts(&mut delivered), ["and the docs"]);

    // A prompt that may have been delivered is reported, not sent again.
    let exports = ParkingExports::new(SessionState::Running, None);
    let parking = exports.clone();
    let (backend, mut delivered) = parking_backend(
        exports.clone(),
        &[true],
        Arc::new(move || parking.set_child_state(SessionState::Parked)),
    );
    let answer = send_input(&backend, "once only").await;
    assert!(answer.is_error, "{}", answer.message);
    assert_eq!(exports.unparks(), 0);
    assert!(delivered_prompts(&mut delivered).is_empty());
}

/// #1186: right after a handback the daemon parks the child, and the park
/// holds the child's actor. A `send_input` arriving then used to fail with
/// "session is reserved for a lifecycle operation", because a delivery
/// receipt lookup was refused while the park held the actor and was not
/// retried. The input must instead wait out the park and arrive exactly once.
// Hard-won: 7029361e: send_input failed during a park because its receipt lookup was not retried.
#[tokio::test]
async fn send_input_waits_out_a_park_that_holds_the_child() {
    if !isolated_parked_test("send_input_waits_out_a_park_that_holds_the_child") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::new(SessionState::Running, None);
    let (backend, mut delivered) = reserved_parking_backend(exports.clone(), 3);
    let answer = send_input(&backend, "one more thing").await;
    assert!(!answer.is_error, "{}", answer.message);
    assert_eq!(delivered_prompts(&mut delivered), ["one more thing"]);
}

/// A restart that fails leaves the child parked, so the parent can try
/// again, and the parent reads what failed.
#[tokio::test]
async fn a_failed_restart_leaves_the_child_parked_and_tells_the_parent_why() {
    if !isolated_parked_test("a_failed_restart_leaves_the_child_parked_and_tells_the_parent_why") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let exports = ParkingExports::new(
        SessionState::Parked,
        Some(
            "the parent's container ran out of process slots (pids.current 8192 of pids.max 8192)",
        ),
    );
    let (backend, mut delivered) = parking_backend(exports.clone(), &[], Arc::new(|| {}));

    let answer = send_input(&backend, "check the tests too").await;

    assert!(answer.is_error, "{}", answer.message);
    assert!(
        answer.message.contains("still parked") && answer.message.contains("retry"),
        "{}",
        answer.message
    );
    assert!(
        answer
            .message
            .contains("ran out of process slots (pids.current 8192 of pids.max 8192)"),
        "the cause reaches the parent: {}",
        answer.message
    );
    assert_eq!(exports.child_state(), SessionState::Parked);
    assert!(delivered_prompts(&mut delivered).is_empty());
}

fn record_finished_child(
    child_id: &str,
    command_id: &str,
    report: &str,
    start_position: u64,
    completed_ordinal: u64,
    accepted_ordinal: u64,
) {
    let mut conversation = mj_core::state::MaterializedSession::empty(child_id);
    conversation.applied_event_ordinal = completed_ordinal;
    conversation.applied_event_digest = format!("{completed_ordinal:064x}");
    let mut turn = finished_turn(command_id);
    turn.turn_start_position = Some(start_position);
    turn.completed_ordinal = completed_ordinal;
    turn.accepted_ordinal = Some(accepted_ordinal);
    conversation.last_turn_outcome = Some(turn);
    crate::database::save_materialized_session(&conversation).unwrap();
    assert!(
        crate::database::record_subagent_handback(
            child_id,
            &mj_core::subagent::SubagentHandback {
                command_id: command_id.into(),
                message: report.into(),
                recorded_at_ms: 1,
            },
        )
        .unwrap()
    );
}

fn add_child_relation(child_id: &str) {
    let session = parent_record(child_id, "helper");
    crate::database::save_subagent_session(
        &session,
        &mj_core::subagent::SubagentRecord {
            child_session_id: child_id.into(),
            parent_session_id: "parent-1".into(),
            task_name: format!("task {child_id}"),
            profile_id: "helper".into(),
            model: None,
            effort: None,
            working_directory: Default::default(),
            initial_prompt: "inspect".into(),
            request_key: format!("request-{child_id}"),
            created_at: "2026-09-24T00:00:00Z".into(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: true,
        },
    )
    .unwrap();
}

async fn durable_wait(backend: &Arc<ApiBackend>, request_id: &str) -> serde_json::Value {
    let answer = backend
        .execute_subagent_tool_durable(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
                request_id: request_id.into(),
                created_at_ms: mj_core::clock::epoch_millis(),
                action: mj_core::subagent::SubagentToolAction::WaitAgents,
            },
        )
        .await
        .unwrap();
    assert!(!answer.result.is_error, "{}", answer.result.message);
    serde_json::from_str(&answer.result.message).unwrap()
}

/// A wait commits its report marker with the durable result. Replaying the
/// same request returns that exact result, while a later request omits output.
#[tokio::test]
async fn wait_reports_a_finished_child_once_and_replay_preserves_the_answer() {
    if !isolated_parked_test("wait_reports_a_finished_child_once_and_replay_preserves_the_answer") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "The lockfile is current.", 3, 9, 7);
    let exports = ParkingExports::new(SessionState::Parked, None);
    let (backend, _delivered) = parking_backend(exports, &[], Arc::new(|| {}));

    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "first-wait".into(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action: mj_core::subagent::SubagentToolAction::WaitAgents,
    };
    let first_result = backend
        .execute_subagent_tool_durable("parent-1".into(), request.clone())
        .await
        .unwrap();
    let first: serde_json::Value = serde_json::from_str(&first_result.result.message).unwrap();
    assert_eq!(
        first["status"],
        mj_core::subagent::WAIT_STATUS_REPORTED,
        "{first}"
    );
    let agent = &first["agents"][0];
    assert_eq!(agent["state"], "completed", "{agent}");
    assert_eq!(agent["output"], "The lockfile is current.", "{agent}");
    assert_eq!(agent["parked"], true, "{agent}");
    assert!(
        crate::database::load_subagent("child-1")
            .unwrap()
            .unwrap()
            .reported_finish
            .is_some(),
        "the finish marker is durable with the answer"
    );

    let replay = backend
        .execute_subagent_tool_durable("parent-1".into(), request)
        .await
        .unwrap();
    assert_eq!(replay, first_result, "replay must not recompute the wait");

    let later = durable_wait(&backend, "second-wait").await;
    assert_eq!(
        later["status"],
        mj_core::subagent::WAIT_STATUS_NOTHING_TO_WAIT_FOR,
        "{later}"
    );
    assert_eq!(later["agents"][0]["child_session_id"], "child-1", "{later}");
    assert_eq!(
        later["agents"][0]["output"],
        serde_json::Value::Null,
        "{later}"
    );
}

#[tokio::test]
async fn an_undelivered_fallback_answer_restores_its_report_for_the_next_wait() {
    if !isolated_parked_test("an_undelivered_fallback_answer_restores_its_report_for_the_next_wait")
    {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "fallback report", 3, 9, 7);
    let (backend, _, _, _) = wait_prompt_backend(active_parent_view("working"));

    let first = backend
        .execute_subagent_tool_durable(
            "parent-1".into(),
            mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
                request_id: "fallback-wait".into(),
                created_at_ms: mj_core::clock::epoch_millis(),
                action: mj_core::subagent::SubagentToolAction::WaitAgents,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first.result.message).unwrap()["status"],
        mj_core::subagent::WAIT_STATUS_REPORTED
    );
    assert!(
        crate::database::load_subagent("child-1")
            .unwrap()
            .unwrap()
            .reported_finish
            .is_some()
    );

    let stored = crate::database::load_delegation("parent-1", "fallback-wait")
        .unwrap()
        .unwrap()
        .1
        .unwrap();
    assert_eq!(stored.reported_finishes.len(), 1);
    crate::database::unreport_delegation_finishes("parent-1".into(), stored.reported_finishes)
        .unwrap();
    assert!(
        crate::database::load_subagent("child-1")
            .unwrap()
            .unwrap()
            .reported_finish
            .is_none(),
        "a worker timeout fallback did not deliver the durable answer"
    );

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    let next = durable_wait(&backend, "fallback-wait-retry").await;
    assert_eq!(next["status"], mj_core::subagent::WAIT_STATUS_REPORTED);
    assert_eq!(next["agents"][0]["output"], "fallback report", "{next}");
}

#[tokio::test]
async fn renaming_a_terminally_failed_child_does_not_report_it_again() {
    if !isolated_parked_test("renaming_a_terminally_failed_child_does_not_report_it_again") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let (backend, exports, _, mut submitted) = wait_prompt_backend(active_parent_view("working"));
    {
        let mut records = exports.records.lock().unwrap();
        let child = records.get_mut("child-1").unwrap();
        child.state = SessionState::Error;
        child.last_error = Some("worker exited during startup".into());
        child.updated_at = "2026-10-06T12:00:00Z".into();
    }

    let first = durable_wait(&backend, "terminal-first").await;
    assert_eq!(first["status"], mj_core::subagent::WAIT_STATUS_REPORTED);
    assert_eq!(first["agents"][0]["output"], "worker exited during startup");
    assert!(matches!(
        crate::database::load_subagent("child-1")
            .unwrap()
            .unwrap()
            .reported_finish,
        Some(mj_core::subagent::SubagentFinishIdentity::Terminal {
            last_completed_ordinal: None,
            ..
        })
    ));

    {
        let mut records = exports.records.lock().unwrap();
        let child = records.get_mut("child-1").unwrap();
        child.session_title_override = Some("renamed helper".into());
        child.updated_at = "2026-10-06T13:00:00Z".into();
    }
    let second = durable_wait(&backend, "terminal-after-rename").await;
    assert_eq!(
        second["status"],
        mj_core::subagent::WAIT_STATUS_NOTHING_TO_WAIT_FOR,
        "mutable session metadata does not create a new finish"
    );
    assert_eq!(second["agents"][0]["output"], serde_json::Value::Null);
    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    assert!(submitted.try_recv().is_err());
}

/// A child resumed with send_input has a new turn span and can report again.
#[tokio::test]
async fn wait_reports_a_second_finish_after_send_input() {
    if !isolated_parked_test("wait_reports_a_second_finish_after_send_input") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "first report", 3, 9, 7);
    let exports = ParkingExports::new(SessionState::Parked, None);
    let (backend, mut delivered) = parking_backend(exports, &[], Arc::new(|| {}));
    let first = durable_wait(&backend, "first-wait").await;
    assert_eq!(
        first["status"],
        mj_core::subagent::WAIT_STATUS_REPORTED,
        "{first}"
    );

    let input = send_input(&backend, "one more thing").await;
    assert!(!input.is_error, "{}", input.message);
    let accepted_ordinal =
        serde_json::from_str::<serde_json::Value>(&input.message).unwrap()["turn_id"]
            .as_u64()
            .unwrap();
    assert_eq!(delivered_prompts(&mut delivered), ["one more thing"]);
    record_finished_child(
        "child-1",
        "task-2",
        "second report",
        13,
        19,
        accepted_ordinal,
    );
    let second = durable_wait(&backend, "second-wait").await;
    assert_eq!(
        second["status"],
        mj_core::subagent::WAIT_STATUS_REPORTED,
        "{second}"
    );
    assert_eq!(second["agents"][0]["output"], "second report", "{second}");
    assert_ne!(
        crate::database::load_subagent("child-1")
            .unwrap()
            .unwrap()
            .reported_finish,
        None
    );
}

/// A wait stays blocked while children run and answers as soon as any one
/// child finishes, with output only for that child.
#[tokio::test]
async fn wait_blocks_until_any_child_finishes_and_hides_other_output() {
    if !isolated_parked_test("wait_blocks_until_any_child_finishes_and_hides_other_output") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let mut conversation = mj_core::state::MaterializedSession::empty("child-1");
    conversation.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    conversation.applied_event_ordinal = 2;
    conversation.applied_event_digest = format!("{:064x}", 2);
    crate::database::save_materialized_session(&conversation).unwrap();
    let exports = ParkingExports::new(SessionState::Running, None);
    exports
        .records
        .lock()
        .unwrap()
        .insert("child-2".into(), parent_record("child-2", "helper"));
    let (backend, _delivered) = parking_backend(exports, &[], Arc::new(|| {}));
    let waiting = {
        let backend = backend.clone();
        tokio::spawn(async move { durable_wait(&backend, "blocking-wait").await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while waiting.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    );
    assert!(!waiting.is_finished(), "wait should block before a finish");

    add_child_relation("child-2");
    let mut conversation = mj_core::state::MaterializedSession::empty("child-2");
    conversation.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    conversation.applied_event_ordinal = 2;
    conversation.applied_event_digest = format!("{:064x}", 2);
    crate::database::save_materialized_session(&conversation).unwrap();
    record_finished_child("child-2", "task-2", "child two report", 13, 19, 12);
    let answer = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .expect("wait wakes on one child's finish")
        .unwrap();
    assert_eq!(
        answer["status"],
        mj_core::subagent::WAIT_STATUS_REPORTED,
        "{answer}"
    );
    let child_one = answer["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["child_session_id"] == "child-1")
        .unwrap();
    let child_two = answer["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["child_session_id"] == "child-2")
        .unwrap();
    assert_eq!(child_one["output"], serde_json::Value::Null, "{child_one}");
    assert_eq!(child_two["output"], "child two report", "{child_two}");
}

/// A wait with no child to watch answers immediately, and the reminder turn
/// remains a report even though it has no ordinary turn span.
/// I1-3 and I1-4: the reminder turn has no acceptance ordinal, but answers the
/// prompt it reminded about and must still be reported immediately.
// Hard-won: e1207055: a handback in a reminder turn had no acceptance ordinal so wait hung indefinitely.
#[tokio::test]
async fn wait_returns_nothing_to_wait_for_and_reports_reminder_turns() {
    if !isolated_parked_test("wait_returns_nothing_to_wait_for_and_reports_reminder_turns") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::save_session(&parent_record("parent-1", "parent")).unwrap();
    let exports = ParkingExports::new(SessionState::Parked, None);
    let (backend, _delivered) = parking_backend(exports.clone(), &[], Arc::new(|| {}));
    let started = std::time::Instant::now();
    let none = durable_wait(&backend, "empty-wait").await;
    assert_eq!(
        none["status"],
        mj_core::subagent::WAIT_STATUS_NOTHING_TO_WAIT_FOR,
        "{none}"
    );
    assert_eq!(none["agents"], serde_json::json!([]), "{none}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    store_parent_and_child("child-1");
    crate::database::record_subagent_prompt("child-1", 84).unwrap();
    let reminder = mj_core::subagent::handback_reminder_command_id(103);
    let mut conversation = mj_core::state::MaterializedSession::empty("child-1");
    conversation.applied_event_ordinal = 120;
    conversation.applied_event_digest = format!("{:064x}", 120);
    conversation.last_turn_outcome = Some(mj_core::state::MaterializedTurnOutcome {
        accepted_ordinal: None,
        turn_start_position: None,
        completed_ordinal: 118,
        ..finished_turn(&reminder)
    });
    crate::database::save_materialized_session(&conversation).unwrap();
    assert!(
        crate::database::record_subagent_handback(
            "child-1",
            &mj_core::subagent::SubagentHandback {
                command_id: reminder,
                message: "README.md contains 10 words and 6 lines.".into(),
                recorded_at_ms: 1,
            },
        )
        .unwrap()
    );
    let started = std::time::Instant::now();
    let waited = durable_wait(&backend, "reminder-wait").await;
    assert_eq!(
        waited["status"],
        mj_core::subagent::WAIT_STATUS_REPORTED,
        "{waited}"
    );
    assert_eq!(
        waited["agents"][0]["output"], "README.md contains 10 words and 6 lines.",
        "{waited}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    crate::database::record_subagent_prompt("child-1", 110).unwrap();
    let progress = load_child_progress("child-1").unwrap();
    assert!(progress.awaiting_prompt(None), "{progress:?}");
}

#[tokio::test]
async fn finished_child_queues_one_wait_prompt_for_an_idle_parent() {
    if !isolated_parked_test("finished_child_queues_one_wait_prompt_for_an_idle_parent") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "report", 3, 9, 7);
    let (backend, _, shared_view, mut submitted) = wait_prompt_backend(ready_view("model"));

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    let view = shared_view.lock().unwrap().clone();
    let snapshot = view.snapshot.unwrap();
    assert!(
        snapshot.materialized.active_turn.is_some(),
        "idle parent starts a turn"
    );
    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    assert_eq!(
        shared_view
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .materialized
            .queued_prompts
            .len(),
        1,
        "an active reminder does not suppress a queued duplicate"
    );
    assert_eq!(
        delivered_prompts(&mut submitted),
        [PARENT_WAIT_PROMPT_TEXT, PARENT_WAIT_PROMPT_TEXT]
    );
}

#[tokio::test]
async fn a_later_finish_queues_a_prompt_during_the_active_reminder_turn() {
    if !isolated_parked_test("a_later_finish_queues_a_prompt_during_the_active_reminder_turn") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-a", "report A", 3, 9, 7);
    let (backend, exports, shared_view, mut submitted) = wait_prompt_backend(ready_view("model"));

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    assert!(
        shared_view
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .materialized
            .active_turn
            .is_some(),
        "the first reminder starts the parent's turn"
    );
    assert_eq!(delivered_prompts(&mut submitted), [PARENT_WAIT_PROMPT_TEXT]);

    let collected = durable_wait(&backend, "collect-a").await;
    assert_eq!(collected["agents"][0]["output"], "report A", "{collected}");
    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();

    add_child_relation("child-b");
    let mut child_b = parent_record("child-b", "helper");
    child_b.state = SessionState::Parked;
    exports
        .records
        .lock()
        .unwrap()
        .insert("child-b".into(), child_b);
    record_finished_child("child-b", "task-b", "report B", 4, 12, 10);
    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();

    let view = shared_view.lock().unwrap().clone();
    assert_eq!(
        view.snapshot.unwrap().materialized.queued_prompts.len(),
        1,
        "B finishing after A was collected queues a reminder during the same turn"
    );
    assert_eq!(delivered_prompts(&mut submitted), [PARENT_WAIT_PROMPT_TEXT]);
}

#[tokio::test]
async fn parent_with_outstanding_wait_does_not_get_a_wait_prompt() {
    if !isolated_parked_test("parent_with_outstanding_wait_does_not_get_a_wait_prompt") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "report", 3, 9, 7);
    let mut view = active_parent_view("working");
    view.snapshot.as_mut().unwrap().subagent_requests.push(
        mj_core::subagent::SubagentToolRequest {
            originating_command_id: Some("parent-turn".into()),
            request_id: "pending-wait".into(),
            created_at_ms: 1,
            action: mj_core::subagent::SubagentToolAction::WaitAgents,
        },
    );
    let (backend, _, _, mut submitted) = wait_prompt_backend(view);

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();

    assert!(submitted.try_recv().is_err());
}

#[tokio::test]
async fn queued_wait_prompt_coalesces_and_wait_withdraws_it() {
    if !isolated_parked_test("queued_wait_prompt_coalesces_and_wait_withdraws_it") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "report", 3, 9, 7);
    let (backend, _, shared_view, mut submitted) =
        wait_prompt_backend(active_parent_view("working"));

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();
    assert_eq!(
        shared_view
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .materialized
            .queued_prompts
            .len(),
        1,
        "a busy parent keeps one queued reminder"
    );

    let answer = durable_wait(&backend, "wait-withdrawal").await;
    assert_eq!(answer["status"], mj_core::subagent::WAIT_STATUS_REPORTED);
    assert!(
        shared_view
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .materialized
            .queued_prompts
            .is_empty()
    );
    let commands = std::iter::from_fn(|| submitted.try_recv().ok())
        .map(|(_, command)| command)
        .collect::<Vec<_>>();
    assert!(commands.iter().any(|command| matches!(
        command,
        RelayCommand::Prompt { prompt }
            if prompt.iter().any(|block| matches!(
                block,
                ContentBlock::Text(text) if text.text == PARENT_WAIT_PROMPT_TEXT
            ))
    )));
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, RelayCommand::RemoveQueuedPrompt { .. }))
    );
}

#[tokio::test]
async fn closed_child_does_not_queue_a_wait_prompt() {
    if !isolated_parked_test("closed_child_does_not_queue_a_wait_prompt") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    record_finished_child("child-1", "task-1", "report", 3, 9, 7);
    let (backend, exports, _, mut submitted) = wait_prompt_backend(active_parent_view("working"));
    exports.set_child_state(SessionState::Stopped);

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();

    assert!(submitted.try_recv().is_err());
    let answer = durable_wait(&backend, "wait-after-close").await;
    assert_eq!(
        answer["status"],
        mj_core::subagent::WAIT_STATUS_NOTHING_TO_WAIT_FOR,
        "a child stopped by close is outside wait's scope even if it finished first"
    );
    assert_eq!(answer["agents"], serde_json::json!([]));
}

#[tokio::test]
async fn startup_failure_queues_a_wait_prompt() {
    if !isolated_parked_test("startup_failure_queues_a_wait_prompt") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    store_parent_and_child("child-1");
    let (backend, exports, _, mut submitted) = wait_prompt_backend(active_parent_view("working"));
    {
        let mut records = exports.records.lock().unwrap();
        let child = records.get_mut("child-1").unwrap();
        child.state = SessionState::Error;
        child.last_error = Some("worker exited during startup".into());
        child.updated_at = "2026-10-06T12:00:00Z".into();
    }

    backend.ensure_parent_wait_prompt("parent-1").await.unwrap();

    assert_eq!(delivered_prompts(&mut submitted), [PARENT_WAIT_PROMPT_TEXT]);
}

#[test]
fn startup_cleanup_is_unfinished_even_after_startup_delivery_failed() {
    let mut record = crate::controller::test_support::checkpoint_test_session("child");
    record.state = SessionState::StartupCleanup;
    record.last_error = Some("launch cancelled; cleanup failed: host unreachable".into());
    let status = StartStatus::Failed {
        message: "startup refused".into(),
    };
    let (state, output, finished) = subagent_status(
        Some(&record),
        None,
        Some(&status),
        None,
        false,
        &ChildProgress::settled(ReportState::Fallback),
    );
    assert_eq!(state, "stopping");
    assert_eq!(output, record.last_error);
    assert!(!finished);
}
