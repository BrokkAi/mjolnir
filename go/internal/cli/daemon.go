// Port of mj-cli/src/daemon.rs.
//
// Application-owned daemon startup and attachment maintenance.
//
// Rust re-exports `mj_client::daemon::*` and
// `mj_client::executable::process_executable_path` from this module; Go
// callers use package client directly (client.ConnectExisting,
// client.DaemonClient, client.ProtocolVersion, client.DaemonNotRunningOf,
// client.ProcessExecutablePath, ...). Its re-export of
// `mj_daemon::daemon::run_daemon_process` belongs to `mj daemon-run`, which
// the Go build does not have yet.
//
// tokio's clock, tasks and select become context.Context, goroutines and
// select; Rust's `Option<T>` results are a pointer or a (value, bool) pair.

package cli

import (
	"context"
	"crypto/rand"
	"fmt"
	"os/exec"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

// runWhereDaemonLives forwards machine-owned commands before touching
// client stores or credentials: into the daemon container, or over SSH to
// the daemon's host. It returns false when the command runs here.
func runWhereDaemonLives(ctx context.Context, args []string, interactive bool) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs run_where_daemon_lives")
}

const devRestartStaleDaemonEnv = "MJ_DEV_RESTART_STALE_DAEMON"

// startNoticeDelay is how long a launched daemon may take to publish its
// endpoint before the client says out loud that startup is still running.
// Startup opens the store, applies migrations, pins worker sources and
// reconciles interrupted sessions before it can answer anything, so a busy
// machine or a large instance can pass this point and still be healthy.
const startNoticeDelay = 8 * time.Second

// startTimeout bounds startup. Large stores can take more than a minute to
// rebuild a table during an upgrade; this keeps startup bounded while
// allowing those migrations to finish.
const startTimeout = 300 * time.Second

// startupNotices is where slow-startup notices go while a splash owns the
// terminal. A nil route means stderr, as for every command that has no
// splash.
var startupNotices struct {
	mu    sync.Mutex
	route *core.Unbounded[string]
}

// startupNotice reports startup that is taking a while. A splash on the
// alternate screen shows the message itself, since stderr would scribble
// over its frame.
func startupNotice(message string) {
	panic("unported: mj-cli/src/daemon.rs startup_notice")
}

// startupNoticeRoute sends startup notices to the receiver
// openStartupNoticeRoute returns until it is closed (Rust drops it).
type startupNoticeRoute struct{}

// openStartupNoticeRoute is Rust's `StartupNoticeRoute::open`.
func openStartupNoticeRoute() (*startupNoticeRoute, *core.Unbounded[string]) {
	panic("unported: mj-cli/src/daemon.rs StartupNoticeRoute::open")
}

// Close is Rust's Drop: notices go to stderr again.
func (r *startupNoticeRoute) Close() {
	panic("unported: mj-cli/src/daemon.rs StartupNoticeRoute::drop")
}

// startupNoticeInterval is how often a notice repeats: the splash replaces
// its status line every second; stderr notices remain infrequent.
func startupNoticeInterval() time.Duration {
	startupNotices.mu.Lock()
	defer startupNotices.mu.Unlock()
	if startupNotices.route != nil {
		return time.Second
	}
	return 30 * time.Second
}

// daemonStartGuard is the daemon startup lock held exclusively: the only
// hold under which a client may start, stop or replace the daemon.
type daemonStartGuard struct {
	lock *core.FileLock
}

// sharedStartGuard is the daemon startup lock held shared: enough to use the
// running daemon, never to start or replace one. Any number of clients hold
// it at once, and none of them while a client holds it exclusively.
type sharedStartGuard struct {
	lock *core.FileLock
}

func acquireStartGuard(ctx context.Context, path string) (*daemonStartGuard, error) {
	lock, err := acquireStartLock(ctx, path, false)
	if err != nil {
		return nil, err
	}
	return &daemonStartGuard{lock: lock}, nil
}

func acquireSharedStartGuard(ctx context.Context, path string) (*sharedStartGuard, error) {
	lock, err := acquireStartLock(ctx, path, true)
	if err != nil {
		return nil, err
	}
	return &sharedStartGuard{lock: lock}, nil
}

func acquireStartLock(ctx context.Context, path string, shared bool) (*core.FileLock, error) {
	panic("unported: mj-cli/src/daemon.rs acquire_start_lock")
}

// sharedHandoffNotice is the line shown while another client holds the
// startup lock. When that client is replacing the daemon, the running
// daemon names what the handoff is waiting for (blockers, nil when it named
// nothing), so this client can say it too.
func sharedHandoffNotice(blockers []string) string {
	panic("unported: mj-cli/src/daemon.rs shared_handoff_notice")
}

// daemonWasReplaced is whether the daemon identified by instanceID has
// exited or been replaced, as after an automatic upgrade.
func daemonWasReplaced(ctx context.Context, instanceID string) bool {
	panic("unported: mj-cli/src/daemon.rs daemon_was_replaced")
}

// runningDaemonBlockers is what the running daemon, if any, reports as
// holding a handoff open; nil when it reports nothing.
func runningDaemonBlockers(ctx context.Context) []string {
	panic("unported: mj-cli/src/daemon.rs running_daemon_blockers")
}

// loggedPhase: startup phases shorter than this are not logged; longer ones
// always are, so a slow replacement leaves a record of where its time went.
const loggedPhase = 250 * time.Millisecond

// ConnectOrStart connects to this instance's daemon, starting or replacing
// it when needed.
func ConnectOrStart(ctx context.Context) (*client.DaemonClient, error) {
	panic("unported: mj-cli/src/daemon.rs connect_or_start")
}

func connectOrStartLocal(ctx context.Context) (*client.DaemonClient, error) {
	panic("unported: mj-cli/src/daemon.rs connect_or_start_local")
}

// connectOrStartHolding is the body of ConnectOrStart for a caller that
// already holds the daemon startup lock.
//
// That lock is not reentrant within one process, so a caller that needs it
// held across more than one step (a restart holds it across the stop and
// the replacement) must come through here instead of calling ConnectOrStart
// again. The guard is taken only so the requirement is visible at every call
// site.
func connectOrStartHolding(ctx context.Context, startup *daemonStartGuard) (*client.DaemonClient, error) {
	panic("unported: mj-cli/src/daemon.rs connect_or_start_holding")
}

func startupWaitFailure(label string, lastError error, probePending bool) string {
	panic("unported: mj-cli/src/daemon.rs startup_wait_failure")
}

// legacyPlacement is the placement of a daemon that predates the `Identity`
// action.
//
// This is the legacy 2.65.0 path: those container daemons shared the host's
// process namespace on Linux, so `/proc/<pid>/environ` named their
// container. A daemon of this protocol answers for itself instead. Keep this
// only while an upgrade can start from 2.65.x, and delete it with the legacy
// launcher.
func legacyPlacement(pid uint32) client.DaemonPlacement {
	panic("unported: mj-cli/src/daemon.rs legacy_placement")
}

// daemonPlacement is the placement the daemon reported (nil when it did
// not), unless the record says it cannot be trusted.
func daemonPlacement(metadata *client.DaemonMetadata, reported *client.DaemonPlacement) client.DaemonPlacement {
	panic("unported: mj-cli/src/daemon.rs daemon_placement")
}

// writeLauncherMetadata publishes the host's `daemon.json` for a daemon
// whose own filesystem is a container volume. The body is owner-only and
// replaced atomically, like the daemon's own publication.
func writeLauncherMetadata(metadata *client.DaemonMetadata) error {
	panic("unported: mj-cli/src/daemon.rs write_launcher_metadata")
}

// daemonIdentityOrLegacy is what a daemon reports about itself, or the
// legacy answer when an older daemon cannot report at all.
func daemonIdentityOrLegacy(ctx context.Context, metadata *client.DaemonMetadata) client.DaemonIdentity {
	panic("unported: mj-cli/src/daemon.rs daemon_identity_or_legacy")
}

// configuredPlacement is where the configuration puts the daemon on this
// computer: configuredPlacementHost or configuredPlacementContainer.
type configuredPlacement interface{ isConfiguredPlacement() }

// configuredPlacementHost is on the host; reason says why when a container
// was asked for.
type configuredPlacementHost struct{ reason *string }

type configuredPlacementContainer struct{ spec *containerSpec }

func (configuredPlacementHost) isConfiguredPlacement()      {}
func (configuredPlacementContainer) isConfiguredPlacement() {}

// configuredPlacementContainerName is the container's name, compared with
// the daemon's reported placement.
func configuredPlacementContainerName(placement configuredPlacement) (string, bool) {
	if container, ok := placement.(configuredPlacementContainer); ok {
		return container.spec.name, true
	}
	return "", false
}

// resolvePlacement resolves `[daemon]` to a placement. Both the launch and
// the decision to keep or replace a running daemon ask this, so they cannot
// disagree about where the daemon belongs.
func resolvePlacement(ctx context.Context, daemon *core.DaemonConfig) (configuredPlacement, error) {
	panic("unported: mj-cli/src/daemon.rs resolve_placement")
}

func loadDaemonConfig(ctx context.Context) (core.DaemonConfig, error) {
	panic("unported: mj-cli/src/daemon.rs load_daemon_config")
}

// daemonRunsThisBuild is whether the daemon, as it describes itself, is this
// build (nil: unknown).
//
// The daemon reports the executable file time in its build identity, so a
// host daemon is this build only when that file is this client's. A daemon
// in a container runs the image's own `mj`, which the launch accepted only
// because the image was built from this build's version and revision; its
// file time is the image's, so version and revision decide there.
func daemonRunsThisBuild(identity *client.DaemonIdentity) (*bool, error) {
	panic("unported: mj-cli/src/daemon.rs daemon_runs_this_build")
}

// hostDaemonLoginEnvironment reconstructs the account's login environment
// when this client runs inside a session (found false otherwise). A session
// is an overlay on that environment; rebuilding the base avoids keeping a
// second list of target, profile, harness and shell overrides here. The
// existing identity also covers older workers.
func hostDaemonLoginEnvironment() (environment map[string]string, found bool, err error) {
	panic("unported: mj-cli/src/daemon.rs host_daemon_login_environment")
}

func configureHostDaemonEnvironment(command *exec.Cmd, loginEnvironment map[string]string, found bool) error {
	panic("unported: mj-cli/src/daemon.rs configure_host_daemon_environment")
}

// launchDaemon starts `mj daemon-run` where `[daemon] runtime` says: a
// detached process on this computer, or a process in this instance's daemon
// container. Either way the caller follows the launched process until the
// daemon answers.
func launchDaemon(ctx context.Context, logPath string, priorUnpublished *client.ContainerDaemonRecord) (*launchedDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs launch_daemon")
}

func hostDaemonCommand() (*exec.Cmd, error) {
	panic("unported: mj-cli/src/daemon.rs host_daemon_command")
}

// daemonScopeLauncher (Linux only) is the systemd-run and runtime directory
// that start the daemon in its own user scope, or false when the daemon
// stays in this client's cgroup.
func daemonScopeLauncher(loginEnvironment map[string]string, found bool) (systemdRun, runtimeDir string, ok bool) {
	panic("unported: mj-cli/src/daemon.rs daemon_scope_launcher")
}

// retireLegacyNode retires the `mj node` that releases 2.66.0 through
// 2.70.x started beside every host daemon. That node exits when it loses
// its daemon and finds no `node-endpoint.json`, so removing the file here,
// after the old daemon is gone and before its replacement starts, retires
// it. Delete this once no supported upgrade starts from those releases.
func retireLegacyNode() {
	panic("unported: mj-cli/src/daemon.rs retire_legacy_node")
}

func randomToken() (string, error) {
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		return "", fmt.Errorf("generate the daemon token: %v", err)
	}
	return core.LowerHex(bytes), nil
}

func checkStoreReadiness(ctx context.Context) error {
	panic("unported: mj-cli/src/daemon.rs check_store_readiness")
}

func pingDaemon(ctx context.Context, daemon *client.DaemonClient) error {
	panic("unported: mj-cli/src/daemon.rs ping_daemon")
}

// WaitForWebAccess waits for the web access publication that owns all local
// API connection details: advertised URL, listener address, and certificate
// pin. It answers only client.WebViewerAccessReady.
func WaitForWebAccess(ctx context.Context, daemon *client.DaemonClient) (client.WebViewerAccess, error) {
	return waitForWebAccessWithTimeout(ctx, daemon, startTimeout)
}

func waitForWebAccessWithTimeout(ctx context.Context, daemon *client.DaemonClient, timeout time.Duration) (client.WebViewerAccess, error) {
	panic("unported: mj-cli/src/daemon.rs wait_for_web_access_with_timeout")
}

// daemonBuild is how the running daemon's build stands against this
// client's.
type daemonBuild struct {
	// release compares the release versions alone (-1, 0, 1 for the daemon
	// older, the same, newer), for the steps that apply only across
	// releases: replacing an older release before connecting to it, and an
	// attached client's re-execution into an upgraded release.
	release int
	// order is the full order: release, then commit time, then executable
	// time.
	order client.DaemonBuildOrder
}

func (b daemonBuild) daemonIsNewer() bool {
	return b.order == client.DaemonBuildOrderNewer
}

// daemonBuildOf is the one place a client decides whether the running
// daemon is newer than itself (Rust `daemon_build`). Ordinary startup, the
// development refresh and `mj daemon restart` all ask here while they hold
// the startup lock, so the answer they act on is the one for the daemon
// that lock lets them replace.
func daemonBuildOf(metadata *client.DaemonMetadata, placement *client.DaemonPlacement) (daemonBuild, error) {
	panic("unported: mj-cli/src/daemon.rs daemon_build")
}

// daemonBuildNotice is the sentence naming both builds, followed by what
// this client does.
func daemonBuildNotice(metadata *client.DaemonMetadata, decision string) string {
	panic("unported: mj-cli/src/daemon.rs daemon_build_notice")
}

// keptDaemonNoticeSaid is whether keptDaemonNotice has spoken.
var keptDaemonNoticeSaid atomic.Bool

// keptDaemonNotice says once per process that this client keeps using a
// daemon it will not replace. Every command, and every dashboard action,
// comes through startup, and the reason does not change between them.
func keptDaemonNotice(message string) {
	panic("unported: mj-cli/src/daemon.rs kept_daemon_notice")
}

// existingDaemon is what the running daemon, if any, is to this client.
type existingDaemon interface{ isExistingDaemon() }

// existingDaemonAbsent: none runs, so one has to be started.
type existingDaemonAbsent struct{}

type existingDaemonExitedContainer struct{ metadata client.DaemonMetadata }

type existingDaemonPendingContainer struct{ record client.ContainerDaemonRecord }

type existingDaemonReplaced struct{}

// existingDaemonUnanswered: a recorded daemon still owns the store or has a
// live process.
type existingDaemonUnanswered struct{ metadata client.DaemonMetadata }

// existingDaemonUse: it answers and this client may use it.
type existingDaemonUse struct{ daemon *client.DaemonClient }

// existingDaemonReplace: it answers but has to be replaced first. notice is
// what to tell the person when the replacement starts.
type existingDaemonReplace struct {
	metadata  client.DaemonMetadata
	placement client.DaemonPlacement
	notice    *string
}

func (existingDaemonAbsent) isExistingDaemon()           {}
func (existingDaemonExitedContainer) isExistingDaemon()  {}
func (existingDaemonPendingContainer) isExistingDaemon() {}
func (existingDaemonReplaced) isExistingDaemon()         {}
func (existingDaemonUnanswered) isExistingDaemon()       {}
func (existingDaemonUse) isExistingDaemon()              {}
func (existingDaemonReplace) isExistingDaemon()          {}

// inspectExistingDaemon is the one decision about the running daemon. Wire
// compatibility alone does not imply application readiness: an older
// release may lack migrations or fixes without changing the protocol. A
// same-version development build can also need a migration. Only the
// replacement daemon may perform that work.
//
// Deciding needs the startup lock in either mode, so the daemon it looks at
// is not being started or replaced meanwhile. Only an exclusive holder may
// act on existingDaemonReplace, through prepareExistingDaemon.
func inspectExistingDaemon(ctx context.Context, held *sharedStartGuard) (existingDaemon, error) {
	return decideExistingDaemon(ctx)
}

// daemonReleaseKind is how a recorded daemon stands toward its store.
type daemonReleaseKind int

const (
	daemonReleaseStillOwned daemonReleaseKind = iota
	daemonReleaseExited
	daemonReleaseReplaced
	daemonReleaseUnknown
)

// daemonRelease is Rust's `DaemonRelease`; reason is set for
// daemonReleaseUnknown.
type daemonRelease struct {
	kind   daemonReleaseKind
	reason string
}

func sameDaemonEndpoint(current, previous *client.DaemonMetadata) bool {
	return current.Address == previous.Address && current.Token == previous.Token
}

func matchingContainerOwner(metadata *client.DaemonMetadata) (*client.ContainerDaemonRecord, error) {
	panic("unported: mj-cli/src/daemon.rs matching_container_owner")
}

func missingContainerOwnerReason(err error) string {
	panic("unported: mj-cli/src/daemon.rs missing_container_owner_reason")
}

func probeDaemonRelease(ctx context.Context, metadata *client.DaemonMetadata) (daemonRelease, error) {
	panic("unported: mj-cli/src/daemon.rs probe_daemon_release")
}

const unreadableContainerOwnerRecovery = "Verify that no container daemon is running, then run `mj daemon stop --force` to clear the unreadable daemon-container.json"

func unpublishedContainer(ctx context.Context) (existingDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs unpublished_container")
}

func unanswered(ctx context.Context, metadata client.DaemonMetadata) (existingDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs unanswered")
}

func decideExistingDaemon(ctx context.Context) (existingDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs decide_existing_daemon")
}

// preparedDaemon is Rust's `PreparedDaemon`: ready when set, otherwise
// launch one, after priorUnpublished when a container launch was pending.
type preparedDaemon struct {
	ready            *client.DaemonClient
	priorUnpublished *client.ContainerDaemonRecord
}

// prepareExistingDaemon acts on decideExistingDaemon while holding the
// startup lock exclusively: use the running daemon, or replace it and
// report that none is running yet. A recorded daemon that does not answer is
// waited for until it answers or provably exits; starting another beside it
// could leave two daemons serving one instance.
func prepareExistingDaemon(ctx context.Context, startup *daemonStartGuard) (preparedDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs prepare_existing_daemon")
}

func pendingContainerMetadata(record *client.ContainerDaemonRecord) client.DaemonMetadata {
	panic("unported: mj-cli/src/daemon.rs pending_container_metadata")
}

func connectReadyContainer(ctx context.Context, metadata *client.DaemonMetadata) (*client.DaemonClient, error) {
	panic("unported: mj-cli/src/daemon.rs connect_ready_container")
}

func requestDaemonIdentity(ctx context.Context, daemon *client.DaemonClient) (client.DaemonIdentity, error) {
	panic("unported: mj-cli/src/daemon.rs request_daemon_identity")
}

func containerDaemonMayBeAlive(ctx context.Context, runtime core.DaemonRuntime, id string) bool {
	panic("unported: mj-cli/src/daemon.rs container_daemon_may_be_alive")
}

type containerAdoption int

const (
	containerAdoptionPublished containerAdoption = iota
	containerAdoptionExited
)

func adoptContainerDaemon(ctx context.Context, record *client.ContainerDaemonRecord) (containerAdoption, error) {
	panic("unported: mj-cli/src/daemon.rs adopt_container_daemon")
}

// ensureConfigLoads refuses to stop a daemon when the one that would replace
// it cannot read the configuration and would exit at startup, leaving the
// instance stopped. action finishes "The Mjolnir daemon was not ...".
func ensureConfigLoads(configPath, action string) error {
	panic("unported: mj-cli/src/daemon.rs ensure_config_loads")
}

// RestartedDaemon is the daemon a restart produced.
type RestartedDaemon struct {
	Pid uint32
	// RunsThisBuild is whether the daemon runs this client's build, as the
	// daemon reports it (nil: unknown).
	RunsThisBuild *bool
}

// restartAttempts is how many times a restart stops the daemon it finds and
// starts its own.
//
// The startup lock excludes a client that has not started its daemon yet,
// but not one that started it just before this restart took the lock, so
// one retry is worth making. A second loss means that client is producing
// daemons faster than they can be replaced, which is a report rather than a
// race to keep running.
const restartAttempts = 2

// stopInstance stops this instance, including when the daemon is already
// absent. The startup lock is held throughout, so a stop cannot call the
// daemon absent while another client is still launching it. It returns
// whether a running daemon was stopped.
func stopInstance(ctx context.Context, force bool) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs stop_instance")
}

func clearUnreadableContainerOwner() error {
	panic("unported: mj-cli/src/daemon.rs clear_unreadable_container_owner")
}

func stopPendingContainer(ctx context.Context, startup *daemonStartGuard, record *client.ContainerDaemonRecord, force bool) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs stop_pending_container")
}

// RestartDaemon stops the running Mjolnir daemon and starts one from this
// client's executable.
//
// The startup lock is held from before the stop until the replacement has
// answered a request. Every client that starts a daemon takes that lock
// first, including clients built before this function existed, so none of
// them can install a daemon of its own in the gap the stop opens. Without
// that, an attached client running an older build wins the gap and the
// restart reports someone else's daemon as the one it was asked for.
func RestartDaemon(ctx context.Context) (RestartedDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs restart_daemon")
}

// restartVerdict turns one build-identity answer into the restart's result.
//
// This is separate from the restart itself so the outcome can be exercised
// without starting daemons. It is also the decision the command used to
// skip: reporting success on a changed process id alone is what let a
// restart announce a daemon running code the caller had just replaced.
func restartVerdict(pid uint32, identity *client.DaemonIdentity, runsThisBuild *bool) (RestartedDaemon, error) {
	panic("unported: mj-cli/src/daemon.rs restart_verdict")
}

// startupOutcomeKind is why waiting for a launched daemon stopped.
type startupOutcomeKind int

const (
	// startupOutcomeReady: it answered a request.
	startupOutcomeReady startupOutcomeKind = iota
	// startupOutcomeExited: the launched process is gone.
	startupOutcomeExited
	// startupOutcomeStillStarting: it is still alive but has not answered
	// within startTimeout.
	startupOutcomeStillStarting
)

// startupOutcome is Rust's `StartupOutcome<T>`: ready holds the answer of a
// startupOutcomeReady; lastError and probePending describe a
// startupOutcomeStillStarting.
type startupOutcome[T any] struct {
	kind         startupOutcomeKind
	ready        T
	lastError    error
	probePending bool
}

// waitForReadyDaemon waits for a launched daemon to answer a request.
//
// A daemon that is still alive is still starting: initialization runs
// before it publishes its endpoint, so a slow start is not a failure and the
// wait continues. Only the process leaving, or the hard startTimeout bound,
// ends the wait without a client. notice is called once, startNoticeDelay
// into the wait.
func waitForReadyDaemon[T any](
	ctx context.Context,
	isAlive func(ctx context.Context) bool,
	probe func(ctx context.Context) (T, error),
	notice func(waited time.Duration),
) startupOutcome[T] {
	panic("unported: mj-cli/src/daemon.rs wait_for_ready_daemon")
}

// launchedDaemon is the daemon this client launched and where its log stood
// at launch.
type launchedDaemon struct {
	pid       uint32
	logOffset uint64
	container *containerLaunch
}

func (d *launchedDaemon) label() string {
	if d.container != nil {
		return "container " + d.container.containerName
	}
	return fmt.Sprintf("(waiter PID %d)", d.pid)
}

func (d *launchedDaemon) isAlive(ctx context.Context) bool {
	panic("unported: mj-cli/src/daemon.rs LaunchedDaemon::is_alive")
}

// outputSinceLaunch includes process diagnostics and detached stderr, and
// names where they came from. Failures before logger initialization have
// only stderr; later failures also have a process log.
func (d *launchedDaemon) outputSinceLaunch(ctx context.Context, logPath string) (path string, output string) {
	panic("unported: mj-cli/src/daemon.rs LaunchedDaemon::output_since_launch")
}

func (d *launchedDaemon) failure(reason, output, logPath string) error {
	panic("unported: mj-cli/src/daemon.rs LaunchedDaemon::failure")
}

// launchDiagnostics finds the launched daemon's diagnostic log (nil when it
// has none yet) and, for a daemon that already exited, its exit record.
func launchDiagnostics(directory string, pid uint32) (*string, *core.DaemonExitRecord, error) {
	panic("unported: mj-cli/src/daemon.rs launch_diagnostics")
}

// launchLogTail is the last 20 non-blank lines of the at most 64 KiB written
// to path after offset.
func launchLogTail(path string, offset uint64) (string, error) {
	panic("unported: mj-cli/src/daemon.rs launch_log_tail")
}

// daemonLaunchExecutable is the running executable: `/proc/self/exe` on
// Linux, because the pathname can disappear on upgrade and executing the
// running inode keeps the daemon on this client's protocol.
func daemonLaunchExecutable() (string, error) {
	panic("unported: mj-cli/src/daemon.rs daemon_launch_executable")
}

// developmentDaemonRefresh is Rust's `DEVELOPMENT_DAEMON_REFRESH`
// OnceCell (Linux and macOS): the refresh runs until it once succeeds.
var developmentDaemonRefresh struct {
	mu   sync.Mutex
	done bool
}

// developmentWorkersChangedSince (Linux and macOS) is whether a local
// development worker binary changed after started.
func developmentWorkersChangedSince(
	started time.Time,
	resolve func(arch string) (workerstaging.WorkerBinaryAvailability, error),
) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs development_workers_changed_since")
}

func maybeReplaceStaleDevelopmentDaemon(ctx context.Context, startup *daemonStartGuard) error {
	panic("unported: mj-cli/src/daemon.rs maybe_replace_stale_development_daemon")
}

// replaceDaemon hands the daemon off to this build. Automatic upgrades have
// no authority to cancel work, even when a daemon is slow or temporarily
// unreachable. Explicit restart uses stopDaemon.
func replaceDaemon(ctx context.Context, startup *daemonStartGuard, metadata *client.DaemonMetadata, placement *client.DaemonPlacement) error {
	panic("unported: mj-cli/src/daemon.rs replace_daemon")
}

// handOffDaemon asks the daemon to hand off and follows it until it exits.
func handOffDaemon(ctx context.Context, metadata *client.DaemonMetadata) error {
	panic("unported: mj-cli/src/daemon.rs hand_off_daemon")
}

type handoffAskOutcome int

const (
	handoffAskAccepted handoffAskOutcome = iota
	handoffAskGone
	handoffAskReplaced
)

// String is Rust's Debug, which the handoff log records.
func (o handoffAskOutcome) String() string {
	switch o {
	case handoffAskAccepted:
		return "Accepted"
	case handoffAskGone:
		return "Gone"
	case handoffAskReplaced:
		return "Replaced"
	}
	return fmt.Sprintf("handoffAskOutcome(%d)", int(o))
}

const (
	handoffAskInterval = 250 * time.Millisecond
	blockerPrewarmLead = time.Second
)

func askPrepareUpgrade(ctx context.Context, metadata *client.DaemonMetadata) (client.DaemonReply, error) {
	panic("unported: mj-cli/src/daemon.rs ask_prepare_upgrade")
}

func askForHandoff(ctx context.Context, metadata *client.DaemonMetadata) (handoffAskOutcome, error) {
	panic("unported: mj-cli/src/daemon.rs ask_for_handoff")
}

// handoffOwnerChange is how the store's owner changed (false: it did not).
func handoffOwnerChange(ctx context.Context, metadata *client.DaemonMetadata) (handoffAskOutcome, bool, error) {
	panic("unported: mj-cli/src/daemon.rs handoff_owner_change")
}

// clearExitedDaemon removes the exited daemon's records. The caller holds
// the startup lock across the compare and removal.
func clearExitedDaemon(metadata *client.DaemonMetadata) error {
	panic("unported: mj-cli/src/daemon.rs clear_exited_daemon")
}

func removeMatchingMetadata(previous *client.DaemonMetadata) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs remove_matching_metadata")
}

func waitForDaemonRelease(ctx context.Context, metadata *client.DaemonMetadata, timeout time.Duration) error {
	panic("unported: mj-cli/src/daemon.rs wait_for_daemon_release")
}

// upgradeWaitNotice is the line shown while an automatic upgrade waits for
// the old daemon (blockers nil when the daemon named none).
func upgradeWaitNotice(blockers []string) string {
	panic("unported: mj-cli/src/daemon.rs upgrade_wait_notice")
}

// pendingBlockers is a background observation scoped to one wait for the
// startup lock or handoff. close (Rust's Drop) abandons a query still
// running.
type pendingBlockers struct {
	task   *core.JoinHandle[[]string]
	cancel context.CancelFunc
	// latest is the last answer; nil when there is none.
	latest []string
}

func (b *pendingBlockers) ensureStarted(ctx context.Context, query func(ctx context.Context) []string) {
	panic("unported: mj-cli/src/daemon.rs PendingBlockers::ensure_started")
}

func (b *pendingBlockers) refresh(ctx context.Context, query func(ctx context.Context) []string) []string {
	panic("unported: mj-cli/src/daemon.rs PendingBlockers::refresh")
}

func (b *pendingBlockers) close() {
	panic("unported: mj-cli/src/daemon.rs PendingBlockers::drop")
}

// upgradeBlockers asks the daemon what holds its handoff open. Older
// daemons close the connection for an unknown action. With no labels (nil),
// the wait notice describes the handoff without naming any specific work.
func upgradeBlockers(ctx context.Context, metadata *client.DaemonMetadata) []string {
	panic("unported: mj-cli/src/daemon.rs upgrade_blockers")
}

func ensureContainerOwnerRecorded(ctx context.Context, startup *daemonStartGuard, metadata *client.DaemonMetadata, placement *client.DaemonPlacement) error {
	panic("unported: mj-cli/src/daemon.rs ensure_container_owner_recorded")
}

type stopFallback int

const (
	stopFallbackGraceful stopFallback = iota
	stopFallbackForce
	stopFallbackRestart
)

func stopDaemon(ctx context.Context, startup *daemonStartGuard, metadata *client.DaemonMetadata, fallback stopFallback) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs stop_daemon")
}

// clearUnreachableContainerRecord clears a legacy container daemon's stale
// record. The caller holds the startup lock and has explicitly forced
// legacy recovery.
func clearUnreachableContainerRecord(ctx context.Context, metadata *client.DaemonMetadata) (bool, error) {
	panic("unported: mj-cli/src/daemon.rs clear_unreachable_container_record")
}

// signalDaemon stops the daemon metadata names after it did not answer a
// protocol stop: SIGTERM on Unix, which every supported daemon handles as
// graceful cancellation. Windows has no SIGTERM, and the daemon has no
// console to receive Ctrl-Break, so it is terminated; its state is in the
// store and its workers keep running, so the next daemon recovers it as
// after a crash.
func signalDaemon(ctx context.Context, metadata *client.DaemonMetadata) error {
	panic("unported: mj-cli/src/daemon.rs signal_daemon")
}

// hostPID is what runs at a daemon's recorded PID, judged by its command
// line so a reused PID cannot stand in for the daemon.
type hostPID int

const (
	hostPIDDaemon hostPID = iota
	// hostPIDNotDaemon: gone, or running something other than
	// `mj daemon-run`.
	hostPIDNotDaemon
	// hostPIDUnreadable: the process exists but its command line cannot be
	// read. A daemon in the middle of exiting looks like this on macOS and
	// Windows: its arguments are gone before its PID is.
	hostPIDUnreadable
)

// hostPIDOf is Rust's `host_pid`.
func hostPIDOf(pid uint32) hostPID {
	panic("unported: mj-cli/src/daemon.rs host_pid")
}

// hostPIDIsDaemon is whether the daemon still runs at pid, for a caller
// about to signal it. An unreadable command line is inconclusive, so it is
// an error here.
func hostPIDIsDaemon(pid uint32) (bool, error) {
	switch hostPIDOf(pid) {
	case hostPIDDaemon:
		return true, nil
	case hostPIDNotDaemon:
		return false, nil
	}
	return false, fmt.Errorf("could not read the command line for Mjolnir daemon PID %d", pid)
}

// DaemonPresence is what the keep-alive last saw when it refreshed this
// client's presence: DaemonPresenceAttached, DaemonPresenceMissing or
// DaemonPresenceUpgraded. Values compare with ==.
type DaemonPresence interface{ isDaemonPresence() }

// DaemonPresenceAttached: a daemon answered and knows this client is
// attached.
type DaemonPresenceAttached struct{}

// DaemonPresenceMissing: no daemon answered. Reason is for the person
// reading it.
type DaemonPresenceMissing struct{ Reason string }

// DaemonPresenceUpgraded: a newer installed daemon owns the instance. The
// terminal must preserve its local state and re-exec that build before
// reading the store again.
type DaemonPresenceUpgraded struct{ Target UpgradeTarget }

func (DaemonPresenceAttached) isDaemonPresence() {}
func (DaemonPresenceMissing) isDaemonPresence()  {}
func (DaemonPresenceUpgraded) isDaemonPresence() {}

// Attachment is the keep-alive task and what it reports about the daemon.
type Attachment struct {
	Task     *core.JoinHandle[struct{}]
	Presence *core.Watch[DaemonPresence]
}

const attachmentRefreshDisplayTimeout = 5 * time.Second

// MaintainAttachment keeps this client's presence fresh in whatever daemon
// is running, until cancellation ends (Rust's CancellationToken).
//
// This is a two-second timer, not a user action, so it only ever
// reconnects. It deliberately does not start a daemon: a timer cannot
// express the intent to start one, and when the client's own executable has
// been replaced on disk the daemon it would start runs code the user has
// already discarded. That is exactly how a stopped daemon used to come back
// on an old build, beating the restart that had just stopped it. When no
// daemon answers, the surface says so and offers its explicit restart
// instead.
func MaintainAttachment(cancellation context.Context, clientID string) Attachment {
	panic("unported: mj-cli/src/daemon.rs maintain_attachment")
}

func observeAttachment(ctx context.Context, clientID string) (DaemonPresence, error) {
	panic("unported: mj-cli/src/daemon.rs observe_attachment")
}

// UpgradeTarget is the newer build an upgraded daemon runs.
type UpgradeTarget struct {
	Executable string
	Generation string
}

// continueUnderUpgradedBuild finishes a one-shot command under the daemon's
// newer build, as a dashboard does when an upgrade replaces its daemon. args
// name only what is left to do, so nothing the command already did is
// repeated. It returns only when the newer build could not be started.
func continueUnderUpgradedBuild(target *UpgradeTarget, args []string) error {
	panic("unported: mj-cli/src/daemon.rs continue_under_upgraded_build")
}

// upgradedDaemonExecutable is the newer build to continue under, or nil.
func upgradedDaemonExecutable(ctx context.Context) (*UpgradeTarget, error) {
	panic("unported: mj-cli/src/daemon.rs upgraded_daemon_executable")
}
