// Port of mj-worker/src/worker_runtime/tool_cache.rs.

// Configure the build tools already used by a checkout. All generated files
// belong to this worker; shared directories contain only tool-owned caches.

//go:build unix

package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func toolCachePrepare(cwd string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/tool_cache.rs prepare")
}

// extendWritableDirectories: cache storage is writable to the harness but is
// not a repository root for checkpointing or review discovery. Use the
// prepared environment so raw SSH home-relative placement has already become
// absolute.
func extendWritableDirectories(harness core.HarnessKind, environment map[string]string, directories *[]string) {
	if harness == core.HarnessKindMuse {
		return
	}
	for _, name := range toolCacheDirectoryVariables {
		path, ok := environment[name]
		if ok && filepath.IsAbs(path) && !slices.Contains(*directories, path) {
			*directories = append(*directories, path)
		}
	}
}

func toolCacheHasAny(root string, names []string) bool {
	return slices.ContainsFunc(names, func(name string) bool {
		info, err := os.Stat(filepath.Join(root, name))
		return err == nil && info.Mode().IsRegular()
	})
}

// bazelPath: Bazel hashes PATH into action keys. Remove only Mjolnir's
// private launcher directories; all user toolchain entries, including empty
// entries, survive.
const bazelPath = `mj_path=
mj_path_set=
mj_rest=${PATH:-}
while :; do
    case "$mj_rest" in
        *:*) mj_entry=${mj_rest%%:*}; mj_rest=${mj_rest#*:}; mj_more=yes ;;
        *) mj_entry=$mj_rest; mj_more= ;;
    esac
    if { [ -z "${MJ_BUILD_TOOL_BIN:-}" ] || [ "$mj_entry" != "$MJ_BUILD_TOOL_BIN" ]; } &&
       { [ -z "${MJ_GITHUB_CLI_BIN:-}" ] || [ "$mj_entry" != "$MJ_GITHUB_CLI_BIN" ]; }; then
        if [ -n "$mj_path_set" ]; then mj_path=$mj_path:$mj_entry; else mj_path=$mj_entry; fi
        mj_path_set=yes
    fi
    [ -n "$mj_more" ] || break
done
PATH=$mj_path
export PATH
unset mj_path mj_path_set mj_rest mj_entry mj_more
`

func preserveGoSettings(home string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/tool_cache.rs preserve_go_settings")
}

func supportsMbxExec(mbx string, cwd string, environment map[string]string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/tool_cache.rs supports_mbx_exec")
}

func toolCacheSetDefault(environment map[string]string, name string, path string) {
	if _, ok := environment[name]; !ok {
		environment[name] = path
	}
}

func toolCacheQuotePath(path string) string {
	return core.PosixQuote(path)
}

func writeLauncher(path string, body string) error {
	return writeExecutable(path, "#!/bin/sh\nset -eu\n"+body)
}

func prepareGradle(cwd string, home string, private string, shared string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/tool_cache.rs prepare_gradle")
}

func toolCacheReadOptional(path string) (string, error) {
	text, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return "", nil
	}
	if err != nil {
		return "", fmt.Errorf("read %s: %w", path, err)
	}
	return string(text), nil
}

func hasGradleCacheProperty(text string) bool {
	for line := range strings.Lines(text) {
		if strings.HasPrefix(strings.TrimLeftFunc(line, unicode.IsSpace), "org.gradle.caching") {
			return true
		}
	}
	return false
}

func linkConfiguration(source string, destination string) error {
	panic("unported: mj-worker/src/worker_runtime/tool_cache.rs link_configuration")
}
