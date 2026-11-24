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
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BurntSushi/toml"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	platform, err := host.Platform(executor)
	if err != nil {
		return nil, err
	}
	switch platform {
	case HostPlatformLinux:
		return nil, nil
	case HostPlatformWindows:
		return &platformLimit{reason: windowsHost}, nil
	}
	native, err := ProbeNativeVersion(host, executor)
	if err != nil {
		return nil, err
	}
	const containers = "Container sessions run without the shared build cache, because " +
		"containers on macOS run Linux in a virtual machine that cannot run a macOS mbx or share " +
		"its cache."
	// The probe also searches user directories a login PATH may omit, so it
	// cannot say that bare sessions find what it found.
	var reason string
	if native != nil {
		reason = fmt.Sprintf("mbx %s runs natively on this Mac at %s; bare sessions here use it when it is on "+
			"their login PATH. %s", native.Version, native.Program, containers)
	} else {
		reason = "mbx is not installed on this Mac; bare sessions here use it once it is installed " +
			"natively and on their login PATH. " + containers
	}
	return &platformLimit{native: native, reason: reason}, nil
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
	key := resolutionKey(host, settings)
	if freshness != freshnessFresh {
		resolutions.Lock()
		recorded, ok := resolutions.byKey[key]
		resolutions.Unlock()
		if ok && time.Since(recorded.recorded) < resolutionLifetime &&
			(freshness == freshnessMemoized || recorded.failure == nil && !inspectionUnavailable(recorded.inspection)) {
			if recorded.failure != nil {
				return inspection{}, errors.New(*recorded.failure)
			}
			return recorded.inspection, nil
		}
	}
	inspected, err := inspectHost(host, settings, executor)
	recorded := resolution{recorded: time.Now(), inspection: inspected}
	if err != nil {
		failure := alternate(err)
		recorded = resolution{recorded: recorded.recorded, failure: &failure}
	}
	resolutions.Lock()
	resolutions.byKey[key] = recorded
	resolutions.Unlock()
	return inspected, err
}

// inspectionUnavailable is a host inspection whose cache was Unavailable,
// which a MemoizedSuccess caller asks again.
func inspectionUnavailable(inspected inspection) bool {
	_, unavailable := inspected.preview.OffReason.(core.BuildCacheOffUnavailable)
	return inspected.cache == nil && unavailable
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
	inspected, err := inspect(host, settings, freshnessMemoized, executor)
	if err != nil {
		slog.Warn(fmt.Sprintf("build cache unavailable: %s", alternate(err)), "host", host.Key())
		return nil
	}
	if inspected.cache == nil {
		if reason := inspected.preview.OffReason; reason != nil {
			var directory any
			if inspected.preview.Directory != nil {
				directory = *inspected.preview.Directory
			}
			slog.Warn(fmt.Sprintf("sessions on this target run without the build cache: %s", reason), "directory", directory)
		}
		return nil
	}
	cache := *inspected.cache
	// Creating the directory is the one side effect a session has and the
	// settings screen does not, so it sits here rather than inside the shared
	// inspection. It runs per session because a memoized inspection says what
	// the host looked like, not that the directory still exists.
	if err := createDirectory(host, cache.Directory, executor); err != nil {
		slog.Warn(fmt.Sprintf("the build cache directory could not be created: %s", alternate(err)), "directory", cache.Directory)
		return nil
	}
	applied, err := applyCache(host, settings, cache, executor)
	if err != nil {
		slog.Warn(fmt.Sprintf("applying machine build cache configuration failed: %s", alternate(err)), "host", host.Key())
		executor.NotifyNotice(fmt.Sprintf("Build cache configuration could not be applied: %s", alternate(err)))
		return nil
	}
	return &applied
}

func applyCache(host CacheHost, settings *core.TargetBuildCache, cache ResolvedBuildCache, executor core.CommandExecutor) (ResolvedBuildCache, error) {
	key := resolutionKey(host, settings)
	result, err := func() (ResolvedBuildCache, error) {
		applied, err := configurationApply(host, &cache, executor)
		if err != nil {
			return ResolvedBuildCache{}, err
		}
		if !applied {
			// Another application won. Accept it only if it already installs
			// this policy; never replay an older desired value over a newer one.
			inspected, err := inspect(host, settings, freshnessFresh, executor)
			if err != nil {
				return ResolvedBuildCache{}, err
			}
			if inspected.cache == nil {
				return ResolvedBuildCache{}, errors.New("build cache became unavailable during application")
			}
			cache = *inspected.cache
			if !optionalStringEqual(cache.PreviousConfig, cache.ConfigFile) {
				return ResolvedBuildCache{}, errors.New("machine build cache policy changed during application; retry with current machine settings")
			}
		}
		cache.PreviousConfig = cache.ConfigFile
		resolutions.Lock()
		if recorded, ok := resolutions.byKey[key]; ok && recorded.failure == nil {
			applied := cache
			recorded.inspection.cache = &applied
			recorded.inspection.preview.Application = core.BuildCacheApplicationApplied{}
			resolutions.byKey[key] = recorded
		}
		resolutions.Unlock()
		return cache, nil
	}()
	applications.Lock()
	applications.byKey[key] = err
	applications.Unlock()
	return result, err
}

// supportedHost is where a session can keep its build cache, with the
// target's build cache settings, or false for a target that cannot keep one.
// Apple container's host mount lacks verified shared locks. Nitro keeps its
// store on the guest filesystem.
func supportedHost(target *core.TargetsTargetTemplate) (cachePlacement, core.TargetBuildCache, bool) {
	var placement cachePlacement
	switch target.Kind {
	case core.TargetsTargetTemplateLocalPodman, core.TargetsTargetTemplateLocalDocker,
		core.TargetsTargetTemplateSshPodman, core.TargetsTargetTemplateSshDocker:
		host, ok := CacheHostForTarget(target)
		if !ok {
			return cachePlacement{}, core.TargetBuildCache{}, false
		}
		placement = cachePlacement{shared: &host}
	case core.TargetsTargetTemplateSshNitro:
		placement = cachePlacement{}
	default:
		return cachePlacement{}, core.TargetBuildCache{}, false
	}
	var settings core.TargetBuildCache
	if target.Container != nil && target.Container.BuildCache != nil {
		settings = *target.Container.BuildCache
	}
	return placement, settings, true
}

// machineSettings is a machine's build cache settings, or the defaults.
func machineSettings(machine core.Machine) core.TargetBuildCache {
	if settings := machine.BuildCache(); settings != nil {
		return *settings
	}
	return core.TargetBuildCache{}
}

// PreviewBuildCache is what the settings screen shows for one machine's
// blank build cache fields: the same host inspection a session runs, without
// creating the directory. nil when the machine has no standing host to share
// a cache on.
func PreviewBuildCache(machine core.Machine, executor core.CommandExecutor) (*core.BuildCachePreview, error) {
	host, ok := CacheHostForMachine(machine)
	if !ok {
		return nil, nil
	}
	settings := machineSettings(machine)
	inspected, err := inspect(host, &settings, freshnessFresh, executor)
	if err != nil {
		return nil, err
	}
	preview := inspected.preview
	if _, ok := InstallCandidateKind(&preview); ok {
		profile, err := LoginProfileDetails(host, executor)
		if err != nil {
			return nil, err
		}
		if profile != nil {
			file := profile.File
			preview.MbxProfileFile = &file
			preview.MbxProfileWarning = profile.Warning
			preview.MbxManualPathLine = profile.ManualPathLine
		}
	}
	return &preview, nil
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
	host, ok := CacheHostForMachine(machine)
	if !ok {
		return nil
	}
	settings := machineSettings(machine)
	inspected, err := inspect(host, &settings, freshnessFresh, executor)
	if err != nil {
		return err
	}
	if _, unsupported := inspected.preview.OffReason.(core.BuildCacheOffUnsupportedPlatform); unsupported {
		return nil
	}
	var cache *ResolvedBuildCache
	if inspected.cache != nil {
		applied, err := applyCache(host, &settings, *inspected.cache, executor)
		if err != nil {
			return err
		}
		cache = &applied
	}

	var native *NativeMbx
	if cache != nil {
		copied := cache.NativeMbx
		native = &copied
	} else {
		probed, err := ProbeNativeVersion(host, executor)
		if err != nil {
			return err
		}
		if compatible, ok := classifyNativeMbx(probed).(nativeMbxCompatible); ok {
			native = &compatible.native
		}
	}
	if native == nil {
		// Existing copies remain usable by their mounted containers until the
		// host has a compatible native mbx again.
		return nil
	}

	directories := append([]string{}, mountedDirectories...)
	if cache != nil && !slices.ContainsFunc(directories, func(directory string) bool { return pathEqual(directory, cache.Directory) }) {
		directories = append(directories, cache.Directory)
	}
	for _, directory := range directories {
		// Existing containers retain their mounts if placement changes. Apply
		// policy there as before, then refresh every copy those mounts expose.
		if cache != nil && !pathEqual(directory, cache.Directory) {
			if _, err := publishAt(host, cache, directory, executor); err != nil {
				return err
			}
		}
		if _, err := SyncMbxBinaryFromNative(host, native, directory, executor); err != nil {
			return err
		}
	}
	return nil
}

func publishAt(host CacheHost, cache *ResolvedBuildCache, directory string, executor core.CommandExecutor) (string, error) {
	projected := *cache
	projected.ConfigDirectory = configurationSharedDirectory(directory)
	previous, err := configurationReadFile(host, pathJoin(projected.ConfigDirectory, "config.toml"), executor)
	if err != nil {
		return "", err
	}
	projected.PreviousConfig = previous
	applied, err := configurationApply(host, &projected, executor)
	if err != nil {
		return "", err
	}
	if !applied {
		return "", errors.New("machine configuration changed during application; retry with current settings")
	}
	return projected.ConfigDirectory, nil
}

// PrepareSessionConfiguration upgrades an existing container through its
// existing cache mount. Resolving ownership uses its actual host, never a
// target name that can be reassigned.
func PrepareSessionConfiguration(config *core.Config, backend *core.TargetsTargetLocator, recorded *core.SessionBuildCache, executor core.CommandExecutor) (string, error) {
	host, ok := HostForLocator(backend)
	if !ok {
		return "", errors.New("build cache has no container host")
	}
	var settings core.TargetBuildCache
	for _, id := range slices.Sorted(maps.Keys(config.Machines)) {
		machine := config.Machines[id]
		candidate, ok := CacheHostForMachine(machine)
		if !ok || candidate.Key() != host.Key() {
			continue
		}
		if configured := machine.BuildCache(); configured != nil {
			settings = *configured
			break
		}
	}
	// Reuse the launch's original resolution key first. Retained containers
	// can still mount an older directory, or a cache disabled since launch;
	// only those need a second resolution projected onto the recorded mount.
	inspected, err := inspect(host, &settings, freshnessMemoizedSuccess, executor)
	if err != nil {
		return "", err
	}
	_, turnedOff := inspected.preview.OffReason.(core.BuildCacheOffTurnedOff)
	needsProjection := inspected.cache != nil && !pathEqual(inspected.cache.Directory, recorded.Directory) || turnedOff
	if needsProjection {
		directory := recorded.Directory
		enabled := true
		settings.Directory = &directory
		settings.Enabled = &enabled
		inspected, err = inspect(host, &settings, freshnessMemoizedSuccess, executor)
		if err != nil {
			return "", err
		}
	}
	if inspected.cache == nil {
		reason := "host inspection returned no cache"
		if inspected.preview.OffReason != nil {
			reason = inspected.preview.OffReason.String()
		}
		return "", fmt.Errorf("build cache configuration unavailable: %s", reason)
	}
	return publishAt(host, inspected.cache, recorded.Directory, executor)
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
	if native == nil {
		return nativeMbxAbsent{}
	}
	if _, err := parseSemver(native.Version); err != nil {
		return nativeMbxUnknown{
			reason:  fmt.Sprintf("the host reported an unrecognized mbx version %s", core.RustDebugString(native.Version)),
			version: native.Version,
		}
	}
	return nativeMbxCompatible{native: *native}
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
	type hostTargets struct {
		host    CacheHost
		targets []string
	}
	hosts := map[string]*hostTargets{}
	checks := []DoctorHostMbx{}
	for _, id := range slices.Sorted(maps.Keys(config.Targets)) {
		target := config.Targets[id]
		var container core.ContainerTemplate
		switch target := target.(type) {
		case core.TargetTemplateLocalPodman:
			container = target.ContainerTemplate
		case core.TargetTemplateLocalDocker:
			container = target.ContainerTemplate
		case core.TargetTemplateSshPodman:
			container = target.ContainerTemplate
		case core.TargetTemplateSshDocker:
			container = target.ContainerTemplate
		default:
			continue
		}
		if container.BuildCache != nil && container.BuildCache.Enabled != nil && !*container.BuildCache.Enabled {
			continue
		}
		host, err := CacheHostForPathTarget(target)
		if err != nil {
			checks = append(checks, DoctorHostMbx{
				Host:    id,
				Targets: []string{id},
				Status:  DoctorHostMbxStatusUnknown{Value: alternate(err)},
			})
			continue
		}
		key := host.Key()
		entry, ok := hosts[key]
		if !ok {
			entry = &hostTargets{host: host}
			hosts[key] = entry
		}
		entry.targets = append(entry.targets, id)
	}
	for _, key := range slices.Sorted(maps.Keys(hosts)) {
		entry := hosts[key]
		status, err := func() (DoctorHostMbxStatus, error) {
			limit, err := platformLimitOf(entry.host, executor)
			if err != nil {
				return nil, err
			}
			if limit != nil {
				return DoctorHostMbxStatusUnsupported{Value: limit.reason}, nil
			}
			native, err := ProbeNativeVersion(entry.host, executor)
			if err != nil {
				return nil, err
			}
			switch status := classifyNativeMbx(native).(type) {
			case nativeMbxCompatible:
				return DoctorHostMbxStatusCompatible{Value: status.native.Version}, nil
			case nativeMbxUnknown:
				return DoctorHostMbxStatusUnknown{Value: status.reason}, nil
			}
			return DoctorHostMbxStatusAbsent{}, nil
		}()
		if err != nil {
			status = DoctorHostMbxStatusUnknown{Value: alternate(err)}
		}
		checks = append(checks, DoctorHostMbx{Host: key, Targets: entry.targets, Status: status})
	}
	return checks
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
	limit, err := platformLimitOf(host, executor)
	if err != nil {
		return inspection{}, err
	}
	if limit != nil {
		var nativeVersion *string
		if limit.native != nil {
			version := limit.native.Version
			nativeVersion = &version
		}
		return inspection{preview: core.BuildCachePreview{
			NativeMbx:   nativeVersion,
			Application: core.BuildCacheApplicationPending{},
			OffReason:   core.BuildCacheOffUnsupportedPlatform{Value: limit.reason},
		}}, nil
	}
	off := func(preview core.BuildCachePreview) inspection {
		return inspection{preview: preview}
	}
	probed, err := ProbeNativeVersion(host, executor)
	if err != nil {
		return inspection{}, err
	}
	compatibility := classifyNativeMbx(probed)
	compatible, ok := compatibility.(nativeMbxCompatible)
	if !ok {
		var nativeVersion *string
		if unknown, ok := compatibility.(nativeMbxUnknown); ok {
			version := unknown.version
			nativeVersion = &version
		}
		return off(core.BuildCachePreview{
			NativeMbx:   nativeVersion,
			Application: core.BuildCacheApplicationPending{},
			OffReason:   core.BuildCacheOffUnavailable{Value: cacheUnavailableReason(compatibility)},
		}), nil
	}
	native := compatible.native
	directory, err := nativeCacheDirectory(host, &native, executor)
	if err != nil {
		return inspection{}, err
	}
	if !core.IsAbsoluteOnHostOrTarget(directory) {
		return inspection{}, fmt.Errorf("build cache directory %s is not absolute", directory)
	}

	userManaged := true
	configDirectory := configurationSharedDirectory(directory)
	previousConfig, err := configurationReadFile(host, pathJoin(configDirectory, "config.toml"), executor)
	if err != nil {
		return inspection{}, err
	}
	text, err := hostConfigFile(host, executor)
	if err != nil {
		return inspection{}, err
	}
	configured, err := configurationConfiguredLimit(text, "gc", "max_total_size")
	if err != nil {
		return inspection{}, err
	}
	var limitSource core.BuildCacheLimit = core.BuildCacheLimitMbxDefault{}
	if configured != nil {
		limitSource = core.BuildCacheLimitHostConfiguration{Value: configured}
	}
	configText := ""
	if text != nil {
		configText = *text
	}
	configFile := &configText
	targetRoot := relocatedTargetRoot(configText, directory)
	application := configurationApplication(previousConfig, configFile)
	if _, pending := application.(core.BuildCacheApplicationPending); pending {
		applications.Lock()
		failure, recorded := applications.byKey[resolutionKey(host, settings)]
		applications.Unlock()
		if recorded && failure != nil {
			application = core.BuildCacheApplicationFailed{Value: alternate(failure)}
		}
	}
	// Read before the checks below, so a host that cannot share the cache
	// right now still reports what the cache did while it could.
	stats := readStats(host, directory, executor)
	nativeVersion := native.Version
	preview := func(offReason core.BuildCacheOff) core.BuildCachePreview {
		previewDirectory := directory
		budgetNote := "One budget covers shared compiler outputs, managed worktrees, and incremental state."
		var previewStats *core.BuildCacheStats
		if stats != nil {
			copied := *stats
			previewStats = &copied
		}
		return core.BuildCachePreview{
			NativeMbx:    &nativeVersion,
			Directory:    &previewDirectory,
			MaxTotalSize: limitSource,
			UserManaged:  userManaged,
			Application:  application,
			BudgetNote:   &budgetNote,
			Stats:        previewStats,
			OffReason:    offReason,
		}
	}

	// The directory may not exist yet; its filesystem is its nearest
	// existing ancestor's.
	volume, err := nearestExistingAncestor(host, directory, executor)
	if err != nil {
		return inspection{}, err
	}
	// A machine that is not turned off still has to support the cache: an
	// explicit `enabled = true` cannot make a volume without reflinks usable.
	if settings.Enabled != nil && !*settings.Enabled {
		return off(preview(core.BuildCacheOffTurnedOff{})), nil
	}
	supported, err := reflinksSupported(host, volume, executor)
	if err != nil {
		return inspection{}, err
	}
	if !supported {
		return off(preview(core.BuildCacheOffUnavailable{Value: fmt.Sprintf(
			"the filesystem under %s does not support reflinks, so restoring cached "+
				"outputs would copy every byte", directory)})), nil
	}
	reason, unusable, err := unusableFilesystem(host, volume, executor)
	if err != nil {
		return inspection{}, err
	}
	if unusable {
		return off(preview(core.BuildCacheOffUnavailable{Value: fmt.Sprintf(
			"%s is on a %s, where mbx's file locks are unreliable", directory, reason)})), nil
	}

	// A relocated target root is a separate mount, and a restore into it is a
	// clone only when it shares one with the store. Copying instead is correct
	// and much slower, and mbx's materializer falls back to it without saying
	// so, which makes this the only place it can be noticed. It is reported
	// rather than disqualifying: a slow cache still beats no cache.
	if targetRoot != nil {
		rootVolume, err := nearestExistingAncestor(host, *targetRoot, executor)
		if err != nil {
			return inspection{}, err
		}
		cross, err := crossReflinksSupported(host, volume, rootVolume, executor)
		if err != nil {
			return inspection{}, err
		}
		if !cross {
			slog.Warn("the host's mbx target root does not share a mount with the build cache, "+
				"so restoring a cached output copies every byte instead of cloning it",
				"cache", directory, "target_root", *targetRoot)
		}
	}

	return inspection{
		preview: preview(nil),
		cache: &ResolvedBuildCache{
			Directory:       directory,
			NativeMbx:       native,
			TargetRoot:      targetRoot,
			ConfigFile:      configFile,
			ConfigDirectory: configDirectory,
			PreviousConfig:  previousConfig,
		},
	}, nil
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
	path := pathJoin(directory, tallyRelative)
	command := host.ShellCommand(readConfigScript, label, []string{path}, "read the container host build cache totals")
	output, err := executor.Execute(&command)
	if err != nil || output.Status != 0 {
		return nil
	}
	var totals tally
	if err := decodeTally(output.Stdout, &totals); err != nil {
		slog.Debug(fmt.Sprintf("the build cache totals could not be read: %s", err), "path", path)
		return nil
	}
	return &core.BuildCacheStats{
		Builds:             totals.Builds,
		CachedCompilations: totals.CachedCompilations,
		AvoidedCompilerNs:  totals.AvoidedCompilerNs,
		ReflinkedBytes:     totals.ReflinkedBytes,
	}
}

// decodeTally reads the tally as serde reads a struct: a JSON object (a
// `null` is not one), missing counters zero and unknown ones ignored.
func decodeTally(data []byte, totals *tally) error {
	if trimmed := bytes.TrimSpace(data); bytes.Equal(trimmed, []byte("null")) {
		return errors.New("invalid type: null, expected struct Tally")
	}
	return json.Unmarshal(data, totals)
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
	command := host.ShellCommand(NativeVersionScript, label, nil, "read the container host mbx version")
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	if output.Status == 1 {
		return nil, nil
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("mbx version probe exited with status %d", output.Status)
	}
	native, err := parseNativeProbeOutput(output.Stdout)
	if err != nil {
		return nil, err
	}
	return &native, nil
}

func parseNativeProbeOutput(stdout []byte) (NativeMbx, error) {
	text := lossy(stdout)
	program, version, ok := strings.Cut(strings.TrimSpace(text), "\n")
	if !ok {
		return NativeMbx{}, errors.New("mbx version probe gave no version")
	}
	fields := strings.Fields(version)
	if len(fields) == 0 {
		return NativeMbx{}, errors.New("mbx version probe gave an empty version")
	}
	if !filepath.IsAbs(program) {
		return NativeMbx{}, fmt.Errorf("mbx version probe returned a non-absolute executable path %s", core.RustDebugString(program))
	}
	return NativeMbx{Program: program, Version: fields[len(fields)-1]}, nil
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
	status := classifyNativeMbx(native)
	compatible, ok := status.(nativeMbxCompatible)
	if !ok {
		return CachedMbxBinary{}, errors.New(cacheUnavailableReason(status))
	}
	current := compatible.native
	if !core.IsAbsoluteOnHostOrTarget(directory) {
		return CachedMbxBinary{}, fmt.Errorf("build cache directory is not absolute: %s", directory)
	}
	path := CacheBinaryPath(directory)
	key := host.Key() + "|" + path
	mbxSyncLocks.Lock()
	lock, ok := mbxSyncLocks.byKey[key]
	if !ok {
		lock = &sync.Mutex{}
		mbxSyncLocks.byKey[key] = lock
	}
	mbxSyncLocks.Unlock()
	lock.Lock()
	defer lock.Unlock()
	for attempt := range 3 {
		command := host.ShellCommand(syncMbxBinaryScript, label,
			[]string{current.Program, path, current.Version, NativeVersionScript},
			"synchronize native mbx into the shared cache")
		output, err := executor.Execute(&command)
		if err != nil {
			return CachedMbxBinary{}, err
		}
		if output.Status == 0 {
			return CachedMbxBinary{Path: path, Version: current.Version}, nil
		}
		if output.Status != 75 {
			return CachedMbxBinary{}, fmt.Errorf("%s failed with status %d: %s",
				command.Purpose, output.Status, trimmedLossy(output.Stderr))
		}
		probed, err := ProbeNativeVersion(host, executor)
		if err != nil {
			return CachedMbxBinary{}, err
		}
		status := classifyNativeMbx(probed)
		compatible, ok := status.(nativeMbxCompatible)
		if !ok {
			return CachedMbxBinary{}, errors.New(cacheUnavailableReason(status))
		}
		current = compatible.native
		if attempt == 2 {
			return CachedMbxBinary{}, errors.New("native mbx kept changing during cache synchronization")
		}
	}
	panic("the bounded synchronization retry loop returns or fails")
}

// AvailableBytes is the available column of a `df -P` report (the third
// field of its data row). Callers decide the block size used by the report
// they requested.
func AvailableBytes(report string) (uint64, bool) {
	var rows []string
	for _, line := range strings.Split(report, "\n") {
		if strings.TrimSpace(line) != "" {
			rows = append(rows, line)
		}
	}
	if len(rows) < 2 {
		return 0, false
	}
	fields := strings.Fields(rows[1])
	if len(fields) < 3 {
		return 0, false
	}
	return parseRustUint64(fields[len(fields)-3])
}

// parseRustUint64 is Rust's `str::parse::<u64>`: decimal digits with an
// optional leading `+`.
func parseRustUint64(text string) (uint64, bool) {
	digits := strings.TrimPrefix(text, "+")
	if digits == "" || digits[0] < '0' || digits[0] > '9' {
		return 0, false
	}
	value, err := strconv.ParseUint(digits, 10, 64)
	return value, err == nil
}

// nativeCacheDirectory is the host's own cache directory. `mbx cache dir`
// prints the store, which is the `actions` directory inside the cache
// directory.
func nativeCacheDirectory(host CacheHost, native *NativeMbx, executor core.CommandExecutor) (string, error) {
	command := host.Command([]string{native.Program, "cache", "dir", "--json"},
		"read the container host mbx cache directory")
	output, err := executor.Execute(&command)
	if err != nil {
		return "", err
	}
	output, err = mbxChecked(output, &command)
	if err != nil {
		return "", err
	}
	var report any
	if err := json.Unmarshal(output.Stdout, &report); err != nil {
		return "", fmt.Errorf("parse the mbx cache directory report: %w", err)
	}
	object, _ := report.(map[string]any)
	store, ok := object["store"].(string)
	if !ok {
		return "", errors.New("the mbx cache directory report has no store path")
	}
	parent, ok := pathParent(store)
	if !ok {
		return "", fmt.Errorf("mbx store path %s has no parent", core.RustDebugString(store))
	}
	return parent, nil
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
func readLocalStorageConfig(path string) (*string, error) {
	meta, err := os.Lstat(path)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("inspect %s: %w", path, err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		if meta.Mode()&os.ModeSymlink != 0 && os.IsNotExist(err) {
			return nil, fmt.Errorf("read %s: dangling symlink: %w", path, err)
		}
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	if !utf8.Valid(data) {
		return nil, fmt.Errorf("read %s: invalid UTF-8", path)
	}
	text := string(data)
	return &text, nil
}

type mbxStorageConfig struct{ cache, targetRoot *string }

func parseStorageConfig(text string) (mbxStorageConfig, error) {
	var document map[string]any
	var config mbxStorageConfig
	if _, err := toml.Decode(text, &document); err != nil {
		return config, fmt.Errorf("parse mbx storage configuration: %w", err)
	}
	if v, exists := document["cache_dir"]; exists {
		value, ok := v.(string)
		if !ok {
			return config, fmt.Errorf("cache_dir must be a string")
		}
		config.cache = &value
	}
	if v, exists := document["target"]; exists {
		target, ok := v.(map[string]any)
		if !ok {
			return config, fmt.Errorf("target must be a table")
		}
		if v, exists := target["root"]; exists {
			value, ok := v.(string)
			if !ok {
				return config, fmt.Errorf("target.root must be a string")
			}
			config.targetRoot = &value
		}
	}
	return config, nil
}
func LocalStoragePaths() ([]string, error) {
	var configFile *string
	configPath := "<unavailable>"
	if directory, err := os.UserConfigDir(); err == nil {
		configPath = pathJoin(directory, "mbx/config.toml")
		configFile, err = readLocalStorageConfig(configPath)
		if err != nil {
			return nil, err
		}
	}
	nonEmpty := func(name string) *string {
		if value := os.Getenv(name); value != "" {
			return &value
		}
		return nil
	}
	var defaultCache *string
	if directory, err := os.UserCacheDir(); err == nil {
		cache := pathJoin(directory, "mbx")
		defaultCache = &cache
	}
	paths, err := resolveLocalStoragePaths(nonEmpty("MBX_CACHE_DIR"), nonEmpty("MBX_TARGET_ROOT"), configFile, defaultCache)
	if err != nil {
		return nil, fmt.Errorf("resolve mbx config %s: %w", configPath, err)
	}
	return paths, nil
}

func resolveLocalStoragePaths(cacheOverride, targetRootOverride, configFile, defaultCache *string) ([]string, error) {
	var config mbxStorageConfig
	if configFile != nil {
		var err error
		config, err = parseStorageConfig(*configFile)
		if err != nil {
			return nil, err
		}
	}
	cache := firstPresent(cacheOverride, config.cache, defaultCache)
	if cache == nil {
		return []string{}, nil
	}
	configuredTargetRoot := config.targetRoot
	targetRoot := pathJoin(*cache, "targets")
	if root := firstPresent(targetRootOverride, configuredTargetRoot); root != nil {
		if filepath.IsAbs(*root) {
			targetRoot = *root
		} else {
			targetRoot = pathJoin(*cache, *root)
		}
	}
	return []string{*cache, targetRoot}, nil
}

// firstPresent is Rust's `Option::or` chain: the first value that is set.
func firstPresent(values ...*string) *string {
	for _, value := range values {
		if value != nil {
			return value
		}
	}
	return nil
}

// relocatedTargetRoot is the `[target] root` a host configuration sets, when
// it lies outside the cache directory and therefore needs its own mount.
func relocatedTargetRoot(configFile string, directory string) *string {
	config, err := parseStorageConfig(configFile)
	if err != nil {
		slog.Warn(fmt.Sprintf("the host mbx configuration is unreadable: %s", err))
		return nil
	}
	if config.targetRoot == nil {
		return nil
	}
	root := *config.targetRoot
	joined := pathJoin(directory, root)
	if pathStartsWith(joined, directory) {
		return nil
	}
	return &joined
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
	command := host.ShellCommand(nearestAncestorScript, label, []string{directory}, "locate the build cache volume")
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
		return "", fmt.Errorf("decode cache ancestor: %w", err)
	}
	if !filepath.IsAbs(path) {
		return "", fmt.Errorf("build cache volume %s is not absolute", path)
	}
	return path, nil
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
	filesystems, err := targets.ProbeFilesystemTypes(host.SSH, []string{directory}, executor)
	if err != nil {
		return "", false, err
	}
	if len(filesystems) == 0 {
		return "", false, errors.New("the filesystem probe named no filesystem")
	}
	// OverlayUnsupportedFilesystem already groups virtiofs and 9p with the
	// network filesystems. The other reasons it gives are about stacking an
	// overlay, which a plain read-write bind mount does not do.
	reason, ok := core.OverlayUnsupportedFilesystem(filesystems[0])
	if ok && (reason == "network filesystem" || reason == "FUSE filesystem") {
		return reason, true, nil
	}
	return "", false, nil
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
	placement, settings, ok := supportedHost(target)
	if !ok {
		return nil
	}
	if placement.shared == nil {
		// Use the session's identity so the machine policy service never
		// mistakes this guest directory for a retained host mount.
		if settings.Enabled != nil && !*settings.Enabled {
			return nil
		}
		return &core.SessionBuildCache{
			Host:      "nitro:" + session.ID,
			Directory: nitroCacheDirectory,
		}
	}
	host := *placement.shared
	// Sessions at the legacy shared `/workspace` would collide in the
	// shared store's path-keyed records.
	if session.ContainerWorkspace == nil {
		return nil
	}
	resolved := resolveShared(host, &settings, executor)
	if resolved == nil {
		return nil
	}
	// Make mbx available before the checkout exists. Target-side discovery
	// decides whether to wrap C/C++ tools; a mirror's HEAD can differ from
	// the session's branch, and Cargo is no longer the only consumer.
	synced, err := SyncCurrentMbxBinary(host, resolved.Directory, executor)
	if err != nil {
		slog.Warn(fmt.Sprintf("the shared mbx binary could not be synchronized; the session runs without the build cache: %s", alternate(err)),
			"host", host.Key())
		executor.NotifyNotice(fmt.Sprintf("The shared Rust build cache is unavailable: %s. The session will start without it.", alternate(err)))
		return nil
	}
	if unavailable, ok := synced.(CachedMbxSyncUnavailable); ok {
		slog.Warn(fmt.Sprintf("sessions run without the shared build cache: %s", unavailable.Value), "host", host.Key())
		executor.NotifyNotice(fmt.Sprintf("The shared Rust build cache is unavailable: %s. The session will start without it.", unavailable.Value))
		return nil
	}
	buildCache := core.SessionBuildCache{
		Host:       host.Key(),
		Directory:  resolved.Directory,
		TargetRoot: resolved.TargetRoot,
	}
	if !attachMounts(&buildCache, mounts) {
		return nil
	}
	return &buildCache
}

// attachMounts mounts the cache, and a relocated target root, read-write at
// the same absolute paths the host uses. An attached directory that already
// covers one of those paths wins, and the session runs without the cache.
func attachMounts(buildCache *core.SessionBuildCache, mounts *[]core.AdditionalMount) bool {
	wanted := []string{buildCache.Directory}
	if buildCache.TargetRoot != nil {
		wanted = append(wanted, *buildCache.TargetRoot)
	}
	for _, destination := range wanted {
		for _, mount := range *mounts {
			if pathStartsWith(mount.Destination, destination) || pathStartsWith(destination, mount.Destination) {
				slog.Warn("an attached directory overlaps the build cache, so this session runs without it",
					"destination", destination)
				return false
			}
		}
	}
	for _, directory := range wanted {
		*mounts = append(*mounts, core.AdditionalMount{
			Source:      directory,
			Destination: directory,
			Access:      core.MountAccessRw,
		})
	}
	return true
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

// semverVersion is a version as Rust's semver::Version holds it.
type semverVersion struct {
	major, minor, patch uint64
	pre, build          string
}

// parseSemver reads a version as semver::Version::parse does: exactly
// `MAJOR.MINOR.PATCH`, without leading zeros, then an optional pre-release
// and build metadata, and nothing else.
func parseSemver(text string) (semverVersion, error) {
	var version semverVersion
	rest := text
	numbers := []*uint64{&version.major, &version.minor, &version.patch}
	for index, number := range numbers {
		end := 0
		for end < len(rest) && rest[end] >= '0' && rest[end] <= '9' {
			end++
		}
		if end == 0 || end > 1 && rest[0] == '0' {
			return version, fmt.Errorf("invalid version number in %s", core.RustDebugString(text))
		}
		value, err := strconv.ParseUint(rest[:end], 10, 64)
		if err != nil {
			return version, fmt.Errorf("version number exceeds u64::MAX in %s", core.RustDebugString(text))
		}
		*number = value
		rest = rest[end:]
		if index < 2 {
			if !strings.HasPrefix(rest, ".") {
				return version, fmt.Errorf("unexpected character in %s", core.RustDebugString(text))
			}
			rest = rest[1:]
		}
	}
	if pre, ok := strings.CutPrefix(rest, "-"); ok {
		end := strings.IndexByte(pre, '+')
		if end < 0 {
			end = len(pre)
		}
		if !semverIdentifiersValid(pre[:end], true) {
			return version, fmt.Errorf("invalid pre-release in %s", core.RustDebugString(text))
		}
		version.pre = pre[:end]
		rest = pre[end:]
	}
	if build, ok := strings.CutPrefix(rest, "+"); ok {
		if !semverIdentifiersValid(build, false) {
			return version, fmt.Errorf("invalid build metadata in %s", core.RustDebugString(text))
		}
		version.build = build
		rest = ""
	}
	if rest != "" {
		return version, fmt.Errorf("unexpected character after patch version number in %s", core.RustDebugString(text))
	}
	return version, nil
}

// semverIdentifiersValid checks dot-separated pre-release or build
// identifiers: non-empty ASCII alphanumerics and hyphens, and for a
// pre-release no numeric identifier with a leading zero.
func semverIdentifiersValid(text string, rejectLeadingZero bool) bool {
	for _, identifier := range strings.Split(text, ".") {
		if identifier == "" {
			return false
		}
		numeric := true
		for i := 0; i < len(identifier); i++ {
			c := identifier[i]
			if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c == '-') {
				return false
			}
			numeric = numeric && c >= '0' && c <= '9'
		}
		if rejectLeadingZero && numeric && len(identifier) > 1 && identifier[0] == '0' {
			return false
		}
	}
	return true
}

// compareSemver is semver::Version's Ord: major, minor, patch, then a
// pre-release below its release, then build metadata.
func compareSemver(left, right semverVersion) int {
	if order := cmp.Compare(left.major, right.major); order != 0 {
		return order
	}
	if order := cmp.Compare(left.minor, right.minor); order != 0 {
		return order
	}
	if order := cmp.Compare(left.patch, right.patch); order != 0 {
		return order
	}
	switch {
	case left.pre == right.pre:
	case left.pre == "":
		return 1
	case right.pre == "":
		return -1
	default:
		if order := compareSemverIdentifiers(left.pre, right.pre); order != 0 {
			return order
		}
	}
	switch {
	case left.build == right.build:
		return 0
	case left.build == "":
		return -1
	case right.build == "":
		return 1
	}
	return compareSemverIdentifiers(left.build, right.build)
}

// compareSemverIdentifiers orders dot-separated identifiers: numeric ones
// numerically and below alphanumeric ones, alphanumeric ones by their bytes,
// and a shorter list of equal identifiers first.
func compareSemverIdentifiers(left, right string) int {
	a, b := strings.Split(left, "."), strings.Split(right, ".")
	numeric := func(text string) bool {
		for i := 0; i < len(text); i++ {
			if text[i] < '0' || text[i] > '9' {
				return false
			}
		}
		return true
	}
	for i := 0; i < len(a) && i < len(b); i++ {
		var order int
		switch leftNumeric, rightNumeric := numeric(a[i]), numeric(b[i]); {
		case leftNumeric && rightNumeric:
			l, r := strings.TrimLeft(a[i], "0"), strings.TrimLeft(b[i], "0")
			order = cmp.Compare(len(l), len(r))
			if order == 0 {
				order = strings.Compare(l, r)
			}
			if order == 0 {
				// More leading zeros sort later, as semver orders build metadata.
				order = cmp.Compare(len(a[i]), len(b[i]))
			}
		case leftNumeric:
			order = -1
		case rightNumeric:
			order = 1
		default:
			order = strings.Compare(a[i], b[i])
		}
		if order != 0 {
			return order
		}
	}
	return cmp.Compare(len(a), len(b))
}
