// Port of mj-targets/src/targets/preflight.rs.

package targets

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

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
	// A launch can own this gate through the unbounded ProcessExecutor used by
	// the options API. Daemon shutdown joins the cancellable startup and image
	// refresh callers, so they must be able to leave while waiting for it.
	backoff := time.Millisecond
	for {
		if executor.CancellationRequested() {
			return 0, errors.New("operation cancelled while waiting to check Apple container runtime")
		}
		if appleContainerEnsure.TryLock() {
			break
		}
		time.Sleep(backoff)
		backoff = min(backoff*2, 50*time.Millisecond)
	}
	defer appleContainerEnsure.Unlock()
	if executor.CancellationRequested() {
		return 0, errors.New("operation cancelled while checking Apple container runtime")
	}

	status, err := AppleContainerRuntimeStatus(executor)
	if err != nil {
		return 0, err
	}
	if status.Kind == AppleContainerStatusRunning {
		return AppleContainerRuntimeRunning, nil
	}

	start := core.NewCommandSpec("container", []string{"system", "start", "--disable-kernel-install"}).
		WithPurpose("start Apple container runtime").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	output, err := executor.Execute(&start)
	if err != nil {
		return 0, err
	}
	if output.Status != 0 {
		return 0, fmt.Errorf("container system start failed with status %d: %s", output.Status, appleContainerOutput(&output))
	}

	status, err = AppleContainerRuntimeStatus(executor)
	if err != nil {
		return 0, err
	}
	if status.Kind == AppleContainerStatusRunning {
		return AppleContainerRuntimeStarted, nil
	}
	return 0, fmt.Errorf("container system status still failed after start with status %d: %s", status.Status, status.Detail)
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
	if err := core.ValidateSSH(ssh); err != nil {
		return AppleContainerPreflight{}, err
	}
	destination := ssh.Destination
	probe := func(args [2]string, purpose string) core.CommandSpec {
		return core.SSHValidationCommand(ssh, []string{args[0], args[1]}, purpose).
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	}
	reported := func(command core.CommandSpec) (string, error) {
		output, err := executor.Execute(&command)
		if err != nil {
			return "", fmt.Errorf("Apple container preflight on %s could not run `%s`: %w", destination, command.Purpose, err)
		}
		if output.Status != 0 {
			return "", fmt.Errorf("Apple container preflight failed on %s: `%s` exited with status %d: %s",
				destination, command.Purpose, output.Status, strings.TrimSpace(preflightLossy(output.Stderr)))
		}
		return strings.TrimSpace(preflightLossy(output.Stdout)), nil
	}

	uname, err := reported(probe(PreflightPlatformProbe, "detect the Apple container host platform"))
	if err != nil {
		return AppleContainerPreflight{}, err
	}
	platform, err := TargetPlatformParse(uname)
	if err != nil {
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight failed on %s: the host reported %s: %w",
			destination, core.RustDebugString(uname), err)
	}
	if platform.Os != TargetOsDarwin {
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight failed on %s: Apple container runs only on macOS, and this host reports %s.",
			destination, core.RustDebugString(uname))
	}
	if platform.Architecture != "aarch64" {
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight failed on %s: Apple container requires Apple silicon, and this host reports %s.",
			destination, core.RustDebugString(uname))
	}

	macosVersion, err := reported(probe(AppleContainerMacosProbe, "detect the remote macOS release"))
	if err != nil {
		return AppleContainerPreflight{}, err
	}
	majorText, _, _ := strings.Cut(macosVersion, ".")
	major, ok := preflightParseU32(majorText)
	if !ok {
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight failed on %s: `sw_vers -productVersion` printed %s",
			destination, core.RustDebugString(macosVersion))
	}
	if major < AppleContainerMinimumMacos {
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight failed on %s: Apple container requires macOS %d or newer, and this host runs %s.",
			destination, AppleContainerMinimumMacos, macosVersion)
	}

	version, err := reported(probe(AppleContainerVersionProbe, "check the remote Apple container installation"))
	if err != nil {
		return AppleContainerPreflight{}, err
	}
	// `container system status` exits nonzero while the daemon is stopped, so
	// it is checked for its status rather than its words.
	status := core.SSHValidationCommand(ssh, AppleContainerStatusProbe[:], "check the remote Apple container daemon").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	output, err := executor.Execute(&status)
	if err != nil {
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight on %s could not query the daemon: %w", destination, err)
	}
	if output.Status != 0 {
		var messages []string
		for _, message := range []string{strings.TrimSpace(preflightLossy(output.Stdout)), strings.TrimSpace(preflightLossy(output.Stderr))} {
			if message != "" {
				messages = append(messages, message)
			}
		}
		return AppleContainerPreflight{}, fmt.Errorf("Apple container preflight failed on %s: the Apple container daemon is not running (%s). Start it there with `container system start`.",
			destination, strings.Join(messages, " "))
	}
	return AppleContainerPreflight{MacosVersion: macosVersion, Version: version}, nil
}

// preflightParseU32 is Rust's `str::parse::<u32>`: decimal digits with an
// optional leading `+`.
func preflightParseU32(text string) (uint32, bool) {
	text = strings.TrimPrefix(text, "+")
	if text == "" || strings.HasPrefix(text, "+") || strings.HasPrefix(text, "-") {
		return 0, false
	}
	value, err := strconv.ParseUint(text, 10, 32)
	if err != nil {
		return 0, false
	}
	return uint32(value), true
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
	// source is the error this reason was attached to as context, the way
	// anyhow's `.context(DockerUnavailable::NotInstalled)` keeps the failed
	// spawn on the chain for callers that classify it. Never shown.
	source error
}

// Unwrap is the error the reason was attached to, if any.
func (d DockerUnavailable) Unwrap() error { return d.source }

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
	command := core.NewCommandSpec(DockerServerVersion[0], DockerServerVersion[1:]).
		WithPurpose("check Docker daemon").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	if ssh != nil {
		command = commandOverSSH(core.SshTrafficClassControl, command, ssh)
	}
	output, err := executor.Execute(&command)
	if err != nil {
		// Over SSH a missing program would be `ssh` itself, which the SSH
		// checks report; only a local one means Docker is not installed.
		// The cause stays in the chain for callers that classify it.
		if ssh == nil && isMissingProgram(err) {
			return DockerPreflight{}, DockerUnavailable{Kind: DockerUnavailableNotInstalled, source: err}
		}
		return DockerPreflight{}, fmt.Errorf("Docker preflight failed: run `docker info` as the user running Mjolnir: %w", err)
	}
	if output.Status != 0 {
		reported := strings.TrimSpace(preflightLossy(output.Stderr))
		if ssh == nil {
			if dockerDaemonNotRunning(reported) {
				return DockerPreflight{}, DockerUnavailable{Kind: DockerUnavailableNotRunning, Reported: reported}
			}
			return DockerPreflight{}, DockerUnavailable{Kind: DockerUnavailableNotAnswering, Status: output.Status, Reported: reported}
		}
		return DockerPreflight{}, fmt.Errorf("Docker preflight failed: `docker version` exited with status %d: %s. Run `docker info` as the user running Mjolnir. See %s.",
			output.Status, reported, DockerDocumentationURL)
	}
	reported := preflightLossy(output.Stdout)
	fields := strings.Fields(reported)
	var version, os string
	if len(fields) > 0 {
		version = fields[0]
	}
	if len(fields) > 1 {
		os = fields[1]
	}
	if version == "" || os != "linux" {
		return DockerPreflight{}, fmt.Errorf("Docker preflight failed: expected a Linux Docker daemon, got %s. See %s.",
			core.RustDebugString(strings.TrimSpace(reported)), DockerDocumentationURL)
	}
	return DockerPreflight{Version: version}, nil
}

// VerifySSHPodman verifies the same rootless Podman preconditions on an SSH
// host.
//
// The probes run through the noninteractive SSH options, so an unreachable
// host fails fast instead of blocking doctor or session preflight.
func VerifySSHPodman(ssh *core.SshTarget, executor core.CommandExecutor) (PodmanPreflight, error) {
	host := podmanHost{ssh: ssh}
	if err := core.ValidateSSH(ssh); err != nil {
		return PodmanPreflight{}, fmt.Errorf("%s: the configured SSH destination is unusable (%s). Set a valid `host` (and optional `user`) for this ssh-podman target. See %s.",
			host.failure(), err, PodmanDocumentationURL)
	}
	// One SSH round trip carries every probe; a remote shell runs them in
	// sequence and frames each result so the checks below stay unchanged.
	probes, err := runSSHPodmanProbes(host, executor)
	if err != nil {
		return PodmanPreflight{}, err
	}
	platform, ok := probes[platformProbeKey]
	macosHost := ok && platform.Status == 0 && string(preflightTrimASCII(platform.Stdout)) == "Darwin"
	preflight, err := verifyPodmanProbes(host, macosHost, func(probe PodmanProbe) (core.CommandOutput, error) {
		output, ok := probes[probe.Key()]
		if !ok {
			message, _ := sshTransportFailure(host, fmt.Sprintf("the preflight output ended before the %s probe", probe.Key()))
			return core.CommandOutput{}, errors.New(message)
		}
		return checkPodmanProbeStatus(host, probe, output)
	})
	if err != nil {
		return PodmanPreflight{}, err
	}
	// Lingering is systemd's: a Mac has no `loginctl`, and its engine is the
	// Podman machine rather than a user service.
	if !macosHost {
		var linger *core.CommandOutput
		if output, ok := probes[lingerProbeKey]; ok {
			linger = &output
		}
		if warning := sshPodmanLingerWarning(ssh, linger); warning != nil {
			preflight.Warnings = append(preflight.Warnings, *warning)
		}
	}
	return preflight, nil
}

// preflightTrimASCII is Rust's `<[u8]>::trim_ascii`.
func preflightTrimASCII(data []byte) []byte {
	isSpace := func(b byte) bool { return b == ' ' || b == '\t' || b == '\n' || b == '\f' || b == '\r' }
	for len(data) > 0 && isSpace(data[0]) {
		data = data[1:]
	}
	for len(data) > 0 && isSpace(data[len(data)-1]) {
		data = data[:len(data)-1]
	}
	return data
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
	versionOutput, err := probeOutput(PodmanProbeVersion)
	if err != nil {
		return PodmanPreflight{}, err
	}
	version, err := parsePodmanVersion(host, versionOutput.Stdout)
	if err != nil {
		return PodmanPreflight{}, err
	}

	infoOutput, err := probeOutput(PodmanProbeInfo)
	if err != nil {
		return PodmanPreflight{}, err
	}
	info, err := parsePodmanInfo(host, infoOutput.Stdout)
	if err != nil {
		return PodmanPreflight{}, err
	}

	if !info.rootless {
		return PodmanPreflight{}, probeFailure(host, PodmanPostconditionRootless, fmt.Sprintf(
			"%s: %s was not met. `podman info` reports rootful mode.",
			host.failure(), PodmanPostconditionRootless.statement()))
	}

	// A remote client is expected only on a Mac, whose Podman machine is
	// that host's engine whether Mjolnir runs there or reaches it over SSH.
	// On Linux, `serviceIsRemote` means CONTAINER_HOST or a selected
	// connection points this target at another host's engine, whose paths and
	// containers are not the ones Mjolnir records.
	if info.serviceIsRemote && !macosHost {
		return PodmanPreflight{}, probeFailure(host, PodmanPostconditionRemote, fmt.Sprintf(
			"%s: %s was not met. `podman info` reports serviceIsRemote=true.",
			host.failure(), PodmanPostconditionRemote.statement()))
	}

	for _, containerID := range []uint64{0, 1} {
		if !info.coversUID(containerID) {
			return PodmanPreflight{}, probeFailure(host, PodmanPostconditionUIDMap, fmt.Sprintf(
				"%s: %s was not met. `podman info` reports UID map %s.",
				host.failure(), PodmanPostconditionUIDMap.statement(), info.uidMapDescription()))
		}
	}

	var warnings []PodmanPreflightWarning
	if len(info.unenforceableLimits) != 0 {
		warnings = append(warnings, PodmanPreflightWarning{
			Detail:      UnenforceableLimitsDetail(info.unenforceableLimits),
			Remediation: host.remediationScope() + CgroupDelegationRemediation,
		})
	}
	return PodmanPreflight{Version: version, Warnings: warnings}, nil
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
	value, err := preflightDecodeJSON(stdout)
	if err != nil {
		return podmanInfo{}, probeFailure(host, PodmanPostconditionInfo, fmt.Sprintf(
			"%s: `podman info --format json` returned unparsable output: %s.", host.failure(), err))
	}
	rootless, ok := preflightPointer(value, "host", "security", "rootless").(bool)
	if !ok {
		return podmanInfo{}, podmanInfoFailure(host, "it did not report rootless mode")
	}
	serviceIsRemote, _ := preflightPointer(value, "host", "serviceIsRemote").(bool)
	entries, ok := preflightPointer(value, "host", "idMappings", "uidmap").([]any)
	if !ok {
		return podmanInfo{}, podmanInfoFailure(host, "it did not report a UID map")
	}
	uidMap := make([][3]uint64, 0, len(entries))
	for _, entry := range entries {
		var mapping [3]uint64
		for i, name := range []string{"container_id", "host_id", "size"} {
			number, ok := preflightU64(preflightPointer(entry, name))
			if !ok {
				return podmanInfo{}, podmanInfoFailure(host, "it reported a UID mapping without "+name)
			}
			mapping[i] = number
		}
		uidMap = append(uidMap, mapping)
	}
	var limits []CgroupLimit
	if object, ok := value.(map[string]any); ok {
		if hostValue, ok := object["host"]; ok {
			limits = unenforceableLimits(hostValue)
		}
	}
	return podmanInfo{
		rootless:            rootless,
		serviceIsRemote:     serviceIsRemote,
		uidMap:              uidMap,
		unenforceableLimits: limits,
	}, nil
}

// preflightDecodeJSON decodes one JSON value as serde_json::Value does,
// keeping integers exact.
func preflightDecodeJSON(data []byte) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil, err
	}
	if _, err := decoder.Token(); err != io.EOF {
		return nil, errors.New("trailing characters after the JSON value")
	}
	return value, nil
}

// preflightPointer is serde_json's `Value::pointer` over object keys: nil
// when any step is missing or not an object.
func preflightPointer(value any, keys ...string) any {
	for _, key := range keys {
		object, ok := value.(map[string]any)
		if !ok {
			return nil
		}
		value = object[key]
	}
	return value
}

// preflightU64 is serde_json's `Value::as_u64`.
func preflightU64(value any) (uint64, bool) {
	number, ok := value.(json.Number)
	if !ok {
		return 0, false
	}
	parsed, err := strconv.ParseUint(number.String(), 10, 64)
	return parsed, err == nil
}

func podmanInfoFailure(host podmanHost, observation string) error {
	return probeFailure(host, PodmanPostconditionInfo, fmt.Sprintf("%s: `podman info` could not be used: %s.", host.failure(), observation))
}

// sshPodmanLingerWarning reports either an explicitly unsafe systemd setting
// or an unavailable durability check. Neither condition makes an otherwise
// usable target fail. A nil output means the probe's result was missing.
func sshPodmanLingerWarning(ssh *core.SshTarget, output *core.CommandOutput) *PodmanPreflightWarning {
	if output == nil {
		warning := lingerUnavailableWarning(ssh, "the probe could not run: the preflight output did not include it")
		return &warning
	}
	linger := preflightLossy(output.Stdout)
	value := preflightASCIILower(strings.TrimSpace(linger))
	switch {
	case output.Status == 0 && value == "yes":
		return nil
	case output.Status == 0 && value == "no":
		return &PodmanPreflightWarning{
			Detail:      fmt.Sprintf("Remote user lingering is disabled on %s; SSH-Podman sessions may be terminated when the last SSH connection closes.", ssh.Destination),
			Remediation: fmt.Sprintf("On %s, run `sudo loginctl enable-linger \"$(id -un)\"`.", ssh.Destination),
		}
	}
	status := output.Status
	stderr := strings.TrimSpace(preflightLossy(output.Stderr))
	var reason string
	switch {
	case status == 127 || strings.Contains(stderr, "loginctl: not found"):
		reason = "`loginctl` was not found; this host may not use systemd"
	case status != 0:
		reason = fmt.Sprintf("`loginctl` exited with status %d: %s", status, stderr)
	default:
		reason = fmt.Sprintf("`loginctl` returned an unrecognized Linger value %s", core.RustDebugString(linger))
	}
	warning := lingerUnavailableWarning(ssh, reason)
	return &warning
}

// preflightASCIILower is Rust's `str::to_ascii_lowercase`.
func preflightASCIILower(text string) string {
	lowered := []byte(text)
	for i, b := range lowered {
		if 'A' <= b && b <= 'Z' {
			lowered[i] = b + ('a' - 'A')
		}
	}
	return string(lowered)
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
	// `ssh` reserves this status for its own connection failures; the Podman
	// probes never produce it. Reporting that case separately keeps an
	// unreachable host from being mistaken for a broken Podman installation.
	if output.Status == core.SSHTransportExitStatus {
		if message, ok := sshTransportFailure(host, strings.TrimSpace(preflightLossy(output.Stderr))); ok {
			return core.CommandOutput{}, errors.New(message)
		}
	}
	if output.Status != 0 {
		stderr := strings.TrimSpace(preflightLossy(output.Stderr))
		postcondition := probe.postcondition()
		// Podman's message is usually a sentence without a final period.
		// End it before the remediation starts, so the fix does not read as
		// part of the command output.
		reported := stderr + "."
		if strings.HasSuffix(stderr, ".") || strings.HasSuffix(stderr, "!") || strings.HasSuffix(stderr, "?") {
			reported = stderr
		}
		return core.CommandOutput{}, probeFailure(host, postcondition, fmt.Sprintf(
			"%s: %s failed. Podman reported: %s", host.failure(), postcondition.statement(), reported))
	}
	return output, nil
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
	command := host.command([]string{"sh", "-c", SSHPodmanPreflightScript}, "check remote Podman prerequisites")
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, podmanProbeRunFailure(host, PodmanProbeVersion, err.Error())
	}
	if output.Status == core.SSHTransportExitStatus {
		if message, ok := sshTransportFailure(host, strings.TrimSpace(preflightLossy(output.Stderr))); ok {
			return nil, errors.New(message)
		}
	}
	probes := parsePodmanProbeOutput(output.Stdout)
	if _, ok := probes[PodmanProbeVersion.Key()]; !ok {
		return nil, podmanProbeRunFailure(host, PodmanProbeVersion, fmt.Sprintf(
			"the preflight probes returned unparsable output (status %d): %s",
			output.Status, strings.TrimSpace(preflightLossy(output.Stderr))))
	}
	return probes, nil
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
	lines := preflightLines(preflightLossy(stdout))
	blocks := map[string]string{}
	probes := map[string]core.CommandOutput{}
	for index := 0; index < len(lines); index++ {
		line := lines[index]
		if rest, ok := strings.CutPrefix(line, frames.Begin); ok {
			if name, ok := strings.CutPrefix(rest, " "); ok && name != "" {
				var body []string
				closed := false
				for index++; index < len(lines); index++ {
					if lines[index] == frames.End {
						closed = true
						break
					}
					body = append(body, lines[index])
				}
				if closed {
					blocks[name] = strings.Join(body, "\n")
				}
				continue
			}
		}
		rest, ok := strings.CutPrefix(line, frames.Status)
		if !ok {
			continue
		}
		fields := strings.Fields(rest)
		if len(fields) < 2 {
			continue
		}
		name := fields[0]
		status, err := strconv.ParseInt(fields[1], 10, 32)
		out, outOK := blocks[name+".stdout"]
		if outOK {
			delete(blocks, name+".stdout")
		}
		errText, errOK := blocks[name+".stderr"]
		if errOK {
			delete(blocks, name+".stderr")
		}
		if err != nil || !outOK || !errOK {
			continue
		}
		probes[name] = core.CommandOutput{Status: int(status), Stdout: []byte(out), Stderr: []byte(errText)}
	}
	return probes
}

// preflightLines is Rust's `str::lines`: lines end at `\n` or `\r\n`, and a
// final line ending adds no empty line.
func preflightLines(text string) []string {
	var lines []string
	for text != "" {
		line := text
		next := ""
		if index := strings.IndexByte(text, '\n'); index >= 0 {
			line = text[:index]
			next = text[index+1:]
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = next
	}
	return lines
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
	failure := host.failure()
	version := strings.TrimSpace(preflightLossy(stdout))
	unreadable := func() error {
		return probeFailure(host, PodmanPostconditionVersion, fmt.Sprintf(
			"%s: %s returned %s.", failure, PodmanPostconditionVersion.statement(), core.RustDebugString(version)))
	}
	candidate := ""
	for _, part := range strings.Fields(version) {
		if part[0] >= '0' && part[0] <= '9' {
			candidate = part
			break
		}
	}
	if candidate == "" {
		return "", unreadable()
	}
	numbers := strings.Split(candidate, ".")
	major, ok := preflightParseU32(numbers[0])
	if !ok {
		return "", unreadable()
	}
	// A version with no minor component, such as `podman version 4`, names
	// the earliest release of that series.
	var minor uint32
	if len(numbers) > 1 {
		if parsed, ok := preflightParseU32(numbers[1]); ok {
			minor = parsed
		}
	}
	if major < podmanMinimumVersion[0] || (major == podmanMinimumVersion[0] && minor < podmanMinimumVersion[1]) {
		return "", probeFailure(host, PodmanPostconditionVersion, fmt.Sprintf(
			"%s: %s was not met (found %s).", failure, PodmanPostconditionVersion.statement(), candidate))
	}
	return candidate, nil
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
	if len(paths) == 0 {
		return []string{}, nil
	}
	args := []string{"sh", "-c", filesystemTypesScript, "hel-filesystem"}
	args = append(args, paths...)
	command := podmanHost{ssh: ssh}.command(args, "probe mount source filesystem")
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("filesystem probe failed with status %d: %s", output.Status, strings.TrimSpace(preflightLossy(output.Stderr)))
	}
	lines := preflightLines(preflightLossy(output.Stdout))
	types := make([]string, 0, len(lines))
	for _, line := range lines {
		types = append(types, strings.TrimSpace(line))
	}
	if len(types) != len(paths) {
		return nil, fmt.Errorf("filesystem probe named %d filesystems for %d directories", len(types), len(paths))
	}
	return types, nil
}

// preflightLossy is Rust's String::from_utf8_lossy.
func preflightLossy(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}
