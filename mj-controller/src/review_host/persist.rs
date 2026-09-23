use super::*;

impl HostState {
    pub(super) fn prepared(
        &mut self,
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
        prepared: Result<Prepared, StartRefusal>,
    ) {
        let cancelled = self
            .preparation_cancellation
            .get(&session_id)
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
        let prepared = if cancelled {
            Err(StartRefusal("review preparation cancelled".into()))
        } else {
            prepared
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.preparation_cancellation.remove(&session_id);
                self.preparing.remove(&session_id);
                release_prompts(&session_id);
                self.record_notice(&session_id, start_refusal_notice(&refusal.0));
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
        prepared.state.active = Some(format!("review-{epoch}"));
        let state = prepared.state.clone();
        self.pending_open.insert(
            session_id.clone(),
            PendingOpen {
                epoch,
                manual,
                reply,
                prepared,
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
            self.preparation_cancellation.remove(&session_id);
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
            PersistenceCompletion::Open => {
                let Some(pending) = self.pending_open.remove(&session_id) else {
                    return;
                };
                self.preparing.remove(&session_id);
                let cancelled = self
                    .preparation_cancellation
                    .remove(&session_id)
                    .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
                if cancelled && result.is_ok() {
                    let mut state = pending.prepared.state;
                    state.active = None;
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
                let seed = seed_from_session(
                    &pending.prepared.materialized,
                    pending.prepared.tier,
                    &pending.prepared.state,
                    if pending.manual {
                        "manual"
                    } else {
                        "automatic"
                    },
                );
                let (driver, requests) =
                    if let Some(pending) = pending.prepared.resume_forward.clone() {
                        let command_id = pending.command_id.clone();
                        let (mut driver, _) = TurnReviewDriver::resume_forward(seed, pending);
                        let requests = driver.forward(command_id);
                        (driver, requests)
                    } else {
                        TurnReviewDriver::start(seed)
                    };
                self.reviews.insert(
                    session_id.clone(),
                    ReviewSlot {
                        epoch: pending.epoch,
                        driver,
                        roles: BTreeMap::new(),
                        reviewer: pending.prepared.reviewer,
                        state: pending.prepared.state,
                        // `start_role` assigns a process-wide generation before
                        // every fresh role. Zero remains the explicit
                        // generation for a role that resumes in place.
                        generation: 0,
                    },
                );
                answer(pending.reply, Ok(()));
                self.run(&session_id, requests);
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
