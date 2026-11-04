//! Ordered host requests and durable receipts for prompts entered before attachment.
use super::*;
use crate::daemon;
use mj_chat::chat::StartupPrompt;
use std::future::Future;
use std::pin::Pin;
use tokio_util::task::AbortOnDropHandle;

async fn retry_admission<F, Fut>(settlement: tokio_util::sync::CancellationToken, mut attempt: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        let done = tokio::select! {biased; _=settlement.cancelled()=>return,done=attempt()=>done};
        if done {
            return;
        }
        tokio::select! {_=settlement.cancelled()=>return,_=tokio::time::sleep(backoff)=>{}}
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

// A conflict describes another immutable request under the original ID; it
// cannot establish that the original request was unaccepted.
fn admission_failure_is_definite(error: &anyhow::Error) -> bool {
    if error.is::<mj_client::daemon::StartupPromptIdentityConflict>() {
        return false;
    }
    error
        .downcast_ref::<mj_client::daemon::DaemonRefusal>()
        .is_some_and(|refusal| !refusal.delivery_unconfirmed())
}

type SaveJob = Pin<Box<dyn Future<Output = ()> + Send>>;

struct SessionMailbox {
    sender: Sender<SaveJob>,
    _worker: AbortOnDropHandle<()>,
}
impl SessionMailbox {
    fn new() -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SaveJob>(64);
        let worker = AbortOnDropHandle::new(tokio::spawn(async move {
            while let Some(work) = receiver.recv().await {
                work.await;
            }
        }));
        let worker = tokio::spawn(async move {
            if let Err(error) = worker.await
                && !error.is_cancelled()
            {
                tracing::error!(%error,"ordered startup request worker failed");
            }
        });
        Self {
            sender,
            _worker: AbortOnDropHandle::new(worker),
        }
    }
    fn admit(&self, work: SaveJob) -> Result<()> {
        self.sender
            .try_send(work)
            .map_err(|error| anyhow::anyhow!("startup request queue is unavailable: {error}"))
    }
}

#[derive(Default)]
pub(super) struct StartupPrompts {
    mailboxes: BTreeMap<String, SessionMailbox>,
    pending: BTreeMap<String, BTreeMap<String, TrackedPrompt>>,
    polls: BTreeMap<String, OutcomePoll>,
    next_order: u64,
    moves: BTreeMap<String, MoveAdmission>,
    next_move_generation: u64,
}
struct HeldSave {
    _guard: mj_client::operations::CriticalOperationGuard,
}
#[derive(Clone)]
struct MoveAdmission {
    generation: u64,
    phase: MoveAdmissionPhase,
}
#[derive(Clone)]
enum MoveAdmissionPhase {
    Preparing(Option<String>),
    Ready(String),
    Completed(String),
    Failed(Option<String>),
}
enum OriginAdmission {
    Hold,
    Send,
    Refuse,
}

enum TrackedPrompt {
    Waiting {
        prompt: StartupPrompt,
        inherited_draft: Option<String>,
        recalled: bool,
        admission_error: Option<String>,
        order: u64,
        held: Option<HeldSave>,
        settlement: tokio_util::sync::CancellationToken,
    },
    Settled,
}
struct OutcomePoll {
    task: Option<AbortOnDropHandle<()>>,
    next: std::time::Instant,
    backoff: Duration,
    offset: usize,
}
impl Default for OutcomePoll {
    fn default() -> Self {
        Self {
            task: None,
            next: std::time::Instant::now(),
            backoff: Duration::from_secs(1),
            offset: 0,
        }
    }
}

impl StartupPrompts {
    fn set_move_phase(&mut self, session_id: &str, phase: MoveAdmissionPhase) {
        let identity = |phase: &MoveAdmissionPhase| match phase {
            MoveAdmissionPhase::Preparing(id) | MoveAdmissionPhase::Failed(id) => id.clone(),
            MoveAdmissionPhase::Ready(id) | MoveAdmissionPhase::Completed(id) => Some(id.clone()),
        };
        let changed=self.moves.get(session_id).is_some_and(|current|matches!((identity(&current.phase),identity(&phase)),(Some(old),Some(new)) if old!=new));
        let generation = if changed {
            self.next_move_generation = self.next_move_generation.wrapping_add(1);
            self.next_move_generation
        } else {
            self.moves
                .get(session_id)
                .map_or(0, |admission| admission.generation)
        };
        self.moves
            .insert(session_id.to_owned(), MoveAdmission { generation, phase });
    }
    pub(super) fn begin_move(&mut self, session_id: &str, id: Option<String>) {
        self.next_move_generation = self.next_move_generation.wrapping_add(1);
        self.moves.insert(
            session_id.to_owned(),
            MoveAdmission {
                generation: self.next_move_generation,
                phase: MoveAdmissionPhase::Preparing(id),
            },
        );
    }
    pub(super) fn prepare_move(&mut self, session_id: &str, id: Option<String>) {
        if matches!(
            self.moves.get(session_id).map(|admission| &admission.phase),
            Some(MoveAdmissionPhase::Preparing(None))
        ) {
            self.set_move_phase(session_id, MoveAdmissionPhase::Preparing(id));
        } else if matches!(self.moves.get(session_id).map(|admission|&admission.phase),Some(MoveAdmissionPhase::Failed(previous)) if previous.is_none())
        {
            self.set_move_phase(session_id, MoveAdmissionPhase::Failed(id));
        }
    }
    pub(super) fn move_origin(&self, session_id: &str) -> Option<(u64, Option<String>)> {
        let admission = self.moves.get(session_id)?;
        match &admission.phase {
            MoveAdmissionPhase::Preparing(id) => Some((admission.generation, id.clone())),
            MoveAdmissionPhase::Ready(id) => Some((admission.generation, Some(id.clone()))),
            _ => None,
        }
    }
    pub(super) fn terminal_move(&self, session_id: &str) -> Option<(u64, Option<String>, bool)> {
        let admission = self.moves.get(session_id)?;
        match &admission.phase {
            MoveAdmissionPhase::Completed(id) => {
                Some((admission.generation, Some(id.clone()), true))
            }
            MoveAdmissionPhase::Failed(id) => Some((admission.generation, id.clone(), false)),
            _ => None,
        }
    }
    pub(super) fn observe_move(&mut self, session_id: &str, id: String, terminal: Option<bool>) {
        match self.moves.get(session_id).map(|admission| &admission.phase) {
            Some(MoveAdmissionPhase::Preparing(None)) => return,
            Some(MoveAdmissionPhase::Preparing(Some(expected))) if expected != &id => return,
            Some(
                MoveAdmissionPhase::Ready(expected)
                | MoveAdmissionPhase::Completed(expected)
                | MoveAdmissionPhase::Failed(Some(expected)),
            ) if terminal.is_some() && expected != &id => return,
            _ => {}
        }
        let state = match terminal {
            None => MoveAdmissionPhase::Ready(id),
            Some(true) => MoveAdmissionPhase::Completed(id),
            Some(false) => MoveAdmissionPhase::Failed(Some(id)),
        };
        self.set_move_phase(session_id, state);
    }
    pub(super) fn finish_move(&mut self, session_id: &str, completed: Option<String>) {
        let state = if let Some(id) = completed {
            MoveAdmissionPhase::Completed(id)
        } else {
            let id = self.move_origin(session_id).and_then(|(_, id)| id);
            MoveAdmissionPhase::Failed(id)
        };
        self.set_move_phase(session_id, state);
    }
    fn origin_admission(&self, session_id: &str, origin: Option<&str>) -> OriginAdmission {
        let Some(origin) = origin else {
            return OriginAdmission::Send;
        };
        match self.moves.get(session_id).map(|admission| &admission.phase) {
            Some(MoveAdmissionPhase::Ready(id) | MoveAdmissionPhase::Completed(id))
                if id == origin =>
            {
                OriginAdmission::Send
            }
            Some(MoveAdmissionPhase::Preparing(Some(id))) if id == origin => OriginAdmission::Hold,
            Some(MoveAdmissionPhase::Preparing(None)) | None => OriginAdmission::Hold,
            _ => OriginAdmission::Refuse,
        }
    }
    pub(super) fn flush_held(
        &mut self,
        updates: &UnboundedSender<DashboardIoUpdate>,
        tracker: &CriticalOperationTracker,
    ) {
        let mut ready = Vec::new();
        let mut refused =
            BTreeMap::<String, BTreeMap<String, mj_client::daemon::StartupPromptOutcome>>::new();
        for (session_id, entries) in &self.pending {
            for (id, entry) in entries {
                if let TrackedPrompt::Waiting {
                    prompt,
                    order,
                    held: Some(_),
                    ..
                } = entry
                {
                    match self.origin_admission(session_id, prompt.move_operation_id.as_deref()) {
                        OriginAdmission::Send => {
                            ready.push((*order, session_id.clone(), id.clone()))
                        }
                        OriginAdmission::Refuse => {
                            refused.entry(session_id.clone()).or_default().insert(id.clone(),mj_client::daemon::StartupPromptOutcome::Refused {encoded_draft:prompt.payload.encode_draft(),reason:"The move ended before this prompt was admitted; its draft is kept.".into()});
                        }
                        OriginAdmission::Hold => {}
                    }
                }
            }
        }
        ready.sort_by_key(|entry| entry.0);
        for (_, session_id, id) in ready {
            if let Some(TrackedPrompt::Waiting {
                prompt,
                inherited_draft,
                held: Some(_held),
                ..
            }) = self
                .pending
                .get_mut(&session_id)
                .and_then(|entries| entries.remove(&id))
            {
                self.enqueue(
                    session_id,
                    prompt,
                    inherited_draft,
                    updates.clone(),
                    tracker.clone(),
                );
            }
        }
        for (session_id, outcomes) in refused {
            io::report(
                "restoring unadmitted move prompts",
                updates,
                DashboardIoUpdate::StartupPromptOutcomes {
                    session_id,
                    result: Ok(outcomes),
                },
            );
        }
    }
    pub(super) fn retain_unconfirmed(
        &self,
        dashboard: &mut DashboardState,
        chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>,
    ) {
        for (session_id, entries) in &self.pending {
            for state in entries.values() {
                if let TrackedPrompt::Waiting {
                    prompt,
                    inherited_draft,
                    ..
                } = state
                {
                    if let Some(chat) = chats
                        .get_mut(session_id)
                        .filter(|_| !dashboard.has_standby_composer(session_id))
                    {
                        chat.retain_startup_receipt(prompt.clone(), inherited_draft.clone());
                    } else {
                        dashboard.retain_startup_receipt(
                            session_id,
                            prompt.clone(),
                            inherited_draft.clone(),
                        );
                    }
                }
            }
        }
    }

    pub(super) fn cancel_unadmitted(
        &mut self,
        dashboard: &mut DashboardState,
        chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>,
    ) {
        let mut refused =
            BTreeMap::<String, BTreeMap<String, mj_client::daemon::StartupPromptOutcome>>::new();
        for (session_id, entries) in &self.pending {
            for (id, state) in entries {
                if let TrackedPrompt::Waiting {
                    prompt,
                    held: Some(_),
                    ..
                } = state
                {
                    refused.entry(session_id.clone()).or_default().insert(
                        id.clone(),
                        mj_client::daemon::StartupPromptOutcome::Refused {
                            encoded_draft: prompt.payload.encode_draft(),
                            reason: "The prompt was not admitted before closing this view.".into(),
                        },
                    );
                }
            }
        }
        for (session_id, outcomes) in refused {
            self.apply_outcomes(&session_id, outcomes, dashboard, chats);
        }
    }
    pub(super) fn enqueue(
        &mut self,
        session_id: String,
        prompt: StartupPrompt,
        inherited_draft: Option<String>,
        updates: UnboundedSender<DashboardIoUpdate>,
        tracker: CriticalOperationTracker,
    ) {
        self.enqueue_inner(session_id, prompt, inherited_draft, updates, tracker, false);
    }
    pub(super) fn resume(
        &mut self,
        session_id: String,
        prompt: StartupPrompt,
        inherited_draft: Option<String>,
        updates: UnboundedSender<DashboardIoUpdate>,
        tracker: CriticalOperationTracker,
    ) {
        if self
            .pending
            .get(&session_id)
            .is_some_and(|entries| entries.contains_key(&prompt.command_id))
        {
            return;
        }
        self.enqueue_inner(session_id, prompt, inherited_draft, updates, tracker, true);
    }
    fn enqueue_inner(
        &mut self,
        session_id: String,
        prompt: StartupPrompt,
        inherited_draft: Option<String>,
        updates: UnboundedSender<DashboardIoUpdate>,
        tracker: CriticalOperationTracker,
        recovering: bool,
    ) {
        let label = format!("queueing a prompt for session {}", short_id(&session_id));
        let guard = tracker.begin(label.clone());
        let reported_session = session_id.clone();
        let reported_prompt = prompt.clone();
        let error_session = session_id.clone();
        let error_prompt = prompt.clone();
        let error_updates = updates.clone();
        let encoded_draft = prompt.payload.encode_draft();
        let pending_prompt = prompt.clone();
        let settlement = tokio_util::sync::CancellationToken::new();
        if !recovering
            && !matches!(
                self.origin_admission(&session_id, prompt.move_operation_id.as_deref()),
                OriginAdmission::Send
            )
        {
            self.pending.entry(session_id).or_default().insert(
                prompt.command_id.clone(),
                TrackedPrompt::Waiting {
                    prompt,
                    inherited_draft,
                    recalled: false,
                    admission_error: None,
                    order: self.next_order,
                    settlement: settlement.clone(),
                    held: Some(HeldSave { _guard: guard }),
                },
            );
            self.next_order = self.next_order.wrapping_add(1);
            return;
        }
        let mailbox = self
            .mailboxes
            .entry(session_id.clone())
            .or_insert_with(SessionMailbox::new);
        let saved_inherited_draft = inherited_draft.clone();
        let retry_settlement = settlement.clone();
        let admitted = mailbox.admit(Box::pin(async move {
            retry_admission(retry_settlement, || {
                let session_id = session_id.clone();
                let prompt = prompt.clone();
                let inherited_draft = inherited_draft.clone();
                let reported_session = reported_session.clone();
                let reported_prompt = reported_prompt.clone();
                let label = label.clone();
                let updates = updates.clone();
                let encoded_draft = encoded_draft.clone();
                async move {
                    let (complete, completed) = tokio::sync::oneshot::channel();
                    let task = AbortOnDropHandle::new(io::spawn_background_async(
                        label,
                        updates,
                        io::SAVE_ACK_TIMEOUT,
                        async move {
                            let result = daemon::connect_or_start()
                                .await?
                                .queue_startup_prompt(
                                    session_id,
                                    prompt.command_id,
                                    prompt.payload.text.clone(),
                                    prompt.payload.content_blocks(),
                                    inherited_draft,
                                    encoded_draft,
                                    prompt.move_operation_id,
                                )
                                .await;
                            match result {
                                Ok(()) => Ok(Ok(())),
                                Err(error) if admission_failure_is_definite(&error) => {
                                    Ok(Err(format!("{error:#}")))
                                }
                                Err(error) => Err(error),
                            }
                        },
                        move |result| {
                            let (result, definitely_unaccepted, terminal) = match result {
                                Ok(result) => (result, true, true),
                                Err(error) => (Err(error), false, false),
                            };
                            let _ = complete.send(terminal);
                            DashboardIoUpdate::StartupPromptQueued {
                                session_id: reported_session,
                                prompt: reported_prompt,
                                result,
                                definitely_unaccepted,
                            }
                        },
                    ));
                    if let Err(error) = task.await {
                        tracing::error!(%error,"startup admission supervisor failed");
                    }
                    completed.await.unwrap_or(false)
                }
            })
            .await;
            drop(guard);
        }));
        match admitted {
            Ok(()) => {
                self.pending.entry(error_session).or_default().insert(
                    pending_prompt.command_id.clone(),
                    TrackedPrompt::Waiting {
                        prompt: pending_prompt,
                        inherited_draft: saved_inherited_draft,
                        recalled: false,
                        admission_error: None,
                        order: self.next_order,
                        held: None,
                        settlement: settlement.clone(),
                    },
                );
                self.next_order = self.next_order.wrapping_add(1);
            }
            Err(error) => {
                self.pending
                    .entry(error_session.clone())
                    .or_default()
                    .insert(
                        error_prompt.command_id.clone(),
                        TrackedPrompt::Waiting {
                            prompt: error_prompt.clone(),
                            inherited_draft: saved_inherited_draft,
                            recalled: false,
                            admission_error: None,
                            order: self.next_order,
                            held: None,
                            settlement: settlement.clone(),
                        },
                    );
                self.next_order = self.next_order.wrapping_add(1);
                io::report(
                    "queueing startup prompt",
                    &error_updates,
                    DashboardIoUpdate::StartupPromptQueued {
                        session_id: error_session,
                        prompt: error_prompt,
                        result: Err(format!("{error:#}")),
                        definitely_unaccepted: true,
                    },
                );
            }
        }
    }
    pub(super) fn withdraw(
        &mut self,
        session_id: String,
        prompt: StartupPrompt,
        updates: UnboundedSender<DashboardIoUpdate>,
        tracker: CriticalOperationTracker,
    ) {
        if let Some(TrackedPrompt::Waiting { recalled, .. }) = self
            .pending
            .get_mut(&session_id)
            .and_then(|prompts| prompts.get_mut(&prompt.command_id))
        {
            *recalled = true;
        }
        if matches!(
            self.pending
                .get(&session_id)
                .and_then(|entries| entries.get(&prompt.command_id)),
            Some(TrackedPrompt::Waiting { held: Some(_), .. })
        ) {
            self.settle(&session_id, &prompt.command_id);
            io::report(
                "recalling unadmitted prompt",
                &updates,
                DashboardIoUpdate::StartupPromptWithdrawn {
                    session_id,
                    prompt,
                    result: Ok(true),
                },
            );
            return;
        }
        let label = format!("withdrawing a prompt for session {}", short_id(&session_id));
        let guard = tracker.begin(label.clone());
        let reported_session = session_id.clone();
        let command_id = prompt.command_id.clone();
        let error_session = session_id.clone();
        let error_prompt = prompt.clone();
        let error_updates = updates.clone();
        let mailbox = self
            .mailboxes
            .entry(session_id.clone())
            .or_insert_with(SessionMailbox::new);
        if let Err(error) = mailbox.admit(Box::pin(async move {
            let job = io::spawn_background_async(
                label,
                updates,
                io::SAVE_ACK_TIMEOUT,
                async move {
                    daemon::connect_or_start()
                        .await?
                        .withdraw_startup_prompt(session_id, command_id)
                        .await
                },
                move |result| DashboardIoUpdate::StartupPromptWithdrawn {
                    session_id: reported_session,
                    prompt,
                    result,
                },
            );
            if let Err(error) = job.await {
                tracing::error!(%error, "startup withdrawal supervisor failed");
            }
            drop(guard);
        })) {
            io::report(
                "withdrawing startup prompt",
                &error_updates,
                DashboardIoUpdate::StartupPromptWithdrawn {
                    session_id: error_session,
                    prompt: error_prompt,
                    result: Err(format!("{error:#}")),
                },
            );
        }
    }
    pub(super) fn is_settled(&self, session_id: &str, command_id: &str) -> bool {
        matches!(
            self.pending
                .get(session_id)
                .and_then(|entries| entries.get(command_id)),
            Some(TrackedPrompt::Settled)
        )
    }
    /// False means admission is uncertain or already settled: never create a resendable duplicate.
    pub(super) fn admission_failed(
        &mut self,
        session_id: &str,
        command_id: &str,
        error: &str,
        definite: bool,
    ) -> bool {
        match self
            .pending
            .get_mut(session_id)
            .and_then(|entries| entries.get_mut(command_id))
        {
            Some(entry) => match entry {
                TrackedPrompt::Waiting {
                    recalled,
                    admission_error,
                    settlement,
                    ..
                } => {
                    if definite {
                        let restore = !*recalled;
                        settlement.cancel();
                        *entry = TrackedPrompt::Settled;
                        restore
                    } else {
                        *admission_error = Some(error.to_owned());
                        false
                    }
                }
                TrackedPrompt::Settled => false,
            },
            None => definite,
        }
    }
    pub(super) fn withdrawal_needs_receipt(&self, session_id: &str, command_id: &str) -> bool {
        matches!(
            self.pending
                .get(session_id)
                .and_then(|prompts| prompts.get(command_id)),
            Some(TrackedPrompt::Settled)
                | Some(TrackedPrompt::Waiting {
                    admission_error: Some(_),
                    ..
                })
        )
    }
    pub(super) fn poll(&mut self, updates: &UnboundedSender<DashboardIoUpdate>) {
        let now = std::time::Instant::now();
        for (session_id, entries) in &self.pending {
            let ids = entries
                .iter()
                .filter_map(|(id, state)| {
                    matches!(state, TrackedPrompt::Waiting { held: None, .. }).then_some(id.clone())
                })
                .collect::<Vec<_>>();
            if ids.is_empty() {
                continue;
            }
            let poll = self.polls.entry(session_id.clone()).or_default();
            if poll.task.is_some() || poll.next > now {
                continue;
            }
            let command_ids = ids
                .iter()
                .cycle()
                .skip(poll.offset % ids.len())
                .take(ids.len().min(128))
                .cloned()
                .collect::<Vec<_>>();
            poll.offset = (poll.offset + command_ids.len()) % ids.len();
            let query_session = session_id.clone();
            let reported_session = session_id.clone();
            poll.task = Some(AbortOnDropHandle::new(io::spawn_background_async(
                "checking queued prompt outcomes",
                updates.clone(),
                io::SAVE_ACK_TIMEOUT,
                async move {
                    daemon::connect_or_start()
                        .await?
                        .startup_prompt_outcomes(query_session, command_ids)
                        .await
                },
                move |result| DashboardIoUpdate::StartupPromptOutcomes {
                    session_id: reported_session,
                    result,
                },
            )));
        }
    }
    pub(super) fn outcomes(&mut self, session_id: &str, succeeded: bool) {
        if let Some(poll) = self.polls.get_mut(session_id) {
            poll.task = None;
            poll.backoff = if succeeded {
                Duration::from_secs(1)
            } else {
                (poll.backoff * 2).min(Duration::from_secs(30))
            };
            poll.next = std::time::Instant::now() + poll.backoff;
        }
    }
    pub(super) fn apply_outcomes(
        &mut self,
        session_id: &str,
        outcomes: BTreeMap<String, mj_client::daemon::StartupPromptOutcome>,
        dashboard: &mut DashboardState,
        chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>,
    ) {
        let mut outcomes = outcomes.into_iter().collect::<Vec<_>>();
        outcomes.sort_by_key(|(id, _)| self.order_of(session_id, id));
        for (command_id, outcome) in outcomes {
            let Some((mut prompt, recalled)) = self.known_prompt(session_id, &command_id) else {
                continue;
            };
            let active = !dashboard.has_standby_composer(session_id);
            let applied = match outcome {
                mj_client::daemon::StartupPromptOutcome::Refused {
                    encoded_draft,
                    reason,
                } => {
                    let result = if recalled {
                        Ok(())
                    } else {
                        match mj_chat::chat::PromptPayload::decode_draft(&encoded_draft) {
                            Ok(payload) => {
                                prompt.payload = payload;
                                if let Some(chat) = chats.get_mut(session_id).filter(|_| active) {
                                    chat.restore_startup_prompt(prompt.clone())
                                } else {
                                    dashboard.restore_startup_prompt(session_id, prompt.clone())
                                }
                            }
                            Err(error) => Err(error),
                        }
                    };
                    dashboard.set_failure_notice(format!(
                        "Queued prompt returned to the draft: {reason}"
                    ));
                    result
                }
                mj_client::daemon::StartupPromptOutcome::Accepted if recalled => {
                    if let Some(chat) = chats.get_mut(session_id).filter(|_| active) {
                        chat.startup_prompt_withdrawn(prompt.clone(), Ok(false))
                            .map(|_| ())
                    } else {
                        dashboard
                            .startup_prompt_withdrawn(session_id, prompt.clone(), Ok(false))
                            .map(|_| ())
                    }
                }
                _ => Ok(()),
            };
            match applied {
                Ok(()) => {
                    if let Some(chat) = chats.get_mut(session_id).filter(|_| active) {
                        chat.settle_startup_preview(&command_id);
                    } else {
                        dashboard.settle_startup_preview(session_id, &command_id);
                    }
                    self.settle(session_id, &command_id);
                }
                Err(error) => dashboard.set_failure_notice(format!(
                    "Could not restore the queued prompt yet; it remains saved: {error:#}"
                )),
            }
        }
    }
    pub(super) fn order_of(&self, session_id: &str, command_id: &str) -> u64 {
        match self
            .pending
            .get(session_id)
            .and_then(|entries| entries.get(command_id))
        {
            Some(TrackedPrompt::Waiting { order, .. }) => *order,
            _ => u64::MAX,
        }
    }
    pub(super) fn known_prompt(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Option<(StartupPrompt, bool)> {
        match self.pending.get(session_id)?.get(command_id)? {
            TrackedPrompt::Waiting {
                prompt, recalled, ..
            } => Some((prompt.clone(), *recalled)),
            TrackedPrompt::Settled => None,
        }
    }
    pub(super) fn settle(&mut self, session_id: &str, command_id: &str) {
        if let Some(entry) = self
            .pending
            .get_mut(session_id)
            .and_then(|entries| entries.get_mut(command_id))
        {
            if let TrackedPrompt::Waiting { settlement, .. } = entry {
                settlement.cancel();
            }
            *entry = TrackedPrompt::Settled;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_recall_cannot_overtake_its_admission_and_another_session_remains_independent() {
        let first = SessionMailbox::new();
        let independent = SessionMailbox::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (recalled_tx, mut recalled_rx) = tokio::sync::oneshot::channel();
        first
            .admit(Box::pin(async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
            }))
            .unwrap();
        first
            .admit(Box::pin(async move {
                recalled_tx.send(()).unwrap();
            }))
            .unwrap();
        entered_rx.await.unwrap();
        assert!(matches!(
            recalled_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let (other_tx, other_rx) = tokio::sync::oneshot::channel();
        independent
            .admit(Box::pin(async move {
                other_tx.send(()).unwrap();
            }))
            .unwrap();
        other_rx.await.unwrap();
        release_tx.send(()).unwrap();
        recalled_rx.await.unwrap();
    }
    #[tokio::test]
    async fn durable_refusals_restore_images_once_without_duplicating_recalled_input() {
        use mj_chat::chat::{ChatState, Notices, PromptPayload, SessionHeaderIdentity};
        use mj_client::daemon::StartupPromptOutcome;
        let config = Config::default();
        let mut dashboard = DashboardState::new(
            config.clone(),
            mj_core::state::State::default(),
            BTreeMap::new(),
        );
        let reference = mj_core::attachment::AttachmentRef::new(
            b"\x89PNG\r\n\x1a\ninstalled",
            "image/png".into(),
            2,
            2,
        )
        .unwrap();
        let payload = PromptPayload::with_image(
            "original ",
            mj_chat::clipboard::ClipboardImage::from_reference(reference).unwrap(),
        );
        let mut composer = ChatState::standby(
            "session",
            &config,
            SessionHeaderIdentity::default(),
            Notices::default(),
        );
        composer.set_draft(payload.encode_draft());
        composer.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let mut prompt = composer.latest_startup_prompt().unwrap();
        prompt.move_operation_id = Some("saved-original-move".into());
        composer.paste("newer text");
        dashboard.adopt_standby_composer("session", composer.take_live_composer().unwrap());
        let mut pending = StartupPrompts::default();
        pending.pending.entry("session".into()).or_default().insert(
            prompt.command_id.clone(),
            TrackedPrompt::Waiting {
                prompt: prompt.clone(),
                inherited_draft: Some("inherited-before-admission".into()),
                recalled: false,
                admission_error: None,
                order: 0,
                held: None,
                settlement: tokio_util::sync::CancellationToken::new(),
            },
        );
        assert!(!pending.admission_failed(
            "session",
            &prompt.command_id,
            "lost enqueue acknowledgement",
            false
        ));
        assert_eq!(
            dashboard.standby_prompt_draft("session").unwrap(),
            "newer text",
            "uncertain admission must not restore a resendable duplicate"
        );
        assert_eq!(
            pending
                .known_prompt("session", &prompt.command_id)
                .unwrap()
                .0
                .command_id,
            prompt.command_id
        );
        pending.retain_unconfirmed(&mut dashboard, &mut BTreeMap::new());
        let saved = dashboard.standby_prompt_draft("session").unwrap();
        let mut reopened = ChatState::standby(
            "session",
            &config,
            SessionHeaderIdentity::default(),
            Notices::default(),
        );
        reopened.set_draft(saved.clone());
        assert_eq!(
            reopened.startup_retry_prompts(),
            vec![(prompt.clone(), Some("inherited-before-admission".into()))],
            "bounded shutdown must preserve exact ID, payload, images and Move origin"
        );
        // A lost reply may already have an accepted receipt. Resume the complete
        // original request and settle it without returning a second sendable draft.
        let conflict = anyhow::Error::new(mj_client::daemon::StartupPromptIdentityConflict);
        assert!(!admission_failure_is_definite(&conflict));
        assert!(admission_failure_is_definite(&anyhow::Error::new(
            mj_client::daemon::DaemonRefusal("not admitted".into())
        )));
        assert!(!pending.admission_failed(
            "session",
            &prompt.command_id,
            &conflict.to_string(),
            admission_failure_is_definite(&conflict)
        ));
        let (retry_prompt, inherited_draft) = reopened.startup_retry_prompts().pop().unwrap();
        let mut accepted_dashboard = DashboardState::new(
            config.clone(),
            mj_core::state::State::default(),
            BTreeMap::new(),
        );
        accepted_dashboard
            .adopt_standby_composer("session", reopened.take_live_composer().unwrap());
        let mut accepted = StartupPrompts::default();
        let (updates, _received) = tokio::sync::mpsc::unbounded_channel();
        let (tracker, _) = CriticalOperationTracker::new();
        accepted.resume(
            "session".into(),
            retry_prompt,
            inherited_draft,
            updates,
            tracker,
        );
        assert!(
            matches!(accepted.pending["session"].get(&prompt.command_id),
            Some(TrackedPrompt::Waiting { inherited_draft: Some(saved), .. })
                if saved == "inherited-before-admission")
        );
        accepted.apply_outcomes(
            "session",
            BTreeMap::from([(prompt.command_id.clone(), StartupPromptOutcome::Accepted)]),
            &mut accepted_dashboard,
            &mut BTreeMap::new(),
        );
        assert_eq!(
            accepted_dashboard.standby_prompt_draft("session").unwrap(),
            "newer text"
        );
        assert!(
            accepted_dashboard
                .standby_startup_prompts("session")
                .is_empty()
        );
        assert!(accepted.is_settled("session", &prompt.command_id));

        // Reopen the actual backend recovery representation: receipt consumption
        // and payload merge happen atomically, so terminal polling cannot append A twice.
        let recovered_saved = mj_core::draft::merge_drafts(
            &mj_core::draft::remove_startup_receipt(&saved, &prompt.command_id).unwrap(),
            &payload.encode_draft(),
        )
        .unwrap();
        let mut recovered_owner = ChatState::standby(
            "session",
            &config,
            SessionHeaderIdentity::default(),
            Notices::default(),
        );
        recovered_owner.set_draft(recovered_saved);
        assert!(recovered_owner.startup_retry_prompts().is_empty());
        let recovered_payload = PromptPayload::decode_draft(&recovered_owner.draft()).unwrap();
        assert_eq!(recovered_payload.text, "newer text\n\noriginal [image 1]");
        assert_eq!(recovered_payload.images.len(), 1);
        let mut recovered_dashboard = DashboardState::new(
            config.clone(),
            mj_core::state::State::default(),
            BTreeMap::new(),
        );
        recovered_dashboard
            .adopt_standby_composer("session", recovered_owner.take_live_composer().unwrap());
        let recovered_before = recovered_dashboard.standby_prompt_draft("session").unwrap();
        StartupPrompts::default().apply_outcomes(
            "session",
            BTreeMap::from([(
                prompt.command_id.clone(),
                StartupPromptOutcome::Refused {
                    encoded_draft: payload.encode_draft(),
                    reason: "already recovered atomically".into(),
                },
            )]),
            &mut recovered_dashboard,
            &mut BTreeMap::new(),
        );
        assert_eq!(
            recovered_dashboard.standby_prompt_draft("session").unwrap(),
            recovered_before,
            "late terminal receipt cannot append the already recovered payload again"
        );

        let outcomes = BTreeMap::from([(
            prompt.command_id.clone(),
            StartupPromptOutcome::Refused {
                encoded_draft: payload.encode_draft(),
                reason: "destination move cancelled".into(),
            },
        )]);
        let mut chats = BTreeMap::new();
        pending.apply_outcomes("session", outcomes.clone(), &mut dashboard, &mut chats);
        let recovered = dashboard.standby_prompt_draft("session").unwrap();
        pending.apply_outcomes("session", outcomes, &mut dashboard, &mut chats);
        assert_eq!(
            dashboard.standby_prompt_draft("session").unwrap(),
            recovered
        );
        let recovered = PromptPayload::decode_draft(&recovered).unwrap();
        assert_eq!(recovered.text, "newer text\n\noriginal [image 1]");
        assert_eq!(recovered.images.len(), 1);
        assert!(dashboard.standby_startup_prompts("session").is_empty());
        assert!(pending.is_settled("session", &prompt.command_id));

        let mut recalled = ChatState::standby(
            "recalled",
            &config,
            SessionHeaderIdentity::default(),
            Notices::default(),
        );
        recalled.set_draft("edit me".into());
        recalled.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let prompt = recalled.latest_startup_prompt().unwrap();
        recalled.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        ));
        recalled.paste(" now");
        dashboard.adopt_standby_composer("recalled", recalled.take_live_composer().unwrap());
        pending
            .pending
            .entry("recalled".into())
            .or_default()
            .insert(
                prompt.command_id.clone(),
                TrackedPrompt::Waiting {
                    prompt: prompt.clone(),
                    inherited_draft: None,
                    recalled: true,
                    admission_error: None,
                    order: 1,
                    held: None,
                    settlement: tokio_util::sync::CancellationToken::new(),
                },
            );
        assert!(!pending.admission_failed(
            "recalled",
            &prompt.command_id,
            "explicit admission refusal",
            true
        ));
        pending.apply_outcomes(
            "recalled",
            BTreeMap::from([(
                prompt.command_id.clone(),
                StartupPromptOutcome::Refused {
                    encoded_draft: prompt.payload.encode_draft(),
                    reason: "not admitted".into(),
                },
            )]),
            &mut dashboard,
            &mut chats,
        );
        assert_eq!(
            dashboard.standby_prompt_draft("recalled").unwrap(),
            "edit me now"
        );
        assert!(pending.is_settled("recalled", &prompt.command_id));
    }
    #[tokio::test]
    async fn unadmitted_move_prompts_wait_for_exact_origin_and_return_to_draft_on_cancel() {
        use mj_chat::chat::PromptPayload;
        let mut pending = StartupPrompts::default();
        let (updates, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (tracker, _) = CriticalOperationTracker::new();
        pending.begin_move("session", Some("move-a".into()));
        let prompt = StartupPrompt {
            command_id: "held-a".into(),
            payload: PromptPayload::text("original queued input"),
            move_operation_id: Some("move-a".into()),
        };
        pending.enqueue(
            "session".into(),
            prompt.clone(),
            None,
            updates.clone(),
            tracker.clone(),
        );
        pending.observe_move("session", "historical-move".into(), Some(true));
        pending.flush_held(&updates, &tracker);
        assert!(
            received.try_recv().is_err(),
            "a historical Move must not release the new prompt"
        );
        assert_eq!(
            tracker.blockers().len(),
            1,
            "local admission remains tracked for bounded quit"
        );
        pending.finish_move("session", None);
        pending.flush_held(&updates, &tracker);
        let DashboardIoUpdate::StartupPromptOutcomes { session_id, result } =
            received.try_recv().unwrap()
        else {
            panic!("expected local refusal")
        };
        let mut dashboard = DashboardState::new(
            Config::default(),
            mj_core::state::State::default(),
            BTreeMap::new(),
        );
        dashboard.seed_standby_prompt("session", "newer draft".into());
        pending.apply_outcomes(
            &session_id,
            result.unwrap(),
            &mut dashboard,
            &mut BTreeMap::new(),
        );
        assert_eq!(
            dashboard.standby_prompt_draft("session").unwrap(),
            "newer draft\n\noriginal queued input"
        );
        assert!(tracker.blockers().is_empty());
        assert!(matches!(
            pending.origin_admission("session", None),
            OriginAdmission::Send
        ));

        pending.begin_move("session", Some("move-b".into()));
        let prompt = StartupPrompt {
            command_id: "held-b".into(),
            payload: PromptPayload::text("recalled before admission"),
            move_operation_id: Some("move-b".into()),
        };
        pending.enqueue(
            "session".into(),
            prompt.clone(),
            None,
            updates.clone(),
            tracker.clone(),
        );
        pending.withdraw("session".into(), prompt, updates.clone(), tracker.clone());
        assert!(matches!(
            received.try_recv().unwrap(),
            DashboardIoUpdate::StartupPromptWithdrawn {
                result: Ok(true),
                ..
            }
        ));
        assert!(tracker.blockers().is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn transport_before_admission_retries_the_same_payload_before_recall_can_run() {
        let mailbox = SessionMailbox::new();
        let independent = SessionMailbox::new();
        let (attempted, mut attempts) = tokio::sync::mpsc::unbounded_channel();
        let recovered = Arc::new(tokio::sync::Notify::new());
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prompt = StartupPrompt {
            command_id: "retry-stable".into(),
            payload: mj_chat::chat::PromptPayload::text("keep exact payload"),
            move_operation_id: Some("original-move".into()),
        };
        let retried = prompt.clone();
        let permit = recovered.clone();
        let (tracker, _) = CriticalOperationTracker::new();
        let guard = tracker.begin("queued exact-ID retry");
        mailbox
            .admit(Box::pin(async move {
                retry_admission(tokio_util::sync::CancellationToken::new(), || {
                    let number = attempt_count.fetch_add(1, Ordering::SeqCst);
                    let attempted = attempted.clone();
                    let prompt = retried.clone();
                    let permit = permit.clone();
                    async move {
                        attempted.send((number, prompt)).unwrap();
                        if number == 0 {
                            false
                        } else {
                            permit.notified().await;
                            true
                        }
                    }
                })
                .await;
                drop(guard);
            }))
            .unwrap();
        let (recalled, mut recall) = tokio::sync::oneshot::channel();
        mailbox
            .admit(Box::pin(async move {
                recalled.send(()).unwrap();
            }))
            .unwrap();
        let (_, first) = attempts.recv().await.unwrap();
        assert_eq!(first, prompt);
        assert!(recall.try_recv().is_err());
        assert_eq!(tracker.blockers().len(), 1);
        let (other, done) = tokio::sync::oneshot::channel();
        independent
            .admit(Box::pin(async move {
                other.send(()).unwrap();
            }))
            .unwrap();
        done.await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        let (number, second) = attempts.recv().await.unwrap();
        assert_eq!(number, 1);
        assert_eq!(second, prompt);
        assert!(
            recall.try_recv().is_err(),
            "recall must remain behind the unresolved exact-ID admission"
        );
        recovered.notify_one();
        recall.await.unwrap();
        assert!(tracker.blockers().is_empty());
    }
}
