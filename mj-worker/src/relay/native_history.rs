use super::*;
use mj_core::native_agent::NativeAgentEvent;

impl DurableRelay {
    /// Older snapshots predate the evidence that permits replacing an empty
    /// native session. Recover that evidence once, without replaying historical
    /// commands over the live snapshot or changing any journal frontiers.
    pub(super) fn recover_native_history_evidence(&mut self) -> bool {
        if self.snapshot.native_session_used {
            return false;
        }
        let evidence = match self.read_native_history_evidence() {
            Ok(evidence) => evidence,
            Err(error) => {
                // Old sealed history may be damaged even while the current
                // native conversation is usable. Keep resume possible, but do
                // not authorize an empty replacement from unproven evidence.
                tracing::warn!(
                    session_id = self.snapshot.session_id,
                    "could not recover native session history evidence: {error:#}"
                );
                self.snapshot.native_session_used = true;
                return true;
            }
        };
        self.snapshot.native_session_opened_ordinal = evidence.opened.and_then(|(id, ordinal)| {
            (Some(id.as_str()) == self.snapshot.native_session_id.as_deref()).then_some(ordinal)
        });
        self.snapshot.native_session_used = evidence.used
            || self
                .snapshot
                .native_session_opened_ordinal
                .is_none_or(|opened| opened <= self.snapshot.recovery_floor_ordinal);
        true
    }

    fn read_native_history_evidence(&self) -> Result<NativeHistoryEvidence> {
        let plan = self.replay_plan();
        let mut ordinal = self.snapshot.retained_through();
        let mut digest = self.snapshot.retained_digest().to_owned();
        let mut evidence = NativeHistoryEvidence::default();
        // Reuse bounded, digest-validated replay, including sealed segments.
        // This runs during worker startup, before it serves any requests.
        while ordinal < self.snapshot.latest_ordinal {
            let page = plan.read_events_after(ordinal, &digest, RELAY_REPLAY_BYTE_BUDGET)?;
            for event in page.events {
                evidence.observe(event, &self.snapshot.dispatches);
            }
            ordinal = page.through_ordinal;
            digest = page.through_digest;
        }
        Ok(evidence)
    }
}

#[derive(Default)]
struct NativeHistoryEvidence {
    opened: Option<(String, u64)>,
    used: bool,
    // Keep identities and kinds only; queued prompt bodies can be large.
    commands: BTreeMap<String, RelayCommandKind>,
}

impl NativeHistoryEvidence {
    fn observe(&mut self, event: RelayEvent, dispatches: &BTreeMap<String, RelayDispatchRecord>) {
        match event.observation {
            RelayObservation::SessionOpened {
                native_session_id,
                resumed,
                ..
            } => {
                self.opened = Some((native_session_id, event.ordinal));
                self.used |= resumed;
            }
            RelayObservation::CommandQueued {
                command_id,
                command,
                ..
            } => {
                self.commands.insert(command_id, command.kind());
            }
            RelayObservation::CommandStarted { command_id, .. } => {
                // An unknown command may have been queued below the retained
                // frontier; its absence cannot prove that nothing was sent.
                let could_be_prompt = self
                    .commands
                    .get(&command_id)
                    .is_none_or(|kind| *kind == RelayCommandKind::Prompt);
                // CommandStarted promotes a prompt to Pending before ACP
                // receives it. The saved dispatch state can still prove that
                // this accepted work has never left the worker's queue.
                self.used |= could_be_prompt
                    && dispatches
                        .get(&command_id)
                        .is_none_or(prompt_may_have_reached_agent);
            }
            RelayObservation::CommandCompleted {
                command_id,
                outcome,
            } => {
                self.commands.remove(&command_id);
                match outcome {
                    RelayCommandOutcome::ContextCleared {
                        native_session_id, ..
                    } => {
                        self.opened = Some((native_session_id, event.ordinal));
                        self.used = false;
                    }
                    RelayCommandOutcome::Prompt { .. } | RelayCommandOutcome::Steered { .. } => {
                        self.used = true;
                    }
                    _ => {}
                }
            }
            RelayObservation::CommandInterrupted {
                command_id,
                command,
                ..
            } => {
                self.commands.remove(&command_id);
                self.used |= command == RelayCommandKind::Prompt;
            }
            RelayObservation::CommandRejected { command_id, .. } => {
                self.commands.remove(&command_id);
            }
            RelayObservation::SessionUpdate { update } => {
                self.used |= mj_core::acp::session_update_has_native_history(&update);
            }
            RelayObservation::NativeAgent {
                event:
                    NativeAgentEvent::Spawned { .. }
                    | NativeAgentEvent::State { .. }
                    | NativeAgentEvent::Update { .. },
            }
            | RelayObservation::HarnessTurnStarted { .. }
            | RelayObservation::PermissionAutoApproved { .. }
            | RelayObservation::ElicitationRequested { .. } => self.used = true,
            RelayObservation::NativeAgent { .. }
            | RelayObservation::AgentInitialized { .. }
            | RelayObservation::SessionConfigured { .. }
            | RelayObservation::SessionModesConfigured { .. }
            | RelayObservation::ConfigurationUpdated { .. }
            | RelayObservation::CheckpointReady { .. }
            | RelayObservation::SessionRestarted
            | RelayObservation::Warning { .. }
            | RelayObservation::Notice { .. }
            | RelayObservation::ElicitationResolved { .. }
            | RelayObservation::ElicitationsCleared
            | RelayObservation::UserShellOutput { .. }
            | RelayObservation::TerminalOutput { .. }
            | RelayObservation::SteeringUnconfirmed { .. }
            | RelayObservation::RetryAssessmentStarted { .. }
            | RelayObservation::RetryAssessmentResolved { .. }
            | RelayObservation::HarnessTurnSettled { .. }
            | RelayObservation::Closing
            | RelayObservation::Closed => {}
        }
    }
}

pub(super) fn prompt_may_have_reached_agent(dispatch: &RelayDispatchRecord) -> bool {
    dispatch.command.prompt_blocks().is_some()
        && !matches!(
            dispatch.state,
            RelayDispatchState::Queued | RelayDispatchState::Pending
        )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use agent_client_protocol::schema::v1::{ContentChunk, SessionInfoUpdate};

    fn remove_history_evidence(root: &Path) {
        let path = root.join(RELAY_STATE_FILE);
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved.as_object_mut().unwrap().remove("native_session_used");
        saved
            .as_object_mut()
            .unwrap()
            .remove("native_session_opened_ordinal");
        fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
    }

    fn open_session(relay: &mut DurableRelay, resumed: bool) {
        relay
            .record_observation(RelayObservation::SessionOpened {
                native_session_id: "legacy-thread".into(),
                resumed,
                native_continuity_lost: false,
            })
            .unwrap();
    }

    #[test]
    fn upgrade_recovers_unused_native_history_from_sealed_journal_and_keeps_queued_work() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        open_session(&mut relay, false);
        // Put the opening outside both the snapshot tail and the hot window.
        for _ in 0..40 {
            relay
                .record_observation(RelayObservation::Warning {
                    message: "x".repeat(64 * 1024),
                })
                .unwrap();
        }
        relay
            .record_observation(RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new())),
            })
            .unwrap();
        submit_relay(&mut relay, "pending-work", prompt("preserve this work"));
        let frontier = (relay.latest_ordinal(), relay.latest_digest().to_owned());
        let commands = relay.snapshot.dispatches.clone();
        relay.persist_snapshot().unwrap();
        drop(relay);
        remove_history_evidence(temp.path());

        for _ in 0..2 {
            let relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
            assert!(!relay.native_session_may_have_history());
            assert_eq!(relay.snapshot.native_session_opened_ordinal, Some(1));
            assert_eq!(
                (relay.latest_ordinal(), relay.latest_digest()),
                (frontier.0, frontier.1.as_str())
            );
            assert_eq!(relay.snapshot.dispatches, commands);
            assert_eq!(
                relay.snapshot.native_session_id.as_deref(),
                Some("legacy-thread")
            );
        }
    }

    #[test]
    fn upgrade_does_not_replace_native_history_after_a_prompt_or_agent_content() {
        for prompt_was_sent in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
            open_session(&mut relay, false);
            if prompt_was_sent {
                submit_relay(&mut relay, "sent-work", prompt("already delivered"));
                relay.claim_pending_commands(true).unwrap();
                relay
                    .record_command_completed(
                        "sent-work",
                        RelayCommandOutcome::Prompt {
                            stop_reason: "end_turn".into(),
                            diagnostic: None,
                            usage: None,
                        },
                    )
                    .unwrap();
            } else {
                relay
                    .record_observation(RelayObservation::SessionUpdate {
                        update: Box::new(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::from("existing conversation"),
                        ))),
                    })
                    .unwrap();
            }
            relay.persist_snapshot().unwrap();
            drop(relay);
            remove_history_evidence(temp.path());
            let relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
            assert!(relay.native_session_may_have_history());
        }
    }

    #[test]
    fn upgrade_keeps_resumed_or_archived_native_history_conservative() {
        for resumed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
            open_session(&mut relay, resumed);
            if !resumed {
                let cursor = ready_checkpoint(&mut relay, "checkpoint");
                submit_floor(&mut relay, "archived", cursor);
            }
            relay.persist_snapshot().unwrap();
            drop(relay);
            remove_history_evidence(temp.path());
            let relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
            assert!(relay.native_session_may_have_history());
        }
    }

    #[test]
    fn upgrade_keeps_unreadable_native_history_conservative_without_blocking_resume() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        open_session(&mut relay, false);
        // Startup only needs the latest segments; migration also needs the
        // oldest one. Damage there must not authorize an empty replacement.
        for _ in 0..80 {
            relay
                .record_observation(RelayObservation::Warning {
                    message: "x".repeat(64 * 1024),
                })
                .unwrap();
        }
        relay.persist_snapshot().unwrap();
        let oldest = relay.journal_spans.first().unwrap().path.clone();
        assert_eq!(oldest.extension().unwrap(), "gz");
        drop(relay);
        remove_history_evidence(temp.path());
        fs::write(oldest, b"invalid compressed journal").unwrap();
        for _ in 0..2 {
            let relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
            assert!(relay.native_session_may_have_history());
            assert_eq!(
                relay.snapshot.native_session_id.as_deref(),
                Some("legacy-thread")
            );
        }
    }

    #[test]
    fn upgrade_recovers_an_empty_session_opened_after_an_archived_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        let cursor = ready_checkpoint(&mut relay, "checkpoint");
        submit_floor(&mut relay, "archived", cursor);
        open_session(&mut relay, false);
        relay.persist_snapshot().unwrap();
        drop(relay);
        remove_history_evidence(temp.path());
        let relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
        assert!(!relay.native_session_may_have_history());
    }
}
