// The mj command tree. Port of the cli and command types in
// mj-cli/src/main.rs; each command's arguments are declared by its bind
// method beside the command (main.go holds the rest of main.rs).

package cli

import (
	"errors"
	"io"
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/spf13/cobra"
)

// Version is the release this build reports; it tracks the Rust workspace.
const Version = core.PackageVersion

// cli is what `mj` parsed from its command line (Rust's clap Cli).
type cli struct {
	// instance isolates configuration, database, daemon, and logs under
	// `instances/<name>`.
	instance *string
	// noSplash starts the dashboard without the animated startup splash.
	noSplash bool
	// workspace opens the dashboard in this workspace, by name. Given before
	// a command, it applies to that command when the command works in a
	// workspace.
	workspace *string
	// command is nil for plain `mj`, which opens the dashboard.
	command command
}

// command is one `mj` subcommand and its arguments (Rust's Command enum).
// The variants are the command* types below; plain `mj` has none.
type command interface{ isCommand() }

// commandImage is `mj image`: build and inspect project session images
// from mise recipes. Its arguments (image_commands::ImageArgs) arrive with
// image_commands.rs, which is not ported yet.
type commandImage struct{}

// commandGo is `mj go`: work in a folder using remembered account and
// target settings. Its arguments (go::GoArgs) arrive with go.rs.
type commandGo struct{}

// commandSimple is `mj simple`: a minimal local session with direct
// Ctrl-key commands. Its arguments (simple::SimpleArgs) arrive with
// simple.rs.
type commandSimple struct{}

// commandWorkspaces is `mj workspaces`.
type commandWorkspaces struct{ args workspacesArgs }

// commandDaemon is `mj daemon`.
type commandDaemon struct{ args daemonArgs }

// commandDaemonRun is the hidden `mj daemon-run`, the persistent controller
// process.
type commandDaemonRun struct{}

// commandDaemonWaitExit is the hidden `mj daemon-wait-exit`.
type commandDaemonWaitExit struct{ args daemonWaitExitArgs }

// commandAcp is `mj acp`: serve the Agent Client Protocol on standard input
// and output. Its arguments (acp::AcpArgs) arrive with acp.rs.
type commandAcp struct{}

// commandDoctor is `mj doctor`.
type commandDoctor struct{ args doctorArgs }

// commandEc2 is `mj ec2`. Its arguments (ec2::Ec2Args) arrive with ec2.rs.
type commandEc2 struct{}

// commandSetup is the hidden `mj setup`, the former spelling of
// `mj doctor instructions`.
type commandSetup struct{ args setupArgs }

// commandImport is `mj import`. Its arguments (import::ImportArgs) arrive
// with import.rs.
type commandImport struct{}

// commandRecover is `mj recover`.
type commandRecover struct{ args recoverArgs }

// commandCheckpoint is the removed `mj checkpoint`.
type commandCheckpoint struct{ args removedCommandArgs }

// commandMove is `mj move`.
type commandMove struct{ args moveArgs }

// commandUpgradeContainer is `mj upgrade-container`: preview or explicitly
// queue a storage-preserving container upgrade.
type commandUpgradeContainer struct {
	// session is the session whose existing container should be inspected
	// or upgraded.
	session string
	// acknowledgeWritableLayerLoss accepts loss of writable-layer state
	// after reviewing the image preview. Conflicts with cancel.
	acknowledgeWritableLayerLoss bool
	// cancel cancels a pending upgrade before replacement starts.
	cancel bool
}

// commandMoveSources is `mj move-sources`: inspect stopped sources retained
// by Move, or explicitly remove one.
type commandMoveSources struct {
	// session is the session whose retained Move sources should be listed
	// or cleaned up.
	session string
	// cleanup is the operation ID of the retained source to delete.
	cleanup *string
	// yes confirms deletion of the retained source and its excluded files.
	yes bool
}

// commandLogin is `mj login`.
type commandLogin struct{ args loginArgs }

// commandNew is `mj new`.
type commandNew struct{ args newArgs }

// commandPrompt is `mj prompt`.
type commandPrompt struct{ args promptArgs }

// commandClearQueue is `mj clear-queue`.
type commandClearQueue struct{ args clearQueueArgs }

// commandEvent is `mj event`.
type commandEvent struct{ args eventArgs }

// commandMessage is `mj message`.
type commandMessage struct{ args messageArgs }

// commandWait is `mj wait`.
type commandWait struct{ args waitArgs }

// commandTranscript is `mj transcript`.
type commandTranscript struct{ args transcriptArgs }

// commandUsage is `mj usage`.
type commandUsage struct{ args usageArgs }

// commandEvents is `mj events`.
type commandEvents struct{ args eventsArgs }

// commandPutFile is `mj put-file`.
type commandPutFile struct{ args putFileArgs }

// commandFetch is `mj fetch`.
type commandFetch struct{ args fetchArgs }

// commandElicitations is `mj elicitations`.
type commandElicitations struct{ args elicitationsArgs }

// commandRespond is `mj respond`.
type commandRespond struct{ args respondArgs }

// commandDiff is `mj diff`.
type commandDiff struct{ args diffArgs }

// commandExport is `mj export`.
type commandExport struct{ args exportArgs }

// commandSessions is `mj sessions`.
type commandSessions struct{ args sessionsArgs }

// commandSuspend is `mj suspend`.
type commandSuspend struct{ args suspendArgs }

// commandPurgeUntracked is `mj purge-untracked`.
type commandPurgeUntracked struct{ args purgeUntrackedArgs }

// commandDestroy is `mj destroy`.
type commandDestroy struct{ args destroyArgs }

// commandNotices is `mj notices`.
type commandNotices struct{ args noticesArgs }

// commandResume is `mj resume`.
type commandResume struct{ args resumeArgs }

// commandRestart is `mj restart`: restart a running session's agent
// (replaces pre-release workers in place).
type commandRestart struct{ args sessionArgs }

// commandInterruptTurn is `mj interrupt-turn`.
type commandInterruptTurn struct{ args sessionArgs }

// commandReview is `mj review`.
type commandReview struct{ args reviewArgs }

// commandStopTask is `mj stop-task`.
type commandStopTask struct{ args stopTaskArgs }

// commandApiInfo is `mj api-info`.
type commandApiInfo struct{ args apiInfoArgs }

// commandApiToken is `mj api-token`.
type commandApiToken struct{}

// commandGithubToken is `mj github-token`.
type commandGithubToken struct{ args githubTokenArgs }

// commandUi is `mj ui`.
type commandUi struct{ args uiArgs }

// commandModels is `mj models`.
type commandModels struct{ args modelsArgs }

// commandQuota is `mj quota`.
type commandQuota struct{ args quotaArgs }

// commandHosts is `mj hosts`.
type commandHosts struct{ args hostsArgs }

// commandSetConfig is `mj set-config`.
type commandSetConfig struct{ args setConfigArgs }

// commandClose is the removed `mj close`.
type commandClose struct{ args removedCommandArgs }

// commandCancelTurn is the removed `mj cancel-turn`.
type commandCancelTurn struct{ args removedCommandArgs }

func (commandImage) isCommand()            {}
func (commandGo) isCommand()               {}
func (commandSimple) isCommand()           {}
func (commandWorkspaces) isCommand()       {}
func (commandDaemon) isCommand()           {}
func (commandDaemonRun) isCommand()        {}
func (commandDaemonWaitExit) isCommand()   {}
func (commandAcp) isCommand()              {}
func (commandDoctor) isCommand()           {}
func (commandEc2) isCommand()              {}
func (commandSetup) isCommand()            {}
func (commandImport) isCommand()           {}
func (commandRecover) isCommand()          {}
func (commandCheckpoint) isCommand()       {}
func (commandMove) isCommand()             {}
func (commandUpgradeContainer) isCommand() {}
func (commandMoveSources) isCommand()      {}
func (commandLogin) isCommand()            {}
func (commandNew) isCommand()              {}
func (commandPrompt) isCommand()           {}
func (commandClearQueue) isCommand()       {}
func (commandEvent) isCommand()            {}
func (commandMessage) isCommand()          {}
func (commandWait) isCommand()             {}
func (commandTranscript) isCommand()       {}
func (commandUsage) isCommand()            {}
func (commandEvents) isCommand()           {}
func (commandPutFile) isCommand()          {}
func (commandFetch) isCommand()            {}
func (commandElicitations) isCommand()     {}
func (commandRespond) isCommand()          {}
func (commandDiff) isCommand()             {}
func (commandExport) isCommand()           {}
func (commandSessions) isCommand()         {}
func (commandSuspend) isCommand()          {}
func (commandPurgeUntracked) isCommand()   {}
func (commandDestroy) isCommand()          {}
func (commandNotices) isCommand()          {}
func (commandResume) isCommand()           {}
func (commandRestart) isCommand()          {}
func (commandInterruptTurn) isCommand()    {}
func (commandReview) isCommand()           {}
func (commandStopTask) isCommand()         {}
func (commandApiInfo) isCommand()          {}
func (commandApiToken) isCommand()         {}
func (commandGithubToken) isCommand()      {}
func (commandUi) isCommand()               {}
func (commandModels) isCommand()           {}
func (commandQuota) isCommand()            {}
func (commandHosts) isCommand()            {}
func (commandSetConfig) isCommand()        {}
func (commandClose) isCommand()            {}
func (commandCancelTurn) isCommand()       {}

// errNotPorted marks a command whose Rust behaviour the Go build does not
// have yet. It fails visibly instead of pretending to work.
var errNotPorted = errors.New("this command is not yet ported to the Go build of mj")

// leaf makes cmd run run once clap's checks pass.
func leaf(cmd *cobra.Command, run func() error) {
	cmd.Args = cobra.ArbitraryArgs
	cmd.RunE = func(c *cobra.Command, args []string) error {
		if err := clapOf(c).finish(args); err != nil {
			return err
		}
		return run()
	}
}

// group makes cmd a command whose positional arguments are subcommands.
// Without one, run runs when the subcommand is optional; a required one
// prints the help, as clap does.
func group(cmd *cobra.Command, subcommandRequired bool, run func() error) {
	meta := clapOf(cmd)
	meta.subcommandRequired = subcommandRequired
	cmd.Args = unrecognizedSubcommand
	cmd.RunE = func(c *cobra.Command, args []string) error {
		if subcommandRequired {
			return &helpRequested{command: c}
		}
		if err := clapOf(c).finish(nil); err != nil {
			return err
		}
		return run()
	}
}

// subcommand is a new command for one subcommand variant.
func subcommand(name, about string) *cobra.Command {
	return &cobra.Command{Use: name, Short: about}
}

// notPortedCommand is a command whose arguments arrive with its own file:
// until then it accepts any arguments, and running it says it is not
// ported.
func notPortedCommand(name, about string, run func() error) *cobra.Command {
	cmd := subcommand(name, about)
	cmd.DisableFlagParsing = true
	cmd.RunE = func(*cobra.Command, []string) error { return run() }
	return cmd
}

// newCommandTree builds the mj command tree. onParsed receives what the
// command line asked for once clap's checks have passed.
func newCommandTree(onParsed func(cli) error) *cobra.Command {
	// clap lists commands in declaration order.
	cobra.EnableCommandSorting = false
	var parsed cli
	root := &cobra.Command{Use: "mj", Short: "ACP session control plane", Version: Version}
	installClapStyle(root)
	rootClap := clapOf(root)
	rootClap.option("instance",
		"Isolate configuration, database, daemon, and logs under `instances/<name>`. This is a different `mj` world, not a harness profile; explicit `MJ_CONFIG_DIR`/`MJ_DATA_DIR` still take precedence",
		storeOptional(&parsed.instance), withShort("i"), global(), withEnv(core.InstanceEnv), withValueName("NAME"))
	rootClap.flag(&parsed.noSplash, "no_splash",
		"Start the dashboard without the animated startup splash, for example over a slow remote connection",
		global(), withEnv("MJ_NO_SPLASH"))
	rootClap.option("workspace",
		"Open the dashboard in this workspace, by name. Given before a command, it applies to that command when the command works in a workspace",
		storeOptional(&parsed.workspace), withValueName("NAME"))
	root.Flags().BoolP("version", "V", false, "Print version")
	root.SetHelpCommand(&cobra.Command{Use: "help", Hidden: true, Run: func(cmd *cobra.Command, args []string) {
		target, _, err := root.Find(args)
		if err != nil || target == nil {
			target = root
		}
		writeHelp(cmd.OutOrStdout(), target)
	}})
	group(root, false, func() error { return onParsed(parsed) })
	if value, ok := os.LookupEnv("MJ_NO_SPLASH"); ok && value != "" && value != "false" && value != "0" {
		parsed.noSplash = true
	}
	// done records the command and hands the command line on.
	done := func(command command) func() error {
		return func() error {
			parsed.command = command
			return onParsed(parsed)
		}
	}
	add := func(cmd *cobra.Command) { rootClap.subcommand(cmd) }

	add(notPortedCommand("image", "Build and inspect project session images from mise recipes", done(commandImage{})))
	add(notPortedCommand("go", "Work in a folder using remembered account and target settings", done(commandGo{})))
	add(notPortedCommand("simple", "Open a minimal local session with direct Ctrl-key commands", done(commandSimple{})))

	var workspaces workspacesArgs
	workspacesCmd := subcommand("workspaces", "Open the workspace selector even when Mjolnir could auto-attach, or list and create workspaces without a terminal")
	add(workspacesCmd)
	workspaces.bind(workspacesCmd, func() error { return done(commandWorkspaces{workspaces})() })

	var daemon daemonArgs
	daemonCmd := subcommand("daemon", "Inspect or control the persistent per-user daemon")
	add(daemonCmd)
	daemon.bind(daemonCmd, func() error { return done(commandDaemon{daemon})() })

	daemonRun := subcommand("daemon-run", "Internal persistent controller process")
	daemonRun.Hidden = true
	add(daemonRun)
	leaf(daemonRun, func() error { return done(commandDaemonRun{})() })

	var waitExit daemonWaitExitArgs
	waitExitCmd := subcommand("daemon-wait-exit", "")
	waitExitCmd.Hidden = true
	add(waitExitCmd)
	bindDaemonWaitExit(&waitExit, waitExitCmd, func() error { return done(commandDaemonWaitExit{waitExit})() })

	add(notPortedCommand("acp", "Serve the Agent Client Protocol on standard input and output, running each session it creates through this daemon", done(commandAcp{})))

	var doctor doctorArgs
	doctorCmd := subcommand("doctor", "Diagnose platform and configuration prerequisites")
	add(doctorCmd)
	doctor.bind(doctorCmd, func() error { return done(commandDoctor{doctor})() })

	add(notPortedCommand("ec2", "Create, list, or destroy persistent EC2 hosts with rootless Podman", done(commandEc2{})))

	var setup setupArgs
	setupCmd := subcommand("setup", "Former spelling of `mj doctor instructions`")
	setupCmd.Hidden = true
	add(setupCmd)
	setup.bind(setupCmd, func() error { return done(commandSetup{setup})() })

	add(notPortedCommand("import", "Adopt a native coding-agent session as a stopped Mjolnir session", done(commandImport{})))

	var recover recoverArgs
	recoverCmd := subcommand("recover", "Find, adopt, or explicitly destroy managed workers missing from state")
	add(recoverCmd)
	recover.bind(recoverCmd, func() error { return done(commandRecover{recover})() })

	var checkpoint removedCommandArgs
	checkpointCmd := subcommand("checkpoint", "Removed; suspend saves the session")
	checkpointCmd.Hidden = true
	add(checkpointCmd)
	checkpoint.bind(checkpointCmd, func() error { return done(commandCheckpoint{checkpoint})() })

	var move moveArgs
	moveCmd := subcommand("move", "Move a session to another configured profile and/or target")
	add(moveCmd)
	move.bind(moveCmd, func() error { return done(commandMove{move})() })

	var upgrade commandUpgradeContainer
	upgradeCmd := subcommand("upgrade-container", "Preview or explicitly queue a storage-preserving container upgrade")
	add(upgradeCmd)
	upgradeClap := clapOf(upgradeCmd)
	upgradeClap.option("session", "Session whose existing container should be inspected or upgraded", storeString(&upgrade.session), required())
	upgradeClap.flag(&upgrade.acknowledgeWritableLayerLoss, "acknowledge_writable_layer_loss", "Accept loss of writable-layer state after reviewing the image preview", conflictsWith("cancel"))
	upgradeClap.flag(&upgrade.cancel, "cancel", "Cancel a pending upgrade before replacement starts")
	leaf(upgradeCmd, func() error { return done(upgrade)() })

	var sources commandMoveSources
	sourcesCmd := subcommand("move-sources", "Inspect stopped sources retained by Move, or explicitly remove one")
	add(sourcesCmd)
	sourcesClap := clapOf(sourcesCmd)
	sourcesClap.option("session", "Session whose retained Move sources should be listed or cleaned up", storeString(&sources.session), required())
	sourcesClap.option("cleanup", "Operation ID of the retained source to delete", storeOptional(&sources.cleanup))
	sourcesClap.flag(&sources.yes, "yes", "Confirm deletion of the retained source and its excluded files")
	leaf(sourcesCmd, func() error { return done(sources)() })

	var loginArguments loginArgs
	loginCmd := subcommand("login", "Sign in a profile, installing its pinned harness when needed. Live sessions pick up fresh credentials")
	add(loginCmd)
	loginArguments.bind(loginCmd, func() error { return done(commandLogin{loginArguments})() })

	var newArguments newArgs
	newCmd := subcommand("new", "Create a session through the API and print its id")
	add(newCmd)
	newArguments.bind(newCmd, func() error { return done(commandNew{newArguments})() })

	var promptArguments promptArgs
	promptCmd := subcommand("prompt", "Send a prompt as the user and wait for its turn to end")
	add(promptCmd)
	promptArguments.bind(promptCmd, func() error { return done(commandPrompt{promptArguments})() })

	var clearQueueArguments clearQueueArgs
	clearQueueCmd := subcommand("clear-queue", "Cancel queued prompts while preserving the session and its current turn")
	add(clearQueueCmd)
	clearQueueArguments.bind(clearQueueCmd, func() error { return done(commandClearQueue{clearQueueArguments})() })

	var eventArguments eventArgs
	eventCmd := subcommand("event", "Queue an external event in a session's durable mailbox")
	add(eventCmd)
	eventArguments.bind(eventCmd, func() error { return done(commandEvent{eventArguments})() })

	var messageArguments messageArgs
	messageCmd := subcommand("message", "Send a message and return: from your shell, a prompt; inside a session, a message to another session or one of your sub-agents")
	add(messageCmd)
	messageArguments.bind(messageCmd, func() error { return done(commandMessage{messageArguments})() })

	var waitArguments waitArgs
	waitCmd := subcommand("wait", "Block until a turn ends and print how it ended")
	add(waitCmd)
	waitArguments.bind(waitCmd, func() error { return done(commandWait{waitArguments})() })

	var transcriptArguments transcriptArgs
	transcriptCmd := subcommand("transcript", "Page through a session's transcript")
	add(transcriptCmd)
	transcriptArguments.bind(transcriptCmd, func() error { return done(commandTranscript{transcriptArguments})() })

	var usageArguments usageArgs
	usageCmd := subcommand("usage", "Read recorded token usage and coverage")
	add(usageCmd)
	usageArguments.bind(usageCmd, func() error { return done(commandUsage{usageArguments})() })

	var eventsArguments eventsArgs
	eventsCmd := subcommand("events", "Follow durable session events as line-delimited JSON")
	add(eventsCmd)
	eventsArguments.bind(eventsCmd, func() error { return done(commandEvents{eventsArguments})() })

	var putFileArguments putFileArgs
	putFileCmd := subcommand("put-file", "Atomically upload a file to an idle session workspace")
	add(putFileCmd)
	putFileArguments.bind(putFileCmd, func() error { return done(commandPutFile{putFileArguments})() })

	var fetchArguments fetchArgs
	fetchCmd := subcommand("fetch", "Copy a file, or a directory as a zip archive, from a session's target into this machine's temporary directory and print where it landed")
	add(fetchCmd)
	fetchArguments.bind(fetchCmd, func() error { return done(commandFetch{fetchArguments})() })

	var elicitationsArguments elicitationsArgs
	elicitationsCmd := subcommand("elicitations", "List structured input requests from a session")
	add(elicitationsCmd)
	elicitationsArguments.bind(elicitationsCmd, func() error { return done(commandElicitations{elicitationsArguments})() })

	var respondArguments respondArgs
	respondCmd := subcommand("respond", "Respond to a structured input request")
	add(respondCmd)
	respondArguments.bind(respondCmd, func() error { return done(commandRespond{respondArguments})() })

	var diffArguments diffArgs
	diffCmd := subcommand("diff", "Print a unified diff of a session's work")
	add(diffCmd)
	diffArguments.bind(diffCmd, func() error { return done(commandDiff{diffArguments})() })

	var exportArguments exportArgs
	exportCmd := subcommand("export", "Get a session's work out as a patch, a pushed branch, or a git bundle")
	add(exportCmd)
	exportArguments.bind(exportCmd, func() error { return done(commandExport{exportArguments})() })

	var sessionsArguments sessionsArgs
	sessionsCmd := subcommand("sessions", "List the sessions the daemon holds")
	add(sessionsCmd)
	sessionsArguments.bind(sessionsCmd, func() error { return done(commandSessions{sessionsArguments})() })

	var suspendArguments suspendArgs
	suspendCmd := subcommand("suspend", "Save a recovery copy and release the environment for later Resume")
	add(suspendCmd)
	suspendArguments.bind(suspendCmd, func() error { return done(commandSuspend{suspendArguments})() })

	var purgeArguments purgeUntrackedArgs
	purgeCmd := subcommand("purge-untracked", "Remove legacy untracked checkpoint payloads from stopped sessions")
	add(purgeCmd)
	purgeArguments.bind(purgeCmd, func() error { return done(commandPurgeUntracked{purgeArguments})() })

	var destroyArguments destroyArgs
	destroyCmd := subcommand("destroy", "Permanently destroy a session, its environment, and recovery archive")
	add(destroyCmd)
	destroyArguments.bind(destroyCmd, func() error { return done(commandDestroy{destroyArguments})() })

	var noticesArguments noticesArgs
	noticesCmd := subcommand("notices", "Read recent cleanup notices, including after a session was destroyed")
	add(noticesCmd)
	noticesArguments.bind(noticesCmd, func() error { return done(commandNotices{noticesArguments})() })

	var resumeArguments resumeArgs
	resumeCmd := subcommand("resume", "Resume a stopped session from its checkpoint")
	add(resumeCmd)
	resumeArguments.bind(resumeCmd, func() error { return done(commandResume{resumeArguments})() })

	var restartArguments sessionArgs
	restartCmd := subcommand("restart", "Restart a running session's agent (replaces pre-release workers in place)")
	add(restartCmd)
	restartArguments.bind(restartCmd, func() error { return done(commandRestart{restartArguments})() })

	var interruptArguments sessionArgs
	interruptCmd := subcommand("interrupt-turn", "Cancel the turn a session is running")
	add(interruptCmd)
	interruptArguments.bind(interruptCmd, func() error { return done(commandInterruptTurn{interruptArguments})() })

	var reviewArguments reviewArgs
	reviewCmd := subcommand("review", "Review a session's committed work, or show the review it has open")
	add(reviewCmd)
	reviewArguments.bind(reviewCmd, func() error { return done(commandReview{reviewArguments})() })

	var stopTaskArguments stopTaskArgs
	stopTaskCmd := subcommand("stop-task", "Stop a session's background task, or all tasks the worker can stop")
	add(stopTaskCmd)
	stopTaskArguments.bind(stopTaskCmd, func() error { return done(commandStopTask{stopTaskArguments})() })

	var apiInfoArguments apiInfoArgs
	apiInfoCmd := subcommand("api-info", "Print the API base URL")
	add(apiInfoCmd)
	apiInfoArguments.bind(apiInfoCmd, func() error { return done(commandApiInfo{apiInfoArguments})() })

	apiTokenCmd := subcommand("api-token", "Print only the API bearer token for external HTTP clients")
	add(apiTokenCmd)
	leaf(apiTokenCmd, func() error { return done(commandApiToken{})() })

	var githubTokenArguments githubTokenArgs
	githubTokenCmd := subcommand("github-token", "Print a valid GitHub App token for an owner or repository")
	add(githubTokenCmd)
	githubTokenArguments.bind(githubTokenCmd, func() error { return done(commandGithubToken{githubTokenArguments})() })

	var uiArguments uiArgs
	uiCmd := subcommand("ui", "Change what the person's dashboard shows: pane sizes, which session is open, or Settings with changes staged for them to save")
	add(uiCmd)
	uiArguments.bind(uiCmd, func() error { return done(commandUi{uiArguments})() })

	var modelsArguments modelsArgs
	modelsCmd := subcommand("models", "Discover available models and efforts for a profile")
	add(modelsCmd)
	modelsArguments.bind(modelsCmd, func() error { return done(commandModels{modelsArguments})() })

	var quotaArguments quotaArgs
	quotaCmd := subcommand("quota", "Show each profile's remaining quota as the daemon last read it")
	add(quotaCmd)
	quotaArguments.bind(quotaCmd, func() error { return done(commandQuota{quotaArguments})() })

	var hostsArguments hostsArgs
	hostsCmd := subcommand("hosts", "Show each host's CPU, RAM and disk space as the daemon last read it")
	add(hostsCmd)
	hostsArguments.bind(hostsCmd, func() error { return done(commandHosts{hostsArguments})() })

	var setConfigArguments setConfigArgs
	setConfigCmd := subcommand("set-config", "Apply a session configuration setting")
	add(setConfigCmd)
	setConfigArguments.bind(setConfigCmd, func() error { return done(commandSetConfig{setConfigArguments})() })

	var closeArguments removedCommandArgs
	closeCmd := subcommand("close", "Removed; kept so the old name says what replaced it")
	closeCmd.Hidden = true
	add(closeCmd)
	closeArguments.bind(closeCmd, func() error { return done(commandClose{closeArguments})() })

	var cancelArguments removedCommandArgs
	cancelCmd := subcommand("cancel-turn", "Removed; kept so the old name says what replaced it")
	cancelCmd.Hidden = true
	add(cancelCmd)
	cancelArguments.bind(cancelCmd, func() error { return done(commandCancelTurn{cancelArguments})() })

	addHelpSubcommandsBelow(root)
	propagateGlobalArgs(root, nil)
	return root
}

// addHelpSubcommandsBelow gives every command below root that has
// subcommands clap's `help` subcommand; the root has cobra's.
func addHelpSubcommandsBelow(root *cobra.Command) {
	for _, child := range root.Commands() {
		if len(child.Commands()) > 0 {
			addHelpSubcommandsBelow(child)
			addHelpSubcommand(child)
		}
	}
}

// bindDaemonWaitExit declares `mj daemon-wait-exit --exit-dir <EXIT_DIR>
// <COMMAND>...`. The command is trailing and may begin with hyphens, so
// everything from its first word on is the command (clap's
// trailing_var_arg and allow_hyphen_values); the arguments are read here
// rather than by pflag.
func bindDaemonWaitExit(args *daemonWaitExitArgs, cmd *cobra.Command, run func() error) {
	cmd.DisableFlagParsing = true
	cmd.RunE = func(c *cobra.Command, raw []string) error {
		exitDir, seen := "", false
		for index := 0; index < len(raw); index++ {
			argument := raw[index]
			switch {
			case argument == "--exit-dir":
				if index+1 >= len(raw) {
					return &UsageError{Message: "a value is required for '--exit-dir <EXIT_DIR>' but none was supplied", Command: c, noUsage: true}
				}
				index++
				exitDir, seen = raw[index], true
			case strings.HasPrefix(argument, "--exit-dir="):
				exitDir, seen = strings.TrimPrefix(argument, "--exit-dir="), true
			case argument == "--":
				args.command = append(args.command, raw[index+1:]...)
				index = len(raw)
			default:
				args.command = append(args.command, raw[index:]...)
				index = len(raw)
			}
		}
		var missing []string
		if !seen {
			missing = append(missing, "--exit-dir <EXIT_DIR>")
		}
		if len(args.command) == 0 {
			missing = append(missing, "<COMMAND>...")
		}
		if len(missing) > 0 {
			return &UsageError{Message: "the following required arguments were not provided:\n  " + strings.Join(missing, "\n  "), Command: c}
		}
		args.exitDir = exitDir
		return run()
	}
}

// parseCommandLine is clap's try_parse_from: what args (without the
// program name) ask for, or the usage error, without running anything.
func parseCommandLine(args []string) (cli, error) {
	var result cli
	var ran bool
	root := newCommandTree(func(parsed cli) error {
		result, ran = parsed, true
		return nil
	})
	root.SetArgs(args)
	root.SetOut(io.Discard)
	root.SetErr(io.Discard)
	if _, err := root.ExecuteC(); err != nil {
		return cli{}, asUsageError(root, err)
	}
	if !ran {
		// --help or --version answered instead of a command.
		return cli{}, &UsageError{Message: "the command line asked for help or the version", Command: root, noUsage: true}
	}
	// A removed command is refused with its replacement; the error names a
	// command that does not run, so a parse that reached it is still a parse.
	return result, nil
}

// asUsageError reports an error from cobra's own parsing (a flag before the
// subcommand, which cobra parses while it finds the subcommand) in clap's
// words.
func asUsageError(root *cobra.Command, err error) error {
	var usage *UsageError
	var help *helpRequested
	if errors.As(err, &usage) || errors.As(err, &help) || errors.Is(err, errNotPorted) {
		return err
	}
	if strings.HasPrefix(err.Error(), "unknown flag: ") || strings.HasPrefix(err.Error(), "unknown shorthand flag: ") ||
		strings.HasPrefix(err.Error(), "flag needs an argument: ") {
		return clapFlagError(root, err)
	}
	return err
}

// Main runs mj with the process arguments and returns its exit code.
func Main(args []string, stdout, stderr io.Writer) int {
	root := newCommandTree(runMain)
	root.SetArgs(args)
	root.SetOut(stdout)
	root.SetErr(stderr)
	if _, err := root.ExecuteC(); err != nil {
		return reportError(stderr, asUsageError(root, err))
	}
	return 0
}
