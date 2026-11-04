use super::*;

impl HostState {
    /// Returns true once shutdown has drained persistence and the host loop may
    /// stop.
    pub(super) async fn handle(&mut self, event: HostEvent) -> bool {
        if let Some(reason) = &self.startup_failure {
            let active_owner = match &event {
                HostEvent::ReviewerFinished { session_id, .. }
                | HostEvent::Resolve { session_id, .. } => Some(session_id),
                _ => None,
            };
            if active_owner.is_some_and(|session_id| self.reviews.contains_key(session_id)) {
                return Box::pin(self.handle_without_startup_failure(event)).await;
            }
            match event {
                HostEvent::StartPush { reply, .. } | HostEvent::OverridePush { reply, .. } => {
                    let _ = reply.send(Err(StartRefusal(reason.clone())));
                    return false;
                }
                HostEvent::Start { reply, .. } => {
                    answer(reply, Err(StartRefusal(reason.clone())));
                    return false;
                }
                HostEvent::ReviewerFinished { reply, .. } | HostEvent::Resolve { reply, .. } => {
                    let _ = reply.send(Err(reason.clone()));
                    return false;
                }
                HostEvent::QuestionTarget { reply, .. } => {
                    let _ = reply.send(None);
                    return false;
                }
                event => return Box::pin(self.handle_without_startup_failure(event)).await,
            }
        }
        self.handle_without_startup_failure(event).await
    }

    async fn handle_without_startup_failure(&mut self, event: HostEvent) -> bool {
        if !self.startup_ready
            && matches!(
                &event,
                HostEvent::StartPush { .. }
                    | HostEvent::OverridePush { .. }
                    | HostEvent::Start { .. }
                    | HostEvent::Resolve { .. }
                    | HostEvent::QuestionTarget { .. }
            )
        {
            self.deferred_admissions.push(event);
            return false;
        }
        match event {
            HostEvent::View {
                session_id,
                snapshot,
                prompt_driven,
            } => self.observe(session_id, snapshot, prompt_driven).await,
            HostEvent::Retain { live } => self.retain_sessions(&live),
            HostEvent::StartPush {
                session_id,
                path,
                reply,
            } => self.begin_push(session_id, path, reply),
            HostEvent::OverridePush {
                session_id,
                reason,
                reply,
            } => self.override_push(session_id, reason, reply),
            HostEvent::OverrideValidated { session_id, permit } => {
                self.override_validated(&session_id, permit)
            }
            HostEvent::OverrideFinished { session_id } => self.override_finished(&session_id),
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
                role,
                result,
            } => self.child_registered(session_id, epoch, role, result),
            HostEvent::RoleFailed {
                session_id,
                epoch,
                role,
                child_session_id,
                reason,
            } => self.role_failed(
                &session_id,
                epoch,
                &role,
                child_session_id.as_deref(),
                reason,
            ),
            HostEvent::RetryPushDelivery { session_id, epoch } => {
                if self.reviews.get(&session_id).is_some_and(|slot| {
                    slot.epoch == epoch && slot.push.as_ref().is_some_and(|push| !push.finishing)
                }) {
                    let requests = self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.recovery_requests())
                        .unwrap_or_default();
                    self.run(&session_id, requests);
                }
            }
            HostEvent::ReviewerOutcomeSaved {
                session_id,
                epoch,
                requests,
                result,
            } => self.reviewer_outcome_saved(session_id, epoch, requests, result),
            HostEvent::QuestionTarget {
                session_id,
                child_session_id,
                question_id,
                reply,
            } => {
                let target =
                    self.reviews
                        .get(&session_id)
                        .and_then(|slot| slot.state.active_group.as_ref())
                        .filter(|group| {
                            group.registrations.iter().any(|item| {
                                item.child_session_id.as_deref() == Some(&child_session_id)
                            })
                        })
                        .map(|_| (child_session_id, question_id));
                let _ = reply.send(target);
            }
            HostEvent::ReviewerFinished {
                session_id,
                child_session_id,
                answer,
                reply,
            } => {
                if !self.startup_ready && !self.reviews.contains_key(&session_id) {
                    self.deferred_handbacks.push((
                        session_id,
                        ReviewerHandback {
                            child_session_id,
                            answer,
                            reply,
                        },
                    ));
                } else {
                    self.reviewer_finished(session_id, child_session_id, answer, reply);
                }
            }
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
            HostEvent::RestoreGroups { groups } => {
                for (session_id, state) in groups {
                    if let Err(error) = self.restore_group(session_id, state) {
                        let _ =
                            Box::pin(self.handle(HostEvent::StartupRecoveryFailed(error))).await;
                        return false;
                    }
                }
                self.startup_ready = true;
                for (session_id, handback) in std::mem::take(&mut self.deferred_handbacks) {
                    self.reviewer_finished(
                        session_id,
                        handback.child_session_id,
                        handback.answer,
                        handback.reply,
                    );
                }
                for event in std::mem::take(&mut self.deferred_admissions) {
                    let _ = Box::pin(self.handle(event)).await;
                }
            }
            HostEvent::StartupRecoveryFailed(error) => {
                self.startup_failure = Some(format!("review startup recovery failed: {error}"));
                self.startup_ready = true;
                for (_, handback) in std::mem::take(&mut self.deferred_handbacks) {
                    let _ = handback
                        .reply
                        .send(Err(format!("review startup recovery failed: {error}")));
                }
                for event in std::mem::take(&mut self.deferred_admissions) {
                    match event {
                        HostEvent::StartPush { reply, .. }
                        | HostEvent::OverridePush { reply, .. } => {
                            let _ = reply.send(Err(StartRefusal(format!(
                                "review startup recovery failed: {error}"
                            ))));
                        }
                        HostEvent::Start { reply, .. } => answer(
                            reply,
                            Err(StartRefusal(format!(
                                "review startup recovery failed: {error}"
                            ))),
                        ),
                        HostEvent::Resolve { reply, .. } => {
                            let _ =
                                reply.send(Err(format!("review startup recovery failed: {error}")));
                        }
                        HostEvent::QuestionTarget { reply, .. } => {
                            let _ = reply.send(None);
                        }
                        _ => unreachable!(),
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
            ReviewStep::PushDelivered(result) => match result {
                Ok(state) => {
                    if let Some(slot) = self.reviews.get_mut(&session_id) {
                        slot.state = *state;
                    }
                    self.close_reviewer_child(&session_id);
                    self.run_one(&session_id, ReviewRequest::Close);
                }
                Err(error) => {
                    self.close_reviewer_child(&session_id);
                    self.retry_push_delivery(&session_id, error);
                }
            },
            ReviewStep::Delta(result) => {
                let requests = match result {
                    Ok(deltas) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| {
                            if slot.push.is_some() && mj_review::delta::trees_unchanged(&deltas) {
                                slot.driver.empty_push_delta_captured(deltas)
                            } else {
                                slot.driver.delta_captured(deltas)
                            }
                        })
                        .unwrap_or_default(),
                    Err(error) => {
                        self.fail(
                            &session_id,
                            format!("the change could not be captured: {error}"),
                        );
                        return;
                    }
                };
                if let Some(slot) = self.reviews.get_mut(&session_id) {
                    if let Some(group) = slot.state.active_group.as_mut() {
                        group.driver = slot.driver.snapshot();
                    }
                    let state = slot.state.clone();
                    self.awaiting_capture_persistence
                        .insert(session_id.clone(), requests);
                    if let Err(error) = self.persist(
                        session_id.clone(),
                        state,
                        Some(PersistenceCompletion::Captured),
                    ) {
                        self.awaiting_capture_persistence.remove(&session_id);
                        self.fail(&session_id, error);
                    }
                }
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
                let state = self.reviews.get_mut(&session_id).map(|slot| {
                    slot.sync_group();
                    slot.state.clone()
                });
                if requests.is_empty()
                    && let Some(state) = state
                    && let Err(error) = self.persist(session_id.clone(), state, None)
                {
                    tracing::warn!(%session_id, %error, "could not save review handoff retry state");
                }
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
