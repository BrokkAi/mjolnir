use super::*;

impl ActiveChat {
    /// Resolve shared settings before consuming the captured plan decision.
    pub(crate) fn open_second_opinion(
        &mut self,
        request: mj_core::elicitation::ElicitationRequest,
        proposal: String,
    ) {
        if self.context.is_none() {
            self.state
                .set_notice("A second opinion needs this session's configuration");
            self.state.restore_elicitation(request);
            return;
        }
        self.state
            .open_second_opinion(CapturedProposal { request, proposal });
        self.prepare_reviewer();
    }

    pub(crate) fn run_second_opinion(&mut self, intent: SecondOpinionIntent) {
        match intent {
            SecondOpinionIntent::Retry => self.prepare_reviewer(),
            SecondOpinionIntent::Workflow(requests) => {
                // Forget before dispatch: a crash must not restore a review whose feedback went out.
                self.forget_review();
                for request in requests {
                    self.run_workflow_request(request);
                }
            }
            SecondOpinionIntent::Closed => {
                if let Some(flag) = self.reviewer_preparation.take() {
                    flag.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        }
    }

    fn prepare_reviewer(&mut self) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        if let Some(flag) = self.reviewer_preparation.take() {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        self.reviewer_preparation_sequence += 1;
        let generation = self.reviewer_preparation_sequence;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.reviewer_preparation = Some(cancelled.clone());
        let stager = context.reviewer_stager.clone();
        let config = context.config.clone();
        let record = context.session.clone();
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        tokio::spawn(async move {
            let mut started_generation = None;
            let result = async {
                // Turn review and second opinion use the same default role.
                if let ReviewerOutcome::Status(status) =
                    session.reviewer(ReviewerAction::Status).await?
                    && status.active_prompt.is_some()
                {
                    anyhow::bail!("the reviewer is busy");
                }
                let resolved = session.resolve_review_settings(cancelled.clone()).await?;
                anyhow::ensure!(
                    !cancelled.load(std::sync::atomic::Ordering::Acquire),
                    "review preparation cancelled"
                );
                let profile = resolved.profile.clone();
                let lifetime = resolved.generation;
                let staging_cancelled = cancelled.clone();
                let mut launch = tokio::task::spawn_blocking(move || {
                    stager.stage(config, record, profile, lifetime, staging_cancelled)
                })
                .await??;
                launch.model = resolved.main.model.clone();
                launch.effort = resolved.main.effort.clone();
                anyhow::ensure!(
                    !cancelled.load(std::sync::atomic::Ordering::Acquire),
                    "review preparation cancelled"
                );
                started_generation = Some(lifetime);
                match session
                    .reviewer(ReviewerAction::Start {
                        config: Box::new(launch),
                    })
                    .await?
                {
                    ReviewerOutcome::Started(_) => Ok(resolved),
                    other => anyhow::bail!("unexpected reviewer startup: {other:?}"),
                }
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let abandoned =
                cancelled.load(std::sync::atomic::Ordering::Acquire) || updates.is_closed();
            if (abandoned || result.is_err())
                && let Some(generation) = started_generation
                && let Err(error) = session
                    .reviewer(ReviewerAction::PauseGeneration { generation })
                    .await
            {
                tracing::warn!(%error, "could not stop failed or cancelled reviewer");
            }
            if abandoned {
                return;
            }
            // Keep cleanup ownership until the UI has consumed the result. Dropping
            // a chat with an unread successful startup must also stop that reviewer.
            let (acknowledged, acknowledgement) = tokio::sync::oneshot::channel();
            if let Err(error) = updates.send(ChatIoUpdate::ReviewerPrepared {
                generation,
                result,
                acknowledged,
            }) {
                tracing::debug!(%error, "reviewer preparation result dropped");
            }
            if acknowledgement.await.is_err()
                && let Some(generation) = started_generation
                && let Err(error) = session
                    .reviewer(ReviewerAction::PauseGeneration { generation })
                    .await
            {
                tracing::warn!(%error, "could not stop abandoned reviewer");
            }
        });
    }

    pub(crate) fn apply_reviewer_prepared(
        &mut self,
        generation: u64,
        result: std::result::Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    ) {
        if generation != self.reviewer_preparation_sequence
            || !matches!(
                self.state.second_opinion(),
                Some(SecondOpinion::Setup { .. })
            )
        {
            if let Ok(resolved) = result {
                let session = self.session.clone();
                tokio::spawn(async move {
                    if let Err(error) = session
                        .reviewer(ReviewerAction::PauseGeneration {
                            generation: resolved.generation,
                        })
                        .await
                    {
                        tracing::warn!(%error, "could not stop stale reviewer preparation");
                    }
                });
            }
            return;
        }
        self.reviewer_preparation = None;
        match result {
            Ok(resolved) => {
                self.reviewer_generation = resolved.generation;
                self.confirm_reviewer();
                if let Some(view) = self.state.second_opinion_mut() {
                    view.set_status(format!(
                        "{} · asking the planner for context…",
                        resolved.description()
                    ));
                }
            }
            Err(error) => {
                if let Some(view) = self.state.second_opinion_mut() {
                    view.report_failure(error);
                }
            }
        }
    }

    /// Persists the open review so a UI restart can pick it back up.
    pub(crate) fn persist_review(&self) {
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion() else {
            return;
        };
        let stored = mj_core::storage::StoredReview {
            workflow: review.workflow.clone(),
            generation: self.reviewer_generation,
            context_baseline: review.context_baseline,
            native_lost: false,
            reviewer_transcript: review.reviewer.transcript(),
        };
        if let Some(persistence) = &self.persistence {
            if let Err(error) = persistence.send(ChatDaemonRequest::SaveReview {
                session_id: self.session.session_id().to_owned(),
                review: stored,
            }) {
                tracing::warn!(%error, "could not queue the open review for persistence");
            }
        } else {
            self.state.notices.set("Review persistence is unavailable");
        }
    }

    /// Forgets a review that has finished, so nothing is restored for it.
    pub(crate) fn forget_review(&self) {
        if let Some(persistence) = &self.persistence {
            if let Err(error) = persistence.send(ChatDaemonRequest::ClearReview {
                session_id: self.session.session_id().to_owned(),
            }) {
                tracing::warn!(%error, "could not queue the finished review for persistence");
            }
        } else {
            self.state.notices.set("Review persistence is unavailable");
        }
    }

    /// The reviewer is ready; consume the plan decision and gather context.
    pub(crate) fn confirm_reviewer(&mut self) {
        let Some(view) = self.state.second_opinion() else {
            return;
        };
        let captured = view.captured().clone();
        let command_id = self.state.next_second_opinion_command_id("context");
        let (workflow, request) =
            ReviewWorkflow::start(captured.id(), captured.proposal.clone(), command_id.clone());
        let baseline = self.state.latest_seq();
        if let Some(view) = self.state.second_opinion_mut() {
            view.begin_review(workflow, "asking the planner for context…", baseline);
        }
        // The harness's decision is answered only now. Declining keeps plan
        // mode active, which is what lets the planner answer a context
        // question instead of starting to implement.
        queue_chat_remote_operation(
            self.remote.operations(),
            ChatRemoteOperation::RespondElicitation {
                request: captured.request.clone(),
                response: mj_core::acp::plan_review_keep_planning(),
                plan_followup: None,
            },
            &mut self.state,
        );
        self.persist_review();
        self.run_workflow_request(request);
        self.poll_reviewer_events();
    }

    pub(crate) fn run_workflow_request(&mut self, request: WorkflowRequest) {
        match request {
            WorkflowRequest::PromptPrimary { command_id, prompt } => {
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Prompt {
                        command_id,
                        text: prompt,
                        images: Vec::new(),
                    },
                    &mut self.state,
                );
            }
            WorkflowRequest::PromptReviewer { command_id, prompt } => {
                let session = self.session.clone();
                let updates = self.chat_io_tx.clone();
                tokio::spawn(async move {
                    let result = session
                        .reviewer(ReviewerAction::Submit {
                            command_id,
                            command: mj_core::relay::RelayCommand::Prompt {
                                prompt: vec![
                                    agent_client_protocol::schema::v1::ContentBlock::Text(
                                        agent_client_protocol::schema::v1::TextContent::new(prompt),
                                    ),
                                ],
                            },
                        })
                        .await
                        .map(|_| ())
                        .map_err(|error| format!("{error:#}"));
                    if let Err(error) = updates.send(ChatIoUpdate::ReviewerStarted(result)) {
                        tracing::debug!(%error, "reviewer prompt result dropped");
                    }
                });
            }
            WorkflowRequest::PauseReviewer => self.pause_reviewer(),
            WorkflowRequest::RestoreDecision { proposal, .. } => {
                // Gathering context consumed the harness's own approval, so
                // only Hel can put this decision back in front of the user.
                let restored = mj_core::acp::normalized_plan_review(
                    self.state.next_second_opinion_command_id("plan-review"),
                    &serde_json::json!({ "plan": proposal }),
                );
                self.state.restore_elicitation(restored);
            }
        }
    }

    pub(crate) fn pause_reviewer(&self) {
        let session = self.session.clone();
        tokio::spawn(async move {
            if let Err(error) = session.reviewer(ReviewerAction::Pause).await {
                tracing::debug!(error = %format!("{error:#}"), "pausing the reviewer failed");
            }
        });
    }

    /// Answers a form the reviewer's harness is waiting on.
    pub(crate) fn answer_reviewer(
        &mut self,
        role: Option<String>,
        elicitation_id: String,
        response: mj_core::elicitation::ElicitationResponse,
    ) {
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        let answered_role = role.clone();
        tokio::spawn(async move {
            let result = session
                .reviewer_as(
                    role,
                    ReviewerAction::RespondElicitation {
                        elicitation_id,
                        response,
                    },
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            if let Err(error) = updates.send(ChatIoUpdate::ReviewerStarted(result)) {
                tracing::debug!(%error, "reviewer form answer result dropped");
            }
        });
        // The answer unblocks that harness's turn, so keep reading its journal.
        match answered_role {
            Some(role) => self.poll_turn_review_role(&role),
            None => self.poll_reviewer_events(),
        }
    }

    /// Puts a form the reviewer is waiting on in front of the user, or answers
    /// it for them when it is not theirs to answer.
    pub(crate) fn surface_reviewer_elicitations(&mut self) {
        let pending: Vec<(Option<String>, mj_core::elicitation::ElicitationRequest)> =
            match (self.state.second_opinion(), self.state.turn_review()) {
                (Some(view), _) => view
                    .reviewer()
                    .map(|reviewer| {
                        reviewer
                            .pending_elicitations()
                            .iter()
                            .map(|request| (None, request.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
                (None, Some(review)) => review
                    .pending_elicitations()
                    .into_iter()
                    .map(|(role, request)| (Some(role), request))
                    .collect(),
                (None, None) => Vec::new(),
            };
        self.state.reconcile_reviewer_elicitation(&pending);
        for (role, request) in pending {
            // A reviewer's plan decision is the reviewer proposing work, not
            // the plan under review. It is never shown as the primary's
            // decision; the reviewer was asked to critique, not to implement,
            // so it is declined and its critique stands as the answer.
            if mj_core::acp::is_plan_review_id(&request.id) {
                self.answer_reviewer(role, request.id, mj_core::acp::plan_review_keep_planning());
                continue;
            }
            if self.state.reviewer_elicitation_open() {
                return;
            }
            if !self.state.show_review_role_elicitation(role, request) {
                return;
            }
        }
        self.apply_deferred_elicitation_draft();
    }

    /// Reads the reviewer's journal from where the pane left off.
    ///
    /// One sidecar serves both review views, so whichever is open supplies the
    /// cursor; they are mutually exclusive by construction.
    pub(crate) fn poll_reviewer_events(&self) {
        let Some(reviewer) = self
            .state
            .second_opinion()
            .and_then(SecondOpinion::reviewer)
        else {
            return;
        };
        let after_ordinal = reviewer.cursor_ordinal;
        let after_digest = if reviewer.cursor_digest.is_empty() {
            mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            reviewer.cursor_digest.clone()
        };
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        tokio::spawn(async move {
            let result = match session
                .reviewer(ReviewerAction::Attach {
                    after_ordinal,
                    after_digest,
                })
                .await
            {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                Err(error) => Err(format!("{error:#}")),
            };
            if let Err(error) = updates.send(ChatIoUpdate::ReviewerEvents { result }) {
                tracing::debug!(%error, "reviewer events dropped because the chat closed");
            }
        });
    }

    /// Reads one reviewing role's journal from where its pane left off.
    ///
    /// Each role has its own relay, so each is polled on its own cursor; a
    /// lane's transcript never arrives in the supervisor's pane.
    pub(crate) fn poll_turn_review_role(&self, role: &str) {
        self.poll_turn_review_role_after(role, Duration::ZERO);
    }

    /// The same, after `delay`.
    ///
    /// An attach answers at once even when the journal has not moved, so a
    /// loop that re-attaches on every empty page is a spin. A review runs
    /// several roles at once, so each idle role waits a beat before asking
    /// again; a page that did carry events is followed up immediately, which
    /// is what keeps a streaming answer smooth.
    pub(crate) fn poll_turn_review_role_after(&self, role: &str, delay: Duration) {
        let Some(review) = self.state.turn_review() else {
            return;
        };
        let (after_ordinal, cursor_digest) = review.cursor(role);
        let after_digest = if cursor_digest.is_empty() {
            mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            cursor_digest
        };
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        let role = role.to_owned();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = match session
                .reviewer_as(
                    Some(role.clone()),
                    ReviewerAction::Attach {
                        after_ordinal,
                        after_digest,
                    },
                )
                .await
            {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                Err(error) => Err(format!("{error:#}")),
            };
            if let Err(error) = updates.send(ChatIoUpdate::TurnReviewEvents { role, result }) {
                tracing::debug!(%error, "review events dropped because the chat closed");
            }
        });
    }

    /// Folds one reviewing role's events into its pane, and keeps reading.
    ///
    /// Display only: the daemon reads the same journals to drive the review,
    /// and nothing here advances it. Two readers on one journal are safe --
    /// an attach is a read at a cursor, and only the host acknowledges.
    pub(crate) fn apply_turn_review_role_events(
        &mut self,
        role: String,
        result: std::result::Result<Vec<mj_core::relay::RelayEvent>, String>,
    ) {
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                // A journal this terminal cannot read is a display problem,
                // not a review problem: the daemon is still running it.
                tracing::debug!(%role, %error, "could not read a reviewing role's journal");
                self.report_review_refusal(error);
                return;
            }
        };
        let idle = events.is_empty();
        let session_id = review_role_session_id(self.session.session_id(), &role);
        if let Some(review) = self.state.turn_review_mut()
            && !events.is_empty()
        {
            review.pane(&role).apply_events(&session_id, &events);
        }
        self.surface_reviewer_elicitations();
        if self.state.turn_review().is_none() {
            self.reviewed_roles.remove(&role);
            return;
        }
        if !self
            .state
            .turn_review()
            .is_some_and(|review| review.role_is_active(&role))
        {
            // A verdict retains role rows for navigation, but their harnesses
            // have already been paused and need no further journal attaches.
            self.reviewed_roles.remove(&role);
            return;
        }
        self.poll_turn_review_role_after(
            &role,
            if idle {
                REVIEW_POLL_IDLE_INTERVAL
            } else {
                Duration::ZERO
            },
        );
    }

    pub(crate) fn apply_reviewer_events(
        &mut self,
        result: std::result::Result<Vec<mj_core::relay::RelayEvent>, String>,
    ) {
        let session_id = reviewer_session_id(self.session.session_id());
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                if let Some(view) = self.state.second_opinion_mut() {
                    view.report_failure(error);
                }
                return;
            }
        };
        let reviewer_changed = if !events.is_empty() {
            self.state
                .second_opinion_mut()
                .and_then(|view| view.reviewer_mut())
                .is_some_and(|reviewer| reviewer.apply_events(&session_id, &events))
        } else {
            false
        };
        if reviewer_changed
            && let Some(SecondOpinion::Review(review)) = self.state.second_opinion_mut()
            && let Some(answer) = review.reviewer.latest_answer()
            && let mj_core::second_opinion::ReviewStage::Reviewing { command_id } =
                review.workflow.stage().clone()
        {
            review.workflow.reviewer_turn_completed(&command_id, answer);
        }
        let finished = self.state.second_opinion().is_some_and(
            |view| matches!(view, SecondOpinion::Review(review) if review.workflow.finished()),
        );
        if let Some(view) = self.state.second_opinion_mut()
            && view.reviewer().is_some_and(|reviewer| !reviewer.is_empty())
        {
            view.set_status("Enter to act · Tab to choose");
        }
        self.persist_review();
        self.surface_reviewer_elicitations();
        if !finished {
            self.poll_reviewer_events();
        }
    }
}
