// Port of mj-targets/src/targets/nitro.rs.

// Nitro Sandbox sessions: one Firecracker microVM per session on an EC2 host
// reached over SSH, driven through `mj-worker microvm` as root.
//
// The engine implements the subset of Podman's verbs, flags and output
// formats that Mjolnir uses over SSH, so recovery, worker start, copies and
// image refresh reuse the Podman call sites with a different command prefix
// (core.EngineArgv). Two things differ on purpose:
//
//   - The engine owns compound operations. `run` creates the named volumes
//     with the session's labels and removes everything it created when the
//     launch fails, so there is no shell wrapper like
//     `PODMAN_VOLUME_RUN_SCRIPT`.
//   - A sandbox is a VM. The controller drops requests for host directories
//     with a notice, and this module refuses any that arrive. A remote mbx
//     cache uses its own engine flags so the host can enforce namespace
//     access.

package targets

import (
	"encoding/json"
	"errors"
	"fmt"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// kernelMinimum is the oldest host kernel the sandbox boot path was proven on
// (Amazon Linux 2023 `6.18.51-120.162`).
var kernelMinimum = [3]uint32{6, 18, 51}

// NitroEngineInfo is what `microvm info --format json` reports, as the
// controller reads it.
//
// These key names are the controller's expectation of the engine's output;
// the engine contract names the facts (versions, pool, slots, CPUs) and this
// pins their spelling.
type NitroEngineInfo struct {
	EngineVersion      string  `json:"engine_version"`
	StateFormat        uint32  `json:"state_format"`
	FirecrackerVersion string  `json:"firecracker_version"`
	CranePath          *string `json:"crane_path"`
	CraneVersion       *string `json:"crane_version"`
	Architecture       *string `json:"architecture"`
	KernelVersion      string  `json:"kernel_version"`
	// MemoryPoolTotalMib is the memory the host reserved for sandboxes at
	// boot, in MiB.
	MemoryPoolTotalMib uint64 `json:"memory_pool_total_mib"`
	MemoryPoolFreeMib  uint64 `json:"memory_pool_free_mib"`
	SlotsInUse         uint32 `json:"slots_in_use"`
	SlotsTotal         uint32 `json:"slots_total"`
	CPUs               uint32 `json:"cpus"`
}

// UnmarshalJSON decodes as serde's derived Deserialize does: every field
// except the Options is required and may not be null.
func (i *NitroEngineInfo) UnmarshalJSON(data []byte) error {
	required := []string{
		"engine_version", "state_format", "firecracker_version", "kernel_version",
		"memory_pool_total_mib", "memory_pool_free_mib", "slots_in_use", "slots_total", "cpus",
	}
	if _, err := containerUpgradeCheckFields(data, required, required); err != nil {
		return err
	}
	type plain NitroEngineInfo
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*i = NitroEngineInfo(decoded)
	return nil
}

// NitroPreflight is a Nitro host that can run sandboxes, for `mj doctor` and
// the Targets pane.
type NitroPreflight struct {
	Info NitroEngineInfo
}

// Summary is one line naming what doctor reports as ready.
func (p NitroPreflight) Summary() string {
	return fmt.Sprintf("Firecracker %s, %d MiB sandbox memory pool (%d MiB free), kernel %s",
		p.Info.FirecrackerVersion, p.Info.MemoryPoolTotalMib, p.Info.MemoryPoolFreeMib, p.Info.KernelVersion)
}

// nitroPoolMemory is the sandbox memory pool as (used, total) bytes, from the
// engine's report.
func nitroPoolMemory(info string) (used uint64, total uint64, err error) {
	const mib = 1024 * 1024
	var report NitroEngineInfo
	if err := json.Unmarshal([]byte(info), &report); err != nil {
		return 0, 0, fmt.Errorf("decode the Nitro sandbox engine report: %w", err)
	}
	if report.MemoryPoolFreeMib > report.MemoryPoolTotalMib {
		return 0, 0, errors.New("the Nitro sandbox engine reports more free memory than its pool holds")
	}
	return (report.MemoryPoolTotalMib - report.MemoryPoolFreeMib) * mib, report.MemoryPoolTotalMib * mib, nil
}

// NitroInfoCommand reads the Nitro sandbox engine's host report.
func NitroInfoCommand(ssh *core.SshTarget) core.CommandSpec {
	return core.SSHEngineCommand(core.SshTrafficClassControl, ssh, core.MicrovmEngine, []string{"info", "--format", "json"}).
		WithPurpose("read the Nitro sandbox engine's host report").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
}

// ParseNitroInfo parses the engine's report, and says what is wrong with a
// host that cannot run a sandbox.
func ParseNitroInfo(destination string, stdout []byte) (NitroPreflight, error) {
	panic("unported: mj-targets/src/targets/nitro.rs parse_nitro_info")
}

func kernelRelease(release string) ([3]uint32, error) {
	parts := strings.FieldsFunc(release, func(c rune) bool { return c < '0' || c > '9' })
	var numbers [3]uint32
	for i := range numbers {
		if i >= len(parts) {
			return [3]uint32{}, errors.New("kernel release has fewer than three numbers")
		}
		number, err := strconv.ParseUint(parts[i], 10, 32)
		if err != nil {
			return [3]uint32{}, errors.New("kernel release number is out of range")
		}
		numbers[i] = uint32(number)
	}
	return numbers, nil
}

// NitroEngineUnavailable: the engine did not answer its report. It is not
// installed on the host yet, or it cannot run there. Distinct from a report
// that says the host cannot run sandboxes, because the fix differs.
type NitroEngineUnavailable string

func (e NitroEngineUnavailable) Error() string {
	return string(e)
}

// VerifySSHNitro checks that the host can run sandboxes. The caller installs
// the engine first, because the engine is the worker binary and nothing here
// can run without it.
func VerifySSHNitro(ssh *core.SshTarget, executor core.CommandExecutor) (NitroPreflight, error) {
	panic("unported: mj-targets/src/targets/nitro.rs verify_ssh_nitro")
}

// NitroWorkspaceVolume names the workspace volume the engine creates for a
// sandbox.
func NitroWorkspaceVolume(container string) string {
	return container + "-workspace"
}

// NitroMbxRemote is the host-side settings for the per-VM proxy.
type NitroMbxRemote struct {
	URL    string
	Region *string
	Budget *string
	// Namespaces: the first namespace is the guest's default.
	Namespaces []string
}

// nitroRunArgs is the run arguments for the engine: the shape
// containerRunArgs gives Podman, minus what a VM has no use for
// (capabilities, security options, a pids limit, host mounts, user
// namespaces) and with options spelled the way the engine contract lists
// them.
func nitroRunArgs(template *core.TargetsContainerTemplate, name string, sessionID string, additionalMounts []core.AdditionalMount, workspaceRoot string, mbxRemote *NitroMbxRemote) ([]string, error) {
	panic("unported: mj-targets/src/targets/nitro.rs nitro_run_args")
}

// nitroContainerRun creates the sandbox, then makes `/tmp` the world-writable
// sticky directory the Podman launch script makes it (the fresh volume is
// root-owned 0755).
func nitroContainerRun(template *core.TargetsContainerTemplate, name string, sessionID string, additionalMounts []core.AdditionalMount, ssh *core.SshTarget, workspaceRoot string, mbxRemote *NitroMbxRemote) ([]core.CommandSpec, error) {
	panic("unported: mj-targets/src/targets/nitro.rs nitro_container_run")
}

// inheritedEnvironmentNames is the names of the `--env NAME` arguments that
// carry no value.
func inheritedEnvironmentNames(arguments []string) []string {
	var names []string
	for i := 0; i+1 < len(arguments); i++ {
		if arguments[i] == "--env" && !strings.Contains(arguments[i+1], "=") {
			names = append(names, arguments[i+1])
		}
	}
	return names
}

// engineScript is a script that runs the engine as `mj_microvm`. It is the
// same command as core.EngineArgv, spelled for a script the login shell
// hands to `sh -c`.
func engineScript(body string) string {
	return fmt.Sprintf("set -eu\nmj_microvm() { %s \"$@\"; }\n%s", core.EngineShellWords(core.MicrovmEngine), body)
}

func nitroScriptCommand(ssh *core.SshTarget, body string, arguments []string) core.CommandSpec {
	words := []string{"sh", "-c", engineScript(body), "mj-nitro"}
	words = append(words, arguments...)
	return core.SSHCommandOwned(core.SshTrafficClassControl, ssh, words)
}

// `--time 0` matches the Podman scripts' shape, but the engine does not kill
// the VM outright: it maps 0 to its default grace (`mj-worker` microvm
// `cli.rs`, `Command::Stop`), so the guest flushes, syncs and unmounts before
// the unit stops, and the retained disks hold every completed write.
const nitroStopScript = `container=$1
mj_microvm stop --time 0 --ignore "$container" >/dev/null
`

const nitroQuiesceScript = `container=$1
mj_microvm stop --time 0 --ignore "$container" >/dev/null
# ` + "`container exists`" + ` exits 0 for present, 1 for absent, and anything else for an
# engine or storage failure.
if mj_microvm container exists "$container"; then
    running=$(mj_microvm container inspect --format '{{.State.Running}}' "$container")
    [ "$running" = false ] || { echo 'Nitro session sandbox is still running' >&2; exit 1; }
else
    status=$?
    [ "$status" = 1 ] || exit "$status"
fi
`

const nitroRemoveScript = `container=$1
mj_microvm rm --force --ignore "$container" >/dev/null
`

const nitroRemoveVolumesScript = `for volume in "$@"; do
    mj_microvm volume rm --force "$volume" >/dev/null
done
`

// Like `podman container exists`, `exists` exits 0 for present, 1 for absent,
// and anything else for an engine failure.
const nitroAbsentScript = `for resource in "container $1" "volume $2" "volume $3"; do
    if mj_microvm ${resource% *} exists "${resource#* }"; then
        exit 1
    else
        status=$?
        [ "$status" = 1 ] || exit 2
    fi
done
`

// nitroQuiesceCommand stops a sandbox without deleting its disks, and proves
// it is not running.
func nitroQuiesceCommand(ssh *core.SshTarget, container string) core.CommandSpec {
	return nitroScriptCommand(ssh, nitroQuiesceScript, []string{container}).
		WithPurpose("stop exact Nitro session sandbox without removing storage").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageStoppingTarget})
}

// nitroDestroyCommands stops, then removes the sandbox, then its volumes.
// The order matters: the engine kills the Firecracker process before it
// deletes a disk, and a volume goes only after the sandbox that mounts it is
// gone.
func nitroDestroyCommands(ssh *core.SshTarget, container string) []core.CommandSpec {
	return []core.CommandSpec{
		nitroScriptCommand(ssh, nitroStopScript+nitroRemoveScript, []string{container}).
			WithPurpose("remove exact stopped Nitro session sandbox").
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageRemovingContainer}),
		nitroScriptCommand(ssh, nitroRemoveVolumesScript, []string{NitroWorkspaceVolume(container), ContainerTemporaryVolumeName(container)}).
			WithPurpose("remove exact Nitro session volumes").
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageRemovingStorage}),
	}
}

// nitroDestroyInOneCommand is the same removal as one command, for a
// disposable smoke test that runs its cleanup as a single step.
func nitroDestroyInOneCommand(ssh *core.SshTarget, container string) core.CommandSpec {
	return nitroScriptCommand(ssh, nitroStopScript+nitroRemoveScript+"shift\n"+nitroRemoveVolumesScript,
		[]string{container, NitroWorkspaceVolume(container), ContainerTemporaryVolumeName(container)}).
		WithPurpose("remove disposable setup sandbox")
}

// nitroAbsenceCommand exits 0 when nothing of the session remains, 1 when
// something does, and anything else when the engine could not answer.
func nitroAbsenceCommand(ssh *core.SshTarget, container string) core.CommandSpec {
	return nitroScriptCommand(ssh, nitroAbsentScript,
		[]string{container, NitroWorkspaceVolume(container), ContainerTemporaryVolumeName(container)}).
		WithPurpose("confirm exact Nitro session resources are absent")
}

// nitroInspectCommand reads the sandbox's mounts, from the engine's full
// `container inspect` JSON (a one-element array), so the controller needs no
// `--format` template.
func nitroInspectCommand(ssh *core.SshTarget, container string) core.CommandSpec {
	return core.SSHEngineCommand(core.SshTrafficClassControl, ssh, core.MicrovmEngine, []string{"container", "inspect", container}).
		WithPurpose("inspect session container storage")
}
