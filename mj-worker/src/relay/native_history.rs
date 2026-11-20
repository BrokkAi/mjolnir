use super::*;
use mj_core::native_agent::NativeAgentEvent;

impl DurableRelay {
    /// Older snapshots predate the evidence that permits replacing an empty
    /// native session. Recover that evidence once, without replaying historical
    /// commands over the live snapshot or changing any journal frontiers.
    pub(super) fn recover_native_history_evidence(&mut self) -> bool {
        if self.snapshot.native_use_evidence.is_some() {
            return false;
        }
        if self.snapshot.native_session_used {
            self.snapshot.record_native_use(NativeSessionUse::Used);
            return true;
        }
        // Shipped snapshots already proved this identity was opened after the
        // retained floor and never used. Startup text is not contrary evidence.
        if self
            .snapshot
            .native_session_opened_ordinal
            .is_some_and(|opened| opened > self.snapshot.recovery_floor_ordinal)
        {
            let usage = if self
                .snapshot
                .dispatches
                .values()
                .any(RelayDispatchRecord::prompt_may_have_reached_agent)
            {
                self.snapshot.native_session_used = true;
                NativeSessionUse::Unknown
            } else {
                NativeSessionUse::Unused
            };
            self.snapshot.record_native_use(usage);
            return true;
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
                self.snapshot.record_native_use(NativeSessionUse::Unknown);
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
        self.snapshot
            .record_native_use(if self.snapshot.native_session_used {
                NativeSessionUse::Unknown
            } else {
                NativeSessionUse::Unused
            });
        if evidence.used {
            self.snapshot.record_native_use(NativeSessionUse::Used);
        }
        true
    }

    fn read_native_history_evidence(&self) -> Result<NativeHistoryEvidence> {
        let mut evidence = NativeHistoryEvidence::default();
        self.visit_legacy_evidence(|event| evidence.observe(event, &self.snapshot.dispatches))?;
        Ok(evidence)
    }

    pub(super) fn recover_pending_elicitations(&mut self) -> bool {
        let missing: BTreeSet<_> = self
            .snapshot
            .assessment_questions
            .iter()
            .filter(|id| !self.snapshot.pending_elicitations.contains_key(*id))
            .cloned()
            .collect();
        if missing.is_empty() {
            return false;
        }
        let mut recovered = BTreeMap::new();
        for event in &self.hot_events {
            if let RelayObservation::ElicitationRequested { request } = &event.observation
                && missing.contains(&request.id)
            {
                recovered.insert(request.id.clone(), request.clone());
            }
        }
        if recovered.len() != missing.len()
            && let Err(error) = self.visit_legacy_evidence(|event| {
                if let RelayObservation::ElicitationRequested { request } = event.observation
                    && missing.contains(&request.id)
                {
                    recovered.insert(request.id.clone(), request);
                }
            })
        {
            tracing::warn!(session_id = %self.snapshot.session_id,
                "could not read legacy pending question history; preserving unavailable IDs: {error:#}");
        }
        for id in missing {
            let body = recovered.remove(&id);
            if body.is_none() {
                tracing::warn!(session_id = %self.snapshot.session_id, question_id = %id,
                    "legacy pending question body is unavailable; the worker remains usable");
            }
            self.snapshot.pending_elicitations.insert(id, body);
        }
        true
    }

    fn visit_legacy_evidence(&self, mut visit: impl FnMut(RelayEvent)) -> Result<()> {
        let mut ordinal = self
            .snapshot
            .acknowledged_through
            .min(self.snapshot.recovery_floor_ordinal);
        let mut digest = if ordinal == self.snapshot.acknowledged_through {
            self.snapshot.acknowledged_digest.clone()
        } else {
            self.snapshot.recovery_floor_digest.clone()
        };
        let plan = self.legacy_native_replay_plan(ordinal, &digest);
        // Reuse bounded, digest-validated replay, including sealed segments.
        // This runs during worker startup, before it serves any requests.
        while ordinal < self.snapshot.latest_ordinal {
            let page = plan.read_events_after(ordinal, &digest, RELAY_REPLAY_BYTE_BUDGET)?;
            for event in page.events {
                visit(event);
            }
            ordinal = page.through_ordinal;
            digest = page.through_digest;
        }
        Ok(())
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
                        .is_none_or(RelayDispatchRecord::prompt_may_have_reached_agent);
            }
            RelayObservation::CommandCompleted {
                command_id,
                outcome,
                ..
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
                self.used |= !mj_core::acp::session_update_is_session_state(&update);
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
            RelayObservation::ManagedSubagentConfigured { .. }
            | RelayObservation::WorkLivenessChanged { .. }
            | RelayObservation::NativeAgent { .. }
            | RelayObservation::AgentInitialized { .. }
            | RelayObservation::SessionConfigured { .. }
            | RelayObservation::SessionModesConfigured { .. }
            | RelayObservation::ConfigurationUpdated { .. }
            | RelayObservation::CheckpointReady { .. }
            | RelayObservation::SessionRestarted
            | RelayObservation::SessionFault { .. }
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
            | RelayObservation::MailboxHookLeaseCreated { .. }
            | RelayObservation::MailboxHookLeaseReturned { .. }
            | RelayObservation::MailboxEventsDelivered { .. }
            | RelayObservation::Closing
            | RelayObservation::HandbackCheckStarted { .. }
            | RelayObservation::QuestionTargetUpdated { .. }
            | RelayObservation::QuestionLocatorFinished { .. }
            | RelayObservation::TurnAssessmentUpdated { .. }
            | RelayObservation::Closed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use agent_client_protocol::v1::schema::{ContentChunk, SessionInfoUpdate};

    fn remove_history_evidence(root: &Path) {
        let path = root.join(RELAY_STATE_FILE);
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved.as_object_mut().unwrap().remove("native_use_evidence");
        saved["format_version"] = serde_json::json!(27);
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
                replaced_unused_native_session_id: None,
            })
            .unwrap();
    }

    // Hard-won: b62fe3b: a legacy question below the recovery floor blocked worker upgrade.
    #[test]
    fn upgrade_keeps_a_pruned_legacy_question_id_without_blocking_worker_open() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        let question = mj_core::elicitation::ElicitationRequest::from_acp_params("pruned-question", serde_json::json!({
            "mode":"form", "sessionId":SESSION, "message":"Original question", "requestedSchema":{"type":"object", "properties":{}}
        })).unwrap();
        relay
            .record_observation(RelayObservation::ElicitationRequested {
                request: question.clone(),
            })
            .unwrap();
        let frontier = relay.latest_ordinal();
        let digest = relay.latest_digest().to_owned();
        acknowledge_relay(&mut relay, "old-acked-question", frontier);
        drop(relay);
        let path = temp.path().join(RELAY_STATE_FILE);
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["format_version"] = serde_json::json!(27);
        saved["recovery_floor_ordinal"] = serde_json::json!(frontier);
        saved["recovery_floor_digest"] = serde_json::json!(digest);
        saved
            .as_object_mut()
            .unwrap()
            .remove("pending_elicitations");
        fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
        for _ in 0..2 {
            let relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
            assert!(relay.snapshot.assessment_questions.contains(&question.id));
            assert_eq!(
                relay.snapshot.pending_elicitations.get(&question.id),
                Some(&None)
            );
            let baseline = relay.live_session_baseline().unwrap();
            assert_eq!(baseline.unavailable_questions, 0);
            assert!(baseline.questions.is_empty());
        }
        let mut relay = DurableRelay::open(temp.path(), SESSION, "upgraded").unwrap();
        relay
            .record_observation(RelayObservation::ElicitationRequested {
                request: question.clone(),
            })
            .unwrap();
        let response = relay.handle(relay_request(
            "reissued-question",
            RelayRequest::ReadPendingElicitation {
                elicitation_id: question.id.clone(),
            },
        ));
        assert!(
            matches!(response.body, RelayResponseBody::Ok { payload: RelayResponsePayload::PendingElicitation { request: Some(ref request) } } if request == &question)
        );
    }

    // Hard-won: 363ef03: ACK pruning discarded the only durable pending-question body.
    #[test]
    fn legacy_pending_question_survives_migration_pruning_and_worker_restart() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        let question = mj_core::elicitation::ElicitationRequest::from_acp_params("legacy-question", serde_json::json!({
            "mode":"form", "sessionId":SESSION, "message":"Which destination?", "requestedSchema":{"type":"object", "properties":{}}
        })).unwrap();
        relay
            .record_observation(RelayObservation::ElicitationRequested {
                request: question.clone(),
            })
            .unwrap();
        for _ in 0..40 {
            relay
                .record_observation(RelayObservation::Warning {
                    message: "x".repeat(64 * 1024),
                })
                .unwrap();
        }
        relay.persist_snapshot().unwrap();
        let frontier = relay.latest_ordinal();
        let digest = relay.latest_digest().to_owned();
        drop(relay);
        let path = temp.path().join(RELAY_STATE_FILE);
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["format_version"] = serde_json::json!(27);
        saved["acknowledged_through"] = serde_json::json!(frontier);
        saved["acknowledged_digest"] = serde_json::json!(digest);
        saved
            .as_object_mut()
            .unwrap()
            .remove("pending_elicitations");
        fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "new").unwrap();
        let acknowledged = acknowledge_relay(&mut relay, "prune-migrated-question", frontier);
        assert!(matches!(
            acknowledged.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Acknowledged { .. }
            }
        ));
        assert_eq!(
            fs::metadata(
                temp.path()
                    .join(RELAY_JOURNAL_DIR)
                    .join(RELAY_ACTIVE_SEGMENT)
            )
            .unwrap()
            .len(),
            0
        );
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), SESSION, "new").unwrap();
        let response = relay.handle(relay_request(
            "read-question",
            RelayRequest::ReadPendingElicitation {
                elicitation_id: question.id.clone(),
            },
        ));
        assert!(
            matches!(response.body, RelayResponseBody::Ok { payload: RelayResponsePayload::PendingElicitation { request: Some(ref recovered) } } if recovered == &question)
        );
        relay
            .record_observation(RelayObservation::ElicitationResolved {
                elicitation_id: question.id.clone(),
                action: "accept".into(),
                reply: None,
            })
            .unwrap();
        assert!(relay.snapshot.pending_elicitations.is_empty());
        relay
            .record_observation(RelayObservation::ElicitationRequested { request: question })
            .unwrap();
        relay
            .record_observation(RelayObservation::ElicitationsCleared)
            .unwrap();
        drop(relay);
        let relay = DurableRelay::open(temp.path(), SESSION, "new").unwrap();
        assert!(relay.snapshot.pending_elicitations.is_empty());
    }

    // Hard-won: 581ae9e: startup agent text overwrote an existing legacy unused proof.
    #[test]
    fn upgrade_preserves_existing_unused_proof_despite_startup_text() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        open_session(&mut relay, false);
        relay
            .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from("startup greeting"),
            )))
            .unwrap();
        relay.persist_snapshot().unwrap();
        drop(relay);
        let path = temp.path().join(RELAY_STATE_FILE);
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved.as_object_mut().unwrap().remove("native_use_evidence");
        saved["format_version"] = serde_json::json!(27);
        fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
        for _ in 0..2 {
            let relay = DurableRelay::open(temp.path(), SESSION, "new").unwrap();
            assert!(!relay.native_session_may_have_history());
            assert_eq!(
                relay.snapshot.native_use_evidence.as_ref().unwrap().usage,
                NativeSessionUse::Unused
            );
        }
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

    // Hard-won: 14b4b00de: restart recovery discarded the only evidence that a prompt reached the agent.
    #[test]
    fn restart_preserves_native_history_evidence_from_an_in_flight_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "old").unwrap();
        open_session(&mut relay, false);
        submit_relay(&mut relay, "sent-work", prompt("possibly delivered"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        assert_eq!(
            relay.snapshot.dispatches["sent-work"].state,
            RelayDispatchState::InFlight
        );
        // Claim-time durable uncertainty already protects the thread here.
        assert_eq!(
            relay.native_session_history(),
            NativeSessionHistory::MayHaveHistory
        );
        drop(relay);

        // Legacy claims had no durable use marker. Keep the opening and
        // in-flight dispatch so upgrade must recover that evidence before
        // interrupting the old command and compacting its payload.
        let path = temp.path().join(RELAY_STATE_FILE);
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["format_version"] = serde_json::json!(27);
        saved.as_object_mut().unwrap().remove("native_session_used");
        saved.as_object_mut().unwrap().remove("native_use_evidence");
        assert_eq!(saved["dispatches"]["sent-work"]["state"], "in_flight");
        fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

        // First open interrupts the claim, and the second loads the compacted snapshot.
        for _ in 0..2 {
            let relay = DurableRelay::open(temp.path(), SESSION, "restarted").unwrap();
            assert!(!relay.snapshot.dispatches.contains_key("sent-work"));
            assert_eq!(
                relay.native_session_history(),
                NativeSessionHistory::MayHaveHistory
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
                            work_facts: None,
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
