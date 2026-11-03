use super::*;

impl ActiveChat {
    /// Answers a form the reviewer's harness is waiting on.
    pub(crate) fn answer_reviewer(
        &mut self,
        role: Option<String>,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) {
        let control = self.session_manager.clone();
        let child = match role.as_ref() {
            Some(child) => {
                let owns_question = self.state.turn_review().is_some_and(|review| {
                    review.view.owns_reviewer(child)
                        && review.view.questions.iter().any(|question| {
                            question.role == *child && question.request.id == elicitation_id
                        })
                });
                if !owns_question {
                    self.report_review_refusal(
                        "The reviewer's question is no longer active".to_owned(),
                    );
                    return;
                }
                child.clone()
            }
            None => return,
        };
        let updates = self.chat_io_tx.clone();
        let answered_role = role.clone();
        let automatic = mj_core::acp::is_plan_review_id(&elicitation_id);
        tokio::spawn(async move {
            let result = match control.session(child).await {
                Ok(handle) => handle.respond_elicitation(elicitation_id, response).await,
                Err(error) => Err(error),
            }
            .map_err(|error| format!("{error:#}"));
            let Some(update) = reviewer_answer_update(result, automatic) else {
                return;
            };
            if let Err(error) = updates.send(update) {
                tracing::debug!(%error, "reviewer form answer result dropped");
            }
        });
        if let Some(role) = answered_role {
            self.poll_turn_review_role(&role);
        }
    }

    /// Puts a form the reviewer is waiting on in front of the user, or answers
    /// it for them when it is not theirs to answer.
    pub(crate) fn surface_reviewer_elicitations(&mut self) {
        let pending: Vec<(Option<String>, mj_core::elicitation::ElicitationRequest)> = self
            .state
            .turn_review()
            .map(|review| {
                review
                    .pending_elicitations()
                    .into_iter()
                    .map(|(role, request)| (Some(role), request))
                    .collect()
            })
            .unwrap_or_default();
        self.state.reconcile_reviewer_elicitation(&pending);
        for (role, request) in pending {
            // A reviewer's plan decision is the reviewer proposing work, not
            // the plan under review. It is never shown as the primary's
            // decision; the reviewer was asked to critique, not to implement,
            // so it is declined and its critique stands as the answer.
            if mj_core::acp::is_plan_review_id(&request.id) {
                self.answer_reviewer(role, request.id, mj_core::acp::plan_review_keep_planning());
                continue;
            }
            if self.state.reviewer_elicitation_open() {
                return;
            }
            if !self.state.show_review_role_elicitation(role, request) {
                return;
            }
        }
        self.apply_deferred_elicitation_draft();
    }

    /// Reads the reviewer's journal from where its pane left off.
    pub(crate) fn poll_turn_review_role(&self, role: &str) {
        self.poll_turn_review_role_after(role, Duration::ZERO);
    }

    /// The same, after `delay`.
    ///
    /// An attach answers at once even when the journal has not moved, so idle
    /// journals wait a beat before asking again.
    pub(crate) fn poll_turn_review_role_after(&self, role: &str, delay: Duration) {
        let Some(review) = self.state.turn_review() else {
            return;
        };
        if !review.view.owns_reviewer(role) {
            return;
        }
        let child = role.to_owned();
        let control = self.session_manager.clone();
        let updates = self.chat_io_tx.clone();
        let role = child.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = control
                .snapshot(child)
                .await
                .map(Box::new)
                .map_err(|error| format!("{error:#}"));
            if let Err(error) = updates.send(ChatIoUpdate::TurnReviewEvents { role, result }) {
                tracing::debug!(%error, "review events dropped because the chat closed");
            }
        });
    }

    pub(crate) fn apply_turn_review_role_events(
        &mut self,
        child: String,
        result: std::result::Result<Box<mj_core::state::MaterializedSession>, String>,
    ) {
        let Some(review) = self.state.turn_review_mut() else {
            self.reviewed_roles.clear();
            return;
        };
        if !review.view.owns_reviewer(&child) {
            self.reviewed_roles.remove(&child);
            return;
        }
        let role_state = review
            .view
            .roles
            .iter()
            .find(|role| role.child_session_id.as_deref() == Some(&child))
            .map(|role| role.state);
        let mut active =
            role_state.is_some_and(|state| reviewer_transcript_needs_poll(state, &child, None));
        match result {
            Ok(snapshot) => {
                if snapshot.session_id == child {
                    active = role_state.is_some_and(|state| {
                        reviewer_transcript_needs_poll(state, &child, Some(&snapshot))
                    });
                    if let Some(pane) = review.pane_for(&child) {
                        pane.replace_snapshot(*snapshot);
                    }
                } else {
                    tracing::warn!(%child, actual = %snapshot.session_id, "reviewer snapshot had the wrong session id");
                }
            }
            Err(error) => {
                tracing::debug!(%child, %error, "reviewer transcript is not available yet");
            }
        }
        self.surface_reviewer_elicitations();
        if active {
            self.poll_turn_review_role_after(&child, REVIEW_POLL_IDLE_INTERVAL);
        }
    }
}

/// A reviewer handback can precede its harness turn completion. The child is
/// fresh for this review, so its started prompt's terminal outcome is the
/// journal's own barrier after the review transcript. Suspension can interrupt
/// that prompt after handback. Failed launches may have no turn.
fn reviewer_transcript_needs_poll(
    state: mj_core::review::driver::RoleState,
    child: &str,
    snapshot: Option<&mj_core::state::MaterializedSession>,
) -> bool {
    use mj_core::review::driver::RoleState;
    use mj_core::state::TurnOutcomeKind;

    let own = snapshot.filter(|snapshot| snapshot.session_id == child);
    match state {
        RoleState::Pending | RoleState::Running => true,
        RoleState::Clean | RoleState::Findings => !own.is_some_and(|snapshot| {
            snapshot.last_turn_outcome.as_ref().is_some_and(|outcome| {
                outcome.turn_start_position.is_some()
                    && matches!(
                        &outcome.outcome,
                        TurnOutcomeKind::Completed { .. } | TurnOutcomeKind::Interrupted { .. }
                    )
                    && outcome.completed_ordinal <= snapshot.applied_event_ordinal
            })
        }),
        RoleState::Failed => own.is_some_and(|snapshot| {
            snapshot.active_turn.is_some() || !snapshot.queued_prompts.is_empty()
        }),
    }
}

/// What the chat hears about one answer to a reviewer's form.
///
/// A request that is no longer pending was answered, withdrawn, or expired
/// before the answer arrived. That is not a failed review, so it is said as a
/// notice rather than reported as a review failure; and an answer Mjolnir
/// sent on its own (a reviewer's plan decision) needs no notice at all
/// (I2-5).
fn reviewer_answer_update(
    result: std::result::Result<(), String>,
    automatic: bool,
) -> Option<ChatIoUpdate> {
    match result {
        Err(error) if error.contains("is no longer pending") => (!automatic).then(|| {
            ChatIoUpdate::ReviewerNotice(
                "The reviewer's request had already ended (answered elsewhere, withdrawn, or \
                 expired), so this answer was not delivered."
                    .to_owned(),
            )
        }),
        result => Some(ChatIoUpdate::ReviewerStarted(result)),
    }
}

#[cfg(test)]
mod answer_tests {
    use super::*;

    #[test]
    fn a_terminal_reviewer_waits_for_its_own_completed_prompt_snapshot() {
        use mj_core::review::driver::RoleState;
        use mj_core::state::{MaterializedSession, MaterializedTurnOutcome, TurnOutcomeKind};

        let mut child = MaterializedSession::empty("review-child");
        assert!(reviewer_transcript_needs_poll(
            RoleState::Findings,
            "review-child",
            Some(&child),
        ));
        child.applied_event_ordinal = 8;
        child.last_turn_outcome = Some(MaterializedTurnOutcome {
            diagnostic: None,
            usage: None,
            command_id: "review-prompt".into(),
            accepted_ordinal: Some(2),
            turn_start_position: Some(3),
            completed_ordinal: 8,
            completed_at_ms: 0,
            outcome: TurnOutcomeKind::Completed {
                stop_reason: "end_turn".into(),
            },
        });
        assert!(!reviewer_transcript_needs_poll(
            RoleState::Findings,
            "review-child",
            Some(&child),
        ));
        child.last_turn_outcome.as_mut().unwrap().outcome = TurnOutcomeKind::Interrupted {
            message: "suspended after handback".into(),
            reason: None,
        };
        assert!(!reviewer_transcript_needs_poll(
            RoleState::Findings,
            "review-child",
            Some(&child),
        ));
        child.applied_event_ordinal = 7;
        assert!(reviewer_transcript_needs_poll(
            RoleState::Findings,
            "review-child",
            Some(&child),
        ));
        assert!(reviewer_transcript_needs_poll(
            RoleState::Clean,
            "another-child",
            Some(&child),
        ));
    }

    // Hard-won: 7c4a57a: late reviewer answers were silently dropped and misreported as review failures
    #[test]
    fn a_late_answer_says_why_it_was_not_delivered() {
        let late = Err(r#"elicitation "tool-permission-2" is no longer pending"#.to_owned());
        match reviewer_answer_update(late.clone(), false) {
            Some(ChatIoUpdate::ReviewerNotice(notice)) => {
                assert!(notice.contains("not delivered"), "{notice}");
            }
            _ => panic!("a person's late answer gets a notice"),
        }
        assert!(
            reviewer_answer_update(late, true).is_none(),
            "Mjolnir's own answer to a plan decision needs no notice"
        );
        assert!(matches!(
            reviewer_answer_update(Err("relay unavailable".into()), false),
            Some(ChatIoUpdate::ReviewerStarted(Err(_)))
        ));
    }
}
