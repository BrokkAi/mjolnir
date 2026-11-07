use super::*;

impl HostState {
    pub(super) fn run_one(&mut self, session_id: &str, request: ReviewRequest) {
        match request {
            ReviewRequest::CaptureDelta => {
                self.review_step(
                    session_id,
                    ReviewerAction::CaptureDelta { repositories: None },
                    |outcome| {
                        ReviewStep::Delta(match outcome {
                            Ok(ReviewerOutcome::Delta { repositories }) => Ok(repositories),
                            other => Err(unexpected(other)),
                        })
                    },
                );
            }
            ReviewRequest::StartReviewer {
                role,
                command_id: _,
                prompt,
            } => self.register_reviewer_child(session_id, role, prompt),
            ReviewRequest::CloseReviewer { role } => {
                self.close_reviewer_role(session_id, &role);
            }
            ReviewRequest::Close => {
                if self.closing.contains(session_id) {
                    return;
                }
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    let outcome = if matches!(
                        slot.driver.phase(),
                        TurnReviewPhase::Resolved(Resolution::Cancelled)
                    ) {
                        Err(StartRefusal(
                            "review cancelled before its reviewer started".into(),
                        ))
                    } else {
                        Ok(())
                    };
                    answer(slot.start_reply.take(), outcome);
                    slot.state.active = None;
                    slot.state.active_group = None;
                    let state = slot.state.clone();
                    self.closing.insert(session_id.to_owned());
                    if let Err(error) = self.persist(
                        session_id.to_owned(),
                        state,
                        Some(PersistenceCompletion::Close),
                    ) {
                        tracing::warn!(session_id, %error, "could not queue review close persistence");
                        self.closing.remove(session_id);
                        self.release_group(session_id);
                    }
                }
            }
        }
    }
}
