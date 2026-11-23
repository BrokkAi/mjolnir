// Port of mj-tui/src/setup/draft.rs.
//
// Rust's `pub(super) use mj_client::settings::draft::{config_from_draft,
// pointer, storage_path}` has no Go form: the setup files call
// client.ConfigFromDraft, client.Pointer and client.StoragePath directly.

package tui

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func incompleteSubagentNotice(task string, index int) string {
	return fmt.Sprintf("Choose a model for %s entry %d, or remove it.", client.Label(task), index+1)
}

// current is the value of the open page.
func (d *SetupDialog) current() any {
	value, ok := setupLookup(d.draft, d.path)
	if !ok {
		panic("settings path exists")
	}
	return value
}

// keys is the open page's visible keys (Rust test support).
func (d *SetupDialog) keys() []string {
	return client.VisibleKeys(d.path, d.current())
}

func (d *SetupDialog) paths() [][]string {
	return client.VisiblePaths(d.path, d.current())
}

// savedIdentityMatches is whether the draft still describes the profile that
// is saved on disk.
func (d *SetupDialog) savedIdentityMatches(profileID string) bool {
	saved, ok := d.savedProfileValues[profileID]
	if !ok {
		return false
	}
	profiles := setupValueGet(d.draft, "profiles")
	if !setupValueHas(profiles, profileID) {
		return false
	}
	return setupValueEqual(setupValueGet(profiles, profileID), saved)
}

// refreshSavedProfiles re-reads the saved profile shape after another writer
// changed the configuration while this dialog was open.
func (d *SetupDialog) refreshSavedProfiles(config *core.Config) {
	d.savedProfiles = make(map[string]struct{}, len(config.Profiles))
	for id := range config.Profiles {
		d.savedProfiles[id] = struct{}{}
	}
	_, saved := client.DraftOpen(config)
	d.savedProfileValues = map[string]any{}
	if profiles, ok := setupValueObject(setupValueGet(saved, "profiles")); ok {
		for _, member := range profiles {
			d.savedProfileValues[member.Key] = setupValueClone(member.Value)
		}
	}
}

func (d *SetupDialog) collection() bool {
	return client.IsCollection(d.path, d.current())
}

func (d *SetupDialog) descriptor(id SetupControl) (*SettingControl, bool) {
	for i := range d.descriptors {
		if d.descriptors[i].id == id {
			return &d.descriptors[i], true
		}
	}
	return nil, false
}

// focusedPath is the focused descriptor's path (Rust test support).
func (d *SetupDialog) focusedPath() ([]string, bool) {
	id, ok := d.form.Focused()
	if !ok {
		return nil, false
	}
	control, ok := d.descriptor(id)
	if !ok {
		return nil, false
	}
	return slices.Clone(control.path), true
}

// focusedFieldPath: a cached buffer may also back a Page button on a
// collection page. Only the current descriptor decides whether focus belongs
// to an editor.
func (d *SetupDialog) focusedFieldPath() ([]string, bool) {
	id, ok := d.form.Focused()
	if !ok {
		return nil, false
	}
	control, ok := d.descriptor(id)
	if !ok {
		return nil, false
	}
	if _, field := control.kind.(SettingKindField); !field {
		return nil, false
	}
	return slices.Clone(control.path), true
}

func (d *SetupDialog) focusPath(path []string) {
	d.prepare()
	for _, control := range d.descriptors {
		if slices.Equal(control.path, path) {
			d.form.Focus(control.id)
			d.form.RevealFocused()
			return
		}
	}
}

func (d *SetupDialog) openPage(path []string) {
	d.reviewEditor = nil
	d.path = slices.Clone(path)
	d.combo = chat.ComboBoxState[SetupControl]{}
	d.form = chat.NewDialog[SetupControl]()
}

func (d *SetupDialog) togglePath(path []string) {
	notice, err := client.Toggle(&d.draft, path)
	if err != nil {
		panic(fmt.Sprintf("declared checkbox exists: %v", err))
	}
	if len(path) == 3 && path[0] == "profiles" && path[2] == "enabled" {
		d.invalidateReviewValidationFor(&path[1])
	}
	if notice != nil {
		d.notice = notice
	}
}

func (d *SetupDialog) openSearch() {
	search := newSearchState(d.draft)
	d.search = &search
	d.form = chat.NewDialog[SetupControl]()
}

func (d *SetupDialog) closeSearch() {
	d.search = nil
	d.form = chat.NewDialog[SetupControl]()
}

// jumpTo: search navigates to pages and only focuses scalar controls.
func (d *SetupDialog) jumpTo(path []string) Jump {
	if len(path) == 0 {
		return JumpDone
	}
	parent := path[:len(path)-1]
	d.closeSearch()
	if slices.Equal(path, []string{"review"}) {
		return JumpOpenReview
	}
	if len(path) >= 3 && isSubagentCategoryPage(path[:2]) {
		d.openPage(path[:2])
		modelPath := []string{"subagents", path[1], path[2], "model"}
		d.prepare()
		if len(path) == 4 && slices.ContainsFunc(d.descriptors, func(control SettingControl) bool {
			return slices.Equal(control.path, path)
		}) {
			d.focusPath(path)
		} else {
			d.focusPath(modelPath)
		}
		return JumpDone
	}
	target, found := setupLookup(d.draft, path)
	targetIsPage := found && (setupValueIsObject(target) || setupValueIsArray(target))
	parentValue, found := setupLookup(d.draft, parent)
	targetIsCollectionItem := found && client.IsCollection(parent, parentValue)
	if targetIsPage || targetIsCollectionItem {
		d.openPage(path)
	} else {
		d.openPage(setupPageForPath(path))
		d.focusPath(path)
	}
	return JumpDone
}

// setupHasPrefix is Rust's slice starts_with: prefix is path's first
// segments.
func setupHasPrefix(path, prefix []string) bool {
	return len(path) >= len(prefix) && slices.Equal(path[:len(prefix)], prefix)
}

// incompleteSubagentRow is the first subagent entry without a model, in task
// when one is given and in every category otherwise.
func (d *SetupDialog) incompleteSubagentRow(task *string) (string, int, bool) {
	categories := SubagentCategories[:]
	if task != nil {
		categories = []string{*task}
	}
	for _, category := range categories {
		entries, ok := setupValueArray(setupValuePath(d.draft, "subagents", category))
		if !ok {
			continue
		}
		for index, entry := range entries {
			model, ok := setupValueString(setupValueGet(entry, "model"))
			if !ok || strings.TrimSpace(model) == "" {
				return category, index, true
			}
		}
	}
	return "", 0, false
}

// back leaves the editor or the page, and reports whether the dialog is at
// its root and should close.
func (d *SetupDialog) back() bool {
	if d.editor != nil {
		d.editor = nil
		d.form = chat.NewDialog[SetupControl]()
		return false
	}
	if isSubagentCategoryPage(d.path) {
		var pending [][]string
		for _, path := range d.fields.paths() {
			field, _ := d.fields.get(path)
			if setupHasPrefix(path, d.path) && field.input.text().Value() != field.applied {
				pending = append(pending, path)
			}
		}
		var firstInvalid []string
		for _, path := range pending {
			if d.applyPending(path) != nil && firstInvalid == nil {
				firstInvalid = path
			}
		}
		if firstInvalid != nil {
			d.focusPath(firstInvalid)
			return false
		}
		category := d.path[1]
		if task, index, ok := d.incompleteSubagentRow(&category); ok {
			modelPath := []string{"subagents", task, strconv.Itoa(index), "model"}
			d.focusPath(modelPath)
			notice := incompleteSubagentNotice(task, index)
			d.notice = &notice
			return false
		}
	}
	child := slices.Clone(d.path)
	if len(d.path) == 0 {
		return true
	}
	d.path = slices.Clone(d.path[:len(d.path)-1])
	d.combo = chat.ComboBoxState[SetupControl]{}
	d.form = chat.NewDialog[SetupControl]()
	d.focusPath(child)
	return false
}

// add adds an entry to the open collection, and returns the new subagent
// row's model path to focus.
func (d *SetupDialog) add() ([]string, bool) {
	if !d.collection() {
		return nil, false
	}
	if isSubagentCategoryPage(d.path) {
		path, err := client.EditAddEntry(&d.draft, d.path, nil)
		if err != nil {
			panic(fmt.Sprintf("a list accepts a new entry: %v", err))
		}
		path = append(path, "model")
		d.combo = chat.ComboBoxState[SetupControl]{}
		d.notice = nil
		return path, true
	}
	if setupValueIsArray(d.current()) {
		path, err := client.EditAddEntry(&d.draft, d.path, nil)
		if err != nil {
			panic(fmt.Sprintf("a list accepts a new entry: %v", err))
		}
		d.openPage(path)
	} else {
		d.editor = &Editor{
			path:   slices.Clone(d.path),
			input:  editorInputText(chat.NewTextInput()),
			adding: true,
		}
		d.form = chat.NewDialog[SetupControl]()
	}
	return nil, false
}

func (d *SetupDialog) rebindFieldsAfterRemoval(parent []string, removal *client.Removal) {
	rebound := setupFields{}
	for _, path := range d.fields.paths() {
		field, _ := d.fields.get(path)
		if slices.ContainsFunc(removal.Removed, func(removed []string) bool { return setupHasPrefix(path, removed) }) {
			continue
		}
		if removal.ListIndex != nil && setupHasPrefix(path, parent) && len(path) > len(parent) {
			if index, err := strconv.ParseUint(path[len(parent)], 10, 64); err == nil && index > uint64(*removal.ListIndex) {
				path[len(parent)] = strconv.FormatUint(index-1, 10)
			}
		}
		rebound[setupPathKey(path)] = field
	}
	d.fields = rebound
}

// removeSubagentEntry removes row index from the open subagent category page
// and returns the next row's model path to focus, or false when the list is
// now empty.
func (d *SetupDialog) removeSubagentEntry(index int) ([]string, bool) {
	if !isSubagentCategoryPage(d.path) {
		return nil, false
	}
	parent := slices.Clone(d.path)
	entryPath := []string{parent[0], parent[1], strconv.Itoa(index)}
	removal, err := client.RemoveEntry(&d.draft, entryPath)
	if err != nil {
		refusal := err.Error()
		d.notice = &refusal
		return nil, false
	}
	d.notice = removal.Notice
	d.rebindFieldsAfterRemoval(parent, &removal)
	d.combo = chat.ComboBoxState[SetupControl]{}
	d.descriptors = nil
	remaining := 0
	if entries, ok := setupValueArray(d.current()); ok {
		remaining = len(entries)
	}
	if remaining == 0 {
		return nil, false
	}
	return []string{parent[0], parent[1], strconv.Itoa(min(index, remaining-1)), "model"}, true
}

func (d *SetupDialog) applyEditor() error {
	if d.editor == nil {
		return nil
	}
	editor := *d.editor
	if editor.adding {
		name := editor.input.text().Value()
		newPath, err := client.EditAddEntry(&d.draft, editor.path, &name)
		if err != nil {
			return errors.New(err.Error())
		}
		d.editor = nil
		if len(editor.path) > 0 && editor.path[0] == "profiles" {
			profileID := newPath[1]
			if len(editor.path) > 1 {
				profileID = editor.path[1]
			}
			d.invalidateReviewValidationFor(&profileID)
		}
		d.openPage(newPath)
		return nil
	}
	return nil
}

func (d *SetupDialog) applyPending(path []string) error {
	field, ok := d.fields.get(path)
	if !ok {
		return nil
	}
	input := field.input
	err := d.applyValue(path, &input, nil)
	if field, ok := d.fields.get(path); ok {
		field.err = nil
		if err != nil {
			message := err.Error()
			field.err = &message
		}
	}
	if err != nil {
		d.form.RevealFocused()
	}
	return err
}

// applyValue: all scalar controls share the schema-aware write and its side
// effects. A nil choice applies the input's text.
func (d *SetupDialog) applyValue(path []string, input *EditorInput, choice *any) error {
	if d.buildCacheFieldUserManaged(path) {
		return errors.New("This machine's mbx installation owns its budgets. Mjolnir does not modify them.")
	}
	edited := editorInputText(chat.NewTextInput())
	if input != nil {
		edited = *input
	}
	old, oldOK := setupLookup(d.draft, path)
	old = setupValueClone(old)
	var setting client.SettingInput = client.SettingInputText{Value: edited.text().Value()}
	if choice != nil {
		setting = client.SettingInputChoice{Value: *choice}
	}
	rewritten, err := client.ApplyValue(&d.draft, path, setting)
	if err != nil {
		return errors.New(err.Error())
	}
	if rewritten != nil {
		edited.text().SetValue(*rewritten)
	}
	current, currentOK := setupLookup(d.draft, path)
	if changed := !setupValueOptionEqual(current, currentOK, old, oldOK); changed && len(path) > 0 && path[0] == "profiles" {
		d.invalidateReviewValidationFor(setupPathSegment(path, 1))
	}
	if field, ok := d.fields.get(path); ok {
		field.input = edited
		field.applied = field.input.String()
		source, ok := setupLookup(d.draft, path)
		if !ok {
			panic("applied field exists")
		}
		field.source = setupValueClone(source)
		field.err = nil
	}
	// Kind edits may remove fields; reconcile pending buffers deliberately.
	d.retainExistingFields()
	return nil
}

// acceptsStagedEdits is whether edits made on the person's behalf can join
// this dialog now: not while they type a value, search, edit review
// settings, or save.
func (d *SetupDialog) acceptsStagedEdits() bool {
	if d.saving || d.editor != nil || d.search != nil || d.reviewEditor != nil {
		return false
	}
	for _, field := range d.fields {
		if field.input.text().Value() != field.applied {
			return false
		}
	}
	return true
}

// stageEdits stages edits made on the person's behalf (`mj ui settings`)
// through the shared settings model, all or none, and returns their page
// paths.
func (d *SetupDialog) stageEdits(edits []client.SettingEdit) ([][]string, error) {
	paths, err := client.StageAll(&d.draft, edits)
	if err != nil {
		return nil, err
	}
	for _, path := range paths {
		// A field's typing buffer would otherwise show its old text.
		d.fields.remove(path)
		if len(path) > 0 && path[0] == "profiles" {
			d.invalidateReviewValidationFor(setupPathSegment(path, 1))
		}
	}
	d.retainExistingFields()
	return paths, nil
}

// retainExistingFields drops the buffers of fields the draft no longer has
// (Rust's `fields.retain(|path, _| draft.pointer(..).is_some())`).
func (d *SetupDialog) retainExistingFields() {
	for key := range d.fields {
		if _, ok := setupLookup(d.draft, setupKeyPath(key)); !ok {
			delete(d.fields, key)
		}
	}
}

// setupPathSegment is `path.get(index).map(String::as_str)`.
func setupPathSegment(path []string, index int) *string {
	if index < 0 || index >= len(path) {
		return nil
	}
	return &path[index]
}

func (d *SetupDialog) save(capabilities *core.ProfileCapabilitiesSnapshot) DashboardAction {
	if _, resolving := d.pendingSave.(PendingSaveResolving); d.saving || resolving {
		return DashboardActionNone{}
	}
	d.pendingSave = PendingSaveIdle{}
	d.reconcileHostOwnedFields()
	var pending [][]string
	for _, path := range d.fields.paths() {
		field, _ := d.fields.get(path)
		if field.input.text().Value() != field.applied {
			pending = append(pending, path)
		}
	}
	var firstInvalid []string
	for _, path := range pending {
		// Target-home expansion is asynchronous, just as it is for Enter.
		resolve, err := d.resolvePathAction(path)
		switch {
		case err != nil:
			field, _ := d.fields.get(path)
			message := err.Error()
			field.err = &message
			if firstInvalid == nil {
				firstInvalid = path
			}
			continue
		case resolve != nil:
			if firstInvalid == nil {
				field, _ := d.fields.get(path)
				d.pendingSave = PendingSaveResolving{
					path:  slices.Clone(path),
					value: field.input.String(),
					draft: setupValueClone(d.draft),
				}
				return resolve
			}
			continue
		}
		if d.applyPending(path) != nil && firstInvalid == nil {
			firstInvalid = path
		}
	}
	if firstInvalid != nil {
		d.openPage(setupPageForPath(firstInvalid))
		d.focusPath(firstInvalid)
		return DashboardActionNone{}
	}
	if task, index, ok := d.incompleteSubagentRow(nil); ok {
		category := []string{"subagents", task}
		modelPath := []string{"subagents", task, strconv.Itoa(index), "model"}
		if !slices.Equal(d.path, category) {
			d.openPage(category)
		}
		d.focusPath(modelPath)
		notice := incompleteSubagentNotice(task, index)
		d.notice = &notice
		return DashboardActionNone{}
	}
	config, err := client.ConfigFromDraft(d.draft)
	var failure *string
	if err != nil {
		message := err.Error()
		failure = &message
	} else if d.reviewValidation != nil {
		// Full validation reads profile files and needs resolved secrets.
		// The background save owns it; this is only a sources projection.
		if message, invalid := reviewSettingsDialogValidationError(d.reviewValidation, &config.Review); invalid {
			failure = &message
		}
	}
	if failure != nil {
		d.notice = failure
		return DashboardActionNone{}
	}
	d.saving = true
	notice := "Saving settings…"
	d.notice = &notice
	encoded, err := json.Marshal(config)
	if err != nil {
		panic(fmt.Sprintf("config serializes: %v", err))
	}
	updated := string(encoded)
	if profileID := d.pendingLogin; profileID != nil {
		d.pendingLogin = nil
		return DashboardActionSaveSetupAndLogin{
			Generation: d.generation,
			Original:   d.original,
			Updated:    updated,
			ProfileID:  *profileID,
		}
	}
	return DashboardActionSaveSetup{
		Generation: d.generation,
		Original:   d.original,
		Updated:    updated,
	}
}

// isDirty compares only values that can be persisted to the config file. The
// expanded schema and all navigation/discovery fields are transient UI state
// and must not make a freshly opened Setup dialog dirty.
func (d *SetupDialog) isDirty() bool {
	for _, field := range d.fields {
		if field.input.text().Value() != field.applied {
			return true
		}
	}
	if d.editor != nil && !d.editor.input.text().IsEmpty() {
		return true
	}
	if _, _, incomplete := d.incompleteSubagentRow(nil); incomplete {
		return true
	}
	original, originalErr := core.WithEnvironmentSourcesOnly(func() setupConfigRead {
		var config core.Config
		err := json.Unmarshal([]byte(d.original), &config)
		return setupConfigRead{config: config, err: err}
	}).unpack()
	current, currentErr := client.ConfigFromDraft(d.draft)
	if originalErr != nil || currentErr != nil {
		return true
	}
	return !setupConfigEqual(original, current)
}

// setupConfigRead carries a decoded configuration out of a scoped read.
type setupConfigRead struct {
	config core.Config
	err    error
}

func (r setupConfigRead) unpack() (core.Config, error) { return r.config, r.err }

// setupConfigEqual is Config's derived PartialEq. It compares the stored
// form, which is how both sides are written and read, so a nil and an empty
// collection compare equal as Rust's do, and then the sets the file does not
// store.
func setupConfigEqual(a, b core.Config) bool {
	left, errLeft := json.Marshal(a)
	right, errRight := json.Marshal(b)
	return errLeft == nil && errRight == nil && bytes.Equal(left, right) &&
		maps.Equal(a.DefaultTargets, b.DefaultTargets) && maps.Equal(a.DisabledTargets, b.DisabledTargets)
}

func (d *SetupDialog) openReview(dashboard *DashboardState) DashboardAction {
	config, err := client.ConfigFromDraft(d.draft)
	if err != nil {
		notice := fmt.Sprintf("Fix the invalid settings draft before opening Code Review: %s", err.Error())
		d.notice = &notice
		d.form = chat.NewDialog[SetupControl]()
		d.prepare()
		return DashboardActionNone{}
	}
	review := newReviewSettingsDialog(&config)
	review.updateChoices(&config, &dashboard.profileCapabilities, dashboard.usagePricedProfiles(&config))
	d.syncReviewValidation(review)
	d.reviewEditor = review
	d.form = chat.NewDialog[SetupControl]()
	d.prepare()
	return DashboardActionNone{}
}

// syncReview writes review into the draft. The saved form leaves defaults
// out; the draft keeps every field, so the page reads the same before and
// after a visit.
func (d *SetupDialog) syncReview(review *core.ReviewConfig) {
	value, err := setupValueTo(review)
	if err != nil {
		panic(fmt.Sprintf("review serializes: %v", err))
	}
	path := []string{"review"}
	client.Expand(&value, &path)
	setupValueInsert(&d.draft, nil, "review", value)
}

func (d *SetupDialog) syncReviewValidation(review *ReviewSettingsDialog) {
	d.syncReview(&review.review)
	snapshot := review.validationSnapshot()
	d.reviewValidation = &snapshot
}

func (d *SetupDialog) invalidateReviewValidationFor(profileID *string) {
	if profileID != nil || d.reviewValidation != nil {
		d.reviewValidation = nil
	}
}

func (d *SetupDialog) beginManagedSetup() {
	d.editor = nil
	d.search = nil
	d.path = []string{"profiles"}
	d.combo = chat.ComboBoxState[SetupControl]{}
	first := 0
	d.managedSetup = &first
	d.notice = nil
	d.form = chat.NewDialog[SetupControl]()
	d.prepare()
}

func (d *SetupDialog) leaveManagedSetup() {
	d.managedSetup = nil
	d.form = chat.NewDialog[SetupControl]()
	d.prepare()
}

func (d *SetupDialog) createManagedProfileAndLogin(capabilities *core.ProfileCapabilitiesSnapshot) DashboardAction {
	if d.managedSetup == nil {
		return DashboardActionNone{}
	}
	state := *d.managedSetup
	if state < 0 || state >= len(managedSetupHarnesses) {
		notice := "Choose an agent to set up."
		d.notice = &notice
		return DashboardActionNone{}
	}
	kind := managedSetupHarnesses[state]
	profileID := d.availableProfileID(kind.ID())
	home := kind.ManagedProfileHome(profileID)
	// The kind's own defaults, so the profile holds only fields its kind
	// reads, the same as a profile added on the Profiles page.
	profile := client.Defaults(
		[]string{"profiles", profileID},
		jsonvalue.Object{{Key: "kind", Value: string(kind)}},
	)
	setupValueInsert(&profile, nil, "home", home)
	setupValueInsert(&d.draft, []string{"profiles"}, profileID, profile)
	d.managedSetup = nil
	notice := fmt.Sprintf("Created %s. Next, Mjolnir prepares it and opens sign-in.", kind.DisplayName())
	d.notice = &notice
	d.pendingLogin = &profileID
	return d.save(capabilities)
}

func (d *SetupDialog) availableProfileID(base string) string {
	profiles, ok := setupValueObject(setupValueGet(d.draft, "profiles"))
	if !ok {
		panic("profiles is an object")
	}
	if _, found := profiles.Get(base); !found {
		return base
	}
	for suffix := 2; ; suffix++ {
		candidate := fmt.Sprintf("%s-%d", base, suffix)
		if _, found := profiles.Get(candidate); !found {
			return candidate
		}
	}
}
