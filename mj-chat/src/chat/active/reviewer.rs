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
                let owner = self
                    .state
                    .turn_review()
                    .and_then(|review| review.view.child_session_id.as_ref());
                if owner != Some(child) {
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
    /// An attach answers at once even when the journal has not moved, so a
    /// loop that re-attaches on every empty page is a spin. A review runs
    /// review uses one role, so an idle journal waits a beat before asking
    /// again; a page that did carry events is followed up immediately, which
    /// keeps a streaming answer smooth.
    pub(crate) fn poll_turn_review_role_after(&self, _role: &str, delay: Duration) {
        let Some(review) = self.state.turn_review() else {
            return;
        };
        let Some(child) = review.view.child_session_id.clone() else {
            return;
        };
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
            self.reviewed_role = None;
            return;
        };
        if review.view.child_session_id.as_deref() != Some(&child) {
            return;
        }
        match result {
            Ok(snapshot) => {
                review.pane().replace_snapshot(*snapshot);
            }
            Err(error) => {
                tracing::debug!(%child, %error, "reviewer transcript is not available yet");
            }
        }
        let active = review.view.is_working() || !review.view.questions.is_empty();
        self.surface_reviewer_elicitations();
        if active {
            self.poll_turn_review_role_after(&child, REVIEW_POLL_IDLE_INTERVAL);
        } else {
            self.reviewed_role = None;
        }
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
