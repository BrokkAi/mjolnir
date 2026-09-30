use super::*;

use mj_core::workspace::ConversationLayout;
use ratatui::layout::Direction;

use crate::tile_layout::{NavDirection, PaneId, find_in_direction};

/// The notice a refused split leaves on the bar. A later split that succeeds
/// clears it, because it no longer describes the last split attempt.
pub const SPLIT_REFUSED_NOTICE: &str = "Not enough room to split this pane.";

/// How much of a split a single resize step moves.
const RESIZE_STEP: f32 = 0.05;

/// The conversation band used before the first frame has measured one. It is
/// only a starting point for `can_split`, which the next frame recomputes.
const UNMEASURED_CONVERSATION_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 120,
    height: 30,
};

impl DashboardState {
    /// The pane the keyboard belongs to.
    pub fn focused_pane(&self) -> PaneId {
        self.navigation.layout().focused()
    }

    /// The session a pane shows, if it is not empty.
    pub fn pane_session(&self, pane: PaneId) -> Option<&str> {
        self.navigation.sessions().get(&pane).map(String::as_str)
    }

    /// Identity captured by background work for the session assigned to a pane.
    pub fn pane_assignment(&self, pane: PaneId) -> Option<PaneAssignment> {
        self.navigation.assignment(pane)
    }

    /// The pane showing `session_id`. A session is in at most one pane.
    pub fn pane_for_session(&self, session_id: &str) -> Option<PaneId> {
        self.navigation
            .sessions()
            .iter()
            .find(|(_, session)| session.as_str() == session_id)
            .map(|(pane, _)| *pane)
    }

    /// Each pane that shows a session, with the session it shows.
    pub fn pane_sessions(&self) -> Vec<(PaneId, String)> {
        self.navigation
            .sessions()
            .iter()
            .map(|(pane, session_id)| (*pane, session_id.clone()))
            .collect()
    }

    /// Every session the layout currently shows.
    pub fn pane_session_ids(&self) -> BTreeSet<String> {
        self.navigation.sessions().values().cloned().collect()
    }

    /// Activate a conversation; the Sessions marker derives from this pane.
    pub fn focus_pane(&mut self, pane: PaneId) {
        if self.navigation.layout().focused() == pane {
            return;
        }
        self.navigation.focus_pane(pane);
        if self.navigation.layout().focused() != pane {
            return;
        }
        self.mark_layout_modified();
        self.clamp_selections();
    }

    /// The panes to draw and hit-test this frame.
    ///
    /// Zoomed, that is the focused pane alone filling the whole band: the
    /// others keep their place in the arrangement but are not on screen, so
    /// nothing may be drawn for them or pointed at in them.
    pub fn conversation_panes(&self, area: Rect) -> Vec<crate::tile_layout::PaneInfo> {
        if self.conversation_zoomed && self.navigation.layout().pane_count() > 1 {
            return vec![crate::tile_layout::PaneInfo {
                id: self.navigation.layout().focused(),
                rect: area,
                is_focused: true,
            }];
        }
        self.navigation.layout().panes(area)
    }

    /// Whether the focused pane is filling the band on its own.
    pub fn conversation_zoomed(&self) -> bool {
        self.conversation_zoomed && self.navigation.layout().pane_count() > 1
    }

    /// Toggle the zoom on the focused pane. With nothing to hide there is
    /// nothing to zoom, and the surface says so rather than changing how it
    /// already looks.
    pub(crate) fn zoom_pane_command(&mut self) -> DashboardAction {
        if self.navigation.layout().pane_count() < 2 {
            self.set_notice("Only one pane; nothing to zoom");
            return DashboardAction::None;
        }
        self.conversation_zoomed = !self.conversation_zoomed;
        DashboardAction::ConversationPanesChanged { focus_moved: false }
    }

    /// Split the focused pane and show `session` in the new leaf, which takes
    /// the focus. `None` means the conversation area is too small to split,
    /// in which case the layout is unchanged and [`SPLIT_REFUSED_NOTICE`]
    /// is shown.
    pub fn split_focused_pane(
        &mut self,
        direction: Direction,
        session: Option<&str>,
    ) -> Option<PaneId> {
        let pane = self.split_conversation_pane(self.focused_pane(), direction)?;
        if let Some(session) = session {
            self.set_pane_session(pane, Some(session));
        }
        Some(pane)
    }

    pub fn has_conversation_pane(&self, pane: PaneId) -> bool {
        self.navigation.layout().pane_ids().contains(&pane)
    }

    pub fn split_conversation_pane(
        &mut self,
        target: PaneId,
        direction: Direction,
    ) -> Option<PaneId> {
        let Some(pane) =
            self.navigation
                .split_pane(target, direction, 0.5, self.conversation_area())
        else {
            self.set_notice(SPLIT_REFUSED_NOTICE);
            return None;
        };
        if self.notice().as_deref() == Some(SPLIT_REFUSED_NOTICE) {
            self.clear_notice();
        }
        self.conversation_zoomed = false;
        self.browse_pane = Some(pane);
        self.navigation.focus_pane(pane);
        self.reconcile_pins();
        self.focus_sessions();
        self.mark_layout_modified();
        Some(pane)
    }

    pub(crate) fn split_command(&mut self, direction: Direction) -> DashboardAction {
        DashboardAction::SplitConversation {
            pane: self.focused_pane(),
            direction,
        }
    }

    /// Remove a pinned or empty pane and report the session it showed.
    /// Browse remains available for list navigation.
    pub fn close_pane(&mut self, pane: PaneId) -> Option<String> {
        if pane == self.browse_pane() {
            self.set_notice("Browse must remain; move it with Swap pane.");
            return None;
        }
        self.conversation_zoomed = false;
        let session = self.navigation.close_pane(pane);
        self.reconcile_pins();
        self.mark_layout_modified();
        self.clamp_selections();
        session
    }

    pub fn browse_pane(&self) -> PaneId {
        self.browse_pane
            .unwrap_or_else(|| self.navigation.layout().pane_ids()[0])
    }

    pub fn pin_id(&self, session: &str) -> Option<u32> {
        self.pin_ids.get(session).copied()
    }

    pub(crate) fn reconcile_pins(&mut self) {
        let browse = self.browse_pane();
        self.pin_ids.retain(|session, _| {
            self.navigation
                .sessions()
                .iter()
                .any(|(pane, shown)| *pane != browse && shown == session)
        });
        for (pane, session) in self.navigation.sessions() {
            if *pane == browse || self.pin_ids.contains_key(session) {
                continue;
            }
            let badge = (0..)
                .find(|id| !self.pin_ids.values().any(|used| used == id))
                .expect("available pin identity");
            self.pin_ids.insert(session.clone(), badge);
        }
    }

    pub fn take_navigation_session(&mut self) -> Option<String> {
        self.navigation_session.take()
    }

    pub fn reveal_pane(&mut self, pane: PaneId) {
        if pane != self.focused_pane() {
            self.conversation_zoomed = false;
        }
    }

    pub fn swap_conversation_panes(&mut self, source: PaneId, target: PaneId) {
        if self.navigation.swap_panes(source, target) {
            self.conversation_zoomed = false;
            self.mark_layout_modified();
        }
    }

    /// Whether resize mode currently owns ordinary keyboard input.
    pub fn resize_mode_active(&self) -> bool {
        self.resize_mode && !self.modal_open()
    }

    pub(crate) fn begin_resize_mode(&mut self) -> DashboardAction {
        if self.navigation.layout().pane_count() < 2 {
            self.set_notice("Only one pane; nothing to resize");
            return DashboardAction::None;
        }
        self.conversation_zoomed = false;
        self.resize_mode = true;
        DashboardAction::ConversationPanesChanged { focus_moved: false }
    }

    pub(crate) fn swap_pane_command(&mut self, nav: NavDirection) -> DashboardAction {
        let panes = self.navigation.layout().panes(self.conversation_area());
        let Some(focused) = panes.iter().find(|pane| pane.is_focused) else {
            return DashboardAction::None;
        };
        let Some(target) = find_in_direction(focused, nav, &panes) else {
            return DashboardAction::None;
        };
        self.conversation_zoomed = false;
        self.navigation.swap_panes(focused.id, target);
        self.mark_layout_modified();
        DashboardAction::ConversationPanesChanged { focus_moved: false }
    }

    /// Grow or shrink the focused pane toward `nav` by one step.
    pub fn resize_focused_pane(&mut self, nav: NavDirection) {
        let area = self.conversation_area();
        self.navigation.resize_focused(nav, RESIZE_STEP, area);
        self.mark_layout_modified();
    }

    /// Move the focus to the nearest pane toward `nav`. Reports whether a
    /// pane was there to move to.
    pub fn focus_pane_toward(&mut self, nav: NavDirection) -> bool {
        let panes = self.navigation.layout().panes(self.conversation_area());
        let Some(focused) = panes.iter().find(|pane| pane.is_focused) else {
            return false;
        };
        let Some(target) = find_in_direction(focused, nav, &panes) else {
            return false;
        };
        self.focus_pane(target);
        true
    }

    /// Move the focus toward `nav`, reporting the work the controller owes
    /// the change: saving the arrangement and re-reading which conversation
    /// the keyboard is now in. Nothing to move to changes nothing.
    pub(crate) fn focus_pane_command(&mut self, nav: NavDirection) -> DashboardAction {
        if self.focus_pane_toward(nav) {
            DashboardAction::ConversationPanesChanged { focus_moved: true }
        } else {
            DashboardAction::None
        }
    }

    /// Move the keyboard back to the pane it was in before this one.
    ///
    /// There is nothing to go back to before the first focus move, and
    /// closing a pane forgets it, so the command says so rather than moving
    /// the keyboard somewhere the user did not ask for.
    pub(crate) fn focus_last_pane_command(&mut self) -> DashboardAction {
        let previous = self
            .navigation
            .layout()
            .previous_focus()
            .filter(|pane| *pane != self.navigation.layout().focused())
            .filter(|pane| self.navigation.layout().pane_ids().contains(pane));
        let Some(previous) = previous else {
            self.set_notice("No previous pane");
            return DashboardAction::None;
        };
        self.focus_pane(previous);
        DashboardAction::ConversationPanesChanged { focus_moved: true }
    }

    /// Move the focused pane's border toward `nav` by one step.
    pub(crate) fn resize_pane_command(&mut self, nav: NavDirection) -> DashboardAction {
        self.resize_focused_pane(nav);
        DashboardAction::ConversationPanesChanged { focus_moved: false }
    }

    /// The band the panes are laid out in. Before the first frame, and while
    /// the surface is too small to draw a conversation, the layout still has
    /// to answer questions about pane sizes, so a nominal band stands in.
    pub(crate) fn conversation_area(&self) -> Rect {
        if let Some(area) = self.conversation_area {
            return area;
        }
        match self.pane_areas {
            Some([sessions, targets, _]) => Rect::new(
                sessions.right(),
                sessions.y,
                targets.right().saturating_sub(sessions.right()),
                sessions.height,
            ),
            None => UNMEASURED_CONVERSATION_AREA,
        }
    }

    /// The live arrangement in the form the workspace store keeps.
    pub fn export_conversation_layout(&self) -> ConversationLayout {
        let mut layout = self
            .navigation
            .layout()
            .to_conversation_layout(self.navigation.sessions());
        layout.browse = Some(self.browse_pane().raw());
        layout.pins = self.pin_ids.clone();
        layout
    }

    /// The arrangement saved for one workspace: the live tree when it is the
    /// active workspace, and the cached copy otherwise.
    pub fn conversation_layout_for(&self, workspace_id: &str) -> ConversationLayout {
        if self.active_workspace_id.as_deref() == Some(workspace_id) {
            return self.export_conversation_layout();
        }
        self.workspace_views
            .get(workspace_id)
            .map(|view| view.conversation_layout.clone())
            .unwrap_or_default()
    }

    /// Applies a controller-provided layout unless this client has already
    /// changed that workspace's arrangement.
    pub fn cache_workspace_layout(&mut self, workspace_id: &str, layout: ConversationLayout) {
        if layout.validate().is_err() || self.workspace_layouts_modified.contains(workspace_id) {
            return;
        }
        if self.active_workspace_id.as_deref() == Some(workspace_id) {
            self.restore_conversation_layout(&layout);
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
                pane_sizes: PaneSizes::default(),
                conversation_layout: ConversationLayout::default(),
                collapsed_project_keys: BTreeSet::new(),
                focus: Focus::Sessions,
            })
            .conversation_layout = layout;
    }

    /// Whether the local client has changed this workspace's arrangement.
    pub fn workspace_layout_modified(&self, workspace_id: &str) -> bool {
        self.workspace_layouts_modified.contains(workspace_id)
    }

    /// Adopt a stored arrangement. A pane whose recorded session is no longer
    /// one this surface knows about becomes an empty pane rather than a
    /// failure: sessions are closed and removed outside this client.
    pub(crate) fn restore_conversation_layout(&mut self, layout: &ConversationLayout) {
        self.navigation.restore(layout, |id| {
            session_belongs_in_layout(
                &self.state,
                &self.native_agents,
                self.active_workspace_id.as_deref(),
                id,
            )
        });
        self.browse_pane = Some(PaneId::from_raw(layout.browse.unwrap_or(layout.focus)));
        self.pin_ids = layout.pins.clone();
        self.conversation_zoomed = false;
        self.reconcile_pins();
    }

    /// Start over with one empty pane, for a workspace with nothing stored.
    pub(crate) fn reset_conversation_layout(&mut self) {
        self.navigation.reset();
        self.browse_pane = None;
        self.pin_ids.clear();
        self.pane_menu = None;
        self.conversation_zoomed = false;
    }

    pub(crate) fn mark_layout_modified(&mut self) {
        if let Some(workspace_id) = &self.active_workspace_id {
            self.workspace_layouts_modified.insert(workspace_id.clone());
        }
    }
}

/// Whether a conversation pane of the active workspace may keep showing this
/// session. The session record owns which workspace a session is in; a pane,
/// and the selection that follows it, leaves with the record when the record
/// moves to another workspace. Native agent views have no record of their
/// own and stay with the pane that opened them.
pub(crate) fn session_belongs_in_layout<V>(
    state: &State,
    native_agents: &BTreeMap<String, V>,
    active_workspace_id: Option<&str>,
    session_id: &str,
) -> bool {
    let Some(record) = state.sessions.get(session_id) else {
        return false;
    };
    native_agents.contains_key(session_id)
        || active_workspace_id.is_none_or(|active| record.workspace_id == active)
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use crate::test_support::{dashboard_with_session, key, running_session};

    fn dashboard() -> DashboardState {
        let mut dashboard = dashboard_with_session(running_session());
        for i in 2..=6 {
            let mut session = running_session();
            session.id = format!("session-{i}");
            dashboard.state.sessions.insert(session.id.clone(), session);
        }
        dashboard.conversation_area = Some(Rect::new(0, 0, 160, 60));
        dashboard.set_current_session(Some("session-1"));
        dashboard
    }

    #[test]
    fn selection_changes_panes_synchronously_without_entering_the_composer() {
        let mut d = dashboard();
        let pin = d.focused_pane();
        let browse = d.split_focused_pane(Direction::Horizontal, None).unwrap();
        assert_eq!(d.selected_session_id(), None);
        d.handle_key(key(KeyCode::Down));
        assert_eq!(d.selected_session_id(), Some("session-1"));
        assert_eq!(d.focused_pane(), pin);
        assert_eq!(d.focus(), Focus::Sessions);
        d.handle_key(key(KeyCode::Down));
        assert_eq!(d.focused_pane(), browse);
        assert_eq!(d.selected_session_id(), Some("session-2"));
        assert_eq!(d.current_session_id(), Some("session-2"));
        assert_eq!(d.command_session_id(), Some("session-2"));
        assert_eq!(d.focus(), Focus::Sessions);
        assert_eq!(d.pane_session(pin), Some("session-1"));
        d.focus_pane(pin);
        d.focus_prompt();
        assert_eq!(d.selected_session_id(), Some("session-1"));
        assert_eq!(d.command_session_id(), Some("session-1"));
        d.focus_sessions();
        assert_eq!(d.command_session_id(), Some("session-1"));
    }

    #[test]
    fn an_empty_pane_selects_the_last_row_on_up_and_cannot_command_another_pane() {
        let mut d = dashboard();
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        assert_eq!(d.selected_session_id(), None);
        assert_eq!(d.command_session_id(), None);
        d.handle_key(key(KeyCode::Up));
        assert_eq!(d.selected_session_id(), Some("session-6"));
        assert_eq!(d.current_session_id(), Some("session-6"));
    }

    #[test]
    fn assignment_identity_survives_focus_but_not_reassignment_or_restore() {
        let mut d = dashboard();
        let pane = d.focused_pane();
        let first = d.pane_assignment(pane).unwrap();
        let other = d
            .split_focused_pane(Direction::Horizontal, Some("session-2"))
            .unwrap();
        d.focus_pane(pane);
        assert_eq!(d.pane_assignment(pane), Some(first));
        d.set_pane_session(pane, Some("session-3"));
        d.set_pane_session(pane, Some("session-1"));
        assert_ne!(d.pane_assignment(pane), Some(first));
        let before_restore = d.pane_assignment(pane);
        let saved = d.export_conversation_layout();
        d.restore_conversation_layout(&saved);
        assert_ne!(d.pane_assignment(pane), before_restore);
        assert_eq!(d.selected_session_id(), Some("session-1"));
        d.close_pane(pane);
        assert_eq!(d.focused_pane(), other);
        assert_eq!(d.selected_session_id(), Some("session-2"));
        assert_eq!(d.pane_assignment(pane), None);
    }

    #[test]
    fn filters_keep_the_active_row_until_navigation_selects_a_match() {
        let mut d = dashboard();
        d.focus_sessions();
        d.handle_key(key(KeyCode::Char('/')));
        for ch in "session-2".chars() {
            d.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(d.selected_session_id(), Some("session-1"));
        assert_eq!(d.current_session_id(), Some("session-1"));
        assert_eq!(
            d.ordered_sessions()
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["session-1", "session-2"]
        );
        let rows = crate::test_support::drawn(&mut d, 140, 40);
        assert!(
            rows.iter().any(|row| row.contains("Outside filter")),
            "{rows:?}"
        );
        d.handle_key(key(KeyCode::Enter));
        d.handle_key(key(KeyCode::Down));
        assert_eq!(d.selected_session_id(), Some("session-2"));
        assert_eq!(d.current_session_id(), Some("session-2"));
        assert_eq!(d.ordered_sessions().len(), 1);
        assert!(
            !crate::test_support::drawn(&mut d, 140, 40)
                .iter()
                .any(|row| row.contains("Outside filter"))
        );
    }

    #[test]
    fn filter_counts_exclude_an_active_row_from_another_list_scope() {
        let mut d = dashboard();
        let mut historical = running_session();
        historical.id = "history".into();
        historical.state = SessionState::Stopped;
        d.state.sessions.insert(historical.id.clone(), historical);
        d.select_active_session("history");
        d.focus_sessions();
        d.handle_key(key(KeyCode::Char('/')));
        d.handle_key(key(KeyCode::Char('z')));
        assert_eq!(d.ordered_sessions().len(), 1);
        assert_eq!(d.selected_session_id(), Some("history"));
        assert_eq!(d.sessions_hidden_count(), 6);
    }

    #[test]
    fn splitting_builds_a_grid_with_one_browse_and_stable_pins() {
        let mut d = dashboard();
        let left = d.browse_pane();
        let right = d.split_focused_pane(Direction::Horizontal, None).unwrap();
        assert_eq!(d.pin_id("session-1"), Some(0));
        d.set_pane_session(right, Some("session-2"));
        d.focus_pane(left);
        let lower_left = d.split_focused_pane(Direction::Vertical, None).unwrap();
        assert_eq!(d.pin_id("session-2"), Some(1));
        d.set_pane_session(lower_left, Some("session-3"));
        d.focus_pane(right);
        let lower_right = d.split_focused_pane(Direction::Vertical, None).unwrap();
        assert_eq!(d.browse_pane(), lower_right);
        assert_eq!(d.pin_id("session-3"), Some(2));
        assert_eq!(d.pane_session(lower_right), None);
        let pins = d.pin_ids.clone();
        d.swap_conversation_panes(left, lower_right);
        assert_eq!(d.browse_pane(), lower_right);
        assert_eq!(d.pin_ids, pins);
        d.select_active_session("session-4");
        assert_eq!(d.selected_session_id(), Some("session-4"));
        d.focus_pane(right);
        assert_eq!(d.selected_session_id(), Some("session-2"));
        assert_eq!(d.pane_session(lower_right), Some("session-4"));
        assert_eq!(d.pane_session(left), Some("session-1"));
        assert_eq!(d.pane_session(right), Some("session-2"));
        assert_eq!(d.pane_session(lower_left), Some("session-3"));
        assert!(d.export_conversation_layout().validate().is_ok());
    }

    #[test]
    fn a_refused_split_preserves_focus_zoom_and_roles() {
        let mut d = dashboard();
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.conversation_zoomed = true;
        d.conversation_area = Some(Rect::new(0, 0, 40, 8));
        let before = d.export_conversation_layout();
        assert!(d.split_focused_pane(Direction::Horizontal, None).is_none());
        assert_eq!(d.export_conversation_layout(), before);
        assert!(d.conversation_zoomed);
    }

    #[test]
    fn restore_keeps_browse_separate_from_focus_and_legacy_layouts_convert() {
        let mut d = dashboard();
        let pin = d.browse_pane();
        let browse = d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.set_pane_session(browse, Some("session-2"));
        d.focus_pane(pin);
        let saved = d.export_conversation_layout();
        d.restore_conversation_layout(&saved);
        assert_eq!(d.focused_pane(), pin);
        assert_eq!(d.browse_pane(), browse);
        assert_eq!(d.selected_session_id(), Some("session-1"));
        assert_eq!(d.pin_id("session-1"), Some(0));
        let mut legacy = saved;
        legacy.browse = None;
        legacy.pins.clear();
        d.restore_conversation_layout(&legacy);
        assert_eq!(d.browse_pane(), pin);
        assert_eq!(d.pin_id("session-2"), Some(0));
    }

    #[test]
    fn refresh_removal_clears_selection_and_missing_pins_leave_slots() {
        let mut d = dashboard();
        let pin = d.browse_pane();
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.select_active_session("session-2");
        let mut state = d.state.clone();
        state.sessions.remove("session-1");
        state.sessions.remove("session-2");
        d.set_state(state);
        assert_eq!(d.selected_session_id(), None);
        assert_eq!(d.pane_session(pin), None);
        assert_eq!(d.navigation.layout().pane_count(), 2);
        assert!(d.pin_ids.is_empty());
        d.focus_sessions();
        d.handle_key(key(KeyCode::Down));
        assert_eq!(d.selected_session_id(), d.current_session_id());
    }

    #[test]
    fn clearing_a_pin_keeps_the_slot_and_does_not_renumber_other_pins() {
        let mut d = dashboard();
        let first = d.browse_pane();
        let second = d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.set_pane_session(second, Some("session-2"));
        d.split_focused_pane(Direction::Vertical, None).unwrap();
        let before = d.navigation.layout().pane_count();
        d.set_pane_session(first, None);
        assert_eq!(d.navigation.layout().pane_count(), before);
        assert_eq!(d.pin_id("session-2"), Some(1));
        assert_eq!(d.pin_id("session-1"), None);
        let browse = d.browse_pane();
        assert_eq!(d.close_pane(browse), None);
        assert_eq!(d.navigation.layout().pane_count(), before);
        d.close_pane(first);
        assert_eq!(d.navigation.layout().pane_count(), before - 1);
    }
}
