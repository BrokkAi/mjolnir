//go:build unix

// Port of the unix-only tests in mj-import/src/utility_llm.rs.

package mjimport

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// Hard-won: #1487: utility discovery could start a second provider quota checker on a cache miss.
func TestUtilityQuotaUsesOnlyFreshStoredReports(t *testing.T) {
	const child = "MJ_UTILITY_QUOTA_STORED_REPORT_TEST_CHILD"
	if os.Getenv(child) == "" {
		targets.NewIsolatedTest(targets.TestName("utility_llm::tests", "TestUtilityQuotaUsesOnlyFreshStoredReports")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}

	dataDir := os.Getenv("MJ_DATA_DIR")
	fakeBin := filepath.Join(dataDir, "fake-bin")
	if err := os.MkdirAll(fakeBin, 0o755); err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(dataDir, "quota-checker-started")
	executable := filepath.Join(fakeBin, "grok")
	if err := os.WriteFile(executable, []byte("#!/bin/sh\nprintf called >> \"$UTILITY_QUOTA_MARKER\"\nexit 0\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(executable, 0o755); err != nil {
		t.Fatal(err)
	}

	profile := core.NewHarnessProfile(core.HarnessKindGrok, filepath.Join(dataDir, "grok-profile"))
	profile.Environment = core.EnvironmentFromMap(map[string]string{
		"PATH":                 fakeBin,
		"UTILITY_QUOTA_MARKER": marker,
	})
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	runtime := &UtilityLlmRuntime{}
	profiles := []UtilityProfile{{ID: "grok", Profile: &profile}}
	markerExists := func() bool {
		_, err := os.Stat(marker)
		return err == nil
	}

	if reports := runtime.Quotas(profiles); len(reports) != 0 {
		t.Fatalf("a missing owner report is unknown: %+v", reports)
	}
	if markerExists() {
		t.Fatal("a cache miss must not start the configured provider checker")
	}

	resets := int64(1_900_000_000)
	stale := client.ProfileQuota{
		ProfileID: "grok",
		Harness:   core.HarnessKindGrok,
		Windows: []client.QuotaWindow{{
			Label:                "Week",
			RemainingPercent:     percent(50),
			ResetsAtEpochSeconds: &resets,
		}},
		RefreshedAtEpochSeconds: saturatingSub(nowSeconds(), quotaFreshSeconds+1),
	}
	request := quotaRequest("grok", &profile)
	identity := request.CacheIdentity()
	fresh := stale
	fresh.RefreshedAtEpochSeconds = nowSeconds()
	if err := store.SaveQuotaReport(identity, &fresh); err != nil {
		t.Fatal(err)
	}
	got, ok := runtime.Quotas(profiles)["grok"]
	if !ok || !jsonEqual(t, got, fresh) {
		t.Fatalf("a fresh owner report remains available to utility selection: %+v", got)
	}
	if markerExists() {
		t.Fatal("reading a fresh report must not start the configured provider checker")
	}

	if err := store.SaveQuotaReport(identity, &stale); err != nil {
		t.Fatal(err)
	}

	if reports := runtime.Quotas(profiles); len(reports) != 0 {
		t.Fatalf("a stale owner report is unknown: %+v", reports)
	}
	if markerExists() {
		t.Fatal("a stale report must not start the configured provider checker")
	}
}
