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
                let generation = self
                    .reviews
                    .get(session_id)
                    .and_then(|slot| slot.role_generations.get(&role))
                    .copied();
                let key = format!("pause:{role}");
                self.review_effect(
                    session_id,
                    Some(role),
                    generation.map_or(ReviewerAction::Pause, |generation| {
                        ReviewerAction::PauseGeneration { generation }
                    }),
                    key,
                );
            }
            ReviewRequest::AdvanceBaseline { trees, .. } => {
                self.review_effect(
                    session_id,
                    None,
                    ReviewerAction::AdvanceBaseline { trees },
                    "baseline".into(),
                );
            }
            local @ (ReviewRequest::RecordPriorReview { .. } | ReviewRequest::ClearPriorReview) => {
                self.run(session_id, vec![local]);
            }
            ReviewRequest::Close => {
                if self.closing.contains(session_id) {
                    return;
                }
                if let Some(slot) = self.reviews.get_mut(session_id) {
                    slot.state.active = None;
                    slot.state.orchestration = None;
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
                    let epoch = slot.epoch;
                    let state = slot.state.clone();
                    self.closing.insert(session_id.to_owned());
                    if let Err(error) = self.persist(
                        session_id.to_owned(),
                        state,
                        Some(PersistenceCompletion::Close { epoch }),
                    ) {
                        self.state_saved(
                            session_id.to_owned(),
                            PersistenceCompletion::Close { epoch },
                            Err(error),
                        );
                    }
                }
            }
        }
    }

    /// Stages the configured reviewer profile and starts one role under it.
    pub(super) fn start_role(&mut self, session_id: &str, role: String, fresh: bool) {
        let _ = fresh;
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        // Generation was selected and checkpointed before this effect started.
        let generation = slot
            .role_generations
            .get(&role)
            .copied()
            .unwrap_or(slot.generation);
        slot.generation = generation;
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
            let _ = events
                .send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::RoleStarted { role, result },
                })
                .await;
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
        let generation = self
            .reviews
            .get(session_id)
            .and_then(|slot| slot.role_generations.get(role))
            .copied()
            .unwrap_or_default();
        let owner = session_id.to_owned();
        let result_role = role.to_owned();
        self.spawn_reviewer(
            session_id.to_owned(),
            Some(role.to_owned()),
            ReviewerAction::SubmitDurable {
                generation,
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
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let submitted = async {
                if primary_accepted(environment.clone(), session_id.clone(), command_id.clone())
                    .await?
                {
                    return Ok(());
                }
                let handle = control
                    .session(session_id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                handle
                    .sync_now()
                    .await
                    .map_err(|error| format!("refresh review handoff receipt: {error:#}"))?;
                if primary_accepted(environment, session_id.clone(), command_id).await? {
                    return Ok(());
                }
                handle
                    .submit_review_delivery(admission, prompt_command(prompt))
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"))
            }
            .await;
            let _ = events
                .send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::PrimaryPrompted(submitted),
                })
                .await;
        });
    }

    /// Reads one role's journal from where the host left off.
    pub(super) fn poll_role(&mut self, session_id: &str, role: &str, delay: Duration) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        if !slot.polling_roles.insert(role.to_owned()) {
            return;
        }
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
            let _ = events
                .send(HostEvent::Step {
                    session_id,
                    epoch,
                    step: ReviewStep::RoleEvents { role, result },
                })
                .await;
        });
    }

    pub(super) fn role_events(
        &mut self,
        session_id: String,
        role: String,
        result: Result<Vec<RelayEvent>, String>,
    ) {
        if let Some(slot) = self.reviews.get_mut(&session_id) {
            slot.polling_roles.remove(&role);
        }
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                let active = self.reviews.get_mut(&session_id).is_some_and(|slot| {
                    slot.delivery_errors.insert(format!("poll:{role}"), error);
                    slot.driver.active_roles().contains(&role)
                });
                if active {
                    self.poll_role(&session_id, &role, Duration::from_secs(1));
                    self.publish(&session_id);
                }
                return;
            }
        };
        let Some(slot) = self.reviews.get_mut(&session_id) else {
            return;
        };
        slot.delivery_errors.remove(&format!("poll:{role}"));
        let idle = events.is_empty();
        let relay_session = role_session_id(&session_id, &role);
        let transcript = slot.roles.entry(role.clone()).or_default();
        if let Err(error) = transcript.apply(&relay_session, &events) {
            self.fail(&session_id, error);
            return;
        }
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
                    RelayObservation::CommandCompleted { command_id, outcome , ..}
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
        if !events.is_empty() || !requests.is_empty() {
            self.run(&session_id, requests);
        }
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
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        if slot.reading_dispatches {
            return;
        }
        slot.reading_dispatches = true;
        self.review_step(session_id, ReviewerAction::ReadLaneDispatches, |outcome| {
            ReviewStep::Dispatches(match outcome {
                Ok(ReviewerOutcome::PendingLaneDispatches { dispatches }) => Ok(dispatches),
                other => Err(unexpected(other)),
            })
        });
    }
}

async fn primary_accepted(
    environment: Arc<dyn ReviewEnvironment>,
    session: String,
    command: String,
) -> Result<bool, String> {
    tokio::task::spawn_blocking(move || environment.primary_prompt_accepted(&session, &command))
        .await
        .map_err(|error| format!("review handoff receipt task stopped: {error}"))?
}
