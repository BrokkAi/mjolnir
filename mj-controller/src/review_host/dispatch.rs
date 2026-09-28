use super::*;

impl HostState {
    /// Returns true once shutdown has drained persistence and the host loop may
    /// stop.
    pub(super) async fn handle(&mut self, event: HostEvent) -> bool {
        match event {
            HostEvent::View {
                session_id,
                snapshot,
                prompt_driven,
                finished_turn,
            } => {
                self.observe(session_id, snapshot, prompt_driven, finished_turn)
                    .await
            }
            HostEvent::Retain { live } => self.retain_sessions(&live),
            HostEvent::Start {
                session_id,
                manual,
                reply,
            } => self.begin(session_id, manual, reply),
            HostEvent::Prepared {
                session_id,
                manual,
                reply,
                prepared,
            } => self.prepared(session_id, manual, reply, prepared),
            HostEvent::RetryRecovery { session_id } => {
                self.recovery_in_flight.remove(&session_id);
                self.begin_recovery(&session_id);
            }
            HostEvent::RecoveryPrepared {
                session_id,
                prepared,
            } => self.recovery_prepared(session_id, prepared),
            HostEvent::RetryCheckpoint {
                session_id,
                epoch,
                revision,
            } => self.retry_checkpoint(session_id, epoch, revision),
            HostEvent::RetryClose { session_id, epoch } => {
                if self.reviews.get(&session_id).map(|slot| slot.epoch) == Some(epoch)
                    && self.closing.remove(&session_id)
                {
                    if let Some(slot) = self.reviews.get_mut(&session_id) {
                        slot.running_effects.remove("close");
                    }
                    self.dirty.insert(session_id);
                }
            }
            HostEvent::StateSaved {
                session_id,
                completion,
                result,
            } => self.state_saved(session_id, completion, result),
            HostEvent::Resolve {
                session_id,
                resolution,
                reply,
            } => {
                let answer = self.resolve(&session_id, resolution);
                if answer.is_ok() && self.dirty.contains(&session_id) {
                    self.resolve_replies.insert(session_id, reply);
                } else {
                    let _ = reply.send(answer);
                }
            }
            HostEvent::Step {
                session_id,
                epoch,
                step,
            } => self.step(session_id, epoch, step),
            HostEvent::Initialized { result } => match result {
                Ok(interrupted) => {
                    for session_id in interrupted {
                        hold_prompts(&session_id);
                        self.recovery_candidates.insert(session_id.clone());
                        if self.sessions.contains_key(&session_id) {
                            self.begin_recovery(&session_id);
                        }
                    }
                    self.shared.initialized.send_replace(Some(Ok(())));
                }
                Err(error) => {
                    self.shared.initialized.send_replace(Some(Err(error)));
                }
            },
            HostEvent::Shutdown { reply } => {
                let result = self.shutdown().await;
                let _ = reply.send(result);
                return true;
            }
        }
        false
    }

    /// Applies one asynchronous result to the review that asked for it.
    pub(super) fn step(&mut self, session_id: String, epoch: u64, step: ReviewStep) {
        // A result from a review that has since been cancelled, resolved, or
        // replaced is not this review's business.
        if self.closing.contains(&session_id)
            || self.reviews.get(&session_id).map(|slot| slot.epoch) != Some(epoch)
        {
            return;
        }
        if let ReviewStep::RolePrompted {
            role,
            result: Err(error),
        } = &step
            && self.reviews.get(&session_id).is_some_and(|slot| {
                slot.driver
                    .awaited_commands()
                    .iter()
                    .any(|(awaited_role, _)| awaited_role == role)
            })
        {
            self.retry_effect(&session_id, format!("prompt:{role}"), error.clone());
            return;
        }
        if let ReviewStep::PrimaryPrompted(Err(error)) = &step {
            let receipt = self.reviews.get(&session_id).and_then(|slot| {
                slot.receipts
                    .values()
                    .find(|receipt| receipt.role.is_none())
            });
            if let Some(receipt) = receipt {
                if receipt.phase == super::durable::ReceiptPhase::Waiting {
                    self.retry_effect(&session_id, "primary".into(), error.clone());
                }
                return;
            }
        }
        self.settle_effect(&session_id, &step);
        match step {
            ReviewStep::ReceiptSettled {
                command_id,
                phase,
                result,
            } => {
                let Some(slot) = self.reviews.get_mut(&session_id) else {
                    return;
                };
                slot.running_effects
                    .remove(&format!("receipt:{command_id}"));
                let Some(receipt) = slot
                    .receipts
                    .get(&command_id)
                    .cloned()
                    .filter(|receipt| receipt.phase == phase)
                else {
                    return;
                };
                let mut requests = Vec::new();
                match result {
                    Err(error) => {
                        slot.delivery_errors
                            .insert(format!("receipt:{command_id}"), error);
                    }
                    Ok(accepted) => {
                        slot.delivery_errors
                            .remove(&format!("receipt:{command_id}"));
                        if phase == super::durable::ReceiptPhase::Release {
                            slot.receipts.remove(&command_id);
                        } else {
                            slot.outbox.retain(|request| !matches!(request, ReviewRequest::PromptRole { command_id: id, .. } | ReviewRequest::PromptPrimary { command_id: id, .. } if id == &command_id));
                            if accepted.is_some() {
                                slot.receipts
                                    .get_mut(&command_id)
                                    .expect("receipt exists")
                                    .phase = super::durable::ReceiptPhase::Release;
                            } else {
                                slot.receipts.remove(&command_id);
                            }
                            if receipt.role.is_none() {
                                if accepted
                                    .as_ref()
                                    .is_some_and(|receipt| receipt.failure.is_none())
                                {
                                    requests = slot.driver.forward_succeeded();
                                } else {
                                    slot.driver.forward_failed(
                                        "corrective prompt was cancelled before acceptance",
                                    );
                                    requests = slot.driver.cancel();
                                    slot.state.pending_forward = None;
                                }
                            }
                        }
                    }
                }
                self.run(&session_id, requests);
            }
            ReviewStep::RetryEffect { key } => {
                if let Some(slot) = self.reviews.get_mut(&session_id) {
                    slot.running_effects.remove(&key);
                    let awaited = slot.driver.awaited_commands();
                    slot.outbox.retain(|request| {
                        if super::durable::effect_key(request) != key {
                            return true;
                        }
                        match request {
                            ReviewRequest::PromptRole {
                                role, command_id, ..
                            } => {
                                let keep = awaited
                                    .iter()
                                    .any(|(owner, command)| owner == role && command == command_id);
                                if !keep
                                    && let Some(receipt) = slot.receipts.get_mut(command_id)
                                    && receipt.phase == super::durable::ReceiptPhase::Waiting
                                {
                                    receipt.phase = super::durable::ReceiptPhase::Cancel;
                                }
                                keep
                            }
                            _ => true,
                        }
                    });
                    self.dirty.insert(session_id);
                }
            }
            ReviewStep::EffectSettled { key, result } => {
                if let Some(slot) = self.reviews.get_mut(&session_id) {
                    slot.running_effects.remove(&key);
                    match result {
                        Ok(()) => slot
                            .outbox
                            .retain(|request| super::durable::effect_key(request) != key),
                        Err(error) => {
                            tracing::warn!(%session_id, %key, %error, "review cleanup will be retried")
                        }
                    }
                    self.dirty.insert(session_id);
                }
            }
            ReviewStep::Delta(result) => {
                let requests = match result {
                    Ok(deltas) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.delta_captured(deltas))
                        .unwrap_or_default(),
                    Err(error) => {
                        self.fail(
                            &session_id,
                            format!("the change could not be captured: {error}"),
                        );
                        return;
                    }
                };
                self.run(&session_id, requests);
            }
            ReviewStep::Analysis(result) => {
                let requests = self
                    .reviews
                    .get_mut(&session_id)
                    .map(|slot| slot.driver.analysis_completed(result))
                    .unwrap_or_default();
                self.run(&session_id, requests);
            }
            ReviewStep::RoleStarted { role, result } => {
                let requests = match self.reviews.get_mut(&session_id) {
                    Some(slot) => match result {
                        Ok(()) => slot.driver.role_started(&role),
                        // A lane that cannot start is a coverage gap the
                        // supervisor is told about; any other role failing to
                        // start fails the review.
                        Err(error) if mj_review::lanes::lane_by_id(&role).is_some() => {
                            slot.driver.lane_failed(&role, error)
                        }
                        Err(error)
                            if matches!(
                                slot.driver.phase(),
                                TurnReviewPhase::Forwarding { .. }
                            ) =>
                        {
                            tracing::debug!(
                                session_id = %session_id,
                                role = %role,
                                %error,
                                "ignoring a late reviewer start result during primary handoff"
                            );
                            return;
                        }
                        Err(error) => {
                            self.fail(&session_id, error);
                            return;
                        }
                    },
                    None => return,
                };
                self.run(&session_id, requests);
            }
            ReviewStep::RolePrompted { .. } => {}
            ReviewStep::PrimaryPrompted(result) => {
                let requests = match result {
                    Ok(()) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.forward_succeeded())
                        .unwrap_or_default(),
                    Err(error) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| slot.driver.forward_failed(error))
                        .unwrap_or_default(),
                };
                self.run(&session_id, requests);
            }
            ReviewStep::RoleEvents { role, result } => {
                if role == SUPERVISOR_ROLE
                    && let Ok(events) = &result
                    && let Some(slot) = self.reviews.get_mut(&session_id)
                {
                    let awaited = slot
                        .driver
                        .awaited_commands()
                        .into_iter()
                        .find(|(role, _)| role == SUPERVISOR_ROLE)
                        .map(|(_, id)| id);
                    if awaited.is_some_and(|awaited| events.iter().any(|event| matches!(&event.observation, RelayObservation::CommandCompleted { command_id, .. } if command_id == &awaited))) {
                        // The worker owns lane admission. Read its queue after
                        // observing completion before deciding the supervisor
                        // has no outstanding specialists.
                        slot.pending_supervisor = Some(events.clone());
                        slot.dispatch_after_completion = slot.reading_dispatches;
                        self.poll_dispatches(&session_id);
                        return;
                    }
                }
                self.role_events(session_id, role, result);
            }
            ReviewStep::DispatchesAcked { ids, result } => {
                if let Some(slot) = self.reviews.get_mut(&session_id) {
                    slot.running_effects.remove("dispatch-ack");
                    match result {
                        Ok(()) => {
                            for id in ids {
                                slot.pending_dispatch_acks.remove(&id);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%session_id, %error, "lane dispatch acknowledgement will be retried");
                        }
                    }
                    self.dirty.insert(session_id);
                }
            }
            ReviewStep::Dispatches(result) => {
                if let Some(slot) = self.reviews.get_mut(&session_id) {
                    slot.reading_dispatches = false;
                }
                let requests = match result {
                    Ok(dispatches)
                        if dispatches.is_empty()
                            && self
                                .reviews
                                .get(&session_id)
                                .is_some_and(|slot| slot.pending_supervisor.is_none()) =>
                    {
                        return;
                    }
                    Ok(dispatches) => self
                        .reviews
                        .get_mut(&session_id)
                        .map(|slot| {
                            let mut requests = Vec::new();
                            for dispatch in dispatches {
                                slot.pending_dispatch_acks.insert(dispatch.id.clone());
                                if slot.role_generations.get(SUPERVISOR_ROLE)
                                    == Some(&dispatch.generation)
                                    && slot.accepted_dispatches.insert(dispatch.id)
                                {
                                    requests.push(dispatch.request);
                                }
                            }
                            if requests.is_empty() {
                                Vec::new()
                            } else {
                                slot.driver.lanes_dispatched(requests)
                            }
                        })
                        .unwrap_or_default(),
                    Err(error) => {
                        if let Some(slot) = self.reviews.get_mut(&session_id) {
                            slot.delivery_errors.insert("dispatch-read".into(), error);
                            if !slot.driver.supervisor_running() {
                                return;
                            }
                        }
                        self.publish(&session_id);
                        self.poll_dispatches(&session_id);
                        return;
                    }
                };
                self.run(&session_id, requests);
                let pending = self.reviews.get_mut(&session_id).and_then(|slot| {
                    if slot.dispatch_after_completion {
                        slot.dispatch_after_completion = false;
                        None
                    } else {
                        slot.pending_supervisor.take()
                    }
                });
                if let Some(events) = pending {
                    self.role_events(session_id, SUPERVISOR_ROLE.into(), Ok(events));
                } else if self
                    .reviews
                    .get(&session_id)
                    .is_some_and(|slot| slot.pending_supervisor.is_some())
                {
                    self.poll_dispatches(&session_id);
                }
            }
        }
    }

    /// Release the retained last-view for every session no longer in the live
    /// set, so a stopped or destroyed session's `MaterializedSession` (its full
    /// transcript) does not linger. The in-flight review state in `reviews`/
    /// `closing` is separate and keeps its own lifetime.
    pub(super) fn retain_sessions(&mut self, live: &std::collections::BTreeSet<String>) {
        self.sessions
            .retain(|session_id, _| live.contains(session_id));
    }
}
