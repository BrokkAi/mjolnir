// Port of mj-tui/src/dashboard_panes.rs.

package tui

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (d *DashboardState) paneSizeMut(pane SupportPane) *core.PaneSize {
	switch pane {
	case SupportPaneSessions:
		return &d.paneSizes.Sessions
	case SupportPaneTargets:
		return &d.paneSizes.Targets
	case SupportPaneQuota:
		return &d.paneSizes.Quota
	case SupportPaneNotifications:
		return &d.notifications.size
	}
	panic(fmt.Sprintf("unknown support pane %v", pane))
}

// reconcilePaneSizes resolves the exclusive maximum for every size
// transition. A restored persisted maximum takes precedence over the
// process-local preference.
func (d *DashboardState) reconcilePaneSizes(preferred *SupportPane) {
	panes := [...]SupportPane{SupportPaneSessions, SupportPaneTargets, SupportPaneQuota, SupportPaneNotifications}
	var maximum *SupportPane
	if preferred != nil && d.PaneSize(*preferred) == core.PaneSizeMaximized {
		maximum = preferred
	} else {
		for index := range panes {
			if d.PaneSize(panes[index]) == core.PaneSizeMaximized {
				maximum = &panes[index]
				break
			}
		}
	}
	for _, pane := range panes {
		if (maximum == nil || *maximum != pane) && d.PaneSize(pane) == core.PaneSizeMaximized {
			*d.paneSizeMut(pane) = core.PaneSizeStandard
		}
	}
}

// PaneSize is the size one support pane currently asks for.
func (d *DashboardState) PaneSize(pane SupportPane) core.PaneSize {
	switch pane {
	case SupportPaneSessions:
		return d.paneSizes.Sessions
	case SupportPaneTargets:
		return d.paneSizes.Targets
	case SupportPaneQuota:
		return d.paneSizes.Quota
	case SupportPaneNotifications:
		return d.notifications.size
	}
	panic(fmt.Sprintf("unknown support pane %v", pane))
}

// PaneSizes captures the current dashboard arrangement for workspace
// persistence.
func (d *DashboardState) PaneSizes() core.PaneSizes {
	return d.paneSizes
}

// RestorePaneSizes restores a persisted dashboard arrangement and clamps
// list selections to the current data. Restoring is independent of whether
// this frame has enough room to grow a pane to its maximum size.
func (d *DashboardState) RestorePaneSizes(sizes core.PaneSizes) error {
	if err := sizes.Validate(); err != nil {
		return err
	}
	d.paneSizes = sizes
	d.reconcilePaneSizes(nil)
	d.clampSelections()
	return nil
}

// restoreWorkspacePaneSizes: workspace layouts own Targets and Profiles;
// Sessions belongs to the dashboard. Its global maximum takes precedence
// over a saved maximum.
func (d *DashboardState) restoreWorkspacePaneSizes(sizes core.PaneSizes) {
	sizes.Sessions = d.paneSizes.Sessions
	d.paneSizes = sizes
	d.reconcilePaneSizes(nil)
}

// paneMaximizeEnabled is Rust's DashboardState::pane_maximize_enabled. lib.rs's
// field of the same name is paneMaximizeEnabledField in Go.
func (d *DashboardState) paneMaximizeEnabled(pane SupportPane) bool {
	switch pane {
	case SupportPaneSessions:
		return d.paneMaximizeEnabledField[0]
	case SupportPaneTargets:
		return d.paneMaximizeEnabledField[1]
	case SupportPaneQuota:
		return d.paneMaximizeEnabledField[2]
	case SupportPaneNotifications:
		return d.notifications.maximizeEnabled
	}
	panic(fmt.Sprintf("unknown support pane %v", pane))
}

func (d *DashboardState) setPaneMaximizeEnabled(enabled [4]supportPaneEnabled) {
	for _, setting := range enabled {
		switch setting.pane {
		case SupportPaneSessions:
			d.paneMaximizeEnabledField[0] = setting.enabled
		case SupportPaneTargets:
			d.paneMaximizeEnabledField[1] = setting.enabled
		case SupportPaneQuota:
			d.paneMaximizeEnabledField[2] = setting.enabled
		case SupportPaneNotifications:
			d.notifications.maximizeEnabled = setting.enabled
		}
	}
}

// SetPaneSize selects one pane size. A maximum is exclusive; the previous
// maximum becomes Standard while every other explicit choice stays
// untouched.
func (d *DashboardState) SetPaneSize(pane SupportPane, size core.PaneSize) {
	previous := d.paneSizes
	*d.paneSizeMut(pane) = size
	d.reconcilePaneSizes(&pane)
	d.clampSelections()
	if d.paneSizes != previous {
		if workspaceID, ok := d.ActiveWorkspaceID(); ok {
			d.workspacePaneSizesModified[workspaceID] = struct{}{}
		}
	}
}

// CycleFocusedPaneSize cycles the focused support pane without moving the
// keyboard. Prompt is not resizable, so it explains how to choose a pane
// instead.
func (d *DashboardState) CycleFocusedPaneSize() {
	pane, ok := d.focus.SupportPane()
	if !ok {
		d.SetNotice("Select Sessions, Targets, Profiles, or Notifications before cycling the pane size.")
		return
	}
	next := d.PaneSize(pane).Cycled()
	if next == core.PaneSizeMaximized && !d.paneMaximizeEnabled(pane) {
		next = next.Cycled()
	}
	d.SetPaneSize(pane, next)
}

// TogglePanePreset is the pane preset's stable global arrangement: restore
// any custom layout to all Standard; from all Standard, minimize every
// support pane for the conversation.
func (d *DashboardState) TogglePanePreset() {
	if d.paneSizes.AllStandard() && d.notifications.size == core.PaneSizeStandard {
		d.notifications.size = core.PaneSizeMinimized
		d.paneSizes = core.PaneSizes{
			Sessions: core.PaneSizeMinimized,
			Targets:  core.PaneSizeMinimized,
			Quota:    core.PaneSizeMinimized,
		}
	} else {
		d.notifications.size = core.PaneSizeStandard
		d.paneSizes = core.NewPaneSizes()
	}
	d.clampSelections()
}

func (d *DashboardState) SessionsMinimized() bool {
	return d.narrowLayout || d.PaneSize(SupportPaneSessions) == core.PaneSizeMinimized
}

// PendingElicitations is the pending questions from the latest accepted
// full projection. A startup summary intentionally reports none (ok false),
// because it does not carry the complete request list and must not
// invalidate a local draft.
func (d *DashboardState) PendingElicitations(sessionID string) (uint64, []core.ElicitationRequest, bool) {
	panic("unported: mj-tui/src/dashboard_panes.rs pending_elicitations")
}

func (d *DashboardState) focusedRowsVisible() bool {
	pane, ok := d.focus.SupportPane()
	return !ok || pane == SupportPaneSessions || d.PaneSize(pane) != core.PaneSizeMinimized
}

// ModalOpen is whether a modal dialog or wizard owns the keyboard.
func (d *DashboardState) ModalOpen() bool {
	_, dashboard := d.mode.(ModeDashboard)
	return d.awayDialog() != nil || d.paneMenu != nil || !dashboard
}
