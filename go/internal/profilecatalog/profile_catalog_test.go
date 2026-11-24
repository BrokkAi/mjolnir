// Port of the tests in mj-profile-catalog/src/profile_catalog.rs.

package profilecatalog

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func calls() *atomic.Int64 { return new(atomic.Int64) }

// build is Rust's `ProfileCatalog::build` for one test: the catalog shuts
// down when the test ends, as dropping the test's runtime ends every task it
// spawned.
func build(t *testing.T, probe Probe, modelProbe ModelProbe) *ProfileCatalog {
	return BuildProfileCatalog(t.Context(), probe, modelProbe)
}

// withProbe is Rust's `ProfileCatalog::with_probe` for one test.
func withProbe(t *testing.T, probe Probe) *ProfileCatalog {
	return build(t, probe, sameProbe(probe))
}

// sameProbe answers model-specific probes with probe, as
// `Arc::new(move |profile, _| probe(profile))` does.
func sameProbe(probe Probe) ModelProbe {
	return func(profile string, _ string) ProbeRun { return probe(profile) }
}

// gate is the `tokio::sync::watch` channel of a bool a fixture probe waits
// on: a release sent before a waiter arrives is still observed.
type gate struct{ watch *core.Watch[bool] }

func newGate(open bool) *gate { return &gate{watch: core.NewWatch(open)} }

func (g *gate) send(open bool) { g.watch.SendReplace(open) }

// wait is `if !*gate.borrow_and_update() { gate.changed().await }`. ctx
// ending stands for Rust dropping the waiting future.
func (g *gate) wait(ctx context.Context) {
	changed, stop := g.watch.Subscribe()
	defer stop()
	if g.watch.Borrow() {
		return
	}
	select {
	case <-changed:
	case <-ctx.Done():
	}
}

// yieldNow is tokio's yield_now on the test's one thread: spawned work runs
// until it blocks before the test continues. Go cannot observe that point,
// so the test steps aside for a moment.
func yieldNow() { time.Sleep(10 * time.Millisecond) }

// flagProbe is a probe that fails while the flag is set and counts its
// calls, so a test can drive the background pass, the call that retries
// after it, and the cache in between.
func flagProbe(fails *atomic.Bool, calls *atomic.Int64) Probe {
	return func(profile string) ProbeRun {
		return func(context.Context) (core.ProfileConfig, error) {
			calls.Add(1)
			if fails.Load() {
				return core.ProfileConfig{}, errors.New("harness is not installed")
			}
			return TestChoices(profile), nil
		}
	}
}

// gatedProbe is a probe that reports each discovery it starts and then
// waits for the test to release it, so a test can hold a discovery in
// flight and see who shares it.
func gatedProbe(calls *atomic.Int64, started chan<- string, release *gate) Probe {
	return func(profile string) ProbeRun {
		return func(ctx context.Context) (core.ProfileConfig, error) {
			calls.Add(1)
			started <- profile
			release.wait(ctx)
			if err := ctx.Err(); err != nil {
				return core.ProfileConfig{}, err
			}
			return TestChoices(profile), nil
		}
	}
}

// homesConfig is a configuration whose profile homes are real directories
// under root, so the home watch has files to observe, together with the
// id-to-home map the fixture probe answers from.
func homesConfig(root string, profiles []ProfileCandidate) (core.Config, map[string]string) {
	config := core.DefaultConfig()
	config.Profiles = map[string]core.HarnessProfile{}
	homes := map[string]string{}
	for _, profile := range profiles {
		home := filepath.Join(root, profile.ID)
		homes[profile.ID] = home
		config.Profiles[profile.ID] = core.NewHarnessProfile(profile.Kind, home)
	}
	return config, homes
}

func writeHomeModel(t *testing.T, root, profile, model string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Join(root, profile), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, profile, "config.toml"), []byte(fmt.Sprintf("model = %q\n", model)), 0o666); err != nil {
		t.Fatal(err)
	}
}

// setModified is Rust's `File::set_modified` on an open, writable handle.
func setModified(t *testing.T, path string, modified time.Time) {
	t.Helper()
	if err := os.Chtimes(path, time.Time{}, modified); err != nil {
		t.Fatal(err)
	}
}

// readHomeChoices is the choices the profile home's `config.toml`
// advertises: the `model =` line, read the way the real probe reads the
// staged copy.
func readHomeChoices(homes map[string]string, profile string) (core.ProfileConfig, error) {
	home, ok := homes[profile]
	if !ok {
		panic("fixture home")
	}
	text, err := os.ReadFile(filepath.Join(home, "config.toml"))
	if err != nil {
		return core.ProfileConfig{}, fmt.Errorf("read the %s home config.toml: %w", profile, err)
	}
	rest, prefixed := strings.CutPrefix(strings.TrimSpace(string(text)), `model = "`)
	model, quoted := strings.CutSuffix(rest, `"`)
	if !prefixed || !quoted {
		return core.ProfileConfig{}, errors.New("the home config.toml names a model")
	}
	choices := TestChoices(profile)
	choices.Model = &model
	choices.Models = []core.SessionConfigChoice{{Value: model, Name: model}}
	return choices, nil
}

// flakyModelProbe is a probe for model-specific entries that fails while
// the flag is set, so a test can make one entry's re-probe fail while its
// sibling's succeeds.
func flakyModelProbe(homes map[string]string, calls *atomic.Int64, failing *atomic.Bool) ModelProbe {
	return func(profile string, _ string) ProbeRun {
		calls.Add(1)
		return func(context.Context) (core.ProfileConfig, error) {
			if failing.Load() {
				return core.ProfileConfig{}, errors.New("model report failed")
			}
			return readHomeChoices(homes, profile)
		}
	}
}

// homeModelProbe is a probe that answers from the profile home's
// `config.toml`.
func homeModelProbe(homes map[string]string, calls *atomic.Int64) Probe {
	return func(profile string) ProbeRun {
		calls.Add(1)
		return func(context.Context) (core.ProfileConfig, error) {
			return readHomeChoices(homes, profile)
		}
	}
}

// flakyHomeModelProbe is a home-reading probe that fails while the flag is
// set, so a test can drive a failed edit re-probe and its retry.
func flakyHomeModelProbe(homes map[string]string, calls *atomic.Int64, failing *atomic.Bool) Probe {
	return func(profile string) ProbeRun {
		calls.Add(1)
		return func(context.Context) (core.ProfileConfig, error) {
			if failing.Load() {
				return core.ProfileConfig{}, errors.New("provider is offline")
			}
			return readHomeChoices(homes, profile)
		}
	}
}

// heldHomeModelProbe is a home-reading probe that can be held after it has
// read the home, so a test keeps a re-probe in flight — as an expiry
// refresh can be when a home edit lands — and then releases it. The call
// count increments after the read, so reaching a count proves what the
// in-flight probes answered from.
func heldHomeModelProbe(homes map[string]string, calls *atomic.Int64, release *gate) Probe {
	return func(profile string) ProbeRun {
		return func(ctx context.Context) (core.ProfileConfig, error) {
			choices, err := readHomeChoices(homes, profile)
			if err != nil {
				return core.ProfileConfig{}, err
			}
			calls.Add(1)
			release.wait(ctx)
			if err := ctx.Err(); err != nil {
				return core.ProfileConfig{}, err
			}
			return choices, nil
		}
	}
}

func currentModel(catalog *ProfileCatalog, profile string) *string {
	choices, err := catalog.Capabilities(context.Background(), []string{profile})
	if err != nil || len(choices) == 0 {
		return nil
	}
	return choices[len(choices)-1].Model
}

func modelIs(model *string, want string) bool { return model != nil && *model == want }

// convergeOn waits until the catalog serves model as the profile's default,
// so the spawned refresh supervisors have run.
func convergeOn(t *testing.T, catalog *ProfileCatalog, profile, model string) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for !modelIs(currentModel(catalog, profile), model) {
		if time.Now().After(deadline) {
			t.Fatalf("%s did not converge on %s", profile, model)
		}
		runtime.Gosched()
	}
}

func refreshing(catalog *ProfileCatalog) int {
	in, unlock := catalog.lock()
	defer unlock()
	return len(in.refreshing)
}

// waitRefreshed waits until no refresh is in flight.
func waitRefreshed(catalog *ProfileCatalog) {
	for refreshing(catalog) != 0 {
		runtime.Gosched()
	}
}

func expectCalls(t *testing.T, calls *atomic.Int64, want int64, why string) {
	t.Helper()
	if got := calls.Load(); got != want {
		t.Fatalf("%d calls, want %d: %s", got, want, why)
	}
}

func everyEntry(cacheKey, *core.ProfileConfig) bool { return true }
func noEntry(cacheKey, *core.ProfileConfig) bool    { return false }

// Hard-won: 8c88175f7: reads rebuilt the entire unchanged model catalog.
func TestPendingModelEffortsShareOneAttemptAndUnchangedProfilesStayWarm(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	calls := calls()
	started := make(chan string, 16)
	release := newGate(false)
	catalog := build(t,
		func(id string) ProbeRun {
			return func(context.Context) (core.ProfileConfig, error) {
				choices := TestChoices(id)
				choices.Models = append(choices.Models, core.SessionConfigChoice{Value: "other", Name: "Other"})
				return choices, nil
			}
		},
		func(id string, model string) ProbeRun {
			calls.Add(1)
			return func(ctx context.Context) (core.ProfileConfig, error) {
				started <- id
				release.wait(ctx)
				if err := ctx.Err(); err != nil {
					return core.ProfileConfig{}, err
				}
				choices := TestChoices(id)
				choices.Models = append(choices.Models, core.SessionConfigChoice{Value: model, Name: model})
				choices.Model = &model
				return choices, nil
			}
		})
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
	catalog.Sync(&config)
	reader := make(chan error, 1)
	readerConfig := cloneConfig(&config)
	go func() {
		_, err := catalog.OptionsFor(ctx, &readerConfig, "parent", stringPointer("other"))
		reader <- err
	}()
	if id := <-started; id != "parent" {
		t.Fatalf("started %q", id)
	}
	if catalog.Snapshot().Get().Options(&config, nil) == nil {
		t.Fatal("the default catalogue is not published")
	}
	partial := catalog.Snapshot().Get().Options(&config, stringPointer("other"))
	if partial == nil {
		t.Fatal("no partial options")
	}
	if len(partial.Efforts) != 0 {
		t.Fatalf("efforts %v", partial.Efforts)
	}
	if !slices.ContainsFunc(partial.Unavailable, func(reason string) bool {
		return strings.Contains(reason, "effort discovery pending")
	}) {
		t.Fatalf("unavailable %v", partial.Unavailable)
	}
	// Adding a different profile must leave this model's pending owner
	// intact.
	config.Profiles["sibling"] = TestConfig([]ProfileCandidate{{ID: "sibling", Kind: core.HarnessKindClaude}}).Profiles["sibling"]
	catalog.Sync(&config)
	select {
	case <-reader:
		t.Fatal("the reader finished before its probe was released")
	default:
	}
	expectCalls(t, calls, 1, "one pending model probe")
	release.send(true)
	if err := <-reader; err != nil {
		t.Fatal(err)
	}
	catalog.SyncNow(ctx, &config)
	if _, err := catalog.OptionsFor(ctx, &config, "parent", stringPointer("other")); err != nil {
		t.Fatal(err)
	}
	delete(config.Profiles, "sibling")
	catalog.SyncNow(ctx, &config)
	if _, err := catalog.OptionsFor(ctx, &config, "parent", stringPointer("other")); err != nil {
		t.Fatal(err)
	}
	expectCalls(t, calls, 2, "membership changes retain unchanged efforts")
	held := catalog.Snapshot()
	if _, err := catalog.OptionsFor(ctx, &config, "parent", stringPointer("other")); err != nil {
		t.Fatal(err)
	}
	if !held.PtrEq(catalog.Snapshot()) {
		t.Fatal("a warm read rebuilt the snapshot")
	}
}

// Hard-won: #1380: warming launched one harness per advertised model.
func TestOnlySelectedModelsLaunchEffortProbes(t *testing.T) {
	currentThread(t)
	var mu sync.Mutex
	probed := []string{}
	modelProbe := func(id string, model string) ProbeRun {
		mu.Lock()
		probed = append(probed, model)
		mu.Unlock()
		return func(context.Context) (core.ProfileConfig, error) {
			choices := TestChoices(id)
			choices.Model = &model
			return choices, nil
		}
	}
	// An aggregating harness advertises hundreds of provider models.
	catalog := build(t, func(id string) ProbeRun {
		return func(context.Context) (core.ProfileConfig, error) {
			choices := TestChoices(id)
			for n := range 300 {
				choices.Models = append(choices.Models, core.SessionConfigChoice{
					Value: fmt.Sprintf("provider/model-%d", n),
					Name:  fmt.Sprintf("Model %d", n),
				})
			}
			return choices, nil
		}
	}, modelProbe)
	config := TestConfig([]ProfileCandidate{{ID: "opencode", Kind: core.HarnessKindOpenCode}})
	config.Subagents.Straightforward = []core.SubagentChoice{{Model: "provider/model-7"}}
	catalog.Sync(&config)
	settled := func(model string) {
		deadline := time.Now().Add(10 * time.Second)
		for catalog.Snapshot().Get().Options(&config, &model) == nil {
			if time.Now().After(deadline) {
				t.Fatal("selected efforts are discovered")
			}
			runtime.Gosched()
		}
	}
	probedModels := func() []string {
		mu.Lock()
		defer mu.Unlock()
		return slices.Clone(probed)
	}
	settled("provider/model-7")
	if got := probedModels(); !slices.Equal(got, []string{"provider/model-7"}) {
		t.Fatalf("probed %v", got)
	}

	// A Settings draft selecting another model probes just that one, and
	// only the models probed are published.
	draft := cloneConfig(&config)
	draft.Subagents.Subtle = []core.SubagentChoice{{Model: "provider/model-42"}}
	catalog.Ensure(&draft)
	settled("provider/model-42")
	if got := probedModels(); !slices.Equal(got, []string{"provider/model-7", "provider/model-42"}) {
		t.Fatalf("probed %v", got)
	}
	key := config.Profiles["opencode"].CapabilitiesKey("opencode")
	efforts := slices.Sorted(maps.Keys(catalog.Snapshot().Get().Profiles[key].Efforts))
	if !slices.Equal(efforts, []string{"opencode-model", "provider/model-42", "provider/model-7"}) {
		t.Fatalf("published efforts for %v", efforts)
	}
}

// Muse catalogues change server-side without a managed-pin bump, so a
// retained answer hides models and efforts people expect. Every Muse read
// probes live; other harnesses keep serving their warm entries.
func TestMuseReadsProbeLiveWhileOtherHarnessesStayWarm(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	calls := calls()
	catalog := withProbe(t, func(profile string) ProbeRun {
		return func(context.Context) (core.ProfileConfig, error) {
			revision := calls.Add(1) - 1
			model := fmt.Sprintf("%s-model-%d", profile, revision)
			choices := TestChoices(profile)
			choices.Model = &model
			choices.Models = []core.SessionConfigChoice{{Value: model, Name: model}}
			return choices, nil
		}
	})
	config := TestConfig([]ProfileCandidate{{ID: "muse", Kind: core.HarnessKindMuse}, {ID: "codex", Kind: core.HarnessKindCodex}})
	catalog.SyncNow(ctx, &config)

	read, err := catalog.Capabilities(ctx, []string{"muse"})
	if err != nil {
		t.Fatal(err)
	}
	first := read[0].Models[0].Value
	// Wait until the supervisor published this probe, so the next read
	// cannot join the same in-flight attempt: it must start its own.
	key := config.Profiles["muse"].CapabilitiesKey("muse")
	deadline := time.Now().Add(10 * time.Second)
	for {
		entry, ok := catalog.Snapshot().Get().Profiles[key]
		if ok && entry.Choices.Kind == core.CapabilityStateReady && len(entry.Choices.Value.Models) > 0 &&
			entry.Choices.Value.Models[0].Value == first {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the supervisor publishes each live Muse probe")
		}
		time.Sleep(10 * time.Millisecond)
	}

	second, err := catalog.Capabilities(ctx, []string{"muse"})
	if err != nil {
		t.Fatal(err)
	}
	if second[0].Models[0].Value == first {
		t.Fatal("a Muse read never serves the retained catalogue")
	}

	codexFirst, err := catalog.Capabilities(ctx, []string{"codex"})
	if err != nil {
		t.Fatal(err)
	}
	codexSecond, err := catalog.Capabilities(ctx, []string{"codex"})
	if err != nil {
		t.Fatal(err)
	}
	if codexFirst[0].Models[0].Value != codexSecond[0].Models[0].Value {
		t.Fatal("other harnesses keep serving their warm entries")
	}
}

func TestConcurrentReadersJoinStartupHydration(t *testing.T) {
	currentThread(t)
	calls := calls()
	started := make(chan string, 16)
	release := newGate(false)
	catalog := withProbe(t, gatedProbe(calls, started, release))
	config := TestConfig([]ProfileCandidate{
		{ID: "parent", Kind: core.HarnessKindCodex},
		{ID: "helper", Kind: core.HarnessKindClaude},
	})
	catalog.Sync(&config)
	<-started
	<-started
	type read struct {
		choices []core.ProfileConfig
		err     error
	}
	reader := make(chan read, 1)
	go func() {
		choices, err := catalog.Capabilities(context.Background(), []string{"parent", "helper"})
		reader <- read{choices: choices, err: err}
	}()
	catalog.Sync(&config)
	expectCalls(t, calls, 2, "readers join the startup probes")
	if candidates, err := catalog.Candidates(); err != nil || len(candidates) != 2 {
		t.Fatalf("candidates %v, %v", candidates, err)
	}
	if catalog.Snapshot().Get().Options(&config, nil) != nil {
		t.Fatal("options before any probe completed")
	}
	release.send(true)
	result := <-reader
	if result.err != nil || len(result.choices) != 2 {
		t.Fatalf("read %v, %v", result.choices, result.err)
	}
	expectCalls(t, calls, 2, "readers join the startup probes")
}

func TestProfileDefinitionChangesDoNotRetireUnrelatedReadsOrPublishOldChoices(t *testing.T) {
	currentThread(t)
	calls := calls()
	started := make(chan string, 16)
	release := newGate(false)
	catalog := withProbe(t, func(id string) ProbeRun {
		number := calls.Add(1) - 1
		return func(ctx context.Context) (core.ProfileConfig, error) {
			started <- id
			if number == 0 {
				release.wait(ctx)
				if err := ctx.Err(); err != nil {
					return core.ProfileConfig{}, err
				}
			}
			choices := TestChoices(id)
			choices.ObservedAt = number
			return choices, nil
		}
	})
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
	catalog.Sync(&config)
	<-started
	oldKey := config.Profiles["parent"].CapabilitiesKey("parent")
	type read struct {
		choices []core.ProfileConfig
		err     error
	}
	pending := make(chan read, 1)
	go func() {
		choices, err := catalog.Capabilities(context.Background(), []string{"parent"})
		pending <- read{choices: choices, err: err}
	}()
	yieldNow()
	parent := config.Profiles["parent"]
	parent.Home = "/changed"
	config.Profiles["parent"] = parent
	config.Profiles["sibling"] = TestConfig([]ProfileCandidate{{ID: "sibling", Kind: core.HarnessKindClaude}}).Profiles["sibling"]
	catalog.Sync(&config)
	current := <-pending
	if current.err != nil {
		t.Fatal(current.err)
	}
	if current.choices[0].ObservedAt != 1 {
		t.Fatalf("observed at %d", current.choices[0].ObservedAt)
	}
	choices, err := catalog.Capabilities(context.Background(), []string{"sibling"})
	if err != nil {
		t.Fatal(err)
	}
	if !modelIs(choices[0].Model, "sibling-model") {
		t.Fatalf("sibling model %v", choices[0].Model)
	}
	release.send(true)
	if oldKey == config.Profiles["parent"].CapabilitiesKey("parent") {
		t.Fatal("the home is not part of the capability key")
	}
	choices, err = catalog.Capabilities(context.Background(), []string{"parent"})
	if err != nil {
		t.Fatal(err)
	}
	if choices[0].ObservedAt != 1 {
		t.Fatalf("observed at %d", choices[0].ObservedAt)
	}
	expectCalls(t, calls, 3, "one probe per definition")
}

func TestDraftsShareGlobalEntriesWithoutAdoptingTheirProfileList(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	calls := calls()
	catalog := withProbe(t, CountingProbe(calls))
	live := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
	catalog.SyncNow(ctx, &live)
	draft := cloneConfig(&live)
	parent := draft.Profiles["parent"]
	parent.Home = "/draft"
	draft.Profiles["parent"] = parent
	catalog.Ensure(&draft)
	catalog.Ensure(&draft)
	var reads sync.WaitGroup
	errs := make(chan error, 2)
	for range 2 {
		reads.Go(func() {
			_, err := catalog.OptionsFor(ctx, &draft, "parent", nil)
			errs <- err
		})
	}
	reads.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatal(err)
		}
	}
	expectCalls(t, calls, 2, "the draft shares one probe")
	adopted, err := catalog.Live()
	if err != nil {
		t.Fatal(err)
	}
	if adopted.Profiles["parent"].Home != live.Profiles["parent"].Home {
		t.Fatal("a draft became the live configuration")
	}
	if _, err := catalog.OptionsFor(ctx, &live, "parent", nil); err != nil {
		t.Fatal(err)
	}
	expectCalls(t, calls, 2, "the live entry stays warm")
}

func TestExpiredEntriesAreProbedAgainAndKeptUntilAProbeSucceeds(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	calls := calls()
	failing := new(atomic.Bool)
	catalog := withProbe(t, flagProbe(failing, calls))
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
	catalog.SyncNow(ctx, &config)
	expectCalls(t, calls, 1, "startup probe")
	catalog.refresh(noEntry)
	expectCalls(t, calls, 1, "current entries stay")

	failing.Store(true)
	catalog.refresh(everyEntry)
	waitRefreshed(catalog)
	// The default entry and the current model's copy are both probed.
	expectCalls(t, calls, 3, "both entries re-probe")
	choices, err := catalog.Capabilities(ctx, []string{"parent"})
	if err != nil {
		t.Fatal(err)
	}
	if !modelIs(choices[0].Model, "parent-model") {
		t.Fatal("a failed refresh keeps serving the old choices")
	}

	failing.Store(false)
	catalog.refresh(everyEntry)
	waitRefreshed(catalog)
	expectCalls(t, calls, 5, "both entries re-probe again")
	if _, err := catalog.Capabilities(ctx, []string{"parent"}); err != nil {
		t.Fatal(err)
	}
}

func pollHome(t *testing.T, home string, previous *homeFiles) homeFiles {
	t.Helper()
	files, ok := pollHomeFiles(home, previous)
	if !ok {
		t.Fatal("the home could not be polled")
	}
	return files
}

func TestHomeWatchDigestTracksContentNotMetadata(t *testing.T) {
	home := t.TempDir()
	absent := pollHome(t, home, nil)
	unchanged := pollHome(t, home, &absent)
	if absent.digest != unchanged.digest {
		t.Fatal("no edit, no change")
	}

	config := filepath.Join(home, "config.toml")
	if err := os.WriteFile(config, []byte("model = \"one\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	written := pollHome(t, home, &absent)
	if absent.digest == written.digest {
		t.Fatal("a new catalog input is an edit")
	}

	if err := os.WriteFile(config, []byte("model = \"one\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	if written.digest != pollHome(t, home, &written).digest {
		t.Fatal("rewriting identical content is metadata, not an edit")
	}

	if err := os.WriteFile(config, []byte("model = \"two\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	if written.digest == pollHome(t, home, &written).digest {
		t.Fatal("changed content is an edit")
	}

	if err := os.Remove(config); err != nil {
		t.Fatal(err)
	}
	if written.digest == pollHome(t, home, &written).digest {
		t.Fatal("a deleted input is an edit")
	}
}

// Hard-won: d9efe7006: Windows timestamp fixtures need writable handles.
func TestHomeWatchRehashCatchesASameSizeRewriteWithAnUnchangedStat(t *testing.T) {
	home := t.TempDir()
	config := filepath.Join(home, "config.toml")
	if err := os.WriteFile(config, []byte("model = \"one\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	first := pollHome(t, home, nil)
	storedMtime := time.Unix(0, first.inputs[0].stat.mtime)

	// A rewrite that keeps the byte length and restores the mtime is
	// invisible to the stat; only the recency re-hash catches it, the way a
	// same-second edit lands on a one-second-granularity filesystem.
	if err := os.WriteFile(config, []byte("model = \"two\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	setModified(t, config, storedMtime)
	rewritten := pollHome(t, home, &first)
	if !fileStatsEqual(first.inputs[0].stat, rewritten.inputs[0].stat) {
		t.Fatalf("stat %+v, want %+v", rewritten.inputs[0].stat, first.inputs[0].stat)
	}
	if first.digest == rewritten.digest {
		t.Fatal("a same-size rewrite with an unchanged stat is still an edit")
	}
}

// Hard-won: #1337 review: a timestamp-preserving rewrite hid from the
// poll while a probe recorded the fresh digest, so the identity looked
// stale every pass and re-probed forever.
// Hard-won: d9efe7006: Windows timestamp fixtures need writable handles.
func TestAStatHiddenRewriteConvergesOnceAProbeReadsIt(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	old := time.Now().Add(-120 * time.Second)
	config := filepath.Join(root, "solo", "config.toml")
	writeHomeModel(t, root, "solo", "aaa")
	setModified(t, config, old)
	calls := calls()
	profiles, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	catalog := withProbe(t, homeModelProbe(homes, calls))
	catalog.SyncNow(context.Background(), &profiles)
	expectCalls(t, calls, 1, "startup probe")
	catalog.watchHomes()

	// A timestamp-preserving rewrite of the same length is invisible to
	// the stat poll and stays undetected while no probe reads the home.
	writeHomeModel(t, root, "solo", "bbb")
	setModified(t, config, old)
	catalog.watchHomes()
	expectCalls(t, calls, 1, "a stat-hidden rewrite stays undetected until something reads the home")

	// A probe reads the whole home — the guaranteed one-hour expiry refresh
	// is one — and records the fresh digest. The poll, still blind, must
	// catch up from the probe's read instead of churning a re-probe every
	// pass.
	catalog.refresh(everyEntry)
	waitRefreshed(catalog)
	if model := currentModel(catalog, "solo"); !modelIs(model, "bbb") {
		t.Fatalf("model %v", model)
	}
	for range 3 {
		catalog.watchHomes()
		waitRefreshed(catalog)
	}
	expectCalls(t, calls, 3, "the poll reconciles with the probe's read and stays quiet")
}

// Hard-won: #1337: editing a profile's harness home never reached the catalog.
func TestAnEditedHomeRepublishesWithinOneWatchPassAndReprimesOnlyThatHome(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "alpha", "alpha-first")
	writeHomeModel(t, root, "beta", "beta-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{
		{ID: "alpha", Kind: core.HarnessKindCodex},
		{ID: "beta", Kind: core.HarnessKindCodex},
	})
	catalog := withProbe(t, homeModelProbe(homes, calls))
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 2, "startup probes")

	// The first pass only baselines the homes; nothing is re-probed.
	catalog.watchHomes()
	expectCalls(t, calls, 2, "baselining launches no probe")

	// Rewriting identical content (new mtime, same bytes) is not an edit.
	writeHomeModel(t, root, "alpha", "alpha-first")
	catalog.watchHomes()
	expectCalls(t, calls, 2, "a metadata-only rewrite re-probes nothing")

	writeHomeModel(t, root, "alpha", "alpha-second")
	catalog.watchHomes()
	expectCalls(t, calls, 4, "the edited home's default and current-model entries re-probe; beta stays warm")
	convergeOn(t, catalog, "alpha", "alpha-second")
	if model := currentModel(catalog, "beta"); !modelIs(model, "beta-first") {
		t.Fatalf("beta model %v", model)
	}
	expectCalls(t, calls, 4, "nothing else re-probes")
}

func TestAHomeSharedByTwoProfilesReprimesEachOfThemAndNoOthers(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "one", "shared-first")
	writeHomeModel(t, root, "other", "other-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{
		{ID: "one", Kind: core.HarnessKindCodex},
		{ID: "two", Kind: core.HarnessKindCodex},
		{ID: "other", Kind: core.HarnessKindCodex},
	})
	// Two profiles install the same home; editing it must re-probe both.
	two := config.Profiles["two"]
	two.Home = filepath.Join(root, "one")
	config.Profiles["two"] = two
	homes["two"] = filepath.Join(root, "one")
	catalog := withProbe(t, homeModelProbe(homes, calls))
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 3, "startup probes")

	catalog.watchHomes()
	expectCalls(t, calls, 3, "baselining launches no probe")
	writeHomeModel(t, root, "one", "shared-second")
	catalog.watchHomes()
	expectCalls(t, calls, 7, "both profiles of the edited home re-probe, each entry they serve")
	convergeOn(t, catalog, "one", "shared-second")
	convergeOn(t, catalog, "two", "shared-second")
	if model := currentModel(catalog, "other"); !modelIs(model, "other-first") {
		t.Fatalf("other model %v", model)
	}
	expectCalls(t, calls, 7, "nothing else re-probes")
}

// Hard-won: #1337: a home edit detected while another re-probe was in
// flight settled on the pre-edit answer that probe published.
func TestAChangeDetectedDuringAnInFlightReprobeIsNotLost(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	release := newGate(true)
	catalog := withProbe(t, heldHomeModelProbe(homes, calls, release))
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 1, "startup probe")
	catalog.watchHomes()

	// An expiry refresh starts and reads the home before the edit; held in
	// flight, it will publish the pre-edit answer when released.
	release.send(false)
	catalog.refresh(everyEntry)
	for calls.Load() < 3 {
		runtime.Gosched()
	}
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 3, "entries already re-probing are joined, not doubled")

	// The held probes publish the pre-edit answer.
	release.send(true)
	waitRefreshed(catalog)
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-first") {
		t.Fatalf("the in-flight probe published what it read, got %v", model)
	}
	expectCalls(t, calls, 3, "no new probe")

	// The completing probes record the digest they read, which predates the
	// edit, so the edit stays pending: the next pass re-detects it and
	// re-probes from the current home. This pass's probes are spawned, so
	// convergence proves they ran.
	catalog.watchHomes()
	convergeOn(t, catalog, "solo", "solo-second")
	expectCalls(t, calls, 5, "the edit re-probes both entries")
}

// Hard-won: #1337 review: an edit landing while one entry's probe was
// in flight re-armed the retry window at launch, so the lagging entry
// — which never failed — waited out the window behind its sibling.
func TestALaggingEntryReprimesOnTheNextPassAfterItsSiblingLaunched(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	release := newGate(true)
	catalog := build(t,
		func(profile string) ProbeRun {
			return func(ctx context.Context) (core.ProfileConfig, error) {
				// Count after the read so a test can tell the home was read
				// before it edits the file.
				choices, err := readHomeChoices(homes, profile)
				if err != nil {
					return core.ProfileConfig{}, err
				}
				calls.Add(1)
				release.wait(ctx)
				if err := ctx.Err(); err != nil {
					return core.ProfileConfig{}, err
				}
				return choices, nil
			}
		},
		func(profile string, model string) ProbeRun {
			calls.Add(1)
			return func(context.Context) (core.ProfileConfig, error) {
				choices, err := readHomeChoices(homes, profile)
				if err != nil {
					return core.ProfileConfig{}, err
				}
				choices.Model = &model
				return choices, nil
			}
		})
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 1, "startup probe")
	catalog.watchHomes()

	// An expiry-style refresh holds the default entry's probe in flight,
	// having read the home before the edit.
	release.send(false)
	catalog.refresh(func(key cacheKey, _ *core.ProfileConfig) bool { return !key.hasModel })
	for calls.Load() < 2 {
		runtime.Gosched()
	}
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	// The pass joins the held probe and launches only the sibling.
	expectCalls(t, calls, 3, "only the sibling launches")
	// Let the held probe publish the pre-edit answer it already read; the
	// sibling publishes the edit.
	release.send(true)
	waitRefreshed(catalog)
	// The held probe published the pre-edit answer; the sibling published
	// the edit. Nothing failed, so no window bounds the lagging entry: the
	// next pass re-probes it. The lagging default probe counts after it
	// reads, so convergence proves both ran.
	catalog.watchHomes()
	expectCalls(t, calls, 4, "the next pass launches the lagging entry's re-probe at once")
	convergeOn(t, catalog, "solo", "solo-second")
	expectCalls(t, calls, 5, "the lagging entry re-probed")
}

// Hard-won: #1337: a failed edit re-probe recorded nothing, so the edit
// looked settled until the one-hour expiry refresh.
func TestAFailedHomeEditReprobeIsRetriedUntilItSucceeds(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	failing := new(atomic.Bool)
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	flaky := flakyHomeModelProbe(homes, calls, failing)
	catalog := build(t, flaky, sameProbe(flaky))
	catalog.homeEditRetryWait = 50 * time.Millisecond
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 1, "startup probe")
	catalog.watchHomes()

	failing.Store(true)
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 3, "the edit re-probes every served entry at once")
	// Let the launched probes run, and confirm they ran while the flag was
	// set: they fail, the old choices stay, and nothing settles.
	waitRefreshed(catalog)
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-first") {
		t.Fatalf("a failed re-probe keeps serving the old choices, got %v", model)
	}

	// The failure recorded nothing, so the edit stays pending; only the
	// retry cadence keeps a persistent failure from probing every pass.
	catalog.watchHomes()
	expectCalls(t, calls, 3, "the failed re-probe waits for its retry window")

	failing.Store(false)
	deadline := time.Now().Add(10 * time.Second)
	for {
		catalog.watchHomes()
		if modelIs(currentModel(catalog, "solo"), "solo-second") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the failed re-probe is retried until the edit converges")
		}
		time.Sleep(10 * time.Millisecond)
	}
	expectCalls(t, calls, 5, "the retry re-probes both entries once")
}

// Hard-won: #1337 review: a follow-up edit right after a successful
// re-probe stalled behind the failed-reprobe retry window.
func TestAFollowUpEditReprimesWithoutWaitingForTheRetryWindow(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	catalog := withProbe(t, homeModelProbe(homes, calls))
	catalog.homeEditRetryWait = 60 * time.Second
	catalog.SyncNow(context.Background(), &config)
	catalog.watchHomes()

	// The first edit re-probes and succeeds, all before the retry window
	// that arming recorded could matter.
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 3, "the edit re-probes both entries")
	waitRefreshed(catalog)
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-second") {
		t.Fatalf("model %v", model)
	}

	// A follow-up edit is a new change: it must re-probe on the next pass,
	// not wait out the previous launch's retry window. Every served entry
	// re-probes: the default, the previous model's, and the current
	// model's.
	writeHomeModel(t, root, "solo", "solo-third")
	catalog.watchHomes()
	expectCalls(t, calls, 6, "a new edit re-probes on the next pass even inside the retry window")
	convergeOn(t, catalog, "solo", "solo-third")
}

// Hard-won: #1337 review: an identity-level settled digest let one
// entry's failed re-probe hide behind a sibling's success.
func TestAFailedEntryReprimesEvenWhenItsSiblingReprobeSucceeded(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	failing := new(atomic.Bool)
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	catalog := build(t, homeModelProbe(homes, calls), flakyModelProbe(homes, calls, failing))
	catalog.homeEditRetryWait = 50 * time.Millisecond
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 1, "startup probe")
	catalog.watchHomes()

	// The edit re-probes every served entry; the default entry's probe
	// succeeds and the model entry's fails.
	failing.Store(true)
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 3, "the edit re-probes both entries")
	waitRefreshed(catalog)
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-second") {
		t.Fatalf("the default entry's re-probe published the edit, got %v", model)
	}

	// The failed model entry keeps serving its pre-edit copy; its sibling's
	// success must not settle it. The failure re-arms the retry window, so
	// the next pass waits it out instead of re-probing the whole identity
	// every poll.
	catalog.watchHomes()
	expectCalls(t, calls, 3, "the failed re-probe re-arms the retry window")
	time.Sleep(60 * time.Millisecond)
	catalog.watchHomes()
	expectCalls(t, calls, 6, "a failed entry re-probes after the window even when its sibling's re-probe succeeded")
	// That round fails too (the flag is still set), re-arming the window;
	// let the failures land before allowing success.
	waitRefreshed(catalog)

	failing.Store(false)
	for range 4 {
		catalog.watchHomes()
		waitRefreshed(catalog)
		time.Sleep(60 * time.Millisecond)
	}
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-second") {
		t.Fatalf("model %v", model)
	}
	expectCalls(t, calls, 9, "the retry re-probes each entry once")
}

// Hard-won: #1337 review: a still-pending entry kept its identity stale,
// re-probing the healthy entries every retry window.
func TestAPendingEntryDoesNotKeepReprimingItsHealthySiblings(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	config.Subagents.Straightforward = []core.SubagentChoice{{Model: "extra"}}
	// The selected model's probe waits for a release the test controls,
	// standing for a harness launch that has not finished.
	release := newGate(false)
	catalog := build(t,
		func(profile string) ProbeRun {
			calls.Add(1)
			return func(context.Context) (core.ProfileConfig, error) {
				choices, err := readHomeChoices(homes, profile)
				if err != nil {
					return core.ProfileConfig{}, err
				}
				choices.Models = append(choices.Models, core.SessionConfigChoice{Value: "extra", Name: "extra"})
				return choices, nil
			}
		},
		func(profile string, model string) ProbeRun {
			calls.Add(1)
			return func(ctx context.Context) (core.ProfileConfig, error) {
				release.wait(ctx)
				if err := ctx.Err(); err != nil {
					return core.ProfileConfig{}, err
				}
				choices, err := readHomeChoices(homes, profile)
				if err != nil {
					return core.ProfileConfig{}, err
				}
				choices.Model = &model
				return choices, nil
			}
		})
	catalog.Sync(&config)
	// The default probe launches at once; the selected model's probe is
	// claimed when it publishes, and the test holds it in flight, its entry
	// still Pending.
	expectCalls(t, calls, 1, "the default probe launches at once")
	convergeOn(t, catalog, "solo", "solo-first")
	expectCalls(t, calls, 2, "the selected model's probe is claimed")

	// The Pending entry neither settles nor blocks convergence: passes stay
	// quiet, with no re-probes of the healthy default.
	catalog.watchHomes()
	catalog.watchHomes()
	expectCalls(t, calls, 2, "a pending entry does not keep its identity stale")

	// Once the probe finishes and records its digest, the identity settles
	// at one digest and stays quiet.
	release.send(true)
	deadline := time.Now().Add(10 * time.Second)
	for {
		in, unlock := catalog.lock()
		ready := true
		for _, e := range in.entries {
			if e.kind != entryReady {
				ready = false
			}
		}
		unlock()
		if ready {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the held probe publishes")
		}
		runtime.Gosched()
	}
	catalog.watchHomes()
	expectCalls(t, calls, 2, "the settled identity stays quiet")
}

// Hard-won: #1337 review: a fresh edit after a failed re-probe waited
// out the retry window instead of re-probing on the next pass.
func TestAFreshEditBypassesTheRetryWindowOfAnEarlierFailure(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	failing := new(atomic.Bool)
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	flaky := flakyHomeModelProbe(homes, calls, failing)
	catalog := build(t, flaky, sameProbe(flaky))
	catalog.homeEditRetryWait = 60 * time.Second
	catalog.SyncNow(context.Background(), &config)
	catalog.watchHomes()

	// The first edit's re-probe fails, arming the window.
	failing.Store(true)
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 3, "the edit re-probes both entries")
	waitRefreshed(catalog)
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-first") {
		t.Fatalf("model %v", model)
	}

	// A fresh edit is a new digest, not the failure the window is retrying:
	// it re-probes on the next pass, inside the window.
	writeHomeModel(t, root, "solo", "solo-third")
	catalog.watchHomes()
	expectCalls(t, calls, 5, "a fresh edit bypasses the earlier failure's retry window")
}

// Hard-won: #1337 review: a failure outside a watch launch (the routine
// expiry refresh) armed the window against every digest, so a fresh
// edit waited out the window too.
func TestAFreshEditBypassesTheWindowOfAnUnrelatedFailure(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	failing := new(atomic.Bool)
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	flaky := flakyHomeModelProbe(homes, calls, failing)
	catalog := build(t, flaky, sameProbe(flaky))
	catalog.homeEditRetryWait = 50 * time.Millisecond
	catalog.SyncNow(context.Background(), &config)
	catalog.watchHomes()

	// A routine expiry refresh fails once, arming the window against the
	// state the probe was correcting.
	failing.Store(true)
	catalog.refresh(everyEntry)
	waitRefreshed(catalog)
	expectCalls(t, calls, 3, "the expiry refresh re-probed every served entry and failed")

	// The failure's window is about the old digest; a fresh edit is a new
	// digest and re-probes on the next pass, inside the window.
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 5, "a fresh edit bypasses the window armed by an unrelated failure")
	waitRefreshed(catalog)

	// The bypassed re-probe failed too, and the window adopted the edit's
	// digest: the same state now retries at the bounded cadence instead of
	// re-probing every pass.
	catalog.watchHomes()
	expectCalls(t, calls, 5, "a failed fresh edit waits out the window it re-armed")

	failing.Store(false)
	for range 4 {
		catalog.watchHomes()
		waitRefreshed(catalog)
		time.Sleep(60 * time.Millisecond)
	}
	if model := currentModel(catalog, "solo"); !modelIs(model, "solo-second") {
		t.Fatalf("model %v", model)
	}
	expectCalls(t, calls, 7, "the retry re-probes both entries once")
}

// Hard-won: #1337 review: a permanently failed entry kept its identity
// stale, re-probing the healthy entries every pass.
func TestAFailedEntryDoesNotKeepReprimingItsHealthySiblings(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	config.Subagents.Straightforward = []core.SubagentChoice{{Model: "extra"}}
	catalog := build(t,
		func(profile string) ProbeRun {
			calls.Add(1)
			return func(context.Context) (core.ProfileConfig, error) {
				choices, err := readHomeChoices(homes, profile)
				if err != nil {
					return core.ProfileConfig{}, err
				}
				choices.Models = append(choices.Models, core.SessionConfigChoice{Value: "extra", Name: "extra"})
				return choices, nil
			}
		},
		func(profile string, model string) ProbeRun {
			calls.Add(1)
			return func(context.Context) (core.ProfileConfig, error) {
				// The delisted model's probe fails for good; other models
				// answer from the home like the default probe does.
				if model == "extra" {
					return core.ProfileConfig{}, DiscoveryUnsupported{}
				}
				choices, err := readHomeChoices(homes, profile)
				if err != nil {
					return core.ProfileConfig{}, err
				}
				choices.Model = &model
				return choices, nil
			}
		})
	catalog.SyncNow(context.Background(), &config)
	// SyncNow claimed the default and the current model, and the selected
	// model's probe failed permanently.
	expectCalls(t, calls, 2, "the default and the selected model probed")

	// The failed entry neither settles nor blocks convergence: passes stay
	// quiet, with no re-probes of the healthy entries.
	catalog.watchHomes()
	catalog.watchHomes()
	expectCalls(t, calls, 2, "a failed entry does not keep its identity stale")

	// A home edit still re-probes the healthy entries, and convergence
	// returns once they record the new digest.
	writeHomeModel(t, root, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 4, "the healthy entries re-probe")
	convergeOn(t, catalog, "solo", "solo-second")
	catalog.watchHomes()
	expectCalls(t, calls, 4, "the identity converged")
}

// Hard-won: #1337: an edit made while the daemon was stopped stayed stale after start.
func TestANewCatalogGenerationDiscoversTheCurrentHome(t *testing.T) {
	currentThread(t)
	root := t.TempDir()
	writeHomeModel(t, root, "solo", "solo-first")
	calls := calls()
	config, homes := homesConfig(root, []ProfileCandidate{{ID: "solo", Kind: core.HarnessKindCodex}})
	first := withProbe(t, homeModelProbe(homes, calls))
	first.SyncNow(context.Background(), &config)
	if model := currentModel(first, "solo"); !modelIs(model, "solo-first") {
		t.Fatalf("model %v", model)
	}

	// Capability entries and the process-global probe cache live only as
	// long as the daemon process, so a daemon that stopped and started
	// again discovers fresh. A new catalog over the same configuration is
	// what such a start looks like from inside one test process.
	writeHomeModel(t, root, "solo", "solo-second")
	second := withProbe(t, homeModelProbe(homes, calls))
	second.SyncNow(context.Background(), &config)
	expectCalls(t, calls, 2, "a fresh generation probes instead of serving the previous generation's answer")
	if model := currentModel(second, "solo"); !modelIs(model, "solo-second") {
		t.Fatalf("model %v", model)
	}
}

func TestFailuresAreReportedAndOnlyTheOwnerRetriesAfterBackoff(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		calls := calls()
		failing := new(atomic.Bool)
		failing.Store(true)
		probe := flagProbe(failing, calls)
		catalog := BuildProfileCatalog(ctx, probe, sameProbe(probe))
		config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
		catalog.SyncNow(ctx, &config)
		for range 3 {
			if _, err := catalog.Capabilities(ctx, []string{"parent"}); err == nil {
				t.Fatal("a failed profile answered")
			}
		}
		expectCalls(t, calls, 1, "failed UI queries cannot launch retries")
		options := catalog.Snapshot().Get().Options(&config, nil)
		if options == nil || !strings.Contains(options.Unavailable[0], "harness is not installed") {
			t.Fatalf("options %+v", options)
		}
		failing.Store(false)
		time.Sleep(time.Second)
		synctest.Wait()
		if _, err := catalog.Capabilities(ctx, []string{"parent"}); err != nil {
			t.Fatal(err)
		}
		expectCalls(t, calls, 2, "the owner retried once")
	})
}

// Hard-won: 18ef04c19: a retryable failure kept a disabled profile's supervisor probing in the background.
func TestADisabledProfileRetriesAFailureOnlyWhenRequestedAgain(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		calls := calls()
		failing := new(atomic.Bool)
		failing.Store(true)
		probe := flagProbe(failing, calls)
		catalog := BuildProfileCatalog(ctx, probe, sameProbe(probe))
		config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
		parent := config.Profiles["parent"]
		parent.Enabled = false
		config.Profiles["parent"] = parent
		catalog.SyncNow(ctx, &config)
		if _, err := catalog.fetchSettled(ctx, &config, "parent", nil); err == nil {
			t.Fatal("a failing probe answered")
		}
		expectCalls(t, calls, 1, "the request probed once")
		time.Sleep(600 * time.Second)
		synctest.Wait()
		expectCalls(t, calls, 1, "no background retry")

		failing.Store(false)
		if _, err := catalog.fetchSettled(ctx, &config, "parent", nil); err != nil {
			t.Fatal(err)
		}
		expectCalls(t, calls, 2, "the next request retried")
	})
}

// Hard-won: a5eb2adae: re-enabling a profile kept its failed entry with no supervisor to retry it.
func TestReEnablingAProfileRetriesItsFailedDiscovery(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		calls := calls()
		failing := new(atomic.Bool)
		failing.Store(true)
		probe := flagProbe(failing, calls)
		catalog := BuildProfileCatalog(ctx, probe, sameProbe(probe))
		config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
		parent := config.Profiles["parent"]
		parent.Enabled = false
		config.Profiles["parent"] = parent
		catalog.SyncNow(ctx, &config)
		if _, err := catalog.fetchSettled(ctx, &config, "parent", nil); err == nil {
			t.Fatal("a failing probe answered")
		}
		time.Sleep(600 * time.Second)
		synctest.Wait()
		expectCalls(t, calls, 1, "no background retry")

		failing.Store(false)
		parent.Enabled = true
		config.Profiles["parent"] = parent
		catalog.SyncNow(ctx, &config)
		if _, err := catalog.Capabilities(ctx, []string{"parent"}); err != nil {
			t.Fatal(err)
		}
		expectCalls(t, calls, 2, "re-enabling retried")
	})
}

// Hard-won: #1232: Windows has no local worker, so every probe failed and the owner retried forever.
func TestADiscoveryNoRetryCanFixWaitsForASessionToReportChoices(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		calls := calls()
		// Discovery reads the profile cache first; this flag stands for a
		// session's worker having put its choices there.
		observed := new(atomic.Bool)
		probe := func(profile string) ProbeRun {
			calls.Add(1)
			cached := observed.Load()
			return func(context.Context) (core.ProfileConfig, error) {
				if cached {
					return TestChoices(profile), nil
				}
				return core.ProfileConfig{}, DiscoveryUnsupported{}
			}
		}
		catalog := BuildProfileCatalog(ctx, probe, sameProbe(probe))
		config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
		catalog.SyncNow(ctx, &config)
		time.Sleep(600 * time.Second)
		synctest.Wait()
		expectCalls(t, calls, 1, "no retry")
		options := catalog.Snapshot().Get().Options(&config, nil)
		if options == nil || !strings.Contains(options.Unavailable[0], "runs no local Mjolnir worker") {
			t.Fatalf("%+v", options)
		}

		observed.Store(true)
		catalog.RereadFailed("parent")
		choices, err := catalog.fetch(ctx, &config, "parent", nil)
		if err != nil {
			t.Fatal(err)
		}
		if !profileConfigsEqual(&choices, ptr(TestChoices("parent"))) {
			t.Fatalf("choices %+v", choices)
		}
		options = catalog.Snapshot().Get().Options(&config, nil)
		if options == nil || len(options.Unavailable) != 0 {
			t.Fatalf("%+v", options)
		}
		if !slices.EqualFunc(options.Models, TestChoices("parent").Models, sessionConfigChoicesEqual) {
			t.Fatalf("models %+v", options.Models)
		}
	})
}

func ptr[T any](value T) *T { return &value }

func TestShutdownCancelsReadersAndBackgroundHydration(t *testing.T) {
	currentThread(t)
	calls := calls()
	started := make(chan string, 16)
	release := newGate(false)
	cancellation, cancel := context.WithCancel(t.Context())
	defer cancel()
	probe := gatedProbe(calls, started, release)
	catalog := BuildProfileCatalog(cancellation, probe, sameProbe(probe))
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
	catalog.Sync(&config)
	<-started
	cancel()
	_, err := catalog.Capabilities(context.Background(), []string{"parent"})
	if err == nil || !strings.Contains(err.Error(), "shutdown") {
		t.Fatalf("error %v", err)
	}
}

func TestAnswersBeforeAdoptionReportThatWithoutLaunchingAProbe(t *testing.T) {
	currentThread(t)
	calls := calls()
	catalog := withProbe(t, CountingProbe(calls))
	if _, err := catalog.Candidates(); err == nil || !strings.Contains(err.Error(), "not adopted") {
		t.Fatalf("error %v", err)
	}
	if _, err := catalog.Capabilities(context.Background(), []string{"parent"}); err == nil {
		t.Fatal("a read before adoption answered")
	}
	expectCalls(t, calls, 0, "no probe before adoption")
}

// Rust's tokio::join! polls both requests before the probe they share runs.
// Goroutines promise no such order, and a request that comes after the
// answer re-probes it once expired (the fixture's 2023 observation is
// expired on the real clock), so the test runs on synctest's clock, which
// starts in 2000 and keeps the fixture current.
func TestDisabledProfileIsDiscoveredOnlyOnRequestAndNeverRefreshed(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		probes := calls()
		probe := CountingProbe(probes)
		catalog := BuildProfileCatalog(ctx, probe, sameProbe(probe))
		config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
		parent := config.Profiles["parent"]
		parent.Enabled = false
		config.Profiles["parent"] = parent
		catalog.SyncNow(ctx, &config)
		expectCalls(t, probes, 0, "a disabled profile is not probed")
		if candidates, err := catalog.Candidates(); err != nil || len(candidates) != 0 {
			t.Fatalf("candidates %v, %v", candidates, err)
		}
		var reads sync.WaitGroup
		errs := make(chan error, 2)
		for range 2 {
			reads.Go(func() {
				_, err := catalog.fetchSettled(ctx, &config, "parent", nil)
				errs <- err
			})
		}
		reads.Wait()
		close(errs)
		for err := range errs {
			if err != nil {
				t.Fatal(err)
			}
		}
		expectCalls(t, probes, 1, "the requests share one probe")
		if launched := catalog.refresh(everyEntry); len(launched) != 0 {
			t.Fatalf("refreshed %v", launched)
		}
		if candidates, err := catalog.Candidates(); err != nil || len(candidates) != 0 {
			t.Fatalf("candidates %v, %v", candidates, err)
		}
	})
}

// Hard-won: 1152c612e: a definition kept from before disabling still read its old enabled copy and kept refreshing.
func TestDisablingADiscoveredProfileEndsItsBackgroundRefresh(t *testing.T) {
	currentThread(t)
	probes := calls()
	catalog := withProbe(t, CountingProbe(probes))
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}})
	catalog.SyncNow(context.Background(), &config)
	expectCalls(t, probes, 1, "startup probe")
	parent := config.Profiles["parent"]
	parent.Enabled = false
	config.Profiles["parent"] = parent
	catalog.SyncNow(context.Background(), &config)
	if launched := catalog.refresh(everyEntry); len(launched) != 0 {
		t.Fatalf("refreshed %v", launched)
	}
	expectCalls(t, probes, 1, "no refresh of a disabled profile")
}

func TestSubagentCandidatesIncludeEveryEnabledProfile(t *testing.T) {
	catalog := NewProfileCatalog(t.Context())
	config := TestConfig([]ProfileCandidate{
		{ID: "codex", Kind: core.HarnessKindCodex},
		{ID: "helper", Kind: core.HarnessKindCodex},
		{ID: "claude4", Kind: core.HarnessKindClaude},
		{ID: "disabled-parent", Kind: core.HarnessKindKimi},
	})
	disabled := config.Profiles["disabled-parent"]
	disabled.Enabled = false
	config.Profiles["disabled-parent"] = disabled
	catalog.Sync(&config)

	ids := func(candidates []ProfileCandidate, err error) []string {
		if err != nil {
			t.Fatal(err)
		}
		ids := []string{}
		for _, candidate := range candidates {
			ids = append(ids, candidate.ID)
		}
		return ids
	}
	subagentIDs := ids(catalog.Candidates())
	configuredIDs := ids(catalog.ConfiguredCandidates())
	if !slices.Equal(subagentIDs, []string{"claude4", "codex", "helper"}) {
		t.Fatalf("candidates %v", subagentIDs)
	}
	if slices.Contains(subagentIDs, "disabled-parent") {
		t.Fatal("a disabled profile is a candidate")
	}
	if !slices.Equal(configuredIDs, []string{"claude4", "codex", "helper"}) {
		t.Fatalf("configured %v", configuredIDs)
	}
}

func legacyUpgradeFixture(t *testing.T, cancellation context.Context, path string, codexOffline *atomic.Bool) *ProfileCatalog {
	probe := func(profile string) ProbeRun {
		offline := codexOffline.Load()
		return func(context.Context) (core.ProfileConfig, error) {
			if profile == "codex" && offline {
				return core.ProfileConfig{}, errors.New("codex profile is unavailable")
			}
			models := []string{"claude-sonnet-4.5"}
			if profile == "codex" {
				models = []string{"gpt-6-terra", "gpt-6-sol"}
			}
			choices := core.ProfileConfig{Model: stringPointer(models[0]), Efforts: []core.SessionConfigChoice{}, ObservedAt: 1}
			for _, value := range models {
				choices.Models = append(choices.Models, choice(value))
			}
			return choices, nil
		}
	}
	catalog := BuildProfileCatalog(cancellation, probe, sameProbe(probe))
	catalog.configPath = &path
	config := TestConfig([]ProfileCandidate{
		{ID: "codex", Kind: core.HarnessKindCodex},
		{ID: "sonnet", Kind: core.HarnessKindClaude},
	})
	config.Review = core.ReviewConfigLegacyDefault()
	catalog.Sync(&config)
	return catalog
}

func legacyReviewFile(t *testing.T, path string) {
	legacyReviewFileWith(t, path, []ProfileCandidate{
		{ID: "codex", Kind: core.HarnessKindCodex},
		{ID: "sonnet", Kind: core.HarnessKindClaude},
	})
}

func legacyReviewFileWith(t *testing.T, path string, profiles []ProfileCandidate) {
	t.Helper()
	config := TestConfig(profiles)
	for id, profile := range config.Profiles {
		if profile.Kind == core.HarnessKindOpenCode {
			profile.Home = filepath.Join(profile.Home, "opencode")
			config.Profiles[id] = profile
		}
	}
	config.Review = core.ReviewConfigLegacyDefault()
	if err := config.SaveTo(path); err != nil {
		t.Fatal(err)
	}
}

func loadConfig(t *testing.T, path string) core.Config {
	t.Helper()
	config, err := core.ConfigLoadFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	return config
}

func TestPartialLegacyDiscoveryRetriesBeforePersistingToItsConfigPath(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "custom-config.toml")
	legacyReviewFile(t, path)
	offline := new(atomic.Bool)
	offline.Store(true)
	catalog := legacyUpgradeFixture(t, t.Context(), path, offline)
	err := catalog.upgradeLegacyReview(ctx, 1)
	if err == nil || !strings.Contains(err.Error(), "codex") {
		t.Fatalf("error %v", err)
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("a partial discovery persisted")
	}
	offline.Store(false)
	catalog.RereadFailed("codex")
	if err := catalog.upgradeLegacyReview(ctx, 2); err != nil {
		t.Fatal(err)
	}
	review := loadConfig(t, path).Review
	if review.IsLegacy() {
		t.Fatal("the upgrade did not persist")
	}
	if len(review.Reviewers) != 2 || review.Reviewers[0].Model != "gpt-6-terra" {
		t.Fatalf("reviewers %+v", review.Reviewers)
	}
}

// subscriptionReport is the report a refresh produces for a subscription
// profile.
func subscriptionReport(profile string, harness core.HarnessKind) client.ProfileQuota {
	remaining := uint8(80)
	return client.ProfileQuota{
		ProfileID: profile,
		Harness:   harness,
		Windows: []client.QuotaWindow{{
			Label:            "Week",
			RemainingPercent: &remaining,
		}},
		RefreshedAtEpochSeconds: 0,
	}
}

// notReported is the report a refresh produces for an OpenCode profile.
func notReported(profile string) client.ProfileQuota {
	report := subscriptionReport(profile, core.HarnessKindOpenCode)
	report.Windows = []client.QuotaWindow{}
	report.Extra = stringPointer(client.NotReportedLabel)
	return report
}

// billingCatalog is a catalog over codex, sonnet and an OpenCode
// "openrouter" profile whose catalogue holds only `latest` aliases, with
// quota reports reports.
func billingCatalog(t *testing.T, cancellation context.Context, path string, reports *QuotaReports) *ProfileCatalog {
	legacyReviewFileWith(t, path, []ProfileCandidate{
		{ID: "codex", Kind: core.HarnessKindCodex},
		{ID: "sonnet", Kind: core.HarnessKindClaude},
		{ID: "openrouter", Kind: core.HarnessKindOpenCode},
	})
	probe := func(profile string) ProbeRun {
		return func(context.Context) (core.ProfileConfig, error) {
			var models []string
			switch profile {
			case "codex":
				models = []string{"gpt-6.1-terra", "gpt-6.1-sol"}
			case "sonnet":
				models = []string{"sonnet"}
			default:
				models = []string{
					"openrouter/~openai/gpt-terra-latest",
					"openrouter/~openai/gpt-sol-latest",
					"openrouter/~anthropic/claude-sonnet-latest",
				}
			}
			choices := core.ProfileConfig{Models: []core.SessionConfigChoice{}, Efforts: []core.SessionConfigChoice{}, ObservedAt: 1}
			for _, value := range models {
				name := value
				if value == "sonnet" {
					name = "Sonnet 4.5"
				}
				choices.Models = append(choices.Models, core.SessionConfigChoice{Value: value, Name: name})
			}
			return choices, nil
		}
	}
	catalog := BuildProfileCatalog(cancellation, probe, sameProbe(probe))
	catalog.configPath = &path
	catalog.SetQuotaReports(reports)
	config := loadConfig(t, path)
	config.Review = core.ReviewConfigLegacyDefault()
	catalog.Sync(&config)
	return catalog
}

func publishedReports(reports map[string]client.ProfileQuota) accounts.QuotaReportMap {
	published := accounts.QuotaReportMap{}
	for id, report := range reports {
		published[id] = accounts.QuotaReportPublication{CacheIdentity: "test-identity", Report: &report}
	}
	return published
}

func upgradedModels(t *testing.T, path string) []string {
	models := []string{}
	for _, choice := range loadConfig(t, path).Review.Reviewers {
		models = append(models, choice.Model)
	}
	return models
}

func TestLegacyUpgradeSkipsModelsOnlyOpencodeProfilesOffer(t *testing.T) {
	path := filepath.Join(t.TempDir(), "custom-config.toml")
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		// The OpenCode profile's real report is `n/a`, not an API label.
		reports := &QuotaReports{Reports: publishedReports(map[string]client.ProfileQuota{
			"codex":      subscriptionReport("codex", core.HarnessKindCodex),
			"openrouter": notReported("openrouter"),
		})}
		catalog := billingCatalog(t, ctx, path, reports)
		upgrade := make(chan error, 1)
		go func() { upgrade <- catalog.upgradeLegacyReview(ctx, 1) }()
		// OpenAI fills both slots, so an unused Claude profile's unknown
		// billing must not delay the upgrade.
		if err := <-upgrade; err != nil {
			t.Fatal(err)
		}
		if loadConfig(t, path).Review.IsLegacy() {
			t.Fatal("the upgrade did not persist")
		}
		if got := upgradedModels(t, path); !slices.Equal(got, []string{"gpt-6.1-terra", "gpt-6.1-sol"}) {
			t.Fatalf("reviewers %v", got)
		}
	})
}

func TestLegacyUpgradeTreatsAnOpencodeProfileWithoutAReportAsUsagePriced(t *testing.T) {
	path := filepath.Join(t.TempDir(), "custom-config.toml")
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		reports := &QuotaReports{Reports: publishedReports(map[string]client.ProfileQuota{
			"codex":  subscriptionReport("codex", core.HarnessKindCodex),
			"sonnet": subscriptionReport("sonnet", core.HarnessKindClaude),
		})}
		catalog := billingCatalog(t, ctx, path, reports)
		if err := catalog.upgradeLegacyReview(ctx, 1); err != nil {
			t.Fatal(err)
		}
		if got := upgradedModels(t, path); !slices.Equal(got, []string{"gpt-6.1-terra", "gpt-6.1-sol"}) {
			t.Fatalf("reviewers %v", got)
		}
	})
}

func TestLegacyUpgradeDoesNotPersistWhileADraftedProfileHasNoBilling(t *testing.T) {
	path := filepath.Join(t.TempDir(), "custom-config.toml")
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		// The selected OpenAI profile's quota probe never reports.
		reports := &QuotaReports{Reports: publishedReports(map[string]client.ProfileQuota{
			"sonnet": subscriptionReport("sonnet", core.HarnessKindClaude),
		})}
		catalog := billingCatalog(t, ctx, path, reports)
		err := catalog.upgradeLegacyReview(ctx, 1)
		if err == nil || !strings.Contains(err.Error(), "quota reports: codex") {
			t.Fatalf("error %v", err)
		}
		if !loadConfig(t, path).Review.IsLegacy() {
			t.Fatal("the upgrade persisted a guess")
		}
		// A report that arrives before the next attempt lets it finish.
		report := subscriptionReport("codex", core.HarnessKindCodex)
		reports.Lock()
		reports.Reports["codex"] = accounts.QuotaReportPublication{CacheIdentity: "test-identity", Report: &report}
		reports.Unlock()
		if err := catalog.upgradeLegacyReview(ctx, 2); err != nil {
			t.Fatal(err)
		}
		if got := upgradedModels(t, path); !slices.Equal(got, []string{"gpt-6.1-terra", "gpt-6.1-sol"}) {
			t.Fatalf("reviewers %v", got)
		}
	})
}

func TestStuckProfileTimesOutWithoutHidingAReadyProfile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "custom-config.toml")
	legacyReviewFile(t, path)
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		probe := func(profile string) ProbeRun {
			return func(ctx context.Context) (core.ProfileConfig, error) {
				if profile == "codex" {
					// Rust's std::future::pending: never answers while
					// the definition lives.
					<-ctx.Done()
					return core.ProfileConfig{}, ctx.Err()
				}
				return core.ProfileConfig{
					Model:      stringPointer("claude-sonnet-4.5"),
					Models:     []core.SessionConfigChoice{{Value: "claude-sonnet-4.5", Name: "Sonnet"}},
					Efforts:    []core.SessionConfigChoice{},
					ObservedAt: 1,
				}, nil
			}
		}
		catalog := BuildProfileCatalog(ctx, probe, sameProbe(probe))
		catalog.configPath = &path
		config := TestConfig([]ProfileCandidate{
			{ID: "codex", Kind: core.HarnessKindCodex},
			{ID: "sonnet", Kind: core.HarnessKindClaude},
		})
		config.Review = core.ReviewConfigLegacyDefault()
		catalog.Sync(&config)
		task := make(chan error, 1)
		go func() { task <- catalog.upgradeLegacyReview(ctx, 12) }()
		synctest.Wait()
		time.Sleep(31 * time.Second)
		if err := <-task; err == nil {
			t.Fatal("a stuck profile let the upgrade finish")
		}
		if !loadConfig(t, path).Review.IsLegacy() {
			t.Fatal("the upgrade persisted without the stuck profile")
		}
	})
}

func TestExhaustedLegacyDiscoveryKeepsRetrying(t *testing.T) {
	currentThread(t)
	path := filepath.Join(t.TempDir(), "custom-config.toml")
	legacyReviewFile(t, path)
	offline := new(atomic.Bool)
	offline.Store(true)
	catalog := legacyUpgradeFixture(t, t.Context(), path, offline)
	if err := catalog.upgradeLegacyReview(context.Background(), 12); err == nil {
		t.Fatal("an offline profile let the upgrade finish")
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("the upgrade persisted")
	}
}

func TestFailedLegacyDiscoveryDoesNotFinalizeEmptyReviewers(t *testing.T) {
	currentThread(t)
	path := filepath.Join(t.TempDir(), "config.toml")
	legacyReviewFile(t, path)
	probe := func(string) ProbeRun {
		return func(context.Context) (core.ProfileConfig, error) {
			return core.ProfileConfig{}, errors.New("unavailable")
		}
	}
	catalog := build(t, probe, sameProbe(probe))
	catalog.configPath = &path
	config := TestConfig([]ProfileCandidate{{ID: "codex", Kind: core.HarnessKindCodex}})
	catalog.Sync(&config)
	if err := catalog.upgradeLegacyReview(context.Background(), 12); err == nil {
		t.Fatal("a failed discovery finished the upgrade")
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("the upgrade persisted")
	}
}

func TestLegacyAutoUpgradePersistsOnceWithoutRewritingOtherSections(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(fmt.Sprintf(
		"# keep this comment\nversion = %d\n[review]\ntrigger = \"turn\"\n[phone]\nenabled = false\n",
		core.ConfigVersion)), 0o666); err != nil {
		t.Fatal(err)
	}
	reviewer := core.SubagentChoice{Model: "claude-sonnet-4.5"}
	wrote, err := persistLegacyReviewTo(path, []core.SubagentChoice{reviewer}, false, nil, &core.LegacyReview{})
	if err != nil || !wrote {
		t.Fatalf("persist %v, %v", wrote, err)
	}
	saved, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(saved), "# keep this comment") {
		t.Fatalf("saved %s", saved)
	}
	if !strings.Contains(string(saved), "[phone]\nenabled = false") {
		t.Fatalf("saved %s", saved)
	}
	loaded := loadConfig(t, path)
	if loaded.Review.IsLegacy() {
		t.Fatal("the review is still legacy")
	}
	if len(loaded.Review.Reviewers) != 1 || loaded.Review.Reviewers[0].Model != reviewer.Model || loaded.Review.Reviewers[0].Effort != nil {
		t.Fatalf("reviewers %+v", loaded.Review.Reviewers)
	}
	if _, err := persistLegacyReviewTo(path, nil, false, nil, &core.LegacyReview{}); err == nil {
		t.Fatal("an empty upgrade persisted")
	}
	if got := len(loadConfig(t, path).Review.Reviewers); got != 1 {
		t.Fatalf("%d reviewers", got)
	}
}

func TestLegacyUpgradeRetriesWhenProfileDefinitionsChangeBeforeSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	legacyReviewFile(t, path)
	original := loadConfig(t, path)
	definitions := upgradeDefinitions(&original)
	changed := cloneConfig(&original)
	codex := changed.Profiles["codex"]
	codex.Home = filepath.Join(codex.Home, "new-home")
	changed.Profiles["codex"] = codex
	if err := changed.SaveTo(path); err != nil {
		t.Fatal(err)
	}
	_, err := persistLegacyReviewTo(path, []core.SubagentChoice{{Model: "sonnet"}}, false, definitions, original.Review.Legacy)
	if err == nil || !strings.Contains(err.Error(), "profile definitions changed") {
		t.Fatalf("error %v", err)
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("the upgrade persisted")
	}
}

func TestLegacyUpgradeRetriesWhenAProfileModelsListChangesBeforeSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	legacyReviewFile(t, path)
	original := loadConfig(t, path)
	definitions := upgradeDefinitions(&original)
	changed := cloneConfig(&original)
	codex := changed.Profiles["codex"]
	codex.Models = []string{"gpt-6.1-sol"}
	changed.Profiles["codex"] = codex
	if err := changed.SaveTo(path); err != nil {
		t.Fatal(err)
	}
	_, err := persistLegacyReviewTo(path, []core.SubagentChoice{{Model: "gpt-6.1-terra"}}, false, definitions, original.Review.Legacy)
	if err == nil || !strings.Contains(err.Error(), "profile definitions changed") {
		t.Fatalf("error %v", err)
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("the upgrade persisted")
	}
}

func TestCompleteLegacyAutoWithoutSupportedModelsTurnsReviewOff(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	legacyReviewFile(t, path)
	config := loadConfig(t, path)
	definitions := upgradeDefinitions(&config)
	wrote, err := persistLegacyReviewTo(path, nil, true, definitions, config.Review.Legacy)
	if err != nil || !wrote {
		t.Fatalf("persist %v, %v", wrote, err)
	}
	upgraded := loadConfig(t, path)
	if upgraded.Review.IsLegacy() {
		t.Fatal("the review is still legacy")
	}
	if upgraded.Review.Trigger() != core.ReviewTriggerOff {
		t.Fatalf("trigger %v", upgraded.Review.Trigger())
	}
	if len(upgraded.Review.Reviewers) != 0 {
		t.Fatalf("reviewers %+v", upgraded.Review.Reviewers)
	}
}

func TestPendingAutomaticReviewUpgradesAndUnrelatedSaveKeepsIt(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	// A configuration that asked for review without naming a model. One with
	// no `[review]` table never asked, so it stays off and is not given
	// reviewers.
	if err := os.WriteFile(path, []byte(fmt.Sprintf("version = %d\n# keep\n[review]\ntrigger = \"push\"\n", core.ConfigVersion)), 0o666); err != nil {
		t.Fatal(err)
	}
	config := loadConfig(t, path)
	if !config.Review.IsLegacy() {
		t.Fatal("a review without a model is not legacy")
	}
	none := filepath.Join(directory, "none.toml")
	if err := os.WriteFile(none, []byte(fmt.Sprintf("version = %d\n", core.ConfigVersion)), 0o666); err != nil {
		t.Fatal(err)
	}
	if loadConfig(t, none).Review.IsLegacy() {
		t.Fatal("a configuration without review asked for it")
	}
	if err := config.SaveTo(path); err != nil {
		t.Fatal(err)
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("an unrelated save dropped the pending upgrade")
	}
	saved, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(saved), "reviewers") {
		t.Fatalf("saved %s", saved)
	}
	if _, err := persistLegacyReviewTo(path, nil, false, nil, &core.LegacyReview{}); err == nil {
		t.Fatal("an empty upgrade persisted")
	}
	if !loadConfig(t, path).Review.IsLegacy() {
		t.Fatal("a refused upgrade dropped the pending one")
	}
	saved, err = os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(saved), "reviewers = []") {
		t.Fatalf("saved %s", saved)
	}
}

// The `models` allow-list reaches every reader through the two exits of the
// catalogue, and an edit applies without a new probe.
func TestAModelsAllowListFiltersTheSnapshotAndReadsUntilTheConfigChanges(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	probes := calls()
	catalog := withProbe(t, func(profile string) ProbeRun {
		probes.Add(1)
		return func(context.Context) (core.ProfileConfig, error) {
			choices := TestChoices(profile)
			for index := 1; index < 4; index++ {
				choices.Models = append(choices.Models, choice(fmt.Sprintf("m%d", index)))
			}
			for _, value := range []string{"claude-opus", "gpt-5", "gemini-3"} {
				choices.Models = append(choices.Models, choice(value))
			}
			return choices, nil
		}
	})
	config := TestConfig([]ProfileCandidate{{ID: "p", Kind: core.HarnessKindAntigravity}})
	key := config.Profiles["p"].CapabilitiesKey("p")
	snapshotModels := func() []string {
		entry := catalog.Snapshot().Get().Profiles[key]
		if entry.Choices.Kind != core.CapabilityStateReady {
			t.Fatalf("not ready: %+v", entry.Choices)
		}
		return choiceValues(entry.Choices.Value.Models)
	}
	read := func(models []core.SessionConfigChoice, err error) []string {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
		return choiceValues(models)
	}
	readDefault := func() []string {
		t.Helper()
		choices, err := catalog.Capabilities(ctx, []string{"p"})
		if err != nil {
			t.Fatal(err)
		}
		return choiceValues(choices[0].Models)
	}
	everything := []string{"p-model", "m1", "m2", "m3", "claude-opus", "gpt-5", "gemini-3"}
	setProfile := func(edit func(profile *core.HarnessProfile)) {
		profile := config.Profiles["p"]
		edit(&profile)
		config.Profiles["p"] = profile
	}
	catalog.SyncNow(ctx, &config)
	if got := snapshotModels(); !slices.Equal(got, everything) {
		t.Fatalf("snapshot %v", got)
	}

	// The current model stays offered although the list omits it.
	setProfile(func(profile *core.HarnessProfile) { profile.Models = []string{"m2", "unadvertised"} })
	if config.Profiles["p"].CapabilitiesKey("p") != key {
		t.Fatal("the allow-list is not part of the cache key")
	}
	probed := probes.Load()
	catalog.SyncNow(ctx, &config)
	if got := snapshotModels(); !slices.Equal(got, []string{"p-model", "m2"}) {
		t.Fatalf("snapshot %v", got)
	}
	if got := readDefault(); !slices.Equal(got, []string{"p-model", "m2"}) {
		t.Fatalf("read %v", got)
	}
	modelRead, err := catalog.ModelCapabilities(ctx, "p", "m2")
	if got := read(modelRead.Models, err); !slices.Equal(got, []string{"p-model", "m2"}) {
		t.Fatalf("model read %v", got)
	}
	unrestricted, err := catalog.UnrestrictedCapabilities(ctx, "p")
	if got := read(unrestricted.Models, err); !slices.Equal(got, everything) {
		t.Fatalf("unrestricted %v", got)
	}
	if got := catalog.ModelOffer("p").Models; !slices.Equal(got, []string{"m2", "unadvertised"}) {
		t.Fatalf("offer %v", got)
	}
	expectCalls(t, probes, probed, "editing the allow-list does not rediscover")

	setProfile(func(profile *core.HarnessProfile) { profile.Enabled = false })
	catalog.SyncNow(ctx, &config)
	if got := readDefault(); !slices.Equal(got, []string{"p-model", "m2"}) {
		t.Fatalf("read %v", got)
	}
	if got := catalog.ModelOffer("p").Models; !slices.Equal(got, []string{"m2", "unadvertised"}) {
		t.Fatalf("offer %v", got)
	}
	setProfile(func(profile *core.HarnessProfile) { profile.Models = nil })
	catalog.SyncNow(ctx, &config)
	if got := readDefault(); !slices.Equal(got, everything) {
		t.Fatalf("read %v", got)
	}
	setProfile(func(profile *core.HarnessProfile) {
		profile.Enabled = true
		profile.ThirdPartyModels = false
	})
	catalog.SyncNow(ctx, &config)
	if got := snapshotModels(); !slices.Equal(got, []string{"p-model", "m1", "m2", "m3", "gemini-3"}) {
		t.Fatalf("snapshot %v", got)
	}
}

// Hard-won: 9d1f70d2b: refreshing Muse retired an admission waiter.
func TestAdmissionFollowsAReplacedPendingMuseOwner(t *testing.T) {
	currentThread(t)
	probes := calls()
	starts := make(chan string, 16)
	release := newGate(false)
	catalog := withProbe(t, gatedProbe(probes, starts, release))
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}, {ID: "p", Kind: core.HarnessKindMuse}})
	config.Subagents.Straightforward = []core.SubagentChoice{{Model: "p-model"}}
	policy := core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	catalog.Sync(&config)
	validationConfig := cloneConfig(&config)
	validation := make(chan error, 1)
	go func() {
		validation <- ValidateSessionSubagentPolicy(context.Background(), catalog, &validationConfig, "parent", &policy, "localhost")
	}()
	yieldNow()
	select {
	case err := <-validation:
		t.Fatalf("validation finished before discovery: %v", err)
	default:
	}
	for range 2 {
		<-starts
	}
	catalog.Sync(&config)
	if started := <-starts; started != "p" {
		t.Fatalf("started %q", started)
	}
	// The waiter must join the replacement before either probe completes.
	yieldNow()
	select {
	case err := <-validation:
		t.Fatalf("validation finished before discovery: %v", err)
	default:
	}
	release.send(true)
	if err := <-validation; err != nil {
		t.Fatal(err)
	}
	expectCalls(t, probes, 3, "the replacement is the only new probe")
}

func TestAdmissionSharesPendingMuseWarmupAndRetainsItsResult(t *testing.T) {
	currentThread(t)
	probes := calls()
	catalog := withProbe(t, CountingProbe(probes))
	config := TestConfig([]ProfileCandidate{{ID: "parent", Kind: core.HarnessKindCodex}, {ID: "p", Kind: core.HarnessKindMuse}})
	selected := core.SubagentChoice{Model: "p-model"}
	config.Subagents.Straightforward = []core.SubagentChoice{selected}
	config.Subagents.Subtle = []core.SubagentChoice{selected}
	catalog.Sync(&config)
	policy := core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	for range 2 {
		if err := ValidateSessionSubagentPolicy(context.Background(), catalog, &config, "parent", &policy, "localhost"); err != nil {
			t.Fatal(err)
		}
	}
	expectCalls(t, probes, 2, "one startup probe per profile across both validations")
}

// Hard-won: dfbb210a5: restricted Muse reads restarted default discovery.
func TestRestrictedLiveModelReadReusesCachedDefault(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	probes := calls()
	catalog := build(t, CountingProbe(probes), func(id string, model string) ProbeRun {
		probes.Add(1)
		return func(context.Context) (core.ProfileConfig, error) {
			choices := TestChoices(id)
			choices.Model = &model
			return choices, nil
		}
	})
	config := TestConfig([]ProfileCandidate{{ID: "p", Kind: core.HarnessKindMuse}})
	profile := config.Profiles["p"]
	profile.Models = []string{"allowed"}
	config.Profiles["p"] = profile
	catalog.SyncNow(ctx, &config)
	probes.Store(0)
	choices, err := catalog.ModelCapabilities(ctx, "p", "allowed")
	if err != nil {
		t.Fatal(err)
	}
	expectCalls(t, probes, 1, "only the model read probes")
	if !slices.Contains(choiceValues(choices.Models), "p-model") {
		t.Fatalf("models %v", choiceValues(choices.Models))
	}
}

// Hard-won: 9120b5643: an explicit read of a Muse profile disabled after discovery joined its retired owner and failed.
func TestADisabledMuseProfileGetsAFreshOwnerOnRequest(t *testing.T) {
	currentThread(t)
	ctx := context.Background()
	probes := calls()
	catalog := withProbe(t, CountingProbe(probes))
	config := TestConfig([]ProfileCandidate{{ID: "p", Kind: core.HarnessKindMuse}})
	catalog.SyncNow(ctx, &config)
	profile := config.Profiles["p"]
	profile.Enabled = false
	config.Profiles["p"] = profile
	catalog.SyncNow(ctx, &config)
	if _, err := catalog.fetch(ctx, &config, "p", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := catalog.fetch(ctx, &config, "p", stringPointer("p-model")); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: a2afafcb3: a pending read applied its starting allowlist.
func TestAPendingDiscoveryUsesTheCurrentModelsSetting(t *testing.T) {
	currentThread(t)
	started := make(chan struct{}, 16)
	release := newGate(false)
	catalog := withProbe(t, func(id string) ProbeRun {
		return func(ctx context.Context) (core.ProfileConfig, error) {
			started <- struct{}{}
			release.wait(ctx)
			if err := ctx.Err(); err != nil {
				return core.ProfileConfig{}, err
			}
			choices := TestChoices(id)
			choices.Models = append(choices.Models, choice("old"), choice("new"))
			return choices, nil
		}
	})
	config := TestConfig([]ProfileCandidate{{ID: "p", Kind: core.HarnessKindCodex}})
	profile := config.Profiles["p"]
	profile.Models = []string{"old"}
	config.Profiles["p"] = profile
	catalog.Sync(&config)
	<-started
	type read struct {
		choices core.ProfileConfig
		err     error
	}
	reader := make(chan read, 1)
	readerConfig := cloneConfig(&config)
	go func() {
		choices, err := catalog.fetch(context.Background(), &readerConfig, "p", nil)
		reader <- read{choices: choices, err: err}
	}()
	yieldNow()
	profile.Models = []string{"new"}
	config.Profiles["p"] = profile
	catalog.Sync(&config)
	release.send(true)
	result := <-reader
	if result.err != nil {
		t.Fatal(result.err)
	}
	if got := choiceValues(result.choices.Models); !slices.Equal(got, []string{"p-model", "new"}) {
		t.Fatalf("models %v", got)
	}
}
