// Port of mj-worker-staging/src/worker_binary/store.rs.

// Immutable target binaries. Session installation only creates new directory
// entries; it never writes executable bytes through an existing inode.

package workerstaging

import (
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
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
	command := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"true"})
	args := make([]string, len(command.Args))
	for index, arg := range command.Args {
		args[index] = core.RustDebugString(arg)
	}
	return fmt.Sprintf("%s:%s:[%s]:%s", core.RustDebugString(core.DataDir()), command.Program,
		strings.Join(args, ", "), root)
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
	key := StoreKey(locator, root)
	storeGates.mu.Lock()
	defer storeGates.mu.Unlock()
	if storeGates.gates == nil {
		storeGates.gates = map[string]weak.Pointer[StoreGate]{}
	}
	for name, gate := range storeGates.gates {
		if gate.Value() == nil {
			delete(storeGates.gates, name)
		}
	}
	if existing, ok := storeGates.gates[key]; ok {
		if gate := existing.Value(); gate != nil {
			return gate
		}
	}
	gate := &StoreGate{label: gateTarget(locator) + " " + root}
	storeGates.gates[key] = weak.Make(gate)
	return gate
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
	// Queuing is not work performed by this operation. Preserve its command
	// budget while continuing to observe explicit supervisor cancellation.
	untimed := core.NewWithoutDeadline(executor)
	defer untimed.End()
	var wait *core.LockWait
	var queued *core.ProvisionStageGuard
	defer func() {
		if wait != nil {
			wait.Drop()
			queued.End()
		}
	}()
	for {
		if guard := gate.TryLock(sessionID, purpose); guard != nil {
			if wait != nil {
				wait.Acquired()
			}
			return guard, nil
		}
		if untimed.CancellationRequested() {
			command := core.NewCommandSpec("true", nil).
				WithPurpose(fmt.Sprintf("wait to %s in %s", purpose, gate.label))
			if _, err := untimed.Execute(&command); err != nil {
				return nil, err
			}
			return nil, fmt.Errorf("shared target content wait cancelled: %s (%s)", gate.label, purpose)
		}
		if wait == nil {
			wait = core.NewLockWait("the shared target content store")
			queued = core.NewProvisionStageGuard(untimed, core.ProvisionStage{Kind: core.ProvisionStageWaitingForWorkerStore})
		}
		wait.Blocked(gate.label, fmt.Sprintf("%s (%s)", sessionID, purpose), gate.describeHolder)
		time.Sleep(25 * time.Millisecond)
	}
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
	root, err := StoreRoot(locator, sessionID)
	if err != nil {
		return err
	}
	store := core.Text(root)
	// Register before any pin can be created. Registration and sweeping share
	// one session owner, so no sweep can use a snapshot predating a live pin.
	id, err := core.NewCommandID("link")
	if err != nil {
		return err
	}
	name := "hel.store-" + id
	if sessionRoot != nil {
		live, err := LiveStagingRegisterAndSweep(executor, locator, sessionID, *sessionRoot, name)
		if err != nil {
			return err
		}
		defer live.Release()
	}
	temporary := destination + "." + name
	if sessionRoot != nil {
		temporary = *sessionRoot + "/" + name
	}
	if source == nil {
		plan, err := binaryUploadPlan(locator, sessionID, nil, destination, nil)
		if err != nil {
			return err
		}
		if _, err := plan.Execute(executor); err != nil {
			return err
		}
		// Switching to an image link can release the last session reference
		// to a host-copied build. Image installs must collect that store too.
		collectIfDue(executor, locator, sessionID, store, nil)
		return nil
	}
	digest, err := core.WorkerExecutableDigest(*source)
	if err != nil {
		return err
	}
	directory := store + "/" + digest
	cached := directory + "/hel"
	// A published build needs no store lock: the link script verifies the
	// digest of the private pin it creates. Only a missing or corrupt entry
	// takes the upload lock below. A collector in another daemon can retire
	// an entry before the link, so that explicitly unaccepted installation
	// retries with fresh bytes.
	command := func(mode string) core.CommandSpec {
		return script(core.SshTrafficClassBulk, locator, linkSharedWorkerScript,
			[]string{cached, temporary, destination, digest, mode}, "link shared Mjolnir worker into session")
	}
	publish := false
	for range 3 {
		if publish {
			if err := ensurePublished(executor, locator, sessionID, *source, store, digest); err != nil {
				return err
			}
		}
		link := command("link")
		linked, err := executor.Execute(&link)
		if err != nil {
			return err
		}
		if linked.Status == linkNoHardLinks {
			if err := targets.EnsureRoomFor(locator, []string{temporary},
				func() (uint64, error) { return FileSize(*source) },
				"copy the replacement Mjolnir worker"); err != nil {
				return err
			}
			copied := command("copy")
			if linked, err = executor.Execute(&copied); err != nil {
				return err
			}
		}
		switch linked.Status {
		case 0:
			// After the pin exists, so this install's own entry is linked.
			collectIfDue(executor, locator, sessionID, store, &digest)
			return nil
		case linkMissing, linkDigestMismatch:
			publish = true
		default:
			return fmt.Errorf("install shared Mjolnir worker: %s", lossyText(linked.Stderr))
		}
	}
	return errors.New("shared Mjolnir worker repeatedly retired during installation")
}

// linkSharedWorkerScript links (or, in `copy` mode, copies) the published
// build into a private pin, verifies its digest and promotes it.
const linkSharedWorkerScript = `set -eu
source=$1; temporary=$2; destination=$3
trap 'rm -f -- "$temporary"' EXIT
if [ "$5" = link ]; then
    if ! ln -- "$source" "$temporary" 2>/dev/null; then
        [ -f "$source" ] || exit 75
        # A concurrent publisher can create $source after the first ln fails.
        # Retry once now that $source exists so we only report no hard links
        # when ln fails against a present source.
        if ! ln -- "$source" "$temporary" 2>/dev/null; then
            [ -f "$source" ] || exit 75
            exit 76
        fi
    fi
else
    if ! cp -- "$source" "$temporary"; then
        # A collector in another controller can retire an unpinned source
        # after the failed link. Only that missing-source failure is retryable.
        [ -f "$source" ] || exit 75
        exit 1
    fi
fi
# The private link pins the inode even if another daemon collects the entry.
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$temporary");; *) actual=$(sha256sum -- "$temporary");; esac
[ "${actual%% *}" = "$4" ] || { echo 'shared worker digest mismatch' >&2; exit 77; }
promote_worker_binary "$temporary" "$destination"
# This build serves every control subcommand, so a staged control worker has
# nothing left to do; its link would only pin a store entry.
case "$destination" in */hel) rm -f -- "${destination%/*}"/hel-control* ;; esac
`

// verifyUploadedWorkerScript checks an upload's digest before publication.
const verifyUploadedWorkerScript = `set -eu
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$1");; *) actual=$(sha256sum -- "$1");; esac
[ "${actual%% *}" = "$2" ] || { echo 'uploaded Mjolnir worker digest mismatch' >&2; exit 1; }
`

// publishSharedWorkerScript publishes a verified upload into the store.
const publishSharedWorkerScript = `set -eu
mkdir -p -- "$2"
# Copies such as docker cp keep the source build's age; collection must see a
# fresh entry.
touch -- "$1"
# Atomic creation converges independent uploaders on the winning inode.
if [ ! -e "$2/hel" ]; then
    if ! ln -- "$1" "$2/hel" 2>/dev/null; then
        [ -f "$2/hel" ] || mv -n -- "$1" "$2/hel"
    fi
fi
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$2/hel");; *) actual=$(sha256sum -- "$2/hel");; esac
if [ "${actual%% *}" != "$3" ]; then
    # A corrupt entry is replaced, never written through its existing inode.
    mv -f -- "$1" "$2/hel"
fi
`

// ensurePublished makes `{store}/{digest}/hel` exist with the right bytes.
// The lock is per build: the first holder uploads, verifies and publishes,
// and every later holder finds it published after acquiring and returns
// without a transfer. Builds that are already published never reach this
// function. Publication then owns the whole store, shared with collection,
// so a collector cannot retire the digest directory between its creation
// and the publishing link. Uploads are rare, so different builds waiting
// for each other here is cheap.
func ensurePublished(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, source, store, digest string) error {
	directory := store + "/" + digest
	gate := StoreGateFor(locator, directory)
	guard, err := LockGate(executor, gate, sessionID, "publish a worker build")
	if err != nil {
		return err
	}
	defer guard.Release()
	verify := InstalledFileDigestCommand(locator, directory+"/hel", "verify shared Mjolnir worker")
	if targets.InstalledDigestMatches(executor, &verify, digest) {
		return nil
	}
	storeOwner := StoreGateFor(locator, store)
	storeGuard, err := LockGate(executor, storeOwner, sessionID, "upload and collect worker builds")
	if err != nil {
		return err
	}
	defer storeGuard.Release()
	// Reclaim old builds before the capacity check below can refuse the
	// upload, even when this interval's collection already ran.
	claimCollection(locator, store)
	collectReporting(executor, locator, store, &digest)
	id, err := core.NewCommandID("worker-store")
	if err != nil {
		return err
	}
	upload := store + "/.upload-" + id
	temporary := upload + "/hel"
	if err := targets.EnsureRoomFor(locator, []string{upload},
		func() (uint64, error) { return FileSize(source) },
		"stage the replacement Mjolnir worker"); err != nil {
		return err
	}
	batchCompleted := false
	transfer := func() error {
		if _, err := targets.ExecuteChecked(executor,
			core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"mkdir", "-p", upload}).
				WithPurpose("create private worker store upload")); err != nil {
			return err
		}
		if err := transferBinary(executor, locator, sessionID, source, temporary); err != nil {
			return err
		}
		steps := []core.CommandStep{
			core.NewCommandStep("verify uploaded Mjolnir worker",
				scriptArgv(verifyUploadedWorkerScript, []string{temporary, digest})),
			core.NewCommandStep("publish shared Mjolnir worker",
				scriptArgv(publishSharedWorkerScript, []string{temporary, directory, digest})),
			core.NewCommandStep("discard private worker store upload",
				[]string{"rm", "-rf", "--", upload}).BestEffortCleanup(),
		}
		command := core.LocatorCommand(core.SshTrafficClassBulk, locator, core.StepScript(steps)).
			WithPurpose("publish shared Mjolnir worker")
		// A failed or disconnected batch may not have reached its EXIT trap.
		// Retry its idempotent discard through the executor's cleanup path.
		output, err := executor.Execute(&command)
		if err != nil {
			return err
		}
		batchCompleted = output.Status == 0
		for _, warning := range core.StepScriptWarnings(&output) {
			slog.Warn(warning)
		}
		_, err = core.CheckedCommandOutput(&command, output)
		return err
	}()
	if !batchCompleted {
		cleanup := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"rm", "-rf", "--", upload}).
			WithPurpose("discard private worker store upload")
		output, err := executor.ExecuteCleanup(&cleanup)
		if err != nil || output.Status != 0 {
			slog.Warn("worker store upload cleanup failed", "result", cleanupResult(output, err))
		}
	}
	return transfer
}

// cleanupResult renders a cleanup's outcome for a log line, as Rust's
// `{:?}` of the `Result<CommandOutput>` shows it.
func cleanupResult(output core.CommandOutput, err error) string {
	if err != nil {
		return fmt.Sprintf("Err(%s)", err)
	}
	return fmt.Sprintf("Ok(CommandOutput { status: %d, stdout: %v, stderr: %v })", output.Status, output.Stdout, output.Stderr)
}

// transferBinary: SSH containers reuse the host store as their transport
// cache, then publish into their own filesystem. The private host link pins
// bytes during engine cp.
func transferBinary(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, source, destination string) error {
	switch locator.Kind {
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		// This path already belongs to the private store upload. Stream the
		// worker without the deadline reserved for the surrounding commands.
		untimed := core.NewWithoutDeadline(executor)
		err := UploadWorkerFile(untimed, locator.SSH, source, destination, "stage replacement Mjolnir worker")
		untimed.End()
		if err != nil {
			return err
		}
		_, err = targets.ExecuteChecked(executor,
			core.SSHCommand(core.SshTrafficClassControl, locator.SSH, []string{"chmod", "700", destination}).
				WithPurpose("make replaced Mjolnir worker executable"))
		return err
	case core.TargetsTargetLocatorSshPodman, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer, core.TargetsTargetLocatorSshNitro:
	default:
		plan, err := binaryUploadPlan(locator, sessionID, &source, destination, nil)
		if err != nil {
			return err
		}
		_, err = plan.Execute(executor)
		return err
	}
	ssh := *locator.SSH
	ssh.SSHArgs = slices.Clone(ssh.SSHArgs)
	host := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &ssh,
		Workspace: ".local/share/hel/workspaces/" + sessionID,
	}
	hostStore, err := StoreRoot(&host, sessionID)
	if err != nil {
		return err
	}
	id, err := core.NewCommandID("container-upload")
	if err != nil {
		return err
	}
	transport := core.Text(core.Join(hostStore, ".transport-"+id))
	pin := core.Text(core.Join(transport, "hel"))
	result := func() error {
		if _, err := targets.ExecuteChecked(executor,
			core.LocatorCommand(core.SshTrafficClassControl, &host, []string{"mkdir", "-p", transport}).
				WithPurpose("create private host worker transport")); err != nil {
			return err
		}
		if err := installBinaryAt(executor, &host, sessionID, &source, pin, nil); err != nil {
			return err
		}
		plan, err := binaryUploadPlan(locator, sessionID, &source, destination, &pin)
		if err != nil {
			return err
		}
		_, err = plan.Execute(executor)
		return err
	}()
	cleanup := core.LocatorCommand(core.SshTrafficClassControl, &host, []string{"rm", "-rf", "--", transport}).
		WithPurpose("discard host worker transport pin")
	output, cleanupErr := executor.ExecuteCleanup(&cleanup)
	if cleanupErr != nil || output.Status != 0 {
		slog.Warn("host worker transport pin cleanup failed", "result", cleanupResult(output, cleanupErr))
	}
	return result
}
