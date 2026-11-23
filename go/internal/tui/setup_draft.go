// Port of mj-tui/src/setup/draft.rs.
//
// Rust's `pub(super) use mj_client::settings::draft::{config_from_draft,
// pointer, storage_path}` has no Go form: the setup files call
// client.ConfigFromDraft, client.Pointer and client.StoragePath directly.

package tui

import (
	"fmt"
	"slices"
	"strings"

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
	d.path = path
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
	panic("unported: mj-tui/src/setup/draft.rs jump_to")
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
	panic("unported: mj-tui/src/setup/draft.rs back")
}

// add adds an entry to the open collection, and returns the new subagent
// row's model path to focus.
func (d *SetupDialog) add() ([]string, bool) {
	panic("unported: mj-tui/src/setup/draft.rs add")
}

func (d *SetupDialog) rebindFieldsAfterRemoval(parent []string, removal *client.Removal) {
	panic("unported: mj-tui/src/setup/draft.rs rebind_fields_after_removal")
}

// removeSubagentEntry removes row index from the open subagent category page
// and returns the next row's model path to focus, or false when the list is
// now empty.
func (d *SetupDialog) removeSubagentEntry(index int) ([]string, bool) {
	panic("unported: mj-tui/src/setup/draft.rs remove_subagent_entry")
}

func (d *SetupDialog) applyEditor() error {
	panic("unported: mj-tui/src/setup/draft.rs apply_editor")
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
	panic("unported: mj-tui/src/setup/draft.rs apply_value")
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
	panic("unported: mj-tui/src/setup/draft.rs save")
}

// isDirty compares only values that can be persisted to the config file. The
// expanded schema and all navigation/discovery fields are transient UI state
// and must not make a freshly opened Setup dialog dirty.
func (d *SetupDialog) isDirty() bool {
	panic("unported: mj-tui/src/setup/draft.rs is_dirty")
}

func (d *SetupDialog) openReview(dashboard *DashboardState) DashboardAction {
	panic("unported: mj-tui/src/setup/draft.rs open_review")
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
	panic("unported: mj-tui/src/setup/draft.rs sync_review_validation")
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
	panic("unported: mj-tui/src/setup/draft.rs create_managed_profile_and_login")
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
