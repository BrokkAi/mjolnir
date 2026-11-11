use super::*;
use crate::session_manager::{
    RelaySessionTarget, RemoteSessionRequest, RemoteSessionRequests, spawn_remote_session_manager,
};
use mj_core::state::{ManagedSessionSnapshot, MaterializedExecutionState, MaterializedSession};
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
        "Review did not start: another operation was using the session. \
         Commit tracked changes and request review again."
    );
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
        ReviewerAction::RecordReviewed { .. } | ReviewerAction::RecordOverridden { .. } => {
            Ok(ReviewerOutcome::BaselineAdvanced)
        }
        ReviewerAction::SetReviewBase { path, commit } => Ok(ReviewerOutcome::ReviewBaseSet {
            changed: true,
            root: path.clone().unwrap_or_else(|| "/workspace/app".into()),
            commit: commit.clone(),
            tree: "settree".into(),
        }),
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
    // Sessions run the current worker unless a test publishes an older one.
    operational.relay_protocol_version = Some(mj_core::relay::RELAY_PROTOCOL_VERSION);
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
            subagent_requests: Default::default(),
            subagent_completions: Default::default(),
            subagent_queue_error: None,
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational: operational.into(),
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
    /// While set, every close fails, and each attempt is counted.
    close_fails: std::sync::atomic::AtomicBool,
    close_attempts: std::sync::atomic::AtomicUsize,
    start_error: Mutex<Option<String>>,
    /// What the store says about each reviewer child; absent means live with
    /// no handback.
    standings: Mutex<std::collections::BTreeMap<String, ChildStanding>>,
    /// A child's next standing read fails with this error, once.
    standing_errors: Mutex<std::collections::BTreeMap<String, String>>,
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
    /// Saves that fail with "database busy" after `save_error` has been spent.
    extra_save_failures: std::sync::atomic::AtomicUsize,
    final_save_error: Mutex<Option<String>>,
    /// The requesting session's record is gone: the store refuses the final
    /// save of its review, as the foreign key on its review state does.
    owner_deleted: std::sync::atomic::AtomicBool,
    deleted_owner_saves: std::sync::atomic::AtomicUsize,
    subagent: std::sync::atomic::AtomicBool,
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
            close_fails: std::sync::atomic::AtomicBool::new(false),
            close_attempts: std::sync::atomic::AtomicUsize::new(0),
            start_error: Mutex::new(None),
            standings: Mutex::new(std::collections::BTreeMap::new()),
            standing_errors: Mutex::new(std::collections::BTreeMap::new()),
            register_error: Mutex::new(None),
            register_gate: Mutex::new(None),
            registered_gate: Mutex::new(None),
            state: Mutex::new(TurnReviewState::default()),
            writes: Mutex::new(Vec::new()),
            owner: Mutex::new(None),
            save_gate: Mutex::new(None),
            sweep_gate: Mutex::new(None),
            save_error: Mutex::new(None),
            extra_save_failures: std::sync::atomic::AtomicUsize::new(0),
            final_save_error: Mutex::new(None),
            owner_deleted: std::sync::atomic::AtomicBool::new(false),
            deleted_owner_saves: std::sync::atomic::AtomicUsize::new(0),
            subagent: std::sync::atomic::AtomicBool::new(false),
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

    fn session_exists(&self, _session_id: &str) -> Result<bool, String> {
        Ok(!self.owner_deleted.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn reviewer_child_standing(&self, child: &str) -> Result<ChildStanding, String> {
        if let Some(error) = self.standing_errors.lock().unwrap().remove(child) {
            return Err(error);
        }
        Ok(self
            .standings
            .lock()
            .unwrap()
            .get(child)
            .cloned()
            .unwrap_or_default())
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
            self.close_attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.close_fails.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("worker restart queue is full".into());
            }
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
        if self
            .extra_save_failures
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
        {
            return Err("database busy".into());
        }
        if state.active_group.is_none()
            && state.active.is_none()
            && let Some(error) = self.final_save_error.lock().unwrap().take()
        {
            return Err(error);
        }
        // The real store's foreign key refuses every save for a deleted session.
        if self.owner_deleted.load(std::sync::atomic::Ordering::SeqCst) {
            self.deleted_owner_saves
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Err("FOREIGN KEY constraint failed".into());
        }
        let mut stored = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = state.clone();
        if let Some(group) = next.active_group.as_ref() {
            for event in partial_review_events(session_id, stored.active_group.as_ref(), group) {
                self.enqueue_verdict(session_id, &event)?;
            }
        }
        *stored = next.clone();
        drop(stored);
        self.writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((next, std::thread::current().id()));
        Ok(())
    }

    fn clear_interrupted(&self) -> Result<Vec<String>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interrupted = state.active_group.is_none() && (state.active.is_some());
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
            base_commit: Some("basecommit".into()),
            head_commit: Some("headcommit".into()),
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
            base_commit: Some("basecommit".into()),
            head_commit: Some("headcommit".into()),
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
        legacy: None,
        disabled: false,
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
        .start(session)
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

/// Drives a session from running to idle and starts a review of the turn by
/// hand. The end of a turn no longer arms a review, so the manual start is the
/// remaining way into the turn pipeline these tests exercise. The daemon
/// observes each view as it publishes it, so the fake does both too.
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
    let host = host.clone();
    let session = manager.session.clone();
    tokio::spawn(async move {
        let _ = host.start(&session).await;
    });
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
            base_commit: Some("basecommit".into()),
            head_commit: Some("headcommit".into()),
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
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
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
    assert_eq!(notice, "Review complete: no material findings");
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review releases the session's prompts");
    host.shutdown().await.unwrap();
}

// A new primary turn is reflected by the live actor recheck, so preparation
// refuses instead of reviewing a stale idle snapshot.

// Session observations are an edge stream. A burst larger than the old
// bounded hand-off must retain its final idle edge, or manual admission
// sees a stale running session and automatic review can be lost too.

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
            base_commit: Some("basecommit".into()),
            head_commit: Some("headcommit".into()),
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

    let refusal = tokio::time::timeout(Duration::from_secs(1), host.start(session))
        .await
        .expect("the host loop remains responsive while persistence blocks")
        .expect_err("the same review is already starting");
    assert!(refusal.0.contains("already open"), "{refusal}");
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
            base_commit: Some("basecommit".into()),
            head_commit: Some("headcommit".into()),
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

    // Restart before the first verdict is delivered.
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
    finish_child(&new, &environment, session, &child, Some("No findings.")).await;
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    reply.send(answer_for(&action)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while new.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review closes");
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
        "Review was cancelled when Mjolnir restarted; request review again."
    );
    new.shutdown().await.unwrap();
}

// Queued primary prompts do not block independent review preparation.

/// One owner retries a reviewer child's close, and a close that keeps failing
/// backs off to minutes instead of resubmitting a worker restart every few seconds.
// Hard-won: #1456: failed reviewer-child closes were resubmitted every 30 s forever, refilling the worker restart queue.
#[tokio::test(start_paused = true)]
async fn a_failing_reviewer_close_backs_off_and_stops_once_it_succeeds() {
    use std::sync::atomic::Ordering::SeqCst;
    let session = session_id("closebackoff");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment.close_fails.store(true, SeqCst);
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    // The second hand-off arrives while the first close is pending: one owner.
    host.close_child("child-1".into());
    host.close_child("child-1".into());

    tokio::time::sleep(Duration::from_secs(3600)).await;
    // Attempts at 0 s, then after 1, 2, 4 ... 256 s, then every 300 s: about
    // 20 in an hour. A 30 s cap gives about 125 and a duplicate owner doubles them.
    let attempts = environment.close_attempts.load(SeqCst);
    assert!(
        (15..=25).contains(&attempts),
        "{attempts} close attempts in an hour"
    );

    environment.close_fails.store(false, SeqCst);
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(*environment.closed.lock().unwrap(), vec!["child-1"]);
    let settled = environment.close_attempts.load(SeqCst);
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(environment.close_attempts.load(SeqCst), settled);

    host.shutdown().await.expect("shutdown the host");
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
            base_commit: Some("basecommit".into()),
            head_commit: Some("headcommit".into()),
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
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    reply.send(answer_for(&action)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let state = environment.state();
    assert!(state.active.is_none());
    assert!(environment.closed.lock().unwrap().contains(&child));
    assert_eq!(environment.live_holds(), 0);
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
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    reply.send(answer_for(&action)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(&session).is_some() {
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
        legacy: None,
        disabled: false,
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
    let review_id = environment.state().active_group.unwrap().driver.review_id;
    {
        let mailbox = environment.mailbox.lock().unwrap();
        assert_eq!(mailbox.len(), 1, "findings arrive before the slow peer");
        let partial = mj_core::mailbox::render_mailbox_event(&mailbox[0]);
        assert!(partial.starts_with("Review 1/2 — partial result"));
        assert!(partial.contains("second defect") && !partial.contains("first defect"));
    }
    assert!(
        host.view(&session).unwrap().verdict.is_none(),
        "one unfinished peer cannot complete the group"
    );
    host.shutdown().await.unwrap();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), armed_count(2), environment.clone());
    wait_for_group_children(&environment, &host, &session, 2).await;
    assert_eq!(environment.mailbox.lock().unwrap().len(), 1);
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
            let prompt = environment
                .mailbox
                .lock()
                .unwrap()
                .get(1)
                .map(mj_core::mailbox::render_mailbox_event);
            if let Some(prompt) = prompt {
                return prompt;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(prompt.find("first defect").unwrap() < prompt.find("second defect").unwrap());
    assert!(prompt.contains(&children[0]) && prompt.contains(&children[1]));
    assert!(prompt.contains("may overlap"));
    assert!(
        prompt
            .contains("Review finished; this is the final result. No reviewers are still running.")
    );
    let transcript = mj_core::mailbox::render_mailbox_events(&environment.mailbox.lock().unwrap())
        .replace(&review_id, "review-id")
        .replace(&children[0], "reviewer-one")
        .replace(&children[1], "reviewer-two");
    assert!(!transcript.contains("<untrusted-mailbox-events>"));
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "asynchronous-review-results",
        &transcript,
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn clean_reviewers_are_silent_until_the_final_result() {
    for (name, first, last) in [
        ("clean-last", Some("a real defect"), "No findings."),
        ("clean-first", Some("No findings."), "a real defect"),
        ("clean-both", Some("No findings."), "No findings."),
        ("null-first", None, "a real defect"),
    ] {
        let session = session_id(name);
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        let host =
            TurnReviewHost::spawn_in(manager.control.clone(), armed_count(2), environment.clone());
        open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
        let children = wait_for_group_children(&environment, &host, &session, 2).await;
        finish_child(&host, &environment, &session, &children[0], first).await;
        let partial_count = usize::from(first == Some("a real defect"));
        assert_eq!(environment.mailbox.lock().unwrap().len(), partial_count);
        finish_child(&host, &environment, &session, &children[1], Some(last)).await;
        if name == "clean-both" {
            let (_, action, reply) = manager
                .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
                .await;
            reply.send(answer_for(&action)).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while host.view(&session).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mailbox = environment.mailbox.lock().unwrap().clone();
        assert_eq!(mailbox.len(), partial_count + 1);
        let final_result = mj_core::mailbox::render_mailbox_event(mailbox.last().unwrap());
        assert!(final_result.contains(
            "Review finished; this is the final result. No reviewers are still running."
        ));
        assert_eq!(
            environment.state().push_findings.is_some(),
            matches!(name, "clean-first" | "clean-last")
        );
        host.shutdown().await.unwrap();
    }
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
                assert!(
                    environment
                        .state()
                        .active_group
                        .as_ref()
                        .is_some_and(|group| group.registrations.iter().any(|item| {
                            item.child_session_id.as_deref() == Some(child.as_str())
                        })),
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
                base_commit: Some("basecommit".into()),
                head_commit: Some("headcommit".into()),
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
    let refusal = host.start(session).await.expect_err("one review at a time");
    assert_eq!(
        refusal.0,
        "a review is already open; wait for its verdict or cancel it before starting another"
    );
    host.shutdown().await.expect("shutdown the host");
}

/// The delegation loop asks the host, not a copy of its views, whether a
/// child's review is open, and holds the child's completion while it is.
#[tokio::test]
async fn the_host_reports_a_session_with_a_running_review_as_open_until_it_closes() {
    let session = session_id("openquery");
    let session = session.as_str();
    let mut manager = FakeManager::new(session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    // Before startup recovery finishes the host cannot know, and says so.
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match host.has_open_review(session).await {
                Err(crate::review_host::OpenReviewUnknown::NotReady) => {
                    tokio::task::yield_now().await
                }
                answer => break answer,
            }
        }
    })
    .await
    .expect("the host finishes startup");
    assert!(!first.unwrap());
    open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    assert!(host.has_open_review(session).await.unwrap());
    assert!(!host.has_open_review("another-session").await.unwrap());
    host.resolve(session, Resolution::Cancelled).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.has_open_review(session).await.unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a cancelled review closes");
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
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    reply.send(answer_for(&action)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while new.view(&session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
                base_commit: Some("basecommit".into()),
                head_commit: Some("headcommit".into()),
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
        declined: Vec::new(),
    };
    let (mut driver, _) = TurnReviewDriver::start(
        seed,
        "group-1".into(),
        std::num::NonZeroUsize::new(1).unwrap(),
    );
    driver.delta_captured(vec![push_delta("/workspace/app", "newtree")]);
    let state = TurnReviewState {
        active: Some("group-1".into()),
        active_group: Some(mj_core::storage::ActiveReviewGroup {
            driver: driver.snapshot(),
            reviewers: vec![ReviewerIdentity::default()],
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
        tokio::spawn(async move {
            host.override_push(&session, one_override("old finding"))
                .await
        })
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
            declined: Vec::new(),
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
        active: Some("partly-failed-group".into()),
        active_group: Some(mj_core::storage::ActiveReviewGroup {
            driver: driver.snapshot(),
            reviewers: vec![ReviewerIdentity::default(); 2],
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

/// A group restored from the store with one busy child per role, as a new
/// daemon finds it. Returns once every child has been started.
async fn restore_busy_group(
    manager: &FakeManager,
    environment: &Arc<FakeEnvironment>,
    count: usize,
    not_started: &[&str],
) -> (TurnReviewHost, Vec<String>) {
    let (mut driver, _) = TurnReviewDriver::start(
        TurnReviewSeed {
            task: "inspect the changes".into(),
            user_messages: Vec::new(),
            declined: Vec::new(),
        },
        "restored-group".into(),
        std::num::NonZeroUsize::new(count).unwrap(),
    );
    driver.delta_captured(vec![push_delta("/workspace/app", "newtree")]);
    let children: Vec<String> = (1..=count).map(|n| format!("busy-child-{n}")).collect();
    *environment.owner.lock().unwrap() = Some(manager.session.clone());
    *environment.state.lock().unwrap() = TurnReviewState {
        active: Some("restored-group".into()),
        active_group: Some(mj_core::storage::ActiveReviewGroup {
            driver: driver.snapshot(),
            reviewers: vec![ReviewerIdentity::default(); count],
            registrations: children
                .iter()
                .enumerate()
                .map(|(index, child)| mj_core::storage::ReviewerRegistration {
                    role: format!("reviewer-{}", index + 1),
                    request_key: format!("key-{child}"),
                    child_session_id: Some(child.clone()),
                })
                .collect(),
        }),
        ..Default::default()
    };
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        push_config_count(count),
        environment.clone(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while children.iter().any(|child| {
            !not_started.contains(&child.as_str())
                && !environment.started.lock().unwrap().contains(child)
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the new daemon reattaches every restored reviewer");
    (host, children)
}

async fn wait_until_review_resolves(host: &TurnReviewHost, session: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.view(session).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the review resolves");
}

/// The one verdict a resolved review delivered: its findings and the
/// reviewers that never reported.
fn delivered_findings(
    environment: &FakeEnvironment,
) -> (bool, Vec<mj_core::review::verdict::ReviewerFailure>) {
    let mailbox = environment.mailbox.lock().unwrap();
    let mut verdicts = mailbox.iter().filter_map(|event| match &event.body {
        mj_core::mailbox::MailboxEventBody::ReviewVerdict { verdict, .. } => Some(verdict),
        _ => None,
    });
    let verdict = verdicts.next().expect("the review delivers a verdict");
    assert!(
        verdicts.next().is_none(),
        "exactly one final verdict is delivered"
    );
    let ReviewVerdict::Findings { evidence, .. } = verdict else {
        panic!("the surviving reviewer's findings are delivered: {verdict:?}");
    };
    (verdict.is_complete(), evidence.reviewer_failures.clone())
}

/// A handback a reviewer recorded for the turn its role awaits.
fn stored_handback(
    environment: &FakeEnvironment,
    role: &str,
    message: &str,
) -> Option<mj_core::subagent::SubagentHandback> {
    let command_id = environment
        .state()
        .active_group
        .expect("a restored group")
        .driver
        .roles
        .iter()
        .find(|item| item.role == role)
        .expect("the role exists")
        .command_id
        .clone();
    Some(mj_core::subagent::SubagentHandback {
        command_id,
        message: message.to_owned(),
        recorded_at_ms: 0,
    })
}

// Hard-won: f61b30a62: a restored reviewer whose session ended without a report left its review open forever
#[tokio::test]
async fn a_restored_reviewer_that_ends_without_an_outcome_leaves_an_incomplete_review() {
    let session = session_id("restored-ended");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;

    host.child_ended(
        children[0].clone(),
        "the reviewer's session ended (stopped)".into(),
    );
    finish_child(
        &host,
        &environment,
        &session,
        &children[1],
        Some("real defect"),
    )
    .await;
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].label.contains("Reviewer 1"), "{failures:?}");
    assert!(
        failures[0].reason.contains("session ended (stopped)"),
        "{failures:?}"
    );
    assert!(environment.state().active_group.is_none());
    host.shutdown().await.unwrap();
}

// Hard-won: f61b30a62: a restored reviewer whose session had already ended left its review open forever
#[tokio::test]
async fn a_restored_reviewer_whose_session_already_ended_fails_its_role() {
    let session = session_id("restored-start");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment.standings.lock().unwrap().insert(
        "busy-child-1".into(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: None,
        },
    );
    let (host, children) = restore_busy_group(&manager, &environment, 2, &["busy-child-1"]).await;

    finish_child(
        &host,
        &environment,
        &session,
        &children[1],
        Some("real defect"),
    )
    .await;
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0]
            .reason
            .contains("ended before the review was restored (stopped)"),
        "{failures:?}"
    );
    assert!(
        !environment
            .started
            .lock()
            .unwrap()
            .contains(&"busy-child-1".to_owned()),
        "an ended child is not started"
    );
    host.shutdown().await.unwrap();
}

// Hard-won: pre-push review of 4aaf9274c: a restored reviewer whose standing could not be read was started as live and never settled if it had ended
#[tokio::test]
async fn a_restored_reviewer_whose_standing_cannot_be_read_waits_and_reads_again() {
    let session = session_id("restored-unread");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .standing_errors
        .lock()
        .unwrap()
        .insert("busy-child-1".into(), "database is locked".into());
    environment.standings.lock().unwrap().insert(
        "busy-child-1".into(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: None,
        },
    );
    let (host, children) = restore_busy_group(&manager, &environment, 2, &["busy-child-1"]).await;

    // The unread child is neither started nor failed while the read fails.
    assert!(host.view(&session).is_some());
    assert!(
        !environment
            .started
            .lock()
            .unwrap()
            .contains(&"busy-child-1".to_owned())
    );
    finish_child(
        &host,
        &environment,
        &session,
        &children[1],
        Some("real defect"),
    )
    .await;
    // The read is repeated on the review's retry delay (1 s first), and
    // then the ended session settles the role.
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0]
            .reason
            .contains("ended before the review was restored (stopped)"),
        "{failures:?}"
    );
    assert!(
        !environment
            .started
            .lock()
            .unwrap()
            .contains(&"busy-child-1".to_owned()),
        "an ended child is not started"
    );
    host.shutdown().await.unwrap();
}

// Hard-won: pre-push review of d31ae04cd: an ended reviewer whose standing could not be read was failed, discarding a handback it had saved
#[tokio::test]
async fn an_ended_reviewer_whose_standing_cannot_be_read_is_read_again_before_it_settles() {
    let session = session_id("ended-unread");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    environment
        .standing_errors
        .lock()
        .unwrap()
        .insert(children[0].clone(), "database is locked".into());
    environment.standings.lock().unwrap().insert(
        children[0].clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: stored_handback(&environment, "reviewer-1", "real defect"),
        },
    );

    host.child_ended(
        children[0].clone(),
        "the reviewer's session ended (stopped)".into(),
    );
    finish_child(&host, &environment, &session, &children[1], Some("other")).await;
    // The first read fails, so the role waits; the read repeated on the
    // review's retry delay finds the handback.
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(complete, "{failures:?}");
    assert!(failures.is_empty(), "{failures:?}");
    host.shutdown().await.unwrap();
}

// Hard-won: pre-push review of 4aaf9274c: an ended reviewer's failure bypassed the serialized outcome path, so a failed save of another outcome dropped it
#[tokio::test]
async fn an_ended_reviewers_failure_is_applied_after_a_failed_outcome_save() {
    let session = session_id("ended-during-save");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;

    // Reviewer 1's outcome is saving, and the save will fail.
    let gate = SaveGate::new();
    *environment.save_gate.lock().unwrap() = Some(gate.clone());
    *environment.save_error.lock().unwrap() = Some("database busy".into());
    let first = tokio::spawn({
        let host = host.clone();
        let session = session.clone();
        let child = children[0].clone();
        async move {
            host.reviewer_finished(session, child, Some("real defect".into()))
                .await
        }
    });
    gate.entered().await;
    // Reviewer 2's session ends while that save is pending.
    host.child_ended(
        children[1].clone(),
        "the reviewer's session ended (error)".into(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.release();
    assert!(first.await.unwrap().unwrap_err().contains("database busy"));

    // The failed save restored reviewer 1's role to awaiting; reviewer 2's
    // failure waited behind it and is applied now.
    host.reviewer_finished(
        session.clone(),
        children[0].clone(),
        Some("real defect".into()),
    )
    .await
    .unwrap();
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].label.contains("Reviewer 2"), "{failures:?}");
    assert!(
        failures[0].reason.contains("session ended (error)"),
        "{failures:?}"
    );
    host.shutdown().await.unwrap();
}

// Hard-won: pre-push review of 2442c5f00: a reviewer that ended while its handback was queued was never settled when that handback's save failed after the delegation observing it was gone
#[tokio::test]
async fn a_reviewer_that_ended_behind_a_failing_save_is_settled_from_its_standing() {
    let session = session_id("ended-queued-save-fails");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    environment.standings.lock().unwrap().insert(
        children[1].clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: None,
        },
    );

    // Reviewer 1's outcome is saving, and both it and the next save fail.
    let gate = SaveGate::new();
    *environment.save_gate.lock().unwrap() = Some(gate.clone());
    *environment.save_error.lock().unwrap() = Some("database busy".into());
    environment
        .extra_save_failures
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let first = tokio::spawn({
        let host = host.clone();
        let session = session.clone();
        let child = children[0].clone();
        async move {
            host.reviewer_finished(session, child, Some("real defect".into()))
                .await
        }
    });
    gate.entered().await;
    // Reviewer 2's handback queues behind that save, and its session ends.
    // The delegation that carries the handback gives up on the failure.
    let second = tokio::spawn({
        let host = host.clone();
        let session = session.clone();
        let child = children[1].clone();
        async move {
            host.reviewer_finished(session, child, Some("late report".into()))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    host.child_ended(
        children[1].clone(),
        "the reviewer's session ended (stopped)".into(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.release();
    assert!(first.await.unwrap().unwrap_err().contains("database busy"));
    assert!(second.await.unwrap().unwrap_err().contains("database busy"));

    host.reviewer_finished(
        session.clone(),
        children[0].clone(),
        Some("real defect".into()),
    )
    .await
    .unwrap();
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].label.contains("Reviewer 2"), "{failures:?}");
    assert!(
        failures[0].reason.contains("session ended (stopped)"),
        "{failures:?}"
    );
    host.shutdown().await.unwrap();
}

// Hard-won: pre-push review of 2442c5f00: a stored handback of another command settled an ended reviewer's role as its answer
#[tokio::test]
async fn an_ended_reviewers_handback_for_another_command_is_not_its_answer() {
    let session = session_id("ended-other-command");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    let mut stale = stored_handback(&environment, "reviewer-1", "an earlier task's report");
    stale.as_mut().unwrap().command_id = "another-command".into();
    environment.standings.lock().unwrap().insert(
        children[0].clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: stale,
        },
    );

    host.child_ended(
        children[0].clone(),
        "the reviewer's session ended (stopped)".into(),
    );
    finish_child(&host, &environment, &session, &children[1], Some("other")).await;
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete, "{failures:?}");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].label.contains("Reviewer 1"), "{failures:?}");
    host.shutdown().await.unwrap();
}

/// After a restart a start error can be transient, so a live restored
/// reviewer that fails to start keeps its role.
#[tokio::test]
async fn a_live_restored_reviewer_that_fails_to_start_is_not_failed() {
    let session = session_id("restored-transient");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    *environment.start_error.lock().unwrap() = Some("relay not ready".to_owned());
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(host.view(&session).is_some(), "the review stays open");
    finish_child(
        &host,
        &environment,
        &session,
        &children[0],
        Some("real defect"),
    )
    .await;
    finish_child(
        &host,
        &environment,
        &session,
        &children[1],
        Some("No findings."),
    )
    .await;
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(complete && failures.is_empty(), "{failures:?}");
    host.shutdown().await.unwrap();
}

// Hard-won: f61b30a62: a review of a destroyed requester retried its undeliverable verdict forever
#[tokio::test]
async fn a_review_whose_requester_was_destroyed_is_dropped_without_retrying() {
    let session = session_id("restored-destroyed");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    environment
        .owner_deleted
        .store(true, std::sync::atomic::Ordering::SeqCst);

    for child in &children {
        host.child_ended(
            child.clone(),
            "the reviewer's session ended (removed)".into(),
        );
    }
    wait_until_review_resolves(&host, &session).await;

    // The first retry would come after one second.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let attempts = environment
        .deleted_owner_saves
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(attempts >= 1);
    assert!(host.view(&session).is_none());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        environment
            .deleted_owner_saves
            .load(std::sync::atomic::Ordering::SeqCst),
        attempts,
        "the undeliverable verdict is not attempted again"
    );
    host.shutdown().await.unwrap();
}

// Hard-won: pre-push review: a reviewer ending under a destroyed requester failed its outcome save and was re-reported forever
#[tokio::test]
async fn a_reviewer_ending_under_a_destroyed_requester_drops_the_review() {
    let session = session_id("restored-destroyed-reviewer");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    environment
        .owner_deleted
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // One reviewer ends; the other is still running, so the group has not
    // resolved and only the owner check can end the review.
    host.child_ended(
        children[0].clone(),
        "the reviewer's session ended (removed)".into(),
    );
    wait_until_review_resolves(&host, &session).await;

    let attempts = environment
        .deleted_owner_saves
        .load(std::sync::atomic::Ordering::SeqCst);
    // The end-report retry would come after one second.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        environment
            .deleted_owner_saves
            .load(std::sync::atomic::Ordering::SeqCst),
        attempts,
        "an ended reviewer's report is not retried for a destroyed requester"
    );
    host.shutdown().await.unwrap();
}

// Hard-won: f61b30a62: a reviewer that ended with an undelivered handback lost its verdict
#[tokio::test]
async fn a_reviewer_that_ends_with_an_undelivered_handback_resolves_with_it() {
    let session = session_id("restored-handback");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    environment.standings.lock().unwrap().insert(
        children[0].clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: stored_handback(&environment, "reviewer-1", "real defect"),
        },
    );

    host.child_ended(
        children[0].clone(),
        "the reviewer's session ended (stopped)".into(),
    );
    finish_child(
        &host,
        &environment,
        &session,
        &children[1],
        Some("No findings."),
    )
    .await;
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(complete, "{failures:?}");
    assert!(failures.is_empty(), "{failures:?}");
    host.shutdown().await.unwrap();
}

/// Ending is a fact about a role only until it has an outcome, and it counts
/// once however often it is reported.
#[tokio::test]
async fn a_reviewer_ending_after_its_outcome_or_twice_fails_nothing_more() {
    let session = session_id("restored-idempotent");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;

    host.reviewer_finished(
        session.clone(),
        children[0].clone(),
        Some("real defect".into()),
    )
    .await
    .unwrap();
    // Closing a finished child ends its session; its report stands.
    host.child_ended(
        children[0].clone(),
        "the reviewer's session ended (stopped)".into(),
    );
    assert!(host.view(&session).is_some(), "reviewer 2 is still awaited");
    for _ in 0..2 {
        host.child_ended(
            children[1].clone(),
            "the reviewer's session ended (error)".into(),
        );
    }
    wait_until_review_resolves(&host, &session).await;

    let (complete, failures) = delivered_findings(&environment);
    assert!(!complete);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].label.contains("Reviewer 2"), "{failures:?}");
    host.shutdown().await.unwrap();
}

// A review someone asked for delivers its findings: a findings verdict is
// forwarded to the primary agent as soon as it is reached, without waiting
// for a surface to choose Forward.

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
    assert!(environment.state().active.is_none());
    host.shutdown().await.unwrap();
}

/// Per-turn review was removed: a turn that answers a prompt, or that the
/// harness starts on its own, arms nothing.
#[tokio::test]
async fn the_end_of_a_turn_arms_no_review() {
    let session = session_id("noturnarm");
    let mut manager = FakeManager::new(&session).await;
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        push_config(),
        FakeEnvironment::new(),
    );
    for prompt_driven in [true, false] {
        for execution in [
            MaterializedExecutionState::Running { started_at_ms: 0 },
            MaterializedExecutionState::Idle,
        ] {
            host.observe(&session, &view_of(&session, execution, prompt_driven));
        }
    }
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
        legacy: None,
        disabled: false,
        reviewers: vec![mj_core::config::SubagentChoice {
            model: "test-reviewer".into(),
            effort: None,
        }],
    })
}

fn push_config_count(count: usize) -> ReviewConfigSource {
    Arc::new(move |_| ReviewConfig {
        legacy: None,
        disabled: false,
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
    open_commit_review(manager, host, environment, false).await
}

async fn open_commit_review(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
    manual: bool,
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
    running.snapshot.as_mut().unwrap().materialized.transcript =
        ["Keep the public API stable", "Fix the retry behavior"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                Arc::new(mj_core::state::TranscriptItem {
                    stable_id: format!("user:{index}"),
                    position: index as u64 + 1,
                    latest_content_event_ordinal: Some(index as u64 + 1),
                    created_at_ms: 0,
                    last_changed_at_ms: 0,
                    body: mj_core::state::TranscriptBody::User {
                        content: vec![serde_json::json!({"type": "text", "text": text})],
                    },
                })
            })
            .collect();
    manager
        .publisher
        .publish(manager.session.clone(), running)
        .await
        .unwrap();
    let request = {
        let host = host.clone();
        let session = manager.session.clone();
        tokio::spawn(async move {
            if manual {
                host.start(&session)
                    .await
                    .map(|_| serde_json::json!({"accepted":true}))
            } else {
                host.begin_push(&session, None).await
            }
        })
    };
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(matches!(action, ReviewerAction::CaptureDelta { .. }));
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
    if !manual {
        assert_eq!(answer["repositories"][0]["base_commit"], "aaaaaaa123");
        assert_eq!(answer["repositories"][0]["head_commit"], "bbbbbbb456");
        assert_eq!(
            answer["repositories"][0]["base_kind"],
            "merge base with main"
        );
        assert_eq!(answer["repositories"][0]["changed_file_count"], 1);
    }
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
                    host.start(&session).await
                }
            })
        };
        let (_, action, reply) = manager
            .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
            .await;
        assert!(matches!(
            action,
            ReviewerAction::CaptureDelta { repositories: None }
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
            environment.state.lock().unwrap().decline_findings(
                [declined("other repository", "still applies")],
                vec!["/workspace/other".into()],
            );
        }
        let disabled = matches!(name, "manualoff" | "manualchild");
        let config: ReviewConfigSource = Arc::new(move |_| ReviewConfig {
            legacy: None,
            disabled,
            reviewers: vec![mj_core::config::SubagentChoice {
                model: "test-reviewer".into(),
                effort: None,
            }],
        });
        let host = TurnReviewHost::spawn_in(manager.control.clone(), config, environment.clone());
        let child = open_commit_review(
            &mut manager,
            &host,
            &environment,
            name.starts_with("manual"),
        )
        .await;
        {
            let children = environment.children.lock().unwrap();
            let prompt = &children[0].2;
            assert!(!prompt.contains("corrective pass"));
            assert!(!prompt.contains("other repository"));
            assert!(prompt.contains(
                "<user_message index=\"1\">\nKeep the public API stable\n</user_message>"
            ));
            assert!(prompt.contains("<user_message index=\"2\" current_outer_turn=\"true\">\nFix the retry behavior\n</user_message>"));
            assert_eq!(
                prompt.contains("<declined_findings>\n- Finding: [P3] old finding\n  Reason: old reason\n</declined_findings>"),
                name == "pushclean",
                "the declined findings reach the reviewer"
            );
        }
        assert_eq!(
            host.begin_push(&session, None).await.unwrap_err().0,
            "a review is already open; wait for its verdict or cancel it before starting another"
        );
        let finished = {
            let host = host.clone();
            let session = session.clone();
            let child = child.clone();
            let report = report.map(str::to_owned);
            tokio::spawn(async move { host.reviewer_finished(session, child, report).await })
        };
        let mut recorded = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while host.view(&session).is_some() {
                            tokio::select! {
                    _ = tokio::task::yield_now() => {},
                    request = manager.requests.recv() => match request.unwrap() {
                        RemoteSessionRequest::Reviewer { action, reply, .. } => {
                            match &action {
                                ReviewerAction::RecordReviewed { trees } => { assert!(clean); assert_eq!(trees[&PathBuf::from("/workspace/app")], ("newtree".into(), "bbbbbbb456".into())); recorded = true; },
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
        let mailbox = environment.mailbox.lock().unwrap().clone();
        assert_eq!(mailbox.len(), 1);
        assert!(mailbox[0].wake);
        assert!(mj_core::mailbox::render_mailbox_event(&mailbox[0]).starts_with(expected));
        let state = environment.state();
        assert!(
            if name == "pushclean" {
                state.declined_findings.len() == 1
                    && state.declined_findings[0].finding == "other repository"
            } else {
                state.declined_findings.is_empty()
            },
            "a scoped clean verdict clears only its repositories"
        );
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
    let original_declined = environment.state().declined_findings;
    assert!(!original_declined.is_empty());
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
    assert_eq!(state.declined_findings, original_declined);
    {
        let mailbox = environment.mailbox.lock().unwrap();
        assert_eq!(mailbox.len(), 2);
        let mj_core::mailbox::MailboxEventBody::ReviewVerdict { verdict, .. } = &mailbox[1].body
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

fn declined(finding: &str, reason: &str) -> mj_core::subagent::ReviewOverride {
    mj_core::subagent::ReviewOverride {
        finding: finding.into(),
        reason: reason.into(),
    }
}

fn one_override(reason: &str) -> Vec<mj_core::subagent::ReviewOverride> {
    vec![declined(
        "[P1] src/lib.rs:7 -- retry loses the request",
        reason,
    )]
}

fn pending_push_findings(environment: &FakeEnvironment) {
    *environment.state.lock().unwrap() = TurnReviewState {
        declined_findings: vec![mj_core::storage::DeclinedFinding {
            entry: declined("[P3] old finding", "old reason"),
            repositories: vec!["/workspace/app".into()],
        }],
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
    assert!(matches!(action, ReviewerAction::CaptureDelta { .. }));
    reply
        .send(Ok(ReviewerOutcome::Delta { repositories }))
        .unwrap();
}

async fn finish_override_worker_actions(manager: &mut FakeManager, expected_notice: &str) {
    finish_override_worker_actions_for(manager, expected_notice, false).await
}

/// `old_worker` is a session whose worker predates `RecordOverridden`.
async fn finish_override_worker_actions_for(
    manager: &mut FakeManager,
    expected_notice: &str,
    old_worker: bool,
) {
    finish_override_worker_actions_with_failure(manager, expected_notice, old_worker, None).await;
}

async fn finish_override_worker_actions_with_failure(
    manager: &mut FakeManager,
    expected_notice: &str,
    old_worker: bool,
    failure: Option<(&FakeEnvironment, &str)>,
) {
    match manager.next().await {
        RemoteSessionRequest::Submit {
            command: RelayCommand::RecordNotice { text },
            reply,
            ..
        } => {
            assert!(text.contains(expected_notice), "{text}");
            assert!(
                text.contains(
                    "- Finding: [P1] src/lib.rs:7 -- retry loses the request\n  Reason: "
                ),
                "{text}"
            );
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
    let trees = match &action {
        ReviewerAction::RecordOverridden { trees } if !old_worker => trees,
        ReviewerAction::RecordReviewed { trees } if old_worker => trees,
        other => panic!("unexpected record action {other:?}"),
    };
    assert_eq!(
        trees[&PathBuf::from("/workspace/app")],
        ("newtree".to_owned(), "bbbbbbb456".to_owned())
    );
    if let Some((environment, error)) = failure {
        *environment.save_error.lock().unwrap() = Some(error.into());
    }
    reply.send(answer_for(&action)).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "an override pins no review baseline"
    );
}

#[tokio::test]
async fn push_override_rejects_empty_or_blank_entries_before_any_capture() {
    let session = session_id("override-invalid");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    for (overrides, expected) in [
        (Vec::new(), "at least one finding"),
        (vec![declined("  ", "reason")], "the finding it declines"),
        (vec![declined("[P1] finding", " \n")], "needs a reason"),
        (
            (0..33).map(|n| declined(&format!("F{n}"), "R")).collect(),
            "at most 32",
        ),
    ] {
        let error = host.override_push(&session, overrides).await.unwrap_err();
        assert!(error.0.contains(expected), "{}", error.0);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "a refused request touches nothing"
    );
    assert_eq!(environment.state().declined_findings.len(), 1);
    host.shutdown().await.unwrap();
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
            tokio::spawn(
                async move { host.override_push(&session, one_override("disagree")).await },
            )
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
        tokio::spawn(async move {
            host.override_push(&session, one_override(&"🔥".repeat(4000)))
                .await
        })
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
        tokio::spawn(async move {
            host.override_push(&session, one_override("incorrect"))
                .await
        })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(
        host.override_push(&session, one_override("again"))
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
    let finishing = finish_override_worker_actions(&mut manager, "override requested");
    tokio::pin!(finishing);
    tokio::select! {
        () = gate.entered() => {},
        () = &mut finishing => panic!("override finished before the blocked save"),
    }
    assert!(!request.is_finished());
    assert!(
        host.override_push(&session, one_override("again"))
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
    finishing.await;
    assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
    shutdown.await.unwrap().unwrap();
    assert!(environment.state().push_findings.is_none());
}

// Hard-won: 9ebd246b2: crashes between state and ref writes lost declined findings.
#[tokio::test]
async fn push_override_retries_after_initial_save_or_ref_failure() {
    for fail_save in [true, false] {
        let session = session_id(if fail_save {
            "override-initial-fail"
        } else {
            "override-ref-fail"
        });
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        pending_push_findings(&environment);
        if fail_save {
            *environment.save_error.lock().unwrap() = Some("injected save failure".into());
        }
        let host =
            TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
        for attempt in 0..2 {
            let request = {
                let host = host.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    host.override_push(&session, one_override("false positive"))
                        .await
                })
            };
            answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")])
                .await;
            if attempt == 0 {
                let RemoteSessionRequest::Submit { reply, .. } = manager.next().await else {
                    panic!("expected notice");
                };
                reply.send(Ok(1)).unwrap();
                if !fail_save {
                    let (_, action, reply) = manager
                        .next_reviewer(|_, action| {
                            matches!(action, ReviewerAction::RecordOverridden { .. })
                        })
                        .await;
                    assert!(matches!(action, ReviewerAction::RecordOverridden { .. }));
                    reply.send(Err("injected ref failure".into())).unwrap();
                }
                assert!(request.await.unwrap().is_err());
                assert!(environment.state().push_findings.is_some());
            } else {
                finish_override_worker_actions(&mut manager, "false positive").await;
                assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
                let state = environment.state();
                assert!(state.push_findings.is_none());
                assert_eq!(state.declined_findings.len(), 2);
            }
        }
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn push_override_retries_after_reviewed_ref_was_recorded_but_state_save_failed() {
    let session = session_id("override-save-retry");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    for attempt in 0..2 {
        let request = {
            let host = host.clone();
            let session = session.clone();
            tokio::spawn(async move {
                host.override_push(&session, one_override("false positive"))
                    .await
            })
        };
        answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
        finish_override_worker_actions_with_failure(
            &mut manager,
            "false positive",
            false,
            (attempt == 0).then_some((&environment, "database busy")),
        )
        .await;
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
            // The first save kept the decline before the ref write. Retrying
            // the failed final save must not duplicate that decline.
            assert_eq!(
                environment.state().declined_findings,
                vec![
                    declined("[P3] old finding", "old reason"),
                    declined(
                        "[P1] src/lib.rs:7 -- retry loses the request",
                        "false positive"
                    ),
                ]
            );
        }
    }
    host.shutdown().await.unwrap();
}

async fn publish_relay_protocol(manager: &FakeManager, session: &str, version: u32) {
    let mut older = view(session, MaterializedExecutionState::Idle);
    older
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(version);
    manager
        .publisher
        .publish(session.to_owned(), older)
        .await
        .unwrap();
}

#[tokio::test]
async fn push_override_on_an_old_worker_records_reviewed_and_says_the_base_moved() {
    let session = session_id("override-old-worker");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    publish_relay_protocol(
        &manager,
        &session,
        mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL - 1,
    )
    .await;
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move {
            host.override_push(&session, one_override("false positive"))
                .await
        })
    };
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    finish_override_worker_actions_for(&mut manager, "false positive", true).await;
    let reply = request.await.unwrap().unwrap();
    assert_eq!(reply["accepted"], true);
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains("worker is older") && message.contains("review base moved"),
        "{message}"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_override_on_a_current_worker_says_the_base_is_unchanged() {
    let session = session_id("override-current-worker");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move {
            host.override_push(&session, one_override("false positive"))
                .await
        })
    };
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    finish_override_worker_actions(&mut manager, "false positive").await;
    let reply = request.await.unwrap().unwrap();
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("review base is unchanged"),
        "{reply}"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_review_baseline_moves_the_base_and_clears_the_declined_list() {
    let session = session_id("set-base");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    environment.state.lock().unwrap().decline_findings(
        [declined("shared", "keep the other repository")],
        vec!["/workspace/app".into(), "/workspace/linked".into()],
    );
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move {
            host.set_push_base(&session, "abc1234".into(), Some("../linked".into()))
                .await
        })
    };
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::SetReviewBase { .. }))
        .await;
    assert!(matches!(
        &action,
        ReviewerAction::SetReviewBase { path: Some(path), commit }
            if path == &PathBuf::from("../linked") && commit == "abc1234"
    ));
    reply
        .send(Ok(ReviewerOutcome::ReviewBaseSet {
            changed: true,
            root: "/workspace/linked".into(),
            commit: "abc1234ffff".into(),
            tree: "settree".into(),
        }))
        .unwrap();
    let reply = request.await.unwrap().unwrap();
    assert_eq!(reply["accepted"], true);
    assert_eq!(reply["commit"], "abc1234ffff");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains("/workspace/linked") && message.contains("start from commit abc1234ffff"),
        "{message}"
    );
    let state = environment.state();
    assert_eq!(
        state.declined_findings[1].repositories,
        vec![PathBuf::from("/workspace/app")]
    );
    assert_eq!(
        state.declined_findings[0].repositories,
        vec![PathBuf::from("/workspace/app")]
    );

    assert!(
        state.push_findings.is_some(),
        "a pending findings verdict stays overridable"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err()
    );
    host.shutdown().await.unwrap();
}

// Hard-won: 9ebd246b2: an older reviewed commit erased declined findings.
#[tokio::test]
async fn set_review_baseline_preserves_state_when_a_later_base_already_exists() {
    let session = session_id("set-base-unchanged");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let original = environment.state();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.set_push_base(&session, "older".into(), None).await })
    };
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::SetReviewBase { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::ReviewBaseSet {
            root: "/workspace/app".into(),
            commit: "later".into(),
            tree: "tree".into(),
            changed: false,
        }))
        .unwrap();
    let result = request.await.unwrap().unwrap();
    assert!(result["message"].as_str().unwrap().contains("later"));
    assert_eq!(environment.state(), original);
    assert!(environment.writes().is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_review_baseline_keeps_the_declined_list_when_the_worker_refuses() {
    let session = session_id("set-base-refused");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move { host.set_push_base(&session, "abc1234".into(), None).await })
    };
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::SetReviewBase { .. }))
        .await;
    reply
        .send(Err(
            "commit abc1234 was never reviewed or overridden; call request_review first".into(),
        ))
        .unwrap();
    let error = request.await.unwrap().unwrap_err();
    assert!(error.0.contains("call request_review first"), "{}", error.0);
    assert_eq!(environment.state().declined_findings.len(), 1);
    assert!(
        environment.writes().is_empty(),
        "a refused base move saves nothing"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_review_baseline_refuses_an_old_worker_and_a_running_review() {
    let session = session_id("set-base-refusals");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());

    // An override in flight holds the session's review slot.
    let busy = {
        let host = host.clone();
        let session = session.clone();
        tokio::spawn(async move {
            host.override_push(&session, one_override("false positive"))
                .await
        })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let error = host
        .set_push_base(&session, "abc1234".into(), None)
        .await
        .unwrap_err();
    assert!(
        error.0.contains("review is open") && error.0.contains("try again"),
        "{}",
        error.0
    );
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "newtree")],
        }))
        .unwrap();
    finish_override_worker_actions(&mut manager, "false positive").await;
    busy.await.unwrap().unwrap();

    publish_relay_protocol(
        &manager,
        &session,
        mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL - 1,
    )
    .await;
    let error = host
        .set_push_base(&session, "abc1234".into(), None)
        .await
        .unwrap_err();
    assert!(
        error.0.contains("restart the session") && error.0.contains("older"),
        "{}",
        error.0
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "a refused base move reaches no worker"
    );
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
        tokio::spawn(async move {
            host.override_push(&session, one_override("incorrect"))
                .await
        })
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
    finish_override_worker_actions_for(&mut manager, "incorrect", true).await;
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
        tokio::spawn(async move {
            host.override_push(&session, one_override("incorrect"))
                .await
        })
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
        "this harness cannot enforce the push gate; use a harness with push review support",
        "nothing to review because base == HEAD",
    ] {
        for manual in [false, true] {
            let request = {
                let host = host.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    if manual {
                        host.start(&session).await
                    } else {
                        host.begin_push(&session, None).await.map(|_| ())
                    }
                })
            };
            let (_, _, reply) = manager
                .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
                .await;
            reply.send(Err(reason.into())).unwrap();
            assert_eq!(request.await.unwrap().unwrap_err().0, reason);
        }
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
    assert!(host.resolve(&session, Resolution::Completed).await.is_err());
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
