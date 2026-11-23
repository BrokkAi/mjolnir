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
	"errors"
	"io/fs"
	"net/http"
	"os"
	"strings"
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
	panic("unported: mj-accounts/src/kimi_auth.rs KimiRuntimeAuth::resolve")
}

type kimiManagedProviderConfig struct {
	baseURL   *string
	oauthHost *string
}

// readKimiManagedProvider reads only the managed Kimi Code provider sizing
// that changes where quota lives. The rest of Kimi's config schema is
// allowed to evolve without breaking this probe.
func readKimiManagedProvider(path string) (kimiManagedProviderConfig, error) {
	panic("unported: mj-accounts/src/kimi_auth.rs read_managed_provider")
}

// kimiOAuthStorageName: Kimi Code scopes OAuth files by deployment. The
// mainland default keeps the legacy `kimi-code.json`; every other endpoint
// pair uses a 16-hex digest of the same JSON object the official CLI
// hashes.
func kimiOAuthStorageName(baseURL, oauthHost string) string {
	panic("unported: mj-accounts/src/kimi_auth.rs oauth_storage_name")
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
	panic("unported: mj-accounts/src/kimi_auth.rs KimiAuth::new")
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
	panic("unported: mj-accounts/src/kimi_auth.rs KimiAuth::token")
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
