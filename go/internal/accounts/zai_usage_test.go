// Port of the tests in mj-accounts/src/zai_usage.rs.

package accounts

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func TestGoldenZaiUsageWindowsFollowTheProviderQuotaPayload(t *testing.T) {
	// The complete rows are captured from Z.ai's monitor endpoint
	// (`/api/monitor/usage/quota/limit`):
	// https://github.com/steipete/CodexBar/issues/2724
	// The last rows retain its shape while omitting optional and unusable
	// provider fields.
	payload := `{
		"code": 200,
		"msg": "Operation successful",
		"data": {
			"limits": [
				{
					"type": "CREDIT_LIMIT",
					"unit": 3,
					"number": 5,
					"usage": 2000,
					"currentValue": 71,
					"remaining": 1929,
					"percentage": 3,
					"nextResetTime": 1786073946574
				},
				{
					"type": "CREDIT_LIMIT",
					"unit": 6,
					"number": 1,
					"usage": 10000,
					"currentValue": 71,
					"remaining": 9929,
					"percentage": 1,
					"nextResetTime": 1786660486998
				},
				{
					"type": "TOKENS_LIMIT",
					"percentage": 79.5,
					"nextResetTime": 1786073946574
				},
				{
					"type": "CREDIT_LIMIT",
					"number": 1,
					"currentValue": 0,
					"remaining": 0
				},
				{
					"type": "TIME_LIMIT",
					"number": 5,
					"usage": 100,
					"currentValue": 10,
					"remaining": 90
				}
			],
			"level": "lite"
		},
		"success": true
	}`
	var decoded struct {
		Data struct {
			Limits []zaiLimit `json:"limits"`
		} `json:"data"`
	}
	if err := json.Unmarshal([]byte(payload), &decoded); err != nil {
		t.Fatalf("provider limit rows: %v", err)
	}
	unknown := func(value *int64) string {
		if value == nil {
			return "unknown"
		}
		return fmt.Sprint(*value)
	}
	rows := []string{}
	for _, limit := range decoded.Data.Limits {
		window := parseZaiLimit(limit)
		if window == nil {
			continue
		}
		rows = append(rows, fmt.Sprintf("%s remaining=%d used=%s limit=%s resets_at=%s",
			window.Label, window.RemainingPercent, unknown(window.Used), unknown(window.Limit), unknown(window.ResetsAt)))
	}
	core.AssertGolden(t, "testdata", "zai-usage-windows", strings.Join(rows, "\n"))
}

// Rust: #[ignore = "requires MJ_ZAI_TEST_KEY with a live Coding Plan key"].
func TestLiveCodingPlanQuotaHasInferenceWindows(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires MJ_ZAI_TEST_KEY with a live Coding Plan key")
	}
	key, ok := os.LookupEnv("MJ_ZAI_TEST_KEY")
	if !ok {
		t.Fatal("set MJ_ZAI_TEST_KEY to a Coding Plan API key")
	}
	windows, err := ZaiUsageQuery(context.Background(), "api.z.ai", key)
	if err != nil {
		t.Fatal(err)
	}
	if len(windows) == 0 {
		t.Fatal("no windows")
	}
	for _, window := range windows {
		if window.RemainingPercent > 100 {
			t.Fatalf("remaining %d", window.RemainingPercent)
		}
	}
}
