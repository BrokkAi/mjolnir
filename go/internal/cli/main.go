// Port of mj-cli/src/main.rs, apart from the cli and command types and the
// cobra command tree, which are in root.go.
//
// main.rs owns the command-line surface and the one-shot subcommands. The
// long-running surfaces live beside it: the dashboard drives the terminal UI,
// pollers the background feeds both of them read, and import session
// adoption.
//
// Rust-only process setup has no Go counterpart: jemalloc as the global
// allocator with its `malloc_conf` arena limit (Go's allocator and scavenger
// return memory themselves), rustls' crypto provider, the test-hooks
// feature, and the test-fixture startup macro.

package cli

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/buildid"
	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/charmbracelet/x/term"
	"github.com/spf13/cobra"
)

// workspaceName is `--workspace` for the commands that work in a workspace.
// It was a global option, so `--help` showed it on every command, including
// the many where it did nothing (F-16).
type workspaceName struct {
	// name is the workspace to work in, by name. `mj new` and `mj acp`
	// require it. `mj sessions` and `mj events` show only that workspace
	// when it is given, and every workspace when it is not. `mj import`, and
	// `mj resume --wiki` when it imports another tool's session, put the
	// session in it, and need it when the instance has more than one
	// workspace. Flag `--workspace NAME`, clap id `workspace_name`.
	name *string
}

// workspaceNameHelp is the help of the flattened `--workspace`.
const workspaceNameHelp = "Workspace to work in, by name. `mj new` and `mj acp` require it. `mj sessions` and `mj events` show only that workspace when it is given, and every workspace when it is not. `mj import`, and `mj resume --wiki` when it imports another tool's session, put the session in it, and need it when the instance has more than one workspace"

// bind declares the flattened `--workspace NAME` option on cmd (the clap
// derive of WorkspaceName).
func (w *workspaceName) bind(cmd *cobra.Command) {
	clapOf(cmd).option("workspace_name", workspaceNameHelp, storeOptional(&w.name), withLong("workspace"), withValueName("NAME"))
}

// or is the name given with the command, or else the one given before it.
func (w workspaceName) or(beforeCommand *string) *string {
	if w.name != nil {
		return w.name
	}
	return beforeCommand
}

// removedCommandArgs is whatever was passed to a removed command. It is
// accepted only so the command can name its replacement instead of failing
// on its arguments.
type removedCommandArgs struct {
	// rest is every argument, hidden, trailing, hyphen values allowed.
	rest []string
}

// bind declares the hidden trailing arguments on cmd.
func (args *removedCommandArgs) bind(cmd *cobra.Command, run func() error) {
	cmd.DisableFlagParsing = true
	cmd.Args = cobra.ArbitraryArgs
	cmd.RunE = func(_ *cobra.Command, rest []string) error {
		args.rest = rest
		return run()
	}
}

// replacementNotice is what a removed command was replaced by, for a command
// that was removed.
func replacementNotice(parsed command) (string, bool) {
	switch parsed.(type) {
	case commandCheckpoint:
		return "`mj checkpoint` was removed; `mj suspend` saves the session", true
	case commandClose:
		return "`mj close` was replaced by `mj suspend` and `mj destroy`", true
	case commandCancelTurn:
		return "`mj cancel-turn` was replaced by `mj interrupt-turn`", true
	}
	return "", false
}

// workspacesArgs: `mj workspaces` on its own opens the workspace manager in
// the dashboard, as it always has. The subcommands are the non-interactive
// form a script uses.
type workspacesArgs struct {
	// command is nil for plain `mj workspaces`.
	command workspacesCommand
}

// bind declares the workspaces subcommands on cmd; run runs the parsed
// command.
func (args *workspacesArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	var list workspacesListArgs
	listCmd := subcommand("list", "List the workspaces sessions can be created in")
	meta.subcommand(listCmd)
	list.bind(listCmd, func() error {
		args.command = workspacesCommandList{list}
		return run()
	})
	var create workspaceCreateArgs
	createCmd := subcommand("create", "Create a workspace, or select the one that already has the name")
	meta.subcommand(createCmd)
	create.bind(createCmd, func() error {
		args.command = workspacesCommandCreate{create}
		return run()
	})
	group(cmd, false, run)
}

// workspacesCommand is a `mj workspaces` subcommand.
type workspacesCommand interface{ isWorkspacesCommand() }

// workspacesCommandList lists the workspaces sessions can be created in.
type workspacesCommandList struct{ args workspacesListArgs }

// workspacesCommandCreate creates a workspace, or selects the one that
// already has the name.
type workspacesCommandCreate struct{ args workspaceCreateArgs }

func (workspacesCommandList) isWorkspacesCommand()   {}
func (workspacesCommandCreate) isWorkspacesCommand() {}

// daemonArgs are the arguments of `mj daemon`.
type daemonArgs struct {
	command daemonCommand
}

// bind declares the daemon subcommands on cmd; run runs the parsed command.
func (args *daemonArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)

	var endpoint daemonCommandEndpoint
	endpointCmd := subcommand("endpoint", "Print the local authenticated control endpoint for an SSH client")
	meta.subcommand(endpointCmd)
	endpointClap := clapOf(endpointCmd)
	endpointClap.flag(&endpoint.json, "json", "Emit exactly one JSON object containing control credentials", required())
	endpointClap.flag(&endpoint.existing, "existing", "Fail if no daemon is running; never start one")
	leaf(endpointCmd, func() error {
		args.command = endpoint
		return run()
	})

	afkCmd := subcommand("afk", "Set or show whether the operator is away")
	meta.subcommand(afkCmd)
	afkClap := clapOf(afkCmd)
	for _, variant := range []struct {
		name    string
		command afkCommand
	}{{"on", afkCommandOn}, {"off", afkCommandOff}, {"status", afkCommandStatus}} {
		variantCmd := subcommand(variant.name, "")
		afkClap.subcommand(variantCmd)
		leaf(variantCmd, func() error {
			args.command = daemonCommandAfk{command: variant.command}
			return run()
		})
	}
	group(afkCmd, true, nil)

	statusCmd := subcommand("status", "Show daemon PID, version, start time, and client count")
	meta.subcommand(statusCmd)
	leaf(statusCmd, func() error {
		args.command = daemonCommandStatus{}
		return run()
	})

	var stop daemonStopArgs
	stopCmd := subcommand("stop", "Stop the daemon. A graceful stop leaves detached workers running unless `--suspend-sessions` suspends them first")
	meta.subcommand(stopCmd)
	stop.bind(stopCmd, func() error {
		args.command = daemonCommandStop{args: stop}
		return run()
	})

	restartCmd := subcommand("restart", "Gracefully replace the daemon with this Mjolnir build")
	meta.subcommand(restartCmd)
	leaf(restartCmd, func() error {
		args.command = daemonCommandRestart{}
		return run()
	})
	group(cmd, true, nil)
}

// daemonCommand is a `mj daemon` subcommand.
type daemonCommand interface{ isDaemonCommand() }

// daemonCommandEndpoint prints the local authenticated control endpoint for
// an SSH client.
type daemonCommandEndpoint struct {
	// json emits exactly one JSON object containing control credentials.
	// Required.
	json bool
	// existing fails if no daemon is running; never start one.
	existing bool
}

// daemonCommandAfk sets or shows whether the operator is away.
type daemonCommandAfk struct{ command afkCommand }

// daemonCommandStatus shows daemon PID, version, start time, and client
// count.
type daemonCommandStatus struct{}

// daemonCommandStop stops the daemon. A graceful stop leaves detached
// workers running unless `--suspend-sessions` suspends them first.
type daemonCommandStop struct{ args daemonStopArgs }

// daemonCommandRestart gracefully replaces the daemon with this Mjolnir
// build.
type daemonCommandRestart struct{}

func (daemonCommandEndpoint) isDaemonCommand() {}
func (daemonCommandAfk) isDaemonCommand()      {}
func (daemonCommandStatus) isDaemonCommand()   {}
func (daemonCommandStop) isDaemonCommand()     {}
func (daemonCommandRestart) isDaemonCommand()  {}

// afkCommand is a `mj daemon afk` subcommand.
type afkCommand int

const (
	afkCommandOn afkCommand = iota
	afkCommandOff
	afkCommandStatus
)

// daemonStopArgs are the arguments of `mj daemon stop`.
type daemonStopArgs struct {
	// force stops an unresponsive or still-starting daemon, including
	// workers in its container, or clears an unreachable legacy container
	// record.
	force bool
	// suspendSessions suspends every active session first, saving a
	// recovery copy of each, then stops the daemon. A session that does not
	// suspend keeps the daemon running and is named.
	suspendSessions bool
	// discardUntracked discards untracked files, nested repositories,
	// untracked stashes, and `.mj/keep/` contents when suspending sessions.
	// Requires suspendSessions.
	discardUntracked bool
}

// bind declares the daemon stop options on cmd.
func (args *daemonStopArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.flag(&args.force, "force", "Stop an unresponsive or still-starting daemon, including workers in its container, or clear an unreachable legacy container record")
	meta.flag(&args.suspendSessions, "suspend_sessions", "Suspend every active session first, saving a recovery copy of each, then stop the daemon. A session that does not suspend keeps the daemon running and is named")
	meta.flag(&args.discardUntracked, "discard_untracked", "Discard untracked files, nested repositories, untracked stashes, and `.mj/keep/` contents when suspending sessions", requires("suspend_sessions"))
	leaf(cmd, run)
}

// moveArgs move a session to another target or profile.
//
// Cross-target moves keep only Git state (branches, tags, stashes, commits,
// staged and unstaged changes to tracked files, and linked worktrees inside
// the checkout) plus scratch in `$MJ_KEEP_DIR`. Untracked files are lost
// unless you `git add` them first; Git-ignored files are never kept.
//
// The group "destination" requires at least one of target, profile and
// cpus.
type moveArgs struct {
	// subagents is the destination delegation policy: native, mjolnir, or
	// none (no delegation tools). Omit to retain the session's policy, or
	// the recorded policy when retrying a sealed Move.
	subagents *string
	// oracle is oracle availability for this session: enabled or disabled.
	oracle *string
	// orchestrators enables orchestrators (requires Mjolnir delegation).
	orchestrators bool
	// subagentStraightforward overrides the configured model for
	// straightforward Mjolnir sub-agents.
	subagentStraightforward *string
	// subagentStraightforwardEffort overrides straightforward sub-agent
	// effort (requires --subagent-straightforward).
	subagentStraightforwardEffort *string
	// subagentSubtle overrides the configured model for subtle Mjolnir
	// sub-agents.
	subagentSubtle *string
	// subagentSubtleEffort overrides subtle sub-agent effort (requires
	// --subagent-subtle).
	subagentSubtleEffort *string
	// subagentOracle overrides the oracle model for this session.
	subagentOracle *string
	// subagentOracleEffort is the oracle effort; requires --subagent-oracle.
	subagentOracleEffort *string
	// subagentModel is retired (hidden); use the four per-type sub-agent
	// flags.
	subagentModel *string
	// discardUntracked discards untracked files, nested repositories, and
	// untracked stashes on cross-target Move.
	discardUntracked bool
	// abortOperation aborts any in-progress Git operation before moving.
	abortOperation bool
	// dryRun inspects what would be moved and what would be lost without
	// changing anything.
	dryRun bool
	// subagentEffort is retired (hidden); use the four per-type sub-agent
	// flags.
	subagentEffort *string
	// skipKeepPaths skips a relative path under `.mj/keep/<session-id>/`
	// during cross-target Move (repeatable `--skip`). Skipped paths are
	// deleted when a managed source is retired, or kept on a raw checkout.
	skipKeepPaths []string
	// session is the session to move. The session identity is retained by
	// the operation.
	session string
	// target is the destination target template. Omit to retain the current
	// target.
	target *string
	// profile is the destination profile. Omit to retain the current
	// profile.
	profile *string
	// cpus is the container CPU limit for the destination, with
	// --memory-gib. Omit both to keep the source size, or to take the
	// destination host's remembered size (else 4 CPUs) when the source has
	// none. Given alone, they resize the container on the target the session
	// already names. At least 1; conflicts with --clear-resources.
	cpus *uint64
	// memoryGiB is the container memory limit in GiB for the destination,
	// with --cpus (`--memory-gib GIB`). At least 1; conflicts with
	// --clear-resources.
	memoryGiB *uint64
	// queue is what to do with prompts/configuration commands already
	// queued.
	queue *moveQueue
	// yes confirms interruption and runs without an interactive prompt.
	yes bool
	// json prints exactly one structured outcome to stdout.
	json bool
	// clearResources removes inherited container sizing: use a fixed host's
	// resources, or the destination's default size for a container.
	clearResources bool
}

// bind declares the move options and the destination group on cmd.
func (args *moveArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("subagents", "Destination delegation policy: native, mjolnir, or none (no delegation tools). Omit to retain the session's policy, or the recorded policy when retrying a sealed Move", storeOptional(&args.subagents))
	meta.option("oracle", "Oracle availability for this session", storeOptional(&args.oracle), possibleValues("enabled", "disabled"))
	meta.flag(&args.orchestrators, "orchestrators", "Enable orchestrators (requires Mjolnir delegation)")
	meta.option("subagent_straightforward", "Override the configured model for straightforward Mjolnir sub-agents", storeOptional(&args.subagentStraightforward))
	meta.option("subagent_straightforward_effort", "Override straightforward sub-agent effort (requires --subagent-straightforward)", storeOptional(&args.subagentStraightforwardEffort))
	meta.option("subagent_subtle", "Override the configured model for subtle Mjolnir sub-agents", storeOptional(&args.subagentSubtle))
	meta.option("subagent_subtle_effort", "Override subtle sub-agent effort (requires --subagent-subtle)", storeOptional(&args.subagentSubtleEffort))
	meta.option("subagent_oracle", "Override the oracle model for this session", storeOptional(&args.subagentOracle))
	meta.option("subagent_oracle_effort", "Oracle effort; requires --subagent-oracle", storeOptional(&args.subagentOracleEffort), requires("subagent_oracle"))
	meta.option("subagent_model", "Retired; use the four per-type sub-agent flags", storeOptional(&args.subagentModel), hidden())
	meta.flag(&args.discardUntracked, "discard_untracked", "Discard untracked files, nested repositories, and untracked stashes on cross-target Move")
	meta.flag(&args.abortOperation, "abort_operation", "Abort any in-progress Git operation before moving")
	meta.flag(&args.dryRun, "dry_run", "Inspect what would be moved and what would be lost without changing anything")
	meta.option("subagent_effort", "Retired; use the four per-type sub-agent flags", storeOptional(&args.subagentEffort), hidden())
	meta.option("skip_keep_paths", "Skip a relative path under `.mj/keep/<session-id>/` during cross-target Move (repeatable). Skipped paths are deleted when a managed source is retired, or kept on a raw checkout", appendString(&args.skipKeepPaths), withLong("skip"), multiple())
	meta.option("session", "Session to move. The session identity is retained by the operation", storeString(&args.session), required())
	meta.option("target", "Destination target template. Omit to retain the current target", storeOptional(&args.target))
	meta.option("profile", "Destination profile. Omit to retain the current profile", storeOptional(&args.profile))
	meta.option("cpus", "Container CPU limit for the destination, with --memory-gib. Omit both to keep the source size, or to take the destination host's remembered size (else 4 CPUs) when the source has none. Given alone, they resize the container on the target the session already names", storeU64(&args.cpus, 1), requires("memory_gib"), conflictsWith("clear_resources"))
	meta.option("memory_gib", "Container memory limit in GiB for the destination, with --cpus", storeU64(&args.memoryGiB, 1), withValueName("GIB"), requires("cpus"), conflictsWith("clear_resources"))
	meta.option("queue", "What to do with prompts/configuration commands already queued", func(raw string) error {
		queue := moveQueue(raw)
		args.queue = &queue
		return nil
	}, possibleValues("discard", "start"))
	meta.flag(&args.yes, "yes", "Confirm interruption and run without an interactive prompt")
	meta.flag(&args.json, "json", "Print exactly one structured outcome to stdout")
	meta.flag(&args.clearResources, "clear_resources", "Remove inherited container sizing: use a fixed host's resources, or the destination's default size for a container")
	meta.group("destination", true, true, "target", "profile", "cpus")
	leaf(cmd, run)
}

// moveQueue is `mj move --queue`.
type moveQueue string

const (
	moveQueueDiscard moveQueue = "discard"
	moveQueueStart   moveQueue = "start"
)

// disposition is the resume queue disposition the choice stands for.
func (queue moveQueue) disposition() core.ResumeQueueDisposition {
	if queue == moveQueueStart {
		return core.ResumeQueueDispositionStart
	}
	return core.ResumeQueueDispositionDiscard
}

// loginArgs are the arguments of `mj login`.
type loginArgs struct {
	// daemonLocal is hidden: the login runs here, where the daemon lives.
	daemonLocal bool
	// profile is the profile to authenticate. Optional when exactly one
	// profile exists.
	profile *string
	// setupToken (Claude profiles only) mints a long-lived subscription
	// token with `claude setup-token` and stores it for every session of
	// this profile. Conflicts with method and cli.
	setupToken bool
	// method is the ACP authentication method id (`--method ID`). Sign-in
	// normally picks an interactive method itself; use this to choose one
	// when a bridge offers several.
	method *string
	// cli uses the harness command on PATH instead of the pinned ACP login.
	// Conflicts with method.
	cli bool
}

// bind declares the login options on cmd.
func (args *loginArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.flag(&args.daemonLocal, "daemon_local", "", hidden())
	meta.option("profile", "Profile to authenticate. Optional when exactly one profile exists", storeOptional(&args.profile))
	meta.flag(&args.setupToken, "setup_token", "Claude profiles only: mint a long-lived subscription token with `claude setup-token` and store it for every session of this profile", conflictsWith("method", "cli"))
	meta.option("method", "ACP authentication method id. Sign-in normally picks an interactive method itself; use this to choose one when a bridge offers several", storeOptional(&args.method), withValueName("ID"))
	meta.flag(&args.cli, "cli", "Use the harness command on PATH instead of the pinned ACP login", conflictsWith("method"))
	leaf(cmd, run)
}

// recoverArgs are the arguments of `mj recover`.
type recoverArgs struct {
	command recoverCommand
}

// bind declares the recover subcommands on cmd; run runs the parsed
// command.
func (args *recoverArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)

	var scan recoverCommandScan
	scanCmd := subcommand("scan", "List managed worker resources not present in controller state")
	meta.subcommand(scanCmd)
	scanClap := clapOf(scanCmd)
	scanClap.flag(&scan.json, "json", "Print the response as JSON instead of text")
	scanClap.flag(&scan.allInstances, "all_instances", "Also list workers created by other Mjolnir instances, or by builds that left no instance stamp")
	leaf(scanCmd, func() error {
		args.command = scan
		return run()
	})

	var adopt recoverCommandAdopt
	adoptCmd := subcommand("adopt", "Probe a managed worker and add it back to controller state")
	meta.subcommand(adoptCmd)
	adoptClap := clapOf(adoptCmd)
	adoptClap.option("session", "Session id of the worker, as `mj recover scan` lists it", storeString(&adopt.session), required())
	adoptClap.option("target", "Target the worker runs on, as `mj recover scan` lists it", storeString(&adopt.target), required())
	adoptClap.option("profile", "Required only for current-v1 workers created before ownership markers", storeOptional(&adopt.profile))
	adoptClap.option("bundle", "Required only for current-v1 workers created before ownership markers", storeOptional(&adopt.bundle))
	adoptClap.flag(&adopt.allInstances, "all_instances", "Allow adopting a worker another instance created, or one with no instance stamp")
	leaf(adoptCmd, func() error {
		args.command = adopt
		return run()
	})

	var destroy recoverCommandDestroy
	destroyCmd := subcommand("destroy", "Destroy an untracked managed resource after exact-ID confirmation")
	meta.subcommand(destroyCmd)
	destroyClap := clapOf(destroyCmd)
	destroyClap.option("session", "Session id of the worker, as `mj recover scan` lists it", storeString(&destroy.session), required())
	destroyClap.option("target", "Target the worker runs on, as `mj recover scan` lists it", storeString(&destroy.target), required())
	destroyClap.option("confirm", "The session id again, to confirm the destruction", storeString(&destroy.confirm), required())
	destroyClap.flag(&destroy.allInstances, "all_instances", "Allow destroying a worker another instance created, or one with no instance stamp")
	leaf(destroyCmd, func() error {
		args.command = destroy
		return run()
	})
	group(cmd, true, nil)
}

// recoverCommand is a `mj recover` subcommand.
type recoverCommand interface{ isRecoverCommand() }

// recoverCommandScan lists managed worker resources not present in
// controller state.
type recoverCommandScan struct {
	// json prints the response as JSON instead of text.
	json bool
	// allInstances also lists workers created by other Mjolnir instances,
	// or by builds that left no instance stamp.
	allInstances bool
}

// recoverCommandAdopt probes a managed worker and adds it back to
// controller state.
type recoverCommandAdopt struct {
	// session is the session id of the worker, as `mj recover scan` lists
	// it.
	session string
	// target is the target the worker runs on, as `mj recover scan` lists
	// it.
	target string
	// profile is required only for current-v1 workers created before
	// ownership markers.
	profile *string
	// bundle is required only for current-v1 workers created before
	// ownership markers.
	bundle *string
	// allInstances allows adopting a worker another instance created, or
	// one with no instance stamp.
	allInstances bool
}

// recoverCommandDestroy destroys an untracked managed resource after
// exact-ID confirmation.
type recoverCommandDestroy struct {
	// session is the session id of the worker, as `mj recover scan` lists
	// it.
	session string
	// target is the target the worker runs on, as `mj recover scan` lists
	// it.
	target string
	// confirm is the session id again, to confirm the destruction.
	confirm string
	// allInstances allows destroying a worker another instance created, or
	// one with no instance stamp.
	allInstances bool
}

func (recoverCommandScan) isRecoverCommand()    {}
func (recoverCommandAdopt) isRecoverCommand()   {}
func (recoverCommandDestroy) isRecoverCommand() {}

// doctorArgs are the arguments of `mj doctor`.
type doctorArgs struct {
	// command is nil for the prerequisite checks themselves.
	command doctorCommand
	// json emits a machine-readable array of prerequisite checks.
	json bool
	// smoke runs disposable container smoke tests where supported.
	smoke bool
}

// bind declares the doctor options and subcommands on cmd.
func (args *doctorArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	bindDoctorCommand(meta, func(parsed doctorCommand) error {
		args.command = parsed
		return run()
	})
	meta.flag(&args.json, "json", "Emit a machine-readable array of prerequisite checks")
	meta.flag(&args.smoke, "smoke", "Run disposable container smoke tests where supported")
	group(cmd, false, run)
}

// setupArgs are the arguments of the hidden `mj setup`.
type setupArgs struct {
	command doctorCommand
}

// bind declares the setup subcommands on cmd.
func (args *setupArgs) bind(cmd *cobra.Command, run func() error) {
	bindDoctorCommand(clapOf(cmd), func(parsed doctorCommand) error {
		args.command = parsed
		return run()
	})
	group(cmd, true, nil)
}

// bindDoctorCommand declares DoctorCommand's subcommands on meta.
func bindDoctorCommand(meta *clapCommand, run func(doctorCommand) error) {
	var instructions doctorCommandInstructions
	instructionsCmd := subcommand("instructions", "Print coding-agent instructions for preparing a host")
	meta.subcommand(instructionsCmd)
	clapOf(instructionsCmd).option("platform", "Platform to prepare", func(raw string) error {
		instructions.platform = setupPlatform(raw)
		return nil
	}, possibleValues("linux", "macos"), required())
	leaf(instructionsCmd, func() error { return run(instructions) })
}

// doctorCommand is a `mj doctor` subcommand.
type doctorCommand interface{ isDoctorCommand() }

// doctorCommandInstructions prints coding-agent instructions for preparing
// a host.
type doctorCommandInstructions struct {
	// platform is the platform to prepare. Required.
	platform setupPlatform
}

func (doctorCommandInstructions) isDoctorCommand() {}

// setupPlatform is `mj doctor instructions --platform`.
type setupPlatform string

const (
	setupPlatformLinux setupPlatform = "linux"
	setupPlatformMacos setupPlatform = "macos"
)

// runMain is the part of Rust's main that runs once the command line is
// parsed: a removed command's notice (exit 2, clap's usage status), the
// daemon-wait-exit helper before anything else starts, the open file limit,
// the worker build identity, the instance environment, the daemon's working
// directory, logging and panic logging, then run; a daemon-run refused
// because another daemon owns the store exits with core's store-owned exit
// code. Main in root.go parses and calls it.
func runMain(parsed cli) error {
	if notice, ok := replacementNotice(parsed.command); ok {
		// Exit 2, the status clap uses for a usage error, which this is.
		fmt.Fprintf(os.Stderr, "error: %s\n", notice)
		return &exitStatus{code: 2}
	}
	if waitExit, ok := parsed.command.(commandDaemonWaitExit); ok {
		return waitExitRun(waitExit.args)
	}
	// First, before any child: every child inherits the limit.
	core.RaiseOpenFileLimit()
	core.Install(buildid.Identity())
	// Apply before logging, daemon startup, or any path lookup: everything
	// derives its directories from the instance environment, and the daemon
	// child inherits it. This also covers `daemon-run`, which is this same
	// binary, so a per-instance daemon validates its own environment on boot.
	if err := core.ApplyInstanceFlag(parsed.instance); err != nil {
		return err
	}
	_, daemonRun := parsed.command.(commandDaemonRun)
	if daemonRun {
		// Set this before starting work: the launcher's session checkout can
		// be removed while the daemon is still running.
		home, ok := core.UserHome()
		if !ok {
			return errors.New("locate the background process's home directory")
		}
		if err := os.Chdir(home); err != nil {
			return fmt.Errorf("set background working directory to %s: %w", home, err)
		}
	}
	log, err := startControllerLog(commandName(parsed.command), processKindOf(parsed.command))
	if err != nil {
		return err
	}
	result := runLoggingPanics(parsed)
	if result != nil {
		var exit *exitStatus
		if !errors.As(result, &exit) {
			slog.Error("Mjolnir exited with an error", "error", result.Error())
		}
	} else {
		slog.Info("Mjolnir stopped")
	}
	if log != nil && log.writerGuard != nil {
		log.writerGuard.Close()
	}
	var owned *controller.StoreAlreadyOwnedError
	if daemonRun && result != nil && errors.As(result, &owned) {
		fmt.Fprintf(os.Stderr, "Error: %s\n", result)
		return &exitStatus{code: int(core.StoreOwnedExitCode)}
	}
	return result
}

// runLoggingPanics is run with a panic's report sent to the log, as Rust's
// panic hook does, before the panic goes on.
func runLoggingPanics(parsed cli) error {
	defer func() {
		if recovered := recover(); recovered != nil {
			logPanic(recovered)
			panic(recovered)
		}
	}()
	return run(parsed)
}

// maxRuntimeThreads is the cap Rust puts on Tokio's worker threads: the
// daemon mostly waits on I/O, and on a 120-CPU host the idle workers'
// wakeups cost more than they add. Whether the Go daemon needs the same cap
// (as GOMAXPROCS) is measured when the daemon is ported.
const maxRuntimeThreads = 16

// run runs the parsed command and the bounded shutdown that follows. The
// dashboard's startup update check and its detach notice run with the
// dashboard, which is not ported.
func run(parsed cli) error {
	runtime := core.CurrentRuntime()
	ctx := context.Background()
	_, daemon := parsed.command.(commandDaemonRun)
	result := runCommand(ctx, parsed.command, parsed.workspace, parsed.noSplash)
	if daemon {
		shutdownDaemonRuntime(runtime)
	} else {
		core.Shutdown(runtime, core.BlockingWorkAwait{}, runtimeShutdownGrace)
	}
	return result
}

// detach_notice (what a detaching terminal says it leaves behind: the
// daemon, how many sessions it is still running, and the command that stops
// them all) takes the dashboard's DetachedSessions and is declared with
// dashboard.rs.

// mjCommand is an `mj` command line for this instance, for a message to
// suggest.
func mjCommand(arguments string) string {
	if instance, ok := core.InstanceName(); ok {
		return "mj --instance " + instance + " " + arguments
	}
	return "mj " + arguments
}

// listedSession is a session as a stop or detach message names it.
type listedSession struct {
	title     string
	lifecycle sessionruntime.ViewerLifecycleCategory
}

// sessionRoots is every session that suspending stands for itself, in every
// workspace, read from the runtime feed the dashboard itself follows: each
// session but a sub-agent whose parent is active, since suspending the
// parent stops it. A sub-agent left behind by a parent that is not is its
// own root, as a workspace close counts it. The socket is used rather than
// the web API so this works with the web viewer disabled.
func sessionRoots(ctx context.Context, daemon *client.DaemonClient) (map[string]listedSession, error) {
	frame, err := daemon.RuntimeChanges(ctx, nil, false)
	if err != nil {
		return nil, err
	}
	if frame.Kind != client.RuntimeFrameSnapshot || frame.Projection == nil {
		return nil, errors.New("the daemon answered a request for its sessions without a snapshot")
	}
	return sessionRootsFromProjection(&frame.Projection.Records, &frame.Projection.Subagents), nil
}

// sessionRootsFromProjection is sessionRoots for one projection.
func sessionRootsFromProjection(
	records *core.SnapshotMap[string, core.SessionRecord],
	subagents *core.SnapshotMap[string, core.SubagentMetadata],
) map[string]listedSession {
	active := func(id string) bool {
		record, ok := records.Get(id)
		return ok && sessionruntime.ViewerLifecycleCategoryOf(record.State).IsDashboardVisible()
	}
	roots := map[string]listedSession{}
	for record := range records.Values() {
		if child, ok := subagents.Get(record.ID); ok && active(child.ParentSessionID) {
			continue
		}
		roots[record.ID] = listedSession{
			title:     record.ListedTitle(),
			lifecycle: sessionruntime.ViewerLifecycleCategoryOf(record.State),
		}
	}
	return roots
}

// activeSessions is the roots the daemon still holds on its dashboard: the
// same lifecycle category the web API and the dashboard call active.
func activeSessions(ctx context.Context, daemon *client.DaemonClient) (map[string]listedSession, error) {
	sessions, err := sessionRoots(ctx, daemon)
	if err != nil {
		return nil, err
	}
	maps.DeleteFunc(sessions, func(_ string, session listedSession) bool {
		return !session.lifecycle.IsDashboardVisible()
	})
	return sessions, nil
}

// awaitStoppingSession waits for a session that is already suspending, or
// being destroyed, to leave the dashboard, and says how it ended. The daemon
// owns that operation and decides when it ends; this only reads its outcome.
func awaitStoppingSession(ctx context.Context, sessionID string) (string, error) {
	daemon, err := client.ConnectExisting(ctx)
	if err != nil {
		return "", err
	}
	defer daemon.Close()
	for {
		roots, err := sessionRoots(ctx, daemon)
		if err != nil {
			return "", err
		}
		session, ok := roots[sessionID]
		switch {
		case ok && session.lifecycle.IsDashboardVisible():
			select {
			case <-time.After(time.Second):
			case <-ctx.Done():
				return "", ctx.Err()
			}
		case ok && session.lifecycle == sessionruntime.ViewerLifecycleCategorySuspended:
			return "Suspended", nil
		// Only a destroy removes a session's record.
		case !ok:
			return "Destroyed", nil
		default:
			return "", fmt.Errorf("it failed to stop; `%s` says why", mjCommand("sessions --session "+sessionID))
		}
	}
}

// suspendAllSessions suspends every active session, together, and returns
// once each one has stopped, as `mj daemon stop --suspend-sessions` does
// before it stops the daemon. Suspending a parent stops its sub-agents. A
// session already suspending or being destroyed is waited for, not
// suspended again. Fails, naming each session, when any of them did not
// stop; those that did stay stopped, so running the command again picks up
// only what is left. The sessions are those active when it is asked: one
// another client starts meanwhile is not among them. Returns how many
// sessions it stopped.
func suspendAllSessions(ctx context.Context, discardUntracked bool) (int, error) {
	// Workers outlive a stopped daemon, so this starts one when none is
	// running: only a daemon can suspend them.
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return 0, err
	}
	sessions, err := activeSessions(ctx, daemon)
	daemon.Close()
	if err != nil {
		return 0, err
	}
	count := len(sessions)
	if count == 0 {
		return 0, nil
	}
	plural := "s"
	if count == 1 {
		plural = ""
	}
	fmt.Printf("Suspending %d active session%s…\n", count, plural)
	type finished struct {
		name    string
		outcome string
		err     error
	}
	results := make(chan finished, count)
	for _, id := range slices.Sorted(maps.Keys(sessions)) {
		session := sessions[id]
		name := fmt.Sprintf("%s (%s)", id, session.title)
		// A suspension answers when it has ended, which takes as long as its
		// recovery copy, so each one has its own connection.
		go func() {
			defer func() {
				if recovered := recover(); recovered != nil {
					results <- finished{name: name, err: fmt.Errorf("a suspension task failed: %s", core.PanicMessage(recovered))}
				}
			}()
			if session.lifecycle == sessionruntime.ViewerLifecycleCategorySuspending {
				outcome, err := awaitStoppingSession(ctx, id)
				results <- finished{name: name, outcome: outcome, err: err}
				return
			}
			err := func() error {
				existing, err := client.ConnectExisting(ctx)
				if err != nil {
					return err
				}
				defer existing.Close()
				consent := core.SuspendConsent{}
				if discardUntracked {
					consent.DiscardUntracked = core.UntrackedConsent{Kind: core.ConsentAny}
				}
				return existing.SuspendSessionWithConsent(ctx, id, consent)
			}()
			results <- finished{name: name, outcome: "Suspended", err: err}
		}()
	}
	var failures []string
	for range count {
		result := <-results
		if result.err != nil {
			failures = append(failures, fmt.Sprintf("%s: %s", result.name, nameSuspendFlags(result.err)))
			continue
		}
		fmt.Printf("%s %s.\n", result.outcome, result.name)
	}
	if len(failures) == 0 {
		return count, nil
	}
	plural = "s"
	if len(failures) == 1 {
		plural = ""
	}
	return 0, fmt.Errorf("the Mjolnir daemon was left running; %d session%s did not stop:\n  %s", len(failures), plural, strings.Join(failures, "\n  "))
}

// logPanic sends a panic's report to the log rather than to a detached
// standard error, which may itself be full (Rust's install_panic_logging).
// Go has no process-wide panic hook; runLoggingPanics calls this for the
// command's own goroutine before the panic goes on.
func logPanic(recovered any) {
	panicLog("Mjolnir panicked: " + core.PanicMessage(recovered))
}

// processKindOf classifies a command for log retention: the persistent
// daemon and any terminal-owning interactive surface each get their own
// retained window so short CLI invocations never push a long-running
// process's log out of it. (Rust's process_kind; logging.go's type takes the
// name processKind.)
func processKindOf(parsed command) processKind {
	switch parsed := parsed.(type) {
	case commandDaemonRun:
		return processKindDaemon
	case nil, commandGo, commandSimple:
		return processKindTui
	case commandWorkspaces:
		if parsed.args.command == nil {
			return processKindTui
		}
	}
	return processKindCli
}

// commandName is the command's name as logging records it.
func commandName(parsed command) string {
	switch parsed.(type) {
	case nil:
		return "dashboard"
	case commandImage:
		return "image"
	case commandGo:
		return "go"
	case commandSimple:
		return "simple"
	case commandWorkspaces:
		return "workspaces"
	case commandDaemon:
		return "daemon"
	case commandDaemonRun:
		return "daemon-run"
	case commandDaemonWaitExit:
		return "daemon-wait-exit"
	case commandAcp:
		return "acp"
	case commandDoctor:
		return "doctor"
	case commandEc2:
		return "ec2"
	case commandSetup:
		return "setup"
	case commandImport:
		return "import"
	case commandRecover:
		return "recover"
	case commandCheckpoint:
		return "checkpoint"
	case commandMove:
		return "move"
	case commandUpgradeContainer:
		return "upgrade-container"
	case commandMoveSources:
		return "move-sources"
	case commandLogin:
		return "login"
	case commandNew:
		return "new"
	case commandPrompt:
		return "prompt"
	case commandClearQueue:
		return "clear-queue"
	case commandEvent:
		return "event"
	case commandMessage:
		return "message"
	case commandWait:
		return "wait"
	case commandTranscript:
		return "transcript"
	case commandUsage:
		return "usage"
	case commandEvents:
		return "events"
	case commandPutFile:
		return "put-file"
	case commandFetch:
		return "fetch"
	case commandElicitations:
		return "elicitations"
	case commandRespond:
		return "respond"
	case commandDiff:
		return "diff"
	case commandExport:
		return "export"
	case commandSessions:
		return "sessions"
	case commandSuspend:
		return "suspend"
	case commandRestart:
		return "restart"
	case commandPurgeUntracked:
		return "purge-untracked"
	case commandDestroy:
		return "destroy"
	case commandNotices:
		return "notices"
	case commandResume:
		return "resume"
	case commandInterruptTurn:
		return "interrupt-turn"
	case commandReview:
		return "review"
	case commandStopTask:
		return "stop-task"
	case commandApiInfo:
		return "api-info"
	case commandApiToken:
		return "api-token"
	case commandGithubToken:
		return "github-token"
	case commandUi:
		return "ui"
	case commandModels:
		return "models"
	case commandQuota:
		return "quota"
	case commandHosts:
		return "hosts"
	case commandSetConfig:
		return "set-config"
	case commandClose:
		return "close"
	case commandCancelTurn:
		return "cancel-turn"
	}
	panic(fmt.Sprintf("commandName: unknown command %T", parsed))
}

// runtimeShutdownGrace is how long runtime shutdown waits for blocking work
// that is awaiting runtime work through core.BlockOn.
const runtimeShutdownGrace = 5 * time.Second

// daemonBlockingGrace is how long the daemon's exit waits for guarded
// blocking awaits, and then for the rest of the blocking work.
//
// By the time the runtime shuts down, the daemon's epilogue has stopped
// every task it owns and the database writer has committed every accepted
// write. What still runs is work no owner waits for, such as the blocking
// half of a task the epilogue aborted. Waiting for it without a bound kept
// the old daemon alive, and the next one waiting, for seconds after its
// epilogue had finished.
const daemonBlockingGrace = 1 * time.Second

// shutdownDaemonRuntime is the daemon's runtime shutdown: bounded, and it
// names what it leaves behind.
func shutdownDaemonRuntime(runtime *core.Runtime) {
	started := time.Now()
	report := core.Shutdown(runtime, core.BlockingWorkAwaitFor{Limit: daemonBlockingGrace}, daemonBlockingGrace)
	if report != (core.ShutdownReport{}) {
		// Subprocesses and SSH admissions register their waits by name;
		// other blocking work has no name, so the counts stand for it.
		snapshots, _ := core.ActiveBlockingOperations()
		operations := make([]string, 0, len(snapshots))
		for _, operation := range snapshots {
			operations = append(operations, fmt.Sprintf("%s (%s, %d ms)", operation.Purpose, operation.Program, operation.ElapsedMs))
		}
		slog.Warn("the daemon exited leaving blocking work that no owner waits for",
			"awaits_left", report.AwaitsLeft, "blocking_left", report.BlockingLeft, "operations", operations)
	}
	if took := time.Since(started); took >= 250*time.Millisecond {
		slog.Info("daemon runtime shutdown finished", "duration_ms", took.Milliseconds())
	}
}

// shutdownDashboardRuntime abandons the dashboard's disposable blocking
// reads, which must not delay process exit.
func shutdownDashboardRuntime(runtime *core.Runtime) {
	core.Shutdown(runtime, core.BlockingWorkAbandon{}, runtimeShutdownGrace)
}

// stdinAndStdoutAreTerminals is Rust's IsTerminal check on both.
func stdinAndStdoutAreTerminals() bool {
	return term.IsTerminal(os.Stdin.Fd()) && term.IsTerminal(os.Stdout.Fd())
}

// runCommand runs one parsed command. Rust returns the dashboard's
// DashboardExit and takes the startup update notice (mj-controller's
// StartupUpdateNotice); both belong to the dashboard, which is not ported, so
// the Go declaration returns only the error until dashboard.rs is.
func runCommand(ctx context.Context, parsed command, requestedWorkspace *string, noSplash bool) error {
	// The dashboard, `go` and `simple` first ask a computer that never ran a
	// daemon where it runs; the question belongs to the dashboard and runs
	// with it.
	switch parsed := parsed.(type) {
	case nil:
		return fmt.Errorf("the dashboard: %w", errNotPorted)
	case commandGo:
		if requestedWorkspace != nil {
			return errors.New("mj go selects its workspace from the folder; omit --workspace")
		}
		return fmt.Errorf("mj go: %w", errNotPorted)
	case commandSimple:
		if requestedWorkspace != nil {
			return errors.New("mj simple selects its workspace from the folder; omit --workspace")
		}
		return fmt.Errorf("mj simple: %w", errNotPorted)
	case commandWorkspaces:
		switch command := parsed.args.command.(type) {
		case nil:
			return fmt.Errorf("the dashboard's workspace manager: %w", errNotPorted)
		case workspacesCommandList:
			return workspacesList(ctx, command.args)
		case workspacesCommandCreate:
			return workspacesCreate(ctx, command.args)
		}
	case commandDaemon:
		return runDaemonCommand(ctx, parsed.args)
	case commandDaemonRun:
		return fmt.Errorf("mj daemon-run: %w", errNotPorted)
	case commandDaemonWaitExit:
		panic("the waiter runs before anything else starts")
	case commandAcp:
		return fmt.Errorf("mj acp: %w", errNotPorted)
	case commandEc2:
		forwarded, err := runWhereDaemonLives(ctx, daemonForwardedArgs(), false)
		if err != nil || forwarded {
			return err
		}
		return fmt.Errorf("mj ec2: %w", errNotPorted)
	case commandDoctor:
		if parsed.args.command != nil {
			return printSetupInstructions(parsed.args.command)
		}
		forwarded, err := runWhereDaemonLives(ctx, daemonForwardedArgs(), false)
		if err != nil || forwarded {
			return err
		}
		return doctor(ctx, parsed.args)
	case commandSetup:
		return printSetupInstructions(parsed.args.command)
	case commandImport:
		return fmt.Errorf("mj import: %w", errNotPorted)
	case commandRecover:
		return runRecover(ctx, parsed.args)
	case commandUpgradeContainer:
		return upgradeContainer(ctx, parsed)
	case commandMove:
		return moveSession(ctx, parsed.args)
	case commandMoveSources:
		if parsed.cleanup != nil && !parsed.yes {
			return fmt.Errorf("Inspect with mj move-sources --session %s, then pass --cleanup OPERATION --yes to delete that source and its excluded files", parsed.session)
		}
		daemon, err := ConnectOrStart(ctx)
		if err != nil {
			return err
		}
		defer daemon.Close()
		sources, err := daemon.MoveSources(ctx, parsed.session, parsed.cleanup)
		if err != nil {
			return err
		}
		encoded, err := core.SerdeJSONPretty(sources)
		if err != nil {
			return err
		}
		fmt.Println(string(encoded))
		return nil
	case commandLogin:
		forwarded := []string{"login"}
		if parsed.args.profile != nil {
			forwarded = append(forwarded, "--profile", *parsed.args.profile)
		}
		if parsed.args.setupToken {
			forwarded = append(forwarded, "--setup-token")
		}
		if parsed.args.method != nil {
			forwarded = append(forwarded, "--method", *parsed.args.method)
		}
		if parsed.args.cli {
			forwarded = append(forwarded, "--cli")
		}
		if !parsed.args.daemonLocal {
			ran, err := runWhereDaemonLives(ctx, forwarded, true)
			if err != nil || ran {
				return err
			}
		}
		return login(ctx, parsed.args)
	case commandImage:
		return fmt.Errorf("mj image: %w", errNotPorted)
	case commandNew:
		return newSession(ctx, parsed.args, parsed.args.workspace.or(requestedWorkspace))
	case commandPrompt:
		return prompt(ctx, parsed.args)
	case commandClearQueue:
		return clearQueue(ctx, parsed.args)
	case commandEvent:
		return event(ctx, parsed.args)
	case commandMessage:
		return message(ctx, parsed.args)
	case commandWait:
		return wait(ctx, parsed.args)
	case commandPutFile:
		return putFile(ctx, parsed.args)
	case commandFetch:
		return fetch(ctx, parsed.args)
	case commandElicitations:
		return elicitations(ctx, parsed.args)
	case commandRespond:
		return respond(ctx, parsed.args)
	case commandEvents:
		return events(ctx, parsed.args, parsed.args.workspace.or(requestedWorkspace))
	case commandUsage:
		return usage(ctx, parsed.args)
	case commandTranscript:
		return transcript(ctx, parsed.args)
	case commandDiff:
		return diff(ctx, parsed.args)
	case commandExport:
		return export(ctx, parsed.args)
	case commandSessions:
		return sessions(ctx, parsed.args, parsed.args.workspace.or(requestedWorkspace))
	case commandSuspend:
		return suspend(ctx, parsed.args)
	case commandRestart:
		return restartSession(ctx, parsed.args)
	case commandPurgeUntracked:
		return purgeUntracked(ctx, parsed.args)
	case commandDestroy:
		return destroy(ctx, parsed.args)
	case commandNotices:
		return notices(ctx, parsed.args)
	case commandResume:
		args := parsed.args
		args.workspace.name = args.workspace.or(requestedWorkspace)
		return resume(ctx, args)
	case commandInterruptTurn:
		return interruptTurn(ctx, parsed.args)
	case commandReview:
		return review(ctx, parsed.args)
	case commandStopTask:
		return stopTask(ctx, parsed.args)
	case commandUi:
		return ui(ctx, parsed.args)
	case commandModels:
		return models(ctx, parsed.args)
	case commandQuota:
		return quota(ctx, parsed.args)
	case commandHosts:
		return hosts(ctx, parsed.args)
	case commandSetConfig:
		return setConfig(ctx, parsed.args)
	case commandApiToken:
		return apiToken(ctx)
	case commandApiInfo:
		return apiInfo(ctx, parsed.args)
	case commandGithubToken:
		return githubToken(ctx, parsed.args)
	// Answered in runMain before anything starts.
	case commandClose, commandCancelTurn, commandCheckpoint:
		notice, _ := replacementNotice(parsed)
		return errors.New(notice)
	}
	panic(fmt.Sprintf("runCommand: unknown command %T", parsed))
}

// upgradeContainer runs `mj upgrade-container`: it previews the container
// upgrade and queues it once the person acknowledges the writable-layer
// loss, or cancels a pending one.
func upgradeContainer(ctx context.Context, parsed commandUpgradeContainer) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	defer daemon.Close()
	if parsed.cancel {
		if err := daemon.CancelContainerUpgrade(ctx, parsed.session); err != nil {
			return err
		}
		fmt.Println("cancelled pending container upgrade")
		return nil
	}
	preview, err := daemon.PreviewContainerUpgrade(ctx, parsed.session)
	if err != nil {
		return err
	}
	targetID := "not cached"
	if preview.TargetImageID != nil {
		targetID = *preview.TargetImageID
	}
	fmt.Printf("Current image: %s (%s)\nTarget image: %s (%s)\n%s\n",
		preview.CurrentImage, preview.CurrentImageID, preview.TargetImage, targetID, core.ContainerUpgradeWarning)
	if !parsed.acknowledgeWritableLayerLoss {
		fmt.Println("Review this preview, then pass --acknowledge-writable-layer-loss to queue the upgrade.")
		return nil
	}
	if err := daemon.UpgradeContainer(ctx, core.ContainerUpgradeRequest{Selection: preview, AcknowledgeWritableLayerLoss: true}); err != nil {
		return err
	}
	fmt.Println("container upgrade pending; accepted work will finish first")
	return nil
}

// moveFailure is the failed outcome of a move that has no preparation yet.
func moveFailure(args *moveArgs, message string) core.MoveOutcome {
	outcome := core.MoveOutcome{
		SessionID: args.session,
		Outcome:   "failed",
		Error:     &message,
	}
	if args.profile != nil {
		outcome.ProfileID = *args.profile
	}
	if args.target != nil {
		outcome.TargetTemplateID = *args.target
	}
	return outcome
}

// moveSession runs the daemon-owned move operation from the one-shot CLI.
//
// Preparation is deliberately a separate request: it lets unattended
// callers prove that a queue choice is explicit and lets interactive
// callers show the interruption warning before the source is stopped. Once
// admitted, a lost CLI connection does not cancel the daemon operation; only
// an explicit Ctrl-C does.
func moveSession(ctx context.Context, args moveArgs) error {
	subagents, err := subagentPolicy(args.subagents, args.subagentStraightforward, args.subagentStraightforwardEffort,
		args.subagentSubtle, args.subagentSubtleEffort, args.subagentModel, args.subagentEffort, args.orchestrators)
	if err != nil {
		return err
	}
	oracleChoice, err := oracleOverride(args.subagentOracle, args.subagentOracleEffort, args.oracle)
	if err != nil {
		return err
	}
	var oracleEnabled *bool
	if args.oracle != nil {
		enabled := *args.oracle == "enabled"
		oracleEnabled = &enabled
	} else if oracleChoice != nil {
		enabled := true
		oracleEnabled = &enabled
	}
	if !args.yes && !args.dryRun && !stdinAndStdoutAreTerminals() {
		err := errors.New("non-interactive moves require --yes")
		if args.json {
			outcome := moveFailure(&args, err.Error())
			if printErr := printMoveJSON(&outcome); printErr != nil {
				return printErr
			}
		}
		return err
	}
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		if args.json {
			outcome := moveFailure(&args, err.Error())
			if printErr := printMoveJSON(&outcome); printErr != nil {
				return printErr
			}
		}
		return fmt.Errorf("connect to move daemon: %w", err)
	}
	defer daemon.Close()
	var resourceAllocation core.SessionResourceAllocation
	if args.cpus != nil && args.memoryGiB != nil {
		if *args.memoryGiB > (1<<64-1)>>30 {
			err := errors.New("--memory-gib is too large")
			if args.json {
				outcome := moveFailure(&args, err.Error())
				if printErr := printMoveJSON(&outcome); printErr != nil {
					return printErr
				}
			}
			return err
		}
		resourceAllocation = core.SessionResourceAllocationContainer{Cpus: *args.cpus, MemoryBytes: *args.memoryGiB << 30}
	}
	sizedByFlags := resourceAllocation != nil
	selection := core.MoveSelection{
		Subagents:    subagents,
		Oracle:       oracleEnabled,
		OracleChoice: oracleChoice,
		Consent: core.SuspendConsent{
			WithoutCheckpoint:   false,
			ConfirmNoCheckpoint: false,
		},
		SkipKeepPaths:           slices.Clone(args.skipKeepPaths),
		SessionID:               args.session,
		ProfileID:               args.profile,
		TargetTemplateID:        args.target,
		ResourceAllocation:      resourceAllocation,
		ClearResourceAllocation: args.clearResources,
	}
	if args.discardUntracked {
		selection.Consent.DiscardUntracked = core.UntrackedConsent{Kind: core.ConsentAny}
	}
	if args.abortOperation {
		selection.Consent.AbortOperations = core.OperationConsent{Kind: core.ConsentAny}
	}
	preparation, err := func() (core.MovePreparation, error) {
		// With no policy or model flags, the opt-in only changes the
		// orchestrator choice in the session's existing policy.
		if args.orchestrators && args.subagents == nil && args.subagentStraightforward == nil && args.subagentSubtle == nil {
			record, err := daemon.SessionRecord(ctx, args.session)
			if err != nil {
				return core.MovePreparation{}, err
			}
			if record != nil && record.Subagents != nil && record.Subagents.Mode == core.SubagentPolicyMjolnir {
				selection.Subagents = &core.SubagentPolicy{
					Mode:            core.SubagentPolicyMjolnir,
					Straightforward: record.Subagents.Straightforward,
					Subtle:          record.Subagents.Subtle,
					Orchestrators:   true,
				}
			}
		}
		return daemon.PrepareMoveSession(ctx, selection)
	}()
	if err != nil {
		if args.json {
			outcome := moveFailure(&args, err.Error())
			if printErr := printMoveJSON(&outcome); printErr != nil {
				return printErr
			}
		}
		return fmt.Errorf("prepare move: %w", withClearResourcesHint(err, sizedByFlags))
	}
	if args.dryRun {
		if args.json {
			encoded, err := core.SerdeJSONPretty(preparation)
			if err != nil {
				return err
			}
			fmt.Println(string(encoded))
		} else {
			fmt.Print(formatMoveDryRun(preparation.RoutePreview, preparation.SkipKeepSizes, preparation.SuspendPreflight))
		}
		return nil
	}
	if preview := preparation.ContainerUpgrade; preview != nil {
		if args.queue != nil {
			return errors.New("Container settings replacement waits for accepted work. Omit --queue or clear the queue before confirming.")
		}
		if args.yes {
			fmt.Fprintln(os.Stderr, preview.Warning)
		} else {
			confirmed, err := promptMoveConfirmation(ctx, &preparation, nil)
			if err != nil {
				return err
			}
			if !confirmed {
				return errors.New("container settings replacement cancelled")
			}
		}
		if err := daemon.UpgradeContainer(ctx, core.ContainerUpgradeRequest{Selection: *preview, AcknowledgeWritableLayerLoss: true}); err != nil {
			return err
		}
		if args.json {
			outcome := moveErrorOutcome(&preparation, "")
			outcome.Outcome = "container-upgrade-pending"
			outcome.Error = nil
			outcome.Recovery = nil
			return printMoveJSON(&outcome)
		}
		fmt.Fprintln(os.Stderr, "Container settings replacement pending; accepted work will finish first.")
		return nil
	}
	if !args.json && preparation.DestinationChecks == core.DestinationChecksAfterProvisioning {
		fmt.Fprintln(os.Stderr, core.EC2MovePreparationNotice)
	}
	if !args.json && len(preparation.SkipKeepSizes) > 0 {
		fmt.Fprintln(os.Stderr, "Skipped .mj/keep/ paths (deleted when managed source is retired; kept on raw checkout):")
		for _, skipped := range preparation.SkipKeepSizes {
			fmt.Fprintf(os.Stderr, "  %s (%s)\n", skipped.Path, core.FormatBytes(skipped.Bytes))
		}
	}
	pending := len(preparation.QueuedCommands)
	var queue *core.ResumeQueueDisposition
	switch {
	case args.queue != nil:
		disposition := args.queue.disposition()
		queue = &disposition
	case preparation.LockedQueue != nil:
		locked := *preparation.LockedQueue
		queue = &locked
	case pending == 0:
	case args.yes:
		err := errors.New(queuedCommandsNeedChoice(pending))
		if args.json {
			outcome := moveErrorOutcome(&preparation, err.Error())
			if printErr := printMoveJSON(&outcome); printErr != nil {
				return printErr
			}
		}
		return err
	default:
		chosen, err := promptQueueChoice(ctx, pending)
		if err != nil {
			return err
		}
		queue = &chosen
	}

	printMoveConversion(&preparation)
	if !args.json {
		printMoveDestinationSize(&preparation)
	}
	if args.yes {
		// --yes acknowledges interruption, but intentionally does not
		// choose what happens to queued work; that choice was handled above.
	} else if confirmed, err := promptMoveConfirmation(ctx, &preparation, queue); err != nil {
		return err
	} else if !confirmed {
		err := errors.New("move cancelled before interruption")
		if args.json {
			outcome := moveErrorOutcome(&preparation, err.Error())
			if printErr := printMoveJSON(&outcome); printErr != nil {
				return printErr
			}
		}
		return err
	}

	if !args.json {
		profile := preparation.SourceProfileID
		if preparation.Selection.ProfileID != nil {
			profile = *preparation.Selection.ProfileID
		}
		target := preparation.SourceTargetTemplateID
		if preparation.Selection.TargetTemplateID != nil {
			target = *preparation.Selection.TargetTemplateID
		}
		fmt.Fprintf(os.Stderr, "Moving %s to %s/%s (operation %s)…\n", preparation.Selection.SessionID, profile, target, preparation.OperationID)
	}
	request := core.MoveSessionRequest{Preparation: preparation, Queue: queue, AcknowledgeInterruption: true}
	warnings := newLaunchWarnings(ctx)
	defer warnings.close()

	interrupted, stopInterrupt := signal.NotifyContext(ctx, os.Interrupt)
	defer stopInterrupt()
	pollCtx, stopPolling := context.WithCancel(ctx)
	var polling sync.WaitGroup
	polling.Add(1)
	go func() {
		defer polling.Done()
		ticker := time.NewTicker(time.Second)
		defer ticker.Stop()
		for {
			warnings.report(pollCtx, args.session)
			select {
			case <-ticker.C:
			case <-pollCtx.Done():
				return
			}
		}
	}()
	type moved struct {
		outcome core.MoveOutcome
		err     error
	}
	operation := make(chan moved, 1)
	go func() {
		outcome, err := daemon.MoveSession(interrupted, request)
		operation <- moved{outcome, err}
	}()
	var outcome core.MoveOutcome
	select {
	case result := <-operation:
		stopPolling()
		polling.Wait()
		if result.err != nil {
			return withDaemonHandoffMoveHint(result.err)
		}
		outcome = result.outcome
	case <-interrupted.Done():
		stopPolling()
		polling.Wait()
		// The cancelled request has dropped its connection; let it return
		// before the client it used is closed.
		<-operation
		// Abandoning the request only detaches this client. Ask the daemon
		// explicitly, then bound the wait so a wedged target does not make
		// Ctrl-C appear ineffective.
		cancelCtx, cancelDone := context.WithTimeout(ctx, 10*time.Second)
		err := func() error {
			cancelling, err := ConnectOrStart(cancelCtx)
			if err != nil {
				return err
			}
			defer cancelling.Close()
			return cancelling.CancelLifecycle(cancelCtx, args.session)
		}()
		timedOut := errors.Is(cancelCtx.Err(), context.DeadlineExceeded)
		cancelDone()
		switch {
		case timedOut:
			slog.Warn("timed out requesting move cancellation")
		case err != nil:
			slog.Warn("move cancellation request failed", "error", err.Error())
		}
		cancelErr := fmt.Errorf("move cancellation requested for %s", args.session)
		message := cancelErr.Error()
		recovery := "Reconnect to observe the daemon-owned move operation."
		cancelled := core.MoveOutcome{
			OperationID:      preparation.OperationID,
			SessionID:        preparation.Selection.SessionID,
			ProfileID:        valueOr(preparation.Selection.ProfileID, preparation.SourceProfileID),
			TargetTemplateID: valueOr(preparation.Selection.TargetTemplateID, preparation.SourceTargetTemplateID),
			Outcome:          "cancelled",
			Error:            &message,
			Recovery:         &recovery,
		}
		if args.json {
			if err := printMoveJSON(&cancelled); err != nil {
				return err
			}
		} else {
			printMoveHuman(&cancelled)
		}
		return cancelErr
	}
	warnings.report(ctx, args.session)
	if args.json {
		if err := printMoveJSON(&outcome); err != nil {
			return err
		}
	} else {
		printMoveHuman(&outcome)
	}
	if outcome.Outcome == "failed" || outcome.Outcome == "cancelled" {
		detail := "no further details"
		if outcome.Error != nil {
			detail = *outcome.Error
		}
		return fmt.Errorf("move %s: %s", outcome.Outcome, detail)
	}
	return nil
}

// valueOr is *value, or fallback when value is nil (Rust's
// `unwrap_or_else(|| fallback.clone())`).
func valueOr(value *string, fallback string) string {
	if value != nil {
		return *value
	}
	return fallback
}

// withDaemonHandoffMoveHint: a Move outlives the daemon that accepted it: a
// handoff cancels its file copy and the next daemon resumes it from the
// durable record. A client that loses its connection mid-Move cannot tell
// that from a failure, so say so instead of leaving the bare transport
// error.
func withDaemonHandoffMoveHint(err error) error {
	var lost client.DaemonTransportLost
	if errors.As(err, &lost) {
		return fmt.Errorf("the daemon connection ended while the Move was running, usually a daemon upgrade or restart; an accepted Move continues under the next daemon, so do not start another one: watch the session, and run `mj move` again with the same options only if it stays disconnected: %w", err)
	}
	return err
}

// withClearResourcesHint: the daemon owns the bare-target refusal text; the
// flag that resolves it is specific to this command line, so it is added
// here. A Move that named a size with flags needs the opposite advice: the
// fixed host sizes itself.
func withClearResourcesHint(err error, sizedByFlags bool) error {
	if !strings.Contains(err.Error(), core.BareTargetFixedResources) {
		return err
	}
	if sizedByFlags {
		return fmt.Errorf("drop --cpus and --memory-gib: a fixed host uses its own CPU and memory: %w", err)
	}
	return fmt.Errorf("add --clear-resources to remove the inherited container sizing when moving to a fixed host: %w", err)
}

// queuedCommandsNeedChoice is the refusal for queued work that an
// unattended move did not choose a disposition for.
func queuedCommandsNeedChoice(pending int) string {
	noun, verb := "commands", "require"
	if pending == 1 {
		noun, verb = "command", "requires"
	}
	return fmt.Sprintf("%d queued %s %s an explicit --queue discard|start with --yes", pending, noun, verb)
}

// launchWarnings reads daemon-produced launch notices without changing the
// daemon protocol.
type launchWarnings struct {
	daemon     *client.DaemonClient
	cursor     *client.RuntimeCursor
	lastNotice uint64
	// mu serializes reads: the notice poll and the final report share the
	// one connection.
	mu sync.Mutex
}

// newLaunchWarnings connects for launch notices, or reports why it could
// not and reads none.
func newLaunchWarnings(ctx context.Context) *launchWarnings {
	warnings, err := launchWarningsConnect(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Could not read launch notices: %s\n", err)
		return &launchWarnings{}
	}
	return warnings
}

// launchWarningsConnect connects and starts after the newest notice the
// daemon already holds.
func launchWarningsConnect(ctx context.Context) (*launchWarnings, error) {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return nil, err
	}
	frame, err := daemon.RuntimeChanges(ctx, nil, false)
	if err != nil {
		daemon.Close()
		return nil, err
	}
	if frame.Kind != client.RuntimeFrameSnapshot || frame.Projection == nil {
		daemon.Close()
		return nil, errors.New("the daemon did not return a runtime snapshot")
	}
	var last uint64
	for _, notice := range frame.Projection.Metadata.Notices {
		last = max(last, notice.ID)
	}
	cursor := frame.Cursor
	return &launchWarnings{daemon: daemon, cursor: &cursor, lastNotice: last}, nil
}

// close releases the notice connection.
func (w *launchWarnings) close() {
	if w.daemon != nil {
		w.daemon.Close()
	}
}

// report prints the session's new launch notices, or why they could not be
// read.
func (w *launchWarnings) report(ctx context.Context, sessionID string) {
	if err := w.read(ctx, sessionID); err != nil && ctx.Err() == nil {
		fmt.Fprintf(os.Stderr, "Could not read launch notices: %s\n", err)
	}
}

// read prints the session's notices newer than the last one seen.
func (w *launchWarnings) read(ctx context.Context, sessionID string) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.daemon == nil {
		return nil
	}
	frame, err := w.daemon.RuntimeChanges(ctx, w.cursor, false)
	if err != nil {
		return err
	}
	var metadata *client.RuntimeMetadata
	switch frame.Kind {
	case client.RuntimeFrameSnapshot:
		cursor := frame.Cursor
		w.cursor = &cursor
		if frame.Projection != nil {
			metadata = &frame.Projection.Metadata
		}
	case client.RuntimeFrameDelta:
		cursor := frame.Cursor
		w.cursor = &cursor
		if frame.Changes != nil {
			metadata = frame.Changes.Metadata
		}
	case client.RuntimeFrameResetRequired:
		w.cursor = nil
		return nil
	}
	if metadata != nil {
		for _, notice := range metadata.Notices {
			if notice.ID > w.lastNotice && notice.SessionID == sessionID {
				fmt.Fprintln(os.Stderr, core.SanitizeTerminalText(notice.Text))
			}
			w.lastNotice = max(w.lastNotice, notice.ID)
		}
	}
	return nil
}

// readPromptLine reads one line of standard input off the caller's
// goroutine, so a cancelled context does not wait for the person.
func readPromptLine(ctx context.Context) (string, error) {
	type answer struct {
		line string
		err  error
	}
	answered := make(chan answer, 1)
	go func() {
		line, err := bufio.NewReader(os.Stdin).ReadString('\n')
		if errors.Is(err, io.EOF) {
			err = nil
		}
		answered <- answer{line, err}
	}()
	select {
	case result := <-answered:
		return result.line, result.err
	case <-ctx.Done():
		return "", ctx.Err()
	}
}

// promptQueueChoice asks what to do with queued work; discarding is the
// default.
func promptQueueChoice(ctx context.Context, pending int) (core.ResumeQueueDisposition, error) {
	plural := "s"
	if pending == 1 {
		plural = ""
	}
	fmt.Fprintf(os.Stderr, "%d queued command%s found. [D]iscard queued work (default) or [S]tart after move? ", pending, plural)
	line, err := readPromptLine(ctx)
	if err != nil {
		return "", fmt.Errorf("queue choice prompt task failed: %w", err)
	}
	if strings.EqualFold(strings.TrimSpace(line), "s") {
		return core.ResumeQueueDispositionStart, nil
	}
	return core.ResumeQueueDispositionDiscard, nil
}

// printMoveConversion says what moving a local checkout into an isolated
// workspace will do. This prints for `--yes` too: the checkout stays behind
// and uncommitted work is copied, which a person should be able to read in
// the transcript of the command afterwards.
func printMoveConversion(preparation *core.MovePreparation) {
	conversion := preparation.Conversion
	if conversion == nil {
		return
	}
	fmt.Fprintln(os.Stderr, conversion.SummaryLine())
	for _, warning := range conversion.WarningLines() {
		fmt.Fprintln(os.Stderr, warning)
	}
}

// printMoveDestinationSize prints the size the destination container will
// have. `mj move` shows it for `--yes` too, because the default a Move picks
// is not something the person typed and has to be readable afterwards.
func printMoveDestinationSize(preparation *core.MovePreparation) {
	if preparation.InPlace {
		return
	}
	container, ok := preparation.Selection.ResourceAllocation.(core.SessionResourceAllocationContainer)
	if !ok {
		return
	}
	fmt.Fprintf(os.Stderr, "Destination container size: %d CPUs, %s GiB.\n", container.Cpus, core.FloorGibTenths(container.MemoryBytes))
}

// promptMoveConfirmation shows what the move will interrupt and asks to
// continue.
func promptMoveConfirmation(ctx context.Context, preparation *core.MovePreparation, queue *core.ResumeQueueDisposition) (bool, error) {
	if preview := preparation.ContainerUpgrade; preview != nil {
		fmt.Fprintf(os.Stderr, "Apply container settings and restart?\n%s\n", preview.Warning)
		return confirmMovePrompt(ctx)
	}
	profile := valueOr(preparation.Selection.ProfileID, preparation.SourceProfileID)
	target := valueOr(preparation.Selection.TargetTemplateID, preparation.SourceTargetTemplateID)
	fmt.Fprintf(os.Stderr, "Move session %s from %s/%s to %s/%s.\n", preparation.Selection.SessionID,
		preparation.SourceProfileID, preparation.SourceTargetTemplateID, profile, target)
	if preparation.CrossHarness {
		fmt.Fprintln(os.Stderr, "This changes the harness or model service; the transcript handoff is text-only.")
	}
	if preparation.InPlace {
		fmt.Fprintln(os.Stderr, "Only the harness and profile are replaced; the environment and workspace are kept.")
	}
	if preparation.Active {
		if preparation.InPlace {
			fmt.Fprintln(os.Stderr, "Active work will be interrupted; the session keeps its environment.")
		} else {
			fmt.Fprintln(os.Stderr, "Active work will be interrupted and restored into a fresh environment.")
		}
	}
	if preparation.Selection.ClearResourceAllocation {
		fmt.Fprintln(os.Stderr, "Destination uses fixed/default resources; inherited sizing will be removed.")
	}
	if queue != nil {
		queued := preparation.QueuedCommands
		if len(queued) > 0 {
			plural := "s"
			if len(queued) == 1 {
				plural = ""
			}
			fmt.Fprintf(os.Stderr, "Queued work (%d command%s):\n", len(queued), plural)
			for index, command := range queued {
				kind, text := "prompt", queuedContentText(command.Content)
				if setConfig, ok := command.Kind.(core.QueuedCommandKindSetConfig); ok {
					kind, text = "config", core.ConfigCommandText(setConfig.Key, setConfig.Value)
				}
				text = strings.ReplaceAll(text, "\n", " ")
				if runes := []rune(text); len(runes) > 180 {
					text = string(runes[:179]) + "…"
				}
				fmt.Fprintf(os.Stderr, "  %d. %s: %s\n", index+1, kind, text)
			}
		}
		disposition := "be discarded"
		if *queue == core.ResumeQueueDispositionStart {
			disposition = "start"
		}
		fmt.Fprintf(os.Stderr, "Queued work will %s after destination readiness.\n", disposition)
	}
	return confirmMovePrompt(ctx)
}

// queuedContentText is a queued prompt's text, as
// core.MaterializedContentText reads its content blocks.
func queuedContentText(content []any) string {
	blocks := make([]json.RawMessage, 0, len(content))
	for _, block := range content {
		encoded, err := core.SerdeJSON(block)
		if err != nil {
			continue
		}
		blocks = append(blocks, encoded)
	}
	return core.MaterializedContentText(blocks)
}

// confirmMovePrompt asks `Continue? [y/N]` on standard error and reads the
// answer.
func confirmMovePrompt(ctx context.Context) (bool, error) {
	fmt.Fprint(os.Stderr, "Continue? [y/N] ")
	line, err := readPromptLine(ctx)
	if err != nil {
		return false, fmt.Errorf("move confirmation prompt task failed: %w", err)
	}
	return strings.EqualFold(strings.TrimSpace(line), "y"), nil
}

// formatMoveDryRun is what `mj move --dry-run` prints without `--json`.
func formatMoveDryRun(route *core.RoutePreview, skipKeepSizes []core.SkipKeepSize, preflight *core.SuspendPreflight) string {
	var out strings.Builder
	if route != nil {
		fmt.Fprintf(&out, "Route: %s\n", route.Label())
	}
	if len(skipKeepSizes) > 0 {
		out.WriteString("Skipped .mj/keep/ paths (deleted when managed source is retired; kept on raw checkout):\n")
		for _, skipped := range skipKeepSizes {
			fmt.Fprintf(&out, "  %s (%s)\n", skipped.Path, core.FormatBytes(skipped.Bytes))
		}
	}
	if preflight != nil {
		fmt.Fprintf(&out, "Untracked files: %d, nested repositories: %d, untracked stashes: %d\n",
			len(preflight.Untracked)+len(preflight.KeepDirEntries), len(preflight.NestedRepositories), len(preflight.UntrackedStashes))
		if preflight.HasBlockers() {
			out.WriteString("Blockers:\n")
			for _, blocker := range preflight.Blockers {
				fmt.Fprintf(&out, "  - %s\n", blocker.Remedy)
			}
		}
	}
	return out.String()
}

// moveErrorOutcome is a failed outcome for a prepared move.
func moveErrorOutcome(preparation *core.MovePreparation, message string) core.MoveOutcome {
	return core.MoveOutcome{
		OperationID:      preparation.OperationID,
		SessionID:        preparation.Selection.SessionID,
		ProfileID:        valueOr(preparation.Selection.ProfileID, preparation.SourceProfileID),
		TargetTemplateID: valueOr(preparation.Selection.TargetTemplateID, preparation.SourceTargetTemplateID),
		Outcome:          "failed",
		Error:            &message,
	}
}

// printMoveJSON prints one outcome as a single JSON line.
func printMoveJSON(outcome *core.MoveOutcome) error {
	encoded, err := core.SerdeJSON(outcome)
	if err != nil {
		return err
	}
	fmt.Println(string(encoded))
	return nil
}

// printMoveHuman prints one outcome as a sentence.
func printMoveHuman(outcome *core.MoveOutcome) {
	destination := outcome.ProfileID + "/" + outcome.TargetTemplateID
	switch outcome.Outcome {
	case "completed":
		fmt.Printf("Moved %s to %s; ready and idle. (operation %s)\n", outcome.SessionID, destination, outcome.OperationID)
	case "unchanged":
		fmt.Printf("%s is already on %s; unchanged. (operation %s)\n", outcome.SessionID, destination, outcome.OperationID)
	case "interrupted":
		fmt.Printf("Move for %s will continue after daemon upgrade. (operation %s)\n", outcome.SessionID, outcome.OperationID)
	default:
		detail, recovery := "", ""
		if outcome.Error != nil {
			detail = ": " + *outcome.Error
		}
		if outcome.Recovery != nil {
			recovery = " Recovery: " + *outcome.Recovery
		}
		fmt.Printf("Move %s for %s (operation %s)%s%s\n", outcome.Outcome, outcome.SessionID, outcome.OperationID, detail, recovery)
	}
}

// run_workspace_dashboard, DashboardStartup (what the dashboard needs before
// it can draw: the daemon client, the go mode, and the store as the
// dashboard first shows it) and start_dashboard (connect to the daemon,
// choose the workspace, and load the store, as its own task so the splash
// can animate meanwhile) hold the dashboard's LoadedDashboard, DashboardExit
// and UpgradeResume and mj-controller's StartupUpdateNotice; they are
// declared with dashboard.rs.

// resolveStoreWorkspace is the id of the workspace requested by name, or of
// the only workspace when none is named.
func resolveStoreWorkspace(ctx context.Context, requested *string) (string, error) {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return "", err
	}
	defer daemon.Close()
	workspaces, err := daemon.ListWorkspaces(ctx)
	if err != nil {
		return "", err
	}
	if requested != nil {
		wanted := strings.ToLower(strings.TrimSpace(*requested))
		for _, candidate := range workspaces {
			if strings.ToLower(candidate.Workspace.Name) == wanted {
				return candidate.Workspace.ID, nil
			}
		}
		records := make([]core.WorkspaceRecord, 0, len(workspaces))
		for _, candidate := range workspaces {
			records = append(records, candidate.Workspace)
		}
		return "", unknownWorkspace(*requested, records)
	}
	switch len(workspaces) {
	case 1:
		return workspaces[0].Workspace.ID, nil
	case 0:
		return "", errors.New("no workspace exists; run `mj` to create one")
	}
	return "", errors.New("several workspaces exist; pass `--workspace NAME`")
}

// workspaceRequired is the refusal for a command that creates a session
// without `--workspace`.
//
// There is no hidden workspace to fall back on: every session lives in one
// the dashboard and the viewer list, so the command has to name it (launch
// finding H-3). The refusal lists what exists and how to make one.
//
// It never starts a daemon: starting one only to refuse cost `mj new` more
// than a second (launch finding R2-14), and `mj acp` must not start one
// before a client has asked it for a session. A running daemon supplies the
// list; without one the list is read from the store, and a store that does
// not exist yet has none. A list that cannot be read is left out rather than
// hiding the reason.
func workspaceRequired(ctx context.Context, command string) error {
	workspaces, ok := listedWorkspacesWithoutStarting(ctx)
	if !ok {
		return errors.New(workspaceRequiredMessage(command, nil))
	}
	listed := make([]workspaceSessions, 0, len(workspaces))
	for _, workspace := range workspaces {
		listed = append(listed, workspaceSessions{name: workspace.Name, sessions: workspace.SessionCount})
	}
	return errors.New(workspaceRequiredMessage(command, &listed))
}

// listedWorkspacesWithoutStarting is the workspaces as a running daemon
// lists them or, when none runs and the daemon's store lives on this
// computer, as that store holds them. Never starts a daemon. ok is false
// when no list could be read, which includes a store in a daemon
// container's volume.
func listedWorkspacesWithoutStarting(ctx context.Context) (workspaces []core.WorkspaceRecord, ok bool) {
	daemon, err := client.ConnectExisting(ctx)
	if err == nil {
		defer daemon.Close()
		listings, err := daemon.ListWorkspaces(ctx)
		if err != nil {
			slog.Warn("could not list workspaces for the refusal", "error", err.Error())
			return nil, false
		}
		records := make([]core.WorkspaceRecord, 0, len(listings))
		for _, listing := range listings {
			records = append(records, listing.Workspace)
		}
		return records, true
	}
	if _, stopped := client.DaemonNotRunningOf(err); stopped {
		return storedWorkspacesOnThisComputer()
	}
	slog.Warn("could not reach the daemon to list workspaces", "error", err.Error())
	return nil, false
}

// storedWorkspacesOnThisComputer is the workspaces in a host daemon's store,
// read while no daemon runs. Only a host placement has its store here; a
// container's is in its volume. A store that does not exist yet has none;
// one that cannot be read answers ok false.
func storedWorkspacesOnThisComputer() (workspaces []core.WorkspaceRecord, ok bool) {
	placement, err := core.DaemonConfigLoadPlacement(core.ConfigPath())
	if err != nil || placement.Runtime != core.DaemonRuntimeHost {
		return nil, false
	}
	// Rust's Path::exists: any metadata error reads as absent.
	if _, err := os.Stat(store.DatabasePath()); err != nil {
		return []core.WorkspaceRecord{}, true
	}
	workspaces, err = store.ListWorkspaces()
	if err != nil {
		slog.Warn("could not read the workspaces from the store", "error", err.Error())
		return nil, false
	}
	return workspaces, true
}

// refuseUnknownWorkspace refuses a `--workspace` name that no workspace
// carries, before anything starts a daemon (launch findings R5-9 and R6-2).
// The list comes from a running daemon or, when none runs, from the store,
// as in workspaceRequired. A name that cannot be checked is let through; the
// daemon refuses it later if it is unknown.
func refuseUnknownWorkspace(ctx context.Context, name string) error {
	workspaces, ok := listedWorkspacesWithoutStarting(ctx)
	if !ok {
		return nil
	}
	wanted := strings.ToLower(strings.TrimSpace(name))
	for _, workspace := range workspaces {
		if strings.ToLower(workspace.Name) == wanted {
			return nil
		}
	}
	return unknownWorkspace(name, workspaces)
}

// workspaceSessions is a workspace's name and how many sessions it holds, as
// the refusals list them.
type workspaceSessions struct {
	name     string
	sessions uint64
}

// workspaceRequiredMessage is workspaceRequired's text. A nil workspaces
// means the list could not be read.
func workspaceRequiredMessage(command string, workspaces *[]workspaceSessions) string {
	var message strings.Builder
	message.WriteString(command + " needs --workspace NAME: every session lives in a workspace")
	pushWorkspaceList(&message, workspaces)
	return message.String()
}

// unknownWorkspaceMessage is the refusal for a `--workspace` name no
// workspace carries: the daemon's workspaces and how to make one, as
// workspaceRequiredMessage gives them (launch finding R2-5).
func unknownWorkspaceMessage(name string, workspaces *[]workspaceSessions) string {
	var message strings.Builder
	message.WriteString("unknown workspace " + core.RustDebugString(name))
	pushWorkspaceList(&message, workspaces)
	return message.String()
}

// unknownWorkspace is the same refusal from a daemon's workspace listing.
func unknownWorkspace(name string, workspaces []core.WorkspaceRecord) error {
	listed := make([]workspaceSessions, 0, len(workspaces))
	for _, workspace := range workspaces {
		listed = append(listed, workspaceSessions{name: workspace.Name, sessions: workspace.SessionCount})
	}
	return errors.New(unknownWorkspaceMessage(name, &listed))
}

// pushWorkspaceList appends the listed workspaces with their session counts,
// or says there are none, and ends with how to make one. A nil workspaces
// means the list could not be read, so only the hint is added.
func pushWorkspaceList(message *strings.Builder, workspaces *[]workspaceSessions) {
	switch {
	case workspaces == nil:
	case len(*workspaces) == 0:
		message.WriteString("; this instance has none yet")
	default:
		message.WriteString(". Workspaces:")
		for _, workspace := range *workspaces {
			noun := "sessions"
			if workspace.sessions == 1 {
				noun = "session"
			}
			fmt.Fprintf(message, "\n  %s  (%d %s)", workspace.name, workspace.sessions, noun)
		}
	}
	message.WriteString("\nCreate one with `mj workspaces create NAME`.")
}

// workspaceNameForDirectory is the name plain `mj` gives the workspace it
// creates in directory: the folder's own name. `mj go` looks for a workspace
// by this name, so the two commands share one workspace per folder.
func workspaceNameForDirectory(directory string) string {
	// Rust's Path::file_name: none for a root, `.` or `..`, and a name that
	// is not Unicode is not used.
	cleaned := filepath.Clean(directory)
	name := filepath.Base(cleaned)
	if name == "." || name == ".." || strings.ContainsRune(name, filepath.Separator) ||
		filepath.VolumeName(cleaned) == cleaned || strings.TrimSpace(name) == "" || !utf8.ValidString(name) {
		name = "workspace"
	}
	runes := []rune(strings.TrimSpace(name))
	if len(runes) > 64 {
		runes = runes[:64]
	}
	return string(runes)
}

// suggestedWorkspaceName is the name of the workspace plain `mj` creates
// when there is none: the folder's name, or `workspace` for an SSH daemon,
// numbered past the names already taken.
func suggestedWorkspaceName(workspaces []client.WorkspaceListing) (string, error) {
	placement, err := core.DaemonConfigLoadPlacement(core.ConfigPath())
	if err != nil {
		return "", err
	}
	base := "workspace"
	if placement.Runtime != core.DaemonRuntimeSsh {
		directory, err := os.Getwd()
		if err != nil {
			return "", fmt.Errorf("read current directory for workspace name: %w", err)
		}
		base = workspaceNameForDirectory(directory)
	}
	// The store keeps the name `default` for sessions made before a
	// workspace was required, even while that workspace is not listed.
	names := map[string]bool{core.DefaultWorkspaceID: true}
	for _, candidate := range workspaces {
		names[strings.ToLower(candidate.Workspace.Name)] = true
	}
	if !names[strings.ToLower(base)] {
		return base, nil
	}
	for number := 2; number <= 10_000; number++ {
		suffix := fmt.Sprintf("-%d", number)
		prefix := []rune(base)
		if limit := max(64-len([]rune(suffix)), 0); len(prefix) > limit {
			prefix = prefix[:limit]
		}
		candidate := string(prefix) + suffix
		if !names[strings.ToLower(candidate)] {
			return candidate, nil
		}
	}
	return "workspace-1", nil
}

// runDaemonCommand runs `mj daemon` (Rust's daemon_command; the
// subcommand type takes the name daemonCommand).
func runDaemonCommand(ctx context.Context, args daemonArgs) error {
	switch args.command.(type) {
	case daemonCommandStatus, daemonCommandStop, daemonCommandRestart:
		forwarded, err := runWhereDaemonLives(ctx, daemonForwardedArgs(), false)
		if err != nil {
			return err
		}
		if forwarded {
			return nil
		}
	}

	switch command := args.command.(type) {
	case daemonCommandEndpoint:
		return printEndpoint(ctx, command.existing)
	case daemonCommandAfk:
		daemon, err := ConnectOrStart(ctx)
		if err != nil {
			return err
		}
		defer daemon.Close()
		if command.command != afkCommandStatus {
			away := command.command == afkCommandOn
			reply, err := daemon.Request(ctx, client.DaemonActionSetOperatorAway{Away: away})
			if err != nil {
				return err
			}
			if _, done := reply.(client.DaemonReplyDone); !done {
				return fmt.Errorf("unexpected operator away reply: %s", client.DaemonReplyDebug(reply))
			}
		}
		frame, err := daemon.RuntimeChanges(ctx, nil, false)
		if err != nil {
			return err
		}
		if frame.Kind != client.RuntimeFrameSnapshot || frame.Projection == nil {
			return errors.New("daemon did not return a runtime snapshot")
		}
		away := "off"
		if frame.Projection.Metadata.OperatorPresence.Away {
			away = "on"
		}
		fmt.Printf("Operator away: %s\n", away)
		return nil
	case daemonCommandStatus:
		daemon, err := client.ConnectManagement(ctx)
		if err != nil {
			// No endpoint file is the ordinary stopped state.
			if stopped, ok := client.DaemonNotRunningOf(err); ok {
				fmt.Printf("Mjolnir daemon is stopped (no %s).\n", stopped.MetadataPath)
				return nil
			}
			return fmt.Errorf("Mjolnir daemon is not answering: %w", err)
		}
		defer daemon.Close()
		status, err := daemon.Status(ctx)
		if err != nil {
			return err
		}
		identity, err := daemon.Identity(ctx)
		if err != nil {
			return err
		}
		location := fmt.Sprint(status.Pid)
		if name, ok := identity.Placement.Container(); ok {
			location = "in container " + name
		}
		version := status.BuildVersion
		if build, err := client.BuildIdentityParse(status.BuildVersion); err == nil {
			version = build.Describe()
		}
		plural := "s"
		if status.AttachedClients == 1 {
			plural = ""
		}
		fmt.Printf("Mjolnir daemon %s (version %s) started %s; %d attached client%s; web viewer %s\n",
			location, version, status.StartedAt, status.AttachedClients, plural, status.PhoneStatus)
		// Two builds can report the same version, so the version line
		// cannot answer "is my rebuilt code running?". The build identity
		// the daemon reports can, and it is the question a development
		// restart turns on.
		runsThisBuild, err := daemonRunsThisBuild(&identity)
		if err != nil {
			return err
		}
		if runsThisBuild != nil {
			if *runsThisBuild {
				fmt.Println("This daemon runs this build.")
			} else {
				daemonBuild := identity.BuildVersion
				if build, err := client.BuildIdentityParse(identity.BuildVersion); err == nil {
					daemonBuild = build.Describe()
				}
				fmt.Printf("This daemon runs build %s, not this client's build (%s); commands work, but code you rebuilt is not running. Run `mj daemon restart` from this build.\n",
					daemonBuild, client.ThisBuild().Describe())
			}
		}
		if daemon.ProtocolVersion() != client.ProtocolVersion {
			fmt.Printf("The daemon speaks protocol %d while this build speaks %d; status/stop/restart work, and other commands will replace it on next use.\n",
				daemon.ProtocolVersion(), client.ProtocolVersion)
		}
	case daemonCommandStop:
		stop := command.args
		if err := core.EnsureMayControlStore(core.DataDir(), "stop the Mjolnir daemon"); err != nil {
			return err
		}
		if stop.suspendSessions {
			stopped, err := suspendAllSessions(ctx, stop.discardUntracked)
			if err != nil {
				return err
			}
			if _, err := stopInstance(ctx, stop.force); err != nil {
				return err
			}
			switch stopped {
			case 0:
				fmt.Println("Mjolnir daemon stopped; no sessions were active.")
			case 1:
				fmt.Println("Mjolnir daemon stopped after stopping its active session.")
			default:
				fmt.Printf("Mjolnir daemon stopped after stopping its %d active sessions.\n", stopped)
			}
			return nil
		}
		stopped, err := stopInstance(ctx, stop.force)
		if err != nil {
			return err
		}
		if !stopped {
			fmt.Println("Mjolnir daemon is already stopped.")
			return nil
		}
		if stop.force {
			fmt.Println("Mjolnir daemon stopped.")
			return nil
		}
		fmt.Printf("Mjolnir daemon stopped; detached workers remain active. `%s` suspends them too.\n", mjCommand("daemon stop --suspend-sessions"))
	case daemonCommandRestart:
		restarted, err := RestartDaemon(ctx)
		if err != nil {
			return err
		}
		// A restart onto another build is an error, so reaching here means
		// the daemon is either proved to be this build or unidentifiable;
		// say which, because "restarted" alone is what used to mislead.
		checked := ""
		if restarted.RunsThisBuild != nil && *restarted.RunsThisBuild {
			checked = ", running this build"
		}
		fmt.Printf("Mjolnir daemon restarted as PID %d%s.\n", restarted.Pid, checked)
	}
	return nil
}

// daemonForwardedArgs is the command line to replay where the daemon lives.
// Placement supplies the destination instance, so the client instance
// selector is removed; every other argument keeps its meaning.
func daemonForwardedArgs() []string {
	var forwarded []string
	args := os.Args[1:]
	for index := 0; index < len(args); index++ {
		argument := args[index]
		if argument == "-i" || argument == "--instance" {
			index++
			continue
		}
		if strings.HasPrefix(argument, "--instance=") || (strings.HasPrefix(argument, "-i") && len(argument) > 2) {
			continue
		}
		forwarded = append(forwarded, argument)
	}
	return forwarded
}

// login runs the harness's own interactive login against a profile's
// canonical home. Mjolnir never sees the credential contents; it compares
// fingerprints before and after so it can tell the operator whether anything
// changed.
//
// The daemon-local login installs mj-controller's termination coordinator
// and runs mj-controller's harness_login, neither of which the Go build has
// yet.
func login(ctx context.Context, args loginArgs) error {
	return loginInner(ctx, args)
}

// loginInner signs in with only the configuration and the profile's own
// home, so it also works before the daemon or its database exists, which is
// the first thing a freshly configured machine does.
func loginInner(ctx context.Context, args loginArgs) error {
	config, err := core.ConfigLoad()
	if err != nil {
		return err
	}
	profileID, err := resolveLoginProfile(&config, args.profile)
	if err != nil {
		return err
	}
	if _, ok := config.Profiles[profileID]; !ok {
		return fmt.Errorf("unknown profile %s; configured profiles: %s", core.RustDebugString(profileID), profileIDs(&config))
	}
	return fmt.Errorf("mj login: signing in runs mj-controller's harness login: %w", errNotPorted)
}

// profileIDs is the enabled profiles' ids, comma separated.
func profileIDs(config *core.Config) string {
	var ids []string
	for id := range config.EnabledProfiles() {
		ids = append(ids, id)
	}
	return strings.Join(ids, ", ")
}

// resolveLoginProfile is the profile `mj login` signs in: the one named, or
// the only enabled one.
func resolveLoginProfile(config *core.Config, requested *string) (string, error) {
	if requested != nil {
		if _, err := config.NamedProfile(*requested); err != nil {
			return "", err
		}
		return *requested, nil
	}
	var enabled []string
	for id := range config.EnabledProfiles() {
		enabled = append(enabled, id)
		if len(enabled) == 2 {
			break
		}
	}
	switch len(enabled) {
	case 1:
		return enabled[0], nil
	case 2:
		return "", fmt.Errorf("several profiles are configured; pass --profile with one of: %s", profileIDs(config))
	}
	return "", errors.New("no enabled agent profiles are configured; add one in Settings → Agent Profiles")
}

// runRecover runs `mj recover` (Rust's recover; recover is a Go builtin).
func runRecover(ctx context.Context, args recoverArgs) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	defer daemon.Close()
	switch command := args.command.(type) {
	case recoverCommandScan:
		scan, err := daemon.ScanRecovery(ctx, command.allInstances)
		if err != nil {
			return err
		}
		if command.json {
			encoded, err := core.SerdeJSONPretty(scan)
			if err != nil {
				return err
			}
			fmt.Println(string(encoded))
			return nil
		}
		// An empty scan printed nothing, which reads the same as a command
		// that did not run (F-16).
		if len(scan.Candidates) == 0 {
			if scan.HiddenOtherInstances == 0 {
				fmt.Println("nothing to recover")
			} else {
				fmt.Println("nothing to recover for this instance")
			}
		}
		for _, candidate := range scan.Candidates {
			instance := "unknown instance"
			if candidate.InstanceID != nil {
				instance = "instance " + *candidate.InstanceID
				if *candidate.InstanceID == scan.InstanceID {
					instance = "this instance"
				}
			}
			metadata := "v1 resource; profile and bundle unknown"
			if candidate.Ownership != nil {
				metadata = "ownership verified"
			}
			// A resource left behind by a session Mjolnir still tracks
			// cannot be adopted under that session id, so say which of the
			// two recovery actions applies to it.
			if candidate.TrackedSession != nil {
				metadata = fmt.Sprintf("left by tracked session in state %s; destroy only", candidate.TrackedSession.AsStr())
			}
			fmt.Printf("%s\t%s\t%s\t%s\n", candidate.SessionID, candidate.TargetTemplateID, instance, metadata)
		}
		if scan.HiddenOtherInstances > 0 {
			fmt.Fprintf(os.Stderr, "note: %s created by other or unknown instances were not listed; pass --all-instances to include them\n",
				core.Counted(uint64(scan.HiddenOtherInstances), "worker", "workers"))
		}
		for _, warning := range scan.Warnings {
			fmt.Fprintf(os.Stderr, "warning: %s\n", warning)
		}
		return nil
	case recoverCommandAdopt:
		interrupted, stop := signal.NotifyContext(ctx, os.Interrupt)
		defer stop()
		var installed bool
		var mu sync.Mutex
		err := daemon.AdoptRecovery(interrupted, client.RecoveryAdoptionRequest{
			SessionID:    command.session,
			TargetID:     command.target,
			Profile:      command.profile,
			Bundle:       command.bundle,
			AllInstances: command.allInstances,
		}, func(event client.RecoveryAdoptionProgress) {
			switch event := event.(type) {
			case client.RecoveryAdoptionProgressWaiting:
				fmt.Fprintf(os.Stderr, "Waiting for %s.\n", core.SanitizeTerminalText(strings.Join(event.Blockers, "; ")))
				if event.DestroyCommand != nil {
					fmt.Fprintf(os.Stderr, "If the worker never becomes idle, the only way forward is to destroy it: %s\n", *event.DestroyCommand)
				}
			case client.RecoveryAdoptionProgressInstalled:
				mu.Lock()
				installed = true
				mu.Unlock()
				fmt.Fprintf(os.Stderr, "adopted worker %s; reattaching\n", event.SessionID)
			}
		})
		if interrupted.Err() != nil && ctx.Err() == nil {
			mu.Lock()
			adopted := installed
			mu.Unlock()
			if adopted {
				return errors.New("worker adopted; reattaching interrupted; check this instance's session state")
			}
			return errors.New("adoption outcome unknown: no final reply was received; the install may still complete. Check this instance's session state before retrying recover adopt")
		}
		if err != nil {
			return err
		}
		fmt.Printf("adopted worker %s\n", command.session)
		return nil
	case recoverCommandDestroy:
		if err := daemon.DestroyRecovery(ctx, command.session, command.target, command.confirm, command.allInstances); err != nil {
			return err
		}
		fmt.Printf("destroyed orphan worker resource %s\n", command.session)
		return nil
	}
	return nil
}

// printSetupInstructions prints mj-setup's host preparation instructions for
// the platform. mj-setup is not ported, so it says so.
func printSetupInstructions(parsed doctorCommand) error {
	instructions := parsed.(doctorCommandInstructions)
	return fmt.Errorf("mj doctor instructions --platform %s: the instructions are mj-setup's: %w", instructions.platform, errNotPorted)
}

// catalogWait is how long doctor waits for the daemon to report one
// profile's catalogue. A profile whose discovery is still running is
// reported as not checked.
const catalogWait = 10 * time.Second

// doctor_catalogs (the catalogues each enabled profile advertises, read from
// a daemon that is already running; doctor never starts one) returns
// mj-setup's doctor::Catalogs and is declared when mj-setup is ported.

// doctor runs `mj doctor`'s prerequisite checks, which are mj-setup's.
func doctor(ctx context.Context, args doctorArgs) error {
	return fmt.Errorf("mj doctor: the prerequisite checks are mj-setup's: %w", errNotPorted)
}

// doctorFailure is doctor's closing error. The human report already prints
// every fix, so its closing line points at them rather than send the user to
// `--json` for the same text.
func doctorFailure(json bool) error {
	if json {
		return errors.New("Mjolnir has fixable prerequisites; follow the `remediation` of each `fixable` check.")
	}
	return errors.New("Mjolnir has fixable prerequisites; follow the remediation lines under each `fixable` check above.")
}

// Rust re-exports mj_core::state::short_id here as the prefix every message
// uses when it names a session; Go callers use core.ShortID.

// terminalGuard owns the terminal while a dashboard runs: raw mode, the
// alternate screen, mouse capture, bracketed paste, unambiguous key
// reporting where the terminal supports it, and the window title. Rust draws
// through ratatui over crossterm; the Charm terminal handle it holds in Go is
// chosen when the dashboard is ported, so until then the guard refuses to
// take the terminal rather than half-own it.
type terminalGuard struct {
	keyboardEnhancement bool
	// title is the window title last written, so it is only rewritten when
	// it changes and cleared on exit only if it was ever set.
	title *string
}

// errTerminalNotPorted is why the guard does not take the terminal.
var errTerminalNotPorted = fmt.Errorf("the dashboard's terminal: %w", errNotPorted)

// terminalGuardEnter takes the terminal (TerminalGuard::enter). The first
// entry asks the terminal for its background before anything reads input.
func terminalGuardEnter() (*terminalGuard, error) {
	return nil, errTerminalNotPorted
}

// draw draws one frame as a synchronized update (DEC mode 2026), so a
// terminal applies the frame as a whole; terminals without the mode ignore
// it.
func (g *terminalGuard) draw(render func(*chat.Frame)) error {
	return errTerminalNotPorted
}

// ringBell rings the terminal bell. Terminals, multiplexers, and SSH clients
// pass BEL through, which is what makes it the one signal that works
// everywhere.
func (g *terminalGuard) ringBell() error {
	return errTerminalNotPorted
}

// setTitle sets the terminal window title, writing only when it changed.
func (g *terminalGuard) setTitle(title string) error {
	return errTerminalNotPorted
}

// copyToTerminalClipboard hands text to the terminal's own clipboard with
// OSC 52, the path that works over SSH and inside multiplexers, where the
// desktop clipboard the process can reach is the wrong machine's.
func (g *terminalGuard) copyToTerminalClipboard(text string) error {
	return errTerminalNotPorted
}

// close restores the terminal (Rust's Drop). A guard that never took the
// terminal has nothing to restore.
func (g *terminalGuard) close() {}
