// Port of mj-targets/src/target_storage.rs.
//
// The one owner of "is this filesystem on a target full".
//
// The daemon's capacity service measures, on its usual cadence, each
// filesystem Mjolnir writes to on each host: worker roots, staged profile
// homes, project directories and managed clones, caches, container storage
// and `/tmp`, one record per filesystem. A target command that fails with
// "No space left on device" marks the filesystem of the path it names, or,
// when it names none, holds every filesystem on the host until the new
// measurement it asks for says which one is full. Everything else asks this
// board: a write checks the filesystem it lands on, recovery waits on the
// filesystem it needs, and the runtime feed, API and web viewer publish
// what it says.
//
// Hosts are named the way package core's storage helpers name them: `local`
// for the daemon's machine, otherwise the SSH host without its user.
//
// Name collisions: the private Board type is board beside the
// currentBoard function (Rust: board()).

package targets

import (
	"log/slog"
	"maps"
	"reflect"
	"slices"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// noSpaceHoldSeconds: a failed write keeps its filesystem full until a
// measurement taken this long after it says otherwise. A probe already in
// flight when the write failed may describe the disk from before the
// failure.
const noSpaceHoldSeconds uint64 = 15

// noSpaceFailure is a failed write: when it failed (epoch seconds) and what
// it said.
type noSpaceFailure struct {
	at     uint64
	detail string
}

type hostEntry struct {
	home        *string
	filesystems []core.FilesystemSpace
	sampledAt   *uint64
	// noSpace is failures attributed to one filesystem, by mount point.
	noSpace map[string]noSpaceFailure
	// unattributed is a failure that named no path this board could place.
	unattributed *noSpaceFailure
}

func (e *hostEntry) view(host string) core.TargetStorageView {
	var home *string
	if e.home != nil {
		copied := *e.home
		home = &copied
	}
	var sampledAt *uint64
	if e.sampledAt != nil {
		copied := *e.sampledAt
		sampledAt = &copied
	}
	var unattributed *string
	if e.unattributed != nil {
		detail := e.unattributed.detail
		unattributed = &detail
	}
	return core.TargetStorageViewEvaluate(host, home, e.filesystems, sampledAt,
		func(space *core.FilesystemSpace) *string {
			failure, ok := e.noSpace[space.Mount]
			if !ok {
				return nil
			}
			detail := failure.detail
			return &detail
		}, unattributed)
}

func (e *hostEntry) superseded(at uint64) bool {
	return e.sampledAt != nil && *e.sampledAt >= saturatingAdd(at, noSpaceHoldSeconds)
}

// saturatingAdd is Rust's u64::saturating_add.
func saturatingAdd(a, b uint64) uint64 {
	if sum := a + b; sum >= a {
		return sum
	}
	return ^uint64(0)
}

type board struct {
	hostsMu sync.Mutex
	hosts   map[string]*hostEntry
	// publication is guarded by publicationMu; nil until a reader asks.
	publicationMu sync.Mutex
	publication   *core.SharedSnapshot[[]core.TargetStorageView]
	refreshMu     sync.Mutex
	// refresh is the capacity service's wake-up channel, once attached.
	refresh chan<- struct{}
	// generation advances whenever a filesystem's verdict or free space
	// changes. generationMu makes each advance one read-modify-write (Rust:
	// watch::Sender::send_modify).
	generationMu sync.Mutex
	generation   *core.Watch[uint64]
}

func newBoard() *board {
	return &board{hosts: map[string]*hostEntry{}, generation: core.NewWatch[uint64](0)}
}

func (b *board) changed() {
	b.generationMu.Lock()
	defer b.generationMu.Unlock()
	b.generation.SendReplace(b.generation.Borrow() + 1)
}

func (b *board) requestRefresh() {
	b.refreshMu.Lock()
	defer b.refreshMu.Unlock()
	if b.refresh == nil {
		return
	}
	// A full channel already holds a refresh request.
	select {
	case b.refresh <- struct{}{}:
	default:
	}
}

var (
	boardsMu sync.Mutex
	// boards is one board per data directory, so isolated tests in one
	// process do not share hosts. A daemon has exactly one.
	boards = map[string]*board{}
)

// currentBoard is the board of this process's data directory (Rust:
// board()).
func currentBoard() *board {
	boardsMu.Lock()
	defer boardsMu.Unlock()
	key := core.DataDir()
	b, ok := boards[key]
	if !ok {
		b = newBoard()
		boards[key] = b
	}
	return b
}

func nowEpochSeconds() uint64 {
	seconds := time.Now().Unix()
	if seconds < 0 {
		return 0
	}
	return uint64(seconds)
}

// InstallNoSpaceObserver makes this process the owner that executors report
// full disks to.
func InstallNoSpaceObserver() {
	core.SetNoSpaceObserver(func(host, detail string) {
		ObserveNoSpace(host, detail)
	})
}

// AttachRefresh lets a no-space observation wake the capacity service for a
// new reading. Sends never block: a full channel already holds a request.
func AttachRefresh(refresh chan<- struct{}) {
	b := currentBoard()
	b.refreshMu.Lock()
	defer b.refreshMu.Unlock()
	b.refresh = refresh
}

// RecordSamples installs a measurement. It replaces each sampled host's
// filesystems, and supersedes the full-disk failures it was taken after.
func RecordSamples(samples []core.HostStorageSample, sampledAt uint64) {
	if len(samples) == 0 {
		return
	}
	b := currentBoard()
	changed := false
	func() {
		b.hostsMu.Lock()
		defer b.hostsMu.Unlock()
		for i := range samples {
			sample := &samples[i]
			entry := b.entry(sample.Host)
			before := entry.view(sample.Host)
			if sample.Home != nil {
				home := *sample.Home
				entry.home = &home
			}
			entry.filesystems = storageCloneFilesystems(sample.Filesystems)
			at := sampledAt
			entry.sampledAt = &at
			hold := func(at uint64) bool { return sampledAt < saturatingAdd(at, noSpaceHoldSeconds) }
			for mount, failure := range entry.noSpace {
				if !hold(failure.at) {
					delete(entry.noSpace, mount)
				}
			}
			if entry.unattributed != nil && !hold(entry.unattributed.at) {
				entry.unattributed = nil
			}
			after := entry.view(sample.Host)
			if !reflect.DeepEqual(before.Filesystems, after.Filesystems) ||
				!reflect.DeepEqual(before.UnattributedNoSpace, after.UnattributedNoSpace) {
				changed = true
			}
		}
		b.publicationMu.Lock()
		b.publication = nil
		b.publicationMu.Unlock()
	}()
	if changed {
		b.changed()
	}
}

// entry is host's entry, created empty when the board has not heard of it.
// The caller holds hostsMu.
func (b *board) entry(host string) *hostEntry {
	entry, ok := b.hosts[host]
	if !ok {
		entry = &hostEntry{noSpace: map[string]noSpaceFailure{}}
		b.hosts[host] = entry
	}
	return entry
}

// storageCloneFilesystems is an owned copy of a measurement's filesystems.
func storageCloneFilesystems(filesystems []core.FilesystemSpace) []core.FilesystemSpace {
	if filesystems == nil {
		return nil
	}
	cloned := make([]core.FilesystemSpace, len(filesystems))
	for i, space := range filesystems {
		space.Paths = slices.Clone(space.Paths)
		cloned[i] = space
	}
	return cloned
}

// ObserveNoSpace: a write on host failed because a disk is full. The
// filesystem of the path the failure names is marked; a failure that names
// no path holds the whole host until the measurement it asks for.
func ObserveNoSpace(host string, detail string) {
	path, ok := core.NoSpacePath(detail)
	var named *string
	if ok {
		named = &path
	}
	ObserveNoSpaceAt(host, named, detail, nowEpochSeconds())
}

// ObserveNoSpaceAt: a write to path on host failed for lack of space at at
// (epoch seconds). A measurement taken after that already says what the
// disk holds now, so an old failure, such as a dead worker's exit record,
// does not override it.
func ObserveNoSpaceAt(host string, path *string, detail string, at uint64) {
	var paths []string
	if path != nil {
		paths = []string{*path}
	}
	ObserveNoSpaceOn(host, paths, detail, at)
}

// ObserveNoSpaceOn: a write to one of paths on host failed for lack of space
// at at, and the failure does not say which: every filesystem they land on
// is held until a later measurement. When none of them is placed, the whole
// host is.
func ObserveNoSpaceOn(host string, paths []string, detail string, at uint64) {
	b := currentBoard()
	superseded, changed := func() (bool, bool) {
		b.hostsMu.Lock()
		defer b.hostsMu.Unlock()
		entry := b.entry(host)
		if entry.superseded(at) {
			return true, false
		}
		before := entry.view(host)
		mounts := map[string]struct{}{}
		for _, path := range paths {
			if filesystem := before.FilesystemFor(path); filesystem != nil {
				mounts[filesystem.Mount] = struct{}{}
			}
		}
		if len(mounts) == 0 {
			entry.unattributed = &noSpaceFailure{at: at, detail: detail}
		}
		for mount := range mounts {
			entry.noSpace[mount] = noSpaceFailure{at: at, detail: detail}
		}
		changed := !reflect.DeepEqual(before, entry.view(host))
		if changed {
			b.publicationMu.Lock()
			b.publication = nil
			b.publicationMu.Unlock()
		}
		return false, changed
	}()
	if superseded {
		slog.Debug("a later measurement supersedes this full-disk failure", "host", host, "detail", detail)
		return
	}
	slog.Warn("a write failed because the target's disk is full", "host", host, "paths", paths, "detail", detail)
	if changed {
		b.changed()
	}
	b.requestRefresh()
}

// View is what the board says about host, if it has heard anything.
func View(host string) (core.TargetStorageView, bool) {
	b := currentBoard()
	b.hostsMu.Lock()
	defer b.hostsMu.Unlock()
	entry, ok := b.hosts[host]
	if !ok {
		return core.TargetStorageView{}, false
	}
	return entry.view(host), true
}

// Views is every host the board knows, for publication.
func Views() []core.TargetStorageView {
	return ViewsSnapshot().IntoInner()
}

// ViewsSnapshot: readers share the board's publication until a measurement
// or disk failure changes it. Both replacement and reads hold the host lock.
func ViewsSnapshot() core.SharedSnapshot[[]core.TargetStorageView] {
	b := currentBoard()
	b.hostsMu.Lock()
	defer b.hostsMu.Unlock()
	b.publicationMu.Lock()
	defer b.publicationMu.Unlock()
	if b.publication == nil {
		// Rust's BTreeMap iterates hosts in order.
		hosts := slices.Sorted(maps.Keys(b.hosts))
		views := make([]core.TargetStorageView, 0, len(hosts))
		for _, host := range hosts {
			views = append(views, b.hosts[host].view(host))
		}
		publication := core.NewSharedSnapshot(views)
		b.publication = &publication
	}
	return *b.publication
}

// Subscribe is the board's generation, which changes whenever a
// filesystem's verdict or free space changes.
func Subscribe() *core.Watch[uint64] {
	return currentBoard().generation
}

// StorageHost is the host a target writes to.
func StorageHost(locator *core.TargetsTargetLocator) string {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare,
		core.TargetsTargetLocatorLocalPodman,
		core.TargetsTargetLocatorLocalDocker,
		core.TargetsTargetLocatorAppleContainer:
		return core.LocalStorageHost
	default:
		return core.StorageHostOfDestination(&locator.SSH.Destination)
	}
}

// hostPath is where on its host a write to path on locator lands. A path
// inside a container lands in the engine's storage; Apple's runtime keeps
// its storage in a VM image this board does not measure.
func hostPath(locator *core.TargetsTargetLocator, path string) (string, bool) {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare, core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		return path, true
	case core.TargetsTargetLocatorLocalPodman:
		return core.LocalHomePath(core.ContainerStoragePodman.Path()), true
	case core.TargetsTargetLocatorSshPodman:
		return core.ContainerStoragePodman.Path(), true
	case core.TargetsTargetLocatorLocalDocker, core.TargetsTargetLocatorSshDocker:
		return core.ContainerStorageDocker.Path(), true
	case core.TargetsTargetLocatorSshNitro:
		// A sandbox's disks are sparse files in the engine's state
		// directory, so a write inside the guest fills that filesystem on
		// the host.
		return core.ContainerStorageMicrovm.Path(), true
	default:
		// Apple's runtime keeps its storage in a VM disk image on whichever
		// host runs it, which this board does not measure.
		return "", false
	}
}

// EnsureRoom refuses a write of bytes (zero when the size is unknown) to
// path on locator that would leave its filesystem with less than the
// reserve. A filesystem the board has not measured is not refused: a write
// that then fails is still classified.
func EnsureRoom(locator *core.TargetsTargetLocator, path string, bytes uint64, what string) error {
	return EnsureRoomFor(locator, []string{path}, func() (uint64, error) { return bytes, nil }, what)
}

// EnsureRoomFor is EnsureRoom for a write that spans several directories,
// such as a harness install that fills its cache and the staged profile
// home. The size is measured only when a filesystem the write lands on is
// known, so an unmeasured target costs nothing.
func EnsureRoomFor(locator *core.TargetsTargetLocator, paths []string, bytes func() (uint64, error), what string) error {
	host := StorageHost(locator)
	view, ok := View(host)
	if !ok {
		return nil
	}
	var landing []string
	for _, path := range paths {
		onHost, ok := hostPath(locator, path)
		if !ok {
			continue
		}
		if view.FilesystemFor(onHost) != nil || view.UnattributedNoSpace != nil {
			landing = append(landing, onHost)
		}
	}
	if len(landing) == 0 {
		return nil
	}
	size, err := bytes()
	if err != nil {
		return err
	}
	for _, path := range landing {
		if refusal, refused := view.RefuseWrite(path, size, what); refused {
			slog.Warn("refused a write to a full filesystem: "+refusal, "host", host, "path", path, "bytes", size, "what", what)
			return core.RefusalPrecondition(refusal).WithCode(StorageFullCode)
		}
	}
	return nil
}

// HarnessCachePath is where the worker installs managed harnesses on a
// target: its XDG cache.
func HarnessCachePath(locator *core.TargetsTargetLocator) string {
	const harnessCache = ".cache/mjolnir/harnesses"
	if locator.Kind == core.TargetsTargetLocatorLocalBare {
		return core.LocalHomePath(harnessCache)
	}
	return harnessCache
}

// StorageFullCode is the refusal code carried by EnsureRoom refusals.
const StorageFullCode = "target_storage_full"

// IsStorageFull reports whether err is a write refused or failed because a
// disk is full.
func IsStorageFull(err error) bool {
	if refusal, ok := core.RefusalOf(err); ok {
		if code, ok := refusal.Code(); ok && code == StorageFullCode {
			return true
		}
	}
	return core.ReportsNoSpace(err.Error())
}

// FullProblem is the sentence for a full filesystem one of paths on host
// lands on.
func FullProblem(host string, paths []string) (string, bool) {
	view, ok := View(host)
	if !ok {
		return "", false
	}
	return view.ProblemFor(paths)
}

// WorkerRestartProblem is the host and sentence while any filesystem a
// session's worker and harness write to is full: its worker root (journal,
// staged worker), workspace, staged profile home or `/tmp`. Recovery waits
// on exactly this, because a restart needs room on all of them.
func WorkerRestartProblem(locator core.TargetLocator, sessionID string, workspace *string) (host string, problem string, ok bool) {
	paths := core.SessionStoragePathsOf(locator, sessionID, workspace)
	problem, ok = FullProblem(paths.Host, paths.All())
	if !ok {
		return "", "", false
	}
	return paths.Host, problem, true
}

// SessionProblemIn is the sentence a session shows instead of "unreachable"
// while one of its own filesystems is full. Use the same captured board as
// the host rows.
func SessionProblemIn(storage []core.TargetStorageView, session *core.SessionRecord) (string, bool) {
	if session.Target == nil {
		return "", false
	}
	var projectDirectory *string
	if directory, ok := session.RecordCheckout().ProjectDirectory(); ok {
		projectDirectory = &directory
	}
	paths := core.SessionStoragePathsOf(session.Target, session.ID, projectDirectory)
	for i := range storage {
		if storage[i].Host == paths.Host {
			return storage[i].ProblemFor(paths.All())
		}
	}
	return "", false
}

// TestStorageSample is one host with `/` holding the worker roots and
// `~/Projects` on a filesystem of its own, as on precision-3260.
func TestStorageSample(host string, rootFree uint64, projectsFree uint64) core.HostStorageSample {
	home := "/home/jonathan"
	return core.HostStorageSample{
		Host: host,
		Home: &home,
		Filesystems: []core.FilesystemSpace{
			{
				Mount:          "/",
				AvailableBytes: rootFree,
				TotalBytes:     500 << 30,
				ReservedBytes:  25 << 30,
				Paths: []string{
					"/home/jonathan/.local/share/hel/workers",
					"/home/jonathan/.local/share/hel/profiles",
					"/home/jonathan/.cache/mjolnir",
					"/tmp",
				},
				QuotaLimited: false,
			},
			{
				Mount:          "/home/jonathan/Projects",
				AvailableBytes: projectsFree,
				TotalBytes:     1000 << 30,
				ReservedBytes:  0,
				Paths:          []string{"/home/jonathan/Projects"},
				QuotaLimited:   false,
			},
		},
	}
}
