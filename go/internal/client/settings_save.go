// Port of mj-client/src/settings/save.rs.

// Writing a settings draft to the configuration file. Every surface saves
// through here, so a save means the same thing wherever it was made.

package client

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SaveAt saves the edit from original to updated into the configuration at
// path. Fields the edit left alone keep whatever the file holds now, so
// another client's change to them survives; a field both changed is
// refused.
func SaveAt(path string, original, updated string, state *core.StateState) (core.Config, error) {
	panic("unported: mj-client/src/settings/save.rs save_at")
}

// MergeEdit applies only fields the dialog changed and refuses conflicting
// concurrent edits. Each value is Rust's Option<&Value>: nil is absent,
// which differs from a present JSON null.
func MergeEdit(original, updated, current *any, path string) (*any, error) {
	panic("unported: mj-client/src/settings/save.rs merge_edit")
}

// EditConflict is a field this save and another client both changed.
// Nothing is written.
type EditConflict struct {
	Path string
}

func (e *EditConflict) Error() string {
	return e.Path + " changed in another client. Reopen Settings to edit the latest value."
}
