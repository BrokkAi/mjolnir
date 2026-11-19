use super::*;

impl ActiveChat {
    /// Answers a form the reviewer's harness is waiting on.
    pub(crate) fn answer_reviewer(
        &mut self,
        role: Option<String>,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
        draft: Option<ChatElicitationDraft>,
    ) {
        let child = match role.as_ref() {
            Some(child) => {
                let owns_question = self.state.turn_review().is_some_and(|review| {
                    review.view.owns_reviewer(child)
                        && review.view.questions.iter().any(|question| {
                            question.role == *child && question.request.id == elicitation_id
                        })
                });
                if !owns_question {
                    let error = "The reviewer's question is no longer active".to_owned();
                    if let Some(draft) = draft {
                        self.state
                            .restore_failed_reviewer_elicitation(draft, error.clone());
                    }
                    self.report_review_refusal(error);
                    return;
                }
                child.clone()
            }
            None => {
                if let Some(draft) = draft {
                    let error = "The reviewer for this question is no longer available".to_owned();
                    self.state
                        .restore_failed_reviewer_elicitation(draft, error.clone());
                    self.report_review_refusal(error);
                }
                return;
            }
        };
        queue_chat_remote_operation(
            self.remote.operations(),
            ChatRemoteOperation::ReviewerAnswer {
                role: child,
                request_id: elicitation_id,
                response,
                draft: draft.map(Box::new),
            },
            &mut self.state,
        );
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
            if self
                .state
                .reviewer_answer_pending(role.as_deref(), &request)
            {
                continue;
            }
            // A reviewer's plan decision is the reviewer proposing work, not
            // the plan under review. It is never shown as the primary's
            // decision; the reviewer was asked to critique, not to implement,
            // so it is declined and its critique stands as the answer.
            if mj_core::acp::is_plan_review_id(&request.id) {
                self.answer_reviewer(
                    role,
                    request.id,
                    mj_core::acp::plan_review_keep_planning(),
                    None,
                );
                continue;
            }
            if self.state.reviewer_elicitation_open() {
                self.apply_deferred_elicitation_draft();
                self.state.apply_deferred_form_results();
                return;
            }
            if !self.state.show_review_role_elicitation(role, request) {
                self.apply_deferred_elicitation_draft();
                self.state.apply_deferred_form_results();
                return;
            }
            self.apply_deferred_elicitation_draft();
        }
        self.apply_deferred_elicitation_draft();
        self.state.apply_deferred_form_results();
    }
}
