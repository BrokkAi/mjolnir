// Port of the tests in mj-profile-catalog/src/profile_config.rs.

package profilecatalog

import (
	"context"
	"encoding/json"
	"errors"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

// resolveCached serves current cached choices, or probes. An out-of-date
// entry is still served when the probe fails, so an offline or signed-out
// provider keeps its last known choices instead of losing them. Production
// discovery uses resolveCachedDiscovery, which also reports whether the
// harness ran; this wrapper remains for its unit tests.
func resolveCached(refresh bool, current func(choices *core.ProfileConfig) bool, load func() (*core.ProfileConfig, error), probe func() (core.ProfileConfig, error), save func(choices *core.ProfileConfig) error) (core.ProfileConfig, error) {
	discovery, err := resolveCachedDiscovery(refresh, current, load, probe, save)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	return discovery.Choices, nil
}

func choice(value string) core.SessionConfigChoice {
	return core.SessionConfigChoice{Value: value, Name: value}
}

func choiceValues(choices []core.SessionConfigChoice) []string {
	values := make([]string, len(choices))
	for index, choice := range choices {
		values[index] = choice.Value
	}
	return values
}

func refusalCode(t *testing.T, err error) (string, bool) {
	t.Helper()
	refusal, ok := core.RefusalOf(err)
	if !ok {
		t.Fatalf("not a refusal: %v", err)
	}
	return refusal.Code()
}

// Hard-won: #1534 review: a first-listed older alias hid the newer profile during admission.
// Hard-won: a35beec20: Move hid an offered model after probing its effort.
func TestSubagentOptionsPreserveVersionsSharingTheSameLaunchAlias(t *testing.T) {
	config := TestConfig([]ProfileCandidate{
		{ID: "a-stale", Kind: core.HarnessKindClaude},
		{ID: "newer", Kind: core.HarnessKindClaude},
	})
	data, err := os.ReadFile(filepath.Join("testdata", "claude-antigravity-models.json"))
	if err != nil {
		t.Fatal(err)
	}
	var recorded map[string][]core.SessionConfigChoice
	if err := json.Unmarshal(data, &recorded); err != nil {
		t.Fatal(err)
	}
	type call struct {
		id    string
		model *string
	}
	var mu sync.Mutex
	calls := []call{}
	options, err := SubagentOptionsWith(context.Background(), &config, "newer", stringPointer("claude-sonnet-5-5"),
		func(_ context.Context, id string, model *string) (core.ProfileConfig, error) {
			mu.Lock()
			calls = append(calls, call{id: id, model: model})
			mu.Unlock()
			models := slices.Clone(recorded["claude"])
			if id == "a-stale" {
				models[1].Name = "Sonnet 5"
			}
			effort := core.SessionConfigChoice{Value: "high", Name: "High"}
			selected := model != nil
			if model != nil {
				if id != "newer" || *model != "sonnet" {
					t.Errorf("selected-model probe (%s, %s), want (newer, sonnet)", id, *model)
				}
				// A selected-model probe may return just the settings it
				// discovered after selection, without the default list.
				models = []core.SessionConfigChoice{}
			}
			current := model
			if current == nil {
				current = stringPointer("opus")
			}
			efforts := []core.SessionConfigChoice{}
			if selected {
				efforts = []core.SessionConfigChoice{effort}
			}
			return core.ProfileConfig{Model: current, Models: models, Efforts: efforts, ObservedAt: 0}, nil
		})
	if err != nil {
		t.Fatal(err)
	}
	if err := options.Validate(&core.SubagentChoice{Model: "claude-sonnet-5-5", Effort: stringPointer("high")}); err != nil {
		t.Fatal(err)
	}
	if got := core.ModelID(options.Models, "sonnet"); got != "claude-sonnet-5-5" {
		t.Fatalf("model id %q", got)
	}
	selected := 0
	for _, call := range calls {
		if call.model != nil {
			selected++
		}
	}
	if selected != 1 {
		t.Fatalf("%d selected-model probes, want 1", selected)
	}
}

// A profile's `models` list hides a model from delegation as well as from
// pickers: session creation must not accept a sub-agent model that later
// selection cannot find.
func TestSubagentOptionsOfferOnlyModelsTheProfileListAllows(t *testing.T) {
	config := TestConfig([]ProfileCandidate{{ID: "gateway", Kind: core.HarnessKindCodex}})
	gateway := config.Profiles["gateway"]
	gateway.Models = []string{"allowed"}
	config.Profiles["gateway"] = gateway
	probe := func(_ context.Context, _ string, model *string) (core.ProfileConfig, error) {
		current := "default"
		if model != nil {
			current = *model
		}
		return core.ProfileConfig{
			Model:      &current,
			Models:     []core.SessionConfigChoice{choice("default"), choice("allowed"), choice("hidden")},
			Efforts:    []core.SessionConfigChoice{choice("high")},
			ObservedAt: 0,
		}, nil
	}

	hidden, err := SubagentOptionsWith(context.Background(), &config, "gateway", stringPointer("hidden"), probe)
	if err != nil {
		t.Fatal(err)
	}
	if got := choiceValues(hidden.Models); !slices.Equal(got, []string{"default", "allowed"}) {
		t.Fatalf("models %v", got)
	}
	if len(hidden.Efforts) != 0 {
		t.Fatal("a hidden model offers no efforts")
	}

	allowed, err := SubagentOptionsWith(context.Background(), &config, "gateway", stringPointer("allowed"), probe)
	if err != nil {
		t.Fatal(err)
	}
	if got := choiceValues(allowed.Models); !slices.Equal(got, []string{"default", "allowed"}) {
		t.Fatalf("models %v", got)
	}
	if len(allowed.Efforts) != 1 {
		t.Fatalf("efforts %v", allowed.Efforts)
	}
}

func TestLiveAntigravityProfileDiscoveryReadsBaseModelsAndEfforts(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires MJ_ANTIGRAVITY_TEST_HOME with a copied Google login and MJ_WORKER_BINARY pointing to the current mj-worker build")
	}
	home, ok := os.LookupEnv("MJ_ANTIGRAVITY_TEST_HOME")
	if !ok {
		t.Fatal("set copied Google login home")
	}
	profile := core.NewHarnessProfile(core.HarnessKindAntigravity, home)
	choices, err := probeProfile("antigravity-discovery-test", &profile, map[string]string{}, nil, new(atomic.Bool))
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Contains(choiceValues(choices.Models), "gemini-3.8-flash") {
		t.Fatalf("models %v", choiceValues(choices.Models))
	}
	if !slices.Contains(choiceValues(choices.Efforts), "low") {
		t.Fatalf("efforts %v", choiceValues(choices.Efforts))
	}
	if slices.Contains(choiceValues(choices.Models), "gemini-4-carbon-high") {
		t.Fatalf("models %v", choiceValues(choices.Models))
	}
}

// Hard-won: #1284: discovery staged Muse credentials outside its XDG home.
func TestLiveMuseProfileDiscoveryReadsStagedLoginAndCatalog(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires MJ_MUSE_TEST_HOME with a Muse login and MJ_WORKER_BINARY pointing to the current mj-worker build; discovers models without a provider turn")
	}
	account, ok := os.LookupEnv("MJ_MUSE_TEST_HOME")
	if !ok {
		t.Fatal("set authenticated Muse home")
	}
	isolated := t.TempDir()
	home := filepath.Join(isolated, "muse")
	if err := os.Mkdir(home, 0o777); err != nil {
		t.Fatal(err)
	}
	credentials, err := os.ReadFile(filepath.Join(account, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), credentials, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "settings.json"),
		[]byte(`{"schema_version":1,"provider":"meta","model":"muse-spark-1.3"}`), 0o666); err != nil {
		t.Fatal(err)
	}
	// The source home is isolated too: refreshing a staged login must never
	// change the person's own credentials or session data.
	credentials, err = os.ReadFile(filepath.Join(home, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	profile := core.NewHarnessProfile(core.HarnessKindMuse, home)
	profile.LegacySubagents = nil
	choices, err := probeProfile("muse-discovery-1284-test", &profile, map[string]string{}, nil, new(atomic.Bool))
	if err != nil {
		t.Fatal(err)
	}
	if choices.Model == nil || *choices.Model != "muse-spark-1.3" {
		t.Fatalf("model %v", choices.Model)
	}
	if !slices.Contains(choiceValues(choices.Models), "muse-spark-1.3") {
		t.Fatalf("models %v", choiceValues(choices.Models))
	}
	if len(choices.Efforts) == 0 {
		t.Fatal("no efforts")
	}
	after, err := os.ReadFile(filepath.Join(home, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	if string(after) != string(credentials) {
		t.Fatal("the staged login changed the source credentials")
	}
}

// Discovery offers the providers of an OpenCode 2 login stored in its
// database, and of an OpenCode 1 login in `auth.json` that no OpenCode 2 has
// taken over, both through the pinned OpenCode's own import.
func TestLiveOpencodeProfileDiscoveryOffersLoginsFromItsDatabase(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires MJ_OPENCODE_TEST_BINARY naming an OpenCode 2 executable and MJ_WORKER_BINARY pointing to the current mj-worker build; discovers models without a provider turn")
	}
	opencode, ok := os.LookupEnv("MJ_OPENCODE_TEST_BINARY")
	if !ok {
		t.Fatal("set an OpenCode 2 executable")
	}
	isolated := t.TempDir()
	stored := filepath.Join(isolated, "stored", "opencode")
	if err := os.MkdirAll(stored, 0o777); err != nil {
		t.Fatal(err)
	}
	// OpenCode itself writes the login, so the read meets its own schema.
	export := filepath.Join(isolated, "export.json")
	if err := os.WriteFile(export,
		[]byte(`[{"id":"cred_mjolnir_live_test","integrationID":"deepseek","label":"DeepSeek","active":true,"value":{"type":"key","key":"sk-mjolnir-live-test"}}]`), 0o666); err != nil {
		t.Fatal(err)
	}
	// Status only, as Rust's Command::status: the child inherits this
	// process's standard streams, so no pipe is read.
	command := exec.Command(opencode, "auth", "import", "--standalone", export)
	command.Env = append(os.Environ(),
		"XDG_CONFIG_HOME="+filepath.Join(isolated, "stored"),
		"XDG_DATA_HOME="+filepath.Join(stored, ".data"),
		"OPENCODE_DISABLE_AUTOUPDATE=1")
	command.Stdout, command.Stderr = os.Stdout, os.Stderr
	if err := command.Run(); err != nil {
		t.Fatalf("seed the OpenCode login: %v", err)
	}
	legacy := filepath.Join(isolated, "legacy", "opencode")
	if err := os.MkdirAll(filepath.Join(legacy, ".data", "opencode"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(legacy, ".data", "opencode", "auth.json"),
		[]byte(`{"deepseek":{"type":"api","key":"sk-mjolnir-live-legacy"}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	for _, home := range []string{stored, legacy} {
		profile := core.NewHarnessProfile(core.HarnessKindOpenCode, home)
		profile.LegacySubagents = nil
		choices, err := probeProfile("opencode-database-login-test", &profile, map[string]string{}, nil, new(atomic.Bool))
		if err != nil {
			t.Fatal(err)
		}
		if !slices.ContainsFunc(choices.Models, func(choice core.SessionConfigChoice) bool {
			return strings.HasPrefix(choice.Value, "deepseek/")
		}) {
			t.Fatalf("%s: discovered models: %v", home, choiceValues(choices.Models))
		}
	}
}

func TestUnsavedProfileDiscoveryReadsTheOwnedCacheWithoutSavingSettings(t *testing.T) {
	const child = "MJ_TEST_UNSAVED_PROFILE_DISCOVERY"
	if _, ok := os.LookupEnv(child); !ok {
		targets.NewIsolatedTest(targets.TestName("profile_config::tests", "TestUnsavedProfileDiscoveryReadsTheOwnedCacheWithoutSavingSettings")).
			Env(child, "1").
			Env("MJ_INSTANCE", "unsaved-profile-discovery-test").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	ctx := context.Background()
	if err := core.DefaultConfig().Save(); err != nil {
		t.Fatal(err)
	}
	home := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	profile.LegacySubagents = nil
	key := fingerprint(&profile)
	catalog := core.ProfileConfig{
		Model:      stringPointer("chosen"),
		Models:     []core.SessionConfigChoice{choice("chosen")},
		Efforts:    []core.SessionConfigChoice{choice("high")},
		ObservedAt: time.Now().Unix(),
	}
	storeChoices("unsaved", key, nil, &catalog)
	storeChoices("unsaved", key, catalog.Model, &catalog)
	draft := core.DefaultConfig()
	if draft.Profiles == nil {
		draft.Profiles = map[string]core.HarnessProfile{}
	}
	draft.Profiles["unsaved"] = profile
	if draft.Subagents.IsConfigured() {
		t.Fatal("a default configuration names sub-agents")
	}
	draft.Subagents.Straightforward = []core.SubagentChoice{{Model: "chosen", Effort: stringPointer("high")}}
	for _, test := range []struct {
		model     string
		effort    *string
		available bool
	}{
		{"chosen", stringPointer("high"), true},
		{"chosen", nil, true},
		{"missing", nil, false},
		{"chosen", stringPointer("missing"), false},
		{"chosen", stringPointer("adaptive"), false},
	} {
		policy := core.SubagentPolicy{
			Mode:   core.SubagentPolicyMjolnir,
			Subtle: &core.SubagentChoice{Model: test.model, Effort: test.effort},
		}
		err := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &draft, "unsaved", &policy, "localhost")
		if test.available {
			if err != nil {
				t.Fatalf("%s: %v", test.model, err)
			}
			continue
		}
		if err == nil {
			t.Fatalf("%s/%v accepted", test.model, test.effort)
		}
		if code, ok := refusalCode(t, err); !ok || code != core.ChoiceUnavailableCode {
			t.Fatalf("code %q", code)
		}
	}
	if err := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &draft, "unsaved", &core.SubagentPolicy{
		Mode:   core.SubagentPolicyMjolnir,
		Subtle: &core.SubagentChoice{Model: "chosen"},
	}, "localhost"); err != nil {
		t.Fatal(err)
	}
	// Subtle is optional: the straightforward choice alone is enough.
	if err := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &draft, "unsaved", &core.SubagentPolicy{
		Mode: core.SubagentPolicyMjolnir,
	}, "localhost"); err != nil {
		t.Fatal(err)
	}
	draft.Subagents.Straightforward = nil
	missing := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &draft, "unsaved", &core.SubagentPolicy{
		Mode:   core.SubagentPolicyMjolnir,
		Subtle: &core.SubagentChoice{Model: "chosen"},
	}, "localhost")
	if missing == nil {
		t.Fatal("subtle alone was accepted")
	}
	if code, ok := refusalCode(t, missing); !ok || code != core.ChoiceUnavailableCode {
		t.Fatalf("code %q", code)
	}
	if !strings.Contains(missing.Error(), "[subagents.straightforward]") {
		t.Fatalf("subtle alone is never enough: %v", missing)
	}
	options, err := NewProfileCatalog(context.Background()).OptionsFor(ctx, &draft, "unsaved", stringPointer("chosen"))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(options.Models, catalog.Models) {
		t.Fatalf("models %v", options.Models)
	}
	if !reflect.DeepEqual(options.Efforts, catalog.Efforts) {
		t.Fatalf("efforts %v", options.Efforts)
	}
	if len(options.Unavailable) != 0 {
		t.Fatalf("unavailable %v", options.Unavailable)
	}
	loaded, err := core.ConfigLoad()
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := loaded.Profiles["unsaved"]; ok {
		t.Fatal("discovery saved the draft profile")
	}
}

func TestDiscoveryFingerprintIgnoresCreationDefaultsAndUnrelatedProfilePreferences(t *testing.T) {
	profile := core.NewHarnessProfile(core.HarnessKindCodex, t.TempDir())
	original := fingerprint(&profile)
	for range 2 {
		window := 42
		profile.ContextWindowBytes = &window
		profile.GuardianReviewModel = stringPointer("session")
		if fingerprint(&profile) != original {
			t.Fatal("a preference changed the fingerprint")
		}
	}
	profile.Environment = core.EnvironmentFromMap(map[string]string{"PROVIDER": "different"})
	if fingerprint(&profile) == original {
		t.Fatal("the environment did not change the fingerprint")
	}
}

func TestSetupTokenRotationPreservesConfiguredCapabilities(t *testing.T) {
	root := t.TempDir()
	token := filepath.Join(root, "token")
	profile := core.NewHarnessProfile(core.HarnessKindClaude, root)
	resolve := func(environment map[string]string) map[string]string {
		_ = workerstaging.ApplyClaudeSetupToken(environment, profile.Kind, token)
		return environment
	}
	login := fingerprint(&profile)
	if err := core.WriteClaudeOauthToken(token, []byte("setup-first")); err != nil {
		t.Fatal(err)
	}
	first := resolve(map[string]string{})
	if first[core.ClaudeOauthTokenEnv] != "setup-first" {
		t.Fatalf("token %q", first[core.ClaudeOauthTokenEnv])
	}
	firstKey := fingerprint(&profile)
	if login != firstKey {
		t.Fatal("a setup token changed the fingerprint")
	}
	if err := core.WriteClaudeOauthToken(token, []byte("setup-second")); err != nil {
		t.Fatal(err)
	}
	if first[core.ClaudeOauthTokenEnv] != "setup-first" {
		t.Fatal("an in-flight probe retains its authentication snapshot")
	}
	if firstKey != fingerprint(&profile) {
		t.Fatal("a rotated setup token changed the fingerprint")
	}
	explicit := map[string]string{core.ClaudeOauthTokenEnv: "explicit"}
	if got := resolve(maps.Clone(explicit)); !maps.Equal(got, explicit) {
		t.Fatalf("explicit environment became %v", got)
	}
	if strings.Contains(firstKey, "setup-first") {
		t.Fatal("the fingerprint carries the token")
	}
}

func TestRetiringADefinitionCancelsProbesAndPreservesItsReplacement(t *testing.T) {
	cancellation, cancel := context.WithCancel(context.Background())
	defer cancel()
	running := make(chan struct{}, 1)
	first := make(chan error, 1)
	go func() {
		_, err := serializedUntil(context.Background(), "retirement-test", cancellation, func(cancelled *atomic.Bool) (struct{}, error) {
			running <- struct{}{}
			for !cancelled.Load() {
				time.Sleep(time.Millisecond)
			}
			return struct{}{}, errors.New("running probe cancelled")
		})
		first <- err
	}()
	<-running
	var queuedRan atomic.Bool
	queued := make(chan error, 1)
	go func() {
		_, err := serializedUntil(context.Background(), "retirement-test", cancellation, func(*atomic.Bool) (core.ProfileConfig, error) {
			queuedRan.Store(true)
			return core.ProfileConfig{}, nil
		})
		queued <- err
	}()
	runtime.Gosched()
	select {
	case <-queued:
		t.Fatal("queued probe passed the running owner")
	default:
	}
	cancel()
	select {
	case err := <-queued:
		if err == nil {
			t.Fatal("the queued probe ran after its definition retired")
		}
	case <-time.After(time.Second):
		t.Fatal("the queued probe outlived its definition")
	}
	if queuedRan.Load() {
		t.Fatal("the queued probe's job ran")
	}
	select {
	case err := <-first:
		if err == nil {
			t.Fatal("the running probe succeeded after cancellation")
		}
	case <-time.After(time.Second):
		t.Fatal("the running probe outlived its definition")
	}
	replacement := make(chan error, 1)
	go func() {
		_, err := serializedUntil(context.Background(), "retirement-test", context.Background(), func(*atomic.Bool) (core.ProfileConfig, error) {
			return core.ProfileConfig{}, nil
		})
		replacement <- err
	}()
	select {
	case err := <-replacement:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("the replacement waited for the retired definition")
	}
}

func TestRuntimeShutdownCancelsAProbesBlockingJobEvenIfItsSupervisorIsDropped(t *testing.T) {
	t.Skip("Go has no runtime whose shutdown drops a running supervisor goroutine; superviseProbe still sets the job's cancelled flag on every exit, as Rust's CancelOnDrop does")
}

func TestConcurrentColdLookupsShareTheFirstProbe(t *testing.T) {
	var mu sync.Mutex
	var cache *core.ProfileConfig
	var calls atomic.Int64
	var tasks sync.WaitGroup
	errs := make(chan error, 8)
	for range 8 {
		tasks.Go(func() {
			_, err := serialized(context.Background(), "concurrent-cache-test", func(*atomic.Bool) (core.ProfileConfig, error) {
				return resolveCached(
					false,
					func(*core.ProfileConfig) bool { return true },
					func() (*core.ProfileConfig, error) {
						mu.Lock()
						defer mu.Unlock()
						if cache == nil {
							return nil, nil
						}
						cached := *cache
						return &cached, nil
					},
					func() (core.ProfileConfig, error) {
						calls.Add(1)
						time.Sleep(20 * time.Millisecond)
						return core.ProfileConfig{Models: []core.SessionConfigChoice{}, Efforts: []core.SessionConfigChoice{}, ObservedAt: 1}, nil
					},
					func(value *core.ProfileConfig) error {
						mu.Lock()
						defer mu.Unlock()
						saved := *value
						cache = &saved
						return nil
					},
				)
			})
			errs <- err
		})
	}
	tasks.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatal(err)
		}
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("%d probes, want 1", got)
	}
}

func TestEmptyCacheDiscoversAutomaticallyAndFailedProbesRemainRetryable(t *testing.T) {
	var cache *core.ProfileConfig
	calls := 0
	choices := core.ProfileConfig{
		Model:      stringPointer("full/model"),
		Models:     []core.SessionConfigChoice{},
		Efforts:    []core.SessionConfigChoice{},
		ObservedAt: 1,
	}
	load := func() (*core.ProfileConfig, error) {
		if cache == nil {
			return nil, nil
		}
		cached := *cache
		return &cached, nil
	}
	lookup := func(fail bool) (core.ProfileConfig, error) {
		return resolveCached(
			false,
			func(*core.ProfileConfig) bool { return true },
			load,
			func() (core.ProfileConfig, error) {
				calls++
				if fail {
					return core.ProfileConfig{}, errors.New("probe failed")
				}
				return choices, nil
			},
			func(value *core.ProfileConfig) error {
				saved := *value
				cache = &saved
				return nil
			},
		)
	}
	if _, err := lookup(true); err == nil {
		t.Fatal("a failed cold probe succeeded")
	}
	if cache != nil {
		t.Fatal("a failed probe filled the cache")
	}
	if got, err := lookup(false); err != nil || !reflect.DeepEqual(got, choices) {
		t.Fatalf("lookup %v, %v", got, err)
	}
	if got, err := lookup(true); err != nil || !reflect.DeepEqual(got, choices) {
		t.Fatalf("a warm cache must not run the failing probe: %v, %v", got, err)
	}
	if calls != 2 {
		t.Fatalf("%d probes, want 2", calls)
	}
	refreshed, err := resolveCached(
		true,
		func(*core.ProfileConfig) bool { return true },
		load,
		func() (core.ProfileConfig, error) {
			fresh := choices
			fresh.ObservedAt = 2
			return fresh, nil
		},
		func(*core.ProfileConfig) error { return nil },
	)
	if err != nil || refreshed.ObservedAt != 2 {
		t.Fatalf("refresh %v, %v", refreshed, err)
	}
}

func TestChoicesExpireAfterAnHour(t *testing.T) {
	lifetime := int64(ChoicesLifetime / time.Second)
	at := func(observedAt int64) *core.ProfileConfig {
		return &core.ProfileConfig{Models: []core.SessionConfigChoice{}, Efforts: []core.SessionConfigChoice{}, ObservedAt: observedAt}
	}
	if !isCurrentAt(at(1_000), 1_000+lifetime-1) {
		t.Fatal("choices expired early")
	}
	if isCurrentAt(at(1_000), 1_000+lifetime) {
		t.Fatal("choices outlived their lifetime")
	}
}

func TestOutOfDateChoicesAreProbedAgainAndKeptWhenTheProbeFails(t *testing.T) {
	stale := core.ProfileConfig{
		Model:      stringPointer("old"),
		Models:     []core.SessionConfigChoice{},
		Efforts:    []core.SessionConfigChoice{},
		ObservedAt: 1,
	}
	fresh := stale
	fresh.Model = stringPointer("new")
	fresh.ObservedAt = 2
	var saved *core.ProfileConfig
	resolved, err := resolveCached(
		false,
		func(choices *core.ProfileConfig) bool { return choices.ObservedAt > 1 },
		func() (*core.ProfileConfig, error) { cached := stale; return &cached, nil },
		func() (core.ProfileConfig, error) { return fresh, nil },
		func(choices *core.ProfileConfig) error { copied := *choices; saved = &copied; return nil },
	)
	if err != nil || !reflect.DeepEqual(resolved, fresh) {
		t.Fatalf("resolved %v, %v", resolved, err)
	}
	if saved == nil || !reflect.DeepEqual(*saved, fresh) {
		t.Fatalf("saved %v", saved)
	}
	resolved, err = resolveCached(
		false,
		func(*core.ProfileConfig) bool { return false },
		func() (*core.ProfileConfig, error) { cached := stale; return &cached, nil },
		func() (core.ProfileConfig, error) { return core.ProfileConfig{}, errors.New("provider offline") },
		func(*core.ProfileConfig) error { panic("a failed probe saves nothing") },
	)
	if err != nil || !reflect.DeepEqual(resolved, stale) {
		t.Fatalf("resolved %v, %v", resolved, err)
	}
}

// Hard-won: #1337 review: a cache-served retry recorded the home's
// current digest as settled, masking an edit for up to the choice
// lifetime. Only an answer the harness actually read may settle.
func TestResolveCachedDiscoveryReportsWhetherTheHarnessRan(t *testing.T) {
	cached := core.ProfileConfig{
		Model:      stringPointer("cached"),
		Models:     []core.SessionConfigChoice{},
		Efforts:    []core.SessionConfigChoice{},
		ObservedAt: 1,
	}
	probed := cached
	probed.Model = stringPointer("probed")
	probed.ObservedAt = 2
	loadCached := func() (*core.ProfileConfig, error) { copied := cached; return &copied, nil }

	// A current cache row is served without probing.
	served, err := resolveCachedDiscovery(
		false,
		func(*core.ProfileConfig) bool { return true },
		loadCached,
		func() (core.ProfileConfig, error) { panic("a current cache row must not probe") },
		func(*core.ProfileConfig) error { panic("a cache hit saves nothing") },
	)
	if err != nil || !reflect.DeepEqual(served.Choices, cached) {
		t.Fatalf("served %v, %v", served.Choices, err)
	}
	if served.Probed {
		t.Fatal("a cache row answers from an earlier read of the home")
	}

	// A refresh bypasses the cache and reports the probe ran.
	var saved *core.ProfileConfig
	served, err = resolveCachedDiscovery(
		true,
		func(*core.ProfileConfig) bool { return true },
		loadCached,
		func() (core.ProfileConfig, error) { return probed, nil },
		func(choices *core.ProfileConfig) error { copied := *choices; saved = &copied; return nil },
	)
	if err != nil || !reflect.DeepEqual(served.Choices, probed) || !served.Probed {
		t.Fatalf("served %+v, %v", served, err)
	}
	if saved == nil || !reflect.DeepEqual(*saved, probed) {
		t.Fatalf("saved %v", saved)
	}

	// A failed probe falls back to the stale row, also not read now.
	served, err = resolveCachedDiscovery(
		false,
		func(*core.ProfileConfig) bool { return false },
		loadCached,
		func() (core.ProfileConfig, error) { return core.ProfileConfig{}, errors.New("provider offline") },
		func(*core.ProfileConfig) error { panic("a failed probe saves nothing") },
	)
	if err != nil || !reflect.DeepEqual(served.Choices, cached) || served.Probed {
		t.Fatalf("served %+v, %v", served, err)
	}
}

// Muse models and efforts change server-side without a managed-pin bump, so
// caching a worker's observed catalogue hides options people expect. Muse
// observations must leave the cache untouched while other harnesses keep
// caching theirs.
func TestMuseWorkerObservationsAreNeverCached(t *testing.T) {
	const child = "MJ_TEST_MUSE_OBSERVATIONS_UNCACHED"
	if _, ok := os.LookupEnv(child); !ok {
		targets.NewIsolatedTest(targets.TestName("profile_config::tests", "TestMuseWorkerObservationsAreNeverCached")).
			Env(child, "1").
			Env("MJ_INSTANCE", "muse-observations-uncached-test").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	ctx := context.Background()
	root := t.TempDir()
	// A Muse home must end in /muse, its XDG configuration directory.
	home := filepath.Join(root, "muse")
	if err := os.Mkdir(home, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "settings.json"), []byte(`{"model":"muse-spark-1.3-contributor"}`), 0o666); err != nil {
		t.Fatal(err)
	}
	profile := func(kind core.HarnessKind) core.HarnessProfile {
		profileHome := root
		if kind == core.HarnessKindMuse {
			profileHome = home
		}
		profile := core.NewHarnessProfile(kind, profileHome)
		profile.Enabled = true
		profile.Environment = core.NewEnvironment()
		profile.ContextWindowBytes = nil
		profile.GuardianReviewModel = nil
		profile.CodexProfile = nil
		profile.LegacyMuseAutoReview = nil
		profile.LoadBalancingMayConsumeCredits = false
		profile.ClaudePermissionMode = nil
		profile.MjolnirGuardian = true
		profile.AWS = nil
		profile.Models = nil
		profile.LegacySubagents = nil
		return profile
	}
	config := core.DefaultConfig()
	if config.Profiles == nil {
		config.Profiles = map[string]core.HarnessProfile{}
	}
	config.Profiles["muse"] = profile(core.HarnessKindMuse)
	config.Profiles["codex"] = profile(core.HarnessKindCodex)
	if err := config.Save(); err != nil {
		t.Fatal(err)
	}

	observed := func() core.RelayOperationalState {
		version := core.RelayProtocolVersion
		category := schema.SessionConfigOptionCategoryThoughtLevel
		return core.RelayOperationalState{
			RelayProtocolVersion: &version,
			SessionID:            "observed",
			Execution:            core.RelayExecutionStateIdle,
			LatestDigest:         core.RelayEventGenesisDigest,
			AcknowledgedDigest:   core.RelayEventGenesisDigest,
			RecoveryFloorDigest:  core.RelayEventGenesisDigest,
			NativeSessionID:      stringPointer("observed"),
			ConfigOptions: []schema.SessionConfigOption{{
				Kind:     schema.SessionConfigOptionKindSelect,
				ID:       schema.SessionConfigId("effort"),
				Name:     "Effort",
				Category: &category,
				Select: &schema.SessionConfigSelect{
					CurrentValue: schema.SessionConfigValueId("low"),
					Options: []schema.SessionConfigSelectOption{
						{Value: schema.SessionConfigValueId("low"), Name: "Low"},
						{Value: schema.SessionConfigValueId("high"), Name: "High"},
					},
				},
			}},
			Config: map[string]string{},
		}
	}

	// A stale row from before the bypass must survive the observation
	// byte-identical: nothing may refresh it, and no model-keyed row may
	// appear beside it.
	stale := core.ProfileConfig{
		Model:      stringPointer("stale-model"),
		Models:     []core.SessionConfigChoice{},
		Efforts:    []core.SessionConfigChoice{},
		ObservedAt: 1,
	}
	museProfile, _ := config.EnabledProfile("muse")
	museFingerprint := fingerprint(&museProfile)
	storeChoices("muse", museFingerprint, nil, &stale)
	cachedBefore, hadBefore := loadChoices("muse", nil, museFingerprint)

	stored, err := Observe(ctx, "muse", "", core.NewSharedSnapshot(observed()), ObservationCheckNoProbe)
	if err != nil {
		t.Fatal(err)
	}
	if stored {
		t.Fatal("a Muse observation caches nothing")
	}
	if cachedAfter, hadAfter := loadChoices("muse", nil, museFingerprint); hadAfter != hadBefore || !reflect.DeepEqual(cachedAfter, cachedBefore) {
		t.Fatalf("the stale row changed: %+v", cachedAfter)
	}
	if _, ok := loadChoices("muse", stringPointer("muse-spark-1.3-contributor"), museFingerprint); ok {
		t.Fatal("a Muse observation stores no model-keyed row either")
	}

	// Other harnesses still cache worker observations.
	stored, err = Observe(ctx, "codex", "", core.NewSharedSnapshot(observed()), ObservationCheckNoProbe)
	if err != nil {
		t.Fatal(err)
	}
	if !stored {
		t.Fatal("a Codex observation cached nothing")
	}
	codexProfile, _ := config.EnabledProfile("codex")
	if _, ok := loadChoices("codex", nil, fingerprint(&codexProfile)); !ok {
		t.Fatal("the Codex observation is not cached")
	}
}

func TestCreateAndMoveMjolnirPolicyRefuseUnconfiguredAndUnsupportedParents(t *testing.T) {
	ctx := context.Background()
	for _, kind := range []core.HarnessKind{
		core.HarnessKindCodex,
		core.HarnessKindDsh,
		core.HarnessKindOpenCode,
		core.HarnessKindGrok,
		core.HarnessKindMuse,
	} {
		config := core.DefaultConfig()
		parent := core.NewHarnessProfile(kind, "/fixture")
		parent.Enabled = true
		parent.Environment = core.NewEnvironment()
		parent.MjolnirGuardian = true
		parent.LegacySubagents = nil
		config.Profiles = map[string]core.HarnessProfile{"parent": parent}
		err := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &config, "parent",
			&core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}, "localhost")
		if err == nil {
			t.Fatalf("%s: an unconfigured Mjolnir policy was accepted", kind)
		}
		refusal, ok := core.RefusalOf(err)
		if !ok {
			t.Fatalf("%s: not a refusal: %v", kind, err)
		}
		if code, ok := refusal.Code(); !ok || code != core.ChoiceUnavailableCode {
			t.Fatalf("%s: code %q", kind, code)
		}
		if kind.SupportsDelegationTools() {
			if !strings.Contains(refusal.Message(), "[subagents.straightforward]") || !strings.Contains(refusal.Message(), "--subagent-straightforward") {
				t.Fatalf("%s: %s", kind, refusal.Message())
			}
		} else if !strings.Contains(refusal.Message(), "Claude, Codex, DSH, or OpenCode") {
			t.Fatalf("%s: %s", kind, refusal.Message())
		}
		// None asks for no Mjolnir sub-agents at all; every harness accepts
		// it, including one without delegation tools (#1284).
		if err := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &config, "parent",
			&core.SubagentPolicy{Mode: core.SubagentPolicyNone}, "localhost"); err != nil {
			t.Fatalf("%s: %v", kind, err)
		}
		// Native keeps a harness's own sub-agents; DSH has none, so an
		// explicit Native is refused with the two choices it offers.
		native := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &config, "parent",
			&core.SubagentPolicy{Mode: core.SubagentPolicyNative}, "localhost")
		if kind.HasNativeSubagents() {
			if native != nil {
				t.Fatalf("%s: %v", kind, native)
			}
			continue
		}
		if native == nil {
			t.Fatalf("%s: Native was accepted", kind)
		}
		refusal, ok = core.RefusalOf(native)
		if !ok {
			t.Fatalf("%s: not a refusal: %v", kind, native)
		}
		if code, ok := refusal.Code(); ok {
			t.Fatalf("%s: the Settings remedy does not apply, got code %q", kind, code)
		}
		if !strings.Contains(refusal.Message(), "choose Mjolnir or None") {
			t.Fatal(refusal.Message())
		}
		// The default for an omitted policy is accepted.
		defaultPolicy := core.SubagentPolicyDefaultFor(kind)
		if err := ValidateSessionSubagentPolicy(ctx, NewProfileCatalog(context.Background()), &config, "parent",
			&defaultPolicy, "localhost"); err != nil {
			t.Fatalf("%s: %v", kind, err)
		}
	}
}
