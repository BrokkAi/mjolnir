//! Checkpoints couple driver transitions to the effects they authorize.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum ReceiptPhase {
    Waiting,
    Release,
    Cancel,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct ReviewReceipt {
    pub(super) role: Option<String>,
    pub(super) generation: u64,
    pub(super) phase: ReceiptPhase,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    version: u32,
    epoch: u64,
    driver: TurnReviewDriver,
    roles: BTreeMap<String, RoleTranscript>,
    reviewer: ReviewerIdentity,
    generation: u64,
    role_generations: BTreeMap<String, u64>,
    outbox: Vec<ReviewRequest>,
    receipts: BTreeMap<String, ReviewReceipt>,
    accepted_dispatches: BTreeSet<String>,
    pending_dispatch_acks: BTreeSet<String>,
}

pub(super) fn effect_key(request: &ReviewRequest) -> String {
    match request {
        ReviewRequest::CaptureDelta { .. } => "capture".into(),
        ReviewRequest::AnalyzeDelta { .. } => "analysis".into(),
        ReviewRequest::StartRole { role, .. } => format!("start:{role}"),
        ReviewRequest::PromptRole { role, .. } => format!("prompt:{role}"),
        ReviewRequest::PromptPrimary { .. } => "primary".into(),
        ReviewRequest::PauseRole { role } => format!("pause:{role}"),
        ReviewRequest::AdvanceBaseline { .. } => "baseline".into(),
        ReviewRequest::RecordPriorReview { .. } => "prior".into(),
        ReviewRequest::ClearPriorReview => "clear-prior".into(),
        ReviewRequest::Close => "close".into(),
    }
}

impl ReviewSlot {
    pub(super) fn checkpoint(&self) -> Result<serde_json::Value, String> {
        serde_json::to_value(Checkpoint {
            version: 1,
            epoch: self.epoch,
            driver: self.driver.clone(),
            roles: self.roles.clone(),
            reviewer: self.reviewer.clone(),
            generation: self.generation,
            role_generations: self.role_generations.clone(),
            outbox: self.outbox.clone(),
            receipts: self.receipts.clone(),
            accepted_dispatches: self.accepted_dispatches.clone(),
            pending_dispatch_acks: self.pending_dispatch_acks.clone(),
        })
        .map_err(|error| format!("serialize review checkpoint: {error}"))
    }

    pub(super) fn restore(mut state: TurnReviewState) -> Result<Self, String> {
        let checkpoint: Checkpoint = serde_json::from_value(
            state
                .orchestration
                .take()
                .ok_or_else(|| "review checkpoint is missing".to_owned())?,
        )
        .map_err(|error| format!("parse review checkpoint: {error}"))?;
        if checkpoint.version != 1 {
            return Err("unsupported review checkpoint version".into());
        }
        Ok(Self {
            epoch: checkpoint.epoch,
            driver: checkpoint.driver,
            roles: checkpoint.roles,
            reviewer: checkpoint.reviewer,
            state,
            generation: checkpoint.generation,
            role_generations: checkpoint.role_generations,
            outbox: checkpoint.outbox,
            receipts: checkpoint.receipts,
            running_effects: BTreeSet::new(),
            delivery_errors: BTreeMap::new(),
            polling_roles: BTreeSet::new(),
            reading_dispatches: false,
            pending_supervisor: None,
            dispatch_after_completion: false,
            accepted_dispatches: checkpoint.accepted_dispatches,
            pending_dispatch_acks: checkpoint.pending_dispatch_acks,
        })
    }
}

impl HostState {
    pub(super) fn checkpoint_dirty(&mut self) {
        for id in std::mem::take(&mut self.dirty) {
            if self.closing.contains(&id) {
                continue;
            }
            if self.checkpointing.contains_key(&id) {
                self.dirty.insert(id);
                continue;
            }
            let Some(slot) = self.reviews.get_mut(&id) else {
                continue;
            };
            let epoch = slot.epoch;
            self.next_checkpoint_revision = self
                .next_checkpoint_revision
                .checked_add(1)
                .expect("review checkpoint revision exhausted");
            let revision = self.next_checkpoint_revision;
            self.checkpointing.insert(id.clone(), (epoch, revision));
            let state = match slot.checkpoint() {
                Ok(checkpoint) => {
                    slot.state.orchestration = Some(checkpoint);
                    slot.state.clone()
                }
                Err(error) => {
                    self.checkpoint_saved(id, epoch, revision, Err(error));
                    continue;
                }
            };
            if let Err(error) = self.persist(
                id.clone(),
                state,
                Some(PersistenceCompletion::Checkpoint { epoch, revision }),
            ) {
                self.checkpoint_saved(id, epoch, revision, Err(error));
            }
        }
    }

    pub(super) fn retry_checkpoint(&mut self, session_id: String, epoch: u64, revision: u64) {
        if self.checkpointing.get(&session_id) != Some(&(epoch, revision))
            || self.reviews.get(&session_id).map(|slot| slot.epoch) != Some(epoch)
        {
            return;
        }
        self.checkpointing.remove(&session_id);
        self.dirty.insert(session_id);
        // The owner serializes its current desired state after the backoff.
        self.checkpoint_dirty();
    }

    pub(super) fn checkpoint_saved(
        &mut self,
        session_id: String,
        epoch: u64,
        revision: u64,
        result: Result<(), String>,
    ) {
        if self.checkpointing.get(&session_id) != Some(&(epoch, revision))
            || self.reviews.get(&session_id).map(|slot| slot.epoch) != Some(epoch)
        {
            return;
        }
        if let Err(error) = result {
            tracing::error!(%session_id, %error, "review checkpoint failed; retaining work for retry");
            self.persistence_errors.insert(session_id.clone(), error);
            self.publish(&session_id);
            let events = self.events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = events
                    .send(HostEvent::RetryCheckpoint {
                        session_id,
                        epoch,
                        revision,
                    })
                    .await;
            });
            return;
        }
        self.checkpointing.remove(&session_id);
        if self.dirty.contains(&session_id) {
            // A newer desired snapshot exists. This ACK authorizes no effect
            // from that snapshot; keep the barrier until its own commit.
            self.checkpoint_dirty();
            return;
        }
        self.persistence_errors.remove(&session_id);
        if self
            .reviews
            .get(&session_id)
            .is_some_and(|slot| matches!(slot.driver.verdict(), Some(ReviewVerdict::Failed { .. })))
        {
            release_prompts(&session_id);
        }
        self.publish(&session_id);
        if let Some(reply) = self.start_replies.remove(&session_id) {
            let _ = reply.send(Ok(()));
        }
        if let Some(reply) = self.resolve_replies.remove(&session_id) {
            let _ = reply.send(Ok(()));
        }
        self.dispatch_outbox(&session_id);
    }

    pub(super) fn dispatch_outbox(&mut self, session_id: &str) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        let can_close = slot
            .outbox
            .iter()
            .all(|request| matches!(request, ReviewRequest::Close))
            && slot.pending_dispatch_acks.is_empty()
            && slot.receipts.is_empty();
        let requests = slot
            .outbox
            .iter()
            .filter(|request| {
                let receipt_ready = match request {
                    ReviewRequest::PromptRole { command_id, .. } | ReviewRequest::PromptPrimary { command_id, .. } => slot.receipts.get(command_id).is_none_or(|receipt| receipt.phase == ReceiptPhase::Waiting),
                    _ => true,
                };
                let role_ready = match request {
                    ReviewRequest::StartRole { role, .. } => !slot.receipts.values().any(|receipt| receipt.role.as_ref() == Some(role) && receipt.phase != ReceiptPhase::Waiting),
                    ReviewRequest::PauseRole { role } => !slot.outbox.iter().any(|pending| matches!(pending, ReviewRequest::StartRole { role: owner, .. } | ReviewRequest::PromptRole { role: owner, .. } if owner == role)),
                    _ => true,
                };
                receipt_ready && role_ready && (!matches!(request, ReviewRequest::Close) || can_close)
                    && slot.running_effects.insert(effect_key(request))
            })
            .cloned()
            .collect::<Vec<_>>();
        let acks = if !slot.pending_dispatch_acks.is_empty()
            && slot.running_effects.insert("dispatch-ack".into())
        {
            Some(
                slot.pending_dispatch_acks
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let receipts = slot
            .receipts
            .iter()
            .filter(|(id, receipt)| {
                receipt.phase != ReceiptPhase::Waiting
                    && slot.running_effects.insert(format!("receipt:{id}"))
            })
            .map(|(id, receipt)| (id.clone(), receipt.clone()))
            .collect::<Vec<_>>();
        for request in requests {
            self.run_one(session_id, request);
        }
        for (command_id, receipt) in receipts {
            self.run_receipt(session_id, command_id, receipt);
        }
        if let Some(ids) = acks {
            let completion_ids = ids.clone();
            self.review_step(
                session_id,
                ReviewerAction::AckLaneDispatches { ids },
                move |outcome| ReviewStep::DispatchesAcked {
                    ids: completion_ids,
                    result: match outcome {
                        Ok(ReviewerOutcome::LaneDispatchesAcknowledged) => Ok(()),
                        other => Err(unexpected(other)),
                    },
                },
            );
        }
    }

    pub(super) fn run_receipt(&self, session_id: &str, command_id: String, receipt: ReviewReceipt) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let events = self.events.clone();
        let control = self.control.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let result = async {
                if let Some(role) = receipt.role {
                    let action = match receipt.phase {
                        ReceiptPhase::Cancel => ReviewerAction::CancelReviewerCommandAdmission {
                            generation: receipt.generation,
                            command_id: command_id.clone(),
                        },
                        ReceiptPhase::Release => ReviewerAction::ReleaseReviewerCommandReceipt {
                            generation: receipt.generation,
                            command_id: command_id.clone(),
                        },
                        ReceiptPhase::Waiting => {
                            return Err("receipt is not ready for settlement".into());
                        }
                    };
                    match reviewer_action(&control, &session_id, Some(role), action).await? {
                        ReviewerOutcome::CommandReceipt { receipt } => {
                            Ok(receipt.map(|receipt| *receipt))
                        }
                        ReviewerOutcome::CommandReceiptReleased => Ok(None),
                        other => Err(unexpected(Ok(other))),
                    }
                } else {
                    let handle = control
                        .session(session_id.clone())
                        .await
                        .map_err(|error| format!("{error:#}"))?;
                    match receipt.phase {
                        ReceiptPhase::Cancel => handle
                            .cancel_command_admission(command_id.clone())
                            .await
                            .map_err(|error| format!("{error:#}")),
                        ReceiptPhase::Release => handle
                            .release_command_receipt(command_id.clone())
                            .await
                            .map(|()| None)
                            .map_err(|error| format!("{error:#}")),
                        ReceiptPhase::Waiting => Err("receipt is not ready for settlement".into()),
                    }
                }
            }
            .await;
            if result.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let _ = events
                .send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::ReceiptSettled {
                        command_id,
                        phase: receipt.phase,
                        result: result.map(|receipt| receipt.map(Box::new)),
                    },
                })
                .await;
        });
    }

    pub(super) fn review_effect(
        &self,
        session_id: &str,
        role: Option<String>,
        action: ReviewerAction,
        key: String,
    ) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let result = reviewer_action(&control, &session_id, role, action)
                .await
                .map(|_| ());
            if result.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let _ = events
                .send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::EffectSettled { key, result },
                })
                .await;
        });
    }

    pub(super) fn retry_effect(&mut self, session_id: &str, key: String, error: String) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        slot.delivery_errors.insert(key.clone(), error);
        let epoch = slot.epoch;
        self.publish(session_id);
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = events
                .send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::RetryEffect { key },
                })
                .await;
        });
    }

    pub(super) fn settle_effect(&mut self, session_id: &str, step: &ReviewStep) {
        let key = match step {
            ReviewStep::Delta(_) => Some("capture".into()),
            ReviewStep::Analysis(_) => Some("analysis".into()),
            ReviewStep::RoleStarted { role, .. } => Some(format!("start:{role}")),
            ReviewStep::RolePrompted { role, .. } => Some(format!("prompt:{role}")),
            ReviewStep::PrimaryPrompted(_) => Some("primary".into()),
            _ => None,
        };
        if let Some(key) = key
            && let Some(slot) = self.reviews.get_mut(session_id)
        {
            if matches!(
                step,
                ReviewStep::RolePrompted { result: Ok(()), .. }
                    | ReviewStep::PrimaryPrompted(Ok(()))
            ) {
                for request in &slot.outbox {
                    if effect_key(request) == key
                        && let ReviewRequest::PromptRole { command_id, .. }
                        | ReviewRequest::PromptPrimary { command_id, .. } = request
                        && let Some(receipt) = slot.receipts.get_mut(command_id)
                        && receipt.phase == ReceiptPhase::Waiting
                    {
                        receipt.phase = ReceiptPhase::Release;
                    }
                }
            }
            slot.outbox.retain(|request| effect_key(request) != key);
            slot.running_effects.remove(&key);
            slot.delivery_errors.remove(&key);
            self.dirty.insert(session_id.to_owned());
        }
    }
}
