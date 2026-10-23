use super::*;

impl HostState {
    /// Returns true once shutdown has drained persistence and the host loop may
    /// stop.
    pub(super) async fn handle(&mut self, event: HostEvent) -> bool {
        match event {
            HostEvent::View {
                session_id,
                snapshot,
                prompt_driven,
            } => self.observe(session_id, snapshot, prompt_driven).await,
            HostEvent::Retain { live } => self.retain_sessions(&live),
            HostEvent::Start {
                session_id,
                manual,
                reply,
            } => self.begin(session_id, manual, reply),
            HostEvent::Prepared {
                session_id,
                manual,
                reply,
                prepared,
            } => self.prepared(session_id, manual, reply, prepared),
            HostEvent::RecoveryPrepared {
                session_id,
                prepared,
            } => self.recovery_prepared(session_id, prepared),
            HostEvent::StateSaved {
                session_id,
                completion,
                result,
            } => self.state_saved(session_id, completion, result),
            HostEvent::ChildRegistered {
                session_id,
                epoch,
                result,
            } => self.child_registered(session_id, epoch, result),
            HostEvent::ReviewerFinished {
                session_id,
                child_session_id,
                answer,
                reply,
            } => self.reviewer_finished(session_id, child_session_id, answer, reply),
            HostEvent::Resolve {
                session_id,
                resolution,
                reply,
            } => {
                let answer = self.resolve(&session_id, resolution);
                let _ = reply.send(answer);
            }
            HostEvent::Step {
                session_id,
                epoch,
                step,
            } => self.step(session_id, epoch, step),
            HostEvent::Interrupted { interrupted } => {
                for session_id in interrupted {
                    self.recovery_candidates.insert(session_id.clone());
                    if self.sessions.get(&session_id).is_some_and(|watch| {
                        matches!(watch.execution, MaterializedExecutionState::Idle)
                    }) {
                        self.begin_recovery(&session_id);
                    }
                }
            }
            HostEvent::Shutdown { reply } => {
                let result = self.shutdown().await;
                let _ = reply.send(result);
                return true;
            }
        }
        false
    }

    /// Applies one asynchronous result to the review that asked for it.
    pub(super) fn step(&mut self, session_id: String, epoch: u64, step: ReviewStep) {
        // A result from a review that has since been cancelled, resolved, or
        // replaced is not this review's business.
        if self.reviews.get(&session_id).map(|slot| slot.epoch) != Some(epoch) {
            return;
        }
        match step {
            ReviewStep::Failed(error) => {
                if self.reviews.get(&session_id).is_some_and(|slot| {
                    matches!(slot.driver.phase(), TurnReviewPhase::Running { .. })
                }) {
                    self.fail(&session_id, error);
                    self.close_failed_reviewer_after_persistence(&session_id);
                }
            }
            ReviewStep::Delta(result) => {
                let requests = match result {
                    Ok(deltas) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.delta_captured(deltas))
                        .unwrap_or_default(),
                    Err(error) => {
                        self.fail(
                            &session_id,
                            format!("the change could not be captured: {error}"),
                        );
                        return;
                    }
                };
                self.run(&session_id, requests);
            }
            ReviewStep::PrimaryPrompted(result) => {
                let requests = match result {
                    Ok(()) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.forward_succeeded())
                        .unwrap_or_default(),
                    Err(error) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.forward_failed(error))
                        .unwrap_or_default(),
                };
                self.run(&session_id, requests);
            }
        }
    }

    /// Release the retained last-view for every session no longer in the live
    /// set, so a stopped or destroyed session's `MaterializedSession` (its full
    /// transcript) does not linger. The in-flight review state in `reviews`/
    /// `closing` is separate and keeps its own lifetime.
    pub(super) fn retain_sessions(&mut self, live: &std::collections::BTreeSet<String>) {
        self.sessions
            .retain(|session_id, _| live.contains(session_id));
    }
}
