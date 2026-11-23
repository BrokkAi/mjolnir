// Port of mj-targets/src/targets/provision.rs.

package targets

import (
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
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
	githubToken := options.GithubToken
	belongs, err := core.ResourceNameBelongsTo(name, sessionID)
	if err != nil {
		return core.CommandPlan{}, err
	}
	if !belongs {
		return core.CommandPlan{}, errors.New("invalid target resource generation")
	}
	if err := bundle.Validate(); err != nil {
		return core.CommandPlan{}, err
	}
	if _, container := template.ContainerEngine(); container {
		for _, repository := range bundle.Repositories {
			if repository.URL != nil && repository.Reference == nil && core.IsHostOnlyGitSource(*repository.URL) {
				return core.CommandPlan{}, core.RefusalPrecondition(fmt.Sprintf(
					"Git source %s is host-only and needs a container host with the Git cache. Choose a target that can mount the cache or configure a reachable Git source.",
					core.DisplayURL(*repository.URL)))
			}
		}
	}
	workspace := core.ContainerWorkspaceRoot(containerWorkspace)
	if len(additionalMounts) > 0 {
		switch template.Kind {
		case core.TargetsTargetTemplateLocalPodman, core.TargetsTargetTemplateLocalDocker,
			core.TargetsTargetTemplateAppleContainer, core.TargetsTargetTemplateSshPodman,
			core.TargetsTargetTemplateSshDocker, core.TargetsTargetTemplateSshAppleContainer,
			core.TargetsTargetTemplateSshNitro:
		default:
			return core.CommandPlan{}, errors.New("additional mounts require a container-backed target")
		}
	}
	// Apple's `container` CLI drives the host's own VM disk image, so the
	// remote plan is the local one with every command run over SSH; a remote
	// Docker host provisions exactly as a local one does.
	if template.Kind == core.TargetsTargetTemplateSshAppleContainer || template.Kind == core.TargetsTargetTemplateSshDocker {
		if err := core.ValidateSSH(template.SSH); err != nil {
			return core.CommandPlan{}, err
		}
		local := core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateLocalDocker, Container: template.Container}
		if template.Kind == core.TargetsTargetTemplateSshAppleContainer {
			local.Kind = core.TargetsTargetTemplateAppleContainer
		}
		plan, err := ProvisionPlanNamed(&local, sessionID, bundle, additionalMounts, containerWorkspace, name, options)
		if err != nil {
			return core.CommandPlan{}, err
		}
		for i, command := range plan.Commands {
			plan.Commands[i] = commandOverSSH(core.SshTrafficClassBulk, command, template.SSH)
		}
		return plan, nil
	}
	var commands []core.CommandSpec
	provisioning := core.ProvisionStage{Kind: core.ProvisionStageProvisioning}
	inContainer := func(engine string) func(args []string) core.CommandSpec {
		return func(args []string) core.CommandSpec { return core.ContainerExec(engine, name, args) }
	}
	switch template.Kind {
	case core.TargetsTargetTemplateLocalBare:
		return core.CommandPlan{}, errors.New("local bare projects must use the existing-project provisioning path")
	case core.TargetsTargetTemplateLocalPodman:
		if err := core.ValidateContainerTemplate(template.Container); err != nil {
			return core.CommandPlan{}, err
		}
		run, err := podmanContainerRun(template.Container, name, sessionID, additionalMounts, nil, workspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, run)
		commands = append(commands, InstallGitPlan(ExecutionBoundaryContainer{Engine: "podman", ContainerID: name}).Commands...)
		commands = append(commands, CloneCommands(bundle, workspace, githubToken, inContainer("podman"))...)
	case core.TargetsTargetTemplateLocalDocker:
		if err := core.ValidateContainerTemplate(template.Container); err != nil {
			return core.CommandPlan{}, err
		}
		run, err := dockerContainerRun(template.Container, name, sessionID, additionalMounts, workspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, run)
		commands = append(commands, InstallGitPlan(ExecutionBoundaryContainer{Engine: "docker", ContainerID: name}).Commands...)
		commands = append(commands, CloneCommands(bundle, workspace, githubToken, inContainer("docker"))...)
	case core.TargetsTargetTemplateAppleContainer:
		if err := core.ValidateContainerTemplate(template.Container); err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, core.NewCommandSpec("container", []string{"system", "status"}).
			WithPurpose("check Apple container service").
			WithStage(provisioning))
		commands = append(commands, appleImagePrepareCommands(template.Container)...)
		run, err := containerRun("container", template.Container, name, sessionID, additionalMounts, workspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, run)
		commands = append(commands, InstallGitPlan(ExecutionBoundaryContainer{Engine: "container", ContainerID: name}).Commands...)
		commands = append(commands, CloneCommands(bundle, workspace, githubToken, inContainer("container"))...)
	case core.TargetsTargetTemplateSshBare:
		ssh := template.SSH
		if err := core.ValidateSSH(ssh); err != nil {
			return core.CommandPlan{}, err
		}
		workspace, err := core.WorkspaceFor(template, sessionID)
		if err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"mkdir", "-p", workspace}).
			WithPurpose("create SSH session workspace").
			WithStage(provisioning).
			WithCreatesTarget())
		commands = append(commands, InstallGitPlan(ExecutionBoundarySsh{SSH: ssh}).Commands...)
		commands = append(commands, CloneCommands(bundle, workspace, githubToken, func(args []string) core.CommandSpec {
			return core.SSHCommandOwned(core.SshTrafficClassBulk, ssh, args)
		})...)
	case core.TargetsTargetTemplateSshNitro:
		ssh := template.SSH
		if err := core.ValidateSSH(ssh); err != nil {
			return core.CommandPlan{}, err
		}
		if err := core.ValidateContainerTemplate(template.Container); err != nil {
			return core.CommandPlan{}, err
		}
		// The engine creates the workspace and /tmp volumes and removes them
		// again if the launch fails, so the plan is the engine's `run`
		// directly, not a shell wrapper that does that itself.
		run, err := nitroContainerRun(template.Container, name, sessionID, additionalMounts, ssh, workspace, options.NitroMbxRemote)
		if err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, run...)
		commands = append(commands, InstallGitPlan(ExecutionBoundarySshContainer{Engine: core.MicrovmEngine, SSH: ssh, ContainerID: name}).Commands...)
		commands = append(commands, CloneCommands(bundle, workspace, githubToken, func(args []string) core.CommandSpec {
			return commandOverSSH(core.SshTrafficClassBulk, core.ContainerExec(core.MicrovmEngine, name, args), ssh)
		})...)
	case core.TargetsTargetTemplateSshPodman:
		ssh := template.SSH
		if err := core.ValidateSSH(ssh); err != nil {
			return core.CommandPlan{}, err
		}
		if err := core.ValidateContainerTemplate(template.Container); err != nil {
			return core.CommandPlan{}, err
		}
		run, err := podmanContainerRun(template.Container, name, sessionID, additionalMounts, ssh, workspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands, run)
		commands = append(commands, InstallGitPlan(ExecutionBoundarySshContainer{Engine: "podman", SSH: ssh, ContainerID: name}).Commands...)
		commands = append(commands, CloneCommands(bundle, workspace, githubToken, func(args []string) core.CommandSpec {
			return commandOverSSH(core.SshTrafficClassBulk, core.ContainerExec("podman", name, args), ssh)
		})...)
	default:
		return core.CommandPlan{}, fmt.Errorf("unknown target kind %s", core.RustDebugString(string(template.Kind)))
	}
	return core.CommandPlan{
		Description: "provision Mjolnir session " + sessionID,
		Commands:    commands,
	}, nil
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
	if err := core.ValidateBareProjectPath(projectDirectory); err != nil {
		return core.CommandPlan{}, err
	}
	switch template.Kind {
	case core.TargetsTargetTemplateLocalBare:
	case core.TargetsTargetTemplateSshBare:
		if err := core.ValidateSSH(template.SSH); err != nil {
			return core.CommandPlan{}, err
		}
		if _, err := core.WorkspaceFor(template, sessionID); err != nil {
			return core.CommandPlan{}, err
		}
	default:
		return core.CommandPlan{}, errors.New("raw project directories require a bare target")
	}
	return core.CommandPlan{
		Description: "provision Mjolnir session " + sessionID,
		Commands:    []core.CommandSpec{},
	}, nil
}

// SetupSmokePlan creates the short-lived local container used to verify a
// setup target.
//
// This deliberately shares the same argv construction as session targets so
// setup catches an unusable image or runtime before the first session
// exists.
func SetupSmokePlan(template *core.TargetsTargetTemplate, smokeID string) (core.CommandPlan, error) {
	name, err := core.ResourceName(smokeID)
	if err != nil {
		return core.CommandPlan{}, err
	}
	description := "smoke test Mjolnir setup target " + smokeID
	if template.Kind == core.TargetsTargetTemplateSshNitro {
		ssh := template.SSH
		if err := core.ValidateSSH(ssh); err != nil {
			return core.CommandPlan{}, err
		}
		if err := core.ValidateContainerTemplate(template.Container); err != nil {
			return core.CommandPlan{}, err
		}
		create, err := nitroContainerRun(template.Container, name, smokeID, nil, ssh, core.ContainerWorkspace, nil)
		if err != nil {
			return core.CommandPlan{}, err
		}
		return core.CommandPlan{
			Description: description,
			Commands: []core.CommandSpec{
				create[0].WithPurpose("create disposable setup sandbox"),
				core.SSHEngineCommand(core.SshTrafficClassControl, ssh, core.MicrovmEngine, []string{"exec", "-i", name, "true"}).
					WithPurpose("execute setup smoke command"),
				nitroDestroyInOneCommand(ssh, name),
			},
		}, nil
	}
	var engine string
	var boundary ExecutionBoundary = ExecutionBoundaryDirect{}
	switch template.Kind {
	case core.TargetsTargetTemplateLocalPodman:
		engine = "podman"
	case core.TargetsTargetTemplateLocalDocker:
		engine = "docker"
	case core.TargetsTargetTemplateAppleContainer:
		engine = "container"
	case core.TargetsTargetTemplateSshPodman, core.TargetsTargetTemplateSshDocker, core.TargetsTargetTemplateSshAppleContainer:
		if err := core.ValidateSSH(template.SSH); err != nil {
			return core.CommandPlan{}, err
		}
		engine, _ = template.ContainerEngine()
		boundary = ExecutionBoundarySsh{SSH: template.SSH}
	default:
		return core.CommandPlan{}, errors.New("setup smoke tests require a local or SSH container target")
	}
	container := template.Container
	if err := core.ValidateContainerTemplate(container); err != nil {
		return core.CommandPlan{}, err
	}

	var run core.CommandSpec
	var locator core.TargetsTargetLocator
	switch engine {
	case "podman":
		run, err = podmanContainerRun(container, name, smokeID, nil, nil, core.ContainerWorkspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		storage, err := core.PodmanWorkspaceLocatorNamed(container, name)
		if err != nil {
			return core.CommandPlan{}, err
		}
		locator = core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: name, WorkspaceStorage: storage}
	case "docker":
		run, err = dockerContainerRun(container, name, smokeID, nil, core.ContainerWorkspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		locator = core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}
	default:
		run, err = containerRun(engine, container, name, smokeID, nil, core.ContainerWorkspace)
		if err != nil {
			return core.CommandPlan{}, err
		}
		if ssh, remote := boundary.(ExecutionBoundarySsh); remote {
			clone := ssh.SSH.Clone()
			locator = core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshAppleContainer, SSH: &clone, ContainerID: name}
		} else {
			locator = core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorAppleContainer, ContainerID: name}
		}
	}
	exec := []string{engine, "exec", "-i", name, "true"}
	cleanup, err := RetireMoveTargetPlan(&locator, smokeID)
	if err != nil {
		return core.CommandPlan{}, err
	}
	cleanupCommands, err := cleanup.CommandsWithoutStaging()
	if err != nil {
		return core.CommandPlan{}, err
	}
	script := "set -eu\n"
	for _, command := range cleanupCommands {
		script += core.JoinRemoteCommand(append([]string{command.Program}, command.Args...)) + "\n"
	}
	remove := []string{"sh", "-c", script}
	if engine == "container" {
		remove = []string{engine, "rm", "--force", name}
	}
	if ssh, remote := boundary.(ExecutionBoundarySsh); remote {
		run = commandOverSSH(core.SshTrafficClassControl, run, ssh.SSH)
	}
	return core.CommandPlan{
		Description: description,
		Commands: []core.CommandSpec{
			run.WithPurpose("create disposable setup container"),
			atBoundary(core.SshTrafficClassControl, boundary, exec).WithPurpose("execute setup smoke command"),
			atBoundary(core.SshTrafficClassControl, boundary, remove).WithPurpose("remove disposable setup container"),
		},
	}, nil
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
	switch template.Kind {
	case core.TargetsTargetTemplateLocalDocker:
		return runDockerOverlaySmokeTest(template.Container, smokeID, executor)
	case core.TargetsTargetTemplateSshDocker:
		return runSSHDockerOverlaySmokeTest(template.SSH, template.Container, smokeID, executor)
	}
	plan, err := SetupSmokePlan(template, smokeID)
	if err != nil {
		return err
	}
	if err := executeChecked(executor, &plan.Commands[0]); err != nil {
		return err
	}
	smokeResult := executeChecked(executor, &plan.Commands[1])
	cleanupResult := executeChecked(executor, &plan.Commands[2])
	if smokeResult != nil {
		return smokeResult
	}
	return cleanupResult
}

func runSSHDockerOverlaySmokeTest(ssh *core.SshTarget, container *core.TargetsContainerTemplate, smokeID string, executor core.CommandExecutor) error {
	if err := core.ValidateSSH(ssh); err != nil {
		return err
	}
	if err := core.ValidateContainerTemplate(container); err != nil {
		return err
	}
	name, err := core.ResourceName(smokeID)
	if err != nil {
		return err
	}
	prepare := core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"sh", "-c",
		"set -eu; root=$(mktemp -d /tmp/mj-docker-overlay-smoke.XXXXXXXXXX); printf 'lower\\n' >\"$root/original.txt\"; chmod 777 \"$root\"; chmod 666 \"$root/original.txt\"; printf '%s\\n' \"$root\""}).
		WithPurpose("create remote Docker OverlayFS smoke source")
	output, err := executor.Execute(&prepare)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("%s failed on %s: %s", prepare.Purpose, ssh.Destination, lossyText(output.Stderr))
	}
	decoded, err := libFromUTF8(output.Stdout)
	if err != nil {
		return fmt.Errorf("decode remote smoke directory: %w", err)
	}
	lower := strings.TrimSpace(decoded)
	if !strings.HasPrefix(lower, "/tmp/mj-docker-overlay-smoke.") || strings.ContainsAny(lower, "\n\r") || strings.Contains(lower, "/../") {
		return fmt.Errorf("unexpected remote smoke directory %s", core.RustDebugString(lower))
	}
	mount := core.AdditionalMount{Source: lower, Destination: "/mnt/hel-overlay-smoke", Access: core.MountAccessCow}
	result := func() error {
		run, err := dockerContainerRun(container, name, smokeID, []core.AdditionalMount{mount}, core.ContainerWorkspace)
		if err != nil {
			return err
		}
		create := commandOverSSH(core.SshTrafficClassControl, run, ssh)
		if err := executeChecked(executor, &create); err != nil {
			return err
		}
		probe := commandOverSSH(core.SshTrafficClassControl,
			core.ContainerExec("docker", name, []string{"sh", "-c", dockerOverlaySmokeProbe}), ssh).
			WithPurpose("verify remote Docker OverlayFS copy-on-write attachment")
		if err := executeChecked(executor, &probe); err != nil {
			return err
		}
		unchanged := core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"sh", "-c",
			"test \"$(cat \"$1/original.txt\")\" = lower && test ! -e \"$1/container-created.txt\"", "mj-check-smoke-source", lower}).
			WithPurpose("verify original remote attachment is unchanged")
		return executeChecked(executor, &unchanged)
	}()
	cleanup := func() error {
		clone := ssh.Clone()
		plan, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshDocker, SSH: &clone, ContainerID: name}, smokeID)
		if err != nil {
			return err
		}
		commands, err := plan.CommandsWithoutStaging()
		if err != nil {
			return err
		}
		for i := range commands {
			if err := executeChecked(executor, &commands[i]); err != nil {
				return err
			}
		}
		// Never remove a lower directory until its container and volumes are
		// gone.
		remove := core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"rm", "-rf", "--", lower}).
			WithPurpose("remove remote Docker smoke source")
		return executeChecked(executor, &remove)
	}()
	switch {
	case result != nil && cleanup != nil:
		return fmt.Errorf("remote smoke cleanup also failed: %s: %w", cleanup.Error(), result)
	case result != nil:
		return result
	}
	return cleanup
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
	parent := os.TempDir()
	if runtime.GOOS == "darwin" {
		home, ok := core.UserHome()
		if !ok {
			return "", errors.New("locate shared home directory for Docker smoke test")
		}
		parent = home
	}
	directory, err := os.MkdirTemp(parent, ".mj-docker-overlay-smoke-*")
	if err != nil {
		return "", fmt.Errorf("create Docker OverlayFS smoke directory: %w", err)
	}
	return directory, nil
}

func runDockerOverlaySmokeTest(container *core.TargetsContainerTemplate, smokeID string, executor core.CommandExecutor) error {
	if err := core.ValidateContainerTemplate(container); err != nil {
		return err
	}
	lower, err := dockerOverlaySmokeDirectory()
	if err != nil {
		return err
	}
	// The directory goes when this returns, as Rust's TempDir does, unless a
	// surviving overlay still references it.
	retained := false
	defer func() {
		if !retained {
			if err := os.RemoveAll(lower); err != nil {
				slog.Warn("could not remove the Docker smoke source", "path", lower, "error", err)
			}
		}
	}()
	original := filepath.Join(lower, "original.txt")
	added := filepath.Join(lower, "container-created.txt")
	if err := os.WriteFile(original, []byte("lower\n"), 0o666); err != nil {
		return fmt.Errorf("write Docker OverlayFS smoke source: %w", err)
	}
	if runtime.GOOS != "windows" {
		// The disposable probe tests the mount, independently of host/image
		// UID.
		if err := os.Chmod(lower, 0o777); err != nil {
			return err
		}
		if err := os.Chmod(original, 0o666); err != nil {
			return err
		}
	}
	name, err := core.ResourceName(smokeID)
	if err != nil {
		return err
	}
	// Test the attachment sessions on this daemon actually get: a VM-hosted
	// daemon cannot overlay host directories, so they mount read-only there.
	_, vmShare, err := LocalDockerVMShare(executor)
	if err != nil {
		return err
	}
	access, probeScript, probePurpose := core.MountAccessCow, dockerOverlaySmokeProbe, "verify Docker OverlayFS copy-on-write attachment"
	if vmShare {
		access, probeScript, probePurpose = core.MountAccessRo, dockerReadOnlySmokeProbe, "verify Docker read-only attachment"
	}
	mount := core.AdditionalMount{Source: lower, Destination: "/mnt/hel-overlay-smoke", Access: access}
	run, err := dockerContainerRun(container, name, smokeID, []core.AdditionalMount{mount}, core.ContainerWorkspace)
	if err != nil {
		return err
	}
	create := run.WithPurpose("create disposable Docker attachment smoke container")
	probe := core.ContainerExec("docker", name, []string{"sh", "-c", probeScript}).WithPurpose(probePurpose)
	plan, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}, smokeID)
	if err != nil {
		return err
	}
	commands, err := plan.CommandsWithoutStaging()
	if err != nil {
		return err
	}
	if len(commands) == 0 {
		return errors.New("Docker OverlayFS smoke cleanup plan is empty")
	}
	cleanup := commands[0]

	smokeResult := executeChecked(executor, &create)
	if smokeResult == nil {
		smokeResult = executeChecked(executor, &probe)
	}
	if cleanupErr := executeChecked(executor, &cleanup); cleanupErr != nil {
		// A surviving overlay still references its lower directory.
		retained = true
		cleanupErr = fmt.Errorf("Docker smoke cleanup failed; retained source at %s: %w", lower, cleanupErr)
		if smokeResult == nil {
			return cleanupErr
		}
		return fmt.Errorf("%s: %w", cleanupErr.Error(), smokeResult)
	}
	if smokeResult != nil {
		return smokeResult
	}
	contents, err := os.ReadFile(original)
	if err != nil {
		return fmt.Errorf("read Docker OverlayFS smoke source after container write: %w", err)
	}
	if string(contents) != "lower\n" {
		return errors.New("Docker attachment smoke test changed its source directory")
	}
	if _, err := os.Stat(added); err == nil {
		return errors.New("Docker attachment smoke test created a file in its source directory")
	}
	return nil
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
