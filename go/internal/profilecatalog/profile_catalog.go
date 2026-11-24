// Port of mj-profile-catalog/src/profile_catalog.rs.

// One daemon-owned cache, warmed independently of settings and tool calls.
// Entries belong to configured profile identities, never a shared
// generation. Every reader joins the owner's attempt; only its supervisor
// retries failures.
//
// Name collisions and renames: Rust's `Attempt` (a `futures::Shared` boxed
// future) is probeAttempt, so the `attempt` method keeps its name; the
// `home_edit_retry_wait` method is editRetryWait beside the
// homeEditRetryWait field it reads. `(String, HarnessKind)` pairs are
// ProfileCandidate. Rust's associated functions without a receiver are
// package functions: `ProfileCatalog::new` is NewProfileCatalog,
// `new_with_config_path` NewProfileCatalogWithConfigPath, `build`
// BuildProfileCatalog, `with_probe` NewProfileCatalogWithProbe and
// `persist_legacy_review_to` persistLegacyReviewTo.
//
// The `#[cfg(test)]` fixture probes `flag_probe` and `gated_probe` belong in
// this file's tests.

package profilecatalog

import (
	"cmp"
	"context"
	"crypto/sha256"
	"errors"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Probe discovers a profile's default choices (Rust: `dyn Fn(String) ->
// BoxFuture<Result<ProfileConfig>>`, hidden from the docs; tests and the
// daemon's tests inject one). ctx is the cancellation of the definition the
// probe runs for. The catalog calls it when the attempt first runs; Rust
// calls it when the attempt is built and polls the future it returns from
// then.
type Probe func(ctx context.Context, profile string) (core.ProfileConfig, error)

// ModelProbe discovers a profile's choices with model selected.
type ModelProbe func(ctx context.Context, profile string, model string) (core.ProfileConfig, error)

// NoticeCallback shows the person a message (Rust: `Arc<dyn Fn(String)>`).
type NoticeCallback func(message string)

// QuotaReports is the daemon's latest quota report per profile, shared with
// the daemon (Rust: `Arc<Mutex<QuotaReportMap>>`). Hold the mutex while
// reading or changing Reports.
type QuotaReports struct {
	sync.Mutex
	Reports accounts.QuotaReportMap
}

// ProfileCandidate is one configured profile and its harness (Rust: a
// `(String, HarnessKind)` pair).
type ProfileCandidate struct {
	ID   string
	Kind core.HarnessKind
}

// cacheKey is Rust's `CacheKey`, `(String, Option<String>)`: a profile
// identity and the model a model-specific entry describes. hasModel false is
// the identity's default entry (None), which orders before every model.
type cacheKey struct {
	identity string
	model    string
	hasModel bool
}

func newCacheKey(identity string, model *string) cacheKey {
	if model == nil {
		return cacheKey{identity: identity}
	}
	return cacheKey{identity: identity, model: *model, hasModel: true}
}

// modelOption is the key's model as Rust's Option: nil for the default
// entry.
func (k cacheKey) modelOption() *string {
	if !k.hasModel {
		return nil
	}
	model := k.model
	return &model
}

// compareCacheKeys orders keys as Rust orders the tuple: by identity, then
// None before Some, then by model.
func compareCacheKeys(a, b cacheKey) int {
	if order := cmp.Compare(a.identity, b.identity); order != 0 {
		return order
	}
	if a.hasModel != b.hasModel {
		if a.hasModel {
			return 1
		}
		return -1
	}
	return cmp.Compare(a.model, b.model)
}

// quotaReportWait is how long the legacy upgrade waits for every offering
// profile's first quota report before treating profiles without one as
// subscriptions.
const quotaReportWait = 30 * time.Second

// refreshCheckInterval is how often ready entries are checked against the
// choice lifetime. An entry is probed again at most this long after it
// expires.
const refreshCheckInterval = 5 * 60 * time.Second

// homeWatchInterval is how often the owner checks every distinct profile
// home for catalog-input edits. A pass stats the three inputs per home and
// reads a file only when its stat changed or the file may have been written
// inside one mtime tick of the last pass, so the steady state does no file
// I/O beyond the stats.
const homeWatchInterval = 30 * time.Second

// homeEditRetryWait is how long after a failed re-probe the home watch tries
// the same edit again. Matches the expiry refresh cadence: a failure that
// persists, such as a missing login, is retried rarely rather than every
// pass.
const homeEditRetryWait = 5 * 60 * time.Second

// recentEditWindow: a file written more recently than this is re-hashed on
// every pass, even when its stat matches the stored one: filesystems with
// one-second mtime granularity can hide a same-length rewrite inside an
// unchanged (mtime, size) pair, and the recency read catches it on the next
// pass.
const recentEditWindow = 2 * 30 * time.Second

// homeCatalogInputs are the files in a profile home that decide what a
// session advertises: the harness's own configuration, which names the
// model provider and the default model, and the model catalog Mjolnir
// merges over a provider's list. Staging copies `home/<name>` verbatim, so
// the owner watches exactly what a probe will stage. Reads never touch these
// from a read path; the home watch and the probes read them, from background
// tasks.
var homeCatalogInputs = [3]string{"config.toml", "models.json", "settings.json"}

// fileStat is one watched input's cheap identity. A nil *fileStat is an
// absent file.
type fileStat struct {
	// mtime is the modification time in Unix nanoseconds; hasMtime false is
	// Rust's None, for a platform that reports none.
	mtime    int64
	hasMtime bool
	size     uint64
}

// watchedInput is one catalog input's observed state: its stat and the hash
// of its contents, taken when the stat last changed or the file may have
// been rewritten inside one mtime tick. Both are nil for an absent file.
type watchedInput struct {
	stat *fileStat
	hash *[32]byte
}

// homeFiles is the observed catalog-input state of one home. The digest
// compares content, not metadata: rewriting a file with identical bytes only
// refreshes the stat, so it never registers as an edit.
type homeFiles struct {
	inputs [3]watchedInput
	digest [32]byte
}

// probeOutcome is what one probe answers, and the catalog-input digest of
// the profile home as the probe started — what its staged copy was cut
// from. Recorded when the probe succeeds, so the home watch compares what
// the catalog serves against what it was built from, never against when the
// probe finished.
type probeOutcome struct {
	choices    core.ProfileConfig
	homeDigest *[32]byte
}

// retryDelays are the delays before the owner of a failed entry tries
// again. Every attempt stages the profile and launches its harness, so a
// failure that persists, such as a full disk or a missing login, is retried
// rarely rather than every minute. A changed definition or a session's
// report starts sooner.
var retryDelays = [6]time.Duration{
	1 * time.Second,
	5 * time.Second,
	30 * time.Second,
	60 * time.Second,
	5 * 60 * time.Second,
	15 * 60 * time.Second,
}

// attemptError is a failed attempt: its reason, whether a later attempt
// could succeed, and the catalog-input digest the attempt read before it ran
// — the home state the failure is about, which a retry window should arm
// against so a fresh edit bypasses it.
type attemptError struct {
	message    string
	retryable  bool
	homeDigest *[32]byte
}

func newAttemptError(err error, homeDigest *[32]byte) *attemptError {
	return &attemptError{
		message:    err.Error(),
		retryable:  !DiscoveryIsUnsupported(err),
		homeDigest: homeDigest,
	}
}

func (e *attemptError) Error() string { return e.message }

// probeAttempt is Rust's `Attempt`, a `futures::Shared` boxed future: one
// probe that runs at most once, started by whoever waits on it first, with
// every waiter sharing its result. Pointer identity is Rust's
// `Shared::ptr_eq`. Waiters share the outcome's slices and must not mutate
// them.
type probeAttempt struct {
	once    sync.Once
	run     func() (probeOutcome, *attemptError)
	done    chan struct{}
	outcome probeOutcome
	err     *attemptError
}

func newProbeAttempt(run func() (probeOutcome, *attemptError)) *probeAttempt {
	return &probeAttempt{run: run, done: make(chan struct{})}
}

// start runs the attempt on its own goroutine unless it already started,
// and returns the channel closed when it has finished.
func (a *probeAttempt) start() <-chan struct{} {
	a.once.Do(func() {
		go func() {
			a.outcome, a.err = a.run()
			close(a.done)
		}()
	})
	return a.done
}

// wait starts the attempt unless it already started and returns its
// result. ok is false when ctx ended first; the attempt carries on for its
// other waiters.
func (a *probeAttempt) wait(ctx context.Context) (outcome probeOutcome, failure *attemptError, ok bool) {
	select {
	case <-a.start():
		return a.outcome, a.err, true
	case <-ctx.Done():
		return probeOutcome{}, nil, false
	}
}

// startedAttempt is an attempt a lock holder claimed and must spawn once it
// releases the lock (Rust: a `(CacheKey, Attempt)` pair).
type startedAttempt struct {
	key     cacheKey
	attempt *probeAttempt
}

// entryKind is the variant of a cache entry.
type entryKind int

const (
	entryPending entryKind = iota
	entryReady
	entryFailed
)

// entry is Rust's `Entry` enum: Pending(attempt), Ready(choices) or
// Failed{attempt, error}.
type entry struct {
	kind entryKind
	// attempt is the owning attempt of a Pending or Failed entry.
	attempt *probeAttempt
	// choices is a Ready entry's answer.
	choices core.ProfileConfig
	// err is a Failed entry's message.
	err string
}

func (e entry) state() core.CapabilityState[core.ProfileConfig] {
	switch e.kind {
	case entryReady:
		return core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStateReady, Value: cloneChoices(e.choices)}
	case entryFailed:
		return core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStateFailed, Error: e.err}
	}
	return core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStatePending}
}

func (e entry) owns(attempt *probeAttempt) bool {
	return e.kind != entryReady && e.attempt == attempt
}

// definition is one configured profile identity's owner.
type definition struct {
	id string
	// config is the configuration the definition was claimed from, shared
	// and never mutated (Rust: `Arc<Config>`).
	config *core.Config
	// cancellation is a child of the catalog's (Rust: `child_token`).
	cancellation context.Context
	cancel       context.CancelFunc
}

// homeEditRetry is when the watch may launch the next re-probe for an
// identity whose edit re-probe failed, and the digest that failure was
// retrying (Rust: `(Instant, Option<[u8; 32]>)`).
type homeEditRetry struct {
	after time.Time
	armed *[32]byte
}

// inner is the catalog's state under its lock. Maps whose Rust iteration
// order matters are iterated in sorted key order (compareCacheKeys for
// cacheKey).
type inner struct {
	live        *core.Config
	publication *core.SharedSnapshot[core.ProfileCapabilitiesSnapshot]
	definitions map[string]definition
	entries     map[cacheKey]entry
	// wanted is the models a configuration selected, by profile identity,
	// from every configuration ensured, live or draft. Their efforts are
	// discovered as soon as the profile lists them; other models wait for a
	// reader.
	wanted map[string]map[string]bool
	// refreshing is the probes replacing out-of-date ready entries, which
	// stay served until a probe succeeds.
	refreshing map[cacheKey]*probeAttempt
	// homeStats is the catalog-input state of each watched home, a pure
	// polling optimization: content hashes are reused while a file's stat
	// holds, so a pass that no edit touched reads no file.
	homeStats map[string]homeFiles
	// homeSettled is the catalog-input digest each served entry was probed
	// from, recorded when that entry's probe succeeds. The home watch
	// compares every served entry of an identity against the current files,
	// so one entry's failed re-probe cannot hide behind a sibling's success:
	// the failed entry keeps its older digest and stays stale.
	homeSettled map[cacheKey][32]byte
	// homeEditRetryAfter is when the watch may launch the next re-probe for
	// an identity whose edit re-probe failed, and the digest that failure
	// was retrying. A pass whose current digest differs from the armed one
	// is a fresh edit: it bypasses the window, which a launch then re-arms
	// against the new digest. Bounded like the expiry refresh: a persistent
	// failure is retried rarely, not every pass.
	homeEditRetryAfter map[string]homeEditRetry
}

// probesAutomatically reports whether background work (periodic refresh,
// home watching) may probe def's profile. A retained definition keeps the
// configuration it was built from, so ask the live one: disabling a profile
// ends its automatic probes even while its definition serves explicit
// requests.
func (in *inner) probesAutomatically(def *definition) bool {
	config := in.live
	if config == nil {
		config = def.config
	}
	_, enabled := config.EnabledProfile(def.id)
	return enabled
}

// convergedAtOneDigest reports whether every non-Failed entry of identity
// carries a settled digest, all identical: the identity's served answers all
// describe one home state, so nothing is left retrying. A still-failing
// entry keeps its older digest and leaves the identity pending.
func (in *inner) convergedAtOneDigest(identity string) bool {
	var first *[32]byte
	for key, e := range in.entries {
		if key.identity != identity || e.kind == entryFailed {
			continue
		}
		digest, ok := in.homeSettled[key]
		if !ok || (first != nil && digest != *first) {
			return false
		}
		first = &digest
	}
	return true
}

// modelOffer is the model offer of the profile identity names: the live
// configuration's when it has that profile, else the latest draft's. Read
// at publication, so an edit applies without rediscovery.
func (in *inner) modelOffer(identity string) core.ModelOffer {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs Inner::model_offer")
}

// catalogProbe is the probe pair a test catalog answers from instead of
// discovery (Rust: `Option<(Arc<Probe>, Arc<ModelProbe>)>`).
type catalogProbe struct {
	probe      Probe
	modelProbe ModelProbe
}

// ProfileCatalog is the daemon's one capability cache. Use it through a
// pointer; the zero value is not ready.
type ProfileCatalog struct {
	cancellation context.Context
	mu           sync.Mutex
	inner        inner
	publisherMu  sync.Mutex
	publisher    func()
	noticeMu     sync.Mutex
	notice       NoticeCallback
	// revisions is tokio's watch channel of the publication revision.
	revisions                  *core.Watch[uint64]
	legacyReviewUpgradeStarted atomic.Bool
	configPath                 *string
	// quotaReports is the daemon's latest quota report per profile. Without
	// it every profile counts as a subscription when the legacy upgrade
	// picks models.
	quotaReportsMu sync.Mutex
	quotaReports   *QuotaReports
	probe          *catalogProbe
	// homeEditRetryWait is how long the home watch waits before re-probing
	// an identity whose edit re-probe failed. Tests shorten this to observe
	// the retry.
	homeEditRetryWait time.Duration
}

// NewProfileCatalog is Rust's `ProfileCatalog::new`. cancellation is the
// daemon's shutdown token; every definition's token is its child.
func NewProfileCatalog(cancellation context.Context) *ProfileCatalog {
	return &ProfileCatalog{
		cancellation:      cancellation,
		revisions:         core.NewWatch[uint64](0),
		homeEditRetryWait: homeEditRetryWait,
	}
}

// NewProfileCatalogWithConfigPath is a catalog that may persist the legacy
// review upgrade to configPath.
func NewProfileCatalogWithConfigPath(cancellation context.Context, configPath string) *ProfileCatalog {
	catalog := NewProfileCatalog(cancellation)
	catalog.configPath = &configPath
	return catalog
}

func (c *ProfileCatalog) SetPublisher(publisher func()) {
	c.publisherMu.Lock()
	defer c.publisherMu.Unlock()
	c.publisher = publisher
}

func (c *ProfileCatalog) SetQuotaReports(reports *QuotaReports) {
	c.quotaReportsMu.Lock()
	defer c.quotaReportsMu.Unlock()
	c.quotaReports = reports
}

// billingAmong is the profiles in profiles billed per use, and those whose
// billing is not known yet: no report classifies them and their harness
// does not decide. The upgrade writes its choice once, so it waits briefly
// for reports to arrive and refuses to guess afterwards. Without a report
// source (no daemon), every profile counts as a subscription.
func (c *ProfileCatalog) billingAmong(profiles []ProfileCandidate) (priced map[string]bool, unknown map[string]bool) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::billing_among")
}

func (c *ProfileCatalog) SetNotice(notice NoticeCallback) {
	c.noticeMu.Lock()
	defer c.noticeMu.Unlock()
	c.notice = notice
}

func (c *ProfileCatalog) publish() {
	// Only the wake-up matters to readers; concurrent publishes may store
	// the same revision, and each still wakes every waiter.
	c.revisions.SendReplace(c.revisions.Borrow() + 1)
	c.publisherMu.Lock()
	publisher := c.publisher
	c.publisherMu.Unlock()
	if publisher != nil {
		publisher()
	}
}

// revisionReceiver is a tokio watch::Receiver on the catalog's publication
// revisions: changed is closed by the first publish after the receiver last
// caught up.
type revisionReceiver struct {
	watch   *core.Watch[uint64]
	changed <-chan struct{}
	stop    func()
}

// subscribe is Rust's `revisions.subscribe()`: every publish so far counts
// as seen.
func (c *ProfileCatalog) subscribe() *revisionReceiver {
	changed, stop := c.revisions.Subscribe()
	return &revisionReceiver{watch: c.revisions, changed: changed, stop: stop}
}

// caughtUp marks every publish so far as seen, once changed has fired, so
// the next wait observes only later publishes.
func (r *revisionReceiver) caughtUp() {
	r.stop()
	r.changed, r.stop = r.watch.Subscribe()
}

// close withdraws the receiver (Rust: dropping it).
func (r *revisionReceiver) close() { r.stop() }

// Snapshot is the published capability snapshot, built on first read after
// a change and shared until the next one.
func (c *ProfileCatalog) Snapshot() core.SharedSnapshot[core.ProfileCapabilitiesSnapshot] {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::snapshot")
}

// Sync adopts config as the live configuration. Adoption keeps ready
// capability entries. A definition whose identity changed, or whose profile
// was enabled or disabled, is retired: its supervisors stop, and the next
// claim builds a fresh owner that reuses ready entries and retries failed
// ones.
func (c *ProfileCatalog) Sync(config *core.Config) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::sync")
}

func (c *ProfileCatalog) upgradeLegacyReview(ctx context.Context, attempt int) error {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::upgrade_legacy_review")
}

// persistLegacyReviewTo is Rust's `ProfileCatalog::persist_legacy_review_to`.
func persistLegacyReviewTo(path string, reviewers []core.SubagentChoice, disableAuto bool, definitions []upgradeDefinition, legacy *core.LegacyReview) (bool, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::persist_legacy_review_to")
}

// Ensure claims new definitions, default probes and the efforts of the
// models config selects under one lock. A Settings draft uses this same
// path without becoming the live configuration.
func (c *ProfileCatalog) Ensure(config *core.Config) {
	c.ensureProfiles(config, true, nil)
}

func (c *ProfileCatalog) ensureProfiles(config *core.Config, refreshLive bool, requested *string) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::ensure_profiles")
}

// RereadFailed reads a live profile's failed entries again, after a
// session's worker put choices in the profile cache that discovery reads
// first. A replaced entry ends its old supervisor, which no longer owns it.
func (c *ProfileCatalog) RereadFailed(profileID string) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::reread_failed")
}

// claimModels claims an entry for the current model and each selected model
// choices lists that has none. The current model's efforts are already
// known; a selected model is probed. Every probe launches the harness, and
// aggregating harnesses such as OpenCode advertise hundreds of models, so a
// model nobody selected is probed only when a reader asks for it.
// homeDigest is what the probe that produced choices staged from; the
// copied current-model entry serves exactly that, so it settles at the same
// digest. Call with the catalog's lock held.
func (c *ProfileCatalog) claimModels(in *inner, def *definition, identity string, choices *core.ProfileConfig, homeDigest *[32]byte) []startedAttempt {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::claim_models")
}

// startRefreshing checks ready entries against the choice lifetime until
// shutdown.
func (c *ProfileCatalog) startRefreshing() {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::start_refreshing")
}

// startWatchingHomes watches every distinct profile home for catalog-input
// edits until shutdown. The watch is the only code that stats or reads home
// files, and it runs here, never on a read path.
func (c *ProfileCatalog) startWatchingHomes() {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::start_watching_homes")
}

// watchHomes is one pass over every distinct profile home. For each
// profile, compare the digest its served answer was probed from — recorded
// when its last probe succeeded — against the digest of the home's files
// now. A mismatch means the home was edited after that probe read it (or no
// probe has succeeded yet), so re-probe exactly that profile through the
// keep-serving-the-old-answer refresh path. An edit therefore stays pending
// until a probe that read the edited home published: one that read it
// earlier records the old digest, and one that fails records nothing.
func (c *ProfileCatalog) watchHomes() {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::watch_homes")
}

// editRetryWait is how long after a failed edit re-probe the home watch
// tries again (Rust: the `home_edit_retry_wait` method).
func (c *ProfileCatalog) editRetryWait() time.Duration {
	return c.homeEditRetryWait
}

// refresh probes again every ready entry that expired selects. Readers keep
// the entry until a probe succeeds; a failed probe leaves it in place and
// records nothing. Returns the keys whose probes launched.
func (c *ProfileCatalog) refresh(expired func(key cacheKey, choices *core.ProfileConfig) bool) []cacheKey {
	return c.refreshWhere(expired, false)
}

// refreshWhere is refresh, which skips disabled profiles unless
// includeDisabled: only a request naming one refreshes it.
func (c *ProfileCatalog) refreshWhere(expired func(key cacheKey, choices *core.ProfileConfig) bool, includeDisabled bool) []cacheKey {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::refresh_where")
}

func (c *ProfileCatalog) driveRefresh(key cacheKey, attempt *probeAttempt, cancellation context.Context) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::drive_refresh")
}

func (c *ProfileCatalog) attempt(def *definition, model *string) *probeAttempt {
	return c.buildAttempt(def, model, false)
}

// attemptRefresh is a re-probe the owner launches after detecting a change
// the cache cannot see, such as an edited home: the harness must answer
// from the current files, so the process-global cache is bypassed. Readers
// that join a pending first probe keep the plain attempt above.
func (c *ProfileCatalog) attemptRefresh(def *definition, model *string) *probeAttempt {
	return c.buildAttempt(def, model, true)
}

func (c *ProfileCatalog) buildAttempt(def *definition, model *string, refresh bool) *probeAttempt {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::build_attempt")
}

func (c *ProfileCatalog) spawn(key cacheKey, attempt *probeAttempt) {
	go c.drive(key, attempt)
}

// drive supervises the entry at key while attempt owns it: it publishes the
// attempt's result and retries a retryable failure with backoff until the
// entry is replaced or its definition retired.
func (c *ProfileCatalog) drive(key cacheKey, attempt *probeAttempt) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::drive")
}

// fetch is what id offers under its `models` allow-list. Every reader of a
// profile's choices comes through here or through Snapshot.
func (c *ProfileCatalog) fetch(ctx context.Context, config *core.Config, id string, model *string) (core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::fetch")
}

// fetchUnrestricted is what id's harness advertises, before the allow-list.
// Only `mj doctor` asks for this, to compare the allow-list with the
// catalogue.
func (c *ProfileCatalog) fetchUnrestricted(ctx context.Context, config *core.Config, id string, model *string) (core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::fetch_unrestricted")
}

// fetchSettled: admission checks configured choices against the warm owner,
// including Muse.
func (c *ProfileCatalog) fetchSettled(ctx context.Context, config *core.Config, id string, model *string) (core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::fetch_settled")
}

// fetchLive reads a Muse profile live instead of serving a retained entry.
// New models and effort tiers appear server-side at any time, so a retained
// answer is never served: every read probes the harness that actually
// runs. An in-flight probe is still joined so concurrent readers share it,
// and failures are served as-is so a broken harness keeps the supervisor's
// backoff instead of being hammered on every read. The supervisor still
// publishes each result, so the snapshot converges.
func (c *ProfileCatalog) fetchLive(ctx context.Context, key cacheKey, model *string, cancellation context.Context, revisions *revisionReceiver) (core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::fetch_live")
}

// Live is a copy of the adopted configuration.
func (c *ProfileCatalog) Live() (core.Config, error) {
	in, unlock := c.lock()
	defer unlock()
	if in.live == nil {
		return core.Config{}, errors.New("the profile catalogue has not adopted a configuration")
	}
	return cloneConfig(in.live), nil
}

// cloneConfig is Rust's `Config::clone`: a copy that shares no map or slice
// a caller could mutate with the original.
func cloneConfig(config *core.Config) core.Config {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs Config::clone")
}

// ModelOffer is the live model offer of profile id.
func (c *ProfileCatalog) ModelOffer(id string) core.ModelOffer {
	in, unlock := c.lock()
	defer unlock()
	if in.live == nil {
		return core.ModelOffer{}
	}
	profile, err := in.live.NamedProfile(id)
	if err != nil {
		return core.ModelOffer{}
	}
	return profile.ModelOffer()
}

// CachedModelEfforts is the efforts profile id offers for model when a
// completed discovery of that model, or of the default model it is, already
// holds them, including a confirmed empty list; false is unknown. Never
// starts a probe. Muse is always unknown: its catalogue is read live because
// the service changes it, so a retained answer can be stale.
func (c *ProfileCatalog) CachedModelEfforts(id string, model string) ([]core.SessionConfigChoice, bool) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::cached_model_efforts")
}

func (c *ProfileCatalog) Candidates() ([]ProfileCandidate, error) {
	config, err := c.Live()
	if err != nil {
		return nil, err
	}
	candidates := []ProfileCandidate{}
	for id, profile := range config.EnabledProfiles() {
		candidates = append(candidates, ProfileCandidate{ID: id, Kind: profile.Kind})
	}
	return candidates, nil
}

func (c *ProfileCatalog) ReadyCandidates(model string) ([]ProfileCandidate, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::ready_candidates")
}

func (c *ProfileCatalog) ConfiguredCandidates() ([]ProfileCandidate, error) {
	config, err := c.Live()
	if err != nil {
		return nil, err
	}
	candidates := []ProfileCandidate{}
	for id, profile := range config.EnabledProfiles() {
		candidates = append(candidates, ProfileCandidate{ID: id, Kind: profile.Kind})
	}
	return candidates, nil
}

func (c *ProfileCatalog) ModelCapabilities(ctx context.Context, profile string, model string) (core.ProfileConfig, error) {
	return c.liveCapabilities(ctx, profile, &model)
}

// UnrestrictedCapabilities is the catalogue id's harness advertises,
// ignoring its `models` allow-list. See fetchUnrestricted.
func (c *ProfileCatalog) UnrestrictedCapabilities(ctx context.Context, id string) (core.ProfileConfig, error) {
	return c.readLive(ctx, id, nil, true)
}

func (c *ProfileCatalog) liveCapabilities(ctx context.Context, id string, model *string) (core.ProfileConfig, error) {
	return c.readLive(ctx, id, model, false)
}

func (c *ProfileCatalog) readLive(ctx context.Context, id string, model *string, unrestricted bool) (core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::read_live")
}

// Capabilities reads every profile in profiles concurrently, in order; the
// first failure in that order is the error.
func (c *ProfileCatalog) Capabilities(ctx context.Context, profiles []string) ([]core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::capabilities")
}

// AdmissionOptions is the sub-agent options for each of models (a set),
// read from the warm owners. On failure the error is that of the first
// failing model in sorted order.
func (c *ProfileCatalog) AdmissionOptions(ctx context.Context, config *core.Config, parent string, models map[string]bool) (map[string]core.SubagentOptions, error) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::admission_options")
}

func (c *ProfileCatalog) OptionsFor(ctx context.Context, config *core.Config, parent string, model *string) (core.SubagentOptions, error) {
	return SubagentOptionsWith(ctx, config, parent, model, func(ctx context.Context, id string, model *string) (core.ProfileConfig, error) {
		return c.fetch(ctx, config, id, model)
	})
}

// lock takes the catalog's lock (Rust: the MutexGuard); call unlock when
// done.
func (c *ProfileCatalog) lock() (in *inner, unlock func()) {
	c.mu.Lock()
	return &c.inner, c.mu.Unlock
}

// edit is lock, discarding the cached publication first.
func (c *ProfileCatalog) edit() (in *inner, unlock func()) {
	in, unlock = c.lock()
	in.publication = nil
	return in, unlock
}

// BuildProfileCatalog is a catalog that answers from probe and modelProbe
// instead of discovery (Rust: `ProfileCatalog::build`, hidden from the
// docs).
func BuildProfileCatalog(cancellation context.Context, probe Probe, modelProbe ModelProbe) *ProfileCatalog {
	catalog := NewProfileCatalog(cancellation)
	catalog.probe = &catalogProbe{probe: probe, modelProbe: modelProbe}
	return catalog
}

// NewProfileCatalogWithProbe is a catalog whose default and model-specific
// probes are both probe (Rust: `ProfileCatalog::with_probe`, hidden from the
// docs).
func NewProfileCatalogWithProbe(probe Probe) *ProfileCatalog {
	return BuildProfileCatalog(context.Background(), probe, func(ctx context.Context, profile string, _ string) (core.ProfileConfig, error) {
		return probe(ctx, profile)
	})
}

// SyncNow adopts config and waits for every enabled profile's default
// choices and each listed model's efforts (hidden from the docs in Rust).
func (c *ProfileCatalog) SyncNow(ctx context.Context, config *core.Config) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs ProfileCatalog::sync_now")
}

// upgradeDefinition is one enabled profile as the legacy review upgrade saw
// it: its id, its capability key, and its `models` list. The key leaves
// `models` out, so the save also compares the list: a profile that stopped
// offering a chosen model must not get it saved as a reviewer.
type upgradeDefinition struct {
	id       string
	identity string
	models   core.ModelOffer
}

func upgradeDefinitions(config *core.Config) []upgradeDefinition {
	definitions := []upgradeDefinition{}
	for id, profile := range config.EnabledProfiles() {
		definitions = append(definitions, upgradeDefinition{id: id, identity: profile.CapabilitiesKey(id), models: profile.ModelOffer()})
	}
	return definitions
}

// definitionHome is the expanded home directory a definition's probe stages
// from, when the profile is still enabled in the definition's configuration.
func definitionHome(def *definition) (string, bool) {
	profile, ok := def.config.EnabledProfile(def.id)
	if !ok {
		return "", false
	}
	home, err := core.ExpandLocal(profile.Home)
	if err != nil {
		return profile.Home, true
	}
	return home, true
}

// hashHomeInputs is the catalog-input state of home, read in full.
// pollHomeFiles calls this only for inputs whose stat changed or may have
// been rewritten inside one mtime tick; probes call it at launch to record
// what they staged from. False on a hard I/O error.
func hashHomeInputs(home string) ([3]watchedInput, bool) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs hash_home_inputs")
}

// pollHomeFiles is the current catalog-input state of home. previous is the
// state the last watch pass saw: an input whose stat is unchanged keeps its
// content hash, so a pass that no edit touched reads no file. An input
// whose mtime is recent is re-hashed even when its stat matches: filesystems
// with one-second mtime granularity can hide a same-length rewrite inside an
// unchanged (mtime, size) pair. A hard I/O error (anything but a missing
// input) returns false; the home keeps its previous state and is retried on
// the next pass rather than being treated as edited.
func pollHomeFiles(home string, previous *homeFiles) (homeFiles, bool) {
	panic("unported: mj-profile-catalog/src/profile_catalog.rs poll_home_files")
}

// hashHomeInput is the content hash of one catalog input; nil when the
// input is absent. A file that cannot be read races the directory scan and
// is observed as absent for this pass; a spurious re-probe from that is
// safe, and the next pass sees the file again.
func hashHomeInput(path string, present bool) *[32]byte {
	if !present {
		return nil
	}
	bytes, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	hash := sha256.Sum256(bytes)
	return &hash
}

// homeDigest is the identity of a home's catalog inputs: every input's name
// and either an absent marker or its content hash.
func homeDigest(inputs *[3]watchedInput) [32]byte {
	hasher := sha256.New()
	for index, name := range homeCatalogInputs {
		hasher.Write([]byte(name))
		if hash := inputs[index].hash; hash != nil {
			hasher.Write([]byte{1})
			hasher.Write(hash[:])
		} else {
			hasher.Write([]byte{0})
		}
	}
	var digest [32]byte
	copy(digest[:], hasher.Sum(nil))
	return digest
}

// TestConfig is a configuration with the named profiles enabled (Rust:
// `test_config`, hidden from the docs).
func TestConfig(profiles []ProfileCandidate) core.Config {
	config := core.DefaultConfig()
	config.Profiles = map[string]core.HarnessProfile{}
	for _, profile := range profiles {
		config.Profiles[profile.ID] = core.NewHarnessProfile(profile.Kind, filepath.Join("/home/agent", profile.ID))
	}
	return config
}

// CountingProbe is a probe that answers with the profile's own id and
// counts its calls, so a test can prove that a warm answer runs none.
func CountingProbe(calls *atomic.Int64) Probe {
	return func(_ context.Context, profile string) (core.ProfileConfig, error) {
		calls.Add(1)
		return TestChoices(profile), nil
	}
}

// TestChoices is the capabilities a fixture probe reports for a profile.
func TestChoices(profile string) core.ProfileConfig {
	model := profile + "-model"
	return core.ProfileConfig{
		Model:      &model,
		Models:     []core.SessionConfigChoice{{Value: profile + "-model", Name: profile + " model"}},
		Efforts:    []core.SessionConfigChoice{},
		ObservedAt: 1_700_000_000,
	}
}
