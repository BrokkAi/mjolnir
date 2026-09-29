use super::*;

impl HostState {
    /// Forwards, dismisses, or cancels an open review on a surface's request.
    pub(super) fn resolve(
        &mut self,
        session_id: &str,
        resolution: Resolution,
    ) -> Result<(), String> {
        if resolution == Resolution::Cancelled
            && let Some(flag) = self.preparation_cancellation.get(session_id)
        {
            flag.store(true, std::sync::atomic::Ordering::Release);
            release_prompts(session_id);
            return Ok(());
        }
        let (requests, pending_state) = {
            let Some(slot) = self.reviews.get_mut(session_id) else {
                return Err("no review is open for that session".to_owned());
            };
            let requests = match resolution {
                Resolution::Forwarded => {
                    if !slot.driver.can_forward() {
                        return Err("there are no findings to forward".to_owned());
                    }
                    slot.driver.forward(
                        new_command_id("review-forward").map_err(|error| format!("{error:#}"))?,
                    )
                }
                Resolution::Dismissed => {
                    if slot.driver.verdict().is_none() {
                        return Err("the review has not reached a verdict yet".to_owned());
                    }
                    slot.driver.dismiss()
                }
                Resolution::Cancelled => slot.driver.cancel(),
                Resolution::NothingToReview | Resolution::CoverageStarted => {
                    return Err("that is not a resolution a surface can ask for".to_owned());
                }
            };
            if requests.is_empty() {
                return Err("the review could not be resolved that way".to_owned());
            }
            let pending_state = if resolution == Resolution::Forwarded {
                let Some(pending) = slot.driver.pending_forward() else {
                    return Err("the review handoff has no durable findings".to_owned());
                };
                slot.state.pending_forward = Some(pending);
                Some(slot.state.clone())
            } else {
                None
            };
            (requests, pending_state)
        };
        if let Some(state) = pending_state {
            self.awaiting_forward_persistence
                .insert(session_id.to_owned(), requests);
            if let Err(error) = self.persist(
                session_id.to_owned(),
                state,
                Some(PersistenceCompletion::Forward),
            ) {
                self.awaiting_forward_persistence.remove(session_id);
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.driver.forward_failed(format!(
                        "the handoff could not be recorded durably: {error}"
                    ));
                }
                self.publish(session_id);
                return Err(error);
            }
            self.publish(session_id);
            return Ok(());
        }
        self.run(session_id, requests);
        Ok(())
    }

    /// Ends a review that cannot continue. Every failure path is the same: a
    /// verdict the user dismisses, and a baseline that stays where it was, so
    /// the change is reviewed again rather than silently skipped.
    pub(super) fn fail(&mut self, session_id: &str, message: impl Into<String>) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        slot.state.active = None;
        let state = slot.state.clone();
        let requests = slot.driver.request_failed(message);
        // A failed review remains visible so the person can dismiss it, but it
        // no longer owns the turn or has work capable of progressing.
        release_prompts(session_id);
        if let Err(error) = self.persist(session_id.to_owned(), state, None) {
            tracing::warn!(session_id, %error, "could not queue failed review persistence");
        }
        self.run(session_id, requests);
    }

    pub(super) fn run(&mut self, session_id: &str, requests: Vec<ReviewRequest>) {
        for request in requests {
            self.run_one(session_id, request);
        }
        self.publish(session_id);
    }
}
