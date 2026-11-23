// Port of mj-build-cache/src/mbx.rs.
//
// The mbx build cache for Rust container sessions.
//
// mbx wraps Cargo: a binary named `cargo` that is really `mbx` intercepts the
// build, looks every compiler action up in a content-addressed store, and
// restores cached outputs instead of recompiling. Podman and Docker mount
// the host store read-write at the same path in each container. Nitro guests
// keep their store on the guest filesystem. Mjolnir tells host mbx when a
// workspace it removed will never build again (see mbx_release.go).
//
// Cache discovery can leave new sessions uncached. Once a cache is selected,
// configuration failures are reported rather than launching with stale policy.

package buildcache

import (
	"encoding/json"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MbxVersion is the mbx release Mjolnir installs (Rust re-exports
// mj_core::config::MBX_VERSION here).
const MbxVersion = core.MbxVersion

const nitroCacheDirectory = "/var/lib/hel/mbx"

// cachePlacement is where a session keeps its build cache (Rust enum
// CachePlacement): shared from a host (Shared), or on a Nitro guest's own
// filesystem (NitroGuest) when shared is nil.
type cachePlacement struct {
	shared *CacheHost
}

const mbxCopyRelative = ".mjolnir/bin/mbx"

// tallyRelative is mbx's running totals, relative to the cache directory.
const tallyRelative = "actions/savings/v1/tally.json"

const resolutionLifetime = 600 * time.Second

const label = "hel-mbx"

const windowsHost = "Mjolnir's shared mbx cache requires a Linux host."

// platformLimit is why a host's platform keeps its mbx away from its
// containers, and the native mbx it found there.
type platformLimit struct {
	native *NativeMbx
	reason string
}

// platformLimitOf is the one decision on whether a host's platform can share
// its mbx with containers, or nil on Linux, where it can (Rust fn
// platform_limit). A Mac runs mbx natively, so its own mbx is still found and
// reported: bare sessions there use it.
func platformLimitOf(host CacheHost, executor core.CommandExecutor) (*platformLimit, error) {
	panic("unported: mj-build-cache/src/mbx.rs platform_limit")
}

// ResolvedBuildCache is what a container target's host offers as a build
// cache.
type ResolvedBuildCache struct {
	// Directory is the cache directory on the host, mounted at the same path
	// in the container.
	Directory string
	// NativeMbx is the compatible native mbx executable used to refresh the
	// cache copy.
	NativeMbx NativeMbx
	// TargetRoot is a `[target] root` the host configuration relocates
	// outside the cache directory, which the container needs mounted at the
	// same path too.
	TargetRoot *string
	// ConfigFile is the desired policy for the shared machine file, never a
	// private session copy.
	ConfigFile      *string
	ConfigDirectory string
	PreviousConfig  *string
}

// resolution is one remembered host inspection: when it ran, and the
// inspection or its failure text (Rust `(Instant, Result<Inspection,
// String>)`).
type resolution struct {
	recorded   time.Time
	inspection inspection
	failure    *string
}

// resolutions caches host inspections, keyed by host and per-target
// settings. An inspection runs several commands on the host, and a burst of
// new sessions must not repeat them for each one. A failure is remembered
// too, so a host that cannot answer is not re-probed by every session in that
// burst.
var resolutions = struct {
	sync.Mutex
	byKey map[string]resolution
}{byKey: map[string]resolution{}}

// applications is the outcome of the last application per resolution key: a
// nil error for success, the failure otherwise.
var applications = struct {
	sync.Mutex
	byKey map[string]error
}{byKey: map[string]error{}}

// mbxSyncLocks serializes cache copy synchronization per host and copy path.
var mbxSyncLocks = struct {
	sync.Mutex
	byKey map[string]*sync.Mutex
}{byKey: map[string]*sync.Mutex{}}

// freshness is whether a caller can be served a memoized answer or needs the
// host asked again.
type freshness int

const (
	// freshnessMemoized: provisioning; an answer from the last
	// resolutionLifetime will do.
	freshnessMemoized freshness = iota
	// freshnessMemoizedSuccess: configuration a session already depends on;
	// a recent successful answer will do, but a remembered failure —
	// including a host inspection whose cache was Unavailable — is asked
	// again. Another caller's failure (such as the background policy service
	// timing out on a busy link, or a transient reflink outage) must not fail
	// a launch that can now use the host.
	freshnessMemoizedSuccess
	// freshnessFresh: the settings screen, which is read precisely when
	// somebody has just changed something on the host. A fresh answer also
	// replaces the memoized one, so the next session sees the same thing the
	// screen does.
	freshnessFresh
)

// resolutionKey keys resolutions and applications by host and settings
// (Rust `format!("{}|{settings:?}", host.key())`). The settings' JSON stands
// in for their Debug text: it distinguishes the same values.
func resolutionKey(host CacheHost, settings *core.TargetBuildCache) string {
	text, err := json.Marshal(settings)
	if err != nil {
		panic(fmt.Sprintf("encode build cache settings: %v", err))
	}
	return host.Key() + "|" + string(text)
}

// inspect is the one place a host is inspected. Sessions and the settings
// screen differ only in the freshness they ask for, so they cannot drift into
// reporting different things about the same host.
func inspect(host CacheHost, settings *core.TargetBuildCache, freshness freshness, executor core.CommandExecutor) (inspection, error) {
	panic("unported: mj-build-cache/src/mbx.rs inspect")
}

// Resolve resolves a target's shared host cache. Nitro guest caches are
// prepared without a host lookup.
func Resolve(target *core.TargetsTargetTemplate, executor core.CommandExecutor) *ResolvedBuildCache {
	placement, settings, ok := supportedHost(target)
	if !ok || placement.shared == nil {
		return nil
	}
	return resolveShared(*placement.shared, &settings, executor)
}

func resolveShared(host CacheHost, settings *core.TargetBuildCache, executor core.CommandExecutor) *ResolvedBuildCache {
	panic("unported: mj-build-cache/src/mbx.rs resolve_shared")
}

func applyCache(host CacheHost, settings *core.TargetBuildCache, cache ResolvedBuildCache, executor core.CommandExecutor) (ResolvedBuildCache, error) {
	panic("unported: mj-build-cache/src/mbx.rs apply_cache")
}

// supportedHost is where a session can keep its build cache, with the
// target's build cache settings, or false for a target that cannot keep one.
// Apple container's host mount lacks verified shared locks. Nitro keeps its
// store on the guest filesystem.
func supportedHost(target *core.TargetsTargetTemplate) (cachePlacement, core.TargetBuildCache, bool) {
	panic("unported: mj-build-cache/src/mbx.rs supported_host")
}

// PreviewBuildCache is what the settings screen shows for one machine's
// blank build cache fields: the same host inspection a session runs, without
// creating the directory. nil when the machine has no standing host to share
// a cache on.
func PreviewBuildCache(machine core.Machine, executor core.CommandExecutor) (*core.BuildCachePreview, error) {
	panic("unported: mj-build-cache/src/mbx.rs preview_build_cache")
}

// MachineCacheHostKey is the CacheHost.Key of a machine's host, or false for
// a machine without a standing host.
func MachineCacheHostKey(machine core.Machine) (string, bool) {
	host, ok := CacheHostForMachine(machine)
	if !ok {
		return "", false
	}
	return host.Key(), true
}

// ApplyMachineBuildCache applies one machine's desired policy. Both
// provisioning and the daemon use the same compare-and-replace operation;
// native settings are only read.
func ApplyMachineBuildCache(machine core.Machine, mountedDirectories []string, executor core.CommandExecutor) error {
	panic("unported: mj-build-cache/src/mbx.rs apply_machine_build_cache")
}

func publishAt(host CacheHost, cache *ResolvedBuildCache, directory string, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx.rs publish_at")
}

// PrepareSessionConfiguration upgrades an existing container through its
// existing cache mount. Resolving ownership uses its actual host, never a
// target name that can be reassigned.
func PrepareSessionConfiguration(config *core.Config, backend *core.TargetsTargetLocator, recorded *core.SessionBuildCache, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx.rs prepare_session_configuration")
}

// DoctorHostMbx is native mbx compatibility for a host used by configured
// container targets.
type DoctorHostMbx struct {
	Host    string
	Targets []string
	Status  DoctorHostMbxStatus
}

// DoctorHostMbxStatus is one host's native mbx as `mj doctor` reports it: a
// DoctorHostMbxStatusUnsupported, DoctorHostMbxStatusAbsent,
// DoctorHostMbxStatusCompatible or DoctorHostMbxStatusUnknown.
type DoctorHostMbxStatus interface {
	isDoctorHostMbxStatus()
}

// DoctorHostMbxStatusUnsupported carries why the host's platform keeps its
// mbx away from containers.
type DoctorHostMbxStatusUnsupported struct{ Value string }

type DoctorHostMbxStatusAbsent struct{}

// DoctorHostMbxStatusCompatible carries the native mbx version.
type DoctorHostMbxStatusCompatible struct{ Value string }

// DoctorHostMbxStatusUnknown carries why the check could not decide.
type DoctorHostMbxStatusUnknown struct{ Value string }

func (DoctorHostMbxStatusUnsupported) isDoctorHostMbxStatus() {}
func (DoctorHostMbxStatusAbsent) isDoctorHostMbxStatus()      {}
func (DoctorHostMbxStatusCompatible) isDoctorHostMbxStatus()  {}
func (DoctorHostMbxStatusUnknown) isDoctorHostMbxStatus()     {}

// nativeMbxStatus is one classification of a host probe, shared by doctor
// and session preview: nativeMbxAbsent, nativeMbxCompatible or
// nativeMbxUnknown.
type nativeMbxStatus interface {
	isNativeMbxStatus()
}

type nativeMbxAbsent struct{}

type nativeMbxCompatible struct{ native NativeMbx }

type nativeMbxUnknown struct {
	version string
	reason  string
}

func (nativeMbxAbsent) isNativeMbxStatus()     {}
func (nativeMbxCompatible) isNativeMbxStatus() {}
func (nativeMbxUnknown) isNativeMbxStatus()    {}

func classifyNativeMbx(native *NativeMbx) nativeMbxStatus {
	panic("unported: mj-build-cache/src/mbx.rs classify_native_mbx")
}

func cacheUnavailableReason(status nativeMbxStatus) string {
	switch status := status.(type) {
	case nativeMbxAbsent:
		return "mbx is not installed on the container host. Install mbx from Settings › Setup › Machines to enable the shared build cache."
	case nativeMbxUnknown:
		return status.reason + "; install or upgrade mbx from Settings › Setup › Machines to enable the shared build cache"
	}
	panic("compatible mbx enables the cache")
}

// DoctorHostMbxChecks checks each relevant host once (Rust fn
// doctor_host_mbx; the Go name leaves DoctorHostMbx to the type). The cache
// is optional, so disabled caches and hosts with no Podman or Docker target
// need no compatibility check.
func DoctorHostMbxChecks(config *core.Config, executor core.CommandExecutor) []DoctorHostMbx {
	panic("unported: mj-build-cache/src/mbx.rs doctor_host_mbx")
}

// inspection is everything the host says about a target's build cache, read
// without changing the host.
type inspection struct {
	preview core.BuildCachePreview
	// cache is the cache a session would mount, or nil when it runs without
	// one.
	cache *ResolvedBuildCache
}

func inspectHost(host CacheHost, settings *core.TargetBuildCache, executor core.CommandExecutor) (inspection, error) {
	panic("unported: mj-build-cache/src/mbx.rs inspect_host")
}

// readStats is mbx's running totals for this cache, or nil when it has none
// yet.
//
// Read from the tally file rather than by running `mbx stats`, which also
// walks the content-addressed store to size it: that took 90 seconds on a
// 540 GB cache here, where the tally is a few hundred bytes. It also means
// the numbers need no mbx binary on the host.
//
// A cache that has never been used has no tally, which is not a failure.
//
// A newer mbx may add counters; unknown ones are ignored rather than costing
// the whole report, exactly as mbx reads the file itself.
func readStats(host CacheHost, directory string, executor core.CommandExecutor) *core.BuildCacheStats {
	panic("unported: mj-build-cache/src/mbx.rs read_stats")
}

// tally is mbx's tally file as readStats reads it (a type local to Rust's
// read_stats, `#[serde(default)]`): a missing counter is zero and unknown
// counters are ignored.
type tally struct {
	Builds             uint64 `json:"builds"`
	CachedCompilations uint64 `json:"cached_compilations"`
	AvoidedCompilerNs  uint64 `json:"avoided_compiler_ns"`
	ReflinkedBytes     uint64 `json:"reflinked_bytes"`
}

// NativeMbx is the host's own mbx: its absolute resolved path and version.
type NativeMbx struct {
	Program string
	Version string
}

// NativeVersionScript finds the same native executable whether it is on PATH
// or in a common per-user install directory, then resolves symlinks before
// reporting it. Links are followed one at a time because macOS before 12.3
// has no `readlink -f`; every path handed to `readlink` starts with `/`.
const NativeVersionScript = `for candidate in "$(command -v mbx 2>/dev/null || true)" "$HOME/.local/bin/mbx" "$HOME/.cargo/bin/mbx"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] || continue
    case "$candidate" in /*) resolved=$candidate ;; *) resolved=$PWD/$candidate ;; esac
    hops=0
    while [ -L "$resolved" ]; do
        [ "$hops" -lt 40 ] || continue 2
        link=$(readlink "$resolved" 2>/dev/null) || continue 2
        case "$link" in /*) resolved=$link ;; *) resolved=${resolved%/*}/$link ;; esac
        hops=$((hops + 1))
    done
    directory=$(cd -P "${resolved%/*}/" 2>/dev/null && pwd -P) || continue
    resolved=${directory%/}/${resolved##*/}
    case "$resolved" in /*) ;; *) continue ;; esac
    if version=$("$resolved" --version 2>/dev/null); then
        printf '%s
%s' "$resolved" "$version"
        exit 0
    fi
done
exit 1`

// ProbeNativeVersion is the host's own mbx, or nil when no supported
// candidate can run.
func ProbeNativeVersion(host CacheHost, executor core.CommandExecutor) (*NativeMbx, error) {
	panic("unported: mj-build-cache/src/mbx.rs probe_native_version")
}

func parseNativeProbeOutput(stdout []byte) (NativeMbx, error) {
	panic("unported: mj-build-cache/src/mbx.rs parse_native_probe_output")
}

// CachedMbxBinary is the current native mbx copy visible through the shared
// cache mount.
type CachedMbxBinary struct {
	Path    string
	Version string
}

// CachedMbxSync is the outcome of refreshing a cache's mbx copy: a
// CachedMbxSyncReady or a CachedMbxSyncUnavailable.
type CachedMbxSync interface {
	isCachedMbxSync()
}

type CachedMbxSyncReady struct{ Value CachedMbxBinary }

// CachedMbxSyncUnavailable carries why the host has no compatible mbx.
type CachedMbxSyncUnavailable struct{ Value string }

func (CachedMbxSyncReady) isCachedMbxSync()       {}
func (CachedMbxSyncUnavailable) isCachedMbxSync() {}

const syncMbxBinaryScript = `set -eu
source=$1 destination=$2 expected=$3 probe_script=$4
directory=$(dirname -- "$destination")
mkdir -p -- "$directory"
if command -v flock >/dev/null 2>&1; then
    exec 9>"$directory/.mbx.lock"
    flock -x 9
fi
probe_matches() {
    current=$(sh -c "$probe_script" 2>/dev/null) || return 1
    newline='
'
    current_program=${current%%"$newline"*}
    current_version=${current##* }
    [ "$current_program" = "$source" ] && [ "$current_version" = "$expected" ]
}
if ! probe_matches; then
    echo "native mbx changed while cache synchronization was waiting" >&2
    exit 75
fi
source_output=$("$source" --version 2>/dev/null) || {
    echo "native mbx stopped working during cache synchronization" >&2
    exit 2
}
source_version=${source_output##* }
[ "$source_version" = "$expected" ] || {
    echo "native mbx changed version during cache synchronization" >&2
    exit 75
}
source_size=$(wc -c < "$source")
if [ -f "$destination" ] && [ ! -L "$destination" ] && [ -x "$destination" ]; then
    installed_output=$("$destination" --version 2>/dev/null) || installed_output=
    installed_version=${installed_output##* }
    installed_size=$(wc -c < "$destination")
    if [ "$installed_version" = "$expected" ] && [ "$source_size" -eq "$installed_size" ]; then
        if ! probe_matches; then
            echo "native mbx changed during cache synchronization" >&2
            exit 75
        fi
        printf 'unchanged\n'
        exit 0
    fi
fi
temporary=$(mktemp "${destination}.mjolnir.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
cp -- "$source" "$temporary"
chmod 755 -- "$temporary"
temporary_output=$("$temporary" --version 2>/dev/null) || {
    echo "copied mbx cannot run on the container host" >&2
    exit 2
}
temporary_version=${temporary_output##* }
temporary_size=$(wc -c < "$temporary")
[ "$temporary_version" = "$expected" ] && [ "$source_size" -eq "$temporary_size" ] || {
    echo "copied mbx changed during cache synchronization" >&2
    exit 2
}
mv -f -- "$temporary" "$destination"
trap - EXIT HUP INT TERM
if ! probe_matches; then
    echo "native mbx changed during cache synchronization" >&2
    exit 75
fi
printf 'synced\n'`

// CacheBinaryPath is where a cache directory holds its copy of the native
// mbx.
func CacheBinaryPath(directory string) string {
	return pathJoin(directory, mbxCopyRelative)
}

// SyncCurrentMbxBinary copies the current compatible host executable into a
// cache directory that is already mounted read-write in its containers. A
// missing or old native mbx leaves any previous copy untouched.
func SyncCurrentMbxBinary(host CacheHost, directory string, executor core.CommandExecutor) (CachedMbxSync, error) {
	native, err := ProbeNativeVersion(host, executor)
	if err != nil {
		return nil, err
	}
	switch status := classifyNativeMbx(native).(type) {
	case nativeMbxCompatible:
		binary, err := SyncMbxBinaryFromNative(host, &status.native, directory, executor)
		if err != nil {
			return nil, err
		}
		return CachedMbxSyncReady{Value: binary}, nil
	default:
		return CachedMbxSyncUnavailable{Value: cacheUnavailableReason(status)}, nil
	}
}

// SyncMbxBinaryFromNative atomically refreshes one cache copy from a version
// that has already been classified as compatible. Version and size avoid
// copying an unchanged multi-megabyte executable on every resume or
// reconciliation tick.
func SyncMbxBinaryFromNative(host CacheHost, native *NativeMbx, directory string, executor core.CommandExecutor) (CachedMbxBinary, error) {
	panic("unported: mj-build-cache/src/mbx.rs sync_mbx_binary_from_native")
}

// AvailableBytes is the available column of a `df -P` report (the third
// field of its data row). Callers decide the block size used by the report
// they requested.
func AvailableBytes(report string) (uint64, bool) {
	panic("unported: mj-build-cache/src/mbx.rs available_bytes")
}

// nativeCacheDirectory is the host's own cache directory. `mbx cache dir`
// prints the store, which is the `actions` directory inside the cache
// directory.
func nativeCacheDirectory(host CacheHost, native *NativeMbx, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx.rs native_cache_directory")
}

const readConfigScript = `[ -f "$1" ] || exit 3
cat -- "$1"`

// hostConfigFile is the host's `~/.config/mbx/config.toml`, which containers
// receive verbatim so their mbx uses the host's own limits. mbx has no
// command that prints its effective configuration, so the file itself is the
// only accurate source.
func hostConfigFile(host CacheHost, executor core.CommandExecutor) (*string, error) {
	directory, err := configurationHostDirectory(host, executor)
	if err != nil {
		return nil, err
	}
	return configurationReadFile(host, pathJoin(directory, "config.toml"), executor)
}

// LocalStoragePaths is the local directories mbx uses for its cache and
// managed targets.
func LocalStoragePaths() []string {
	panic("unported: mj-build-cache/src/mbx.rs local_storage_paths")
}

func resolveLocalStoragePaths(cacheOverride, targetRootOverride, configFile, defaultCache *string) []string {
	panic("unported: mj-build-cache/src/mbx.rs resolve_local_storage_paths")
}

// relocatedTargetRoot is the `[target] root` a host configuration sets, when
// it lies outside the cache directory and therefore needs its own mount.
func relocatedTargetRoot(configFile string, directory string) *string {
	panic("unported: mj-build-cache/src/mbx.rs relocated_target_root")
}

const nearestAncestorScript = `d=$1
while [ ! -d "$d" ]; do
    parent=$(dirname -- "$d")
    if [ "$parent" = "$d" ]; then
        break
    fi
    d=$parent
done
printf '%s' "$d"`

// nearestExistingAncestor is the deepest existing directory at or above
// directory. The cache directory may not exist yet, and both `df` and the
// reflink probe need a real one.
func nearestExistingAncestor(host CacheHost, directory string, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx.rs nearest_existing_ancestor")
}

const reflinkScript = `dir=$1
d=$(mktemp -d "$dir/.mj-reflink.XXXXXX") || exit 1
printf x > "$d/a" && cp --reflink=always "$d/a" "$d/b"
status=$?
rm -rf -- "$d"
exit $status`

// reflinksSupported is whether the cache volume can clone files instead of
// copying their bytes. Reflinks are what make restoring a cached output
// nearly free, so a host without them defaults to running without the cache.
func reflinksSupported(host CacheHost, volume string, executor core.CommandExecutor) (bool, error) {
	command := host.ShellCommand(reflinkScript, label, []string{volume}, "probe the build cache volume for reflinks")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	return output.Status == 0, nil
}

const crossReflinkScript = `src=$1
dst=$2
s=$(mktemp -d "$src/.mj-reflink.XXXXXX") || exit 1
d=$(mktemp -d "$dst/.mj-reflink.XXXXXX") || { rm -rf -- "$s"; exit 1; }
printf x > "$s/a" && cp --reflink=always "$s/a" "$d/b"
status=$?
rm -rf -- "$s" "$d"
exit $status`

// crossReflinksSupported is whether a cached output can be cloned from the
// store into the managed target root instead of copied. `FICLONE` fails
// across two mounts even when both are the same filesystem, so this asks the
// pair rather than each side.
func crossReflinksSupported(host CacheHost, storeDirectory, targetRoot string, executor core.CommandExecutor) (bool, error) {
	command := host.ShellCommand(crossReflinkScript, label, []string{storeDirectory, targetRoot},
		"probe the managed target root for reflinks from the build cache")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	return output.Status == 0, nil
}

func createDirectory(host CacheHost, directory string, executor core.CommandExecutor) error {
	command := host.Command([]string{"mkdir", "-p", "--", directory}, "create the build cache directory")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	_, err = mbxChecked(output, &command)
	return err
}

// unusableFilesystem is a filesystem mbx cannot use, or false. It refuses NFS
// outright, and file locks over FUSE, virtiofs, and 9p are unreliable, which
// a shared store depends on.
func unusableFilesystem(host CacheHost, directory string, executor core.CommandExecutor) (string, bool, error) {
	panic("unported: mj-build-cache/src/mbx.rs unusable_filesystem")
}

// mbxChecked is mbx.rs's `checked`: output when the command succeeded, its
// failure otherwise.
func mbxChecked(output core.CommandOutput, command *core.CommandSpec) (core.CommandOutput, error) {
	if output.Status == 0 {
		return output, nil
	}
	return core.CommandOutput{}, fmt.Errorf("%s failed with status %d: %s",
		command.Purpose, output.Status, trimmedLossy(output.Stderr))
}

// -- per-session decision -------------------------------------------------

// MbxPrepare decides the cache placement to record on the session and
// attaches any shared mounts (Rust mbx::prepare). Resumes and moves resolve
// current policy.
func MbxPrepare(target *core.TargetsTargetTemplate, session *core.SessionRecord, mounts *[]core.AdditionalMount, executor core.CommandExecutor) *core.SessionBuildCache {
	panic("unported: mj-build-cache/src/mbx.rs prepare")
}

// attachMounts mounts the cache, and a relocated target root, read-write at
// the same absolute paths the host uses. An attached directory that already
// covers one of those paths wins, and the session runs without the cache.
func attachMounts(buildCache *core.SessionBuildCache, mounts *[]core.AdditionalMount) bool {
	panic("unported: mj-build-cache/src/mbx.rs attach_mounts")
}

// HostForLocator reads the configuration on the host that actually owns
// this container. The named template may have been removed or reassigned
// since creation.
func HostForLocator(target *core.TargetsTargetLocator) (CacheHost, bool) {
	switch target.Kind {
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorLocalDocker:
		return CacheHost{}, true
	case core.TargetsTargetLocatorSshPodman, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer:
		ssh := target.SSH.Clone()
		return CacheHost{SSH: &ssh}, true
	}
	return CacheHost{}, false
}

// SyncMbxBinaryForContainer refreshes a new-scheme session's copy from the
// host that owns its container.
func SyncMbxBinaryForContainer(target *core.TargetsTargetLocator, directory string, executor core.CommandExecutor) (CachedMbxSync, error) {
	host, ok := HostForLocator(target)
	if !ok {
		return nil, errors.New("build cache has no container host")
	}
	return SyncCurrentMbxBinary(host, directory, executor)
}

// trimmedLossy is Rust's `String::from_utf8_lossy(bytes).trim()`.
func trimmedLossy(bytes []byte) string {
	panic("unported: mj-build-cache std helper trimmedLossy")
}
