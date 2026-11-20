use super::*;

/// What the requester is told when the reviewers run without a brief.
pub fn brief_skipped_notice(reason: &str) -> String {
    format!("Review started without a relation brief: {reason}")
}

impl HostState {
    /// One preparation finished. This is the one place that decides whether
    /// the review may open: only now are its roots known, and nothing else can
    /// run between this decision and the insert into `pending_open`.
    pub fn prepared(&mut self, request: u64, prepared: Result<Prepared, StartRefusal>) {
        let Some(preparation) = self.preparations.remove(&request) else {
            return;
        };
        let session_id = preparation.session_id.clone();
        // A cancellation that arrives after preparation succeeded still wins:
        // nothing has opened yet. A preparation that failed keeps its own
        // reason, which says more than the cancellation that followed it.
        let prepared = match prepared {
            Ok(_) if preparation.cancel.is_cancelled() => {
                Err(StartRefusal("review preparation cancelled".into()))
            }
            prepared => prepared,
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.publish(&session_id);
                let _ = preparation.reply.send(Err(refusal));
                return;
            }
        };
        // A review of this session closed, or a base moved, since this capture
        // began, and the capture may be older than that. It captures again
        // rather than opening a review of a stale range; each restart follows a
        // real base move, so they end.
        if self.base_generation(&session_id) != preparation.base_generation {
            tracing::info!(
                %session_id,
                "a review base moved while a review was being prepared; capturing again"
            );
            let Preparation {
                session_id,
                scope,
                cancel,
                reply,
                ..
            } = preparation;
            let base_generation = self.base_generation(&session_id);
            self.preparations.insert(
                request,
                Preparation {
                    session_id: session_id.clone(),
                    scope: scope.clone(),
                    cancel: cancel.clone(),
                    base_generation,
                    reply,
                },
            );
            let config = (self.config)(&session_id);
            self.spawn_preparation(request, session_id, scope, cancel, config);
            return;
        }
        let roots: Vec<_> = prepared
            .captured
            .iter()
            .map(|delta| delta.root.clone())
            .collect();
        if let Some(refusal) = self.open_refusal(&session_id, prepared.protocol, &roots) {
            self.publish(&session_id);
            let _ = preparation.reply.send(Err(refusal));
            return;
        }
        let Preparation { cancel, reply, .. } = preparation;
        self.open_review(session_id, prepared, cancel, reply);
    }

    /// Opens a review that may open: allocates it, writes its durable marker,
    /// and holds its roots from here on.
    fn open_review(
        &mut self,
        session_id: String,
        prepared: Prepared,
        cancel: tokio_util::sync::CancellationToken,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        self.next_epoch = self.next_epoch.saturating_add(1);
        let epoch = self.next_epoch;
        let accepted = super::begin::accepted_review(&prepared.captured);
        let fail = |reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
                    what: &str,
                    error: anyhow::Error| {
            let _ = reply.send(Err(StartRefusal(format!("{what}: {error:#}"))));
        };
        let review_id = match new_command_id("review-group") {
            Ok(id) => id,
            Err(error) => {
                self.publish(&session_id);
                fail(reply, "could not allocate review id", error);
                return;
            }
        };
        let mut seed = prepared.seed.clone();
        seed.declined = self
            .review_state
            .get(&session_id)
            .map(|owner| owner.part.declined_findings.as_slice())
            .unwrap_or_default()
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
                driver.delta_captured_with_writer(deltas, prepared.brief_writer.is_some())
            }
        };
        let mut registrations = Vec::new();
        for role in &driver.snapshot().roles {
            match new_command_id("reviewer-child") {
                Ok(request_key) => registrations.push(mj_core::storage::ReviewerRegistration {
                    role: role.role.clone(),
                    request_key,
                    child_session_id: None,
                }),
                Err(error) => {
                    self.publish(&session_id);
                    fail(reply, "could not allocate reviewer request", error);
                    return;
                }
            }
        }
        if prepared.brief_writer.is_some() {
            match new_command_id("reviewer-child") {
                Ok(request_key) => registrations.push(mj_core::storage::ReviewerRegistration {
                    role: mj_core::review::driver::BRIEF_WRITER_ROLE.to_owned(),
                    request_key,
                    child_session_id: None,
                }),
                Err(error) => {
                    self.publish(&session_id);
                    fail(reply, "could not allocate brief writer request", error);
                    return;
                }
            }
        }
        let open = OpenReview {
            active: Some(review_id.clone()),
            active_group: Some(mj_core::storage::ActiveReviewGroup {
                driver: driver.snapshot(),
                reviewers: prepared.reviewers.clone(),
                registrations,
                brief_writer: prepared.brief_writer.clone(),
            }),
        };
        let key = ReviewKey {
            session_id: session_id.clone(),
            review_id: review_id.clone(),
        };
        // The review owns its roots from here, before its marker is durable.
        self.pending_open.insert(
            key.clone(),
            PendingOpen {
                epoch,
                reply: Some(OpenReply { reply, accepted }),
                prepared,
                open,
                cancel,
                driver,
                requests,
            },
        );
        if let Err(error) =
            self.persist(&session_id, Some(PersistenceCompletion::Open { review_id }))
        {
            let pending = self
                .pending_open
                .remove(&key)
                .expect("pending review was just inserted");
            self.publish(&session_id);
            answer(
                pending.reply,
                Err(StartRefusal(format!(
                    "could not record the active review: {error}"
                ))),
            );
        }
    }

    pub fn state_saved(
        &mut self,
        session_id: String,
        completion: PersistenceCompletion,
        result: Result<(), String>,
    ) {
        let key = ReviewKey {
            session_id: session_id.clone(),
            review_id: completion.review_id().to_owned(),
        };
        match completion {
            PersistenceCompletion::Captured { .. } => {
                let Some(requests) = self.awaiting_capture_persistence.remove(&key) else {
                    return;
                };
                if let Err(error) = result {
                    self.fail(&key, error);
                } else {
                    self.run(&key, requests);
                }
            }
            PersistenceCompletion::LaunchChild { role, .. } => {
                if let Err(error) = result {
                    if let Some(slot) = self.reviews.get(&key) {
                        self.role_failed(&session_id, slot.epoch, &role, None, error);
                    }
                    return;
                }
                // Registration is durable. Provisioning and harness readiness
                // continue asynchronously, within the accepted review.
                if let Some(slot) = self.reviews.get_mut(&key) {
                    answer(slot.start_reply.take(), Ok(()));
                }
                self.launch_reviewer_child(&key, &role, true);
            }
            PersistenceCompletion::Open { .. } => {
                let Some(pending) = self.pending_open.remove(&key) else {
                    return;
                };
                if pending.cancel.is_cancelled() && result.is_ok() {
                    // The marker is durable; a save that leaves this review out
                    // clears it.
                    if let Err(error) = self.persist(&session_id, None) {
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
                    // Another review's save queued meanwhile listed this one.
                    // A save that leaves it out takes its marker back.
                    if let Err(repair) = self.persist(&session_id, None) {
                        tracing::warn!(%session_id, error = %repair, "could not clear a review that failed to open");
                    }
                    self.publish(&session_id);
                    answer(
                        pending.reply,
                        Err(StartRefusal(format!(
                            "could not record the active review: {error}"
                        ))),
                    );
                    return;
                }
                let brief_skipped = pending.prepared.brief_skipped;
                self.reviews.insert(
                    key.clone(),
                    ReviewSlot {
                        epoch: pending.epoch,
                        driver: pending.driver,
                        reviewers: pending.prepared.reviewers,
                        open: pending.open,
                        protocol: pending.prepared.protocol,
                        _background: pending.prepared.background,
                        finishing: false,
                        leaving_row: false,
                        delivery: Arc::new(super::resolve::Delivery::default()),
                        retry_delay: std::time::Duration::from_secs(1),
                        start_reply: pending.reply,
                        pending_outcome: None,
                        queued_outcomes: Vec::new(),
                        ended_while_queued: BTreeMap::new(),
                    },
                );
                if let Some(slot) = self.reviews.get_mut(&key)
                    && slot.driver.last_verdict() == Some(&ReviewVerdict::Clean)
                {
                    answer(slot.start_reply.take(), Ok(()));
                }
                if let Some(reason) = brief_skipped {
                    self.record_notice(&session_id, brief_skipped_notice(&reason));
                }
                self.run(&key, pending.requests);
            }
            PersistenceCompletion::Close { .. } => {
                self.closing.remove(&key);
                if let Err(error) = result {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "could not clear the active review marker"
                    );
                }
                let notice = self.reviews.get(&key).and_then(|slot| {
                    resolution_notice(slot.driver.phase(), slot.driver.last_verdict())
                });
                let warnings = self
                    .reviews
                    .get(&key)
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
                self.drop_slot(&key);
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
}
