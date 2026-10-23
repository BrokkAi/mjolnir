use super::*;

const HANDBACK_INSTRUCTION: &str = "\n\nReturn the rubric's JSON result as the complete text of a single handback call. Do not edit files or push. After calling handback, stop.";

impl HostState {
    pub(super) fn register_reviewer_child(
        &mut self,
        session_id: &str,
        command_id: String,
        prompt: String,
    ) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        slot.command_id = Some(command_id);
        let request_key = match new_command_id("reviewer-child") {
            Ok(key) => key,
            Err(error) => {
                self.fail(
                    session_id,
                    format!("could not allocate reviewer request: {error:#}"),
                );
                return;
            }
        };
        let reviewer = slot.reviewer.clone();
        let epoch = slot.epoch;
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let owner = session_id.clone();
            let register = environment.clone();
            let result = tokio::task::spawn_blocking(move || {
                register.register_child(
                    &owner,
                    &reviewer,
                    format!("{prompt}{HANDBACK_INSTRUCTION}"),
                    request_key,
                )
            })
            .await
            .map_err(|error| format!("reviewer registration stopped: {error}"))
            .and_then(|result| result);
            if let Err(error) = events.send(HostEvent::ChildRegistered {
                session_id,
                epoch,
                result,
            }) && let HostEvent::ChildRegistered {
                result: Ok(child), ..
            } = error.0
                && let Err(error) = environment.close_child(&child).await
            {
                tracing::warn!(child, %error, "could not close a reviewer registered during shutdown");
            }
        });
    }

    pub(super) fn child_registered(
        &mut self,
        session_id: String,
        epoch: u64,
        result: Result<String, String>,
    ) {
        let child = match result {
            Ok(child) => child,
            Err(error) => {
                if self
                    .reviews
                    .get(&session_id)
                    .is_some_and(|slot| slot.epoch == epoch)
                {
                    self.fail(&session_id, error);
                }
                return;
            }
        };
        let Some(slot) = self.reviews.get_mut(&session_id).filter(|slot| {
            slot.epoch == epoch && matches!(slot.driver.phase(), TurnReviewPhase::Running { .. })
        }) else {
            self.close_child(child);
            return;
        };
        slot.state.child_session_id = Some(child);
        let state = slot.state.clone();
        if let Err(error) = self.persist(
            session_id.clone(),
            state,
            Some(PersistenceCompletion::LaunchChild),
        ) {
            self.fail(&session_id, error);
            self.close_reviewer_child(&session_id);
        }
        self.publish(&session_id);
    }

    pub(super) fn launch_reviewer_child(&self, session_id: &str) {
        let Some(slot) = self.reviews.get(session_id) else {
            return;
        };
        let Some(child) = slot.state.child_session_id.clone() else {
            return;
        };
        if !matches!(slot.driver.phase(), TurnReviewPhase::Running { .. }) {
            self.close_child(child);
            return;
        }
        let epoch = slot.epoch;
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = environment.start_child(&child).await {
                let delivered = events.send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::Failed(format!("the reviewer failed to start: {error}")),
                });
                tracing::warn!(child, %error, "reviewer child failed to start");
                if delivered.is_err()
                    && let Err(error) = environment.close_child(&child).await
                {
                    tracing::warn!(child, %error, "could not close failed reviewer after host shutdown");
                }
            }
        });
    }

    pub(super) fn close_failed_reviewer_after_persistence(&self, session_id: &str) {
        if let Some(child) = self
            .reviews
            .get(session_id)
            .and_then(|slot| slot.state.child_session_id.clone())
            && let Some(persistence) = &self.persistence
            && let Err(error) = persistence.send(PersistenceRequest::CloseFailedChild {
                session_id: session_id.to_owned(),
                child,
            })
        {
            tracing::warn!(%session_id, %error, "could not queue failed reviewer cleanup");
        }
    }

    pub(super) fn close_reviewer_child(&self, session_id: &str) {
        if let Some(child) = self
            .reviews
            .get(session_id)
            .and_then(|slot| slot.state.child_session_id.clone())
        {
            self.close_child(child);
        }
    }

    fn close_child(&self, child: String) {
        let environment = self.environment.clone();
        tokio::spawn(async move {
            if let Err(error) = environment.close_child(&child).await {
                tracing::warn!(child, %error, "could not close reviewer child");
            }
        });
    }

    pub(super) fn reviewer_finished(
        &mut self,
        session_id: String,
        child_session_id: String,
        answer: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        let Some(slot) = self
            .reviews
            .get_mut(&session_id)
            .filter(|slot| slot.state.child_session_id.as_deref() == Some(&child_session_id))
        else {
            let _ = reply.send(Ok(()));
            return;
        };
        if matches!(slot.driver.phase(), TurnReviewPhase::Running { .. }) {
            let requests = match answer {
                Some(answer) => slot.driver.role_turn_completed(
                    slot.command_id
                        .as_deref()
                        .expect("registered reviewer has a command"),
                    &answer,
                ),
                None => {
                    self.fail(
                        &session_id,
                        "the reviewer finished without calling handback",
                    );
                    Vec::new()
                }
            };
            self.run(&session_id, requests);
        }
        // All verdict writes precede this acknowledgement in the persistence lane.
        if let Some(persistence) = &self.persistence {
            if let Err(error) = persistence.send(PersistenceRequest::AcknowledgeReviewer {
                session_id: session_id.clone(),
                reply,
            }) && let PersistenceRequest::AcknowledgeReviewer { reply, .. } = error.0
            {
                let _ = reply.send(Err("review persistence stopped".to_owned()));
            }
        } else {
            let _ = reply.send(Err("review persistence stopped".to_owned()));
        }
    }
}
