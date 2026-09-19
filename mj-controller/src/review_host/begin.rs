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
        if !finished_turn || !(self.config)().enabled {
            return;
        }
        self.begin(session_id, false, None);
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
        if self.preparing.contains(&session_id) {
            answer(
                reply,
                Err(StartRefusal("a review is already starting".to_owned())),
            );
            return;
        }
        let config = (self.config)();
        let tier = config.tier;
        let control = self.control.clone();
        let events = self.events.clone();
        let prepare_session = session_id.clone();
        let environment = self.environment.clone();
        // Admission and prompt refusal are one transition. Any prompt already
        // ahead of the preparation's reviewer-status command is drained before
        // `prepare` reads the actor view; every later prompt sees this hold.
        hold_prompts(&session_id);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.preparation_cancellation
            .insert(session_id.clone(), cancelled.clone());
        self.preparing.insert(session_id.clone());
        self.publish(&session_id);
        tokio::spawn(async move {
            let prepared = prepare(
                &control,
                &environment,
                &prepare_session,
                config,
                tier,
                cancelled,
            )
            .await;
            let _ = events.send(HostEvent::Prepared {
                session_id: prepare_session,
                manual,
                reply,
                prepared,
            });
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
            || self.preparing.contains(session_id)
        {
            return;
        }
        let Some(watch) = self.sessions.get(session_id) else {
            return;
        };
        if watch
            .materialized
            .as_ref()
            .is_none_or(|snapshot| !snapshot.queued_prompts.is_empty())
        {
            return;
        }
        hold_prompts(session_id);
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
                self.preparing.insert(session_id.clone());
                self.prepared(session_id, false, None, Ok(prepared));
            }
            Ok(None) => {
                self.recovery_candidates.remove(&session_id);
                release_prompts(&session_id);
                self.record_notice(
                    &session_id,
                    "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes".to_owned(),
                );
            }
            Err(error) => {
                // Keep the candidate so a later connected/idle observation can
                // retry. No success notice is emitted for an unknown outcome.
                tracing::warn!(session_id = %session_id, %error, "could not reconcile an interrupted review handoff");
                release_prompts(&session_id);
            }
        }
    }

    /// The gates that need nothing but the host's own state.
    pub(super) fn refuse_start(&self, session_id: &str) -> Option<StartRefusal> {
        if self.reviews.contains_key(session_id) {
            return Some(StartRefusal("a review is already open".to_owned()));
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
        let queued = watch
            .materialized
            .as_ref()
            .is_some_and(|materialized| !materialized.queued_prompts.is_empty());
        if queued {
            // Reviewing now would hold prompts the user has already sent. The
            // review after the queue drains covers the whole batch instead.
            return Some(StartRefusal(
                "prompts are queued; the review waits for them".to_owned(),
            ));
        }
        None
    }
}
