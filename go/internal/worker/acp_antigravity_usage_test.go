// Port of mj-worker/src/acp/antigravity_usage.rs: its tests module.

package worker

import (
	"encoding/json"
	"reflect"
	"slices"
	"strings"
	"testing"

	acpunstable "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// antigravityUsageRecorded is `usage` of a recorded Antigravity 1.5.11
// `session/prompt` response (a four-step Gemini 4 Neon turn verified against
// `transcript.jsonl`).
const antigravityUsageRecorded = `{"totalTokens":144764,"inputTokens":40053,"outputTokens":3500,
        "thoughtTokens":1830,"cachedReadTokens":101211,"cachedWriteTokens":0}`

func antigravityUsageParse(t *testing.T, report string) (core.TokenUsage, acpunstable.Meta) {
	t.Helper()
	var usage acpunstable.Usage
	if err := json.Unmarshal([]byte(report), &usage); err != nil {
		t.Fatalf("ACP usage: %v", err)
	}
	meta := usage.Meta.Clone()
	return core.TokenUsageFromACP(core.HarnessKindAntigravity, usage), meta
}

// Moving the pin must come with a fresh live check of the prompt response's
// usage scope; then add the new version to antigravityWholeTurnVersions.
func TestThePinnedAntigravityIsAWholeTurnVersion(t *testing.T) {
	if !slices.Contains(antigravityWholeTurnVersions[:], core.AntigravityVersion) {
		t.Fatalf("%s is not a whole-turn version", core.AntigravityVersion)
	}
	pinned, older := core.AntigravityVersion, "1.5.10"
	if !AntigravityUsageReportsWholeTurns(&pinned) {
		t.Fatal("the pinned version does not report whole turns")
	}
	if AntigravityUsageReportsWholeTurns(&older) || AntigravityUsageReportsWholeTurns(nil) {
		t.Fatal("an unchecked version reports whole turns")
	}
}

func TestTheRecordedAntigravityReportBecomesAPriceableTurnAndAMismatchStaysOut(t *testing.T) {
	usage, meta := antigravityUsageParse(t, antigravityUsageRecorded)
	if usage.Scope != core.UsageScopeUnspecified {
		t.Fatalf("scope %v", usage.Scope)
	}
	if err := AntigravityUsageNormalize(&usage, meta); err != nil {
		t.Fatalf("recorded report adds up: %v", err)
	}
	if usage.Scope != core.UsageScopeTurn {
		t.Fatalf("scope %v", usage.Scope)
	}
	if usage.InputTokens != 40_053 || usage.OutputTokens != 3_500 || usage.TotalTokens != 144_764 {
		t.Fatalf("counts %d %d %d", usage.InputTokens, usage.OutputTokens, usage.TotalTokens)
	}
	if usage.ThoughtTokens == nil || *usage.ThoughtTokens != 1_830 ||
		usage.CachedReadTokens == nil || *usage.CachedReadTokens != 101_211 ||
		usage.CachedWriteTokens == nil || *usage.CachedWriteTokens != 0 {
		t.Fatalf("details %v %v %v", usage.ThoughtTokens, usage.CachedReadTokens, usage.CachedWriteTokens)
	}

	totals := map[string]core.UsageCounterTotal{}
	for name, tokens := range map[string]uint64{
		"input_tokens":        usage.InputTokens,
		"output_tokens":       usage.OutputTokens,
		"total_tokens":        usage.TotalTokens,
		"cached_read_tokens":  101_211,
		"cached_write_tokens": 0,
	} {
		totals[name] = core.UsageCounterTotal{Tokens: tokens, ReportedTurns: 1}
	}
	// 40,053 * $3/M + 3,500 * $15/M + 101,211 * $0.15/M = $0.18784065
	model := "gemini-4-neon"
	estimate, err := core.Estimate(&model, totals)
	if err != nil {
		t.Fatalf("estimate: %v", err)
	}
	if estimate == nil || *estimate != 1_878_406_500 {
		t.Fatalf("estimate %v", estimate)
	}

	for _, bad := range []string{
		strings.Replace(antigravityUsageRecorded, "144764", "144765", 1),
		strings.Replace(antigravityUsageRecorded, "1830", "3501", 1),
	} {
		mismatched, meta := antigravityUsageParse(t, bad)
		before := mismatched
		if err := AntigravityUsageNormalize(&mismatched, meta); err == nil {
			t.Fatalf("a mismatched report normalized: %s", bad)
		}
		if !reflect.DeepEqual(mismatched, before) {
			t.Fatalf("a refused report changed: %+v", mismatched)
		}
		if mismatched.Scope != core.UsageScopeUnspecified {
			t.Fatalf("scope %v", mismatched.Scope)
		}
	}

	// An adapter that declares the report incomplete is believed, even when
	// its counters add up.
	declared := strings.Replace(antigravityUsageRecorded,
		`"cachedWriteTokens":0}`,
		`"cachedWriteTokens":0,"_meta":{"mjolnir.dev/usage-scope":"unspecified"}}`, 1)
	incomplete, meta := antigravityUsageParse(t, declared)
	before := incomplete
	if err := AntigravityUsageNormalize(&incomplete, meta); err != nil {
		t.Fatalf("declared scope is kept: %v", err)
	}
	if !reflect.DeepEqual(incomplete, before) || incomplete.Scope != core.UsageScopeUnspecified {
		t.Fatalf("declared report changed: %+v", incomplete)
	}
}
