use super::*;

/// A handle on the review host. Cheap to clone; every method is a message.
#[derive(Clone)]
pub struct TurnReviewHost {
    pub(super) events: mpsc::UnboundedSender<HostEvent>,
    pub(super) shared: Arc<HostShared>,
}

/// What surfaces read without waiting for the host's task.
pub(super) struct HostShared {
    pub(super) views: Mutex<BTreeMap<String, RuntimeReviewView>>,
    pub(super) changed: Arc<dyn Fn() + Send + Sync>,
    pub(super) shutdown: tokio::sync::OnceCell<Result<(), String>>,
}

impl std::fmt::Debug for TurnReviewHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TurnReviewHost")
    }
}

impl TurnReviewHost {
    /// Starts the host's task, reviewing through the real controller.
    #[must_use]
    pub fn spawn(control: SessionManagerControl, config: ReviewConfigSource) -> Self {
        Self::spawn_notifying(control, config, Arc::new(|| {}))
    }

    /// Starts the production host and calls `changed` whenever a surface view
    /// is added, changed, or removed.
    #[must_use]
    pub fn spawn_notifying(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::spawn_in_notifying(control, config, Arc::new(ControllerEnvironment), changed)
    }

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
    pub(super) fn spawn_in_notifying(
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
        });
        let host = Self {
            events: events.clone(),
            shared: shared.clone(),
        };
        let persistence_task = tokio::spawn(persistence_loop(
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
        tokio::spawn(host_loop(
            HostState {
                control,
                config,
                environment,
                shared,
                events,
                persistence: Some(persistence),
                persistence_task: Some(persistence_task),
                reviews: BTreeMap::new(),
                preparing: BTreeSet::new(),
                pending_open: BTreeMap::new(),
                closing: BTreeSet::new(),
                awaiting_forward_persistence: BTreeMap::new(),
                next_epoch: 0,
                sessions: BTreeMap::new(),
                preparation_cancellation: BTreeMap::new(),
                recovery_candidates: BTreeSet::new(),
                recovery_in_flight: BTreeSet::new(),
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
        // Running -> Idle is an edge, not a level: the session manager
        // suppresses unchanged views, so dropping one here can lose an
        // automatic review permanently. An unbounded hand-off keeps the
        // daemon's update loop nonblocking without dropping that edge.
        let _ = self.events.send(HostEvent::View {
            session_id: session_id.to_owned(),
            snapshot: view
                .snapshot
                .as_ref()
                .map(|snapshot| Box::new(snapshot.materialized.clone())),
            prompt_driven: view.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot
                    .operational
                    .active_prompt
                    .as_ref()
                    .is_some_and(|prompt| {
                        let user_id = format!("user:{}", prompt.command_id);
                        !snapshot
                            .materialized
                            .transcript
                            .iter()
                            .find(|item| item.stable_id == user_id)
                            .is_some_and(|item| match &item.body {
                                mj_core::state::TranscriptBody::User { content } => matches!(
                                    mj_core::acp::context_command_text(
                                        &mj_core::transcript::materialized_content_text(content)
                                    ),
                                    Some((mj_core::acp::ContextCommand::Compact, _))
                                ),
                                _ => false,
                            })
                    })
            }),
        });
    }

    /// Reviews the turn that just finished, on request.
    pub async fn start(&self, session_id: &str, manual: bool) -> Result<(), StartRefusal> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::Start {
                session_id: session_id.to_owned(),
                manual,
                reply: Some(reply),
            })
            .map_err(|_| StartRefusal("the review host stopped".to_owned()))?;
        answer
            .await
            .map_err(|_| StartRefusal("the review host stopped".to_owned()))?
    }

    /// Forwards, dismisses, or cancels the open review.
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

    /// Stops accepting review work, releases every prompt hold, and drains the
    /// ordered persistence lane before the daemon shuts its database writer
    /// down.
    pub async fn shutdown(&self) -> Result<(), String> {
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

    /// Whether an unresolved review is holding this session's prompts.
    #[must_use]
    pub fn refuses_prompt(&self, session_id: &str) -> bool {
        prompt_refusal(session_id).is_some()
    }
}
