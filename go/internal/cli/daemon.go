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
// spawn_blocking work runs inline: every caller is already off any render
// loop.

package cli

import (
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

// runWhereDaemonLives forwards machine-owned commands before touching
// client stores or credentials: into the daemon container, or over SSH to
// the daemon's host. It returns false when the command runs here.
func runWhereDaemonLives(ctx context.Context, args []string, interactive bool) (bool, error) {
	interactive = interactive && !slices.Contains(args, "--json")
	placement, err := loadDaemonConfig(ctx)
	if err != nil {
		return false, err
	}
	if placement.Runtime != core.DaemonRuntimeSsh {
		// Lifecycle commands for a host or container daemon run here,
		// through management discovery and the engine. Running them inside
		// the container would start a stopped daemon before reporting on it.
		if len(args) > 0 && args[0] == "daemon" {
			return false, nil
		}
		return execInContainer(ctx, args, interactive)
	}
	if placement.Host == nil {
		return false, errors.New("[daemon] host is required for SSH placement")
	}
	host := *placement.Host
	if strings.HasPrefix(host, "-") {
		return false, errors.New("[daemon] host cannot be an SSH option")
	}
	instance, hasInstance := placement.RemoteInstance()
	var instancePointer *string
	if hasInstance {
		instancePointer = &instance
	}
	if slices.Equal(args, []string{"daemon", "status"}) {
		fmt.Printf("Daemon host: %s\n", host)
		if hasInstance {
			fmt.Printf("Daemon instance: %s\n", instance)
		} else {
			fmt.Println("Daemon instance: the remote's own")
		}
		fmt.Printf("Client config: %s\n", core.ConfigPath())
		this := client.ThisBuild()
		fmt.Printf("Client build: %s\n", this.Published())
		endpoint, err := client.Discover(ctx, placement, instancePointer, client.DiscoveryIntentExisting)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Daemon build unavailable: %s\n", err)
		} else {
			remote, err := client.BuildIdentityParse(endpoint.BuildVersion)
			if err != nil {
				return false, err
			}
			fmt.Printf("Daemon build: %s\n", remote.Published())
			fmt.Printf("same_source: %t\n", this.SameSource(remote))
		}
	}
	argv := []string{placement.RemoteCommandOrDefault()}
	if hasInstance {
		argv = append(argv, "--instance", instance)
	}
	argv = append(argv, args...)
	var sshArgs []string
	if interactive {
		sshArgs = append(sshArgs, "-t")
	}
	sshArgs = append(sshArgs, placement.SSHArgs...)
	sshArgs = append(sshArgs, host, core.DaemonLoginCommand(argv))
	status, err := core.RunInherited(exec.Command("ssh", sshArgs...))
	if err != nil {
		return false, err
	}
	if status.ExitCode() == 127 {
		fmt.Fprintf(os.Stderr, "[daemon] remote_command %s was not found on %s; install mj remotely with scripts/install.sh (or install the same release)\n",
			core.RustDebugString(placement.RemoteCommandOrDefault()), host)
	}
	if !status.Success() {
		code := status.ExitCode()
		if code < 0 {
			code = 1
		}
		os.Exit(code)
	}
	return true, nil
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
	startupNotices.mu.Lock()
	defer startupNotices.mu.Unlock()
	if route := startupNotices.route; route != nil && route.Send(message) == nil {
		return
	}
	fmt.Fprintln(os.Stderr, message)
}

// startupNoticeRoute sends startup notices to the receiver
// openStartupNoticeRoute returns until it is closed (Rust drops it).
type startupNoticeRoute struct{}

// openStartupNoticeRoute is Rust's `StartupNoticeRoute::open`.
func openStartupNoticeRoute() (*startupNoticeRoute, *core.Unbounded[string]) {
	channel := core.NewUnbounded[string]()
	startupNotices.mu.Lock()
	startupNotices.route = channel
	startupNotices.mu.Unlock()
	return &startupNoticeRoute{}, channel
}

// Close is Rust's Drop: notices go to stderr again.
func (r *startupNoticeRoute) Close() {
	startupNotices.mu.Lock()
	startupNotices.route = nil
	startupNotices.mu.Unlock()
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

// release is Rust's Drop of the guard: the lock is given up and its file
// closed.
func (g *daemonStartGuard) release() {
	releaseStartLock(&g.lock)
}

// release is Rust's Drop of the guard.
func (g *sharedStartGuard) release() {
	releaseStartLock(&g.lock)
}

// releaseStartLock unlocks and closes a held start lock once.
func releaseStartLock(lock **core.FileLock) {
	if *lock == nil {
		return
	}
	file := (*lock).File()
	(*lock).Drop()
	*lock = nil
	_ = file.Close()
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
	asked := time.Now()
	prewarmAt := asked.Add(startNoticeDelay - blockerPrewarmLead)
	noticeAt := asked.Add(startNoticeDelay)
	blockers := &pendingBlockers{}
	defer blockers.close()
	for {
		lock, err := tryStartLock(path, shared)
		if err != nil {
			return nil, err
		}
		if lock != nil {
			if waited := time.Since(asked); waited >= loggedPhase {
				slog.Info("waited for the daemon startup lock another client held",
					"waited_ms", waited.Milliseconds(), "shared", shared)
			}
			return lock, nil
		}
		now := time.Now()
		if !now.Before(noticeAt) {
			startupNotice(sharedHandoffNotice(blockers.refresh(ctx, runningDaemonBlockers)))
			noticeAt = now.Add(startupNoticeInterval())
		} else if !now.Before(prewarmAt) {
			blockers.ensureStarted(ctx, runningDaemonBlockers)
		}
		if err := sleepContext(ctx, client.RetryDelay); err != nil {
			return nil, err
		}
	}
}

// tryStartLock is one attempt at the startup lock: the held lock, or nil
// when another client holds it in a mode that excludes this one.
func tryStartLock(path string, shared bool) (*core.FileLock, error) {
	if parent := filepath.Dir(path); parent != path {
		if err := core.CreateOwnerOnlyDirectory(parent); err != nil {
			return nil, err
		}
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open daemon startup lock: %w", err)
	}
	locked, err := core.TryLockFile(file, shared)
	if err != nil {
		file.Close()
		return nil, fmt.Errorf("lock daemon startup: %w", err)
	}
	if !locked {
		file.Close()
		return nil, nil
	}
	return core.NewFileLock(file), nil
}

// sharedHandoffNotice is the line shown while another client holds the
// startup lock. When that client is replacing the daemon, the running
// daemon names what the handoff is waiting for (blockers, nil when it named
// nothing), so this client can say it too.
func sharedHandoffNotice(blockers []string) string {
	if blockers != nil {
		return fmt.Sprintf("Mjolnir is waiting for another client to finish the daemon handoff; the running daemon is finishing: %s.",
			strings.Join(blockers, ", "))
	}
	return "Mjolnir is waiting for another client to finish the daemon handoff."
}

// daemonWasReplaced is whether the daemon identified by instanceID has
// exited or been replaced, as after an automatic upgrade.
func daemonWasReplaced(ctx context.Context, instanceID string) bool {
	daemon, err := client.ConnectExisting(ctx)
	if err != nil {
		return true
	}
	defer daemon.Close()
	identity, err := daemon.Identity(ctx)
	if err != nil {
		return true
	}
	return identity.InstanceID != instanceID
}

// runningDaemonBlockers is what the running daemon, if any, reports as
// holding a handoff open; nil when it reports nothing.
func runningDaemonBlockers(ctx context.Context) []string {
	metadata, err := client.ReadMetadataAny()
	if err != nil {
		return nil
	}
	return upgradeBlockers(ctx, &metadata)
}

// loggedPhase: startup phases shorter than this are not logged; longer ones
// always are, so a slow replacement leaves a record of where its time went.
const loggedPhase = 250 * time.Millisecond

// ConnectOrStart connects to this instance's daemon, starting or replacing
// it when needed.
func ConnectOrStart(ctx context.Context) (*client.DaemonClient, error) {
	connector, err := client.DaemonConnectorShared(ctx)
	if err != nil {
		return nil, err
	}
	return connector.ConnectOrStart(ctx, connectOrStartLocal)
}

func connectOrStartLocal(ctx context.Context) (*client.DaemonClient, error) {
	lockPath := filepath.Join(core.DataDir(), "daemon-start.lock")
	// Most clients find a daemon they can use. Deciding that needs only a
	// shared hold, so a burst of clients reconnecting after a handoff checks
	// the daemon side by side instead of one at a time, which multiplied
	// each check by the number of clients waiting. A shared hold still waits
	// out a client that holds the lock to start or replace the daemon, so
	// every waiter uses the daemon that client publishes.
	if _, set := os.LookupEnv(devRestartStaleDaemonEnv); !set {
		shared, err := acquireSharedStartGuard(ctx, lockPath)
		if err != nil {
			return nil, err
		}
		existing, err := inspectExistingDaemon(ctx, shared)
		shared.release()
		if err != nil {
			return nil, err
		}
		if use, ok := existing.(existingDaemonUse); ok {
			return use.daemon, nil
		}
	}
	// Starting or replacing the daemon is serialized across clients, and the
	// decision is taken again under the exclusive hold: another client may
	// have published a daemon since the shared check.
	startup, err := acquireStartGuard(ctx, lockPath)
	if err != nil {
		return nil, err
	}
	defer startup.release()
	held := time.Now()
	connected, err := connectOrStartHolding(ctx, startup)
	// Every other client waits while this one holds the lock, so how long
	// it held it is what they waited for.
	if elapsed := time.Since(held); elapsed >= loggedPhase {
		slog.Info("held the daemon startup lock", "held_ms", elapsed.Milliseconds())
	}
	return connected, err
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
	if err := maybeReplaceStaleDevelopmentDaemon(ctx, startup); err != nil {
		return nil, err
	}
	prepared, err := prepareExistingDaemon(ctx, startup)
	if err != nil {
		return nil, err
	}
	if prepared.ready != nil {
		return prepared.ready, nil
	}
	priorUnpublished := prepared.priorUnpublished

	// Metadata disappears before shutdown releases the sole-writer lock. Do
	// not launch a child that can only fail to acquire that lock. A
	// container daemon owns a store in its volume, so the host guard is not
	// about it at all.
	configuredContainer := false
	if daemon, err := loadDaemonConfig(ctx); err == nil {
		_, configuredContainer = daemon.Runtime.Engine()
	}
	if !configuredContainer {
		released := time.Now()
		handoffDeadline := released.Add(client.StopTimeout)
		for {
			guard, err := controller.ControllerStoreGuardTryAcquire()
			if err != nil {
				return nil, err
			}
			if guard != nil {
				guard.Drop()
				if waited := time.Since(released); waited >= loggedPhase {
					slog.Info("waited for the previous daemon to release the store", "waited_ms", waited.Milliseconds())
				}
				break
			}
			// A daemon started outside this client's startup lock may be
			// becoming ready.
			prepared, err := prepareExistingDaemon(ctx, startup)
			if err != nil {
				return nil, err
			}
			if prepared.ready != nil {
				return prepared.ready, nil
			}
			priorUnpublished = prepared.priorUnpublished
			if !time.Now().Before(handoffDeadline) {
				return nil, fmt.Errorf("controller store is still owned by another process after %ds; daemon startup was not attempted",
					int64(client.StopTimeout/time.Second))
			}
			if err := sleepContext(ctx, client.RetryDelay); err != nil {
				return nil, err
			}
		}
	}

	logPath := filepath.Join(core.DataDir(), "daemon.log")
	launched, err := launchDaemon(ctx, logPath, priorUnpublished)
	if err != nil {
		return nil, err
	}
	launchedAt := time.Now()
	label := launched.label()
	slog.Info("launched the daemon", "daemon", label)

	outcome := waitForReadyDaemon(ctx,
		launched.isAlive,
		func(ctx context.Context) (*client.DaemonClient, error) {
			if launched.container != nil {
				return connectReadyContainer(ctx, &launched.container.metadata)
			}
			daemon, err := client.ConnectExisting(ctx)
			if err != nil {
				return nil, err
			}
			if err := pingDaemon(ctx, daemon); err != nil {
				daemon.Close()
				return nil, err
			}
			if err := checkStoreReadiness(ctx); err != nil {
				daemon.Close()
				return nil, err
			}
			return daemon, nil
		},
		func(waited time.Duration) {
			startupNotice(fmt.Sprintf("Mjolnir daemon %s has been starting for %ds; still waiting.", label, int64(waited/time.Second)))
		},
	)
	var reason string
	switch outcome.kind {
	case startupOutcomeReady:
		if launched.container != nil {
			if err := writeLauncherMetadata(&launched.container.metadata); err != nil {
				outcome.ready.Close()
				return nil, err
			}
		}
		slog.Info("the launched daemon is ready", "daemon", label, "ready_ms", time.Since(launchedAt).Milliseconds())
		return outcome.ready, nil
	// A daemon that fails to initialize explains itself in its log and
	// exits without ever publishing an endpoint. That explanation is the
	// answer; the client's own failure to reach the absent endpoint is not.
	case startupOutcomeExited:
		// An engine may still finish an interrupted exec. Keep its
		// credentials.
		reason = fmt.Sprintf("Mjolnir daemon %s exited before it was ready", label)
	case startupOutcomeStillStarting:
		reason = startupWaitFailure(label, outcome.lastError, outcome.probePending)
	}
	outputPath, output := launched.outputSinceLaunch(ctx, logPath)
	return nil, launched.failure(reason, output, outputPath)
}

func startupWaitFailure(label string, lastError error, probePending bool) string {
	reason := fmt.Sprintf("Mjolnir daemon %s is still starting after %ds", label, int64(startTimeout/time.Second))
	if probePending {
		reason += ". A readiness probe is still pending"
	}
	if lastError != nil {
		reason += ". Last completed probe failed: " + lastError.Error()
	}
	return reason
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
	if pid == 0 {
		return client.DaemonPlacement{Kind: client.DaemonPlacementKindContainer, Name: "<unknown>"}
	}
	if runtime.GOOS != "linux" {
		return client.DaemonPlacement{Kind: client.DaemonPlacementKindHost}
	}
	environ, err := os.ReadFile(fmt.Sprintf("/proc/%d/environ", pid))
	if err == nil {
		prefix := client.DaemonContainerEnv + "="
		for _, entry := range strings.Split(string(environ), "\x00") {
			if name, ok := strings.CutPrefix(entry, prefix); ok {
				return client.DaemonPlacement{Kind: client.DaemonPlacementKindContainer, Name: core.LossyUTF8([]byte(name))}
			}
		}
	}
	return client.DaemonPlacement{Kind: client.DaemonPlacementKindHost}
}

// daemonPlacement is the placement the daemon reported (nil when it did
// not), unless the record says it cannot be trusted.
func daemonPlacement(metadata *client.DaemonMetadata, reported *client.DaemonPlacement) client.DaemonPlacement {
	if reported != nil {
		if _, inContainer := reported.Container(); metadata.Pid != 0 || inContainer {
			return *reported
		}
	}
	return legacyPlacement(metadata.Pid)
}

// writeLauncherMetadata publishes the host's `daemon.json` for a daemon
// whose own filesystem is a container volume. The body is owner-only and
// replaced atomically, like the daemon's own publication.
func writeLauncherMetadata(metadata *client.DaemonMetadata) error {
	path := client.MetadataPath()
	parent := filepath.Dir(path)
	if parent == path {
		return errors.New("container daemon metadata path has no parent")
	}
	if err := core.CreateOwnerOnlyDirectory(parent); err != nil {
		return err
	}
	temporary := filepath.Join(parent, fmt.Sprintf(".daemon.%d.tmp", os.Getpid()))
	body, err := core.SerdeJSONPretty(metadata)
	if err != nil {
		return err
	}
	file, err := os.OpenFile(temporary, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return fmt.Errorf("create %s: %w", temporary, err)
	}
	if _, err := file.Write(body); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if runtime.GOOS == "windows" {
		// Windows does not replace an existing destination with rename.
		if err := removeFile(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return fmt.Errorf("replace daemon metadata %s: %w", path, err)
		}
	}
	if err := os.Rename(temporary, path); err != nil {
		return fmt.Errorf("publish daemon metadata %s: %w", path, err)
	}
	return nil
}

// daemonIdentityOrLegacy is what a daemon reports about itself, or the
// legacy answer when an older daemon cannot report at all.
func daemonIdentityOrLegacy(ctx context.Context, metadata *client.DaemonMetadata) client.DaemonIdentity {
	observed, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	daemon, err := client.DaemonClientConnect(observed, *metadata)
	if err == nil {
		identity, err := requestDaemonIdentity(observed, daemon)
		daemon.Close()
		if err == nil {
			identity.Placement = daemonPlacement(metadata, &identity.Placement)
			return identity
		}
	}
	return client.DaemonIdentity{
		InstanceID:   "",
		BuildVersion: metadata.BuildVersion,
		Placement:    legacyPlacement(metadata.Pid),
	}
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
	if _, ok := daemon.Runtime.Engine(); !ok {
		return configuredPlacementHost{}, nil
	}
	placement, err := resolveContainerPlacement(ctx, daemon)
	if err != nil {
		return nil, err
	}
	switch resolved := placement.(type) {
	case containerPlacementContainer:
		return configuredPlacementContainer{spec: resolved.spec}, nil
	case containerPlacementHost:
		reason := resolved.reason
		return configuredPlacementHost{reason: &reason}, nil
	}
	panic("unknown container placement")
}

func loadDaemonConfig(ctx context.Context) (core.DaemonConfig, error) {
	return core.DaemonConfigLoadPlacement(core.ConfigPath())
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
	daemon, err := client.BuildIdentityParse(identity.BuildVersion)
	if err != nil {
		return nil, fmt.Errorf("parse daemon build version: %w", err)
	}
	this := client.ThisBuild()
	var runs bool
	if _, inContainer := identity.Placement.Container(); inContainer {
		runs = daemon.SameSource(*this)
	} else {
		runs = daemon.Equal(*this)
	}
	return &runs, nil
}

// hostDaemonLoginEnvironment reconstructs the account's login environment
// when this client runs inside a session (found false otherwise). A session
// is an overlay on that environment; rebuilding the base avoids keeping a
// second list of target, profile, harness and shell overrides here. The
// existing identity also covers older workers.
func hostDaemonLoginEnvironment() (environment map[string]string, found bool, err error) {
	_, inSession := os.LookupEnv("MJ_SESSION_ID")
	_, hasBashEnv := os.LookupEnv("MJ_SESSION_BASH_ENV")
	if inSession || hasBashEnv {
		environment, err := harnessinstall.Discover()
		if err != nil {
			return nil, false, fmt.Errorf("restore account login environment for the daemon: %w", err)
		}
		return environment, true, nil
	}
	return nil, false, nil
}

func configureHostDaemonEnvironment(command *exec.Cmd, loginEnvironment map[string]string, found bool) error {
	if found {
		environment := make([]string, 0, len(loginEnvironment))
		for name, value := range loginEnvironment {
			switch name {
			// Absence is significant too: a login script must not
			// redirect the instance selected by this client.
			case "MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE":
				continue
			}
			environment = append(environment, name+"="+value)
		}
		slices.Sort(environment)
		command.Env = environment
	} else if command.Env == nil {
		command.Env = os.Environ()
	}
	// Resolve against the client's cwd before the child changes directory.
	// ConfigDir/DataDir own override and named-instance precedence.
	config, err := absolutePath(core.ConfigDir())
	if err != nil {
		return err
	}
	data, err := absolutePath(core.DataDir())
	if err != nil {
		return err
	}
	command.Env = setCommandEnv(command.Env, "MJ_CONFIG_DIR", config)
	command.Env = setCommandEnv(command.Env, "MJ_DATA_DIR", data)
	if instance, ok := os.LookupEnv("MJ_INSTANCE"); ok {
		command.Env = setCommandEnv(command.Env, "MJ_INSTANCE", instance)
	}
	command.Dir = data
	return nil
}

// setCommandEnv is Command::env on an explicit environment list: name is
// set to value, replacing an earlier setting.
func setCommandEnv(environment []string, name, value string) []string {
	return append(removeCommandEnv(environment, name), name+"="+value)
}

// removeCommandEnv is Command::env_remove on an explicit environment list.
func removeCommandEnv(environment []string, name string) []string {
	kept := environment[:0:0]
	for _, entry := range environment {
		if key, _, _ := strings.Cut(entry, "="); key == name {
			continue
		}
		kept = append(kept, entry)
	}
	return kept
}

// absolutePath is std::path::absolute: a relative path is joined to the
// current directory without touching the filesystem. `.` components and
// repeated separators go; `..` components stay, as POSIX resolution would
// need the filesystem to remove them.
func absolutePath(path string) (string, error) {
	if path == "" {
		return "", errors.New("cannot make an empty path absolute")
	}
	if !filepath.IsAbs(path) {
		directory, err := os.Getwd()
		if err != nil {
			return "", err
		}
		path = directory + string(filepath.Separator) + path
	}
	if runtime.GOOS == "windows" {
		return filepath.Abs(path)
	}
	var kept []string
	for _, component := range strings.Split(path, "/") {
		if component == "" || component == "." {
			continue
		}
		kept = append(kept, component)
	}
	absolute := "/" + strings.Join(kept, "/")
	if strings.HasSuffix(path, "/") && absolute != "/" {
		absolute += "/"
	}
	return absolute, nil
}

// launchDaemon starts `mj daemon-run` where `[daemon] runtime` says: a
// detached process on this computer, or a process in this instance's daemon
// container. Either way the caller follows the launched process until the
// daemon answers.
func launchDaemon(ctx context.Context, logPath string, priorUnpublished *client.ContainerDaemonRecord) (*launchedDaemon, error) {
	retireLegacyNode()
	// An unreadable file is the daemon's to report: it refuses to start and
	// says why, which is the answer this launch then shows.
	daemon, err := loadDaemonConfig(ctx)
	if err != nil {
		daemon = core.DefaultDaemonConfig()
	}
	placement, err := resolvePlacement(ctx, &daemon)
	if err != nil {
		return nil, err
	}
	switch resolved := placement.(type) {
	case configuredPlacementHost:
		if resolved.reason != nil {
			engine, ok := daemon.Runtime.Engine()
			if !ok {
				engine = "a container"
			}
			startupNotice(fmt.Sprintf("Settings run the Mjolnir daemon in %s, but %s. It runs on this computer instead.", engine, *resolved.reason))
		}
	case configuredPlacementContainer:
		launched, err := launchContainerDaemon(ctx, resolved.spec, priorUnpublished)
		if err != nil {
			return nil, err
		}
		slog.Info("launched the daemon in its container",
			"container", launched.containerName, "port", launched.metadata.Address.Port())
		return &launchedDaemon{pid: 0, logOffset: 0, container: launched}, nil
	}
	if priorUnpublished != nil {
		if err := client.RemoveContainerOwnerIfMatching(priorUnpublished.Address, priorUnpublished.Token); err != nil {
			return nil, err
		}
	}
	// Everything the daemon writes from here on belongs to this launch.
	var logOffset uint64
	if info, err := os.Stat(logPath); err == nil {
		logOffset = uint64(info.Size())
	}
	command, err := hostDaemonCommand()
	if err != nil {
		return nil, err
	}
	// This switch makes the invoking development client authoritative. It
	// has no meaning inside the persistent daemon or its child processes.
	for _, name := range []string{
		devRestartStaleDaemonEnv,
		"MJ_UPGRADE_RESUME_FILE",
		client.DaemonContainerEnv,
		// A host daemon picks its own endpoint and token; only a container
		// launcher chooses them.
		"MJ_DAEMON_LISTEN",
		"MJ_DAEMON_TOKEN",
	} {
		command.Env = removeCommandEnv(command.Env, name)
	}
	pid, err := core.SpawnDetached(command, logPath)
	if err != nil {
		return nil, err
	}
	return &launchedDaemon{pid: pid, logOffset: logOffset}, nil
}

func hostDaemonCommand() (*exec.Cmd, error) {
	executable, err := daemonLaunchExecutable()
	if err != nil {
		return nil, err
	}
	exitDir, err := absolutePath(core.DaemonExitsDir(core.DataDir()))
	if err != nil {
		return nil, err
	}
	args := []string{"daemon-wait-exit", "--exit-dir", exitDir, "--"}

	loginEnvironment, found, err := hostDaemonLoginEnvironment()
	if err != nil {
		return nil, err
	}
	systemdRun, runtimeDir, scoped := daemonScopeLauncher(loginEnvironment, found)
	command := exec.Command(executable)
	if err := configureHostDaemonEnvironment(command, loginEnvironment, found); err != nil {
		return nil, err
	}

	if scoped {
		// Keep the waiter outside the scope so it can record a scope-level
		// kill. Resolve PATH before the launcher changes into the daemon
		// data directory.
		instance, ok := core.InstanceName()
		if !ok {
			instance = "default"
		}
		launcher, err := absolutePath(systemdRun)
		if err != nil {
			return nil, err
		}
		args = append(args, launcher, "--user", "--scope", "--quiet", "--collect",
			"--description=Mjolnir daemon ("+instance+")", "--", "/proc/self/exe", "daemon-run")
		command.Env = setCommandEnv(command.Env, "XDG_RUNTIME_DIR", runtimeDir)
	} else {
		args = append(args, executable, "daemon-run")
	}
	command.Args = append([]string{executable}, args...)
	if runtime.GOOS != "windows" {
		command.Args[0] = "mj"
	}
	return command, nil
}

// daemonScopeLauncher (Linux only) is the systemd-run and runtime directory
// that start the daemon in its own user scope, or false when the daemon
// stays in this client's cgroup.
func daemonScopeLauncher(loginEnvironment map[string]string, found bool) (systemdRun, runtimeDir string, ok bool) {
	if runtime.GOOS != "linux" {
		return "", "", false
	}
	// Check the environment systemd-run will receive, including when a
	// session client restores the account's login environment.
	variable := func(name string) (string, bool) {
		if found {
			value, ok := loginEnvironment[name]
			return value, ok
		}
		return os.LookupEnv(name)
	}
	uid := syscall.Geteuid()
	// A restored login environment can omit the runtime directory PAM
	// assigns.
	runtimeDir, present := variable("XDG_RUNTIME_DIR")
	if !present {
		runtimeDir = fmt.Sprintf("/run/user/%d", uid)
	}
	if !pathExists(filepath.Join(runtimeDir, "systemd", "private")) {
		return "", "", false
	}
	userService := fmt.Sprintf("user@%d.service", uid)
	cgroups, err := os.ReadFile("/proc/self/cgroup")
	if err != nil {
		return "", "", false
	}
	for _, line := range rustLines(string(cgroups)) {
		parts := strings.SplitN(line, ":", 3)
		if len(parts) < 3 {
			continue
		}
		if slices.Contains(strings.Split(parts[2], "/"), userService) {
			return "", "", false
		}
	}
	// #1584: cgroup v2 checks write access at the common ancestor, so
	// rootless Podman execs need to start inside the delegated user subtree.
	path, pathOK := variable("PATH")
	systemdRun, foundRun := harnessinstall.FindExecutableOnPath("systemd-run", path, pathOK)
	if !foundRun {
		return "", "", false
	}
	// logind stops the user manager at logout without lingering. Keep the
	// detached daemon and its bare workers in their original cgroup then.
	user, err := harnessinstall.AccountName()
	if err != nil {
		return "", "", false
	}
	// logind C-escapes account names for linger filenames. Check names that
	// appear verbatim there so an escaped filename cannot match another
	// user.
	for index := 0; index < len(user); index++ {
		b := user[index]
		if b < ' ' || b > '~' || b == '/' || b == '\\' || b == '\'' || b == '"' {
			return "", "", false
		}
	}
	if info, err := os.Stat(filepath.Join("/var/lib/systemd/linger", user)); err != nil || !info.Mode().IsRegular() {
		return "", "", false
	}
	return systemdRun, runtimeDir, true
}

// retireLegacyNode retires the `mj node` that releases 2.66.0 through
// 2.70.x started beside every host daemon. That node exits when it loses
// its daemon and finds no `node-endpoint.json`, so removing the file here,
// after the old daemon is gone and before its replacement starts, retires
// it. Delete this once no supported upgrade starts from those releases.
func retireLegacyNode() {
	data := core.DataDir()
	for _, name := range []string{"node-endpoint.json", "node-id", "node.lock"} {
		if err := removeFile(filepath.Join(data, name)); err != nil && !errors.Is(err, fs.ErrNotExist) {
			slog.Warn("could not remove a file of the retired node", "error", err, "file", name)
		}
	}
}

func randomToken() (string, error) {
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		return "", fmt.Errorf("generate the daemon token: %v", err)
	}
	return core.LowerHex(bytes), nil
}

func checkStoreReadiness(ctx context.Context) error {
	return store.CheckReadCompatibility()
}

func pingDaemon(ctx context.Context, daemon *client.DaemonClient) error {
	reply, err := daemon.RequestSingle(ctx, client.DaemonActionPing{})
	if err != nil {
		return err
	}
	if _, ok := reply.(client.DaemonReplyPong); !ok {
		return fmt.Errorf("unexpected startup reply %s", client.DaemonReplyDebug(reply))
	}
	return nil
}

// WaitForWebAccess waits for the web access publication that owns all local
// API connection details: advertised URL, listener address, and certificate
// pin. It answers only client.WebViewerAccessReady.
func WaitForWebAccess(ctx context.Context, daemon *client.DaemonClient) (client.WebViewerAccess, error) {
	return waitForWebAccessWithTimeout(ctx, daemon, startTimeout)
}

// errWebViewerTimeout is the error of a web viewer that did not become
// ready in time (Rust's Elapsed under its context).
var errWebViewerTimeout = errors.New("timed out waiting for the web viewer to become ready: deadline has elapsed")

func waitForWebAccessWithTimeout(ctx context.Context, daemon *client.DaemonClient, timeout time.Duration) (client.WebViewerAccess, error) {
	timed, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	elapsed := func(err error) error {
		if ctx.Err() == nil && errors.Is(timed.Err(), context.DeadlineExceeded) {
			return errWebViewerTimeout
		}
		return err
	}
	for {
		access, err := daemon.WebAccess(timed)
		if err != nil {
			return nil, elapsed(err)
		}
		switch access := access.(type) {
		case client.WebViewerAccessReady:
			return access, nil
		case client.WebViewerAccessStarting:
			if err := sleepContext(timed, client.RetryDelay); err != nil {
				return nil, elapsed(err)
			}
		case client.WebViewerAccessFailed:
			return nil, fmt.Errorf("the web viewer failed to start: %s", access.Message)
		case client.WebViewerAccessUnavailable:
			return nil, fmt.Errorf("the web viewer is unavailable: %s", access.Message)
		default:
			return nil, fmt.Errorf("unexpected web viewer access %s", access)
		}
	}
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
	daemon, err := client.BuildIdentityParse(metadata.BuildVersion)
	if err != nil {
		return daemonBuild{}, fmt.Errorf("parse daemon build version: %w", err)
	}
	this := client.ThisBuild()
	// A daemon in a container runs the image's `mj`, whose file times are
	// the image's: the same version and revision is the same build.
	var order client.DaemonBuildOrder
	if _, inContainer := placement.Container(); inContainer && daemon.SameSource(*this) {
		order = client.DaemonBuildOrderSame
	} else {
		order = client.CompareDaemonBuild(&daemon, this)
	}
	release := daemon.CompareRelease(*this)
	switch {
	case release < 0:
		release = -1
	case release > 0:
		release = 1
	}
	return daemonBuild{release: release, order: order}, nil
}

// daemonBuildNotice is the sentence naming both builds, followed by what
// this client does.
func daemonBuildNotice(metadata *client.DaemonMetadata, decision string) string {
	daemon := metadata.BuildVersion
	if build, err := client.BuildIdentityParse(metadata.BuildVersion); err == nil {
		daemon = build.Describe()
	}
	return fmt.Sprintf("The running daemon is build %s; this client is build %s. %s", daemon, client.ThisBuild().Describe(), decision)
}

// keptDaemonNoticeSaid is whether keptDaemonNotice has spoken.
var keptDaemonNoticeSaid atomic.Bool

// keptDaemonNotice says once per process that this client keeps using a
// daemon it will not replace. Every command, and every dashboard action,
// comes through startup, and the reason does not change between them.
func keptDaemonNotice(message string) {
	if !keptDaemonNoticeSaid.Swap(true) {
		startupNotice(message)
	} else {
		slog.Info(message)
	}
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
	record, err := client.ReadContainerOwner()
	if err != nil {
		return nil, err
	}
	if record != nil && record.Matches(metadata) {
		return record, nil
	}
	return nil, nil
}

func missingContainerOwnerReason(err error) string {
	detail := ""
	if err != nil {
		detail = " (" + err.Error() + ")"
	}
	return "daemon.json records a container daemon (pid: 0) without a matching daemon-container.json " +
		"owner record" + detail + ". Mjolnir cannot verify whether it exited. Verify that no container " +
		"daemon is running, then run `mj daemon stop --force` to clear the stale record."
}

func probeDaemonRelease(ctx context.Context, metadata *client.DaemonMetadata) (daemonRelease, error) {
	owner, decided := func() (*client.ContainerDaemonRecord, *daemonRelease) {
		if current, err := client.ReadMetadataAny(); err == nil && !sameDaemonEndpoint(&current, metadata) {
			return nil, &daemonRelease{kind: daemonReleaseReplaced}
		}
		if metadata.Pid == 0 {
			record, err := matchingContainerOwner(metadata)
			if err == nil && record != nil {
				return record, nil
			}
			return nil, &daemonRelease{kind: daemonReleaseUnknown, reason: missingContainerOwnerReason(err)}
		}
		guard, err := controller.ControllerStoreGuardTryAcquire()
		switch {
		case err != nil:
			return nil, &daemonRelease{kind: daemonReleaseUnknown, reason: err.Error()}
		case guard == nil:
			return nil, &daemonRelease{kind: daemonReleaseStillOwned}
		}
		// The daemon released the store and is exiting. Nothing is
		// signalled here, so a PID that still exists is waited for even when
		// its command line is already gone; the caller's timeout bounds the
		// wait.
		owned := hostPIDOf(metadata.Pid) != hostPIDNotDaemon
		guard.Drop()
		if owned {
			return nil, &daemonRelease{kind: daemonReleaseStillOwned}
		}
		return nil, &daemonRelease{kind: daemonReleaseExited}
	}()
	if decided != nil {
		return *decided, nil
	}
	status := probeContainerDaemon(ctx, owner.Runtime, owner.ContainerID)
	switch status.kind {
	case containerDaemonRunning:
		return daemonRelease{kind: daemonReleaseStillOwned}, nil
	case containerDaemonExited:
		return daemonRelease{kind: daemonReleaseExited}, nil
	}
	return daemonRelease{kind: daemonReleaseUnknown, reason: status.reason}, nil
}

const unreadableContainerOwnerRecovery = "Verify that no container daemon is running, then run `mj daemon stop --force` to clear the unreadable daemon-container.json"

func unpublishedContainer(ctx context.Context) (existingDaemon, error) {
	record, err := client.ReadContainerOwner()
	if err != nil {
		return nil, fmt.Errorf("%s: %w", unreadableContainerOwnerRecovery, err)
	}
	if record == nil {
		return existingDaemonAbsent{}, nil
	}
	return existingDaemonPendingContainer{record: *record}, nil
}

func unanswered(ctx context.Context, metadata client.DaemonMetadata) (existingDaemon, error) {
	release, err := probeDaemonRelease(ctx, &metadata)
	if err != nil {
		return nil, err
	}
	switch release.kind {
	case daemonReleaseExited:
		if metadata.Pid == 0 {
			return existingDaemonExitedContainer{metadata: metadata}, nil
		}
		return unpublishedContainer(ctx)
	case daemonReleaseReplaced:
		return existingDaemonReplaced{}, nil
	case daemonReleaseStillOwned:
		return existingDaemonUnanswered{metadata: metadata}, nil
	}
	return nil, errors.New(release.reason)
}

func decideExistingDaemon(ctx context.Context) (existingDaemon, error) {
	metadata, err := client.ReadMetadataAny()
	if err != nil {
		if _, stopped := client.DaemonNotRunningOf(err); !stopped {
			guard, guardErr := controller.ControllerStoreGuardTryAcquire()
			if guardErr != nil {
				return nil, fmt.Errorf("check controller ownership after failing to read daemon.json: %w", guardErr)
			}
			if guard == nil {
				return nil, fmt.Errorf("the controller store is owned but daemon.json is unreadable: %w", err)
			}
			guard.Drop()
		}
		return unpublishedContainer(ctx)
	}
	if err := client.EnsureSupportedDaemonProtocol(metadata); err != nil {
		verdict, uerr := unanswered(ctx, metadata)
		if uerr != nil {
			return nil, uerr
		}
		if _, still := verdict.(existingDaemonUnanswered); still {
			return nil, err
		}
		return verdict, nil
	}
	if metadata.ProtocolVersion < client.ProtocolVersion {
		var placement client.DaemonPlacement
		if metadata.Pid == 0 {
			// A reachable pre-tracking container can name its engine via
			// Identity. Its endpoint alone cannot establish ownership on
			// exit.
			placement = daemonIdentityOrLegacy(ctx, &metadata).Placement
		} else {
			placement = legacyPlacement(metadata.Pid)
		}
		if _, inContainer := placement.Container(); !inContainer || placement.Runtime == nil {
			verdict, err := unanswered(ctx, metadata)
			if err != nil {
				return nil, err
			}
			if _, still := verdict.(existingDaemonUnanswered); !still {
				return verdict, nil
			}
		}
		build, err := daemonBuildOf(&metadata, &placement)
		if err != nil {
			return nil, err
		}
		if build.daemonIsNewer() {
			return nil, fmt.Errorf("refusing to replace a newer daemon with this client: %s",
				daemonBuildNotice(&metadata, "Run the daemon's build instead."))
		}
		return existingDaemonReplace{metadata: metadata, placement: placement}, nil
	}
	daemon, err := client.DaemonClientConnect(ctx, metadata)
	if err != nil {
		return unanswered(ctx, metadata)
	}
	// Discovery has no outer readiness clock. A silent endpoint must let
	// prepareExistingDaemon check ownership and update its wait notice.
	pinging, cancel := context.WithTimeout(ctx, 2*time.Second)
	err = pingDaemon(pinging, daemon)
	cancel()
	if err != nil {
		daemon.Close()
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		return unanswered(ctx, metadata)
	}
	// This protocol reports its own placement. A failed discovery cannot
	// safely decide whether to use or replace it, and may have interrupted
	// the connection.
	identity, err := requestDaemonIdentity(ctx, daemon)
	if err != nil {
		daemon.Close()
		return nil, fmt.Errorf("read the running daemon's identity: %w", err)
	}
	placement := daemonPlacement(&metadata, &identity.Placement)
	build, err := daemonBuildOf(&metadata, &placement)
	if err != nil {
		daemon.Close()
		return nil, err
	}
	// A container daemon's store is in its volume; the host store says
	// nothing about it. The daemon reports its own migration failure.
	if _, inContainer := placement.Container(); !inContainer {
		if err := checkStoreReadiness(ctx); err != nil {
			var mismatch core.StoreSchemaMismatch
			needsMigration := false
			if errors.As(err, &mismatch) {
				_, needsMigration = mismatch.Reason.(core.StoreSchemaMismatchReasonNeedsMigration)
			}
			daemon.Close()
			if !needsMigration {
				return nil, err
			}
			if build.daemonIsNewer() {
				return nil, fmt.Errorf("a newer daemon has not completed database initialization: %w", err)
			}
			return existingDaemonReplace{metadata: metadata, placement: placement}, nil
		}
	}
	switch build.order {
	case client.DaemonBuildOrderSame:
		// Settings choose where the daemon runs; the next client of the same
		// build moves it there through the ordinary handoff, which leaves
		// workers running.
		running, runningInContainer := placement.Container()
		// A configuration this client cannot read leaves the daemon be.
		configuredKnown := false
		configured, configuredInContainer := "", false
		if config, err := loadDaemonConfig(ctx); err == nil {
			resolved, err := resolvePlacement(ctx, &config)
			if err != nil {
				slog.Warn("could not resolve where Settings run the daemon", "error", err.Error())
			} else {
				configuredKnown = true
				configured, configuredInContainer = configuredPlacementContainerName(resolved)
			}
		}
		if configuredKnown && (configuredInContainer != runningInContainer || configured != running) {
			var notice string
			if configuredInContainer {
				notice = fmt.Sprintf("Settings run the Mjolnir daemon in container %s; moving it there.", configured)
			} else {
				notice = "Settings run the Mjolnir daemon on this computer; moving it out of its container."
			}
			daemon.Close()
			return existingDaemonReplace{metadata: metadata, placement: placement, notice: &notice}, nil
		}
	case client.DaemonBuildOrderOlder:
		notice := daemonBuildNotice(&metadata, "This client's build is newer; replacing the daemon.")
		daemon.Close()
		return existingDaemonReplace{metadata: metadata, placement: placement, notice: &notice}, nil
	// Same release, protocol and readable store: the older client can use
	// the newer daemon, and must not replace it.
	case client.DaemonBuildOrderNewer:
		keptDaemonNotice(daemonBuildNotice(&metadata,
			"The daemon's build is newer, so it keeps running and this client uses it."))
	case client.DaemonBuildOrderUnknown:
		keptDaemonNotice(daemonBuildNotice(&metadata,
			"Mjolnir cannot tell which build is newer, so the daemon keeps running and this "+
				"client uses it. Run `mj daemon restart` to replace it with this build."))
	}
	return existingDaemonUse{daemon: daemon}, nil
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
	asked := time.Now()
	noticeAt := asked.Add(startNoticeDelay)
	for {
		existing, err := decideExistingDaemon(ctx)
		if err != nil {
			return preparedDaemon{}, err
		}
		switch existing := existing.(type) {
		case existingDaemonAbsent:
			return preparedDaemon{}, nil
		case existingDaemonUse:
			return preparedDaemon{ready: existing.daemon}, nil
		case existingDaemonExitedContainer:
			if err := clearExitedDaemon(&existing.metadata); err != nil {
				return preparedDaemon{}, err
			}
		case existingDaemonPendingContainer:
			adoption, err := adoptContainerDaemon(ctx, &existing.record)
			if err != nil {
				return preparedDaemon{}, err
			}
			if adoption == containerAdoptionExited {
				record := existing.record
				return preparedDaemon{priorUnpublished: &record}, nil
			}
		case existingDaemonReplaced:
		case existingDaemonReplace:
			if existing.notice != nil {
				startupNotice(*existing.notice)
			} else {
				slog.Info("the daemon is an older protocol or release, or its store needs a migration; replacing it",
					"daemon_protocol", existing.metadata.ProtocolVersion,
					"client_protocol", client.ProtocolVersion,
					"daemon_build", existing.metadata.BuildVersion)
			}
			if err := replaceDaemon(ctx, startup, &existing.metadata, &existing.placement); err != nil {
				return preparedDaemon{}, err
			}
		case existingDaemonUnanswered:
			if !time.Now().Before(noticeAt) {
				waited := int64(time.Since(asked) / time.Second)
				slog.Info("the recorded daemon has not answered and has not exited",
					"pid", existing.metadata.Pid,
					"address", existing.metadata.Address.String(),
					"waited_s", waited)
				startupNotice(fmt.Sprintf("The Mjolnir daemon at %s has not answered for %ds and is still running; waiting for it to answer or exit.",
					existing.metadata.Address, waited))
				noticeAt = time.Now().Add(startupNoticeInterval())
			}
			if err := sleepContext(ctx, client.RetryDelay); err != nil {
				return preparedDaemon{}, err
			}
		}
	}
}

func pendingContainerMetadata(record *client.ContainerDaemonRecord) client.DaemonMetadata {
	protocol := client.ProtocolVersion
	if record.ProtocolVersion != nil {
		protocol = *record.ProtocolVersion
	}
	runtime := record.Runtime
	return client.DaemonMetadata{
		ProtocolVersion: protocol,
		Pid:             0,
		Address:         record.Address,
		Token:           record.Token,
		StartedAt:       record.StartedAt,
		BuildVersion:    "",
		InstanceID:      "",
		Placement: client.DaemonPlacement{
			Kind:    client.DaemonPlacementKindContainer,
			Name:    record.ContainerName,
			Runtime: &runtime,
		},
	}
}

func connectReadyContainer(ctx context.Context, metadata *client.DaemonMetadata) (*client.DaemonClient, error) {
	daemon, err := client.DaemonClientConnect(ctx, *metadata)
	if err != nil {
		return nil, err
	}
	if err := pingDaemon(ctx, daemon); err != nil {
		daemon.Close()
		return nil, err
	}
	return daemon, nil
}

func requestDaemonIdentity(ctx context.Context, daemon *client.DaemonClient) (client.DaemonIdentity, error) {
	reply, err := daemon.RequestSingle(ctx, client.DaemonActionIdentity{})
	if err != nil {
		return client.DaemonIdentity{}, err
	}
	if identity, ok := reply.(client.DaemonReplyIdentity); ok {
		return identity.Identity, nil
	}
	return client.DaemonIdentity{}, fmt.Errorf("unexpected daemon identity reply %s", client.DaemonReplyDebug(reply))
}

func containerDaemonMayBeAlive(ctx context.Context, runtime core.DaemonRuntime, id string) bool {
	return probeContainerDaemon(ctx, runtime, id).kind != containerDaemonExited
}

type containerAdoption int

const (
	containerAdoptionPublished containerAdoption = iota
	containerAdoptionExited
)

func adoptContainerDaemon(ctx context.Context, record *client.ContainerDaemonRecord) (containerAdoption, error) {
	switch status := probeContainerDaemon(ctx, record.Runtime, record.ContainerID); status.kind {
	case containerDaemonExited:
		return containerAdoptionExited, nil
	case containerDaemonUnknown:
		return 0, errors.New(status.reason)
	}

	metadata := pendingContainerMetadata(record)
	outcome := waitForReadyDaemon(ctx,
		func(ctx context.Context) bool {
			return containerDaemonMayBeAlive(ctx, record.Runtime, record.ContainerID)
		},
		func(ctx context.Context) (client.DaemonIdentity, error) {
			daemon, err := connectReadyContainer(ctx, &metadata)
			if err != nil {
				return client.DaemonIdentity{}, err
			}
			defer daemon.Close()
			return requestDaemonIdentity(ctx, daemon)
		},
		func(waited time.Duration) {
			startupNotice(fmt.Sprintf("Mjolnir daemon container %s has been starting for %ds; still waiting.",
				record.ContainerName, int64(waited/time.Second)))
		},
	)
	switch outcome.kind {
	case startupOutcomeReady:
		identity := outcome.ready
		published := metadata
		published.Placement = daemonPlacement(&metadata, &identity.Placement)
		published.BuildVersion = identity.BuildVersion
		published.InstanceID = identity.InstanceID
		if err := writeLauncherMetadata(&published); err != nil {
			return 0, err
		}
		return containerAdoptionPublished, nil
	case startupOutcomeExited:
		return containerAdoptionExited, nil
	}
	return 0, errors.New(startupWaitFailure("container "+record.ContainerName, outcome.lastError, outcome.probePending))
}

// ensureConfigLoads refuses to stop a daemon when the one that would replace
// it cannot read the configuration and would exit at startup, leaving the
// instance stopped. action finishes "The Mjolnir daemon was not ...".
func ensureConfigLoads(configPath, action string) error {
	if _, err := core.ConfigLoadFrom(configPath); err != nil {
		return fmt.Errorf("%s. The Mjolnir daemon was not %s; fix the configuration and try again.", err, action)
	}
	return nil
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
	if err := core.EnsureMayControlStore(core.DataDir(), "stop the Mjolnir daemon"); err != nil {
		return false, err
	}
	startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
	if err != nil {
		return false, err
	}
	defer startup.release()
	fallback := stopFallbackGraceful
	if force {
		fallback = stopFallbackForce
	}
	var stopped bool
	metadata, err := client.ReadMetadataAny()
	switch {
	case err == nil:
		if stopped, err = stopDaemon(ctx, startup, &metadata, fallback); err != nil {
			return false, err
		}
	case isDaemonNotRunning(err):
		record, ownerErr := client.ReadContainerOwner()
		switch {
		case ownerErr == nil && record != nil:
			if stopped, err = stopPendingContainer(ctx, startup, record, force); err != nil {
				return false, err
			}
		case ownerErr == nil:
			stopped = false
		case force:
			if err := clearUnreadableContainerOwner(); err != nil {
				return false, err
			}
			stopped = true
		default:
			return false, fmt.Errorf("%s: %w", unreadableContainerOwnerRecovery, ownerErr)
		}
	default:
		return false, err
	}
	retireLegacyNode()
	return stopped, nil
}

// isDaemonNotRunning is Rust's `daemon_not_running(&error).is_some()`.
func isDaemonNotRunning(err error) bool {
	_, stopped := client.DaemonNotRunningOf(err)
	return stopped
}

func clearUnreadableContainerOwner() error {
	path := client.ContainerOwnerPath()
	if err := removeFile(path); err != nil {
		return fmt.Errorf("remove %s: %w", path, err)
	}
	fmt.Fprintf(os.Stderr, "Cleared the unreadable container daemon record in %s.\n", path)
	return nil
}

func stopPendingContainer(ctx context.Context, startup *daemonStartGuard, record *client.ContainerDaemonRecord, force bool) (bool, error) {
	status := probeContainerDaemon(ctx, record.Runtime, record.ContainerID)
	switch {
	case status.kind == containerDaemonExited && !status.containerRunning:
		// A stopped or absent container cannot start a delayed exec.
		if err := client.RemoveContainerOwnerIfMatching(record.Address, record.Token); err != nil {
			return false, err
		}
		return false, nil
	case (status.kind == containerDaemonRunning || status.kind == containerDaemonExited) && force:
		if err := stopContainer(ctx, record.Runtime, record.ContainerID); err != nil {
			return false, err
		}
		if err := client.RemoveContainerOwnerIfMatching(record.Address, record.Token); err != nil {
			return false, err
		}
		return true, nil
	case status.kind == containerDaemonExited:
		// Keep credentials while the container runs so a delayed exec
		// remains discoverable and any same-target retry reuses the same
		// token.
		return false, nil
	case status.kind == containerDaemonUnknown:
		return false, errors.New(status.reason)
	}
	metadata := pendingContainerMetadata(record)
	return stopDaemon(ctx, startup, &metadata, stopFallbackGraceful)
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
	if err := core.EnsureMayControlStore(core.DataDir(), "restart the Mjolnir daemon"); err != nil {
		return RestartedDaemon{}, err
	}
	// A daemon that cannot read its configuration exits at once. Find that
	// out while the old one still runs.
	if err := ensureConfigLoads(core.ConfigPath(), "restarted"); err != nil {
		return RestartedDaemon{}, err
	}
	startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
	if err != nil {
		return RestartedDaemon{}, err
	}
	defer startup.release()
	for attempt := 1; attempt <= restartAttempts; attempt++ {
		if _, err := client.ReadMetadataAny(); err != nil && isDaemonNotRunning(err) {
			record, err := client.ReadContainerOwner()
			if err != nil {
				return RestartedDaemon{}, err
			}
			if record != nil {
				if _, err := adoptContainerDaemon(ctx, record); err != nil {
					return RestartedDaemon{}, err
				}
			}
		}
		if metadata, err := client.ReadMetadataAny(); err == nil {
			// An explicit restart may replace a build it cannot order, but
			// never a newer one: that is a downgrade, which `mj daemon stop`
			// makes deliberate.
			identity := daemonIdentityOrLegacy(ctx, &metadata)
			build, err := daemonBuildOf(&metadata, &identity.Placement)
			if err != nil {
				return RestartedDaemon{}, err
			}
			if build.daemonIsNewer() {
				return RestartedDaemon{}, errors.New(daemonBuildNotice(&metadata,
					"The daemon's build is newer, so it was not restarted. Run `mj daemon "+
						"restart` from the daemon's build, or run `mj daemon stop` first to "+
						"start this older build instead."))
			}
			if err := ensureContainerOwnerRecorded(ctx, startup, &metadata, &identity.Placement); err != nil {
				return RestartedDaemon{}, err
			}
			// Keep restart's ability to stop an unresponsive tracked daemon.
			// Clearing an untracked container record requires explicit stop
			// --force.
			if _, err := stopDaemon(ctx, startup, &metadata, stopFallbackRestart); err != nil {
				return RestartedDaemon{}, err
			}
		}
		daemon, err := connectOrStartHolding(ctx, startup)
		if err != nil {
			return RestartedDaemon{}, err
		}
		status, err := daemon.Status(ctx)
		if err != nil {
			daemon.Close()
			return RestartedDaemon{}, err
		}
		identity, err := daemon.Identity(ctx)
		daemon.Close()
		if err != nil {
			return RestartedDaemon{}, err
		}
		runsThisBuild, err := daemonRunsThisBuild(&identity)
		if err != nil {
			return RestartedDaemon{}, err
		}
		if runsThisBuild == nil || *runsThisBuild || attempt == restartAttempts {
			return restartVerdict(status.Pid, &identity, runsThisBuild)
		}
	}
	panic("the final attempt always returns a verdict")
}

// restartVerdict turns one build-identity answer into the restart's result.
//
// This is separate from the restart itself so the outcome can be exercised
// without starting daemons. It is also the decision the command used to
// skip: reporting success on a changed process id alone is what let a
// restart announce a daemon running code the caller had just replaced.
func restartVerdict(pid uint32, identity *client.DaemonIdentity, runsThisBuild *bool) (RestartedDaemon, error) {
	if runsThisBuild != nil && !*runsThisBuild {
		daemon := identity.BuildVersion
		if build, err := client.BuildIdentityParse(identity.BuildVersion); err == nil {
			daemon = build.Describe()
		}
		return RestartedDaemon{}, fmt.Errorf("Mjolnir daemon %d runs build %s, not this build (%s). "+
			"Another attached client started it. Close clients from the "+
			"previous build, then run `mj daemon restart` again.",
			pid, daemon, client.ThisBuild().Describe())
	}
	return RestartedDaemon{Pid: pid, RunsThisBuild: runsThisBuild}, nil
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
//
// The probe and the liveness check run beside the clocks, as Rust's
// select! polls them together; ending the wait cancels the context they
// were given, as dropping their futures does. When several branches are
// ready at once the order is Rust's `biased` one: ready, exited, notice,
// timeout. An answer that arrives after the wait ended is closed if it can
// be.
func waitForReadyDaemon[T any](
	ctx context.Context,
	isAlive func(ctx context.Context) bool,
	probe func(ctx context.Context) (T, error),
	notice func(waited time.Duration),
) startupOutcome[T] {
	started := time.Now()
	waiting, cancel := context.WithCancel(ctx)
	defer cancel()

	// state is the probe's progress, read once when the wait times out.
	var state struct {
		sync.Mutex
		lastError error
		pending   bool
		decided   bool
	}
	ready := make(chan T)
	go func() {
		for {
			state.Lock()
			if state.decided {
				state.Unlock()
				return
			}
			state.pending = true
			state.Unlock()
			value, err := probe(waiting)
			if err == nil {
				select {
				case ready <- value:
				case <-waiting.Done():
					closeIfCloser(value)
				}
				return
			}
			state.Lock()
			if state.decided {
				state.Unlock()
				return
			}
			state.lastError = err
			state.pending = false
			state.Unlock()
			if sleepContext(waiting, client.RetryDelay) != nil {
				return
			}
		}
	}()
	exited := make(chan struct{})
	go func() {
		for {
			if !isAlive(waiting) {
				if waiting.Err() == nil {
					close(exited)
				}
				return
			}
			if sleepContext(waiting, time.Second) != nil {
				return
			}
		}
	}()
	noticeTimer := time.NewTimer(startNoticeDelay)
	defer noticeTimer.Stop()
	timeout := time.NewTimer(startTimeout)
	defer timeout.Stop()
	noticed := false
	stillStarting := func() startupOutcome[T] {
		state.Lock()
		state.decided = true
		outcome := startupOutcome[T]{kind: startupOutcomeStillStarting, lastError: state.lastError, probePending: state.pending}
		state.Unlock()
		return outcome
	}
	for {
		// biased: a ready answer wins over an exit, an exit over the clocks.
		select {
		case value := <-ready:
			return startupOutcome[T]{kind: startupOutcomeReady, ready: value}
		default:
		}
		select {
		case <-exited:
			return startupOutcome[T]{kind: startupOutcomeExited}
		default:
		}
		if !noticed {
			select {
			case <-noticeTimer.C:
				noticed = true
				notice(time.Since(started))
				continue
			default:
			}
		}
		select {
		case <-timeout.C:
			return stillStarting()
		default:
		}
		noticeChannel := noticeTimer.C
		if noticed {
			noticeChannel = nil
		}
		select {
		case value := <-ready:
			return startupOutcome[T]{kind: startupOutcomeReady, ready: value}
		case <-exited:
			return startupOutcome[T]{kind: startupOutcomeExited}
		case <-noticeChannel:
			noticed = true
			notice(time.Since(started))
		case <-timeout.C:
			// The other branches are checked again first: a probe that
			// answered at the bound still wins.
			select {
			case value := <-ready:
				return startupOutcome[T]{kind: startupOutcomeReady, ready: value}
			case <-exited:
				return startupOutcome[T]{kind: startupOutcomeExited}
			default:
			}
			return stillStarting()
		case <-ctx.Done():
			return stillStarting()
		}
	}
}

// closeIfCloser drops a value the way Rust drops an abandoned future's
// result: a connection is closed.
func closeIfCloser(value any) {
	if closer, ok := value.(interface{ Close() }); ok {
		closer.Close()
	}
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
	if d.container != nil {
		return containerDaemonMayBeAlive(ctx, d.container.runtime, d.container.containerID)
	}
	return client.ProcessIsAlive(d.pid)
}

// outputSinceLaunch includes process diagnostics and detached stderr, and
// names where they came from. Failures before logger initialization have
// only stderr; later failures also have a process log.
func (d *launchedDaemon) outputSinceLaunch(ctx context.Context, logPath string) (path string, output string) {
	if d.container != nil {
		program, ok := d.container.runtime.Engine()
		if !ok {
			panic("a daemon container has an engine")
		}
		log := containerLogTail(ctx, program, d.container.containerID)
		return "container " + d.container.containerName, log
	}
	path, output, err := func() (string, string, error) {
		directory := filepath.Dir(logPath)
		if directory == logPath {
			return "", "", errors.New("daemon stderr path has no parent")
		}
		diagnostics, exit, err := launchDiagnostics(directory, d.pid)
		if err != nil {
			return "", "", err
		}
		var output strings.Builder
		if diagnostics != nil {
			tail, err := launchLogTail(*diagnostics, 0)
			if err != nil {
				return "", "", err
			}
			output.WriteString(tail)
		}
		stderr, err := launchLogTail(logPath, d.logOffset)
		if err != nil {
			return "", "", err
		}
		if stderr != "" {
			if output.Len() > 0 {
				output.WriteString("\nDaemon stderr:\n")
			}
			output.WriteString(stderr)
		}
		if exit != nil {
			if output.Len() > 0 {
				output.WriteString("\n")
			}
			output.WriteString(core.FormatExitReason(exit))
		}
		path := logPath
		if diagnostics != nil {
			path = *diagnostics
		}
		return path, output.String(), nil
	}()
	if err != nil {
		slog.Warn("could not read the daemon logs after a failed launch", "error", err)
		return logPath, "could not read daemon logs: " + err.Error()
	}
	return path, output
}

func (d *launchedDaemon) failure(reason, output, logPath string) error {
	var inner error
	if output == "" {
		inner = fmt.Errorf("%s; it wrote nothing to %s", reason, logPath)
	} else {
		inner = fmt.Errorf("%s; it reported:\n%s", reason, output)
	}
	return fmt.Errorf("start Mjolnir daemon; details are in %s: %w", logPath, inner)
}

// launchDiagnostics finds the launched daemon's diagnostic log (nil when it
// has none yet) and, for a daemon that already exited, its exit record.
func launchDiagnostics(directory string, pid uint32) (*string, *core.DaemonExitRecord, error) {
	path, found, err := daemonLogPath(directory, pid)
	if err != nil {
		return nil, nil, err
	}
	if found {
		return &path, nil, nil
	}
	// Failed children have already been reaped by the waiter. Their record
	// links the launch PID to the PID used in the daemon's log filename.
	if entries, err := os.ReadDir(core.DaemonExitsDir(directory)); err == nil {
		for _, entry := range entries {
			name := entry.Name()
			if filepath.Ext(name) != ".json" || strings.HasPrefix(name, ".") || !entry.Type().IsRegular() {
				continue
			}
			record, err := core.ReadDaemonExitRecord(filepath.Join(core.DaemonExitsDir(directory), name))
			if err != nil || record.WaiterPid == nil || *record.WaiterPid != pid {
				continue
			}
			logPath, found, err := daemonLogPath(directory, record.Pid)
			if err != nil {
				return nil, nil, err
			}
			if !found {
				return nil, &record, nil
			}
			return &logPath, &record, nil
		}
	}
	// A launch that is still starting has a child process and no exit
	// record.
	for _, child := range childProcessIDs(pid) {
		path, found, err := daemonLogPath(directory, child)
		if err != nil {
			return nil, nil, err
		}
		if found {
			return &path, nil, nil
		}
	}
	return nil, nil, nil
}

// launchLogTail is the last 20 non-blank lines of the at most 64 KiB written
// to path after offset.
func launchLogTail(path string, offset uint64) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open daemon log %s: %w", path, err)
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return "", err
	}
	// Keep both memory and failure messages bounded during a noisy startup.
	const limit = 64 * 1024
	size := uint64(info.Size())
	start := offset
	if size > limit && size-limit > start {
		start = size - limit
	}
	if _, err := file.Seek(int64(start), io.SeekStart); err != nil {
		return "", err
	}
	appended, err := io.ReadAll(io.LimitReader(file, limit))
	if err != nil {
		return "", err
	}
	var lines []string
	for _, line := range rustLines(core.LossyUTF8(appended)) {
		if strings.TrimSpace(line) != "" {
			lines = append(lines, line)
		}
	}
	return strings.Join(lines[max(len(lines)-20, 0):], "\n"), nil
}

// daemonLaunchExecutable is the running executable: `/proc/self/exe` on
// Linux, because the pathname can disappear on upgrade and executing the
// running inode keeps the daemon on this client's protocol.
func daemonLaunchExecutable() (string, error) {
	if runtime.GOOS == "linux" {
		return "/proc/self/exe", nil
	}
	executable, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("find current mj executable: %w", err)
	}
	return executable, nil
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
	for _, arch := range []string{"aarch64", "x86_64"} {
		availability, err := resolve(arch)
		if err != nil {
			slog.Debug("development worker source is unavailable", "arch", arch, "error", err)
			continue
		}
		local, ok := availability.(workerstaging.WorkerBinaryAvailabilityLocal)
		if !ok {
			continue
		}
		info, err := os.Stat(local.Path)
		if err != nil {
			return false, fmt.Errorf("inspect development worker %s: %w", local.Path, err)
		}
		if info.ModTime().After(started) {
			return true, nil
		}
	}
	return false, nil
}

func maybeReplaceStaleDevelopmentDaemon(ctx context.Context, startup *daemonStartGuard) error {
	if _, set := os.LookupEnv(devRestartStaleDaemonEnv); !set {
		return nil
	}
	if runtime.GOOS != "linux" && runtime.GOOS != "darwin" {
		return nil
	}
	developmentDaemonRefresh.mu.Lock()
	defer developmentDaemonRefresh.mu.Unlock()
	if developmentDaemonRefresh.done {
		return nil
	}
	err := func() error {
		metadata, err := client.ReadMetadataAny()
		if err != nil {
			return nil
		}
		if client.EnsureSupportedDaemonProtocol(metadata) != nil {
			return nil
		}
		// The variable makes this client authoritative over stale builds,
		// not over newer ones. Ordinary startup explains the kept daemon.
		identity := daemonIdentityOrLegacy(ctx, &metadata)
		build, err := daemonBuildOf(&metadata, &identity.Placement)
		if err != nil {
			return err
		}
		if build.daemonIsNewer() {
			return nil
		}
		started, err := time.Parse(time.RFC3339Nano, metadata.StartedAt)
		if err != nil {
			return fmt.Errorf("parse development daemon start time: %w", err)
		}
		runs, err := daemonRunsThisBuild(&identity)
		if err != nil {
			return err
		}
		stale := runs != nil && !*runs
		if !stale {
			if stale, err = developmentWorkersChangedSince(started, workerstaging.WorkerBinaryPrerequisiteForArch); err != nil {
				return err
			}
		}
		if !stale {
			return nil
		}
		startupNotice(fmt.Sprintf("Mjolnir daemon %d is using an older development build; restarting it.", metadata.Pid))
		return replaceDaemon(ctx, startup, &metadata, &identity.Placement)
	}()
	if err != nil {
		return err
	}
	developmentDaemonRefresh.done = true
	return nil
}

// replaceDaemon hands the daemon off to this build. Automatic upgrades have
// no authority to cancel work, even when a daemon is slow or temporarily
// unreachable. Explicit restart uses stopDaemon.
func replaceDaemon(ctx context.Context, startup *daemonStartGuard, metadata *client.DaemonMetadata, placement *client.DaemonPlacement) error {
	if err := core.EnsureMayControlStore(core.DataDir(),
		fmt.Sprintf("replace Mjolnir daemon %d (build %s)", metadata.Pid, metadata.BuildVersion)); err != nil {
		return err
	}
	if err := ensureConfigLoads(core.ConfigPath(), "replaced"); err != nil {
		return err
	}
	if err := ensureContainerOwnerRecorded(ctx, startup, metadata, placement); err != nil {
		return err
	}
	// The replacement daemon pins this build's worker sources before it can
	// serve; for a new build that is seconds of copying and hashing. Do it
	// while the old daemon drains, and wait for it before the launch so the
	// two never hash the same files at once.
	type warmed struct {
		err  error
		took time.Duration
	}
	warming := core.SpawnBlocking(func() warmed {
		started := time.Now()
		err := workerstaging.WarmWorkerBinarySources()
		return warmed{err: err, took: time.Since(started)}
	})
	handedOff := handOffDaemon(ctx, metadata)
	result, err := warming.Join(context.Background())
	switch {
	case err != nil:
		slog.Warn("worker source pinning task failed", "error", err)
	// Startup pins the sources again and reports its own failure; this one
	// only says the head start was lost.
	case result.err != nil:
		slog.Warn("could not pin this build's worker sources during the handoff", "error", result.err.Error())
	case result.took >= loggedPhase:
		slog.Info("pinned this build's worker sources during the handoff", "duration_ms", result.took.Milliseconds())
	}
	return handedOff
}

// handOffDaemon asks the daemon to hand off and follows it until it exits.
func handOffDaemon(ctx context.Context, metadata *client.DaemonMetadata) error {
	asked := time.Now()
	slog.Info("asking the daemon to hand off to this build", "pid", metadata.Pid, "build", metadata.BuildVersion)
	outcome, err := askForHandoff(ctx, metadata)
	if err != nil {
		return err
	}
	slog.Info("daemon handoff admission finished",
		"pid", metadata.Pid, "waited_ms", time.Since(asked).Milliseconds(), "outcome", outcome.String())
	switch outcome {
	case handoffAskAccepted:
		if err := waitForDaemonRelease(ctx, metadata, client.StopTimeout); err != nil {
			return fmt.Errorf("the daemon accepted the handoff but did not exit: %w", err)
		}
	case handoffAskGone:
		if metadata.Pid == 0 {
			return clearExitedDaemon(metadata)
		}
	}
	return nil
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
	daemon, err := client.DaemonClientConnect(ctx, *metadata)
	if err != nil {
		return nil, err
	}
	defer daemon.Close()
	return daemon.RequestSingle(ctx, client.DaemonActionPrepareUpgrade{})
}

func askForHandoff(ctx context.Context, metadata *client.DaemonMetadata) (handoffAskOutcome, error) {
	waiting, cancel := context.WithCancel(ctx)
	defer cancel()
	type answer struct {
		outcome handoffAskOutcome
		err     error
	}
	asked := make(chan answer, 1)
	observed := make(chan answer, 1)
	go func() {
		for {
			reply, err := askPrepareUpgrade(waiting, metadata)
			if waiting.Err() != nil {
				return
			}
			if err == nil {
				switch reply.(type) {
				case client.DaemonReplyDone:
					asked <- answer{outcome: handoffAskAccepted}
					return
				case client.DaemonReplyUpgradePending:
				default:
					asked <- answer{err: fmt.Errorf("unexpected upgrade admission reply %s", client.DaemonReplyDebug(reply))}
					return
				}
			} else {
				// PrepareUpgrade is idempotent on the daemon. A transient
				// connect or transport error may reflect concurrent
				// teardown before the store lock drops; keep retrying while
				// the owner is watched for replacement or exit.
				slog.Debug("automatic upgrade cannot establish safe handoff yet", "error", err)
			}
			if sleepContext(waiting, handoffAskInterval) != nil {
				return
			}
		}
	}()
	go func() {
		for {
			outcome, changed, err := handoffOwnerChange(waiting, metadata)
			if waiting.Err() != nil {
				return
			}
			if err != nil {
				observed <- answer{err: err}
				return
			}
			if changed {
				observed <- answer{outcome: outcome}
				return
			}
			if sleepContext(waiting, handoffAskInterval) != nil {
				return
			}
		}
	}()
	go func() {
		blockers := &pendingBlockers{}
		defer blockers.close()
		query := func(ctx context.Context) []string { return upgradeBlockers(ctx, metadata) }
		prewarm := startNoticeDelay - blockerPrewarmLead
		if sleepContext(waiting, prewarm) != nil {
			return
		}
		blockers.ensureStarted(waiting, query)
		if sleepContext(waiting, startNoticeDelay-prewarm) != nil {
			return
		}
		for {
			startupNotice(upgradeWaitNotice(blockers.refresh(waiting, query)))
			if sleepContext(waiting, startupNoticeInterval()) != nil {
				return
			}
		}
	}()
	// biased: the admission answer wins over an owner change seen at once.
	var result answer
	select {
	case result = <-asked:
	case result = <-observed:
		select {
		case result = <-asked:
		default:
		}
	case <-ctx.Done():
		return 0, ctx.Err()
	}
	return result.outcome, result.err
}

// handoffOwnerChange is how the store's owner changed (false: it did not).
func handoffOwnerChange(ctx context.Context, metadata *client.DaemonMetadata) (handoffAskOutcome, bool, error) {
	release, err := probeDaemonRelease(ctx, metadata)
	if err != nil {
		return 0, false, err
	}
	switch release.kind {
	case daemonReleaseReplaced:
		return handoffAskReplaced, true, nil
	case daemonReleaseExited:
		return handoffAskGone, true, nil
	case daemonReleaseStillOwned:
		return 0, false, nil
	}
	return 0, false, errors.New(release.reason)
}

// clearExitedDaemon removes the exited daemon's records. The caller holds
// the startup lock across the compare and removal.
func clearExitedDaemon(metadata *client.DaemonMetadata) error {
	if _, err := removeMatchingMetadata(metadata); err != nil {
		return err
	}
	if metadata.Pid == 0 {
		return client.RemoveContainerOwnerIfMatching(metadata.Address, metadata.Token)
	}
	return nil
}

func removeMatchingMetadata(previous *client.DaemonMetadata) (bool, error) {
	current, err := client.ReadMetadataAny()
	if err != nil || !sameDaemonEndpoint(&current, previous) {
		return false, nil
	}
	path := client.MetadataPath()
	switch err := removeFile(path); {
	case err == nil:
		return true, nil
	case errors.Is(err, fs.ErrNotExist):
		return false, nil
	default:
		return false, fmt.Errorf("remove %s: %w", path, err)
	}
}

func waitForDaemonRelease(ctx context.Context, metadata *client.DaemonMetadata, timeout time.Duration) error {
	timed, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	elapsed := func() error {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return fmt.Errorf("could not verify release of the Mjolnir daemon at %s within %ds: deadline has elapsed",
			metadata.Address, int64(timeout/time.Second))
	}
	exited := false
	for done := false; !done; {
		release, err := probeDaemonRelease(timed, metadata)
		if timed.Err() != nil {
			return elapsed()
		}
		if err != nil {
			return err
		}
		switch release.kind {
		case daemonReleaseReplaced:
			done = true
		case daemonReleaseExited:
			exited, done = true, true
		case daemonReleaseUnknown:
			return errors.New(release.reason)
		case daemonReleaseStillOwned:
			if sleepContext(timed, client.RetryDelay) != nil {
				return elapsed()
			}
		}
	}
	if exited && metadata.Pid == 0 {
		return clearExitedDaemon(metadata)
	}
	return nil
}

// upgradeWaitNotice is the line shown while an automatic upgrade waits for
// the old daemon (blockers nil when the daemon named none).
func upgradeWaitNotice(blockers []string) string {
	if blockers != nil {
		return fmt.Sprintf("Mjolnir upgrade is waiting for: %s; existing sessions remain available.", strings.Join(blockers, ", "))
	}
	return "Mjolnir upgrade is waiting for ongoing work; existing sessions remain available."
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
	if b.task != nil {
		return
	}
	queryCtx, cancel := context.WithCancel(ctx)
	b.cancel = cancel
	b.task = core.Spawn(func() []string { return query(queryCtx) })
}

func (b *pendingBlockers) refresh(ctx context.Context, query func(ctx context.Context) []string) []string {
	if b.task != nil {
		select {
		case <-b.task.Done():
			labels, err := b.task.Join(context.Background())
			if err != nil {
				slog.Warn("daemon upgrade blockers query failed", "error", err)
				labels = nil
			}
			b.latest = labels
			b.cancel()
			b.task, b.cancel = nil, nil
		default:
		}
	}
	b.ensureStarted(ctx, query)
	return b.latest
}

func (b *pendingBlockers) close() {
	if b.cancel != nil {
		b.cancel()
	}
}

// upgradeBlockers asks the daemon what holds its handoff open. Older
// daemons close the connection for an unknown action. With no labels (nil),
// the wait notice describes the handoff without naming any specific work.
func upgradeBlockers(ctx context.Context, metadata *client.DaemonMetadata) []string {
	daemon, err := client.DaemonClientConnect(ctx, *metadata)
	if err != nil {
		return nil
	}
	defer daemon.Close()
	reply, err := daemon.RequestSingle(ctx, client.DaemonActionUpgradeBlockers{})
	if err != nil {
		return nil
	}
	if blockers, ok := reply.(client.DaemonReplyUpgradeBlockers); ok && len(blockers.Blockers) > 0 {
		return blockers.Blockers
	}
	return nil
}

func ensureContainerOwnerRecorded(ctx context.Context, startup *daemonStartGuard, metadata *client.DaemonMetadata, placement *client.DaemonPlacement) error {
	if metadata.Pid != 0 {
		return nil
	}
	err := func() error {
		record, err := matchingContainerOwner(metadata)
		if err != nil {
			return err
		}
		if record != nil {
			return nil
		}
		name, inContainer := placement.Container()
		if !inContainer || placement.Runtime == nil {
			return errors.New(missingContainerOwnerReason(nil))
		}
		runtime := *placement.Runtime
		containerID, found, err := resolveContainerID(ctx, runtime, name)
		if err != nil {
			return err
		}
		if !found {
			return fmt.Errorf("container %s was not listed by the engine", name)
		}
		protocol := metadata.ProtocolVersion
		return client.WriteContainerOwner(&client.ContainerDaemonRecord{
			Schema:          client.ContainerOwnerSchema,
			ProtocolVersion: &protocol,
			Runtime:         runtime,
			ContainerID:     containerID,
			ContainerName:   name,
			Address:         metadata.Address,
			Token:           metadata.Token,
			StartedAt:       metadata.StartedAt,
		})
	}()
	if err != nil {
		return fmt.Errorf("The daemon was left running because its container owner record could not be established: %w", err)
	}
	return nil
}

type stopFallback int

const (
	stopFallbackGraceful stopFallback = iota
	stopFallbackForce
	stopFallbackRestart
)

func stopDaemon(ctx context.Context, startup *daemonStartGuard, metadata *client.DaemonMetadata, fallback stopFallback) (bool, error) {
	answering := false
	attempt := func() error {
		daemon, err := client.DaemonClientConnect(ctx, *metadata)
		if err != nil {
			return err
		}
		management := client.NewManagementClient(daemon)
		defer management.Close()
		// Capture ownership before sending Stop to a legacy container.
		if metadata.Pid == 0 {
			if record, err := matchingContainerOwner(metadata); !(err == nil && record != nil) {
				reading, cancel := context.WithTimeout(ctx, 2*time.Second)
				identity, err := requestDaemonIdentity(reading, daemon)
				timedOut := ctx.Err() == nil && errors.Is(reading.Err(), context.DeadlineExceeded)
				cancel()
				if err != nil {
					if timedOut {
						return errors.New("time out reading the legacy container daemon's identity: deadline has elapsed")
					}
					return err
				}
				answering = true
				if err := ensureContainerOwnerRecorded(ctx, startup, metadata, &identity.Placement); err != nil {
					return stopAborted{err: err}
				}
			}
		}
		return management.Stop(ctx)
	}
	err := attempt()
	var aborted stopAborted
	if errors.As(err, &aborted) {
		return false, aborted.err
	}
	if err == nil {
		if err := waitForDaemonRelease(ctx, metadata, client.StopDrainTimeout); err != nil {
			return false, fmt.Errorf("the daemon accepted the stop but its release could not be verified: %w", err)
		}
		return true, nil
	}
	release, probeErr := probeDaemonRelease(ctx, metadata)
	if probeErr != nil {
		return false, probeErr
	}
	switch release.kind {
	case daemonReleaseExited:
		if err := clearExitedDaemon(metadata); err != nil {
			return false, err
		}
		return false, nil
	case daemonReleaseReplaced:
		return false, errors.New("The Mjolnir daemon was replaced while stopping it. Run the stop command again.")
	case daemonReleaseStillOwned:
		if fallback != stopFallbackGraceful {
			if err := signalDaemon(ctx, metadata); err != nil {
				return false, err
			}
			return true, nil
		}
		return false, fmt.Errorf("The Mjolnir daemon still owns its store and did not acknowledge the stop. Run `mj daemon stop --force` to stop it: %w", err)
	}
	if fallback == stopFallbackForce && metadata.Pid == 0 && !answering {
		cleared, clearErr := clearUnreachableContainerRecord(ctx, metadata)
		if clearErr != nil {
			return false, clearErr
		}
		if cleared {
			return true, nil
		}
	}
	return false, fmt.Errorf("%s: %w", release.reason, err)
}

// stopAborted carries an error that ends stopDaemon at once, as Rust's `?`
// inside the stop attempt does, instead of falling back to the release
// probe.
type stopAborted struct{ err error }

func (e stopAborted) Error() string { return e.err.Error() }

// clearUnreachableContainerRecord clears a legacy container daemon's stale
// record. The caller holds the startup lock and has explicitly forced
// legacy recovery.
func clearUnreachableContainerRecord(ctx context.Context, metadata *client.DaemonMetadata) (bool, error) {
	record, err := matchingContainerOwner(metadata)
	unreadable := false
	switch {
	case err == nil && record != nil:
		return false, nil
	case err != nil:
		unreadable = true
	}
	connecting, cancel := context.WithTimeout(ctx, 2*time.Second)
	daemon, err := connectReadyContainer(connecting, metadata)
	cancel()
	if err == nil {
		daemon.Close()
		return false, nil
	}
	removed, err := removeMatchingMetadata(metadata)
	if err != nil {
		return false, err
	}
	if !removed {
		return false, errors.New("The daemon.json record changed while stopping the Mjolnir daemon. Run the stop command again.")
	}
	fmt.Fprintf(os.Stderr, "Cleared the stale container daemon record in %s.\n", client.MetadataPath())
	if unreadable {
		if err := clearUnreadableContainerOwner(); err != nil {
			return false, err
		}
	}
	return true, nil
}

// signalDaemon stops the daemon metadata names after it did not answer a
// protocol stop: SIGTERM on Unix, which every supported daemon handles as
// graceful cancellation. Windows has no SIGTERM, and the daemon has no
// console to receive Ctrl-Break, so it is terminated; its state is in the
// store and its workers keep running, so the next daemon recovers it as
// after a crash.
func signalDaemon(ctx context.Context, metadata *client.DaemonMetadata) error {
	if metadata.Pid == 0 {
		record, err := matchingContainerOwner(metadata)
		if err != nil {
			return err
		}
		if record == nil {
			return errors.New("the container daemon no longer has a matching daemon-container.json owner record")
		}
		// Forcing the container to stop also stops its workers.
		if err := stopContainer(ctx, record.Runtime, record.ContainerID); err != nil {
			return err
		}
		return clearExitedDaemon(metadata)
	}
	placement := daemonIdentityOrLegacy(ctx, metadata).Placement
	if name, inContainer := placement.Container(); inContainer && placement.Runtime != nil {
		if err := stopContainer(ctx, *placement.Runtime, name); err != nil {
			return err
		}
		return clearExitedDaemon(metadata)
	}
	if err := signalHostDaemon(metadata.Pid); err != nil {
		return err
	}
	return waitForDaemonRelease(ctx, metadata, client.StopTimeout)
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
	if !client.ProcessIsAlive(pid) {
		return hostPIDNotDaemon
	}
	// The command line is read only when the refresh asks for it.
	if command, ok := processCommandLine(pid); ok && len(command) > 0 {
		if len(command) > 1 && command[1] == "daemon-run" {
			return hostPIDDaemon
		}
		return hostPIDNotDaemon
	}
	if client.ProcessIsAlive(pid) {
		return hostPIDUnreadable
	}
	return hostPIDNotDaemon
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

// attachmentRefreshPeriod is how often the keep-alive refreshes.
const attachmentRefreshPeriod = 2 * time.Second

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
	presence := core.NewWatch[DaemonPresence](DaemonPresenceAttached{})
	task := core.Spawn(func() struct{} {
		// tokio's interval: the first tick is at once; a late tick delays
		// the ones after it (MissedTickBehavior::Delay).
		next := time.Now()
		for {
			timer := time.NewTimer(time.Until(next))
			select {
			case <-cancellation.Done():
				timer.Stop()
				return struct{}{}
			case <-timer.C:
			}
			ticked := time.Now()
			refreshing, cancel := context.WithTimeout(cancellation, attachmentRefreshDisplayTimeout)
			observed, err := observeAttachment(refreshing, clientID)
			timedOut := errors.Is(refreshing.Err(), context.DeadlineExceeded)
			cancel()
			if cancellation.Err() != nil {
				return struct{}{}
			}
			if timedOut {
				err = errors.New("Err(Elapsed(()))")
			} else if err != nil {
				err = fmt.Errorf("Ok(Err(%s))", err)
			}
			if err != nil {
				reason := "could not refresh daemon attachment: " + err.Error()
				slog.Warn(reason)
				observed = DaemonPresenceMissing{Reason: reason}
			}
			// Only a change is sent, so a daemon that keeps answering does
			// not wake the render loop every two seconds.
			if presence.Borrow() != observed {
				presence.SendReplace(observed)
			}
			next = ticked.Add(attachmentRefreshPeriod)
		}
	})
	return Attachment{Task: task, Presence: presence}
}

func observeAttachment(ctx context.Context, clientID string) (DaemonPresence, error) {
	target, err := upgradedDaemonExecutable(ctx)
	if err != nil {
		return nil, err
	}
	if target != nil {
		return DaemonPresenceUpgraded{Target: *target}, nil
	}
	daemon, err := client.ConnectExisting(ctx)
	if err != nil {
		return nil, err
	}
	defer daemon.Close()
	reply, err := daemon.RequestSingle(ctx, client.DaemonActionAttach{ClientID: clientID})
	if err != nil {
		return nil, err
	}
	switch reply.(type) {
	case client.DaemonReplyDone:
		return DaemonPresenceAttached{}, nil
	case client.DaemonReplyUpgradePending:
		return DaemonPresenceMissing{Reason: "daemon upgrade handoff is in progress"}, nil
	}
	return nil, fmt.Errorf("unexpected attach reply %s", client.DaemonReplyDebug(reply))
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
	slog.Info("continuing this command under the daemon's newer build",
		"executable", target.Executable, "args", fmt.Sprintf("%q", args))
	fmt.Fprintf(os.Stderr, "Mjolnir upgraded; continuing with %s.\n", target.Executable)
	environment := setCommandEnv(os.Environ(), "MJ_UPGRADE_DAEMON", target.Generation)
	environment = setCommandEnv(environment, "MJOLNIR_NO_UPDATE_CHECK", "1")
	if runtime.GOOS != "windows" {
		err := syscall.Exec(target.Executable, append([]string{target.Executable}, args...), environment)
		return fmt.Errorf("continue under the upgraded build %s: %w", target.Executable, err)
	}
	command := exec.Command(target.Executable, args...)
	command.Env = environment
	status, err := core.RunInteractive(command)
	if err != nil {
		return fmt.Errorf("continue under the upgraded build %s: %w", target.Executable, err)
	}
	code := status.ExitCode()
	if code < 0 {
		code = 1
	}
	os.Exit(code)
	return nil
}

// upgradedDaemonExecutable is the newer build to continue under, or nil.
func upgradedDaemonExecutable(ctx context.Context) (*UpgradeTarget, error) {
	connector, err := client.DaemonConnectorShared(ctx)
	if err != nil {
		return nil, err
	}
	if connector.IsSSH() {
		return nil, nil
	}
	metadata, err := client.ReadMetadataAny()
	if err != nil {
		return nil, nil
	}
	identity := daemonIdentityOrLegacy(ctx, &metadata)
	build, err := daemonBuildOf(&metadata, &identity.Placement)
	if err != nil {
		return nil, err
	}
	newer := build.release > 0
	if !newer && metadata.ProtocolVersion <= client.ProtocolVersion {
		return nil, nil
	}
	// A container daemon runs the image's own executable; there is no local
	// file this client could continue under. The next command starts the
	// client's own build in the ordinary way.
	if _, inContainer := identity.Placement.Container(); inContainer {
		return nil, nil
	}
	generation := identity.InstanceID
	if generation == "" {
		generation = strconv.FormatUint(uint64(metadata.Pid), 10) + ":" + metadata.StartedAt
	}
	// Do not loop if another installer replaced the executable again during
	// handoff.
	if current, ok := os.LookupEnv("MJ_UPGRADE_DAEMON"); ok && current == generation {
		return nil, nil
	}
	executable, ok := client.ProcessExecutablePath(metadata.Pid)
	if !ok {
		return nil, nil
	}
	if info, err := os.Stat(executable); err != nil || !info.Mode().IsRegular() {
		return nil, nil
	}
	return &UpgradeTarget{Executable: executable, Generation: generation}, nil
}

// printEndpoint prints this machine's daemon endpoint as JSON (`mj daemon
// endpoint`). Machine endpoint discovery is always local to this launcher.
// In particular, an SSH placement here must not recursively launch another
// SSH client.
func printEndpoint(ctx context.Context, existing bool) error {
	path := core.ConfigPath()
	placement, err := core.DaemonConfigLoadPlacement(path)
	if err != nil {
		return err
	}
	if placement.Runtime == core.DaemonRuntimeSsh {
		return fmt.Errorf("%s: daemon endpoint cannot follow SSH placement; choose host, docker or podman on this machine", path)
	}
	var daemon *client.DaemonClient
	if existing {
		if daemon, err = client.ConnectExisting(ctx); err != nil {
			return fmt.Errorf("no running daemon available for daemon endpoint --existing: %w", err)
		}
	} else if daemon, err = ConnectOrStart(ctx); err != nil {
		return err
	}
	defer daemon.Close()
	endpoint, err := daemon.Endpoint(ctx)
	if err != nil {
		return err
	}
	configPath := core.ConfigPath()
	endpoint.ConfigPath = &configPath
	body, err := core.SerdeJSON(endpoint)
	if err != nil {
		return err
	}
	fmt.Println(string(body))
	return nil
}
