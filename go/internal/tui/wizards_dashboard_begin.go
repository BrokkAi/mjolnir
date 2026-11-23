// Port of mj-tui/src/wizards/dashboard/begin.rs.

// Opening the new-session, resume, archive-restore and Move wizards.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recentLaunchChoice is the launch the new-session wizard seeds its choices
// from: the last interactive launch when this client made one, else the
// newest session. Nil when there is none.
func (d *DashboardState) recentLaunchChoice() *client.LaunchRecency {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::recent_launch_choice")
}

func (d *DashboardState) newWizardHasAutoPrimary() bool {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::new_wizard_has_auto_primary")
}

func (d *DashboardState) newWizardProfileCount() int {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::new_wizard_profile_count")
}

// newWizardProfileID is the profile id of the wizard's profile row index:
// the empty id for the Auto (Primary) row, false past the end.
func (d *DashboardState) newWizardProfileID(index int) (string, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::new_wizard_profile_id")
}

func (d *DashboardState) newWizardProfileLabel(index int) string {
	if d.newWizardHasAutoPrimary() && index == 0 {
		return "Auto (Primary)"
	}
	id, _ := d.newWizardProfileID(index)
	return id
}

// defaultNewProfileIndex is the index among the offered profiles the wizard
// opens on. A launch that copies another session's settings uses it too when
// that session's profile cannot be used.
func (d *DashboardState) defaultNewProfileIndex() int {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::default_new_profile_index")
}

func (d *DashboardState) beginNew() DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_new")
}

func (d *DashboardState) beginNewWizard() DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_new_wizard")
}

// BeginResumeFor opens the resume wizard for one session by id. The
// dashboard reaches this for a failed but checkpointed session; the resume
// dialog reaches it for a stopped one.
func (d *DashboardState) BeginResumeFor(sessionID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_resume_for")
}

func (d *DashboardState) beginResumeInWorkspace(sessionID string, workspaceID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_resume_in_workspace")
}

// beginArchiveRestore opens the resume wizard for an archived SessionWiki
// session.
//
// The Mjolnir record is gone, but the index carries the profile and target
// the session ran under, so the wizard opens on them. An id the
// configuration no longer has leaves that step on its first choice, which is
// what a restore with nothing to go on has always done.
func (d *DashboardState) beginArchiveRestore(wikiID string, title string, profileID *string, targetID *string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_archive_restore")
}

// beginMove opens the resume controls for an active session as a Move
// operation. The source workspace and session identity are fixed; only the
// destination profile, target, sizing, and attachments are editable.
func (d *DashboardState) beginMove() DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_move")
}

// BeginMoveRecovery opens the Move wizard for a retained failure while the
// source is still live. The failed destination is prefilled so the user can
// inspect the exact interruption and queue choice before retrying it.
func (d *DashboardState) BeginMoveRecovery(operation core.MoveOperation) {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::begin_move_recovery")
}

func (d *DashboardState) skipInitialProfile() DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::skip_initial_profile")
}

// TakeMountHistoryRefresh asks once per wizard open for the stored mount and
// project history, which sessions created after startup add to. The
// dashboard loop polls this after every event, as it does
// TakePrerequisiteCheck, and runs the read on a worker.
func (d *DashboardState) TakeMountHistoryRefresh() (DashboardAction, bool) {
	if !d.mountHistoryRefreshPending {
		return nil, false
	}
	d.mountHistoryRefreshPending = false
	return DashboardActionLoadMountHistory{}, true
}

// ApplyMountHistory replaces the remembered mount and project history with
// what the database holds now, by host.
func (d *DashboardState) ApplyMountHistory(history map[string][]string) {
	panic("unported: mj-tui/src/wizards/dashboard/begin.rs DashboardState::apply_mount_history")
}
