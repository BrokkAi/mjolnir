// Port of the tests in mj-accounts/src/quota_telemetry.rs.

package accounts

import (
	"math"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// telemetryRemaining is an optional `(remaining, reset)` reading.
type telemetryRemaining struct {
	remaining uint8
	reset     *int64
}

func telemetrySampleReport(profileID string, ts uint64, remaining5h, remainingWeek *telemetryRemaining) client.ProfileQuota {
	windows := []client.QuotaWindow{}
	if remaining5h != nil {
		windows = append(windows, client.QuotaWindow{
			Label:                "5-hour",
			RemainingPercent:     schedulerU8(remaining5h.remaining),
			DurationSeconds:      schedulerI64(5 * 3600),
			ResetsAtEpochSeconds: remaining5h.reset,
		})
	}
	if remainingWeek != nil {
		windows = append(windows, client.QuotaWindow{
			Label:                "Week",
			RemainingPercent:     schedulerU8(remainingWeek.remaining),
			DurationSeconds:      schedulerI64(7 * 86_400),
			ResetsAtEpochSeconds: remainingWeek.reset,
		})
	}
	return client.ProfileQuota{
		ProfileID:               profileID,
		Harness:                 core.HarnessKindClaude,
		MonthlyPriceUsd:         schedulerU32(200),
		Windows:                 windows,
		RefreshedAtEpochSeconds: ts,
	}
}

func TestCorruptedIntervalsDoNotProduceNegativeOrInflatedVelocity(t *testing.T) {
	now := int64(1_000_000)
	reset := now + 4*3600
	history := []client.ProfileQuota{
		// Valid step: 10% -> 16% (+6% in 30 min = 12 %/hr)
		telemetrySampleReport("p1", uint64(now-3600), nil, &telemetryRemaining{90, schedulerI64(reset)}),
		telemetrySampleReport("p1", uint64(now-1800), nil, &telemetryRemaining{84, schedulerI64(reset)}),
		// Corrupted top-up: used drops 16% -> 4% (remaining jumps 84% -> 96%)
		telemetrySampleReport("p1", uint64(now-1200), nil, &telemetryRemaining{96, schedulerI64(reset)}),
		// Corrupted shifted deadline (> 120s shift): reset jumps by +600s
		telemetrySampleReport("p1", uint64(now-600), nil, &telemetryRemaining{80, schedulerI64(reset + 600)}),
		// Valid step after shift: 20% -> 22% (+2% in 10 min = 12 %/hr)
		telemetrySampleReport("p1", uint64(now), nil, &telemetryRemaining{78, schedulerI64(reset + 600)}),
	}
	telemetry := ComputeWindowTelemetry("p1", "id1", history, now)
	wt, ok := telemetry[NewWindowKey("p1", "id1", WindowKindLong, nil)]
	if !ok {
		t.Fatal("long window telemetry")
	}
	// Sum of valid deltas: 6% + 2% = 8% over 0.5h + (1/6)h = 2/3h => 12.0 %/hr
	if math.Abs(wt.Trailing60mPctPerHour-12.0) >= 1e-6 {
		t.Fatalf("expected 12.0 %%/hr, got %v", wt.Trailing60mPctPerHour)
	}
	if wt.SecondToLastCleanTs == nil || *wt.SecondToLastCleanTs != now-600 {
		t.Fatalf("second to last clean ts %v", wt.SecondToLastCleanTs)
	}
}

func TestDrawAndReserveSerializesConcurrentDrawsReleasesOnDropAndDedupsRefresh(t *testing.T) {
	telemetryStore := NewQuotaTelemetryStore()
	now := int64(1_700_000_000)
	ranking := PowerRankingSnapshotBaseline()

	makeCandidate := func(id string, pct uint8) *ScheduledCandidate {
		return &ScheduledCandidate{
			ProfileID: id,
			Harness:   core.HarnessKindCodex,
			Model:     "gpt-5.4",
			Effort:    schedulerText("high"),
			Quota: CandidateQuota{
				Kind:            CandidateQuotaSubscription,
				MonthlyPriceUsd: schedulerU32(200),
				Windows: []client.QuotaWindow{{
					Label:                "weekly",
					RemainingPercent:     schedulerU8(pct),
					ResetsAtEpochSeconds: schedulerI64(now + 7*86400),
					DurationSeconds:      schedulerI64(7 * 86400),
				}},
				RefreshedAtEpochSeconds: uint64(now - 60),
			},
			CacheIdentity: schedulerText(id),
		}
	}
	subtle := schedulerText("subtle")

	// 1. Uncommitted reservation is released immediately on Drop.
	{
		candidates := []*ScheduledCandidate{makeCandidate("p1", 80)}
		reservation, err := DrawAndReserve(telemetryStore, &candidates, core.ExecutionPolicyUnconstrained, PlacementChild, subtle, nil, nil, ranking, now)
		if err != nil || reservation == nil {
			t.Fatalf("reservation: %v %v", reservation, err)
		}
		if got := len(telemetryStore.Snapshot().RecentPlacements["p1"]); got != 1 {
			t.Fatalf("placements %d", got)
		}
		reservation.Release()
	}
	if got := len(telemetryStore.Snapshot().RecentPlacements["p1"]); got != 0 {
		t.Fatal("dropped reservation must be released immediately")
	}

	// 2. Concurrent draws serialize atomically and spill to peer once p1 is
	//    saturated.
	type drawn struct {
		profile     string
		reservation *PlacementReservation
	}
	var barrier sync.WaitGroup
	barrier.Add(2)
	results := make([]drawn, 2)
	var done sync.WaitGroup
	for index := range 2 {
		done.Go(func() {
			candidates := []*ScheduledCandidate{makeCandidate("p1", 3), makeCandidate("p2", 80)}
			barrier.Done()
			barrier.Wait()
			reservation, err := DrawAndReserve(telemetryStore, &candidates, core.ExecutionPolicyUnconstrained, PlacementChild, subtle, nil, nil, PowerRankingSnapshotBaseline(), now)
			if err != nil || reservation == nil {
				t.Errorf("reservation: %v %v", reservation, err)
				return
			}
			results[index] = drawn{profile: candidates[0].ProfileID, reservation: reservation}
		})
	}
	done.Wait()
	if t.Failed() {
		t.FailNow()
	}
	// Commit one with a real session_id and verify refresh deduplicates it.
	// Rust takes the first result out of the vector and drops the rest,
	// which releases the other reservation.
	first := results[0]
	results[1].reservation.Release()
	first.reservation.Commit(schedulerText("sess-dedup-1"))
	if got := len(telemetryStore.Snapshot().RecentPlacements[first.profile]); got != 1 {
		t.Fatalf("placements %d", got)
	}

	// 3. ApplyStoreRefresh with the same session_id never double-counts.
	telemetryStore.ApplyStoreRefresh(map[WindowKey]WindowTelemetry{}, []store.RecentPlacementRow{{
		SessionID:             "sess-dedup-1",
		ProfileID:             first.profile,
		Category:              "subtle",
		CreatedAtEpochSeconds: now,
	}}, now-3600)
	if got := len(telemetryStore.Snapshot().RecentPlacements[first.profile]); got != 1 {
		t.Fatalf("committed_pending entry must be deduplicated by session_id on refresh: %d", got)
	}

	// A Move's destination reservation survives rows that still describe its
	// source, and rows with an older session creation timestamp.
	moved := NewQuotaTelemetryStore()
	candidates := []*ScheduledCandidate{makeCandidate("profile-b", 80)}
	reservation, err := DrawAndReserve(moved, &candidates, core.ExecutionPolicyUnconstrained, PlacementChild, schedulerText("primary"), nil, nil, ranking, now)
	if err != nil || reservation == nil {
		t.Fatalf("reservation: %v %v", reservation, err)
	}
	reservation.Commit(schedulerText("moved-session"))
	for _, row := range []struct {
		profileID string
		createdAt int64
	}{{"profile-a", now - 30}, {"profile-b", now - 3600}} {
		moved.ApplyStoreRefresh(map[WindowKey]WindowTelemetry{}, []store.RecentPlacementRow{{
			SessionID:             "moved-session",
			ProfileID:             row.profileID,
			Category:              "primary",
			CreatedAtEpochSeconds: row.createdAt,
		}}, now-7200)
		want := 1
		if row.profileID == "profile-b" {
			want = 2
		}
		if got := len(moved.Snapshot().RecentPlacements["profile-b"]); got != want {
			t.Fatalf("destination reservation must survive source or older destination history: %s %d", row.profileID, got)
		}
	}
	moved.ApplyStoreRefresh(map[WindowKey]WindowTelemetry{}, []store.RecentPlacementRow{{
		SessionID:             "moved-session",
		ProfileID:             "profile-b",
		Category:              "primary",
		CreatedAtEpochSeconds: now,
	}}, now-7200)
	if got := len(moved.Snapshot().RecentPlacements["profile-b"]); got != 1 {
		t.Fatalf("placements %d", got)
	}
}
