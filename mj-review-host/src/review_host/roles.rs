use super::*;

impl HostState {
    pub fn run_one(&mut self, key: &ReviewKey, request: ReviewRequest) {
        match request {
            ReviewRequest::CaptureDelta => {
                self.review_step(
                    key,
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
            } => self.register_reviewer_child(key, role, prompt),
            ReviewRequest::StartBriefWriter {
                command_id: _,
                prompt,
            } => self.register_reviewer_child(
                key,
                mj_core::review::driver::BRIEF_WRITER_ROLE.to_owned(),
                prompt,
            ),
            ReviewRequest::CloseBriefWriter => {
                self.close_reviewer_role(key, mj_core::review::driver::BRIEF_WRITER_ROLE);
            }
            ReviewRequest::NoteBriefSkipped { reason } => {
                self.record_notice(
                    &key.session_id,
                    super::persist::brief_skipped_notice(&reason),
                );
            }
            ReviewRequest::CloseReviewer { role } => {
                self.close_reviewer_role(key, &role);
            }
            ReviewRequest::Close => {
                if self.closing.contains(key) {
                    return;
                }
                if let Some(slot) = self.reviews.get_mut(key) {
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
                    let resolution = match slot.driver.phase() {
                        TurnReviewPhase::Resolved(Resolution::Cancelled) => {
                            mj_store::database::FinishedReviewResolution::Cancelled
                        }
                        _ => match slot.driver.last_verdict() {
                            Some(ReviewVerdict::Failed { .. }) => {
                                mj_store::database::FinishedReviewResolution::Failed
                            }
                            Some(_) => mj_store::database::FinishedReviewResolution::Completed,
                            // A cleared review without a verdict was cancelled.
                            None => mj_store::database::FinishedReviewResolution::Cancelled,
                        },
                    };
                    answer(slot.start_reply.take(), outcome);
                    slot.open.active = None;
                    slot.open.active_group = None;
                    self.closing.insert(key.clone());
                    if let Err(error) = self.persist(
                        &key.session_id,
                        Some(PersistenceCompletion::Close {
                            review_id: key.review_id.clone(),
                            resolution,
                        }),
                    ) {
                        tracing::warn!(session_id = %key.session_id, %error, "could not queue review close persistence");
                        self.closing.remove(key);
                        self.drop_slot(key);
                    }
                }
            }
        }
    }
}
