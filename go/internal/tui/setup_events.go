// Port of mj-tui/src/setup/events.rs.

package tui

import (
	"fmt"
	"maps"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// BeginManagedSetup opens Settings on the guided chooser for a harness
// Mjolnir installs itself.
func (d *DashboardState) BeginManagedSetup() {
	d.beginSettingsSection("profiles", nil)
	if dialog := setupDialogMut(d.mode); dialog != nil {
		dialog.beginManagedSetup()
	}
}

// beginSettingsSection opens Settings on section, and on its entry when the
// draft has one.
func (d *DashboardState) beginSettingsSection(section string, entry *string) {
	panic("unported: mj-tui/src/setup/events.rs begin_settings_section")
}

// beginStagedSettings opens Settings on the page of the first edit with
// every edit staged in the draft, unsaved. An open Settings keeps its other
// unsaved edits and takes these as well. Nothing changes when any edit does
// not apply.
func (d *DashboardState) beginStagedSettings(edits []client.SettingEdit, notice string) error {
	panic("unported: mj-tui/src/setup/events.rs begin_staged_settings")
}

// BeginSetup opens Settings on its first page.
func (d *DashboardState) BeginSetup() {
	panic("unported: mj-tui/src/setup/events.rs begin_setup")
}

func (d *DashboardState) dismissSetup(dialog *SetupDialog) DashboardAction {
	panic("unported: mj-tui/src/setup/events.rs dismiss_setup")
}

// beginSetupReview opens the Code Review editor of the open Settings (Rust
// test support).
func (d *DashboardState) beginSetupReview() DashboardAction {
	panic("unported: mj-tui/src/setup/events.rs begin_setup_review")
}

// SetProfileCapabilities replaces the profile capability snapshot.
func (d *DashboardState) SetProfileCapabilities(snapshot core.ProfileCapabilitiesSnapshot) {
	panic("unported: mj-tui/src/setup/events.rs set_profile_capabilities")
}

// refreshReviewChoices recomputes the open Code Review editor's choices and
// Autoconfigure suggestion from the current capabilities and quota reports.
func (d *DashboardState) refreshReviewChoices() {
	panic("unported: mj-tui/src/setup/events.rs refresh_review_choices")
}

// SetProfileLoginStatuses replaces the saved-profile sign-in statuses read
// off the loop.
func (d *DashboardState) SetProfileLoginStatuses(generation uint64, statuses map[string]core.LoginStatus) {
	if generation < d.profileLoginGeneration {
		return
	}
	d.profileLoginGeneration = generation
	d.profileLoginStatuses = statuses
	if dialog := setupDialogMut(d.mode); dialog != nil {
		dialog.loginStatuses = maps.Clone(d.profileLoginStatuses)
	}
}

func (d *DashboardState) profileLoginStatus(profileID string) (core.LoginStatus, bool) {
	status, ok := d.profileLoginStatuses[profileID]
	return status, ok
}

// Config is the configuration this dashboard is currently showing.
func (d *DashboardState) Config() *core.Config {
	panic("unported: mj-tui/src/setup/events.rs config")
}

// invalidateProfileLoginStatuses drops statuses and any read still in flight
// after the profile set changes. The generation bump makes an older result
// unusable.
func (d *DashboardState) invalidateProfileLoginStatuses() {
	panic("unported: mj-tui/src/setup/events.rs invalidate_profile_login_statuses")
}

// TakeProfileLoginRefresh takes the request to read saved-profile sign-in
// status off the loop.
func (d *DashboardState) TakeProfileLoginRefresh() (uint64, bool) {
	if !d.profileLoginRefresh {
		return 0, false
	}
	d.profileLoginRefresh = false
	d.profileLoginGeneration++
	return d.profileLoginGeneration, true
}

// takeProfileHydration is Settings' prerequisite poll after every background
// reply; nil when there is nothing to send.
func (d *DashboardState) takeProfileHydration() DashboardAction {
	panic("unported: mj-tui/src/setup/events.rs take_profile_hydration")
}

// ApplyProfileHydration reports a failed capability warm-up for a demand
// this dashboard still waits on.
func (d *DashboardState) ApplyProfileHydration(key string, err error) {
	if err == nil {
		return
	}
	_, requested := d.profileHydrationRequests[key]
	sent := d.profileHydrationSent != nil && *d.profileHydrationSent == key
	if requested || sent {
		d.profileHydrationErrors[key] = err.Error()
		d.SetNotice(fmt.Sprintf("Profile capability hydration failed: %s", err.Error()))
	}
}

// TickReviewAutoconfigure: the one-second UI clock completes an
// Autoconfigure wait even if discovery never publishes another snapshot.
func (d *DashboardState) TickReviewAutoconfigure() bool {
	panic("unported: mj-tui/src/setup/events.rs tick_review_autoconfigure")
}

// takeSetupSubagentChoices fills the subagent pages' model choices from the
// capability snapshot; it never returns an action.
func (d *DashboardState) takeSetupSubagentChoices() DashboardAction {
	dialog := setupDialogMut(d.mode)
	if dialog == nil {
		return nil
	}
	unreachable, err := client.FillSubagentOptions(&dialog.draft, &d.profileCapabilities)
	if err != nil {
		return nil
	}
	syncSubagentDiscoveryNotice(dialog, unreachable)
	dialog.prepare()
	return nil
}

func (d *DashboardState) handleSetupEvent(event tea.Msg, dialog *SetupDialog) DashboardAction {
	panic("unported: mj-tui/src/setup/events.rs handle_setup_event")
}

func syncSubagentDiscoveryNotice(dialog *SetupDialog, unreachable *string) {
	if len(dialog.path) == 0 || dialog.path[0] != "subagents" {
		return
	}
	noticeIsDiscovery := dialog.notice != nil && strings.HasPrefix(*dialog.notice, "Discovery: ")
	if dialog.notice == nil || noticeIsDiscovery {
		dialog.notice = unreachable
	}
}
