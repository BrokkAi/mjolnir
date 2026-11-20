use super::*;

impl HostState {
    /// Puts one controller-authored line into the session's conversation, so a
    /// resolution is visible on every surface rather than in one UI's notice
    /// bar.
    pub fn record_notice(&self, session_id: &str, text: String) {
        let control = self.control.clone();
        let session_id = session_id.to_owned();
        mj_core::runtime::spawn(async move {
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
    pub fn review_step(
        &mut self,
        key: &ReviewKey,
        action: ReviewerAction,
        into_step: impl FnOnce(Result<ReviewerOutcome, String>) -> ReviewStep + Send + 'static,
    ) {
        let Some(epoch) = self.reviews.get(key).map(|slot| slot.epoch) else {
            return;
        };
        let owner = key.session_id.clone();
        self.spawn_reviewer(key.session_id.clone(), None, action, move |outcome| {
            Some(HostEvent::Step {
                session_id: owner,
                epoch,
                step: into_step(outcome),
            })
        });
    }

    pub fn spawn_reviewer(
        &self,
        session_id: String,
        role: Option<String>,
        action: ReviewerAction,
        into_event: impl FnOnce(Result<ReviewerOutcome, String>) -> Option<HostEvent> + Send + 'static,
    ) {
        let control = self.control.clone();
        let events = self.events.clone();
        mj_core::runtime::spawn(async move {
            let outcome = reviewer_action(&control, &session_id, role, action).await;
            if let Some(event) = into_event(outcome) {
                let _ = events.send(event);
            }
        });
    }

    /// Republishes what surfaces read: one view for each of the session's
    /// open reviews, and a placeholder for each preparation and operation in
    /// flight. Called after every state change, so a snapshot poll and a phone
    /// request see the same reviews.
    pub fn publish(&self, session_id: &str) {
        let mut next: BTreeMap<String, RuntimeReviewView> = BTreeMap::new();
        for (key, slot) in self
            .reviews
            .iter()
            .filter(|(key, _)| key.session_id == session_id)
        {
            let mut view = slot.view(session_id);
            if let Some(group) = &slot.open.active_group {
                for registration in &group.registrations {
                    if let Some(child) = &registration.child_session_id
                        && let Some(session) = self
                            .sessions
                            .get(child)
                            .and_then(|watch| watch.materialized.as_ref())
                    {
                        view.questions.extend(
                            session
                                .pending_elicitations
                                .iter()
                                .filter(|request| !mj_core::acp::is_plan_review_id(&request.id))
                                .map(|request| ReviewerQuestion {
                                    role: child.clone(),
                                    request: request.clone(),
                                }),
                        );
                    }
                }
            }
            next.insert(key.review_id.clone(), view);
        }
        // A placeholder sorts after every review (`~` follows the letters), so
        // a surface that shows one view per session shows a real review first.
        let placeholder = |status: &str| RuntimeReviewView {
            child_session_id: None,
            session_id: session_id.into(),
            review_id: String::new(),
            roots: Vec::new(),
            questions: Vec::new(),
            phase: TurnReviewPhase::LaunchingReviewer,
            roles: Vec::new(),
            status: status.into(),
            verdict: None,
        };
        for (request, _) in self
            .preparations
            .iter()
            .filter(|(_, preparation)| preparation.session_id == session_id)
        {
            next.insert(
                format!("~preparing-{request}"),
                placeholder("Preparing reviewer…"),
            );
        }
        for (operation, _) in self
            .operations
            .iter()
            .filter(|(_, held)| held.session_id == session_id)
        {
            next.insert(
                format!("~operation-{operation}"),
                placeholder("Overriding review…"),
            );
        }
        let mut views = self
            .shared
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = false;
        let stale: Vec<_> = views
            .keys()
            .filter(|(session, id)| session == session_id && !next.contains_key(id))
            .cloned()
            .collect();
        for key in stale {
            views.remove(&key);
            changed = true;
        }
        for (id, view) in next {
            let key = (session_id.to_owned(), id);
            if views.get(&key) != Some(&view) {
                views.insert(key, view);
                changed = true;
            }
        }
        drop(views);
        if changed {
            (self.shared.changed)();
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        // The child index and its durable close intent survive this host.
        // Never hold daemon handoff for a retry or an unavailable worker.
        self.child_closures.shutdown().await;
        self.child_closure_owners.clear();
        // Events that waited for a session's state will not run.
        for (_, waiting) in std::mem::take(&mut self.loading) {
            for event in waiting {
                if let HostEvent::StartPush { reply, .. }
                | HostEvent::OverridePush { reply, .. }
                | HostEvent::SetPushBase { reply, .. } = event
                {
                    let _ = reply.send(Err(StartRefusal("the daemon is shutting down".into())));
                }
            }
        }
        // Worker requests are repeatable user operations. Stop waiting during
        // handoff; the persistence lane still drains each accepted local save.
        for operation in self.operations.values() {
            operation.task.abort();
        }
        for (_, operation) in std::mem::take(&mut self.operations) {
            let Operation {
                task,
                mut result,
                reply,
                ..
            } = operation;
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "push review operation task stopped during shutdown");
            }
            let response = result.try_recv().unwrap_or_else(|_| {
                Err(StartRefusal(
                    "push review operation stopped during shutdown; repeat the request after restart. The worker may already have applied it".into(),
                ))
            });
            let _ = reply.send(response);
        }
        let session_ids = self
            .preparations
            .values()
            .map(|preparation| preparation.session_id.clone())
            .chain(self.reviews.keys().map(|key| key.session_id.clone()))
            .collect::<BTreeSet<_>>();
        // The stored rows to leave behind, composed while the pending reviews
        // are still listed: a review whose marker was queued stays restorable.
        let rows: Vec<_> = session_ids
            .iter()
            .filter_map(|session_id| {
                let interrupted = self
                    .preparations
                    .values()
                    .any(|preparation| preparation.session_id == *session_id);
                let listed = self.reviews.iter().any(|(key, slot)| {
                    key.session_id == *session_id && !slot.leaving_row && !slot.finishing
                });
                if !interrupted && !listed {
                    return None;
                }
                match self.compose(session_id) {
                    Ok(mut state) => {
                        // A review still preparing has no state row of its own
                        // yet. The marker lets the next daemon's startup sweep
                        // report it like any other interrupted review.
                        if interrupted {
                            state.open_reviews.push(OpenReview {
                                active: Some("review-preparing".to_owned()),
                                active_group: None,
                            });
                        }
                        Some((session_id.clone(), state))
                    }
                    Err(error) => {
                        tracing::warn!(session_id, %error, "could not compose review shutdown persistence");
                        None
                    }
                }
            })
            .collect();
        for pending in std::mem::take(&mut self.pending_open).into_values() {
            answer(
                pending.reply,
                Err(StartRefusal("the daemon is shutting down".to_owned())),
            );
        }

        // Take the lane once and use that single sender for every final write.
        // No sender may survive the join below or the receiver can never
        // observe EOF.
        for slot in self.reviews.values_mut() {
            // Recording is recoverable from the saved terminal snapshot.
            // Stop waiting for the worker during daemon shutdown.
            slot.delivery.cancel();
            answer(
                slot.start_reply.take(),
                Err(StartRefusal("the daemon is shutting down".into())),
            );
        }
        let lane = self.persistence.take();
        // A review open now keeps its in-flight marker. The next daemon's
        // startup sweep reads it to tell the person the review was cancelled;
        // clearing it here made the restart silent. The baseline has not
        // advanced, so the next review covers the same changes.
        for (session_id, state) in rows {
            let queued = lane
                .as_ref()
                .ok_or_else(|| "the review persistence lane stopped".to_owned())
                .and_then(|persistence| {
                    persistence
                        .send(PersistenceRequest::Save {
                            session_id: session_id.clone(),
                            state: Box::new(state),
                            completion: None,
                        })
                        .map_err(|_| "the review persistence lane stopped".to_owned())
                });
            if let Err(error) = queued {
                tracing::warn!(session_id, %error, "could not queue review shutdown persistence");
            }
        }
        for preparation in self.preparations.values() {
            preparation.cancel.cancel();
        }
        self.preparations.clear();
        self.closing.clear();
        self.reviews.clear();
        for session_id in &session_ids {
            self.publish(session_id);
        }

        drop(lane);
        match self.persistence_task.take() {
            Some(task) => task
                .await
                .map_err(|error| format!("review persistence lane panicked: {error}")),
            None => Ok(()),
        }
    }
}
