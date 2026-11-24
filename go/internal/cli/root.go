// The mj command tree. Port of the cli and command types in
// mj-cli/src/main.rs; each command's behaviour lives beside it as it is ported
// (main.go holds the rest of main.rs).

package cli

import (
	"errors"
	"fmt"
	"io"
	"os"

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

// topLevelCommands lists the visible commands in the order mj --help shows them.
var topLevelCommands = []struct{ name, about string }{
	{"image", "Build and inspect project session images from mise recipes"},
	{"go", "Work in a folder using remembered account and target settings"},
	{"simple", "Open a minimal local session with direct Ctrl-key commands"},
	{"workspaces", "Open the workspace selector even when Mjolnir could auto-attach, or list and create workspaces without a terminal"},
	{"daemon", "Inspect or control the persistent per-user daemon"},
	{"acp", "Serve the Agent Client Protocol on standard input and output, running each session it creates through this daemon"},
	{"doctor", "Diagnose platform and configuration prerequisites"},
	{"ec2", "Create, list, or destroy persistent EC2 hosts with rootless Podman"},
	{"setup", "Rerun agent and repository discovery, preserving existing configuration"},
	{"import", "Adopt a native coding-agent session as a stopped Mjolnir session"},
	{"recover", "Find, adopt, or explicitly destroy managed workers missing from state"},
	{"move", "Move a session to another configured profile and/or target"},
	{"upgrade-container", "Preview or explicitly queue a storage-preserving container upgrade"},
	{"move-sources", "Inspect stopped sources retained by Move, or explicitly remove one"},
	{"login", "Sign in a profile, installing its pinned harness when needed. Live sessions pick up fresh credentials"},
	{"new", "Create a session through the API and print its id"},
	{"prompt", "Send a prompt as the user and wait for its turn to end"},
	{"clear-queue", "Cancel queued prompts while preserving the session and its current turn"},
	{"event", "Queue an external event in a session's durable mailbox"},
	{"message", "Send a message and return: from your shell, a prompt; inside a session, a message to another session or one of your sub-agents"},
	{"wait", "Block until a turn ends and print how it ended"},
	{"transcript", "Page through a session's transcript"},
	{"usage", "Read recorded token usage and coverage"},
	{"events", "Follow durable session events as line-delimited JSON"},
	{"put-file", "Atomically upload a file to an idle session workspace"},
	{"fetch", "Copy a file, or a directory as a zip archive, from a session's target into this machine's temporary directory and print where it landed"},
	{"elicitations", "List structured input requests from a session"},
	{"respond", "Respond to a structured input request"},
	{"diff", "Print a unified diff of a session's work"},
	{"export", "Get a session's work out as a patch, a pushed branch, or a git bundle"},
	{"sessions", "List the sessions the daemon holds"},
	{"suspend", "Save a recovery copy and release the environment for later Resume"},
	{"purge-untracked", "Remove legacy untracked checkpoint payloads from stopped sessions"},
	{"destroy", "Permanently destroy a session, its environment, and recovery archive"},
	{"notices", "Read recent cleanup notices, including after a session was destroyed"},
	{"resume", "Resume a stopped session from its checkpoint"},
	{"interrupt-turn", "Cancel the turn a session is running"},
	{"review", "Review a session's committed work, or show the review it has open"},
	{"stop-task", "Stop a session's background task, or all tasks the worker can stop"},
	{"api-info", "Print the API base URL"},
	{"api-token", "Print only the API bearer token for external HTTP clients"},
	{"github-token", "Print a valid GitHub App token for an owner or repository"},
	{"ui", "Change what the person's dashboard shows: pane sizes, which session is open, or Settings with changes staged for them to save"},
	{"models", "Discover available models and efforts for a profile"},
	{"quota", "Show each profile's remaining quota as the daemon last read it"},
	{"hosts", "Show each host's CPU, RAM and disk space as the daemon last read it"},
	{"set-config", "Apply a session configuration setting"},
}

// removedCommands are accepted only so the old name says what replaced it.
var removedCommands = map[string]string{
	"checkpoint":  "`mj checkpoint` was removed; `mj suspend` saves the session",
	"close":       "`mj close` was replaced by `mj suspend` and `mj destroy`",
	"cancel-turn": "`mj cancel-turn` was replaced by `mj interrupt-turn`",
}

// errNotPorted marks a command whose Rust behaviour the Go build does not
// have yet. It fails visibly instead of pretending to work.
var errNotPorted = errors.New("this command is not yet ported to the Go build of mj")

// NewRoot builds the mj command tree.
func NewRoot(args []string) *cobra.Command {
	// clap lists commands in declaration order.
	cobra.EnableCommandSorting = false
	var parsed cli
	// The flags bind to text; cli keeps clap's Option, so absent stays nil.
	var instance, workspace string
	root := &cobra.Command{
		Use:     "mj",
		Short:   "ACP session control plane",
		Version: Version,
		Args:    unrecognizedSubcommand,
		PersistentPreRunE: func(cmd *cobra.Command, _ []string) error {
			if cmd.Flags().Changed("instance") {
				parsed.instance = &instance
			}
			if cmd.Flags().Changed("workspace") {
				parsed.workspace = &workspace
			}
			return core.ApplyInstanceFlag(parsed.instance)
		},
		RunE: func(*cobra.Command, []string) error {
			return fmt.Errorf("the dashboard: %w", errNotPorted)
		},
	}
	installClapStyle(root, args)
	root.SetHelpCommand(&cobra.Command{Use: "help", Hidden: true, Run: func(cmd *cobra.Command, args []string) {
		target, _, err := root.Find(args)
		if err != nil || target == nil {
			target = root
		}
		writeHelp(cmd.OutOrStdout(), target, true)
	}})
	persistent := root.PersistentFlags()
	stringFlag(persistent, &instance, flagSpec{Name: "instance", Short: "i", ValueName: "NAME", Env: core.InstanceEnv, Order: 0,
		Help: "Isolate configuration, database, daemon, and logs under `instances/<name>`. This is a different `mj` world, not a harness profile; explicit `MJ_CONFIG_DIR`/`MJ_DATA_DIR` still take precedence"})
	boolFlag(persistent, &parsed.noSplash, flagSpec{Name: "no-splash", Env: "MJ_NO_SPLASH", Order: 1,
		Help: "Start the dashboard without the animated startup splash, for example over a slow remote connection"})
	stringFlag(root.Flags(), &workspace, flagSpec{Name: "workspace", ValueName: "NAME", Order: 2,
		Help: "Open the dashboard in this workspace, by name. Given before a command, it applies to that command when the command works in a workspace"})
	if value, ok := os.LookupEnv("MJ_NO_SPLASH"); ok && value != "" && value != "false" && value != "0" {
		parsed.noSplash = true
	}

	for _, entry := range topLevelCommands {
		root.AddCommand(&cobra.Command{
			Use:   entry.name,
			Short: entry.about,
			RunE: func(*cobra.Command, []string) error {
				return fmt.Errorf("mj %s: %w", entry.name, errNotPorted)
			},
		})
	}
	for name, replacement := range removedCommands {
		root.AddCommand(&cobra.Command{
			Use:                name,
			Hidden:             true,
			DisableFlagParsing: true,
			RunE: func(*cobra.Command, []string) error {
				return errors.New(replacement)
			},
		})
	}
	return root
}

// Main runs mj with the process arguments and returns its exit code.
func Main(args []string, stdout, stderr io.Writer) int {
	root := NewRoot(args)
	root.SetArgs(args)
	root.SetOut(stdout)
	root.SetErr(stderr)
	if err := root.Execute(); err != nil {
		return reportError(stderr, err)
	}
	return 0
}
