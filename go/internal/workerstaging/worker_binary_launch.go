// Port of mj-worker-staging/src/worker_binary/launch.rs.

package workerstaging

import (
	"errors"
	"slices"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessiontypes"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// OracleLaunchEnabled: workspace owners can use their configured oracle.
func OracleLaunchEnabled(session *core.SessionRecord, ownsEnvironment bool, choices *core.SubagentConfig) bool {
	return ownsEnvironment && len(session.EffectiveOracle(choices)) != 0
}

type WorkerFilePreparation int

const (
	WorkerFilePreparationInstall WorkerFilePreparation = iota
	// WorkerFilePreparationRefreshProfile restages the profile into a
	// retained home, and replaces the worker binary when the retained
	// worker's build differs.
	WorkerFilePreparationRefreshProfile
	// WorkerFilePreparationRefreshProfileOnly restages the profile into a
	// retained home before a stopped worker starts, so it starts with the
	// profile's current login (#1547). The worker binary is the caller's
	// business.
	WorkerFilePreparationRefreshProfileOnly
)

// LaunchHistory is the session history a launch continues:
// LaunchHistoryStored or LaunchHistoryRestored. A restore deletes the
// worker's relay state, so the history the restore brings back decides what
// the launch must not repeat. Everywhere else the stored projection is the
// live history.
type LaunchHistory interface {
	isLaunchHistory()
}

type LaunchHistoryStored struct{}

type LaunchHistoryRestored struct {
	Snapshot *core.CanonicalSessionSnapshot
}

func (LaunchHistoryStored) isLaunchHistory()   {}
func (LaunchHistoryRestored) isLaunchHistory() {}

// PromptStarted is whether this history shows the prompt with commandID as
// started. A transcript item `user:<command id>` appears when a prompt
// starts; a prompt that was only queued has none, so its input never ran.
func PromptStarted(sessionID string, history LaunchHistory, commandID string) (bool, error) {
	stableID := "user:" + commandID
	if restored, ok := history.(LaunchHistoryRestored); ok {
		for _, item := range restored.Snapshot.Transcript {
			if item.StableID == stableID {
				return true, nil
			}
		}
		return false, nil
	}
	return store.MaterializedTranscriptItemExists(sessionID, stableID)
}

func DisableMbxCacheShims(sessionID string, backend *core.TargetsTargetLocator, workerRoot string, executor core.CommandExecutor, reason string) {
	panic("unported: mj-worker-staging/src/worker_binary/launch.rs disable_mbx_cache_shims")
}

// BuildCacheConfigRoots is the distinct `XDG_CONFIG_HOME` values of the
// launch's target and harness environments, in Rust path order.
func BuildCacheConfigRoots(launch *core.WorkerLaunchConfig) []string {
	panic("unported: mj-worker-staging/src/worker_binary/launch.rs build_cache_config_roots")
}

// ApplyJevSettings carries the Jev settings to the worker's launch
// environment. The harness supervisor excludes the worker's Fastino key.
func ApplyJevSettings(launch *core.WorkerLaunchConfig, config *core.JevConfig) {
	panic("unported: mj-worker-staging/src/worker_binary/launch.rs apply_jev_settings")
}

// WorkerWorkspaceForRecovery is the workspace a bare-target worker must be
// able to enter; nil for container and EC2 targets.
func WorkerWorkspaceForRecovery(backend *core.TargetsTargetLocator, directory string) *sessiontypes.WorkerWorkspace {
	var target core.ManagedWorktreeTarget
	switch backend.Kind {
	case core.TargetsTargetLocatorLocalBare:
		target = core.ManagedWorktreeTargetLocal{}
	case core.TargetsTargetLocatorSshBare:
		target = core.ManagedWorktreeTargetSsh{
			Destination: backend.SSH.Destination,
			SSHArgs:     slices.Clone(backend.SSH.SSHArgs),
		}
	default:
		return nil
	}
	return &sessiontypes.WorkerWorkspace{Target: target, Directory: directory}
}

// LaunchWorkspace is the workspace a launch uses and what decides it.
type LaunchWorkspace struct {
	SessionID string
	// Container is the session's recorded container workspace; nil when
	// none.
	Container *string
	// ParentWorktree is the parent's managed worktree for a sub-agent child;
	// nil otherwise.
	ParentWorktree *core.ManagedWorktree
	// Owner is the record that owns the environment, for a borrowing child;
	// nil when the session owns it.
	Owner              *core.SessionRecord
	WorkingDirectories []string
}

// LaunchWorkspacePaths is the primary workspace path and the additional
// repository paths of a launch.
func LaunchWorkspacePaths(checkout core.Checkout, bundle *core.ProjectBundle, backend *core.TargetsTargetLocator, workerWorkspace *LaunchWorkspace) (string, []string, error) {
	if projectDirectory, ok := checkout.ProjectDirectory(); ok {
		return projectDirectory, nil, nil
	}
	if bundle == nil {
		return "", nil, errors.New("session bundle is missing")
	}
	return WorkspacePaths(backend, bundle, workerWorkspace.SessionID, workerWorkspace.Container)
}

// WorkerLaunchConfig is the launch configuration of session's worker, its
// project memory configuration (nil when the session has none), and the
// target profile home.
func WorkerLaunchConfig(session *core.SessionRecord, profile *core.HarnessProfile, bundle *core.ProjectBundle, backend *core.TargetsTargetLocator, workerWorkspace LaunchWorkspace, target *core.TargetRuntimeSettings, config *core.Config) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig, string, error) {
	return WorkerLaunchConfigWithCheckout(session, session.RecordCheckout(), profile, bundle, backend, workerWorkspace, target, config)
}

func WorkerLaunchConfigWithCheckout(session *core.SessionRecord, checkout core.Checkout, profile *core.HarnessProfile, bundle *core.ProjectBundle, backend *core.TargetsTargetLocator, workerWorkspace LaunchWorkspace, target *core.TargetRuntimeSettings, config *core.Config) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig, string, error) {
	panic("unported: mj-worker-staging/src/worker_binary/launch.rs worker_launch_config_with_checkout")
}

// reportedExclusions is the profiles ExcludeHarnessEnvironment has already
// reported removals for (Rust: its REPORTED static).
var reportedExclusions struct {
	mu       sync.Mutex
	profiles map[string]struct{}
}

// ExcludeHarnessEnvironment leaves out of a launch every variable the
// profile's harness must never see, and names them for the worker, which
// removes them again once it has added the target's own login environment.
//
// Saying so once per profile is enough: the launch config is rebuilt for
// every recovery check, and the variables do not change between them.
func ExcludeHarnessEnvironment(profileID string, profile *core.HarnessProfile, environment map[string]string) []string {
	panic("unported: mj-worker-staging/src/worker_binary/launch.rs exclude_harness_environment")
}

// HarnessRuntimePolicy is where a worker on backend gets its harness. Local
// work follows core.LocalHarnessRuntime: the image's harnesses in the
// daemon container, the managed installation on a host.
func HarnessRuntimePolicy(backend *core.TargetsTargetLocator) core.HarnessRuntimePolicy {
	switch backend.Kind {
	case core.TargetsTargetLocatorLocalBare:
		return core.LocalHarnessRuntime()
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		return core.HarnessRuntimePolicyManaged
	default:
		return core.HarnessRuntimePolicyAmbient
	}
}

// WorkerRunsInImage is whether the worker's own environment is an image's:
// a container target, or a local worker beside a daemon in its container.
func WorkerRunsInImage(backend *core.TargetsTargetLocator) bool {
	if _, ok := backend.ContainerEngine(); ok {
		return true
	}
	return HarnessRuntimePolicy(backend) == core.HarnessRuntimePolicyImage
}
