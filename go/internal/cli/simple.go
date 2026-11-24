// Port of mj-cli/src/simple.rs: the arguments of `mj simple`. The minimal
// local session it opens is not ported yet.

package cli

import "github.com/spf13/cobra"

// simpleArgs are the arguments of `mj simple`.
type simpleArgs struct {
	// folder is the source folder (defaults to the current directory).
	folder *string
	// path is an explicit project path on the daemon host. Conflicts with
	// folder.
	path *string
}

// bind declares the simple arguments on cmd.
func (args *simpleArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.positional("folder", "Source folder (defaults to the current directory)", storeOptional(&args.folder))
	meta.option("path", "Explicit project path on the daemon host", storeOptional(&args.path), conflictsWith("folder"))
	leaf(cmd, run)
}
