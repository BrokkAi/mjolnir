use super::*;

/// A handle on the review host. Cheap to clone; every method is a message.
#[derive(Clone)]
pub struct TurnReviewHost {
    pub events: mpsc::UnboundedSender<HostEvent>,
    pub shared: Arc<HostShared>,
}

/// What surfaces read without waiting for the host's task.
pub struct HostShared {
    pub views: Mutex<BTreeMap<String, RuntimeReviewView>>,
    pub changed: Arc<dyn Fn() + Send + Sync>,
    pub shutdown: tokio::sync::OnceCell<Result<(), String>>,
    /// Set when the daemon asks the host to stop, before the host's task
    /// ends. A host gone without it set stopped on its own.
    pub stopping: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for TurnReviewHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TurnReviewHost")
    }
}

impl TurnReviewHost {
    /// The same, against a caller-supplied environment. `config` is read at
    /// each trigger decision.
    #[must_use]
    pub fn spawn_in(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        environment: Arc<dyn ReviewEnvironment>,
    ) -> Self {
        Self::spawn_in_notifying(control, config, environment, Arc::new(|| {}))
    }

    #[must_use]
    pub fn spawn_in_notifying(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        environment: Arc<dyn ReviewEnvironment>,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        let (persistence, persistence_receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(HostShared {
            views: Mutex::default(),
            changed,
            shutdown: tokio::sync::OnceCell::new(),
            stopping: std::sync::atomic::AtomicBool::new(false),
        });
        let host = Self {
            events: events.clone(),
            shared: shared.clone(),
        };
        let persistence_task = mj_core::runtime::spawn(persistence_loop(
            environment.clone(),
            events.clone(),
            persistence_receiver,
        ));
        // The restart sweep is the first operation in the same FIFO lane that
        // records new active reviews, so it cannot clear a review that opened
        // while the sweep was still running.
        persistence
            .send(PersistenceRequest::SweepInterrupted)
            .expect("new review persistence lane accepts its initial sweep");
        mj_core::runtime::spawn(host_loop(
            HostState {
                control,
                config,
                environment,
                shared,
                events,
                persistence: Some(persistence),
                persistence_task: Some(persistence_task),
                child_closures: tokio::task::JoinSet::new(),
                child_closure_owners: BTreeMap::new(),
                sweep_queue: std::collections::VecDeque::new(),
                reviews: BTreeMap::new(),
                preparing: BTreeMap::new(),
                pending_open: BTreeMap::new(),
                closing: BTreeSet::new(),
                awaiting_capture_persistence: BTreeMap::new(),
                next_epoch: 0,
                sessions: BTreeMap::new(),
                startup_ready: false,
                startup_failure: None,
                deferred_handbacks: Vec::new(),
                deferred_admissions: Vec::new(),
            },
            receiver,
        ));
        host
    }

    /// Reports one session's latest view. This is the trigger's only input.
    /// Prune the retained last-views to the live session set. The daemon calls
    /// this from its reconcile so a stopped or destroyed session's transcript is
    /// released rather than retained in `sessions` forever.
    pub fn retain_sessions(&self, live: std::collections::BTreeSet<String>) {
        let _ = self.events.send(HostEvent::Retain { live });
    }

    pub fn observe(&self, session_id: &str, view: &ManagedSessionView) {
        // Retain the latest reviewer questions without blocking the daemon's
        // update loop.
        let _ = self.events.send(HostEvent::View {
            session_id: session_id.to_owned(),
            snapshot: view
                .snapshot
                .as_ref()
                .map(|snapshot| Box::new(snapshot.materialized.clone())),
        });
    }

    pub async fn begin_push(
        &self,
        session_id: &str,
        path: Option<String>,
    ) -> Result<serde_json::Value, StartRefusal> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::StartPush {
                session_id: session_id.to_owned(),
                path,
                reply,
            })
            .map_err(|_| StartRefusal("the review host stopped".into()))?;
        answer
            .await
            .map_err(|_| StartRefusal("the review host stopped".into()))?
    }

    pub async fn override_push(
        &self,
        session_id: &str,
        overrides: Vec<ReviewOverride>,
    ) -> Result<serde_json::Value, StartRefusal> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::OverridePush {
                session_id: session_id.to_owned(),
                overrides,
                reply,
            })
            .map_err(|_| StartRefusal("the review host stopped".into()))?;
        answer
            .await
            .map_err(|_| StartRefusal("the review host stopped".into()))?
    }

    pub async fn set_push_base(
        &self,
        session_id: &str,
        commit: String,
        path: Option<String>,
    ) -> Result<serde_json::Value, StartRefusal> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::SetPushBase {
                session_id: session_id.to_owned(),
                commit,
                path,
                reply,
            })
            .map_err(|_| StartRefusal("the review host stopped".into()))?;
        answer
            .await
            .map_err(|_| StartRefusal("the review host stopped".into()))?
    }

    /// Reviews committed work through the request_review gate.
    pub async fn start(&self, session_id: &str) -> Result<(), StartRefusal> {
        self.begin_push(session_id, None).await.map(|_| ())
    }

    /// Whether `session_id` has a review open. The host's own slots answer, so
    /// callers never infer this from a copy of the review views. A session
    /// with an open review is waiting for its verdict.
    pub async fn has_open_review(&self, session_id: &str) -> Result<bool, OpenReviewUnknown> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::HasOpenReview {
                session_id: session_id.to_owned(),
                reply,
            })
            .map_err(|_| self.stopped())?;
        answer.await.map_err(|_| self.stopped())?
    }

    /// Why the host's task no longer answers.
    fn stopped(&self) -> OpenReviewUnknown {
        if self
            .shared
            .stopping
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            OpenReviewUnknown::ShuttingDown
        } else {
            OpenReviewUnknown::HostStopped
        }
    }

    /// Puts the host in the state a failed startup recovery leaves it in.
    // For tests. Compiled always, so that mj-daemon's tests reach it without a
    // Cargo feature that would give this crate a second build.
    #[doc(hidden)]
    pub fn fail_startup_recovery(&self, error: &str) {
        let _ = self
            .events
            .send(HostEvent::StartupRecoveryFailed(error.to_owned()));
    }

    /// Hands a finished reviewer child to the host's closer. The host retries
    /// the close with backoff until it succeeds; callers must not close the
    /// child again themselves.
    pub fn close_child(&self, child_session_id: String) {
        let _ = self.events.send(HostEvent::CloseChild { child_session_id });
    }

    /// Reports that a reviewer child's session ended (stopped, destroyed or
    /// failed). The daemon's record owner calls this once per such transition.
    /// A role that already has an outcome, and a child no open group tracks,
    /// are left alone.
    pub fn child_ended(&self, child_session_id: String, reason: String) {
        let _ = self.events.send(HostEvent::ChildEnded {
            child_session_id,
            reason,
        });
    }

    pub async fn reviewer_finished(
        &self,
        session_id: String,
        child_session_id: String,
        answer: Option<String>,
    ) -> Result<(), String> {
        let report = match answer {
            Some(answer) => ReviewerReport::Answer(answer),
            None => ReviewerReport::Failed("the reviewer finished without calling handback".into()),
        };
        self.report_reviewer(session_id, child_session_id, report)
            .await
    }

    /// Reports that a reviewer's turn ended with no handback and says how the
    /// turn ended. A turn that was interrupted or rejected is not a reviewer
    /// that forgot to hand back, so the caller words the reason from the turn.
    pub async fn reviewer_ended_without_handback(
        &self,
        session_id: String,
        child_session_id: String,
        reason: String,
    ) -> Result<(), String> {
        self.report_reviewer(session_id, child_session_id, ReviewerReport::Failed(reason))
            .await
    }

    async fn report_reviewer(
        &self,
        session_id: String,
        child_session_id: String,
        report: ReviewerReport,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.events
            .send(HostEvent::ReviewerFinished {
                session_id,
                child_session_id,
                report,
                reply,
            })
            .map_err(|_| "review host stopped".to_owned())?;
        result.await.map_err(|_| "review host stopped".to_owned())?
    }

    /// Handles a legacy serialized resolution request. Only before-push
    /// cancellation remains valid; ordinary turn reviews resolve themselves.
    pub async fn resolve(&self, session_id: &str, resolution: Resolution) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::Resolve {
                session_id: session_id.to_owned(),
                resolution,
                reply,
            })
            .map_err(|_| "the review host stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "the review host stopped".to_owned())?
    }

    /// Stops accepting review work and drains the
    /// ordered persistence lane before the daemon shuts its database writer
    /// down.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.shared
            .stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.shared
            .shutdown
            .get_or_init(|| async {
                let (reply, answer) = oneshot::channel();
                self.events
                    .send(HostEvent::Shutdown { reply })
                    .map_err(|_| "the review host stopped".to_owned())?;
                answer
                    .await
                    .map_err(|_| "the review host stopped during shutdown".to_owned())?
            })
            .await
            .clone()
    }

    /// Every open review, for a snapshot a surface renders.
    #[must_use]
    pub fn views(&self) -> Vec<RuntimeReviewView> {
        self.shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// One session's review, if it has one.
    #[must_use]
    pub fn view(&self, session_id: &str) -> Option<RuntimeReviewView> {
        self.shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    pub async fn question_target(
        &self,
        session_id: &str,
        elicitation_id: &str,
    ) -> Option<(String, String)> {
        let (child, question) = mj_client::review::parse_reviewer_question_id(elicitation_id)?;
        let (reply, result) = oneshot::channel();
        self.events
            .send(HostEvent::QuestionTarget {
                session_id: session_id.to_owned(),
                child_session_id: child.to_owned(),
                question_id: question.to_owned(),
                reply,
            })
            .ok()?;
        result.await.ok().flatten()
    }
}

/// Why the host could not say whether a session has an open review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenReviewUnknown {
    /// Its startup recovery failed and it did not restore this session.
    RecoveryFailed(String),
    /// Its startup sweep has not finished restoring reviews yet.
    NotReady,
    /// The daemon is stopping; the next daemon restores open reviews.
    ShuttingDown,
    /// The host task ended while the daemon was still running.
    HostStopped,
}
