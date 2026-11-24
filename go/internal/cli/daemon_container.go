// Port of mj-cli/src/daemon/container.rs.
//
// Launching `mj daemon-run` in a durable, container-native container.
//
// The container is the daemon's machine. It has its own filesystem, its own
// network namespace, and one named volume mounted at containerVolumeMount.
// The host supplies only three things: the engine command, a published
// loopback port, and the token. No host directory, network namespace, PID
// namespace or uid is shared with the container. A host directory the user
// explicitly wants inside a session belongs in `[daemon] mounts`.
//
// The container holds a `sleep infinity` placeholder so it can be reused
// across daemon processes. A daemon replacement starts a new daemon inside
// the same container; workers started by the daemon are processes in that
// container, and the data volume outlives both. The host client publishes
// `daemon.json` itself, because the container cannot write the host data
// directory.
//
// The module's generic names carry a `container` prefix in this flat
// package (containerEngine, containerFailure, stopContainer, ...).

package cli

import (
	"context"
	"net/netip"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// containerInstanceLabel is the label carrying the instance identity,
// shared by every container this instance's daemons ever ran in.
const containerInstanceLabel = "ai.brokk.mjolnir.daemon-instance"

// containerQueryTimeout bounds short engine queries: inspect, top, port,
// exec of the launch script.
const containerQueryTimeout = 60 * time.Second

// containerPullTimeout: pulling the agent-dev image is gigabytes on a first
// start.
const containerPullTimeout = 30 * 60 * time.Second

const containerOutputLimit = 1024 * 1024

// containerImageExecutable is the `mj` the image's builder stage installs.
const containerImageExecutable = "/opt/mjolnir/mj"

// containerRevisionLabel is the label naming the Git revision the image's
// `mj` was built from.
const containerRevisionLabel = "org.brokk.mjolnir.revision"

// containerVolumeMount is the one volume every daemon process of this
// instance uses.
const containerVolumeMount = "/var/lib/mjolnir"

// containerPort is the port the daemon listens on inside its own network
// namespace. The host publishes it on a random loopback port, so containers
// never collide.
const containerPort uint16 = 8787

// containerLaunch is a daemon started in its container.
type containerLaunch struct {
	runtime       core.DaemonRuntime
	containerID   string
	containerName string
	metadata      client.DaemonMetadata
}

// containerPlacement is where the configuration puts the daemon on this
// computer, decided once for both the launch and the check that keeps or
// replaces a running daemon: containerPlacementContainer or
// containerPlacementHost (no image carries this build; the daemon runs on
// the host).
type containerPlacement interface{ isContainerPlacement() }

type containerPlacementContainer struct{ spec *containerSpec }

type containerPlacementHost struct{ reason string }

func (containerPlacementContainer) isContainerPlacement() {}
func (containerPlacementHost) isContainerPlacement()      {}

// resolveContainerPlacement resolves config (a container runtime) to the
// container the daemon runs in, pulling the image when it is not here yet
// (Rust `container::placement`).
func resolveContainerPlacement(ctx context.Context, config *core.DaemonConfig) (containerPlacement, error) {
	panic("unported: mj-cli/src/daemon/container.rs placement")
}

// launchContainerDaemon starts a daemon in the container spec describes,
// creating the container and its volume when they do not exist yet (Rust
// `container::launch`).
func launchContainerDaemon(ctx context.Context, spec *containerSpec, priorUnpublished *client.ContainerDaemonRecord) (*containerLaunch, error) {
	panic("unported: mj-cli/src/daemon/container.rs launch")
}

func containerDaemonMetadata(spec *containerSpec, address netip.AddrPort, token string) client.DaemonMetadata {
	panic("unported: mj-cli/src/daemon/container.rs daemon_metadata")
}

type containerDaemonStatusKind int

const (
	containerDaemonRunning containerDaemonStatusKind = iota
	containerDaemonExited
	containerDaemonUnknown
)

// containerDaemonStatus is Rust's `ContainerDaemonStatus`:
// containerRunning describes a containerDaemonExited, reason a
// containerDaemonUnknown. Values compare with ==.
type containerDaemonStatus struct {
	kind             containerDaemonStatusKind
	containerRunning bool
	reason           string
}

// resolveContainerID is the id of the container named name, or false when
// the engine lists none.
func resolveContainerID(ctx context.Context, runtime core.DaemonRuntime, name string) (string, bool, error) {
	panic("unported: mj-cli/src/daemon/container.rs resolve_container_id")
}

// resolveContainerIDByName is Rust's `resolve_id_by_name`. Container names
// may contain literal dots in addition to letters, digits, hyphens, and
// underscores. Docker matches names with a leading slash while Podman omits
// it, so it accepts an optional leading slash and verifies the exact name
// from the formatted output.
func resolveContainerIDByName(ctx context.Context, program, name string) (string, bool, error) {
	panic("unported: mj-cli/src/daemon/container.rs resolve_id_by_name")
}

func parseContainerIDForName(program, stdout, name string) (string, bool, error) {
	panic("unported: mj-cli/src/daemon/container.rs parse_container_id_for_name")
}

func listContainerIDs(ctx context.Context, program, filter string) (string, error) {
	return listContainers(ctx, program, filter, "{{.ID}}")
}

func listContainers(ctx context.Context, program, filter, format string) (string, error) {
	panic("unported: mj-cli/src/daemon/container.rs list_containers")
}

func containerIsRunning(ctx context.Context, program, id string) (bool, error) {
	panic("unported: mj-cli/src/daemon/container.rs container_is_running")
}

// containerTop lists a field of the container's processes. Docker hands its
// arguments to ps(1); Podman takes field names.
func containerTop(ctx context.Context, program, id, field string) (string, error) {
	panic("unported: mj-cli/src/daemon/container.rs container_top")
}

func probeContainerDaemon(ctx context.Context, runtime core.DaemonRuntime, containerID string) containerDaemonStatus {
	panic("unported: mj-cli/src/daemon/container.rs probe_container_daemon")
}

// topOutputHasDaemonRun is whether a `top args` listing shows `mj
// daemon-run` or its waiter, including through the transient launch shell
// that execs the image binary in place.
func topOutputHasDaemonRun(stdout string) bool {
	panic("unported: mj-cli/src/daemon/container.rs top_output_has_daemon_run")
}

// stopContainer stops the daemon's container when it does not answer the
// protocol stop (Rust `container::stop`). This is the forced path; it stops
// the workers in the container too.
func stopContainer(ctx context.Context, runtime core.DaemonRuntime, name string) error {
	panic("unported: mj-cli/src/daemon/container.rs stop")
}

// containerLogTail is the last part of the daemon log inside the container,
// for a launch failure (Rust `container::log_tail`).
func containerLogTail(ctx context.Context, program, name string) string {
	panic("unported: mj-cli/src/daemon/container.rs log_tail")
}

// execInContainer runs `mj <args>` inside the configured daemon container,
// with the same roots the daemon uses. It returns false when this is a host
// placement, so the caller runs the command locally. The nested command
// carries the container's name in core.DaemonContainerEnv, like the daemon:
// it runs in the daemon container, so it uses the image's harnesses and
// cannot try to launch another container.
func execInContainer(ctx context.Context, args []string, interactive bool) (bool, error) {
	panic("unported: mj-cli/src/daemon/container.rs exec_in_container")
}

// thisRevision is this client's Git revision, which the daemon image's label
// must carry.
func thisRevision() (string, bool) {
	panic("unported: mj-cli/src/daemon/container.rs this_revision")
}

// inspectImage is an image present here: its id and the revision its `mj`
// was built from (nil when unlabelled), or found false.
func inspectImage(ctx context.Context, program, image string) (id string, revision *string, found bool, err error) {
	panic("unported: mj-cli/src/daemon/container.rs inspect_image")
}

// resolveImage is the id of an image whose `mj` is this build: the
// configured image, or this build's default image, or, with no image
// configured, any image here labelled with this revision. A non-empty
// unavailable explains why there is none.
func resolveImage(ctx context.Context, program string, config *core.DaemonConfig) (id string, unavailable string, err error) {
	panic("unported: mj-cli/src/daemon/container.rs resolve_image")
}

// containerSpec is everything that decides what the daemon container looks
// like. Its digest names the container, so a changed spec gets a new
// container instead of altering one that may still hold workers.
type containerSpec struct {
	instance string
	name     string
	// image is the image's id, so a retagged image gets a container of its
	// own.
	image  string
	engine core.DaemonRuntime
	volume string
	// mounts are explicit host directories the user asked to expose inside
	// the container, at the same absolute path. Nothing is mounted by
	// default.
	mounts []string
}

func (s *containerSpec) program() string {
	program, ok := s.engine.Engine()
	if !ok {
		panic("a container spec has an engine")
	}
	return program
}

func (s *containerSpec) runtime() core.DaemonRuntime {
	return s.engine
}

// containerSpecForThisHost is Rust's `ContainerSpec::for_this_host`.
func containerSpecForThisHost(engine core.DaemonRuntime, config *core.DaemonConfig, image string) (*containerSpec, error) {
	panic("unported: mj-cli/src/daemon/container.rs ContainerSpec::for_this_host")
}

func (s *containerSpec) runCommand() []string {
	panic("unported: mj-cli/src/daemon/container.rs ContainerSpec::run_command")
}

// publishedPort is the container engine's published loopback port for the
// daemon's fixed inside port.
func publishedPort(ctx context.Context, program, name string) (uint16, error) {
	panic("unported: mj-cli/src/daemon/container.rs published_port")
}

func shortDigest(bytes []byte) string {
	panic("unported: mj-cli/src/daemon/container.rs short_digest")
}

// containerEngine runs the engine through core.RunBounded (Rust `engine`).
func containerEngine(ctx context.Context, program string, args []string, timeout time.Duration) (core.ProcessOutput, error) {
	panic("unported: mj-cli/src/daemon/container.rs engine")
}

// containerEngineWithEnv is containerEngine with environment added (Rust
// `engine_with_env`).
func containerEngineWithEnv(ctx context.Context, program string, args []string, environment [][2]string, timeout time.Duration) (core.ProcessOutput, error) {
	panic("unported: mj-cli/src/daemon/container.rs engine_with_env")
}

// containerFailure is the error of an engine command that did not succeed
// (Rust `failure`).
func containerFailure(program, action string, output *core.ProcessOutput) error {
	panic("unported: mj-cli/src/daemon/container.rs failure")
}

// removeIdlePredecessors removes this instance's containers from earlier
// specifications once nothing but their placeholder runs in them. A
// container that still holds workers is left alone and blocks a second
// container from opening the same volume.
func removeIdlePredecessors(ctx context.Context, program string, spec *containerSpec) error {
	panic("unported: mj-cli/src/daemon/container.rs remove_idle_predecessors")
}

// processesIn is how many processes run in a container, or false when it is
// not running.
func processesIn(ctx context.Context, program, id string) (int, bool, error) {
	panic("unported: mj-cli/src/daemon/container.rs processes_in")
}

// ensureContainerRunning is Rust's `ensure_running`.
func ensureContainerRunning(ctx context.Context, program string, spec *containerSpec) error {
	panic("unported: mj-cli/src/daemon/container.rs ensure_running")
}

// execContainerDaemon starts a daemon process in the container's own
// filesystem (Rust `exec_daemon`). The token and the data roots are
// environment, never arguments, and the daemon's output goes to the volume's
// log.
func execContainerDaemon(ctx context.Context, program string, spec *containerSpec, id, token string) error {
	panic("unported: mj-cli/src/daemon/container.rs exec_daemon")
}
