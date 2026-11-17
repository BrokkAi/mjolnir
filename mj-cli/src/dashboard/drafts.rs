use super::*;

/// Transfer the latest local draft into both sides of an asynchronous attach:
/// the chat being prepared and the composer the person can edit meanwhile.
pub(super) fn prepare_attach_draft(
    cache: &mut ComposerDraftCache,
    dashboard: &mut DashboardState,
    session: &mj_core::state::SessionRecord,
) -> String {
    if let Some(text) = dashboard.standby_prompt_draft(&session.id) {
        cache.capture(&session.id, text, &session.draft_input);
    }
    let draft = cache.open(&session.id, &session.draft_input).text;
    if !dashboard.has_standby_composer(&session.id) {
        dashboard.seed_standby_prompt(&session.id, draft.clone());
    }
    draft
}

/// Standby owns the composer while attaching; otherwise the warm chat owns it.
/// In particular an empty standby must win over an older warm draft.
pub(super) fn capture_editor_draft(
    cache: &mut ComposerDraftCache,
    dashboard: &DashboardState,
    chats: &BTreeMap<String, mj_chat::chat::ActiveChat>,
    session_id: &str,
    inherited: &str,
) {
    if let Some(text) = dashboard
        .standby_prompt_draft(session_id)
        .or_else(|| chats.get(session_id).map(|chat| chat.draft()))
    {
        cache.capture(session_id, text, inherited);
    }
}

/// Explicit notification navigation survives an asynchronous attach, but is
/// consumed only after the target is anchored or a failure has been reported.
pub(super) fn apply_notification_position(
    positions: &mut BTreeMap<String, mj_tui::NotificationPosition>,
    chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>,
    dashboard: &DashboardState,
    session_id: &str,
) -> Option<bool> {
    use mj_tui::NotificationPosition;
    let chat = chats.get_mut(session_id)?;
    let target = positions.get(session_id)?;
    let target = dashboard.resolve_notification_position(session_id, target);
    let located = match target {
        NotificationPosition::Message(id)
        | NotificationPosition::History { message_id: id, .. } => {
            if !chat.restore_transcript_message(&id) {
                return None;
            }
            true
        }
        NotificationPosition::End => {
            chat.restore_transcript_position(mj_chat::chat::TranscriptPosition::bottom());
            false
        }
        NotificationPosition::LatestAgentMessage | NotificationPosition::Question(_) => {
            return None;
        }
    };
    positions.remove(session_id);
    Some(located)
}

pub(super) fn notification_history_items(chat: &mj_chat::chat::ChatState, position: u64) -> u64 {
    let latest = chat
        .entries()
        .last()
        .map_or(position, |entry| entry.start_seq)
        .max(position);
    // Eight leading items put the question's immediate context above the
    // jump. The count spans target to latest; it is what this jump needs,
    // not a history cap.
    latest
        .saturating_sub(position)
        .saturating_add(1)
        .saturating_add(8)
}

fn clear_notification_loading_notice(dashboard: &mut DashboardState) {
    if matches!(
        dashboard.notice().as_deref(),
        Some(
            "Loading notification message…"
                | "Waiting for the notification message locator…"
                | "loading earlier history…"
        )
    ) {
        dashboard.clear_notice();
    }
}

/// Completes the same pending jump after background conversion. A successful
/// read is insufficient: the installed chat must actually anchor its target.
pub(super) fn finish_notification_history(
    positions: &mut BTreeMap<String, mj_tui::NotificationPosition>,
    chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>,
    dashboard: &mut DashboardState,
    session_id: &str,
    message_id: &str,
    result: std::result::Result<mj_chat::chat::TranscriptSnapshot, String>,
) -> Option<bool> {
    let target = dashboard.resolve_notification_position(session_id, positions.get(session_id)?);
    let matches = match target {
        mj_tui::NotificationPosition::Message(id)
        | mj_tui::NotificationPosition::History { message_id: id, .. } => id == message_id,
        _ => false,
    };
    if !matches {
        return None;
    }
    let chat = chats.get_mut(session_id)?;
    let error = match result {
        Ok(snapshot) => {
            chat.hydrate_transcript_history(snapshot);
            if apply_notification_position(positions, chats, dashboard, session_id) == Some(true) {
                clear_notification_loading_notice(dashboard);
                return Some(true);
            }
            "the stored history does not contain the target".to_owned()
        }
        Err(error) => error,
    };
    chats
        .get_mut(session_id)?
        .restore_transcript_position(mj_chat::chat::TranscriptPosition::bottom());
    positions.remove(session_id);
    dashboard.set_notice(format!(
        "Could not load that message: {error}. Showing the end of the conversation."
    ));
    Some(false)
}

impl DashboardContext {
    /// Opens the conversation for `session_id` without waiting on the session
    /// manager. Attaching can involve worker/relay I/O, so the result comes
    /// back through the dashboard I/O channel and the surface stays responsive
    /// while it is in flight.
    ///
    /// Cache pending question forms from the warm chat before it is replaced.
    /// The composer itself is tracked by [`ComposerDraftCache`].
    pub(crate) fn save_question_draft(&mut self, session_id: &str) {
        let Some(chat) = self.chats.get(session_id) else {
            return;
        };
        let session_id = session_id.to_owned();
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

    /// Consume an explicit navigation request only when a chat is ready. Both
    /// the warm-chat path and asynchronous attach completion use this owner.
    pub(crate) fn apply_notification_position(&mut self, session_id: &str) {
        let result = apply_notification_position(
            &mut self.notification_positions,
            &mut self.chats,
            &self.dashboard,
            session_id,
        );
        if result == Some(false) {
            self.dashboard.set_notice("The notification message could not be located; showing the end of the conversation.");
            return;
        }
        if let Some(target) = self.notification_positions.get(session_id) {
            let target = self
                .dashboard
                .resolve_notification_position(session_id, target);
            if let mj_tui::NotificationPosition::Message(id) = target
                && let Some(result) = self
                    .dashboard
                    .notification_message_position(session_id, &id)
            {
                self.notification_item_located(session_id, &id, result);
                return;
            }
            let notice = match self.notification_positions.get(session_id).unwrap() {
                mj_tui::NotificationPosition::Question(_) => {
                    "Waiting for the notification message locator…"
                }
                mj_tui::NotificationPosition::Message(_)
                | mj_tui::NotificationPosition::History { .. } => "loading earlier history…",
                _ => "Loading notification message…",
            };
            self.dashboard.set_notice(notice);
        } else {
            clear_notification_loading_notice(&mut self.dashboard);
        }
    }

    pub(super) fn notification_history_requests(
        &self,
    ) -> BTreeMap<super::summary_reads::ReadKey, u64> {
        use super::summary_reads::ReadKey;
        self.notification_positions
            .iter()
            .filter_map(|(session_id, target)| {
                let chat = self.chats.get(session_id)?;
                match self
                    .dashboard
                    .resolve_notification_position(session_id, target)
                {
                    mj_tui::NotificationPosition::Message(id) => {
                        Some((ReadKey::Item(session_id.clone(), id), 0))
                    }
                    mj_tui::NotificationPosition::History {
                        message_id,
                        position,
                    } => {
                        let items = notification_history_items(chat, position);
                        Some((ReadKey::History(session_id.clone(), message_id), items))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    pub(crate) fn notification_item_located(
        &mut self,
        session_id: &str,
        message_id: &str,
        result: std::result::Result<u64, String>,
    ) {
        let Some(target) = self.notification_positions.get(session_id) else {
            return;
        };
        if self
            .dashboard
            .resolve_notification_position(session_id, target)
            != mj_tui::NotificationPosition::Message(message_id.to_owned())
        {
            return;
        }
        match result {
            Ok(position) => {
                self.notification_positions.insert(
                    session_id.to_owned(),
                    mj_tui::NotificationPosition::History {
                        message_id: message_id.to_owned(),
                        position,
                    },
                );
                self.apply_notification_position(session_id);
            }
            Err(error) => {
                finish_notification_history(
                    &mut self.notification_positions,
                    &mut self.chats,
                    &mut self.dashboard,
                    session_id,
                    message_id,
                    Err(error),
                );
            }
        }
    }

    /// Enters an existing pin or opens the session in Browse.
    /// The replaced view is saved and the new chat attaches in the background.
    pub(crate) fn open_chat_session(&mut self, session_id: &str) {
        let pane = self
            .dashboard
            .pane_for_session(session_id)
            .unwrap_or(self.dashboard.browse_pane());
        self.dashboard.reveal_pane(pane);
        self.dashboard.focus_pane(pane);
        self.open_chat_session_into(pane, session_id);
    }

    pub(crate) fn open_chat_session_into(&mut self, pane: PaneId, session_id: &str) {
        if !self.dashboard.has_conversation_pane(pane) {
            return;
        }
        let native = self.dashboard.is_native_agent(session_id);
        if !native && !self.session_in_active_workspace(session_id) {
            return;
        }
        let outgoing = self
            .dashboard
            .pane_session(pane)
            .filter(|session| *session != session_id)
            .map(str::to_owned);
        if let Some(outgoing) = outgoing {
            self.selection.clear();
            self.park_chat_composer(&outgoing);
            // The pane is changing session. Save what the conversation
            // leaving it holds, then drop it unless another pane shows it —
            // which it cannot, since a session is in at most one pane.
            self.capture_composer_draft(&outgoing);
            self.save_question_draft(&outgoing);
            self.record_chat_detach(&outgoing);
            if self.dashboard.pane_for_session(&outgoing) == Some(pane) {
                self.chats.remove(&outgoing);
            }
        }
        self.dashboard.set_pane_session(pane, Some(session_id));
        let assignment = self.dashboard.pane_assignment(pane);
        if self.attachments.entry(pane).or_default().bind(assignment) {
            self.opening_chat_sessions.remove(&pane);
        }
        if native {
            self.cancel_chat_open_in(pane);
            return;
        }
        // A stopped sub-agent has no worker to attach to, and an attach would
        // wait out its timeout and leave the pane empty. Its conversation is
        // stored, so read that instead.
        if self.dashboard.is_stopped_subagent(session_id) {
            self.cancel_chat_open_in(pane);
            if self.dashboard.begin_stopped_subagent(session_id) {
                spawn_stopped_subagent_transcript(
                    session_id.to_owned(),
                    self.dashboard_io_tx.clone(),
                );
            }
            return;
        }
        self.capture_composer_draft(session_id);
        self.attachments.entry(pane).or_default().select(session_id);
        self.save_question_draft(session_id);
        // A lifecycle owns the row's conversation until its authoritative
        // completion. Do not start an attach that can arrive after Stop/Move
        // and put a retiring chat back on screen.
        if self.dashboard.transition_kind(session_id).is_some()
            || self.dashboard.transition_failure_kind(session_id).is_some()
            || self.dashboard.session_failed(session_id)
        {
            self.defer_chat_open_in(pane);
            return;
        }
        // A suspended session has no worker to attach to; an attach would
        // wait out its timeout and report that opening did not respond
        // (R4-11), whichever pane asked.
        if self.dashboard.pane_session_is_suspended(session_id) {
            self.release_suspended_pane(pane, session_id);
            return;
        }
        if self
            .chats
            .get(session_id)
            .is_some_and(mj_chat::chat::ActiveChat::session_feed_open)
        {
            self.cancel_chat_open_in(pane);
            if let Some(composer) = self.dashboard.take_standby_composer(session_id)
                && let Some(chat) = self.chats.get_mut(session_id)
            {
                chat.adopt_live_composer(composer);
                chat.keep_deferred_submission_as_draft();
            }
            self.apply_notification_position(session_id);
            self.acknowledge_visible_chats();
            return;
        }
        if self.opening_chat_sessions.get(&pane).map(String::as_str) == Some(session_id) {
            return;
        }
        self.cancel_chat_open_in(pane);
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
                session_record.listed_title().to_owned()
            },
            harness_kind: Some(session_record.harness_kind),
            subagent_count: self.dashboard.subagent_count_for(&session_record.id),
            reviewers: self.dashboard.reviewer_counts_for(&session_record.id),
        };
        let sessions = self.worker_commands_tx.clone();
        let notices = self.notices.clone();
        let updates = self.dashboard_io_tx.clone();
        let session_id = session_id.to_owned();
        let bundle_id = session_record.bundle_id.clone();
        if !self.dashboard.has_standby_composer(&session_id)
            && let Some(chat) = self.chats.get_mut(&session_id)
            && let Some(composer) = chat.take_live_composer()
        {
            self.dashboard.adopt_standby_composer(&session_id, composer);
        }
        // A draft typed while the session's transition ran belongs to this
        // composer now; it wins over the warm chat's older captured text.
        let draft = prepare_attach_draft(
            &mut self.composer_drafts,
            &mut self.dashboard,
            &session_record,
        );
        let context = mj_chat::chat::ChatSessionContext {
            config: self.controller.config.clone(),
            session: session_record,
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
                    mj_chat::chat::ChatDaemonRequest::StartTurnReview { session_id } => {
                        Some(session_id.clone())
                    }
                    _ => None,
                };
                let result = async {
                    let mut daemon = crate::daemon::connect_or_start().await?;
                    match request {
                        mj_chat::chat::ChatDaemonRequest::RecordAbandonedPrompt {
                            session_id,
                            bundle_id,
                            text,
                        } => {
                            daemon
                                .record_abandoned_prompt(session_id, bundle_id, text)
                                .await
                        }
                        mj_chat::chat::ChatDaemonRequest::StartTurnReview { session_id } => {
                            daemon.start_turn_review(session_id).await
                        }
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
        self.opening_chat_sessions.insert(pane, session_id.clone());
        self.sync_opening_session();
        let detach = self
            .dashboard
            .first_key_label(mj_tui::CommandId::QuitDetach)
            .map(|key| format!("; {key} quits"))
            .unwrap_or_default();
        self.dashboard.set_notice(format!(
            "Opening session{} Esc cancels; select another session to switch{detach}.",
            mj_chat::theme::glyphs().ellipsis
        ));
        let reported_session_id = session_id.clone();
        let attachment_session_id = session_id.clone();
        let assignment = assignment.expect("opening an assigned conversation pane");
        self.attachments.entry(pane).or_default().spawn(
            &attachment_session_id,
            attachment::ATTACH_TIMEOUT,
            async move {
                let managed = sessions
                    .wait_for_session(&session_id, attachment::ATTACH_TIMEOUT)
                    .await
                    .map_err(|error| format!("{error:#}"))?;
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
                        assignment,
                        pane,
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

    /// Remembers the completion request in flight. Only one is useful at a
    /// time, so a new one cancels the last.
    pub(crate) fn track_completion(&mut self, cancelled: Arc<AtomicBool>) {
        if let Some((_, previous)) = self
            .completion_job
            .replace((self.dashboard.path_input_context(), cancelled))
        {
            previous.store(true, Ordering::Release);
        }
    }

    pub(crate) fn cancel_stale_path_input(&mut self) {
        let context = self.dashboard.path_input_context();
        if self
            .path_input_job
            .as_ref()
            .is_some_and(|(job_context, _)| *job_context != context)
            && let Some((_, cancelled)) = self.path_input_job.take()
        {
            cancelled.store(true, Ordering::Release);
        }
        if self
            .completion_job
            .as_ref()
            .is_some_and(|(job_context, _)| *job_context != context)
            && let Some((_, cancelled)) = self.completion_job.take()
        {
            cancelled.store(true, Ordering::Release);
        }
    }
}
