//! Folding one relay event into the next snapshot.

use anyhow::{Result, anyhow, bail};

use super::*;

/// Whether applying this observation moves durable relay state beyond the
/// event frontier.
///
/// Transcript observations do not: replaying them from the journal reaches the
/// same snapshot, so appending one need not stage a snapshot copy, re-check the
/// snapshot budgets, or rewrite `relay-state.json`. Every arm here mirrors an
/// arm of [`apply_relay_event`]; `transcript_observations_move_nothing_but_the_frontier`
/// fails if the two ever disagree.
pub fn observation_changes_state(observation: &RelayObservation) -> bool {
    match observation {
        RelayObservation::AgentInitialized { .. }
        | RelayObservation::SessionOpened { .. }
        | RelayObservation::SessionConfigured { .. }
        | RelayObservation::SessionModesConfigured { .. }
        | RelayObservation::CommandQueued { .. }
        | RelayObservation::CommandStarted { .. }
        | RelayObservation::CommandCompleted { .. }
        | RelayObservation::CommandRejected { .. }
        | RelayObservation::CommandInterrupted { .. }
        | RelayObservation::ConfigurationUpdated { .. }
        | RelayObservation::CheckpointReady { .. }
        // A restart ends any turn the harness started on its own, so it now
        // moves durable state instead of only the frontier.
        | RelayObservation::SessionRestarted
        | RelayObservation::HarnessTurnStarted { .. }
        | RelayObservation::HarnessTurnSettled { .. }
        | RelayObservation::Closing
        | RelayObservation::Closed => true,
        RelayObservation::SessionUpdate { update } => matches!(
            update.as_ref(),
            SessionUpdate::AvailableCommandsUpdate(_)
                | SessionUpdate::ConfigOptionUpdate(_)
                | SessionUpdate::CurrentModeUpdate(_)
                | SessionUpdate::SessionInfoUpdate(_)
        ),
        RelayObservation::PermissionAutoApproved { .. }
        | RelayObservation::ElicitationRequested { .. }
        | RelayObservation::ElicitationResolved { .. }
        | RelayObservation::ElicitationsCleared
        | RelayObservation::Warning { .. }
        | RelayObservation::UserShellOutput { .. }
        | RelayObservation::TerminalOutput { .. }
        | RelayObservation::Notice { .. } => false,
    }
}

pub fn apply_relay_event(snapshot: &mut RelaySnapshot, event: &RelayEvent) -> Result<()> {
    validate_relay_event(snapshot.latest_ordinal, &snapshot.latest_digest, event)?;
    match &event.observation {
        RelayObservation::AgentInitialized {
            capabilities,
            agent_info,
            ..
        } => {
            snapshot.agent_capabilities = Some(capabilities.clone());
            snapshot.agent_info = agent_info.clone();
        }
        RelayObservation::SessionOpened {
            native_session_id,
            native_continuity_lost,
            resumed,
        } => {
            snapshot.native_session_id = Some(native_session_id.clone());
            snapshot.native_session_opened_ordinal = Some(event.ordinal);
            // A normal open clears the flag; only the fallback sets it.
            snapshot.native_continuity_lost = *native_continuity_lost;
            // A resumed thread was not created here, so this journal cannot
            // show everything the thread contains. Once true this never
            // clears: replacing such a thread would discard native history.
            if *resumed {
                snapshot.native_session_used = true;
            }
        }
        RelayObservation::SessionConfigured { config_options } => {
            snapshot.config_options = config_options.clone();
        }
        RelayObservation::SessionModesConfigured { modes } => {
            snapshot.modes = modes.clone();
        }
        RelayObservation::CommandQueued {
            command_id,
            command,
            created_at_ms,
        } => {
            if cancels_capacity_retry(command) {
                if let Some(retry) = snapshot.capacity_retry.as_mut()
                    && retry.command_id == *command_id
                    && matches!(command, RelayCommand::Prompt { .. })
                {
                    retry.submitted = true;
                } else {
                    snapshot.capacity_retry = None;
                }
            }
            snapshot.handled_commands.insert(
                command_id.clone(),
                HandledRelayCommand {
                    command: command.clone(),
                    accepted_ordinal: event.ordinal,
                    terminal_ordinal: None,
                },
            );
            snapshot.dispatches.insert(
                command_id.clone(),
                RelayDispatchRecord {
                    command: command.clone(),
                    state: RelayDispatchState::Queued,
                },
            );
            // Prompts and configuration changes share one FIFO queue so they
            // reach the agent in the order the user submitted them.
            let payload = match command {
                RelayCommand::Prompt { prompt } => Some(StoredQueuedRelayPayload::Prompt {
                    prompt: prompt.clone(),
                }),
                RelayCommand::SetConfig { key, value } => {
                    Some(StoredQueuedRelayPayload::SetConfig {
                        key: key.clone(),
                        value: value.clone(),
                    })
                }
                _ => None,
            };
            if let Some(payload) = payload {
                snapshot.queued_prompts.push(StoredQueuedRelayCommand {
                    command_id: command_id.clone(),
                    payload,
                    created_at_ms: *created_at_ms,
                });
            }
            if let RelayCommand::RunUserShell { command } = command {
                snapshot.active_user_shells.insert(
                    command_id.clone(),
                    ActiveUserShell {
                        command_id: command_id.clone(),
                        command: command.clone(),
                        created_at_ms: *created_at_ms,
                        started_at_ms: None,
                    },
                );
            }
            if matches!(command, RelayCommand::Close { .. }) {
                snapshot.execution = RelayExecutionState::Closing;
            }
        }
        RelayObservation::CommandStarted {
            command_id,
            started_at_ms,
        } => {
            let dispatch = snapshot
                .dispatches
                .get_mut(command_id)
                .ok_or_else(|| anyhow!("started unknown relay command {command_id}"))?;
            dispatch.state = RelayDispatchState::Pending;
            match &dispatch.command {
                RelayCommand::Prompt { .. } => {
                    let index = snapshot
                        .queued_prompts
                        .iter()
                        .position(|queued| queued.command_id == *command_id)
                        .ok_or_else(|| anyhow!("started prompt {command_id} was not queued"))?;
                    let queued = snapshot.queued_prompts.remove(index);
                    let StoredQueuedRelayPayload::Prompt { prompt } = queued.payload else {
                        bail!("queued command {command_id} is not a prompt");
                    };
                    snapshot.execution = RelayExecutionState::Running;
                    snapshot.activity_turn_started_at_ms = Some(*started_at_ms);
                    snapshot.active_prompt = Some(StoredActiveRelayPrompt {
                        command_id: queued.command_id,
                        prompt,
                        created_at_ms: queued.created_at_ms,
                        started_at_ms: *started_at_ms,
                    });
                }
                // A configuration change leaves the queue when it starts, but
                // the ACP session stays idle: it applies between turns.
                RelayCommand::SetConfig { .. } => {
                    let index = snapshot
                        .queued_prompts
                        .iter()
                        .position(|queued| queued.command_id == *command_id)
                        .ok_or_else(|| {
                            anyhow!("started configuration change {command_id} was not queued")
                        })?;
                    snapshot.queued_prompts.remove(index);
                }
                RelayCommand::Close { .. } => snapshot.execution = RelayExecutionState::Closing,
                RelayCommand::RunUserShell { .. } => {
                    let shell = snapshot
                        .active_user_shells
                        .get_mut(command_id)
                        .ok_or_else(|| anyhow!("started unknown user shell {command_id}"))?;
                    shell.started_at_ms = Some(*started_at_ms);
                }
                RelayCommand::BeginCheckpoint { .. } => {
                    if snapshot.checkpoint_barrier.is_some() {
                        bail!("checkpoint barrier started while another barrier was active");
                    }
                    snapshot.checkpoint_barrier = Some(command_id.clone());
                    snapshot.checkpoint_ready_through = None;
                    snapshot.checkpoint_ready_digest = None;
                }
                _ => {}
            }
        }
        RelayObservation::CommandCompleted {
            command_id,
            outcome,
        } => {
            let command = snapshot
                .dispatches
                .get(command_id)
                .ok_or_else(|| anyhow!("completed unknown relay command {command_id}"))?
                .command
                .clone();
            snapshot
                .dispatches
                .get_mut(command_id)
                .expect("dispatch disappeared")
                .state = RelayDispatchState::Completed;
            snapshot
                .handled_commands
                .get_mut(command_id)
                .ok_or_else(|| anyhow!("completed command {command_id} is not in the ledger"))?
                .terminal_ordinal = Some(event.ordinal);
            if let RelayCommandOutcome::Prompt { stop_reason, .. } = outcome {
                let accepted = snapshot.handled_commands[command_id].accepted_ordinal;
                let superseded = snapshot.handled_commands.values().any(|handled| {
                    handled.accepted_ordinal > accepted && cancels_capacity_retry(&handled.command)
                });
                let attempt = snapshot
                    .capacity_retry
                    .as_ref()
                    .filter(|retry| retry.command_id == *command_id)
                    .map_or(1, |retry| retry.attempt.saturating_add(1));
                snapshot.capacity_retry = if stop_reason == CAPACITY_STOP_REASON && !superseded {
                    Some(CapacityRetry::new(
                        attempt,
                        event.ordinal,
                        event.recorded_at_ms,
                    ))
                } else {
                    None
                };
            }
            match (command, outcome) {
                (RelayCommand::Prompt { .. }, RelayCommandOutcome::Prompt { .. }) => {
                    if snapshot
                        .active_prompt
                        .as_ref()
                        .map(|active| &active.command_id)
                        == Some(command_id)
                    {
                        snapshot.active_prompt = None;
                    }
                    // ACP completion must not settle a later native goal turn.
                    if !snapshot.goal.running() {
                        snapshot.harness_turn = None;
                    }
                    if snapshot.execution == RelayExecutionState::Running
                        && !snapshot.goal.running()
                    {
                        snapshot.execution = RelayExecutionState::Idle;
                    }
                    if snapshot
                        .pending_prompt_context
                        .as_ref()
                        .and_then(|context| context.attached_command_id.as_deref())
                        == Some(command_id.as_str())
                    {
                        snapshot.pending_prompt_context = None;
                    }
                    snapshot.pending_user_shell_contexts.retain(|context| {
                        context.attached_command_id.as_deref() != Some(command_id.as_str())
                    });
                }
                (RelayCommand::RunUserShell { .. }, RelayCommandOutcome::UserShell { result }) => {
                    snapshot.active_user_shells.remove(command_id);
                    let accepted_ordinal = snapshot
                        .handled_commands
                        .get(command_id)
                        .ok_or_else(|| anyhow!("completed user shell is not in the ledger"))?
                        .accepted_ordinal;
                    snapshot
                        .pending_user_shell_contexts
                        .push(PendingUserShellContext {
                            shell_command_id: command_id.clone(),
                            accepted_ordinal,
                            text: result.prompt_context(),
                            attached_command_id: None,
                        });
                }
                (RelayCommand::CancelUserShell { .. }, RelayCommandOutcome::UserShellCancelled) => {
                }
                (
                    RelayCommand::RemoveQueuedPrompt { queued_command_id },
                    RelayCommandOutcome::QueueChanged {
                        removed_command_ids,
                    },
                ) => {
                    let expected = snapshot
                        .queued_prompts
                        .iter()
                        .any(|queued| queued.command_id == queued_command_id)
                        .then_some(vec![queued_command_id]);
                    if expected.as_deref() != Some(removed_command_ids.as_slice()) {
                        bail!("removed queue outcome does not match the durable queue");
                    }
                    terminalize_removed_prompts(snapshot, removed_command_ids, event.ordinal)?;
                }
                (
                    RelayCommand::ClearQueuedPrompts,
                    RelayCommandOutcome::QueueChanged {
                        removed_command_ids,
                    },
                ) => {
                    let expected: Vec<String> = snapshot
                        .queued_prompts
                        .iter()
                        .map(|queued| queued.command_id.clone())
                        .collect();
                    if expected != *removed_command_ids {
                        bail!("cleared queue outcome does not match the durable queue");
                    }
                    terminalize_removed_prompts(snapshot, removed_command_ids, event.ordinal)?;
                }
                (RelayCommand::SetConfig { key, value }, RelayCommandOutcome::Configured) => {
                    snapshot.config.insert(key.clone(), value.clone());
                    crate::acp::AcceptedSessionConfig::record_completed(
                        &mut snapshot.config,
                        &key,
                        &value,
                        &snapshot.config_options,
                    );
                }
                (RelayCommand::SetSessionMode { mode_id }, RelayCommandOutcome::SessionModeSet) => {
                    snapshot.config.insert("mode".to_owned(), mode_id);
                }
                (RelayCommand::GoalControl { .. }, RelayCommandOutcome::GoalControlled) => {}
                (RelayCommand::Cancel, RelayCommandOutcome::Cancelled)
                | (RelayCommand::CancelTurn, RelayCommandOutcome::Cancelled) => {}
                (RelayCommand::Cancel, RelayCommandOutcome::Steered { queued_command_id }) => {
                    let queued = snapshot
                        .queued_prompts
                        .first()
                        .ok_or_else(|| anyhow!("steered prompt is no longer queued"))?;
                    if queued.command_id != *queued_command_id
                        || !matches!(queued.payload, StoredQueuedRelayPayload::Prompt { .. })
                    {
                        bail!("steered prompt is not the queued prompt head");
                    }
                    let target = snapshot
                        .dispatches
                        .get_mut(queued_command_id)
                        .ok_or_else(|| anyhow!("steered unknown queued prompt"))?;
                    if target.state != RelayDispatchState::Queued
                        || !matches!(target.command, RelayCommand::Prompt { .. })
                    {
                        bail!("steered target is not a queued prompt");
                    }
                    target.state = RelayDispatchState::Completed;
                    snapshot
                        .handled_commands
                        .get_mut(queued_command_id)
                        .ok_or_else(|| anyhow!("steered prompt is not in the ledger"))?
                        .terminal_ordinal = Some(event.ordinal);
                    snapshot.queued_prompts.remove(0);
                    if snapshot
                        .pending_prompt_context
                        .as_ref()
                        .and_then(|context| context.attached_command_id.as_deref())
                        == Some(queued_command_id.as_str())
                    {
                        snapshot.pending_prompt_context = None;
                    }
                    snapshot.pending_user_shell_contexts.retain(|context| {
                        context.attached_command_id.as_deref() != Some(queued_command_id.as_str())
                    });
                }
                (RelayCommand::Close { .. }, RelayCommandOutcome::Closed) => {
                    snapshot.execution = RelayExecutionState::Closed;
                    snapshot.active_prompt = None;
                }
                (
                    RelayCommand::CompleteCheckpoint { barrier_command_id },
                    RelayCommandOutcome::CheckpointCompleted,
                ) => {
                    if snapshot.checkpoint_barrier.as_deref() != Some(&barrier_command_id) {
                        bail!("checkpoint completion does not match the active barrier");
                    }
                    let ready_through = snapshot
                        .checkpoint_ready_through
                        .ok_or_else(|| anyhow!("checkpoint barrier was not ready"))?;
                    let ready_digest = snapshot
                        .checkpoint_ready_digest
                        .clone()
                        .ok_or_else(|| anyhow!("checkpoint barrier ready digest is missing"))?;
                    snapshot.recovery_floor_ordinal = ready_through;
                    snapshot.recovery_floor_digest = ready_digest;
                    snapshot.checkpoint_barrier = None;
                    snapshot.checkpoint_ready_through = None;
                    snapshot.checkpoint_ready_digest = None;
                    if let Some(barrier) = snapshot.dispatches.get_mut(&barrier_command_id) {
                        barrier.state = RelayDispatchState::Completed;
                    }
                    if let Some(barrier) = snapshot.handled_commands.get_mut(&barrier_command_id) {
                        barrier.terminal_ordinal = Some(event.ordinal);
                    }
                }
                (
                    RelayCommand::ReleaseCheckpoint { barrier_command_id },
                    RelayCommandOutcome::CheckpointReleased,
                ) => {
                    if snapshot.checkpoint_barrier.as_deref() != Some(&barrier_command_id) {
                        bail!("checkpoint release does not match the active barrier");
                    }
                    if snapshot.checkpoint_ready_through.is_none() {
                        bail!("checkpoint barrier was not ready");
                    }
                    // Dispatch resumes, but the recovery floor stays where the
                    // last installed archive left it: nothing yet proves this
                    // archive reached the controller's disk.
                    snapshot.checkpoint_barrier = None;
                    snapshot.checkpoint_ready_through = None;
                    snapshot.checkpoint_ready_digest = None;
                    if let Some(barrier) = snapshot.dispatches.get_mut(&barrier_command_id) {
                        barrier.state = RelayDispatchState::Completed;
                    }
                    if let Some(barrier) = snapshot.handled_commands.get_mut(&barrier_command_id) {
                        barrier.terminal_ordinal = Some(event.ordinal);
                    }
                }
                (
                    RelayCommand::AdvanceRecoveryFloor { through },
                    RelayCommandOutcome::RecoveryFloorAdvanced,
                ) => {
                    if through.ordinal < snapshot.recovery_floor_ordinal {
                        bail!("recovery floor cannot move back");
                    }
                    snapshot.recovery_floor_ordinal = through.ordinal;
                    snapshot.recovery_floor_digest = through.digest;
                }
                (RelayCommand::RecordNotice { .. }, RelayCommandOutcome::NoticeRecorded) => {}
                (RelayCommand::BeginCheckpoint { .. }, _) => {
                    bail!("checkpoint barriers complete through checkpoint-ready")
                }
                (command, outcome) => {
                    bail!(
                        "relay command {:?} has incompatible completion outcome {outcome:?}",
                        command.kind()
                    )
                }
            }
        }
        RelayObservation::CommandRejected {
            command_id,
            command: observed_command,
            message,
        }
        | RelayObservation::CommandInterrupted {
            command_id,
            command: observed_command,
            message,
        } => {
            let state = if matches!(event.observation, RelayObservation::CommandRejected { .. }) {
                RelayDispatchState::Rejected
            } else {
                RelayDispatchState::Interrupted
            };
            let command = snapshot
                .dispatches
                .get(command_id)
                .ok_or_else(|| anyhow!("terminated unknown relay command {command_id}"))?
                .command
                .clone();
            if command.kind() != *observed_command {
                bail!("terminated command {command_id} has the wrong command identity");
            }
            snapshot
                .dispatches
                .get_mut(command_id)
                .expect("dispatch disappeared")
                .state = state;
            snapshot
                .handled_commands
                .get_mut(command_id)
                .ok_or_else(|| anyhow!("terminated command {command_id} is not in the ledger"))?
                .terminal_ordinal = Some(event.ordinal);
            snapshot
                .queued_prompts
                .retain(|queued| queued.command_id != *command_id);
            snapshot.active_user_shells.remove(command_id);
            if let RelayCommand::RunUserShell { command } = &command {
                let accepted_ordinal = snapshot
                    .handled_commands
                    .get(command_id)
                    .expect("terminated shell command disappeared from the ledger")
                    .accepted_ordinal;
                let result = UserShellResult {
                    command: command.clone(),
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    exit_code: None,
                    signal: None,
                    duration_ms: 0,
                    status: if state == RelayDispatchState::Rejected {
                        UserShellStatus::Failed
                    } else {
                        UserShellStatus::Interrupted
                    },
                    error: Some(message.clone()),
                };
                snapshot
                    .pending_user_shell_contexts
                    .push(PendingUserShellContext {
                        shell_command_id: command_id.clone(),
                        accepted_ordinal,
                        text: result.prompt_context(),
                        attached_command_id: None,
                    });
            }
            if snapshot
                .active_prompt
                .as_ref()
                .map(|active| &active.command_id)
                == Some(command_id)
            {
                snapshot.active_prompt = None;
                if !snapshot.goal.running() {
                    snapshot.harness_turn = None;
                    snapshot.execution = RelayExecutionState::Idle;
                }
            }
            if snapshot
                .pending_prompt_context
                .as_ref()
                .and_then(|context| context.attached_command_id.as_deref())
                == Some(command_id.as_str())
            {
                snapshot
                    .pending_prompt_context
                    .as_mut()
                    .expect("pending prompt context disappeared")
                    .attached_command_id = None;
            }
            for context in &mut snapshot.pending_user_shell_contexts {
                if context.attached_command_id.as_deref() == Some(command_id.as_str()) {
                    context.attached_command_id = None;
                }
            }
            if matches!(command, RelayCommand::BeginCheckpoint { .. })
                && snapshot.checkpoint_barrier.as_deref() == Some(command_id)
            {
                snapshot.checkpoint_barrier = None;
                snapshot.checkpoint_ready_through = None;
                snapshot.checkpoint_ready_digest = None;
            }
            if matches!(command, RelayCommand::Close { .. })
                && snapshot.execution == RelayExecutionState::Closing
            {
                snapshot.execution = RelayExecutionState::Idle;
            }
        }
        RelayObservation::ConfigurationUpdated { key, value } => {
            snapshot.config.insert(key.clone(), value.clone());
            // Keep the accepted model/effort pair coherent however the change
            // arrived. A selector Hel applied for itself has no `SetConfig`
            // command to fold in afterwards, and a model change can retire
            // the effort the session had stored.
            crate::acp::AcceptedSessionConfig::record_completed(
                &mut snapshot.config,
                key,
                value,
                &snapshot.config_options,
            );
        }
        RelayObservation::CheckpointReady {
            command_id,
            through,
        } => {
            let Some(dispatch) = snapshot.dispatches.get(command_id) else {
                bail!("checkpoint ready for unknown command {command_id}");
            };
            if !matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. }) {
                bail!("checkpoint ready for non-barrier command {command_id}");
            }
            if snapshot.checkpoint_barrier.as_deref() != Some(command_id) {
                bail!("checkpoint ready does not match the active barrier");
            }
            if *through != event.ordinal {
                bail!("checkpoint ready frontier does not match its event ordinal");
            }
            snapshot.checkpoint_ready_through = Some(*through);
            snapshot.checkpoint_ready_digest = Some(event.digest.clone());
        }
        RelayObservation::HarnessTurnStarted { started_at_ms } => {
            snapshot.activity_turn_started_at_ms = Some(*started_at_ms);
            snapshot.harness_turn = Some(StoredHarnessTurn {
                started_at_ms: *started_at_ms,
                first_ordinal: event.ordinal,
            });
            snapshot.last_harness_turn_started_ordinal = Some(event.ordinal);
            if snapshot.execution == RelayExecutionState::Idle {
                snapshot.execution = RelayExecutionState::Running;
            }
        }
        RelayObservation::HarnessTurnSettled { .. } => {
            snapshot.harness_turn = None;
            if snapshot.active_prompt.is_none()
                && snapshot.execution == RelayExecutionState::Running
            {
                snapshot.execution = RelayExecutionState::Idle;
            }
        }
        // The control plane behind the session was replaced, so a turn the
        // harness had started on its own no longer exists. Both callers record
        // this with no prompt in flight.
        RelayObservation::SessionRestarted => {
            snapshot.goal.restart();
            if snapshot.harness_turn.take().is_some()
                && snapshot.active_prompt.is_none()
                && snapshot.execution == RelayExecutionState::Running
            {
                snapshot.execution = RelayExecutionState::Idle;
            }
        }
        RelayObservation::Closing => {
            snapshot.capacity_retry = None;
            snapshot.harness_turn = None;
            snapshot.execution = RelayExecutionState::Closing;
        }
        RelayObservation::Closed => {
            snapshot.capacity_retry = None;
            snapshot.activity_turn_started_at_ms = None;
            snapshot.harness_turn = None;
            snapshot.execution = RelayExecutionState::Closed;
            snapshot.active_prompt = None;
        }
        RelayObservation::SessionUpdate { update } => match update.as_ref() {
            SessionUpdate::SessionInfoUpdate(_) => {
                snapshot.goal.apply(update)?;
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                snapshot.available_commands = update.available_commands.clone();
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                snapshot.config_options = update.config_options.clone();
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                if let Some(modes) = snapshot.modes.as_mut() {
                    modes.current_mode_id = update.current_mode_id.clone();
                }
                snapshot
                    .config
                    .insert("mode".to_owned(), update.current_mode_id.to_string());
            }
            _ => {}
        },
        RelayObservation::PermissionAutoApproved { .. }
        | RelayObservation::ElicitationRequested { .. }
        | RelayObservation::ElicitationResolved { .. }
        | RelayObservation::ElicitationsCleared
        | RelayObservation::Warning { .. }
        | RelayObservation::UserShellOutput { .. }
        | RelayObservation::TerminalOutput { .. }
        | RelayObservation::Notice { .. } => {}
    }
    snapshot.latest_ordinal = event.ordinal;
    snapshot.latest_digest = event.digest.clone();
    Ok(())
}

/// Whether finishing this relay-local command can let journal GC drop history.
/// Only a recovery-floor move does; releasing a barrier deliberately leaves the
/// floor where an installed archive left it.
pub fn releases_history(command: &RelayCommand) -> bool {
    matches!(
        command,
        RelayCommand::CompleteCheckpoint { .. } | RelayCommand::AdvanceRecoveryFloor { .. }
    )
}

fn terminalize_removed_prompts(
    snapshot: &mut RelaySnapshot,
    removed_command_ids: &[String],
    terminal_ordinal: u64,
) -> Result<()> {
    for command_id in removed_command_ids {
        let dispatch = snapshot
            .dispatches
            .get_mut(command_id)
            .ok_or_else(|| anyhow!("removed unknown queued command {command_id}"))?;
        if !dispatch.command.is_queue_entry() || dispatch.state != RelayDispatchState::Queued {
            bail!("removed command {command_id} is not a queued command");
        }
        dispatch.state = RelayDispatchState::Rejected;
        snapshot
            .handled_commands
            .get_mut(command_id)
            .ok_or_else(|| anyhow!("removed command {command_id} is not in the ledger"))?
            .terminal_ordinal = Some(terminal_ordinal);
    }
    snapshot.queued_prompts.retain(|queued| {
        !removed_command_ids
            .iter()
            .any(|command_id| command_id == &queued.command_id)
    });
    Ok(())
}

pub fn validate_relay_snapshot_frontiers(snapshot: &RelaySnapshot) -> Result<()> {
    if snapshot.acknowledged_through > snapshot.latest_ordinal {
        bail!("relay acknowledgement is ahead of the event frontier");
    }
    if snapshot.recovery_floor_ordinal > snapshot.latest_ordinal {
        bail!("relay recovery floor is ahead of the event frontier");
    }
    validate_relay_digest(&snapshot.latest_digest, "relay latest digest")?;
    validate_relay_digest(
        &snapshot.acknowledged_digest,
        "relay acknowledgement digest",
    )?;
    validate_relay_digest(
        &snapshot.recovery_floor_digest,
        "relay recovery floor digest",
    )?;
    if (snapshot.latest_ordinal == 0) != (snapshot.latest_digest == RELAY_EVENT_GENESIS_DIGEST) {
        bail!("relay latest frontier and genesis digest disagree");
    }
    if (snapshot.acknowledged_through == 0)
        != (snapshot.acknowledged_digest == RELAY_EVENT_GENESIS_DIGEST)
    {
        bail!("relay acknowledgement frontier and genesis digest disagree");
    }
    if (snapshot.recovery_floor_ordinal == 0)
        != (snapshot.recovery_floor_digest == RELAY_EVENT_GENESIS_DIGEST)
    {
        bail!("relay recovery floor and genesis digest disagree");
    }
    Ok(())
}

/// Explicit user work and lifecycle admission supersede automated recovery.
fn cancels_capacity_retry(command: &RelayCommand) -> bool {
    matches!(
        command,
        RelayCommand::Prompt { .. }
            | RelayCommand::Cancel
            | RelayCommand::CancelTurn
            | RelayCommand::SetConfig { .. }
            | RelayCommand::GoalControl { .. }
            | RelayCommand::SetSessionMode { .. }
            | RelayCommand::Close { .. }
    )
}
