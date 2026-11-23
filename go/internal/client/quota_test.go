// Port of the tests in mj-client/src/quota.rs.

package client

import (
	"encoding/json"
	"os"
	"strings"
	"testing"
)

func TestResetDisplayMatchesSharedBrowserCases(t *testing.T) {
	data, err := os.ReadFile("testdata/quota/reset_display_cases.json")
	if err != nil {
		t.Fatal(err)
	}
	var cases []struct {
		Name         string              `json:"name"`
		Now          uint64              `json:"now"`
		Reset        *int64              `json:"reset"`
		Fallback     *string             `json:"fallback"`
		Style        ResetCountdownStyle `json:"style"`
		BankedResets *uint64             `json:"banked_resets"`
		Expected     string              `json:"expected"`
	}
	if err := json.Unmarshal(data, &cases); err != nil {
		t.Fatal(err)
	}
	for _, c := range cases {
		if got := FormatQuotaReset(c.Now, c.Reset, c.Fallback, c.Style, c.BankedResets); got != c.Expected {
			t.Errorf("%s: got %q, want %q", c.Name, got, c.Expected)
		}
	}
}

func TestARefusedCheckReadsAsMinutesUntilTheNextCheckAndEndsWithIt(t *testing.T) {
	var quota ProfileQuota
	if err := json.Unmarshal([]byte(`{
		"profile_id": "claude", "harness": "claude", "windows": [],
		"extra": null, "error": null, "refreshed_at_epoch_seconds": 0,
		"rate_limited_until_epoch_seconds": 1900
	}`), &quota); err != nil {
		t.Fatal(err)
	}
	label := func(now uint64) string {
		text, ok := quota.RateLimitLabel(now)
		if !ok {
			return "<none>"
		}
		return text
	}
	if got := label(1000); got != "quota check refused · next in 15 min" {
		t.Errorf("at 1000: %q", got)
	}
	if got := label(1000 + 15*60 - 1); got != "quota check refused · next in 1 min" {
		t.Errorf("a second before: %q", got)
	}
	if got := label(1000 + 15*60); got != "<none>" {
		t.Errorf("at the check: %q", got)
	}
	// A short hold inside the regular interval: the label lasts until the
	// next check, not until the hold ends.
	until, next := uint64(1000+60), uint64(1000+10*60)
	quota.RateLimitedUntilEpochSeconds = &until
	quota.NextCheckAtEpochSeconds = &next
	if got := label(1000 + 61); got != "quota check refused · next in 9 min" {
		t.Errorf("short hold: %q", got)
	}
	if got := label(1000 + 10*60); got != "<none>" {
		t.Errorf("at the next check: %q", got)
	}
	quota.RateLimitedUntilEpochSeconds = nil
	if _, ok := quota.NextCheckAt(); ok {
		t.Error("no hold, but a next check")
	}
	encoded, err := json.Marshal(quota)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "rate_limited_until_epoch_seconds") {
		t.Errorf("an absent hold was written: %s", encoded)
	}
}

func TestOldQuotaReportsDecodeWithoutABankedBalance(t *testing.T) {
	var quota ProfileQuota
	if err := json.Unmarshal([]byte(`{
		"profile_id": "claude", "harness": "claude", "windows": [{
			"label": "Week", "remaining_percent": 75, "used": null,
			"limit": null, "resets": null, "resets_at_epoch_seconds": null
		}],
		"extra": null, "error": null, "refreshed_at_epoch_seconds": 0
	}`), &quota); err != nil {
		t.Fatal(err)
	}
	if quota.BankedResets != nil || quota.PaidCredits != nil {
		t.Errorf("banked %v, paid %v", quota.BankedResets, quota.PaidCredits)
	}
	if quota.Windows[0].DurationSeconds != nil || quota.Windows[0].ModelScope != nil {
		t.Errorf("window %+v", quota.Windows[0])
	}
	encoded, err := json.Marshal(quota)
	if err != nil {
		t.Fatal(err)
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(encoded, &fields); err != nil {
		t.Fatal(err)
	}
	if _, ok := fields["banked_resets"]; ok {
		t.Errorf("an absent balance was written: %s", encoded)
	}
}
