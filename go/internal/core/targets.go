// Port of mj-core/src/targets.rs.
//
// Declarative execution plans for Hel session targets.
//
// Plans deliberately contain argv vectors instead of local shell strings. A
// shell is used only at the SSH boundary, where OpenSSH necessarily sends a
// command string; every remotely supplied argument is POSIX-quoted there.

package core

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const SessionLabel = "dev.mj.session"
const ManagedLabel = "dev.mj.managed"
const SessionTag = "dev.mj.session"
const ManagedTag = "dev.mj.managed"

// InstanceLabel records which Mjolnir instance (named `--instance` or
// data-directory fingerprint) created a worker; see InstanceIdentity.
const InstanceLabel = "dev.mj.instance"
const InstanceTag = "dev.mj.instance"

// ContainerWorkspace is the shared in-container workspace every container
// session used before per-session workspaces existed. New sessions record a
// path under it; see ContainerWorkspaceRoot.
const ContainerWorkspace = "/workspace"

// HostRunsWorkers reports whether this machine runs Mjolnir workers itself.
// Windows is a controller only: its sessions run on Linux, in a container or
// on an SSH host, so it has no local bare target and no local worker to probe
// a profile with.
const HostRunsWorkers = runtime.GOOS != "windows"

// ProvisionStageKind names a ProvisionStage variant. The values are the Rust
// variant names, which serde writes for this externally tagged enum.
type ProvisionStageKind string

const (
	// ProvisionStagePullingImage waits for a background download of this
	// target's image to finish, rather than starting a second download of
	// the same image.
	ProvisionStagePullingImage ProvisionStageKind = "PullingImage"
	// ProvisionStageWaitingForImagePull is queued behind another task pulling
	// this image. This is waiting for a turn, not work, so it never counts as
	// a stall.
	ProvisionStageWaitingForImagePull        ProvisionStageKind = "WaitingForImagePull"
	ProvisionStageWaitingForContainerUpgrade ProvisionStageKind = "WaitingForContainerUpgrade"
	// ProvisionStageWaitingForWorkerStore is queued behind another task
	// holding the worker binary store for this target. This is waiting for a
	// turn, not work.
	ProvisionStageWaitingForWorkerStore ProvisionStageKind = "WaitingForWorkerStore"
	// ProvisionStageWaitingForSshConnection is queued behind another task
	// opening the shared SSH connection to this host. This is waiting for a
	// turn, not work.
	ProvisionStageWaitingForSshConnection ProvisionStageKind = "WaitingForSshConnection"
	// ProvisionStageWaitingForStartSlot is queued behind other children
	// starting in the same container. This is waiting for a turn, not work,
	// so it never counts as a stall.
	ProvisionStageWaitingForStartSlot ProvisionStageKind = "WaitingForStartSlot"
	ProvisionStageProvisioning        ProvisionStageKind = "Provisioning"
	ProvisionStageBooting             ProvisionStageKind = "Booting"
	ProvisionStageCloning             ProvisionStageKind = "Cloning"
	ProvisionStageSyncing             ProvisionStageKind = "Syncing"
	ProvisionStageRestoring           ProvisionStageKind = "Restoring"
	ProvisionStageStarting            ProvisionStageKind = "Starting"
	// ProvisionStageInstalling carries Harness.
	ProvisionStageInstalling ProvisionStageKind = "Installing"
	// ProvisionStagePreparingHarness carries Harness, Step and SinceMs.
	ProvisionStagePreparingHarness  ProvisionStageKind = "PreparingHarness"
	ProvisionStageCompacting        ProvisionStageKind = "Compacting"
	ProvisionStageRecoveryCopy      ProvisionStageKind = "RecoveryCopy"
	ProvisionStageVerifying         ProvisionStageKind = "Verifying"
	ProvisionStageClosing           ProvisionStageKind = "Closing"
	ProvisionStageStoppingTarget    ProvisionStageKind = "StoppingTarget"
	ProvisionStageRemovingContainer ProvisionStageKind = "RemovingContainer"
	ProvisionStageRemovingStorage   ProvisionStageKind = "RemovingStorage"
	ProvisionStageCleaningCache     ProvisionStageKind = "CleaningCache"
)

// provisionStageOrder is the Rust declaration order, which derive(Ord) uses.
var provisionStageOrder = []ProvisionStageKind{
	ProvisionStagePullingImage, ProvisionStageWaitingForImagePull,
	ProvisionStageWaitingForContainerUpgrade, ProvisionStageWaitingForWorkerStore,
	ProvisionStageWaitingForSshConnection, ProvisionStageWaitingForStartSlot,
	ProvisionStageProvisioning, ProvisionStageBooting, ProvisionStageCloning,
	ProvisionStageSyncing, ProvisionStageRestoring, ProvisionStageStarting,
	ProvisionStageInstalling, ProvisionStagePreparingHarness, ProvisionStageCompacting,
	ProvisionStageRecoveryCopy, ProvisionStageVerifying, ProvisionStageClosing,
	ProvisionStageStoppingTarget, ProvisionStageRemovingContainer,
	ProvisionStageRemovingStorage, ProvisionStageCleaningCache,
}

// ProvisionStage is the launch phase a command belongs to, reported as
// launch progress. One struct with a kind field: Harness is set for
// Installing and PreparingHarness, Step and SinceMs for PreparingHarness.
type ProvisionStage struct {
	Kind    ProvisionStageKind
	Harness HarnessKind
	Step    string
	SinceMs int64
}

// Compare orders stages as Rust's derived Ord does: by variant, then by
// the variant's fields.
func (s ProvisionStage) Compare(other ProvisionStage) int {
	panic("unported: mj-core/src/targets.rs ProvisionStage::cmp")
}

// MarshalJSON writes serde's externally tagged form: a unit variant as its
// name, Installing as {"Installing": harness} and PreparingHarness as
// {"PreparingHarness": {"harness", "step", "since_ms"}}.
func (s ProvisionStage) MarshalJSON() ([]byte, error) {
	switch s.Kind {
	case ProvisionStageInstalling:
		return json.Marshal(map[string]HarnessKind{string(s.Kind): s.Harness})
	case ProvisionStagePreparingHarness:
		return json.Marshal(map[string]any{string(s.Kind): struct {
			Harness HarnessKind `json:"harness"`
			Step    string      `json:"step"`
			SinceMs int64       `json:"since_ms"`
		}{s.Harness, s.Step, s.SinceMs}})
	default:
		return json.Marshal(string(s.Kind))
	}
}

// UnmarshalJSON reads serde's externally tagged form.
func (s *ProvisionStage) UnmarshalJSON(data []byte) error {
	var name string
	if err := json.Unmarshal(data, &name); err == nil {
		for _, kind := range provisionStageOrder {
			if string(kind) == name && kind != ProvisionStageInstalling && kind != ProvisionStagePreparingHarness {
				*s = ProvisionStage{Kind: kind}
				return nil
			}
		}
		return fmt.Errorf("unknown variant `%s` of ProvisionStage", name)
	}
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return err
	}
	if len(tagged) != 1 {
		return errors.New("ProvisionStage must be a string or a single-key object")
	}
	for name, body := range tagged {
		switch ProvisionStageKind(name) {
		case ProvisionStageInstalling:
			var harness HarnessKind
			if err := json.Unmarshal(body, &harness); err != nil {
				return err
			}
			*s = ProvisionStage{Kind: ProvisionStageInstalling, Harness: harness}
		case ProvisionStagePreparingHarness:
			var fields struct {
				Harness HarnessKind `json:"harness"`
				Step    string      `json:"step"`
				SinceMs int64       `json:"since_ms"`
			}
			if err := json.Unmarshal(body, &fields); err != nil {
				return err
			}
			*s = ProvisionStage{Kind: ProvisionStagePreparingHarness, Harness: fields.Harness, Step: fields.Step, SinceMs: fields.SinceMs}
		default:
			return fmt.Errorf("unknown variant `%s` of ProvisionStage", name)
		}
	}
	return nil
}

// Label is the stage's name in launch progress.
func (s ProvisionStage) Label() string {
	panic("unported: mj-core/src/targets.rs ProvisionStage::label")
}

// PausesStallClock reports whether time in this stage stays off a stall
// clock. Queue time is not progress: the stage waits for a turn behind other
// work, and admission ends it. Harness preparation reports its own progress
// and failure (the native startup deadline is likewise suspended during it),
// and one step, such as a download, can be slow without being stuck.
func (s ProvisionStage) PausesStallClock() bool {
	switch s.Kind {
	case ProvisionStageWaitingForImagePull, ProvisionStageWaitingForContainerUpgrade,
		ProvisionStageWaitingForWorkerStore, ProvisionStageWaitingForSshConnection,
		ProvisionStageWaitingForStartSlot, ProvisionStagePreparingHarness:
		return true
	}
	return false
}

// StartedAtEpochSeconds: the worker owns the start time for harness
// preparation. Other stages start when the controller observes them.
func (s ProvisionStage) StartedAtEpochSeconds() (uint64, bool) {
	if s.Kind != ProvisionStagePreparingHarness {
		return 0, false
	}
	return uint64(max(s.SinceMs, 0) / 1_000), true
}

// SensitiveCommandInput is input that must reach the child without becoming
// part of its arguments, environment, serialized plan, or debug
// representation.
type SensitiveCommandInput struct {
	bytes []byte
}

// String redacts the input.
func (SensitiveCommandInput) String() string { return "<redacted>" }

// StreamReplyObserver receives a streamed command's stdout replies, after SSH
// framing has been removed. A nil chunk is the Rust None: end of an accepted
// stream, so input awaiting a reply can finish even when the remote command
// exits early. Non-empty chunks only are reported otherwise.
type StreamReplyObserver func(chunk []byte)

// StreamReply wraps a StreamReplyObserver; two are equal only when they are
// the same observer.
type StreamReply struct {
	observer StreamReplyObserver
}

// CommandObserver receives the executor's activity during one streamed
// command.
type CommandObserver struct {
	report func(CommandActivity)
}

func (o *CommandObserver) reportActivity(activity CommandActivity) {
	o.report(activity)
}

// CommandActivityKind names a CommandActivity variant.
type CommandActivityKind int

const (
	CommandActivityStdinStarted CommandActivityKind = iota
	// CommandActivityStdinBytes carries Bytes.
	CommandActivityStdinBytes
	CommandActivityStdinFinished
	// CommandActivityStageStarted carries Stage.
	CommandActivityStageStarted
	// CommandActivityStageFinished carries Stage.
	CommandActivityStageFinished
)

// CommandActivity is one event from the process executor during one
// streamed command.
type CommandActivity struct {
	Kind  CommandActivityKind
	Bytes int
	Stage ProvisionStage
}

// CommandSpec is one argv command of an execution plan.
type CommandSpec struct {
	Program string
	// arg0 overrides `argv[0]` when the child's login mode is encoded there
	// rather than in an argument. This is executor-local (Unix only) and is
	// not serialized.
	arg0 *string
	Args []string
	Env  map[string]string
	// EnvRemove lists ambient variables to remove before the child starts.
	// Used for a probe that must not see the daemon's inherited credentials.
	EnvRemove []string
	// ClearEnv replaces ambient process variables with the explicitly
	// supplied environment.
	ClearEnv bool
	// SensitiveOutput: output carries credentials and must not be inspected
	// by diagnostic observers. Executor-local, never persisted in command
	// plans.
	SensitiveOutput bool
	// WarnOnStderr logs non-empty stderr at WARN even when the command exits
	// 0. Executor-local, never persisted in command plans.
	WarnOnStderr bool
	Cwd          *string
	Purpose      string
	Stage        *ProvisionStage
	// ParallelGroup: commands that share this marker and appear
	// consecutively in a plan's command list may run concurrently under
	// CommandPlan.ExecuteConcurrent. Commands without a marker, or whose
	// neighbors do not share it, keep running strictly in plan order.
	ParallelGroup *uint32
	// CreatesTarget: this command brings the session's target into
	// existence. Every command after it in a provisioning plan runs against
	// a target that already exists, so a later failure owes that target's
	// teardown.
	CreatesTarget bool
	// Detaches: this command deliberately leaves a process running after it
	// exits: it starts a detached worker, whose lifetime belongs to the
	// session rather than to the launch. The executor still gives the command
	// its own process group, and cancellation still signals that group, but
	// the successful completion of a launch must not kill what it launched.
	Detaches bool
	// preserveChildrenOnCancel lets child processes finish if the supervisor
	// cancels this command. The caller must ensure children can complete
	// without the command process. Executor-local; not serialized.
	preserveChildrenOnCancel bool
	// SSHDestination is the SSH destination this command opens a connection
	// to, when it does. Tagged commands retry a server refusal before
	// authentication rather than reporting it as a command failure.
	SSHDestination *string
	// SSHSession is the shared SSH connection this command runs on as one
	// session, when it does. Just before spawning, the executor leases a
	// session on one of the connection's masters with SshSessionsLease and
	// puts the options that bind the command to that master in front of its
	// arguments (see CommandSpec.OpenSSHSession). The arguments stored here
	// never contain them.
	SSHSession *SshSessionRequest
	// SSHSessionProbe: the session is a fail-fast probe: it is counted on a
	// shard but never opens a master (see CommandSpec.SSHProbeSession).
	SSHSessionProbe bool
	// sshStartMarker: the remote command line starts by printing
	// SSHStartMarker, so the reader of this command's stdout strips it and
	// the command's start is known from it. Set only by OpenSSHSession, on
	// the command it returns.
	sshStartMarker bool
	// sensitiveStdin is input that must reach the child without becoming
	// part of its arguments, environment, serialized plan, or debug
	// representation.
	sensitiveStdin *SensitiveCommandInput
	streamReply    *StreamReply
	observer       *CommandObserver
}

// commandSpecWire is CommandSpec's serialized shape, in serde field order.
type commandSpecWire struct {
	Program         string             `json:"program"`
	Args            []string           `json:"args"`
	Env             map[string]string  `json:"env"`
	EnvRemove       []string           `json:"env_remove,omitempty"`
	ClearEnv        bool               `json:"clear_env,omitempty"`
	Cwd             *string            `json:"cwd,omitempty"`
	Purpose         string             `json:"purpose"`
	Stage           *ProvisionStage    `json:"stage"`
	ParallelGroup   *uint32            `json:"parallel_group"`
	CreatesTarget   bool               `json:"creates_target,omitempty"`
	Detaches        bool               `json:"detaches,omitempty"`
	SSHDestination  *string            `json:"ssh_destination,omitempty"`
	SSHSession      *SshSessionRequest `json:"ssh_session,omitempty"`
	SSHSessionProbe bool               `json:"ssh_session_probe,omitempty"`
	SSHStartMarker  bool               `json:"ssh_start_marker,omitempty"`
}

// MarshalJSON writes the persisted fields only, as serde does.
func (c CommandSpec) MarshalJSON() ([]byte, error) {
	wire := commandSpecWire{
		Program: c.Program, Args: c.Args, Env: c.Env, EnvRemove: c.EnvRemove,
		ClearEnv: c.ClearEnv, Cwd: c.Cwd, Purpose: c.Purpose, Stage: c.Stage,
		ParallelGroup: c.ParallelGroup, CreatesTarget: c.CreatesTarget,
		Detaches: c.Detaches, SSHDestination: c.SSHDestination,
		SSHSession: c.SSHSession, SSHSessionProbe: c.SSHSessionProbe,
		SSHStartMarker: c.sshStartMarker,
	}
	if wire.Args == nil {
		wire.Args = []string{}
	}
	if wire.Env == nil {
		wire.Env = map[string]string{}
	}
	return json.Marshal(wire)
}

// UnmarshalJSON reads the persisted fields; executor-local fields stay zero.
func (c *CommandSpec) UnmarshalJSON(data []byte) error {
	var wire commandSpecWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*c = CommandSpec{
		Program: wire.Program, Args: wire.Args, Env: wire.Env, EnvRemove: wire.EnvRemove,
		ClearEnv: wire.ClearEnv, Cwd: wire.Cwd, Purpose: wire.Purpose, Stage: wire.Stage,
		ParallelGroup: wire.ParallelGroup, CreatesTarget: wire.CreatesTarget,
		Detaches: wire.Detaches, SSHDestination: wire.SSHDestination,
		SSHSession: wire.SSHSession, SSHSessionProbe: wire.SSHSessionProbe,
		sshStartMarker: wire.SSHStartMarker,
	}
	if c.Env == nil {
		c.Env = map[string]string{}
	}
	return nil
}

// NewCommandSpec builds a command running program with args.
func NewCommandSpec(program string, args []string) CommandSpec {
	return CommandSpec{
		Program: program,
		Args:    append([]string{}, args...),
		Env:     map[string]string{},
	}
}

// Observe streamed stdin and any stages reported while opening SSH.
// Callbacks may run on the writer goroutine and are local to this execution.
func (c CommandSpec) Observe(report func(CommandActivity)) CommandSpec {
	c.observer = &CommandObserver{report: report}
	return c
}

// Arg0 starts the program with a specific `argv[0]` (Unix only).
//
// POSIX shells use `-l`, but C shells recognize login mode from a leading
// dash in `argv[0]` (for example, `-tcsh`). SSH does the same when it
// starts an account's login shell.
func (c CommandSpec) Arg0(arg0 string) CommandSpec {
	c.arg0 = &arg0
	return c
}

// WithStreamReply reports the command's stdout replies to observer.
func (c CommandSpec) WithStreamReply(observer StreamReplyObserver) CommandSpec {
	c.streamReply = &StreamReply{observer: observer}
	return c
}

// WithPurpose sets the purpose (Rust: CommandSpec::purpose).
func (c CommandSpec) WithPurpose(purpose string) CommandSpec {
	c.Purpose = purpose
	return c
}

// WithStage sets the stage (Rust: CommandSpec::stage).
func (c CommandSpec) WithStage(stage ProvisionStage) CommandSpec {
	c.Stage = &stage
	return c
}

// WithParallelGroup marks this command as eligible to run concurrently with
// its plan-adjacent siblings that share the same group (Rust:
// CommandSpec::parallel_group).
func (c CommandSpec) WithParallelGroup(group uint32) CommandSpec {
	c.ParallelGroup = &group
	return c
}

// WithWarnOnStderr logs non-empty stderr at WARN when this command succeeds
// (Rust: CommandSpec::warn_on_stderr).
func (c CommandSpec) WithWarnOnStderr() CommandSpec {
	c.WarnOnStderr = true
	return c
}

// WithSSHDestination records that this command opens an `ssh` connection to
// destination, which is the host (or `user@host`) argument, never an option
// value (Rust: CommandSpec::ssh_destination).
func (c CommandSpec) WithSSHDestination(destination string) CommandSpec {
	c.SSHDestination = &destination
	return c
}

// WithSSHSession records that this command is an `ssh` or `scp` invocation
// that runs as one session on ssh's shared connection. It also opens a
// connection to that destination when sharing is off, so a server's refusal
// of it is retried the same way (Rust: CommandSpec::ssh_session).
func (c CommandSpec) WithSSHSession(ssh *SshTarget, class SshTrafficClass) CommandSpec {
	destination := ssh.Destination
	c.SSHDestination = &destination
	c.SSHSession = &SshSessionRequest{Target: ssh.Clone(), Class: class}
	return c
}

// SSHProbeSession records that this command is a fail-fast `ssh` probe, such
// as a target validation, that joins ssh's shared connection without ever
// opening a master. Its short timeouts and keepalives belong to the probe
// alone: a master opened by it would impose them on every later session. The
// probe still uses one of the master's sessions while it runs, so the
// executor counts it in the session ledger like any other session.
func (c CommandSpec) SSHProbeSession(ssh *SshTarget) CommandSpec {
	c = c.WithSSHSession(ssh, SshTrafficClassControl)
	c.SSHSessionProbe = true
	return c
}

// OpenSSHSession leases the SSH session this command asks for and returns the
// command as it must be spawned, with the lease that must outlive the child.
//
// An `ssh` command's remote line is prefixed to print SSHStartMarker first.
// The reader of its stdout must strip it and report the start to the lease's
// StartGate (see SessionCommand.Gate); the executors and
// `spawn_session_proxy` do. A command without a session request is returned
// unchanged. executor runs the master check and, when needed, the opener.
// walk is the command's walk across the connection's shards: a command that
// retries after a refusal passes the same walk to each attempt (see
// SshRefusalRetry.Walk).
func (c *CommandSpec) OpenSSHSession(executor CommandExecutor, walk *SshWalk) (*SessionCommand, error) {
	return c.leaseSSHSession(executor, walk, true)
}

// OpenSSHSessionWithoutMarker is OpenSSHSession for a command whose `ssh` is
// run by another program that reads its stdout as its own protocol, such as
// the shell wrapper Move gives `rsync --rsh`: no marker is added, and nothing
// reports the session's start, so it counts only as a reservation.
func (c *CommandSpec) OpenSSHSessionWithoutMarker(executor CommandExecutor, walk *SshWalk) (*SessionCommand, error) {
	return c.leaseSSHSession(executor, walk, false)
}

func (c *CommandSpec) leaseSSHSession(executor CommandExecutor, walk *SshWalk, marker bool) (*SessionCommand, error) {
	panic("unported: mj-core/src/targets.rs CommandSpec::lease_ssh_session")
}

// ExpectsSSHStartMarker reports whether the command's stdout starts with
// SSHStartMarker.
func (c *CommandSpec) ExpectsSSHStartMarker() bool {
	return c.sshStartMarker
}

// WithCreatesTarget marks this command as the one that creates the session's
// target (Rust: CommandSpec::creates_target).
func (c CommandSpec) WithCreatesTarget() CommandSpec {
	c.CreatesTarget = true
	return c
}

// PreserveChildrenOnCancel keeps child processes running if the supervising
// executor stops waiting.
func (c CommandSpec) PreserveChildrenOnCancel() CommandSpec {
	c.preserveChildrenOnCancel = true
	return c
}

// WithSensitiveStdin feeds private file content through the shared
// concurrent pipe handler. The bytes stay out of argv, environments,
// serialization, and debug output.
func (c CommandSpec) WithSensitiveStdin(input []byte) CommandSpec {
	c.sensitiveStdin = &SensitiveCommandInput{bytes: input}
	return c
}

// SensitiveStdin is the command-owned input, for executors and test fakes.
func (c *CommandSpec) SensitiveStdin() ([]byte, bool) {
	if c.sensitiveStdin == nil {
		return nil, false
	}
	return c.sensitiveStdin.bytes, true
}

// SessionCommand is a command ready to spawn, together with the SSH session
// lease it runs on. Keep this value alive until the child has exited:
// releasing the lease frees the session slot.
type SessionCommand struct {
	command *CommandSpec
	lease   *SshSessionLease
}

// Command is the command to spawn.
func (s *SessionCommand) Command() *CommandSpec { return s.command }

// Lease is the session lease, when the command runs on a shared connection.
func (s *SessionCommand) Lease() *SshSessionLease { return s.lease }

// Gate is where the session stands, when its command prints the start
// marker: the reader of its stdout reports into it.
func (s *SessionCommand) Gate() *StartGate {
	if !s.command.sshStartMarker || s.lease == nil {
		return nil
	}
	return s.lease.Gate()
}

// IntoParts returns the command and its lease.
func (s *SessionCommand) IntoParts() (CommandSpec, *SshSessionLease) {
	return *s.command, s.lease
}

// CommandOutput is a finished command's exit status and output.
type CommandOutput struct {
	Status int
	Stdout []byte
	Stderr []byte
}

// DeploymentCapacityKind names the kind of a capacity probe target.
type DeploymentCapacityKind string

const (
	DeploymentCapacityKindHost     DeploymentCapacityKind = "host"
	DeploymentCapacityKindAwsFleet DeploymentCapacityKind = "aws_fleet"
)

// DeploymentCapacityTarget is one host or AWS fleet whose capacity is
// probed.
type DeploymentCapacityTarget struct {
	ID        string
	Host      string
	TargetIDs []string
	Kind      DeploymentCapacityKind
	Local     bool
	// Probes are alternative commands for a host, or one command per live
	// AWS instance. The local host has none: the host's own statistics give
	// its CPU and memory, and its storage is measured over
	// LocalStoragePaths.
	Probes []CommandSpec
	// LocalStoragePaths are the paths on this machine whose filesystems
	// local targets write to. Empty for every other host, whose probes carry
	// their own paths.
	LocalStoragePaths []string
	// ProbeError prevents a partial AWS fleet sample when one live instance
	// cannot be probed yet.
	ProbeError *string
}

// DeploymentCapacityUsage is one capacity reading.
type DeploymentCapacityUsage struct {
	// MemoryPool reports whether MemoryTotalBytes is a pool set aside for
	// sessions at boot (a Nitro host's sandbox memory) rather than the
	// host's RAM. A pool is already net of the host's own needs, so
	// admission applies no reserve to it.
	MemoryPool       bool
	CPUPercent       *uint8
	MemoryUsedBytes  uint64
	MemoryTotalBytes uint64
	LogicalCores     uint64
	DiskTotalBytes   *uint64
	// Storage is free space on the filesystems Mjolnir writes to, per machine
	// probed: one for a host, one per instance for a fleet.
	Storage []HostStorageSample
}

// AdditionalMount is an additional directory made available to one session.
//
// Containers use isolated mounts. Remote targets may instead receive a
// controller-packed snapshot at the destination while retaining this shared
// persisted shape.
type AdditionalMount struct {
	Source      string
	Destination string
	Access      MountAccess
}

// MountAccess is how a container sees an attached directory.
type MountAccess string

const (
	// MountAccessRo: the container can read the directory but not change it.
	MountAccessRo MountAccess = "ro"
	// MountAccessCow: the container writes to a private copy-on-write
	// overlay; the host directory never changes.
	MountAccessCow MountAccess = "cow"
	// MountAccessRw: the container writes straight through to the host
	// directory.
	MountAccessRw MountAccess = "rw"
)

// MountAccessAll lists every mode in declaration order.
var MountAccessAll = [3]MountAccess{MountAccessRo, MountAccessCow, MountAccessRw}

// Label is the mode's short name.
func (a MountAccess) Label() string { return string(a) }

// WithoutOverlay is the mode this attachment takes where the host filesystem
// cannot carry a copy-on-write overlay. Read-only is the substitute, because
// the alternative would let the container write through to the host
// directory the user asked to keep unchanged.
//
// This is the one rule for an unavailable overlay: the wizard offers the same
// modes MountAccessOffered lists, and the runtime downgrades a stored mount
// the same way.
func (a MountAccess) WithoutOverlay() MountAccess {
	if a == MountAccessCow {
		return MountAccessRo
	}
	return a
}

// MountAccessOffered lists the modes an attachment may be given, given
// whether the host filesystem can carry the copy-on-write overlay.
func MountAccessOffered(overlayAvailable bool) []MountAccess {
	var offered []MountAccess
	for _, access := range MountAccessAll {
		if overlayAvailable || access.WithoutOverlay() == access {
			offered = append(offered, access)
		}
	}
	return offered
}

// AdditionalMountRepr is the persisted shape. Read-only and copy-on-write
// mounts keep the original `read_only` boolean alone, so archives and API
// payloads that older builds read stay unchanged; only read-write adds
// `access`, which older builds cannot represent.
type AdditionalMountRepr struct {
	Source      string       `json:"source"`
	Destination string       `json:"destination"`
	ReadOnly    bool         `json:"read_only"`
	Access      *MountAccess `json:"access,omitempty"`
}

// MarshalJSON writes AdditionalMountRepr.
func (m AdditionalMount) MarshalJSON() ([]byte, error) {
	repr := AdditionalMountRepr{
		Source:      m.Source,
		Destination: m.Destination,
		ReadOnly:    m.Access == MountAccessRo,
	}
	if m.Access == MountAccessRw {
		access := MountAccessRw
		repr.Access = &access
	}
	return json.Marshal(repr)
}

// UnmarshalJSON reads AdditionalMountRepr, refusing unknown fields.
func (m *AdditionalMount) UnmarshalJSON(data []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	var repr AdditionalMountRepr
	if err := decoder.Decode(&repr); err != nil {
		return err
	}
	access := MountAccessCow
	if repr.ReadOnly {
		access = MountAccessRo
	}
	if repr.Access != nil {
		access = *repr.Access
	}
	*m = AdditionalMount{Source: repr.Source, Destination: repr.Destination, Access: access}
	return nil
}

// OverlayUnsupportedFilesystem says why a filesystem cannot host a container
// target's copy-on-write overlay, or false when it can. Unknown types are
// allowed: the overlay is the better mount and only a filesystem known to
// break it is downgraded.
//
// The names are those Linux `stat -f -c %T` and macOS `mount` report,
// matched case-insensitively.
func OverlayUnsupportedFilesystem(filesystem string) (string, bool) {
	panic("unported: mj-core/src/targets.rs overlay_unsupported_filesystem")
}

// ValidateMountDestination: container destinations cannot use the
// controller or login user's home.
func ValidateMountDestination(path string) error {
	panic("unported: mj-core/src/targets.rs validate_mount_destination")
}

// ValidateAdditionalMounts checks every mount's source and destination and
// refuses a destination configured twice.
func ValidateAdditionalMounts(mounts []AdditionalMount) error {
	panic("unported: mj-core/src/targets.rs validate_additional_mounts")
}

// DefaultMountDestination chooses the editable default destination for an
// additional host directory.
func DefaultMountDestination(source string, existing []AdditionalMount) string {
	panic("unported: mj-core/src/targets.rs default_mount_destination")
}

// CommandExecutor runs target commands. Rust's trait has default methods;
// Go implementers embed CommandExecutorDefaults for the ones that need no
// access to the implementer, and write ExecuteCleanup and
// ExecuteCleanupWithTimeout themselves (the Rust defaults forward to Execute
// and ExecuteCleanup respectively).
type CommandExecutor interface {
	Execute(command *CommandSpec) (CommandOutput, error)

	// ExecuteCleanup bounds cleanup attempts while preserving supervisor
	// cancellation. Durable owners retry interrupted cleanup under the next
	// lifecycle or daemon. Rust default: Execute.
	ExecuteCleanup(command *CommandSpec) (CommandOutput, error)

	// ExecuteCleanupWithTimeout bounds one cleanup attempt by timeout.
	// Executors with shared cancellation continue to honor it, and
	// executor-specific hard limits may cap the requested timeout. Executors
	// without per-call deadlines retain their normal cleanup behavior. Rust
	// default: ExecuteCleanup.
	ExecuteCleanupWithTimeout(command *CommandSpec, timeout time.Duration) (CommandOutput, error)

	// CancellationRequested reports whether the operation supervising this
	// executor has requested cancellation. Test executors and ordinary
	// process execution are not cancellable unless they opt in.
	CancellationRequested() bool

	// StageStarted reports entry into a lifecycle stage. Callers that cover
	// more than one command should hold a ProvisionStageGuard for the whole
	// operation so concurrent stages remain visible between subprocesses.
	StageStarted(stage ProvisionStage)

	// StageFinished reports exit from a lifecycle stage previously passed to
	// StageStarted.
	StageFinished(stage ProvisionStage)

	// NotifyNotice reports a decision an operation made on the user's
	// behalf. This is not a failure: the work continues, and the user is
	// told what changed.
	NotifyNotice(notice string)

	// BeforeMoveSourceStop stops the source's children only once destination
	// preparation has succeeded.
	BeforeMoveSourceStop(ctx context.Context) error

	// ReserveMoveDestination: the Move source has been sealed; its
	// destination belongs exclusively to the lifecycle until queue admission
	// completes.
	ReserveMoveDestination()

	// BeginResumableWork yields daemon admission for interruptible work
	// recoverable after handoff.
	BeginResumableWork() error

	// YieldAdmissionIfResumable releases daemon admission, if this operation
	// holds it and the next daemon restarts the operation from a durable
	// record. Returns whether admission was released; the caller then owes
	// EndResumableWork. For a wait that can take minutes, such as shared
	// target content, so a handoff does not wait for it.
	YieldAdmissionIfResumable() bool

	// ShutdownInterrupted: the daemon is yielding a durable operation to its
	// replacement.
	ShutdownInterrupted() bool

	// EndResumableWork reacquires named control admission before leaving
	// resumable work.
	EndResumableWork(label string) error

	ExecuteWithStdin(command *CommandSpec, input io.Reader) (CommandOutput, error)

	// ExecuteWithoutDeadline runs command with this executor's cancellation
	// but without its deadline, for work a timeout named for another step
	// must not cover. An executor without a deadline runs it as Execute
	// does; one with a deadline drops it and keeps its cancellation.
	// Required, so every executor states which it is.
	ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error)

	// ExecuteWithStdinWithoutDeadline is ExecuteWithoutDeadline for a command
	// that reads input.
	ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error)

	// CancellationRequestedWithoutDeadline is CancellationRequested as the
	// no-deadline methods see it: a deadline that has passed is not a
	// cancellation.
	CancellationRequestedWithoutDeadline() bool

	// PauseDeadline excludes this scope's wall time from the operation
	// deadline. Required: wrappers forward to their owner; executors with
	// per-command deadlines or no deadline return an empty pause.
	// Cancellation remains active. End the returned pause when the scope
	// ends.
	PauseDeadline() *DeadlinePause
}

// CommandExecutorDefaults provides the Rust trait's default methods that do
// not call back into the implementer. Embed it in an executor.
type CommandExecutorDefaults struct{}

func (CommandExecutorDefaults) CancellationRequested() bool                { return false }
func (CommandExecutorDefaults) StageStarted(ProvisionStage)                {}
func (CommandExecutorDefaults) StageFinished(ProvisionStage)               {}
func (CommandExecutorDefaults) NotifyNotice(string)                        {}
func (CommandExecutorDefaults) BeforeMoveSourceStop(context.Context) error { return nil }
func (CommandExecutorDefaults) ReserveMoveDestination()                    {}
func (CommandExecutorDefaults) BeginResumableWork() error                  { return nil }
func (CommandExecutorDefaults) YieldAdmissionIfResumable() bool            { return false }
func (CommandExecutorDefaults) ShutdownInterrupted() bool                  { return false }
func (CommandExecutorDefaults) EndResumableWork(string) error              { return nil }
func (CommandExecutorDefaults) ExecuteWithStdin(*CommandSpec, io.Reader) (CommandOutput, error) {
	return CommandOutput{}, errors.New("this command executor does not support streamed stdin")
}

// SshStageReporter forwards SSH admission stages through the command's owner
// while the process executor retains cancellation and executes the actual
// master commands.
type SshStageReporter struct {
	CommandExecutorDefaults
	executor CommandExecutor
	observer *CommandObserver
}

func (r *SshStageReporter) Execute(command *CommandSpec) (CommandOutput, error) {
	return r.executor.Execute(command)
}

func (r *SshStageReporter) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return r.Execute(command)
}

func (r *SshStageReporter) ExecuteCleanupWithTimeout(command *CommandSpec, _ time.Duration) (CommandOutput, error) {
	return r.ExecuteCleanup(command)
}

func (r *SshStageReporter) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return r.executor.ExecuteWithoutDeadline(command)
}

func (r *SshStageReporter) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return r.executor.ExecuteWithStdinWithoutDeadline(command, input)
}

func (r *SshStageReporter) CancellationRequested() bool {
	return r.executor.CancellationRequested()
}

func (r *SshStageReporter) CancellationRequestedWithoutDeadline() bool {
	return r.executor.CancellationRequestedWithoutDeadline()
}

func (r *SshStageReporter) PauseDeadline() *DeadlinePause {
	return r.executor.PauseDeadline()
}

func (r *SshStageReporter) StageStarted(stage ProvisionStage) {
	if r.observer != nil {
		r.observer.reportActivity(CommandActivity{Kind: CommandActivityStageStarted, Stage: stage})
		return
	}
	r.executor.StageStarted(stage)
}

func (r *SshStageReporter) StageFinished(stage ProvisionStage) {
	if r.observer != nil {
		r.observer.reportActivity(CommandActivity{Kind: CommandActivityStageFinished, Stage: stage})
		return
	}
	r.executor.StageFinished(stage)
}

// WithoutDeadline is an executor whose commands run without the wrapped
// executor's deadline but still stop when its supervisor cancels. Staging
// work that precedes a timeout-bound command runs on this, so the timeout
// covers only the command it names. Its lifetime also pauses that deadline,
// including gate waits and other staging work that runs no command; call
// End when done (Rust: Drop).
type WithoutDeadline struct {
	executor CommandExecutor
	pause    *DeadlinePause
}

// NewWithoutDeadline wraps executor and pauses its deadline.
func NewWithoutDeadline(executor CommandExecutor) *WithoutDeadline {
	return &WithoutDeadline{executor: executor, pause: executor.PauseDeadline()}
}

// End ends the deadline pause.
func (w *WithoutDeadline) End() { w.pause.End() }

func (w *WithoutDeadline) PauseDeadline() *DeadlinePause { return w.executor.PauseDeadline() }

func (w *WithoutDeadline) CancellationRequestedWithoutDeadline() bool {
	return w.executor.CancellationRequestedWithoutDeadline()
}

func (w *WithoutDeadline) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return w.Execute(command)
}

func (w *WithoutDeadline) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return w.ExecuteWithStdin(command, input)
}

func (w *WithoutDeadline) Execute(command *CommandSpec) (CommandOutput, error) {
	return w.executor.ExecuteWithoutDeadline(command)
}

func (w *WithoutDeadline) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return w.executor.ExecuteCleanup(command)
}

func (w *WithoutDeadline) ExecuteCleanupWithTimeout(command *CommandSpec, timeout time.Duration) (CommandOutput, error) {
	return w.executor.ExecuteCleanupWithTimeout(command, timeout)
}

// CancellationRequested: the wrapper's whole purpose is to ignore the
// wrapped deadline, so a passed deadline must not read as a cancellation
// here either.
func (w *WithoutDeadline) CancellationRequested() bool {
	return w.executor.CancellationRequestedWithoutDeadline()
}

func (w *WithoutDeadline) StageStarted(stage ProvisionStage)  { w.executor.StageStarted(stage) }
func (w *WithoutDeadline) StageFinished(stage ProvisionStage) { w.executor.StageFinished(stage) }
func (w *WithoutDeadline) NotifyNotice(notice string)         { w.executor.NotifyNotice(notice) }

func (w *WithoutDeadline) BeforeMoveSourceStop(context.Context) error { return nil }
func (w *WithoutDeadline) ReserveMoveDestination()                    {}

func (w *WithoutDeadline) BeginResumableWork() error { return w.executor.BeginResumableWork() }

func (w *WithoutDeadline) YieldAdmissionIfResumable() bool {
	return w.executor.YieldAdmissionIfResumable()
}

func (w *WithoutDeadline) ShutdownInterrupted() bool { return w.executor.ShutdownInterrupted() }

func (w *WithoutDeadline) EndResumableWork(label string) error {
	return w.executor.EndResumableWork(label)
}

func (w *WithoutDeadline) ExecuteWithStdin(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return w.executor.ExecuteWithStdinWithoutDeadline(command, input)
}

// ProvisionStageGuard is a scoped lifecycle-stage report for controller-side
// work or a sequence of commands. Ending the guard reports completion even
// when the work returns early with an error; call End on every path
// (Rust: Drop).
type ProvisionStageGuard struct {
	executor CommandExecutor
	stage    ProvisionStage
}

// NewProvisionStageGuard reports stage as started.
func NewProvisionStageGuard(executor CommandExecutor, stage ProvisionStage) *ProvisionStageGuard {
	executor.StageStarted(stage)
	return &ProvisionStageGuard{executor: executor, stage: stage}
}

// End reports the stage as finished.
func (g *ProvisionStageGuard) End() { g.executor.StageFinished(g.stage) }

// ProcessExecutor runs commands as local processes with no cancellation.
type ProcessExecutor struct {
	CommandExecutorDefaults
}

// withSSHRefusalRetry runs one `ssh` invocation, retrying it when the server
// turned it away before the remote command started.
//
// A refusal means the remote command never started (no byte of its stdout,
// not even the start marker, ever arrived), so re-running the whole
// invocation cannot repeat a side effect, whether or not the command streams
// stdin: nothing of the caller's input is read before the start.
// SshRefusalRetry decides each wait and when to stop: a refusal that
// persists for its time bound fails the command with an error naming the
// causes, for every caller; the caller's cancellation or deadline ends the
// retries sooner.
//
// A command that asks for a shared-connection session leases it on each
// attempt, and the lease tells the ledger what the refusal means
// (SshSessionLease.NoteRefusal). A session turned away by the transport
// usually means its master died, so the retry checks and reopens the master.
// A session refused for `MaxSessions` moves to the next shard of the
// command's walk at once: the walk is the state change that ends that loop,
// so it needs no wait and spends no time.
//
// run receives the leased command and the gate its stdout reader must report
// into (nil when the command prints no start marker).
//
// Commands that are not tagged with a destination run untouched.
func withSSHRefusalRetry(
	command *CommandSpec,
	executor CommandExecutor,
	isCancelled func() bool,
	run func(command *CommandSpec, gate *StartGate) (CommandOutput, error),
) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs with_ssh_refusal_retry")
}

// noteSessionRefusal classifies a finished command's refusal by the server
// and tells the session's lease what it means. Returns the refusal and
// whether retrying at once can land elsewhere (see
// SshSessionLease.NoteRefusal); ok is false when the command was not
// refused. A command that printed the start marker was not refused, whatever
// its exit status.
func noteSessionRefusal(session *SessionCommand, output *CommandOutput) (refusal SshRefusal, walksOn bool, ok bool) {
	panic("unported: mj-core/src/targets.rs note_session_refusal")
}

// SleepUnlessCancelled waits out delay, giving up early if the supervising
// operation is cancelled. Returns whether the wait completed.
func SleepUnlessCancelled(delay time.Duration, isCancelled func() bool) bool {
	deadline := time.Now().Add(delay)
	for {
		if isCancelled() {
			return false
		}
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return true
		}
		time.Sleep(min(remaining, 50*time.Millisecond))
	}
}

// finishedCommand: every process executor finishes a command here: one debug
// line, so a slow launch or resume phase can be attributed from logs instead
// of re-profiled by hand, and one look at a failure for a full disk on the
// target.
func finishedCommand(command *CommandSpec, started time.Time, status int, stdout, stderr []byte) CommandOutput {
	panic("unported: mj-core/src/targets.rs finished_command")
}

// runOnce is one attempt, with no retry of its own.
func (e ProcessExecutor) runOnce(command *CommandSpec, gate *StartGate) (CommandOutput, error) {
	// One path for both executors: the child gets its own process group,
	// output is drained for a bounded time after it exits, and leftover
	// descendants are killed. With no cancellation it only ends when the
	// child does.
	return NewCancellableProcessExecutor(new(atomic.Bool)).runOnce(command, gate)
}

// Both process executors wait for their child, and for the SSH session lease
// before it, off the caller's goroutine's critical path.

func (e ProcessExecutor) PauseDeadline() *DeadlinePause { return DeadlinePauseNone() }

func (e ProcessExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e ProcessExecutor) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

func (e ProcessExecutor) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e ProcessExecutor) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

func (e ProcessExecutor) ExecuteCleanupWithTimeout(command *CommandSpec, _ time.Duration) (CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e ProcessExecutor) Execute(command *CommandSpec) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs ProcessExecutor::execute")
}

func (e ProcessExecutor) ExecuteWithStdin(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs ProcessExecutor::execute_with_stdin")
}

// CommandOutputObserver observes bytes as a supervised command drains them.
// The callback may run concurrently for stdout and stderr, and must finish
// without blocking.
type CommandOutputObserver func(chunk []byte)

// ObservedStdin reports StdinFinished when ended (Rust: Drop).
type ObservedStdin struct {
	observer *CommandObserver
}

func (o *ObservedStdin) end() {
	o.observer.reportActivity(CommandActivity{Kind: CommandActivityStdinFinished})
}

// ProgressWriter writes to a child's stdin, reporting progress to an
// observer in small writes so backpressure exposes progress before a full
// input buffer has drained.
type ProgressWriter struct {
	stdin    io.WriteCloser
	observer *CommandObserver
}

func (w *ProgressWriter) Write(data []byte) (int, error) {
	panic("unported: mj-core/src/targets.rs ProgressWriter::write")
}

// streamCommandWithStdin streams input into a freshly spawned child and
// collects its output.
//
// Both executors share this one implementation because the pipe edge cases
// are easy to get subtly wrong in a second copy.
//
// isCancelled reports whether the supervising operation wants the transfer
// abandoned; ProcessExecutor passes a check that is never true, which also
// makes the kill path unreachable for it.
//
// With a gate, the command prints a start marker that is stripped from the
// collected stdout, and input is not read, nor anything written to the
// child, until the marker arrives. A session the server refused therefore
// leaves input untouched for the caller to retry with.
func streamCommandWithStdin(
	process *exec.Cmd,
	command *CommandSpec,
	input io.Reader,
	isCancelled func() bool,
	gate *StartGate,
	observer CommandOutputObserver,
) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs stream_command_with_stdin")
}

// CancellableProcessExecutor runs commands as local processes in their own
// process groups, stopping them when its supervisor cancels or its deadline
// passes. Copies share cancellation and deadline.
type CancellableProcessExecutor struct {
	CommandExecutorDefaults
	cancelled *atomic.Bool
	token     context.Context
	// foregroundTerminal is Unix only.
	foregroundTerminal bool
	deadline           *ProcessDeadline
}

// ProcessDeadline is one shared clock that owns both deadline checks and
// nested pause accounting. Its mutex guards every field.
type ProcessDeadline struct {
	mu          sync.Mutex
	expires     time.Time
	timeout     time.Duration
	pausedSince *time.Time
	pauseDepth  int
}

func newProcessDeadline(timeout time.Duration) *ProcessDeadline {
	return &ProcessDeadline{expires: time.Now().Add(timeout), timeout: timeout}
}

// expired must be called with mu held.
func (d *ProcessDeadline) expired() bool {
	now := time.Now()
	if d.pausedSince != nil {
		now = *d.pausedSince
	}
	return !now.Before(d.expires)
}

// DeadlinePause excludes a scope from its owner's deadline, including work
// between commands. Nested or overlapping scopes count their union rather
// than adding time twice. Call End exactly once (Rust: Drop).
type DeadlinePause struct {
	deadline *ProcessDeadline
}

// DeadlinePauseNone: executors with no operation-wide deadline have nothing
// to pause.
func DeadlinePauseNone() *DeadlinePause { return &DeadlinePause{} }

func newDeadlinePause(deadline *ProcessDeadline) *DeadlinePause {
	panic("unported: mj-core/src/targets.rs DeadlinePause::new")
}

// End ends the pause; the deadline moves by the paused time once every
// overlapping pause has ended.
func (p *DeadlinePause) End() {
	panic("unported: mj-core/src/targets.rs DeadlinePause::drop")
}

// ProcessCancellationGuard cancels synchronous executor work when its
// supervising wait ends; call Cancel on every exit path (Rust: Drop).
type ProcessCancellationGuard struct {
	cancelled *atomic.Bool
}

// Cancel requests cancellation (Rust: Drop).
func (g *ProcessCancellationGuard) Cancel() { g.cancelled.Store(true) }

// ExecuteWithStdinObserved feeds input, closes stdin at EOF, and observes
// both output streams while retaining the executor's process-group,
// cancellation and SSH ownership.
func (e CancellableProcessExecutor) ExecuteWithStdinObserved(command *CommandSpec, input io.Reader, observer CommandOutputObserver) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs CancellableProcessExecutor::execute_with_stdin_observed")
}

// CancelOnDrop returns a guard that cancels this executor when cancelled.
func (e CancellableProcessExecutor) CancelOnDrop() *ProcessCancellationGuard {
	return &ProcessCancellationGuard{cancelled: e.cancelled}
}

// NewCancellableProcessExecutor builds an executor cancelled by cancelled.
func NewCancellableProcessExecutor(cancelled *atomic.Bool) CancellableProcessExecutor {
	return CancellableProcessExecutor{cancelled: cancelled}
}

// WithForegroundTerminal uses the supervising login's cancellation for
// blocking preparation. Interactive account shells need foreground terminal
// ownership even though their output is captured and cancellation owns
// their group (Unix only).
func (e CancellableProcessExecutor) WithForegroundTerminal() CancellableProcessExecutor {
	e.foregroundTerminal = true
	return e
}

// WithCancellation also cancels when ctx is done (Rust: a CancellationToken).
func (e CancellableProcessExecutor) WithCancellation(ctx context.Context) CancellableProcessExecutor {
	e.token = ctx
	return e
}

// IsCancelled reports cancellation by token, flag or deadline.
func (e CancellableProcessExecutor) IsCancelled() bool {
	if e.IsCancelledWithoutDeadline() {
		return true
	}
	if e.deadline == nil {
		return false
	}
	e.deadline.mu.Lock()
	defer e.deadline.mu.Unlock()
	return e.deadline.expired()
}

// IsCancelledWithoutDeadline reports whether the supervisor, not the
// deadline, has asked to stop.
func (e CancellableProcessExecutor) IsCancelledWithoutDeadline() bool {
	return (e.token != nil && e.token.Err() != nil) || e.cancelled.Load()
}

// CancellableProcessExecutorWithTimeout builds an executor bounded by one
// deadline (Rust: CancellableProcessExecutor::with_timeout).
func CancellableProcessExecutorWithTimeout(timeout time.Duration) CancellableProcessExecutor {
	return CancellableProcessExecutor{cancelled: new(atomic.Bool), deadline: newProcessDeadline(timeout)}
}

// WithDeadline bounds an existing flag-based executor with a deadline, so a
// wedged child becomes a reported failure instead of running forever.
func (e CancellableProcessExecutor) WithDeadline(timeout time.Duration) CancellableProcessExecutor {
	e.deadline = newProcessDeadline(timeout)
	return e
}

func (e CancellableProcessExecutor) cancellationMessage(purpose string) string {
	panic("unported: mj-core/src/targets.rs CancellableProcessExecutor::cancellation_message")
}

func (e CancellableProcessExecutor) checkCancelled(purpose string) error {
	if e.IsCancelled() {
		return errors.New(e.cancellationMessage(purpose))
	}
	return nil
}

// describeCancellation replaces an "operation cancelled" error with the
// executor's cancellation message.
func (e CancellableProcessExecutor) describeCancellation(command *CommandSpec, err error) error {
	panic("unported: mj-core/src/targets.rs CancellableProcessExecutor::describe_cancellation")
}

func configuredCommand(command *CommandSpec) *exec.Cmd {
	panic("unported: mj-core/src/targets.rs configured_command")
}

func cancellableCommand(command *CommandSpec) *exec.Cmd {
	panic("unported: mj-core/src/targets.rs cancellable_command")
}

// targetsIODrainTimeout is how long a command's output may keep arriving
// after its leader exits. A backgrounded descendant inherits the output
// pipes and can hold them open for its whole life, although the command it
// was started by is finished. The same bound as Codex's
// `IO_DRAIN_TIMEOUT_MS`. The owned process group is terminated once this
// drain ends.
const targetsIODrainTimeout = 2 * time.Second

// PollablePipe is a child pipe that can be waited on with a timeout, so its
// reader can be stopped while a descendant still holds the write end.
type PollablePipe interface {
	io.Reader
	readable(timeout time.Duration) bool
}

// PipeCollector collects one child pipe on its own goroutine. Stopping it
// drops the goroutine's end of the pipe, so a descendant that keeps the pipe
// open cannot hold the reader past the drain.
type PipeCollector struct {
	mu    sync.Mutex
	bytes []byte
	stop  *atomic.Bool
	task  PipeTask
}

// PipeTask is the reader goroutine: running until done is closed, then
// finished with err.
type PipeTask struct {
	done chan struct{}
	err  error
}

func pipeCollectorSpawn(pipe PollablePipe) *PipeCollector {
	return pipeCollectorSpawnScanning(pipe, nil)
}

// pipeCollectorSpawnScanning collects a session's stdout. With a gate the
// start marker is removed from the front, the first byte reports the
// session's start, and the end of the pipe reports that it ended; the gate
// settles however the goroutine stops.
func pipeCollectorSpawnScanning(pipe PollablePipe, gate *StartGate) *PipeCollector {
	return pipeCollectorSpawnOutput(pipe, gate, nil)
}

func pipeCollectorSpawnOutput(pipe PollablePipe, gate *StartGate, output *os.File) *PipeCollector {
	return pipeCollectorSpawnObserved(pipe, gate, output, nil)
}

func pipeCollectorSpawnObserved(pipe PollablePipe, gate *StartGate, output *os.File, observer CommandOutputObserver) *PipeCollector {
	panic("unported: mj-core/src/targets.rs PipeCollector::spawn_observed")
}

func (c *PipeCollector) isFinished() bool {
	select {
	case <-c.task.done:
		return true
	default:
		return false
	}
}

func (c *PipeCollector) join() {
	<-c.task.done
}

// failed: a finished reader owns its one result. The supervisor observes
// failure before waiting for a child that may otherwise keep working
// forever.
func (c *PipeCollector) failed() bool {
	return c.isFinished() && c.task.err != nil
}

// finish waits for end of stream until deadline, then stops the reader and
// returns everything it collected.
func (c *PipeCollector) finish(stream string, deadline time.Time) ([]byte, error) {
	panic("unported: mj-core/src/targets.rs PipeCollector::finish")
}

// close stops the reader (Rust: Drop).
func (c *PipeCollector) close() { c.stop.Store(true) }

func terminateCancellableChild(child *exec.Cmd, preserveChildren bool) {
	panic("unported: mj-core/src/targets.rs terminate_cancellable_child")
}

// runOnce is one attempt, with no retry of its own. A gate makes the attempt
// read the start marker from stdout and report the session's start.
func (e CancellableProcessExecutor) runOnce(command *CommandSpec, gate *StartGate) (CommandOutput, error) {
	return e.runOnceOutput(command, gate, nil)
}

// ExecuteToFile streams stdout into a file, retaining the shared
// cancellation, process-group, and SSH refusal/start-marker semantics.
// Returned stdout is empty.
func (e CancellableProcessExecutor) ExecuteToFile(command *CommandSpec, output *os.File) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs CancellableProcessExecutor::execute_to_file")
}

func (e CancellableProcessExecutor) runOnceOutput(command *CommandSpec, gate *StartGate, output *os.File) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs CancellableProcessExecutor::run_once_output")
}

func (e CancellableProcessExecutor) PauseDeadline() *DeadlinePause {
	return newDeadlinePause(e.deadline)
}

func (e CancellableProcessExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.IsCancelledWithoutDeadline()
}

func (e CancellableProcessExecutor) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return e.WithDeadline(15 * time.Second).Execute(command)
}

func (e CancellableProcessExecutor) ExecuteCleanupWithTimeout(command *CommandSpec, timeout time.Duration) (CommandOutput, error) {
	return e.WithDeadline(timeout).Execute(command)
}

func (e CancellableProcessExecutor) CancellationRequested() bool { return e.IsCancelled() }

func (e CancellableProcessExecutor) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	e.deadline = nil
	return e.Execute(command)
}

func (e CancellableProcessExecutor) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	e.deadline = nil
	return e.ExecuteWithStdin(command, input)
}

func (e CancellableProcessExecutor) Execute(command *CommandSpec) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs CancellableProcessExecutor::execute")
}

func (e CancellableProcessExecutor) ExecuteWithStdin(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return e.ExecuteWithStdinObserved(command, input, nil)
}

// CommandTimedOut: a command supervised by BoundedProcessExecutor did not
// finish before its deadline. Callers use errors.As to tell a hung probe
// apart from a command that could not be started at all.
type CommandTimedOut struct {
	Program string
	Purpose string
	Timeout time.Duration
}

func (e *CommandTimedOut) Error() string {
	return fmt.Sprintf("`%s` did not answer within %d seconds while trying to %s",
		e.Program, uint64(e.Timeout/time.Second), e.Purpose)
}

// BoundedProcessExecutor runs every command with its own deadline.
//
// CancellableProcessExecutorWithTimeout bounds a whole operation from a
// single shared deadline, which suits one provisioning run. Prerequisite
// probes are different: each one is expected to answer quickly, and a wedged
// socket or blackholed network must not stall the probes that follow it. A
// timeout here names the probe that hung, so the caller can report it the
// same way it reports any other probe failure.
type BoundedProcessExecutor struct {
	CommandExecutorDefaults
	timeout time.Duration
}

// NewBoundedProcessExecutor bounds each command by timeout.
func NewBoundedProcessExecutor(timeout time.Duration) BoundedProcessExecutor {
	return BoundedProcessExecutor{timeout: timeout}
}

func (e BoundedProcessExecutor) PauseDeadline() *DeadlinePause { return DeadlinePauseNone() }

func (e BoundedProcessExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

// ExecuteWithoutDeadline: this executor's only bound is its deadline and it
// has no cancellation, so without the deadline it is a plain process.
func (e BoundedProcessExecutor) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return ProcessExecutor{}.Execute(command)
}

func (e BoundedProcessExecutor) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return ProcessExecutor{}.ExecuteWithStdin(command, input)
}

func (e BoundedProcessExecutor) Execute(command *CommandSpec) (CommandOutput, error) {
	executor := CancellableProcessExecutorWithTimeout(e.timeout)
	output, err := executor.Execute(command)
	if err != nil && executor.IsCancelled() {
		return output, &CommandTimedOut{Program: command.Program, Purpose: command.Purpose, Timeout: e.timeout}
	}
	return output, err
}

func (e BoundedProcessExecutor) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

// ExecuteCleanupWithTimeout: this executor's configured timeout is a hard
// per-command cap; the call-specific cleanup timeout can only make the
// attempt shorter.
func (e BoundedProcessExecutor) ExecuteCleanupWithTimeout(command *CommandSpec, timeout time.Duration) (CommandOutput, error) {
	return CancellableProcessExecutorWithTimeout(min(e.timeout, timeout)).Execute(command)
}

func (e BoundedProcessExecutor) ExecuteWithStdin(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return CancellableProcessExecutorWithTimeout(e.timeout).ExecuteWithStdin(command, input)
}

// CommandPlan is an ordered list of commands with a description.
type CommandPlan struct {
	Description string        `json:"description"`
	Commands    []CommandSpec `json:"commands"`
}

// MarshalJSON writes Commands as [] when empty, as serde does.
func (p CommandPlan) MarshalJSON() ([]byte, error) {
	type plain CommandPlan
	if p.Commands == nil {
		p.Commands = []CommandSpec{}
	}
	return json.Marshal(plain(p))
}

// ValidateInheritedEnvironmentName: environment inherited through a shell
// must be an assignment identifier.
func ValidateInheritedEnvironmentName(name string) error {
	valid := name != ""
	for index := 0; valid && index < len(name); index++ {
		b := name[index]
		valid = b == '_' || (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') || (index > 0 && b >= '0' && b <= '9')
	}
	if !valid {
		return errors.New("invalid inherited environment variable name")
	}
	return nil
}

// CommandStep is one argv-only operation in an ordered shell batch.
type CommandStep struct {
	argv             []string
	purpose          string
	acceptedStatuses []int
	cleanup          bool
	bestEffort       bool
}

// NewCommandStep runs argv for purpose, accepting status 0.
func NewCommandStep(purpose string, argv []string) CommandStep {
	return CommandStep{argv: append([]string{}, argv...), purpose: purpose, acceptedStatuses: []int{0}}
}

// AcceptedStatuses replaces the statuses that count as success.
func (s CommandStep) AcceptedStatuses(statuses ...int) CommandStep {
	s.acceptedStatuses = append([]int{}, statuses...)
	return s
}

// Cleanup runs on shell exit, including when an earlier operation failed.
func (s CommandStep) Cleanup() CommandStep {
	s.cleanup = true
	return s
}

// BestEffortCleanup reports a cleanup failure to the caller without failing
// the batch. Callers must log StepScriptWarnings even when execution failed.
func (s CommandStep) BestEffortCleanup() CommandStep {
	s.cleanup = true
	s.bestEffort = true
	return s
}

// StepScript builds one `sh -c` argv. Local decisions and transport
// selection stay with the caller; only these already-decided operations run
// in the script.
func StepScript(steps []CommandStep) []string {
	panic("unported: mj-core/src/targets.rs step_script")
}

// StepScriptError decodes the first failed step, preserving its stderr and
// original purpose. The marker includes JSON-escaped purpose text so
// transports and serialized plans need no extra metadata, including for
// purposes containing newlines.
func StepScriptError(output *CommandOutput) (string, bool) {
	panic("unported: mj-core/src/targets.rs step_script_error")
}

// StepScriptWarnings are failures of cleanup steps marked best-effort.
// Always inspect these before checking the command result, so an earlier
// failure cannot hide cleanup errors.
func StepScriptWarnings(output *CommandOutput) []string {
	panic("unported: mj-core/src/targets.rs step_script_warnings")
}

func parseStepMarker(line, prefix string) (status int, purpose string, ok bool) {
	panic("unported: mj-core/src/targets.rs parse_step_marker")
}

// ProvideTargetEnvironmentSecret supplies one container environment value
// without placing it in the Podman/SSH argument vector. See
// ProvideTargetEnvironmentSecrets.
func (p *CommandPlan) ProvideTargetEnvironmentSecret(target *TargetsTargetTemplate, name, value string) error {
	return p.ProvideTargetEnvironmentSecrets(target, [][2]string{{name, value}})
}

// ProvideTargetEnvironmentSecrets supplies container environment values
// (name, value pairs) without placing them in the Podman/SSH argument
// vector. The target launcher reads one value per line from stdin, exports
// each, and the container engine is asked to inherit it by name
// (`--env NAME`).
func (p *CommandPlan) ProvideTargetEnvironmentSecrets(target *TargetsTargetTemplate, secrets [][2]string) error {
	panic("unported: mj-core/src/targets.rs CommandPlan::provide_target_environment_secrets")
}

// Execute runs the plan's commands in order, failing at the first failure.
func (p *CommandPlan) Execute(executor CommandExecutor) ([]CommandOutput, error) {
	outputs := make([]CommandOutput, 0, len(p.Commands))
	for i := range p.Commands {
		command := &p.Commands[i]
		output, err := executor.Execute(command)
		if err != nil {
			return nil, err
		}
		checked, err := CheckedCommandOutput(command, output)
		if err != nil {
			return nil, err
		}
		outputs = append(outputs, checked)
	}
	return outputs, nil
}

// ExecuteConcurrent executes the plan the same way Execute does, except that
// commands sharing a CommandSpec.ParallelGroup marker and appearing
// consecutively in Commands run concurrently as one batch.
//
// A batch starts only once every earlier command has succeeded, and a batch
// that fails reports the first failure in plan order regardless of which
// command finished first — the same fail-fast contract Execute provides
// between individual commands. The executor must be safe for concurrent
// use.
func (p *CommandPlan) ExecuteConcurrent(executor CommandExecutor) ([]CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs CommandPlan::execute_concurrent")
}

// SplitAtTargetCreation splits the plan around the command that creates the
// session's target: the commands through that one, then the commands that
// run against a target which already exists.
//
// A plan that creates nothing — an existing project directory, say — splits
// into nothing, so a caller never arms a teardown for a target it did not
// bring into existence.
func (p *CommandPlan) SplitAtTargetCreation() (creation, remainder CommandPlan, ok bool) {
	for index, command := range p.Commands {
		if command.CreatesTarget {
			creation = CommandPlan{Description: p.Description, Commands: append([]CommandSpec{}, p.Commands[:index+1]...)}
			remainder = CommandPlan{Description: p.Description, Commands: append([]CommandSpec{}, p.Commands[index+1:]...)}
			return creation, remainder, true
		}
	}
	return CommandPlan{}, CommandPlan{}, false
}

// LogCommandWarnings emits a WARN log if a command succeeded with non-empty
// stderr and opted in.
func LogCommandWarnings(command *CommandSpec, output *CommandOutput) {
	panic("unported: mj-core/src/targets.rs log_command_warnings")
}

// CheckedCommandOutput fails the same way CommandPlan.Execute does for a
// non-zero exit status; kept as a shared helper so ExecuteConcurrent reports
// identical error text.
func CheckedCommandOutput(command *CommandSpec, output CommandOutput) (CommandOutput, error) {
	panic("unported: mj-core/src/targets.rs checked_command_output")
}

// CommandThreadPanicMessage describes a recovered panic payload for error
// context.
func CommandThreadPanicMessage(payload any) string {
	switch message := payload.(type) {
	case string:
		return message
	case error:
		return message.Error()
	default:
		return "non-string panic payload"
	}
}

// RepositorySpec is one repository of a project bundle.
type RepositorySpec struct {
	// URL is the network clone URL. Managed workspaces require a configured
	// remote.
	URL         *string  `json:"url"`
	PushURLs    []string `json:"push_urls,omitempty"`
	Destination string   `json:"destination"`
	GitRef      *string  `json:"git_ref"`
	// Reference is a read-only bare repository mounted into the target for
	// Git object reuse. A missing or unusable reference is only an
	// optimization miss. With HistoryCutoff, it is instead a Git bundle in
	// the target that holds only the cutoff history, and the checkout
	// fetches from it.
	Reference *string `json:"reference,omitempty"`
	// HistoryCutoff starts the checkout with these commits and their
	// ancestors only. It is created by `git init` and a fetch by object ID
	// instead of a clone, so it has no `origin` remote and no other refs.
	HistoryCutoff *HistoryCutoff `json:"history_cutoff,omitempty"`
}

// HistoryCutoff names the commits a checkout created without later history
// may hold, with their ancestors. See RepositorySpec.HistoryCutoff.
type HistoryCutoff struct {
	// Head is the commit the checkout starts at, detached.
	Head string `json:"head"`
	// Base is the diff base when it is not Head. The checkout keeps it
	// reachable as `refs/mj/base`, because it need not be an ancestor of
	// Head.
	Base *string `json:"base,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (c *HistoryCutoff) UnmarshalJSON(data []byte) error {
	type plain HistoryCutoff
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	var decoded plain
	if err := decoder.Decode(&decoded); err != nil {
		return err
	}
	*c = HistoryCutoff(decoded)
	return nil
}

// HistoryCutoffBaseRef is the ref the checkout keeps the base at, which is
// also the base's name in a clone-cache cutoff bundle.
const HistoryCutoffBaseRef = "refs/mj/base"

// HistoryCutoffBundleHeadRef is the head's name in a clone-cache cutoff
// bundle.
const HistoryCutoffBundleHeadRef = "refs/mj/head"

// Commits is Head, then the base when there is one.
func (c *HistoryCutoff) Commits() []string {
	if c.Base == nil {
		return []string{c.Head}
	}
	return []string{c.Head, *c.Base}
}

// Validate requires lowercase full object IDs and a base distinct from the
// head.
func (c *HistoryCutoff) Validate() error {
	panic("unported: mj-core/src/targets.rs HistoryCutoff::validate")
}

// ProjectBundleSpec is the repositories of one isolated project.
type ProjectBundleSpec struct {
	Primary      string           `json:"primary"`
	Repositories []RepositorySpec `json:"repositories"`
}

// MarshalJSON writes Repositories as [] when empty, as serde does.
func (s ProjectBundleSpec) MarshalJSON() ([]byte, error) {
	type plain ProjectBundleSpec
	if s.Repositories == nil {
		s.Repositories = []RepositorySpec{}
	}
	return json.Marshal(plain(s))
}

// Validate checks the bundle's paths, remotes and primary repository.
func (s *ProjectBundleSpec) Validate() error {
	panic("unported: mj-core/src/targets.rs ProjectBundleSpec::validate")
}

// TargetsPodmanWorkspaceStorageKind names a TargetsPodmanWorkspaceStorage
// variant (serde tag "kind", snake_case).
type TargetsPodmanWorkspaceStorageKind string

const (
	TargetsPodmanWorkspaceStoragePodmanVolume TargetsPodmanWorkspaceStorageKind = "podman_volume"
	// TargetsPodmanWorkspaceStorageHostHelper carries Root and Helper.
	TargetsPodmanWorkspaceStorageHostHelper TargetsPodmanWorkspaceStorageKind = "host_helper"
	// TargetsPodmanWorkspaceStorageContainerLayer is the default; the empty
	// kind means it too.
	TargetsPodmanWorkspaceStorageContainerLayer TargetsPodmanWorkspaceStorageKind = "container_layer"
)

// TargetsPodmanWorkspaceStorage is where a Podman target keeps a session's
// workspace (Rust: targets::PodmanWorkspaceStorage). The zero value is the
// Rust default, container_layer.
type TargetsPodmanWorkspaceStorage struct {
	Kind   TargetsPodmanWorkspaceStorageKind
	Root   string
	Helper []string
}

// EffectiveKind is Kind with the zero value read as container_layer.
func (s TargetsPodmanWorkspaceStorage) EffectiveKind() TargetsPodmanWorkspaceStorageKind {
	if s.Kind == "" {
		return TargetsPodmanWorkspaceStorageContainerLayer
	}
	return s.Kind
}

// MarshalJSON writes serde's internally tagged form.
func (s TargetsPodmanWorkspaceStorage) MarshalJSON() ([]byte, error) {
	kind := s.EffectiveKind()
	if kind == TargetsPodmanWorkspaceStorageHostHelper {
		helper := s.Helper
		if helper == nil {
			helper = []string{}
		}
		return json.Marshal(struct {
			Kind   TargetsPodmanWorkspaceStorageKind `json:"kind"`
			Root   string                            `json:"root"`
			Helper []string                          `json:"helper"`
		}{kind, s.Root, helper})
	}
	return json.Marshal(struct {
		Kind TargetsPodmanWorkspaceStorageKind `json:"kind"`
	}{kind})
}

// UnmarshalJSON reads serde's internally tagged form.
func (s *TargetsPodmanWorkspaceStorage) UnmarshalJSON(data []byte) error {
	var wire struct {
		Kind   TargetsPodmanWorkspaceStorageKind `json:"kind"`
		Root   *string                           `json:"root"`
		Helper *[]string                         `json:"helper"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	switch wire.Kind {
	case TargetsPodmanWorkspaceStoragePodmanVolume, TargetsPodmanWorkspaceStorageContainerLayer:
		*s = TargetsPodmanWorkspaceStorage{Kind: wire.Kind}
	case TargetsPodmanWorkspaceStorageHostHelper:
		if wire.Root == nil {
			return errors.New("missing field `root`")
		}
		if wire.Helper == nil {
			return errors.New("missing field `helper`")
		}
		*s = TargetsPodmanWorkspaceStorage{Kind: wire.Kind, Root: *wire.Root, Helper: *wire.Helper}
	default:
		return fmt.Errorf("unknown variant `%s` of PodmanWorkspaceStorage", wire.Kind)
	}
	return nil
}

// TargetsContainerTemplate is a container target's settings as an execution
// plan needs them (Rust: targets::ContainerTemplate).
type TargetsContainerTemplate struct {
	Image            string                        `json:"image"`
	PullPolicy       ImagePullPolicy               `json:"pull_policy"`
	ExtraRunArgs     []string                      `json:"extra_run_args"`
	WorkspaceStorage TargetsPodmanWorkspaceStorage `json:"workspace_storage"`
	// BuildCache holds per-target mbx build cache overrides, carried from the
	// user configuration so cache resolution can read them off a runtime
	// target.
	BuildCache *TargetBuildCache `json:"build_cache"`
}

// MarshalJSON writes ExtraRunArgs as [] when empty, as serde does.
func (t TargetsContainerTemplate) MarshalJSON() ([]byte, error) {
	type plain TargetsContainerTemplate
	if t.ExtraRunArgs == nil {
		t.ExtraRunArgs = []string{}
	}
	return json.Marshal(plain(t))
}

// UnmarshalJSON applies serde's defaults: a missing pull_policy is Auto.
func (t *TargetsContainerTemplate) UnmarshalJSON(data []byte) error {
	type plain TargetsContainerTemplate
	decoded := plain{PullPolicy: ImagePullPolicyAuto}
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*t = TargetsContainerTemplate(decoded)
	return nil
}

// Resolve is how fresh this target wants its image, with Auto read from the
// image reference. This is the freshness the background refresher acts on.
func (p ImagePullPolicy) Resolve(image string) ImagePullPolicy {
	if p != ImagePullPolicyAuto {
		return p
	}
	if imageIsDigestPinned(image) {
		return ImagePullPolicyMissing
	}
	if imageIsRemote(image) && imageUsesLatestTag(image) {
		return ImagePullPolicyNewer
	}
	return ImagePullPolicyMissing
}

// AtLaunch is how fresh a launch insists on being. Auto never pulls here:
// the daemon refreshes remote `:latest` images on its own schedule, so a
// session starts from the cached image instead of blocking a launch on a
// multi-gigabyte download. An explicit policy still means what it says.
func (p ImagePullPolicy) AtLaunch(image string) ImagePullPolicy {
	if p == ImagePullPolicyAuto {
		return ImagePullPolicyMissing
	}
	return p.Resolve(image)
}

// Describe is what this policy does to image, in the words a settings screen
// can show. Auto is derived rather than an alias, so it names the launch
// behavior and the background refresh it implies, from the same image
// reading Resolve uses.
func (p ImagePullPolicy) Describe(image string) string {
	switch p {
	case ImagePullPolicyAlways:
		return "Pull every launch"
	case ImagePullPolicyNewer:
		return "Pull when the registry is newer"
	case ImagePullPolicyMissing:
		return "Pull only if missing"
	case ImagePullPolicyNever:
		return "Never pull"
	}
	if p.Resolve(image) == ImagePullPolicyNewer {
		return "Pull if missing at launch; refresh :latest in background"
	}
	return "Pull if missing"
}

// PodmanValue is Podman's spelling of an already-resolved policy.
func (p ImagePullPolicy) PodmanValue() string {
	switch p {
	case ImagePullPolicyAlways:
		return "always"
	case ImagePullPolicyNewer:
		return "newer"
	case ImagePullPolicyMissing:
		return "missing"
	case ImagePullPolicyNever:
		return "never"
	}
	panic("auto pull policy must resolve")
}

// ImageHostKind names an ImageHost variant.
type ImageHostKind string

const (
	ImageHostLocalPodman    ImageHostKind = "LocalPodman"
	ImageHostLocalDocker    ImageHostKind = "LocalDocker"
	ImageHostAppleContainer ImageHostKind = "AppleContainer"
	// The SSH kinds carry SSH.
	ImageHostSshPodman         ImageHostKind = "SshPodman"
	ImageHostSshDocker         ImageHostKind = "SshDocker"
	ImageHostSshAppleContainer ImageHostKind = "SshAppleContainer"
	// ImageHostSshNitro is the `mj-worker microvm` engine on an EC2 host;
	// see MicrovmEngine.
	ImageHostSshNitro ImageHostKind = "SshNitro"
)

// ImageHost is where a background image refresh runs.
//
// The SSH form wraps commands the way provisioning does rather than the way
// the preflight probes do: a pull runs for minutes, and the probes'
// two-second keepalive would drop the connection underneath it.
type ImageHost struct {
	Kind ImageHostKind
	SSH  *SshTarget
}

// MicrovmEngine is the name of the Nitro Sandbox host engine,
// `mj-worker microvm`. It implements the subset of Podman's verbs, flags and
// output formats that Mjolnir drives over SSH, so most call sites treat it
// like `podman`.
const MicrovmEngine = "microvm"

// MicrovmEngineWorker is where the host engine's worker binary lives on an
// SSH host, relative to the login home. The daemon installs it there before
// the first engine call (see the controller's `ensure_microvm_engine`); this
// is the only place that names the path.
const MicrovmEngineWorker = ".local/share/hel/microvm/hel"

// EngineArgv is the argument vector that runs engine on its host, up to the
// verb.
//
// Podman, Docker and Apple's `container` are one program. The microVM engine
// is the installed worker run as root. `sudo` needs an absolute path and ssh
// quotes every argument, so the login shell cannot expand `$HOME`; the inner
// `sh` does it before `sudo` starts. Everything after this prefix reaches the
// engine unchanged, including stdin, stdout and the exit status.
func EngineArgv(engine string) []string {
	return EngineArgvWithEnv(engine, nil)
}

// EngineArgvWithEnv is EngineArgv for a command whose engine must inherit
// the environment variables names from the ssh command's environment. `sudo`
// resets the environment, so the microVM engine's prefix preserves exactly
// these; a value on the command line would be logged by `sudo` and visible
// in `ps`. Other engines inherit the whole environment and ignore names.
func EngineArgvWithEnv(engine string, names []string) []string {
	if engine == MicrovmEngine {
		return []string{"sh", "-c", fmt.Sprintf(`exec %s "$@"`, EngineShellWordsWithEnv(engine, names)), "mj-microvm"}
	}
	return []string{engine}
}

// EngineShellWords is engine as the words of a shell command line, for a
// script that runs it: `exec $(engine_shell_words) volume rm "$volume"`. The
// words contain `$HOME` for the microVM engine, so they belong inside a
// script that does not quote them away, never in an argument that ssh
// quotes.
func EngineShellWords(engine string) string {
	return EngineShellWordsWithEnv(engine, nil)
}

// EngineShellWordsWithEnv is EngineShellWords preserving the environment
// variables names through `sudo`; see EngineArgvWithEnv.
func EngineShellWordsWithEnv(engine string, names []string) string {
	if engine != MicrovmEngine {
		return engine
	}
	preserve := ""
	if len(names) > 0 {
		preserve = "--preserve-env=" + strings.Join(names, ",") + " "
	}
	return fmt.Sprintf(`sudo -n %s"$HOME/%s" %s`, preserve, MicrovmEngineWorker, MicrovmEngine)
}

// WithEngineArgv is args with a leading engine name expanded to the command
// that runs it.
func WithEngineArgv(args []string) []string {
	if len(args) > 0 && args[0] == MicrovmEngine {
		return append(EngineArgv(MicrovmEngine), args[1:]...)
	}
	return args
}

// EngineCommand is engine followed by args, as one command.
func EngineCommand(engine string, args []string) CommandSpec {
	argv := append(EngineArgv(engine), args...)
	return NewCommandSpec(argv[0], argv[1:])
}

// SSHEngineCommand is engine followed by args, run on the SSH host.
func SSHEngineCommand(class SshTrafficClass, ssh *SshTarget, engine string, args []string) CommandSpec {
	return SSHCommandOwned(class, ssh, append(EngineArgv(engine), args...))
}

// Engine is the container engine program this host runs.
func (h *ImageHost) Engine() string {
	switch h.Kind {
	case ImageHostLocalPodman, ImageHostSshPodman:
		return "podman"
	case ImageHostLocalDocker, ImageHostSshDocker:
		return "docker"
	case ImageHostAppleContainer, ImageHostSshAppleContainer:
		return "container"
	default:
		return MicrovmEngine
	}
}

// Label is how this host is named in a log line.
func (h *ImageHost) Label() string {
	panic("unported: mj-core/src/targets.rs ImageHost::label")
}

// Command runs args (an engine command line, starting with the engine's
// name) on this host.
func (h *ImageHost) Command(args []string, purpose string) CommandSpec {
	panic("unported: mj-core/src/targets.rs ImageHost::command")
}

// RefreshWhen is when a background refresh downloads an image.
//
// A host that already has the image is the common case, and most targets
// only need the copy to exist. Ordering matters: merging two targets that
// share an image takes the larger value, so one demanding target upgrades
// the pair.
type RefreshWhen int

const (
	// RefreshWhenWhenAbsent pulls only when the host has no copy (missing,
	// or auto on a fixed tag).
	RefreshWhenWhenAbsent RefreshWhen = iota
	// RefreshWhenAlways pulls every tick (always, or newer / auto on a
	// moving tag).
	RefreshWhenAlways
)

// ImageRefresh is the commands that keep one host's copy of one image
// current, away from any session launch. They run in this order, and only
// for that host.
type ImageRefresh struct {
	Host     ImageHost
	Image    string
	Platform *string
	// When says whether this image is downloaded only when the host lacks
	// it, or on every refresh.
	When RefreshWhen
	// ImageID reads the cached image id, so a pull that changed nothing
	// stays quiet. Run before and after the pull.
	ImageID CommandSpec
	Pull    CommandSpec
	// Prune removes dangling images and orphaned content only. Keep tagged
	// images ready for future sessions; `--all` would delete those preloaded
	// images.
	Prune CommandSpec
}

// ImageRefreshFor is the background refresh for one configured container
// target, or false when the target asked never to download the image in the
// background (Rust: image_refresh; renamed because the type ImageRefresh
// takes the name).
//
// `never` is the only opt-out. Every other policy at least wants the host to
// have a copy, so the daemon downloads a missing image once; `always` and
// `newer` keep the hourly pull they always had.
func ImageRefreshFor(host ImageHost, image string, platform *string, pullPolicy ImagePullPolicy) (ImageRefresh, bool) {
	panic("unported: mj-core/src/targets.rs image_refresh")
}

func imageIsDigestPinned(image string) bool {
	i := strings.LastIndexByte(image, '@')
	return i >= 0 && i+1 < len(image)
}

func imageIsRemote(image string) bool {
	return !strings.HasPrefix(image, "localhost/") && !strings.HasPrefix(image, "local/")
}

func imageUsesLatestTag(image string) bool {
	name, _, _ := strings.Cut(image, "@")
	finalComponent := name
	if i := strings.LastIndexByte(name, '/'); i >= 0 {
		finalComponent = name[i+1:]
	}
	return !strings.Contains(finalComponent, ":") || strings.HasSuffix(finalComponent, ":latest")
}

// SshTarget is one `ssh` destination with the arguments that reach it.
type SshTarget struct {
	Destination string   `json:"destination"`
	SSHArgs     []string `json:"ssh_args"`
}

// MarshalJSON writes SSHArgs as [] when empty, as serde does.
func (t SshTarget) MarshalJSON() ([]byte, error) {
	type plain SshTarget
	if t.SSHArgs == nil {
		t.SSHArgs = []string{}
	}
	return json.Marshal(plain(t))
}

// Clone copies the target, including its argument slice.
func (t *SshTarget) Clone() SshTarget {
	return SshTarget{Destination: t.Destination, SSHArgs: append([]string{}, t.SSHArgs...)}
}

// TargetsTargetTemplateKind names a TargetsTargetTemplate variant (serde tag
// "kind", snake_case).
type TargetsTargetTemplateKind string

const (
	TargetsTargetTemplateLocalBare TargetsTargetTemplateKind = "local_bare"
	// The local container kinds carry Container, serialized flattened
	// beside the tag.
	TargetsTargetTemplateLocalPodman    TargetsTargetTemplateKind = "local_podman"
	TargetsTargetTemplateLocalDocker    TargetsTargetTemplateKind = "local_docker"
	TargetsTargetTemplateAppleContainer TargetsTargetTemplateKind = "apple_container"
	// TargetsTargetTemplateSshBare carries SSH and WorkspacePrefix.
	TargetsTargetTemplateSshBare TargetsTargetTemplateKind = "ssh_bare"
	// The SSH container kinds carry SSH and Container.
	TargetsTargetTemplateSshPodman         TargetsTargetTemplateKind = "ssh_podman"
	TargetsTargetTemplateSshDocker         TargetsTargetTemplateKind = "ssh_docker"
	TargetsTargetTemplateSshAppleContainer TargetsTargetTemplateKind = "ssh_apple_container"
	TargetsTargetTemplateSshNitro          TargetsTargetTemplateKind = "ssh_nitro"
)

// TargetsTargetTemplate is a target as an execution plan needs it (Rust:
// targets::TargetTemplate). One struct with a kind field; the fields a kind
// does not carry are zero.
type TargetsTargetTemplate struct {
	Kind            TargetsTargetTemplateKind
	Container       *TargetsContainerTemplate
	SSH             *SshTarget
	WorkspacePrefix string
}

// MarshalJSON writes serde's internally tagged form.
func (t TargetsTargetTemplate) MarshalJSON() ([]byte, error) {
	panic("unported: mj-core/src/targets.rs TargetTemplate serialize")
}

// UnmarshalJSON reads serde's internally tagged form; a missing
// workspace_prefix of ssh_bare defaults to defaultSSHPrefix().
func (t *TargetsTargetTemplate) UnmarshalJSON(data []byte) error {
	panic("unported: mj-core/src/targets.rs TargetTemplate deserialize")
}

func defaultSSHPrefix() string {
	return ".local/share/hel/workspaces"
}

// TargetsPodmanWorkspaceLocatorKind names a TargetsPodmanWorkspaceLocator
// variant (serde tag "kind", snake_case).
type TargetsPodmanWorkspaceLocatorKind string

const (
	// TargetsPodmanWorkspaceLocatorContainerLayer is the default; the empty
	// kind means it too.
	TargetsPodmanWorkspaceLocatorContainerLayer TargetsPodmanWorkspaceLocatorKind = "container_layer"
	// TargetsPodmanWorkspaceLocatorVolume carries Name.
	TargetsPodmanWorkspaceLocatorVolume TargetsPodmanWorkspaceLocatorKind = "volume"
	// TargetsPodmanWorkspaceLocatorHostPath carries Path, Helper and
	// Resource.
	TargetsPodmanWorkspaceLocatorHostPath TargetsPodmanWorkspaceLocatorKind = "host_path"
)

// TargetsPodmanWorkspaceLocator is where one Podman session's workspace
// lives (Rust: targets::PodmanWorkspaceLocator). The zero value is the Rust
// default, container_layer.
type TargetsPodmanWorkspaceLocator struct {
	Kind     TargetsPodmanWorkspaceLocatorKind
	Name     string
	Path     string
	Helper   []string
	Resource string
}

// EffectiveKind is Kind with the zero value read as container_layer.
func (l TargetsPodmanWorkspaceLocator) EffectiveKind() TargetsPodmanWorkspaceLocatorKind {
	if l.Kind == "" {
		return TargetsPodmanWorkspaceLocatorContainerLayer
	}
	return l.Kind
}

// MarshalJSON writes serde's internally tagged form.
func (l TargetsPodmanWorkspaceLocator) MarshalJSON() ([]byte, error) {
	kind := l.EffectiveKind()
	switch kind {
	case TargetsPodmanWorkspaceLocatorVolume:
		return json.Marshal(struct {
			Kind TargetsPodmanWorkspaceLocatorKind `json:"kind"`
			Name string                            `json:"name"`
		}{kind, l.Name})
	case TargetsPodmanWorkspaceLocatorHostPath:
		helper := l.Helper
		if helper == nil {
			helper = []string{}
		}
		return json.Marshal(struct {
			Kind     TargetsPodmanWorkspaceLocatorKind `json:"kind"`
			Path     string                            `json:"path"`
			Helper   []string                          `json:"helper"`
			Resource string                            `json:"resource"`
		}{kind, l.Path, helper, l.Resource})
	default:
		return json.Marshal(struct {
			Kind TargetsPodmanWorkspaceLocatorKind `json:"kind"`
		}{kind})
	}
}

// UnmarshalJSON reads serde's internally tagged form.
func (l *TargetsPodmanWorkspaceLocator) UnmarshalJSON(data []byte) error {
	panic("unported: mj-core/src/targets.rs PodmanWorkspaceLocator deserialize")
}

// TargetsTargetLocatorKind names a TargetsTargetLocator variant (serde tag
// "kind", snake_case).
type TargetsTargetLocatorKind string

const (
	// TargetsTargetLocatorLocalBare carries WorkerRoot.
	TargetsTargetLocatorLocalBare TargetsTargetLocatorKind = "local_bare"
	// TargetsTargetLocatorLocalPodman carries ContainerID, WorkspaceStorage
	// and BorrowedFrom.
	TargetsTargetLocatorLocalPodman TargetsTargetLocatorKind = "local_podman"
	// TargetsTargetLocatorLocalDocker and AppleContainer carry ContainerID
	// and BorrowedFrom.
	TargetsTargetLocatorLocalDocker    TargetsTargetLocatorKind = "local_docker"
	TargetsTargetLocatorAppleContainer TargetsTargetLocatorKind = "apple_container"
	// TargetsTargetLocatorAwsEc2 carries Profile, Region, InstanceID, SSH and
	// Workspace.
	TargetsTargetLocatorAwsEc2 TargetsTargetLocatorKind = "aws_ec2"
	// TargetsTargetLocatorSshBare carries SSH, Workspace and WorkerID.
	TargetsTargetLocatorSshBare TargetsTargetLocatorKind = "ssh_bare"
	// TargetsTargetLocatorSshPodman carries SSH, ContainerID,
	// WorkspaceStorage and BorrowedFrom.
	TargetsTargetLocatorSshPodman TargetsTargetLocatorKind = "ssh_podman"
	// The other SSH container kinds carry SSH, ContainerID and BorrowedFrom.
	TargetsTargetLocatorSshDocker         TargetsTargetLocatorKind = "ssh_docker"
	TargetsTargetLocatorSshAppleContainer TargetsTargetLocatorKind = "ssh_apple_container"
	TargetsTargetLocatorSshNitro          TargetsTargetLocatorKind = "ssh_nitro"
)

// TargetsTargetLocator is a provisioned target as an execution plan needs it
// (Rust: targets::TargetLocator). One struct with a kind field; the fields a
// kind does not carry are zero.
type TargetsTargetLocator struct {
	Kind       TargetsTargetLocatorKind
	WorkerRoot string
	// ContainerID names the container or sandbox.
	ContainerID      string
	WorkspaceStorage TargetsPodmanWorkspaceLocator
	// BorrowedFrom is the session that owns the container when this locator
	// is a sub-agent child borrowing its parent's container; nil when the
	// session owns the container itself.
	BorrowedFrom *string
	Profile      string
	Region       string
	InstanceID   string
	SSH          *SshTarget
	Workspace    string
	// WorkerID: a borrowed-target child has its own worker identity in the
	// parent's workspace.
	WorkerID *string
}

// MarshalJSON writes serde's internally tagged form, with each kind's fields
// in declaration order.
func (l TargetsTargetLocator) MarshalJSON() ([]byte, error) {
	panic("unported: mj-core/src/targets.rs TargetLocator serialize")
}

// UnmarshalJSON reads serde's internally tagged form.
func (l *TargetsTargetLocator) UnmarshalJSON(data []byte) error {
	panic("unported: mj-core/src/targets.rs TargetLocator deserialize")
}

// ContainerEngine is the engine this template's target runs, when it runs
// one.
func (t *TargetsTargetTemplate) ContainerEngine() (string, bool) {
	switch t.Kind {
	case TargetsTargetTemplateLocalPodman, TargetsTargetTemplateSshPodman:
		return "podman", true
	case TargetsTargetTemplateLocalDocker, TargetsTargetTemplateSshDocker:
		return "docker", true
	case TargetsTargetTemplateAppleContainer, TargetsTargetTemplateSshAppleContainer:
		return "container", true
	case TargetsTargetTemplateSshNitro:
		return MicrovmEngine, true
	}
	return "", false
}

// ImageHost is the host that downloads this target's image, with the
// container settings that name the image. false for targets that run no
// image.
func (t *TargetsTargetTemplate) ImageHost() (ImageHost, *TargetsContainerTemplate, bool) {
	panic("unported: mj-core/src/targets.rs TargetTemplate::image_host")
}

// CPUAccounting: container placement includes borrowed workers: each can
// survive its owner moving.
func (l *TargetsTargetLocator) CPUAccounting() CpuAccountingMode {
	if _, ok := l.ContainerEngine(); ok {
		return CpuAccountingModeContainer
	}
	return CpuAccountingModeProcessTree
}

// KindName is the target kind spelling shared with the configured
// TargetTemplate.
func (l *TargetsTargetLocator) KindName() string {
	return strings.ReplaceAll(string(l.Kind), "_", "-")
}

// ContainerEngine is the engine this locator's target runs, when it runs
// one.
func (l *TargetsTargetLocator) ContainerEngine() (string, bool) {
	switch l.Kind {
	case TargetsTargetLocatorLocalPodman, TargetsTargetLocatorSshPodman:
		return "podman", true
	case TargetsTargetLocatorLocalDocker, TargetsTargetLocatorSshDocker:
		return "docker", true
	case TargetsTargetLocatorAppleContainer, TargetsTargetLocatorSshAppleContainer:
		return "container", true
	case TargetsTargetLocatorSshNitro:
		return MicrovmEngine, true
	}
	return "", false
}

// TargetRecoveryPlan is the commands and identity needed to bring a stopped
// managed target back online. Only runtimes whose stopped resources retain
// their durable files provide one; callers leave every other target kind
// alone.
type TargetRecoveryPlan struct {
	Exists    CommandSpec
	Inspect   CommandSpec
	Start     CommandSpec
	SessionID string
}

// TargetRecoveryOutcome is what recovering a target did.
type TargetRecoveryOutcome int

const (
	TargetRecoveryOutcomeNotRequired TargetRecoveryOutcome = iota
	TargetRecoveryOutcomeMissing
	TargetRecoveryOutcomeAlreadyRunning
	TargetRecoveryOutcomeStarted
)

// ResourceName is the container and volume name for a session.
func ResourceName(sessionID string) (string, error) {
	panic("unported: mj-core/src/targets.rs resource_name")
}

// MoveResourceName: resource generations let a Move retain its source while
// provisioning the destination.
func MoveResourceName(sessionID, operationID string) (string, error) {
	panic("unported: mj-core/src/targets.rs move_resource_name")
}

// ResourceNameBelongsTo reports whether name is the session's resource name
// or one of its Move generations.
func ResourceNameBelongsTo(name, sessionID string) (bool, error) {
	panic("unported: mj-core/src/targets.rs resource_name_belongs_to")
}

// PodmanWorkspaceLocatorFor is the workspace locator for a session's new
// container (Rust: podman_workspace_locator; renamed because the type takes
// the name).
func PodmanWorkspaceLocatorFor(template *TargetsContainerTemplate, sessionID string) (TargetsPodmanWorkspaceLocator, error) {
	name, err := ResourceName(sessionID)
	if err != nil {
		return TargetsPodmanWorkspaceLocator{}, err
	}
	return PodmanWorkspaceLocatorNamed(template, name)
}

// PodmanWorkspaceLocatorNamed is the workspace locator for the container
// resource name.
func PodmanWorkspaceLocatorNamed(template *TargetsContainerTemplate, name string) (TargetsPodmanWorkspaceLocator, error) {
	panic("unported: mj-core/src/targets.rs podman_workspace_locator_named")
}

// ContainerWorkspaceRoot is the in-container workspace root a session's
// repositories live under.
//
// recorded is the session record's `container_workspace`. A session whose
// container predates per-session workspaces has none and keeps the legacy
// shared `/workspace`; every session created since records
// `/workspace/<session id>`, so a build cache shared by every container on a
// host never sees two checkouts of one project at the same absolute path.
func ContainerWorkspaceRoot(recorded *string) string {
	panic("unported: mj-core/src/targets.rs container_workspace_root")
}

// NewContainerWorkspace is the per-session container workspace recorded for
// a session created now.
func NewContainerWorkspace(sessionID string) (string, error) {
	panic("unported: mj-core/src/targets.rs new_container_workspace")
}

// AWSWorkspace is the workspace directory an EC2 session owns in its login
// home.
//
// EC2 instances are provisioned per session, so the path is decided by the
// session id alone and is the same whether it is read from a target template
// or rebuilt from a stored locator.
func AWSWorkspace(sessionID string) string {
	return ".local/share/hel/workspaces/" + sessionID
}

// WorkspaceFor is the workspace an SSH bare session owns.
func WorkspaceFor(template *TargetsTargetTemplate, sessionID string) (string, error) {
	panic("unported: mj-core/src/targets.rs workspace_for")
}

// CommandOnLocator wraps an argv vector for execution at a provisioned
// session target.
func CommandOnLocator(class SshTrafficClass, locator *TargetsTargetLocator, sessionID string, args []string, purpose string) (CommandSpec, error) {
	if err := VerifyLocator(locator, sessionID); err != nil {
		return CommandSpec{}, err
	}
	if len(args) == 0 {
		return CommandSpec{}, errors.New("target command must not be empty")
	}
	return LocatorCommand(class, locator, args).WithPurpose(purpose), nil
}

// LocatorCommand wraps a non-empty argv vector for execution at a target,
// without checking that the locator belongs to a particular session. Every
// per-target command is built here, so each target kind is spelled out once.
func LocatorCommand(class SshTrafficClass, locator *TargetsTargetLocator, args []string) CommandSpec {
	panic("unported: mj-core/src/targets.rs locator_command")
}

// WorkerRoot is the worker root of a session on its target.
func WorkerRoot(locator *TargetsTargetLocator, sessionID string) (string, error) {
	panic("unported: mj-core/src/targets.rs worker_root")
}

// ContainerExec runs args in a container with stdin attached.
func ContainerExec(engine, containerID string, args []string) CommandSpec {
	return EngineCommand(engine, append([]string{"exec", "-i", containerID}, args...))
}

// DrainedContainerExec is a container exec run on a remote host over SSH,
// through the output drain below. Only SSH targets need it: there the drain
// runs on the remote host and outlives a cancelled local ssh, whereas a
// local target's cancellation kills the whole process group, drain
// included, and a local controller (Windows) may have no POSIX shell to run
// it.
func DrainedContainerExec(engine, containerID string, args []string) CommandSpec {
	inner := ContainerExec(engine, containerID, args)
	wrapped := append([]string{"-c", containerExecDrain, "exec-drain", inner.Program}, inner.Args...)
	return NewCommandSpec("sh", wrapped)
}

// Podman's attached client removes its exec record only on normal exit. A
// broken SSH output pipe kills it with SIGPIPE and leaks that record. The
// host-side POSIX drain preserved both streams and status and prevented
// leaks in all 40 cancellation trials on morannon (2026-11-07). Keep
// draining after EPIPE. An infinite command retains its temporary status
// directory until it ends.
const containerExecDrain = `status_dir=$(mktemp -d) || exit 125
trap 'rm -rf "$status_dir"' 0
drain() {
    trap '' PIPE
    cat
    cat >/dev/null
}
{
    {
        "$@"
        printf '%s\n' "$?" >"$status_dir/status"
    } 2>&1 1>&3 | drain >&2
} 3>&1 | drain
if [ -r "$status_dir/status" ]; then
    read -r status <"$status_dir/status"
    exit "$status"
fi
exit 125
`
