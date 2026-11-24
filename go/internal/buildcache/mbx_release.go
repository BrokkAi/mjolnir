// Port of mj-build-cache/src/mbx/release.rs.
//
// Telling mbx that a workspace Mjolnir removed will never be built again.
//
// mbx keys a managed target directory and its learned incremental state by
// the workspace path and nothing else. It collects them on its own only when
// it can prove the checkout is gone from the filesystem that holds its target
// root. A container's `/workspace/<id>` never passes that test from the host
// or from another container, and neither does a checkout on a different
// filesystem from the target root, so their build output would wait for
// `target.max_age` (30 days by default) or for disk pressure. The code that
// removes a workspace therefore says so, with `mbx clean --under <root>`.
//
// BuildStateRelease is the one place that decides what to release and how.
// A release runs after the workspace and every process that could build in it
// are gone: the worker's process group, the container, then the files, and
// only then mbx's state. A build can no longer recreate what it removes, and
// `mbx clean --under` removes state recorded for the workspace and nested
// worktrees. A release is bounded by its own deadline, and a failure is
// reported, never returned: it must not fail or block the teardown that asked
// for it.
//
// Rust's test-only BuildStateRelease::for_target lives in this file's tests.

package buildcache

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"strings"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// releaseLabel is `$0` for the release script, so it is recognizable in a
// process list.
const releaseLabel = "mj-mbx-release"

// releaseTimeout bounds a release: remote cleanup can take longer than
// ordinary teardown commands, especially when mbx is running over SSH.
const releaseTimeout = 120 * time.Second

// absent is what stdout says when a bare host has no mbx at all, so there is
// no build state to release.
const absent = "mj-mbx: absent"

// failuresFile holds failed releases, one JSON object per line, under the
// data directory.
const failuresFile = "mbx-release-failures.jsonl"

// failuresMaxBytes: the journal starts over past this size; it reports
// recent failures, not history.
const failuresMaxBytes uint64 = 256 * 1024

// FailureReportSecs is how long `mj doctor` keeps reporting a failed
// release. mbx's own age limit reclaims the output after 30 days by default.
const FailureReportSecs uint64 = 14 * 24 * 60 * 60

// `$1` mode (`native` or `shared`), `$2` the shared cache directory, `$3`
// its configuration home, `$4` the preferred in-cache mbx path, `$5` the
// old local binary root, `$6` the optional workspace root; the remaining
// arguments are workspaces for older mbx versions.
//
// A shared cache prefers the host executable its container mounted, then
// searches the same locations as the native host probe. A bare host uses
// that search directly.
//
// A bare workspace path is resolved through its nearest existing ancestor,
// because mbx records the physical path Cargo reported from inside the
// checkout, and the checkout itself is already gone. A container root is
// passed as written: it was never a path on this host.
//
// The script starts in the home directory, which a removed checkout cannot
// be, so neither the shell nor mbx inherits a deleted working directory. A
// bare SSH workspace is relative to that home.
const releaseScript = `cd "${HOME:-/}" 2>/dev/null || cd / || exit 1
trap '' HUP
set -u
mode=$1 cache=$2 config=$3 preferred=$4 legacy_root=$5 under_root=$6
shift 6
mbx=
consider() {
    [ -z "$mbx" ] || return 0
    [ -n "$1" ] && [ -x "$1" ] || return 0
    "$1" --version >/dev/null 2>&1 || return 0
    mbx=$1
}
physical() {
    dir=$1 rest=
    while [ ! -d "$dir" ]; do
        parent=$(dirname -- "$dir")
        [ "$parent" != "$dir" ] || break
        rest=/$(basename -- "$dir")$rest
        dir=$parent
    done
    resolved=$(CDPATH= cd -- "$dir" && pwd -P) || return 1
    printf '%s%s\n' "${resolved%/}" "$rest"
}
if [ "$mode" = shared ]; then
    consider "$preferred"
fi
consider "$(command -v mbx 2>/dev/null || true)"
consider "$HOME/.local/bin/mbx"
consider "$HOME/.cargo/bin/mbx"
if [ -z "$mbx" ] && [ "$mode" = shared ]; then
    if [ -n "$legacy_root" ]; then
        for candidate in "$legacy_root"/*/*/mbx; do
            consider "$candidate"
        done
    else
        for candidate in "$HOME/.cache/mjolnir/mbx"/*/mbx; do
            consider "$candidate"
        done
    fi
fi
if [ -z "$mbx" ]; then
    if [ "$mode" = native ]; then
        echo 'mj-mbx: absent'
        exit 0
    fi
    echo 'no mbx on this host can release the shared build cache' >&2
    exit 3
fi
run_clean() (
    output_dir=$(mktemp -d "${TMPDIR:-/tmp}/mj-mbx-release.XXXXXX") || {
        echo 'could not create temporary output directory for mbx clean' >&2
        return 1
    }
    if [ "$mode" = shared ]; then
        MBX_CACHE_DIR=$cache XDG_CONFIG_HOME=$config "$mbx" clean "$@" </dev/null \
            >"$output_dir/stdout" 2>"$output_dir/stderr"
    else
        "$mbx" clean "$@" </dev/null >"$output_dir/stdout" 2>"$output_dir/stderr"
    fi
    clean_status=$?
    cat "$output_dir/stdout"
    cat "$output_dir/stderr" >&2
    rm -rf "$output_dir"
    return "$clean_status"
)
status=0
if [ -n "$under_root" ]; then
    if [ "$mode" = shared ]; then
        help=$(MBX_CACHE_DIR=$cache XDG_CONFIG_HOME=$config "$mbx" clean --help 2>&1 || true)
    else
        help=$("$mbx" clean --help 2>&1 || true)
    fi
    case "$help" in
        *--under*)
            if [ "$mode" = shared ]; then
                root=$under_root
            elif root=$(physical "$under_root"); then
                :
            else
                echo "could not resolve $under_root" >&2
                exit 1
            fi
            run_clean --under "$root" || status=$?
            exit "$status"
            ;;
    esac
fi
for workspace in "$@"; do
    if [ "$mode" = shared ]; then
        run_clean "$workspace" || status=$?
    elif path=$(physical "$workspace"); then
        run_clean "$path" || status=$?
    else
        echo "could not resolve $workspace" >&2
        status=1
    fi
done
exit "$status"`

// releaseStore is which mbx owns the build state of the released workspaces
// (Rust enum Store): a bare host's own mbx, with its own configuration
// (Native), or, when shared is true, the cache directory a host's containers
// mount at the same path (Shared).
type releaseStore struct {
	shared    bool
	directory string
}

// BuildStateRelease is the build state of workspaces Mjolnir has removed,
// and where it lives.
type BuildStateRelease struct {
	host         CacheHost
	store        releaseStore
	preferredMbx *string
	// legacyMbxRoot is the old per-version cache used by local shared-cache
	// sessions. Remote copies remain under the SSH user's home directory.
	legacyMbxRoot *string
	// cleanupRoot is the session's own workspace root. Newer mbx can release
	// every target and incremental directory recorded at or below this path.
	cleanupRoot *string
	// workspaces is the repository paths used when the host has an older mbx
	// without `clean --under`.
	workspaces []string
}

// BuildStateReleaseManagedCheckout is a managed clone or linked worktree, on
// this machine (ssh nil) or an SSH host.
func BuildStateReleaseManagedCheckout(ssh *core.SshTarget, root string) BuildStateRelease {
	cleanupRoot := root
	return BuildStateRelease{
		host:        CacheHost{SSH: ssh},
		store:       releaseStore{},
		cleanupRoot: &cleanupRoot,
		workspaces:  []string{root},
	}
}

// BuildStateReleaseForEnvironment is the workspace root and fallback
// repository paths for a session's own target: an SSH workspace, or a
// container that mounted the shared build cache. nil for a target another
// session owns, one that never had build state Mjolnir can name, and a
// container that ran without the cache.
func BuildStateReleaseForEnvironment(sessionID string, placement *store.EnvironmentPlacement, backend *core.TargetsTargetLocator, config *core.Config) *BuildStateRelease {
	host, root, ok := environmentWorkspaceRoot(sessionID, placement, backend)
	if !ok {
		return nil
	}
	var releaseFrom releaseStore
	var preferredMbx *string
	if backend.Kind != core.TargetsTargetLocatorSshBare {
		cache := placement.BuildCache
		if cache == nil {
			return nil
		}
		if host.Key() != cache.Host {
			slog.Warn("the build cache was recorded on another host, so its state is not released",
				"session_id", sessionID, "recorded", cache.Host, "actual", host.Key())
			return nil
		}
		releaseFrom = releaseStore{shared: true, directory: cache.Directory}
		preferred := CacheBinaryPath(cache.Directory)
		preferredMbx = &preferred
	}
	var repositories []string
	switch {
	case placement.Repositories != nil:
		repositories = *placement.Repositories
	default:
		bundle, ok := config.Bundles[placement.BundleID]
		if !ok {
			return nil
		}
		for _, repository := range bundle.Repositories {
			repositories = append(repositories, repository.Destination)
		}
	}
	workspaces := make([]string, 0, len(repositories))
	for _, destination := range repositories {
		workspaces = append(workspaces, core.Join(root, destination))
	}
	var legacyMbxRoot *string
	if releaseFrom.shared && host.IsLocal() {
		legacy := pathJoin(core.DataDir(), "mbx")
		legacyMbxRoot = &legacy
	}
	if len(workspaces) == 0 {
		return nil
	}
	return &BuildStateRelease{
		host:          host,
		store:         releaseFrom,
		preferredMbx:  preferredMbx,
		legacyMbxRoot: legacyMbxRoot,
		cleanupRoot:   &root,
		workspaces:    workspaces,
	}
}

// Excluding is this release without anything under the workspace root of
// the target the session goes on running in (liveSession on liveBackend;
// liveSession nil for none), when that is on the same host. nil when nothing
// is left to release.
//
// mbx keys state by path alone, and a session keeps its id when it moves, so
// the target a Move retires can hold the same paths as the one the session
// continues in. Any path under the live root is kept, whether or not the
// live target has the build cache.
func (r BuildStateRelease) Excluding(liveSession *core.SessionRecord, liveBackend *core.TargetsTargetLocator) *BuildStateRelease {
	if liveSession != nil {
		if host, root, ok := workspaceRoot(liveSession, liveBackend); ok && host.Key() == r.host.Key() {
			if r.cleanupRoot != nil && (pathStartsWith(*r.cleanupRoot, root) || pathStartsWith(root, *r.cleanupRoot)) {
				r.cleanupRoot = nil
			}
			kept := []string{}
			for _, workspace := range r.workspaces {
				if !pathStartsWith(workspace, root) {
					kept = append(kept, workspace)
				}
			}
			r.workspaces = kept
		}
	}
	if len(r.workspaces) == 0 && r.cleanupRoot == nil {
		return nil
	}
	return &r
}

// Run releases the target root, or each known workspace for older mbx.
// Never fails: a release that does not complete is logged, shown to the
// person, and recorded for `mj doctor`.
func (r *BuildStateRelease) Run(executor core.CommandExecutor) {
	if !enabled() {
		return
	}
	command := r.command()
	output, err := executor.ExecuteCleanupWithTimeout(&command, releaseTimeout)
	var failure string
	notConfirmed := false
	switch {
	case err == nil && output.Status == 0:
		stdout := lossy(output.Stdout)
		if strings.TrimSpace(stdout) == absent {
			slog.Debug("no mbx on this host, so there is no build state to release", "host", r.host.Key())
		} else {
			// mbx exits 0 when it keeps a target a running command holds, and
			// says so on stderr.
			slog.Info("released the mbx build state of removed workspaces",
				"host", r.host.Key(), "workspaces", r.workspaces,
				"output", strings.TrimSpace(stdout), "warnings", trimmedLossy(output.Stderr))
		}
		return
	case err == nil:
		failure = fmt.Sprintf("exit status %d: %s", output.Status, trimmedLossy(output.Stderr))
	case cleanupWasInterrupted(err):
		failure = fmt.Sprintf("mj stopped waiting for mbx clean; it may still be running: %s", alternate(err))
		notConfirmed = true
	default:
		failure = alternate(err)
	}
	workspaces := strings.Join(r.workspaces, ", ")
	if notConfirmed {
		slog.Warn("mj stopped waiting for mbx clean; it may still be running",
			"host", r.host.Key(), "workspaces", workspaces, "error", failure)
		executor.NotifyNotice(fmt.Sprintf("mj stopped waiting for mbx clean on %s; it may still be running. "+
			"`mj doctor` shows how to retry it for %s.", r.host.Key(), workspaces))
	} else {
		slog.Warn("mbx kept the build state of removed workspaces",
			"host", r.host.Key(), "workspaces", workspaces, "error", failure)
		executor.NotifyNotice(fmt.Sprintf("The Rust build cache on %s still holds build output for %s: %s. "+
			"`mj doctor` shows how to remove it.", r.host.Key(), workspaces, failure))
	}
	if err := recordFailure(r, failure); err != nil {
		slog.Warn("could not record the failed mbx release for mj doctor", "error", alternate(err))
	}
}

func (r *BuildStateRelease) command() core.CommandSpec {
	mode, directory, configHome := "native", "", ""
	if r.store.shared {
		mode, directory, configHome = "shared", r.store.directory, configurationHome(r.store.directory)
	}
	text := func(value *string) string {
		if value == nil {
			return ""
		}
		return *value
	}
	arguments := []string{mode, directory, configHome, text(r.preferredMbx), text(r.legacyMbxRoot), text(r.cleanupRoot)}
	arguments = append(arguments, r.workspaces...)
	return r.host.ShellCommand(releaseScript, releaseLabel, arguments,
		"release the mbx build state of removed workspaces").PreserveChildrenOnCancel()
}

// remediation is the remediation `mj doctor` prints for this release.
func (r *BuildStateRelease) remediation() string {
	environment := ""
	if r.store.shared {
		environment = fmt.Sprintf("MBX_CACHE_DIR=%s XDG_CONFIG_HOME=%s ", r.store.directory, configurationHome(r.store.directory))
	}
	commands := make([]string, 0, len(r.workspaces))
	for _, workspace := range r.workspaces {
		commands = append(commands, environment+"mbx clean "+workspace)
	}
	fallback := strings.Join(commands, "; ")
	if r.cleanupRoot == nil {
		return fallback
	}
	return fmt.Sprintf("%smbx clean --under %s (older mbx: %s)", environment, *r.cleanupRoot, fallback)
}

// workspaceRoot is the host and directory under which a session's own target
// checks out its repositories, for the targets whose build state Mjolnir can
// name: an SSH workspace of a bundle session, and a container's per-session
// workspace. A borrowed target belongs to another session and has none of
// its own.
func workspaceRoot(session *core.SessionRecord, backend *core.TargetsTargetLocator) (CacheHost, string, bool) {
	placement := store.EnvironmentPlacementFromSession(session)
	return environmentWorkspaceRoot(session.ID, &placement, backend)
}

func environmentWorkspaceRoot(sessionID string, placement *store.EnvironmentPlacement, backend *core.TargetsTargetLocator) (CacheHost, string, bool) {
	if core.IsBorrowed(backend) {
		return CacheHost{}, "", false
	}
	switch backend.Kind {
	case core.TargetsTargetLocatorSshBare:
		// The root is released with `mbx clean --under`, so it must be this
		// session's alone: a shared directory would release every session
		// in it.
		if placement.ProjectDirectory == nil && core.VerifySessionWorkspace(backend.Workspace, sessionID) == nil {
			ssh := backend.SSH.Clone()
			return CacheHost{SSH: &ssh}, backend.Workspace, true
		}
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorLocalDocker,
		core.TargetsTargetLocatorSshPodman, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer:
		// Only a per-session workspace: the legacy shared `/workspace` would
		// name every legacy session's checkout at once.
		if placement.ContainerWorkspace == nil {
			return CacheHost{}, "", false
		}
		workspace := *placement.ContainerWorkspace
		expected, err := core.NewContainerWorkspace(sessionID)
		if err != nil || !pathEqual(workspace, expected) {
			return CacheHost{}, "", false
		}
		host, ok := HostForLocator(backend)
		if !ok {
			return CacheHost{}, "", false
		}
		return host, workspace, true
	}
	return CacheHost{}, "", false
}

// configurationHome is the `XDG_CONFIG_HOME` under which the containers' mbx
// finds the machine policy Mjolnir publishes in the shared cache,
// `<home>/mbx/config.toml`.
func configurationHome(directory string) string {
	configuration := core.BuildCacheConfigurationDirectory(directory)
	if parent, ok := pathParent(configuration); ok {
		return parent
	}
	return configuration
}

// failure is one line of the failures journal (Rust struct Failure).
type failure struct {
	AtSecs      uint64 `json:"at_secs"`
	Host        string `json:"host"`
	Error       string `json:"error"`
	Remediation string `json:"remediation"`
}

// UnmarshalJSON requires every field, as serde does.
func (f *failure) UnmarshalJSON(data []byte) error {
	var wire struct {
		AtSecs      *uint64 `json:"at_secs"`
		Host        *string `json:"host"`
		Error       *string `json:"error"`
		Remediation *string `json:"remediation"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	for _, field := range []struct {
		name    string
		present bool
	}{
		{"at_secs", wire.AtSecs != nil}, {"host", wire.Host != nil},
		{"error", wire.Error != nil}, {"remediation", wire.Remediation != nil},
	} {
		if !field.present {
			return fmt.Errorf("missing field `%s`", field.name)
		}
	}
	*f = failure{AtSecs: *wire.AtSecs, Host: *wire.Host, Error: *wire.Error, Remediation: *wire.Remediation}
	return nil
}

func failuresPath() string {
	return pathJoin(core.DataDir(), failuresFile)
}

// recordFailure appends one line. A line this short is one `write` on an
// append-mode file, so concurrent teardowns do not interleave.
func recordFailure(release *BuildStateRelease, errorText string) error {
	path := failuresPath()
	if parent, ok := pathParent(path); ok && parent != "" {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return err
		}
	}
	info, err := os.Stat(path)
	restart := err == nil && uint64(info.Size()) > failuresMaxBytes
	flags := os.O_CREATE | os.O_WRONLY | os.O_APPEND
	if restart {
		flags = os.O_CREATE | os.O_WRONLY | os.O_TRUNC
	}
	file, err := os.OpenFile(path, flags, 0o666)
	if err != nil {
		return err
	}
	defer file.Close()
	errorRunes := []rune(errorText)
	if len(errorRunes) > 512 {
		errorRunes = errorRunes[:512]
	}
	var line bytes.Buffer
	encoder := json.NewEncoder(&line)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(failure{
		AtSecs:      nowSecs(),
		Host:        release.host.Key(),
		Error:       string(errorRunes),
		Remediation: release.remediation(),
	}); err != nil {
		return err
	}
	_, err = file.Write(line.Bytes())
	return err
}

func nowSecs() uint64 {
	seconds := time.Now().Unix()
	if seconds < 0 {
		return 0
	}
	return uint64(seconds)
}

// cleanupWasInterrupted is whether any error in the chain says the cleanup
// was cancelled.
func cleanupWasInterrupted(err error) bool {
	for ; err != nil; err = errors.Unwrap(err) {
		if strings.HasPrefix(err.Error(), "operation cancelled") {
			return true
		}
	}
	return false
}

// ReleaseFailure is a release that failed recently, for `mj doctor`.
type ReleaseFailure struct {
	Host        string
	Error       string
	Remediation string
}

// RecentReleaseFailures is the releases that failed within
// FailureReportSecs, newest last.
func RecentReleaseFailures() []ReleaseFailure {
	failures := []ReleaseFailure{}
	data, err := os.ReadFile(failuresPath())
	if err != nil || !utf8.Valid(data) {
		return failures
	}
	now := nowSecs()
	var since uint64
	if now > FailureReportSecs {
		since = now - FailureReportSecs
	}
	for _, line := range rustLines(string(data)) {
		var recorded failure
		if err := json.Unmarshal([]byte(line), &recorded); err != nil || recorded.AtSecs < since {
			continue
		}
		failures = append(failures, ReleaseFailure{
			Host:        recorded.Host,
			Error:       recorded.Error,
			Remediation: recorded.Remediation,
		})
	}
	return failures
}

// releasesEnabledForTest is Rust's thread-local ENABLED. Most controller
// tests tear sessions down through the real process executor, and a release
// would then run this machine's own mbx against the developer's live build
// cache. Releases stay off in tests unless a test turns them on. Go has no
// thread-locals, so the switch is process-wide: a test that turns releases
// on must not run in parallel with one that relies on them being off.
var releasesEnabledForTest atomic.Bool

func enabled() bool {
	if cfgTest || core.TestFixturesEnabled() {
		return releasesEnabledForTest.Load()
	}
	return true
}

// EnableForTest turns releases on in tests until the returned restore
// function runs (Rust: until the returned guard drops).
func EnableForTest() (restore func()) {
	releasesEnabledForTest.Store(true)
	return func() { releasesEnabledForTest.Store(false) }
}
