use super::*;

impl DashboardState {
    /// Records the conversation the focused pane shows, which decides which
    /// project the compact Sessions list belongs to.
    pub fn set_current_session(&mut self, session_id: Option<&str>) {
        self.set_pane_session(self.navigation.layout().focused(), session_id);
    }

    /// Assigns the conversation a pane should show before background loading.
    /// Background completion never changes this assignment.
    pub fn set_pane_session(&mut self, pane: crate::tile_layout::PaneId, session_id: Option<&str>) {
        if !self.navigation.assign(pane, session_id) {
            return;
        }
        self.reconcile_pins();
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
        self.navigation.selected()
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
        self.current_session_id()
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
    /// Whether the session's target failed, so there is no worker to attach
    /// to. Opening one is a decision (read the transcript, or recover it),
    /// which is why the automatic startup pick and the selection follow
    /// leave it alone and the band explains instead.
    pub fn session_failed(&self, session_id: &str) -> bool {
        self.state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.state == SessionState::Error)
    }

    /// What to show for a failed session: its recorded error, or a plain
    /// sentence when the daemon recorded none.
    pub(crate) fn session_failure_text(&self, session_id: &str) -> String {
        self.state
            .sessions
            .get(session_id)
            .and_then(|session| session.last_error.clone())
            .unwrap_or_else(|| "The session's worker is no longer running.".to_owned())
    }

    /// A daemon notice about a session in another workspace names that
    /// workspace first, so a footer that cuts the tail still says where to
    /// look.
    pub fn notice_naming_workspace(&self, session_id: &str, text: String) -> String {
        let Some(session) = self.state.sessions.get(session_id) else {
            return text;
        };
        if self.active_workspace_id.as_deref() == Some(session.workspace_id.as_str()) {
            return text;
        }
        let name = self
            .workspace_display_name(&session.workspace_id)
            .to_owned();
        format!("In workspace {name}: {text}")
    }

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

    /// Lifecycle transitions which retain an editable prompt.
    pub fn transition_accepts_prompts(kind: SessionTransitionKind) -> bool {
        matches!(
            kind,
            SessionTransitionKind::Starting
                | SessionTransitionKind::Resuming
                | SessionTransitionKind::Moving
        )
    }
    /// The selected session whose prompt band shows the standby composer: a
    /// Starting, Resuming, or Moving transitions park the conversation
    /// behind it, or an attach for the session is in flight. Stopping,
    /// Destroying, and failed transitions keep the status panel instead:
    /// there is no conversation to type toward.
    pub(crate) fn standby_prompt_session(&self) -> Option<&str> {
        let session_id = self.current_session_id()?;
        if self.session_failed(session_id) || self.transition_failure_kind(session_id).is_some() {
            return None;
        }
        let parked = self
            .transition_kind(session_id)
            .is_some_and(Self::transition_accepts_prompts)
            || self.opening_session.as_deref() == Some(session_id);
        parked.then_some(session_id)
    }

    /// The standby composer a session's prompt band is editing, creating it on
    /// first use so every host path (seeding, keys, paste, render) shares one
    /// instance.
    pub(crate) fn standby_prompt_mut(&mut self, session_id: &str) -> &mut ChatState {
        let reviewers = self.reviewer_counts_for(session_id);
        if !self.standby_prompts.contains_key(session_id) {
            let standby = self.build_standby_prompt(session_id);
            self.standby_prompts.insert(session_id.to_owned(), standby);
        }
        let prompt = self
            .standby_prompts
            .get_mut(session_id)
            .expect("standby prompt was just inserted");
        prompt.set_reviewer_counts(reviewers);
        prompt
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
                    session.listed_title().to_owned()
                }
            }),
            harness_kind: session.as_ref().map(|session| session.harness_kind),
            subagent_count: self.subagent_count_for(session_id),
            reviewers: self.reviewer_counts_for(session_id),
        }
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

    /// Removes a session's standby composer and returns its draft, for the
    /// chat open that adopts it as the composer's starting input.
    pub fn take_standby_prompt_draft(&mut self, session_id: &str) -> Option<String> {
        self.standby_prompts
            .remove(session_id)
            .map(|standby| standby.draft())
    }

    pub fn retain_startup_receipt(
        &mut self,
        session_id: &str,
        prompt: mj_chat::chat::StartupPrompt,
        inherited_draft: Option<String>,
    ) {
        self.standby_prompt_mut(session_id)
            .retain_startup_receipt(prompt, inherited_draft);
    }
    pub fn settle_startup_preview(&mut self, session_id: &str, command_id: &str) {
        if let Some(chat) = self.standby_prompts.get_mut(session_id) {
            chat.settle_startup_preview(command_id);
        }
    }
    pub fn restore_startup_prompt(
        &mut self,
        session_id: &str,
        prompt: mj_chat::chat::StartupPrompt,
    ) -> anyhow::Result<()> {
        self.standby_prompt_mut(session_id)
            .restore_startup_prompt(prompt)
    }
    pub fn startup_prompt_withdrawn(
        &mut self,
        session_id: &str,
        prompt: mj_chat::chat::StartupPrompt,
        result: Result<bool, String>,
    ) -> anyhow::Result<bool> {
        self.standby_prompt_mut(session_id)
            .startup_prompt_withdrawn(prompt, result)
    }
    pub fn set_standby_move_origin(
        &mut self,
        session_id: &str,
        origin: Option<(u64, Option<String>)>,
    ) {
        self.standby_prompt_mut(session_id)
            .set_composer_move_origin(origin);
    }
    pub fn settle_standby_move_origin(
        &mut self,
        session_id: &str,
        generation: u64,
        id: Option<&str>,
        succeeded: bool,
    ) {
        if let Some(chat) = self.standby_prompts.get_mut(session_id) {
            chat.settle_deferred_move_origin(generation, id, succeeded);
        }
    }
    pub fn keep_standby_submission_as_draft(&mut self, session_id: &str) {
        if let Some(chat) = self.standby_prompts.get_mut(session_id) {
            chat.keep_deferred_submission_as_draft();
        }
    }
    pub fn standby_session_ids(&self) -> Vec<String> {
        self.standby_prompts.keys().cloned().collect()
    }
    pub fn has_standby_composer(&self, session_id: &str) -> bool {
        self.standby_prompts.contains_key(session_id)
    }
    /// Snapshot without disturbing pending reads or image processing.
    pub fn standby_prompt_draft(&self, session_id: &str) -> Option<String> {
        self.standby_prompts.get(session_id).map(ChatState::draft)
    }
    pub fn take_standby_composer(
        &mut self,
        session_id: &str,
    ) -> Option<mj_chat::chat::LiveComposer> {
        self.standby_prompts
            .remove(session_id)
            .and_then(|mut chat| chat.take_live_composer())
    }
    pub fn adopt_standby_composer(
        &mut self,
        session_id: &str,
        composer: mj_chat::chat::LiveComposer,
    ) {
        self.standby_prompt_mut(session_id)
            .adopt_live_composer(composer);
    }
    pub fn standby_startup_prompts(&self, session_id: &str) -> Vec<mj_chat::chat::StartupPrompt> {
        self.standby_prompts
            .get(session_id)
            .map(ChatState::startup_prompts)
            .unwrap_or_default()
    }
    /// Drain every composer, including those hidden by navigation. A result belongs to its original owner.
    pub fn drain_standby_composers(&mut self) -> (bool, Vec<DashboardAction>) {
        let mut changed = false;
        let mut actions = Vec::new();
        for (session_id, chat) in &mut self.standby_prompts {
            changed |= chat.has_composer_updates();
            for prompt in chat.take_prepared_startup_prompts() {
                actions.push(DashboardAction::QueueStartupPrompt {
                    session_id: session_id.clone(),
                    prompt,
                });
                changed = true;
            }
            if let Some(prompt) = chat.drain_composer() {
                actions.push(DashboardAction::QueueStartupPrompt {
                    session_id: session_id.clone(),
                    prompt,
                });
            }
        }
        (changed, actions)
    }

    /// Keys for the standby composer shown while a Starting/Resuming/Moving
    /// transition or an in-flight attach owns the selected session. It is the
    /// real composer, so the whole readline chord set edits the draft; only
    /// the dashboard's own pane keys are reserved. Everything configurable is
    /// matched by the prefix router before the key reaches here. `Enter` on a
    /// plain prompt returns `QueueStartupPrompt` so the host can have the
    /// daemon deliver it when the session is live; a command keeps the draft
    /// and explains. `Some` means the key was consumed, including as a no-op.
    pub(crate) fn handle_standby_prompt_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if self.focus != Focus::Prompt {
            return None;
        }
        let session_id = self.standby_prompt_session()?.to_owned();
        // Pane keys still belong to the dashboard; the prefix router has
        // already taken everything a user can rebind.
        if crate::actions::pane_command_for_key(key, self.focus).is_some() {
            return None;
        }
        // On macOS the dashboard's primary accelerator is represented by
        // SUPER, while the chat composer implements readline controls as
        // CONTROL. Once the dashboard has declined the chord, keep that
        // platform convention from turning Ctrl-A/K/Y into inserted text.
        let key = standby_prompt_key(key);
        let action = self.standby_prompt_mut(&session_id).handle_key(key);
        match action {
            ChatAction::CycleFocus { reverse } => {
                self.cycle_focus(reverse);
            }
            ChatAction::Prompt(_) => {
                self.record_event_handled();
                let prompt = self
                    .standby_prompt_mut(&session_id)
                    .latest_startup_prompt()
                    .expect("submitted standby prompt has a preview");
                return Some(DashboardAction::QueueStartupPrompt { session_id, prompt });
            }
            ChatAction::RemoveQueuedPrompt { id, text, .. } => {
                // Up recalled the newest queued prompt into the composer. The
                // preview is gone here already; the daemon must drop its copy
                // too, or it would send the original when the session is ready.
                self.record_event_handled();
                let prompt = self
                    .standby_prompt_mut(&session_id)
                    .recalled_startup_prompt(&id, &text);
                return Some(DashboardAction::WithdrawStartupPrompt { session_id, prompt });
            }
            action @ (ChatAction::PasteFromClipboard | ChatAction::AttachPaste { .. }) => {
                self.standby_prompt_mut(&session_id)
                    .dispatch_composer_action(action);
            }
            _ => {}
        }
        self.record_event_handled();
        Some(DashboardAction::None)
    }
}
