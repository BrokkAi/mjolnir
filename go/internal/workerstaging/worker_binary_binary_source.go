// Port of mj-worker-staging/src/worker_binary/binary_source.rs.

package workerstaging

import (
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"runtime"
	"slices"
	"strings"
	"sync"
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
	directory, ok := pathParent(controller)
	if !ok {
		return "", "", false
	}
	names := WorkerSiblingNames(controller)
	type candidate struct{ path, source string }
	// Packaged worker beside the controller, named for the target triple.
	candidates := []candidate{{PackagedWorkerBinaryPath(directory, triple), "beside the mj binary"}}
	darwin := strings.HasSuffix(triple, "-apple-darwin")
	if darwin {
		candidates = append(candidates, candidate{
			PackagedWorkerBinaryPath(directory, "universal-apple-darwin"), "universal Darwin worker beside mj",
		})
	}
	// Development checkout: a controller at target/<profile>/<name> finds its
	// musl sibling at target/<triple>/<profile>/<name>. The static build is
	// preferred because the target's glibc may be older than the host's, so
	// it is probed before the same-directory worker (which is the controller
	// itself in a development checkout).
	profile, hasProfile := pathFileName(directory)
	targetDir, hasTargetDir := pathParent(directory)
	if hasProfile && hasTargetDir {
		candidates = append(candidates,
			candidate{pathJoin(targetDir, "worker", triple, profile, "mj-worker"), "isolated development musl worker"},
			candidate{pathJoin(targetDir, triple, profile, "mj-worker"), "development musl worker"})
		for _, name := range names {
			candidates = append(candidates, candidate{pathJoin(targetDir, triple, profile, name), "development musl sibling"})
		}
	}
	// A legacy package may put an `hel`-named worker beside an `mj`
	// controller. Never select the controller's own same-directory path: on
	// glibc Linux that is not a portable worker, and after an upgrade it is
	// the replacement controller rather than the still-running executable.
	controllerName, hasControllerName := RunningExecutableFileName(controller)
	if !darwin {
		for _, name := range names {
			if hasControllerName && name == controllerName {
				continue
			}
			candidates = append(candidates, candidate{pathJoin(directory, name), "beside the running executable"})
		}
	}
	for _, candidate := range candidates {
		if isFile(candidate.path) {
			return candidate.path, candidate.source, true
		}
	}
	return "", "", false
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
	architectures := []WorkerBinarySourceKey{
		{rustArch(), WorkerBinaryRequirementLocalHost},
		{"x86_64", WorkerBinaryRequirementPortableLinux},
		{"aarch64", WorkerBinaryRequirementPortableLinux},
		{"x86_64", WorkerBinaryRequirementDarwin},
		{"aarch64", WorkerBinaryRequirementDarwin},
	}
	type resolution struct {
		availability WorkerBinaryAvailability
		err          error
		elapsed      time.Duration
	}
	resolved := make([]resolution, len(architectures))
	var group sync.WaitGroup
	for index, key := range architectures {
		group.Go(func() {
			defer func() {
				if recover() != nil {
					resolved[index] = resolution{err: fmt.Errorf(
						"resolving the worker source for %s (%s) panicked", key.Arch, key.Requirement)}
				}
			}()
			started := time.Now()
			availability, err := resolve(key.Arch, key.Requirement)
			resolved[index] = resolution{availability, err, time.Since(started)}
		})
	}
	group.Wait()

	var uniquePaths []string
	for _, result := range resolved {
		if local, ok := result.availability.(WorkerBinaryAvailabilityLocal); ok && result.err == nil &&
			!slices.Contains(uniquePaths, local.Path) {
			uniquePaths = append(uniquePaths, local.Path)
		}
	}
	type pin struct {
		cached  string
		err     error
		elapsed time.Duration
	}
	pins := make([]pin, len(uniquePaths))
	for index, path := range uniquePaths {
		group.Go(func() {
			defer func() {
				if recover() != nil {
					pins[index] = pin{err: fmt.Errorf("pinning %s panicked", path)}
				}
			}()
			started := time.Now()
			cached, err := CopyWorkerSourceToCache(path, cacheRoot)
			pins[index] = pin{cached, err, time.Since(started)}
		})
	}
	group.Wait()
	pinnedPaths := map[string]pin{}
	for index, path := range uniquePaths {
		pinnedPaths[path] = pins[index]
	}

	entries := map[WorkerBinarySourceKey]WorkerBinarySourceResult{}
	for index, key := range architectures {
		result := resolved[index]
		arch, requirement := key.Arch, key.Requirement
		var pinned WorkerBinarySourceResult
		switch availability := result.availability.(type) {
		case WorkerBinaryAvailabilityLocal:
			if result.err != nil {
				break
			}
			outcome := pinnedPaths[availability.Path]
			if outcome.err == nil {
				slog.Info("worker source selected", "triple", requirement.Triple(arch), "source", availability.Source,
					"path", availability.Path, "pinned", outcome.cached, "build", core.WorkerBuildID(),
					"resolve_ms", result.elapsed.Milliseconds(), "pin_ms", outcome.elapsed.Milliseconds())
				pinned = WorkerBinarySourceResult{Availability: WorkerBinaryAvailabilityLocal{Path: outcome.cached, Source: availability.Source}}
			} else {
				message := fmt.Sprintf("pin worker source %s for %s (%s): %s", availability.Path, arch, requirement, outcome.err)
				slog.Warn(message, "arch", arch, "requirement", string(requirement), "error", message)
				pinned = WorkerBinarySourceResult{Error: message}
			}
		case WorkerBinaryAvailabilityRemote:
			if result.err != nil {
				break
			}
			slog.Info("worker source selected", "triple", availability.Triple, "source", "MJ_WORKER_URL",
				"url", availability.URL, "sha256", availability.SHA256, "build", core.WorkerBuildID())
			pinned = WorkerBinarySourceResult{Availability: availability}
		}
		if result.err != nil {
			message := result.err.Error()
			slog.Debug("worker source was unavailable when the daemon started",
				"arch", arch, "requirement", string(requirement), "error", message)
			pinned = WorkerBinarySourceResult{Error: message}
		} else if pinned.Availability == nil && pinned.Error == "" {
			pinned = WorkerBinarySourceResult{Error: "no worker source"}
		}
		entries[key] = pinned
	}
	return &WorkerBinarySourceSnapshot{cacheRoot: cacheRoot, Entries: entries}
}

func (s *WorkerBinarySourceSnapshot) Resolve(arch string, requirement WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
	source, ok := s.Entries[WorkerBinarySourceKey{Arch: arch, Requirement: requirement}]
	if !ok {
		return nil, fmt.Errorf("worker source for %s (%s) was not captured when the daemon started", arch, requirement)
	}
	if source.Availability == nil {
		return nil, fmt.Errorf("worker source for %s (%s) was unavailable when the daemon started: %s", arch, requirement, source.Error)
	}
	if local, ok := source.Availability.(WorkerBinaryAvailabilityLocal); ok {
		if err := VerifyWorkerBuildIndexed(s.cacheRoot, local.Path); err != nil {
			slog.Warn("rejecting pinned worker source", "path", local.Path, "error", err.Error())
			return nil, err
		}
	}
	return source.Availability, nil
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
	info, err := os.Stat(source)
	if err != nil {
		return "", false
	}
	key := sha256.New()
	key.Write([]byte(core.WorkerBuildID()))
	key.Write([]byte{0})
	key.Write([]byte(source))
	key.Write(binary.LittleEndian.AppendUint64(nil, uint64(info.Size())))
	if !indexFileIdentity(key, info) {
		return "", false
	}
	return pathJoin(cacheRoot, "index", core.LowerHex(key.Sum(nil))), true
}

// IndexedPinnedWorker is the pinned copy of source if the index says this
// exact file was already pinned and the copy is still present with the
// same length.
func IndexedPinnedWorker(cacheRoot, source string) (string, bool) {
	entry, ok := indexEntryPath(cacheRoot, source)
	if !ok {
		return "", false
	}
	recorded, err := os.ReadFile(entry)
	if err != nil {
		return "", false
	}
	digest := strings.TrimSpace(string(recorded))
	if len(digest) != 64 {
		return "", false
	}
	for index := 0; index < len(digest); index++ {
		b := digest[index]
		if !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f' || b >= 'A' && b <= 'F') {
			return "", false
		}
	}
	pinned := pathJoin(cacheRoot, digest, "hel")
	sourceInfo, err := os.Stat(source)
	if err != nil {
		return "", false
	}
	pinnedInfo, err := os.Stat(pinned)
	if err != nil || pinnedInfo.Size() != sourceInfo.Size() {
		return "", false
	}
	return pinned, true
}

func recordIndexedPin(cacheRoot, source, digest string) {
	entry, ok := indexEntryPath(cacheRoot, source)
	if !ok {
		return
	}
	// Best effort: a missing entry only costs a re-hash on the next start.
	write := func() error {
		directory, _ := pathParent(entry)
		if err := os.MkdirAll(directory, 0o777); err != nil {
			return err
		}
		staged, err := os.CreateTemp(directory, ".tmp")
		if err != nil {
			return err
		}
		defer os.Remove(staged.Name())
		if _, err := staged.WriteString(digest); err != nil {
			staged.Close()
			return err
		}
		if err := staged.Close(); err != nil {
			return err
		}
		return os.Rename(staged.Name(), entry)
	}
	if err := write(); err != nil {
		slog.Debug("could not index a pinned worker", "source", source, "error", rustIOError(err).Error())
	}
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
	if pinned, ok := IndexedPinnedWorker(cacheRoot, source); ok {
		return pinned, nil
	}
	if err := os.MkdirAll(cacheRoot, 0o777); err != nil {
		return "", fmt.Errorf("create pinned worker cache %s: %w", cacheRoot, rustIOError(err))
	}
	input, err := os.Open(source)
	if err != nil {
		return "", fmt.Errorf("open worker source %s: %w", source, rustIOError(err))
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return "", fmt.Errorf("stat worker source %s: %w", source, rustIOError(err))
	}
	temporary, err := os.CreateTemp(cacheRoot, ".tmp")
	if err != nil {
		return "", fmt.Errorf("create pinned worker staging file %s: %w", cacheRoot, rustIOError(err))
	}
	discard := func() {
		temporary.Close()
		os.Remove(temporary.Name())
	}
	digest := sha256.New()
	buffer := make([]byte, 128*1024)
	for {
		count, err := input.Read(buffer)
		if count > 0 {
			if _, err := temporary.Write(buffer[:count]); err != nil {
				discard()
				return "", fmt.Errorf("copy worker source %s: %w", source, rustIOError(err))
			}
			digest.Write(buffer[:count])
		}
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			discard()
			return "", fmt.Errorf("read worker source %s: %w", source, rustIOError(err))
		}
	}
	if err := temporary.Sync(); err != nil {
		discard()
		return "", fmt.Errorf("flush pinned worker source %s: %w", source, rustIOError(err))
	}
	if err := os.Chmod(temporary.Name(), info.Mode().Perm()); err != nil {
		discard()
		return "", fmt.Errorf("preserve permissions for %s: %w", source, rustIOError(err))
	}
	sum := core.LowerHex(digest.Sum(nil))
	pinned, err := PublishCachedWorker(temporary, cacheRoot, sum)
	if err != nil {
		return "", err
	}
	recordIndexedPin(cacheRoot, source, sum)
	// The daemon resolves this verified copy repeatedly. Index its own file
	// identity too, so an unchanged pin needs no executable scan while a
	// removed or modified copy still misses and is verified again.
	recordIndexedPin(cacheRoot, pinned, sum)
	return pinned, nil
}

// PublishCachedWorker publishes one immutable cache artifact from
// temporary, a file created in cacheRoot (Rust: tempfile::NamedTempFile).
// It takes ownership of temporary: the file is closed, and removed unless it
// became the artifact. The final publication is atomic and never replaces
// an artifact another daemon may have already captured.
func PublishCachedWorker(temporary *os.File, cacheRoot, digest string) (string, error) {
	name := temporary.Name()
	temporary.Close()
	// The temporary name never becomes the artifact: publication links it
	// under the artifact's name, as persist_noclobber does.
	defer os.Remove(name)
	if err := core.VerifyUnpublishedWorkerBuild(name); err != nil {
		return "", err
	}
	directory := pathJoin(cacheRoot, digest)
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return "", fmt.Errorf("create pinned worker cache %s: %w", directory, rustIOError(err))
	}
	destination := pathJoin(directory, "hel")
	if isFile(destination) {
		if err := verifyCachedWorker(destination, digest); err != nil {
			return "", err
		}
		return destination, nil
	}
	err := os.Link(name, destination)
	switch {
	case err == nil:
		if runtime.GOOS != "windows" {
			if err := syncDirectory(directory); err != nil {
				return "", fmt.Errorf("flush pinned worker cache %s: %w", directory, rustIOError(err))
			}
		}
		return destination, nil
	case errors.Is(err, os.ErrExist):
		if isFile(destination) {
			if err := verifyCachedWorker(destination, digest); err != nil {
				return "", err
			}
			return destination, nil
		}
	}
	return "", fmt.Errorf("publish pinned worker artifact %s: %w", destination, rustIOError(linkCause(err)))
}

// linkCause is the OS error of a failed link, as tempfile's persist error
// carries it.
func linkCause(err error) error {
	var linkErr *os.LinkError
	if errors.As(err, &linkErr) {
		return linkErr.Err
	}
	return err
}

// syncDirectory flushes a directory's entries to disk.
func syncDirectory(directory string) error {
	handle, err := os.Open(directory)
	if err != nil {
		return err
	}
	defer handle.Close()
	return handle.Sync()
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
	if snapshot := PinnedWorkerBinarySources.Get(); snapshot != nil {
		pinned, pinnedErr := snapshot.Resolve(arch, requirement)
		if PinnedSourceIsUsable(pinned, pinnedErr, isFile) {
			return pinned, nil
		}
		// Either nothing resolved when the daemon started, or the file the
		// pin named has been taken away since. Both used to fail every
		// session on this daemon until someone restarted it, which is
		// #1068: a build directory that a cache reaper removed took every
		// later session with it. Look again instead.
		return resolveWorkerSourceAgain(arch, requirement, pinnedErr)
	}
	current, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("resolve Mjolnir controller binary: %w", rustIOError(err))
	}
	return WorkerBinaryPrerequisiteForCurrent(arch, requirement, current, isFile)
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
	current, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("resolve Mjolnir controller binary: %w", rustIOError(err))
	}
	resolved, err := WorkerBinaryPrerequisiteForCurrent(arch, requirement, current, isFile)
	if err != nil {
		// Report what the daemon found at startup as well: it may name a
		// different, more useful absence than this attempt does.
		if pinnedError != nil {
			return nil, fmt.Errorf("%s: %w", pinnedError.Error(), err)
		}
		return nil, err
	}
	local, ok := resolved.(WorkerBinaryAvailabilityLocal)
	if !ok {
		return resolved, nil
	}
	cacheRoot := pathJoin(core.DataDir(), "workers", "pinned")
	path, err := CopyWorkerSourceToCache(local.Path, cacheRoot)
	if err != nil {
		return nil, fmt.Errorf("pin the re-resolved worker source: %w", err)
	}
	slog.Info("re-resolved a worker source the daemon could not pin at startup",
		"arch", arch, "requirement", string(requirement), "source", local.Source)
	return WorkerBinaryAvailabilityLocal{Path: path, Source: local.Source}, nil
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
	triple := requirement.Triple(arch)
	var rejected []string
	matchesBuild := func(path string) bool {
		if !isFile(path) {
			return false
		}
		if err := verify(path); err != nil {
			slog.Warn("skipping incompatible worker source", "path", path, "error", err.Error())
			rejected = append(rejected, err.Error())
			return false
		}
		return true
	}
	if path, ok := core.EnvOverrideOS("WORKER_BINARY"); ok {
		if !isFile(path) {
			return nil, fmt.Errorf("MJ_WORKER_BINARY is not a file: %s", path)
		}
		// An explicit override names the one worker to use. Installing a
		// different one instead would hide the mismatch.
		if err := verify(path); err != nil {
			return nil, fmt.Errorf("MJ_WORKER_BINARY does not match the running mj; rebuild it from the same commit or unset MJ_WORKER_BINARY: %w", err)
		}
		return WorkerBinaryAvailabilityLocal{Path: path, Source: "MJ_WORKER_BINARY"}, nil
	}
	// A rebuilt or renamed checkout leaves a running controller pointing at
	// a path that no longer holds a binary. Every lookup derived from that
	// path is meaningless, so remember the fact and skip those lookups.
	controllerReplaced := !isFile(current)
	if directory, ok := core.EnvOverrideOS("WORKER_DIR"); ok {
		candidates := []string{
			PackagedWorkerBinaryPath(directory, triple),
			pathJoin(directory, triple, "hel"),
		}
		if strings.HasSuffix(triple, "-apple-darwin") {
			candidates = append(candidates, PackagedWorkerBinaryPath(directory, "universal-apple-darwin"))
		}
		for _, path := range candidates {
			if matchesBuild(path) {
				return WorkerBinaryAvailabilityLocal{Path: path, Source: "MJ_WORKER_DIR"}, nil
			}
		}
	}
	if requirement == WorkerBinaryRequirementLocalHost {
		if path, source, ok := SelectNativeWorker(current, matchesBuild); ok {
			return WorkerBinaryAvailabilityLocal{Path: path, Source: source}, nil
		}
	}
	if !controllerReplaced {
		if path, source, ok := SelectSiblingWorker(current, triple, matchesBuild); ok {
			return WorkerBinaryAvailabilityLocal{Path: path, Source: source}, nil
		}
	}
	if template, ok := core.EnvOverride("WORKER_URL"); ok {
		expected, ok := core.EnvOverride("WORKER_SHA256")
		if !ok {
			return nil, errors.New("MJ_WORKER_URL requires MJ_WORKER_SHA256")
		}
		if err := ValidateWorkerSHA256(expected); err != nil {
			return nil, err
		}
		return WorkerBinaryAvailabilityRemote{
			URL:    strings.ReplaceAll(template, "{target}", triple),
			SHA256: expected,
			Triple: triple,
		}, nil
	}
	if len(rejected) != 0 {
		return nil, fmt.Errorf("no worker matching controller worker inputs %s for %s; rejected sources:\n%s\nInstall the worker from the same mj release, or rebuild it from the current workspace inputs with `cargo build --target %s -p brokk-mj-worker --bin mj-worker` and set MJ_WORKER_BINARY to that file (local native: `cargo build -p brokk-mj-worker --bin mj-worker`).",
			core.WorkerBuildID(), triple, strings.Join(rejected, "\n"), triple)
	}
	// Telling someone to install a worker beside a binary that is no longer
	// there sends them looking in the wrong place.
	if controllerReplaced {
		return nil, fmt.Errorf("the running mj binary was replaced or removed on disk (%s); restart the Mjolnir daemon so it runs the current build, then retry", DisplayPath(current))
	}
	return nil, fmt.Errorf("no worker for %s; install mj-worker-%s beside mj, set MJ_WORKER_DIR/MJ_WORKER_BINARY, or configure MJ_WORKER_URL and MJ_WORKER_SHA256", triple, triple)
}
