// Port of mj-cli/src/acp.rs: the arguments of `mj acp`. The ACP adapter
// itself is not ported yet.

package cli

import "github.com/spf13/cobra"

// acpArgs say where a consumer tells the adapter its sessions should run.
//
// Every flag is optional, and an omitted one falls back to the pair that
// `mj new` resolves the same way, so `mj acp` alone is a valid agent command.
type acpArgs struct {
	// profile is the profile the session runs its harness from.
	profile *string
	// target is the target template the session is provisioned on.
	target *string
	// bundle is an existing bundle to run. Without one the consumer's
	// working directory is the project, which is what a local target needs.
	bundle *string
	// at starts the workspace checked out at this full commit ID in the
	// bundle's primary repository (`--at SHA`, requires bundle).
	at *string
	// branch is, with at, the new branch to create there; otherwise the
	// existing branch to check out.
	branch *string
	// base records this Git revision as the diff base, when it should not
	// be at (`--base REV`).
	base      *string
	workspace workspaceName
	// onExit is what happens to the sessions this process created when it
	// exits.
	onExit exitPolicy
}

// exitPolicy is what the adapter does with its sessions when its consumer
// is gone.
//
// The policy runs once, when the adapter exits, over every session it
// created: a consumer that sends several prompts to one session, or opens
// several sessions, is not interrupted between them.
type exitPolicy string

const (
	// exitPolicyKeep leaves every session as it is, for a consumer a person
	// follows up on.
	exitPolicyKeep exitPolicy = "keep"
	// exitPolicySuspend checkpoints each session and releases its worker and
	// target, leaving a session a person can resume.
	exitPolicySuspend exitPolicy = "suspend"
	// exitPolicyDestroy destroys each session once its turn has stopped, for
	// a scheduler that takes its answer from the turn and wants nothing left
	// behind.
	exitPolicyDestroy exitPolicy = "destroy"
)

// bind declares the acp options on cmd.
func (args *acpArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("profile", "Profile the session runs its harness from", storeOptional(&args.profile))
	meta.option("target", "Target template the session is provisioned on", storeOptional(&args.target))
	meta.option("bundle", "Existing bundle to run. Without one the consumer's working directory is the project, which is what a local target needs", storeOptional(&args.bundle))
	meta.option("at", "Start the workspace checked out at this full commit ID in the bundle's primary repository", storeOptional(&args.at), withValueName("SHA"), requires("bundle"))
	meta.option("branch", "With --at, the new branch to create there; otherwise the existing branch to check out", storeOptional(&args.branch))
	meta.option("base", "Record this Git revision as the diff base, when it should not be --at", storeOptional(&args.base), withValueName("REV"))
	args.workspace.bind(cmd)
	args.onExit = exitPolicyKeep
	meta.option("on_exit", "What happens to the sessions this process created when it exits", func(raw string) error {
		args.onExit = exitPolicy(raw)
		return nil
	}, valueEnum(
		[2]string{string(exitPolicyKeep), "Leave every session as it is, for a consumer a person follows up on"},
		[2]string{string(exitPolicySuspend), "Checkpoint each session and release its worker and target, leaving a session a person can resume"},
		[2]string{string(exitPolicyDestroy), "Destroy each session once its turn has stopped, for a scheduler that takes its answer from the turn and wants nothing left behind"},
	), defaultValue(string(exitPolicyKeep)))
	leaf(cmd, run)
}
