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
// return memory themselves), and the test-fixture startup macro.

package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
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

// bind declares the flattened `--workspace NAME` option on cmd (the clap
// derive of WorkspaceName).
func (w *workspaceName) bind(cmd *cobra.Command) {
	panic("unported: mj-cli/src/main.rs WorkspaceName (clap Args)")
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
	panic("unported: mj-cli/src/main.rs RemovedCommandArgs (clap Args)")
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
	panic("unported: mj-cli/src/main.rs WorkspacesArgs (clap Args)")
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
	panic("unported: mj-cli/src/main.rs DaemonArgs (clap Args)")
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
	panic("unported: mj-cli/src/main.rs DaemonStopArgs (clap Args)")
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
	panic("unported: mj-cli/src/main.rs MoveArgs (clap Args)")
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
	panic("unported: mj-cli/src/main.rs LoginArgs (clap Args)")
}

// recoverArgs are the arguments of `mj recover`.
type recoverArgs struct {
	command recoverCommand
}

// bind declares the recover subcommands on cmd; run runs the parsed
// command.
func (args *recoverArgs) bind(cmd *cobra.Command, run func() error) {
	panic("unported: mj-cli/src/main.rs RecoverArgs (clap Args)")
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
	panic("unported: mj-cli/src/main.rs DoctorArgs (clap Args)")
}

// setupArgs are the arguments of the hidden `mj setup`.
type setupArgs struct {
	command doctorCommand
}

// bind declares the setup subcommands on cmd.
func (args *setupArgs) bind(cmd *cobra.Command, run func() error) {
	panic("unported: mj-cli/src/main.rs SetupArgs (clap Args)")
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
// the worker build identity, test hooks, the instance environment, the
// daemon's working directory, logging and panic logging, then run; a
// daemon-run refused because another daemon owns the store exits with
// core's store-owned exit code. Main in root.go parses and calls it.
func runMain(parsed cli) error {
	panic("unported: mj-cli/src/main.rs main")
}

// maxRuntimeThreads is the cap Rust puts on Tokio's worker threads: the
// daemon mostly waits on I/O, and on a 120-CPU host the idle workers'
// wakeups cost more than they add. Whether the Go daemon needs the same cap
// (as GOMAXPROCS) is measured when the daemon is ported.
const maxRuntimeThreads = 16

// run runs the parsed command: the dashboard's startup update check, the
// command, and the bounded shutdown that follows.
func run(parsed cli) error {
	panic("unported: mj-cli/src/main.rs run")
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
	panic("unported: mj-cli/src/main.rs session_roots")
}

// sessionRootsFromProjection is sessionRoots for one projection.
func sessionRootsFromProjection(
	records *core.SnapshotMap[string, core.SessionRecord],
	subagents *core.SnapshotMap[string, core.SubagentMetadata],
) map[string]listedSession {
	panic("unported: mj-cli/src/main.rs session_roots_from_projection")
}

// activeSessions is the roots the daemon still holds on its dashboard: the
// same lifecycle category the web API and the dashboard call active.
func activeSessions(ctx context.Context, daemon *client.DaemonClient) (map[string]listedSession, error) {
	panic("unported: mj-cli/src/main.rs active_sessions")
}

// awaitStoppingSession waits for a session that is already suspending, or
// being destroyed, to leave the dashboard, and says how it ended. The daemon
// owns that operation and decides when it ends; this only reads its outcome.
func awaitStoppingSession(ctx context.Context, sessionID string) (string, error) {
	panic("unported: mj-cli/src/main.rs await_stopping_session")
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
	panic("unported: mj-cli/src/main.rs suspend_all_sessions")
}

// installPanicLogging sends a panic's report to the log rather than to a
// detached standard error, which may itself be full.
func installPanicLogging() {
	panic("unported: mj-cli/src/main.rs install_panic_logging")
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
	panic("unported: mj-cli/src/main.rs shutdown_daemon_runtime")
}

// shutdownDashboardRuntime abandons the dashboard's disposable blocking
// reads, which must not delay process exit.
func shutdownDashboardRuntime(runtime *core.Runtime) {
	panic("unported: mj-cli/src/main.rs shutdown_dashboard_runtime")
}

// runCommand runs one parsed command. Rust returns the dashboard's
// DashboardExit and takes the startup update notice (mj-controller's
// StartupUpdateNotice); both belong to the dashboard, which is not ported, so
// the Go declaration returns only the error until dashboard.rs is.
func runCommand(ctx context.Context, parsed command, requestedWorkspace *string, noSplash bool) error {
	panic("unported: mj-cli/src/main.rs run_command")
}

// moveSession runs the daemon-owned move operation from the one-shot CLI.
//
// Preparation is deliberately a separate request: it lets unattended
// callers prove that a queue choice is explicit and lets interactive
// callers show the interruption warning before the source is stopped. Once
// admitted, a lost CLI connection does not cancel the daemon operation; only
// an explicit Ctrl-C does.
func moveSession(ctx context.Context, args moveArgs) error {
	panic("unported: mj-cli/src/main.rs move_session")
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
}

// newLaunchWarnings connects for launch notices, or reports why it could
// not and reads none.
func newLaunchWarnings(ctx context.Context) *launchWarnings {
	panic("unported: mj-cli/src/main.rs LaunchWarnings::new")
}

// launchWarningsConnect connects and starts after the newest notice the
// daemon already holds.
func launchWarningsConnect(ctx context.Context) (*launchWarnings, error) {
	panic("unported: mj-cli/src/main.rs LaunchWarnings::connect")
}

// report prints the session's new launch notices, or why they could not be
// read.
func (w *launchWarnings) report(ctx context.Context, sessionID string) {
	panic("unported: mj-cli/src/main.rs LaunchWarnings::report")
}

// read prints the session's notices newer than the last one seen.
func (w *launchWarnings) read(ctx context.Context, sessionID string) error {
	panic("unported: mj-cli/src/main.rs LaunchWarnings::read")
}

// promptQueueChoice asks what to do with queued work; discarding is the
// default.
func promptQueueChoice(ctx context.Context, pending int) (core.ResumeQueueDisposition, error) {
	panic("unported: mj-cli/src/main.rs prompt_queue_choice")
}

// printMoveConversion says what moving a local checkout into an isolated
// workspace will do. This prints for `--yes` too: the checkout stays behind
// and uncommitted work is copied, which a person should be able to read in
// the transcript of the command afterwards.
func printMoveConversion(preparation *core.MovePreparation) {
	panic("unported: mj-cli/src/main.rs print_move_conversion")
}

// printMoveDestinationSize prints the size the destination container will
// have. `mj move` shows it for `--yes` too, because the default a Move picks
// is not something the person typed and has to be readable afterwards.
func printMoveDestinationSize(preparation *core.MovePreparation) {
	panic("unported: mj-cli/src/main.rs print_move_destination_size")
}

// promptMoveConfirmation shows what the move will interrupt and asks to
// continue.
func promptMoveConfirmation(ctx context.Context, preparation *core.MovePreparation, queue *core.ResumeQueueDisposition) (bool, error) {
	panic("unported: mj-cli/src/main.rs prompt_move_confirmation")
}

// confirmMovePrompt asks `Continue? [y/N]` on standard error and reads the
// answer.
func confirmMovePrompt() (bool, error) {
	panic("unported: mj-cli/src/main.rs confirm_move_prompt")
}

// formatMoveDryRun is what `mj move --dry-run` prints without `--json`.
func formatMoveDryRun(route *core.RoutePreview, skipKeepSizes []core.SkipKeepSize, preflight *core.SuspendPreflight) string {
	panic("unported: mj-cli/src/main.rs format_move_dry_run")
}

// moveErrorOutcome is a failed outcome for a prepared move.
func moveErrorOutcome(preparation *core.MovePreparation, message string) core.MoveOutcome {
	panic("unported: mj-cli/src/main.rs move_error_outcome")
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
	panic("unported: mj-cli/src/main.rs print_move_human")
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
	panic("unported: mj-cli/src/main.rs resolve_store_workspace")
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
	panic("unported: mj-cli/src/main.rs workspace_required")
}

// listedWorkspacesWithoutStarting is the workspaces as a running daemon
// lists them or, when none runs and the daemon's store lives on this
// computer, as that store holds them. Never starts a daemon. ok is false
// when no list could be read, which includes a store in a daemon
// container's volume.
func listedWorkspacesWithoutStarting(ctx context.Context) (workspaces []core.WorkspaceRecord, ok bool) {
	panic("unported: mj-cli/src/main.rs listed_workspaces_without_starting")
}

// storedWorkspacesOnThisComputer is the workspaces in a host daemon's store,
// read while no daemon runs. Only a host placement has its store here; a
// container's is in its volume. A store that does not exist yet has none;
// one that cannot be read answers ok false.
func storedWorkspacesOnThisComputer() (workspaces []core.WorkspaceRecord, ok bool) {
	panic("unported: mj-cli/src/main.rs stored_workspaces_on_this_computer")
}

// refuseUnknownWorkspace refuses a `--workspace` name that no workspace
// carries, before anything starts a daemon (launch findings R5-9 and R6-2).
// The list comes from a running daemon or, when none runs, from the store,
// as in workspaceRequired. A name that cannot be checked is let through; the
// daemon refuses it later if it is unknown.
func refuseUnknownWorkspace(ctx context.Context, name string) error {
	panic("unported: mj-cli/src/main.rs refuse_unknown_workspace")
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
	panic("unported: mj-cli/src/main.rs unknown_workspace")
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
	panic("unported: mj-cli/src/main.rs workspace_name_for_directory")
}

// suggestedWorkspaceName is the name of the workspace plain `mj` creates
// when there is none: the folder's name, or `workspace` for an SSH daemon,
// numbered past the names already taken.
func suggestedWorkspaceName(workspaces []client.WorkspaceListing) (string, error) {
	panic("unported: mj-cli/src/main.rs suggested_workspace_name")
}

// runDaemonCommand runs `mj daemon` (Rust's daemon_command; the
// subcommand type takes the name daemonCommand).
func runDaemonCommand(ctx context.Context, args daemonArgs) error {
	panic("unported: mj-cli/src/main.rs daemon_command")
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
func login(ctx context.Context, args loginArgs) error {
	panic("unported: mj-cli/src/main.rs login")
}

// loginInner signs in with only the configuration and the profile's own
// home, so it also works before the daemon or its database exists, which is
// the first thing a freshly configured machine does.
func loginInner(ctx context.Context, args loginArgs) error {
	panic("unported: mj-cli/src/main.rs login_inner")
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
	panic("unported: mj-cli/src/main.rs recover")
}

// printSetupInstructions prints mj-setup's host preparation instructions for
// the platform.
func printSetupInstructions(parsed doctorCommand) {
	panic("unported: mj-cli/src/main.rs print_setup_instructions")
}

// catalogWait is how long doctor waits for the daemon to report one
// profile's catalogue. A profile whose discovery is still running is
// reported as not checked.
const catalogWait = 10 * time.Second

// doctor_catalogs (the catalogues each enabled profile advertises, read from
// a daemon that is already running; doctor never starts one) returns
// mj-setup's doctor::Catalogs and is declared when mj-setup is ported.

// doctor runs `mj doctor`'s prerequisite checks.
func doctor(ctx context.Context, args doctorArgs) error {
	panic("unported: mj-cli/src/main.rs doctor")
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
// chosen when the dashboard is ported.
type terminalGuard struct {
	keyboardEnhancement bool
	// title is the window title last written, so it is only rewritten when
	// it changes and cleared on exit only if it was ever set.
	title *string
}

// terminalGuardEnter takes the terminal (TerminalGuard::enter). The first
// entry asks the terminal for its background before anything reads input.
func terminalGuardEnter() (*terminalGuard, error) {
	panic("unported: mj-cli/src/main.rs TerminalGuard::enter")
}

// draw draws one frame as a synchronized update (DEC mode 2026), so a
// terminal applies the frame as a whole; terminals without the mode ignore
// it.
func (g *terminalGuard) draw(render func(*chat.Frame)) error {
	panic("unported: mj-cli/src/main.rs TerminalGuard::draw")
}

// ringBell rings the terminal bell. Terminals, multiplexers, and SSH clients
// pass BEL through, which is what makes it the one signal that works
// everywhere.
func (g *terminalGuard) ringBell() error {
	panic("unported: mj-cli/src/main.rs TerminalGuard::ring_bell")
}

// setTitle sets the terminal window title, writing only when it changed.
func (g *terminalGuard) setTitle(title string) error {
	panic("unported: mj-cli/src/main.rs TerminalGuard::set_title")
}

// copyToTerminalClipboard hands text to the terminal's own clipboard with
// OSC 52, the path that works over SSH and inside multiplexers, where the
// desktop clipboard the process can reach is the wrong machine's.
func (g *terminalGuard) copyToTerminalClipboard(text string) error {
	panic("unported: mj-cli/src/main.rs TerminalGuard::copy_to_terminal_clipboard")
}

// close restores the terminal (Rust's Drop): clears a title this process
// set, pops keyboard enhancement, leaves the alternate screen and input
// modes, leaves raw mode and shows the cursor, logging each failure.
func (g *terminalGuard) close() {
	panic("unported: mj-cli/src/main.rs TerminalGuard::drop")
}
