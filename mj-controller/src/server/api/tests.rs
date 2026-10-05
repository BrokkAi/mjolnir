use super::*;
use mj_core::state::{SessionResourceAllocation, TurnOutcomeKind};
use std::collections::BTreeSet;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use axum::http::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use http_body_util::BodyExt as _;
use mj_client::session::{
    ManagedSessionView, PendingRelaySubmit, PendingRelaySync, SessionHandleBackend,
};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt as _;

use super::super::{
    ControllerRequest, ServerOptions, ServerRequests, ViewerSnapshot, router,
    tests::sample_config_state,
};

fn error_event(seq: u64) -> crate::database::ApiEvent {
    crate::database::ApiEvent {
        seq,
        session_id: "session-1".into(),
        recorded_at_ms: 10,
        event: crate::database::ApiEventData::SessionFault {
            reason: mj_core::event_outcome::OutcomeReason::StartupFailed,
            message: "test failure".into(),
            command_id: None,
        },
    }
}

// Hard-won: b9a90454: dashboard-created sessions were published with their IDs as titles.
#[test]
fn a_session_nobody_has_named_is_published_by_its_creation_title_not_its_id() {
    // F-12: a dashboard-created session listed its hex id as its title.
    let (config, mut state) = sample_config_state();
    let record = state.sessions.get_mut("session-1").unwrap();
    record.session_title_override = None;
    record.acp_session_title = None;
    record.title = "proj via fake".into();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    assert_eq!(snapshot.sessions[0].title, "proj via fake");

    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .acp_session_title = Some("Fix the parser".into());
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    assert_eq!(snapshot.sessions[0].title, "Fix the parser");
}

// Hard-won: b9a90454: prompt --wait reported running from a stale session view.
#[tokio::test]
async fn a_finished_wait_does_not_report_the_session_still_running() {
    // F-12: `prompt --wait --json` answered with `chat_phase: running` because
    // the published view had not caught up with the live actor yet.
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let mut session = ApiSession::from(&snapshot.sessions[0]);
    session.chat_phase = crate::server::ViewerChatPhase::Running;
    let observation = WaitObservation {
        execution: MaterializedExecutionState::Idle,
        ..WaitObservation::default()
    };
    let backend: Arc<dyn SubagentBackend> = Arc::new(FakeBackend::default());

    let response = finish_wait(
        &backend,
        "session-1",
        session,
        observation,
        WaitDecision::simple(WaitOutcome::Finished, None),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        response.session.chat_phase,
        crate::server::ViewerChatPhase::Idle
    );
}

#[tokio::test]
async fn wait_reports_background_knowledge_without_claiming_checkpoint_readiness() {
    let root = tempfile::tempdir().unwrap();
    let relay = mj_worker::relay::DurableRelay::open(root.path(), "session-1", "1.0.0").unwrap();
    let materialized = mj_core::state::MaterializedSession::empty("session-1");
    let mut live = mj_client::session::ManagedSessionView {
        connected: true,
        error: None,
        snapshot: Some(mj_core::state::ManagedSessionSnapshot {
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational: relay.operational_state(),
            latest_credential_sync_signal: None,
            worker_build: None,
        }),
    };
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    // The runtime snapshot publishes this for an attached, idle session.
    let mut session = snapshot.sessions[0].clone();
    session.capabilities.prompt = true;
    let session = &session;
    let backend: Arc<dyn SubagentBackend> = Arc::new(FakeBackend::default());
    for known in [None, Some(false), Some(true)] {
        live.snapshot
            .as_mut()
            .unwrap()
            .operational
            .background_work_known = known;
        let observation = build_observation(&snapshot, session, Some(&live), None, None);
        let decision = resolve_wait(&observation, &WaitRequest::default()).unwrap();
        let response = finish_wait(
            &backend,
            &session.id,
            ApiSession::from(session),
            observation,
            decision,
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.session.background_work.unwrap().known, known);
    }
    live.snapshot
        .as_mut()
        .unwrap()
        .operational
        .background_commands
        .push(mj_core::relay::BackgroundCommand {
            id: "task-1".into(),
            started_at_ms: 1,
            command: "background agent".into(),
            can_stop: false,
        });
    let observation = build_observation(&snapshot, session, Some(&live), None, None);
    assert_eq!(observation.background_work.unwrap().tasks[0].id, "task-1");
    live.connected = false;
    assert!(
        build_observation(&snapshot, session, Some(&live), None, None)
            .background_work
            .is_none()
    );
}

#[tokio::test]
async fn event_stream_replays_then_follows_live_events_with_version_and_ids() {
    let backend = Arc::new(FakeBackend::default());
    backend
        .events
        .lock()
        .unwrap()
        .extend([error_event(1), error_event(2)]);
    let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
    let response = app
        .oneshot(
            bearer(Request::get(
                "/api/v1/events?session_id=session-1&workspace_id=default",
            ))
            .header("Last-Event-ID", "1")
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[API_VERSION_HEADER], API_VERSION);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
    assert!(text.contains("id: 2"), "{text}");
    assert!(text.contains("event: session_fault"), "{text}");
    backend.events.lock().unwrap().push(error_event(3));
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(frame.data_ref().unwrap())
            .unwrap()
            .contains("id: 3")
    );
    let queries = backend.event_queries.lock().unwrap();
    assert_eq!(queries[0].0.workspace_id.as_deref(), Some("default"));
    assert_eq!(queries[0].1, Some(1));
}

#[tokio::test]
async fn event_stream_rejects_an_unknown_session_instead_of_waiting_forever() {
    let backend = Arc::new(FakeBackend::default());
    let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
    let response = app
        .oneshot(
            bearer(Request::get(
                "/api/v1/events?session_id=session-that-never-existed",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(backend.event_queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn event_stream_slow_readers_do_not_block_requests_or_shutdown() {
    let backend = Arc::new(FakeBackend::default());
    backend.events.lock().unwrap().extend((1..=200).map(|seq| {
        let mut event = error_event(seq);
        event.event = crate::database::ApiEventData::SessionFault {
            reason: mj_core::event_outcome::OutcomeReason::StartupFailed,
            message: "x".repeat(8192),
            command_id: None,
        };
        event
    }));
    let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
    let stream = app
        .clone()
        .oneshot(
            bearer(Request::get("/api/v1/events?after_seq=0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Fill the bounded delivery channel while leaving the stream unread.
    tokio::task::yield_now().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        app.oneshot(
            bearer(Request::get("/api/v1/sessions"))
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    backend.shutdown.cancel();
    let body = tokio::time::timeout(Duration::from_secs(2), stream.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert!(
        body.len() < 200 * 8192,
        "shutdown must not drain the entire unread history"
    );
}

#[tokio::test]
async fn event_stream_without_cursor_starts_at_the_current_frontier() {
    let backend = Arc::new(FakeBackend::default());
    backend.events.lock().unwrap().push(error_event(1));
    let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    backend.events.lock().unwrap().push(error_event(2));
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(frame.data_ref().unwrap())
            .unwrap()
            .contains("id: 2")
    );
}

#[tokio::test]
async fn event_stream_rejects_bad_cursors_and_requires_authentication() {
    let backend = Arc::new(FakeBackend::default());
    backend.events.lock().unwrap().push(error_event(1));
    let (app, _actions, _snapshots, _bundles) = api_app(backend, |_| {});
    for (uri, header) in [
        ("/api/v1/events?after_seq=0", "1"),
        ("/api/v1/events", "invalid"),
        ("/api/v1/events?after_seq=2", "2"),
        ("/api/v1/events", "18446744073709551615"),
    ] {
        let response = app
            .clone()
            .oneshot(
                bearer(Request::get(uri))
                    .header("Last-Event-ID", header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{uri}, {header}"
        );
    }
    let response = app
        .oneshot(Request::get("/api/v1/events").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The workspaces the fake store holds.
///
/// It holds one by default, because the interesting case is the other one: an
/// instance with no workspace at all, which is what a fresh `mj -i <name>` is.
struct FakeWorkspaces(Mutex<Vec<mj_core::workspace::WorkspaceRecord>>);

impl FakeWorkspaces {
    fn empty() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    fn record(name: &str) -> mj_core::workspace::WorkspaceRecord {
        mj_core::workspace::WorkspaceRecord {
            id: format!("id-of-{name}"),
            name: name.to_owned(),
            created_at: "now".into(),
            last_opened_at: "now".into(),
            session_count: 0,
        }
    }
}

impl Default for FakeWorkspaces {
    fn default() -> Self {
        Self(Mutex::new(vec![Self::record("existing")]))
    }
}

/// A hand-written backend. Mocking the trait would only re-state its
/// signature; this returns the exact observations each test needs and
/// records what the handlers asked for.
/// The child id the fake backend reports for a spawn.
const SPAWNED_CHILD: &str = "spawned-child-1";

/// A live session actor that reports one view and accepts every command.
/// Hand-written rather than mocked so a handler test runs the real path from
/// the route through the backend to the session's own configuration.
#[derive(Clone)]
struct FakeSession {
    session_id: String,
    view: ManagedSessionView,
}

impl SessionHandleBackend for FakeSession {
    fn search_prompts(
        &self,
        _bundle_id: String,
        _scope: mj_core::storage::HistoryScope,
        _query: String,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::storage::PromptHistoryEntry>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn review_state(&self) -> BoxFuture<'_, AnyResult<mj_client::session::ReviewState>> {
        Box::pin(async { Ok(Default::default()) })
    }
    fn config_result(
        &self,
        _command_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<Option<String>>>> {
        Box::pin(async { Ok(Some(None)) })
    }
    fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
        Box::new(self.clone())
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn view(&self) -> ManagedSessionView {
        self.view.clone()
    }
    fn is_stopped(&self) -> bool {
        false
    }
    fn has_changed(&self) -> AnyResult<bool> {
        Ok(false)
    }
    fn changed(&mut self) -> BoxFuture<'_, AnyResult<ManagedSessionView>> {
        Box::pin(std::future::pending())
    }
    fn enqueue_submit(
        &self,
        _command_id: String,
        _command: mj_core::relay::RelayCommand,
    ) -> BoxFuture<'_, AnyResult<PendingRelaySubmit>> {
        Box::pin(async { Ok(PendingRelaySubmit::new(Box::pin(async { Ok(1) }))) })
    }
    fn enqueue_sync(&self) -> BoxFuture<'_, AnyResult<PendingRelaySync>> {
        Box::pin(async { Ok(PendingRelaySync::new(Box::pin(async { Ok(()) }))) })
    }
    fn respond_elicitation(
        &self,
        _elicitation_id: String,
        _response: mj_core::elicitation::ElicitationResponse,
    ) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop_background_task(&self, _background_task_id: String) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn reviewer(
        &self,
        _role: Option<String>,
        _action: mj_client::session::ReviewerAction,
    ) -> BoxFuture<'_, AnyResult<mj_client::session::ReviewerOutcome>> {
        Box::pin(async { anyhow::bail!("no reviewer in this fake") })
    }
}

#[derive(Default)]
struct FakeBackend {
    workspaces: FakeWorkspaces,
    /// Successive answers to `turn_state`, newest last. The final entry
    /// repeats once exhausted, so a wait loop settles rather than spinning.
    turn_states: Mutex<Vec<Option<TurnState>>>,
    prompt_ordinal: u64,
    prompts: Mutex<Vec<(String, String)>>,
    summary: Option<TurnSummary>,
    /// Follow-ups the start handler asked for.
    followups: Mutex<Vec<(String, StartFollowup)>>,
    /// A custom quota-ranked candidate set for new-session model selection.
    model_candidates: Option<SubagentCandidates>,
    start_status: Option<StartStatus>,
    /// The page and the limit the transcript handler asked for.
    transcript: Mutex<Option<TranscriptPage>>,
    transcript_limits: Mutex<Vec<usize>>,
    history: Option<mj_core::storage::TranscriptHistoryPage>,
    history_cursors: Mutex<Vec<Option<mj_core::storage::TranscriptCursor>>>,
    /// Export answers. `None` stands for a refusal, which is what an
    /// export that cannot be produced looks like to a handler.
    diff: Option<String>,
    diff_options: Mutex<Vec<DiffOptions>>,
    file: Option<Vec<u8>>,
    pushed: Option<PushedBranch>,
    bundle: Option<BundleExport>,
    /// When set, the diff fails outright rather than being refused.
    diff_fails: bool,
    bundle_fails: bool,
    target_access_missing: bool,
    /// The path the file handler asked the backend for.
    file_paths: Mutex<Vec<PathBuf>>,
    file_writes: Mutex<Vec<(PathBuf, Vec<u8>, bool)>>,
    events: Mutex<Vec<crate::database::ApiEvent>>,
    /// What the live session actor reports, when this fake has one. The
    /// controller snapshot deliberately disagrees with it in the tests that
    /// set this, which is the lag a configuration change has to survive.
    live_view: Option<ManagedSessionView>,
    shutdown: tokio_util::sync::CancellationToken,
    /// This server's own upgrade admission, so a test can close it without
    /// closing the process-wide gate every other test shares.
    upgrade_gate: Arc<crate::upgrade::Gate>,
    event_queries: Mutex<Vec<(crate::database::ApiEventFilter, Option<u64>)>>,
    /// Sub-agent children that have handed back their reports.
    handed_back: BTreeSet<String>,
    /// How often `turn_state` read the store.
    turn_state_reads: std::sync::atomic::AtomicUsize,
    startup_reads: std::sync::atomic::AtomicUsize,
    report_reads: std::sync::atomic::AtomicUsize,
    wait_revision: std::sync::atomic::AtomicU64,
    /// What `subagent_report` answers; tests change it as a report lands.
    child_report: Mutex<Option<(bool, mj_core::subagent::SubagentReport)>>,
}

impl FakeBackend {
    fn next_turn_state(&self) -> Option<TurnState> {
        let mut states = self.turn_states.lock().unwrap();
        if states.len() > 1 {
            states.remove(0)
        } else {
            states.first().cloned().flatten()
        }
    }
}

impl SubagentBackend for FakeBackend {
    fn wait_revision(&self, _: &str) -> AnyResult<Option<u64>> {
        Ok(Some(
            self.wait_revision.load(std::sync::atomic::Ordering::SeqCst),
        ))
    }

    fn transcript_history(
        &self,
        _session_id: String,
        before: Option<mj_core::storage::TranscriptCursor>,
    ) -> BoxFuture<'_, AnyResult<mj_core::storage::TranscriptHistoryPage>> {
        Box::pin(async move {
            self.history_cursors.lock().unwrap().push(before);
            self.history
                .clone()
                .ok_or_else(|| anyhow::anyhow!("history unavailable"))
        })
    }
    fn events(
        &self,
        filter: crate::database::ApiEventFilter,
        after_seq: Option<u64>,
    ) -> BoxFuture<'_, AnyResult<crate::database::ApiEventPage>> {
        Box::pin(async move {
            self.event_queries
                .lock()
                .unwrap()
                .push((filter.clone(), after_seq));
            let events = self.events.lock().unwrap();
            let latest_seq = events.last().map_or(0, |e| e.seq);
            let cursor = after_seq.unwrap_or(latest_seq);
            let page: Vec<_> = events
                .iter()
                .filter(|e| {
                    e.seq > cursor
                        && filter
                            .session_id
                            .as_ref()
                            .is_none_or(|id| id == &e.session_id)
                })
                .take(200)
                .cloned()
                .collect();
            Ok(crate::database::ApiEventPage {
                next_after_seq: page.last().map_or(latest_seq.max(cursor), |e| e.seq),
                latest_seq,
                events: page,
            })
        })
    }

    fn profile_config(
        &self,
        _profile: String,
        _model: Option<String>,
        _refresh: bool,
    ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
        Box::pin(async {
            Ok(mj_core::worker_launch::ProfileConfig {
                model: Some("kimi-code/k3".into()),
                models: vec![mj_core::acp::SessionConfigChoice {
                    value: "kimi-code/k3".into(),
                    name: "K3".into(),
                    description: None,
                }],
                efforts: vec![mj_core::acp::SessionConfigChoice {
                    value: "high".into(),
                    name: "High".into(),
                    description: None,
                }],
                observed_at: 1,
            })
        })
    }

    /// The parent's own profile, offering what `profile_config` reports.
    fn subagent_candidates(
        &self,
        parent_profile: String,
    ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        let model_candidates = self.model_candidates.clone();
        Box::pin(async move {
            if let Some(candidates) = model_candidates {
                return Ok(candidates);
            }
            let choices = self
                .profile_config(parent_profile.clone(), None, false)
                .await?;
            Ok(SubagentCandidates {
                offered: vec![SubagentCandidate {
                    profile_id: parent_profile,
                    harness: mj_core::config::HarnessKind::Codex,
                    choices,
                    remaining_percent: None,
                }],
                unavailable: Vec::new(),
            })
        })
    }

    fn session_handle(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<SessionHandle>>> {
        let view = self.live_view.clone();
        Box::pin(async move {
            Ok(view.map(|view| SessionHandle::new(FakeSession { session_id, view })))
        })
    }
    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, AnyResult<u64>> {
        Box::pin(async move {
            self.prompts.lock().unwrap().push((session_id, text));
            Ok(self.prompt_ordinal)
        })
    }
    fn turn_state(&self, _session_id: String) -> BoxFuture<'_, AnyResult<Option<TurnState>>> {
        self.turn_state_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(self.next_turn_state()) })
    }
    fn subagent_report(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<(bool, mj_core::subagent::SubagentReport)>>> {
        self.report_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(self.child_report.lock().unwrap().clone()) })
    }
    fn subagent_handed_back(&self, child_session_id: String) -> BoxFuture<'_, AnyResult<bool>> {
        Box::pin(async move { Ok(self.handed_back.contains(&child_session_id)) })
    }
    fn turn_summary(
        &self,
        _session_id: String,
        _turn: TurnSpan,
    ) -> BoxFuture<'_, AnyResult<TurnSummary>> {
        Box::pin(async {
            self.summary
                .clone()
                .context("this fake has no turn summary")
        })
    }
    fn start_subagent(
        &self,
        request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'_, AnyResult<mj_core::subagent::SubagentRecord>> {
        Box::pin(async move {
            Ok(mj_core::subagent::SubagentRecord {
                child_session_id: SPAWNED_CHILD.to_owned(),
                parent_session_id: request.parent_session_id,
                task_name: request.task_name,
                profile_id: request.profile_id,
                model: request.model,
                effort: request.effort,
                working_directory: request.working_directory,
                initial_prompt: request.initial_prompt,
                request_key: request.request_key,
                created_at: "2026-09-18T00:00:00Z".to_owned(),
                noticed_turn: None,
                handback_tool: false,
            })
        })
    }
    fn start_followup(
        &self,
        session_id: String,
        followup: StartFollowup,
    ) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async move {
            self.followups.lock().unwrap().push((session_id, followup));
            Ok(())
        })
    }
    fn start_status(&self, _session_id: String) -> BoxFuture<'_, AnyResult<Option<StartStatus>>> {
        self.startup_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(self.start_status.clone()) })
    }
    fn transcript(
        &self,
        _session_id: String,
        _after_seq: u64,
        limit: usize,
        _role: Option<mj_core::transcript::TranscriptRole>,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>> {
        Box::pin(async move {
            self.transcript_limits.lock().unwrap().push(limit);
            Ok(self.transcript.lock().unwrap().clone())
        })
    }
    fn diff(
        &self,
        _session_id: String,
        options: DiffOptions,
    ) -> BoxFuture<'_, Result<String, ExportError>> {
        Box::pin(async move {
            self.diff_options.lock().unwrap().push(options);
            if self.diff_fails {
                return Err(ExportError::Failed(anyhow::anyhow!("git exploded")));
            }
            self.diff
                .clone()
                .ok_or_else(|| ExportError::Refused("this session has no live target".into()))
        })
    }
    fn read_file(
        &self,
        _session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>> {
        Box::pin(async move {
            self.file_paths.lock().unwrap().push(path);
            self.file
                .clone()
                .ok_or_else(|| ExportError::Refused("this session has no live target".into()))
        })
    }
    fn write_file(
        &self,
        _session_id: String,
        path: PathBuf,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> BoxFuture<'_, Result<(), ExportError>> {
        Box::pin(async move {
            self.file_writes
                .lock()
                .unwrap()
                .push((path, bytes, overwrite));
            Ok(())
        })
    }
    fn push_branch(
        &self,
        _session_id: String,
        branch: String,
    ) -> BoxFuture<'_, Result<PushedBranch, ExportError>> {
        Box::pin(async move {
            if self.target_access_missing {
                return Err(missing_target_access_error());
            }
            self.pushed
                .clone()
                .map(|pushed| PushedBranch { branch, ..pushed })
                .ok_or_else(|| ExportError::Refused("this session is running a turn".into()))
        })
    }
    fn list_workspaces(
        &self,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::workspace::WorkspaceRecord>>> {
        Box::pin(async { Ok(self.workspaces.0.lock().unwrap().clone()) })
    }
    fn create_workspace(
        &self,
        name: String,
    ) -> BoxFuture<'_, AnyResult<mj_core::workspace::WorkspaceRecord>> {
        Box::pin(async move {
            let mut workspaces = self.workspaces.0.lock().unwrap();
            if let Some(existing) = workspaces
                .iter()
                .find(|workspace| workspace.name.eq_ignore_ascii_case(&name))
            {
                return Ok(existing.clone());
            }
            let created = FakeWorkspaces::record(&name);
            workspaces.push(created.clone());
            Ok(created)
        })
    }
    fn bundle(&self, _session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>> {
        Box::pin(async {
            if self.target_access_missing {
                return Err(missing_target_access_error());
            }
            if self.bundle_fails {
                return Err(ExportError::Failed(anyhow::anyhow!(
                    "checkpoint storage failed"
                )));
            }
            self.bundle
                .clone()
                .ok_or_else(|| ExportError::Refused("no commits beyond the session base".into()))
        })
    }
}

/// Returns the snapshot sender alongside the router: dropping it closes the
/// watch channel, which the wait loop correctly treats as the controller
/// going away.
/// The choices a running session offers, as a live view the fake reports.
fn live_view(model: &str, efforts: &[&str]) -> ManagedSessionView {
    let materialized = mj_core::state::MaterializedSession::empty("session-1");
    let mut operational =
        mj_core::relay::RelaySnapshot::new("session-1".into()).operational_state();
    operational.config_options = serde_json::from_value(serde_json::json!([
        {"id": "model", "name": "Model", "category": "model", "type": "select",
         "currentValue": model,
         "options": [{"value": "slow", "name": "Slow"}, {"value": "flash", "name": "Flash"}]},
        {"id": "thinking", "name": "Effort", "category": "thought_level", "type": "select",
         "currentValue": efforts[0],
         "options": efforts.iter().map(|value| serde_json::json!({"value": value, "name": value})).collect::<Vec<_>>()},
    ]))
    .expect("the fixture describes selects the schema accepts");
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

// Hard-won: d01553c0: successful configuration changes disappeared while the session snapshot lagged.
#[tokio::test]
async fn session_detail_keeps_the_setter_configuration_while_the_snapshot_lags() {
    let backend = Arc::new(FakeBackend {
        live_view: Some(live_view("flash", &["max", "high"])),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |snapshot| {
        snapshot.sessions[0].capabilities.set_config = true;
        snapshot.sessions[0].config_options = crate::server::session_config_view(
            mj_core::config::HarnessKind::Codex,
            &live_view("slow", &["high"]).snapshot.unwrap().operational,
        );
    });

    let response = app
        .clone()
        .oneshot(
            bearer(Request::patch("/api/v1/sessions/session-1/config"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"effort","value":"max"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let applied = json_body(response).await;

    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/sessions/session-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail = json_body(response).await;
    let options = detail["config_options"].as_array().unwrap();
    for (key, current) in [("model", "flash"), ("effort", "max")] {
        let option = options.iter().find(|option| option["key"] == key).unwrap();
        assert_eq!(option["current"], current);
    }
    assert_eq!(detail["config_options"], applied["config_options"]);
}

/// A model change replaces the effort catalogue at once, while the controller
/// snapshot still carries the previous model's choices for a while. Validating
/// the next change against the snapshot refused an effort the session does
/// offer (#1091).
// Hard-won: 410836fe: a valid effort was rejected using stale choices after a model change.
#[tokio::test]
async fn an_effort_the_live_session_offers_is_accepted_while_the_snapshot_still_lags() {
    let backend = Arc::new(FakeBackend {
        live_view: Some(live_view("flash", &["low", "high", "max"])),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |snapshot| {
        snapshot.sessions[0].capabilities.set_config = true;
        // What the previous model offered, which is all the snapshot knows.
        snapshot.sessions[0].config_options = vec![super::super::ViewerConfigOption {
            key: "effort".into(),
            label: "effort".into(),
            current: Some("low".into()),
            choices: ["low", "high"]
                .into_iter()
                .map(|value| super::super::ViewerConfigChoice {
                    value: value.into(),
                    name: value.into(),
                    description: None,
                })
                .collect(),
        }];
    });

    let response = app
        .clone()
        .oneshot(
            bearer(Request::patch("/api/v1/sessions/session-1/config"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"effort","value":"max"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the value the model just selected offers must not be refused"
    );
    let body = json_body(response).await;
    let effort = body["config_options"]
        .as_array()
        .expect("the answer lists the session's options")
        .iter()
        .find(|option| option["key"] == "effort")
        .expect("effort is one of them")
        .clone();
    assert_eq!(
        effort["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["low", "high", "max"],
        "the answer reports the live choices, not the snapshot's"
    );

    // Neither list offers this one, so the refusal stands.
    let response = app
        .oneshot(
            bearer(Request::patch("/api/v1/sessions/session-1/config"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"effort","value":"extreme"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

fn api_app(
    backend: Arc<FakeBackend>,
    adjust: impl FnOnce(&mut ViewerSnapshot),
) -> (
    axum::Router,
    mpsc::Receiver<ControllerRequest>,
    watch::Sender<ViewerSnapshot>,
    mpsc::Receiver<super::super::BundleRequest>,
) {
    api_app_with_preferences(backend, adjust, absent_preferences_path())
}

/// A path that cannot hold fast-start preferences, so a test that does not
/// name one never reads the developer's real `go.json`. The directory does not
/// exist, which is the ordinary "nothing saved yet" case.
fn absent_preferences_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir()
        .join(format!(
            "mjolnir-api-options-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
        .join("go.json")
}

fn api_app_with_preferences(
    backend: Arc<FakeBackend>,
    adjust: impl FnOnce(&mut ViewerSnapshot),
    preferences_path: PathBuf,
) -> (
    axum::Router,
    mpsc::Receiver<ControllerRequest>,
    watch::Sender<ViewerSnapshot>,
    mpsc::Receiver<super::super::BundleRequest>,
) {
    // Every local engine answers, so a test's availability does not depend on
    // the engines the machine running it has.
    api_app_with_engines(
        backend,
        adjust,
        preferences_path,
        Arc::new(|kind| {
            matches!(kind, "local-podman" | "local-docker" | "apple-container")
                .then_some(crate::controller::LocalEngineReadiness::Ready)
        }),
    )
}

fn api_app_with_engines(
    backend: Arc<FakeBackend>,
    adjust: impl FnOnce(&mut ViewerSnapshot),
    preferences_path: PathBuf,
    engine_probe: super::EngineProbe,
) -> (
    axum::Router,
    mpsc::Receiver<ControllerRequest>,
    watch::Sender<ViewerSnapshot>,
    mpsc::Receiver<super::super::BundleRequest>,
) {
    api_app_with_worker_check(backend, adjust, preferences_path, engine_probe, None)
}

fn api_app_with_worker_check(
    backend: Arc<FakeBackend>,
    adjust: impl FnOnce(&mut ViewerSnapshot),
    preferences_path: PathBuf,
    engine_probe: super::EngineProbe,
    worker_check: Option<crate::server::WorkerSourceCheck>,
) -> (
    axum::Router,
    mpsc::Receiver<ControllerRequest>,
    watch::Sender<ViewerSnapshot>,
    mpsc::Receiver<super::super::BundleRequest>,
) {
    let (config, state) = sample_config_state();
    // The sample record carries a recorded error. It is left in place: a
    // session-scoped error must not answer a wait about one turn, so every
    // wait test below runs against a session that is carrying one.
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    // The fake viewer represents an attached idle worker unless overridden.
    snapshot.sessions[0].activity_state = Some(mj_core::activity::ActivityState::default());
    adjust(&mut snapshot);
    let (snapshot_tx, snapshot_rx) = watch::channel(snapshot);
    let (_conversation_tx, conversation_rx) =
        watch::channel(mj_core::snapshot_map::SnapshotMap::new());
    let (action_tx, action_rx) = mpsc::channel(8);
    let (bundle_tx, bundle_rx) = mpsc::channel(8);
    let (receipt_tx, _receipt_rx) = mpsc::channel(8);
    let (preflight_tx, _preflight_rx) = mpsc::channel(8);
    let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(8);
    let (client_state_tx, _client_state_rx) = mpsc::channel(8);
    let mut options = ServerOptions::new(
        "127.0.0.1:0".parse().unwrap(),
        snapshot_rx,
        conversation_rx,
        ServerRequests {
            action_tx,
            bundle_tx,
            receipt_tx,
            preflight_tx,
            move_preparation_tx,
            client_state_tx,
        },
    )
    .unwrap()
    .with_test_credentials("123456", b"01234567890123456789012345678901");
    options.shutdown = backend.shutdown.clone();
    options.set_upgrade_gate(backend.upgrade_gate.clone());
    options.set_subagent_backend(backend);
    options.set_preferences_path(preferences_path);
    options.set_engine_probe(engine_probe);
    if let Some(check) = worker_check {
        options.set_worker_source_check(check);
    }
    (router(options), action_rx, snapshot_tx, bundle_rx)
}

fn bearer(request: axum::http::request::Builder) -> axum::http::request::Builder {
    request.header(AUTHORIZATION, "Bearer test-api-token")
}

async fn login_cookie(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::post("/auth/session")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"code":"123456"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    response
        .headers()
        .get(SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn json_body(response: Response) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn the_api_refuses_an_unauthenticated_caller_and_still_names_its_version() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get(API_VERSION_HEADER).unwrap(),
        API_VERSION,
        "a client must be able to tell a wrong token from a wrong server"
    );
    assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-store");

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/sessions")
                .header(AUTHORIZATION, "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn closing_supersedes_a_failed_initial_configuration() {
    let observation = WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Suspending),
        start_status: Some(StartStatus::Failed {
            message: "bad model".into(),
        }),
        ..Default::default()
    };
    assert_eq!(
        resolve_wait(&observation, &WaitRequest::default())
            .unwrap()
            .outcome,
        WaitOutcome::Stopped
    );
}

#[tokio::test]
async fn either_the_bearer_token_or_the_viewer_cookie_lists_sessions() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});
    let cookie = login_cookie(&app).await;

    for request in [
        bearer(Request::get("/api/v1/sessions")),
        Request::get("/api/v1/sessions").header(COOKIE, cookie),
    ] {
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(API_VERSION_HEADER).unwrap(),
            API_VERSION
        );
        let body = json_body(response).await;
        assert_eq!(body["sessions"][0]["id"], "session-1");
    }
}

#[tokio::test]
async fn one_session_is_readable_by_id_and_an_unknown_one_is_not_found() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});

    let response = app
        .clone()
        .oneshot(
            bearer(Request::get("/api/v1/sessions/session-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["id"], "session-1");

    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/sessions/session-9"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A session that has been provisioned and whose worker has not attached
/// yet, which is what every session is for the seconds after it is created.
fn waiting_for_its_worker(snapshot: &mut ViewerSnapshot) {
    let session = &mut snapshot.sessions[0];
    session.state = "disconnected".into();
    session.lifecycle = ViewerLifecycleCategory::Live;
    session.has_error = false;
    session.capabilities.prompt = false;
}

// Hard-won: a583fa10: new sessions returned prompt conflicts until their worker attached.
#[tokio::test]
async fn a_prompt_to_a_session_still_starting_is_taken_once_its_worker_attaches() {
    // F-4: a new session refused prompts with 409 for the twenty seconds its
    // worker took to attach, so every caller needed its own retry loop.
    let backend = Arc::new(FakeBackend {
        prompt_ordinal: 3,
        ..FakeBackend::default()
    });
    let (app, _actions, snapshot_tx, _bundles) = api_app(backend.clone(), waiting_for_its_worker);
    let request = tokio::spawn(
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"first words"}"#))
                .unwrap(),
        ),
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // The handshake marks the record running a moment before the worker's
    // first report makes the session promptable.
    snapshot_tx.send_modify(|snapshot| snapshot.sessions[0].state = "running".into());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !request.is_finished() && backend.prompts.lock().unwrap().is_empty(),
        "nothing is submitted or refused before the worker attaches"
    );

    snapshot_tx.send_modify(|snapshot| snapshot.sessions[0].capabilities.prompt = true);
    let response = request.await.unwrap().unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(json_body(response).await["turn_id"], 3);
}

// Hard-won: 5dafd135: a withdrawn startup prompt was still submitted after the worker attached.
#[tokio::test]
async fn an_interrupt_withdraws_a_prompt_held_for_a_starting_session() {
    // R2-1: an ACP cancel sent while the prompt was held reached
    // interrupt-turn, which refused it because there was no turn yet. The
    // held prompt then became a turn that ran to completion.
    let backend = Arc::new(FakeBackend {
        prompt_ordinal: 3,
        ..FakeBackend::default()
    });
    let (app, _actions, snapshot_tx, _bundles) = api_app(backend.clone(), waiting_for_its_worker);
    let held = tokio::spawn(
        app.clone().oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"first words"}"#))
                .unwrap(),
        ),
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let interrupted = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/interrupt-turn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        interrupted.status(),
        StatusCode::ACCEPTED,
        "withdrawing a held prompt is what interrupting a starting session does"
    );
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), held)
        .await
        .expect("the held prompt answers once it is withdrawn")
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await;
    assert!(
        body.to_string().contains("interrupted"),
        "the refusal says why: {body}"
    );

    // The session coming up later must not revive the withdrawn prompt.
    snapshot_tx.send_modify(|snapshot| snapshot.sessions[0].capabilities.prompt = true);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(backend.prompts.lock().unwrap().is_empty());
}

/// Launch finding R5-8: a prompt to a session in the `error` state was
/// refused with "this session cannot take a prompt right now", which reads
/// as "try again". It now says the session failed, why, and the two ways
/// out.
// Hard-won: 109d508d: failed sessions told prompt callers to try again without a remedy.
#[tokio::test]
async fn a_prompt_to_a_failed_session_says_it_failed_and_how_to_go_on() {
    let backend = Arc::new(FakeBackend::default());
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |snapshot| {
        let session = &mut snapshot.sessions[0];
        session.state = "error".into();
        session.lifecycle = ViewerLifecycleCategory::Failed;
        session.has_error = true;
        session.launch_error = Some("worker bootstrap failed: operation cancelled".into());
        // A checkpoint from an earlier suspension is what makes resume a
        // way out (launch finding R6-1).
        session.has_checkpoint = true;
        session.capabilities.prompt = false;
        session.capabilities.resume = true;
        session.capabilities.destroy = true;
    });
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"are you there"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await.to_string();
    assert!(!body.contains("right now"), "{body}");
    assert!(body.contains("this session failed"), "{body}");
    assert!(
        body.contains("worker bootstrap failed: operation cancelled"),
        "{body}"
    );
    assert!(body.contains("mj resume --session session-1"), "{body}");
    assert!(body.contains("mj destroy --session session-1"), "{body}");
    assert!(backend.prompts.lock().unwrap().is_empty());
}

/// Launch finding R6-1: a launch that failed before it saved a checkpoint was
/// refused with advice to `mj resume` it, and that resume then failed in the
/// background with "session has no checkpoint". The refusal names only what
/// can work: destroying the session.
// Hard-won: 78245c3a: resume advice accepted a session with no checkpoint and then failed silently.
#[tokio::test]
async fn a_prompt_to_a_session_that_failed_before_its_first_checkpoint_offers_only_destroy() {
    let backend = Arc::new(FakeBackend::default());
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |snapshot| {
        // The fixture record has no checkpoint, like a launch that failed
        // before its first one. The capabilities are what the phone
        // projection publishes for a failed session with no operation.
        let session = &mut snapshot.sessions[0];
        session.state = "error".into();
        session.lifecycle = ViewerLifecycleCategory::Failed;
        session.has_error = true;
        session.launch_error = Some("worker bootstrap failed: operation cancelled".into());
        session.capabilities.prompt = false;
        session.capabilities.resume = true;
        session.capabilities.destroy = true;
    });
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"are you there"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await.to_string();
    assert!(!body.contains("mj resume"), "{body}");
    assert!(body.contains("before it saved a checkpoint"), "{body}");
    assert!(body.contains("mj destroy --session session-1"), "{body}");
    assert!(
        body.contains("worker bootstrap failed: operation cancelled"),
        "{body}"
    );
    assert!(backend.prompts.lock().unwrap().is_empty());
}

fn start_body(extra: &str) -> String {
    format!(r#"{{"profile_id":"codex-1","target_id":"podman","bundle_id":"hel"{extra}}}"#)
}

fn start_request(body: String) -> Request<Body> {
    bearer(Request::post("/api/v1/sessions"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

// Hard-won: d91504f8: API-created container sessions had no CPU or memory limits.
#[tokio::test]
async fn start_returns_the_created_session_and_hands_its_prompt_to_the_followup() {
    let backend = Arc::new(FakeBackend::default());
    let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

    let response = tokio::spawn(app.oneshot(start_request(start_body(
        r#","prompt":"add a README line""#,
    ))));
    let request = actions.recv().await.unwrap();
    assert_eq!(
        request.action,
        ControllerAction::New {
            review: None,
            at: None,
            branch: None,
            base: None,
            subagents: None,
            create_managed_worktree: None,
            workspace_id: String::new(),
            profile_id: "codex-1".into(),
            bundle_id: "hel".into(),
            target_id: "podman".into(),
            // A container session is never created without a limit.
            resource_allocation: Some(Box::new(SessionResourceAllocation::Container {
                cpus: mj_core::state::BASELINE_CONTAINER_CPUS,
                memory_bytes: mj_core::state::BASELINE_CONTAINER_MEMORY_BYTES,
            })),
            title: None,
            project_directory: None,
            dirty_ack: Vec::new(),
        }
    );
    request
        .reply
        .send(ActionOutcome::Accepted {
            session_id: Some("session-2".into()),
        })
        .unwrap();

    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(json_body(response).await["session_id"], "session-2");
    let followups = backend.followups.lock().unwrap();
    assert_eq!(followups.len(), 1);
    assert_eq!(followups[0].0, "session-2");
    assert_eq!(
        followups[0].1.prompt.as_deref(),
        Some("add a README line"),
        "the first prompt is the backend's to submit once the harness is ready"
    );
}

// Hard-won: 00325fa0: API start refusals dropped running and limit counts.
#[tokio::test]
async fn start_names_the_pool_counts_when_the_daemon_is_at_its_action_limit() {
    let backend = Arc::new(FakeBackend::default());
    let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

    let response = tokio::spawn(app.oneshot(start_request(start_body(""))));
    let request = actions.recv().await.unwrap();
    request
        .reply
        .send(ActionOutcome::Busy {
            running: 4,
            limit: 4,
        })
        .unwrap();

    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = json_body(response).await;
    assert_eq!(body["running_actions"], 4, "{body}");
    assert_eq!(body["action_limit"], 4, "{body}");
}

/// The unavailable-model refusal reaches the client with its code, so each
/// client can add the remedy that fits it.
// Hard-won: 9fe54d8e: API refusals directed browser callers to CLI-only flags.
#[tokio::test]
async fn start_answers_an_unavailable_subagent_model_with_a_code() {
    let backend = Arc::new(FakeBackend::default());
    let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});
    let body = start_body(r#","subagents":{"mode":"single_model","model":"fake-model"}"#);
    let response = tokio::spawn(app.oneshot(start_request(body)));
    let request = actions.recv().await.unwrap();
    request
        .reply
        .send(ActionOutcome::Refused(
            mj_core::refusal::Refusal::unusable(
                "Selected subagent model \"fake-model\" is unavailable.",
            )
            .with_code(mj_core::subagent::CHOICE_UNAVAILABLE_CODE),
        ))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = json_body(response).await;
    assert_eq!(error["code"], mj_core::subagent::CHOICE_UNAVAILABLE_CODE);
    assert!(error["error"].as_str().unwrap().contains("fake-model"));
}

// Hard-won: d91504f8: API-created container sessions had no CPU or memory limits.
#[tokio::test]
async fn start_sizes_a_container_session_from_the_host_default_and_overrides() {
    let host = mj_core::state::HostContainerSize {
        cpus: 16,
        memory_bytes: 64 << 30,
    };
    let backend = Arc::new(FakeBackend::default());
    let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend, |snapshot| {
        let target = snapshot
            .targets
            .iter_mut()
            .find(|target| target.id == "podman")
            .unwrap();
        target.container_host_limits = Some(host);
        target.default_resource_allocation = Some(SessionResourceAllocation::Container {
            cpus: 12,
            memory_bytes: 48 << 30,
        });
    });

    // One override keeps the default for the other dimension.
    let response = tokio::spawn(
        app.clone()
            .oneshot(start_request(start_body(r#","cpus":4"#))),
    );
    let request = actions.recv().await.unwrap();
    let ControllerAction::New {
        resource_allocation,
        ..
    } = &request.action
    else {
        panic!("expected a New action, got {:?}", request.action);
    };
    assert_eq!(
        resource_allocation.as_deref(),
        Some(&SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 48 << 30,
        })
    );
    request
        .reply
        .send(ActionOutcome::Accepted {
            session_id: Some("session-2".into()),
        })
        .unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::CREATED
    );

    // A size the host cannot give is refused before anything is created.
    let response = app
        .clone()
        .oneshot(start_request(start_body(&format!(
            r#","memory_bytes":{}"#,
            host.memory_bytes + 1
        ))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A bare target has no container size to override.
    let response = app
        .oneshot(start_request(
            r#"{"profile_id":"codex-1","target_id":"raw","project_directory":"/repo","cpus":4}"#
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await.to_string();
    assert!(body.contains("not a container target"), "{body}");
    assert!(
        actions.try_recv().is_err(),
        "no refused start reaches the controller"
    );
}

/// Launch finding R2-4: `mj workspaces create default` answered `500
/// Internal Server Error` with a sentence about sessions made before a
/// workspace was required. The name is the caller's mistake, so it is refused
/// as other unusable names are, in words a new user can act on.
// Hard-won: 26ae8304: a reserved workspace name returned 500 for caller error.
#[tokio::test]
async fn the_reserved_workspace_name_is_refused_as_the_callers_mistake() {
    let backend = Arc::new(FakeBackend {
        workspaces: FakeWorkspaces::empty(),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/workspaces"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":" Default "}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await.to_string();
    assert!(
        body.contains("the workspace name \\\"default\\\" is reserved"),
        "{body}"
    );
    // Launch finding R3-11: the refusal gave no reason. It says what the
    // name is kept for.
    assert!(
        body.contains("sessions made before Mjolnir had workspaces"),
        "{body}"
    );
    assert!(body.contains("choose another name"), "{body}");
    assert!(backend.workspaces.0.lock().unwrap().is_empty());
}

// Hard-won: 5bf8e5cf: a client key could resurrect a session after failed close.
#[tokio::test]
async fn start_rejects_a_request_that_still_sends_an_idempotency_key() {
    let backend = Arc::new(FakeBackend::default());
    let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

    let response = app
        .oneshot(start_request(start_body(r#","idempotency_key":"key-1""#)))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the field is gone, so the body no longer parses"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("idempotency_key"),
        "the refusal must name the field it did not expect: {body}"
    );
    assert!(
        actions.try_recv().is_err(),
        "a request that does not parse must not reach the controller"
    );
    assert!(backend.followups.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_session_with_no_projection_row_has_no_transcript() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});
    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/sessions/session-1/transcript"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn suspend_refuses_an_unverified_clone_from_the_published_snapshot_until_acknowledged() {
    let (app, mut actions, _snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), |snapshot| {
            snapshot.sessions[0].publication_state =
                Some(mj_core::state::PublicationState::Unknown);
        });

    let response = app
        .clone()
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/suspend"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(actions.try_recv().is_err());

    let response = tokio::spawn(
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/suspend"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"acknowledge_unpublished_work":true}"#))
                .unwrap(),
        ),
    );
    let request = actions.recv().await.unwrap();
    assert_eq!(
        request.action,
        ControllerAction::Suspend {
            session_id: "session-1".into(),
            acknowledge_unpublished_work: true,
        }
    );
    request
        .reply
        .send(super::super::ActionOutcome::accepted())
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

/// A snapshot whose session-1 has one running sub-agent, child-1, and nothing
/// to publish.
fn with_a_running_subagent(snapshot: &mut ViewerSnapshot) {
    let mut child = snapshot.sessions[0].clone();
    child.id = "child-1".into();
    child.state = "running".into();
    child.subagent_session_ids.clear();
    snapshot.sessions[0].subagent_session_ids = vec!["child-1".into()];
    snapshot.sessions[0].publication_state = None;
    snapshot.sessions.push(child);
}

#[tokio::test]
async fn a_forced_close_ignores_active_subagents() {
    let (app, mut actions, _snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), with_a_running_subagent);
    let response = tokio::spawn(
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/destroy"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        ),
    );
    let request = actions.recv().await.unwrap();
    assert_eq!(
        request.action,
        ControllerAction::Destroy {
            session_id: "session-1".into(),
            delete_branch: false,
        }
    );
    request
        .reply
        .send(super::super::ActionOutcome::accepted())
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

// Hard-won: 472aff0b: destroy deleted user branches without a way to keep them.
#[tokio::test]
async fn a_forced_close_asks_to_delete_the_branch_only_when_the_body_does() {
    let (app, mut actions, _snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), |_| {});

    for (body, delete_branch) in [(r#"{}"#, false), (r#"{"delete_branch":true}"#, true)] {
        let response = tokio::spawn(
            app.clone().oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/destroy"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            ),
        );
        let request = actions.recv().await.unwrap();
        assert_eq!(
            request.action,
            ControllerAction::Destroy {
                session_id: "session-1".into(),
                delete_branch,
            }
        );
        request
            .reply
            .send(super::super::ActionOutcome::accepted())
            .unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }
}

#[test]
fn a_force_close_is_not_wire_representable() {
    // The browser viewer posts this enum to `/actions`, so a wire request
    // must not be able to ask for the destructive variant.
    assert!(
        serde_json::from_str::<ControllerAction>(r#"{"action":"destroy","session_id":"s"}"#)
            .is_err()
    );
}

#[tokio::test]
async fn cancel_turn_is_refused_when_there_is_no_turn_to_cancel() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/interrupt-turn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

/// A wait for turn N ends when turn N or a later one has ended, and answers
/// about the latest one, since only its outcome is kept. The answer says which
/// turn it describes and which one was asked about (RVA-2).
#[tokio::test(start_paused = true)]
async fn wait_for_an_earlier_turn_names_the_later_turn_it_answers_about() {
    let backend = Arc::new(FakeBackend {
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Idle,
            active_turn: None,
            last_turn_outcome: Some(MaterializedTurnOutcome {
                diagnostic: None,
                usage: None,
                command_id: "prompt-2".into(),
                accepted_ordinal: Some(8),
                turn_start_position: Some(9),
                completed_ordinal: 12,
                completed_at_ms: 900,
                outcome: TurnOutcomeKind::Completed {
                    stop_reason: "end_turn".into(),
                },
            }),
        })]),
        summary: Some(TurnSummary {
            turn_number: 2,
            turn_started_at_ms: 100,
            last_changed_at_ms: 900,
            final_message: Some("the later reply".into()),
            tool_calls: 0,
        }),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/wait"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"turn_id":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(response).await;
    assert_eq!(body["outcome"], "finished");
    assert_eq!(body["turn_id"], 8);
    assert_eq!(body["requested_turn_id"], 5);
    assert_eq!(body["final_message"], "the later reply");
}

#[tokio::test(start_paused = true)]
async fn wait_preserves_quota_diagnostic_without_scheduling_retry() {
    let diagnostic = mj_core::diagnostic::TurnDiagnostic::from_provider(&serde_json::json!({
        "code":"provider.auth_error", "message":"Five-hour usage limit exceeded; resets at 23:00 UTC.",
        "details":{"statusCode":403,"resetAt":"23:00 UTC"}
    })).unwrap();
    let mut turn = completed(5, "QuotaLimit");
    turn.diagnostic = Some(diagnostic.clone());
    let backend = Arc::new(FakeBackend {
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Idle,
            active_turn: None,
            last_turn_outcome: Some(turn),
        })]),
        summary: Some(TurnSummary {
            turn_number: 1,
            turn_started_at_ms: 100,
            last_changed_at_ms: 500,
            final_message: None,
            tool_calls: 0,
        }),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/wait"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"turn_id":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["outcome"], "quota_limit");
    assert_eq!(body["message"], diagnostic.message);
    assert_eq!(body["diagnostic"]["http_status"], 403);
    assert_eq!(body["diagnostic"]["reset_at"], "23:00 UTC");
    assert_eq!(body["session"]["last_turn_diagnostic"], body["diagnostic"]);
    assert!(body["capacity_retry"].is_null());
    assert!(
        body["session"]["last_turn_outcome"]
            .get("diagnostic")
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn wait_reports_a_timeout_rather_than_guessing_at_a_running_turn() {
    let backend = Arc::new(FakeBackend {
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Running { started_at_ms: 10 },
            active_turn: Some(MaterializedTurn {
                command_id: "prompt-1".into(),
                accepted_ordinal: Some(5),
                turn_start_position: 6,
                started_at_ms: 10,
                steered_into: None,
            }),
            last_turn_outcome: None,
        })]),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/wait"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"turn_id":5,"timeout_secs":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["outcome"], "timeout");
    assert_eq!(body["turn_id"], 5);
}

/// A wait in progress when the daemon shuts down answers at once. An upgrade
/// handoff marks the answer so the client asks the next daemon; an explicit
/// stop does not, so a stopped daemon is not started again by its waiters.
// Hard-won: c87e5e88: wait clients lost handoff behavior during live daemon replacement.
#[tokio::test]
async fn a_wait_ended_by_an_upgrade_handoff_tells_its_client_to_ask_the_next_daemon() {
    for handoff in [true, false] {
        let backend = Arc::new(FakeBackend {
            turn_states: Mutex::new(vec![Some(TurnState {
                execution: MaterializedExecutionState::Running { started_at_ms: 10 },
                active_turn: Some(MaterializedTurn {
                    command_id: "prompt-1".into(),
                    accepted_ordinal: Some(5),
                    turn_start_position: 6,
                    started_at_ms: 10,
                    steered_into: None,
                }),
                last_turn_outcome: None,
            })]),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});
        let waiting = tokio::spawn(
            app.oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/wait"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"turn_id":5,"timeout_secs":3600}"#))
                    .unwrap(),
            ),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "the turn is still running");
        if handoff {
            assert!(
                backend.upgrade_gate.try_close(),
                "a waiting request does not hold the handoff"
            );
        }
        backend.shutdown.cancel();
        let response = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("shutdown ends the wait at once")
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let marked = response
            .headers()
            .get(crate::server::UPGRADE_HEADER)
            .cloned();
        let body = json_body(response).await;
        if handoff {
            assert_eq!(marked.unwrap(), "pending");
            assert_eq!(body["code"], DAEMON_HANDOFF_CODE);
        } else {
            assert!(marked.is_none(), "an explicit stop is not a handoff");
            assert!(body.get("code").is_none(), "{body}");
        }
    }
}

/// A handoff tears the daemon down in no fixed order. When the session feed
/// closes before the shutdown signal reaches a wait, the wait still sends its
/// client to the next daemon; in a lab one of eight waiters failed this way.
// Hard-won: 4ce83357: a handoff waiter failed when its feed closed before shutdown signal.
#[tokio::test]
async fn a_wait_whose_feed_closes_during_a_handoff_still_sends_its_client_on() {
    let backend = Arc::new(FakeBackend {
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Running { started_at_ms: 10 },
            active_turn: Some(MaterializedTurn {
                command_id: "prompt-1".into(),
                accepted_ordinal: Some(5),
                turn_start_position: 6,
                started_at_ms: 10,
                steered_into: None,
            }),
            last_turn_outcome: None,
        })]),
        ..FakeBackend::default()
    });
    let (app, _actions, snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});
    let waiting = tokio::spawn(
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/wait"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"turn_id":5,"timeout_secs":3600}"#))
                .unwrap(),
        ),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished(), "the turn is still running");
    assert!(backend.upgrade_gate.try_close());
    // The feed closes first; the shutdown signal has not arrived.
    drop(snapshot_tx);
    let response = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("a closed feed ends the wait")
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(crate::server::UPGRADE_HEADER)
            .unwrap(),
        "pending"
    );
    assert_eq!(json_body(response).await["code"], DAEMON_HANDOFF_CODE);
}

/// An event stream that an upgrade handoff ends names the cursor it reached,
/// so its client resumes on the next daemon without losing an event.
// Hard-won: c87e5e88: event clients could not resume after a daemon handoff.
#[tokio::test]
async fn an_event_stream_ended_by_a_handoff_names_the_cursor_to_resume_from() {
    let backend = Arc::new(FakeBackend::default());
    backend.events.lock().unwrap().push(error_event(4));
    let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/events?after_seq=2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(frame.data_ref().unwrap())
            .unwrap()
            .contains("id: 4")
    );
    assert!(backend.upgrade_gate.try_close());
    backend.shutdown.cancel();
    let rest = tokio::time::timeout(Duration::from_secs(2), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    let rest = std::str::from_utf8(&rest).unwrap();
    assert!(
        rest.contains(&format!("event: {DAEMON_HANDOFF_CODE}")),
        "{rest}"
    );
    assert!(
        rest.contains("id: 4"),
        "resume after the last event sent: {rest}"
    );
}

// Hard-won: a1d7b484: a harness that answered nothing was reported as a finished turn.
#[test]
fn stop_reasons_map_to_outcomes_and_unknown_ones_stay_visible() {
    assert_eq!(map_stop_reason("end_turn"), (WaitOutcome::Finished, None));
    assert_eq!(map_stop_reason("EndTurn"), (WaitOutcome::Finished, None));
    assert_eq!(map_stop_reason("cancelled"), (WaitOutcome::Cancelled, None));
    assert_eq!(
        map_stop_reason("ModelCapacity"),
        (WaitOutcome::Error, Some("ModelCapacity".to_owned()))
    );
    assert_eq!(
        map_stop_reason("refusal"),
        (WaitOutcome::Error, Some("refusal".to_owned())),
        "an unrecognized ending must not be reported as success"
    );
    assert_eq!(
        map_stop_reason(mj_core::acp::AWAITING_INPUT_STOP_REASON),
        (WaitOutcome::InputRequired, None)
    );
    // A prompt the harness ended without answering is an error a script can
    // recognize by name, not a finished turn (#970).
    assert_eq!(
        map_stop_reason("prompt_unanswered"),
        (WaitOutcome::Error, Some("prompt_unanswered".to_owned()))
    );
}

/// A turn the worker failed because the harness produced nothing must reach
/// `mj wait` as an error carrying both the name and the explanation, the same
/// way a stalled turn does.
// Hard-won: a1d7b484: a harness that answered nothing was reported as a finished turn.
#[test]
fn an_unanswered_turn_reaches_wait_as_a_named_error() {
    let mut outcome = completed(7, "prompt_unanswered");
    outcome.diagnostic = Some(mj_core::diagnostic::TurnDiagnostic {
        message: "ACP prompt returned no session updates: Claude Code ended the turn without \
                  producing any message, thought or tool call"
            .into(),
        code: Some("prompt_unanswered".into()),
        http_status: None,
        reset_at: None,
    });
    let decision = WaitDecision::from_outcome(&outcome);
    assert_eq!(decision.outcome, WaitOutcome::Error);
    assert_eq!(decision.stop_reason.as_deref(), Some("prompt_unanswered"));
    assert!(
        decision
            .message
            .as_deref()
            .is_some_and(|message| message.contains("without producing any message")),
        "{:?}",
        decision.message
    );
}

fn completed(accepted_ordinal: u64, stop_reason: &str) -> MaterializedTurnOutcome {
    MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: format!("prompt-{accepted_ordinal}"),
        accepted_ordinal: Some(accepted_ordinal),
        turn_start_position: Some(accepted_ordinal + 1),
        completed_ordinal: accepted_ordinal + 2,
        completed_at_ms: 500,
        outcome: TurnOutcomeKind::Completed {
            stop_reason: stop_reason.into(),
        },
    }
}

fn idle(outcome: Option<MaterializedTurnOutcome>) -> WaitObservation {
    WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Live),
        execution: MaterializedExecutionState::Idle,
        last_turn_outcome: outcome,
        ..WaitObservation::default()
    }
}

/// A wait must never conclude that a turn finished from a state that only
/// says nobody can see the session. `mj wait` and the sub-agent wait share
/// this one decision. Durable turn ownership remains a guard even when the
/// published activity state says idle.
// Hard-won: a11622fe: an unaccounted session could still own work that a false finish would endanger.
#[test]
fn a_wait_never_concludes_finished_while_the_session_is_unaccounted_for() {
    let request = WaitRequest::default();

    // The #1025 window: the durable record says a turn is running and the
    // daemon has no live view. The wait keeps waiting.
    let running = WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Live),
        execution: MaterializedExecutionState::Running { started_at_ms: 1 },
        active_turn: Some(MaterializedTurn {
            command_id: "api-1".into(),
            accepted_ordinal: Some(7),
            turn_start_position: 8,
            started_at_ms: 1,
            steered_into: None,
        }),
        ..WaitObservation::default()
    };
    assert_eq!(resolve_wait(&running, &request), None);

    // The projection lagging the other way: the flag says idle while the turn
    // record is still open. "Newest turn" must not answer from that either.
    let lagging = WaitObservation {
        execution: MaterializedExecutionState::Idle,
        ..running.clone()
    };
    assert_eq!(resolve_wait(&lagging, &request), None);

    // Queued work behind a finished turn is not an ending for "newest turn".
    let queued = WaitObservation {
        execution: MaterializedExecutionState::Idle,
        active_turn: None,
        queued: 1,
        last_turn_outcome: Some(completed(7, "end_turn")),
        ..WaitObservation::default()
    };
    assert_eq!(resolve_wait(&queued, &request), None);

    // And the states a session can report while unaccounted for are never
    // idle and always hold work, so nothing downstream can read completion
    // into them either.
    for state in [
        mj_core::activity::ActivityState::Unknown {
            last_known: Box::new(mj_core::activity::ActivityState::Turn {
                started_at_ms: Some(1),
                last_activity_at_ms: None,
            }),
            since_ms: Some(2),
        },
        mj_core::activity::ActivityState::Unknown {
            last_known: Box::new(mj_core::activity::ActivityState::Idle { since_ms: None }),
            since_ms: None,
        },
        mj_core::activity::ActivityState::Unrecognized,
    ] {
        assert!(!state.is_idle(), "{state:?}");
        assert!(state.has_work_in_flight(), "{state:?}");
    }
}

/// `mj wait` with no turn must not say "finished" while the session is still
/// provisioning, or is live but not yet able to take a prompt (a resume that
/// has not reattached): the next prompt would be refused.
// Hard-won: 942fa9c5: wait said finished while provisioning or reattachment still blocked prompts.
#[test]
fn a_wait_without_a_turn_waits_until_the_session_can_take_a_prompt() {
    let request = WaitRequest {
        return_on_input: false,
        turn_id: None,
        timeout_secs: None,
    };
    let mut starting = idle(None);
    starting.lifecycle = Some(ViewerLifecycleCategory::Starting);
    starting.cannot_take_prompt = true;
    assert_eq!(resolve_wait(&starting, &request), None);

    let mut reattaching = idle(Some(completed(3, "end_turn")));
    reattaching.cannot_take_prompt = true;
    assert_eq!(resolve_wait(&reattaching, &request), None);

    let ready = idle(None);
    assert_eq!(
        resolve_wait(&ready, &request).map(|decision| decision.outcome),
        Some(WaitOutcome::Finished)
    );
}

/// Launch finding R3-1: `mj wait` with no turn, run right after
/// `mj new ... "<prompt>"`, answered "finished" with `chat_phase idle` while
/// the first prompt was still waiting to be submitted. The session was
/// attached and idle, but the prompt `mj new` handed over had not become a
/// turn yet. That prompt is work in flight, like a running turn.
// Hard-won: 0c9916a1: wait answered finished before the create-time prompt was submitted.
#[tokio::test(start_paused = true)]
async fn a_wait_right_after_creating_with_a_prompt_waits_for_that_prompt() {
    let idle_session = || {
        Some(TurnState {
            execution: MaterializedExecutionState::Idle,
            active_turn: None,
            last_turn_outcome: None,
        })
    };
    let can_take_prompt = |snapshot: &mut ViewerSnapshot| {
        snapshot.sessions[0].capabilities.prompt = true;
    };
    let wait = |app: axum::Router| async move {
        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/wait"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"timeout_secs":2}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    };

    let queued_first_prompt = Arc::new(FakeBackend {
        turn_states: Mutex::new(vec![idle_session()]),
        start_status: Some(StartStatus::Pending),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(queued_first_prompt, can_take_prompt);
    let body = wait(app).await;
    assert_eq!(
        body["outcome"], "timeout",
        "the first prompt has not been answered yet: {body}"
    );

    // With nothing handed over at creation, an idle session that can take a
    // prompt has nothing to wait for.
    let nothing_queued = Arc::new(FakeBackend {
        turn_states: Mutex::new(vec![idle_session()]),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(nothing_queued, can_take_prompt);
    assert_eq!(wait(app).await["outcome"], "finished");
}

#[test]
fn an_earlier_prompt_s_outcome_never_answers_a_later_prompt_s_wait() {
    let request = WaitRequest {
        return_on_input: false,
        turn_id: Some(12),
        timeout_secs: None,
    };
    // Prompt A was accepted at 10 and finished while B, accepted at 12, is
    // still queued. Idle plus "newest turn" would answer with A's ending.
    assert_eq!(
        resolve_wait(&idle(Some(completed(10, "end_turn"))), &request),
        None
    );
    let decision = resolve_wait(&idle(Some(completed(12, "end_turn"))), &request)
        .expect("B's own outcome ends the wait");
    assert_eq!(decision.outcome, WaitOutcome::Finished);
    assert_eq!(decision.turn_id, Some(12));
}

#[test]
fn a_legacy_capacity_outcome_only_ends_the_wait_once_no_retry_is_armed() {
    let request = WaitRequest {
        return_on_input: false,
        turn_id: Some(10),
        timeout_secs: None,
    };
    let mut pending = idle(Some(completed(10, "ModelCapacity")));
    pending.capacity_retry = Some(CapacityRetry {
        attempt: 1,
        retry_at_ms: 60_000,
        command_id: "capacity-retry-10".into(),
        submitted: false,
    });
    assert_eq!(
        resolve_wait(&pending, &request),
        None,
        "the worker will retry, so the caller must not prompt over it"
    );

    let settled = idle(Some(completed(10, "ModelCapacity")));
    assert_eq!(
        resolve_wait(&settled, &request).unwrap().outcome,
        WaitOutcome::Error
    );
}

#[test]
fn server_retry_assessment_and_generic_retry_keep_the_wait_open() {
    let request = WaitRequest {
        turn_id: Some(10),
        ..WaitRequest::default()
    };
    for stop_reason in ["end_turn", "error"] {
        let mut pending = idle(Some(completed(10, stop_reason)));
        pending.retry_assessment_pending = true;
        assert_eq!(resolve_wait(&pending, &request), None);
        pending.retry_assessment_pending = false;
        pending.capacity_retry = Some(CapacityRetry {
            attempt: 1,
            retry_at_ms: 60_000,
            command_id: "server-retry-10".into(),
            submitted: false,
        });
        assert_eq!(resolve_wait(&pending, &request), None);
        pending.capacity_retry = None;
        assert!(resolve_wait(&pending, &request).is_some());
    }
}

#[test]
fn rejections_stopped_sessions_and_an_empty_session_each_end_the_wait() {
    let anything = WaitRequest::default();

    let mut rejected = idle(None);
    rejected.last_turn_outcome = Some(MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: "prompt-1".into(),
        accepted_ordinal: Some(4),
        turn_start_position: None,
        completed_ordinal: 5,
        completed_at_ms: 10,
        outcome: TurnOutcomeKind::Rejected {
            reason: None,
            message: "transport failed".into(),
        },
    });
    let decision = resolve_wait(&rejected, &anything).unwrap();
    assert_eq!(decision.outcome, WaitOutcome::Error);
    assert_eq!(decision.message.as_deref(), Some("transport failed"));

    let mut stopped = idle(Some(completed(10, "end_turn")));
    stopped.lifecycle = Some(ViewerLifecycleCategory::Suspended);
    assert_eq!(
        resolve_wait(&stopped, &anything).unwrap().outcome,
        WaitOutcome::Stopped,
        "a stopped session cannot finish a turn, whatever its last one did"
    );

    let decision = resolve_wait(&idle(None), &anything).unwrap();
    assert_eq!(decision.outcome, WaitOutcome::Finished);
    assert_eq!(
        decision.turn_id, None,
        "an idle session with nothing queued has no turn to name"
    );

    let mut running = idle(None);
    running.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    assert_eq!(resolve_wait(&running, &anything), None);

    let mut queued = idle(Some(completed(10, "end_turn")));
    queued.queued = 1;
    assert_eq!(
        resolve_wait(&queued, &anything),
        None,
        "a queued prompt means the session is not done"
    );
}

// Hard-won: ce490a90: stale session errors failed healthy waits while launch notices were misattributed.
#[test]
fn a_launch_failure_fails_the_wait_but_an_unrelated_session_error_does_not() {
    let mut launch_failed = idle(None);
    launch_failed.launch_failed = true;
    launch_failed.launch_error =
        Some("worker bootstrap failed: Connection closed by 10.0.0.1 port 22".into());
    let decision = resolve_wait(&launch_failed, &WaitRequest::default()).unwrap();
    assert_eq!(
        decision.outcome,
        WaitOutcome::Error,
        "nothing will finish a turn on a session that never launched"
    );
    assert_eq!(
        decision.message.as_deref(),
        Some("worker bootstrap failed: Connection closed by 10.0.0.1 port 22"),
        "the wait reports why the launch failed, not a bare sentence"
    );

    // A launch failure with no recorded reason still fails, with the
    // fixed sentence as a fallback.
    let mut launch_failed_bare = idle(None);
    launch_failed_bare.launch_failed = true;
    assert_eq!(
        resolve_wait(&launch_failed_bare, &WaitRequest::default())
            .unwrap()
            .message
            .as_deref(),
        Some("the session failed to launch")
    );

    let failed_start = WaitObservation {
        start_status: Some(StartStatus::Failed {
            message: "the profile has no home".into(),
        }),
        ..idle(None)
    };
    let decision = resolve_wait(&failed_start, &WaitRequest::default()).unwrap();
    assert_eq!(decision.outcome, WaitOutcome::Error);
    assert_eq!(decision.message.as_deref(), Some("the profile has no home"));

    let durable_failure = WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Failed),
        ..idle(None)
    };
    let decision = resolve_wait(&durable_failure, &WaitRequest::default()).unwrap();
    assert_eq!(decision.outcome, WaitOutcome::Error);
    assert_eq!(
        decision.message.as_deref(),
        Some("the session is in a failed state")
    );

    // The session carries an error from some earlier action. The turn the
    // caller named is running fine, so the wait keeps waiting.
    let running = WaitObservation {
        execution: MaterializedExecutionState::Running { started_at_ms: 1 },
        active_turn: Some(MaterializedTurn {
            command_id: "prompt-12".into(),
            accepted_ordinal: Some(12),
            turn_start_position: 13,
            started_at_ms: 1,
            steered_into: None,
        }),
        ..idle(Some(completed(10, "end_turn")))
    };
    assert_eq!(
        resolve_wait(
            &running,
            &WaitRequest {
                return_on_input: false,
                turn_id: Some(12),
                timeout_secs: None,
            }
        ),
        None,
        "a stale session error must not report a running turn as failed"
    );
}

// Hard-won: f461e518: an accepted close could fail later without informing its caller.
#[test]
fn a_wait_follows_a_close_and_reports_one_that_did_not_finish() {
    let request = WaitRequest::default();

    // The close owns the session. Without this the wait would answer
    // "stopped" while the close was still running, and never see how it ended.
    let running_close = WaitObservation {
        closing: true,
        lifecycle: Some(ViewerLifecycleCategory::Suspending),
        ..idle(Some(completed(10, "end_turn")))
    };
    assert_eq!(resolve_wait(&running_close, &request), None);

    // It ended and the session is alive again, so it failed. The reason the
    // close recorded is what the wait has to report: the request that asked
    // for the close was answered when it was admitted.
    let failed_close = WaitObservation {
        close_failure: Some(
            "the suspension did not finish: the checkpoint could not be written".into(),
        ),
        lifecycle: Some(ViewerLifecycleCategory::Live),
        ..idle(Some(completed(10, "end_turn")))
    };
    let decision = resolve_wait(&failed_close, &request).unwrap();
    assert_eq!(decision.outcome, WaitOutcome::Error);
    assert_eq!(
        decision.message.as_deref(),
        Some("the suspension did not finish: the checkpoint could not be written"),
        "reporting the finished turn instead would call a failed close a success"
    );

    // A close that reached its destination still ends the wait as stopped.
    let finished_close = WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Suspended),
        ..idle(Some(completed(10, "end_turn")))
    };
    assert_eq!(
        resolve_wait(&finished_close, &request).unwrap().outcome,
        WaitOutcome::Stopped
    );

    // A close that left the session dead reports its recorded reason rather
    // than only that the session failed.
    let dead = WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Failed),
        launch_error: Some("close failed and left the session without a live worker".into()),
        ..idle(None)
    };
    assert_eq!(
        resolve_wait(&dead, &request).unwrap().message.as_deref(),
        Some("close failed and left the session without a live worker")
    );
}

// Hard-won: ce490a90: launch notices were matched by notice ID instead of session ID.
#[test]
fn a_launch_failure_for_another_session_is_not_this_session_s() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let session_id = snapshot.sessions[0].id.clone();
    snapshot.launch_failures = vec![super::super::ViewerLaunchFailure {
        id: format!("{}-4", std::process::id()),
        workspace_id: snapshot.sessions[0].workspace_id.clone(),
        session_id: Some("some-other-session".to_owned()),
        error: Some("worker bootstrap failed".to_owned()),
    }];

    let observation = build_observation(&snapshot, &snapshot.sessions[0], None, None, None);
    assert!(
        !observation.launch_failed,
        "another session's failed launch says nothing about this one"
    );

    snapshot.launch_failures[0].session_id = Some(session_id);
    let observation = build_observation(&snapshot, &snapshot.sessions[0], None, None, None);
    assert!(observation.launch_failed);
}

// Hard-won: be5abcca: failed launches returned no actionable reason.
#[test]
fn api_session_exposes_a_launch_failure_reason_only_when_the_session_errored() {
    let (config, mut state) = sample_config_state();

    // A running session that carries an internal error still only flags it;
    // it never puts the raw text on the wire.
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let running = ApiSession::from(&snapshot.sessions[0]);
    assert!(
        running.has_error,
        "the running session still flags an error"
    );
    assert_eq!(
        running.error, None,
        "a running session does not expose raw error text"
    );
    assert!(
        serde_json::to_value(&running)
            .unwrap()
            .get("error")
            .is_none(),
        "the error field is omitted when there is nothing to show"
    );

    // Once the same session has failed to launch, it carries its reason so
    // a client sees why instead of a bare state.
    state.sessions.get_mut("session-1").unwrap().state = mj_core::state::SessionState::Error;
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let failed = ApiSession::from(&snapshot.sessions[0]);
    assert_eq!(
        failed.error.as_deref(),
        Some("secret-token at /highly/secret/codex"),
        "a failed launch surfaces its recorded reason"
    );
    assert_eq!(
        serde_json::to_value(&failed).unwrap()["error"],
        "secret-token at /highly/secret/codex"
    );
}

// Hard-won: ce490a90: unusable workers appeared as unexplained wait timeouts.
#[test]
fn relay_health_names_each_way_the_live_view_can_be_unusable() {
    use mj_client::session::{ManagedSessionView, ViewError};

    let connected = ManagedSessionView {
        connected: true,
        ..ManagedSessionView::default()
    };
    assert_eq!(
        RelayHealth::from(&connected),
        RelayHealth {
            state: RelayState::Connected,
            detail: None,
        }
    );
    assert_eq!(
        RelayHealth::from(&ManagedSessionView::default()).state,
        RelayState::Disconnected,
        "not yet attached is not the same as a failure"
    );

    for (error, expected) in [
        (
            ViewError::Unreachable("ssh: connection refused".into()),
            RelayState::Unreachable,
        ),
        (
            ViewError::TargetMissing("container gone".into()),
            RelayState::TargetMissing,
        ),
        (
            ViewError::ProjectionIntegrity("digest mismatch".into()),
            RelayState::ProjectionIntegrity,
        ),
    ] {
        let detail = error.detail().to_owned();
        // Connected plus an error is what a relay that dropped mid-turn
        // looks like; the error is the thing the caller needs.
        let view = ManagedSessionView {
            connected: true,
            error: Some(error),
            ..ManagedSessionView::default()
        };
        assert_eq!(
            RelayHealth::from(&view),
            RelayHealth {
                state: expected,
                detail: Some(detail),
            }
        );
    }
}

#[tokio::test]
async fn the_file_route_returns_bytes_and_refuses_a_path_that_leaves_the_workspace() {
    let backend = Arc::new(FakeBackend {
        file: Some(b"file bytes".to_vec()),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

    let response = app
        .clone()
        .oneshot(
            bearer(Request::get(
                "/api/v1/sessions/session-1/files?path=app/README.md",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"file bytes");
    assert_eq!(
        backend.file_paths.lock().unwrap().as_slice(),
        [PathBuf::from("app/README.md")]
    );

    // A `..` names a sibling repository in a multi-repo bundle now that a path
    // resolves in the agent's directory. Only the daemon knows how far it may
    // climb, so it reaches the backend rather than being refused here (#1079).
    let response = app
        .clone()
        .oneshot(
            bearer(Request::get(
                "/api/v1/sessions/session-1/files?path=../lib/README.md",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        backend.file_paths.lock().unwrap().as_slice(),
        [
            PathBuf::from("app/README.md"),
            PathBuf::from("../lib/README.md")
        ]
    );

    for path in ["/etc/passwd", ""] {
        let response = app
            .clone()
            .oneshot(
                bearer(Request::get(format!(
                    "/api/v1/sessions/session-1/files?path={path}"
                )))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{path:?} must never reach the target"
        );
    }
    assert_eq!(
        backend.file_paths.lock().unwrap().len(),
        2,
        "a rejected path is not sent to the backend"
    );
}

#[tokio::test]
async fn an_empty_bundle_is_refused_rather_than_served_as_an_empty_file() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/export"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"kind":"bundle"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(response).await["error"],
        "no commits beyond the session base"
    );

    let (app, _actions, _snapshot_tx, _bundles) = api_app(
        Arc::new(FakeBackend {
            bundle_fails: true,
            ..FakeBackend::default()
        }),
        |_| {},
    );
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/export"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"kind":"bundle"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json_body(response).await["error"],
        "checkpoint storage failed"
    );
}
fn input_request() -> mj_core::elicitation::ElicitationRequest {
    mj_core::elicitation::ElicitationRequest::from_acp_params("question-1", serde_json::json!({
        "sessionId": "session-1", "mode": "form", "message": "Choose a name", "requestedSchema": {
            "type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}
        }
    })).unwrap()
}

#[tokio::test]
async fn file_upload_accepts_large_binary_bodies_and_rejects_unsafe_paths_and_limits() {
    let backend = Arc::new(FakeBackend::default());
    let (app, _actions, snapshots, _bundles) = api_app(backend.clone(), |snapshot| {
        snapshot.sessions[0].is_idle = true;
        snapshot.sessions[0].lifecycle = ViewerLifecycleCategory::Live;
    });
    let payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let response = app
        .clone()
        .oneshot(
            bearer(Request::put(
                "/api/v1/sessions/session-1/files?path=input/data.bin&overwrite=true",
            ))
            .body(Body::from(payload.clone()))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["bytes"], payload.len());
    assert_eq!(
        backend.file_writes.lock().unwrap()[0],
        (PathBuf::from("input/data.bin"), payload, true)
    );
    // As with reads, how far `..` may climb depends on the session's layout, so
    // only an absolute or empty path is refused here (#1079).
    for path in ["/absolute", ""] {
        let response = app
            .clone()
            .oneshot(
                bearer(Request::put(format!(
                    "/api/v1/sessions/session-1/files?path={path}"
                )))
                .body(Body::from("bad"))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .clone()
        .oneshot(
            bearer(Request::put("/api/v1/sessions/session-1/files?path=large"))
                .body(Body::from(vec![
                    0;
                    mj_checkpoint::archive::MAX_SESSION_FILE_BYTES
                        as usize
                        + 1
                ]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    snapshots.send_modify(|s| s.sessions[0].is_idle = false);
    let response = app
        .oneshot(
            bearer(Request::put("/api/v1/sessions/session-1/files?path=busy"))
                .body(Body::from("bad"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(backend.file_writes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn structured_inputs_are_listed_validated_and_forwarded() {
    let (app, mut actions, _snapshots, _bundles) = api_app(Arc::new(FakeBackend::default()), |s| {
        s.sessions[0].pending_elicitations = vec![input_request()]
    });
    let response = app
        .clone()
        .oneshot(
            bearer(Request::get("/api/v1/sessions/session-1/elicitations"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await[0]["id"], "question-1");
    let response = app
        .clone()
        .oneshot(
            bearer(Request::post(
                "/api/v1/sessions/session-1/elicitations/question-1",
            ))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"action":"accept","content":{}}"#))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(actions.try_recv().is_err());
    let response = tokio::spawn(
        app.oneshot(
            bearer(Request::post(
                "/api/v1/sessions/session-1/elicitations/question-1",
            ))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"action":"accept","content":{"name":"example"}}"#,
            ))
            .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();
    assert!(
        matches!(action.action, ControllerAction::RespondElicitation { elicitation_id, .. } if elicitation_id == "question-1")
    );
    action
        .reply
        .send(ActionOutcome::Accepted { session_id: None })
        .unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[test]
fn input_aware_wait_is_opt_in_and_respects_completed_turns_and_stopping() {
    let mut observation = WaitObservation {
        pending_elicitations: vec![input_request()],
        execution: MaterializedExecutionState::Running { started_at_ms: 1 },
        ..Default::default()
    };
    assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());
    let mut request = WaitRequest {
        return_on_input: true,
        ..Default::default()
    };
    assert_eq!(
        resolve_wait(&observation, &request).unwrap().outcome,
        WaitOutcome::InputRequired
    );
    observation.last_turn_outcome = Some(completed(5, "end_turn"));
    request.turn_id = Some(5);
    assert_eq!(
        resolve_wait(&observation, &request).unwrap().outcome,
        WaitOutcome::Finished
    );
    observation.lifecycle = Some(ViewerLifecycleCategory::Suspending);
    assert_eq!(
        resolve_wait(&observation, &request).unwrap().outcome,
        WaitOutcome::Stopped
    );
}

#[tokio::test]
async fn input_aware_wait_returns_the_form_without_needing_a_turn_summary() {
    let (app, _actions, _snapshots, _bundles) = api_app(Arc::new(FakeBackend::default()), |s| {
        s.sessions[0].pending_elicitations = vec![input_request()]
    });
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/wait"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"return_on_input":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["outcome"], "input_required");
    assert_eq!(body["pending_elicitations"][0]["id"], "question-1");
}

/// Make the fixture session look the way `mj suspend` leaves one: suspended, with
/// resume as the thing it can do next.
fn make_stopped(snapshot: &mut ViewerSnapshot) {
    snapshot.workspaces.push(super::super::ViewerWorkspace {
        id: snapshot.sessions[0].workspace_id.clone(),
        name: "default".into(),
    });
    let session = &mut snapshot.sessions[0];
    session.state = "stopped".into();
    session.lifecycle = ViewerLifecycleCategory::Suspended;
    session.has_checkpoint = true;
    session.capabilities.resume = true;
    session.capabilities.prompt = false;
    session.incompatible_resume_targets.clear();
    session.compatible_resume_targets = vec!["podman".into(), "raw".into()];
}

#[tokio::test]
async fn resuming_a_running_session_is_refused_with_the_reason() {
    let (app, _actions, _snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/resume"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await;
    let error = body["error"].as_str().unwrap().to_owned();
    assert!(
        error.contains("already running") && !error.contains("close"),
        "unexpected refusal: {error}"
    );
}

/// `close` is gone (2e3077d9). A resume refused because the session is
/// suspending says what to wait for, in the words the CLI uses now.
// Hard-won: f8f641a2: suspend-then-resume refusal still advised users to use removed close.
#[test]
fn a_resume_refusal_names_what_to_wait_for_in_current_words() {
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let mut session = snapshot.sessions[0].clone();
    session.lifecycle = ViewerLifecycleCategory::Suspending;
    let refusal = resume_refusal(&session);
    assert!(refusal.contains("wait until it is suspended"), "{refusal}");
    assert!(!refusal.contains("close"), "{refusal}");
}

/// W-2: a failed Move that holds the session's environment withdraws
/// Resume, and the refusal says why and what to do instead, rather than
/// claiming an operation is running.
// Hard-won: ae5e37e5: a failed Move with a retained environment was mistaken for a running operation.
#[test]
fn a_resume_refused_for_a_move_held_environment_names_the_move() {
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let mut session = snapshot.sessions[0].clone();
    session.lifecycle = ViewerLifecycleCategory::Failed;
    session.move_recovery = Some(crate::server::ViewerMoveRecovery {
        operation_id: "move-a".into(),
        source_profile_id: "destination".into(),
        source_target_template_id: "destination".into(),
        destination_profile_id: "fake".into(),
        destination_target_template_id: "localhost".into(),
        phase: "failed".into(),
        queue: "discard".into(),
        clear_resource_allocation: false,
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_additional_mounts: Vec::new(),
        destination_resource_allocation: None,
        checkpoint_retained: true,
        environment_retained: true,
        destination_ready: false,
        queue_admission_started: false,
        queue_admission_finished: false,
    });
    let refusal = resume_refusal(&session);
    assert!(
        refusal.contains("retry the Move to fake / localhost"),
        "{refusal}"
    );
    assert!(!refusal.contains("operation running"), "{refusal}");
    session.move_recovery.as_mut().unwrap().checkpoint_retained = false;
    let refusal = resume_refusal(&session);
    assert!(refusal.contains("destroy the session"), "{refusal}");
    assert!(!refusal.contains("retry the Move"), "{refusal}");
}

/// Launch finding R6-1: `mj resume` on a session with no checkpoint was
/// accepted, and the daemon's resume then failed with "session has no
/// checkpoint" where the caller never saw it. The API now refuses it at once,
/// with that reason, the way it refuses every other resume that cannot work.
// Hard-won: 78245c3a: resume without a checkpoint was accepted and then failed silently.
#[tokio::test]
async fn resuming_a_failed_session_without_a_checkpoint_is_refused_at_once() {
    let (app, mut actions, _snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), |snapshot| {
            make_stopped(snapshot);
            // A launch that failed before its first checkpoint.
            let session = &mut snapshot.sessions[0];
            session.has_checkpoint = false;
            session.state = "error".into();
            session.lifecycle = ViewerLifecycleCategory::Failed;
            session.has_error = true;
            session.launch_error = Some("worker bootstrap failed: operation cancelled".into());
        });
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/resume"))
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("the resume was admitted instead of refused")
    .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await;
    let error = body["error"].as_str().unwrap().to_owned();
    assert!(error.contains("session has no checkpoint"), "{error}");
    assert!(error.contains("mj destroy --session session-1"), "{error}");
    assert!(actions.try_recv().is_err(), "no resume reaches the daemon");
}

/// Launch finding R6-1: the published session says whether its record holds
/// a checkpoint, which is what the resume route and the prompt refusal read.
#[test]
fn a_published_session_says_whether_it_has_a_checkpoint() {
    let (config, mut state) = sample_config_state();
    let record = state.sessions.get_mut("session-1").unwrap();
    record.state = mj_core::state::SessionState::Error;
    assert!(record.checkpoint.is_none());
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    assert!(!snapshot.sessions[0].has_checkpoint);

    state.sessions.get_mut("session-1").unwrap().checkpoint =
        Some(mj_core::state::CheckpointMetadata {
            archive_path: "/private/archive.hel.zip".into(),
            sha256: "a".repeat(64),
            created_at: "now".into(),
            event_frontier: 3,
        });
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    assert!(snapshot.sessions[0].has_checkpoint);
    let published = serde_json::to_string(&snapshot.sessions[0]).unwrap();
    assert!(
        !published.contains("/private/archive.hel.zip"),
        "{published}"
    );
}

#[tokio::test]
async fn resuming_an_unknown_session_is_not_found() {
    let (app, _actions, _snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), make_stopped);
    let response = app
        .oneshot(
            bearer(Request::post("/api/v1/sessions/session-404/resume"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn a_wait_follows_a_running_resume_and_reports_why_a_failed_one_stopped() {
    // While the resume runs, the durable record still says stopped. Answering
    // `stopped` there would tell a caller its session will never come up.
    let mut observation = WaitObservation {
        lifecycle: Some(ViewerLifecycleCategory::Suspended),
        resuming: true,
        ..Default::default()
    };
    assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());

    // The resume failed and rolled the record back, leaving its reason there.
    observation.resuming = false;
    observation.launch_error = Some("resume failed: checkpoint archive is missing".into());
    let decision = resolve_wait(&observation, &WaitRequest::default()).unwrap();
    assert_eq!(decision.outcome, WaitOutcome::Stopped);
    assert_eq!(
        decision.message.as_deref(),
        Some("resume failed: checkpoint archive is missing")
    );
}

/// The child a spawn creates is not in the viewer snapshot yet: that snapshot
/// is republished on a tick. Answering "unknown session" for a spawn that
/// succeeded told the caller its child does not exist while the child was
/// starting, and invited it to spawn a second one.
// Hard-won: 2bc686dc: HTTP spawn returned 404 for children that were already running.
#[tokio::test]
async fn a_spawn_waits_for_its_child_to_appear_instead_of_reporting_it_unknown() {
    let (app, _actions, snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), |snapshot| {
            snapshot.sessions[0].harness_kind = "codex".to_owned();
            snapshot.sessions[0].subagents = mj_core::subagent::SubagentPolicy::AllModels;
        });
    let parent = {
        let snapshot = snapshot_tx.borrow();
        snapshot.sessions[0].id.clone()
    };

    // The child reaches the snapshot a moment after the spawn returns from the
    // controller, which is what happens in a live daemon.
    let publisher = tokio::spawn({
        let snapshot_tx = snapshot_tx.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let mut snapshot = snapshot_tx.borrow().clone();
            let mut child = snapshot.sessions[0].clone();
            child.id = SPAWNED_CHILD.to_owned();
            child.title = "spawned child".to_owned();
            snapshot.sessions.push(child);
            snapshot_tx.send_replace(snapshot);
        }
    });

    let response = app
        .oneshot(
            bearer(Request::post(format!(
                "/api/v1/sessions/{parent}/subagents"
            )))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"task_name":"probe","instructions":"say ready","model":"kimi-code/k3"}"#,
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    publisher.await.unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_body(response).await;
    assert_eq!(body["session"]["id"], SPAWNED_CHILD);
    assert_eq!(body["task_name"], "probe");
    assert!(body.get("request_key").is_none(), "{body}");
}

#[tokio::test]
async fn suspension_rejects_destruction_flags_and_removed_routes() {
    for (route, body, expected) in [
        (
            "suspend",
            r#"{"force":true}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "suspend",
            r#"{"delete_branch":true}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "destroy",
            r#"{"force":true}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("close", "{}", StatusCode::NOT_FOUND),
        ("cancel-turn", "{}", StatusCode::NOT_FOUND),
    ] {
        let (app, mut actions, _, _) = api_app(Arc::new(FakeBackend::default()), |_| {});
        let response = app
            .oneshot(
                bearer(Request::post(format!("/api/v1/sessions/session-1/{route}")))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{route}: {body}");
        assert!(actions.try_recv().is_err());
    }
}

#[test]
fn both_wait_forms_follow_continuation() {
    let observation = WaitObservation {
        activity: mj_core::activity::ActivityState::CheckingContinuation,
        execution: MaterializedExecutionState::Idle,
        last_turn_outcome: Some(completed(7, "end_turn")),
        ..Default::default()
    };
    assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());
    let request = WaitRequest {
        turn_id: Some(7),
        ..Default::default()
    };
    assert!(resolve_wait(&observation, &request).is_none());
}

#[test]
fn session_wait_uses_the_workers_published_jev_activity() {
    use mj_core::activity::ActivityState;
    let root = tempfile::tempdir().unwrap();
    let relay = mj_worker::relay::DurableRelay::open(root.path(), "session-1", "test").unwrap();
    let mut materialized = mj_core::state::MaterializedSession::empty("session-1");
    materialized.last_turn_outcome = Some(completed(7, "end_turn"));
    let mut live = ManagedSessionView {
        connected: true,
        error: None,
        snapshot: Some(mj_core::state::ManagedSessionSnapshot {
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational: relay.operational_state(),
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            latest_credential_sync_signal: None,
            worker_build: None,
        }),
    };
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let mut session = snapshot.sessions[0].clone();
    session.capabilities.prompt = true;
    session.activity_state = Some(ActivityState::Idle { since_ms: None });
    let explicit = WaitRequest {
        turn_id: Some(7),
        ..Default::default()
    };
    for activity in [
        ActivityState::Expecting { since_ms: 10 },
        ActivityState::Background {
            started_at_ms: Some(10),
        },
        ActivityState::CheckingContinuation,
        ActivityState::Goal,
        ActivityState::Unrecognized,
    ] {
        live.snapshot.as_mut().unwrap().operational.activity = Some(activity.clone());
        let observation = build_observation(&snapshot, &session, Some(&live), None, None);
        assert_eq!(observation.activity, activity);
        assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());
        assert!(resolve_wait(&observation, &explicit).is_none());
        let observation = WaitObservation {
            start_status: Some(StartStatus::Submitted { turn_id: 7 }),
            ..observation
        };
        assert!(
            resolve_wait(&observation, &WaitRequest::default()).is_none(),
            "an implicit initial turn target must still follow session activity"
        );
    }
    // Jev can judge a reply finished even with an abandoned background task.
    // Consume that published decision rather than recounting the processes.
    let operational = &mut live.snapshot.as_mut().unwrap().operational;
    operational
        .background_commands
        .push(mj_core::relay::BackgroundCommand {
            id: "leftover".into(),
            started_at_ms: 1,
            command: "sleep infinity".into(),
            can_stop: false,
        });
    operational.activity = Some(ActivityState::Idle { since_ms: Some(20) });
    operational.turn_completion = Some(mj_core::activity::verdict::TurnCompletion {
        command_id: "prompt-7".into(),
        completed_ordinal: 20,
        decision: mj_core::activity::verdict::Decision::InferIdle,
    });
    session.activity_state = Some(ActivityState::Expecting { since_ms: 10 });
    let observation = build_observation(&snapshot, &session, Some(&live), None, None);
    assert_eq!(
        resolve_wait(&observation, &WaitRequest::default())
            .unwrap()
            .outcome,
        WaitOutcome::Finished
    );

    live.connected = false;
    session.activity_state = Some(ActivityState::Idle { since_ms: None });
    let disconnected = build_observation(&snapshot, &session, Some(&live), None, None);
    assert!(matches!(
        disconnected.activity,
        ActivityState::Unknown { .. }
    ));
    assert!(resolve_wait(&disconnected, &WaitRequest::default()).is_none());
    assert!(
        resolve_wait(&disconnected, &explicit).is_some(),
        "recorded turn outcomes survive a disconnect"
    );
    session.activity_state = None;
    let unknown = build_observation(&snapshot, &session, None, None, None);
    assert!(resolve_wait(&unknown, &WaitRequest::default()).is_none());
}

#[test]
fn quota_recovery_keeps_wait_pending_and_unknown_reset_reports_quota() {
    let mut pending = idle(Some(completed(10, "end_turn")));
    pending.quota_recovery = Some(mj_core::continuation::QuotaRecovery {
        user_command_id: "user-request".into(),
        completed_command_id: pending
            .last_turn_outcome
            .as_ref()
            .unwrap()
            .command_id
            .clone(),
        profile_id: "test-profile".into(),
        reset_at_ms: Some(1000),
        retry_at_ms: Some(61000),
        notice: "No reliable reset time".into(),
        submitted: false,
    });
    assert!(resolve_wait(&pending, &WaitRequest::default()).is_none());
    pending.quota_recovery.as_mut().unwrap().retry_at_ms = None;
    assert_eq!(
        resolve_wait(&pending, &WaitRequest::default())
            .unwrap()
            .outcome,
        WaitOutcome::QuotaLimit
    );
}

/// Every secret the sample fixture holds: a profile home and environment, a
/// container image and environment, a local repository source, and a native
/// session id. The options route narrows the public projection to a launch
/// decision, so none of them may appear in its body.
const SAMPLE_SECRETS: [&str; 6] = [
    "/highly/secret/codex",
    "secret-token",
    "secret.registry/image",
    "secret-target",
    "/private/source/hel",
    "native-secret-id",
];

#[tokio::test]
async fn options_list_what_a_caller_may_launch_without_leaking_configuration() {
    let (app, _, _, _) = api_app(Arc::new(FakeBackend::default()), |_| {});

    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/options"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(API_VERSION_HEADER).unwrap(),
        API_VERSION
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    for secret in SAMPLE_SECRETS {
        assert!(
            !text.contains(secret),
            "the launch options published {secret}"
        );
    }
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(body["revision"].as_u64(), Some(1));
    assert_eq!(body["profiles"][0]["id"], "codex-1");
    assert_eq!(body["profiles"][0]["harness"], "codex");
    assert_eq!(body["bundles"][0]["id"], "hel");
    assert_eq!(body["bundles"][0]["repositories"][0]["github"], "owner/hel");

    let targets = body["targets"].as_array().unwrap();
    let raw = targets.iter().find(|target| target["id"] == "raw").unwrap();
    assert_eq!(raw["kind"], "local-bare");
    assert_eq!(raw["requires_project_directory"].as_bool(), Some(true));
    // No host reading covers this target in this test. An unchecked target is
    // unknown, not unavailable: nothing has said it is broken.
    assert_eq!(raw["availability"], "unknown");
    assert!(raw["host"].is_null());
    assert!(raw["unavailable_reason"].is_null());

    // Nothing has been saved as this instance's default.
    assert!(body["default"].is_null());
}

/// F-7: on a host with no Docker engine, `mj doctor` and the dashboard's
/// Targets pane said the built-in `docker` target was unavailable while this
/// route said `ready`, because the local host's capacity reading answered.
/// The route now asks the same engine check, and says the engine is missing.
// Hard-won: ba563efa: options said Docker was ready while doctor and the dashboard found no engine.
#[tokio::test]
async fn options_mark_a_local_target_without_its_engine_unavailable() {
    use crate::controller::LocalEngineReadiness;

    let (app, _, _, _) = api_app_with_engines(
        Arc::new(FakeBackend::default()),
        |snapshot| {
            snapshot.targets.push(crate::server::ViewerTarget {
                id: "docker".into(),
                kind: "local-docker".into(),
                resource_allocation_kind: mj_core::state::ResourceAllocationKind::Container,
                requires_project_directory: false,
                remembered_container_size: None,
                container_host_limits: None,
                default_resource_allocation: None,
                runtime_missing: false,
                default_candidate: false,
                availability: crate::server::api::LaunchAvailability::Unknown,
                unavailable_reason: None,
                recent_project_directories: Vec::new(),
            });
            snapshot.capacity = vec![crate::server::ViewerTargetCapacity {
                id: "local".into(),
                label: "local".into(),
                target_ids: vec!["docker".into(), "podman".into(), "raw".into()],
                cpu_percent: Some(20),
                memory_used_bytes: None,
                memory_total_bytes: None,
                logical_cores: None,
                disk_total_bytes: None,
                virtual_machines: None,
                sampled_at_epoch_seconds: Some(1),
                refreshing: false,
                stale: false,
                has_error: false,
                storage: Vec::new(),
            }];
        },
        absent_preferences_path(),
        Arc::new(|kind| match kind {
            "local-docker" => Some(LocalEngineReadiness::NotInstalled),
            "local-podman" => Some(LocalEngineReadiness::Ready),
            _ => None,
        }),
    );

    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/options"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(response).await;
    let targets = body["targets"].as_array().unwrap();
    let target = |id: &str| targets.iter().find(|target| target["id"] == id).unwrap();

    assert_eq!(target("docker")["availability"], "unavailable");
    assert_eq!(
        target("docker")["unavailable_reason"],
        "Docker is not installed on this host"
    );
    assert_eq!(target("podman")["availability"], "ready");
    assert_eq!(target("raw")["availability"], "ready");
}

/// A runtime missing on the daemon's host is permanent, so the options say so
/// with `runtime_missing`; a host that did not answer is unavailable but not
/// missing, so callers keep it listed.
#[tokio::test]
async fn options_tell_a_missing_runtime_from_a_host_that_did_not_answer() {
    let (app, _, _, _) = api_app(Arc::new(FakeBackend::default()), |snapshot| {
        snapshot.targets.push(crate::server::ViewerTarget {
            id: "docker".into(),
            kind: "local-docker".into(),
            resource_allocation_kind: mj_core::state::ResourceAllocationKind::Container,
            requires_project_directory: false,
            remembered_container_size: None,
            container_host_limits: None,
            default_resource_allocation: None,
            runtime_missing: true,
            default_candidate: false,
            availability: crate::server::api::LaunchAvailability::Unknown,
            unavailable_reason: None,
            recent_project_directories: Vec::new(),
        });
        snapshot.capacity = vec![crate::server::ViewerTargetCapacity {
            id: "builder".into(),
            label: "builder".into(),
            target_ids: vec!["podman".into()],
            cpu_percent: None,
            memory_used_bytes: None,
            memory_total_bytes: None,
            logical_cores: None,
            disk_total_bytes: None,
            virtual_machines: None,
            sampled_at_epoch_seconds: None,
            refreshing: false,
            stale: false,
            has_error: true,
            storage: Vec::new(),
        }];
    });
    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/options"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(response).await;
    let targets = body["targets"].as_array().unwrap();
    let target = |id: &str| targets.iter().find(|target| target["id"] == id).unwrap();

    assert_eq!(target("docker")["availability"], "unavailable");
    assert_eq!(target("docker")["runtime_missing"], true);
    assert_eq!(target("podman")["availability"], "unavailable");
    assert!(target("podman").get("runtime_missing").is_none());
}

/// A target the daemon has no worker binary for is refused at admission with
/// the reason, before anything is launched (RVE-2); a target it does have one
/// for is admitted.
// Hard-won: a95e5e66: launch admission accepted targets that could not supply a worker.
#[tokio::test]
async fn naming_a_target_with_no_worker_binary_is_refused_with_the_reason() {
    let check: crate::server::WorkerSourceCheck = Arc::new(|target| {
        (target == "podman").then(|| {
            "worker source for aarch64 (Darwin) was unavailable when the daemon started: not built"
                .to_owned()
        })
    });
    let (app, mut actions, _snapshot_tx, _bundles) = api_app_with_worker_check(
        Arc::new(FakeBackend::default()),
        |_| {},
        absent_preferences_path(),
        Arc::new(|_| Some(crate::controller::LocalEngineReadiness::Ready)),
        Some(check),
    );
    let response = app
        .oneshot(start_request(
            r#"{"profile_id":"codex-1","target_id":"podman","bundle_id":"hel"}"#.into(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await.to_string();
    assert!(body.contains("podman"), "{body}");
    assert!(body.contains("aarch64 (Darwin)"), "{body}");
    assert!(body.contains("MJ_WORKER_DIR"), "{body}");
    assert!(actions.try_recv().is_err(), "nothing was launched");
}

#[tokio::test]
async fn options_report_the_saved_default_and_publish_only_its_two_identifiers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("go.json");
    mj_core::go::GoPreferences::save_recipe(
        &path,
        PathBuf::from("/private/project"),
        mj_core::go::GoRecipe {
            profile_id: "codex-1".into(),
            target_id: "podman".into(),
            bundle_id: Some("hel".into()),
            project_directory: Some(PathBuf::from("/private/project")),
            create_managed_worktree: None,
            subagents: None,
            additional_mounts: Vec::new(),
            resource_allocation: None,
        },
        true,
    )
    .unwrap();

    let (app, _, _, _) = api_app_with_preferences(Arc::new(FakeBackend::default()), |_| {}, path);
    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/options"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(response).await;

    assert_eq!(body["default"]["profile_id"], "codex-1");
    assert_eq!(body["default"]["target_id"], "podman");
    // A saved recipe also carries a bundle and a project directory. Those are
    // per-project choices and must not travel as part of the default.
    assert_eq!(body["default"].as_object().unwrap().len(), 2);
    assert!(!body.to_string().contains("/private/project"));
}

/// Save a fast-start default the way `mj go --global-default` does.
fn save_global_default(path: &std::path::Path, profile_id: &str, target_id: &str) {
    mj_core::go::GoPreferences::save_recipe(
        path,
        PathBuf::from("/private/project"),
        mj_core::go::GoRecipe {
            profile_id: profile_id.into(),
            target_id: target_id.into(),
            bundle_id: None,
            project_directory: None,
            create_managed_worktree: None,
            subagents: None,
            additional_mounts: Vec::new(),
            resource_allocation: None,
        },
        true,
    )
    .unwrap();
}

#[tokio::test]
async fn start_without_identifiers_uses_the_saved_default() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("go.json");
    save_global_default(&path, "codex-1", "podman");
    let (app, mut actions, _, _) =
        api_app_with_preferences(Arc::new(FakeBackend::default()), |_| {}, path.clone());

    let response = tokio::spawn(app.oneshot(start_request(r#"{"bundle_id":"hel"}"#.into())));
    let request = actions.recv().await.unwrap();
    let ControllerAction::New {
        profile_id,
        target_id,
        ..
    } = &request.action
    else {
        panic!("expected a New action, got {:?}", request.action);
    };
    // The controller always receives two explicit identifiers, whatever the
    // caller left out: a session whose profile was implicit would be a session
    // nobody could explain afterwards.
    assert_eq!(profile_id, "codex-1");
    assert_eq!(target_id, "podman");
    request
        .reply
        .send(ActionOutcome::Accepted {
            session_id: Some("session-2".into()),
        })
        .unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::CREATED
    );

    // The saved default names the bare target; an explicit request target
    // wins while only the omitted profile falls back to the same file.
    save_global_default(&path, "codex-1", "raw");
    let (app, mut actions, _, _) =
        api_app_with_preferences(Arc::new(FakeBackend::default()), |_| {}, path);
    let body = r#"{"target_id":"podman","bundle_id":"hel"}"#;
    let response = tokio::spawn(app.oneshot(start_request(body.into())));
    let request = actions.recv().await.unwrap();
    let ControllerAction::New {
        profile_id,
        target_id,
        ..
    } = &request.action
    else {
        panic!("expected a New action, got {:?}", request.action);
    };
    assert_eq!(profile_id, "codex-1", "the profile falls back");
    assert_eq!(target_id, "podman", "the named target wins");
    request
        .reply
        .send(ActionOutcome::Accepted {
            session_id: Some("session-2".into()),
        })
        .unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn start_explains_a_saved_default_that_names_a_missing_profile() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("go.json");
    save_global_default(&path, "gone", "podman");
    let (app, mut actions, _, _) =
        api_app_with_preferences(Arc::new(FakeBackend::default()), |_| {}, path);

    let response = app
        .oneshot(start_request(r#"{"bundle_id":"hel"}"#.into()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let message = json_body(response).await["error"]
        .as_str()
        .unwrap()
        .to_owned();
    // The caller named nothing, so "unknown profile" alone would read as
    // though it had.
    assert!(message.contains("saved default"), "{message}");
    assert!(message.contains("gone"), "{message}");
    assert!(actions.try_recv().is_err());
}

#[tokio::test]
async fn earlier_history_authenticates_validates_the_cursor_and_renders_stored_messages() {
    use mj_core::storage::{TranscriptCursor, TranscriptHistoryPage};
    let cursor = TranscriptCursor {
        position: 10,
        stable_id: "agent:10".into(),
    };
    let backend = Arc::new(FakeBackend {
        history: Some(TranscriptHistoryPage {
            items: vec![Arc::new(mj_core::state::TranscriptItem {
                stable_id: "agent:10".into(),
                position: 10,
                latest_content_event_ordinal: Some(20),
                created_at_ms: 10,
                last_changed_at_ms: 20,
                body: mj_core::state::TranscriptBody::Agent {
                    chunks: vec![
                        serde_json::json!({"content":{"type":"text", "text":"stored answer"}}),
                    ],
                    streaming: false,
                },
            })],
            before: Some(cursor.clone()),
            frontier: 20,
        }),
        ..FakeBackend::default()
    });
    let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});
    let unauthenticated = app
        .clone()
        .oneshot(
            Request::get("/api/v1/sessions/session-1/history")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    for url in [
        "/api/v1/sessions/session-1/history?before_position=10",
        "/api/v1/sessions/session-1/history?before_id=agent:10",
    ] {
        let response = app
            .clone()
            .oneshot(bearer(Request::get(url)).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .clone()
        .oneshot(
            bearer(Request::get(
                "/api/v1/sessions/session-1/history?before_position=10&before_id=agent:10",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["items"][0]["text"], "stored answer");
    assert_eq!(body["items"][0]["role"], "agent");
    assert_eq!(body["before"], serde_json::to_value(&cursor).unwrap());
    assert_eq!(body["frontier"], 20);
    assert_eq!(*backend.history_cursors.lock().unwrap(), vec![Some(cursor)]);
    let missing = app
        .oneshot(
            bearer(Request::get("/api/v1/sessions/missing/history"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

fn missing_target_access_error() -> ExportError {
    let record = crate::controller::test_support::checkpoint_test_session("session-1");
    ExportError::Failed(
        record
            .target_runtime_settings(&mj_core::config::Config::default())
            .unwrap_err()
            .context("resolve export target"),
    )
}

#[tokio::test]
async fn missing_target_access_returns_actionable_conflict_for_branch_and_bundle_exports() {
    for body in [
        r#"{"kind":"bundle"}"#,
        r#"{"kind":"branch","branch":"saved-work"}"#,
    ] {
        let (app, _actions, _snapshots, _bundles) = api_app(
            Arc::new(FakeBackend {
                target_access_missing: true,
                ..Default::default()
            }),
            |_| {},
        );
        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/export"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let error = body["error"].as_str().unwrap();
        assert!(
            error.contains("session-1") && error.contains("podman") && error.contains("Restore"),
            "{error}"
        );
    }
}

fn finished_child_turn(command_id: &str) -> MaterializedTurnOutcome {
    MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: command_id.into(),
        accepted_ordinal: Some(5),
        turn_start_position: Some(5),
        completed_ordinal: 9,
        completed_at_ms: 1,
        outcome: TurnOutcomeKind::Completed {
            stop_reason: "end_turn".into(),
        },
    }
}

/// A child's turn that ended without its report is not an ending, for a wait
/// on that turn or on the newest one: Mjolnir reminds the child, and the wait
/// answers after the reminder turn.
#[test]
fn a_session_wait_follows_a_child_through_its_handback_reminder() {
    let mut observation = WaitObservation {
        execution: MaterializedExecutionState::Idle,
        last_turn_outcome: Some(finished_child_turn("task")),
        ..WaitObservation::default()
    };
    observation.apply_subagent_report(true, &mj_core::subagent::SubagentReport::default(), 0);
    assert_eq!(observation.report_pending_for.as_deref(), Some("task"));
    assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());
    let on_the_turn = WaitRequest {
        turn_id: Some(5),
        ..WaitRequest::default()
    };
    assert!(resolve_wait(&observation, &on_the_turn).is_none());

    // The child handed back its report: the turn ends the wait.
    let mut delivered = WaitObservation {
        execution: MaterializedExecutionState::Idle,
        last_turn_outcome: Some(finished_child_turn("task")),
        ..WaitObservation::default()
    };
    delivered.apply_subagent_report(
        true,
        &mj_core::subagent::SubagentReport {
            handback: Some(mj_core::subagent::SubagentHandback {
                command_id: "task".into(),
                message: "the full report".into(),
                recorded_at_ms: 1,
            }),
            ..Default::default()
        },
        0,
    );
    assert!(delivered.report_pending_for.is_none());
    assert_eq!(
        resolve_wait(&delivered, &on_the_turn).map(|decision| decision.outcome),
        Some(WaitOutcome::Finished)
    );

    // A child without the tool ends its turn as any session does.
    let mut plain = WaitObservation {
        execution: MaterializedExecutionState::Idle,
        last_turn_outcome: Some(finished_child_turn("task")),
        ..WaitObservation::default()
    };
    plain.apply_subagent_report(false, &mj_core::subagent::SubagentReport::default(), 0);
    assert!(resolve_wait(&plain, &WaitRequest::default()).is_some());
}

/// `mj wait` on a child answers with what the child handed back, not the
/// turn's last message, and says so.
#[tokio::test]
async fn a_session_wait_on_a_child_answers_with_its_handback() {
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let session = ApiSession::from(&snapshot.sessions[0]);
    let backend: Arc<dyn SubagentBackend> = Arc::new(FakeBackend {
        summary: Some(TurnSummary {
            turn_number: 2,
            turn_started_at_ms: 100,
            last_changed_at_ms: 900,
            final_message: Some("Report delivered.".into()),
            tool_calls: 0,
        }),
        ..FakeBackend::default()
    });
    let observation = |report: mj_core::subagent::SubagentReport| {
        let mut observation = WaitObservation {
            execution: MaterializedExecutionState::Idle,
            last_turn_outcome: Some(finished_child_turn("task")),
            ..WaitObservation::default()
        };
        observation.apply_subagent_report(true, &report, 0);
        observation
    };
    let answer = |observation: WaitObservation| {
        let backend = backend.clone();
        let session = session.clone();
        async move {
            let decision = resolve_wait(&observation, &WaitRequest::default()).unwrap();
            finish_wait(&backend, "session-1", session, observation, decision, None)
                .await
                .unwrap()
        }
    };

    let response = answer(observation(mj_core::subagent::SubagentReport {
        handback: Some(mj_core::subagent::SubagentHandback {
            command_id: "task".into(),
            message: "the full report".into(),
            recorded_at_ms: 1,
        }),
        ..Default::default()
    }))
    .await;
    assert_eq!(response.final_message.as_deref(), Some("the full report"));
    assert_eq!(response.report_source.as_deref(), Some("handback"));

    // The reminder could not be sent: the last message stands, and says so.
    let response = answer(observation(mj_core::subagent::SubagentReport {
        reminder_failed_for: Some("task".into()),
        ..Default::default()
    }))
    .await;
    assert_eq!(response.final_message.as_deref(), Some("Report delivered."));
    assert_eq!(response.report_source.as_deref(), Some("last_message"));
}

#[test]
fn both_wait_forms_use_recorded_jev_outcomes_and_targeted_wait_ignores_later_activity() {
    use mj_core::activity::{
        ActivityState,
        verdict::{Decision, TurnCompletion},
    };
    let mut observation = idle(Some(completed(7, "end_turn")));
    let targeted = WaitRequest {
        turn_id: Some(7),
        ..Default::default()
    };
    for (decision, expected) in [
        (Decision::ExpectContinuation, None),
        (Decision::InferIdle, Some(WaitOutcome::Finished)),
        (Decision::AwaitingInput, Some(WaitOutcome::InputRequired)),
    ] {
        observation.turn_completion = Some(TurnCompletion {
            command_id: "prompt-7".into(),
            completed_ordinal: 20,
            decision,
        });
        for request in [&WaitRequest::default(), &targeted] {
            assert_eq!(
                resolve_wait(&observation, request).map(|d| d.outcome),
                expected
            );
        }
        if expected.is_some() {
            assert_eq!(
                resolve_wait(&observation, &targeted)
                    .unwrap()
                    .turn
                    .unwrap()
                    .completed_position,
                20,
                "the returned summary includes the continuation Jev judged"
            );
        }
        // The completion record belongs to this command, not the current
        // activity. Later work must not reinterpret a settled target.
        observation.activity = ActivityState::Goal;
        assert_eq!(
            resolve_wait(&observation, &targeted).map(|d| d.outcome),
            expected
        );
        assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());
        observation.activity = ActivityState::default();
    }
    observation.turn_completion.as_mut().unwrap().command_id = "different-command".into();
    observation.activity = ActivityState::Expecting { since_ms: 20 };
    assert!(resolve_wait(&observation, &targeted).is_none());
}

/// A live actor that has not delivered its first projection, so a wait reads
/// the session's turn state from the store on every decision.
fn actor_without_projection() -> ManagedSessionView {
    ManagedSessionView {
        snapshot: None,
        connected: true,
        error: None,
    }
}

/// Copy the fixture session as a second one, which the tests' revisions
/// change while the first is waited on.
fn add_second_session(snapshot: &mut ViewerSnapshot) {
    let mut other = snapshot.sessions[0].clone();
    other.id = "session-2".into();
    snapshot.sessions.push(other);
}

fn spawn_wait(
    app: axum::Router,
    body: &'static str,
) -> tokio::task::JoinHandle<Result<Response, std::convert::Infallible>> {
    tokio::spawn(
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/wait"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        ),
    )
}

/// The controller republishes its snapshot for every revision of any session.
/// A waiter on an idle session must not decide again, or read the store, for
/// a revision that leaves its own session unchanged: twenty idle `mj wait`
/// callers beside three streaming sessions kept the daemon above two cores.
/// It must still answer as soon as its own session asks for input.
// Hard-won: f5c83878: waiters opened the database repeatedly on unrelated session changes.
#[tokio::test(start_paused = true)]
async fn a_waiter_reads_nothing_for_other_sessions_revisions_and_answers_its_own() {
    let backend = Arc::new(FakeBackend {
        live_view: Some(actor_without_projection()),
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Running { started_at_ms: 10 },
            active_turn: Some(MaterializedTurn {
                command_id: "prompt-1".into(),
                accepted_ordinal: Some(5),
                turn_start_position: 6,
                started_at_ms: 10,
                steered_into: None,
            }),
            last_turn_outcome: None,
        })]),
        ..FakeBackend::default()
    });
    let (app, _actions, snapshot_tx, _bundles) = api_app(backend.clone(), add_second_session);
    let reads = || {
        backend
            .turn_state_reads
            .load(std::sync::atomic::Ordering::SeqCst)
    };
    super::wait::SKIPPED_WAIT_PASSES.with(|skipped| skipped.set(0));
    let waiter = spawn_wait(app, r#"{"return_on_input":true,"timeout_secs":600}"#);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(reads(), 1, "the first pass decides");

    for revision in 0..50 {
        snapshot_tx.send_modify(|snapshot| {
            snapshot.revision += 1;
            snapshot.sessions[1].title = format!("streaming {revision}");
            snapshot.sessions[1].latest_event_ordinal = revision;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(reads(), 1, "other sessions' revisions read nothing");
    assert_eq!(
        backend
            .startup_reads
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        backend
            .report_reads
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        super::wait::SKIPPED_WAIT_PASSES.with(std::cell::Cell::get),
        50,
        "every publication woke the waiter, and none needed a decision"
    );
    assert!(!waiter.is_finished());

    snapshot_tx.send_modify(|snapshot| {
        snapshot.revision += 1;
        snapshot.sessions[0].pending_elicitations = vec![input_request()];
    });
    let response = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the waiter answers its own session's change at once")
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["outcome"], "input_required");
    assert_eq!(reads(), 2);
}

// Hard-won: 396e2d49: a committed turn change was filtered before its viewer row changed.
#[tokio::test(start_paused = true)]
async fn a_waiter_answers_a_durable_turn_change_before_the_viewer_row_changes() {
    let backend = Arc::new(FakeBackend {
        live_view: Some(actor_without_projection()),
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Running { started_at_ms: 10 },
            active_turn: Some(MaterializedTurn {
                command_id: "task".into(),
                accepted_ordinal: Some(5),
                turn_start_position: 6,
                started_at_ms: 10,
                steered_into: None,
            }),
            last_turn_outcome: None,
        })]),
        summary: Some(TurnSummary {
            turn_number: 1,
            turn_started_at_ms: 10,
            last_changed_at_ms: 900,
            final_message: Some("Completed.".into()),
            tool_calls: 0,
        }),
        ..FakeBackend::default()
    });
    let (app, _actions, snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});
    let waiter = spawn_wait(app, r#"{"turn_id":5,"timeout_secs":600}"#);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiter.is_finished());
    *backend.turn_states.lock().unwrap() = vec![Some(TurnState {
        execution: MaterializedExecutionState::Idle,
        active_turn: None,
        last_turn_outcome: Some(finished_child_turn("task")),
    })];
    backend
        .wait_revision
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    snapshot_tx.send_modify(|snapshot| snapshot.revision += 1);
    let response = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the committed turn ends the wait without a changed viewer row")
        .unwrap()
        .unwrap();
    assert_eq!(json_body(response).await["outcome"], "finished");
    assert_eq!(
        backend
            .turn_state_reads
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

/// A child's report lands in the durable records, which republish the
/// snapshot without changing the child's row. The waiter must decide again
/// then, and answer with the report.
#[tokio::test(start_paused = true)]
async fn a_waiter_answers_when_its_child_s_report_lands() {
    let backend = Arc::new(FakeBackend {
        live_view: Some(actor_without_projection()),
        turn_states: Mutex::new(vec![Some(TurnState {
            execution: MaterializedExecutionState::Idle,
            active_turn: None,
            last_turn_outcome: Some(finished_child_turn("task")),
        })]),
        summary: Some(TurnSummary {
            turn_number: 1,
            turn_started_at_ms: 100,
            last_changed_at_ms: 900,
            final_message: Some("Report delivered.".into()),
            tool_calls: 0,
        }),
        child_report: Mutex::new(Some((true, mj_core::subagent::SubagentReport::default()))),
        ..FakeBackend::default()
    });
    let (app, _actions, snapshot_tx, _bundles) = api_app(backend.clone(), |snapshot| {
        add_second_session(snapshot);
        snapshot.sessions[0].activity_state =
            Some(mj_core::activity::ActivityState::Idle { since_ms: None });
    });
    let waiter = spawn_wait(app, r#"{"turn_id":5,"timeout_secs":600}"#);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiter.is_finished(), "the child still owes its report");

    *backend.child_report.lock().unwrap() = Some((
        true,
        mj_core::subagent::SubagentReport {
            handback: Some(mj_core::subagent::SubagentHandback {
                command_id: "task".into(),
                message: "the full report".into(),
                recorded_at_ms: 1,
            }),
            ..Default::default()
        },
    ));
    backend
        .wait_revision
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    snapshot_tx.send_modify(|snapshot| snapshot.revision += 1);
    let response = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the landed report ends the wait")
        .unwrap()
        .unwrap();
    let body = json_body(response).await;
    assert_eq!(body["outcome"], "finished");
    assert_eq!(body["final_message"], "the full report");
    assert_eq!(body["report_source"], "handback");
}

/// A review that cannot start answers 409 with the reason written for the
/// person, not a 500 behind a log reference.
// Hard-won: 6b96d591: a review that could not start was returned as a 500 behind a log reference.
#[tokio::test]
async fn a_review_that_cannot_start_is_a_conflict_with_its_reason() {
    let (app, mut actions, _snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), |_| {});
    let response = tokio::spawn(
        app.oneshot(
            bearer(Request::post("/api/v1/sessions/session-1/review"))
                .body(Body::empty())
                .unwrap(),
        ),
    );
    let request = actions.recv().await.unwrap();
    request
        .reply
        .send(ActionOutcome::Refused(
            mj_core::refusal::Refusal::precondition(
                "prompts are queued; the review waits for them",
            ),
        ))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await;
    assert_eq!(
        body["error"],
        "prompts are queued; the review waits for them"
    );
}

#[tokio::test]
async fn review_status_reports_the_open_review_or_none() {
    let (app, _actions, snapshot_tx, _bundles) = api_app(Arc::new(FakeBackend::default()), |_| {});
    let get = || {
        bearer(Request::get("/api/v1/sessions/session-1/review"))
            .body(Body::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(get()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["session_id"], "session-1");
    assert!(body["review"].is_null(), "{body}");

    snapshot_tx.send_modify(|snapshot| {
        let mut session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == "session-1")
            .unwrap()
            .clone();
        session.turn_review = Some(crate::server::ViewerTurnReview {
            tier: "quick".into(),
            status: "sending findings to the primary agent…".into(),
            roles: vec![crate::server::ViewerReviewRole {
                label: "reviewer".into(),
                state: "findings".into(),
            }],
            verdict: Some(crate::server::ViewerReviewVerdict {
                kind: "findings".into(),
                text: "[P1] src/lib.rs:1 -- unbounded retry loop".into(),
                allowed: vec!["forward".into(), "dismiss".into(), "cancel".into()],
            }),
        });
        snapshot.sessions.push(session);
    });
    let body = json_body(app.clone().oneshot(get()).await.unwrap()).await;
    assert_eq!(body["review"]["tier"], "quick");
    assert_eq!(body["review"]["roles"][0]["state"], "findings");
    assert_eq!(body["review"]["verdict"]["kind"], "findings");

    let response = app
        .oneshot(
            bearer(Request::get("/api/v1/sessions/no-such-session/review"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Dismissing, cancelling, or forwarding sends the same controller action as
/// the viewer's buttons. A resolution the open review does not allow, or no
/// open review, is refused up front (400); a refusal the daemon reaches
/// itself, such as a review that closed meanwhile, is a 409 with its reason.
#[tokio::test]
async fn resolving_a_review_sends_the_controller_action_or_explains_the_refusal() {
    let (app, mut actions, snapshot_tx, _bundles) =
        api_app(Arc::new(FakeBackend::default()), |_| {});
    let post = |path: &str| {
        bearer(Request::post(path.to_owned()))
            .body(Body::empty())
            .unwrap()
    };
    async fn next(actions: &mut mpsc::Receiver<ControllerRequest>) -> ControllerRequest {
        tokio::time::timeout(std::time::Duration::from_secs(5), actions.recv())
            .await
            .expect("the action reaches the controller")
            .unwrap()
    }

    let response = app
        .clone()
        .oneshot(post("/api/v1/sessions/session-1/review/dismiss"))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "no review is open"
    );

    snapshot_tx.send_modify(|snapshot| {
        let mut session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == "session-1")
            .unwrap()
            .clone();
        session.turn_review = Some(crate::server::ViewerTurnReview {
            tier: "quick".into(),
            status: "the review failed: relay reviewer_start timed out after 300 seconds".into(),
            roles: Vec::new(),
            verdict: Some(crate::server::ViewerReviewVerdict {
                kind: "failed".into(),
                text: "relay reviewer_start timed out after 300 seconds".into(),
                allowed: vec!["dismiss".into(), "cancel".into()],
            }),
        });
        snapshot.sessions.push(session);
    });

    let response = tokio::spawn(
        app.clone()
            .oneshot(post("/api/v1/sessions/session-1/review/dismiss")),
    );
    let request = next(&mut actions).await;
    assert_eq!(
        request.action,
        ControllerAction::ResolveReview {
            session_id: "session-1".into(),
            resolution: "dismiss".into(),
        }
    );
    request.reply.send(ActionOutcome::accepted()).unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(json_body(response).await["resolution"], "dismiss");

    let response = app
        .clone()
        .oneshot(post("/api/v1/sessions/session-1/review/forward"))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a failed review has no findings to forward"
    );

    let response = tokio::spawn(
        app.clone()
            .oneshot(post("/api/v1/sessions/session-1/review/cancel")),
    );
    let request = next(&mut actions).await;
    request
        .reply
        .send(ActionOutcome::Refused(
            mj_core::refusal::Refusal::precondition("no review is open for that session"),
        ))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(response).await["error"],
        "no review is open for that session"
    );

    let response = app
        .oneshot(post("/api/v1/sessions/session-1/review/approve"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
