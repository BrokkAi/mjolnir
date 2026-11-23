// Port of mj-tui/src/wizards/dashboard/resume.rs.

// The resume wizard's steps, Move preparation, archive restores and the
// prerequisite checks a resume runs before it launches.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// archiveRestoreInFlight is the answer to a second restore of an archive
// whose first has not reported back.
const archiveRestoreInFlight = "Restore already started."

// resumeInFlight is the answer to a second Resume press while the first
// press's check is still running.
const resumeInFlight = "Resume already started."

func (d *DashboardState) advanceResumeWizard(wizard *ResumeWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::advance_resume_wizard")
}

func (d *DashboardState) startMovePreparation(wizard *ResumeWizard, profileID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::start_move_preparation")
}

// takeStaleMovePreparation prepares a Move again once an edit on its review
// discarded the old preparation, such as a new delegation choice. A failed
// preparation waits for the explicit retry instead.
func (d *DashboardState) takeStaleMovePreparation() (DashboardAction, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::take_stale_move_preparation")
}

func (d *DashboardState) requestMovePreparationForReview(wizard *ResumeWizard, profileID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::request_move_preparation_for_review")
}

func (d *DashboardState) preflightResumeSessionAction(wizard *ResumeWizard, profileID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::preflight_resume_session_action")
}

// claimArchiveRestore marks the restore of the archived transcript wikiID as
// started. Answers false, with a notice, when one already is: each restore
// makes a new session, so a second one would be a duplicate.
func (d *DashboardState) claimArchiveRestore(wikiID string) bool {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::claim_archive_restore")
}

// archiveRestoreInFlight reports whether a restore of wikiID has been sent
// and not reported back.
func (d *DashboardState) archiveRestoreInFlight(wikiID string) bool {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::archive_restore_in_flight")
}

// FinishArchiveRestore ends the restore that launch sent, once the daemon
// has registered its session or refused it. Any other launch is left alone.
func (d *DashboardState) FinishArchiveRestore(launch DashboardAction) {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::finish_archive_restore")
}

// resumePreflightInFlight reports whether a live resume's check has been
// sent and has not reported back.
func (d *DashboardState) resumePreflightInFlight() bool {
	return d.resumePreflightGeneration != nil && *d.resumePreflightGeneration == d.SessionPreflightGeneration()
}

// FailResumePreflight keeps a failed check visible until the user explicitly
// retries it.
func (d *DashboardState) FailResumePreflight(message string) {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::fail_resume_preflight")
}

// EndResumePreflight ends a live resume's check that has failed, so Resume
// can start another, and moves the generation on so that any other result
// sent under the same generation is dropped. The other results end the check
// the same way: Ready closes the wizard, and the conversion and origin
// dialogs move the generation on when they open. Does nothing when no resume
// check is in flight, so a new-session check that shares the failure path is
// left alone.
func (d *DashboardState) EndResumePreflight() {
	if d.resumePreflightInFlight() {
		d.invalidateSessionPreflight()
	}
}

func (d *DashboardState) ApplyMovePreparation(requestID uint64, preparation core.MovePreparation) bool {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::apply_move_preparation")
}

func (d *DashboardState) SetMovePreparationFailed(sessionID string, requestID uint64, message string) bool {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::set_move_preparation_failed")
}

func (d *DashboardState) TakeMovePreparation(sessionID string) *core.MovePreparation {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::take_move_preparation")
}

func (d *DashboardState) ApplySessionMountPreflightFailure(source string, message string) {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::apply_session_mount_preflight_failure")
}

func (d *DashboardState) FinishSessionMountPreflight() {
	d.CancelModal()
}

// BeginRemoteSessionPreflight marks the new-session review as waiting for
// its network clone plan. The modal stays open so the result can be reviewed
// before creation.
func (d *DashboardState) BeginRemoteSessionPreflight(generation uint64) {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::begin_remote_session_preflight")
}

// ApplyRemoteSessionPreflight applies a completed network clone plan, or
// err, retaining the error in the modal when the configured bundle cannot be
// used as an isolated source.
func (d *DashboardState) ApplyRemoteSessionPreflight(generation uint64, repositories []RemoteRepositoryPreview, err error) {
	panic("unported: mj-tui/src/wizards/dashboard/resume.rs DashboardState::apply_remote_session_preflight")
}
