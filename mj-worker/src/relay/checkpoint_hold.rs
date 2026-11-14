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
//! State changes that are not journaled (agent terminals, background tasks,
//! the idle clock) need no hold ending: the daemon reads them from the live
//! operational state when it validates a checkpoint, not from the cut.
//!
//! When the barrier ends, the held writes are journaled in arrival order,
//! unless a Close was accepted at the cut. The sealed relay's archive ends at
//! the cut, so what the harness said after it is dropped rather than written
//! behind the seal.
use super::*;

/// One write that waits for the checkpoint barrier to end.
pub(super) enum HeldWrite {
    SessionUpdate(SessionUpdate),
    Observation(RelayObservation),
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
            .push_back(HeldWrite::Observation(observation));
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
        self.held_writes.push_back(HeldWrite::SessionUpdate(update));
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
                    self.record_session_update(update)?;
                }
                HeldWrite::Observation(observation) => {
                    self.record_observation(observation)?;
                }
            }
        }
        Ok(())
    }
}
