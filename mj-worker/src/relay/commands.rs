use super::*;

pub(super) fn validate_identifier(value: &str, name: &str) -> Result<()> {
    if value.len() < 8
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid {name}");
    }
    Ok(())
}

impl DurableRelay {
    fn validate_turn_control(&self, command: &RelayCommand) -> Result<(), String> {
        match command {
            RelayCommand::BeginCheckpoint { .. } if self.snapshot.steering.as_ref().is_some_and(|s| s.holds_queue()) => return Err("Resolve uncertain steering delivery before checkpointing or moving this session".into()),
            RelayCommand::Steer {
                active_prompt_id,
                queued_prompt_id,
            } => {
                if self.snapshot.checkpoint_barrier.is_some() {
                    return Err("A checkpoint is already admitted".into());
                }
                if self.snapshot.active_prompt.as_ref().map(|p| &p.command_id)
                    != Some(active_prompt_id)
                {
                    return Err("The requested turn is no longer running".into());
                }
                if !self.snapshot.queued_prompts.first().is_some_and(|q|
                    q.command_id == *queued_prompt_id && matches!(q.payload, StoredQueuedRelayPayload::Prompt { .. }))
                {
                    return Err("The queued prompt changed; steering was not sent".into());
                }
                if self
                    .snapshot
                    .steering
                    .as_ref()
                    .is_some_and(|s| s.holds_queue())
                    || self.snapshot.cancelling_prompt_id.is_some()
                {
                    return Err(
                        "Turn control is already pending; no additional request was sent".into(),
                    );
                }
            }
            RelayCommand::CancelTurnFor { active_prompt_id } => {
                if self.snapshot.checkpoint_barrier.is_some() {
                    return Err("A checkpoint is already admitted".into());
                }
                if self.snapshot.active_prompt.as_ref().map(|p| &p.command_id)
                    != Some(active_prompt_id)
                {
                    return Err("The requested turn is no longer running".into());
                }
                if self.snapshot.cancelling_prompt_id.is_some() {
                    return Err("Cancellation is already pending".into());
                }
            }
            RelayCommand::ResolveSteering { steering_id } => {
                let Some(steering) = self
                    .snapshot
                    .steering
                    .as_ref()
                    .filter(|s| &s.command_id == steering_id)
                else {
                    return Err("The steering operation changed".into());
                };
                if steering.status != mj_core::relay::SteeringStatus::Unconfirmed
                    || self.snapshot.active_prompt.is_some()
                {
                    return Err(
                        "Wait for the original turn to settle before retrying uncertain input"
                            .into(),
                    );
                }
            }
            RelayCommand::RemoveQueuedPrompt { queued_command_id }
                if self.snapshot.steering.as_ref().is_some_and(|s| {
                    &s.queued_prompt_id == queued_command_id && s.holds_queue()
                }) && self.snapshot.active_prompt.is_some() =>
            {
                return Err("Wait for steering to settle before removing its prompt".into());
            }
            RelayCommand::ClearQueuedPrompts
                if self
                    .snapshot
                    .steering
                    .as_ref()
                    .is_some_and(|s| s.holds_queue())
                    && self.snapshot.active_prompt.is_some() =>
            {
                return Err("Wait for steering to settle before clearing the queue".into());
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn submit_command(
        &mut self,
        command_id: &str,
        command: RelayCommand,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        if let Err(error) =
            ensure_serialized_budget(&command, RELAY_COMMAND_BYTE_BUDGET, "relay command")
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                error.to_string(),
                false,
                None,
            )));
        }
        if validate_identifier(command_id, "command ID").is_err() {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "invalid command ID",
                false,
                None,
            )));
        }
        let command = if let RelayCommand::Prompt { prompt } = &command {
            if let Some((maintenance, args)) = mj_core::acp::context_command(prompt) {
                if prompt.len() != 1
                    || (maintenance == mj_core::acp::ContextCommand::Clear && !args.is_empty())
                    || (maintenance == mj_core::acp::ContextCommand::Compact
                        && !args.is_empty()
                        && self.verdict_harness == Some(mj_core::config::HarnessKind::Codex))
                {
                    return Ok(Err(relay_protocol_error(
                        RelayErrorCode::InvalidRequest,
                        "This context command does not support these arguments or attachments",
                        false,
                        None,
                    )));
                }
                if maintenance == mj_core::acp::ContextCommand::Clear {
                    RelayCommand::ClearContext
                } else {
                    RelayCommand::Prompt {
                        prompt: vec![agent_client_protocol::schema::v1::ContentBlock::from(
                            if args.is_empty() {
                                "/compact".to_owned()
                            } else {
                                format!("/compact {args}")
                            },
                        )],
                    }
                }
            } else {
                command
            }
        } else {
            command
        };
        if let Some(handled) = self.snapshot.handled_commands.get(command_id) {
            let accepted_ordinal = {
                if handled.command != command {
                    return Ok(Err(relay_protocol_error(
                        RelayErrorCode::InvalidRequest,
                        "command ID was already used for a different command",
                        false,
                        None,
                    )));
                }
                handled.accepted_ordinal
            };
            // A journal append can succeed before snapshot persistence reports
            // an error. Retrying the durable command must resume any remaining
            // relay-local transition instead of merely echoing its first ACK.
            if command.is_relay_local() {
                self.finish_relay_local_command(command_id)?;
            }
            return Ok(Ok(RelayResponsePayload::Accepted {
                command_id: command_id.to_owned(),
                ordinal: accepted_ordinal,
            }));
        }
        if self
            .snapshot
            .dispatches
            .values()
            .any(|dispatch| matches!(dispatch.command, RelayCommand::ClearContext))
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "Context is being cleared; wait for the new conversation",
                false,
                None,
            )));
        }
        if matches!(command, RelayCommand::ClearContext) {
            if !matches!(
                self.verdict_harness,
                Some(mj_core::config::HarnessKind::Codex | mj_core::config::HarnessKind::Claude)
            ) {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "This harness does not support /clear",
                    false,
                    None,
                )));
            }
            if self.snapshot.native_session_id.is_none()
                || self.snapshot.execution != RelayExecutionState::Idle
                || !self.snapshot.dispatches.is_empty()
                || !self.snapshot.queued_prompts.is_empty()
                || self.snapshot.goal.running()
                || self.snapshot.goal.active()
                || self.snapshot.goal.pending_resume.is_some()
                || self.snapshot.goal.decision.is_some()
                || self.snapshot.harness_turn.is_some()
                || !self.snapshot.active_user_shells.is_empty()
                || !self.background_commands().is_empty()
                || self.native_agent_count() > 0
                || self.snapshot.checkpoint_barrier.is_some()
                || self.snapshot.capacity_retry.is_some()
            {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "/clear requires an idle session with no queued or background work; finish or cancel that work first",
                    false,
                    None,
                )));
            }
        }
        if let RelayCommand::Prompt { prompt } = &command {
            let verified = mj_core::attachment::references(prompt).and_then(|references| {
                let store = mj_core::attachment::AttachmentStore::worker(&self.root);
                for reference in references {
                    store.read(&reference)?;
                }
                Ok(())
            });
            if let Err(error) = verified {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidRequest,
                    error.to_string(),
                    false,
                    None,
                )));
            }
        }
        let pending_close_barrier = self.pending_close_barrier_id().map(str::to_owned);
        let completes_pending_close = pending_close_barrier.as_deref().is_some_and(|barrier| {
            matches!(
                &command,
                RelayCommand::CompleteCheckpoint { barrier_command_id }
                    if barrier_command_id == barrier
            )
        });
        if pending_close_barrier.is_some() && !completes_pending_close {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "relay session is sealed for close",
                false,
                None,
            )));
        }
        if matches!(
            self.snapshot.execution,
            RelayExecutionState::Closing | RelayExecutionState::Closed
        ) && !completes_pending_close
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "relay session is closing",
                false,
                None,
            )));
        }
        if let RelayCommand::Prompt { prompt } = &command
            && prompt.is_empty()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "prompt is empty",
                false,
                None,
            )));
        }
        if let RelayCommand::RunUserShell { command } = &command
            && command.trim().is_empty()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "shell command is empty",
                false,
                None,
            )));
        }
        if let RelayCommand::CancelUserShell { shell_command_id } = &command
            && !self
                .snapshot
                .active_user_shells
                .contains_key(shell_command_id)
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "there is no active shell command with that ID",
                false,
                None,
            )));
        }
        if let RelayCommand::SetConfig { key, value } = &command
            && (key.trim().is_empty() || value.trim().is_empty())
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "configuration key and value are required",
                false,
                None,
            )));
        }
        if let RelayCommand::RecordNotice { text } = &command
            && text.trim().is_empty()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "notice text is required",
                false,
                None,
            )));
        }
        // A late cancellation must not advance the checkpoint cursor or leave
        // a cancellation queued for a future turn after the barrier releases.
        if matches!(
            command,
            RelayCommand::CancelTurn | RelayCommand::GoalControl { .. }
        ) && self.snapshot.checkpoint_barrier.is_some()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "the checkpoint barrier is already admitted",
                false,
                None,
            )));
        }
        if let Err(message) = self.validate_turn_control(&command) {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                message,
                false,
                None,
            )));
        }
        if let RelayCommand::Cancel = command
            && self.snapshot.active_prompt.is_none()
            && self
                .snapshot
                .capacity_retry
                .as_ref()
                .is_none_or(|r| r.submitted)
        {
            let message = if self.snapshot.harness_turn.is_some() {
                "the agent is working on its own after a background task; there is no prompt to cancel"
            } else {
                "there is no active prompt to cancel"
            };
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                message,
                false,
                None,
            )));
        }
        if let RelayCommand::RemoveQueuedPrompt { queued_command_id } = &command
            && !self
                .snapshot
                .queued_prompts
                .iter()
                .any(|queued| queued.command_id == *queued_command_id)
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "unknown queued prompt",
                false,
                None,
            )));
        }
        if let RelayCommand::CompleteCheckpoint { barrier_command_id }
        | RelayCommand::ReleaseCheckpoint { barrier_command_id } = &command
            && (self.snapshot.checkpoint_barrier.as_deref() != Some(barrier_command_id)
                || self.snapshot.checkpoint_ready_through.is_none())
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "checkpoint barrier is not active",
                false,
                None,
            )));
        }
        if let RelayCommand::AdvanceRecoveryFloor { through } = &command
            && let Some(message) = self.recovery_floor_rejection(through)?
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                message,
                false,
                None,
            )));
        }
        if let RelayCommand::Close {
            barrier_command_id,
            expected,
        } = &command
        {
            let ready = self
                .snapshot
                .checkpoint_ready_through
                .zip(self.snapshot.checkpoint_ready_digest.as_ref());
            let exact_cut = self.snapshot.checkpoint_barrier.as_deref() == Some(barrier_command_id)
                && ready.is_some_and(|(ordinal, digest)| {
                    ordinal == expected.ordinal && digest == &expected.digest
                })
                && self.snapshot.latest_ordinal == expected.ordinal
                && self.snapshot.latest_digest == expected.digest;
            if !exact_cut {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "close does not match the current checkpoint cut",
                    false,
                    None,
                )));
            }
        }

        if self.checkpoint_only
            && !command.is_relay_local()
            && !matches!(
                command,
                RelayCommand::BeginCheckpoint { .. } | RelayCommand::Close { .. }
            )
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "session is being preserved for Move; resume it to run commands",
                false,
                None,
            )));
        }
        if let RelayCommand::ContinueAuthorizedWork {
            expected,
            user_command_id,
            completed_command_id,
            attempt,
        } = &command
        {
            let state = &self.snapshot.continuation;
            let facts = self.activity_facts();
            let planning = self.verdict_harness.is_some_and(|harness| {
                mj_core::acp::AcpSessionFacts::from_operational(
                    harness,
                    &self.snapshot.config,
                    &self.snapshot.config_options,
                    self.snapshot.modes.as_ref(),
                )
                .plan_mode_active()
            });
            if planning
                || !state.eligible()
                || state.user_command_id.as_ref() != Some(user_command_id)
                || state.completed_command_id.as_ref() != Some(completed_command_id)
                || *attempt != state.attempts + 1
                || self.snapshot.latest_ordinal != expected.ordinal
                || self.snapshot.latest_digest != expected.digest
                || !mj_core::activity::is_quiet(&facts)
                || facts.background_commands != 0
                || !self.snapshot.queued_prompts.is_empty()
                || self.snapshot.goal.active()
                || self.pending_close_barrier_id().is_some()
            {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "continuation evidence is stale or the allowance is exhausted",
                    false,
                    None,
                )));
            }
        }
        let created_at_ms = epoch_millis();
        let accepted_ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandQueued {
                command_id: command_id.to_owned(),
                command: command.clone(),
                created_at_ms,
            },
        )?;

        if command.is_relay_local() {
            self.finish_relay_local_command(command_id)?;
        }
        self.promote_next_queued_command()?;
        Ok(Ok(RelayResponsePayload::Accepted {
            command_id: command_id.to_owned(),
            ordinal: accepted_ordinal,
        }))
    }

    /// Why a recovery floor move must be refused, or `None` when it is valid.
    ///
    /// Journal garbage collection retains history through this ordinal, so a
    /// cursor off this relay's own event chain would discard events that no
    /// installed archive covers. The floor therefore only moves forward, only
    /// within the durable frontier, and only to a matching digest.
    fn recovery_floor_rejection(&self, through: &RelayCursor) -> Result<Option<String>> {
        if through.ordinal > self.snapshot.latest_ordinal {
            return Ok(Some(format!(
                "recovery floor {} is ahead of the relay frontier {}",
                through.ordinal, self.snapshot.latest_ordinal
            )));
        }
        if through.ordinal < self.snapshot.recovery_floor_ordinal {
            return Ok(Some(format!(
                "recovery floor {} is behind the current floor {}",
                through.ordinal, self.snapshot.recovery_floor_ordinal
            )));
        }
        if validate_relay_digest(&through.digest, "recovery floor digest").is_err() {
            return Ok(Some("recovery floor digest is malformed".to_owned()));
        }
        let Some(expected) = self.digest_at(through.ordinal)? else {
            return Ok(Some(format!(
                "relay digest is unavailable at event {}",
                through.ordinal
            )));
        };
        if through.digest != expected {
            return Ok(Some(format!(
                "recovery floor digest does not match the relay event chain at event {}",
                through.ordinal
            )));
        }
        Ok(None)
    }

    /// Finish a relay-local command from its durable dispatch record. This is
    /// deliberately restartable: every intermediate mutation is an event, so
    /// reopening the relay can resume after any append without duplicating or
    /// skipping the remaining queue transition.
    pub(super) fn finish_relay_local_command(&mut self, command_id: &str) -> Result<()> {
        let dispatch = self
            .snapshot
            .dispatches
            .get(command_id)
            .with_context(|| format!("unknown relay-local command {command_id}"))?;
        if !dispatch.command.is_relay_local() {
            bail!("command {command_id} is not relay-local");
        }
        let command = dispatch.command.clone();
        let state = dispatch.state;
        if matches!(
            state,
            RelayDispatchState::Completed
                | RelayDispatchState::Rejected
                | RelayDispatchState::Interrupted
        ) {
            if state == RelayDispatchState::Completed && releases_history(&command) {
                // The completion event may be durable even if its following
                // journal GC reported a transient persistence error.
                self.garbage_collect_relay_history()?;
            }
            return Ok(());
        }
        // Recorded before the command starts, and unguarded by dispatch state:
        // a retry that repeats this append is harmless because the projection
        // keys the transcript line on this command, not on the event ordinal.
        if let RelayCommand::RecordNotice { text } = &command {
            let message = text.clone();
            self.append_relay_event(Some(command_id), RelayObservation::Notice { message })?;
        }
        if state == RelayDispatchState::Queued {
            self.append_relay_event(
                Some(command_id),
                RelayObservation::CommandStarted {
                    command_id: command_id.to_owned(),
                    started_at_ms: epoch_millis(),
                },
            )?;
        }

        let removed_command_ids = match &command {
            RelayCommand::RemoveQueuedPrompt { queued_command_id } => {
                if self
                    .snapshot
                    .queued_prompts
                    .iter()
                    .any(|queued| queued.command_id == *queued_command_id)
                {
                    vec![queued_command_id.clone()]
                } else {
                    Vec::new()
                }
            }
            RelayCommand::ClearQueuedPrompts => self
                .snapshot
                .queued_prompts
                .iter()
                .map(|queued| queued.command_id.clone())
                .collect(),
            _ => Vec::new(),
        };
        let outcome = match &command {
            RelayCommand::CompleteCheckpoint { .. } => RelayCommandOutcome::CheckpointCompleted,
            RelayCommand::ReleaseCheckpoint { .. } => RelayCommandOutcome::CheckpointReleased,
            RelayCommand::AdvanceRecoveryFloor { .. } => RelayCommandOutcome::RecoveryFloorAdvanced,
            RelayCommand::RecordNotice { .. } | RelayCommand::ResolveSteering { .. } => {
                RelayCommandOutcome::NoticeRecorded
            }
            _ => RelayCommandOutcome::QueueChanged {
                removed_command_ids,
            },
        };
        self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandCompleted {
                command_id: command_id.to_owned(),
                outcome,
            },
        )?;
        if releases_history(&command) {
            self.garbage_collect_relay_history()?;
        }
        Ok(())
    }

    /// Durably claim commands before they are handed to the live ACP driver.
    /// A checkpoint barrier is admitted only after every previously-started
    /// ACP effect is terminal. Once admitted, it is the sole claimable command
    /// and all ACP dispatch remains frozen until that exact barrier completes.
    pub fn claim_pending_commands(
        &mut self,
        acp_session_configured: bool,
    ) -> Result<Vec<ClaimedRelayCommand>> {
        self.claim_pending_commands_up_to(acp_session_configured, usize::MAX)
    }

    /// Claim no more work than the caller already holds transport permits for.
    /// The dispatcher reserves one ACP command permit per claimed command
    /// before calling, so every claim can be handed over without waiting even
    /// when other senders share that channel. Bounding the durable in-flight
    /// batch that way is what keeps command backpressure from parking the
    /// coordinator that must keep draining ACP's bounded event channel.
    pub fn claim_pending_commands_up_to(
        &mut self,
        acp_session_configured: bool,
        maximum: usize,
    ) -> Result<Vec<ClaimedRelayCommand>> {
        if self.checkpoint_only || !acp_session_configured || maximum == 0 {
            return Ok(Vec::new());
        }
        // Reject controls whose targets settled between admission and dispatch.
        let stale: Vec<_> = self
            .snapshot
            .dispatches
            .iter()
            .filter_map(|(id, d)| {
                if !matches!(
                    d.state,
                    RelayDispatchState::Queued | RelayDispatchState::Pending
                ) {
                    return None;
                }
                let target = match &d.command {
                    RelayCommand::Steer {
                        active_prompt_id, ..
                    }
                    | RelayCommand::CancelTurnFor { active_prompt_id } => active_prompt_id,
                    _ => return None,
                };
                (self.snapshot.active_prompt.as_ref().map(|p| &p.command_id) != Some(target))
                    .then_some(id.clone())
            })
            .collect();
        for id in stale {
            self.record_command_rejected(&id, "The requested turn is no longer running")?;
        }
        self.promote_next_queued_command()?;
        if self
            .snapshot
            .steering
            .as_ref()
            .is_some_and(|s| s.holds_queue())
        {
            while let Some((barrier_id, _)) = self.next_queued_checkpoint() {
                self.record_command_rejected(&barrier_id, "Resolve uncertain steering delivery before checkpointing or moving this session")?;
            }
        }
        if self.snapshot.checkpoint_barrier.is_none() {
            if let Some((barrier_id, barrier_ordinal)) = self.next_queued_checkpoint() {
                let mut earlier_controls = self.queued_controls_before(barrier_ordinal);
                if !earlier_controls.is_empty() {
                    earlier_controls.truncate(maximum);
                    self.start_queued_controls(earlier_controls)?;
                // A turn the harness started on its own is real work in the
                // agent's workspace, so the barrier waits for it exactly as it
                // waits for a prompt.
                } else if !self.effectful_command_in_progress()
                    && !self
                        .snapshot
                        .steering
                        .as_ref()
                        .is_some_and(|s| s.holds_queue())
                    && self.snapshot.harness_turn.is_none()
                {
                    self.append_relay_event(
                        Some(&barrier_id),
                        RelayObservation::CommandStarted {
                            command_id: barrier_id.clone(),
                            started_at_ms: epoch_millis(),
                        },
                    )?;
                }
            } else {
                let mut controls = self.queued_controls_before(u64::MAX);
                controls.truncate(maximum);
                self.start_queued_controls(controls)?;
            }
        }

        let active_barrier = self.snapshot.checkpoint_barrier.as_deref();
        let mut claimable: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter_map(|(command_id, dispatch)| {
                if dispatch.state != RelayDispatchState::Pending {
                    return None;
                }
                match active_barrier {
                    Some(barrier_id) if command_id == barrier_id => self
                        .snapshot
                        .handled_commands
                        .get(command_id)
                        .map(|handled| (handled.accepted_ordinal, command_id.clone())),
                    Some(_) => None,
                    None => self
                        .snapshot
                        .handled_commands
                        .get(command_id)
                        .map(|handled| (handled.accepted_ordinal, command_id.clone())),
                }
            })
            .collect();
        claimable.sort_by_key(|(accepted_ordinal, _)| *accepted_ordinal);
        claimable.truncate(maximum);
        let mut claimed = Vec::with_capacity(claimable.len());
        let mut next_snapshot = self.snapshot.clone();
        for (accepted_ordinal, command_id) in claimable {
            let steering_prompt = matches!(
                next_snapshot.dispatches[&command_id].command,
                RelayCommand::Cancel | RelayCommand::Steer { .. }
            )
            .then(|| next_snapshot.queued_prompts.first())
            .flatten()
            .and_then(|queued| match &queued.payload {
                StoredQueuedRelayPayload::Prompt { prompt } => Some(ClaimedSteeringPrompt {
                    attachment_root: None,
                    queued_command_id: queued.command_id.clone(),
                    prompt: prompt.clone(),
                }),
                StoredQueuedRelayPayload::SetConfig { .. } => None,
            });
            let dispatch = next_snapshot
                .dispatches
                .get_mut(&command_id)
                .expect("claimable command disappeared");
            dispatch.state = RelayDispatchState::InFlight;
            let hidden_prompt_context = dispatch
                .command
                .prompt_blocks()
                .is_some_and(|prompt| !mj_core::acp::prompt_requests_compaction(&prompt))
                .then(|| {
                    let mut contexts = Vec::new();
                    if let Some(context) = next_snapshot.pending_prompt_context.as_mut() {
                        if context.attached_command_id.is_none() {
                            context.attached_command_id = Some(command_id.clone());
                        }
                        if context.attached_command_id.as_deref() == Some(command_id.as_str()) {
                            contexts.push(context.text.clone());
                        }
                    }
                    for context in &mut next_snapshot.pending_user_shell_contexts {
                        if context.accepted_ordinal >= accepted_ordinal {
                            continue;
                        }
                        if context.attached_command_id.is_none() {
                            context.attached_command_id = Some(command_id.clone());
                        }
                        if context.attached_command_id.as_deref() == Some(command_id.as_str()) {
                            contexts.push(context.text.clone());
                        }
                    }
                    (!contexts.is_empty()).then(|| contexts.join("\n\n"))
                })
                .flatten();
            claimed.push(ClaimedRelayCommand {
                command_id,
                accepted_ordinal,
                command: dispatch.command.clone(),
                hidden_prompt_context,
                steering_prompt,
            });
        }
        if !claimed.is_empty() {
            // An in-flight claim is not in the journal, so it is only durable
            // once the snapshot itself is.
            self.commit_snapshot(next_snapshot)?;
            if claimed
                .iter()
                .any(|claim| claim.command.prompt_blocks().is_some())
            {
                self.capacity_response = CapacityResponse::default();
            }
        }
        Ok(claimed)
    }

    /// Advance only lifecycle commands after the old owning process was stopped.
    /// No ACP channel or harness readiness is involved in this mode.
    pub fn dispatch_checkpoint_only(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.checkpoint_only,
            "worker is not in checkpoint-only mode"
        );
        let mut commands: Vec<_> = self
            .snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| {
                matches!(
                    dispatch.command,
                    RelayCommand::BeginCheckpoint { .. } | RelayCommand::Close { .. }
                )
            })
            .filter(|(_, dispatch)| {
                matches!(
                    dispatch.state,
                    RelayDispatchState::Queued | RelayDispatchState::Pending
                )
            })
            .map(|(id, _)| {
                (
                    self.snapshot.handled_commands[id].accepted_ordinal,
                    id.clone(),
                )
            })
            .collect();
        commands.sort();
        for (_, id) in commands {
            let command = self.snapshot.dispatches[&id].command.clone();
            // Close is sealed by acceptance but executes only after the controller
            // releases its verified barrier, just like the ordinary coordinator.
            if self.snapshot.checkpoint_barrier.is_some() {
                continue;
            }
            if self.snapshot.dispatches[&id].state == RelayDispatchState::Queued {
                self.append_relay_event(
                    Some(&id),
                    RelayObservation::CommandStarted {
                        command_id: id.clone(),
                        started_at_ms: epoch_millis(),
                    },
                )?;
            }
            let mut next = self.snapshot.clone();
            next.dispatches
                .get_mut(&id)
                .expect("lifecycle command")
                .state = RelayDispatchState::InFlight;
            self.commit_snapshot(next)?;
            match command {
                RelayCommand::BeginCheckpoint { .. } => {
                    self.record_checkpoint_ready(&id)?;
                }
                RelayCommand::Close { .. } => {
                    self.record_command_completed(&id, RelayCommandOutcome::Closed)?;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    /// Claim user shell work independently of ACP turns. Run commands honor
    /// the caller's concurrency limit; cancellation controls bypass it so a
    /// full shell pool can always be stopped.
    pub fn claim_pending_user_shell_commands_up_to(
        &mut self,
        maximum_runs: usize,
    ) -> Result<Vec<ClaimedRelayCommand>> {
        if self.checkpoint_only || self.snapshot.checkpoint_barrier.is_some() {
            return Ok(Vec::new());
        }
        let barrier_ordinal = self
            .next_queued_checkpoint()
            .map_or(u64::MAX, |(_, ordinal)| ordinal);
        let mut cancels = Vec::new();
        let cancelled_shells: std::collections::BTreeSet<String> = self
            .snapshot
            .dispatches
            .values()
            .filter(|dispatch| dispatch.state == RelayDispatchState::Queued)
            .filter_map(|dispatch| match &dispatch.command {
                RelayCommand::CancelUserShell { shell_command_id } => {
                    Some(shell_command_id.clone())
                }
                _ => None,
            })
            .collect();
        let mut runs = Vec::new();
        for (command_id, dispatch) in &self.snapshot.dispatches {
            if dispatch.state != RelayDispatchState::Queued {
                continue;
            }
            let Some(handled) = self.snapshot.handled_commands.get(command_id) else {
                continue;
            };
            match dispatch.command {
                RelayCommand::CancelUserShell { .. } => {
                    cancels.push((handled.accepted_ordinal, command_id.clone()));
                }
                RelayCommand::RunUserShell { .. }
                    if handled.accepted_ordinal < barrier_ordinal
                        && !cancelled_shells.contains(command_id) =>
                {
                    runs.push((handled.accepted_ordinal, command_id.clone()));
                }
                _ => {}
            }
        }
        cancels.sort();
        runs.sort();
        runs.truncate(maximum_runs);
        let mut selected = cancels;
        selected.extend(runs);
        selected.sort();
        for (_, command_id) in &selected {
            self.append_relay_event(
                Some(command_id),
                RelayObservation::CommandStarted {
                    command_id: command_id.clone(),
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        let mut next_snapshot = self.snapshot.clone();
        let mut claimed = Vec::with_capacity(selected.len());
        for (accepted_ordinal, command_id) in selected {
            let dispatch = next_snapshot
                .dispatches
                .get_mut(&command_id)
                .expect("claimed shell command disappeared");
            dispatch.state = RelayDispatchState::InFlight;
            claimed.push(ClaimedRelayCommand {
                command_id,
                accepted_ordinal,
                command: dispatch.command.clone(),
                hidden_prompt_context: None,
                steering_prompt: None,
            });
        }
        if !claimed.is_empty() {
            self.commit_snapshot(next_snapshot)?;
        }
        Ok(claimed)
    }

    fn start_queued_controls(&mut self, command_ids: Vec<String>) -> Result<()> {
        for command_id in command_ids {
            self.append_relay_event(
                Some(&command_id),
                RelayObservation::CommandStarted {
                    command_id: command_id.clone(),
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        Ok(())
    }

    fn queued_controls_before(&self, before_ordinal: u64) -> Vec<String> {
        let active_prompt_ordinal = self.snapshot.active_prompt.as_ref().and_then(|active| {
            self.snapshot
                .handled_commands
                .get(&active.command_id)
                .map(|handled| handled.accepted_ordinal)
        });
        let mut controls: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter_map(|(command_id, dispatch)| {
                if dispatch.state != RelayDispatchState::Queued
                    || !dispatch.command.is_effectful_acp()
                    || dispatch.command.is_queue_entry()
                {
                    return None;
                }
                let accepted = self
                    .snapshot
                    .handled_commands
                    .get(command_id)?
                    .accepted_ordinal;
                // Preserve controls accepted before the active prompt (they
                // must reach ACP first), but keep later controls queued until
                // that prompt finishes. The legacy Cancel control deliberately
                // bypasses a running prompt and may carry steering; CancelTurn
                // bypasses a pending checkpoint as well, but never steers.
                if active_prompt_ordinal.is_some_and(|prompt| accepted > prompt)
                    && !matches!(
                        dispatch.command,
                        RelayCommand::Cancel
                            | RelayCommand::Steer { .. }
                            | RelayCommand::CancelTurnFor { .. }
                            | RelayCommand::CancelTurn
                            | RelayCommand::GoalControl { .. }
                    )
                {
                    return None;
                }
                let running_turn =
                    self.snapshot.active_prompt.is_some() || self.snapshot.harness_turn.is_some();
                if accepted < before_ordinal
                    || (running_turn && matches!(dispatch.command, RelayCommand::CancelTurn))
                    || matches!(dispatch.command, RelayCommand::GoalControl { .. })
                {
                    Some((accepted, command_id.clone()))
                } else {
                    None
                }
            })
            .collect();
        controls.sort_by_key(|(ordinal, _)| *ordinal);
        controls
            .into_iter()
            .map(|(_, command_id)| command_id)
            .collect()
    }

    fn effectful_command_in_progress(&self) -> bool {
        self.snapshot.active_prompt.is_some()
            || self.snapshot.dispatches.values().any(|dispatch| {
                (dispatch.command.is_effectful_acp() || dispatch.command.is_effectful_user_shell())
                    && matches!(
                        dispatch.state,
                        RelayDispatchState::Pending | RelayDispatchState::InFlight
                    )
            })
    }

    fn next_queued_checkpoint(&self) -> Option<(String, u64)> {
        self.snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| {
                dispatch.state == RelayDispatchState::Queued
                    && matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
            })
            .filter_map(|(command_id, _)| {
                self.snapshot
                    .handled_commands
                    .get(command_id)
                    .map(|handled| (command_id.clone(), handled.accepted_ordinal))
            })
            .min_by_key(|(_, accepted)| *accepted)
    }

    pub fn capacity_retry_deadline(&self) -> Option<i64> {
        self.snapshot
            .capacity_retry
            .as_ref()
            .filter(|r| !r.submitted)
            .map(|r| r.retry_at_ms)
    }

    /// Admit a due retry through the same durable queue as an external prompt.
    pub fn submit_due_capacity_retry(&mut self, now_ms: i64) -> Result<bool> {
        let Some(retry) = self
            .snapshot
            .capacity_retry
            .as_ref()
            .filter(|r| !r.submitted)
        else {
            return Ok(false);
        };
        // Nothing else may be happening: a retry submits a prompt of its own,
        // so anything the session still owns would collide with it. That is
        // the shared quiet predicate, not a list of its own. The retry's own
        // armed state is cleared first, the same way a caller holding a
        // checkpoint barrier asks whether anything *else* is running: it is
        // the reason this is being asked, not a reason to refuse.
        let mut facts = self.activity_facts();
        facts.capacity_retry_armed = false;
        if retry.retry_at_ms > now_ms
            || self.background_work != BackgroundWorkPolicy::CodexExecCards
            || !mj_core::activity::is_quiet(&facts)
            || self.pending_close_barrier_id().is_some()
        {
            return Ok(false);
        }
        let id = retry.command_id.clone();
        let prompt = vec![ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new("Continue"),
        )];
        self.submit_command(&id, RelayCommand::Prompt { prompt })?
            .map_err(|error| anyhow!("submit capacity retry: {error:?}"))?;
        Ok(true)
    }

    pub fn record_command_completed(
        &mut self,
        command_id: &str,
        outcome: RelayCommandOutcome,
    ) -> Result<u64> {
        self.require_in_flight(command_id)?;
        if matches!(
            self.snapshot.dispatches[command_id].command,
            RelayCommand::BeginCheckpoint { .. }
        ) {
            bail!("checkpoint barriers complete through record_checkpoint_ready");
        }
        let mut outcome = outcome;
        if let RelayCommandOutcome::Prompt { stop_reason, .. } = &mut outcome {
            if self.background_work == BackgroundWorkPolicy::CodexExecCards
                && matches!(
                    stop_reason.as_str(),
                    "EndTurn" | "end_turn" | "Error" | "error"
                )
                && self.capacity_response.at_capacity()
            {
                *stop_reason = CAPACITY_STOP_REASON.to_owned();
            }
            self.capacity_response = CapacityResponse::default();
        }
        let finishes_turn = matches!(outcome, RelayCommandOutcome::Prompt { .. });
        let classify_reply = matches!(&outcome, RelayCommandOutcome::Prompt { stop_reason, .. }
            if mj_core::state::classify_prompt_completion(stop_reason) == mj_core::state::PromptCompletion::Finished);
        // Report confirmed changes in the conversation on every surface.
        // The command identity makes retries of this append project only once.
        let notice = self
            .snapshot
            .dispatches
            .get(command_id)
            .and_then(|dispatch| match (&dispatch.command, &outcome) {
                (RelayCommand::SetConfig { key, value }, RelayCommandOutcome::Configured) => {
                    Some(format!("{key} set to {value}"))
                }
                (RelayCommand::SetSessionMode { mode_id }, RelayCommandOutcome::SessionModeSet) => {
                    Some(format!("Session mode: {mode_id}"))
                }
                (RelayCommand::GoalControl { action }, RelayCommandOutcome::GoalControlled) => {
                    Some(format!("Goal: {}", action.as_str()))
                }
                _ => None,
            });
        if let Some(message) = notice {
            self.append_relay_event(Some(command_id), RelayObservation::Notice { message })?;
        }
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandCompleted {
                command_id: command_id.to_owned(),
                outcome,
            },
        )?;
        if finishes_turn && !self.snapshot.goal.running() {
            self.finish_turn_activity()?;
        }
        self.replied_verdict_pending |= classify_reply;
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_command_rejected(
        &mut self,
        command_id: &str,
        message: impl Into<String>,
    ) -> Result<u64> {
        self.require_dispatch(command_id)?;
        let command = self.snapshot.dispatches[command_id].command.kind();
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandRejected {
                command_id: command_id.to_owned(),
                command,
                message: message.into(),
            },
        )?;
        if command == RelayCommandKind::Prompt {
            self.finish_turn_activity()?;
        }
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_command_interrupted(
        &mut self,
        command_id: &str,
        message: impl Into<String>,
    ) -> Result<u64> {
        self.require_dispatch(command_id)?;
        let command = self.snapshot.dispatches[command_id].command.kind();
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandInterrupted {
                command_id: command_id.to_owned(),
                command,
                message: message.into(),
            },
        )?;
        if command == RelayCommandKind::Prompt {
            self.finish_turn_activity()?;
        }
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_checkpoint_ready(&mut self, command_id: &str) -> Result<u64> {
        self.require_in_flight(command_id)?;
        if self.snapshot.checkpoint_barrier.as_deref() != Some(command_id) {
            bail!("checkpoint barrier {command_id} is not active");
        }
        if self.snapshot.checkpoint_ready_through.is_some() {
            bail!("checkpoint barrier {command_id} is already ready");
        }
        if !matches!(
            self.snapshot.dispatches[command_id].command,
            RelayCommand::BeginCheckpoint { .. }
        ) {
            bail!("command {command_id} is not a checkpoint barrier");
        }
        let through = self
            .snapshot
            .latest_ordinal
            .checked_add(1)
            .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
        self.append_relay_event(
            Some(command_id),
            RelayObservation::CheckpointReady {
                command_id: command_id.to_owned(),
                through,
            },
        )
    }

    /// Release checkpoint barriers owned by a controller connection that
    /// disappeared. The runtime calls this when that connection drops so an
    /// offline prompt queue can never remain paused indefinitely.
    pub fn cancel_checkpoint_barrier_on_disconnect(
        &mut self,
        command_id: &str,
    ) -> Result<Option<u64>> {
        let Some(dispatch) = self.snapshot.dispatches.get(command_id) else {
            return Ok(None);
        };
        if !matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
            || !matches!(
                dispatch.state,
                RelayDispatchState::Queued
                    | RelayDispatchState::Pending
                    | RelayDispatchState::InFlight
            )
        {
            return Ok(None);
        }
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandInterrupted {
                command_id: command_id.to_owned(),
                command: RelayCommandKind::BeginCheckpoint,
                message: "checkpoint barrier cancelled because its controller disconnected"
                    .to_owned(),
            },
        )?;
        self.promote_next_queued_command()?;
        Ok(Some(ordinal))
    }

    fn require_dispatch(&self, command_id: &str) -> Result<()> {
        if !self.snapshot.dispatches.contains_key(command_id) {
            bail!("unknown relay command {command_id}");
        }
        Ok(())
    }

    fn require_in_flight(&self, command_id: &str) -> Result<()> {
        let Some(dispatch) = self.snapshot.dispatches.get(command_id) else {
            bail!("unknown relay command {command_id}");
        };
        if dispatch.state != RelayDispatchState::InFlight {
            bail!("relay command {command_id} is not in flight");
        }
        Ok(())
    }

    fn close_requested(&self) -> bool {
        self.pending_close_barrier_id().is_some()
    }

    fn pending_close_barrier_id(&self) -> Option<&str> {
        self.snapshot.dispatches.values().find_map(|dispatch| {
            if matches!(
                dispatch.state,
                RelayDispatchState::Completed
                    | RelayDispatchState::Rejected
                    | RelayDispatchState::Interrupted
            ) {
                return None;
            }
            match &dispatch.command {
                RelayCommand::Close {
                    barrier_command_id, ..
                } => Some(barrier_command_id.as_str()),
                _ => None,
            }
        })
    }

    /// Start the head of the durable command queue once the relay is idle.
    /// Entries run strictly one at a time, in the order they were accepted.
    pub(super) fn promote_next_queued_command(&mut self) -> Result<Option<u64>> {
        // A turn the harness started on its own leaves execution Running, but
        // it must not hold a queued prompt: the adapter queues a prompt that
        // arrives mid-turn and answers it as soon as that turn ends.
        // `active_prompt` is the real gate on dispatch.
        if self.checkpoint_only
            || self.snapshot.active_prompt.is_some()
            || self
                .snapshot
                .steering
                .as_ref()
                .is_some_and(|s| s.holds_queue())
            || self.promoted_config_in_progress()
            || self.snapshot.checkpoint_barrier.is_some()
            || matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
            || self.pending_checkpoint_barrier()
            || self.close_requested()
        {
            return Ok(None);
        }
        let Some(queued) = self.snapshot.queued_prompts.first().cloned() else {
            return Ok(None);
        };
        let queued_ordinal = self
            .snapshot
            .handled_commands
            .get(&queued.command_id)
            .map_or(u64::MAX, |handled| handled.accepted_ordinal);
        if self.snapshot.active_user_shells.keys().any(|command_id| {
            self.snapshot
                .handled_commands
                .get(command_id)
                .is_some_and(|handled| handled.accepted_ordinal < queued_ordinal)
        }) {
            return Ok(None);
        }
        let ordinal = self.append_relay_event(
            Some(&queued.command_id),
            RelayObservation::CommandStarted {
                command_id: queued.command_id.clone(),
                started_at_ms: epoch_millis(),
            },
        )?;
        Ok(Some(ordinal))
    }

    /// A promoted configuration change leaves execution idle while it reaches
    /// ACP, so the queue needs its own guard to stay sequential. Completion,
    /// rejection, and interruption all promote the next entry.
    fn promoted_config_in_progress(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::SetConfig { .. })
                && matches!(
                    dispatch.state,
                    RelayDispatchState::Pending | RelayDispatchState::InFlight
                )
        })
    }

    fn pending_checkpoint_barrier(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
                && matches!(
                    dispatch.state,
                    RelayDispatchState::Queued
                        | RelayDispatchState::Pending
                        | RelayDispatchState::InFlight
                )
        })
    }
}
