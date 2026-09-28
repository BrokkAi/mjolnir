use super::*;

#[derive(Debug, Clone)]
pub struct SessionManagerUpdate {
    pub session_id: String,
    pub view: ManagedSessionView,
}

pub struct SessionManagerChannels {
    pub targets: watch::Sender<Vec<RelaySessionTarget>>,
    pub control: SessionManagerControl,
    pub updates: SessionManagerUpdates,
    pub shutdown: SessionManagerShutdown,
}

/// Client-side half of a remotely owned session manager.
///
/// The daemon remains the only process with relay connections. A control
/// surface publishes the daemon's latest views here and forwards requests from
/// [`RemoteSessionRequests`] over its authenticated transport.
pub struct RemoteSessionManagerChannels {
    pub targets: watch::Sender<Vec<RelaySessionTarget>>,
    pub control: SessionManagerControl,
    pub updates: SessionManagerUpdates,
    pub shutdown: SessionManagerShutdown,
    pub publisher: RemoteSessionPublisher,
    pub requests: RemoteSessionRequests,
}

#[derive(Clone)]
pub struct RemoteSessionPublisher {
    pub(super) updates: mpsc::UnboundedSender<RemoteManagerUpdate>,
}

impl RemoteSessionPublisher {
    pub async fn publish(&self, session_id: String, view: ManagedSessionView) -> Result<()> {
        self.updates
            .send(RemoteManagerUpdate::Publish { session_id, view })
            .context("remote session manager stopped")
    }

    pub fn try_publish(&self, session_id: String, view: ManagedSessionView) -> Result<()> {
        self.updates
            .send(RemoteManagerUpdate::Publish { session_id, view })
            .context("remote session manager update queue is unavailable")
    }
}

pub struct RemoteSessionRequests {
    pub(super) requests: mpsc::Receiver<RemoteSessionRequest>,
}

impl RemoteSessionRequests {
    pub async fn recv(&mut self) -> Option<RemoteSessionRequest> {
        self.requests.recv().await
    }
}

pub enum RemoteSessionRequest {
    Submit {
        session_id: String,
        command_id: String,
        command: RelayCommand,
        admission: Option<ReviewDeliveryAdmission>,
        reply: oneshot::Sender<std::result::Result<u64, mj_client::session::SubmitFailure>>,
    },
    Sync {
        session_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    RespondElicitation {
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    StopBackgroundTask {
        session_id: String,
        background_task_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    Reviewer {
        session_id: String,
        /// Which reviewing role the action drives; `None` is the default one.
        role: Option<String>,
        action: ReviewerAction,
        reply: oneshot::Sender<std::result::Result<ReviewerOutcome, String>>,
    },
}

impl RemoteSessionRequest {
    /// The session this request acts on. Requests for one session have to be
    /// carried out in the order they were made.
    pub fn session_id(&self) -> &str {
        match self {
            Self::Submit { session_id, .. }
            | Self::Sync { session_id, .. }
            | Self::RespondElicitation { session_id, .. }
            | Self::StopBackgroundTask { session_id, .. }
            | Self::Reviewer { session_id, .. } => session_id,
        }
    }
}

/// Keeps each session's relay requests in the order they were made, while
/// letting different sessions overlap.
///
/// A bridge that spawns every request concurrently loses the order the caller
/// submitted them in, and the order is load-bearing: `/effort` followed by a
/// prompt has to reach the relay that way round, or the prompt runs under the
/// old setting. Awaiting each request inline would restore the order but would
/// also make one slow session block every other one, so instead each request
/// waits on its own session's previous request and nothing else.
#[derive(Default)]
pub struct SessionRequestOrder {
    pub(super) latest: std::collections::HashMap<SessionRequestStream, tokio::task::JoinHandle<()>>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) enum SessionRequestStream {
    Primary(String),
    Reviewer(String, Option<String>),
}

impl SessionRequestOrder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `forward` for `request` after everything already queued for the
    /// same primary or reviewer role has finished. Independent reviewers
    /// must not delay primary controls or one another.
    pub fn dispatch<F, Fut>(&mut self, request: RemoteSessionRequest, forward: F)
    where
        F: FnOnce(RemoteSessionRequest) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        // Sessions that have gone quiet leave a finished handle behind; drop
        // them here so the map tracks live work rather than every session the
        // bridge has ever served.
        self.latest.retain(|_, handle| !handle.is_finished());
        let stream = match &request {
            RemoteSessionRequest::Reviewer {
                session_id, role, ..
            } => SessionRequestStream::Reviewer(session_id.clone(), role.clone()),
            _ => SessionRequestStream::Primary(request.session_id().to_owned()),
        };
        let previous = self.latest.remove(&stream);
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                // A panicked predecessor still releases its successor: the
                // request behind it is the user's, and dropping it silently
                // would be worse than running it late.
                if let Err(error) = previous.await {
                    tracing::error!(%error, "previous session request task failed");
                }
            }
            forward(request).await;
        });
        self.latest.insert(stream, handle);
    }
}

/// Exclusive owner of the manager task and every relay actor below it.
///
/// Long-running control surfaces explicitly await [`Self::shutdown`] before
/// their Tokio runtime goes away. Drop remains an aborting fallback for tests
/// and early-return paths that cannot await.
pub struct SessionManagerShutdown {
    pub(super) signal: Option<oneshot::Sender<()>>,
    pub(super) task: Option<tokio::task::JoinHandle<()>>,
}

impl SessionManagerShutdown {
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(signal) = self.signal.take() {
            let _ = signal.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.context("session manager shutdown task failed")?;
        }
        Ok(())
    }
}

impl Drop for SessionManagerShutdown {
    fn drop(&mut self) {
        if let Some(signal) = self.signal.take() {
            let _ = signal.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub(crate) struct CoalescedUpdateSender {
    producers: Arc<Mutex<BTreeMap<String, Arc<()>>>>,
    producer: Option<Arc<UpdateProducer>>,
    pub(super) delegation: Option<DelegationSender>,
    pub(super) observer: Option<Arc<DelegationPublisher>>,
    pub(super) pending: Arc<Mutex<BTreeMap<String, PendingUpdate>>>,
    pub(super) wake: mpsc::Sender<()>,
}

struct UpdateProducer {
    registry: Arc<Mutex<BTreeMap<String, Arc<()>>>>,
    session_id: String,
    identity: Arc<()>,
}

impl Drop for UpdateProducer {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .expect("session producer registry poisoned");
        if registry
            .get(&self.session_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.identity))
        {
            registry.remove(&self.session_id);
        }
    }
}

/// Bounded latest-state feed for the dashboard. At most one snapshot per
/// session is retained while the consumer is busy.
pub struct SessionManagerUpdates {
    pub(super) pending: Arc<Mutex<BTreeMap<String, PendingUpdate>>>,
    pub(super) wake: mpsc::Receiver<()>,
    // Keep the update owned until the consumer asks for another one. Its
    // completion edge can schedule a review or continuation in the meantime.
    delivered_work: Option<crate::upgrade::Work>,
}

pub(super) struct PendingUpdate {
    update: SessionManagerUpdate,
    work: Option<crate::upgrade::Work>,
}

impl CoalescedUpdateSender {
    pub(super) fn for_actor(&self, session_id: &str) -> Self {
        assert!(
            self.producer.is_none(),
            "only the manager registers producers"
        );
        let identity = Arc::new(());
        let mut registry = self
            .producers
            .lock()
            .expect("session producer registry poisoned");
        registry.insert(session_id.to_owned(), identity.clone());
        // Replacing a producer also invalidates its undelivered observation.
        self.pending
            .lock()
            .expect("session update coalescer poisoned")
            .remove(session_id);
        let mut sender = self.clone();
        sender.producer = Some(Arc::new(UpdateProducer {
            registry: self.producers.clone(),
            session_id: session_id.to_owned(),
            identity,
        }));
        sender
    }

    pub(crate) fn send(&self, update: SessionManagerUpdate) {
        let registry = self
            .producers
            .lock()
            .expect("session producer registry poisoned");
        if let Some(producer) = &self.producer {
            assert_eq!(producer.session_id, update.session_id);
            if !registry
                .get(&update.session_id)
                .is_some_and(|current| Arc::ptr_eq(current, &producer.identity))
            {
                return;
            }
        }
        if let Some(observer) = &self.observer {
            observer.publish(&update.view);
        }
        if self.wake.is_closed() {
            return;
        }
        self.pending
            .lock()
            .expect("session update coalescer poisoned")
            .insert(
                update.session_id.clone(),
                PendingUpdate {
                    update,
                    work: crate::upgrade::activity("session update").ok(),
                },
            );
        let _ = self.wake.try_send(());
    }
}

impl SessionManagerUpdates {
    pub(super) fn pop_pending(&mut self) -> Option<SessionManagerUpdate> {
        self.delivered_work = None;
        let pending = self
            .pending
            .lock()
            .expect("session update coalescer poisoned")
            .pop_first()
            .map(|(_, update)| update)?;
        self.delivered_work = pending.work;
        Some(pending.update)
    }

    pub async fn recv(&mut self) -> Option<SessionManagerUpdate> {
        loop {
            if let Some(update) = self.pop_pending() {
                return Some(update);
            }
            self.wake.recv().await?;
        }
    }

    pub fn try_recv(
        &mut self,
    ) -> std::result::Result<SessionManagerUpdate, mpsc::error::TryRecvError> {
        if let Some(update) = self.pop_pending() {
            return Ok(update);
        }
        self.wake.try_recv()?;
        self.pop_pending().ok_or(mpsc::error::TryRecvError::Empty)
    }
}

pub(crate) fn coalesced_update_channel() -> (CoalescedUpdateSender, SessionManagerUpdates) {
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let (wake_tx, wake_rx) = mpsc::channel(1);
    (
        CoalescedUpdateSender {
            producers: Default::default(),
            producer: None,
            delegation: None,
            observer: None,
            pending: pending.clone(),
            wake: wake_tx,
        },
        SessionManagerUpdates {
            pending,
            wake: wake_rx,
            delivered_work: None,
        },
    )
}
