// Port of the tests in mj-import/src/utility_llm.rs.

package mjimport

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/anvilclient"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func TestKimiUtilityUsesProfileAuthEndpointAndHeadersWithoutARuntime(t *testing.T) {
	var mu sync.Mutex
	var failures []string
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/coding/v1/models" || request.Method != http.MethodGet {
			http.NotFound(writer, request)
			return
		}
		mu.Lock()
		for header, want := range map[string]string{
			"authorization":   "Bearer profile-key",
			"x-msh-device-id": "profile-device",
			"x-profile-test":  "profile-header",
		} {
			if got := request.Header.Get(header); got != want {
				failures = append(failures, fmt.Sprintf("%s = %q, want %q", header, got, want))
			}
		}
		mu.Unlock()
		writer.Header().Set("Content-Type", "application/json")
		fmt.Fprint(writer, `{"data":[{"id":"kimi-k2.5"}]}`)
	}))
	defer server.Close()
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "device_id"), []byte("profile-device"), 0o644); err != nil {
		t.Fatal(err)
	}
	profile := core.NewHarnessProfile(core.HarnessKindKimi, home)
	profile.Environment = core.EnvironmentFromMap(map[string]string{
		"KIMI_API_KEY":             "profile-key",
		"KIMI_CODE_BASE_URL":       server.URL + "/coding/v1",
		"KIMI_CODE_CUSTOM_HEADERS": "X-Profile-Test: profile-header",
		"PATH":                     "/missing-kimi-runtime",
	})
	backend, err := backendForProfile(&profile)
	if err != nil {
		t.Fatal(err)
	}
	if backend == nil {
		t.Fatal("the Kimi profile built no backend")
	}
	models, err := backend.ListModels(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(models, []string{"kimi-k2.5"}) {
		t.Fatalf("models = %q", models)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(failures) != 0 {
		t.Fatal(strings.Join(failures, "\n"))
	}
	if _, err := os.Stat(filepath.Join(home, "credentials", "kimi-code.json")); err == nil {
		t.Fatal("building the backend wrote a Kimi credential file")
	}
}

func TestDisabledProfilesAreIneligibleForUtilityWork(t *testing.T) {
	config := core.DefaultConfig()
	profile := core.NewHarnessProfile(core.HarnessKindCodex, "/profiles/codex")
	profile.Enabled = false
	if config.Profiles == nil {
		config.Profiles = map[string]core.HarnessProfile{}
	}
	config.Profiles["codex"] = profile
	runtime := &UtilityLlmRuntime{}

	_, err := runtime.Resolve(context.Background(), &config)
	var unavailable *noUtilityModel
	if !errors.As(err, &unavailable) {
		t.Fatalf("error = %v", err)
	}
	if !strings.Contains(err.Error(), "no enabled utility model") {
		t.Fatalf("error = %v", err)
	}
}

// failoverBackend answers structured requests with a fixed reply, or fails
// them as a signed-out provider does.
type failoverBackend struct {
	reply *string
}

func (failoverBackend) ListModels(context.Context) ([]string, error) {
	panic("models were already resolved")
}

func (b failoverBackend) StreamChat(_ context.Context, request anvilclient.StreamChatRequest) (anvilclient.LlmResponse, error) {
	if request.Tools != nil {
		panic("a utility request offered tools")
	}
	if request.StructuredOutput == nil {
		panic("strict output")
	}
	if request.StructuredOutput.SchemaName != "utility_test" {
		panic("schema name = " + request.StructuredOutput.SchemaName)
	}
	if b.reply == nil {
		return nil, errors.New("HTTP 401 unauthorized")
	}
	return anvilclient.LlmResponseText{Text: *b.reply}, nil
}

func TestStructuredFailoverTriesTheNextModelAfterProviderAuthFailure(t *testing.T) {
	candidates := []UtilityCandidate{
		candidateFor("signed-out", core.HarnessKindCodex, utilityFamilyCodex, UtilityQuotaClassHealthy, 100),
		candidateFor("available", core.HarnessKindCodex, utilityFamilyCodex, UtilityQuotaClassHealthy, 100),
	}
	reply := `{"verdict":"yes"}`
	candidates[0].backend = failoverBackend{}
	candidates[1].backend = failoverBackend{reply: &reply}

	profile, model, output, err := InferStructuredWithFailover(context.Background(), candidates,
		"Return a structured verdict.", "May I proceed?", "utility_test", textSchema("verdict"))
	if err != nil {
		t.Fatal(err)
	}
	if profile != "available" || model != "test-model" {
		t.Fatalf("answered by %s %s", profile, model)
	}
	if verdict := utilityJSONField(output, "verdict"); verdict != "yes" {
		t.Fatalf("verdict = %v", verdict)
	}
}

func TestNewestModelUsesAliasThenNaturalVersion(t *testing.T) {
	if core.ModelVersionCmp("grok-next", "grok-10.2") <= 0 {
		t.Fatal("grok-next is not newer than grok-10.2")
	}
	if core.ModelVersionCmp("gpt-5.10-luna", "gpt-5.9-luna") <= 0 {
		t.Fatal("gpt-5.10-luna is not newer than gpt-5.9-luna")
	}
	catalog := []anvilclient.ModelMetadata{
		modelWithWindow("muse-spark-1.2", nil),
		modelWithWindow("muse-spark-1.3-contributor", nil),
		modelWithWindow("muse-spark-1.3", nil),
		modelWithWindow("muse-spark-1.4-image", nil),
	}
	newest := newestFamilyModel(utilityFamilyMuse, catalog)
	if newest == nil {
		t.Fatal("regular Muse Spark model")
	}
	if newest.ID != "muse-spark-1.3" {
		t.Fatalf("newest = %q", newest.ID)
	}
}

func candidateFor(profileID string, harness core.HarnessKind, family utilityFamily, quotaClass UtilityQuotaClass, quotaScore uint8) UtilityCandidate {
	return UtilityCandidate{
		ProfileID:  profileID,
		Harness:    harness,
		Model:      "test-model",
		QuotaClass: quotaClass,
		QuotaScore: quotaScore,
		PageBytes:  DefaultContextBytes,
		family:     family,
		backend:    anvilclient.NewCodexClientWithAuthPath("auth.json"),
	}
}

func TestUtilityOrderKeepsQuotaClassThenProviderPriorityAndAPILast(t *testing.T) {
	candidates := []UtilityCandidate{
		// A Codex profile pointed at DeepSeek ranks with DeepSeek, not with
		// the Codex harness it runs under.
		candidateFor("deepseek", core.HarnessKindCodex, utilityFamilyDeepSeek, UtilityQuotaClassHealthy, 99),
		candidateFor("muse", core.HarnessKindMuse, utilityFamilyMuse, UtilityQuotaClassHealthy, 20),
		candidateFor("codex", core.HarnessKindCodex, utilityFamilyCodex, UtilityQuotaClassHealthy, 20),
		candidateFor("grok-reserve", core.HarnessKindGrok, utilityFamilyGrok, UtilityQuotaClassReserve, 10),
		candidateFor("codex-api", core.HarnessKindCodex, utilityFamilyCodex, UtilityQuotaClassUsagePriced, 100),
	}
	slices.SortStableFunc(candidates, func(left, right UtilityCandidate) int { return candidateOrder(&left, &right) })
	var order []string
	for _, candidate := range candidates {
		order = append(order, candidate.ProfileID)
	}
	if !slices.Equal(order, []string{"codex", "muse", "deepseek", "grok-reserve", "codex-api"}) {
		t.Fatalf("order = %q", order)
	}
}

func TestAnInlineTokenDeepseekProfileServesUtilityInference(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("model_provider = \"deepseek\"\n[model_providers.deepseek]\n"+
		"base_url = \"https://api.deepseek.com/v1\"\nwire_api = \"responses\"\n"+
		"experimental_bearer_token = \"inline-key\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	if family, ok := profileUtilityFamily(&profile); !ok || family != utilityFamilyDeepSeek {
		t.Fatalf("family = %v, %v", family, ok)
	}
	backend, err := backendForProfile(&profile)
	if err != nil {
		t.Fatal(err)
	}
	if backend == nil {
		t.Fatal("the inline token builds a chat-completions backend")
	}
}

func modelWithWindow(id string, contextLength *uint32) anvilclient.ModelMetadata {
	metadata := anvilclient.ModelMetadataIDOnly(id)
	metadata.ContextLength = contextLength
	return metadata
}

func tokens(count uint32) *uint32 { return &count }

func TestPageBytesFollowTheSummarizerContextWindow(t *testing.T) {
	// Four bytes per token, half the window left for the prompt and the
	// response.
	kimi := modelWithWindow("k3", tokens(400_000))
	if got := pageBytesFor(core.HarnessKindKimi, &kimi); got != 800_000 {
		t.Fatalf("page bytes = %d", got)
	}
	huge := modelWithWindow("k3", tokens(2_000_000))
	if got := pageBytesFor(core.HarnessKindKimi, &huge); got != MaxPageBytes {
		t.Fatalf("a huge published window is still capped: %d", got)
	}
	// Codex publishes no window, and the GPT-5 family's is far larger than
	// the cap.
	codex := modelWithWindow("gpt-5.6-luna", nil)
	if got := pageBytesFor(core.HarnessKindCodex, &codex); got != MaxPageBytes {
		t.Fatalf("page bytes = %d", got)
	}
	// Any other backend that publishes nothing keeps the conservative
	// default.
	grok := modelWithWindow("grok-4.6", nil)
	if got := pageBytesFor(core.HarnessKindGrok, &grok); got != DefaultContextBytes {
		t.Fatalf("page bytes = %d", got)
	}
}

func TestBackendPageBytesTakeTheSmallestCandidate(t *testing.T) {
	candidate := func(profileID string, pageBytes int) UtilityCandidate {
		return UtilityCandidate{
			ProfileID:  profileID,
			Harness:    core.HarnessKindCodex,
			Model:      "gpt-5.6-luna",
			QuotaClass: UtilityQuotaClassHealthy,
			QuotaScore: 100,
			PageBytes:  pageBytes,
			family:     utilityFamilyCodex,
			backend:    anvilclient.NewCodexClientWithAuthPath("auth.json"),
		}
	}

	// Failover means any candidate may answer any request, so the smallest
	// window governs the page size.
	mixed := NewUtilityCompactionBackend(context.Background(), []UtilityCandidate{
		candidate("wide", MaxPageBytes),
		candidate("narrow", 300_000),
	})
	if got := mixed.PageBytes(); got != 300_000 {
		t.Fatalf("page bytes = %d", got)
	}

	// A window below the compaction floor would fail the whole compaction
	// before a request was sent; an oversize page is split instead.
	tiny := NewUtilityCompactionBackend(context.Background(), []UtilityCandidate{candidate("tiny", 8*1024)})
	if got := tiny.PageBytes(); got != MinContextBytes {
		t.Fatalf("page bytes = %d", got)
	}
}

func percent(value uint8) *uint8 { return &value }

func TestZeroQuotaIsExcludedAndAPIRanksLast(t *testing.T) {
	extra := client.APILabel
	report := client.ProfileQuota{
		ProfileID: "p",
		Harness:   core.HarnessKindCodex,
		Windows:   []client.QuotaWindow{},
		Extra:     &extra,
	}
	expect := func(class UtilityQuotaClass, score uint8, usable bool) {
		t.Helper()
		gotClass, gotScore, gotUsable := ClassifyQuota(&report)
		if gotUsable != usable || usable && (gotClass != class || gotScore != score) {
			t.Fatalf("classify = (%v, %d, %v), want (%v, %d, %v)", gotClass, gotScore, gotUsable, class, score, usable)
		}
	}
	expect(UtilityQuotaClassUsagePriced, 100, true)
	report.Extra = nil
	report.Windows = append(report.Windows, client.QuotaWindow{Label: "weekly", RemainingPercent: percent(0)})
	expect(0, 0, false)
	report.Windows[0].RemainingPercent = percent(10)
	expect(UtilityQuotaClassReserve, 10, true)
	report.Windows[0].RemainingPercent = percent(11)
	expect(UtilityQuotaClassHealthy, 11, true)
	report.Windows[0].RemainingPercent = nil
	expect(UtilityQuotaClassUnknown, 0, true)
	report.Windows[0].RemainingPercent = percent(0)
	failure := "quota refresh failed"
	report.Error = &failure
	expect(UtilityQuotaClassUnknown, 0, true)
}

// liveProfiles loads the configured profiles named by the environment for
// the ignored live tests, in a config holding only them.
func liveProfiles(t *testing.T, requested [][2]string) core.Config {
	t.Helper()
	loaded, err := core.ConfigLoad()
	if err != nil {
		t.Fatalf("load Mjolnir configuration: %v", err)
	}
	config := core.DefaultConfig()
	config.Profiles = map[string]core.HarnessProfile{}
	for _, entry := range requested {
		variable, kind := entry[0], core.HarnessKind(entry[1])
		profileID := os.Getenv(variable)
		if profileID == "" {
			t.Fatalf("set %s to a configured profile id", variable)
		}
		profile, ok := loaded.Profiles[profileID]
		if !ok {
			t.Fatalf("profile %q is not configured", profileID)
		}
		if profile.Kind != kind {
			t.Fatalf("profile %q is %s, want %s", profileID, profile.Kind, kind)
		}
		config.Profiles[profileID] = profile
	}
	return config
}

func skipUnlessIgnoredIncluded(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip(reason)
	}
}

// Exercises paid, authenticated provider paths. This is intentionally
// ignored: run it through `scripts/test-utility-llm-live.sh`.
func TestUtilityLlmLiveAllProfiles(t *testing.T) {
	skipUnlessIgnoredIncluded(t, "requires four real profiles, network access, and paid quota")
	requested := [][2]string{
		{"MJ_UTILITY_LIVE_CODEX_PROFILE", string(core.HarnessKindCodex)},
		{"MJ_UTILITY_LIVE_GROK_PROFILE", string(core.HarnessKindGrok)},
		{"MJ_UTILITY_LIVE_KIMI_PROFILE", string(core.HarnessKindKimi)},
		// DeepSeek is served by a Codex profile pointed at its API.
		{"MJ_UTILITY_LIVE_DEEPSEEK_PROFILE", string(core.HarnessKindCodex)},
	}
	config := liveProfiles(t, requested)
	ctx := context.Background()
	candidates, err := (&UtilityLlmRuntime{}).Resolve(ctx, &config)
	if err != nil {
		t.Fatalf("resolve all four utility profiles: %v", err)
	}
	if len(candidates) != 4 {
		t.Fatalf("each live profile must be usable: %d candidates", len(candidates))
	}
	for _, entry := range requested {
		profileID, kind := os.Getenv(entry[0]), core.HarnessKind(entry[1])
		if !slices.ContainsFunc(candidates, func(candidate UtilityCandidate) bool {
			return candidate.ProfileID == profileID && candidate.Harness == kind
		}) {
			t.Fatalf("missing utility candidate %q", profileID)
		}
	}
	var wait sync.WaitGroup
	results := make(chan error, len(candidates))
	for _, candidate := range candidates {
		wait.Add(1)
		go func() {
			defer wait.Done()
			backend := NewUtilityCompactionBackend(ctx, []UtilityCandidate{candidate})
			snapshot, err := backend.Compact(ctx, "Summarize this completed coding turn: the user asked for a live utility-model check and the implementation returned success. Preserve both facts.")
			if err != nil {
				results <- fmt.Errorf("live inference failed for %s: %w", candidate.ProfileID, err)
				return
			}
			if strings.TrimSpace(snapshot) == "" {
				results <- fmt.Errorf("%s returned an empty snapshot", candidate.ProfileID)
				return
			}
			fmt.Fprintf(os.Stderr, "utility live ok: profile=%s kind=%v model=%s quota=%v summary_bytes=%d\n",
				candidate.ProfileID, candidate.Harness, candidate.Model, candidate.QuotaClass, len(snapshot))
			results <- nil
		}()
	}
	wait.Wait()
	close(results)
	count := 0
	for err := range results {
		count++
		if err != nil {
			t.Error(err)
		}
	}
	if count != 4 {
		t.Fatalf("results = %d", count)
	}
}

// Exercises the native Muse backend and its Spark-family model selection.
// Set MJ_UTILITY_LIVE_MUSE_PROFILE to a configured Muse profile ID.
func TestUtilityLlmLiveMuse(t *testing.T) {
	skipUnlessIgnoredIncluded(t, "requires a real Muse profile, network access, and paid quota")
	config := liveProfiles(t, [][2]string{{"MJ_UTILITY_LIVE_MUSE_PROFILE", string(core.HarnessKindMuse)}})
	profileID := os.Getenv("MJ_UTILITY_LIVE_MUSE_PROFILE")
	ctx := context.Background()
	candidates, err := (&UtilityLlmRuntime{}).Resolve(ctx, &config)
	if err != nil {
		t.Fatalf("resolve the live Muse utility profile: %v", err)
	}
	if len(candidates) != 1 {
		t.Fatalf("candidates = %d", len(candidates))
	}
	candidate := candidates[0]
	if candidate.ProfileID != profileID || candidate.Harness != core.HarnessKindMuse {
		t.Fatalf("candidate = %v", candidate)
	}
	if !utilityFamilyMuse.matches(candidate.Model) || !strings.HasPrefix(candidate.Model, "muse-spark-") {
		t.Fatalf("model = %q", candidate.Model)
	}
	for _, excluded := range []string{"contributor", "image", "voice"} {
		if strings.Contains(candidate.Model, excluded) {
			t.Fatalf("model = %q", candidate.Model)
		}
	}
	backend := NewUtilityCompactionBackend(ctx, []UtilityCandidate{candidate})
	summary, err := backend.Compact(ctx, "Facts: the utility backend selected the newest regular Muse Spark model. Facts: the selected model returned a schema-valid state snapshot. Summarize these facts faithfully in the state_snapshot field.")
	if err != nil {
		t.Fatalf("Muse Spark utility inference: %v", err)
	}
	if strings.TrimSpace(summary) == "" || len(summary) > maxSummaryBytes {
		t.Fatalf("summary = %q", summary)
	}
	fmt.Fprintf(os.Stderr, "Muse utility live ok: model=%s, summary_bytes=%d\n", candidate.Model, len(summary))
}

// Exercises a DeepSeek-on-Codex profile as a utility model end to end: the
// DeepSeek family, its newest flash model, and one real inference. Set
// MJ_UTILITY_LIVE_DEEPSEEK_PROFILE to a configured Codex profile whose home
// names the DeepSeek provider.
func TestUtilityLlmLiveDeepseekCodex(t *testing.T) {
	skipUnlessIgnoredIncluded(t, "requires a real DeepSeek-on-Codex profile, network access, and paid quota")
	config := liveProfiles(t, [][2]string{{"MJ_UTILITY_LIVE_DEEPSEEK_PROFILE", string(core.HarnessKindCodex)}})
	profileID := os.Getenv("MJ_UTILITY_LIVE_DEEPSEEK_PROFILE")
	profile := config.Profiles[profileID]
	if family, ok := profileUtilityFamily(&profile); !ok || family != utilityFamilyDeepSeek {
		t.Fatalf("family = %v, %v", family, ok)
	}
	ctx := context.Background()
	candidates, err := (&UtilityLlmRuntime{}).Resolve(ctx, &config)
	if err != nil {
		t.Fatalf("resolve the live DeepSeek-on-Codex utility profile: %v", err)
	}
	if len(candidates) != 1 {
		t.Fatalf("candidates = %d", len(candidates))
	}
	candidate := candidates[0]
	if candidate.ProfileID != profileID || candidate.Harness != core.HarnessKindCodex ||
		candidate.QuotaClass != UtilityQuotaClassHealthy || !utilityFamilyDeepSeek.matches(candidate.Model) {
		t.Fatalf("candidate = %v", candidate)
	}
	backend := NewUtilityCompactionBackend(ctx, []UtilityCandidate{candidate})
	summary, err := backend.Compact(ctx, "Facts: the utility backend selected the newest DeepSeek flash model through a Codex profile. Facts: the selected model returned a schema-valid state snapshot. Summarize these facts faithfully in the state_snapshot field.")
	if err != nil {
		t.Fatalf("DeepSeek utility inference: %v", err)
	}
	if strings.TrimSpace(summary) == "" || len(summary) > maxSummaryBytes {
		t.Fatalf("summary = %q", summary)
	}
	fmt.Fprintf(os.Stderr, "DeepSeek utility live ok: model=%s, summary_bytes=%d\n", candidate.Model, len(summary))
}

// jsonEqual compares two values by their JSON encoding.
func jsonEqual(t *testing.T, left, right any) bool {
	t.Helper()
	a, err := json.Marshal(left)
	if err != nil {
		t.Fatal(err)
	}
	b, err := json.Marshal(right)
	if err != nil {
		t.Fatal(err)
	}
	return string(a) == string(b)
}
