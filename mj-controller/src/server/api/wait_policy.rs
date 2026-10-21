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
    pub assessment: Option<mj_core::assessment::Summary>,
    /// The worker's assessment record for the turn that ended last, whether a
    /// prompt or a turn the harness started itself. It owns Jev's verdict on
    /// that turn.
    pub turn_assessment: Option<mj_core::assessment::TurnAssessment>,
    /// The worker's activity state. It alone answers whether the session is
    /// idle, as it does for the dashboard.
    pub activity: mj_core::activity::ActivityState,
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
    /// Command ids of the prompts queued behind the active turn.
    pub queued: Vec<String>,
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
    /// How the turn that left `outcome` ended, or `None` while it has not.
    ///
    /// A turn has ended once the worker has stopped working on its request.
    /// It has not while the worker is about to send that same request again
    /// (a server retry armed for it, or that retry queued or running), while
    /// Jev is still reading its reply, or while a sub-agent child still owes
    /// its report for it. Nothing that starts a new command holds it: a turn
    /// the harness starts itself, background work, an expected continuation,
    /// Mjolnir's automatic continuation, a later prompt or a goal. Those are
    /// what the session does next.
    ///
    /// How it ended is the harness outcome, except where the turn's own
    /// assessment record says otherwise: Jev read the reply as a question for
    /// the user, or as a quota stop. A quota recovery recorded for the turn
    /// answers `quota_limit` with its notice, whether or not a resume is
    /// scheduled: the resume is a new command.
    fn turn_end(&self, outcome: &MaterializedTurnOutcome) -> Option<WaitDecision> {
        use mj_core::assessment::{Action, Status};
        if self.resubmits(outcome)
            || self.report_pending_for.as_deref() == Some(outcome.command_id.as_str())
        {
            return None;
        }
        let verdict = self.turn_assessment.as_ref().filter(|assessment| {
            assessment.turn_id == outcome.command_id
                && assessment.completed_ordinal == outcome.completed_ordinal
                && assessment.current()
        });
        // Pending until Jev answers; failed until the request is tried again.
        if verdict
            .is_some_and(|assessment| matches!(assessment.status, Status::Pending | Status::Failed))
        {
            return None;
        }
        let mut result = WaitDecision::from_outcome(outcome);
        let action = verdict.and_then(|assessment| assessment.action);
        let recovery = self
            .quota_recovery
            .as_ref()
            .filter(|recovery| recovery.completed_command_id == outcome.command_id);
        if recovery.is_some() || action == Some(Action::RecoverQuota) {
            result.outcome = WaitOutcome::QuotaLimit;
            if let Some(recovery) = recovery {
                result.message = Some(recovery.notice.clone());
            }
        } else if result.outcome == WaitOutcome::Finished && action == Some(Action::AwaitInput) {
            result.outcome = WaitOutcome::InputRequired;
            result.stop_reason = Some(mj_core::acp::AWAITING_INPUT_STOP_REASON.into());
        }
        Some(result)
    }

    /// Whether the worker is sending the request of the turn that left
    /// `outcome` again. A server retry carries the turn it retries in its
    /// command id, so this holds while the retry is armed, queued or running,
    /// and the retry's own ending is what answers for the turn.
    fn resubmits(&self, outcome: &MaterializedTurnOutcome) -> bool {
        let retries = |command_id: &str| {
            mj_core::relay::retried_turn(command_id) == Some(outcome.completed_ordinal)
        };
        self.capacity_retry
            .as_ref()
            .is_some_and(|retry| !retry.submitted && retries(&retry.command_id))
            || self
                .active_turn
                .as_ref()
                .is_some_and(|turn| retries(&turn.command_id))
            || self.queued.iter().any(|command_id| retries(command_id))
    }

    /// Whether the session has settled. The worker's activity state owns this.
    /// The rest are facts the worker cannot know: whether the daemon can hand
    /// the session a prompt, a prompt queued behind the active turn, and a
    /// prompt handed over at creation that has not ended as a turn yet. That
    /// prompt may not even show in the worker's view while it is submitted.
    fn settled(&self) -> bool {
        let first_prompt_open = match &self.start_status {
            Some(StartStatus::Pending) => true,
            Some(StartStatus::Submitted { turn_id }) => self
                .last_turn_outcome
                .as_ref()
                .and_then(|outcome| outcome.accepted_ordinal)
                .is_none_or(|ordinal| ordinal < *turn_id),
            _ => false,
        };
        self.activity.is_idle()
            && !self.cannot_take_prompt
            && !first_prompt_open
            && self.queued.is_empty()
    }

    /// What the session does next, for the answer: the worker's activity
    /// state, unless the session is idle.
    pub fn next(&self) -> Option<mj_core::activity::ActivityState> {
        (!self.activity.is_idle()).then(|| self.activity.clone())
    }

    /// What a `return_on_input` wait answers while the harness asks for
    /// structured input.
    fn input_request(&self) -> Option<WaitDecision> {
        (!self.pending_elicitations.is_empty()).then(|| WaitDecision {
            outcome: WaitOutcome::InputRequired,
            stop_reason: None,
            message: Some("the harness needs a response to a structured input request".into()),
            turn_id: self
                .active_turn
                .as_ref()
                .and_then(|turn| turn.accepted_ordinal),
            turn: None,
        })
    }

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
        use mj_core::event_outcome::{OutcomeReason, TurnResultKind};
        let result = outcome.result();
        let kind = match result.kind {
            TurnResultKind::Completed => WaitOutcome::Finished,
            TurnResultKind::InputRequired => WaitOutcome::InputRequired,
            TurnResultKind::Cancelled => WaitOutcome::Cancelled,
            TurnResultKind::Failed if result.reason == Some(OutcomeReason::QuotaLimit) => {
                WaitOutcome::QuotaLimit
            }
            TurnResultKind::Rejected | TurnResultKind::Interrupted | TurnResultKind::Failed => {
                WaitOutcome::Error
            }
        };
        let stop_reason = result.stop_reason;
        let message = result.message;
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
/// A wait asks one of two questions, and each has one owner. With a turn
/// (`--turn N`, and `mj prompt`, which passes the turn it submitted): did
/// prompted turn N end, and how? [`WaitObservation::turn_end`] answers that.
/// Without one: has the session settled? [`WaitObservation::settled`] answers
/// that, from the worker's activity state.
///
/// A session that is carrying an error from some earlier, unrelated action is
/// not a reason to fail the turn the caller asked about: the session error
/// badge has no expiry, and reporting it here made every later wait on that
/// session return `error` while the turn ran on perfectly well. A turn that
/// really did fail reports `error` through its own outcome.
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
/// 3. A wait on turn N answers once the latest turn outcome was accepted at or
///    after N and that turn has ended. It never waits for what the session
///    does next; the response names that as `next`. A session keeps only its
///    latest outcome, so the answer can describe a later turn than N.
/// 4. A wait without a turn answers once the session has settled, with how
///    the latest turn ended. A settled session with no turn at all has
///    nothing to wait for and answers `finished` at once.
///
/// Either form with `return_on_input` answers `input_required` while the
/// harness asks for structured input, unless turn N has already ended.
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
    if let Some(target) = request.turn_id {
        let ended = observation
            .last_turn_outcome
            .as_ref()
            .filter(|outcome| {
                outcome
                    .accepted_ordinal
                    .is_some_and(|ordinal| ordinal >= target)
            })
            .and_then(|outcome| observation.turn_end(outcome));
        return match ended {
            None if request.return_on_input => observation.input_request(),
            ended => ended,
        };
    }
    if request.return_on_input
        && let Some(asking) = observation.input_request()
    {
        return Some(asking);
    }
    if !observation.settled() {
        return None;
    }
    match observation.last_turn_outcome.as_ref() {
        Some(outcome) => observation.turn_end(outcome),
        None => Some(WaitDecision::simple(WaitOutcome::Finished, None)),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------
