// Port of brokk-anvil-client 0.28.6 src/kimi_auth.rs (the part Mjolnir uses).

package anvilclient

import (
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
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
//
// A host that already refreshes this profile's OAuth credential passes the
// same provider here so the profile has exactly one refresher and one
// credential writer in the process. APIKey and CredentialsPath are ignored
// because the provider owns authentication.
func (c KimiBackendConfig) BuildWithTokenProvider(auth BearerTokenProvider) (LlmBackend, error) {
	headers, err := kimiProfileHeaders(c.DeviceIDPath, c.CustomHeaders)
	if err != nil {
		return nil, err
	}
	return NewOpenAiClientWithKimiSupport(strings.TrimRight(c.BaseURL, "/"), auth, headers), nil
}

// anvilVersion is the Anvil crate version the Kimi headers report.
const anvilVersion = "0.28.6"

func kimiProfileHeaders(deviceIDPath *string, custom http.Header) (http.Header, error) {
	headers := custom.Clone()
	if headers == nil {
		headers = http.Header{}
	}
	headers.Set("User-Agent", "anvil/"+anvilVersion)
	headers.Set("X-Msh-Platform", "kimi_code_cli")
	headers.Set("X-Msh-Version", anvilVersion)
	if deviceIDPath != nil {
		if data, err := os.ReadFile(*deviceIDPath); err == nil && utf8.Valid(data) {
			if deviceID := strings.TrimSpace(string(data)); deviceID != "" {
				if !validHeaderValue(deviceID) {
					return nil, errors.New("failed to parse header value")
				}
				headers.Set("X-Msh-Device-Id", deviceID)
			}
		}
	}
	return headers, nil
}

// validHeaderValue is the http crate's HeaderValue check: no control
// character other than a tab.
func validHeaderValue(value string) bool {
	for index := 0; index < len(value); index++ {
		character := value[index]
		if !(character >= 32 && character != 127 || character == '\t') {
			return false
		}
	}
	return true
}
