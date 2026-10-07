use super::*;

pub(super) fn project_observation(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    event: &RelayEvent,
    mutation: &mut MaterializedSessionMutation,
) -> Result<()> {
    match &event.observation {
        RelayObservation::SteeringUnconfirmed { .. } => {}
        RelayObservation::NativeAgent { .. } => {
            mutation.native_agent = Some(event.clone());
        }
        RelayObservation::AgentInitialized { .. } => {}
        RelayObservation::SessionOpened { resumed, .. } => {
            mutation.pending_elicitations = Some(Vec::new());
            if !resumed {
                push_system(mutation, event, "harness session started");
            }
        }
        RelayObservation::SessionConfigured { config_options } => {
            let mut configuration = current.configuration.clone();
            configuration.values = configuration_values(config_options)?;
            mutation.configuration = Some(configuration);
        }
        RelayObservation::SessionModesConfigured { .. } => {}
        RelayObservation::SessionUpdate { update } => {
            project_session_update(current, index, event, update, mutation)?;
        }
        RelayObservation::PermissionAutoApproved {
            option_id,
            option_name,
        } => push_system(
            mutation,
            event,
            format!("permission auto-approved: {option_name} ({option_id})"),
        ),
        RelayObservation::ElicitationRequested { request } => {
            let mut pending = current.pending_elicitations.clone();
            pending.retain(|existing| existing.id != request.id);
            pending.push(request.clone());
            mutation.pending_elicitations = Some(pending);
            // A plan decision also becomes a durable transcript item at the
            // point the harness proposed it, so the proposal renders inline
            // after the conversation that produced it and outlives both the
            // decision dialog and the session's process.
            if let Some(plan) = mj_core::acp::plan_review_proposal(request) {
                close_streams(index, mutation, event.recorded_at_ms);
                upsert(
                    mutation,
                    TranscriptItem {
                        stable_id: plan_proposal_item_id(event.ordinal),
                        position: event.ordinal,
                        latest_content_event_ordinal: None,
                        created_at_ms: event.recorded_at_ms,
                        last_changed_at_ms: event.recorded_at_ms,
                        body: TranscriptBody::PlanProposal {
                            proposal_id: request.id.clone(),
                            plan: plan.to_owned(),
                        },
                    },
                );
            }
        }
        RelayObservation::ElicitationResolved { elicitation_id, .. } => {
            let mut pending = current.pending_elicitations.clone();
            pending.retain(|request| request.id != *elicitation_id);
            mutation.pending_elicitations = Some(pending);
        }
        RelayObservation::ElicitationsCleared => {
            mutation.pending_elicitations = Some(Vec::new());
        }
        RelayObservation::CommandQueued {
            command_id,
            command,
            created_at_ms,
        } => match command {
            RelayCommand::ClearContext => {
                mutation.execution = Some(MaterializedExecutionState::Running {
                    started_at_ms: *created_at_ms,
                });
                push_system(mutation, event, "Clearing context…");
            }
            // Every command that carries a prompt starts a user turn, so the
            // transcript shows what the harness was told. Mjolnir's own
            // prompts (a handback reminder, a quota resume) included.
            command if command.prompt_blocks().is_some() => {
                let prompt = command.prompt_blocks().expect("prompt command");
                if let RelayCommand::ContinueAuthorizedWork { attempt, .. } = command {
                    push_system(
                        mutation,
                        event,
                        format!("Continuing requested work automatically · {attempt} of 3"),
                    );
                }
                if let RelayCommand::MailboxWake { events } = command {
                    push_system(
                        mutation,
                        event,
                        format!(
                            "Delivered {} in a wake prompt",
                            mailbox_event_count(events.len())
                        ),
                    );
                }
                let mailbox_wake = matches!(command, RelayCommand::MailboxWake { .. });
                let content = if mailbox_wake {
                    Vec::new()
                } else {
                    prompt
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<serde_json::Result<Vec<_>>>()?
                };
                if current.session_title.is_none() && !mailbox_wake {
                    let prompt_text = crate::transcript::materialized_content_text(&content);
                    if let Some(title) = current
                        .resolved_title()
                        .or_else(|| provisional_session_title(&prompt_text))
                    {
                        mutation.session_title = Some(Some(title));
                    }
                }
                let mut queue = current.queued_prompts.clone();
                queue.retain(|queued| queued.command_id != *command_id);
                queue.push(MaterializedQueuedPrompt {
                    command_id: command_id.clone(),
                    kind: QueuedCommandKind::Prompt,
                    content,
                    queued_at_ms: *created_at_ms,
                    accepted_ordinal: Some(event.ordinal),
                });
                mutation.queued_prompts = Some(queue);
            }
            // A configuration change waits in the same queue as prompts and is
            // displayed as the composer text that produced it.
            RelayCommand::SetConfig { key, value } => {
                let mut queue = current.queued_prompts.clone();
                queue.retain(|queued| queued.command_id != *command_id);
                queue.push(MaterializedQueuedPrompt {
                    command_id: command_id.clone(),
                    kind: QueuedCommandKind::SetConfig {
                        key: key.clone(),
                        value: value.clone(),
                    },
                    content: vec![serde_json::to_value(ContentBlock::Text(TextContent::new(
                        config_command_text(key, value),
                    )))?],
                    queued_at_ms: *created_at_ms,
                    accepted_ordinal: Some(event.ordinal),
                });
                mutation.queued_prompts = Some(queue);
            }
            RelayCommand::RunUserShell { command } => upsert(
                mutation,
                TranscriptItem {
                    stable_id: user_shell_item_id(command_id),
                    position: event.ordinal,
                    latest_content_event_ordinal: None,
                    created_at_ms: *created_at_ms,
                    last_changed_at_ms: *created_at_ms,
                    body: TranscriptBody::System {
                        text: user_shell_text(command, "queued", "", "", false, false),
                    },
                },
            ),
            RelayCommand::DeliverMailboxEvent {
                event: mailbox_event,
            } => push_system(
                mutation,
                event,
                format!("Mailbox event queued from {}", mailbox_event.source),
            ),
            RelayCommand::RemoveQueuedPrompt { .. } | RelayCommand::ClearQueuedPrompts => {}
            RelayCommand::Close { .. } => {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Closing);
            }
            _ => {}
        },
        RelayObservation::CommandStarted {
            command_id,
            started_at_ms,
        } => {
            if let Some(queue_index) = current
                .queued_prompts
                .iter()
                .position(|queued| queued.command_id == *command_id)
            {
                let mut queue = current.queued_prompts.clone();
                let entry = queue.remove(queue_index);
                let entry_accepted_ordinal = entry.accepted_ordinal;
                mutation.queued_prompts = Some(queue);
                // A configuration change applies between turns: it never
                // becomes a transcript turn and never starts the turn clock.
                if entry.kind.is_prompt() {
                    close_streams(index, mutation, event.recorded_at_ms);
                    upsert(
                        mutation,
                        TranscriptItem {
                            stable_id: format!("user:{command_id}"),
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: *started_at_ms,
                            last_changed_at_ms: *started_at_ms,
                            body: TranscriptBody::User {
                                content: entry.content,
                            },
                        },
                    );
                    mutation.execution = Some(MaterializedExecutionState::Running {
                        started_at_ms: *started_at_ms,
                    });
                    mutation.active_turn = Some(Some(MaterializedTurn {
                        command_id: command_id.clone(),
                        accepted_ordinal: entry_accepted_ordinal,
                        turn_start_position: event.ordinal,
                        started_at_ms: *started_at_ms,
                        steered_into: None,
                    }));
                }
            }
            if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                let mut item = TranscriptItem::clone(existing);
                if let TranscriptBody::System { text } = &mut item.body {
                    *text = text.replacen("Shell · queued", "Shell · running", 1);
                }
                item.last_changed_at_ms = item.last_changed_at_ms.max(*started_at_ms);
                upsert(mutation, item);
            }
        }
        RelayObservation::CommandCompleted {
            command_id,
            outcome,
            command,
            ..
        } => {
            let mut queue = current.queued_prompts.clone();
            queue.retain(|queued| queued.command_id != *command_id);
            match outcome {
                mj_core::relay::RelayCommandOutcome::ContextCleared { .. } => {
                    mutation.clear_turn_outcome = true;
                    close_streams(index, mutation, event.recorded_at_ms);
                    mutation.execution = Some(MaterializedExecutionState::Idle);
                    mutation.active_turn = Some(None);
                    mutation.pending_elicitations = Some(Vec::new());
                    let mut configuration = current.configuration.clone();
                    let goal = configuration.goal.take().unwrap_or_default();
                    configuration.goal = Some(Box::new(mj_core::goal::GoalState {
                        capability: goal.capability,
                        known: true,
                        ..Default::default()
                    }));
                    mutation.configuration = Some(configuration);
                    upsert(
                        mutation,
                        TranscriptItem {
                            stable_id: format!(
                                "{}{command_id}",
                                mj_core::archive::CONTEXT_BOUNDARY_PREFIX
                            ),
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: event.recorded_at_ms,
                            last_changed_at_ms: event.recorded_at_ms,
                            body: TranscriptBody::System {
                                text: "Context cleared — a new conversation starts here.".into(),
                            },
                        },
                    );
                }
                mj_core::relay::RelayCommandOutcome::Prompt {
                    stop_reason,
                    usage,
                    diagnostic,
                } => {
                    let native_running =
                        current.configuration.goal.as_deref().is_some_and(mj_core::goal::GoalState::running);
                    if !native_running {
                        close_streams(index, mutation, event.recorded_at_ms);
                        mutation.execution = Some(MaterializedExecutionState::Idle);
                    }
                    // A cancelled turn's reply stops mid-sentence; mark where
                    // it ended so it does not read as a finished reply.
                    if mj_core::state::classify_prompt_completion(stop_reason)
                        == mj_core::state::PromptCompletion::Cancelled
                    {
                        push_system_with_id(
                            mutation,
                            event,
                            format!(
                                "{}{command_id}",
                                mj_core::transcript::TURN_INTERRUPTED_ITEM_PREFIX
                            ),
                            mj_core::transcript::TURN_INTERRUPTED_TEXT,
                        );
                    }
                    let active = current
                        .active_turn
                        .as_ref()
                        .filter(|turn| turn.belongs_to(command_id));
                    mutation.last_turn_outcome = Some(MaterializedTurnOutcome {
                        diagnostic: diagnostic.clone(),
                        usage: usage.clone(),
                        command_id: command_id.clone(),
                        accepted_ordinal: active.and_then(|turn| turn.accepted_ordinal),
                        turn_start_position: active.map(|turn| turn.turn_start_position),
                        completed_ordinal: event.ordinal,
                        completed_at_ms: event.recorded_at_ms,
                        outcome: TurnOutcomeKind::Completed {
                            stop_reason: stop_reason.clone(),
                        },
                    });
                    mutation.active_turn = Some(None);
                }
                mj_core::relay::RelayCommandOutcome::UserShell { result } => {
                    if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                        let mut item = TranscriptItem::clone(existing);
                        item.body = TranscriptBody::System {
                            text: user_shell_result_text(result),
                        };
                        item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                        upsert(mutation, item);
                    }
                }
                mj_core::relay::RelayCommandOutcome::Closed => {
                    close_streams(index, mutation, event.recorded_at_ms);
                    mutation.execution = Some(MaterializedExecutionState::Closed);
                }
                mj_core::relay::RelayCommandOutcome::QueueChanged {
                    removed_command_ids,
                } => queue.retain(|queued| {
                    !removed_command_ids
                        .iter()
                        .any(|command_id| command_id == &queued.command_id)
                }),
                mj_core::relay::RelayCommandOutcome::Steered { queued_command_id } => {
                    let Some(queue_index) = queue
                        .iter()
                        .position(|queued| queued.command_id == *queued_command_id)
                    else {
                        bail!("steered prompt is missing from the materialized queue");
                    };
                    let entry = queue.remove(queue_index);
                    if !entry.kind.is_prompt() {
                        bail!("steered queue entry is not a prompt");
                    }
                    // The running turn becomes the steered prompt's turn: the
                    // harness keeps the same command in flight but the work it
                    // now reports belongs to the queued prompt.
                    mutation.active_turn = Some(Some(MaterializedTurn {
                        command_id: queued_command_id.clone(),
                        accepted_ordinal: entry.accepted_ordinal,
                        turn_start_position: event.ordinal,
                        started_at_ms: event.recorded_at_ms,
                        // The relay keeps the original prompt in flight, and
                        // its completion is what ends this turn.
                        steered_into: current.active_turn.as_ref().map(|turn| {
                            turn.steered_into
                                .clone()
                                .unwrap_or_else(|| turn.command_id.clone())
                        }),
                    }));
                    close_streams(index, mutation, event.recorded_at_ms);
                    upsert(
                        mutation,
                        TranscriptItem {
                            stable_id: format!("user:{queued_command_id}"),
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: event.recorded_at_ms,
                            last_changed_at_ms: event.recorded_at_ms,
                            body: TranscriptBody::User {
                                content: entry.content,
                            },
                        },
                    );
                }
                mj_core::relay::RelayCommandOutcome::Configured => {
                    mutation.config_results.push((command_id.clone(), None));
                }
                mj_core::relay::RelayCommandOutcome::Cancelled => {
                    if matches!(
                        command,
                        Some(
                            mj_core::relay::RelayCommandKind::CancelTurn
                                | mj_core::relay::RelayCommandKind::Cancel
                        )
                    ) {
                        end_cancelled_harness_turn(current, event, command_id, mutation);
                    }
                }
                mj_core::relay::RelayCommandOutcome::GoalControlled
                | mj_core::relay::RelayCommandOutcome::SessionModeSet
                | mj_core::relay::RelayCommandOutcome::CheckpointCompleted
                | mj_core::relay::RelayCommandOutcome::CheckpointReleased
                | mj_core::relay::RelayCommandOutcome::RecoveryFloorAdvanced
                | mj_core::relay::RelayCommandOutcome::NoticeRecorded
                | mj_core::relay::RelayCommandOutcome::UserShellCancelled
                // The returned prompt keeps its queue entry.
                | mj_core::relay::RelayCommandOutcome::SteeringReturned { .. } => {}
            }
            if queue != current.queued_prompts {
                mutation.queued_prompts = Some(queue);
            }
        }
        RelayObservation::CommandRejected {
            command_id,
            command,
            message,
            reason,
        }
        | RelayObservation::CommandInterrupted {
            command_id,
            command,
            message,
            reason,
        } => {
            if *command == RelayCommandKind::ClearContext {
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
            if matches!(
                command,
                RelayCommandKind::SetConfig | RelayCommandKind::RestoreExecutionMode
            ) {
                mutation
                    .config_results
                    .push((command_id.clone(), Some(message.clone())));
            }
            let prompt_was_started = index.get(&format!("user:{command_id}")).is_some();
            let queued_entry = current
                .queued_prompts
                .iter()
                .find(|queued| queued.command_id == *command_id)
                .cloned();
            let mut queue = current.queued_prompts.clone();
            queue.retain(|queued| queued.command_id != *command_id);
            if queue != current.queued_prompts {
                mutation.queued_prompts = Some(queue);
            }
            if prompt_was_started
                && !current
                    .configuration
                    .goal
                    .as_deref()
                    .is_some_and(mj_core::goal::GoalState::running)
            {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
            if *command == RelayCommandKind::Prompt {
                // A prompt that never started has its acceptance ordinal on the
                // queue entry; one that started carries it on the active turn.
                let active = current
                    .active_turn
                    .as_ref()
                    .filter(|turn| turn.belongs_to(command_id));
                let outcome_text = message.clone();
                mutation.last_turn_outcome = Some(MaterializedTurnOutcome {
                    diagnostic: None,
                    usage: None,
                    command_id: command_id.clone(),
                    accepted_ordinal: active.and_then(|turn| turn.accepted_ordinal).or_else(|| {
                        queued_entry
                            .as_ref()
                            .and_then(|entry| entry.accepted_ordinal)
                    }),
                    turn_start_position: active.map(|turn| turn.turn_start_position),
                    completed_ordinal: event.ordinal,
                    completed_at_ms: event.recorded_at_ms,
                    outcome: if matches!(
                        event.observation,
                        RelayObservation::CommandRejected { .. }
                    ) {
                        TurnOutcomeKind::Rejected {
                            reason: *reason,
                            message: outcome_text,
                        }
                    } else {
                        TurnOutcomeKind::Interrupted {
                            reason: *reason,
                            message: outcome_text,
                        }
                    },
                });
                if active.is_some() {
                    mutation.active_turn = Some(None);
                }
            }
            if *command == RelayCommandKind::Close
                && current.execution == MaterializedExecutionState::Closing
            {
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
            if matches!(command, RelayCommandKind::RunUserShell) {
                if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                    let mut item = TranscriptItem::clone(existing);
                    if let TranscriptBody::System { text } = &mut item.body {
                        *text = format!(
                            "{}\nerror: {message}",
                            text.replacen("Shell · queued", "Shell · interrupted", 1)
                                .replacen("Shell · running", "Shell · interrupted", 1)
                        );
                    }
                    item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                    upsert(mutation, item);
                }
            } else if prompt_was_started
                && matches!(
                    event.observation,
                    RelayObservation::CommandInterrupted { .. }
                )
            {
                push_system_with_id(
                    mutation,
                    event,
                    format!(
                        "{}{}",
                        mj_core::transcript::WORK_INTERRUPTED_ITEM_PREFIX,
                        event.ordinal
                    ),
                    format!("Work interrupted: {message}"),
                );
            } else if !controller_coordination(*command)
                || reason.is_some_and(|reason| !reason.expected_cancellation())
            {
                // The command id is internal. The relay event keeps it for
                // diagnosis, and the log line below ties it to the notice.
                tracing::info!(%command_id, ?command, %message, "relay command did not complete");
                push_system(mutation, event, message.clone());
            }
            // Expected coordination cleanup stays in the journal and API.
            // Typed control failures remain visible to the person.
        }
        RelayObservation::ConfigurationUpdated { key, value } => {
            let mut configuration = current.configuration.clone();
            configuration
                .values
                .insert(key.clone(), Value::String(value.clone()));
            mutation.configuration = Some(configuration);
        }
        RelayObservation::CheckpointReady { .. } => {}
        RelayObservation::UserShellOutput {
            command_id,
            command,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } => {
            if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                let mut item = TranscriptItem::clone(existing);
                item.body = TranscriptBody::System {
                    text: user_shell_text(
                        command,
                        "running",
                        stdout,
                        stderr,
                        *stdout_truncated,
                        *stderr_truncated,
                    ),
                };
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
            }
        }
        // Terminal output can land before or after the tool call that names the
        // terminal, so both orderings have to end in the same place: attached to
        // every referencing tool item, or parked in a standalone item that the
        // tool call consumes when it arrives.
        RelayObservation::TerminalOutput {
            terminal_id,
            output,
            truncated,
            exit_code,
            signal,
        } => {
            let record = TerminalOutputRecord {
                terminal_id: terminal_id.clone(),
                output: output.clone(),
                truncated: *truncated,
                exit_code: *exit_code,
                signal: signal.clone(),
            };
            let raw_owner = uniquely_matching_raw_tool(index, &record);
            let referrers = index
                .terminal_referrers(terminal_id)
                .cloned()
                .collect::<Vec<_>>();
            let mut attached = false;
            for existing in &referrers {
                if raw_owner.as_ref().is_some_and(|owner| {
                    owner.stable_id != existing.stable_id
                        && fallback_tool_item(existing).unwrap_or(false)
                }) {
                    mutation.transcript.push(TranscriptMutation::Remove {
                        stable_id: existing.stable_id.clone(),
                    });
                    attached = true;
                    continue;
                }
                let mut item = TranscriptItem::clone(existing);
                let TranscriptBody::Tool {
                    terminal_outputs,
                    terminal_refs,
                    ..
                } = &mut item.body
                else {
                    unreachable!("matched a tool body above");
                };
                replace_or_push_terminal_record(terminal_outputs, record.clone());
                if !terminal_refs.contains(terminal_id) {
                    terminal_refs.push(terminal_id.clone());
                }
                finalize_fallback_terminal_tool(&mut item)?;
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
                attached = true;
            }
            if let Some(existing) = raw_owner
                && !referrers
                    .iter()
                    .any(|referrer| referrer.stable_id == existing.stable_id)
            {
                let mut item = TranscriptItem::clone(&existing);
                let TranscriptBody::Tool {
                    terminal_outputs,
                    terminal_refs,
                    ..
                } = &mut item.body
                else {
                    unreachable!("matched a tool body above");
                };
                replace_or_push_terminal_record(terminal_outputs, record.clone());
                terminal_refs.push(terminal_id.clone());
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
                attached = true;
            }
            if !attached {
                let stable_id = terminal_item_id(terminal_id);
                match index.get(&stable_id) {
                    Some(existing) => {
                        let mut item = TranscriptItem::clone(existing);
                        item.body = TranscriptBody::TerminalOutput { record };
                        item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                        upsert(mutation, item);
                    }
                    None => upsert(
                        mutation,
                        TranscriptItem {
                            stable_id,
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: event.recorded_at_ms,
                            last_changed_at_ms: event.recorded_at_ms,
                            body: TranscriptBody::TerminalOutput { record },
                        },
                    ),
                }
            }
        }
        RelayObservation::Warning { message } | RelayObservation::SessionFault { message, .. } => {
            push_system(mutation, event, format!("warning: {message}"));
        }
        RelayObservation::SessionRestarted => {
            let mut configuration = current.configuration.clone();
            if let Some(goal) = configuration.goal.as_mut() {
                goal.restart();
                mutation.configuration = Some(configuration);
            }
            push_system_with_id(
                mutation,
                event,
                format!(
                    "{}{}",
                    crate::transcript::SESSION_RESTART_ITEM_PREFIX,
                    event.ordinal
                ),
                crate::transcript::SESSION_RESTART_TEXT,
            );
            // A restart during a turn the harness started on its own leaves
            // nothing that can finish it. Without this the session stays
            // Running with open streams, which canonical export refuses.
            if matches!(
                current.execution,
                MaterializedExecutionState::Running { .. }
            ) {
                push_system_with_id(
                    mutation,
                    event,
                    format!(
                        "{}{}",
                        mj_core::transcript::WORK_INTERRUPTED_ITEM_PREFIX,
                        event.ordinal
                    ),
                    "Work interrupted by session restart",
                );
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
        }
        RelayObservation::HarnessTurnStarted { .. } if current.active_turn.is_some() => {
            // Codex reports native execution starts for ordinary replies too.
            // The user turn already supplies the transcript boundary and clock.
        }
        RelayObservation::HarnessTurnStarted { started_at_ms } => {
            upsert(
                mutation,
                TranscriptItem {
                    stable_id: format!(
                        "{}{}",
                        crate::transcript::HARNESS_TURN_ITEM_PREFIX,
                        event.ordinal
                    ),
                    position: event.ordinal,
                    latest_content_event_ordinal: None,
                    created_at_ms: event.recorded_at_ms,
                    last_changed_at_ms: event.recorded_at_ms,
                    body: TranscriptBody::System {
                        text: crate::transcript::HARNESS_TURN_TEXT.to_owned(),
                    },
                },
            );
            mutation.execution = Some(MaterializedExecutionState::Running {
                started_at_ms: *started_at_ms,
            });
        }
        RelayObservation::HarnessTurnSettled {
            prompt_in_flight, ..
        } => {
            // A prompt dispatched mid-turn is still running when the turn the
            // harness started on its own settles. The relay keeps the session
            // Running for it, and so does this: the prompt's own result closes
            // the streams and stops the clock.
            if !prompt_in_flight {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
        }
        // Keyed on the command rather than the event ordinal, and skipped once
        // the line exists: a relay that re-records the same notice after a
        // persistence retry leaves exactly one line in the conversation.
        RelayObservation::Notice { message } => {
            let stable_id = match &event.command_id {
                Some(command_id) => format!("system:notice:{command_id}"),
                None => format!("system:{}", event.ordinal),
            };
            if index.get(&stable_id).is_none() {
                push_system_with_id(mutation, event, stable_id, message.clone());
            }
        }
        RelayObservation::MailboxEventsDelivered {
            event_keys,
            path,
            hook_event,
            ..
        } => {
            let count = mailbox_event_count(event_keys.len());
            let how = match path {
                mj_core::mailbox::MailboxDeliveryPath::ToolHook => hook_event.as_ref().map_or_else(
                    || "by tool hook".to_owned(),
                    |event| format!("by tool hook ({event})"),
                ),
                mj_core::mailbox::MailboxDeliveryPath::Prompt => "with prompt".to_owned(),
                mj_core::mailbox::MailboxDeliveryPath::Wake => "in a wake prompt".to_owned(),
            };
            push_system(mutation, event, format!("Delivered {count} {how}"));
        }
        RelayObservation::Closing => {
            close_streams(index, mutation, event.recorded_at_ms);
            mutation.execution = Some(MaterializedExecutionState::Closing);
        }
        RelayObservation::Closed => {
            close_streams(index, mutation, event.recorded_at_ms);
            mutation.execution = Some(MaterializedExecutionState::Closed);
        }
        RelayObservation::RetryAssessmentStarted { .. }
        | RelayObservation::TurnAssessmentUpdated { .. }
        | RelayObservation::RetryAssessmentResolved { .. } => {}
    }
    Ok(())
}

fn mailbox_event_count(count: usize) -> String {
    format!("{count} mailbox event{}", if count == 1 { "" } else { "s" })
}

/// A stop applied while the harness runs a turn of its own (a goal
/// continuation, or work a background command woke). No prompt of ours is
/// running, so no prompt completion will carry the cancellation: give the turn
/// the same "Interrupted" row and cancelled outcome an ordinary turn gets
/// (I2-8). A stop that lands on a prompt of ours, or on nothing, adds neither.
fn end_cancelled_harness_turn(
    current: &MaterializedSession,
    event: &RelayEvent,
    cancel_command_id: &str,
    mutation: &mut MaterializedSessionMutation,
) {
    if current.active_turn.is_some()
        || !matches!(
            current.execution,
            MaterializedExecutionState::Running { .. }
        )
    {
        return;
    }
    let Some(start_position) = current
        .transcript
        .iter()
        .rev()
        .find(|item| {
            item.stable_id
                .starts_with(mj_core::transcript::HARNESS_TURN_ITEM_PREFIX)
        })
        .map(|item| item.position)
    else {
        return;
    };
    let turn_id = mj_core::continuation::harness_turn_id(start_position);
    // A second stop of the same turn changes nothing.
    if current
        .last_turn_outcome
        .as_ref()
        .is_some_and(|turn| turn.command_id == turn_id)
    {
        return;
    }
    push_system_with_id(
        mutation,
        event,
        format!(
            "{}{cancel_command_id}",
            mj_core::transcript::TURN_INTERRUPTED_ITEM_PREFIX
        ),
        mj_core::transcript::TURN_INTERRUPTED_TEXT,
    );
    mutation.last_turn_outcome = Some(MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: turn_id,
        accepted_ordinal: None,
        turn_start_position: Some(start_position),
        completed_ordinal: event.ordinal,
        completed_at_ms: event.recorded_at_ms,
        outcome: TurnOutcomeKind::Completed {
            stop_reason: "Cancelled".into(),
        },
    });
}

pub(super) fn user_shell_item_id(command_id: &str) -> String {
    format!("shell:{command_id}")
}

/// Stable id of the captured plan proposal created by the relay event at
/// `ordinal`. The ordinal keys it because the harness-side review id restarts
/// with every harness process, while the ordinal is durable and replay-stable.
pub fn plan_proposal_item_id(ordinal: u64) -> String {
    format!("plan-proposal:{ordinal}")
}

pub(super) fn user_shell_text(
    command: &str,
    status: &str,
    stdout: &str,
    stderr: &str,
    stdout_truncated: bool,
    stderr_truncated: bool,
) -> String {
    let mut text = format!("Shell · {status}\n$ {command}");
    if !stdout.is_empty() {
        text.push_str("\n\nstdout:\n");
        text.push_str(stdout);
        if stdout_truncated {
            text.push_str("\n[output continues; final tail will be shown on completion]");
        }
    }
    if !stderr.is_empty() {
        text.push_str("\n\nstderr:\n");
        text.push_str(stderr);
        if stderr_truncated {
            text.push_str("\n[output continues; final tail will be shown on completion]");
        }
    }
    text
}

pub(super) fn user_shell_result_text(result: &mj_core::relay::UserShellResult) -> String {
    let status = match result.status {
        mj_core::relay::UserShellStatus::Exited => match result.exit_code {
            Some(0) => "done".to_owned(),
            Some(code) => format!("failed (exit {code})"),
            None => "finished".to_owned(),
        },
        mj_core::relay::UserShellStatus::Signaled => format!(
            "signaled ({})",
            result.signal.as_deref().unwrap_or("unknown signal")
        ),
        mj_core::relay::UserShellStatus::TimedOut => "timed out".to_owned(),
        mj_core::relay::UserShellStatus::Cancelled => "cancelled".to_owned(),
        mj_core::relay::UserShellStatus::Interrupted => "interrupted".to_owned(),
        mj_core::relay::UserShellStatus::Failed => "failed".to_owned(),
    };
    let mut text = user_shell_text(
        &result.command,
        &format!("{status} · {} ms", result.duration_ms),
        &result.stdout,
        &result.stderr,
        result.stdout_truncated,
        result.stderr_truncated,
    );
    if let Some(error) = &result.error {
        text.push_str("\n\nerror: ");
        text.push_str(error);
    }
    text
}

/// Commands the daemon issues to the worker for its own bookkeeping, never
/// on the person's behalf.
fn controller_coordination(command: RelayCommandKind) -> bool {
    matches!(
        command,
        RelayCommandKind::BeginCheckpoint
            | RelayCommandKind::CompleteCheckpoint
            | RelayCommandKind::ReleaseCheckpoint
            | RelayCommandKind::AdvanceRecoveryFloor
    )
}
