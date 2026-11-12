use super::*;

impl HostState {
    /// Cancel an open review or its preparation.
    pub(super) fn resolve(
        &mut self,
        session_id: &str,
        resolution: Resolution,
    ) -> Result<(), String> {
        if resolution != Resolution::Cancelled {
            return Err(
                "reviews deliver their verdict automatically; only Cancel is available".into(),
            );
        }
        if resolution == Resolution::Cancelled
            && let Some(PreparingOperation::Review { cancelled, .. }) =
                self.preparing.get(session_id)
        {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            return Ok(());
        }
        let slot = self
            .reviews
            .get_mut(session_id)
            .ok_or("no review is open for that session")?;
        if !slot.delivery.cancel() {
            return Err("approval recording has finished; the verdict is being saved".into());
        }
        self.next_epoch = self.next_epoch.saturating_add(1);
        slot.epoch = self.next_epoch;
        slot.finishing = false;
        let requests = slot.driver.cancel();
        slot.sync_group();
        self.run(session_id, requests);
        Ok(())
    }

    /// Ends a review that cannot continue without advancing its baseline.
    pub(super) fn fail(&mut self, session_id: &str, message: impl Into<String>) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        let message = message.into();
        answer(slot.start_reply.take(), Err(StartRefusal(message.clone())));
        slot.state.active = None;
        let requests = slot.driver.request_failed(message);
        slot.sync_group();
        let state = slot.state.clone();
        if let Err(error) = self.persist(session_id.to_owned(), state, None) {
            tracing::warn!(session_id, %error, "could not queue failed review persistence");
        }
        self.run(session_id, requests);
    }

    pub(super) fn run(&mut self, session_id: &str, requests: Vec<ReviewRequest>) {
        // Verdict delivery can discard the group. Transfer its child ownership
        // to supervised cleanup before that asynchronous delivery starts.
        for request in &requests {
            if let ReviewRequest::CloseReviewer { role } = request {
                self.close_reviewer_role(session_id, role);
            }
        }
        if self.finish_push(session_id, &requests) {
            self.publish(session_id);
            return;
        }
        for request in requests {
            if !matches!(request, ReviewRequest::CloseReviewer { .. }) {
                self.run_one(session_id, request);
            }
        }
        self.publish(session_id);
    }
}

impl HostState {
    /// The host owns verdict delivery; the driver still owns review classification.
    fn finish_push(&mut self, session_id: &str, _requests: &[ReviewRequest]) -> bool {
        let Some(slot) = self.reviews.get_mut(session_id) else {
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
        let deltas = slot.driver.deltas().to_vec();
        let epoch = slot.epoch;
        let review_id = slot.driver.snapshot().review_id;
        let completion = PushCompletion {
            session_id: session_id.to_owned(),
            epoch,
            review_id,
            control: self.control.clone(),
            state: slot.state.clone(),
            deltas,
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
            self.retry_push_delivery(session_id, format!("Before-push review failed: {error}"));
        }
        true
    }

    pub(super) fn retry_push_delivery(&mut self, session_id: &str, error: impl std::fmt::Display) {
        let (delay, epoch) = {
            let Some(slot) = self.reviews.get_mut(session_id) else {
                return;
            };
            slot.finishing = false;
            (slot.take_retry_delay(), slot.epoch)
        };
        tracing::warn!(%session_id, %error, retry_in_seconds = delay.as_secs(), "push review delivery will retry");
        if delay == std::time::Duration::from_secs(1) {
            self.record_notice(session_id, format!("Before-push review verdict delivery failed: {error}; retrying while the push stays blocked."));
        }
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = events.send(HostEvent::RetryPushDelivery { session_id, epoch });
        });
    }
}

pub(super) async fn finish_push_delivery(
    environment: Arc<dyn ReviewEnvironment>,
    completion: PushCompletion,
) -> Result<(Box<TurnReviewState>, ReviewVerdict), String> {
    let PushCompletion {
        session_id: owner,
        review_id,
        control,
        mut state,
        deltas,
        mut verdict,
        delivery,
        ..
    } = completion;
    let cancellation = delivery.cancellation();
    state.push_findings = None;
    if verdict.is_complete()
        && let ReviewVerdict::Findings { synthesis, .. } = &verdict
    {
        state.push_findings = Some(mj_core::storage::PushFindingsRecord {
            review_id: review_id.clone(),
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
    if verdict.is_clean() {
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
            result = push_action_typed(&control, &owner, ReviewerAction::RecordReviewed { trees }) => result,
        };
        if let Err(reason) = recorded {
            if reason
                .downcast_ref::<crate::worker_client::RelayRejected>()
                .is_some_and(|rejected| !rejected.is_retryable())
            {
                verdict = ReviewVerdict::Failed {
                    reason: format!("approval recording failed: {reason:#}"),
                };
            } else {
                return Err(format!("approval recording failed: {reason:#}"));
            }
        } else {
            state.advance_review_bases(
                &deltas
                    .iter()
                    .map(|delta| delta.root.clone())
                    .collect::<Vec<_>>(),
            );
            state.push_findings = None;
        }
    }
    // Worker waits are resumable from the terminal snapshot and must not
    // delay handoff. Only the final durable delivery owns daemon admission.
    let admission = crate::upgrade::activity_unless_draining("push review verdict")
        .map_err(|error| format!("push review delivery deferred for daemon handoff: {error}"))?;
    if !delivery.begin_commit() {
        return Err("approval recording cancelled".into());
    }
    state.active = None;
    state.active_group = None;
    let result = tokio::task::spawn_blocking(move || {
        let _admission = admission;
        for delta in deltas {
            let event = mj_core::mailbox::MailboxEvent {
                key: format!("push-review:{owner}:{review_id}:{}", delta.root.display()),
                source: "review".into(),
                wake: true,
                created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                body: mj_core::mailbox::MailboxEventBody::ReviewVerdict {
                    review_id: review_id.clone(),
                    repository: delta.root,
                    base_commit: delta.base_commit.unwrap_or_default(),
                    head_commit: delta.head_commit.unwrap_or_default(),
                    tree: delta.current_tree,
                    verdict: verdict.clone(),
                },
            };
            environment.enqueue_verdict(&owner, &event)?;
        }
        environment.save_state(&owner, &state)?;
        Ok((Box::new(state), verdict))
    })
    .await
    .map_err(|error| format!("push verdict task stopped: {error}"))
    .and_then(|result| result);
    if result.is_err() {
        delivery.retry();
    }
    result
}

pub(super) async fn push_action(
    control: &SessionManagerControl,
    session: &str,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    push_action_typed(control, session, action)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn push_action_typed(
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
pub(super) struct Delivery(std::sync::Mutex<DeliveryPhase>);

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

    pub(super) fn cancel(&self) -> bool {
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
