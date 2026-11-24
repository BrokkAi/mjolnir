// Port of mj-worker-staging/src/worker_binary/tests.rs, part 4: managed
// skills, Jev settings, build-cache shims, and the credentials each launch
// carries.

package workerstaging

import (
	"encoding/json"
	"fmt"
	"maps"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

func TestIsolatedProfileStagingRemovesTheHostCliSkillAndMatchesSync(t *testing.T) {
	for _, kind := range core.HarnessKindAll {
		home := t.TempDir()
		directory := core.MjSkillDirectory(kind)
		mj := filepath.Join(home, directory)
		writeFixture(t, filepath.Join(mj, "SKILL.md"), []byte("old mj"))
		writeFixture(t, filepath.Join(mj, "references/old.md"), []byte("old reference"))
		other := filepath.Join(home, kind.SyncedSkillDirs()[0], "review/SKILL.md")
		writeFixture(t, other, []byte("review"))
		profile := zaiProfile(t, home)
		profile.Kind = kind
		stage := t.TempDir()
		if err := StageProfile(&profile, stage); err != nil {
			t.Fatal(err)
		}
		if err := StageManagedSkills(kind, stage, core.SkillsScopeIsolated); err != nil {
			t.Fatal(err)
		}
		if fixtureExists(filepath.Join(stage, directory)) {
			t.Fatalf("%v kept the host CLI skill", kind)
		}
		installed, err := core.CollectSkills(kind, stage)
		if err != nil {
			t.Fatal(err)
		}
		expected, err := core.SessionSkills(kind, home, core.SkillsArchiveFormatGzip, core.SkillsScopeIsolated)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(installed, expected) {
			t.Fatalf("%v: the staged skills differ from what the sync pushes", kind)
		}
		relative, err := filepath.Rel(home, other)
		if err != nil {
			t.Fatal(err)
		}
		if readFixture(t, filepath.Join(stage, relative)) != "review" {
			t.Fatalf("%v lost the user's skill", kind)
		}
	}
}

// Claude Code provisions `skills/synced/` from the user's claude.ai
// account, and keeps `skills/.trash/`, in whatever home it runs from, the
// session's included; the Codex CLI does the same with its built-in skills
// in `skills/.system/`. Launch leaves these to the harness rather than
// copying them into every session (4 MB of them for Claude on the launch
// host).
func TestStagingLeavesHarnessOwnedSkillsToTheHarness(t *testing.T) {
	for _, kind := range core.HarnessKindAll {
		var owned []string
		switch kind {
		case core.HarnessKindClaude:
			owned = []string{
				"skills/synced/.bucket-org_user",
				"skills/synced/org_user/manifest.json",
				"skills/synced/org_user/docx/SKILL.md",
				"skills/.trash/1789646711611/pdf/SKILL.md",
			}
		case core.HarnessKindCodex:
			owned = []string{
				"skills/.system/.codex-system-skills.marker",
				"skills/.system/imagegen/SKILL.md",
			}
		}
		home := t.TempDir()
		skill := kind.SyncedSkillDirs()[0] + "/review/SKILL.md"
		for _, relative := range append([]string{skill}, owned...) {
			writeFixture(t, filepath.Join(home, relative), []byte(relative+"\n"))
		}

		staged := t.TempDir()
		profile := core.NewHarnessProfile(kind, home)

		if err := StageProfile(&profile, staged); err != nil {
			t.Fatal(err)
		}
		if err := StageManagedSkills(profile.Kind, staged, core.SkillsScopeLocalhost); err != nil {
			t.Fatal(err)
		}

		if readFixture(t, filepath.Join(staged, skill)) != skill+"\n" {
			t.Fatalf("%v lost the user's skill", kind)
		}
		for _, path := range kind.HarnessOwnedSkillPaths() {
			if !slices.ContainsFunc(owned, func(relative string) bool { return strings.HasPrefix(relative, path+"/") }) {
				t.Fatalf("no test file under %v %s", kind, path)
			}
			if fixtureExists(filepath.Join(staged, path)) {
				t.Fatalf("%v %s was staged", kind, path)
			}
		}
		for _, relative := range owned {
			if fixtureExists(filepath.Join(staged, relative)) {
				t.Fatalf("%v %s was staged", kind, relative)
			}
		}
		expected, err := core.SessionSkills(profile.Kind, home, core.SkillsArchiveFormatGzip, core.SkillsScopeLocalhost)
		if err != nil {
			t.Fatal(err)
		}
		installed, err := core.CollectSkills(profile.Kind, staged)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(installed, expected) {
			t.Fatalf("%v: the staged skills differ from what the sync pushes", kind)
		}
	}
}

func ubuntuContainerTemplate() core.ContainerTemplate {
	return core.ContainerTemplate{Image: "ubuntu:24.04"}
}

// `[jev] enabled = false` reaches the worker as its launch environment and
// takes the Jev key out of both the worker's and the harness's environment.
func TestTheJevSwitchReachesTheWorkerAndRemovesTheKey(t *testing.T) {
	home := t.TempDir()
	profile := zaiProfile(t, home)
	const sessionID = "0123456789abcdef0123456789abcdef"
	workspace, err := core.NewContainerWorkspace(sessionID)
	if err != nil {
		t.Fatal(err)
	}
	bundle := store.LocalBundle("/src/project")
	containerID, err := core.ResourceName(sessionID)
	if err != nil {
		t.Fatal(err)
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: containerID}
	settings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalPodman{ContainerTemplate: ubuntuContainerTemplate()})
	session := store.CheckpointTestSession(sessionID)
	session.HarnessKind = core.HarnessKindCodex
	session.LastProfile = "glm"
	session.ProjectDirectory = nil
	session.ContainerWorkspace = &workspace
	config := core.DefaultConfig()
	launch, _, _, err := WorkerLaunchConfig(&session, &profile, &bundle, &locator, LaunchWorkspace{
		SessionID:          sessionID,
		Container:          &workspace,
		WorkingDirectories: []string{},
	}, &settings, &config)
	if err != nil {
		t.Fatal(err)
	}
	on := cloneLaunch(t, launch)
	defaults := core.DefaultJevConfig()
	ApplyJevSettings(&on, &defaults)
	if !reflect.DeepEqual(on.TargetEnvironment, launch.TargetEnvironment) || !reflect.DeepEqual(on.Environment, launch.Environment) {
		t.Fatal("the default Jev settings changed the launch")
	}

	resolver := core.SecretResolverFixed(map[string]string{}, map[string]string{"FASTINO": "personal-key"})
	jev := core.JevConfig{Enabled: true, FastinoKey: core.EnvironmentValueFromSecret{Value: "FASTINO"}}
	core.WithSecretResolver(resolver, func() struct{} {
		ApplyJevSettings(&launch, &jev)
		return struct{}{}
	})
	if launch.TargetEnvironment[core.FastinoKeyEnvironment] != "personal-key" {
		t.Fatalf("%v", launch.TargetEnvironment)
	}
	if _, ok := launch.Environment[core.FastinoKeyEnvironment]; ok {
		t.Fatal("the harness got the Fastino key")
	}

	jev.Enabled = false
	ApplyJevSettings(&launch, &jev)
	for _, environment := range []map[string]string{launch.TargetEnvironment, launch.Environment} {
		if _, ok := environment[core.FastinoKeyEnvironment]; ok {
			t.Fatal("a disabled Jev kept the key")
		}
		if environment[core.DisabledEnvironment] != "1" {
			t.Fatalf("%v", environment)
		}
	}
}

// cloneLaunch is a copy of launch whose environments are its own, as Rust's
// Clone makes for the fields ApplyJevSettings edits.
func cloneLaunch(t *testing.T, launch core.WorkerLaunchConfig) core.WorkerLaunchConfig {
	t.Helper()
	copied := launch
	copied.TargetEnvironment = maps.Clone(launch.TargetEnvironment)
	copied.Environment = maps.Clone(launch.Environment)
	return copied
}

// lineRecorder records each command as one rendered line and answers with
// output.
func lineRecorder(lines *[]string, mu *sync.Mutex, output core.CommandOutput) *workerTestExecutor {
	return &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		mu.Lock()
		defer mu.Unlock()
		*lines = append(*lines, command.Program+" "+strings.Join(command.Args, " "))
		return output, nil
	}}
}

func TestInstallingTheBuildCachePlacesMarkedSharedCopyShimsOnTheSessionPath(t *testing.T) {
	var commands []string
	var mu sync.Mutex
	executor := lineRecorder(&commands, &mu, core.CommandOutput{})
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "hel-session"}
	configuration := "/cache/.mjolnir/config/mbx"

	if err := InstallMbxShims(executor, &locator, "/home/hel/.hel/worker", "/mnt/fast/mbx-cache/.mjolnir/bin/mbx", &configuration, nil); err != nil {
		t.Fatal(err)
	}

	index := slices.IndexFunc(commands, func(line string) bool { return strings.Contains(line, "MBX_CARGO_SHIM_MODE=1") })
	if index < 0 {
		t.Fatal("the supported mbx Cargo launcher is written in the container")
	}
	shim := commands[index]
	if !strings.Contains(shim, "MBX_CARGO_SHIM_PATH=$(command -v \"$0\")") || !strings.Contains(shim, "# mjolnir-mbx-shim") {
		t.Fatal(shim)
	}
	if !strings.Contains(shim, core.PosixQuote("exec '/mnt/fast/mbx-cache/.mjolnir/bin/mbx' \"$@\"")) {
		t.Fatalf("both wrappers execute the synchronized cache copy: %s", shim)
	}
	if slices.ContainsFunc(commands, func(line string) bool { return strings.Contains(line, " cp ") }) {
		t.Fatalf("%q", commands)
	}
	if !slices.ContainsFunc(commands, func(line string) bool {
		return strings.Contains(line, "'exec' '-i' 'hel-session' 'sh' '-c'") && strings.Contains(line, "config/mbx")
	}) {
		t.Fatalf("the machine mbx configuration is linked into the container: %q", commands)
	}
}

func TestContainerMbxVersionMustMatchTheSyncedCopyBeforeShimInstallation(t *testing.T) {
	var commands []string
	var mu sync.Mutex
	executor := lineRecorder(&commands, &mu, commandOutputOf(0, "mbx 1.23.0", ""))
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "hel-session"}
	err := VerifyMbxBinary(executor, &locator, "/mnt/fast/mbx-cache/.mjolnir/bin/mbx", "1.22.0")
	requireErrorContains(t, err, "container reports mbx 1.23.0")
	if len(commands) != 1 || !strings.Contains(commands[0], "podman exec hel-session /mnt/fast/mbx-cache/.mjolnir/bin/mbx --version") {
		t.Fatalf("%q", commands)
	}
}

// codexLoginProfile is a Codex profile whose `auth.json` records authMode,
// and whose own environment sets an API key.
func codexLoginProfile(t *testing.T, home, authMode string) core.HarnessProfile {
	t.Helper()
	login, err := json.Marshal(map[string]any{"auth_mode": authMode, "OPENAI_API_KEY": nil})
	if err != nil {
		t.Fatal(err)
	}
	writeFixture(t, filepath.Join(home, "auth.json"), login)
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	profile.Environment = core.EnvironmentFromMap(map[string]string{
		"OPENAI_API_KEY":  "sk-svcacct-profile",
		"PROFILE_SETTING": "kept",
	})
	return profile
}

type targetLaunch struct {
	target string
	launch core.WorkerLaunchConfig
}

// launchesOnEveryTarget is the launch config of one session of profile on
// this machine, in its own container, and as a sub-agent child in its
// parent's container. Every target sets `CODEX_API_KEY` and
// `OPENAI_BASE_URL` in its own environment.
func launchesOnEveryTarget(t *testing.T, profile *core.HarnessProfile) []targetLaunch {
	t.Helper()
	const parentID = "0123456789abcdef0123456789abcdef"
	const childID = "1123456789abcdef0123456789abcdef"
	parentWorkspace, err := core.NewContainerWorkspace(parentID)
	if err != nil {
		t.Fatal(err)
	}
	bundle := store.LocalBundle("/src/project")
	container := core.TargetTemplateLocalPodman{ContainerTemplate: ubuntuContainerTemplate()}
	withTargetKeys := func(template core.TargetTemplate) core.TargetRuntimeSettings {
		settings := core.TargetRuntimeSettingsFromTemplate(template)
		if settings.Environment == nil {
			settings.Environment = map[string]string{}
		}
		settings.Environment["CODEX_API_KEY"] = "sk-target"
		settings.Environment["OPENAI_BASE_URL"] = "https://example.invalid/v1"
		return settings
	}
	config := core.DefaultConfig()
	launch := func(session core.SessionRecord, bundle *core.ProjectBundle, locator core.TargetsTargetLocator, workspace LaunchWorkspace, settings core.TargetRuntimeSettings) core.WorkerLaunchConfig {
		t.Helper()
		launch, _, _, err := WorkerLaunchConfig(&session, profile, bundle, &locator, workspace, &settings, &config)
		if err != nil {
			t.Fatal(err)
		}
		return launch
	}

	project := t.TempDir()
	local := store.CheckpointTestSession(parentID)
	local.TargetTemplateID = "localhost"
	local.ProjectDirectory = &project
	localRoot := "/home/me/.local/share/hel/workers/" + parentID
	local.Target = core.TargetLocatorLocalBare{WorkerRoot: localRoot}
	localhost := launch(local, nil,
		core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: localRoot},
		LaunchWorkspace{SessionID: parentID, WorkingDirectories: []string{}},
		withTargetKeys(core.TargetTemplateLocalBare{}))

	containerID, err := core.ResourceName(parentID)
	if err != nil {
		t.Fatal(err)
	}
	parent := store.CheckpointTestSession(parentID)
	parent.ProjectDirectory = nil
	parent.ContainerWorkspace = &parentWorkspace
	inContainer := launch(parent, &bundle,
		core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: containerID},
		LaunchWorkspace{SessionID: parentID, Container: &parentWorkspace, WorkingDirectories: []string{}},
		withTargetKeys(container))

	child := store.CheckpointTestSession(childID)
	child.ProjectDirectory = nil
	child.ContainerWorkspace = &parentWorkspace
	borrowedFrom := parentID
	asChild := launch(child, &bundle,
		core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: containerID, BorrowedFrom: &borrowedFrom},
		LaunchWorkspace{SessionID: parentID, Container: &parentWorkspace, WorkingDirectories: []string{}},
		withTargetKeys(container))
	return []targetLaunch{
		{"localhost", localhost},
		{"container", inContainer},
		{"sub-agent", asChild},
	}
}

func TestCpuAccountingFollowsContainerPlacementInLaunchConfig(t *testing.T) {
	home := t.TempDir()
	profile := codexLoginProfile(t, home, "chatgpt")
	for _, entry := range launchesOnEveryTarget(t, &profile) {
		expected := core.CpuAccountingModeContainer
		if entry.target == "localhost" {
			expected = core.CpuAccountingModeProcessTree
		}
		if entry.launch.CPUAccounting != expected {
			t.Fatalf("%s: %v", entry.target, entry.launch.CPUAccounting)
		}
	}
}

func TestPodmanHarnessesInheritContainerHomeAndClaudeGetsTheSandboxMarker(t *testing.T) {
	home := t.TempDir()
	codex := codexLoginProfile(t, home, "chatgpt")
	for _, entry := range launchesOnEveryTarget(t, &codex) {
		launch := entry.launch
		if entry.target == "localhost" {
			if _, ok := launch.Environment["HOME"]; ok {
				t.Fatal("localhost set HOME")
			}
			if _, ok := launch.Environment[core.SessionGitConfigIncludePath]; ok {
				t.Fatal("localhost set the git config include")
			}
			continue
		}
		if _, ok := launch.TargetEnvironment["HOME"]; ok {
			t.Fatalf("%s set the target's HOME", entry.target)
		}
		if _, ok := launch.Environment["HOME"]; ok {
			t.Fatalf("%s set HOME", entry.target)
		}
		if launch.TargetEnvironment[core.SessionGitConfigIncludePath] != "/home/hel/.gitconfig" ||
			launch.Environment[core.SessionGitConfigIncludePath] != "/home/hel/.gitconfig" {
			t.Fatalf("%s: git config include", entry.target)
		}
		if _, ok := launch.Environment["IS_SANDBOX"]; ok {
			t.Fatalf("%s: Codex got the sandbox marker", entry.target)
		}
	}

	claude := core.NewHarnessProfile(core.HarnessKindClaude, home)
	for _, entry := range launchesOnEveryTarget(t, &claude) {
		launch := entry.launch
		if entry.target == "localhost" {
			if _, ok := launch.Environment["IS_SANDBOX"]; ok {
				t.Fatal("localhost Claude got the sandbox marker")
			}
			continue
		}
		if _, ok := launch.Environment["HOME"]; ok {
			t.Fatalf("%s set HOME", entry.target)
		}
		if launch.Environment[core.SessionGitConfigIncludePath] != "/home/hel/.gitconfig" {
			t.Fatalf("%s: git config include", entry.target)
		}
		if launch.Environment["IS_SANDBOX"] != "1" {
			t.Fatalf("%s: no sandbox marker", entry.target)
		}
	}

	// Hard-won: review of a13f351: Docker sessions on the root default image got no sandbox marker, so Claude refused to start.
	const parentID = "0123456789abcdef0123456789abcdef"
	workspace, err := core.NewContainerWorkspace(parentID)
	if err != nil {
		t.Fatal(err)
	}
	session := store.CheckpointTestSession(parentID)
	session.ProjectDirectory = nil
	session.ContainerWorkspace = &workspace
	docker := core.TargetTemplateLocalDocker{ContainerTemplate: core.ContainerTemplate{Image: core.DefaultSessionImage}}
	containerID, err := core.ResourceName(parentID)
	if err != nil {
		t.Fatal(err)
	}
	bundle := store.LocalBundle("/src/project")
	settings := core.TargetRuntimeSettingsFromTemplate(docker)
	config := core.DefaultConfig()
	launch, _, _, err := WorkerLaunchConfig(&session, &claude, &bundle,
		&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: containerID},
		LaunchWorkspace{SessionID: parentID, Container: &workspace, WorkingDirectories: []string{}},
		&settings, &config)
	if err != nil {
		t.Fatal(err)
	}
	if launch.Environment["IS_SANDBOX"] != "1" {
		t.Fatal("a Docker Claude session got no sandbox marker")
	}
}

func TestOnlyLocalhostHarnessesReceiveTheOwningDaemonsConfigurationPaths(t *testing.T) {
	home := t.TempDir()
	profile := codexLoginProfile(t, home, "chatgpt")
	for _, entry := range launchesOnEveryTarget(t, &profile) {
		launch := entry.launch
		if entry.target == "localhost" {
			configDir, _ := filepath.Abs(core.ConfigDir())
			dataDir, _ := filepath.Abs(core.DataDir())
			if launch.Environment["MJ_CONFIG_DIR"] != configDir || launch.Environment["MJ_DATA_DIR"] != dataDir {
				t.Fatalf("%v", launch.Environment)
			}
			instance, ok := core.InstanceName()
			got, present := launch.Environment["MJ_INSTANCE"]
			if ok != present || got != instance {
				t.Fatalf("MJ_INSTANCE %q %v, want %q %v", got, present, instance, ok)
			}
		} else {
			for _, name := range []string{"MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE"} {
				if _, ok := launch.Environment[name]; ok {
					t.Fatalf("%s: %s", entry.target, name)
				}
			}
		}
	}
}

// #1160: eleven Codex children of a ChatGPT profile died on their first
// request, some with a service-account API key the ChatGPT backend
// rejected. A ChatGPT profile's harness environment carries no such key on
// any target, and the launch tells the worker to remove the same variables
// from the target's own login environment, which only the worker sees.
// Hard-won: c4e2838: shipped ChatGPT Codex children failed when an inherited API key reached chatgpt.com
func TestAChatgptCodexLaunchCarriesNoAPIKeyOnAnyTarget(t *testing.T) {
	home := t.TempDir()
	profile := codexLoginProfile(t, home, "chatgpt")
	for _, entry := range launchesOnEveryTarget(t, &profile) {
		launch := entry.launch
		for _, name := range core.CodexCredentialEnvironment {
			if _, ok := launch.Environment[name]; ok {
				t.Fatalf("%s: the harness environment still sets %s", entry.target, name)
			}
		}
		if !reflect.DeepEqual(launch.ExcludedEnvironment, core.CodexCredentialEnvironment[:]) {
			t.Fatalf("%s: the worker is not told what to remove: %q", entry.target, launch.ExcludedEnvironment)
		}
		if launch.Environment["PROFILE_SETTING"] != "kept" {
			t.Fatalf("%s lost the profile's own setting", entry.target)
		}
	}
}

// A Codex profile that signs in with an API key keeps the variables that
// carry it, whether `codex login --with-api-key` stored it or a custom
// provider names it.
func TestAnAPIKeyCodexLaunchKeepsItsKeyOnEveryTarget(t *testing.T) {
	fresh := core.NewHarnessProfile(core.HarnessKindCodex, t.TempDir())
	for _, entry := range launchesOnEveryTarget(t, &fresh) {
		if entry.launch.Environment["CODEX_API_KEY"] != "sk-target" || len(entry.launch.ExcludedEnvironment) != 0 {
			t.Fatalf("%s: %v %q", entry.target, entry.launch.Environment, entry.launch.ExcludedEnvironment)
		}
	}

	profile := codexLoginProfile(t, t.TempDir(), "apikey")
	for _, entry := range launchesOnEveryTarget(t, &profile) {
		launch := entry.launch
		if launch.Environment["OPENAI_API_KEY"] != "sk-svcacct-profile" || launch.Environment["CODEX_API_KEY"] != "sk-target" || len(launch.ExcludedEnvironment) != 0 {
			t.Fatalf("%s: %v %q", entry.target, launch.Environment, launch.ExcludedEnvironment)
		}
	}

	zai := zaiProfile(t, t.TempDir())
	for _, entry := range launchesOnEveryTarget(t, &zai) {
		launch := entry.launch
		if launch.Environment["ZAI_API_KEY"] != "coding-plan-key" || launch.Environment["CODEX_API_KEY"] != "sk-target" || len(launch.ExcludedEnvironment) != 0 {
			t.Fatalf("%s: %v %q", entry.target, launch.Environment, launch.ExcludedEnvironment)
		}
	}
}

func TestClaudeProviderSwitchesControlKeychainStagingAndOauth(t *testing.T) {
	home := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, filepath.Join(home, "claude"))
	staged := filepath.Join(home, "staged")
	makeFixtureDir(t, staged)
	for _, entry := range []struct {
		name, value string
		usesOAuth   bool
	}{
		{"ANTHROPIC_AUTH_TOKEN", "target-key", false},
		{"ANTHROPIC_API_KEY", "target-key", false},
		{"ANTHROPIC_API_KEY", " ", true},
		{core.ClaudeOauthTokenEnv, " \t ", true},
		{"CLAUDE_CODE_USE_VERTEX", "1", false},
		{"CLAUDE_CODE_USE_FOUNDRY", "1", false},
		{"CLAUDE_CODE_USE_BEDROCK", "false", true},
	} {
		environment := map[string]string{
			core.HarnessKindClaude.HomeEnv(): staged,
			entry.name:                       entry.value,
		}
		keychain := accounts.FakeKeychainHolding([]byte(`{"claudeAiOauth":{"accessToken":"keychain","refreshToken":"keychain-refresh","expiresAt":100}}`))
		if err := StageClaudeKeychainLogin(&profile, core.HarnessHostMacOs, staged, environment, keychain); err != nil {
			t.Fatal(err)
		}
		if _, ok := core.ClaudeOauthLoginPath(environment); ok != entry.usesOAuth {
			t.Fatalf("%s=%s runtime OAuth", entry.name, entry.value)
		}
		if (len(keychain.Calls()) != 0) != entry.usesOAuth {
			t.Fatalf("%s=%s Keychain staging", entry.name, entry.value)
		}
	}
}

// Hard-won: #1385: selecting AWS_PROFILE did not provision credentials into isolated workers.
func TestABedrockCodexLaunchPassesAWSSettingsToEveryTarget(t *testing.T) {
	home := t.TempDir()
	writeFixture(t, filepath.Join(home, "config.toml"), []byte("model = \"global.openai.gpt-6-luna\"\n"+
		"model_provider = \"amazon-bedrock-runtime\"\n"+
		"[model_providers.amazon-bedrock-runtime.aws]\n"+
		"region = \"us-east-1\"\n"))
	awsEnvironment := map[string]string{
		"AWS_REGION":                  "us-east-1",
		"AWS_DEFAULT_REGION":          "us-east-1",
		"AWS_PROFILE":                 "bedrock",
		"AWS_CONFIG_FILE":             "/home/hel/.aws/config",
		"AWS_SHARED_CREDENTIALS_FILE": "/home/hel/.aws/credentials",
		"AWS_EC2_METADATA_DISABLED":   "false",
	}
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	profile.Environment = core.EnvironmentFromMap(awsEnvironment)

	for _, entry := range launchesOnEveryTarget(t, &profile) {
		launch := entry.launch
		for name, value := range awsEnvironment {
			if got, ok := launch.Environment[name]; !ok || got != value {
				t.Fatalf("%s: %s = %q", entry.target, name, got)
			}
		}
		for _, name := range core.CodexCredentialEnvironment {
			if _, ok := launch.Environment[name]; ok {
				t.Fatalf("%s: inherited OpenAI key %s", entry.target, name)
			}
		}
		if !reflect.DeepEqual(launch.ExcludedEnvironment, core.CodexCredentialEnvironment[:]) {
			t.Fatalf("%s: %q", entry.target, launch.ExcludedEnvironment)
		}
	}

	// A profile without its own target credential source must use the
	// controller-provisioned process provider in each isolated staged home.
	for _, kind := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindCodex} {
		profile := profile
		profile.Kind = kind
		profile.Environment = core.EnvironmentFromMap(map[string]string{
			"AWS_PROFILE":             "bedrock",
			"AWS_REGION":              "us-east-1",
			"CLAUDE_CODE_USE_BEDROCK": " On ",
		})
		for _, entry := range launchesOnEveryTarget(t, &profile) {
			launch := entry.launch
			isolated := entry.target != "localhost"
			configFile, ok := launch.Environment["AWS_CONFIG_FILE"]
			if ok != isolated {
				t.Fatalf("%v %s: AWS_CONFIG_FILE %q", kind, entry.target, configFile)
			}
			if isolated && configFile != core.Text(core.Join(launch.HarnessHome, "aws-config")) {
				t.Fatalf("%v %s: AWS_CONFIG_FILE %q", kind, entry.target, configFile)
			}
		}
	}
}

// #1160: a sub-agent child in its parent's container on a remote machine
// runs from a staged home of its own, named after the child, and the launch
// tells its worker which file there holds the login. That file is where a
// credential sync push lands, so a login refreshed by `mj login` reaches the
// child and not its parent's home.
func TestAChildInARemoteContainerTakesItsLoginInItsOwnStagedHome(t *testing.T) {
	profile := codexLoginProfile(t, t.TempDir(), "chatgpt")
	const parentID = "0123456789abcdef0123456789abcdef"
	const childID = "1123456789abcdef0123456789abcdef"
	parentWorkspace, err := core.NewContainerWorkspace(parentID)
	if err != nil {
		t.Fatal(err)
	}
	bundle := store.LocalBundle("/src/project")
	child := store.CheckpointTestSession(childID)
	child.LastProfile = "codex4"
	child.ProjectDirectory = nil
	child.ContainerWorkspace = &parentWorkspace
	template := core.TargetTemplateSshPodman{
		SSH:               core.SshConnection{Host: "morannon", ExtraArgs: []string{}},
		ContainerTemplate: core.ContainerTemplate{Image: "ghcr.io/brokkai/mjolnir/agent-dev:latest"},
	}
	borrowedFrom := parentID
	settings := core.TargetRuntimeSettingsFromTemplate(template)
	config := core.DefaultConfig()
	launch, _, targetHome, err := WorkerLaunchConfig(&child, &profile, &bundle, &core.TargetsTargetLocator{
		Kind:         core.TargetsTargetLocatorSshPodman,
		BorrowedFrom: &borrowedFrom,
		SSH:          &core.SshTarget{Destination: "morannon", SSHArgs: []string{}},
		ContainerID:  strings.Repeat("c", 64),
	}, LaunchWorkspace{SessionID: parentID, Container: &parentWorkspace, WorkingDirectories: []string{}}, &settings, &config)
	if err != nil {
		t.Fatal(err)
	}

	if launch.HarnessHome != targetHome {
		t.Fatalf("%s %s", launch.HarnessHome, targetHome)
	}
	if launch.Environment[core.SessionGitConfigIncludePath] != "/home/hel/.gitconfig" {
		t.Fatal("no git config include")
	}
	if launch.HarnessHome != "/var/lib/hel/profiles/"+childID {
		t.Fatalf("the child's staged home is its own, not its parent's: %s", launch.HarnessHome)
	}
	if launch.Environment["CODEX_HOME"] != targetHome {
		t.Fatal(launch.Environment["CODEX_HOME"])
	}
	if launch.AuthenticationMarker == nil || *launch.AuthenticationMarker != "auth.json" {
		t.Fatalf("%v", launch.AuthenticationMarker)
	}
	if launch.Cwd != fmt.Sprintf("/workspace/%s/project", parentID) {
		t.Fatalf("the child works in its parent's checkout: %s", launch.Cwd)
	}
}

func TestACustomProviderSessionCarriesItsKeyAndRunsFromAPrivateHome(t *testing.T) {
	project := t.TempDir()
	profile := zaiProfile(t, t.TempDir())
	session := store.CheckpointTestSession("session-glm")
	session.HarnessKind = core.HarnessKindCodex
	session.LastProfile = "glm"
	session.TargetTemplateID = "localhost"
	session.ProjectDirectory = &project
	const root = "/home/me/.local/share/hel/workers/session-glm"
	session.Target = core.TargetLocatorLocalBare{WorkerRoot: root}
	settings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})
	config := core.DefaultConfig()

	launch, _, targetHome, err := WorkerLaunchConfig(&session, &profile, nil,
		&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root},
		LaunchWorkspace{SessionID: session.ID, WorkingDirectories: []string{}}, &settings, &config)
	if err != nil {
		t.Fatal(err)
	}

	if launch.Environment["ZAI_API_KEY"] != "coding-plan-key" || launch.Environment["CODEX_HOME"] != targetHome {
		t.Fatalf("%v", launch.Environment)
	}
	if targetHome != root+"/profile" {
		t.Fatalf("the session runs from the staged copy, not the user's profile home: %s", targetHome)
	}
	if launch.AuthenticationMarker == nil || *launch.AuthenticationMarker != "config.toml" {
		t.Fatal("the worker checks the Codex configuration, not a ChatGPT auth file")
	}
	// Guardian still applies: a raw local target keeps configured
	// approvals.
	if launch.ExecutionPolicy != core.ExecutionPolicyConfiguredApprovals {
		t.Fatalf("%v", launch.ExecutionPolicy)
	}
	if launch.Environment["INITIAL_AGENT_MODE"] != "agent" {
		t.Fatal(launch.Environment["INITIAL_AGENT_MODE"])
	}
	if profile.Kind.Guardian() != core.GuardianReviewerHarness {
		t.Fatalf("%v", profile.Kind.Guardian())
	}
}
