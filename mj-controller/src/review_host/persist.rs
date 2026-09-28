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
        if let Err(error) = self.persist(
            session_id.clone(),
            state,
            Some(PersistenceCompletion::Open { epoch }),
        ) {
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
            PersistenceCompletion::Checkpoint { epoch, revision } => {
                self.checkpoint_saved(session_id, epoch, revision, result)
            }
            PersistenceCompletion::Open { epoch } => {
                if self
                    .pending_open
                    .get(&session_id)
                    .map(|pending| pending.epoch)
                    != Some(epoch)
                {
                    return;
                }
                let Some(mut pending) = self.pending_open.remove(&session_id) else {
                    return;
                };
                self.preparing.remove(&session_id);
                let cancelled = self
                    .preparation_cancellation
                    .remove(&session_id)
                    .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
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
                let (mut driver, mut requests) =
                    if let Some(pending) = pending.prepared.resume_forward.clone() {
                        let command_id = pending.command_id.clone();
                        let (mut driver, _) = TurnReviewDriver::resume_forward(seed, pending);
                        let requests = driver.forward(command_id);
                        (driver, requests)
                    } else {
                        let (mut driver, requests) = TurnReviewDriver::start(seed);
                        // Preparation already captured the change, so the
                        // review starts from that capture instead of asking
                        // the worker for the same one again.
                        match pending.prepared.captured {
                            Some(deltas) => {
                                let requests = driver.delta_captured(deltas);
                                (driver, requests)
                            }
                            None => (driver, requests),
                        }
                    };
                if cancelled {
                    // Fresh preparation has emitted no effect. Legacy recovered
                    // handoffs may already be accepted and must reconcile their
                    // original command identity with the worker below.
                    if pending.prepared.resume_forward.is_none() {
                        requests = driver.cancel();
                        if !requests.contains(&ReviewRequest::Close) {
                            requests.push(ReviewRequest::Close);
                        }
                        pending.prepared.state.pending_forward = None;
                    }
                    answer(
                        pending.reply.take(),
                        Err(StartRefusal("review preparation cancelled".into())),
                    );
                }
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
                        role_generations: BTreeMap::new(),
                        outbox: Vec::new(),
                        receipts: BTreeMap::new(),
                        running_effects: BTreeSet::new(),
                        delivery_errors: BTreeMap::new(),
                        polling_roles: BTreeSet::new(),
                        reading_dispatches: false,
                        pending_supervisor: None,
                        dispatch_after_completion: false,
                        accepted_dispatches: BTreeSet::new(),
                        pending_dispatch_acks: BTreeSet::new(),
                    },
                );
                if let Some(reply) = pending.reply {
                    self.start_replies.insert(session_id.clone(), reply);
                }
                self.run(&session_id, requests);
                if cancelled && let Some(slot) = self.reviews.get_mut(&session_id) {
                    for receipt in slot.receipts.values_mut() {
                        if receipt.phase == super::durable::ReceiptPhase::Waiting {
                            receipt.phase = super::durable::ReceiptPhase::Cancel;
                        }
                    }
                }
            }
            PersistenceCompletion::Close { epoch } => {
                if self.reviews.get(&session_id).map(|slot| slot.epoch) != Some(epoch)
                    || !self.closing.contains(&session_id)
                {
                    return;
                }
                if let Err(error) = result {
                    self.persistence_errors.insert(session_id.clone(), error);
                    self.publish(&session_id);
                    let events = self.events.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let _ = events
                            .send(HostEvent::RetryClose { session_id, epoch })
                            .await;
                    });
                    return;
                }
                self.closing.remove(&session_id);
                self.persistence_errors.remove(&session_id);
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
