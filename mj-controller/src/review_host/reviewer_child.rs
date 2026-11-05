use super::*;

const HANDBACK_INSTRUCTION: &str = "\n\nReturn your plain-text findings, or exactly `No findings.` when there are none, as the complete text of a single handback call. Do not edit files or push. After calling handback, stop.";

impl HostState {
    pub(super) fn register_reviewer_child(
        &mut self,
        session_id: &str,
        role: String,
        prompt: String,
    ) {
        let Some(slot) = self.reviews.get(session_id) else {
            return;
        };
        let Some(group) = slot.state.active_group.as_ref() else {
            return;
        };
        let Some(registration) = group.registrations.iter().find(|item| item.role == role) else {
            return;
        };
        if registration.child_session_id.is_some() {
            self.launch_reviewer_child(session_id, &role, false);
            return;
        }
        let request_key = registration.request_key.clone();
        let reviewer = group.reviewer.clone();
        // A push review covers one repository when it names a worktree, so
        // its reviewer reads context from that checkout, not the session's.
        let working_directory = match group.driver.deltas.as_slice() {
            [delta] if group.scope == mj_core::relay::CaptureScope::PushableCommits => {
                Some(delta.root.clone())
            }
            _ => None,
        };
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
                    working_directory,
                )
            })
            .await
            .map_err(|error| format!("reviewer registration stopped: {error}"))
            .and_then(|result| result);
            if let Err(error) = events.send(HostEvent::ChildRegistered {
                session_id,
                epoch,
                role,
                result,
            }) {
                // A replacement daemon may already have adopted this durable
                // request key. Only that owner may decide whether to close it.
                tracing::warn!(%error, "reviewer registration finished after host shutdown; startup reconciliation owns the child");
            }
        });
    }

    pub(super) fn child_registered(
        &mut self,
        session_id: String,
        epoch: u64,
        role: String,
        result: Result<String, String>,
    ) {
        let child = match result {
            Ok(child) => child,
            Err(error) => {
                self.role_failed(&session_id, epoch, &role, None, error);
                return;
            }
        };
        let Some(slot) = self.reviews.get_mut(&session_id).filter(|slot| {
            slot.epoch == epoch && matches!(slot.driver.phase(), TurnReviewPhase::Running { .. })
        }) else {
            self.close_child(child);
            return;
        };
        let Some(registration) = slot.state.active_group.as_mut().and_then(|group| {
            group
                .registrations
                .iter_mut()
                .find(|item| item.role == role)
        }) else {
            self.close_child(child);
            return;
        };
        registration.child_session_id = Some(child);
        let state = slot.state.clone();
        if let Err(error) = self.persist(
            session_id.clone(),
            state,
            Some(PersistenceCompletion::LaunchChild { role }),
        ) {
            self.fail(&session_id, error);
            self.close_reviewer_child(&session_id);
        }
        self.publish(&session_id);
    }

    pub(super) fn launch_reviewer_child(&self, session_id: &str, role: &str, fresh: bool) {
        let Some(slot) = self.reviews.get(session_id) else {
            return;
        };
        let Some(child) = slot
            .state
            .active_group
            .as_ref()
            .and_then(|group| group.registrations.iter().find(|item| item.role == role))
            .and_then(|registration| registration.child_session_id.clone())
        else {
            return;
        };
        if !slot
            .driver
            .awaited_commands()
            .iter()
            .any(|(name, _)| name == role)
        {
            return;
        }
        let epoch = slot.epoch;
        let role = role.to_owned();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = environment.start_child(&child).await {
                // A restored child may already be running its worker turn.
                // Startup readiness timing cannot turn that work into a failed review.
                if fresh {
                    tracing::warn!(child, %error, "reviewer child failed to start");
                    let _ = events.send(HostEvent::RoleFailed {
                        session_id,
                        epoch,
                        role,
                        child_session_id: Some(child),
                        reason: format!("the reviewer failed to start: {error}"),
                    });
                } else {
                    tracing::warn!(child, %error, "reviewer child startup reconciliation did not finish");
                }
            }
        });
    }

    pub(super) fn close_reviewer_child(&mut self, session_id: &str) {
        let children: Vec<_> = self
            .reviews
            .get(session_id)
            .and_then(|slot| slot.state.active_group.as_ref())
            .into_iter()
            .flat_map(|group| group.registrations.iter())
            .filter_map(|item| item.child_session_id.clone())
            .collect();
        for child in children {
            self.close_child(child);
        }
    }

    pub(super) fn close_reviewer_role(&mut self, session_id: &str, role: &str) {
        if let Some(child) = self
            .reviews
            .get(session_id)
            .and_then(|slot| slot.state.active_group.as_ref())
            .and_then(|group| group.registrations.iter().find(|item| item.role == role))
            .and_then(|item| item.child_session_id.clone())
        {
            self.close_child(child);
        }
    }

    pub(super) fn close_child(&mut self, child: String) {
        if self
            .child_closure_owners
            .values()
            .any(|(id, _)| id == &child)
        {
            return;
        }
        self.spawn_child_close(child, std::time::Duration::ZERO);
    }

    fn spawn_child_close(&mut self, child: String, delay: std::time::Duration) {
        let environment = self.environment.clone();
        let closing = child.clone();
        let task = self.child_closures.spawn(async move {
            tokio::time::sleep(delay).await;
            environment.close_child(&closing).await
        });
        self.child_closure_owners.insert(task.id(), (child, delay));
    }

    pub(super) fn child_closed(
        &mut self,
        result: Result<(tokio::task::Id, Result<(), String>), tokio::task::JoinError>,
    ) {
        let id = match &result {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        let (child, delay) = self
            .child_closure_owners
            .remove(&id)
            .expect("a supervised child close has an owner");
        let failure = match result {
            Ok((_, Ok(()))) => return,
            Ok((_, Err(error))) => error,
            Err(error) => format!("reviewer cleanup task stopped: {error}"),
        };
        let delay = (delay * 2)
            .max(std::time::Duration::from_secs(1))
            .min(std::time::Duration::from_secs(30));
        tracing::warn!(%child, error = %failure, retry_in_seconds = delay.as_secs(),
            "could not close reviewer child; its borrowed target remains owned and cleanup will retry");
        self.spawn_child_close(child, delay);
    }

    pub(super) fn role_failed(
        &mut self,
        session_id: &str,
        epoch: u64,
        role: &str,
        child: Option<&str>,
        reason: String,
    ) {
        let Some(slot) = self
            .reviews
            .get_mut(session_id)
            .filter(|slot| slot.epoch == epoch)
        else {
            return;
        };
        let requests = slot.driver.role_failed(role, child, reason);
        if let Some(group) = slot.state.active_group.as_mut() {
            group.driver = slot.driver.snapshot();
        }
        let failure = match slot.driver.last_verdict() {
            Some(ReviewVerdict::Failed { reason }) => Some(reason.clone()),
            _ => None,
        };
        if let Some(reason) = &failure {
            answer(slot.start_reply.take(), Err(StartRefusal(reason.clone())));
            slot.state.active = None;
        }
        let state = slot.state.clone();
        if let Err(error) = self.persist(session_id.to_owned(), state, None) {
            self.fail(session_id, error);
            return;
        }
        self.run(session_id, requests);
    }

    pub(super) fn reviewer_finished(
        &mut self,
        session_id: String,
        child_session_id: String,
        answer: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        let Some(slot) = self.reviews.get_mut(&session_id) else {
            let _ = reply.send(Ok(()));
            return;
        };
        let Some(group) = slot.state.active_group.as_ref() else {
            let _ = reply.send(Ok(()));
            return;
        };
        let Some(registration) = group
            .registrations
            .iter()
            .find(|item| item.child_session_id.as_deref() == Some(&child_session_id))
        else {
            let _ = reply.send(Ok(()));
            return;
        };
        if slot.pending_outcome.is_some() {
            slot.queued_outcomes.push(ReviewerHandback {
                child_session_id,
                answer,
                reply,
            });
            return;
        }
        let role = registration.role.clone();
        let command_id = group
            .driver
            .roles
            .iter()
            .find(|item| item.role == role)
            .map(|item| item.command_id.clone());
        let Some(command_id) = command_id else {
            let _ = reply.send(Err("review command is missing".into()));
            return;
        };
        let prior = slot.state.clone();
        let prior_driver = slot.driver.snapshot();
        let requests = match answer {
            Some(answer) => {
                slot.driver
                    .role_turn_completed(&command_id, &child_session_id, &answer)
            }
            None => slot.driver.role_failed(
                &role,
                Some(&child_session_id),
                "the reviewer finished without calling handback",
            ),
        };
        if prior_driver == slot.driver.snapshot() {
            let _ = reply.send(Ok(()));
            return;
        }
        if let Some(group) = slot.state.active_group.as_mut() {
            group.driver = slot.driver.snapshot();
        }
        if matches!(
            slot.driver.last_verdict(),
            Some(ReviewVerdict::Failed { .. })
        ) {
            slot.state.active = None;
        }
        slot.pending_outcome = Some(prior);
        let state = slot.state.clone();
        let epoch = slot.epoch;
        let request = PersistenceRequest::SaveReviewerOutcome {
            session_id: session_id.clone(),
            epoch,
            state: Box::new(state),
            requests,
            reply,
        };
        match self.persistence.as_ref() {
            Some(lane) => {
                if let Err(error) = lane.send(request)
                    && let PersistenceRequest::SaveReviewerOutcome { reply, .. } = error.0
                {
                    let _ = reply.send(Err("review persistence stopped".into()));
                    self.fail(&session_id, "review persistence stopped");
                }
            }
            None => {
                if let PersistenceRequest::SaveReviewerOutcome { reply, .. } = request {
                    let _ = reply.send(Err("review persistence stopped".into()));
                    self.fail(&session_id, "review persistence stopped");
                }
            }
        }
    }

    pub(super) fn reviewer_outcome_saved(
        &mut self,
        session_id: String,
        epoch: u64,
        requests: Vec<ReviewRequest>,
        result: Result<(), String>,
    ) {
        if let Some(slot) = self
            .reviews
            .get_mut(&session_id)
            .filter(|slot| slot.epoch == epoch)
        {
            let prior = slot.pending_outcome.take();
            if result.is_err()
                && let Some(prior) = prior
                && let Some(group) = prior.active_group.as_ref()
                && let Ok(driver) = TurnReviewDriver::restore(group.driver.clone())
            {
                slot.driver = driver;
                slot.state = prior;
            }
            if result.is_ok() {
                self.run(&session_id, requests);
            }
            let queued = self
                .reviews
                .get_mut(&session_id)
                .map(|slot| std::mem::take(&mut slot.queued_outcomes))
                .unwrap_or_default();
            for handback in queued {
                self.reviewer_finished(
                    session_id.clone(),
                    handback.child_session_id,
                    handback.answer,
                    handback.reply,
                );
            }
        }
    }
}
