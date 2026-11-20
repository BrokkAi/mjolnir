// Port of the tests in mj-core/src/codex_provider.rs.

package core

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func codexProviderParse(text string) (*CodexProvider, error) {
	return ParseConfig(text, "config.toml")
}

func TestConfigWithoutModelProviderReportsNoSelectedProvider(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("model = \"gpt-5.5\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	provider, err := ReadCodexProvider(home, nil)
	if err != nil {
		t.Fatal(err)
	}
	if provider != nil {
		t.Fatalf("provider %v", provider)
	}
}

func TestBuiltInBedrockRuntimeAcceptsAWSOverridesWithoutACustomProviderDefinition(t *testing.T) {
	provider, err := codexProviderParse("model = \"global.openai.gpt-6-luna\"\n" +
		"model_provider = \"amazon-bedrock-runtime\"\n" +
		"[model_providers.amazon-bedrock-runtime.aws]\n" +
		"region = \"us-east-1\"\n")
	if err != nil {
		t.Fatal(err)
	}
	if provider == nil {
		t.Fatal("built-in provider")
	}
	if provider.Definition != (CodexProviderDefinitionBuiltIn{Provider: BuiltInCodexProviderAmazonBedrockRuntime}) {
		t.Fatalf("definition %v", provider.Definition)
	}
	if !provider.UsesAWSCredentials() {
		t.Fatal("uses AWS credentials")
	}
	if provider.ID() != "amazon-bedrock-runtime" {
		t.Fatalf("id %q", provider.ID())
	}
}

func TestEnvKeyProviderReportsItsVariableAndBaseURL(t *testing.T) {
	provider, err := codexProviderParse("model = \"glm-5.3\"\n" +
		"model_provider = \"zai\"\n" +
		"[model_providers.zai]\n" +
		"base_url = \"https://api.z.ai/api/v1\"\n" +
		"env_key = \"ZAI_API_KEY\"\n" +
		"wire_api = \"responses\"\n")
	if err != nil || provider == nil {
		t.Fatalf("provider %v %v", provider, err)
	}
	custom := provider.Custom()
	if custom == nil {
		t.Fatal("custom provider")
	}
	if custom.ID != "zai" || custom.BaseURL != "https://api.z.ai/api/v1" {
		t.Fatalf("custom %v", custom)
	}
	if custom.EnvKey == nil || *custom.EnvKey != "ZAI_API_KEY" {
		t.Fatalf("env key %v", custom.EnvKey)
	}
	if custom.BearerToken != nil {
		t.Fatal("bearer token")
	}
	if custom.Key() != CodexProviderKey(CodexProviderKeyEnvKey("ZAI_API_KEY")) {
		t.Fatalf("key %v", custom.Key())
	}
	if host, ok := provider.Host(); !ok || host != "api.z.ai" {
		t.Fatalf("host %q", host)
	}
}

func TestInlineBearerTokenProviderIsAcceptedWithoutAnEnvKey(t *testing.T) {
	provider, err := codexProviderParse("model_provider = \"zai\"\n" +
		"[model_providers.zai]\n" +
		"base_url = \"https://api.z.ai/api/v1\"\n" +
		"experimental_bearer_token = \"secret\"\n" +
		"wire_api = \"responses\"\n")
	if err != nil || provider == nil {
		t.Fatalf("provider %v %v", provider, err)
	}
	custom := provider.Custom()
	if custom == nil {
		t.Fatal("custom provider")
	}
	if custom.EnvKey != nil {
		t.Fatal("env key")
	}
	if custom.BearerToken == nil || *custom.BearerToken != "secret" {
		t.Fatalf("bearer token %v", custom.BearerToken)
	}
	if custom.Key() != CodexProviderKey(CodexProviderKeyInline("secret")) {
		t.Fatalf("key %v", custom.Key())
	}
}

func TestAnInlineBearerTokenNeverAppearsInDebugOutput(t *testing.T) {
	provider, err := codexProviderParse("model_provider = \"zai\"\n" +
		"[model_providers.zai]\n" +
		"base_url = \"https://api.z.ai/api/v1\"\n" +
		"experimental_bearer_token = \"super-secret-token\"\n" +
		"wire_api = \"responses\"\n")
	if err != nil || provider == nil {
		t.Fatalf("provider %v %v", provider, err)
	}
	for _, format := range []string{"%v", "%+v", "%#v"} {
		rendered := fmt.Sprintf(format, provider)
		if strings.Contains(rendered, "super-secret-token") {
			t.Fatalf("%s: %s", format, rendered)
		}
		if !strings.Contains(rendered, "<redacted>") {
			t.Fatalf("%s: %s", format, rendered)
		}
		rendered = fmt.Sprintf(format, provider.Custom().Key())
		if strings.Contains(rendered, "super-secret-token") {
			t.Fatalf("%s: %s", format, rendered)
		}
	}
}

func TestChatWireAPIIsRejectedWithTheSupportedValue(t *testing.T) {
	_, err := codexProviderParse("model_provider = \"zai\"\n" +
		"[model_providers.zai]\n" +
		"base_url = \"https://api.z.ai/api/v1\"\n" +
		"env_key = \"ZAI_API_KEY\"\n" +
		"wire_api = \"chat\"\n")
	if err == nil || !strings.Contains(err.Error(), "responses") {
		t.Fatalf("chat is rejected: %v", err)
	}
}

func TestUnknownProviderWithoutATableNamesTheMissingCustomProviderTable(t *testing.T) {
	_, err := codexProviderParse("model_provider = \"unknown-service\"\n")
	if err == nil {
		t.Fatal("unknown provider without a table")
	}
	if !strings.Contains(err.Error(), "not a Codex built-in provider") || !strings.Contains(err.Error(), "model_providers.unknown-service") {
		t.Fatal(err)
	}
}

func TestProviderWithoutAnyKeySourceIsRejected(t *testing.T) {
	_, err := codexProviderParse("model_provider = \"zai\"\n" +
		"[model_providers.zai]\n" +
		"base_url = \"https://api.z.ai/api/v1\"\n" +
		"wire_api = \"responses\"\n")
	if err == nil || !strings.Contains(err.Error(), "env_key") {
		t.Fatalf("no key: %v", err)
	}
}

func TestUserSuppliedModelCatalogPathIsReported(t *testing.T) {
	provider, err := codexProviderParse("model_provider = \"zai\"\n" +
		"model_catalog_json = \"mine.json\"\n" +
		"[model_providers.zai]\n" +
		"base_url = \"https://api.z.ai/api/v1\"\n" +
		"env_key = \"ZAI_API_KEY\"\n" +
		"wire_api = \"responses\"\n")
	if err != nil || provider == nil {
		t.Fatalf("provider %v %v", provider, err)
	}
	if provider.ModelCatalogJSON == nil || *provider.ModelCatalogJSON != "mine.json" {
		t.Fatalf("catalog %v", provider.ModelCatalogJSON)
	}
}
