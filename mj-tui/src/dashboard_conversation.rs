use super::*;

use mj_core::workspace::ConversationLayout;
use ratatui::layout::Direction;

use crate::tile_layout::{NavDirection, PaneId, TileLayout, find_in_direction};

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
        self.conversation_layout.focused()
    }

    /// The session a pane shows, if it is not empty.
    pub fn pane_session(&self, pane: PaneId) -> Option<&str> {
        self.pane_sessions.get(&pane).map(String::as_str)
    }

    /// The pane showing `session_id`. A session is in at most one pane.
    pub fn pane_for_session(&self, session_id: &str) -> Option<PaneId> {
        self.pane_sessions
            .iter()
            .find(|(_, session)| session.as_str() == session_id)
            .map(|(pane, _)| *pane)
    }

    /// Each pane that shows a session, with the session it shows.
    pub fn pane_sessions(&self) -> Vec<(PaneId, String)> {
        self.pane_sessions
            .iter()
            .map(|(pane, session_id)| (*pane, session_id.clone()))
            .collect()
    }

    /// Every session the layout currently shows.
    pub fn pane_session_ids(&self) -> BTreeSet<String> {
        self.pane_sessions.values().cloned().collect()
    }

    /// Focus a pane and move the Sessions selection onto what it shows, so
    /// the highlighted row and the conversation with the keyboard agree.
    pub fn focus_pane(&mut self, pane: PaneId) {
        if self.conversation_layout.focused() == pane {
            return;
        }
        self.conversation_layout.focus_pane(pane);
        if self.conversation_layout.focused() != pane {
            return;
        }
        if let Some(session_id) = self.pane_sessions.get(&pane).cloned() {
            self.select_active_session(&session_id);
        }
        self.mark_layout_modified();
        self.clamp_selections();
    }

    /// Split the focused pane and show `session` in the new leaf, which takes
    /// the focus. `None` means the conversation area is too small to split,
    /// in which case nothing changes.
    pub fn split_focused_pane(
        &mut self,
        direction: Direction,
        session: Option<&str>,
    ) -> Option<PaneId> {
        let area = self.conversation_area();
        let focused = self.conversation_layout.focused();
        let pane = self
            .conversation_layout
            .split_pane(focused, direction, 0.5, area)?;
        if let Some(session_id) = session {
            self.pane_sessions.insert(pane, session_id.to_owned());
        }
        self.conversation_layout.focus_pane(pane);
        if let Some(session_id) = session {
            self.select_active_session(session_id);
        }
        self.mark_layout_modified();
        self.clamp_selections();
        Some(pane)
    }

    /// The split commands, whether they came from a palette or a key.
    ///
    /// With a session selected the split shows it, which is what "Open in
    /// split" means. From a key there may be no selection at all — the
    /// Sessions list can be empty — and the split then makes an empty pane
    /// rather than doing nothing.
    pub(crate) fn split_command(&mut self, direction: Direction) -> DashboardAction {
        if self.selected_session().is_some() {
            return self.open_selected_session_in_split(direction);
        }
        DashboardAction::SplitPane { direction }
    }

    /// Remove `pane` and report the session it showed. The last pane is
    /// emptied rather than removed: the conversation area always has
    /// somewhere to open the next session. Closing a pane that does not hold
    /// the keyboard leaves the focus and its history alone.
    pub fn close_pane(&mut self, pane: PaneId) -> Option<String> {
        let session_id = self.pane_sessions.remove(&pane);
        if self.conversation_layout.pane_count() > 1 {
            self.conversation_layout.close_pane(pane);
        }
        // The Sessions highlight follows the keyboard. After closing the
        // focused pane the keyboard is in a surviving pane, and leaving the
        // highlight on the closed pane's session would pull that conversation
        // into the pane that took the focus.
        if let Some(surviving) = self
            .pane_sessions
            .get(&self.conversation_layout.focused())
            .cloned()
        {
            self.select_active_session(&surviving);
        }
        self.mark_layout_modified();
        self.clamp_selections();
        session_id
    }

    /// Grow or shrink the focused pane toward `nav` by one step.
    pub fn resize_focused_pane(&mut self, nav: NavDirection) {
        let area = self.conversation_area();
        self.conversation_layout
            .resize_focused(nav, RESIZE_STEP, area);
        self.mark_layout_modified();
    }

    /// Move the focus to the nearest pane toward `nav`. Reports whether a
    /// pane was there to move to.
    pub fn focus_pane_toward(&mut self, nav: NavDirection) -> bool {
        let panes = self.conversation_layout.panes(self.conversation_area());
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
        self.conversation_layout
            .to_conversation_layout(&self.pane_sessions)
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
                selected_session_id: None,
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
        let (tree, sessions) = TileLayout::from_conversation_layout(layout);
        self.conversation_layout = tree;
        // A session belongs to one pane. A stored arrangement that names the
        // same session twice keeps the first pane and empties the rest, so the
        // same conversation is never drawn in two places.
        let mut claimed = BTreeSet::new();
        self.pane_sessions = sessions
            .into_iter()
            .filter(|(_, session_id)| self.state.sessions.contains_key(session_id))
            .filter(|(_, session_id)| claimed.insert(session_id.clone()))
            .collect();
        // The highlight follows the keyboard, and the keyboard is in the pane
        // the arrangement named. Without this the clamp below would leave the
        // highlight on the first row, and following that selection would pull
        // its conversation into the restored focus pane.
        if let Some(session_id) = self.pane_sessions.get(&self.conversation_layout.focused()) {
            let session_id = session_id.clone();
            self.select_active_session(&session_id);
        }
    }

    /// Start over with one empty pane, for a workspace with nothing stored.
    pub(crate) fn reset_conversation_layout(&mut self) {
        self.conversation_layout = TileLayout::new().0;
        self.pane_sessions.clear();
    }

    pub(crate) fn mark_layout_modified(&mut self) {
        if let Some(workspace_id) = &self.active_workspace_id {
            self.workspace_layouts_modified.insert(workspace_id.clone());
        }
    }
}
