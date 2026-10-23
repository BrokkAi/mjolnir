use super::*;

impl ActiveChat {
    /// Answers a form the reviewer's harness is waiting on.
    pub(crate) fn answer_reviewer(
        &mut self,
        role: Option<String>,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) {
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        let answered_role = role.clone();
        let automatic = mj_core::acp::is_plan_review_id(&elicitation_id);
        tokio::spawn(async move {
            let result = session
                .reviewer_as(
                    role,
                    ReviewerAction::RespondElicitation {
                        elicitation_id,
                        response,
                    },
                )
                .await
                .map(|_| ())
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
    pub(crate) fn poll_turn_review_role_after(&self, role: &str, delay: Duration) {
        let Some(review) = self.state.turn_review() else {
            return;
        };
        let (after_ordinal, cursor_digest) = review.cursor();
        let after_digest = if cursor_digest.is_empty() {
            mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            cursor_digest
        };
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        let role = role.to_owned();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = match session
                .reviewer_as(
                    Some(role.clone()),
                    ReviewerAction::Attach {
                        after_ordinal,
                        after_digest,
                    },
                )
                .await
            {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                Err(error) => Err(format!("{error:#}")),
            };
            if let Err(error) = updates.send(ChatIoUpdate::TurnReviewEvents { role, result }) {
                tracing::debug!(%error, "review events dropped because the chat closed");
            }
        });
    }

    /// Folds reviewer events into its pane, and keeps reading.
    ///
    /// Display only: the daemon reads the same journals to drive the review,
    /// and nothing here advances it. Two readers on one journal are safe --
    /// an attach is a read at a cursor, and only the host acknowledges.
    pub(crate) fn apply_turn_review_role_events(
        &mut self,
        role: String,
        result: std::result::Result<Vec<mj_core::relay::RelayEvent>, String>,
    ) {
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                // A journal this terminal cannot read is a display problem,
                // not a review problem: the daemon is still running it.
                tracing::debug!(%role, %error, "could not read a reviewing role's journal");
                self.report_review_refusal(error);
                return;
            }
        };
        let idle = events.is_empty();
        let session_id = review_role_session_id(self.session.session_id(), &role);
        if let Some(review) = self.state.turn_review_mut()
            && !events.is_empty()
        {
            review.pane().apply_events(&session_id, &events);
        }
        self.surface_reviewer_elicitations();
        if self.state.turn_review().is_none() {
            if self.reviewed_role.as_deref() == Some(role.as_str()) {
                self.reviewed_role = None;
            }
            return;
        }
        let Some(review) = self.state.turn_review() else {
            return;
        };
        if !review.view.roles.iter().any(|status| status.role == role) {
            return;
        }
        if !review.role_is_active(&role) {
            // A verdict keeps the reviewer row for context but needs no more
            // journal attaches.
            if self.reviewed_role.as_deref() == Some(role.as_str()) {
                self.reviewed_role = None;
            }
            return;
        }
        self.poll_turn_review_role_after(
            &role,
            if idle {
                REVIEW_POLL_IDLE_INTERVAL
            } else {
                Duration::ZERO
            },
        );
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
