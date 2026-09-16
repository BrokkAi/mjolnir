use super::*;

impl DashboardState {
    #[must_use]
    pub fn pane_size(&self, pane: SupportPane) -> PaneSize {
        pane_size_for(self.pane_sizes, pane)
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
        let changed = self.pane_sizes != sizes;
        self.pane_sizes = sizes;
        self.clamp_selections();
        if changed {
            self.mark_render_changed();
        }
        Ok(())
    }

    pub(crate) fn pane_maximize_enabled(&self, pane: SupportPane) -> bool {
        match pane {
            SupportPane::Sessions => self.pane_maximize_enabled[0],
            SupportPane::Targets => self.pane_maximize_enabled[1],
            SupportPane::Quota => self.pane_maximize_enabled[2],
        }
    }

    pub(crate) fn set_pane_maximize_enabled(&mut self, enabled: [(SupportPane, bool); 3]) {
        for (pane, enabled) in enabled {
            match pane {
                SupportPane::Sessions => self.pane_maximize_enabled[0] = enabled,
                SupportPane::Targets => self.pane_maximize_enabled[1] = enabled,
                SupportPane::Quota => self.pane_maximize_enabled[2] = enabled,
            }
        }
    }

    /// Selects one pane size. A maximum is exclusive; the previous maximum
    /// becomes Standard while every other explicit choice stays untouched.
    pub fn set_pane_size(&mut self, pane: SupportPane, size: PaneSize) {
        let previous = self.pane_sizes;
        if size == PaneSize::Maximized {
            for other in [
                SupportPane::Sessions,
                SupportPane::Targets,
                SupportPane::Quota,
            ] {
                if other != pane && pane_size_for(self.pane_sizes, other) == PaneSize::Maximized {
                    *pane_size_for_mut(&mut self.pane_sizes, other) = PaneSize::Standard;
                }
            }
        }
        *pane_size_for_mut(&mut self.pane_sizes, pane) = size;
        self.clamp_selections();
        if self.pane_sizes != previous {
            if let Some(workspace_id) = &self.active_workspace_id {
                self.workspace_pane_sizes_modified
                    .insert(workspace_id.clone());
            }
            self.mark_render_changed();
        }
    }

    /// Cycles the focused support pane without moving the keyboard. Prompt is
    /// not resizable, so it explains how to choose a pane instead.
    pub fn cycle_focused_pane_size(&mut self) {
        let Some(pane) = self.focus.support_pane() else {
            self.set_notice("Select Sessions, Targets, or Quota before pressing Alt-Z.");
            return;
        };
        let mut next = self.pane_size(pane).cycled();
        if next == PaneSize::Maximized && !self.pane_maximize_enabled(pane) {
            next = next.cycled();
        }
        self.set_pane_size(pane, next);
    }

    /// Alt-G's stable global preset: restore any custom arrangement to all
    /// Standard; from all Standard, minimize every support pane for the conversation.
    pub fn toggle_pane_preset(&mut self) {
        let previous = self.pane_sizes;
        if self.pane_sizes.all_standard() {
            self.pane_sizes = PaneSizes {
                sessions: PaneSize::Minimized,
                targets: PaneSize::Minimized,
                quota: PaneSize::Minimized,
            };
        } else {
            self.pane_sizes = PaneSizes::default();
        }
        self.clamp_selections();
        if self.pane_sizes != previous {
            self.mark_render_changed();
        }
    }

    #[must_use]
    pub fn sessions_minimized(&self) -> bool {
        self.pane_size(SupportPane::Sessions) == PaneSize::Minimized
    }

    /// Number of pending agent questions across the sessions shown by the
    /// navigator. The minimized navigator uses this as its one compact
    /// aggregate while expanded rows identify the individual sessions.
    pub(crate) fn pending_input_count(&self) -> usize {
        self.ordered_sessions()
            .into_iter()
            .filter_map(|session| self.session_details.get(&session.id))
            .map(|detail| detail.pending_elicitations.len())
            .sum()
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
        !matches!(self.mode, Mode::Dashboard)
    }
}
