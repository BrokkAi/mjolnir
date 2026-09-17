use super::*;

impl DashboardContext {
    /// Opens the conversation for `session_id` without waiting on the session
    /// manager. Attaching can involve worker/relay I/O, so the result comes
    /// back through the dashboard I/O channel and the surface stays responsive
    /// while it is in flight.
    ///
    /// Cache pending question forms from the warm chat before it is replaced.
    /// The composer itself is tracked by [`ComposerDraftCache`].
    pub(crate) fn save_active_question_draft(&mut self) {
        let Some(chat) = self.active_chat.as_ref() else {
            return;
        };
        let session_id = chat.session_id().to_owned();
        let drafts = chat.elicitation_drafts();
        if !drafts.is_empty() {
            let captured_event_ordinal = chat.latest_event_ordinal();
            self.question_drafts.insert(
                session_id,
                drafts
                    .into_iter()
                    .map(|draft| CachedQuestionDraft {
                        draft,
                        captured_event_ordinal,
                    })
                    .collect(),
            );
        } else {
            // A request was answered or removed while this session was away;
            // retaining its old entry would make a later attach look like a
            // new pending request.
            self.question_drafts.remove(&session_id);
        }
    }

    pub(crate) fn reconcile_question_drafts(&mut self) {
        self.question_drafts.retain(|session_id, drafts| {
            if !self.controller.state.sessions.contains_key(session_id) {
                return false;
            }
            let Some((projection_ordinal, pending)) =
                self.dashboard.pending_elicitations(session_id)
            else {
                // A startup summary has no complete pending-request list yet;
                // keep the cache until its accepted full projection arrives.
                return true;
            };
            drafts.retain(|draft| {
                if !question_draft_projection_is_current(
                    draft.captured_event_ordinal,
                    projection_ordinal,
                ) {
                    // The active feed may have captured a newer local form
                    // than the dashboard's last accepted full projection.
                    return true;
                }
                // Reviewer requests live in the sidecar review projection,
                // not in the primary materialized session. The attached chat
                // validates those by source/role when it is restored.
                draft.draft.reviewer() || pending.iter().any(|request| draft.draft.matches(request))
            });
            !drafts.is_empty()
        });
    }

    pub(crate) fn restore_question_draft(
        &mut self,
        session_id: &str,
        chat: &mut mj_chat::chat::ActiveChat,
    ) {
        let Some(drafts) = self.question_drafts.remove(session_id) else {
            return;
        };
        // ActiveChat defers reviewer restoration when the reviewer stream has
        // not surfaced its form yet. A false result here means the projection
        // has no matching pending request, so retaining the entry would allow
        // a stale answer to return on a later attach.
        chat.restore_elicitation_drafts(drafts.into_iter().map(|cached| cached.draft).collect());
    }

    pub(crate) fn open_chat_session(&mut self, session_id: &str) {
        if !self.session_in_active_workspace(session_id) {
            return;
        }
        if self
            .active_chat
            .as_ref()
            .is_some_and(|chat| chat.session_id() != session_id)
        {
            self.selection.clear();
        }
        // The warm chat remains alive while another session attaches. Capture
        // its current composer before any background snapshot can arrive.
        self.capture_active_composer_draft();
        self.dashboard.select_active_session(session_id);
        self.attachment.select(session_id);
        self.save_active_question_draft();
        // A lifecycle owns the row's conversation until its authoritative
        // completion. Do not start an attach that can arrive after Stop/Move
        // and put a retiring chat back on screen.
        if self.dashboard.transition_kind(session_id).is_some()
            || self.dashboard.transition_failure_kind(session_id).is_some()
        {
            self.defer_chat_open();
            self.dashboard.set_current_session(None);
            return;
        }
        if self
            .active_chat
            .as_ref()
            .is_some_and(|chat| chat.session_id() == session_id && chat.session_feed_open())
        {
            self.cancel_chat_open();
            self.dashboard.set_current_session(Some(session_id));
            self.acknowledge_visible_chat();
            return;
        }
        if self.opening_chat_session.as_deref() == Some(session_id) {
            return;
        }
        self.cancel_chat_open();
        let Some(session_record) = self.controller.state.sessions.get(session_id).cloned() else {
            self.dashboard.set_notice(format!(
                "Could not open session: unknown session {session_id}"
            ));
            return;
        };
        let header = mj_chat::chat::SessionHeaderIdentity {
            target: self
                .controller
                .state
                .project_identity_session(&session_record)
                .project_target(&self.controller.config, &session_record.target_template_id),
            profile: session_record.last_profile.clone(),
            title: if self.dashboard.go_mode().is_some() {
                self.dashboard.go_conversation_title(session_id)
            } else {
                session_record.display_title().to_owned()
            },
            harness_kind: Some(session_record.harness_kind),
            subagent_count: self
                .controller
                .state
                .subagents
                .values()
                .filter(|record| record.parent_session_id == session_record.id)
                .count(),
        };
        let sessions = self.worker_commands_tx.clone();
        let notices = self.notices.clone();
        let updates = self.dashboard_io_tx.clone();
        let session_id = session_id.to_owned();
        let bundle_id = session_record.bundle_id.clone();
        // A draft typed while the session's transition ran belongs to this
        // composer now; it wins over the warm chat's older captured text.
        if let Some(text) = self.dashboard.take_standby_prompt_draft(&session_id) {
            self.composer_drafts
                .capture(&session_id, text, &session_record.draft_input);
        }
        let draft = self
            .composer_drafts
            .open(&session_id, &session_record.draft_input)
            .text;
        let context = mj_chat::chat::ChatSessionContext {
            config: self.controller.config.clone(),
            session: session_record,
            reviewer_stager: mj_controller::controller::reviewer_stager(),
        };
        let (persistence_tx, mut persistence_rx) =
            tokio::sync::mpsc::unbounded_channel::<mj_chat::chat::ChatDaemonRequest>();
        let refusals = self.dashboard_io_tx.clone();
        tokio::spawn(async move {
            while let Some(request) = persistence_rx.recv().await {
                // A review action's refusal is a sentence for the person who
                // pressed the key, so it comes back to the chat rather than
                // only into the log.
                let refusal_session = match &request {
                    mj_chat::chat::ChatDaemonRequest::StartTurnReview { session_id }
                    | mj_chat::chat::ChatDaemonRequest::ResolveTurnReview { session_id, .. } => {
                        Some(session_id.clone())
                    }
                    _ => None,
                };
                let result = async {
                    let mut daemon = crate::daemon::connect_or_start().await?;
                    match request {
                        mj_chat::chat::ChatDaemonRequest::SaveReview { session_id, review } => {
                            daemon.save_active_review(session_id, review).await
                        }
                        mj_chat::chat::ChatDaemonRequest::ClearReview { session_id } => {
                            daemon.clear_active_review(session_id).await
                        }
                        mj_chat::chat::ChatDaemonRequest::RememberReviewerSelection {
                            workspace_id,
                            selection,
                        } => {
                            daemon
                                .remember_reviewer_selection(workspace_id, selection)
                                .await
                        }
                        mj_chat::chat::ChatDaemonRequest::StartTurnReview { session_id } => {
                            daemon.start_turn_review(session_id).await
                        }
                        mj_chat::chat::ChatDaemonRequest::ResolveTurnReview {
                            session_id,
                            resolution,
                        } => daemon.resolve_turn_review(session_id, resolution).await,
                    }
                }
                .await;
                if let (Err(error), Some(session_id)) = (&result, refusal_session) {
                    report(
                        "persisting chat state",
                        &refusals,
                        DashboardIoUpdate::ReviewRefused {
                            session_id,
                            message: format!("{error:#}"),
                        },
                    );
                }
                if let Err(error) = result {
                    tracing::warn!(%error, "could not persist chat state through the daemon");
                }
            }
        });
        self.opening_chat_session = Some(session_id.clone());
        self.dashboard.set_current_session(Some(&session_id));
        self.dashboard.select_active_session(&session_id);
        self.dashboard.set_opening_session(Some(&session_id));
        self.dashboard.set_notice(
            "Opening session… Esc cancels; select another session to switch; Alt-Q quits.",
        );
        let reported_session_id = session_id.clone();
        let attachment_session_id = session_id.clone();
        self.attachment.spawn(
            &attachment_session_id,
            attachment::ATTACH_TIMEOUT,
            async move {
                let managed = sessions
                    .wait_for_session(&session_id, attachment::ATTACH_TIMEOUT)
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                let review_state = managed
                    .client()
                    .review_state()
                    .await
                    .map_err(|error| format!("{error:#}"));
                tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_blocking(move || {
                    mj_chat::chat::ActiveChat::prepare_with_persistence(
                        managed.client(),
                        &bundle_id,
                        Some(context),
                        sessions.client(),
                        header,
                        draft,
                        notices,
                        Some(persistence_tx),
                    )
                    .with_review_state(review_state)
                }))
                .await
                .map_err(|error| format!("chat preparation task failed: {error}"))
            },
            move |generation, result| {
                report(
                    "opening a session",
                    &updates,
                    DashboardIoUpdate::ChatOpened {
                        generation,
                        session_id: reported_session_id.clone(),
                        result: Box::new(result),
                    },
                );
            },
        );
    }

    /// Tells every operation still in flight to stop. Cancellation is
    /// cooperative, so this only requests it.
    pub(crate) fn track_path_input(&mut self, cancelled: Arc<AtomicBool>) {
        if let Some((_, previous)) = self
            .path_input_job
            .replace((self.dashboard.path_input_context(), cancelled))
        {
            previous.store(true, Ordering::Release);
        }
    }

    pub(crate) fn cancel_stale_path_input(&mut self) {
        if self
            .path_input_job
            .as_ref()
            .is_some_and(|(context, _)| *context != self.dashboard.path_input_context())
            && let Some((_, cancelled)) = self.path_input_job.take()
        {
            cancelled.store(true, Ordering::Release);
        }
    }
}
