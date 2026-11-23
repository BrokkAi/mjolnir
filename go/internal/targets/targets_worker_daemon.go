// Port of mj-targets/src/targets/worker_daemon.rs.

package targets

import (
	"errors"
	"fmt"
	"reflect"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ControlWorkerPath is where the staged control worker lives: one stable
// name in the session's own worker root, a hard link to a worker-store
// entry. It exists only when the root's own `hel` cannot serve a control
// subcommand, and it is staged only on demand
// (`controller::worker_binary::run_control_command`). Any build that passes
// the control scripts' `--help` probe serves, so the name does not encode
// the daemon's build and daemons of every build share the file. The root is
// its one owner: the link keeps the store entry alive under the collector's
// ordinary link-count rule and goes with the root; installing a `hel` that
// carries the interface removes it.
func ControlWorkerPath(workerRoot string) string {
	return core.Text(core.Join(workerRoot, ControlWorkerName))
}

// ControlWorkerName is the file name of the staged control worker. Also the
// prefix the control scripts glob (`hel-control*`), which keeps files staged
// under the hashed names of earlier releases usable.
const ControlWorkerName = "hel-control"

// WorkerControlUnstagedStatus is the exit status of a control script when
// nothing in the root serves its subcommand. Nothing ran. The caller stages
// the control worker and reruns the command once
// (`controller::worker_binary::run_control_command`); a read-only diagnosis
// reports it as a finding.
const WorkerControlUnstagedStatus = 78

// installedWorkerScript is shell that runs
// `hel worker <subcommand> <args> --root <root>` from the first binary in
// the root that proves it serves the subcommand: the root's own `hel`, then
// any `hel-control*`. A binary proves it by exiting 0 for
// `worker <subcommand> --help`; every other result (status 2 from a build
// that predates the subcommand, 126 or 127, a crash, a wrong-architecture
// file) counts as not proven. When none proves it the script exits
// WorkerControlUnstagedStatus before doing anything. The decision is made on
// the target against the file about to run, so the daemon keeps no record of
// which build can do what. A root without a `hel` never had a worker started
// from it, so whenNoBinary answers for it. The contract the subcommands keep
// is in `mj-worker/src/worker_stop.rs`.
func installedWorkerScript(workerRoot string, subcommand string, args string, whenNoBinary string) ControlScript {
	root := core.PosixQuote(workerRoot)
	return newControlScript(workerRoot, fmt.Sprintf(`hel_root=%[1]s
if [ -e "$hel_root/hel" ]; then
    hel_bin=
    for hel_candidate in "$hel_root/hel" "$hel_root"/%[2]s*; do
        if [ -f "$hel_candidate" ] && [ -x "$hel_candidate" ] \
            && "$hel_candidate" worker %[3]s --help >/dev/null 2>&1; then
            hel_bin=$hel_candidate
            break
        fi
    done
    if [ -z "$hel_bin" ]; then
        echo "no worker in $hel_root serves 'worker %[3]s'" >&2
        exit %[4]d
    fi
    "$hel_bin" worker %[3]s%[5]s --root "$hel_root" || exit $?
else
    %[6]s
fi
`, root, ControlWorkerName, subcommand, WorkerControlUnstagedStatus, args, whenNoBinary))
}

// ControlWorkerStager installs the control worker into a worker root. The
// code that owns worker binaries lives above this package and implements it;
// a plan's Execute takes one, so no caller can run a control command without
// the means to stage what it needs.
type ControlWorkerStager interface {
	StageControlWorker(caller core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, root string) error
}

// RunStagedControlCommand runs one control command, staging the control
// worker and rerunning it once when the script reports (status 78) that the
// root has nothing to serve it.
// `controller::worker_binary::run_control_command` documents the protocol.
func RunStagedControlCommand(
	caller core.CommandExecutor,
	stager ControlWorkerStager,
	locator *core.TargetsTargetLocator,
	sessionID string,
	command *core.CommandSpec,
) (core.CommandOutput, error) {
	first, err := caller.Execute(command)
	if err != nil {
		return core.CommandOutput{}, err
	}
	if first.Status != WorkerControlUnstagedStatus {
		return first, nil
	}
	root, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return core.CommandOutput{}, err
	}
	// Publishing a build waits behind other uploads of it and can take
	// minutes. A resumable operation does not hold daemon admission for that:
	// a handoff cancels the wait and the next daemon starts it again.
	yielded := caller.YieldAdmissionIfResumable()
	staged := stager.StageControlWorker(caller, locator, sessionID, root)
	if yielded && !caller.CancellationRequested() {
		if err := caller.EndResumableWork("control worker install"); err != nil {
			return core.CommandOutput{}, err
		}
	}
	if staged != nil {
		return core.CommandOutput{}, staged
	}
	second, err := caller.Execute(command)
	if err != nil {
		return core.CommandOutput{}, err
	}
	if second.Status == WorkerControlUnstagedStatus {
		return core.CommandOutput{}, fmt.Errorf("the control worker staged in %s does not serve %s: %s",
			root, command.Purpose, strings.TrimSpace(lossyText(second.Stderr)))
	}
	return second, nil
}

// ControlScript is a shell script that carries the root whose control worker
// it invokes. Composition and transport wrapping preserve the requirement
// without parsing shell text.
type ControlScript struct {
	// root is nil for a script that runs no control worker.
	root   *string
	script string
}

func newControlScript(root string, script string) ControlScript {
	return ControlScript{root: &root, script: script}
}

func ControlScriptWithoutControl(script string) ControlScript {
	return ControlScript{root: nil, script: script}
}

func (s ControlScript) MapScript(compose func(string) string) ControlScript {
	s.script = compose(s.script)
	return s
}

func (s ControlScript) IntoCommand(wrap func(string) core.CommandSpec) ControlCommand {
	return ControlCommand{root: s.root, command: wrap(s.script)}
}

// UnstagedScript is the explicit escape for a caller that handles status 78
// itself, and for read-only diagnoses.
func (s *ControlScript) UnstagedScript() string {
	return s.script
}

// ControlCommand is a command whose staging requirement survives SSH and
// container wrapping.
type ControlCommand struct {
	root    *string
	command core.CommandSpec
}

// UnstagedCommand is only for a caller that handles status 78 itself or
// reports it as a diagnosis.
func (c ControlCommand) UnstagedCommand() core.CommandSpec {
	return c.command
}

// ControlCommandFromCommandSpec is a command that runs no control worker
// (Rust: From<CommandSpec> for ControlCommand).
func ControlCommandFromCommandSpec(command core.CommandSpec) ControlCommand {
	return ControlCommand{root: nil, command: command}
}

// workerControl is the target whose worker root a WorkerControlPlan stages
// the control worker into.
type workerControl struct {
	locator   core.TargetsTargetLocator
	sessionID string
	root      string
}

// WorkerControlPlan: commands are private: execution stages the requirement
// carried by their builders.
type WorkerControlPlan struct {
	description string
	commands    []core.CommandSpec
	control     *workerControl
}

func NewWorkerControlPlan(description string, commands []ControlCommand, locator *core.TargetsTargetLocator, sessionID string) (WorkerControlPlan, error) {
	var root *string
	for _, command := range commands {
		if command.root != nil {
			if root != nil && !libSamePath(*root, *command.root) {
				return WorkerControlPlan{}, errors.New("a control plan cannot stage different worker roots")
			}
			root = command.root
		}
	}
	plan := WorkerControlPlan{description: description, commands: make([]core.CommandSpec, 0, len(commands))}
	for _, command := range commands {
		plan.commands = append(plan.commands, command.command)
	}
	if root != nil {
		plan.control = &workerControl{locator: *locator, sessionID: sessionID, root: *root}
	}
	return plan, nil
}

// WorkerControlPlanWithoutControl is a plan whose commands never run the
// control worker.
func WorkerControlPlanWithoutControl(description string, commands []core.CommandSpec) WorkerControlPlan {
	return WorkerControlPlan{description: description, commands: commands, control: nil}
}

// StagesControlWorker reports whether running this plan stages the control
// worker first.
func (p *WorkerControlPlan) StagesControlWorker() bool {
	return p.control != nil
}

// Execute runs every command with executor. A command whose script finds
// nothing in the root to serve its subcommand (status 78) has the control
// worker staged and is rerun once (RunStagedControlCommand).
func (p *WorkerControlPlan) Execute(executor core.CommandExecutor, stager ControlWorkerStager) ([]core.CommandOutput, error) {
	outputs := make([]core.CommandOutput, 0, len(p.commands))
	for i := range p.commands {
		command := &p.commands[i]
		var output core.CommandOutput
		if control := p.control; control != nil {
			root, err := core.WorkerRoot(&control.locator, control.sessionID)
			if err != nil {
				return nil, err
			}
			if !libSamePath(root, control.root) {
				return nil, errors.New("control script root differs from the staging target")
			}
			output, err = RunStagedControlCommand(executor, stager, &control.locator, control.sessionID, command)
			if err != nil {
				return nil, err
			}
		} else {
			var err error
			output, err = executor.Execute(command)
			if err != nil {
				return nil, err
			}
		}
		checked, err := core.CheckedCommandOutput(command, output)
		if err != nil {
			return nil, err
		}
		outputs = append(outputs, checked)
	}
	return outputs, nil
}

// CommandsWithoutStaging is the commands of a plan that does not run the
// control worker, for a caller that composes them itself. A plan that does
// run it is refused: running its scripts would skip staging.
func (p *WorkerControlPlan) CommandsWithoutStaging() ([]core.CommandSpec, error) {
	if p.control != nil {
		return nil, fmt.Errorf("%s runs the staged control worker, so it must run through `execute`", p.description)
	}
	return p.commands, nil
}

// Scripts is the scripts of any plan, for a test that inspects them. They
// skip staging.
func (p *WorkerControlPlan) Scripts() []core.CommandSpec {
	return p.commands
}

// Equal compares plans by value, as Rust's derived PartialEq does.
func (p *WorkerControlPlan) Equal(other *WorkerControlPlan) bool {
	if p.description != other.description || len(p.commands) != len(other.commands) {
		return false
	}
	for i := range p.commands {
		if !workerControlCommandEqual(p.commands[i], other.commands[i]) {
			return false
		}
	}
	switch {
	case p.control == nil || other.control == nil:
		return p.control == nil && other.control == nil
	}
	return p.control.sessionID == other.control.sessionID &&
		libSamePath(p.control.root, other.control.root) &&
		reflect.DeepEqual(p.control.locator, other.control.locator)
}

// workerControlCommandEqual is Rust's derived PartialEq for CommandSpec: an empty
// argument list or environment equals an absent one, and observers compare
// by identity.
func workerControlCommandEqual(left, right core.CommandSpec) bool {
	normalize := func(command *core.CommandSpec) {
		if len(command.Args) == 0 {
			command.Args = nil
		}
		if len(command.Env) == 0 {
			command.Env = nil
		}
		if len(command.EnvRemove) == 0 {
			command.EnvRemove = nil
		}
	}
	normalize(&left)
	normalize(&right)
	return reflect.DeepEqual(left, right)
}

// WorkerControlPlanFromCommandSpec is a plan of one command that runs no
// control worker (Rust: From<CommandSpec> for WorkerControlPlan).
func WorkerControlPlanFromCommandSpec(command core.CommandSpec) WorkerControlPlan {
	return WorkerControlPlanWithoutControl(command.Purpose, []core.CommandSpec{command})
}

// WorkerControlPlanFromCommandPlan is a plan whose commands run no control
// worker (Rust: From<CommandPlan> for WorkerControlPlan).
func WorkerControlPlanFromCommandPlan(plan core.CommandPlan) WorkerControlPlan {
	return WorkerControlPlanWithoutControl(plan.Description, slices.Clone(plan.Commands))
}

// WorkerDaemonLivenessScript reports whether the exact session worker is
// alive without signaling it. A successful probe prints one of `alive`,
// `starting` or `dead`; transport or shell failures stay distinguishable
// from a confirmed absent worker.
func WorkerDaemonLivenessScript(workerRoot string) ControlScript {
	return installedWorkerScript(workerRoot, "liveness", "", "printf 'dead\\n'")
}

// WorkerDaemonHolderScript prints the pid of the worker that holds the
// root's lock, alone, or nothing.
func WorkerDaemonHolderScript(workerRoot string) ControlScript {
	return installedWorkerScript(workerRoot, "holder", "", ":")
}

// StopWorkerDaemonScript stops the detached worker daemon rooted at
// workerRoot: TERM, then KILL after two seconds. Use this when the session
// continues from the root, so the worker and its harness can shut down in
// order. Returns once the root's lock is free and nothing the worker started
// is left.
func StopWorkerDaemonScript(workerRoot string) ControlScript {
	return installedWorkerScript(workerRoot, "stop", "", ":")
}

// KillWorkerDaemonScript kills a retired worker daemon rooted at workerRoot,
// and its process tree, at once. Only for retirement: the caller removes the
// root next, and a sealed checkpoint (or a discard) means nothing in it
// needs an orderly shutdown.
func KillWorkerDaemonScript(workerRoot string) ControlScript {
	return installedWorkerScript(workerRoot, "stop", " --kill", ":")
}

// ClearRelayStatePlan stops a leaked worker and deletes the durable relay
// state under its root; nil for a target that needs nothing here.
//
// A resume seeds fresh relay state into the same root a closed session used.
// Leftover state wins over that seed at startup, so it has to go, and
// whatever might still be writing it has to go first. Container and instance
// targets are rebuilt from scratch on resume, so they need nothing here.
//
// A staged home that is a link to a profile home, left for a session an
// earlier release started there (`controller::local_profile_homes`), is
// unlinked too. The restore that follows writes the native session into the
// staged home while the worker files are installed beside it, and it must
// land in a directory of the session's own, not in the profile home.
func ClearRelayStatePlan(locator *core.TargetsTargetLocator, sessionID string) (*WorkerControlPlan, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return nil, err
	}
	sessionWorkerRoot, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return nil, err
	}
	stagedHome := core.PosixQuote(sessionWorkerRoot + "/profile")
	script := StopWorkerDaemonScript(sessionWorkerRoot).MapScript(func(stop string) string {
		return fmt.Sprintf("%s\nrm -rf -- %s %s\nif [ -L %s ]; then rm -f -- %s; fi\n",
			stop,
			core.PosixQuote(sessionWorkerRoot+"/"+core.RelayStateFile),
			core.PosixQuote(sessionWorkerRoot+"/"+core.RelayJournalDir),
			stagedHome, stagedHome)
	})
	var command ControlCommand
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		command = script.IntoCommand(func(script string) core.CommandSpec {
			return core.NewCommandSpec("sh", []string{"-c", script}).
				WithPurpose("stop a leaked local Mjolnir worker and clear its relay state")
		})
	case core.TargetsTargetLocatorSshBare:
		command = script.IntoCommand(func(script string) core.CommandSpec {
			return core.SSHCommand(core.SshTrafficClassControl, locator.SSH, []string{"sh", "-c", script}).
				WithPurpose("stop a leaked remote Mjolnir worker and clear its relay state")
		})
	default:
		return nil, nil
	}
	plan, err := NewWorkerControlPlan("clear a leaked worker's relay state", []ControlCommand{command}, locator, sessionID)
	if err != nil {
		return nil, err
	}
	return &plan, nil
}

// StopWorkerPlan stops the session's worker with the staged control worker.
// The plan stages it first when it runs.
func StopWorkerPlan(locator *core.TargetsTargetLocator, sessionID string, purpose string) (WorkerControlPlan, error) {
	root, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return WorkerControlPlan{}, err
	}
	script := StopWorkerDaemonScript(root)
	return NewWorkerControlPlan(purpose, []ControlCommand{
		script.IntoCommand(func(script string) core.CommandSpec {
			return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
				WithPurpose(purpose)
		}),
	}, locator, sessionID)
}

// InPlaceWorkerResetPlan is everything an in-place harness replacement must
// remove before the new profile is staged: the running daemon, relay state,
// the installed worker files, and the previous per-session profile root.
// Runs inside the target on every locator, unlike ClearRelayStatePlan,
// because the environment survives the swap whether or not it is a
// container.
//
// The worker files are unlinked rather than overwritten: a mapped `hel`
// cannot be overwritten while the daemon holds it, and a stale
// `ownership.json` must not survive a crash and let a later reader believe
// the old profile is still installed. The worker root itself is recreated,
// so the caller can install straight afterwards.
func InPlaceWorkerResetPlan(locator *core.TargetsTargetLocator, sessionID string, previousProfileRoot string) (WorkerControlPlan, error) {
	return workerResetPlan(locator, sessionID, &previousProfileRoot)
}

// RetainedNativeWorkerResetPlan resets only the relay for a native
// conversation that stays in its home. Stop its processes first; keep the
// executable, native files and caches.
func RetainedNativeWorkerResetPlan(locator *core.TargetsTargetLocator, sessionID string) (WorkerControlPlan, error) {
	return workerResetPlan(locator, sessionID, nil)
}

func workerResetPlan(locator *core.TargetsTargetLocator, sessionID string, previousProfileRoot *string) (WorkerControlPlan, error) {
	if err := core.VerifyLocator(locator, sessionID); err != nil {
		return WorkerControlPlan{}, err
	}
	sessionWorkerRoot, err := core.WorkerRoot(locator, sessionID)
	if err != nil {
		return WorkerControlPlan{}, err
	}
	// A root that is a link left for a session an earlier release started
	// (`controller::local_profile_homes`) is unlinked, never followed: the
	// path carries no trailing slash, so `rm` removes the link itself.
	script := StopWorkerDaemonScript(sessionWorkerRoot).MapScript(func(stop string) string {
		var script strings.Builder
		fmt.Fprintf(&script, "%s\nrm -rf -- %s %s\n",
			stop,
			core.PosixQuote(sessionWorkerRoot+"/"+core.RelayStateFile),
			core.PosixQuote(sessionWorkerRoot+"/"+core.RelayJournalDir))
		metadata := []string{"launch.json", "ownership.json"}
		if previousProfileRoot != nil {
			metadata = append([]string{"hel"}, metadata...)
		} else {
			metadata = append(metadata, core.NativeSessionIdentityFile, core.RestoredRelaySeedFile)
		}
		quoted := make([]string, len(metadata))
		for i, name := range metadata {
			quoted[i] = core.PosixQuote(sessionWorkerRoot + "/" + name)
		}
		fmt.Fprintf(&script, "rm -f -- %s\n", strings.Join(quoted, " "))
		if previousProfileRoot != nil {
			fmt.Fprintf(&script, "rm -rf -- %s\nmkdir -p -- %s\n",
				core.PosixQuote(strings.TrimRight(*previousProfileRoot, "/")),
				core.PosixQuote(sessionWorkerRoot))
		}
		return script.String()
	})
	command := script.IntoCommand(func(script string) core.CommandSpec {
		return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script}).
			WithPurpose("reset the worker root for an in-place harness replacement").
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageSyncing})
	})
	return NewWorkerControlPlan("reset the worker root", []ControlCommand{command}, locator, sessionID)
}
