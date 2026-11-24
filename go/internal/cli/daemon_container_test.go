// Port of the tests in mj-cli/src/daemon/container.rs.

package cli

import (
	"context"
	"net/netip"
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func TestDockerAndPodmanTopDistinguishDaemonsFromWorkers(t *testing.T) {
	docker := "COMMAND\nsleep infinity\n/opt/mjolnir/mj daemon-run\n"
	podman := "COMMAND\nsleep infinity\nmj daemon-run\n"
	for _, output := range []string{
		docker,
		podman,
		"COMMAND\n/usr/local/bin/mj daemon-run\n",
		"COMMAND\nmj daemon-wait-exit -- /opt/mjolnir/mj daemon-run\n",
		"COMMAND\nsh -c exec /opt/mjolnir/mj daemon-run >>\"$MJ_DAEMON_LOG\" 2>&1\n",
		"COMMAND\n/bin/sh -c exec /opt/mjolnir/mj daemon-wait-exit -- mj daemon-run\n",
	} {
		if !topOutputHasDaemonRun(output) {
			t.Errorf("%s", output)
		}
	}
	workers := "COMMAND\nsleep infinity\n/usr/local/bin/hel worker\n                       /usr/bin/codex app-server\n/bin/sh -c echo daemon-run\n                       grep daemon-run\n"
	if topOutputHasDaemonRun(workers) {
		t.Fatalf("workers read as a daemon: %s", workers)
	}
}

func TestContainerNameListingMatchesBothDockerAndPodmanFormats(t *testing.T) {
	type parsed struct {
		id    string
		found bool
	}
	parse := func(program, stdout, name string) parsed {
		t.Helper()
		id, found, err := parseContainerIDForName(program, stdout, name)
		check(t, err)
		return parsed{id: id, found: found}
	}
	podman := "id-other\tmj-daemon-2\nid-match\tmj-daemon\n"
	if got := parse("podman", podman, "mj-daemon"); got != (parsed{id: "id-match", found: true}) {
		t.Fatalf("podman = %+v", got)
	}
	docker := "id-match\t/mj-daemon\n"
	if got := parse("docker", docker, "mj-daemon"); got != (parsed{id: "id-match", found: true}) {
		t.Fatalf("docker = %+v", got)
	}
	absent := "id-other\tmj-daemon-2\n"
	if got := parse("podman", absent, "mj-daemon"); got != (parsed{}) {
		t.Fatalf("absent = %+v", got)
	}
}

// Rust: #[ignore = "requires live podman socket"].
func TestPodmanProbeDistinguishesStoppedContainerAndLivePlaceholder(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires live podman socket (Rust: #[ignore])")
	}
	ctx := context.Background()
	token, err := randomToken()
	check(t, err)
	name := "mj-test-daemon-probe-" + token
	runtime := core.DaemonRuntimePodman
	image := "docker.io/library/alpine:3"
	run, err := containerEngine(ctx, "podman",
		[]string{"run", "--detach", "--name", name, image, "sleep", "3600"}, containerPullTimeout)
	check(t, err)
	if !run.Status.Success() {
		t.Fatalf("%v", containerFailure("podman", "run test container", &run))
	}
	id := strings.TrimSpace(string(run.Stdout))

	resolvedID, resolvedFound, resolveErr := resolveContainerID(ctx, runtime, name)
	wasRunning, wasRunningErr := containerIsRunning(ctx, "podman", id)
	live := probeContainerDaemon(ctx, runtime, id)
	stopped := stopContainer(ctx, runtime, id)
	isRunning, isRunningErr := containerIsRunning(ctx, "podman", id)
	afterStop := probeContainerDaemon(ctx, runtime, id)
	cleanup, err := containerEngine(ctx, "podman", []string{"rm", "--force", id}, containerQueryTimeout)
	check(t, err)
	if !cleanup.Status.Success() {
		t.Fatalf("%v", containerFailure("podman", "remove test container", &cleanup))
	}

	check(t, resolveErr)
	if !resolvedFound || resolvedID != id {
		t.Fatalf("resolved = %q, %t", resolvedID, resolvedFound)
	}
	check(t, wasRunningErr)
	if !wasRunning {
		t.Fatal("the placeholder must still be running")
	}
	if live != (containerDaemonStatus{kind: containerDaemonExited, containerRunning: true}) {
		t.Fatalf("live = %+v", live)
	}
	check(t, stopped)
	check(t, isRunningErr)
	if isRunning {
		t.Fatal("the stopped container still runs")
	}
	if afterStop != (containerDaemonStatus{kind: containerDaemonExited, containerRunning: false}) {
		t.Fatalf("after stop = %+v", afterStop)
	}
}

func testContainerSpec(mounts []string) *containerSpec {
	return &containerSpec{
		instance: "instance",
		name:     "mj-daemon-instance-abc",
		image:    "image-id",
		engine:   core.DaemonRuntimeDocker,
		volume:   "mj-instance-data",
		mounts:   mounts,
	}
}

// The container is its own machine: a private network and PID namespace,
// one data volume, and no implicit host home or path.
func TestADaemonContainerSharesNothingButItsVolume(t *testing.T) {
	args := testContainerSpec([]string{"/srv/project"}).runCommand()
	has := func(wanted string) bool {
		for _, arg := range args {
			if arg == wanted {
				return true
			}
		}
		return false
	}
	if !has("--publish=127.0.0.1::8787") {
		t.Fatalf("args = %q", args)
	}
	if !has("--volume=mj-instance-data:/var/lib/mjolnir") {
		t.Fatalf("args = %q", args)
	}
	if !has("--volume=/srv/project:/srv/project") {
		t.Fatalf("args = %q", args)
	}
	volumes := 0
	for _, arg := range args {
		if strings.Contains(arg, "--network=host") || strings.Contains(arg, "--pid=host") || strings.Contains(arg, "--userns") {
			t.Fatalf("the container shares a host namespace: %q", args)
		}
		if strings.HasPrefix(arg, "--volume=") {
			volumes++
		}
	}
	if volumes != 2 {
		t.Fatalf("only the data volume and an explicit mount may be present: %q", args)
	}
}

func TestAContainerDaemonPublishesNoHostPid(t *testing.T) {
	metadata := containerDaemonMetadata(testContainerSpec(nil),
		netip.AddrPortFrom(netip.AddrFrom4([4]byte{127, 0, 0, 1}), 43123), "token")
	if metadata.Pid != 0 {
		t.Fatalf("pid = %d", metadata.Pid)
	}
	if metadata.Address.Port() != 43123 {
		t.Fatalf("port = %d", metadata.Address.Port())
	}
	if _, inContainer := metadata.Placement.Container(); !inContainer {
		t.Fatalf("placement = %+v", metadata.Placement)
	}
}
