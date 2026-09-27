//! Direct control observations. Dashboard backpressure cannot delay this feed.
use super::*;
use mj_core::state::MaterializedTurnOutcome;
use mj_core::subagent::SubagentToolRequest;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DelegationObservation {
    pub requests: Vec<SubagentToolRequest>,
    pub completed: Vec<String>,
    pub idle: bool,
    pub outcome: Option<MaterializedTurnOutcome>,
    pub in_flight: Vec<String>,
    pub credential_signal: Option<CredentialSyncSignal>,
}

impl DelegationObservation {
    fn from_view(view: &ManagedSessionView) -> Option<Self> {
        let snapshot = view.snapshot.as_ref()?;
        Some(Self {
            requests: snapshot.subagent_requests.clone(),
            completed: snapshot
                .subagent_results
                .iter()
                .map(|r| r.request_id.clone())
                .collect(),
            idle: matches!(
                snapshot.materialized.execution,
                mj_core::state::MaterializedExecutionState::Idle
            ),
            outcome: snapshot.materialized.last_turn_outcome.clone(),
            in_flight: snapshot
                .materialized
                .active_turn
                .iter()
                .map(|t| t.command_id.clone())
                .chain(
                    snapshot
                        .materialized
                        .queued_prompts
                        .iter()
                        .map(|p| p.command_id.clone()),
                )
                .collect(),
            credential_signal: snapshot.latest_credential_sync_signal.clone(),
        })
    }
}

#[derive(Default)]
struct Mailbox {
    next_generation: u64,
    current: BTreeMap<String, (u64, Option<DelegationObservation>)>,
    pending: BTreeMap<String, Option<DelegationObservation>>,
    ready: VecDeque<String>,
}
impl Mailbox {
    fn enqueue(&mut self, id: &str, value: Option<DelegationObservation>) {
        if self.pending.insert(id.to_owned(), value).is_none() {
            self.ready.push_back(id.to_owned());
        }
    }
}

#[derive(Clone)]
pub(crate) struct DelegationSender {
    mailbox: Arc<Mutex<Mailbox>>,
    wake: mpsc::Sender<()>,
}
pub(crate) struct DelegationUpdates {
    mailbox: Arc<Mutex<Mailbox>>,
    wake: mpsc::Receiver<()>,
}
pub(crate) struct DelegationPublisher {
    sender: DelegationSender,
    session: String,
    generation: u64,
}
impl DelegationSender {
    pub(super) fn register(&self, session: &str) -> Arc<DelegationPublisher> {
        let mut mailbox = self.mailbox.lock().expect("delegation mailbox poisoned");
        mailbox.next_generation += 1;
        let generation = mailbox.next_generation;
        mailbox
            .current
            .insert(session.to_owned(), (generation, None));
        Arc::new(DelegationPublisher {
            sender: self.clone(),
            session: session.to_owned(),
            generation,
        })
    }
}
impl DelegationPublisher {
    pub(super) fn publish(&self, view: &ManagedSessionView) {
        // A disconnected view is not evidence that durable requests disappeared.
        let Some(value) = DelegationObservation::from_view(view) else {
            return;
        };
        let mut mailbox = self
            .sender
            .mailbox
            .lock()
            .expect("delegation mailbox poisoned");
        let Some((generation, previous)) = mailbox.current.get_mut(&self.session) else {
            return;
        };
        if *generation != self.generation || previous.as_ref() == Some(&value) {
            return;
        }
        *previous = Some(value.clone());
        mailbox.enqueue(&self.session, Some(value));
        let _ = self.sender.wake.try_send(());
    }
}
impl Drop for DelegationPublisher {
    fn drop(&mut self) {
        let mut mailbox = self
            .sender
            .mailbox
            .lock()
            .expect("delegation mailbox poisoned");
        if mailbox
            .current
            .get(&self.session)
            .is_some_and(|(g, _)| *g == self.generation)
        {
            mailbox.current.remove(&self.session);
            mailbox.enqueue(&self.session, None);
            let _ = self.sender.wake.try_send(());
        }
    }
}
impl DelegationUpdates {
    pub(crate) async fn recv(&mut self) -> Option<(String, Option<DelegationObservation>)> {
        loop {
            {
                let mut mailbox = self.mailbox.lock().expect("delegation mailbox poisoned");
                if let Some(id) = mailbox.ready.pop_front() {
                    let value = mailbox
                        .pending
                        .remove(&id)
                        .expect("queued delegation observation");
                    return Some((id, value));
                }
            }
            self.wake.recv().await?;
        }
    }
}
pub(crate) fn delegation_channel() -> (DelegationSender, DelegationUpdates) {
    let mailbox = Arc::new(Mutex::new(Mailbox::default()));
    let (tx, rx) = mpsc::channel(1);
    (
        DelegationSender {
            mailbox: mailbox.clone(),
            wake: tx,
        },
        DelegationUpdates { mailbox, wake: rx },
    )
}
