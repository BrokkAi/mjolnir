// Port of mj-targets/src/targets/cleanup.rs.

package targets

import (
	"errors"
	"fmt"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func ClosePlan(locator *core.TargetsTargetLocator, sessionID string) (WorkerControlPlan, error) {
	return closePlanScoped(locator, sessionID, true, nil)
}

// RetireMoveTargetPlan: a Move can have two generations on one host. Shared
// session caches remain owned by the destination.
func RetireMoveTargetPlan(locator *core.TargetsTargetLocator, sessionID string) (WorkerControlPlan, error) {
	return closePlanScoped(locator, sessionID, false, nil)
}

// TerminateEc2InstanceCommand: termination needs the durable AWS identity
// even before SSH has an address.
func TerminateEc2InstanceCommand(profile string, region string, instanceID string) (core.CommandSpec, error) {
	if !core.ValidEc2InstanceID(instanceID) {
		return core.CommandSpec{}, errors.New("refusing cleanup: invalid EC2 instance ID")
	}
	// EC2 TerminateInstances is idempotent for an already-terminated
	// instance.
	return core.NewCommandSpec("aws", []string{
		"--profile", profile,
		"--region", region,
		"ec2", "terminate-instances",
		"--instance-ids", instanceID,
	}).WithPurpose("terminate exact EC2 session instance"), nil
}

// closePlanScoped builds a session's close plan; stopped is the proof that
// the worker's managed Stop completed, or nil when the plan stops it.
func closePlanScoped(locator *core.TargetsTargetLocator, sessionID string, removeSharedCache bool, stopped StoppedWorkerProof) (WorkerControlPlan, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return WorkerControlPlan{}, err
	}
	if core.IsBorrowed(locator) {
		return WorkerControlPlan{}, errors.New("refusing to operate on a target borrowed from another session; act on the owning session instead")
	}
	description := "close Mjolnir session " + sessionID
	// A remote container closes exactly as the local one does, with every
	// command run on the host that holds it.
	if locator.Kind == core.TargetsTargetLocatorSshDocker || locator.Kind == core.TargetsTargetLocatorSshAppleContainer {
		local := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: locator.ContainerID}
		if locator.Kind == core.TargetsTargetLocatorSshAppleContainer {
			local.Kind = core.TargetsTargetLocatorAppleContainer
		}
		localPlan, err := closePlanScoped(&local, sessionID, removeSharedCache, nil)
		if err != nil {
			return WorkerControlPlan{}, err
		}
		commands, err := localPlan.CommandsWithoutStaging()
		if err != nil {
			return WorkerControlPlan{}, err
		}
		remote := make([]ControlCommand, 0, len(commands))
		for _, command := range commands {
			remote = append(remote, ControlCommandFromCommandSpec(commandOverSSH(core.SshTrafficClassControl, command, locator.SSH)))
		}
		return NewWorkerControlPlan(description, remote, locator, sessionID)
	}

	// The engine stops a sandbox's process before it deletes a disk, so
	// removal needs no more than the sandbox and its two named volumes. A
	// sandbox has no host Git cache snapshot to remove.
	if locator.Kind == core.TargetsTargetLocatorSshNitro {
		var commands []ControlCommand
		for _, command := range nitroDestroyCommands(locator.SSH, locator.ContainerID) {
			commands = append(commands, ControlCommandFromCommandSpec(command))
		}
		return NewWorkerControlPlan("clean up stopped Mjolnir session "+sessionID, commands, locator, sessionID)
	}
	sessionWorkerRoot, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return WorkerControlPlan{}, err
	}
	sessionProfileHome := ".local/share/hel/profiles/" + sessionID
	if locator.Kind == core.TargetsTargetLocatorLocalPodman || locator.Kind == core.TargetsTargetLocatorSshPodman {
		plan, err := podmanCleanupPlan(locator, sessionID)
		if err != nil {
			return WorkerControlPlan{}, err
		}
		var commands []ControlCommand
		for _, command := range plan.Commands {
			if !removeSharedCache && command.Stage != nil && command.Stage.Kind == core.ProvisionStageCleaningCache {
				continue
			}
			commands = append(commands, ControlCommandFromCommandSpec(command))
		}
		return NewWorkerControlPlan(plan.Description, commands, locator, sessionID)
	}
	// stopScript is the kill that comes first, or nothing once the managed
	// Stop has already ended the worker.
	stopScript := func() ControlScript {
		if stopped != nil {
			return ControlScriptWithoutControl("")
		}
		return KillWorkerDaemonScript(sessionWorkerRoot)
	}
	removeSharedCacheArgument := "false"
	if removeSharedCache {
		removeSharedCacheArgument = "true"
	}
	var command ControlCommand
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		// The daemon dies before its root does: a survivor's next durable
		// write would recreate the directory this command removes. The
		// session's staged profile home, and with it the project-memory
		// replica and native history inside, goes with the worker root; a
		// harness with a nested home keeps its root under the data directory
		// instead.
		script := stopScript().MapScript(func(kill string) string {
			return fmt.Sprintf("%s\nrm -rf -- %s %s\n", kill,
				core.PosixQuote(sessionWorkerRoot),
				core.PosixQuote(LocalNestedProfileRoot(sessionID)))
		})
		purpose := "stop the local Mjolnir worker and remove exact local Mjolnir worker state"
		if stopped != nil {
			purpose = "remove exact stopped local Mjolnir worker state"
		}
		command = script.IntoCommand(func(script string) core.CommandSpec {
			return core.NewCommandSpec("sh", []string{"-c", script}).WithPurpose(purpose)
		})
	case core.TargetsTargetLocatorLocalDocker:
		script := temporaryVolumeFunctions + "\n" + localDockerCloseScript
		command = ControlCommandFromCommandSpec(core.NewCommandSpec("sh", []string{
			"-c",
			script,
			"mj-close",
			locator.ContainerID,
			sessionID,
			removeSharedCacheArgument,
			ContainerTemporaryVolumeName(locator.ContainerID),
		}).WithPurpose("remove local Docker session container, overlay volumes, and cache state"))
	case core.TargetsTargetLocatorAppleContainer:
		script := "status=0; container rm --force \"$1\" || status=$?; if [ \"$status\" -eq 0 ] && [ \"$3\" = true ]; then rm -rf -- \"$HOME/.cache/mjolnir/git/sessions/$2\" || status=$?; fi; exit \"$status\""
		command = ControlCommandFromCommandSpec(core.NewCommandSpec("sh", []string{
			"-c",
			script,
			"mj-close",
			locator.ContainerID,
			sessionID,
			removeSharedCacheArgument,
		}).WithPurpose("remove Apple session container and Git cache snapshot"))
	case core.TargetsTargetLocatorAwsEc2:
		terminate, err := TerminateEc2InstanceCommand(locator.Profile, locator.Region, locator.InstanceID)
		if err != nil {
			return WorkerControlPlan{}, err
		}
		command = ControlCommandFromCommandSpec(terminate)
	case core.TargetsTargetLocatorSshBare:
		// Same ordering constraint as the local bare target: stop the daemon
		// before deleting the root it keeps writing to.
		script := stopScript().MapScript(func(kill string) string {
			return fmt.Sprintf("%s\nrm -rf -- %s %s %s\n", kill,
				core.PosixQuote(locator.Workspace),
				core.PosixQuote(sessionWorkerRoot),
				core.PosixQuote(sessionProfileHome))
		})
		purpose := "stop the remote Mjolnir worker and remove exact SSH session workspace and runtime state"
		if stopped != nil {
			purpose = "remove exact stopped SSH session workspace and runtime state"
		}
		command = script.IntoCommand(func(script string) core.CommandSpec {
			return core.SSHCommand(core.SshTrafficClassControl, locator.SSH, []string{"sh", "-c", script}).WithPurpose(purpose)
		})
	default:
		panic("unreachable: every other target kind is handled above")
	}
	return NewWorkerControlPlan(description, []ControlCommand{command}, locator, sessionID)
}

const localDockerCloseScript = `status=0
engine=docker
session=$2
container=$1
temporary_volume=$4
helper="$1-mount-init"
if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.attachment-helper"}}|{{index .Config.Labels "dev.mj.session"}}' "$helper" 2>/dev/null); then
    if [ "$identity" = "true|$2" ]; then
        docker rm --force "$helper" || status=$?
    else
        echo 'refusing to remove a foreign Docker attachment helper' >&2
        status=2
    fi
elif ! docker info >/dev/null 2>&1; then
    echo 'could not determine whether the Docker attachment helper exists' >&2
    status=1
fi
if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$1" 2>/dev/null); then
    if [ "$identity" = "true|$2" ]; then
        docker rm --force "$1" || status=$?
    else
        echo 'refusing to remove a Docker container Mjolnir does not own for this session' >&2
        status=2
    fi
elif ! docker info >/dev/null 2>&1; then
    echo 'could not determine whether the Docker session container exists' >&2
    status=1
fi
if [ "$status" -eq 0 ]; then
    remove_temporary_volume || status=$?
fi
if [ "$status" -eq 0 ]; then
    volumes=$(docker volume ls --quiet --filter "label=dev.mj.managed=true" --filter "label=dev.mj.session=$2" --filter "name=^$1-mount-") || status=$?
    if [ "$status" -eq 0 ]; then
        backings=
        for volume in $volumes; do
            backing=$(docker volume inspect --format '{{index .Labels "dev.mj.attachment-backing"}}' "$volume") || { status=$?; continue; }
            if [ "$backing" = true ]; then
                backings="$backings $volume"
            else
                docker volume rm --force "$volume" || status=$?
            fi
        done
        if [ "$status" -eq 0 ]; then
            for backing in $backings; do docker volume rm --force "$backing" || status=$?; done
        fi
    fi
fi
if [ "$status" -eq 0 ] && [ "$3" = true ]; then
    rm -rf -- "$HOME/.cache/mjolnir/git/sessions/$2" || status=$?
fi
if [ "$status" -eq 0 ]; then
    root="$HOME/.cache/mjolnir/docker-overlays/$1"
    if [ "$(cat "$root/.hel-session" 2>/dev/null || true)" = "$2" ]; then
        case $1 in mj-*|hel-*) rm -rf -- "$root" || status=$? ;; *) status=2 ;; esac
    fi
fi
exit "$status"`

// LocalNestedProfileRoot is where a session of a harness with an XDG-nested
// home (Muse, OpenCode) on this machine keeps its per-session profile root,
// which lies under the data directory rather than the worker root, so
// closing the session names it separately.
func LocalNestedProfileRoot(sessionID string) string {
	return filepath.Join(core.DataDir(), "profiles", sessionID)
}

// BorrowedWorkerCleanupPlan stops and removes only a child session's private
// worker state from a target owned by its parent. This never removes the
// target or project workspace.
func BorrowedWorkerCleanupPlan(locator *core.TargetsTargetLocator, childSessionID string) (WorkerControlPlan, error) {
	return borrowedWorkerCleanupPlanScoped(locator, childSessionID, nil)
}

func borrowedWorkerCleanupPlanScoped(locator *core.TargetsTargetLocator, childSessionID string, stopped StoppedWorkerProof) (WorkerControlPlan, error) {
	if err := core.VerifyLocator(locator, childSessionID); err != nil {
		return WorkerControlPlan{}, err
	}
	workerRoot, err := core.WorkerRoot(locator, childSessionID)
	if err != nil {
		return WorkerControlPlan{}, err
	}
	script := KillWorkerDaemonScript(workerRoot)
	if stopped != nil {
		script = ControlScriptWithoutControl("")
	}
	script = script.MapScript(func(script string) string {
		script += fmt.Sprintf("rm -rf -- %s\n", core.PosixQuote(workerRoot))
		if locator.Kind == core.TargetsTargetLocatorLocalBare {
			script += fmt.Sprintf("rm -rf -- %s\n", core.PosixQuote(LocalNestedProfileRoot(childSessionID)))
		} else {
			script += fmt.Sprintf("rm -rf -- %s %s\n",
				core.PosixQuote("/var/lib/hel/profiles/"+childSessionID),
				core.PosixQuote(".local/share/hel/profiles/"+childSessionID))
		}
		return script
	})
	purpose := "stop a borrowed-target sub-agent and remove its private worker state"
	if stopped != nil {
		purpose = "remove a stopped borrowed-target sub-agent's private worker state"
	}
	command := script.IntoCommand(func(script string) core.CommandSpec {
		return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).WithPurpose(purpose)
	})
	return NewWorkerControlPlan("clean up sub-agent worker "+childSessionID, []ControlCommand{command}, locator, childSessionID)
}

const podmanContainerIdentityScript = `set -eu
container=$1
session=$2
if identity=$(podman container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo 'refusing to operate on a Podman container Mjolnir does not own for this session' >&2
        exit 2
    }
elif ! podman info >/dev/null 2>&1; then
    echo 'could not determine whether the Podman session container exists' >&2
    exit 1
else
    exit 0
fi
`

// QuiescePlan stops a Podman target without deleting its potentially large
// writable layer; nil for a target that has none. A successful return means
// the exact owned container is absent or not running.
func QuiescePlan(locator *core.TargetsTargetLocator, sessionID string) (*core.CommandPlan, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return nil, err
	}
	if core.IsBorrowed(locator) {
		return nil, errors.New("refusing to operate on a target borrowed from another session; act on the owning session instead")
	}
	description := "quiesce Mjolnir session " + sessionID
	if locator.Kind == core.TargetsTargetLocatorSshNitro {
		return &core.CommandPlan{
			Description: description,
			Commands:    []core.CommandSpec{nitroQuiesceCommand(locator.SSH, locator.ContainerID)},
		}, nil
	}
	var ssh *core.SshTarget
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
	case core.TargetsTargetLocatorSshPodman:
		ssh = locator.SSH
	default:
		return nil, nil
	}
	script := podmanContainerIdentityScript + `if podman container inspect "$container" >/dev/null 2>&1; then
    stop_err=$(podman stop --time 0 --ignore "$container" 2>&1 >/dev/null) || true
    [ -z "$stop_err" ] || printf '%s\n' "$stop_err" >&2
    # ` + "`container exists`" + ` exits 1 only for a definite absence; 125 is an error.
    status=0
    podman container exists "$container" || status=$?
    case $status in
        0)
            running=$(podman container inspect --format '{{.State.Running}}' "$container")
            [ "$running" = false ] || { echo 'Podman session container is still running' >&2; exit 1; }
            ;;
        1) ;;
        *)
            echo 'could not determine whether the Podman session container stopped' >&2
            exit 1
            ;;
    esac
fi
`
	args := []string{"sh", "-c", script, "mj-quiesce", locator.ContainerID, sessionID}
	var command core.CommandSpec
	if ssh != nil {
		command = core.SSHCommand(core.SshTrafficClassControl, ssh, args)
	} else {
		command = core.NewCommandSpec(args[0], args[1:])
	}
	command = command.WithPurpose("stop exact Podman session container without removing storage").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageStoppingTarget}).
		WithWarnOnStderr()
	return &core.CommandPlan{Description: description, Commands: []core.CommandSpec{command}}, nil
}

// StopContainerPlan stops every writer in an owned container through its
// engine; nil for a target that is not a container. Unlike `exec`, this also
// proves termination when a container exists but is already stopped. Podman
// retains its existing storage-preserving quiescence implementation.
func StopContainerPlan(locator *core.TargetsTargetLocator, sessionID string) (*core.CommandPlan, error) {
	plan, err := QuiescePlan(locator, sessionID)
	if err != nil || plan != nil {
		return plan, err
	}
	var ssh *core.SshTarget
	apple := false
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalDocker:
	case core.TargetsTargetLocatorAppleContainer:
		apple = true
	case core.TargetsTargetLocatorSshDocker:
		ssh = locator.SSH
	case core.TargetsTargetLocatorSshAppleContainer:
		ssh, apple = locator.SSH, true
	default:
		return nil, nil
	}
	script := dockerStopContainerScript
	if apple {
		// Apple's default list contains only running containers; a stopped
		// container remains in `list --all`. Stop only the exact recorded ID.
		// CLI contract: github.com/apple/container/blob/main/docs/command-reference.md
		script = appleStopContainerScript
	}
	command := core.NewCommandSpec("sh", []string{"-c", script, "mj-stop-container", locator.ContainerID, sessionID}).
		WithPurpose("confirm owned container writers stopped through the engine").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageStoppingTarget})
	if ssh != nil {
		command = commandOverSSH(core.SshTrafficClassControl, command, ssh)
	}
	return &core.CommandPlan{
		Description: "stop environment container " + sessionID,
		Commands:    []core.CommandSpec{command},
	}, nil
}

const appleStopContainerScript = `set -eu
container=$1
running=$(container list --quiet)
for id in $running; do
    if [ "$id" = "$container" ]; then container stop --time 0 "$container" >/dev/null; fi
done
running=$(container list --quiet)
for id in $running; do
    [ "$id" != "$container" ] || { echo 'Apple session container is still running' >&2; exit 1; }
done
`

const dockerStopContainerScript = `set -eu
container=$1
session=$2
if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || { echo 'refusing to stop a foreign Docker container' >&2; exit 2; }
    running=$(docker container inspect --format '{{.State.Running}}' "$container")
    case $running in
        true) docker stop --time 0 "$container" >/dev/null ;;
        false) ;;
        *) echo 'unknown Docker container execution state' >&2; exit 1 ;;
    esac
    running=$(docker container inspect --format '{{.State.Running}}' "$container")
    [ "$running" = false ] || { echo 'Docker session container is still running' >&2; exit 1; }
else
    docker info >/dev/null 2>&1 || { echo 'could not determine whether the Docker session container exists' >&2; exit 1; }
fi
`

func podmanCleanupPlan(locator *core.TargetsTargetLocator, sessionID string) (core.CommandPlan, error) {
	var ssh *core.SshTarget
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
	case core.TargetsTargetLocatorSshPodman:
		ssh = locator.SSH
	default:
		panic("Podman cleanup requires a Podman locator")
	}
	containerID, workspaceStorage := locator.ContainerID, locator.WorkspaceStorage
	removeContainerScript := podmanContainerIdentityScript + `rm_err=$(podman rm --force --ignore "$container" 2>&1 >/dev/null) || true
[ -z "$rm_err" ] || printf '%s\n' "$rm_err" >&2
# ` + "`container exists`" + ` exits 1 only for a definite absence; 125 is an error.
status=0
podman container exists "$container" || status=$?
case $status in
    1) ;;
    0) echo 'Podman session container is still present' >&2; exit 1 ;;
    *) echo 'could not determine whether the Podman session container was removed' >&2; exit 1 ;;
esac
`
	atHost := func(args []string) core.CommandSpec {
		if ssh != nil {
			return core.SSHCommandOwned(core.SshTrafficClassControl, ssh, args)
		}
		return core.NewCommandSpec(args[0], args[1:])
	}
	commands := []core.CommandSpec{
		atHost([]string{"sh", "-c", removeContainerScript, "mj-remove-container", containerID, sessionID}).
			WithPurpose("remove exact stopped Podman session container").
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageRemovingContainer}).
			WithWarnOnStderr(),
	}
	removingStorage := core.ProvisionStage{Kind: core.ProvisionStageRemovingStorage}
	switch workspaceStorage.EffectiveKind() {
	case core.TargetsPodmanWorkspaceLocatorVolume:
		script := `set -eu
volume=$1
session=$2
if identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo 'refusing to remove a Podman volume Mjolnir does not own for this session' >&2
        exit 2
    }
    podman volume rm --force "$volume" >/dev/null
elif ! podman info >/dev/null 2>&1; then
    echo 'could not determine whether the Podman workspace volume exists' >&2
    exit 1
fi
`
		commands = append(commands,
			atHost([]string{"sh", "-c", script, "mj-remove-volume", workspaceStorage.Name, sessionID}).
				WithPurpose("remove exact Podman session workspace volume").
				WithStage(removingStorage))
	case core.TargetsPodmanWorkspaceLocatorHostPath:
		helper := core.JoinRemoteCommand(workspaceStorage.Helper)
		script := strings.ReplaceAll(`set -eu
resource=$1
state=$({helper} status "$resource")
case $state in
    present) {helper} destroy "$resource" ;;
    absent) ;;
    *) echo "workspace helper returned invalid status $state for $resource" >&2; exit 1 ;;
esac
[ "$({helper} status "$resource")" = absent ] || {
    echo "workspace helper did not destroy $resource" >&2
    exit 1
}
`, "{helper}", helper)
		commands = append(commands,
			atHost([]string{"sh", "-c", script, "mj-remove-host-workspace", workspaceStorage.Resource}).
				WithPurpose("remove exact helper-managed Podman session workspace").
				WithStage(removingStorage))
	}
	temporaryVolume := ContainerTemporaryVolumeName(containerID)
	temporaryCleanup := fmt.Sprintf("set -eu\nengine=podman\ntemporary_volume=$1\nsession=$2\n%s\nremove_temporary_volume\n", temporaryVolumeFunctions)
	commands = append(commands,
		atHost([]string{"sh", "-c", temporaryCleanup, "mj-remove-temporary-volume", temporaryVolume, sessionID}).
			WithPurpose("remove exact Podman session temporary volume").
			WithStage(removingStorage),
		atHost([]string{"rm", "-rf", "--", ".cache/mjolnir/git/sessions/" + sessionID}).
			WithPurpose("remove Podman session Git cache snapshot").
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageCleaningCache}),
	)
	return core.CommandPlan{
		Description: "clean up stopped Mjolnir session " + sessionID,
		Commands:    commands,
	}, nil
}

// CleanupTargetIsConfirmedAbsent confirms that a container is absent after
// its exact delete command failed. Other target deletion commands are
// already idempotent: filesystem removal uses `rm -rf`, Podman uses
// `--ignore`, and EC2 termination is an idempotent API operation. Apple
// lists exact container IDs; Docker checks both the exact container name and
// exact session-labeled volumes while distinguishing an unavailable daemon
// from absence.
func CleanupTargetIsConfirmedAbsent(locator *core.TargetsTargetLocator, sessionID string, executor core.CommandExecutor) (bool, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return false, err
	}
	if core.IsBorrowed(locator) {
		return false, errors.New("refusing to operate on a target borrowed from another session; act on the owning session instead")
	}
	var command core.CommandSpec
	statusIsAnswer := true
	switch locator.Kind {
	case core.TargetsTargetLocatorAppleContainer, core.TargetsTargetLocatorSshAppleContainer:
		command = core.NewCommandSpec("container", []string{"list", "--all", "--quiet"}).
			WithPurpose("confirm exact Apple session container is absent")
		statusIsAnswer = false
	case core.TargetsTargetLocatorLocalDocker, core.TargetsTargetLocatorSshDocker:
		command = core.NewCommandSpec("sh", []string{
			"-c",
			"if docker container inspect \"$1\" >/dev/null 2>&1; then exit 1; fi; docker info >/dev/null 2>&1 || exit 2; test -z \"$(docker volume ls --quiet --filter label=dev.mj.managed=true --filter label=dev.mj.session=$2)\"",
			"hel-confirm-absent",
			locator.ContainerID,
			sessionID,
		}).WithPurpose("confirm exact Docker session resources are absent")
	case core.TargetsTargetLocatorLocalPodman:
		command = podmanAbsenceCommand(nil, locator.ContainerID, &locator.WorkspaceStorage, sessionID)
	case core.TargetsTargetLocatorSshPodman:
		command = podmanAbsenceCommand(locator.SSH, locator.ContainerID, &locator.WorkspaceStorage, sessionID)
	case core.TargetsTargetLocatorSshNitro:
		command = nitroAbsenceCommand(locator.SSH, locator.ContainerID)
	default:
		return false, nil
	}
	if locator.Kind == core.TargetsTargetLocatorSshDocker || locator.Kind == core.TargetsTargetLocatorSshAppleContainer {
		command = commandOverSSH(core.SshTrafficClassControl, command, locator.SSH)
	}
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	if statusIsAnswer {
		switch output.Status {
		case 0:
			return true, nil
		case 1:
			return false, nil
		}
		return false, fmt.Errorf("%s failed with status %d: %s", command.Purpose, output.Status, lossyText(output.Stderr))
	}
	if output.Status != 0 {
		return false, fmt.Errorf("%s failed with status %d: %s", command.Purpose, output.Status, lossyText(output.Stderr))
	}
	listed, err := libFromUTF8(output.Stdout)
	if err != nil {
		return false, fmt.Errorf("decode Apple container list: %w", err)
	}
	for _, id := range libLines(listed) {
		if strings.TrimSpace(id) == locator.ContainerID {
			return false, nil
		}
	}
	return true, nil
}

func podmanAbsenceCommand(ssh *core.SshTarget, containerID string, workspaceStorage *core.TargetsPodmanWorkspaceLocator, sessionID string) core.CommandSpec {
	var storageCheck, storage string
	switch workspaceStorage.EffectiveKind() {
	case core.TargetsPodmanWorkspaceLocatorVolume:
		storageCheck = `podman volume exists "$3"
case $? in
    0) exit 1 ;;
    1) exit 0 ;;
    *) exit 2 ;;
esac`
		storage = workspaceStorage.Name
	case core.TargetsPodmanWorkspaceLocatorHostPath:
		helper := core.JoinRemoteCommand(workspaceStorage.Helper)
		storageCheck = strings.ReplaceAll(`state=$({helper} status "$3") || exit 2
case $state in
    absent) exit 0 ;;
    present) exit 1 ;;
    *) exit 2 ;;
esac`, "{helper}", helper)
		storage = workspaceStorage.Resource
	default:
		storageCheck = "exit 0"
		storage = "-"
	}
	script := `podman container exists "$1"
case $? in
    0) exit 1 ;;
    1) ;;
    *) exit 2 ;;
esac
podman volume exists "$4"
case $? in
    0) exit 1 ;;
    1) ;;
    *) exit 2 ;;
esac
` + storageCheck
	args := []string{
		"sh",
		"-c",
		script,
		"mj-confirm-podman-absent",
		containerID,
		sessionID,
		storage,
		ContainerTemporaryVolumeName(containerID),
	}
	var command core.CommandSpec
	if ssh != nil {
		command = core.SSHCommandOwned(core.SshTrafficClassControl, ssh, args)
	} else {
		command = core.NewCommandSpec(args[0], args[1:])
	}
	return command.WithPurpose("confirm exact Podman session resources are absent")
}

// StoppedCleanupPlan is a removal plan that follows completed managed Stop
// and retains the launch permits. This authority does not assert
// containment of arbitrary detached writers.
type StoppedCleanupPlan[P StoppedWorkerProof] struct {
	proof P
	plan  WorkerControlPlan
}

// StoppedWorkerProof is proof that the managed Stop of one exact worker
// completed and that its launch permits are still held. The lifecycle code
// that performs the Stop issues it; nothing in this package can construct
// one, so a removal plan that skips the worker's own stop cannot be built
// from a guess.
type StoppedWorkerProof interface {
	SessionID() string
	Target() *core.TargetsTargetLocator
	Root() string
}

func (p *StoppedCleanupPlan[P]) Execute(executor core.CommandExecutor, stager ControlWorkerStager) ([]core.CommandOutput, error) {
	return p.plan.Execute(executor, stager)
}

// ensureStoppedRoot refuses a proof whose worker root is no longer the one
// its target resolves to.
func ensureStoppedRoot(proof StoppedWorkerProof) error {
	root, err := core.WorkerRoot(proof.Target(), proof.SessionID())
	if err != nil {
		return err
	}
	if root != proof.Root() {
		return errors.New("stopped worker root changed")
	}
	return nil
}

func CloseStoppedTargetPlan[P StoppedWorkerProof](proof P) (StoppedCleanupPlan[P], error) {
	if err := ensureStoppedRoot(proof); err != nil {
		return StoppedCleanupPlan[P]{}, err
	}
	plan, err := closePlanScoped(proof.Target(), proof.SessionID(), true, proof)
	if err != nil {
		return StoppedCleanupPlan[P]{}, err
	}
	return StoppedCleanupPlan[P]{proof: proof, plan: plan}, nil
}

func RetireStoppedMoveTargetPlan[P StoppedWorkerProof](proof P) (StoppedCleanupPlan[P], error) {
	if err := ensureStoppedRoot(proof); err != nil {
		return StoppedCleanupPlan[P]{}, err
	}
	plan, err := closePlanScoped(proof.Target(), proof.SessionID(), false, proof)
	if err != nil {
		return StoppedCleanupPlan[P]{}, err
	}
	return StoppedCleanupPlan[P]{proof: proof, plan: plan}, nil
}

func StoppedBorrowedWorkerCleanupPlan[P StoppedWorkerProof](proof P) (StoppedCleanupPlan[P], error) {
	if err := ensureStoppedRoot(proof); err != nil {
		return StoppedCleanupPlan[P]{}, err
	}
	plan, err := borrowedWorkerCleanupPlanScoped(proof.Target(), proof.SessionID(), proof)
	if err != nil {
		return StoppedCleanupPlan[P]{}, err
	}
	return StoppedCleanupPlan[P]{proof: proof, plan: plan}, nil
}
