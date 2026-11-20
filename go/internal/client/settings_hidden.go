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
	panic("unported: mj-client/src/settings/hidden.rs environments")
}

// Hide replaces literal credentials in a configuration or draft value with
// the marker.
func Hide(value *any) {
	panic("unported: mj-client/src/settings/hidden.rs hide")
}

// RehideRestored hides only literals that were markers in the browser's
// incoming draft. Values newly entered by the user must survive a
// server-side page action.
func RehideRestored(value *any, incoming any) {
	panic("unported: mj-client/src/settings/hidden.rs rehide_restored")
}

// Restore puts back each hidden value from stored, the stored shape of the
// configuration as it is now, and returns the values put back, so a caller
// can keep them out of anything else it sends. A marker whose value the file
// no longer holds is refused: saving it would write a value nobody entered.
func Restore(value *any, stored any) ([]string, error) {
	panic("unported: mj-client/src/settings/hidden.rs restore")
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
