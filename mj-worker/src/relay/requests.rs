use super::*;

impl DurableRelay {
    /// Validate the authoritative scan and reserve its idle state under one lock.
    #[cfg(unix)]
    pub(crate) fn reserve_idle_after_antigravity_refresh(
        &mut self,
        envelope: RelayRequestEnvelope,
        synced_epoch: Option<u64>,
    ) -> RelayResponseEnvelope {
        if let Some(body) = self.envelope_rejection(&envelope) {
            return RelayResponseEnvelope {
                request_id: envelope.request_id,
                protocol_version: envelope.protocol_version,
                body,
            };
        }
        let RelayRequest::ReserveIdle { command_id } = &envelope.request else {
            unreachable!("idle reservation only")
        };
        if !self.has_handled_command(command_id)
            && (synced_epoch.is_none()
                || (self.background_work == BackgroundWorkPolicy::AntigravityTasks
                    && synced_epoch != Some(self.antigravity_hook_epoch())))
        {
            return RelayResponseEnvelope {
                request_id: envelope.request_id,
                protocol_version: envelope.protocol_version,
                body: RelayResponseBody::Ok {
                    payload: RelayResponsePayload::IdleReservation { ordinal: None },
                },
            };
        }
        self.handle(envelope)
    }

    pub fn handle(&mut self, envelope: RelayRequestEnvelope) -> RelayResponseEnvelope {
        self.handle_with_antigravity_stop_baseline(envelope, None)
    }

    /// The outer option says whether the live worker read SQLite. A completed
    /// read can report that the row did not exist.
    pub(crate) fn handle_with_antigravity_stop_baseline(
        &mut self,
        envelope: RelayRequestEnvelope,
        observed_baseline_digest: Option<Option<[u8; 32]>>,
    ) -> RelayResponseEnvelope {
        let request_id = envelope.request_id.clone();
        let body = self
            .handle_inner(&envelope, observed_baseline_digest)
            .unwrap_or_else(|error| RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: format!("{error:#}"),
                    retryable: true,
                    detail: None,
                },
            });
        // A completed or released barrier ends the hold on the worker's own
        // writes. The request itself already succeeded, so a journal failure
        // here is reported rather than turned into its answer.
        if let Err(error) = self.settle_held_writes() {
            tracing::error!(
                session_id = %self.snapshot.session_id,
                "could not journal writes held behind a checkpoint barrier: {error:#}"
            );
        }
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
    pub(crate) fn envelope_rejection(
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

    fn handle_inner(
        &mut self,
        envelope: &RelayRequestEnvelope,
        observed_baseline_digest: Option<Option<[u8; 32]>>,
    ) -> Result<RelayResponseBody> {
        if let Some(body) = self.envelope_rejection(envelope) {
            return Ok(body);
        }

        let payload = match &envelope.request {
            RelayRequest::Hello { .. } => unreachable!(),
            RelayRequest::AntigravityStop {
                conversation_id,
                fully_idle,
                termination_reason,
            } => {
                self.antigravity_stop(
                    conversation_id.as_deref(),
                    *fully_idle,
                    termination_reason.as_deref(),
                    observed_baseline_digest,
                )?;
                RelayResponsePayload::AntigravityStopObserved
            }
            RelayRequest::Attach {
                after_ordinal,
                after_digest,
                known_state_digest,
            } => match self.attach(*after_ordinal, after_digest, known_state_digest.as_deref())? {
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
            RelayRequest::HoldInPlaceMoveDispatch {
                operation_id,
                expected_store_id,
            } => match self.hold_in_place_move_dispatch(operation_id, expected_store_id)? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::DiscardHeldMoveCommands {
                operation_id,
                expected_store_id,
                command_ids,
            } => match self.discard_held_move_commands(
                operation_id,
                expected_store_id,
                command_ids,
            )? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::SubmitFront {
                operation_id,
                expected_store_id,
                command_id,
                command,
            } => match self.submit_front_command(
                operation_id,
                expected_store_id,
                command_id,
                command.clone(),
            )? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::ReleaseInPlaceMoveDispatch {
                expected_harness_restarted,
                operation_id,
                expected_store_id,
            } => match self.release_in_place_move_dispatch(
                operation_id,
                expected_store_id,
                *expected_harness_restarted,
            )? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::RestartHarnessInPlace { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidRequest,
                    "restart_harness_in_place is handled by the worker runtime",
                    false,
                    None,
                ));
            }
            RelayRequest::ReadQueuedCommand { command_id } => RelayResponsePayload::QueuedCommand {
                // One admitted command is bounded by RELAY_COMMAND_BYTE_BUDGET,
                // independently of the total size of the durable queue.
                command: self
                    .snapshot
                    .dispatches
                    .get(command_id)
                    .filter(|dispatch| dispatch.command.is_queue_entry())
                    .map(|dispatch| dispatch.command.clone()),
                accepted_ordinal: self
                    .snapshot
                    .handled_commands
                    .get(command_id)
                    .map(|handled| handled.accepted_ordinal),
            },
            RelayRequest::ReadCheckpointMailbox {
                barrier_command_id,
                cut,
            } => match self.checkpoint_mailbox(barrier_command_id, cut)? {
                Ok(mailbox) => {
                    if envelope.protocol_version < RELAY_CHECKPOINT_QUOTA_INPUT_PROTOCOL
                        && (!self.snapshot.quota_interrupted_input.is_empty()
                            || !self.snapshot.quota_prompt_contexts.is_empty()
                            || !self.snapshot.frozen_prompt_contexts.is_empty()
                            || !self.snapshot.steered_prompt_contexts.is_empty())
                    {
                        return Ok(relay_error(
                            RelayErrorCode::InvalidState,
                            "controller cannot preserve quota-interrupted input at this checkpoint",
                            false,
                            None,
                        ));
                    }
                    RelayResponsePayload::CheckpointMailbox { mailbox }
                }
                Err(message) => {
                    return Ok(relay_error(
                        RelayErrorCode::InvalidState,
                        message,
                        false,
                        None,
                    ));
                }
            },
            RelayRequest::ReadCheckpointQuotaInput {
                barrier_command_id,
                cut,
                offset,
            } => {
                if self.checkpoint_native_use(barrier_command_id, cut).is_err() {
                    return Ok(relay_error(
                        RelayErrorCode::InvalidState,
                        "the relay is no longer at this checkpoint cut",
                        false,
                        None,
                    ));
                }
                let mut input = serde_json::json!({
                    "pending": self.snapshot.quota_interrupted_input,
                    "claimed": self.snapshot.quota_prompt_contexts,
                    "frozen_prompt_contexts": self.snapshot.frozen_prompt_contexts,
                });
                if !self.snapshot.steered_prompt_contexts.is_empty() {
                    input["steered"] =
                        serde_json::to_value(&self.snapshot.steered_prompt_contexts)?;
                }
                let text = serde_json::to_string(&input)?;
                let Some(offset) = usize::try_from(*offset)
                    .ok()
                    .filter(|&offset| offset <= text.len() && text.is_char_boundary(offset))
                else {
                    return Ok(relay_error(
                        RelayErrorCode::InvalidRequest,
                        "invalid checkpoint quota input offset",
                        false,
                        None,
                    ));
                };
                let page = |end: usize| RelayResponsePayload::CheckpointQuotaInput {
                    page: text[offset..end].to_owned(),
                    next_offset: (end < text.len()).then_some(end as u64),
                };
                let fits = |payload: &RelayResponsePayload| -> Result<bool> {
                    let response = RelayResponseEnvelope {
                        request_id: envelope.request_id.clone(),
                        protocol_version: envelope.protocol_version,
                        body: RelayResponseBody::Ok {
                            payload: payload.clone(),
                        },
                    };
                    Ok(serde_json::to_vec(&response)?.len().saturating_add(1) <= MAX_FRAME_BYTES)
                };
                let mut low = offset;
                let mut high = offset.saturating_add(MAX_FRAME_BYTES).min(text.len());
                let mut best = offset;
                while low < high {
                    let mid = low + (high - low).div_ceil(2);
                    let mut boundary = mid;
                    while !text.is_char_boundary(boundary) {
                        boundary -= 1;
                    }
                    if fits(&page(boundary))? {
                        low = mid;
                        best = boundary;
                    } else {
                        high = mid - 1;
                    }
                }
                if best == offset && offset != text.len() {
                    return Ok(relay_error(
                        RelayErrorCode::InvalidState,
                        "checkpoint quota input page cannot fit in a relay response frame",
                        false,
                        None,
                    ));
                }
                page(best)
            }
            RelayRequest::ReadCheckpointNativeUse {
                barrier_command_id,
                cut,
                pending_context_offset,
            } => match self.checkpoint_native_use(barrier_command_id, cut) {
                Ok((native_use_evidence, pending_prompt_context)) => {
                    let text =
                        if envelope.protocol_version >= RELAY_CHECKPOINT_PROMPT_CONTEXT_PROTOCOL {
                            pending_prompt_context
                        } else {
                            None
                        };
                    let offset = usize::try_from(pending_context_offset.unwrap_or(0)).ok();
                    let Some(offset) = offset.filter(|&offset| match text {
                        Some(text) => offset <= text.len() && text.is_char_boundary(offset),
                        None => offset == 0,
                    }) else {
                        return Ok(relay_error(
                            RelayErrorCode::InvalidRequest,
                            "invalid checkpoint prompt context offset",
                            false,
                            None,
                        ));
                    };
                    let page = |end: usize, next_context_offset: Option<u64>| {
                        RelayResponsePayload::CheckpointNativeUse {
                            native_use_evidence: native_use_evidence.clone(),
                            pending_prompt_context: text.map(|text| text[offset..end].to_owned()),
                            next_context_offset,
                        }
                    };
                    let fits = |payload: &RelayResponsePayload| -> Result<bool> {
                        let response = RelayResponseEnvelope {
                            request_id: envelope.request_id.clone(),
                            protocol_version: envelope.protocol_version,
                            body: RelayResponseBody::Ok {
                                payload: payload.clone(),
                            },
                        };
                        Ok(serde_json::to_vec(&response)?.len().saturating_add(1)
                            <= MAX_FRAME_BYTES)
                    };
                    let end = text.map_or(0, str::len);
                    if end - offset <= MAX_FRAME_BYTES && fits(&page(end, None))? {
                        page(end, None)
                    } else {
                        let Some(text) = text else {
                            return Ok(relay_error(
                                RelayErrorCode::InvalidState,
                                "checkpoint native-use evidence cannot fit in a relay response frame",
                                false,
                                None,
                            ));
                        };
                        let mut low = offset;
                        let mut high = offset.saturating_add(MAX_FRAME_BYTES).min(end);
                        let mut best = offset;
                        while low < high {
                            let mid = low + (high - low).div_ceil(2);
                            let mut boundary = mid;
                            while !text.is_char_boundary(boundary) {
                                boundary -= 1;
                            }
                            let candidate = page(boundary, Some(boundary as u64));
                            if fits(&candidate)? {
                                low = mid;
                                best = boundary;
                            } else {
                                high = mid - 1;
                            }
                        }
                        if best == offset {
                            return Ok(relay_error(
                                RelayErrorCode::InvalidState,
                                "checkpoint prompt context cannot fit in a relay response frame",
                                false,
                                None,
                            ));
                        }
                        page(best, Some(best as u64))
                    }
                }
                Err(message) => {
                    return Ok(relay_error(
                        RelayErrorCode::InvalidState,
                        message,
                        false,
                        None,
                    ));
                }
            },
            RelayRequest::ReadPendingElicitation { elicitation_id } => {
                RelayResponsePayload::PendingElicitation {
                    request: self
                        .snapshot
                        .pending_elicitations
                        .get(elicitation_id)
                        .cloned()
                        .flatten(),
                }
            }
            RelayRequest::BeginLiveBaseline
            | RelayRequest::ReadLiveBaselinePage { .. }
            | RelayRequest::AbortLiveBaseline => {
                bail!("live baseline download requires its owning connection")
            }
            RelayRequest::CommandRetentionStatus => RelayResponsePayload::CommandRetentionState {
                legacy_pending: self.snapshot.legacy_command_retention,
                operation_ids: self.snapshot.command_retention.keys().cloned().collect(),
            },
            RelayRequest::UpdateCommandRetention {
                operation_id,
                command_ids,
                reconcile_legacy,
            } => {
                if !reconcile_legacy || self.snapshot.legacy_command_retention {
                    let mut next = self.snapshot.clone();
                    if let Some(operation_id) = operation_id {
                        super::commands::validate_identifier(
                            operation_id,
                            "retention operation id",
                        )?;
                        if command_ids.is_empty() {
                            next.command_retention.remove(operation_id);
                        } else {
                            next.command_retention
                                .insert(operation_id.clone(), command_ids.clone());
                        }
                    } else {
                        anyhow::ensure!(
                            *reconcile_legacy && command_ids.is_empty(),
                            "retention requires an operation identity"
                        );
                    }
                    // This publication precedes any following Submit on the connection.
                    // A stale upgrade reconciliation becomes a no-op after this point.
                    // Operation pin updates do not own the upgrade hold: a
                    // different pending Move may still need reconciliation.
                    if *reconcile_legacy {
                        next.legacy_command_retention = false;
                    }
                    self.commit_snapshot(next)?;
                }
                RelayResponsePayload::CommandRetentionUpdated
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
            RelayRequest::DrainMailbox {
                hook_event,
                conversation_id,
                initial_num_steps,
            } => self.drain_mailbox_with_initial_steps(
                hook_event,
                conversation_id.as_deref(),
                *initial_num_steps,
            )?,
            RelayRequest::AckMailbox { lease_id } => self.ack_mailbox(lease_id)?,
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
            | RelayRequest::FileAttachmentPresent { .. }
            | RelayRequest::InstallFileAttachment { .. }
            | RelayRequest::ReadFileAttachment { .. }
            | RelayRequest::CpuUsage
            | RelayRequest::CredentialState
            | RelayRequest::AwsCredentialsState
            | RelayRequest::InstallAwsCredentials { .. }
            | RelayRequest::RemoveAwsCredentials
            | RelayRequest::AwsCredentialsScope
            | RelayRequest::SlopcopLoginState
            | RelayRequest::InstallSlopcopLogin { .. }
            | RelayRequest::RemoveSlopcopLogin
            | RelayRequest::ReadRetiredLogins { .. }
            | RelayRequest::AckRetiredLogin { .. }
            | RelayRequest::ReadCredentials
            | RelayRequest::InstallCredentials { .. }
            | RelayRequest::SkillsState
            | RelayRequest::InstallSkills { .. }
            | RelayRequest::GithubTokenState
            | RelayRequest::InstallGithubToken { .. }
            | RelayRequest::RemoveGithubToken
            | RelayRequest::ProjectMemorySnapshot
            | RelayRequest::ProjectMemoryTransfer { .. }
            | RelayRequest::HistoryQuery { .. }
            | RelayRequest::CompleteHistoryRequest { .. }
            | RelayRequest::CompleteGuardianRequest { .. }
            | RelayRequest::InstallProjectMemorySnapshot { .. }
            | RelayRequest::ReplaceProjectMemoryTree { .. }
            | RelayRequest::CompleteSubagentRequest { .. }
            | RelayRequest::PollSubagentQueue { .. }
            | RelayRequest::WaitForChange { .. }
            | RelayRequest::SetSubagentAdmission { .. }
            | RelayRequest::SetOperatorAway { .. } => {
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
            // The live connection owns the worker's pending reviews. A
            // sealed relay serving only the journal has none.
            RelayRequest::GuardianRequests => RelayResponsePayload::GuardianRequests {
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
        self.commit_snapshot(next)
    }

    fn attach(
        &mut self,
        after_ordinal: u64,
        after_digest: &str,
        known_state_digest: Option<&str>,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        let state = self.operational_state();
        self.replay_plan()
            .attach(after_ordinal, after_digest, state, known_state_digest)
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
