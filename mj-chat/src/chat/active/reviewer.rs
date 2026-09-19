use super::*;

impl ActiveChat {
    /// Opens the reviewer waterfall for a captured plan.
    ///
    /// The harness's decision stays pending: it is answered only once a
    /// reviewer is running, because gathering context needs an idle planning
    /// session and cancelling before then must leave the decision intact.
    pub(crate) fn open_second_opinion(
        &mut self,
        request: mj_core::elicitation::ElicitationRequest,
        proposal: String,
    ) {
        let Some(context) = self.context.as_ref() else {
            self.state
                .set_notice("A second opinion needs this session's configuration");
            self.state.restore_elicitation(request);
            return;
        };
        let profiles = self.reviewer_profiles();
        if profiles.is_empty() {
            self.state
                .set_notice("Configure a second profile to review plans with");
            self.state.restore_elicitation(request);
            return;
        }
        let defaults = self.reviewer_defaults.clone();
        let workspace_id = context.session.workspace_id.clone();
        // A workspace that has already chosen a reviewer does not choose
        // again: the same reviewer resumes with its own conversation. The
        // waterfall reopens only when starting it that way fails.
        let remembered = defaults
            .profile(&workspace_id)
            .filter(|id| context.config.enabled_profile(id).is_some())
            .map(|profile_id| ReviewerSelection {
                profile_id: profile_id.to_owned(),
                model: remembered_value(defaults.model(&workspace_id, profile_id)),
                effort: remembered_value(
                    defaults.effort(
                        &workspace_id,
                        profile_id,
                        defaults
                            .model(&workspace_id, profile_id)
                            .unwrap_or(mj_core::second_opinion::HARNESS_DEFAULT_VALUE),
                    ),
                ),
            });
        let setup = ReviewerSetup::new(workspace_id, profiles, defaults);
        self.state
            .open_second_opinion(CapturedProposal { request, proposal }, setup);
        if let Some(selection) = remembered {
            if let Some(view) = self.state.second_opinion_mut() {
                view.set_status("resuming the reviewer…");
            }
            self.probe_reviewer(
                0,
                selection.profile_id.clone(),
                selection.model.clone(),
                selection.effort.clone(),
                false,
            );
            self.resuming_reviewer = Some(selection);
        }
    }

    /// Performs the steps the second-opinion view asked for.
    pub(crate) fn run_second_opinion(&mut self, intent: SecondOpinionIntent) {
        match intent {
            SecondOpinionIntent::Setup(requests) => {
                for request in requests {
                    self.run_setup_request(request);
                }
            }
            SecondOpinionIntent::Confirmed {
                profile_id,
                model,
                effort,
            } => self.confirm_reviewer(profile_id, model, effort),
            SecondOpinionIntent::Workflow(requests) => {
                // Every workflow batch that reaches here ends the review, so
                // the record goes before the steps run: a crash between them
                // must not restore a split whose feedback already went out.
                self.forget_review();
                for request in requests {
                    self.run_workflow_request(request);
                }
            }
            SecondOpinionIntent::Closed => {}
        }
    }

    pub(crate) fn run_setup_request(&mut self, request: SetupRequest) {
        match request {
            SetupRequest::Probe {
                generation,
                profile_id,
            } => self.probe_reviewer(generation, profile_id, None, None, false),
            SetupRequest::ApplyModel { generation, model } => {
                let Some(profile_id) = self.setup_profile_id() else {
                    return;
                };
                self.probe_reviewer(generation, profile_id, Some(model), None, true);
            }
            SetupRequest::CancelProbe { .. } => self.pause_reviewer(),
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
            self.state
                .feedback
                .set_failure("Review persistence is unavailable");
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
            self.state
                .feedback
                .set_failure("Review persistence is unavailable");
        }
    }

    pub(crate) fn setup_profile_id(&self) -> Option<String> {
        let SecondOpinion::Setup { setup, .. } = self.state.second_opinion()? else {
            return None;
        };
        setup
            .profiles()
            .get(setup.profile_index())
            .map(|profile| profile.id.clone())
    }

    /// Stages a profile and starts (or reconfigures) the reviewer under it,
    /// reporting the options it advertises back to the waterfall.
    pub(crate) fn probe_reviewer(
        &mut self,
        generation: u64,
        profile_id: String,
        model: Option<String>,
        effort: Option<String>,
        configuring: bool,
    ) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let reviewer_stager = context.reviewer_stager.clone();
        let config = context.config.clone();
        let session_record = context.session.clone();
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        // The reviewer's lifetime generation is what decides whether the
        // running reviewer can be kept; `generation` here only says which
        // probe this answer belongs to.
        let lifetime = self.reviewer_generation;
        tokio::spawn(async move {
            let staged = tokio::task::spawn_blocking(move || {
                reviewer_stager.stage(config, session_record, profile_id, lifetime)
            })
            .await;
            let result = async {
                let mut config = match staged {
                    Ok(Ok(config)) => config,
                    Ok(Err(error)) => return Err(format!("{error:#}")),
                    Err(error) => return Err(format!("staging the reviewer stopped: {error}")),
                };
                config.model = model;
                config.effort = effort;
                match session
                    .reviewer(ReviewerAction::Start {
                        config: Box::new(config),
                    })
                    .await
                {
                    Ok(ReviewerOutcome::Started(started)) => Ok(started.config_options),
                    Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                    Err(error) => Err(format!("{error:#}")),
                }
            }
            .await;
            let update = if configuring {
                ChatIoUpdate::ReviewerConfigured { generation, result }
            } else {
                ChatIoUpdate::ReviewerProbe { generation, result }
            };
            if let Err(error) = updates.send(update) {
                tracing::debug!(%error, "reviewer result dropped because the chat closed");
            }
        });
    }

    /// Confirms the chosen reviewer: remember it, answer the harness's own
    /// plan decision, and ask the planner for the context the reviewer needs.
    pub(crate) fn confirm_reviewer(
        &mut self,
        profile_id: String,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(view) = self.state.second_opinion() else {
            return;
        };
        let captured = view.captured().clone();
        if let Some(context) = self.context.as_ref() {
            let selection = ReviewerSelection {
                profile_id,
                model,
                effort,
            };
            self.reviewer_defaults
                .remember(&context.session.workspace_id, &selection);
            if let Some(persistence) = &self.persistence {
                if let Err(error) = persistence.send(ChatDaemonRequest::RememberReviewerSelection {
                    workspace_id: context.session.workspace_id.clone(),
                    selection,
                }) {
                    tracing::warn!(%error, "could not queue the reviewer choice for persistence");
                }
            } else {
                self.state
                    .set_notice("Reviewer choice persistence is unavailable");
            }
        }

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

    /// Reports what the reviewer advertises back to the waterfall.
    ///
    /// A result from a probe the user has moved past is dropped by the state
    /// machine, which also names the reviewer to stop, so a slow harness can
    /// never overwrite a newer selection.
    pub(crate) fn apply_reviewer_options(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<SessionConfigOption>, String>,
        configuring: bool,
    ) {
        // A resumed review never shows the waterfall: the choice was already
        // made, so a successful start goes straight to the review and only a
        // failure falls back to asking again.
        if let Some(selection) = self.resuming_reviewer.clone() {
            match result {
                Ok(_) => {
                    self.resuming_reviewer = None;
                    self.confirm_reviewer(selection.profile_id, selection.model, selection.effort);
                    return;
                }
                Err(error) => {
                    self.resuming_reviewer = None;
                    if let Some(view) = self.state.second_opinion_mut() {
                        view.report_failure(format!(
                            "the remembered reviewer could not start: {error}"
                        ));
                    }
                    return;
                }
            }
        }
        let Some(SecondOpinion::Setup { setup, .. }) = self.state.second_opinion_mut() else {
            return;
        };
        let stale = match result {
            Ok(options) if configuring => setup.model_applied(generation, &options),
            Ok(options) => setup.probe_succeeded(generation, &options),
            Err(error) => {
                setup.probe_failed(generation, error);
                None
            }
        };
        if let Some(request) = stale {
            self.run_setup_request(request);
        }
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

    /// Folds a page of reviewer events into the pane and keeps reading.
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
