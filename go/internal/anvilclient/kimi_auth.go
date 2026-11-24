// Port of brokk-anvil-client 0.28.6 src/kimi_auth.rs (the part Mjolnir uses).

package anvilclient

import (
	"net/http"
	"path/filepath"
)

const (
	KimiAPIKeyEnv          = "KIMI_API_KEY"
	KimiCodeBaseURLEnv     = "KIMI_CODE_BASE_URL"
	DefaultKimiCodeBaseURL = "https://api.kimi.com/coding/v1"
	kimiDefaultOAuthHost   = "https://auth.kimi.com"
)

// KimiBackendConfig is explicit, process-environment-independent
// configuration for one Kimi profile.
type KimiBackendConfig struct {
	APIKey          *string
	CredentialsPath string
	DeviceIDPath    *string
	OAuthHost       string
	BaseURL         string
	CustomHeaders   http.Header
}

// KimiBackendConfigFromHome is the configuration of the Kimi Code home home.
func KimiBackendConfigFromHome(home string) KimiBackendConfig {
	deviceIDPath := filepath.Join(home, "device_id")
	return KimiBackendConfig{
		CredentialsPath: filepath.Join(home, "credentials", "kimi-code.json"),
		DeviceIDPath:    &deviceIDPath,
		OAuthHost:       kimiDefaultOAuthHost,
		BaseURL:         DefaultKimiCodeBaseURL,
		CustomHeaders:   http.Header{},
	}
}

// BuildWithTokenProvider builds the Kimi backend with auth supplying every
// request's bearer token: an OpenAI-compatible client with Kimi's headers
// (its device id and the configured custom headers).
func (c KimiBackendConfig) BuildWithTokenProvider(auth BearerTokenProvider) (LlmBackend, error) {
	panic("unported: brokk-anvil-client/src/kimi_auth.rs KimiBackendConfig::build_with_token_provider")
}
