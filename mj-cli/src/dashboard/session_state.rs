use super::*;

impl DashboardContext {
    /// Keeps one projection per session in flight and remembers only the
    /// newest snapshot that arrived behind it. The shared permits bound work
    /// across different sessions as well.
    /// Fill a freshly resumed conversation from the tail of its stored
    /// transcript, off the event loop.
    ///
    /// The projection is durable the moment the resume completes, so nothing
    /// has to travel back through the daemon reply to show it. The view keeps
    /// only the last `TAIL_SEED_ITEMS` entries, so the seed reads exactly that
    /// many rather than the whole history. The poller delivers the complete
    /// projection a moment later; this exists so the conversation is not blank
    /// until it does.
    pub(crate) fn request_transcript_tail_seed(&mut self, session_id: &str) {
        let session_id = session_id.to_owned();
        let updates = self.dashboard_io_tx.clone();
        tokio::spawn(async move {
            let seeded = tokio::task::spawn_blocking({
                let session_id = session_id.clone();
                move || {
                    mj_controller::database::load_materialized_projection_tail(
                        &session_id,
                        mj_chat::chat::TAIL_SEED_ITEMS,
                    )
                }
            })
            .await;
            // A failed seed costs a blank conversation until the next poll,
            // which is where the full projection comes from anyway. It is not
            // worth failing a resume that otherwise succeeded.
            let materialized = match seeded {
                Ok(Ok(Some((materialized, _)))) => materialized,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    tracing::debug!(%session_id, %error, "transcript tail seed failed");
                    return;
                }
                Err(error) => {
                    tracing::debug!(%session_id, %error, "transcript tail seed task failed");
                    return;
                }
            };
            report(
                "seeding the transcript tail",
                &updates,
                DashboardIoUpdate::TranscriptTailSeed {
                    materialized: Box::new(materialized),
                },
            );
        });
    }

    pub(crate) fn request_materialized_projection(
        &mut self,
        materialized: MaterializedSession,
        viewed_through_event_ordinal: u64,
    ) {
        let Some((materialized, viewed_through_event_ordinal)) = enqueue_materialized_projection(
            &mut self.materialized_projections_in_flight,
            &mut self.pending_materialized_projections,
            materialized,
            viewed_through_event_ordinal,
        ) else {
            return;
        };

        let session_id = materialized.session_id.clone();
        let previous = self.dashboard.take_projection_cache(&session_id);
        spawn_materialized_session_projection(
            materialized,
            viewed_through_event_ordinal,
            previous,
            self.dashboard_io_tx.clone(),
            Arc::clone(&self.materialized_projection_permits),
        );
    }

    pub(crate) fn hydrate_stored_session_summaries(&mut self) {
        let sessions = self
            .controller
            .state
            .sessions
            .values()
            .filter(|session| session.state.is_active())
            .map(|session| (session.id.clone(), session.viewed_through_event_ordinal))
            .collect::<Vec<_>>();
        // The startup pick compares recorded activity, which is exactly what
        // these summaries carry, so it waits for them.
        self.startup = StartupSession::begin(
            sessions
                .iter()
                .filter(|(id, _)| self.session_in_active_workspace(id))
                .map(|(id, _)| id.clone()),
            std::time::Instant::now(),
        );
        for (session_id, viewed_through_event_ordinal) in sessions {
            spawn_stored_session_summary(
                session_id,
                viewed_through_event_ordinal,
                self.dashboard_io_tx.clone(),
            );
        }
    }

    /// Records that one live session's stored summary has arrived, however it
    /// turned out, and opens the startup conversation once they all have.
    pub(crate) fn finish_startup_summary(&mut self, session_id: &str) {
        self.startup.summary_arrived(session_id);
        self.maybe_open_startup_session();
    }

    /// Follow a changed selection once. A failed open needs an explicit retry,
    /// rather than another attempt on every render or background completion.
    pub(crate) fn follow_selected_session(&mut self) {
        let Some(selected) = self.dashboard.selected_session_id().map(str::to_owned) else {
            return;
        };
        if self.dashboard.transition_kind(&selected).is_some()
            || self.dashboard.transition_failure_kind(&selected).is_some()
        {
            self.defer_chat_open();
            return;
        }
        if self.attachment.select(&selected) {
            self.open_chat_session(&selected);
        }
    }

    pub(crate) fn cancel_chat_open(&mut self) {
        self.attachment.cancel();
        self.opening_chat_session = None;
        self.dashboard.set_opening_session(None);
    }

    pub(crate) fn defer_chat_open(&mut self) {
        self.attachment.defer();
        self.opening_chat_session = None;
        self.dashboard.set_opening_session(None);
    }

    /// The warm chat when it belongs on screen.
    ///
    /// While an attach for a different session is in flight, the chat still
    /// loaded is the one the selection has moved off. Drawing it under the new
    /// row's highlight, or handing it the keyboard, would report the wrong
    /// conversation, so it stays hidden until selected again. Its feeds keep
    /// running while an attach is pending, failed, or cancelled.
    pub(crate) fn visible_chat(&mut self) -> Option<&mut mj_chat::chat::ActiveChat> {
        let Self {
            active_chat,
            opening_chat_session,
            dashboard,
            ..
        } = self;
        let opening = opening_chat_session.as_deref();
        active_chat.as_mut().filter(|chat| {
            // A launch standby stands in front of the conversation that was
            // selected when the creation started, so typing meant for the new
            // session cannot land in the old one.
            !dashboard.launch_standby_capturing()
                && chat_is_visible(opening, chat.session_id())
                && dashboard.selected_session_id() == Some(chat.session_id())
                && dashboard.transition_kind(chat.session_id()).is_none()
                && dashboard
                    .transition_failure_kind(chat.session_id())
                    .is_none()
        })
    }

    /// The dashboard and the conversation on screen, borrowed together, so a
    /// caller can act on one and report through the other.
    pub(crate) fn dashboard_and_visible_chat(
        &mut self,
    ) -> (&mut DashboardState, Option<&mut mj_chat::chat::ActiveChat>) {
        let visible = self.visible_chat().is_some();
        let Self {
            dashboard,
            active_chat,
            ..
        } = self;
        (dashboard, active_chat.as_mut().filter(|_| visible))
    }

    pub(crate) fn needs_animation(&mut self) -> bool {
        self.dashboard.needs_fast_tick()
            || self
                .visible_chat()
                .is_some_and(|chat| chat.needs_animation())
    }

    /// The user took the choice into their own hands, so the surface stops
    /// trying to pick a conversation for them.
    pub(crate) fn cancel_startup_session(&mut self) {
        self.startup.cancel();
    }

    pub(crate) fn refresh_go_context(&mut self) {
        if self.dashboard.go_mode().is_none() || self.go_context_in_flight {
            return;
        }
        let Some(session_id) = self.dashboard.selected_session_id().map(str::to_owned) else {
            return;
        };
        if self
            .go_context_refresh
            .as_ref()
            .is_some_and(|(id, refreshed)| {
                id == &session_id && refreshed.elapsed() < Duration::from_secs(5)
            })
        {
            return;
        }
        self.go_context_in_flight = true;
        self.go_context_refresh = Some((session_id.clone(), std::time::Instant::now()));
        let report_id = session_id.clone();
        io::spawn_io(
            "reading session working context",
            self.dashboard_io_tx.clone(),
            move || {
                let executor = mj_controller::targets::CancellableProcessExecutor::with_timeout(
                    Duration::from_secs(3),
                );
                Controller::load()?.session_working_context(&session_id, &executor)
            },
            move |result| io::DashboardIoUpdate::GoContext {
                session_id: report_id,
                result,
            },
        );
    }

    pub(crate) fn remember_go_selection(&mut self) {
        if self.go_selection_in_flight {
            return;
        }
        let Some(go) = self.dashboard.go_mode() else {
            return;
        };
        let Some(session_id) = self.dashboard.selected_session_id().map(str::to_owned) else {
            return;
        };
        if self.go_selection_requested.as_ref() == Some(&session_id) {
            return;
        }
        let Some(workspace_id) = go.workspace_id.clone() else {
            return;
        };
        if self.dashboard.active_workspace_id() != Some(workspace_id.as_str()) {
            return;
        }
        let directory = go.directory.clone();
        self.go_selection_requested = Some(session_id.clone());
        self.go_selection_in_flight = true;
        io::spawn_critical_io(
            self.critical_operations.clone(),
            "remembering conversation",
            self.dashboard_io_tx.clone(),
            move || {
                mj_core::go::GoPreferences::remember_session(
                    &mj_core::go::GoPreferences::path(),
                    &directory,
                    &workspace_id,
                    session_id,
                )
            },
            io::DashboardIoUpdate::GoSelectionSaved,
        );
    }

    /// Opens the conversation the surface should start on, once the summaries
    /// it compares have arrived or the wait for them has run out.
    /// Reports whether the pick ran, which happens at most once.
    pub(crate) fn maybe_open_startup_session(&mut self) -> bool {
        if !self.startup.ready(std::time::Instant::now()) {
            return false;
        }
        let Some(session_id) = startup_session_choice(
            self.dashboard.active_workspace_id(),
            self.controller.state.sessions.values().filter(|session| {
                self.dashboard.transition_kind(&session.id).is_none()
                    && self
                        .dashboard
                        .transition_failure_kind(&session.id)
                        .is_none()
            }),
            |session_id| self.dashboard.session_activity_at_ms(session_id),
        ) else {
            self.dashboard.focus_sessions();
            return true;
        };
        self.dashboard.focus_prompt();
        self.open_chat_session(&session_id);
        true
    }

    pub(crate) fn resolve_project_sources(&mut self) {
        let session_ids = self
            .controller
            .state
            .sessions
            .values()
            .filter(|session| session.state.is_active() && session.project_directory.is_some())
            .filter(|session| {
                !self.dashboard.has_resolved_project_source(&session.id)
                    && !self.project_sources_in_flight.contains(&session.id)
            })
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            self.project_sources_in_flight.insert(session_id.clone());
            spawn_project_source_resolution(
                &self.controller,
                session_id,
                self.dashboard_io_tx.clone(),
                self.critical_operations.clone(),
            );
        }
    }

    pub(crate) fn finish_materialized_projection(
        &mut self,
        session_id: String,
        result: std::result::Result<Box<PreparedMaterializedSessionDetail>, String>,
    ) {
        self.materialized_projections_in_flight.remove(&session_id);
        match result {
            Ok(detail) => {
                if self.dashboard.apply_prepared_materialized_session(*detail) {
                    // Only a projection that passed the dashboard's ordinal
                    // guard may answer whether a cached request is still
                    // pending. Startup summaries deliberately do not do so.
                    self.reconcile_question_drafts();
                }
            }
            Err(error) => self
                .dashboard
                .set_notice(format!("Could not update session transcript: {error}")),
        }
        if let Some((materialized, viewed_through_event_ordinal)) =
            self.pending_materialized_projections.remove(&session_id)
        {
            self.request_materialized_projection(materialized, viewed_through_event_ordinal);
        }
    }
}
