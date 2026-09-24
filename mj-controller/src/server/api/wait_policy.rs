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
        PromptCompletion::InputRequired => (WaitOutcome::InputRequired, None),
        PromptCompletion::Finished => (WaitOutcome::Finished, None),
        PromptCompletion::Cancelled => (WaitOutcome::Cancelled, None),
        PromptCompletion::QuotaLimit => (WaitOutcome::QuotaLimit, None),
        PromptCompletion::Error => (WaitOutcome::Error, Some(stop_reason.to_owned())),
    }
}

/// Everything one pass of the wait loop knows about a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitObservation {
    pub checking_continuation: bool,
    pub background_work: Option<ApiBackgroundWork>,
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
    pub lifecycle: Option<ViewerLifecycleCategory>,
    /// A resume operation owns this session now. Its durable record still says
    /// stopped — it stays stopped until the archive has been verified — so
    /// without this a wait would answer `stopped` for a session that is on its
    /// way up.
    pub resuming: bool,
    /// A close owns this session now. Like `resuming`, this ends nothing: the
    /// wait follows the close until it finishes.
    pub closing: bool,
    /// The reason a close recorded on a session that is alive again, which is
    /// what a close that failed leaves behind. It is published only until the
    /// next action or transition for the session succeeds, so it always refers
    /// to a close nobody has recovered from.
    pub close_failure: Option<String>,
    /// The session cannot take a prompt yet: it is still provisioning, a
    /// lifecycle operation owns it, or its worker is not attached. A wait with
    /// no target turn keeps waiting, because "finished" would invite a prompt
    /// that is then refused.
    pub cannot_take_prompt: bool,
    /// A recorded launch failure names this session.
    pub launch_failed: bool,
    /// Why the launch failed, when a reason was recorded.
    pub launch_error: Option<String>,
    pub execution: MaterializedExecutionState,
    pub active_turn: Option<MaterializedTurn>,
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    pub queued: usize,
    pub capacity_retry: Option<CapacityRetry>,
    pub retry_assessment_pending: bool,
    pub quota_recovery: Option<mj_core::continuation::QuotaRecovery>,
    pub start_status: Option<StartStatus>,
    /// This session is a Mjolnir sub-agent child, whose answer says where its
    /// report came from.
    pub subagent: bool,
    /// The finished turn, by command id, whose child still owes its report.
    /// Mjolnir reminds the child to hand it back, so like an armed retry the
    /// turn is not an ending yet.
    pub report_pending_for: Option<String>,
    /// The report a child handed back, and the turn it answers.
    pub handback: Option<(String, String)>,
}

impl WaitObservation {
    /// Fold a child's recorded report into this observation, judged against
    /// the turn this observation saw finish.
    pub fn apply_subagent_report(
        &mut self,
        handback_tool: bool,
        report: &mj_core::subagent::SubagentReport,
        now_ms: i64,
    ) {
        self.subagent = true;
        let Some(turn) = self.last_turn_outcome.as_ref() else {
            return;
        };
        let in_flight = self
            .active_turn
            .iter()
            .map(|turn| turn.command_id.as_str())
            .collect::<Vec<_>>();
        match mj_core::subagent::report_state(handback_tool, report, Some(turn), &in_flight, now_ms)
        {
            mj_core::subagent::ReportState::Pending { .. } => {
                self.report_pending_for = Some(turn.command_id.clone());
            }
            mj_core::subagent::ReportState::Delivered(message) => {
                self.handback = Some((turn.command_id.clone(), message));
            }
            mj_core::subagent::ReportState::Fallback => {}
        }
    }
}

/// The transcript positions one finished turn covers.
///
/// A turn is a span, not a starting point. The session keeps recording after a
/// turn ends — a harness resume notice arrives as an agent message of its own —
/// and only what falls inside the span is that turn's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSpan {
    pub start_position: u64,
    pub completed_position: u64,
}

/// What one pass of the wait loop concluded, before the turn summary is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitDecision {
    pub outcome: WaitOutcome,
    pub stop_reason: Option<String>,
    pub message: Option<String>,
    pub turn_id: Option<u64>,
    /// Which transcript positions the finished turn covers, so its summary can
    /// be read.
    pub turn: Option<TurnSpan>,
}

impl WaitDecision {
    pub(super) fn simple(outcome: WaitOutcome, message: Option<String>) -> Self {
        Self {
            outcome,
            stop_reason: None,
            message,
            turn_id: None,
            turn: None,
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
            turn: outcome.turn_start_position.map(|start_position| TurnSpan {
                start_position,
                completed_position: outcome.completed_ordinal,
            }),
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
/// 0. A resume running for this session ends nothing: it is a session coming
///    up, and its durable record says stopped until the archive is verified.
///    A close running for it ends nothing either, for the same reason in
///    reverse: the wait follows it and reports how it ended. A close that
///    left the session alive failed, and this is the only place left to say
///    so, because the request that asked for it was answered when it was
///    admitted. That reason outlives the wait that started it, on purpose: a
///    close can fail before the next command has even connected, and it is
///    cleared as soon as anything for the session succeeds.
/// 1. A stopped or stopping session ends the wait as `stopped`, superseding
///    any initialization result that raced with the close request.
/// 2. A launch failure or failed initialization is reported before a turn; a durable
///    failed lifecycle ends it as `error` even after a daemon restart.
/// 3. Otherwise the wait has a target turn: the caller's explicit `turn_id`,
///    else the turn a create-with-prompt call submitted, else "the newest
///    one", which additionally requires the session to be idle with an empty
///    queue — with queued prompts, "idle" alone would return an earlier
///    prompt's outcome — and able to take a prompt, so a session that is
///    still provisioning or reattaching is not reported as finished.
/// 4. A completed turn under server assessment or with a retry armed is not an
///    ending: the worker may submit the retry itself, so the wait keeps waiting.
///
/// A turn that really did fail still reports `error`: a rejected or interrupted
/// turn, and an unrecognized stop reason, all come back through the turn record
/// in rule 3.
pub fn resolve_wait(observation: &WaitObservation, request: &WaitRequest) -> Option<WaitDecision> {
    let stopping = matches!(
        observation.lifecycle,
        Some(ViewerLifecycleCategory::Suspended | ViewerLifecycleCategory::Suspending)
    ) || matches!(
        observation.execution,
        MaterializedExecutionState::Closing | MaterializedExecutionState::Closed
    );
    // A resume owns the session: nothing about it has settled yet, and its
    // durable record still says stopped. The wait keeps waiting; its own
    // deadline still bounds it.
    if observation.resuming {
        return None;
    }
    if let Some(reason) = &observation.close_failure {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some(reason.clone()),
        ));
    }
    // The close owns the session; its own deadline still bounds this wait.
    if observation.closing {
        return None;
    }
    if stopping {
        return Some(WaitDecision::simple(
            WaitOutcome::Stopped,
            // A resume that failed rolled the record back to stopped and left
            // its reason there. Reporting it is the difference between "the
            // session is stopped" and knowing why it did not come up.
            Some(
                observation
                    .launch_error
                    .clone()
                    .unwrap_or_else(|| "the session is stopped or stopping".to_owned()),
            ),
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
            // A close that left the session dead recorded why; saying only
            // that it failed would throw that away.
            Some(
                observation
                    .launch_error
                    .clone()
                    .unwrap_or_else(|| "the session is in a failed state".to_owned()),
            ),
        ));
    }
    let retry_pending = |outcome: &MaterializedTurnOutcome| {
        observation.report_pending_for.as_deref() == Some(outcome.command_id.as_str())
            || observation.retry_assessment_pending
            || observation.capacity_retry.is_some()
            || observation.quota_recovery.as_ref().is_some_and(|r| {
                r.retry_at_ms.is_some() && r.completed_command_id == outcome.command_id
            })
    };
    if let Some(recovery) = &observation.quota_recovery
        && recovery.retry_at_ms.is_none()
        && observation
            .last_turn_outcome
            .as_ref()
            .is_some_and(|t| t.command_id == recovery.completed_command_id)
        && request.turn_id.is_none_or(|target| {
            observation
                .last_turn_outcome
                .as_ref()
                .and_then(|t| t.accepted_ordinal)
                .is_some_and(|a| a >= target)
        })
    {
        return Some(WaitDecision::simple(
            WaitOutcome::QuotaLimit,
            Some(recovery.notice.clone()),
        ));
    }
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
            turn: None,
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
            if observation.checking_continuation
                || observation.cannot_take_prompt
                || observation.execution != MaterializedExecutionState::Idle
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaiting_input_is_a_successful_wait_outcome() {
        assert_eq!(
            map_stop_reason(mj_core::acp::AWAITING_INPUT_STOP_REASON),
            (WaitOutcome::InputRequired, None)
        );
    }
}
