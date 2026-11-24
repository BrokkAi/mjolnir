// Port of mj-worker-staging/src/worker_binary/store.rs.

// Immutable target binaries. Session installation only creates new directory
// entries; it never writes executable bytes through an existing inode.

package workerstaging

import (
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"time"
	"weak"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// StoreRoot is beside the session roots, in their target filesystem (inside
// containers).
func StoreRoot(locator *core.TargetsTargetLocator, sessionID string) (string, error) {
	root, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return "", err
	}
	store, ok := storeDirectory(root)
	if !ok {
		return "", errors.New("worker root has no parent")
	}
	return store, nil
}

// storeDirectory is the worker store beside a worker root; the only place
// that interprets where a store is. False when the root has no parent.
func storeDirectory(workerRoot string) (string, bool) {
	parent, ok := pathParent(workerRoot)
	if !ok {
		return "", false
	}
	return core.Join(parent, "worker-binaries"), true
}

// StoreKey is the key of one target store: this daemon's data directory,
// the target's command prefix and the store path.
func StoreKey(locator *core.TargetsTargetLocator, root string) string {
	panic("unported: mj-worker-staging/src/worker_binary/store.rs store_key")
}

// storeGates is Rust's GATES: one gate per store key, held weakly so a
// gate nobody holds is forgotten.
var storeGates struct {
	mu    sync.Mutex
	gates map[string]weak.Pointer[StoreGate]
}

// StoreGateFor is Rust's `store_gate` (renamed: Go cannot have a function
// and a type both named StoreGate).
//
// One owner per `root` string in this daemon. Callers choose the string. An
// upload takes `{store}/{digest}` so one build is transferred once, then the
// store itself, which publication and collection share so a collector never
// retires a directory an upload is publishing into. A session's staging
// registry takes its own root. Cached installs of a published build take
// none of these. Other daemons targeting the same host are protected by the
// upload's unique name and its atomic publication. Waiting observes the
// executor cancellation.
func StoreGateFor(locator *core.TargetsTargetLocator, root string) *StoreGate {
	panic("unported: mj-worker-staging/src/worker_binary/store.rs store_gate")
}

// gateTarget is where a gate's store lives, for the log line of a long
// wait.
func gateTarget(locator *core.TargetsTargetLocator) string {
	switch locator.Kind {
	case core.TargetsTargetLocatorSshPodman, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer, core.TargetsTargetLocatorSshNitro:
		return fmt.Sprintf("container %s on %s", locator.ContainerID, locator.SSH.Destination)
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorLocalDocker,
		core.TargetsTargetLocatorAppleContainer:
		return "local container " + locator.ContainerID
	case core.TargetsTargetLocatorSshBare, core.TargetsTargetLocatorAwsEc2:
		return "host " + locator.SSH.Destination
	default:
		return "this host"
	}
}

// StoreGate is one owner for a store root in this daemon. The record of who
// holds it is the lock itself: taking the gate sets it and releasing the
// guard on any path clears it, so a waiter reads the owner from the same
// place that decides ownership.
type StoreGate struct {
	label  string
	mu     sync.Mutex
	holder *gateHolder
}

type gateHolder struct {
	sessionID string
	purpose   string
	since     time.Time
}

// GateGuard is ownership of a StoreGate; Release gives it up (Rust: Drop).
type GateGuard struct {
	gate *StoreGate
}

// TryLock takes the gate if it is free, recording sessionID and purpose.
// Nil when another owner holds it.
func (g *StoreGate) TryLock(sessionID, purpose string) *GateGuard {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.holder != nil {
		return nil
	}
	g.holder = &gateHolder{sessionID: sessionID, purpose: purpose, since: time.Now()}
	return &GateGuard{gate: g}
}

func (g *StoreGate) describeHolder() string {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.holder == nil {
		return "released just now"
	}
	return fmt.Sprintf("%s (%s) held for %ds", g.holder.sessionID, g.holder.purpose, int64(time.Since(g.holder.since)/time.Second))
}

// Release gives the gate up. Releasing a guard twice does nothing.
func (g *GateGuard) Release() {
	if g == nil || g.gate == nil {
		return
	}
	g.gate.mu.Lock()
	g.gate.holder = nil
	g.gate.mu.Unlock()
	g.gate = nil
}

// LockGate waits for the gate, logging the holder once the wait passes five
// seconds. Waiting observes the executor's cancellation. A wait that has to
// queue is reported as its own stage, so a lifecycle watching for stalls
// does not count the queue as time with no progress.
func LockGate(executor core.CommandExecutor, gate *StoreGate, sessionID, purpose string) (*GateGuard, error) {
	panic("unported: mj-worker-staging/src/worker_binary/store.rs lock_gate")
}

func script(class core.SshTrafficClass, locator *core.TargetsTargetLocator, body string, args []string, purpose string) core.CommandSpec {
	return core.LocatorCommand(class, locator, scriptArgv(body, args)).WithPurpose(purpose)
}

func scriptArgv(body string, args []string) []string {
	argv := []string{"sh", "-c", PromoteWorkerBinary + "\n" + body, "mj-worker-store"}
	return append(argv, args...)
}

// collectScript is Rust's COLLECT.
//
// Rename an old entry out of the published namespace before deciding
// whether it is unreferenced. A concurrent installer either pins it first,
// or sees a missing path and retries. Never remove a directory that an
// uploader owns.
const collectScript = `set -eu
store=$1; keep=${2-}
[ -d "$store" ] || exit 0
# Timed-out transfers may not be able to run their cleanup command. All live
# transfers in this daemon share the owner held by collection.
for upload in "$store"/.upload-* "$store"/.transport-*; do
    [ -d "$upload" ] || continue
    [ -n "$(find "$upload" -prune -mmin +1440 -print)" ] || continue
    rm -rf -- "$upload"
done
for retired in "$store"/.gc-*; do
    [ -f "$retired/hel" ] || continue
    [ -n "$(find "$retired/hel" -type f -mmin +1440 -links 1 -print)" ] || continue
    rm -f -- "$retired/hel"
    rmdir -- "$retired"
done
for file in "$store"/*/hel; do
    [ -f "$file" ] || continue
    [ -n "$(find "$file" -type f -mmin +1440 -links 1 -print)" ] || continue
    dir=${file%/hel}; name=${dir##*/}
    case $name in *[!0123456789abcdef]*|'') continue;; esac
    [ ${#name} = 64 ] || continue
    [ "$name" != "$keep" ] || continue
    retired="$store/.gc-$name-$$"
    mv -- "$dir" "$retired" 2>/dev/null || continue
    if [ -n "$(find "$retired/hel" -type f -links 1 -print)" ]; then
        rm -f -- "$retired/hel"
    else
        mkdir -p -- "$dir"
        if [ ! -e "$dir/hel" ]; then
            ln -- "$retired/hel" "$dir/hel" || exit 1
        fi
        rm -f -- "$retired/hel"
    fi
    rmdir -- "$retired"
done`

// collect retires unused entries of the store at root, keeping keep (nil
// keeps none beyond the referenced ones).
func collect(executor core.CommandExecutor, locator *core.TargetsTargetLocator, root string, keep *string) error {
	keepArg := ""
	if keep != nil {
		keepArg = *keep
	}
	_, err := targets.ExecuteChecked(executor, script(core.SshTrafficClassControl, locator, collectScript,
		[]string{root, keepArg}, "collect unused Mjolnir worker binaries"))
	return err
}

// collectInterval: collection only retires entries older than a day, so one
// run per store and interval is enough; running it for every install queued
// hours of commands.
const collectInterval = 60 * 60 * time.Second

// claimCollection claims this daemon's collection for store; at most one
// caller per interval wins and runs it inline, so the run has exactly one
// owner.
//
// Why this cannot starve this daemon's own installs: one run retires an
// entry at most once, and a republished entry has a fresh modification time
// (the publish script touches each upload), so no later run within the day
// retires it again. A run therefore forces at most one retry of an install,
// and the install loop allows three. Entries pinned by a link are restored,
// never removed.
func claimCollection(locator *core.TargetsTargetLocator, store string) bool {
	key := StoreKey(locator, store)
	collections.mu.Lock()
	defer collections.mu.Unlock()
	if last, ok := collections.last[key]; ok && time.Since(last) < collectInterval {
		return false
	}
	if collections.last == nil {
		collections.last = map[string]time.Time{}
	}
	collections.last[key] = time.Now()
	return true
}

// collections is Rust's `collections()` static: when each store was last
// claimed for collection.
var collections struct {
	mu   sync.Mutex
	last map[string]time.Time
}

// collectIfDue collects after a cached install, if this interval's run is
// still unclaimed. A cached install never waits for the store: while a
// publication owns it, the publication collects itself, so this skips
// without claiming. A failed run is reported and not retried before the
// next interval; an upload that needs space collects regardless, and the
// next daemon starts unclaimed.
func collectIfDue(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, store string, keep *string) {
	if executor.CancellationRequested() {
		return
	}
	gate := StoreGateFor(locator, store)
	guard := gate.TryLock(sessionID, "collect unused worker binaries")
	if guard == nil {
		return
	}
	defer guard.Release()
	if claimCollection(locator, store) {
		collectReporting(executor, locator, store, keep)
	}
}

func collectReporting(executor core.CommandExecutor, locator *core.TargetsTargetLocator, store string, keep *string) {
	if err := collect(executor, locator, store, keep); err != nil {
		slog.Warn("worker binary store collection failed", "store", store, "error", err.Error())
	}
}

func InstallWorkerBinary(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall, name string) error {
	root, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return err
	}
	destination := core.Text(core.Join(root, name))
	return InstallWorkerBinaryAt(executor, locator, sessionID, worker, destination, &root)
}

// Exit statuses of the link script that mean "this entry cannot serve": the
// path is gone (or retired by a collector), or its bytes do not match.
const (
	linkMissing        = 75
	linkNoHardLinks    = 76
	linkDigestMismatch = 77
)

// InstallWorkerBinaryAt installs worker at destination. sessionRoot is the
// session's worker root, nil for an install outside one.
func InstallWorkerBinaryAt(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall, destination string, sessionRoot *string) error {
	var source *string
	if path, ok := worker.HostBinary(); ok {
		if err := core.VerifyWorkerBuild(path); err != nil {
			return err
		}
		source = &path
	}
	return installBinaryAt(executor, locator, sessionID, source, destination, sessionRoot)
}

// InstallSessionExecutable installs a caller-verified executable through
// the same immutable stores and transport used by the worker. The store
// verifies the bytes at each hop.
func InstallSessionExecutable(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, source string, name string) (string, error) {
	root, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return "", err
	}
	destination := core.Join(root, name)
	if err := installBinaryAt(executor, locator, sessionID, &source, core.Text(destination), &root); err != nil {
		return "", err
	}
	return destination, nil
}

// installBinaryAt installs source (nil: the container image's worker) at
// destination. sessionRoot is the session's worker root, nil for an install
// outside one.
func installBinaryAt(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, source *string, destination string, sessionRoot *string) error {
	panic("unported: mj-worker-staging/src/worker_binary/store.rs install_binary_at")
}

// ensurePublished makes `{store}/{digest}/hel` exist with the right bytes.
// The lock is per build: the first holder uploads, verifies and publishes,
// and every later holder finds it published after acquiring and returns
// without a transfer. Builds that are already published never reach this
// function. Publication then owns the whole store, shared with collection,
// so a collector cannot retire the digest directory between its creation
// and the publishing link. Uploads are rare, so different builds waiting
// for each other here is cheap.
func ensurePublished(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, source, store, digest string) error {
	panic("unported: mj-worker-staging/src/worker_binary/store.rs ensure_published")
}

// transferBinary: SSH containers reuse the host store as their transport
// cache, then publish into their own filesystem. The private host link pins
// bytes during engine cp.
func transferBinary(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, source, destination string) error {
	panic("unported: mj-worker-staging/src/worker_binary/store.rs transfer_binary")
}
