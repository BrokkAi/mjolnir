use super::*;

/// Stands in for the session id of the launch standby, which is opened before
/// the daemon has registered a session. [`DashboardState::adopt_launch_standby`]
/// replaces it with the real id.
const LAUNCH_STANDBY_SESSION_ID: &str = "launching";

impl DashboardState {
    /// Records the conversation the focused pane shows, which decides which
    /// project the compact Sessions list belongs to.
    pub fn set_current_session(&mut self, session_id: Option<&str>) {
        self.set_pane_session(self.conversation_layout.focused(), session_id);
    }

    /// Records the conversation one named pane shows. An attach that started
    /// for a pane lands in that pane, whichever one has the focus by the time
    /// it arrives.
    pub fn set_pane_session(&mut self, pane: crate::tile_layout::PaneId, session_id: Option<&str>) {
        if self.pane_sessions.get(&pane).map(String::as_str) == session_id {
            return;
        }
        // A session belongs to one pane, so moving it into this one takes it
        // out of any other.
        if let Some(session_id) = session_id {
            self.pane_sessions
                .retain(|other, shown| *other == pane || shown != session_id);
        }
        match session_id {
            Some(session_id) => {
                self.pane_sessions.insert(pane, session_id.to_owned());
            }
            None => {
                self.pane_sessions.remove(&pane);
            }
        }
        self.mark_layout_modified();
        self.clamp_selections();
    }

    /// When this session's materialized projection last changed, in
    /// milliseconds since the epoch. `None` while nothing has been projected
    /// for it yet.
    pub fn session_activity_at_ms(&self, session_id: &str) -> Option<u64> {
        self.session_details
            .get(session_id)
            .and_then(|detail| detail.last_activity_at_ms)
    }

    /// The session the focused pane shows, if it is not empty.
    pub fn current_session_id(&self) -> Option<&str> {
        self.pane_sessions
            .get(&self.conversation_layout.focused())
            .map(String::as_str)
    }

    /// Records the session an attach is running for, or clears it when the
    /// attach settles.
    pub fn set_opening_session(&mut self, session_id: Option<&str>) {
        let session_id = session_id.map(str::to_owned);
        if self.opening_session == session_id {
            return;
        }
        self.opening_session = session_id;
    }

    /// The session an attach is still running for, if any.
    pub fn opening_session(&self) -> Option<&str> {
        self.opening_session.as_deref()
    }

    /// The session the Sessions pane has selected. The conversation on screen
    /// follows this, so moving the selection moves the transcript.
    pub fn selected_session_id(&self) -> Option<&str> {
        self.selected_session_id.as_deref()
    }

    /// The operation that owns a session's conversation, if any. A local or
    /// daemon operation wins over the durable record; the state fallback keeps
    /// a recovering provisioning/closing/destroying record hidden until its
    /// authoritative lifecycle completion arrives.
    pub fn transition_kind(&self, session_id: &str) -> Option<SessionTransitionKind> {
        if let Some(operation) = self.session_operations.get(session_id) {
            // An explicit non-transition operation (Connecting/Importing) is
            // still authoritative: it must not fall through to a stale
            // Provisioning record and hide the conversation.
            return operation.kind.transition_kind();
        }
        let session = self.state.sessions.get(session_id)?;
        // A failed close/destroy remains durable for recovery, but it is no
        // longer an in-flight transition. Keep its error and recovery controls
        // visible instead of showing a spinner.
        if session.last_error.is_some() {
            return None;
        }
        session.state.transition_kind()
    }

    /// A durable transition record that failed before it could return to an
    /// ordinary state. This is intentionally narrower than `Error`: only
    /// Closing/Destroying records with an explicit error qualify.
    pub fn transition_failure_kind(&self, session_id: &str) -> Option<SessionTransitionKind> {
        if self.session_operations.contains_key(session_id) {
            return None;
        }
        let session = self.state.sessions.get(session_id)?;
        if session.last_error.is_some()
            && matches!(
                session.state,
                SessionState::Closing | SessionState::Destroying
            )
        {
            session.state.transition_kind()
        } else {
            None
        }
    }

    /// The selected session whose prompt band shows the standby composer: a
    /// Starting or Resuming transition parks the conversation behind it, or
    /// an attach for the session is in flight. Retiring transitions (Moving/
    /// Stopping/Destroying) and failed ones keep the status panel instead:
    /// there is no conversation to type toward.
    pub(crate) fn standby_prompt_session(&self) -> Option<&str> {
        let session_id = self.selected_session_id()?;
        let parked = self.transition_kind(session_id).is_some_and(|kind| {
            matches!(
                kind,
                SessionTransitionKind::Starting | SessionTransitionKind::Resuming
            )
        }) || self.opening_session.as_deref() == Some(session_id);
        parked.then_some(session_id)
    }

    /// The standby composer a session's prompt band is editing, creating it on
    /// first use so every host path (seeding, keys, paste, render) shares one
    /// instance.
    pub(crate) fn standby_prompt_mut(&mut self, session_id: &str) -> &mut ChatState {
        if !self.standby_prompts.contains_key(session_id) {
            let standby = self.build_standby_prompt(session_id);
            self.standby_prompts.insert(session_id.to_owned(), standby);
        }
        self.standby_prompts
            .get_mut(session_id)
            .expect("standby prompt was just inserted")
    }

    pub(crate) fn build_standby_prompt(&self, session_id: &str) -> ChatState {
        let header = self.standby_prompt_header(session_id);
        ChatState::standby(session_id, &self.config, header, self.notices.clone())
    }

    /// The session-list columns a standby composer shows in its header.
    fn standby_prompt_header(&self, session_id: &str) -> SessionHeaderIdentity {
        let session = self.state.sessions.get(session_id);
        SessionHeaderIdentity {
            target: session.as_ref().map_or(String::new(), |session| {
                self.state
                    .project_identity_session(session)
                    .project_target(&self.config, &session.target_template_id)
            }),
            profile: session
                .as_ref()
                .map_or(String::new(), |session| session.last_profile.clone()),
            title: session.as_ref().map_or(String::new(), |session| {
                if self.go.is_some() {
                    self.go_conversation_title(&session.id)
                } else {
                    session.display_title().to_owned()
                }
            }),
            harness_kind: session.as_ref().map(|session| session.harness_kind),
            subagent_count: self
                .state
                .subagents
                .values()
                .filter(|record| record.parent_session_id == session_id)
                .count(),
        }
    }

    /// Opens the launch standby: the composer that holds typing from the
    /// moment the new-session wizard closes until the daemon registers the
    /// session. The conversation on screen is cleared so the session that was
    /// selected before neither draws nor takes the keys.
    pub fn begin_launch_standby(&mut self, header: SessionHeaderIdentity) {
        self.launch_standby = Some(ChatState::standby(
            LAUNCH_STANDBY_SESSION_ID,
            &self.config,
            header,
            self.notices.clone(),
        ));
        self.launch_standby_anchor = self.selected_session_id.clone();
        self.set_current_session(None);
        self.focus_prompt();
    }

    /// Whether a launch standby is waiting for its session.
    pub fn has_launch_standby(&self) -> bool {
        self.launch_standby.is_some()
    }

    /// Whether the launch standby is the surface keys, paste, and the prompt
    /// band belong to. Selecting another session hands them back to that
    /// session's conversation; the launch standby keeps its text until the
    /// launch registers.
    pub fn launch_standby_capturing(&self) -> bool {
        self.launch_standby.is_some() && self.selected_session_id == self.launch_standby_anchor
    }

    /// Hands the launch standby to the session that has just registered: it
    /// becomes that session's standby composer, and the prompts queued in it
    /// are returned oldest first so the host can have the daemon deliver
    /// each one.
    pub fn adopt_launch_standby(&mut self, session_id: &str) -> Vec<String> {
        self.launch_standby_anchor = None;
        let Some(mut standby) = self.launch_standby.take() else {
            return Vec::new();
        };
        standby.adopt_session_id(session_id);
        let header = self.standby_prompt_header(session_id);
        standby.set_subagent_count(header.subagent_count);
        standby.set_header_summary(header.target, header.profile, header.title);
        if let Some(harness_kind) = header.harness_kind {
            standby.set_harness_kind(harness_kind);
        }
        let queued = standby.queued_prompt_texts();
        self.standby_prompts.insert(session_id.to_owned(), standby);
        queued
    }

    /// Seeds the standby composer from a warm chat's input, so a restart
    /// carries the text on screen through the transition instead of blanking
    /// it.
    pub fn seed_standby_prompt(&mut self, session_id: &str, text: String) {
        if text.is_empty() {
            return;
        }
        self.standby_prompt_mut(session_id).set_draft(text);
    }

    /// Puts a queued prompt back into the standby composer after the daemon
    /// refused it: the preview goes away and the text returns in front of
    /// whatever has since been typed.
    pub fn restore_standby_prompt(&mut self, session_id: &str, text: &str) {
        let standby = self.standby_prompt_mut(session_id);
        standby.remove_queued_prompt_text(text);
        let draft = standby.draft();
        let restored = if draft.is_empty() {
            text.to_owned()
        } else {
            format!("{text}\n{draft}")
        };
        standby.set_draft(restored);
    }

    /// Removes a session's standby composer and returns its draft, for the
    /// chat open that adopts it as the composer's starting input.
    pub fn take_standby_prompt_draft(&mut self, session_id: &str) -> Option<String> {
        self.standby_prompts
            .remove(session_id)
            .map(|standby| standby.draft())
    }

    /// Keys for the standby composer shown while a Starting/Resuming
    /// transition or an in-flight attach owns the selected session. It is the
    /// real composer, so the whole readline chord set edits the draft; only
    /// the dashboard's own chords are reserved, which keeps, say, Alt-X
    /// cancel working while typing. `Enter` on a plain prompt returns
    /// `QueueStartupPrompt` so the host can have the daemon deliver it when
    /// the session is live; a command keeps the draft and explains. `Some`
    /// means the key was consumed, including as a no-op.
    ///
    /// The launch standby, which has no session id yet, takes the same keys
    /// the same way; its Enter only keeps the preview, because the prompt is
    /// queued when the session registers and adopts the composer.
    pub(crate) fn handle_standby_prompt_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if self.focus != Focus::Prompt {
            return None;
        }
        let session_id = match self.standby_prompt_session() {
            Some(session_id) => Some(session_id.to_owned()),
            None if self.launch_standby_capturing() => None,
            None => return None,
        };
        // Chords the dashboard answers from every surface — the palette, the
        // pane keys, canceling an operation — still belong to it.
        if crate::actions::spec_for_key(key, self.focus).is_some() {
            return None;
        }
        // On macOS the dashboard's primary accelerator is represented by
        // SUPER, while the chat composer implements readline controls as
        // CONTROL. Once the dashboard has declined the chord, keep that
        // platform convention from turning Ctrl-A/K/Y into inserted text.
        let key = standby_prompt_key(key);
        let action = match &session_id {
            Some(session_id) => self.standby_prompt_mut(session_id).handle_key(key),
            None => self
                .launch_standby
                .as_mut()
                .expect("the launch standby is capturing keys")
                .handle_key(key),
        };
        match action {
            ChatAction::CycleFocus { reverse } => {
                self.cycle_focus(reverse);
            }
            ChatAction::Prompt(text) => {
                self.record_event_handled();
                let Some(session_id) = session_id else {
                    // The launch standby has kept the text as a preview; it is
                    // handed to the daemon when the session registers.
                    return Some(DashboardAction::None);
                };
                return Some(DashboardAction::QueueStartupPrompt { session_id, text });
            }
            ChatAction::PasteFromClipboard => {
                // Clipboard reads and image attachments belong to the attached
                // chat; until then the chord is answered honestly instead of
                // silently doing nothing.
                self.set_notice(
                    "Pasting from the clipboard opens when the session is live; the draft is kept.",
                );
            }
            _ => {}
        }
        self.record_event_handled();
        Some(DashboardAction::None)
    }
}
