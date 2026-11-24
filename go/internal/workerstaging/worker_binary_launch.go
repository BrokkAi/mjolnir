// Port of mj-worker-staging/src/worker_binary/launch.rs.

package workerstaging

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessiontypes"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	cleanup := RemoveGeneratedMbxShims(executor, backend, workerRoot)
	cleanupNote := ""
	if cleanup != nil {
		slog.Warn("could not remove marked shared-mbx launchers: "+cleanup.Error(), "session_id", sessionID)
		cleanupNote = fmt.Sprintf(" Mjolnir could not remove its marked launchers: %s.", cleanup)
	}
	slog.Warn("shared mbx is unavailable in this container: "+reason, "session_id", sessionID)
	executor.NotifyNotice(fmt.Sprintf(
		"The Rust build cache is unavailable in this container: %s.%s The session will start without it.",
		reason, cleanupNote))
}

// BuildCacheConfigRoots is the distinct `XDG_CONFIG_HOME` values of the
// launch's target and harness environments, in Rust path order.
func BuildCacheConfigRoots(launch *core.WorkerLaunchConfig) []string {
	var roots []string
	for _, environment := range []map[string]string{launch.TargetEnvironment, launch.Environment} {
		root, ok := environment["XDG_CONFIG_HOME"]
		if !ok || slices.ContainsFunc(roots, func(existing string) bool { return comparePathOrder(existing, root) == 0 }) {
			continue
		}
		roots = append(roots, root)
	}
	slices.SortFunc(roots, comparePathOrder)
	return roots
}

// comparePathOrder is Rust's `Path` ordering, which compares by component.
func comparePathOrder(a, b string) int {
	left, right := stagingComponents(a), stagingComponents(b)
	for index := 0; index < len(left) && index < len(right); index++ {
		if left[index].kind != right[index].kind {
			return int(left[index].kind) - int(right[index].kind)
		}
		if order := strings.Compare(left[index].text, right[index].text); order != 0 {
			return order
		}
	}
	return len(left) - len(right)
}

// ApplyJevSettings carries the Jev settings to the worker's launch
// environment. The harness supervisor excludes the worker's Fastino key.
func ApplyJevSettings(launch *core.WorkerLaunchConfig, config *core.JevConfig) {
	if launch.TargetEnvironment == nil {
		launch.TargetEnvironment = map[string]string{}
	}
	if launch.Environment == nil {
		launch.Environment = map[string]string{}
	}
	for _, environment := range []map[string]string{launch.TargetEnvironment, launch.Environment} {
		delete(environment, core.FastinoKeyEnvironment)
		if !config.Enabled {
			environment[core.DisabledEnvironment] = "1"
		}
	}
	key, err := config.ResolveFastinoKey()
	switch {
	case err != nil:
		slog.Warn("Jev will use the shared proxy key", "error", err.Error())
	case key != nil:
		// The worker takes the key from its target launch environment.
		launch.TargetEnvironment[core.FastinoKeyEnvironment] = *key
	}
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
	fail := func(err error) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig, string, error) {
		return core.WorkerLaunchConfig{}, nil, "", err
	}
	sessionID := session.ID
	executionPolicy := target.ExecutionPolicy
	targetProfileHome := targets.TargetProfileHome(backend, sessionID, profile)
	primary, additional, err := LaunchWorkspacePaths(checkout, bundle, backend, &workerWorkspace)
	if err != nil {
		return fail(err)
	}
	cwd := primary
	for _, directory := range workerWorkspace.WorkingDirectories {
		cwd = core.Join(cwd, directory)
	}
	additionalDirectories := slices.Clone(additional)
	if additionalDirectories == nil {
		additionalDirectories = []string{}
	}
	owner := session
	if workerWorkspace.Owner != nil {
		owner = workerWorkspace.Owner
	}
	for _, resource := range core.AppliedContainerSettingsOf(owner).AdditionalMounts {
		additionalDirectories = append(additionalDirectories, resource.Destination)
	}
	if profile.Kind == core.HarnessKindMuse && len(additionalDirectories) != 0 {
		return fail(fmt.Errorf("%s ACP does not support multiple workspace roots; use a single-repository bundle",
			profile.Kind.DisplayName()))
	}
	bridgeCommand, bridgeArgs := BridgeLaunch(profile.Kind, executionPolicy)
	targetEnvironment := maps.Clone(target.Environment)
	if targetEnvironment == nil {
		targetEnvironment = map[string]string{}
	}
	engine, isContainer := backend.ContainerEngine()
	podmanContainer := isContainer && engine == "podman"
	nitroBuildCache := session.BuildCache != nil && backend.Kind == core.TargetsTargetLocatorSshNitro
	if podmanContainer {
		// Provisioning writes inherited settings to this file under the
		// container's configured home. A root Podman worker can have
		// HOME=/root, so the session Git config includes the file by path.
		targetEnvironment[core.SessionGitConfigIncludePath] = "/home/hel/.gitconfig"
	}
	// The turn bounds are read by the worker process, which re-execs with a
	// cleared environment, so a value set for the daemon cannot reach it by
	// inheritance. Carry the two knobs explicitly when the daemon was started
	// with them, so shortening a timeout for a test works on every target
	// and not only on the container targets that can set it in
	// configuration. `RUST_LOG` travels the same way and for the same
	// reason: a worker that has gone quiet is diagnosed from its own log,
	// and the log level cannot be raised after the fact on a worker that
	// re-execs with a cleared environment.
	for _, name := range []string{"MJ_TURN_STALL_TIMEOUT_MS", "MJ_TURN_TOOL_STALL_TIMEOUT_MS", "RUST_LOG"} {
		if value, ok := os.LookupEnv(name); ok && utf8.ValidString(value) {
			targetEnvironment[name] = value
		}
	}
	// The build cache reaches the harness, its terminals, and the reviewer
	// sidecar, all of which run Cargo through the mbx shim.
	if buildCache := session.BuildCache; buildCache != nil {
		targetEnvironment["MBX_CACHE_DIR"] = buildCache.Directory
		if nitroBuildCache {
			delete(targetEnvironment, "MJ_MBX_CONFIG_DIR")
		} else {
			targetEnvironment["MJ_MBX_CONFIG_DIR"] = core.BuildCacheConfigurationDirectory(buildCache.Directory)
		}
		// Compiler symlinks name this worker's private executable. Sharing
		// them lets another container replace them with an unreachable path.
		workerRoot, err := core.WorkerRoot(backend, sessionID)
		if err != nil {
			return fail(err)
		}
		targetEnvironment["MBX_SHIMS_DIR"] = core.Text(core.Join(workerRoot, "mbx-shims"))
		// The per-build summary and savings lines are for a human at a
		// terminal; in a harness session they only add noise to Cargo
		// output.
		targetEnvironment["MBX_SUMMARY"] = "off"
		targetEnvironment["MBX_SAVINGS"] = "off"
	}
	environment := maps.Clone(targetEnvironment)
	maps.Copy(environment, profile.Environment.Resolved())
	// Only the placement may set the Nitro mbx launch directive.
	nitroMbxDirective := core.SessionNitroMbxEnv
	delete(targetEnvironment, nitroMbxDirective)
	delete(environment, nitroMbxDirective)
	if nitroBuildCache {
		targetEnvironment[nitroMbxDirective] = "1"
		// The engine owns the remote settings in the guest process
		// environment.
		remotePrefix := core.MbxRemoteEnvPrefix
		maps.DeleteFunc(targetEnvironment, func(name, _ string) bool { return strings.HasPrefix(name, remotePrefix) })
		maps.DeleteFunc(environment, func(name, _ string) bool { return strings.HasPrefix(name, remotePrefix) })
		for _, key := range []string{"MBX_CACHE_DIR", "MBX_SHIMS_DIR"} {
			environment[key] = targetEnvironment[key]
		}
		delete(environment, "MJ_MBX_CONFIG_DIR")
	}
	source, err := targets.Source(profile, backend, target.Environment)
	if err != nil {
		return fail(err)
	}
	if source != nil {
		delete(environment, "AWS_DEFAULT_PROFILE")
		environment["AWS_PROFILE"] = source.Environment["AWS_PROFILE"]
		for _, file := range [][2]string{
			{"AWS_SHARED_CREDENTIALS_FILE", core.AwsCredentialsEmptyCredentialsFile},
			{"AWS_CONFIG_FILE", core.AwsCredentialsConfigFile},
		} {
			environment[file[0]] = core.Text(core.Join(targetProfileHome, file[1]))
		}
	}
	// This ownership fact is generated by the controller, so a profile
	// cannot impersonate another session or turn a child into a top-level
	// sender.
	environment["MJ_SESSION_ID"] = sessionID
	if session.Target != nil && session.Target.SkillsScope() == core.SkillsScopeLocalhost {
		// Harnesses clear their environment. Host CLI commands must still
		// address the daemon that created this session, including named
		// instances.
		configDir, err := filepath.Abs(core.ConfigDir())
		if err != nil {
			return fail(fmt.Errorf("resolve host Mjolnir configuration directory: %w", err))
		}
		environment["MJ_CONFIG_DIR"] = configDir
		dataDir, err := filepath.Abs(core.DataDir())
		if err != nil {
			return fail(fmt.Errorf("resolve host Mjolnir data directory: %w", err))
		}
		environment["MJ_DATA_DIR"] = dataDir
		if instance, ok := core.InstanceName(); ok {
			environment["MJ_INSTANCE"] = instance
		} else {
			delete(environment, "MJ_INSTANCE")
		}
	}
	profile.Kind.ConfigureStagedHomeEnvironment(targetProfileHome, environment)
	if err := profile.ConfigureExecutionEnvironment(executionPolicy, environment); err != nil {
		return fail(err)
	}
	// The project's identity also keys the shared tool cache and names the
	// Claude project directory, so it is worked out for every session. Only
	// the memory launch config below depends on the session's choice.
	projectMemory, err := ProjectMemoryLaunch(session, bundle, primary, additional, targetProfileHome, workerWorkspace.ParentWorktree)
	if err != nil {
		return fail(err)
	}
	projectMemory.MCPDelivery = ProjectMemoryMCPDelivery(profile.Kind, backend)
	sessionWorkerRoot, err := core.WorkerRoot(backend, session.ID)
	if err != nil {
		return fail(err)
	}
	historySocket := core.Join(sessionWorkerRoot, "control.sock")
	projectMemory.HistorySocket = &historySocket
	if cache := target.ToolCache; cache != nil {
		workerRoot, err := core.WorkerRoot(backend, sessionID)
		if err != nil {
			return fail(err)
		}
		private := core.Join(workerRoot, "tool-cache")
		for _, setting := range [][2]string{
			{"MJ_TOOL_CACHE_DIR", cache.Directory},
			{"MJ_TOOL_CACHE_PROJECT", projectMemory.ProjectKey},
			{"MJ_TOOL_CACHE_HOME", core.Text(private)},
			{"MJ_NX_HOME_DIR", cache.NxHome},
		} {
			targetEnvironment[setting[0]] = setting[1]
			environment[setting[0]] = setting[1]
		}
	}
	if profile.Kind == core.HarnessKindClaude {
		environment["CLAUDE_CODE_PROJECT_DIR_NAME"] = ProjectMemoryReplicaSlug(projectMemory.ProjectKey, sessionID)
		if session.NoProjectMemory {
			// Claude Code reads and offers its auto-memory directory unless
			// told not to. Set after the profile's own environment so the
			// session's choice wins.
			environment[ClaudeDisableAutoMemoryEnv] = "1"
		}
	}
	var memory *core.ProjectMemoryLaunchConfig
	if !session.NoProjectMemory {
		memory = &projectMemory
	}
	ApplyClaudeSetupToken(environment, profile.Kind, core.ClaudeOauthTokenPath(session.LastProfile))
	excludedEnvironment := ExcludeHarnessEnvironment(session.LastProfile, profile, environment)
	if session.NoGithubCredentials {
		core.RemoveGithubAuthenticationEnvironment(environment)
		core.RemoveGithubAuthenticationEnvironment(targetEnvironment)
	}
	if isContainer && profile.Kind == core.HarnessKindClaude {
		// Claude Code rejects bypassPermissions under uid 0 unless it is
		// told the process is already running in an isolated sandbox. Every
		// container engine can run the session as root. Podman and Nitro
		// always do. Docker and Apple container do when the image user is
		// root, as on the default buildpack-deps image.
		environment["IS_SANDBOX"] = "1"
	}
	// Placement owns this directive; profile/target environment cannot
	// enable system-package installation on a bare target.
	delete(environment, core.SessionProjectToolsRoots)
	delete(targetEnvironment, core.SessionProjectToolsRoots)
	if isContainer {
		roots, err := launchJSONString(append([]string{primary}, additional...))
		if err != nil {
			return fail(err)
		}
		environment[core.SessionProjectToolsRoots] = roots
	}
	approvalChoices := config.ProfileApprovalChoices(*profile)
	var authenticationMarker *string
	if name, ok := pathStripPrefix(profile.AuthenticationMarker(), profile.Home); ok {
		authenticationMarker = &name
	}
	var launchMemory *core.ProjectMemoryLaunchConfig
	if memory != nil {
		copied := *memory
		copied.RepositoryRoots = maps.Clone(memory.RepositoryRoots)
		launchMemory = &copied
	}
	launch := core.WorkerLaunchConfig{
		CPUAccounting:         backend.CPUAccounting(),
		NoGithubCredentials:   session.NoGithubCredentials,
		Oracle:                false,
		InitialCommands:       []core.InitialWorkerCommand{},
		TargetEnvironment:     targetEnvironment,
		SeedImageEnvironment:  WorkerRunsInImage(backend),
		RunMode:               core.WorkerRunModeHarness,
		SessionID:             sessionID,
		Subagents:             core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		AgentMailboxesEnabled: true,
		SubtleSubagents:       true,
		ReviewTrigger:         core.ReviewTriggerOff,
		Harness:               profile.Kind,
		// The staged home mirrors the profile home, so the marker's path
		// within the profile home is the one the worker must check and the
		// credential sync must write. Kimi's is nested
		// (`credentials/kimi-code.json`); its file name alone named a file
		// at the top of the staged home that Kimi never reads.
		HarnessHome:           targetProfileHome,
		AuthenticationMarker:  authenticationMarker,
		BridgeCommand:         bridgeCommand,
		BridgeArgs:            bridgeArgs,
		HarnessRuntime:        HarnessRuntimePolicy(backend),
		Environment:           environment,
		ExcludedEnvironment:   excludedEnvironment,
		Cwd:                   cwd,
		AdditionalDirectories: additionalDirectories,
		NativeSessionID:       session.NativeSessionID,
		ProjectMemory:         launchMemory,
		ExecutionPolicy:       executionPolicy,
		ClaudePermissionMode:  approvalChoices.ClaudePermissionMode,
		MjolnirGuardian:       approvalChoices.MjolnirGuardian,
	}
	return launch, memory, targetProfileHome, nil
}

// launchJSONString is serde_json::to_string of a string list: compact, no
// HTML escapes.
func launchJSONString(values []string) (string, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(values); err != nil {
		return "", err
	}
	return strings.TrimSuffix(buffer.String(), "\n"), nil
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
	before := maps.Clone(environment)
	excluded := profile.ExcludeHarnessEnvironment(environment)
	var removed []string
	for _, name := range excluded {
		if _, ok := before[name]; ok {
			removed = append(removed, name)
		}
	}
	if len(removed) == 0 {
		return excluded
	}
	reportedExclusions.mu.Lock()
	_, reported := reportedExclusions.profiles[profileID]
	if !reported {
		if reportedExclusions.profiles == nil {
			reportedExclusions.profiles = map[string]struct{}{}
		}
		reportedExclusions.profiles[profileID] = struct{}{}
	}
	reportedExclusions.mu.Unlock()
	if !reported {
		slog.Info("left API key settings out of the harness environment: this Codex profile must not use an OpenAI API key",
			"profile_id", profileID, "removed", strings.Join(removed, ", "))
	}
	return excluded
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
