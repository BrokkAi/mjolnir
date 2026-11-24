// Port of brokk-anvil-client 0.28.6 src/codex_auth.rs (the part Mjolnir uses).

package anvilclient

import (
	"bytes"
	"context"
	cryptorand "crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
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

const (
	codexClientID = "app_EMoamEEZ73f0CkXaXp7hrann"
	codexTokenURL = "https://auth.openai.com/oauth/token"
	// codexRefreshAfter is Codex's staleness window for its OAuth tokens.
	codexRefreshAfter       = 8 * 24 * time.Hour
	codexAuthConnectTimeout = 10 * time.Second
	codexAuthRequestTimeout = 60 * time.Second
	maxAuthResponseBytes    = 256 * 1024
	maxAuthErrorBodyBytes   = 2048
	// codexAuthScopes are the scopes Codex CLI asks for; the
	// `api.connectors.*` ones are required for the ChatGPT-subscription
	// Codex gateway.
	codexAuthScopes = "openid profile email offline_access api.connectors.read api.connectors.invoke"
)

// MarshalJSON writes the file as serde does: `auth_mode` always, the rest
// only when present, and `last_refresh` as chrono writes a DateTime<Utc>.
func (a AuthDotJSON) MarshalJSON() ([]byte, error) {
	type wire struct {
		AuthMode     *string    `json:"auth_mode"`
		OpenAIAPIKey *string    `json:"OPENAI_API_KEY,omitempty"`
		Tokens       *TokenData `json:"tokens,omitempty"`
		LastRefresh  *string    `json:"last_refresh,omitempty"`
	}
	value := wire{AuthMode: a.AuthMode, OpenAIAPIKey: a.OpenAIAPIKey, Tokens: a.Tokens}
	if a.LastRefresh != nil {
		text := chronoUTCText(*a.LastRefresh)
		value.LastRefresh = &text
	}
	return marshalSerdeJSON(value)
}

// chronoUTCText is chrono's serde form of a DateTime<Utc>: as many
// fractional digits (none, 3, 6 or 9) as the nanoseconds need, and `Z`.
func chronoUTCText(t time.Time) string {
	t = t.UTC()
	nanos := t.Nanosecond()
	layout := "2006-01-02T15:04:05"
	switch {
	case nanos == 0:
	case nanos%1_000_000 == 0:
		layout += ".000"
	case nanos%1_000 == 0:
		layout += ".000000"
	default:
		layout += ".000000000"
	}
	return t.Format(layout) + "Z"
}

// writeAuthDotJSONAt is an atomic write: stage to a temporary file in the
// same directory, then rename, so the credential file is never
// half-written.
func writeAuthDotJSONAt(path string, auth *AuthDotJSON) error {
	parent := filepath.Dir(path)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return fmt.Errorf("creating %s: %w", parent, err)
	}
	tmp := strings.TrimSuffix(path, filepath.Ext(path)) + ".tmp." + newUUIDv4()
	encoded, err := marshalSerdeJSON(auth)
	if err != nil {
		return fmt.Errorf("serializing AuthDotJson: %w", err)
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, encoded, "", "  "); err != nil {
		return fmt.Errorf("serializing AuthDotJson: %w", err)
	}
	if err := os.WriteFile(tmp, pretty.Bytes(), 0o600); err != nil {
		return fmt.Errorf("writing %s: %w", tmp, err)
	}
	if err := os.Chmod(tmp, 0o600); err != nil {
		return fmt.Errorf("chmod 600 %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("renaming %s -> %s: %w", tmp, path, err)
	}
	return nil
}

// newUUIDv4 is a random version 4 UUID.
func newUUIDv4() string {
	var random [16]byte
	if _, err := cryptorand.Read(random[:]); err != nil {
		panic(fmt.Sprintf("read random bytes: %v", err))
	}
	random[6] = random[6]&0x0f | 0x40
	random[8] = random[8]&0x3f | 0x80
	text := fmt.Sprintf("%x", random)
	return text[0:8] + "-" + text[8:12] + "-" + text[12:16] + "-" + text[16:20] + "-" + text[20:32]
}

// urlencode is the minimal percent-encoder Codex CLI uses for its form
// fields and query values.
func urlencode(text string) string {
	var out strings.Builder
	for index := 0; index < len(text); index++ {
		b := text[index]
		switch {
		case b >= 'A' && b <= 'Z', b >= 'a' && b <= 'z', b >= '0' && b <= '9', b == '-', b == '_', b == '.', b == '~':
			out.WriteByte(b)
		default:
			fmt.Fprintf(&out, "%%%02X", b)
		}
	}
	return out.String()
}

// codexAuthHTTPClient never follows a redirect.
func codexAuthHTTPClient() *http.Client {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{Timeout: codexAuthConnectTimeout}).DialContext
	return &http.Client{
		Transport: transport,
		Timeout:   codexAuthRequestTimeout,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
}

type oidcTokenResponse struct {
	AccessToken  string  `json:"access_token"`
	RefreshToken *string `json:"refresh_token"`
	IDToken      *string `json:"id_token"`
}

func boundedErrorBody(body string) string {
	if len(body) <= maxAuthErrorBodyBytes {
		return body
	}
	end := maxAuthErrorBodyBytes
	for end > 0 && !utf8.RuneStart(body[end]) {
		end--
	}
	return body[:end]
}

func readResponseBytesBounded(response *http.Response) ([]byte, error) {
	body, err := io.ReadAll(io.LimitReader(response.Body, maxAuthResponseBytes+1))
	if err != nil {
		return nil, fmt.Errorf("reading auth response: %w", err)
	}
	if len(body) > maxAuthResponseBytes {
		return nil, fmt.Errorf("auth response exceeded %d bytes", maxAuthResponseBytes)
	}
	return body, nil
}

// readResponseTextBounded is the bounded body as text, empty when it is not
// UTF-8.
func readResponseTextBounded(response *http.Response) string {
	body, err := readResponseBytesBounded(response)
	if err != nil || !utf8.Valid(body) {
		return ""
	}
	return string(body)
}

func postTokenForm(ctx context.Context, client *http.Client, form url.Values, failure string) (*http.Response, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, codexTokenURL, strings.NewReader(form.Encode()))
	if err != nil {
		return nil, fmt.Errorf("%s: %w", failure, err)
	}
	request.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	response, err := client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", failure, err)
	}
	return response, nil
}

func exchangeRefreshToken(ctx context.Context, client *http.Client, refreshToken string) (*oidcTokenResponse, error) {
	response, err := postTokenForm(ctx, client, url.Values{
		"grant_type":    {"refresh_token"},
		"client_id":     {codexClientID},
		"refresh_token": {refreshToken},
		// Ask the server to keep issuing an id_token alongside the new
		// access/refresh pair so the API key can be re-derived.
		"scope": {codexAuthScopes},
	}, "token endpoint POST failed")
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return nil, fmt.Errorf("token endpoint returned HTTP %s: %s",
			httpStatusText(response.StatusCode), boundedErrorBody(readResponseTextBounded(response)))
	}
	body, err := readResponseBytesBounded(response)
	if err != nil {
		return nil, err
	}
	var parsed oidcTokenResponse
	if err := json.Unmarshal(body, &parsed); err != nil {
		return nil, fmt.Errorf("parsing token endpoint response: %w", err)
	}
	return &parsed, nil
}

// tokenExchangeIDToken is the RFC 8693 token exchange that converts an
// OpenID id_token into a regular `OPENAI_API_KEY`, as Codex CLI does after
// OAuth completes.
func tokenExchangeIDToken(ctx context.Context, client *http.Client, idToken string) (string, error) {
	response, err := postTokenForm(ctx, client, url.Values{
		"grant_type":         {"urn:ietf:params:oauth:grant-type:token-exchange"},
		"client_id":          {codexClientID},
		"requested_token":    {"openai-api-key"},
		"subject_token":      {idToken},
		"subject_token_type": {"urn:ietf:params:oauth:token-type:id_token"},
	}, "token-exchange POST failed")
	if err != nil {
		return "", err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return "", fmt.Errorf("token-exchange failed (HTTP %s): %s",
			httpStatusText(response.StatusCode), boundedErrorBody(readResponseTextBounded(response)))
	}
	body, err := readResponseBytesBounded(response)
	if err != nil {
		return "", err
	}
	var parsed struct {
		AccessToken *string `json:"access_token"`
	}
	if err := json.Unmarshal(body, &parsed); err != nil {
		return "", fmt.Errorf("parsing token-exchange response: %w", err)
	}
	if parsed.AccessToken == nil {
		return "", errors.New("parsing token-exchange response: missing field `access_token`")
	}
	return *parsed.AccessToken, nil
}

// isStale: `last_refresh` is older than Codex's staleness window. No
// `last_refresh` means not stale.
func isStale(auth *AuthDotJSON) bool {
	return auth.LastRefresh != nil && time.Since(*auth.LastRefresh) >= codexRefreshAfter
}

// refreshIfStaleAt refreshes stale credentials and writes them back to
// path.
func refreshIfStaleAt(ctx context.Context, path string, auth *AuthDotJSON) (bool, error) {
	if !isStale(auth) {
		return false, nil
	}
	if auth.Tokens == nil {
		return false, errors.New("auth.json has no tokens to refresh")
	}
	tokens := auth.Tokens
	client := codexAuthHTTPClient()
	refreshed, err := exchangeRefreshToken(ctx, client, tokens.RefreshToken)
	if err != nil {
		return false, err
	}
	if refreshed.IDToken == nil {
		return false, errors.New("refresh response missing id_token")
	}
	idToken := *refreshed.IDToken
	refreshToken := tokens.RefreshToken
	if refreshed.RefreshToken != nil {
		refreshToken = *refreshed.RefreshToken
	}
	// Refresh stays successful even if the account can't mint an
	// OPENAI_API_KEY; a transient failure keeps any stored key.
	apiKey := auth.OpenAIAPIKey
	if key, err := tokenExchangeIDToken(ctx, client, idToken); err == nil {
		apiKey = &key
	} else {
		slog.Debug(fmt.Sprintf("token-exchange skipped during refresh: %v", err))
	}
	accountID, err := ExtractChatgptAccountID(refreshed.AccessToken)
	if err != nil {
		return false, err
	}
	chatgpt := "chatgpt"
	now := time.Now().UTC()
	*auth = AuthDotJSON{
		AuthMode:     &chatgpt,
		OpenAIAPIKey: apiKey,
		Tokens: &TokenData{
			IDToken:      idToken,
			AccessToken:  refreshed.AccessToken,
			RefreshToken: refreshToken,
			AccountID:    accountID,
		},
		LastRefresh: &now,
	}
	if err := writeAuthDotJSONAt(path, auth); err != nil {
		return false, err
	}
	return true, nil
}

// ExtractChatgptAccountID pulls the `chatgpt_account_id` claim out of an
// access_token JWT without verifying the signature: it was just received
// over HTTPS from auth.openai.com, and only an opaque account identifier is
// needed.
func ExtractChatgptAccountID(accessToken string) (string, error) {
	parts := strings.Split(accessToken, ".")
	if len(parts) < 2 {
		return "", errors.New("access_token is not a JWT")
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		payload, err = base64.URLEncoding.DecodeString(parts[1])
		if err != nil {
			return "", fmt.Errorf("base64-decoding JWT payload: %w", err)
		}
	}
	claims, err := jsonvalue.Parse(payload)
	if err != nil {
		return "", fmt.Errorf("parsing JWT payload as JSON: %w", err)
	}
	auth, _ := jsonMember(claims, "https://api.openai.com/auth")
	accountID, _ := jsonMember(auth, "chatgpt_account_id")
	if text, ok := accountID.(string); ok {
		return text, nil
	}
	return "", errors.New("JWT missing https://api.openai.com/auth.chatgpt_account_id claim")
}

// jsonMember is serde_json's `value.get(key)` on a JSON value.
func jsonMember(value any, key string) (any, bool) {
	if object, ok := value.(jsonvalue.Object); ok {
		return object.Get(key)
	}
	return nil, false
}
