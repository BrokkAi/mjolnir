use super::*;
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
            message,
            ..
        }
        | RelayObservation::CommandInterrupted {
            command_id,
            message,
            ..
        } => {
            events.push(ApiEventData::Error {
                message: message.clone(),
                command_id: Some(command_id.clone()),
            });
        }
        _ => {}
    }
    if let Some(turn) = &mutation.last_turn_outcome {
        if let mj_core::state::TurnOutcomeKind::Completed { stop_reason } = &turn.outcome {
            match mj_core::state::classify_prompt_completion(stop_reason) {
                mj_core::state::PromptCompletion::InputRequired => {
                    events.push(ApiEventData::InputRequired {
                        request: None,
                        turn_id: turn.accepted_ordinal,
                    });
                }
                mj_core::state::PromptCompletion::Error => {
                    events.push(ApiEventData::Error {
                        message: stop_reason.clone(),
                        command_id: Some(turn.command_id.clone()),
                    });
                }
                _ => {}
            }
        }
        events.push(ApiEventData::TurnEnded { turn: turn.clone() });
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
    fn structured_input_event_keeps_its_request_payload() {
        let request = mj_core::elicitation::ElicitationRequest {
            id: "question".into(),
            message: "Which branch?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        let event = ApiEventData::InputRequired {
            request: Some(request.clone()),
            turn_id: Some(1),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json["data"]["request"],
            serde_json::to_value(request).unwrap()
        );
        assert_eq!(serde_json::from_value::<ApiEventData>(json).unwrap(), event);
    }

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
}
