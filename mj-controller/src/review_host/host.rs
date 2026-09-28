use super::*;

/// A handle on the review host. Cheap to clone; every method is a message.
#[derive(Clone)]
pub struct TurnReviewHost {
    pub(super) events: mpsc::Sender<HostEvent>,
    pub(super) shared: Arc<HostShared>,
}

/// What surfaces read without waiting for the host's task.
pub(super) struct HostShared {
    pub(super) views: Mutex<BTreeMap<String, RuntimeReviewView>>,
    pub(super) changed: Arc<dyn Fn() + Send + Sync>,
    pub(super) shutdown: tokio::sync::OnceCell<Result<(), String>>,
    pub(super) observations: Mutex<super::observations::Observations>,
    pub(super) observation_ready: tokio::sync::Notify,
    pub(super) stop_delivery: tokio_util::sync::CancellationToken,
    pub(super) initialized: tokio::sync::watch::Sender<Option<Result<(), String>>>,
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
        Self::spawn_notifying(control, config, Arc::new(|| {}), None)
    }

    /// Starts the production host and calls `changed` whenever a surface view
    /// is added, changed, or removed. A review waits behind the background
    /// work `background` coordinates, such as a recovery copy, rather than
    /// racing it for the session.
    #[must_use]
    pub fn spawn_notifying(
        control: SessionManagerControl,
        config: ReviewConfigSource,
        changed: Arc<dyn Fn() + Send + Sync>,
        background: Option<Arc<crate::recovery_gate::RecoveryGate>>,
    ) -> Self {
        Self::spawn_in_notifying(
            control,
            config,
            Arc::new(ControllerEnvironment { background }),
            changed,
        )
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
        let (events, receiver) = mpsc::channel(256);
        let (persistence, persistence_receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(HostShared {
            views: Mutex::default(),
            changed,
            shutdown: tokio::sync::OnceCell::new(),
            observations: Mutex::default(),
            observation_ready: tokio::sync::Notify::new(),
            stop_delivery: tokio_util::sync::CancellationToken::new(),
            initialized: tokio::sync::watch::channel(None).0,
        });
        let host = Self {
            events: events.clone(),
            shared: shared.clone(),
        };
        let persistence_task = tokio::spawn(persistence_loop(
            environment.clone(),
            events.clone(),
            persistence_receiver,
            shared.stop_delivery.clone(),
        ));
        // Discover durable owners before any new review writes. Daemon startup
        // waits for this discovery to install holds before accepting prompts.
        persistence
            .send(PersistenceRequest::DiscoverOwned)
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

                next_epoch: 0,
                sessions: BTreeMap::new(),
                preparation_cancellation: BTreeMap::new(),
                recovery_candidates: BTreeSet::new(),
                recovery_in_flight: BTreeSet::new(),
                dirty: BTreeSet::new(),
                checkpointing: BTreeMap::new(),
                next_checkpoint_revision: 0,
                persistence_errors: BTreeMap::new(),
                start_replies: BTreeMap::new(),
                resolve_replies: BTreeMap::new(),
            },
            receiver,
        ));
        host
    }

    /// Wait only for durable ownership discovery and prompt holds, never for
    /// a worker role or its running turn.
    pub async fn ready(&self) -> Result<(), String> {
        let mut initialized = self.shared.initialized.subscribe();
        loop {
            if let Some(result) = initialized.borrow_and_update().clone() {
                return result;
            }
            initialized
                .changed()
                .await
                .map_err(|_| "review initialization stopped".to_owned())?;
        }
    }

    /// Reports one session's latest view. This is the trigger's only input.
    /// Prune the retained last-views to the live session set. The daemon calls
    /// this from its reconcile so a stopped or destroyed session's transcript is
    /// released rather than retained in `sessions` forever.
    pub fn retain_sessions(&self, live: std::collections::BTreeSet<String>) {
        self.shared
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(live);
        self.shared.observation_ready.notify_one();
    }

    pub fn observe(&self, session_id: &str, view: &ManagedSessionView) {
        self.shared
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(session_id, view);
        self.shared.observation_ready.notify_one();
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
            .await
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
            .await
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
                    .await
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
