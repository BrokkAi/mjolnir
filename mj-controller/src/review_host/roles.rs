use super::*;

impl HostState {
    pub(super) fn run_one(&mut self, session_id: &str, request: ReviewRequest) {
        match request {
            ReviewRequest::CaptureDelta { baselines } => {
                self.review_step(
                    session_id,
                    ReviewerAction::CaptureDelta { baselines },
                    |outcome| {
                        ReviewStep::Delta(match outcome {
                            Ok(ReviewerOutcome::Delta { repositories }) => Ok(repositories),
                            other => Err(unexpected(other)),
                        })
                    },
                );
            }
            ReviewRequest::AnalyzeDelta { repositories } => {
                self.review_step(
                    session_id,
                    ReviewerAction::AnalyzeDelta { repositories },
                    |outcome| {
                        ReviewStep::Analysis(match outcome {
                            Ok(ReviewerOutcome::ChangedFunctions { packet }) => Ok(packet),
                            other => Err(unexpected(other)),
                        })
                    },
                );
            }
            ReviewRequest::StartRole { role, fresh } => self.start_role(session_id, role, fresh),
            ReviewRequest::PromptRole {
                role,
                command_id,
                prompt,
            } => {
                self.prompt_role(session_id, &role, command_id, prompt);
                self.poll_role(session_id, &role, Duration::ZERO);
            }
            ReviewRequest::PromptPrimary { command_id, prompt } => {
                // The review's own corrective prompt must not be held by the
                // review's own lock, and by this point the review has
                // resolved, so the lock is already released below.
                self.prompt_primary(session_id, command_id, prompt);
            }
            ReviewRequest::PauseRole { role } => {
                let session_id = session_id.to_owned();
                self.spawn_reviewer(
                    session_id.clone(),
                    Some(role),
                    ReviewerAction::Pause,
                    move |outcome| {
                        if let Err(error) = outcome {
                            tracing::debug!(
                                session_id = %session_id,
                                %error,
                                "pausing a review role failed"
                            );
                        }
                        None
                    },
                );
            }
            ReviewRequest::AdvanceBaseline {
                trees,
                reviewed_through_ordinal,
            } => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.baselines = trees.clone();
                    slot.state.reviewed_through_ordinal = reviewed_through_ordinal;
                    // Clear an accepted handoff in the same durable state
                    // write as its prior-review record and baseline. Until
                    // this point shutdown/restart must retain it for command
                    // id reconciliation.
                    slot.state.pending_forward = None;
                    let state = slot.state.clone();
                    if let Err(error) = self.persist(session_id.to_owned(), state, None) {
                        tracing::warn!(session_id, %error, "could not queue review baseline persistence");
                    }
                }
                let session_id = session_id.to_owned();
                self.spawn_reviewer(
                    session_id.clone(),
                    None,
                    ReviewerAction::AdvanceBaseline { trees },
                    move |outcome| {
                        if let Err(error) = outcome {
                            // The controller's copy is what the next capture is
                            // taken against; the worker-side ref is only a gc
                            // pin, so a failure here costs nothing but the pin.
                            tracing::debug!(
                                session_id = %session_id,
                                %error,
                                "the review baseline ref could not be pinned"
                            );
                        }
                        None
                    },
                );
            }
            ReviewRequest::RecordPriorReview { prior } => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.prior_review = Some(prior);
                }
            }
            ReviewRequest::ClearPriorReview => {
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.prior_review = None;
                    let state = slot.state.clone();
                    if let Err(error) = self.persist(session_id.to_owned(), state, None) {
                        tracing::warn!(session_id, %error, "could not queue prior review cleanup");
                    }
                }
            }
            ReviewRequest::Close => {
                if self.closing.contains(session_id) {
                    return;
                }
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.active = None;
                    if matches!(
                        slot.driver.phase(),
                        TurnReviewPhase::Resolved(Resolution::Cancelled)
                    ) {
                        // A user explicitly cancelled a rejected handoff, so
                        // discard its retry record along with the held pane.
                        // An in-flight or accepted handoff never reaches this
                        // branch before its durable reconciliation sequence.
                        slot.state.pending_forward = None;
                    }
                    let state = slot.state.clone();
                    self.closing.insert(session_id.to_owned());
                    // The primary handoff has already returned a durable
                    // acceptance before Close can be requested. Releasing now
                    // lets later user prompts queue behind that accepted
                    // corrective turn; no held prompt can overtake it.
                    release_prompts(session_id);
                    if let Err(error) = self.persist(
                        session_id.to_owned(),
                        state,
                        Some(PersistenceCompletion::Close),
                    ) {
                        tracing::warn!(session_id, %error, "could not queue review close persistence");
                        self.closing.remove(session_id);
                        self.reviews.remove(session_id);
                        release_prompts(session_id);
                    }
                }
            }
        }
    }

    /// Stages the configured reviewer profile and starts one role under it.
    pub(super) fn start_role(&mut self, session_id: &str, role: String, fresh: bool) {
        let fresh_generation = if fresh {
            match next_review_generation() {
                Ok(generation) => Some(generation),
                Err(error) => {
                    self.fail(
                        session_id,
                        format!("the reviewer could not allocate a fresh conversation: {error}"),
                    );
                    return;
                }
            }
        } else {
            None
        };
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        // A fresh role must not reuse the running harness session: the
        // validator judges the reviewer's claims against source, so it must
        // not inherit them. Bumping the generation is what the sidecar reads
        // as "this is a different reviewer".
        if let Some(generation) = fresh_generation {
            slot.generation = generation;
        }
        let generation = slot.generation;
        let epoch = slot.epoch;
        let reviewer = slot.reviewer.clone();
        let repositories = slot.driver.repository_roots();
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let result = launch_role(
                &control,
                &environment,
                &session_id,
                &role,
                &reviewer,
                generation,
                &repositories,
            )
            .await;
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::RoleStarted { role, result },
            });
        });
    }

    pub(super) fn prompt_role(
        &mut self,
        session_id: &str,
        role: &str,
        command_id: String,
        prompt: String,
    ) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let owner = session_id.to_owned();
        let result_role = role.to_owned();
        self.spawn_reviewer(
            session_id.to_owned(),
            Some(role.to_owned()),
            ReviewerAction::Submit {
                command_id,
                command: prompt_command(prompt),
            },
            move |outcome| {
                let result = match outcome {
                    Ok(ReviewerOutcome::Accepted { .. }) => Ok(()),
                    other => Err(unexpected(other)),
                };
                Some(HostEvent::Step {
                    session_id: owner,
                    epoch,
                    step: ReviewStep::RolePrompted {
                        role: result_role,
                        result,
                    },
                })
            },
        );
    }

    /// Sends the review's corrective prompt to the primary agent. The actor
    /// receives a scoped admission that is valid only for this review and
    /// command id; the hold remains until the resulting event acknowledges a
    /// durable relay acceptance.
    pub(super) fn prompt_primary(&mut self, session_id: &str, command_id: String, prompt: String) {
        let Some(epoch) = self.reviews.get(session_id).map(|slot| slot.epoch) else {
            return;
        };
        let Some(admission) = admit_review_delivery(session_id, epoch, &command_id) else {
            self.step(
                session_id.to_owned(),
                epoch,
                ReviewStep::PrimaryPrompted(Err(
                    "the review handoff admission is no longer valid".to_owned()
                )),
            );
            return;
        };
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let submitted = async {
                let handle = control
                    .session(session_id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                handle
                    .submit_review_delivery(admission, prompt_command(prompt))
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"))
            }
            .await;
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::PrimaryPrompted(submitted),
            });
        });
    }

    /// Reads one role's journal from where the host left off.
    pub(super) fn poll_role(&mut self, session_id: &str, role: &str, delay: Duration) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        let transcript = slot.roles.entry(role.to_owned()).or_default();
        let after_ordinal = transcript.cursor_ordinal;
        let after_digest = if transcript.cursor_digest.is_empty() {
            mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            transcript.cursor_digest.clone()
        };
        let epoch = slot.epoch;
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        let role = role.to_owned();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = reviewer_action(
                &control,
                &session_id,
                Some(role.clone()),
                ReviewerAction::Attach {
                    after_ordinal,
                    after_digest,
                },
            )
            .await;
            let result = match result {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                other => Err(unexpected(other)),
            };
            let _ = events.send(HostEvent::Step {
                session_id,
                epoch,
                step: ReviewStep::RoleEvents { role, result },
            });
        });
    }

    pub(super) fn role_events(
        &mut self,
        session_id: String,
        role: String,
        result: Result<Vec<RelayEvent>, String>,
    ) {
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                if self.reviews.get(&session_id).is_some_and(|slot| {
                    matches!(slot.driver.phase(), TurnReviewPhase::Forwarding { .. })
                }) {
                    tracing::debug!(
                        session_id = %session_id,
                        role = %role,
                        %error,
                        "ignoring a late reviewer-role poll during primary handoff"
                    );
                    return;
                }
                self.fail(&session_id, error);
                return;
            }
        };
        let Some(slot) = self.reviews.get_mut(&session_id) else {
            return;
        };
        let idle = events.is_empty();
        let relay_session = role_session_id(&session_id, &role);
        let transcript = slot.roles.entry(role.clone()).or_default();
        transcript.apply(&relay_session, &events);
        // The newest agent message is not enough on its own: after the
        // validator starts, the reviewer's own findings are still the newest
        // message in that role's journal. The relay's completion record for
        // the exact command the driver submitted is what settles it.
        let awaited = slot
            .driver
            .awaited_commands()
            .into_iter()
            .find(|(awaited_role, _)| *awaited_role == role)
            .map(|(_, command_id)| command_id);
        let completed = awaited.as_ref().is_some_and(|awaited| {
            events.iter().any(|event| {
                matches!(
                    &event.observation,
                    RelayObservation::CommandCompleted { command_id, outcome }
                        if command_id == awaited
                            && matches!(
                                outcome,
                                mj_core::relay::RelayCommandOutcome::Prompt { .. }
                            )
                )
            })
        });
        let requests = match (completed, awaited) {
            (true, Some(awaited)) => {
                let answer = slot
                    .roles
                    .get(&role)
                    .and_then(RoleTranscript::latest_answer)
                    .unwrap_or_default();
                let slot = self.reviews.get_mut(&session_id).expect("the slot is open");
                slot.driver.role_turn_completed(&awaited, &answer)
            }
            _ => Vec::new(),
        };
        self.run(&session_id, requests);
        let Some(slot) = self.reviews.get(&session_id) else {
            return;
        };
        if slot.driver.active_roles().contains(&role) {
            self.poll_role(
                &session_id,
                &role,
                if idle {
                    ROLE_POLL_IDLE_INTERVAL
                } else {
                    Duration::ZERO
                },
            );
        }
        if role == SUPERVISOR_ROLE
            && self
                .reviews
                .get(&session_id)
                .is_some_and(|slot| slot.driver.supervisor_running())
        {
            self.poll_dispatches(&session_id);
        }
    }

    /// Collects the specialist lanes the supervisor asked for through its MCP
    /// tool. The tool answers the supervisor at once and leaves the request in
    /// the worker; this is where the host picks it up and launches them.
    pub(super) fn poll_dispatches(&mut self, session_id: &str) {
        self.review_step(session_id, ReviewerAction::TakeLaneDispatches, |outcome| {
            ReviewStep::Dispatches(match outcome {
                Ok(ReviewerOutcome::LaneDispatches { requests }) => Ok(requests),
                other => Err(unexpected(other)),
            })
        });
    }
}
