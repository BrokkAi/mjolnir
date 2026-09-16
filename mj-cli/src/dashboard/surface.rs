use super::*;

impl DashboardContext {
    /// Redraws the view on screen, if anything asked for a redraw.
    ///
    /// A notice is the only report several background failures get, and it can
    /// be written from any task through the shared slot. Comparing the slot
    /// with what the last frame drew is what makes a notice reach the screen
    /// even when nothing else marked the view dirty; without it a notice could
    /// be replaced or dismissed having rendered zero frames.
    pub(crate) fn draw(&mut self) -> Result<()> {
        let notice_generation = self.notices.generation();
        self.dirty |= notice_generation != self.drawn_notice_generation;
        self.dirty |= self.dashboard.take_render_changed();
        let chat_visible = self.visible_chat().is_some();
        let chat_changed = self
            .active_chat
            .as_mut()
            .is_some_and(|chat| chat.take_render_changed());
        self.dirty |= chat_visible && chat_changed;
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        self.drawn_notice_generation = notice_generation;
        let Self {
            terminal,
            dashboard,
            active_chat,
            opening_chat_session,
            selection,
            selection_text,
            drawn_size,
            ..
        } = self;
        let opening = opening_chat_session.as_deref();
        let selected_session = dashboard.selected_session_id().map(str::to_owned);
        let transcript_selected = selection.active_surface() == Some(SurfaceId::Transcript);
        // The highlight and the extraction both run inside the draw closure,
        // once the surface has drawn: the hitboxes are registered by that
        // render and the cells the selection covers only exist in this frame.
        terminal.terminal.draw(|frame| {
            *drawn_size = Some((frame.area().width, frame.area().height));
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
            .is_some_and(mj_chat::chat::ActiveChat::transcript_selection_invalidated);
        if invalidated && self.selection.active_surface() == Some(SurfaceId::Transcript) {
            self.selection.clear();
            self.dirty = true;
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
        let before = selection.visual_state();
        selection.retrack(dashboard.frame_surfaces());
        self.dirty |= before != selection.visual_state();
        Ok(())
    }

    /// Routes one terminal event through the selection engine, hit-testing
    /// against the surfaces the view on screen registered.
    pub(super) fn route_selection(&mut self, event: Event) -> SelectionRouting {
        let before_selection = self.selection.visual_state();
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
            self.dirty |= before_selection != selection.visual_state();
            return SelectionRouting::Forward(event);
        }
        let routed = route_prompt_selection(selection, dashboard, event);
        self.dirty |= before_selection != selection.visual_state();
        routed
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
            self.dirty = true;
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
            self.dirty = true;
        }
    }
}
