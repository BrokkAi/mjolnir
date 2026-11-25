// Port of mj-worker-staging/src/worker_binary/tests.rs, part 3: project
// memory in launches, Codex catalog staging, and profile staging.

package workerstaging

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"sort"
	"strings"
	"sync"
	"testing"

	"github.com/BurntSushi/toml"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

func localBareLaunchWorkspace(sessionID string) LaunchWorkspace {
	return LaunchWorkspace{SessionID: sessionID, WorkingDirectories: []string{}}
}

func TestASessionWithoutProjectMemoryLaunchesWithoutMemory(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1291: local bare fixture; a Windows controller runs no local bare sessions")
	}
	directory := t.TempDir()
	project := filepath.Join(directory, "project")
	makeFixtureDir(t, project)

	for index, kind := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindCodex} {
		sessionID := fmt.Sprintf("%032x", index+1)
		workerRoot := filepath.Join(directory, "workers", sessionID)
		backend := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}
		profile := core.NewHarnessProfile(kind, filepath.Join(directory, kind.ID()+"-home"))
		config := core.DefaultConfig()
		settings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})
		launch := func(noProjectMemory bool) (core.WorkerLaunchConfig, *core.ProjectMemoryLaunchConfig) {
			t.Helper()
			session := store.CheckpointTestSession(sessionID)
			session.HarnessKind = kind
			session.LastProfile = kind.ID()
			session.TargetTemplateID = "localhost"
			session.ProjectDirectory = &project
			session.NoProjectMemory = noProjectMemory
			config, memory, _, err := WorkerLaunchConfig(&session, &profile, nil, &backend, localBareLaunchWorkspace(sessionID), &settings, &config)
			if err != nil {
				t.Fatal(err)
			}
			return config, memory
		}

		ordinary, ordinaryMemory := launch(false)
		if ordinary.ProjectMemory == nil || !reflect.DeepEqual(ordinary.ProjectMemory, ordinaryMemory) {
			t.Fatalf("%v: %v %v", kind, ordinary.ProjectMemory, ordinaryMemory)
		}
		if _, ok := ordinary.Environment[ClaudeDisableAutoMemoryEnv]; ok {
			t.Fatalf("%v disabled auto-memory", kind)
		}

		optedOut, optedOutMemory := launch(true)
		if optedOut.ProjectMemory != nil || optedOutMemory != nil {
			t.Fatalf("%v launched with memory", kind)
		}
		// Claude's own auto-memory is switched off, and the project's
		// identity still names its transcript directory and tool cache.
		value, ok := optedOut.Environment[ClaudeDisableAutoMemoryEnv]
		if kind == core.HarnessKindClaude {
			if !ok || value != "1" {
				t.Fatalf("%v: %q %v", kind, value, ok)
			}
		} else if ok {
			t.Fatalf("%v: %q", kind, value)
		}
		if optedOut.Environment["CLAUDE_CODE_PROJECT_DIR_NAME"] != ordinary.Environment["CLAUDE_CODE_PROJECT_DIR_NAME"] {
			t.Fatalf("%v changed the project directory name", kind)
		}
	}
}

func TestProjectMemoryReplicaIsSeparateFromControllerAttachmentDirectories(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1291: local bare fixture; a Windows controller runs no local bare sessions")
	}
	directory := t.TempDir()
	project := filepath.Join(directory, "project")
	makeFixtureDir(t, project)

	for index, kind := range core.HarnessKindAll {
		sessionID := fmt.Sprintf("%032x", index+1)
		workerRoot := filepath.Join(directory, "workers", sessionID)
		backend := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}
		profile := core.NewHarnessProfile(kind, filepath.Join(directory, kind.ID()+"-home"))
		session := store.CheckpointTestSession(sessionID)
		session.HarnessKind = kind
		session.LastProfile = kind.ID()
		session.TargetTemplateID = "localhost"
		session.ProjectDirectory = &project
		session.Target = core.TargetLocatorLocalBare{WorkerRoot: workerRoot}
		config := core.DefaultConfig()
		settings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})

		launch, memory, _, err := WorkerLaunchConfig(&session, &profile, nil, &backend, localBareLaunchWorkspace(sessionID), &settings, &config)
		if err != nil {
			t.Fatal(err)
		}
		if memory == nil {
			t.Fatalf("%v: a session without the opt-out has project memory", kind)
		}
		if slices.Contains(launch.AdditionalDirectories, memory.Root) {
			t.Fatalf("%v keeps the target-local memory replica out of attachment directories", kind)
		}
		if launch.ProjectMemory == nil || launch.ProjectMemory.Root != memory.Root {
			t.Fatalf("%v retains its memory replica in the dedicated launch field", kind)
		}
	}
}

// isolatedCatalogCache is a catalog cache of the test's own, so the
// fallback path never touches the daemon's shared one.
type isolatedCatalogCache struct {
	mu      sync.Mutex
	entries map[[2]string]string
}

func newIsolatedCatalogCache() *isolatedCatalogCache {
	return &isolatedCatalogCache{entries: map[[2]string]string{}}
}

func (c *isolatedCatalogCache) Load(profileID, key string) (string, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	body, ok := c.entries[[2]string{profileID, key}]
	return body, ok
}

func (c *isolatedCatalogCache) Store(profileID, key, body string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.entries[[2]string{profileID, key}] = body
}

const zaiConfig = "model = \"glm-5.3\"\n" +
	"model_provider = \"zai\"\n" +
	"\n" +
	"[model_providers.zai]\n" +
	"base_url = \"https://api.z.ai/api/v1\"\n" +
	"env_key = \"ZAI_API_KEY\"\n" +
	"wire_api = \"responses\"\n"

const zaiCatalog = `{"models":[
    {"slug":"glm-5.3","supported_reasoning_levels":["low","high","max"]},
    {"slug":"glm-5.3-flash","supported_reasoning_levels":["low","high","max"]}
]}`

func zaiProfile(t *testing.T, home string) core.HarnessProfile {
	t.Helper()
	writeFixture(t, filepath.Join(home, "config.toml"), []byte(zaiConfig))
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	profile.Environment = core.EnvironmentFromMap(map[string]string{"ZAI_API_KEY": "coding-plan-key"})
	return profile
}

func readStagedCatalog(t *testing.T, staged string) core.CodexCatalog {
	t.Helper()
	contents, err := os.ReadFile(filepath.Join(staged, "models.json"))
	if err != nil {
		t.Fatal(err)
	}
	catalog, err := core.CodexCatalogParse(contents)
	if err != nil {
		t.Fatal(err)
	}
	return catalog
}

// `muse serve` never fetches its model list; it reads the list saved under
// `$XDG_DATA_HOME/muse/model-catalog` and otherwise offers one built-in
// model. The staged copy must land where the session's Muse looks.
// Hard-won: c80d1f80: Muse sessions offered only Muse's one built-in model.
func TestStagingAMuseProfileHandsItsSavedModelListToTheSessionDataDirectory(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "muse")
	saved := filepath.Join(home, ".data/muse/model-catalog/6d657461__p746268.json")
	writeFixture(t, saved, []byte(`{"schema_version":2,"rows":[]}`))
	profile := core.NewHarnessProfile(core.HarnessKindMuse, home)
	stagedHome := filepath.Join(t.TempDir(), "muse")

	if err := StageProfile(&profile, stagedHome); err != nil {
		t.Fatal(err)
	}

	environment := map[string]string{}
	core.HarnessKindMuse.ConfigureHomeEnvironment(stagedHome, environment)
	if readFixture(t, filepath.Join(environment["XDG_DATA_HOME"], "muse/model-catalog/6d657461__p746268.json")) != readFixture(t, saved) {
		t.Fatal("the saved model list did not reach the session data directory")
	}
}

// Hard-won: #1320: a reviewer staged its profile without the provider's model catalog and offered OpenAI's model names
func TestStagingACustomProviderProfileWritesACatalogTheSessionCanPickFrom(t *testing.T) {
	home := t.TempDir()
	staged := t.TempDir()
	profile := zaiProfile(t, home)
	cache := newIsolatedCatalogCache()
	var asked [][2]string

	// The one staging call every session, reviewer and probe makes.
	if err := StageProfileWithCatalog("glm", &profile, staged, func(url, key string) ([]byte, error) {
		asked = append(asked, [2]string{url, key})
		return []byte(zaiCatalog), nil
	}, cache); err != nil {
		t.Fatal(err)
	}

	if !reflect.DeepEqual(asked, [][2]string{{"https://api.z.ai/api/v1/models", "coding-plan-key"}}) {
		t.Fatalf("the provider's own key authorizes its catalog fetch: %q", asked)
	}
	catalog := readStagedCatalog(t, staged)
	if !reflect.DeepEqual(catalog.Slugs(), []string{"glm-5.3", "glm-5.3-flash"}) {
		t.Fatalf("%q", catalog.Slugs())
	}
	for _, model := range catalog.Models {
		if model["auto_review_model_override"] != "glm-5.3-flash" {
			t.Fatalf("Guardian reviews run on the newest flash model: %v", model)
		}
	}
	// The key is top-level, so Codex reads it, and the user's own lines
	// survive.
	config := readFixture(t, filepath.Join(staged, "config.toml"))
	if strings.Count(config, "model_catalog_json") != 1 {
		t.Fatalf("exactly one top-level key names the catalog: %s", config)
	}
	if !strings.Contains(config, "model_catalog_json = \"models.json\"") {
		t.Fatal(config)
	}
	for _, line := range strings.Split(zaiConfig, "\n") {
		if line != "" && !strings.Contains(config, line) {
			t.Fatalf("missing %q in %s", line, config)
		}
	}
	provider, err := core.ReadCodexProvider(staged, nil)
	if err != nil {
		t.Fatal(err)
	}
	if provider == nil || provider.ModelCatalogJSON == nil || *provider.ModelCatalogJSON != "models.json" {
		t.Fatal("Codex reads the staged catalog as a top-level key")
	}
	// The staged copy is what the session runs from, on every target.
	if fixtureExists(filepath.Join(home, "models.json")) {
		t.Fatal("the user's own profile home stays untouched")
	}
}

// `codex app-server` refuses `--profile`, so the overlay must already be
// layered into the staged `config.toml`, deep-merged as `codex -p` merges
// it, and the overlay's provider must be the one the catalog is fetched
// from. Codex silently runs without a missing overlay; Mjolnir refuses
// instead.
func TestStagingACodexProfileOverlayLayersItOverTheBaseConfiguration(t *testing.T) {
	home := t.TempDir()
	staged := t.TempDir()
	profile := zaiProfile(t, home)
	overlay := "ds"
	profile.CodexProfile = &overlay
	base := "model = \"gpt-5.5\"\n\n[mcp_servers.base]\ncommand = \"base-server\"\n\n[tui]\nnotifications = true\n"
	writeFixture(t, filepath.Join(home, "config.toml"), []byte(base))
	writeFixture(t, filepath.Join(home, "ds.config.toml"), []byte(zaiConfig+"\n[mcp_servers.overlay]\ncommand = \"overlay-server\"\n"))
	cache := newIsolatedCatalogCache()
	var asked []string

	if err := profile.EnsureReady("codex-ds"); err != nil {
		t.Fatal(err)
	}
	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := StageCodexCatalog("codex-ds", &profile, staged, func(url, _ string) ([]byte, error) {
		asked = append(asked, url)
		return []byte(zaiCatalog), nil
	}, cache); err != nil {
		t.Fatal(err)
	}

	if !reflect.DeepEqual(asked, []string{"https://api.z.ai/api/v1/models"}) {
		t.Fatalf("%q", asked)
	}
	var config map[string]any
	if _, err := toml.Decode(readFixture(t, filepath.Join(staged, "config.toml")), &config); err != nil {
		t.Fatal(err)
	}
	if config["model"] != "glm-5.3" || config["model_provider"] != "zai" || config["model_catalog_json"] != "models.json" {
		t.Fatalf("%v", config)
	}
	servers, ok := config["mcp_servers"].(map[string]any)
	if !ok {
		t.Fatalf("%v", config)
	}
	names := make([]string, 0, len(servers))
	for name := range servers {
		names = append(names, name)
	}
	sort.Strings(names)
	if !reflect.DeepEqual(names, []string{"base", "overlay"}) {
		t.Fatalf("tables merge key by key: %v", config)
	}
	if tui, ok := config["tui"].(map[string]any); !ok || tui["notifications"] != true {
		t.Fatalf("%v", config)
	}
	if readFixture(t, filepath.Join(home, "config.toml")) != base {
		t.Fatal("the user's own configuration stays untouched")
	}

	missing := "missing"
	profile.CodexProfile = &missing
	err := profile.EnsureReady("codex-missing")
	requireErrorContains(t, err, "missing.config.toml")
	if err := StageProfile(&profile, t.TempDir()); err == nil {
		t.Fatal("a missing overlay was staged")
	}
}

// Hard-won: ee59e57: a valid profile catalog key caused validation failure and a duplicate staged key
func TestAProfileAuthoredCatalogIsDiscardedAndTheStagedKeyReplacesItsOwn(t *testing.T) {
	home := t.TempDir()
	staged := t.TempDir()
	profile := deepseekProfile(t, home)
	// A catalog of the profile's own, named the way Codex names one.
	writeFixture(t, filepath.Join(home, "mine.models.json"), []byte(`{"models":[
            {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high","max"]},
            {"slug":"deepseek-private","display_name":"DeepSeek Private"}
        ]}`))
	authored := "model_catalog_json = \"mine.models.json\"\n" + readFixture(t, filepath.Join(home, "config.toml"))
	writeFixture(t, filepath.Join(home, "config.toml"), []byte(authored))

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := StageCodexCatalog("deepseek", &profile, staged, func(string, string) ([]byte, error) {
		return []byte(deepseekList), nil
	}, newIsolatedCatalogCache()); err != nil {
		t.Fatalf("a profile-authored catalog is merged, not refused: %v", err)
	}

	catalog := readStagedCatalog(t, staged)
	if !reflect.DeepEqual(catalog.Slugs(), []string{"deepseek-flash", "deepseek-v4-pro"}) {
		t.Fatalf("the file named by model_catalog_json is ignored; only the provider's list and the profile's models.json shape the staged catalog: %q", catalog.Slugs())
	}
	config := readFixture(t, filepath.Join(staged, "config.toml"))
	if strings.Count(config, "model_catalog_json") != 1 {
		t.Fatalf("the staged copy has one key, not a duplicate Codex would reject: %s", config)
	}
	if !strings.Contains(config, "model_catalog_json = \"models.json\"") {
		t.Fatal(config)
	}
	if strings.Contains(config, "mine.models.json") {
		t.Fatalf("no staged key points into the profile's own home: %s", config)
	}
	if readFixture(t, filepath.Join(home, "config.toml")) != authored {
		t.Fatal("the profile's own file is never modified")
	}
}

func TestAFailedCatalogFetchFallsBackToTheLastCachedCatalog(t *testing.T) {
	home := t.TempDir()
	staged := t.TempDir()
	profile := zaiProfile(t, home)
	cache := newIsolatedCatalogCache()

	if err := StageCodexCatalog("glm", &profile, staged, func(string, string) ([]byte, error) {
		return []byte(zaiCatalog), nil
	}, cache); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(filepath.Join(staged, "models.json")); err != nil {
		t.Fatal(err)
	}

	unreachable := func(string, string) ([]byte, error) { return nil, errors.New("the provider is unreachable") }
	if err := StageCodexCatalog("glm", &profile, staged, unreachable, cache); err != nil {
		t.Fatalf("a provider outage must not block a launch: %v", err)
	}
	fallback := readStagedCatalog(t, staged)
	if slugs := fallback.Slugs(); !reflect.DeepEqual(slugs, []string{"glm-5.3", "glm-5.3-flash"}) {
		t.Fatalf("%q", slugs)
	}

	// With nothing cached for a different provider, the launch fails and
	// says which profile and URL could not be reached.
	err := StageCodexCatalog("glm", &profile, staged, unreachable, newIsolatedCatalogCache())
	requireErrorContains(t, err, "glm", "https://api.z.ai/api/v1/models")
}

// Hard-won: 55b8ea7: provider catalog and discovered model rows evicted each other from SQLite
const deepseekConfig = "model = \"deepseek-v4-pro\"\n" +
	"model_provider = \"deepseek\"\n" +
	"\n" +
	"[model_providers.deepseek]\n" +
	"base_url = \"https://api.deepseek.com/v1\"\n" +
	"env_key = \"DEEPSEEK_API_KEY\"\n" +
	"wire_api = \"responses\"\n"

const deepseekList = `{"object":"list","data":[
    {"id":"deepseek-flash","object":"model","owned_by":"deepseek"},
    {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
]}`

func deepseekProfile(t *testing.T, home string) core.HarnessProfile {
	t.Helper()
	writeFixture(t, filepath.Join(home, "config.toml"), []byte(deepseekConfig))
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	profile.Environment = core.EnvironmentFromMap(map[string]string{"DEEPSEEK_API_KEY": "deepseek-key"})
	return profile
}

func stageCatalogFor(t *testing.T, profile *core.HarnessProfile, body, staged string) (core.CodexCatalog, error) {
	t.Helper()
	if err := StageCodexCatalog("deepseek", profile, staged, func(string, string) ([]byte, error) {
		return []byte(body), nil
	}, newIsolatedCatalogCache()); err != nil {
		return core.CodexCatalog{}, err
	}
	return core.CodexCatalogParse([]byte(readFixture(t, filepath.Join(staged, "models.json"))))
}

func jsonEquals(t *testing.T, value any, expected string) bool {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var left, right any
	if err := json.Unmarshal(encoded, &left); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal([]byte(expected), &right); err != nil {
		t.Fatal(err)
	}
	return reflect.DeepEqual(left, right)
}

func TestAPlainModelListBecomesACatalogTheProfilesOverridesRefine(t *testing.T) {
	home := t.TempDir()
	staged := t.TempDir()
	profile := deepseekProfile(t, home)
	writeFixture(t, filepath.Join(home, "models.json"), []byte(`{"models":[
            {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high"]},
            {"slug":"deepseek-preview","display_name":"DeepSeek Preview"}
        ]}`))

	catalog, err := stageCatalogFor(t, &profile, deepseekList, staged)
	if err != nil {
		t.Fatalf("an OpenAI-format model list stages a catalog: %v", err)
	}

	if !reflect.DeepEqual(catalog.Slugs(), []string{"deepseek-flash", "deepseek-v4-pro", "deepseek-preview"}) {
		t.Fatalf("the override adds a model the provider's list omits: %q", catalog.Slugs())
	}
	if !jsonEquals(t, catalog.Models[1]["supported_reasoning_levels"], `["low","high"]`) {
		t.Fatalf("the override gives the translated entry its reasoning levels: %v", catalog.Models[1])
	}
	if catalog.Models[0]["auto_review_model_override"] != "deepseek-flash" {
		t.Fatalf("the newest flash model reviews by default: %v", catalog.Models[0])
	}
}

// Hard-won: #1267: an inline `experimental_bearer_token` provider was skipped
// for catalog staging, so no models.json and no guardian reviewer were staged.
func TestAnInlineTokenProviderStagesItsCatalogAndGuardianReviewer(t *testing.T) {
	home := t.TempDir()
	staged := t.TempDir()
	writeFixture(t, filepath.Join(home, "config.toml"), []byte("model = \"deepseek-flash\"\n"+
		"model_provider = \"deepseek\"\n"+
		"\n"+
		"[model_providers.deepseek]\n"+
		"base_url = \"https://api.deepseek.com/v1\"\n"+
		"experimental_bearer_token = \"inline-deepseek-key\"\n"+
		"wire_api = \"responses\"\n"))
	// No `env_key` and an empty environment: the key is only in
	// `config.toml`.
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	var asked [][2]string

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := StageCodexCatalog("deepseek-inline", &profile, staged, func(url, key string) ([]byte, error) {
		asked = append(asked, [2]string{url, key})
		return []byte(deepseekList), nil
	}, newIsolatedCatalogCache()); err != nil {
		t.Fatal(err)
	}

	if !reflect.DeepEqual(asked, [][2]string{{"https://api.deepseek.com/v1/models", "inline-deepseek-key"}}) {
		t.Fatalf("the provider's own catalog is fetched with the inline key: %q", asked)
	}
	catalog := readStagedCatalog(t, staged)
	if catalog.Models[0]["auto_review_model_override"] != "deepseek-flash" {
		t.Fatal("the guardian reviewer is stamped for an inline-token profile too")
	}
	stagedConfig := readFixture(t, filepath.Join(staged, "config.toml"))
	if !strings.Contains(stagedConfig, "model_catalog_json = \"models.json\"") {
		t.Fatalf("the staged configuration points at the staged catalog: %s", stagedConfig)
	}
	if !strings.Contains(stagedConfig, "experimental_bearer_token = \"inline-deepseek-key\"") {
		t.Fatalf("the staged home still carries the key the session authenticates with: %s", stagedConfig)
	}
}

func TestStageGrokProfileCopiesAuthenticationAndAgentIdentity(t *testing.T) {
	home := t.TempDir()
	writeFixture(t, filepath.Join(home, "auth.json"), []byte("{\"https://auth.x.ai::1\":{}}"))
	writeFixture(t, filepath.Join(home, "agent_id"), []byte("stable-agent-id"))
	writeFixture(t, filepath.Join(home, "config.toml"), []byte("model = \"grok-4.6\"\n"))
	// Native session storage is checkpointed, never staged.
	writeFixture(t, filepath.Join(home, "sessions/session_search.sqlite"), []byte("x"))
	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindGrok, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}

	if readFixture(t, filepath.Join(staged, "agent_id")) != "stable-agent-id" {
		t.Fatal("the agent identity changed")
	}
	if !isFile(filepath.Join(staged, "auth.json")) || !isFile(filepath.Join(staged, "config.toml")) {
		t.Fatal("authentication or configuration was not staged")
	}
	if fixtureExists(filepath.Join(staged, "sessions")) {
		t.Fatal("native session storage was staged")
	}
}

func TestStageClaudeProfilePreservesRolloutIdentity(t *testing.T) {
	home := t.TempDir()
	identity := `{
            "machineID": "stable-machine",
            "userID": "stable-user",
            "cachedGrowthBookFeatures": {
                "tengu_velvet_mallet_fable_5": true
            }
        }`
	writeFixture(t, filepath.Join(home, ".claude.json"), []byte(identity))
	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}

	if readFixture(t, filepath.Join(staged, ".claude.json")) != identity {
		t.Fatal("the rollout identity changed")
	}
}

func TestStagingReproducesTheSkillsTreeTheSyncWillPush(t *testing.T) {
	// The launch stage and the credential sync must agree byte for byte:
	// the first reconciliation replaces the whole tree, so a stage the sync
	// does not reproduce would have its managed skills wiped a minute later.
	home := t.TempDir()
	writeFixture(t, filepath.Join(home, "skills/review/SKILL.md"), []byte("review skill\n"))
	writeFixture(t, filepath.Join(home, "skills/mj/SKILL.md"), []byte("the user's own\n"))

	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := StageManagedSkills(profile.Kind, staged, core.SkillsScopeLocalhost); err != nil {
		t.Fatal(err)
	}

	expected, err := core.SessionSkills(profile.Kind, home, core.SkillsArchiveFormatGzip, core.SkillsScopeLocalhost)
	if err != nil {
		t.Fatal(err)
	}
	installed, err := core.CollectSkills(profile.Kind, staged)
	if err != nil {
		t.Fatal(err)
	}
	if installed.Fingerprint() != expected.Fingerprint() || !reflect.DeepEqual(installed, expected) {
		t.Fatal("the staged skills differ from what the sync pushes")
	}
	if readFixture(t, filepath.Join(staged, "skills/review/SKILL.md")) != "review skill\n" {
		t.Fatal("the user's skill changed")
	}
	if readFixture(t, filepath.Join(staged, "skills/mj/SKILL.md")) == "the user's own\n" {
		t.Fatal("the managed mj skill kept the user's copy")
	}
}
