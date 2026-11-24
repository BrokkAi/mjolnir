// Port of mj-cli/src/go.rs: the arguments of `mj go`. Fast-start
// preparation and the dashboard it opens are not ported yet.

package cli

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
	"github.com/spf13/cobra"
)

// goArgs are the arguments of `mj go`.
type goArgs struct {
	// folder is the source folder (defaults to the current directory).
	folder *string
	// path is an explicit project path on the daemon host. Conflicts with
	// folder.
	path *string
	// setup changes the remembered setup for this folder.
	setup bool
	// globalDefault chooses a setup and also makes it the default for other
	// new projects.
	globalDefault bool
}

// bind declares the go arguments on cmd.
func (args *goArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.positional("folder", "Source folder (defaults to the current directory)", storeOptional(&args.folder))
	meta.option("path", "Explicit project path on the daemon host", storeOptional(&args.path), conflictsWith("folder"))
	meta.flag(&args.setup, "setup", "Change the remembered setup for this folder")
	meta.flag(&args.globalDefault, "global_default", "Choose a setup and also make it the default for other new projects")
	leaf(cmd, run)
}

// resolveGoWorkspace is go.rs's resolve_workspace: the workspace the folder
// of mode belongs to, created when it has none. Declared for the dashboard
// (slice 2b).
func resolveGoWorkspace(ctx context.Context, daemon *client.DaemonClient, mode *tui.GoMode) (string, error) {
	panic("unported: mj-cli/src/go.rs resolve_workspace")
}

// savedWorkspaceModes is every project folder's remembered fast-start mode.
// Declared for the dashboard (slice 2b).
func savedWorkspaceModes() ([]tui.GoMode, error) {
	panic("unported: mj-cli/src/go.rs saved_workspace_modes")
}
