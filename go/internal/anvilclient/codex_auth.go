// Port of brokk-anvil-client 0.28.6 src/codex_auth.rs (the part Mjolnir uses).

package anvilclient

import (
	"encoding/json"
	"fmt"
	"os"
	"time"
)

// AuthDotJSON is the schema of Codex's `auth.json`. Field names (and the
// `OPENAI_API_KEY` shouting case) are dictated by Codex CLI's storage
// format; do not rename them without breaking cross-compatibility.
type AuthDotJSON struct {
	AuthMode     *string    `json:"auth_mode"`
	OpenAIAPIKey *string    `json:"OPENAI_API_KEY,omitempty"`
	Tokens       *TokenData `json:"tokens,omitempty"`
	LastRefresh  *time.Time `json:"last_refresh,omitempty"`
}

type TokenData struct {
	IDToken      string `json:"id_token"`
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	AccountID    string `json:"account_id"`
}

// UnmarshalJSON requires every field, as serde does.
func (t *TokenData) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range []string{"id_token", "access_token", "refresh_token", "account_id"} {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	type plain TokenData
	return json.Unmarshal(data, (*plain)(t))
}

// ReadAuthDotJSONAt reads a Codex `auth.json`, or nil when there is none.
// As Rust's `Path::exists`, a path whose metadata cannot be read counts as
// absent.
func ReadAuthDotJSONAt(path string) (*AuthDotJSON, error) {
	if _, err := os.Stat(path); err != nil {
		return nil, nil
	}
	bytes, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading %s: %w", path, err)
	}
	var parsed AuthDotJSON
	if err := json.Unmarshal(bytes, &parsed); err != nil {
		return nil, fmt.Errorf("parsing %s: %w", path, err)
	}
	return &parsed, nil
}
