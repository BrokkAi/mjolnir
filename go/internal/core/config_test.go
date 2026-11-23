// Port of mj-core/src/config/tests.rs.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"sort"
	"strings"
	"testing"
	"time"
)

func configTestPtr[T any](value T) *T { return &value }

// configTestRead reads text as toml::from_str::<Config> does, with fixed
// process and secret values.
func configTestRead(t *testing.T, text string, process, secrets map[string]string) Config {
	t.Helper()
	type read struct {
		config Config
		err    error
	}
	result := WithSecretResolver(SecretResolverFixed(process, secrets), func() read {
		var config Config
		err := unmarshalTOML(text, &config)
		return read{config, err}
	})
	if result.err != nil {
		t.Fatalf("read the configuration: %v", result.err)
	}
	return result.config
}

func configTestFromTOML(t *testing.T, text string) Config {
	t.Helper()
	var config Config
	if err := unmarshalTOML(text, &config); err != nil {
		t.Fatalf("read the configuration: %v", err)
	}
	return config
}

func configTestLoad(t *testing.T, path string) Config {
	t.Helper()
	config, err := ConfigLoadFrom(path)
	if err != nil {
		t.Fatalf("load %s: %v", path, err)
	}
	return config
}

func configTestLoadError(t *testing.T, path string) string {
	t.Helper()
	_, err := ConfigLoadFrom(path)
	if err == nil {
		t.Fatalf("load %s succeeded", path)
	}
	return err.Error()
}

func configTestSave(t *testing.T, config Config, path string) {
	t.Helper()
	if err := config.SaveTo(path); err != nil {
		t.Fatalf("save %s: %v", path, err)
	}
}

func configTestReadFile(t *testing.T, path string) string {
	t.Helper()
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(body)
}

func configTestWrite(t *testing.T, path, body string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
}

func configTestTOML(t *testing.T, value any) string {
	t.Helper()
	text, err := marshalTOML(value)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	return text
}

// configTestIdentity is everything Rust's derived PartialEq compares on a
// Config: its serialized form, and what that form leaves out.
func configTestIdentity(t *testing.T, config Config) string {
	t.Helper()
	data, err := serdeJSON(config)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	environments := map[string]map[string]string{}
	for id, profile := range config.Profiles {
		resolved := map[string]string{}
		for key, value := range profile.Environment.Resolved() {
			resolved[key] = value
		}
		environments[id] = resolved
	}
	extra, err := serdeJSON(map[string]any{
		"version":      config.Version,
		"disabled":     sortedKeys(config.DisabledTargets),
		"defaults":     sortedKeys(config.DefaultTargets),
		"environments": environments,
		"review":       config.Review.Disabled,
	})
	if err != nil {
		t.Fatal(err)
	}
	return string(data) + string(extra)
}

func configTestAssertEqual(t *testing.T, got, want Config) {
	t.Helper()
	if left, right := configTestIdentity(t, got), configTestIdentity(t, want); left != right {
		t.Fatalf("configurations differ:\n got %s\nwant %s", left, right)
	}
}

func configTestContains(t *testing.T, text, want string) {
	t.Helper()
	if !strings.Contains(text, want) {
		t.Fatalf("%q does not contain %q", text, want)
	}
}

func configTestLacks(t *testing.T, text, unwanted string) {
	t.Helper()
	if strings.Contains(text, unwanted) {
		t.Fatalf("%q contains %q", text, unwanted)
	}
}

func configTestValidateError(t *testing.T, config Config) string {
	t.Helper()
	err := config.Validate()
	if err == nil {
		t.Fatal("validation succeeded")
	}
	return err.Error()
}

// R4-6: `[targets.x] kind = "podman"` without `image` stopped the daemon from
// starting ("missing field `image`"), and `extra_run_args`, which is not a
// target setting, was accepted without a word.
// Hard-won: 412f0adc: a missing container image stopped daemon startup and unknown config keys vanished silently
func TestAContainerTargetWithoutAnImageUsesTheDefaultAndNamesUnknownKeys(t *testing.T) {
	for _, kind := range []string{"podman", "docker"} {
		config := configTestFromTOML(t, fmt.Sprintf(
			"version = %d\n[targets.r4-%s]\nkind = \"%s\"\nextra_run_args = [\"--network=host\"]\n", ConfigVersion, kind, kind))
		var container ContainerTemplate
		switch target := config.Targets["r4-"+kind].(type) {
		case TargetTemplateLocalPodman:
			container = target.ContainerTemplate
		case TargetTemplateLocalDocker:
			container = target.ContainerTemplate
		default:
			t.Fatalf("%s changed kind", kind)
		}
		if container.Image != DefaultSessionImage {
			t.Fatalf("%s image %q", kind, container.Image)
		}
	}

	table := map[string]any{
		"kind": "podman", "image": "a:1", "machine": "local", "pull_policy": "never",
		"platform": "linux/amd64", "cpus": "2", "memory": "4g", "environment": map[string]any{},
		"workspace_storage": map[string]any{"kind": "container-layer"}, "extra_run_args": []any{"--network=host"},
	}
	if got := newlyUnknownTargetKeys("r4-unknown-keys", "podman", table); !slices.Equal(got, []string{"extra_run_args"}) {
		t.Fatalf("unknown keys %v", got)
	}
	if got := newlyUnknownTargetKeys("r4-unknown-keys", "podman", table); len(got) != 0 {
		t.Fatalf("each unknown key is reported once: %v", got)
	}
	bare := map[string]any{"kind": "bare", "machine": "box", "permissions": "guardian"}
	if got := newlyUnknownTargetKeys("r4-bare", "bare", bare); len(got) != 0 {
		t.Fatalf("bare unknown keys %v", got)
	}

	// The list of known keys is every setting a container table can carry.
	everySetting := ContainerTemplate{
		Image:            "a:1",
		PullPolicy:       ImagePullPolicyNever,
		Platform:         configTestPtr("linux/amd64"),
		CPUs:             configTestPtr("2"),
		Memory:           configTestPtr("4g"),
		Environment:      EnvironmentFromMap(map[string]string{"A": "1"}),
		WorkspaceStorage: PodmanWorkspaceStorageContainerLayer{},
		BuildCache:       &TargetBuildCache{Enabled: configTestPtr(true)},
	}
	data, err := serdeJSON(everySetting)
	if err != nil {
		t.Fatal(err)
	}
	fields, err := decodeObject(data)
	if err != nil {
		t.Fatal(err)
	}
	keys := sortedKeys(fields)
	known := append([]string{}, containerTemplateKeys[:]...)
	sort.Strings(known)
	if !slices.Equal(keys, known) {
		t.Fatalf("keys %v, known %v", keys, known)
	}
}

// A machine whose file never named a workspace directory keeps using the
// directory its workspaces are already in, under the former product name;
// new machines get Mjolnir's own. Launch campaign finding C-17.
func TestAMachineWithoutAWorkspaceDirectoryKeepsTheExistingOne(t *testing.T) {
	config := configTestFromTOML(t, fmt.Sprintf("version = %d\n[machines.box]\nkind = \"ssh\"\nhost = \"box\"\n", ConfigVersion))
	machine, ok := config.Machines["box"].(MachineSsh)
	if !ok {
		t.Fatal("an SSH machine")
	}
	if !samePath(machine.WorkspacePrefix, LegacyWorkspacePrefix) {
		t.Fatalf("workspace prefix %q", machine.WorkspacePrefix)
	}
	if LegacyWorkspacePrefix != ".local/share/hel/workspaces" || DefaultWorkspacePrefix != ".local/share/mjolnir/workspaces" {
		t.Fatal("workspace prefixes changed")
	}
}

func configTestZaiProfile(t *testing.T, home string, environment map[string]string) HarnessProfile {
	t.Helper()
	configTestWrite(t, filepath.Join(home, "config.toml"),
		"model = \"glm-5.3\"\n"+
			"model_provider = \"zai\"\n"+
			"[model_providers.zai]\n"+
			"base_url = \"https://api.z.ai/api/v1\"\n"+
			"env_key = \"ZAI_API_KEY\"\n"+
			"wire_api = \"responses\"\n")
	profile := NewHarnessProfile(HarnessKindCodex, home)
	profile.Environment = EnvironmentFromMap(environment)
	return profile
}

// #1160: a Codex profile that signs in with ChatGPT must never be able to use
// an API key. It loses every variable Codex would take a credential or an
// address from; a profile that uses an API key keeps them.
// Hard-won: c4e2838d: a leaked API key broke ChatGPT Codex turns
func TestCodexLaunchProtectsChatgptLoginsAndAllowsKeysForFreshProfiles(t *testing.T) {
	environment := func() map[string]string {
		return map[string]string{
			"OPENAI_API_KEY":     "sk-svcacct-test",
			"CODEX_API_KEY":      "sk-test",
			"CODEX_ACCESS_TOKEN": "token",
			"OPENAI_BASE_URL":    "https://example.invalid/v1",
			"RUST_LOG":           "info",
		}
	}
	everyName := CodexCredentialEnvironment[:]
	onlyRustLog := func(launched map[string]string) {
		t.Helper()
		if !slices.Equal(sortedKeys(launched), []string{"RUST_LOG"}) {
			t.Fatalf("launched %v", launched)
		}
	}
	untouched := func(launched map[string]string) {
		t.Helper()
		if !reflect.DeepEqual(launched, environment()) {
			t.Fatalf("launched %v", launched)
		}
	}

	// A ChatGPT login, as `codex login` writes it.
	chatgptHome := t.TempDir()
	configTestWrite(t, filepath.Join(chatgptHome, "auth.json"), `{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"access_token":"a"}}`)
	chatgpt := NewHarnessProfile(HarnessKindCodex, chatgptHome)
	if login, ok := chatgpt.CodexLogin(); !ok || login != CodexLoginChatGpt {
		t.Fatal("a ChatGPT login")
	}
	launched := environment()
	if got := chatgpt.ExcludeHarnessEnvironment(launched); !slices.Equal(got, everyName) {
		t.Fatalf("excluded %v", got)
	}
	onlyRustLog(launched)

	// A profile without a login file can receive keys from the target.
	fresh := NewHarnessProfile(HarnessKindCodex, t.TempDir())
	if _, ok := fresh.CodexLogin(); ok {
		t.Fatal("a fresh profile has no Codex login")
	}
	launched = environment()
	if got := fresh.ExcludeHarnessEnvironment(launched); len(got) != 0 {
		t.Fatalf("excluded %v", got)
	}
	untouched(launched)

	// `codex login --with-api-key` stores the key and says so.
	apiKeyHome := t.TempDir()
	configTestWrite(t, filepath.Join(apiKeyHome, "auth.json"), `{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}`)
	apiKey := NewHarnessProfile(HarnessKindCodex, apiKeyHome)
	if login, ok := apiKey.CodexLogin(); !ok || login != CodexLoginApiKey {
		t.Fatal("an API-key login")
	}
	launched = environment()
	if got := apiKey.ExcludeHarnessEnvironment(launched); len(got) != 0 {
		t.Fatalf("excluded %v", got)
	}
	untouched(launched)

	// A custom provider authenticates however its configuration says.
	provider := configTestZaiProfile(t, t.TempDir(), map[string]string{})
	if _, ok := provider.CodexLogin(); ok {
		t.Fatal("a custom provider has no Codex login")
	}
	launched = environment()
	if got := provider.ExcludeHarnessEnvironment(launched); len(got) != 0 {
		t.Fatalf("excluded %v", got)
	}
	untouched(launched)

	// Other harnesses do not read these variables for their own login.
	claude := NewHarnessProfile(HarnessKindClaude, t.TempDir())
	if _, ok := claude.CodexLogin(); ok {
		t.Fatal("Claude has no Codex login")
	}
	launched = environment()
	if got := claude.ExcludeHarnessEnvironment(launched); len(got) != 0 {
		t.Fatalf("excluded %v", got)
	}
	untouched(launched)
}

func TestAnAPIKeyCodexProfileNeedsItsKeyInTheProfileEnvironment(t *testing.T) {
	home := t.TempDir()
	withoutKey := configTestZaiProfile(t, home, map[string]string{})
	if err := withoutKey.validate("glm"); err != nil {
		t.Fatalf("the key lives outside Mjolnir's file, so the configuration is valid: %v", err)
	}
	err := withoutKey.EnsureReady("glm")
	if err == nil {
		t.Fatal("a missing key leaves the profile unusable")
	}
	configTestContains(t, err.Error(), "ZAI_API_KEY = { from_secret")
	configTestContains(t, err.Error(), "[profiles.glm.environment]")

	withKey := configTestZaiProfile(t, home, map[string]string{"ZAI_API_KEY": "secret"})
	if err := withKey.validate("glm"); err != nil {
		t.Fatalf("a configured key validates: %v", err)
	}
	if err := withKey.EnsureReady("glm"); err != nil {
		t.Fatalf("a configured key is ready: %v", err)
	}
	if scheme := withKey.AuthScheme(); scheme != (AuthSchemeApiKey{EnvKey: "ZAI_API_KEY"}) {
		t.Fatalf("auth scheme %#v", scheme)
	}
	if key, ok := withKey.CodexProviderAPIKey(); !ok || key != "secret" {
		t.Fatalf("provider key %q", key)
	}
	if marker := withKey.AuthenticationMarker(); marker != filepath.Join(home, "config.toml") {
		t.Fatalf("the Codex configuration proves an API-key profile is set up: %s", marker)
	}
	if _, ok := withKey.CredentialFreshness([]byte("{}")); ok {
		t.Fatal("an API key orders no copies")
	}
	if _, ok := withKey.CredentialExpiry([]byte("{}")); ok {
		t.Fatal("an API key has no expiry")
	}
}

func TestABedrockCodexProfileUsesTheAWSChainWithoutAnAPIKeyOrLoginFile(t *testing.T) {
	home := t.TempDir()
	configTestWrite(t, filepath.Join(home, "config.toml"),
		"model = \"global.openai.gpt-6-luna\"\n"+
			"model_provider = \"amazon-bedrock-runtime\"\n"+
			"[model_providers.amazon-bedrock-runtime.aws]\n"+
			"region = \"us-east-1\"\n")
	profile := NewHarnessProfile(HarnessKindCodex, home)
	profile.Environment = EnvironmentFromMap(map[string]string{})

	if err := profile.EnsureReady("bedrock"); err != nil {
		t.Fatalf("the AWS chain does not need a profile API key: %v", err)
	}
	if scheme := profile.AuthScheme(); scheme != (AuthSchemeAwsCredentialChain{}) {
		t.Fatalf("auth scheme %#v", scheme)
	}
	if profile.AuthScheme().UsesNativeLoginFile() {
		t.Fatal("no login file")
	}
	if marker := profile.AuthenticationMarker(); marker != filepath.Join(home, "config.toml") {
		t.Fatalf("marker %s", marker)
	}
	if _, ok := profile.CredentialFreshness([]byte("{}")); ok {
		t.Fatal("no freshness")
	}
	if _, ok := profile.CredentialExpiry([]byte("{}")); ok {
		t.Fatal("no expiry")
	}
	if _, _, err := LoginCommand(&profile); err == nil {
		t.Fatal("an AWS role profile has no interactive Codex login")
	}
}

func TestAnInlineTokenCodexProfileIsAuthenticatedWithoutALoginFile(t *testing.T) {
	home := t.TempDir()
	// An inline-token provider never writes `auth.json`, so the profile is
	// proven by the same `config.toml` that carries the key.
	configTestWrite(t, filepath.Join(home, "config.toml"),
		"model = \"deepseek-flash\"\n"+
			"model_provider = \"deepseek\"\n"+
			"\n"+
			"[model_providers.deepseek]\n"+
			"base_url = \"https://api.deepseek.com/v1\"\n"+
			"experimental_bearer_token = \"inline-deepseek-key\"\n"+
			"wire_api = \"responses\"\n")
	profile := NewHarnessProfile(HarnessKindCodex, home)
	profile.Environment = EnvironmentFromMap(map[string]string{})

	if err := profile.validate("deepseek"); err != nil {
		t.Fatalf("an inline-token profile is valid: %v", err)
	}
	if err := profile.EnsureReady("deepseek"); err != nil {
		t.Fatalf("an inline token needs no profile environment: %v", err)
	}
	if scheme := profile.AuthScheme(); scheme != (AuthSchemeInlineApiKey{}) {
		t.Fatalf("auth scheme %#v", scheme)
	}
	if !profile.AuthScheme().IsAPIKey() || profile.AuthScheme().UsesNativeLoginFile() {
		t.Fatal("an inline key is an API key without a login file")
	}
	if marker := profile.AuthenticationMarker(); marker != filepath.Join(home, "config.toml") {
		t.Fatalf("the Codex configuration proves an inline-key profile is set up: %s", marker)
	}
	if _, ok := profile.CredentialFreshness([]byte("{}")); ok {
		t.Fatal("no freshness")
	}
	if _, ok := profile.CredentialExpiry([]byte("{}")); ok {
		t.Fatal("no expiry")
	}
	if key, ok := profile.CodexProviderAPIKey(); !ok || key != "inline-deepseek-key" {
		t.Fatalf("provider key %q", key)
	}
	_, _, err := LoginCommand(&profile)
	if err == nil {
		t.Fatal("an inline-token profile has no interactive login")
	}
	configTestContains(t, err.Error(), "inlines its provider API key")
}

// A Codex home that names its key variable works the way standalone Codex
// does: an exported key reaches the profile without an `environment` entry,
// and is never written to config.toml.
// Hard-won: d7a1bb61: an exported provider key worked standalone but failed under Mjolnir
func TestACodexProfileInheritsItsProviderKeyFromTheEnvironmentMjolnirRunsIn(t *testing.T) {
	home := t.TempDir()
	configTestZaiProfile(t, home, map[string]string{})
	text := fmt.Sprintf("version = %d\n\n[profiles.glm]\nkind = \"codex\"\nhome = %s\n", ConfigVersion, debugString(home))

	exported := configTestRead(t, text, map[string]string{"ZAI_API_KEY": "from-the-shell"}, map[string]string{})
	profile := exported.Profiles["glm"]
	if value, _ := profile.Environment.Get("ZAI_API_KEY"); value != "from-the-shell" {
		t.Fatalf("inherited %q", value)
	}
	if err := profile.EnsureReady("glm"); err != nil {
		t.Fatalf("the exported key is enough: %v", err)
	}
	written := configTestTOML(t, exported)
	configTestLacks(t, written, "ZAI_API_KEY")
	configTestLacks(t, written, "from-the-shell")

	// An entry the profile sets wins over the environment.
	explicit := text + "\n[profiles.glm.environment]\nZAI_API_KEY = \"configured\"\n"
	configured := configTestRead(t, explicit, map[string]string{"ZAI_API_KEY": "from-the-shell"}, map[string]string{})
	if value, _ := configured.Profiles["glm"].Environment.Get("ZAI_API_KEY"); value != "configured" {
		t.Fatalf("configured %q", value)
	}

	// Unset everywhere: the configuration still reads, the profile says how
	// to supply the key.
	unset := configTestRead(t, text, map[string]string{}, map[string]string{})
	err := unset.Profiles["glm"].EnsureReady("glm")
	if err == nil {
		t.Fatal("no key anywhere")
	}
	configTestContains(t, err.Error(), "export ZAI_API_KEY")
	configTestContains(t, err.Error(), "from_secret")
}

// A DSH profile authenticates with the key saved in DSH itself first, then
// the exported key, then the DSH home's `.env`, then Mjolnir's own secret.
// The store is read as DSH writes it: comments, grant records it does not
// interpret, and a file it refuses when others can read it.
// Hard-won: b526c21ff: DSH sessions ignored the key saved in DSH and refused to start
func TestADshProfileTakesItsKeyFromDshBeforeTheEnvironmentAndSecrets(t *testing.T) {
	home := t.TempDir()
	credentials := filepath.Join(home, ".credentials.yaml")
	dotenv := filepath.Join(home, ".env")
	writePrivate := func(path, text string) {
		t.Helper()
		configTestWrite(t, path, text)
		if err := os.Chmod(path, 0o600); err != nil {
			t.Fatalf("chmod 600: %v", err)
		}
	}
	// The layout from dsh-credentials-local's README, as the Models settings
	// write it.
	writePrivate(credentials, "version: 1\n\n"+
		"refs:\n  "+
		"# saved from the Models page\n  "+
		"DEEPSEEK_API_KEY: sk-saved-in-dsh\n  "+
		"OPENAI_API_KEY: sk-other\n\n"+
		"records:\n  "+
		"llm-pi-ai/openai-codex:\n    "+
		"kind: grant\n    "+
		"payload:\n      "+
		"type: oauth\n      "+
		"access: eyJhbGciOi\n")
	configTestWrite(t, dotenv, "export DEEPSEEK_API_KEY=\"sk-from-dotenv\" # home\n")
	text := fmt.Sprintf("version = %d\n\n[profiles.dsh]\nkind = \"dsh\"\nhome = %s\n\n"+
		"[profiles.dsh.environment]\nDEEPSEEK_API_KEY = { from_secret = \"DEEPSEEK_API_KEY\" }\n",
		ConfigVersion, debugString(home))
	secrets := map[string]string{"DEEPSEEK_API_KEY": "sk-mjolnir-secret"}
	read := func(process map[string]string) Config { return configTestRead(t, text, process, secrets) }
	key := func(config Config) string {
		value, _ := config.Profiles["dsh"].Environment.Get("DEEPSEEK_API_KEY")
		return value
	}
	exported := map[string]string{"DEEPSEEK_API_KEY": "sk-exported"}

	saved := read(exported)
	if got := key(saved); got != "sk-saved-in-dsh" {
		t.Fatalf("key %q", got)
	}
	if err := saved.Profiles["dsh"].EnsureReady("dsh"); err != nil {
		t.Fatalf("ready: %v", err)
	}
	written := configTestTOML(t, saved)
	configTestLacks(t, written, "sk-saved-in-dsh")
	configTestContains(t, written, "from_secret")

	if err := os.Remove(credentials); err != nil {
		t.Fatal(err)
	}
	if got := key(read(exported)); got != "sk-exported" {
		t.Fatalf("key %q", got)
	}
	if got := key(read(map[string]string{})); got != "sk-from-dotenv" {
		t.Fatalf("key %q", got)
	}
	if err := os.Remove(dotenv); err != nil {
		t.Fatal(err)
	}
	if got := key(read(map[string]string{})); got != "sk-mjolnir-secret" {
		t.Fatalf("key %q", got)
	}

	// A browser sign-in is no key dsh-acp can send; say so rather than
	// reporting the key as simply missing.
	writePrivate(credentials, "version: 1\nrecords:\n  deepseek-account-platform/default:\n    kind: grant\n    payload:\n      version: 1\n      token: account-token\n      issuer: https://platform.deepseek.com\n")
	textWithoutSecret := strings.Replace(text,
		"\n[profiles.dsh.environment]\nDEEPSEEK_API_KEY = { from_secret = \"DEEPSEEK_API_KEY\" }\n", "", 1)
	signedInConfig := func(text string) Config {
		return configTestRead(t, text, map[string]string{}, map[string]string{})
	}
	err := signedInConfig(textWithoutSecret).Profiles["dsh"].EnsureReady("dsh")
	if err == nil {
		t.Fatal("no API key")
	}
	configTestContains(t, err.Error(), "signed in to a DeepSeek account")
	configTestLacks(t, err.Error(), "account-token")

	// DSH refuses a store others can read, so its key is never used. A key
	// from another source still works, because the session sends only the
	// key; with none, the refusal says how to fix the file.
	if runtime.GOOS != "windows" {
		writePrivate(credentials, "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-saved-in-dsh\n")
		if err := os.Chmod(credentials, 0o644); err != nil {
			t.Fatalf("chmod 644: %v", err)
		}
		if got := key(read(exported)); got != "sk-exported" {
			t.Fatalf("key %q", got)
		}
		err := signedInConfig(textWithoutSecret).Profiles["dsh"].EnsureReady("dsh")
		if err == nil {
			t.Fatal("readable by others")
		}
		configTestContains(t, err.Error(), "chmod 600")
		configTestLacks(t, err.Error(), "sk-saved-in-dsh")
	}
}

// OpenCode signs in to a provider from a key in its environment, with no
// stored login. An OpenCode profile inherits exactly the variables OpenCode
// reads: its catalog's provider keys, the `env` lists of providers in its
// configuration, and `{env:NAME}` references, from JSONC as OpenCode parses
// it. A malformed file contributes nothing, as OpenCode skips it.
// Hard-won: 1e1e6f92c: OpenCode sessions never received provider keys exported to Mjolnir
func TestAnOpencodeProfileInheritsTheProviderKeysOpencodeReads(t *testing.T) {
	home := t.TempDir()
	configTestWrite(t, filepath.Join(home, "opencode.jsonc"), `{
  // A gateway the person runs; its key is a configured provider's `+"`env`"+`.
  "provider": {
    "gateway": {
      "env": ["GATEWAY_KEY"],
      "options": { "baseURL": "{env:GATEWAY_URL}" },
    },
  },
}
`)
	configTestWrite(t, filepath.Join(home, "opencode.json"), "{'provider': {'other': {'env': ['MALFORMED_KEY']}}}\n")
	text := fmt.Sprintf("version = %d\n\n[profiles.opencode]\nkind = \"open-code\"\nhome = %s\n", ConfigVersion, debugString(home))
	read := func(process map[string]string) Config { return configTestRead(t, text, process, map[string]string{}) }

	exported := read(map[string]string{
		"DEEPSEEK_API_KEY":   "deepseek-key",
		"OPENROUTER_API_KEY": "",
		"GATEWAY_KEY":        "gateway-key",
		"GATEWAY_URL":        "https://gateway.example",
		"MALFORMED_KEY":      "never-read",
		"UNRELATED_SECRET":   "not-opencodes",
	})
	profile := exported.Profiles["opencode"]
	// An empty variable is unset, for OpenCode as for `from_env`.
	want := map[string]string{
		"DEEPSEEK_API_KEY": "deepseek-key",
		"GATEWAY_KEY":      "gateway-key",
		"GATEWAY_URL":      "https://gateway.example",
	}
	if got := profile.Environment.Resolved(); !reflect.DeepEqual(got, want) {
		t.Fatalf("inherited %v", got)
	}
	// A substituted URL is not a login.
	if got := profile.OpencodeEnvironmentLogins(); !slices.Equal(got, []string{"DEEPSEEK_API_KEY", "GATEWAY_KEY"}) {
		t.Fatalf("logins %v", got)
	}
	written := configTestTOML(t, exported)
	configTestLacks(t, written, "deepseek-key")
	configTestLacks(t, written, "DEEPSEEK_API_KEY")

	unset := read(map[string]string{})
	if got := unset.Profiles["opencode"].OpencodeEnvironmentLogins(); len(got) != 0 {
		t.Fatalf("logins %v", got)
	}
	// Exporting a key changes which models OpenCode offers, so a catalogue
	// cached without it must not answer.
	if profile.CapabilitiesKey("opencode") == unset.Profiles["opencode"].CapabilitiesKey("opencode") {
		t.Fatal("an exported key must change the capability key")
	}
}

// Hard-won: ee59e575: users were told to delete their supported profile model catalog
func TestACodexProfileMayNameItsOwnModelCatalog(t *testing.T) {
	home := t.TempDir()
	profile := configTestZaiProfile(t, home, map[string]string{"ZAI_API_KEY": "secret"})
	body := configTestReadFile(t, filepath.Join(home, "config.toml"))
	configTestWrite(t, filepath.Join(home, "config.toml"), "model_catalog_json = \"mine.json\"\n"+body)
	profile.Enabled = true
	// Mjolnir stages its own catalog and replaces the key there, so a
	// profile-authored file is not a reason to refuse.
	if err := profile.validate("glm"); err != nil {
		t.Fatalf("a profile-authored catalog validates: %v", err)
	}
	provider, err := profile.CodexProvider()
	if err != nil || provider == nil {
		t.Fatalf("a custom provider: %v", err)
	}
	if provider.ModelCatalogJSON == nil || *provider.ModelCatalogJSON != "mine.json" {
		t.Fatalf("the file the profile names is recorded, though Mjolnir stages its own: %v", provider.ModelCatalogJSON)
	}
}

// Saving edits the user's file in place: comments, blank lines, and the
// order of sections and keys survive a save that changes one value. Launch
// campaign finding C-21.
// Hard-won: 25a04304: saving config deleted comments and rewrote user layout
func TestSavingKeepsTheFilesCommentsAndOrder(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	original := fmt.Sprintf("# My Mjolnir settings\nversion = %d\n\n"+
		"# Quiet, please.\n[notify]\ntitle = false # no counts in the title\nbell = true\n\n"+
		"[advanced]\n# Clocks help me debug.\ndetailed_activity_clocks = true\n", ConfigVersion)
	configTestWrite(t, path, original)
	config := configTestLoad(t, path)
	config.Notify.Bell = false
	configTestSave(t, config, path)

	saved := configTestReadFile(t, path)
	if want := strings.Replace(original, "bell = true", "bell = false", 1); saved != want {
		t.Fatalf("saved %q, want %q", saved, want)
	}
	if config.Advanced.AutoAfkMinutes != 0 {
		t.Fatal("auto AFK default")
	}
	configTestLacks(t, saved, "auto_afk_minutes")
	configTestAssertEqual(t, configTestLoad(t, path), config)

	config.Advanced.AutoAfkMinutes = ^uint32(0)
	configTestSave(t, config, path)
	configTestContains(t, configTestReadFile(t, path), "auto_afk_minutes = 4294967295")
	if got := configTestLoad(t, path).Advanced.AutoAfkMinutes; got != ^uint32(0) {
		t.Fatalf("auto AFK %d", got)
	}

	// A version 12 file is marked as this build's, in place.
	configTestWrite(t, path, "# keep me\nversion = 12 # the file format\n")
	configTestSave(t, configTestLoad(t, path), path)
	if got, want := configTestReadFile(t, path), fmt.Sprintf("# keep me\nversion = %d # the file format\n", ConfigVersion); got != want {
		t.Fatalf("saved %q, want %q", got, want)
	}
}

// A reference under `environment` resolves from `secrets.toml` beside the
// configuration, and a save writes the reference back, never the value.
func TestEnvironmentReferencesResolveFromTheSecretsFileAndSurviveASave(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	secrets := filepath.Join(directory, SecretsFile)
	configTestWrite(t, secrets, "PROVIDER_API_KEY = \"stored-value\"\n")
	if err := os.Chmod(secrets, 0o600); err != nil {
		t.Fatal(err)
	}
	original := fmt.Sprintf("version = %d\n\n[notify]\ntitle = false\nbell = true\n\n"+
		"[profiles.codex]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n"+
		"[profiles.codex.environment]\nPROVIDER_API_KEY = { from_secret = \"PROVIDER_API_KEY\" }\n"+
		"PLAIN = \"value\"\n", ConfigVersion)
	configTestWrite(t, path, original)
	config := configTestLoad(t, path)
	environment := config.Profiles["codex"].Environment
	if value, _ := environment.Get("PROVIDER_API_KEY"); value != "stored-value" {
		t.Fatalf("secret %q", value)
	}
	if value, _ := environment.Get("PLAIN"); value != "value" {
		t.Fatalf("plain %q", value)
	}
	if source := environment.Sources()["PROVIDER_API_KEY"]; source != (EnvironmentValueFromSecret{Value: "PROVIDER_API_KEY"}) {
		t.Fatalf("source %#v", source)
	}

	config.Notify.Bell = false
	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	configTestLacks(t, saved, "stored-value")
	if want := strings.Replace(original, "bell = true", "bell = false", 1); saved != want {
		t.Fatalf("saved %q, want %q", saved, want)
	}
	configTestAssertEqual(t, configTestLoad(t, path), config)

	// A missing secret makes the profile unusable, not the configuration
	// unreadable, and an unrelated save still writes the reference back.
	configTestWrite(t, secrets, "")
	config = configTestLoad(t, path)
	if _, ok := config.Profiles["codex"].Environment.Get("PROVIDER_API_KEY"); ok {
		t.Fatal("a missing secret has no value")
	}
	config.Notify.Bell = true
	configTestSave(t, config, path)
	if got := configTestReadFile(t, path); got != original {
		t.Fatalf("saved %q, want %q", got, original)
	}
}

// Test-and-fix C-8 reported a missing secret as a TOML parse error with a
// caret hundreds of columns wide. Now the configuration loads, and the
// profile names the entry and the file in one line where it is used.
// Hard-won: 871a4d9b: one unresolved profile secret prevented daemon startup
func TestAMissingSecretIsNamedInOneLineWhereItsProfileIsUsed(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	padding := strings.Repeat("X", 400)
	configTestWrite(t, path, fmt.Sprintf("version = %d\n\n[profiles.codex]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n"+
		"[profiles.codex.environment]\nFAKE_TOKEN = { from_secret = \"FAKE_TOKEN\" }\nPAD = \"%s\"\n", ConfigVersion, padding))
	config := configTestLoad(t, path)
	err := config.Profiles["codex"].EnsureReady("codex")
	if err == nil {
		t.Fatal("a missing secret")
	}
	// The person's to fix, so a client is told the reason, not "internal error".
	refusal, ok := RefusalOf(err)
	if !ok {
		t.Fatal("the failure is a refusal")
	}
	if refusal.Kind() != RefusalKindPrecondition {
		t.Fatalf("refusal kind %v", refusal.Kind())
	}
	message := err.Error()
	if !strings.HasPrefix(message, "profile \"codex\": FAKE_TOKEN = { from_secret = \"FAKE_TOKEN\" } needs ") {
		t.Fatal(message)
	}
	configTestContains(t, message, SecretsFile)
	configTestLacks(t, message, "\n")
}

// Hard-won: ee847f7: a save removed every top-level setting the user wrote
// out at its default, taking an explicit `[daemon] runtime = "host"` with it;
// retired keys nested in such a table were only removed by that accident.
func TestASaveKeepsSettingsWrittenAtTheirDefaultAndRemovesRetiredKeys(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	kept := fmt.Sprintf("version = %d\ntheme = \"auto\"\n\n[daemon]\nruntime = \"host\"\n\n[phone]\nenabled = false\n", ConfigVersion)
	configTestWrite(t, path, kept+"\n[subagents]\nmax_concurrent = 6\nenabled = true\n\n[sessionwiki]\nenabled = true\n")
	configTestSave(t, configTestLoad(t, path), path)
	// The written default stays beside the removed key; a table that held
	// only a retired key goes with it.
	if got, want := configTestReadFile(t, path), kept+"\n[subagents]\nmax_concurrent = 6\n"; got != want {
		t.Fatalf("saved %q, want %q", got, want)
	}
}

func TestObsoleteStartupSettingsAreIgnoredAndRemovedWhenSaving(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	expected := configTestSampleConfig()
	original := configTestTOML(t, expected)
	for _, enabled := range []bool{true, false} {
		configTestWrite(t, path, fmt.Sprintf("%s\n[startup]\nenabled = %t\nprompt = false\nprofile = \"missing\"\ntarget = \"missing\"\n", original, enabled))
		loaded := configTestLoad(t, path)
		configTestAssertEqual(t, loaded, expected)
		data, err := serdeJSON(loaded)
		if err != nil {
			t.Fatal(err)
		}
		fields, _ := decodeObject(data)
		if _, ok := fields["startup"]; ok {
			t.Fatal("startup serialized")
		}
		configTestSave(t, loaded, path)
		configTestLacks(t, configTestReadFile(t, path), "[startup]")
	}
}

func TestRetiredStoppedSessionFiltersLoadAndAreDroppedOnSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	for _, runtimeName := range []string{"auto", "host", "podman", "docker"} {
		legacy := fmt.Sprintf("version = 6\nshow_stopped_sessions = true\n\n[advanced]\nshow_stopped_sessions = true\n\n[quota]\nruntime = \"%s\"\n", runtimeName)
		configTestWrite(t, path, legacy)
		config := configTestLoad(t, path)
		if got := configTestReadFile(t, path); got != legacy {
			t.Fatalf("load rewrote the file: %q", got)
		}
		configTestSave(t, config, path)
		body := configTestReadFile(t, path)
		configTestLacks(t, body, "show_stopped_sessions")
		saved, err := parseTOMLDocument(body)
		if err != nil {
			t.Fatal(err)
		}
		if quota, ok := saved["quota"].(map[string]any); ok {
			if _, ok := quota["runtime"]; ok {
				t.Fatal(body)
			}
		}
		configTestAssertEqual(t, configTestLoad(t, path), config)
	}
}

func TestNestedHomeMappingKeepsConfigCredentialsAndSessionDataTogether(t *testing.T) {
	for _, test := range []struct {
		kind       HarnessKind
		credential string
	}{
		{HarnessKindMuse, "auth.json"},
		{HarnessKindOpenCode, ".data/opencode/mjolnir-credentials.json"},
	} {
		home := Join("/private/session", test.kind.ID())
		environment := map[string]string{"XDG_DATA_HOME": "/unrelated"}
		test.kind.ConfigureHomeEnvironment(home, environment)
		if environment["XDG_CONFIG_HOME"] != "/private/session" {
			t.Fatalf("%v config home %q", test.kind, environment["XDG_CONFIG_HOME"])
		}
		if want := fmt.Sprintf("/private/session/%s/.data", test.kind.ID()); environment["XDG_DATA_HOME"] != want {
			t.Fatalf("%v data home %q", test.kind, environment["XDG_DATA_HOME"])
		}
		if got := test.kind.HomeFromEnvironment(environment["XDG_CONFIG_HOME"]); !samePath(got, home) {
			t.Fatalf("%v home %q", test.kind, got)
		}
		if got := HarnessAuthenticationMarker(test.kind, home); !samePath(got, filepath.Join(home, test.credential)) {
			t.Fatalf("%v marker %q", test.kind, got)
		}
	}
}

func configTestSampleConfig() Config {
	config := DefaultConfig()
	profile := NewHarnessProfile(HarnessKindCodex, "/home/test/.codex-one")
	profile.Environment = EnvironmentFromMap(map[string]string{"RUST_LOG": "info"})
	config.Profiles = map[string]HarnessProfile{"codex-1": profile}
	config.Bundles = map[string]ProjectBundle{"hel": {
		PrimaryRepo: "app",
		Repositories: []ProjectRepository{{
			ID:          "app",
			Github:      configTestPtr("BrokkAi/hel"),
			Destination: "app",
		}},
	}}
	config.Targets = map[string]TargetTemplate{"podman-default": TargetTemplateLocalPodman{ContainerTemplate: ContainerTemplate{
		Image:            "ubuntu:24.04",
		PullPolicy:       ImagePullPolicyAuto,
		WorkspaceStorage: PodmanWorkspaceStoragePodmanVolume{},
	}}}
	return config
}

// A session or probe runs from a home Mjolnir staged for it, so every harness
// is pointed at that home, macOS included.
func TestEveryHarnessIsPointedAtItsStagedHome(t *testing.T) {
	for _, kind := range HarnessKindAll {
		environment := map[string]string{}
		home := filepath.Join("/private/session", kind.ID())
		kind.ConfigureStagedHomeEnvironment(home, environment)
		value, ok := environment[kind.HomeEnv()]
		if !ok {
			t.Fatalf("%v sets no home", kind)
		}
		if got := kind.HomeFromEnvironment(value); !samePath(got, home) {
			t.Fatalf("%v home %q", kind, got)
		}
		if kind == HarnessKindKimi && environment["KIMI_CODE_SESSION_LOCK"] != "0" {
			t.Fatal("Kimi session lock")
		}
	}
}

// Commands that act on the person's own profile home leave Claude's variable
// unset on macOS, where the login lives in the Keychain, and set it
// everywhere else.
func TestOnlyClaudeOnMacosKeepsItsOwnHomeForProfileCommands(t *testing.T) {
	home := "/home/me/.claude-work"

	mac := map[string]string{}
	HarnessKindClaude.ConfigureProfileHomeEnvironment(home, HarnessHostMacOs, mac)
	if len(mac) != 0 {
		t.Fatalf("%v", mac)
	}

	linux := map[string]string{}
	HarnessKindClaude.ConfigureProfileHomeEnvironment(home, HarnessHostOther, linux)
	if linux["CLAUDE_CONFIG_DIR"] != home {
		t.Fatalf("%v", linux)
	}

	for _, kind := range HarnessKindAll {
		environment := map[string]string{"OPENCODE_CONFIG_DIR": "/host/provider-config"}
		kind.ConfigureProfileHomeEnvironment("/home/me/muse", HarnessHostMacOs, environment)
		if environment["OPENCODE_CONFIG_DIR"] != "/host/provider-config" {
			t.Fatalf("%v replaced OPENCODE_CONFIG_DIR", kind)
		}
		if _, ok := environment[kind.HomeEnv()]; ok != (kind != HarnessKindClaude) {
			t.Fatalf("%v home set: %v", kind, ok)
		}
	}
}

func configTestEnforcement(t *testing.T, kind HarnessKind, policy ExecutionPolicy) ExecutionEnforcement {
	t.Helper()
	enforcement, ok := kind.ExecutionEnforcement(policy)
	if !ok {
		t.Fatalf("%v enforces no %v policy", kind, policy)
	}
	return enforcement
}

func configTestOptional(value string, ok bool) *string {
	if !ok {
		return nil
	}
	return &value
}

func configTestAssertOptional(t *testing.T, value string, ok bool, want *string) {
	t.Helper()
	got := configTestOptional(value, ok)
	if !equalStringPointers(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func configTestAssertPair(t *testing.T, key, value string, ok bool, want *[2]string) {
	t.Helper()
	if want == nil {
		if ok {
			t.Fatalf("got (%q, %q), want none", key, value)
		}
		return
	}
	if !ok || key != want[0] || value != want[1] {
		t.Fatalf("got (%q, %q, %v), want %v", key, value, ok, *want)
	}
}

func TestHarnessMappingAndPermissionModesAreFixed(t *testing.T) {
	for kind, env := range map[HarnessKind]string{
		HarnessKindCodex: "CODEX_HOME", HarnessKindClaude: "CLAUDE_CONFIG_DIR",
		HarnessKindKimi: "KIMI_CODE_HOME", HarnessKindGrok: "GROK_HOME",
	} {
		if kind.HomeEnv() != env {
			t.Fatalf("%v home env %q", kind, kind.HomeEnv())
		}
	}
	codex := configTestEnforcement(t, HarnessKindCodex, ExecutionPolicyUnconstrained)
	mode, ok := codex.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("agent-full-access"))
	if codex.Label() != "agent-full-access" {
		t.Fatal(codex.Label())
	}
	claude := configTestEnforcement(t, HarnessKindClaude, ExecutionPolicyUnconstrained)
	mode, ok = claude.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("bypassPermissions"))
	kimi := configTestEnforcement(t, HarnessKindKimi, ExecutionPolicyUnconstrained)
	mode, ok = kimi.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("auto"))
}

func TestUnconstrainedEnforcementSplitsAcpModesFromLaunchControls(t *testing.T) {
	for _, kind := range []HarnessKind{HarnessKindCodex, HarnessKindKimi} {
		enforcement := configTestEnforcement(t, kind, ExecutionPolicyUnconstrained)
		mode, ok := enforcement.ACPMode()
		configTestAssertOptional(t, mode, ok, configTestPtr(enforcement.Label()))
		flag, ok := enforcement.LaunchFlag()
		configTestAssertOptional(t, flag, ok, nil)
	}
	key, value, ok := configTestEnforcement(t, HarnessKindCodex, ExecutionPolicyUnconstrained).LaunchEnvironment()
	configTestAssertPair(t, key, value, ok, &[2]string{"INITIAL_AGENT_MODE", "agent-full-access"})
	grok := configTestEnforcement(t, HarnessKindGrok, ExecutionPolicyUnconstrained)
	mode, ok := grok.ACPMode()
	configTestAssertOptional(t, mode, ok, nil)
	flag, ok := grok.LaunchFlag()
	configTestAssertOptional(t, flag, ok, configTestPtr("--always-approve"))
	if grok.Label() != "always-approve / sandbox-off" {
		t.Fatal(grok.Label())
	}
	key, value, ok = grok.LaunchEnvironment()
	configTestAssertPair(t, key, value, ok, &[2]string{"GROK_SANDBOX", "off"})
	claude := configTestEnforcement(t, HarnessKindClaude, ExecutionPolicyUnconstrained)
	mode, ok = claude.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("bypassPermissions"))
	if claude.Label() != "bypassPermissions / sandbox-off" {
		t.Fatal(claude.Label())
	}
	muse := configTestEnforcement(t, HarnessKindMuse, ExecutionPolicyUnconstrained)
	// allowAll emits no permission requests, so the adapter's client-side
	// auto-review would have nothing to decide: isolated targets run Muse
	// without permission checks, like OpenCode's `permission = "allow"`.
	mode, ok = muse.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("allowAll"))
	if muse.Label() != "allowAll / no-review / sandbox-off / :ask-me" {
		t.Fatal(muse.Label())
	}
	key, value, ok = muse.ACPSetting()
	configTestAssertPair(t, key, value, ok, &[2]string{"auto_review", "off"})
	key, value, ok = muse.LaunchEnvironment()
	configTestAssertPair(t, key, value, ok, &[2]string{"MUSE_APPROVAL_MODE", "allowAll"})
	if argument, ok := muse.LaunchArgument(); !ok || argument != (LaunchArgument{Variable: "MUSE_SERVE_ARGS", Word: "--disable-sandbox", Present: true}) {
		t.Fatalf("launch argument %+v", argument)
	}
	if setting, ok := muse.StagedSetting(); !ok || setting.Value != ":ask-me" {
		t.Fatalf("staged setting %+v", setting)
	}
}

func TestConfiguredApprovalsPreserveOtherProfilesAndSelectGuardians(t *testing.T) {
	for _, kind := range HarnessKindAll {
		stored := DefaultConfig()
		profile := NewHarnessProfile(kind, filepath.Join("/profiles", kind.ID()))
		profile.MjolnirGuardian = false
		stored.Profiles["p"] = profile
		config := configTestFromTOML(t, configTestTOML(t, stored))
		if err := config.Validate(); err != nil {
			t.Fatalf("all harnesses can disable Mjolnir review: %v", err)
		}
		disabled := config.Profiles["p"].ApprovalChoices()
		enabled := disabled
		enabled.MjolnirGuardian = true
		if !kind.MjolnirGuardianReviews(ExecutionPolicyConfiguredApprovals, enabled) {
			t.Fatalf("%v: enabled guardian does not review", kind)
		}
		if kind.MjolnirGuardianReviews(ExecutionPolicyConfiguredApprovals, disabled) {
			t.Fatalf("%v: disabled guardian reviews", kind)
		}
		if kind.MjolnirGuardianReviews(ExecutionPolicyUnconstrained, enabled) {
			t.Fatalf("%v: guardian reviews an unconstrained target", kind)
		}
	}

	config := DefaultConfig()
	if config.Guardian != DefaultGuardianConfig() {
		t.Fatalf("guardian %+v", config.Guardian)
	}
	configTestLacks(t, configTestTOML(t, config), "[guardian]")

	config.Guardian = GuardianConfig{Enabled: false, LocalClassifier: false, UseGPU: false}
	loaded := configTestFromTOML(t, configTestTOML(t, config))
	if loaded.Guardian != config.Guardian {
		t.Fatalf("round trip [guardian]: %+v", loaded.Guardian)
	}

	profileOn := NewHarnessProfile(HarnessKindKimi, "/profiles/kimi")
	profileOff := profileOn
	profileOff.MjolnirGuardian = false
	globalOn := DefaultConfig()
	if !globalOn.ProfileApprovalChoices(profileOn).MjolnirGuardian {
		t.Fatal("global and profile on")
	}
	if globalOn.ProfileApprovalChoices(profileOff).MjolnirGuardian {
		t.Fatal("profile off")
	}
	globalOn.Guardian.Enabled = false
	if globalOn.ProfileApprovalChoices(profileOn).MjolnirGuardian {
		t.Fatal("global off")
	}
	if globalOn.ProfileApprovalChoices(profileOff).MjolnirGuardian {
		t.Fatal("both off")
	}

	codex := configTestEnforcement(t, HarnessKindCodex, ExecutionPolicyConfiguredApprovals)
	mode, ok := codex.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("agent"))
	key, value, ok := codex.LaunchEnvironment()
	configTestAssertPair(t, key, value, ok, &[2]string{"INITIAL_AGENT_MODE", "agent"})
	claude := configTestEnforcement(t, HarnessKindClaude, ExecutionPolicyConfiguredApprovals)
	mode, ok = claude.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("auto"))
	if claude.Label() != "auto / guardian" {
		t.Fatal(claude.Label())
	}
	if _, ok := claude.SessionSandbox(); ok {
		t.Fatal("Claude's guardian asks nothing of the sandbox")
	}
	if _, ok := claude.StagedSetting(); ok {
		t.Fatal("Claude's guardian stages nothing")
	}
	muse := configTestEnforcement(t, HarnessKindMuse, ExecutionPolicyConfiguredApprovals)
	mode, ok = muse.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("promptUnmatched"))
	key, value, ok = muse.ACPSetting()
	configTestAssertPair(t, key, value, ok, &[2]string{"auto_review", "on"})
	if muse.Label() != "promptUnmatched / auto-review / :ask-me" {
		t.Fatal(muse.Label())
	}
	key, value, ok = muse.LaunchEnvironment()
	configTestAssertPair(t, key, value, ok, &[2]string{"MUSE_APPROVAL_MODE", "promptUnmatched"})
	if argument, ok := muse.LaunchArgument(); !ok || argument != (LaunchArgument{Variable: "MUSE_SERVE_ARGS", Word: "--disable-sandbox", Present: false}) {
		t.Fatalf("launch argument %+v", argument)
	}
	if setting, ok := muse.StagedSetting(); !ok || setting.Value != ":ask-me" {
		t.Fatalf("staged setting %+v", setting)
	}

	for _, kind := range []HarnessKind{HarnessKindKimi, HarnessKindGrok} {
		if _, ok := kind.ExecutionEnforcement(ExecutionPolicyConfiguredApprovals); ok {
			t.Fatalf("%v enforces configured approvals", kind)
		}
	}
}

// A config file written while Muse profiles could opt back into review on
// unconstrained targets (`muse_auto_review`, 2.60 to 2.71) still loads on any
// profile kind. The key is ignored and dropped when the file is saved: Muse
// follows its runtime's policy like every harness, running unconstrained
// sessions without permission checks and guardian ones with auto-review.
func TestAStoredMuseAutoReviewStillLoadsAndNoLongerChangesReview(t *testing.T) {
	for _, test := range []struct{ kind, home string }{
		{"muse", "/home/test/.config/muse"},
		{"codex", "/home/test/.codex"},
	} {
		config := configTestFromTOML(t, fmt.Sprintf("version = %d\n[profiles.p]\nkind = \"%s\"\nhome = \"%s\"\nmuse_auto_review = true\n",
			ConfigVersion, test.kind, test.home))
		if err := config.Validate(); err != nil {
			t.Fatalf("the legacy key is accepted: %v", err)
		}
		configTestLacks(t, configTestTOML(t, config), "muse_auto_review")
	}

	config := configTestFromTOML(t, fmt.Sprintf("version = %d\n[profiles.muse]\nkind = \"muse\"\nhome = \"/home/test/.config/muse\"\nmuse_auto_review = true\n", ConfigVersion))
	muse := config.Profiles["muse"]
	enforcement, ok := muse.ExecutionEnforcement(ExecutionPolicyUnconstrained)
	if !ok {
		t.Fatal("Muse enforces an execution policy")
	}
	mode, ok := enforcement.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("allowAll"))
	key, value, ok := enforcement.ACPSetting()
	configTestAssertPair(t, key, value, ok, &[2]string{"auto_review", "off"})
	environment := map[string]string{}
	if err := muse.ConfigureExecutionEnvironment(ExecutionPolicyUnconstrained, environment); err != nil {
		t.Fatal(err)
	}
	if environment["MUSE_APPROVAL_MODE"] != "allowAll" {
		t.Fatalf("%v", environment)
	}
	guardian, ok := muse.ExecutionEnforcement(ExecutionPolicyConfiguredApprovals)
	if !ok {
		t.Fatal("Muse guardian enforces an execution policy")
	}
	mode, ok = guardian.ACPMode()
	configTestAssertOptional(t, mode, ok, configTestPtr("promptUnmatched"))
	key, value, ok = guardian.ACPSetting()
	configTestAssertPair(t, key, value, ok, &[2]string{"auto_review", "on"})
}

// Hard-won: #1278: guardian Muse kept a stale --disable-sandbox in MUSE_SERVE_ARGS
func TestMuseSandboxWordConvergesWithEachPolicyAndKeepsUserFlags(t *testing.T) {
	environment := map[string]string{"MUSE_SERVE_ARGS": "--trust-workspace  --disable-sandbox --log-level debug"}
	serveArgs := func(environment map[string]string) *string {
		value, ok := environment["MUSE_SERVE_ARGS"]
		return configTestOptional(value, ok)
	}
	configure := func(policy ExecutionPolicy, environment map[string]string) {
		t.Helper()
		if err := HarnessKindMuse.ConfigureExecutionEnvironment(policy, environment); err != nil {
			t.Fatal(err)
		}
	}
	assertArgs := func(environment map[string]string, want *string) {
		t.Helper()
		if got := serveArgs(environment); !equalStringPointers(got, want) {
			t.Fatalf("MUSE_SERVE_ARGS %v, want %v", got, want)
		}
	}

	configure(ExecutionPolicyConfiguredApprovals, environment)
	assertArgs(environment, configTestPtr("--trust-workspace --log-level debug"))
	configure(ExecutionPolicyUnconstrained, environment)
	assertArgs(environment, configTestPtr("--trust-workspace --log-level debug --disable-sandbox"))
	configure(ExecutionPolicyConfiguredApprovals, environment)
	assertArgs(environment, configTestPtr("--trust-workspace --log-level debug"))

	// A variable that held only the sandbox word is removed, not left empty.
	environment = map[string]string{}
	configure(ExecutionPolicyUnconstrained, environment)
	assertArgs(environment, configTestPtr("--disable-sandbox"))
	configure(ExecutionPolicyConfiguredApprovals, environment)
	assertArgs(environment, nil)
}

func TestBridgeArgsCarryTheAcpSubcommandPerHarness(t *testing.T) {
	for _, policy := range []ExecutionPolicy{ExecutionPolicyConfiguredApprovals, ExecutionPolicyUnconstrained} {
		if len(HarnessKindCodex.BridgeArgs(policy)) != 0 || len(HarnessKindClaude.BridgeArgs(policy)) != 0 {
			t.Fatal("Codex and Claude bridges take no arguments")
		}
		if got := HarnessKindKimi.BridgeArgs(policy); !slices.Equal(got, []string{"acp"}) {
			t.Fatalf("Kimi %v", got)
		}
		want := []string{"agent", "stdio"}
		if policy.IsUnconstrained() {
			want = []string{"agent", "--always-approve", "stdio"}
		}
		if got := HarnessKindGrok.BridgeArgs(policy); !slices.Equal(got, want) {
			t.Fatalf("policy: %v: %v", policy, got)
		}
	}
}

func configTestSetBundle(config *Config, edit func(*ProjectBundle)) {
	bundle := config.Bundles["hel"]
	bundle.Repositories = append([]ProjectRepository{}, bundle.Repositories...)
	edit(&bundle)
	config.Bundles["hel"] = bundle
}

func TestBundleRejectsTraversalAndDuplicateDestinations(t *testing.T) {
	config := configTestSampleConfig()
	configTestSetBundle(&config, func(bundle *ProjectBundle) { bundle.Repositories[0].Destination = "../escape" })
	configTestContains(t, configTestValidateError(t, config), "'..'")

	config = configTestSampleConfig()
	configTestSetBundle(&config, func(bundle *ProjectBundle) {
		bundle.Repositories = append(bundle.Repositories, ProjectRepository{ID: "docs", Github: configTestPtr("BrokkAi/docs"), Destination: "app"})
	})
	configTestContains(t, configTestValidateError(t, config), "overlapping destinations")
}

func TestBundleRequiresExistingPrimaryRepository(t *testing.T) {
	config := configTestSampleConfig()
	configTestSetBundle(&config, func(bundle *ProjectBundle) { bundle.PrimaryRepo = "missing" })
	configTestContains(t, configTestValidateError(t, config), "does not exist")
}

func TestBundleConfigRoundTripRemovesHTTPUserinfoAndKeepsSSHUser(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	config := configTestSampleConfig()
	configTestSetBundle(&config, func(bundle *ProjectBundle) {
		repository := &bundle.Repositories[0]
		repository.Github = configTestPtr("https://gitlab.example/team/hel.git?token=route#checkout")
		ssh := *repository
		ssh.ID = "ssh"
		ssh.Destination = "ssh"
		ssh.Github = configTestPtr("ssh://build@gitlab.example/team/hel.git")
		bundle.Repositories = append(bundle.Repositories, ssh)
	})
	configTestSave(t, config, path)

	// Loading keeps the old source usable, and an ordinary save must also
	// remove the credentials from the actual file when preserving comments.
	body := strings.ReplaceAll(configTestReadFile(t, path), "https://gitlab.example", "https://user:password@gitlab.example")
	configTestWrite(t, path, "# keep this comment\n"+body)
	loaded := configTestLoad(t, path)
	if !reflect.DeepEqual(loaded.Bundles, config.Bundles) {
		t.Fatalf("loaded bundles %+v", loaded.Bundles)
	}
	configTestSave(t, loaded, path)
	saved := configTestReadFile(t, path)
	if !strings.HasPrefix(saved, "# keep this comment") {
		t.Fatal(saved)
	}
	configTestLacks(t, saved, "user:password")
	if reloaded := configTestLoad(t, path); !reflect.DeepEqual(reloaded.Bundles, config.Bundles) {
		t.Fatalf("reloaded bundles %+v", reloaded.Bundles)
	}
}

// Hard-won: fd9e9238c: an older accepted repository value blocked config load after upgrade.
func TestBundleKeepsLoadingLegacyRepositoryValues(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestSave(t, configTestSampleConfig(), path)
	previousConfig := configTestReadFile(t, path)

	for _, source := range []string{
		"./repo",
		"~/repo",
		"owner@host/repo",
		"file:mirror/repo",
		"git@github.com:owner/repo",
		"BrokkAi/hel+fork",
	} {
		configTestWrite(t, path, strings.ReplaceAll(previousConfig, "BrokkAi/hel", source))
		config, err := ConfigLoadFrom(path)
		if err != nil {
			t.Fatalf("%s: %v", source, err)
		}
		if github := config.Bundles["hel"].Repositories[0].Github; github == nil || *github != source {
			t.Fatalf("%s: github %v", source, github)
		}
		configTestSave(t, config, path)
	}
}

func TestBundleAcceptsOneAbsoluteLocalSource(t *testing.T) {
	config := configTestSampleConfig()
	configTestSetBundle(&config, func(bundle *ProjectBundle) {
		bundle.Repositories[0].Github = nil
		// A local source is a directory on this controller.
		bundle.Repositories[0].Local = configTestPtr(filepath.Join(os.TempDir(), "app"))
	})
	if err := config.Validate(); err != nil {
		t.Fatal(err)
	}
	configTestSetBundle(&config, func(bundle *ProjectBundle) { bundle.Repositories[0].Local = configTestPtr("relative/app") })
	configTestContains(t, configTestValidateError(t, config), "absolute")
}

func TestBundleRequiresExactlyOneRepositorySource(t *testing.T) {
	config := configTestSampleConfig()
	configTestSetBundle(&config, func(bundle *ProjectBundle) { bundle.Repositories[0].Local = configTestPtr("/home/test/src/app") })
	configTestContains(t, configTestValidateError(t, config), "exactly one")
}

func TestConfigTomlRoundTripIsAtomic(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "nested/config.toml")
	config := configTestSampleConfig()
	config.SidebarSize = PaneSizeMinimized
	config.Images.Repository = configTestPtr("localhost:5000/projects")
	config.Images.Bundles = map[string]map[string]string{
		"hel":     {"linux/amd64": "localhost:5000/projects/hel:recipe"},
		"retired": {"linux/amd64": "localhost:5000/projects/retired:recipe"},
	}
	configTestSave(t, config, path)
	loaded := configTestLoad(t, path)
	image := func(bundle *string, platform string) string {
		value, ok := loaded.SessionImage(bundle, "podman-default", platform)
		if !ok {
			t.Fatal("no session image")
		}
		return value
	}
	if got := image(configTestPtr("retired"), "linux/amd64"); got != "ubuntu:24.04" {
		t.Fatal(got)
	}
	if _, ok := loaded.Images.Bundles["retired"]; !ok {
		t.Fatal("retired assignments are retained")
	}
	if got := image(configTestPtr("hel"), "linux/amd64"); got != "localhost:5000/projects/hel:recipe" {
		t.Fatal(got)
	}
	if got := image(configTestPtr("hel"), "linux/arm64"); got != "ubuntu:24.04" {
		t.Fatal(got)
	}
	if got := image(nil, "linux/amd64"); got != "ubuntu:24.04" {
		t.Fatal(got)
	}
	configTestAssertEqual(t, configTestLoad(t, path), config)
	body := configTestReadFile(t, path)
	configTestLacks(t, body, "pull_policy")
	if count := strings.Count(body, "kind = \"podman\""); count != 1 {
		t.Fatalf("%d podman kinds in %s", count, body)
	}
	entries, err := os.ReadDir(filepath.Join(directory, "nested"))
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if strings.HasSuffix(entry.Name(), ".tmp") {
			t.Fatalf("left %s behind", entry.Name())
		}
	}
}

func TestGithubAppConfigurationIsOptionalAndRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 1\n")
	legacy := configTestLoad(t, path)
	if !serdeEqual(legacy.Github, DefaultGithubConfig()) || !legacy.Github.Watch.Enabled || !legacy.GithubEventsEnabled() {
		t.Fatalf("legacy github %+v", legacy.Github)
	}
	configTestSave(t, legacy, path)
	configTestLacks(t, configTestReadFile(t, path), "[github")

	configured := DefaultConfig()
	configured.Github.App = &GithubAppConfig{
		AppID:              1234,
		PrivateKeyPath:     "/controller/keys/app.pem",
		Installations:      map[string]uint64{"Acme": 5678},
		SessionPermissions: &GithubPermissionSet{"contents": GithubPermissionLevelWrite},
		TokenPermissions:   &GithubPermissionSet{"statuses": GithubPermissionLevelRead},
	}
	configured.Github.SessionTokenPath = configTestPtr("~/.secrets/gh_agents_pat")
	configured.Github.SessionCredentials = false
	configured.Github.Watch.IntervalSeconds = 30
	configured.Github.Watch.APIBase = "http://127.0.0.1:9411"
	configured.Github.Watch.Enabled = false
	configured.Github.Watch.LocalInference = true
	configured.Github.Watch.UseGPU = false
	if configured.GithubEventsEnabled() {
		t.Fatal("events are off")
	}
	configTestSave(t, configured, path)
	body := configTestReadFile(t, path)
	for _, want := range []string{"[github.app]", "session_credentials = false", "[github.app.installations]",
		"[github.app.session_permissions]", "[github.app.token_permissions]", "[github.watch]", "[github.watch]\nenabled = false"} {
		configTestContains(t, body, want)
	}
	configTestLacks(t, body, "[mailbox]")
	serialized, err := parseTOMLDocument(body)
	if err != nil {
		t.Fatal(err)
	}
	if enabled := serialized["github"].(map[string]any)["watch"].(map[string]any)["enabled"]; enabled != false {
		t.Fatalf("watch enabled %v", enabled)
	}
	configTestAssertEqual(t, configTestLoad(t, path), configured)

	jevOff := DefaultConfig()
	jevOff.Jev.Enabled = false
	if jevOff.GithubEventsEnabled() {
		t.Fatal("Jev off turns events off")
	}

	for _, test := range []struct {
		legacy, current *bool
		expected        bool
	}{
		{configTestPtr(false), nil, false},
		{nil, configTestPtr(false), false},
		{configTestPtr(false), configTestPtr(true), false},
		{configTestPtr(true), configTestPtr(false), false},
		{configTestPtr(true), configTestPtr(true), true},
	} {
		// Automatic continuation was removed (#1532); its section still loads.
		body := "version = 17\n[continuation]\nenabled = false\n"
		if test.legacy != nil {
			body += fmt.Sprintf("[mailbox]\nenabled = %t\n", *test.legacy)
		}
		if test.current != nil {
			body += fmt.Sprintf("[github.watch]\nenabled = %t\n", *test.current)
		}
		configTestWrite(t, path, body)
		loaded := configTestLoad(t, path)
		if loaded.Github.Watch.Enabled != test.expected {
			t.Fatalf("%+v: watch enabled %v", test, loaded.Github.Watch.Enabled)
		}
		configTestSave(t, loaded, path)
		saved := configTestReadFile(t, path)
		configTestLacks(t, saved, "[mailbox]")
		configTestLacks(t, saved, "[continuation]")
		if configTestLoad(t, path).Github.Watch.Enabled != test.expected {
			t.Fatal(saved)
		}
		if !test.expected {
			configTestContains(t, saved, "[github.watch]\nenabled = false")
		}
	}

	configTestWrite(t, path, "version = 14\n[github.app]\napp_id = 0\nprivate_key_path = 'app.pem'\n")
	configTestContains(t, configTestLoadError(t, path), "app_id must be a positive integer")
}

func TestSaveReviewReloadsLatestConfigAndPreservesUnrelatedSections(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestSave(t, configTestSampleConfig(), path)

	// Simulate a concurrent dashboard changing an unrelated section after the
	// editor opened. The review save must start from this newer file.
	latest := configTestLoad(t, path)
	latest.Phone.Enabled = false
	latest.Daemon.Runtime = DaemonRuntimePodman
	latest.Daemon.Image = configTestPtr("example.test/daemon:fixture")
	latest.Daemon.Mounts = []string{"/srv/review-projects"}
	profile := latest.Profiles["codex-1"]
	profile.Environment.Insert("LATEST_SETTING", "kept")
	latest.Profiles["codex-1"] = profile
	configTestSave(t, latest, path)
	current := configTestReadFile(t, path)
	previousVersion := fmt.Sprintf("version = %d", ConfigVersion)
	configTestContains(t, current, previousVersion)
	configTestWrite(t, path, strings.Replace(current, previousVersion, "version = 20", 1))

	reviewer := SubagentChoice{Model: "review-model", Effort: configTestPtr("high")}
	review := ReviewConfig{
		Reviewers:   []SubagentChoice{reviewer, reviewer, reviewer},
		BriefWriter: &BriefWriterConfig{Enabled: true, Model: "brief-model", Effort: configTestPtr("high")},
	}
	saved, err := ConfigSaveReviewTo(path, review)
	if err != nil {
		t.Fatal(err)
	}
	if saved.Version != ConfigVersion {
		t.Fatalf("version %d", saved.Version)
	}
	if !serdeEqual(saved.Review, review) || len(saved.Review.Reviewers) != 3 {
		t.Fatalf("review %+v", saved.Review)
	}
	if !serdeEqual(saved.Daemon, latest.Daemon) || saved.Phone.Enabled {
		t.Fatal("unrelated sections changed")
	}
	if value, _ := saved.Profiles["codex-1"].Environment.Get("LATEST_SETTING"); value != "kept" {
		t.Fatalf("LATEST_SETTING %q", value)
	}
	configTestAssertEqual(t, configTestLoad(t, path), saved)
}

func TestUpdateToSerializesDisjointProcessEdits(t *testing.T) {
	const (
		child         = "HEL_CONFIG_UPDATE_CHILD"
		pathVariable  = "HEL_CONFIG_UPDATE_PATH"
		ready         = "HEL_CONFIG_UPDATE_READY"
		secondStarted = "HEL_CONFIG_UPDATE_SECOND_STARTED"
		release       = "HEL_CONFIG_UPDATE_RELEASE"
	)
	role, isChild := os.LookupEnv(child)
	if !isChild {
		directory := t.TempDir()
		path := filepath.Join(directory, "config.toml")
		configTestSave(t, configTestSampleConfig(), path)
		readyPath := filepath.Join(directory, "ready")
		secondStartedPath := filepath.Join(directory, "second-started")
		releasePath := filepath.Join(directory, "release")
		start := func(role string) *exec.Cmd {
			command := exec.Command(os.Args[0], "-test.run", "^TestUpdateToSerializesDisjointProcessEdits$", "-test.v")
			command.Env = append(os.Environ(),
				child+"="+role, pathVariable+"="+path, ready+"="+readyPath,
				secondStarted+"="+secondStartedPath, release+"="+releasePath)
			command.Stdout = os.Stdout
			command.Stderr = os.Stderr
			if err := command.Start(); err != nil {
				t.Fatal(err)
			}
			return command
		}
		waitFor := func(path string) bool {
			for range 1000 {
				if _, err := os.Stat(path); err == nil {
					return true
				}
				time.Sleep(10 * time.Millisecond)
			}
			_, err := os.Stat(path)
			return err == nil
		}
		first := start("phone")
		// The first child writes this only after it has acquired the sibling
		// lock and entered its edit closure.
		firstEntered := waitFor(readyPath)
		second := start("profile")
		secondReachedUpdate := waitFor(secondStartedPath)
		secondDone := make(chan error, 1)
		go func() { secondDone <- second.Wait() }()
		secondBlocked := true
		select {
		case err := <-secondDone:
			secondBlocked = false
			secondDone <- err
		default:
		}
		// Always release the first child before asserting, so a failed
		// observation cannot leave a child waiting after this test exits.
		configTestWrite(t, releasePath, "release")
		firstErr := first.Wait()
		secondErr := <-secondDone
		if !firstEntered {
			t.Fatal("first config update child never entered")
		}
		if !secondReachedUpdate {
			t.Fatal("second config update child never reached its update")
		}
		if !secondBlocked {
			t.Fatal("second config update child was not blocked")
		}
		if firstErr != nil {
			t.Fatalf("first config update child failed: %v", firstErr)
		}
		if secondErr != nil {
			t.Fatalf("second config update child failed: %v", secondErr)
		}
		config := configTestLoad(t, path)
		if config.Phone.Enabled {
			t.Fatal("the phone edit was lost")
		}
		if value, _ := config.Profiles["codex-1"].Environment.Get("CONCURRENT"); value != "kept" {
			t.Fatal("the profile edit was lost")
		}
		return
	}

	path := os.Getenv(pathVariable)
	readyPath := os.Getenv(ready)
	secondStartedPath := os.Getenv(secondStarted)
	releasePath := os.Getenv(release)
	if role == "profile" {
		// Confirm the first process owns the stable lock before telling the
		// parent it can release it. This cannot pass merely because the
		// second process was slow to reach ConfigUpdateTo.
		lock, err := os.OpenFile(configLockPath(path), os.O_RDWR, 0)
		if err != nil {
			t.Fatal(err)
		}
		locked, err := configTryLock(lock)
		if err != nil || locked {
			t.Fatalf("the lock is held by the first child: locked %v, %v", locked, err)
		}
		_ = lock.Close()
		configTestWrite(t, secondStartedPath, "started")
	}
	_, _, err := ConfigUpdateTo(path, func(config *Config) (struct{}, error) {
		switch role {
		case "phone":
			configTestWrite(t, readyPath, "entered")
			for {
				if _, err := os.Stat(releasePath); err == nil {
					break
				}
				time.Sleep(10 * time.Millisecond)
			}
			config.Phone.Enabled = false
		case "profile":
			profile := config.Profiles["codex-1"]
			profile.Environment.Insert("CONCURRENT", "kept")
			config.Profiles["codex-1"] = profile
		default:
			t.Fatalf("unknown config update child role %q", role)
		}
		return struct{}{}, nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestCancellableConfigEditCanLeaveAContendedLockWithoutSaving(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	guard, err := configLockAcquire(path)
	if err != nil {
		t.Fatal(err)
	}
	attempts := 0
	_, _, err = ConfigUpdateToCancellable(path, func() bool {
		attempts++
		return attempts > 1
	}, func(*Config) (struct{}, error) {
		panic("an edit cancelled while waiting must not start")
	})
	if err == nil || !strings.Contains(err.Error(), "file lock wait cancelled") {
		t.Fatalf("error %v", err)
	}
	if _, err := os.Stat(path); err == nil {
		t.Fatal("the cancelled edit saved")
	}
	if err := guard.Close(); err != nil {
		t.Fatal(err)
	}

	cancelled := false
	_, _, err = ConfigUpdateToCancellable(path, func() bool { return cancelled }, func(*Config) (struct{}, error) {
		cancelled = true
		return struct{}{}, nil
	})
	if err == nil || !strings.Contains(err.Error(), "configuration edit cancelled") {
		t.Fatalf("error %v", err)
	}
	if _, err := os.Stat(path); err == nil {
		t.Fatal("the cancelled edit saved")
	}
}

func TestUpdateToFailureLeavesThePreviousFileUnchanged(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestSave(t, configTestSampleConfig(), path)
	before := configTestReadFile(t, path)

	_, _, err := ConfigUpdateTo(path, func(config *Config) (struct{}, error) {
		config.Phone.Bind = "not-an-address"
		return struct{}{}, nil
	})
	if err == nil {
		t.Fatal("an invalid bind must fail")
	}
	configTestContains(t, err.Error(), "parse phone bind")
	if configTestReadFile(t, path) != before {
		t.Fatal("the file changed")
	}
}

func TestUpdateToRefusesANewerConfigBeforeEditing(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	body := fmt.Sprintf("version = %d\nfuture = true\n", ConfigVersion+1)
	configTestWrite(t, path, body)

	_, _, err := ConfigUpdateTo(path, func(config *Config) (struct{}, error) {
		config.Phone.Enabled = false
		return struct{}{}, nil
	})
	if err == nil {
		t.Fatal("a newer config must be refused")
	}
	configTestContains(t, err.Error(), "newer Mjolnir")
	if configTestReadFile(t, path) != body {
		t.Fatal("the file changed")
	}
}

func TestVersionOnePodmanConfigUpgradesToIsolatedWorkspaceStorage(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 1\n\n[targets.podman]\nkind = \"local-podman\"\nimage = \"ubuntu:24.04\"\n")

	config := configTestLoad(t, path)

	if config.Version != ConfigVersion {
		t.Fatalf("version %d", config.Version)
	}
	target, ok := config.Targets["podman"].(TargetTemplateLocalPodman)
	if !ok {
		t.Fatal("version-one Podman target changed kind")
	}
	if _, ok := target.ContainerTemplate.WorkspaceStorage.(PodmanWorkspaceStoragePodmanVolume); !ok {
		t.Fatalf("workspace storage %#v", target.ContainerTemplate.WorkspaceStorage)
	}
	if !strings.HasPrefix(configTestReadFile(t, path), "version = 1") {
		t.Fatal("load rewrote the file")
	}
}

func TestOldConfigRestoresScanWithoutRewritingUntilSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 2\n")

	config := configTestLoad(t, path)
	if config.Spinner != SpinnerStyleScan || config.Version != ConfigVersion {
		t.Fatalf("spinner %v version %d", config.Spinner, config.Version)
	}
	if configTestReadFile(t, path) != "version = 2\n" {
		t.Fatal("load rewrote the file")
	}
	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	if !strings.HasPrefix(saved, fmt.Sprintf("version = %d", ConfigVersion)) {
		t.Fatal(saved)
	}
	configTestLacks(t, saved, "spinner")
}

func TestEveryPreviousConfigVersionUpgradesWithCompatibleDefaults(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	for version := 1; version < ConfigVersion; version++ {
		configTestWrite(t, path, fmt.Sprintf("version = %d\n", version))
		config := configTestLoad(t, path)
		if config.Version != ConfigVersion {
			t.Fatalf("version %d became %d", version, config.Version)
		}
		if config.Theme != (ThemeChoiceFixed{Value: UiThemeMidnight}) {
			t.Fatalf("theme %#v", config.Theme)
		}
		if config.Advanced.DetailedActivityClocks {
			t.Fatal("detailed clocks default")
		}
	}
}

func TestThemePreferencesUpgradeAndRoundTripWithoutReplacingOtherSettings(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	old := "version = 4\nshow_stopped_sessions = false\n"
	configTestWrite(t, path, old)
	config := configTestLoad(t, path)
	if config.Theme != (ThemeChoiceFixed{Value: UiThemeMidnight}) || config.Version != ConfigVersion {
		t.Fatalf("theme %#v version %d", config.Theme, config.Version)
	}
	if configTestReadFile(t, path) != old {
		t.Fatal("load rewrote the file")
	}

	for _, theme := range ThemeChoiceAll() {
		_, _, err := ConfigUpdateTo(path, func(config *Config) (struct{}, error) {
			config.Theme = theme
			return struct{}{}, nil
		})
		if err != nil {
			t.Fatal(err)
		}
		expected := config
		expected.Theme = theme
		configTestAssertEqual(t, configTestLoad(t, path), expected)
	}
}

func TestLegacyDraculaThemeLoadsAndSavesAsDarcula(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\ntheme = \"dracula\"\n", ConfigVersion))

	config := configTestLoad(t, path)
	if config.Theme != (ThemeChoiceFixed{Value: UiThemeDarcula}) {
		t.Fatalf("theme %#v", config.Theme)
	}
	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	configTestContains(t, saved, "theme = \"darcula\"")
	configTestLacks(t, saved, "dracula")
}

func TestRawSSHPermissionsAreRequiredAndPodmanRejectsThem(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	config := configTestSampleConfig()
	container := config.Targets["podman-default"].(TargetTemplateLocalPodman).ContainerTemplate
	ssh := SshConnection{Host: "builder"}
	config.Targets = map[string]TargetTemplate{
		"builder-guardian": TargetTemplateSshBare{SSH: ssh, Permissions: PermissionModeGuardian, WorkspacePrefix: defaultNamedMachinePrefix()},
		"builder-yolo":     TargetTemplateSshBare{SSH: ssh, Permissions: PermissionModeYolo, WorkspacePrefix: defaultNamedMachinePrefix()},
		"builder-podman":   TargetTemplateSshPodman{SSH: ssh, ContainerTemplate: container},
	}

	configTestSave(t, config, path)

	body := configTestReadFile(t, path)
	configTestContains(t, body, "permissions = \"guardian\"")
	configTestContains(t, body, "permissions = \"yolo\"")
	if strings.Count(body, "permissions = ") != 2 {
		t.Fatal(body)
	}
	// Saving names the host the three runtimes share, so the file that comes
	// back has the machine the in-memory config never spelled out.
	config.Machines["builder"] = MachineSsh{SSH: SshConnection{Host: "builder"}, WorkspacePrefix: defaultNamedMachinePrefix()}
	if strings.Count(body, "[machines.builder]") != 1 {
		t.Fatal(body)
	}
	configTestAssertEqual(t, configTestLoad(t, path), config)

	configTestWrite(t, path, "version = 1\n[targets.builder]\nkind = \"ssh-bare\"\nhost = \"builder\"\n")
	configTestContains(t, configTestLoadError(t, path), "permissions")

	configTestWrite(t, path, "version = 1\n[targets.builder]\nkind = \"ssh-podman\"\nhost = \"builder\"\npermissions = \"guardian\"\nimage = \"example.invalid/agent:latest\"\n")
	configTestContains(t, configTestLoadError(t, path), "only applies to a bare runtime")
}

func TestMissingConfigUsesCleanV1Defaults(t *testing.T) {
	config := configTestLoad(t, filepath.Join(t.TempDir(), "missing.toml"))
	configTestAssertEqual(t, config, DefaultConfig())
	if !config.Phone.Enabled || !config.Phone.TailscaleDetect {
		t.Fatal("phone defaults")
	}
}

func TestOmittedPhoneFieldsEnableTheWebViewerAndTailscaleDetection(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 1\n[phone]\nbind = \"127.0.0.1:4765\"\n")

	config := configTestLoad(t, path)

	if !config.Phone.Enabled || !config.Phone.TailscaleDetect || config.Phone.Bind != "127.0.0.1:4765" {
		t.Fatalf("phone %+v", config.Phone)
	}
}

func TestVersionSevenProfilesUpgradeEnabledAndDisabledRoundTripsExplicitly(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 7\n[profiles.work]\nkind = \"codex\"\nhome = \"/profiles/work\"\n")

	config := configTestLoad(t, path)
	if config.Version != ConfigVersion || !config.Profiles["work"].Enabled {
		t.Fatal("upgraded profile")
	}
	var enabled []string
	for id := range config.EnabledProfiles() {
		enabled = append(enabled, id)
	}
	if !slices.Equal(enabled, []string{"work"}) {
		t.Fatalf("enabled %v", enabled)
	}

	configTestSave(t, config, path)
	body := configTestReadFile(t, path)
	if !strings.HasPrefix(body, fmt.Sprintf("version = %d", ConfigVersion)) {
		t.Fatal(body)
	}
	configTestLacks(t, body, "enabled = true")

	profile := config.Profiles["work"]
	profile.Enabled = false
	config.Profiles["work"] = profile
	configTestSave(t, config, path)
	configTestContains(t, configTestReadFile(t, path), "enabled = false")
	if configTestLoad(t, path).Profiles["work"].Enabled {
		t.Fatal("the profile is disabled")
	}
}

// A runtime switched off stays configured and off through every save, so
// detection, which adds only kinds a machine does not run yet, never adds it
// back. That includes a standard local runtime, which the file names only
// once the user switches it off.
func TestARuntimeSwitchedOffStaysOffThroughLoadsSavesAndUpdates(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	configTestWrite(t, path, "version = 18\n"+
		"[machines.box]\nkind = \"ssh\"\nhost = \"box.example.test\"\n"+
		"[targets.box]\nkind = \"bare\"\nmachine = \"box\"\nenabled = false\n"+
		"[targets.box-docker]\nkind = \"docker\"\nmachine = \"box\"\nimage = \"a:1\"\nenabled = false\n"+
		"[targets.box-podman]\nkind = \"podman\"\nmachine = \"box\"\nimage = \"a:1\"\nenabled = true\n")

	config := configTestLoad(t, path)
	if got := sortedKeys(config.DisabledTargets); !slices.Equal(got, []string{"box", "box-docker"}) {
		t.Fatalf("disabled %v", got)
	}
	var enabled []string
	for id := range config.EnabledTargets() {
		enabled = append(enabled, id)
	}
	if !slices.Equal(enabled, []string{"box-podman"}) {
		t.Fatalf("enabled %v", enabled)
	}
	_, err := config.EnsureTargetEnabled("box-docker")
	if err == nil {
		t.Fatal("a disabled runtime is refused")
	}
	configTestContains(t, err.Error(), "turned off")

	// A fresh write names the switch only where it is off.
	fresh := filepath.Join(directory, "fresh.toml")
	configTestSave(t, config, fresh)
	saved := configTestReadFile(t, fresh)
	if !strings.HasPrefix(saved, fmt.Sprintf("version = %d", ConfigVersion)) {
		t.Fatal(saved)
	}
	if strings.Count(saved, "enabled = false") != 2 {
		t.Fatal(saved)
	}
	configTestLacks(t, saved, "enabled = true")
	configTestAssertEqual(t, configTestLoad(t, fresh), config)

	if _, _, err := ConfigUpdateWithLocalTargetsAt(path, func(config *Config) (struct{}, error) {
		config.DisabledTargets["docker"] = struct{}{}
		return struct{}{}, nil
	}); err != nil {
		t.Fatal(err)
	}
	saved = configTestReadFile(t, path)
	configTestContains(t, saved, "[targets.docker]")
	configTestLacks(t, saved, "[targets.podman]")
	if _, ok := configTestLoad(t, path).WithLocalTargets().EnabledTarget("docker"); ok {
		t.Fatal("docker is off")
	}

	configTestWrite(t, path, fmt.Sprintf("version = %d\n[targets.x]\nkind = \"docker\"\nenabled = \"no\"\n", ConfigVersion))
	configTestContains(t, configTestLoadError(t, path), "write true or false")
}

// The global `[subagents] enabled` switch was removed; whether a session uses
// Mjolnir sub-agents is now stored per session. A config file left over from
// before the removal must still load, and a save must drop the key.
func TestALegacySubagentsEnabledKeyStillLoadsAndIsDroppedOnSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[subagents]\nenabled = true\n", ConfigVersion))

	config := configTestLoad(t, path)
	if config.Subagents.MaxConcurrent != 8 {
		t.Fatal(config.Subagents.MaxConcurrent)
	}

	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	configTestLacks(t, saved, "enabled")
	configTestLacks(t, saved, "[subagents]")
	if configTestLoad(t, path).Subagents.MaxConcurrent != 8 {
		t.Fatal("max concurrent")
	}

	// Other settings in the same section survive the same round trip.
	config.Subagents.MaxConcurrent = 4
	configTestSave(t, config, path)
	if configTestLoad(t, path).Subagents.MaxConcurrent != 4 {
		t.Fatal("max concurrent")
	}
	// A retained global section must drop the ignored key as well.
	configTestWrite(t, path, strings.ReplaceAll(configTestReadFile(t, path), "[subagents]", "[subagents]\nenabled = true"))
	configTestSave(t, configTestLoad(t, path), path)
	configTestLacks(t, configTestReadFile(t, path), "enabled")
}

func TestVersionEightEnablesParentOnlySubagentsByDefault(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 8\n[profiles.work]\nkind = \"codex\"\nhome = \"/profiles/work\"\n")

	config := configTestLoad(t, path)

	if config.Version != ConfigVersion || config.Subagents.MaxConcurrent != 8 {
		t.Fatalf("version %d max concurrent %d", config.Version, config.Subagents.MaxConcurrent)
	}
}

func configTestMachineCache(t *testing.T, machine Machine) *TargetBuildCache {
	t.Helper()
	if machine == nil {
		t.Fatal("no machine")
	}
	return machine.BuildCache()
}

func TestLegacyGlobalCacheOptOutIsSavedOnEachMachineWithoutOtherEdits(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	for _, version := range []int{12, 13} {
		source := fmt.Sprintf(`# Keep my configuration notes.
version = %d
[build_cache]
enabled = false
[machines.builder]
kind = "ssh"
host = "builder.example.com" # Keep this host note.
[machines.builder.build_cache]
enabled = true
max_total_size = "50GiB"
target_max_size = "10GB"
directory = "/cache"
[machines.other]
kind = "ssh"
host = "other.example.com"
[targets.podman]
kind = "podman"
[targets.remote]
kind = "docker"
machine = "builder"
`, version)
		configTestWrite(t, path, source)
		config := configTestLoad(t, path)
		if config.Version != ConfigVersion {
			t.Fatal(config.Version)
		}
		if configTestReadFile(t, path) != source {
			t.Fatal("load is read-only")
		}
		for _, id := range []string{"local", "builder", "other"} {
			cache := configTestMachineCache(t, config.Machines[id])
			if cache == nil || cache.Enabled == nil || *cache.Enabled {
				t.Fatalf("%s cache %+v", id, cache)
			}
		}
		cache := configTestMachineCache(t, config.Machines["builder"])
		if cache.MaxTotalSize == nil || *cache.MaxTotalSize != "50GiB" || cache.Directory == nil || *cache.Directory != "/cache" {
			t.Fatalf("builder cache %+v", cache)
		}
		for _, target := range config.Targets {
			switch target.(type) {
			case TargetTemplateLocalPodman, TargetTemplateSshDocker:
			default:
				t.Fatal("container runtime")
			}
			buildCache := target.Container().BuildCache
			if buildCache == nil || buildCache.Enabled == nil || *buildCache.Enabled {
				t.Fatalf("container cache %+v", buildCache)
			}
		}
		configTestSave(t, config, path)
		saved := configTestReadFile(t, path)
		configTestLacks(t, saved, "[build_cache]")
		configTestContains(t, saved, "# Keep my configuration notes.")
		configTestContains(t, saved, "# Keep this host note.")
		configTestAssertEqual(t, configTestLoad(t, path), config)
		configTestSave(t, config, path)
		if configTestReadFile(t, path) != saved {
			t.Fatal("a second save changed the file")
		}
	}
}

func TestCacheOptOutMigrationCoversImplicitLocalAndAllOlderConfigVersions(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	for version := 1; version <= 13; version++ {
		configTestWrite(t, path, fmt.Sprintf("version = %d\n[build_cache]\nenabled = false\n", version))
		config := configTestLoad(t, path)
		cache := configTestMachineCache(t, config.Machines["local"])
		if cache == nil || cache.Enabled == nil || *cache.Enabled {
			t.Fatalf("version %d local cache %+v", version, cache)
		}
		configTestSave(t, config, path)
		configTestAssertEqual(t, configTestLoad(t, path), config)
		// A newly added machine uses the normal default, not a hidden veto.
		machine, err := UnmarshalMachineJSON([]byte(`{"kind":"ssh", "host":"new.example.com"}`))
		if err != nil {
			t.Fatal(err)
		}
		config.Machines["new"] = machine
		configTestSave(t, config, path)
		if configTestMachineCache(t, configTestLoad(t, path).Machines["new"]) != nil {
			t.Fatal("a new machine has no cache settings")
		}
	}
}

func TestEnabledOrAbsentLegacyGlobalCacheKeepsMachinePolicy(t *testing.T) {
	for _, legacy := range []string{"", "[build_cache]\nenabled = true\n"} {
		config := configTestFromTOML(t, fmt.Sprintf(`version = 13
%s
[machines.off]
kind = "ssh"
host = "off.example.com"
[machines.off.build_cache]
enabled = false
[machines.on]
kind = "ssh"
host = "on.example.com"
[machines.on.build_cache]
enabled = true
`, legacy))
		if _, ok := config.Machines["local"]; ok {
			t.Fatal("no local machine")
		}
		if cache := configTestMachineCache(t, config.Machines["off"]); cache == nil || cache.Enabled == nil || *cache.Enabled {
			t.Fatalf("off cache %+v", cache)
		}
		if cache := configTestMachineCache(t, config.Machines["on"]); cache == nil || cache.Enabled == nil || !*cache.Enabled {
			t.Fatalf("on cache %+v", cache)
		}
		saved, err := marshalTOMLPretty(config)
		if err != nil {
			t.Fatal(err)
		}
		configTestLacks(t, saved, "[build_cache]")
		configTestAssertEqual(t, configTestFromTOML(t, saved), config)
	}
}

func TestLegacyGlobalCacheOptOutIsAppliedAfterFusedTargetConversion(t *testing.T) {
	config := configTestFromTOML(t, configTestVersionTenConfig+"\n[build_cache]\nenabled = false\n")
	for _, id := range []string{"local", "builder.example.com"} {
		if cache := configTestMachineCache(t, config.Machines[id]); cache == nil || cache.Enabled == nil || *cache.Enabled {
			t.Fatalf("%s cache %+v", id, cache)
		}
	}
	if cache := configTestMachineCache(t, config.Machines["local"]); cache.MaxTotalSize == nil || *cache.MaxTotalSize != "50GiB" {
		t.Fatalf("local cache %+v", cache)
	}
	if configTestMachineCache(t, config.Machines["aws"]) != nil {
		t.Fatal("an EC2 machine has no cache")
	}
	for _, target := range config.Targets {
		switch target.(type) {
		case TargetTemplateLocalPodman, TargetTemplateLocalDocker, TargetTemplateSshPodman:
			cache := target.Container().BuildCache
			if cache == nil || cache.Enabled == nil || *cache.Enabled {
				t.Fatalf("container cache %+v", cache)
			}
		}
	}
	saved, err := marshalTOMLPretty(config)
	if err != nil {
		t.Fatal(err)
	}
	configTestAssertEqual(t, configTestFromTOML(t, saved), config)
}

func TestSavingCanReenableOneMachineDuringGlobalCacheMigration(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 13\n[build_cache]\nenabled = false\n")
	config := configTestLoad(t, path)
	config.Machines["local"] = MachineLocal{}
	configTestSave(t, config, path)
	configTestLacks(t, configTestReadFile(t, path), "build_cache")
	if _, ok := configTestLoad(t, path).Machines["local"]; ok {
		t.Fatal("the implied local machine is not written")
	}
}

func TestLegacyBuildBudgetsAreIgnoredWithoutChangingHostPlacement(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	for _, legacy := range []map[string]any{
		{"max_size": "500GiB", "target_max_size": "250GiB"},
		{"max_size": []any{false}, "target_max_size": map[string]any{"invalid": true}},
	} {
		settings := legacy
		settings["enabled"] = false
		settings["directory"] = "/cache"
		data, err := serdeJSON(settings)
		if err != nil {
			t.Fatal(err)
		}
		var cache TargetBuildCache
		if err := cache.UnmarshalJSON(data); err != nil {
			t.Fatal(err)
		}
		if err := cache.validate("builder"); err != nil {
			t.Fatal(err)
		}
		if cache.Enabled == nil || *cache.Enabled || cache.Directory == nil || *cache.Directory != "/cache" || cache.MaxTotalSize != nil {
			t.Fatalf("cache %+v", cache)
		}
		source := configTestTOML(t, map[string]any{
			"version":  ConfigVersion,
			"machines": map[string]any{"builder": map[string]any{"kind": "ssh", "host": "builder.example.com", "build_cache": settings}},
		})
		configTestWrite(t, path, source)
		config := configTestLoad(t, path)
		if !serdeEqual(config.Machines["builder"].BuildCache(), &cache) {
			t.Fatalf("machine cache %+v", config.Machines["builder"].BuildCache())
		}
		serialized, err := serdeJSON(cache)
		if err != nil {
			t.Fatal(err)
		}
		configTestLacks(t, string(serialized), "max_size")
		configTestLacks(t, string(serialized), "target_max_size")
		settings["max_total_size"] = "2TB"
		data, _ = serdeJSON(settings)
		var reset TargetBuildCache
		if err := reset.UnmarshalJSON(data); err != nil {
			t.Fatal(err)
		}
		if reset.MaxTotalSize == nil || *reset.MaxTotalSize != "2TB" {
			t.Fatalf("reset %+v", reset)
		}
	}
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[machines.builder]\nkind = \"ssh\"\n"+
		"host = \"builder.example.com\"\n[machines.builder.build_cache]\nmax_totl_size = \"2TB\"\n", ConfigVersion))
	configTestContains(t, configTestLoadError(t, path), "`machines.builder.build_cache.max_totl_size`")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[targets.cached]\nkind = \"podman\"\n"+
		"[targets.cached.build_cache]\nenabeld = true\n", ConfigVersion))
	configTestContains(t, configTestLoadError(t, path), "`targets.cached.build_cache.enabeld`")
	// A kind without the setting never read the table, so it still loads.
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[machines.ec2]\nkind = \"aws-ec2\"\n"+
		"region = \"us-east-1\"\nlaunch_template = \"lt\"\nssh_user = \"ubuntu\"\n"+
		"[machines.ec2.build_cache]\nold_knob = 1\n"+
		"[targets.plain]\nkind = \"bare\"\n[targets.plain.build_cache]\nold_knob = 1\n", ConfigVersion))
	configTestLoad(t, path)
}

func TestLegacySubagentProfileAllowlistLoadsAndIsDroppedOnSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[subagents.eligible_profiles]\nmissing = true\n", ConfigVersion))
	config := configTestLoad(t, path)
	configTestSave(t, config, path)
	configTestLacks(t, configTestReadFile(t, path), "eligible_profiles")
}

// Hard-won: dcc60664: a disabled eligible profile stopped every mj command
func TestADisabledProfileLoadsInsteadOfFailing(t *testing.T) {
	// Disabled profiles do not stop the daemon from starting.
	config := configTestFromTOML(t, fmt.Sprintf("version = %d\n"+
		"[subagents.eligible_profiles]\nwork = true\n"+
		"[profiles.work]\nenabled = false\nkind = \"grok\"\nhome = \"/profiles/work\"\n", ConfigVersion))
	if err := config.Validate(); err != nil {
		t.Fatal(err)
	}
	if config.Profiles["work"].Enabled {
		t.Fatal("the profile is disabled")
	}
}

func configTestSessionReview(t *testing.T, text string) SessionReview {
	t.Helper()
	review, err := UnmarshalSessionReviewJSON([]byte(text))
	if err != nil {
		t.Fatalf("%s: %v", text, err)
	}
	return review
}

func TestLegacyReviewKeysAndSessionRecordsPreservePartialOverrides(t *testing.T) {
	var legacy ReviewConfig
	if err := unmarshalTOML("trigger = 'turn'\ncount = 4\nprofile = 'old'\nmodel = 'gpt-6-terra'\neffort = 'high'\n", &legacy); err != nil {
		t.Fatal(err)
	}
	if len(legacy.Reviewers) != MaxReviewers {
		t.Fatal(len(legacy.Reviewers))
	}
	configTestLacks(t, configTestTOML(t, legacy), "profile")
	session := configTestSessionReview(t, `{"mode":"turn","count":3,"model":"gpt-6-terra","effort":"high","tier":"old"}`)
	if got := len(legacy.ForSession(session).Reviewers); got != 3 {
		t.Fatal(got)
	}
	saved, err := serdeJSON(session)
	if err != nil {
		t.Fatal(err)
	}
	configTestLacks(t, string(saved), "reviewers")
	configTestContains(t, string(saved), "count")
	configTestLacks(t, string(saved), "tier")
	countOnly := configTestSessionReview(t, `{"mode":"turn","count":1}`)
	if got := len(legacy.ForSession(countOnly).Reviewers); got != 1 {
		t.Fatal(got)
	}
	effortOnly := configTestSessionReview(t, `{"mode":"turn","effort":"low"}`)
	for _, reviewer := range legacy.ForSession(effortOnly).Reviewers {
		if reviewer.Effort == nil || *reviewer.Effort != "low" {
			t.Fatalf("reviewer %+v", reviewer)
		}
	}
	modelOnly := configTestSessionReview(t, `{"mode":"turn","model":"other"}`)
	for _, reviewer := range legacy.ForSession(modelOnly).Reviewers {
		if reviewer.Model != "other" || reviewer.Effort == nil || *reviewer.Effort != "high" {
			t.Fatalf("reviewer %+v", reviewer)
		}
	}
	autoConfig := ReviewConfigLegacyDefault()
	effective := autoConfig.ForSession(modelOnly)
	if effective.IsLegacy() || len(effective.Reviewers) != 2 {
		t.Fatalf("effective %+v", effective)
	}
	pending := []SubagentChoice{{Model: "default"}, {Model: "default"}}
	if got := len(ApplyLegacyOverride(pending, configTestPtr(1), nil, nil, autoConfig)); got != 1 {
		t.Fatal(got)
	}
	for _, choice := range ApplyLegacyOverride(pending, nil, nil, configTestPtr("high"), autoConfig) {
		if choice.Effort == nil || *choice.Effort != "high" {
			t.Fatalf("choice %+v", choice)
		}
	}
	if encoded, _ := serdeJSON(countOnly); string(encoded) != `{"mode":"push","count":1}` {
		t.Fatal(string(encoded))
	}
	var auto ReviewConfig
	if err := unmarshalTOML("enabled = true\ncount = 2\n", &auto); err != nil {
		t.Fatal(err)
	}
	if len(auto.Reviewers) != 0 {
		t.Fatal(auto.Reviewers)
	}
}

func TestExplicitWebViewerOptOutSurvivesSerialization(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	config := DefaultConfig()
	config.Phone.Enabled = false
	config.Phone.TailscaleDetect = false

	configTestSave(t, config, path)
	body := configTestReadFile(t, path)

	configTestContains(t, body, "enabled = false")
	configTestContains(t, body, "tailscale_detect = false")
	configTestAssertEqual(t, configTestLoad(t, path), config)
}

func TestPhoneConfigRequiresTLSOffLoopbackAndCompleteKeyPairs(t *testing.T) {
	config := DefaultConfig()
	config.Phone.Enabled = true
	config.Phone.Bind = "0.0.0.0:3765"
	configTestContains(t, configTestValidateError(t, config), "TLS")

	config.Phone.TLSCert = configTestPtr("certificate.pem")
	configTestContains(t, configTestValidateError(t, config), "both")
	config.Phone.TLSKey = configTestPtr("private-key.pem")
	if err := config.Validate(); err != nil {
		t.Fatal(err)
	}
}

func TestEmptyConfigUsesCleanV1Defaults(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "\n\t")
	loaded := configTestLoad(t, path)
	if loaded.Review.IsLegacy() || loaded.Review.Trigger() != ReviewTriggerOff {
		t.Fatal("review is off")
	}
}

func TestLegacyAutoSurvivesUnrelatedSavesAndJSONDrafts(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[review]\ntrigger = 'turn'\n", ConfigVersion))
	config := configTestLoad(t, path)
	if !config.Review.IsLegacy() {
		t.Fatal("legacy review")
	}
	config.Phone.Enabled = false
	configTestSave(t, config, path)
	configTestLacks(t, configTestReadFile(t, path), "reviewers")
	data, err := serdeJSON(config)
	if err != nil {
		t.Fatal(err)
	}
	configTestLacks(t, string(data), "reviewers")
	var decoded Config
	if err := decoded.UnmarshalJSON(data); err != nil {
		t.Fatal(err)
	}
	if !decoded.Review.IsLegacy() {
		t.Fatal("a JSON draft keeps the pending review")
	}
	config.Review.ClearLegacy()
	configTestSave(t, config, path)
	configTestContains(t, configTestReadFile(t, path), "reviewers = []")
	if configTestLoad(t, path).Review.IsLegacy() {
		t.Fatal("no pending review")
	}
}

func TestLegacyProfileCountAndEffortSurviveUnrelatedSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[review]\ntrigger = 'push'\ncount = 3\nprofile = 'claude'\neffort = 'high'\n", ConfigVersion))
	config := configTestLoad(t, path)
	if !config.Review.IsLegacy() {
		t.Fatal("legacy review")
	}
	config.Phone.Enabled = false
	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	for _, want := range []string{"count = 3", "profile = ", "effort = "} {
		configTestContains(t, saved, want)
	}
	configTestLacks(t, saved, "reviewers")
	if !serdeEqual(configTestLoad(t, path).Review, config.Review) {
		t.Fatal("review changed")
	}
}

func TestANewerConfigIsRefusedWithoutTouchingTheFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	body := fmt.Sprintf("version = %d\nsetting_from_the_future = true\n", ConfigVersion+1)
	configTestWrite(t, path, body)

	configTestContains(t, configTestLoadError(t, path), "newer Mjolnir")
	if configTestReadFile(t, path) != body {
		t.Fatal("the file changed")
	}
}

func TestANewerConfigWrittenAfterLoadStillBlocksASave(t *testing.T) {
	// Another Hel may upgrade the file between this build's load and its
	// save; the save must re-check the file rather than trust its marker.
	path := filepath.Join(t.TempDir(), "config.toml")
	config := configTestSampleConfig()
	configTestSave(t, config, path)

	body := fmt.Sprintf("version = %d\n", ConfigVersion+1)
	configTestWrite(t, path, body)

	err := config.SaveTo(path)
	if err == nil {
		t.Fatal("a newer file blocks the save")
	}
	configTestContains(t, err.Error(), "newer Mjolnir")
	if configTestReadFile(t, path) != body {
		t.Fatal("the file changed")
	}
}

// A hand-written config.toml without its `version` line stopped `mj` with the
// raw TOML error "missing field `version`", which did not say what to add
// (launch finding R14-4, reverify-14 tmux/026).
// Hard-won: 0870826d: R14-4 left hand-written config users with a generic missing-version error
func TestAConfigWithoutAVersionNamesTheLineToAdd(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "[profiles.codex]\nkind = \"codex\"\nhome = \"/nonexistent\"\n")

	want := fmt.Sprintf("%s: config.toml needs a `version = %d` line at the top (the current "+
		"configuration schema); see https://mjolnir.brokk.ai/configuration/", path, ConfigVersion)
	if got := configTestLoadError(t, path); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}

	// Any other parse error keeps the parser's words after the file's path.
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[profiles.codex\n", ConfigVersion))
	_, err := ConfigLoadFrom(path)
	var loadError *ConfigLoadErrorToml
	if !errors.As(err, &loadError) || loadError.Location == nil || *loadError.Location != [2]int{2, 16} {
		t.Fatalf("load error %#v", err)
	}
	message := err.Error()
	if !strings.HasPrefix(message, fmt.Sprintf("parse Mjolnir config %s: ", path)) {
		t.Fatal(message)
	}
	configTestContains(t, message, "TOML parse error")
}

func TestRemovedProfileOverridesHaveAnActionableError(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, "version = 1\n[profiles.codex]\nkind = \"codex\"\nhome = \"/tmp/codex\"\nmodel = \"gpt-old\"\n")
	message := configTestLoadError(t, path)
	configTestContains(t, message, "`model` is no longer supported")
	configTestContains(t, message, "/config")
}

// A config an earlier release wrote can hold a MiMo Code profile, possibly
// chosen as the reviewer. The file must still load with every other profile;
// the reviewer model remains usable through another enabled profile.
func TestAProfileForARemovedHarnessIsDroppedWithoutFailingTheConfig(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n"+
		"[review]\ntrigger = \"turn\"\nprofile = \"mimo\"\nmodel = \"mimo-v2\"\n"+
		"[profiles.mimo]\nkind = \"mimo\"\nhome = \"/tmp/mimocode\"\n"+
		"[profiles.codex]\nkind = \"codex\"\nhome = \"/tmp/codex\"\n", ConfigVersion))

	loaded := configTestLoad(t, path)
	if got := sortedKeys(loaded.Profiles); !slices.Equal(got, []string{"codex"}) {
		t.Fatalf("profiles %v", got)
	}
	if loaded.Review.Trigger() != ReviewTriggerPush || loaded.Review.Reviewers[0].Model != "mimo-v2" {
		t.Fatalf("review %+v", loaded.Review)
	}

	configTestSave(t, loaded, path)
	saved := configTestReadFile(t, path)
	configTestContains(t, saved, "mimo-v2")
	configTestLacks(t, saved, "profile = \"mimo\"")
}

func TestProfileCannotOverrideItsIsolatedHome(t *testing.T) {
	config := configTestSampleConfig()
	profile := config.Profiles["codex-1"]
	profile.Environment.Insert("CODEX_HOME", "/shared-and-racy")
	config.Profiles["codex-1"] = profile
	configTestContains(t, configTestValidateError(t, config), "must use `home`")
}

// Hard-won: 02e00ba4: named instances collided on the default viewer port and API commands stopped
func TestNamedInstancesDefaultToTheirOwnStableViewerPort(t *testing.T) {
	if got := DefaultPhoneBindFor(nil); got != "127.0.0.1:3765" {
		t.Fatal(got)
	}
	port := func(name string) uint16 {
		t.Helper()
		bind, err := netip.ParseAddrPort(DefaultPhoneBindFor(&name))
		if err != nil {
			t.Fatal(err)
		}
		if !bind.Addr().IsLoopback() {
			t.Fatalf("%s binds beyond loopback", name)
		}
		return bind.Port()
	}
	launch := port("launch-i1")
	if launch != port("launch-i1") {
		t.Fatal("the default must survive restarts")
	}
	if launch < InstanceViewerPortsStart || launch > InstanceViewerPortsEnd {
		t.Fatalf("%d is outside the documented range", launch)
	}
	if launch == 3765 || port("dev") == port("dev-2") {
		t.Fatal("ports collide")
	}
}

// configTestVersionTenConfig is the version 10 file from the plan's
// acceptance step, and what saving it writes back.
const configTestVersionTenConfig = `version = 10

[targets.localhost]
kind = "local-bare"

[targets.podman]
kind = "local-podman"
image = "example.invalid/agent:latest"

[targets.podman.build_cache]
max_total_size = "50GiB"

[targets.docker]
kind = "local-docker"
image = "example.invalid/agent:latest"

[targets.builder]
kind = "ssh-bare"
host = "builder.example.com"
permissions = "guardian"

[targets.builder-podman]
kind = "ssh-podman"
host = "builder.example.com"
image = "example.invalid/agent:latest"

[targets.aws]
kind = "aws-ec2"
region = "us-east-1"
launch_template = "lt-0123"
ssh_user = "ubuntu"
`

func TestAVersionTenConfigBecomesMachinesAndRuntimesOnTheNextSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, configTestVersionTenConfig)

	config := configTestLoad(t, path)
	if _, ok := config.EnabledTarget("aws"); ok {
		t.Fatal("an EC2 target is not available")
	}
	for id := range config.EnabledTargets() {
		if id == "aws" {
			t.Fatal("an EC2 target is not enabled")
		}
	}
	_, err := config.EnsureTargetEnabled("aws")
	if err == nil {
		t.Fatal("an EC2 target is refused")
	}
	configTestContains(t, err.Error(), LegacyEC2TargetNotice)
	if config.Version != ConfigVersion {
		t.Fatal(config.Version)
	}
	if got := sortedKeys(config.Machines); !slices.Equal(got, []string{"aws", "builder.example.com", "local"}) {
		t.Fatalf("machines %v", got)
	}
	cache := TargetBuildCache{MaxTotalSize: configTestPtr("50GiB")}
	if !serdeEqual(config.Machines["local"], MachineLocal{TargetBuildCache: &cache}) {
		t.Fatalf("local machine %#v", config.Machines["local"])
	}
	// Both local container runtimes now share the one host cache.
	for _, id := range []string{"podman", "docker"} {
		switch config.Targets[id].(type) {
		case TargetTemplateLocalPodman, TargetTemplateLocalDocker:
		default:
			t.Fatalf("%s changed kind", id)
		}
		if !serdeEqual(config.Targets[id].Container().BuildCache, &cache) {
			t.Fatalf("%s cache", id)
		}
	}
	if _, ok := config.Targets["localhost"].(TargetTemplateLocalBare); !ok {
		t.Fatal("localhost")
	}
	if builder, ok := config.Targets["builder"].(TargetTemplateSshBare); !ok || builder.Permissions != PermissionModeGuardian {
		t.Fatal("builder")
	}
	if _, ok := config.Targets["aws"].(TargetTemplateAwsEc2); !ok {
		t.Fatal("aws")
	}

	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	t.Log(saved)
	if !strings.HasPrefix(saved, fmt.Sprintf("version = %d", ConfigVersion)) {
		t.Fatal(saved)
	}
	for _, expected := range []string{
		"[machines.local]",
		"[machines.local.build_cache]",
		"max_total_size = \"50GiB\"",
		"[machines.\"builder.example.com\"]",
		"kind = \"ssh\"",
		"host = \"builder.example.com\"",
		"[machines.aws]",
		"kind = \"aws-ec2\"",
		"[targets.localhost]\nkind = \"bare\"\n",
		"[targets.podman]\nkind = \"podman\"\n",
		"machine = \"builder.example.com\"",
	} {
		configTestContains(t, saved, expected)
	}
	configTestLacks(t, saved, "local-podman")
	if strings.Count(saved, "build_cache") != 1 {
		t.Fatal(saved)
	}
	// A second load of the rewritten file is the same configuration.
	configTestAssertEqual(t, configTestLoad(t, path), config)
}

func TestAnAppleContainerRuntimeNamesAnSSHMachineAndSurvivesARoundTrip(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	configTestWrite(t, path, fmt.Sprintf(`version = %d
[machines.mac]
kind = "ssh"
host = "mac.example.com"
[targets.mac-apple]
kind = "apple-container"
machine = "mac"
image = "example.invalid/agent:latest"
[machines.ec2]
kind = "ssh"
host = "ec2.example.com"
[targets.ec2-nitro]
kind = "nitro"
machine = "ec2"
`, ConfigVersion))
	config := configTestLoad(t, path)
	// A Nitro sandbox is the same shape: a stored `nitro` runtime on an SSH
	// machine fuses into the remote kind, and a saved file never mentions it.
	nitro, ok := config.Targets["ec2-nitro"].(TargetTemplateSshNitro)
	if !ok {
		t.Fatal("nitro on an ssh machine must fuse into the remote kind")
	}
	if nitro.SSH.Host != "ec2.example.com" || nitro.ContainerTemplate.Image != DefaultSessionImage {
		t.Fatalf("nitro %+v", nitro)
	}
	if RuntimeKindName(nitro) != "nitro" || nitro.KindName() != "ssh-nitro" {
		t.Fatal("nitro kinds")
	}
	if nitro.ExecutionPolicy() != ExecutionPolicyUnconstrained || !ContainerMemoryIsHardLimit(nitro) {
		t.Fatal("nitro policy")
	}
	// No machine can host a sandbox but an SSH one.
	local := filepath.Join(directory, "local.toml")
	configTestWrite(t, local, fmt.Sprintf("version = %d\n[targets.here]\nkind = \"nitro\"\nmachine = \"local\"\n", ConfigVersion))
	configTestContains(t, configTestLoadError(t, local), "runs on an SSH machine")
	apple, ok := config.Targets["mac-apple"].(TargetTemplateSshAppleContainer)
	if !ok {
		t.Fatal("apple-container on an ssh machine must fuse into the remote kind")
	}
	if apple.SSH.Host != "mac.example.com" || apple.ContainerTemplate.Image != "example.invalid/agent:latest" {
		t.Fatalf("apple %+v", apple)
	}
	// The runtime half is still `apple-container`, so a saved file never
	// mentions the fused kind.
	if RuntimeKindName(apple) != "apple-container" {
		t.Fatal("apple kind")
	}
	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	configTestContains(t, saved, "kind = \"apple-container\"")
	configTestContains(t, saved, "machine = \"mac\"")
	configTestLacks(t, saved, "ssh-apple-container")
	configTestContains(t, saved, "kind = \"nitro\"")
	configTestLacks(t, saved, "ssh-nitro")
	if !serdeEqual(configTestLoad(t, path).Targets, config.Targets) {
		t.Fatal("targets changed")
	}
}

// The engine is the same whether it runs here or over SSH, so the remote
// runtime accepts and refuses exactly what the local one does.
func TestARemoteAppleContainerTargetValidatesLikeTheLocalOne(t *testing.T) {
	parse := func(kind, extra string) TargetTemplate {
		t.Helper()
		document, err := parseTOMLDocument(fmt.Sprintf("kind = \"%s\"\nhost = \"mac.example.com\"\nimage = \"example.invalid/agent:latest\"\n%s", kind, extra))
		if err != nil {
			t.Fatal(err)
		}
		data, err := serdeJSON(document)
		if err != nil {
			t.Fatal(err)
		}
		target, err := UnmarshalTargetTemplateJSON(data)
		if err != nil {
			t.Fatal(err)
		}
		return target
	}
	for _, test := range []struct{ kind, id string }{{"ssh-apple-container", "mac-apple"}, {"apple-container", "apple"}} {
		target := parse(test.kind, "")
		if err := target.validate(test.id); err != nil {
			t.Fatal(err)
		}
		if target.Container() == nil {
			t.Fatalf("%s has a container", test.kind)
		}
		// Apple's runtime keeps its workspace inside its VM disk image, so
		// Podman's workspace-storage override is refused on both.
		err := parse(test.kind, "workspace_storage = { kind = \"container-layer\" }").validate(test.id)
		if err == nil {
			t.Fatalf("%s accepts workspace storage", test.kind)
		}
		configTestContains(t, err.Error(), "only supported by Podman")
		// Both accept `platform`, which only reaches the engine as
		// `--platform`.
		if err := parse(test.kind, "platform = \"linux/arm64\"").validate(test.id); err != nil {
			t.Fatalf("%s: %v", test.kind, err)
		}
	}
}

func TestTheCurrentVersionRefusesTheOldFusedKinds(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	// Version 11 is the last one that may still name a fused kind, because
	// that version belongs to the key-binding change rather than this split.
	configTestWrite(t, path, "version = 11\n[targets.podman]\nkind = \"local-podman\"\nimage = \"a:1\"\n")
	if _, ok := configTestLoad(t, path).Targets["podman"].(TargetTemplateLocalPodman); !ok {
		t.Fatal("version 11 reads the fused kind")
	}

	configTestWrite(t, path, fmt.Sprintf("version = %d\n[targets.podman]\nkind = \"local-podman\"\nimage = \"a:1\"\n", ConfigVersion))
	message := configTestLoadError(t, path)
	for _, want := range []string{"\"podman\"", "local-podman", "machine"} {
		configTestContains(t, message, want)
	}
}

// A single build compiles one channel, so the rule both channels follow is
// checked directly here, and the baked value is checked against it.
func TestTheBakedReleaseImageAgreesWithTheChannelRule(t *testing.T) {
	// A Go development build bakes nothing: it names the floating image, as
	// ContainerImageFor does for a development build.
	if ReleaseContainerImage != ContainerImageFor("ignored", false) &&
		!strings.HasPrefix(ReleaseContainerImage, ContainerImageRepository+":") {
		t.Fatalf("release image %q", ReleaseContainerImage)
	}
}

func TestAReleasePinsItsVersionAndADevelopmentBuildUsesLatest(t *testing.T) {
	const version = "9.9.9"
	if got := ContainerImageFor(version, true); got != ContainerImageRepository+":"+version {
		t.Fatal(got)
	}
	if got := ContainerImageFor(version, false); got != LegacyDefaultContainerImage {
		t.Fatal(got)
	}
}

func configTestContainerTarget(image string) TargetTemplate {
	return TargetTemplateLocalDocker{ContainerTemplate: ContainerTemplate{
		Image:            image,
		PullPolicy:       ImagePullPolicyAuto,
		WorkspaceStorage: PodmanWorkspaceStoragePodmanVolume{},
	}}
}

func TestMigratingTheLegacyDefaultImageRewritesOnlyTheDefault(t *testing.T) {
	sessionDefault := DefaultSessionImage
	defaultTarget := configTestContainerTarget(LegacyDefaultContainerImage)
	migrateLegacyDefaultImage(&defaultTarget, sessionDefault)
	if defaultTarget.Container().Image != sessionDefault {
		t.Fatal(defaultTarget.Container().Image)
	}

	custom := configTestContainerTarget("example.invalid/agent:1")
	migrateLegacyDefaultImage(&custom, sessionDefault)
	if custom.Container().Image != "example.invalid/agent:1" {
		t.Fatal(custom.Container().Image)
	}
}

// A file written before the running build's default image existed must
// follow that build instead of pinning the image that was current when it was
// written.
func TestAVersion14FileFollowsTheRunningBuildsDefaultImage(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = 14\n"+
		"[targets.podman]\nkind = \"podman\"\nimage = \"%s\"\n"+
		"[targets.custom]\nkind = \"docker\"\nimage = \"example.invalid/agent:1\"\n", LegacyDefaultContainerImage))
	config := configTestLoad(t, path)
	if config.Version != ConfigVersion || config.Targets["podman"].Container().Image != DefaultSessionImage {
		t.Fatal("the legacy default image follows the build")
	}
	configTestSave(t, config, path)
	saved := configTestReadFile(t, path)
	configTestContains(t, saved, fmt.Sprintf("version = %d", ConfigVersion))
	// The default is implicit now; only the customized image stays pinned.
	configTestLacks(t, saved, fmt.Sprintf("image = \"%s\"", LegacyDefaultContainerImage))
	configTestContains(t, saved, "example.invalid/agent:1")
}

func TestLegacyProfileSubagentsLoadWithoutMigrationAndDisappearOnSave(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[profiles.legacy]\nkind = \"codex\"\nhome = %s\nsubagents = { mode = \"single_model\", model = \"old\", effort = \"adaptive\" }\n",
		ConfigVersion, debugString(root)))
	config := configTestLoad(t, path)
	if config.Subagents.IsConfigured() {
		t.Fatal("legacy choices are not migrated")
	}
	configTestSave(t, config, path)
	configTestLacks(t, configTestReadFile(t, path), "subagents")
	data, err := serdeJSON(config)
	if err != nil {
		t.Fatal(err)
	}
	var value map[string]any
	if err := unmarshalJSONGeneric(data, &value); err != nil {
		t.Fatal(err)
	}
	value["subagents"] = map[string]any{
		"straightforward": map[string]any{"model": "chosen", "effort": "adaptive"},
		"subtle":          map[string]any{"model": "chosen"},
	}
	data, _ = serdeJSON(value)
	var invalid Config
	if err := invalid.UnmarshalJSON(data); err != nil {
		t.Fatal(err)
	}
	configTestContains(t, configTestValidateError(t, invalid), "adaptive")
}

func TestSubtleSubagentsAreOptionalButNeverConfiguredAlone(t *testing.T) {
	choice := func(model string) SubagentChoice { return SubagentChoice{Model: model} }
	config := DefaultConfig()
	config.Subagents.Straightforward = []SubagentChoice{choice("plain")}
	if err := config.Validate(); err != nil {
		t.Fatal(err)
	}
	if !config.Subagents.IsConfigured() {
		t.Fatal("straightforward alone is configured")
	}
	policy := SubagentPolicy{Mode: SubagentPolicyMjolnir}
	if config.Subagents.OffersSubtle(policy) {
		t.Fatal("no subtle choice")
	}
	// A session override can add the subtle type for one session.
	smart := choice("smart")
	withSubtle := SubagentPolicy{Mode: SubagentPolicyMjolnir, Subtle: &smart}
	if !config.Subagents.OffersSubtle(withSubtle) {
		t.Fatal("the session's subtle choice")
	}
	config.Subagents.Subtle = []SubagentChoice{choice("smart")}
	if err := config.Validate(); err != nil {
		t.Fatal(err)
	}
	if !config.Subagents.OffersSubtle(policy) {
		t.Fatal("the global subtle choice")
	}

	config.Subagents.Straightforward = nil
	if config.Subagents.IsConfigured() {
		t.Fatal("subtle alone is not configured")
	}
	configTestContains(t, configTestValidateError(t, config), "[subagents.subtle] requires [subagents.straightforward]")
}

func TestSubagentConfigAcceptsSingularAndArrayAndDropsLegacyYolo(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	configTestWrite(t, path, `
version = 23

[subagents]
use_same_on_all_targets = false
max_concurrent = 8

[subagents.straightforward]
model = "gpt-5.4-mini"
effort = "high"

[[subagents.primary]]
model = "claude-opus-4-6"
effort = "high"

[[subagents.primary]]
model = "gpt-5.4"
effort = "xhigh"

[subagents.yolo.straightforward]
model = "kimi-k2.5"
`)
	loaded := configTestLoad(t, path)
	if loaded.Subagents.MaxConcurrent != 8 {
		t.Fatalf("max concurrent %d", loaded.Subagents.MaxConcurrent)
	}
	if len(loaded.Subagents.Straightforward) != 1 || loaded.Subagents.Straightforward[0].Model != "gpt-5.4-mini" {
		t.Fatalf("straightforward %+v", loaded.Subagents.Straightforward)
	}
	if len(loaded.Subagents.Primary) != 2 {
		t.Fatalf("primary %+v", loaded.Subagents.Primary)
	}
	if !loaded.Subagents.PrimaryConfigured() || !loaded.Subagents.IsConfigured() {
		t.Fatal("configured")
	}
	configTestSave(t, loaded, path)
	saved := configTestReadFile(t, path)
	configTestLacks(t, saved, "yolo")
	configTestLacks(t, saved, "use_same_on_all_targets")
	configTestContains(t, saved, "version = 24")
}

func TestSubagentConfigRejectsSameProviderInOneCategory(t *testing.T) {
	config := DefaultConfig()
	config.Subagents.Primary = []SubagentChoice{
		{Model: "claude-opus-4-6", Effort: configTestPtr("high")},
		{Model: "claude-sonnet-4-6", Effort: configTestPtr("high")},
	}
	err := configTestValidateError(t, config)
	configTestContains(t, err, "[subagents.primary] may configure at most one model per provider")
	configTestContains(t, err, "Anthropic")
}

func TestOracleConfigurationIsIndependentAndRejectsAdaptiveEffort(t *testing.T) {
	config := DefaultConfig()
	config.Subagents.Oracle = []SubagentChoice{{Model: "advisor", Effort: configTestPtr("high")}}
	if err := config.Validate(); err != nil {
		t.Fatal(err)
	}
	if !config.Subagents.OracleConfigured() {
		t.Fatal("the oracle is configured")
	}
	encoded, err := serdeJSON(config)
	if err != nil {
		t.Fatal(err)
	}
	var value map[string]any
	if err := unmarshalJSONGeneric(encoded, &value); err != nil {
		t.Fatal(err)
	}
	if value["subagents"].(map[string]any)["oracle"].([]any)[0].(map[string]any)["model"] != "advisor" {
		t.Fatal(string(encoded))
	}
	var decoded Config
	if err := decoded.UnmarshalJSON(encoded); err != nil {
		t.Fatal(err)
	}
	if len(decoded.Subagents.Oracle) != 1 || !decoded.Subagents.Oracle[0].equal(config.Subagents.Oracle[0]) {
		t.Fatal("the oracle survives JSON")
	}
	config.Subagents.Oracle[0].Effort = configTestPtr("adaptive")
	configTestContains(t, configTestValidateError(t, config), "[subagents.oracle]")
}

// Codex profiles can point at different providers, so a native Codex thread
// continues only on a profile that sends requests to the same service. Each
// profile's provider comes from its own Codex files, overlay included, and a
// source profile that is gone leaves no evidence of its service.
func TestNativeContinuityFollowsTheCodexModelServiceNotOnlyTheHarness(t *testing.T) {
	openai := t.TempDir()
	deepseek := t.TempDir()
	configTestWrite(t, filepath.Join(deepseek, "config.toml"),
		"model = \"deepseek-v4-pro\"\n"+
			"model_provider = \"deepseek\"\n"+
			"[model_providers.deepseek]\n"+
			"base_url = \"https://api.deepseek.com/v1\"\n"+
			"env_key = \"DEEPSEEK_API_KEY\"\n"+
			"wire_api = \"responses\"\n")
	configTestWrite(t, filepath.Join(deepseek, "c245.config.toml"), "model_reasoning_effort = \"max\"\n")
	// The same service under another table name and spelling.
	renamed := t.TempDir()
	configTestWrite(t, filepath.Join(renamed, "config.toml"),
		"model_provider = \"ds\"\n"+
			"[model_providers.ds]\n"+
			"base_url = \"https://API.DeepSeek.com/v1/\"\n"+
			"env_key = \"DEEPSEEK_API_KEY\"\n"+
			"wire_api = \"responses\"\n")
	profile := func(kind HarnessKind, home string, codexProfile *string) HarnessProfile {
		profile := NewHarnessProfile(kind, home)
		profile.CodexProfile = codexProfile
		return profile
	}
	config := DefaultConfig()
	config.Profiles["codex3"] = profile(HarnessKindCodex, openai, nil)
	config.Profiles["codex4"] = profile(HarnessKindCodex, openai, nil)
	config.Profiles["deepseek245"] = profile(HarnessKindCodex, deepseek, configTestPtr("c245"))
	config.Profiles["ds"] = profile(HarnessKindCodex, renamed, nil)
	config.Profiles["claude1"] = profile(HarnessKindClaude, openai, nil)
	config.Profiles["claude2"] = profile(HarnessKindClaude, openai, nil)
	continues := func(kind HarnessKind, source, destination string) bool {
		t.Helper()
		result, err := config.NativeContinuity(kind, source, destination)
		if err != nil {
			t.Fatalf("%s -> %s: %v", source, destination, err)
		}
		return result
	}

	if !continues(HarnessKindCodex, "codex3", "codex4") ||
		continues(HarnessKindCodex, "codex3", "deepseek245") ||
		continues(HarnessKindCodex, "deepseek245", "codex3") ||
		!continues(HarnessKindCodex, "deepseek245", "ds") ||
		continues(HarnessKindCodex, "codex3", "claude1") ||
		!continues(HarnessKindClaude, "claude1", "claude2") {
		t.Fatal("continuity follows the model service")
	}
	// "Unknown" is not "the same": a removed Codex profile could have named
	// any provider, while every Claude profile uses Claude's own service.
	if continues(HarnessKindCodex, "removed", "codex4") || !continues(HarnessKindClaude, "removed", "claude2") {
		t.Fatal("a removed source profile")
	}
}

// The removed `trigger` and `enabled` keys load with any value. A table that
// names no reviewers keeps asking for automatic ones only if it asked for
// review, and the key is written only while that is still pending.
func TestRemovedReviewKeysLoadAndOnlyPendingAutomaticReviewersKeepTheTrigger(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	for _, test := range []struct {
		settings string
		expected ReviewTrigger
		pending  bool
	}{
		{"", ReviewTriggerOff, false},
		{"enabled = true", ReviewTriggerPush, true},
		{"enabled = false", ReviewTriggerOff, false},
		{"enabled = true\ntrigger = \"off\"", ReviewTriggerOff, false},
		{"enabled = false\ntrigger = \"turn\"", ReviewTriggerPush, true},
		{"enabled = true\ntrigger = \"push\"", ReviewTriggerPush, true},
		{"trigger = \"sometimes\"", ReviewTriggerOff, false},
		{"trigger = 7", ReviewTriggerOff, false},
	} {
		configTestWrite(t, path, fmt.Sprintf("version = %d\n[profiles.reviewer]\nkind = \"claude\"\nhome = \"/profiles/reviewer\"\n[review]\nprofile = \"reviewer\"\n%s\n",
			ConfigVersion, test.settings))
		config := configTestLoad(t, path)
		if config.Review.Trigger() != test.expected || config.Review.IsLegacy() != test.pending {
			t.Fatalf("%s: trigger %v pending %v", test.settings, config.Review.Trigger(), config.Review.IsLegacy())
		}
		configTestSave(t, config, path)
		saved, err := parseTOMLDocument(configTestReadFile(t, path))
		if err != nil {
			t.Fatal(err)
		}
		review, _ := saved["review"].(map[string]any)
		if _, ok := review["enabled"]; ok {
			t.Fatalf("%s: enabled written", test.settings)
		}
		trigger, hasTrigger := review["trigger"].(string)
		if hasTrigger != test.pending || hasTrigger && trigger != "push" {
			t.Fatalf("%s: trigger %q", test.settings, trigger)
		}
	}
}

// Review is on exactly when reviewers are configured, whatever the removed
// `trigger` key said, and the next save drops the key.
func TestReviewIsOnExactlyWhenReviewersAreConfiguredAndTheTriggerIsDropped(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	reviewers := "reviewers = [{ model = \"gpt-6-terra\" }]"
	for _, trigger := range []*string{nil, configTestPtr("off"), configTestPtr("push"), configTestPtr("turn")} {
		for _, test := range []struct {
			list     string
			expected ReviewTrigger
		}{
			{reviewers, ReviewTriggerPush},
			{"reviewers = []", ReviewTriggerOff},
		} {
			line := ""
			if trigger != nil {
				line = fmt.Sprintf("trigger = \"%s\"\n", *trigger)
			}
			configTestWrite(t, path, fmt.Sprintf("version = %d\n# kept\n[review]\n%s%s\n", ConfigVersion, line, test.list))
			config := configTestLoad(t, path)
			if config.Review.Trigger() != test.expected {
				t.Fatalf("%v %s: %v", trigger, test.list, config.Review.Trigger())
			}
			config.Phone.Enabled = false
			configTestSave(t, config, path)
			saved := configTestReadFile(t, path)
			configTestLacks(t, saved, "trigger")
			configTestContains(t, saved, "# kept")
			if configTestLoad(t, path).Review.Trigger() != test.expected {
				t.Fatal(saved)
			}
		}
	}
	// Once automatic reviewers are chosen the pending trigger goes too.
	pending := ReviewConfigLegacyDefault()
	if pending.Trigger() != ReviewTriggerPush {
		t.Fatal("a pending review is on")
	}
	pending.Reviewers = []SubagentChoice{{Model: "gpt-6-terra"}}
	pending.ClearLegacy()
	configTestLacks(t, configTestTOML(t, pending), "trigger")
}

// A session that chose Off is off whatever reviewers exist; one that names
// neither Off nor Push follows whether review is on.
func TestSessionChoiceDecidesWhetherReviewRuns(t *testing.T) {
	choice := SubagentChoice{Model: "gpt-6-terra"}
	on := ReviewConfig{Reviewers: []SubagentChoice{choice}}
	off := ReviewConfig{}
	inherit := SessionReviewInherit{Reviewers: &[]SubagentChoice{choice}}
	push := SessionReviewPush{}
	for _, test := range []struct {
		config   ReviewConfig
		session  SessionReview
		expected ReviewTrigger
	}{
		{on, SessionReviewOff{}, ReviewTriggerOff},
		{on, push, ReviewTriggerPush},
		{on, inherit, ReviewTriggerPush},
		// A session reviewer choice enables review even without global
		// reviewers.
		{off, inherit, ReviewTriggerPush},
		{off, SessionReviewPush{Reviewers: &[]SubagentChoice{choice}}, ReviewTriggerPush},
		{off, push, ReviewTriggerOff},
	} {
		if got := test.config.ForSession(test.session).Trigger(); got != test.expected {
			t.Fatalf("%+v %#v: %v", test.config, test.session, got)
		}
	}
}

// Per-turn review was removed. Sessions and launch files an older build
// stored with `turn` (or `on`) read as `push`, and are written as `push`.
func TestStoredTurnReviewReadsAsPushAndIsNeverWritten(t *testing.T) {
	choice := `[{"model":"gpt-6-terra","effort":"high"}]`
	modeOf := func(review SessionReview) string {
		t.Helper()
		data, err := serdeJSON(review)
		if err != nil {
			t.Fatal(err)
		}
		var value map[string]any
		if err := unmarshalJSONGeneric(data, &value); err != nil {
			t.Fatal(err)
		}
		mode, _ := value["mode"].(string)
		return mode
	}
	for _, mode := range []string{"turn", "on", "push"} {
		session := configTestSessionReview(t, fmt.Sprintf(`{"mode":"%s","reviewers":%s}`, mode, choice))
		push, ok := session.(SessionReviewPush)
		if !ok || push.Reviewers == nil || len(*push.Reviewers) != 1 {
			t.Fatalf("%s: %#v", mode, session)
		}
		if trigger, ok := session.Trigger(); !ok || trigger != ReviewTriggerPush {
			t.Fatalf("%s: trigger", mode)
		}
		if modeOf(session) != "push" {
			t.Fatalf("%s is written as push", mode)
		}
	}
	// The count, model and effort form an older build stored.
	legacy := configTestSessionReview(t, `{"mode":"turn","count":2,"model":"gpt-6-terra"}`)
	if trigger, ok := legacy.Trigger(); !ok || trigger != ReviewTriggerPush || modeOf(legacy) != "push" {
		t.Fatal("legacy push")
	}
	var trigger ReviewTrigger
	if err := unmarshalJSONGeneric([]byte(`"turn"`), &trigger); err != nil || trigger != ReviewTriggerPush {
		t.Fatalf("trigger %v, %v", trigger, err)
	}
	if encoded, _ := serdeJSON(trigger); string(encoded) != `"push"` {
		t.Fatal(string(encoded))
	}
}

// Hard-won: 060306642: startup discovery duplicated a home containing `..`.
func TestSetupAdditionsRecognizesEquivalentProfileHomes(t *testing.T) {
	directory := t.TempDir()
	root, err := filepath.EvalSymlinks(directory)
	if err != nil {
		t.Fatal(err)
	}
	home := filepath.Join(root, "home/.claude")
	if err := os.MkdirAll(home, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(root, "profiles"), 0o755); err != nil {
		t.Fatal(err)
	}
	type pair struct{ configured, detected string }
	spellings := []pair{{root + "/profiles/../home/./.claude", home}}
	if runtime.GOOS != "windows" {
		if err := os.Mkdir(filepath.Join(root, "redirect"), 0o755); err != nil {
			t.Fatal(err)
		}
		link := filepath.Join(root, "redirect/linked-home")
		if err := os.Symlink(filepath.Join(root, "home"), link); err != nil {
			t.Fatal(err)
		}
		spellings = append(spellings, pair{link + "/.claude", home})
		// Resolve symlinks before `..`: lexical collapse would choose the
		// wrong folder.
		spellings = append(spellings, pair{link + "/../home/.claude", home})
	}
	// Missing intermediate directories and homes exercise the lexical
	// fallback.
	spellings = append(spellings, pair{root + "/missing/../home/.claude", home})
	spellings = append(spellings, pair{root + "/missing/../absent/./.claude", filepath.Join(root, "absent/.claude")})
	if userHome, ok := UserHome(); ok {
		relative := filepath.Base(directory) + "/missing/../.claude"
		spellings = append(spellings, pair{"~/" + relative, userHome + "/" + relative})
	}
	for _, spelling := range spellings {
		configured := DefaultConfig()
		configured.Profiles["claude"] = NewHarnessProfile(HarnessKindClaude, spelling.configured)
		discovered := DefaultConfig()
		discovered.Profiles["claude"] = NewHarnessProfile(HarnessKindClaude, spelling.detected)
		discovered.Profiles["different"] = NewHarnessProfile(HarnessKindClaude, filepath.Join(root, "different/.claude"))
		additions := configured.SetupAdditions(discovered)
		if got := sortedKeys(additions.Profiles); !slices.Equal(got, []string{"different"}) {
			t.Fatalf("%s: additions %v", spelling.configured, got)
		}
		if configured.Profiles["claude"].Home != spelling.configured {
			t.Fatal("the configured home is not rewritten")
		}
	}
}

// `models` is optional: a profile without it loads unchanged and saves without
// the key, and a list survives a save and reload.
func TestAProfileModelsAllowListRoundTripsAndIsOmittedWhenEmpty(t *testing.T) {
	directory := t.TempDir()
	home := filepath.Join(directory, "opencode")
	if err := os.Mkdir(home, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "config.toml")
	configTestWrite(t, path, fmt.Sprintf("version = %d\n[profiles.ds8bit]\nkind = \"open-code\"\nhome = %s\n", ConfigVersion, debugString(home)))

	config := configTestLoad(t, path)
	if len(config.Profiles["ds8bit"].Models) != 0 {
		t.Fatal("no models")
	}
	configTestSave(t, config, path)
	configTestLacks(t, configTestReadFile(t, path), "models")
	// The credits option loads off, is not written while off, and survives
	// being on.
	if config.Profiles["ds8bit"].LoadBalancingMayConsumeCredits {
		t.Fatal("credits are off")
	}
	configTestLacks(t, configTestReadFile(t, path), "load_balancing_may_consume_credits")
	edit := func(change func(*HarnessProfile)) {
		profile := config.Profiles["ds8bit"]
		change(&profile)
		config.Profiles["ds8bit"] = profile
	}
	edit(func(profile *HarnessProfile) { profile.LoadBalancingMayConsumeCredits = true })
	configTestSave(t, config, path)
	if !configTestLoad(t, path).Profiles["ds8bit"].LoadBalancingMayConsumeCredits {
		t.Fatal("credits are on")
	}
	edit(func(profile *HarnessProfile) { profile.LoadBalancingMayConsumeCredits = false })
	configTestSave(t, config, path)

	listed := "openrouter/deepseek/deepseek-v4.1-flash-1017"
	edit(func(profile *HarnessProfile) { profile.Models = []string{listed} })
	configTestSave(t, config, path)
	configTestContains(t, configTestReadFile(t, path), listed)
	if got := configTestLoad(t, path).Profiles["ds8bit"].Models; !slices.Equal(got, []string{listed}) {
		t.Fatalf("models %v", got)
	}
}

func TestAnAWSRoleTableOnAProfileThatCannotUseItDoesNotRefuseSessions(t *testing.T) {
	profile := NewHarnessProfile(HarnessKindClaude, t.TempDir())
	profile.AWS = &AwsRoleConfig{RoleARN: "arn:aws:iam::123456789012:role/mj-bedrock"}
	if profile.AuthScheme() == (AuthSchemeAwsCredentialChain{}) {
		t.Fatal("a Claude login is not the AWS chain")
	}
	if err := profile.EnsureReady("claude"); err != nil {
		t.Fatal(err)
	}
}

// Real harness configuration must attribute the quota source, even when model
// requests or the local provider table name point somewhere else.
// Hard-won: quota pooling attributed Anthropic OAuth allowance to endpoint overrides.
func TestQuotaProviderReadsHarnessConfiguration(t *testing.T) {
	home := t.TempDir()
	assertProvider := func(profile HarnessProfile, want QuotaProvider) {
		t.Helper()
		if got := profile.QuotaProvider(); got != want {
			t.Fatalf("quota provider %#v, want %#v", got, want)
		}
	}
	configTestWrite(t, filepath.Join(home, "config.toml"), "model_provider = 'openai'\n")
	configTestWrite(t, filepath.Join(home, "deep.config.toml"), "model_provider = 'openai-compatible'\n[model_providers.openai-compatible]\nname = 'Proxy'\nbase_url = 'https://api.deepseek.com/v1'\nwire_api = 'responses'\nenv_key = 'DEEPSEEK_API_KEY'\n")
	profile := NewHarnessProfile(HarnessKindCodex, home)
	profile.CodexProfile = configTestPtr("deep")
	assertProvider(profile, QuotaProviderDeepSeek{})
	profile.CodexProfile = nil
	assertProvider(profile, QuotaProviderOpenAI{})
	profile.Environment.Insert("OPENAI_BASE_URL", "https://api.deepseek.com/v1")
	configTestWrite(t, filepath.Join(home, "auth.json"), `{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"access_token":"a"}}`)
	assertProvider(profile, QuotaProviderOpenAI{})
	configTestWrite(t, filepath.Join(home, "auth.json"), `{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}`)
	// Collection still asks the ChatGPT login for its allowance, even though
	// API-key model requests can follow OPENAI_BASE_URL to DeepSeek.
	if source := CodexQuotaSourceOfProfile(profile); source != (CodexQuotaSourceChatGptLogin{}) {
		t.Fatalf("source %#v", source)
	}
	assertProvider(profile, QuotaProviderOpenAI{})

	// Custom table names cannot split one service's quota or impersonate
	// another.
	for _, test := range []struct{ id, endpoint string }{
		{"anthropic", "https://api.z.ai/api/coding/paas/v4"},
		{"another-table", "https://open.bigmodel.cn/api/paas/v4"},
	} {
		configTestWrite(t, filepath.Join(home, "config.toml"), fmt.Sprintf(
			"model_provider = '%s'\n[model_providers.%s]\nbase_url = '%s'\nwire_api = 'responses'\nenv_key = 'ZAI_API_KEY'\n",
			test.id, test.id, test.endpoint))
		assertProvider(profile, QuotaProviderZai{})
	}

	configTestWrite(t, filepath.Join(home, "opencode.jsonc"), `{
        // OpenCode allows comments and trailing commas.
        "model": "local-table/some-model",
        "provider": {"local-table": {"options": {"baseURL": "https://api.moonshot.ai/v1"}}},
    }`)
	assertProvider(NewHarnessProfile(HarnessKindOpenCode, home), QuotaProviderMoonshot{})

	configTestWrite(t, filepath.Join(home, ".credentials.json"), `{"claudeAiOauth":{"accessToken":"test-oauth","refreshToken":"test-refresh","expiresAt":4102444800000,"subscriptionType":"max"}}`)
	claude := NewHarnessProfile(HarnessKindClaude, home)
	claude.Environment.Insert("ANTHROPIC_BASE_URL", "https://api.deepseek.com/anthropic")
	assertProvider(claude, QuotaProviderAnthropic{})
	// Claude quota comes from the same OAuth login even through a model
	// proxy.
	claude.Environment.Insert("ANTHROPIC_BASE_URL", "https://openrouter.ai/api")
	assertProvider(claude, QuotaProviderAnthropic{})
	claude.Environment.Insert("CLAUDE_CODE_USE_BEDROCK", "1")
	if claude.AuthScheme() != (AuthSchemeAwsCredentialChain{}) {
		t.Fatal("Bedrock uses the AWS chain")
	}
	assertProvider(claude, QuotaProviderAnthropic{})

	muse := NewHarnessProfile(HarnessKindMuse, home)
	muse.Environment.Insert("TBH_MINT_BASE_URL", "https://api.meta.ai")
	muse.Environment.Insert("TBH_AUTH_BASE_URL", "https://auth.meta.com")
	assertProvider(muse, QuotaProviderMeta{})
	if muse.QuotaProvider().DisplayName() != "Meta" {
		t.Fatal("display name")
	}
	// Only the mint endpoint supplies quota; the OAuth endpoint renews
	// tokens.
	muse.Environment.Insert("TBH_AUTH_BASE_URL", "https://login.example.test")
	assertProvider(muse, QuotaProviderMeta{})
	muse.Environment.Insert("TBH_MINT_BASE_URL", "https://mint.example.test")
	assertProvider(muse, QuotaProviderOther{Value: "mint.example.test"})

	// Meta deployments group together across harnesses and local table
	// names.
	opencode := NewHarnessProfile(HarnessKindOpenCode, home)
	for _, endpoint := range []string{
		"https://api.meta.ai/v1",
		"https://auth.meta.com/oidc/device/token/",
		"https://openrouter.ai/api",
	} {
		data, err := serdeJSON(map[string]any{
			"model":    "local-table/some-model",
			"provider": map[string]any{"local-table": map[string]any{"options": map[string]any{"baseURL": endpoint}}},
		})
		if err != nil {
			t.Fatal(err)
		}
		configTestWrite(t, filepath.Join(home, "opencode.jsonc"), string(data))
		var expected QuotaProvider = QuotaProviderMeta{}
		if strings.Contains(endpoint, "openrouter") {
			expected = QuotaProviderOther{Value: "openrouter.ai"}
		}
		assertProvider(opencode, expected)
	}
}

func unmarshalJSONGeneric(data []byte, value any) error {
	if err := json.Unmarshal(data, value); err != nil {
		return errors.New(err.Error())
	}
	return nil
}
