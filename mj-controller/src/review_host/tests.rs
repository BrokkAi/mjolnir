use super::*;
use crate::session_manager::{
    RelaySessionTarget, RemoteSessionRequest, RemoteSessionRequests, spawn_remote_session_manager,
};
use mj_core::state::{ManagedSessionSnapshot, MaterializedSession};
use std::path::PathBuf;

/// One session id per test keeps the fake manager and durable review state
/// independent across concurrent test execution.
fn session_id(test: &str) -> String {
    format!("018f9dd2-a3b4-7c8d-9000-{test}")
}

/// I1-14: a review that could not start because a lifecycle operation held
/// the session said only "session is reserved for a lifecycle operation".
// Hard-won: 8100bee6: the refusal surfaced raw lifecycle error text instead of an actionable message
#[test]
fn a_review_that_cannot_start_says_so_in_plain_words() {
    assert_eq!(
        start_refusal_notice("session is reserved for a lifecycle operation"),
        "Turn review did not start: another operation was using the session. \
         The next review covers these changes."
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
        pending_followup: false,
        forward_attempts: 0,
        reviewed_through_ordinal: 1,
        ..TurnReviewState::default()
    };

    let seed = seed_from_session(&session, &state, "manual");
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
        "reviewers receive real prompts in chronological order"
    );
    assert!(
        seed.user_messages
            .iter()
            .all(|message| !message.text.contains("HARNESS NOTE")),
        "harness notes are not user messages"
    );

    // A corrective-only pass still has no new user prompt, but retains the
    // latest real prompt as its current outer task.
    state.reviewed_through_ordinal = 5;
    let corrective = seed_from_session(&session, &state, "manual");
    assert_eq!(corrective.task, "also finish the parser error path");
    assert_eq!(corrective.user_messages.len(), 3);
}

/// An idle relay state for the primary session view.
fn operational() -> mj_core::relay::RelayOperationalState {
    serde_json::from_value(serde_json::json!({
        "session_id": "reviewer",
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
        let channels = spawn_remote_session_manager().expect("remote manager");
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
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("the host makes a request")
            .expect("the manager is still running")
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
        ReviewerAction::CaptureDelta { .. } => Ok(ReviewerOutcome::Delta {
            repositories: Vec::new(),
        }),
        ReviewerAction::AdvanceBaseline { .. } | ReviewerAction::RecordReviewed { .. } => {
            Ok(ReviewerOutcome::BaselineAdvanced)
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
            subagent_completions: Vec::new(),
            subagent_queue_error: None,
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
    children: Mutex<Vec<(String, ReviewerIdentity, String)>>,
    child_relations: Mutex<Vec<ReviewerChildRelation>>,
    registered_working_directories: Mutex<Vec<Option<PathBuf>>>,
    mailbox: Mutex<Vec<mj_core::mailbox::MailboxEvent>>,
    started: Mutex<Vec<String>>,
    closed: Mutex<Vec<String>>,
    close_error: Mutex<Option<String>>,
    start_error: Mutex<Option<String>>,
    register_error: Mutex<Option<String>>,
    register_gate: Mutex<Option<Arc<SaveGate>>>,
    registered_gate: Mutex<Option<Arc<SaveGate>>>,
    /// The review bookkeeping, in memory rather than in the developer's
    /// own database.
    state: Mutex<TurnReviewState>,
    writes: Mutex<Vec<(TurnReviewState, std::thread::ThreadId)>>,
    /// The session the state belongs to, learned from the first save, so the
    /// restart sweep can name it the way the database's sweep does.
    owner: Mutex<Option<String>>,
    save_gate: Mutex<Option<Arc<SaveGate>>>,
    sweep_gate: Mutex<Option<Arc<SaveGate>>>,
    save_error: Mutex<Option<String>>,
    final_save_error: Mutex<Option<String>>,
    subagent: std::sync::atomic::AtomicBool,
    classify_gate: Mutex<Option<Arc<SaveGate>>>,
    review_refusal: Mutex<Option<String>>,
    /// Refusals the next reviewer resolutions answer with, in order.
    resolve_refusals: Mutex<std::collections::VecDeque<String>>,
    /// How many background holds the host took on the session.
    background_holds: std::sync::atomic::AtomicUsize,
    /// How many of those holds are still held.
    live_holds: Arc<std::sync::atomic::AtomicUsize>,
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
            children: Mutex::new(Vec::new()),
            child_relations: Mutex::new(Vec::new()),
            registered_working_directories: Mutex::new(Vec::new()),
            mailbox: Mutex::new(Vec::new()),
            started: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            close_error: Mutex::new(None),
            start_error: Mutex::new(None),
            register_error: Mutex::new(None),
            register_gate: Mutex::new(None),
            registered_gate: Mutex::new(None),
            state: Mutex::new(TurnReviewState::default()),
            writes: Mutex::new(Vec::new()),
            owner: Mutex::new(None),
            save_gate: Mutex::new(None),
            sweep_gate: Mutex::new(None),
            save_error: Mutex::new(None),
            final_save_error: Mutex::new(None),
            subagent: std::sync::atomic::AtomicBool::new(false),
            classify_gate: Mutex::new(None),
            review_refusal: Mutex::new(None),
            resolve_refusals: Mutex::new(std::collections::VecDeque::new()),
            background_holds: std::sync::atomic::AtomicUsize::new(0),
            live_holds: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            resolve_hangs: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn refuse_next_resolve(&self, reason: &str) {
        self.resolve_refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(reason.to_owned());
    }

    fn refuse_review(&self, reason: &str) {
        *self
            .review_refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason.to_owned());
    }

    fn background_holds(&self) -> usize {
        self.background_holds
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn live_holds(&self) -> usize {
        self.live_holds.load(std::sync::atomic::Ordering::Acquire)
    }

    fn state(&self) -> TurnReviewState {
        self.state
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

    fn block_sweep(&self) -> Arc<SaveGate> {
        let gate = SaveGate::new();
        *self.sweep_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

impl ReviewEnvironment for FakeEnvironment {
    fn review_refusal(&self, _session_id: &str) -> Result<(), String> {
        self.review_refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map_or(Ok(()), Err)
    }

    fn enqueue_verdict(
        &self,
        _session: &str,
        event: &mj_core::mailbox::MailboxEvent,
    ) -> Result<(), String> {
        let mut mailbox = self.mailbox.lock().unwrap();
        if !mailbox.iter().any(|queued| queued.key == event.key) {
            mailbox.push(event.clone());
        }
        Ok(())
    }
    fn reviewer_children(&self) -> Result<Vec<ReviewerChildRelation>, String> {
        Ok(self.child_relations.lock().unwrap().clone())
    }

    fn active_groups(&self) -> Result<Vec<(String, TurnReviewState)>, String> {
        if let Some(gate) = self.sweep_gate.lock().unwrap().clone() {
            gate.wait();
        }
        let state = self.state();
        if state.active_group.is_none() {
            return Ok(Vec::new());
        }
        Ok(self
            .owner
            .lock()
            .unwrap()
            .clone()
            .map(|owner| vec![(owner, state)])
            .unwrap_or_default())
    }

    fn pending_followups(&self) -> Result<Vec<String>, String> {
        if !self.state().pending_followup {
            return Ok(Vec::new());
        }
        Ok(self.owner.lock().unwrap().clone().into_iter().collect())
    }
    fn register_child(
        &self,
        session: &str,
        reviewer: &ReviewerIdentity,
        prompt: String,
        request_key: String,
        working_directory: Option<PathBuf>,
    ) -> Result<String, String> {
        let gate = self.register_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        if let Some(error) = self.register_error.lock().unwrap().take() {
            return Err(error);
        }
        self.registered_working_directories
            .lock()
            .unwrap()
            .push(working_directory);
        let canonical_key = mj_core::subagent::SubagentKind::Reviewer.request_key(&request_key);
        let mut relations = self.child_relations.lock().unwrap();
        if let Some(existing) = relations.iter().find(|relation| {
            relation.parent_session_id == session && relation.request_key == canonical_key
        }) {
            return Ok(existing.child_session_id.clone());
        }
        let mut children = self.children.lock().unwrap();
        let child = format!("{session}-child-{}", children.len());
        children.push((child.clone(), reviewer.clone(), prompt));
        relations.push(ReviewerChildRelation {
            parent_session_id: session.to_owned(),
            request_key: canonical_key,
            child_session_id: child.clone(),
        });
        drop(children);
        drop(relations);
        let registered_gate = self.registered_gate.lock().unwrap().clone();
        if let Some(gate) = registered_gate {
            gate.wait();
        }
        Ok(child)
    }
    fn start_child<'a>(
        &'a self,
        child: &'a str,
    ) -> mj_client::session::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.started.lock().unwrap().push(child.to_owned());
            self.start_error.lock().unwrap().take().map_or(Ok(()), Err)
        })
    }
    fn close_child<'a>(
        &'a self,
        child: &'a str,
    ) -> mj_client::session::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let Some(error) = self.close_error.lock().unwrap().take() {
                return Err(error);
            }
            self.closed.lock().unwrap().push(child.to_owned());
            Ok(())
        })
    }

    fn check(&self, _session_id: &str, _profile: &str) -> Result<(), String> {
        Ok(())
    }

    fn is_subagent(&self, _session_id: &str) -> bool {
        if let Some(gate) = self.classify_gate.lock().unwrap().clone() {
            gate.wait();
        }
        self.subagent.load(std::sync::atomic::Ordering::Acquire)
    }

    fn resolve<'a>(
        &'a self,
        _handle: ManagedSessionHandle,
        config: ReviewConfig,
        _cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<Vec<mj_core::review::settings::ResolvedReviewSettings>, String>,
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
            Ok(config
                .reviewers
                .iter()
                .map(|choice| mj_core::review::settings::ResolvedReviewSettings {
                    profile: "reviewer".into(),
                    main: mj_core::review::settings::ReviewModelSettings {
                        model: Some(choice.model.clone()),
                        effort: choice.effort.clone(),
                        fast_mode: false,
                    },
                    ..Default::default()
                })
                .collect())
        })
    }

    fn load_state(&self, _session_id: &str) -> Result<TurnReviewState, String> {
        Ok(self.state())
    }

    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String> {
        *self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id.to_owned());
        let gate = self
            .save_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        if let Some(error) = self.save_error.lock().unwrap().take() {
            return Err(error);
        }
        if state.active_group.is_none()
            && state.active.is_none()
            && let Some(error) = self.final_save_error.lock().unwrap().take()
        {
            return Err(error);
        }
        let mut stored = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = state.clone();
        if !(stored.active_group.is_none() && next.active_group.is_some()) {
            next.pending_followup |= stored.pending_followup;
        }
        *stored = next.clone();
        drop(stored);
        self.writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((next, std::thread::current().id()));
        Ok(())
    }

    fn mark_followup(&self, session_id: &str) -> Result<(), String> {
        *self.owner.lock().unwrap() = Some(session_id.to_owned());
        self.state.lock().unwrap().pending_followup = true;
        Ok(())
    }

    fn clear_interrupted(&self) -> Result<Vec<String>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interrupted = state.active_group.is_none()
            && (state.active.is_some() || state.pending_forward.is_some());
        if state.active_group.is_none() {
            state.active = None;
        }
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(owner.filter(|_| interrupted).into_iter().collect())
    }

    fn hold_background_work<'a>(
        &'a self,
        _session_id: &'a str,
        _deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, Option<BackgroundHold>> {
        self.background_holds
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.live_holds
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let hold: BackgroundHold = Box::new(LiveHold(self.live_holds.clone()));
        Box::pin(async move { Some(hold) })
    }
}

/// Counts itself out of the fake's live holds when the host drops it.
struct LiveHold(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for LiveHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// R4-9: a turn that ended with a command still running in the background
/// also started the automatic recovery copy, which took the session's lease
/// while the review was choosing its reviewer. The review gave up with "Turn
/// review did not start: another operation was using the session". It now
/// waits for that background work and tries again, within a bound.
// Hard-won: 4db540d3: review gave up while an active recovery copy briefly refused attachment
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

    // Preparation captures the change before it chooses a reviewer (I2-10).
    // Only a turn that changed something goes on to choose one.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert_eq!(
        environment.background_holds(),
        1,
        "the review holds background work off before it captures"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
        }],
    }));

    // The copy's lease cancels the first choice. The review waits, chooses
    // again, opens, and starts work on the capture.
    let _child = wait_for_child(&environment, &host, session).await;
    assert_eq!(
        environment.background_holds(),
        1,
        "the retried choice runs under the same hold"
    );
    assert_eq!(
        environment.live_holds(),
        1,
        "the open review keeps its hold"
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
    // Preparation captures the change before it chooses a reviewer (I2-10).
    // Only a turn that changed something reaches the refused choice.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
        }],
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while environment.live_holds() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the refused review releases prompts");
    assert_eq!(environment.background_holds(), 1);
    assert_eq!(
        environment.live_holds(),
        0,
        "a review that could not start releases its hold"
    );
    host.shutdown().await.unwrap();
}

fn armed(_profile: Option<&str>) -> ReviewConfigSource {
    armed_count(1)
}

fn armed_count(count: usize) -> ReviewConfigSource {
    Arc::new(move |_| ReviewConfig {
        trigger: mj_core::config::ReviewTrigger::Turn,
        reviewers: vec![
            mj_core::config::SubagentChoice {
                model: "test-reviewer".into(),
                effort: None
            };
            count
        ],
    })
}

async fn wait_for_group_children(
    environment: &FakeEnvironment,
    host: &TurnReviewHost,
    session: &str,
    count: usize,
) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let children: Vec<_> = environment
                .state()
                .active_group
                .as_ref()
                .map(|group| {
                    group
                        .registrations
                        .iter()
                        .filter_map(|item| item.child_session_id.clone())
                        .collect()
                })
                .unwrap_or_default();
            if children.len() == count
                && children
                    .iter()
                    .all(|child| environment.started.lock().unwrap().contains(child))
                && host.view(session).is_some()
            {
                return children;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every reviewer child starts")
}

#[tokio::test]
async fn an_unreviewable_project_is_refused_before_the_worker_is_asked_to_capture() {
    let session = session_id("plainreview");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    environment.refuse_review("this session works in a directory without a committed Git checkout");
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment,
    );
    host.observe(
        session,
        &view(session, mj_core::state::MaterializedExecutionState::Idle),
    );

    let refusal = host
        .start(session, true)
        .await
        .expect_err("a plain directory cannot be reviewed");
    assert!(
        refusal.0.contains("without a committed Git checkout"),
        "{refusal:?}"
    );
    let capture = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if let RemoteSessionRequest::Reviewer { action, .. } = manager.next().await {
                break action;
            }
        }
    })
    .await;
    assert!(
        capture.is_err(),
        "the worker must not be asked to capture an unreviewable project"
    );
    host.shutdown().await.unwrap();
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

/// I2-9: a Mjolnir sub-agent's turn is reviewed through its parent's turn.
/// Reviewing the child on its own raced the parent's lifecycle operations
/// and posted their internal refusals into the child's transcript.
// Hard-won: 1f754710: sub-agent turns incorrectly triggered their own review lifecycle
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
    let view = host.view(&session);
    assert!(
        view.as_ref()
            .is_none_or(|view| !view.status.contains("did not start")),
        "{view:?}"
    );
    host.shutdown().await.unwrap();
}

/// Resolutions are gated on the verdict the review actually reached, in
/// the host rather than in any surface, so every surface gets the same
/// answer.
// Hard-won: 1f754710: lifecycle cancellation was presented as an internal review failure
#[test]
fn a_lifecycle_cancellation_is_not_shown_as_an_internal_error() {
    let notice = start_refusal_notice("reviewer operation cancelled for session lifecycle change");
    assert!(!notice.contains("lifecycle"), "{notice}");
    assert!(
        notice.contains("another operation was using the session"),
        "{notice}"
    );
}

/// Launch finding I2-10: with review on, a Codex turn that changed no files
/// showed "Preparing reviewer…" and held prompts for two minutes before
/// "Nothing to review: the turn changed no files". Choosing a reviewer is the
/// slow step, and a turn with nothing to review needs none, so the capture
/// comes first and an empty one resolves without waiting for a reviewer.
// Hard-won: f13227b3: an empty review waited for a reviewer and delayed completion
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
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: Some("reviewed-tree".to_owned()),
            current_tree: "reviewed-tree".to_owned(),
            patch: String::new(),
            diffstat: "0 files changed".to_owned(),
            changed_lines: 0,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
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
        while host.view(session).is_some() {
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
// Hard-won: 66888613: a temporary recovery-copy refusal delayed capture and produced an empty review
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
        environment.background_holds(),
        1,
        "the retried capture runs under the same hold"
    );
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: std::path::PathBuf::from("/workspace/app"),
            baseline_tree: Some("reviewed-tree".to_owned()),
            current_tree: "reviewed-tree".to_owned(),
            patch: String::new(),
            diffstat: "0 files changed".to_owned(),
            changed_lines: 0,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
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
        while host.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review releases the session's prompts");
    host.shutdown().await.unwrap();
}

/// A new primary turn is reflected by the live actor recheck, so preparation
/// refuses instead of reviewing a stale idle snapshot.
#[tokio::test]
async fn preparation_rechecks_the_live_actor_without_holding_prompts() {
    let session = session_id("preparelive");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let gate = SaveGate::new();
    *environment.classify_gate.lock().unwrap() = Some(gate.clone());
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );

    finish_a_turn(&manager, &host).await;
    tokio::time::timeout(Duration::from_secs(5), gate.entered())
        .await
        .unwrap();
    assert!(host.view(session).is_some());

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
    gate.release();

    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a stale preparation is refused");
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
    let config: ReviewConfigSource = Arc::new(|_| ReviewConfig {
        trigger: mj_core::config::ReviewTrigger::Off,
        reviewers: vec![mj_core::config::SubagentChoice {
            model: "test-reviewer".into(),
            effort: None,
        }],
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
    // Preparation captures the change before the review opens (I2-10); a
    // turn with a change keeps the review open.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
        }],
    }));
    starting
        .await
        .expect("start task")
        .expect("the retained idle edge admits the review");
    assert!(host.view(session).is_some());
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
    // Preparation captures the change before the review opens (I2-10).
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let open_gate = environment.block_saves();
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
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
    // The child stays open without a handback while shutdown drains persistence.
    let _child = wait_for_child(&environment, &host, session).await;
    assert!(environment.state().active.is_some());
    assert!(host.view(session).is_some());

    let close_gate = environment.block_saves();
    let first_host = host.clone();
    let second_host = host.clone();
    let first = tokio::spawn(async move { first_host.shutdown().await });
    let second = tokio::spawn(async move { second_host.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), close_gate.entered())
        .await
        .expect("shutdown queues the final inactive state");
    assert!(!first.is_finished(), "shutdown drains the blocked write");
    assert!(
        !second.is_finished(),
        "concurrent shutdown joins the same drain"
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

    assert!(
        environment.state().active.is_some(),
        "the in-flight marker survives shutdown, so the next daemon can tell the person"
    );
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
            .is_some_and(|(state, _)| state.active.is_some())
    );
    let test_thread = std::thread::current().id();
    assert!(
        writes.iter().all(|(_, writer)| *writer != test_thread),
        "synchronous database writes run off the Tokio host thread"
    );
}

/// A restart keeps a durable review open and leaves its baseline untouched
/// while the busy reviewer continues under the replacement daemon.
// Hard-won: c974a33c: daemon restart erased an open review marker
#[tokio::test]
async fn a_review_open_at_a_restart_keeps_its_child_and_baseline() {
    let session = session_id("restartnotice");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    {
        let mut state = environment.state.lock().unwrap();
        state
            .baselines
            .insert("/workspace/app".into(), "before-turn".into());
    }
    let old = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    finish_a_turn(&manager, &old).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("before-turn".to_owned()),
            current_tree: "after-turn".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
        }],
    }));
    let child = wait_for_child(&environment, &old, session).await;
    assert!(environment.state().active.is_some());

    // Two more completed turns while the review is open coalesce into one
    // durable follow-up. Restart before the first verdict is delivered.
    finish_a_turn(&manager, &old).await;
    finish_a_turn(&manager, &old).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment.state().pending_followup {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the follow-up is durable before restart");

    old.shutdown().await.unwrap();

    let new = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    new.observe(session, &view(session, MaterializedExecutionState::Idle));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if new
                .view(session)
                .is_some_and(|view| view.reviewer_children().any(|id| id == child))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the new daemon restores the open review");
    let state = environment.state();
    assert!(
        !environment.closed.lock().unwrap().contains(&child),
        "restart retains the owned reviewer"
    );
    assert!(state.active_group.is_some());
    assert_eq!(
        state.baselines[&PathBuf::from("/workspace/app")],
        "before-turn",
        "the baseline did not advance, so the next review covers both turns"
    );
    finish_child(&new, &environment, session, &child, Some("No findings.")).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: PathBuf::from("/workspace/app"),
                baseline_tree: Some("after-turn".into()),
                current_tree: "latest".into(),
                patch: "+later".into(),
                changed_lines: 1,
                diffstat: "1 file changed".into(),
                ..Default::default()
            }],
        }))
        .unwrap();
    let followup_child = wait_for_child(&environment, &new, session).await;
    assert_ne!(followup_child, child);
    assert!(!environment.state().pending_followup);
    finish_child(
        &new,
        &environment,
        session,
        &followup_child,
        Some("No findings."),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while new.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the one coalesced follow-up closes");
    assert_eq!(environment.children.lock().unwrap().len(), 2);
    new.shutdown().await.unwrap();
}

/// I1-8 as observed: the restart came while the review was still choosing its
/// reviewer, before any review state was written, and the row read
/// `Reviewing`.
// Hard-won: c974a33c: daemon restart lost the marker for a review still preparing
#[tokio::test]
async fn a_review_still_preparing_at_a_restart_is_reported_by_the_next_daemon() {
    let session = session_id("restartprep");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    {
        let mut state = environment.state.lock().unwrap();
        state
            .baselines
            .insert("/workspace/app".into(), "before-turn".into());
    }
    let old = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    finish_a_turn(&manager, &old).await;
    // Preparation asks the reviewer for its status; leave it unanswered.
    let (_, _, _status_reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(old.view(session).is_some(), "the review is preparing");

    old.shutdown().await.unwrap();

    let new = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    new.observe(session, &view(session, MaterializedExecutionState::Idle));
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
    .expect("the new daemon tells the person the review was cancelled");
    assert_eq!(
        notice,
        "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes"
    );
    assert_eq!(
        environment.state().baselines[&PathBuf::from("/workspace/app")],
        "before-turn"
    );
    new.shutdown().await.unwrap();
}

/// Queued primary prompts do not block independent review preparation.
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
        provenance: Default::default(),
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
        .send(HostEvent::Interrupted {
            interrupted: vec![session.clone()],
        })
        .unwrap();
    let RemoteSessionRequest::Submit {
        command_id, reply, ..
    } = manager.next().await
    else {
        panic!("recovery submits the pending handoff directly");
    };
    assert_eq!(command_id, pending.command_id);
    assert_eq!(environment.state().pending_forward, Some(pending.clone()));
    assert_eq!(
        environment.state().baselines[&PathBuf::from("/workspace/app")],
        "old"
    );
    assert!(
        host.resolve(&session, Resolution::Forwarded).await.is_err(),
        "duplicate Forward is not another submission"
    );
    assert!(
        host.resolve(&session, Resolution::Cancelled).await.is_err(),
        "an unknown delivery cannot be undone"
    );
    reply
        .send(Err("primary temporarily unavailable".into()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !host.view(&session).is_some_and(|view| {
            matches!(
                view.phase,
                TurnReviewPhase::Forwarding { error: Some(_), .. }
            )
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejection remains actionable");
    assert_eq!(environment.state().pending_forward, Some(pending.clone()));
    let RemoteSessionRequest::Submit {
        command_id, reply, ..
    } = tokio::time::timeout(Duration::from_secs(5), manager.next())
        .await
        .unwrap()
    else {
        panic!("automatic retry submits the same handoff");
    };
    assert_eq!(command_id, pending.command_id);
    let (stale_reply, stale_answer) = oneshot::channel();
    host.events
        .send(HostEvent::ReviewerFinished {
            session_id: session.clone(),
            child_session_id: "old-review-child".into(),
            answer: None,
            reply: stale_reply,
        })
        .unwrap();
    stale_answer.await.unwrap().unwrap();
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
    tokio::time::timeout(Duration::from_secs(2), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted handoff closes");
    let state = environment.state();
    assert!(state.pending_forward.is_none());
    assert!(state.prior_review.is_some());
    assert_eq!(state.baselines[&PathBuf::from("/workspace/app")], "new");
    assert!(environment.children.lock().unwrap().is_empty());
    host.shutdown().await.unwrap();
}

// Hard-won: f47500b6c: a turn ending during preparation lost its follow-up
#[tokio::test]
async fn turn_finishing_during_review_preparation_starts_one_followup() {
    let session = session_id("prepfollowup");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    finish_a_turn(&manager, &host).await;
    let (_, _, first_capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    finish_a_turn(&manager, &host).await;
    finish_a_turn(&manager, &host).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment.state().pending_followup {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("follow-up is durable during preparation");
    first_capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: PathBuf::from("/workspace/app"),
                baseline_tree: Some("base".into()),
                current_tree: "first".into(),
                patch: "+first".into(),
                changed_lines: 1,
                diffstat: "1 file changed".into(),
                ..Default::default()
            }],
        }))
        .unwrap();
    let child = wait_for_child(&environment, &host, &session).await;
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    let (_, _, second_capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    second_capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: PathBuf::from("/workspace/app"),
                baseline_tree: Some("first".into()),
                current_tree: "second".into(),
                patch: "+second".into(),
                changed_lines: 1,
                diffstat: "1 file changed".into(),
                ..Default::default()
            }],
        }))
        .unwrap();
    let child = wait_for_child(&environment, &host, &session).await;
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    assert_eq!(environment.children.lock().unwrap().len(), 2);
    host.shutdown().await.unwrap();
}

// Hard-won: f47500b6c: the live queued-prompt snapshot refused review with a spurious notice
#[tokio::test]
async fn queued_prompts_do_not_hold_a_review_back() {
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
    manager
        .publisher
        .publish(session.to_owned(), queued.clone())
        .await
        .unwrap();
    host.observe(session, &queued);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment.state().pending_followup {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued turn retains durable follow-up");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
            .await
            .is_err(),
        "queued prompts must not emit a refusal notice or start capture"
    );
    let idle = view(session, mj_core::state::MaterializedExecutionState::Idle);
    manager
        .publisher
        .publish(session.to_owned(), idle.clone())
        .await
        .unwrap();
    host.observe(session, &idle);
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: Vec::new(),
    }));
    host.shutdown().await.unwrap();
}

/// A reviewer launch failure closes on its own and preserves the baseline.
// Hard-won: #1448: failed reviewer starts retained borrowed targets after their review ended.
#[tokio::test]
async fn a_failed_review_closes_without_advancing_coverage() {
    let session = session_id("failedrole0");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    *environment.start_error.lock().unwrap() = Some("review harness failed to launch".to_owned());
    *environment.close_error.lock().unwrap() = Some("temporary close admission refusal".into());
    finish_a_turn(&manager, &host).await;

    // Preparation captures the change before the review opens (I2-10), so
    // the active marker is durable by the time a reviewer starts rather than
    // by the capture.
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let _ = reply.send(Ok(ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            base_commit: None,
            head_commit: None,
            base_kind: None,
            root: PathBuf::from("/workspace/app"),
            baseline_tree: Some("base".to_owned()),
            current_tree: "new".to_owned(),
            patch: "diff --git a/a b/a\n@@\n+one\n".to_owned(),
            diffstat: "1 file changed, 1 insertion(+)".to_owned(),
            changed_lines: 1,
            files: Vec::new(),
            merged_in: Vec::new(),
            unattributed: None,
            push_review_warning: None,
        }],
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.view(session).is_none() && environment.state().active.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failure releases and persists the turn");

    tokio::time::timeout(Duration::from_secs(5), async {
        while environment.closed.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a failed reviewer is closed, retrying a transient cleanup refusal");
    assert_eq!(environment.closed.lock().unwrap().len(), 1);
    assert!(environment.state().baselines.is_empty());
    // Hard-won: f47500b6c: the close notice hid the reviewer launch failure reason.
    let notice = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } = manager.next().await
            {
                let _ = reply.send(Ok(1));
                if text.starts_with("Review failed:") {
                    break text;
                }
            }
        }
    })
    .await
    .expect("failed review records a notice");
    assert!(
        notice.contains("review harness failed to launch"),
        "{notice}"
    );

    host.shutdown().await.expect("shutdown the host");
}

/// A relay event carrying one agent message, for the answer a role's
/// journal reports.
/// The child's single handback settles the review and its captured baseline.
#[tokio::test]
async fn a_clean_reviewer_report_resolves_the_review() {
    let session = session_id("cleanreport");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let children = environment.children.lock().unwrap().clone();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].1.profile, "reviewer");
    assert!(
        children[0]
            .2
            .contains("git -C /workspace/app diff --no-ext-diff --ignore-submodules=none base new")
    );
    assert!(children[0].2.contains("1 file changed, 1 insertion(+)"));
    assert!(!children[0].2.contains("+one"));
    assert!(children[0].2.contains("single handback call"));
    assert!(children[0].2.contains("Do not edit files or push"));
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let state = environment.state();
    assert_eq!(state.baselines[&PathBuf::from("/workspace/app")], "new");
    assert_eq!(state.reviewed_through_ordinal, 12);
    assert!(state.active.is_none());
    assert!(environment.closed.lock().unwrap().contains(&child));
    assert_eq!(environment.live_holds(), 0);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn primary_prompt_and_corrective_delivery_queue_while_review_is_running() {
    let session = session_id("nonblocking-review");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let handle = manager.control.session(session.clone()).await.unwrap();
    let user_submit = tokio::spawn(async move {
        handle
            .submit(
                "user-while-reviewing".into(),
                prompt_command("next task".into()),
            )
            .await
    });
    let RemoteSessionRequest::Submit {
        command_id,
        command: RelayCommand::Prompt { .. },
        reply,
        ..
    } = manager.next().await
    else {
        panic!("the primary prompt is admitted during review");
    };
    assert_eq!(command_id, "user-while-reviewing");
    reply.send(Ok(13)).unwrap();
    user_submit.await.unwrap().unwrap();
    assert!(host.view(&session).is_some(), "the review continues");

    host.observe(
        &session,
        &view(
            &session,
            MaterializedExecutionState::Running { started_at_ms: 1 },
        ),
    );
    finish_child(
        &host,
        &environment,
        &session,
        &child,
        Some("[P2] src/lib.rs:1 -- fix boundary"),
    )
    .await;
    let RemoteSessionRequest::Submit {
        command: RelayCommand::Prompt { .. },
        reply,
        ..
    } = manager.next().await
    else {
        panic!("findings enter the ordinary prompt queue, not steering");
    };
    reply.send(Ok(14)).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_corrective_delivery_retries_one_command_then_closes_without_coverage() {
    let session = session_id("forward-retries");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    finish_child(
        &host,
        &environment,
        &session,
        &child,
        Some("[P2] src/lib.rs:1 -- defect"),
    )
    .await;
    let mut command_id = None;
    for _ in 0..3 {
        let RemoteSessionRequest::Submit {
            command_id: next,
            command: RelayCommand::Prompt { .. },
            reply,
            ..
        } = tokio::time::timeout(Duration::from_secs(10), manager.next())
            .await
            .unwrap()
        else {
            panic!("a corrective attempt is an ordinary prompt");
        };
        if let Some(first) = &command_id {
            assert_eq!(&next, first, "a retry must use the same command id");
        } else {
            command_id = Some(next);
        }
        reply.send(Err("primary unavailable".into())).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the third rejection closes the review");
    let state = environment.state();
    assert!(state.baselines.is_empty());
    assert!(state.pending_forward.is_none());
    assert_eq!(state.forward_attempts, 3);
    let notice = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } = manager.next().await
            {
                reply.send(Ok(1)).unwrap();
                if text.contains("could not be delivered") {
                    break text;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(notice.contains("unreviewed"));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn reviewer_outcome_is_saved_before_handback_acknowledgement() {
    let session = session_id("outcome-save");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let gate = environment.block_saves();
    let finished = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move {
            host.reviewer_finished(session, child, Some("No findings.".into()))
                .await
        })
    };
    gate.entered().await;
    assert!(
        !finished.is_finished(),
        "handback waits for durable outcome"
    );
    assert!(
        environment
            .state()
            .active_group
            .as_ref()
            .unwrap()
            .driver
            .roles[0]
            .outcome
            .is_none()
    );
    gate.release();
    finished.await.unwrap().unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_outcome_save_can_retry_same_child_without_losing_coverage() {
    let session = session_id("outcome-save-retry");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    *environment.save_error.lock().unwrap() = Some("database busy".into());
    let first = host
        .reviewer_finished(session.clone(), child.clone(), Some("No findings.".into()))
        .await;
    assert!(first.unwrap_err().contains("database busy"));
    host.reviewer_finished(session.clone(), child, Some("No findings.".into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while environment
            .state()
            .baselines
            .get(&PathBuf::from("/workspace/app"))
            .map(String::as_str)
            != Some("new")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn parallel_findings_keep_launch_order_when_children_finish_out_of_order() {
    let session = session_id("parallel-order");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let choices = vec![
        mj_core::config::SubagentChoice {
            model: "gpt-6-terra".into(),
            effort: Some("xhigh".into()),
        },
        mj_core::config::SubagentChoice {
            model: "gpt-6-sol".into(),
            effort: Some("medium".into()),
        },
    ];
    let config: ReviewConfigSource = Arc::new(move |_| ReviewConfig {
        trigger: mj_core::config::ReviewTrigger::Turn,
        reviewers: choices.clone(),
    });
    let host = TurnReviewHost::spawn_in(manager.control.clone(), config, environment.clone());
    open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let children = wait_for_group_children(&environment, &host, &session, 2).await;
    let models = {
        let registered = environment.children.lock().unwrap();
        children
            .iter()
            .map(|child| {
                registered
                    .iter()
                    .find(|(id, _, _)| id == child)
                    .unwrap()
                    .1
                    .main
                    .model
                    .clone()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        models,
        [Some("gpt-6-terra".into()), Some("gpt-6-sol".into())]
    );
    finish_child(
        &host,
        &environment,
        &session,
        &children[1],
        Some("second defect"),
    )
    .await;
    assert!(
        host.view(&session).unwrap().verdict.is_none(),
        "one unfinished peer cannot complete the group"
    );
    finish_child(
        &host,
        &environment,
        &session,
        &children[0],
        Some("first defect"),
    )
    .await;
    let prompt = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match manager.next().await {
                RemoteSessionRequest::Submit {
                    command: RelayCommand::Prompt { prompt },
                    reply,
                    ..
                } => {
                    reply.send(Ok(42)).unwrap();
                    return format!("{prompt:?}");
                }
                RemoteSessionRequest::Reviewer { action, reply, .. } => {
                    reply.send(answer_for(&action)).unwrap();
                }
                RemoteSessionRequest::Submit { reply, .. } => {
                    reply.send(Ok(1)).unwrap();
                }
                other => panic!("unexpected request {}", other.session_id()),
            }
        }
    })
    .await
    .unwrap();
    assert!(prompt.find("first defect").unwrap() < prompt.find("second defect").unwrap());
    assert!(prompt.contains(&children[0]) && prompt.contains(&children[1]));
    assert!(prompt.contains("may overlap"));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn clean_and_failed_group_closes_without_advancing_baseline() {
    let session = session_id("parallel-failed-hold");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), armed_count(2), environment.clone());
    open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let children = wait_for_group_children(&environment, &host, &session, 2).await;
    finish_child(
        &host,
        &environment,
        &session,
        &children[0],
        Some("No findings."),
    )
    .await;
    finish_child(&host, &environment, &session, &children[1], None).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() || environment.state().active.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the failed group closes itself");
    assert!(environment.state().baselines.is_empty());
    assert!(environment.state().active.is_none());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn reviewer_questions_route_through_both_registered_children() {
    let session = session_id("parallel-questions");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), armed_count(2), environment.clone());
    open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let children = wait_for_group_children(&environment, &host, &session, 2).await;
    for child in &children {
        observe_question(&host, child, "Which file?");
        let target = host
            .question_target(&session, &format!("review.{child}.question-1"))
            .await;
        assert_eq!(target, Some((child.clone(), "question-1".into())));
    }
    assert!(
        host.question_target(&session, "review.unregistered.question-1")
            .await
            .is_none()
    );
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = host.view(&session).unwrap();
            if view.questions.len() == 2 {
                return view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        view.questions
            .iter()
            .map(|item| item.role.clone())
            .collect::<Vec<_>>(),
        children
    );
    host.shutdown().await.unwrap();
}

async fn wait_for_child(
    environment: &FakeEnvironment,
    host: &TurnReviewHost,
    session: &str,
) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(child) = host
                .view(session)
                .and_then(|view| view.reviewer_children().next().map(str::to_owned))
                && environment.started.lock().unwrap().contains(&child)
            {
                assert_eq!(
                    environment
                        .state()
                        .active_group
                        .as_ref()
                        .and_then(|group| group.registrations.first())
                        .and_then(|item| item.child_session_id.as_deref()),
                    Some(child.as_str()),
                    "the child is durable before it starts"
                );
                return child;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the reviewer child starts")
}

async fn open_a_review_to_its_first_poll(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
) -> String {
    finish_a_turn(manager, host).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                base_commit: None,
                head_commit: None,
                base_kind: None,
                root: PathBuf::from("/workspace/app"),
                baseline_tree: Some("base".into()),
                current_tree: "new".into(),
                patch: "+one".into(),
                changed_lines: 1,
                diffstat: "1 file changed, 1 insertion(+)".into(),
                ..Default::default()
            }],
        }))
        .unwrap();
    wait_for_child(environment, host, &manager.session).await
}

async fn finish_child(
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
    session: &str,
    child: &str,
    answer: Option<&str>,
) {
    host.reviewer_finished(
        session.to_owned(),
        child.to_owned(),
        answer.map(str::to_owned),
    )
    .await
    .unwrap();
    environment.close_child(child).await.unwrap();
}

fn observe_question(host: &TurnReviewHost, child: &str, message: &str) {
    let mut snapshot = MaterializedSession::empty(child);
    snapshot
        .pending_elicitations
        .push(mj_core::elicitation::ElicitationRequest {
            id: "decline-1".into(),
            message: message.into(),
            title: None,
            description: None,
            fields: Vec::new(),
            tool_permission: None,
        });
    host.events
        .send(HostEvent::View {
            session_id: child.into(),
            snapshot: Some(Box::new(snapshot)),
            prompt_driven: false,
        })
        .unwrap();
}

/// RVC-3: the Auto reviewer's harness declined and asked "Retry with
/// claude-opus-4-8?". Only the terminal's review pane showed that form; the
/// session list, `mj elicitations` and the phone did not. The host projects
/// the form from the role's journal into the review it publishes, and the
/// review reads as a question rather than as work in progress.
// Hard-won: 8ab93dd3: review questions appeared only in the terminal, not API and phone projections
#[tokio::test]
async fn a_reviewers_question_is_published_with_its_review() {
    let session = session_id("reviewerform");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let message = "claude-fable-5-1 declined this request (cyber). Retry with claude-opus-4-8?";
    observe_question(&host, &child, message);
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(view) = host.view(session).filter(|view| !view.questions.is_empty()) {
                return view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the reviewer's form is published with the review");
    assert_eq!(view.questions.len(), 1);
    assert_eq!(view.questions[0].role, child);
    assert_eq!(view.questions[0].request.message, message);
    assert_eq!(view.activity_label(), Some("Question"));
    assert!(
        !view.is_working(),
        "a review waiting on a person is not working"
    );
    host.shutdown().await.expect("shutdown the host");
}

/// A review asked for while the open one waits on a person names that
/// question rather than only saying a review is open.
#[tokio::test]
async fn a_review_asked_for_while_the_open_one_waits_on_a_question_says_so() {
    let session = session_id("openwaiting");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    observe_question(&host, &child, "Retry with claude-opus-4-8?");
    tokio::time::timeout(Duration::from_secs(5), async {
        while host
            .view(session)
            .is_none_or(|view| view.questions.is_empty())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the question is published");
    let refusal = host
        .start(session, true)
        .await
        .expect_err("one review at a time");
    assert_eq!(
        refusal.0,
        "the previous review is waiting for your answer to a question \
         (\"Retry with claude-opus-4-8?\"); answer it in the virtual workspace"
    );
    host.shutdown().await.expect("shutdown the host");
}

/// A saved group's busy child remains owned after a daemon restart; its
/// handback can race the startup sweep without losing coverage.
#[tokio::test]
async fn a_reviewer_handback_arriving_after_restart_completes_the_saved_group() {
    let session = session_id("reattachreview");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let old = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &old, &environment).await;
    old.shutdown().await.unwrap();
    let new = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    new.observe(&session, &view(&session, MaterializedExecutionState::Idle));
    finish_child(&new, &environment, &session, &child, Some("No findings.")).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while new.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        environment.state().baselines[&PathBuf::from("/workspace/app")],
        "new"
    );
    assert!(environment.state().active_group.is_none());
    new.shutdown().await.unwrap();
}

/// Registration is durable in the sub-agent index before the review row can
/// save the returned child ID. Startup must recognize the key in that gap.
#[tokio::test]
async fn restart_between_reviewer_registration_and_id_save_keeps_the_adopted_child() {
    let session = session_id("registrationgap");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let gate = SaveGate::new();
    *environment.registered_gate.lock().unwrap() = Some(gate.clone());
    let old = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    finish_a_turn(&manager, &old).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: "/workspace/app".into(),
                baseline_tree: Some("base".into()),
                current_tree: "new".into(),
                patch: "+one".into(),
                changed_lines: 1,
                ..Default::default()
            }],
        }))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate.entered())
        .await
        .unwrap();
    let relation = environment.child_relations.lock().unwrap()[0].clone();
    let group = environment.state().active_group.unwrap();
    assert!(group.registrations[0].child_session_id.is_none());
    assert_eq!(
        relation.request_key,
        mj_core::subagent::SubagentKind::Reviewer.request_key(&group.registrations[0].request_key)
    );

    old.shutdown().await.unwrap();
    *environment.registered_gate.lock().unwrap() = None;
    let new = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = wait_for_child(&environment, &new, &session).await;
    assert_eq!(child, relation.child_session_id);
    assert_eq!(environment.children.lock().unwrap().len(), 1);
    gate.release(); // the retired host's event channel is closed now
    tokio::task::yield_now().await;
    assert!(environment.closed.lock().unwrap().is_empty());
    new.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_restores_group_before_push_or_override_admission() {
    let session = session_id("startup-group-gate");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let seed = mj_core::review::driver::TurnReviewSeed {
        task: "inspect the push".into(),
        user_messages: Vec::new(),
        baselines: Default::default(),
        through_ordinal: 1,
        prior_review: None,
    };
    let (mut driver, _) = TurnReviewDriver::start(
        seed,
        "group-1".into(),
        std::num::NonZeroUsize::new(1).unwrap(),
    );
    driver.delta_captured(vec![push_delta("/workspace/app", "newtree")]);
    let state = TurnReviewState {
        pending_followup: false,
        forward_attempts: 0,
        active: Some("group-1".into()),
        active_group: Some(mj_core::storage::ActiveReviewGroup {
            driver: driver.snapshot(),
            reviewers: vec![ReviewerIdentity::default()],
            scope: mj_core::relay::CaptureScope::PushableCommits,
            registrations: vec![mj_core::storage::ReviewerRegistration {
                role: "reviewer-1".into(),
                request_key: "stable-key".into(),
                child_session_id: Some("busy-child".into()),
            }],
        }),
        push_findings: Some(mj_core::storage::PushFindingsRecord {
            review_id: "older-verdict".into(),
            trees: [(
                PathBuf::from("/workspace/app"),
                ("newtree".into(), "bbbbbbb456".into()),
            )]
            .into(),
            synthesis: "older finding".into(),
        }),
        ..Default::default()
    };
    *environment.state.lock().unwrap() = state;
    *environment.owner.lock().unwrap() = Some(session.clone());
    let gate = environment.block_sweep();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    gate.entered().await;
    let push = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.begin_push(&session, None).await })
    };
    let override_request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.override_push(&session, "old finding".into()).await })
    };
    tokio::task::yield_now().await;
    assert!(!push.is_finished() && !override_request.is_finished());
    gate.release();
    assert!(
        push.await
            .unwrap()
            .unwrap_err()
            .0
            .contains("review is already open")
    );
    assert!(
        override_request
            .await
            .unwrap()
            .unwrap_err()
            .0
            .contains("review is open")
    );
    assert_eq!(
        environment
            .state()
            .active_group
            .as_ref()
            .unwrap()
            .registrations[0]
            .child_session_id
            .as_deref(),
        Some("busy-child")
    );
    host.shutdown().await.unwrap();
}

// Hard-won: #1448: startup retained failed reviewer roles as if they still owned review work.
#[tokio::test]
async fn startup_reaps_settled_and_orphaned_reviewers_but_preserves_their_busy_peer() {
    let session = session_id("reap-settled");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (mut driver, _) = TurnReviewDriver::start(
        TurnReviewSeed {
            task: "inspect the changes".into(),
            user_messages: Vec::new(),
            baselines: Default::default(),
            through_ordinal: 1,
            prior_review: None,
        },
        "partly-failed-group".into(),
        std::num::NonZeroUsize::new(2).unwrap(),
    );
    driver.delta_captured(vec![push_delta("/workspace/app", "newtree")]);
    driver.role_failed("reviewer-1", Some("failed-child"), "startup failed");
    let registrations: Vec<_> = [("reviewer-1", "failed-child"), ("reviewer-2", "busy-child")]
        .into_iter()
        .map(|(role, child)| mj_core::storage::ReviewerRegistration {
            role: role.into(),
            request_key: format!("key-{child}"),
            child_session_id: Some(child.into()),
        })
        .collect();
    for registration in &registrations {
        environment
            .child_relations
            .lock()
            .unwrap()
            .push(ReviewerChildRelation {
                parent_session_id: session.clone(),
                request_key: mj_core::subagent::SubagentKind::Reviewer
                    .request_key(&registration.request_key),
                child_session_id: registration.child_session_id.clone().unwrap(),
            });
    }
    environment
        .child_relations
        .lock()
        .unwrap()
        .push(ReviewerChildRelation {
            parent_session_id: session.clone(),
            request_key: "reviewer:orphan-key".into(),
            child_session_id: "orphan-child".into(),
        });
    *environment.owner.lock().unwrap() = Some(session.clone());
    *environment.state.lock().unwrap() = TurnReviewState {
        pending_followup: false,
        forward_attempts: 0,
        active: Some("partly-failed-group".into()),
        active_group: Some(mj_core::storage::ActiveReviewGroup {
            driver: driver.snapshot(),
            reviewers: vec![ReviewerIdentity::default(); 2],
            scope: Default::default(),
            registrations,
        }),
        ..Default::default()
    };
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let closed = environment.closed.lock().unwrap().clone();
            let started = environment.started.lock().unwrap().clone();
            assert!(!closed.iter().any(|id| id == "busy-child"));
            assert!(!started.iter().any(|id| id == "failed-child"));
            if closed.iter().any(|id| id == "failed-child")
                && closed.iter().any(|id| id == "orphan-child")
                && started.iter().any(|id| id == "busy-child")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("only the role still awaiting an outcome survives startup");
    host.shutdown().await.unwrap();
    assert!(environment.state().active.is_some());
}

/// A review someone asked for delivers its findings: a findings verdict is
/// forwarded to the primary agent as soon as it is reached, without waiting
/// for a surface to choose Forward.
#[tokio::test]
async fn a_findings_verdict_is_forwarded_without_a_manual_resolve() {
    let session = session_id("autoforward");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    finish_child(
        &host,
        &environment,
        session,
        &child,
        Some("[P1] src/lib.rs:1 -- unbounded retry loop"),
    )
    .await;
    let (command_id, prompt) = loop {
        match manager.next().await {
            RemoteSessionRequest::Submit {
                command_id,
                command: RelayCommand::Prompt { prompt },
                reply,
                ..
            } => {
                let _ = reply.send(Ok(42));
                break (command_id, format!("{prompt:?}"));
            }
            RemoteSessionRequest::Reviewer { action, reply, .. } => {
                let _ = reply.send(answer_for(&action));
            }
            other => panic!("unexpected request {}", other.session_id()),
        }
    };
    assert!(
        command_id.starts_with("review-forward"),
        "the primary receives the review's corrective prompt: {command_id}"
    );
    assert!(
        prompt.contains("unbounded retry loop"),
        "the corrective prompt carries the finding"
    );
    assert!(
        prompt.contains("not independently verified"),
        "the review's findings reach the primary as one reviewer's, unverified: {prompt}"
    );
    assert!(
        host.resolve(session, Resolution::Forwarded).await.is_err(),
        "the findings were already forwarded"
    );
    host.shutdown().await.expect("shutdown the host");
}

/// A child has its own worker, so launching it never takes its parent's
/// reviewer lease. Cancellation still closes the child.
#[tokio::test]
async fn a_reviewer_child_starts_without_taking_its_parents_reviewer_lease() {
    let session = session_id("startrefuse");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    assert!(
        host.view(&session)
            .unwrap()
            .reviewer_children()
            .next()
            .is_some()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
            .await
            .is_err(),
        "launching the child sends no reviewer command to its parent"
    );
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment.closed.lock().unwrap().contains(&child) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reviewer_without_a_handback_fails_without_advancing_the_baseline() {
    let session = session_id("nohandback");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    let child = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    finish_child(&host, &environment, &session, &child, None).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed reviewer resolves itself");
    assert!(environment.state().baselines.is_empty());
    assert!(environment.state().active.is_none());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_trigger_does_not_arm_review_at_the_end_of_a_turn() {
    let session = session_id("pushtrigger");
    let mut manager = FakeManager::new(&session).await;
    let config: ReviewConfigSource = Arc::new(|_| ReviewConfig {
        trigger: mj_core::config::ReviewTrigger::Push,
        ..Default::default()
    });
    let host = TurnReviewHost::spawn_in(manager.control.clone(), config, FakeEnvironment::new());
    finish_a_turn(&manager, &host).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err()
    );
    assert!(host.view(&session).is_none());
    host.shutdown().await.unwrap();
}

fn push_config() -> ReviewConfigSource {
    Arc::new(|_| ReviewConfig {
        trigger: mj_core::config::ReviewTrigger::Push,
        reviewers: vec![mj_core::config::SubagentChoice {
            model: "test-reviewer".into(),
            effort: None,
        }],
    })
}

fn push_config_count(count: usize) -> ReviewConfigSource {
    Arc::new(move |_| ReviewConfig {
        trigger: mj_core::config::ReviewTrigger::Push,
        reviewers: vec![
            mj_core::config::SubagentChoice {
                model: "test-reviewer".into(),
                effort: None
            };
            count
        ],
    })
}

#[tokio::test]
async fn push_path_reaches_capture_and_old_worker_refuses_it() {
    let session = session_id("pushworktree");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let mut running = view(
        &session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(48);
    manager
        .publisher
        .publish(session.clone(), running.clone())
        .await
        .unwrap();
    assert!(
        host.begin_push(&session, Some("../linked".into()))
            .await
            .unwrap_err()
            .0
            .contains("predates worktree review")
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(49);
    manager
        .publisher
        .publish(session.clone(), running)
        .await
        .unwrap();
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.begin_push(&session, Some("../linked".into())).await })
    };
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(
        matches!(action, ReviewerAction::CaptureDelta { repositories: Some(paths), .. } if paths == vec![PathBuf::from("../linked")])
    );
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: "/workspace/linked".into(),
                base_commit: Some("aaaaaaa123".into()),
                head_commit: Some("bbbbbbb456".into()),
                current_tree: "newtree".into(),
                patch: "diff --git a/a b/a\n@@\n+one\n".into(),
                files: vec![mj_core::relay::FileLineChange {
                    path: "a".into(),
                    insertions: 1,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
        .unwrap();
    assert!(request.await.unwrap().is_ok());
    let _child = wait_for_child(&environment, &host, &session).await;
    assert_eq!(
        *environment.registered_working_directories.lock().unwrap(),
        vec![Some(PathBuf::from("/workspace/linked"))]
    );
    host.shutdown().await.unwrap();
}

async fn open_push(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
) -> String {
    let mut running = view(
        &manager.session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(43);
    manager
        .publisher
        .publish(manager.session.clone(), running)
        .await
        .unwrap();
    let request = {
        let host = host.clone();
        let session = manager.session.clone();
        tokio::spawn(async move { host.begin_push(&session, None).await })
    };
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(matches!(
        action,
        ReviewerAction::CaptureDelta {
            repositories: None,
            scope: mj_core::relay::CaptureScope::PushableCommits,
            ..
        }
    ));
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: "/workspace/app".into(),
                base_commit: Some("aaaaaaa123".into()),
                head_commit: Some("bbbbbbb456".into()),
                base_kind: Some("merge base with main".into()),
                baseline_tree: Some("base".into()),
                current_tree: "newtree".into(),
                patch: "diff --git a/a b/a\n@@\n+one\n".into(),
                files: vec![mj_core::relay::FileLineChange {
                    path: "a".into(),
                    insertions: 1,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
        .unwrap();
    let answer = request.await.unwrap().unwrap();
    assert_eq!(answer["accepted"], true);
    assert_eq!(answer["repositories"][0]["base_commit"], "aaaaaaa123");
    assert_eq!(answer["repositories"][0]["head_commit"], "bbbbbbb456");
    assert_eq!(
        answer["repositories"][0]["base_kind"],
        "merge base with main"
    );
    assert_eq!(answer["repositories"][0]["changed_file_count"], 1);
    wait_for_child(environment, host, &manager.session).await
}

// Hard-won: 5dde3143c: child registration refusal was acknowledged as a started review.
#[tokio::test]
async fn review_starts_report_child_registration_refusal_to_their_callers() {
    for push in [false, true] {
        let session = session_id("pushlaunchrefusal");
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        *environment.register_error.lock().unwrap() =
            Some("review child registration refused".into());
        let host = TurnReviewHost::spawn_in(
            manager.control.clone(),
            if push {
                push_config()
            } else {
                armed(Some("reviewer"))
            },
            environment.clone(),
        );
        let mut running = view(
            &manager.session,
            if push {
                MaterializedExecutionState::Running { started_at_ms: 1 }
            } else {
                MaterializedExecutionState::Idle
            },
        );
        running
            .snapshot
            .as_mut()
            .unwrap()
            .operational
            .relay_protocol_version = Some(43);
        manager
            .publisher
            .publish(manager.session.clone(), running)
            .await
            .unwrap();
        host.observe(
            &manager.session,
            &view(&manager.session, MaterializedExecutionState::Idle),
        );
        let request = {
            let host = host.clone();
            let session = manager.session.clone();
            tokio::spawn(async move {
                if push {
                    host.begin_push(&session, None).await.map(|_| ())
                } else {
                    host.start(&session, true).await
                }
            })
        };
        let (_, action, reply) = manager
            .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
            .await;
        assert!(matches!(
            action,
            ReviewerAction::CaptureDelta {
                scope,
                ..
            } if scope == if push { mj_core::relay::CaptureScope::PushableCommits } else { mj_core::relay::CaptureScope::TurnDelta }
        ));
        reply
            .send(Ok(ReviewerOutcome::Delta {
                repositories: vec![mj_core::relay::RepoDelta {
                    root: "/workspace/app".into(),
                    base_commit: Some("aaaaaaa123".into()),
                    head_commit: Some("bbbbbbb456".into()),
                    base_kind: Some("merge base with main".into()),
                    baseline_tree: Some("base".into()),
                    current_tree: "newtree".into(),
                    patch: "diff --git a/a b/a\n@@\n+one\n".into(),
                    files: vec![mj_core::relay::FileLineChange {
                        path: "a".into(),
                        insertions: 1,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }))
            .unwrap();
        let refusal = request.await.unwrap().unwrap_err();
        assert!(
            refusal.0.contains("review child registration refused"),
            "{refusal}"
        );
        host.shutdown().await.unwrap();
    }
}

// Hard-won: #1448: push verdict delivery discarded registrations before closing their workers.
#[tokio::test]
async fn empty_push_delta_records_clean_tree_without_a_reviewer() {
    let session = session_id("emptypush000");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .resolve_hangs
        .store(true, std::sync::atomic::Ordering::Release);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let mut running = view(
        &session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(43);
    manager
        .publisher
        .publish(session.clone(), running)
        .await
        .unwrap();
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.begin_push(&session, None).await })
    };
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                baseline_tree: Some("newtree".into()),
                ..push_delta("/workspace/app", "newtree")
            }],
        }))
        .unwrap();
    assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            match manager.requests.recv().await.unwrap() {
                RemoteSessionRequest::Reviewer { action, reply, .. } => {
                    match &action {
                        ReviewerAction::RecordReviewed { trees } => assert_eq!(
                            trees[&PathBuf::from("/workspace/app")],
                            ("newtree".into(), "bbbbbbb456".into())
                        ),
                        ReviewerAction::AdvanceBaseline { .. } => {}
                        _ => panic!("unexpected reviewer action {action:?}"),
                    }
                    reply.send(answer_for(&action)).unwrap();
                }
                RemoteSessionRequest::Submit { command, reply, .. } => {
                    assert!(matches!(command, RelayCommand::RecordNotice { .. }));
                    reply.send(Ok(1)).unwrap();
                }
                _ => panic!("unexpected request"),
            }
        }
    })
    .await
    .unwrap();
    assert!(environment.children.lock().unwrap().is_empty());
    let mailbox = environment.mailbox.lock().unwrap().clone();
    assert_eq!(mailbox.len(), 1);
    assert!(
        mj_core::mailbox::render_mailbox_event(&mailbox[0]).starts_with("Review clean for commits")
    );
    assert_eq!(
        environment.state().baselines[&PathBuf::from("/workspace/app")],
        "newtree"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_git_push_delta_records_warning_and_clean_tree_without_reviewer() {
    let session = session_id("oldgitpush00");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .resolve_hangs
        .store(true, std::sync::atomic::Ordering::Release);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let mut running = view(
        &session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(43);
    manager
        .publisher
        .publish(session.clone(), running)
        .await
        .unwrap();
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.begin_push(&session, None).await })
    };
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                baseline_tree: Some("newtree".into()),
                push_review_warning: Some("Pushed without review: Git 2.34.1 in /workspace/app is older than 2.39.5. Upgrade Git to review merged work.".into()),
                ..push_delta("/workspace/app", "newtree")
            }],
        }))
        .unwrap();
    assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
    let mut notices = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() || notices.len() < 2 {
            match manager.requests.recv().await.unwrap() {
                RemoteSessionRequest::Reviewer { action, reply, .. } => {
                    match &action {
                        ReviewerAction::RecordReviewed { trees } => assert_eq!(
                            trees[&PathBuf::from("/workspace/app")],
                            ("newtree".into(), "bbbbbbb456".into())
                        ),
                        ReviewerAction::AdvanceBaseline { .. } => {}
                        _ => panic!("unexpected reviewer action {action:?}"),
                    }
                    reply.send(answer_for(&action)).unwrap();
                }
                RemoteSessionRequest::Submit { command, reply, .. } => {
                    if let RelayCommand::RecordNotice { text } = command {
                        notices.push(text);
                    } else {
                        panic!("unexpected command");
                    }
                    reply.send(Ok(1)).unwrap();
                }
                _ => panic!("unexpected request"),
            }
        }
    })
    .await
    .unwrap();
    assert!(environment.children.lock().unwrap().is_empty());
    let mailbox = environment.mailbox.lock().unwrap().clone();
    assert_eq!(mailbox.len(), 1);
    assert!(
        mj_core::mailbox::render_mailbox_event(&mailbox[0]).starts_with("Review clean for commits")
    );
    assert_eq!(
        environment.state().baselines[&PathBuf::from("/workspace/app")],
        "newtree"
    );
    assert!(notices.iter().any(|notice| notice.contains("Git 2.34.1")));
    host.shutdown().await.unwrap();
}

// Hard-won: c5e89cf12: explicit review requests were refused unless automatic review ran on push.
#[tokio::test]
async fn push_review_delivers_each_verdict_without_holding_or_prompting_the_primary() {
    for (name, report, expected) in [
        (
            "pushclean",
            Some("No findings."),
            "Review clean for commits",
        ),
        (
            "pushfindings",
            Some("A correctness defect needs a fix"),
            "Review of commits",
        ),
        ("pushfailed", None, "Review of commits"),
        (
            "manualoff",
            Some("No findings."),
            "Review clean for commits",
        ),
        (
            "manualturn",
            Some("No findings."),
            "Review clean for commits",
        ),
        (
            "manualchild",
            Some("No findings."),
            "Review clean for commits",
        ),
    ] {
        let session = session_id(name);
        let clean = report == Some("No findings.");
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        environment
            .subagent
            .store(name == "manualchild", std::sync::atomic::Ordering::Release);
        if name == "pushclean" {
            pending_push_findings(&environment);
            environment
                .state
                .lock()
                .unwrap()
                .baselines
                .insert(PathBuf::from("/workspace/lib"), "libtree".into());
        }
        let trigger = match name {
            "manualoff" | "manualchild" => mj_core::config::ReviewTrigger::Off,
            "manualturn" => mj_core::config::ReviewTrigger::Turn,
            _ => mj_core::config::ReviewTrigger::Push,
        };
        let config: ReviewConfigSource = Arc::new(move |_| ReviewConfig {
            trigger,
            reviewers: vec![mj_core::config::SubagentChoice {
                model: "test-reviewer".into(),
                effort: None,
            }],
        });
        let host = TurnReviewHost::spawn_in(manager.control.clone(), config, environment.clone());
        let child = open_push(&mut manager, &host, &environment).await;
        assert_eq!(
            host.begin_push(&session, None).await.unwrap_err().0,
            "a review is already open"
        );
        let finished = {
            let host = host.clone();
            let session = session.clone();
            let child = child.clone();
            let report = report.map(str::to_owned);
            tokio::spawn(async move { host.reviewer_finished(session, child, report).await })
        };
        let mut recorded = false;
        let mut advanced = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while host.view(&session).is_some() {
                            tokio::select! {
                    _ = tokio::task::yield_now() => {},
                    request = manager.requests.recv() => match request.unwrap() {
                        RemoteSessionRequest::Reviewer { action, reply, .. } => {
                            match &action {
                                ReviewerAction::RecordReviewed { trees } => { assert!(clean); assert_eq!(trees[&PathBuf::from("/workspace/app")], ("newtree".into(), "bbbbbbb456".into())); recorded = true; },
                                ReviewerAction::AdvanceBaseline { .. } => { assert!(recorded); advanced = true; },
                                _ => panic!("unexpected review operation {action:?}"),
                            }
                            reply.send(answer_for(&action)).unwrap();
                        },
                        RemoteSessionRequest::Submit { command, reply, .. } => {
                            assert!(matches!(command, RelayCommand::RecordNotice { .. }), "findings must arrive through mailbox");
                            reply.send(Ok(1)).unwrap();
                        },
                        _ => panic!("unexpected primary operation"),
                    }
                }
            }
        }).await.unwrap();
        finished.await.unwrap().unwrap();
        assert_eq!(recorded, clean);
        assert_eq!(advanced, recorded);
        let mailbox = environment.mailbox.lock().unwrap().clone();
        assert_eq!(mailbox.len(), 1);
        assert!(mailbox[0].wake);
        assert!(mj_core::mailbox::render_mailbox_event(&mailbox[0]).starts_with(expected));
        let state = environment.state();
        assert_eq!(
            state
                .baselines
                .contains_key(&PathBuf::from("/workspace/app")),
            recorded
        );
        if name == "pushclean" {
            // The review covered one repository; the other keeps its baseline.
            assert_eq!(state.baselines[&PathBuf::from("/workspace/lib")], "libtree");
        }
        assert_eq!(state.prior_review.is_some(), name == "pushfindings");
        assert_eq!(state.push_findings.is_some(), name == "pushfindings");
        if let Some(findings) = &state.push_findings {
            assert_eq!(
                findings.synthesis,
                format!("Reviewer 1 (session {child})\nA correctness defect needs a fix")
            );
            assert_eq!(
                findings.trees[&PathBuf::from("/workspace/app")],
                ("newtree".into(), "bbbbbbb456".into())
            );
        }
        assert!(state.active.is_none());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !environment.closed.lock().unwrap().contains(&child) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("push completion transfers its child to cleanup before clearing the group");
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn incomplete_push_findings_clear_stale_override_without_advancing_coverage() {
    let session = session_id("push-partial");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let original_prior = environment.state().prior_review;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        push_config_count(2),
        environment.clone(),
    );
    open_push(&mut manager, &host, &environment).await;
    let children = wait_for_group_children(&environment, &host, &session, 2).await;
    finish_child(
        &host,
        &environment,
        &session,
        &children[0],
        Some("real defect"),
    )
    .await;
    finish_child(&host, &environment, &session, &children[1], None).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let state = environment.state();
    assert!(state.push_findings.is_none());
    assert_eq!(state.prior_review, original_prior);
    assert!(state.baselines.is_empty());
    {
        let mailbox = environment.mailbox.lock().unwrap();
        assert_eq!(mailbox.len(), 1);
        let mj_core::mailbox::MailboxEventBody::ReviewVerdict { verdict, .. } = &mailbox[0].body
        else {
            panic!("push verdict is in the mailbox");
        };
        assert!(matches!(verdict, ReviewVerdict::Findings { .. }));
        assert!(!verdict.is_complete());
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_delivery_retries_same_mailbox_key_after_final_state_save_fails() {
    let session = session_id("push-final-save-retry");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    let review_id = environment
        .state()
        .active_group
        .as_ref()
        .unwrap()
        .driver
        .review_id
        .clone();
    *environment.final_save_error.lock().unwrap() = Some("database busy".into());
    finish_child(
        &host,
        &environment,
        &session,
        &child,
        Some("fix the parser"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    {
        let mailbox = environment.mailbox.lock().unwrap();
        assert_eq!(
            mailbox.len(),
            1,
            "replay keeps the original mailbox idempotency key"
        );
        assert!(mailbox[0].key.contains(&review_id));
    }
    assert!(environment.state().active_group.is_none());
    host.shutdown().await.unwrap();
}

fn pending_push_findings(environment: &FakeEnvironment) {
    *environment.state.lock().unwrap() = TurnReviewState {
        pending_followup: false,
        forward_attempts: 0,
        prior_review: Some(mj_core::review::lanes::PriorReviewContext {
            synthesis: "fix the regression".into(),
            evidence: Default::default(),
        }),
        push_findings: Some(mj_core::storage::PushFindingsRecord {
            review_id: "review-1".into(),
            trees: [(
                PathBuf::from("/workspace/app"),
                ("newtree".into(), "bbbbbbb456".into()),
            )]
            .into(),
            synthesis: "fix the regression".into(),
        }),
        ..Default::default()
    };
}

fn push_delta(root: &str, tree: &str) -> mj_core::relay::RepoDelta {
    mj_core::relay::RepoDelta {
        root: root.into(),
        base_commit: Some("aaaaaaa123".into()),
        head_commit: Some("bbbbbbb456".into()),
        current_tree: tree.into(),
        ..Default::default()
    }
}

async fn answer_override_capture(
    manager: &mut FakeManager,
    repositories: Vec<mj_core::relay::RepoDelta>,
) {
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(matches!(
        action,
        ReviewerAction::CaptureDelta {
            repositories: None,
            scope: mj_core::relay::CaptureScope::PushableCommits,
            ..
        }
    ));
    reply
        .send(Ok(ReviewerOutcome::Delta { repositories }))
        .unwrap();
}

async fn finish_override_worker_actions(manager: &mut FakeManager, expected_notice: &str) {
    match manager.next().await {
        RemoteSessionRequest::Submit {
            command: RelayCommand::RecordNotice { text },
            reply,
            ..
        } => {
            assert!(text.contains(expected_notice), "{text}");
            assert!(text.contains("fix the regression"));
            assert!(text.len() <= mj_review::SYNTHESIS_LIMIT);
            reply.send(Ok(1)).unwrap();
        }
        other => panic!(
            "expected the override audit before any ref change, got {}",
            other.session_id()
        ),
    }
    let (action, reply) = match manager.next().await {
        RemoteSessionRequest::Reviewer { action, reply, .. } => (action, reply),
        other => panic!(
            "expected reviewed-ref recording, got {}",
            other.session_id()
        ),
    };
    assert!(
        matches!(action, ReviewerAction::RecordReviewed { ref trees } if trees[&PathBuf::from("/workspace/app")] == ("newtree".into(), "bbbbbbb456".into()))
    );
    reply.send(answer_for(&action)).unwrap();
    let (action, reply) = match manager.next().await {
        RemoteSessionRequest::Reviewer { action, reply, .. } => (action, reply),
        other => panic!("expected baseline pinning, got {}", other.session_id()),
    };
    assert!(matches!(action, ReviewerAction::AdvanceBaseline { .. }));
    reply.send(answer_for(&action)).unwrap();
}

#[tokio::test]
async fn push_override_rejects_missing_or_changed_findings_without_unlocking_a_tree() {
    for (name, pending, deltas, expected) in [
        ("missing", false, vec![], "no findings verdict"),
        ("empty", true, vec![], "HEAD changed"),
        (
            "moved",
            true,
            vec![push_delta("/workspace/app", "different")],
            "HEAD changed",
        ),
        (
            "extra",
            true,
            vec![
                push_delta("/workspace/app", "newtree"),
                push_delta("/workspace/other", "tree"),
            ],
            "HEAD changed",
        ),
        (
            "duplicate",
            true,
            vec![
                push_delta("/workspace/app", "newtree"),
                push_delta("/workspace/app", "newtree"),
            ],
            "HEAD changed",
        ),
    ] {
        let session = session_id(&format!("override-{name}"));
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        if pending {
            pending_push_findings(&environment);
        }
        let host =
            TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
        let request = {
            let host = host.clone();
            let session = session.clone();
            tokio::spawn(async move { host.override_push(&session, "disagree".into()).await })
        };
        if pending {
            answer_override_capture(&mut manager, deltas).await;
        }
        assert!(request.await.unwrap().unwrap_err().0.contains(expected));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
                .await
                .is_err(),
            "no audit or ref write follows refusal"
        );
        assert_eq!(environment.state().push_findings.is_some(), pending);
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn push_override_audit_refusal_keeps_findings_and_never_records_a_ref() {
    let session = session_id("override-audit-refusal");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.override_push(&session, "🔥".repeat(4000)).await })
    };
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    match manager.next().await {
        RemoteSessionRequest::Submit {
            command: RelayCommand::RecordNotice { text },
            reply,
            ..
        } => {
            assert!(text.contains("🔥") && text.contains("[override reason truncated]"));
            assert!(text.contains("fix the regression") && text.contains("review-1"));
            assert!(text.len() <= mj_review::SYNTHESIS_LIMIT);
            reply.send(Err("journal unavailable".into())).unwrap();
        }
        other => panic!("expected audit submit, got {}", other.session_id()),
    }
    assert!(
        request
            .await
            .unwrap()
            .unwrap_err()
            .0
            .contains("journal unavailable")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "no reviewed ref is recorded without audit"
    );
    assert!(environment.state().push_findings.is_some());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_override_keeps_reservation_during_save_and_drains_admitted_work_on_shutdown() {
    let session = session_id("override-shutdown");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.override_push(&session, "incorrect".into()).await })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(
        host.override_push(&session, "again".into())
            .await
            .unwrap_err()
            .0
            .contains("review is open")
    );
    assert!(
        host.begin_push(&session, None)
            .await
            .unwrap_err()
            .0
            .contains("review is already open")
    );
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "newtree")],
        }))
        .unwrap();
    let gate = environment.block_saves();
    finish_override_worker_actions(&mut manager, "override requested").await;
    gate.entered().await;
    assert!(!request.is_finished());
    assert!(
        host.override_push(&session, "again".into())
            .await
            .unwrap_err()
            .0
            .contains("review is open")
    );
    let shutdown = {
        let host = host.clone();
        tokio::spawn(async move { host.shutdown().await })
    };
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "admitted override finishes before shutdown drains persistence"
    );
    gate.release();
    assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
    shutdown.await.unwrap().unwrap();
    assert!(environment.state().push_findings.is_none());
}

#[tokio::test]
async fn push_override_retries_after_reviewed_ref_was_recorded_but_state_save_failed() {
    let session = session_id("override-save-retry");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    environment
        .state
        .lock()
        .unwrap()
        .baselines
        .insert(PathBuf::from("/workspace/lib"), "libtree".into());
    *environment.save_error.lock().unwrap() = Some("database busy".into());
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    for attempt in 0..2 {
        let request = {
            let host = host.clone();
            let session = session.clone();
            tokio::spawn(async move { host.override_push(&session, "false positive".into()).await })
        };
        answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
        finish_override_worker_actions(&mut manager, "false positive").await;
        let result = request.await.unwrap();
        if attempt == 0 {
            assert!(result.unwrap_err().0.contains("database busy"));
            assert!(
                environment.state().push_findings.is_some(),
                "a retry must still know the exact reviewed trees"
            );
        } else {
            assert_eq!(result.unwrap()["accepted"], true);
            assert!(environment.state().push_findings.is_none());
            assert!(environment.state().prior_review.is_none());
            assert_eq!(
                environment.state().baselines[&PathBuf::from("/workspace/app")],
                "newtree"
            );
            // The override covered one repository; the other keeps its baseline.
            assert_eq!(
                environment.state().baselines[&PathBuf::from("/workspace/lib")],
                "libtree"
            );
        }
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_override_load_waits_for_cancelled_review_open_clear_save() {
    let session = session_id("override-load-barrier");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let mut running = view(
        &session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(43);
    manager
        .publisher
        .publish(session.clone(), running)
        .await
        .unwrap();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let review = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.begin_push(&session, None).await })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let open_gate = environment.block_saves();
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "newtree")],
        }))
        .unwrap();
    open_gate.entered().await;
    host.resolve(&session, Resolution::Cancelled).await.unwrap();
    let clear_gate = environment.block_saves();
    open_gate.release();
    assert!(review.await.unwrap().unwrap_err().0.contains("cancelled"));
    let override_request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.override_push(&session, "incorrect".into()).await })
    };
    clear_gate.entered().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "FIFO load cannot capture while the earlier clearing save is blocked"
    );
    clear_gate.release();
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    finish_override_worker_actions(&mut manager, "incorrect").await;
    assert_eq!(override_request.await.unwrap().unwrap()["accepted"], true);
    assert!(environment.state().push_findings.is_none());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_cancels_unadmitted_push_override_without_marking_a_review_interrupted() {
    let session = session_id("override-cancel-shutdown");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.override_push(&session, "incorrect".into()).await })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    host.shutdown().await.unwrap();
    drop(capture);
    assert!(request.await.unwrap().is_err());
    assert!(
        environment.state().active.is_none(),
        "override preparation is not a review to recover"
    );
    assert!(environment.state().push_findings.is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "shutdown did not submit an audit or ref"
    );
}

#[tokio::test]
async fn explicit_commit_review_refuses_unsupported_workers() {
    let session = session_id("pushrefusals");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment);
    let mut published = view(&session, MaterializedExecutionState::Idle);
    published
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(42);
    manager
        .publisher
        .publish(session.clone(), published)
        .await
        .unwrap();
    assert_eq!(
        host.begin_push(&session, None).await.unwrap_err().0,
        "the session's worker predates push review; restart the session"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_push_review_restart_keeps_its_busy_child_and_delivers_its_verdict() {
    let session = session_id("pushrestart");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    host.shutdown().await.unwrap();
    let restarted =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while restarted.view(&session).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!environment.closed.lock().unwrap().contains(&child));
    finish_child(
        &restarted,
        &environment,
        &session,
        &child,
        Some("fix the parser"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while restarted.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(environment.mailbox.lock().unwrap().len(), 1);
    assert!(environment.state().baselines.is_empty());
    assert!(environment.state().active_group.is_none());
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_review_refusals_leave_no_prompt_hold_or_reviewer_child() {
    let session = session_id("pushpreparation");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let moving = crate::controller::move_session::MoveMutationGuard::reserve(&session).unwrap();
    assert_eq!(
        host.begin_push(&session, None).await.unwrap_err().0,
        "session is moving"
    );
    drop(moving);
    let mut published = view(
        &session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    published
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(43);
    manager
        .publisher
        .publish(session.clone(), published)
        .await
        .unwrap();
    for reason in [
        "commit or stash these tracked changes first:\n  dirty.rs",
        "nothing to review because base == HEAD",
    ] {
        let request = {
            let host = host.clone();
            let session = session.clone();
            tokio::spawn(async move { host.begin_push(&session, None).await })
        };
        let (_, _, reply) = manager
            .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
            .await;
        reply.send(Err(reason.into())).unwrap();
        assert_eq!(request.await.unwrap().unwrap_err().0, reason);
    }
    assert!(environment.children.lock().unwrap().is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_push_review_closes_the_child_and_leaves_the_gate_blocked() {
    let session = session_id("pushcancel");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    assert!(host.resolve(&session, Resolution::Dismissed).await.is_err());
    assert!(host.resolve(&session, Resolution::Forwarded).await.is_err());
    host.resolve(&session, Resolution::Cancelled).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() || !environment.closed.lock().unwrap().contains(&child)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(environment.mailbox.lock().unwrap().is_empty());
    assert!(environment.state().baselines.is_empty());
    host.shutdown().await.unwrap();
}

// Hard-won: 32d96d199: repeated reviews reused driver IDs and lost later verdicts to mailbox deduplication.
#[tokio::test]
async fn repeated_push_reviews_keep_distinct_mailbox_keys_across_host_restart() {
    let session = session_id("pushmailboxids");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let mut host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    for round in 0..3 {
        let child = open_push(&mut manager, &host, &environment).await;
        finish_child(
            &host,
            &environment,
            &session,
            &child,
            Some("The parser still has a correctness defect."),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while host.view(&session).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if round == 1 {
            host.shutdown().await.unwrap();
            host = TurnReviewHost::spawn_in(
                manager.control.clone(),
                push_config(),
                environment.clone(),
            );
        }
    }
    {
        let mailbox = environment.mailbox.lock().unwrap();
        assert_eq!(mailbox.len(), 3);
        assert_eq!(
            mailbox
                .iter()
                .map(|event| event.key.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
    }
    host.shutdown().await.unwrap();
}
