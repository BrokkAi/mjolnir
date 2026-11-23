// Port of mj-tui/src/wizards/dashboard/resume.rs.

// The resume wizard's steps, Move preparation, archive restores and the
// prerequisite checks a resume runs before it launches.

package tui

import (
	"reflect"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// archiveRestoreInFlight is the answer to a second restore of an archive
// whose first has not reported back.
const archiveRestoreInFlight = "Restore already started."

// resumeInFlight is the answer to a second Resume press while the first
// press's check is still running.
const resumeInFlight = "Resume already started."

func (d *DashboardState) advanceResumeWizard(wizard *ResumeWizard) DashboardAction {
	profiles := d.resumeWizardProfiles(wizard)
	targets := d.config.Get().Targets
	switch wizard.step {
	case WizardStepProfile:
		wizard.step = WizardStepTarget
		if d.skipTargetStep(wizard) {
			return d.advanceResumeWizard(wizard)
		}
		wizard.form.Focus(stepInitial(wizard.step))
		action := d.initializeWizardResources(wizard)
		d.mode = ModeResume{wizard}
		return action
	case WizardStepTarget:
		targetID := nthKey(targets, wizard.target)
		if reason, ok := d.resumeTargetRejection(wizard.sessionID, targetID); ok {
			d.notices.Set(reason)
			d.mode = ModeResume{wizard}
			return DashboardActionNone{}
		}
		if targetIsSized(targets[targetID]) && wizard.resourceAllocation == nil {
			notice := "Resource sizing is not ready."
			if wizard.sizingError != nil {
				notice = *wizard.sizingError
			}
			d.notices.Set(notice)
			d.mode = ModeResume{wizard}
			return DashboardActionNone{}
		}
		wizard.mounts.historySelection = 0
		wizard.mounts.history = nil
		if host, ok := core.MountHistoryHost(targets[targetID]); ok {
			if history, ok := d.state.Get().MountHistory.Get(host); ok {
				wizard.mounts.history = slices.Clone(history)
			}
		}
		wizard.mounts.selectedMount = 0
		if wizard.SkipsReview() {
			wizard.step = WizardStepLaunching
		} else {
			wizard.step = WizardStepReview
		}
		wizard.form.Focus(WizardControlSubmit)
		if wizard.moving {
			if wizard.profile < 0 || wizard.profile >= len(profiles) {
				panic("move wizard is only opened with a compatible profile")
			}
			return d.requestMovePreparationForReview(wizard, profiles[wizard.profile].ID)
		}
		if wizard.SkipsReview() {
			profile := wizard.destinationProfile(d)
			return d.preflightResumeSessionAction(wizard, profile)
		}
		d.mode = ModeResume{wizard}
		return DashboardActionNone{}
	case WizardStepBundle:
		panic("resume does not select a bundle")
	case WizardStepReview, WizardStepLaunching:
		panic("review input is handled before picker navigation")
	case WizardStepMounts:
		panic("mount input is handled before picker navigation")
	case WizardStepNewBundle:
		panic("resume does not create bundles")
	default:
		panic("resume does not select a project directory")
	}
}

func (d *DashboardState) startMovePreparation(wizard *ResumeWizard, profileID string) DashboardAction {
	if wizard.subagentChange(d) != nil {
		if _, failed := wizard.subagents.error(); failed {
			d.mode = ModeResume{wizard}
			return DashboardActionNone{}
		}
	}
	targets := d.config.Get().Targets
	targetTemplateID := nthKey(targets, wizard.target)
	mounts := slices.Clone(wizard.mounts.mounts)
	clearResourceAllocation := false
	switch targets[targetTemplateID].(type) {
	case core.TargetTemplateLocalBare, core.TargetTemplateSshBare:
		clearResourceAllocation = true
	}
	d.nextMovePreparationRequestID++
	requestID := d.nextMovePreparationRequestID
	pending := requestID
	wizard.preparationRequestID = &pending
	wizard.preparing = true
	wizard.preparationError = nil
	subagents := wizard.subagentChange(d)
	var oracle *bool
	if wizard.subagents.oracleAvailable() {
		if record := d.SessionRecord(wizard.sessionID); record != nil && record.Oracle != wizard.subagents.oracle {
			enabled := wizard.subagents.oracle
			oracle = &enabled
		}
	}
	queue := core.ResumeQueueDispositionStart
	if wizard.discardQueue {
		queue = core.ResumeQueueDispositionDiscard
	}
	action := DashboardActionMoveSession{
		Subagents:               subagents,
		Oracle:                  oracle,
		Consent:                 core.SuspendConsent{},
		SessionID:               wizard.sessionID,
		ProfileID:               profileID,
		TargetTemplateID:        targetTemplateID,
		AdditionalMounts:        mounts,
		ResourceAllocation:      wizard.resourceAllocation,
		ClearResourceAllocation: clearResourceAllocation,
		PreparationRequestID:    &requestID,
		Queue:                   &queue,
	}
	d.mode = ModeResume{wizard}
	return action
}

// takeStaleMovePreparation prepares a Move again once an edit on its review
// discarded the old preparation, such as a new delegation choice. A failed
// preparation waits for the explicit retry instead.
func (d *DashboardState) takeStaleMovePreparation() (DashboardAction, bool) {
	mode, ok := d.mode.(ModeResume)
	if !ok {
		return nil, false
	}
	wizard := mode.ResumeWizard
	_, subagentsFailed := wizard.subagents.error()
	if !wizard.moving ||
		(wizard.step != WizardStepReview && wizard.step != WizardStepLaunching) ||
		(wizard.subagentChange(d) != nil && subagentsFailed) ||
		wizard.preparing ||
		wizard.preparation != nil ||
		wizard.preparationError != nil {
		return nil, false
	}
	profileID := wizard.destinationProfile(d)
	d.mode = ModeDashboard{}
	return d.startMovePreparation(wizard, profileID), true
}

func (d *DashboardState) requestMovePreparationForReview(wizard *ResumeWizard, profileID string) DashboardAction {
	if !wizard.moving || wizard.preparing || wizard.preparation != nil {
		d.mode = ModeResume{wizard}
		return DashboardActionNone{}
	}
	return d.startMovePreparation(wizard, profileID)
}

func (d *DashboardState) preflightResumeSessionAction(wizard *ResumeWizard, profileID string) DashboardAction {
	targetTemplateID := nthKey(d.config.Get().Targets, wizard.target)
	mounts := slices.Clone(wizard.mounts.mounts)
	queue := core.ResumeQueueDispositionStart
	if wizard.discardQueue {
		queue = core.ResumeQueueDispositionDiscard
	}
	if wizard.moving {
		if wizard.preparing {
			d.mode = ModeResume{wizard}
			return DashboardActionNone{}
		}
		if wizard.preparation == nil {
			return d.startMovePreparation(wizard, profileID)
		}
		selection := &wizard.preparation.Selection
		if drafted := wizard.subagentSelection(d); drafted != nil {
			var prepared core.SubagentPolicy
			if selection.Subagents != nil {
				prepared = *selection.Subagents
			} else if record := d.SessionRecord(wizard.sessionID); record != nil && record.Subagents != nil {
				prepared = *record.Subagents
			}
			if !wizardSubagentPolicyEqual(wizard.subagents.shown(&prepared), *drafted) {
				wizard.preparation = nil
				return d.startMovePreparation(wizard, profileID)
			}
		}
		additionalMounts := slices.Clone(wizard.mounts.mounts)
		if selection.AdditionalMounts != nil {
			additionalMounts = slices.Clone(*selection.AdditionalMounts)
		}
		action := DashboardActionMoveSession{
			Subagents:               selection.Subagents,
			Oracle:                  selection.Oracle,
			OracleChoice:            selection.OracleChoice,
			Consent:                 selection.Consent,
			SkipKeepPaths:           slices.Clone(selection.SkipKeepPaths),
			SessionID:               selection.SessionID,
			ProfileID:               *selection.ProfileID,
			TargetTemplateID:        *selection.TargetTemplateID,
			AdditionalMounts:        additionalMounts,
			ResourceAllocation:      selection.ResourceAllocation,
			ClearResourceAllocation: selection.ClearResourceAllocation,
			Queue:                   &queue,
		}
		wizard.form.SetSubmissionPending(true)
		d.mode = ModeResume{wizard}
		return action
	}
	if wizard.source == ResumeSourceArchive {
		if !d.claimArchiveRestore(wizard.sessionID) {
			d.mode = ModeResume{wizard}
			return DashboardActionNone{}
		}
		// A restored archive has no checkpoint and no repositories to
		// preflight: it starts as a new session and the summary follows.
		// With nothing left to check, the wizard closes now, as a live
		// resume's does once its launch starts. The dashboard loop shows
		// "Launching …" when the session registers. A failure opens the
		// launch-failure dialog, whose Retry sends this restore again.
		action := DashboardActionRestoreArchivedSession{
			WorkspaceID:      wizard.workspaceID,
			WikiID:           wizard.sessionID,
			ProfileID:        profileID,
			TargetTemplateID: targetTemplateID,
		}
		d.CancelModal()
		return action
	}
	// The wizard stays open while the attached directories and the
	// checkpoint's repositories are checked, so Resume can be pressed again
	// before the check reports back. A second check would race the first to
	// open the same dialog or start the same launch.
	if d.resumePreflightInFlight() {
		d.notices.Set(resumeInFlight)
		d.mode = ModeResume{wizard}
		return DashboardActionNone{}
	}
	wizard.preflightError = nil
	generation := d.SessionPreflightGeneration()
	d.resumePreflightGeneration = &generation
	launch := DashboardActionResumeSession{
		WorkspaceID:        wizard.workspaceID,
		SessionID:          wizard.sessionID,
		ProfileID:          profileID,
		TargetTemplateID:   targetTemplateID,
		AdditionalMounts:   slices.Clone(mounts),
		ResourceAllocation: wizard.resourceAllocation,
		DiscardQueue:       wizard.discardQueue,
	}
	d.mode = ModeResume{wizard}
	preflight := DashboardActionPreflightResumeRepositories{Launch: launch}
	if len(mounts) == 0 {
		return preflight
	}
	return DashboardActionValidateSessionMounts{
		TargetTemplateID: targetTemplateID,
		Mounts:           mounts,
		Launch:           preflight,
	}
}

// claimArchiveRestore marks the restore of the archived transcript wikiID as
// started. Answers false, with a notice, when one already is: each restore
// makes a new session, so a second one would be a duplicate.
func (d *DashboardState) claimArchiveRestore(wikiID string) bool {
	if _, inFlight := d.archiveRestoresInFlight[wikiID]; !inFlight {
		if d.archiveRestoresInFlight == nil {
			d.archiveRestoresInFlight = make(map[string]struct{})
		}
		d.archiveRestoresInFlight[wikiID] = struct{}{}
		return true
	}
	d.notices.Set(archiveRestoreInFlight)
	return false
}

// archiveRestoreInFlight reports whether a restore of wikiID has been sent
// and not reported back.
func (d *DashboardState) archiveRestoreInFlight(wikiID string) bool {
	_, inFlight := d.archiveRestoresInFlight[wikiID]
	return inFlight
}

// FinishArchiveRestore ends the restore that launch sent, once the daemon
// has registered its session or refused it. Any other launch is left alone.
func (d *DashboardState) FinishArchiveRestore(launch DashboardAction) {
	if restore, ok := launch.(DashboardActionRestoreArchivedSession); ok {
		delete(d.archiveRestoresInFlight, restore.WikiID)
	}
}

// resumePreflightInFlight reports whether a live resume's check has been
// sent and has not reported back.
func (d *DashboardState) resumePreflightInFlight() bool {
	return d.resumePreflightGeneration != nil && *d.resumePreflightGeneration == d.SessionPreflightGeneration()
}

// FailResumePreflight keeps a failed check visible until the user explicitly
// retries it.
func (d *DashboardState) FailResumePreflight(message string) {
	if mode, ok := d.mode.(ModeResume); ok {
		failure := message
		mode.preflightError = &failure
		mode.form.Focus(WizardControlSubmit)
		mode.form.SetSubmissionPending(false)
	}
	d.notices.Set(message)
	d.EndResumePreflight()
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
	sessionID := preparation.Selection.SessionID
	mode, ok := d.mode.(ModeResume)
	if !ok {
		return false
	}
	wizard := mode.ResumeWizard
	if !wizard.moving ||
		wizard.sessionID != sessionID ||
		(wizard.step != WizardStepReview && wizard.step != WizardStepLaunching) ||
		!wizard.preparing ||
		wizard.preparationRequestID == nil || *wizard.preparationRequestID != requestID {
		return false
	}
	if preview := preparation.ContainerUpgrade; preview != nil {
		d.ShowContainerUpgradePreview(*preview, true)
		return true
	}
	selection := &preparation.Selection
	if (selection.ClearResourceAllocation || selection.ResourceAllocation != nil) &&
		!reflect.DeepEqual(wizard.resourceAllocation, selection.ResourceAllocation) {
		wizard.resourceAllocation = selection.ResourceAllocation
		wizard.resourceEditor.reset(wizard.resourceAllocation)
	}
	wizard.preparation = &preparation
	wizard.preparing = false
	wizard.preparationError = nil
	return true
}

func (d *DashboardState) SetMovePreparationFailed(sessionID string, requestID uint64, message string) bool {
	mode, ok := d.mode.(ModeResume)
	if !ok {
		return false
	}
	wizard := mode.ResumeWizard
	if !wizard.moving ||
		wizard.sessionID != sessionID ||
		(wizard.step != WizardStepReview && wizard.step != WizardStepLaunching) ||
		!wizard.preparing ||
		wizard.preparationRequestID == nil || *wizard.preparationRequestID != requestID {
		return false
	}
	wizard.preparing = false
	wizard.preparation = nil
	wizard.preparationRequestID = nil
	failure := message
	wizard.preparationError = &failure
	wizard.form.Focus(WizardControlSubmit)
	return true
}

func (d *DashboardState) TakeMovePreparation(sessionID string) *core.MovePreparation {
	var preparation *core.MovePreparation
	if mode, ok := d.mode.(ModeResume); ok && mode.moving && mode.sessionID == sessionID {
		preparation = mode.preparation
		mode.preparation = nil
	}
	if preparation != nil {
		d.CancelModal()
	}
	return preparation
}

func (d *DashboardState) ApplySessionMountPreflightFailure(source string, message string) {
	sameSource := func(mount core.AdditionalMount) bool {
		return slices.Equal(resumePathComponents(mount.Source), resumePathComponents(source))
	}
	switch mode := d.mode.(type) {
	case ModeNew:
		wizard := mode.NewWizard
		if index := slices.IndexFunc(wizard.mounts.mounts, sameSource); index >= 0 {
			wizard.mounts.selectedMount = index
			if prepareSelectedMountEditor(&wizard.mounts) {
				wizard.step = WizardStepMounts
			}
		}
		// The check has ended. Keeping its failure on the review means
		// leaving the editor does not start the same failing check again.
		wizard.remotePreflightInFlight = false
		preflightFailure := message
		wizard.remotePreflightError = &preflightFailure
		mountFailure := message
		wizard.mounts.error = &mountFailure
	case ModeResume:
		wizard := mode.ResumeWizard
		if index := slices.IndexFunc(wizard.mounts.mounts, sameSource); index >= 0 {
			wizard.mounts.selectedMount = index
			if prepareSelectedMountEditor(&wizard.mounts) {
				wizard.step = WizardStepMounts
			}
		}
		mountFailure := message
		wizard.mounts.error = &mountFailure
	}
	d.EndResumePreflight()
}

func (d *DashboardState) FinishSessionMountPreflight() {
	d.CancelModal()
}

// BeginRemoteSessionPreflight marks the new-session review as waiting for
// its network clone plan. The modal stays open so the result can be reviewed
// before creation.
func (d *DashboardState) BeginRemoteSessionPreflight(generation uint64) {
	if generation != d.SessionPreflightGeneration() {
		return
	}
	if mode, ok := d.mode.(ModeNew); ok {
		mode.remotePreflightInFlight = true
		mode.remotePreflightError = nil
		mode.remoteRepositories = nil
	}
}

// ApplyRemoteSessionPreflight applies a completed network clone plan, or
// err, retaining the error in the modal when the configured bundle cannot be
// used as an isolated source.
func (d *DashboardState) ApplyRemoteSessionPreflight(generation uint64, repositories []RemoteRepositoryPreview, err error) {
	if generation != d.SessionPreflightGeneration() {
		return
	}
	mode, ok := d.mode.(ModeNew)
	if !ok {
		return
	}
	wizard := mode.NewWizard
	wizard.remotePreflightInFlight = false
	if err != nil {
		wizard.remoteRepositories = nil
		failure := err.Error()
		wizard.remotePreflightError = &failure
	} else {
		checked := repositories
		wizard.remoteRepositories = &checked
		wizard.remotePreflightError = nil
	}
	wizard.form.Focus(WizardControlSubmit)
}
