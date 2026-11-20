// Port of mj-core/src/config/github.rs.
//
// GitHub credentials configured for the controller.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

// GithubConfig is the optional GitHub integration settings.
type GithubConfig struct {
	// SessionCredentials shares GitHub authentication with newly created
	// sessions. Each session retains its accepted choice, and children
	// inherit an opt-out.
	SessionCredentials bool `json:"session_credentials" toml:"session_credentials"`
	// SessionTokenPath is a controller-side token file distributed to
	// sessions instead of App or host CLI credentials. Its contents are never
	// serialized here.
	SessionTokenPath *string `json:"session_token_path,omitempty" toml:"session_token_path,omitempty"`
	// App is GitHub App authentication. When absent, the controller uses its
	// existing GH_TOKEN, GITHUB_TOKEN, or `gh auth token` lookup.
	App *GithubAppConfig `json:"app,omitempty" toml:"app,omitempty"`
	// Watch is daemon polling for repositories in live sessions.
	Watch GithubWatchConfig `json:"watch" toml:"watch"`
}

// DefaultGithubConfig is GithubConfig's Default.
func DefaultGithubConfig() GithubConfig {
	return GithubConfig{SessionCredentials: true, Watch: DefaultGithubWatchConfig()}
}

func (g GithubConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(g.SessionCredentials), "session_credentials", g.SessionCredentials)
	object.fieldUnless(g.SessionTokenPath == nil, "session_token_path", g.SessionTokenPath)
	object.fieldUnless(g.App == nil, "app", g.App)
	object.fieldUnless(g.Watch.IsDefault(), "watch", g.Watch)
	return object.finish()
}

func (g *GithubConfig) UnmarshalJSON(data []byte) error {
	type plain GithubConfig
	value := plain(DefaultGithubConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*g = GithubConfig(value)
	return nil
}

func (g GithubConfig) IsDefault() bool {
	return g.SessionCredentials && g.SessionTokenPath == nil && g.App == nil && g.Watch.IsDefault()
}

func (g GithubConfig) Validate() error {
	if g.SessionTokenPath != nil {
		if err := ValidateAbsoluteInput(*g.SessionTokenPath); err != nil {
			return fmt.Errorf("[github] session_token_path: %w", err)
		}
	}
	if g.App != nil {
		return g.App.Validate()
	}
	return nil
}

// GithubWatchConfig is the configuration for the durable GitHub mailbox
// producer.
type GithubWatchConfig struct {
	// Enabled is whether the watcher produces GitHub events. Jev must also be
	// enabled.
	Enabled bool `json:"enabled" toml:"enabled"`
	// LocalInference opts in to local relevance scoring instead of sending
	// every item to the proxy.
	LocalInference bool `json:"local_inference" toml:"local_inference"`
	// UseGPU selects a measured GPU for local inference; false keeps scoring
	// on CPU.
	UseGPU bool `json:"use_gpu" toml:"use_gpu"`
	// IntervalSeconds is the delay between polls, bounded to avoid a tight
	// retry loop.
	IntervalSeconds uint64 `json:"interval_seconds" toml:"interval_seconds"`
	// APIBase is the GitHub REST API base URL; configurable for isolated
	// fake-server tests.
	APIBase string `json:"api_base" toml:"api_base"`
}

func (g GithubWatchConfig) IsDefault() bool { return g == DefaultGithubWatchConfig() }

// DefaultGithubWatchConfig is GithubWatchConfig's Default.
func DefaultGithubWatchConfig() GithubWatchConfig {
	return GithubWatchConfig{Enabled: true, UseGPU: true, IntervalSeconds: 60, APIBase: "https://api.github.com"}
}

func (g GithubWatchConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(g.Enabled), "enabled", g.Enabled)
	object.fieldUnless(isFalse(g.LocalInference), "local_inference", g.LocalInference)
	object.fieldUnless(isTrue(g.UseGPU), "use_gpu", g.UseGPU)
	object.field("interval_seconds", g.IntervalSeconds)
	object.field("api_base", g.APIBase)
	return object.finish()
}

func (g *GithubWatchConfig) UnmarshalJSON(data []byte) error {
	type plain GithubWatchConfig
	value := plain(DefaultGithubWatchConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*g = GithubWatchConfig(value)
	return nil
}

// GithubAppConfig is the controller-host credentials for one GitHub App and
// its installations.
type GithubAppConfig struct {
	// AppID is the numeric identifier shown in the GitHub App settings page.
	AppID uint64 `json:"app_id" toml:"app_id"`
	// PrivateKeyPath is the PEM private key on the controller host. Never
	// copied to a worker.
	PrivateKeyPath string `json:"private_key_path" toml:"private_key_path"`
	// Installations is optional GitHub owner login to installation ID
	// overrides.
	Installations map[string]uint64 `json:"installations,omitempty" toml:"installations,omitempty"`
	// SessionPermissions is the complete permission grant requested for
	// session tokens. Nil keeps the installation's full permission grant.
	SessionPermissions *GithubPermissionSet `json:"session_permissions,omitempty" toml:"session_permissions,omitempty"`
	// TokenPermissions is the complete permission grant requested by
	// `mj github-token`. Nil keeps the installation's full permission grant.
	TokenPermissions *GithubPermissionSet `json:"token_permissions,omitempty" toml:"token_permissions,omitempty"`
}

// UnmarshalJSON requires `app_id` and `private_key_path`, as serde does.
func (g *GithubAppConfig) UnmarshalJSON(data []byte) error {
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	for _, name := range []string{"app_id", "private_key_path"} {
		if _, ok := fields[name]; !ok {
			return missingField(name)
		}
	}
	type plain GithubAppConfig
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*g = GithubAppConfig(value)
	return nil
}

// GithubPermissionSet is a subset of GitHub App permissions requested for an
// installation token.
type GithubPermissionSet = map[string]GithubPermissionLevel

// GithubPermissionLevel is a permission level GitHub allows an installation
// access token to request.
type GithubPermissionLevel string

const (
	GithubPermissionLevelRead  GithubPermissionLevel = "read"
	GithubPermissionLevelWrite GithubPermissionLevel = "write"
)

func (l GithubPermissionLevel) AsStr() string { return string(l) }

func (l *GithubPermissionLevel) UnmarshalText(text []byte) error {
	switch GithubPermissionLevel(text) {
	case GithubPermissionLevelRead, GithubPermissionLevelWrite:
		*l = GithubPermissionLevel(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "read", "write")
}

func (g GithubAppConfig) Validate() error {
	if g.AppID == 0 {
		return errors.New("[github.app] app_id must be a positive integer")
	}
	if g.PrivateKeyPath == "" {
		return errors.New("[github.app] private_key_path must not be empty")
	}
	return g.validatePermissionsAndInstallations()
}

// validatePermissionsAndInstallations is the rest of
// GithubAppConfig::validate: permission names, then installation owners.
func (g GithubAppConfig) validatePermissionsAndInstallations() error {
	for _, section := range []struct {
		name        string
		permissions *GithubPermissionSet
	}{
		{"session_permissions", g.SessionPermissions},
		{"token_permissions", g.TokenPermissions},
	} {
		if section.permissions == nil {
			continue
		}
		for _, permission := range sortedKeys(*section.permissions) {
			if !validGithubPermissionName(permission) {
				return fmt.Errorf("[github.app.%s] key %s is not a valid GitHub permission name",
					section.name, debugString(permission))
			}
		}
	}
	owners := map[string]bool{}
	for _, owner := range sortedKeys(g.Installations) {
		if !ValidGithubOwnerLogin(owner) {
			return fmt.Errorf("[github.app.installations] key %s is not a valid GitHub owner login", debugString(owner))
		}
		folded := configASCIILower(owner)
		if owners[folded] {
			return fmt.Errorf("[github.app.installations] owner %s is configured more than once with different letter casing",
				debugString(owner))
		}
		owners[folded] = true
		if g.Installations[owner] == 0 {
			return fmt.Errorf("[github.app.installations.%s] installation ID must be a positive integer", owner)
		}
	}
	return nil
}

func validGithubPermissionName(permission string) bool {
	if permission == "" || permission[0] < 'a' || permission[0] > 'z' {
		return false
	}
	for i := 1; i < len(permission); i++ {
		b := permission[i]
		if !(b >= 'a' && b <= 'z' || b >= '0' && b <= '9' || b == '_') {
			return false
		}
	}
	return true
}

func ValidGithubOwnerLogin(owner string) bool {
	if owner == "" || len(owner) > 39 || strings.HasPrefix(owner, "-") || strings.HasSuffix(owner, "-") ||
		strings.Contains(owner, "--") {
		return false
	}
	for i := 0; i < len(owner); i++ {
		b := owner[i]
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '-') {
			return false
		}
	}
	return true
}
