// Port of mj-worker-staging/src/worker_binary/binary_source.rs.

package workerstaging

import (
	"fmt"
	"log/slog"
	"os"
	"runtime"
	"slices"
	"strings"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// WorkerBinaryAvailability is where a worker binary can be had: a local
// file (WorkerBinaryAvailabilityLocal) or a download
// (WorkerBinaryAvailabilityRemote). Rust enum; the variants are comparable
// with ==.
type WorkerBinaryAvailability interface {
	isWorkerBinaryAvailability()
}

type WorkerBinaryAvailabilityLocal struct {
	Path   string
	Source string
}

type WorkerBinaryAvailabilityRemote struct {
	URL    string
	SHA256 string
	Triple string
}

func (WorkerBinaryAvailabilityLocal) isWorkerBinaryAvailability()  {}
func (WorkerBinaryAvailabilityRemote) isWorkerBinaryAvailability() {}

// WorkerBinarySourceSnapshot is the sources captured before the daemon
// starts its managers and coordinators.
//
// Local sources are copied into an immutable, content-addressed cache
// during capture. Remote sources retain only their URL, digest, and target
// triple; the network fetch still happens when a target is provisioned.
type WorkerBinarySourceSnapshot struct {
	cacheRoot string
	Entries   map[WorkerBinarySourceKey]WorkerBinarySourceResult
}

// WorkerBinarySourceKey is Rust's (architecture, requirement) entry key.
type WorkerBinarySourceKey struct {
	Arch        string
	Requirement WorkerBinaryRequirement
}

// WorkerBinarySourceResult is Rust's `Result<WorkerBinaryAvailability,
// String>`: Availability is nil when the source was unavailable, and Error
// then says why.
type WorkerBinarySourceResult struct {
	Availability WorkerBinaryAvailability
	Error        string
}

// WorkerBinarySourcesCell is Rust's `OnceLock<WorkerBinarySourceSnapshot>`:
// set at most once, read many times.
type WorkerBinarySourcesCell struct {
	snapshot atomic.Pointer[WorkerBinarySourceSnapshot]
}

// Get is the snapshot, nil before one is set.
func (c *WorkerBinarySourcesCell) Get() *WorkerBinarySourceSnapshot { return c.snapshot.Load() }

// Set stores snapshot unless one is already set; false when it was.
func (c *WorkerBinarySourcesCell) Set(snapshot *WorkerBinarySourceSnapshot) bool {
	return c.snapshot.CompareAndSwap(nil, snapshot)
}

// PinnedWorkerBinarySources is Rust's PINNED_WORKER_BINARY_SOURCES.
var PinnedWorkerBinarySources WorkerBinarySourcesCell

func PackagedWorkerBinaryPath(directory, triple string) string {
	return pathJoin(directory, "mj-worker-"+triple)
}

// RunningExecutableFileName: Linux exposes an unlinked running executable
// through `/proc` with a ` (deleted)` suffix. os.Executable preserves that
// suffix, but it is not part of the executable's real file name and must not
// leak into sibling lookup after `cargo` or a package upgrade replaces the
// controller. False when controller has no file name.
func RunningExecutableFileName(controller string) (string, bool) {
	name, ok := pathFileName(controller)
	if !ok {
		return "", false
	}
	if runtime.GOOS == "linux" {
		if trimmed, deleted := strings.CutSuffix(name, " (deleted)"); deleted {
			return trimmed, true
		}
	}
	return name, true
}

// WorkerSiblingNames is the file names a worker binary may carry when it
// sits beside the controller or in a development sibling directory. The
// controller's own file name comes first (after the 2.0 rename that is
// `mj`), then the legacy `hel` name that older packages shipped, so both
// resolve without hardcoding one.
func WorkerSiblingNames(controller string) []string {
	var names []string
	if own, ok := RunningExecutableFileName(controller); ok {
		names = append(names, own)
	}
	if !slices.Contains(names, "hel") {
		names = append(names, "hel")
	}
	return names
}

// SelectNativeWorker: a local-bare session runs on the controller host, so
// it may use the native worker built or packaged beside `mj`. Managed
// targets never consider this name because a macOS or glibc binary is not
// portable into Linux targets. False when there is none.
func SelectNativeWorker(controller string, isFile func(path string) bool) (path string, source string, ok bool) {
	directory, ok := pathParent(controller)
	if !ok {
		return "", "", false
	}
	profile, hasProfile := pathFileName(directory)
	targetDir, hasTargetDir := pathParent(directory)
	if hasProfile && hasTargetDir {
		development := pathJoin(targetDir, "worker", profile, "mj-worker")
		if isFile(development) {
			return development, "isolated native development worker", true
		}
	}
	packaged := pathJoin(directory, "mj-worker")
	if isFile(packaged) {
		return packaged, "native worker beside mj", true
	}
	return "", "", false
}

// SelectSiblingWorker chooses a worker binary that ships beside the
// controller or in a development musl sibling directory. isFile probes the
// filesystem; tests pass a hand-written probe. The static musl sibling is
// probed before the worker in the controller's own directory, because in a
// development checkout that same-directory candidate resolves to the
// controller itself, whose glibc may be newer than the target's. False when
// there is none.
func SelectSiblingWorker(controller, triple string, isFile func(path string) bool) (path string, source string, ok bool) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs select_sibling_worker")
}

// WorkerBinaryRequirement is the kind of worker a target needs. The values
// are the Rust variant names, which messages print with `{:?}`.
type WorkerBinaryRequirement string

const (
	WorkerBinaryRequirementPortableLinux WorkerBinaryRequirement = "PortableLinux"
	WorkerBinaryRequirementDarwin        WorkerBinaryRequirement = "Darwin"
	WorkerBinaryRequirementLocalHost     WorkerBinaryRequirement = "LocalHost"
)

// WorkerBinaryRequirementForOs is Rust's `WorkerBinaryRequirement::for_os`.
func WorkerBinaryRequirementForOs(os targets.TargetOs) WorkerBinaryRequirement {
	if os == targets.TargetOsDarwin {
		return WorkerBinaryRequirementDarwin
	}
	return WorkerBinaryRequirementPortableLinux
}

func (r WorkerBinaryRequirement) Triple(arch string) string {
	if r == WorkerBinaryRequirementDarwin || (r == WorkerBinaryRequirementLocalHost && runtime.GOOS == "darwin") {
		return arch + "-apple-darwin"
	}
	return arch + "-unknown-linux-musl"
}

// WorkerBinarySourceSnapshotCapture is Rust's
// `WorkerBinarySourceSnapshot::capture`: resolve and pin every source the
// daemon may use.
//
// Resolution runs per source on its own goroutine, and each distinct local
// file is pinned on its own goroutine, so one slow disk read does not queue
// the others behind it. resolve may therefore be called concurrently.
func WorkerBinarySourceSnapshotCapture(cacheRoot string, resolve func(arch string, requirement WorkerBinaryRequirement) (WorkerBinaryAvailability, error)) *WorkerBinarySourceSnapshot {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs WorkerBinarySourceSnapshot::capture")
}

func (s *WorkerBinarySourceSnapshot) Resolve(arch string, requirement WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs WorkerBinarySourceSnapshot::resolve")
}

// PinWorkerBinarySources captures the worker sources used by this daemon
// before its asynchronous managers start. Missing sources are retained as
// per-architecture errors so an unused architecture does not prevent daemon
// startup.
func PinWorkerBinarySources() error {
	if PinnedWorkerBinarySources.Get() != nil {
		return nil
	}
	started := time.Now()
	snapshot, err := captureWorkerBinarySources()
	if err != nil {
		return err
	}
	slog.Info("worker sources pinned", "elapsed_ms", time.Since(started).Milliseconds())
	// The daemon boot path calls this once. If a second caller races it, keep
	// the first complete snapshot and never replace paths it may already use.
	PinnedWorkerBinarySources.Set(snapshot)
	return nil
}

// WarmWorkerBinarySources verifies and pins this build's worker sources
// into the shared cache without keeping the snapshot, so the daemon this
// build starts next finds every source already indexed.
//
// A client replacing the daemon runs this while the old daemon drains: the
// copying and hashing a new build needs then happens beside the handoff
// instead of on the new daemon's startup path. The cache is
// content-addressed and published atomically without replacing anything,
// so the old daemon's pins are unaffected. Startup still captures the
// sources itself and stays the only owner of the pinned snapshot.
func WarmWorkerBinarySources() error {
	_, err := captureWorkerBinarySources()
	return err
}

func captureWorkerBinarySources() (*WorkerBinarySourceSnapshot, error) {
	current, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("resolve Mjolnir controller binary: %w", rustIOError(err))
	}
	cacheRoot := pathJoin(core.DataDir(), "workers", "pinned")
	return WorkerBinarySourceSnapshotCapture(cacheRoot, func(arch string, requirement WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		return WorkerBinaryPrerequisiteWithVerifier(arch, requirement, current, isFile,
			func(path string) error { return VerifyWorkerBuildIndexed(cacheRoot, path) })
	}), nil
}

// indexEntryPath is the cached record of a source file this daemon already
// verified and pinned.
//
// The key covers the build id and the source's path, size, mtime, inode
// and ctime, so a rebuilt or replaced file misses. The value is the digest
// of the pinned copy. The index only saves work: deleting it makes the next
// start read, verify and hash every source again. False when the source
// cannot be read.
func indexEntryPath(cacheRoot, source string) (string, bool) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs index_entry_path")
}

// IndexedPinnedWorker is the pinned copy of source if the index says this
// exact file was already pinned and the copy is still present with the
// same length.
func IndexedPinnedWorker(cacheRoot, source string) (string, bool) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs indexed_pinned_worker")
}

func recordIndexedPin(cacheRoot, source, digest string) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs record_indexed_pin")
}

// VerifyWorkerBuildIndexed is core.VerifyWorkerBuild, skipped for a file
// the index already knows: the same path, size, mtime, inode and ctime were
// verified against this build and pinned before. Anything else is read and
// checked in full.
func VerifyWorkerBuildIndexed(cacheRoot, path string) error {
	if _, ok := IndexedPinnedWorker(cacheRoot, path); ok {
		return nil
	}
	return core.VerifyWorkerBuild(path)
}

func CopyWorkerSourceToCache(source, cacheRoot string) (string, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs copy_worker_source_to_cache")
}

// PublishCachedWorker publishes one immutable cache artifact from
// temporary, a file created in cacheRoot (Rust: tempfile::NamedTempFile).
// It takes ownership of temporary: the file is closed, and removed unless it
// became the artifact. The final publication is atomic and never replaces
// an artifact another daemon may have already captured.
func PublishCachedWorker(temporary *os.File, cacheRoot, digest string) (string, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs publish_cached_worker")
}

func verifyCachedWorker(path, digest string) error {
	if err := core.VerifyWorkerBuild(path); err != nil {
		return err
	}
	actual, err := core.WorkerExecutableDigest(path)
	if err != nil {
		return err
	}
	if actual != digest {
		return fmt.Errorf("content-addressed worker cache %s does not match %s checksum", path, digest)
	}
	return nil
}

// WorkerBinaryPrerequisiteForArch finds a worker source without
// downloading it.
//
// Container provisioning resolves this after discovering the target
// architecture. Doctor uses the same lookup with the selected container's
// expected architecture, so it can recommend a fix without creating a
// container or making a network request.
func WorkerBinaryPrerequisiteForArch(arch string) (WorkerBinaryAvailability, error) {
	return WorkerBinaryForArch(arch, WorkerBinaryRequirementPortableLinux)
}

// NativeWorkerBinaryPrerequisite is the worker a `local-bare` session on
// this host would use.
//
// A local session runs on the controller's own machine, so it may use the
// native worker rather than the portable Linux one. `mj doctor` reports on
// it separately for that reason: rebuilding only the portable worker leaves
// a local session on old code, and the other way round.
func NativeWorkerBinaryPrerequisite() (WorkerBinaryAvailability, error) {
	return WorkerBinaryForArch(rustArch(), WorkerBinaryRequirementLocalHost)
}

func WorkerBinaryForArch(arch string, requirement WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs worker_binary_for_arch")
}

// PinnedSourceIsUsable is whether a pinned worker source (Rust:
// `&Result<WorkerBinaryAvailability>`, here the pair) can still be used as
// it stands.
//
// A remote source is a URL and stays usable. A local one is a path, and a
// path can stop being a file after the daemon pinned it: a build directory a
// cache reaper removed is exactly #1068.
func PinnedSourceIsUsable(pinned WorkerBinaryAvailability, pinnedErr error, isFile func(path string) bool) bool {
	if pinnedErr != nil {
		return false
	}
	switch source := pinned.(type) {
	case WorkerBinaryAvailabilityLocal:
		return isFile(source.Path)
	case WorkerBinaryAvailabilityRemote:
		return true
	}
	return false
}

// resolveWorkerSourceAgain resolves a worker source now, after the pinned
// one turned out to be unusable. pinnedError is why, nil when the pin was
// usable but its file is gone.
//
// A resolved local binary is copied into the daemon's own pinned cache, so
// whatever removed the first one cannot remove this one too.
func resolveWorkerSourceAgain(arch string, requirement WorkerBinaryRequirement, pinnedError error) (WorkerBinaryAvailability, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs resolve_worker_source_again")
}

// WorkerBinaryPrerequisiteForCurrent is the lookup itself, with the
// controller's own path and the file probe passed in so both can be
// exercised without the machine they describe.
func WorkerBinaryPrerequisiteForCurrent(arch string, requirement WorkerBinaryRequirement, current string, isFile func(path string) bool) (WorkerBinaryAvailability, error) {
	return WorkerBinaryPrerequisiteWithVerifier(arch, requirement, current, isFile, core.VerifyWorkerBuild)
}

// WorkerBinaryPrerequisiteWithVerifier is the lookup with the build-stamp
// check passed in. Startup pinning passes a check that skips files it
// already verified; other callers use the process-local executable memo.
func WorkerBinaryPrerequisiteWithVerifier(arch string, requirement WorkerBinaryRequirement, current string, isFile func(path string) bool, verify func(path string) error) (WorkerBinaryAvailability, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_source.rs worker_binary_prerequisite_with_verifier")
}
