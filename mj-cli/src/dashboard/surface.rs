use super::*;

/// Posts one desktop notification with the platform's own helper. Output is
/// piped and discarded so a helper's stderr cannot reach the terminal that
/// the dashboard is drawing on.
fn post_system_notification(notification: &mj_tui::Notification) -> Result<()> {
    let title = format!("mj: {}", notification.session_title);
    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("osascript");
        command.arg("-e").arg(format!(
            "display notification {} with title {}",
            applescript_string(&notification.body),
            applescript_string(&title)
        ));
        command
    } else {
        let mut command = std::process::Command::new("notify-send");
        command
            .arg("--app-name=mj")
            .arg(&title)
            .arg(&notification.body);
        command
    };
    let output = mj_core::subprocess::run_with_input(&mut command, b"")?;
    if !output.status.success() {
        anyhow::bail!(
            "{} exited with {}: {}",
            command.get_program().to_string_lossy(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// An AppleScript string literal: double quotes and backslashes escaped.
fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

impl DashboardContext {
    /// Rebuilds the view on screen.
    ///
    /// `ratatui` compares this frame with the previous one and writes only the
    /// cells that differ, so drawing when nothing moved costs CPU time and no
    /// terminal output. That is why the loop draws once per wakeup instead of
    /// tracking which mutations were visible.
    pub(crate) fn draw(&mut self) -> Result<()> {
        self.drawn_notice_generation = self.notices.generation();
        let Self {
            terminal,
            dashboard,
            active_chat,
            opening_chat_session,
            selection,
            selection_text,
            ..
        } = self;
        let opening = opening_chat_session.as_deref();
        let selected_session = dashboard.selected_session_id().map(str::to_owned);
        let transcript_selected = selection.active_surface() == Some(SurfaceId::Transcript);
        // The highlight and the extraction both run inside the draw closure,
        // once the surface has drawn: the hitboxes are registered by that
        // render and the cells the selection covers only exist in this frame.
        terminal.terminal.draw(|frame| {
            render_combined(
                frame,
                dashboard,
                active_chat.as_mut().filter(|chat| {
                    chat_is_visible(opening, chat.session_id())
                        && dashboard.transition_kind(chat.session_id()).is_none()
                        && dashboard
                            .transition_failure_kind(chat.session_id())
                            .is_none()
                        && selected_session.as_deref() == Some(chat.session_id())
                }),
                transcript_selected,
            );
            *selection_text = draw_selection(frame, selection, dashboard.frame_surfaces());
        })?;
        self.dashboard.acknowledge_render();
        if let Some(chat) = self.visible_chat() {
            chat.acknowledge_render();
        }
        // The transcript reports a row space it can no longer measure the
        // selection in — a width change, a rebuilt cache, a jump across the
        // deep past. Dropping the selection is the honest answer; walking
        // history to rescue it is the full-transcript probe this design exists
        // to avoid.
        let invalidated = self
            .visible_chat()
            .is_some_and(|chat| chat.transcript_selection_invalidated());
        if invalidated && self.selection.active_surface() == Some(SurfaceId::Transcript) {
            self.selection.clear();
        }
        Ok(())
    }

    /// Reports sessions that started needing a person since the last frame:
    /// the terminal title, the bell, and a desktop notification, by the
    /// `[notify]` configuration. The dashboard decides what is due; this only
    /// writes it out. A desktop notification runs its helper off the loop.
    pub(crate) fn emit_notifications(&mut self) -> Result<()> {
        if let Some(title) = self.dashboard.terminal_title() {
            self.terminal.set_title(&title)?;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let due = self.dashboard.notification_events(now_ms);
        if due.is_empty() {
            return Ok(());
        }
        let notify = self.dashboard.notify_config().clone();
        if notify.bell {
            self.terminal.ring_bell()?;
        }
        if notify.mode == mj_core::config::NotifyMode::System {
            for notification in due {
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = post_system_notification(&notification) {
                        tracing::warn!(%error, session = %notification.session_id, "desktop notification failed");
                    }
                });
            }
        }
        Ok(())
    }

    /// The hitboxes the surface registered on its last frame. The combined
    /// renderer merges the conversation's into this one registry, so there is
    /// only ever one to consult.
    pub(crate) fn frame_surfaces(&self) -> &FrameSurfaces {
        self.dashboard.frame_surfaces()
    }

    /// The surface a held drag is asking to scroll, if any.
    pub(crate) fn autoscroll_request(&self) -> Option<(SurfaceId, i8)> {
        self.selection.autoscroll_request(self.frame_surfaces())
    }

    /// Scrolls the surface a drag is holding against its edge, then re-resolves
    /// the still pointer against the frame that scroll produced.
    ///
    /// The pointer emits no events while it is held still, so the rows that
    /// moved under it only join the selection once the registry is rebuilt,
    /// which needs the redraw in between.
    pub(crate) fn apply_autoscroll(&mut self) -> Result<()> {
        let Some((surface, direction)) = self.autoscroll_request() else {
            return Ok(());
        };
        let Some(chat) = self.active_chat.as_mut() else {
            return Ok(());
        };
        chat.autoscroll_selection(surface, direction);
        self.draw()?;
        let Self {
            selection,
            dashboard,
            ..
        } = self;
        selection.retrack(dashboard.frame_surfaces());
        Ok(())
    }

    /// Routes one terminal event through the selection engine, hit-testing
    /// against the surfaces the view on screen registered.
    pub(super) fn route_selection(&mut self, event: Event) -> SelectionRouting {
        let chat_owns_pointer = !self.dashboard.modal_open()
            && match &event {
                Event::Mouse(mouse) => self
                    .visible_chat()
                    .is_some_and(|chat| chat.component_handles_mouse(*mouse)),
                _ => false,
            };
        let Self {
            selection,
            dashboard,
            ..
        } = self;
        if let Event::Mouse(mouse) = &event
            && (dashboard.component_handles_mouse(*mouse) || chat_owns_pointer)
        {
            selection.clear();
            return SelectionRouting::Forward(event);
        }
        route_prompt_selection(selection, dashboard, event)
    }

    /// Copies the finished selection to the system and terminal clipboards.
    ///
    /// The frame on screen predates the release that finished the drag, so
    /// this redraws before reading. Surfaces that scroll their own rows own
    /// the text a selection covers, because most of it is not on the frame the
    /// stash is read from; everything else comes out of that stash.
    pub(crate) fn copy_selection(
        &mut self,
        surface: SurfaceId,
        range: SelectionRange,
    ) -> Result<()> {
        self.draw()?;
        let extracted = match surface {
            SurfaceId::Transcript => self
                .active_chat
                .as_mut()
                .and_then(|chat| chat.transcript_selection_text(&range)),
            SurfaceId::ElicitationMessage => self
                .active_chat
                .as_ref()
                .and_then(|chat| chat.elicitation_selection_text(&range)),
            // The reviewer pane scrolls its own rows, so the text a selection
            // covers comes out of that pane rather than off this frame.
            SurfaceId::ReviewerTranscript => self
                .active_chat
                .as_ref()
                .and_then(|chat| chat.reviewer_selection_text(&range)),
            _ => self.selection_text.take(),
        };
        let Some(text) = extracted.filter(|text| !text.trim().is_empty()) else {
            tracing::debug!(?surface, ?range, "selection covered no text");
            return Ok(());
        };
        // The desktop clipboard opens a blocking platform connection, so it
        // runs on a blocking task; OSC 52 is one escape sequence and also
        // reaches a terminal Hel is talking to over SSH.
        spawn_clipboard_write(text.clone(), self.dashboard_io_tx.clone());
        if let Err(error) = self.terminal.copy_to_terminal_clipboard(&text) {
            self.dashboard
                .set_failure_notice(format!("Copy to the terminal clipboard failed: {error:#}"));
            return Ok(());
        }
        let lines = text.lines().count().max(1);
        self.dashboard.set_notice(format!(
            "Copied {lines} line{}",
            if lines == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    /// Recomputes what depends on controller state after it may have changed.
    pub(crate) fn refresh_controller_derived_state(&mut self) {
        if !self.controller_changed {
            return;
        }
        self.controller_changed = false;
        let archive_targets = checkpoint_archive_targets(&self.controller);
        if archive_targets != self.checkpoint_archive_targets_seen {
            self.checkpoint_archive_targets_seen = archive_targets.clone();
            self.checkpoint_archive_generation =
                self.checkpoint_archive_generation.wrapping_add(1).max(1);
            spawn_checkpoint_archive_size_refresh(
                self.checkpoint_archive_generation,
                archive_targets,
                self.dashboard_io_tx.clone(),
            );
        }
        let capacity_targets = self.controller.deployment_capacity_targets();
        if *self.capacity_targets_tx.borrow() != capacity_targets {
            self.capacity_targets_tx
                .send_replace(capacity_targets.clone());
            self.dashboard
                .set_deployment_capacity_targets(capacity_targets);
        }
    }

    /// Asks the poller for fresh quotas and reports the refresh in the UI.
    /// Returns the generation, so a manual refresh can recognize its own
    /// completion.
    pub(crate) fn request_quota_refresh(&mut self) -> u64 {
        let profiles = quota_refresh_profiles(&self.controller);
        self.dashboard
            .begin_quota_refresh(profiles.iter().map(|profile| profile.profile_id.clone()));
        let generation = self
            .quota_profiles_tx
            .borrow()
            .generation
            .wrapping_add(1)
            .max(1);
        self.quota_profiles_tx.send_replace(QuotaRefreshBatch {
            generation,
            profiles,
        });
        generation
    }

    /// Republishes what the pollers should watch, leaving out sessions a
    /// lifecycle operation currently owns.
    pub(crate) fn refresh_poll_targets(&self) {
        refresh_dashboard_poll_targets(
            &self.controller,
            &self.worker_targets_tx,
            &self.resource_targets_tx,
            &self.credential_sync_handle,
            &self.lifecycle_operations.keys().cloned().collect(),
        );
    }

    /// Drops the warm chat when it belongs to `session_id`.
    ///
    /// Pause and destroy retire that session's actor. Resume starts a new one,
    /// often on a different profile. Keeping the old view would redraw a
    /// Closing/Closed snapshot and refuse prompts.
    pub(crate) fn drop_warm_chat_for(&mut self, session_id: &str) {
        if self.opening_chat_session.as_deref() == Some(session_id) {
            self.defer_chat_open();
        } else {
            self.attachment.retire(session_id);
        }
        if self
            .active_chat
            .as_ref()
            .is_some_and(|chat| chat.session_id() == session_id)
        {
            if let Some(ordinal) = self
                .active_chat
                .as_ref()
                .map(mj_chat::chat::ActiveChat::latest_event_ordinal)
            {
                // A completed lifecycle retires the warm actor without
                // passing through the normal session-switch path. Preserve
                // its latest composer before dropping that actor as well.
                self.record_detach(ordinal);
            }
            self.active_chat = None;
            self.selection.clear();
        }
    }
}
