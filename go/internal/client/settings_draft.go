// Port of mj-client/src/settings/draft.rs.

// The settings draft: the stored shape of a configuration with every
// optional field filled in, and the paths that address it.

package client

import (
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// StoragePath is where a page path is stored. The Interface page is
// synthetic: it gathers root settings, and its prefix key lives under
// `keys`.
func StoragePath(path []string) []string {
	if len(path) == 2 && path[0] == "interface" && path[1] == "prefix" {
		return []string{"keys", "prefix"}
	}
	if len(path) > 0 && path[0] == "interface" {
		return slices.Clone(path[1:])
	}
	return slices.Clone(path)
}

// PagePath is the page path of a setting named the way the file stores it.
// The Interface page's settings live at the root (`theme`) or under `keys`
// (`keys.prefix`); every other setting's stored path is its page path.
func PagePath(path []string) []string {
	if len(path) == 1 && (path[0] == "theme" || path[0] == "spinner" || path[0] == "sessions_side") {
		return []string{"interface", path[0]}
	}
	if len(path) == 2 && path[0] == "keys" && path[1] == "prefix" {
		return []string{"interface", "prefix"}
	}
	return slices.Clone(path)
}

// Pointer is the JSON pointer of the stored path of a page path.
func Pointer(path []string) string {
	var pointer strings.Builder
	for _, key := range StoragePath(path) {
		pointer.WriteByte('/')
		pointer.WriteString(strings.ReplaceAll(strings.ReplaceAll(key, "~", "~0"), "/", "~1"))
	}
	return pointer.String()
}

func OnlyUnsetFields(value any) bool {
	switch value := value.(type) {
	case nil:
		return true
	case jsonvalue.Object:
		for _, member := range value {
			if !OnlyUnsetFields(member.Value) {
				return false
			}
		}
		return true
	}
	return false
}

// DraftOpen is Rust's settings::draft::open (renamed: daemon/ssh.rs's open
// keeps the name): the value an edit starts from and the draft it edits.
//
// original is the stored shape exactly as the configuration serializes,
// which is the base a save merges against. The draft adds this machine and
// every optional field, so each page can show what is unset.
func DraftOpen(config *core.Config) (original, draft any) {
	panic("unported: mj-client/src/settings/draft.rs open")
}

// ConfigFromDraft reads a draft back as a configuration, leaving draft
// unchanged.
func ConfigFromDraft(draft any) (core.Config, error) {
	panic("unported: mj-client/src/settings/draft.rs config_from_draft")
}

// EnsureLocalMachine: a configuration with no machines of its own still has
// this machine, so the Machines page always has an entry to open. The key is
// placed where the file keeps it, before the runtimes that name it, so the
// page list reads in the same order as the file.
func EnsureLocalMachine(draft *any) {
	panic("unported: mj-client/src/settings/draft.rs ensure_local_machine")
}
