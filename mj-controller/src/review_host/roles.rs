use super::*;

impl HostState {
    pub(super) fn run_one(&mut self, session_id: &str, request: ReviewRequest) {
        match request {
            ReviewRequest::CaptureDelta { baselines } => {
                self.review_step(
                    session_id,
                    ReviewerAction::CaptureDelta { baselines },
                    |outcome| {
                        ReviewStep::Delta(match outcome {
                            Ok(ReviewerOutcome::Delta { repositories }) => Ok(repositories),
                            other => Err(unexpected(other)),
                        })
                    },
                );
            }
            ReviewRequest::StartReviewer { command_id, prompt } => {
                self.register_reviewer_child(session_id, command_id, prompt)
            }
            ReviewRequest::PromptPrimary { command_id, prompt } => {
                // The review's own corrective prompt must not be held by the
                // review's own lock, and by this point the review has
                // resolved, so the lock is already released below.
                self.prompt_primary(session_id, command_id, prompt);
            }
            ReviewRequest::CloseReviewer => {
                if self.reviews.get(session_id).is_some_and(|slot| {
                    matches!(
                        slot.driver.phase(),
                        TurnReviewPhase::Resolved(Resolution::Cancelled)
                    )
                }) {
                    self.close_reviewer_child(session_id);
                }
            }
            ReviewRequest::AdvanceBaseline {
                trees,
                reviewed_through_ordinal,
            } => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.baselines = trees.clone();
                    slot.state.reviewed_through_ordinal = reviewed_through_ordinal;
                    // Clear an accepted handoff in the same durable state
                    // write as its prior-review record and baseline. Until
                    // this point shutdown/restart must retain it for command
                    // id reconciliation.
                    slot.state.pending_forward = None;
                    let state = slot.state.clone();
                    if let Err(error) = self.persist(session_id.to_owned(), state, None) {
                        tracing::warn!(session_id, %error, "could not queue review baseline persistence");
                    }
                }
                let session_id = session_id.to_owned();
                self.spawn_reviewer(
                    session_id.clone(),
                    None,
                    ReviewerAction::AdvanceBaseline { trees },
                    move |outcome| {
                        if let Err(error) = outcome {
                            // The controller's copy is what the next capture is
                            // taken against; the worker-side ref is only a gc
                            // pin, so a failure here costs nothing but the pin.
                            tracing::debug!(
                                session_id = %session_id,
                                %error,
                                "the review baseline ref could not be pinned"
                            );
                        }
                        None
                    },
                );
            }
            ReviewRequest::RecordPriorReview { prior } => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.prior_review = Some(prior);
                }
            }
            ReviewRequest::ClearPriorReview => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.prior_review = None;
                    let state = slot.state.clone();
                    if let Err(error) = self.persist(session_id.to_owned(), state, None) {
                        tracing::warn!(session_id, %error, "could not queue prior review cleanup");
                    }
                }
            }
            ReviewRequest::Close => {
                if self.closing.contains(session_id) {
                    return;
                }
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.active = None;
                    if matches!(
                        slot.driver.phase(),
                        TurnReviewPhase::Resolved(Resolution::Cancelled)
                    ) {
                        // A user explicitly cancelled a rejected handoff, so
                        // discard its retry record along with the held pane.
                        // An in-flight or accepted handoff never reaches this
                        // branch before its durable reconciliation sequence.
                        slot.state.pending_forward = None;
                    }
                    let state = slot.state.clone();
                    self.closing.insert(session_id.to_owned());
                    // The primary handoff has already returned a durable
                    // acceptance before Close can be requested. Releasing now
                    // lets later user prompts queue behind that accepted
                    // corrective turn; no held prompt can overtake it.
                    release_prompts(session_id);
                    if let Err(error) = self.persist(
                        session_id.to_owned(),
                        state,
                        Some(PersistenceCompletion::Close),
                    ) {
                        tracing::warn!(session_id, %error, "could not queue review close persistence");
                        self.closing.remove(session_id);
                        self.reviews.remove(session_id);
                        release_prompts(session_id);
                    }
                }
            }
        }
    }

    /// Sends the review's corrective prompt to the primary agent. The actor
    /// receives a scoped admission that is valid only for this review and
    /// command id; the hold remains until the resulting event acknowledges a
    /// durable relay acceptance.
    pub(super) fn prompt_primary(&mut self, session_id: &str, command_id: String, prompt: String) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let Some(admission) = admit_review_delivery(session_id, epoch, &command_id) else {
            self.step(
                session_id.to_owned(),
                epoch,
                ReviewStep::PrimaryPrompted(Err(
                    "the review handoff admission is no longer valid".to_owned()
                )),
            );
            return;
        };
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let submitted = async {
                let handle = control
                    .session(session_id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                handle
                    .submit_review_delivery(admission, prompt_command(prompt))
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"))
            }
            .await;
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::PrimaryPrompted(submitted),
            });
        });
    }
}
