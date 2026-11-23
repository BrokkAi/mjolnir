// Port of the tests in mj-tui/src/render_changes.rs.

package tui

import (
	"encoding/json"
	"fmt"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestIdleDashboardHasNoClockToAdvance(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.AcknowledgeRender()

	if dashboard.ClockChanged() {
		t.Fatal("an idle dashboard reported a clock change")
	}
}

func TestVisibleOperationClockAdvancesWithoutRendering(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.BeginSessionOperationAt(
		"session-1",
		SessionOperationKindLaunching,
		nil,
		max(epochSeconds(), 1)-1,
	)
	dashboard.AcknowledgeRender()
	if dashboard.ClockChanged() {
		t.Fatal("the clock changed right after the render")
	}

	dashboard.renderChangeSnapshot.clock.sessions[0].value = "0s"
	if !dashboard.ClockChanged() {
		t.Fatal("a changed operation clock was not noticed")
	}
}

func TestQuotaReadingCrossingItsResetChangesTheClock(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	now := epochSeconds()
	var quota client.ProfileQuota
	if err := json.Unmarshal([]byte(fmt.Sprintf(`{
		"profile_id": "claude",
		"harness": "claude",
		"refreshed_at_epoch_seconds": %d,
		"windows": [{
			"label": "Week",
			"duration_seconds": 604800,
			"remaining_percent": 50,
			"resets_at_epoch_seconds": %d
		}]
	}`, now-3600, now+1)), &quota); err != nil {
		t.Fatal(err)
	}
	config := dashboard.config.Mut()
	if config.Profiles == nil {
		config.Profiles = map[string]core.HarnessProfile{}
	}
	config.Profiles["claude"] = core.NewHarnessProfile(core.HarnessKindClaude, "/x")
	dashboard.quotas["claude"] = quota
	dashboard.AcknowledgeRender()
	before := dashboard.quotaClockSignature(now)
	after := dashboard.quotaClockSignature(now + 2)
	awaiting := func(clocks []DisplayedClock) (string, bool) {
		for _, clock := range clocks {
			if clock.key == "quota:awaiting-reading" {
				return clock.value, true
			}
		}
		return "", false
	}
	if value, ok := awaiting(before); !ok || value != "0/0" {
		t.Fatalf("before = %q (%v), want 0/0", value, ok)
	}
	if value, ok := awaiting(after); !ok || value != "1/0" {
		t.Fatalf("after = %q (%v), want 1/0", value, ok)
	}
}
