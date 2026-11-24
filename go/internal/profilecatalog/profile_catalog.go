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
	"fmt"
	"io/fs"
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProbeRun is the work a probe hands back, run when the attempt that holds
// it first runs (Rust: the boxed future a probe returns, polled from then).
// ctx is the cancellation of the definition the probe runs for; its end
// stands for Rust dropping the future.
type ProbeRun func(ctx context.Context) (core.ProfileConfig, error)

// Probe discovers a profile's default choices (Rust: `dyn Fn(String) ->
// BoxFuture<Result<ProfileConfig>>`, hidden from the docs; tests and the
// daemon's tests inject one). As in Rust, the catalog calls it when it
// builds an attempt, under the catalog's lock, and runs what it returns when
// the attempt first runs. It must not call back into the catalog.
type Probe func(profile string) ProbeRun

// ModelProbe discovers a profile's choices with model selected.
type ModelProbe func(profile string, model string) ProbeRun

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

// sortedCacheKeys is m's keys in Rust's BTreeMap order.
func sortedCacheKeys[V any](m map[cacheKey]V) []cacheKey {
	return slices.SortedFunc(maps.Keys(m), compareCacheKeys)
}

// debugModel is a key's model as Rust's `{:?}` writes an Option<String>.
func debugModel(key cacheKey) string {
	if !key.hasModel {
		return "None"
	}
	return fmt.Sprintf("Some(%q)", key.model)
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

// fileStatsEqual compares two stats as Rust compares `Option<FileStat>`.
func fileStatsEqual(a, b *fileStat) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// hashesEqual compares two content hashes as Rust compares
// `Option<[u8; 32]>`.
func hashesEqual(a, b *[32]byte) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
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
	if in.live != nil {
		for _, id := range slices.Sorted(maps.Keys(in.live.Profiles)) {
			profile := in.live.Profiles[id]
			if profile.CapabilitiesKey(id) == identity {
				return profile.ModelOffer()
			}
		}
	}
	if def, ok := in.definitions[identity]; ok {
		if profile, ok := def.config.Profiles[def.id]; ok {
			return profile.ModelOffer()
		}
	}
	return core.ModelOffer{}
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
	c.quotaReportsMu.Lock()
	reports := c.quotaReports
	c.quotaReportsMu.Unlock()
	priced, unknown = map[string]bool{}, map[string]bool{}
	if reports == nil {
		return priced, unknown
	}
	reports.Lock()
	defer reports.Unlock()
	for _, profile := range profiles {
		var report *client.ProfileQuota
		if publication, ok := reports.Reports[profile.ID]; ok {
			report = publication.Report
		}
		switch client.ProfileQuotaBillingFor(profile.Kind, report) {
		case client.QuotaBillingUsage:
			priced[profile.ID] = true
		case client.QuotaBillingSubscription:
		case client.QuotaBillingUnknown:
			unknown[profile.ID] = true
		}
	}
	return priced, unknown
}

func (c *ProfileCatalog) SetNotice(notice NoticeCallback) {
	c.noticeMu.Lock()
	defer c.noticeMu.Unlock()
	c.notice = notice
}

// currentNotice is the notice callback, if one is set.
func (c *ProfileCatalog) currentNotice() NoticeCallback {
	c.noticeMu.Lock()
	defer c.noticeMu.Unlock()
	return c.notice
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
	in, unlock := c.lock()
	defer unlock()
	if in.publication != nil {
		return *in.publication
	}
	profiles := make(map[string]core.ProfileCapabilities, len(in.definitions))
	for identity := range in.definitions {
		allowed := in.modelOffer(identity)
		choices := core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStatePending}
		if e, ok := in.entries[newCacheKey(identity, nil)]; ok {
			choices = e.state()
		}
		if choices.Kind == core.CapabilityStateReady {
			choices.Value = choices.Value.RestrictedTo(&allowed)
		}
		// Only models whose efforts were claimed are listed.
		efforts := map[string]core.CapabilityState[[]core.SessionConfigChoice]{}
		if choices.Kind == core.CapabilityStateReady {
			for _, model := range choices.Value.Models {
				value := model.Value
				e, ok := in.entries[newCacheKey(identity, &value)]
				if !ok {
					continue
				}
				var state core.CapabilityState[[]core.SessionConfigChoice]
				switch e.kind {
				case entryReady:
					state = core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStateReady, Value: nonNilChoices(slices.Clone(e.choices.Efforts))}
				case entryFailed:
					state = core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStateFailed, Error: e.err}
				default:
					state = core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStatePending}
				}
				efforts[value] = state
			}
		}
		profiles[identity] = core.ProfileCapabilities{Choices: choices, Efforts: efforts}
	}
	snapshot := core.NewSharedSnapshot(core.ProfileCapabilitiesSnapshot{Profiles: profiles})
	in.publication = &snapshot
	return snapshot
}

// nonNilChoices is choices with nil as a present empty list, as Rust's Vec
// serializes.
func nonNilChoices(choices []core.SessionConfigChoice) []core.SessionConfigChoice {
	if choices == nil {
		return []core.SessionConfigChoice{}
	}
	return choices
}

// Sync adopts config as the live configuration. Adoption keeps ready
// capability entries. A definition whose identity changed, or whose profile
// was enabled or disabled, is retired: its supervisors stop, and the next
// claim builds a fresh owner that reuses ready entries and retries failed
// ones.
func (c *ProfileCatalog) Sync(config *core.Config) {
	in, unlock := c.lock()
	first := in.live == nil
	changed := in.live == nil || !configsEqual(in.live, config)
	// The allow-list is applied when the snapshot is built, so a cached
	// snapshot would keep serving the old one.
	if in.live == nil || !modelOffersByProfileEqual(in.live, config) {
		in.publication = nil
	}
	if old := in.live; old != nil {
		for id, profile := range old.Profiles {
			identity := profile.CapabilitiesKey(id)
			updated, err := config.NamedProfile(id)
			if err != nil || updated.CapabilitiesKey(id) != identity || updated.Enabled != profile.Enabled {
				if def, ok := in.definitions[identity]; ok {
					def.cancel()
				}
			}
		}
	}
	live := cloneConfig(config)
	in.live = &live
	unlock()
	if first {
		c.startRefreshing()
		c.startWatchingHomes()
	}
	if config.Review.IsLegacy() && c.configPath != nil && !c.legacyReviewUpgradeStarted.Swap(true) {
		go c.upgradeLegacyReviewUntilDone()
	}
	c.Ensure(config)
	if changed {
		c.publish()
	}
}

// upgradeLegacyReviewUntilDone retries the legacy review upgrade with a
// growing delay until it completes or the catalog shuts down.
func (c *ProfileCatalog) upgradeLegacyReviewUntilDone() {
	for attempt := 1; ; attempt++ {
		err := c.upgradeLegacyReview(c.cancellation, attempt)
		if c.cancellation.Err() != nil || err == nil {
			return
		}
		slog.Warn("legacy review autoconfiguration did not complete", "attempt", attempt, "error", err.Error())
		timer := time.NewTimer(min(time.Duration(attempt)*5*time.Second, 300*time.Second))
		select {
		case <-c.cancellation.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
	}
}

func (c *ProfileCatalog) upgradeLegacyReview(ctx context.Context, attempt int) error {
	_ = attempt
	discoveryConfig, err := c.Live()
	if err != nil {
		return err
	}
	definitions := upgradeDefinitions(&discoveryConfig)
	if discoveryConfig.Review.Legacy == nil {
		return errors.New("review is no longer legacy")
	}
	legacy := core.DeepClone(*discoveryConfig.Review.Legacy)
	harnesses, err := c.Candidates()
	if err != nil {
		return err
	}
	ids := make([]string, len(harnesses))
	for index, harness := range harnesses {
		ids[index] = harness.ID
	}
	// Each profile has its own deadline so one stuck probe cannot hide
	// results from profiles that answered. Partial results remain transient
	// until all profiles answer or the retry budget is exhausted.
	type discovered struct {
		choices  core.ProfileConfig
		err      error
		timedOut bool
	}
	results := make([]discovered, len(ids))
	var waiting sync.WaitGroup
	for index, id := range ids {
		waiting.Go(func() {
			deadline, cancel := context.WithTimeout(ctx, 30*time.Second)
			defer cancel()
			choices, err := c.liveCapabilities(deadline, id, nil)
			timedOut := err != nil && errors.Is(err, context.DeadlineExceeded) && errors.Is(deadline.Err(), context.DeadlineExceeded)
			results[index] = discovered{choices: choices, err: err, timedOut: timedOut}
		})
	}
	waiting.Wait()
	if err := ctx.Err(); err != nil {
		return err
	}
	omitted := []string{}
	available := []accounts.AvailableProfile{}
	for index, id := range ids {
		result := results[index]
		switch {
		case result.timedOut:
			slog.Warn("profile discovery timed out during legacy review upgrade", "profile", id)
			omitted = append(omitted, id)
		case result.err != nil:
			slog.Warn("profile discovery failed during legacy review upgrade", "profile", id, "error", result.err.Error())
			omitted = append(omitted, id)
		default:
			available = append(available, accounts.AvailableProfile{ID: id, Choices: result.choices})
		}
	}
	pinnedAvailable := legacy.Profile != nil && slices.ContainsFunc(available, func(profile accounts.AvailableProfile) bool {
		return profile.ID == *legacy.Profile
	})
	if len(omitted) > 0 && !pinnedAvailable {
		return fmt.Errorf("legacy review upgrade waiting for profiles: %s", strings.Join(omitted, ", "))
	}
	offering := []ProfileCandidate{}
	for _, harness := range harnesses {
		if slices.ContainsFunc(available, func(profile accounts.AvailableProfile) bool { return profile.ID == harness.ID }) {
			offering = append(offering, harness)
		}
	}
	deadline := time.Now().Add(quotaReportWait)
	var usagePriced, unclassified map[string]bool
	for {
		priced, unknown := c.billingAmong(offering)
		if len(unknown) == 0 || !time.Now().Before(deadline) {
			usagePriced, unclassified = priced, unknown
			break
		}
		timer := time.NewTimer(250 * time.Millisecond)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
	}
	// A profile with no quota report yet may be a paid API. Persisting a
	// guess would keep its models in the reviewers for good, so wait for a
	// later attempt whenever such a profile offers a drafted family.
	waitingFor := accounts.UnclassifiedInDraft(&legacy, available, usagePriced, unclassified)
	if len(waitingFor) > 0 {
		return fmt.Errorf("legacy review upgrade waiting for quota reports: %s", strings.Join(waitingFor, ", "))
	}
	reviewers, known, adjustments := accounts.UpgradeReviewers(ctx, &legacy, available, usagePriced,
		func(ctx context.Context, profile, model string) (core.ProfileConfig, error) {
			return c.ModelCapabilities(ctx, profile, model)
		})
	if !known {
		return errors.New("reviewer efforts remain unknown")
	}
	disableAuto := len(reviewers) == 0
	if disableAuto && (pinnedAvailable || len(omitted) > 0) {
		return errors.New("legacy review upgrade found no reviewers; waiting for matching models")
	}
	count := len(reviewers)
	configuredNotice := ""
	if len(reviewers) > 0 {
		rows := make([]string, len(reviewers))
		for index, choice := range reviewers {
			if choice.Effort != nil {
				rows[index] = fmt.Sprintf("%s %s", choice.Model, *choice.Effort)
			} else {
				rows[index] = fmt.Sprintf("%s default", choice.Model)
			}
		}
		changes := make([]string, len(adjustments))
		for index, adjustment := range adjustments {
			used := "default"
			if adjustment.Used != nil {
				used = *adjustment.Used
			}
			changes[index] = fmt.Sprintf("%s uses %s (%s is not offered)", adjustment.Model, used, adjustment.Requested)
		}
		if len(changes) == 0 {
			configuredNotice = fmt.Sprintf("Code review reviewers were configured from your profiles: %s.", strings.Join(rows, ", "))
		} else {
			configuredNotice = fmt.Sprintf("Code review reviewers were configured from your profiles: %s; %s.", strings.Join(rows, ", "), strings.Join(changes, "; "))
		}
	}
	if c.configPath == nil {
		return errors.New("catalog has no config path for review upgrade")
	}
	wrote, err := persistLegacyReviewTo(*c.configPath, reviewers, disableAuto, definitions, &legacy)
	if err != nil {
		return err
	}
	if wrote {
		if disableAuto {
			message := "Automatic code review was turned off: none of your profiles offers a model Autoconfigure supports. Choose reviewers in Settings › Code Review."
			slog.Warn(message)
			if notice := c.currentNotice(); notice != nil {
				notice(message)
			}
		}
		if configuredNotice != "" {
			if notice := c.currentNotice(); notice != nil {
				notice(configuredNotice)
			}
		}
		slog.Info("persisted legacy Auto review configuration to config.toml", "count", count)
	}
	return nil
}

// persistLegacyReviewTo is Rust's `ProfileCatalog::persist_legacy_review_to`.
func persistLegacyReviewTo(path string, reviewers []core.SubagentChoice, disableAuto bool, definitions []upgradeDefinition, legacy *core.LegacyReview) (bool, error) {
	if len(reviewers) == 0 && !disableAuto {
		return false, errors.New("cannot persist an empty reviewer upgrade")
	}
	if reviewers == nil {
		reviewers = []core.SubagentChoice{}
	}
	_, wrote, err := core.ConfigUpdateTo(path, func(config *core.Config) (bool, error) {
		if !config.Review.IsLegacy() {
			return false, nil
		}
		if !reflect.DeepEqual(config.Review.Legacy, legacy) {
			return false, errors.New("legacy review settings changed during reviewer discovery")
		}
		if !upgradeDefinitionsEqual(upgradeDefinitions(config), definitions) {
			return false, errors.New("profile definitions changed during reviewer discovery")
		}
		// With no reviewers to choose, clearing the pending request is what
		// turns review off.
		config.Review.Reviewers = reviewers
		config.Review.ClearLegacy()
		return true, nil
	})
	if err != nil {
		return false, err
	}
	return wrote, nil
}

// Ensure claims new definitions, default probes and the efforts of the
// models config selects under one lock. A Settings draft uses this same
// path without becoming the live configuration.
func (c *ProfileCatalog) Ensure(config *core.Config) {
	c.ensureProfiles(config, true, nil)
}

func (c *ProfileCatalog) ensureProfiles(config *core.Config, refreshLive bool, requested *string) {
	ensured := cloneConfig(config)
	shared := &ensured
	var started []startedAttempt
	in, unlock := c.lock()
	for _, id := range slices.Sorted(maps.Keys(shared.Profiles)) {
		profile := shared.Profiles[id]
		if requested != nil {
			if *requested != id {
				continue
			}
		} else if !profile.Enabled {
			continue
		}
		identity := profile.CapabilitiesKey(id)
		wanted := in.wanted[identity]
		if wanted == nil {
			wanted = map[string]bool{}
			in.wanted[identity] = wanted
		}
		for _, model := range shared.SelectedModels(id) {
			wanted[model] = true
		}
		// Muse catalogues change server-side without a managed-pin bump, so
		// a retained entry hides models and efforts people expect. Every
		// warm pass re-probes a Muse profile instead of keeping its entries;
		// reads below never serve them either.
		live := profile.Kind == core.HarnessKindMuse
		if def, ok := in.definitions[identity]; ok {
			if (!live || !refreshLive) && def.cancellation.Err() == nil {
				// Keep successful entries and the in-flight attempt. A
				// failed owner's next retry uses refreshed credentials.
				modelsChanged := true
				if old, ok := def.config.Profiles[def.id]; ok {
					modelsChanged = !modelOffersEqual(old.ModelOffer(), profile.ModelOffer())
				}
				def.config = shared
				in.definitions[identity] = def
				if modelsChanged {
					in.publication = nil
				}
				key := newCacheKey(identity, nil)
				if e, ok := in.entries[key]; ok && e.kind == entryReady {
					started = append(started, c.claimModels(in, &def, identity, &e.choices, in.settledDigest(key))...)
				}
				continue
			}
			if !live {
				for key, e := range in.entries {
					if key.identity == identity && e.kind != entryReady {
						delete(in.entries, key)
					}
				}
			}
		}
		in.publication = nil
		if live {
			// End the previous owner's supervisors so only the fresh attempt
			// below can publish for this identity.
			if def, ok := in.definitions[identity]; ok {
				def.cancel()
			}
			for key := range in.entries {
				if key.identity == identity {
					delete(in.entries, key)
				}
			}
		}
		cancellation, cancel := context.WithCancel(c.cancellation)
		def := definition{id: id, config: shared, cancellation: cancellation, cancel: cancel}
		key := newCacheKey(identity, nil)
		var attempt *probeAttempt
		if _, ok := in.entries[key]; !ok {
			attempt = c.attempt(&def, nil)
		}
		in.definitions[identity] = def
		if e, ok := in.entries[key]; ok && e.kind == entryReady {
			started = append(started, c.claimModels(in, &def, identity, &e.choices, in.settledDigest(key))...)
		}
		if attempt != nil {
			in.entries[key] = entry{kind: entryPending, attempt: attempt}
			started = append(started, startedAttempt{key: key, attempt: attempt})
		}
	}
	unlock()
	if len(started) > 0 {
		c.publish()
	}
	for _, claimed := range started {
		c.spawn(claimed.key, claimed.attempt)
	}
}

// settledDigest is the digest key's entry was probed from, if one is
// recorded.
func (in *inner) settledDigest(key cacheKey) *[32]byte {
	digest, ok := in.homeSettled[key]
	if !ok {
		return nil
	}
	return &digest
}

// RereadFailed reads a live profile's failed entries again, after a
// session's worker put choices in the profile cache that discovery reads
// first. A replaced entry ends its old supervisor, which no longer owns it.
func (c *ProfileCatalog) RereadFailed(profileID string) {
	var started []startedAttempt
	in, unlock := c.edit()
	if in.live == nil {
		unlock()
		return
	}
	profile, err := in.live.NamedProfile(profileID)
	if err != nil {
		unlock()
		return
	}
	identity := profile.CapabilitiesKey(profileID)
	def, ok := in.definitions[identity]
	if !ok || def.cancellation.Err() != nil {
		unlock()
		return
	}
	for _, key := range sortedCacheKeys(in.entries) {
		if key.identity != identity || in.entries[key].kind != entryFailed {
			continue
		}
		attempt := c.attempt(&def, key.modelOption())
		in.entries[key] = entry{kind: entryPending, attempt: attempt}
		started = append(started, startedAttempt{key: key, attempt: attempt})
	}
	unlock()
	if len(started) > 0 {
		c.publish()
	}
	for _, claimed := range started {
		c.spawn(claimed.key, claimed.attempt)
	}
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
	var started []startedAttempt
	for _, model := range choices.Models {
		value := model.Value
		modelKey := newCacheKey(identity, &value)
		current := choices.Model != nil && *choices.Model == value
		selected := false
		for wanted := range in.wanted[identity] {
			if offer, ok := core.ModelCatalogResolve(choices.Models, wanted); ok && offer.Value == value {
				selected = true
				break
			}
		}
		if _, exists := in.entries[modelKey]; exists || !(current || selected) {
			continue
		}
		in.publication = nil
		if current {
			in.entries[modelKey] = entry{kind: entryReady, choices: cloneChoices(*choices)}
			if homeDigest != nil {
				in.homeSettled[modelKey] = *homeDigest
			}
		} else {
			pending := c.attempt(def, &value)
			in.entries[modelKey] = entry{kind: entryPending, attempt: pending}
			started = append(started, startedAttempt{key: modelKey, attempt: pending})
		}
	}
	return started
}

// startRefreshing checks ready entries against the choice lifetime until
// shutdown.
func (c *ProfileCatalog) startRefreshing() {
	go func() {
		// Startup discovery is already current; the first check waits.
		ticker := time.NewTicker(refreshCheckInterval)
		defer ticker.Stop()
		for {
			select {
			case <-c.cancellation.Done():
				return
			case <-ticker.C:
			}
			c.refresh(func(_ cacheKey, choices *core.ProfileConfig) bool { return !IsCurrent(choices) })
		}
	}()
}

// startWatchingHomes watches every distinct profile home for catalog-input
// edits until shutdown. The watch is the only code that stats or reads home
// files, and it runs here, never on a read path.
func (c *ProfileCatalog) startWatchingHomes() {
	go func() {
		// Startup discovery already reads the homes; the first pass only
		// baselines them.
		ticker := time.NewTicker(homeWatchInterval)
		defer ticker.Stop()
		for {
			select {
			case <-c.cancellation.Done():
				return
			case <-ticker.C:
			}
			c.watchHomes()
		}
	}()
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
	in, unlock := c.lock()
	identitiesByHome := map[string]map[string]bool{}
	for identity, def := range in.definitions {
		if def.cancellation.Err() != nil {
			continue
		}
		if !in.probesAutomatically(&def) {
			continue
		}
		profile := def.config.Profiles[def.id]
		home, err := core.ExpandLocal(profile.Home)
		if err != nil {
			home = profile.Home
		}
		if identitiesByHome[home] == nil {
			identitiesByHome[home] = map[string]bool{}
		}
		identitiesByHome[home][identity] = true
	}
	stats := maps.Clone(in.homeStats)
	settled := maps.Clone(in.homeSettled)
	served := []cacheKey{}
	for _, key := range sortedCacheKeys(in.entries) {
		if in.entries[key].kind == entryReady {
			served = append(served, key)
		}
	}
	unlock()

	type polledHome struct {
		files homeFiles
		ok    bool
	}
	polled := map[string]polledHome{}
	for home := range identitiesByHome {
		var previous *homeFiles
		if files, ok := stats[home]; ok {
			previous = &files
		}
		files, ok := pollHomeFiles(home, previous)
		polled[home] = polledHome{files: files, ok: ok}
	}
	stale := map[string]bool{}
	converged := map[string]bool{}
	digestByIdentity := map[string][32]byte{}
	updates := map[string]homeFiles{}
	for home, result := range polled {
		if !result.ok {
			// A hard read error keeps the previous stats, so the next pass
			// compares against the same knowledge.
			continue
		}
		current := result.files
		// A probe reads the whole home; the poll may reuse a hash taken
		// before a timestamp-preserving rewrite (cp -p) hid an edit from the
		// stat. When the poll saw no change yet a settled digest disagrees,
		// the poll is behind: re-hash in full so the identity converges
		// instead of churning a re-probe every pass.
		previous, hadPrevious := stats[home]
		pollBlind := hadPrevious && previous.digest == current.digest && func() bool {
			for identity := range identitiesByHome[home] {
				for _, key := range served {
					if digest, ok := settled[key]; key.identity == identity && ok && digest != current.digest {
						return true
					}
				}
			}
			return false
		}()
		if pollBlind {
			if fresh, ok := hashHomeInputs(home); ok {
				current = homeFiles{digest: homeDigest(&fresh), inputs: fresh}
			}
		}
		// The stat cache is only a polling optimization; store whatever the
		// pass observed.
		updates[home] = current
		// Every served answer must have been read from the home as it now
		// is. A Failed or Pending entry serves no complete answer — its own
		// supervisor finishes it, and its success records a digest that
		// joins the check — so it neither settles nor blocks convergence.
		for identity := range identitiesByHome[home] {
			everyEntrySettled := true
			for _, key := range served {
				if key.identity != identity {
					continue
				}
				if digest, ok := settled[key]; !ok || digest != current.digest {
					everyEntrySettled = false
					break
				}
			}
			if everyEntrySettled {
				converged[identity] = true
			} else {
				stale[identity] = true
			}
			digestByIdentity[identity] = current.digest
		}
	}
	// A failed re-probe records nothing, so the identity stays stale and the
	// next pass tries again; bound how often the re-probe repeats, matching
	// the expiry refresh cadence for persistent failures. Only failures arm
	// the window, against the digest they were correcting: a pass whose
	// current digest differs from the armed one is a fresh edit and
	// bypasses it. An identity observed converged this pass has nothing left
	// to retry: its window is cleared before deciding what launches, so a
	// follow-up edit after a success re-probes on the next pass instead of
	// waiting it out.
	watched := map[string]bool{}
	for _, identities := range identitiesByHome {
		for identity := range identities {
			watched[identity] = true
		}
	}
	now := time.Now()
	due := map[string]bool{}
	in, unlock = c.lock()
	for identity := range in.homeEditRetryAfter {
		if !watched[identity] || converged[identity] {
			delete(in.homeEditRetryAfter, identity)
		}
	}
	for identity := range stale {
		current := digestByIdentity[identity]
		retry, ok := in.homeEditRetryAfter[identity]
		if ok && retry.after.After(now) && (retry.armed == nil || *retry.armed == current) {
			continue
		}
		due[identity] = true
	}
	unlock()
	if len(due) > 0 {
		// No window is armed here: the refreshing map already keeps the
		// launched probes from doubling, and a failure re-arms with the
		// digest it was correcting. Arming a successful launch would let the
		// window suppress a lagging sibling that never failed.
		c.refresh(func(key cacheKey, _ *core.ProfileConfig) bool { return due[key.identity] })
	}
	in, unlock = c.lock()
	defer unlock()
	for home, files := range updates {
		in.homeStats[home] = files
	}
	// Homes no profile uses anymore stop being watched, and identities no
	// definition serves stop being settled or throttled.
	for home := range in.homeStats {
		if _, ok := identitiesByHome[home]; !ok {
			delete(in.homeStats, home)
		}
	}
	for key := range in.homeSettled {
		if !watched[key.identity] {
			delete(in.homeSettled, key)
		}
	}
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
	type launch struct {
		key          cacheKey
		attempt      *probeAttempt
		cancellation context.Context
	}
	var started []launch
	in, unlock := c.lock()
	keys := []cacheKey{}
	for _, key := range sortedCacheKeys(in.entries) {
		e := in.entries[key]
		if _, refreshing := in.refreshing[key]; e.kind == entryReady && expired(key, &e.choices) && !refreshing {
			keys = append(keys, key)
		}
	}
	for _, key := range keys {
		def, ok := in.definitions[key.identity]
		if !ok || def.cancellation.Err() != nil {
			continue
		}
		if !includeDisabled && !in.probesAutomatically(&def) {
			continue
		}
		attempt := c.attemptRefresh(&def, key.modelOption())
		in.refreshing[key] = attempt
		started = append(started, launch{key: key, attempt: attempt, cancellation: def.cancellation})
	}
	unlock()
	launched := make([]cacheKey, len(started))
	for index, refresh := range started {
		launched[index] = refresh.key
	}
	for _, refresh := range started {
		go c.driveRefresh(refresh.key, refresh.attempt, refresh.cancellation)
	}
	return launched
}

func (c *ProfileCatalog) driveRefresh(key cacheKey, attempt *probeAttempt, cancellation context.Context) {
	completed := false
	select {
	case <-cancellation.Done():
	case <-attempt.start():
		completed = true
	}
	in, unlock := c.edit()
	if current, ok := in.refreshing[key]; !ok || current != attempt {
		unlock()
		return
	}
	delete(in.refreshing, key)
	if !completed {
		unlock()
		return
	}
	if failure := attempt.err; failure != nil {
		// The re-probe failed: the entry keeps its pre-edit copy and the
		// identity stays stale. Re-arm the retry window so the next pass
		// waits it out instead of re-probing the identity every poll. The
		// window retries one digest — the home state the failed probe read,
		// else the state the entry serves — so a pass whose current digest
		// differs is a fresh edit and bypasses it, even for an entry a
		// cache-served publish never settled. With no digest known at all,
		// the window stays unarmed: any current digest could be a fresh
		// edit's, and the window must not suppress it. The window is cleared
		// by the home watch when it observes every served entry settled,
		// never by a probe.
		armed := failure.homeDigest
		if armed == nil {
			armed = in.settledDigest(key)
		}
		if retry, ok := in.homeEditRetryAfter[key.identity]; ok {
			retry.after = time.Now().Add(c.editRetryWait())
			// Adopt the digest this failure read: the window retries the
			// state the probe just failed, so an unchanged home waits the
			// bound instead of re-probing every pass. A newer edit still
			// bypasses, differing from the armed digest.
			if armed != nil {
				retry.armed = armed
			}
			in.homeEditRetryAfter[key.identity] = retry
		} else if armed != nil {
			// Arm only when some digest is known: with none, any current
			// digest could be a fresh edit's, and the window must not
			// suppress it.
			in.homeEditRetryAfter[key.identity] = homeEditRetry{after: time.Now().Add(c.editRetryWait()), armed: armed}
		}
		unlock()
		return
	}
	outcome := attempt.outcome
	previousEntry, ok := in.entries[key]
	if !ok || previousEntry.kind != entryReady {
		unlock()
		return
	}
	previous := previousEntry.choices
	digest := outcome.homeDigest
	if digest != nil {
		in.homeSettled[key] = *digest
	}
	// Discharge the failed-reprobe window only when this success leaves the
	// identity at one digest: a still-failing sibling keeps its older digest
	// and the window's bound, whatever order the probes complete in. The
	// home watch also clears the window for identities it observes
	// converged.
	if in.convergedAtOneDigest(key.identity) {
		delete(in.homeEditRetryAfter, key.identity)
	}
	choices := cloneChoices(outcome.choices)
	in.entries[key] = entry{kind: entryReady, choices: choices}
	var started []startedAttempt
	if !key.hasModel {
		def := in.definitions[key.identity]
		started = c.claimModels(in, &def, key.identity, &choices, digest)
	}
	previous.ObservedAt = choices.ObservedAt
	changed := len(started) > 0 || !profileConfigsEqual(&previous, &choices)
	unlock()
	if changed {
		c.publish()
	}
	for _, claimed := range started {
		c.spawn(claimed.key, claimed.attempt)
	}
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
	id := def.id
	config := def.config
	cancellation := def.cancellation
	var selected *string
	if model != nil {
		value := *model
		selected = &value
	}
	var probe func() (Discovery, error)
	if c.probe != nil {
		// As in Rust, the probe is called now and its work runs when the
		// attempt first runs.
		var run ProbeRun
		if selected == nil {
			run = c.probe.probe(id)
		} else {
			run = c.probe.modelProbe(id, *selected)
		}
		probe = func() (Discovery, error) {
			choices, err := run(cancellation)
			if err != nil {
				return Discovery{}, err
			}
			return Discovery{Choices: choices, Probed: true}, nil
		}
	} else {
		probe = func() (Discovery, error) {
			return DiscoverFor(context.Background(), *config, id, selected, cancellation, refresh)
		}
	}
	home, hasHome := definitionHome(def)
	return newProbeAttempt(func() (probeOutcome, *attemptError) {
		// Read the catalog inputs before the probe stages them. The home
		// watch later compares what a probe read, not when it finished, so
		// an edit that lands mid-probe is re-detected, and a failed probe
		// records nothing and stays pending.
		var digestAtStart *[32]byte
		if hasHome {
			if inputs, ok := hashHomeInputs(home); ok {
				digest := homeDigest(&inputs)
				digestAtStart = &digest
			}
		}
		discovery, err := probe()
		if err != nil {
			return probeOutcome{}, newAttemptError(err, digestAtStart)
		}
		// A cache-served answer predates the home edit the digest just
		// captured: it settles nothing, so the watch keeps the entry pending
		// for a real re-probe instead of masking the edit.
		var settled *[32]byte
		if discovery.Probed {
			settled = digestAtStart
		}
		return probeOutcome{choices: discovery.Choices, homeDigest: settled}, nil
	})
}

func (c *ProfileCatalog) spawn(key cacheKey, attempt *probeAttempt) {
	go c.drive(key, attempt)
}

// drive supervises the entry at key while attempt owns it: it publishes the
// attempt's result and retries a retryable failure with backoff until the
// entry is replaced or its definition retired.
func (c *ProfileCatalog) drive(key cacheKey, attempt *probeAttempt) {
	in, unlock := c.lock()
	if e, ok := in.entries[key]; !ok || !e.owns(attempt) {
		unlock()
		return
	}
	cancellation := in.definitions[key.identity].cancellation
	unlock()
	failures := 0
	for {
		select {
		case <-cancellation.Done():
			return
		case <-attempt.start():
		}
		outcome, failure := attempt.outcome, attempt.err
		var started []startedAttempt
		in, unlock := c.edit()
		if e, ok := in.entries[key]; !ok || !e.owns(attempt) {
			unlock()
			return
		}
		if failure == nil {
			if outcome.homeDigest != nil {
				in.homeSettled[key] = *outcome.homeDigest
			}
			// See driveRefresh: discharge the window only at one digest, so
			// a failing sibling keeps the bound.
			if in.convergedAtOneDigest(key.identity) {
				delete(in.homeEditRetryAfter, key.identity)
			}
			choices := cloneChoices(outcome.choices)
			in.entries[key] = entry{kind: entryReady, choices: choices}
			if !key.hasModel {
				def := in.definitions[key.identity]
				started = append(started, c.claimModels(in, &def, key.identity, &choices, outcome.homeDigest)...)
			}
		} else {
			in.entries[key] = entry{kind: entryFailed, attempt: attempt, err: failure.message}
			profile := in.definitions[key.identity].id
			if failure.retryable {
				slog.Warn("profile capability hydration failed; retrying with backoff", "profile", profile, "model", debugModel(key), "error", failure.message)
			} else {
				slog.Info("profile capabilities stay unknown", "profile", profile, "model", debugModel(key), "error", failure.message)
			}
		}
		unlock()
		c.publish()
		for _, claimed := range started {
			c.spawn(claimed.key, claimed.attempt)
		}
		// A failure no retry can fix stays the entry's state until a changed
		// definition replaces it.
		if failure == nil || !failure.retryable {
			return
		}
		delay := retryDelays[min(failures, len(retryDelays)-1)]
		failures++
		timer := time.NewTimer(delay)
		select {
		case <-cancellation.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
		in, unlock = c.edit()
		if e, ok := in.entries[key]; !ok || !e.owns(attempt) {
			unlock()
			return
		}
		// A disabled profile is probed only on request: the failure stays,
		// and the next request that names the profile starts one fresh
		// attempt (RereadFailed).
		def := in.definitions[key.identity]
		if !in.probesAutomatically(&def) {
			unlock()
			return
		}
		attempt = c.attempt(&def, key.modelOption())
		in.entries[key] = entry{kind: entryPending, attempt: attempt}
		unlock()
		c.publish()
	}
}

// fetch is what id offers under its `models` allow-list. Every reader of a
// profile's choices comes through here or through Snapshot.
func (c *ProfileCatalog) fetch(ctx context.Context, config *core.Config, id string, model *string) (core.ProfileConfig, error) {
	profile, err := config.NamedProfile(id)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	identity := profile.CapabilitiesKey(id)
	// Model-specific probes select that model to discover its efforts. Only
	// the default probe can grant the default-model exemption.
	specific := model != nil
	choices, err := c.fetchUnrestricted(ctx, config, id, model)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	allowed := c.currentModelOffer(identity)
	if allowed.IsUnrestricted() {
		return choices, nil
	}
	selected := choices.Model
	// A default read already carries the default model. A model-specific
	// read uses the default entry it already depended on. Reading it must
	// not start another live probe and replace concurrent readers.
	var defaultModel *string
	if specific {
		in, unlock := c.lock()
		cached, ok := in.entries[newCacheKey(identity, nil)]
		unlock()
		switch {
		case !ok:
			fetched, err := c.fetchUnrestricted(ctx, config, id, nil)
			if err != nil {
				return core.ProfileConfig{}, err
			}
			defaultModel = fetched.Model
		case cached.kind == entryReady:
			defaultModel = cached.choices.Model
		case cached.kind == entryPending:
			outcome, failure, ok := cached.attempt.wait(ctx)
			if !ok {
				return core.ProfileConfig{}, ctx.Err()
			}
			if failure != nil {
				return core.ProfileConfig{}, errors.New(failure.message)
			}
			defaultModel = outcome.choices.Model
		default:
			return core.ProfileConfig{}, errors.New(cached.err)
		}
	} else {
		defaultModel = selected
	}
	choices.Model = defaultModel
	// The list may have changed during the probes; apply the current one.
	allowed = c.currentModelOffer(identity)
	choices = choices.RestrictedTo(&allowed)
	choices.Model = selected
	return choices, nil
}

// currentModelOffer is the model offer of identity under the catalog's
// lock.
func (c *ProfileCatalog) currentModelOffer(identity string) core.ModelOffer {
	in, unlock := c.lock()
	defer unlock()
	return in.modelOffer(identity)
}

// fetchUnrestricted is what id's harness advertises, before the allow-list.
// Only `mj doctor` asks for this, to compare the allow-list with the
// catalogue.
func (c *ProfileCatalog) fetchUnrestricted(ctx context.Context, config *core.Config, id string, model *string) (core.ProfileConfig, error) {
	profile, err := config.NamedProfile(id)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	live := profile.Kind == core.HarnessKindMuse
	// A live read has its own fresh-probe admission in fetchLive.
	// Re-ensuring an adopted definition would retire that owner and discard
	// the default entry that model-specific reads depend on.
	identity := profile.CapabilitiesKey(id)
	// A retired owner (its profile changed, or was enabled or disabled) is
	// no owner: claim a fresh one.
	in, unlock := c.lock()
	def, owned := in.definitions[identity]
	owned = owned && def.cancellation.Err() == nil
	unlock()
	if !live || !owned {
		c.ensureProfiles(config, true, &id)
	}
	key := newCacheKey(identity, model)
	in, unlock = c.lock()
	def, ok := in.definitions[key.identity]
	unlock()
	if !ok {
		return core.ProfileConfig{}, errors.New("profile definition retired")
	}
	revisions := c.subscribe()
	defer revisions.close()
	if live {
		return c.fetchLive(ctx, key, key.modelOption(), def.cancellation, revisions)
	}
	return c.fetchSettled(ctx, config, id, key.modelOption())
}

// fetchSettled: admission checks configured choices against the warm owner,
// including Muse.
func (c *ProfileCatalog) fetchSettled(ctx context.Context, config *core.Config, id string, model *string) (core.ProfileConfig, error) {
	profile, err := config.NamedProfile(id)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	identity := profile.CapabilitiesKey(id)
	// Claim and reuse under the same lock, so concurrent first readers
	// cannot replace each other's Muse owner. Only warm passes refresh it.
	c.ensureProfiles(config, false, &id)
	if !profile.Enabled {
		// Background work never probes a disabled profile, so the request
		// that names one retries its failures and re-probes its expired
		// entries. Readers keep the old answer until a probe succeeds.
		c.RereadFailed(id)
		c.refreshWhere(func(key cacheKey, choices *core.ProfileConfig) bool {
			return key.identity == identity && !IsCurrent(choices)
		}, true)
	}
	key := newCacheKey(identity, model)
	revisions := c.subscribe()
	defer revisions.close()
	for {
		// Subscribe before inspecting ownership, so completion cannot be
		// missed. Readers never drive or publish the supervisor's attempt.
		in, unlock := c.lock()
		if c.cancellation.Err() != nil {
			unlock()
			return core.ProfileConfig{}, errors.New("profile discovery cancelled by daemon shutdown")
		}
		def, ok := in.definitions[key.identity]
		if !ok || def.cancellation.Err() != nil {
			unlock()
			return core.ProfileConfig{}, errors.New("profile definition retired")
		}
		cancellation := def.cancellation
		defaultEntry, ok := in.entries[newCacheKey(key.identity, nil)]
		if !ok {
			unlock()
			return core.ProfileConfig{}, errors.New("default claimed by ensure")
		}
		var current entry
		var started *probeAttempt
		if existing, ok := in.entries[key]; key.hasModel && defaultEntry.kind != entryReady {
			current = defaultEntry
		} else if ok {
			current = existing
		} else {
			attempt := c.attempt(&def, key.modelOption())
			current = entry{kind: entryPending, attempt: attempt}
			in.publication = nil
			in.entries[key] = current
			started = attempt
		}
		unlock()
		if started != nil {
			c.spawn(key, started)
			c.publish()
		}
		switch current.kind {
		case entryReady:
			return cloneChoices(current.choices), nil
		case entryFailed:
			return core.ProfileConfig{}, errors.New(current.err)
		}
		// Shutdown also cancels this definition; report the owning cause
		// consistently when both tokens are ready.
		select {
		case <-c.cancellation.Done():
			return core.ProfileConfig{}, errors.New("profile discovery cancelled by daemon shutdown")
		case <-cancellation.Done():
			// A warm pass can replace this identity while we wait. Rejoin
			// its current owner under the lock on the next iteration.
		case <-revisions.changed:
			revisions.caughtUp()
		case <-ctx.Done():
			return core.ProfileConfig{}, ctx.Err()
		}
	}
}

// fetchLive reads a Muse profile live instead of serving a retained entry.
// New models and effort tiers appear server-side at any time, so a retained
// answer is never served: every read probes the harness that actually
// runs. An in-flight probe is still joined so concurrent readers share it,
// and failures are served as-is so a broken harness keeps the supervisor's
// backoff instead of being hammered on every read. The supervisor still
// publishes each result, so the snapshot converges.
func (c *ProfileCatalog) fetchLive(ctx context.Context, key cacheKey, model *string, cancellation context.Context, revisions *revisionReceiver) (core.ProfileConfig, error) {
	stopped := func() error {
		// Rust's biased select: shutdown is the owning cause when both
		// tokens are ready.
		if c.cancellation.Err() != nil {
			return errors.New("profile discovery cancelled by daemon shutdown")
		}
		return errors.New("profile definition retired")
	}
	if model != nil {
		// Efforts are discovered against the default catalogue; wait for it
		// the way a shared reader would.
		for {
			in, unlock := c.lock()
			defaultEntry, ok := in.entries[newCacheKey(key.identity, nil)]
			unlock()
			if ok && defaultEntry.kind == entryReady {
				break
			}
			if ok && defaultEntry.kind == entryFailed {
				return core.ProfileConfig{}, errors.New(defaultEntry.err)
			}
			if c.cancellation.Err() != nil || cancellation.Err() != nil {
				return core.ProfileConfig{}, stopped()
			}
			select {
			case <-c.cancellation.Done():
				return core.ProfileConfig{}, stopped()
			case <-cancellation.Done():
				return core.ProfileConfig{}, stopped()
			case <-revisions.changed:
				revisions.caughtUp()
			case <-ctx.Done():
				return core.ProfileConfig{}, ctx.Err()
			}
		}
	}
	in, unlock := c.lock()
	var attempt *probeAttempt
	started := false
	existing, ok := in.entries[key]
	switch {
	case ok && existing.kind == entryPending:
		attempt = existing.attempt
	case ok && existing.kind == entryFailed:
		unlock()
		return core.ProfileConfig{}, errors.New(existing.err)
	default:
		def := in.definitions[key.identity]
		attempt = c.attempt(&def, key.modelOption())
		in.publication = nil
		in.entries[key] = entry{kind: entryPending, attempt: attempt}
		started = true
	}
	unlock()
	if started {
		c.spawn(key, attempt)
		c.publish()
	}
	outcome, failure, ok := attempt.wait(ctx)
	if !ok {
		return core.ProfileConfig{}, ctx.Err()
	}
	if failure != nil {
		return core.ProfileConfig{}, errors.New(failure.message)
	}
	return cloneChoices(outcome.choices), nil
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
// a caller could mutate with the original. core.DeepClone copies every
// exported path; a profile's Environment keeps its tables in unexported
// fields, so it is cloned on its own.
func cloneConfig(config *core.Config) core.Config {
	clone := core.DeepClone(*config)
	for id, profile := range clone.Profiles {
		profile.Environment = config.Profiles[id].Environment.Clone()
		clone.Profiles[id] = profile
	}
	return clone
}

// configsEqual is Config's derived PartialEq: profiles compare as
// HarnessProfile.Equal does, everything else field by field. A present
// empty table and an absent one may compare unequal here, which only costs
// a redundant publication.
func configsEqual(a, b *core.Config) bool {
	if len(a.Profiles) != len(b.Profiles) {
		return false
	}
	for id, profile := range a.Profiles {
		other, ok := b.Profiles[id]
		if !ok || !profile.Equal(other) {
			return false
		}
	}
	left, right := *a, *b
	left.Profiles, right.Profiles = nil, nil
	return reflect.DeepEqual(left, right)
}

// modelOffersEqual is ModelOffer's derived PartialEq.
func modelOffersEqual(a, b core.ModelOffer) bool {
	return slices.Equal(a.Models, b.Models) && slices.Equal(a.ExcludedScopes, b.ExcludedScopes)
}

// modelOffersByProfileEqual compares every profile's model offer in a and b
// (Rust: the two `BTreeMap<String, ModelOffer>`).
func modelOffersByProfileEqual(a, b *core.Config) bool {
	if len(a.Profiles) != len(b.Profiles) {
		return false
	}
	for id, profile := range a.Profiles {
		other, ok := b.Profiles[id]
		if !ok || !modelOffersEqual(profile.ModelOffer(), other.ModelOffer()) {
			return false
		}
	}
	return true
}

// profileConfigsEqual is ProfileConfig's derived PartialEq.
func profileConfigsEqual(a, b *core.ProfileConfig) bool {
	if (a.Model == nil) != (b.Model == nil) || (a.Model != nil && *a.Model != *b.Model) {
		return false
	}
	return a.ObservedAt == b.ObservedAt &&
		slices.EqualFunc(a.Models, b.Models, sessionConfigChoicesEqual) &&
		slices.EqualFunc(a.Efforts, b.Efforts, sessionConfigChoicesEqual)
}

// sessionConfigChoicesEqual is SessionConfigChoice's derived PartialEq.
func sessionConfigChoicesEqual(a, b core.SessionConfigChoice) bool {
	if a.Value != b.Value || a.Name != b.Name {
		return false
	}
	if a.Description == nil || b.Description == nil {
		return a.Description == b.Description
	}
	return *a.Description == *b.Description
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
	in, unlock := c.lock()
	defer unlock()
	if in.live == nil {
		return nil, false
	}
	profile, err := in.live.NamedProfile(id)
	if err != nil {
		return nil, false
	}
	if profile.Kind == core.HarnessKindMuse {
		return nil, false
	}
	identity := profile.CapabilitiesKey(id)
	// The default entry and the default model's own entry refresh
	// independently; the newer observation answers. Either entry counts only
	// when it reports the requested model: a model-specific probe can answer
	// with the harness's default instead.
	var newest *core.ProfileConfig
	for _, key := range []cacheKey{newCacheKey(identity, nil), newCacheKey(identity, &model)} {
		e, ok := in.entries[key]
		if !ok || e.kind != entryReady || e.choices.Model == nil || *e.choices.Model != model {
			continue
		}
		// Rust's max_by_key keeps the last of equal maxima.
		if newest == nil || e.choices.ObservedAt >= newest.ObservedAt {
			choices := e.choices
			newest = &choices
		}
	}
	if newest == nil {
		return nil, false
	}
	return nonNilChoices(slices.Clone(newest.Efforts)), true
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
	config, err := c.Live()
	if err != nil {
		return nil, err
	}
	snapshot := c.Snapshot()
	published := snapshot.Get()
	identity := published.ModelID(&config, model)
	candidates := []ProfileCandidate{}
	for id, profile := range config.EnabledProfiles() {
		capabilities, ok := published.Profiles[profile.CapabilitiesKey(id)]
		if !ok || capabilities.Choices.Kind != core.CapabilityStateReady {
			continue
		}
		if model != "" {
			if _, offered := core.Offer(capabilities.Choices.Value.Models, identity, model); !offered {
				continue
			}
		}
		candidates = append(candidates, ProfileCandidate{ID: id, Kind: profile.Kind})
	}
	return candidates, nil
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
	for {
		config, err := c.Live()
		if err != nil {
			return core.ProfileConfig{}, err
		}
		profile, err := config.NamedProfile(id)
		if err != nil {
			return core.ProfileConfig{}, err
		}
		identity := profile.CapabilitiesKey(id)
		var choices core.ProfileConfig
		if unrestricted {
			choices, err = c.fetchUnrestricted(ctx, &config, id, model)
		} else {
			choices, err = c.fetch(ctx, &config, id, model)
		}
		current, liveErr := c.Live()
		if liveErr != nil {
			return core.ProfileConfig{}, liveErr
		}
		if profile, nameErr := current.NamedProfile(id); nameErr == nil && profile.CapabilitiesKey(id) == identity {
			return choices, err
		}
		// Only this profile's configured definition changed. Follow its new
		// owner; unrelated profile updates cannot retire this read.
		if ctxErr := ctx.Err(); ctxErr != nil {
			return core.ProfileConfig{}, ctxErr
		}
	}
}

// Capabilities reads every profile in profiles concurrently, in order; the
// first failure in that order is the error.
func (c *ProfileCatalog) Capabilities(ctx context.Context, profiles []string) ([]core.ProfileConfig, error) {
	if _, err := c.Live(); err != nil {
		return nil, err
	}
	type read struct {
		choices core.ProfileConfig
		err     error
	}
	reads := make([]read, len(profiles))
	var waiting sync.WaitGroup
	for index, id := range profiles {
		waiting.Go(func() {
			choices, err := c.liveCapabilities(ctx, id, nil)
			reads[index] = read{choices: choices, err: err}
		})
	}
	waiting.Wait()
	capabilities := make([]core.ProfileConfig, 0, len(profiles))
	for _, read := range reads {
		if read.err != nil {
			return nil, read.err
		}
		capabilities = append(capabilities, read.choices)
	}
	return capabilities, nil
}

// AdmissionOptions is the sub-agent options for each of models (a set),
// read from the warm owners. On failure the error is that of the first
// failing model in sorted order.
func (c *ProfileCatalog) AdmissionOptions(ctx context.Context, config *core.Config, parent string, models map[string]bool) (map[string]core.SubagentOptions, error) {
	if _, err := config.NamedProfile(parent); err != nil {
		return nil, err
	}
	type read struct {
		choices core.ProfileConfig
		err     error
	}
	ids := []string{}
	for id := range config.EnabledProfiles() {
		ids = append(ids, id)
	}
	reads := make([]read, len(ids))
	var waiting sync.WaitGroup
	for index, id := range ids {
		waiting.Go(func() {
			choices, err := c.fetchSettled(ctx, config, id, nil)
			reads[index] = read{choices: choices, err: err}
		})
	}
	waiting.Wait()
	defaults := make(map[string]read, len(ids))
	for index, id := range ids {
		defaults[id] = reads[index]
	}
	sorted := slices.Sorted(maps.Keys(models))
	type admitted struct {
		options core.SubagentOptions
		err     error
	}
	results := make([]admitted, len(sorted))
	for index, model := range sorted {
		waiting.Go(func() {
			options, err := SubagentOptionsWith(ctx, config, parent, &model,
				func(ctx context.Context, id string, selected *string) (core.ProfileConfig, error) {
					if selected == nil {
						result := defaults[id]
						if result.err != nil {
							return core.ProfileConfig{}, errors.New(result.err.Error())
						}
						return cloneChoices(result.choices), nil
					}
					return c.fetchSettled(ctx, config, id, selected)
				})
			results[index] = admitted{options: options, err: err}
		})
	}
	waiting.Wait()
	options := make(map[string]core.SubagentOptions, len(sorted))
	for index, model := range sorted {
		if results[index].err != nil {
			return nil, results[index].err
		}
		options[model] = results[index].options
	}
	return options, nil
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
	if c.inner.entries == nil {
		c.inner.definitions = map[string]definition{}
		c.inner.entries = map[cacheKey]entry{}
		c.inner.wanted = map[string]map[string]bool{}
		c.inner.refreshing = map[cacheKey]*probeAttempt{}
		c.inner.homeStats = map[string]homeFiles{}
		c.inner.homeSettled = map[cacheKey][32]byte{}
		c.inner.homeEditRetryAfter = map[string]homeEditRetry{}
	}
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
	return BuildProfileCatalog(context.Background(), probe, func(profile string, _ string) ProbeRun {
		return probe(profile)
	})
}

// SyncNow adopts config and waits for every enabled profile's default
// choices and each listed model's efforts (hidden from the docs in Rust).
func (c *ProfileCatalog) SyncNow(ctx context.Context, config *core.Config) {
	c.Sync(config)
	var waiting sync.WaitGroup
	for id := range config.EnabledProfiles() {
		waiting.Go(func() {
			choices, err := c.fetch(ctx, config, id, nil)
			if err != nil {
				return
			}
			for _, model := range choices.Models {
				value := model.Value
				_, _ = c.fetch(ctx, config, id, &value)
			}
		})
	}
	waiting.Wait()
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

// upgradeDefinitionsEqual compares two upgrade views as Rust compares the
// `Vec<UpgradeDefinition>`.
func upgradeDefinitionsEqual(a, b []upgradeDefinition) bool {
	return slices.EqualFunc(a, b, func(a, b upgradeDefinition) bool {
		return a.id == b.id && a.identity == b.identity && modelOffersEqual(a.models, b.models)
	})
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

// statHomeInput is one catalog input's stat; nil when it is absent. Any
// other error is a hard I/O error.
func statHomeInput(path string) (*fileStat, error) {
	info, err := os.Stat(path)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, nil
		}
		return nil, err
	}
	return &fileStat{mtime: info.ModTime().UnixNano(), hasMtime: true, size: uint64(info.Size())}, nil
}

// hashHomeInputs is the catalog-input state of home, read in full.
// pollHomeFiles calls this only for inputs whose stat changed or may have
// been rewritten inside one mtime tick; probes call it at launch to record
// what they staged from. False on a hard I/O error.
func hashHomeInputs(home string) ([3]watchedInput, bool) {
	var inputs [3]watchedInput
	for index, name := range homeCatalogInputs {
		path := filepath.Join(home, name)
		stat, err := statHomeInput(path)
		if err != nil {
			return inputs, false
		}
		inputs[index] = watchedInput{hash: hashHomeInput(path, stat != nil), stat: stat}
	}
	return inputs, true
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
	var inputs [3]watchedInput
	contentChanged := previous == nil
	for index, name := range homeCatalogInputs {
		path := filepath.Join(home, name)
		stat, err := statHomeInput(path)
		if err != nil {
			slog.Debug("home catalog watch skipped this pass", "home", home, "input", name, "error", err.Error())
			return homeFiles{}, false
		}
		var previousInput *watchedInput
		if previous != nil {
			previousInput = &previous.inputs[index]
		}
		recentlyWritten := false
		if stat != nil && stat.hasMtime {
			mtime := time.Unix(0, stat.mtime)
			now := time.Now()
			recentlyWritten = !now.Before(mtime) && now.Sub(mtime) < recentEditWindow
		}
		var input watchedInput
		if !recentlyWritten && previousInput != nil && fileStatsEqual(previousInput.stat, stat) {
			input = *previousInput
		} else {
			input = watchedInput{hash: hashHomeInput(path, stat != nil), stat: stat}
		}
		if previousInput == nil || !hashesEqual(previousInput.hash, input.hash) {
			contentChanged = true
		}
		inputs[index] = input
	}
	digest := [32]byte{}
	if contentChanged {
		digest = homeDigest(&inputs)
	} else {
		digest = previous.digest
	}
	return homeFiles{inputs: inputs, digest: digest}, true
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
// counts its calls, so a test can prove that a warm answer runs none. As in
// Rust, the call is counted when the catalog builds the attempt.
func CountingProbe(calls *atomic.Int64) Probe {
	return func(profile string) ProbeRun {
		calls.Add(1)
		return func(context.Context) (core.ProfileConfig, error) {
			return TestChoices(profile), nil
		}
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
