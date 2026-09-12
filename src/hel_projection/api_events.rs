use super::*;
use crate::hel_database::ApiEventData;

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
        if let crate::hel_state::TurnOutcomeKind::Completed { stop_reason } = &turn.outcome
            && crate::hel_state::classify_prompt_completion(stop_reason)
                == crate::hel_state::PromptCompletion::Error
        {
            events.push(ApiEventData::Error {
                message: stop_reason.clone(),
                command_id: Some(turn.command_id.clone()),
            });
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
                    request: request.clone(),
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
