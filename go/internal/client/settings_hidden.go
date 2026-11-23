// Port of mj-client/src/settings/hidden.rs.

// Literal environment values never leave the daemon for a browser.
//
// A profile or container runtime may hold an API key as a literal
// environment value. The browser's copy of a draft carries a marker in its
// place; the browser may replace the value but never reads it. Saving puts
// the stored value back first. A reference (`from_env`, `from_secret`) names
// where a value comes from rather than holding it, so it is shown as is.

package client

import (
	"fmt"
	"iter"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const markerKey = "mj_hidden_value"

// IsMarker is whether value stands in for a hidden literal.
func IsMarker(value any) bool {
	object, ok := value.(jsonvalue.Object)
	if !ok || len(object) != 1 {
		return false
	}
	marked, ok := object.Get(markerKey)
	return ok && marked == true
}

// hiddenEnvironment is one environment map of a configuration value, with
// what owns it: section is "profile" or "runtime", id the entry's name.
// entries shares its members with the value, so replacing a member's value
// changes the configuration.
type hiddenEnvironment struct {
	section string
	id      string
	entries jsonvalue.Object
}

// environments is the environment maps of a configuration value, with what
// owns each.
func environments(value *any) iter.Seq[hiddenEnvironment] {
	return func(yield func(hiddenEnvironment) bool) {
		root, ok := (*value).(jsonvalue.Object)
		if !ok {
			return
		}
		for _, sectionMember := range root {
			var section string
			switch sectionMember.Key {
			case "profiles":
				section = "profile"
			case "targets":
				section = "runtime"
			default:
				continue
			}
			entries, ok := settingsObject(sectionMember.Value)
			if !ok {
				continue
			}
			for _, entry := range entries {
				environment, ok := settingsObject(settingsGet(entry.Value, "environment"))
				if !ok {
					continue
				}
				if !yield(hiddenEnvironment{section: section, id: entry.Key, entries: environment}) {
					return
				}
			}
		}
	}
}

// hiddenMarker is the value that stands in for a hidden literal.
func hiddenMarker() any { return jsonvalue.Object{{Key: markerKey, Value: true}} }

// hiddenOwner is the configuration section that owns an environment's
// section.
func hiddenOwner(section string) string {
	if section == "profile" {
		return "profiles"
	}
	return "targets"
}

// Hide replaces literal credentials in a configuration or draft value with
// the marker.
func Hide(value *any) {
	settingsPointerMut(value, "/jev/fastino_key", func(key *any) {
		if _, ok := (*key).(string); ok {
			*key = hiddenMarker()
		}
	})
	for environment := range environments(value) {
		for i := range environment.entries {
			if _, ok := environment.entries[i].Value.(string); ok {
				environment.entries[i].Value = hiddenMarker()
			}
		}
	}
}

// RehideRestored hides only literals that were markers in the browser's
// incoming draft. Values newly entered by the user must survive a
// server-side page action.
func RehideRestored(value *any, incoming any) {
	if IsMarker(settingsPath(incoming, "jev", "fastino_key")) {
		settingsPointerMut(value, "/jev/fastino_key", func(key *any) {
			if _, ok := (*key).(string); ok {
				*key = hiddenMarker()
			}
		})
	}
	for environment := range environments(value) {
		owner := hiddenOwner(environment.section)
		for i := range environment.entries {
			key := environment.entries[i].Key
			if IsMarker(settingsPath(incoming, owner, environment.id, "environment", key)) {
				environment.entries[i].Value = hiddenMarker()
			}
		}
	}
}

// Restore puts back each hidden value from stored, the stored shape of the
// configuration as it is now, and returns the values put back, so a caller
// can keep them out of anything else it sends. A marker whose value the file
// no longer holds is refused: saving it would write a value nobody entered.
func Restore(value *any, stored any) ([]string, error) {
	var restored []string
	for environment := range environments(value) {
		owner := hiddenOwner(environment.section)
		for i := range environment.entries {
			if !IsMarker(environment.entries[i].Value) {
				continue
			}
			key := environment.entries[i].Key
			literal, ok := settingsString(settingsPath(stored, owner, environment.id, "environment", key))
			if !ok {
				return nil, settingsError(fmt.Sprintf(
					"The hidden value of %s in %s %s was changed or removed in another client. Reopen Settings, then enter it again.",
					key, environment.section, buildIdentityDebug(environment.id)))
			}
			restored = append(restored, literal)
			environment.entries[i].Value = literal
		}
	}
	var failure error
	settingsPointerMut(value, "/jev/fastino_key", func(key *any) {
		if !IsMarker(*key) {
			return
		}
		literal, ok := settingsString(settingsPath(stored, "jev", "fastino_key"))
		if !ok {
			failure = settingsError("The hidden Fastino key was changed or removed in another client. Reopen Settings, then enter it again.")
			return
		}
		restored = append(restored, literal)
		*key = literal
	})
	if failure != nil {
		return nil, failure
	}
	if containsMarker(*value) {
		return nil, settingsError("A hidden value is not where Settings left it. Reopen Settings.")
	}
	if restored == nil {
		restored = []string{}
	}
	return restored, nil
}

// Scrub is message with every one of secrets replaced, for a failure that
// might quote a value it was given.
func Scrub(message string, secrets []string) string {
	for _, secret := range secrets {
		if secret != "" {
			message = strings.ReplaceAll(message, secret, "(hidden)")
		}
	}
	return message
}

func containsMarker(value any) bool {
	if IsMarker(value) {
		return true
	}
	switch value := value.(type) {
	case jsonvalue.Object:
		for _, member := range value {
			if containsMarker(member.Value) {
				return true
			}
		}
	case []any:
		for _, entry := range value {
			if containsMarker(entry) {
				return true
			}
		}
	}
	return false
}
