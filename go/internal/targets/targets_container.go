// Port of mj-targets/src/targets/container.rs.

package targets

import (
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func ContainerTemporaryVolumeName(container string) string {
	return container + "-tmp"
}

type ContainerStorage struct {
	ManagedTemporaryVolume bool
	Attachments            []core.AdditionalMount
}

// InspectContainerStorage reads the mounts the engine reports for a session
// container: whether it carries the managed /tmp volume, and the effective
// access of each recorded attachment.
func InspectContainerStorage(locator *core.TargetsTargetLocator, attachments []core.AdditionalMount, executor core.CommandExecutor) (ContainerStorage, error) {
	// The engine's full `container inspect` JSON carries the mounts too, so a
	// sandbox is read from that instead of a `--format` template.
	if locator.Kind == core.TargetsTargetLocatorSshNitro {
		// The session may still record attachments the user chose; a launch
		// dropped them (with a notice), so the sandbox has none to verify.
		command := nitroInspectCommand(locator.SSH, locator.ContainerID)
		output, err := executor.Execute(&command)
		if err != nil {
			return ContainerStorage{}, err
		}
		if output.Status != 0 {
			return ContainerStorage{}, fmt.Errorf("inspect session container storage failed: %s", lossyText(output.Stderr))
		}
		var inspected []struct {
			Mounts []containerInspectedMount `json:"Mounts"`
		}
		if err := json.Unmarshal(output.Stdout, &inspected); err != nil {
			return ContainerStorage{}, fmt.Errorf("decode session container inspection: %w", err)
		}
		for _, sandbox := range inspected {
			if err := requireMountDestinations(sandbox.Mounts, false); err != nil {
				return ContainerStorage{}, fmt.Errorf("decode session container inspection: %w", err)
			}
		}
		if len(inspected) != 1 {
			return ContainerStorage{}, fmt.Errorf("container inspection returned %d sandboxes instead of one", len(inspected))
		}
		return ContainerStorage{
			ManagedTemporaryVolume: hasManagedTemporaryVolume(inspected[0].Mounts, locator.ContainerID),
			Attachments:            []core.AdditionalMount{},
		}, nil
	}
	var engine string
	var ssh *core.SshTarget
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
		engine = "podman"
	case core.TargetsTargetLocatorLocalDocker:
		engine = "docker"
	case core.TargetsTargetLocatorSshPodman:
		engine, ssh = "podman", locator.SSH
	case core.TargetsTargetLocatorSshDocker:
		engine, ssh = "docker", locator.SSH
	default:
		storage := ContainerStorage{Attachments: []core.AdditionalMount{}}
		if containerEngine, ok := locator.ContainerEngine(); ok && containerEngine == "container" {
			for _, mount := range attachments {
				mount.Access = mount.Access.WithoutOverlay()
				storage.Attachments = append(storage.Attachments, mount)
			}
		}
		return storage, nil
	}
	container := locator.ContainerID
	command := core.NewCommandSpec(engine, []string{"container", "inspect", "--format", "{{json .Mounts}}", container}).
		WithPurpose("inspect session container storage")
	if ssh != nil {
		command = commandOverSSH(core.SshTrafficClassControl, command, ssh)
	}
	output, err := executor.Execute(&command)
	if err != nil {
		return ContainerStorage{}, err
	}
	if output.Status != 0 {
		return ContainerStorage{}, fmt.Errorf("inspect session container storage failed: %s", lossyText(output.Stderr))
	}
	var mounts []containerInspectedMount
	if err := json.Unmarshal(output.Stdout, &mounts); err != nil {
		return ContainerStorage{}, fmt.Errorf("decode session container mounts: %w", err)
	}
	if err := requireMountDestinations(mounts, true); err != nil {
		return ContainerStorage{}, fmt.Errorf("decode session container mounts: %w", err)
	}
	effective := make([]core.AdditionalMount, 0, len(attachments))
	for _, attachment := range attachments {
		index := slices.IndexFunc(mounts, func(mount containerInspectedMount) bool {
			return libSamePath(*mount.Destination, attachment.Destination)
		})
		if index < 0 {
			return ContainerStorage{}, fmt.Errorf("external attachment %s is missing from container %s", attachment.Destination, container)
		}
		// The engine owns the effective permissions: provisioning can
		// downgrade a requested overlay without changing the saved choice.
		if !*mounts[index].Writable {
			attachment.Access = core.MountAccessRo
		}
		effective = append(effective, attachment)
	}
	return ContainerStorage{
		ManagedTemporaryVolume: hasManagedTemporaryVolume(mounts, container),
		Attachments:            effective,
	}, nil
}

// containerInspectedMount is one mount an engine reports for a container, in its
// PascalCase JSON.
type containerInspectedMount struct {
	Name        string  `json:"Name"`
	Destination *string `json:"Destination"`
	Writable    *bool   `json:"RW"`
}

// requireMountDestinations fails as serde does for a mount without the
// fields it requires: Destination always, RW when the engine's mount
// listing is read.
func requireMountDestinations(mounts []containerInspectedMount, writable bool) error {
	for _, mount := range mounts {
		if mount.Destination == nil {
			return errors.New("missing field `Destination`")
		}
		if writable && mount.Writable == nil {
			return errors.New("missing field `RW`")
		}
	}
	return nil
}

// hasManagedTemporaryVolume reports whether the container's /tmp is the
// managed temporary volume Mjolnir created for it.
func hasManagedTemporaryVolume(mounts []containerInspectedMount, container string) bool {
	return slices.ContainsFunc(mounts, func(mount containerInspectedMount) bool {
		return libSamePath(*mount.Destination, "/tmp") && mount.Name == ContainerTemporaryVolumeName(container)
	})
}

// Podman exec leaves `--user` unset, so each container's stored configured
// user controls initialization as well as later worker commands. Shared by
// all Podman workspace policies and Docker attachment launches. Storage is
// released only after its owning container has been removed.

const temporaryVolumeFunctions = `
prepare_temporary_volume() {
    [ -n "$temporary_volume" ] || return 0
    if ! "$engine" volume inspect "$temporary_volume" >/dev/null 2>&1; then
        "$engine" info >/dev/null
        "$engine" volume create --driver local --label "dev.mj.managed=true" --label "dev.mj.session=$session" "$temporary_volume" >/dev/null
    fi
    identity=$("$engine" volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$temporary_volume")
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign $engine temporary volume $temporary_volume" >&2
        return 1
    }
}
remove_temporary_volume() {
    [ -n "$temporary_volume" ] || return 0
    if identity=$("$engine" volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$temporary_volume" 2>/dev/null); then
        [ "$identity" = "true|$session" ] || {
            echo "refusing to remove foreign $engine temporary volume $temporary_volume" >&2
            return 1
        }
        "$engine" volume rm --force "$temporary_volume" >/dev/null
    else
        "$engine" info >/dev/null
    fi
}
remove_failed_container() {
    if identity=$("$engine" container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
        [ "$identity" = "true|$session" ] || {
            echo "refusing to remove foreign $engine container $container" >&2
            return 1
        }
        "$engine" rm --force "$container" >/dev/null
    else
        "$engine" info >/dev/null
    fi
}
start_container() {
    "$@"
    if [ -n "$temporary_volume" ]; then
        if [ -n "$exec_user" ]; then
            "$engine" exec --user "$exec_user" "$container" sh -c 'set -eu; chown 0:0 /tmp; chmod 1777 /tmp'
        else
            "$engine" exec "$container" sh -c 'set -eu; chown 0:0 /tmp; chmod 1777 /tmp'
        fi
    fi
}
`

func containerLaunchScript(engine string, script string) string {
	execUser := ""
	if engine == "docker" {
		execUser = "0"
	}
	return fmt.Sprintf("set -eu\nengine=%s\nexec_user=%s\n%s\n%s", engine, execUser, temporaryVolumeFunctions, script)
}

// temporaryVolumeArgument is the managed /tmp volume for a container, or
// empty when an attachment already mounts /tmp (compared as paths, by
// component).
func temporaryVolumeArgument(name string, mounts []core.AdditionalMount) string {
	if slices.ContainsFunc(mounts, func(mount core.AdditionalMount) bool { return libSamePath(mount.Destination, "/tmp") }) {
		return ""
	}
	return ContainerTemporaryVolumeName(name)
}

// containerCapabilities is shared by provisioning and replacements on
// retained storage.
func containerCapabilities(engine string) []string {
	switch engine {
	case "podman":
		return []string{"PERFMON"}
	default:
		return nil
	}
}

// containerSecurityOpts is the security options Mjolnir itself puts on a
// session container. The retained container guard and the replacement
// builder both read this list so a storage-preserving upgrade cannot
// silently drop an option the new container needs.
func containerSecurityOpts(engine string) []string {
	switch engine {
	// Host bind mounts carry the host's SELinux label (`cache_home_t` or
	// `user_home_t`), which a `container_t` process is denied. Relabeling the
	// shared mbx cache on every start would walk every object in it, and
	// Docker targets already run without that separation; keep the rootless
	// user namespace and drop only the SELinux layer that cannot see these
	// mounts.
	case "podman":
		return []string{"label=disable"}
	default:
		return nil
	}
}

func containerRun(engine string, template *core.TargetsContainerTemplate, name string, sessionID string, additionalMounts []core.AdditionalMount, workspaceRoot string) (core.CommandSpec, error) {
	args, err := containerRunArgs(engine, template, name, sessionID, additionalMounts, nil, workspaceRoot, true)
	if err != nil {
		return core.CommandSpec{}, err
	}
	return core.NewCommandSpec(engine, args).
		WithPurpose("start session container").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning}).
		WithCreatesTarget(), nil
}

const podmanVolumeRunScript = `set -eu
session=$1
container=$2
volume=$3
temporary_volume=$4
shift 4
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        if remove_failed_container; then
            remove_temporary_volume || echo "Podman temporary storage cleanup failed for session $session" >&2
            if [ -n "$volume" ] && identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null) && [ "$identity" = "true|$session" ]; then
                podman volume rm --force "$volume" >/dev/null || echo "Podman workspace cleanup failed for session $session" >&2
            fi
        else
            echo "Podman container cleanup failed; retained storage for session $session" >&2
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
prepare_temporary_volume
if [ -z "$volume" ]; then
    start_container "$@"
    exit 0
elif identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign Podman volume $volume" >&2
        exit 1
    }
else
    podman info >/dev/null
    podman volume create --label "dev.mj.managed=true" --label "dev.mj.session=$session" "$volume" >/dev/null
    identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume")
    [ "$identity" = "true|$session" ] || {
        echo "Podman volume $volume does not carry the expected Mjolnir identity" >&2
        exit 1
    }
fi
start_container "$@"
`

func podmanHostHelperRunScript(helper []string) string {
	joined := core.JoinRemoteCommand(helper)
	return strings.ReplaceAll(`set -eu
session=$1
container=$2
resource=$3
temporary_volume=$4
shift 4
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        if remove_failed_container; then
            remove_temporary_volume || echo "Podman temporary storage cleanup failed for session $session" >&2
            {helper} destroy "$resource" >/dev/null || echo "Podman workspace cleanup failed for session $session" >&2
        else
            echo "Podman container cleanup failed; retained storage for session $session" >&2
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
prepare_temporary_volume
state=$({helper} status "$resource")
case $state in
    absent) {helper} create "$resource" ;;
    present) ;;
    *) echo "workspace helper returned invalid status $state for $resource" >&2; exit 1 ;;
esac
[ "$({helper} status "$resource")" = present ] || {
    echo "workspace helper did not create $resource" >&2
    exit 1
}
start_container "$@"
`, "{helper}", joined)
}

func podmanContainerRun(
	template *core.TargetsContainerTemplate,
	name string,
	sessionID string,
	additionalMounts []core.AdditionalMount,
	ssh *core.SshTarget,
	workspaceRoot string,
) (core.CommandSpec, error) {
	workspace, err := core.PodmanWorkspaceLocatorNamed(template, name)
	if err != nil {
		return core.CommandSpec{}, err
	}
	runArgs, err := containerRunArgs("podman", template, name, sessionID, additionalMounts, &workspace, workspaceRoot, true)
	if err != nil {
		return core.CommandSpec{}, err
	}
	var wrapped []string
	switch workspace.EffectiveKind() {
	case core.TargetsPodmanWorkspaceLocatorHostPath:
		wrapped = []string{
			"sh",
			"-c",
			containerLaunchScript("podman", podmanHostHelperRunScript(workspace.Helper)),
			"mj-podman-run",
			sessionID,
			name,
			workspace.Resource,
			temporaryVolumeArgument(name, additionalMounts),
			"podman",
		}
	default:
		volume := ""
		if workspace.EffectiveKind() == core.TargetsPodmanWorkspaceLocatorVolume {
			volume = workspace.Name
		}
		wrapped = []string{
			"sh",
			"-c",
			containerLaunchScript("podman", podmanVolumeRunScript),
			"mj-podman-run",
			sessionID,
			name,
			volume,
			temporaryVolumeArgument(name, additionalMounts),
			"podman",
		}
	}
	wrapped = append(wrapped, runArgs...)
	var command core.CommandSpec
	if ssh != nil {
		command = core.SSHCommandOwned(core.SshTrafficClassControl, ssh, wrapped)
	} else {
		command = core.NewCommandSpec(wrapped[0], wrapped[1:])
	}
	layer := workspace.EffectiveKind() == core.TargetsPodmanWorkspaceLocatorContainerLayer
	var purpose string
	switch {
	case layer && ssh != nil:
		purpose = "start remote Podman container"
	case layer:
		purpose = "start session container"
	case ssh != nil:
		purpose = "start remote Podman container with isolated workspace storage"
	default:
		purpose = "start Podman container with isolated workspace storage"
	}
	return command.WithPurpose(purpose).
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning}).
		WithCreatesTarget(), nil
}

const dockerOverlayRunScript = `set -eu
session=$1
container=$2
image=$3
pull=$4
temporary_volume=$5
shift 5
helper="$container-mount-init"
volumes=
backings=
remove_helper() {
    if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.attachment-helper"}}|{{index .Config.Labels "dev.mj.session"}}' "$helper" 2>/dev/null); then
        [ "$identity" = "true|$session" ] || {
            echo "refusing foreign Docker attachment helper $helper" >&2
            return 1
        }
        docker rm --force "$helper" >/dev/null
    else
        docker info >/dev/null
    fi
}
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        released=true
        remove_helper || released=false
        remove_failed_container || released=false
        if [ "$released" = true ]; then
            remove_temporary_volume || released=false
        fi
        if [ "$released" = true ]; then
            for volume in $volumes; do
                docker volume rm --force "$volume" >/dev/null || released=false
            done
        fi
        if [ "$released" = true ]; then
            for backing in $backings; do
                docker volume rm --force "$backing" >/dev/null || released=false
            done
        fi
        if [ "$released" != true ]; then
            echo "Docker attachment cleanup failed; retained backing storage for session $session" >&2
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
prepare_temporary_volume
owned_volume() {
    identity=$(docker volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$1")
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign Docker volume $1" >&2
        return 1
    }
}
remove_helper
while [ "$1" != -- ]; do
    ordinal=$1
    source=$2
    volume=$3
    shift 3
    backing="$volume-backing"
    if docker volume inspect "$volume" >/dev/null 2>&1; then
        owned_volume "$volume"
        owned_volume "$backing"
        volumes="$volumes $volume"
        backings="$backings $backing"
        continue
    fi
    if ! docker volume inspect "$backing" >/dev/null 2>&1; then
        docker volume create --driver local \
            --label "dev.mj.managed=true" --label "dev.mj.session=$session" \
            --label "dev.mj.attachment-backing=true" "$backing" >/dev/null
    fi
    owned_volume "$backing"
    backings="$backings $backing"
    docker run --name "$helper" --pull="$pull" --network none --user 0 \
        --label "dev.mj.attachment-helper=true" --label "dev.mj.session=$session" \
        --volume "$backing:/mj-attachment" \
        --mount "type=bind,source=$source,target=/mj-source,readonly" \
        --entrypoint sh "$image" -c '
            set -eu
            mkdir -p /mj-attachment/upper /mj-attachment/work
            chown "$(stat -c %u:%g /mj-source)" /mj-attachment/upper
            chmod "$(stat -c %a /mj-source)" /mj-attachment/upper
        ' >/dev/null
    remove_helper
    root=$(docker volume inspect --format '{{.Mountpoint}}' "$backing")
    case $root in /*) ;; *) echo "invalid Docker backing volume mountpoint: $root" >&2; exit 1 ;; esac
    upper="$root/upper"
    work="$root/work"
    docker volume create \
        --driver local \
        --label "dev.mj.managed=true" \
        --label "dev.mj.session=$session" \
        --opt type=overlay \
        --opt device=overlay \
        --opt "o=lowerdir=$source,upperdir=$upper,workdir=$work" \
        "$volume" >/dev/null
    owned_volume "$volume"
    volumes="$volumes $volume"
done
shift
start_container "$@"
`

func dockerOverlayVolumeName(containerName string, ordinal int) string {
	return fmt.Sprintf("%s-mount-%d", containerName, ordinal)
}

func dockerPullPolicy(template *core.TargetsContainerTemplate) string {
	switch template.PullPolicy.AtLaunch(template.Image) {
	case core.ImagePullPolicyAlways, core.ImagePullPolicyNewer:
		return "always"
	case core.ImagePullPolicyMissing:
		return "missing"
	case core.ImagePullPolicyNever:
		return "never"
	default:
		panic("at_launch resolves auto")
	}
}

func dockerContainerRun(
	template *core.TargetsContainerTemplate,
	name string,
	sessionID string,
	additionalMounts []core.AdditionalMount,
	workspaceRoot string,
) (core.CommandSpec, error) {
	runArgs, err := containerRunArgs("docker", template, name, sessionID, additionalMounts, nil, workspaceRoot, true)
	if err != nil {
		return core.CommandSpec{}, err
	}
	args := []string{
		"-c",
		containerLaunchScript("docker", dockerOverlayRunScript),
		"hel-docker-run",
		sessionID,
		name,
		template.Image,
		dockerPullPolicy(template),
		temporaryVolumeArgument(name, additionalMounts),
	}
	overlaid := false
	for ordinal, mount := range additionalMounts {
		if mount.Access != core.MountAccessCow {
			continue
		}
		overlaid = true
		args = append(args, strconv.Itoa(ordinal), mount.Source, dockerOverlayVolumeName(name, ordinal))
	}
	args = append(args, "--", "docker")
	args = append(args, runArgs...)
	purpose := "start session container"
	if overlaid {
		purpose = "start Docker session container with isolated attachments"
	}
	return core.NewCommandSpec("sh", args).
		WithPurpose(purpose).
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning}).
		WithCreatesTarget(), nil
}

// podmanPullArgument is Podman's `--pull` flag for this template, or false
// when the resolved policy is Podman's own default. Every `podman run` Hel
// issues for a template uses this one answer.
func podmanPullArgument(template *core.TargetsContainerTemplate) (string, bool) {
	pullPolicy := template.PullPolicy.AtLaunch(template.Image)
	if pullPolicy == core.ImagePullPolicyMissing {
		return "", false
	}
	return "--pull=" + pullPolicy.PodmanValue(), true
}

// containerPidsLimit is the processes and threads a Mjolnir session
// container may hold.
//
// Podman and Docker default to 2048, which was never a choice Mjolnir made
// and is far too small for what it puts in one container. Measured on a
// local Podman target: one session's tree is about 700 threads, because its
// worker, its ACP supervisor and the harness's own Node process each hold a
// thread pool sized to the host's CPU count. Two sessions in a container had
// already taken 1521 of the 2048, and a parent may hold six sub-agent
// children by default, which needs roughly 4,900.
//
// Past the limit every `fork` fails with EAGAIN, which reached users as
// "sh: 1: Cannot fork", as "Resource temporarily unavailable" launching the
// ACP bridge, and as sub-agent starts that simply never reported a harness
// inside their 300-second wait. A limit is not a reservation, so this costs
// nothing until the container actually needs the room.
const containerPidsLimit uint32 = 8192

// containerPidsLimitArgs is the pids-limit flag for a Mjolnir-created
// container, unless the template already sets one: an operator who chose a
// number keeps it.
func containerPidsLimitArgs(engine string, template *core.TargetsContainerTemplate) []string {
	if engine != "podman" && engine != "docker" {
		return nil
	}
	if slices.ContainsFunc(template.ExtraRunArgs, func(argument string) bool {
		return strings.HasPrefix(argument, "--pids-limit")
	}) {
		return nil
	}
	return []string{fmt.Sprintf("--pids-limit=%d", containerPidsLimit)}
}

// keepaliveScript is the command that keeps a Podman or Docker session
// container alive.
//
// With `--init`, PID 1 is the engine's init and its only child is this
// `sleep`. The init forwards SIGTERM and similar signals to that child, and
// the child's exit stops the container and the worker with it. An agent that
// walks a build's parent chain (`ps -o ppid=`) and kills what it finds
// reaches PID 1 this way. So `sleep` starts with every catchable signal whose
// default action ends or stops it set to ignore, and an ignored signal stays
// ignored across `exec`. The numbers are Linux's: HUP INT QUIT ILL TRAP ABRT
// BUS FPE USR1 SEGV USR2 PIPE ALRM TERM STKFLT TSTP TTIN TTOU XCPU XFSZ
// VTALRM PROF IO PWR SYS. The kernel already drops SIGKILL and SIGSTOP that a
// process in the container sends to PID 1. The run arguments set
// `--stop-signal KILL` so that stopping or removing the container does not
// wait out the grace period for a signal that is now ignored.
const keepaliveScript = "trap '' 1 2 3 4 5 6 7 8 10 11 12 13 14 15 16 20 21 22 24 25 26 27 29 30 31; exec sleep infinity"

// containerRunArgs builds the engine's `run` arguments. Every argument is an
// independent input the engine needs: the template, the resource identity,
// the mounts, and the workspace the session runs in.
func containerRunArgs(
	engine string,
	template *core.TargetsContainerTemplate,
	name string,
	sessionID string,
	additionalMounts []core.AdditionalMount,
	podmanWorkspace *core.TargetsPodmanWorkspaceLocator,
	workspaceRoot string,
	initializeWorkspace bool,
) ([]string, error) {
	if err := core.ValidateAdditionalMounts(additionalMounts); err != nil {
		return nil, err
	}
	args := []string{"run"}
	switch engine {
	case "podman":
		if pull, ok := podmanPullArgument(template); ok {
			args = append(args, pull)
		}
		// PID 1 is `sleep infinity`, which reaps nothing, so every exec that
		// outlives its parent leaves a zombie behind. Apple's `container`
		// engine is left alone: its support for `--init` and `--stop-signal`
		// is unverified, and its PID 1 is `sleep` itself, which the kernel
		// shields from signals sent from inside the container.
		args = append(args, "--init", "--stop-signal", "KILL")
	case "docker":
		args = append(args, "--pull="+dockerPullPolicy(template), "--init", "--stop-signal", "KILL")
	}
	args = append(args, containerPidsLimitArgs(engine, template)...)
	args = append(args, "--detach", "--name", name)
	args = append(args, managedResourceIdentityArgs(managedResourceKindContainer, sessionID)...)
	args = append(args, template.ExtraRunArgs...)
	for _, capability := range containerCapabilities(engine) {
		args = append(args, "--cap-add="+capability)
	}
	if engine == "podman" {
		// Rootless Podman maps container uid 0 to the host user. Keeping the
		// image user's uid mapping would force Podman to chown every image
		// layer on storage drivers that cannot shift ids. Keep these after
		// custom args so the session identity and HOME remain deliberate.
		args = append(args, "--user", "0:0", "--env", "HOME=/home/hel")
		for _, option := range containerSecurityOpts(engine) {
			args = append(args, "--security-opt", option)
		}
	}
	if temporaryVolume := temporaryVolumeArgument(name, additionalMounts); temporaryVolume != "" {
		switch engine {
		// No `nocopy`: Podman passes it to the OCI runtime as bind-mount
		// data, which runc rejects. The volume is new, so copy-up is
		// harmless, and `start_container` resets /tmp ownership and mode.
		case "podman":
			args = append(args, "--volume", temporaryVolume+":/tmp:rw")
		case "docker":
			args = append(args, "--mount", "type=volume,source="+temporaryVolume+",target=/tmp,volume-nocopy")
		}
	}
	if engine == "podman" {
		workspace := core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorContainerLayer}
		if podmanWorkspace != nil {
			workspace = *podmanWorkspace
		}
		switch workspace.EffectiveKind() {
		case core.TargetsPodmanWorkspaceLocatorVolume:
			// `:U` chowns the volume to container root, which maps to the
			// host user in rootless Podman's default namespace.
			suffix := ""
			if initializeWorkspace {
				suffix = ",U"
			}
			args = append(args, "--volume", fmt.Sprintf("%s:%s:rw%s", workspace.Name, workspaceRoot, suffix))
		case core.TargetsPodmanWorkspaceLocatorHostPath:
			// The host directory belongs to the host user, which rootless
			// Podman's default namespace maps to container uid 0.
			args = append(args, "--volume", fmt.Sprintf("%s:%s:rw", workspace.Path, workspaceRoot))
		}
	}
	for ordinal, mount := range additionalMounts {
		source, destination := mount.Source, mount.Destination
		switch engine {
		case "podman":
			var mode string
			switch mount.Access {
			case core.MountAccessRo:
				mode = "ro"
			case core.MountAccessCow:
				mode = "O"
			default:
				mode = "rw"
			}
			args = append(args, "--volume", fmt.Sprintf("%s:%s:%s", source, destination, mode))
		case "docker":
			if mount.Access == core.MountAccessCow {
				source = dockerOverlayVolumeName(name, ordinal)
			}
			suffix := ""
			if mount.Access == core.MountAccessRo {
				suffix = ":ro"
			}
			args = append(args, "--volume", fmt.Sprintf("%s:%s%s", source, destination, suffix))
		case "container":
			// Apple's runtime has no copy-on-write overlay, so those mounts
			// stay read-only rather than silently writing through.
			if mount.Access == core.MountAccessRw {
				args = append(args, "--mount", fmt.Sprintf("type=bind,source=%s,target=%s", source, destination))
			} else {
				args = append(args, "--mount", fmt.Sprintf("type=bind,source=%s,target=%s,readonly", source, destination))
			}
		default:
			return nil, fmt.Errorf("additional mounts are unsupported for container engine %s", core.RustDebugString(engine))
		}
	}
	args = append(args, template.Image)
	if engine == "podman" || engine == "docker" {
		args = append(args, "sh", "-c", keepaliveScript)
	} else {
		args = append(args, "sleep", "infinity")
	}
	return args, nil
}

func appleImagePrepareCommands(template *core.TargetsContainerTemplate) []core.CommandSpec {
	var command core.CommandSpec
	switch template.PullPolicy.Resolve(template.Image) {
	case core.ImagePullPolicyAlways, core.ImagePullPolicyNewer:
		command = core.NewCommandSpec("container", []string{"image", "pull", template.Image}).
			WithPurpose("refresh container image " + template.Image)
	case core.ImagePullPolicyNever:
		command = core.NewCommandSpec("container", []string{"image", "inspect", template.Image}).
			WithPurpose("find pinned container image " + template.Image)
	case core.ImagePullPolicyMissing:
		return nil
	default:
		panic("auto pull policy must resolve")
	}
	return []core.CommandSpec{command.WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})}
}

// commandOverSSH moves a command to the remote host without losing its input
// or lifecycle metadata.
func commandOverSSH(class core.SshTrafficClass, command core.CommandSpec, ssh *core.SshTarget) core.CommandSpec {
	remote := append([]string{command.Program}, command.Args...)
	wrapped := core.SSHCommandOwned(class, ssh, remote)
	command.Program = wrapped.Program
	command.Args = wrapped.Args
	// The wrapped command now opens an SSH session, so it is admitted and
	// placed on a shared connection like any other.
	command.SSHDestination = wrapped.SSHDestination
	command.SSHSession = wrapped.SSHSession
	return command
}

func atBoundary(class core.SshTrafficClass, boundary ExecutionBoundary, args []string) core.CommandSpec {
	switch boundary := boundary.(type) {
	case ExecutionBoundaryContainer:
		return core.ContainerExec(boundary.Engine, boundary.ContainerID, args)
	case ExecutionBoundarySsh:
		return core.SSHCommandOwned(class, boundary.SSH, args)
	case ExecutionBoundarySshContainer:
		remote := core.EngineArgv(boundary.Engine)
		remote = append(remote, "exec", "-i", boundary.ContainerID)
		remote = append(remote, args...)
		return core.SSHCommandOwned(class, boundary.SSH, remote)
	default:
		return core.NewCommandSpec(args[0], slices.Clone(args[1:]))
	}
}
