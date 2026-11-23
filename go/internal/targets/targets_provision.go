// Port of mj-targets/src/targets/provision.rs.

package targets

import (
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProvisionPlan provisions a session's target. containerWorkspace is the
// session record's recorded container workspace. Container targets clone
// into it and mount their workspace storage there; a session that predates
// per-session workspaces records none (nil) and keeps the shared
// `/workspace`.
func ProvisionPlan(
	template *core.TargetsTargetTemplate,
	sessionID string,
	bundle *core.ProjectBundleSpec,
	additionalMounts []core.AdditionalMount,
	containerWorkspace *string,
) (core.CommandPlan, error) {
	name, err := core.ResourceName(sessionID)
	if err != nil {
		return core.CommandPlan{}, err
	}
	return ProvisionPlanNamed(template, sessionID, bundle, additionalMounts, containerWorkspace, name, ProvisionOptions{})
}

type ProvisionOptions struct {
	GithubToken    *string
	NitroMbxRemote *NitroMbxRemote
}

func ProvisionPlanNamed(
	template *core.TargetsTargetTemplate,
	sessionID string,
	bundle *core.ProjectBundleSpec,
	additionalMounts []core.AdditionalMount,
	containerWorkspace *string,
	name string,
	options ProvisionOptions,
) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/provision.rs provision_plan_named")
}

// ProvisionLocalBundlePlan is an independent bundle workspace on the
// controller's host. The controller supplies the recorded worker root so
// provisioning and restoration agree.
func ProvisionLocalBundlePlan(bundle *core.ProjectBundleSpec, workerRoot string, githubToken *string) (core.CommandPlan, error) {
	if err := bundle.Validate(); err != nil {
		return core.CommandPlan{}, err
	}
	return core.CommandPlan{
		Description: "provision local bundle workspace",
		Commands: CloneCommands(bundle, workerRoot, githubToken, func(args []string) core.CommandSpec {
			return core.NewCommandSpec(args[0], append([]string(nil), args[1:]...))
		}),
	}, nil
}

// ProvisionBareProjectPlan builds the no-op infrastructure plan for an
// existing bare project. The wizard validates the project for early
// feedback; worker/ACP startup is authoritative if it changes before launch.
// Worker state is installed later under the dedicated worker and profile
// roots, not under a cloned workspace.
func ProvisionBareProjectPlan(template *core.TargetsTargetTemplate, sessionID string, projectDirectory string) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/provision.rs provision_bare_project_plan")
}

// SetupSmokePlan creates the short-lived local container used to verify a
// setup target.
//
// This deliberately shares the same argv construction as session targets so
// setup catches an unusable image or runtime before the first session
// exists.
func SetupSmokePlan(template *core.TargetsTargetTemplate, smokeID string) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/provision.rs setup_smoke_plan")
}

// RunSetupSmokeTest runs the disposable setup smoke test and always attempts
// container cleanup after a successful create step.
//
// The test runs the target's image, which the engine pulls if it is missing.
// It holds the image download lock while it runs, so it waits for a daemon
// that is downloading the same image instead of pulling it a second time at
// once (J-13).
func RunSetupSmokeTest(template *core.TargetsTargetTemplate, smokeID string, executor core.CommandExecutor) error {
	_, err := WithImageReady(template, executor, func() (struct{}, error) {
		return struct{}{}, runSetupSmokeTestUnlocked(template, smokeID, executor)
	})
	return err
}

func runSetupSmokeTestUnlocked(template *core.TargetsTargetTemplate, smokeID string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/provision.rs run_setup_smoke_test_unlocked")
}

func runSSHDockerOverlaySmokeTest(ssh *core.SshTarget, container *core.TargetsContainerTemplate, smokeID string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/provision.rs run_ssh_docker_overlay_smoke_test")
}

const dockerOverlaySmokeProbe = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && printf 'changed\\n' >/mnt/hel-overlay-smoke/original.txt && printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt"

// dockerReadOnlySmokeProbe is the attachment a VM-hosted Docker daemon gets
// instead of the overlay: the container reads the host directory and cannot
// write to it.
const dockerReadOnlySmokeProbe = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && ! printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt 2>/dev/null"

// dockerOverlaySmokeDirectory creates the smoke test's source directory and
// returns its path. macOS temporary directories are not normally shared into
// Docker VMs. The home directory is shared by Colima's default
// configuration.
func dockerOverlaySmokeDirectory() (string, error) {
	panic("unported: mj-targets/src/targets/provision.rs docker_overlay_smoke_directory")
}

func runDockerOverlaySmokeTest(container *core.TargetsContainerTemplate, smokeID string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/provision.rs run_docker_overlay_smoke_test")
}

func executeChecked(executor core.CommandExecutor, command *core.CommandSpec) error {
	output, err := executor.Execute(command)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("%s failed with status %d: %s", command.Purpose, output.Status, strings.TrimSpace(lossyText(output.Stderr)))
	}
	core.LogCommandWarnings(command, &output)
	return nil
}
