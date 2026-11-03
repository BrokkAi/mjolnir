use super::*;

impl HostState {
    pub(super) fn prepared(
        &mut self,
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
        prepared: Result<Prepared, StartRefusal>,
    ) {
        let cancelled = matches!(self.preparing.get(&session_id), Some(PreparingOperation::Review { cancelled }) if cancelled.load(std::sync::atomic::Ordering::Acquire));
        let prepared = if cancelled {
            Err(StartRefusal("review preparation cancelled".into()))
        } else {
            prepared
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.preparing.remove(&session_id);
                release_prompts(&session_id);
                if manual || refusal.0 != SUBAGENT_REFUSAL {
                    self.record_notice(&session_id, start_refusal_notice(&refusal.0));
                }
                self.publish(&session_id);
                answer(reply, Err(refusal));
                return;
            }
        };
        if self.reviews.contains_key(&session_id) {
            self.preparing.remove(&session_id);
            release_prompts(&session_id);
            let refusal = StartRefusal("a review is already open".to_owned());
            answer(reply, Err(refusal));
            return;
        }
        self.next_epoch = self.next_epoch.saturating_add(1);
        let epoch = self.next_epoch;
        let mut prepared = prepared;
        if prepared.resume_forward.is_none() {
            prepared.state.child_session_id = None;
        }
        let review_id = match new_command_id("review-group") {
            Ok(id) => id,
            Err(error) => {
                self.preparing.remove(&session_id);
                release_prompts(&session_id);
                answer(
                    reply,
                    Err(StartRefusal(format!(
                        "could not allocate review id: {error:#}"
                    ))),
                );
                return;
            }
        };
        let seed = seed_from_session(
            &prepared.materialized,
            &prepared.state,
            if manual { "manual" } else { "automatic" },
        );
        let (mut driver, mut requests) = if let Some(pending) = prepared.resume_forward.clone() {
            let command_id = pending.command_id.clone();
            let (mut driver, _) = TurnReviewDriver::resume_forward(seed, pending);
            let requests = driver.forward(command_id);
            (driver, requests)
        } else {
            TurnReviewDriver::start(seed, review_id.clone(), prepared.count)
        };
        if let Some(deltas) = prepared.captured.clone() {
            requests = driver.delta_captured(deltas);
        }
        if prepared.resume_forward.is_none() {
            let mut registrations = Vec::new();
            for role in &driver.snapshot().roles {
                let request_key = match new_command_id("reviewer-child") {
                    Ok(key) => key,
                    Err(error) => {
                        self.preparing.remove(&session_id);
                        release_prompts(&session_id);
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
                reviewer: prepared.reviewer.clone(),
                scope: prepared.scope,
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
            let retry_recovery = pending.prepared.resume_forward.is_some();
            self.preparing.remove(&session_id);
            self.publish(&session_id);
            if retry_recovery {
                self.recovery_candidates.insert(session_id.clone());
            }
            release_prompts(&session_id);
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
                let cancelled = matches!(self.preparing.remove(&session_id), Some(PreparingOperation::Review { cancelled }) if cancelled.load(std::sync::atomic::Ordering::Acquire));
                if cancelled && result.is_ok() {
                    let mut state = pending.prepared.state;
                    state.active = None;
                    state.active_group = None;
                    if let Err(error) = self.persist(session_id.clone(), state, None) {
                        tracing::warn!(%session_id, %error, "could not clear cancelled review preparation");
                    }
                    release_prompts(&session_id);
                    self.publish(&session_id);
                    answer(
                        pending.reply,
                        Err(StartRefusal("review preparation cancelled".into())),
                    );
                    return;
                }
                if let Err(error) = result {
                    if pending.prepared.resume_forward.is_some() {
                        self.recovery_candidates.insert(session_id.clone());
                    }
                    release_prompts(&session_id);
                    self.publish(&session_id);
                    answer(
                        pending.reply,
                        Err(StartRefusal(format!(
                            "could not record the active review: {error}"
                        ))),
                    );
                    return;
                }
                let push = (pending.prepared.scope
                    == mj_core::relay::CaptureScope::PushableCommits)
                    .then(|| PushReview {
                        deltas: pending.prepared.captured.clone().unwrap_or_default(),
                        finishing: false,
                        retry_delay: std::time::Duration::from_secs(1),
                    });
                self.reviews.insert(
                    session_id.clone(),
                    ReviewSlot {
                        epoch: pending.epoch,
                        driver: pending.driver,
                        reviewer: pending.prepared.reviewer,
                        state: pending.prepared.state,
                        _background: pending.prepared.background,
                        push,
                        start_reply: pending.reply,
                        pending_outcome: None,
                        queued_outcomes: Vec::new(),
                    },
                );
                self.run(&session_id, pending.requests);
            }
            PersistenceCompletion::Forward => {
                let Some(requests) = self.awaiting_forward_persistence.remove(&session_id) else {
                    return;
                };
                if let Err(error) = result {
                    if let Some(slot) = self.reviews.get_mut(&session_id) {
                        slot.driver.forward_failed(format!(
                            "the handoff could not be recorded durably: {error}"
                        ));
                    }
                    self.publish(&session_id);
                    return;
                }
                self.run(&session_id, requests);
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
                self.reviews.remove(&session_id);
                release_prompts(&session_id);
                if let Some(notice) = notice {
                    self.record_notice(&session_id, notice);
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
