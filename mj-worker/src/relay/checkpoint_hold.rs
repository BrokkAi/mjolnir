//! The worker's own journal writes wait while a checkpoint barrier holds its
//! cut.
//!
//! A ready barrier names the cut a checkpoint archives: the relay frontier at
//! its `CheckpointReady` event. The daemon exports that cut and, for a close,
//! seals the relay at exactly that frontier. The barrier already freezes
//! command dispatch; this freezes the rest of what the worker writes on its
//! own, so the worker is the only owner of the cut and nothing it does can
//! move it between the daemon's look and its Close (campaign finding I2-3).
//!
//! What waits is output that changes no agent work: harness notifications,
//! notices and warnings, native-agent reports. The relay decides this at the
//! one place every journal write passes, `append_relay_event`: anything it is
//! about to journal other than those, and other than the barrier's own
//! commands and a Close sealed at the cut, ends the hold, with the held
//! writes journaled first in their order. A runtime event that journals
//! nothing therefore leaves the hold alone. A turn the agent starts, a goal
//! change, a question for a person or a restart spoils the cut anyway: a
//! routine checkpoint sees the new harness turn and abandons its archive, and
//! a close is refused and returns the session to work, exactly as before the
//! hold existed.
//!
//! Background work that starts after the cut (an agent terminal, an adapter
//! task) journals nothing, so the frontier cannot show it. The relay sees it
//! where all background work changes, `persist_activity_transition`, and
//! spoils the barrier's cut: the hold ends and a Close at the cut is refused,
//! like a Close after a journaled turn. Work already running when the barrier
//! became ready does not spoil it; a Move that accepted interrupting it
//! closes over it on purpose. The mark is process-local, like the work:
//! terminals and adapter tasks do not survive a worker restart, and work
//! that starts after one is seen again.
//!
//! When the barrier ends, the held writes are journaled in arrival order,
//! unless a Close was accepted at the cut. The sealed relay's archive ends at
//! the cut, so what the harness said after it is dropped rather than written
//! behind the seal.
use super::*;

/// One write that waits for the checkpoint barrier to end.
pub(super) enum HeldWrite {
    // The schema `SessionUpdate` is several hundred bytes. Keep the queue
    // node small so a hold does not copy that payload on every push.
    SessionUpdate(Box<SessionUpdate>),
    Observation(Box<RelayObservation>),
}

/// Whether an observation can wait behind a checkpoint cut. Everything not
/// listed is journaled at once and ends the hold, so a new kind of observation
/// is never delayed until someone decides it can wait.
fn observation_can_wait(observation: &RelayObservation) -> bool {
    matches!(
        observation,
        RelayObservation::SessionUpdate { .. }
            | RelayObservation::Notice { .. }
            | RelayObservation::Warning { .. }
            | RelayObservation::NativeAgent { .. }
    )
}

impl DurableRelay {
    /// Whether the worker's own writes wait for the checkpoint barrier: a
    /// barrier is ready, and nothing has ended its hold early.
    pub fn worker_writes_held(&self) -> bool {
        self.snapshot.checkpoint_ready_through.is_some()
            && self.snapshot.checkpoint_barrier.is_some()
            && self.hold_ended_for != self.snapshot.checkpoint_barrier
    }

    /// Why a Close at this cut must be refused, or `None` when the cut is
    /// still the one the barrier made. The only place Close acceptance reads
    /// the cut: the barrier, its ready cursor, the journal frontier, and
    /// whether work started after it.
    pub(super) fn close_cut_refusal(
        &self,
        barrier_command_id: &str,
        expected: &RelayCursor,
    ) -> Option<&'static str> {
        if !self.at_checkpoint_cut(barrier_command_id, expected) {
            return Some("close does not match the current checkpoint cut");
        }
        (self.cut_spoiled_for.as_deref() == self.snapshot.checkpoint_barrier.as_deref()).then_some(
            "background work started after the checkpoint cut or checkpoint state changed",
        )
    }

    /// Whether relay state is still exactly the cut this barrier made: the
    /// barrier, its ready cursor, and a journal frontier that has not moved.
    fn at_checkpoint_cut(&self, barrier_command_id: &str, expected: &RelayCursor) -> bool {
        let ready = self
            .snapshot
            .checkpoint_ready_through
            .zip(self.snapshot.checkpoint_ready_digest.as_ref());
        self.snapshot.checkpoint_barrier.as_deref() == Some(barrier_command_id)
            && ready.is_some_and(|(ordinal, digest)| {
                ordinal == expected.ordinal && digest == &expected.digest
            })
            && self.snapshot.latest_ordinal == expected.ordinal
            && self.snapshot.latest_digest == expected.digest
    }

    /// The mail this relay holds at a ready barrier's cut, for the checkpoint
    /// that archives the cut. Events leased to a hook come back first, where
    /// returning the lease would put them: the hook never confirmed that the
    /// agent saw them. The outer error is a frame-size failure; the inner one
    /// is a refusal because the relay is no longer at that cut.
    pub(super) fn checkpoint_mailbox(
        &self,
        barrier_command_id: &str,
        cut: &RelayCursor,
    ) -> Result<std::result::Result<mj_core::archive::CanonicalMailbox, &'static str>> {
        if !self.at_checkpoint_cut(barrier_command_id, cut) {
            return Ok(Err("the relay is no longer at this checkpoint cut"));
        }
        let mut pending_events = self
            .snapshot
            .mailbox_hook_lease
            .as_ref()
            .map(|lease| lease.events.clone())
            .unwrap_or_default();
        pending_events.extend(self.snapshot.pending_mailbox_events.iter().cloned());
        let mailbox = mj_core::archive::CanonicalMailbox {
            pending_events,
            delivered_event_keys: self.snapshot.delivered_mailbox_event_keys.clone(),
            prompt_contexts: self.snapshot.mailbox_prompt_contexts.clone(),
        };
        // The answer travels in one relay frame, which the relay snapshot that
        // holds these fields may exceed. Refuse with an answer rather than a
        // frame the transport would drop.
        ensure_serialized_budget(
            &mailbox,
            mj_core::relay::MAX_FRAME_BYTES - RELAY_EVENT_ENVELOPE_RESERVE,
            "checkpoint mailbox",
        )?;
        Ok(Ok(mailbox))
    }

    /// The worker-owned native-use evidence and pending context at the same cut.
    pub(super) fn checkpoint_native_use(
        &self,
        barrier_command_id: &str,
        cut: &RelayCursor,
    ) -> std::result::Result<
        (
            Option<mj_core::relay::NativeSessionUseEvidence>,
            Option<&str>,
        ),
        &'static str,
    > {
        if !self.at_checkpoint_cut(barrier_command_id, cut)
            || self.cut_spoiled_for.as_deref() == Some(barrier_command_id)
        {
            return Err("the relay is no longer at this checkpoint cut");
        }
        Ok((
            self.snapshot.native_use_evidence.clone(),
            self.snapshot
                .pending_prompt_context
                .as_ref()
                .map(|context| context.text.as_str()),
        ))
    }

    /// Spoil the ready barrier's cut for changes the frontier cannot show.
    pub(super) fn spoil_checkpoint_cut(&mut self) -> Result<()> {
        if self.snapshot.checkpoint_ready_through.is_none() {
            return Ok(());
        }
        let Some(barrier) = self.snapshot.checkpoint_barrier.clone() else {
            return Ok(());
        };
        self.cut_spoiled_for = Some(barrier);
        self.end_checkpoint_hold()
    }

    /// The background work running now: the facts `checkpoint_blocker` reads.
    fn current_background_work(&self) -> BTreeSet<String> {
        let mut current: BTreeSet<String> = self
            .background_commands()
            .into_iter()
            .map(|command| command.id)
            .collect();
        current.extend(
            self.active_agent_terminals
                .keys()
                .map(|id| format!("terminal:{id}")),
        );
        current
    }

    /// Take the work running as the barrier makes its cut as the baseline, so
    /// only work that starts after the cut spoils it, however the earlier
    /// work arrived.
    pub(super) fn baseline_background_work_at_cut(&mut self) {
        self.known_background_work = self.current_background_work();
    }

    /// Compare the background work now with the work last seen. Work that is
    /// new spoils the cut of a ready barrier.
    pub(super) fn note_background_work(&mut self) -> Result<()> {
        let current = self.current_background_work();
        let started = !current.is_subset(&self.known_background_work);
        self.known_background_work = current;
        if started {
            self.spoil_checkpoint_cut()?;
        }
        Ok(())
    }

    /// The agent is working or the harness changed, so this barrier's cut can
    /// no longer describe an idle session. Journal what waited, in order, and
    /// stop holding for this barrier.
    pub fn end_checkpoint_hold(&mut self) -> Result<()> {
        if !self.worker_writes_held() {
            return Ok(());
        }
        self.hold_ended_for = self.snapshot.checkpoint_barrier.clone();
        self.replay_held_writes()
    }

    /// Called by [`Self::append_relay_event`] before it writes. Whatever the
    /// relay journals now ends the hold, unless it is part of the cut's own
    /// protocol: the barrier's commands and a Close that is sealed at the cut.
    /// Held writes are replayed first, so they stay ahead of this event, and
    /// their own appends find the hold already ended.
    pub(super) fn end_checkpoint_hold_for(
        &mut self,
        command_id: Option<&str>,
        observation: &RelayObservation,
    ) -> Result<()> {
        if !self.worker_writes_held()
            || self.close_pending()
            || self.is_checkpoint_control(command_id, observation)
        {
            return Ok(());
        }
        self.end_checkpoint_hold()
    }

    /// Whether the event belongs to a checkpoint barrier or Close command.
    /// These are how the cut is made, completed and sealed, not work that
    /// spoils it.
    fn is_checkpoint_control(
        &self,
        command_id: Option<&str>,
        observation: &RelayObservation,
    ) -> bool {
        let command = match observation {
            RelayObservation::CommandQueued { command, .. } => Some(command),
            _ => command_id
                .and_then(|id| self.snapshot.dispatches.get(id))
                .map(|dispatch| &dispatch.command),
        };
        matches!(
            command,
            Some(
                RelayCommand::BeginCheckpoint { .. }
                    | RelayCommand::CompleteCheckpoint { .. }
                    | RelayCommand::ReleaseCheckpoint { .. }
                    | RelayCommand::Close { .. }
            )
        )
    }

    /// Journal the writes that waited once their barrier has ended. A relay
    /// sealed at the cut drops them instead: nothing after the cut belongs to
    /// the archive the close installs.
    pub(crate) fn settle_held_writes(&mut self) -> Result<()> {
        if self.held_writes.is_empty() || self.worker_writes_held() {
            return Ok(());
        }
        if self.close_pending() {
            tracing::info!(
                session_id = %self.snapshot.session_id,
                dropped = self.held_writes.len(),
                "dropped worker writes held behind the cut a close sealed"
            );
            self.held_writes.clear();
            return Ok(());
        }
        self.replay_held_writes()
    }

    /// Hold an observation behind the checkpoint cut, or end the hold when it
    /// cannot wait. Hands the observation back when it is to be journaled now.
    pub(super) fn hold_observation(
        &mut self,
        observation: RelayObservation,
    ) -> Result<Option<RelayObservation>> {
        if !self.worker_writes_held() {
            return Ok(Some(observation));
        }
        if !observation_can_wait(&observation) {
            self.end_checkpoint_hold()?;
            return Ok(Some(observation));
        }
        self.held_writes
            .push_back(HeldWrite::Observation(Box::new(observation)));
        Ok(None)
    }

    /// Hold a harness notification behind the checkpoint cut, or end the hold
    /// when it shows the agent at work or changes the session's goal. Hands
    /// the update back when it is to be journaled now.
    pub(super) fn hold_session_update(
        &mut self,
        update: SessionUpdate,
    ) -> Result<Option<SessionUpdate>> {
        if !self.worker_writes_held() {
            return Ok(Some(update));
        }
        if self.session_update_needs_the_journal_now(&update)? {
            self.end_checkpoint_hold()?;
            return Ok(Some(update));
        }
        self.held_writes
            .push_back(HeldWrite::SessionUpdate(Box::new(update)));
        Ok(None)
    }

    /// The same decisions `record_session_update` makes, asked before it makes
    /// them: a goal change, a Codex goal that starts running, or Claude output
    /// that opens a turn of the harness's own.
    fn session_update_needs_the_journal_now(&self, update: &SessionUpdate) -> Result<bool> {
        let mut goal = self.snapshot.goal.clone();
        goal.apply(update)?;
        if goal != self.snapshot.goal {
            return Ok(true);
        }
        let claude = self.harness_turns == HarnessTurnPolicy::ClaudeAdapter;
        Ok(claude
            && !self.is_claude_stop_acknowledgement(update)
            && self.notice_outside_any_turn(update).is_none()
            && self.opens_harness_turn(update))
    }

    fn replay_held_writes(&mut self) -> Result<()> {
        for write in std::mem::take(&mut self.held_writes) {
            match write {
                HeldWrite::SessionUpdate(update) => {
                    self.record_session_update(*update)?;
                }
                HeldWrite::Observation(observation) => {
                    self.record_observation(*observation)?;
                }
            }
        }
        Ok(())
    }
}
