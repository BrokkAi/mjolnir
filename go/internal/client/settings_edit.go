// Port of mj-client/src/settings/edit.rs.

// How an edit changes a settings draft. Every surface applies edits through
// these functions, so a value means the same thing whichever one typed it.
//
// Rust returns Result<_, String> here; the Go error's text is that string.

package client

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func clearReviewLegacy(draft *any) {
	settingsPointerMut(draft, "/review", func(review *any) {
		object, ok := (*review).(jsonvalue.Object)
		if !ok || object == nil {
			return
		}
		object.SwapDelete("trigger")
		object.SwapDelete("count")
		object.SwapDelete("profile")
		object.SwapDelete("effort")
		if _, found := object.Get("reviewers"); !found {
			object.Set("reviewers", []any{})
		}
		*review = object
	})
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

// errSettingGone is the failure of an edit whose setting is no longer in the
// draft.
const errSettingGone = settingsError("This setting no longer exists.")

// ApplyValue sets the scalar at path from what was typed or chosen. Returns
// the text the field now holds when applying it rewrote the text, as
// expanding `~` in a local path does.
func ApplyValue(draft *any, path []string, input SettingInput) (*string, error) {
	var text string
	var choice any
	hasChoice := false
	switch typed := input.(type) {
	case SettingInputText:
		text = typed.Value
	case SettingInputChoice:
		choice, hasChoice = typed.Value, true
	}
	var rewritten *string
	parent := path[:len(path)-1]
	parentValue, found := settingsPointer(*draft, Pointer(parent))
	if !found {
		return nil, errSettingGone
	}
	key := path[len(path)-1]
	isPrefix := slices.Equal(path, []string{"interface", "prefix"})
	defaults := Defaults(parent, parentValue)
	number, isNumber := WholeNumberOf(path)
	clear := !hasChoice && text == "" &&
		(settingsHasNull(defaults, key) || isPrefix || (isNumber && number.Defaulted))
	if !clear && text != "" && !hasChoice {
		if kind, ok := PathKindOf(path); ok {
			switch kind {
			case PathKindLocal:
				expanded, err := core.ExpandLocal(text)
				if err != nil {
					return nil, settingsError(err.Error())
				}
				if expanded != text {
					rewritten = &expanded
				}
				text = expanded
			case PathKindRelativeDestination:
				if err := core.ValidateRelativeDestination(text); err != nil {
					return nil, settingsError(err.Error())
				}
				needs, err := core.NeedsHome(text)
				if err != nil {
					return nil, settingsError(err.Error())
				}
				if needs {
					return nil, settingsError("Repository destinations must be safe relative paths; ~ is not supported.")
				}
			}
		}
	}
	old, found := settingsPointer(*draft, Pointer(path))
	if !found {
		return nil, errSettingGone
	}
	old = settingsClone(old)
	var value any
	switch {
	case clear && isPrefix:
		value = core.DefaultPrefix
	case clear:
		value = nil
	case hasChoice:
		value = choice
	case isNumber:
		parsed, err := ParseWholeNumber(number, text)
		if err != nil {
			return nil, err
		}
		value = parsed
	case IsBuildCacheBudget(path):
		trimmed := strings.TrimSpace(text)
		if trimmed == "" {
			value = nil
		} else {
			gigabytes, err := parseRustUint64(trimmed)
			if err != nil {
				return nil, settingsError("Enter a whole number of gigabytes.")
			}
			if gigabytes == 0 {
				return nil, settingsError("Enter at least 1 GB, or clear the field to use the default.")
			}
			value = core.BuildCacheSizeFromGigabytes(gigabytes)
		}
	case strings.TrimSpace(text) == "" && old == nil:
		value = nil
	default:
		value = text
	}
	// An empty optional field means the account/runtime default.
	parentPath := slices.Clone(parent)
	parentNow, _ := settingsPointer(*draft, Pointer(parentPath))
	defaults = Defaults(parentPath, parentNow)
	if settingsEqual(value, "") && settingsHasNull(defaults, key) {
		value = nil
	}
	if text, ok := value.(string); ok && slices.Equal(path, []string{"jev", "fastino_key"}) {
		reference, isReference, err := core.EnvironmentValueTypedReference(text)
		if isReference {
			if err != nil {
				return nil, settingsError(err.Error())
			}
			encoded, err := settingsToValue(reference)
			if err != nil {
				return nil, settingsError(err.Error())
			}
			value = encoded
		} else if err := core.JevConfigValidateFastinoKey(text); err != nil {
			return nil, settingsError(err.Error())
		}
	}
	if isPrefix {
		var keys core.KeysConfig
		if err := settingsFromValue(settingsGet(*draft, "keys"), &keys); err != nil {
			return nil, settingsError(err.Error())
		}
		keys.Prefix, _ = value.(string)
		if _, err := keys.Resolve(); err != nil {
			return nil, settingsError(err.Error())
		}
	}
	if clear && !settingsHasNull(defaults, key) && !isPrefix && !(isNumber && number.Defaulted) {
		return nil, settingsError("This setting is required. Choose a value instead of clearing it.")
	}
	changed := !settingsEqual(old, value)
	if key == "kind" {
		oldParent, _ := settingsPointer(*draft, Pointer(parentPath))
		oldParent = settingsClone(oldParent)
		replacement := Defaults(parentPath, jsonvalue.Object{{Key: "kind", Value: value}}).(jsonvalue.Object)
		for i := range replacement {
			if replacement[i].Key == "kind" {
				continue
			}
			if previous, found := settingsObjectGet(oldParent, replacement[i].Key); found {
				replacement[i].Value = settingsClone(previous)
			}
		}
		replacement.Set("kind", value)
		settingsPointerMut(draft, Pointer(parentPath), func(target *any) { *target = replacement })
	} else {
		settingsPointerMut(draft, Pointer(path), func(target *any) { *target = value })
	}
	if changed && len(path) == 4 && path[0] == "subagents" && isSettingsTask(path[1]) && key == "model" {
		index, _ := strconv.Atoi(path[2])
		settingsSet(draft, nil, "subagents", path[1], index, "effort")
	}
	if changed && len(path) == 4 && path[0] == "review" && path[1] == "reviewers" && key == "model" {
		index, _ := strconv.Atoi(path[2])
		settingsSet(draft, nil, "review", "reviewers", index, "effort")
	}
	if changed && len(path) == 4 && path[0] == "review" && path[1] == "reviewers" {
		clearReviewLegacy(draft)
	}
	return rewritten, nil
}

// settingsObjectGet is `value.get(key)` on an object: the member, when there
// is one.
func settingsObjectGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// Stage sets the setting at page path to value the way a person would in
// Settings: a checkbox is ticked or cleared, a listed value is chosen, and
// anything else is typed, so it is checked and rewritten exactly as typing
// it would be. A nil value (JSON null) clears the field.
func Stage(draft *any, path []string, value any) error {
	current, found := settingsPointer(*draft, Pointer(path))
	if !found {
		return settingsError("there is no such setting")
	}
	if settingsIsObject(current) || settingsIsArray(current) {
		return settingsError("this is a page of settings; name one of the settings on it")
	}
	for depth := 1; depth < len(path); depth++ {
		parent := path[:depth]
		parentValue, found := settingsPointer(*draft, Pointer(parent))
		if !found {
			panic("parent of a setting")
		}
		if HiddenKey(parent, parentValue, path[depth]) {
			return settingsError("Settings does not offer this setting here")
		}
	}
	if wanted, ok := value.(bool); ok {
		if held, ok := current.(bool); ok {
			if held != wanted {
				if _, err := Toggle(draft, path); err != nil {
					return err
				}
			}
			return nil
		}
	}
	choices := Choices(StoragePath(path), *draft)
	if len(choices) > 1 && value != nil {
		if !settingsContains(choices, value) {
			var listed []string
			for _, choice := range choices {
				if choice != nil {
					listed = append(listed, settingsDisplay(choice))
				}
			}
			return settingsError(fmt.Sprintf("%s is not one of %s", settingsDisplay(value), strings.Join(listed, ", ")))
		}
		_, err := ApplyValue(draft, path, SettingInputChoice{Value: value})
		return err
	}
	var text string
	switch typed := value.(type) {
	case nil:
	case string:
		text = typed
	case jsonvalue.Object, []any:
		return settingsError("give one value; set a list or table one setting at a time")
	default:
		text = settingsDisplay(value)
	}
	_, err := ApplyValue(draft, path, SettingInputText{Value: text})
	return err
}

// StageAll stages every edit, or none: the draft changes only when each edit
// applies and the result is a configuration that could be saved. Paths may
// use the stored spelling of an Interface setting (`theme`, `keys.prefix`).
// Returns the page path of each edit, in order.
func StageAll(draft *any, edits []SettingEdit) ([][]string, error) {
	if len(edits) == 0 {
		return nil, settingsError("name at least one setting to change")
	}
	staged := settingsClone(*draft)
	paths := make([][]string, 0, len(edits))
	for _, edit := range edits {
		path := PagePath(edit.Path)
		var value any
		if len(edit.Value) > 0 {
			parsed, err := jsonvalue.Parse(edit.Value)
			if err != nil {
				return nil, settingsError(fmt.Sprintf("%s: %v", edit.Dotted(), err))
			}
			value = parsed
		}
		if err := Stage(&staged, path, value); err != nil {
			return nil, settingsError(fmt.Sprintf("%s: %v", edit.Dotted(), err))
		}
		paths = append(paths, path)
	}
	config, err := ConfigFromDraft(settingsClone(staged))
	if err == nil {
		err = config.Validate()
	}
	if err != nil {
		return nil, settingsError(fmt.Sprintf("these settings would not save: %v", err))
	}
	*draft = staged
	return paths, nil
}

// Toggle flips the checkbox at path. Disabling a profile also clears the
// places that name it, so the saved configuration never points at a profile
// that cannot run; the returned notice says what was cleared.
func Toggle(draft *any, path []string) (*string, error) {
	var failure error
	enabled := false
	found := settingsPointerMut(draft, Pointer(path), func(value *any) {
		held, ok := (*value).(bool)
		if !ok {
			failure = settingsError("This setting is not a checkbox.")
			return
		}
		enabled = !held
		*value = enabled
	})
	if !found {
		return nil, errSettingGone
	}
	if failure != nil {
		return nil, failure
	}
	if len(path) != 3 || path[0] != "profiles" || path[2] != "enabled" || enabled {
		return nil, nil
	}
	notice := fmt.Sprintf("Disabled profile %s.", buildIdentityDebug(path[1]))
	return &notice, nil
}

// EditAddEntry is Rust's settings::edit::add_entry (renamed: settings/view's
// AddEntry type keeps the name). It adds an entry to the collection at
// collection. A list grows by one default entry; a named collection needs
// the new entry's name. Returns the new entry's path.
func EditAddEntry(draft *any, collection []string, name *string) ([]string, error) {
	isReviewers := slices.Equal(collection, []string{"review", "reviewers"})
	if _, found := settingsPointer(*draft, Pointer(collection)); isReviewers && !found {
		settingsSet(draft, []any{}, "review", "reviewers")
	}
	value, found := settingsPointer(*draft, Pointer(collection))
	if !found {
		return nil, settingsError("This list no longer exists.")
	}
	path := slices.Clone(collection)
	if array, ok := value.([]any); ok {
		if isReviewers && len(array) >= core.MaxReviewers {
			return nil, settingsError("At most three reviewers are allowed.")
		}
		isSubagentList := len(collection) == 2 && collection[0] == "subagents" && isSettingsTask(collection[1])
		var entry any
		switch {
		case isReviewers || isSubagentList:
			entry = jsonvalue.Object{{Key: "model", Value: ""}, {Key: "effort", Value: nil}}
		case len(collection) > 0 && collection[len(collection)-1] == "repositories":
			entry = RepositoryDefault()
		default:
			entry = ""
		}
		array = append(array, entry)
		settingsPointerMut(draft, Pointer(collection), func(target *any) { *target = array })
		path = append(path, strconv.Itoa(len(array)-1))
		if isReviewers {
			clearReviewLegacy(draft)
		}
		return path, nil
	}
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, settingsError("Entries cannot be added here.")
	}
	entryName := ""
	if name != nil {
		entryName = strings.TrimSpace(*name)
	}
	if entryName == "" {
		return nil, settingsError("Choose a name for the new entry.")
	}
	if _, exists := object.Get(entryName); exists {
		return nil, settingsError(fmt.Sprintf("An entry named %s already exists.", buildIdentityDebug(entryName)))
	}
	path = append(path, entryName)
	var entry any
	if len(collection) > 0 && collection[len(collection)-1] == "environment" {
		entry = ""
	} else {
		entry = Defaults(path, jsonvalue.Object{})
	}
	object.Set(entryName, entry)
	settingsPointerMut(draft, Pointer(collection), func(target *any) { *target = object })
	return path, nil
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
	targets, ok := settingsObject(settingsGet(draft, "targets"))
	if !ok {
		return []string{}
	}
	ids := []string{}
	for _, target := range targets {
		machine, ok := settingsString(settingsGet(target.Value, "machine"))
		if !ok {
			machine = core.LocalMachineID
		}
		if machine == machineID {
			ids = append(ids, target.Key)
		}
	}
	return ids
}

// Removable is whether the entry at path is one a person may remove.
func Removable(draft any, path []string) bool {
	if len(path) == 0 {
		return false
	}
	key, parent := path[len(path)-1], path[:len(path)-1]
	value, found := settingsPointer(draft, Pointer(parent))
	return found && IsCollection(parent, value) &&
		!(slices.Equal(parent, []string{"machines"}) && key == core.LocalMachineID)
}

// RemoveEntry removes the collection entry at path.
func RemoveEntry(draft *any, path []string) (Removal, error) {
	if len(path) == 0 {
		return Removal{}, settingsError("The settings themselves cannot be removed.")
	}
	key, parent := path[len(path)-1], path[:len(path)-1]
	if value, found := settingsPointer(*draft, Pointer(parent)); !found || !IsCollection(parent, value) {
		return Removal{}, settingsError("Only entries of a list can be removed.")
	}
	isMachines := slices.Equal(parent, []string{"machines"})
	if isMachines && key == core.LocalMachineID {
		return Removal{}, settingsError("This machine is always available.")
	}
	// A runtime cannot outlive the machine it names, so removing a machine
	// takes its runtimes with it. Left behind, they would make the next save
	// fail on a machine the file no longer defines.
	var runtimes []string
	if isMachines {
		runtimes = RuntimeIDsOnMachine(*draft, key)
	}
	removed := [][]string{slices.Clone(path)}
	var listIndex *int
	var failure error
	removedFromList := false
	settingsPointerMut(draft, Pointer(parent), func(target *any) {
		switch typed := (*target).(type) {
		case jsonvalue.Object:
			typed.SwapDelete(key)
			*target = typed
		case []any:
			index, err := parseRustUint64(key)
			if err != nil || index >= uint64(len(typed)) {
				failure = settingsError("This list entry does not exist.")
				return
			}
			*target = slices.Delete(typed, int(index), int(index)+1)
			position := int(index)
			listIndex = &position
			removedFromList = true
		}
	})
	if failure != nil {
		return Removal{}, failure
	}
	if removedFromList && slices.Equal(parent, []string{"review", "reviewers"}) {
		clearReviewLegacy(draft)
	}
	var notice *string
	if targets, ok := settingsObject(settingsGet(*draft, "targets")); ok && len(runtimes) > 0 {
		for _, runtime := range runtimes {
			targets.SwapDelete(runtime)
			removed = append(removed, []string{"targets", runtime})
		}
		settingsSet(draft, targets, "targets")
		var text string
		if len(runtimes) == 1 {
			text = fmt.Sprintf("Removed machine %s and its runtime %s.", buildIdentityDebug(key), buildIdentityDebug(runtimes[0]))
		} else {
			quoted := make([]string, len(runtimes))
			for i, runtime := range runtimes {
				quoted[i] = buildIdentityDebug(runtime)
			}
			text = fmt.Sprintf("Removed machine %s and its %d runtimes %s.", buildIdentityDebug(key), len(runtimes), strings.Join(quoted, ", "))
		}
		notice = &text
	}
	return Removal{Removed: removed, ListIndex: listIndex, Notice: notice}, nil
}
