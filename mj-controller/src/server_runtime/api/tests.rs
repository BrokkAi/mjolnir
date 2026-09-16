use super::*;

use crate::server_runtime::profile_catalog::{ProfileCatalog, counting_probe, test_config};
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

#[test]
fn subagent_profiles_choose_the_most_remaining_quota_per_harness() {
    let candidates = vec![
        ("codex-low".into(), HarnessKind::Codex),
        ("claude-only".into(), HarnessKind::Claude),
        ("codex-high".into(), HarnessKind::Codex),
    ];
    let reports = BTreeMap::from([
        (
            "codex-low".into(),
            quota("codex-low", HarnessKind::Codex, &[80, 15]),
        ),
        (
            "codex-high".into(),
            quota("codex-high", HarnessKind::Codex, &[60, 55]),
        ),
    ]);

    assert_eq!(
        select_profile_per_harness(candidates, &reports),
        vec![
            ("codex-high".into(), HarnessKind::Codex),
            ("claude-only".into(), HarnessKind::Claude),
        ]
    );
}

#[test]
fn subagent_profile_selection_puts_unknown_quota_last_and_breaks_ties_by_id() {
    let candidates = vec![
        ("codex-unknown".into(), HarnessKind::Codex),
        ("codex-b".into(), HarnessKind::Codex),
        ("codex-a".into(), HarnessKind::Codex),
    ];
    let reports = BTreeMap::from([
        (
            "codex-a".into(),
            quota("codex-a", HarnessKind::Codex, &[50]),
        ),
        (
            "codex-b".into(),
            quota("codex-b", HarnessKind::Codex, &[50]),
        ),
    ]);

    assert_eq!(
        select_profile_per_harness(candidates, &reports),
        vec![("codex-a".into(), HarnessKind::Codex)]
    );
}

#[test]
fn failed_subagent_followup_is_terminal_error_with_its_cause() {
    let status = StartStatus::Failed {
        message: "model is unavailable".into(),
    };
    assert_eq!(
        subagent_status(None, None, Some(&status)),
        ("error".into(), Some("model is unavailable".into()), true)
    );
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
        goal: Default::default(),
        capacity_retry: None,
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
        mjolnir_subagents: None,
        create_managed_worktree: None,
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
        "the parent's own profile and the eligible one are offered, once per harness"
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

    backend
        .record_subagent_completion_notice(
            "parent-1".into(),
            "child-abcdef012345",
            "audit deps",
            3,
            "completed",
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
    assert!(
        text.contains("audit deps") && text.contains("finished turn 3"),
        "unexpected notice text: {text}"
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
