use super::*;

/// Classify a harness stop reason.
///
/// Stop reasons are free text the harness chooses, so the comparison is
/// case-insensitive and tolerates both `end_turn` and `endTurn`. Anything
/// unrecognized is an error carrying the raw reason, because silently calling
/// an unknown ending "finished" would tell the caller its work succeeded when
/// nobody knows that it did.
pub fn map_stop_reason(stop_reason: &str) -> (WaitOutcome, Option<String>) {
    use mj_core::state::{PromptCompletion, classify_prompt_completion};

    match classify_prompt_completion(stop_reason) {
        PromptCompletion::Finished => (WaitOutcome::Finished, None),
        PromptCompletion::Cancelled => (WaitOutcome::Cancelled, None),
        PromptCompletion::QuotaLimit => (WaitOutcome::QuotaLimit, None),
        PromptCompletion::Error => (WaitOutcome::Error, Some(stop_reason.to_owned())),
    }
}

/// Everything one pass of the wait loop knows about a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitObservation {
    pub background_work: Option<ApiBackgroundWork>,
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
    pub lifecycle: Option<ViewerLifecycleCategory>,
    /// A recorded launch failure names this session.
    pub launch_failed: bool,
    /// Why the launch failed, when a reason was recorded.
    pub launch_error: Option<String>,
    pub execution: MaterializedExecutionState,
    pub active_turn: Option<MaterializedTurn>,
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    pub queued: usize,
    pub capacity_retry: Option<CapacityRetry>,
    pub start_status: Option<StartStatus>,
}

/// What one pass of the wait loop concluded, before the turn summary is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitDecision {
    pub outcome: WaitOutcome,
    pub stop_reason: Option<String>,
    pub message: Option<String>,
    pub turn_id: Option<u64>,
    /// Where the finished turn began, so its summary can be read.
    pub turn_start_position: Option<u64>,
}

impl WaitDecision {
    pub(super) fn simple(outcome: WaitOutcome, message: Option<String>) -> Self {
        Self {
            outcome,
            stop_reason: None,
            message,
            turn_id: None,
            turn_start_position: None,
        }
    }

    pub(super) fn from_outcome(outcome: &MaterializedTurnOutcome) -> Self {
        let (kind, stop_reason, message) = match &outcome.outcome {
            TurnOutcomeKind::Completed { stop_reason } => {
                let (kind, message) = map_stop_reason(stop_reason);
                (
                    kind,
                    Some(stop_reason.clone()),
                    outcome
                        .diagnostic
                        .as_ref()
                        .map(|d| d.message.clone())
                        .or(message),
                )
            }
            TurnOutcomeKind::Rejected { message } => {
                (WaitOutcome::Error, None, Some(message.clone()))
            }
            TurnOutcomeKind::Interrupted { message } => {
                (WaitOutcome::Error, None, Some(message.clone()))
            }
        };
        Self {
            outcome: kind,
            stop_reason,
            message,
            turn_id: outcome.accepted_ordinal,
            turn_start_position: outcome.turn_start_position,
        }
    }
}

/// Decide whether this observation ends the wait.
///
/// A wait answers for one turn, so only the turn's own fate ends it. In
/// particular a session that is carrying an error from some earlier, unrelated
/// action is not a reason to fail the turn the caller asked about: the session
/// error badge has no expiry, and reporting it here made every later wait on
/// that session return `error` while the turn ran on perfectly well.
///
/// The rules run in order, and the order is the point:
///
/// 1. A stopped or stopping session ends the wait as `stopped`, superseding
///    any initialization result that raced with the close request.
/// 2. A launch failure or failed initialization is reported before a turn; a durable
///    failed lifecycle ends it as `error` even after a daemon restart.
/// 3. Otherwise the wait has a target turn: the caller's explicit `turn_id`,
///    else the turn a create-with-prompt call submitted, else "the newest
///    one", which additionally requires the session to be idle with an empty
///    queue — with queued prompts, "idle" alone would return an earlier
///    prompt's outcome.
/// 4. A capacity outcome with a retry armed is not an ending: the worker will
///    submit the retry itself, so the wait keeps waiting.
///
/// A turn that really did fail still reports `error`: a rejected or interrupted
/// turn, and an unrecognized stop reason, all come back through the turn record
/// in rule 3.
pub fn resolve_wait(observation: &WaitObservation, request: &WaitRequest) -> Option<WaitDecision> {
    let stopping = matches!(
        observation.lifecycle,
        Some(ViewerLifecycleCategory::Stopped | ViewerLifecycleCategory::Stopping)
    ) || matches!(
        observation.execution,
        MaterializedExecutionState::Closing | MaterializedExecutionState::Closed
    );
    if stopping {
        return Some(WaitDecision::simple(
            WaitOutcome::Stopped,
            Some("the session is stopped or stopping".to_owned()),
        ));
    }
    if observation.launch_failed {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some(
                observation
                    .launch_error
                    .clone()
                    .unwrap_or_else(|| "the session failed to launch".to_owned()),
            ),
        ));
    }
    if let Some(StartStatus::Failed { message }) = &observation.start_status {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some(message.clone()),
        ));
    }
    if observation.lifecycle == Some(ViewerLifecycleCategory::Failed) {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some("the session is in a failed state".to_owned()),
        ));
    }
    let retry_pending = |outcome: &MaterializedTurnOutcome| {
        observation.capacity_retry.is_some()
            && matches!(
                &outcome.outcome,
                TurnOutcomeKind::Completed { stop_reason } if is_capacity_stop_reason(stop_reason)
            )
    };
    let target = request.turn_id.or(match &observation.start_status {
        Some(StartStatus::Submitted { turn_id }) => Some(*turn_id),
        _ => None,
    });
    let target_finished = target.is_some_and(|target| {
        observation
            .last_turn_outcome
            .as_ref()
            .is_some_and(|outcome| {
                outcome
                    .accepted_ordinal
                    .is_some_and(|ordinal| ordinal >= target)
                    && !retry_pending(outcome)
            })
    });
    if request.return_on_input && !target_finished && !observation.pending_elicitations.is_empty() {
        return Some(WaitDecision {
            outcome: WaitOutcome::InputRequired,
            stop_reason: None,
            message: Some("the harness needs a response to a structured input request".into()),
            turn_id: observation
                .active_turn
                .as_ref()
                .and_then(|turn| turn.accepted_ordinal),
            turn_start_position: None,
        });
    }
    match target {
        Some(target) => {
            let outcome = observation.last_turn_outcome.as_ref()?;
            if outcome
                .accepted_ordinal
                .is_none_or(|ordinal| ordinal < target)
            {
                return None;
            }
            if retry_pending(outcome) {
                return None;
            }
            Some(WaitDecision::from_outcome(outcome))
        }
        None => {
            if observation.execution != MaterializedExecutionState::Idle
                || observation.active_turn.is_some()
                || observation.queued > 0
            {
                return None;
            }
            match observation.last_turn_outcome.as_ref() {
                Some(outcome) if retry_pending(outcome) => None,
                Some(outcome) => Some(WaitDecision::from_outcome(outcome)),
                // Idle with nothing queued and nothing ever finished: there is
                // no turn to wait for, so say so immediately rather than block
                // for the full timeout.
                None => Some(WaitDecision::simple(WaitOutcome::Finished, None)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------
