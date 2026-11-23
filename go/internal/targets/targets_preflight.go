// Port of mj-targets/src/targets/preflight.rs.

package targets

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os/exec"
	"runtime"
	"strings"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// managedResourceKind is a kind of resource Hel owns for a session.
type managedResourceKind int

const (
	managedResourceKindContainer managedResourceKind = iota
)

// managedResourceIdentityArgs builds command-line fragments that identify
// resources Hel owns for a session.
func managedResourceIdentityArgs(kind managedResourceKind, sessionID string) []string {
	instance := core.InstanceIdentity()
	switch kind {
	case managedResourceKindContainer:
		return []string{
			"--label", core.SessionLabel + "=" + sessionID,
			"--label", core.ManagedLabel + "=true",
			"--label", core.InstanceLabel + "=" + instance,
		}
	}
	panic(fmt.Sprintf("unknown managed resource kind %d", kind))
}

// PodmanPreflight is what a passing rootless Podman preflight found.
type PodmanPreflight struct {
	Version string
	// Warnings are non-fatal host configuration problems that can make
	// sessions fragile.
	Warnings []PodmanPreflightWarning
}

// PodmanPreflightWarning is one non-fatal host configuration problem.
type PodmanPreflightWarning struct {
	Detail      string
	Remediation string
}

// Notice is the warning as one notice line.
func (w PodmanPreflightWarning) Notice() string {
	return w.Detail + " " + w.Remediation
}

// podmanHost is where the Podman prerequisite probes run: this host when ssh
// is nil (Rust PodmanHost::Local), else the SSH host (PodmanHost::Ssh).
//
// The same postconditions apply locally and over SSH; only the command
// wrapping and the wording of a failure differ.
type podmanHost struct {
	ssh *core.SshTarget
}

// failure is the sentence opener for every failure raised by these probes.
func (h podmanHost) failure() string {
	if h.ssh == nil {
		return "Podman preflight failed"
	}
	return "Remote Podman preflight failed on " + h.ssh.Destination
}

// remediationScope is the prefix that says where a remediation must be
// applied.
func (h podmanHost) remediationScope() string {
	if h.ssh == nil {
		return ""
	}
	return fmt.Sprintf("On %s: ", h.ssh.Destination)
}

// command runs args (program first) on this host. It ports both of Rust's
// `command` and `command_owned`, which differ only in how they borrow args.
func (h podmanHost) command(args []string, purpose string) core.CommandSpec {
	var command core.CommandSpec
	if h.ssh == nil {
		command = core.NewCommandSpec(args[0], args[1:]).WithPurpose(purpose)
	} else {
		command = core.SSHValidationCommand(h.ssh, args, purpose)
	}
	return command.WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
}

// VerifyLocalPodman verifies the fast local preconditions for Hel's rootless
// Podman target.
//
// This intentionally never pulls an image. Image availability is verified by
// `mj doctor --smoke` and by the subsequent target creation command.
func VerifyLocalPodman(executor core.CommandExecutor) (PodmanPreflight, error) {
	return verifyPodman(podmanHost{}, executor)
}

// DockerPreflight is what a passing Docker preflight found.
type DockerPreflight struct {
	Version string
}

// VerifyLocalDocker verifies that the Docker CLI can reach a Linux Docker
// daemon.
//
// Image and OverlayFS support are exercised by `mj doctor --smoke`; this fast
// probe runs before every launch and never pulls an image.
func VerifyLocalDocker(executor core.CommandExecutor) (DockerPreflight, error) {
	return verifyDocker(nil, executor)
}

var appleContainerEnsure sync.Mutex

// appleContainerRuntimeGate is the single gate shared by startup, launch
// readiness, and image refresh. Tests hold it to stand in for a concurrent
// caller.
func appleContainerRuntimeGate() *sync.Mutex {
	return &appleContainerEnsure
}

// AppleContainerRuntime is what ensuring Apple's local runtime did.
type AppleContainerRuntime int

const (
	// AppleContainerRuntimeRunning: the service was already answering health
	// checks.
	AppleContainerRuntimeRunning AppleContainerRuntime = iota
	// AppleContainerRuntimeStarted: the service was stopped, and Mjolnir
	// started it.
	AppleContainerRuntimeStarted
)

// EnsureAppleContainerRuntime ensures Apple's local container service is
// running.
//
// Several launch surfaces can ask at once. One process-wide gate makes the
// check and start one decision, so two callers cannot both try to register
// the same launchd service. `--disable-kernel-install` keeps this unattended
// path from prompting for, or silently downloading, Apple's default kernel;
// the manual command documented for first-time setup remains interactive.
func EnsureAppleContainerRuntime(executor core.CommandExecutor) (AppleContainerRuntime, error) {
	panic("unported: mj-targets/src/targets/preflight.rs ensure_apple_container_runtime")
}

// AppleContainerStatusKind names an AppleContainerStatus variant.
type AppleContainerStatusKind int

const (
	// AppleContainerStatusRunning: the command exited with status 0.
	AppleContainerStatusRunning AppleContainerStatusKind = iota
	// AppleContainerStatusStopped: the command ran and exited nonzero, which
	// is how it reports a stopped service.
	AppleContainerStatusStopped
)

// AppleContainerStatus is what `container system status` says about Apple's
// local container service. One struct with a kind field: Status and Detail
// are set for Stopped, Detail being whatever the command printed.
type AppleContainerStatus struct {
	Kind   AppleContainerStatusKind
	Status int
	Detail string
}

// AppleContainerRuntimeStatus asks whether Apple's local container service is
// running, without changing anything.
//
// This is the one owner of that question. Callers that only need to know (the
// launch options and the image refresher) use it alone; only
// EnsureAppleContainerRuntime goes on to start the service. An error means
// the answer is unknown: the command could not run, timed out, or was
// cancelled.
func AppleContainerRuntimeStatus(executor core.CommandExecutor) (AppleContainerStatus, error) {
	status := core.NewCommandSpec("container", AppleContainerStatusProbe[1:]).
		WithPurpose("check Apple container runtime").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	output, err := executor.Execute(&status)
	if err != nil {
		return AppleContainerStatus{}, err
	}
	if output.Status == 0 {
		return AppleContainerStatus{Kind: AppleContainerStatusRunning}, nil
	}
	return AppleContainerStatus{Kind: AppleContainerStatusStopped, Status: output.Status, Detail: appleContainerOutput(&output)}, nil
}

func appleContainerOutput(output *core.CommandOutput) string {
	var messages []string
	for _, message := range []string{strings.TrimSpace(preflightLossy(output.Stdout)), strings.TrimSpace(preflightLossy(output.Stderr))} {
		if message != "" {
			messages = append(messages, message)
		}
	}
	return strings.Join(messages, "\n")
}

// VerifySSHDocker verifies that the Docker CLI on an SSH host can reach a
// Linux Docker daemon.
func VerifySSHDocker(ssh *core.SshTarget, executor core.CommandExecutor) (DockerPreflight, error) {
	if err := core.ValidateSSH(ssh); err != nil {
		return DockerPreflight{}, err
	}
	preflight, err := verifyDocker(ssh, executor)
	if err != nil {
		return DockerPreflight{}, fmt.Errorf("Docker preflight on %s failed; run docker info on that SSH host: %w", ssh.Destination, err)
	}
	return preflight, nil
}

// The remote commands the Apple container check runs on an SSH host, in
// order. The machine probe sends the same command lines in its one batch.
var (
	PreflightPlatformProbe     = [2]string{"uname", "-sm"}
	AppleContainerMacosProbe   = [2]string{"sw_vers", "-productVersion"}
	AppleContainerVersionProbe = [2]string{"container", "--version"}
	AppleContainerStatusProbe  = [3]string{"container", "system", "status"}
)

// AppleContainerPreflight is what a reachable Apple container host reported
// about itself.
type AppleContainerPreflight struct {
	// MacosVersion is the macOS release, as `sw_vers -productVersion` prints
	// it.
	MacosVersion string
	// Version is what `container --version` prints, when the CLI answers it.
	Version string
}

// VerifySSHAppleContainer verifies that an SSH host can run Apple
// containers.
//
// Apple's `container` CLI drives a per-host VM disk image, so a session's
// workspace lives on the SSH host exactly as a local one lives on this Mac.
// There is no daemon socket to reach over a tunnel, which is why this target
// exists as its own kind instead of a tunnel to a local runtime.
//
// Platform support comes first: a Linux host has no `container` command and
// no reason to be told to install one.
func VerifySSHAppleContainer(ssh *core.SshTarget, executor core.CommandExecutor) (AppleContainerPreflight, error) {
	panic("unported: mj-targets/src/targets/preflight.rs verify_ssh_apple_container")
}

// AppleContainerMinimumMacos is the oldest macOS release Apple's official
// signed `container` package supports.
const AppleContainerMinimumMacos uint32 = 26

// LocalDockerVMShare is why the local Docker daemon cannot overlay host
// directories, or false when it runs on this Linux host and sees them
// directly.
//
// On macOS every Docker daemon (Docker Desktop, Colima, OrbStack) runs in a
// Linux VM, as Docker Desktop also does on Linux. The VM reaches host
// directories through a file share (virtiofs, gRPC FUSE, sshfs, 9p) that
// cannot back an overlay lower layer, and the local volume driver may not see
// them at their host path at all (#1152). Bind mounts are translated through
// the share, so these daemons still get read-only attachments.
func LocalDockerVMShare(executor core.CommandExecutor) (string, bool, error) {
	const reason = "Docker runs in a VM that reaches host directories through a file share"
	if runtime.GOOS != "linux" {
		return reason, true, nil
	}
	command := core.NewCommandSpec("docker", []string{"version", "--format", "{{.Server.Platform.Name}}"}).
		WithPurpose("identify the Docker daemon platform").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	output, err := executor.Execute(&command)
	if err != nil {
		return "", false, err
	}
	if output.Status != 0 {
		return "", false, fmt.Errorf("%s failed with status %d: %s", command.Purpose, output.Status, strings.TrimSpace(preflightLossy(output.Stderr)))
	}
	if strings.HasPrefix(strings.TrimSpace(preflightLossy(output.Stdout)), "Docker Desktop") {
		return reason, true, nil
	}
	return "", false, nil
}

// EngineNotInstalled is how the launch options, the session wizard, doctor
// and Setup say that a local container engine's command is not on this host.
func EngineNotInstalled(engine string) string {
	return engine + " is not installed on this host"
}

// LocalEngineCommand is the command a local container target's engine runs
// as. False for any other target, whose readiness is not a local engine's.
func LocalEngineCommand(template core.TargetTemplate) (string, bool) {
	switch template.(type) {
	case core.TargetTemplateLocalPodman, *core.TargetTemplateLocalPodman:
		return "podman", true
	case core.TargetTemplateLocalDocker, *core.TargetTemplateLocalDocker:
		return "docker", true
	case core.TargetTemplateAppleContainer, *core.TargetTemplateAppleContainer:
		return "container", true
	}
	return "", false
}

// RuntimeMissingOnHost reports whether template needs a local container
// engine whose command is not on path, a PATH value (pathOK false: no PATH).
// That is a fact about this host and stays true until the engine is
// installed, unlike a host that did not answer its last check. Every place
// that decides whether to offer a target reads this one answer: the terminal
// wizards, the web viewer's snapshot, and the launch options.
func RuntimeMissingOnHost(template core.TargetTemplate, path string, pathOK bool) bool {
	engine, ok := LocalEngineCommand(template)
	return ok && !ProgramOnPath(engine, path, pathOK)
}

// ProgramOnPath reports whether program is found in one of the directories
// of path, a PATH value, under the platform's executable extensions
// (`docker.exe` on Windows). A missing PATH (pathOK false) finds nothing.
func ProgramOnPath(program string, path string, pathOK bool) bool {
	_, found := harnessinstall.FindProgramOnPath(program, path, pathOK)
	return found
}

// DockerUnavailableKind names a DockerUnavailable variant.
type DockerUnavailableKind int

const (
	// DockerUnavailableNotInstalled: `docker` is not on PATH.
	DockerUnavailableNotInstalled DockerUnavailableKind = iota
	// DockerUnavailableNotRunning: the CLI ran but found no daemon to talk
	// to. Carries Reported.
	DockerUnavailableNotRunning
	// DockerUnavailableNotAnswering: the CLI ran and failed for another
	// reason, such as a socket the user may not open. Carries Status and
	// Reported.
	DockerUnavailableNotAnswering
)

// DockerUnavailable is why local Docker cannot run sessions: one sentence
// per case, where the raw error chain said "run docker for check Docker
// daemon: No such file or directory (os error 2)" (launch finding R5-3).
type DockerUnavailable struct {
	Kind     DockerUnavailableKind
	Status   int
	Reported string
}

// Remedy is what to do before Docker can run sessions, for a check made
// before anything is launched, such as the session wizard's target row. It
// does not mention Retry launch, which only the launch-failure dialog offers
// (launch finding R6-3).
func (d DockerUnavailable) Remedy() string {
	switch d.Kind {
	case DockerUnavailableNotInstalled:
		return "Install Docker or choose another target."
	case DockerUnavailableNotRunning:
		return "Start Docker."
	default:
		return "Fix what it reports."
	}
}

// LaunchRemedy is what to do after a launch failed this check: the same
// advice, then the failure dialog's Retry launch.
func (d DockerUnavailable) LaunchRemedy() string {
	switch d.Kind {
	case DockerUnavailableNotInstalled:
		return "Install Docker or choose another target, then Retry launch."
	case DockerUnavailableNotRunning:
		return "Start Docker, then Retry launch."
	default:
		return "Fix what it reports, then Retry launch."
	}
}

// Remediation is what doctor and Setup advise.
func (d DockerUnavailable) Remediation() string {
	switch d.Kind {
	case DockerUnavailableNotInstalled:
		return fmt.Sprintf("Install Docker (%s), or use another target.", DockerDocumentationURL)
	case DockerUnavailableNotRunning:
		return "Start Docker, then make sure `docker info` succeeds as the user running Mjolnir."
	default:
		return fmt.Sprintf("Make sure `docker info` succeeds as the user running Mjolnir. See %s.", DockerDocumentationURL)
	}
}

// Error is the one-sentence reason (Rust's Display).
func (d DockerUnavailable) Error() string {
	switch d.Kind {
	case DockerUnavailableNotInstalled:
		return EngineNotInstalled("Docker") + "."
	case DockerUnavailableNotRunning:
		message := "Docker is installed, but its daemon is not running."
		if d.Reported != "" {
			message += " `docker version` said: " + d.Reported
		}
		return message
	default:
		message := fmt.Sprintf("Docker did not answer its check on this host: `docker version` exited with status %d", d.Status)
		if d.Reported != "" {
			message += ": " + d.Reported
		}
		return message + ". Run `docker info` as the user running Mjolnir to see why."
	}
}

// dockerDaemonNotRunning reports whether the Docker CLI's own words say it
// found no daemon to talk to.
func dockerDaemonNotRunning(reported string) bool {
	return strings.Contains(reported, "Cannot connect to the Docker daemon") ||
		strings.Contains(reported, "Is the docker daemon running")
}

// isMissingProgram reports whether a command could not start because its
// program is not on PATH.
func isMissingProgram(err error) bool {
	return errors.Is(err, exec.ErrNotFound) || errors.Is(err, fs.ErrNotExist)
}

// DockerServerVersion is the command the Docker check runs: it answers only
// when a daemon does.
var DockerServerVersion = [4]string{
	"docker",
	"version",
	"--format",
	"{{.Server.Version}} {{.Server.Os}}",
}

func verifyDocker(ssh *core.SshTarget, executor core.CommandExecutor) (DockerPreflight, error) {
	panic("unported: mj-targets/src/targets/preflight.rs verify_docker")
}

// VerifySSHPodman verifies the same rootless Podman preconditions on an SSH
// host.
//
// The probes run through the noninteractive SSH options, so an unreachable
// host fails fast instead of blocking doctor or session preflight.
func VerifySSHPodman(ssh *core.SshTarget, executor core.CommandExecutor) (PodmanPreflight, error) {
	panic("unported: mj-targets/src/targets/preflight.rs verify_ssh_podman")
}

// PodmanPostcondition is one rootless Podman postcondition, with the wording
// used to report it.
type PodmanPostcondition int

const (
	PodmanPostconditionVersion PodmanPostcondition = iota
	PodmanPostconditionRootless
	PodmanPostconditionRemote
	PodmanPostconditionInfo
	PodmanPostconditionUIDMap
)

// PodmanProbe is one Podman command whose result is checked.
//
// `podman info` works through a remote client, unlike `podman unshare`. That
// matters on macOS, where Podman always talks to a `podman machine` VM and
// the client is remote by design.
type PodmanProbe int

const (
	PodmanProbeVersion PodmanProbe = iota
	PodmanProbeInfo
)

// PodmanProbeFailure is a rootless Podman postcondition that was not met,
// carrying which one.
//
// `mj doctor` needs the postcondition, not its wording, to name the fix.
// Carrying it on the error means the diagnosis never depends on matching
// message text that this repository itself produces.
type PodmanProbeFailure struct {
	postcondition PodmanPostcondition
	// observation is what was observed, without the fix.
	observation string
	// message is the observation followed by the fix and the guide link.
	message string
}

func (f *PodmanProbeFailure) Error() string {
	return f.message
}

// FailedPodmanPostcondition is the postcondition that failed, when the error
// came from a probe.
func FailedPodmanPostcondition(err error) (PodmanPostcondition, bool) {
	var failure *PodmanProbeFailure
	if errors.As(err, &failure) {
		return failure.postcondition, true
	}
	return 0, false
}

// PodmanProbeObservation is what a failed probe observed, without the fix,
// so a report that prints the fix separately does not repeat it.
func PodmanProbeObservation(err error) (string, bool) {
	var failure *PodmanProbeFailure
	if errors.As(err, &failure) {
		return failure.observation, true
	}
	return "", false
}

// probeFailure fails a postcondition with what was observed, followed by its
// fix and the published guide, keeping the postcondition machine-readable.
func probeFailure(host podmanHost, postcondition PodmanPostcondition, observation string) error {
	message := fmt.Sprintf("%s %s%s See %s.", observation, host.remediationScope(), postcondition.Remediation(), PodmanDocumentationURL)
	return &PodmanProbeFailure{postcondition: postcondition, observation: observation, message: message}
}

// Key is the name of this probe in the batched remote script's output.
func (p PodmanProbe) Key() string {
	switch p {
	case PodmanProbeVersion:
		return "version"
	default:
		return "info"
	}
}

// Args is the probe's command line.
func (p PodmanProbe) Args() []string {
	switch p {
	case PodmanProbeVersion:
		return []string{"podman", "--version"}
	default:
		return []string{"podman", "info", "--format", "json"}
	}
}

// Purpose is the probe command's purpose.
func (p PodmanProbe) Purpose() string {
	switch p {
	case PodmanProbeVersion:
		return "check Podman version"
	default:
		return "check rootless Podman mode and UID map"
	}
}

func (p PodmanProbe) postcondition() PodmanPostcondition {
	switch p {
	case PodmanProbeVersion:
		return PodmanPostconditionVersion
	default:
		return PodmanPostconditionInfo
	}
}

// checks is what running this probe checks, in words that follow "to
// check".
func (p PodmanProbe) checks() string {
	switch p {
	case PodmanProbeVersion:
		return "that Podman 4.0.0 or newer is installed"
	default:
		return "that Podman is rootless and maps container UIDs 0 and 1"
	}
}

func (c PodmanPostcondition) statement() string {
	switch c {
	case PodmanPostconditionVersion:
		return "Postcondition `podman --version` succeeds with Podman 4.0.0 or newer"
	case PodmanPostconditionRootless:
		return "Postcondition Podman is rootless"
	case PodmanPostconditionRemote:
		return "Postcondition Podman uses a local engine"
	case PodmanPostconditionInfo:
		return "Postcondition `podman info` succeeds"
	default:
		return "Postcondition `podman info` maps container UIDs 0 and 1"
	}
}

// Remediation is the fix for this postcondition.
func (c PodmanPostcondition) Remediation() string {
	switch c {
	case PodmanPostconditionVersion:
		return "Install or upgrade Podman: on macOS `brew install podman` then `podman machine init --now`; Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`."
	case PodmanPostconditionRootless:
		return "Run Mjolnir as the ordinary user without `sudo`, and use a rootless Podman. On macOS run `podman machine set --rootful=false` and restart the machine; on Linux do not run Podman as root and unset `CONTAINER_HOST` if a remote connection is selected."
	case PodmanPostconditionRemote:
		return "Unset `CONTAINER_HOST` and `CONTAINER_CONNECTION`, then select the local rootless Podman connection. On macOS the Podman machine connection is the supported local client; a manually configured remote connection is not."
	case PodmanPostconditionInfo:
		return "Make sure `podman info` succeeds as the user running Mjolnir. On macOS start the Podman machine with `podman machine start`, creating it with `podman machine init --now` if none exists; on Linux install the UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora) if Podman reports one is missing."
	default:
		return "Install UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora), then add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"` and start a fresh login session. On macOS, update Podman and recreate the machine if its rootless UID map does not cover container IDs 0 and 1."
	}
}

func verifyPodman(host podmanHost, executor core.CommandExecutor) (PodmanPreflight, error) {
	return verifyPodmanProbes(host, runtime.GOOS == "darwin", func(probe PodmanProbe) (core.CommandOutput, error) {
		return executePodmanProbe(executor, host, probe)
	})
}

// verifyPodmanProbes applies the rootless Podman postconditions to probe
// results, however they were obtained: one command each locally, one
// batched command over SSH.
//
// macosHost says the host running `podman` is a Mac, whose engine is always
// its Podman machine.
func verifyPodmanProbes(host podmanHost, macosHost bool, probeOutput func(PodmanProbe) (core.CommandOutput, error)) (PodmanPreflight, error) {
	panic("unported: mj-targets/src/targets/preflight.rs verify_podman_probes")
}

// CgroupLimit is a session container limit that rootless Podman applies
// through a cgroup v2 controller, which systemd must delegate to the user's
// session.
type CgroupLimit int

const (
	CgroupLimitCPU CgroupLimit = iota
	CgroupLimitMemory
)

// CgroupLimitAll is every limit, in declaration order.
var CgroupLimitAll = [2]CgroupLimit{CgroupLimitCPU, CgroupLimitMemory}

func (l CgroupLimit) controller() string {
	switch l {
	case CgroupLimitCPU:
		return "cpu"
	default:
		return "memory"
	}
}

// RunFlagPrefix is the `podman run` flag, as `backend_container` writes it,
// `--cpus=4`.
func (l CgroupLimit) RunFlagPrefix() string {
	switch l {
	case CgroupLimitCPU:
		return "--cpus="
	default:
		return "--memory="
	}
}

// CgroupDelegationRemediation: openSUSE and SLE ship `Delegate=` for
// `user@.service`, so their rootless Podman runs with only the pids
// controller. runc then fails every container that sets `--cpus` or
// `--memory`, naming a missing `cpu.max` or `memory.swap.max`.
const CgroupDelegationRemediation = "To enforce them, have systemd delegate the controllers to user sessions: create `/etc/systemd/system/user@.service.d/80-delegate.conf` containing `[Service]` and `Delegate=pids memory cpu`, run `sudo systemctl daemon-reload`, then log in again."

// UnenforceableLimitsDetail says which session limits rootless Podman cannot
// enforce, and why.
func UnenforceableLimitsDetail(limits []CgroupLimit) string {
	names := make([]string, len(limits))
	for i, limit := range limits {
		names[i] = limit.controller()
	}
	controllers := strings.Join(names, " and ")
	noun := "controllers"
	if len(limits) == 1 {
		noun = "controller"
	}
	return fmt.Sprintf("Rootless Podman cannot enforce session %s limits because systemd does not delegate the %s cgroup %s to this user, so sessions start without them.", controllers, controllers, noun)
}

// unenforceableLimits is the limits Podman cannot enforce, read from the
// `host` object of `podman info` (decoded JSON). Only cgroup v2 is judged:
// rootless Podman on v1 already ignores resource limits itself, and an
// absent controller list is unknown, not empty.
func unenforceableLimits(host any) []CgroupLimit {
	object, _ := host.(map[string]any)
	if version, _ := object["cgroupVersion"].(string); version != "v2" {
		return nil
	}
	controllers, ok := object["cgroupControllers"].([]any)
	if !ok {
		return nil
	}
	var limits []CgroupLimit
	for _, limit := range CgroupLimitAll {
		delegated := false
		for _, controller := range controllers {
			if name, ok := controller.(string); ok && name == limit.controller() {
				delegated = true
				break
			}
		}
		if !delegated {
			limits = append(limits, limit)
		}
	}
	return limits
}

// ProbePodmanUnenforceableLimits asks the Podman that will run a container
// which limits it cannot enforce. A nil ssh asks this host's Podman.
func ProbePodmanUnenforceableLimits(ssh *core.SshTarget, executor core.CommandExecutor) ([]CgroupLimit, error) {
	command := podmanHost{ssh: ssh}.command([]string{"podman", "info", "--format", "{{json .Host}}"}, "check Podman cgroup controllers")
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("`podman info` failed with status %d: %s", output.Status, strings.TrimSpace(preflightLossy(output.Stderr)))
	}
	var host any
	if err := json.Unmarshal(output.Stdout, &host); err != nil {
		return nil, fmt.Errorf("`podman info --format '{{json .Host}}'` returned unparsable output: %w", err)
	}
	return unenforceableLimits(host), nil
}

// podmanInfo is the parts of `podman info` the preflight checks.
type podmanInfo struct {
	rootless        bool
	serviceIsRemote bool
	// uidMap holds (container_id, host_id, size) triples from
	// `host.idMappings.uidmap`.
	uidMap              [][3]uint64
	unenforceableLimits []CgroupLimit
}

func (i *podmanInfo) coversUID(containerID uint64) bool {
	for _, mapping := range i.uidMap {
		start, size := mapping[0], mapping[2]
		end := start + size
		if end < start {
			// Rust's checked_add: an overflowing range covers nothing.
			continue
		}
		if start <= containerID && containerID < end {
			return true
		}
	}
	return false
}

func (i *podmanInfo) uidMapDescription() string {
	if len(i.uidMap) == 0 {
		return "no UID mappings"
	}
	parts := make([]string, len(i.uidMap))
	for k, mapping := range i.uidMap {
		parts[k] = fmt.Sprintf("%d:%d:%d", mapping[0], mapping[1], mapping[2])
	}
	return strings.Join(parts, ", ")
}

// parsePodmanInfo reads rootless mode and the UID map from
// `podman info --format json`.
//
// `podman info` reports the server's view, so it works for local Rootless
// Podman, a remote Podman machine, and an SSH host alike.
func parsePodmanInfo(host podmanHost, stdout []byte) (podmanInfo, error) {
	panic("unported: mj-targets/src/targets/preflight.rs parse_podman_info")
}

func podmanInfoFailure(host podmanHost, observation string) error {
	return probeFailure(host, PodmanPostconditionInfo, fmt.Sprintf("%s: `podman info` could not be used: %s.", host.failure(), observation))
}

// sshPodmanLingerWarning reports either an explicitly unsafe systemd setting
// or an unavailable durability check. Neither condition makes an otherwise
// usable target fail. A nil output means the probe's result was missing.
func sshPodmanLingerWarning(ssh *core.SshTarget, output *core.CommandOutput) *PodmanPreflightWarning {
	panic("unported: mj-targets/src/targets/preflight.rs ssh_podman_linger_warning")
}

func lingerUnavailableWarning(ssh *core.SshTarget, reason string) PodmanPreflightWarning {
	return PodmanPreflightWarning{
		Detail:      fmt.Sprintf("Remote user-manager durability check is unavailable on %s because %s. Mjolnir cannot verify whether rootless Podman sessions survive logout.", ssh.Destination, reason),
		Remediation: fmt.Sprintf("Configure %s's service manager to keep the user and rootless Podman services running after logout; if it uses systemd, make `loginctl` available and enable lingering.", ssh.Destination),
	}
}

func executePodmanProbe(executor core.CommandExecutor, host podmanHost, probe PodmanProbe) (core.CommandOutput, error) {
	command := host.command(probe.Args(), probe.Purpose())
	output, err := executor.Execute(&command)
	if err != nil {
		return core.CommandOutput{}, podmanProbeRunFailure(host, probe, probeRunReason(err))
	}
	return checkPodmanProbeStatus(host, probe, output)
}

// probeRunReason is why a probe command could not be started: a missing
// `podman` in plain words, else the whole error chain, whose outer layer
// ("run podman for check Podman version") says nothing on its own.
func probeRunReason(err error) string {
	if isMissingProgram(err) {
		return "`podman` is not installed or not on PATH"
	}
	return err.Error()
}

// podmanProbeRunFailure is the failure for a probe that could not be run at
// all.
func podmanProbeRunFailure(host podmanHost, probe PodmanProbe, reported string) error {
	if message, ok := sshTransportFailure(host, reported); ok {
		return errors.New(message)
	}
	return probeFailure(host, probe.postcondition(), fmt.Sprintf("%s: could not run `%s` to check %s: %s.", host.failure(), strings.Join(probe.Args(), " "), probe.checks(), reported))
}

func checkPodmanProbeStatus(host podmanHost, probe PodmanProbe, output core.CommandOutput) (core.CommandOutput, error) {
	panic("unported: mj-targets/src/targets/preflight.rs check_podman_probe_status")
}

const (
	lingerProbeKey    = "linger"
	platformProbeKey  = "platform"
	probeBlockBegin   = "__mj_probe_begin__"
	probeBlockEnd     = "__mj_probe_end__"
	probeStatusPrefix = "__mj_probe_status__"
)

// ProbeFrames is the markers a batched probe script frames each result
// with. A script that runs another batched script as one of its probes uses
// different markers, so the inner script's frames stay opaque inside the
// outer one's.
type ProbeFrames struct {
	Begin  string
	End    string
	Status string
}

var podmanProbeFrames = ProbeFrames{
	Begin:  probeBlockBegin,
	End:    probeBlockEnd,
	Status: probeStatusPrefix,
}

// SSHPodmanPreflightScript runs every remote Podman probe in one SSH round
// trip.
//
// Each probe's stdout is captured in a shell variable and reprinted between
// framing markers, while its stderr is written straight to the saved stdout
// inside its own frame, so multi-line and arbitrary output survives intact.
// The version probe short-circuits the rest: without Podman the later probes
// can only repeat its failure.
const SSHPodmanPreflightScript = `
exec 3>&1
probe() {
    name=$1
    shift
    printf '__mj_probe_begin__ %s.stderr\n' "$name"
    out=$("$@" 2>&3)
    status=$?
    printf '\n__mj_probe_end__\n'
    printf '__mj_probe_begin__ %s.stdout\n%s\n__mj_probe_end__\n' "$name" "$out"
    printf '__mj_probe_status__ %s %s\n' "$name" "$status"
    return "$status"
}
probe version podman --version || exit 0
probe platform uname -s
probe info podman info --format json
probe linger sh -c 'loginctl show-user "$(id -u)" --property=Linger --value'
exit 0
`

func runSSHPodmanProbes(host podmanHost, executor core.CommandExecutor) (map[string]core.CommandOutput, error) {
	panic("unported: mj-targets/src/targets/preflight.rs run_ssh_podman_probes")
}

// ProbeFixture is one probe result for SSHPodmanProbeFixture (Rust passes a
// `(name, status, stdout, stderr)` tuple).
type ProbeFixture struct {
	Name   string
	Status int
	Stdout string
	Stderr string
}

// SSHPodmanProbeFixture builds what the batched remote script prints for
// the given probe results.
//
// Tests across this crate stand in for a remote host, so they need the real
// framing rather than a second, drifting description of it.
func SSHPodmanProbeFixture(probes []ProbeFixture) []byte {
	var output strings.Builder
	for _, probe := range probes {
		fmt.Fprintf(&output, "%s %s.stderr\n", probeBlockBegin, probe.Name)
		output.WriteString(probe.Stderr)
		fmt.Fprintf(&output, "\n%s\n", probeBlockEnd)
		fmt.Fprintf(&output, "%s %s.stdout\n", probeBlockBegin, probe.Name)
		output.WriteString(strings.TrimSuffix(probe.Stdout, "\n"))
		fmt.Fprintf(&output, "\n%s\n", probeBlockEnd)
		fmt.Fprintf(&output, "%s %s %d\n", probeStatusPrefix, probe.Name, probe.Status)
	}
	return []byte(output.String())
}

// parsePodmanProbeOutput splits the batched script's framed output into one
// result per probe.
//
// A probe appears only once its status line has been read, so output
// truncated mid-probe is reported as a missing probe rather than a partial
// result.
func parsePodmanProbeOutput(stdout []byte) map[string]core.CommandOutput {
	return ParseFramedProbes(stdout, podmanProbeFrames)
}

// ParseFramedProbes splits framed output into one result per probe, as
// parsePodmanProbeOutput does for frames.
func ParseFramedProbes(stdout []byte, frames ProbeFrames) map[string]core.CommandOutput {
	panic("unported: mj-targets/src/targets/preflight.rs parse_framed_probes")
}

func sshTransportFailure(host podmanHost, reported string) (string, bool) {
	if host.ssh == nil {
		return "", false
	}
	destination := host.ssh.Destination
	return fmt.Sprintf("%s: SSH could not run the probes on %s. Verify that `ssh %s` succeeds noninteractively from this host. See %s. ssh reported: %s",
		host.failure(), destination, destination, PodmanDocumentationURL, reported), true
}

func parsePodmanVersion(host podmanHost, stdout []byte) (string, error) {
	panic("unported: mj-targets/src/targets/preflight.rs parse_podman_version")
}

// filesystemTypesScript prints the filesystem type of each argument, one per
// line. macOS `stat` cannot name one, so there each `mount` entry,
// `<device> on <point> (<type>, ...)`, is matched against the end of the
// path's `df` line: the longest point it ends with holds the path, and among
// entries for that point the last, the one on top, names the type. Both
// device names and mount points can contain spaces and `%`, so no `df`
// column is parsed. That match is a function rather than inline in `$(...)`:
// macOS `/bin/sh` is bash 3.2, which ends a command substitution at the
// first `)`, so it cannot parse a `case` pattern inside one.
const filesystemTypesScript = `if [ "$(uname -s)" != Darwin ]; then
    exec stat -f -c %T -- "$@"
fi
mounted_type() {
    best= found=
    while IFS= read -r line; do
        head=${line% (*}
        [ "$head" != "$line" ] || continue
        case $head in *" on /"*) ;; *) continue ;; esac
        point=/${head#* on /}
        case $1 in *" $point") ;; *) continue ;; esac
        [ "${#point}" -ge "${#best}" ] || continue
        options=${line#"$head ("}
        best=$point found=${options%%[,)]*}
    done
    printf '%s' "$found"
}
mounts=$(mount) || exit 1
for path do
    usage=$(df -P -- "$path" | sed -n 2p)
    [ -n "$usage" ] || { printf 'df reported no filesystem for %s\n' "$path" >&2; exit 1; }
    filesystem=$(printf '%s\n' "$mounts" | mounted_type "$usage")
    [ -n "$filesystem" ] || { printf 'mount lists no filesystem holding %s\n' "$path" >&2; exit 1; }
    printf '%s\n' "$filesystem"
done`

// ProbeFilesystemTypes is the filesystem type of each directory, probed on
// the host that owns it: the one that runs the container engine, or a bare
// session's own machine. ssh names that host; nil probes this machine.
//
// The reply is positional, so the whole batch fails unless the host answered
// for every directory in order.
func ProbeFilesystemTypes(ssh *core.SshTarget, paths []string, executor core.CommandExecutor) ([]string, error) {
	panic("unported: mj-targets/src/targets/preflight.rs probe_filesystem_types")
}

// preflightLossy is Rust's String::from_utf8_lossy.
func preflightLossy(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}
