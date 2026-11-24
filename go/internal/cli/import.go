// Port of mj-cli/src/import.rs: the arguments of `mj import`. Adopting a
// native session is not ported yet.

package cli

import (
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/spf13/cobra"
)

// importArgs are the arguments of `mj import`: the harness is the
// subcommand.
type importArgs struct {
	harness core.HarnessKind
	args    nativeImportArgs
}

// workspace is the workspace named with the command.
func (args *importArgs) workspace() workspaceName { return args.args.workspace }

// nativeImportArgs are the arguments every `mj import <harness>` subcommand
// takes. The harness is the subcommand name, so the remaining options are
// shared. One of session and latest is required (the group
// `import-session-selection`).
type nativeImportArgs struct {
	// session is the native session UUID to import.
	session *string
	// latest imports the most recently modified session.
	latest bool
	// profile is the daemon profile to associate with the session. Needed
	// when several enabled profiles run this harness.
	profile *string
	// bundle is an existing configured bundle to associate with the imported
	// session.
	bundle *string
	// path is the project directory on the daemon host (defaults to the
	// recorded directory).
	path *string
	// title is the title displayed in Mjolnir's dashboard.
	title *string
	// allowDirtyLocal proceeds after acknowledging dirty detected Git roots
	// (`--allow-dirty`, alias `--allow-dirty-local`).
	allowDirtyLocal bool
	// allowOmittedNonGit proceeds after acknowledging edited non-Git or
	// scratch directories will be omitted.
	allowOmittedNonGit bool
	// reviewers is each reviewer model and optional effort (`--reviewer
	// MODEL[:EFFORT]`); a final colon suffix is effort only for known
	// efforts (minimal, low, medium, high, xhigh, max); a trailing colon
	// escapes it. Repeat up to three times.
	reviewers []string
	// review is whether the imported session is reviewed before push
	// (`push`) or not (`off`). Omitted follows `[review]`: review runs when
	// reviewers are configured.
	review    *string
	workspace workspaceName
}

// bind declares the import subcommands on cmd; run runs the parsed command.
func (args *importArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	for _, harness := range []struct {
		name, about string
		kind        core.HarnessKind
	}{
		{"claude", "Import a session created by vanilla Claude Code", core.HarnessKindClaude},
		{"codex", "Import a session created by vanilla Codex", core.HarnessKindCodex},
		{"kimi", "Import a session created by vanilla Kimi Code", core.HarnessKindKimi},
		{"grok", "Import a session created by vanilla Grok Build", core.HarnessKindGrok},
		{"muse", "Import a session created by Muse Code", core.HarnessKindMuse},
	} {
		var native nativeImportArgs
		harnessCmd := subcommand(harness.name, harness.about)
		meta.subcommand(harnessCmd)
		native.bind(harnessCmd)
		leaf(harnessCmd, func() error {
			args.harness, args.args = harness.kind, native
			return run()
		})
	}
	group(cmd, true, nil)
}

// bind declares the shared import options on cmd.
func (args *nativeImportArgs) bind(cmd *cobra.Command) {
	meta := clapOf(cmd)
	meta.option("session", "Native session UUID to import", storeOptional(&args.session))
	meta.flag(&args.latest, "latest", "Import the most recently modified session")
	meta.option("profile", "Daemon profile to associate with the session. Needed when several enabled profiles run this harness", storeOptional(&args.profile))
	meta.option("bundle", "Existing configured bundle to associate with the imported session", storeOptional(&args.bundle))
	meta.option("path", "Project directory on the daemon host (defaults to the recorded directory)", storeOptional(&args.path))
	meta.option("title", "Title displayed in Mjolnir's dashboard", storeOptional(&args.title))
	meta.flag(&args.allowDirtyLocal, "allow_dirty_local", "Proceed after acknowledging dirty detected Git roots", withLong("allow-dirty"), visibleAlias("allow-dirty-local"))
	meta.flag(&args.allowOmittedNonGit, "allow_omitted_non_git", "Proceed after acknowledging edited non-Git or scratch directories will be omitted")
	meta.option("reviewers", "Reviewer model and optional effort; final colon suffix is effort only for known efforts (minimal, low, medium, high, xhigh, max); trailing colon escapes it. Repeat up to three times", appendString(&args.reviewers),
		withLong("reviewer"), withValueName("MODEL[:EFFORT]"), multiple())
	meta.option("review", "Whether the imported session is reviewed before push (`push`) or not (`off`). Omitted follows `[review]`: review runs when reviewers are configured", storeOptional(&args.review), withValueName("off|push"))
	args.workspace.bind(cmd)
	meta.group("import-session-selection", true, false, "session", "latest")
}

// The dashboard's import of a native session. Declared for the dashboard
// (slice 2b); the import itself is not ported yet.

// pendingDashboardImport is the native session the dashboard was asked to
// import.
type pendingDashboardImport struct {
	profileID       string
	nativeSessionID string
	displayTitle    string
}

// dashboardImportSafety is what the person settled about the import's
// checkout before it starts.
type dashboardImportSafety struct {
	createManagedWorktree *bool
	accepted              bool
	projectRoot           *string
}

// importBundlePrompt asks how to bundle a project the import found.
type importBundlePrompt struct {
	recordedRoot      string
	managedWorktree   core.ManagedWorktreeOptions
	dirtyGitRoots     []string
	omittedNonGitDirs []string
	scratchGitRoots   []string
}

// dashboardImportSuccess is a finished import.
type dashboardImportSuccess struct {
	harness string
	// archiveDirectory is the temporary directory the archive was staged
	// in, removed with the success (Rust's tempfile::TempDir).
	archiveDirectory string
	sessionID        string
	controller       controller.Controller
}

// dashboardImportTaskResult is how an import task ended.
type dashboardImportTaskResult interface{ isDashboardImportTaskResult() }

type dashboardImportTaskResultNeedsBundle struct{ prompt importBundlePrompt }

type dashboardImportTaskResultImported struct{ success *dashboardImportSuccess }

type dashboardImportTaskResultCancelled struct{}

func (dashboardImportTaskResultNeedsBundle) isDashboardImportTaskResult() {}
func (dashboardImportTaskResultImported) isDashboardImportTaskResult()    {}
func (dashboardImportTaskResultCancelled) isDashboardImportTaskResult()   {}

// dashboardImportUpdate is one report from a running import task.
type dashboardImportUpdate interface{ isDashboardImportUpdate() }

type dashboardImportUpdateUploading struct{ taskID uint64 }

type dashboardImportUpdateRegistering struct{ taskID uint64 }

type dashboardImportUpdateProgress struct {
	taskID  uint64
	step    int
	total   *int
	message string
}

// dashboardImportUpdateFinished carries the task's result, or err when it
// failed (Rust's Box<Result<DashboardImportTaskResult>>).
type dashboardImportUpdateFinished struct {
	taskID  uint64
	pending pendingDashboardImport
	result  dashboardImportTaskResult
	err     error
}

func (dashboardImportUpdateUploading) isDashboardImportUpdate()   {}
func (dashboardImportUpdateRegistering) isDashboardImportUpdate() {}
func (dashboardImportUpdateProgress) isDashboardImportUpdate()    {}
func (dashboardImportUpdateFinished) isDashboardImportUpdate()    {}

// dashboardImportRequest is one import the dashboard starts.
type dashboardImportRequest struct {
	workspaceID string
	pending     pendingDashboardImport
	safety      dashboardImportSafety
	taskID      uint64
	cancelled   *atomic.Bool
}

// spawnDashboardImport runs request off the loop, reporting on updates.
func spawnDashboardImport(controller *controller.Controller, request dashboardImportRequest, updates chan<- dashboardImportUpdate, tracker client.CriticalOperationTracker) {
	panic("unported: mj-cli/src/import.rs spawn_dashboard_import")
}
