// Port of mj-worker-staging/src/worker_binary/catalog.rs.

package workerstaging

import (
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CatalogFetch fetches a provider's model catalog. A function parameter so
// tests can supply a body without reaching the network. Takes the catalog URL
// and the API key; returns the raw response body.
type CatalogFetch func(url string, apiKey string) ([]byte, error)

// StagedCatalogFile is the catalog file Mjolnir writes into a staged Codex
// home, and the key it points `config.toml` at. Relative to `CODEX_HOME`, so
// the staged copy works unchanged on any target.
const StagedCatalogFile = "models.json"

// CatalogCacheKey is the key one provider's fetched catalog is remembered
// under. Every provider has its own, named after its base URL.
func CatalogCacheKey(baseURL string) string {
	return "catalog:" + baseURL
}

// CatalogCache is where a fetched catalog is remembered so a provider outage
// cannot block a launch. The daemon's memory is one implementation; a test
// can supply another. key is CatalogCacheKey of the provider the body came
// from.
type CatalogCache interface {
	// Load is the body stored for profileID and key, if any.
	Load(profileID string, key string) (string, bool)
	Store(profileID string, key string, body string)
}

// catalogs holds the catalogs fetched by this daemon, by profile and
// CatalogCacheKey. They live only as long as the daemon.
var catalogs = struct {
	sync.Mutex
	entries map[[2]string]string
}{entries: map[[2]string]string{}}

// SharedCatalogCache is the catalog cache shared by everything in this
// daemon.
type SharedCatalogCache struct{}

func (SharedCatalogCache) Load(profileID string, key string) (string, bool) {
	catalogs.Lock()
	defer catalogs.Unlock()
	body, ok := catalogs.entries[[2]string{profileID, key}]
	return body, ok
}

func (SharedCatalogCache) Store(profileID string, key string, body string) {
	catalogs.Lock()
	defer catalogs.Unlock()
	catalogs.entries[[2]string{profileID, key}] = body
}

// StageCodexCatalog gives a staged Codex home the model catalog its provider
// advertises.
//
// Codex fetches its model list from its own service only for ChatGPT logins.
// Without a catalog file a profile pointed at another provider would offer
// OpenAI's built-in model names and send them to that provider, so Mjolnir
// fetches the provider's own catalog, stamps the Guardian reviewer on every
// entry, writes it beside the staged `config.toml`, and points the staged
// configuration at it.
//
// Profiles with no custom provider are left alone. A failed fetch falls back
// to the last catalog stored for this profile, so a provider outage does not
// block a launch; with neither, the launch fails naming the profile and the
// URL.
func StageCodexCatalog(profileID string, profile *core.HarnessProfile, destination string, fetch CatalogFetch, cache CatalogCache) error {
	provider, err := profile.CodexProvider()
	if err != nil {
		return err
	}
	if provider == nil {
		return nil
	}
	custom := provider.Custom()
	if custom == nil {
		// Codex owns model discovery for its built-in providers, including
		// Bedrock's bundled catalog. Do not try to fetch `/models` from them.
		return nil
	}
	apiKey, ok := profile.CodexProviderAPIKey()
	if !ok {
		// Only an `env_key` provider whose variable is unset reaches this;
		// `ensure_ready` names that variable before anything stages the
		// profile.
		envKey := "provider key"
		if custom.EnvKey != nil {
			envKey = *custom.EnvKey
		}
		return fmt.Errorf("profile %s has no %s value to read its model catalog with", core.RustDebugString(profileID), envKey)
	}
	url := strings.TrimRight(custom.BaseURL, "/") + "/models"
	key := CatalogCacheKey(custom.BaseURL)
	body, fetchErr := fetch(url, apiKey)
	if fetchErr == nil {
		if utf8.Valid(body) {
			cache.Store(profileID, key, string(body))
		}
	} else {
		cached, ok := cache.Load(profileID, key)
		if !ok {
			return fmt.Errorf("profile %s: could not fetch the model catalog from %s and no cached copy is available: %w",
				core.RustDebugString(profileID), url, fetchErr)
		}
		slog.Warn(fmt.Sprintf("could not fetch the model catalog from %s, using the last cached copy: %v", url, fetchErr),
			"profile_id", profileID, "provider", provider.ID())
		body = []byte(cached)
	}
	catalog, err := core.CodexCatalogParse(body)
	if err != nil {
		return fmt.Errorf("profile %s: model catalog from %s: %w", core.RustDebugString(profileID), url, err)
	}
	if err := ApplyCatalogOverrides(profileID, profile.Home, &catalog); err != nil {
		return err
	}
	if err := StampGuardianReviewer(profileID, profile, &catalog); err != nil {
		return err
	}
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return rustIOError(err)
	}
	if err := os.WriteFile(filepath.Join(destination, StagedCatalogFile), []byte(catalog.ToJSON()), 0o666); err != nil {
		return rustIOError(err)
	}
	return PointConfigAtCatalog(filepath.Join(destination, "config.toml"))
}

// ApplyCatalogOverrides refines the fetched catalog with the user's own
// `models.json`, when the profile home has one.
//
// A provider that serves OpenAI's plain model list gives Mjolnir only model
// ids, so the translated entries carry conservative defaults. The override
// file is how a user states what that provider actually supports: each entry
// is matched by `slug` and its fields are copied over the fetched entry, and a
// slug the provider did not list is added. Mjolnir writes the merged result
// over the staged `models.json`, so the user's own file never reaches Codex
// unmerged.
func ApplyCatalogOverrides(profileID string, home string, catalog *core.CodexCatalog) error {
	path := filepath.Join(home, StagedCatalogFile)
	bytes, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("read %s: %w", path, rustIOError(err))
	}
	overrides, err := core.ParseCodexShape(bytes)
	if err != nil {
		return fmt.Errorf("profile %s: model catalog overrides in %s: %w", core.RustDebugString(profileID), path, err)
	}
	core.MergeOverrides(catalog, &overrides)
	return nil
}

// StampGuardianReviewer records the Guardian reviewer choice on every
// catalog entry.
//
// Codex reads the reviewer from the session model's own catalog entry, so the
// override is stamped on all of them. The profile setting decides which model
// that is: the newest flash model by default, the session model itself when
// the setting is `session` (nothing is stamped, which is Codex's own
// fallback), or a named slug. A named slug the catalog does not list fails the
// launch, because stamping it would leave Codex silently reviewing with
// something else.
func StampGuardianReviewer(profileID string, profile *core.HarnessProfile, catalog *core.CodexCatalog) error {
	setting := core.GuardianReviewNewestFlash
	if profile.GuardianReviewModel != nil {
		setting = *profile.GuardianReviewModel
	}
	if setting == core.GuardianReviewSession {
		slog.Info(`guardian_review_model is "session"; Guardian reviews run on the session model`, "profile_id", profileID)
		return nil
	}
	if setting == core.GuardianReviewNewestFlash {
		if reviewer, ok := core.GuardianReviewModel(catalog.Slugs()); ok {
			core.StampReviewer(catalog, reviewer)
		} else {
			// With no small model to review with, Codex falls back to
			// reviewing with the session model, which still runs Guardian.
			slog.Info("the model catalog lists no flash model; Guardian reviews run on the session model", "profile_id", profileID)
		}
		return nil
	}
	slugs := catalog.Slugs()
	if !slices.Contains(slugs, setting) {
		return fmt.Errorf("profile %s: guardian_review_model %s is not in the provider's model catalog, which lists %s",
			core.RustDebugString(profileID), core.RustDebugString(setting), strings.Join(slugs, ", "))
	}
	core.StampReviewer(catalog, setting)
	return nil
}

// catalogKey is the top-level key a staged Codex `config.toml` uses to name
// its catalog.
const catalogKey = "model_catalog_json"

// PointConfigAtCatalog points a staged Codex `config.toml` at the staged
// catalog.
//
// The key is top-level in Codex's configuration, and TOML puts every
// top-level key before the first table header, so it is written into the root
// table. A profile may have written its own key — pointing at a catalog file
// of its own — so the staged copy replaces it rather than adding a second
// one, which would be a duplicate key Codex rejects outright. The replacement
// keeps the user's own decoration around the key. Only the staged copy
// changes; the profile's own `config.toml` is never written.
func PointConfigAtCatalog(path string) error {
	// Rust reads with `unwrap_or_default`: an unreadable file is an empty
	// document.
	existing, _ := os.ReadFile(path)
	if !utf8.Valid(existing) {
		existing = nil
	}
	document, err := core.TomlEditSetRootString(string(existing), catalogKey, StagedCatalogFile)
	if err != nil {
		return fmt.Errorf("parse staged Codex configuration %s: %w", path, err)
	}
	if err := os.WriteFile(path, []byte(document), 0o666); err != nil {
		return fmt.Errorf("point %s at the staged model catalog: %w", path, rustIOError(err))
	}
	return nil
}

// FetchCatalogOverHTTPS fetches a provider's catalog over HTTPS. Mirrors the
// bounded client the Coding Plan quota reader uses: a short timeout and no
// redirects.
func FetchCatalogOverHTTPS(url string, apiKey string) ([]byte, error) {
	return onDedicatedThread(func() ([]byte, error) {
		client := &http.Client{
			Timeout:       8 * time.Second,
			CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
		}
		request, err := http.NewRequest(http.MethodGet, url, nil)
		if err != nil {
			return nil, err
		}
		request.Header.Set("Authorization", "Bearer "+apiKey)
		request.Header.Set("Accept", "application/json")
		response, err := client.Do(request)
		if err != nil {
			return nil, err
		}
		defer response.Body.Close()
		if err := catalogErrorForStatus(response); err != nil {
			return nil, err
		}
		return io.ReadAll(response.Body)
	})
}

// catalogErrorForStatus is reqwest's error_for_status: a client or server
// error status as an error naming it and the response's URL.
func catalogErrorForStatus(response *http.Response) error {
	var kind string
	switch {
	case response.StatusCode >= 400 && response.StatusCode < 500:
		kind = "client"
	case response.StatusCode >= 500 && response.StatusCode < 600:
		kind = "server"
	default:
		return nil
	}
	message := fmt.Sprintf("HTTP status %s error (%d %s)", kind, response.StatusCode, http.StatusText(response.StatusCode))
	if response.Request != nil && response.Request.URL != nil {
		message += fmt.Sprintf(" for url (%s)", response.Request.URL)
	}
	return errors.New(message)
}
