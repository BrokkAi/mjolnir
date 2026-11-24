// Port of mj-profile-catalog/src/profile_config.rs.

// Automatically populated profile capabilities, cached for the life of the
// daemon.
//
// Name collisions: the `observation_check` function is ObservationCheckFor
// beside the ObservationCheck type; the `CHOICES` static is choiceCache, its
// `choice_key` function newChoiceKey beside the choiceKey type, and its
// `load`/`store` functions loadChoices/storeChoices (a package-level `store`
// would clash with the store package the tests import).
//
// The `#[cfg(test)]` helper `resolve_cached` belongs in this file's tests.

package profilecatalog

import (
	"context"
	"errors"
	"math"
	"slices"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SubagentOptions is the pre-session choice of sub-agent models and
// efforts. Pre-session choices use the same enabled-profile set as
// delegation. Discovery remains supervised and cached; independent profiles
// run concurrently.
func SubagentOptions(ctx context.Context, parent string, model *string) (core.SubagentOptions, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs subagent_options")
}

// DiscoverFor is the catalog's probe. `refresh` probes bypass the
// process-global cache: they exist because something outside the profile
// definition (a home edit the catalog owner detected, an expired entry) may
// have changed what the harness advertises, so a cache row discovered
// earlier must not answer. Reports whether the harness actually ran: a
// served cache row answers from an earlier read of the home, which the
// catalog's settled-digest recording must not treat as current.
//
// cancellation is the definition's token (Rust: a CancellationToken): its
// end retires the probe. ctx only abandons the caller's wait.
func DiscoverFor(ctx context.Context, config core.Config, id string, model *string, cancellation context.Context, refresh bool) (Discovery, error) {
	return serializedUntil(ctx, id, cancellation, func(cancelled *atomic.Bool) (Discovery, error) {
		return discoverWithProvenance(&config, id, model, refresh, cancelled)
	})
}

// ValidateSessionSubagentPolicy validates the session's straightforward
// choice, and its subtle choice when it has one, against enabled profiles.
// Subtle is optional; straightforward is not. A YOLO target reads its own
// set of choices.
func ValidateSessionSubagentPolicy(ctx context.Context, catalog *ProfileCatalog, config *core.Config, profileID string, policy *core.SubagentPolicy, targetID string) error {
	panic("unported: mj-profile-catalog/src/profile_config.rs validate_session_subagent_policy")
}

// SubagentOptionsWith gathers the sub-agent options of every enabled
// profile through probe, which discovers a profile's choices with model
// selected (nil for the profile's default). Rust's generic `F: Fn(String,
// Option<String>) -> Future<Output = Result<ProfileConfig>>`.
func SubagentOptionsWith(ctx context.Context, config *core.Config, parent string, model *string, probe func(ctx context.Context, id string, model *string) (core.ProfileConfig, error)) (core.SubagentOptions, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs subagent_options_with")
}

// probeLock serializes the probes of one profile and lets CancelAll stop
// them.
type probeLock struct {
	// gate is tokio's async Mutex: a one-slot semaphore, so a waiter can give
	// up when cancelled.
	gate chan struct{}
	// cancellation is the token CancelAll cancels.
	cancellation context.Context
	cancel       context.CancelFunc
	// holders counts the requests using this lock (Rust: the Arc's strong
	// count), guarded by probes.mu. The registry drops a lock nobody holds,
	// so the next request gets a fresh, uncancelled one.
	holders int
}

func newProbeLock() *probeLock {
	cancellation, cancel := context.WithCancel(context.Background())
	return &probeLock{gate: make(chan struct{}, 1), cancellation: cancellation, cancel: cancel}
}

// CancelAll cancels every profile probe in flight or queued.
func CancelAll() {
	probes.mu.Lock()
	defer probes.mu.Unlock()
	for _, lock := range probes.locks {
		lock.cancel()
	}
}

// probes is Rust's `PROBES`: the probe lock of every profile some request
// holds (Rust: weak references, dropped with their last holder).
var probes struct {
	mu    sync.Mutex
	locks map[string]*probeLock
}

// Discover runs discovery for profileID. A request owns its background job
// independently of the HTTP connection. Concurrent callers serialize by
// profile and reuse the completed cache.
func Discover(ctx context.Context, profileID string, model *string, refresh bool) (core.ProfileConfig, error) {
	return serialized(ctx, profileID, func(cancelled *atomic.Bool) (core.ProfileConfig, error) {
		return discoverBlocking(profileID, model, refresh, cancelled)
	})
}

func serialized(ctx context.Context, profileID string, job func(cancelled *atomic.Bool) (core.ProfileConfig, error)) (core.ProfileConfig, error) {
	return serializedUntil(ctx, profileID, context.Background(), job)
}

// serializedUntil runs job as profileID's next probe, under a supervisor
// that sets job's cancelled flag when cancellation or CancelAll ends it, or
// when the runtime shuts down, and waits for the job before it releases the
// profile's lock. ctx only abandons the caller's wait; the supervisor and
// its job carry on.
func serializedUntil[T any](ctx context.Context, profileID string, cancellation context.Context, job func(cancelled *atomic.Bool) (T, error)) (T, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs serialized_until")
}

// ObservationCheck is what a session's worker must show before its observed
// choices are cached.
type ObservationCheck int

const (
	// ObservationCheckProbeBuild: its build matches the local probe binary,
	// so it reports what a probe would.
	ObservationCheckProbeBuild ObservationCheck = iota
	// ObservationCheckNoProbe: none. This machine runs no worker, so no
	// probe can ever describe a profile, and a session's worker is the only
	// source there is.
	ObservationCheckNoProbe
)

// ObservationCheckFor is the one decision of whose observations may fill
// the profile cache (Rust: `observation_check`). Where a probe can run, only
// a managed bare worker that matches it may; container and ambient
// installations cannot establish that match. False is Rust's None.
func ObservationCheckFor(target core.TargetLocator) (ObservationCheck, bool) {
	if !core.HostRunsWorkers {
		return ObservationCheckNoProbe, true
	}
	switch target.(type) {
	case core.TargetLocatorLocalBare, core.TargetLocatorSshBare, core.TargetLocatorAwsEc2:
		return ObservationCheckProbeBuild, true
	}
	return 0, false
}

// Observe caches the choices a session's worker reports, when check allows
// it. Returns whether anything was cached.
func Observe(ctx context.Context, profileID string, workerBuild string, state core.SharedSnapshot[core.RelayOperationalState], check ObservationCheck) (bool, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs observe")
}

// fingerprint is a profile's cache identity, which follows configured
// definitions only. Harness-home files and refreshed credentials do not
// invalidate advertised choices.
func fingerprint(profile *core.HarnessProfile) string {
	return profile.CapabilitiesKey("")
}

func discoverBlocking(profileID string, model *string, refresh bool, cancelled *atomic.Bool) (core.ProfileConfig, error) {
	if cancelled.Load() {
		return core.ProfileConfig{}, errors.New("profile discovery cancelled")
	}
	config, err := core.ConfigLoad()
	if err != nil {
		return core.ProfileConfig{}, err
	}
	discovery, err := discoverWithProvenance(&config, profileID, model, refresh, cancelled)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	return discovery.Choices, nil
}

// Discovery is what discovery returned, and whether the harness actually
// ran: a process-cache row younger than the choice lifetime is served
// without launching anything, and its choices may predate the profile
// home's current files. Settled-digest recording and any other answer that
// must describe the home as it is now need Probed.
type Discovery struct {
	Choices core.ProfileConfig
	Probed  bool
}

func discoverWithProvenance(config *core.Config, profileID string, model *string, refresh bool, cancelled *atomic.Bool) (Discovery, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs discover_with_provenance")
}

// ChoicesLifetime is how long discovered choices are served before
// discovery probes again. Providers add and retire models without any
// change Mjolnir can see.
const ChoicesLifetime = 60 * 60 * time.Second

// IsCurrent reports whether choices were observed within ChoicesLifetime.
func IsCurrent(choices *core.ProfileConfig) bool {
	return isCurrentAt(choices, time.Now().Unix())
}

func isCurrentAt(choices *core.ProfileConfig, now int64) bool {
	age := now - choices.ObservedAt
	// Rust's saturating_sub: an overflow clamps instead of wrapping.
	switch {
	case choices.ObservedAt < 0 && age < now:
		age = math.MaxInt64
	case choices.ObservedAt > 0 && age > now:
		age = math.MinInt64
	}
	return age < int64(ChoicesLifetime/time.Second)
}

// resolveCachedDiscovery serves current cached choices, or probes. An
// out-of-date entry is still served when the probe fails, so an offline or
// signed-out provider keeps its last known choices instead of losing them.
// It reports whether the answer came from running the harness (Probed) or
// from a cache row that was read earlier. load answers nil when nothing is
// cached.
func resolveCachedDiscovery(refresh bool, current func(choices *core.ProfileConfig) bool, load func() (*core.ProfileConfig, error), probe func() (core.ProfileConfig, error), save func(choices *core.ProfileConfig) error) (Discovery, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs resolve_cached_discovery")
}

// DiscoveryUnsupported is discovery that no retry can make succeed: this
// machine runs no local worker to probe a profile with. The profile's
// capabilities stay unknown; its sessions still offer the harness's own
// choices.
type DiscoveryUnsupported struct{}

func (DiscoveryUnsupported) Error() string {
	return "this machine runs no local Mjolnir worker, so it cannot discover the profile's models and efforts before a session starts"
}

// DiscoveryIsUnsupported reports whether err is a discovery that no retry
// can make succeed: DiscoveryUnsupported anywhere in its chain.
func DiscoveryIsUnsupported(err error) bool {
	return errors.As(err, new(DiscoveryUnsupported))
}

func probeProfile(profileID string, profile *core.HarnessProfile, environment map[string]string, model *string, cancelled *atomic.Bool) (core.ProfileConfig, error) {
	panic("unported: mj-profile-catalog/src/profile_config.rs probe_profile")
}

// choiceKey is Rust's `ChoiceKey`: discovered choices by profile, model
// (empty for the profile's default) and definition fingerprint.
type choiceKey struct {
	profile     string
	model       string
	fingerprint string
}

// choiceCache is Rust's `CHOICES`. Its entries live only as long as the
// daemon, so a restart always discovers again. Readers that iterate it sort
// the keys, as Rust's BTreeMap orders them (profile, model, fingerprint).
var choiceCache struct {
	mu      sync.Mutex
	choices map[choiceKey]core.ProfileConfig
}

func newChoiceKey(profile string, model *string, fingerprint string) choiceKey {
	key := choiceKey{profile: profile, fingerprint: fingerprint}
	if model != nil {
		key.model = *model
	}
	return key
}

// AdvertisedModels is each profile's most recently observed model list.
func AdvertisedModels() map[string][]core.SessionConfigChoice {
	panic("unported: mj-profile-catalog/src/profile_config.rs advertised_models")
}

// loadChoices is Rust's `load`; false is None.
func loadChoices(profile string, model *string, fingerprint string) (core.ProfileConfig, bool) {
	choiceCache.mu.Lock()
	defer choiceCache.mu.Unlock()
	choices, ok := choiceCache.choices[newChoiceKey(profile, model, fingerprint)]
	return cloneChoices(choices), ok
}

// storeChoices is Rust's `store`.
func storeChoices(profile string, fingerprint string, model *string, choices *core.ProfileConfig) {
	choiceCache.mu.Lock()
	defer choiceCache.mu.Unlock()
	if choiceCache.choices == nil {
		choiceCache.choices = map[choiceKey]core.ProfileConfig{}
	}
	choiceCache.choices[newChoiceKey(profile, model, fingerprint)] = cloneChoices(*choices)
}

// cloneChoices is Rust's `ProfileConfig::clone`: the copy shares no slice
// with the original.
func cloneChoices(choices core.ProfileConfig) core.ProfileConfig {
	choices.Models = slices.Clone(choices.Models)
	choices.Efforts = slices.Clone(choices.Efforts)
	return choices
}
