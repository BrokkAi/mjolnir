// Port of the tests in mj-accounts/src/profile_scheduler.rs.

package accounts

import (
	"fmt"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func schedulerCandidate(quota CandidateQuota, mayConsumeCredits bool) SubagentCandidate {
	return SubagentCandidate{
		ProfileID:         "p",
		Harness:           core.HarnessKindCodex,
		Choices:           core.ProfileConfig{ObservedAt: 1},
		Quota:             quota,
		MayConsumeCredits: mayConsumeCredits,
		Unhealthy:         map[string]Unhealthy{},
	}
}

func schedulerU8(value uint8) *uint8 { return &value }

func schedulerI64(value int64) *int64 { return &value }

func schedulerU32(value uint32) *uint32 { return &value }

func schedulerSubscription(remaining uint8, credits *client.PaidCredits) CandidateQuota {
	return CandidateQuota{
		Kind:        CandidateQuotaSubscription,
		Windows:     []client.QuotaWindow{{Label: "Week", RemainingPercent: schedulerU8(remaining)}},
		PaidCredits: credits,
	}
}

func TestLoadBalancingExcludesSubscriptionsWithoutQuotaUnlessCreditsAreAllowed(t *testing.T) {
	ask := func(quota CandidateQuota, option bool) *Ineligible {
		candidate := schedulerCandidate(quota, option)
		ineligible, ok := Ineligibility(&candidate, "m", 0)
		if !ok {
			return nil
		}
		return &ineligible
	}
	is := func(got *Ineligible, want Ineligible) bool { return got != nil && *got == want }
	credits := client.NewPaidCreditsLimited(5.0)
	if credits == nil {
		t.Fatal("finite credits")
	}
	for _, option := range []bool{false, true} {
		if got := ask(schedulerSubscription(0, nil), option); !is(got, IneligibleOutOfQuota) {
			t.Fatalf("exhausted, option %v: %v", option, got)
		}
		if got := ask(schedulerSubscription(50, nil), option); got != nil {
			t.Fatalf("half full, option %v: %v", option, *got)
		}
		if got := ask(CandidateQuota{Kind: CandidateQuotaUsagePriced}, option); got != nil {
			t.Fatalf("usage priced, option %v: %v", option, *got)
		}
		if got := ask(CandidateQuota{Kind: CandidateQuotaUnknown}, option); got != nil {
			t.Fatalf("unknown, option %v: %v", option, *got)
		}
	}
	if got := ask(schedulerSubscription(0, credits), false); !is(got, IneligibleCreditsNotAllowed) {
		t.Fatalf("credits not allowed: %v", got)
	}
	if got := ask(schedulerSubscription(0, credits), true); got != nil {
		t.Fatalf("credits allowed: %v", *got)
	}
}

func schedulerClaudeWindow(remaining uint8, resetsAt *int64, failure *string) client.ProfileQuota {
	return client.ProfileQuota{
		ProfileID:       "claude",
		Harness:         core.HarnessKindClaude,
		MonthlyPriceUsd: schedulerU32(200),
		Windows: []client.QuotaWindow{{
			Label:                "5-hour",
			RemainingPercent:     schedulerU8(remaining),
			DurationSeconds:      schedulerI64(5 * 60 * 60),
			ResetsAtEpochSeconds: resetsAt,
		}},
		Error:                   failure,
		RefreshedAtEpochSeconds: 1,
	}
}

func schedulerPublication(report, lastGood *client.ProfileQuota) *QuotaReportPublication {
	return &QuotaReportPublication{CacheIdentity: "claude-account", Report: report, LastGood: lastGood}
}

func schedulerText(value string) *string { return &value }

// A non-429 failure after a 0% Claude reading must stay exhausted.
// Treating the failure as unknown would schedule it ahead of paid and API.
func TestFailedProbeAfterExhaustedReadingStaysExhausted(t *testing.T) {
	now := int64(1_000_000)
	lastGood := schedulerClaudeWindow(0, schedulerI64(now+3_600), nil)
	failed := lastGood
	failed.Windows = nil
	failed.Error = schedulerText("connection reset by peer")
	failed.RefreshedAtEpochSeconds = 2
	quota := CandidateQuotaFromPublication(schedulerPublication(&failed, &lastGood), core.HarnessKindClaude, now)
	if quota.Kind != CandidateQuotaSubscription {
		t.Fatalf("%+v", quota)
	}
	candidate := schedulerCandidate(quota, false)
	candidate.Harness = core.HarnessKindClaude
	if got, ok := Ineligibility(&candidate, "claude-opus-5-5", now); !ok || got != IneligibleOutOfQuota {
		t.Fatalf("%v %v", got, ok)
	}
}

// Once the stale 0% window's reset has passed, it no longer blocks.
func TestExpiredLastGoodWindowStopsBlocking(t *testing.T) {
	resetAt := int64(1_000_000)
	lastGood := schedulerClaudeWindow(0, schedulerI64(resetAt), nil)
	failed := lastGood
	failed.Windows = nil
	failed.Error = schedulerText("timed out")
	quota := CandidateQuotaFromPublication(schedulerPublication(&failed, &lastGood), core.HarnessKindClaude, resetAt)
	if quota.Kind != CandidateQuotaSubscription {
		t.Fatal("an expired window is still the last good reading, scored as reset")
	}
	candidate := schedulerCandidate(quota, false)
	candidate.Harness = core.HarnessKindClaude
	if got, ok := Ineligibility(&candidate, "claude-opus-5-5", resetAt); ok {
		t.Fatalf("%v", got)
	}
}

// Usage-priced harnesses stay on the API tier when the probe never landed.
func TestUsagePricedHarnessWithoutAReportIsNotUnknown(t *testing.T) {
	if got := CandidateQuotaFromPublication(nil, core.HarnessKindDsh, 0); got.Kind != CandidateQuotaUsagePriced {
		t.Fatalf("%+v", got)
	}
	failed := schedulerClaudeWindow(0, nil, schedulerText("dns failure"))
	failed.Harness = core.HarnessKindDsh
	failed.ProfileID = "deepseek"
	failed.Windows = nil
	if got := CandidateQuotaFromPublication(schedulerPublication(&failed, nil), core.HarnessKindDsh, 0); got.Kind != CandidateQuotaUsagePriced {
		t.Fatalf("%+v", got)
	}
	claudeFailed := schedulerClaudeWindow(0, nil, schedulerText("dns failure"))
	claudeFailed.Windows = nil
	if got := CandidateQuotaFromPublication(schedulerPublication(&claudeFailed, nil), core.HarnessKindClaude, 0); got.Kind != CandidateQuotaUnknown {
		t.Fatalf("%+v", got)
	}
}

// Paid credits with no windows are still a last-good subscription reading.
func TestPaidCreditsWithoutWindowsStaySchedulable(t *testing.T) {
	lastGood := schedulerClaudeWindow(0, nil, nil)
	lastGood.Windows = nil
	lastGood.PaidCredits = client.NewPaidCreditsLimited(5.0)
	failed := lastGood
	failed.PaidCredits = nil
	failed.Error = schedulerText("connection reset by peer")
	quota := CandidateQuotaFromPublication(schedulerPublication(&failed, &lastGood), core.HarnessKindClaude, 0)
	if quota.Kind != CandidateQuotaSubscription || quota.PaidCredits == nil || len(quota.Windows) != 0 {
		t.Fatalf("%+v", quota)
	}
}

func TestUndatedExhaustedLastGoodExpiresAfterWindowDuration(t *testing.T) {
	lastGood := schedulerClaudeWindow(0, nil, nil)
	lastGood.Windows[0].DurationSeconds = schedulerI64(5 * 3600)
	lastGood.RefreshedAtEpochSeconds = 1_000
	failed := lastGood
	failed.Windows = nil
	failed.Error = schedulerText("connection reset by peer")
	record := schedulerPublication(&failed, &lastGood)
	// Still within the 5-hour window duration: keep the exhausted reading.
	if before := CandidateQuotaFromPublication(record, core.HarnessKindClaude, 1_000+5*3600-1); before.Kind != CandidateQuotaSubscription {
		t.Fatalf("%+v", before)
	}
	// Once the window duration elapses, undated last_good expires to
	// Unknown.
	if after := CandidateQuotaFromPublication(record, core.HarnessKindClaude, 1_000+5*3600); after.Kind != CandidateQuotaUnknown {
		t.Fatalf("%+v", after)
	}
}

func scheduledSub(profileID string, harness core.HarnessKind, model string, effort *string, monthlyPriceUsd *uint32, windows []client.QuotaWindow, paidCredits *client.PaidCredits, mayConsumeCredits bool) *ScheduledCandidate {
	return &ScheduledCandidate{
		ProfileID: profileID,
		Harness:   harness,
		Model:     model,
		Effort:    effort,
		Quota: CandidateQuota{
			Kind:                    CandidateQuotaSubscription,
			MonthlyPriceUsd:         monthlyPriceUsd,
			Windows:                 windows,
			PaidCredits:             paidCredits,
			RefreshedAtEpochSeconds: 1_000_000,
		},
		MayConsumeCredits: mayConsumeCredits,
		CacheIdentity:     schedulerText(fmt.Sprintf("id-%s", profileID)),
	}
}

func weekWindow(remaining uint8, resetsAt int64) client.QuotaWindow {
	return client.QuotaWindow{
		Label:                "Week",
		RemainingPercent:     schedulerU8(remaining),
		DurationSeconds:      schedulerI64(7 * 86_400),
		ResetsAtEpochSeconds: schedulerI64(resetsAt),
	}
}

func scheduledIDs(candidates []*ScheduledCandidate) []string {
	ids := []string{}
	for _, candidate := range candidates {
		ids = append(ids, candidate.ProfileID)
	}
	return ids
}

func TestPoolFirstDrawInvarianceDeduplicatesModelsSharingOnePool(t *testing.T) {
	now := int64(1_000_000)
	reset := now + 3*86_400
	telemetry := QuotaTelemetrySnapshot{}
	ctx := SchedulingContext{
		ExecutionPolicy: core.ExecutionPolicyUnconstrained,
		Placement:       PlacementChild,
		Category:        schedulerText("straightforward"),
		Telemetry:       &telemetry,
		PowerRanking:    PowerRankingSnapshotBaseline(),
		NowSeconds:      now,
	}
	candidates := []*ScheduledCandidate{
		scheduledSub("codex1", core.HarnessKindCodex, "gpt-6.1-sol", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(80, reset)}, nil, false),
		scheduledSub("codex1", core.HarnessKindCodex, "gpt-6.1-luna", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(80, reset)}, nil, false),
		scheduledSub("codex1", core.HarnessKindCodex, "gpt-5.4", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(80, reset)}, nil, false),
		scheduledSub("claude1", core.HarnessKindClaude, "claude-opus-5-5", schedulerText("xhigh"), schedulerU32(200), []client.QuotaWindow{weekWindow(80, reset)}, nil, false),
	}
	RankScheduledCandidates(&candidates, &ctx)
	// Three Codex models share one unscoped pool on `codex1`, so only 2
	// pools survive.
	ids := scheduledIDs(candidates)
	if len(ids) != 2 || !slices.Contains(ids, "codex1") || !slices.Contains(ids, "claude1") {
		t.Fatalf("%v", ids)
	}
}

func TestBurstPlacementsOnStaleReadingTripRunwayGateAndDivertToPeer(t *testing.T) {
	now := int64(1_000_000)
	reset := now + 3*86_400
	telemetry := QuotaTelemetrySnapshot{
		Windows:          map[WindowKey]WindowTelemetry{},
		RecentPlacements: map[string][]RecentPlacement{},
		CategoryStats:    map[string]CategoryBurnStats{},
	}
	// `codex-hot` has a stale reading (second_to_last_clean_ts = now - 600)
	// and 30 burst `subtle` placements since then.
	telemetry.Windows[NewWindowKey("codex-hot", "id-codex-hot", WindowKindLong, nil)] = WindowTelemetry{
		Trailing60mPctPerHour: 1.0,
		SecondToLastCleanTs:   schedulerI64(now - 600),
	}
	burst := []RecentPlacement{}
	for i := int64(0); i < 30; i++ {
		burst = append(burst, RecentPlacement{Category: "subtle", CreatedAtEpochSeconds: now - 500 + i})
	}
	telemetry.RecentPlacements["codex-hot"] = burst

	ctx := SchedulingContext{
		ExecutionPolicy: core.ExecutionPolicyUnconstrained,
		Placement:       PlacementChild,
		Category:        schedulerText("subtle"),
		Telemetry:       &telemetry,
		PowerRanking:    PowerRankingSnapshotBaseline(),
		NowSeconds:      now,
	}
	candidates := []*ScheduledCandidate{
		// 10% remaining is enough without reservations, but 30 * subtle
		// (~11.7%) trips the gate.
		scheduledSub("codex-hot", core.HarnessKindCodex, "gpt-6.1-sol", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(10, reset)}, nil, false),
		scheduledSub("codex-cool", core.HarnessKindCodex, "gpt-6.1-sol", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(10, reset)}, nil, false),
	}
	RankScheduledCandidates(&candidates, &ctx)
	if candidates[0].ProfileID != "codex-cool" || candidates[1].ProfileID != "codex-hot" {
		t.Fatalf("%v", scheduledIDs(candidates))
	}
}

func TestRawTargetGuardianFilterExcludesMjolnirGuardianHarnesses(t *testing.T) {
	now := int64(1_000_000)
	reset := now + 3*86_400
	telemetry := QuotaTelemetrySnapshot{}
	makeCandidates := func() []*ScheduledCandidate {
		return []*ScheduledCandidate{
			scheduledSub("kimi1", core.HarnessKindKimi, "kimi-k2.5", nil, schedulerU32(200), []client.QuotaWindow{weekWindow(90, reset)}, nil, false),
			scheduledSub("opencode1", core.HarnessKindOpenCode, "gpt-6.1-sol", nil, schedulerU32(200), []client.QuotaWindow{weekWindow(90, reset)}, nil, false),
			{
				ProfileID: "dsh1",
				Harness:   core.HarnessKindDsh,
				Model:     "deepseek-chat",
				Quota:     CandidateQuota{Kind: CandidateQuotaUsagePriced},
			},
			scheduledSub("claude1", core.HarnessKindClaude, "claude-opus-5-5", schedulerText("xhigh"), schedulerU32(200), []client.QuotaWindow{weekWindow(90, reset)}, nil, false),
		}
	}

	guarded := makeCandidates()
	guardedCtx := SchedulingContext{
		ExecutionPolicy: core.ExecutionPolicyConfiguredApprovals,
		Placement:       PlacementTopLevel,
		Category:        schedulerText("primary"),
		Telemetry:       &telemetry,
		PowerRanking:    PowerRankingSnapshotBaseline(),
		NowSeconds:      now,
	}
	RankScheduledCandidates(&guarded, &guardedCtx)
	if ids := scheduledIDs(guarded); !slices.Equal(ids, []string{"claude1"}) {
		t.Fatalf("%v", ids)
	}

	unconstrained := makeCandidates()
	unconstrainedCtx := guardedCtx
	unconstrainedCtx.ExecutionPolicy = core.ExecutionPolicyUnconstrained
	RankScheduledCandidates(&unconstrained, &unconstrainedCtx)
	if len(unconstrained) != 4 {
		t.Fatalf("%v", scheduledIDs(unconstrained))
	}
}

func TestPaidAndAPITiersOrderByTasksPer200AndCreditBalance(t *testing.T) {
	now := int64(1_000_000)
	float := func(value float64) *float64 { return &value }
	ranking := PowerRankingSnapshot{
		SchemaVersion: 1,
		Entries: []PowerRankingEntry{
			{
				Config:          "cheap/high",
				Model:           "cheap-model",
				Effort:          schedulerText("high"),
				TasksPer200Sub:  float(100.0),
				TasksPer200API:  float(80.0),
				PlanUsdPerMonth: float(200.0),
				Basis:           "test",
			},
			{
				Config:          "expensive/high",
				Model:           "expensive-model",
				Effort:          schedulerText("high"),
				TasksPer200Sub:  float(20.0),
				TasksPer200API:  float(10.0),
				PlanUsdPerMonth: float(200.0),
				Basis:           "test",
			},
		},
	}
	telemetry := QuotaTelemetrySnapshot{}
	ctx := SchedulingContext{
		ExecutionPolicy: core.ExecutionPolicyUnconstrained,
		Placement:       PlacementChild,
		Category:        schedulerText("straightforward"),
		Telemetry:       &telemetry,
		PowerRanking:    &ranking,
		NowSeconds:      now,
	}
	candidates := []*ScheduledCandidate{
		{
			ProfileID: "api-expensive",
			Harness:   core.HarnessKindDsh,
			Model:     "expensive-model",
			Effort:    schedulerText("high"),
			Quota:     CandidateQuota{Kind: CandidateQuotaUsagePriced},
		},
		{
			ProfileID: "api-cheap",
			Harness:   core.HarnessKindDsh,
			Model:     "cheap-model",
			Effort:    schedulerText("high"),
			Quota:     CandidateQuota{Kind: CandidateQuotaUsagePriced},
		},
		scheduledSub("paid-small", core.HarnessKindClaude, "expensive-model", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(0, now+3600)}, client.NewPaidCreditsLimited(5.0), true),
		scheduledSub("paid-large", core.HarnessKindClaude, "cheap-model", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(0, now+3600)}, client.NewPaidCreditsLimited(50.0), true),
	}
	RankScheduledCandidates(&candidates, &ctx)
	if ids := scheduledIDs(candidates); !slices.Equal(ids, []string{"paid-large", "paid-small", "api-cheap", "api-expensive"}) {
		t.Fatalf("%v", ids)
	}
}

func TestOrchestratorPlacementExcludesHarnessesWithoutDelegationTools(t *testing.T) {
	now := int64(1_700_000_000)
	telemetry := QuotaTelemetrySnapshot{}
	ctx := SchedulingContext{
		ExecutionPolicy: core.ExecutionPolicyUnconstrained,
		Placement:       PlacementOrchestrator,
		Category:        schedulerText("orchestrator"),
		Telemetry:       &telemetry,
		PowerRanking:    PowerRankingSnapshotBaseline(),
		NowSeconds:      now,
	}
	candidates := []*ScheduledCandidate{
		scheduledSub("kimi-1", core.HarnessKindKimi, "k2", nil, nil, []client.QuotaWindow{weekWindow(90, now+3600)}, nil, false),
		scheduledSub("grok-1", core.HarnessKindGrok, "grok-4", nil, nil, []client.QuotaWindow{weekWindow(90, now+3600)}, nil, false),
		scheduledSub("muse-1", core.HarnessKindMuse, "muse", nil, nil, []client.QuotaWindow{weekWindow(90, now+3600)}, nil, false),
		scheduledSub("codex-1", core.HarnessKindCodex, "gpt-5.4", schedulerText("high"), schedulerU32(200), []client.QuotaWindow{weekWindow(80, now+3600)}, nil, false),
	}
	RankScheduledCandidates(&candidates, &ctx)
	if ids := scheduledIDs(candidates); !slices.Equal(ids, []string{"codex-1"}) {
		t.Fatalf("%v", ids)
	}
}
