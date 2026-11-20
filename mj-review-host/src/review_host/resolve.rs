use super::*;

impl HostState {
    /// Cancel every review of the session, and every preparation of one.
    pub fn resolve(&mut self, session_id: &str, resolution: Resolution) -> Result<(), String> {
        if resolution != Resolution::Cancelled {
            return Err(
                "reviews deliver their verdict automatically; only Cancel is available".into(),
            );
        }
        let mut cancelled = false;
        for preparation in self
            .preparations
            .values()
            .filter(|preparation| preparation.session_id == session_id)
        {
            preparation.cancel.cancel();
            cancelled = true;
        }
        for (_, pending) in self
            .pending_open
            .iter()
            .filter(|(key, _)| key.session_id == session_id)
        {
            pending.cancel.cancel();
            cancelled = true;
        }
        let mut refusal = None;
        for key in self.keys_of(session_id) {
            match self.cancel_review(&key) {
                Ok(()) => cancelled = true,
                Err(error) => {
                    refusal.get_or_insert(error);
                }
            }
        }
        if cancelled {
            Ok(())
        } else {
            Err(refusal.unwrap_or_else(|| "no review is open for that session".to_owned()))
        }
    }

    fn cancel_review(&mut self, key: &ReviewKey) -> Result<(), String> {
        let slot = self
            .reviews
            .get_mut(key)
            .ok_or("no review is open for that session")?;
        if !slot.delivery.cancel() {
            return Err("approval recording has finished; the verdict is being saved".into());
        }
        self.next_epoch = self.next_epoch.saturating_add(1);
        slot.epoch = self.next_epoch;
        slot.finishing = false;
        let requests = slot.driver.cancel();
        slot.sync_group();
        self.run(key, requests);
        Ok(())
    }

    /// Ends a review that cannot continue without advancing its baseline.
    pub fn fail(&mut self, key: &ReviewKey, message: impl Into<String>) {
        let Some(slot) = self.reviews.get_mut(key) else {
            return;
        };
        let message = message.into();
        answer(slot.start_reply.take(), Err(StartRefusal(message.clone())));
        slot.open.active = None;
        let requests = slot.driver.request_failed(message);
        slot.sync_group();
        if let Err(error) = self.persist(&key.session_id, None) {
            tracing::warn!(session_id = %key.session_id, %error, "could not queue failed review persistence");
        }
        self.run(key, requests);
    }

    pub fn run(&mut self, key: &ReviewKey, requests: Vec<ReviewRequest>) {
        // Verdict delivery can discard the group. Transfer its child ownership
        // to supervised cleanup before that asynchronous delivery starts.
        for request in &requests {
            if let ReviewRequest::CloseReviewer { role } = request {
                self.close_reviewer_role(key, role);
            }
        }
        if self.finish_push(key, &requests) {
            self.publish(&key.session_id);
            return;
        }
        for request in requests {
            if !matches!(request, ReviewRequest::CloseReviewer { .. }) {
                self.run_one(key, request);
            }
        }
        self.publish(&key.session_id);
    }
}

impl HostState {
    /// The host owns verdict delivery; the driver still owns review classification.
    /// Delivery has two steps. The worker records an approved tree in a task;
    /// the host then decides what the session's state becomes and queues the
    /// one durable write that ends the review.
    fn finish_push(&mut self, key: &ReviewKey, _requests: &[ReviewRequest]) -> bool {
        let Some(slot) = self.reviews.get_mut(key) else {
            return false;
        };
        if slot.driver.finished() {
            return false;
        }
        if slot.finishing {
            return true;
        }
        let Some(verdict) = slot.driver.last_verdict().cloned() else {
            return false;
        };
        slot.finishing = true;
        let completion = PushCompletion {
            session_id: key.session_id.clone(),
            epoch: slot.epoch,
            control: self.control.clone(),
            deltas: slot.driver.deltas().to_vec(),
            verdict,
            delivery: slot.delivery.clone(),
        };
        let queued = self
            .persistence
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("review persistence stopped"))
            .and_then(|persistence| {
                persistence
                    .send(PersistenceRequest::FinishPush(Box::new(completion)))
                    .map_err(|_| anyhow::anyhow!("review persistence stopped"))
            });
        if let Err(error) = queued {
            self.retry_push_delivery(key, format!("Before-push review failed: {error}"));
        }
        true
    }

    /// The worker answered for a verdict's approval. Decides what the
    /// session's state becomes and queues the durable write that ends the
    /// review: its verdict messages and the row without it, in one step.
    pub fn deliver(&mut self, key: &ReviewKey, approval: Approval) {
        let Some(slot) = self.reviews.get(key) else {
            return;
        };
        // Worker waits are resumable from the terminal snapshot and must not
        // delay handoff. Only the final durable delivery owns daemon admission.
        let admission = match mj_store::upgrade::activity_unless_draining("push review verdict") {
            Ok(admission) => admission,
            Err(error) => {
                self.delivery_failed(
                    key,
                    format!("push review delivery deferred for daemon handoff: {error}"),
                );
                return;
            }
        };
        if !slot.delivery.begin_commit() {
            self.delivery_failed(key, "approval recording cancelled".into());
            return;
        }
        let Approval { verdict, recorded } = approval;
        let deltas = slot.driver.deltas().to_vec();
        let epoch = slot.epoch;
        let protocol = slot.protocol;
        let delivery = slot.delivery.clone();
        let roots: Vec<_> = deltas.iter().map(|delta| delta.root.clone()).collect();
        let Some(owner) = self.review_state.get_mut(&key.session_id) else {
            self.delivery_failed(key, "the review state is not loaded".into());
            return;
        };
        // The latest verdict for a root replaces the outstanding one. A
        // session whose worker cannot choose between verdicts keeps one.
        if concurrent_reviews(protocol) {
            owner.part.retire_findings(roots.iter());
        } else {
            owner.part.push_findings.clear();
        }
        if verdict.is_complete()
            && let ReviewVerdict::Findings { synthesis, .. } = &verdict
        {
            owner
                .part
                .replace_findings(mj_core::storage::PushFindingsRecord {
                    review_id: key.review_id.clone(),
                    trees: deltas
                        .iter()
                        .map(|delta| {
                            (
                                delta.root.clone(),
                                (
                                    delta.current_tree.clone(),
                                    delta.head_commit.clone().unwrap_or_default(),
                                ),
                            )
                        })
                        .collect(),
                    synthesis: synthesis.clone(),
                });
        }
        if recorded {
            owner.part.advance_review_bases(&roots);
        }
        let resolution = match &verdict {
            ReviewVerdict::Failed { .. } => mj_store::database::FinishedReviewResolution::Failed,
            _ => mj_store::database::FinishedReviewResolution::Completed,
        };
        if let Some(slot) = self.reviews.get_mut(key) {
            slot.leaving_row = true;
        }
        let queued = self.compose(&key.session_id).and_then(|state| {
            self.persistence
                .as_ref()
                .ok_or_else(|| "review persistence stopped".to_owned())?
                .send(PersistenceRequest::Deliver(Box::new(DeliveryCommit {
                    session_id: key.session_id.clone(),
                    epoch,
                    review_id: key.review_id.clone(),
                    state,
                    events: verdict_events(&key.session_id, &key.review_id, deltas, &verdict),
                    verdict,
                    resolution,
                    delivery: delivery.clone(),
                    admission,
                })))
                .map_err(|_| "review persistence stopped".to_owned())
        });
        if let Err(error) = queued {
            delivery.retry();
            if let Some(slot) = self.reviews.get_mut(key) {
                slot.leaving_row = false;
            }
            self.delivery_failed(key, error);
        }
    }

    /// A delivery step did not finish. The review stays open with its group
    /// stored, and delivery tries again, unless the requester is gone.
    pub fn delivery_failed(&mut self, key: &ReviewKey, error: String) {
        let epoch = self.reviews.get_mut(key).map(|slot| {
            slot.leaving_row = false;
            slot.epoch
        });
        self.close_reviewer_child(key);
        if let Some(epoch) = epoch {
            self.drop_review_if_owner_gone(
                &key.session_id,
                epoch,
                Some(ReviewStep::PushRetry(error)),
            );
        }
    }

    pub fn retry_push_delivery(&mut self, key: &ReviewKey, error: impl std::fmt::Display) {
        let (delay, epoch) = {
            let Some(slot) = self.reviews.get_mut(key) else {
                return;
            };
            slot.finishing = false;
            (slot.take_retry_delay(), slot.epoch)
        };
        tracing::warn!(session_id = %key.session_id, %error, retry_in_seconds = delay.as_secs(), "push review delivery will retry");
        if delay == std::time::Duration::from_secs(1) {
            self.record_notice(&key.session_id, format!("Before-push review verdict delivery failed: {error}; retrying while the push stays blocked."));
        }
        let events = self.events.clone();
        let session_id = key.session_id.clone();
        mj_core::runtime::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = events.send(HostEvent::RetryPushDelivery { session_id, epoch });
        });
    }
}

/// The messages that tell the requester what a review decided, one per
/// repository.
fn verdict_events(
    owner: &str,
    review_id: &str,
    deltas: Vec<mj_core::relay::RepoDelta>,
    verdict: &ReviewVerdict,
) -> Vec<mj_core::mailbox::MailboxEvent> {
    deltas
        .into_iter()
        .map(|delta| mj_core::mailbox::MailboxEvent {
            key: format!("push-review:{owner}:{review_id}:{}", delta.root.display()),
            source: "review".into(),
            wake: true,
            created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
            body: mj_core::mailbox::MailboxEventBody::ReviewVerdict {
                review_id: review_id.to_owned(),
                repository: delta.root,
                base_commit: delta.base_commit.unwrap_or_default(),
                head_commit: delta.head_commit.unwrap_or_default(),
                tree: delta.current_tree,
                verdict: verdict.clone(),
            },
        })
        .collect()
}

/// The worker's part of a verdict: record an approved tree as reviewed. A
/// worker that refuses it for good turns the verdict into a failure.
pub async fn record_approval(completion: PushCompletion) -> Result<Approval, String> {
    let PushCompletion {
        session_id: owner,
        control,
        deltas,
        verdict,
        delivery,
        ..
    } = completion;
    let cancellation = delivery.cancellation();
    if !verdict.is_clean() {
        return Ok(Approval {
            verdict,
            recorded: false,
        });
    }
    let trees = deltas
        .iter()
        .map(|delta| {
            (
                delta.root.clone(),
                (
                    delta.current_tree.clone(),
                    delta.head_commit.clone().unwrap_or_default(),
                ),
            )
        })
        .collect();
    let recorded = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err("approval recording cancelled".into()),
        result = push_action(&control, &owner, ReviewerAction::RecordReviewed { trees }) => result,
    };
    match recorded {
        Ok(_) => Ok(Approval {
            verdict,
            recorded: true,
        }),
        Err(reason)
            if reason
                .downcast_ref::<mj_session_runtime::worker_client::RelayRejected>()
                .is_some_and(|rejected| !rejected.is_retryable()) =>
        {
            Ok(Approval {
                verdict: ReviewVerdict::Failed {
                    reason: format!("approval recording failed: {reason:#}"),
                },
                recorded: false,
            })
        }
        Err(reason) => Err(format!("approval recording failed: {reason:#}")),
    }
}

/// The durable end of a verdict, in one admitted step: the messages that
/// carry it, then the row the host composed without this review. The host
/// already took the delivery's commit, so a cancellation can no longer stop it.
pub async fn commit_delivery(
    environment: Arc<dyn ReviewEnvironment>,
    commit: DeliveryCommit,
) -> Result<ReviewVerdict, String> {
    let DeliveryCommit {
        session_id: owner,
        review_id,
        state,
        events,
        verdict,
        resolution,
        delivery,
        admission,
        ..
    } = commit;
    let result = mj_core::runtime::spawn_blocking(move || {
        let _admission = admission;
        for event in &events {
            environment.enqueue_verdict(&owner, event)?;
        }
        environment.close_review(&owner, &review_id, &state, resolution)?;
        Ok(verdict)
    })
    .await
    .map_err(|error| format!("push verdict task stopped: {error}"))
    .and_then(|result| result);
    if result.is_err() {
        delivery.retry();
    }
    result
}

pub async fn push_action(
    control: &SessionManagerControl,
    session: &str,
    action: ReviewerAction,
) -> anyhow::Result<ReviewerOutcome> {
    let started = std::time::Instant::now();
    let handle = control.session(session.to_owned()).await?;
    tracing::info!(
        session_id = session,
        elapsed_ms = started.elapsed().as_millis(),
        "review session lookup completed"
    );
    handle.reviewer(action).await
}

// The transition to committing and user cancellation share one decision point.
// Once committing starts there is no worker wait left to cancel.
#[derive(Default)]
pub struct Delivery(std::sync::Mutex<DeliveryPhase>);

#[derive(Default)]
enum DeliveryPhase {
    #[default]
    Pending,
    Recording(tokio_util::sync::CancellationToken),
    Committing,
    Cancelled,
}

impl Delivery {
    fn cancellation(&self) -> tokio_util::sync::CancellationToken {
        let mut phase = self.0.lock().expect("review delivery poisoned");
        let token = tokio_util::sync::CancellationToken::new();
        if matches!(*phase, DeliveryPhase::Cancelled) {
            token.cancel();
        } else {
            *phase = DeliveryPhase::Recording(token.clone());
        }
        token
    }

    fn begin_commit(&self) -> bool {
        let mut phase = self.0.lock().expect("review delivery poisoned");
        if matches!(*phase, DeliveryPhase::Cancelled) {
            return false;
        }
        *phase = DeliveryPhase::Committing;
        true
    }

    fn retry(&self) {
        let mut phase = self.0.lock().expect("review delivery poisoned");
        if matches!(*phase, DeliveryPhase::Committing) {
            *phase = DeliveryPhase::Pending;
        }
    }

    pub fn cancel(&self) -> bool {
        let mut phase = self.0.lock().expect("review delivery poisoned");
        if matches!(*phase, DeliveryPhase::Committing) {
            return false;
        }
        if let DeliveryPhase::Recording(token) = &*phase {
            token.cancel();
        }
        *phase = DeliveryPhase::Cancelled;
        true
    }
}
