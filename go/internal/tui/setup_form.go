// Port of mj-tui/src/setup/form.rs.

package tui

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupButton is one action of the Settings dialog (Rust's
// `(SetupControl, &'static str, bool)`).
func setupButton(id SetupControl, label string, enabled bool) chat.ButtonSpec[SetupControl] {
	return chat.ButtonSpec[SetupControl]{ID: id, Label: label, Enabled: enabled}
}

// actions is the one action set for the current screen. Buttons appear only
// on the pages where they apply; the renderer, the inert mirror behind a
// choice popup, and prepare all read this list so they cannot drift apart.
func (d *SetupDialog) actions() []chat.ButtonSpec[SetupControl] {
	// The search covers the page, so the page's own actions go with it and
	// Back means "leave the search" rather than "leave this page".
	if d.search != nil {
		return []chat.ButtonSpec[SetupControl]{
			setupButton(SetupControlBack, "Back", true),
			setupButton(SetupControlSave, d.saveLabel(), !d.saving),
		}
	}
	// The Detection results view reads its blocks, not the page; Back here
	// means "leave the report".
	if d.report != nil {
		return []chat.ButtonSpec[SetupControl]{
			setupButton(SetupControlBack, "Back", true),
			setupButton(SetupControlSave, d.saveLabel(), !d.saving),
		}
	}
	if d.managedSetup != nil {
		return []chat.ButtonSpec[SetupControl]{
			setupButton(SetupControlBack, "Back", true),
			setupButton(SetupControlManagedContinue, "Set up and sign in", !d.saving),
		}
	}
	if d.editor != nil {
		return []chat.ButtonSpec[SetupControl]{
			setupButton(SetupControlBack, "Back", true),
			setupButton(SetupControlApply, "Apply", true),
		}
	}
	// The list page stays visible behind a choice popup, but its item and
	// machine actions must not be reachable through it.
	interactive := d.editor == nil
	collection := d.collection()
	var actions []chat.ButtonSpec[SetupControl]
	if len(d.path) > 0 {
		actions = append(actions, setupButton(SetupControlBack, "Back", true))
	}
	if collection {
		// The projects page creates a bundle rather than adding a bare
		// entry, so its button says what it makes.
		addLabel := "Add"
		if slices.Equal(d.path, []string{"bundles"}) {
			addLabel = "Create"
		}
		actions = append(actions, setupButton(SetupControlAdd, addLabel, interactive))
	}
	if len(d.path) > 0 {
		key, parent := d.path[len(d.path)-1], d.path[:len(d.path)-1]
		if value, ok := setupLookup(d.draft, parent); ok && client.IsCollection(parent, value) {
			local := slices.Equal(parent, []string{"machines"}) && key == core.LocalMachineID
			actions = append(actions, setupButton(SetupControlRemove, "Remove", interactive && !local))
		}
	}
	// Each detection writes only the section it belongs to, so each button
	// appears on that section's page alone.
	if len(d.path) == 1 {
		switch d.path[0] {
		case "profiles":
			actions = append(actions, setupButton(SetupControlDetectProfiles, "Detect profiles",
				interactive && !d.discovering && !d.saving))
		case "targets":
			actions = append(actions, setupButton(SetupControlDetectRuntimes, "Detect runtimes",
				interactive && !d.discovering && !d.saving))
		}
	}
	// A runtime's machine is an SSH host or template added on the Machines
	// page; offer the jump from where the need shows up.
	if slices.Equal(d.path, []string{"targets"}) {
		actions = append(actions, setupButton(SetupControlOpenMachines, "Machines", interactive))
	}
	// The report of what a runtimes detection turned down stays readable
	// from its page until Settings closes.
	if slices.Equal(d.path, []string{"targets"}) && len(d.detectionRejected) > 0 {
		actions = append(actions, setupButton(SetupControlDetectionResults, "Detection results",
			interactive && !d.saving))
	}
	if slices.Equal(d.path, []string{"profiles"}) {
		actions = append(actions, setupButton(SetupControlManagedSetup, "Set up managed",
			interactive && !d.discovering && !d.saving))
	}
	// Login operates on a saved profile identified by this detail page.
	if len(d.path) == 2 && d.path[0] == "profiles" {
		profileID := d.path[1]
		_, saved := d.savedProfiles[profileID]
		unchanged := d.savedIdentityMatches(profileID)
		if unchanged {
			for key, field := range d.fields {
				if setupHasPrefix(setupKeyPath(key), d.path) && field.input.text().Value() != field.applied {
					unchanged = false
					break
				}
			}
		}
		enabled := true
		if flag, ok := setupValueBool(setupValuePath(d.draft, "profiles", profileID, "enabled")); ok {
			enabled = flag
		}
		_, notRequired := d.loginStatuses[profileID].(core.LoginStatusNotRequired)
		needsLogin := !notRequired
		actions = append(actions, setupButton(SetupControlSignIn, "Sign in",
			interactive && saved && unchanged && enabled && needsLogin && !d.discovering && !d.saving))
	}
	if label, enabled, ok := d.installMbxPageAction(); ok {
		actions = append(actions, setupButton(SetupControlInstallMbx, label, interactive && enabled && !d.saving))
	}
	actions = append(actions, setupButton(SetupControlSave, d.saveLabel(), !d.saving))
	return actions
}

func (d *SetupDialog) saveLabel() string {
	if d.saving {
		return "Saving…"
	}
	return "Save and Close"
}

// footerActions are the actions drawn in the dialog's footer row: the way
// back out of a page and the commit that closes the dialog.
func (d *SetupDialog) footerActions() []chat.ButtonSpec[SetupControl] {
	var footer []chat.ButtonSpec[SetupControl]
	for _, action := range d.actions() {
		if action.ID == SetupControlBack || action.ID == SetupControlSave {
			footer = append(footer, action)
		}
	}
	return footer
}

// pageActions are the actions that belong to the page itself, stacked in the
// column at the dialog's right edge.
func (d *SetupDialog) pageActions() []chat.ButtonSpec[SetupControl] {
	var page []chat.ButtonSpec[SetupControl]
	for _, action := range d.actions() {
		if action.ID != SetupControlBack && action.ID != SetupControlSave {
			page = append(page, action)
		}
	}
	return page
}

func (d *SetupDialog) noticeView() setupNoticeView {
	return setupNoticeView{
		path:      append([]string{}, d.path...),
		editing:   d.editor != nil,
		searching: d.search != nil,
	}
}

// expireNotice anchors a new notice to the view showing it, and drops a
// notice once the dialog has left that view.
func (d *SetupDialog) expireNotice() {
	if d.notice == nil {
		d.noticeAnchor = nil
		return
	}
	text := *d.notice
	view := d.noticeView()
	if d.noticeAnchor != nil && d.noticeAnchor.text == text {
		if !d.noticeAnchor.view.equal(view) {
			d.notice = nil
			d.noticeAnchor = nil
		}
		return
	}
	d.noticeAnchor = &NoticeAnchor{text: text, view: view}
}

// choicePosition is the index of the draft's value at path among values,
// or 0 when the draft holds none of them.
func (d *SetupDialog) choicePosition(path []string, values []any) int {
	current, ok := setupLookup(d.draft, path)
	if !ok {
		return 0
	}
	for index, value := range values {
		if setupValueEqual(current, value) {
			return index
		}
	}
	return 0
}

// prepare declares the controls of the current screen on the form.
func (d *SetupDialog) prepare() {
	if d.reviewEditor != nil {
		d.reviewEditor.prepare()
		return
	}
	d.reconcileHostOwnedFields()
	d.expireNotice()
	if search := d.search; search != nil {
		count := len(search.matches)
		selected := search.selected
		query := search.input.Value()
		actions := d.actions()
		form := &d.form
		form.BeginUpdate()
		form.Declare(SetupControlSearch, chat.ControlKindTextField{})
		form.DeclareWithEnabled(SetupControlResults, chat.ControlKindChoiceList{Len: count, Selected: selected}, count > 0)
		// A new query is a new list, so the viewport starts at its top
		// rather than wherever the previous results were scrolled to.
		form.SetListIdentity(SetupControlResults, "search/"+query)
		form.SetListActivation(SetupControlResults, chat.ListActivationSingleClick)
		form.SetMenu(true)
		for _, action := range actions {
			form.DeclareWithEnabled(action.ID, chat.ControlKindButton{}, action.Enabled)
		}
		form.EndFrame(SetupControlSearch)
		return
	}
	if d.report != nil {
		// Only the footer actions belong to the report; the page's controls
		// stay undeclared so focus cannot land on them.
		actions := d.actions()
		form := &d.form
		form.BeginUpdate()
		for _, action := range actions {
			form.DeclareWithEnabled(action.ID, chat.ControlKindButton{}, action.Enabled)
		}
		form.EndFrame(SetupControlBack)
		return
	}
	d.reconcileControls()
	actions := d.actions()
	form := &d.form
	form.BeginUpdate()
	openID, comboOpen := d.combo.OpenID()
	var initial SetupControl
	if d.editor != nil {
		form.Declare(SetupControlField, chat.ControlKindTextField{})
		initial = SetupControlField
	} else {
		for _, control := range d.descriptors {
			var kind chat.ControlKind
			switch settingKind := control.kind.(type) {
			case SettingKindPage, SettingKindRemoveEntry, SettingKindManagedHarness:
				kind = chat.ControlKindButton{}
			case SettingKindCheckbox:
				kind = chat.ControlKindCheckbox{}
			case SettingKindChoices:
				kind = chat.ControlKindComboBox{
					Len:      len(settingKind.values),
					Selected: d.combo.Selection(control.id, d.choicePosition(control.path, settingKind.values)),
					Expanded: d.combo.IsOpen(control.id),
				}
			case SettingKindField:
				field, ok := d.fields.get(control.path)
				if !ok {
					panic("a declared field has its buffer")
				}
				if input := field.input.pathInput(); input != nil {
					kind = input.ControlKind()
				} else {
					kind = chat.ControlKindTextField{}
				}
			case SettingKindReadOnly:
				continue
			}
			form.DeclareWithEnabled(control.id, kind, !comboOpen || openID == control.id)
		}
		initial = SetupControlSave
		if isSubagentCategoryPage(d.path) {
			initial = SetupControlAdd
		}
		for _, control := range d.descriptors {
			if _, readOnly := control.kind.(SettingKindReadOnly); !readOnly {
				initial = control.id
				break
			}
		}
	}
	for _, action := range actions {
		form.DeclareWithEnabled(action.ID, chat.ControlKindButton{}, action.Enabled && !comboOpen)
	}
	form.EndFrame(initial)
}

// Setup answers the dashboard's modal questions (ModalSurface) through
// whichever of its two forms is on top: the review-settings editor when it
// is open, the setup form otherwise.

func (d *SetupDialog) confirmationOpen() bool {
	if d.reviewEditor != nil {
		return d.reviewEditor.form.ConfirmationOpen()
	}
	return d.form.ConfirmationOpen()
}

func (d *SetupDialog) renderConfirmation(frame *chat.Frame, area uv.Rectangle, surfaces *chat.FrameSurfaces) {
	if d.reviewEditor != nil {
		d.reviewEditor.form.RenderConfirmation(frame, area, surfaces)
		return
	}
	d.form.RenderConfirmation(frame, area, surfaces)
}

func (d *SetupDialog) handlesMouse(column, row int) bool {
	if d.reviewEditor != nil {
		form := &d.reviewEditor.form
		return form.CapturesPointer() || form.Contains(column, row)
	}
	return d.form.CapturesPointer() || d.form.Contains(column, row)
}

func (d *SetupDialog) cancelPointer() bool {
	if d.reviewEditor != nil {
		form := &d.reviewEditor.form
		changed := form.CapturesPointer()
		form.CancelPointer()
		return changed
	}
	changed := d.form.CapturesPointer()
	d.form.CancelPointer()
	return changed
}

func (d *SetupDialog) resetGeometry() {
	if d.reviewEditor != nil {
		d.reviewEditor.form.ResetGeometry()
		return
	}
	d.form.ResetGeometry()
}

// textInputFocused: field ownership comes from this page's descriptor, not
// retained draft buffers.
func (d *SetupDialog) textInputFocused() bool {
	if d.form.IsFocused(SetupControlField) {
		return true
	}
	if _, ok := d.focusedFieldPath(); ok {
		return true
	}
	return d.search != nil && d.form.IsFocused(SetupControlSearch)
}

func (d *SetupDialog) prepareDialogState() {
	d.form.SetActionRole(SetupControlCancel, chat.ActionRoleCancel)
	d.form.SetActionRole(SetupControlBack, chat.ActionRoleBack)
	switch {
	case d.reviewEditor != nil:
		d.reviewEditor.prepareDialogState()
	case d.managedSetup != nil:
		d.form.SetDirty(false)
		d.form.SetDismissActions(nil)
		d.form.SetDefaultAction(SetupControlManagedContinue)
	case d.editor != nil:
		d.form.SetDirty(false)
		d.form.SetDismissActions(nil)
		d.form.SetDefaultAction(SetupControlApply)
	default:
		// The outer setup draft uses its normalized saved-config comparison.
		d.form.SetDirty(false)
		d.form.SetDismissActions(nil)
		d.form.SetDefaultAction(SetupControlSave)
	}
}

// layerDetail is Rust's `format!("{:?}/{:?}/{}", path, editor path,
// review open)`.
func (d *SetupDialog) layerDetail() string {
	editor := "None"
	if d.editor != nil {
		editor = "Some(" + setupDebugStrings(d.editor.path) + ")"
	}
	return fmt.Sprintf("%s/%s/%t", setupDebugStrings(d.path), editor, d.reviewEditor != nil)
}

// setupOpenChoice is the open combobox's control, the value its popup is on,
// and how many values it offered (Rust's `(SetupControl, Value, usize)`).
type setupOpenChoice struct {
	id     SetupControl
	value  any
	oldLen int
}

func (d *SetupDialog) reconcileControls() {
	previous := d.descriptors
	d.descriptors = nil
	previousID := func(matches func(control *SettingControl) bool) SetupControl {
		for i := range previous {
			if matches(&previous[i]) {
				return previous[i].id
			}
		}
		id := SetupControlRow(d.nextControl)
		d.nextControl++
		return id
	}
	if d.managedSetup != nil {
		descriptors := make([]SettingControl, 0, len(managedSetupHarnesses))
		for index, kind := range managedSetupHarnesses {
			path := []string{"_managed", kind.ID()}
			id := previousID(func(control *SettingControl) bool { return slices.Equal(control.path, path) })
			descriptors = append(descriptors, SettingControl{id: id, path: path, kind: SettingKindManagedHarness{index: index}})
		}
		d.descriptors = descriptors
		return
	}
	var openValue *setupOpenChoice
	if openID, ok := d.combo.OpenID(); ok {
		for i := range previous {
			control := &previous[i]
			if control.id != openID {
				continue
			}
			if choices, isChoices := control.kind.(SettingKindChoices); isChoices {
				committed := d.choicePosition(control.path, choices.values)
				if selection := d.combo.Selection(openID, committed); selection >= 0 && selection < len(choices.values) {
					openValue = &setupOpenChoice{id: openID, value: choices.values[selection], oldLen: len(choices.values)}
				}
			}
			break
		}
	}
	var paths [][]string
	if isSubagentCategoryPage(d.path) {
		task := d.path[1]
		if entries, ok := setupValueArray(d.current()); ok {
			for index, entry := range entries {
				base := []string{"subagents", task, strconv.Itoa(index)}
				paths = append(paths, append(slices.Clone(base), "model"))
				if model, ok := setupValueString(setupValueGet(entry, "model")); ok && strings.TrimSpace(model) != "" {
					paths = append(paths, append(slices.Clone(base), "effort"))
				}
				paths = append(paths, base)
			}
		}
	} else if current := d.current(); setupValueIsObject(current) || setupValueIsArray(current) {
		paths = d.paths()
	} else {
		paths = [][]string{slices.Clone(d.path)}
	}
	for _, path := range paths {
		value, ok := setupLookup(d.draft, path)
		if !ok {
			panic("visible setting exists")
		}
		var kind SettingKind
		_, isRow := subagentRowIndex(path)
		switch {
		case d.buildCacheFieldUserManaged(path):
			kind = SettingKindReadOnly{}
		case isSubagentCategoryPage(d.path) && isRow && len(path) == 3:
			kind = SettingKindRemoveEntry{}
		default:
			page := d.path
			if isSubagentCategoryPage(d.path) {
				page = path[:3]
			}
			switch rowKind := client.RowKindOf(d.draft, page, path).(type) {
			case client.RowKindPage:
				kind = SettingKindPage{}
			case client.RowKindCheckbox:
				kind = SettingKindCheckbox{}
			case client.RowKindChoices:
				choices := slices.Clone(rowKind.Value)
				if current, isText := setupValueString(value); isSubagentCategoryPage(d.path) && isText &&
					strings.TrimSpace(current) != "" &&
					!slices.ContainsFunc(choices, func(choice any) bool { return setupValueEqual(choice, value) }) {
					insertAt := 0
					if path[len(path)-1] == "effort" && len(choices) > 0 && jsonvalue.IsNull(choices[0]) {
						insertAt = 1
					}
					choices = slices.Insert(choices, insertAt, setupValueClone(value))
				}
				kind = SettingKindChoices{values: choices}
			case client.RowKindField:
				kind = SettingKindField{}
			}
		}
		id := previousID(func(control *SettingControl) bool {
			return slices.Equal(control.path, path) && sameSettingKindVariant(control.kind, kind)
		})
		if _, isField := kind.(SettingKindField); isField {
			text := client.FieldText(path, value)
			field, ok := d.fields.get(path)
			if !ok {
				field = &PendingField{
					input:   fieldInput(path, text),
					applied: text,
					source:  setupValueClone(value),
				}
				d.fields[setupPathKey(path)] = field
			}
			// Background updates refresh untouched fields, never a pending
			// edit.
			if field.input.text().Value() == field.applied && !setupValueEqual(field.source, value) {
				field.input = fieldInput(path, text)
				field.applied = text
				field.source = setupValueClone(value)
				field.err = nil
			}
		}
		if openValue != nil && openValue.id == id {
			if choices, isChoices := kind.(SettingKindChoices); isChoices {
				target := openValue.value
				if openValue.oldLen == 1 {
					target, _ = setupLookup(d.draft, path)
				}
				position := slices.IndexFunc(choices.values, func(choice any) bool { return setupValueEqual(choice, target) })
				d.combo.Open(id, max(position, 0))
			}
		}
		d.descriptors = append(d.descriptors, SettingControl{id: id, path: path, kind: kind})
	}
	if openID, ok := d.combo.OpenID(); ok && !slices.ContainsFunc(d.descriptors, func(control SettingControl) bool {
		return control.id == openID
	}) {
		d.combo = chat.ComboBoxState[SetupControl]{}
	}
	d.retainExistingFields()
}

// emptyFieldHint is the placeholder an empty field shows.
func (d *SetupDialog) emptyFieldHint(path []string) (string, bool) {
	if len(path) == 0 {
		return "", false
	}
	key := path[len(path)-1]
	parent := path[:len(path)-1]
	parentValue, ok := setupLookup(d.draft, parent)
	if !ok {
		return "", false
	}
	defaults := client.Defaults(parent, parentValue)
	if len(parent) > 0 && parent[len(parent)-1] == "environment" {
		return "", false
	}
	if slices.Equal(path, []string{"interface", "prefix"}) {
		return core.DefaultPrefix, true
	}
	if hint, ok := d.buildCacheAutomaticLabel(key); ok {
		return hint, true
	}
	if hint, ok := d.archiveSpaceAutomaticLabel(key); ok {
		return hint, true
	}
	if number, ok := client.WholeNumberOf(path); ok && number.Defaulted {
		if setupValueHas(defaults, key) {
			if value := setupValueGet(defaults, key); !jsonvalue.IsNull(value) {
				return client.FieldText(path, value), true
			}
		}
		return "Default", true
	}
	if setupValueHas(defaults, key) && jsonvalue.IsNull(setupValueGet(defaults, key)) {
		return "Default", true
	}
	return "", false
}

func (d *SetupDialog) choiceLabel(path []string, value any) string {
	return client.OptionLabel(path, value, d.draft)
}

func fieldInput(path []string, text string) EditorInput {
	if _, ok := client.PathKindOf(path); ok {
		return editorInputPath(chat.PathInputFromValue(text))
	}
	return editorInputText(chat.TextInputFromValue(text))
}
