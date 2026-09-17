use super::*;

use ratatui::style::Style;

use mj_client::quota::API_LABEL;

use crate::render::{headroom_color, quota_remaining_percent, weekly_quota_exhausted};

impl DashboardState {
    pub(crate) fn selected_session(&self) -> Option<&SessionRecord> {
        let selected = self.selected_session_id.as_deref()?;
        self.ordered_sessions()
            .into_iter()
            .find(|session| session.id == selected)
    }

    /// The live sessions the Sessions pane is showing, as indices into
    /// [`Self::ordered_sessions`].
    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(index),
                _ => None,
            })
            .collect()
    }

    /// The rows the Sessions pane draws at every explicit size: a heading per
    /// project and one row per live session.
    ///
    /// Standard and Maximized use four-line session rows; Minimized keeps only
    /// each session's top summary line. Focus never changes the representation.
    pub(crate) fn sessions_rows(&self) -> Vec<SessionsRow> {
        let sessions = self.ordered_sessions();
        self.expanded_sessions_rows(&sessions)
    }

    pub(crate) fn expanded_sessions_rows(&self, sessions: &[&SessionRecord]) -> Vec<SessionsRow> {
        // Two projects can share a short name, in which case both need their
        // full names to stay distinguishable.
        let mut short_names = BTreeMap::<String, BTreeSet<String>>::new();
        for session in sessions {
            let source = self.project_source(session);
            short_names
                .entry(source.short)
                .or_default()
                .insert(source.key);
        }
        let numbered = self.project_keys().len() > 1;
        let mut rows = Vec::new();
        let mut previous = None;
        let mut number = 0;
        for (index, session) in sessions.iter().enumerate() {
            let source = self.project_source(session);
            if previous.as_ref() != Some(&source.key) {
                number += 1;
                let label = if short_names
                    .get(&source.short)
                    .is_some_and(|projects| projects.len() > 1)
                {
                    source.full.clone()
                } else {
                    source.short.clone()
                };
                rows.push(SessionsRow::ProjectHeading {
                    key: source.key.clone(),
                    label,
                    number: (numbered && number <= 9).then_some(number),
                });
                previous = Some(source.key.clone());
            }
            rows.push(SessionsRow::Session {
                index,
                expanded: !self.collapsed_project_keys.contains(&source.key),
            });
        }
        rows
    }

    /// Where the selection sits among the rows on screen, for the table's
    /// highlight. `None` when nothing is selected or the selection is not on
    /// screen.
    pub(crate) fn selected_visible_index(&self) -> Option<usize> {
        let selected = self.selected_session_id.as_deref()?;
        let sessions = self.ordered_sessions();
        self.visible_session_indices()
            .into_iter()
            .position(|index| sessions.get(index).is_some_and(|s| s.id == selected))
    }

    /// Sessions visible in the selected workspace, grouped by project and
    /// ordered by creation. Stopped sessions are included only when their
    /// advanced display setting is enabled; in-flight transitions remain
    /// visible regardless. The controller may feed all workspaces into one
    /// state snapshot; the tab is the local view filter.
    pub(crate) fn ordered_sessions(&self) -> Vec<&SessionRecord> {
        if let Some(parent_id) = self.subagent_parent_id.as_deref() {
            let mut children = self
                .state
                .subagents
                .values()
                .filter(|record| record.parent_session_id == parent_id)
                .filter_map(|record| self.state.sessions.get(&record.child_session_id))
                .collect::<Vec<_>>();
            children.sort_by_cached_key(|session| session.creation_order_key());
            return children;
        }
        let Some(active_workspace_id) = self.active_workspace_id.as_deref() else {
            return Vec::new();
        };
        let active = self
            .state
            .sessions
            .values()
            .filter(|session| {
                session.workspace_id == active_workspace_id
                    && !self.state.subagents.contains_key(&session.id)
                    && (session.state.is_active()
                        || self.transition_kind(&session.id).is_some()
                        || (self.config.advanced.show_stopped_sessions
                            && session.state == SessionState::Stopped))
            })
            .collect::<Vec<_>>();
        let inputs = active
            .iter()
            .map(|session| {
                let source = self.project_source(session);
                (
                    session.id.clone(),
                    session.created_at.clone(),
                    source.key,
                    source.short,
                    source.full,
                )
            })
            .collect::<Vec<_>>();
        let mut cache = self.session_order_cache.borrow_mut();
        if cache.inputs == inputs {
            return cache
                .ids
                .iter()
                .filter_map(|id| self.state.sessions.get(id))
                .collect();
        }
        let mut active = active;
        active.sort_by_cached_key(|session| session.creation_order_key());
        let mut groups = BTreeMap::<String, Vec<&SessionRecord>>::new();
        for session in active {
            groups
                .entry(self.project_source(session).key)
                .or_default()
                .push(session);
        }
        let mut groups = groups.into_values().collect::<Vec<_>>();
        // Display spelling must not split sessions with the same canonical key.
        groups.sort_by_cached_key(|sessions| {
            let source = self.project_source(sessions[0]);
            (source.short.to_lowercase(), source.full, source.key)
        });
        let ordered = groups.into_iter().flatten().collect::<Vec<_>>();
        cache.inputs = inputs;
        cache.ids = ordered.iter().map(|session| session.id.clone()).collect();
        ordered
    }

    pub fn project_source(&self, session: &SessionRecord) -> ProjectSourceIdentity {
        let session = self.state.project_identity_session(session);
        self.project_sources
            .get(&session.id)
            .cloned()
            .unwrap_or_else(|| session.project_source(&self.config))
    }

    pub fn has_resolved_project_source(&self, session_id: &str) -> bool {
        self.project_sources.contains_key(session_id)
    }

    pub fn set_project_source(&mut self, session_id: &str, source: ProjectSourceIdentity) {
        if self.state.sessions.contains_key(session_id) {
            self.project_sources.insert(session_id.to_owned(), source);
            self.clamp_selections();
        }
    }

    pub(crate) fn project_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for session in self.ordered_sessions() {
            let key = self.project_source(session).key;
            if keys.last() != Some(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// Whether this session's project draws its full four-row form. Projects
    /// default to expanded; only an explicit collapse takes that away.
    pub fn project_is_expanded(&self, session: &SessionRecord) -> bool {
        !self
            .collapsed_project_keys
            .contains(&self.project_source(session).key)
    }

    /// Collapses an expanded project or expands a collapsed one, leaving
    /// every other project alone.
    pub(crate) fn toggle_project(&mut self, project_key: &str) {
        if !self.collapsed_project_keys.remove(project_key) {
            self.collapsed_project_keys.insert(project_key.to_owned());
        }
    }

    pub(crate) fn toggle_selected_project(&mut self) {
        let key = self
            .selected_session()
            .map(|session| self.project_source(session).key);
        if let Some(key) = key {
            self.toggle_project(&key);
        }
    }

    pub(crate) fn toggle_project_number(&mut self, number: usize) {
        if number == 0 {
            return;
        }
        if let Some(key) = self.project_keys().get(number - 1).cloned() {
            self.toggle_project(&key);
        }
    }

    pub(crate) fn mark_all_read(&mut self) -> DashboardAction {
        let mut receipts = Vec::new();
        for (session_id, detail) in &mut self.session_details {
            if !detail.has_unread() {
                continue;
            }
            let Some(through) = detail.materialized_applied_event_ordinal else {
                continue;
            };
            let Some(session) = self.state.sessions.get_mut(session_id) else {
                continue;
            };
            if through > session.viewed_through_event_ordinal {
                session.viewed_through_event_ordinal = through;
                detail.clear_unread();
                receipts.push((session_id.clone(), through));
            }
        }
        if receipts.is_empty() {
            self.set_notice("No unread sessions.");
            DashboardAction::None
        } else {
            self.set_notice("Marked all sessions read.");
            DashboardAction::MarkAllRead { receipts }
        }
    }

    pub(crate) fn compatible_profiles(&self, session_id: &str) -> Vec<(&String, HarnessKind)> {
        if !self.state.sessions.contains_key(session_id) {
            return Vec::new();
        }
        self.config
            .profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, profile)| (id, profile.kind))
            .collect()
    }

    /// One row of the profile picker tables: the warning marker, the profile
    /// id, the harness, and the two quota percentages remaining. The picker
    /// pads the cells into aligned columns and draws the marker's footnote
    /// below the table.
    pub(crate) fn profile_choice(&self, id: &str, harness: HarnessKind) -> PickerChoice {
        let (weekly, five_hour) = self.profile_quota_cells(id);
        PickerChoice::table(vec![
            guardian_warning_marker(harness),
            PickerCell::text(id),
            PickerCell::text(harness.display_name()),
            weekly,
            five_hour,
        ])
    }

    /// The WEEKLY and 5H cells of a profile picker row, as percentages
    /// remaining coloured by how much headroom they leave. The five-hour cell
    /// stays blank whenever there is no five-hour figure worth reading: a
    /// profile that is refreshing, failing, usage-priced, or out of weekly
    /// quota altogether.
    fn profile_quota_cells(&self, id: &str) -> (PickerCell, PickerCell) {
        let plain = |text: &str| (PickerCell::text(text), PickerCell::blank());
        if self.quota_refreshing.contains(id) {
            return plain("refreshing");
        }
        let Some(quota) = self.quotas.get(id) else {
            return plain("refreshing");
        };
        if quota.error.is_some() {
            return plain(
                &quota
                    .error_label()
                    .unwrap_or_else(|| "unavailable".to_string()),
            );
        }
        if quota.is_usage_priced() {
            return plain(API_LABEL);
        }
        let Some(weekly) = quota.weekly_window().and_then(quota_remaining_percent) else {
            return plain("unavailable");
        };
        let five_hour = if weekly_quota_exhausted(quota) {
            None
        } else {
            quota.five_hour_window().and_then(quota_remaining_percent)
        };
        let percent = |value: u8| {
            PickerCell::styled(
                format!("{value}%"),
                Style::default().fg(headroom_color(value)),
            )
        };
        (
            percent(weekly),
            five_hour.map_or_else(PickerCell::blank, percent),
        )
    }

    /// The selected session, if its target template creates a container.
    pub(crate) fn selected_container_session(&self) -> Option<&SessionRecord> {
        let session = self.selected_session()?;
        matches!(
            self.config.targets.get(&session.target_template_id)?,
            HelTargetTemplate::LocalPodman { .. }
                | HelTargetTemplate::LocalDocker { .. }
                | HelTargetTemplate::AppleContainer { .. }
                | HelTargetTemplate::SshPodman { .. }
                | HelTargetTemplate::SshDocker { .. }
        )
        .then_some(session)
    }

    pub(crate) fn config_is_empty(&self) -> bool {
        self.config.enabled_profiles().next().is_none() || self.config.targets.is_empty()
    }

    /// Identity for supervised launch checks; cancellation invalidates late replies.
    pub fn session_preflight_generation(&self) -> u64 {
        self.session_preflight_generation
    }

    /// Invalidates an in-flight session preflight while keeping its modal open.
    /// Selection changes use this so a late result cannot describe a different
    /// target or repository bundle.
    pub(crate) fn invalidate_session_preflight(&mut self) {
        self.session_preflight_generation = self.session_preflight_generation.wrapping_add(1);
    }

    pub fn cancel_modal(&mut self) {
        if self.review_settings_discovery_active() {
            self.review_settings_generation = self.review_settings_generation.wrapping_add(1);
        }
        self.session_preflight_generation = self.session_preflight_generation.wrapping_add(1);
        if matches!(self.mode, Mode::WorkspaceManager(_)) {
            self.workspace_management_generation =
                self.workspace_management_generation.wrapping_add(1);
        }
        self.mode = Mode::Dashboard;
        self.rebuild_resume_rows();
    }

    pub(crate) fn focus_len_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.visible_session_indices().len(),
            Focus::Targets => self.capacity_details.len(),
            Focus::Quota => self.config.enabled_profiles().count(),
            Focus::Workspaces | Focus::Prompt => 0,
        }
    }

    /// Moves the focused list's selection to `index`, counted among the rows
    /// currently on screen.
    pub(crate) fn set_selection_for(&mut self, focus: Focus, index: usize) -> bool {
        self.scroll_lookahead.set(None);
        if focus == Focus::Sessions {
            self.set_session_action_focus(None);
        }
        match focus {
            Focus::Sessions => {
                let sessions = self.ordered_sessions();
                let next = self
                    .visible_session_indices()
                    .get(index)
                    .and_then(|session| sessions.get(*session))
                    .map(|session| session.id.clone());
                if self.selected_session_id != next {
                    self.selected_session_id = next;
                    true
                } else {
                    false
                }
            }
            Focus::Targets => {
                if self.capacity_index != index {
                    self.capacity_index = index;
                    true
                } else {
                    false
                }
            }
            Focus::Quota => {
                if self.quota_index != index {
                    self.quota_index = index;
                    true
                } else {
                    false
                }
            }
            Focus::Workspaces | Focus::Prompt => false,
        }
    }

    pub(crate) fn selection_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.selected_visible_index().unwrap_or(0),
            Focus::Targets => self.capacity_index,
            Focus::Quota => self.quota_index,
            Focus::Workspaces | Focus::Prompt => 0,
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.scroll_selection_for(self.focus, delta);
    }

    pub(crate) fn scroll_selection_for(&mut self, focus: Focus, delta: isize) {
        let len = self.focus_len_for(focus);
        if len == 0 {
            self.set_selection_for(focus, 0);
            return;
        }
        let mut index = self.selection_for(focus).min(len.saturating_sub(1));
        let previous = index;
        move_index(&mut index, len, delta);
        self.set_selection_for(focus, index);
        if index != previous {
            self.scroll_lookahead.set(Some((
                focus,
                if delta < 0 {
                    SelectionDirection::Up
                } else {
                    SelectionDirection::Down
                },
            )));
        }
    }

    pub(crate) fn clamp_selections(&mut self) {
        // The selection is anchored by id, so it survives the list changing
        // under it; it only moves when the session it named stopped being on
        // screen.
        let sessions = self.ordered_sessions();
        let visible = self
            .visible_session_indices()
            .into_iter()
            .filter_map(|index| sessions.get(index).map(|session| session.id.clone()))
            .collect::<Vec<_>>();
        if !self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| visible.contains(id))
        {
            self.selected_session_id = visible.into_iter().next();
        }
        let project_keys = self.project_keys();
        self.collapsed_project_keys
            .retain(|key| project_keys.contains(key));
        self.quota_index = self
            .quota_index
            .min(self.config.enabled_profiles().count().saturating_sub(1));
        self.capacity_index = self
            .capacity_index
            .min(self.capacity_details.len().saturating_sub(1));
    }
}
