// Port of mj-tui/src/wizards/dashboard/new_session.rs.

// The new-session wizard's steps, its prerequisite checks, and the launch
// request it ends in, plus project bundle creation and removal.

package tui

import (
	"errors"
	"fmt"
	"reflect"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (d *DashboardState) validateNewProject(wizard *NewWizard) DashboardAction {
	directory := strings.TrimSpace(wizard.projectDirectory.Value())
	if directory == "" {
		message := "Project directory cannot be empty."
		wizard.projectDirectoryError = &message
	} else if err := core.ValidateAbsoluteInput(directory); err != nil {
		message := err.Error()
		wizard.projectDirectoryError = &message
	} else {
		targetTemplateID := nthKey(d.config.Get().Targets, wizard.target)
		wizard.projectDirectoryError = nil
		wizard.form.SetSubmissionPending(true)
		d.mode = ModeNew{wizard}
		return DashboardActionValidateProjectDirectory{
			TargetTemplateID: targetTemplateID,
			Directory:        directory,
		}
	}
	d.mode = ModeNew{wizard}
	return DashboardActionNone{}
}

func (d *DashboardState) advanceNewWizard(wizard *NewWizard) DashboardAction {
	config := d.config.Get()
	switch wizard.step {
	case WizardStepProfile:
		wizard.step = WizardStepTarget
		if d.skipTargetStep(wizard) {
			return d.advanceNewWizard(wizard)
		}
		wizard.form.Focus(stepInitial(wizard.step))
		action := d.initializeWizardResources(wizard)
		d.mode = ModeNew{wizard}
		return action
	case WizardStepBundle:
		wizard.step = WizardStepReview
		wizard.form.Focus(WizardControlSubmit)
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	case WizardStepTarget:
		targetTemplateID := nthKey(config.Targets, wizard.target)
		if reason, ok := d.targetUnavailable(targetTemplateID); ok {
			d.notices.Set(reason)
			d.mode = ModeNew{wizard}
			return DashboardActionNone{}
		}
		target, ok := config.Targets[targetTemplateID]
		if !ok {
			panic("selected target index is present in config")
		}
		if targetIsSized(target) && wizard.resourceAllocation == nil {
			notice := "Resource sizing is not ready."
			if wizard.sizingError != nil {
				notice = *wizard.sizingError
			}
			d.notices.Set(notice)
			d.mode = ModeNew{wizard}
			return DashboardActionNone{}
		}
		_, localBare := target.(core.TargetTemplateLocalBare)
		if goMode := d.goMode; goMode != nil {
			if !core.IsBareProjectTarget(target) {
				wizard.step = WizardStepBundle
				wizard.bundleCreationInFlight = true
				d.mode = ModeNew{wizard}
				return DashboardActionGoPrepareProject{TargetID: targetTemplateID}
			}
			if localBare {
				wizard.projectDirectory = chat.PathInputFromValue(goMode.Directory)
			} else {
				directory := ""
				if recipe := goMode.Recipe; recipe != nil && recipe.TargetID == targetTemplateID && recipe.ProjectDirectory != nil {
					directory = *recipe.ProjectDirectory
				}
				wizard.projectDirectory = chat.PathInputFromValue(directory)
			}
		}
		state := d.state.Get()
		if core.IsBareProjectTarget(target) {
			wizard.mounts.history = nil
			wizard.projectHistory = nil
			if host, ok := core.ProjectHistoryHost(target); ok {
				wizard.projectHistory = slices.Clone(state.ProjectDirectories(host))
			}
			wizard.projectHistoryIndex = 0
			// A local project starts as the repository `mj` was started in;
			// otherwise the most recent project.
			var launch *string
			if localBare {
				launch = d.launchProjectDirectory
			}
			var recent *string
			if d.recentLaunch != nil && d.recentLaunch.TargetID == targetTemplateID {
				recent = d.recentLaunch.ProjectDirectory
			}
			if d.goMode == nil && wizard.projectDirectory.IsEmpty() {
				directory := launch
				if directory == nil {
					directory = recent
				}
				if directory == nil && len(wizard.projectHistory) > 0 {
					directory = &wizard.projectHistory[0]
				}
				if directory != nil {
					wizard.projectDirectory = chat.PathInputFromValue(*directory)
				}
			}
			wizard.step = WizardStepProjectDirectory
		} else {
			wizard.mounts.historySelection = 0
			wizard.mounts.history = nil
			if host, ok := core.MountHistoryHost(target); ok {
				if history, ok := state.MountHistory.Get(host); ok {
					wizard.mounts.history = slices.Clone(history)
				}
			}
			wizard.step = WizardStepBundle
		}
		if wizard.step == WizardStepBundle && len(config.Bundles) == 0 {
			wizard.openProjects(d)
		} else {
			wizard.form.Focus(stepInitial(wizard.step))
		}
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	case WizardStepReview, WizardStepLaunching:
		panic("review input is handled before picker navigation")
	case WizardStepMounts:
		panic("mount input is handled before picker navigation")
	case WizardStepNewBundle:
		panic("bundle input is handled above")
	default:
		panic("project directory input is handled above")
	}
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
	config := d.config.Get()
	targetTemplateID := nthKey(config.Targets, wizard.target)
	if !core.IsBareProjectTarget(config.Targets[targetTemplateID]) {
		if wizard.remoteRepositories != nil && wizard.remotePreflightError == nil {
			return d.createSessionAction(wizard)
		}
		wizard.remotePreflightError = nil
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	if _, ok := wizard.selectedWorktreeOptions(config); !ok {
		wizard.remotePreflightError = nil
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	if len(wizard.mounts.mounts) == 0 {
		return d.createSessionAction(wizard)
	}
	action := DashboardActionValidateSessionMounts{
		TargetTemplateID: targetTemplateID,
		Mounts:           slices.Clone(wizard.mounts.mounts),
		Launch:           d.createSessionActionWithoutClosing(wizard),
	}
	d.mode = ModeNew{wizard}
	return action
}

// TakePrerequisiteCheck starts the prerequisite check an isolated creation
// review needs before Create is enabled: attached directories first, then
// network sources. The dashboard loop asks after every event and background
// update, so a review never waits on a check that nothing started, whichever
// path opened it.
func (d *DashboardState) TakePrerequisiteCheck() (DashboardAction, bool) {
	if action := d.takeProfileHydration(); action != nil {
		return action, true
	}
	if action := d.takeSubagentDiscovery(); action != nil {
		return action, true
	}
	if action := d.takeSetupSubagentChoices(); action != nil {
		return action, true
	}
	profileStep := false
	switch mode := d.mode.(type) {
	case ModeNew:
		profileStep = mode.step == WizardStepProfile
	case ModeResume:
		profileStep = mode.step == WizardStepProfile
	}
	if profileStep {
		if action := d.skipInitialProfile(); !wizardActionIsNone(action) {
			return action, true
		}
	}
	if action, ok := d.takeStaleMovePreparation(); ok {
		return action, true
	}
	// A sole raw target whose readiness just arrived needs no selector.
	skipTarget := false
	switch mode := d.mode.(type) {
	case ModeNew:
		_, lone := d.loneTarget(mode.NewWizard)
		skipTarget = mode.step == WizardStepTarget && lone
	case ModeResume:
		_, lone := d.loneTarget(mode.ResumeWizard)
		skipTarget = mode.step == WizardStepTarget && lone
	}
	if skipTarget {
		var action DashboardAction
		switch mode := d.mode.(type) {
		case ModeNew:
			d.mode = ModeDashboard{}
			d.skipTargetStep(mode.NewWizard)
			action = d.advanceNewWizard(mode.NewWizard)
		case ModeResume:
			d.mode = ModeDashboard{}
			d.skipTargetStep(mode.ResumeWizard)
			action = d.advanceResumeWizard(mode.ResumeWizard)
		}
		if !wizardActionIsNone(action) {
			return action, true
		}
	}
	config := d.config.Get()
	autoSubmit := false
	switch mode := d.mode.(type) {
	case ModeNew:
		_, options := mode.selectedWorktreeOptions(config)
		autoSubmit = mode.step == WizardStepLaunching &&
			options &&
			!mode.remotePreflightInFlight &&
			mode.remotePreflightError == nil
	case ModeResume:
		autoSubmit = mode.step == WizardStepLaunching &&
			mode.moving &&
			mode.preparation != nil &&
			!mode.preparing &&
			mode.preparationError == nil &&
			!mode.form.SubmissionPending()
	}
	if autoSubmit {
		var action DashboardAction
		switch mode := d.mode.(type) {
		case ModeNew:
			d.mode = ModeDashboard{}
			action = d.preflightCreateSessionAction(mode.NewWizard)
		case ModeResume:
			d.mode = ModeDashboard{}
			profile := mode.destinationProfile(d)
			action = d.preflightResumeSessionAction(mode.ResumeWizard, profile)
		}
		if wizardActionIsNone(action) {
			return nil, false
		}
		return action, true
	}
	// The project step marks configured projects whose directory is gone.
	// The checks start with the first step, like the target checks.
	if mode, ok := d.mode.(ModeNew); ok &&
		(mode.step == WizardStepProfile || mode.step == WizardStepTarget || mode.step == WizardStepBundle) {
		if check, ok := d.beginProjectDirectoryChecks(); ok {
			return check, true
		}
	}
	mode, ok := d.mode.(ModeNew)
	if !ok {
		return nil, false
	}
	wizard := mode.NewWizard
	if (wizard.step != WizardStepReview && wizard.step != WizardStepLaunching) ||
		wizard.remotePreflightInFlight ||
		wizard.remoteRepositories != nil ||
		wizard.remotePreflightError != nil {
		return nil, false
	}
	targetTemplateID := nthKey(config.Targets, wizard.target)
	if core.IsBareProjectTarget(config.Targets[targetTemplateID]) {
		if _, ok := wizard.selectedWorktreeOptions(config); ok {
			return nil, false
		}
		directory := strings.TrimSpace(wizard.projectDirectory.Value())
		wizard.remotePreflightInFlight = true
		return DashboardActionValidateProjectDirectory{
			TargetTemplateID: targetTemplateID,
			Directory:        directory,
		}, true
	}
	launch := d.createSessionActionWithoutClosing(wizard)
	var check DashboardAction
	if len(wizard.mounts.mounts) == 0 {
		check = DashboardActionPreflightCreateSession{Launch: launch}
	} else {
		check = DashboardActionValidateSessionMounts{
			TargetTemplateID: targetTemplateID,
			Mounts:           slices.Clone(wizard.mounts.mounts),
			Launch:           launch,
		}
	}
	wizard.remotePreflightInFlight = true
	return check, true
}

func (d *DashboardState) createSessionActionWithoutClosing(wizard *NewWizard) DashboardAction {
	config := d.config.Get()
	targetTemplateID := nthKey(config.Targets, wizard.target)
	rawProject := core.IsBareProjectTarget(config.Targets[targetTemplateID])
	options, hasOptions := wizard.selectedWorktreeOptions(config)
	createManagedWorktree := rawProject && wizard.createManagedWorktree && hasOptions && options.Available
	var oracle *bool
	if wizard.subagents.oracleAvailable() {
		enabled := wizard.subagents.oracle
		oracle = &enabled
	}
	// A profile that vanished after the launch was submitted is refused by
	// name by the controller.
	profileID, _ := wizard.selectedProfileID(d)
	var provider *core.QuotaProvider
	if selected := wizard.selectedProvider(d); selected != nil {
		provider = &selected
	}
	var bundleID string
	var projectDirectory *string
	var mounts []core.AdditionalMount
	if rawProject {
		bundleID = core.RawProjectContextID(wizard.projectDirectory.Value())
		directory := strings.TrimSpace(wizard.projectDirectory.Value())
		projectDirectory = &directory
	} else {
		bundleID = nthBundleKey(config, d.launchRecency, wizard.bundle)
		mounts = slices.Clone(wizard.mounts.mounts)
	}
	return DashboardActionCreateSession{
		RememberContainerSize: wizard.rememberContainerSize,
		Subagents:             wizard.subagents.createSelection(),
		Oracle:                oracle,
		OracleChoice:          wizardClonedChoice(wizard.subagents.oracleChoice),
		CreateManagedWorktree: &createManagedWorktree,
		WorkspaceID:           wizard.workspaceID,
		ProfileID:             profileID,
		Provider:              provider,
		BundleID:              bundleID,
		ProjectDirectory:      projectDirectory,
		TargetTemplateID:      targetTemplateID,
		AdditionalMounts:      mounts,
		ResourceAllocation:    wizard.resourceAllocation,
	}
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
	record := d.state.Get().Sessions.GetShared(sourceID)
	if record == nil {
		d.notices.Set("This session is no longer available.")
		return DashboardActionNone{}
	}
	source := *record
	d.wantAvailabilityCheck()
	// Like every launch that skips the new-session key, this one waits for
	// the host's Git identity check; the wizard fallback goes through the
	// same gate the new-session key uses.
	action, err := d.newSessionLikeRequest(&source)
	if err == nil {
		return d.GateLaunchOnGitIdentity(action)
	}
	reason := err.Error()
	if held := d.gitIdentityGateAction(); held != nil {
		if d.gitIdentityGate.Kind == GitIdentityGateChecking {
			d.SetNotice(fmt.Sprintf(
				"%s Checking the Git identity on this host before choosing new settings…",
				reason,
			))
		}
		return held
	}
	action = d.beginNewWizard()
	// With no profile or runtime to offer, the wizard has already said why
	// in its own notice.
	if _, ok := d.mode.(ModeNew); ok {
		d.notices.Set(fmt.Sprintf("%s Choose the settings for the new session.", reason))
	}
	return action
}

// newSessionLikeRequest is the creation request for beginNewSessionLike, or
// why the source's runtime or project cannot be used (the error's text).
func (d *DashboardState) newSessionLikeRequest(source *core.SessionRecord) (DashboardAction, error) {
	config := d.config.Get()
	targetTemplateID := source.TargetTemplateID
	if reason, ok := d.targetUnavailable(targetTemplateID); ok {
		return nil, fmt.Errorf("Runtime %s cannot be used: %s", targetTemplateID, reason)
	}
	rawProject := core.IsBareProjectTarget(config.Targets[targetTemplateID])
	// The checkout, not the record's path, names the project: a managed
	// worktree's own path is the source's, and a sub-agent works in its
	// owner's checkout.
	checkout, err := d.state.Get().Checkout(source.ID)
	if err != nil {
		return nil, fmt.Errorf("The session's project is unknown: %s", err)
	}
	var directory *string
	sourceHadWorktree := false
	switch effective := checkout.Effective().(type) {
	case core.CheckoutManagedWorktree:
		path := effective.Worktree.SourceProjectDirectory
		directory = &path
		sourceHadWorktree = true
	case core.CheckoutAttached:
		path := effective.Path
		directory = &path
	}
	if (directory != nil) != rawProject {
		return nil, fmt.Errorf("The session's project does not fit runtime %s.", targetTemplateID)
	}
	var bundleID string
	if directory != nil {
		bundleID = core.RawProjectContextID(*directory)
	} else {
		if _, ok := config.Bundles[source.BundleID]; !ok {
			return nil, fmt.Errorf("Project %s is no longer configured.", source.BundleID)
		}
		bundleID = source.BundleID
	}
	var changes []string
	profileID := source.LastProfile
	if !d.profileOffered(source.LastProfile) {
		var reason string
		if _, enabled := config.EnabledProfile(source.LastProfile); !enabled {
			reason = "it is disabled or no longer configured"
		} else if missing, ok := d.profileMissing(source.LastProfile); ok {
			reason = missing
		} else {
			reason = "it is not available"
		}
		replacement, ok := d.nthOfferedProfile(d.defaultNewProfileIndex())
		if !ok {
			return nil, errors.New("No profile is available here.")
		}
		changes = append(changes, fmt.Sprintf(
			"Profile %s is not available (%s), so this session uses %s.",
			source.LastProfile,
			strings.TrimRight(reason, "."),
			replacement,
		))
		profileID = replacement
	}
	workspaceID := source.WorkspaceID
	if !slices.Contains(d.workspaceIDs(), source.WorkspaceID) {
		active := ""
		if current := d.activeWorkspaceID.Get(); *current != nil {
			active = **current
		}
		changes = append(changes, fmt.Sprintf(
			"The session's workspace no longer exists, so this session starts in %s.",
			d.workspaceDisplayName(active),
		))
		workspaceID = active
	}
	// A sub-agent's own delegation, oracle and review are fixed by its
	// parent's launch; the session that owns it holds the real choices.
	chooser := source
	if owner := checkout.Owner(); owner != nil {
		chooser = owner
	}
	var policy core.SubagentPolicy
	if chooser.Subagents != nil {
		policy = *chooser.Subagents
	}
	delegation := newSubagentDraft(policy).withOracle(chooser.Oracle, wizardClonedChoice(chooser.OracleChoice))
	delegation.refreshCreate(profileID, config, &policy, chooser.Oracle)
	// The launch starts at once, so a choice the refresh could not keep is
	// reported, like the profile and workspace above. Native shown as None
	// for a harness without sub-agents of its own is not a change.
	keptPolicy := delegation.shown(&policy)
	if selected := delegation.selected(); !wizardSubagentPolicyEqual(selected, keptPolicy) {
		reason := ""
		if unavailable, ok := delegation.unavailableReason(); ok {
			reason = fmt.Sprintf(" (%s)", strings.TrimRight(unavailable, "."))
		}
		changes = append(changes, fmt.Sprintf(
			"Sub-agents: %s is not available%s, so this session uses %s.",
			delegation.policyLabel(&policy),
			reason,
			delegation.policyLabel(&selected),
		))
	}
	if chooser.Oracle && !delegation.oracleAvailable() {
		changes = append(changes,
			"Oracle is not available (no oracle model is configured), so this session "+
				"starts without it.")
	}
	selected := delegation.selected()
	var oracle *bool
	if delegation.oracleAvailable() {
		enabled := delegation.oracle
		oracle = &enabled
	}
	var review *core.SessionReview
	if chooser.Review != nil {
		copied := *chooser.Review
		review = &copied
	}
	// Like the wizard, ask for a worktree only where one can exist, and ask
	// for a new one rather than reusing the source's.
	createManagedWorktree := rawProject && sourceHadWorktree
	var mounts []core.AdditionalMount
	if !rawProject {
		mounts = slices.Clone(source.AdditionalMounts)
	}
	var note *string
	if len(changes) > 0 {
		joined := strings.Join(changes, " ")
		note = &joined
	}
	return DashboardActionCreateSession{
		RememberContainerSize: false,
		Subagents:             &selected,
		Oracle:                oracle,
		OracleChoice:          wizardClonedChoice(delegation.oracleChoice),
		Review:                review,
		CreateManagedWorktree: &createManagedWorktree,
		WorkspaceID:           workspaceID,
		ProfileID:             profileID,
		BundleID:              bundleID,
		ProjectDirectory:      directory,
		TargetTemplateID:      targetTemplateID,
		AdditionalMounts:      mounts,
		// A sub-agent runs in its owner's container, whose size lives on the
		// owner's record.
		ResourceAllocation: chooser.ResourceAllocation,
		Carried: CarriedLaunchSettings{
			NoProjectMemory:     source.NoProjectMemory,
			NoGithubEvents:      source.NoGithubEvents,
			NoGithubCredentials: source.NoGithubCredentials,
			Note:                note,
		},
	}, nil
}

func (d *DashboardState) ApplyCreatedBundle(config core.Config, bundleID string) DashboardAction {
	d.config.Set(config)
	mode, ok := d.mode.(ModeNew)
	if !ok {
		return DashboardActionNone{}
	}
	wizard := mode.NewWizard
	if !wizard.bundleCreationInFlight {
		return DashboardActionNone{}
	}
	wizard.bundleCreationInFlight = false
	index := slices.Index(bundleIDsByRecentCreation(d.config.Get(), d.launchRecency), bundleID)
	if index < 0 {
		d.notices.Set(fmt.Sprintf("Selected project %q was not found.", bundleID))
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	d.invalidateNewRemotePreflight(wizard)
	wizard.bundle = index
	wizard.step = WizardStepReview
	d.notices.Set(fmt.Sprintf("Selected project %s.", bundleID))
	d.mode = ModeNew{wizard}
	return DashboardActionNone{}
}

// beginBundleRemoval starts removing the selected saved project, or explains
// why a session still needs it. The config write re-checks against fresh
// state.
func (d *DashboardState) beginBundleRemoval(wizard *NewWizard) DashboardAction {
	wizard.bundleRemovalOutcome = nil
	ids := bundleIDsByRecentCreation(d.config.Get(), d.launchRecency)
	if wizard.bundle < 0 || wizard.bundle >= len(ids) {
		return d.keep(wizard)
	}
	bundleID := ids[wizard.bundle]
	if refusal, ok := d.state.Get().BundleRemovalRefusal(bundleID); ok {
		wizard.bundleRemovalOutcome = &refusal
		d.notices.Set(refusal)
		return d.keep(wizard)
	}
	wizard.bundleRemovalInFlight = true
	d.notices.Set(fmt.Sprintf("Removing project %s…", bundleID))
	d.mode = ModeNew{wizard}
	return DashboardActionRemoveBundle{BundleID: bundleID}
}

// ApplyRemovedBundle installs the config without the removed project and
// keeps the selection on a row that still exists.
func (d *DashboardState) ApplyRemovedBundle(config core.Config, bundleID string) {
	d.config.Set(config)
	if mode, ok := d.mode.(ModeNew); ok && mode.bundleRemovalInFlight {
		wizard := mode.NewWizard
		wizard.bundleRemovalInFlight = false
		wizard.bundleRemovalOutcome = nil
		bundles := len(d.config.Get().Bundles)
		wizard.bundle = min(wizard.bundle, max(bundles-1, 0))
		d.invalidateNewRemotePreflight(wizard)
		if bundles == 0 {
			wizard.form.Focus(WizardControlAdd)
		}
		d.mode = ModeNew{wizard}
	}
	d.notices.Set(fmt.Sprintf("Removed project %s.", bundleID))
}

func (d *DashboardState) FailBundleRemoval(reason string) {
	if mode, ok := d.mode.(ModeNew); ok && mode.bundleRemovalInFlight {
		mode.bundleRemovalInFlight = false
		outcome := fmt.Sprintf("Could not remove project: %s", reason)
		mode.bundleRemovalOutcome = &outcome
	}
	d.notices.Set(fmt.Sprintf("Could not remove project: %s", reason))
}

// FailBundleCreation reopens the new-bundle editor after its asynchronous
// create failed. The draft remains untouched so the user can correct and
// retry it.
func (d *DashboardState) FailBundleCreation(reason string) {
	if mode, ok := d.mode.(ModeNew); ok && mode.bundleCreationInFlight {
		mode.bundleCreationInFlight = false
		message := reason
		mode.projectPicker.creationError = &message
	}
	d.notices.Set(fmt.Sprintf("Could not use project: %s", reason))
}

// wizardSubagentPolicyEqual is SubagentPolicy's derived PartialEq, where the
// zero mode and native are the same default policy.
func wizardSubagentPolicyEqual(left, right core.SubagentPolicy) bool {
	if left.Mode == "" {
		left.Mode = core.SubagentPolicyNative
	}
	if right.Mode == "" {
		right.Mode = core.SubagentPolicyNative
	}
	return reflect.DeepEqual(left, right)
}
