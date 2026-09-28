use super::*;

impl DurableRelay {
    pub fn handle(&mut self, envelope: RelayRequestEnvelope) -> RelayResponseEnvelope {
        let request_id = envelope.request_id.clone();
        let body = self
            .handle_inner(&envelope)
            .unwrap_or_else(|error| RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: format!("{error:#}"),
                    retryable: true,
                    detail: None,
                },
            });
        let protocol_version = match &body {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello { negotiated, .. },
            } => *negotiated,
            _ => envelope.protocol_version,
        };
        RelayResponseEnvelope {
            request_id,
            protocol_version,
            body,
        }
    }

    /// Everything answered before a request reaches relay state: a usable
    /// request ID, protocol negotiation, and a method this peer's protocol
    /// version admits. `Some` is the response to send instead of handling.
    /// [`Self::take_deferred_attach`] consults the same checks so an attach it
    /// defers is one the normal path would have accepted.
    pub(super) fn envelope_rejection(
        &self,
        envelope: &RelayRequestEnvelope,
    ) -> Option<RelayResponseBody> {
        if envelope.request_id.trim().is_empty() || envelope.request_id.len() > 256 {
            return Some(relay_error(
                RelayErrorCode::InvalidRequest,
                "request_id is required",
                false,
                None,
            ));
        }
        if let RelayRequest::Hello { supported, .. } = &envelope.request {
            let writer_range = RelayVersionRange {
                min: RELAY_PROTOCOL_VERSION,
                max: RELAY_PROTOCOL_VERSION,
            };
            let Some(negotiated) = writer_range.negotiate(*supported) else {
                return Some(relay_error(
                    RelayErrorCode::IncompatibleProtocol,
                    format!(
                        "controller supports {}-{}, relay supports protocol {}-{}",
                        supported.min, supported.max, writer_range.min, writer_range.max
                    ),
                    false,
                    None,
                ));
            };
            return Some(RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello {
                    negotiated,
                    relay_version: self.relay_version.clone(),
                    session_id: self.snapshot.session_id.clone(),
                    worker_build: self.worker_build.clone(),
                },
            });
        }
        mj_core::relay::protocol::relay_protocol_rejection(envelope)
    }

    fn handle_inner(&mut self, envelope: &RelayRequestEnvelope) -> Result<RelayResponseBody> {
        if let Some(body) = self.envelope_rejection(envelope) {
            return Ok(body);
        }

        if self.command_ledger_is_sealed()
            && matches!(
                envelope.request,
                RelayRequest::SubmitDurable { .. }
                    | RelayRequest::CancelCommandAdmission { .. }
                    | RelayRequest::ReleaseCommandReceipt { .. }
            )
        {
            return Ok(relay_error(
                RelayErrorCode::InvalidState,
                "command ledger is sealed for checkpoint transfer",
                true,
                None,
            ));
        }

        let payload = match &envelope.request {
            RelayRequest::Hello { .. } => unreachable!(),
            RelayRequest::CheckpointCommandLedger {
                through_ordinal,
                through_digest,
                seal,
                offset,
            } => {
                let barrier = self
                    .snapshot
                    .checkpoint_barrier
                    .clone()
                    .context("command ledger export requires an active checkpoint barrier")?;
                anyhow::ensure!(
                    self.snapshot.checkpoint_ready_through == Some(*through_ordinal)
                        && self.snapshot.checkpoint_ready_digest.as_ref() == Some(through_digest),
                    "command ledger export frontier changed"
                );
                let captures_cut = self
                    .checkpoint_command_ledger
                    .as_ref()
                    .is_none_or(|(id, _)| id != &barrier)
                    || (*seal && !self.command_ledger_is_sealed());
                if captures_cut {
                    anyhow::ensure!(
                        self.snapshot.latest_ordinal == *through_ordinal
                            && self.snapshot.latest_digest == *through_digest,
                        "command ledger export frontier changed before capture"
                    );
                }
                if *seal && !self.command_ledger_is_sealed() {
                    anyhow::ensure!(
                        *offset == 0,
                        "command ledger sealing must start at offset zero"
                    );
                    let mut next = self.snapshot.clone();
                    next.command_ledger_seal = Some(barrier.clone());
                    self.commit_snapshot(next)?;
                    self.checkpoint_command_ledger = None;
                }
                if self
                    .checkpoint_command_ledger
                    .as_ref()
                    .is_none_or(|(id, _)| id != &barrier)
                {
                    anyhow::ensure!(
                        *offset == 0,
                        "command ledger export was interrupted; restart from offset zero"
                    );
                    let ledger =
                        mj_core::relay::CheckpointCommandLedger::from_snapshot(&self.snapshot);
                    self.checkpoint_command_ledger = Some((barrier, serde_json::to_vec(&ledger)?));
                }
                let (_, bytes) = self
                    .checkpoint_command_ledger
                    .as_ref()
                    .expect("captured ledger");
                anyhow::ensure!(
                    *offset <= bytes.len(),
                    "command ledger offset is out of bounds"
                );
                let end = offset.saturating_add(256 * 1024).min(bytes.len());
                RelayResponsePayload::CheckpointCommandLedger {
                    data: bytes[*offset..end].to_vec(),
                    total_bytes: bytes.len(),
                }
            }
            RelayRequest::Attach {
                after_ordinal,
                after_digest,
            } => match self.attach(*after_ordinal, after_digest)? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::Acknowledge {
                through_ordinal,
                through_digest,
            } => match self.acknowledge(*through_ordinal, through_digest)? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::Submit {
                command_id,
                command,
            } => match self.submit_command(command_id, command.clone())? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::SubmitDurable {
                command_id,
                command,
            } => {
                if let Err(error) = validate_identifier(command_id, "command ID") {
                    return Ok(RelayResponseBody::Error {
                        error: relay_protocol_error(
                            RelayErrorCode::InvalidRequest,
                            error.to_string(),
                            false,
                            None,
                        ),
                    });
                }
                // Persist the caller's hold before admission. If acceptance or its
                // reply fails, GC still cannot erase the evidence needed to retry.
                let newly_retained = !self.snapshot.retained_command_receipts.contains(command_id);
                if newly_retained && self.snapshot.retained_command_receipts.len() >= 4096 {
                    return Ok(RelayResponseBody::Error {
                        error: relay_protocol_error(
                            RelayErrorCode::InvalidState,
                            "durable command receipt capacity reached; settle existing deliveries before retrying",
                            true,
                            None,
                        ),
                    });
                }
                if newly_retained {
                    let mut next = self.snapshot.clone();
                    next.retained_command_receipts.insert(command_id.clone());
                    self.commit_snapshot(next)?;
                }
                match self.submit_command(command_id, command.clone())? {
                    Ok(payload) => payload,
                    Err(error) => {
                        // An explicit rejection did not accept work. I/O errors
                        // instead leave the hold intact because delivery is uncertain.
                        if newly_retained {
                            let mut next = self.snapshot.clone();
                            next.retained_command_receipts.remove(command_id);
                            self.commit_snapshot(next)?;
                        }
                        return Ok(RelayResponseBody::Error { error });
                    }
                }
            }
            RelayRequest::CancelCommandAdmission { command_id } => {
                if let Err(error) = validate_identifier(command_id, "command ID") {
                    return Ok(RelayResponseBody::Error {
                        error: relay_protocol_error(
                            RelayErrorCode::InvalidRequest,
                            error.to_string(),
                            false,
                            None,
                        ),
                    });
                }
                if let Some(receipt) = self.snapshot.handled_commands.get(command_id) {
                    RelayResponsePayload::CommandReceipt {
                        receipt: Some(receipt.clone()),
                    }
                } else {
                    if !self
                        .snapshot
                        .cancelled_command_admissions
                        .contains(command_id)
                    {
                        // These are retained safety state, not an evictable cache:
                        // stale connections can submit even after receipt release
                        // or worker restart. Refuse new cancellations at the bound.
                        if self.snapshot.cancelled_command_admissions.len() >= 4096 {
                            return Ok(RelayResponseBody::Error {
                                error: relay_protocol_error(
                                    RelayErrorCode::InvalidState,
                                    "cancelled command admission capacity reached",
                                    false,
                                    None,
                                ),
                            });
                        }
                        let mut next = self.snapshot.clone();
                        next.cancelled_command_admissions.insert(command_id.clone());
                        next.retained_command_receipts.remove(command_id);
                        self.commit_snapshot(next)?;
                    }
                    RelayResponsePayload::CommandReceipt { receipt: None }
                }
            }
            RelayRequest::CommandReceipt { command_id } => RelayResponsePayload::CommandReceipt {
                receipt: self.snapshot.handled_commands.get(command_id).cloned(),
            },
            RelayRequest::ReleaseCommandReceipt { command_id } => {
                if self.snapshot.retained_command_receipts.contains(command_id) {
                    let mut next = self.snapshot.clone();
                    next.retained_command_receipts.remove(command_id);
                    self.commit_snapshot(next)?;
                }
                self.garbage_collect_relay_history()?;
                RelayResponsePayload::CommandReceiptReleased
            }
            RelayRequest::Status => {
                let state = self.operational_state();
                ensure_serialized_budget(
                    &state,
                    RELAY_STATE_BYTE_BUDGET,
                    "relay operational state",
                )?;
                RelayResponsePayload::Status(state)
            }
            RelayRequest::ReserveIdle { command_id } => {
                let already_accepted = self.snapshot.handled_commands.contains_key(command_id);
                let idle = self
                    .verdict_harness
                    .is_some_and(|harness| self.operational_state().safe_to_replace(harness));
                if !self.reviewer_admissions.is_empty() || (!already_accepted && !idle) {
                    RelayResponsePayload::IdleReservation { ordinal: None }
                } else {
                    match self.submit_command(
                        command_id,
                        RelayCommand::BeginCheckpoint {
                            reason: Some("idle worker replacement".into()),
                        },
                    )? {
                        Ok(RelayResponsePayload::Accepted { ordinal, .. }) => {
                            RelayResponsePayload::IdleReservation {
                                ordinal: Some(ordinal),
                            }
                        }
                        Ok(_) => unreachable!("barrier submission returns its accepted ordinal"),
                        Err(error) => return Ok(RelayResponseBody::Error { error }),
                    }
                }
            }
            RelayRequest::InstallPromptContext { text } => {
                self.install_prompt_context(text.clone())?;
                RelayResponsePayload::PromptContextInstalled
            }
            RelayRequest::AttachmentPresent { .. }
            | RelayRequest::InstallAttachment { .. }
            | RelayRequest::ReadAttachment { .. }
            | RelayRequest::CredentialState
            | RelayRequest::ReadCredentials
            | RelayRequest::InstallCredentials { .. }
            | RelayRequest::SkillsState
            | RelayRequest::InstallSkills { .. }
            | RelayRequest::GithubTokenState
            | RelayRequest::InstallGithubToken { .. }
            | RelayRequest::RemoveGithubToken
            | RelayRequest::ProjectMemorySnapshot
            | RelayRequest::HistoryQuery { .. }
            | RelayRequest::CompleteHistoryRequest { .. }
            | RelayRequest::InstallProjectMemorySnapshot { .. }
            | RelayRequest::CompleteSubagentRequest { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "connection-only requests must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
            RelayRequest::SubagentRequests => RelayResponsePayload::SubagentRequests {
                requests: Vec::new(),
                results: Vec::new(),
            },
            // A durable-only relay has no live history broker. The worker's
            // connection transport intercepts this when a broker is present.
            RelayRequest::HistoryRequests => RelayResponsePayload::HistoryRequests {
                requests: Vec::new(),
            },
            RelayRequest::RespondElicitation { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "elicitation responses must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
            RelayRequest::StopBackgroundTask { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "background task stops must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
            RelayRequest::Reviewer { .. } => {
                // The reviewer has its own relay and its own harness process.
                // Only the live worker transport owns both, so this relay
                // never answers for it.
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "reviewer requests must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
        };
        Ok(RelayResponseBody::Ok { payload })
    }

    pub fn install_prompt_context(&mut self, text: String) -> Result<()> {
        if text.trim().is_empty() {
            bail!("pending prompt context is empty");
        }
        if self.snapshot.active_prompt.is_some()
            || self
                .snapshot
                .pending_prompt_context
                .as_ref()
                .is_some_and(|context| context.attached_command_id.is_some())
        {
            bail!("cannot replace prompt context while its prompt is active");
        }
        let mut next = self.snapshot.clone();
        match next.pending_prompt_context.as_mut() {
            Some(context) if context.text != text => {
                context.text.push_str("\n\n");
                context.text.push_str(&text);
            }
            Some(_) => {}
            None => {
                next.pending_prompt_context = Some(PendingPromptContext {
                    text,
                    attached_command_id: None,
                });
            }
        }
        ensure_serialized_budget(
            &next,
            RELAY_SNAPSHOT_BYTE_BUDGET,
            "relay snapshot with pending prompt context",
        )?;
        self.commit_snapshot(next)
    }

    fn attach(
        &mut self,
        after_ordinal: u64,
        after_digest: &str,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        let state = self.operational_state();
        self.replay_plan()
            .attach(after_ordinal, after_digest, state)
    }

    fn acknowledge(
        &mut self,
        through_ordinal: u64,
        through_digest: &str,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        let plan = self.replay_plan();
        if let Err(error) = plan.validate_cursor(through_ordinal, through_digest) {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::Desynchronized,
                error.to_string(),
                false,
                Some(plan.desynchronized_detail(through_ordinal, through_digest)),
            )));
        }
        if through_ordinal > self.snapshot.acknowledged_through {
            let mut next_snapshot = self.snapshot.clone();
            next_snapshot.acknowledged_through = through_ordinal;
            next_snapshot.acknowledged_digest = through_digest.to_owned();
            // The acknowledgement becomes durable before any journal GC.
            self.commit_snapshot(next_snapshot)?;
        }
        // An earlier attempt may have durably advanced the ACK and then
        // failed while rewriting or pruning history. Retrying the exact ACK
        // must retry that cleanup instead of treating it as wholly complete.
        if through_ordinal == self.snapshot.acknowledged_through {
            self.garbage_collect_relay_history()?;
        }
        Ok(Ok(RelayResponsePayload::Acknowledged {
            through_ordinal: self.snapshot.acknowledged_through,
            through_digest: self.snapshot.acknowledged_digest.clone(),
        }))
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    use crate::relay::test_support::*;

    #[test]
    fn durable_receipt_survives_lost_reply_collection_and_restart_until_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        let request = || {
            relay_request(
                "durable-submit",
                RelayRequest::SubmitDurable {
                    command_id: "durable-command".into(),
                    command: prompt("run exactly once"),
                },
            )
        };
        let first = relay.handle(request());
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted { ordinal, .. },
        } = first.body
        else {
            panic!("{first:?}")
        };
        // The caller loses the acceptance response; the effect finishes anyway.
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        relay
            .record_command_completed(
                "durable-command",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                    diagnostic: None,
                },
            )
            .unwrap();
        let ready = ready_checkpoint(&mut relay, "receipt-checkpoint");
        submit_release(&mut relay, "release-checkpoint", "receipt-checkpoint");
        attach_relay(&mut relay, "receipt-attach", 0);
        let latest = relay.latest_ordinal();
        acknowledge_relay(&mut relay, "receipt-ack", latest);
        submit_floor(&mut relay, "receipt-floor", ready);
        assert!(relay.snapshot.retained_through() > ordinal);
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        let before = relay.latest_ordinal();
        assert!(matches!(
            relay.snapshot.handled_commands["durable-command"].outcome,
            Some(RelayCommandOutcome::Prompt { .. })
        ));

        assert!(matches!(relay.handle(request()).body,
            RelayResponseBody::Ok { payload: RelayResponsePayload::Accepted { ordinal: accepted, .. } } if accepted == ordinal));
        assert_eq!(relay.latest_ordinal(), before);
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        assert!(matches!(
            relay
                .handle(relay_request(
                    "lookup-receipt",
                    RelayRequest::CommandReceipt {
                        command_id: "durable-command".into()
                    }
                ))
                .body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::CommandReceipt { receipt: Some(_) }
            }
        ));
        for _ in 0..2 {
            assert!(matches!(
                relay
                    .handle(relay_request(
                        "release-receipt",
                        RelayRequest::ReleaseCommandReceipt {
                            command_id: "durable-command".into()
                        }
                    ))
                    .body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::CommandReceiptReleased
                }
            ));
        }
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        assert!(
            !relay
                .snapshot
                .retained_command_receipts
                .contains("durable-command")
        );
        assert!(matches!(
            relay
                .handle(relay_request(
                    "lookup-settled",
                    RelayRequest::CommandReceipt {
                        command_id: "durable-command".into()
                    }
                ))
                .body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::CommandReceipt { receipt: None }
            }
        ));
    }

    #[test]
    fn rejected_configuration_receipt_keeps_failure_after_journal_collection() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        let response = relay.handle(relay_request(
            "submit-config",
            RelayRequest::SubmitDurable {
                command_id: "durable-config".into(),
                command: set_config("model", "missing"),
            },
        ));
        assert!(matches!(
            response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Accepted { .. }
            }
        ));
        relay.claim_pending_commands(true).unwrap();
        relay
            .record_command_rejected(
                "durable-config",
                mj_core::event_outcome::OutcomeReason::AdmissionRejected,
                "unknown model",
            )
            .unwrap();
        let ready = ready_checkpoint(&mut relay, "config-checkpoint");
        submit_release(&mut relay, "release-config-checkpoint", "config-checkpoint");
        attach_relay(&mut relay, "config-attach", 0);
        let latest = relay.latest_ordinal();
        acknowledge_relay(&mut relay, "config-ack", latest);
        submit_floor(&mut relay, "config-floor", ready);
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        let response = relay.handle(relay_request(
            "lookup-config",
            RelayRequest::CommandReceipt {
                command_id: "durable-config".into(),
            },
        ));
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::CommandReceipt {
                    receipt: Some(receipt),
                },
        } = response.body
        else {
            panic!("{response:?}")
        };
        assert!(receipt.terminal_ordinal.is_some());
        assert!(receipt.outcome.is_none());
        assert_eq!(receipt.failure.as_deref(), Some("unknown model"));
    }

    #[test]
    fn admission_cancellation_fences_late_submits_after_release_and_restart() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        // A new connection cancels before the old connection's buffered submit
        // reaches the relay owner. None is now a durable admission decision.
        let cancel = || {
            relay_request(
                "cancel-admission",
                RelayRequest::CancelCommandAdmission {
                    command_id: "cancelled-command".into(),
                },
            )
        };
        for _ in 0..2 {
            assert!(matches!(
                relay.handle(cancel()).body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::CommandReceipt { receipt: None }
                }
            ));
        }
        relay.handle(relay_request(
            "release-cancelled",
            RelayRequest::ReleaseCommandReceipt {
                command_id: "cancelled-command".into(),
            },
        ));
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        for request in [
            RelayRequest::SubmitDurable {
                command_id: "cancelled-command".into(),
                command: prompt("old buffered submission"),
            },
            RelayRequest::Submit {
                command_id: "cancelled-command".into(),
                command: prompt("old buffered submission"),
            },
        ] {
            assert!(matches!(
                relay.handle(relay_request("late-submit", request)).body,
                RelayResponseBody::Error {
                    error: RelayProtocolError {
                        retryable: false,
                        ..
                    }
                }
            ));
        }
        assert!(relay.snapshot.handled_commands.is_empty());
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    }

    #[test]
    fn admission_cancellation_reports_work_that_won_the_race() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        let response = relay.handle(relay_request(
            "winning-submit",
            RelayRequest::SubmitDurable {
                command_id: "winning-command".into(),
                command: prompt("accepted work"),
            },
        ));
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted { ordinal, .. },
        } = response.body
        else {
            panic!("{response:?}")
        };
        let response = relay.handle(relay_request(
            "late-cancel",
            RelayRequest::CancelCommandAdmission {
                command_id: "winning-command".into(),
            },
        ));
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::CommandReceipt {
                    receipt: Some(receipt),
                },
        } = response.body
        else {
            panic!("{response:?}")
        };
        assert_eq!(receipt.accepted_ordinal, ordinal);
        assert!(relay.snapshot.cancelled_command_admissions.is_empty());
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    }

    #[test]
    fn old_protocol_cannot_accept_a_durable_command_without_retention() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        let mut request = relay_request(
            "old-submit",
            RelayRequest::SubmitDurable {
                command_id: "durable-command".into(),
                command: prompt("work"),
            },
        );
        request.protocol_version = 25;
        assert!(matches!(
            relay.handle(request).body,
            RelayResponseBody::Error { .. }
        ));
        assert!(relay.snapshot.handled_commands.is_empty());
        assert!(relay.snapshot.retained_command_receipts.is_empty());
    }
}
