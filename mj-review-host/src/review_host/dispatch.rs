use super::*;

impl HostState {
    /// Returns true once shutdown has drained persistence and the host loop may
    /// stop.
    pub async fn handle(&mut self, event: HostEvent) -> bool {
        if let Some(reason) = &self.startup_failure {
            // A group restored before recovery failed still runs: its events,
            // and questions about whether it is open, are answered as usual.
            let active_owner = match &event {
                HostEvent::ReviewerFinished { session_id, .. }
                | HostEvent::Resolve { session_id, .. }
                | HostEvent::HasOpenReview { session_id, .. } => Some(session_id),
                _ => None,
            };
            if active_owner.is_some_and(|session_id| self.has_slot(session_id)) {
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
                // A session recovery did not restore is missing from the maps,
                // so "no review" cannot be answered truthfully. (Restored ones
                // were answered above as active owners.)
                HostEvent::HasOpenReview { reply, .. } => {
                    let _ = reply.send(Err(super::OpenReviewUnknown::RecoveryFailed(
                        reason.clone(),
                    )));
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
                    | HostEvent::QuestionTarget { .. }
            )
        {
            self.deferred_admissions.push(event);
            return false;
        }
        // These need the session's stored review state, which the host reads
        // once and then owns. They wait for it.
        let event = match &event {
            HostEvent::StartPush { session_id, .. }
            | HostEvent::OverridePush { session_id, .. }
            | HostEvent::SetPushBase { session_id, .. } => {
                let session_id = session_id.clone();
                match self.when_loaded(&session_id, event) {
                    Some(event) => event,
                    None => return false,
                }
            }
            _ => event,
        };
        match event {
            HostEvent::View {
                session_id,
                snapshot,
                protocol,
            } => self.observe(session_id, snapshot, protocol).await,
            HostEvent::Retain { live } => self.retain_sessions(&live),
            HostEvent::StartPush {
                session_id,
                scope,
                reply,
            } => self.begin_push(session_id, scope, reply),
            HostEvent::OverridePush {
                session_id,
                path,
                overrides,
                reply,
            } => self.override_push(session_id, path, overrides, reply),
            HostEvent::SetPushBase {
                session_id,
                commit,
                path,
                reply,
            } => self.set_push_base(session_id, commit, path, reply),
            HostEvent::OperationRequest {
                operation,
                request,
                reply,
            } => self.operation_request(operation, request, reply),
            HostEvent::OperationFinished { operation } => self.operation_finished(operation),
            HostEvent::Prepared { request, prepared } => self.prepared(request, *prepared),
            HostEvent::SessionLoaded { session_id, result } => {
                self.session_loaded(session_id, result).await
            }
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
                if let Some(key) = self.key_for_epoch(&session_id, epoch) {
                    match origin {
                        StandingRead::Restored => self.launch_reviewer_child(&key, &role, false),
                        StandingRead::Ended {
                            child_session_id,
                            reason,
                        } => self.child_ended(&child_session_id, reason),
                    }
                }
            }
            HostEvent::RetryPushDelivery { session_id, epoch } => {
                if let Some(key) = self.key_for_epoch(&session_id, epoch)
                    && self.reviews.get(&key).is_some_and(|slot| !slot.finishing)
                {
                    let requests = self
                        .reviews
                        .get_mut(&key)
                        .map(|slot| slot.driver.recovery_requests())
                        .unwrap_or_default();
                    self.run(&key, requests);
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
                let target = self
                    .key_for_child(&session_id, &child_session_id)
                    .map(|_| (child_session_id, question_id));
                let _ = reply.send(target);
            }
            // Never deferred: callers hold admissions or deliver other
            // events while they ask, so before the startup sweep finishes a
            // session it has not restored yet answers "not ready".
            HostEvent::HasOpenReview { session_id, reply } => {
                let open = self.has_open_work(&session_id);
                let _ = reply.send(if open || self.startup_ready {
                    Ok(open)
                } else {
                    Err(super::OpenReviewUnknown::NotReady)
                });
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
                if !self.startup_ready && !self.has_slot(&session_id) {
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
                    self.adopt(&session_id, &state);
                    // A review without a group was cut off before it opened;
                    // the sweep reported it and cleared it.
                    for review in state
                        .open_reviews
                        .into_iter()
                        .filter(|review| review.active_group.is_some())
                    {
                        if let Err(error) = self.restore_group(&session_id, review) {
                            let _ = Box::pin(self.handle(HostEvent::StartupRecoveryFailed(error)))
                                .await;
                            return false;
                        }
                    }
                }
                self.sweep_queue = abandoned_children.into();
                for _ in 0..super::reviewer_child::SWEEP_CLOSE_WIDTH {
                    self.close_next_swept_child();
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
                        // A group restored before the failure still runs.
                        HostEvent::HasOpenReview { session_id, reply } => {
                            let _ = reply.send(if self.has_slot(&session_id) {
                                Ok(true)
                            } else {
                                Err(super::OpenReviewUnknown::RecoveryFailed(format!(
                                    "review startup recovery failed: {error}"
                                )))
                            });
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
    pub fn step(&mut self, session_id: String, epoch: u64, step: ReviewStep) {
        // A result from a review that has since been cancelled, resolved, or
        // replaced is not this review's business.
        let Some(key) = self.key_for_epoch(&session_id, epoch) else {
            return;
        };
        match step {
            ReviewStep::PushRecorded(result) => match result {
                Ok(approval) => self.deliver(&key, approval),
                Err(error) => self.delivery_failed(&key, error),
            },
            ReviewStep::PushDelivered(result) => match result {
                Ok(verdict) => {
                    if let Some(slot) = self.reviews.get_mut(&key) {
                        slot.delivery_undo = None;
                        if let ReviewVerdict::Failed { reason } = verdict {
                            slot.driver.request_failed(reason);
                        }
                        slot.driver.complete();
                    }
                    self.close_reviewer_child(&key);
                    self.run_one(&key, ReviewRequest::Close);
                }
                Err(error) => self.delivery_failed(&key, error),
            },
            ReviewStep::PushRetry(error) => self.retry_push_delivery(&key, error),
            ReviewStep::OwnerGone => {
                // Nothing to tell the person: the session is destroyed, and
                // its review state went with its record.
                tracing::info!(
                    session_id,
                    review_id = key.review_id,
                    "dropping the review of a session that no longer exists"
                );
                self.close_reviewer_child(&key);
                self.drop_slot(&key);
                self.publish(&session_id);
            }
            ReviewStep::Delta(result) => {
                let requests = match result {
                    Ok(deltas) => self
                        .reviews
                        .get_mut(&key)
                        .map(|slot| {
                            if mj_review::delta::trees_unchanged(&deltas) {
                                slot.driver.empty_push_delta_captured(deltas)
                            } else {
                                slot.driver.delta_captured(deltas)
                            }
                        })
                        .unwrap_or_default(),
                    Err(error) => {
                        self.fail(&key, format!("the change could not be captured: {error}"));
                        return;
                    }
                };
                if let Some(slot) = self.reviews.get_mut(&key) {
                    slot.sync_group();
                    self.awaiting_capture_persistence
                        .insert(key.clone(), requests);
                    if let Err(error) = self.persist(
                        &session_id,
                        Some(PersistenceCompletion::Captured {
                            review_id: key.review_id.clone(),
                        }),
                    ) {
                        self.awaiting_capture_persistence.remove(&key);
                        self.fail(&key, error);
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
    pub fn drop_review_if_owner_gone(
        &self,
        session_id: &str,
        epoch: u64,
        otherwise: Option<ReviewStep>,
    ) {
        let environment = self.environment.clone();
        let events = self.events.clone();
        let owner = session_id.to_owned();
        mj_core::runtime::spawn(async move {
            let lookup = owner.clone();
            let exists =
                mj_core::runtime::spawn_blocking(move || environment.session_exists(&lookup)).await;
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
    pub fn retain_sessions(&mut self, live: &std::collections::BTreeSet<String>) {
        let removed: BTreeSet<_> = self
            .reviews
            .keys()
            .filter(|key| !live.contains(&key.session_id))
            .map(|key| key.session_id.clone())
            .collect();
        for session_id in removed {
            if let Err(error) = self.resolve(&session_id, Resolution::Cancelled) {
                tracing::debug!(session_id, %error, "review was already committing when its session was removed");
            }
        }
        self.sessions
            .retain(|session_id, _| live.contains(session_id));
        self.release_idle_state(live);
    }
}
