// Port of mj-worker/src/worker_runtime/tool_cache.rs.

// Configure the build tools already used by a checkout. All generated files
// belong to this worker; shared directories contain only tool-owned caches.

//go:build unix

package worker

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

func toolCachePrepare(cwd string, environment map[string]string) error {
	shared, ok := environment["MJ_TOOL_CACHE_DIR"]
	if !ok {
		return nil
	}
	if !filepath.IsAbs(shared) {
		return errors.New("native build cache must be absolute")
	}
	home, ok := environment["HOME"]
	if !ok {
		return errors.New("build caches need HOME")
	}
	private, ok := environment["MJ_TOOL_CACHE_HOME"]
	if !ok {
		return errors.New("native cache placement needs a private directory")
	}
	if !filepath.IsAbs(private) {
		private = core.Join(home, private)
	}
	environment["MJ_TOOL_CACHE_HOME"] = private
	cwdDigest := sha256.Sum256([]byte(cwd))
	private = core.Join(private, hex.EncodeToString(cwdDigest[:]))
	project, ok := environment["MJ_TOOL_CACHE_PROJECT"]
	if !ok {
		return errors.New("native cache placement needs a project identity")
	}
	projectDigest := sha256.Sum256([]byte(project))
	projectCache := core.Join(core.Join(shared, "projects"), hex.EncodeToString(projectDigest[:]))
	bin := core.Join(private, "bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return fmt.Errorf("create private build tool launchers: %w", err)
	}
	inheritedPath := environment["PATH"]
	previousBin, hasPreviousBin := environment["MJ_BUILD_TOOL_BIN"]
	var kept []string
	for _, entry := range toolCacheSplitPaths(inheritedPath) {
		if workerRuntimePathEqual(entry, bin) || (hasPreviousBin && workerRuntimePathEqual(entry, previousBin)) {
			continue
		}
		kept = append(kept, entry)
	}
	path, err := toolCacheJoinPaths(kept)
	if err != nil {
		return err
	}
	environment["PATH"] = path

	if toolCacheHasAny(cwd, []string{"go.mod", "go.work"}) {
		if err := preserveGoSettings(home, environment); err != nil {
			return err
		}
		toolCacheSetDefault(environment, "GOCACHE", core.Join(shared, "go"))
		// Go otherwise hashes the absolute package directory, preventing
		// reuse between worktrees. An explicit -trimpath=false remains
		// authoritative.
		flags := environment["GOFLAGS"]
		if !slices.ContainsFunc(strings.Fields(flags), func(flag string) bool {
			return flag == "-trimpath" || strings.HasPrefix(flag, "-trimpath=")
		}) {
			if flags != "" {
				flags += " "
			}
			flags += "-trimpath"
		}
		environment["GOFLAGS"] = flags
	}
	if toolCacheHasAny(cwd, []string{"turbo.json", "turbo.jsonc"}) {
		toolCacheSetDefault(environment, "TURBO_CACHE_DIR", core.Join(projectCache, "turbo"))
	}
	if toolCacheHasAny(cwd, []string{"settings.gradle", "settings.gradle.kts", "build.gradle", "build.gradle.kts"}) {
		if err := prepareGradle(cwd, home, private, projectCache, environment); err != nil {
			return err
		}
	}
	if toolCacheIsFile(core.Join(cwd, "nx.json")) {
		// Nx 23.2+ shares artifacts and the cache DB under ~/.nx, while its
		// checkout-specific graph state stays private. The controller mounts
		// the host's directory there. Overriding either NX_* directory opts
		// out of Nx's coordinated placement and must remain the user's
		// choice.
		if mounted, ok := environment["MJ_NX_HOME_DIR"]; ok && !workerRuntimePathEqual(core.Join(home, ".nx"), mounted) {
			return errors.New("Nx cache mount does not match HOME; set the container HOME in its target environment")
		}
	}

	if toolCacheHasAny(cwd, []string{"MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel", ".bazelversion"}) {
		rc := core.Join(private, "bazelrc")
		cache := core.Join(projectCache, "bazel")
		cache = strings.ReplaceAll(strings.ReplaceAll(cache, `\`, `\\`), `"`, `\"`)
		if err := core.AtomicWriteExisting(rc, []byte(fmt.Sprintf("build --disk_cache=\"%s\"\n", cache))); err != nil {
			return err
		}
		for _, tool := range []string{"bazel", "bazelisk"} {
			real, found, err := harnessinstall.FindCommand(tool, environment)
			if err != nil {
				return err
			}
			if !found {
				continue
			}
			body := fmt.Sprintf("%s\nif [ \"$#\" = 1 ] && [ \"$1\" = --version ]; then exec %s \"$@\"; fi\nexec %s --bazelrc=%s \"$@\"\n",
				bazelPath, toolCacheQuotePath(real), toolCacheQuotePath(real), toolCacheQuotePath(rc))
			if err := writeLauncher(core.Join(bin, tool), body); err != nil {
				return err
			}
		}
	}
	if toolCacheHasAny(cwd, []string{"CMakeLists.txt", "Makefile", "makefile", "GNUmakefile"}) {
		mbx, found, err := harnessinstall.FindCommand("mbx", environment)
		if err != nil {
			return err
		}
		if found {
			supported, err := supportsMbxExec(mbx, cwd, environment)
			if err != nil {
				return err
			}
			if supported {
				for _, tool := range []string{"cmake", "make", "gmake", "ninja"} {
					real, found, err := harnessinstall.FindCommand(tool, environment)
					if err != nil {
						return err
					}
					if !found {
						continue
					}
					body := fmt.Sprintf("if [ -n \"${MJ_MBX_EXEC_ACTIVE:-}\" ]; then exec %s \"$@\"; fi\nexport MJ_MBX_EXEC_ACTIVE=1\nexec %s exec %s \"$@\"\n",
						toolCacheQuotePath(real), toolCacheQuotePath(mbx), toolCacheQuotePath(real))
					if err := writeLauncher(core.Join(bin, tool), body); err != nil {
						return err
					}
				}
			}
		}
	}
	path, err = toolCacheJoinPaths(append([]string{bin}, toolCacheSplitPaths(path)...))
	if err != nil {
		return err
	}
	environment["PATH"] = path
	// The existing Git shell hook restores this after bash login files reset
	// PATH. Keep a single BASH_ENV owner, including its user-supplied hook.
	environment["MJ_BUILD_TOOL_BIN"] = bin
	return shellEnvironmentConfigure(core.Join(private, "shell-env"), environment)
}

// toolCacheSplitPaths is Rust's std::env::split_paths on Unix: every entry
// between separators, the empty ones included.
func toolCacheSplitPaths(path string) []string {
	return strings.Split(path, string(os.PathListSeparator))
}

// toolCacheJoinPaths is Rust's std::env::join_paths on Unix.
func toolCacheJoinPaths(entries []string) (string, error) {
	for _, entry := range entries {
		if strings.ContainsRune(entry, os.PathListSeparator) {
			return "", fmt.Errorf("path segment contains separator `%c`", os.PathListSeparator)
		}
	}
	return strings.Join(entries, string(os.PathListSeparator)), nil
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
		return toolCacheIsFile(filepath.Join(root, name))
	})
}

// toolCacheIsFile is Rust's Path::is_file: it follows symbolic links.
func toolCacheIsFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
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
	goCommand, found, err := harnessinstall.FindCommand("go", environment)
	if err != nil {
		return err
	}
	if !found {
		return nil
	}
	// Query outside the checkout and forbid toolchain downloads during worker
	// preparation. GOENV includes `go env -w` settings, not just shell
	// exports.
	command := exec.Command(goCommand, "env", "GOENV")
	command.Dir = home
	childEnvironment := make(map[string]string, len(environment)+1)
	for name, value := range environment {
		childEnvironment[name] = value
	}
	childEnvironment["GOTOOLCHAIN"] = "local"
	command.Env = workerRuntimeEnvironmentList(childEnvironment)
	output, err := core.RunCapturingStdout(command)
	if err != nil {
		return err
	}
	if !output.Status.Success() {
		return fmt.Errorf("could not read Go's persistent environment: %s", fromUTF8Lossy(output.Stderr))
	}
	if !utf8.Valid(output.Stdout) {
		return errors.New("Go's persistent environment path is not UTF-8")
	}
	path := strings.TrimSpace(string(output.Stdout))
	if path == "" || path == "off" {
		return nil
	}
	text, err := toolCacheReadOptional(path)
	if err != nil {
		return err
	}
	for _, line := range toolCacheLines(text) {
		name, value, ok := strings.Cut(line, "=")
		if !ok || (name != "GOFLAGS" && name != "GOCACHE") {
			continue
		}
		if _, present := environment[name]; !present {
			environment[name] = value
		}
	}
	return nil
}

// toolCacheLines is Rust's str::lines: split on `\n`, each line without its
// `\n` or `\r\n`, and no final empty line.
func toolCacheLines(text string) []string {
	var lines []string
	for line := range strings.Lines(text) {
		line = strings.TrimSuffix(line, "\n")
		line = strings.TrimSuffix(line, "\r")
		lines = append(lines, line)
	}
	return lines
}

func supportsMbxExec(mbx string, cwd string, environment map[string]string) (bool, error) {
	command := exec.Command(mbx, "exec", "--help")
	command.Dir = cwd
	command.Env = workerRuntimeEnvironmentList(environment)
	output, err := core.RunCapturingStdout(command)
	if err != nil {
		return false, err
	}
	if !output.Status.Success() {
		slog.Warn("C/C++ cache launchers need mbx with standalone exec support; upgrade the host mbx installation")
	}
	return output.Status.Success(), nil
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
	// Only task outputs are shared. Dependency locks and daemons in one user
	// home cannot coordinate across separate container network namespaces.
	destination := core.Join(private, "gradle")
	source, ok := environment["MJ_ORIGINAL_GRADLE_USER_HOME"]
	if !ok {
		source, ok = environment["GRADLE_USER_HOME"]
	}
	if !ok {
		source = core.Join(home, ".gradle")
	}
	if !filepath.IsAbs(source) {
		source = core.Join(cwd, source)
	}
	if workerRuntimePathEqual(source, destination) {
		return errors.New("Gradle cache preparation lost its original user home")
	}
	init := core.Join(destination, "init.d")
	if err := os.MkdirAll(init, 0o777); err != nil {
		return err
	}
	userProperties, err := toolCacheReadOptional(core.Join(source, "gradle.properties"))
	if err != nil {
		return err
	}
	projectProperties, err := toolCacheReadOptional(core.Join(cwd, "gradle.properties"))
	if err != nil {
		return err
	}
	properties := userProperties
	if !hasGradleCacheProperty(userProperties) && !hasGradleCacheProperty(projectProperties) {
		properties += "\norg.gradle.caching=true\n"
	}
	if err := core.AtomicWriteExisting(core.Join(destination, "gradle.properties"), []byte(properties)); err != nil {
		return err
	}
	// Preserve user initialization, including credentials and repositories,
	// without copying mutable dependency caches or daemon registries.
	for _, name := range []string{"init.gradle", "init.gradle.kts"} {
		path := core.Join(source, name)
		if toolCacheIsFile(path) {
			if err := linkConfiguration(path, core.Join(destination, name)); err != nil {
				return err
			}
		}
	}
	sourceInit := core.Join(source, "init.d")
	if info, err := os.Stat(sourceInit); err == nil && info.IsDir() {
		entries, err := os.ReadDir(sourceInit)
		if err != nil {
			return err
		}
		for _, entry := range entries {
			if err := linkConfiguration(core.Join(sourceInit, entry.Name()), core.Join(init, entry.Name())); err != nil {
				return err
			}
		}
	}
	// Settings scripts run first: a repository's explicitly chosen local
	// directory takes precedence over the automatically shared default.
	encoded, err := workerRuntimeMarshalJSON(core.Join(shared, "gradle"))
	if err != nil {
		return err
	}
	cachePath := strings.ReplaceAll(string(encoded), "$", `\$`)
	script := fmt.Sprintf("gradle.settingsEvaluated { settings ->\n    if (settings.buildCache.local.directory == null) {\n        settings.buildCache.local.directory = new File(%s)\n    }\n}\n", cachePath)
	generated := core.Join(init, "zz-mjolnir-build-cache.gradle")
	if _, err := os.Stat(core.Join(sourceInit, "zz-mjolnir-build-cache.gradle")); err == nil {
		return errors.New("user Gradle initialization uses Mjolnir's reserved init filename")
	}
	if err := core.AtomicWriteExisting(generated, []byte(script)); err != nil {
		return err
	}
	environment["MJ_ORIGINAL_GRADLE_USER_HOME"] = source
	environment["GRADLE_USER_HOME"] = destination
	return nil
}

func toolCacheReadOptional(path string) (string, error) {
	text, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return "", nil
	}
	if err != nil {
		return "", fmt.Errorf("read %s: %w", path, err)
	}
	if !utf8.Valid(text) {
		return "", fmt.Errorf("read %s: stream did not contain valid UTF-8", path)
	}
	return string(text), nil
}

func hasGradleCacheProperty(text string) bool {
	for _, line := range toolCacheLines(text) {
		if strings.HasPrefix(strings.TrimLeftFunc(line, unicode.IsSpace), "org.gradle.caching") {
			return true
		}
	}
	return false
}

func linkConfiguration(source string, destination string) error {
	if target, err := os.Readlink(destination); err == nil && workerRuntimePathEqual(target, source) {
		return nil
	}
	parent := filepath.Dir(destination)
	if parent == "" {
		return errors.New("Gradle configuration has no parent")
	}
	staging, err := os.MkdirTemp(parent, ".tmp")
	if err != nil {
		return err
	}
	defer os.RemoveAll(staging)
	link := filepath.Join(staging, "config")
	if err := os.Symlink(source, link); err != nil {
		return err
	}
	return os.Rename(link, destination)
}
