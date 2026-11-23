// Port of mj-tui/src/wizards/dashboard/new_session.rs.

// The new-session wizard's steps, its prerequisite checks, and the launch
// request it ends in, plus project bundle creation and removal.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (d *DashboardState) validateNewProject(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::validate_new_project")
}

func (d *DashboardState) advanceNewWizard(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::advance_new_wizard")
}

func (d *DashboardState) createSessionAction(wizard *NewWizard) DashboardAction {
	action := d.createSessionActionWithoutClosing(wizard)
	d.CancelModal()
	return action
}

func (d *DashboardState) invalidateNewRemotePreflight(wizard *NewWizard) {
	d.invalidateSessionPreflight()
	wizard.remoteRepositories = nil
	wizard.remotePreflightInFlight = false
	wizard.remotePreflightError = nil
}

// preflightCreateSessionAction: Create launches an isolated session only
// from a completed prerequisite check. Retry clears the failure so
// TakePrerequisiteCheck starts the check again.
func (d *DashboardState) preflightCreateSessionAction(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::preflight_create_session_action")
}

// TakePrerequisiteCheck starts the prerequisite check an isolated creation
// review needs before Create is enabled: attached directories first, then
// network sources. The dashboard loop asks after every event and background
// update, so a review never waits on a check that nothing started, whichever
// path opened it.
func (d *DashboardState) TakePrerequisiteCheck() (DashboardAction, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::take_prerequisite_check")
}

func (d *DashboardState) createSessionActionWithoutClosing(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::create_session_action_without_closing")
}

// beginNewSessionLike is "New session with these settings": it starts a
// top-level session with the settings of sourceID, without a dialog.
//
// The request is the CreateSession the wizard ends in, so the host shows the
// launch, registers it and reports a refusal exactly as it does for a wizard
// launch. Everything that belongs to the source itself stays behind: its
// title, conversation, parent link and managed worktree (a new worktree is
// requested when the source had one).
//
// A choice that cannot apply now is replaced and the launch notice says so.
// When the runtime or the project cannot apply, nothing sensible replaces
// it, so the ordinary new-session wizard opens instead.
func (d *DashboardState) beginNewSessionLike(sourceID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::begin_new_session_like")
}

// newSessionLikeRequest is the creation request for beginNewSessionLike, or
// why the source's runtime or project cannot be used (the error's text).
func (d *DashboardState) newSessionLikeRequest(source *core.SessionRecord) (DashboardAction, error) {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::new_session_like_request")
}

func (d *DashboardState) ApplyCreatedBundle(config core.Config, bundleID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::apply_created_bundle")
}

// beginBundleRemoval starts removing the selected saved project, or explains
// why a session still needs it. The config write re-checks against fresh
// state.
func (d *DashboardState) beginBundleRemoval(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::begin_bundle_removal")
}

// ApplyRemovedBundle installs the config without the removed project and
// keeps the selection on a row that still exists.
func (d *DashboardState) ApplyRemovedBundle(config core.Config, bundleID string) {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::apply_removed_bundle")
}

func (d *DashboardState) FailBundleRemoval(reason string) {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::fail_bundle_removal")
}

// FailBundleCreation reopens the new-bundle editor after its asynchronous
// create failed. The draft remains untouched so the user can correct and
// retry it.
func (d *DashboardState) FailBundleCreation(reason string) {
	panic("unported: mj-tui/src/wizards/dashboard/new_session.rs DashboardState::fail_bundle_creation")
}
