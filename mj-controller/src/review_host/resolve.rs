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
            // Preparation owns its result until it refuses before opening, or
            // its already queued Open commits and the cancelled owner closes.
            return Ok(());
        }
        let requests = {
            let Some(slot) = self.reviews.get_mut(session_id) else {
                return Err("no review is open for that session".to_owned());
            };
            if resolution == Resolution::Cancelled
                && let TurnReviewPhase::Forwarding { command_id, .. } = slot.driver.phase()
                && let Some(receipt) = slot.receipts.get_mut(command_id)
            {
                receipt.phase = super::durable::ReceiptPhase::Cancel;
                self.dirty.insert(session_id.to_owned());
                return Ok(());
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
            if resolution == Resolution::Forwarded {
                let Some(pending) = slot.driver.pending_forward() else {
                    return Err("the review handoff has no durable findings".to_owned());
                };
                slot.state.pending_forward = Some(pending);
            }
            if resolution == Resolution::Cancelled {
                for receipt in slot.receipts.values_mut() {
                    if receipt.phase == super::durable::ReceiptPhase::Waiting {
                        receipt.phase = super::durable::ReceiptPhase::Cancel;
                    }
                }
            }
            requests
        };
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
        let requests = slot.driver.request_failed(message);
        // The failed verdict releases prompt ownership once its checkpoint
        // commits, so replacement cannot resurrect a previously released turn.
        self.run(session_id, requests);
    }

    pub(super) fn run(&mut self, session_id: &str, requests: Vec<ReviewRequest>) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        for request in requests {
            // These are controller-owned facts, committed in the same checkpoint
            // as the driver transition that decided them.
            match &request {
                ReviewRequest::RecordPriorReview { prior } => {
                    slot.state.prior_review = Some(prior.clone())
                }
                ReviewRequest::ClearPriorReview => slot.state.prior_review = None,
                ReviewRequest::AdvanceBaseline {
                    trees,
                    reviewed_through_ordinal,
                } => {
                    slot.state.baselines = trees.clone();
                    slot.state.reviewed_through_ordinal = *reviewed_through_ordinal;
                    slot.state.pending_forward = None;
                }
                ReviewRequest::PromptRole {
                    role, command_id, ..
                } => {
                    slot.receipts.insert(
                        command_id.clone(),
                        super::durable::ReviewReceipt {
                            role: Some(role.clone()),
                            generation: slot
                                .role_generations
                                .get(role)
                                .copied()
                                .unwrap_or(slot.generation),
                            phase: super::durable::ReceiptPhase::Waiting,
                        },
                    );
                }
                ReviewRequest::PromptPrimary { command_id, .. } => {
                    slot.receipts.insert(
                        command_id.clone(),
                        super::durable::ReviewReceipt {
                            role: None,
                            generation: 0,
                            phase: super::durable::ReceiptPhase::Waiting,
                        },
                    );
                }
                ReviewRequest::StartRole { role, fresh } => {
                    let generation = if *fresh {
                        next_review_generation()
                    } else {
                        Ok(slot
                            .role_generations
                            .get(role)
                            .copied()
                            .unwrap_or(slot.generation))
                    };
                    match generation {
                        Ok(generation) => {
                            slot.role_generations.insert(role.clone(), generation);
                        }
                        Err(error) => {
                            self.fail(
                                session_id,
                                format!("cannot allocate reviewer identity: {error}"),
                            );
                            return;
                        }
                    }
                }
                _ => {}
            }
            if !matches!(
                request,
                ReviewRequest::RecordPriorReview { .. } | ReviewRequest::ClearPriorReview
            ) && !slot.outbox.contains(&request)
            {
                slot.outbox.push(request);
            }
        }
        let awaited = slot.driver.awaited_commands();
        for (command_id, receipt) in &mut slot.receipts {
            if receipt.phase == super::durable::ReceiptPhase::Waiting
                && let Some(role) = &receipt.role
                && !awaited
                    .iter()
                    .any(|(owner, command)| owner == role && command == command_id)
            {
                // Completion or failure may race a lost submit acknowledgement.
                // Reconcile with the worker before dropping this delivery owner.
                receipt.phase = super::durable::ReceiptPhase::Cancel;
            }
        }
        self.dirty.insert(session_id.to_owned());
        self.publish(session_id);
    }
}
