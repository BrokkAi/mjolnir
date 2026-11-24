// Port of mj-worker-staging/src/worker_binary/upgrade.rs.

package workerstaging

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"slices"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessiontypes"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// ReplaceInstalledWorkerBinary replaces `{worker_root}/hel` with the worker
// this controller would install: a link to the container image's own worker
// when it is this build, else a copy of the host binary.
//
// Checkpoint export starts that path as a new process. A live daemon
// already has the previous inode mapped, so this does not restart it.
// Writing through `hel.next` and renaming avoids `ETXTBSY` on a running
// image.
//
// ctx carries the session's worker permit (Rust: worker_lifecycle's
// task-local).
func ReplaceInstalledWorkerBinary(ctx context.Context, executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall) error {
	if _, err := store.Require(ctx, sessionID); err != nil {
		return err
	}
	return InstallWorkerBinary(executor, locator, sessionID, worker, "hel")
}

// PreparedWorkerBinary is an upload that does not change the executable
// path used by the running worker or its sidecars. Promotion happens only
// while an idle reservation is held. A successfully transferred private
// binary has no live process using it. Deferred admission and failed swaps
// discard it without touching live files: Close discards it (Rust: Drop).
type PreparedWorkerBinary struct {
	name      string
	cleanup   core.CommandSpec
	executor  core.CommandExecutor
	locator   core.TargetsTargetLocator
	sessionID string
	live      *LiveStaging
}

// LiveStaging is one private staging name this daemon is writing or
// holding for a session.
//
// Recovery stages under the worker permit and an upgrade stages before
// taking it, so two preparations for one session can overlap. The registry
// behind this type is the one owner of "which staged files are still in
// use": a new staging removes every other private staging file in the
// worker root, which can only be a leftover from an attempt that failed or
// a daemon that stopped mid-upload. Release unregisters it (Rust: Drop).
type LiveStaging struct {
	key  liveStagingKey
	name string
}

// liveStagingKey is Rust's (data directory, session id) registry key.
type liveStagingKey struct {
	dataDir   string
	sessionID string
}

// liveStagings is Rust's `live_stagings()` registry: the live names per
// session.
var liveStagings struct {
	mu    sync.Mutex
	names map[liveStagingKey]map[string]struct{}
}

// LiveStagingRegisterAndSweep is Rust's `LiveStaging::register_and_sweep`:
// register name and remove every other private staging file in root.
func LiveStagingRegisterAndSweep(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, root, name string) (*LiveStaging, error) {
	// A snapshot must remain current until its sweep finishes. Other
	// registrations wait here before creating their first directory entry.
	gate := StoreGateFor(locator, root+"/staging")
	owner, err := LockGate(executor, gate, sessionID, "register and sweep private worker staging")
	if err != nil {
		return nil, err
	}
	defer owner.Release()
	live, names := liveStagingRegister(sessionID, name)
	if _, err := targets.ExecuteChecked(executor, sweepStaleStagingsCommand(locator, root, names)); err != nil {
		live.Release()
		return nil, err
	}
	return live, nil
}

// liveStagingRegister registers name and returns every name live for the
// session, this one included, in order, from the same locked view.
func liveStagingRegister(sessionID, name string) (*LiveStaging, []string) {
	key := liveStagingKey{dataDir: core.DataDir(), sessionID: sessionID}
	liveStagings.mu.Lock()
	defer liveStagings.mu.Unlock()
	if liveStagings.names == nil {
		liveStagings.names = map[liveStagingKey]map[string]struct{}{}
	}
	set := liveStagings.names[key]
	if set == nil {
		set = map[string]struct{}{}
		liveStagings.names[key] = set
	}
	set[name] = struct{}{}
	names := make([]string, 0, len(set))
	for live := range set {
		names = append(names, live)
	}
	slices.Sort(names)
	return &LiveStaging{key: key, name: name}, names
}

// Release unregisters the staging name. Releasing twice does nothing.
func (l *LiveStaging) Release() {
	if l == nil {
		return
	}
	liveStagings.mu.Lock()
	defer liveStagings.mu.Unlock()
	if names, ok := liveStagings.names[l.key]; ok {
		delete(names, l.name)
		if len(names) == 0 {
			delete(liveStagings.names, l.key)
		}
	}
}

// staleStagingSweepScript is the script of sweepStaleStagingsCommand.
const staleStagingSweepScript = `root=$1; shift
for path in "$root"/hel.prepared-* "$root"/hel.store-*; do
    [ -e "$path" ] || continue
    name=${path##*/}
    keep=0
    for live in "$@"; do
        case $name in "$live"|"$live".next|"$live".store-*) keep=1 ;; esac
    done
    [ "$keep" = 1 ] || rm -f -- "$path"
done`

// sweepStaleStagingsCommand removes prepared binaries and store pins in
// workerRoot that no live staging owns. A failed upload once left its
// partial `.next` file behind, and on a full disk those accumulated by the
// dozen.
func sweepStaleStagingsCommand(locator *core.TargetsTargetLocator, workerRoot string, live []string) core.CommandSpec {
	args := []string{"sh", "-c", staleStagingSweepScript, "mj-staging-sweep", workerRoot}
	args = append(args, live...)
	return core.LocatorCommand(core.SshTrafficClassControl, locator, args).
		WithPurpose("remove stale private worker staging")
}

// Install promotes the prepared binary while owner is held.
func (p *PreparedWorkerBinary) Install(owner *store.WorkerPermit) error {
	return InstallStagedWorkerBinary(owner, p.name, p.executor, &p.locator, p.sessionID)
}

// Name is the private staging name in the worker root (Rust: Deref<str>).
func (p *PreparedWorkerBinary) Name() string { return p.name }

// Close discards the private staging files, then unregisters the staging
// (Rust: Drop).
func (p *PreparedWorkerBinary) Close() {
	if _, err := targets.ExecuteChecked(p.executor, p.cleanup); err != nil {
		slog.Warn("worker binary staging cleanup failed", "error", err.Error(), "staging", p.name)
	}
	p.live.Release()
}

// PrepareRecoveryWorkerBinary resolves and transfers before the daemon
// admits a bounded recovery swap. Nil when the installed worker is current.
//
// A container whose image carries this build gets a fresh link to it: the
// image's worker and a host copy never share bytes, so the digest gate below
// would otherwise copy a host worker over a current one.
func PrepareRecoveryWorkerBinary(executor core.CommandExecutor, refresh *sessiontypes.DeferredWorkerBinaryRefresh) (*PreparedWorkerBinary, error) {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs prepare_recovery_worker_binary")
}

// FileSize is the bytes a local file will take on the target.
func FileSize(path string) (uint64, error) {
	info, err := os.Stat(path)
	if err != nil {
		return 0, fmt.Errorf("measure %s: %w", path, rustIOError(err))
	}
	return uint64(info.Size()), nil
}

func StageWorkerBinaryForUpgrade(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall) (*PreparedWorkerBinary, error) {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs stage_worker_binary_for_upgrade")
}

// PromoteWorkerBinary is Rust's PROMOTE_WORKER_BINARY.
//
// mv rejects two different names for the same inode. Reinstalling a current
// shared binary therefore consumes the private link without changing hel.
const PromoteWorkerBinary = `promote_worker_binary() {
    if [ "$1" -ef "$2" ]; then rm -f -- "$1"; else mv -f -- "$1" "$2"; fi
}`

func InstallStagedWorkerBinary(owner *store.WorkerPermit, staging string, executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string) error {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs install_staged_worker_binary")
}

// ReplaceInstalledWorkerLaunchConfig: ctx carries the session's worker
// permit (Rust: worker_lifecycle's task-local).
func ReplaceInstalledWorkerLaunchConfig(ctx context.Context, executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, launch *core.WorkerLaunchConfig) error {
	if _, err := store.Require(ctx, sessionID); err != nil {
		return err
	}
	plan, err := WorkerLaunchRefreshPlan(locator, sessionID, launch)
	if err != nil {
		return err
	}
	for _, command := range plan.Replace.Commands {
		if _, err := targets.ExecuteChecked(executor, command); err != nil {
			return err
		}
	}
	return nil
}

// PrepareManagedHarnessForUpgrade prepares the exact managed harness using
// the current worker binary. Remote targets receive a separately staged
// copy; local bare targets run the binary directly with a private launch
// config. The running worker is not stopped or replaced, so any failure
// here leaves the quiet session attachable on its previous build.
func PrepareManagedHarnessForUpgrade(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall, launch *core.WorkerLaunchConfig) error {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs prepare_managed_harness_for_upgrade")
}

// prepareContainerHarnessForUpgrade: a private staged binary prepares
// container fallbacks without touching the live worker's executable, launch
// config, control connection, or idle state.
func prepareContainerHarnessForUpgrade(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall, launch *core.WorkerLaunchConfig) error {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs prepare_container_harness_for_upgrade")
}

func PrepareInstalledManagedHarness(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, launch *core.WorkerLaunchConfig) error {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs prepare_installed_managed_harness")
}

// WorkerBinaryUploadPlan transfers into a private path. Published store and
// session paths never appear as upload destinations; callers verify before
// publishing them. remoteSource is the host store pin a remote container
// copies from, nil for every other target.
func WorkerBinaryUploadPlan(locator *core.TargetsTargetLocator, sessionID string, worker WorkerInstall, installed string, remoteSource *string) (core.CommandPlan, error) {
	var source *string
	if path, ok := worker.HostBinary(); ok {
		if err := core.VerifyWorkerBuild(path); err != nil {
			return core.CommandPlan{}, err
		}
		source = &path
	}
	return binaryUploadPlan(locator, sessionID, source, installed, remoteSource)
}

// binaryUploadPlan: source nil installs the container image's worker.
func binaryUploadPlan(locator *core.TargetsTargetLocator, sessionID string, source *string, installed string, remoteSource *string) (core.CommandPlan, error) {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs binary_upload_plan")
}

// installedFileDigestScript is the script of InstalledFileDigestCommand.
const installedFileDigestScript = "case $(uname -s) in Darwin) exec shasum -a 256 -- \"$1\";; Linux) exec sha256sum -- \"$1\";; *) echo 'unsupported target operating system for worker digest' >&2; exit 1;; esac"

func InstalledFileDigestCommand(locator *core.TargetsTargetLocator, path, purpose string) core.CommandSpec {
	// Resolve at execution time: this plan is also built on controller event
	// loops, where probing the remote platform would block. Both tools print
	// the digest first, which is the contract the refresh gate consumes.
	return core.LocatorCommand(core.SshTrafficClassControl, locator,
		[]string{"sh", "-c", installedFileDigestScript, "mj-worker-digest", path}).WithPurpose(purpose)
}

func WorkerLaunchRefreshPlan(locator *core.TargetsTargetLocator, sessionID string, launch *core.WorkerLaunchConfig) (sessiontypes.WorkerLaunchRefreshPlan, error) {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs worker_launch_refresh_plan")
}

func writeLaunchConfigCommand(locator *core.TargetsTargetLocator, installed string, body []byte) core.CommandSpec {
	stagedArg := core.JoinRemoteCommand([]string{installed + ".next"})
	installedArg := core.JoinRemoteCommand([]string{installed})
	script := fmt.Sprintf("umask 077; cat > %s && mv -f -- %s %s", stagedArg, stagedArg, installedArg)
	return core.LocatorCommand(core.SshTrafficClassBulk, locator, []string{"sh", "-c", script}).
		WithSensitiveStdin(body)
}

// WorkerBinaryRefreshPlan plans a refresh without resolving or hashing a
// worker binary. Both happen only after recovery has proved that the worker
// needs a restart. A nil refresh is Rust's None.
func WorkerBinaryRefreshPlan(locator *core.TargetsTargetLocator, sessionID string) (sessiontypes.WorkerBinaryRefresh, error) {
	workerRoot, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return nil, err
	}
	// Planning runs on controller event loops. Resolve and hash only in the
	// recovery task, for every target: even a local container can run a
	// foreign architecture, and an unavailable source must never silently
	// disable the binary refresh while installing a newer launch
	// configuration.
	return sessiontypes.WorkerBinaryRefreshDeferred{Refresh: sessiontypes.DeferredWorkerBinaryRefresh{
		Locator:         *locator,
		SessionID:       sessionID,
		InstalledDigest: InstalledFileDigestCommand(locator, workerRoot+"/hel", "identify installed Mjolnir worker binary"),
	}}, nil
}

// RefreshTargetWorkerBinaryIfStale refreshes a worker binary during
// recovery: pick the worker binary for the target's own architecture, and
// copy it over the installed one only when their digests differ. This runs
// inside the recovery task, where blocking target I/O is allowed; it must
// never be called from a UI/event loop.
//
// The digest gate is what stops a redeploy loop: once the right binary is
// installed, its digest matches the source and nothing is copied again,
// even though recovery may still restart the worker.
func RefreshTargetWorkerBinaryIfStale(executor core.CommandExecutor, refresh *sessiontypes.DeferredWorkerBinaryRefresh) error {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs refresh_target_worker_binary_if_stale")
}

// RefreshInstalledWorkerBinary copies the worker binary this controller
// would install for locator over the session's installed one, only when
// their digests differ. A parked sub-agent calls this before its worker
// starts again, so a daemon that was upgraded while the child was parked
// starts it on the current build.
func RefreshInstalledWorkerBinary(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string) error {
	refresh, err := WorkerBinaryRefreshPlan(locator, sessionID)
	if err != nil {
		return err
	}
	deferred, ok := refresh.(sessiontypes.WorkerBinaryRefreshDeferred)
	if !ok {
		return errors.New("the worker binary refresh plan was not deferred")
	}
	return RefreshTargetWorkerBinaryIfStale(executor, &deferred.Refresh)
}

// ReplaceTargetWorkerBinaryIfStale copies source over the installed worker
// only when the installed digest differs from source's. Returns whether a
// copy ran. Split from the resolver above so the digest gate is testable
// without resolving a real worker binary for a target architecture.
func ReplaceTargetWorkerBinaryIfStale(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, installedDigest *core.CommandSpec, source string) (bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/upgrade.rs replace_target_worker_binary_if_stale")
}
