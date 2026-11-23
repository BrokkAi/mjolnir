// Port of mj-client/src/settings/save.rs.

// Writing a settings draft to the configuration file. Every surface saves
// through here, so a save means the same thing wherever it was made.

package client

import (
	"encoding/json"
	"errors"
	"fmt"
	"slices"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SaveAt saves the edit from original to updated into the configuration at
// path. Fields the edit left alone keep whatever the file holds now, so
// another client's change to them survives; a field both changed is
// refused.
func SaveAt(path string, original, updated string, state *core.StateState) (core.Config, error) {
	// Both sides are read through this build's configuration, so a field the
	// editing client predates arrives on both at the same default, and the
	// merge keeps whatever the file holds for it (#1557).
	var originalConfig core.Config
	if err := json.Unmarshal([]byte(original), &originalConfig); err != nil {
		return core.Config{}, err
	}
	originalValue, err := settingsToValue(originalConfig)
	if err != nil {
		return core.Config{}, err
	}
	var updatedConfig core.Config
	if err := json.Unmarshal([]byte(updated), &updatedConfig); err != nil {
		return core.Config{}, err
	}
	if err := updatedConfig.Validate(); err != nil {
		return core.Config{}, err
	}
	updatedValue, err := settingsToValue(updatedConfig)
	if err != nil {
		return core.Config{}, err
	}
	// A profile this edit adds or changes must be usable, so Setup reports a
	// missing key while it is open. Any other profile's readiness is not this
	// save's business: refusing over it would block every unrelated setting.
	for id, profile := range updatedConfig.EnabledProfiles() {
		if !settingsEqual(settingsPath(originalValue, "profiles", id), settingsPath(updatedValue, "profiles", id)) {
			if err := profile.EnsureReady(id); err != nil {
				return core.Config{}, err
			}
		}
	}
	saved, _, err := core.ConfigUpdateTo(path, func(config *core.Config) (struct{}, error) {
		// The editor includes implicit local defaults. Treat those same
		// defaults as the merge base when they have not been written to disk
		// yet.
		defaults := core.DefaultConfig().WithLocalTargets()
		// Compare in the stored shape the editor works in, where a runtime
		// names its machine, rather than in the fused in-memory shape.
		storedDefaults, err := settingsToValue(defaults)
		if err != nil {
			return struct{}{}, err
		}
		type implicitTarget struct {
			id     string
			target core.TargetTemplate
		}
		var implicit []implicitTarget
		for _, id := range sortedMapKeys(defaults.Targets) {
			target := defaults.Targets[id]
			if _, configured := config.Targets[id]; configured {
				continue
			}
			if settingsEqual(settingsPath(originalValue, "targets", id), settingsPath(storedDefaults, "targets", id)) {
				implicit = append(implicit, implicitTarget{id: id, target: target})
				if config.Targets == nil {
					config.Targets = map[string]core.TargetTemplate{}
				}
				config.Targets[id] = target
			}
		}
		current, err := settingsToValue(*config)
		if err != nil {
			return struct{}{}, err
		}
		merged, err := MergeEdit(&originalValue, &updatedValue, &current, "Setup")
		if err != nil {
			return struct{}{}, err
		}
		if merged == nil {
			return struct{}{}, errors.New("setup cannot remove the configuration")
		}
		var next core.Config
		if err := settingsFromValue(*merged, &next); err != nil {
			return struct{}{}, err
		}
		if next.Jev != config.Jev && (next.Jev.Enabled || next.Jev.FastinoKey != config.Jev.FastinoKey) {
			if literal, ok := next.Jev.FastinoKey.(core.EnvironmentValueLiteral); ok {
				if err := core.JevConfigValidateFastinoKey(literal.Value); err != nil {
					return struct{}{}, err
				}
			} else {
				err := core.WithSecretResolver(core.SecretResolverBeside(path), func() error {
					_, err := next.Jev.ResolveFastinoKey()
					return err
				})
				if err != nil {
					return struct{}{}, err
				}
			}
		}
		if err := state.ValidateSetupUpdate(config, &next); err != nil {
			return struct{}{}, err
		}
		// The defaults were only the merge base. A built-in target the edit
		// left as it was stays out of the file, as Config.Update does, so an
		// unrelated save does not turn it into one the user configured.
		for _, entry := range implicit {
			// A built-in runtime the edit switched off is written with it.
			kept, present := next.Targets[entry.id]
			_, disabled := next.DisabledTargets[entry.id]
			if present && sameTargetTemplate(kept, entry.target) && !disabled {
				delete(next.Targets, entry.id)
			}
		}
		*config = next
		return struct{}{}, nil
	})
	if err != nil {
		return core.Config{}, err
	}
	return saved, nil
}

// sameTargetTemplate is TargetTemplate's PartialEq, read as the two
// templates' serialized forms.
func sameTargetTemplate(a, b core.TargetTemplate) bool {
	left, leftErr := settingsToValue(a)
	right, rightErr := settingsToValue(b)
	return leftErr == nil && rightErr == nil && settingsEqual(left, right)
}

func sortedMapKeys[V any](values map[string]V) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	return keys
}

// MergeEdit applies only fields the dialog changed and refuses conflicting
// concurrent edits. Each value is Rust's Option<&Value>: nil is absent,
// which differs from a present JSON null.
func MergeEdit(original, updated, current *any, path string) (*any, error) {
	if optionalValueEqual(original, updated) || optionalValueEqual(updated, current) {
		return cloneOptionalValue(current), nil
	}
	if optionalValueEqual(original, current) {
		return cloneOptionalValue(updated), nil
	}
	if updated != nil && settingsIsObject(*updated) &&
		(original == nil || settingsIsObject(*original)) &&
		current != nil && settingsIsObject(*current) {
		keys := map[string]struct{}{}
		for _, value := range []*any{original, updated, current} {
			if value == nil {
				continue
			}
			for _, key := range (*value).(jsonvalue.Object).Keys() {
				keys[key] = struct{}{}
			}
		}
		merged := jsonvalue.Object{}
		for _, key := range sortedMapKeys(keys) {
			child, err := MergeEdit(optionalMember(original, key), optionalMember(updated, key),
				optionalMember(current, key), fmt.Sprintf("%s / %s", path, key))
			if err != nil {
				return nil, err
			}
			if child != nil {
				merged.Set(key, *child)
			}
		}
		var result any = merged
		return &result, nil
	}
	return nil, &EditConflict{Path: path}
}

// optionalValueEqual is Option<&Value> equality.
func optionalValueEqual(a, b *any) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return settingsEqual(*a, *b)
}

func cloneOptionalValue(value *any) *any {
	if value == nil {
		return nil
	}
	cloned := settingsClone(*value)
	return &cloned
}

// optionalMember is `value.and_then(|v| v.get(key))`.
func optionalMember(value *any, key string) *any {
	if value == nil {
		return nil
	}
	member, found := settingsObjectGet(*value, key)
	if !found {
		return nil
	}
	return &member
}

// EditConflict is a field this save and another client both changed.
// Nothing is written.
type EditConflict struct {
	Path string
}

func (e *EditConflict) Error() string {
	return e.Path + " changed in another client. Reopen Settings to edit the latest value."
}
