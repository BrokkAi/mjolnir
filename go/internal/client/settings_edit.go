// Port of mj-client/src/settings/edit.rs.

// How an edit changes a settings draft. Every surface applies edits through
// these functions, so a value means the same thing whichever one typed it.
//
// Rust returns Result<_, String> here; the Go error's text is that string.

package client

func clearReviewLegacy(draft *any) {
	panic("unported: mj-client/src/settings/edit.rs clear_review_legacy")
}

// SettingInput is what a person gave a setting: text typed into a field, or
// one of the values the setting offers.
type SettingInput interface {
	isSettingInput()
}

type SettingInputText struct{ Value string }

// SettingInputChoice is one of the setting's offered values (a jsonvalue
// value).
type SettingInputChoice struct{ Value any }

func (SettingInputText) isSettingInput()   {}
func (SettingInputChoice) isSettingInput() {}

// ApplyValue sets the scalar at path from what was typed or chosen. Returns
// the text the field now holds when applying it rewrote the text, as
// expanding `~` in a local path does.
func ApplyValue(draft *any, path []string, input SettingInput) (*string, error) {
	panic("unported: mj-client/src/settings/edit.rs apply_value")
}

// Stage sets the setting at page path to value the way a person would in
// Settings: a checkbox is ticked or cleared, a listed value is chosen, and
// anything else is typed, so it is checked and rewritten exactly as typing
// it would be. A nil value (JSON null) clears the field.
func Stage(draft *any, path []string, value any) error {
	panic("unported: mj-client/src/settings/edit.rs stage")
}

// StageAll stages every edit, or none: the draft changes only when each edit
// applies and the result is a configuration that could be saved. Paths may
// use the stored spelling of an Interface setting (`theme`, `keys.prefix`).
// Returns the page path of each edit, in order.
func StageAll(draft *any, edits []SettingEdit) ([][]string, error) {
	panic("unported: mj-client/src/settings/edit.rs stage_all")
}

// Toggle flips the checkbox at path. Disabling a profile also clears the
// places that name it, so the saved configuration never points at a profile
// that cannot run; the returned notice says what was cleared.
func Toggle(draft *any, path []string) (*string, error) {
	panic("unported: mj-client/src/settings/edit.rs toggle")
}

// EditAddEntry is Rust's settings::edit::add_entry (renamed: settings/view's
// AddEntry type keeps the name). It adds an entry to the collection at
// collection. A list grows by one default entry; a named collection needs
// the new entry's name. Returns the new entry's path.
func EditAddEntry(draft *any, collection []string, name *string) ([]string, error) {
	panic("unported: mj-client/src/settings/edit.rs add_entry")
}

// Removal is what removing an entry took with it.
type Removal struct {
	// Removed is every path that no longer exists: the entry, and the
	// runtimes of a removed machine.
	Removed [][]string
	// ListIndex is the removed entry's index when it was in a list, whose
	// later entries have each moved up by one.
	ListIndex *int
	Notice    *string
}

// RuntimeIDsOnMachine is the runtimes that run on machineID.
func RuntimeIDsOnMachine(draft any, machineID string) []string {
	panic("unported: mj-client/src/settings/edit.rs runtime_ids_on_machine")
}

// Removable is whether the entry at path is one a person may remove.
func Removable(draft any, path []string) bool {
	panic("unported: mj-client/src/settings/edit.rs removable")
}

// RemoveEntry removes the collection entry at path.
func RemoveEntry(draft *any, path []string) (Removal, error) {
	panic("unported: mj-client/src/settings/edit.rs remove_entry")
}
