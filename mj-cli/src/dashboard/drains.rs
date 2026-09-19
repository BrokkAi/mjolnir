use super::*;

impl DashboardContext {
    pub(crate) fn cancel_background_work(&mut self) {
        self.cancel_chat_open();
        self.critical_operations.cancel_all();
        if let Some(cancelled) = &self.review_discovery_cancel {
            cancelled.store(true, Ordering::Release);
        }
        self.cancel_session_preflight();
        for operation in self.lifecycle_operations.values() {
            operation.cancelled.store(true, Ordering::Release);
        }
        if let Some(active) = self.active_import.as_ref() {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    /// Takes every message queued behind the one that woke the loop, feed by
    /// feed, in the order the UI depends on.
    /// Applies whatever the background feeds have queued, and reports whether
    /// any message arrived. The report is what lets a timer wakeup that had
    /// nothing of its own to show still draw the message that rode with it.
    pub(crate) fn drain_feeds(&mut self) -> bool {
        self.cancel_stale_path_input();
        self.drain_quota_updates();
        self.drain_runtime_state();
        self.drain_worker_updates();
        self.drain_runtime_reviews();
        self.drain_runtime_notices();
        self.drain_runtime_config();
        schedule_due_credential_syncs(
            &mut self.credential_sync_signals,
            &self.credential_sync_handle,
            Instant::now(),
        );
        self.drain_credential_results();
        self.drain_resource_updates();
        self.drain_capacity_updates();
        self.drain_aws_resource_options();
        self.drain_import_profiles();
        self.drain_import_tasks();
        self.drain_lifecycle_updates();
        self.drain_dashboard_io();
        self.refresh_open_review();
        // Collected rather than short-circuited: every feed's flag has to be
        // cleared for the next drain.
        [
            self.quota.take_delivered(),
            self.worker.take_delivered(),
            self.runtime_state.take_delivered(),
            self.runtime_reviews.take_delivered(),
            self.runtime_notices.take_delivered(),
            self.runtime_config.take_delivered(),
            self.lifecycle.take_delivered(),
            self.credential_sync.take_delivered(),
            self.resource.take_delivered(),
            self.capacity.take_delivered(),
            self.aws_options.take_delivered(),
            self.import_profiles.take_delivered(),
            self.import_tasks.take_delivered(),
            self.dashboard_io.take_delivered(),
        ]
        .into_iter()
        .any(|delivered| delivered)
    }

    /// Whether the once-a-second clock has anything new to put on screen.
    ///
    /// A notice is the only report several background failures get, and any
    /// task can write it through the shared slot without waking this loop, so
    /// the slot is compared with what the frame on screen was drawn from.
    pub(super) fn clock_tick_redraws(&mut self) -> bool {
        self.dashboard.clock_changed()
            || self.notices.generation() != self.drawn_notice_generation
            || self.visible_chat().is_some_and(|chat| chat.clock_changed())
    }

    /// Hands the open chat whatever turn review the daemon is running for it.
    ///
    /// The chat draws the pane and sends the resolutions; it hosts nothing.
    /// A session with no review gets `None`, which closes the pane.
    pub(crate) fn drain_runtime_reviews(&mut self) {
        let mut latest = None;
        while let Some(reviews) = self.runtime_reviews.next_ready() {
            latest = Some(reviews);
        }
        let Some(reviews) = latest else {
            return;
        };
        self.runtime_review_views = reviews
            .into_iter()
            .map(|review| (review.session_id.clone(), review))
            .collect();
        self.dashboard
            .set_session_reviews(self.runtime_review_views.values().cloned());
        self.apply_runtime_reviews_to_chats();
    }

    /// Report each background notice the daemon published since the last one
    /// this surface showed.
    pub(crate) fn drain_runtime_notices(&mut self) {
        let mut latest = None;
        while let Some(notices) = self.runtime_notices.next_ready() {
            latest = Some(notices);
        }
        let Some(notices) = latest else {
            return;
        };
        for notice in notices {
            if self
                .reported_notice_id
                .is_some_and(|reported| notice.id <= reported)
            {
                continue;
            }
            self.reported_notice_id = Some(notice.id);
            let text = self
                .dashboard
                .notice_naming_workspace(&notice.session_id, notice.text);
            self.dashboard.set_notice(text);
        }
    }

    /// Applies the retained daemon projection to every warm chat. Absence is
    /// meaningful: it closes a review the previous projection held.
    pub(crate) fn apply_runtime_reviews_to_chats(&mut self) {
        for chat in self.chats.values_mut() {
            let review = self.runtime_review_views.get(chat.session_id()).cloned();
            chat.apply_review_view(review);
        }
    }

    /// The same for one conversation, for a chat that has just opened.
    pub(crate) fn apply_runtime_review_to_chat(&mut self, session_id: &str) {
        let Some(chat) = self.chats.get_mut(session_id) else {
            return;
        };
        let review = self.runtime_review_views.get(session_id).cloned();
        chat.apply_review_view(review);
    }

    /// Keeps the stop confirmation aware of the attached chat's plan-review
    /// second opinion. Turn reviews come from `runtime_review_views` and are
    /// projected independently, including for sessions without an open chat.
    pub(crate) fn refresh_open_review(&mut self) {
        for (session_id, open) in self
            .chats
            .values()
            .map(|chat| (chat.session_id().to_owned(), chat.has_open_review()))
            .collect::<Vec<_>>()
        {
            self.dashboard.set_session_review_open(&session_id, open);
        }
    }

    pub(crate) fn drain_quota_updates(&mut self) {
        while let Some(update) = self.quota.next_ready() {
            match update {
                QuotaUpdate::Refreshing { profile_ids } => {
                    self.dashboard.begin_quota_refresh(profile_ids)
                }
                QuotaUpdate::Report(outcome) => {
                    if outcome.credentials_changed {
                        self.credential_sync_handle
                            .sync_profile_now(&outcome.report.profile_id, None);
                    }
                    self.dashboard.apply_quota(outcome.report);
                }
                QuotaUpdate::Finished { generation } => {
                    if complete_manual_quota_refresh(
                        &mut self.manual_quota_refresh_generation,
                        generation,
                    ) {
                        self.dashboard
                            .replace_notice_if(QUOTA_REFRESH_NOTICE, QUOTA_REFRESHED_NOTICE);
                    }
                }
            }
        }
    }

    pub(crate) fn drain_worker_updates(&mut self) {
        while let Some(update) = self.worker.next_ready() {
            self.controller_changed = true;
            let session_id = update.session_id.clone();
            let connected = update.view.connected;
            apply_session_activity(&mut self.dashboard, &session_id, &update.view);
            // Only unreachable relays drive the worker diagnostics flow.
            let connection_error = match update.view.error.as_ref() {
                Some(ViewError::Unreachable(detail)) => Some(detail.clone()),
                Some(ViewError::TargetMissing(_) | ViewError::ProjectionIntegrity(_)) | None => {
                    None
                }
            };
            if let Some(snapshot) = update.view.snapshot.as_ref()
                && let Some(session) = self.controller.state.sessions.get(&session_id).cloned()
                && let Some(signal) = snapshot.latest_credential_sync_signal.clone()
            {
                self.credential_sync_signals
                    .observe(&session_id, &session.last_profile, signal);
            }
            let materialized = update
                .view
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.materialized.clone());
            match apply_worker_poll_update(
                &mut self.controller,
                &mut self.dashboard,
                update,
                &self.dashboard_io_tx,
                &self.critical_operations,
            ) {
                Ok(true) => {
                    let _ = self.resource_triggers_tx.try_send(session_id.clone());
                    if let Some(materialized) = materialized {
                        let viewed_through_event_ordinal = self
                            .controller
                            .state
                            .sessions
                            .get(&session_id)
                            .map_or(0, |session| session.viewed_through_event_ordinal);
                        self.request_materialized_projection(
                            materialized,
                            viewed_through_event_ordinal,
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    self.dashboard
                        .set_notice(format!("Could not save harness title: {error:#}"));
                }
            }
            if let Some(episode_id) =
                self.worker_diagnoses
                    .observe(&session_id, connected, connection_error)
            {
                spawn_worker_diagnosis(
                    &self.controller,
                    session_id,
                    episode_id,
                    self.dashboard_io_tx.clone(),
                    self.critical_operations.clone(),
                );
            }
        }
    }

    pub(crate) fn drain_runtime_state(&mut self) {
        let mut latest = None;
        while let Some(update) = self.runtime_state.next_ready() {
            latest = Some(update);
        }
        let Some(update) = latest else {
            return;
        };
        // Watch receivers can be primed with revision zero and later receive
        // the same revision during startup. Accept that first snapshot, but
        // never let an older daemon response roll a completed operation back.
        if update.revision < self.runtime_state_revision {
            return;
        }
        self.runtime_state_revision = update.revision;
        let removed_layouts = self
            .known_workspace_layouts
            .iter()
            .filter(|id| !update.workspace_names.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in removed_layouts {
            self.pane_size_persistence.forget(&id);
            self.layout_persistence.forget(&id);
            self.workspace_layouts.remove(&id);
            self.known_workspace_layouts.remove(&id);
        }
        let new_layouts = update
            .workspace_names
            .keys()
            .filter(|id| !self.known_workspace_layouts.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        if !new_layouts.is_empty() {
            self.known_workspace_layouts
                .extend(new_layouts.iter().cloned());
            io::spawn_workspace_pane_sizes_load(new_layouts.clone(), self.dashboard_io_tx.clone());
            io::spawn_workspace_layouts_load(new_layouts, self.dashboard_io_tx.clone());
        }
        let next_workspace = if self
            .dashboard
            .active_workspace_id()
            .is_some_and(|id| update.workspace_names.contains_key(id))
        {
            self.dashboard.active_workspace_id().map(str::to_owned)
        } else {
            update.workspace_names.keys().next().cloned()
        };
        self.dashboard.set_workspace_names(update.workspace_names);
        self.select_workspace(next_workspace);
        self.apply_runtime_records(update.records, update.subagents);
        self.dashboard.set_native_agents(update.native_agents);
        self.dashboard.set_move_operations(update.moves);
        self.apply_runtime_lifecycles(update.lifecycles);
        self.controller_changed = true;
    }

    pub(crate) fn apply_runtime_lifecycles(
        &mut self,
        lifecycles: Vec<crate::daemon::RuntimeLifecycleView>,
    ) {
        let active = lifecycles
            .iter()
            .map(|lifecycle| lifecycle.session_id.clone())
            .collect::<BTreeSet<_>>();
        for session_id in self
            .remote_lifecycle_sessions
            .difference(&active)
            .cloned()
            .collect::<Vec<_>>()
        {
            let move_active = self.dashboard.move_operation_active(&session_id);
            if !self.lifecycle_operations.contains_key(&session_id) && !move_active {
                self.dashboard.finish_session_operation(&session_id);
            }
            self.remote_lifecycle_sessions.remove(&session_id);
            self.remote_lifecycle_operations.remove(&session_id);
        }
        for lifecycle in lifecycles {
            let kind = lifecycle_kind(lifecycle.kind);
            mark_active_chat_retiring_for_remote_lifecycle(
                self.chats.get_mut(&lifecycle.session_id),
                &lifecycle.session_id,
                kind,
            );
            // Keep the daemon identity even while a local action is waiting
            // for its callback. The callback is keyed only by session id, so
            // this lets it distinguish an authoritative newer operation.
            let previous_operation_id = self
                .remote_lifecycle_operations
                .insert(lifecycle.session_id.clone(), lifecycle.operation_id.clone());
            self.remote_lifecycle_sessions
                .insert(lifecycle.session_id.clone());
            if !self
                .lifecycle_operations
                .contains_key(&lifecycle.session_id)
            {
                if previous_operation_id
                    .as_ref()
                    .is_some_and(|previous| previous != &lifecycle.operation_id)
                {
                    // A new daemon operation for the same session supersedes
                    // the old overlay even when both snapshots use the same
                    // lifecycle kind.
                    self.dashboard
                        .finish_session_operation(&lifecycle.session_id);
                }
                apply_lifecycle_display(&mut self.dashboard, &lifecycle);
            } else {
                self.dashboard.set_session_operation_identity(
                    &lifecycle.session_id,
                    Some(lifecycle.operation_id.clone()),
                    lifecycle.cancellable,
                );
                self.dashboard.replace_session_operation_stages(
                    &lifecycle.session_id,
                    lifecycle.active_stages,
                );
                if let Some((profile_id, target_id)) = lifecycle.resume_destination {
                    self.dashboard.set_resume_destination(
                        &lifecycle.session_id,
                        profile_id,
                        target_id,
                    );
                }
            }
            if let Some(notice) = lifecycle.notice {
                self.dashboard.set_notice(notice);
            }
        }
    }

    /// Hands the open conversation the surface's current view of the config
    /// and its own session record. The chat snapshots both when it opens, so
    /// without this a long-lived chat would keep offering reviewer profiles
    /// that a config reload has since renamed or removed.
    pub(crate) fn refresh_chat_context(&mut self) {
        let Self {
            chats,
            controller,
            dashboard,
            ..
        } = self;
        for chat in chats.values_mut() {
            let record = controller.state.sessions.get(chat.session_id());
            chat.refresh_context(
                &controller.config,
                record,
                record.map(|session| controller.state.project_identity_session(session)),
            );
            if dashboard.go_mode().is_some() {
                chat.set_display_title(dashboard.go_conversation_title(chat.session_id()));
            }
            let count = dashboard.subagent_count_for(chat.session_id());
            chat.set_subagent_count(count);
        }
    }

    pub(crate) fn drain_runtime_config(&mut self) {
        let mut latest = None;
        while let Some(config) = self.runtime_config.next_ready() {
            latest = Some(config);
        }
        let Some(config) = latest else {
            return;
        };
        // The chat's copy of `[review]` follows the daemon's, so `/review
        // status` and the composer's armed indicator report what is actually
        // running rather than what this process last read from disk.
        for chat in self.chats.values_mut() {
            chat.set_review_config(config.review.clone());
        }
        if config == self.controller.config || self.config_reload_in_flight {
            return;
        }
        self.controller.config = config.clone();
        self.dashboard.set_config(config);
        self.refresh_chat_context();
        self.refresh_poll_targets();
        self.request_quota_refresh();
        self.config_reload_in_flight = true;
        let workspace_id = self.workspace_id.clone();
        let client_id = self.client_id.clone();
        spawn_io(
            "reload daemon configuration",
            self.dashboard_io_tx.clone(),
            move || {
                let mut controller = Controller::load()?;
                retain_workspace_sessions(&mut controller, &workspace_id, &client_id)?;
                Ok(controller)
            },
            DashboardIoUpdate::ConfigReloaded,
        );
    }

    pub(crate) fn apply_runtime_records(
        &mut self,
        records: Vec<SessionRecord>,
        subagents: Vec<SubagentRecord>,
    ) {
        let mut sessions: BTreeMap<String, SessionRecord> = records
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        read_receipts::preserve_read_positions(&mut sessions, &self.controller.state.sessions);
        let subagents: BTreeMap<String, SubagentRecord> = subagents
            .into_iter()
            .map(|subagent| (subagent.child_session_id.clone(), subagent))
            .collect();
        let Self {
            chats, dashboard, ..
        } = self;
        for chat in chats.values_mut() {
            let feed_expected = sessions
                .get(chat.session_id())
                .is_some_and(session_target_is_pollable);
            // A session that is no longer pollable, or that any surface has
            // asked to stop or destroy, is being retired on purpose: its feed
            // will close and the chat must not chase a replacement actor.
            let retiring = !feed_expected
                || matches!(
                    dashboard.session_operation_kind(chat.session_id()),
                    Some(
                        SessionOperationKind::Stopping
                            | SessionOperationKind::Destroying
                            | SessionOperationKind::Moving,
                    )
                )
                || dashboard
                    .transition_failure_kind(chat.session_id())
                    .is_some();
            chat.set_session_retiring(retiring);
            // Order matters: an expected feed clears the retiring flag.
            chat.set_session_feed_expected(feed_expected);
        }
        if self.controller.state.sessions == sessions
            && self.controller.state.subagents == subagents
        {
            return;
        }
        self.controller.state.sessions = sessions;
        self.controller.state.subagents = subagents;
        self.dashboard.set_state(self.controller.state.clone());
        self.reconcile_question_drafts();
        self.refresh_chat_context();
        self.controller_changed = true;
        self.refresh_poll_targets();
    }

    pub(crate) fn drain_credential_results(&mut self) {
        while let Some(result) = self.credential_sync.next_ready() {
            crate::pollers::log_credential_sync_actions(&result);
            let harness = self
                .controller
                .config
                .profiles
                .get(&result.profile_id)
                .map(|profile| profile.kind);
            if let Some(notice) = self.credential_sync_notices.notice(&result, harness) {
                self.dashboard.set_notice(notice);
            }
        }
    }

    pub(crate) fn drain_resource_updates(&mut self) {
        while let Some(update) = self.resource.next_ready() {
            self.dashboard
                .apply_resource_usage(&update.session_id, update.usage);
        }
    }

    pub(crate) fn drain_capacity_updates(&mut self) {
        while let Some(update) = self.capacity.next_ready() {
            self.dashboard.apply_deployment_capacity(
                &update.target_id,
                update.result,
                update.sampled_at_epoch_seconds,
            );
        }
    }

    pub(crate) fn request_capacity_refresh(&mut self) {
        self.dashboard.begin_capacity_refresh();
        match self.capacity_triggers_tx.try_send(()) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
                self.dashboard.set_notice("Refreshing target capacity…");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                self.dashboard
                    .set_notice("Could not refresh target capacity: poller stopped.");
            }
        }
    }

    pub(crate) fn drain_aws_resource_options(&mut self) {
        while let Some((target_id, result)) = self.aws_options.next_ready() {
            self.resolving_aws_resource_options.remove(&target_id);
            self.dashboard
                .apply_aws_resource_options(&target_id, result);
        }
    }

    pub(crate) fn drain_import_profiles(&mut self) {
        while let Some((discovery_id, profile)) = self.import_profiles.next_ready() {
            self.dashboard.apply_resume_profile(discovery_id, profile);
        }
    }

    pub(crate) fn drain_import_tasks(&mut self) {
        while let Some(update) = self.import_tasks.next_ready() {
            match update {
                DashboardImportUpdate::Progress {
                    task_id,
                    step,
                    total,
                    message,
                } => {
                    if self
                        .active_import
                        .as_ref()
                        .is_some_and(|active| active.task_id == task_id)
                    {
                        self.dashboard.update_import_progress(step, total, message);
                    }
                }
                DashboardImportUpdate::Finished {
                    task_id,
                    pending,
                    result,
                } => {
                    if self
                        .active_import
                        .as_ref()
                        .is_none_or(|active| active.task_id != task_id)
                    {
                        continue;
                    }
                    self.active_import = None;
                    match *result {
                        Ok(DashboardImportTaskResult::NeedsBundle(prompt)) => {
                            self.pending_import = Some(pending);
                            self.dashboard.show_import_bundle_confirmation(
                                prompt.dirty_git_roots,
                                prompt.omitted_non_git_dirs,
                                prompt.scratch_git_roots,
                                prompt.has_untracked_files,
                                prompt.managed_worktree,
                            );
                        }
                        Ok(DashboardImportTaskResult::Imported(imported)) => {
                            self.dashboard.finish_import();
                            self.dashboard.set_notice("Saving imported session…");
                            io::spawn_imported_session_apply(
                                *imported,
                                pending,
                                self.dashboard_io_tx.clone(),
                                self.critical_operations.clone(),
                            );
                        }
                        Ok(DashboardImportTaskResult::Cancelled) => {
                            self.dashboard.finish_import();
                            self.dashboard
                                .set_notice("Import cancelled; no Mjolnir files were changed.");
                        }
                        Err(error) => {
                            self.dashboard.finish_import();
                            self.dashboard
                                .set_notice(format!("Import failed: {error:#}"));
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn drain_lifecycle_updates(&mut self) {
        while let Some(update) = self.lifecycle.next_ready() {
            self.controller_changed = true;
            let session_id = update.session_id.clone();
            let operation = self.lifecycle_operations.remove(&session_id);
            // A completion callback carries only the session id. If a newer
            // daemon operation is already present in the coherent runtime
            // snapshot, retain its overlay; the next snapshot that removes
            // that operation will settle it. This prevents an old local
            // callback from hiding an authoritative newer operation.
            if !self.remote_lifecycle_operations.contains_key(&session_id)
                && !self.dashboard.move_operation_active(&session_id)
            {
                self.dashboard.finish_session_operation(&session_id);
            }
            spawn_lifecycle_reload(
                LifecycleReload { update, operation },
                self.workspace_id.clone(),
                self.client_id.clone(),
                self.dashboard_io_tx.clone(),
            );
        }
    }

    pub(crate) fn drain_dashboard_io(&mut self) {
        while let Some(update) = self.dashboard_io.next_ready() {
            self.controller_changed = true;
            self.apply_dashboard_io_update(update);
        }
    }
}
