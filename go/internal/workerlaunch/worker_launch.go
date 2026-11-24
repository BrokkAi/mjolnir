// Port of mj-worker-launch/src/worker_launch.rs.

// Launching a worker: what a session's worker is given to run with, and the
// staging that puts it on the target, from the configuration and the
// records.
//
// Rust's `config: &Config` and `state: &State` stand in for the
// `Controller` these were methods on; in Go they are *core.Config and
// *core.StateState. Rust's `&impl CommandExecutor` is core.CommandExecutor,
// `targets::TargetLocator` is core.TargetsTargetLocator, and an
// `Option<T>` result is a pointer, a nil interface, or a trailing ok bool.

package workerlaunch

import (
	"context"
	"errors"
	"fmt"
	"maps"

	"github.com/BrokkAi/mjolnir/go/internal/buildcache"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessiontypes"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

// AwsCredentialSource is where this session's AWS credentials come from;
// nil when it receives none.
func AwsCredentialSource(config *core.Config, state *core.StateState, sessionID string) (*core.AwsCredentialSource, error) {
	profile, backend, environment, err := AwsSessionInputs(config, state, sessionID)
	if err != nil {
		return nil, err
	}
	return targets.Source(profile, &backend, environment)
}

// AwsCredentialSyncSource is AwsCredentialSource for the credential sync:
// the same for a profile without a role, and for one with a role every
// session that may hold Mjolnir's credentials stays managed (see
// targets.SyncSource).
func AwsCredentialSyncSource(config *core.Config, state *core.StateState, sessionID string) (*core.AwsCredentialSource, error) {
	profile, backend, environment, err := AwsSessionInputs(config, state, sessionID)
	if err != nil {
		return nil, err
	}
	return targets.SyncSource(profile, &backend, environment)
}

// AwsRoleNotice is a notice for the person when the profile's
// `[profiles.<id>.aws]` role cannot apply to this session, and why: its
// target is raw local or EC2, or sets its own AWS credentials, or the profile
// is not a Bedrock one. ok is false when there is nothing to say.
func AwsRoleNotice(config *core.Config, state *core.StateState, sessionID string) (notice string, ok bool, err error) {
	profile, backend, environment, err := AwsSessionInputs(config, state, sessionID)
	if err != nil {
		return "", false, err
	}
	profileID := state.Sessions.Index(sessionID).LastProfile
	notice, ok = targets.UnusedRoleNotice(profileID, profile, targets.SessionHostFromTargetLocator(&backend), environment)
	return notice, ok, nil
}

// AwsSessionInputs is what decides whether this session receives synced AWS
// credentials: its profile, its backend locator and its target environment.
func AwsSessionInputs(config *core.Config, state *core.StateState, sessionID string) (*core.HarnessProfile, core.TargetsTargetLocator, map[string]string, error) {
	fail := func(err error) (*core.HarnessProfile, core.TargetsTargetLocator, map[string]string, error) {
		return nil, core.TargetsTargetLocator{}, nil, err
	}
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return fail(errors.New("session is missing"))
	}
	profile, ok := config.Profiles[session.LastProfile]
	if !ok {
		return fail(errors.New("session profile is missing"))
	}
	if session.Target == nil {
		return fail(errors.New("session target is missing"))
	}
	backend, err := targets.BackendLocator(session.Target, session, config)
	if err != nil {
		return fail(err)
	}
	runtime, err := session.TargetRuntimeSettings(config)
	if err != nil {
		return fail(err)
	}
	return &profile, backend, maps.Clone(runtime.Environment), nil
}

// WorkerPlacement is where this session's worker lives: its backend locator
// and its worker root. This is decided from the session record and
// configuration alone, so a caller can name the worker root before anything
// is installed into it. ctx carries the session's worker permit, when there
// is one (Rust: worker_lifecycle::current).
func WorkerPlacement(ctx context.Context, config *core.Config, state *core.StateState, sessionID string) (core.TargetsTargetLocator, string, error) {
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return core.TargetsTargetLocator{}, "", fmt.Errorf("unknown session %s", sessionID)
	}
	if session.Target == nil {
		return core.TargetsTargetLocator{}, "", errors.New("session target is missing")
	}
	if owner := store.Current(ctx, sessionID); owner != nil {
		if err := owner.VerifyTarget(session.Target); err != nil {
			return core.TargetsTargetLocator{}, "", err
		}
	}
	backend, err := targets.BackendLocator(session.Target, session, config)
	if err != nil {
		return core.TargetsTargetLocator{}, "", err
	}
	workerRoot, err := core.WorkerRoot(&backend, sessionID)
	if err != nil {
		return core.TargetsTargetLocator{}, "", err
	}
	return backend, workerRoot, nil
}

// PrepareWorkerFiles installs a new worker's files. ctx must carry the
// session's worker permit (Rust: worker_lifecycle::require).
func PrepareWorkerFiles(ctx context.Context, config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator, workerRoot string, executor core.CommandExecutor, history workerstaging.LaunchHistory) error {
	_, err := PrepareWorkerFilesFor(ctx, config, state, sessionID, backend, workerRoot, executor, workerstaging.WorkerFilePreparationInstall, history)
	return err
}

// PrepareWorkerFilesFor stages the worker's files for preparation and
// prepares the installed managed harness. Returns the login identity when
// refreshing the profile for an existing worker; nil otherwise.
func PrepareWorkerFilesFor(ctx context.Context, config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator, workerRoot string, executor core.CommandExecutor, preparation workerstaging.WorkerFilePreparation, history workerstaging.LaunchHistory) (core.LoginFingerprint, error) {
	launch, stagedLogin, err := StageWorkerFilesFor(ctx, config, state, sessionID, backend, workerRoot, executor, preparation, history)
	if err != nil {
		return nil, err
	}
	if launch != nil {
		if err := workerstaging.PrepareInstalledManagedHarness(executor, backend, workerRoot, launch); err != nil {
			return nil, err
		}
	}
	return stagedLogin, nil
}

// StageWorkerFilesFor returns the pending harness preflight (the launch
// configuration, nil when there is none) so a new checkout can finish first,
// and the login identity when refreshing the profile for an existing worker
// (nil otherwise). ctx must carry the session's worker permit (Rust:
// worker_lifecycle::require).
func StageWorkerFilesFor(ctx context.Context, config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator, workerRoot string, executor core.CommandExecutor, preparation workerstaging.WorkerFilePreparation, history workerstaging.LaunchHistory) (*core.WorkerLaunchConfig, core.LoginFingerprint, error) {
	panic("unported: mj-worker-launch/src/worker_launch.rs stage_worker_files_for")
}

// InstallBuildCacheShim stages and verifies the session's mbx, then installs
// marked launchers in the session's `bin` directory on the worker PATH.
func InstallBuildCacheShim(config *core.Config, state *core.StateState, session *core.SessionRecord, backend *core.TargetsTargetLocator, launch *core.WorkerLaunchConfig, executor core.CommandExecutor) error {
	panic("unported: mj-worker-launch/src/worker_launch.rs install_build_cache_shim")
}

func PrepareBuildCacheLinks(config *core.Config, state *core.StateState, session *core.SessionRecord, backend *core.TargetsTargetLocator, launch *core.WorkerLaunchConfig, executor core.CommandExecutor) error {
	return InstallBuildCacheShim(config, state, session, backend, launch, executor)
}

// BuildCacheConfiguration is the session's prepared mbx configuration
// directory and the configuration roots its launch reads.
func BuildCacheConfiguration(config *core.Config, state *core.StateState, session *core.SessionRecord, backend *core.TargetsTargetLocator, launch *core.WorkerLaunchConfig, executor core.CommandExecutor) (string, []string, error) {
	if session.BuildCache == nil {
		return "", nil, errors.New("session has no build cache")
	}
	configuration, err := buildcache.PrepareSessionConfiguration(config, backend, session.BuildCache, executor)
	if err != nil {
		return "", nil, err
	}
	return configuration, workerstaging.BuildCacheConfigRoots(launch), nil
}

// DiagnoseWorker probes the installed binary and the worker's recorded state
// after a session becomes unreachable. ok is false only when the session has
// no target to probe; a probe that fails says why.
func DiagnoseWorker(config *core.Config, state *core.StateState, sessionID string) (diagnosis string, ok bool) {
	return DiagnoseWorkerControlled(config, state, sessionID, core.ProcessExecutor{})
}

// DiagnoseWorkerControlled is DiagnoseWorker with the given executor.
func DiagnoseWorkerControlled(config *core.Config, state *core.StateState, sessionID string, executor core.CommandExecutor) (diagnosis string, ok bool) {
	panic("unported: mj-worker-launch/src/worker_launch.rs diagnose_worker_controlled")
}

// WorkerRecoveryPlan is a non-destructive liveness probe plus commands that
// replace a confirmed dead session worker without touching its durable relay
// files. The session manager runs both off its async actor. ctx carries the
// session's worker permit, when there is one (see WorkerPlacement).
func WorkerRecoveryPlan(ctx context.Context, config *core.Config, state *core.StateState, sessionID string, operation *core.MoveOperation) (sessiontypes.WorkerRecoveryPlan, error) {
	panic("unported: mj-worker-launch/src/worker_launch.rs worker_recovery_plan")
}

// SessionLaunchConfig is the launch config for a session, including what
// depends on its sub-agent role, its project memory configuration (nil when
// it has none), and its target profile home. The first launch and every
// relaunch use this, so a relaunched worker keeps its delegation tools and a
// relaunched child keeps its parent's workspace.
func SessionLaunchConfig(config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator, history workerstaging.LaunchHistory) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig, string, error) {
	panic("unported: mj-worker-launch/src/worker_launch.rs session_launch_config")
}

// CurrentWorkerLaunchConfig is the session's launch config under its stored
// move operation, if any.
func CurrentWorkerLaunchConfig(config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator) (core.WorkerLaunchConfig, error) {
	operation, err := store.LoadMoveOperation(sessionID)
	if err != nil {
		return core.WorkerLaunchConfig{}, err
	}
	return WorkerLaunchConfigForMove(config, state, sessionID, backend, operation)
}

// WorkerLaunchConfigForMove is the session's launch config under operation
// (nil when there is none): a checkpoint-only move of the session's own
// source runs the worker in checkpoint-only mode.
func WorkerLaunchConfigForMove(config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator, operation *core.MoveOperation) (core.WorkerLaunchConfig, error) {
	panic("unported: mj-worker-launch/src/worker_launch.rs worker_launch_config_for_move")
}

// ProjectMemorySyncTarget is where the session's project memory is
// synchronized, or nil for a session that has none.
func ProjectMemorySyncTarget(config *core.Config, state *core.StateState, sessionID string) (*sessiontypes.ProjectMemorySyncTarget, error) {
	panic("unported: mj-worker-launch/src/worker_launch.rs project_memory_sync_target")
}

// ConfigureStagedReviewMCP writes servers into the staged profile of a
// harness that reads its MCP configuration from disk.
//
// Claude Code reads `mcpServers` from `.claude.json` in its config
// directory; Kimi reads `mcpServers` from `mcp.json` in its home, and needs
// the runtime id its own schema carries. Both files are the reviewer's
// private copy, so nothing here can reach the user's own configuration.
//
// The file is rewritten as serde_json writes a `serde_json::Value` without
// preserve_order: keys sorted, two-space indentation, no HTML escaping,
// numbers kept as written, and a final newline.
func ConfigureStagedReviewMCP(harness core.HarnessKind, profileStage string, servers []core.ReviewMcpServer) error {
	panic("unported: mj-worker-launch/src/worker_launch.rs configure_staged_review_mcp")
}

// ReconnectCommand is the command that opens a relay connection to a
// session's worker.
func ReconnectCommand(config *core.Config, state *core.StateState, sessionID string) (core.CommandSpec, error) {
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return core.CommandSpec{}, fmt.Errorf("unknown session %s", sessionID)
	}
	if err := session.ValidateConfiguration(config); err != nil {
		return core.CommandSpec{}, err
	}
	if session.Target == nil {
		return core.CommandSpec{}, errors.New("session has no target")
	}
	backend, err := targets.BackendLocator(session.Target, session, config)
	if err != nil {
		return core.CommandSpec{}, err
	}
	plan, err := targets.ReconnectPlan(&backend, sessionID)
	if err != nil {
		return core.CommandSpec{}, err
	}
	if len(plan.Commands) == 0 {
		return core.CommandSpec{}, errors.New("reconnect plan is empty")
	}
	return plan.Commands[0], nil
}
