// Port of mj-worker-staging/src/worker_binary/catalog.rs.

package workerstaging

import (
	"sync"

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
	panic("unported: mj-worker-staging/src/worker_binary/catalog.rs stage_codex_catalog")
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
	panic("unported: mj-worker-staging/src/worker_binary/catalog.rs apply_catalog_overrides")
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
	panic("unported: mj-worker-staging/src/worker_binary/catalog.rs stamp_guardian_reviewer")
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
	panic("unported: mj-worker-staging/src/worker_binary/catalog.rs point_config_at_catalog")
}

// FetchCatalogOverHTTPS fetches a provider's catalog over HTTPS. Mirrors the
// bounded client the Coding Plan quota reader uses: a short timeout and no
// redirects.
func FetchCatalogOverHTTPS(url string, apiKey string) ([]byte, error) {
	panic("unported: mj-worker-staging/src/worker_binary/catalog.rs fetch_catalog_over_https")
}
