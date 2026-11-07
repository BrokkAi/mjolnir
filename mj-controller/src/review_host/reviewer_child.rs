use super::*;

const HANDBACK_INSTRUCTION: &str = "\n\nReturn your plain-text findings, or exactly `No findings.` when there are none, as the complete text of a single handback call. Do not edit files or push. After calling handback, stop.";

/// First retry delay after a reviewer child close fails.
const CLOSE_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(1);
/// A close that keeps failing is retried this rarely. Every attempt can queue a
/// worker restart behind the target's other lifecycle work (#1456), so a dead
/// child must not be resubmitted every few seconds.
const CLOSE_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(300);

/// What the closer has learned about one child's close: how many attempts in a
/// row failed and the last failure it reported.
#[derive(Default)]
pub(super) struct CloseAttempts {
    failures: u32,
    last_failure: Option<String>,
}

impl CloseAttempts {
    /// Records one failed attempt. Returns the wait before the next attempt and
    /// whether the failure is news (the first one, or a different cause), so a
    /// persistent failure is warned about once rather than at every retry.
    fn failed(&mut self, failure: String) -> (std::time::Duration, bool) {
        self.failures = self.failures.saturating_add(1);
        let report = self.last_failure.as_ref() != Some(&failure);
        self.last_failure = Some(failure);
        (
            crate::recovery::backoff_delay(CLOSE_RETRY_BASE, CLOSE_RETRY_CAP, self.failures),
            report,
        )
    }
}

/// Reads one reviewer child's standing off the host's task.
async fn reviewer_standing(
    environment: &Arc<dyn ReviewEnvironment>,
    child: &str,
) -> Result<ChildStanding, String> {
    let environment = environment.clone();
    let child = child.to_owned();
    tokio::task::spawn_blocking(move || environment.reviewer_child_standing(&child))
        .await
        .map_err(|error| format!("reviewer standing lookup stopped: {error}"))?
}

/// The one rule for a reviewer child whose session has ended: a handback it
/// recorded is its outcome, and only without one does its role fail. Both go
/// to the host as a report, the way a finished turn's does, so the host
/// decides them one at a time. Both are idempotent in the host.
fn settle_ended_reviewer(
    events: &mpsc::UnboundedSender<HostEvent>,
    session_id: String,
    child: String,
    reason: String,
    handback: Option<String>,
) {
    let report = match handback {
        Some(answer) => ReviewerReport::Answer(answer),
        None => ReviewerReport::Failed(reason.clone()),
    };
    // No delegation caller waits for this report, so nobody would retry a
    // save that failed: the host does, through `ReviewerEndNotSaved`.
    let (reply, saved) = oneshot::channel();
    let _ = events.send(HostEvent::ReviewerFinished {
        session_id,
        child_session_id: child.clone(),
        report,
        reply,
    });
    let events = events.clone();
    tokio::spawn(async move {
        if let Ok(Err(error)) = saved.await {
            let _ = events.send(HostEvent::ReviewerEndNotSaved {
                child_session_id: child,
                reason,
                error,
            });
        }
    });
}

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
        let Some(index) = group
            .registrations
            .iter()
            .position(|item| item.role == role)
        else {
            return;
        };
        let Some(reviewer) = group.reviewers.get(index).cloned() else {
            return;
        };
        // A push review covers one repository when it names a worktree, so
        // its reviewer reads context from that checkout, not the session's.
        let working_directory = match group.driver.deltas.as_slice() {
            [delta] => Some(delta.root.clone()),
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
        let Some(command_id) = slot
            .driver
            .awaited_commands()
            .into_iter()
            .find_map(|(name, command_id)| (name == role).then_some(command_id))
        else {
            return;
        };
        let epoch = slot.epoch;
        let role = role.to_owned();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            if !fresh {
                // A restored child may still be running its worker turn, and
                // a start error after a restart can be transient, so only the
                // store's record that its session ended settles the role.
                match reviewer_standing(&environment, &child).await {
                    Ok(standing) => {
                        if let Some(state) = standing.ended.clone() {
                            settle_ended_reviewer(
                                &events,
                                session_id,
                                child,
                                format!(
                                    "the reviewer's session ended before the review was restored ({state})"
                                ),
                                standing.answer_to(&command_id),
                            );
                            return;
                        }
                    }
                    // Starting a child that may have ended, or failing one that
                    // may be live, would each be a guess. The role stays
                    // pending and the read is repeated.
                    Err(error) => {
                        tracing::warn!(child, %error, "could not read a restored reviewer's standing");
                        let _ = events.send(HostEvent::ReviewerStandingUnread {
                            session_id,
                            epoch,
                            role,
                            origin: StandingRead::Restored,
                            error,
                        });
                        return;
                    }
                }
            }
            if let Err(error) = environment.start_child(&child).await {
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

    /// Reads a reviewer's standing again after the delay the review uses for
    /// its other retries, until a read answers. The role is not settled in
    /// between: a failed read says nothing about whether it saved a handback.
    pub(super) fn reviewer_standing_unread(
        &mut self,
        session_id: &str,
        epoch: u64,
        role: String,
        origin: StandingRead,
        error: String,
    ) {
        let Some(slot) = self
            .reviews
            .get_mut(session_id)
            .filter(|slot| slot.epoch == epoch)
        else {
            return;
        };
        let delay = slot.take_retry_delay();
        tracing::warn!(%session_id, role, ?origin, %error, retry_in_seconds = delay.as_secs(), "reviewer's standing will be read again");
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = events.send(HostEvent::RetryReviewerStanding {
                session_id,
                epoch,
                role,
                origin,
            });
        });
    }

    /// An ended reviewer's report did not save. Report it again after the
    /// review's retry delay; the host holds no other record that it ended.
    pub(super) fn reviewer_end_not_saved(&mut self, child: String, reason: String, error: String) {
        let Some(slot) = self.reviews.values_mut().find(|slot| {
            slot.state.active_group.as_ref().is_some_and(|group| {
                group
                    .registrations
                    .iter()
                    .any(|item| item.child_session_id.as_deref() == Some(&child))
            })
        }) else {
            return;
        };
        let delay = slot.take_retry_delay();
        tracing::warn!(child, %error, retry_in_seconds = delay.as_secs(), "an ended reviewer's report did not save; it will be reported again");
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = events.send(HostEvent::ChildEnded {
                child_session_id: child,
                reason,
            });
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
        self.spawn_child_close(child, std::time::Duration::ZERO, CloseAttempts::default());
    }

    fn spawn_child_close(
        &mut self,
        child: String,
        delay: std::time::Duration,
        attempts: CloseAttempts,
    ) {
        let environment = self.environment.clone();
        let closing = child.clone();
        let task = self.child_closures.spawn(async move {
            tokio::time::sleep(delay).await;
            environment.close_child(&closing).await
        });
        self.child_closure_owners
            .insert(task.id(), (child, attempts));
    }

    pub(super) fn child_closed(
        &mut self,
        result: Result<(tokio::task::Id, Result<(), String>), tokio::task::JoinError>,
    ) {
        let id = match &result {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        let (child, mut attempts) = self
            .child_closure_owners
            .remove(&id)
            .expect("a supervised child close has an owner");
        let failure = match result {
            Ok((_, Ok(()))) => {
                if attempts.failures > 0 {
                    tracing::info!(%child, failed_attempts = attempts.failures, "reviewer child closed after earlier failures");
                }
                return;
            }
            Ok((_, Err(error))) => error,
            Err(error) => format!("reviewer cleanup task stopped: {error}"),
        };
        let (delay, report) = attempts.failed(failure.clone());
        if report {
            tracing::warn!(%child, error = %failure, retry_in_seconds = delay.as_secs(),
                "could not close reviewer child; its borrowed target remains owned and cleanup will keep retrying with backoff");
        } else {
            tracing::debug!(%child, error = %failure, retry_in_seconds = delay.as_secs(),
                "reviewer child close failed again");
        }
        self.spawn_child_close(child, delay, attempts);
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
        let prior = slot.driver.snapshot();
        let requests = slot.driver.role_failed(role, child, reason);
        // A role that already has an outcome, or a review already finished,
        // is not failed again, and a repeated failure persists nothing.
        if slot.driver.snapshot() == prior {
            return;
        }
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

    /// A reviewer child's session ended. A handback it recorded is its
    /// outcome; without one its role fails, through [`Self::reviewer_finished`]
    /// like every other reviewer outcome, so the group resolves like any other
    /// reviewer failure.
    pub(super) fn child_ended(&mut self, child: &str, reason: String) {
        let Some((session_id, epoch, role, command_id)) =
            self.reviews.iter().find_map(|(session_id, slot)| {
                let group = slot.state.active_group.as_ref()?;
                let registration = group
                    .registrations
                    .iter()
                    .find(|item| item.child_session_id.as_deref() == Some(child))?;
                let command_id = group
                    .driver
                    .roles
                    .iter()
                    .find(|item| item.role == registration.role)?
                    .command_id
                    .clone();
                Some((
                    session_id.clone(),
                    slot.epoch,
                    registration.role.clone(),
                    command_id,
                ))
            })
        else {
            return;
        };
        // A handback that is waiting behind a saving outcome settles the
        // role, unless its save fails: the end is noted so that save's
        // failure reports it again.
        if let Some(slot) = self.reviews.get_mut(&session_id)
            && slot
                .queued_outcomes
                .iter()
                .any(|queued| queued.child_session_id == child)
        {
            slot.ended_while_queued.insert(child.to_owned(), reason);
            return;
        }
        let environment = self.environment.clone();
        let events = self.events.clone();
        let child = child.to_owned();
        tokio::spawn(async move {
            // A handback the child recorded before it ended is its outcome.
            let handback = match reviewer_standing(&environment, &child).await {
                Ok(standing) => standing.answer_to(&command_id),
                // Failing the role now could discard a handback the child
                // saved, so the role stays pending and the read is repeated.
                Err(error) => {
                    tracing::warn!(child, %error, "could not check an ended reviewer for a handback");
                    let _ = events.send(HostEvent::ReviewerStandingUnread {
                        session_id,
                        epoch,
                        role,
                        origin: StandingRead::Ended {
                            child_session_id: child,
                            reason,
                        },
                        error,
                    });
                    return;
                }
            };
            tracing::info!(child, session_id, role, %reason, found_handback = handback.is_some(), "reviewer child ended");
            settle_ended_reviewer(&events, session_id, child, reason, handback);
        });
    }

    pub(super) fn reviewer_finished(
        &mut self,
        session_id: String,
        child_session_id: String,
        report: ReviewerReport,
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
                report,
                reply,
            });
            return;
        }
        let role = registration.role.clone();
        let ended = slot
            .ended_while_queued
            .remove(&child_session_id)
            .map(|reason| (child_session_id.clone(), reason));
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
        let requests = match report {
            ReviewerReport::Answer(answer) => {
                slot.driver
                    .role_turn_completed(&command_id, &child_session_id, &answer)
            }
            ReviewerReport::Failed(reason) => {
                slot.driver
                    .role_failed(&role, Some(&child_session_id), reason)
            }
        };
        if prior_driver == slot.driver.snapshot() {
            let _ = reply.send(Ok(()));
            return;
        }
        if let Some(group) = slot.state.active_group.as_mut() {
            group.driver = slot.driver.snapshot();
        }
        if let Some(ReviewVerdict::Failed { reason }) = slot.driver.last_verdict() {
            answer(slot.start_reply.take(), Err(StartRefusal(reason.clone())));
            slot.state.active = None;
        }
        slot.pending_outcome = Some(PendingOutcome { prior, ended });
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
            let pending = slot.pending_outcome.take();
            let mut ended = None;
            if let Some(PendingOutcome { prior, ended: end }) = pending
                && let Err(error) = &result
            {
                if let Some(group) = prior.active_group.as_ref()
                    && let Ok(driver) = TurnReviewDriver::restore(group.driver.clone())
                {
                    slot.driver = driver;
                    slot.state = prior;
                }
                ended = end.map(|(child, reason)| (child, reason, error.clone()));
            }
            if result.is_ok() {
                self.run(&session_id, requests);
            } else {
                // The delegation that handed this reviewer's report in may be
                // gone with its session, and the end it superseded is on no
                // other record: settle the role from the standing again.
                if let Some((child, reason, error)) = ended {
                    self.reviewer_end_not_saved(child, reason, error);
                }
                // A destroyed requester can never save this outcome, and no
                // retry would change that.
                self.drop_review_if_owner_gone(&session_id, epoch, None);
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
                    handback.report,
                    handback.reply,
                );
            }
        }
    }
}
