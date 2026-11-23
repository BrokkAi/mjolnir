// Port of mj-targets/src/targets/container.rs.

package targets

import (
	"fmt"
	"slices"
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
	panic("unported: mj-targets/src/targets/container.rs inspect_container_storage")
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
	panic("unported: mj-targets/src/targets/container.rs temporary_volume_argument")
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
	panic("unported: mj-targets/src/targets/container.rs podman_host_helper_run_script")
}

func podmanContainerRun(
	template *core.TargetsContainerTemplate,
	name string,
	sessionID string,
	additionalMounts []core.AdditionalMount,
	ssh *core.SshTarget,
	workspaceRoot string,
) (core.CommandSpec, error) {
	panic("unported: mj-targets/src/targets/container.rs podman_container_run")
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
	panic("unported: mj-targets/src/targets/container.rs docker_container_run")
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
	panic("unported: mj-targets/src/targets/container.rs container_run_args")
}

func appleImagePrepareCommands(template *core.TargetsContainerTemplate) []core.CommandSpec {
	panic("unported: mj-targets/src/targets/container.rs apple_image_prepare_commands")
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
