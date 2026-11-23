// Port of mj-tui/src/wizards/draft.rs.

// One draft behind the two session wizards.
//
// NewWizard (create a session) and ResumeWizard (resume or move one) share
// a step machine, a control set, a chat.Dialog[WizardControl] form, and an
// attachment editor. WizardDraft names the parts they share plus the hooks
// where they genuinely differ, so wizards_dashboard.go can hold one body per
// behaviour instead of a New copy and a Resume copy.
//
// Both drafts implement WizardDraft through pointer receivers. A Rust method
// that consumes the draft (`self`) takes the pointer here, and so do the
// DashboardState functions it hands the draft to; one that hands the draft
// back (`Err(self)`) reports false instead.
//
// The interface's methods are exported because the drafts have fields of
// the same names (step, profile, form, mounts, ...), which Go does not allow
// beside unexported methods.

package tui

import (
	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DraftChange is a draft edit that may invalidate an answer a background
// task already owes the wizard. A new-session draft invalidates its remote
// creation preflight; a move draft invalidates its move preparation. Each
// wizard decides which changes matter to it in NoteDraftChange.
type DraftChange int

const (
	// DraftChangeFieldEdit: a text field in the draft was edited.
	DraftChangeFieldEdit DraftChange = iota
	// DraftChangeProfileSelected: a different profile row was selected.
	DraftChangeProfileSelected
	// DraftChangeBundleSelected: a different bundle row was selected.
	// Resume has no bundle step.
	DraftChangeBundleSelected
	// DraftChangeTargetSelected: a different target row was selected.
	DraftChangeTargetSelected
	// DraftChangeReadOnlyToggled: the attachment editor's access mode was
	// committed.
	DraftChangeReadOnlyToggled
	// DraftChangeResourcesAdjusted: the container or instance size was
	// changed from the target step.
	DraftChangeResourcesAdjusted
	// DraftChangeAttachmentOpened: an already attached directory was opened
	// for editing from the review.
	DraftChangeAttachmentOpened
	// DraftChangeAttachmentEditorOpened: a fresh attachment editor was
	// opened from the review's Add button.
	DraftChangeAttachmentEditorOpened
	// DraftChangeAttachmentRemoved: an attached directory was removed from
	// the review.
	DraftChangeAttachmentRemoved
	// DraftChangeReviewLeft: the review step was left through Back.
	DraftChangeReviewLeft
)

// WizardDraft is the shared shape of the new-session and resume/move
// wizards.
type WizardDraft interface {
	CompletesPaths

	Step() WizardStep
	SetStep(step WizardStep)
	Profile() int
	SetProfile(index int)
	Target() int
	SetTarget(index int)
	LaunchError() (string, bool)
	ProfileStepSkipped() bool
	SetProfileStepSkipped(skipped bool)
	SkipsReview() bool
	HasBack() bool
	TargetStepSkipped() bool
	SetTargetStepSkipped(skipped bool)
	Mounts() *MountWizard
	MountsMut() *MountWizard
	Form() *chat.Dialog[WizardControl]
	FormMut() *chat.Dialog[WizardControl]
	// ResourceAllocation is nil for fixed or default resources.
	ResourceAllocation() core.SessionResourceAllocation
	ResourceEditor() *ResourceEditor
	ResourceEditorMut() *ResourceEditor
	// RememberContainerSize is the explicit save-as-default checkbox, which
	// only exists on new sessions.
	RememberContainerSize() (bool, bool)
	// SizingMut is the allocation and its validation error, which are
	// edited together.
	SizingMut() (*core.SessionResourceAllocation, **string)
	// IntoMode puts the draft back into the mode that owns it.
	IntoMode() Mode
	// PreviousAllocation is the size this draft starts from when a target is
	// selected: the session's stored allocation for a resume, nothing for a
	// new session.
	PreviousAllocation(dashboard *DashboardState) core.SessionResourceAllocation
	// ReenterReview puts the draft back on the review step. A move
	// re-requests its preparation, because the draft it was prepared for has
	// changed.
	ReenterReview(dashboard *DashboardState) DashboardAction
	ReviewHarness(dashboard *DashboardState) (core.HarnessKind, bool)
	// ReviewBackStep is the step Back leaves the review for.
	ReviewBackStep(dashboard *DashboardState) WizardStep
	// NoteDraftChange records a draft edit, cancelling whatever background
	// answer it makes stale.
	NoteDraftChange(dashboard *DashboardState, change DraftChange)
	// SubmitReview acts on the review's primary button.
	SubmitReview(dashboard *DashboardState) DashboardAction
	// ActivateExtra handles a control that only one wizard has, before the
	// shared Back and Next arms run. False hands the draft back for those
	// arms.
	ActivateExtra(dashboard *DashboardState, id WizardControl) (DashboardAction, bool)
	// Advance moves the draft to its next step.
	Advance(dashboard *DashboardState) DashboardAction
	// ApplyExtraFieldEdit edits a text field that only one wizard has. False
	// hands the edit on to the shared attachment-editor fields.
	ApplyExtraFieldEdit(dashboard *DashboardState, id WizardControl, edit chat.FieldEdit) bool
	// PreparesTargetOnSelect reports whether selecting a target should
	// recompute the size at once. Creation always does; a resume only does
	// for a target the session can use.
	PreparesTargetOnSelect(dashboard *DashboardState) bool
	// RouteExtraInteraction may take or rewrite an interaction first. nil
	// is no interaction.
	RouteExtraInteraction(interaction chat.Interaction[WizardControl]) chat.Interaction[WizardControl]
	// ApplyExtraInteraction handles an interaction with a control that only
	// one wizard has.
	ApplyExtraInteraction(dashboard *DashboardState, interaction chat.Interaction[WizardControl])
	// HandleExtraShortcut handles a bare key that only one wizard answers.
	HandleExtraShortcut(dashboard *DashboardState, key tea.KeyMsg)
	// ProfileCount is how many rows the profile picker shows.
	ProfileCount(dashboard *DashboardState) int
	ReconcileProfilePicker(dashboard *DashboardState)
	ProfileListLen(dashboard *DashboardState) int
	ProfileListSelected(dashboard *DashboardState) int
	ProfileAdvanceEnabled(dashboard *DashboardState) bool
	SelectProfileRow(dashboard *DashboardState, row int, toggleProvider bool)
	// TargetRejection is why this draft cannot use targetID, or false when
	// it can.
	TargetRejection(dashboard *DashboardState, targetID string) (string, bool)
	// TargetCompatible reports whether this draft could use targetID once
	// the target is ready. Unlike TargetRejection, this leaves availability
	// out.
	TargetCompatible(dashboard *DashboardState, targetID string) bool
	// DeclareExtraStep declares the controls of a step only one wizard has.
	DeclareExtraStep(dashboard *DashboardState, form *chat.Dialog[WizardControl])
	// DeclareReviewExtras declares the review controls only one wizard has
	// and answers whether its primary button is enabled.
	DeclareReviewExtras(dashboard *DashboardState, form *chat.Dialog[WizardControl]) bool
	// InputLocked reports whether the wizard is refusing input while a
	// background job runs.
	InputLocked() bool
	// HandleStepEvent handles an event on a step only one wizard has. It
	// answers true when the step took responsibility for it.
	HandleStepEvent(dashboard *DashboardState, event tea.Msg) bool
	// DismissUnfocusedExtraCompletions closes the popup of a path field only
	// one wizard has when the keyboard has moved elsewhere. focused is nil
	// when no control has focus.
	DismissUnfocusedExtraCompletions(focused *WizardControl)
	// FocusedExtraPathInput is the focused path field of a step only one
	// wizard has, with the host that owns its path; a nil input means none.
	// The shared mount source is handled for both.
	FocusedExtraPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind)
}

// draftSkipsReview is WizardDraft's default skips_review.
func draftSkipsReview(wizard WizardDraft) bool {
	return wizard.ProfileStepSkipped() && wizard.TargetStepSkipped()
}

// draftHasBack is WizardDraft's default has_back.
func draftHasBack(wizard WizardDraft) bool {
	switch wizard.Step() {
	case WizardStepProfile, WizardStepLaunching:
		return false
	case WizardStepTarget:
		return !wizard.ProfileStepSkipped()
	case WizardStepBundle, WizardStepProjectDirectory:
		return !wizard.SkipsReview()
	default:
		return true
	}
}

func declareSandboxOffer(dashboard *DashboardState, wizard WizardDraft, form *chat.Dialog[WizardControl]) {
	panic("unported: mj-tui/src/wizards/draft.rs declare_sandbox_offer")
}

func declareReviewAttachments(form *chat.Dialog[WizardControl], mounts *MountWizard) {
	if len(mounts.mounts) > 0 {
		form.DeclareWithEnabled(
			WizardControlReviewAttachments,
			chat.ControlKindChoiceList{Len: len(mounts.mounts), Selected: mounts.selectedMount},
			true,
		)
	}
}

// targetAdvanceEnabled reports whether the target step's Next is enabled:
// the draft must be able to use the selected target, and a sized target
// must have a valid allocation.
func targetAdvanceEnabled(dashboard *DashboardState, wizard WizardDraft) bool {
	panic("unported: mj-tui/src/wizards/draft.rs target_advance_enabled")
}

func (w *NewWizard) Step() WizardStep                   { return w.step }
func (w *NewWizard) SetStep(step WizardStep)            { w.step = step }
func (w *NewWizard) Profile() int                       { return w.profile }
func (w *NewWizard) SetProfile(index int)               { w.profile = index }
func (w *NewWizard) Target() int                        { return w.target }
func (w *NewWizard) SetTarget(index int)                { w.target = index }
func (w *NewWizard) ProfileStepSkipped() bool           { return w.profileStepSkipped }
func (w *NewWizard) SetProfileStepSkipped(skipped bool) { w.profileStepSkipped = skipped }
func (w *NewWizard) SkipsReview() bool                  { return draftSkipsReview(w) }
func (w *NewWizard) HasBack() bool                      { return draftHasBack(w) }
func (w *NewWizard) TargetStepSkipped() bool            { return w.targetStepSkipped }
func (w *NewWizard) SetTargetStepSkipped(skipped bool)  { w.targetStepSkipped = skipped }
func (w *NewWizard) Mounts() *MountWizard               { return &w.mounts }
func (w *NewWizard) MountsMut() *MountWizard            { return &w.mounts }
func (w *NewWizard) Form() *chat.Dialog[WizardControl]  { return &w.form }
func (w *NewWizard) FormMut() *chat.Dialog[WizardControl] {
	return &w.form
}
func (w *NewWizard) ResourceAllocation() core.SessionResourceAllocation {
	return w.resourceAllocation
}
func (w *NewWizard) ResourceEditor() *ResourceEditor    { return &w.resourceEditor }
func (w *NewWizard) ResourceEditorMut() *ResourceEditor { return &w.resourceEditor }
func (w *NewWizard) RememberContainerSize() (bool, bool) {
	return w.rememberContainerSize, true
}

func (w *NewWizard) LaunchError() (string, bool) {
	if w.remotePreflightError != nil {
		return *w.remotePreflightError, true
	}
	return "", false
}

func (w *NewWizard) SizingMut() (*core.SessionResourceAllocation, **string) {
	return &w.resourceAllocation, &w.sizingError
}

func (w *NewWizard) IntoMode() Mode {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::into_mode")
}

// PreviousAllocation: a new session has no earlier size to restore.
func (w *NewWizard) PreviousAllocation(dashboard *DashboardState) core.SessionResourceAllocation {
	return nil
}

// ReenterReview: creation asks nothing of the daemon before Create, so
// returning to the review only puts the draft back.
func (w *NewWizard) ReenterReview(dashboard *DashboardState) DashboardAction {
	if w.SkipsReview() {
		w.step = WizardStepLaunching
	}
	return dashboard.keep(w)
}

func (w *NewWizard) ReviewHarness(dashboard *DashboardState) (core.HarnessKind, bool) {
	id, ok := w.selectedProfileID(dashboard)
	if !ok || id == "" {
		return "", false
	}
	profile, ok := dashboard.config.Get().Profiles[id]
	if !ok {
		return "", false
	}
	return profile.Kind, true
}

// ReviewBackStep: a bare project target reviews a directory; every other
// target reviews a bundle.
func (w *NewWizard) ReviewBackStep(dashboard *DashboardState) WizardStep {
	targets := dashboard.config.Get().Targets
	if core.IsBareProjectTarget(targets[nthKey(targets, w.target)]) {
		return WizardStepProjectDirectory
	}
	return WizardStepBundle
}

// NoteDraftChange: anything that changes what would be created invalidates
// the remote creation preflight. Removing an attachment, opening a fresh
// attachment editor and editing an unrelated field do not, because none of
// them changes the sources the preflight resolved.
func (w *NewWizard) NoteDraftChange(dashboard *DashboardState, change DraftChange) {
	if change == DraftChangeBundleSelected {
		w.bundleRemovalOutcome = nil
	}
	switch change {
	case DraftChangeBundleSelected, DraftChangeResourcesAdjusted, DraftChangeTargetSelected,
		DraftChangeAttachmentOpened, DraftChangeReviewLeft:
		dashboard.invalidateNewRemotePreflight(w)
	}
}

func (w *NewWizard) SubmitReview(dashboard *DashboardState) DashboardAction {
	return dashboard.preflightCreateSessionAction(w)
}

// ActivateExtra: the bundle creator and the project directory belong to
// creation alone. Back is handed back so the shared step machine runs it.
func (w *NewWizard) ActivateExtra(dashboard *DashboardState, id WizardControl) (DashboardAction, bool) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::activate_extra")
}

func (w *NewWizard) Advance(dashboard *DashboardState) DashboardAction {
	return dashboard.advanceNewWizard(w)
}

// ApplyExtraFieldEdit: the project directory and the bundle creator's
// source belong to creation alone. Up and Down on the project directory walk
// the host's remembered directories.
func (w *NewWizard) ApplyExtraFieldEdit(dashboard *DashboardState, id WizardControl, edit chat.FieldEdit) bool {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::apply_extra_field_edit")
}

// PreparesTargetOnSelect: every target is offered, and the size is prepared
// for whichever one is picked; an unusable target is refused later, on
// Next.
func (w *NewWizard) PreparesTargetOnSelect(dashboard *DashboardState) bool {
	return true
}

func (w *NewWizard) RouteExtraInteraction(interaction chat.Interaction[WizardControl]) chat.Interaction[WizardControl] {
	return w.subagents.combo.Route(interaction)
}

func (w *NewWizard) ApplyExtraInteraction(dashboard *DashboardState, interaction chat.Interaction[WizardControl]) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::apply_extra_interaction")
}

func (w *NewWizard) HandleExtraShortcut(dashboard *DashboardState, key tea.KeyMsg) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::handle_extra_shortcut")
}

func (w *NewWizard) ProfileCount(dashboard *DashboardState) int {
	return dashboard.newWizardProfileCount()
}

func (w *NewWizard) ReconcileProfilePicker(dashboard *DashboardState) {
	if w.step == WizardStepProfile {
		w.focusedProvider = w.effectiveFocusedProvider(dashboard)
		w.expandedProviders = w.effectiveExpandedProviders(dashboard)
	}
}

func (w *NewWizard) ProfileListLen(dashboard *DashboardState) int {
	return len(newWizardProfileItems(dashboard, w.effectiveExpandedProviders(dashboard)))
}

func (w *NewWizard) ProfileListSelected(dashboard *DashboardState) int {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::profile_list_selected")
}

func (w *NewWizard) ProfileAdvanceEnabled(dashboard *DashboardState) bool {
	provider := w.effectiveFocusedProvider(dashboard)
	return provider == nil || !newWizardIsResolvingProvider(provider)
}

func (w *NewWizard) SelectProfileRow(dashboard *DashboardState, row int, toggleProvider bool) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::select_profile_row")
}

// TargetRejection: a new session can start on any offered target.
func (w *NewWizard) TargetRejection(dashboard *DashboardState, targetID string) (string, bool) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::target_rejection")
}

// TargetCompatible: a new session can start on any target.
func (w *NewWizard) TargetCompatible(dashboard *DashboardState, targetID string) bool {
	return true
}

func (w *NewWizard) DismissUnfocusedExtraCompletions(focused *WizardControl) {
	if focused == nil || *focused != WizardControlProjectDirectory {
		w.projectDirectory.DismissCompletion()
	}
	if focused == nil || *focused != WizardControlNewBundleSource {
		w.newBundleSource.DismissCompletion()
	}
}

// FocusedExtraPathInput: the project directory lives on the target's
// machine; a bundle source is a controller path only when it is a path at
// all.
func (w *NewWizard) FocusedExtraPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::focused_extra_path_input")
}

func (w *NewWizard) DeclareExtraStep(dashboard *DashboardState, form *chat.Dialog[WizardControl]) {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::declare_extra_step")
}

// InputLocked: creating a bundle rewrites the config, so the form is frozen
// until the daemon answers.
func (w *NewWizard) InputLocked() bool {
	return w.bundleCreationInFlight || w.bundleRemovalInFlight
}

// HandleStepEvent: the bundle creator and the project directory are all
// text field, so they consume every event the form did not: no shortcut key
// may edit a field while a footer button owns focus.
func (w *NewWizard) HandleStepEvent(dashboard *DashboardState, event tea.Msg) bool {
	if w.step == WizardStepNewBundle {
		if key, ok := event.(tea.KeyMsg); ok && key.Key().Code == tea.KeyDelete {
			if focused, ok := w.form.Focused(); ok && focused == WizardControlNewBundleRepositories {
				w.removeSelectedNewBundleRepository()
			}
		}
		return true
	}
	return w.step == WizardStepProjectDirectory
}

func (w *NewWizard) DeclareReviewExtras(dashboard *DashboardState, form *chat.Dialog[WizardControl]) bool {
	panic("unported: mj-tui/src/wizards/draft.rs NewWizard::declare_review_extras")
}

func (w *ResumeWizard) Step() WizardStep                   { return w.step }
func (w *ResumeWizard) SetStep(step WizardStep)            { w.step = step }
func (w *ResumeWizard) Profile() int                       { return w.profile }
func (w *ResumeWizard) SetProfile(index int)               { w.profile = index }
func (w *ResumeWizard) Target() int                        { return w.target }
func (w *ResumeWizard) SetTarget(index int)                { w.target = index }
func (w *ResumeWizard) ProfileStepSkipped() bool           { return w.profileStepSkipped }
func (w *ResumeWizard) SetProfileStepSkipped(skipped bool) { w.profileStepSkipped = skipped }
func (w *ResumeWizard) HasBack() bool                      { return draftHasBack(w) }
func (w *ResumeWizard) TargetStepSkipped() bool            { return w.targetStepSkipped }
func (w *ResumeWizard) SetTargetStepSkipped(skipped bool)  { w.targetStepSkipped = skipped }
func (w *ResumeWizard) Mounts() *MountWizard               { return &w.mounts }
func (w *ResumeWizard) MountsMut() *MountWizard            { return &w.mounts }
func (w *ResumeWizard) Form() *chat.Dialog[WizardControl]  { return &w.form }
func (w *ResumeWizard) FormMut() *chat.Dialog[WizardControl] {
	return &w.form
}
func (w *ResumeWizard) ResourceAllocation() core.SessionResourceAllocation {
	return w.resourceAllocation
}
func (w *ResumeWizard) ResourceEditor() *ResourceEditor    { return &w.resourceEditor }
func (w *ResumeWizard) ResourceEditorMut() *ResourceEditor { return &w.resourceEditor }

// RememberContainerSize: the save-as-default checkbox only exists on new
// sessions.
func (w *ResumeWizard) RememberContainerSize() (bool, bool) {
	return false, false
}

func (w *ResumeWizard) LaunchError() (string, bool) {
	for _, failure := range []*string{w.preparationError, w.preflightError, w.mounts.error} {
		if failure != nil {
			return *failure, true
		}
	}
	return "", false
}

func (w *ResumeWizard) SkipsReview() bool {
	return !w.moving && w.profileStepSkipped && w.targetStepSkipped
}

func (w *ResumeWizard) SizingMut() (*core.SessionResourceAllocation, **string) {
	return &w.resourceAllocation, &w.sizingError
}

func (w *ResumeWizard) IntoMode() Mode {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::into_mode")
}

// PreviousAllocation: a resume starts from the size the session already ran
// with.
func (w *ResumeWizard) PreviousAllocation(dashboard *DashboardState) core.SessionResourceAllocation {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::previous_allocation")
}

func (w *ResumeWizard) ReenterReview(dashboard *DashboardState) DashboardAction {
	if w.SkipsReview() {
		w.step = WizardStepLaunching
		if !w.moving {
			return dashboard.preflightResumeSessionAction(w, w.destinationProfile(dashboard))
		}
	}
	if !w.moving {
		return dashboard.keep(w)
	}
	return dashboard.requestMovePreparationForReview(w, w.destinationProfile(dashboard))
}

func (w *ResumeWizard) ReviewHarness(dashboard *DashboardState) (core.HarnessKind, bool) {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::review_harness")
}

// ReviewBackStep: resume has no bundle or project step, so Back returns to
// the target picker, or to the profile picker when the target step was
// skipped.
func (w *ResumeWizard) ReviewBackStep(dashboard *DashboardState) WizardStep {
	if w.targetStepSkipped {
		return WizardStepProfile
	}
	return WizardStepTarget
}

// NoteDraftChange: a move preparation describes one exact destination
// draft, so every edit but a bundle choice (which resume has no step for)
// discards it.
func (w *ResumeWizard) NoteDraftChange(dashboard *DashboardState, change DraftChange) {
	switch change {
	case DraftChangeTargetSelected, DraftChangeProfileSelected, DraftChangeResourcesAdjusted:
		w.noteDestinationChange()
	case DraftChangeBundleSelected:
	default:
		invalidateMovePreparation(w)
	}
}

func (w *ResumeWizard) SubmitReview(dashboard *DashboardState) DashboardAction {
	return dashboard.preflightResumeSessionAction(w, w.destinationProfile(dashboard))
}

func (w *ResumeWizard) ActivateExtra(dashboard *DashboardState, id WizardControl) (DashboardAction, bool) {
	if w.step == WizardStepReview && w.subagents.activate(id) {
		return dashboard.keep(w), true
	}
	return nil, false
}

func (w *ResumeWizard) Advance(dashboard *DashboardState) DashboardAction {
	return dashboard.advanceResumeWizard(w)
}

// ApplyExtraFieldEdit: resume edits only the attachment editor's fields.
func (w *ResumeWizard) ApplyExtraFieldEdit(dashboard *DashboardState, id WizardControl, edit chat.FieldEdit) bool {
	return false
}

// PreparesTargetOnSelect: preparing the size for a target this session
// cannot resume on would overwrite the size it already has with one it will
// never use.
func (w *ResumeWizard) PreparesTargetOnSelect(dashboard *DashboardState) bool {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::prepares_target_on_select")
}

func (w *ResumeWizard) RouteExtraInteraction(interaction chat.Interaction[WizardControl]) chat.Interaction[WizardControl] {
	return w.subagents.combo.Route(interaction)
}

func (w *ResumeWizard) ApplyExtraInteraction(dashboard *DashboardState, interaction chat.Interaction[WizardControl]) {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::apply_extra_interaction")
}

// HandleExtraShortcut: `q` on the review toggles whether queued work is
// discarded, but only when there is queued work to decide about.
func (w *ResumeWizard) HandleExtraShortcut(dashboard *DashboardState, key tea.KeyMsg) {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::handle_extra_shortcut")
}

func (w *ResumeWizard) ProfileCount(dashboard *DashboardState) int {
	return len(dashboard.resumeWizardProfiles(w))
}

func (w *ResumeWizard) ReconcileProfilePicker(dashboard *DashboardState) {}

func (w *ResumeWizard) ProfileListLen(dashboard *DashboardState) int {
	return w.ProfileCount(dashboard)
}

func (w *ResumeWizard) ProfileListSelected(dashboard *DashboardState) int {
	return w.profile
}

func (w *ResumeWizard) ProfileAdvanceEnabled(dashboard *DashboardState) bool {
	return true
}

func (w *ResumeWizard) SelectProfileRow(dashboard *DashboardState, row int, toggleProvider bool) {
	w.SetProfile(row)
}

func (w *ResumeWizard) TargetRejection(dashboard *DashboardState, targetID string) (string, bool) {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::target_rejection")
}

func (w *ResumeWizard) TargetCompatible(dashboard *DashboardState, targetID string) bool {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::target_compatible")
}

func (w *ResumeWizard) DeclareExtraStep(dashboard *DashboardState, form *chat.Dialog[WizardControl]) {
	panic("invalid resume wizard step")
}

// InputLocked: a resume never waits on a background job before accepting
// input.
func (w *ResumeWizard) InputLocked() bool {
	return false
}

// HandleStepEvent: resume has no step outside the shared ones.
func (w *ResumeWizard) HandleStepEvent(dashboard *DashboardState, event tea.Msg) bool {
	return false
}

func (w *ResumeWizard) DismissUnfocusedExtraCompletions(focused *WizardControl) {}

func (w *ResumeWizard) FocusedExtraPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind) {
	return nil, nil, 0
}

func (w *ResumeWizard) DeclareReviewExtras(dashboard *DashboardState, form *chat.Dialog[WizardControl]) bool {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::declare_review_extras")
}

func (w *ResumeWizard) noteDestinationChange() {
	invalidateMovePreparation(w)
}

// destinationProfile is the profile this resume or move lands on.
func (w *ResumeWizard) destinationProfile(dashboard *DashboardState) string {
	panic("unported: mj-tui/src/wizards/draft.rs ResumeWizard::destination_profile")
}

// keep puts a wizard draft back on screen unchanged.
func (d *DashboardState) keep(wizard WizardDraft) DashboardAction {
	d.mode = wizard.IntoMode()
	return DashboardActionNone{}
}
