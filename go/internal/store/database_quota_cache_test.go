// Port of the tests in mj-store/src/database/quota_cache.rs.

package store

import (
	"encoding/json"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// quotaTestJSON is a report's encoding, which tests compare the way Rust
// compares the decoded reports.
func quotaTestJSON(t *testing.T, quota any) string {
	t.Helper()
	encoded, err := json.Marshal(quota)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func quotaTestClone(t *testing.T, report client.ProfileQuota) client.ProfileQuota {
	t.Helper()
	var clone client.ProfileQuota
	if err := json.Unmarshal([]byte(quotaTestJSON(t, report)), &clone); err != nil {
		t.Fatal(err)
	}
	return clone
}

func TestQuotaHistoryRetainsRawProbesWhileTheCachePreservesResetTimes(t *testing.T) {
	path := filepath.Join(t.TempDir(), "quota.sqlite3")
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	u32 := func(value uint32) *uint32 { return &value }
	u8 := func(value uint8) *uint8 { return &value }
	i64 := func(value int64) *int64 { return &value }
	text := func(value string) *string { return &value }
	report := client.ProfileQuota{
		MonthlyPriceUsd: u32(100),
		ProfileID:       "test",
		Harness:         core.HarnessKindClaude,
		Windows: []client.QuotaWindow{{
			Label:                "5H",
			RemainingPercent:     u8(0),
			ResetsAtEpochSeconds: i64(12345),
		}},
		RefreshedAtEpochSeconds: 1,
	}
	load := func() *client.ProfileQuota {
		t.Helper()
		cached, err := quotaCacheLoad(connection, "account-a")
		if err != nil {
			t.Fatal(err)
		}
		return cached
	}
	save := func(identity string) {
		t.Helper()
		if err := quotaCacheSaveReport(connection, identity, &report); err != nil {
			t.Fatal(err)
		}
	}
	observed := []client.ProfileQuota{quotaTestClone(t, report)}
	save("account-a")
	for _, banked := range []*uint64{func() *uint64 { v := uint64(2); return &v }(), func() *uint64 { v := uint64(0); return &v }(), func() *uint64 { v := uint64(1); return &v }(), nil} {
		report.BankedResets = banked
		report.PaidCredits = nil
		if banked != nil {
			report.PaidCredits = client.NewPaidCreditsLimited(float64(*banked))
		}
		save("account-a")
		observed = append(observed, quotaTestClone(t, report))
		cached := load()
		if !reflect.DeepEqual(cached.BankedResets, banked) {
			t.Fatalf("banked resets = %v, want %v", cached.BankedResets, banked)
		}
		if quotaTestJSON(t, cached.PaidCredits) != quotaTestJSON(t, report.PaidCredits) {
			t.Fatalf("paid credits = %s, want %s", quotaTestJSON(t, cached.PaidCredits), quotaTestJSON(t, report.PaidCredits))
		}
	}
	// A fresh, identical reading is evidence of no change, even within the
	// same second. A partial reading must not inherit historical data.
	save("account-a")
	observed = append(observed, quotaTestClone(t, report))
	report.RefreshedAtEpochSeconds += 600
	report.MonthlyPriceUsd = u32(200)
	report.Windows[0].ResetsAtEpochSeconds = nil
	report.Windows = append(report.Windows, client.QuotaWindow{
		Label:           "Weekly (model)",
		DurationSeconds: i64(7 * 24 * 3600),
		ModelScope:      text("model"),
		Used:            i64(12),
		Limit:           i64(50),
	})
	save("account-a")
	observed = append(observed, quotaTestClone(t, report))
	lastGood := load()
	if lastGood.MonthlyPriceUsd == nil || *lastGood.MonthlyPriceUsd != 200 {
		t.Fatalf("monthly price = %v", lastGood.MonthlyPriceUsd)
	}
	if reset := lastGood.Windows[0].ResetsAtEpochSeconds; reset == nil || *reset != 12345 {
		t.Fatalf("reset = %v", reset)
	}
	report.Error = text("rate limited")
	rateLimited := uint64(1500)
	report.RateLimitedUntilEpochSeconds = &rateLimited
	report.Windows = report.Windows[:0]
	save("account-a")
	observed = append(observed, quotaTestClone(t, report))
	if got := load(); quotaTestJSON(t, got) != quotaTestJSON(t, lastGood) {
		t.Fatalf("a failed probe replaced the cache: %s", quotaTestJSON(t, got))
	}
	report.Error = nil
	report.RateLimitedUntilEpochSeconds = nil
	save("account-a")
	observed = append(observed, quotaTestClone(t, report))
	// Different configurations under the same profile stay distinguishable.
	save("account-b")
	_ = connection.Close()
	connection, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if reset := load().Windows[0].ResetsAtEpochSeconds; reset == nil || *reset != 12345 {
		t.Fatalf("reopened reset = %v", reset)
	}
	rows, err := connection.Query("SELECT profile_id, harness, sampled_at_epoch_seconds, body FROM quota_history WHERE identity='account-a' ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	var decoded []client.ProfileQuota
	for rows.Next() {
		var profile, harness, body string
		var at uint64
		if err := rows.Scan(&profile, &harness, &at, &body); err != nil {
			t.Fatal(err)
		}
		var stored client.ProfileQuota
		if err := json.Unmarshal([]byte(body), &stored); err != nil {
			t.Fatal(err)
		}
		if profile != stored.ProfileID || harness != stored.Harness.ID() || at != stored.RefreshedAtEpochSeconds {
			t.Fatalf("history columns (%s, %s, %d) disagree with %s", profile, harness, at, body)
		}
		decoded = append(decoded, stored)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	rows.Close()
	if quotaTestJSON(t, decoded) != quotaTestJSON(t, observed) {
		t.Fatalf("history:\n got %s\nwant %s", quotaTestJSON(t, decoded), quotaTestJSON(t, observed))
	}

	// The history insert and cache upsert must either both commit or
	// neither.
	cached := load()
	if _, err := connection.Exec(`CREATE TRIGGER fail_quota_cache BEFORE INSERT ON quota_reset_cache
            BEGIN SELECT RAISE(ABORT, 'quota cache write failed'); END;`); err != nil {
		t.Fatal(err)
	}
	report.RefreshedAtEpochSeconds += 600
	if err := quotaCacheSaveReport(connection, "account-a", &report); err == nil {
		t.Fatal("the failing cache write must fail the save")
	}
	if got := load(); quotaTestJSON(t, got) != quotaTestJSON(t, cached) {
		t.Fatalf("the cache changed: %s", quotaTestJSON(t, got))
	}
	var count int
	if err := connection.QueryRow("SELECT count(*) FROM quota_history WHERE identity='account-a'").Scan(&count); err != nil {
		t.Fatal(err)
	}
	if count != len(observed) {
		t.Fatalf("%d history rows, want %d", count, len(observed))
	}
}
