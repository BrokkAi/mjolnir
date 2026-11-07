use super::*;

impl HostState {
    pub(super) fn prepared(
        &mut self,
        session_id: String,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
        prepared: Result<Prepared, StartRefusal>,
    ) {
        let cancelled = matches!(self.preparing.get(&session_id), Some(PreparingOperation::Review { cancelled, .. }) if cancelled.load(std::sync::atomic::Ordering::Acquire));
        let prepared = if cancelled {
            Err(StartRefusal("review preparation cancelled".into()))
        } else {
            prepared
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.preparing.remove(&session_id);
                self.publish(&session_id);
                answer(reply, Err(refusal));
                return;
            }
        };
        if self.reviews.contains_key(&session_id) {
            self.preparing.remove(&session_id);
            let refusal = StartRefusal("a review is already open".to_owned());
            answer(reply, Err(refusal));
            return;
        }
        self.next_epoch = self.next_epoch.saturating_add(1);
        let epoch = self.next_epoch;
        let mut prepared = prepared;
        let review_id = match new_command_id("review-group") {
            Ok(id) => id,
            Err(error) => {
                self.preparing.remove(&session_id);
                answer(
                    reply,
                    Err(StartRefusal(format!(
                        "could not allocate review id: {error:#}"
                    ))),
                );
                return;
            }
        };
        let mut seed = prepared.seed.clone();
        seed.declined = prepared
            .state
            .declined_findings
            .iter()
            .filter(|entry| {
                prepared
                    .captured
                    .iter()
                    .any(|delta| entry.covers(&delta.root))
            })
            .map(|entry| entry.entry.clone())
            .collect();
        let (mut driver, _) = TurnReviewDriver::start(
            seed,
            review_id.clone(),
            std::num::NonZeroUsize::new(prepared.reviewers.len())
                .unwrap_or(std::num::NonZeroUsize::MIN),
        );
        let requests = {
            let deltas = prepared.captured.clone();
            if mj_review::delta::trees_unchanged(&deltas) {
                driver.empty_push_delta_captured(deltas)
            } else {
                driver.delta_captured(deltas)
            }
        };
        {
            let mut registrations = Vec::new();
            for role in &driver.snapshot().roles {
                let request_key = match new_command_id("reviewer-child") {
                    Ok(key) => key,
                    Err(error) => {
                        self.preparing.remove(&session_id);
                        answer(
                            reply,
                            Err(StartRefusal(format!(
                                "could not allocate reviewer request: {error:#}"
                            ))),
                        );
                        return;
                    }
                };
                registrations.push(mj_core::storage::ReviewerRegistration {
                    role: role.role.clone(),
                    request_key,
                    child_session_id: None,
                });
            }
            prepared.state.active_group = Some(mj_core::storage::ActiveReviewGroup {
                driver: driver.snapshot(),
                reviewers: prepared.reviewers.clone(),
                registrations,
            });
        }
        prepared.state.active = Some(review_id);
        let state = prepared.state.clone();
        self.pending_open.insert(
            session_id.clone(),
            PendingOpen {
                epoch,
                reply,
                prepared,
                driver,
                requests,
            },
        );
        if let Err(error) =
            self.persist(session_id.clone(), state, Some(PersistenceCompletion::Open))
        {
            let pending = self
                .pending_open
                .remove(&session_id)
                .expect("pending review was just inserted");
            self.preparing.remove(&session_id);
            self.publish(&session_id);
            answer(
                pending.reply,
                Err(StartRefusal(format!(
                    "could not record the active review: {error}"
                ))),
            );
        }
    }

    pub(super) fn state_saved(
        &mut self,
        session_id: String,
        completion: PersistenceCompletion,
        result: Result<(), String>,
    ) {
        match completion {
            PersistenceCompletion::Captured => {
                let Some(requests) = self.awaiting_capture_persistence.remove(&session_id) else {
                    return;
                };
                if let Err(error) = result {
                    self.fail(&session_id, error);
                } else {
                    self.run(&session_id, requests);
                }
            }
            PersistenceCompletion::LaunchChild { role } => {
                if let Err(error) = result {
                    if let Some(slot) = self.reviews.get(&session_id) {
                        self.role_failed(&session_id, slot.epoch, &role, None, error);
                    }
                    return;
                }
                // Registration is durable. Provisioning and harness readiness
                // continue asynchronously, within the accepted review.
                if let Some(slot) = self.reviews.get_mut(&session_id) {
                    answer(slot.start_reply.take(), Ok(()));
                }
                self.launch_reviewer_child(&session_id, &role, true);
            }
            PersistenceCompletion::Open => {
                let Some(pending) = self.pending_open.remove(&session_id) else {
                    return;
                };
                let cancelled = matches!(self.preparing.remove(&session_id), Some(PreparingOperation::Review { cancelled, .. }) if cancelled.load(std::sync::atomic::Ordering::Acquire));
                if cancelled && result.is_ok() {
                    let mut state = pending.prepared.state;
                    state.active = None;
                    state.active_group = None;
                    if let Err(error) = self.persist(session_id.clone(), state, None) {
                        tracing::warn!(%session_id, %error, "could not clear cancelled review preparation");
                    }
                    self.publish(&session_id);
                    answer(
                        pending.reply,
                        Err(StartRefusal("review preparation cancelled".into())),
                    );
                    return;
                }
                if let Err(error) = result {
                    self.publish(&session_id);
                    answer(
                        pending.reply,
                        Err(StartRefusal(format!(
                            "could not record the active review: {error}"
                        ))),
                    );
                    return;
                }
                self.reviews.insert(
                    session_id.clone(),
                    ReviewSlot {
                        epoch: pending.epoch,
                        driver: pending.driver,
                        reviewers: pending.prepared.reviewers,
                        state: pending.prepared.state,
                        _background: pending.prepared.background,
                        finishing: false,
                        retry_delay: std::time::Duration::from_secs(1),
                        start_reply: pending.reply,
                        pending_outcome: None,
                        queued_outcomes: Vec::new(),
                    },
                );
                if let Some(slot) = self.reviews.get_mut(&session_id)
                    && slot.driver.last_verdict() == Some(&ReviewVerdict::Clean)
                {
                    answer(slot.start_reply.take(), Ok(()));
                }
                self.run(&session_id, pending.requests);
            }
            PersistenceCompletion::Close => {
                self.closing.remove(&session_id);
                if let Err(error) = result {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "could not clear the active review marker"
                    );
                }
                let notice = self.reviews.get(&session_id).and_then(|slot| {
                    resolution_notice(slot.driver.phase(), slot.driver.last_verdict())
                });
                let warnings = self
                    .reviews
                    .get(&session_id)
                    .map(|slot| {
                        if slot.driver.last_verdict() == Some(&ReviewVerdict::Clean) {
                            slot.driver
                                .push_review_warnings()
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        }
                    })
                    .unwrap_or_default();
                self.reviews.remove(&session_id);
                if let Some(notice) = notice {
                    self.record_notice(&session_id, notice);
                }
                for warning in warnings {
                    self.record_notice(&session_id, warning);
                }
                self.publish(&session_id);
            }
        }
    }

    pub(super) fn persist(
        &self,
        session_id: String,
        state: TurnReviewState,
        completion: Option<PersistenceCompletion>,
    ) -> Result<(), String> {
        self.persistence
            .as_ref()
            .ok_or_else(|| "the review persistence lane stopped".to_owned())?
            .send(PersistenceRequest::Save {
                session_id,
                state: Box::new(state),
                completion,
            })
            .map_err(|_| "the review persistence lane stopped".to_owned())
    }
}
