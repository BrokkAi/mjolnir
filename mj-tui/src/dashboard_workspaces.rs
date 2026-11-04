use super::*;

/// What the dashboard did about sub-agents whose parent's suspend stopped
/// them, for the host to finish: their conversations go without saving a
/// draft, since their records are gone, and the parent's conversation opens
/// where the view went back to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoppedBySuspend {
    pub sessions: Vec<String>,
    pub reopen: Option<String>,
}

/// A sub-agent whose record left because its parent's suspend stopped it.
pub(crate) struct SubagentStoppedBySuspend {
    session_id: String,
    parent_id: String,
    title: String,
    /// Its conversation, or its own sub-agents, were on screen.
    shown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChildWorkspace {
    pub parent_id: String,
    pub kind: mj_core::subagent::SubagentKind,
}

impl DashboardState {
    /// The workspace currently used as the Sessions-pane filter.
    pub fn active_workspace_id(&self) -> Option<&str> {
        self.active_workspace_id.as_deref()
    }

    pub fn subagent_parent_id(&self) -> Option<&str> {
        self.child_workspace
            .as_ref()
            .map(|workspace| workspace.parent_id.as_str())
    }

    pub fn reviewers_workspace_open(&self) -> bool {
        self.child_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.kind == mj_core::subagent::SubagentKind::Reviewer)
    }

    pub(crate) fn child_workspace_for(&self, child: &str) -> Option<ChildWorkspace> {
        Some(ChildWorkspace {
            parent_id: self.subagent_parent_for(child)?,
            kind: self
                .state
                .subagents
                .get(child)
                .map_or(mj_core::subagent::SubagentKind::Worker, |relation| {
                    relation.kind
                }),
        })
    }

    pub fn open_subagent_workspace(&mut self, parent_id: String) {
        self.open_child_workspace(parent_id, mj_core::subagent::SubagentKind::Worker);
    }

    pub fn open_reviewer_workspace(&mut self, parent_id: String) {
        self.open_child_workspace(parent_id, mj_core::subagent::SubagentKind::Reviewer);
    }

    fn open_child_workspace(&mut self, parent_id: String, kind: mj_core::subagent::SubagentKind) {
        if !self.state.sessions.contains_key(&parent_id) {
            return;
        }
        self.switch_child_scope(Some(ChildWorkspace {
            parent_id: parent_id.clone(),
            kind,
        }));
        // The pane now lists a different set of rows; the parent list's
        // scroll position means nothing here.
        self.sessions_scroll.set(0);
        self.set_current_session(None);
        // Compact relationships still need a selection to load their details;
        // only rows with known activity can be excluded by the idle filter.
        let child = self
            .derive_ordered_sessions()
            .0
            .into_iter()
            .map(|session| session.id.clone())
            .chain(
                self.managed_child_ids_of_kind(&parent_id, kind)
                    .into_iter()
                    .filter(|id| !self.state.sessions.contains_key(id)),
            )
            .next();
        if let Some(child) = child {
            self.select_active_session(&child);
        }
        self.focus = Focus::Sessions;
        self.clamp_selections();
    }

    pub fn close_subagent_workspace(&mut self) {
        let Some(workspace) = self.child_workspace.as_ref().cloned() else {
            return;
        };
        let parent_id = workspace.parent_id;
        let next = self.child_workspace_for(&parent_id);
        self.switch_child_scope(next);
        self.sessions_scroll.set(0);
        self.select_active_session(&parent_id);
        self.clamp_selections();
    }

    pub(crate) fn switch_child_scope(&mut self, next: Option<ChildWorkspace>) {
        if *self.child_workspace == next {
            return;
        }
        let previous = std::mem::replace(&mut *self.child_workspace, next.clone());
        self.sessions_filters_by_scope
            .insert(previous, self.sessions_filter.take());
        *self.sessions_filter = self
            .sessions_filters_by_scope
            .remove(&next)
            .unwrap_or_else(|| {
                next.filter(|workspace| workspace.kind == mj_core::subagent::SubagentKind::Worker)
                    .map(|_| SessionsFilter {
                        state: Some(SessionStateFilter::NotIdle),
                        ..Default::default()
                    })
            });
    }

    /// The active sub-agents that `next` no longer has because their parent's
    /// suspend, or its discard, stopped them: the parent is still there, and
    /// it is being suspended or already is.
    pub(crate) fn subagents_stopped_by_suspend(
        &self,
        next: &State,
    ) -> Vec<SubagentStoppedBySuspend> {
        let candidates = self
            .durable_records
            .changes(&next.sessions)
            .chain(self.presented_records.changes(&self.state.sessions))
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>();
        #[cfg(test)]
        self.reconciliation_visits
            .set(self.reconciliation_visits.get() + candidates.len());
        candidates
            .into_iter()
            .filter(|id| !next.sessions.contains_key(id))
            .filter_map(|id| self.state.subagents.get(&id))
            .filter_map(|relation| {
                let child = self
                    .state
                    .sessions
                    .get(&relation.child_session_id)
                    .filter(|child| child.state.is_active())?;
                let parent_id = &relation.parent_session_id;
                let parent_suspending = self.session_operation_kind(parent_id)
                    == Some(SessionOperationKind::Suspending);
                let suspending = match next.sessions.get(parent_id) {
                    Some(parent) => {
                        parent_suspending
                            || matches!(
                                parent.state,
                                SessionState::Closing
                                    | SessionState::Destroying
                                    | SessionState::Stopped
                            )
                    }
                    // The feed carries only live sessions, so a parent that
                    // finished its suspend can leave in the same frame.
                    None => {
                        parent_suspending
                            || self.state.sessions.get(parent_id).is_some_and(|parent| {
                                matches!(
                                    parent.state,
                                    SessionState::Closing | SessionState::Destroying
                                )
                            })
                    }
                };
                let shown = [self.subagent_parent_id(), self.selected_session_id()]
                    .into_iter()
                    .flatten()
                    .chain(self.navigation.sessions().values().map(String::as_str))
                    .any(|id| id == child.id);
                suspending.then(|| SubagentStoppedBySuspend {
                    session_id: child.id.clone(),
                    parent_id: parent_id.clone(),
                    title: child.listed_title().to_owned(),
                    shown,
                })
            })
            .collect()
    }

    /// Takes the sub-agents stopped by their parent's suspend off the view
    /// (R15-1). Their records are gone, so a Sub-agents view that showed them
    /// would fall to an empty list: it goes back to the parent's scope with
    /// the parent selected, and says what happened instead.
    pub(crate) fn leave_subagents_stopped_by_suspend(
        &mut self,
        stopped: Vec<SubagentStoppedBySuspend>,
    ) {
        let parents = stopped
            .iter()
            .map(|child| child.parent_id.clone())
            .collect::<BTreeSet<_>>();
        for parent_id in parents {
            let children = stopped
                .iter()
                .filter(|child| child.parent_id == parent_id)
                .collect::<Vec<_>>();
            let in_view = children.iter().any(|child| child.shown)
                || self.subagent_parent_id() == Some(parent_id.as_str());
            if !in_view {
                continue;
            }
            let next = self.child_workspace_for(&parent_id);
            self.switch_child_scope(next);
            self.select_active_session(&parent_id);
            self.clamp_selections();
            let titles = children
                .iter()
                .map(|child| child.title.clone())
                .collect::<Vec<_>>();
            let named = mj_core::subagent::quoted_titles(&titles);
            self.set_notice(if titles.len() == 1 {
                format!("Sub-agent {named} was stopped by the suspend")
            } else {
                format!("Sub-agents {named} were stopped by the suspend")
            });
            // A parent that left the feed has stopped; there is nothing to
            // reopen.
            if self.state.sessions.contains_key(&parent_id) {
                self.stopped_by_suspend.reopen = Some(parent_id);
            }
        }
        self.stopped_by_suspend
            .sessions
            .extend(stopped.into_iter().map(|child| child.session_id));
    }

    /// What the host has to finish for sub-agents a suspend stopped since it
    /// last asked.
    pub fn take_stopped_by_suspend(&mut self) -> StoppedBySuspend {
        std::mem::take(&mut self.stopped_by_suspend)
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
        if *self.active_workspace_id == workspace_id {
            self.clamp_selections();
            return;
        }
        let workspace_focused = self.focus == Focus::Workspaces;
        if let Some(current) = self.active_workspace_id.clone() {
            self.workspace_views
                .insert(current, WorkspaceViewState::from_dashboard(self));
        }

        self.switch_go_workspace(workspace_id.as_deref());
        *self.sessions_filter = None;
        *self.active_workspace_id = workspace_id.clone();
        self.workspace_name = workspace_id
            .as_deref()
            .map(|id| self.workspace_display_name(id).to_owned())
            .unwrap_or_default();
        self.opening_session = None;
        if let Some(workspace_id) = workspace_id {
            if let Some(view) = self.workspace_views.get(&workspace_id).cloned() {
                self.sessions_scroll.set(view.sessions_scroll);
                self.targets_scroll.set(view.targets_scroll);
                self.quota_scroll.set(view.quota_scroll);
                self.capacity_index = view.capacity_index;
                self.quota_index = view.quota_index;
                self.restore_workspace_pane_sizes(view.pane_sizes);
                self.restore_conversation_layout(&view.conversation_layout);
                *self.collapsed_project_keys = view.collapsed_project_keys;
                self.focus = view.focus;
            } else {
                self.sessions_scroll.set(0);
                self.targets_scroll.set(0);
                self.quota_scroll.set(0);
                self.capacity_index = 0;
                self.quota_index = 0;
                self.restore_workspace_pane_sizes(PaneSizes::default());
                self.reset_conversation_layout();
                if let Some(session) = self
                    .go
                    .as_ref()
                    .and_then(|mode| mode.last_session_id.clone())
                {
                    self.select_active_session(&session);
                }
                self.collapsed_project_keys.clear();
                self.focus = Focus::Sessions;
            }
        } else {
            self.sessions_scroll.set(0);
            self.targets_scroll.set(0);
            self.quota_scroll.set(0);
            self.capacity_index = 0;
            self.quota_index = 0;
            self.restore_workspace_pane_sizes(PaneSizes::default());
            self.reset_conversation_layout();
            self.collapsed_project_keys.clear();
            self.focus = Focus::Sessions;
        }
        if let Some(parent_id) = self.navigation_subagent_parent.clone() {
            if self.state.sessions.get(&parent_id).is_some_and(|record| {
                Some(record.workspace_id.as_str()) == self.active_workspace_id()
            }) {
                if self
                    .navigation_session
                    .as_deref()
                    .and_then(|child| self.state.subagents.get(child))
                    .is_some_and(|relation| {
                        relation.kind == mj_core::subagent::SubagentKind::Reviewer
                    })
                {
                    self.open_reviewer_workspace(parent_id);
                } else {
                    self.open_subagent_workspace(parent_id);
                }
                if let Some(session) = self.navigation_session.clone() {
                    self.select_active_session(&session);
                    self.clear_filter_hiding_selection();
                }
            }
            self.navigation_subagent_parent = None;
        } else if let Some(session) = self.navigation_session.clone()
            && self.state.sessions.get(&session).is_some_and(|record| {
                Some(record.workspace_id.as_str()) == self.active_workspace_id()
            })
        {
            self.select_active_session(&session);
            self.clear_filter_hiding_selection();
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
            self.restore_workspace_pane_sizes(sizes);
            self.clamp_selections();
        }
        self.workspace_views
            .entry(workspace_id.to_owned())
            .or_insert_with(|| WorkspaceViewState {
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
        self.workspace_dropdown_area = None;
        self.workspace_scroll_geometry = None;
        self.workspace_hamburger_area = None;
        self.subagent_workspace_close_area = None;
    }

    /// Select a conversation immediately, retaining the current keyboard focus.
    /// Pinned sessions stay in their pane; other sessions replace Browse.
    pub fn select_active_session(&mut self, session_id: &str) {
        // Navigation owns the selection immediately, even while the daemon
        // is loading the row detail for this family's compact relationship.
        let unloaded_child = self.state.subagents.get(session_id).is_some_and(|child| {
            self.subagent_parent_id() == Some(child.parent_session_id.as_str())
        });
        if !self.state.sessions.contains_key(session_id) && !unloaded_child {
            return;
        }
        let pane = self
            .pane_for_session(session_id)
            .unwrap_or(self.browse_pane());
        self.reveal_pane(pane);
        self.set_pane_session(pane, Some(session_id));
        self.focus_pane(pane);
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
        matches!(self.mode, crate::Mode::Dashboard) && self.focus == Focus::Prompt
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
