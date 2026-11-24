// Port of mj-worker-staging/src/worker_binary/process.rs.

package workerstaging

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// StopWorker stops the detached worker daemon at workerRoot without
// deleting its files.
//
// The script signals the worker's process group so a wedged ACP child dies
// with it. Checkpoint then restarts the daemon against the same relay root.
func StopWorker(owner *store.WorkerPermit, executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) error {
	return runOwnedWorkerStop(owner, executor, locator, workerRoot, StopWorkerCommand)
}

// KillRetiredWorker kills a retired worker daemon at workerRoot at once,
// without the TERM grace StopWorker allows. Only for retirement, where the
// root is removed next and nothing in it needs an orderly shutdown.
func KillRetiredWorker(owner *store.WorkerPermit, executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) error {
	return runOwnedWorkerStop(owner, executor, locator, workerRoot, func(locator *core.TargetsTargetLocator, root string) targets.ControlCommand {
		return targets.KillWorkerDaemonScript(root).IntoCommand(func(script string) core.CommandSpec {
			return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
				WithPurpose("kill retired Mjolnir worker daemon")
		})
	})
}

func runOwnedWorkerStop(owner *store.WorkerPermit, executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, command func(locator *core.TargetsTargetLocator, root string) targets.ControlCommand) error {
	root, err := core.WorkerRoot(locator, owner.SessionID())
	if err != nil {
		return err
	}
	if root != workerRoot {
		return errors.New("worker owner does not cover the selected root")
	}
	spec := command(locator, workerRoot).UnstagedCommand()
	output, err := RunControlCommand(executor, locator, owner.SessionID(), &spec)
	if err != nil {
		return err
	}
	_, err = core.CheckedCommandOutput(&spec, output)
	return err
}

// RunControlCommand runs a control command (a script from
// targets.WorkerDaemon*Script or targets.StopWorkerDaemonScript), staging
// the control worker only when the root cannot serve it.
//
// The script picks, on the target and against the file it is about to run,
// the root's own `hel` or a staged `hel-control*` that proves it has the
// subcommand, and exits 78 when none does. The daemon keeps no record of
// that decision. On 78 this stages the control worker (a hard link to the
// current build's worker-store entry under the stable name
// targets.ControlWorkerPath; the worker never executes it, so this is not a
// worker swap and needs no swap admission) and reruns the command once. A
// second 78 is an error: the staged build does not serve the subcommand. A
// binary that passed the probe and then exits 2 lost a race with an install
// between probe and exec; that surfaces as the command's ordinary failure
// and the caller retries it as it would any failed control command.
//
// Deadlines: the command runs on the caller's executor, deadline included.
// The staging, meaning publication of the build to the store plus the link
// (and any wait behind another publication of the same digest), runs
// without the caller's deadline and keeps its cancellation, because it is
// the work a stop's timeout does not name. The untimed scope pauses that
// deadline so the rerun retains its budget.
func RunControlCommand(caller core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, command *core.CommandSpec) (core.CommandOutput, error) {
	return targets.RunStagedControlCommand(caller, ControlWorker{}, locator, sessionID, command)
}

// ControlWorker stages the current build's worker as a root's control
// worker. Every targets.WorkerControlPlan is executed with this.
type ControlWorker struct{}

func (ControlWorker) StageControlWorker(caller core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, root string) error {
	return stageControlWorker(caller, locator, sessionID, root)
}

func stageControlWorker(caller core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, root string) error {
	untimed := core.NewWithoutDeadline(caller)
	defer untimed.End()
	worker, err := WorkerInstallForTarget(locator, untimed)
	if err != nil {
		return fmt.Errorf("the current worker cannot be installed for %s, so its worker cannot be stopped or probed: %w", root, err)
	}
	if err := InstallWorkerBinary(untimed, locator, sessionID, worker, targets.ControlWorkerName); err != nil {
		return fmt.Errorf("install the current worker for %s: %w", root, err)
	}
	return nil
}

// ProbeWorkerLiveness probes worker liveness, staging the control worker if
// the root needs it.
func ProbeWorkerLiveness(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, workerRoot string) (core.CommandOutput, error) {
	command := WorkerLivenessCommand(locator, workerRoot).UnstagedCommand()
	return RunControlCommand(executor, locator, sessionID, &command)
}

// StopWorkerAfterTargetRecovery restores a stopped Podman target before
// signaling its worker. Checkpoint recovery uses this instead of assuming
// every persisted target is running. ctx carries the session's worker
// permit (Rust: worker_lifecycle's task-local).
func StopWorkerAfterTargetRecovery(ctx context.Context, executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, workerRoot string) error {
	owner, err := store.Require(ctx, sessionID)
	if err != nil {
		return err
	}
	target, err := targets.TargetRecoveryPlan(locator, sessionID)
	if err != nil {
		return err
	}
	if _, err := targets.EnsureRecoveryTargetRunning(executor, target); err != nil {
		return fmt.Errorf("restore Mjolnir worker target: %w", err)
	}
	return StopWorker(owner, executor, locator, workerRoot)
}

func StopWorkerCommand(locator *core.TargetsTargetLocator, workerRoot string) targets.ControlCommand {
	return targets.StopWorkerDaemonScript(workerRoot).IntoCommand(func(script string) core.CommandSpec {
		return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
			WithPurpose("stop Mjolnir worker daemon")
	})
}

func WorkerLivenessCommand(locator *core.TargetsTargetLocator, workerRoot string) targets.ControlCommand {
	return targets.WorkerDaemonLivenessScript(workerRoot).IntoCommand(func(script string) core.CommandSpec {
		return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
			WithPurpose("probe Mjolnir worker daemon liveness")
	})
}

// WorkerRecovery is whether a session's worker root holds anything a worker
// could recover.
type WorkerRecovery int

const (
	// WorkerRecoveryRecoverable: the root has relay state or a relay
	// journal.
	WorkerRecoveryRecoverable WorkerRecovery = iota
	// WorkerRecoveryNothingToRecover: the root is absent, or exists without
	// relay state or a journal: no worker ever got as far as its durable
	// relay there. A checkpoint-only worker refuses such a root
	// ("checkpoint recovery requires existing relay state"), so there is
	// nothing to synchronize.
	WorkerRecoveryNothingToRecover
	// WorkerRecoveryUnknown: the probe itself failed (SSH, Podman, a stopped
	// container, a lookup error), so the root may hold state. Callers must
	// treat this as recoverable.
	WorkerRecoveryUnknown
)

// String is the Rust variant name (Debug).
func (r WorkerRecovery) String() string {
	switch r {
	case WorkerRecoveryRecoverable:
		return "Recoverable"
	case WorkerRecoveryNothingToRecover:
		return "NothingToRecover"
	default:
		return "Unknown"
	}
}

// WorkerRecoveryCommand reports `recoverable`, `nothing` (present without
// relay state), or `absent`. Lookup failures exit nonzero; Resume needs
// absence, not just no relay state.
func WorkerRecoveryCommand(locator *core.TargetsTargetLocator, workerRoot string) core.CommandSpec {
	return directoryProbeCommand(locator, workerRoot, true).WithPurpose("probe Mjolnir worker root")
}

// DirectoryPresenceCommand: positive directory presence/absence; lookup
// failures remain errors.
func DirectoryPresenceCommand(locator *core.TargetsTargetLocator, directory string) core.CommandSpec {
	return directoryProbeCommand(locator, directory, false).WithPurpose("probe retained environment directory")
}

const findListedEntry = `hel_lists() {
  while IFS= read -r hel_entry; do
    case "$hel_entry" in "$2"|"$3") return 0 ;; esac
  done <<MJ_ENTRIES
$1
MJ_ENTRIES
  return 1
};`

// provePathAbsent: a failed file test is not absence. Climb to a readable
// ancestor: if its listing contains the next component, the failed lookup is
// Unknown. Only a readable listing without that component proves absence.
func provePathAbsent(path, answer string) string {
	var missing strings.Builder
	for _, ancestor := range pathAncestors(path) {
		name, ok := pathFileName(ancestor)
		if !ok {
			continue
		}
		parent, ok := pathParent(ancestor)
		if !ok || parent == "" {
			parent = "."
		}
		fmt.Fprintf(&missing, "if entries=$(ls -1A %s); then if hel_lists \"$entries\" %s %s; then exit 1; fi; printf '%%s\\n' %s; exit 0; fi; ",
			core.PosixQuote(parent), core.PosixQuote(name), core.PosixQuote(name), core.PosixQuote(answer))
	}
	return missing.String()
}

func directoryProbeCommand(locator *core.TargetsTargetLocator, directory string, relayContents bool) core.CommandSpec {
	missing := provePathAbsent(directory, "absent")
	present := "printf 'present\\n';"
	if relayContents {
		present = "if hel_lists \"$entries\" resume-checkout-recovery.json resume-checkout-recovery.json; then printf 'checkout-partial\\n'; " +
			"elif hel_lists \"$entries\" resume-parent-recovery.json resume-parent-recovery.json; then printf 'partial\\n'; " +
			"elif hel_lists \"$entries\" " + core.PosixQuote(core.RelayStateFile) + " " + core.PosixQuote(core.RelayJournalDir) + "; then " +
			"printf 'recoverable\\n'; else printf 'nothing\\n'; fi;"
	}
	script := "root=" + core.PosixQuote(directory) + "; " +
		findListedEntry + " " +
		"if [ -d \"$root\" ]; then " +
		"entries=$(ls -1A \"$root\") || exit 1; " + present + " " +
		"else " + missing + " exit 1; fi"
	return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script})
}

// WorkerRecoveryOf is Rust's `worker_recovery` (renamed: Go cannot have a
// function and a type both named WorkerRecovery).
//
// It asks the target whether workerRoot holds anything to recover. Only the
// install step creates a child's root and only its cleanup removes it, and
// only a worker that reached its durable relay writes relay state there.
//
// A worker that is still starting has not written relay state yet, so an
// empty root counts as nothing to recover only when no worker for it is
// running afterwards. Liveness is checked after the listing, so a worker
// that started in between is seen.
func WorkerRecoveryOf(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, workerRoot string) WorkerRecovery {
	answer := func(command core.CommandSpec) (string, bool) {
		output, err := executor.Execute(&command)
		if err != nil || output.Status != 0 {
			return "", false
		}
		return strings.TrimSpace(lossyText(output.Stdout)), true
	}
	recovery, ok := answer(WorkerRecoveryCommand(locator, workerRoot))
	if !ok {
		return WorkerRecoveryUnknown
	}
	switch recovery {
	case "recoverable", "partial", "checkout-partial":
		return WorkerRecoveryRecoverable
	case "nothing", "absent":
	default:
		return WorkerRecoveryUnknown
	}
	// Unknown keeps the conservative outcome; the log says why.
	liveness, err := func() (string, error) {
		output, err := ProbeWorkerLiveness(executor, locator, sessionID, workerRoot)
		if err != nil {
			return "", err
		}
		if output.Status != 0 {
			return "", fmt.Errorf("worker liveness probe exited with status %d: %s", output.Status, strings.TrimSpace(lossyText(output.Stderr)))
		}
		return strings.TrimSpace(lossyText(output.Stdout)), nil
	}()
	switch {
	case err != nil:
		slog.Warn("could not probe worker liveness; recovery stays undetermined",
			"session_id", sessionID, "worker_root", workerRoot, "error", err.Error())
		return WorkerRecoveryUnknown
	case liveness == "dead":
		return WorkerRecoveryNothingToRecover
	case liveness == "alive" || liveness == "starting":
		return WorkerRecoveryRecoverable
	default:
		slog.Warn("worker liveness probe gave an unrecognised answer; recovery stays undetermined",
			"session_id", sessionID, "worker_root", workerRoot, "answer", liveness)
		return WorkerRecoveryUnknown
	}
}

// WorkerExitRecordCommand prints the worker's exit record, or JSON null
// when it left none. Reads only, so it works on a target whose disk is
// full.
func WorkerExitRecordCommand(locator *core.TargetsTargetLocator, workerRoot string) core.CommandSpec {
	path := workerRoot + "/" + core.WorkerExitFile
	missing := provePathAbsent(path, "null")
	script := "path=" + core.PosixQuote(path) + "; " + findListedEntry + " " +
		"if [ -e \"$path\" ]; then exec cat -- \"$path\"; fi; " +
		missing + " exit 1"
	return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
		WithPurpose("read the Mjolnir worker's exit record")
}

// RecordedExitReason is the reason a worker recorded for its exit, and when
// (epoch seconds, nil when unknown), from the output of
// WorkerExitRecordCommand. False when there is no readable record: a
// reserved record that says `null` means the worker left no reason.
func RecordedExitReason(output []byte) (reason string, at *uint64, ok bool) {
	record, err := ReadWorkerExitRecord(output)
	if err != nil || record == nil {
		return "", nil, false
	}
	if timestamp, ok := record.Timestamp(); ok {
		at = &timestamp
	}
	return record.Reason, at, true
}

// ReadWorkerExitRecord decodes an exit record; nil for a JSON null.
func ReadWorkerExitRecord(output []byte) (*WorkerExitRecord, error) {
	var record *WorkerExitRecord
	if err := json.Unmarshal(output, &record); err != nil {
		return nil, fmt.Errorf("decode the worker's exit record: %w", err)
	}
	return record, nil
}

func StartWorker(owner *store.WorkerPermit, executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, github *core.GithubConfig) error {
	root, err := core.WorkerRoot(locator, owner.SessionID())
	if err != nil {
		return err
	}
	if root != workerRoot {
		return errors.New("worker owner does not cover the selected root")
	}
	session, err := store.ReadDurableSessionRecord(owner.SessionID())
	if err != nil {
		return err
	}
	if session == nil {
		return errors.New("session disappeared before worker launch admission")
	}
	target := session.Target
	if target == nil {
		return errors.New("worker has no durable placement")
	}
	config, err := core.ConfigLoad()
	if err != nil {
		return err
	}
	backend, err := targets.BackendLocator(target, session, &config)
	if err != nil {
		return err
	}
	if !backend.Equal(*locator) {
		return errors.New("worker launch does not match its admitted placement")
	}
	if err := store.AdmitEnvironmentWorker(owner.SessionID(), target); err != nil {
		return err
	}
	if github.SessionTokenPath != nil {
		session, err := store.ReadDurableSessionRecord(owner.SessionID())
		if err != nil {
			return err
		}
		if session == nil {
			return errors.New("session disappeared before installing its GitHub token")
		}
		if !session.NoGithubCredentials {
			if err := InstallConfiguredSessionGithubToken(github, executor, locator, workerRoot); err != nil {
				return err
			}
		}
	}
	_, err = targets.ExecuteChecked(executor, StartWorkerCommand(locator, workerRoot))
	return err
}

// StartWorkerDurably: every accepted boot is protected by the durable
// replacement state machine. This includes provisioning and resume, not
// just automatic upgrades. ctx is the context the owner's restart records
// are written under (Rust: worker_lifecycle's task-local).
func StartWorkerDurably(ctx context.Context, owner *store.WorkerPermit, target core.TargetLocator, executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, github *core.GithubConfig) error {
	if err := owner.VerifyTarget(target); err != nil {
		return err
	}
	id := owner.SessionID()
	intent, err := store.LoadWorkerRestart(id)
	if err != nil {
		return err
	}
	if intent != nil && intent.OperationID != owner.OperationID() {
		output, err := ProbeWorkerLiveness(executor, locator, id, workerRoot)
		if err != nil {
			return err
		}
		if output.Status != 0 || strings.TrimSpace(lossyText(output.Stdout)) != "dead" {
			return errors.New("another worker replacement is still booting")
		}
		if err := owner.SettleDeadRestart(ctx, target); err != nil {
			return err
		}
	}
	if intent, err = store.LoadWorkerRestart(id); err != nil {
		return err
	}
	if intent == nil {
		if err := owner.BeginRestart(ctx, target, ""); err != nil {
			return err
		}
	}
	if intent, err = store.LoadWorkerRestart(id); err != nil {
		return err
	}
	if intent == nil {
		return errors.New("worker boot intent disappeared")
	}
	if intent.OperationID != owner.OperationID() {
		return errors.New("another worker replacement is still pending")
	}
	phase, err := store.WorkerRestartPhaseOf(id)
	if err != nil {
		return err
	}
	if phase != nil && *phase == store.WorkerRestartPhasePrepared {
		if err := store.AdvanceWorkerRestart(ctx, id, owner.OperationID(), store.WorkerRestartPhaseSwapping); err != nil {
			return err
		}
	}
	if phase, err = store.WorkerRestartPhaseOf(id); err != nil {
		return err
	}
	if phase == nil || *phase != store.WorkerRestartPhaseSwapping {
		return errors.New("worker replacement has already launched")
	}
	if err := StartWorker(owner, executor, locator, workerRoot, github); err != nil {
		return err
	}
	return store.AdvanceWorkerRestart(ctx, id, owner.OperationID(), store.WorkerRestartPhaseAwaitingReadiness)
}

func StartWorkerCommand(locator *core.TargetsTargetLocator, workerRoot string) core.CommandSpec {
	binary := workerRoot + "/hel"
	config := workerRoot + "/launch.json"
	// A launch attempt cannot touch the incumbent's diagnostics or socket.
	// The worker installs worker.log only after claiming worker.lock.
	attemptLog := "hel_launch_log=$(mktemp " + core.PosixQuote(workerRoot) + "/worker-launch.XXXXXXXX) || exit $?; "
	run := core.JoinRemoteCommand([]string{binary, "worker", "run", "--root", workerRoot, "--config", config})
	detachedScript := attemptLog + "nohup " + run + " >\"$hel_launch_log\" 2>&1 </dev/null &"
	// Even a loader failure is retained in the attempt-specific log.
	execScript := attemptLog + "exec " + run + " >\"$hel_launch_log\" 2>&1"
	var command core.CommandSpec
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		// The worker this launches outlives the launch, so completion must
		// not signal the process group it inherited from this shell.
		command = core.NewCommandSpec("sh", []string{"-c", detachedScript})
		command.Detaches = true
	case core.TargetsTargetLocatorLocalPodman:
		command = core.NewCommandSpec("podman", []string{"exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	case core.TargetsTargetLocatorLocalDocker:
		command = core.NewCommandSpec("docker", []string{"exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	case core.TargetsTargetLocatorAppleContainer:
		command = core.NewCommandSpec("container", []string{"exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		command = core.SSHCommand(core.SshTrafficClassControl, locator.SSH, []string{"sh", "-c", detachedScript})
	case core.TargetsTargetLocatorSshPodman:
		command = core.SSHCommand(core.SshTrafficClassControl, locator.SSH,
			[]string{"podman", "exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	case core.TargetsTargetLocatorSshDocker:
		command = core.SSHCommand(core.SshTrafficClassControl, locator.SSH,
			[]string{"docker", "exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	case core.TargetsTargetLocatorSshAppleContainer:
		command = core.SSHCommand(core.SshTrafficClassControl, locator.SSH,
			[]string{"container", "exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	case core.TargetsTargetLocatorSshNitro:
		command = core.SSHEngineCommand(core.SshTrafficClassControl, locator.SSH, core.MicrovmEngine,
			[]string{"exec", "--detach", locator.ContainerID, "sh", "-c", execScript})
	}
	// Everything before this moves data into the target and reports as
	// Sync. Start begins here, with the daemon launch.
	return command.WithPurpose("start detached Mjolnir worker").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageStarting})
}

// startupFailureContext is anyhow's `error.context(failure)` for a
// WorkerHarnessStartupFailure: it reads as "message: error", and errors.As
// finds the failure.
type startupFailureContext struct {
	failure core.WorkerHarnessStartupFailure
	err     error
}

func (e *startupFailureContext) Error() string { return e.failure.Message + ": " + e.err.Error() }
func (e *startupFailureContext) Unwrap() error { return e.err }

func (e *startupFailureContext) As(target any) bool {
	switch target := target.(type) {
	case *core.WorkerHarnessStartupFailure:
		*target = e.failure
		return true
	case **core.WorkerHarnessStartupFailure:
		failure := e.failure
		*target = &failure
		return true
	}
	return false
}

// WorkerProbeDiagnosis enriches an opaque handshake failure by running the
// installed worker binary directly in the target. This surfaces loader
// errors (for example a glibc-linked worker inside an older-glibc
// container) that a detached start swallows.
func WorkerProbeDiagnosis(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, err error) error {
	if failure, ok := WorkerBinaryProbeFailure(executor, locator, workerRoot); ok {
		err = fmt.Errorf("%s: %w", failure, err)
	}
	probe, probeErr := DiagnoseWorkerReadOnly(executor, locator, workerRoot)
	if probeErr != nil {
		return fmt.Errorf("the worker could not be probed: %s: %w", probeErr, err)
	}
	diagnostic := probe.String()
	if probe.Exit != nil && probe.Exit.HarnessStartupFailure != nil {
		// The worker may have died after accepting the initial relay
		// connection. Preserve its startup scope with the probe text.
		return &startupFailureContext{
			failure: core.WorkerHarnessStartupFailure{Message: diagnostic, Kind: probe.Exit.HarnessStartupFailure.Kind},
			err:     err,
		}
	}
	return fmt.Errorf("%s: %w", diagnostic, err)
}

// WorkerBinaryProbeFailure is why the installed worker binary fails to run
// in the target; false when it runs.
func WorkerBinaryProbeFailure(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) (string, bool) {
	binary := workerRoot + "/hel"
	command := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{binary, "--version"}).
		WithPurpose("probe installed worker binary")
	output, err := executor.Execute(&command)
	if err != nil {
		return "worker probe failed: " + err.Error(), true
	}
	if output.Status == 0 {
		return "", false
	}
	stderr := strings.TrimSpace(lossyText(output.Stderr))
	stdout := strings.TrimSpace(lossyText(output.Stdout))
	detail := "the process exited unsuccessfully without output"
	if stderr != "" {
		detail = stderr
	} else if stdout != "" {
		detail = stdout
	}
	return fmt.Sprintf("worker binary %s fails to run in the target: %s; "+
		"if this is a loader/glibc error, provide a musl worker "+
		"(cargo build --release --target <arch>-unknown-linux-musl "+
		"-p brokk-mj-worker --bin mj-worker, "+
		"or set MJ_WORKER_BINARY/MJ_WORKER_DIR)", binary, detail), true
}

// WorkerProbe is what one probe of a starting or dead worker found.
//
// The probe script prints exactly this document: the worker's own startup
// and exit records, inserted unchanged, and the live processes for its
// root. All three facts come from one command, because on a container or
// SSH target every probe costs a round trip.
type WorkerProbe struct {
	// Startup is `worker-startup.json`, when the worker wrote one.
	Startup *WorkerStartupRecord `json:"startup"`
	// Exit is `worker-exit.json`: the worker recorded its own death.
	Exit *WorkerExitRecord `json:"exit"`
	// Pids are the live worker processes for this root, the recorded one
	// first.
	Pids []uint32 `json:"pids"`
}

// UnmarshalJSON decodes as serde's derived Deserialize does: `pids` is
// required, the records may be missing or null.
func (p *WorkerProbe) UnmarshalJSON(data []byte) error {
	var wire struct {
		Startup *WorkerStartupRecord `json:"startup"`
		Exit    *WorkerExitRecord    `json:"exit"`
		Pids    *[]uint32            `json:"pids"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Pids == nil {
		return errors.New("missing field `pids`")
	}
	*p = WorkerProbe{Startup: wire.Startup, Exit: wire.Exit, Pids: *wire.Pids}
	return nil
}

type WorkerStartupRecord struct {
	// Step is the latest step the worker reached.
	Step string `json:"step"`
	// Steps is the ordered startup history; older workers recorded only the
	// latest step.
	Steps []WorkerStartupStep `json:"steps"`
}

// UnmarshalJSON decodes as serde's derived Deserialize does: `step` is
// required and `steps` defaults to empty.
func (r *WorkerStartupRecord) UnmarshalJSON(data []byte) error {
	var wire struct {
		Step  *string             `json:"step"`
		Steps []WorkerStartupStep `json:"steps"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Step == nil {
		return errors.New("missing field `step`")
	}
	*r = WorkerStartupRecord{Step: *wire.Step, Steps: wire.Steps}
	return nil
}

type WorkerStartupStep struct {
	Step string `json:"step"`
}

// UnmarshalJSON decodes as serde's derived Deserialize does: `step` is
// required.
func (s *WorkerStartupStep) UnmarshalJSON(data []byte) error {
	var wire struct {
		Step *string `json:"step"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Step == nil {
		return errors.New("missing field `step`")
	}
	s.Step = *wire.Step
	return nil
}

func (r *WorkerStartupRecord) reached(step string) bool {
	if r.Step == step {
		return true
	}
	for _, entry := range r.Steps {
		if entry.Step == step {
			return true
		}
	}
	return false
}

type WorkerExitRecord struct {
	Reason string `json:"reason"`
	// Refusal is a sentence the worker wrote for whoever asked, when it
	// stopped on a precondition the caller can fix rather than on an
	// internal failure.
	Refusal *string `json:"refusal"`
	// At is when the worker stopped, as RFC 3339.
	At                    *string                           `json:"at"`
	HarnessStartupFailure *core.WorkerHarnessStartupFailure `json:"harness_startup_failure"`
}

// UnmarshalJSON decodes as serde's derived Deserialize does: `reason` is
// required and the rest default to absent.
func (r *WorkerExitRecord) UnmarshalJSON(data []byte) error {
	var wire struct {
		Reason                *string                           `json:"reason"`
		Refusal               *string                           `json:"refusal"`
		At                    *string                           `json:"at"`
		HarnessStartupFailure *core.WorkerHarnessStartupFailure `json:"harness_startup_failure"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Reason == nil {
		return errors.New("missing field `reason`")
	}
	*r = WorkerExitRecord{Reason: *wire.Reason, Refusal: wire.Refusal, At: wire.At, HarnessStartupFailure: wire.HarnessStartupFailure}
	return nil
}

// Timestamp is At as epoch seconds; false when absent, unreadable or
// before the epoch.
func (r *WorkerExitRecord) Timestamp() (uint64, bool) {
	if r.At == nil {
		return 0, false
	}
	at, err := time.Parse(time.RFC3339, *r.At)
	if err != nil || at.Unix() < 0 {
		return 0, false
	}
	return uint64(at.Unix()), true
}

// HarnessFailedToOpen is the marked failure of a harness that could not
// open its session, which starting the same harness again would only
// repeat. Nil when there is none.
//
// A harness that failed to open for lack of space is a disk-full exit
// instead: the storage owner restarts it once space returns, and the same
// record still holds the marker then. That is judged by the failure's own
// cause, because Reason also carries the bridge's stderr, which can mention
// an earlier, unrelated full disk.
func (r *WorkerExitRecord) HarnessFailedToOpen() *core.WorkerHarnessStartupFailure {
	if r.HarnessStartupFailure == nil || core.ReportsNoSpace(r.HarnessStartupFailure.Message) {
		return nil
	}
	return r.HarnessStartupFailure
}

func (p *WorkerProbe) Accepting() bool {
	return p.Startup != nil && p.Startup.reached("serving")
}

func (p *WorkerProbe) ReachedSocket() bool {
	return p.Startup != nil && (p.Startup.reached("bind-socket") || p.Startup.reached("serving"))
}

func (p *WorkerProbe) Alive() bool {
	return len(p.Pids) != 0
}

// Step is the latest startup step; false when the worker recorded none.
func (p *WorkerProbe) Step() (string, bool) {
	if p.Startup == nil {
		return "", false
	}
	return p.Startup.Step, true
}

// String is one sentence naming what the worker did: its recorded exit
// reason, or whether it is running and the step it last reached (Rust:
// Display).
func (p *WorkerProbe) String() string {
	if p.Exit != nil {
		return "the worker exited: " + p.Exit.Reason
	}
	step, stepped := p.Step()
	switch {
	case p.Alive() && stepped:
		return "the worker is running; its last startup step was " + core.RustDebugString(step)
	case p.Alive():
		return "the worker is running and recorded no startup step"
	case stepped:
		return "the worker process is gone; it reached the startup step " + core.RustDebugString(step) +
			" and left no exit record"
	default:
		return "the worker process is gone and recorded no startup step"
	}
}

// ProbeWorker reads the worker's startup record, exit record and live
// processes from the target. The process state distinguishes a worker that
// died early from one that is still running but never accepted a relay
// connection, so it must be read before the caller stops the worker.
//
// A target that cannot be asked, or a record that does not parse, is an
// error: the caller must not mistake it for a worker with nothing to
// report.
//
// For decisions (the readiness wait). The probe reads the lock holder
// through the control worker, so like every command that embeds it, it is a
// targets.WorkerControlPlan and installs that worker first. A diagnosis that
// only reports uses DiagnoseWorkerReadOnly.
func ProbeWorker(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID, workerRoot string) (*WorkerProbe, error) {
	plan, err := targets.NewWorkerControlPlan("probe worker state",
		[]targets.ControlCommand{workerProbeCommand(locator, workerRoot)}, locator, sessionID)
	if err != nil {
		return nil, err
	}
	outputs, err := plan.Execute(executor, ControlWorker{})
	if err != nil {
		return nil, fmt.Errorf("the worker probe failed: %w", err)
	}
	if len(outputs) == 0 {
		return nil, errors.New("the worker probe produced no output")
	}
	return parseWorkerProbe(&outputs[len(outputs)-1])
}

// DiagnoseWorkerReadOnly is ProbeWorker for a diagnosis that only reports:
// it never installs and never waits on the worker store. When nothing in
// the root serves the probe, the diagnosis says so as its finding.
func DiagnoseWorkerReadOnly(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) (*WorkerProbe, error) {
	command := workerProbeCommand(locator, workerRoot).UnstagedCommand()
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	if output.Status == targets.WorkerControlUnstagedStatus {
		return nil, errors.New(strings.TrimSpace(lossyText(output.Stderr)))
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("the worker probe exited with status %d: %s", output.Status, strings.TrimSpace(lossyText(output.Stderr)))
	}
	return parseWorkerProbe(&output)
}

func workerProbeCommand(locator *core.TargetsTargetLocator, workerRoot string) targets.ControlCommand {
	return workerProbeScript(workerRoot).IntoCommand(func(script string) core.CommandSpec {
		return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
			WithPurpose("probe worker state")
	})
}

func parseWorkerProbe(output *core.CommandOutput) (*WorkerProbe, error) {
	var probe WorkerProbe
	if err := json.Unmarshal(output.Stdout, &probe); err != nil {
		return nil, fmt.Errorf("read the worker probe: %s: %w", strings.TrimSpace(lossyText(output.Stdout)), err)
	}
	return &probe, nil
}

// workerProbeScript is the shell half of ProbeWorker and
// DiagnoseWorkerReadOnly. The records are JSON the worker wrote, so they are
// inserted as they are; everything else it prints is a number. The live pid
// is the holder of the worker lock, the same fact the liveness probe
// reports.
func workerProbeScript(workerRoot string) targets.ControlScript {
	return targets.WorkerDaemonHolderScript(workerRoot).MapScript(func(holder string) string {
		return "hel_root=" + core.PosixQuote(workerRoot) + "\n" +
			"hel_record() {\n" +
			"    if [ -s \"$1\" ]; then cat \"$1\"; else printf null; fi\n" +
			"}\n" +
			"printf '{\"startup\":'\n" +
			"hel_record \"$hel_root/" + core.WorkerStartupFile + "\"\n" +
			"printf ',\"exit\":'\n" +
			"hel_record \"$hel_root/" + core.WorkerExitFile + "\"\n" +
			"printf ',\"pids\":['\n" +
			holder + "\n" +
			"printf ']}\\n'\n"
	})
}

// FailureLine is a failure on one line, for a session's error field: the
// first line of each cause. A worker's exit reason can carry its bridge's
// stderr after the first line; the caller logs the full chain.
func FailureLine(err error) string {
	var lines []string
	for _, cause := range failureChain(err) {
		line, _, _ := strings.Cut(cause, "\n")
		if line = strings.TrimSpace(line); line != "" {
			lines = append(lines, line)
		}
	}
	return strings.Join(lines, ": ")
}

// failureChain is err's causes from the outermost in, each without the
// causes it wraps, as anyhow's chain() lists them: a layer `context: cause`
// made with %w is "context" followed by the chain of cause. A layer whose
// text does not end with its cause's is kept whole and ends the chain.
func failureChain(err error) []string {
	var causes []string
	for err != nil {
		text := err.Error()
		next := errors.Unwrap(err)
		if next == nil {
			return append(causes, text)
		}
		own, ok := strings.CutSuffix(text, ": "+next.Error())
		if !ok {
			return append(causes, text)
		}
		causes = append(causes, own)
		err = next
	}
	return causes
}
