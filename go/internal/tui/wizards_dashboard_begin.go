// Port of mj-tui/src/wizards/dashboard/begin.rs.

// Opening the new-session, resume, archive-restore and Move wizards.

package tui

import (
	"fmt"
	"maps"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recentLaunchChoice is the launch the new-session wizard seeds its choices
// from: the last interactive launch when this client made one, else the
// newest session. Nil when there is none.
func (d *DashboardState) recentLaunchChoice() *client.LaunchRecency {
	config := d.config.Get()
	var choice *client.LaunchRecency
	if recent := d.recentLaunch; recent != nil {
		interactive := []client.LaunchRecency{{
			BundleID:         recent.BundleID,
			LastProfile:      recent.ProfileID,
			TargetTemplateID: recent.TargetID,
		}}
		choice = mostRecentConfiguredSession(config, interactive)
	} else {
		choice = mostRecentConfiguredSession(config, d.launchRecency)
	}
	if choice == nil {
		return nil
	}
	copied := *choice
	return &copied
}

func (d *DashboardState) newWizardHasAutoPrimary() bool {
	return d.config.Get().Subagents.PrimaryConfigured()
}

func (d *DashboardState) newWizardProfileCount() int {
	count := 0
	for range d.offeredProfiles() {
		count++
	}
	if d.newWizardHasAutoPrimary() {
		count++
	}
	return count
}

// newWizardProfileID is the profile id of the wizard's profile row index:
// the empty id for the Auto (Primary) row, false past the end.
func (d *DashboardState) newWizardProfileID(index int) (string, bool) {
	if d.newWizardHasAutoPrimary() {
		if index == 0 {
			return "", true
		}
		return d.nthOfferedProfile(index - 1)
	}
	return d.nthOfferedProfile(index)
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
	offset := 0
	if d.newWizardHasAutoPrimary() {
		offset = 1
	}
	session := d.recentLaunchChoice()
	if session == nil {
		return 0
	}
	position := 0
	for id := range d.offeredProfiles() {
		if id == session.LastProfile {
			return position + offset
		}
		position++
	}
	return 0
}

func (d *DashboardState) beginNew() DashboardAction {
	if action := d.gitIdentityGateAction(); action != nil {
		return action
	}
	if mode := d.goMode; mode != nil {
		if mode.Recipe != nil {
			return DashboardActionGoLaunch{Recipe: *mode.Recipe}
		}
		return d.changeGoSetup()
	}
	return d.beginNewWizard()
}

func (d *DashboardState) beginNewWizard() DashboardAction {
	// Opening a picker is when what is there matters; the answer arrives in
	// the background and changes the list only if it differs. It is asked
	// for even when nothing is offered now, so a stale answer is replaced.
	d.wantAvailabilityCheck()
	anyProfile := false
	for range d.offeredProfiles() {
		anyProfile = true
		break
	}
	if !anyProfile {
		d.beginSettingsSection("profiles", nil)
		if missing, ok := d.missingProfilesNotice(); ok {
			d.notices.Set(missing)
		}
		return DashboardActionNone{}
	}
	if len(d.offeredTargetIndices()) == 0 {
		d.beginSettingsSection("targets", nil)
		var missing []string
		for _, line := range d.missingTargetLines() {
			missing = append(missing, line.Full)
		}
		if len(missing) > 0 {
			d.notices.Set(fmt.Sprintf("No runtime is available here. %s", strings.Join(missing, " ")))
		}
		return DashboardActionNone{}
	}
	config := d.config.Get()
	recent := d.recentLaunchChoice()
	profile := d.defaultNewProfileIndex()
	bundle := 0
	if recent != nil {
		if index := slices.Index(bundleIDsByRecentCreation(config, d.launchRecency), recent.BundleID); index >= 0 {
			bundle = index
		}
	}
	wantedTarget := "localhost"
	if recent != nil {
		wantedTarget = recent.TargetTemplateID
	}
	target := d.offeredTargetIndexOrFirst(wantedTarget)
	profileID, ok := d.newWizardProfileID(profile)
	if !ok {
		panic("an offered profile was checked above")
	}
	state := d.state.Get()
	var remembered *core.SubagentPolicy
	if policy, ok := state.RememberedSubagentPolicyFor(profileID); ok {
		remembered = &policy
	}
	delegation := SubagentDraft{}
	delegation.refreshCreate(profileID, config, remembered, state.LastOracleChoiceFor(profileID))
	expandedProviders := map[core.QuotaProvider]struct{}{}
	if provider := newWizardProviderForProfile(d, profile); provider != nil {
		expandedProviders[provider] = struct{}{}
	}
	workspaceID := ""
	if active := d.activeWorkspaceID.Get(); *active != nil {
		workspaceID = **active
	}
	picker := defaultProjectPicker()
	d.mode = ModeNew{&NewWizard{
		subagents:         &delegation,
		workspaceID:       workspaceID,
		step:              WizardStepProfile,
		profile:           profile,
		expandedProviders: expandedProviders,
		bundle:            bundle,
		target:            target,
		mounts:            newMountWizard(nil),
		projectPicker:     &picker,
		newBundleSource:   chat.NewPathInput(),
		projectDirectory:  chat.NewPathInput(),
		form:              chat.NewDialog[WizardControl](),
	}}
	d.mountHistoryRefreshPending = true
	return d.skipInitialProfile()
}

// BeginResumeFor opens the resume wizard for one session by id. The
// dashboard reaches this for a failed but checkpointed session; the resume
// dialog reaches it for a stopped one.
func (d *DashboardState) BeginResumeFor(sessionID string) DashboardAction {
	workspaceID := ""
	if active := d.activeWorkspaceID.Get(); *active != nil {
		workspaceID = **active
	}
	return d.beginResumeInWorkspace(sessionID, workspaceID)
}

func (d *DashboardState) beginResumeInWorkspace(sessionID string, workspaceID string) DashboardAction {
	record := d.SessionRecord(sessionID)
	if record == nil {
		return DashboardActionNone{}
	}
	session := *record
	offer := d.verdict(sessionID).Resume
	if !offer.Offered() {
		var message string
		switch offer {
		case client.SessionOfferActive:
			message = "This session is active; press Enter to open it."
		case client.SessionOfferMoveQueueHeld, client.SessionOfferMoveEnvironmentRetained:
			message = "Retry Move to recover this session."
		case client.SessionOfferTransitioning, client.SessionOfferOperationInProgress:
			message = "This session already has an operation in progress."
		default:
			message = "This session has no verified recovery copy to resume."
		}
		d.notices.Set(message)
		return DashboardActionNone{}
	}
	if _, retry := d.failedLaunchRetries[sessionID]; retry {
		return DashboardActionResumeSession{
			SessionID:          session.ID,
			WorkspaceID:        session.WorkspaceID,
			ProfileID:          session.LastProfile,
			TargetTemplateID:   session.TargetTemplateID,
			AdditionalMounts:   session.AdditionalMounts,
			ResourceAllocation: session.ResourceAllocation,
			DiscardQueue:       false,
		}
	}
	d.wantAvailabilityCheck()
	compatible := d.compatibleProfiles(session.ID)
	if len(compatible) == 0 || len(d.offeredTargetIndices()) == 0 {
		notice, ok := d.missingProfilesNotice()
		if !ok {
			notice = "Resume needs a profile and a target template."
		}
		d.notices.Set(notice)
		return DashboardActionNone{}
	}
	profile := max(slices.IndexFunc(compatible, func(candidate CompatibleProfile) bool {
		return candidate.ID == session.LastProfile
	}), 0)
	target := d.offeredTargetIndexOrFirst(session.TargetTemplateID)
	var policy core.SubagentPolicy
	if session.Subagents != nil {
		policy = *session.Subagents
	}
	delegation := newSubagentDraft(policy).withOracle(session.Oracle, wizardClonedChoice(session.OracleChoice))
	d.mode = ModeResume{&ResumeWizard{
		subagents:   &delegation,
		sessionID:   session.ID,
		source:      ResumeSourceSession,
		title:       session.ListedTitle(),
		workspaceID: workspaceID,
		step:        WizardStepProfile,
		profile:     profile,
		target:      target,
		mounts:      mountWizardWithMounts(nil, slices.Clone(session.AdditionalMounts)),
		form:        chat.NewDialog[WizardControl](),
	}}
	d.mountHistoryRefreshPending = true
	return d.skipInitialProfile()
}

// beginArchiveRestore opens the resume wizard for an archived SessionWiki
// session.
//
// The Mjolnir record is gone, but the index carries the profile and target
// the session ran under, so the wizard opens on them. An id the
// configuration no longer has leaves that step on its first choice, which is
// what a restore with nothing to go on has always done.
func (d *DashboardState) beginArchiveRestore(wikiID string, title string, profileID *string, targetID *string) DashboardAction {
	d.wantAvailabilityCheck()
	anyProfile := false
	for range d.offeredProfiles() {
		anyProfile = true
		break
	}
	if !anyProfile || len(d.offeredTargetIndices()) == 0 {
		notice, ok := d.missingProfilesNotice()
		if !ok {
			notice = "Restoring needs a profile and a target template."
		}
		d.notices.Set(notice)
		return DashboardActionNone{}
	}
	// A wizard opened now could only end in a refused Restore.
	if d.archiveRestoreInFlight(wikiID) {
		d.notices.Set(archiveRestoreInFlight)
		return DashboardActionNone{}
	}
	// The same lists the wizard indexes into: resumeWizardProfiles for an
	// archive source, and every configured target.
	profile := 0
	if profileID != nil {
		position := 0
		for id := range d.offeredProfiles() {
			if id == *profileID {
				profile = position
				break
			}
			position++
		}
	}
	target := 0
	if targetID != nil {
		if index := slices.Index(slices.Sorted(maps.Keys(d.config.Get().Targets)), *targetID); index >= 0 {
			target = index
		}
	}
	workspaceID := ""
	if active := d.activeWorkspaceID.Get(); *active != nil {
		workspaceID = **active
	}
	d.mode = ModeResume{&ResumeWizard{
		subagents:   &SubagentDraft{},
		sessionID:   wikiID,
		source:      ResumeSourceArchive,
		title:       title,
		workspaceID: workspaceID,
		step:        WizardStepProfile,
		profile:     profile,
		target:      target,
		mounts:      mountWizardWithMounts(nil, nil),
		form:        chat.NewDialog[WizardControl](),
	}}
	d.mountHistoryRefreshPending = true
	return d.skipInitialProfile()
}

// beginMove opens the resume controls for an active session as a Move
// operation. The source workspace and session identity are fixed; only the
// destination profile, target, sizing, and attachments are editable.
func (d *DashboardState) beginMove() DashboardAction {
	selected := d.selectedSession()
	if selected == nil {
		return DashboardActionNone{}
	}
	session := *selected
	d.wantAvailabilityCheck()
	compatible := d.compatibleProfiles(session.ID)
	if len(compatible) == 0 || len(d.offeredTargetIndices()) == 0 {
		notice, ok := d.missingProfilesNotice()
		if !ok {
			notice = "Move needs a profile and a target template."
		}
		d.notices.Set(notice)
		return DashboardActionNone{}
	}
	profile := max(slices.IndexFunc(compatible, func(candidate CompatibleProfile) bool {
		return candidate.ID == session.LastProfile
	}), 0)
	target := d.offeredTargetIndexOrFirst(session.TargetTemplateID)
	var policy core.SubagentPolicy
	if session.Subagents != nil {
		policy = *session.Subagents
	}
	delegation := newSubagentDraft(policy).withOracle(session.Oracle, wizardClonedChoice(session.OracleChoice))
	workspaceID := ""
	if active := d.activeWorkspaceID.Get(); *active != nil {
		workspaceID = **active
	}
	d.mode = ModeResume{&ResumeWizard{
		subagents:          &delegation,
		sessionID:          session.ID,
		source:             ResumeSourceSession,
		title:              session.ListedTitle(),
		workspaceID:        workspaceID,
		moving:             true,
		step:               WizardStepProfile,
		profile:            profile,
		target:             target,
		mounts:             mountWizardWithMounts(nil, slices.Clone(session.AdditionalMounts)),
		resourceAllocation: session.ResourceAllocation,
		// Move's safe default is to leave pending work idle. The review
		// checkbox can explicitly opt into starting it after readiness.
		discardQueue: true,
		form:         chat.NewDialog[WizardControl](),
	}}
	d.mountHistoryRefreshPending = true
	return d.skipInitialProfile()
}

// BeginMoveRecovery opens the Move wizard for a retained failure while the
// source is still live. The failed destination is prefilled so the user can
// inspect the exact interruption and queue choice before retrying it.
func (d *DashboardState) BeginMoveRecovery(operation core.MoveOperation) {
	record := d.state.Get().Sessions.GetShared(operation.Selection.SessionID)
	if record == nil {
		d.SetFailureNotice("Move recovery session is no longer available.")
		return
	}
	session := *record
	if !session.State.IsActive() {
		d.SetFailureNotice(
			"Move source is stopped; use Resume with previous settings or retry the retained destination.",
		)
		return
	}
	if operation.Selection.ProfileID == nil {
		d.SetFailureNotice("Move recovery has no retained destination profile.")
		return
	}
	profileID := *operation.Selection.ProfileID
	if operation.Selection.TargetTemplateID == nil {
		d.SetFailureNotice("Move recovery has no retained destination target.")
		return
	}
	targetID := *operation.Selection.TargetTemplateID
	profile := slices.IndexFunc(d.compatibleProfiles(session.ID), func(candidate CompatibleProfile) bool {
		return candidate.ID == profileID
	})
	if profile < 0 {
		d.SetFailureNotice(fmt.Sprintf("Move profile %s is no longer configured.", profileID))
		return
	}
	target := slices.Index(slices.Sorted(maps.Keys(d.config.Get().Targets)), targetID)
	if target < 0 {
		d.SetFailureNotice(fmt.Sprintf("Move target %s is no longer configured.", targetID))
		return
	}
	var policy core.SubagentPolicy
	switch {
	case operation.Selection.Subagents != nil:
		policy = *operation.Selection.Subagents
	case session.Subagents != nil:
		policy = *session.Subagents
	}
	delegation := newSubagentDraft(policy)
	mounts := session.AdditionalMounts
	if operation.Selection.AdditionalMounts != nil {
		mounts = *operation.Selection.AdditionalMounts
	}
	allocation := operation.Selection.ResourceAllocation
	if allocation == nil {
		allocation = session.ResourceAllocation
	}
	workspaceID := ""
	if active := d.activeWorkspaceID.Get(); *active != nil {
		workspaceID = **active
	}
	d.mode = ModeResume{&ResumeWizard{
		subagents:          &delegation,
		sessionID:          session.ID,
		source:             ResumeSourceSession,
		title:              session.ListedTitle(),
		workspaceID:        workspaceID,
		moving:             true,
		step:               WizardStepProfile,
		profile:            profile,
		target:             target,
		mounts:             mountWizardWithMounts(nil, slices.Clone(mounts)),
		resourceAllocation: allocation,
		discardQueue:       operation.Queue == core.ResumeQueueDispositionDiscard,
		form:               chat.NewDialog[WizardControl](),
	}}
}

func (d *DashboardState) skipInitialProfile() DashboardAction {
	switch mode := d.mode.(type) {
	case ModeNew:
		if mode.ProfileCount(d) != 1 {
			return DashboardActionNone{}
		}
		d.mode = ModeDashboard{}
		mode.SetProfileStepSkipped(true)
		mode.profile = 0
		return d.advanceNewWizard(mode.NewWizard)
	case ModeResume:
		if mode.ProfileCount(d) != 1 {
			return DashboardActionNone{}
		}
		d.mode = ModeDashboard{}
		mode.SetProfileStepSkipped(true)
		mode.profile = 0
		return d.advanceResumeWizard(mode.ResumeWizard)
	}
	return DashboardActionNone{}
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
	d.state.Mut().MountHistory = core.CollectSnapshotMap(maps.All(history))
	entries := d.recentProjectEntries()
	mode, ok := d.mode.(ModeNew)
	if !ok {
		return
	}
	wizard := mode.NewWizard
	if wizard.step == WizardStepNewBundle && wizard.projectPicker.tab == ProjectTabRecent {
		selected := 0
		if picker := wizard.projectPicker; picker.selected >= 0 && picker.selected < len(picker.entries) {
			source := picker.entries[picker.selected].Source
			if index := slices.IndexFunc(entries, func(entry core.ProjectEntry) bool {
				return entry.Source == source
			}); index >= 0 {
				selected = index
			}
		}
		wizard.projectPicker.selected = selected
		wizard.projectPicker.entries = entries
	}
	if wizard.step == WizardStepProjectDirectory {
		wizard.projectHistory = nil
		targets := d.config.Get().Targets
		ids := slices.Sorted(maps.Keys(targets))
		if wizard.target >= 0 && wizard.target < len(ids) {
			if host, ok := core.ProjectHistoryHost(targets[ids[wizard.target]]); ok {
				wizard.projectHistory = slices.Clone(d.state.Get().ProjectDirectories(host))
			}
		}
		wizard.projectHistoryIndex = min(wizard.projectHistoryIndex, max(len(wizard.projectHistory)-1, 0))
	}
}

// wizardClonedChoice is Rust's `.as_deref().cloned()` of a boxed choice: a
// copy the wizard's draft may change without touching the record it came
// from.
func wizardClonedChoice(choice *core.SubagentChoice) *core.SubagentChoice {
	if choice == nil {
		return nil
	}
	copied := *choice
	return &copied
}
