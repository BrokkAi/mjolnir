// Port of mj-worker-staging/src/worker_binary/tests.rs, part 5: Muse and
// Kimi staging, container guidance, upgrades and recovery, the staged home's
// contents, and staged instructions.

package workerstaging

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"sort"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessiontypes"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func readJSONFile(t *testing.T, path string) map[string]any {
	t.Helper()
	var document map[string]any
	if err := json.Unmarshal([]byte(readFixture(t, path)), &document); err != nil {
		t.Fatal(err)
	}
	return document
}

func jsonAt(value any, keys ...string) any {
	for _, key := range keys {
		object, ok := value.(map[string]any)
		if !ok {
			return nil
		}
		value = object[key]
	}
	return value
}

// Hard-won: a24070f: Muse launches failed when staged settings retained the shipped :auto-review profile.
// Both policies stage `:ask-me`. Unconstrained Muse additionally runs
// `allowAll` approvals with auto-review off (plus `--disable-sandbox`), so
// no permission requests are emitted and the staged profile never matters.
func TestMuseStagedSettingsReplaceTheAutoReviewProfileUnderEveryPolicy(t *testing.T) {
	for _, entry := range []struct {
		policy  core.ExecutionPolicy
		profile string
	}{
		{core.ExecutionPolicyUnconstrained, ":ask-me"},
		{core.ExecutionPolicyConfiguredApprovals, ":ask-me"},
	} {
		staged := t.TempDir()
		path := filepath.Join(staged, "settings.json")
		writeFixture(t, path, []byte(`{
                "schema_version": 1,
                "provider": "anthropic",
                "model": "muse-1",
                "tui": {"theme": "dark"},
                "permissions": {"schema_version": 1, "default_profile": ":auto-review"}
            }`))

		if err := ApplyStagedExecutionSetting(core.HarnessKindMuse, entry.policy, staged); err != nil {
			t.Fatal(err)
		}

		document := readJSONFile(t, path)
		if document["provider"] != "anthropic" || document["model"] != "muse-1" ||
			jsonAt(document, "tui", "theme") != "dark" || document["schema_version"] != float64(1) ||
			jsonAt(document, "permissions", "schema_version") != float64(1) {
			t.Fatalf("%v", document)
		}
		if jsonAt(document, "permissions", "default_profile") != entry.profile {
			t.Fatalf("%v: %v", entry.policy, document)
		}
	}
}

// A raw local target keeps configured approvals for Muse: muse-acp's
// auto-review is its guardian, and Muse keeps its sandbox.
// Hard-won: 4a9dcb5: raw local Muse sessions used a permission profile Muse serve refused
func TestRawLocalMuseLaunchesWithGuardianApprovals(t *testing.T) {
	project := t.TempDir()
	session := store.CheckpointTestSession("session-muse")
	session.HarnessKind = core.HarnessKindMuse
	session.LastProfile = "muse"
	session.TargetTemplateID = "localhost"
	session.ProjectDirectory = &project
	const root = "/home/me/.local/share/hel/workers/session-muse"
	session.Target = core.TargetLocatorLocalBare{WorkerRoot: root}
	profile := core.NewHarnessProfile(core.HarnessKindMuse, "/profiles/muse")
	settings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})
	config := core.DefaultConfig()

	launch, _, _, err := WorkerLaunchConfig(&session, &profile, nil,
		&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root},
		LaunchWorkspace{SessionID: session.ID, WorkingDirectories: []string{}}, &settings, &config)
	if err != nil {
		t.Fatal(err)
	}

	if launch.ExecutionPolicy != core.ExecutionPolicyConfiguredApprovals {
		t.Fatalf("%v", launch.ExecutionPolicy)
	}
	if launch.Environment["MUSE_APPROVAL_MODE"] != "promptUnmatched" {
		t.Fatal(launch.Environment["MUSE_APPROVAL_MODE"])
	}
	if _, ok := launch.Environment["MUSE_SERVE_ARGS"]; ok {
		t.Fatal("a raw local Muse session got serve arguments")
	}
}

func TestStageKimiProfilePreservesDeviceIdentity(t *testing.T) {
	home := t.TempDir()
	writeFixture(t, filepath.Join(home, "config.toml"), []byte("default_model = \"k3\"\n"))
	writeFixture(t, filepath.Join(home, "device_id"), []byte("stable-device-id"))
	writeFixture(t, filepath.Join(home, "credentials/kimi-code.json"), []byte("{\"access_token\":\"secret\"}"))
	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindKimi, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}

	if readFixture(t, filepath.Join(staged, "device_id")) != "stable-device-id" {
		t.Fatal("the device identity changed")
	}
	if !isFile(filepath.Join(staged, "credentials/kimi-code.json")) {
		t.Fatal("the login was not staged")
	}
}

func TestStagedKimiProfileBindsHistoryToolsToTheTargetRuntime(t *testing.T) {
	home := t.TempDir()
	original := map[string]any{
		"mcpServers": map[string]any{
			"user-server": map[string]any{"command": "user-mcp", "args": []any{"serve"}},
		},
		"userSetting": true,
	}
	originalBody, err := json.MarshalIndent(original, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	writeFixture(t, filepath.Join(home, "mcp.json"), originalBody)
	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindKimi, home)
	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	socket := "/var/lib/hel/workers/session/control.sock"
	memory := core.ProjectMemoryLaunchConfig{
		HistorySocket:   &socket,
		ProjectKey:      "project",
		Root:            "/var/lib/hel/profiles/session/projects/project/memory",
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryHarnessProfile,
	}

	if err := ConfigureKimiHistoryMCP(staged, "/var/lib/hel/workers/session", memory.HistorySocket); err != nil {
		t.Fatal(err)
	}

	configured := readJSONFile(t, filepath.Join(staged, "mcp.json"))
	if configured["userSetting"] != true || jsonAt(configured, "mcpServers", "user-server", "command") != "user-mcp" {
		t.Fatalf("%v", configured)
	}
	if !jsonEquals(t, jsonAt(configured, "mcpServers", "mj-memory"), `{
            "transport": "stdio",
            "command": "/var/lib/hel/workers/session/hel",
            "args": [
                "worker",
                "memory-mcp",
                "--history-socket",
                "/var/lib/hel/workers/session/control.sock"
            ],
            "runtime_id": "local"
        }`) {
		t.Fatalf("%v", jsonAt(configured, "mcpServers", "mj-memory"))
	}
	if readFixture(t, filepath.Join(home, "mcp.json")) != string(originalBody) {
		t.Fatal("the controller-side Kimi profile must remain unchanged")
	}
}

func TestStagedKimiHistoryMcpResolvesSSHPathsFromTargetHome(t *testing.T) {
	staged := t.TempDir()
	socket := ".local/share/hel/workers/session/control.sock"
	memory := core.ProjectMemoryLaunchConfig{
		HistorySocket:   &socket,
		ProjectKey:      "project",
		Root:            ".local/share/hel/profiles/session/projects/project/memory",
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryHarnessProfile,
	}

	if err := ConfigureKimiHistoryMCP(staged, ".local/share/hel/workers/session", memory.HistorySocket); err != nil {
		t.Fatal(err)
	}

	server := jsonAt(readJSONFile(t, filepath.Join(staged, "mcp.json")), "mcpServers", "mj-memory")
	if jsonAt(server, "command") != "sh" || jsonAt(server, "runtime_id") != "local" {
		t.Fatalf("%v", server)
	}
	if !jsonEquals(t, jsonAt(server, "args"), `[
            "-c",
            "exec \"$HOME/$1\" worker memory-mcp --history-socket \"$HOME/$2\"",
            "mj-memory",
            ".local/share/hel/workers/session/hel",
            ".local/share/hel/workers/session/control.sock"
        ]`) {
		t.Fatalf("%v", server)
	}
}

func TestKimiGuidanceUsesAgentsMdWithoutMutatingTheSystemOverride(t *testing.T) {
	home := t.TempDir()
	const systemOverride = "# Custom Kimi system prompt\n"
	writeFixture(t, filepath.Join(home, "SYSTEM.md"), []byte(systemOverride))
	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindKimi, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := AppendHelTargetEnvironment(profile.Kind, staged, &core.TargetsTargetLocator{
		Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "container",
	}); err != nil {
		t.Fatal(err)
	}

	if readFixture(t, filepath.Join(staged, "AGENTS.md")) != MJContainerEnvironment {
		t.Fatal(readFixture(t, filepath.Join(staged, "AGENTS.md")))
	}
	if readFixture(t, filepath.Join(staged, "SYSTEM.md")) != systemOverride {
		t.Fatal("the staged system override changed")
	}
	if fixtureExists(filepath.Join(home, "AGENTS.md")) {
		t.Fatal("the profile home gained AGENTS.md")
	}
	if readFixture(t, filepath.Join(home, "SYSTEM.md")) != systemOverride {
		t.Fatal("the profile's system override changed")
	}
}

func TestContainerAttachmentGuidanceMatchesEnginePermissionsAndHarnessInstructionFiles(t *testing.T) {
	// Recorded from a disposable Podman container with ro/O/rw mounts; only
	// host paths have been normalized.
	mounts, err := os.ReadFile("testdata/tests/fixtures/container-attachment-mounts.json")
	if err != nil {
		t.Fatal(err)
	}
	mountExecutor := &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if command.Purpose != "inspect session container storage" {
			panic(command.Purpose)
		}
		if !slices.Contains(command.Args, "{{json .Mounts}}") {
			panic(fmt.Sprintf("%q", command.Args))
		}
		return core.CommandOutput{Stdout: mounts}, nil
	}}
	var attachments []core.AdditionalMount
	for _, entry := range []struct {
		destination string
		access      core.MountAccess
	}{{"/reference", core.MountAccessRo}, {"/scratch", core.MountAccessCow}, {"/shared", core.MountAccessRw}} {
		attachments = append(attachments, core.AdditionalMount{Source: "/host/external", Destination: entry.destination, Access: entry.access})
	}
	downgraded := attachments[0]
	downgraded.Access = core.MountAccessCow
	var output strings.Builder
	for _, entry := range []struct {
		name        string
		target      core.TargetsTargetLocator
		attachments []core.AdditionalMount
	}{
		{"Podman", core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "container"}, slices.Clone(attachments)},
		{"Docker with a downgraded overlay", core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: "container"}, []core.AdditionalMount{downgraded}},
		{"Apple container", core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorAppleContainer, ContainerID: "container"}, slices.Clone(attachments)},
		{"Container without external attachments", core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: "container"}, []core.AdditionalMount{}},
	} {
		storage, err := targets.InspectContainerStorage(&entry.target, entry.attachments, mountExecutor)
		if err != nil {
			t.Fatal(err)
		}
		var expected *string
		for _, harness := range core.HarnessKindAll {
			staged := t.TempDir()
			if err := AppendHelTargetEnvironment(harness, staged, &entry.target); err != nil {
				t.Fatal(err)
			}
			if err := AppendContainerStorageGuidance(harness, staged, &entry.target, nil, storage.ManagedTemporaryVolume, storage.Attachments); err != nil {
				t.Fatal(err)
			}
			text := readFixture(t, filepath.Join(staged, harness.AgentInstructionsFile()))
			if expected != nil {
				if text != *expected {
					t.Fatalf("%v: %s", harness, text)
				}
			} else {
				fmt.Fprintf(&output, "=== %s ===\n%s\n", entry.name, text)
				expected = &text
			}
		}
	}
	core.AssertGolden(t, "testdata", "container-attachment-instructions", output.String())
}

func TestProjectMemoryReplicasAreSessionPrivate(t *testing.T) {
	const key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
	if got := ProjectMemoryReplicaSlug(key, "session-a"); got != "hel-0123456789abcdef-session-a" {
		t.Fatal(got)
	}
	if ProjectMemoryReplicaSlug(key, "session-a") == ProjectMemoryReplicaSlug(key, "session-b") {
		t.Fatal("two sessions share a replica")
	}
}

// managedCodexLaunch is the launch config the upgrade tests prepare.
func managedCodexLaunch(sessionID, cwd string, environment map[string]string) core.WorkerLaunchConfig {
	if environment == nil {
		environment = map[string]string{}
	}
	return core.WorkerLaunchConfig{
		InitialCommands:       []core.InitialWorkerCommand{},
		Subagents:             core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		AgentMailboxesEnabled: true,
		SubtleSubagents:       true,
		ReviewTrigger:         core.ReviewTriggerOff,
		TargetEnvironment:     map[string]string{},
		SessionID:             sessionID,
		Harness:               core.HarnessKindCodex,
		BridgeCommand:         "ignored",
		BridgeArgs:            []string{},
		HarnessRuntime:        core.HarnessRuntimePolicyManaged,
		Environment:           environment,
		ExcludedEnvironment:   []string{},
		Cwd:                   cwd,
		AdditionalDirectories: []string{},
		ExecutionPolicy:       core.ExecutionPolicyConfiguredApprovals,
		ClaudePermissionMode:  core.ClaudePermissionModeAuto,
		MjolnirGuardian:       true,
	}
}

func TestRemoteUpgradePreparesManagedHarnessWithoutTouchingRunningWorker(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "hel")
	writeFixture(t, source, StampedWorker([]byte("managed harness worker")))
	const session = "session-remote"
	var recorder commandRecorder
	executor := digestExecutor("", &recorder)
	launch := managedCodexLaunch(session, "/srv/mj/session-remote/project", nil)

	// A host of its own, so no other test has claimed this store's
	// collection.
	locator := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "managed-harness-upgrade@host.test", SSHArgs: []string{}},
		Workspace: "/srv/mj/" + session,
	}
	if err := PrepareManagedHarnessForUpgrade(executor, &locator, session, WorkerInstallHost{Path: source}, &launch); err != nil {
		t.Fatal(err)
	}

	commands := recorder.all()
	var purposes []string
	for _, command := range commands {
		purposes = append(purposes, command.Purpose)
	}
	want := []string{
		"clear managed harness preparation staging",
		"create managed harness preparation staging",
		"remove stale private worker staging",
		"link shared Mjolnir worker into session",
		"collect unused Mjolnir worker binaries",
		"stage managed harness launch configuration",
		"prepare exact managed harness",
		"remove managed harness preparation staging",
	}
	if !reflect.DeepEqual(purposes, want) {
		t.Fatalf("%q", purposes)
	}
	for _, purpose := range purposes {
		if strings.Contains(purpose, "stop Mjolnir worker") || strings.Contains(purpose, "start Mjolnir worker") ||
			strings.Contains(purpose, "install the current Mjolnir worker binary") {
			t.Fatalf("the upgrade touched the running worker: %s", purpose)
		}
	}
	prepare := commands[slices.IndexFunc(commands, func(command core.CommandSpec) bool { return command.Purpose == "prepare exact managed harness" })]
	if rendered := prepare.Program + " " + strings.Join(prepare.Args, " "); !strings.Contains(rendered, "worker' 'prepare-harness' '--config'") {
		t.Fatal(rendered)
	}
}

func TestLocalUpgradePreflightUsesCurrentBinaryAndPreservesLaunchPolicy(t *testing.T) {
	var recorded *core.CommandSpec
	var prepared *core.WorkerLaunchConfig
	configRecording := &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if len(command.Args) < 4 {
			return core.CommandOutput{}, errors.New("local prepare command did not include its config path")
		}
		copied := *command
		recorded = &copied
		launch, err := core.WorkerLaunchConfigRead(command.Args[3])
		if err != nil {
			return core.CommandOutput{}, err
		}
		prepared = &launch
		return core.CommandOutput{}, nil
	}}
	launch := managedCodexLaunch("session-local", "/workspace/project", map[string]string{"CODEX_HOME": "/configured/profile/home"})
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: "/worker/session-local"}

	if err := PrepareManagedHarnessForUpgrade(configRecording, &locator, "session-local", WorkerInstallHost{Path: "/controller/hel"}, &launch); err != nil {
		t.Fatal(err)
	}

	if recorded == nil || recorded.Purpose != "prepare exact managed harness" || recorded.Program != "/controller/hel" ||
		!reflect.DeepEqual(recorded.Args[:3], []string{"worker", "prepare-harness", "--config"}) {
		t.Fatalf("%#v", recorded)
	}
	if strings.Contains(recorded.Args[3], "/worker/session-local") {
		t.Fatal(recorded.Args[3])
	}
	if prepared.Environment["CODEX_HOME"] != "/configured/profile/home" || prepared.ExecutionPolicy != core.ExecutionPolicyConfiguredApprovals {
		t.Fatalf("%v", prepared)
	}

	var purposes []string
	failing := &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		purposes = append(purposes, command.Purpose)
		return core.CommandOutput{}, errors.New("managed harness installation failed")
	}}
	err := PrepareManagedHarnessForUpgrade(failing, &locator, "session-local", WorkerInstallHost{Path: "/controller/hel"}, &launch)
	requireErrorContains(t, err, "managed harness installation failed")
	if !reflect.DeepEqual(purposes, []string{"prepare exact managed harness"}) {
		t.Fatalf("%q", purposes)
	}
}

// Hard-won: 5461a2c: recovery kept relaunching an incompatible remote worker that could not start
func TestARemoteWorkerWithAMismatchedBinaryIsReplacedBeforeRestart(t *testing.T) {
	source := filepath.Join(t.TempDir(), "worker")
	writeFixture(t, source, StampedWorker([]byte("fresh musl worker")))
	var recorder commandRecorder
	executor := digestExecutor(strings.Repeat("0", 64)+"  /root/hel\n", &recorder)
	locator := sshBareLocator("session-remote")
	probe := core.NewCommandSpec("true", nil)
	replaced, err := ReplaceTargetWorkerBinaryIfStale(executor, &locator, "session-remote", &probe, source)
	if err != nil {
		t.Fatal(err)
	}
	if !replaced {
		t.Fatal("a stale remote binary must be replaced")
	}
	if len(recorder.all()) <= 1 {
		t.Fatal("the digest probe must be followed by replacement commands")
	}
}

func TestARemoteRecoveryPlanDefersBinaryRefreshToTheRecoveryTask(t *testing.T) {
	locator := sshBareLocator("session-remote")
	refresh, err := WorkerBinaryRefreshPlan(&locator, "session-remote")
	if err != nil {
		t.Fatal(err)
	}
	switch refresh := refresh.(type) {
	case sessiontypes.WorkerBinaryRefreshDeferred:
		if refresh.Refresh.SessionID != "session-remote" || !reflect.DeepEqual(refresh.Refresh.Locator, locator) {
			t.Fatalf("%#v", refresh)
		}
	case sessiontypes.WorkerBinaryRefreshPrepared:
		t.Fatal("a remote target must defer, not prepare, its binary refresh")
	default:
		t.Fatal("a remote target now gets a binary refresh")
	}
}

// A daemon pins its worker sources once, at startup. A pin that no longer
// names a file must send the lookup back to resolution rather than failing
// every session until someone restarts the daemon (#1068).
// Hard-won: 924190f: a cache reaper removed a pinned worker source and sessions failed until restart
func TestAPinnedWorkerSourceWhoseFileIsGoneIsResolvedAgain(t *testing.T) {
	const present = "/pinned/hel"
	exists := func(path string) bool { return path == present }

	if !PinnedSourceIsUsable(WorkerBinaryAvailabilityLocal{Path: present, Source: "pinned"}, nil, exists) {
		t.Fatal("a pin that still names a file is used as it stands")
	}
	if PinnedSourceIsUsable(WorkerBinaryAvailabilityLocal{Path: "/reaped/build/hel", Source: "pinned"}, nil, exists) {
		t.Fatal("a pin whose build directory was removed must be resolved again")
	}
	remote := WorkerBinaryAvailabilityRemote{
		URL:    "https://example.invalid/hel",
		SHA256: strings.Repeat("a", 64),
		Triple: "x86_64-unknown-linux-musl",
	}
	if !PinnedSourceIsUsable(remote, nil, exists) {
		t.Fatal("a remote source is a URL and does not stop existing")
	}
	if PinnedSourceIsUsable(nil, errors.New("no Linux worker for x86_64-unknown-linux-musl"), exists) {
		t.Fatal("a source that never resolved must be tried again, not repeated back")
	}
}

type fixtureEntry struct {
	path   string
	staged bool
}

// fixtureHomeEntries is a fixture harness home shaped like a real one: the
// login and settings a session needs, next to native history, logs, caches
// and the skills the harness maintains itself. Each entry is a
// home-relative path and whether a staged home gets it.
func fixtureHomeEntries(kind core.HarnessKind) []fixtureEntry {
	switch kind {
	case core.HarnessKindCodex:
		return []fixtureEntry{
			{"auth.json", true},
			{"config.toml", true},
			{"AGENTS.md", true},
			{"skills/review/SKILL.md", true},
			{"sessions/2026/09/25/rollout-2026-09-25T09-00-00-native.jsonl", false},
			{"history.jsonl", false},
			{"session_index.jsonl", false},
			{"state_5.sqlite", false},
			{"logs_2.sqlite", false},
			{"thread_history_1.sqlite", false},
			{"models_cache.json", false},
			{"shell_snapshots/snapshot.sh", false},
			{"projects/hel-0123456789abcdef-0123456789abcdef0123456789abcdef/memory/MEMORY.md", false},
			{"skills/.system/imagegen/SKILL.md", false},
		}
	case core.HarnessKindClaude:
		return []fixtureEntry{
			{".credentials.json", true},
			{".claude.json", true},
			{"settings.json", true},
			{"CLAUDE.md", true},
			{"skills/review/SKILL.md", true},
			{"projects/-home-me-app/native.jsonl", false},
			{"history.jsonl", false},
			{"todos/native.json", false},
			{"shell-snapshots/snapshot.sh", false},
			{"statsig/cache", false},
			{"skills/synced/account/SKILL.md", false},
		}
	case core.HarnessKindKimi:
		return []fixtureEntry{
			{"credentials/kimi-code.json", true},
			{"config.toml", true},
			{"device_id", true},
			{"skills/review/SKILL.md", true},
			{"sessions/native/context.jsonl", false},
			{"session_index.jsonl", false},
			{"user-history/history.jsonl", false},
			{"logs/kimi.log", false},
			{"workspaces.json", false},
			{"telemetry/events.json", false},
		}
	case core.HarnessKindGrok:
		return []fixtureEntry{
			{"auth.json", true},
			{"config.toml", true},
			{"agent_id", true},
			{"skills/review/SKILL.md", true},
			{"sessions/native/session_search.sqlite", false},
			{"active_sessions.json", false},
			{"logs/grok.log", false},
			{"models_cache.json", false},
			{"memory-v2/store.json", false},
		}
	case core.HarnessKindMuse:
		return []fixtureEntry{
			{"auth.json", true},
			{"settings.json", true},
			{"trust.json", true},
			{"skills/review/SKILL.md", true},
			{"cache/models.json", false},
			{"logs/muse.log", false},
		}
	case core.HarnessKindOpenCode:
		// OpenCode 2's login is a row of its database, which also holds
		// every session; staging exports the row and leaves the database
		// behind.
		return []fixtureEntry{
			{"opencode.json", true},
			{"AGENTS.md", true},
			{"skills/review/SKILL.md", true},
			{".data/opencode/auth.json", false},
			{".data/opencode/opencode.db", false},
			{".data/opencode/opencode.db-wal", false},
			{".data/opencode/log/opencode.log", false},
			{".data/opencode/repos/native", false},
		}
	case core.HarnessKindAntigravity:
		return []fixtureEntry{
			{"antigravity-cli/antigravity-oauth-token", true},
			{"AGENTS.md", true},
			{"skills/review/SKILL.md", true},
			{"antigravity-cli/conversations/old.db", false},
			{"settings.json", false},
		}
	case core.HarnessKindDsh:
		// dsh-acp keeps only its sessions, written atomically through a
		// temporary file beside each record.
		return []fixtureEntry{
			{"AGENTS.md", true},
			{"skills/review/SKILL.md", true},
			{"sessions/dsh-0123.json", false},
			{"sessions/.dsh-0123.4567.tmp", false},
		}
	}
	panic(fmt.Sprintf("no fixture home for %v", kind))
}

// filesUnder is every regular file under root, as `/`-separated relative
// paths, sorted.
func filesUnder(t *testing.T, root string) []string {
	t.Helper()
	var files []string
	pending := []string{root}
	for len(pending) > 0 {
		directory := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		entries, err := os.ReadDir(directory)
		if err != nil {
			t.Fatal(err)
		}
		for _, entry := range entries {
			path := filepath.Join(directory, entry.Name())
			if info, err := os.Stat(path); err == nil && info.IsDir() {
				pending = append(pending, path)
				continue
			}
			relative, err := filepath.Rel(root, path)
			if err != nil {
				t.Fatal(err)
			}
			files = append(files, filepath.ToSlash(relative))
		}
	}
	sort.Strings(files)
	return files
}

// A staged home is what a session runs from on every target, this machine
// included. It gets exactly the login and settings the session needs, so
// the harness is signed in, and none of the profile home's native history,
// logs or caches.
func TestAStagedHomeGetsTheLoginAndSettingsAndNoNativeHistory(t *testing.T) {
	for _, kind := range core.HarnessKindAll {
		home := t.TempDir()
		staged := t.TempDir()
		for _, entry := range fixtureHomeEntries(kind) {
			writeFixture(t, filepath.Join(home, entry.path), []byte(`{"fixture":true}`))
		}
		if kind == core.HarnessKindOpenCode {
			WriteOpencodeDatabase(filepath.Join(home, ".data/opencode/opencode.db"), "sk-fixture")
		}
		profile := core.NewHarnessProfile(kind, home)

		if err := StageProfile(&profile, staged); err != nil {
			t.Fatal(err)
		}

		var expected []string
		for _, entry := range fixtureHomeEntries(kind) {
			if entry.staged {
				expected = append(expected, entry.path)
			}
		}
		if kind == core.HarnessKindOpenCode {
			expected = append(expected, kind.CredentialFileName())
			var logins any
			if err := json.Unmarshal([]byte(readFixture(t, filepath.Join(staged, kind.CredentialFileName()))), &logins); err != nil {
				t.Fatal(err)
			}
			if !jsonEquals(t, logins, `[{
                    "id": "cred_fixture",
                    "integrationID": "deepseek",
                    "label": "DeepSeek",
                    "active": true,
                    "value": {"type": "key", "key": "sk-fixture"}
                }]`) {
				t.Fatalf("%v", logins)
			}
		}
		sort.Strings(expected)
		if got := filesUnder(t, staged); !reflect.DeepEqual(got, expected) {
			t.Fatalf("%v: %q", kind, got)
		}
		// A harness that reads an API key from its environment has no login.
		_, usesKey := kind.APIKeyVariable()
		if isFile(core.HarnessAuthenticationMarker(kind, staged)) == usesKey {
			t.Fatalf("%v is signed in from its staged home", kind)
		}
	}
}

func TestStagedClaudeMailboxHookMergesAndRestagesWithoutDuplicates(t *testing.T) {
	stage := t.TempDir()
	path := filepath.Join(stage, "settings.json")
	userHook := map[string]any{"hooks": []any{map[string]any{"type": "command", "command": "user-hook"}}}
	settings, err := json.Marshal(map[string]any{
		"model":       "opus",
		"permissions": map[string]any{"deny": []any{"WebFetch"}},
		"hooks": map[string]any{
			"PostToolBatch": []any{userHook},
			"PreToolUse":    []any{map[string]any{"hooks": []any{map[string]any{"type": "command", "command": "before-tool"}}}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	writeFixture(t, path, settings)

	const remoteRoot = ".local/share/hel/workers/session"
	if err := ConfigureClaudeMailboxHook(stage, remoteRoot); err != nil {
		t.Fatal(err)
	}
	first := readFixture(t, path)
	if err := ConfigureClaudeMailboxHook(stage, remoteRoot); err != nil {
		t.Fatal(err)
	}
	if readFixture(t, path) != first {
		t.Fatal("restaging changed the settings")
	}

	var document map[string]any
	if err := json.Unmarshal([]byte(first), &document); err != nil {
		t.Fatal(err)
	}
	if document["model"] != "opus" || !jsonEquals(t, jsonAt(document, "permissions", "deny"), `["WebFetch"]`) {
		t.Fatalf("%v", document)
	}
	mailboxHooks := func(document map[string]any) []map[string]any {
		groups, _ := jsonAt(document, "hooks", "PostToolBatch").([]any)
		var hooks []map[string]any
		for _, group := range groups {
			entries, _ := jsonAt(group, "hooks").([]any)
			for _, entry := range entries {
				hook, _ := entry.(map[string]any)
				if command, ok := hook["command"].(string); ok && strings.HasPrefix(command, "MJOLNIR_MAILBOX_HOOK=1 ") {
					hooks = append(hooks, hook)
				}
			}
		}
		return hooks
	}
	groups, _ := jsonAt(document, "hooks", "PostToolBatch").([]any)
	if len(groups) == 0 || !reflect.DeepEqual(jsonRoundTrip(t, groups[0]), jsonRoundTrip(t, userHook)) {
		t.Fatalf("%v", groups)
	}
	hooks := mailboxHooks(document)
	if len(hooks) != 1 {
		t.Fatalf("%v", hooks)
	}
	want := fmt.Sprintf("MJOLNIR_MAILBOX_HOOK=1 %s worker mailbox-hook --socket %s --event PostToolBatch",
		core.PosixQuote(remoteRoot+"/hel"), core.PosixQuote(remoteRoot+"/control.sock"))
	if hooks[0]["command"] != want {
		t.Fatalf("%v", hooks[0]["command"])
	}
	if hooks[0]["timeout"] != float64(core.MailboxHookTimeoutSecs) {
		t.Fatalf("%v", hooks[0]["timeout"])
	}

	if err := ConfigureClaudeMailboxHook(stage, "/worker path/session"); err != nil {
		t.Fatal(err)
	}
	updated := mailboxHooks(readJSONFile(t, path))
	if len(updated) != 1 || !strings.Contains(updated[0]["command"].(string), "'/worker path/session/control.sock'") {
		t.Fatalf("%v", updated)
	}
}

func jsonRoundTrip(t *testing.T, value any) any {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var decoded any
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	return decoded
}

func TestFixedDelegationGuidanceIsPrivateExactAndIdempotent(t *testing.T) {
	mjolnir := core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	for _, harness := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindCodex, core.HarnessKindDsh, core.HarnessKindOpenCode} {
		home := t.TempDir()
		source := filepath.Join(home, harness.AgentInstructionsFile())
		writeFixture(t, source, []byte("User instructions without final newline"))
		for _, policy := range []core.SubagentPolicy{
			{Mode: core.SubagentPolicyNative},
			mjolnir,
			{Mode: core.SubagentPolicyNone},
			mjolnir,
		} {
			for _, offersSubtle := range []bool{true, false} {
				stage := t.TempDir()
				destination := filepath.Join(stage, harness.AgentInstructionsFile())
				if err := CopyProfileEntry(source, destination); err != nil {
					t.Fatal(err)
				}
				for range 2 {
					if err := AppendSubagentPolicy(harness, stage, &policy, 9, offersSubtle); err != nil {
						t.Fatal(err)
					}
				}
				actual := readFixture(t, destination)
				isMjolnir := policy.Mode == core.SubagentPolicyMjolnir
				expected := "User instructions without final newline"
				if isMjolnir {
					expected += "\n\n" + core.DelegationPolicy(9, offersSubtle)
				}
				if actual != expected {
					t.Fatalf("%v %v: %q", harness, policy.Mode, actual)
				}
				if strings.Contains(actual, "$N") || strings.Contains(actual, "$TASK_TYPES") {
					t.Fatal(actual)
				}
				if strings.Contains(actual, "task_type") != (offersSubtle && isMjolnir) {
					t.Fatal(actual)
				}
			}
			if readFixture(t, source) != "User instructions without final newline" {
				t.Fatal("the source instructions changed")
			}
		}
	}
}

// RCL-3: a restart re-hashed and re-copied every unchanged worker, which
// took 30 s for one debug build on a busy disk. An unchanged source is now
// recognised from its path, size, mtime, inode and ctime.
// Hard-won: 8b62e1b: rehashing unchanged worker sources delayed daemon startup by tens of seconds
// Hard-won: daemon heap profile: resolving an unchanged pinned copy still rescanned the executable.
func TestAnUnchangedWorkerSourceIsNotReadAgainWhenPinning(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "worker")
	writeFixture(t, source, StampedWorker([]byte(strings.Repeat("\x07", 4<<20))))
	cache := filepath.Join(directory, "cache")

	first, err := CopyWorkerSourceToCache(source, cache)
	if err != nil {
		t.Fatal(err)
	}
	if err := VerifyWorkerBuildIndexed(cache, source); err != nil {
		t.Fatal(err)
	}
	if indexed, ok := IndexedPinnedWorker(cache, first); !ok || indexed != first {
		t.Fatalf("%s %v", indexed, ok)
	}
	if err := VerifyWorkerBuildIndexed(cache, first); err != nil {
		t.Fatal(err)
	}

	// Damage the pinned copy without changing its length. A pin that read
	// the source or re-hashed the copy would notice; the indexed one does
	// not.
	info, err := os.Stat(source)
	if err != nil {
		t.Fatal(err)
	}
	writeFixture(t, first, make([]byte, info.Size()))
	if _, ok := IndexedPinnedWorker(cache, first); ok {
		t.Fatal("a damaged copy is still indexed")
	}
	if err := VerifyWorkerBuildIndexed(cache, first); err == nil {
		t.Fatal("a damaged copy verified")
	}
	if again, err := CopyWorkerSourceToCache(source, cache); err != nil || again != first {
		t.Fatalf("%s %v", again, err)
	}
	if err := VerifyWorkerBuildIndexed(cache, source); err != nil {
		t.Fatal(err)
	}

	// A rewritten source (new size or mtime) is read and checked in full.
	writeFixture(t, source, []byte("legacy worker"))
	if err := VerifyWorkerBuildIndexed(cache, source); err == nil {
		t.Fatal("a rewritten stale source verified")
	}
	if _, err := CopyWorkerSourceToCache(source, cache); err == nil {
		t.Fatal("a rewritten stale source was pinned")
	}
}

func TestAMissingPinnedCopyOrIndexFallsBackToAFullPin(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "worker")
	writeFixture(t, source, StampedWorker([]byte("bytes")))
	cache := filepath.Join(directory, "cache")
	first, err := CopyWorkerSourceToCache(source, cache)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(first); err != nil {
		t.Fatal(err)
	}
	if again, err := CopyWorkerSourceToCache(source, cache); err != nil || again != first {
		t.Fatalf("%s %v", again, err)
	}
	if err := core.VerifyWorkerBuild(first); err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(filepath.Join(cache, "index")); err != nil {
		t.Fatal(err)
	}
	if again, err := CopyWorkerSourceToCache(source, cache); err != nil || again != first {
		t.Fatalf("%s %v", again, err)
	}
}

func TestRefreshingProfileConfigurationClearsEveryAWSFileIncludingTheScopeRecord(t *testing.T) {
	executor := newSSHHostExecutor()
	if err := ClearProfileConfiguration(executor, &core.TargetsTargetLocator{
		Kind:        core.TargetsTargetLocatorLocalPodman,
		ContainerID: strings.Repeat("c", 64),
	}, "0123456789abcdef0123456789abcdef", "/home/agent/.codex", core.HarnessKindCodex); err != nil {
		t.Fatal(err)
	}
	commands := executor.recorder.all()
	last := commands[len(commands)-1]
	index := slices.IndexFunc(last.Args, func(argument string) bool { return strings.Contains(argument, "rm -rf") })
	if index < 0 {
		t.Fatal("the removal script runs")
	}
	script := last.Args[index]
	for _, name := range core.AwsCredentialsFiles {
		if !strings.Contains(script, name) {
			t.Fatalf("%s is not cleared: %s", name, script)
		}
	}
}

// Hard-won: 50785454b: read-only source instructions aborted staging; an index read before copying could differ from the replica.
func TestGoldenSessionInstructionsAreAppendedOnceToEachFreshNativeStage(t *testing.T) {
	legacyLaunch, err := os.ReadFile("../worker/testdata/acp/project-memory/legacy-reviewer-launch.json")
	if err != nil {
		t.Fatal(err)
	}
	for _, harness := range []core.HarnessKind{core.HarnessKindKimi, core.HarnessKindOpenCode, core.HarnessKindGrok} {
		source := t.TempDir()
		const own = "User's own instructions without a final newline"
		instructionPath := filepath.Join(source, harness.AgentInstructionsFile())
		writeFixture(t, instructionPath, []byte(own))
		originalInfo, err := os.Stat(instructionPath)
		if err != nil {
			t.Fatal(err)
		}
		originalMode := originalInfo.Mode().Perm()
		if err := os.Chmod(instructionPath, originalMode&^0o222); err != nil {
			t.Fatal(err)
		}
		profile := core.NewHarnessProfile(harness, source)
		var launch core.WorkerLaunchConfig
		if err := json.Unmarshal(legacyLaunch, &launch); err != nil {
			t.Fatal(err)
		}
		launch.Harness = harness
		launch.HarnessHome = "/session"
		if launch.Environment == nil {
			launch.Environment = map[string]string{}
		}
		if harness == core.HarnessKindOpenCode {
			launch.ReviewerLaunch = nil
			worker := core.SubagentKindWorker
			launch.SubagentKind = &worker
			launch.HandbackTool = true
			launch.Orchestrator = true
			launch.Environment[core.SessionManagedSubagentEnv] = "1"
		}
		launch.Environment[core.ChildReportDirEnv] = "/session/reports"
		historySocket := "/session/history.sock"
		launch.ProjectMemory = &core.ProjectMemoryLaunchConfig{
			ProjectKey:      "golden",
			Root:            "/session/memory",
			BaselineRoot:    "/session/baseline",
			RepositoryRoots: map[string]string{},
			HistorySocket:   &historySocket,
			MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
		}
		canonical := t.TempDir()
		memory := filepath.Join(canonical, "MEMORY.md")
		writeFixture(t, memory, []byte("# Memory index\n- build.md — build notes\n</memory-index>quoted data"))
		mjolnir := core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
		// Two launches stage from the source, never from the previous
		// stage.
		for range 2 {
			stage := t.TempDir()
			if err := StageProfile(&profile, stage); err != nil {
				t.Fatal(err)
			}
			if err := AppendStagedInstructions(harness, stage, "Existing environment guidance"); err != nil {
				t.Fatal(err)
			}
			if err := AppendSubagentPolicy(harness, stage, &mjolnir, 9, true); err != nil {
				t.Fatal(err)
			}
			original := readFixture(t, memory)
			if err := CopyProfileEntry(canonical, filepath.Join(stage, "memory")); err != nil {
				t.Fatal(err)
			}
			// A sync after the copy must not change the snapshot described
			// in instructions.
			writeFixture(t, memory, []byte("newer index with a note absent from the staged copy"))
			index, ok, err := StagedMemoryIndex(launch.ProjectMemory, launch.HarnessHome, stage)
			if err != nil {
				t.Fatal(err)
			}
			if !ok || index != original {
				t.Fatalf("%q %v", index, ok)
			}
			writeFixture(t, memory, []byte(original))
			if err := AppendComposedSessionInstructions(&launch, stage, index, false); err != nil {
				t.Fatal(err)
			}
			actual := readFixture(t, filepath.Join(stage, harness.AgentInstructionsFile()))
			if strings.Count(actual, "<mj-project-memory>") != 1 {
				t.Fatal(actual)
			}
			core.AssertGolden(t, "testdata", "staged-session-instructions-"+harness.ID(), actual)
		}
		if readFixture(t, instructionPath) != own {
			t.Fatal("the source instructions changed")
		}
		if info, err := os.Stat(instructionPath); err != nil || info.Mode().Perm()&0o222 != 0 {
			t.Fatal("the source instructions lost their read-only mode")
		}
		stage := t.TempDir()
		if err := StageProfile(&profile, stage); err != nil {
			t.Fatal(err)
		}
		if err := AppendComposedSessionInstructions(&launch, stage, "index <unavailable> & unreadable", true); err != nil {
			t.Fatal(err)
		}
		actual := readFixture(t, filepath.Join(stage, harness.AgentInstructionsFile()))
		if !strings.Contains(actual, "index unavailable: index &lt;unavailable&gt; &amp; unreadable") ||
			!strings.Contains(actual, core.MemoryGuidance) {
			t.Fatal(actual)
		}
		// Permit cleanup on platforms whose read-only attribute prevents
		// removal.
		if err := os.Chmod(instructionPath, originalMode); err != nil {
			t.Fatal(err)
		}
	}
}

func TestSessionStateInstructionsAreAppendedOnceForEveryHarness(t *testing.T) {
	text := core.SessionStateInstructions
	for _, expected := range []string{"## Keeping your work across suspend and move", "$MJ_KEEP_DIR", "Git-ignored files are never kept."} {
		if !strings.Contains(text, expected) {
			t.Fatalf("the instructions lack %q", expected)
		}
	}
	for _, harness := range core.HarnessKindAll {
		stage := t.TempDir()
		path := filepath.Join(stage, harness.AgentInstructionsFile())
		writeFixture(t, path, []byte("existing\n"))
		for range 2 {
			if err := AppendSessionStateInstructions(harness, stage); err != nil {
				t.Fatal(err)
			}
		}
		actual := readFixture(t, path)
		if !strings.HasPrefix(actual, "existing\n") || strings.Count(actual, "## Keeping your work across suspend and move") != 1 || !strings.Contains(actual, text) {
			t.Fatalf("%v\n%s", harness, actual)
		}
	}
}
