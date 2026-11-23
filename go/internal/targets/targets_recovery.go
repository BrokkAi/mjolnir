// Port of mj-targets/src/targets/recovery.rs.

package targets

import (
	"encoding/json"
	"errors"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProvisionOnLocatorPlan is the clone/bootstrap commands for AWS once the
// exact instance ID and address are known.
func ProvisionOnLocatorPlan(locator *core.TargetsTargetLocator, sessionID string, bundle *core.ProjectBundleSpec, githubToken *string) (core.CommandPlan, error) {
	if err := bundle.Validate(); err != nil {
		return core.CommandPlan{}, err
	}
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return core.CommandPlan{}, err
	}
	if core.IsBorrowed(locator) {
		return core.CommandPlan{}, errors.New("refusing to operate on a target borrowed from another session; act on the owning session instead")
	}
	if locator.Kind != core.TargetsTargetLocatorAwsEc2 {
		return core.CommandPlan{}, errors.New("post-launch provisioning is only required for AWS")
	}
	ssh, workspace := locator.SSH, locator.Workspace
	commands := []core.CommandSpec{
		core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"mkdir", "-p", workspace}).
			WithPurpose("create EC2 session workspace").
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageCloning}),
	}
	commands = append(commands, InstallGitPlan(ExecutionBoundarySsh{SSH: ssh}).Commands...)
	commands = append(commands, CloneCommands(bundle, workspace, githubToken, func(args []string) core.CommandSpec {
		return core.SSHCommandOwned(core.SshTrafficClassBulk, ssh, args)
	})...)
	return core.CommandPlan{
		Description: "initialize EC2 session " + sessionID,
		Commands:    commands,
	}, nil
}

// RecoverCheckoutPlan reconstructs repositories without creating or retiring
// their target.
func RecoverCheckoutPlan(
	locator *core.TargetsTargetLocator,
	sessionID string,
	bundle *core.ProjectBundleSpec,
	workspace string,
	githubToken *string,
	existingRepositories map[string]struct{},
) (core.CommandPlan, error) {
	if err := bundle.Validate(); err != nil {
		return core.CommandPlan{}, err
	}
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return core.CommandPlan{}, err
	}
	if core.IsBorrowed(locator) {
		return core.CommandPlan{}, errors.New("recover the owning session's checkout first")
	}
	missing := core.ProjectBundleSpec{Primary: bundle.Primary, Repositories: []core.RepositorySpec{}}
	for _, repository := range bundle.Repositories {
		if _, exists := existingRepositories[repository.Destination]; !exists {
			missing.Repositories = append(missing.Repositories, repository)
		}
	}
	return core.CommandPlan{
		Description: "reconstruct missing checkout in staging",
		Commands: CloneCommands(&missing, workspace, githubToken, func(args []string) core.CommandSpec {
			return core.LocatorCommand(core.SshTrafficClassBulk, locator, args)
		}),
	}, nil
}

func ReconnectPlan(locator *core.TargetsTargetLocator, sessionID string) (core.CommandPlan, error) {
	return relayProxyPlan(locator, sessionID, core.SshTrafficClassControl)
}

// ProjectMemoryRelayCommand is the relay connection that carries one
// project-memory transaction. A paged snapshot or upload moves frames of up
// to 8 MiB; on the shared Control connection they sit in its send queue
// ahead of every other session's small relay RPC and channel open on that
// host (#1580). This is the only place that picks the lane for
// project-memory traffic.
func ProjectMemoryRelayCommand(locator *core.TargetsTargetLocator, sessionID string) (core.CommandSpec, error) {
	plan, err := relayProxyPlan(locator, sessionID, core.SshTrafficClassBulk)
	if err != nil {
		return core.CommandSpec{}, err
	}
	if len(plan.Commands) == 0 {
		return core.CommandSpec{}, errors.New("project-memory relay plan is empty")
	}
	return plan.Commands[0], nil
}

func relayProxyPlan(locator *core.TargetsTargetLocator, sessionID string, class core.SshTrafficClass) (core.CommandPlan, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return core.CommandPlan{}, err
	}
	root, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return core.CommandPlan{}, err
	}
	binary := root + "/hel"
	command := core.LocatorCommand(class, locator, []string{binary, "worker", "proxy", "--root", root}).
		WithPurpose("connect to Mjolnir worker").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageStarting})
	return core.CommandPlan{
		Description: "reconnect Mjolnir session " + sessionID,
		Commands:    []core.CommandSpec{command},
	}, nil
}

// TargetRecoveryPlan describes safe recovery for a container that belongs to
// an active session; nil means no plan. The
// inspect command is deliberately separate from `exec`: a host crash can
// leave the container present but stopped, where `exec` cannot distinguish
// that state from other transport failures.
func TargetRecoveryPlan(locator *core.TargetsTargetLocator, sessionID string) (*core.TargetRecoveryPlan, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return nil, err
	}
	// Nothing to recover at the target level for a borrowed worker; the
	// owning session recovers the target. Reporting no plan keeps the child's
	// liveness probe and worker restart working.
	if core.IsBorrowed(locator) {
		return nil, nil
	}
	// Apple container keeps a stopped container, unlike a Podman or Docker
	// one whose daemon restarts it, so it has no recovery plan; a remote
	// Docker host recovers exactly as a local one does.
	if locator.Kind == core.TargetsTargetLocatorSshDocker {
		local, err := TargetRecoveryPlan(&core.TargetsTargetLocator{
			Kind:        core.TargetsTargetLocatorLocalDocker,
			ContainerID: locator.ContainerID,
		}, sessionID)
		if err != nil || local == nil {
			return nil, err
		}
		ssh := locator.SSH
		return &core.TargetRecoveryPlan{
			Exists:    commandOverSSH(core.SshTrafficClassControl, local.Exists, ssh),
			Inspect:   commandOverSSH(core.SshTrafficClassControl, local.Inspect, ssh),
			Start:     commandOverSSH(core.SshTrafficClassControl, local.Start, ssh),
			SessionID: local.SessionID,
		}, nil
	}

	containerID := locator.ContainerID
	var exists, inspect, start core.CommandSpec
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
		exists = core.NewCommandSpec("podman", []string{"container", "exists", containerID}).
			WithPurpose("check for Mjolnir session container")
		inspect = core.NewCommandSpec("podman", []string{"container", "inspect", containerID}).
			WithPurpose("inspect Mjolnir session container")
		start = core.NewCommandSpec("podman", []string{"start", containerID}).
			WithPurpose("start stopped Mjolnir session container")
	case core.TargetsTargetLocatorLocalDocker:
		exists = core.NewCommandSpec("sh", []string{
			"-c",
			`docker container inspect "$1" >/dev/null 2>&1 && exit 0; docker info >/dev/null 2>&1 && exit 1; exit 125`,
			"mj-docker-exists",
			containerID,
		}).WithPurpose("check for Mjolnir Docker session container")
		inspect = core.NewCommandSpec("docker", []string{"container", "inspect", containerID}).
			WithPurpose("inspect Mjolnir Docker session container")
		start = core.NewCommandSpec("docker", []string{"start", containerID}).
			WithPurpose("start stopped Mjolnir Docker session container")
	case core.TargetsTargetLocatorSshPodman:
		ssh := locator.SSH
		exists = core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"podman", "container", "exists", containerID}).
			WithPurpose("check for remote Mjolnir session container")
		inspect = core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"podman", "container", "inspect", containerID}).
			WithPurpose("inspect remote Mjolnir session container")
		start = core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"podman", "start", containerID}).
			WithPurpose("start stopped remote Mjolnir session container")
	case core.TargetsTargetLocatorSshNitro:
		// A retained sandbox stops when the host reboots or the unit dies,
		// and the engine relaunches it from its retained disks, so it
		// recovers exactly like Podman, through the engine on the host.
		ssh := locator.SSH
		exists = core.SSHEngineCommand(core.SshTrafficClassControl, ssh, core.MicrovmEngine, []string{"container", "exists", containerID}).
			WithPurpose("check for remote Mjolnir session sandbox")
		inspect = core.SSHEngineCommand(core.SshTrafficClassControl, ssh, core.MicrovmEngine, []string{"container", "inspect", containerID}).
			WithPurpose("inspect remote Mjolnir session sandbox")
		start = core.SSHEngineCommand(core.SshTrafficClassControl, ssh, core.MicrovmEngine, []string{"start", containerID}).
			WithPurpose("start stopped remote Mjolnir session sandbox")
	default:
		// SshAppleContainer, LocalBare, AppleContainer, AwsEc2 and SshBare.
		return nil, nil
	}
	return &core.TargetRecoveryPlan{Exists: exists, Inspect: inspect, Start: start, SessionID: sessionID}, nil
}

// EnsureRecoveryTargetRunning starts a confirmed stopped container target
// and verifies it reached `running`. Missing or foreign containers,
// transport failures, and transitional states fail without running the
// start command.
func EnsureRecoveryTargetRunning(executor core.CommandExecutor, plan *core.TargetRecoveryPlan) (core.TargetRecoveryOutcome, error) {
	if plan == nil {
		return core.TargetRecoveryOutcomeNotRequired, nil
	}
	existence, err := executor.Execute(&plan.Exists)
	if err != nil {
		return 0, fmt.Errorf("check whether container session target exists: %w", err)
	}
	switch existence.Status {
	case 0:
	case 1:
		// `podman container exists` deliberately reserves 1 for absence and
		// uses 125 for invocation or storage failures. SSH preserves the
		// remote exit status, so this contract also covers remote Podman.
		return core.TargetRecoveryOutcomeMissing, nil
	default:
		if _, err := core.CheckedCommandOutput(&plan.Exists, existence); err != nil {
			return 0, fmt.Errorf("check whether container session target exists: %w", err)
		}
		panic("unreachable: a successful checked command has status zero")
	}
	status, err := inspectRecoveryTarget(executor, plan)
	if err != nil {
		return 0, err
	}
	switch status {
	case "running":
		return core.TargetRecoveryOutcomeAlreadyRunning, nil
	case "created", "initialized", "stopped", "exited":
		output, err := executor.Execute(&plan.Start)
		if err != nil {
			return 0, err
		}
		if _, err := core.CheckedCommandOutput(&plan.Start, output); err != nil {
			return 0, fmt.Errorf("start confirmed stopped container session target: %w", err)
		}
		after, err := inspectRecoveryTarget(executor, plan)
		if err != nil {
			return 0, fmt.Errorf("verify container session target after starting it: %w", err)
		}
		if after != "running" {
			return 0, fmt.Errorf("container session target reported %s after start", core.RustDebugString(after))
		}
		return core.TargetRecoveryOutcomeStarted, nil
	case "paused", "removing", "stopping", "unknown":
		return 0, fmt.Errorf("refusing to start container session target in %s state", core.RustDebugString(status))
	}
	return 0, fmt.Errorf("container session target reported unexpected state %s", core.RustDebugString(status))
}

func inspectRecoveryTarget(executor core.CommandExecutor, plan *core.TargetRecoveryPlan) (string, error) {
	output, err := executor.Execute(&plan.Inspect)
	if err != nil {
		return "", err
	}
	output, err = core.CheckedCommandOutput(&plan.Inspect, output)
	if err != nil {
		return "", fmt.Errorf("inspect container session target for recovery: %w", err)
	}
	var values []any
	if err := json.Unmarshal(output.Stdout, &values); err != nil {
		return "", fmt.Errorf("parse container target inspection: %w", err)
	}
	if len(values) != 1 {
		return "", fmt.Errorf("container inspection returned %d targets instead of one", len(values))
	}
	target := values[0]
	labels, ok := recoveryJSONPointer(target, "Config", "Labels").(map[string]any)
	if !ok {
		return "", errors.New("container session target has no ownership labels")
	}
	if managed, _ := labels[core.ManagedLabel].(string); managed != "true" {
		return "", errors.New("refusing to start a container target Mjolnir does not own")
	}
	if owner, ok := labels[core.SessionLabel].(string); !ok || owner != plan.SessionID {
		return "", errors.New("refusing to start a container target owned by another session")
	}
	state, ok := recoveryJSONPointer(target, "State", "Status").(string)
	if !ok {
		return "", errors.New("container session target inspection has no state")
	}
	return state, nil
}

// recoveryJSONPointer is serde_json's Value::pointer over object keys: the value at
// path, or nil when a step is missing or not an object.
func recoveryJSONPointer(value any, path ...string) any {
	for _, key := range path {
		object, ok := value.(map[string]any)
		if !ok {
			return nil
		}
		value, ok = object[key]
		if !ok {
			return nil
		}
	}
	return value
}
