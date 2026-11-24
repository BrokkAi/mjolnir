// Port of mj-accounts/src/kimi_auth.rs.

// One Kimi OAuth refresher per profile, shared by quota and inference.
//
// Anvil's inference client used to refresh the profile's OAuth credential
// on its own, racing this daemon's quota poller and the Kimi CLI for the
// single-use refresh token. Inference now borrows the poller's refresher
// through Anvil's token-provider hook: every refresh in this process takes
// the vendor's lock, re-reads the credential once the lock is held, and only
// then spends the refresh token. Profiles with `KIMI_API_KEY` never touch
// the credential file.

package accounts

import (
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BurntSushi/toml"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	defaultKimiCodeBaseURL      = "https://api.kimi.com/coding/v1"
	defaultKimiCodeOAuthHost    = "https://auth.kimi.com"
	defaultKimiOAuthStorageName = "kimi-code"
)

// KimiRuntimeAuth is the endpoint and credential slot Kimi Code itself
// resolved from `config.toml` and the environment. Keeping this in one place
// makes quota probes and inference refresh the same OAuth token at the same
// host.
type KimiRuntimeAuth struct {
	BaseURL         string
	OAuthHost       string
	CredentialsPath string
}

// KimiRuntimeAuthResolve resolves the endpoint and credential slot of the
// Kimi home home under environment.
func KimiRuntimeAuthResolve(home string, environment map[string]string) (KimiRuntimeAuth, error) {
	configured, err := readKimiManagedProvider(filepath.Join(home, "config.toml"))
	if err != nil {
		return KimiRuntimeAuth{}, err
	}
	baseURL, ok := kimiNonempty(environment, "KIMI_CODE_BASE_URL")
	if !ok {
		if configured.baseURL != nil {
			baseURL = *configured.baseURL
		} else {
			baseURL = defaultKimiCodeBaseURL
		}
	}
	oauthHost, ok := kimiNonempty(environment, "KIMI_CODE_OAUTH_HOST")
	if !ok {
		oauthHost, ok = kimiNonempty(environment, "KIMI_OAUTH_HOST")
	}
	if !ok {
		if configured.oauthHost != nil {
			oauthHost = *configured.oauthHost
		} else {
			oauthHost = defaultKimiCodeOAuthHost
		}
	}
	baseURL = kimiNormalizeEndpoint(baseURL)
	oauthHost = kimiNormalizeEndpoint(oauthHost)
	storageName := kimiOAuthStorageName(baseURL, oauthHost)
	return KimiRuntimeAuth{
		BaseURL:         baseURL,
		OAuthHost:       oauthHost,
		CredentialsPath: filepath.Join(home, "credentials", storageName+".json"),
	}, nil
}

type kimiManagedProviderConfig struct {
	baseURL   *string
	oauthHost *string
}

// readKimiManagedProvider reads only the managed Kimi Code provider sizing
// that changes where quota lives. The rest of Kimi's config schema is
// allowed to evolve without breaking this probe.
func readKimiManagedProvider(path string) (kimiManagedProviderConfig, error) {
	contents, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return kimiManagedProviderConfig{}, nil
	}
	if err != nil {
		return kimiManagedProviderConfig{}, fmt.Errorf("read %s: %w", path, err)
	}
	if !utf8.Valid(contents) {
		return kimiManagedProviderConfig{}, fmt.Errorf("decode Kimi Code configuration %s: invalid utf-8 sequence", path)
	}
	var document map[string]any
	if _, err := toml.Decode(string(contents), &document); err != nil {
		return kimiManagedProviderConfig{}, fmt.Errorf("parse Kimi Code configuration %s: %w", path, err)
	}
	providers, _ := document["providers"].(map[string]any)
	provider, ok := providers["managed:kimi-code"].(map[string]any)
	if !ok {
		return kimiManagedProviderConfig{}, nil
	}
	var config kimiManagedProviderConfig
	baseURL, present := provider["base_url"]
	if !present {
		baseURL = provider["baseUrl"]
	}
	if text, isText := baseURL.(string); isText {
		config.baseURL = &text
	}
	if oauth, isTable := provider["oauth"].(map[string]any); isTable {
		host, present := oauth["oauthHost"]
		if !present {
			host = oauth["oauth_host"]
		}
		if text, isText := host.(string); isText {
			config.oauthHost = &text
		}
	}
	return config, nil
}

// kimiOAuthStorageName: Kimi Code scopes OAuth files by deployment. The
// mainland default keeps the legacy `kimi-code.json`; every other endpoint
// pair uses a 16-hex digest of the same JSON object the official CLI
// hashes.
func kimiOAuthStorageName(baseURL, oauthHost string) string {
	if baseURL == defaultKimiCodeBaseURL && oauthHost == defaultKimiCodeOAuthHost {
		return defaultKimiOAuthStorageName
	}
	host, err := jsonvalue.Marshal(oauthHost)
	if err != nil {
		panic("serialize OAuth host")
	}
	base, err := jsonvalue.Marshal(baseURL)
	if err != nil {
		panic("serialize API base URL")
	}
	identity := fmt.Sprintf(`{"oauthHost":%s,"baseUrl":%s}`, host, base)
	digest := sha256.Sum256([]byte(identity))
	prefix := core.LowerHex(digest[:])
	return "kimi-code-env-" + prefix[:16]
}

func kimiNormalizeEndpoint(value string) string {
	return strings.TrimRight(strings.TrimSpace(value), "/")
}

// kimiNonempty is environment[name] trimmed, when it is set and not blank.
func kimiNonempty(environment map[string]string, name string) (string, bool) {
	value, ok := environment[name]
	if !ok {
		return "", false
	}
	value = strings.TrimSpace(value)
	return value, value != ""
}

// KimiAuth is one profile's Kimi credential: its API key, or the OAuth login
// refreshed under the vendor's lock. It provides Anvil's bearer-token hook
// (BearerToken and RejectedBearerToken).
type KimiAuth struct {
	runtime KimiRuntimeAuth
	apiKey  *string
	http    *http.Client
}

func NewKimiAuth(home string, environment map[string]string) (*KimiAuth, error) {
	environment = maps.Clone(environment)
	if environment == nil {
		environment = map[string]string{}
	}
	core.HarnessKindKimi.ConfigureProfileHomeEnvironment(home, core.HarnessHostCurrent(), environment)
	var apiKey *string
	if key, ok := environment["KIMI_API_KEY"]; ok {
		if key = strings.TrimSpace(key); key != "" {
			apiKey = &key
		}
	}
	runtime, err := KimiRuntimeAuthResolve(home, environment)
	if err != nil {
		return nil, err
	}
	dialer := &net.Dialer{Timeout: 10 * time.Second}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = dialer.DialContext
	return &KimiAuth{
		runtime: runtime,
		apiKey:  apiKey,
		http:    &http.Client{Transport: transport, Timeout: 30 * time.Second},
	}, nil
}

// HasCredentials is whether the profile has a key or a local OAuth
// credential to try.
func (a *KimiAuth) HasCredentials() (bool, error) {
	if a.apiKey != nil {
		return true, nil
	}
	_, err := os.Stat(a.runtime.CredentialsPath)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return err == nil, err
}

func (a *KimiAuth) token(ctx context.Context, force bool, rejected *string) (*string, error) {
	if a.apiKey != nil {
		if force {
			return nil, nil
		}
		key := *a.apiKey
		return &key, nil
	}
	token, err := EnsureFreshKimiToken(ctx, a.http, &a.runtime, force, rejected)
	if err != nil {
		return nil, err
	}
	return &token, nil
}

// BearerToken is the token for the next request (Anvil's
// `BearerTokenProvider::bearer_token`).
func (a *KimiAuth) BearerToken(ctx context.Context) (*string, error) {
	return a.token(ctx, false, nil)
}

// RejectedBearerToken: after an explicit 401, refresh under the vendor lock
// unless another process already replaced the rejected token; either way
// the caller retries only if the token it gets back differs from the
// rejected one.
func (a *KimiAuth) RejectedBearerToken(ctx context.Context, rejected string) (*string, error) {
	return a.token(ctx, true, &rejected)
}
