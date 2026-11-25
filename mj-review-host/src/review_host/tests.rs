use super::*;
use mj_core::state::{ManagedSessionSnapshot, MaterializedExecutionState, MaterializedSession};
use mj_session_runtime::session_manager::{
    RelaySessionTarget, RemoteSessionRequest, RemoteSessionRequests, spawn_remote_session_manager,
};
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
    publisher: mj_session_runtime::session_manager::RemoteSessionPublisher,
    /// Refusals the next captures answer with, in order, the way a lease
    /// the recovery copy holds refuses a reviewer action.
    capture_refusals: std::collections::VecDeque<String>,
    _shutdown: mj_session_runtime::session_manager::SessionManagerShutdown,
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
            spec: mj_targets::targets::CommandSpec::new("true", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let mut updates = channels.updates;
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
        // Consume initial publication like the daemon; dropping an undrained
        // coalescer leaves its session-update admission in the shared mailbox.
        while updates.try_recv().is_ok() {}
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
        oneshot::Sender<anyhow::Result<ReviewerOutcome>>,
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
                        let hold = self
                            .control
                            .session_gate
                            .enter_foreground(
                                &self.session,
                                "test capture lifecycle",
                                mj_store::recovery_gate::LifecycleOrigin::Person,
                                &std::sync::atomic::AtomicBool::new(false),
                                &mj_targets::targets::ProcessExecutor,
                            )
                            .await
                            .unwrap();
                        let _ = reply.send(Err(anyhow::anyhow!(refusal)));
                        drop(hold);
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
fn answer_for(action: &ReviewerAction) -> anyhow::Result<ReviewerOutcome> {
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
        ReviewerAction::ResolveRepositories { paths } => Ok(ReviewerOutcome::Repositories {
            roots: paths
                .iter()
                .map(|path| {
                    if path == std::path::Path::new(".") {
                        PathBuf::from("/workspace/app")
                    } else {
                        path.clone()
                    }
                })
                .collect(),
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
        attention: Default::default(),
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
    registered_roles: Mutex<Vec<mj_core::worker_launch::ReviewChildRole>>,
    started: Mutex<Vec<String>>,
    closed: Mutex<Vec<String>>,
    close_error: Mutex<Option<String>>,
    /// While set, every close fails, and each attempt is counted.
    close_fails: std::sync::atomic::AtomicBool,
    close_attempts: std::sync::atomic::AtomicUsize,
    /// Closes running now, and the most that ever ran together.
    closes_in_flight: std::sync::atomic::AtomicUsize,
    most_closes_in_flight: std::sync::atomic::AtomicUsize,
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
    review_refusal: Mutex<Option<String>>,
    /// Refusals the next reviewer resolutions answer with, in order.
    resolve_refusals: Mutex<std::collections::VecDeque<String>>,
    resolve_release: Mutex<Option<mj_store::recovery_gate::ForegroundAdmission>>,
    /// How many background holds the host took on the session.
    background_holds: std::sync::atomic::AtomicUsize,
    background_hangs: std::sync::atomic::AtomicBool,
    /// How many of those holds are still held.
    live_holds: Arc<std::sync::atomic::AtomicUsize>,
    /// Set to make reviewer resolution wait forever, the way an Auto
    /// reviewer choice can take minutes on a real host (I2-10).
    resolve_hangs: std::sync::atomic::AtomicBool,
    /// Every report the host handed to persistence, in arrival order.
    saved_reports: Mutex<Vec<mj_store::database::AcceptedReport>>,
}

/// The single-review reading of a stored row. Most host tests run one review
/// in one session, and ask what the row says about it.
trait OnlyReview {
    fn active(&self) -> Option<String>;
    fn active_group(&self) -> Option<mj_core::storage::ActiveReviewGroup>;
    fn findings(&self) -> Option<mj_core::storage::PushFindingsRecord>;
}

impl OnlyReview for TurnReviewState {
    fn active(&self) -> Option<String> {
        self.open_reviews
            .first()
            .and_then(|review| review.active.clone())
    }

    fn active_group(&self) -> Option<mj_core::storage::ActiveReviewGroup> {
        self.open_reviews
            .first()
            .and_then(|review| review.active_group.clone())
    }

    fn findings(&self) -> Option<mj_core::storage::PushFindingsRecord> {
        self.push_findings.first().cloned()
    }
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
            registered_roles: Mutex::new(Vec::new()),
            started: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            close_error: Mutex::new(None),
            close_fails: std::sync::atomic::AtomicBool::new(false),
            close_attempts: std::sync::atomic::AtomicUsize::new(0),
            closes_in_flight: std::sync::atomic::AtomicUsize::new(0),
            most_closes_in_flight: std::sync::atomic::AtomicUsize::new(0),
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
            review_refusal: Mutex::new(None),
            resolve_refusals: Mutex::new(std::collections::VecDeque::new()),
            resolve_release: Mutex::new(None),
            background_holds: std::sync::atomic::AtomicUsize::new(0),
            background_hangs: std::sync::atomic::AtomicBool::new(false),
            live_holds: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            resolve_hangs: std::sync::atomic::AtomicBool::new(false),
            saved_reports: Mutex::new(Vec::new()),
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
        if state
            .open_reviews
            .iter()
            .all(|review| review.active_group.is_none())
        {
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
        role: mj_core::worker_launch::ReviewChildRole,
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
        self.registered_roles.lock().unwrap().push(role);
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
            let running = self
                .closes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.most_closes_in_flight
                .fetch_max(running, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            let _done = scopeguard_decrement(&self.closes_in_flight);
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
                self.resolve_release.lock().unwrap().take();
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

    fn resolve_choice<'a>(
        &'a self,
        choice: mj_core::config::SubagentChoice,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    > {
        Box::pin(async move {
            Ok(mj_core::review::settings::ResolvedReviewSettings {
                profile: "writer".into(),
                main: mj_core::review::settings::ReviewModelSettings {
                    model: Some(choice.model),
                    effort: choice.effort,
                    fast_mode: false,
                },
                ..Default::default()
            })
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
        if state.open_reviews.is_empty()
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
        for group in next
            .open_reviews
            .iter()
            .filter_map(|review| review.active_group.as_ref())
        {
            let previous = stored
                .open_reviews
                .iter()
                .filter_map(|review| review.active_group.as_ref())
                .find(|previous| previous.driver.review_id == group.driver.review_id);
            for event in partial_review_events(session_id, previous, group) {
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

    fn save_reviewer_outcome(
        &self,
        session_id: &str,
        state: &TurnReviewState,
        reports: &[mj_store::database::AcceptedReport],
    ) -> Result<(), String> {
        self.saved_reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(reports.iter().cloned());
        self.save_state(session_id, state)
    }

    fn clear_interrupted(&self) -> Result<Vec<String>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interrupted = state
            .open_reviews
            .iter()
            .any(|review| review.active_group.is_none());
        state
            .open_reviews
            .retain(|review| review.active_group.is_some());
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
        let hangs = self
            .background_hangs
            .load(std::sync::atomic::Ordering::Acquire);
        Box::pin(async move {
            if hangs {
                std::future::pending::<()>().await;
            }
            Some(hold)
        })
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
    *environment.resolve_release.lock().unwrap() = Some(
        manager
            .control
            .session_gate
            .enter_foreground(
                session,
                "test reviewer lifecycle",
                mj_store::recovery_gate::LifecycleOrigin::Person,
                &std::sync::atomic::AtomicBool::new(false),
                &mj_targets::targets::ProcessExecutor,
            )
            .await
            .unwrap(),
    );
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
        brief_writer: None,
        reviewers: vec![
            mj_core::config::SubagentChoice {
                model: "test-reviewer".into(),
                effort: None
            };
            count
        ],
    })
}

/// One reviewer, preceded by a brief writer.
fn armed_with_writer() -> ReviewConfigSource {
    Arc::new(move |_| ReviewConfig {
        legacy: None,
        disabled: false,
        brief_writer: Some(mj_core::config::BriefWriterConfig {
            enabled: true,
            model: "test-writer".into(),
            effort: Some("high".into()),
        }),
        reviewers: vec![mj_core::config::SubagentChoice {
            model: "test-reviewer".into(),
            effort: None,
        }],
    })
}

/// Waits for the review's `count`th child to be registered and started.
async fn wait_for_nth_child(environment: &FakeEnvironment, count: usize) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let children = environment.children.lock().unwrap().clone();
            if let Some((child, ..)) = children.get(count - 1)
                && environment.started.lock().unwrap().contains(child)
            {
                return child.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("child {count} was not started"))
}

/// The next notice the requester's session records.
async fn next_notice(manager: &mut FakeManager) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } = manager.next().await
            {
                let _ = reply.send(Ok(1));
                break text;
            }
        }
    })
    .await
    .expect("the requester is told")
}

async fn finish_the_review(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
    session: &str,
    reviewer: &str,
) {
    finish_child(host, environment, session, reviewer, Some("No findings.")).await;
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    reply.send(answer_for(&action)).unwrap();
    wait_until_review_resolves(host, session).await;
}

#[tokio::test]
async fn a_brief_writer_hands_back_before_any_reviewer_starts_and_its_brief_reaches_the_reviewer() {
    let session = session_id("brief-handback");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed_with_writer(),
        environment.clone(),
    );
    let writer = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    {
        let children = environment.children.lock().unwrap().clone();
        assert_eq!(children.len(), 1, "no reviewer starts before the brief");
        assert_eq!(children[0].1.profile, "writer");
        assert_eq!(children[0].1.main.effort.as_deref(), Some("high"));
        assert!(!children[0].2.contains("Relation brief for a code change"));
        assert!(
            children[0]
                .2
                .starts_with(mj_core::subagent::READ_ONLY_TASK_SCOPE)
        );
        assert!(children[0].2.contains("## The change"));
        assert!(children[0].2.contains(
            "git -C /workspace/app diff --no-ext-diff --ignore-submodules=none base new"
        ));
        assert!(!children[0].2.contains("Do not edit files or push"));
    }
    let view = host.view(&session).unwrap();
    let rows: Vec<_> = view
        .roles
        .iter()
        .map(|role| {
            (
                role.label.as_str(),
                role.state.label(),
                role.child_session_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [("Brief writer", "running", Some(writer.as_str()))],
        "the writer is visible while it works"
    );
    let group = environment.state().active_group().unwrap();
    assert!(
        group
            .registrations
            .iter()
            .any(|item| item.role == "brief-writer"
                && item.child_session_id.as_deref() == Some(writer.as_str()))
    );

    host.reviewer_finished(
        session.clone(),
        writer.clone(),
        Some("## Paths\n- `src/a.rs:1` is called by `src/b.rs:2`".into()),
    )
    .await
    .unwrap();
    let reviewer = wait_for_nth_child(&environment, 2).await;
    let children = environment.children.lock().unwrap().clone();
    assert!(
        children[1]
            .2
            .contains("## Relations computed from the repository")
    );
    assert!(
        children[1]
            .2
            .contains("`src/a.rs:1` is called by `src/b.rs:2`")
    );
    assert!(children[1].2.contains("Do not edit files or push"));
    assert_eq!(children[1].1.profile, "reviewer");
    assert_eq!(
        *environment.registered_roles.lock().unwrap(),
        [
            mj_core::worker_launch::ReviewChildRole::BriefWriter,
            mj_core::worker_launch::ReviewChildRole::Reviewer
        ]
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment.closed.lock().unwrap().contains(&writer) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the writer is closed once its brief is used");

    finish_the_review(&mut manager, &host, &environment, &session, &reviewer).await;
    assert_eq!(environment.children.lock().unwrap().len(), 2);
    assert_eq!(environment.live_holds(), 0);
    host.shutdown().await.unwrap();
}

// Hard-won: d782078aa review: reconciliation matched a logical role key instead of the submitted prompt.
#[tokio::test]
async fn a_brief_writer_that_ends_with_a_recorded_startup_handback_keeps_its_brief() {
    let session = session_id("brief-recorded-handback");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed_with_writer(),
        environment.clone(),
    );
    let writer = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let handback = stored_handback(
        &environment,
        "brief-writer",
        "## Paths\n- `a.rs:1` calls `b.rs:2`",
    );
    environment.standings.lock().unwrap().insert(
        writer.clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback,
        },
    );
    host.child_ended(writer, "writer ended after recording its brief".into());
    let reviewer = wait_for_nth_child(&environment, 2).await;
    let prompt = environment.children.lock().unwrap()[1].2.clone();
    assert!(prompt.contains("`a.rs:1` calls `b.rs:2`"), "{prompt}");
    finish_the_review(&mut manager, &host, &environment, &session, &reviewer).await;
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_brief_writer_that_ends_without_a_handback_leaves_the_review_to_run_without_a_brief() {
    let session = session_id("brief-no-handback");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed_with_writer(),
        environment.clone(),
    );
    let writer = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let reason = "error: Cannot start the relation brief writer: Muse adapter 0.0.0 does not advertise a persistent instruction channel";
    host.child_ended(writer.clone(), reason.into());
    let notice = next_notice(&mut manager).await;
    assert!(
        notice.starts_with("Review started without a relation brief:"),
        "{notice}"
    );
    assert!(notice.contains(reason), "{notice}");
    let reviewer = wait_for_nth_child(&environment, 2).await;
    let prompt = environment.children.lock().unwrap()[1].2.clone();
    assert!(!prompt.contains("Relations computed"), "{prompt}");
    assert!(prompt.contains("Do not edit files or push"));
    finish_the_review(&mut manager, &host, &environment, &session, &reviewer).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment.closed.lock().unwrap().contains(&writer) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the ended writer is still closed");
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_brief_writer_that_cannot_launch_does_not_stop_the_review() {
    let session = session_id("brief-no-launch");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    *environment.register_error.lock().unwrap() = Some("no capacity for the writer".into());
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed_with_writer(),
        environment.clone(),
    );
    let reviewer = open_a_review_to_its_first_poll(&mut manager, &host, &environment).await;
    let notice = next_notice(&mut manager).await;
    assert!(
        notice.contains("Review started without a relation brief: no capacity for the writer"),
        "{notice}"
    );
    let children = environment.children.lock().unwrap().clone();
    assert_eq!(children.len(), 1, "only the reviewer was ever registered");
    assert_eq!(children[0].1.profile, "reviewer");
    assert!(!children[0].2.contains("Relations computed"));
    finish_the_review(&mut manager, &host, &environment, &session, &reviewer).await;
    host.shutdown().await.unwrap();
}

/// A review a daemon restart interrupted while its writer worked: the writer's
/// session ended while no daemon was attached, with `handback` recorded or not.
async fn restart_while_the_writer_ended(
    name: &str,
    handback: Option<&str>,
    outcome_saved: bool,
) -> (String, String) {
    let session = session_id(name);
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let review_id = "restored-brief";
    let (mut driver, _) = TurnReviewDriver::start(
        TurnReviewSeed {
            task: "inspect the changes".into(),
            user_messages: Vec::new(),
            declined: Vec::new(),
            brief: None,
        },
        review_id.into(),
        std::num::NonZeroUsize::MIN,
    );
    driver.delta_captured_with_writer(vec![push_delta("/workspace/app", "newtree")], true);
    if outcome_saved {
        match handback {
            Some(text) => {
                driver.brief_written(text.to_owned());
            }
            None => {
                driver.brief_failed("writer stopped".into());
            }
        }
    }
    *environment.owner.lock().unwrap() = Some(session.clone());
    *environment.state.lock().unwrap() = TurnReviewState {
        open_reviews: vec![mj_core::storage::OpenReview {
            active: Some(review_id.into()),
            active_group: Some(mj_core::storage::ActiveReviewGroup {
                brief_writer: Some(ReviewerIdentity::default()),
                driver: driver.snapshot(),
                reviewers: vec![ReviewerIdentity::default()],
                registrations: vec![
                    mj_core::storage::ReviewerRegistration {
                        role: "reviewer-1".into(),
                        request_key: "key-reviewer".into(),
                        child_session_id: None,
                    },
                    mj_core::storage::ReviewerRegistration {
                        role: "brief-writer".into(),
                        request_key: "key-writer".into(),
                        child_session_id: Some("writer-child".into()),
                    },
                ],
            }),
        }],
        ..Default::default()
    };
    environment.standings.lock().unwrap().insert(
        "writer-child".into(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: handback
                .and_then(|message| stored_handback(&environment, "brief-writer", message)),
        },
    );
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed_with_writer(),
        environment.clone(),
    );
    let reviewer = wait_for_nth_child(&environment, 1).await;
    if handback.is_none() {
        let notice = next_notice(&mut manager).await;
        assert!(
            notice.starts_with("Review started without a relation brief:"),
            "{notice}"
        );
        if outcome_saved {
            assert!(notice.contains("writer stopped"), "{notice}");
        }
    }
    // Give a wrongly repeated start time to show up.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let children = environment.children.lock().unwrap().clone();
    assert_eq!(children.len(), 1, "one reviewer, and no second writer");
    assert!(
        !environment
            .started
            .lock()
            .unwrap()
            .contains(&"writer-child".to_owned()),
        "the ended writer is not started again"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !environment
            .closed
            .lock()
            .unwrap()
            .contains(&"writer-child".to_owned())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the restored writer is cleaned up");
    host.shutdown().await.unwrap();
    assert_eq!(
        environment
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|child| child.as_str() == "writer-child")
            .count(),
        1
    );
    (reviewer, children[0].2.clone())
}

#[tokio::test]
async fn a_restart_after_the_writer_handed_back_starts_the_reviewer_once_with_the_brief() {
    let (_, prompt) = restart_while_the_writer_ended(
        "restored-brief-written",
        Some("## Paths\n- `a.rs:1` is called by `b.rs:2`"),
        false,
    )
    .await;
    assert!(
        prompt.contains("## Relations computed from the repository"),
        "{prompt}"
    );
    assert!(prompt.contains("`a.rs:1` is called by `b.rs:2`"));
}

#[tokio::test]
async fn a_restart_after_the_writer_ended_without_a_handback_starts_the_reviewer_without_a_brief() {
    let (_, prompt) = restart_while_the_writer_ended("restored-brief-lost", None, false).await;
    assert!(!prompt.contains("Relations computed"), "{prompt}");
}

// Hard-won: 5fc728617: restart discarded a persisted writer cleanup after either outcome.
#[tokio::test]
async fn a_restart_after_the_writer_outcome_was_saved_closes_it_once() {
    for handback in [Some("## Paths\n- `a.rs:1` calls `b.rs:2`"), None] {
        let (_, prompt) =
            restart_while_the_writer_ended("saved-writer-outcome", handback, true).await;
        assert_eq!(prompt.contains("Relations computed"), handback.is_some());
    }
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
                .active_group()
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
    mj_core::runtime::spawn(async move {
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

    assert!(
        tokio::time::timeout(Duration::from_secs(1), host.has_open_review(session))
            .await
            .expect("the host loop remains responsive while persistence blocks")
            .unwrap(),
        "the review being recorded counts as open"
    );
    assert!(
        host.view(session).is_some(),
        "preparation is visible while persistence runs"
    );

    open_gate.release();
    // The child stays open without a handback while shutdown drains persistence.
    let _child = wait_for_child(&environment, &host, session).await;
    assert!(environment.state().active().is_some());
    assert!(host.view(session).is_some());

    let close_gate = environment.block_saves();
    let first_host = host.clone();
    let second_host = host.clone();
    let first = mj_core::runtime::spawn(async move { first_host.shutdown().await });
    let second = mj_core::runtime::spawn(async move { second_host.shutdown().await });
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
        environment.state().active().is_some(),
        "the in-flight marker survives shutdown, so the next daemon can tell the person"
    );
    assert!(host.view(session).is_none());
    let writes = environment.writes();
    assert!(
        writes
            .first()
            .is_some_and(|(state, _)| state.active().is_some())
    );
    assert!(
        writes
            .last()
            .is_some_and(|(state, _)| state.active().is_some())
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
    assert!(environment.state().active().is_some());

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
    assert!(state.active_group().is_some());
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
    let cause = "Cannot start the automated reviewer: Muse adapter 0.0.0 does not advertise a persistent instruction channel";
    *environment.start_error.lock().unwrap() = Some(cause.to_owned());
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
            if host.view(session).is_none() && environment.state().active().is_none() {
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
            let request = manager.next().await;
            assert!(
                !matches!(
                    &request,
                    RemoteSessionRequest::Reviewer {
                        action: ReviewerAction::RecordReviewed { .. },
                        ..
                    }
                ),
                "a failed reviewer must not advance the baseline"
            );
            if let RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } = request
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
    assert!(notice.contains(cause), "{notice}");

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
    assert!(state.active().is_none());
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
        mj_core::runtime::spawn(async move {
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
            .active_group()
            .as_ref()
            .unwrap()
            .driver
            .roles[0]
            .outcome
            .is_none()
    );
    gate.release();
    finished.await.unwrap().unwrap();
    assert_eq!(
        environment.saved_reports.lock().unwrap().clone(),
        vec![mj_store::database::AcceptedReport {
            review_id: environment.state().active_group().unwrap().driver.review_id,
            role: "reviewer-1".into(),
            text: "No findings.".into(),
        }],
        "the store receives the exact text the host settled the role from"
    );
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
        brief_writer: None,
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
    let review_id = environment.state().active_group().unwrap().driver.review_id;
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
            environment.state().findings().is_some(),
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
        while host.view(&session).is_some() || environment.state().active().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the failed group closes itself");
    assert!(environment.state().active().is_none());
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
                        .active_group()
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
            protocol: None,
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
    let second = {
        let host = host.clone();
        let session = session.to_owned();
        mj_core::runtime::spawn(async move { host.start(&session).await })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "newer")],
        }))
        .unwrap();
    let refusal = second
        .await
        .unwrap()
        .expect_err("a root is in one review at a time");
    assert_eq!(
        refusal.0,
        "/workspace/app is already in the open review of /workspace/app. Wait for that verdict and then request /workspace/app together."
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
    // A stopping daemon is told apart from a host whose task died.
    assert_eq!(
        host.has_open_review(session).await,
        Err(crate::review_host::OpenReviewUnknown::ShuttingDown)
    );
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
    assert!(environment.state().active_group().is_none());
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
    let group = environment.state().active_group().unwrap();
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
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let seed = mj_core::review::driver::TurnReviewSeed {
        task: "inspect the push".into(),
        user_messages: Vec::new(),
        declined: Vec::new(),
        brief: None,
    };
    let (mut driver, _) = TurnReviewDriver::start(
        seed,
        "group-1".into(),
        std::num::NonZeroUsize::new(1).unwrap(),
    );
    driver.delta_captured(vec![push_delta("/workspace/app", "newtree")]);
    let state = TurnReviewState {
        push_findings: vec![mj_core::storage::PushFindingsRecord {
            review_id: "older-verdict".into(),
            trees: [(
                PathBuf::from("/workspace/app"),
                ("newtree".into(), "bbbbbbb456".into()),
            )]
            .into(),
            synthesis: "older finding".into(),
        }],
        open_reviews: vec![mj_core::storage::OpenReview {
            active: Some("group-1".into()),
            active_group: Some(mj_core::storage::ActiveReviewGroup {
                brief_writer: None,
                driver: driver.snapshot(),
                reviewers: vec![ReviewerIdentity::default()],
                registrations: vec![mj_core::storage::ReviewerRegistration {
                    role: "reviewer-1".into(),
                    request_key: "stable-key".into(),
                    child_session_id: Some("busy-child".into()),
                }],
            }),
        }],
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
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
    };
    let override_request = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override("old finding"))
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(!push.is_finished() && !override_request.is_finished());
    gate.release();
    // The restored review owns its root, so a review that captures it is
    // refused when it is ready to open, and an override of the older verdict
    // for that root is refused at once.
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "othertree")],
        }))
        .unwrap();
    assert!(
        push.await
            .unwrap()
            .unwrap_err()
            .0
            .contains("is already in the open review of")
    );
    assert!(
        override_request
            .await
            .unwrap()
            .unwrap_err()
            .0
            .contains("is still open")
    );
    assert_eq!(
        environment
            .state()
            .active_group()
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
            brief: None,
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
        open_reviews: vec![mj_core::storage::OpenReview {
            active: Some("partly-failed-group".into()),
            active_group: Some(mj_core::storage::ActiveReviewGroup {
                brief_writer: None,
                driver: driver.snapshot(),
                reviewers: vec![ReviewerIdentity::default(); 2],
                registrations,
            }),
        }],
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
    assert!(environment.state().active().is_some());
}

struct DecrementOnDrop<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for DecrementOnDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn scopeguard_decrement(counter: &std::sync::atomic::AtomicUsize) -> DecrementOnDrop<'_> {
    DecrementOnDrop(counter)
}

// A restart finds every finished reviewer of an old store at once (19,196 in
// the live one). The sweep closes them concurrently up to its measured width,
// and a close that fails retries on its own instead of holding the queue.
#[tokio::test]
async fn startup_sweep_closes_finished_reviewers_concurrently_within_its_width_past_a_failing_one()
{
    let session = session_id("sweep-many");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    for index in 0..200 {
        environment
            .child_relations
            .lock()
            .unwrap()
            .push(ReviewerChildRelation {
                parent_session_id: session.clone(),
                request_key: format!("reviewer:old-{index}"),
                child_session_id: format!("old-child-{index}"),
            });
    }
    *environment.close_error.lock().unwrap() = Some("worker unreachable".into());
    let host = TurnReviewHost::spawn_in(
        manager.control.clone(),
        armed(Some("reviewer")),
        environment.clone(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while environment.closed.lock().unwrap().len() < 200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every finished reviewer is closed, the failed one on its retry");
    let most = environment
        .most_closes_in_flight
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        most > 1 && most <= super::reviewer_child::SWEEP_CLOSE_WIDTH,
        "the sweep closes concurrently but never past its width: {most}"
    );
    host.shutdown().await.unwrap();
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
            brief: None,
        },
        "restored-group".into(),
        std::num::NonZeroUsize::new(count).unwrap(),
    );
    driver.delta_captured(vec![push_delta("/workspace/app", "newtree")]);
    let children: Vec<String> = (1..=count).map(|n| format!("busy-child-{n}")).collect();
    *environment.owner.lock().unwrap() = Some(manager.session.clone());
    *environment.state.lock().unwrap() = TurnReviewState {
        open_reviews: vec![mj_core::storage::OpenReview {
            active: Some("restored-group".into()),
            active_group: Some(mj_core::storage::ActiveReviewGroup {
                brief_writer: None,
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
        }],
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
    let request_key = environment
        .state()
        .active_group()
        .expect("a restored group")
        .registrations
        .iter()
        .find(|item| item.role == role)
        .expect("the role is registered")
        .request_key
        .clone();
    // Use the actual submission path, not the driver's logical command key.
    let command_id = mj_store::database::api_followup_steps(
        &request_key,
        mj_store::database::StartFollowup {
            prompt: Some("review the changes".into()),
            ..Default::default()
        },
    )
    .into_iter()
    .find_map(|(id, step)| {
        matches!(step, mj_store::database::StartupStep::ApiPrompt { .. }).then_some(id)
    })
    .expect("the submitted prompt step");
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
    assert!(environment.state().active_group().is_none());
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
    let first = mj_core::runtime::spawn({
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
    // The failed save is followed by a repair save of the restored state, and
    // then by the second reviewer's save; both fail too.
    environment
        .extra_save_failures
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let first = mj_core::runtime::spawn({
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
    let second = mj_core::runtime::spawn({
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
    let handback = stored_handback(&environment, "reviewer-1", "real defect").unwrap();
    let recovered = mj_core::continuation::restart_continuation_id(&handback.command_id);
    // Hard-won: ebb2e9611: the original handback did not answer its recovered turn.
    assert_eq!(
        ChildStanding {
            ended: None,
            handback: Some(handback.clone())
        }
        .answer_to(&recovered)
        .as_deref(),
        Some("real defect")
    );
    let mut handback = handback;
    handback.command_id = recovered;
    environment.standings.lock().unwrap().insert(
        children[0].clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: Some(handback),
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

#[tokio::test]
async fn a_restored_reviewer_can_hand_back_its_preloaded_prompt() {
    let session = session_id("restored-preloaded-handback");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let (host, children) = restore_busy_group(&manager, &environment, 2, &[]).await;
    let mut handback = stored_handback(&environment, "reviewer-1", "real defect").unwrap();
    let startup_key = mj_core::subagent::SubagentKind::Reviewer
        .startup_request_key(&format!("key-{}", children[0]));
    let group = mj_core::continuation::GeneratedPrompt::subagent_spawn_id(&startup_key);
    handback.command_id = mj_store::database::startup_step_id(&group, "prompt");
    environment.standings.lock().unwrap().insert(
        children[0].clone(),
        ChildStanding {
            ended: Some("stopped".into()),
            handback: Some(handback),
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
    assert!(complete && failures.is_empty(), "{failures:?}");
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
    assert!(environment.state().active().is_none());
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
        brief_writer: None,
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
        brief_writer: None,
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
        host.begin_push(&session, ReviewScope::Paths(vec!["../linked".into()]))
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
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::Paths(vec!["../linked".into()]))
                .await
        })
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

// A request from a session whose review is off is accepted without choosing
// reviewers, and the pushed tree is recorded so an already-installed gate
// does not block the push.
#[tokio::test]
async fn a_review_request_with_review_off_records_the_tree_and_chooses_no_reviewer() {
    let session = session_id("reviewoff");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let off: ReviewConfigSource = Arc::new(|_| ReviewConfig::default());
    let host = TurnReviewHost::spawn_in(manager.control.clone(), off, environment.clone());
    let mut running = view(
        &session,
        MaterializedExecutionState::Running { started_at_ms: 1 },
    );
    running
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL);
    manager
        .publisher
        .publish(session.clone(), running)
        .await
        .unwrap();
    let request = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
    };
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![mj_core::relay::RepoDelta {
                root: "/workspace/repo".into(),
                base_commit: Some("aaaaaaa123".into()),
                head_commit: Some("bbbbbbb456".into()),
                current_tree: "newtree".into(),
                ..Default::default()
            }],
        }))
        .unwrap();
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordOverridden { .. }))
        .await;
    let ReviewerAction::RecordOverridden { trees } = action else {
        unreachable!()
    };
    assert_eq!(
        trees.get(&PathBuf::from("/workspace/repo")),
        Some(&("newtree".to_owned(), "bbbbbbb456".to_owned()))
    );
    reply.send(Ok(ReviewerOutcome::BaselineAdvanced)).unwrap();
    let accepted = request.await.unwrap().unwrap();
    assert_eq!(accepted["accepted"], true);
    assert_eq!(accepted["review"], "off");
    assert!(environment.children.lock().unwrap().is_empty());
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
    open_commit_review_with_delta(manager, host, environment, manual, None).await
}

async fn open_commit_review_with_delta(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
    manual: bool,
    delta: Option<mj_core::relay::RepoDelta>,
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
        mj_core::runtime::spawn(async move {
            if manual {
                host.start(&session)
                    .await
                    .map(|_| serde_json::json!({"accepted":true}))
            } else {
                host.begin_push(&session, ReviewScope::WorkingDirectory)
                    .await
            }
        })
    };
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(matches!(action, ReviewerAction::CaptureDelta { .. }));
    reply
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![delta.unwrap_or_else(|| mj_core::relay::RepoDelta {
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
            })],
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
            mj_core::runtime::spawn(async move {
                if push {
                    host.begin_push(&session, ReviewScope::WorkingDirectory)
                        .await
                        .map(|_| ())
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
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
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
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
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
// Hard-won: 4dc0e2851: review-off cases waited for a reviewer instead of answering RecordOverridden.
// Review-off acceptance is covered by a_review_request_with_review_off_records_the_tree_and_chooses_no_reviewer.
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
            "manualturn",
            Some("No findings."),
            "Review clean for commits",
        ),
    ] {
        let session = session_id(name);
        let clean = report == Some("No findings.");
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        if name == "pushclean" {
            pending_push_findings(&environment);
            environment.state.lock().unwrap().decline_findings(
                [declined("other repository", "still applies")],
                vec!["/workspace/other".into()],
            );
        }
        let config: ReviewConfigSource = Arc::new(move |_| ReviewConfig {
            legacy: None,
            disabled: false,
            brief_writer: None,
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
        let refusal =
            refused_overlapping_review(&mut manager, &host, &session, "/workspace/app").await;
        // This worker is below the concurrency protocol: one review at a time.
        assert!(refusal.contains("a review is already open"), "{refusal}");
        let finished = {
            let host = host.clone();
            let session = session.clone();
            let child = child.clone();
            let report = report.map(str::to_owned);
            mj_core::runtime::spawn(
                async move { host.reviewer_finished(session, child, report).await },
            )
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
        assert_eq!(state.findings().is_some(), name == "pushfindings");
        if let Some(findings) = &state.findings() {
            assert_eq!(
                findings.synthesis,
                format!("Reviewer 1 (session {child})\nA correctness defect needs a fix")
            );
            assert_eq!(
                findings.trees[&PathBuf::from("/workspace/app")],
                ("newtree".into(), "bbbbbbb456".into())
            );
        }
        assert!(state.active().is_none());
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
    assert!(state.findings().is_none());
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
        .active_group()
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
    assert!(environment.state().active_group().is_none());
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
        push_findings: vec![mj_core::storage::PushFindingsRecord {
            review_id: "review-1".into(),
            trees: [(
                PathBuf::from("/workspace/app"),
                ("newtree".into(), "bbbbbbb456".into()),
            )]
            .into(),
            synthesis: "fix the regression".into(),
        }],
        ..Default::default()
    };
}

/// Asks for a review of `root` while another review of it is open, and
/// returns the refusal. The refusal comes when the preparation, which captures
/// first, is ready to open: only then are its roots known.
async fn refused_overlapping_review(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    session: &str,
    root: &str,
) -> String {
    let request = {
        let host = host.clone();
        let session = session.to_owned();
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta(root, "overlapping-tree")],
        }))
        .unwrap();
    request.await.unwrap().unwrap_err().0
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

async fn finish_override_worker_actions(
    manager: &mut FakeManager,
    expected_notice: &str,
) -> String {
    finish_override_worker_actions_for(manager, expected_notice, false).await
}

/// `old_worker` is a session whose worker predates `RecordOverridden`.
async fn finish_override_worker_actions_for(
    manager: &mut FakeManager,
    expected_notice: &str,
    old_worker: bool,
) -> String {
    finish_override_worker_actions_with_failure(manager, expected_notice, old_worker, None).await
}

async fn finish_override_worker_actions_with_failure(
    manager: &mut FakeManager,
    expected_notice: &str,
    old_worker: bool,
    failure: Option<(&FakeEnvironment, &str)>,
) -> String {
    let notice = match manager.next().await {
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
            text
        }
        other => panic!(
            "expected the override audit before any ref change, got {}",
            other.session_id()
        ),
    };
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
    notice
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
        let error = host
            .override_push(&session, None, overrides)
            .await
            .unwrap_err();
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
            mj_core::runtime::spawn(async move {
                host.override_push(&session, None, one_override("disagree"))
                    .await
            })
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
        assert_eq!(environment.state().findings().is_some(), pending);
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
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override(&"🔥".repeat(4000)))
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
    assert!(environment.state().findings().is_some());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_override_keeps_reservation_and_drains_accepted_saves_on_shutdown() {
    let session = session_id("override-shutdown");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let request = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override("incorrect"))
                .await
        })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    assert!(
        host.override_push(&session, None, one_override("again"))
            .await
            .unwrap_err()
            .0
            .contains("override is still running")
    );
    // A review of the same repository is prepared, and refused when it is
    // ready to open: the override holds its root.
    let review = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
    };
    let (_, _, review_capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    review_capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "othertree")],
        }))
        .unwrap();
    assert!(
        review
            .await
            .unwrap()
            .unwrap_err()
            .0
            .contains("being changed by the override")
    );
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "newtree")],
        }))
        .unwrap();
    let gate = environment.block_saves();
    let RemoteSessionRequest::Submit { command, reply, .. } = manager.next().await else {
        panic!("expected override audit notice");
    };
    assert!(matches!(command, RelayCommand::RecordNotice { .. }));
    reply.send(Ok(1)).unwrap();
    gate.entered().await;
    assert!(!request.is_finished());
    assert!(
        host.override_push(&session, None, one_override("again"))
            .await
            .unwrap_err()
            .0
            .contains("override is still running")
    );
    let shutdown = {
        let host = host.clone();
        mj_core::runtime::spawn(async move { host.shutdown().await })
    };
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "shutdown drains accepted local persistence"
    );
    let error = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("shutdown reports interruption before draining the save")
        .unwrap()
        .unwrap_err();
    assert!(error.0.contains("repeat the request after restart"));
    gate.release();
    shutdown.await.unwrap().unwrap();
    assert!(environment.state().findings().is_some());
    assert_eq!(environment.state().declined_findings.len(), 2);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), manager.requests.recv())
            .await
            .is_err(),
        "shutdown does not continue from persistence into a worker ref update"
    );
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
                mj_core::runtime::spawn(async move {
                    host.override_push(&session, None, one_override("false positive"))
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
                    reply
                        .send(Err(anyhow::anyhow!("injected ref failure")))
                        .unwrap();
                }
                assert!(request.await.unwrap().is_err());
                assert!(environment.state().findings().is_some());
            } else {
                finish_override_worker_actions(&mut manager, "false positive").await;
                assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
                let state = environment.state();
                assert!(state.findings().is_none());
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
            mj_core::runtime::spawn(async move {
                host.override_push(&session, None, one_override("false positive"))
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
                environment.state().findings().is_some(),
                "a retry must still know the exact reviewed trees"
            );
        } else {
            assert_eq!(result.unwrap()["accepted"], true);
            assert!(environment.state().findings().is_none());
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
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override("false positive"))
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
        mj_core::runtime::spawn(async move {
            let mut overrides = one_override(
                "false positive\n- Finding: quoted reason\nOverridden synthesis (review quoted):",
            );
            overrides.push(declined(
                "[P3] real finding after the quote",
                "out of scope",
            ));
            host.override_push(&session, None, overrides).await
        })
    };
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    let notice = finish_override_worker_actions(
        &mut manager,
        "false positive\n    - Finding: quoted reason\n    Overridden synthesis (review quoted):",
    )
    .await;
    let disclosures = mj_client::transcript::transcript_disclosures(
        mj_core::transcript::ChatRole::System,
        notice.lines(),
    );
    assert_eq!(
        disclosures[0].summary,
        [
            "- [P1] src/lib.rs:7 -- retry loses the request",
            "- [P3] real finding after the quote",
        ]
    );
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
        mj_core::runtime::spawn(async move {
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
        state.findings().is_some(),
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
        mj_core::runtime::spawn(
            async move { host.set_push_base(&session, "older".into(), None).await },
        )
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
        mj_core::runtime::spawn(async move {
            host.set_push_base(&session, "abc1234".into(), None).await
        })
    };
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::SetReviewBase { .. }))
        .await;
    reply
        .send(Err(anyhow::anyhow!(
            "commit abc1234 was never reviewed or overridden; call request_review first"
        )))
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
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override("false positive"))
                .await
        })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    let base = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.set_push_base(&session, "abc1234".into(), None).await
        })
    };
    let (_, _, resolved) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::ResolveRepositories { .. }))
        .await;
    resolved
        .send(Ok(ReviewerOutcome::Repositories {
            roots: vec!["/workspace/app".into()],
        }))
        .unwrap();
    let error = base.await.unwrap().unwrap_err();
    assert!(
        error.0.contains("override is still running") && error.0.contains("try again"),
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
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
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
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override("incorrect"))
                .await
        })
    };
    clear_gate.entered().await;
    // The override never reads the row: the host composes every save from the
    // state it owns, so nothing the override saves can predate the clearing
    // save queued before it.
    clear_gate.release();
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    finish_override_worker_actions_for(&mut manager, "incorrect", true).await;
    assert_eq!(override_request.await.unwrap().unwrap()["accepted"], true);
    assert!(environment.state().findings().is_none());
    assert!(
        environment.state().open_reviews.is_empty(),
        "the cancelled review's marker is not written back"
    );
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
        mj_core::runtime::spawn(async move {
            host.override_push(&session, None, one_override("incorrect"))
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
        environment.state().active().is_none(),
        "override preparation is not a review to recover"
    );
    assert!(environment.state().findings().is_some());
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
        host.begin_push(&session, ReviewScope::WorkingDirectory)
            .await
            .unwrap_err()
            .0,
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
    assert!(environment.state().active_group().is_none());
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_review_refusals_leave_no_prompt_hold_or_reviewer_child() {
    let session = session_id("pushpreparation");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let moving =
        mj_session_runtime::session_manager::move_ownership::MoveMutationGuard::reserve(&session)
            .unwrap();
    assert_eq!(
        host.begin_push(&session, ReviewScope::WorkingDirectory)
            .await
            .unwrap_err()
            .0,
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
                mj_core::runtime::spawn(async move {
                    if manual {
                        host.start(&session).await
                    } else {
                        host.begin_push(&session, ReviewScope::WorkingDirectory)
                            .await
                            .map(|_| ())
                    }
                })
            };
            let (_, _, reply) = manager
                .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
                .await;
            reply.send(Err(anyhow::anyhow!(reason))).unwrap();
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

// Hard-won: #1502: a completed clean review was discarded while recording waited.
#[tokio::test(start_paused = true)]
async fn push_recording_waits_past_ten_seconds_without_losing_clean_verdict() {
    let session = session_id("recording-queue");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    let (_, action, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    let mut order = mj_session_runtime::session_manager::SessionRequestOrder::new();
    let (release, released) = oneshot::channel();
    let (prior_reply, _prior_result) = oneshot::channel();
    order.dispatch(
        RemoteSessionRequest::Reviewer {
            session_id: session.clone(),
            role: None,
            action: ReviewerAction::CaptureDelta { repositories: None },
            reply: prior_reply,
        },
        |_| async move {
            released.await.unwrap();
        },
    );
    let (sent, received) = oneshot::channel();
    order.dispatch(
        RemoteSessionRequest::Reviewer {
            session_id: session.clone(),
            role: None,
            action,
            reply,
        },
        |request| async move {
            let RemoteSessionRequest::Reviewer { action, reply, .. } = request else {
                unreachable!()
            };
            sent.send(reply.send(answer_for(&action)).is_ok()).unwrap();
        },
    );
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert!(environment.state().active_group().is_some());
    assert!(environment.mailbox.lock().unwrap().is_empty());
    release.send(()).unwrap();
    assert!(
        received.await.unwrap(),
        "role queue time must not expire the recording caller"
    );
    order.drain().await.unwrap();
    let notices = recording_notices_until_closed(&mut manager, &host, &session).await;
    assert!(
        notices
            .iter()
            .any(|text| text == "Review complete: no material findings")
    );
    host.shutdown().await.unwrap();
}

// Hard-won: #1503: a 90-second deadline over all of review preparation cut off slow worker waits and reported them as "review preparation cancelled".
#[tokio::test(start_paused = true)]
async fn slow_review_preparation_waits_until_cancelled_and_names_the_stopped_step() {
    let session = session_id("slow-preparation");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .resolve_hangs
        .store(true, std::sync::atomic::Ordering::Release);
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
    let request = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        })
    };
    let (_, _, capture) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::CaptureDelta { .. }))
        .await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(
        !request.is_finished(),
        "a slow worker capture is waited for"
    );
    capture
        .send(Ok(ReviewerOutcome::Delta {
            repositories: vec![push_delta("/workspace/app", "newtree")],
        }))
        .unwrap();
    // Reviewer choice now waits forever, as an Auto choice can on a real host.
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(
        !request.is_finished(),
        "a slow reviewer choice is waited for"
    );
    assert_eq!(environment.live_holds(), 1);

    host.resolve(&session, Resolution::Cancelled).await.unwrap();
    let refusal = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("cancelling stops the running step at once")
        .unwrap()
        .unwrap_err()
        .0;
    assert!(
        refusal.starts_with("review preparation cancelled while choosing reviewers"),
        "{refusal}"
    );
    assert_eq!(
        environment.live_holds(),
        0,
        "the background hold is released"
    );
    assert!(
        host.view(&session).is_none(),
        "the session can request review again"
    );
    host.shutdown().await.unwrap();
}

async fn recording_notices_until_closed(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    session: &str,
) -> Vec<String> {
    let mut notices = Vec::new();
    loop {
        match manager.next().await {
            RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } => {
                let terminal = text.starts_with("Review complete:")
                    || text.starts_with("Review failed:")
                    || text.starts_with("Review cancelled");
                notices.push(text);
                let _ = reply.send(Ok(1));
                if terminal {
                    break;
                }
            }
            other => panic!("unexpected request for {}", other.session_id()),
        }
    }
    while host.view(session).is_some() {
        tokio::task::yield_now().await;
    }
    notices
}

// Hard-won: #1502: recording failure emitted a clean completion and erased its resumable snapshot.
#[tokio::test]
async fn push_recording_failure_stays_pending_then_replays_the_same_trees() {
    for restart in [false, true] {
        let session = session_id("recording-retry");
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        let mut host =
            TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
        let repository = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = mj_core::subprocess::run_capturing_stdout(
                std::process::Command::new("git")
                    .current_dir(repository.path())
                    .args(args),
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "reviewed",
        ]);
        let commit = git(&["rev-parse", "HEAD"]);
        let tree = git(&["rev-parse", "HEAD^{tree}"]);
        let mut delta = push_delta(repository.path().to_str().unwrap(), &tree);
        delta.head_commit = Some(commit.clone());
        delta.patch = "diff --git a/a b/a\n@@\n+one\n".into();
        let child =
            open_commit_review_with_delta(&mut manager, &host, &environment, true, Some(delta))
                .await;
        finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
        let (_, first, reply) = manager
            .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
            .await;
        let first_trees = match first {
            ReviewerAction::RecordReviewed { trees } => trees,
            _ => unreachable!(),
        };
        for (root, (tree, commit)) in &first_trees {
            mj_checkpoint::archive::record_reviewed_tree(
                &mj_checkpoint::archive::SystemGit,
                root,
                tree,
                commit,
            )
            .unwrap();
        }
        // The worker has applied these refs: only its acknowledgement was lost.
        reply
            .send(Err(anyhow::anyhow!("connection lost after recording")))
            .unwrap();
        match manager.next().await {
            RemoteSessionRequest::Submit {
                command: RelayCommand::RecordNotice { text },
                reply,
                ..
            } => {
                assert!(text.contains("approval recording failed"));
                assert!(!text.contains("Review complete"));
                reply.send(Ok(1)).unwrap();
            }
            _ => panic!("expected recording failure notice"),
        }
        assert!(environment.mailbox.lock().unwrap().is_empty());
        let snapshot = environment.state().active_group().unwrap().driver;
        assert_eq!(snapshot.last_verdict, Some(ReviewVerdict::Clean));
        if restart {
            host.shutdown().await.unwrap();
            host = TurnReviewHost::spawn_in(
                manager.control.clone(),
                push_config(),
                environment.clone(),
            );
        }
        let (_, second, reply) = manager
            .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
            .await;
        match &second {
            ReviewerAction::RecordReviewed { trees } => assert_eq!(trees, &first_trees),
            _ => unreachable!(),
        }
        assert_eq!(
            environment.children.lock().unwrap().len(),
            1,
            "reviewers are not rerun"
        );
        for (root, (tree, commit)) in &first_trees {
            mj_checkpoint::archive::record_reviewed_tree(
                &mj_checkpoint::archive::SystemGit,
                root,
                tree,
                commit,
            )
            .unwrap();
        }
        assert_eq!(
            git(&["rev-parse", &format!("refs/mj/reviewed/{tree}")]),
            commit
        );
        reply.send(answer_for(&second)).unwrap();
        let notices = recording_notices_until_closed(&mut manager, &host, &session).await;
        assert!(
            notices
                .iter()
                .any(|text| text.starts_with("Review complete:"))
        );
        assert!(environment.state().active_group().is_none());
        assert_eq!(environment.mailbox.lock().unwrap().len(), 1);
        host.shutdown().await.unwrap();
    }
}

// Hard-won: #1502: the completion notice used the stale clean driver verdict on recording refusal.
#[tokio::test]
async fn push_recording_permanent_refusal_delivers_failure_without_retrying() {
    let session = session_id("recording-terminal");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    let (_, _, reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    reply
        .send(Err(mj_session_runtime::worker_client::RelayRejected(
            mj_core::relay::RelayProtocolError {
                code: mj_core::relay::RelayErrorCode::InvalidState,
                message: "repository is gone".into(),
                detail: None,
                retryable: false,
            },
        )
        .into()))
        .unwrap();
    let notices = recording_notices_until_closed(&mut manager, &host, &session).await;
    assert!(
        notices
            .iter()
            .any(|text| text.contains("approval recording failed")
                && text.contains("repository is gone"))
    );
    assert!(
        !notices
            .iter()
            .any(|text| text.starts_with("Review complete:"))
    );
    assert!(environment.state().active_group().is_none());
    let mailbox = environment.mailbox.lock().unwrap().clone();
    assert!(
        mj_core::mailbox::render_mailbox_event(&mailbox[0]).contains("approval recording failed")
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn push_recording_can_be_cancelled_while_awaiting_acknowledgement() {
    let session = session_id("recording-cancel");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    let (_, _, mut reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    host.resolve(&session, Resolution::Cancelled).await.unwrap();
    reply.closed().await;
    let notices = recording_notices_until_closed(&mut manager, &host, &session).await;
    assert!(notices.iter().any(
        |text| text.contains("stopped waiting for approval recording")
            && text.contains("worker may already have recorded approval")
            && text.contains("push is allowed")
            && text.contains("request review again")
    ));
    assert!(environment.mailbox.lock().unwrap().is_empty());
    assert!(environment.state().active_group().is_none());
    host.shutdown().await.unwrap();
}

// Hard-won: #1502: a recoverable worker wait held admission and prevented daemon shutdown from starting.
#[tokio::test]
async fn push_recording_does_not_block_daemon_handoff() {
    const PROBE: &str = "MJ_TEST_REVIEW_RECORDING_HANDOFF";
    // Closing the process-wide gate belongs in a separate test process.
    if std::env::var_os(PROBE).is_none() {
        mj_core::runtime::spawn_blocking(|| {
            let root = tempfile::tempdir().unwrap();
            mj_targets::test_isolation::IsolatedTest::new(
                "review_host::tests::push_recording_does_not_block_daemon_handoff",
            )
            .env(PROBE, "1")
            .isolated_store(root.path())
            .run();
        })
        .await
        .unwrap();
        return;
    }
    let session = session_id("recording-handoff");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let child = open_push(&mut manager, &host, &environment).await;
    finish_child(&host, &environment, &session, &child, Some("No findings.")).await;
    let (_, _, mut reply) = manager
        .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
        .await;
    assert!(
        mj_store::upgrade::gate().try_close(),
        "a worker recording wait must not hold daemon handoff admission: {:?}",
        mj_store::upgrade::gate().active_labels()
    );
    tokio::time::timeout(Duration::from_secs(5), host.shutdown())
        .await
        .expect("shutdown cancels the recoverable wait")
        .unwrap();
    reply.closed().await;
    assert_eq!(
        environment
            .state()
            .active_group()
            .unwrap()
            .driver
            .last_verdict,
        Some(ReviewVerdict::Clean)
    );
    assert!(environment.mailbox.lock().unwrap().is_empty());
}

// Hard-won: #1502: baseline and override requests held daemon admission across worker queues.
#[tokio::test(start_paused = true)]
async fn push_state_changes_do_not_block_daemon_handoff() {
    const PROBE: &str = "MJ_TEST_PUSH_STATE_HANDOFF";
    let Some(kind) = std::env::var_os(PROBE) else {
        mj_core::runtime::spawn_blocking(|| {
            for kind in ["baseline", "override"] {
                let root = tempfile::tempdir().unwrap();
                mj_targets::test_isolation::IsolatedTest::new(
                    "review_host::tests::push_state_changes_do_not_block_daemon_handoff",
                )
                .env(PROBE, kind)
                .isolated_store(root.path())
                .run();
            }
        })
        .await
        .unwrap();
        return;
    };
    let session = session_id("state-handoff");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    let baseline = kind == "baseline";
    let request = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            if baseline {
                host.set_push_base(&session, "abc1234".into(), None).await
            } else {
                host.override_push(&session, None, one_override("incorrect"))
                    .await
            }
        })
    };
    if !baseline {
        answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    }
    let (_, action, reply) = manager
        .next_reviewer(|_, action| {
            matches!(
                action,
                ReviewerAction::SetReviewBase { .. } | ReviewerAction::RecordOverridden { .. }
            )
        })
        .await;
    let mut order = mj_session_runtime::session_manager::SessionRequestOrder::new();
    let (release, released) = oneshot::channel();
    let (prior_reply, _prior_result) = oneshot::channel();
    order.dispatch(
        RemoteSessionRequest::Reviewer {
            session_id: session.clone(),
            role: None,
            action: ReviewerAction::CaptureDelta { repositories: None },
            reply: prior_reply,
        },
        |_| async move {
            released.await.unwrap();
        },
    );
    order.dispatch(
        RemoteSessionRequest::Reviewer {
            session_id: session.clone(),
            role: None,
            action,
            reply,
        },
        |request| async move {
            let RemoteSessionRequest::Reviewer { mut reply, .. } = request else {
                unreachable!()
            };
            reply.closed().await;
        },
    );
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert!(
        !request.is_finished(),
        "role queue time must not expire the request"
    );
    assert!(
        mj_store::upgrade::gate().try_close(),
        "worker queue holds admission: {:?}",
        mj_store::upgrade::gate().active_labels()
    );
    tokio::time::timeout(Duration::from_secs(5), host.shutdown())
        .await
        .unwrap()
        .unwrap();
    let error = request.await.unwrap().unwrap_err().0;
    assert!(
        error.contains("repeat the request after restart"),
        "{error}"
    );
    release.send(()).unwrap();
    order.drain().await.unwrap();
    assert!(environment.state().findings().is_some());
}

// Hard-won: Reliability soak 2026-11-16: a review-off start sent its capture into a recovery copy's checkpoint barrier and the worker refused it.
#[tokio::test(start_paused = true)]
async fn a_review_off_request_waits_for_background_work_before_reaching_the_worker() {
    let session = session_id("reviewoff-background");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .background_hangs
        .store(true, std::sync::atomic::Ordering::Release);
    let off: ReviewConfigSource = Arc::new(|_| ReviewConfig::default());
    let host = TurnReviewHost::spawn_in(manager.control.clone(), off, environment.clone());
    manager
        .publisher
        .publish(
            session.clone(),
            view(
                &session,
                MaterializedExecutionState::Running { started_at_ms: 1 },
            ),
        )
        .await
        .unwrap();
    let request = mj_core::runtime::spawn({
        let host = host.clone();
        let session = session.clone();
        async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        }
    });
    // A copy that never ends: the request must be waiting on the hold, having
    // sent the worker nothing.
    let request_to_worker = tokio::time::timeout(
        super::reviewer::BACKGROUND_WORK_WAIT / 2,
        manager.requests.recv(),
    )
    .await;
    assert!(
        request_to_worker.is_err(),
        "the worker was sent work under running background work"
    );
    assert_eq!(environment.background_holds(), 1);
    tokio::time::advance(super::reviewer::BACKGROUND_WORK_WAIT).await;
    let refusal = request.await.unwrap().unwrap_err().0;
    assert!(
        refusal.contains("waiting for background work") && refusal.contains("deadline"),
        "{refusal}"
    );
    assert_eq!(environment.live_holds(), 0);
    host.shutdown().await.unwrap();
}

// Hard-won: 8481d8a51: review admission ignored its existing background-work deadline.
#[tokio::test(start_paused = true)]
async fn review_admission_deadline_reports_refusal_and_releases_its_hold() {
    let session = session_id("admission-deadline");
    let manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment
        .background_hangs
        .store(true, std::sync::atomic::Ordering::Release);
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
    let request = mj_core::runtime::spawn({
        let host = host.clone();
        let session = session.clone();
        async move {
            host.begin_push(&session, ReviewScope::WorkingDirectory)
                .await
        }
    });
    while environment.background_holds() == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(super::reviewer::BACKGROUND_WORK_WAIT + Duration::from_secs(1)).await;
    let refusal = request.await.unwrap().unwrap_err().0;
    assert!(
        refusal.contains("waiting for background work") && refusal.contains("deadline"),
        "{refusal}"
    );
    assert_eq!(environment.live_holds(), 0);
    assert!(
        host.view(&session).is_none(),
        "a refused admission leaves review available to retry"
    );
    host.shutdown().await.unwrap();
}

// ---- Several reviews in one session -------------------------------------

async fn publish_running(manager: &FakeManager) {
    manager
        .publisher
        .publish(
            manager.session.clone(),
            view(
                &manager.session,
                MaterializedExecutionState::Running { started_at_ms: 1 },
            ),
        )
        .await
        .unwrap();
}

/// Asks for a review of exactly these worktrees.
fn begin_scoped(
    host: &TurnReviewHost,
    session: &str,
    paths: &[&str],
) -> tokio::task::JoinHandle<Result<serde_json::Value, StartRefusal>> {
    let host = host.clone();
    let session = session.to_owned();
    let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    mj_core::runtime::spawn(
        async move { host.begin_push(&session, ReviewScope::Paths(paths)).await },
    )
}

/// The next scoped capture the worker is asked for, with the paths it names.
async fn next_scoped_capture(
    manager: &mut FakeManager,
) -> (
    Vec<PathBuf>,
    oneshot::Sender<anyhow::Result<ReviewerOutcome>>,
) {
    let (_, action, reply) = manager
        .next_reviewer(|_, action| {
            matches!(
                action,
                ReviewerAction::CaptureDelta {
                    repositories: Some(_)
                }
            )
        })
        .await;
    let ReviewerAction::CaptureDelta {
        repositories: Some(paths),
    } = action
    else {
        unreachable!("the wanted action is a scoped capture")
    };
    (paths, reply)
}

fn delta_reply(roots: &[&str], tree: &str) -> anyhow::Result<ReviewerOutcome> {
    Ok(ReviewerOutcome::Delta {
        repositories: roots.iter().map(|root| push_delta(root, tree)).collect(),
    })
}

/// Opens a review of one worktree and waits until its caller is told.
async fn open_scoped_review(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    session: &str,
    root: &str,
    tree: &str,
) {
    let request = begin_scoped(host, session, &[root]);
    let (paths, reply) = next_scoped_capture(manager).await;
    assert_eq!(paths, vec![PathBuf::from(root)]);
    reply.send(delta_reply(&[root], tree)).unwrap();
    assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
}

/// The reviewer child of each open review, by the first root it covers, once
/// every review has its child running.
async fn children_by_root(
    environment: &FakeEnvironment,
    reviews: usize,
) -> std::collections::BTreeMap<PathBuf, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = environment.state();
            let children: std::collections::BTreeMap<_, _> = state
                .open_reviews
                .iter()
                .filter_map(|review| {
                    let group = review.active_group.as_ref()?;
                    let child = group.registrations.first()?.child_session_id.clone()?;
                    let root = group.driver.deltas.first()?.root.clone();
                    environment
                        .started
                        .lock()
                        .unwrap()
                        .contains(&child)
                        .then_some((root, child))
                })
                .collect();
            if children.len() == reviews {
                return children;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every review has a running reviewer")
}

/// A reviewer hands back its report; a clean one makes the worker record the
/// approved tree. Waits until the review has left the host.
async fn settle_review(
    manager: &mut FakeManager,
    host: &TurnReviewHost,
    environment: &FakeEnvironment,
    session: &str,
    child: &str,
    report: &str,
) -> Vec<PathBuf> {
    let open_before = environment.state().open_reviews.len();
    let finished = {
        let host = host.clone();
        let (session, child, report) = (session.to_owned(), child.to_owned(), report.to_owned());
        mj_core::runtime::spawn(async move {
            host.reviewer_finished(session, child, Some(report)).await
        })
    };
    let mut recorded = Vec::new();
    if report == "No findings." {
        let (_, action, reply) = manager
            .next_reviewer(|_, action| matches!(action, ReviewerAction::RecordReviewed { .. }))
            .await;
        let ReviewerAction::RecordReviewed { trees } = &action else {
            unreachable!("the wanted action records a reviewed tree")
        };
        recorded = trees.keys().cloned().collect();
        reply.send(answer_for(&action)).unwrap();
    }
    finished.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while environment.state().open_reviews.len() >= open_before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the settled review leaves the stored row");
    recorded
}

/// Waits until the session shows this many reviews.
async fn wait_for_views(host: &TurnReviewHost, session: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.views_of(session).len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the session shows {count} reviews"));
}

#[tokio::test]
async fn reviews_of_disjoint_roots_run_together_and_each_delivers_its_own_verdict() {
    let session = session_id("disjoint-roots");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    publish_running(&manager).await;
    open_scoped_review(&mut manager, &host, &session, "/workspace/a", "tree-a").await;
    open_scoped_review(&mut manager, &host, &session, "/workspace/c", "tree-c").await;
    assert_eq!(environment.state().open_reviews.len(), 2);
    let views = host.views_of(&session);
    assert_eq!(views.len(), 2);
    let mut roots: Vec<_> = views.iter().map(|view| view.roots.clone()).collect();
    roots.sort();
    assert_eq!(roots, [["/workspace/a"], ["/workspace/c"]]);
    assert!(views.iter().all(|view| !view.review_id.is_empty()));
    assert_ne!(views[0].review_id, views[1].review_id);

    // A request that names a root an open review owns is refused, and the
    // refusal offers the roots that are free.
    let partly = begin_scoped(&host, &session, &["/workspace/a", "/workspace/b"]);
    let (_, reply) = next_scoped_capture(&mut manager).await;
    reply
        .send(delta_reply(&["/workspace/a", "/workspace/b"], "tree-ab"))
        .unwrap();
    assert_eq!(
        partly.await.unwrap().unwrap_err().0,
        "/workspace/a is already in the open review of /workspace/a. Request a review of /workspace/b now, or wait for that verdict and then request /workspace/a, /workspace/b together."
    );
    let wholly = begin_scoped(&host, &session, &["/workspace/a", "/workspace/c"]);
    let (_, reply) = next_scoped_capture(&mut manager).await;
    reply
        .send(delta_reply(&["/workspace/a", "/workspace/c"], "tree-ac"))
        .unwrap();
    assert_eq!(
        wholly.await.unwrap().unwrap_err().0,
        "/workspace/a is already in the open review of /workspace/a. Wait for that verdict and then request /workspace/a, /workspace/c together."
    );
    assert_eq!(environment.state().open_reviews.len(), 2);

    let children = children_by_root(&environment, 2).await;
    let recorded = settle_review(
        &mut manager,
        &host,
        &environment,
        &session,
        &children[&PathBuf::from("/workspace/a")],
        "No findings.",
    )
    .await;
    assert_eq!(recorded, vec![PathBuf::from("/workspace/a")]);
    wait_for_views(&host, &session, 1).await;
    settle_review(
        &mut manager,
        &host,
        &environment,
        &session,
        &children[&PathBuf::from("/workspace/c")],
        "A defect needs a fix",
    )
    .await;

    let mailbox = environment.mailbox.lock().unwrap().clone();
    assert_eq!(mailbox.len(), 2, "one verdict message per review");
    for root in ["/workspace/a", "/workspace/c"] {
        assert!(
            mailbox.iter().any(|event| event.key.ends_with(root)),
            "a verdict names {root}"
        );
    }
    let state = environment.state();
    assert!(state.open_reviews.is_empty());
    assert_eq!(state.push_findings.len(), 1, "only the findings verdict");
    assert!(state.push_findings[0].covers(std::path::Path::new("/workspace/c")));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn overlapping_requests_that_race_are_decided_when_they_open() {
    for first in ["a-b", "b-c"] {
        let session = session_id(&format!("race-{first}"));
        let mut manager = FakeManager::new(&session).await;
        let environment = FakeEnvironment::new();
        let host =
            TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
        publish_running(&manager).await;
        let ab = begin_scoped(&host, &session, &["/workspace/a", "/workspace/b"]);
        let bc = begin_scoped(&host, &session, &["/workspace/b", "/workspace/c"]);
        let mut captures = Vec::new();
        for _ in 0..2 {
            captures.push(next_scoped_capture(&mut manager).await);
        }
        let position = |wanted: &str| {
            captures
                .iter()
                .position(|(paths, _)| paths.contains(&PathBuf::from(wanted)))
                .unwrap()
        };
        let (ab_capture, bc_capture) = (position("/workspace/a"), position("/workspace/c"));
        let mut replies: Vec<_> = captures.into_iter().map(|(_, reply)| Some(reply)).collect();
        let ab_reply = replies[ab_capture].take().unwrap();
        let bc_reply = replies[bc_capture].take().unwrap();
        // Whichever preparation finishes first opens; the other is refused.
        let (opened, refused, expected) = if first == "a-b" {
            ab_reply
                .send(delta_reply(&["/workspace/a", "/workspace/b"], "tree-ab"))
                .unwrap();
            assert_eq!(ab.await.unwrap().unwrap()["accepted"], true);
            bc_reply
                .send(delta_reply(&["/workspace/b", "/workspace/c"], "tree-bc"))
                .unwrap();
            (
                "a",
                bc,
                "/workspace/b is already in the open review of /workspace/a, /workspace/b. Request a review of /workspace/c now, or wait for that verdict and then request /workspace/b, /workspace/c together.",
            )
        } else {
            bc_reply
                .send(delta_reply(&["/workspace/b", "/workspace/c"], "tree-bc"))
                .unwrap();
            assert_eq!(bc.await.unwrap().unwrap()["accepted"], true);
            ab_reply
                .send(delta_reply(&["/workspace/a", "/workspace/b"], "tree-ab"))
                .unwrap();
            (
                "c",
                ab,
                "/workspace/b is already in the open review of /workspace/b, /workspace/c. Request a review of /workspace/a now, or wait for that verdict and then request /workspace/a, /workspace/b together.",
            )
        };
        assert_eq!(refused.await.unwrap().unwrap_err().0, expected);
        let state = environment.state();
        assert_eq!(state.open_reviews.len(), 1, "exactly one opened");
        let roots = state.open_reviews[0].roots();
        assert!(
            roots.contains(&PathBuf::from(format!("/workspace/{opened}"))),
            "{roots:?}"
        );
        host.shutdown().await.unwrap();
    }
}

// A capture made before another review of its root closed may be older than a
// base that review moved, so it is captured again instead of opened.
#[tokio::test]
async fn a_capture_older_than_a_closed_review_of_its_root_is_taken_again() {
    let session = session_id("stale-capture");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    publish_running(&manager).await;
    let stale = begin_scoped(&host, &session, &["/workspace/a"]);
    let (_, stale_reply) = next_scoped_capture(&mut manager).await;
    open_scoped_review(&mut manager, &host, &session, "/workspace/a", "tree-1").await;
    let children = children_by_root(&environment, 1).await;
    settle_review(
        &mut manager,
        &host,
        &environment,
        &session,
        &children[&PathBuf::from("/workspace/a")],
        "No findings.",
    )
    .await;
    // Only the stale request's own placeholder is left.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !host
            .views_of(&session)
            .iter()
            .all(|view| view.status.starts_with("Preparing"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the closed review leaves the host");
    stale_reply
        .send(delta_reply(&["/workspace/a"], "tree-old"))
        .unwrap();
    let (_, again) = next_scoped_capture(&mut manager).await;
    again
        .send(delta_reply(&["/workspace/a"], "tree-new"))
        .unwrap();
    assert_eq!(stale.await.unwrap().unwrap()["accepted"], true);
    let state = environment.state();
    let deltas = &state.open_reviews[0]
        .active_group
        .as_ref()
        .unwrap()
        .driver
        .deltas;
    assert_eq!(deltas[0].current_tree, "tree-new");
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_open_reviews_survive_a_host_restart_and_each_settles_on_its_own() {
    let session = session_id("restart-two");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    publish_running(&manager).await;
    open_scoped_review(&mut manager, &host, &session, "/workspace/a", "tree-a").await;
    open_scoped_review(&mut manager, &host, &session, "/workspace/c", "tree-c").await;
    children_by_root(&environment, 2).await;
    host.shutdown().await.unwrap();
    assert_eq!(
        environment.state().open_reviews.len(),
        2,
        "both reviews are left stored"
    );

    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.views_of(&session).len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both reviews are restored");
    let children = children_by_root(&environment, 2).await;
    settle_review(
        &mut manager,
        &host,
        &environment,
        &session,
        &children[&PathBuf::from("/workspace/c")],
        "No findings.",
    )
    .await;
    wait_for_views(&host, &session, 1).await;
    assert_eq!(environment.state().open_reviews.len(), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn override_review_selects_the_verdict_by_path_and_keeps_the_others() {
    let session = session_id("override-by-path");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    pending_push_findings(&environment);
    environment
        .state
        .lock()
        .unwrap()
        .push_findings
        .push(mj_core::storage::PushFindingsRecord {
            review_id: "review-2".into(),
            trees: [(
                PathBuf::from("/workspace/other"),
                ("othertree".into(), "ccccccc789".into()),
            )]
            .into(),
            synthesis: "another finding".into(),
        });
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    // With two verdicts outstanding, no path cannot choose.
    let error = host
        .override_push(&session, None, one_override("incorrect"))
        .await
        .unwrap_err();
    assert!(
        error.0.contains("more than one findings verdict") && error.0.contains("pass the path"),
        "{}",
        error.0
    );
    let request = {
        let host = host.clone();
        let session = session.clone();
        mj_core::runtime::spawn(async move {
            host.override_push(
                &session,
                Some("/workspace/app".into()),
                one_override("incorrect"),
            )
            .await
        })
    };
    answer_override_capture(&mut manager, vec![push_delta("/workspace/app", "newtree")]).await;
    finish_override_worker_actions(&mut manager, "incorrect").await;
    assert_eq!(request.await.unwrap().unwrap()["accepted"], true);
    let state = environment.state();
    assert_eq!(state.push_findings.len(), 1);
    assert_eq!(state.push_findings[0].review_id, "review-2");
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_verdict_for_one_root_leaves_the_findings_of_the_other_roots_of_an_older_record() {
    let session = session_id("shrink-findings");
    let mut manager = FakeManager::new(&session).await;
    let environment = FakeEnvironment::new();
    environment.state.lock().unwrap().push_findings = vec![mj_core::storage::PushFindingsRecord {
        review_id: "older".into(),
        trees: ["/workspace/a", "/workspace/b"]
            .map(|root| (PathBuf::from(root), ("tree".into(), "head".into())))
            .into(),
        synthesis: "older findings".into(),
    }];
    let host =
        TurnReviewHost::spawn_in(manager.control.clone(), push_config(), environment.clone());
    publish_running(&manager).await;
    open_scoped_review(&mut manager, &host, &session, "/workspace/a", "tree-a").await;
    let children = children_by_root(&environment, 1).await;
    settle_review(
        &mut manager,
        &host,
        &environment,
        &session,
        &children[&PathBuf::from("/workspace/a")],
        "No findings.",
    )
    .await;
    let state = environment.state();
    assert_eq!(state.push_findings.len(), 1);
    assert!(!state.push_findings[0].covers(std::path::Path::new("/workspace/a")));
    assert!(state.push_findings[0].covers(std::path::Path::new("/workspace/b")));
    host.shutdown().await.unwrap();
}
