use super::*;
use mj_core::event_outcome::{CommandResult, CommandResultKind, OutcomeReason, TurnResultKind};
use mj_core::storage::ApiEventData;

/// Retain transitions before the database page coalesces its final projection.
pub(super) fn derive(
    current: &MaterializedSession,
    event: &RelayEvent,
    mutation: &MaterializedSessionMutation,
) -> Vec<ApiEventData> {
    let mut events = Vec::new();
    if let Some(Some(turn)) = &mutation.active_turn
        && current.active_turn.as_ref() != Some(turn)
    {
        events.push(ApiEventData::TurnStarted { turn: turn.clone() });
    }
    match &event.observation {
        RelayObservation::AgentInitialized {
            runtime: Some(identity),
            ..
        } => {
            events.push(ApiEventData::RuntimeResolved {
                receipt: mj_core::harness_runtime::RuntimeReceipt {
                    identity: identity.clone(),
                    event_ordinal: event.ordinal,
                    observed_at_ms: event.recorded_at_ms,
                },
            });
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
        } if *command != RelayCommandKind::Prompt => {
            if let Some(reason) = reason
                && *reason != OutcomeReason::LegacyUnclassified
            {
                let outcome = if reason.expected_cancellation() {
                    CommandResultKind::Cancelled
                } else if *reason == OutcomeReason::AdmissionRejected {
                    CommandResultKind::Rejected
                } else {
                    CommandResultKind::Failed
                };
                events.push(ApiEventData::CommandEnded {
                    result: CommandResult::worker(
                        command_id,
                        *command,
                        outcome,
                        Some(*reason),
                        Some(message.clone()),
                    ),
                });
            } else {
                events.push(ApiEventData::LegacyNotice {
                    original_type: "error".into(),
                    message: message.clone(),
                    command_id: Some(command_id.clone()),
                });
            }
        }
        RelayObservation::CommandCompleted {
            command_id,
            barrier_command_id,
            command: Some(command),
            outcome,
            ..
        } if *command != RelayCommandKind::Prompt => {
            if let Some(barrier) = barrier_command_id {
                events.push(ApiEventData::CommandEnded {
                    result: CommandResult::worker(
                        barrier,
                        RelayCommandKind::BeginCheckpoint,
                        CommandResultKind::Succeeded,
                        None,
                        None,
                    ),
                });
            }
            events.push(ApiEventData::CommandEnded {
                result: CommandResult::completed(command_id, *command, outcome),
            });
        }
        RelayObservation::SessionFault { reason, message } => {
            events.push(ApiEventData::SessionFault {
                reason: *reason,
                message: message.clone(),
                command_id: event.command_id.clone(),
            });
        }
        _ => {}
    }
    if let Some(turn) = &mutation.last_turn_outcome {
        if turn.result().kind == TurnResultKind::InputRequired {
            events.push(ApiEventData::InputRequired {
                request: None,
                turn_id: turn.accepted_ordinal,
            });
        }
        events.push(ApiEventData::TurnEnded { turn: turn.into() });
    }
    let turn_id = current
        .active_turn
        .as_ref()
        .and_then(|turn| turn.accepted_ordinal);
    if let Some(pending) = &mutation.pending_elicitations {
        for request in pending {
            if !current.pending_elicitations.contains(request) {
                events.push(ApiEventData::InputRequired {
                    request: Some(request.clone()),
                    turn_id,
                });
            }
        }
        for request in &current.pending_elicitations {
            if !pending.iter().any(|next| next.id == request.id) {
                let action = match &event.observation {
                    RelayObservation::ElicitationResolved { action, .. } => action.clone(),
                    _ => "cleared".into(),
                };
                events.push(ApiEventData::InputResolved {
                    elicitation_id: request.id.clone(),
                    turn_id,
                    action,
                });
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaiting_input_emits_input_required_before_turn_ended() {
        let current = MaterializedSession::empty("session");
        let event = RelayEvent {
            format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
            ordinal: 3,
            previous_digest: String::new(),
            digest: String::new(),
            recorded_at_ms: 1000,
            command_id: None,
            observation: RelayObservation::HarnessTurnStarted {
                started_at_ms: 1000,
            },
        };
        let mutation = MaterializedSessionMutation {
            last_turn_outcome: Some(MaterializedTurnOutcome {
                diagnostic: None,
                usage: None,
                command_id: "prompt".into(),
                accepted_ordinal: Some(1),
                turn_start_position: Some(2),
                completed_ordinal: 3,
                completed_at_ms: 1000,
                outcome: TurnOutcomeKind::Completed {
                    stop_reason: mj_core::acp::AWAITING_INPUT_STOP_REASON.into(),
                },
            }),
            ..Default::default()
        };
        let events = derive(&current, &event, &mutation);
        assert!(matches!(
            events.as_slice(),
            [
                ApiEventData::InputRequired {
                    request: None,
                    turn_id: Some(1)
                },
                ApiEventData::TurnEnded { .. }
            ]
        ));
        let json = serde_json::to_value(&events[0]).unwrap();
        assert!(json["data"].get("request").is_none());
    }

    #[test]
    fn inferred_finished_emits_turn_ended_without_input_required() {
        let current = MaterializedSession::empty("session");
        let event = RelayEvent {
            format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
            ordinal: 3,
            previous_digest: String::new(),
            digest: String::new(),
            recorded_at_ms: 1000,
            command_id: None,
            observation: RelayObservation::HarnessTurnStarted {
                started_at_ms: 1000,
            },
        };
        let mutation = MaterializedSessionMutation {
            last_turn_outcome: Some(MaterializedTurnOutcome {
                diagnostic: None,
                usage: None,
                command_id: "prompt".into(),
                accepted_ordinal: Some(1),
                turn_start_position: Some(2),
                completed_ordinal: 3,
                completed_at_ms: 1000,
                outcome: TurnOutcomeKind::Completed {
                    stop_reason: mj_core::acp::INFERRED_FINISHED_STOP_REASON.into(),
                },
            }),
            ..Default::default()
        };
        let events = derive(&current, &event, &mutation);
        assert!(matches!(
            events.as_slice(),
            [ApiEventData::TurnEnded { .. }]
        ));
    }
}
