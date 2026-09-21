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

fn running_over_ssh(mut env: impl FnMut(&str) -> Option<std::ffi::OsString>) -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .into_iter()
        .any(|name| env(name).is_some_and(|value| !value.is_empty()))
}

fn copy_selected_text(
    text: &str,
    over_ssh: bool,
    schedule_system_copy: impl FnOnce(&str),
    terminal_copy: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    // Over SSH the desktop clipboard belongs to the remote host. Only OSC 52
    // targets the connecting terminal, even when X11 forwarding sets DISPLAY.
    if !over_ssh {
        schedule_system_copy(text);
    }
    terminal_copy(text)
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
            chats,
            opening_chat_sessions,
            selection,
            selection_text,
            ..
        } = self;
        let transcript_selected = selection.active_surface() == Some(SurfaceId::Transcript);
        // The renderer decides which pane draws which conversation and reports
        // back what it drew; that list is what the read receipts follow.
        let mut drawn = Vec::new();
        // The highlight and the extraction both run inside the draw closure,
        // once the surface has drawn: the hitboxes are registered by that
        // render and the cells the selection covers only exist in this frame.
        terminal.terminal.draw(|frame| {
            drawn = render_combined(
                frame,
                dashboard,
                chats,
                opening_chat_sessions,
                transcript_selected,
            );
            *selection_text = draw_selection(frame, selection, dashboard.frame_surfaces());
        })?;
        self.drawn_chat_sessions = drawn;
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
        let Some(chat) = self.focused_chat_mut() else {
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

    /// Copies the finished selection, using only the terminal clipboard over SSH.
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
                .focused_chat_mut()
                .and_then(|chat| chat.transcript_selection_text(&range)),
            SurfaceId::ElicitationMessage => self
                .focused_chat()
                .and_then(|chat| chat.elicitation_selection_text(&range)),
            // The reviewer pane scrolls its own rows, so the text a selection
            // covers comes out of that pane rather than off this frame.
            SurfaceId::ReviewerTranscript => self
                .focused_chat()
                .and_then(|chat| chat.reviewer_selection_text(&range)),
            _ => self.selection_text.take(),
        };
        let Some(text) = extracted.filter(|text| !text.trim().is_empty()) else {
            tracing::debug!(?surface, ?range, "selection covered no text");
            return Ok(());
        };
        let lines = text.lines().count().max(1);
        self.copy_text(
            &text,
            &format!("Copied {lines} line{}", if lines == 1 { "" } else { "s" }),
        )
    }

    pub(crate) fn copy_text(&mut self, text: &str, notice: &str) -> Result<()> {
        let updates = self.dashboard_io_tx.clone();
        if let Err(error) = copy_selected_text(
            text,
            running_over_ssh(|name| std::env::var_os(name)),
            |text| {
                // Opening the desktop clipboard can block, so keep it off
                // the render loop.
                spawn_clipboard_write(text.to_owned(), updates);
            },
            |text| self.terminal.copy_to_terminal_clipboard(text),
        ) {
            self.dashboard
                .set_failure_notice(format!("Copy to the terminal clipboard failed: {error:#}"));
            return Ok(());
        }
        self.dashboard.set_notice(notice);
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
        if self.panes_opening(session_id).is_empty() {
            for attachment in self.attachments.values_mut() {
                attachment.retire(session_id);
            }
        } else {
            self.defer_chat_open_for(session_id);
        }
        if self.chats.contains_key(session_id) {
            // A completed lifecycle retires the warm actor without passing
            // through the normal session-switch path. Preserve its latest
            // composer before dropping that actor as well.
            self.record_chat_detach(session_id);
            self.chats.remove(session_id);
            if self.dashboard.current_session_id() == Some(session_id) {
                self.selection.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_selection_uses_only_the_connecting_terminal_clipboard() {
        for indicator in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
            let over_ssh = running_over_ssh(|name| {
                (name == indicator || name == "DISPLAY").then(|| "present".into())
            });
            let mut copied = None;
            copy_selected_text(
                "selected text\nsecond line",
                over_ssh,
                |_| panic!("SSH selection must not open the remote desktop clipboard"),
                |text| {
                    copied = Some(text.to_owned());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(copied.as_deref(), Some("selected text\nsecond line"));
        }
    }

    #[test]
    fn local_selection_keeps_both_clipboard_routes_with_absent_or_empty_ssh_variables() {
        for value in [None, Some(std::ffi::OsString::new())] {
            let over_ssh = running_over_ssh(|_| value.clone());
            let mut system_text = None;
            let mut terminal_text = None;
            copy_selected_text(
                "local selection",
                over_ssh,
                |text| system_text = Some(text.to_owned()),
                |text| {
                    terminal_text = Some(text.to_owned());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(system_text.as_deref(), Some("local selection"));
            assert_eq!(terminal_text.as_deref(), Some("local selection"));
        }
    }

    #[test]
    fn ssh_terminal_write_failure_is_reported_without_trying_the_remote_clipboard() {
        let error = copy_selected_text(
            "selection",
            true,
            |_| panic!("SSH selection must not open the remote desktop clipboard"),
            |_| anyhow::bail!("terminal write failed"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "terminal write failed");
    }
}
