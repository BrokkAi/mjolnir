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
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/BurntSushi/toml"

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
	if host.IsLocal() {
		directory, err := os.UserConfigDir()
		if err != nil {
			return "", errors.New("locate host mbx configuration")
		}
		return pathJoin(directory, "mbx"), nil
	}
	command := host.ShellCommand(`printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/mbx"`, label, nil,
		"locate host mbx configuration")
	output, err := executor.Execute(&command)
	if err != nil {
		return "", err
	}
	output, err = mbxChecked(output, &command)
	if err != nil {
		return "", err
	}
	path, err := fromUTF8(output.Stdout)
	if err != nil {
		return "", fmt.Errorf("decode mbx configuration path: %w", err)
	}
	if !filepath.IsAbs(path) {
		return "", errors.New("host mbx configuration directory is not absolute")
	}
	return path, nil
}

// configurationReadFile is the machine mbx configuration file at path, or
// nil when it does not exist.
func configurationReadFile(host CacheHost, path string, executor core.CommandExecutor) (*string, error) {
	command := host.ShellCommand(readConfigScript, label, []string{path}, "read machine mbx configuration")
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	if output.Status == 3 {
		return nil, nil
	}
	output, err = mbxChecked(output, &command)
	if err != nil {
		return nil, err
	}
	text, err := fromUTF8(output.Stdout)
	if err != nil {
		return nil, fmt.Errorf("decode machine mbx configuration: %w", err)
	}
	return &text, nil
}

// configurationConfiguredLimit is the size string `table.field` sets in an
// mbx configuration text, or nil when the text or the field is absent.
func configurationConfiguredLimit(text *string, table, field string) (*string, error) {
	if text == nil {
		return nil, nil
	}
	var document map[string]any
	if _, err := toml.Decode(*text, &document); err != nil {
		return nil, fmt.Errorf("parse host mbx configuration: %w", err)
	}
	section, ok := document[table].(map[string]any)
	if !ok {
		return nil, nil
	}
	raw, ok := section[field]
	if !ok {
		return nil, nil
	}
	value, ok := raw.(string)
	if !ok {
		return nil, fmt.Errorf("mbx %s.%s must be a size string", table, field)
	}
	if _, size := core.ParseBuildCacheSize(value); value != "none" && !size {
		return nil, fmt.Errorf("mbx %s.%s is not a size: %s", table, field, core.RustDebugString(value))
	}
	return &value, nil
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
	if optionalStringEqual(cache.PreviousConfig, cache.ConfigFile) {
		return true, nil
	}
	expected := "missing"
	if cache.PreviousConfig != nil {
		digest := sha256.Sum256([]byte(*cache.PreviousConfig))
		expected = core.LowerHex(digest[:])
	}
	command := host.ShellCommand(applyScript, label, []string{cache.ConfigDirectory, expected},
		"apply machine build cache budgets")
	if cache.ConfigFile == nil {
		return false, errors.New("missing managed mbx configuration")
	}
	command = command.WithSensitiveStdin([]byte(*cache.ConfigFile))
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	if output.Status == 75 {
		return false, nil
	}
	if _, err := mbxChecked(output, &command); err != nil {
		return false, err
	}
	return true, nil
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
