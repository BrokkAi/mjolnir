// Port of mj-build-cache/src/git_cache.rs.
//
// Host-side Git caches for container-backed sessions.

package buildcache

import (
	"crypto/sha256"
	"fmt"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const cacheContainerRoot = "/run/hel/git-cache"
const cacheRelativeRoot = ".cache/mjolnir/git"
const cacheMaxKib uint64 = 20 * 1024 * 1024

// A successful redirection creates the lock file exclusively. Some mkdir
// implementations report success after the mkdir syscall loses to a peer.
const cacheLockFunctions = `
cache_lock_signal() {
    if [ "$changing_cache_lock" = 0 ]; then
        exit "$1"
    fi
    pending_cache_lock_signal=$1
}
track_cache_locks() {
    held_cache_lock=
    gc_lock=
    changing_cache_lock=0
    pending_cache_lock_signal=
    trap 'cache_lock_signal 129' HUP
    trap 'cache_lock_signal 130' INT
    trap 'cache_lock_signal 143' TERM
}
finish_cache_lock_change() {
    changing_cache_lock=0
    [ -z "$pending_cache_lock_signal" ] || exit "$pending_cache_lock_signal"
}
try_cache_lock() {
    # Defer exit until the recorded owner agrees with the filesystem.
    changing_cache_lock=1
    lock_status=1
    set -C
    if command : 2>/dev/null > "$1"; then
        if [ "${2:-}" = gc ]; then
            gc_lock=$1
        else
            held_cache_lock=$1
        fi
        lock_status=0
    fi
    set +C
    finish_cache_lock_change
    return "$lock_status"
}
release_cache_lock() {
    changing_cache_lock=1
    # The child must finish unlinking and reporting success even when the
    # same signal hits both the shell and its children.
    (trap '' HUP INT TERM; exec rm -- "$held_cache_lock")
    held_cache_lock=
    finish_cache_lock_change
}
cleanup_cache_locks() {
    trap '' HUP INT TERM
    set --
    [ -z "$held_cache_lock" ] || set -- "$held_cache_lock"
    [ -z "$gc_lock" ] || set -- "$@" "$gc_lock"
    if [ "$#" -gt 0 ]; then
        rm -f -- "$@"
    fi
}
`

// gitShellCommand is CacheHost.ShellCommand with this cache's `$0` label and
// lifecycle stage, so every clone-cache command reports as cloning work.
func (h CacheHost) gitShellCommand(script string, arguments []string, purpose string) core.CommandSpec {
	return h.ShellCommand(cacheLockFunctions+"\n"+script, "hel-git-cache", arguments, purpose).
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageCloning})
}

// managedSessions is the sessions engine still has containers for on this
// host. The engine is a property of the runtime, not of the machine, so the
// caller names it.
func (h CacheHost) managedSessions(engine string, executor core.CommandExecutor) ([]string, error) {
	panic("unported: mj-build-cache/src/git_cache.rs CacheHost::managed_sessions")
}

// PreparedCloneCache is a session's prepared clone-cache snapshots on their
// host.
type PreparedCloneCache struct {
	host        CacheHost
	sessionRoot string
}

// Cleanup removes the session's snapshots.
func (c *PreparedCloneCache) Cleanup(executor core.CommandExecutor) error {
	return cleanupSessionRoot(c.host, c.sessionRoot, executor)
}

// ExactCheckout is the repository destination a session checks out at an
// exact commit, and that commit (Rust's `Option<(&str, &str)>` argument of
// prepare).
type ExactCheckout struct {
	Destination string
	Commit      string
}

// Prepare populates session-scoped Git snapshots and attaches them to a
// container bundle (Rust git_cache::prepare). A history cutoff mounts only
// the selected commits and their ancestors. Host-only sources require a
// snapshot. For other sources, preparation failures are visible optimization
// misses and checkout can reach the remote. githubToken and exactCheckout
// are nil when absent.
func Prepare(target *core.TargetsTargetTemplate, sessionID string, bundle *core.ProjectBundleSpec, mounts *[]core.AdditionalMount, githubToken *string, exactCheckout *ExactCheckout, executor core.CommandExecutor) (*PreparedCloneCache, error) {
	panic("unported: mj-build-cache/src/git_cache.rs prepare")
}

// snapshotName names a snapshot. Snapshots share the mirror, but each source
// and selection owns its files.
func snapshotName(source string, cutoff *core.HistoryCutoff, exact *string) string {
	digest := sha256.New()
	digest.Write([]byte(source))
	if cutoff != nil {
		for _, commit := range cutoff.Commits() {
			digest.Write([]byte{0})
			digest.Write([]byte(commit))
		}
		return core.LowerHex(digest.Sum(nil)) + ".bundle"
	}
	if exact != nil {
		digest.Write([]byte{0})
		digest.Write([]byte(*exact))
	}
	return core.LowerHex(digest.Sum(nil)) + ".git"
}

func cacheHome(host CacheHost, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/git_cache.rs cache_home")
}

func repositoryCacheKey(source string) string {
	panic("unported: mj-build-cache/src/git_cache.rs repository_cache_key")
}

// MirrorRepoRelativePath is the home-relative path to the host clone-cache
// bare mirror for source, e.g. `.cache/mjolnir/git/mirrors/<sha256>/repo.git`.
func MirrorRepoRelativePath(source string) string {
	return cacheRelativeRoot + "/mirrors/" + repositoryCacheKey(source) + "/repo.git"
}

const prepareRepositoryScript = `
set -eu
cache_root=$1
key=$2
source=$3
snapshot=$4
has_token=$5
# How Git reaches an SSH remote, as the controller decided it. Empty leaves
# this host's GIT_SSH_COMMAND as it is: on the controller that is the
# operator's own command, inherited exactly.
ssh_command=$6
source_id=$7
host_only=$8
exact_ref=$9
# Any further arguments are a history cutoff: its head, then its base.
shift 9
umask 077
mkdir -p "$cache_root/mirrors" "$cache_root/locks" "$(dirname "$snapshot")"
# The session root's own time is its preparation stamp: until the container
# exists, it is what keeps the cache GC from treating the root as an orphan.
touch -- "$(dirname "$snapshot")"
entry="$cache_root/mirrors/$key"
mirror="$entry/repo.git"
# An overlapping refresh of the same requested URL satisfies this request too.
# GitHub URLs with different transports still share one mirror. The locked read
# is authoritative; this optimistic read may race eviction or publication.
observed_generation=$(cat "$entry/generation" 2>/dev/null || :)
lock="$cache_root/locks/$key"
preparation=$(mktemp -d "$cache_root/mirrors/$key.partial-$$-XXXXXX")
partial="$preparation/mirror"
snapshot_partial="$snapshot.partial-$$"
# Full history must never enter a session's mounted subtree, even temporarily.
pinned="$preparation/pinned.git"
cutoff_repository="$snapshot.cutoff-$$"
cleanup() {
    cleanup_cache_locks
    rm -rf -- "$preparation" "$snapshot_partial" "$cutoff_repository"
}
track_cache_locks
trap cleanup EXIT
attempt=0
while ! try_cache_lock "$lock"; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 1500 ]; then
        echo "timed out waiting for clone-cache lock $key" >&2
        exit 75
    fi
    sleep 0.2
done
if [ "$has_token" = 1 ]; then
    IFS= read -r GH_TOKEN || exit 1
    export GH_TOKEN
fi
export GIT_TERMINAL_PROMPT=0
if [ -n "$ssh_command" ]; then
    export GIT_SSH_COMMAND="$ssh_command"
fi
git_remote() {
    if [ "$has_token" = 1 ]; then
        git -c credential.https://github.com.helper= -c 'credential.https://github.com.helper=!f() { if [ "$1" = get ]; then printf "%s\n" username=x-access-token "password=$GH_TOKEN"; fi; }; f' "$@"
    else
        git "$@"
    fi
}
generation=$(cat "$entry/generation" 2>/dev/null || :)
next_generation="$source_id:${preparation##*/}"
if [ ! -d "$mirror" ]; then
    rm -rf -- "$entry"
    mkdir "$partial"
    git_remote clone --mirror -- "$source" "$partial/repo.git"
    git -C "$partial/repo.git" config gc.auto 0
    printf '%s\n' "$next_generation" > "$partial/generation"
    mv -- "$partial" "$entry"
elif [ "${generation%%:*}" != "$source_id" ] || [ "$generation" = "$observed_generation" ]; then
    git_remote -C "$mirror" fetch --prune -- "$source" '+refs/*:refs/*'
    # Only a host-only checkout reads the mirror's default branch.
    if [ "$host_only" = 1 ]; then
        advertised_head=$(git_remote -C "$mirror" ls-remote --symref -- "$source" HEAD)
        default_ref=$(printf '%s\n' "$advertised_head" | awk '$1 == "ref:" && $3 == "HEAD" { print $2 }')
        if [ -n "$default_ref" ]; then
            git -C "$mirror" symbolic-ref HEAD "$default_ref"
        elif [ -n "$advertised_head" ]; then
            detached_head=$(printf '%s\n' "$advertised_head" | awk '$2 == "HEAD" { print $1 }')
            if ! git -C "$mirror" cat-file -e "$detached_head^{commit}" 2>/dev/null; then
                # A detached HEAD may be outside every mirrored ref.
                git_remote -C "$mirror" fetch --no-tags -- "$source" HEAD
            fi
            git -C "$mirror" update-ref --no-deref HEAD "$detached_head"
        fi
    fi
    printf '%s\n' "$next_generation" > "$entry/generation"
fi
: > "$entry/last-used"
# Own every object and ref before releasing the mirror to refresh or eviction.
# Git hardlinks immutable objects; this private repository needs no shared lock
# while packing a cutoff bundle or making the mounted snapshot readable.
git clone --mirror --local -- "$mirror" "$pinned"
release_cache_lock
rm -rf -- "$snapshot" "$snapshot_partial"
if [ "$#" -eq 0 ]; then
    if [ -n "$exact_ref" ]; then
        commit=${exact_ref##*/}
        if ! git -C "$pinned" cat-file -e "$commit^{commit}" 2>/dev/null; then
            git_remote -C "$pinned" fetch --quiet --no-tags -- "$source" "$commit"
        fi
        git -C "$pinned" update-ref "$exact_ref" "$commit"
    fi
    mv -- "$pinned" "$snapshot_partial"
else
    # Copy only the cutoff commits and their history out of the private pin,
    # which holds every later commit too. A fetch sends only what the wanted
    # commits reach.
    git init --quiet --bare -- "$cutoff_repository"
    # The names HistoryCutoff::BUNDLE_HEAD_REF and BASE_REF give them, which
    # the checkout fetches by.
    refspecs=
    refs=
    name=head
    for commit do
        case $commit in
            ''|*[!0-9a-f]*) echo "invalid history cutoff commit" >&2; exit 2 ;;
        esac
        if ! git -C "$pinned" cat-file -e "$commit^{commit}" 2>/dev/null; then
            git_remote -C "$pinned" fetch --quiet --no-tags -- "$source" "$commit"
        fi
        refspecs="$refspecs $commit:refs/mj/$name"
        refs="$refs refs/mj/$name"
        name=base
    done
    # Object IDs and these ref names contain no spaces, so splitting is safe.
    # shellcheck disable=SC2086
    git -C "$cutoff_repository" fetch --quiet --no-tags \
        --upload-pack='git -c uploadpack.allowAnySHA1InWant=true upload-pack' \
        -- "$pinned" $refspecs
    # shellcheck disable=SC2086
    git -C "$cutoff_repository" bundle create "$snapshot_partial" $refs
    rm -rf -- "$cutoff_repository" "$pinned"
fi
mv -- "$snapshot_partial" "$snapshot"
# A bind mount can expose the host owner as container root, while the worker
# runs as an unprivileged user. The private cache parents still protect these
# files on the host; the mounted session subtree must be readable.
chmod -R a+rX "$snapshot"
chmod a+rx "$(dirname "$snapshot")"
`

func prepareRepository(host CacheHost, cacheRoot, key, source, snapshot string, cutoff *core.HistoryCutoff, exact *string, githubToken *string, executor core.CommandExecutor) error {
	panic("unported: mj-build-cache/src/git_cache.rs prepare_repository")
}

const cacheGCScript = `
set -eu
root=$1
limit_kib=$2
shift 2
umask 077
mkdir -p "$root/mirrors" "$root/locks" "$root/sessions"
marker="$root/last-gc"
if [ -f "$marker" ] && find "$marker" -mmin -1440 -print | grep -q .; then
    exit 0
fi
# Reap old lock files and stale lock directories from earlier builds.
find "$root/locks" -mindepth 1 -maxdepth 1 \( -type f -o -type d \) -mmin +1440 -exec rm -rf {} \;
candidates="$root/gc-candidates-$$"
track_cache_locks
trap 'cleanup_cache_locks; rm -f -- "$candidates"' EXIT
try_cache_lock "$root/locks/gc" gc || exit 0
# An interrupted eviction may leave an unpublished mirror behind.
rm -rf -- "$root"/mirrors/.trash-*
evict_cache_mirror() {
    trash=
    if [ -d "$1" ]; then
        trash=$(mktemp -d "$root/mirrors/.trash-$$-XXXXXX")
        # Rename within mirrors so a reader sees either the whole entry or no
        # entry, even if deleting the unpublished directory is interrupted.
        mv -- "$1" "$trash/mirror"
    fi
    release_cache_lock
    if [ -n "$trash" ]; then
        rm -rf -- "$trash"
    fi
}
for snapshot in "$root"/sessions/*; do
    [ -d "$snapshot" ] || continue
    session=${snapshot##*/}
    live=0
    for active in "$@"; do
        if [ "$session" = "$active" ]; then
            live=1
            break
        fi
    done
    # A session without a container may still be between preparation and
    # container start, so only the root's own preparation stamp ages it. Its
    # snapshots hardlink mirror objects and inherit their older times.
    if [ "$live" -eq 0 ] && find "$snapshot" -maxdepth 0 -mmin +1440 -print | grep -q .; then
        rm -rf -- "$snapshot"
    fi
done
for entry in "$root"/mirrors/*; do
    [ -d "$entry" ] || continue
    key=${entry##*/}
    [ -f "$entry/last-used" ] || continue
    if find "$entry/last-used" -mmin +43200 -print | grep -q . && try_cache_lock "$root/locks/$key"; then
        evict_cache_mirror "$entry"
    fi
done
size=$(du -sk "$root/mirrors" | awk '{print $1}')
if [ "$size" -gt "$limit_kib" ]; then
    ls -1tr "$root"/mirrors/*/last-used > "$candidates" 2>/dev/null || true
    while IFS= read -r used; do
        [ -n "$used" ] || continue
        entry=${used%/last-used}
        key=${entry##*/}
        if try_cache_lock "$root/locks/$key"; then
            evict_cache_mirror "$entry"
            size=$(du -sk "$root/mirrors" | awk '{print $1}')
            [ "$size" -le "$limit_kib" ] && break
        fi
    done < "$candidates"
fi
: > "$marker"
`

func collectGarbage(host CacheHost, cacheRoot string, liveSessions []string, executor core.CommandExecutor) error {
	return collectGarbageWithLimit(host, cacheRoot, liveSessions, cacheMaxKib, executor)
}

func collectGarbageWithLimit(host CacheHost, cacheRoot string, liveSessions []string, limitKib uint64, executor core.CommandExecutor) error {
	arguments := append([]string{cacheRoot, strconv.FormatUint(limitKib, 10)}, liveSessions...)
	command := host.gitShellCommand(cacheGCScript, arguments, "prune the container host Git cache")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	_, err = checked(output, &command)
	return err
}

func cleanupSessionRoot(host CacheHost, sessionRoot string, executor core.CommandExecutor) error {
	command := host.gitShellCommand(
		"set -eu; root=$1; case $root in */.cache/mjolnir/git/sessions/*) rm -rf -- \"$root\" ;; *) echo 'refusing unsafe clone-cache cleanup path' >&2; exit 2 ;; esac",
		[]string{sessionRoot},
		"remove failed session Git cache snapshot",
	)
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	_, err = checked(output, &command)
	return err
}

// checked is output when the command succeeded, its failure otherwise.
func checked(output core.CommandOutput, command *core.CommandSpec) (core.CommandOutput, error) {
	if output.Status != 0 {
		return core.CommandOutput{}, fmt.Errorf("%s failed with status %d: %s",
			command.Purpose, output.Status, trimmedLossy(output.Stderr))
	}
	return output, nil
}
