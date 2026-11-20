// The mj command tree. Port of the Cli and Command types in
// mj-cli/src/main.rs; each command's behaviour lives beside it as it is ported.

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
const Version = "2.73.0"

// globalOptions are the options the Rust Cli struct holds.
type globalOptions struct {
	instance  string
	noSplash  bool
	workspace string
}

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
	var options globalOptions
	root := &cobra.Command{
		Use:     "mj",
		Short:   "ACP session control plane",
		Version: Version,
		Args:    unrecognizedSubcommand,
		PersistentPreRunE: func(cmd *cobra.Command, _ []string) error {
			var flag *string
			if cmd.Flags().Changed("instance") {
				flag = &options.instance
			}
			return core.ApplyInstanceFlag(flag)
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
	stringFlag(persistent, &options.instance, flagSpec{Name: "instance", Short: "i", ValueName: "NAME", Env: core.InstanceEnv, Order: 0,
		Help: "Isolate configuration, database, daemon, and logs under `instances/<name>`. This is a different `mj` world, not a harness profile; explicit `MJ_CONFIG_DIR`/`MJ_DATA_DIR` still take precedence"})
	boolFlag(persistent, &options.noSplash, flagSpec{Name: "no-splash", Env: "MJ_NO_SPLASH", Order: 1,
		Help: "Start the dashboard without the animated startup splash, for example over a slow remote connection"})
	stringFlag(root.Flags(), &options.workspace, flagSpec{Name: "workspace", ValueName: "NAME", Order: 2,
		Help: "Open the dashboard in this workspace, by name. Given before a command, it applies to that command when the command works in a workspace"})
	if value, ok := os.LookupEnv("MJ_NO_SPLASH"); ok && value != "" && value != "false" && value != "0" {
		options.noSplash = true
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
