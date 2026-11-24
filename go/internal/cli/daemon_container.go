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
	"crypto/sha256"
	"errors"
	"fmt"
	"log/slog"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/x/term"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
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
	program, ok := config.Runtime.Engine()
	if !ok {
		return nil, errors.New("the daemon container launch needs a container runtime")
	}
	path, pathOK := os.LookupEnv("PATH")
	if _, found := harnessinstall.FindProgramOnPath(program, path, pathOK); !found {
		return containerPlacementHost{reason: program + " is not installed on this computer"}, nil
	}
	image, unavailable, err := resolveImage(ctx, program, config)
	if err != nil {
		return nil, err
	}
	if unavailable != "" {
		return containerPlacementHost{reason: unavailable}, nil
	}
	spec, err := containerSpecForThisHost(config.Runtime, config, image)
	if err != nil {
		return nil, err
	}
	return containerPlacementContainer{spec: spec}, nil
}

// launchContainerDaemon starts a daemon in the container spec describes,
// creating the container and its volume when they do not exist yet (Rust
// `container::launch`).
func launchContainerDaemon(ctx context.Context, spec *containerSpec, priorUnpublished *client.ContainerDaemonRecord) (*containerLaunch, error) {
	program := spec.program()
	if err := removeIdlePredecessors(ctx, program, spec); err != nil {
		return nil, err
	}
	if err := ensureContainerRunning(ctx, program, spec); err != nil {
		return nil, err
	}
	port, err := publishedPort(ctx, program, spec.name)
	if err != nil {
		return nil, err
	}
	containerID, found, err := resolveContainerID(ctx, spec.runtime(), spec.name)
	if err != nil {
		return nil, err
	}
	if !found {
		return nil, fmt.Errorf("daemon container %s disappeared before launch", spec.name)
	}
	address := netip.AddrPortFrom(netip.AddrFrom4([4]byte{127, 0, 0, 1}), port)
	var token string
	if prior := priorUnpublished; prior != nil && prior.Runtime == spec.runtime() &&
		prior.ContainerID == containerID && prior.Address == address {
		// Both execs may reach the store lock. Either winner must accept the
		// published token.
		token = prior.Token
	} else {
		if prior != nil {
			if err := client.RemoveContainerOwnerIfMatching(prior.Address, prior.Token); err != nil {
				return nil, err
			}
		}
		if token, err = randomToken(); err != nil {
			return nil, err
		}
	}
	metadata := containerDaemonMetadata(spec, address, token)
	protocol := metadata.ProtocolVersion
	if err := client.WriteContainerOwner(&client.ContainerDaemonRecord{
		Schema:          client.ContainerOwnerSchema,
		ProtocolVersion: &protocol,
		Runtime:         spec.runtime(),
		ContainerID:     containerID,
		ContainerName:   spec.name,
		Address:         metadata.Address,
		Token:           metadata.Token,
		StartedAt:       metadata.StartedAt,
	}); err != nil {
		return nil, err
	}
	if err := execContainerDaemon(ctx, program, spec, containerID, metadata.Token); err != nil {
		return nil, err
	}
	return &containerLaunch{
		runtime:       spec.runtime(),
		containerID:   containerID,
		containerName: spec.name,
		metadata:      metadata,
	}, nil
}

func containerDaemonMetadata(spec *containerSpec, address netip.AddrPort, token string) client.DaemonMetadata {
	runtime := spec.engine
	return client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		// The daemon's PID is a container PID. Clients ask `Identity`; this
		// field is the frozen status shape and is never a host PID here.
		Pid:          0,
		Address:      address,
		Token:        token,
		StartedAt:    time.Now().UTC().Format("2006-01-02T15:04:05Z"),
		BuildVersion: client.ThisBuild().Published(),
		InstanceID:   "",
		Placement: client.DaemonPlacement{
			Kind:    client.DaemonPlacementKindContainer,
			Name:    spec.name,
			Runtime: &runtime,
		},
	}
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
	program, ok := runtime.Engine()
	if !ok {
		return "", false, errors.New("the daemon container has no engine")
	}
	return resolveContainerIDByName(ctx, program, name)
}

// resolveContainerIDByName is Rust's `resolve_id_by_name`. Container names
// may contain literal dots in addition to letters, digits, hyphens, and
// underscores. Docker matches names with a leading slash while Podman omits
// it, so it accepts an optional leading slash and verifies the exact name
// from the formatted output.
func resolveContainerIDByName(ctx context.Context, program, name string) (string, bool, error) {
	valid := name != "" && !strings.ContainsFunc(name, func(r rune) bool {
		return !(r < 0x80 && (r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' ||
			r == '.' || r == '-' || r == '_'))
	})
	if !valid {
		return "", false, fmt.Errorf("invalid daemon container name: %s", core.RustDebugString(name))
	}
	pattern := strings.ReplaceAll(name, ".", "[.]")
	listed, err := listContainers(ctx, program, "name=^/?"+pattern+"$", "{{.ID}}\t{{.Names}}")
	if err != nil {
		return "", false, err
	}
	return parseContainerIDForName(program, listed, name)
}

func parseContainerIDForName(program, stdout, name string) (string, bool, error) {
	var ids []string
	for _, line := range rustLines(stdout) {
		id, names, found := strings.Cut(line, "\t")
		if !found {
			continue
		}
		id = strings.TrimSpace(id)
		matched := false
		for _, candidate := range strings.Split(names, ",") {
			if strings.TrimLeft(strings.TrimSpace(candidate), "/") == name {
				matched = true
				break
			}
		}
		if id != "" && matched {
			ids = append(ids, id)
		}
	}
	if len(ids) > 1 {
		return "", false, fmt.Errorf("%s listed multiple containers named %s", program, name)
	}
	if len(ids) == 0 {
		return "", false, nil
	}
	return ids[0], true, nil
}

// rustLines is Rust's str::lines: split at `\n`, drop one `\r` before it,
// and yield no empty line after a final newline.
func rustLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for index, line := range lines {
		lines[index] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

func listContainerIDs(ctx context.Context, program, filter string) (string, error) {
	return listContainers(ctx, program, filter, "{{.ID}}")
}

func listContainers(ctx context.Context, program, filter, format string) (string, error) {
	output, err := containerEngine(ctx, program,
		[]string{"ps", "--all", "--no-trunc", "--filter", filter, "--format", format},
		containerQueryTimeout)
	if err != nil {
		return "", fmt.Errorf("list containers matching %s: %w", filter, err)
	}
	if !output.Status.Success() {
		return "", containerFailure(program, "list containers matching "+filter, &output)
	}
	if _, err := cliFromUTF8(output.Stdout); err != nil {
		return "", fmt.Errorf("container listing was not valid UTF-8: %w", err)
	}
	return string(output.Stdout), nil
}

func containerIsRunning(ctx context.Context, program, id string) (bool, error) {
	output, err := containerEngine(ctx, program,
		[]string{"inspect", "--format={{.State.Running}}", id}, containerQueryTimeout)
	if err != nil {
		return false, fmt.Errorf("inspect container %s: %w", id, err)
	}
	if !output.Status.Success() {
		return false, containerFailure(program, "inspect container "+id, &output)
	}
	switch state := strings.TrimSpace(core.LossyUTF8(output.Stdout)); state {
	case "true":
		return true, nil
	case "false":
		return false, nil
	default:
		return false, fmt.Errorf("%s reported an unexpected running state for container %s: %s",
			program, id, core.RustDebugString(state))
	}
}

// containerTop lists a field of the container's processes. Docker hands its
// arguments to ps(1); Podman takes field names.
func containerTop(ctx context.Context, program, id, field string) (string, error) {
	args := []string{"top", id}
	if program == "docker" {
		args = append(args, "-o")
	}
	args = append(args, field)
	output, err := containerEngine(ctx, program, args, containerQueryTimeout)
	if err != nil {
		return "", fmt.Errorf("list processes in container %s: %w", id, err)
	}
	if !output.Status.Success() {
		return "", containerFailure(program, "list processes in container "+id, &output)
	}
	return core.LossyUTF8(output.Stdout), nil
}

func probeContainerDaemon(ctx context.Context, runtime core.DaemonRuntime, containerID string) containerDaemonStatus {
	probe := func() (containerDaemonStatus, error) {
		program, ok := runtime.Engine()
		if !ok {
			return containerDaemonStatus{}, errors.New("the daemon container has no engine")
		}
		ids, err := listContainerIDs(ctx, program, "id="+containerID)
		if err != nil {
			return containerDaemonStatus{}, err
		}
		listed := false
		for _, id := range rustLines(ids) {
			if strings.TrimSpace(id) == containerID {
				listed = true
				break
			}
		}
		if !listed {
			return containerDaemonStatus{kind: containerDaemonExited, containerRunning: false}, nil
		}
		running, err := containerIsRunning(ctx, program, containerID)
		if err != nil {
			return containerDaemonStatus{}, err
		}
		if !running {
			return containerDaemonStatus{kind: containerDaemonExited, containerRunning: false}, nil
		}
		top, err := containerTop(ctx, program, containerID, "args")
		if err != nil {
			return containerDaemonStatus{}, err
		}
		if topOutputHasDaemonRun(top) {
			return containerDaemonStatus{kind: containerDaemonRunning}, nil
		}
		return containerDaemonStatus{kind: containerDaemonExited, containerRunning: true}, nil
	}
	status, err := probe()
	if err != nil {
		return containerDaemonStatus{kind: containerDaemonUnknown, reason: err.Error()}
	}
	return status
}

// topOutputHasDaemonRun is whether a `top args` listing shows `mj
// daemon-run` or its waiter, including through the transient launch shell
// that execs the image binary in place.
func topOutputHasDaemonRun(stdout string) bool {
	lines := rustLines(stdout)
	if len(lines) > 0 {
		lines = lines[1:]
	}
	for _, line := range lines {
		argv := strings.Fields(line)
		if len(argv) == 0 {
			continue
		}
		program, ok := pathFileName(argv[0])
		if !ok {
			continue
		}
		next := func(index int) (string, bool) {
			if index < len(argv) {
				return argv[index], true
			}
			return "", false
		}
		second, _ := next(1)
		switch {
		case second == "daemon-run" || second == "daemon-wait-exit":
			if program == "mj" {
				return true
			}
		case second == "-c" && program == "sh":
			// The transient launch shell execs the image binary in place.
			index := 2
			command, present := next(index)
			if present && command == "exec" {
				index++
				command, present = next(index)
			}
			subcommand, _ := next(index + 1)
			if present && command == containerImageExecutable &&
				(subcommand == "daemon-run" || subcommand == "daemon-wait-exit") {
				return true
			}
		}
	}
	return false
}

// pathFileName is Rust's `Path::new(path).file_name()` for a Unix path: the
// last component, or false when the path ends in `..`, `.` or the root.
func pathFileName(path string) (string, bool) {
	trimmed := strings.TrimRight(path, "/")
	if trimmed == "" {
		return "", false
	}
	name := trimmed[strings.LastIndexByte(trimmed, '/')+1:]
	if name == "." || name == ".." {
		return "", false
	}
	return name, true
}

// stopContainer stops the daemon's container when it does not answer the
// protocol stop (Rust `container::stop`). This is the forced path; it stops
// the workers in the container too.
func stopContainer(ctx context.Context, runtime core.DaemonRuntime, name string) error {
	program, ok := runtime.Engine()
	if !ok {
		return errors.New("the daemon container has no engine")
	}
	output, err := containerEngine(ctx, program, []string{"stop", name}, containerQueryTimeout)
	if err != nil {
		return err
	}
	if !output.Status.Success() {
		return containerFailure(program, "stop "+name, &output)
	}
	return nil
}

// containerLogTail is the last part of the daemon log inside the container,
// for a launch failure (Rust `container::log_tail`).
func containerLogTail(ctx context.Context, program, name string) string {
	output, err := containerEngine(ctx, program,
		[]string{"exec", name, "tail", "-c", "65536", containerVolumeMount + "/daemon.log"},
		containerQueryTimeout)
	if err == nil && output.Status.Success() {
		return core.LossyUTF8(output.Stdout)
	}
	logs, err := containerEngine(ctx, program, []string{"logs", "--tail", "60", name}, containerQueryTimeout)
	if err != nil {
		return ""
	}
	return core.LossyUTF8(logs.Stdout)
}

// execInContainer runs `mj <args>` inside the configured daemon container,
// with the same roots the daemon uses. It returns false when this is a host
// placement, so the caller runs the command locally. The nested command
// carries the container's name in core.DaemonContainerEnv, like the daemon:
// it runs in the daemon container, so it uses the image's harnesses and
// cannot try to launch another container.
func execInContainer(ctx context.Context, args []string, interactive bool) (bool, error) {
	if _, inContainer := core.DaemonContainer(); inContainer {
		return false, nil
	}
	config, err := loadDaemonConfig(ctx)
	if err != nil {
		config = core.DefaultDaemonConfig()
	}
	if _, ok := config.Runtime.Engine(); !ok {
		return false, nil
	}
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return false, err
	}
	identity, err := daemon.Identity(ctx)
	daemon.Close()
	if err != nil {
		return false, err
	}
	name, inContainer := identity.Placement.Container()
	if !inContainer || identity.Placement.Runtime == nil {
		return false, nil
	}
	program, ok := identity.Placement.Runtime.Engine()
	if !ok {
		return false, errors.New("the daemon container has no engine")
	}
	engineArgs := []string{"exec"}
	if interactive && term.IsTerminal(os.Stdin.Fd()) && term.IsTerminal(os.Stdout.Fd()) {
		engineArgs = append(engineArgs, "--interactive", "--tty")
	} else {
		engineArgs = append(engineArgs, "--interactive")
	}
	engineArgs = append(engineArgs, name, containerImageExecutable)
	engineArgs = append(engineArgs, args...)
	command := exec.Command(program, engineArgs...)
	command.Env = append(os.Environ(),
		"HOME="+containerVolumeMount+"/home",
		"MJ_DATA_DIR="+containerVolumeMount+"/data",
		"MJ_CONFIG_DIR="+containerVolumeMount+"/config",
		core.DaemonContainerEnv+"="+name,
	)
	status, err := core.RunInherited(command)
	if err != nil {
		return false, err
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

// thisRevision is this client's Git revision, which the daemon image's label
// must carry.
func thisRevision() (string, bool) {
	buildID := core.BuildID()
	index := strings.LastIndex(buildID, "+")
	if index < 0 {
		return "", false
	}
	return buildID[index+1:], true
}

// inspectImage is an image present here: its id and the revision its `mj`
// was built from (nil when unlabelled), or found false.
func inspectImage(ctx context.Context, program, image string) (id string, revision *string, found bool, err error) {
	format := "--format={{.Id}}|{{index .Config.Labels \"" + containerRevisionLabel + "\"}}"
	output, err := containerEngine(ctx, program, []string{"image", "inspect", format, image}, containerQueryTimeout)
	if err != nil {
		return "", nil, false, err
	}
	if !output.Status.Success() {
		return "", nil, false, nil
	}
	text := core.LossyUTF8(output.Stdout)
	rawID, rawRevision, ok := strings.Cut(strings.TrimSpace(text), "|")
	if !ok {
		return "", nil, false, fmt.Errorf("%s reported %s for image %s", program, core.RustDebugString(text), image)
	}
	// Docker prints a missing label as `<no value>`, Podman as nothing.
	if value := strings.TrimSpace(rawRevision); value != "" && value != "<no value>" {
		revision = &value
	}
	return strings.TrimSpace(rawID), revision, true, nil
}

// resolveImage is the id of an image whose `mj` is this build: the
// configured image, or this build's default image, or, with no image
// configured, any image here labelled with this revision. A non-empty
// unavailable explains why there is none.
func resolveImage(ctx context.Context, program string, config *core.DaemonConfig) (id string, unavailable string, err error) {
	wanted, ok := thisRevision()
	if !ok {
		return "", "this mj build does not know its Git revision, so no image can be matched to it", nil
	}
	candidate := core.ReleaseContainerImage
	if config.Image != nil {
		candidate = *config.Image
	}
	foundID, revision, found, err := inspectImage(ctx, program, candidate)
	if err != nil {
		return "", "", err
	}
	if found && revision != nil && *revision == wanted {
		return foundID, "", nil
	}
	if config.Image == nil {
		listed, err := containerEngine(ctx, program, []string{
			"images",
			"--no-trunc",
			"--format={{.ID}}",
			"--filter=label=" + containerRevisionLabel + "=" + wanted,
		}, containerQueryTimeout)
		if err != nil {
			return "", "", err
		}
		if listed.Status.Success() {
			for _, line := range rustLines(core.LossyUTF8(listed.Stdout)) {
				if id := strings.TrimSpace(line); id != "" {
					return id, "", nil
				}
			}
		}
	}
	var pullError error
	if !found {
		startupNotice(fmt.Sprintf("Pulling the daemon container image %s; the first start waits for it.", candidate))
		pulled, err := containerEngine(ctx, program, []string{"pull", candidate}, containerPullTimeout)
		if err != nil {
			return "", "", err
		}
		if pulled.Status.Success() {
			foundID, revision, found, err = inspectImage(ctx, program, candidate)
			if err != nil {
				return "", "", err
			}
		} else {
			pullError = containerFailure(program, "pull "+candidate, &pulled)
		}
	}
	switch {
	case found && revision != nil && *revision == wanted:
		return foundID, "", nil
	case found:
		carried := "(none)"
		if revision != nil {
			carried = *revision
		}
		return "", fmt.Sprintf("image %s carries Mjolnir revision %s, not this build's %s; "+
			"`scripts/build-agent-dev-image.sh %s` builds one for this build from its checkout",
			candidate, carried, wanted, program), nil
	}
	if pullError == nil {
		pullError = fmt.Errorf("image %s is not available", candidate)
	}
	return "", pullError.Error(), nil
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
	data := core.DataDir()
	instance := shortDigest([]byte(data))
	volume := "mj-" + instance + "-data"
	var mounts []string
	for _, mount := range config.Mounts {
		if !filepath.IsAbs(mount) {
			return nil, fmt.Errorf("`[daemon] mounts` entry %s must be an absolute path", mount)
		}
		if _, err := os.Stat(mount); err != nil {
			return nil, fmt.Errorf("`[daemon] mounts` entry %s does not exist on this computer", mount)
		}
		mounts = append(mounts, mount)
	}
	spec := &containerSpec{instance: instance, image: image, engine: engine, volume: volume, mounts: mounts}
	digest := sha256.New()
	program, ok := spec.engine.Engine()
	if !ok {
		program = "host"
	}
	digest.Write([]byte(program))
	digest.Write([]byte{0})
	digest.Write([]byte(spec.image))
	digest.Write([]byte{0})
	digest.Write([]byte(spec.volume))
	for _, mount := range spec.mounts {
		digest.Write([]byte{1})
		digest.Write([]byte(mount))
	}
	hex := core.LowerHex(digest.Sum(nil))
	spec.name = "mj-daemon-" + spec.instance + "-" + hex[:12]
	return spec, nil
}

func (s *containerSpec) runCommand() []string {
	args := []string{
		"run",
		"--detach",
		"--name=" + s.name,
		"--label=" + containerInstanceLabel + "=" + s.instance,
		// The host picks a free loopback port; the container always listens
		// on its own fixed port.
		"--publish=127.0.0.1::" + strconv.Itoa(int(containerPort)),
		"--volume=" + s.volume + ":" + containerVolumeMount,
		"--workdir=" + containerVolumeMount,
		"--entrypoint=sleep",
	}
	for _, mount := range s.mounts {
		args = append(args, "--volume="+mount+":"+mount)
	}
	return append(args, s.image, "infinity")
}

// publishedPort is the container engine's published loopback port for the
// daemon's fixed inside port.
func publishedPort(ctx context.Context, program, name string) (uint16, error) {
	output, err := containerEngine(ctx, program,
		[]string{"port", name, strconv.Itoa(int(containerPort)) + "/tcp"}, containerQueryTimeout)
	if err != nil {
		return 0, err
	}
	if !output.Status.Success() {
		return 0, containerFailure(program, "read the published port of "+name, &output)
	}
	text := core.LossyUTF8(output.Stdout)
	// Docker and Podman both answer `127.0.0.1:<port>`; an IPv6 host is
	// bracketed, so the last colon still separates the port.
	for _, line := range rustLines(text) {
		trimmed := strings.TrimSpace(line)
		last := trimmed[strings.LastIndexByte(trimmed, ':')+1:]
		if port, err := parseRustU16(last); err == nil {
			return port, nil
		}
	}
	return 0, fmt.Errorf("%s reported no loopback port for %s: %s", program, name, core.RustDebugString(text))
}

// parseRustU16 is `str::parse::<u16>`: digits with one optional leading `+`.
func parseRustU16(text string) (uint16, error) {
	value, err := parseRustU64(text)
	if err != nil {
		return 0, err
	}
	if value > 0xFFFF {
		return 0, errors.New("number too large to fit in target type")
	}
	return uint16(value), nil
}

func shortDigest(bytes []byte) string {
	digest := sha256.Sum256(bytes)
	return core.LowerHex(digest[:])[:12]
}

// containerEngine runs the engine through core.RunBounded (Rust `engine`).
func containerEngine(ctx context.Context, program string, args []string, timeout time.Duration) (core.ProcessOutput, error) {
	return containerEngineWithEnv(ctx, program, args, nil, timeout)
}

// containerEngineWithEnv is containerEngine with environment added (Rust
// `engine_with_env`).
func containerEngineWithEnv(ctx context.Context, program string, args []string, environment [][2]string, timeout time.Duration) (core.ProcessOutput, error) {
	command := exec.Command(program, args...)
	if len(environment) > 0 {
		command.Env = os.Environ()
		for _, variable := range environment {
			command.Env = append(command.Env, variable[0]+"="+variable[1])
		}
	}
	output, err := core.RunBounded(ctx, command, containerOutputLimit, timeout)
	if err != nil {
		return core.ProcessOutput{}, fmt.Errorf("run %s: %w", program, err)
	}
	return output, nil
}

// containerFailure is the error of an engine command that did not succeed
// (Rust `failure`).
func containerFailure(program, action string, output *core.ProcessOutput) error {
	return fmt.Errorf("%s could not %s (%s): %s", program, action,
		client.ExitStatusDisplay(output.Status), strings.TrimSpace(core.LossyUTF8(output.Stderr)))
}

// removeIdlePredecessors removes this instance's containers from earlier
// specifications once nothing but their placeholder runs in them. A
// container that still holds workers is left alone and blocks a second
// container from opening the same volume.
func removeIdlePredecessors(ctx context.Context, program string, spec *containerSpec) error {
	listed, err := containerEngine(ctx, program, []string{
		"ps",
		"--all",
		"--format={{.Names}}",
		"--filter=label=" + containerInstanceLabel + "=" + spec.instance,
	}, containerQueryTimeout)
	if err != nil {
		return err
	}
	if !listed.Status.Success() {
		return containerFailure(program, "list daemon containers", &listed)
	}
	for _, name := range rustLines(core.LossyUTF8(listed.Stdout)) {
		name = strings.TrimSpace(name)
		if name == "" || name == spec.name {
			continue
		}
		id, found, err := resolveContainerIDByName(ctx, program, name)
		if err != nil {
			return err
		}
		if !found {
			continue
		}
		count, running, err := processesIn(ctx, program, id)
		if err != nil {
			return err
		}
		if running && count > 1 {
			return fmt.Errorf("an earlier daemon container %s still runs workers; finish or destroy those sessions before changing the daemon container", name)
		}
		removed, err := containerEngine(ctx, program, []string{"rm", "--force", id}, containerQueryTimeout)
		if err != nil {
			return err
		}
		if removed.Status.Success() {
			slog.Info("removed an idle earlier daemon container", "container", name)
		} else {
			slog.Warn("could not remove an idle earlier daemon container",
				"container", name, "error", strings.TrimSpace(core.LossyUTF8(removed.Stderr)))
		}
	}
	return nil
}

// processesIn is how many processes run in a container, or false when it is
// not running.
func processesIn(ctx context.Context, program, id string) (int, bool, error) {
	running, err := containerIsRunning(ctx, program, id)
	if err != nil {
		return 0, false, err
	}
	if !running {
		return 0, false, nil
	}
	top, err := containerTop(ctx, program, id, "pid")
	if err != nil {
		return 0, false, err
	}
	lines := rustLines(top)
	if len(lines) > 0 {
		lines = lines[1:]
	}
	count := 0
	for _, line := range lines {
		if strings.TrimSpace(line) != "" {
			count++
		}
	}
	return count, true, nil
}

// ensureContainerRunning is Rust's `ensure_running`.
func ensureContainerRunning(ctx context.Context, program string, spec *containerSpec) error {
	id, found, err := resolveContainerIDByName(ctx, program, spec.name)
	if err != nil {
		return err
	}
	if found {
		running, err := containerIsRunning(ctx, program, id)
		if err != nil {
			return err
		}
		if running {
			return nil
		}
		started, err := containerEngine(ctx, program, []string{"start", id}, containerQueryTimeout)
		if err != nil {
			return err
		}
		if !started.Status.Success() {
			return containerFailure(program, "start "+spec.name, &started)
		}
		slog.Info("restarted the daemon container", "container", spec.name)
		return nil
	}
	volume, err := containerEngine(ctx, program, []string{"volume", "create", spec.volume}, containerQueryTimeout)
	if err != nil {
		return err
	}
	if !volume.Status.Success() {
		return containerFailure(program, "create data volume "+spec.volume, &volume)
	}
	created, err := containerEngine(ctx, program, spec.runCommand(), containerQueryTimeout)
	if err != nil {
		return err
	}
	if !created.Status.Success() {
		return containerFailure(program, "create "+spec.name, &created)
	}
	slog.Info("created the daemon container", "container", spec.name, "image", spec.image)
	return nil
}

// execContainerDaemon starts a daemon process in the container's own
// filesystem (Rust `exec_daemon`). The token and the data roots are
// environment, never arguments, and the daemon's output goes to the volume's
// log.
func execContainerDaemon(ctx context.Context, program string, spec *containerSpec, id, token string) error {
	script := "exec " + containerImageExecutable + " daemon-wait-exit --exit-dir " + containerVolumeMount +
		"/data/daemon-exits -- " + containerImageExecutable + " daemon-run >>\"$MJ_DAEMON_LOG\" 2>&1"
	args := []string{
		"exec",
		"--detach",
		"--workdir=" + containerVolumeMount,
		"--env=HOME=/var/lib/mjolnir/home",
		"--env=MJ_DATA_DIR=/var/lib/mjolnir/data",
		"--env=MJ_CONFIG_DIR=/var/lib/mjolnir/config",
		"--env=MJ_DAEMON_LISTEN=0.0.0.0:" + strconv.Itoa(int(containerPort)),
		// The token is copied from this process's environment;
		// `--env=NAME` keeps its value out of the engine's argument list.
		"--env=MJ_DAEMON_TOKEN",
		"--env=MJ_DAEMON_CONTAINER=" + spec.name,
		"--env=MJ_DAEMON_CONTAINER_RUNTIME=" + program,
		"--env=MJ_DAEMON_LOG=/var/lib/mjolnir/daemon.log",
		id,
		"sh",
		"-c",
		script,
	}
	output, err := containerEngineWithEnv(ctx, program, args, [][2]string{{"MJ_DAEMON_TOKEN", token}}, containerQueryTimeout)
	if err != nil {
		return err
	}
	if !output.Status.Success() {
		return containerFailure(program, "start mj daemon-run in "+spec.name, &output)
	}
	return nil
}
