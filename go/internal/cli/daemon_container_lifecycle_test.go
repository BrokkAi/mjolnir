//go:build linux || darwin

// Port of the container_tests module of the tests in mj-cli/src/daemon.rs.

package cli

import (
	"context"
	"errors"
	"io"
	"net"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	testContainerID      = "test-container-id"
	testContainerRuntime = core.DaemonRuntimePodman
)

// fakePodmanScript is the engine stand-in: it records every call, answers
// from the state file the test writes, and refuses anything it does not
// know.
const fakePodmanScript = `
printf '%s\n' "$*" >> "$MJ_DATA_DIR/engine.calls"
state=$(cat "$MJ_DATA_DIR/engine.state")
case "$1" in
  image) printf 'test-image|%s\n' "$MJ_TEST_IMAGE_REVISION" ;;
  ps)
    if [ "$state" = "ps-error" ]; then
      echo 'simulated engine ps failure' >&2
      exit 125
    fi
    case "$*" in
      *--format={{.Names}}*) if [ "$state" = "inspect-error" ]; then echo predecessor; fi ;;
      *id=old-container-id*) ;;
      *id=test-container-id*) echo test-container-id ;;
      *'name=^/?'*)
        # Podman matches names without a leading slash; require ` + "`^/?`" + ` so a
        # Docker-only ` + "`^/`" + ` filter fails the suite.
        filter=$(printf '%s\n' "$*" | sed -n 's/.*name=\^\/?\([^$]*\)\$.*/\1/p' | sed 's/\[\.\]/./g')
        [ -n "$filter" ] || exit 64
        printf 'test-container-id\t%s\n' "$filter" ;;
      *) exit 64 ;;
    esac ;;
  inspect)
    [ "$2 $3" = "--format={{.State.Running}} test-container-id" ] || exit 64
    case "$state" in
      inspect-error) echo 'No such container (simulated engine failure)' >&2; exit 125 ;;
      stopped) echo false ;;
      *) echo true ;;
    esac ;;
  top)
    [ "$2 $3" = 'test-container-id args' ] || exit 64
    printf 'COMMAND\nsleep infinity\n'
    case "$state" in
      daemon) echo '/opt/mjolnir/mj daemon-run' ;;
      *) printf 'hel worker\ncodex app-server\n' ;;
    esac ;;
  start)
    [ "$2" = 'test-container-id' ] || exit 64
    echo worker > "$MJ_DATA_DIR/engine.state" ;;
  port) cat "$MJ_DATA_DIR/engine.endpoint" ;;
  exec)
    [ "$2" = '--detach' ] || exit 64
    [ -s "$MJ_DATA_DIR/daemon-container.json" ] || exit 65
    [ ! -e "$MJ_DATA_DIR/daemon.json" ] || exit 66
    printf '%s' "$MJ_DAEMON_TOKEN" > "$MJ_DATA_DIR/exec.token"
    echo daemon > "$MJ_DATA_DIR/engine.state" ;;
  stop)
    [ "$2" = 'test-container-id' ] || exit 64
    echo stopped > "$MJ_DATA_DIR/engine.state" ;;
  *) exit 64 ;;
esac
`

// fakePodman reruns the test in an isolated child whose PATH finds the
// engine stand-in first. True in the parent.
func fakePodman(t *testing.T, child string) bool {
	t.Helper()
	return rerunInIsolatedChildWith(t, child, func(command *exec.Cmd, directory string) {
		tools := filepath.Join(directory, "bin")
		if err := os.Mkdir(tools, 0o755); err != nil {
			t.Fatalf("create the fake engine directory: %v", err)
		}
		core.InstallFakeCommand(tools, "podman", fakePodmanScript)
		buildID := core.BuildID()
		index := strings.LastIndex(buildID, "+")
		if index < 0 {
			t.Fatalf("test build %q has no revision", buildID)
		}
		command.Env = append(command.Env,
			"PATH="+tools+string(os.PathListSeparator)+os.Getenv("PATH"),
			"MJ_TEST_IMAGE_REVISION="+buildID[index+1:])
	})
}

func engineState(t *testing.T, state string) {
	t.Helper()
	check(t, os.WriteFile(filepath.Join(core.DataDir(), "engine.state"), []byte(state), 0o644))
}

func engineCalls() string {
	calls, err := os.ReadFile(filepath.Join(core.DataDir(), "engine.calls"))
	if err != nil {
		return ""
	}
	return string(calls)
}

func configureContainer(t *testing.T) *containerSpec {
	t.Helper()
	check(t, os.WriteFile(core.ConfigPath(), []byte("version = 1\n[daemon]\nruntime = \"podman\"\n"), 0o644))
	config, err := loadDaemonConfig(context.Background())
	check(t, err)
	placement, err := resolvePlacement(context.Background(), &config)
	check(t, err)
	container, ok := placement.(configuredPlacementContainer)
	if !ok {
		t.Fatalf("expected fake engine placement, got %#v", placement)
	}
	return container.spec
}

func testOwner(metadata *client.DaemonMetadata, containerID, name string) client.ContainerDaemonRecord {
	protocol := metadata.ProtocolVersion
	return client.ContainerDaemonRecord{
		Schema:          client.ContainerOwnerSchema,
		ProtocolVersion: &protocol,
		Runtime:         testContainerRuntime,
		ContainerID:     containerID,
		ContainerName:   name,
		Address:         metadata.Address,
		Token:           metadata.Token,
		StartedAt:       metadata.StartedAt,
	}
}

func setEndpoint(t *testing.T, listener net.Listener) {
	t.Helper()
	check(t, os.WriteFile(filepath.Join(core.DataDir(), "engine.endpoint"), []byte(listener.Addr().String()), 0o644))
}

// serveContainerReadiness answers one readiness probe: a Ping carrying the
// owner record's token and, when identity is set, the Identity request
// after it. Failures are reported with t.Errorf.
func serveContainerReadiness(t *testing.T, listener net.Listener, identity *client.DaemonIdentity) {
	stream, err := listener.Accept()
	if err != nil {
		t.Errorf("accept the readiness probe: %v", err)
		return
	}
	defer stream.Close()
	ping, ok := readRequest(t, stream)
	if !ok {
		return
	}
	if _, isPing := ping.Action.(client.DaemonActionPing); !isPing {
		t.Errorf("expected Ping, got %#v", ping.Action)
		return
	}
	record, err := client.ReadContainerOwner()
	if err != nil || record == nil {
		t.Errorf("read the owner record: %v, %v", record, err)
		return
	}
	if ping.Token != record.Token {
		t.Errorf("ping token %q is not the owner's %q", ping.Token, record.Token)
	}
	answer(t, stream, ping, client.DaemonOk[client.DaemonReply](client.DaemonReplyPong{}))
	if identity != nil {
		request, ok := readRequest(t, stream)
		if !ok {
			return
		}
		if _, isIdentity := request.Action.(client.DaemonActionIdentity); !isIdentity {
			t.Errorf("expected Identity, got %#v", request.Action)
			return
		}
		answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyIdentity{Identity: *identity}))
	}
}

// runBeside runs peer in a goroutine and returns a wait for it, bounded by
// limit as Rust's tokio::time::timeout around join! is.
func runBeside(t *testing.T, peer func()) func(limit time.Duration) {
	done := make(chan struct{})
	go func() {
		defer close(done)
		peer()
	}()
	return func(limit time.Duration) {
		t.Helper()
		select {
		case <-done:
		case <-time.After(limit):
			t.Fatalf("the fake daemon did not finish within %s", limit)
		}
	}
}

func countLines(calls, prefix string) int {
	count := 0
	for _, call := range strings.Split(calls, "\n") {
		if strings.HasPrefix(call, prefix) {
			count++
		}
	}
	return count
}

// Hard-won: #1458: stale container metadata prevented relaunch after the daemon exited.
func TestExitedContainerDaemonsRelaunchWithoutDisruptingWorkers(t *testing.T) {
	if fakePodman(t, "MJ_TEST_RELAUNCH_CONTAINER") {
		return
	}
	ctx := context.Background()
	engineState(t, "worker")
	spec := configureContainer(t)
	for _, step := range []struct{ state, oldID string }{
		{"absent", "old-container-id"},
		{"stopped", testContainerID},
		{"worker", testContainerID},
	} {
		engineState(t, step.state)
		check(t, os.WriteFile(filepath.Join(core.DataDir(), "engine.calls"), nil, 0o644))
		old, metadata := fakeDaemon(t, 0)
		ready, _ := fakeDaemon(t, 0)
		old.Close()
		setEndpoint(t, ready)
		check(t, writeLauncherMetadata(&metadata))
		record := testOwner(&metadata, step.oldID, spec.name)
		check(t, client.WriteContainerOwner(&record))
		release, err := probeDaemonRelease(ctx, &metadata)
		check(t, err)
		if release.kind != daemonReleaseExited {
			t.Fatalf("%s: release = %+v", step.state, release)
		}

		startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
		check(t, err)
		wait := runBeside(t, func() { serveContainerReadiness(t, ready, nil) })
		bounded, cancel := context.WithTimeout(ctx, 10*time.Second)
		opened, err := connectOrStartHolding(bounded, startup)
		cancel()
		wait(10 * time.Second)
		check(t, err)
		opened.Close()
		published, err := client.ReadMetadataAny()
		check(t, err)
		owner, err := client.ReadContainerOwner()
		check(t, err)
		if published.Address != listenerAddress(ready) {
			t.Fatalf("published address = %s", published.Address)
		}
		if metadata.Token == published.Token {
			t.Fatal("the relaunch reused the exited daemon's token")
		}
		if owner == nil || !owner.Matches(&published) {
			t.Fatalf("owner = %+v", owner)
		}
		calls := engineCalls()
		if count := countLines(calls, "exec --detach"); count != 1 {
			t.Fatalf("%s", calls)
		}
		if countLines(calls, "stop ") > 0 || countLines(calls, "rm ") > 0 {
			t.Fatalf("%s", calls)
		}
		if strings.Contains(calls, "start test-container-id") != (step.state == "stopped") {
			t.Fatalf("%s", calls)
		}
		check(t, clearExitedDaemon(&published))
		startup.release()
		ready.Close()
	}
}

// Hard-won: #1568: a container's hung RPC was treated as released ownership.
func TestSilentContainerEndpointRetainsOwnershipAndBlocksRelaunch(t *testing.T) {
	if fakePodman(t, "MJ_TEST_SILENT_CONTAINER") {
		return
	}
	ctx := context.Background()
	engineState(t, "daemon")
	spec := configureContainer(t)
	listener, metadata := fakeDaemon(t, 0)
	defer listener.Close()
	check(t, writeLauncherMetadata(&metadata))
	record := testOwner(&metadata, testContainerID, spec.name)
	check(t, client.WriteContainerOwner(&record))
	release, err := probeDaemonRelease(ctx, &metadata)
	check(t, err)
	if release.kind != daemonReleaseStillOwned {
		t.Fatalf("release = %+v", release)
	}
	existing, err := decideExistingDaemon(ctx)
	check(t, err)
	if _, unanswered := existing.(existingDaemonUnanswered); !unanswered {
		t.Fatalf("existing = %#v", existing)
	}
	startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
	check(t, err)
	defer startup.release()
	bounded, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	started := time.Now()
	opened, err := connectOrStartHolding(bounded, startup)
	if err == nil {
		opened.Close()
		t.Fatal("startup used or launched a daemon beside a silent owner")
	}
	if elapsed := time.Since(started); elapsed < 3*time.Second {
		t.Fatalf("startup gave up after %s instead of waiting: %v", elapsed, err)
	}
	if strings.Contains(engineCalls(), "exec --detach") {
		t.Fatalf("%s", engineCalls())
	}
	owner, err := client.ReadContainerOwner()
	check(t, err)
	if owner == nil || !owner.Matches(&metadata) {
		t.Fatalf("owner = %+v", owner)
	}
}

// Hard-won: #1458: a failed container inspect was treated as an idle predecessor.
func TestEngineAndOwnerRecordErrorsFailClosed(t *testing.T) {
	if fakePodman(t, "MJ_TEST_CONTAINER_PROBE_FAILURE") {
		return
	}
	ctx := context.Background()
	engineState(t, "inspect-error")
	spec := configureContainer(t)
	listener, metadata := fakeDaemon(t, 0)
	listener.Close()
	check(t, writeLauncherMetadata(&metadata))
	record := testOwner(&metadata, testContainerID, spec.name)
	check(t, client.WriteContainerOwner(&record))
	release, err := probeDaemonRelease(ctx, &metadata)
	check(t, err)
	if release.kind != daemonReleaseUnknown {
		t.Fatalf("inspect error must leave ownership unknown: %+v", release)
	}
	if !strings.Contains(release.reason, "simulated engine failure") {
		t.Fatalf("%s", release.reason)
	}

	unbound := record
	unbound.Token = "unbound"
	check(t, client.WriteContainerOwner(&unbound))
	release, err = probeDaemonRelease(ctx, &metadata)
	check(t, err)
	if release.kind != daemonReleaseUnknown {
		t.Fatalf("record for another token must leave ownership unknown: %+v", release)
	}
	if !strings.Contains(release.reason, "without a matching daemon-container.json") {
		t.Fatalf("%s", release.reason)
	}

	check(t, os.WriteFile(client.ContainerOwnerPath(), []byte("{"), 0o644))
	release, err = probeDaemonRelease(ctx, &metadata)
	check(t, err)
	if release.kind != daemonReleaseUnknown {
		t.Fatalf("corrupt record must leave ownership unknown: %+v", release)
	}
	if !strings.Contains(release.reason, "parse ") {
		t.Fatalf("%s", release.reason)
	}
	check(t, os.Remove(client.MetadataPath()))
	if _, err := decideExistingDaemon(ctx); err == nil || !strings.Contains(err.Error(), "daemon-container.json") {
		t.Fatalf("startup ignored an unreadable unpublished owner: %v", err)
	}

	check(t, os.Remove(client.ContainerOwnerPath()))
	startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
	check(t, err)
	defer startup.release()
	opened, err := connectOrStartHolding(ctx, startup)
	if err == nil {
		opened.Close()
		t.Fatal("startup ignored the predecessor's inspect error")
	}
	if !strings.Contains(err.Error(), "simulated engine failure") {
		t.Fatalf("unexpected error: %v", err)
	}
	calls := engineCalls()
	if countLines(calls, "rm ") > 0 || countLines(calls, "exec ") > 0 {
		t.Fatalf("%s", calls)
	}
}

// Hard-won: #1458: a launcher crash stranded an unpublished running daemon.
func TestSuccessorAdoptsAContainerAfterTheLauncherLosesItsLock(t *testing.T) {
	if fakePodman(t, "MJ_TEST_ADOPT_CONTAINER") {
		return
	}
	ctx := context.Background()
	engineState(t, "worker")
	spec := configureContainer(t)
	listener, _ := fakeDaemon(t, 0)
	defer listener.Close()
	setEndpoint(t, listener)
	lock := filepath.Join(core.DataDir(), "daemon-start.lock")
	launcher, err := acquireStartGuard(ctx, lock)
	check(t, err)
	launch, err := launchContainerDaemon(ctx, spec, nil)
	check(t, err)
	if pathExists(client.MetadataPath()) {
		t.Fatal("the launcher published before its daemon answered")
	}
	launcher.release()
	expectPending := func() {
		t.Helper()
		existing, err := decideExistingDaemon(ctx)
		check(t, err)
		pending, ok := existing.(existingDaemonPendingContainer)
		if !ok || !pending.record.Matches(&launch.metadata) {
			t.Fatalf("existing = %#v", existing)
		}
	}
	expectPending()
	if pathExists(client.MetadataPath()) {
		t.Fatal("deciding published daemon.json")
	}
	check(t, os.WriteFile(client.MetadataPath(), []byte("{"), 0o644))
	expectPending()
	contents, err := os.ReadFile(client.MetadataPath())
	check(t, err)
	if string(contents) != "{" {
		t.Fatalf("daemon.json = %q", contents)
	}
	check(t, os.Remove(client.MetadataPath()))

	identity := client.DaemonIdentity{
		InstanceID:   "adopted",
		BuildVersion: "999.0.0",
		Placement:    launch.metadata.Placement,
	}
	wait := runBeside(t, func() {
		if pathExists(client.MetadataPath()) {
			t.Errorf("daemon.json was published before the adoption answered")
		}
		serveContainerReadiness(t, listener, &identity)
		// The published record now goes back through build/placement
		// discovery.
		serveContainerReadiness(t, listener, &identity)
	})
	bounded, cancel := context.WithTimeout(ctx, 10*time.Second)
	opened, err := connectOrStartLocal(bounded)
	cancel()
	wait(10 * time.Second)
	check(t, err)
	opened.Close()
	published, err := client.ReadMetadataAny()
	check(t, err)
	if !sameDaemonEndpoint(&published, &launch.metadata) {
		t.Fatalf("published = %+v", published)
	}
	if published.BuildVersion != "999.0.0" {
		t.Fatalf("build = %q", published.BuildVersion)
	}
	if count := countLines(engineCalls(), "exec --detach"); count != 1 {
		t.Fatalf("%s", engineCalls())
	}
}

func TestRetryReusesCredentialsOfAnInterruptedContainerExec(t *testing.T) {
	if fakePodman(t, "MJ_TEST_RETRY_CONTAINER_EXEC") {
		return
	}
	ctx := context.Background()
	engineState(t, "worker")
	spec := configureContainer(t)
	listener, metadata := fakeDaemon(t, 0)
	defer listener.Close()
	prior := testOwner(&metadata, testContainerID, spec.name)
	setEndpoint(t, listener)
	check(t, client.WriteContainerOwner(&prior))
	if pathExists(client.MetadataPath()) {
		t.Fatal("daemon.json exists before any launch")
	}
	if status := probeContainerDaemon(ctx, testContainerRuntime, testContainerID); status != (containerDaemonStatus{kind: containerDaemonExited, containerRunning: true}) {
		t.Fatalf("status = %+v", status)
	}
	// Stopping an apparently exited pending launch while the container
	// still runs must keep its credentials so a delayed exec cannot become
	// orphaned or split from a retry.
	stopped, err := stopInstance(ctx, false)
	check(t, err)
	if stopped {
		t.Fatal("stopping a pending launch reported a stopped daemon")
	}
	owner, err := client.ReadContainerOwner()
	check(t, err)
	if !reflect.DeepEqual(owner, &prior) {
		t.Fatalf("owner = %+v", owner)
	}

	// Explicit --force must stop the running container to prevent a delayed
	// exec from starting after stop reports success.
	stopped, err = stopInstance(ctx, true)
	check(t, err)
	if !stopped {
		t.Fatal("a forced stop reported nothing stopped")
	}
	if !strings.Contains(engineCalls(), "stop test-container-id") {
		t.Fatalf("%s", engineCalls())
	}
	if pathExists(client.ContainerOwnerPath()) {
		t.Fatal("the owner record survived a forced stop")
	}

	// Restore the running container + pending owner for the retry check.
	engineState(t, "worker")
	check(t, client.WriteContainerOwner(&prior))

	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		defer stream.Close()
		ping, ok := readRequest(t, stream)
		if !ok {
			return
		}
		if _, isPing := ping.Action.(client.DaemonActionPing); !isPing {
			t.Errorf("expected Ping, got %#v", ping.Action)
			return
		}
		// The delayed first exec wins the store lock. Only its original
		// token authenticates, regardless of what the retry wrote.
		if ping.Token != prior.Token {
			t.Errorf("ping token = %q", ping.Token)
		}
		execToken, err := os.ReadFile(filepath.Join(core.DataDir(), "exec.token"))
		if err != nil || string(execToken) != prior.Token {
			t.Errorf("exec token = %q, %v", execToken, err)
		}
		if record, err := client.ReadContainerOwner(); err != nil || record == nil || record.Token != prior.Token {
			t.Errorf("owner = %+v, %v", record, err)
		}
		answer(t, stream, ping, client.DaemonOk[client.DaemonReply](client.DaemonReplyPong{}))
	})
	startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
	check(t, err)
	defer startup.release()
	bounded, cancel := context.WithTimeout(ctx, 10*time.Second)
	connected, err := connectOrStartHolding(bounded, startup)
	cancel()
	wait(10 * time.Second)
	check(t, err)
	connected.Close()
	published, err := client.ReadMetadataAny()
	check(t, err)
	if !prior.Matches(&published) {
		t.Fatalf("published = %+v", published)
	}
}

func TestFailingToRecordLegacyContainerOwnerLeavesDaemonRunning(t *testing.T) {
	if fakePodman(t, "MJ_TEST_LEGACY_OWNER_REQUIRED") {
		return
	}
	ctx := context.Background()
	engineState(t, "worker")
	spec := configureContainer(t)
	listener, metadata := fakeDaemon(t, 0)
	defer listener.Close()
	runtime := testContainerRuntime
	identity := client.DaemonIdentity{
		InstanceID:   "legacy-container",
		BuildVersion: metadata.BuildVersion,
		Placement:    client.DaemonPlacement{Kind: client.DaemonPlacementKindContainer, Name: spec.name, Runtime: &runtime},
	}
	check(t, writeLauncherMetadata(&metadata))
	engineState(t, "ps-error")
	startup, err := acquireStartGuard(ctx, filepath.Join(core.DataDir(), "daemon-start.lock"))
	check(t, err)
	bounded, cancel := context.WithTimeout(ctx, 5*time.Second)
	err = replaceDaemon(bounded, startup, &metadata, &identity.Placement)
	cancel()
	if err == nil || !strings.Contains(err.Error(), "simulated engine ps failure") {
		t.Fatalf("unexpected error: %v", err)
	}
	expectNoConnection(t, listener, 100*time.Millisecond)
	startup.release()

	for _, force := range []bool{false, true} {
		wait := runBeside(t, func() {
			stream, err := listener.Accept()
			if err != nil {
				t.Errorf("accept: %v", err)
				return
			}
			defer stream.Close()
			request, ok := readRequest(t, stream)
			if !ok {
				return
			}
			if _, isIdentity := request.Action.(client.DaemonActionIdentity); !isIdentity {
				t.Errorf("expected Identity, got %#v", request.Action)
				return
			}
			answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyIdentity{Identity: identity}))
			if read, err := stream.Read(make([]byte, 1)); read != 0 || !errors.Is(err, io.EOF) {
				t.Errorf("client sent Stop after seeding failed: %d, %v", read, err)
			}
		})
		bounded, cancel := context.WithTimeout(ctx, 5*time.Second)
		_, err := stopInstance(bounded, force)
		cancel()
		wait(5 * time.Second)
		if err == nil {
			t.Fatal("stop reported success without an owner record")
		}
		if !strings.Contains(err.Error(), "left running") || !strings.Contains(err.Error(), "simulated engine ps failure") {
			t.Fatalf("%v", err)
		}
	}
	current, err := client.ReadMetadataAny()
	check(t, err)
	if !sameDaemonEndpoint(&current, &metadata) {
		t.Fatalf("daemon.json = %+v", current)
	}
	if pathExists(client.ContainerOwnerPath()) {
		t.Fatal("an owner record was written")
	}
	if countLines(engineCalls(), "stop ") > 0 {
		t.Fatalf("%s", engineCalls())
	}
}

func TestForceStopRemovesUnreadableOwnerButPreservesValidOwnerOnEngineFailure(t *testing.T) {
	if fakePodman(t, "MJ_TEST_CLEAR_UNREADABLE_CONTAINER_OWNER") {
		return
	}
	ctx := context.Background()
	listener, metadata := fakeDaemon(t, 0)
	listener.Close()
	for _, published := range []bool{false, true} {
		check(t, os.WriteFile(client.ContainerOwnerPath(), []byte("{"), 0o644))
		if published {
			check(t, writeLauncherMetadata(&metadata))
		}
		_, discovery := decideExistingDaemon(ctx)
		if discovery == nil {
			t.Fatal("discovery accepted an unreadable owner")
		}
		_, stopped := stopInstance(ctx, false)
		for _, err := range []error{discovery, stopped} {
			if err == nil || !strings.Contains(err.Error(), "daemon-container.json") || !strings.Contains(err.Error(), "mj daemon stop --force") {
				t.Fatalf("%v", err)
			}
		}
		if pathExists(client.MetadataPath()) != published {
			t.Fatalf("daemon.json exists: %t", pathExists(client.MetadataPath()))
		}
		if !pathExists(client.ContainerOwnerPath()) {
			t.Fatal("the unreadable owner was removed without --force")
		}
		forced, err := stopInstance(ctx, true)
		check(t, err)
		if !forced {
			t.Fatal("a forced stop reported nothing stopped")
		}
		if pathExists(client.MetadataPath()) || pathExists(client.ContainerOwnerPath()) {
			t.Fatal("a forced stop left a record")
		}
		existing, err := decideExistingDaemon(ctx)
		check(t, err)
		if _, absent := existing.(existingDaemonAbsent); !absent {
			t.Fatalf("existing = %#v", existing)
		}
	}

	engineState(t, "ps-error")
	check(t, writeLauncherMetadata(&metadata))
	record := testOwner(&metadata, testContainerID, "daemon")
	check(t, client.WriteContainerOwner(&record))
	_, err := stopInstance(ctx, true)
	if err == nil || !strings.Contains(err.Error(), "simulated engine ps failure") {
		t.Fatalf("unexpected error: %v", err)
	}
	current, err := client.ReadMetadataAny()
	check(t, err)
	if !record.Matches(&current) {
		t.Fatalf("daemon.json = %+v", current)
	}
	owner, err := client.ReadContainerOwner()
	check(t, err)
	if !reflect.DeepEqual(owner, &record) {
		t.Fatalf("owner = %+v", owner)
	}
	if countLines(engineCalls(), "stop ") > 0 {
		t.Fatalf("%s", engineCalls())
	}
}

// Hard-won: #1568: stop treated a container's closed socket as released ownership.
func TestLegacyContainerStopRefusesSilenceAndSeedsAnAnsweringOwner(t *testing.T) {
	if fakePodman(t, "MJ_TEST_LEGACY_CONTAINER_STOP") {
		return
	}
	ctx := context.Background()
	listener, metadata := fakeDaemon(t, 0)
	listener.Close()
	check(t, writeLauncherMetadata(&metadata))
	_, discovery := decideExistingDaemon(ctx)
	if discovery == nil {
		t.Fatal("discovery allowed an untracked container")
	}
	_, handoff := askForHandoff(ctx, &metadata)
	_, stopped := stopInstance(ctx, false)
	for _, err := range []error{discovery, handoff, stopped} {
		if err == nil || !strings.Contains(err.Error(), "mj daemon stop --force") {
			t.Fatalf("%v", err)
		}
	}
	if !pathExists(client.MetadataPath()) {
		t.Fatal("daemon.json was removed without --force")
	}
	forced, err := stopInstance(ctx, true)
	check(t, err)
	if !forced {
		t.Fatal("a forced stop reported nothing stopped")
	}
	if pathExists(client.MetadataPath()) {
		t.Fatal("a forced stop left daemon.json")
	}

	engineState(t, "daemon")
	spec := configureContainer(t)
	listener, metadata = fakeDaemon(t, 0)
	check(t, writeLauncherMetadata(&metadata))
	runtime := testContainerRuntime
	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		request, ok := readRequest(t, stream)
		if !ok {
			stream.Close()
			return
		}
		if _, isIdentity := request.Action.(client.DaemonActionIdentity); !isIdentity {
			t.Errorf("expected Identity, got %#v", request.Action)
		}
		identity := client.DaemonIdentity{
			InstanceID:   "legacy-container",
			BuildVersion: metadata.BuildVersion,
			Placement:    client.DaemonPlacement{Kind: client.DaemonPlacementKindContainer, Name: spec.name, Runtime: &runtime},
		}
		answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyIdentity{Identity: identity}))
		request, ok = readRequest(t, stream)
		if !ok {
			stream.Close()
			return
		}
		if _, isStop := request.Action.(client.DaemonActionStop); !isStop {
			t.Errorf("expected Stop, got %#v", request.Action)
		}
		record, err := client.ReadContainerOwner()
		if err != nil || record == nil || !record.Matches(&metadata) || record.ContainerID != testContainerID {
			t.Errorf("owner = %+v, %v", record, err)
		}
		answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyDone{}))
		stream.Close()
		listener.Close()
		// The endpoint goes away before the daemon process, as in real
		// teardown. Wait until the client observes that process.
		for !strings.Contains(engineCalls(), "top test-container-id args") {
			time.Sleep(10 * time.Millisecond)
		}
		if !pathExists(client.MetadataPath()) || !pathExists(client.ContainerOwnerPath()) {
			t.Errorf("the records went before the daemon process did")
		}
		engineState(t, "worker")
	})
	bounded, cancel := context.WithTimeout(ctx, 10*time.Second)
	forced, err = stopInstance(bounded, true)
	cancel()
	wait(10 * time.Second)
	check(t, err)
	if !forced {
		t.Fatal("the stop reported nothing stopped")
	}
	if strings.Contains(engineCalls(), "stop test-container-id") {
		t.Fatalf("%s", engineCalls())
	}
	if pathExists(client.MetadataPath()) || pathExists(client.ContainerOwnerPath()) {
		t.Fatal("the stop left a record")
	}
}

// listenerAddress is the address a test listener serves.
func listenerAddress(listener net.Listener) netip.AddrPort {
	return netip.MustParseAddrPort(listener.Addr().String())
}

// expectNoConnection is Rust's `timeout(limit, listener.accept()).is_err()`:
// nothing connects within limit.
func expectNoConnection(t *testing.T, listener net.Listener, limit time.Duration) {
	t.Helper()
	tcp, ok := listener.(*net.TCPListener)
	if !ok {
		t.Fatalf("listener %T has no deadline", listener)
	}
	check(t, tcp.SetDeadline(time.Now().Add(limit)))
	defer func() { check(t, tcp.SetDeadline(time.Time{})) }()
	connection, err := tcp.Accept()
	if err == nil {
		connection.Close()
		t.Fatal("a connection arrived")
	}
	var timeout net.Error
	if !errors.As(err, &timeout) || !timeout.Timeout() {
		t.Fatalf("accept failed: %v", err)
	}
}
