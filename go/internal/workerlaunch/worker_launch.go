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
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"

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
	timing := NewLaunchPhaseTimer(sessionID, "prepare worker files")
	defer timing.End()
	fail := func(err error) (*core.WorkerLaunchConfig, core.LoginFingerprint, error) {
		return nil, nil, err
	}
	owner, err := store.Require(ctx, sessionID)
	if err != nil {
		return fail(err)
	}
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return fail(fmt.Errorf("unknown session %s", sessionID))
	}
	target := session.Target
	if target == nil {
		return fail(errors.New("worker preparation has no committed placement"))
	}
	if err := owner.VerifyTarget(target); err != nil {
		return fail(err)
	}
	if err := store.AdmitEnvironmentWorker(sessionID, target); err != nil {
		return fail(err)
	}
	// A busy worker can outlive a daemon upgrade. Reusing its history
	// does not imply that its executable can read this launch format.
	upgradeRetainedWorker := false
	if preparation == workerstaging.WorkerFilePreparationRefreshProfile {
		executable := core.Join(workerRoot, "hel")
		command, err := core.CommandOnLocator(
			core.SshTrafficClassControl,
			backend,
			sessionID,
			[]string{core.Text(executable), "worker", "build-id"},
			"read retained worker build",
		)
		if err != nil {
			return fail(err)
		}
		output, err := targets.ExecuteChecked(executor, command)
		if err != nil {
			return fail(err)
		}
		text, err := strFromUTF8(output.Stdout)
		if err != nil {
			return fail(fmt.Errorf("decode retained worker build: %w", err))
		}
		build := strings.TrimSpace(text)
		if !core.IsWorkerBuild(build) {
			return fail(errors.New("retained worker returned an invalid build identity"))
		}
		upgradeRetainedWorker = build != core.WorkerBuildID()
	}
	profile, ok := config.Profiles[session.LastProfile]
	if !ok {
		return fail(errors.New("session profile is missing"))
	}
	if err := profile.EnsureReady(session.LastProfile); err != nil {
		return fail(err)
	}
	launch, projectMemory, targetProfileHome, err := SessionLaunchConfig(config, state, sessionID, backend, history)
	if err != nil {
		return fail(err)
	}

	if session.NativeSessionID != nil && profile.Kind == core.HarnessKindCodex {
		request, err := core.NewSessionID()
		if err != nil {
			return fail(err)
		}
		launch.GoalResumeRequest = &request
	}
	staging, err := os.MkdirTemp("", ".tmp")
	if err != nil {
		return fail(fmt.Errorf("create worker staging directory: %w", err))
	}
	defer os.RemoveAll(staging)
	launchPath := filepath.Join(staging, "launch.json")
	if err := launch.Write(launchPath); err != nil {
		return fail(err)
	}
	ownershipPath := filepath.Join(staging, "ownership.json")
	instanceID := core.InstanceIdentity()
	ownership := core.WorkerOwnership{
		Version:             core.WorkerOwnershipVersion,
		NoGithubCredentials: session.NoGithubCredentials,
		WorkspaceID:         session.WorkspaceID,
		SessionID:           sessionID,
		ProfileID:           session.LastProfile,
		BundleID:            session.BundleID,
		TargetTemplateID:    session.TargetTemplateID,
		InstanceID:          &instanceID,
	}
	if err := ownership.Write(ownershipPath); err != nil {
		return fail(err)
	}
	// Every session runs from a staged copy of its profile, on every target,
	// so targetProfileHome is always a home the session owns and the stage
	// is always installed there.
	profileStage := filepath.Join(staging, "profile")
	stagingTiming := NewLaunchPhaseTimer(sessionID, "stage harness profile")
	err = workerstaging.StageProfileWithCatalog(
		session.LastProfile,
		&profile,
		profileStage,
		workerstaging.FetchCatalogOverHTTPS,
		workerstaging.SharedCatalogCache{},
	)
	stagingTiming.End()
	if err != nil {
		return fail(err)
	}
	notice, ok, err := AwsRoleNotice(config, state, sessionID)
	if err != nil {
		return fail(err)
	}
	if ok {
		executor.NotifyNotice(notice)
	}
	source, err := AwsCredentialSource(config, state, sessionID)
	if err != nil {
		return fail(err)
	}
	if source != nil {
		credentials, err := targets.Obtain(sessionID, source, executor)
		if err != nil {
			return fail(err)
		}
		if _, err := credentials.Install(profileStage, targetProfileHome); err != nil {
			return fail(err)
		}
	}
	if err := workerstaging.StageClaudeKeychainLogin(
		&profile,
		core.HarnessHostCurrent(),
		profileStage,
		launch.Environment,
		executor,
	); err != nil {
		return fail(err)
	}
	if err := workerstaging.StageManagedSkills(profile.Kind, profileStage, target.SkillsScope()); err != nil {
		return fail(err)
	}
	if err := workerstaging.AppendHelTargetEnvironment(profile.Kind, profileStage, backend); err != nil {
		return fail(err)
	}
	// Edits to attachment settings apply on the next container creation;
	// describe this container's creation snapshot while it is retained.
	attachments := core.AppliedContainerSettingsOf(session).AdditionalMounts
	storage, err := targets.InspectContainerStorage(backend, attachments, executor)
	if err != nil {
		return fail(err)
	}
	if err := workerstaging.AppendContainerStorageGuidance(
		profile.Kind,
		profileStage,
		backend,
		session.ContainerWorkspace,
		storage.ManagedTemporaryVolume,
		storage.Attachments,
	); err != nil {
		return fail(err)
	}
	if err := workerstaging.AppendSessionStateInstructions(profile.Kind, profileStage); err != nil {
		return fail(err)
	}
	if err := workerstaging.AppendSubagentPolicy(
		profile.Kind,
		profileStage,
		&launch.Subagents,
		config.Subagents.MaxConcurrent,
		launch.SubtleSubagents,
	); err != nil {
		return fail(err)
	}
	if err := workerstaging.ApplyStagedExecutionSetting(profile.Kind, launch.ExecutionPolicy, profileStage); err != nil {
		return fail(err)
	}
	if reviewer := launch.ReviewerLaunch; reviewer != nil &&
		core.ReviewMcpDeliveryForHarness(profile.Kind) == core.ReviewMcpDeliveryHarnessProfile {
		if err := ConfigureStagedReviewMCP(profile.Kind, profileStage, reviewer.MCPServers); err != nil {
			return fail(err)
		}
	}
	// The `mj-agents` registration is not staged here: the worker writes
	// it on every start, because an upgrade or recovery replaces the
	// launch configuration without restaging this profile.
	if profile.Kind == core.HarnessKindClaude {
		if err := workerstaging.ConfigureClaudeMailboxHook(profileStage, workerRoot); err != nil {
			return fail(err)
		}
	}
	sharedPaths := slices.Clone(workerstaging.SharedProfilePaths(profile.Kind))
	// A session without project memory gets no replica, no baseline and
	// no history server: nothing of the workspace's memory reaches it.
	if projectMemory != nil {
		replica, err := workerstaging.StageMemoryReplica(projectMemory, targetProfileHome, profileStage)
		if err != nil {
			return fail(err)
		}
		sharedPaths = append(sharedPaths, replica[:]...)
		if projectMemory.MCPDelivery == core.ProjectMemoryMcpDeliveryHarnessProfile {
			if err := workerstaging.ConfigureKimiHistoryMCP(profileStage, workerRoot, projectMemory.HistorySocket); err != nil {
				return fail(err)
			}
		}
	}
	if core.DeliveredByStaging(profile.Kind) {
		// Rust's `Result<String, String>`: the index text, or the reason it
		// is unavailable when indexFailed.
		index, indexFailed := "", false
		if projectMemory != nil {
			contents, _, err := workerstaging.StagedMemoryIndex(projectMemory, targetProfileHome, profileStage)
			if err != nil {
				index, indexFailed = err.Error(), true
				executor.NotifyNotice(fmt.Sprintf(
					"Warning: Project memory index %s unavailable: %s",
					rustPathJoinName(projectMemory.Root, core.MemoryIndex),
					index,
				))
			} else {
				index = contents
			}
		}
		if err := workerstaging.AppendComposedSessionInstructions(&launch, profileStage, index, indexFailed); err != nil {
			return fail(err)
		}
	}
	// The notes above can make instructions the profile alone could
	// deliver too long, so the staged file is checked before it travels.
	if err := profile.EnsureStagedInstructionsDeliverable(session.LastProfile, profileStage); err != nil {
		return fail(err)
	}
	sharedLogin := workerstaging.SharedProfileLogin(&profile, backend)
	if preparation != workerstaging.WorkerFilePreparationInstall {
		// A shared symlink stays live across boot. Only a copied login
		// identifies the credentials staged for this specific launch.
		var stagedLogin core.LoginFingerprint
		if preparation == workerstaging.WorkerFilePreparationRefreshProfileOnly &&
			profile.AuthScheme().UsesNativeLoginFile() &&
			sharedLogin == nil {
			stagedLogin, err = core.LoginFingerprintReadFile(
				profile.Kind,
				core.HarnessAuthenticationMarker(profile.Kind, profileStage),
			)
			if err != nil {
				return fail(err)
			}
		}
		if err := workerstaging.ClearProfileConfiguration(
			executor,
			backend,
			sessionID,
			targetProfileHome,
			profile.Kind,
		); err != nil {
			return fail(err)
		}
		if err := workerstaging.InstallWorkerProfileFiles(
			executor,
			backend,
			sessionID,
			workerRoot,
			targetProfileHome,
			workerstaging.StagedWorkerProfile{
				Launch:      launchPath,
				Ownership:   ownershipPath,
				Profile:     profileStage,
				SharedLogin: sharedLogin,
				SharedPaths: sharedPaths,
			},
		); err != nil {
			return fail(err)
		}
		if upgradeRetainedWorker {
			worker, err := workerstaging.WorkerInstallForTarget(backend, executor)
			if err != nil {
				return fail(err)
			}
			if err := workerstaging.InstallWorkerBinary(executor, backend, sessionID, worker, "hel"); err != nil {
				return fail(err)
			}
			return &launch, stagedLogin, nil
		}
		return nil, stagedLogin, nil
	}
	worker, err := workerstaging.WorkerInstallForTarget(backend, executor)
	if err != nil {
		return fail(err)
	}
	if err := workerstaging.InstallWorkerFiles(
		executor,
		backend,
		sessionID,
		workerRoot,
		targetProfileHome,
		worker,
		launchPath,
		ownershipPath,
		profileStage,
		sharedLogin,
		sharedPaths,
	); err != nil {
		return fail(err)
	}
	if session.BuildCache != nil {
		if err := InstallBuildCacheShim(config, state, session, backend, &launch, executor); err != nil {
			return fail(fmt.Errorf("install shared machine build cache configuration: %w", err))
		}
	}
	return &launch, nil, nil
}

// rustPathJoinName is Rust's `Path::join` of a single file name: a
// separator is added unless base is empty or already ends in one, and
// nothing is cleaned.
func rustPathJoinName(base, name string) string {
	if base != "" && !os.IsPathSeparator(base[len(base)-1]) {
		base += string(filepath.Separator)
	}
	return base + name
}

// strFromUTF8 is Rust's `std::str::from_utf8`, failing with the text of
// Rust's Utf8Error ("invalid utf-8 sequence of 1 bytes from index 3",
// "incomplete utf-8 byte sequence from index 3").
func strFromUTF8(text []byte) (string, error) {
	index := 0
	for index < len(text) {
		first := text[index]
		if first < 0x80 {
			index++
			continue
		}
		width := 0
		switch {
		case first >= 0xC2 && first <= 0xDF:
			width = 2
		case first >= 0xE0 && first <= 0xEF:
			width = 3
		case first >= 0xF0 && first <= 0xF4:
			width = 4
		}
		if width == 0 {
			return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		low, high := byte(0x80), byte(0xBF)
		switch first {
		case 0xE0:
			low = 0xA0
		case 0xED:
			high = 0x9F
		case 0xF0:
			low = 0x90
		case 0xF4:
			high = 0x8F
		}
		for offset := 1; offset < width; offset++ {
			if index+offset >= len(text) {
				return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
			}
			next := text[index+offset]
			if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
				return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", offset, index)
			}
		}
		index += width
	}
	return string(text), nil
}

// InstallBuildCacheShim stages and verifies the session's mbx, then installs
// marked launchers in the session's `bin` directory on the worker PATH.
func InstallBuildCacheShim(config *core.Config, state *core.StateState, session *core.SessionRecord, backend *core.TargetsTargetLocator, launch *core.WorkerLaunchConfig, executor core.CommandExecutor) error {
	cache := session.BuildCache
	if cache == nil {
		return nil
	}
	workerRoot, err := core.WorkerRoot(backend, session.ID)
	if err != nil {
		return err
	}
	if backend.Kind == core.TargetsTargetLocatorSshNitro {
		timing := NewLaunchPhaseTimer(session.ID, "stage Nitro mbx")
		defer timing.End()
		binary, err := func() (string, error) {
			platform, err := workerstaging.TargetPlatform(backend, executor)
			if err != nil {
				return "", err
			}
			if platform.Os != targets.TargetOsLinux {
				return "", errors.New("Nitro guests require Linux mbx")
			}
			artifact, err := buildcache.PinnedLinuxBinary(platform.Architecture, executor)
			if err != nil {
				return "", err
			}
			binary, err := workerstaging.InstallSessionExecutable(executor, backend, session.ID, artifact, "mbx")
			if err != nil {
				return "", err
			}
			if err := workerstaging.VerifyMbxBinary(executor, backend, binary, buildcache.MbxVersion); err != nil {
				return "", err
			}
			return binary, nil
		}()
		if err != nil {
			workerstaging.DisableMbxCacheShims(session.ID, backend, workerRoot, executor, err.Error())
			return nil
		}
		return workerstaging.InstallMbxShims(executor, backend, workerRoot, binary, nil, nil)
	}
	configRoots := workerstaging.BuildCacheConfigRoots(launch)
	configuration := core.BuildCacheConfigurationDirectory(cache.Directory)
	linked, err := workerstaging.LinkLegacyMbxConfiguration(executor, backend, workerRoot, configuration, configRoots)
	if err != nil {
		return err
	}
	if linked {
		return nil
	}
	var binary buildcache.CachedMbxBinary
	synced, err := buildcache.SyncMbxBinaryForContainer(backend, cache.Directory, executor)
	if err != nil {
		workerstaging.DisableMbxCacheShims(session.ID, backend, workerRoot, executor, err.Error())
		return nil
	}
	switch synced := synced.(type) {
	case buildcache.CachedMbxSyncReady:
		binary = synced.Value
	case buildcache.CachedMbxSyncUnavailable:
		workerstaging.DisableMbxCacheShims(session.ID, backend, workerRoot, executor, synced.Value)
		return nil
	default:
		panic(fmt.Sprintf("unknown mbx sync outcome %T", synced))
	}
	if err := workerstaging.VerifyMbxBinary(executor, backend, binary.Path, binary.Version); err != nil {
		workerstaging.DisableMbxCacheShims(session.ID, backend, workerRoot, executor, err.Error())
		return nil
	}
	configuration, configRoots, err = BuildCacheConfiguration(config, state, session, backend, launch, executor)
	if err != nil {
		return err
	}
	return workerstaging.InstallMbxShims(executor, backend, workerRoot, binary.Path, &configuration, configRoots)
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
	session := state.Sessions.GetShared(sessionID)
	if session == nil || session.Target == nil {
		return "", false
	}
	backend, err := targets.BackendLocator(session.Target, session, config)
	if err != nil {
		slog.Debug("could not construct a worker diagnostic probe", "session_id", sessionID, "error", err.Error())
		return "", false
	}
	workerRoot, err := core.WorkerRoot(&backend, sessionID)
	if err != nil {
		slog.Debug("could not derive the worker diagnostic root", "session_id", sessionID, "error", err.Error())
		return "", false
	}
	binaryFailure, binaryFailed := workerstaging.WorkerBinaryProbeFailure(executor, &backend, workerRoot)
	var probe string
	if result, err := workerstaging.DiagnoseWorkerReadOnly(executor, &backend, workerRoot); err != nil {
		probe = "the worker could not be probed: " + err.Error()
	} else {
		probe = result.String()
	}
	if binaryFailed {
		return binaryFailure + "; " + probe, true
	}
	return probe, true
}

// WorkerRecoveryPlan is a non-destructive liveness probe plus commands that
// replace a confirmed dead session worker without touching its durable relay
// files. The session manager runs both off its async actor. ctx carries the
// session's worker permit, when there is one (see WorkerPlacement).
func WorkerRecoveryPlan(ctx context.Context, config *core.Config, state *core.StateState, sessionID string, operation *core.MoveOperation) (sessiontypes.WorkerRecoveryPlan, error) {
	fail := func(err error) (sessiontypes.WorkerRecoveryPlan, error) {
		return sessiontypes.WorkerRecoveryPlan{}, err
	}
	backend, workerRoot, err := WorkerPlacement(ctx, config, state, sessionID)
	if err != nil {
		return fail(err)
	}
	launch, err := WorkerLaunchConfigForMove(config, state, sessionID, &backend, operation)
	if err != nil {
		return fail(err)
	}
	workspace := workerstaging.WorkerWorkspaceForRecovery(&backend, launch.Cwd)
	sourceTarget := state.Sessions.Index(sessionID).Target
	if sourceTarget == nil {
		return fail(errors.New("session target is missing"))
	}
	target, err := targets.TargetRecoveryPlan(&backend, sessionID)
	if err != nil {
		return fail(err)
	}
	livenessProbe, err := targets.NewWorkerControlPlan(
		fmt.Sprintf("probe the worker of session %s", sessionID),
		[]targets.ControlCommand{workerstaging.WorkerLivenessCommand(&backend, workerRoot)},
		&backend,
		sessionID,
	)
	if err != nil {
		return fail(err)
	}
	exitRecord := workerstaging.WorkerExitRecordCommand(&backend, workerRoot)
	binaryRefresh, err := workerstaging.WorkerBinaryRefreshPlan(&backend, sessionID)
	if err != nil {
		return fail(err)
	}
	launchRefresh, err := workerstaging.WorkerLaunchRefreshPlan(&backend, sessionID, &launch)
	if err != nil {
		return fail(err)
	}
	restart, err := targets.NewWorkerControlPlan(
		fmt.Sprintf("restart Mjolnir worker for session %s", sessionID),
		[]targets.ControlCommand{
			workerstaging.StopWorkerCommand(&backend, workerRoot),
			targets.ControlCommandFromCommandSpec(workerstaging.StartWorkerCommand(&backend, workerRoot)),
		},
		&backend,
		sessionID,
	)
	if err != nil {
		return fail(err)
	}
	return sessiontypes.WorkerRecoveryPlan{
		SourceTarget:  sourceTarget,
		Target:        target,
		Workspace:     workspace,
		LivenessProbe: livenessProbe,
		ExitRecord:    &exitRecord,
		BinaryRefresh: binaryRefresh,
		LaunchRefresh: &launchRefresh,
		Restart:       restart,
	}, nil
}

// SessionLaunchConfig is the launch config for a session, including what
// depends on its sub-agent role, its project memory configuration (nil when
// it has none), and its target profile home. The first launch and every
// relaunch use this, so a relaunched worker keeps its delegation tools and a
// relaunched child keeps its parent's workspace.
func SessionLaunchConfig(config *core.Config, state *core.StateState, sessionID string, backend *core.TargetsTargetLocator, history workerstaging.LaunchHistory) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig, string, error) {
	fail := func(err error) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig, string, error) {
		return core.WorkerLaunchConfig{}, nil, "", err
	}
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return fail(fmt.Errorf("unknown session %s", sessionID))
	}
	if err := session.ValidateConfiguration(config); err != nil {
		return fail(err)
	}
	profile, ok := config.Profiles[session.LastProfile]
	if !ok {
		return fail(errors.New("session profile is missing"))
	}
	owner, err := state.WorkspaceOwner(sessionID)
	if err != nil {
		return fail(err)
	}
	checkout := owner.RecordCheckout()
	var bundle *core.ProjectBundle
	if _, ok := checkout.ProjectDirectory(); !ok {
		bundle = owner.ProjectBundle(config)
	}
	target, err := session.TargetRuntimeSettings(config)
	if err != nil {
		return fail(err)
	}
	subagent := state.Subagents.GetShared(sessionID)
	var workingDirectories []string
	cursor := sessionID
	visited := map[string]struct{}{}
	for cursor != owner.ID {
		if _, seen := visited[cursor]; seen {
			return fail(errors.New("cycle in borrowed workspace ancestry"))
		}
		visited[cursor] = struct{}{}
		child := state.Subagents.GetShared(cursor)
		if child == nil {
			return fail(errors.New("borrowed workspace is missing its parent relation"))
		}
		if child.WorkingDirectory != "" {
			workingDirectories = append(workingDirectories, child.WorkingDirectory)
		}
		cursor = child.ParentSessionID
		if ownerID, ok := state.EnvironmentOwnerID(cursor); !ok || ownerID != owner.ID {
			return fail(errors.New("borrowed workspace parent is outside its environment"))
		}
	}
	slices.Reverse(workingDirectories)
	launch, projectMemory, targetProfileHome, err := workerstaging.WorkerLaunchConfigWithCheckout(
		session,
		checkout,
		&profile,
		bundle,
		backend,
		workerstaging.LaunchWorkspace{
			SessionID:          owner.ID,
			Container:          owner.ContainerWorkspace,
			ParentWorktree:     owner.ManagedWorktree,
			Owner:              owner,
			WorkingDirectories: workingDirectories,
		},
		target,
		config,
	)
	if err != nil {
		return fail(err)
	}
	launch.ReviewerLaunch = nil
	if subagent != nil {
		record, err := store.LoadSubagent(sessionID)
		if err != nil {
			return fail(err)
		}
		if record == nil {
			return fail(errors.New("sub-agent launch record is missing"))
		}
		launch.ReviewerLaunch = record.ReviewerLaunch
	}
	if reviewer := launch.ReviewerLaunch; reviewer != nil {
		launch.BridgeCommand = reviewer.BridgeCommand
		launch.BridgeArgs = slices.Clone(reviewer.BridgeArgs)
		launch.ExecutionPolicy = reviewer.ExecutionPolicy
		if launch.Environment == nil {
			launch.Environment = map[string]string{}
		}
		maps.Copy(launch.Environment, reviewer.Environment)
		launch.ExcludedEnvironment = slices.Clone(reviewer.ExcludedEnvironment)
	}
	if launch.Environment == nil {
		launch.Environment = map[string]string{}
	}
	// Read by the target-side MCP server from launch.json beside its socket,
	// independently of how each harness starts or registers that server.
	for _, name := range []string{core.ChildReportDirEnv, core.ChildReportLimitEnv} {
		delete(launch.Environment, name)
	}
	if subagent != nil {
		report, err := store.LoadSubagentReport(sessionID)
		if err != nil {
			return fail(err)
		}
		if report.ReportDir != nil {
			launch.Environment[core.ChildReportDirEnv] = *report.ReportDir
			launch.Environment[core.ChildReportLimitEnv] = strconv.FormatUint(
				core.MaxChildReportBytes(config.Subagents.MaxConcurrent), 10)
		}
	}
	if subagent != nil {
		launch.Environment[core.SessionManagedSubagentEnv] = "1"
		delete(launch.Environment, core.SessionMessageMCPEnv)
	} else {
		delete(launch.Environment, core.SessionManagedSubagentEnv)
		// This controller grants the message-only server to every
		// top-level session; the worker trusts only this marker.
		launch.Environment[core.SessionMessageMCPEnv] = "1"
	}
	_, attached := checkout.(core.CheckoutAttached)
	_, storedHistory := history.(workerstaging.LaunchHistoryStored)
	if subagent == nil &&
		attached &&
		!(session.Project != nil && session.Project.PrimaryIsPlainDirectory()) &&
		storedHistory &&
		session.Checkpoint == nil &&
		session.NativeSessionID == nil {
		launch.Environment[core.SessionRawLaunchRecordEnv] = "1"
	} else {
		delete(launch.Environment, core.SessionRawLaunchRecordEnv)
	}
	workerstaging.ApplyJevSettings(&launch, &config.Jev)
	launch.AgentMailboxesEnabled = true
	var subagentView *core.SubagentRecord
	if subagent != nil {
		view := subagent.ToRecordView()
		subagentView = &view
	}
	policy := core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	if session.Subagents != nil {
		policy = *session.Subagents
	}
	launch.Subagents = policy.ForLaunch(profile.Kind, subagentView)
	// Only a delegating parent's spawn tool reads this. Other sessions keep
	// the default, so their launch files do not change.
	launch.SubtleSubagents = !launch.Subagents.UsesMjolnir() || config.Subagents.OffersSubtle(launch.Subagents)
	owns, known := state.OwnsEnvironment(sessionID)
	ownsEnvironment := known && owns
	launch.Oracle = workerstaging.OracleLaunchEnabled(session, ownsEnvironment, &config.Subagents)
	// Registration decided whether this child can be given the tool.
	launch.HandbackTool = subagent != nil && subagent.HandbackTool
	launch.SubagentKind = nil
	if subagent != nil {
		kind := subagent.Kind
		launch.SubagentKind = &kind
	}
	launch.Orchestrator = subagent != nil && subagent.Orchestrator
	launch.OrchestratorsEnabled = subagent == nil && launch.Subagents.OrchestratorsEnabled()

	// The child's harness opens on its spawn model instead of switching
	// to it from the profile default before the first turn (issue 1217).
	launch.InitialModel = nil
	if subagent != nil && subagent.Model != nil {
		model := *subagent.Model
		launch.InitialModel = &model
	}
	if subagent != nil && subagent.Kind != core.SubagentKindReviewer {
		group := core.GeneratedPromptSubagentSpawnID(subagent.RequestKey)
		steps, err := store.LoadStartupGroup(sessionID, group)
		if err != nil {
			return fail(err)
		}
		// The assignment is delivered at most once per session history.
		// A restore deletes the worker's command receipts, so the history
		// decides: once the assignment has started, the conversation
		// already holds it and the settings it was sent with.
		promptID := store.StartupStepID(group, "prompt")
		assignmentStarted, err := workerstaging.PromptStarted(sessionID, history, promptID)
		if err != nil {
			return fail(err)
		}
		if !assignmentStarted {
			commands := make([]core.InitialWorkerCommand, 0, len(steps))
			for _, step := range steps {
				decoded, err := store.UnmarshalStartupStep([]byte(step.StepJSON))
				if err != nil {
					return fail(err)
				}
				var command core.RelayCommand
				optional := false
				switch decoded := decoded.(type) {
				case store.StartupStepConfigure:
					command = core.RelayCommandSetConfig{Key: decoded.Key, Value: decoded.Value}
					optional = decoded.Optional
				case store.StartupStepApiPrompt:
					command = core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{{
						Kind: acpschema.ContentBlockKindText,
						Text: &acpschema.TextContent{Text: decoded.Text},
					}}}
				default:
					return fail(errors.New("sub-agent startup contains a non-input step"))
				}
				commands = append(commands, core.InitialWorkerCommand{
					CommandID: step.CommandID,
					Command:   command,
					Optional:  optional,
				})
			}
			launch.InitialCommands = commands
		}
	}
	// The workspace owner requests review before pushing. The worker derives
	// the review tools and the push gate from this one value.
	if ownsEnvironment {
		if _, missing, ok := state.EffectiveReview(sessionID); !ok {
			slog.Warn("review choice cannot be resolved; launching without review tools",
				"session_id", sessionID, "missing", missing)
		}
		launch.ReviewTrigger = state.EffectiveReviewConfig(&config.Review, sessionID).Trigger()
	} else {
		launch.ReviewTrigger = core.ReviewTriggerOff
	}
	return launch, projectMemory, targetProfileHome, nil
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
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return core.WorkerLaunchConfig{}, fmt.Errorf("unknown session %s", sessionID)
	}
	launch, _, _, err := SessionLaunchConfig(config, state, sessionID, backend, workerstaging.LaunchHistoryStored{})
	if err != nil {
		return core.WorkerLaunchConfig{}, err
	}
	if operation != nil &&
		operation.SourceCheckpointOnly &&
		operation.DestinationTarget == nil &&
		slices.Contains([]core.MovePhase{
			core.MovePhaseCapturing,
			core.MovePhaseTransferring,
			core.MovePhaseRestoring,
			core.MovePhaseDestinationStarting,
			core.MovePhaseFailed,
			core.MovePhaseCancelled,
		}, operation.Phase) &&
		session.LastProfile == operation.SourceProfileID &&
		targetLocatorsEqual(session.Target, operation.SourceTarget) &&
		slices.Contains([]core.SessionState{
			core.SessionStateRunning,
			core.SessionStateDisconnected,
			core.SessionStateClosing,
		}, session.State) {
		launch.RunMode = core.WorkerRunModeCheckpointOnly
	}
	return launch, nil
}

// targetLocatorsEqual is Rust's `Option<&TargetLocator>` equality: both
// absent, or the same variant with equal fields.
func targetLocatorsEqual(a, b core.TargetLocator) bool {
	return reflect.DeepEqual(a, b)
}

// ProjectMemorySyncTarget is where the session's project memory is
// synchronized, or nil for a session that has none.
func ProjectMemorySyncTarget(config *core.Config, state *core.StateState, sessionID string) (*sessiontypes.ProjectMemorySyncTarget, error) {
	session := state.Sessions.GetShared(sessionID)
	if session == nil {
		return nil, fmt.Errorf("unknown session %s", sessionID)
	}
	if session.NoProjectMemory {
		return nil, nil
	}
	if err := session.ValidateConfiguration(config); err != nil {
		return nil, err
	}
	if session.Target == nil {
		return nil, errors.New("session target is missing")
	}
	backend, err := targets.BackendLocator(session.Target, session, config)
	if err != nil {
		return nil, err
	}
	profile, ok := config.Profiles[session.LastProfile]
	if !ok {
		return nil, errors.New("session profile is missing")
	}
	owner, err := state.WorkspaceOwner(sessionID)
	if err != nil {
		return nil, err
	}
	checkout := owner.RecordCheckout()
	var bundle *core.ProjectBundle
	if _, ok := checkout.ProjectDirectory(); !ok {
		bundle = owner.ProjectBundle(config)
	}
	primary, additional, err := workerstaging.LaunchWorkspacePaths(checkout, bundle, &backend, &workerstaging.LaunchWorkspace{
		SessionID:          owner.ID,
		Container:          owner.ContainerWorkspace,
		ParentWorktree:     owner.ManagedWorktree,
		Owner:              owner,
		WorkingDirectories: []string{},
	})
	if err != nil {
		return nil, err
	}
	targetHome := targets.TargetProfileHome(&backend, sessionID, &profile)
	launch, err := workerstaging.ProjectMemoryLaunch(session, bundle, primary, additional, targetHome, owner.ManagedWorktree)
	if err != nil {
		return nil, err
	}
	return &sessiontypes.ProjectMemorySyncTarget{
		CanonicalRoot: workerstaging.CanonicalMemoryRoot(launch.ProjectKey),
	}, nil
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
	file, ok := harness.MCPConfigFile()
	if !ok {
		return fmt.Errorf("%s does not read MCP servers from its profile", harnessKindDebug(harness))
	}
	kimi := harness == core.HarnessKindKimi
	path := filepath.Join(profileStage, file)
	// Rust reads, edits and rewrites the file exactly as the staging helper
	// does, with this label in each message.
	return workerstaging.EditStagedJSONObject(path, "staged reviewer configuration", func(root map[string]any) error {
		entry, ok := root["mcpServers"]
		if !ok {
			entry = map[string]any{}
			root["mcpServers"] = entry
		}
		configured, ok := entry.(map[string]any)
		if !ok {
			return fmt.Errorf("mcpServers in staged reviewer configuration %s must be a JSON object", path)
		}
		for _, server := range servers {
			args := make([]any, len(server.Args))
			for index, argument := range server.Args {
				args[index] = argument
			}
			entry := map[string]any{
				"command": server.Command,
				"args":    args,
			}
			if kimi {
				entry["transport"] = "stdio"
				entry["runtime_id"] = "local"
			} else {
				entry["type"] = "stdio"
			}
			configured[server.Name] = entry
		}
		return nil
	})
}

// harnessKindDebug is HarnessKind's Rust Debug name, the variant name.
func harnessKindDebug(kind core.HarnessKind) string {
	switch kind {
	case core.HarnessKindCodex:
		return "Codex"
	case core.HarnessKindClaude:
		return "Claude"
	case core.HarnessKindKimi:
		return "Kimi"
	case core.HarnessKindGrok:
		return "Grok"
	case core.HarnessKindMuse:
		return "Muse"
	case core.HarnessKindOpenCode:
		return "OpenCode"
	case core.HarnessKindDsh:
		return "Dsh"
	case core.HarnessKindAntigravity:
		return "Antigravity"
	}
	return string(kind)
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
