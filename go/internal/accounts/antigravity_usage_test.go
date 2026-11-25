// Port of the tests in mj-accounts/src/antigravity_usage.rs.

package accounts

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// Recorded from agy 1.5.11 on a Pro plan: Gemini, third-party (unused, so
// its resets are the Unix epoch) and a separate Gemini 4 Argon group.
const (
	antigravityGeminiGroup       = 0
	antigravityThirdPartyGroup   = 1
	antigravityArgonGroup        = 2
	antigravityArgonOrchestrator = "gemini-pro-orchestrator-g1-pro"
)

func antigravityRecordedUsage(t *testing.T) any {
	t.Helper()
	recorded, err := os.ReadFile(filepath.Join("testdata", "tests", "fixtures", "antigravity-usage.json"))
	if err != nil {
		t.Fatal(err)
	}
	value, err := jsonvalue.Parse(recorded)
	if err != nil {
		t.Fatal(err)
	}
	return value
}

// antigravityGroups is the response's group array.
func antigravityGroups(t *testing.T, response any) []any {
	t.Helper()
	groups, ok := jvPointer(response, "command", "data", "groups")
	if !ok {
		t.Fatal("no groups")
	}
	return groups.([]any)
}

func antigravityExhaustGroup(t *testing.T, response any, group int) {
	t.Helper()
	buckets, _ := jvGet(antigravityGroups(t, response)[group], "buckets")
	for index, bucket := range buckets.([]any) {
		object := bucket.(jsonvalue.Object)
		object.Set("remaining_fraction", int64(0))
		buckets.([]any)[index] = object
	}
}

func antigravityParseValue(t *testing.T, response any) []client.QuotaWindow {
	t.Helper()
	encoded, err := jsonvalue.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	windows, err := antigravityUsageParse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	return windows
}

// antigravityClone is a deep copy of response.
func antigravityClone(t *testing.T, response any) any {
	t.Helper()
	encoded, err := jsonvalue.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	clone, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	return clone
}

func antigravityNow() int64 {
	at, _ := core.ParseChronoRFC3339("2026-11-20T08:00:00Z")
	return at.Unix()
}

func antigravityPoolWindows(windows []client.QuotaWindow, pool core.AntigravityQuotaPool) []client.QuotaWindow {
	scoped := []client.QuotaWindow{}
	for _, window := range windows {
		if found, ok := client.AntigravityPoolOfWindow(&window); ok && found == pool {
			scoped = append(scoped, window)
		}
	}
	return scoped
}

func antigravityScore(t *testing.T, windows []client.QuotaWindow, model string, now int64) *float64 {
	t.Helper()
	score, ok := QuotaScore(windows, model, now)
	if !ok {
		return nil
	}
	return &score
}

func TestRecordedUsageKeepsExhaustedThirdPartyQuotaOutOfGeminiSelection(t *testing.T) {
	response := antigravityRecordedUsage(t)
	antigravityExhaustGroup(t, response, antigravityThirdPartyGroup)
	windows := antigravityParseValue(t, response)
	now := antigravityNow()
	if score := antigravityScore(t, windows, "gemini-3.8-flash", now); score == nil || *score <= 0 {
		t.Fatalf("gemini-3.8-flash score = %v", score)
	}
	for _, model := range []string{"claude-opus-5-5", "claude-sonnet-5-5", "gpt-oss-120b-medium"} {
		if score := antigravityScore(t, windows, model, now); score == nil || *score != 0 {
			t.Fatalf("%s score = %v", model, score)
		}
	}
	if score := antigravityScore(t, windows, "unrelated-model", now); score != nil {
		t.Fatalf("unrelated-model score = %v", *score)
	}

	var geminiWeek *client.QuotaWindow
	for _, window := range antigravityPoolWindows(windows, core.AntigravityQuotaPoolGemini) {
		if window.Label == "Week" {
			geminiWeek = &window
			break
		}
	}
	if geminiWeek == nil {
		t.Fatal("no Gemini week")
	}
	partial := antigravityPoolWindows(windows, core.AntigravityQuotaPoolArgon)
	for index := range partial {
		partial[index].Resets = nil
		partial[index].ResetsAtEpochSeconds = nil
	}
	MergeResetWindows(&partial, windows)
	restored := antigravityPoolWindows(partial, core.AntigravityQuotaPoolArgon)
	if len(restored) != 1 {
		t.Fatalf("%+v", partial)
	}
	want := antigravityPoolWindows(windows, core.AntigravityQuotaPoolArgon)[0].ResetsAtEpochSeconds
	if !int64PtrEqual(restored[0].ResetsAtEpochSeconds, want) {
		t.Fatalf("restored reset %v, want %v", restored[0].ResetsAtEpochSeconds, want)
	}
	if int64PtrEqual(restored[0].ResetsAtEpochSeconds, geminiWeek.ResetsAtEpochSeconds) {
		t.Fatal("a same-named Gemini window must not supply the Argon reset")
	}

	// Missing data must not become exhausted or full quota, and a small
	// positive remainder must not trigger an exhausted-pool Move.
	firstBucket := func() jsonvalue.Object {
		buckets, _ := jvGet(antigravityGroups(t, response)[0], "buckets")
		return buckets.([]any)[0].(jsonvalue.Object)
	}
	bucket := firstBucket()
	bucket.Set("remaining_fraction", 0.00001)
	windows = antigravityParseValue(t, response)
	if windows[0].RemainingPercent == nil || *windows[0].RemainingPercent != 1 {
		t.Fatalf("remaining = %v", windows[0].RemainingPercent)
	}
	bucket = firstBucket()
	bucket.Delete("remaining_fraction")
	buckets, _ := jvGet(antigravityGroups(t, response)[0], "buckets")
	buckets.([]any)[0] = bucket
	encoded, err := jsonvalue.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := antigravityUsageParse(encoded); err == nil {
		t.Fatal("a bucket without its remaining fraction parsed")
	}
}

// Hard-won: 1791e5750: agy's separate Gemini 4 Argon bucket failed every Antigravity quota read.
func TestRecordedUsageScoresArgonAgainstItsOwnPoolWhenThePlanKeepsItApart(t *testing.T) {
	now := antigravityNow()
	windows := antigravityParseValue(t, antigravityRecordedUsage(t))
	argon := antigravityPoolWindows(windows, core.AntigravityQuotaPoolArgon)
	if len(argon) != 1 {
		t.Fatalf("%+v", windows)
	}
	if !argon[0].IsWeeklyOrLonger() {
		t.Fatalf("%+v", argon[0])
	}
	// An unused pool reports the Unix epoch, which is no reset at all.
	for _, window := range antigravityPoolWindows(windows, core.AntigravityQuotaPoolClaude) {
		if window.RemainingPercent == nil || *window.RemainingPercent != 100 {
			t.Fatalf("%+v", window)
		}
		if window.ResetsAtEpochSeconds != nil {
			t.Fatalf("%+v", window)
		}
	}

	// Measured on this plan: an Argon turn draws on the Argon bucket alone,
	// and the Argon orchestrator also implements with Gemini.
	geminiSpent := antigravityRecordedUsage(t)
	antigravityExhaustGroup(t, geminiSpent, antigravityGeminiGroup)
	windows = antigravityParseValue(t, geminiSpent)
	if score := antigravityScore(t, windows, "gemini-4-argon", now); score == nil || *score <= 0 {
		t.Fatalf("gemini-4-argon score = %v", score)
	}
	for _, model := range []string{"gemini-4-neon", antigravityArgonOrchestrator} {
		if score := antigravityScore(t, windows, model, now); score == nil || *score != 0 {
			t.Fatalf("%s score = %v", model, score)
		}
	}
	argonSpent := antigravityRecordedUsage(t)
	antigravityExhaustGroup(t, argonSpent, antigravityArgonGroup)
	windows = antigravityParseValue(t, argonSpent)
	if score := antigravityScore(t, windows, "gemini-4-neon", now); score == nil || *score <= 0 {
		t.Fatalf("gemini-4-neon score = %v", score)
	}
	for _, model := range []string{"gemini-4-argon", antigravityArgonOrchestrator} {
		if score := antigravityScore(t, windows, model, now); score == nil || *score != 0 {
			t.Fatalf("%s score = %v", model, score)
		}
	}

	// A plan without a separate Argon group counts Argon in Gemini's.
	sharedValue := antigravityClone(t, geminiSpent)
	groups := antigravityGroups(t, sharedValue)
	groups = append(groups[:antigravityArgonGroup:antigravityArgonGroup], groups[antigravityArgonGroup+1:]...)
	data, _ := jvPointer(sharedValue, "command", "data")
	dataObject := data.(jsonvalue.Object)
	dataObject.Set("groups", groups)
	shared := antigravityParseValue(t, sharedValue)
	if score := antigravityScore(t, shared, "gemini-4-argon", now); score == nil || *score != 0 {
		t.Fatalf("shared gemini-4-argon score = %v", score)
	}

	// A reading cached before Argon had its own pool carries Gemini's other
	// scope; it must refresh the same allowance, not return beside it.
	fresh := antigravityParseValue(t, geminiSpent)
	for index := range fresh {
		fresh[index].Resets = nil
		fresh[index].ResetsAtEpochSeconds = nil
	}
	MergeResetWindows(&fresh, shared)
	gemini := antigravityPoolWindows(fresh, core.AntigravityQuotaPoolGemini)
	if len(gemini) != 2 {
		t.Fatalf("%+v", fresh)
	}
	for _, window := range gemini {
		if window.ModelScope == nil || *window.ModelScope != "gemini,!argon" || window.ResetsAtEpochSeconds == nil {
			t.Fatalf("%+v", window)
		}
	}
	if score := antigravityScore(t, fresh, "gemini-4-argon", now); score == nil || *score <= 0 {
		t.Fatalf("fresh gemini-4-argon score = %v", score)
	}
}

// Rust: #[ignore = "downloads pinned agy and needs a copied
// MJ_ANTIGRAVITY_TEST_HOME login"].
func TestInstalledAntigravityUsageReadsEverySubscriptionPoolWithoutATurn(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("downloads pinned agy and needs a copied MJ_ANTIGRAVITY_TEST_HOME login")
	}
	home, ok := os.LookupEnv("MJ_ANTIGRAVITY_TEST_HOME")
	if !ok {
		t.Fatal("set a copied credential home")
	}
	cache := t.TempDir()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	windows, err := AntigravityUsageQuery(ctx, home, map[string]string{"XDG_CACHE_HOME": cache})
	if err != nil {
		t.Fatal(err)
	}
	for _, pool := range core.AntigravityQuotaPoolAll {
		scoped := antigravityPoolWindows(windows, pool)
		// Only plans that keep Argon apart report it, and only weekly.
		if pool == core.AntigravityQuotaPoolArgon && len(scoped) == 0 {
			continue
		}
		weekly, fiveHour, reported := false, false, true
		for _, window := range scoped {
			weekly = weekly || window.IsWeeklyOrLonger()
			fiveHour = fiveHour || (window.DurationSeconds != nil && *window.DurationSeconds == 18_000)
			reported = reported && window.RemainingPercent != nil
		}
		if !weekly {
			t.Fatalf("%v: %+v", pool, windows)
		}
		if fiveHour != (pool != core.AntigravityQuotaPoolArgon) {
			t.Fatalf("%v: %+v", pool, windows)
		}
		// An unused pool has no open window and so no reset.
		if !reported {
			t.Fatalf("%v: %+v", pool, windows)
		}
	}
}

func int64PtrEqual(left, right *int64) bool {
	if left == nil || right == nil {
		return left == right
	}
	return *left == *right
}
