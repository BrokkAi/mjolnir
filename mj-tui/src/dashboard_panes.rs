use super::*;

impl DashboardState {
    fn pane_size_mut(&mut self, pane: SupportPane) -> &mut PaneSize {
        match pane {
            SupportPane::Sessions => &mut self.pane_sizes.sessions,
            SupportPane::Targets => &mut self.pane_sizes.targets,
            SupportPane::Quota => &mut self.pane_sizes.quota,
            SupportPane::Notifications => &mut self.notifications.size,
        }
    }

    /// Every size transition resolves the exclusive maximum here. A restored
    /// persisted maximum takes precedence over the process-local preference.
    fn reconcile_pane_sizes(&mut self, preferred: Option<SupportPane>) {
        let panes = [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
            SupportPane::Notifications,
        ];
        let maximum = preferred
            .filter(|pane| self.pane_size(*pane) == PaneSize::Maximized)
            .or_else(|| {
                panes
                    .into_iter()
                    .find(|pane| self.pane_size(*pane) == PaneSize::Maximized)
            });
        for pane in panes {
            if Some(pane) != maximum && self.pane_size(pane) == PaneSize::Maximized {
                *self.pane_size_mut(pane) = PaneSize::Standard;
            }
        }
    }

    #[must_use]
    pub fn pane_size(&self, pane: SupportPane) -> PaneSize {
        match pane {
            SupportPane::Sessions => self.pane_sizes.sessions,
            SupportPane::Targets => self.pane_sizes.targets,
            SupportPane::Quota => self.pane_sizes.quota,
            SupportPane::Notifications => self.notifications.size,
        }
    }

    /// Capture the current dashboard arrangement for workspace persistence.
    #[must_use]
    pub fn pane_sizes(&self) -> PaneSizes {
        self.pane_sizes
    }

    /// Restore a persisted dashboard arrangement and clamp list selections to
    /// the current data. Restoring is independent of whether this frame has
    /// enough room to grow a pane to its maximum size.
    pub fn restore_pane_sizes(&mut self, sizes: PaneSizes) -> anyhow::Result<()> {
        sizes.validate()?;
        self.pane_sizes = sizes;
        self.reconcile_pane_sizes(None);
        self.clamp_selections();
        Ok(())
    }

    /// Workspace layouts own Targets and Profiles; Sessions belongs to the
    /// dashboard. Its global maximum takes precedence over a saved maximum.
    pub(crate) fn restore_workspace_pane_sizes(&mut self, sizes: PaneSizes) {
        let sessions = self.pane_sizes.sessions;
        self.pane_sizes = PaneSizes { sessions, ..sizes };
        self.reconcile_pane_sizes(None);
    }

    pub(crate) fn pane_maximize_enabled(&self, pane: SupportPane) -> bool {
        match pane {
            SupportPane::Sessions => self.pane_maximize_enabled[0],
            SupportPane::Targets => self.pane_maximize_enabled[1],
            SupportPane::Quota => self.pane_maximize_enabled[2],
            SupportPane::Notifications => self.notifications.maximize_enabled,
        }
    }

    pub(crate) fn set_pane_maximize_enabled(&mut self, enabled: [(SupportPane, bool); 4]) {
        for (pane, enabled) in enabled {
            match pane {
                SupportPane::Sessions => self.pane_maximize_enabled[0] = enabled,
                SupportPane::Targets => self.pane_maximize_enabled[1] = enabled,
                SupportPane::Quota => self.pane_maximize_enabled[2] = enabled,
                SupportPane::Notifications => self.notifications.maximize_enabled = enabled,
            }
        }
    }

    /// Selects one pane size. A maximum is exclusive; the previous maximum
    /// becomes Standard while every other explicit choice stays untouched.
    pub fn set_pane_size(&mut self, pane: SupportPane, size: PaneSize) {
        let previous = self.pane_sizes;
        *self.pane_size_mut(pane) = size;
        self.reconcile_pane_sizes(Some(pane));
        self.clamp_selections();
        if self.pane_sizes != previous
            && let Some(workspace_id) = self.active_workspace_id.as_ref()
        {
            self.workspace_pane_sizes_modified
                .insert(workspace_id.clone());
        }
    }

    /// Cycles the focused support pane without moving the keyboard. Prompt is
    /// not resizable, so it explains how to choose a pane instead.
    pub fn cycle_focused_pane_size(&mut self) {
        let Some(pane) = self.focus.support_pane() else {
            self.set_notice("Select Sessions, Targets, Profiles, or Notifications before cycling the pane size.");
            return;
        };
        let mut next = self.pane_size(pane).cycled();
        if next == PaneSize::Maximized && !self.pane_maximize_enabled(pane) {
            next = next.cycled();
        }
        self.set_pane_size(pane, next);
    }

    /// The pane preset's stable global arrangement: restore any custom layout to all
    /// Standard; from all Standard, minimize every support pane for the conversation.
    pub fn toggle_pane_preset(&mut self) {
        if self.pane_sizes.all_standard() && self.notifications.size == PaneSize::Standard {
            self.notifications.size = PaneSize::Minimized;
            self.pane_sizes = PaneSizes {
                sessions: PaneSize::Minimized,
                targets: PaneSize::Minimized,
                quota: PaneSize::Minimized,
            };
        } else {
            self.notifications.size = PaneSize::Standard;
            self.pane_sizes = PaneSizes::default();
        }
        self.clamp_selections();
    }

    #[must_use]
    pub fn sessions_minimized(&self) -> bool {
        self.narrow_layout.get() || self.pane_size(SupportPane::Sessions) == PaneSize::Minimized
    }

    /// The pending questions from the latest accepted full projection. A
    /// startup summary intentionally returns `None`, because it does not
    /// carry the complete request list and must not invalidate a local draft.
    pub fn pending_elicitations(
        &self,
        session_id: &str,
    ) -> Option<(u64, &[mj_core::elicitation::ElicitationRequest])> {
        let detail = self.session_details.get(session_id)?;
        detail
            .pending_elicitations_applied_event_ordinal
            .map(|ordinal| (ordinal, detail.pending_elicitations.as_slice()))
    }

    pub(crate) fn focused_rows_visible(&self) -> bool {
        self.focus.support_pane().is_none_or(|pane| {
            pane == SupportPane::Sessions || self.pane_size(pane) != PaneSize::Minimized
        })
    }

    /// Whether a modal dialog or wizard owns the keyboard.
    pub fn modal_open(&self) -> bool {
        self.away_dialog().is_some()
            || self.pane_menu.is_some()
            || !matches!(self.mode, Mode::Dashboard)
    }
}
