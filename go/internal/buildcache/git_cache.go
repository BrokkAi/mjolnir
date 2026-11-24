// Port of mj-build-cache/src/git_cache.rs.
//
// Host-side Git caches for container-backed sessions.

package buildcache

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"maps"
	"slices"
	"strconv"
	"strings"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	var remote []string
	if engine == "container" {
		remote = []string{"container", "list", "--all", "--format", "json"}
	} else {
		remote = []string{engine, "ps", "--all", "--filter", "label=" + core.ManagedLabel + "=true", "--format", "json"}
	}
	command := h.Command(remote, "find live container Git cache snapshots").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageCloning})
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	output, err = checked(output, &command)
	if err != nil {
		return nil, err
	}
	managed, err := targets.ManagedSessionsFromContainerJSON(output.Stdout)
	if err != nil {
		return nil, err
	}
	sessions := make([]string, 0, len(managed))
	for _, session := range managed {
		sessions = append(sessions, session.SessionID)
	}
	return sessions, nil
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
	host, hostOK := CacheHostForTarget(target)
	engine, engineOK := target.ContainerEngine()
	if !hostOK || !engineOK {
		return nil, nil
	}
	if err := bundle.Validate(); err != nil {
		return nil, err
	}
	exactFor := func(destination, source string, cutoff *core.HistoryCutoff) *string {
		if exactCheckout != nil && exactCheckout.Destination == destination && cutoff == nil && core.IsHostOnlyGitSource(source) {
			commit := exactCheckout.Commit
			return &commit
		}
		return nil
	}
	type snapshotSource struct {
		source   string
		cutoff   *core.HistoryCutoff
		exact    *string
		required bool
	}
	// Keyed and ordered by snapshot name, as Rust's BTreeMap is; a later
	// repository with the same name replaces an earlier one.
	repositories := map[string]snapshotSource{}
	for _, repository := range bundle.Repositories {
		source := *repository.URL
		cutoff := repository.HistoryCutoff
		exact := exactFor(repository.Destination, source, cutoff)
		repositories[snapshotName(source, cutoff, exact)] = snapshotSource{
			source: source, cutoff: cutoff, exact: exact, required: core.IsHostOnlyGitSource(source),
		}
	}
	names := slices.Sorted(maps.Keys(repositories))
	var required []string
	for _, name := range names {
		if repositories[name].required {
			required = append(required, core.DisplayURL(repositories[name].source))
		}
	}
	requiredSources := strings.Join(required, ", ")
	for _, mount := range *mounts {
		if pathStartsWith(mount.Destination, cacheContainerRoot) || pathStartsWith(cacheContainerRoot, mount.Destination) {
			if requiredSources != "" {
				return nil, core.RefusalPrecondition(fmt.Sprintf(
					"Cannot mount the host-only Git sources %s. Move the attached directory that overlaps %s.",
					requiredSources, cacheContainerRoot))
			}
			executor.NotifyNotice("Clone cache disabled because an attached directory overlaps /run/hel/git-cache.")
			return nil, nil
		}
	}

	stage := core.NewProvisionStageGuard(executor, core.ProvisionStage{Kind: core.ProvisionStageCloning})
	defer stage.End()
	home, err := cacheHome(host, executor)
	if err != nil {
		if requiredSources != "" {
			return nil, core.RefusalPrecondition(fmt.Sprintf(
				"Cannot prepare host-only Git sources %s on the container host. Make sure Git and sh are available and the host has an absolute home directory.",
				requiredSources)).Wrap(err)
		}
		executor.NotifyNotice(fmt.Sprintf("Clone cache unavailable on the container host: %s; using direct clones.", alternate(err)))
		return nil, nil
	}
	cacheRoot := pathJoin(home, cacheRelativeRoot)
	sessionRoot := pathJoin(pathJoin(cacheRoot, "sessions"), sessionID)

	type prepared struct {
		name      string
		source    string
		required  bool
		reference string
		err       error
	}
	results := make([]prepared, len(names))
	var running sync.WaitGroup
	for index, name := range names {
		entry := repositories[name]
		results[index] = prepared{name: name, source: entry.source, required: entry.required}
		running.Add(1)
		go func() {
			defer running.Done()
			defer func() {
				if payload := recover(); payload != nil {
					results[index].err = fmt.Errorf("clone-cache preparation thread panicked: %s", core.CommandThreadPanicMessage(payload))
				}
			}()
			snapshot := pathJoin(sessionRoot, name)
			err := prepareRepository(host, cacheRoot, repositoryCacheKey(entry.source), entry.source, snapshot,
				entry.cutoff, entry.exact, githubToken, executor)
			if err != nil {
				results[index].err = err
				return
			}
			results[index].reference = cacheContainerRoot + "/" + name
		}()
	}
	running.Wait()

	references := map[string]string{}
	var failures []error
	for _, result := range results {
		switch {
		case result.err == nil:
			references[result.name] = result.reference
		case result.required:
			failures = append(failures, core.RefusalPrecondition(fmt.Sprintf(
				"Cannot prepare %s on the container host. Verify that the host can fetch the source with Git and write its cache. For a pinned checkout, verify that the requested commits are available from the source.",
				core.DisplayURL(result.source))).Wrap(result.err))
		default:
			executor.NotifyNotice(fmt.Sprintf("Clone cache preparation failed: %s; using a direct checkout for that repository.", alternate(result.err)))
		}
	}
	if len(failures) > 0 || len(references) == 0 {
		if err := cleanupSessionRoot(host, sessionRoot, executor); err != nil {
			executor.NotifyNotice(fmt.Sprintf("Could not remove the failed Git checkout snapshot: %s", alternate(err)))
		}
		if len(failures) > 0 {
			for _, additional := range failures[1:] {
				executor.NotifyNotice(alternate(additional))
			}
			return nil, failures[0]
		}
		return nil, nil
	}
	for index := range bundle.Repositories {
		repository := &bundle.Repositories[index]
		source := *repository.URL
		cutoff := repository.HistoryCutoff
		name := snapshotName(source, cutoff, exactFor(repository.Destination, source, cutoff))
		repository.Reference = nil
		if reference, ok := references[name]; ok {
			repository.Reference = &reference
		}
	}
	*mounts = append(*mounts, core.AdditionalMount{
		Source:      sessionRoot,
		Destination: cacheContainerRoot,
		Access:      core.MountAccessRo,
	})

	liveSessions, err := host.managedSessions(engine, executor)
	if err != nil {
		executor.NotifyNotice(fmt.Sprintf("Clone-cache orphan cleanup was skipped: %s", alternate(err)))
		liveSessions = []string{sessionID}
	}
	if !slices.Contains(liveSessions, sessionID) {
		liveSessions = append(liveSessions, sessionID)
	}
	if err := collectGarbage(host, cacheRoot, liveSessions, executor); err != nil {
		executor.NotifyNotice(fmt.Sprintf("Clone-cache cleanup was skipped: %s", alternate(err)))
	}
	return &PreparedCloneCache{host: host, sessionRoot: sessionRoot}, nil
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
	command := host.gitShellCommand(`command -v git >/dev/null 2>&1 || exit 127; printf '%s' "$HOME"`, nil,
		"locate the container host Git cache")
	output, err := executor.Execute(&command)
	if err != nil {
		return "", err
	}
	output, err = checked(output, &command)
	if err != nil {
		return "", err
	}
	home, err := fromUTF8(output.Stdout)
	if err != nil {
		return "", fmt.Errorf("decode container host home directory: %w", err)
	}
	if !core.IsAbsoluteOnHostOrTarget(home) {
		return "", errors.New("container host HOME is not absolute")
	}
	return home, nil
}

func repositoryCacheKey(source string) string {
	var identity string
	if repository := core.ParseGithubRemote(source, core.ShorthandAllowed); repository != nil {
		identity = "github.com/" + asciiLower(repository.Owner) + "/" + asciiLower(repository.Repository)
	} else {
		// Resolution has already expanded shorthands and Git's configured URL
		// rewrites. Preserve user, port, and the entire transport path here:
		// two accounts or paths on one server can serve different objects.
		// RepositoryIdentity::Network drops userinfo and the .git suffix.
		identity = "git:" + source
	}
	digest := sha256.Sum256([]byte(identity))
	return core.LowerHex(digest[:])
}

// asciiLower is Rust's `str::to_ascii_lowercase`.
func asciiLower(text string) string {
	return strings.Map(func(r rune) rune {
		if 'A' <= r && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, text)
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
	exactRef := ""
	if exact != nil {
		if !core.IsObjectID(*exact) {
			return errors.New("invalid exact checkout commit")
		}
		exactRef = core.HostSnapshotExactRefs + "/" + *exact
	}
	var token *string
	if githubToken != nil && core.IsGithubHTTPSURL(source) {
		token = githubToken
	}
	flag := func(value bool) string {
		if value {
			return "1"
		}
		return "0"
	}
	var sshCommand string
	if host.IsLocal() {
		sshCommand, _ = core.LocalGitSSHCommand()
	} else {
		var err error
		if sshCommand, err = core.RemoteGitSSHCommand(); err != nil {
			return err
		}
	}
	sourceID := sha256.Sum256([]byte(source))
	arguments := []string{
		cacheRoot,
		key,
		source,
		snapshot,
		flag(token != nil),
		sshCommand,
		core.LowerHex(sourceID[:]),
		flag(core.IsHostOnlyGitSource(source)),
		exactRef,
	}
	if cutoff != nil {
		if err := cutoff.Validate(); err != nil {
			return err
		}
		arguments = append(arguments, cutoff.Commits()...)
	}
	command := host.gitShellCommand(prepareRepositoryScript, arguments,
		fmt.Sprintf("prepare cached Git objects for %s", core.DisplayURL(source)))
	if token != nil {
		command = command.WithSensitiveStdin(append([]byte(*token), '\n'))
	}
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	if _, err := checked(output, &command); err != nil {
		return errors.New(strings.ReplaceAll(err.Error(), source, core.DisplayURL(source)))
	}
	return nil
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
