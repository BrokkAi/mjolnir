use super::*;

impl HostState {
    /// Watches one session for the edge that arms an automatic review.
    pub(super) async fn observe(
        &mut self,
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
        prompt_driven: bool,
    ) {
        let execution = snapshot
            .as_ref()
            .map_or(MaterializedExecutionState::Idle, |snapshot| {
                snapshot.execution
            });
        let previous = self.sessions.insert(
            session_id.clone(),
            SessionWatch {
                execution,
                prompt_driven,
                materialized: snapshot,
            },
        );
        let parents: Vec<_> = self
            .reviews
            .iter()
            .filter(|(_, slot)| {
                slot.state.active_group.as_ref().is_some_and(|group| {
                    group
                        .registrations
                        .iter()
                        .any(|item| item.child_session_id.as_deref() == Some(&session_id))
                })
            })
            .map(|(parent, _)| parent.clone())
            .collect();
        for parent in parents {
            self.publish(&parent);
        }
        if self.recovery_candidates.contains(&session_id)
            && matches!(execution, MaterializedExecutionState::Idle)
        {
            self.begin_recovery(&session_id);
            return;
        }
        // A turn the harness starts on its own also runs and then goes idle.
        // Reviewing that is a separate decision, so the edge that arms a
        // review is the end of a turn that answered a prompt.
        let finished_turn = previous.as_ref().is_some_and(|watch| {
            watch.prompt_driven
                && matches!(watch.execution, MaterializedExecutionState::Running { .. })
        }) && matches!(execution, MaterializedExecutionState::Idle);
        if !finished_turn || !(self.config)(&session_id).arms_turn_review() {
            if matches!(execution, MaterializedExecutionState::Idle) {
                self.begin_followup(&session_id);
            }
            return;
        }
        if self.reviews.contains_key(&session_id)
            || self.preparing.contains_key(&session_id)
            || self.pending_open.contains_key(&session_id)
            || self.refuse_start(&session_id).is_some()
            || self.sessions.get(&session_id).is_some_and(|watch| {
                watch
                    .materialized
                    .as_ref()
                    .is_some_and(|view| !view.queued_prompts.is_empty())
            })
        {
            self.mark_followup(&session_id);
            return;
        }
        if self.followups.contains(&session_id) {
            self.begin_followup(&session_id);
            return;
        }
        if !self.startup_ready {
            self.deferred_admissions.push(HostEvent::Start {
                session_id,
                manual: false,
                reply: None,
            });
            return;
        }
        self.begin(session_id, false, None);
    }

    pub(super) fn mark_followup(&mut self, session_id: &str) {
        if self.followups.insert(session_id.to_owned()) {
            self.followup_saving.insert(session_id.to_owned());
            if self.persistence.as_ref().is_none_or(|lane| {
                lane.send(PersistenceRequest::MarkFollowUp {
                    session_id: session_id.to_owned(),
                })
                .is_err()
            }) {
                tracing::warn!(%session_id, "could not queue pending follow-up review: persistence lane stopped");
                self.followup_saving.remove(session_id);
            }
        }
    }

    pub(super) fn begin_followup(&mut self, session_id: &str) {
        if !self.startup_ready
            || !self.followups.contains(session_id)
            || self.followup_saving.contains(session_id)
            || self.closing.contains(session_id)
            || self.refuse_start(session_id).is_some()
            || self.preparing.contains_key(session_id)
            || self.pending_open.contains_key(session_id)
            || self.sessions.get(session_id).is_some_and(|watch| {
                watch
                    .materialized
                    .as_ref()
                    .is_some_and(|view| !view.queued_prompts.is_empty())
            })
        {
            return;
        }
        self.followups.remove(session_id);
        self.begin(session_id.to_owned(), false, None);
    }

    /// Decides whether a review can start, and prepares one if it can.
    ///
    /// The cheap gates are answered here; the ones that need the database or
    /// the worker are answered in the preparation task, so this never blocks
    /// the host's loop.
    pub(super) fn begin(
        &mut self,
        session_id: String,
        manual: bool,
        reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    ) {
        if crate::controller::move_session::move_owns_session(&session_id) {
            answer(reply, Err(StartRefusal("session is moving".to_owned())));
            return;
        }
        if let Some(refusal) = self.refuse_start(&session_id) {
            answer(reply, Err(refusal));
            return;
        }
        if self.preparing.contains_key(&session_id) {
            answer(
                reply,
                Err(StartRefusal("a review is already starting".to_owned())),
            );
            return;
        }
        let config = (self.config)(&session_id);
        let control = self.control.clone();
        let events = self.events.clone();
        let prepare_session = session_id.clone();
        let environment = self.environment.clone();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.preparing.insert(
            session_id.clone(),
            PreparingOperation::Review {
                cancelled: cancelled.clone(),
                push: false,
            },
        );
        self.publish(&session_id);
        tokio::spawn(async move {
            let prepared =
                prepare(&control, &environment, &prepare_session, config, cancelled).await;
            let _ = events.send(HostEvent::Prepared {
                session_id: prepare_session,
                manual,
                reply,
                prepared,
            });
        });
    }

    pub(super) fn begin_push(
        &mut self,
        session_id: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        let refusal = if crate::controller::move_session::move_owns_session(&session_id) {
            Some("session is moving")
        } else if self.reviews.contains_key(&session_id)
            || self.preparing.contains_key(&session_id)
            || self.recovery_candidates.contains(&session_id)
            || self.recovery_in_flight.contains(&session_id)
        {
            Some("a review is already open")
        } else {
            None
        };
        if let Some(reason) = refusal {
            let _ = reply.send(Err(StartRefusal(reason.into())));
            return;
        }
        let config = (self.config)(&session_id);
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.preparing.insert(
            session_id.clone(),
            PreparingOperation::Review {
                cancelled: cancelled.clone(),
                push: true,
            },
        );
        self.publish(&session_id);
        tokio::spawn(async move {
            let prepared = tokio::time::timeout(
                std::time::Duration::from_secs(90),
                prepare_scoped(
                    &control,
                    &environment,
                    &session_id,
                    config,
                    cancelled.clone(),
                    mj_core::relay::CaptureScope::PushableCommits,
                    path.map(|path| vec![std::path::PathBuf::from(path)]),
                ),
            )
            .await;
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(_) => {
                    cancelled.store(true, std::sync::atomic::Ordering::Release);
                    Err(StartRefusal(
                        "review preparation exceeded 90 seconds; call request_review again".into(),
                    ))
                }
            };
            let accepted = prepared.as_ref().ok().map(|prepared| {
                let repositories: Vec<_> = prepared.captured.as_deref().unwrap_or_default().iter().map(|delta| serde_json::json!({
                    "repository": delta.root, "base_commit": delta.base_commit, "base_kind": delta.base_kind,
                    "head_commit": delta.head_commit, "changed_file_count": delta.files.len()
                })).collect();
                serde_json::json!({"accepted": true, "repositories": repositories})
            });
            let (opened, result) = oneshot::channel();
            if events
                .send(HostEvent::Prepared {
                    session_id,
                    manual: true,
                    reply: Some(opened),
                    prepared,
                })
                .is_err()
            {
                let _ = reply.send(Err(StartRefusal("the review host stopped".into())));
                return;
            }
            let outcome = result
                .await
                .unwrap_or_else(|_| Err(StartRefusal("the review host stopped".into())));
            let _ = reply.send(outcome.map(|()| accepted.expect("accepted review has a capture")));
        });
    }

    /// Reconciles a durable corrective handoff left by a previous daemon.
    /// This path does not require a reviewer profile: the review already has
    /// findings, and only the primary relay's idempotent command needs to be
    /// observed again.
    pub(super) fn begin_recovery(&mut self, session_id: &str) {
        if !self.recovery_candidates.contains(session_id)
            || self.recovery_in_flight.contains(session_id)
            || self.reviews.contains_key(session_id)
            || self.preparing.contains_key(session_id)
        {
            return;
        }
        let Some(watch) = self.sessions.get(session_id) else {
            return;
        };
        if watch.materialized.is_none() {
            return;
        }
        self.recovery_in_flight.insert(session_id.to_owned());
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let prepared = prepare_recovery(&control, &environment, &session_id).await;
            let _ = events.send(HostEvent::RecoveryPrepared {
                session_id,
                prepared,
            });
        });
    }

    pub(super) fn recovery_prepared(
        &mut self,
        session_id: String,
        prepared: Result<Option<Prepared>, String>,
    ) {
        self.recovery_in_flight.remove(&session_id);
        match prepared {
            Ok(Some(prepared)) => {
                self.recovery_candidates.remove(&session_id);
                self.preparing.insert(
                    session_id.clone(),
                    PreparingOperation::Review {
                        cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        push: false,
                    },
                );
                self.prepared(session_id, false, None, Ok(prepared));
            }
            Ok(None) => {
                self.recovery_candidates.remove(&session_id);
                self.record_notice(
                    &session_id,
                    "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes".to_owned(),
                );
            }
            Err(error) => {
                // Keep the candidate so a later connected/idle observation can
                // retry. No success notice is emitted for an unknown outcome.
                tracing::warn!(session_id = %session_id, %error, "could not reconcile an interrupted review handoff");
            }
        }
    }

    /// The gates that need nothing but the host's own state.
    pub(super) fn refuse_start(&self, session_id: &str) -> Option<StartRefusal> {
        if let Some(slot) = self.reviews.get(session_id) {
            // Name the reviewer's pending question so the person can find it
            // in the virtual workspace.
            let question = slot.state.active_group.as_ref().and_then(|group| {
                group
                    .registrations
                    .iter()
                    .filter_map(|item| item.child_session_id.as_ref())
                    .filter_map(|child| self.sessions.get(child))
                    .filter_map(|watch| watch.materialized.as_ref())
                    .flat_map(|session| session.pending_elicitations.iter())
                    .next()
            });
            return Some(StartRefusal(match question {
                Some(question) => format!(
                    "the previous review is waiting for your answer to a question ({}); \
                     answer it in the virtual workspace",
                    quoted_question(&question.message)
                ),
                None => "a review is already open".to_owned(),
            }));
        }
        if self.recovery_candidates.contains(session_id)
            || self.recovery_in_flight.contains(session_id)
        {
            return Some(StartRefusal(
                "an interrupted review handoff is being reconciled".to_owned(),
            ));
        }
        let Some(watch) = self.sessions.get(session_id) else {
            return Some(StartRefusal("this session is not connected".to_owned()));
        };
        if !matches!(watch.execution, MaterializedExecutionState::Idle) {
            return Some(StartRefusal(
                "a review runs between turns; this one is still working".to_owned(),
            ));
        }
        None
    }
}
