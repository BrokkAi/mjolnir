// Port of mj-accounts/src/quota/tests.rs.

package accounts

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func quotaZaiProfile(t *testing.T, home, baseURL string) core.HarnessProfile {
	t.Helper()
	config := fmt.Sprintf("model_provider = \"zai\"\n"+
		"[model_providers.zai]\n"+
		"base_url = \"%s\"\n"+
		"env_key = \"ZAI_API_KEY\"\n"+
		"wire_api = \"responses\"\n", baseURL)
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(config), 0o666); err != nil {
		t.Fatal(err)
	}
	return core.HarnessProfile{
		ThirdPartyModels: true,
		Enabled:          true,
		Kind:             core.HarnessKindCodex,
		Home:             home,
		Environment:      core.EnvironmentFromMap(map[string]string{"ZAI_API_KEY": "coding-plan-key"}),
		MjolnirGuardian:  true,
	}
}

func TestAnInlineTokenProviderCredentialDoesNotBecomeNativeOpenai(t *testing.T) {
	home := t.TempDir()
	profile := quotaZaiProfile(t, home, "https://example.invalid/v1")
	path := filepath.Join(home, "config.toml")
	config, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	rewritten := strings.Replace(string(config), `env_key = "ZAI_API_KEY"`, `experimental_bearer_token = "inline-key"`, 1)
	if err := os.WriteFile(path, []byte(rewritten), 0o666); err != nil {
		t.Fatal(err)
	}
	profile.Environment.Clear()
	request := QuotaRefreshRequestForProfile("custom", &profile, home)
	provider, ok := request.CodexSource.(core.CodexQuotaSourceProvider)
	if !ok || provider.Value.APIKey != "inline-key" {
		t.Fatalf("the inline token authorizes the provider's own quota work, not Codex's OpenAI login: %#v", request.CodexSource)
	}
}

// Hard-won: Ollama and keyless custom Codex profiles showed the ChatGPT account's quota
func TestAProviderWithoutAReadableKeyNeverReadsTheChatgptQuota(t *testing.T) {
	home := t.TempDir()
	keyless := quotaZaiProfile(t, home, "https://openrouter.invalid/api/v1")
	keyless.Environment.Clear()
	ollamaHome := t.TempDir()
	if err := os.WriteFile(filepath.Join(ollamaHome, "config.toml"), []byte("model_provider = \"ollama\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	ollama := keyless
	ollama.Environment = keyless.Environment.Clone()
	ollama.Home = ollamaHome
	for _, profile := range []struct {
		id      string
		profile core.HarnessProfile
	}{{"keyless", keyless}, {"ollama", ollama}} {
		request := QuotaRefreshRequestForProfile(profile.id, &profile.profile, home)
		if _, ok := request.CodexSource.(core.CodexQuotaSourceUsagePriced); !ok {
			t.Fatalf("%s: %#v", profile.id, request.CodexSource)
		}
		beforeTheFix := request
		beforeTheFix.CodexSource = core.CodexQuotaSourceChatGptLogin{}
		if request.CacheIdentity() == beforeTheFix.CacheIdentity() {
			t.Fatalf("%s: a ChatGPT report stored for this profile must not be reused", profile.id)
		}
		// A ChatGPT probe would start a Codex app-server here and fail.
		outcome, _ := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
		if !outcome.Report.IsUsagePriced() {
			t.Fatalf("%s: %#v", profile.id, outcome.Report)
		}
	}
}

// Hard-won: 6f73fe58: usage-billed custom providers were incorrectly shown as unavailable
func TestAProviderWithoutAQuotaEndpointReportsUsagePricing(t *testing.T) {
	home := t.TempDir()
	profile := quotaZaiProfile(t, home, "https://example.invalid/v1")
	request := QuotaRefreshRequestForProfile("other", &profile, home)
	outcome, _ := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	if outcome.Report.Error != nil {
		t.Fatalf("error = %q", *outcome.Report.Error)
	}
	if len(outcome.Report.Windows) != 0 {
		t.Fatalf("windows = %#v", outcome.Report.Windows)
	}
	if !outcome.Report.IsUsagePriced() {
		t.Fatalf("not usage priced: %#v", outcome.Report)
	}
	if compact := outcome.Report.Compact(); compact != APILabel {
		t.Fatalf("compact = %q", compact)
	}
}

// Hard-won: bd326e2b: Bedrock Claude probed subscription quota despite using AWS credentials.
func TestBedrockClaudeReportsUsagePricingWithoutAnOauthLogin(t *testing.T) {
	home := t.TempDir()
	encoded, err := json.Marshal(map[string]any{
		"kind": "claude",
		"home": home,
		"environment": map[string]any{
			"CLAUDE_CODE_USE_BEDROCK": "1",
			"PATH":                    home,
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	var profile core.HarnessProfile
	if err := json.Unmarshal(encoded, &profile); err != nil {
		t.Fatal(err)
	}
	request := QuotaRefreshRequestForProfile("aws-model", &profile, home)
	outcome, codexClient := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)

	report := outcome.Report
	if report.ProfileID != "aws-model" || report.Harness != core.HarnessKindClaude {
		t.Fatalf("report = %#v", report)
	}
	if report.Error != nil {
		t.Fatalf("error = %q", *report.Error)
	}
	if len(report.Windows) != 0 || !report.IsUsagePriced() || report.Compact() != APILabel {
		t.Fatalf("report = %#v", report)
	}
	if outcome.CredentialsChanged {
		t.Fatal("credentials changed")
	}
	if codexClient != nil {
		t.Fatal("a Codex client for a Claude profile")
	}
}

// quotaAssertGolden compares rendered output with the checked-in
// testdata/golden/<name>.txt, which is a copy of the Rust crate's
// tests/golden file.
func quotaAssertGolden(t *testing.T, name, actual string) {
	t.Helper()
	path := filepath.Join("testdata", "golden", name+".txt")
	expected, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("could not read golden %s: %v", path, err)
	}
	if want, got := strings.TrimSuffix(string(expected), "\n"), strings.TrimSuffix(actual, "\n"); want != got {
		t.Fatalf("golden output differs at %s\nexpected:\n%s\nactual:\n%s", path, want, got)
	}
}

func quotaU8(value uint8) *uint8    { return &value }
func quotaI64(value int64) *int64   { return &value }
func quotaStr(value string) *string { return &value }

func TestGoldenQuotaCompactRow(t *testing.T) {
	window := func(label string, remaining uint8, resets *string, epoch *int64) client.QuotaWindow {
		return client.QuotaWindow{
			Label:                label,
			RemainingPercent:     quotaU8(remaining),
			Resets:               resets,
			ResetsAtEpochSeconds: epoch,
		}
	}
	report := func(profileID string, harness core.HarnessKind, windows []client.QuotaWindow, failure *string) client.ProfileQuota {
		return client.ProfileQuota{
			ProfileID: profileID,
			Harness:   harness,
			Windows:   windows,
			Error:     failure,
		}
	}
	row := func(report client.ProfileQuota) string {
		label, ok := report.ErrorLabel()
		if !ok {
			label = "none"
		}
		return fmt.Sprintf("%s | %s | error: %s", report.ProfileID, report.Compact(), label)
	}

	var out strings.Builder
	ordinary := report("codex-1", core.HarnessKindCodex,
		[]client.QuotaWindow{window("5H", 70, quotaStr("10:00 Jun 17"), quotaI64(14_400))}, nil)
	fmt.Fprintln(&out, "=== reset and healthy usage (1 profile row) ===")
	fmt.Fprintln(&out, row(ordinary))

	expired := report("claude2", core.HarnessKindClaude, []client.QuotaWindow{}, quotaStr(LoginExpired))
	fmt.Fprintln(&out, "=== expired login (1 profile row) ===")
	fmt.Fprintln(&out, row(expired))

	unavailable := report("claude2", core.HarnessKindClaude, []client.QuotaWindow{}, quotaStr("query Claude usage: HTTP 429"))
	fmt.Fprintln(&out, "=== other provider error (1 profile row) ===")
	fmt.Fprintln(&out, row(unavailable))

	sharedReset := report("codex-1", core.HarnessKindCodex, []client.QuotaWindow{
		window("5H", 70, quotaStr("10:00 Jun 17"), quotaI64(14_400)),
		window("Week", 55, quotaStr("10:00 Jun 17"), quotaI64(14_400)),
	}, nil)
	fmt.Fprintln(&out, "=== shared reset (1 profile row) ===")
	fmt.Fprintln(&out, row(sharedReset))

	exhausted := report("claude", core.HarnessKindClaude, []client.QuotaWindow{
		window("5H", 100, nil, nil),
		window("Week", 0, quotaStr("03:59 Aug 14"), nil),
	}, nil)
	fmt.Fprintln(&out, "=== exhausted weekly window (1 profile row) ===")
	fmt.Fprintln(&out, row(exhausted))

	fmt.Fprintln(&out, "=== weekly and monthly long windows (2 profile rows) ===")
	for _, label := range []string{"Week", "Month"} {
		monthly := report("grok", core.HarnessKindGrok, []client.QuotaWindow{window(label, 60, nil, nil)}, nil)
		fmt.Fprintf(&out, "%s | long-window: %s\n", row(monthly), monthly.WeeklyWindow().Label)
	}

	kimi := report("kimi", core.HarnessKindKimi, []client.QuotaWindow{
		{
			Label:                "Week",
			RemainingPercent:     quotaU8(94),
			Used:                 quotaI64(6),
			Limit:                quotaI64(100),
			Resets:               quotaStr("12:22 Aug 18"),
			ResetsAtEpochSeconds: quotaI64(604_800),
		},
		{
			Label:                "5H",
			RemainingPercent:     quotaU8(97),
			Used:                 quotaI64(3),
			Limit:                quotaI64(100),
			Resets:               quotaStr("10:22 Aug 13"),
			ResetsAtEpochSeconds: quotaI64(18_000),
		},
	}, nil)
	fmt.Fprintln(&out, "=== sustainable Kimi pace (1 profile row) ===")
	fmt.Fprintln(&out, row(kimi))

	burning := window("5H", 70, quotaStr("later"), quotaI64(14_400))
	sustainable := burning
	sustainable.RemainingPercent = quotaU8(80)
	fmt.Fprintln(&out, "=== short-window projection (2 window rows) ===")
	for _, sample := range []client.QuotaWindow{burning, sustainable} {
		compact := report("codex-1", core.HarnessKindCodex, []client.QuotaWindow{sample}, nil)
		fmt.Fprintf(&out, "%d%% remaining | exhausts before reset: %t | %s\n",
			*sample.RemainingPercent, ProjectsExhaustion(&sample, 0), compact.Compact())
	}

	quotaAssertGolden(t, "quota-compact-row", out.String())
}

func TestACodexRefreshMarginIsAnHourOrATenthOfTheTokenLife(t *testing.T) {
	hour := int64(3_600_000)
	now := int64(1_800_000_000_000)
	at := func(value int64) *int64 { return &value }
	// A short-lived token: the flat hour decides.
	if !codexLoginNeedsRefresh(at(now+hour/2), at(now-hour/2), now) {
		t.Fatal("a token half an hour from expiry is refreshed")
	}
	if codexLoginNeedsRefresh(at(now+2*hour), at(now-hour), now) {
		t.Fatal("a token two hours from expiry is not refreshed")
	}
	// A long-lived token: a tenth of its life is wider than the hour.
	if !codexLoginNeedsRefresh(at(now+3*hour), at(now-40*hour), now) {
		t.Fatal("a tenth of a long life is the margin")
	}
	// Without a last refresh, only the flat hour is known.
	if !codexLoginNeedsRefresh(at(now+hour/2), nil, now) {
		t.Fatal("the flat hour without a last refresh")
	}
	if codexLoginNeedsRefresh(at(now+3*hour), nil, now) {
		t.Fatal("three hours left without a last refresh")
	}
	// An unreadable expiry is not a reason to spend the refresh token.
	if codexLoginNeedsRefresh(nil, at(now-hour), now) {
		t.Fatal("an unreadable expiry")
	}
}

func TestMuseQuotaRefreshRecoversAndPopulatesDashboardWindows(t *testing.T) {
	directory := t.TempDir()
	credentials := []byte(`{"providers":{"meta":{"access_token":"profile-token"}}}`)
	if err := os.WriteFile(filepath.Join(directory, "auth.json"), credentials, 0o666); err != nil {
		t.Fatal(err)
	}
	var rejected atomic.Bool
	rejected.Store(true)
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/muse-code/key" || request.Method != http.MethodPost {
			http.NotFound(response, request)
			return
		}
		if got := request.Header.Get("Authorization"); got != "Bearer profile-token" {
			t.Errorf("authorization = %q", got)
		}
		body, _ := io.ReadAll(request.Body)
		if parsed, err := jsonvalue.Parse(body); err != nil || !jsonvalue.Equal(parsed, jsonvalue.Object{{Key: "onboard", Value: false}}) {
			t.Errorf("body = %s", body)
		}
		response.Header().Set("Content-Type", "application/json")
		if rejected.Load() {
			response.WriteHeader(http.StatusUnauthorized)
			io.WriteString(response, `{}`)
			return
		}
		io.WriteString(response, `{"api_key":"must-not-be-persisted","subs_usage":{"weekly":{"used_percent":1,"resets_at":1789344000},"window":{"used_percent":3,"window_duration_mins":300,"resets_at":1788890595}}}`)
	}))
	defer server.Close()
	request := QuotaRefreshRequest{
		ProfileID:   "muse",
		Harness:     core.HarnessKindMuse,
		AuthScheme:  core.AuthSchemeNativeLogin{},
		SourceHome:  directory,
		Environment: map[string]string{"TBH_MINT_BASE_URL": server.URL},
		Cwd:         directory,
		CodexSource: core.CodexQuotaSourceChatGptLogin{},
	}
	var manager QuotaManager
	manager.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, func(QuotaRefreshOutcome) {})
	if manager.Reports()["muse"].Error == nil {
		t.Fatal("the rejected login is an error")
	}
	rejected.Store(false)
	manager.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, func(outcome QuotaRefreshOutcome) {
		if outcome.CredentialsChanged {
			t.Error("credentials changed")
		}
	})
	report := manager.Reports()["muse"]
	if report.Error != nil {
		t.Fatalf("error = %q", *report.Error)
	}
	if report.Extra != nil {
		t.Fatalf("extra = %q", *report.Extra)
	}
	if weekly := report.WeeklyWindow(); weekly == nil || *weekly.RemainingPercent != 99 ||
		weekly.ResetsAtEpochSeconds == nil || *weekly.ResetsAtEpochSeconds != 1789344000 || weekly.Resets == nil {
		t.Fatalf("weekly = %#v", weekly)
	}
	if fiveHour := report.FiveHourWindow(); fiveHour == nil || *fiveHour.RemainingPercent != 97 {
		t.Fatalf("five hour = %#v", fiveHour)
	}
	if compact := report.Compact(); !strings.Contains(compact, "Week 99% left") {
		t.Fatalf("compact = %q", compact)
	}
	stored, err := os.ReadFile(filepath.Join(directory, "auth.json"))
	if err != nil || string(stored) != string(credentials) {
		t.Fatalf("auth.json = %s (%v)", stored, err)
	}
	manager.Shutdown(t.Context())
}

// TestAnIdleMuseProfileKeepsItsWeeklyFigureOnlyAcrossContinuousProbes: while
// no five-hour window is open, Meta's key endpoint answers without any
// usage, weekly included. The weekly figure is carried across that from the
// probes the daemon saved, but only while they leave no gap a window could
// open and close in unseen.
//
// Runs alone in a child process: the database writer is process-wide.
func TestAnIdleMuseProfileKeepsItsWeeklyFigureOnlyAcrossContinuousProbes(t *testing.T) {
	const child = "MJ_MUSE_IDLE_QUOTA_TEST_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		targets.NewIsolatedTest(targets.TestName("quota::tests", "TestAnIdleMuseProfileKeepsItsWeeklyFigureOnlyAcrossContinuousProbes")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	idleMuseQuotaCarriesTheWeekAcrossContinuousProbes(t)
}

func idleMuseQuotaCarriesTheWeekAcrossContinuousProbes(t *testing.T) {
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(`{"providers":{"meta":{"access_token":"profile-token"}}}`), 0o666); err != nil {
		t.Fatal(err)
	}
	var mints atomic.Int64
	// Meta's answer while idle, as recorded live on 2026-11-19 with the
	// account details replaced: no `subs_usage` key at all.
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/muse-code/key" {
			http.NotFound(response, request)
			return
		}
		mints.Add(1)
		response.Header().Set("Content-Type", "application/json")
		io.WriteString(response, `{"action_url":null,"api_key":"must-not-be-persisted","base_url":"https://api.meta.ai/v1","can_subscribe":false,"has_payment_method":false,"is_subs_active":true,"is_subs_upgrade_available":false,"payment_method":null,"require_payment":false,"show_subs_upsell":true,"subs_tier_id":"1","subs_tier_name":"Muse Code Everyday Usage","user_avatar_url":null,"user_email":"user@example.com","user_full_name":"Example User"}`)
	}))
	defer server.Close()
	// Profiles sharing one home share Meta's one answer, and each keeps its
	// own probe history.
	request := func(profileID string) QuotaRefreshRequest {
		return QuotaRefreshRequest{
			ProfileID:   profileID,
			Harness:     core.HarnessKindMuse,
			AuthScheme:  core.AuthSchemeNativeLogin{},
			SourceHome:  home,
			Environment: map[string]string{"TBH_MINT_BASE_URL": server.URL},
			Cwd:         home,
			CodexSource: core.CodexQuotaSourceChatGptLogin{},
		}
	}
	now := core.EpochSeconds()
	window := func(label string, remaining uint8, reset *int64) client.QuotaWindow {
		duration := int64(18_000)
		if label == "Week" {
			duration = 604_800
		}
		return client.QuotaWindow{
			Label:                label,
			RemainingPercent:     quotaU8(remaining),
			DurationSeconds:      &duration,
			Resets:               quotaFormatResetLocalSecondsOf(reset),
			ResetsAtEpochSeconds: reset,
		}
	}
	saved := func(profileID string, ago uint64, windows []client.QuotaWindow, extra *string) {
		t.Helper()
		report := client.ProfileQuota{
			ProfileID:               profileID,
			Harness:                 core.HarnessKindMuse,
			Windows:                 windows,
			Extra:                   extra,
			RefreshedAtEpochSeconds: now - ago,
		}
		identity := request(profileID)
		if err := store.SaveQuotaReport(identity.CacheIdentity(), &report); err != nil {
			t.Fatal(err)
		}
	}
	probe := func(profileID string) client.ProfileQuota {
		outcome, _ := refreshProfileWithCacheRoot(t.Context(), request(profileID), nil, nil)
		return outcome.Report
	}
	weekReset := int64(now) + 3*86_400
	week := window("Week", 78, &weekReset)

	// An hour after a reading with a window open: the week holds, and the
	// five-hour window is idle, so it is left out.
	saved("muse", 3_600, []client.QuotaWindow{week, window("5H", 94, quotaI64(int64(now)-600))}, nil)
	report := probe("muse")
	if report.Error != nil {
		t.Fatalf("error = %q", *report.Error)
	}
	if !reflect.DeepEqual(report.Windows, []client.QuotaWindow{week}) {
		t.Fatalf("windows = %#v", report.Windows)
	}
	if report.Extra != nil {
		t.Fatalf("extra = %q", *report.Extra)
	}
	if compact := report.Compact(); !strings.HasPrefix(compact, "Week 78% left") {
		t.Fatalf("compact = %q", compact)
	}
	// The idle probe is itself saved, so the next one carries from it.
	if windows := probe("muse").Windows; !reflect.DeepEqual(windows, []client.QuotaWindow{week}) {
		t.Fatalf("windows = %#v", windows)
	}

	// Probes served during a rate limit observed nothing, and the last one
	// that did is too old: a window could have come and gone since.
	saved("muse-held", 6*3_600, []client.QuotaWindow{week}, nil)
	saved("muse-held", 3_600, []client.QuotaWindow{week}, quotaStr("rate limited"))
	report = probe("muse-held")
	if report.Error != nil {
		t.Fatalf("error = %q", *report.Error)
	}
	if len(report.Windows) != 0 {
		t.Fatalf("windows = %#v", report.Windows)
	}
	if label, ok := report.WindowlessLabel(); !ok || label != IdleLabel {
		t.Fatalf("windowless label = %q, %t", label, ok)
	}
	if compact := report.Compact(); compact != "idle" {
		t.Fatalf("compact = %q", compact)
	}

	// Just past the five-hour window less the gate's cache age.
	saved("muse-gap", 4*3_600+55*60, []client.QuotaWindow{week}, nil)
	gap := probe("muse-gap")
	if label, ok := gap.WindowlessLabel(); !ok || label != IdleLabel {
		t.Fatalf("windowless label = %q, %t", label, ok)
	}

	// The week reset while nothing ran: it is full, next reset unknown.
	saved("muse-reset", 1_800, []client.QuotaWindow{window("Week", 60, quotaI64(int64(now)-600))}, nil)
	if windows := probe("muse-reset").Windows; !reflect.DeepEqual(windows, []client.QuotaWindow{window("Week", 100, nil)}) {
		t.Fatalf("windows = %#v", windows)
	}

	// Every profile was answered from Meta's one idle reply.
	if count := mints.Load(); count != 1 {
		t.Fatalf("mints = %d", count)
	}
}

// TestAProfileWhoseQuotaCannotBeReadSaysWhyInTheLogOnce: launch finding
// R12-2 (also R11): the Profiles pane showed the claude quota as
// "unavailable" for about eight minutes, and neither the daemon's nor the
// dashboard's log said why. A failed probe was stored as the report's error
// and logged nowhere; the pane shows only a short label for it. The reason
// is logged at info when a profile's quota stops being readable, once while
// it keeps failing the same way, and again when it can be read.
//
// Other quota tests reach the same log lines on their own goroutines, so
// this runs alone in a child process with the default logger capturing.
//
// Hard-won: e9fa6be0: quota failures appeared as unavailable without a diagnostic reason
func TestAProfileWhoseQuotaCannotBeReadSaysWhyInTheLogOnce(t *testing.T) {
	const child = "MJ_QUOTA_LOG_TEST_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		targets.NewIsolatedTest(targets.TestName("quota::tests", "TestAProfileWhoseQuotaCannotBeReadSaysWhyInTheLogOnce")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	log := core.NewCapturedLog()
	slog.SetDefault(slog.New(log))
	quotaFailureAndRecoveryAreLoggedOnce(t, log)
}

func quotaFailureAndRecoveryAreLoggedOnce(t *testing.T, log *core.CapturedLog) {
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	directory := t.TempDir()
	if err := os.WriteFile(filepath.Join(directory, "auth.json"), []byte(`{"providers":{"meta":{"access_token":"profile-token"}}}`), 0o666); err != nil {
		t.Fatal(err)
	}
	var status sync.Mutex
	statusCode := http.StatusInternalServerError
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		status.Lock()
		code := statusCode
		status.Unlock()
		response.Header().Set("Content-Type", "application/json")
		response.WriteHeader(code)
		if code == http.StatusOK {
			io.WriteString(response, `{"subs_usage":{"weekly":{"used_percent":1}}}`)
		} else {
			io.WriteString(response, `{}`)
		}
	}))
	defer server.Close()
	request := QuotaRefreshRequest{
		ProfileID:   "muse",
		Harness:     core.HarnessKindMuse,
		AuthScheme:  core.AuthSchemeNativeLogin{},
		SourceHome:  directory,
		Environment: map[string]string{"TBH_MINT_BASE_URL": server.URL},
		Cwd:         directory,
		CodexSource: core.CodexQuotaSourceChatGptLogin{},
	}
	// Lines logged at exactly info that contain text.
	said := func(text string) []string {
		var lines []string
		for _, line := range log.At(slog.LevelInfo) {
			if strings.Contains(line, text) {
				lines = append(lines, line)
			}
		}
		return lines
	}
	var manager QuotaManager
	ignore := func(QuotaRefreshOutcome) {}

	manager.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, ignore)
	failure := manager.Reports()["muse"].Error
	if failure == nil {
		t.Fatal("a failed probe")
	}
	unreadable := said("could not read the profile's quota")
	if len(unreadable) != 1 {
		t.Fatalf("%q", log.AtOrAbove(slog.LevelInfo))
	}
	if !strings.Contains(unreadable[0], "profile_id=muse") ||
		!strings.Contains(unreadable[0], "error="+strconv.Quote(*failure)) ||
		!strings.Contains(unreadable[0], `shown_as="unavailable"`) {
		t.Fatalf("%q", unreadable)
	}

	// Failing the same way again is not news.
	manager.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, ignore)
	if count := len(said("could not read the profile's quota")); count != 1 {
		t.Fatalf("logged %d times", count)
	}

	// Readable again, which is said once too.
	status.Lock()
	statusCode = http.StatusOK
	status.Unlock()
	manager.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, ignore)
	if report := manager.Reports()["muse"]; report.Error != nil {
		t.Fatalf("error = %q", *report.Error)
	}
	manager.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, ignore)
	readable := said("can be read again")
	if len(readable) != 1 {
		t.Fatalf("%q", log.AtOrAbove(slog.LevelInfo))
	}
	if !strings.Contains(readable[0], "profile_id=muse") {
		t.Fatalf("%q", readable)
	}
	if count := len(said("could not read the profile's quota")); count != 1 {
		t.Fatalf("logged %d times", count)
	}

	// Probe outcomes cross the HTTP reader and real writer lane. Repeated
	// failures and unchanged readings are retained, rather than
	// deduplicated.
	connection, err := sql.Open("sqlite", "file:"+store.DatabasePath()+"?mode=ro")
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	rows, err := connection.Query("SELECT body FROM quota_history WHERE profile_id='muse' ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	var reports []client.ProfileQuota
	for rows.Next() {
		var body string
		if err := rows.Scan(&body); err != nil {
			t.Fatal(err)
		}
		var report client.ProfileQuota
		if err := json.Unmarshal([]byte(body), &report); err != nil {
			t.Fatal(err)
		}
		reports = append(reports, report)
	}
	if err := rows.Close(); err != nil {
		t.Fatal(err)
	}
	if len(reports) != 4 {
		t.Fatalf("%d stored probes", len(reports))
	}
	for _, report := range reports[:2] {
		if report.Error == nil || *report.Error != *failure || len(report.Windows) != 0 {
			t.Fatalf("stored failure = %#v", report)
		}
	}
	for _, report := range reports[2:] {
		if report.Error != nil || *report.Windows[0].RemainingPercent != 99 {
			t.Fatalf("stored reading = %#v", report)
		}
	}
	if !reflect.DeepEqual(reports[2].Windows, reports[3].Windows) {
		t.Fatalf("%#v != %#v", reports[2].Windows, reports[3].Windows)
	}

	manager.Shutdown(t.Context())
}

func TestRetryAfterIsSecondsOrAnHTTPDateAndTheHoldIsBounded(t *testing.T) {
	now := time.Date(2015, 10, 21, 7, 26, 0, 0, time.UTC)
	for _, retry := range []struct {
		header string
		wait   time.Duration
		ok     bool
	}{
		{"120", 120 * time.Second, true},
		{"Wed, 21 Oct 2015 07:28:00 GMT", 120 * time.Second, true},
		{"Wed, 21 Oct 2015 07:20:00 GMT", 0, false},
		{"soon", 0, false},
	} {
		if wait, ok := core.ParseRetryAfter(retry.header, now); wait != retry.wait || ok != retry.ok {
			t.Fatalf("%q: %v, %t", retry.header, wait, ok)
		}
	}

	if hold := RateLimitHold(nil); hold != DefaultRateLimitHold {
		t.Fatalf("hold = %v", hold)
	}
	if DefaultRateLimitHold != 15*time.Minute {
		t.Fatalf("default hold = %v", DefaultRateLimitHold)
	}
	for _, bound := range []struct{ asked, held time.Duration }{
		{0, 60 * time.Second},
		{300 * time.Second, 300 * time.Second},
		{86_400 * time.Second, 6 * time.Hour},
	} {
		asked := bound.asked
		if hold := RateLimitHold(&asked); hold != bound.held {
			t.Fatalf("%v: hold = %v", bound.asked, hold)
		}
	}
}

func quotaReport(refreshedAt uint64, remaining *uint8) client.ProfileQuota {
	report := client.ProfileQuota{
		ProfileID:               "claude",
		Harness:                 core.HarnessKindClaude,
		Windows:                 []client.QuotaWindow{},
		RefreshedAtEpochSeconds: refreshedAt,
	}
	if remaining != nil {
		report.Windows = append(report.Windows, client.QuotaWindow{Label: "Week", RemainingPercent: remaining})
	} else {
		report.Error = quotaStr("unavailable")
	}
	return report
}

func TestARateLimitKeepsTheLastGoodReportAndItsAge(t *testing.T) {
	limited := quotaReport(500, nil)
	limited.Error = quotaStr("rate limited")
	limited.RateLimitedUntilEpochSeconds = func() *uint64 { value := uint64(1400); return &value }()

	// Nothing good to keep: the rate-limit report itself is published.
	if held := HoldReport(nil, limited); !reflect.DeepEqual(held, limited) {
		t.Fatalf("held = %#v", held)
	}
	failed := quotaReport(400, nil)
	if held := HoldReport(&failed, limited); !reflect.DeepEqual(held, limited) {
		t.Fatalf("held = %#v", held)
	}

	good := quotaReport(100, quotaU8(42))
	held := HoldReport(&good, limited)
	if !reflect.DeepEqual(held.Windows, good.Windows) {
		t.Fatalf("windows = %#v", held.Windows)
	}
	if held.RefreshedAtEpochSeconds != 100 {
		t.Fatalf("the age is the good reading's: %d", held.RefreshedAtEpochSeconds)
	}
	if held.Error != nil {
		t.Fatalf("error = %q", *held.Error)
	}
	if held.RateLimitedUntilEpochSeconds == nil || *held.RateLimitedUntilEpochSeconds != 1400 {
		t.Fatalf("hold = %v", held.RateLimitedUntilEpochSeconds)
	}
}

// macOS reads the quota from Claude Code itself and never opens the
// credentials file, so the expiry this asserts is not a state it can reach.
// Running it there would also spawn the real `claude` binary.
func TestExpiredClaudeCredentialsReportLoginExpired(t *testing.T) {
	if runtime.GOOS == "darwin" {
		t.Skip("macOS reads Claude's quota through the CLI")
	}
	directory := t.TempDir()
	credentials, err := jsonvalue.Marshal(jsonvalue.Object{{Key: "claudeAiOauth", Value: jsonvalue.Object{
		{Key: "accessToken", Value: "sk-ant-oat01-expired"},
		{Key: "expiresAt", Value: int64(1)},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, ".credentials.json"), credentials, 0o666); err != nil {
		t.Fatal(err)
	}
	outcome, _ := refreshProfileWithCacheRoot(t.Context(), QuotaRefreshRequest{
		ProfileID:   "claude2",
		Harness:     core.HarnessKindClaude,
		AuthScheme:  core.AuthSchemeNativeLogin{},
		SourceHome:  directory,
		Environment: map[string]string{},
		Cwd:         directory,
		CodexSource: core.CodexQuotaSourceChatGptLogin{},
	}, nil, nil)
	report := outcome.Report
	if len(report.Windows) != 0 {
		t.Fatalf("windows = %#v", report.Windows)
	}
	if report.Error == nil || *report.Error != LoginExpired {
		t.Fatalf("error = %v", report.Error)
	}
	if compact := report.Compact(); compact != LoginExpired {
		t.Fatalf("compact = %q", compact)
	}
}

func TestKimiQuotaRefreshesAfterUnauthorizedAndRetries(t *testing.T) {
	var refreshForms []string
	var formsMu sync.Mutex
	mux := http.NewServeMux()
	mux.HandleFunc("GET /coding/v1/usages", func(response http.ResponseWriter, request *http.Request) {
		response.Header().Set("Content-Type", "application/json")
		if request.Header.Get("Authorization") != "Bearer fresh-access" {
			response.WriteHeader(http.StatusUnauthorized)
			io.WriteString(response, `{}`)
			return
		}
		io.WriteString(response, `{
			"limits": [{
				"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
				"detail": {"limit": "100", "used": "25", "remaining": "75", "resetTime": "2026-09-22T12:22:15.593402Z"}
			}],
			"usages": {
				"limit_5h": {"used_ratio": 0.25, "reset_time": "2026-09-22T12:22:15Z"},
				"limit_7d": {"used_ratio": 0.1, "reset_time": "2026-09-29T12:22:15Z"},
				"limit_month_total": {"used_ratio": 0.0056, "reset_time": "2026-10-22T00:00:00Z"},
				"limit_month_code": {"used_ratio": 0.002, "reset_time": "2026-10-22T00:00:00Z"}
			},
			"boosterWallet": {"balance": {"type": "BOOSTER", "amount": "20000000000", "amountLeft": "10000000000"}}
		}`)
	})
	mux.HandleFunc("POST /api/oauth/token", func(response http.ResponseWriter, request *http.Request) {
		body, _ := io.ReadAll(request.Body)
		formsMu.Lock()
		refreshForms = append(refreshForms, string(body))
		formsMu.Unlock()
		response.Header().Set("Content-Type", "application/json")
		io.WriteString(response, `{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":900,"scope":"kimi-code","token_type":"Bearer"}`)
	})
	server := httptest.NewServer(mux)
	defer server.Close()

	environment := map[string]string{
		"KIMI_CODE_BASE_URL":   server.URL + "/coding/v1",
		"KIMI_CODE_OAUTH_HOST": server.URL,
	}
	home := t.TempDir()
	runtimeAuth, err := KimiRuntimeAuthResolve(home, environment)
	if err != nil {
		t.Fatal(err)
	}
	credentialsPath := runtimeAuth.CredentialsPath
	if err := os.MkdirAll(filepath.Dir(credentialsPath), 0o777); err != nil {
		t.Fatal(err)
	}
	futureExpiry := time.Now().Unix() + 3_600
	stored := fmt.Sprintf(`{"access_token":"rejected-access","refresh_token":"old-refresh","expires_at":%d,"scope":"kimi-code","token_type":"Bearer","expires_in":900}`, futureExpiry)
	if err := os.WriteFile(credentialsPath, []byte(stored), 0o666); err != nil {
		t.Fatal(err)
	}
	windows, _, err := queryKimi(t.Context(), home, environment)
	if err != nil {
		t.Fatal(err)
	}

	if windows[0].Label != "5H" || windows[0].Used == nil || *windows[0].Used != 25 || *windows[0].RemainingPercent != 75 {
		t.Fatalf("first window = %#v", windows[0])
	}
	type labelled struct {
		label     string
		remaining uint8
	}
	var got []labelled
	for _, window := range windows {
		got = append(got, labelled{window.Label, *window.RemainingPercent})
	}
	if want := []labelled{{"5H", 75}, {"Week", 90}, {"Month", 99}, {"Month code", 99}}; !reflect.DeepEqual(got, want) {
		t.Fatalf("windows = %v", got)
	}
	formsMu.Lock()
	forms := slices.Clone(refreshForms)
	formsMu.Unlock()
	if len(forms) != 1 {
		t.Fatalf("forms = %q", forms)
	}
	form, err := url.ParseQuery(forms[0])
	if err != nil {
		t.Fatal(err)
	}
	if form.Get("grant_type") != "refresh_token" || form.Get("refresh_token") != "old-refresh" {
		t.Fatalf("form = %v", form)
	}
	saved, err := readKimiCredentials(t.Context(), credentialsPath)
	if err != nil {
		t.Fatal(err)
	}
	if saved.AccessToken != "fresh-access" || saved.RefreshToken != "fresh-refresh" {
		t.Fatalf("saved = %#v", saved)
	}
	if _, err := os.Stat(filepath.Join(home, "oauth", "kimi-code.lock")); err == nil {
		t.Fatal("the refresh lock outlived the refresh")
	}
}

func TestKimiQuotaAcceptsAnAPIKeyWithoutOauthCredentials(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/coding/v1/usages" {
			http.NotFound(response, request)
			return
		}
		response.Header().Set("Content-Type", "application/json")
		if request.Header.Get("Authorization") != "Bearer profile-api-key" {
			response.WriteHeader(http.StatusUnauthorized)
			io.WriteString(response, `{}`)
			return
		}
		io.WriteString(response, `{"usages":{"limit_5h":{"used_ratio":0.4,"reset_time":"2026-09-22T12:22:15Z"}}}`)
	}))
	defer server.Close()

	home := t.TempDir()
	environment := map[string]string{
		"KIMI_API_KEY":       "profile-api-key",
		"KIMI_CODE_BASE_URL": server.URL + "/coding/v1",
	}
	windows, _, err := queryKimi(t.Context(), home, environment)
	if err != nil {
		t.Fatal(err)
	}
	if len(windows) != 1 || windows[0].Label != "5H" || *windows[0].RemainingPercent != 60 {
		t.Fatalf("windows = %#v", windows)
	}
}

func TestResetTimeNormalizationUses24HourMonthDayFormat(t *testing.T) {
	paris := time.FixedZone("", 2*3_600)
	reset := time.Date(2026, 6, 17, 16, 49, 0, 0, paris)
	if label := formatResetLabel(reset); label != "16:49 Jun 17" {
		t.Fatalf("label = %q", label)
	}
	if label := NormalizeResetText("Jun 17 at 4:49pm"); label == nil || *label != "16:49 Jun 17" {
		t.Fatalf("normalized = %v", label)
	}
}

func TestResetTimestampAcceptsSecondsAndMilliseconds(t *testing.T) {
	seconds := 1_781_712_540.0
	fromSeconds, fromMillis := FormatResetLocal(seconds), FormatResetLocal(seconds*1_000)
	if fromSeconds == nil || fromMillis == nil || *fromSeconds != *fromMillis {
		t.Fatalf("%v != %v", fromSeconds, fromMillis)
	}
	if fromInteger := FormatResetLocalSeconds(int64(seconds)); fromInteger == nil || *fromInteger != *fromSeconds {
		t.Fatalf("%v != %v", fromSeconds, fromInteger)
	}
}

func TestTimeOnlyResetIsRenderedAsTheNextDatetime(t *testing.T) {
	zone := time.FixedZone("", -5*3_600)
	now := time.Date(2026, 8, 10, 14, 0, 0, 0, zone)
	for _, reset := range []struct{ text, label string }{
		{"3:30 PM (America/Chicago)", "15:30 Aug 10"},
		{"at 1pm (America/Chicago)", "13:00 Aug 11"},
	} {
		at, ok := NormalizeResetAt(reset.text, now)
		if !ok || formatResetLabel(at) != reset.label {
			t.Fatalf("%q: %v, %t", reset.text, at, ok)
		}
	}
}

func TestClaudeCommaSeparatedResetIsNormalized(t *testing.T) {
	zone := time.FixedZone("", -5*3_600)
	now := time.Date(2026, 8, 11, 7, 0, 0, 0, zone)
	at, ok := NormalizeResetAt("Aug 14, 4am (America/Chicago)", now)
	if !ok || formatResetLabel(at) != "04:00 Aug 14" {
		t.Fatalf("%v, %t", at, ok)
	}
}

// TestOnlyTheProcessWithTheDatabaseWriterKeepsQuotaResetTimes: R9-1: only
// the daemon holds the database writer, and its own quota refreshes keep the
// reset-time cache. The dashboard refreshes quota for its own display and
// used to try the write anyway, logging "could not preserve quota reset
// times" at warning level on every refresh.
//
// This runs alone in a child process. The writer is process-wide, and the
// default logger is too.
//
// Hard-won: b1ba33fb: dashboard refresh wrote the quota cache without owning the database writer
func TestOnlyTheProcessWithTheDatabaseWriterKeepsQuotaResetTimes(t *testing.T) {
	const child = "MJ_QUOTA_RESET_CACHE_TEST_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		targets.NewIsolatedTest(targets.TestName("quota::tests", "TestOnlyTheProcessWithTheDatabaseWriterKeepsQuotaResetTimes")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	home := t.TempDir()
	// A provider without a quota endpoint gives a successful report without
	// any network, and a successful report is what the cache keeps.
	profile := quotaZaiProfile(t, home, "https://example.invalid/v1")
	request := QuotaRefreshRequestForProfile("other", &profile, home)
	identity := request.CacheIdentity()

	// The dashboard: no writer.
	log := core.NewCapturedLog()
	previous := slog.Default()
	slog.SetDefault(slog.New(log))
	outcome, _ := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	slog.SetDefault(previous)
	if outcome.Report.Error != nil {
		t.Fatalf("error = %q", *outcome.Report.Error)
	}
	if warnings := log.AtOrAbove(slog.LevelWarn); len(warnings) != 0 {
		t.Fatalf("no warning expected: %q", warnings)
	}
	visible := slices.ContainsFunc(log.Events(), func(event core.CapturedLogEvent) bool {
		return event.Level == slog.LevelDebug && strings.Contains(event.Text, "leaving quota history and reset times")
	})
	if !visible {
		t.Fatalf("the skipped write is still visible at debug level: %#v", log.Events())
	}

	// The daemon: it holds the writer, and its refresh keeps the reset
	// times.
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	if cached, err := store.LoadQuotaCache(identity); err != nil || cached != nil {
		t.Fatalf("cache before the daemon's refresh = %#v, %v", cached, err)
	}
	outcome, _ = refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	if outcome.Report.Error != nil {
		t.Fatalf("error = %q", *outcome.Report.Error)
	}
	cached, err := store.LoadQuotaCache(identity)
	if err != nil || cached == nil {
		t.Fatalf("cache = %#v, %v", cached, err)
	}
	if !reflect.DeepEqual(*cached, outcome.Report) {
		t.Fatalf("cache = %#v\nreport = %#v", *cached, outcome.Report)
	}
	connection, err := sql.Open("sqlite", "file:"+store.DatabasePath()+"?mode=ro")
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	var count int
	if err := connection.QueryRow("SELECT count(*) FROM quota_history").Scan(&count); err != nil {
		t.Fatal(err)
	}
	if count != 1 {
		t.Fatalf("%d stored probes", count)
	}
}

// writeOpencodeLogin writes an OpenCode credential database with one row,
// matching the shape the OpenCode login reader reads.
func writeOpencodeLogin(t *testing.T, home, integration, value string) {
	t.Helper()
	data := filepath.Join(home, ".data", "opencode")
	if err := os.MkdirAll(data, 0o777); err != nil {
		t.Fatal(err)
	}
	connection, err := sql.Open("sqlite", filepath.Join(data, "opencode.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := connection.Exec("CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL, " +
		"value text NOT NULL, connector_id text, method_id text, active integer, " +
		"time_created integer NOT NULL, time_updated integer NOT NULL)"); err != nil {
		t.Fatal(err)
	}
	if _, err := connection.Exec("INSERT INTO credential VALUES ('cred_1', ?1, 'login', ?2, NULL, NULL, 1, 1, 1)", integration, value); err != nil {
		t.Fatal(err)
	}
}

func TestOpencodeReportsAPIForAProfileSignedInOnlyWithAStoredAPIKey(t *testing.T) {
	home := t.TempDir()
	writeOpencodeLogin(t, home, "openrouter", `{"type":"key","key":"sk-1"}`)
	profile := core.NewHarnessProfile(core.HarnessKindOpenCode, home)
	profile.Enabled = true
	request := QuotaRefreshRequestForProfile("ds8bit", &profile, home)
	if !request.OpencodeUsagePriced {
		t.Fatal("a stored login that is itself a plain API key is still billed by usage")
	}
	outcome, _ := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	if outcome.Report.Extra == nil || *outcome.Report.Extra != APILabel {
		t.Fatalf("extra = %v", outcome.Report.Extra)
	}
}

func TestOpencodeStaysUnreportedWhenAStoredLoginIsOauth(t *testing.T) {
	home := t.TempDir()
	writeOpencodeLogin(t, home, "anthropic", `{"type":"oauth","refresh":"r","access":"a","expires":1}`)
	profile := core.NewHarnessProfile(core.HarnessKindOpenCode, home)
	profile.Enabled = true
	profile.Environment = core.EnvironmentFromMap(map[string]string{"OPENROUTER_API_KEY": "sk-env"})
	request := QuotaRefreshRequestForProfile("mixed", &profile, home)
	if request.OpencodeUsagePriced {
		t.Fatal("an OAuth login leaves billing unknown even beside an environment key for another provider")
	}
	outcome, _ := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	if outcome.Report.Extra == nil || *outcome.Report.Extra != NotReportedLabel {
		t.Fatalf("extra = %v", outcome.Report.Extra)
	}
}
