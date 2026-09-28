use super::*;
use crate::session_manager::{
    RelaySessionTarget, RemoteSessionRequest, RemoteSessionRequests,
    spawn_reply_fixture_session_manager,
};
use mj_core::review::driver::{RoleState, RoleStatus};
use mj_core::state::{ManagedSessionSnapshot, MaterializedSession};

use mj_core::relay::{
    RELAY_EVENT_FORMAT_V1, RelayCommandOutcome, RelayOperationalState, relay_event_digest,
};

/// One session id per test. The prompt lock is process-wide -- there is
/// one daemon per machine and one host in it -- so tests that shared a
/// session id would release each other's locks.
fn session_id(test: &str) -> String {
    format!("018f9dd2-a3b4-7c8d-9000-{test}")
}

#[test]
fn review_activity_follows_typed_transitions_without_reading_progress_prose() {
    let mut view = RuntimeReviewView {
        session_id: "activity".to_owned(),
        tier: ReviewTier::Quick,
        phase: TurnReviewPhase::LaunchingReviewer,
        roles: Vec::new(),
        status: "validating configuration".to_owned(),
        verdict: None,
    };
    assert_eq!(view.activity_label(), Some("Reviewing"));
    assert!(view.is_working());
    view.phase = TurnReviewPhase::Running {
        roles: vec![RoleStatus {
            role: mj_core::review::driver::VALIDATOR_ROLE.to_owned(),
            label: "Validator".to_owned(),
            state: RoleState::Running,
        }],
    };
    view.status = "checking source".to_owned();
    assert_eq!(view.activity_label(), Some("Validating"));
    assert!(view.is_working());
    view.phase = TurnReviewPhase::Verdict(ReviewVerdict::Findings {
        synthesis: "[P2] app.py:1 -- incorrect bounds".to_owned(),
        evidence: Default::default(),
    });
    assert_eq!(view.activity_label(), Some("Findings"));
    assert!(!view.is_working());
    view.phase = TurnReviewPhase::Verdict(ReviewVerdict::Failed {
        reason: "reviewer unavailable".to_owned(),
    });
    assert_eq!(view.activity_label(), Some("Review failed"));
    assert!(!view.is_working());
    view.phase = TurnReviewPhase::Forwarding {
        synthesis: "findings".into(),
        evidence: Default::default(),
        command_id: "forward".into(),
        error: None,
    };
    assert!(view.is_working());
    if let TurnReviewPhase::Forwarding { error, .. } = &mut view.phase {
        *error = Some("relay unavailable".into());
    }
    assert!(!view.is_working());
    view.phase = TurnReviewPhase::Resolved(Resolution::Cancelled);
    assert_eq!(view.activity_label(), None);
    assert!(!view.is_working());
}

/// I1-14: a review that could not start because a lifecycle operation held
/// the session said only "session is reserved for a lifecycle operation".
#[test]
fn a_review_that_cannot_start_says_so_in_plain_words() {
    assert_eq!(
        start_refusal_notice("session is reserved for a lifecycle operation"),
        "Turn review did not start: another operation was using the session. \
         The next review covers these changes."
    );
}

#[test]
fn resolution_notices_keep_the_verdict_context_after_close() {
    let resolved_dismissed = TurnReviewPhase::Resolved(Resolution::Dismissed);
    assert_eq!(
        resolution_notice(&resolved_dismissed, Some(&ReviewVerdict::Clean)),
        Some("Review complete: no material findings".to_owned())
    );
    assert_eq!(
        resolution_notice(
            &TurnReviewPhase::Resolved(Resolution::Cancelled),
            Some(&ReviewVerdict::Failed {
                reason: "harness failed".to_owned(),
            }),
        ),
        Some("Review failed; the change stays unreviewed".to_owned())
    );
    assert_eq!(
        resolution_notice(
            &resolved_dismissed,
            Some(&ReviewVerdict::Findings {
                synthesis: "[P1] broken".to_owned(),
                evidence: Default::default(),
            }),
        ),
        Some("Review dismissed".to_owned())
    );
}

fn user_prompt(position: u64, text: &str) -> Arc<mj_core::state::TranscriptItem> {
    Arc::new(mj_core::state::TranscriptItem {
        stable_id: format!("user:{position}"),
        position,
        latest_content_event_ordinal: None,
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: mj_core::state::TranscriptBody::User {
            content: vec![serde_json::json!({
                "type": "text",
                "text": text,
            })],
        },
    })
}

#[test]
fn seed_uses_the_latest_real_prompt_and_keeps_history_for_intent() {
    let mut session = MaterializedSession::empty("seed-prompts");
    session.applied_event_ordinal = 5;
    session.transcript = vec![
        user_prompt(1, "implement the old parser"),
        user_prompt(2, "support parse_range"),
        user_prompt(3, "[HARNESS NOTE: review the parser]"),
        user_prompt(4, "also finish the parser error path"),
        user_prompt(5, "[HARNESS NOTE: forwarded findings]"),
    ];
    let mut state = TurnReviewState {
        reviewed_through_ordinal: 1,
        ..TurnReviewState::default()
    };

    let seed = seed_from_session(&session, ReviewTier::Extended, &state, "manual");
    assert_eq!(seed.task, "also finish the parser error path");
    assert_eq!(
        seed.user_messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "implement the old parser",
            "support parse_range",
            "also finish the parser error path",
        ],
        "intent receives real prompts in chronological order"
    );
    assert!(!seed.trajectory.contains("HARNESS NOTE"));

    // A corrective-only pass still has no new user prompt, but retains the
    // latest real prompt as its current outer task.
    state.reviewed_through_ordinal = 5;
    let corrective = seed_from_session(&session, ReviewTier::Extended, &state, "manual");
    assert_eq!(corrective.task, "also finish the parser error path");
    assert_eq!(corrective.user_messages.len(), 3);
}

/// An idle reviewer's operational state. Built through serde because the
/// struct's own constructor belongs to the relay.
fn operational() -> RelayOperationalState {
    serde_json::from_value(serde_json::json!({
        "session_id": "reviewer",
        "relay_protocol_version": mj_core::relay::RELAY_PROTOCOL_VERSION,
        "execution": "idle",
        "latest_ordinal": 0,
        "latest_digest": mj_core::relay::RELAY_EVENT_GENESIS_DIGEST,
        "acknowledged_through": 0,
        "acknowledged_digest": mj_core::relay::RELAY_EVENT_GENESIS_DIGEST,
        "recovery_floor_ordinal": 0,
        "recovery_floor_digest": mj_core::relay::RELAY_EVENT_GENESIS_DIGEST,
        "native_session_id": null,
        "agent_capabilities": null,
        "agent_info": null,
        "config_options": [],
        "available_commands": [],
        "config": {},
        "active_prompt": null,
        "queued_prompts": [],
        "checkpoint_barrier": null,
        "checkpoint_ready": null,
    }))
    .expect("the operational state fixture matches its schema")
}

/// A session manager whose requests the test answers itself.
///
/// This is the production remote-manager plumbing with the daemon end
/// replaced by the test: `control` is exactly what the daemon hands the
/// host, and every reviewer action the host makes arrives here as a
/// request to answer, so the host is exercised through its real interface.
struct FakeManager {
    session: String,
    control: SessionManagerControl,
    requests: RemoteSessionRequests,
    publisher: crate::session_manager::RemoteSessionPublisher,
    /// Refusals the next captures answer with, in order, the way a lease
    /// the recovery copy holds refuses a reviewer action.
    capture_refusals: std::collections::VecDeque<String>,
    _shutdown: crate::session_manager::SessionManagerShutdown,
    _targets: tokio::sync::watch::Sender<Vec<RelaySessionTarget>>,
}

impl FakeManager {
    /// Builds the manager and waits until it is managing the session, so
    /// the host's first request cannot race the actor's creation.
    async fn new(session: &str) -> Self {
        let channels = spawn_reply_fixture_session_manager().expect("remote manager");
        // The target is never dialled: this manager forwards every
        // request to the test instead of to a worker.
        channels.targets.send_replace(vec![RelaySessionTarget {
            session_id: session.to_owned(),
            spec: crate::targets::CommandSpec::new("true", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let manager = Self {
            session: session.to_owned(),
            control: channels.control,
            requests: channels.requests,
            publisher: channels.publisher,
            capture_refusals: std::collections::VecDeque::new(),
            _shutdown: channels.shutdown,
            _targets: channels.targets,
        };
        // The remote manager creates an actor for a session once a view
        // has been published for it, which is what the daemon does with
        // every session it owns.
        manager
            .publisher
            .publish(
                session.to_owned(),
                view(session, mj_core::state::MaterializedExecutionState::Idle),
            )
            .await
            .expect("publish the first view");
        manager
            .control
            .wait_for_session(session, Duration::from_secs(5))
            .await
            .expect("the fake manager manages the session");
        manager
    }

    fn refuse_next_capture(&mut self, reason: &str) {
        self.capture_refusals.push_back(reason.to_owned());
    }

    /// The next request the host makes, or a failure if it makes none.
    async fn next(&mut self) -> RemoteSessionRequest {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let request = self
                    .requests
                    .recv()
                    .await
                    .expect("the manager is still running");
                if let RemoteSessionRequest::Sync { reply, .. } = request {
                    let _ = reply.send(Ok(()));
                } else {
                    return request;
                }
            }
        })
        .await
        .expect("the host makes a request")
    }

    async fn drive_until(&mut self, done: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::select! {
                    request = self.requests.recv() => match request.expect("manager is alive") {
                        RemoteSessionRequest::Reviewer { action, reply, .. } => { let _ = reply.send(answer_for(&action)); }
                        RemoteSessionRequest::Sync { reply, .. } => { let _ = reply.send(Ok(())); }
                        RemoteSessionRequest::Submit { command: RelayCommand::RecordNotice { .. }, reply, .. } => { let _ = reply.send(Ok(1)); }
                        other => panic!("unexpected request {}", other.session_id()),
                    },
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                }
            }
        }).await.expect("worker control effects settle");
    }

    /// Answers reviewer actions until one matches `wanted`, which is then
    /// returned unanswered for the test to answer itself. A capture is
    /// refused first while [`Self::refuse_next_capture`] has queued one.
    async fn next_reviewer(
        &mut self,
        wanted: impl Fn(&Option<String>, &ReviewerAction) -> bool,
    ) -> (
        Option<String>,
        ReviewerAction,
        oneshot::Sender<Result<ReviewerOutcome, String>>,
    ) {
        loop {
            match self.next().await {
                RemoteSessionRequest::Reviewer {
                    role,
                    action,
                    reply,
                    ..
                } => {
                    if matches!(action, ReviewerAction::CaptureDelta { .. })
                        && let Some(refusal) = self.capture_refusals.pop_front()
                    {
                        let _ = reply.send(Err(refusal));
                        continue;
                    }
                    if wanted(&role, &action) {
                        return (role, action, reply);
                    }
                    // Anything else the host asks for on the way is
                    // answered plausibly so the review keeps moving.
                    let _ = reply.send(answer_for(&action));
                }
                RemoteSessionRequest::Submit { reply, .. } => {
                    let _ = reply.send(Ok(1));
                }
                other => panic!("unexpected request {}", other.session_id()),
            }
        }
    }
}

/// A plausible answer to any reviewer action, for the steps a test is not
/// asserting on.
fn answer_for(action: &ReviewerAction) -> Result<ReviewerOutcome, String> {
    match action {
        ReviewerAction::Status => Ok(ReviewerOutcome::Status(Box::new(operational()))),
        ReviewerAction::CaptureDelta { .. } => Ok(ReviewerOutcome::Delta {
            repositories: Vec::new(),
        }),
        ReviewerAction::AnalyzeDelta { .. } => Ok(ReviewerOutcome::ChangedFunctions {
            packet: "- edited retry()".to_owned(),
        }),
        ReviewerAction::AdvanceBaseline { .. } => Ok(ReviewerOutcome::BaselineAdvanced),
        ReviewerAction::ReviewerCommandReceipt { .. }
        | ReviewerAction::CancelReviewerCommandAdmission { .. } => {
            Ok(ReviewerOutcome::CommandReceipt { receipt: None })
        }
        ReviewerAction::ReleaseReviewerCommandReceipt { .. } => {
            Ok(ReviewerOutcome::CommandReceiptReleased)
        }
        ReviewerAction::ReadLaneDispatches => Ok(ReviewerOutcome::PendingLaneDispatches {
            dispatches: Vec::new(),
        }),
        ReviewerAction::AckLaneDispatches { .. } => Ok(ReviewerOutcome::LaneDispatchesAcknowledged),
        ReviewerAction::TakeLaneDispatches => Ok(ReviewerOutcome::LaneDispatches {
            requests: Vec::new(),
        }),
        ReviewerAction::Attach { .. } => Ok(ReviewerOutcome::Attached(Box::new(
            crate::worker_client::RelayAttachment {
                state: operational(),
                events: Vec::new(),
                through_ordinal: 0,
                through_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            },
        ))),
        ReviewerAction::Pause | ReviewerAction::PauseGeneration { .. } => {
            Ok(ReviewerOutcome::Paused)
        }
        ReviewerAction::Submit { .. } | ReviewerAction::SubmitDurable { .. } => {
            Ok(ReviewerOutcome::Accepted { ordinal: 1 })
        }
        ReviewerAction::Start { .. } => Err("no harness in this test".to_owned()),
        ReviewerAction::RespondElicitation { .. } => Ok(ReviewerOutcome::ElicitationResolved),
        ReviewerAction::Acknowledge { .. } => {
            Ok(ReviewerOutcome::Acknowledged(mj_core::relay::RelayCursor {
                ordinal: 0,
                digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            }))
        }
    }
}

/// A view of a turn that is answering a prompt, which is the kind an
/// automatic review is armed by.
fn view(
    session: &str,
    execution: mj_core::state::MaterializedExecutionState,
) -> ManagedSessionView {
    view_of(session, execution, true)
}

/// `prompt_driven` false is a turn the harness started on its own: it runs
/// and goes idle with no prompt of ours in flight.
fn view_of(
    session: &str,
    execution: mj_core::state::MaterializedExecutionState,
    prompt_driven: bool,
) -> ManagedSessionView {
    let mut materialized = MaterializedSession::empty(session);
    materialized.execution = execution;
    materialized.applied_event_ordinal = 12;
    let mut operational = operational();
    if prompt_driven
        && matches!(
            execution,
            mj_core::state::MaterializedExecutionState::Running { .. }
        )
    {
        operational.active_prompt = Some(mj_core::relay::ActiveRelayPrompt {
            command_id: "prompt-1".to_owned(),
            created_at_ms: 0,
            started_at_ms: 0,
        });
    }
    ManagedSessionView {
        snapshot: Some(ManagedSessionSnapshot {
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

/// A controller that says yes: the profile exists and the session is
/// reviewable. Staging answers with a launch config rather than copying a
/// profile onto a target, so a whole review runs without one.
struct FakeEnvironment {
    staged: Mutex<Vec<(String, u64, bool)>>,
    /// The review bookkeeping, in memory rather than in the developer's
    /// own database.
    state: Mutex<TurnReviewState>,
    writes: Mutex<Vec<(TurnReviewState, std::thread::ThreadId)>>,
    save_gate: Mutex<Option<Arc<SaveGate>>>,
    subagent: std::sync::atomic::AtomicBool,
    /// Refusals the next reviewer resolutions answer with, in order.
    resolve_refusals: Mutex<std::collections::VecDeque<String>>,
    /// How many times the host waited for background work on the session.
    background_waits: std::sync::atomic::AtomicUsize,
    /// Set to make reviewer resolution wait forever, the way an Auto
    /// reviewer choice can take minutes on a real host (I2-10).
    resolve_hangs: std::sync::atomic::AtomicBool,
}

struct SaveGate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    released_changed: std::sync::Condvar,
}

impl SaveGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            released_changed: std::sync::Condvar::new(),
        })
    }

    async fn entered(&self) {
        self.entered.notified().await;
    }

    fn wait(&self) {
        self.entered.notify_one();
        let released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.released_changed
                .wait_while(released, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.released_changed.notify_all();
    }
}

impl FakeEnvironment {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            staged: Mutex::new(Vec::new()),
            state: Mutex::new(TurnReviewState::default()),
            writes: Mutex::new(Vec::new()),
            save_gate: Mutex::new(None),
            subagent: std::sync::atomic::AtomicBool::new(false),
            resolve_refusals: Mutex::new(std::collections::VecDeque::new()),
            background_waits: std::sync::atomic::AtomicUsize::new(0),
            resolve_hangs: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn refuse_next_resolve(&self, reason: &str) {
        self.resolve_refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(reason.to_owned());
    }

    fn background_waits(&self) -> usize {
        self.background_waits
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn state(&self) -> TurnReviewState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn staged_roles(&self) -> Vec<(String, u64, bool)> {
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn writes(&self) -> Vec<(TurnReviewState, std::thread::ThreadId)> {
        self.writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn block_saves(&self) -> Arc<SaveGate> {
        let gate = SaveGate::new();
        *self
            .save_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate.clone());
        gate
    }
}

impl ReviewEnvironment for FakeEnvironment {
    fn check(&self, _session_id: &str, _profile: &str) -> Result<(), String> {
        Ok(())
    }

    fn is_subagent(&self, _session_id: &str) -> bool {
        self.subagent.load(std::sync::atomic::Ordering::Acquire)
    }

    fn resolve<'a>(
        &'a self,
        _handle: ManagedSessionHandle,
        config: ReviewConfig,
        _cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    > {
        let refusal = self
            .resolve_refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        Box::pin(async move {
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
            if self
                .resolve_hangs
                .load(std::sync::atomic::Ordering::Acquire)
            {
                std::future::pending::<()>().await;
            }
            Ok(mj_core::review::settings::ResolvedReviewSettings {
                profile: config.profile.unwrap_or_else(|| "auto-reviewer".into()),
                main: mj_core::review::settings::ReviewModelSettings {
                    model: config.model,
                    effort: config.effort,
                    fast_mode: false,
                },
                ..Default::default()
            })
        })
    }

    fn stage(
        &self,
        _session_id: &str,
        profile: &str,
        generation: u64,
        mcp_servers: &[mj_core::worker_launch::ReviewMcpServer],
        dispatch_tool: bool,
    ) -> Result<mj_core::worker_launch::ReviewerLaunchConfig, String> {
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((profile.to_owned(), generation, dispatch_tool));
        Ok(mj_core::worker_launch::ReviewerLaunchConfig {
            profile_id: profile.to_owned(),
            harness: mj_core::config::HarnessKind::Claude,
            bridge_command: std::path::PathBuf::from("/bin/false"),
            bridge_args: Vec::new(),
            environment: Default::default(),
            excluded_environment: Vec::new(),
            execution_policy: mj_core::config::ExecutionPolicy::ConfiguredApprovals,
            model: None,
            effort: None,
            fast_mode: None,
            generation,
            mcp_servers: mcp_servers.to_vec(),
        })
    }

    fn load_state(&self, _session_id: &str) -> Result<TurnReviewState, String> {
        Ok(self.state())
    }

    fn save_state(&self, _session_id: &str, state: &TurnReviewState) -> Result<(), String> {
        let gate = self
            .save_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.clone();
        self.writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((state.clone(), std::thread::current().id()));
        Ok(())
    }

    fn recoverable_reviews(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn background_work_settled<'a>(
        &'a self,
        _session_id: &'a str,
        _deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, ()> {
        self.background_waits
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Box::pin(async {})
    }
}

/// R4-9: a turn that ended with a command still running in the background
/// also started the automatic recovery copy, which took the session's lease
/// while the review was choosing its reviewer. The review gave up with "Turn
/// review did not start: another operation was using the session". It now
/// waits for that background work and tries again, within a bound.
#[tokio::test]
async fn a_review_waits_for_the_recovery_copy_instead_of_giving_up() {
    let session = session_id("waitforcopy0");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    environment.refuse_next_resolve("reviewer operation cancelled for session lifecycle change");
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    finish_a_turn(&manager, &host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));

    // Preparation captures the change before it chooses a reviewer (I2-10).
    // Only a turn that changed something goes on to choose one.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert_eq!(
        environment.background_waits(),
        1,
        "the capture waited for background work, and no reviewer choice has"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));

    // The copy's lease cancels the first choice. The review waits, chooses
    // again, opens, and starts work on the capture.
    let (_, action, _reply) = manager
        .next_reviewer(|_, action| {
            matches!(
                action,
                ReviewerAction::AnalyzeDelta { .. } | ReviewerAction::Start { .. }
            )
        })
        .await;
    assert!(matches!(
        action,
        ReviewerAction::AnalyzeDelta { .. } | ReviewerAction::Start { .. }
    ));
    assert_eq!(
        environment.background_waits(),
        3,
        "after the capture's wait, the refused choice and its retry each waited"
    );
    assert!(
        host.view(session)
            .is_none_or(|view| !view.status.contains("did not start")),
        "{:?}",
        host.view(session)
    );
    host.shutdown().await.unwrap();
}

/// A refusal that has nothing to do with another operation is not retried.
#[tokio::test]
async fn a_review_refused_for_another_reason_does_not_wait() {
    let session = session_id("nowaitrefuse");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    environment.refuse_next_resolve("no reviewer profile is enabled");
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    finish_a_turn(&manager, &host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));
    // Preparation captures the change before it chooses a reviewer (I2-10).
    // Only a turn that changed something reaches the refused choice.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.refuses_prompt(session) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the refused review releases prompts");
    assert_eq!(
        environment.background_waits(),
        2,
        "one wait before the capture and one before the only choice"
    );
    host.shutdown().await.unwrap();
}

fn armed(profile: Option<&str>) -> ReviewConfigSource {
    let profile = profile.map(str::to_owned);
    Arc::new(move || ReviewConfig {
        enabled: true,
        tier: ReviewTier::Quick,
        profile: profile.clone(),
        model: None,
        effort: None,
    })
}

#[test]
fn the_primary_profile_can_run_an_independent_reviewer() {
    let session = mj_core::state::SessionRecord {
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        expected_runtime_identity: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        id: "session-1".to_owned(),
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        title: "task".to_owned(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "primary".to_owned(),
        bundle_id: "bundle".to_owned(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "local".to_owned(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        container_cpus: None,
        container_memory: None,
        state: mj_core::state::SessionState::Running,
        archived: false,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    };

    validate_reviewer_assignment("session-1", Some(&session), "primary")
        .expect("same-profile review uses a separate conversation");
    validate_reviewer_assignment("session-1", Some(&session), "reviewer")
        .expect("a separate reviewer profile is accepted");
}

#[test]
fn delivery_admission_bypasses_only_the_matching_held_prompt() {
    let session = session_id("admission");
    hold_prompts(&session);
    let admission = admit_review_delivery(&session, 7, "forward-7")
        .expect("the live review hold grants its own corrective command");
    assert!(review_delivery_admitted(&session, &admission));
    assert!(!review_delivery_admitted(
        &session,
        &ReviewDeliveryAdmission::new(session.clone(), 7, "other-command".to_owned())
    ));
    assert!(!review_delivery_admitted(
        &session,
        &ReviewDeliveryAdmission::new(session.clone(), 8, "forward-7".to_owned())
    ));
    assert!(prompt_refusal(&session).is_some());
    release_prompts(&session);
}

/// Drives a session from running to idle, which is the edge that arms an
/// automatic review. The daemon observes each view as it publishes it, so
/// the fake does both too.
async fn finish_a_turn(manager: &FakeManager, host: &TurnReviewHost) {
    for execution in [
        mj_core::state::MaterializedExecutionState::Running { started_at_ms: 0 },
        mj_core::state::MaterializedExecutionState::Idle,
    ] {
        let published = view(&manager.session, execution);
        let _ = manager
            .publisher
            .publish(manager.session.clone(), published.clone())
            .await;
        host.observe(&manager.session, &published);
    }
}

#[tokio::test]
async fn auto_preparation_is_visible_and_can_be_cancelled() {
    let session = session_id("autoprepare");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(manager.control.clone(), armed(None), environment);
    finish_a_turn(&manager, &host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    assert!(host.view(&session).unwrap().status.contains("Preparing"));
    host.resolve(&session, Resolution::Cancelled).await.unwrap();
    assert!(
        host.refuses_prompt(&session),
        "preparation owns admission until its outstanding result settles"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));
    manager
        .drive_until(|| !host.refuses_prompt(&session) && host.view(&session).is_none())
        .await;
    host.shutdown().await.unwrap();
}

/// I2-9: a Mjolnir sub-agent's turn is reviewed through its parent's turn.
/// Reviewing the child on its own raced the parent's lifecycle operations
/// and posted their internal refusals into the child's transcript.
#[tokio::test]
async fn a_subagent_turn_is_not_reviewed_on_its_own() {
    let session = session_id("subagent000");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .subagent
        .store(true, std::sync::atomic::Ordering::Release);
    let host = TurnReviewHost::spawn_in(manager.control.clone(), armed(None), environment);
    finish_a_turn(&manager, &host).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "a sub-agent's turn asks its worker for nothing"
    );
    assert!(!host.refuses_prompt(&session));
    let view = host.view(&session);
    assert!(
        view.as_ref()
            .is_none_or(|view| !view.status.contains("did not start")),
        "{view:?}"
    );
    host.shutdown().await.unwrap();
}

#[test]
fn a_lifecycle_cancellation_is_not_shown_as_an_internal_error() {
    let notice = start_refusal_notice("reviewer operation cancelled for session lifecycle change");
    assert!(!notice.contains("lifecycle"), "{notice}");
    assert!(
        notice.contains("another operation was using the session"),
        "{notice}"
    );
}

/// A turn the harness starts on its own also runs and then goes idle.
/// Reviewing those is a separate decision, so the automatic edge ignores
/// one and still arms on the next turn that answers a prompt.
#[tokio::test]
async fn a_self_started_turn_does_not_arm_an_automatic_review() {
    let session = session_id("selfstarted0");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(manager.control.clone(), armed(None), environment.clone());

    for execution in [
        MaterializedExecutionState::Running { started_at_ms: 0 },
        MaterializedExecutionState::Idle,
    ] {
        host.observe(session, &view_of(session, execution, false));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "a turn the harness started on its own arms nothing"
    );

    // The next prompt-driven turn prepares an Auto reviewer.
    finish_a_turn(&manager, &host).await;
    assert!(
        matches!(manager.next().await, RemoteSessionRequest::Reviewer { .. }),
        "a prompt-driven turn still reaches the automatic edge"
    );
    host.shutdown().await.expect("shutdown the host");
}

/// A session nobody is attached to is reviewed: the daemon sees the turn
/// finish, captures, finds nothing changed, records its baseline, and
/// releases the lock. This is the headless case the terminal-hosted
/// review could never do.
#[tokio::test]
async fn a_headless_turn_is_reviewed_and_resolves_itself() {
    let session = session_id("headless000");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    finish_a_turn(&manager, &host).await;

    // The reviewer role is checked for a running second opinion first.
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    assert!(matches!(action, ReviewerAction::Status));
    assert!(
        host.refuses_prompt(session),
        "admission holds prompts before preparation waits on the session actor"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));

    // Then the capture that defines what is under review. Preparation takes
    // it before choosing a reviewer (I2-10), so it comes before the review
    // opens and before its active marker is written; the capture changes no
    // review state, and the prompt hold is already in place.
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(matches!(action, ReviewerAction::CaptureDelta { .. }));
    assert!(
        host.refuses_prompt(session),
        "the review holds the session's prompts from before the capture"
    );
    // Nothing changed, so the review records its baseline and resolves.
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: None,
            current_tree: "first-tree".to_owned(),
            patch: String::new(),
            diffstat: "0 files changed".to_owned(),
            changed_lines: 0,
        }],
    }));

    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::AdvanceBaseline { .. }))
        .await;
    let ReviewerAction::AdvanceBaseline { trees } = action else {
        unreachable!("matched above");
    };
    assert_eq!(
        trees
            .get(std::path::Path::new("/workspace/app"))
            .map(String::as_str),
        Some("first-tree"),
        "the capture becomes the baseline the next review measures from"
    );
    let _ = reply.send(Ok(ReviewerOutcome::BaselineAdvanced));

    tokio::time::timeout(Duration::from_secs(5), async {
        while host.refuses_prompt(session)
            || host.view(session).is_some()
            || environment.state().active.is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a resolved review releases prompts and drains its durable close");
    assert!(host.view(session).is_none(), "the review is over");
    assert_eq!(environment.state().active, None);
    assert!(
        environment
            .writes()
            .first()
            .is_some_and(|(state, _)| state.active.is_some()),
        "the active marker is durable before the review moves the baseline"
    );
}

/// Launch finding I2-10: with review on, a Codex turn that changed no files
/// showed "Preparing reviewer…" and held prompts for two minutes before
/// "Nothing to review: the turn changed no files". Choosing a reviewer is the
/// slow step, and a turn with nothing to review needs none, so the capture
/// comes first and an empty one resolves without waiting for a reviewer.
#[tokio::test]
async fn a_turn_that_changed_nothing_resolves_without_choosing_a_reviewer() {
    let session = session_id("nochanges00");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    environment
        .resolve_hangs
        .store(true, std::sync::atomic::Ordering::Release);
    let host = TurnReviewHost::spawn_in(manager.control.clone(), armed(None), environment.clone());

    finish_a_turn(&manager, &host).await;

    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: Some("reviewed-tree".to_owned()),
            current_tree: "reviewed-tree".to_owned(),
            patch: String::new(),
            diffstat: "0 files changed".to_owned(),
            changed_lines: 0,
        }],
    }));
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::AdvanceBaseline { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::BaselineAdvanced));
    let notice = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } = manager.next().await
            {
                let _ = reply.send(Ok(1));
                return text;
            }
        }
    })
    .await
    .expect("the review records its outcome in the conversation");
    assert_eq!(notice, "Nothing to review: the turn changed no files");
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.refuses_prompt(session) || host.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review releases the session's prompts");
}

/// The capture is a reviewer action too, so the recovery copy's lease can
/// refuse it as it refuses the reviewer choice (R4-9). Preparation then had
/// no capture and went on to choose a reviewer, so a turn that changed
/// nothing still waited through an Auto choice before "Nothing to review"
/// (I2-10). The capture now waits for background work and tries again.
#[tokio::test]
async fn a_capture_the_recovery_copy_refused_is_retried_before_choosing_a_reviewer() {
    let session = session_id("capturewait0");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    manager.refuse_next_capture("session is reserved for a lifecycle operation");
    let environment = FakeEnvironment::new();
    environment
        .resolve_hangs
        .store(true, std::sync::atomic::Ordering::Release);
    let host = TurnReviewHost::spawn_in(manager.control.clone(), armed(None), environment.clone());

    finish_a_turn(&manager, &host).await;

    // The fake refused the first capture. This is the retry, and nothing has
    // chosen a reviewer yet: a choice would hang and never capture again.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert_eq!(
        environment.background_waits(),
        2,
        "each capture waited for background work first"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: Some("reviewed-tree".to_owned()),
            current_tree: "reviewed-tree".to_owned(),
            patch: String::new(),
            diffstat: "0 files changed".to_owned(),
            changed_lines: 0,
        }],
    }));
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::AdvanceBaseline { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::BaselineAdvanced));
    let notice = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } = manager.next().await
            {
                let _ = reply.send(Ok(1));
                return text;
            }
        }
    })
    .await
    .expect("the review records its outcome in the conversation");
    assert_eq!(notice, "Nothing to review: the turn changed no files");
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.refuses_prompt(session) || host.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review releases the session's prompts");
    host.shutdown().await.unwrap();
}

/// A prompt that landed before the admission hold is reflected by the
/// live actor recheck, so preparation refuses and gives the prompt lock
/// back instead of reviewing a stale idle snapshot.
#[tokio::test]
async fn preparation_rechecks_the_live_actor_after_installing_the_prompt_hold() {
    let session = session_id("preparelive");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    finish_a_turn(&manager, &host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    assert!(host.refuses_prompt(session));

    manager
        .publisher
        .publish(
            session.to_owned(),
            view(
                session,
                MaterializedExecutionState::Running { started_at_ms: 1 },
            ),
        )
        .await
        .expect("publish the command that won the admission race");
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));

    tokio::time::timeout(Duration::from_secs(5), async {
        while host.refuses_prompt(session) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a refused preparation releases its prompt hold");
    assert!(host.view(session).is_none());
    assert_eq!(environment.state().active, None);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            manager
                .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        )
        .await
        .is_err(),
        "stale preparation never starts capture"
    );
    host.shutdown().await.expect("shutdown the host");
}

/// Session observations are an edge stream. A burst larger than the old
/// bounded hand-off must retain its final idle edge, or manual admission
/// sees a stale running session and automatic review can be lost too.
#[tokio::test]
async fn observation_bursts_do_not_drop_the_final_idle_edge() {
    let session = session_id("losslessobs");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let config: ReviewConfigSource = Arc::new(|| ReviewConfig {
        enabled: false,
        tier: ReviewTier::Quick,
        profile: Some("reviewer".to_owned()),
        model: None,
        effort: None,
    });
    let host = TurnReviewHost::spawn_in(manager.control.clone(), config, environment);

    let running = view(
        session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    for _ in 0..256 {
        host.observe(session, &running);
    }
    host.observe(session, &view(session, MaterializedExecutionState::Idle));

    let starting_host = host.clone();
    let session_owned = session.to_owned();
    let starting = tokio::spawn(async move { starting_host.start(&session_owned, true).await });
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));
    // Preparation captures the change before the review opens (I2-10); a
    // turn with a change keeps the review open.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));
    starting
        .await
        .expect("start task")
        .expect("the retained idle edge admits the review");
    assert!(host.refuses_prompt(session));
    host.shutdown().await.expect("shutdown the host");
}

/// Durable state uses one blocking FIFO lane: a blocked write cannot stop
/// the host actor, opening is not exposed before `active` is stored, and
/// shutdown waits for the final clear. The public shutdown is safe for
/// concurrent daemon cleanup callers and subsequent idempotent calls.
#[tokio::test]
async fn persistence_is_nonblocking_ordered_and_drained_on_shutdown() {
    let session = session_id("persistlane");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    finish_a_turn(&manager, &host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Status))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Status(Box::new(operational()))));
    // Preparation captures the change before the review opens (I2-10).
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let open_gate = environment.block_saves();
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));
    tokio::time::timeout(Duration::from_secs(5), open_gate.entered())
        .await
        .expect("the active write reaches the blocking lane");

    let refusal = tokio::time::timeout(Duration::from_secs(1), host.start(session, true))
        .await
        .expect("the host loop remains responsive while persistence blocks")
        .expect_err("the same review is already starting");
    assert!(refusal.0.contains("already starting"), "{refusal}");
    assert!(
        host.view(session).is_some(),
        "preparation is visible while persistence runs"
    );

    open_gate.release();
    // Whatever the open review asks for first (its analysis or its
    // reviewer) stays unanswered, which keeps the review open, as holding
    // the capture did when the review took it.
    let _first_request = manager.next().await;
    assert!(environment.state().active.is_some());
    assert!(host.view(session).is_some());

    let close_gate = environment.block_saves();
    let first_host = host.clone();
    let second_host = host.clone();
    let first = tokio::spawn(async move { first_host.shutdown().await });
    let second = tokio::spawn(async move { second_host.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), close_gate.entered())
        .await
        .expect("shutdown records resumable orchestration");
    assert!(!first.is_finished(), "shutdown drains the blocked write");
    assert!(
        !second.is_finished(),
        "concurrent shutdown joins the same drain"
    );
    assert!(
        !host.refuses_prompt(session),
        "logical shutdown releases prompts before persistence finishes"
    );
    close_gate.release();
    first
        .await
        .expect("first shutdown task")
        .expect("first drain");
    second
        .await
        .expect("second shutdown task")
        .expect("shared drain");
    host.shutdown().await.expect("shutdown stays idempotent");

    assert!(environment.state().active.is_some());
    assert!(environment.state().orchestration.is_some());
    assert!(host.view(session).is_none());
    let writes = environment.writes();
    assert!(
        writes
            .first()
            .is_some_and(|(state, _)| state.active.is_some())
    );
    assert!(
        writes
            .last()
            .is_some_and(|(state, _)| state.active.is_some() && state.orchestration.is_some())
    );
    let test_thread = std::thread::current().id();
    assert!(
        writes.iter().all(|(_, writer)| *writer != test_thread),
        "synchronous database writes run off the Tokio host thread"
    );
}

/// Queued prompts hold the review back: reviewing now would hold work the
/// user has already sent, and the review after the queue drains covers the
/// whole batch anyway.
#[tokio::test]
async fn an_interrupted_handoff_retains_findings_until_acceptance_and_retries_the_same_id() {
    let session = session_id("handoff0000");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let pending = PendingForward {
        synthesis: "[P2] src/lib.rs:1 -- incorrect boundary".to_owned(),
        evidence: Default::default(),
        command_id: "durable-forward-id".to_owned(),
        trees: BTreeMap::from([(PathBuf::from("/workspace/app"), "new".to_owned())]),
        reviewed_through_ordinal: 12,
    };
    {
        let mut state = environment.state.lock().unwrap();
        state
            .baselines
            .insert("/workspace/app".into(), "old".into());
        state.pending_forward = Some(pending.clone());
    }
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    host.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    host.events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    let RemoteSessionRequest::Submit {
        command_id,
        admission,
        reply,
        ..
    } = manager.next().await
    else {
        panic!("recovery submits the pending handoff directly, without starting a reviewer");
    };
    assert_eq!(command_id, pending.command_id);
    assert!(review_delivery_admitted(&session, &admission.unwrap()));
    assert!(host.refuses_prompt(&session));
    assert_eq!(environment.state().pending_forward, Some(pending.clone()));
    assert_eq!(
        environment.state().baselines[&PathBuf::from("/workspace/app")],
        "old"
    );
    assert!(
        host.resolve(&session, Resolution::Forwarded).await.is_err(),
        "duplicate Forward is not another submission"
    );
    reply
        .send(Err("primary temporarily unavailable".into()))
        .unwrap();
    assert_eq!(environment.state().pending_forward, Some(pending.clone()));
    // Transport ambiguity retains the delivery owner and retries the original identity.
    let RemoteSessionRequest::Submit {
        command_id,
        admission,
        reply,
        ..
    } = manager.next().await
    else {
        panic!("retry submits the same handoff");
    };
    assert_eq!(command_id, pending.command_id);
    let epoch = admission.unwrap().epoch();
    host.events
        .send(HostEvent::Step {
            session_id: session.clone(),
            epoch,
            step: ReviewStep::RoleEvents {
                role: "reviewer".to_owned(),
                result: Err("late reviewer disconnect".to_owned()),
            },
        })
        .await
        .unwrap();
    let gate = environment.block_saves();
    reply.send(Ok(42)).unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered())
        .await
        .unwrap();
    assert_eq!(
        environment.state().pending_forward,
        Some(pending),
        "the durable pending record remains until the complete accepted outcome is written"
    );
    gate.release();
    manager.drive_until(|| host.view(&session).is_none()).await;
    let state = environment.state();
    assert!(state.pending_forward.is_none());
    assert!(state.prior_review.is_some());
    assert_eq!(state.baselines[&PathBuf::from("/workspace/app")], "new");
    assert!(!host.refuses_prompt(&session));
    assert!(environment.staged_roles().is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_prompts_hold_a_review_back() {
    let session = session_id("queued00000");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    let mut queued = view(session, mj_core::state::MaterializedExecutionState::Idle);
    if let Some(snapshot) = queued.snapshot.as_mut() {
        snapshot.materialized.queued_prompts = vec![mj_core::state::MaterializedQueuedPrompt {
            accepted_ordinal: None,
            command_id: "queued-1".to_owned(),
            kind: mj_core::state::QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "next"})],
            queued_at_ms: 0,
        }];
    }
    host.observe(
        session,
        &view(
            session,
            mj_core::state::MaterializedExecutionState::Running { started_at_ms: 0 },
        ),
    );
    host.observe(session, &queued);

    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "no review starts while prompts are queued"
    );
    let refusal = host
        .start(session, true)
        .await
        .expect_err("a manual review is refused for the same reason");
    assert!(refusal.0.contains("queued"), "{refusal}");
}

/// Resolutions are gated on the verdict the review actually reached, in
/// the host rather than in any surface, so every surface gets the same
/// answer.
#[tokio::test]
async fn resolving_a_review_that_has_no_verdict_is_refused() {
    let session = session_id("resolution0");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    let error = host
        .resolve(session, Resolution::Forwarded)
        .await
        .expect_err("there is no review at all");
    assert!(error.contains("no review is open"), "{error}");

    finish_a_turn(&manager, &host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));

    // The capture now arrives while the review is still being prepared
    // (I2-10), so wait for the review itself rather than for any view.
    tokio::time::timeout(Duration::from_secs(5), async {
        while host
            .view(session)
            .is_none_or(|view| view.status.contains("Preparing"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review is open");

    let error = host
        .resolve(session, Resolution::Forwarded)
        .await
        .expect_err("nothing has been found yet");
    assert!(error.contains("no findings"), "{error}");
    let error = host
        .resolve(session, Resolution::Dismissed)
        .await
        .expect_err("nothing has been decided yet");
    assert!(error.contains("verdict"), "{error}");
    // Cancel is always available, which is what keeps a surface from ever
    // being stuck with an open review it cannot end.
    host.resolve(session, Resolution::Cancelled)
        .await
        .expect("cancel needs no verdict");
    manager.drive_until(|| !host.refuses_prompt(session)).await;
    host.shutdown().await.unwrap();
}

/// A reviewer launch failure is a durable failed verdict, but it no longer
/// owns the primary turn: the active marker and admission hold are both
/// cleared before the user dismisses the visible failure.
#[tokio::test]
async fn a_failed_review_clears_durable_active_state_and_the_prompt_hold() {
    let session = session_id("failedrole0");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    finish_a_turn(&manager, &host).await;

    // Preparation captures the change before the review opens (I2-10), so
    // the active marker is durable by the time a reviewer starts rather than
    // by the capture.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Start { .. }))
        .await;
    assert!(environment.state().active.is_some());
    let _ = reply.send(Err("review harness failed to launch".to_owned()));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let failed = host.view(session).is_some_and(|view| {
                matches!(
                    view.verdict,
                    Some(VerdictView {
                        kind: VerdictKind::Failed,
                        ..
                    })
                )
            });
            if failed && !host.refuses_prompt(session) && environment.state().active.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failure releases and persists the turn");

    host.resolve(session, Resolution::Dismissed)
        .await
        .expect("the visible failure can be dismissed");
    manager.drive_until(|| host.view(session).is_none()).await;
    host.shutdown().await.expect("shutdown the host");
}

/// A relay event carrying one agent message, for the answer a role's
/// journal reports.
fn agent_event(ordinal: u64, previous_digest: &str, text: &str) -> RelayEvent {
    let mut event = RelayEvent {
        format: RELAY_EVENT_FORMAT_V1,
        ordinal,
        previous_digest: previous_digest.to_owned(),
        digest: String::new(),
        recorded_at_ms: i64::try_from(ordinal).unwrap_or_default() * 100,
        command_id: None,
        observation: RelayObservation::SessionUpdate {
            update: Box::new(
                agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(
                    agent_client_protocol::schema::v1::ContentChunk::new(
                        agent_client_protocol::schema::v1::ContentBlock::Text(
                            agent_client_protocol::schema::v1::TextContent::new(text),
                        ),
                    ),
                ),
            ),
        },
    };
    event.digest = relay_event_digest(&event).expect("digest");
    event
}

fn completion_event(ordinal: u64, previous_digest: &str, command_id: &str) -> RelayEvent {
    let mut event = RelayEvent {
        format: RELAY_EVENT_FORMAT_V1,
        ordinal,
        previous_digest: previous_digest.to_owned(),
        digest: String::new(),
        recorded_at_ms: i64::try_from(ordinal).unwrap_or_default() * 100,
        command_id: Some(command_id.to_owned()),
        observation: RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: command_id.to_owned(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".to_owned(),
                usage: None,
            },
        },
    };
    event.digest = relay_event_digest(&event).expect("digest");
    event
}

/// A role's answer is read from its own journal, and it is the completion
/// record for the exact command the driver submitted that says the answer
/// is final -- not merely the newest message in the journal.
#[tokio::test]
async fn a_clean_reviewer_report_resolves_the_review() {
    let session = session_id("cleanreport");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let publications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let published = publications.clone();
    let host = TurnReviewHost::spawn_in_notifying(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
        Arc::new(move || {
            published.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }),
    );
    finish_a_turn(&manager, &host).await;

    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let after_add = publications.load(std::sync::atomic::Ordering::SeqCst);
    assert!(after_add > 0, "opening publishes and wakes surfaces");
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
        }],
    }));

    // The reviewer's harness starts, and the host prompts it.
    let (role, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::Start { .. }))
        .await;
    let after_change = publications.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        after_change > after_add,
        "a projected review state change wakes surfaces"
    );
    assert_eq!(
        role.as_deref(),
        Some(mj_core::review::driver::REVIEWER_ROLE)
    );
    let _ = reply.send(Ok(ReviewerOutcome::Started(Box::new(
        crate::worker_client::StartedReviewer {
            native_session_id: None,
            config_options: Vec::new(),
            reused: false,
            state: operational(),
        },
    ))));

    let (_, action, reply) = manager
        .next_reviewer(|role, action| {
            role.as_deref() == Some(mj_core::review::driver::REVIEWER_ROLE)
                && matches!(action, ReviewerAction::SubmitDurable { .. })
        })
        .await;
    let ReviewerAction::SubmitDurable {
        command_id,
        command,
        ..
    } = action
    else {
        unreachable!("matched above");
    };
    let RelayCommand::Prompt { prompt } = command else {
        panic!("a reviewing role is prompted");
    };
    assert!(
        format!("{prompt:?}").contains("+one"),
        "the reviewer is given the captured change"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Accepted { ordinal: 1 }));

    // Its journal reports a clean answer, ending the command it was given.
    let (_, _, reply) = manager
        .next_reviewer(|role, action| {
            role.as_deref() == Some(mj_core::review::driver::REVIEWER_ROLE)
                && matches!(action, ReviewerAction::Attach { .. })
        })
        .await;
    let _ = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            match manager.requests.recv().await.unwrap() {
                RemoteSessionRequest::Reviewer {
                    action: ReviewerAction::ReleaseReviewerCommandReceipt { .. },
                    reply,
                    ..
                } => {
                    let _ = reply.send(Ok(ReviewerOutcome::CommandReceiptReleased));
                }
                _ => panic!("one role prompt has only one attachment poll in flight"),
            }
        }
    })
    .await;
    let before_identical = publications.load(std::sync::atomic::Ordering::SeqCst);
    let _ = reply.send(Ok(ReviewerOutcome::Attached(Box::new(
        crate::worker_client::RelayAttachment {
            state: operational(),
            events: Vec::new(),
            through_ordinal: 0,
            through_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        },
    ))));

    // An empty journal page runs the host's publish path but leaves the
    // projection identical. It schedules another poll without a wakeup.
    let (_, _, reply) = manager
        .next_reviewer(|role, action| {
            role.as_deref() == Some(mj_core::review::driver::REVIEWER_ROLE)
                && matches!(action, ReviewerAction::Attach { .. })
        })
        .await;
    assert_eq!(
        publications.load(std::sync::atomic::Ordering::SeqCst),
        before_identical,
        "an identical projection does not wake surfaces"
    );
    let answer = agent_event(
        1,
        mj_core::relay::RELAY_EVENT_GENESIS_DIGEST,
        "No findings.",
    );
    let completion = completion_event(2, &answer.digest, &command_id);
    let through_digest = completion.digest.clone();
    let _ = reply.send(Ok(ReviewerOutcome::Attached(Box::new(
        crate::worker_client::RelayAttachment {
            state: operational(),
            events: vec![answer, completion],
            through_ordinal: 2,
            through_digest,
        },
    ))));

    manager
        .drive_until(|| {
            !host.refuses_prompt(session)
                && host.view(session).is_none()
                && environment.state().active.is_none()
        })
        .await;
    assert!(host.view(session).is_none());
    assert!(
        publications.load(std::sync::atomic::Ordering::SeqCst) > before_identical,
        "closing removes the view and wakes surfaces"
    );

    // The reviewer ran under the configured profile, and only the
    // supervisor is ever given the tool that launches specialists.
    let staged = environment.staged_roles();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].0, "reviewer");
    assert_ne!(staged[0].1, 0, "fresh reviewer generation");
    assert!(!staged[0].2);
    // A resolved review records what it reviewed through, so the next one
    // measures from here.
    let recorded = environment.state();
    assert_eq!(
        recorded
            .baselines
            .get(std::path::Path::new("/workspace/app"))
            .map(String::as_str),
        Some("new")
    );
    assert_eq!(recorded.reviewed_through_ordinal, 12);
    assert_eq!(recorded.active, None);
    host.shutdown().await.expect("shutdown the host");
}

fn restartable_review_slot() -> ReviewSlot {
    restartable_review_slot_at(ReviewTier::Quick)
}

fn restartable_review_slot_at(tier: ReviewTier) -> ReviewSlot {
    let role = if tier == ReviewTier::Quick {
        mj_review::driver::REVIEWER_ROLE
    } else {
        SUPERVISOR_ROLE
    };
    let (mut driver, _) = TurnReviewDriver::start(TurnReviewSeed {
        tier,
        task: "finish the parser".into(),
        user_messages: vec![UserMessage::prompt("finish the parser")],
        initial_result: "parser implemented".into(),
        trajectory: "edited parser.rs".into(),
        baselines: BTreeMap::from([(PathBuf::from("/workspace/app"), "base".into())]),
        through_ordinal: 12,
        prior_review: None,
    });
    driver.delta_captured(vec![mj_core::relay::RepoDelta {
        root: "/workspace/app".into(),
        baseline_tree: Some("base".into()),
        current_tree: "new".into(),
        patch: "diff --git a/parser.rs b/parser.rs\n@@\n+parse\n".into(),
        diffstat: "1 insertion".into(),
        changed_lines: 1,
    }]);
    driver.analysis_completed(Ok("parser changed".into()));
    let outbox = driver.role_started(role);
    let receipts = outbox
        .iter()
        .filter_map(|request| match request {
            ReviewRequest::PromptRole { command_id, .. } => Some((
                command_id.clone(),
                super::durable::ReviewReceipt {
                    role: Some(role.into()),
                    generation: 812,
                    phase: super::durable::ReceiptPhase::Waiting,
                },
            )),
            _ => None,
        })
        .collect();
    ReviewSlot {
        epoch: 71,
        driver,
        roles: BTreeMap::new(),
        reviewer: ReviewerIdentity::default(),
        state: TurnReviewState {
            active: Some("review-71".into()),
            ..Default::default()
        },
        generation: 812,
        role_generations: BTreeMap::from([(role.into(), 812)]),
        outbox,
        receipts,
        running_effects: BTreeSet::new(),
        delivery_errors: BTreeMap::new(),
        polling_roles: BTreeSet::new(),
        reading_dispatches: false,
        pending_supervisor: None,
        dispatch_after_completion: false,
        accepted_dispatches: BTreeSet::new(),
        pending_dispatch_acks: BTreeSet::new(),
    }
}

#[tokio::test]
async fn restart_replays_the_durable_role_prompt_with_its_original_identity() {
    let session = session_id("durablereviewprompt");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let mut slot = restartable_review_slot();
    let expected = slot
        .outbox
        .iter()
        .find_map(|request| match request {
            ReviewRequest::PromptRole {
                command_id, prompt, ..
            } => Some((command_id.clone(), prompt.clone())),
            _ => None,
        })
        .unwrap();
    slot.state.orchestration = Some(slot.checkpoint().unwrap());
    *environment.state.lock().unwrap() = slot.state;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    host.ready().await.unwrap();
    host.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    host.events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    let (role, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::SubmitDurable { .. }))
        .await;
    assert_eq!(role.as_deref(), Some(mj_review::driver::REVIEWER_ROLE));
    let ReviewerAction::SubmitDurable {
        command_id,
        command,
        ..
    } = action
    else {
        unreachable!()
    };
    assert_eq!(command_id, expected.0);
    assert_eq!(command, prompt_command(expected.1));
    reply
        .send(Ok(ReviewerOutcome::Accepted { ordinal: 13 }))
        .unwrap();
    assert!(host.refuses_prompt(&session));
    host.shutdown().await.unwrap();
    assert!(environment.state().orchestration.is_some());
}

#[tokio::test]
async fn restart_reattaches_the_saved_role_cursor_without_starting_another_harness() {
    let session = session_id("durablereviewcursor");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let mut slot = restartable_review_slot();
    slot.outbox.clear();
    slot.receipts.clear();
    slot.roles.insert(
        mj_review::driver::REVIEWER_ROLE.into(),
        RoleTranscript {
            cursor_ordinal: 42,
            cursor_digest: "saved-digest".into(),
            session: None,
        },
    );
    slot.state.orchestration = Some(slot.checkpoint().unwrap());
    *environment.state.lock().unwrap() = slot.state;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment,
    );
    host.ready().await.unwrap();
    host.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    host.events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    let RemoteSessionRequest::Reviewer { action, reply, .. } = manager.next().await else {
        panic!("expected reviewer reattachment");
    };
    assert!(
        matches!(action, ReviewerAction::Attach { after_ordinal: 42, after_digest: ref digest } if digest == "saved-digest")
    );
    reply.send(answer_for(&action)).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_preserves_findings_waiting_for_a_user_resolution() {
    let session = session_id("durablereviewverdict");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let mut slot = restartable_review_slot();
    let reviewer_command = slot.driver.awaited_commands()[0].1.clone();
    slot.driver.role_turn_completed(
        &reviewer_command,
        "[P2] parser.rs:1 -- empty input is rejected",
    );
    slot.driver.role_started(mj_review::driver::VALIDATOR_ROLE);
    let validator_command = slot.driver.awaited_commands()[0].1.clone();
    slot.driver.role_turn_completed(
        &validator_command,
        "[P2] parser.rs:1 -- empty input is rejected",
    );
    let expected = slot.driver.phase().clone();
    assert!(matches!(
        expected,
        TurnReviewPhase::Verdict(ReviewVerdict::Findings { .. })
    ));
    slot.outbox.clear();
    slot.receipts.clear();
    slot.state.orchestration = Some(slot.checkpoint().unwrap());
    *environment.state.lock().unwrap() = slot.state;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    host.ready().await.unwrap();
    host.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    host.events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(host.view(&session).unwrap().phase, expected);
    assert!(host.refuses_prompt(&session));
    host.shutdown().await.unwrap();
    let restored = ReviewSlot::restore(environment.state()).unwrap();
    assert_eq!(restored.driver.phase(), &expected);
}

#[test]
fn review_observations_bound_transcripts_and_retain_a_coalesced_completion() {
    let mut mailbox = super::observations::Observations::default();
    let running = MaterializedExecutionState::Running { started_at_ms: 0 };
    mailbox.observe("review", &view("review", running));
    mailbox.observe("review", &view("review", MaterializedExecutionState::Idle));
    for _ in 0..10_000 {
        mailbox.observe("review", &view("review", MaterializedExecutionState::Idle));
    }
    assert!(matches!(
        mailbox.pop(),
        Some(HostEvent::View {
            finished_turn: true,
            ..
        })
    ));
    assert!(mailbox.pop().is_none());
}

#[tokio::test]
async fn specialist_dispatch_is_checkpointed_before_ack_and_ack_replays_after_restart() {
    let session = session_id("durablereviewdispatch");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let mut slot = restartable_review_slot_at(ReviewTier::Extended);
    slot.outbox.clear();
    slot.receipts.clear();
    slot.state.orchestration = Some(slot.checkpoint().unwrap());
    *environment.state.lock().unwrap() = slot.state;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    host.ready().await.unwrap();
    host.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    host.events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    // Keep the journal read outstanding so no periodic observation interferes
    // with the persistence barrier this test controls.
    let outstanding_poll = manager.next().await;
    let gate = environment.block_saves();
    let dispatch = mj_core::relay::ReviewerLaneDispatch {
        id: "lane-stable-1".into(),
        generation: 812,
        request: mj_core::review::lanes::ReviewSubagentRequest {
            agent_type: "control_flow".into(),
            hypothesis: "Check the empty parser input branch".into(),
        },
    };
    host.events
        .send(HostEvent::Step {
            session_id: session.clone(),
            epoch: 71,
            step: ReviewStep::Dispatches(Ok(vec![dispatch.clone()])),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate.entered())
        .await
        .unwrap();
    assert!(
        !environment.state().orchestration.unwrap()["accepted_dispatches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "lane-stable-1")
    );
    gate.release();
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::AckLaneDispatches { .. }))
        .await;
    assert!(
        matches!(action, ReviewerAction::AckLaneDispatches { ref ids } if ids == &["lane-stable-1"])
    );
    let restored = ReviewSlot::restore(environment.state()).unwrap();
    assert!(restored.accepted_dispatches.contains("lane-stable-1"));
    assert!(restored.pending_dispatch_acks.contains("lane-stable-1"));
    // Lose the ACK and replace the controller while worker ownership remains.
    drop(reply);
    host.shutdown().await.unwrap();
    drop(outstanding_poll);
    let restored = ReviewSlot::restore(environment.state()).unwrap();
    assert!(restored.accepted_dispatches.contains("lane-stable-1"));
    let replacement = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    replacement.ready().await.unwrap();
    replacement.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    replacement
        .events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::AckLaneDispatches { .. }))
        .await;
    assert!(
        matches!(action, ReviewerAction::AckLaneDispatches { ref ids } if ids == &["lane-stable-1"])
    );
    reply
        .send(Ok(ReviewerOutcome::LaneDispatchesAcknowledged))
        .unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_ambiguous_review_prompt_reconciles_before_releasing_its_receipt() {
    let session = session_id("cancelreceipt");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let mut slot = restartable_review_slot();
    let command_id = slot.receipts.keys().next().unwrap().clone();
    slot.receipts.get_mut(&command_id).unwrap().phase = super::durable::ReceiptPhase::Cancel;
    slot.outbox.extend(slot.driver.cancel());
    slot.state.orchestration = Some(slot.checkpoint().unwrap());
    *environment.state.lock().unwrap() = slot.state;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    host.ready().await.unwrap();
    host.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    host.events
        .send(HostEvent::Initialized {
            result: Ok(vec![session.clone()]),
        })
        .await
        .unwrap();
    let (_, action, reply) = manager
        .next_reviewer(|_, action| {
            matches!(
                action,
                ReviewerAction::CancelReviewerCommandAdmission { .. }
                    | ReviewerAction::SubmitDurable { .. }
            )
        })
        .await;
    assert!(
        matches!(action, ReviewerAction::CancelReviewerCommandAdmission { generation: 812, command_id: ref id } if id == &command_id)
    );
    assert!(environment.state().orchestration.is_some());
    reply
        .send(Ok(ReviewerOutcome::CommandReceipt { receipt: None }))
        .unwrap();
    let (_, action, reply) = manager
        .next_reviewer(|_, action| {
            matches!(
                action,
                ReviewerAction::PauseGeneration { .. } | ReviewerAction::SubmitDurable { .. }
            )
        })
        .await;
    assert!(matches!(
        action,
        ReviewerAction::PauseGeneration { generation: 812 }
    ));
    let restored = ReviewSlot::restore(environment.state()).unwrap();
    assert!(!restored.receipts.contains_key(&command_id));
    assert!(
        !restored
            .outbox
            .iter()
            .any(|request| matches!(request, ReviewRequest::PromptRole { .. }))
    );
    reply.send(Ok(ReviewerOutcome::Paused)).unwrap();
    host.shutdown().await.unwrap();
}

fn checkpoint_owner(
    manager: &FakeManager,
) -> (HostState, mpsc::UnboundedReceiver<PersistenceRequest>) {
    let (persistence, requests) = mpsc::unbounded_channel();
    let (events, _receiver) = mpsc::channel(256);
    let shared = Arc::new(HostShared {
        views: Mutex::default(),
        changed: Arc::new(|| {}),
        shutdown: tokio::sync::OnceCell::new(),
        observations: Mutex::default(),
        observation_ready: tokio::sync::Notify::new(),
        stop_delivery: tokio_util::sync::CancellationToken::new(),
        initialized: tokio::sync::watch::channel(None).0,
    });
    (
        HostState {
            control: manager.control.clone(),
            config: armed(Some("reviewer")),
            environment: FakeEnvironment::new(),
            shared,
            events,
            persistence: Some(persistence),
            persistence_task: None,
            reviews: BTreeMap::from([(manager.session.clone(), restartable_review_slot())]),
            preparing: BTreeSet::new(),
            pending_open: BTreeMap::new(),
            closing: BTreeSet::new(),
            next_epoch: 71,
            sessions: BTreeMap::new(),
            preparation_cancellation: BTreeMap::new(),
            recovery_candidates: BTreeSet::new(),
            recovery_in_flight: BTreeSet::new(),
            dirty: BTreeSet::from([manager.session.clone()]),
            checkpointing: BTreeMap::new(),
            next_checkpoint_revision: 0,
            persistence_errors: BTreeMap::new(),
            start_replies: BTreeMap::new(),
            resolve_replies: BTreeMap::new(),
        },
        requests,
    )
}

#[tokio::test]
async fn an_older_checkpoint_ack_cannot_authorize_a_newer_review_effect() {
    let session = session_id("checkpointack");
    let manager = FakeManager::new(&session).await;
    let (mut owner, mut writes) = checkpoint_owner(&manager);
    owner.checkpoint_dirty();
    let PersistenceRequest::Save {
        completion: Some(first),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("first checkpoint");
    };
    owner
        .reviews
        .get_mut(&session)
        .unwrap()
        .pending_dispatch_acks
        .insert("new-dispatch".into());
    owner.dirty.insert(session.clone());
    owner.checkpoint_dirty();
    assert!(
        writes.try_recv().is_err(),
        "one checkpoint at a time per review"
    );
    owner.state_saved(session.clone(), first, Ok(()));
    let PersistenceRequest::Save {
        state,
        completion: Some(second),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("coalesced newer checkpoint");
    };
    assert_ne!(first, second);
    assert!(
        state.orchestration.unwrap()["pending_dispatch_acks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "new-dispatch")
    );
    assert!(owner.reviews[&session].running_effects.is_empty());
    owner.state_saved(session.clone(), first, Ok(()));
    assert!(
        owner.reviews[&session].running_effects.is_empty(),
        "duplicate old ACK cannot release the barrier"
    );
    owner.state_saved(session.clone(), second, Ok(()));
    assert!(
        owner.reviews[&session]
            .running_effects
            .contains("dispatch-ack")
    );
}

#[tokio::test]
async fn failed_checkpoint_retries_current_state_and_ignores_its_late_ack() {
    let session = session_id("checkpointretry");
    let manager = FakeManager::new(&session).await;
    let (mut owner, mut writes) = checkpoint_owner(&manager);
    owner.checkpoint_dirty();
    let PersistenceRequest::Save {
        completion: Some(first @ PersistenceCompletion::Checkpoint { epoch, revision }),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("first checkpoint");
    };
    owner.state_saved(
        session.clone(),
        first,
        Err("disk temporarily unavailable".into()),
    );
    owner
        .reviews
        .get_mut(&session)
        .unwrap()
        .state
        .reviewed_through_ordinal = 99;
    owner.dirty.insert(session.clone());
    owner.checkpoint_dirty();
    assert!(
        writes.try_recv().is_err(),
        "backoff retains the sole write owner"
    );
    owner.retry_checkpoint(session.clone(), epoch, revision);
    let PersistenceRequest::Save {
        state,
        completion: Some(second),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("retry uses latest desired state");
    };
    assert_eq!(state.reviewed_through_ordinal, 99);
    owner.state_saved(session.clone(), first, Ok(()));
    owner.retry_checkpoint(session.clone(), epoch, revision);
    assert!(owner.reviews[&session].running_effects.is_empty());
    assert!(
        writes.try_recv().is_err(),
        "late retry cannot enqueue an obsolete snapshot"
    );
    owner.state_saved(session.clone(), second, Ok(()));
    assert!(!owner.reviews[&session].running_effects.is_empty());
}

#[tokio::test]
async fn cancelling_a_committing_open_retains_ownership_through_failed_close() {
    let session = session_id("cancelopencommit");
    let manager = FakeManager::new(&session).await;
    let (mut owner, mut writes) = checkpoint_owner(&manager);
    owner.reviews.clear();
    owner.dirty.clear();
    owner.preparing.insert(session.clone());
    owner.preparation_cancellation.insert(
        session.clone(),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    owner.pending_open.insert(
        session.clone(),
        PendingOpen {
            epoch: 72,
            manual: false,
            reply: None,
            prepared: Prepared {
                state: TurnReviewState {
                    active: Some("review-72".into()),
                    pending_forward: None,
                    ..Default::default()
                },
                reviewer: ReviewerIdentity::default(),
                tier: ReviewTier::Quick,
                materialized: Box::new(MaterializedSession::empty(&session)),
                resume_forward: None,
                captured: None,
            },
        },
    );
    hold_prompts(&session);
    owner.state_saved(
        session.clone(),
        PersistenceCompletion::Open { epoch: 72 },
        Ok(()),
    );
    assert!(prompt_refusal(&session).is_some());
    assert!(
        owner.reviews[&session]
            .outbox
            .iter()
            .all(|request| matches!(request, ReviewRequest::Close))
    );
    owner.checkpoint_dirty();
    let PersistenceRequest::Save {
        state,
        completion: Some(checkpoint),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("cancel checkpoint");
    };
    assert!(state.pending_forward.is_none());
    let restored = ReviewSlot::restore(*state).unwrap();
    assert!(matches!(
        restored.driver.phase(),
        TurnReviewPhase::Resolved(Resolution::Cancelled)
    ));
    owner.state_saved(session.clone(), checkpoint, Ok(()));
    let PersistenceRequest::Save {
        completion: Some(close),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("acknowledged close");
    };
    owner.state_saved(session.clone(), close, Err("disk unavailable".into()));
    assert!(owner.reviews.contains_key(&session));
    assert!(
        prompt_refusal(&session).is_some(),
        "failure cannot release the durable owner"
    );
    owner
        .handle(HostEvent::RetryClose {
            session_id: session.clone(),
            epoch: 72,
        })
        .await;
    owner.checkpoint_dirty();
    let PersistenceRequest::Save {
        completion: Some(checkpoint),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("retry current cancelled state");
    };
    owner.state_saved(session.clone(), checkpoint, Ok(()));
    let PersistenceRequest::Save {
        completion: Some(close),
        ..
    } = writes.try_recv().unwrap()
    else {
        panic!("retry close");
    };
    owner.state_saved(session.clone(), close, Ok(()));
    assert!(!owner.reviews.contains_key(&session));
    assert!(prompt_refusal(&session).is_none());
}
