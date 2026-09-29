use super::*;

impl HostState {
    /// Puts one controller-authored line into the session's conversation, so a
    /// resolution is visible on every surface rather than in one UI's notice
    /// bar.
    pub(super) fn record_notice(&self, session_id: &str, text: String) {
        let control = self.control.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let recorded = async {
                let handle = control
                    .session(session_id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                let command_id =
                    new_command_id("turn-review-notice").map_err(|error| format!("{error:#}"))?;
                handle
                    .submit(command_id, RelayCommand::RecordNotice { text })
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"))
            }
            .await;
            // The conversation line is a courtesy; a relay that refuses it has
            // not damaged the review.
            if let Err(error) = recorded {
                tracing::debug!(
                    session_id = %session_id,
                    %error,
                    "could not record a review notice in the conversation"
                );
            }
        });
    }

    /// Runs one reviewer action for the default role and feeds its outcome
    /// back to the review that asked for it.
    pub(super) fn review_step(
        &mut self,
        session_id: &str,
        action: ReviewerAction,
        into_step: impl FnOnce(Result<ReviewerOutcome, String>) -> ReviewStep + Send + 'static,
    ) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let owner = session_id.to_owned();
        self.spawn_reviewer(session_id.to_owned(), None, action, move |outcome| {
            Some(HostEvent::Step {
                session_id: owner,
                epoch,
                step: into_step(outcome),
            })
        });
    }

    pub(super) fn spawn_reviewer(
        &self,
        session_id: String,
        role: Option<String>,
        action: ReviewerAction,
        into_event: impl FnOnce(Result<ReviewerOutcome, String>) -> Option<HostEvent> + Send + 'static,
    ) {
        let control = self.control.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome = reviewer_action(&control, &session_id, role, action).await;
            if let Some(event) = into_event(outcome) {
                let _ = events.send(event);
            }
        });
    }

    /// Republishes what surfaces read. Called after every state change, so a
    /// snapshot poll and a phone request see the same review.
    pub(super) fn publish(&self, session_id: &str) {
        let mut views = self
            .shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = match self.reviews.get(session_id) {
            Some(slot) => {
                let next = slot.view(session_id);
                if views.get(session_id) == Some(&next) {
                    false
                } else {
                    views.insert(session_id.to_owned(), next);
                    true
                }
            }
            None if self.preparing.contains(session_id) => {
                let next = RuntimeReviewView {
                    session_id: session_id.into(),
                    tier: (self.config)().tier,
                    phase: TurnReviewPhase::LaunchingReviewer,
                    roles: Vec::new(),
                    status: "Preparing reviewer…".into(),
                    verdict: None,
                };
                let changed = views.get(session_id) != Some(&next);
                views.insert(session_id.into(), next);
                changed
            }
            None => views.remove(session_id).is_some(),
        };
        drop(views);
        if changed {
            (self.shared.changed)();
        }
    }

    pub(super) async fn shutdown(&mut self) -> Result<(), String> {
        let session_ids = self
            .preparing
            .iter()
            .chain(self.reviews.keys())
            .chain(self.recovery_in_flight.iter())
            .chain(self.recovery_candidates.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        for pending in std::mem::take(&mut self.pending_open).into_values() {
            answer(
                pending.reply,
                Err(StartRefusal("the daemon is shutting down".to_owned())),
            );
        }

        // Take the lane once and use that single sender for every final write.
        // No sender may survive the join below or the receiver can never
        // observe EOF.
        let lane = self.persistence.take();
        for (session_id, slot) in &mut self.reviews {
            slot.state.active = None;
            let queued = lane
                .as_ref()
                .ok_or_else(|| "the review persistence lane stopped".to_owned())
                .and_then(|persistence| {
                    persistence
                        .send(PersistenceRequest::Save {
                            session_id: session_id.clone(),
                            state: Box::new(slot.state.clone()),
                            completion: None,
                        })
                        .map_err(|_| "the review persistence lane stopped".to_owned())
                });
            if let Err(error) = queued {
                tracing::warn!(session_id, %error, "could not queue review shutdown persistence");
            }
        }
        for flag in self.preparation_cancellation.values() {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        self.preparation_cancellation.clear();
        self.preparing.clear();
        self.closing.clear();
        self.reviews.clear();
        for session_id in &session_ids {
            release_prompts(session_id);
            self.publish(session_id);
        }

        let clear_result = match lane {
            Some(lane) => {
                let (reply, cleared) = oneshot::channel();
                let sent = lane
                    .send(PersistenceRequest::ClearActive { reply })
                    .map_err(|_| "the review persistence lane stopped during shutdown".to_owned());
                drop(lane);
                match sent {
                    Ok(()) => cleared.await.map_err(|_| {
                        "the review persistence lane stopped before cleanup".to_owned()
                    })?,
                    Err(error) => Err(error),
                }
            }
            None => Err("the review persistence lane already stopped".to_owned()),
        };
        let task_result = match self.persistence_task.take() {
            Some(task) => task
                .await
                .map_err(|error| format!("review persistence lane panicked: {error}")),
            None => Ok(()),
        };
        clear_result.and(task_result)
    }
}
