use super::*;

impl DashboardState {
    /// The workspace currently used as the Sessions-pane filter.
    pub fn active_workspace_id(&self) -> Option<&str> {
        self.active_workspace_id.as_deref()
    }

    pub fn subagent_parent_id(&self) -> Option<&str> {
        self.subagent_parent_id.as_deref()
    }

    pub fn open_subagent_workspace(&mut self, parent_id: String) {
        if !self.state.sessions.contains_key(&parent_id) {
            return;
        }
        self.subagent_parent_id = Some(parent_id.clone());
        self.selected_session_id = self
            .state
            .subagents
            .values()
            .find(|record| record.parent_session_id == parent_id)
            .map(|record| record.child_session_id.clone())
            .or_else(|| {
                self.native_agents
                    .iter()
                    .find(|(_, pane)| pane.agent.parent_view_id() == parent_id)
                    .map(|(id, _)| id.clone())
            });
        self.set_current_session(None);
        self.focus = Focus::Sessions;
        self.clamp_selections();
    }

    pub fn close_subagent_workspace(&mut self) {
        let Some(parent_id) = self.subagent_parent_id.take() else {
            return;
        };
        self.subagent_parent_id = self.subagent_parent_for(&parent_id);
        self.selected_session_id = Some(parent_id);
        self.set_current_session(None);
        self.clamp_selections();
    }

    /// Records that a handler took responsibility for the current event.
    pub(crate) fn record_event_handled(&self) {
        self.last_event_consumed.set(true);
    }

    /// Workspace ids in stable tab order. Runtime snapshots may remove an id;
    /// the controller decides which replacement tab to select.
    pub(crate) fn workspace_ids(&self) -> Vec<String> {
        self.workspace_order.clone()
    }

    pub(crate) fn workspace_display_name<'a>(&'a self, workspace_id: &'a str) -> &'a str {
        self.workspace_names
            .get(workspace_id)
            .map(String::as_str)
            .unwrap_or(workspace_id)
    }

    /// Select a tab locally. The caller should save any open chat draft before
    /// invoking this setter; the setter itself performs no external work.
    pub fn set_active_workspace(&mut self, workspace_id: Option<String>) {
        self.resize_mode = false;
        if let Mode::WorkspaceManager(manager) = &mut self.mode {
            manager.active_workspace_id = workspace_id.clone();
        }
        if self.active_workspace_id == workspace_id {
            self.clamp_selections();
            return;
        }
        let workspace_focused = self.focus == Focus::Workspaces;
        if let Some(current) = self.active_workspace_id.clone() {
            self.workspace_views
                .insert(current, WorkspaceViewState::from_dashboard(self));
        }

        self.switch_go_workspace(workspace_id.as_deref());
        self.sessions_filter = None;
        self.active_workspace_id = workspace_id.clone();
        self.workspace_name = workspace_id
            .as_deref()
            .map(|id| self.workspace_display_name(id).to_owned())
            .unwrap_or_default();
        self.opening_session = None;
        if let Some(workspace_id) = workspace_id {
            if let Some(view) = self.workspace_views.get(&workspace_id).cloned() {
                self.selected_session_id = view.selected_session_id;
                self.sessions_scroll.set(view.sessions_scroll);
                self.targets_scroll.set(view.targets_scroll);
                self.quota_scroll.set(view.quota_scroll);
                self.capacity_index = view.capacity_index;
                self.quota_index = view.quota_index;
                self.pane_sizes = view.pane_sizes;
                self.restore_conversation_layout(&view.conversation_layout);
                self.collapsed_project_keys = view.collapsed_project_keys;
                self.focus = view.focus;
            } else {
                self.selected_session_id = self
                    .go
                    .as_ref()
                    .and_then(|mode| mode.last_session_id.clone());
                self.sessions_scroll.set(0);
                self.targets_scroll.set(0);
                self.quota_scroll.set(0);
                self.capacity_index = 0;
                self.quota_index = 0;
                self.pane_sizes = PaneSizes::default();
                self.reset_conversation_layout();
                self.collapsed_project_keys.clear();
                self.focus = Focus::Sessions;
            }
        } else {
            self.selected_session_id = None;
            self.sessions_scroll.set(0);
            self.targets_scroll.set(0);
            self.quota_scroll.set(0);
            self.capacity_index = 0;
            self.quota_index = 0;
            self.pane_sizes = PaneSizes::default();
            self.reset_conversation_layout();
            self.collapsed_project_keys.clear();
            self.focus = Focus::Sessions;
        }
        self.clamp_selections();
        if workspace_focused {
            self.focus = Focus::Workspaces;
        }
    }

    /// Applies a controller-provided pane-size cache unless this client has
    /// edited that workspace's layout since the cache was requested.
    pub fn cache_workspace_pane_sizes(&mut self, workspace_id: &str, sizes: PaneSizes) {
        if sizes.validate().is_err() || self.workspace_pane_sizes_modified.contains(workspace_id) {
            return;
        }
        if self.active_workspace_id.as_deref() == Some(workspace_id) {
            self.pane_sizes = sizes;
            self.clamp_selections();
        }
        self.workspace_views
            .entry(workspace_id.to_owned())
            .or_insert_with(|| WorkspaceViewState {
                selected_session_id: None,
                sessions_scroll: 0,
                targets_scroll: 0,
                quota_scroll: 0,
                capacity_index: 0,
                quota_index: 0,
                pane_sizes: sizes,
                conversation_layout: mj_core::workspace::ConversationLayout::default(),
                collapsed_project_keys: BTreeSet::new(),
                focus: Focus::Sessions,
            })
            .pane_sizes = sizes;
    }

    /// Whether the local client has edited this workspace's pane layout.
    pub fn workspace_pane_sizes_modified(&self, workspace_id: &str) -> bool {
        self.workspace_pane_sizes_modified.contains(workspace_id)
    }

    pub(crate) fn register_workspace_tab_area(&mut self, workspace_id: String, area: Rect) {
        self.workspace_tab_areas.push((workspace_id, area));
    }

    pub(crate) fn clear_workspace_tab_areas(&mut self) {
        self.workspace_tab_areas.clear();
        self.workspace_hamburger_area = None;
        self.subagent_workspace_close_area = None;
    }

    /// Moves the Sessions selection onto `session_id` without changing focus.
    pub fn select_active_session(&mut self, session_id: &str) {
        if self
            .ordered_sessions()
            .iter()
            .any(|session| session.id == session_id)
            && self.selected_session_id.as_deref() != Some(session_id)
        {
            self.selected_session_id = Some(session_id.to_owned());
        }
    }

    /// Select the newly created session and use the ordinary composer.
    pub fn finish_new_session(&mut self, session_id: &str) {
        self.select_active_session(session_id);
        self.focus_pane(self.browse_pane());
        self.request_selected_browse();
        self.focus_prompt();
    }

    /// Global visibility must not change which workspace opens automatically.
    pub fn startup_sessions(&self) -> impl Iterator<Item = &SessionRecord> {
        self.state.sessions.values().filter(|session| {
            session.state.is_active()
                && self
                    .active_workspace_id
                    .as_ref()
                    .is_some_and(|workspace_id| session.workspace_id == *workspace_id)
        })
    }

    /// The part of the combined surface that owns the keyboard.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn prompt_has_focus(&self) -> bool {
        self.focus == Focus::Prompt
    }

    pub(crate) fn set_session_action_focus(&mut self, action: Option<CommandId>) {
        if self.session_action_focus != action {
            self.session_action_focus = action;
        }
    }

    pub fn focus_prompt(&mut self) -> bool {
        if self.focus == Focus::Prompt {
            return false;
        }
        self.focus = Focus::Prompt;
        self.workspace_control_focus = WorkspaceControlFocus::Tabs;
        self.set_session_action_focus(None);
        true
    }

    /// Focuses the Sessions pane without changing its explicit size.
    pub fn focus_sessions(&mut self) -> bool {
        let before = self.focus;
        self.focus = Focus::Sessions;
        self.workspace_control_focus = WorkspaceControlFocus::Tabs;
        self.set_session_action_focus(None);
        self.clamp_selections();
        before != self.focus
    }

    /// Moves focus one stop along the Tab ring.
    ///
    /// Pane sizes are the user's setting, so Tab never changes them.
    pub fn cycle_focus(&mut self, reverse: bool) -> bool {
        let previous = self.focus;
        self.focus = cycle_control(self.focus, &FOCUS_ORDER, reverse);
        self.workspace_control_focus =
            if reverse && previous == Focus::Sessions && self.focus == Focus::Workspaces {
                WorkspaceControlFocus::Menu
            } else {
                WorkspaceControlFocus::Tabs
            };
        if self.focus != Focus::Sessions {
            self.set_session_action_focus(None);
        }
        self.clamp_selections();
        previous != self.focus
    }
}
