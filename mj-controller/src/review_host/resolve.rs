use super::*;

impl HostState {
    /// Forwards, dismisses, or cancels an open review on a surface's request.
    pub(super) fn resolve(
        &mut self,
        session_id: &str,
        resolution: Resolution,
    ) -> Result<(), String> {
        if resolution == Resolution::Cancelled
            && let Some(flag) = self.preparation_cancellation.get(session_id)
        {
            flag.store(true, std::sync::atomic::Ordering::Release);
            release_prompts(session_id);
            return Ok(());
        }
        let (requests, pending_state) = {
            let Some(slot) = self.reviews.get_mut(session_id) else {
                return Err("no review is open for that session".to_owned());
            };
            if slot.push.as_ref().is_some_and(|push| push.finishing) {
                return Err("the review verdict is being delivered".into());
            }
            if slot.push.is_some() && resolution != Resolution::Cancelled {
                return Err("before-push reviews only support Cancel".into());
            }
            let requests = match resolution {
                Resolution::Forwarded => {
                    if !slot.driver.can_forward() {
                        return Err("there are no findings to forward".to_owned());
                    }
                    slot.driver.forward(
                        new_command_id("review-forward").map_err(|error| format!("{error:#}"))?,
                    )
                }
                Resolution::Dismissed => {
                    if slot.driver.verdict().is_none() {
                        return Err("the review has not reached a verdict yet".to_owned());
                    }
                    slot.driver.dismiss()
                }
                Resolution::Cancelled => slot.driver.cancel(),
                Resolution::NothingToReview | Resolution::CoverageStarted => {
                    return Err("that is not a resolution a surface can ask for".to_owned());
                }
            };
            if requests.is_empty() {
                return Err("the review could not be resolved that way".to_owned());
            }
            let pending_state = if resolution == Resolution::Forwarded {
                let Some(pending) = slot.driver.pending_forward() else {
                    return Err("the review handoff has no durable findings".to_owned());
                };
                slot.state.pending_forward = Some(pending);
                Some(slot.state.clone())
            } else {
                None
            };
            (requests, pending_state)
        };
        if let Some(state) = pending_state {
            self.awaiting_forward_persistence
                .insert(session_id.to_owned(), requests);
            if let Err(error) = self.persist(
                session_id.to_owned(),
                state,
                Some(PersistenceCompletion::Forward),
            ) {
                self.awaiting_forward_persistence.remove(session_id);
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.driver.forward_failed(format!(
                        "the handoff could not be recorded durably: {error}"
                    ));
                }
                self.publish(session_id);
                return Err(error);
            }
            self.publish(session_id);
            return Ok(());
        }
        self.run(session_id, requests);
        Ok(())
    }

    /// Ends a review that cannot continue. Every failure path is the same: a
    /// verdict the user dismisses, and a baseline that stays where it was, so
    /// the change is reviewed again rather than silently skipped.
    pub(super) fn fail(&mut self, session_id: &str, message: impl Into<String>) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        slot.state.active = None;
        let state = slot.state.clone();
        let requests = slot.driver.request_failed(message);
        // A failed review remains visible so the person can dismiss it, but it
        // no longer owns the turn or has work capable of progressing.
        release_prompts(session_id);
        if let Err(error) = self.persist(session_id.to_owned(), state, None) {
            tracing::warn!(session_id, %error, "could not queue failed review persistence");
        }
        self.run(session_id, requests);
    }

    pub(super) fn run(&mut self, session_id: &str, requests: Vec<ReviewRequest>) {
        if self.finish_push(session_id, &requests) {
            self.publish(session_id);
            return;
        }
        for request in requests {
            self.run_one(session_id, request);
        }
        self.forward_fresh_findings(session_id);
        self.publish(session_id);
    }

    /// A review someone asked for, automatic or manual, delivers its findings:
    /// there is no case where the agent should not see them, so a findings
    /// verdict is forwarded as soon as it is reached instead of waiting for a
    /// person to press Forward. A clean verdict already closes itself, and a
    /// failed one still waits to be dismissed. Forwarding leaves the verdict
    /// phase, so this runs once per verdict.
    fn forward_fresh_findings(&mut self, session_id: &str) {
        let fresh = self.reviews.get(session_id).is_some_and(|slot| {
            slot.push.is_none()
                && matches!(slot.driver.verdict(), Some(ReviewVerdict::Findings { .. }))
                && slot.driver.can_forward()
        });
        if fresh && let Err(error) = self.resolve(session_id, Resolution::Forwarded) {
            tracing::warn!(session_id, %error, "could not forward the review's findings");
        }
    }
}

impl HostState {
    /// The host owns verdict delivery; the driver still owns review classification.
    fn finish_push(&mut self, session_id: &str, requests: &[ReviewRequest]) -> bool {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return false;
        };
        let Some(push) = slot.push.as_mut() else {
            return false;
        };
        if push.finishing {
            return true;
        }
        let Some(verdict) = slot.driver.last_verdict().cloned() else {
            return false;
        };
        push.finishing = true;
        let deltas = push.deltas.clone();
        let epoch = slot.epoch;
        let review_id = slot
            .command_id
            .clone()
            .unwrap_or_else(|| format!("review-{epoch}"));
        if let ReviewVerdict::Findings {
            synthesis,
            evidence,
        } = &verdict
        {
            slot.state.prior_review = Some(mj_core::review::lanes::PriorReviewContext {
                synthesis: synthesis.clone(),
                evidence: evidence.clone(),
            });
        }
        let reviewed_through_ordinal = requests.iter().find_map(|request| match request {
            ReviewRequest::AdvanceBaseline {
                reviewed_through_ordinal,
                ..
            } => Some(*reviewed_through_ordinal),
            _ => None,
        });
        let completion =
            crate::upgrade::activity("push review verdict").map(|admission| PushCompletion {
                session_id: session_id.to_owned(),
                epoch,
                review_id,
                control: self.control.clone(),
                state: slot.state.clone(),
                deltas,
                verdict,
                _admission: admission,
                reviewed_through_ordinal,
            });
        let queued = completion.and_then(|completion| {
            self.persistence
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("review persistence stopped"))?
                .send(PersistenceRequest::FinishPush(Box::new(completion)))
                .map_err(|_| anyhow::anyhow!("review persistence stopped"))
        });
        if let Err(error) = queued {
            self.record_notice(session_id, format!("Before-push review failed: {error}"));
            self.close_reviewer_child(session_id);
            self.run_one(session_id, ReviewRequest::Close);
        }
        true
    }
}

pub(super) async fn finish_push_delivery(
    environment: Arc<dyn ReviewEnvironment>,
    completion: PushCompletion,
) -> Result<Box<TurnReviewState>, String> {
    let PushCompletion {
        session_id: owner,
        review_id,
        control,
        mut state,
        deltas,
        mut verdict,
        _admission,
        reviewed_through_ordinal,
        ..
    } = completion;
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
        let recorded =
            push_action(&control, &owner, ReviewerAction::RecordReviewed { trees }).await;
        if let Err(reason) = recorded {
            verdict = ReviewVerdict::Failed { reason };
        } else {
            let trees: BTreeMap<_, _> = deltas
                .iter()
                .map(|delta| (delta.root.clone(), delta.current_tree.clone()))
                .collect();
            // A baseline pin failure cannot undo a clean verdict. Report it, keeping
            // the reviewed tree and the controller's authoritative baseline.
            if let Err(error) = push_action(
                &control,
                &owner,
                ReviewerAction::AdvanceBaseline {
                    trees: trees.clone(),
                },
            )
            .await
            {
                tracing::warn!(session_id = %owner, %error, "could not pin the clean push review baseline");
            }
            if let Some(ordinal) = reviewed_through_ordinal {
                state.reviewed_through_ordinal = ordinal;
            }
            state.baselines = trees;
            state.prior_review = None;
        }
    }
    state.active = None;
    tokio::task::spawn_blocking(move || {
        let _admission = _admission;
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
        Ok(Box::new(state))
    })
    .await
    .map_err(|error| format!("push verdict task stopped: {error}"))?
}

async fn push_action(
    control: &SessionManagerControl,
    session: &str,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let handle = control
            .session(session.to_owned())
            .await
            .map_err(|error| format!("{error:#}"))?;
        handle
            .reviewer(action)
            .await
            .map_err(|error| format!("{error:#}"))
    })
    .await
    .map_err(|_| {
        "the worker did not finish recording the push review within 10 seconds".to_owned()
    })?
}
