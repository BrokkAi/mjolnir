// Port of mj-worker/src/acp/opencode_usage.rs: its tests module.

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

// opencodeUsageRecorded is `usage` of a recorded OpenCode 2.2.6
// `session/prompt` response (a three-step turn with reasoning and cache
// reads), as it crosses ACP.
const opencodeUsageRecorded = `{"inputTokens":15434,"outputTokens":468,"totalTokens":43649,
        "thoughtTokens":99,"cachedReadTokens":27648}`

func opencodeUsageParse(t *testing.T, report string) (core.TokenUsage, acpunstable.Meta) {
	t.Helper()
	var usage acpunstable.Usage
	if err := json.Unmarshal([]byte(report), &usage); err != nil {
		t.Fatalf("ACP usage: %v", err)
	}
	meta := usage.Meta.Clone()
	return core.TokenUsageFromACP(core.HarnessKindOpenCode, usage), meta
}

// Moving the pin must come with a fresh live check of the prompt response's
// usage scope; then add the new version to wholeTurnVersions.
func TestThePinnedOpencodeIsAWholeTurnVersion(t *testing.T) {
	if !slices.Contains(wholeTurnVersions[:], core.OpencodeVersion) {
		t.Fatalf("%s is not a whole-turn version", core.OpencodeVersion)
	}
	pinned := core.OpencodeVersion
	older, untested := "2.2.1", "1.14.0"
	if !ReportsWholeTurns(&pinned) || !ReportsWholeTurns(&older) {
		t.Fatal("a whole-turn version does not report whole turns")
	}
	if ReportsWholeTurns(&untested) || ReportsWholeTurns(nil) {
		t.Fatal("an unchecked version reports whole turns")
	}
}

func TestTheRecordedOpencodeReportBecomesAPriceableTurnAndAMismatchStaysOut(t *testing.T) {
	usage, meta := opencodeUsageParse(t, opencodeUsageRecorded)
	if usage.Scope != core.UsageScopeUnspecified {
		t.Fatalf("scope %v", usage.Scope)
	}
	if err := OpencodeUsageNormalize(&usage, meta); err != nil {
		t.Fatalf("recorded report adds up: %v", err)
	}
	if usage.Scope != core.UsageScopeTurn {
		t.Fatalf("scope %v", usage.Scope)
	}
	if usage.InputTokens != 43_082 || usage.OutputTokens != 567 || usage.TotalTokens != 43_649 {
		t.Fatalf("counts %d %d %d", usage.InputTokens, usage.OutputTokens, usage.TotalTokens)
	}
	if *usage.ThoughtTokens != 99 || *usage.CachedReadTokens != 27_648 {
		t.Fatalf("details %d %d", *usage.ThoughtTokens, *usage.CachedReadTokens)
	}

	// The shape pricing accepts: full input and output, cache as a subset.
	totals := map[string]core.UsageCounterTotal{}
	for name, tokens := range map[string]uint64{
		"input_tokens":       usage.InputTokens,
		"output_tokens":      usage.OutputTokens,
		"total_tokens":       usage.TotalTokens,
		"cached_read_tokens": 27_648,
	} {
		totals[name] = core.UsageCounterTotal{Tokens: tokens, ReportedTurns: 1}
	}
	model := "deepseek-v4-flash"
	estimate, err := core.Estimate(&model, totals)
	if err != nil {
		t.Fatalf("estimate: %v", err)
	}
	if estimate == nil {
		t.Fatal("no estimate")
	}

	mismatched, meta := opencodeUsageParse(t, strings.Replace(opencodeUsageRecorded, "43649", "43650", 1))
	before := mismatched
	if err := OpencodeUsageNormalize(&mismatched, meta); err == nil {
		t.Fatal("a mismatched report normalized")
	}
	if !reflect.DeepEqual(mismatched, before) {
		t.Fatalf("a refused report changed: %+v", mismatched)
	}
	if mismatched.Scope != core.UsageScopeUnspecified {
		t.Fatalf("scope %v", mismatched.Scope)
	}

	// An adapter that declares the report incomplete is believed, even when
	// its counters add up.
	declared := strings.Replace(opencodeUsageRecorded,
		`"cachedReadTokens":27648}`,
		`"cachedReadTokens":27648,"_meta":{"mjolnir.dev/usage-scope":"unspecified"}}`, 1)
	incomplete, meta := opencodeUsageParse(t, declared)
	before = incomplete
	if err := OpencodeUsageNormalize(&incomplete, meta); err != nil {
		t.Fatalf("declared scope is kept: %v", err)
	}
	if !reflect.DeepEqual(incomplete, before) || incomplete.Scope != core.UsageScopeUnspecified {
		t.Fatalf("declared report changed: %+v", incomplete)
	}
}
