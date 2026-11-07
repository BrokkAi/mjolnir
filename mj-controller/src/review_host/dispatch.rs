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
                HostEvent::StartPush { reply, .. }
                | HostEvent::OverridePush { reply, .. }
                | HostEvent::SetPushBase { reply, .. } => {
                    let _ = reply.send(Err(StartRefusal(reason.clone())));
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
                // Reviews that recovery did not restore are missing from the
                // maps, so "no review" cannot be answered truthfully.
                HostEvent::HasOpenReview { reply, .. } => {
                    let _ = reply.send(Err(reason.clone()));
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
                    | HostEvent::SetPushBase { .. }
                    | HostEvent::Resolve { .. }
                    | HostEvent::HasOpenReview { .. }
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
            } => self.observe(session_id, snapshot).await,
            HostEvent::Retain { live } => self.retain_sessions(&live),
            HostEvent::StartPush {
                session_id,
                path,
                reply,
            } => self.begin_push(session_id, path, reply),
            HostEvent::OverridePush {
                session_id,
                overrides,
                reply,
            } => self.override_push(session_id, overrides, reply),
            HostEvent::SetPushBase {
                session_id,
                commit,
                path,
                reply,
            } => self.set_push_base(session_id, commit, path, reply),
            HostEvent::OverrideValidated { session_id, permit } => {
                self.override_validated(&session_id, permit)
            }
            HostEvent::OverrideFinished { session_id } => self.override_finished(&session_id),
            HostEvent::Prepared {
                session_id,
                reply,
                prepared,
            } => self.prepared(session_id, reply, *prepared),
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
            HostEvent::ReviewerEndNotSaved {
                child_session_id,
                reason,
                error,
            } => self.reviewer_end_not_saved(child_session_id, reason, error),
            HostEvent::ReviewerStandingUnread {
                session_id,
                epoch,
                role,
                origin,
                error,
            } => self.reviewer_standing_unread(&session_id, epoch, role, origin, error),
            HostEvent::RetryReviewerStanding {
                session_id,
                epoch,
                role,
                origin,
            } => {
                if self
                    .reviews
                    .get(&session_id)
                    .is_some_and(|slot| slot.epoch == epoch)
                {
                    match origin {
                        StandingRead::Restored => {
                            self.launch_reviewer_child(&session_id, &role, false)
                        }
                        StandingRead::Ended {
                            child_session_id,
                            reason,
                        } => self.child_ended(&child_session_id, reason),
                    }
                }
            }
            HostEvent::RetryPushDelivery { session_id, epoch } => {
                if self
                    .reviews
                    .get(&session_id)
                    .is_some_and(|slot| slot.epoch == epoch && !slot.finishing)
                {
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
            HostEvent::HasOpenReview { session_id, reply } => {
                let _ = reply.send(Ok(self.reviews.contains_key(&session_id)
                    || self.preparing.contains_key(&session_id)
                    || self.pending_open.contains_key(&session_id)));
            }
            HostEvent::CloseChild { child_session_id } => self.close_child(child_session_id),
            HostEvent::ChildEnded {
                child_session_id,
                reason,
            } => self.child_ended(&child_session_id, reason),
            HostEvent::ReviewerFinished {
                session_id,
                child_session_id,
                report,
                reply,
            } => {
                if !self.startup_ready && !self.reviews.contains_key(&session_id) {
                    self.deferred_handbacks.push((
                        session_id,
                        ReviewerHandback {
                            child_session_id,
                            report,
                            reply,
                        },
                    ));
                } else {
                    self.reviewer_finished(session_id, child_session_id, report, reply);
                }
            }
            HostEvent::Resolve {
                session_id,
                resolution,
                reply,
            } => {
                let _ = reply.send(self.resolve(&session_id, resolution));
            }
            HostEvent::Step {
                session_id,
                epoch,
                step,
            } => self.step(session_id, epoch, step),
            HostEvent::Interrupted { interrupted } => {
                for session_id in interrupted {
                    self.record_notice(
                        &session_id,
                        "Review was cancelled when Mjolnir restarted; request review again.".into(),
                    );
                }
            }
            HostEvent::RestoreGroups {
                groups,
                abandoned_children,
            } => {
                for (session_id, state) in groups {
                    if let Err(error) = self.restore_group(session_id, state) {
                        let _ =
                            Box::pin(self.handle(HostEvent::StartupRecoveryFailed(error))).await;
                        return false;
                    }
                }
                for child in abandoned_children {
                    self.close_child(child);
                }
                self.startup_ready = true;
                for (session_id, handback) in std::mem::take(&mut self.deferred_handbacks) {
                    self.reviewer_finished(
                        session_id,
                        handback.child_session_id,
                        handback.report,
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
                        | HostEvent::OverridePush { reply, .. }
                        | HostEvent::SetPushBase { reply, .. } => {
                            let _ = reply.send(Err(StartRefusal(format!(
                                "review startup recovery failed: {error}"
                            ))));
                        }
                        HostEvent::Resolve { reply, .. } => {
                            let _ =
                                reply.send(Err(format!("review startup recovery failed: {error}")));
                        }
                        HostEvent::HasOpenReview { reply, .. } => {
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
                        slot.driver.complete();
                    }
                    self.close_reviewer_child(&session_id);
                    self.run_one(&session_id, ReviewRequest::Close);
                }
                Err(error) => {
                    self.close_reviewer_child(&session_id);
                    self.drop_review_if_owner_gone(
                        &session_id,
                        epoch,
                        Some(ReviewStep::PushRetry(error)),
                    );
                }
            },
            ReviewStep::PushRetry(error) => self.retry_push_delivery(&session_id, error),
            ReviewStep::OwnerGone => {
                let review_id = self
                    .reviews
                    .get(&session_id)
                    .map(|slot| slot.driver.snapshot().review_id)
                    .unwrap_or_default();
                // Nothing to tell the person: the session is destroyed, and
                // its review state went with its record.
                tracing::info!(
                    session_id,
                    review_id,
                    "dropping the review of a session that no longer exists"
                );
                self.close_reviewer_child(&session_id);
                self.reviews.remove(&session_id);
                self.publish(&session_id);
            }
            ReviewStep::Delta(result) => {
                let requests = match result {
                    Ok(deltas) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| {
                            if mj_review::delta::trees_unchanged(&deltas) {
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
        }
    }

    /// The one decision for "the requester is gone", asked after any save of
    /// this review's state fails: the real store refuses every such save once
    /// the session's record is deleted. A record that is definitely gone ends
    /// the review through [`ReviewStep::OwnerGone`]; otherwise, including when
    /// the lookup fails, `otherwise` is applied and the caller's own retry
    /// stands. The lookup runs off the host loop.
    pub(super) fn drop_review_if_owner_gone(
        &self,
        session_id: &str,
        epoch: u64,
        otherwise: Option<ReviewStep>,
    ) {
        let environment = self.environment.clone();
        let events = self.events.clone();
        let owner = session_id.to_owned();
        tokio::spawn(async move {
            let lookup = owner.clone();
            let exists =
                tokio::task::spawn_blocking(move || environment.session_exists(&lookup)).await;
            let step = match exists {
                Ok(Ok(false)) => Some(ReviewStep::OwnerGone),
                Ok(Ok(true)) => otherwise,
                Ok(Err(lookup)) => {
                    tracing::warn!(
                        session_id = %owner,
                        error = %lookup,
                        "could not check whether the review's session still exists; keeping the review"
                    );
                    otherwise
                }
                Err(join) => {
                    tracing::warn!(
                        session_id = %owner,
                        error = %join,
                        "the session lookup task failed; keeping the review"
                    );
                    otherwise
                }
            };
            if let Some(step) = step {
                let _ = events.send(HostEvent::Step {
                    session_id: owner,
                    epoch,
                    step,
                });
            }
        });
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
