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
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

// SubagentOptions is the pre-session choice of sub-agent models and
// efforts. Pre-session choices use the same enabled-profile set as
// delegation. Discovery remains supervised and cached; independent profiles
// run concurrently.
func SubagentOptions(ctx context.Context, parent string, model *string) (core.SubagentOptions, error) {
	config, err := core.ConfigLoad()
	if err != nil {
		return core.SubagentOptions{}, err
	}
	return SubagentOptionsWith(ctx, &config, parent, model, func(ctx context.Context, id string, model *string) (core.ProfileConfig, error) {
		return Discover(ctx, id, model, false)
	})
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
	refuse := func(message string) error {
		return core.RefusalUnusable(message).WithCode(core.ChoiceUnavailableCode)
	}
	_ = targetID
	profile, err := config.NamedProfile(profileID)
	if err != nil {
		return err
	}
	kind := profile.Kind
	// None asks for no Mjolnir sub-agents at all, so any harness accepts it;
	// only a Mjolnir policy needs the delegation tools.
	if !kind.SupportsDelegationTools() && policy.UsesMjolnir() {
		return refuse(fmt.Sprintf(
			"%s sessions use their harness's own delegation; Mjolnir sub-agents need a Claude, Codex, DSH, or OpenCode profile",
			kind.DisplayName()))
	}
	if policy.IsNative() && !kind.HasNativeSubagents() {
		return core.RefusalUnusable(fmt.Sprintf(
			"%s has no sub-agents of its own, so Native delegation does nothing for it; choose Mjolnir or None",
			kind.DisplayName()))
	}
	if !policy.UsesMjolnir() {
		return nil
	}
	if len(config.Subagents.EffectiveChoices(*policy, core.SubagentTaskTypeStraightforward)) == 0 {
		return refuse("Mjolnir requires [subagents.straightforward] or --subagent-straightforward <MODEL>")
	}
	taskTypes := []core.SubagentTaskType{core.SubagentTaskTypeStraightforward, core.SubagentTaskTypeSubtle}
	models := map[string]bool{}
	for _, taskType := range taskTypes {
		for _, choice := range config.Subagents.EffectiveChoices(*policy, taskType) {
			models[choice.Model] = true
		}
	}
	options, err := catalog.AdmissionOptions(ctx, config, profileID, models)
	if err != nil {
		return err
	}
	for _, taskType := range taskTypes {
		choices := config.Subagents.EffectiveChoices(*policy, taskType)
		if len(choices) == 0 {
			continue
		}
		section := "subagents." + taskType.ID()
		for _, choice := range choices {
			offered := options[choice.Model]
			if err := offered.Validate(&choice); err != nil {
				return refuse(fmt.Sprintf("[%s]: %s", section, err.Error()))
			}
		}
	}
	return nil
}

// SubagentOptionsWith gathers the sub-agent options of every enabled
// profile through probe, which discovers a profile's choices with model
// selected (nil for the profile's default). Rust's generic `F: Fn(String,
// Option<String>) -> Future<Output = Result<ProfileConfig>>`.
func SubagentOptionsWith(ctx context.Context, config *core.Config, parent string, model *string, probe func(ctx context.Context, id string, model *string) (core.ProfileConfig, error)) (core.SubagentOptions, error) {
	if _, err := config.NamedProfile(parent); err != nil {
		return core.SubagentOptions{}, err
	}
	candidates := []string{}
	for _, id := range slices.Sorted(maps.Keys(config.Profiles)) {
		if config.Profiles[id].Enabled {
			candidates = append(candidates, id)
		}
	}
	options := core.SubagentOptions{
		Models:      []core.SessionConfigChoice{},
		Efforts:     []core.SessionConfigChoice{},
		Unavailable: []string{},
	}
	// Rust's join_all: every profile's default probe runs concurrently, and
	// the results keep the profiles' order.
	type defaultResult struct {
		choices core.ProfileConfig
		err     error
	}
	defaults := make([]defaultResult, len(candidates))
	var waiting sync.WaitGroup
	for index, id := range candidates {
		waiting.Go(func() {
			choices, err := probe(ctx, id, nil)
			if err == nil {
				offer := config.Profiles[id].ModelOffer()
				choices = choices.RestrictedTo(&offer)
			}
			defaults[index] = defaultResult{choices: choices, err: err}
		})
	}
	waiting.Wait()
	models := []core.SessionConfigChoice{}
	for _, result := range defaults {
		if result.err == nil {
			models = append(models, result.choices.Models...)
		}
	}
	var identity *string
	if model != nil {
		resolved := core.ModelID(models, *model)
		identity = &resolved
	}
	type optionResult struct {
		models     []core.SessionConfigChoice
		efforts    []core.SessionConfigChoice
		effortsErr error
		err        error
	}
	results := make([]optionResult, len(candidates))
	for index, id := range candidates {
		waiting.Go(func() {
			result := defaults[index]
			if result.err != nil {
				results[index] = optionResult{err: result.err}
				return
			}
			choices := result.choices
			efforts := []core.SessionConfigChoice{}
			var effortsErr error
			if identity != nil {
				preferred := *identity
				if model != nil {
					preferred = *model
				}
				if offer, ok := core.Offer(choices.Models, *identity, preferred); ok {
					if choices.Model != nil && *choices.Model == offer.Value && len(choices.Efforts) > 0 {
						efforts = choices.Efforts
					} else {
						value := offer.Value
						specific, err := probe(ctx, id, &value)
						efforts, effortsErr = specific.Efforts, err
					}
				}
			}
			// Spawn gets model availability from the default catalogue.
			// A probe after selecting a model describes its efforts only.
			results[index] = optionResult{models: choices.Models, efforts: efforts, effortsErr: effortsErr}
		})
	}
	waiting.Wait()
	for index, result := range results {
		id := candidates[index]
		if result.err != nil {
			options.Unavailable = append(options.Unavailable, fmt.Sprintf("%s: %s", id, result.err.Error()))
			continue
		}
		options.Models = append(options.Models, result.models...)
		if result.effortsErr != nil {
			options.Unavailable = append(options.Unavailable, fmt.Sprintf("%s: %s", id, result.effortsErr.Error()))
			continue
		}
		for _, choice := range result.efforts {
			if !slices.ContainsFunc(options.Efforts, func(existing core.SessionConfigChoice) bool {
				return existing.Value == choice.Value
			}) {
				options.Efforts = append(options.Efforts, choice)
			}
		}
	}
	options.Models = core.MergedChoices(options.Models)
	return options, nil
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
	probes.mu.Lock()
	if probes.locks == nil {
		probes.locks = map[string]*probeLock{}
	}
	for id, lock := range probes.locks {
		if lock.holders == 0 {
			lock.cancel()
			delete(probes.locks, id)
		}
	}
	lock := probes.locks[profileID]
	if lock == nil {
		lock = newProbeLock()
		probes.locks[profileID] = lock
	}
	lock.holders++
	probes.mu.Unlock()
	type result struct {
		value T
		err   error
	}
	done := make(chan result, 1)
	go func() {
		value, err := superviseProbe(lock, cancellation, job)
		probes.mu.Lock()
		lock.holders--
		probes.mu.Unlock()
		done <- result{value: value, err: err}
	}()
	select {
	case result := <-done:
		return result.value, result.err
	case <-ctx.Done():
		var zero T
		return zero, ctx.Err()
	}
}

// superviseProbe is serializedUntil's supervisor: it takes the profile's
// gate, runs job on its own goroutine, and sets job's cancelled flag when
// cancellation or CancelAll ends it, waiting for job before it releases the
// gate.
func superviseProbe[T any](lock *probeLock, cancellation context.Context, job func(cancelled *atomic.Bool) (T, error)) (T, error) {
	var zero T
	// Rust's biased select: the definition's retirement, then CancelAll,
	// then the gate.
	if cancellation.Err() != nil {
		return zero, errors.New("profile definition retired")
	}
	if lock.cancellation.Err() != nil {
		return zero, errors.New("profile discovery cancelled")
	}
	select {
	case <-cancellation.Done():
		return zero, errors.New("profile definition retired")
	case <-lock.cancellation.Done():
		return zero, errors.New("profile discovery cancelled")
	case lock.gate <- struct{}{}:
	}
	defer func() { <-lock.gate }()
	cancelled := new(atomic.Bool)
	// Rust's CancelOnDrop: the blocking job is told to stop on every exit,
	// before the profile lock is released.
	defer cancelled.Store(true)
	type result struct {
		value T
		err   error
	}
	task := make(chan result, 1)
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				task <- result{err: fmt.Errorf("profile discovery task panicked: %v", recovered)}
			}
		}()
		value, err := job(cancelled)
		task <- result{value: value, err: err}
	}()
	var outcome result
	select {
	case <-cancellation.Done():
		cancelled.Store(true)
		outcome = <-task
	case <-lock.cancellation.Done():
		cancelled.Store(true)
		outcome = <-task
	case outcome = <-task:
	}
	if outcome.err != nil {
		if DiscoveryIsUnsupported(outcome.err) {
			slog.Debug("profile discovery unsupported", "error", outcome.err.Error())
		} else {
			slog.Warn("profile discovery failed", "error", outcome.err.Error())
		}
	}
	return outcome.value, outcome.err
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
	stored := new(atomic.Bool)
	_, err := serialized(ctx, profileID, func(cancelled *atomic.Bool) (core.ProfileConfig, error) {
		config, err := core.ConfigLoad()
		if err != nil {
			return core.ProfileConfig{}, err
		}
		profile, err := config.NamedProfile(profileID)
		if err != nil {
			return core.ProfileConfig{}, err
		}
		operational := state.Get()
		facts := core.AcpSessionFactsFromOperational(profile.Kind, operational.Config, operational.ConfigOptions, operational.Modes)
		var model *string
		if current, ok := facts.CurrentModel(); ok {
			model = &current
		}
		choices := core.ProfileConfig{
			Model:      model,
			Models:     core.SessionConfigChoices(operational.ConfigOptions, "model"),
			Efforts:    core.SessionConfigChoices(operational.ConfigOptions, "effort"),
			ObservedAt: time.Now().Unix(),
		}
		// Worker observations do not carry authentication provenance.
		// Claude's setup-token and login catalogues differ, so only probes
		// may cache it. Muse catalogues are never cached at all: its models
		// and efforts change server-side without a managed-pin bump, so a
		// cached copy hides options people expect.
		if profile.Kind == core.HarnessKindClaude || profile.Kind == core.HarnessKindMuse {
			return choices, nil
		}
		fingerprint := fingerprint(&profile)
		switch check {
		case ObservationCheckProbeBuild:
			executor := core.NewCancellableProcessExecutor(cancelled).WithDeadline(30 * time.Second)
			if _, err := workerstaging.WorkerBinaryFor(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare}, executor); err != nil {
				return core.ProfileConfig{}, err
			}
			if core.WorkerBuildID() != workerBuild {
				return choices, nil
			}
		case ObservationCheckNoProbe:
			// Nothing else will ever fill the profile's default entry, so
			// the first session observed stands in for it.
			if _, ok := loadChoices(profileID, nil, fingerprint); !ok {
				storeChoices(profileID, fingerprint, nil, &choices)
			}
		}
		storeChoices(profileID, fingerprint, choices.Model, &choices)
		stored.Store(true)
		return choices, nil
	})
	if err != nil {
		return false, err
	}
	return stored.Load(), nil
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
	if cancelled.Load() {
		return Discovery{}, errors.New("profile discovery cancelled")
	}
	profile, err := config.NamedProfile(profileID)
	if err != nil {
		return Discovery{}, err
	}
	environment := maps.Clone(profile.Environment.Resolved())
	if environment == nil {
		environment = map[string]string{}
	}
	_ = workerstaging.ApplyClaudeSetupToken(environment, profile.Kind, core.ClaudeOauthTokenPath(profileID))
	// Muse models and efforts change server-side without a managed-pin bump,
	// so a cached catalogue hides options people expect. There is no point
	// caching it: always probe the installation that actually runs, and
	// never write the result back.
	if profile.Kind == core.HarnessKindMuse {
		choices, err := probeProfile(profileID, &profile, environment, model, cancelled)
		if err != nil {
			return Discovery{}, err
		}
		return Discovery{Choices: choices, Probed: true}, nil
	}
	fingerprint := fingerprint(&profile)
	return resolveCachedDiscovery(
		refresh,
		IsCurrent,
		func() (*core.ProfileConfig, error) {
			choices, ok := loadChoices(profileID, model, fingerprint)
			if !ok {
				return nil, nil
			}
			return &choices, nil
		},
		func() (core.ProfileConfig, error) {
			return probeProfile(profileID, &profile, maps.Clone(environment), model, cancelled)
		},
		func(choices *core.ProfileConfig) error {
			if model == nil || (choices.Model != nil && *choices.Model == *model) {
				storeChoices(profileID, fingerprint, model, choices)
			}
			storeChoices(profileID, fingerprint, choices.Model, choices)
			return nil
		},
	)
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
	var cached *core.ProfileConfig
	if !refresh {
		loaded, err := load()
		if err != nil {
			return Discovery{}, err
		}
		cached = loaded
	}
	if cached != nil && current(cached) {
		return Discovery{Choices: cloneChoices(*cached), Probed: false}, nil
	}
	choices, err := probe()
	if err == nil {
		if err := save(&choices); err != nil {
			return Discovery{}, err
		}
		return Discovery{Choices: choices, Probed: true}, nil
	}
	if cached == nil {
		return Discovery{}, err
	}
	if !DiscoveryIsUnsupported(err) {
		slog.Warn("profile discovery failed; serving out-of-date cached choices", "error", err.Error())
	}
	return Discovery{Choices: *cached, Probed: false}, nil
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
	// Checked first, so a profile that is also not signed in is still
	// reported as the failure no retry can fix.
	if !core.HostRunsWorkers {
		return core.ProfileConfig{}, DiscoveryUnsupported{}
	}
	if err := profile.EnsureReady(profileID); err != nil {
		return core.ProfileConfig{}, err
	}
	root, err := os.MkdirTemp("", ".tmp")
	if err != nil {
		return core.ProfileConfig{}, fmt.Errorf("create private profile discovery directory: %w", err)
	}
	// Rust's TempDir drop. The executor below has waited for the worker, and
	// killed its process group when cancelled, before this removes it.
	defer os.RemoveAll(root)
	// The staged home and the probe spec carry the profile's logins and
	// environment.
	if runtime.GOOS != "windows" {
		if err := os.Chmod(root, 0o700); err != nil {
			return core.ProfileConfig{}, err
		}
	}
	// Muse and OpenCode read `$XDG_CONFIG_HOME/<id>`, so their staged home
	// must carry that leaf name or the harness never sees the staged login.
	home := profile.Kind.HomeFromEnvironment(filepath.Join(root, "profile"))
	if err := workerstaging.StageProfileWithCatalog(profileID, profile, home, workerstaging.FetchCatalogOverHTTPS, workerstaging.SharedCatalogCache{}); err != nil {
		return core.ProfileConfig{}, err
	}
	cwd := filepath.Join(root, "workspace")
	if err := os.Mkdir(cwd, 0o777); err != nil {
		return core.ProfileConfig{}, err
	}
	executor := core.NewCancellableProcessExecutor(cancelled).WithDeadline(300 * time.Second)
	worker, err := workerstaging.WorkerBinaryFor(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}, executor)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	// Harnesses leave files in their temporary directory: OpenCode's Bun
	// runtime extracts native libraries there on every start and never
	// removes them. One inside the probe's root goes away with the probe.
	temporary := filepath.Join(root, "tmp")
	if err := os.Mkdir(temporary, 0o777); err != nil {
		return core.ProfileConfig{}, err
	}
	if environment == nil {
		environment = map[string]string{}
	}
	for _, name := range []string{"TMPDIR", "TMP", "TEMP"} {
		environment[name] = temporary
	}
	excludedEnvironment := profile.ExcludeHarnessEnvironment(environment)
	spec := core.ProfileProbeSpec{
		Harness:             profile.Kind,
		ProfileHome:         home,
		Environment:         environment,
		ExcludedEnvironment: excludedEnvironment,
		Cwd:                 cwd,
		Model:               model,
		HarnessRuntime:      core.LocalHarnessRuntime(),
	}
	path := filepath.Join(root, "probe.json")
	body, err := json.Marshal(spec)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	if err := core.AtomicWrite(path, body); err != nil {
		return core.ProfileConfig{}, err
	}
	command := core.NewCommandSpec(worker, []string{"worker", "discover-config", "--spec", path}).
		WithPurpose("discover profile configuration")
	output, err := executor.Execute(&command)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	diagnostics := strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�"))
	if len(output.Stderr) > 0 {
		slog.Info("profile discovery worker diagnostics", "harness", profile.Kind, "diagnostics", diagnostics)
	}
	if output.Status != 0 {
		return core.ProfileConfig{}, fmt.Errorf("profile discovery failed: %s", diagnostics)
	}
	var choices core.ProfileConfig
	if err := json.Unmarshal(output.Stdout, &choices); err != nil {
		return core.ProfileConfig{}, fmt.Errorf("decode discovered configuration: %w", err)
	}
	return choices, nil
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
	choiceCache.mu.Lock()
	defer choiceCache.mu.Unlock()
	keys := slices.SortedFunc(maps.Keys(choiceCache.choices), func(a, b choiceKey) int {
		return cmp.Or(cmp.Compare(a.profile, b.profile), cmp.Compare(a.model, b.model), cmp.Compare(a.fingerprint, b.fingerprint))
	})
	newest := map[string]core.ProfileConfig{}
	for _, key := range keys {
		observed := choiceCache.choices[key]
		entry, ok := newest[key.profile]
		if !ok || observed.ObservedAt > entry.ObservedAt {
			newest[key.profile] = observed
		}
	}
	models := make(map[string][]core.SessionConfigChoice, len(newest))
	for profile, observed := range newest {
		models[profile] = slices.Clone(observed.Models)
	}
	return models
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
