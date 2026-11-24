// Port of brokk-anvil-client 0.28.6 src/grok_auth.rs (the part Mjolnir uses).

// Grok Build's OAuth credential store: the selected credential, refreshed
// under a cross-process lock when it is about to expire or was rejected.

package anvilclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const (
	grokIssuer         = "https://auth.x.ai"
	grokClientID       = "b1a00492-073a-47ea-816f-4c329264a828"
	grokRefreshMargin  = 5 * time.Minute
	grokLockTimeout    = 30 * time.Second
	grokDefaultVersion = "1.0.5"
)

type grokCredential struct {
	accessToken string
	userID      string
	email       *string
}

// grokAuthEntry is one scope's credential. Unknown members are kept and
// written back after the known ones (serde flatten).
type grokAuthEntry struct {
	key           string
	authMode      *string
	userID        string
	email         *string
	principalType *string
	principalID   *string
	refreshToken  *string
	expiresAt     *time.Time
	oidcIssuer    *string
	oidcClientID  *string
	extra         jsonvalue.Object
}

var grokAuthEntryFields = []string{
	"key", "auth_mode", "user_id", "email", "principal_type", "principal_id",
	"refresh_token", "expires_at", "oidc_issuer", "oidc_client_id",
}

// decodeGrokAuthEntry decodes an entry as serde does: missing members take
// their defaults, and a member of the wrong type fails the entry.
func decodeGrokAuthEntry(value any) (grokAuthEntry, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return grokAuthEntry{}, false
	}
	var entry grokAuthEntry
	requiredString := func(key string, target *string) bool {
		member, present := object.Get(key)
		if !present {
			return true
		}
		text, ok := member.(string)
		*target = text
		return ok
	}
	optionalString := func(key string, target **string) bool {
		member, present := object.Get(key)
		if !present || member == nil {
			return true
		}
		text, ok := member.(string)
		*target = &text
		return ok
	}
	if !requiredString("key", &entry.key) || !requiredString("user_id", &entry.userID) ||
		!optionalString("auth_mode", &entry.authMode) || !optionalString("email", &entry.email) ||
		!optionalString("principal_type", &entry.principalType) || !optionalString("principal_id", &entry.principalID) ||
		!optionalString("refresh_token", &entry.refreshToken) || !optionalString("oidc_issuer", &entry.oidcIssuer) ||
		!optionalString("oidc_client_id", &entry.oidcClientID) {
		return grokAuthEntry{}, false
	}
	var expires *string
	if !optionalString("expires_at", &expires) {
		return grokAuthEntry{}, false
	}
	if expires != nil {
		parsed, err := time.Parse(time.RFC3339Nano, *expires)
		if err != nil {
			return grokAuthEntry{}, false
		}
		parsed = parsed.UTC()
		entry.expiresAt = &parsed
	}
	for _, member := range object {
		if !slices.Contains(grokAuthEntryFields, member.Key) {
			entry.extra = append(entry.extra, member)
		}
	}
	slices.SortStableFunc(entry.extra, func(left, right jsonvalue.Member) int { return strings.Compare(left.Key, right.Key) })
	return entry, true
}

// value is the entry as serde writes it: the known members in order (None
// as null), then the kept unknown ones.
func (e *grokAuthEntry) value() jsonvalue.Object {
	optional := func(text *string) any {
		if text == nil {
			return nil
		}
		return *text
	}
	var expires any
	if e.expiresAt != nil {
		expires = chronoUTCText(*e.expiresAt)
	}
	object := jsonvalue.Object{
		{Key: "key", Value: e.key},
		{Key: "auth_mode", Value: optional(e.authMode)},
		{Key: "user_id", Value: e.userID},
		{Key: "email", Value: optional(e.email)},
		{Key: "principal_type", Value: optional(e.principalType)},
		{Key: "principal_id", Value: optional(e.principalID)},
		{Key: "refresh_token", Value: optional(e.refreshToken)},
		{Key: "expires_at", Value: expires},
		{Key: "oidc_issuer", Value: optional(e.oidcIssuer)},
		{Key: "oidc_client_id", Value: optional(e.oidcClientID)},
	}
	return append(object, e.extra...)
}

// grokAuthStore is the credential file: scope -> entry, written in key
// order.
type grokAuthStore = jsonvalue.Object

type grokAuthManager struct {
	path string
	http *http.Client
	// refreshMu serializes this process's resolves.
	refreshMu sync.Mutex
}

// loadGrokAuthManager is the manager of the credential file at path, or nil
// when there is none or it holds no usable credential.
func loadGrokAuthManager(path string) (*grokAuthManager, error) {
	if info, err := os.Stat(path); err != nil || !info.Mode().IsRegular() {
		return nil, nil
	}
	manager := &grokAuthManager{path: path, http: &http.Client{}}
	store, err := readGrokStore(path)
	if err != nil {
		return nil, err
	}
	if _, _, ok := selectGrokEntry(store); !ok {
		return nil, nil
	}
	return manager, nil
}

func (m *grokAuthManager) credential(ctx context.Context) (grokCredential, error) {
	return m.resolve(ctx, nil)
}

func (m *grokAuthManager) refreshRejected(ctx context.Context, rejectedToken string) (grokCredential, error) {
	return m.resolve(ctx, &rejectedToken)
}

func grokLoginError() error {
	return errors.New("Grok OAuth is unavailable or expired; run `grok login --oauth` and retry")
}

func (m *grokAuthManager) resolve(ctx context.Context, rejectedToken *string) (grokCredential, error) {
	m.refreshMu.Lock()
	defer m.refreshMu.Unlock()
	store, err := readGrokStore(m.path)
	if err != nil {
		return grokCredential{}, err
	}
	_, selected, ok := selectGrokEntry(store)
	if !ok {
		return grokCredential{}, grokLoginError()
	}
	if rejectedToken == nil && grokEntryIsFresh(&selected) {
		return selected.credential(), nil
	}

	lockFile, err := acquireGrokLock(m.path)
	if err != nil {
		return grokCredential{}, err
	}
	defer lockFile.Close()

	store, err = readGrokStore(m.path)
	if err != nil {
		return grokCredential{}, err
	}
	scope, entry, ok := selectGrokEntry(store)
	if !ok {
		return grokCredential{}, grokLoginError()
	}
	if rejectedToken != nil && entry.key != *rejectedToken || rejectedToken == nil && grokEntryIsFresh(&entry) {
		return entry.credential(), nil
	}
	current, err := grokLockIsCurrent(lockFile, m.path)
	if err != nil {
		return grokCredential{}, err
	}
	if !current {
		return grokCredential{}, errors.New("Grok credential lock was replaced; retry the request")
	}

	if entry.refreshToken == nil || *entry.refreshToken == "" {
		return grokCredential{}, grokLoginError()
	}
	if entry.oidcIssuer == nil || strings.TrimRight(*entry.oidcIssuer, "/") != grokIssuer {
		return grokCredential{}, grokLoginError()
	}
	issuer := strings.TrimRight(*entry.oidcIssuer, "/")
	clientID := grokClientID
	if entry.oidcClientID != nil && *entry.oidcClientID != "" {
		clientID = *entry.oidcClientID
	}
	tokenEndpoint, err := m.discoverTokenEndpoint(ctx, issuer+"/.well-known/openid-configuration")
	if err != nil {
		return grokCredential{}, err
	}
	form := url.Values{
		"grant_type":    {"refresh_token"},
		"refresh_token": {*entry.refreshToken},
		"client_id":     {clientID},
	}
	if entry.principalType != nil && *entry.principalType != "" {
		form.Set("principal_type", *entry.principalType)
	}
	if entry.principalID != nil && *entry.principalID != "" {
		form.Set("principal_id", *entry.principalID)
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, tokenEndpoint, strings.NewReader(form.Encode()))
	if err != nil {
		return grokCredential{}, withContext(err, "refreshing xAI OAuth credentials")
	}
	request.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	response, err := m.http.Do(request)
	if err != nil {
		return grokCredential{}, withContext(err, "refreshing xAI OAuth credentials")
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		body, _ := io.ReadAll(response.Body)
		if bytes.Contains(body, []byte("invalid_grant")) || bytes.Contains(body, []byte("invalid_client")) {
			return grokCredential{}, grokLoginError()
		}
		return grokCredential{}, fmt.Errorf("xAI OAuth refresh failed (HTTP %s): %s", httpStatusText(response.StatusCode), body)
	}
	var refreshed struct {
		AccessToken  *string `json:"access_token"`
		RefreshToken *string `json:"refresh_token"`
		ExpiresIn    *uint64 `json:"expires_in"`
	}
	if err := json.NewDecoder(response.Body).Decode(&refreshed); err != nil || refreshed.AccessToken == nil {
		if err == nil {
			err = errors.New("missing field `access_token`")
		}
		return grokCredential{}, withContext(err, "parsing xAI OAuth refresh response")
	}
	entry.key = *refreshed.AccessToken
	if refreshed.RefreshToken != nil && *refreshed.RefreshToken != "" {
		token := *refreshed.RefreshToken
		entry.refreshToken = &token
	}
	entry.expiresAt = nil
	if refreshed.ExpiresIn != nil {
		expires := time.Now().UTC().Add(time.Duration(*refreshed.ExpiresIn) * time.Second)
		entry.expiresAt = &expires
	}
	store.Set(scope, entry.value())
	if err := writeGrokStore(m.path, store); err != nil {
		return grokCredential{}, err
	}
	return entry.credential(), nil
}

func (m *grokAuthManager) discoverTokenEndpoint(ctx context.Context, discoveryURL string) (string, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, discoveryURL, nil)
	if err != nil {
		return "", withContext(err, "discovering the xAI OAuth token endpoint")
	}
	response, err := m.http.Do(request)
	if err != nil {
		return "", withContext(err, "discovering the xAI OAuth token endpoint")
	}
	defer response.Body.Close()
	if response.StatusCode >= 400 {
		kind := "client"
		if response.StatusCode >= 500 {
			kind = "server"
		}
		return "", withContext(fmt.Errorf("HTTP status %s error (%s) for url (%s)", kind, httpStatusText(response.StatusCode), discoveryURL),
			"xAI OAuth discovery failed")
	}
	var config struct {
		TokenEndpoint *string `json:"token_endpoint"`
	}
	if err := json.NewDecoder(response.Body).Decode(&config); err != nil || config.TokenEndpoint == nil {
		if err == nil {
			err = errors.New("missing field `token_endpoint`")
		}
		return "", withContext(err, "parsing xAI OAuth discovery response")
	}
	return *config.TokenEndpoint, nil
}

// grokClientVersionFromPath is the Grok CLI version recorded beside the
// login, or the default when there is none.
func grokClientVersionFromPath(versionPath *string) string {
	if versionPath == nil {
		return grokDefaultVersion
	}
	raw, err := os.ReadFile(*versionPath)
	if err != nil {
		return grokDefaultVersion
	}
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return grokDefaultVersion
	}
	if version, ok := jsonField(value, "version").(string); ok && strings.TrimSpace(version) != "" {
		return version
	}
	return grokDefaultVersion
}

// selectGrokEntry is the xAI OIDC entry with the latest expiry (the
// smaller scope on a tie).
func selectGrokEntry(store grokAuthStore) (string, grokAuthEntry, bool) {
	var bestScope string
	var best grokAuthEntry
	found := false
	for _, member := range store {
		entry, ok := decodeGrokAuthEntry(member.Value)
		if !ok || entry.authMode == nil || *entry.authMode != "oidc" ||
			entry.oidcIssuer == nil || strings.TrimRight(*entry.oidcIssuer, "/") != grokIssuer ||
			strings.TrimSpace(entry.key) == "" {
			continue
		}
		if !found || grokEntryOrder(member.Key, &entry, bestScope, &best) >= 0 {
			bestScope, best, found = member.Key, entry, true
		}
	}
	return bestScope, best, found
}

// grokEntryOrder is Rust's comparator: expiry first (none before any), then
// the reversed scope.
func grokEntryOrder(scopeA string, a *grokAuthEntry, scopeB string, b *grokAuthEntry) int {
	switch {
	case a.expiresAt == nil && b.expiresAt != nil:
		return -1
	case a.expiresAt != nil && b.expiresAt == nil:
		return 1
	case a.expiresAt != nil && b.expiresAt != nil && !a.expiresAt.Equal(*b.expiresAt):
		if a.expiresAt.Before(*b.expiresAt) {
			return -1
		}
		return 1
	}
	return strings.Compare(scopeB, scopeA)
}

func grokEntryIsFresh(entry *grokAuthEntry) bool {
	return entry.expiresAt != nil && entry.expiresAt.After(time.Now().Add(grokRefreshMargin))
}

func (e *grokAuthEntry) credential() grokCredential {
	return grokCredential{accessToken: e.key, userID: e.userID, email: e.email}
}

func readGrokStore(path string) (grokAuthStore, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("opening Grok OAuth credentials at %s: %w", path, err)
	}
	value, err := jsonvalue.Parse(raw)
	if err == nil {
		if object, ok := value.(jsonvalue.Object); ok {
			return object, nil
		}
		err = errors.New("invalid type: expected a map")
	}
	return nil, fmt.Errorf("parsing Grok OAuth credentials at %s: %w", path, err)
}

func acquireGrokLock(path string) (*os.File, error) {
	lockPath := filepath.Join(filepath.Dir(path), "auth.json.lock")
	file, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		return nil, fmt.Errorf("opening Grok credential lock at %s: %w", lockPath, err)
	}
	start := time.Now()
	for {
		locked, err := grokTryLockExclusive(file)
		if locked {
			return file, nil
		}
		if time.Since(start) >= grokLockTimeout {
			file.Close()
			if err == nil {
				err = errors.New("the lock is held by another process")
			}
			return nil, withContext(err, "timed out waiting for Grok credential lock")
		}
		time.Sleep(50 * time.Millisecond)
		if err != nil {
			slog.Debug("waiting for Grok credential lock", "error", err)
		}
	}
}

func writeGrokStore(path string, store grokAuthStore) error {
	if info, err := os.Lstat(path); err == nil && info.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("refusing to replace symlinked Grok credential file at %s", path)
	}
	sorted := slices.Clone(store)
	slices.SortStableFunc(sorted, func(left, right jsonvalue.Member) int { return strings.Compare(left.Key, right.Key) })
	tempPath := filepath.Join(filepath.Dir(path), ".auth.json."+newUUIDv4()+".tmp")
	temp, err := os.OpenFile(tempPath, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return fmt.Errorf("creating temporary Grok credentials at %s: %w", tempPath, err)
	}
	write := func() error {
		encoded, err := jsonvalue.Marshal(sorted)
		if err != nil {
			return err
		}
		var pretty bytes.Buffer
		if err := json.Indent(&pretty, encoded, "", "  "); err != nil {
			return err
		}
		pretty.WriteByte('\n')
		if _, err := temp.Write(pretty.Bytes()); err != nil {
			return err
		}
		if err := temp.Sync(); err != nil {
			return err
		}
		if err := temp.Close(); err != nil {
			return err
		}
		return os.Rename(tempPath, path)
	}
	if err := write(); err != nil {
		temp.Close()
		os.Remove(tempPath)
		return fmt.Errorf("atomically writing Grok OAuth credentials at %s: %w", path, err)
	}
	return nil
}
