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
	"errors"
	"strings"
	"sync/atomic"
	"time"

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
	panic("unported: mj-build-cache/src/mbx/release.rs BuildStateRelease::for_environment")
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
	panic("unported: mj-build-cache/src/mbx/release.rs BuildStateRelease::excluding")
}

// Run releases the target root, or each known workspace for older mbx.
// Never fails: a release that does not complete is logged, shown to the
// person, and recorded for `mj doctor`.
func (r *BuildStateRelease) Run(executor core.CommandExecutor) {
	panic("unported: mj-build-cache/src/mbx/release.rs BuildStateRelease::run")
}

func (r *BuildStateRelease) command() core.CommandSpec {
	panic("unported: mj-build-cache/src/mbx/release.rs BuildStateRelease::command")
}

// remediation is the remediation `mj doctor` prints for this release.
func (r *BuildStateRelease) remediation() string {
	panic("unported: mj-build-cache/src/mbx/release.rs BuildStateRelease::remediation")
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
	panic("unported: mj-build-cache/src/mbx/release.rs environment_workspace_root")
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
	panic("unported: mj-build-cache/src/mbx/release.rs Failure::deserialize")
}

func failuresPath() string {
	return pathJoin(core.DataDir(), failuresFile)
}

// recordFailure appends one line. A line this short is one `write` on an
// append-mode file, so concurrent teardowns do not interleave.
func recordFailure(release *BuildStateRelease, errorText string) error {
	panic("unported: mj-build-cache/src/mbx/release.rs record_failure")
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
	panic("unported: mj-build-cache/src/mbx/release.rs recent_release_failures")
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
