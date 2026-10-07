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
        if let RelayCommand::DeliverMailboxEvent { event } = &command
            && (self
                .snapshot
                .pending_mailbox_events
                .iter()
                .any(|pending| pending.key == event.key)
                || self
                    .snapshot
                    .delivered_mailbox_event_keys
                    .contains_key(&event.key)
                || self
                    .snapshot
                    .mailbox_hook_lease
                    .as_ref()
                    .is_some_and(|lease| lease.events.iter().any(|leased| leased.key == event.key)))
        {
            // A producer may retry after losing the ACK, or independently
            // submit the same event under a fresh command ID. Pending events
            // are always deduplicated; delivered keys cover the retry window.
            return Ok(Ok(RelayResponsePayload::Accepted {
                command_id: command_id.to_owned(),
                ordinal: self.snapshot.latest_ordinal,
            }));
        }
        if self.clear_context_in_progress() {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "Context is being cleared; wait for the new conversation",
                false,
                None,
            )));
        }
        if let RelayCommand::InstallPromptContext { text } = &command
            && (text.trim().is_empty()
                || self.snapshot.active_prompt.is_some()
                || self
                    .snapshot
                    .pending_prompt_context
                    .as_ref()
                    .is_some_and(|context| context.attached_command_id.is_some()))
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "cannot install empty prompt context or replace context attached to an active prompt",
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
                || self.unfinished_dispatch()
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
        if let RelayCommand::DeliverMailboxEvent { event } = &command
            && (event.key.trim().is_empty()
                || event.key.len() > 1024
                || event.source.trim().is_empty()
                || !event.has_renderable_content())
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "mailbox event key, source, and content are required",
                false,
                None,
            )));
        }
        if let RelayCommand::DeliverMailboxEvent { event } = &command {
            let mut pending = self.snapshot.pending_mailbox_events.clone();
            if let Some(lease) = &self.snapshot.mailbox_hook_lease {
                pending.extend(lease.events.iter().cloned());
            }
            pending.push(event.clone());
            if ensure_serialized_budget(
                &pending,
                RELAY_MAILBOX_BYTE_BUDGET,
                "pending mailbox events",
            )
            .is_err()
            {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidRequest,
                    "session mailbox is full; deliver pending events before adding another",
                    false,
                    None,
                )));
            }
        }
        if let RelayCommand::MailboxWake { events } = &command
            && (events.is_empty()
                || !events.iter().any(|event| event.wake)
                || events != &self.snapshot.pending_mailbox_events)
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "mailbox wake must carry all currently pending events, including a waking event",
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
        // Stop is also accepted while Claude Code works on its own after a
        // background task: the cancel interrupts the running cycle, and that
        // cycle's result ends the turn (`claude_turn_result`). A Codex turn of
        // this kind is a native goal, which has its own controls.
        let claude_harness_turn = self.harness_turns == HarnessTurnPolicy::ClaudeAdapter
            && self.snapshot.harness_turn.is_some();
        if let RelayCommand::Cancel = command
            && self
                .snapshot
                .continuation
                .quota_recovery
                .as_ref()
                .is_none_or(|r| r.submitted)
            && self.snapshot.active_prompt.is_none()
            && !claude_harness_turn
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
        if let RelayCommand::SetQuotaRecovery { expected, recovery } = &command {
            let valid = expected.ordinal == self.snapshot.latest_ordinal
                && expected.digest == self.snapshot.latest_digest
                && recovery.as_ref().is_none_or(|r| {
                    self.quota_recovery_admissible(&r.user_command_id, &r.completed_command_id)
                        && !r.submitted
                        && r.notice.len() <= 4096
                        && !r.profile_id.is_empty()
                        && match (r.reset_at_ms, r.retry_at_ms) {
                            (Some(reset), Some(retry)) => reset.checked_add(60_000) == Some(retry),
                            (None, None) => true,
                            _ => false,
                        }
                });
            if !valid {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "quota recovery evidence is stale",
                    false,
                    None,
                )));
            }
        }
        if let RelayCommand::GoalControl { action } = &command
            && mj_core::continuation::is_quota_goal_resume(command_id)
        {
            let valid = *action == mj_core::goal::GoalControlAction::Resume
                && self.snapshot.goal.resumable_after_quota()
                && mj_core::activity::can_submit(&self.activity_facts())
                && self
                    .snapshot
                    .continuation
                    .quota_recovery
                    .as_ref()
                    .is_some_and(|r| {
                        !r.submitted
                            && r.retry_at_ms
                                .is_some_and(|deadline| deadline <= epoch_millis())
                            && self.quota_recovery_admissible(
                                &r.user_command_id,
                                &r.completed_command_id,
                            )
                    });
            if !valid {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "quota goal resume is not due or its goal changed",
                    false,
                    None,
                )));
            }
        }
        if let RelayCommand::ResumeAfterQuota {
            expected,
            completed_command_id,
        } = &command
        {
            let valid = expected.ordinal == self.snapshot.latest_ordinal
                && expected.digest == self.snapshot.latest_digest
                && self
                    .snapshot
                    .continuation
                    .quota_recovery
                    .as_ref()
                    .is_some_and(|r| {
                        !r.submitted
                            && r.completed_command_id == *completed_command_id
                            && r.retry_at_ms
                                .is_some_and(|deadline| deadline <= epoch_millis())
                            && mj_core::activity::can_submit(&self.activity_facts())
                            && self
                                .quota_recovery_admissible(&r.user_command_id, completed_command_id)
                    });
            if !valid {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "quota recovery is not due or its evidence changed",
                    false,
                    None,
                )));
            }
        }
        if let RelayCommand::HandbackReminder {
            completed_command_id,
            completed_ordinal,
        } = &command
        {
            let current = self.snapshot.turn_completion.as_ref().is_some_and(|turn| {
                turn.command_id == *completed_command_id
                    && turn.completed_ordinal == *completed_ordinal
            });
            if !current
                || self
                    .snapshot
                    .latest_prompt_accepted_ordinal
                    .is_none_or(|ordinal| ordinal >= *completed_ordinal)
                || self
                    .snapshot
                    .last_harness_turn_started_ordinal
                    .is_some_and(|ordinal| ordinal > *completed_ordinal)
                || self
                    .snapshot
                    .native_session_opened_ordinal
                    .is_some_and(|ordinal| ordinal > *completed_ordinal)
                || self.snapshot.active_prompt.is_some()
                || !self.snapshot.queued_prompts.is_empty()
                || !mj_core::activity::can_submit(&self.activity_facts())
                || self.pending_close_barrier_id().is_some()
            {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "handback reminder completion is stale or the worker has accepted newer input",
                    false,
                    None,
                )));
            }
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
            // A background command that Jev judged idle does not hold this
            // back, and plan mode does not either: the prompt cannot get past
            // plan approval, and Jev already weighs whether input is needed.
            if self.snapshot.assessment.as_ref().is_some_and(|a| {
                !a.current() || a.action != Some(mj_core::assessment::Action::Continue)
            }) || !self.snapshot.assessment_questions.is_empty()
                || state.quota_recovery.as_ref().is_some_and(|r| !r.submitted)
                || self.snapshot.goal.budget_limited()
                || !state.eligible()
                || state.user_command_id.as_ref() != Some(user_command_id)
                || state.completed_command_id.as_ref() != Some(completed_command_id)
                || *attempt != state.attempts + 1
                || self.snapshot.latest_ordinal != expected.ordinal
                || self.snapshot.latest_digest != expected.digest
                || !mj_core::activity::can_submit(&facts)
                || mj_core::activity::driver_present(&facts)
                || !self.snapshot.queued_prompts.is_empty()
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
        if let RelayCommand::SeedAssessmentContext { expected, context } = &command
            && (expected.ordinal != self.snapshot.latest_ordinal
                || expected.digest != self.snapshot.latest_digest
                || context.evidence().validate().is_err())
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "assessment context frontier changed or evidence is invalid",
                false,
                None,
            )));
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
        if let Some(message) = match &command {
            RelayCommand::RecordNotice { text } => Some(text.clone()),
            RelayCommand::SetQuotaRecovery {
                recovery: Some(recovery),
                ..
            } => Some(recovery.notice.clone()),
            _ => None,
        } {
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
            RelayCommand::RecordNotice { .. }
            | RelayCommand::SetQuotaRecovery { .. }
            | RelayCommand::SeedAssessmentContext { .. }
            | RelayCommand::InstallPromptContext { .. }
            | RelayCommand::DeliverMailboxEvent { .. }
            | RelayCommand::ResolveSteering { .. } => RelayCommandOutcome::NoticeRecorded,
            _ => RelayCommandOutcome::QueueChanged {
                removed_command_ids,
            },
        };
        self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandCompleted {
                barrier_command_id: match &self.snapshot.dispatches[command_id].command {
                    RelayCommand::CompleteCheckpoint { barrier_command_id }
                    | RelayCommand::ReleaseCheckpoint { barrier_command_id } => {
                        Some(barrier_command_id.clone())
                    }
                    _ => None,
                },
                command: Some(self.snapshot.dispatches[command_id].command.kind()),
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
        self.expire_mailbox_hook_lease_at(epoch_millis())?;
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
            self.record_command_rejected(
                &id,
                mj_core::event_outcome::OutcomeReason::AdmissionRejected,
                "The requested turn is no longer running",
            )?;
        }
        self.submit_automatic_steer()?;
        self.promote_next_queued_command()?;
        if self
            .snapshot
            .steering
            .as_ref()
            .is_some_and(|s| s.holds_queue())
        {
            while let Some((barrier_id, _)) = self.next_queued_checkpoint() {
                self.record_command_rejected(&barrier_id, mj_core::event_outcome::OutcomeReason::AdmissionRejected, "Resolve uncertain steering delivery before checkpointing or moving this session")?;
            }
        }
        if self.snapshot.checkpoint_barrier.is_none() {
            if let Some((barrier_id, barrier_ordinal)) = self.next_queued_checkpoint() {
                let mut earlier_controls = self.queued_controls_before(barrier_ordinal);
                if !earlier_controls.is_empty() {
                    earlier_controls.truncate(maximum);
                    self.start_queued_controls(earlier_controls)?;
                // Checkpoint admission and command promotion use the same
                // relay-owned turn-boundary predicate.
                } else if !self.effectful_command_in_progress()
                    && !self
                        .snapshot
                        .steering
                        .as_ref()
                        .is_some_and(|s| s.holds_queue())
                    && !self.turn_in_progress()
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
        if let Some((_, command_id)) = claimable.iter().find(|(_, command_id)| {
            self.snapshot
                .dispatches
                .get(command_id)
                .is_some_and(|dispatch| mailbox_prompt_eligible(&dispatch.command))
        }) && !self.snapshot.pending_mailbox_events.is_empty()
        {
            let event_keys = self
                .snapshot
                .pending_mailbox_events
                .iter()
                .map(|event| event.key.clone())
                .collect();
            self.append_relay_event(
                None,
                RelayObservation::MailboxEventsDelivered {
                    event_keys,
                    path: mj_core::mailbox::MailboxDeliveryPath::Prompt,
                    prompt_command_id: Some(command_id.clone()),
                    hook_event: None,
                    events: self.snapshot.pending_mailbox_events.clone(),
                    lease_id: None,
                },
            )?;
        }
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
            let hidden_prompt_context = mailbox_prompt_eligible(&dispatch.command)
                .then(|| {
                    let mut contexts = Vec::new();
                    if let Some(context) = next_snapshot.mailbox_prompt_contexts.get(&command_id) {
                        contexts.push(context.clone());
                    }
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
        }
        Ok(claimed)
    }

    /// Steer the head of the queue into the running prompt without being
    /// asked. It goes through admission like an Escape steer, so the journal
    /// and every client see the same steering operation.
    fn submit_automatic_steer(&mut self) -> Result<()> {
        let Some((active_prompt_id, queued_prompt_id)) = self.automatic_steer_target() else {
            return Ok(());
        };
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow!("generate automatic steer id: {error}"))?;
        let command_id = format!("auto-steer-{}", mj_core::hex::lower_hex(random));
        if let Err(error) = self.submit_command(
            &command_id,
            RelayCommand::Steer {
                active_prompt_id,
                queued_prompt_id,
            },
        )? {
            // Admission saw something the target check did not; the prompt
            // simply stays queued, as it would without automatic steering.
            tracing::debug!(?error, "automatic steer was not admitted");
        }
        Ok(())
    }

    /// The running prompt and the queued prompt to steer into it, when the
    /// bridge would return the prompt instead of starting a turn and nothing
    /// is waiting for the running turn to end.
    fn automatic_steer_target(&self) -> Option<(String, String)> {
        if !self.automatic_steering || self.checkpoint_only {
            return None;
        }
        // Automatic steering is only useful for a Hel-owned prompt. In a
        // harness-initiated turn, the agent cannot read the steer until its
        // current tool call returns; Esc can cancel that turn instead.
        let active = &self.snapshot.active_prompt.as_ref()?.command_id;
        if self.snapshot.cancelling_prompt_id.is_some()
            || self.snapshot.checkpoint_barrier.is_some()
            || self.next_queued_checkpoint().is_some()
            || self.pending_close_barrier_id().is_some()
        {
            return None;
        }
        // One steer at a time, and none after a steer into this same turn
        // failed or came back: that turn is ending or cannot take input.
        if let Some(steering) = &self.snapshot.steering
            && (steering.holds_queue()
                || (steering.active_prompt_id == *active
                    && steering.status != mj_core::relay::SteeringStatus::Applied))
        {
            return None;
        }
        let head = self.snapshot.queued_prompts.first()?;
        let StoredQueuedRelayPayload::Prompt { prompt } = &head.payload else {
            return None;
        };
        // Commands run at turn boundaries, not inside another turn.
        if mj_core::acp::prompt_is_slash_command(prompt)
            || self.snapshot.dispatches.get(&head.command_id)?.state != RelayDispatchState::Queued
        {
            return None;
        }
        Some((active.clone(), head.command_id.clone()))
    }

    /// Advance only lifecycle commands after the old owning process was stopped.
    /// No ACP channel or harness readiness is involved in this mode.
    pub fn dispatch_checkpoint_only(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.checkpoint_only,
            "worker is not in checkpoint-only mode"
        );
        self.dispatch_relay_local_lifecycle()
    }

    /// Advance checkpoint and close commands while harness preparation owns no
    /// ACP process. Pending prompts and other ACP commands stay queued.
    pub fn dispatch_preparation_lifecycle(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.checkpoint_only,
            "checkpoint-only workers use dispatch_checkpoint_only"
        );
        self.dispatch_relay_local_lifecycle()
    }

    fn dispatch_relay_local_lifecycle(&mut self) -> Result<()> {
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
                let running_turn = self.turn_in_progress();
                // ACP rejects session configuration requests while its prompt
                // loop is running. Keep these queued until that turn ends;
                // GoalControl and turn controls have dedicated live-turn
                // handlers, and user shells run outside the ACP command loop.
                if running_turn
                    && matches!(
                        dispatch.command,
                        RelayCommand::ClearContext
                            | RelayCommand::RestoreExecutionMode
                            | RelayCommand::SetSessionMode { .. }
                    )
                {
                    return None;
                }
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
        self.snapshot.dispatches.values().any(|dispatch| {
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

    /// Whether a quota recovery for this completion may be recorded. A turn,
    /// background work, a goal or plan mode does not prevent that, since none
    /// gets past the limit; resuming also needs `can_submit`.
    fn quota_recovery_admissible(&self, user: &str, completed: &str) -> bool {
        let c = &self.snapshot.continuation;
        !c.quota_suppressed
            && self.snapshot.assessment.as_ref().is_none_or(|a| {
                a.current() && a.action == Some(mj_core::assessment::Action::RecoverQuota)
            })
            && self.snapshot.assessment_questions.is_empty()
            && !self.snapshot.goal.budget_limited()
            && c.user_command_id.as_deref() == Some(user)
            && c.completed_command_id.as_deref() == Some(completed)
            && self.snapshot.queued_prompts.is_empty()
            && self.pending_close_barrier_id().is_none()
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
        if !self.snapshot.assessment_questions.is_empty()
            || self.snapshot.goal.budget_limited()
            || self
                .snapshot
                .goal
                .snapshot
                .as_ref()
                .is_some_and(|g| g.status == "paused")
            || retry.retry_at_ms > now_ms
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
        let awaiting_input = matches!(&outcome, RelayCommandOutcome::Prompt { stop_reason, .. }
            if stop_reason == mj_core::acp::AWAITING_INPUT_STOP_REASON);
        let finishes_turn = matches!(outcome, RelayCommandOutcome::Prompt { .. });
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
                (RelayCommand::RestoreExecutionMode, RelayCommandOutcome::Configured) => self
                    .snapshot
                    .config
                    .get("mode")
                    .map(|mode| format!("mode restored to {mode}")),
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
        let stop_applied = matches!(outcome, RelayCommandOutcome::Cancelled)
            && self
                .snapshot
                .dispatches
                .get(command_id)
                .is_some_and(|dispatch| {
                    matches!(
                        dispatch.command,
                        RelayCommand::Cancel | RelayCommand::CancelTurn
                    )
                });
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandCompleted {
                barrier_command_id: match &self.snapshot.dispatches[command_id].command {
                    RelayCommand::CompleteCheckpoint { barrier_command_id }
                    | RelayCommand::ReleaseCheckpoint { barrier_command_id } => {
                        Some(barrier_command_id.clone())
                    }
                    _ => None,
                },
                command: Some(self.snapshot.dispatches[command_id].command.kind()),
                command_id: command_id.to_owned(),
                outcome,
            },
        )?;
        if stop_applied {
            self.note_harness_turn_stop();
        }
        if finishes_turn && !self.snapshot.goal.running() {
            self.finish_turn_activity()?;
        }
        if awaiting_input {
            // The running classifier already established the handoff. Keep independently
            // tracked children and their controls while making the parent ready for input.
            self.apply_replied_decision(
                self.turn_context.generation(),
                mj_core::activity::verdict::Decision::InferIdle,
                mj_core::clock::epoch_millis(),
            )?;
        }
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_command_rejected(
        &mut self,
        command_id: &str,
        reason: mj_core::event_outcome::OutcomeReason,
        message: impl Into<String>,
    ) -> Result<u64> {
        self.require_dispatch(command_id)?;
        let command = self.snapshot.dispatches[command_id].command.kind();
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandRejected {
                reason: Some(reason),
                command_id: command_id.to_owned(),
                command,
                message: message.into(),
            },
        )?;
        if command == RelayCommandKind::Prompt {
            self.finish_turn_activity()?;
        }
        self.settle_held_writes()?;
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_command_interrupted(
        &mut self,
        command_id: &str,
        reason: mj_core::event_outcome::OutcomeReason,
        message: impl Into<String>,
    ) -> Result<u64> {
        self.require_dispatch(command_id)?;
        let command = self.snapshot.dispatches[command_id].command.kind();
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandInterrupted {
                reason: Some(reason),
                command_id: command_id.to_owned(),
                command,
                message: message.into(),
            },
        )?;
        if command == RelayCommandKind::Prompt {
            self.finish_turn_activity()?;
        }
        self.settle_held_writes()?;
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
                reason: Some(mj_core::event_outcome::OutcomeReason::ControllerDisconnected),
                command_id: command_id.to_owned(),
                command: RelayCommandKind::BeginCheckpoint,
                message: "checkpoint barrier cancelled because its controller disconnected"
                    .to_owned(),
            },
        )?;
        self.settle_held_writes()?;
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

    /// Whether an accepted Close is waiting to run. The relay is sealed at its
    /// checkpoint cut from the moment it accepts one.
    pub fn close_pending(&self) -> bool {
        self.close_requested()
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
        // Keep turn-starting entries and configuration queue entries in this
        // durable FIFO during a live turn. Claude's adapter queues mid-cycle
        // prompts internally and cancel may discard them; ACP rejects
        // configuration changes while its prompt loop is running.
        if self.checkpoint_only
            || self.turn_in_progress()
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
        self.expire_mailbox_hook_lease_at(epoch_millis())?;
        let Some(queued) = self.snapshot.queued_prompts.first().cloned() else {
            if self.mailbox_wake_is_allowed() {
                self.enqueue_mailbox_wake()?;
                return self.promote_next_queued_command();
            }
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

    fn mailbox_wake_is_allowed(&self) -> bool {
        self.snapshot
            .pending_mailbox_events
            .iter()
            .any(|event| event.wake)
            && self.activity_is_idle()
            && self.snapshot.cancelling_prompt_id.is_none()
            && !self.snapshot.dispatches.values().any(|dispatch| {
                matches!(
                    dispatch.command,
                    RelayCommand::Cancel
                        | RelayCommand::CancelTurn
                        | RelayCommand::CancelTurnFor { .. }
                ) && matches!(
                    dispatch.state,
                    RelayDispatchState::Queued
                        | RelayDispatchState::Pending
                        | RelayDispatchState::InFlight
                )
            })
    }

    fn enqueue_mailbox_wake(&mut self) -> Result<()> {
        let events = self.snapshot.pending_mailbox_events.clone();
        anyhow::ensure!(
            !events.is_empty() && events.iter().any(|event| event.wake),
            "mailbox wake lost its waking event"
        );
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow!("generate mailbox wake ID: {error}"))?;
        let command_id = format!("mailbox-wake-{}", mj_core::hex::lower_hex(random));
        self.append_relay_event(
            Some(&command_id),
            RelayObservation::CommandQueued {
                command_id: command_id.clone(),
                command: RelayCommand::MailboxWake { events },
                created_at_ms: epoch_millis(),
            },
        )?;
        Ok(())
    }

    /// Atomically lease pending events to one harness hook.
    pub fn drain_mailbox(&mut self, hook_event: &str) -> Result<RelayResponsePayload> {
        anyhow::ensure!(
            matches!(hook_event, "PostToolUse" | "PostToolBatch"),
            "unsupported mailbox hook event"
        );
        self.expire_mailbox_hook_lease_at(epoch_millis())?;
        if self.snapshot.mailbox_hook_lease.is_some() {
            return Ok(RelayResponsePayload::MailboxDrained {
                lease_id: None,
                text: None,
                count: 0,
            });
        }
        let events = self.snapshot.pending_mailbox_events.clone();
        if events.is_empty() {
            return Ok(RelayResponsePayload::MailboxDrained {
                lease_id: None,
                text: None,
                count: 0,
            });
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow!("generate mailbox hook lease ID: {error}"))?;
        let lease = mj_core::relay::MailboxHookLease {
            lease_id: format!("mailbox-hook-{}", mj_core::hex::lower_hex(random)),
            events: events.clone(),
            hook_event: hook_event.to_owned(),
            expires_at_ms: epoch_millis()
                .saturating_add(mj_core::mailbox::MAILBOX_HOOK_LEASE_TIMEOUT_MS),
        };
        let text = mj_core::mailbox::render_mailbox_events(&events);
        self.append_relay_event(
            None,
            RelayObservation::MailboxHookLeaseCreated {
                lease: lease.clone(),
            },
        )?;
        Ok(RelayResponsePayload::MailboxDrained {
            lease_id: Some(lease.lease_id),
            text: Some(text),
            count: events.len(),
        })
    }

    pub(super) fn ack_mailbox(&mut self, lease_id: &str) -> Result<RelayResponsePayload> {
        let Some(lease) = self.snapshot.mailbox_hook_lease.as_ref() else {
            return Ok(RelayResponsePayload::MailboxAcknowledged {
                acknowledged: false,
            });
        };
        if lease.lease_id != lease_id {
            return Ok(RelayResponsePayload::MailboxAcknowledged {
                acknowledged: false,
            });
        }
        let events = lease.events.clone();
        let event_keys = events.iter().map(|event| event.key.clone()).collect();
        let hook_event = lease.hook_event.clone();
        self.append_relay_event(
            None,
            RelayObservation::MailboxEventsDelivered {
                event_keys,
                path: mj_core::mailbox::MailboxDeliveryPath::ToolHook,
                prompt_command_id: None,
                hook_event: Some(hook_event),
                events,
                lease_id: Some(lease_id.to_owned()),
            },
        )?;
        Ok(RelayResponsePayload::MailboxAcknowledged { acknowledged: true })
    }

    fn expire_mailbox_hook_lease_at(&mut self, now_ms: i64) -> Result<()> {
        if self
            .snapshot
            .mailbox_hook_lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at_ms <= now_ms)
        {
            self.return_mailbox_hook_lease(mj_core::relay::MailboxHookLeaseReturnReason::Timeout)?;
        }
        Ok(())
    }

    pub fn mailbox_hook_lease_deadline(&self) -> Option<i64> {
        self.snapshot
            .mailbox_hook_lease
            .as_ref()
            .map(|lease| lease.expires_at_ms)
    }

    /// Return an expired hook lease and immediately reconsider the idle wake.
    pub fn expire_mailbox_hook_lease_and_promote(&mut self, now_ms: i64) -> Result<bool> {
        if self
            .mailbox_hook_lease_deadline()
            .is_none_or(|deadline| deadline > now_ms)
        {
            return Ok(false);
        }
        self.expire_mailbox_hook_lease_at(now_ms)?;
        self.promote_next_queued_command()?;
        Ok(true)
    }

    pub(super) fn return_mailbox_hook_lease(
        &mut self,
        reason: mj_core::relay::MailboxHookLeaseReturnReason,
    ) -> Result<()> {
        let Some(lease_id) = self
            .snapshot
            .mailbox_hook_lease
            .as_ref()
            .map(|lease| lease.lease_id.clone())
        else {
            return Ok(());
        };
        self.append_relay_event(
            None,
            RelayObservation::MailboxHookLeaseReturned { lease_id, reason },
        )?;
        Ok(())
    }

    /// A promoted configuration change leaves execution idle while it reaches
    /// ACP, so the queue needs its own guard to stay sequential. Completion,
    /// rejection, and interruption all promote the next entry.
    /// Whether any dispatch is still waiting or running. Terminal records stay
    /// in the ledger until the daemon acknowledges their events, so they are
    /// history, not work.
    fn unfinished_dispatch(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(
                dispatch.state,
                RelayDispatchState::Queued
                    | RelayDispatchState::Pending
                    | RelayDispatchState::InFlight
            )
        })
    }

    /// Whether a `/clear` is still waiting or running. A rejected or
    /// interrupted clear leaves the previous conversation in place, so it
    /// must not hold later work back.
    pub(crate) fn clear_context_in_progress(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::ClearContext)
                && matches!(
                    dispatch.state,
                    RelayDispatchState::Queued
                        | RelayDispatchState::Pending
                        | RelayDispatchState::InFlight
                )
        })
    }

    fn promoted_config_in_progress(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::SetConfig { .. })
                && matches!(
                    dispatch.state,
                    RelayDispatchState::Pending | RelayDispatchState::InFlight
                )
        })
    }

    pub(super) fn pending_checkpoint_barrier(&self) -> bool {
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

fn mailbox_prompt_eligible(command: &RelayCommand) -> bool {
    let Some(prompt) = command.prompt_blocks() else {
        return false;
    };
    !mj_core::acp::prompt_requests_compaction(&prompt)
        && !mj_core::acp::prompt_is_slash_command(&prompt)
        && mj_core::acp::context_command(&prompt).is_none()
}
