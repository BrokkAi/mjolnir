// Port of mj-tui/src/setup/events.rs.

package tui

import (
	"encoding/json"
	"fmt"
	"maps"
	"slices"
	"strings"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
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

// newSetupDialogForDashboard is a Settings dialog over the dashboard's
// configuration that already knows the sign-in statuses read so far.
func (d *DashboardState) newSetupDialogForDashboard() *SetupDialog {
	dialog := newSetupDialog(d.config.Get())
	maps.Copy(dialog.loginStatuses, d.profileLoginStatuses)
	return dialog
}

// beginSettingsSection opens Settings on section, and on its entry when the
// draft has one.
func (d *DashboardState) beginSettingsSection(section string, entry *string) {
	d.profileLoginRefresh = true
	dialog := d.newSetupDialogForDashboard()
	dialog.path = []string{section}
	if entry != nil && setupValueHas(setupValueGet(dialog.draft, section), *entry) {
		dialog.path = append(dialog.path, *entry)
	}
	dialog.prepare()
	d.mode = ModeSetup{dialog}
	d.takeSetupSubagentChoices()
}

// beginStagedSettings opens Settings on the page of the first edit with
// every edit staged in the draft, unsaved. An open Settings keeps its other
// unsaved edits and takes these as well. Nothing changes when any edit does
// not apply.
func (d *DashboardState) beginStagedSettings(edits []client.SettingEdit, notice string) error {
	var dialog *SetupDialog
	wasOpen := false
	if setup, ok := d.mode.(ModeSetup); ok {
		dialog = setup.SetupDialog
		wasOpen = true
		d.mode = ModeDashboard{}
	} else {
		d.profileLoginRefresh = true
		dialog = d.newSetupDialogForDashboard()
	}
	paths, err := dialog.stageEdits(edits)
	if err == nil {
		first := paths[0]
		dialog.openPage(setupPageForPath(first))
		dialog.focusPath(first)
		dialog.notice = &notice
	}
	// An open Settings stays open either way; a new one opens only with the
	// edits in it.
	if wasOpen || err == nil {
		d.mode = ModeSetup{dialog}
		d.takeSetupSubagentChoices()
	}
	return err
}

// BeginSetup opens Settings on its first page.
func (d *DashboardState) BeginSetup() {
	d.profileLoginRefresh = true
	dialog := d.newSetupDialogForDashboard()
	d.mode = ModeSetup{dialog}
	d.takeSetupSubagentChoices()
}

func (d *DashboardState) dismissSetup(dialog *SetupDialog) DashboardAction {
	if dialog.saving {
		d.mode = ModeSetup{dialog}
		return DashboardActionNone{}
	}
	if dialog.isDirty() {
		confirm := newConfirmDialog(ConfirmationDismiss{
			Mode:   ModeSetup{dialog},
			Intent: DismissalIntentDiscardSetup,
		})
		d.mode = ModeConfirm{&confirm}
	} else {
		d.CancelModal()
	}
	return DashboardActionNone{}
}

// beginSetupReview opens the Code Review editor of the open Settings (Rust
// test support).
func (d *DashboardState) beginSetupReview() DashboardAction {
	mode := d.mode
	d.mode = ModeDashboard{}
	setup, ok := mode.(ModeSetup)
	if !ok {
		return DashboardActionNone{}
	}
	dialog := setup.SetupDialog
	action := dialog.openReview(d)
	d.mode = ModeSetup{dialog}
	return action
}

// SetProfileCapabilities replaces the profile capability snapshot.
func (d *DashboardState) SetProfileCapabilities(snapshot core.ProfileCapabilitiesSnapshot) {
	for key, identities := range d.profileHydrationRequests {
		covered := true
		for identity := range identities {
			if _, ok := snapshot.Profiles[identity]; !ok {
				covered = false
				break
			}
		}
		if covered {
			delete(d.profileHydrationRequests, key)
		}
	}
	for key := range d.profileHydrationErrors {
		if _, ok := d.profileHydrationRequests[key]; !ok {
			delete(d.profileHydrationErrors, key)
		}
	}
	d.profileCapabilities = snapshot
	d.takeSetupSubagentChoices()
	d.refreshReviewChoices()
	d.takeSubagentDiscovery()
}

// refreshReviewChoices recomputes the open Code Review editor's choices and
// Autoconfigure suggestion from the current capabilities and quota reports.
func (d *DashboardState) refreshReviewChoices() {
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.reviewEditor == nil {
		return
	}
	config, err := client.ConfigFromDraft(dialog.draft)
	if err != nil {
		return
	}
	usagePriced := d.usagePricedProfiles(&config)
	review := dialog.reviewEditor
	review.updateChoices(&config, &d.profileCapabilities, usagePriced)
	dialog.syncReviewValidation(review)
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
	return d.config.Get()
}

// invalidateProfileLoginStatuses drops statuses and any read still in flight
// after the profile set changes. The generation bump makes an older result
// unusable.
func (d *DashboardState) invalidateProfileLoginStatuses() {
	d.profileLoginGeneration++
	d.profileLoginStatuses = map[string]core.LoginStatus{}
	d.profileLoginRefresh = true
	statuses := map[string]core.LoginStatus{}
	for id, profile := range d.config.Get().Profiles {
		if obviouslyNoInteractiveLogin(&profile) {
			statuses[id] = core.LoginStatusNotRequired{Detail: "no interactive login"}
		}
	}
	d.profileLoginStatuses = statuses
	if dialog := setupDialogMut(d.mode); dialog != nil {
		dialog.refreshSavedProfiles(d.config.Get())
		dialog.loginStatuses = maps.Clone(d.profileLoginStatuses)
	}
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

// setupActionIsNone is `action == DashboardAction::None`.
func setupActionIsNone(action DashboardAction) bool {
	if action == nil {
		return true
	}
	_, none := action.(DashboardActionNone)
	return none
}

// takeProfileHydration is Settings' prerequisite poll after every background
// reply; nil when there is nothing to send.
func (d *DashboardState) takeProfileHydration() DashboardAction {
	// This is Settings' existing prerequisite poll after every background
	// reply. Complete an accepted Save before starting optional capability
	// hydration.
	if dialog := setupDialogMut(d.mode); dialog != nil {
		if _, ready := dialog.pendingSave.(PendingSaveReady); ready {
			dialog.pendingSave = PendingSaveIdle{}
			if action := dialog.save(&d.profileCapabilities); !setupActionIsNone(action) {
				return action
			}
		}
	}
	dialog := setupDialogMut(d.mode)
	if dialog == nil {
		return nil
	}
	config, err := client.ConfigFromDraft(dialog.draft)
	if err != nil {
		return nil
	}
	if review := dialog.reviewEditor; review != nil {
		if choices, ok := review.pendingAutoconfigureChoices(); ok {
			config.Review.Reviewers = slices.Clone(choices)
			// A legacy review serializes without its reviewer list, so the
			// newly selected models would not be warmed.
			config.Review.ClearLegacy()
		}
	}
	// A selected model's efforts are discovered only once it is selected,
	// so a draft choosing another model is warmed like a new profile.
	key := config.CapabilityDemandKey()
	// Saved profiles and selections are already hydrating from daemon
	// startup/reload.
	_, requested := d.profileHydrationRequests[key]
	sent := d.profileHydrationSent != nil && *d.profileHydrationSent == key
	if key == d.config.Get().CapabilityDemandKey() || requested || sent || d.profileCapabilities.Covers(&config) {
		return nil
	}
	missing := map[string]struct{}{}
	for id, profile := range config.EnabledProfiles() {
		identity := profile.CapabilitiesKey(id)
		if _, known := d.profileCapabilities.Profiles[identity]; !known {
			missing[identity] = struct{}{}
		}
	}
	d.profileHydrationSent = &key
	if d.profileHydrationRequests == nil {
		d.profileHydrationRequests = map[string]map[string]struct{}{}
	}
	d.profileHydrationRequests[key] = missing
	encoded, err := json.Marshal(config)
	if err != nil {
		panic(fmt.Sprintf("config serializes: %v", err))
	}
	return DashboardActionWarmProfileCapabilities{Key: key, Config: string(encoded)}
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
		if d.profileHydrationErrors == nil {
			d.profileHydrationErrors = map[string]string{}
		}
		d.profileHydrationErrors[key] = err.Error()
		d.SetNotice(fmt.Sprintf("Profile capability hydration failed: %s", err.Error()))
	}
}

// TickReviewAutoconfigure: the one-second UI clock completes an
// Autoconfigure wait even if discovery never publishes another snapshot.
func (d *DashboardState) TickReviewAutoconfigure() bool {
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.reviewEditor == nil || !dialog.reviewEditor.checkingEfforts() {
		return false
	}
	config, err := client.ConfigFromDraft(dialog.draft)
	if err != nil {
		return false
	}
	usagePriced := d.usagePricedProfiles(&config)
	review := dialog.reviewEditor
	review.updateChoices(&config, &d.profileCapabilities, usagePriced)
	changed := !review.checkingEfforts()
	if changed {
		dialog.syncReviewValidation(review)
	}
	return changed
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

// setupKeyCode is the key a press names, as crossterm's KeyCode names it: a
// printable key is the character it types, so a shifted letter is the
// upper-case character (crossterm's `Char('A')`).
func setupKeyCode(key tea.Key) rune {
	if character, size := utf8.DecodeRuneInString(key.Text); size > 0 && size == len(key.Text) {
		return character
	}
	return key.Code
}

// setupKeyMods is the modifiers held, without the lock keys crossterm does
// not report as modifiers.
func setupKeyMods(key tea.Key) tea.KeyMod {
	return key.Mod &^ (tea.ModCapsLock | tea.ModNumLock | tea.ModScrollLock)
}

// setupErrorNotice is Rust's `result.err()` for a notice: the message, or
// nil on success.
func setupErrorNotice(err error) *string {
	if err == nil {
		return nil
	}
	message := err.Error()
	return &message
}

func (d *DashboardState) handleSetupEvent(event tea.Msg, dialog *SetupDialog) DashboardAction {
	// The dialog is out of the mode while its event is handled; every way
	// out puts back the mode it leaves.
	d.mode = ModeDashboard{}
	if dialog.saving {
		d.mode = ModeSetup{dialog}
		return DashboardActionNone{}
	}
	press, isPress := event.(tea.KeyPressMsg)
	if review := dialog.reviewEditor; review != nil {
		dialog.reviewEditor = nil
		saveShortcut := !review.form.ConfirmationOpen() && isPress &&
			setupKeyCode(press.Key()) == 's' && setupKeyMods(press.Key()).Contains(tea.ModCtrl)
		if saveShortcut {
			if !review.canSave() {
				message := "Choose 1–3 reviewers for automatic review."
				review.saveError = &message
				dialog.reviewEditor = review
				d.mode = ModeSetup{dialog}
				return DashboardActionNone{}
			}
			dialog.syncReviewValidation(review)
			action := dialog.save(&d.profileCapabilities)
			if dialog.saving || dialog.anyFieldError() {
				dialog.prepare()
				d.mode = ModeSetup{dialog}
			} else {
				review.saveError = setupCopyString(dialog.notice)
				dialog.reviewEditor = review
				d.mode = ModeSetup{dialog}
			}
			return action
		}
		reviewConfig, err := client.ConfigFromDraft(dialog.draft)
		if err != nil {
			reviewConfig = *d.config.Get()
		}
		action, outcome := review.handleEvent(d, &reviewConfig, event)
		switch outcome {
		case ReviewSettingsOutcomeContinue:
			dialog.syncReviewValidation(review)
			if config, err := client.ConfigFromDraft(dialog.draft); err == nil {
				review.updateChoices(&config, &d.profileCapabilities, d.usagePricedProfiles(&config))
				dialog.syncReviewValidation(review)
			}
			dialog.reviewEditor = review
		case ReviewSettingsOutcomeBack:
			dialog.syncReviewValidation(review)
			dialog.form = chat.NewDialog[SetupControl]()
			dialog.focusPath([]string{"review"})
		case ReviewSettingsOutcomeCancelSetup:
			dialog.syncReviewValidation(review)
			dialog.reviewEditor = review
			dialog.prepare()
			d.dismissSetup(dialog)
			return action
		case ReviewSettingsOutcomeSave:
			dialog.syncReviewValidation(review)
			dialog.reviewEditor = nil
			dialog.form = chat.NewDialog[SetupControl]()
			action = dialog.save(&d.profileCapabilities)
			if !dialog.saving && !dialog.anyFieldError() {
				review.saveError = setupCopyString(dialog.notice)
				dialog.reviewEditor = review
				dialog.prepare()
			}
		}
		if dialog.reviewEditor == nil {
			dialog.prepare()
		}
		d.mode = ModeSetup{dialog}
		return action
	}
	// The Detection results view owns its scroll keys and its way out;
	// everything else (Tab to the footer, Enter) falls through to the form.
	if dialog.report != nil && handleReportEvent(dialog, event) {
		d.lastEventConsumed = true
		d.mode = ModeSetup{dialog}
		return DashboardActionNone{}
	}
	dialog.prepare()
	fieldFocused := dialog.textInputFocused()
	_, popupOpen := dialog.combo.OpenID()
	if !popupOpen {
		if path, ok := dialog.focusedFieldPath(); ok {
			if field, ok := dialog.fields.get(path); ok {
				if input := field.input.pathInput(); input != nil {
					_, pending := input.CompletionPending()
					popupOpen = input.IsCompleting() || pending
				}
			}
		}
	}
	var shortcut chat.Interaction[SetupControl]
	if isPress && !dialog.form.ConfirmationOpen() {
		key := press.Key()
		code, mods := setupKeyCode(key), setupKeyMods(key)
		// Every shortcut but Save is a bare key, so none of them may fire
		// while the search query has the keyboard or the Detection results
		// view is up.
		bare := !fieldFocused && !popupOpen && dialog.search == nil && dialog.report == nil
		switch {
		case code == 's' && mods.Contains(tea.ModCtrl):
			if dialog.managedSetup != nil {
				shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlManagedContinue}
			} else {
				shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlSave}
			}
		case code == '/' && bare && mods == 0:
			shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlOpenSearch}
		case code == tea.KeyBackspace && bare:
			shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlBack}
		// The dialog's own border promises `Esc back`, so on a page below
		// the first one Esc does what Back does and keeps the draft. Only
		// the first page closes on Esc, and only the title's × closes from
		// anywhere.
		case code == tea.KeyEscape && dialog.editor == nil && !popupOpen && dialog.search == nil &&
			dialog.report == nil && len(dialog.path) > 0 && mods == 0:
			shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlBack}
		case code == 'a' && bare:
			shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlAdd}
		case code == tea.KeyDelete && bare:
			shortcut = chat.InteractionActivate[SetupControl]{ID: SetupControlRemove}
		}
	}
	// Arrows browse the results while the query keeps the keyboard, which is
	// how the command palette's search behaves.
	var browse *tea.KeyPressMsg
	if isPress && dialog.search != nil && dialog.form.IsFocused(SetupControlSearch) &&
		(press.Key().Code == tea.KeyUp || press.Key().Code == tea.KeyDown) {
		browsed := tea.KeyPressMsg{Code: press.Key().Code}
		browse = &browsed
	}
	var formResult *chat.EventResult[chat.Interaction[SetupControl]]
	if shortcut == nil {
		var result chat.EventResult[chat.Interaction[SetupControl]]
		if browse != nil {
			dialog.form.Focus(SetupControlResults)
			result = dialog.form.Handle(*browse)
			dialog.form.Focus(SetupControlSearch)
		} else {
			result = dialog.form.Handle(event)
		}
		formResult = &result
		d.lastEventConsumed = result.Consumed
	}
	if dialog.managedSetup != nil {
		for _, control := range dialog.descriptors {
			if !dialog.form.IsFocused(control.id) {
				continue
			}
			if managed, ok := control.kind.(SettingKindManagedHarness); ok {
				index := managed.index
				dialog.managedSetup = &index
			}
			break
		}
	}
	interaction := shortcut
	if interaction == nil && formResult != nil {
		interaction = formResult.Action
	}
	interaction = dialog.combo.Route(interaction)
	if action, rest, consumed := routePathCompletion(d, dialog, interaction); consumed {
		dialog.prepare()
		d.mode = ModeSetup{dialog}
		return action
	} else {
		interaction = rest
	}
	action, postFocus, done := d.applySetupInteraction(dialog, interaction)
	if done {
		return action
	}
	if setupActionIsNone(action) {
		action = dialog.previewBuildCacheAction()
	}
	if setupActionIsNone(action) {
		action = dialog.previewArchiveSpaceAction()
	}
	if unreachable, err := client.FillSubagentOptions(&dialog.draft, &d.profileCapabilities); err == nil {
		syncSubagentDiscoveryNotice(dialog, unreachable)
	}
	dialog.prepare()
	if postFocus != nil {
		if postFocus.path != nil {
			dialog.focusPath(postFocus.path)
		} else {
			dialog.form.Focus(postFocus.control)
			dialog.form.RevealFocused()
		}
	}
	d.mode = ModeSetup{dialog}
	return action
}

// setupPostRefreshFocus is where focus goes once the page is prepared again:
// a setting's path, or a control when path is nil.
type setupPostRefreshFocus struct {
	path    []string
	control SetupControl
}

// anyFieldError is whether a field refused its pending text.
func (d *SetupDialog) anyFieldError() bool {
	for _, field := range d.fields {
		if field.err != nil {
			return true
		}
	}
	return false
}

// setupCopyString is Option<String>::clone.
func setupCopyString(text *string) *string {
	if text == nil {
		return nil
	}
	copied := *text
	return &copied
}

// applySetupInteraction applies the form's interaction to the dialog. done
// is true when the mode is already decided and action is final.
func (d *DashboardState) applySetupInteraction(dialog *SetupDialog, interaction chat.Interaction[SetupControl]) (action DashboardAction, postFocus *setupPostRefreshFocus, done bool) {
	action = DashboardActionNone{}
	var activated, toggled *SetupControl
	_, cancel := interaction.(chat.InteractionCancel[SetupControl])
	switch typed := interaction.(type) {
	case chat.InteractionActivate[SetupControl]:
		activated = &typed.ID
	case chat.InteractionToggle[SetupControl]:
		toggled = &typed.ID
	}
	activates := func(ids ...SetupControl) bool {
		return activated != nil && slices.Contains(ids, *activated)
	}
	var rowActivation *SetupControl
	if activated != nil {
		if _, isRow := activated.IsRow(); isRow {
			rowActivation = activated
		}
	}
	if toggled != nil {
		if _, isRow := toggled.IsRow(); isRow {
			rowActivation = toggled
		}
	}
	edit, isEdit := interaction.(chat.InteractionEdit[SetupControl])
	selection, isSelect := interaction.(chat.InteractionSelect[SetupControl])
	commit, isCommit := interaction.(chat.InteractionComboBoxCommit[SetupControl])
	removeFocus := func(index int) *setupPostRefreshFocus {
		if path, ok := dialog.removeSubagentEntry(index); ok {
			return &setupPostRefreshFocus{path: path}
		}
		return &setupPostRefreshFocus{control: SetupControlAdd}
	}
	switch {
	case activates(SetupControlOpenSearch) && dialog.managedSetup == nil:
		dialog.openSearch()
	case (cancel || activates(SetupControlBack)) && dialog.search != nil:
		dialog.closeSearch()
	// Esc and Backspace are the report's own; Back here is the footer
	// button or Enter on it.
	case (cancel || activates(SetupControlBack)) && dialog.report != nil:
		dialog.report = nil
	case activates(SetupControlDetectionResults) && len(dialog.detectionRejected) > 0:
		dialog.report = &ReportState{}
	case activates(SetupControlOpenMachines):
		dialog.openPage([]string{"machines"})
	case isEdit && edit.ID == SetupControlSearch:
		if search := dialog.search; search != nil && chat.TextFieldApply(&search.input, edit.Edit) == chat.EditOutcomeChanged {
			search.refilter()
			d.recordEventHandled()
		}
	case isSelect && selection.ID == SetupControlResults:
		if search := dialog.search; search != nil {
			search.selected = selection.Index
		}
	case activates(SetupControlSearch, SetupControlResults) || (toggled != nil && *toggled == SetupControlResults):
		if search := dialog.search; search != nil {
			if selected, ok := dialog.form.Selected(SetupControlResults); ok {
				search.selected = selected
			}
		}
		var target []string
		if search := dialog.search; search != nil {
			if entry, ok := search.selectedEntry(); ok {
				target = slices.Clone(entry.path)
			}
		}
		if target != nil && dialog.jumpTo(target) == JumpOpenReview {
			action = dialog.openReview(d)
		}
	case cancel && dialog.editor != nil:
		dialog.back()
	case (cancel || activates(SetupControlBack)) && dialog.managedSetup != nil:
		dialog.leaveManagedSetup()
	case cancel || activates(SetupControlCancel):
		return d.dismissSetup(dialog), nil, true
	case activates(SetupControlBack):
		if dialog.back() {
			// Leaving from the root is a dismissal; keep the dirty guard.
			return d.dismissSetup(dialog), nil, true
		}
	case rowActivation != nil:
		id := *rowActivation
		descriptor, ok := dialog.descriptor(id)
		if !ok {
			break
		}
		control := SettingControl{id: descriptor.id, path: slices.Clone(descriptor.path), kind: descriptor.kind}
		switch kind := control.kind.(type) {
		case SettingKindPage:
			if slices.Equal(control.path, []string{"review"}) {
				action = dialog.openReview(d)
			} else {
				dialog.openPage(control.path)
			}
		case SettingKindRemoveEntry:
			if index, ok := subagentRowIndex(control.path); ok {
				postFocus = removeFocus(index)
			}
		case SettingKindCheckbox:
			dialog.togglePath(control.path)
		case SettingKindChoices:
			dialog.combo.Open(id, dialog.choicePosition(control.path, kind.values))
		case SettingKindField:
			resolve, err := dialog.resolvePathAction(control.path)
			switch {
			case err != nil:
				if field, ok := dialog.fields.get(control.path); ok {
					field.err = setupErrorNotice(err)
				}
				dialog.form.RevealFocused()
			case resolve != nil:
				action = resolve
			default:
				// Validation feedback belongs to the field; preserve page
				// notices.
				_ = dialog.applyPending(control.path)
			}
		case SettingKindManagedHarness:
			index := kind.index
			dialog.managedSetup = &index
			action = dialog.createManagedProfileAndLogin(&d.profileCapabilities)
		case SettingKindReadOnly:
		}
	case isEdit && isSetupRow(edit.ID):
		if control, ok := dialog.descriptor(edit.ID); ok {
			if field, ok := dialog.fields.get(control.path); ok {
				var outcome chat.EditOutcome
				if input := field.input.pathInput(); input != nil {
					outcome = chat.PathFieldApply(input, edit.Edit)
				} else {
					outcome = chat.TextFieldApply(field.input.text(), edit.Edit)
				}
				if outcome == chat.EditOutcomeChanged {
					field.err = nil
					dialog.pendingSave = PendingSaveIdle{}
					d.recordEventHandled()
				}
			}
		}
	case isCommit && isSetupRow(commit.ID):
		if control, ok := dialog.descriptor(commit.ID); ok {
			if choices, isChoices := control.kind.(SettingKindChoices); isChoices && commit.Index >= 0 && commit.Index < len(choices.values) {
				value := setupValueClone(choices.values[commit.Index])
				path := slices.Clone(control.path)
				dialog.notice = setupErrorNotice(dialog.applyValue(path, nil, &value))
			}
		}
	case isSetupComboDismissRow(interaction):
	case isEdit && edit.ID == SetupControlField:
		if editor := dialog.editor; editor != nil {
			chat.TextFieldApply(editor.input.text(), edit.Edit)
		}
	case activates(SetupControlField, SetupControlApply):
		dialog.notice = setupErrorNotice(dialog.applyEditor())
	case activates(SetupControlManagedSetup):
		dialog.beginManagedSetup()
	case activates(SetupControlManagedContinue) && dialog.managedSetup != nil:
		action = dialog.createManagedProfileAndLogin(&d.profileCapabilities)
	case activates(SetupControlAdd) && dialog.managedSetup == nil:
		if modelPath, ok := dialog.add(); ok {
			postFocus = &setupPostRefreshFocus{path: modelPath}
		}
	case activates(SetupControlRemove):
		if isSubagentCategoryPage(dialog.path) {
			if focused, ok := dialog.form.Focused(); ok {
				if control, ok := dialog.descriptor(focused); ok {
					if index, ok := subagentRowIndex(control.path); ok {
						postFocus = removeFocus(index)
					}
				}
			}
		} else if len(dialog.path) > 0 {
			key, parent := dialog.path[len(dialog.path)-1], slices.Clone(dialog.path[:len(dialog.path)-1])
			if value, ok := setupLookup(dialog.draft, parent); ok && client.IsCollection(parent, value) {
				removal, err := client.RemoveEntry(&dialog.draft, slices.Clone(dialog.path))
				if err != nil {
					dialog.notice = setupErrorNotice(err)
				} else {
					if removal.Notice != nil {
						dialog.notice = removal.Notice
					}
					// Rebind pending text to the surviving entry when array
					// indices shift.
					dialog.rebindFieldsAfterRemoval(parent, &removal)
					if len(parent) > 0 && parent[0] == "profiles" {
						profileID := key
						if len(parent) > 1 {
							profileID = parent[1]
						}
						dialog.invalidateReviewValidationFor(&profileID)
					}
					dialog.descriptors = nil
					dialog.openPage(parent)
				}
			}
		}
	case activates(SetupControlSignIn) && len(dialog.path) == 2 && dialog.path[0] == "profiles":
		if slices.ContainsFunc(dialog.actions(), func(button chat.ButtonSpec[SetupControl]) bool {
			return button.ID == SetupControlSignIn && button.Enabled
		}) {
			action = DashboardActionSetupLogin{Generation: dialog.generation, ProfileID: dialog.path[1]}
		}
	case activates(SetupControlSave) && dialog.editor == nil:
		action = dialog.save(&d.profileCapabilities)
	case activates(SetupControlDetectProfiles, SetupControlDetectRuntimes) && !dialog.discovering:
		scope := DetectScopeProfiles
		notice := "Looking for installed agents…"
		if *activated == SetupControlDetectRuntimes {
			scope = DetectScopeRuntimes
			notice = "Looking for usable runtimes on every machine…"
		}
		dialog.discovering = true
		dialog.notice = &notice
		// A draft that does not parse probes no machine; the result then
		// asks for the draft to be reviewed.
		machines := []client.DaemonPair[string, core.Machine]{}
		if scope == DetectScopeRuntimes {
			if draft, err := client.ConfigFromDraft(dialog.draft); err == nil {
				for _, id := range slices.Sorted(maps.Keys(draft.Machines)) {
					machine := draft.Machines[id]
					if _, local := machine.(core.MachineLocal); !local {
						machines = append(machines, client.DaemonPair[string, core.Machine]{First: id, Second: machine})
					}
				}
			}
		}
		action = DashboardActionDiscoverSetup{Generation: dialog.generation, Scope: scope, Machines: machines}
	case activates(SetupControlInstallMbx):
		offer, ok := dialog.installMbxOffer()
		if !ok {
			d.mode = ModeSetup{dialog}
			return DashboardActionNone{}, nil, true
		}
		confirm := newConfirmDialog(ConfirmationInstallMbx{
			Generation:     dialog.generation,
			Key:            offer.key,
			MachineID:      offer.machineID,
			Machine:        offer.machine,
			Host:           machineHostLabel(offer.machineID, offer.machine),
			Upgrading:      offer.upgrading,
			ProfileFile:    offer.profileFile,
			ProfileWarning: offer.profileWarning,
			ManualPathLine: offer.manualPathLine,
			Previous:       ModeSetup{dialog},
		})
		d.mode = ModeConfirm{&confirm}
		return DashboardActionNone{}, nil, true
	}
	return action, postFocus, false
}

// isSetupRow is whether id is a setting row (Rust's `Row(_)` pattern).
func isSetupRow(id SetupControl) bool {
	_, row := id.IsRow()
	return row
}

// isSetupComboDismissRow is the `ComboBoxDismiss(Row(_))` pattern.
func isSetupComboDismissRow(interaction chat.Interaction[SetupControl]) bool {
	dismiss, ok := interaction.(chat.InteractionComboBoxDismiss[SetupControl])
	return ok && isSetupRow(dismiss.ID)
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
