// Port of mj-build-cache/src/mbx/configuration.rs.
//
// Machine policy. Inspection never writes; application atomically publishes
// the same document all containers mount. Native mbx configuration is
// read-only.
//
// Rust's test-only DEFAULT_MARKER and managed_document live in this file's
// tests.

package buildcache

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// configurationSharedDirectory is already reachable through every session's
// cache mount, including sessions provisioned before machine-level policy. No
// container recreation is needed.
func configurationSharedDirectory(cache string) string {
	return core.BuildCacheConfigurationDirectory(cache)
}

// configurationHostDirectory is the directory of the host's own mbx
// configuration.
func configurationHostDirectory(host CacheHost, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx/configuration.rs host_directory")
}

// configurationReadFile is the machine mbx configuration file at path, or
// nil when it does not exist.
func configurationReadFile(host CacheHost, path string, executor core.CommandExecutor) (*string, error) {
	panic("unported: mj-build-cache/src/mbx/configuration.rs read_file")
}

// configurationConfiguredLimit is the size string `table.field` sets in an
// mbx configuration text, or nil when the text or the field is absent.
func configurationConfiguredLimit(text *string, table, field string) (*string, error) {
	panic("unported: mj-build-cache/src/mbx/configuration.rs configured_limit")
}

// A lock belongs to the host, not the container or daemon. Compare-and-replace
// prevents an inspection made before another application from overwriting it.
// Atomic rename also leaves the last complete policy intact on cancellation.
const applyScript = `set -eu
directory=$1
expected=$2
mkdir -p -- "$directory"
exec 9>"$directory/.mj-apply.lock"
flock -w 5 9
file="$directory/config.toml"
temporary=$(mktemp "$directory/.config.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
cat > "$temporary"
chmod 600 "$temporary"
# Another launch may have applied this same policy since our inspection; the
# outcome is already what we want, so that is not a conflict.
if [ -f "$file" ] && cmp -s -- "$temporary" "$file"; then exit 0; fi
actual=missing
if [ -f "$file" ]; then actual=$(sha256sum -- "$file"); actual=${actual%% *}; fi
[ "$actual" = "$expected" ] || exit 75
mv -f -- "$temporary" "$file"
`

// configurationApply publishes cache's desired policy when the file still
// holds the policy the inspection read. False when another application
// replaced it first.
func configurationApply(host CacheHost, cache *ResolvedBuildCache, executor core.CommandExecutor) (bool, error) {
	panic("unported: mj-build-cache/src/mbx/configuration.rs apply")
}

// configurationApplication is whether the published policy is the desired
// one.
func configurationApplication(previous, desired *string) core.BuildCacheApplication {
	if optionalStringEqual(previous, desired) {
		return core.BuildCacheApplicationApplied{}
	}
	return core.BuildCacheApplicationPending{}
}

// optionalStringEqual is Rust's `Option<String> ==`.
func optionalStringEqual(left, right *string) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}
