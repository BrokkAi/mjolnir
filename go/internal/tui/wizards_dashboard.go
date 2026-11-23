// Port of mj-tui/src/wizards/dashboard.rs.

// The new-session and resume wizards' event handling on the dashboard: one
// persistent component form per wizard, shared step navigation, and the
// attachment editor both wizards use.
//
// Rust's handlers are generic over `W: WizardDraft` and take the wizard by
// value; Go methods cannot be generic, so they take the WizardDraft
// interface, whose implementations are the wizards' pointers.

package tui

import (
	"maps"
	"slices"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func declareWizardControls(dashboard *DashboardState, wizard WizardDraft) {
	form := wizard.Form()
	form.BeginUpdate()
	form.SetActionRole(WizardControlCancel, chat.ActionRoleCancel)
	form.SetActionRole(WizardControlBack, chat.ActionRoleBack)
	initial := stepInitial(wizard.Step())
	targets := dashboard.config.Get().Targets
	switch wizard.Step() {
	case WizardStepProfile:
		form.DeclareWithEnabled(
			WizardControlProfileList,
			chat.ControlKindChoiceList{
				Len:      wizard.ProfileListLen(dashboard),
				Selected: wizard.ProfileListSelected(dashboard),
			},
			true,
		)
		declareWizardButtons(form, false, wizard.ProfileAdvanceEnabled(dashboard))
	case WizardStepTarget:
		enabled := targetAdvanceEnabled(dashboard, wizard)
		offered := dashboard.offeredTargetIndices()
		form.DeclareWithEnabled(
			WizardControlTargetList,
			chat.ControlKindChoiceList{Len: len(offered), Selected: dashboard.targetRow(wizard.Target())},
			true,
		)
		// Indexed by display row, so the table's heading comes first.
		rows := []bool{false}
		for index, id := range slices.Sorted(maps.Keys(targets)) {
			if !slices.Contains(offered, index) {
				continue
			}
			_, rejected := wizard.TargetRejection(dashboard, id)
			rows = append(rows, !rejected)
		}
		form.SetRowEnabled(WizardControlTargetList, rows)
		declareResourceControls(form, targetResources(dashboard, wizard))
		declareWizardButtons(form, wizard.HasBack(), enabled)
	case WizardStepLaunching:
		form.DeclareWithEnabled(WizardControlCancel, chat.ControlKindButton{}, true)
		if _, failed := wizard.LaunchError(); failed {
			form.DeclareWithEnabled(WizardControlSubmit, chat.ControlKindButton{}, true)
		}
	case WizardStepMounts:
		declareMountControls(form, wizard.Mounts())
	case WizardStepReview:
		canAttach := core.OffersAttachedDirectories(targets[nthKey(targets, wizard.Target())])
		submitEnabled := wizard.DeclareReviewExtras(dashboard, form)
		declareReviewControls(form, canAttach, submitEnabled)
	case WizardStepBundle, WizardStepNewBundle, WizardStepProjectDirectory:
		wizard.DeclareExtraStep(dashboard, form)
	}
	form.EndFrame(initial)
}

func invalidateMovePreparation(wizard *ResumeWizard) {
	if wizard.moving {
		wizard.preparation = nil
		wizard.preparing = false
		wizard.preparationRequestID = nil
		wizard.preparationError = nil
	}
}

func declareWizardButtons(form *chat.Dialog[WizardControl], hasBack bool, nextEnabled bool) {
	form.DeclareWithEnabled(WizardControlCancel, chat.ControlKindButton{}, true)
	if hasBack {
		form.DeclareWithEnabled(WizardControlBack, chat.ControlKindButton{}, true)
	}
	form.DeclareActions([]chat.DialogAction[WizardControl]{
		{ID: WizardControlCancel, Label: "Cancel", Role: chat.ActionRoleCancel, Enabled: true},
		{ID: WizardControlNext, Label: "Next", Role: chat.ActionRolePrimary, Enabled: nextEnabled},
	})
}

func openMountAccess(mounts *MountWizard) {
	selected := accessIndex(mounts.accessChoices(), mounts.access)
	mounts.accessCombo.Open(WizardControlMountAccess, selected)
}

func commitMountAccess(mounts *MountWizard, index int) {
	if choices := mounts.accessChoices(); index < len(choices) {
		mounts.access = choices[index]
	}
}

func declareMountControls(form *chat.Dialog[WizardControl], mounts *MountWizard) {
	form.DeclareWithEnabled(WizardControlMountSource, mounts.source.ControlKind(), true)
	form.DeclareWithEnabled(WizardControlMountDestination, chat.ControlKindTextField{}, true)
	form.DeclareWithEnabled(
		WizardControlMountAccess,
		accessComboKind(&mounts.accessCombo, mounts.accessChoices(), mounts.access, WizardControlMountAccess),
		true,
	)
	if historyLen := len(mounts.availableHistory()); historyLen > 0 {
		form.DeclareWithEnabled(
			WizardControlMountHistory,
			chat.ControlKindChoiceList{Len: historyLen, Selected: min(mounts.historySelection, historyLen-1)},
			true,
		)
	}
	form.DeclareWithEnabled(WizardControlCancel, chat.ControlKindButton{}, true)
	form.DeclareWithEnabled(WizardControlBack, chat.ControlKindButton{}, true)
	form.DeclareWithEnabled(WizardControlAdd, chat.ControlKindButton{}, true)
	form.SetDefaultAction(WizardControlAdd)
}

func declareReviewControls(form *chat.Dialog[WizardControl], canAttach bool, submitEnabled bool) {
	form.DeclareWithEnabled(WizardControlCancel, chat.ControlKindButton{}, true)
	form.DeclareWithEnabled(WizardControlBack, chat.ControlKindButton{}, true)
	if canAttach {
		form.DeclareWithEnabled(WizardControlAdd, chat.ControlKindButton{}, true)
	}
	form.DeclareWithEnabled(WizardControlSubmit, chat.ControlKindButton{}, submitEnabled)
	form.SetDefaultAction(WizardControlSubmit)
}

// handleWizardEvent handles a wizard event through its persistent component
// form.
func (d *DashboardState) handleWizardEvent(event tea.Msg, wizard WizardDraft) DashboardAction {
	if wizard.InputLocked() {
		return d.keep(wizard)
	}
	wizard.ReconcileProfilePicker(d)
	declareWizardControls(d, wizard)
	if _, release := event.(tea.KeyReleaseMsg); release {
		return d.keep(wizard)
	}
	formEvent := event
	if press, ok := event.(tea.KeyPressMsg); ok && press.IsRepeat && press.Code == tea.KeyTab {
		// A held Tab (or Shift-Tab) still walks the form, as a fresh press.
		fresh := press
		fresh.IsRepeat = false
		formEvent = fresh
	}
	result := wizard.Form().Handle(formEvent)
	d.lastEventConsumed = result.Consumed
	action, rest, consumed := routePathCompletion(d, wizard, result.Action)
	if consumed {
		d.mode = wizard.IntoMode()
		return action
	}
	interaction := wizard.RouteExtraInteraction(rest)
	if interaction = wizard.MountsMut().accessCombo.Route(interaction); interaction != nil {
		toggleProvider := false
		if release, ok := event.(tea.MouseReleaseMsg); ok && release.Button == tea.MouseLeft {
			if area, ok := wizard.Form().ControlArea(WizardControlProfileList); ok && release.X < area.Min.X+4 {
				toggleProvider = true
			}
		}
		return d.applyWizardInteraction(wizard, interaction, toggleProvider)
	}
	if result.Consumed {
		return d.keep(wizard)
	}
	if press, ok := event.(tea.KeyPressMsg); ok && press.IsRepeat {
		return d.keep(wizard)
	}
	if wizard.HandleStepEvent(d, event) {
		return d.keep(wizard)
	}
	if press, ok := event.(tea.KeyPressMsg); ok {
		return d.handleWizardShortcut(press, wizard)
	}
	return d.keep(wizard)
}

func (d *DashboardState) applyWizardInteraction(wizard WizardDraft, interaction chat.Interaction[WizardControl], isClick bool) DashboardAction {
	switch it := interaction.(type) {
	case chat.InteractionCancel[WizardControl]:
		d.CancelModal()
		return DashboardActionNone{}
	case chat.InteractionActivate[WizardControl]:
		return d.activateWizardControl(wizard, it.ID)
	case chat.InteractionEdit[WizardControl]:
		change := DraftChangeFieldEdit
		if it.ID == WizardControlResourceCpu || it.ID == WizardControlResourceMemory {
			change = DraftChangeResourcesAdjusted
		}
		wizard.NoteDraftChange(d, change)
		d.applyWizardFieldEdit(wizard, it.ID, it.Edit)
		return d.keep(wizard)
	case chat.InteractionComboBoxCommit[WizardControl]:
		if it.ID == WizardControlMountAccess {
			wizard.NoteDraftChange(d, DraftChangeReadOnlyToggled)
			commitMountAccess(wizard.MountsMut(), it.Index)
			wizard.FormMut().Focus(WizardControlMountAccess)
			return d.keep(wizard)
		}
	case chat.InteractionSelect[WizardControl]:
		switch it.ID {
		case WizardControlProfileList:
			wizard.NoteDraftChange(d, DraftChangeProfileSelected)
			wizard.SelectProfileRow(d, it.Index, isClick)
			return d.keep(wizard)
		case WizardControlTargetList:
			// The list shows only the offered targets; the wizard keeps
			// indexing the full configuration.
			selected := wizard.Target()
			if offered := d.offeredTargetIndices(); it.Index >= 0 && it.Index < len(offered) {
				selected = offered[it.Index]
			}
			return d.selectWizardTarget(wizard, selected)
		case WizardControlMountHistory:
			if length := len(wizard.Mounts().availableHistory()); length > 0 {
				wizard.MountsMut().historySelection = min(it.Index, length-1)
				wizard.FormMut().Focus(WizardControlMountHistory)
			}
			return d.keep(wizard)
		case WizardControlReviewAttachments:
			next := min(it.Index, max(len(wizard.Mounts().mounts)-1, 0))
			focused, ok := wizard.Form().Focused()
			focusedElsewhere := !ok || focused != WizardControlReviewAttachments
			if wizard.Mounts().selectedMount != next || focusedElsewhere {
				wizard.MountsMut().selectedMount = next
				wizard.FormMut().Focus(WizardControlReviewAttachments)
			}
			return d.keep(wizard)
		}
	}
	wizard.ApplyExtraInteraction(d, interaction)
	return d.keep(wizard)
}

func (d *DashboardState) applyWizardFieldEdit(wizard WizardDraft, id WizardControl, edit chat.FieldEdit) {
	if id == WizardControlResourceCpu || id == WizardControlResourceMemory {
		before := wizard.ResourceEditor().draftValues()
		wizard.FormMut().TrackDraftPart("resources", before)
		editor := wizard.ResourceEditorMut()
		input := &editor.memory
		if id == WizardControlResourceCpu {
			input = &editor.cpu
		}
		if chat.TextFieldApply(input, edit) == chat.EditOutcomeChanged {
			d.validateWizardResources(wizard)
			d.recordEventHandled()
		}
		after := wizard.ResourceEditor().draftValues()
		wizard.FormMut().TrackDraftPart("resources", after)
		return
	}
	if wizard.ApplyExtraFieldEdit(d, id, edit) {
		return
	}
	d.applyMountFieldEdit(wizard.MountsMut(), id, edit)
}

// applyMountFieldEdit edits the attachment editor's two path fields.
func (d *DashboardState) applyMountFieldEdit(mounts *MountWizard, id WizardControl, edit chat.FieldEdit) {
	var input *chat.PathInput
	switch id {
	case WizardControlMountSource:
		input = &mounts.source
	case WizardControlMountDestination:
		input = &mounts.destination
	default:
		return
	}
	if chat.PathFieldApply(input, edit) == chat.EditOutcomeChanged {
		mounts.error = nil
		d.recordEventHandled()
	}
}

func (d *DashboardState) activateWizardControl(wizard WizardDraft, id WizardControl) DashboardAction {
	if id == WizardControlCancel {
		d.CancelModal()
		return DashboardActionNone{}
	}

	if wizard.Step() == WizardStepMounts {
		return d.activateWizardMount(id, wizard)
	}
	if wizard.Step() == WizardStepReview || wizard.Step() == WizardStepLaunching {
		return d.activateWizardReview(id, wizard)
	}
	if action, handled := wizard.ActivateExtra(d, id); handled {
		return action
	}
	if id == WizardControlBack {
		if !wizard.HasBack() {
			return d.keep(wizard)
		}
		step := wizard.Step()
		switch step {
		case WizardStepTarget:
			step = WizardStepProfile
		case WizardStepBundle, WizardStepProjectDirectory:
			if wizard.TargetStepSkipped() {
				step = WizardStepProfile
			} else {
				step = WizardStepTarget
			}
		}
		wizard.SetStep(step)
		wizard.FormMut().Focus(stepInitial(step))
		return d.keep(wizard)
	}
	return wizard.Advance(d)
}

func (d *DashboardState) activateNewBundleControl(wizard *NewWizard, id WizardControl) DashboardAction {
	if wizard.bundleCreationInFlight {
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	switch id {
	case WizardControlCancel:
		d.CancelModal()
		return DashboardActionNone{}
	case WizardControlBack:
		if len(d.config.Get().Bundles) == 0 {
			wizard.step = WizardStepTarget
		} else {
			wizard.step = WizardStepBundle
		}
		wizard.form.Focus(stepInitial(wizard.step))
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	case WizardControlNewBundleRepositories:
		wizard.form.Focus(WizardControlNewBundleRepositories)
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	case WizardControlNewBundleSource:
		wizard.form.Focus(WizardControlNewBundleSource)
		if wizard.projectPicker.multiple {
			return d.addOrReportNewBundleRepository(wizard)
		}
		return d.submitNewBundle(wizard)
	case WizardControlAdd:
		wizard.form.Focus(WizardControlAdd)
		return d.addOrReportNewBundleRepository(wizard)
	case WizardControlNewBundleRemove:
		wizard.form.Focus(WizardControlNewBundleRemove)
		wizard.removeSelectedNewBundleRepository()
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	case WizardControlNext:
		wizard.form.Focus(WizardControlNext)
		return d.activateProjectControl(wizard, id)
	case WizardControlProfileList,
		WizardControlBundleList,
		WizardControlTargetList,
		WizardControlProjectDirectory,
		WizardControlMountSource,
		WizardControlMountDestination,
		WizardControlMountAccess,
		WizardControlReviewAttachments,
		WizardControlCreateManagedWorktree,
		WizardControlDiscardQueue,
		WizardControlSubmit:
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	if _, recent := id.RecentProjectIndex(); recent {
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	return d.activateProjectControl(wizard, id)
}

func (d *DashboardState) addOrReportNewBundleRepository(wizard *NewWizard) DashboardAction {
	if !wizard.addNewBundleRepository() {
		d.notices.Set("Repository source cannot be empty.")
	}
	d.mode = ModeNew{wizard}
	return DashboardActionNone{}
}

func (d *DashboardState) submitNewBundle(wizard *NewWizard) DashboardAction {
	sources := wizard.newBundleSourcesForSubmit()
	if len(sources) == 0 {
		d.notices.Set("Repository source cannot be empty.")
		d.mode = ModeNew{wizard}
		return DashboardActionNone{}
	}
	wizard.bundleCreationInFlight = true
	wizard.projectPicker.creationError = nil
	d.notices.Set("Preparing project…")
	d.mode = ModeNew{wizard}
	return DashboardActionCreateBundle{Sources: sources}
}

func (d *DashboardState) handleWizardShortcut(key tea.KeyPressMsg, wizard WizardDraft) DashboardAction {
	focused, hasFocus := wizard.Form().Focused()
	if resumeKeyModifiers(key.Mod) != 0 {
		return d.keep(wizard)
	}
	code := chat.KeyPressCode(key)
	pickerFocused := hasFocus && (focused == WizardControlProfileList ||
		focused == WizardControlTargetList || focused == WizardControlBundleList)
	if code == tea.KeyBackspace && pickerFocused {
		return d.activateWizardControl(wizard, WizardControlBack)
	}
	if code == tea.KeyDelete && hasFocus && focused == WizardControlBundleList {
		return d.activateWizardControl(wizard, WizardControlRemoveBundle)
	}
	if code == tea.KeyDelete && hasFocus && focused == WizardControlReviewAttachments {
		wizard.NoteDraftChange(d, DraftChangeAttachmentRemoved)
		removeSelectedMount(wizard.MountsMut())
		if len(wizard.Mounts().mounts) == 0 {
			wizard.FormMut().Focus(WizardControlSubmit)
		}
		// Delete only reaches this control on the review step, so no later
		// arm of this function can apply to it.
		return wizard.ReenterReview(d)
	}
	wizard.HandleExtraShortcut(d, key)
	return d.keep(wizard)
}

func (d *DashboardState) activateWizardReview(id WizardControl, wizard WizardDraft) DashboardAction {
	if action, handled := wizard.ActivateExtra(d, id); handled {
		return action
	}
	targets := d.config.Get().Targets
	canAttach := core.OffersAttachedDirectories(targets[nthKey(targets, wizard.Target())])
	switch id {
	case WizardControlReviewAttachments:
		wizard.NoteDraftChange(d, DraftChangeAttachmentOpened)
		editSelectedMount(wizard)
	case WizardControlCancel:
		d.CancelModal()
		return DashboardActionNone{}
	case WizardControlBack:
		wizard.NoteDraftChange(d, DraftChangeReviewLeft)
		step := wizard.ReviewBackStep(d)
		wizard.SetStep(step)
		wizard.FormMut().Focus(stepInitial(step))
	case WizardControlAdd:
		if canAttach {
			wizard.NoteDraftChange(d, DraftChangeAttachmentEditorOpened)
			beginMountEditor(wizard)
		}
	case WizardControlSubmit:
		return wizard.SubmitReview(d)
	case WizardControlUseSandboxedTarget:
		if offer := d.sandboxOffer(wizard); offer != nil && offer.alternative != nil {
			wizard.SetTargetStepSkipped(false)
			wizard.SetStep(WizardStepTarget)
			wizard.FormMut().Focus(WizardControlNext)
			return d.selectWizardTarget(wizard, *offer.alternative)
		}
	default:
		if action, handled := wizard.ActivateExtra(d, id); handled {
			return action
		}
		return d.keep(wizard)
	}
	return d.keep(wizard)
}

func (d *DashboardState) activateWizardMount(id WizardControl, wizard WizardDraft) DashboardAction {
	targets := d.config.Get().Targets
	targetTemplateID := nthKey(targets, wizard.Target())
	switch id {
	case WizardControlMountSource:
		if wizard.Mounts().source.IsEmpty() {
			message := "Choose or type a directory on the controller."
			wizard.MountsMut().error = &message
			return d.keep(wizard)
		}
		if wizard.Mounts().destination.IsEmpty() {
			destination := defaultResourceDestination(
				targets[targetTemplateID],
				wizard.Mounts().source.Value(),
				wizard.Mounts().mounts,
			)
			wizard.MountsMut().destination = chat.PathInputFromValue(destination)
		}
		wizard.FormMut().Focus(WizardControlMountDestination)
		return d.keep(wizard)
	case WizardControlMountAccess:
		openMountAccess(wizard.MountsMut())
		return d.keep(wizard)
	case WizardControlMountHistory:
		history := wizard.Mounts().availableHistory()
		if selection := wizard.Mounts().historySelection; selection >= 0 && selection < len(history) {
			source := history[selection]
			wizard.MountsMut().source = chat.PathInputFromValue(source)
			if wizard.Mounts().destination.IsEmpty() {
				destination := defaultResourceDestination(targets[targetTemplateID], source, wizard.Mounts().mounts)
				wizard.MountsMut().destination = chat.PathInputFromValue(destination)
			}
			return d.activateWizardMount(WizardControlAdd, wizard)
		}
		return d.keep(wizard)
	case WizardControlMountDestination, WizardControlAdd:
		return d.validateWizardMount(wizard, targetTemplateID)
	case WizardControlCancel:
		d.CancelModal()
		return DashboardActionNone{}
	case WizardControlBack:
		mounts := wizard.MountsMut()
		mounts.source.Clear()
		mounts.destination.Clear()
		mounts.error = nil
		mounts.source.DismissCompletion()
		wizard.FormMut().ForgetDraftPart("attachment editor")
		wizard.SetStep(WizardStepReview)
		wizard.FormMut().Focus(WizardControlAdd)
		return wizard.ReenterReview(d)
	}
	return d.keep(wizard)
}

func (d *DashboardState) validateWizardMount(wizard WizardDraft, targetTemplateID string) DashboardAction {
	if reason, invalid := validateMountEntry(wizard.Mounts()); invalid {
		wizard.MountsMut().error = &reason
		wizard.FormMut().Focus(WizardControlMountSource)
		return d.keep(wizard)
	}
	source := wizard.Mounts().source.String()
	wizard.FormMut().SetSubmissionPending(true)
	d.mode = wizard.IntoMode()
	return DashboardActionValidateMountSource{
		TargetTemplateID: targetTemplateID,
		Source:           source,
	}
}

// wizardActionIsNone is Rust's `action == DashboardAction::None`; a nil
// action, which a Go handler returns for Rust's absent Option, counts as
// none too.
func wizardActionIsNone(action DashboardAction) bool {
	if action == nil {
		return true
	}
	_, none := action.(DashboardActionNone)
	return none
}
