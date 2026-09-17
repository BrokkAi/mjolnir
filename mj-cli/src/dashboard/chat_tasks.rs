use super::*;

impl DashboardContext {
    /// Opens the resume dialog and starts one background scan per profile.
    /// Every profile appears immediately as a placeholder, so the dialog is
    /// usable while the scans are still running, and the scans run
    /// concurrently rather than one after another.
    pub(crate) fn start_resume_discovery(&mut self) {
        self.import_discovery_id = self.import_discovery_id.wrapping_add(1);
        self.dashboard.show_resume_dialog(
            self.import_discovery_id,
            resume_profile_placeholders(
                self.controller
                    .config
                    .enabled_profiles()
                    .map(|(id, profile)| (id.to_owned(), profile.kind)),
            ),
        );
        // The Archived tab lists the most recent indexed sessions before
        // anything is typed, so the first search runs as the dialog opens.
        if let Some((request_id, query)) = self.dashboard.next_wiki_search() {
            crate::dashboard::io::spawn_wiki_search(
                request_id,
                query,
                self.wiki_search_request.clone(),
                self.dashboard_io_tx.clone(),
            );
        }
        let discovery_id = self.import_discovery_id;
        for (profile_id, profile) in self
            .controller
            .config
            .enabled_profiles()
            .map(|(id, profile)| (id.to_owned(), profile.clone()))
        {
            let updates = self.import_updates_tx.clone();
            tokio::task::spawn_blocking(move || {
                let completed = crate::import::discover_import_profile(
                    profile_id,
                    profile.kind,
                    profile.home,
                    |profile| {
                        if let Ok(permit) = updates.try_reserve() {
                            permit.send((discovery_id, profile.clone()));
                        }
                    },
                );
                let _ = updates.blocking_send((discovery_id, completed));
            });
        }
    }

    /// Starts a background import and shows its progress dialog.
    pub(crate) fn start_import(
        &mut self,
        pending: PendingDashboardImport,
        safety: crate::import::DashboardImportSafety,
    ) {
        self.dashboard
            .show_import_progress(pending.display_title.clone());
        self.next_import_task_id = self.next_import_task_id.wrapping_add(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active_import = Some(ActiveDashboardImport {
            task_id: self.next_import_task_id,
            cancelled: cancelled.clone(),
        });
        spawn_dashboard_import(
            &self.controller,
            DashboardImportRequest {
                workspace_id: self.workspace_id.clone(),
                pending,
                safety,
                task_id: self.next_import_task_id,
                cancelled,
            },
            self.import_task_tx.clone(),
            self.critical_operations.clone(),
        );
    }

    /// Applies what the chat view asked for after handling its own input.
    pub(crate) async fn apply_chat_outcome(&mut self, outcome: mj_chat::chat::ChatEventOutcome) {
        match outcome {
            mj_chat::chat::ChatEventOutcome::None | mj_chat::chat::ChatEventOutcome::Handled => {}
            mj_chat::chat::ChatEventOutcome::CycleFocus { reverse } => {
                self.dashboard.cycle_focus(reverse);
            }
            mj_chat::chat::ChatEventOutcome::OpenSubagents => {
                let Some(parent_id) = self
                    .active_chat
                    .as_ref()
                    .map(|chat| chat.session_id().to_owned())
                else {
                    return;
                };
                self.capture_active_composer_draft();
                self.dashboard.open_subagent_workspace(parent_id);
                if let Some(child_id) = self.dashboard.selected_session_id().map(str::to_owned) {
                    self.open_chat_session(&child_id);
                }
            }
            mj_chat::chat::ChatEventOutcome::QuitDetach { .. } => {
                self.request_shutdown();
            }
        }
    }

    /// Persists how far the warm chat has been read and the draft it holds.
    pub(crate) fn record_detach(
        &mut self,
        last_seen_event_ordinal: u64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.capture_active_composer_draft();
        let session_id = self
            .active_chat
            .as_ref()
            .map(mj_chat::chat::ActiveChat::session_id)?
            .to_owned();
        let draft = self.composer_drafts.get(&session_id)?.clone();
        let last_seen_event_ordinal = detach_read_frontier(
            self.visible_chat().is_some(),
            last_seen_event_ordinal,
            self.controller
                .state
                .sessions
                .get(&session_id)
                .map_or(0, |session| session.viewed_through_event_ordinal),
        );
        record_chat_detach_state(
            &mut self.controller,
            &mut self.dashboard,
            DetachedChatState {
                client_id: &self.client_id,
                session_id: &session_id,
                event_ordinal: last_seen_event_ordinal,
                draft,
            },
            &self.dashboard_io_tx,
            self.critical_operations.clone(),
        )
    }

    /// Capture the currently visible chat's composer while retaining the
    /// first shared value it inherited. The text remains process-local until
    /// an explicit detach persistence task archives it.
    pub(crate) fn capture_active_composer_draft(&mut self) {
        let Some((session_id, text)) = self
            .active_chat
            .as_ref()
            .map(|chat| (chat.session_id().to_owned(), chat.draft()))
        else {
            return;
        };
        let inherited_input = self
            .controller
            .state
            .sessions
            .get(&session_id)
            .map_or_else(String::new, |session| session.draft_input.clone());
        self.composer_drafts
            .capture(&session_id, text, &inherited_input);
    }
}
