// Port of the tests in mj-accounts/src/kimi_auth.rs.

package accounts

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestGlobalKimiConfigUsesTheCLIScopedCredentialSlot(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(`[providers."managed:kimi-code"]
type = "kimi"
base_url = "https://api.kimi.ai/coding/v1"

[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code-env-0e4f99c69cc27850"
oauthHost = "https://auth.kimi.ai"
`), 0o644); err != nil {
		t.Fatal(err)
	}

	runtime, err := KimiRuntimeAuthResolve(home, map[string]string{})
	if err != nil {
		t.Fatal(err)
	}

	if runtime.BaseURL != "https://api.kimi.ai/coding/v1" {
		t.Fatalf("base URL = %q", runtime.BaseURL)
	}
	if runtime.OAuthHost != "https://auth.kimi.ai" {
		t.Fatalf("OAuth host = %q", runtime.OAuthHost)
	}
	if want := filepath.Join(home, "credentials/kimi-code-env-0e4f99c69cc27850.json"); runtime.CredentialsPath != want {
		t.Fatalf("credentials path = %q, want %q", runtime.CredentialsPath, want)
	}
}

func TestProfileAPIKeyDoesNotTouchOauthCredentials(t *testing.T) {
	home := t.TempDir()
	credentials := filepath.Join(home, "credentials/kimi-code.json")
	if err := os.Mkdir(filepath.Dir(credentials), 0o755); err != nil {
		t.Fatal(err)
	}
	original := []byte(`{"access_token":"expired","refresh_token":"single-use","expires_at":1}`)
	if err := os.WriteFile(credentials, original, 0o644); err != nil {
		t.Fatal(err)
	}
	auth, err := NewKimiAuth(home, map[string]string{"KIMI_API_KEY": "profile-api-key"})
	if err != nil {
		t.Fatal(err)
	}
	token, err := auth.BearerToken(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if token == nil || *token != "profile-api-key" {
		t.Fatalf("bearer token = %v", token)
	}
	replacement, err := auth.RejectedBearerToken(context.Background(), "profile-api-key")
	if err != nil {
		t.Fatal(err)
	}
	if replacement != nil {
		t.Fatalf("a static key has no replacement to retry with: %q", *replacement)
	}
	stored, err := os.ReadFile(credentials)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(stored, original) {
		t.Fatalf("credentials changed: %s", stored)
	}
}

func TestAFreshOauthTokenIsReadWithoutRefreshing(t *testing.T) {
	home := t.TempDir()
	credentials := filepath.Join(home, "credentials/kimi-code.json")
	if err := os.Mkdir(filepath.Dir(credentials), 0o755); err != nil {
		t.Fatal(err)
	}
	farFuture := time.Now().Unix() + 86_400
	original := fmt.Sprintf(`{"access_token":"fresh","refresh_token":"single-use","expires_at":%d,"expires_in":86400}`, farFuture)
	if err := os.WriteFile(credentials, []byte(original), 0o644); err != nil {
		t.Fatal(err)
	}
	auth, err := NewKimiAuth(home, map[string]string{})
	if err != nil {
		t.Fatal(err)
	}
	token, err := auth.BearerToken(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if token == nil || *token != "fresh" {
		t.Fatalf("bearer token = %v", token)
	}
	stored, err := os.ReadFile(credentials)
	if err != nil {
		t.Fatal(err)
	}
	if string(stored) != original {
		t.Fatalf("credentials changed: %s", stored)
	}
}
