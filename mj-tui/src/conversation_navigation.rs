//! One owner for the active conversation and the session in each pane.

use std::collections::BTreeMap;

use mj_core::workspace::ConversationLayout;
use ratatui::layout::{Direction, Rect};

use crate::tile_layout::{NavDirection, PaneId, TileLayout};

/// Identity of a pane assignment, independent of background attachment attempts.
/// Reassigning A to B to A produces a different identity even before I/O runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneAssignment(u64);

pub(crate) struct ConversationNavigation {
    layout: TileLayout,
    sessions: BTreeMap<PaneId, String>,
    assignments: BTreeMap<PaneId, PaneAssignment>,
    next_assignment: u64,
}

impl Default for ConversationNavigation {
    fn default() -> Self {
        Self {
            layout: TileLayout::new().0,
            sessions: BTreeMap::new(),
            assignments: BTreeMap::new(),
            next_assignment: 0,
        }
    }
}

impl ConversationNavigation {
    pub(crate) fn layout(&self) -> &TileLayout {
        &self.layout
    }

    pub(crate) fn sessions(&self) -> &BTreeMap<PaneId, String> {
        &self.sessions
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.sessions
            .get(&self.layout.focused())
            .map(String::as_str)
    }

    pub(crate) fn assignment(&self, pane: PaneId) -> Option<PaneAssignment> {
        self.assignments.get(&pane).copied()
    }

    pub(crate) fn assign(&mut self, pane: PaneId, session: Option<&str>) -> bool {
        if !self.layout.pane_ids().contains(&pane)
            || self.sessions.get(&pane).map(String::as_str) == session
        {
            return false;
        }
        if let Some(session) = session {
            self.sessions.retain(|_, shown| shown != session);
            self.assignments
                .retain(|id, _| self.sessions.contains_key(id));
            self.next_assignment = self
                .next_assignment
                .checked_add(1)
                .expect("assignment identity exhausted");
            self.sessions.insert(pane, session.to_owned());
            self.assignments
                .insert(pane, PaneAssignment(self.next_assignment));
        } else {
            self.sessions.remove(&pane);
            self.assignments.remove(&pane);
        }
        true
    }

    pub(crate) fn retain_sessions(&mut self, mut keep: impl FnMut(&str) -> bool) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|_, session| keep(session));
        self.assignments
            .retain(|pane, _| self.sessions.contains_key(pane));
        self.sessions.len() != before
    }

    pub(crate) fn focus_pane(&mut self, pane: PaneId) {
        self.layout.focus_pane(pane);
    }

    pub(crate) fn split_pane(
        &mut self,
        pane: PaneId,
        direction: Direction,
        ratio: f32,
        area: Rect,
    ) -> Option<PaneId> {
        self.layout.split_pane(pane, direction, ratio, area)
    }

    pub(crate) fn close_pane(&mut self, pane: PaneId) -> Option<String> {
        if !self.layout.close_pane(pane) {
            return None;
        }
        self.assignments.remove(&pane);
        self.sessions.remove(&pane)
    }

    pub(crate) fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        self.layout.swap_panes(first, second)
    }

    pub(crate) fn resize_focused(&mut self, nav: NavDirection, delta: f32, area: Rect) {
        self.layout.resize_focused(nav, delta, area);
    }

    pub(crate) fn restore(
        &mut self,
        saved: &ConversationLayout,
        mut keep: impl FnMut(&str) -> bool,
    ) {
        let (layout, sessions) = TileLayout::from_conversation_layout(saved);
        self.layout = layout;
        self.sessions.clear();
        self.assignments.clear();
        for (pane, session) in sessions {
            if keep(&session) && !self.sessions.values().any(|shown| shown == &session) {
                self.assign(pane, Some(&session));
            }
        }
    }

    pub(crate) fn reset(&mut self) {
        self.layout = TileLayout::new().0;
        self.sessions.clear();
        self.assignments.clear();
    }
}
