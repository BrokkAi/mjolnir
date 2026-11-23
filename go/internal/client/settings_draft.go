// Port of mj-client/src/settings/draft.rs.

// The settings draft: the stored shape of a configuration with every
// optional field filled in, and the paths that address it.

package client

import (
	"fmt"
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
	original, err := settingsToValue(config)
	if err != nil {
		panic(fmt.Sprintf("configuration serializes: %v", err))
	}
	draft = settingsClone(original)
	EnsureLocalMachine(&draft)
	path := []string{}
	Expand(&draft, &path)
	return original, draft
}

// ConfigFromDraft reads a draft back as a configuration, leaving draft
// unchanged.
func ConfigFromDraft(draft any) (core.Config, error) {
	draft = settingsClone(draft)
	root := draft.(jsonvalue.Object)
	root.SwapDelete("_subagent_options")
	root.SwapDelete("_reviewer_options")
	root.SwapDelete("_review_action_reason")
	draft = root
	for _, task := range []string{"primary", "straightforward", "subtle", "oracle"} {
		if _, ok := settingsArray(settingsPath(draft, "subagents", task)); !ok {
			continue
		}
		settingsIndexMut(&draft, []any{"subagents", task}, func(entries *any) {
			*entries = slices.DeleteFunc((*entries).([]any), func(entry any) bool {
				model, ok := settingsString(settingsGet(entry, "model"))
				return !ok || strings.TrimSpace(model) == ""
			})
		})
	}
	// The Machines page always shows this machine. Until it carries a
	// setting of its own it is the implied machine, not an entry, so it does
	// not turn an untouched draft into a change.
	if machines, ok := settingsObject(settingsGet(draft, "machines")); ok {
		if local, found := machines.Get(core.LocalMachineID); found {
			if cache, ok := settingsObject(settingsGet(local, "build_cache")); ok {
				allUnset := true
				for _, member := range cache {
					if !OnlyUnsetFields(member.Value) {
						allUnset = false
						break
					}
				}
				if allUnset {
					machines.SwapDelete(core.LocalMachineID)
					settingsSet(&draft, machines, "machines")
				}
			}
		}
	}
	// A cleared number that has a default of its own is left out, so the
	// configuration's default applies instead of a null it cannot read.
	for _, field := range [][2]string{{"notify", "delay_seconds"}, {"subagents", "max_concurrent"}} {
		section, key := field[0], field[1]
		if settingsPath(draft, section, key) != nil {
			continue
		}
		if entries, ok := settingsObject(settingsGet(draft, section)); ok {
			entries.SwapDelete(key)
			settingsSet(&draft, entries, section)
		}
	}
	// UI projections inspect sources only. Saving/warming serializes those
	// sources for credential resolution in the background request.
	return core.WithEnvironmentSourcesOnly(func() configResult {
		var config core.Config
		err := settingsFromValue(draft, &config)
		return configResult{config: config, err: err}
	}).unpack()
}

// configResult carries a decoded configuration out of a scoped read.
type configResult struct {
	config core.Config
	err    error
}

func (r configResult) unpack() (core.Config, error) { return r.config, r.err }

// EnsureLocalMachine: a configuration with no machines of its own still has
// this machine, so the Machines page always has an entry to open. The key is
// placed where the file keeps it, before the runtimes that name it, so the
// page list reads in the same order as the file.
func EnsureLocalMachine(draft *any) {
	root, ok := (*draft).(jsonvalue.Object)
	if !ok {
		panic("a configuration is an object")
	}
	if _, found := root.Get("machines"); !found {
		rebuilt := jsonvalue.Object{}
		for _, member := range root {
			if member.Key == "targets" {
				rebuilt.Set("machines", jsonvalue.Object{})
			}
			rebuilt.Set(member.Key, member.Value)
		}
		if _, found := rebuilt.Get("machines"); !found {
			rebuilt.Set("machines", jsonvalue.Object{})
		}
		root = rebuilt
	}
	machinesValue, _ := root.Get("machines")
	machines, ok := machinesValue.(jsonvalue.Object)
	if !ok {
		panic("machines is an object")
	}
	if _, found := machines.Get(core.LocalMachineID); !found {
		machines.Set(core.LocalMachineID, jsonvalue.Object{{Key: "kind", Value: "local"}})
	}
	root.Set("machines", machines)
	*draft = root
}
