// Port of mj-tui/src/dashboard_sessions.rs tests (verdict_tests).

package tui

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: e569a20c5: unread output hid working sessions.
func TestUnreadNeverMasksServedPersonFacingAttention(t *testing.T) {
	dashboard := NewDashboardState(
		core.DefaultConfig(),
		core.StateState{Version: core.StateVersion},
		map[string]client.ProfileQuota{},
	)
	*dashboard.sessionDetails.Mut() = map[string]*SessionDetail{
		"session": {unreadAgentMessages: 1},
	}
	for _, check := range []struct {
		served   core.SessionAttention
		expected AttentionLevel
	}{
		{core.SessionAttentionNeedsInput, AttentionLevelWaiting},
		{core.SessionAttentionImportant, AttentionLevelImportant},
		{core.SessionAttentionWorking, AttentionLevelWorking},
		{core.SessionAttentionInactive, AttentionLevelInactive},
		{core.SessionAttentionUnread, AttentionLevelUnread},
		{core.SessionAttentionIdle, AttentionLevelIdle},
	} {
		served := core.NewSnapshotMap[string, core.SessionAttention]()
		served.Insert("session", check.served)
		dashboard.SetSessionAttention(served)
		if got := presentationAttention(dashboard, "session"); got != check.expected {
			t.Fatalf("served %s: attention = %v, want %v", check.served, got, check.expected)
		}
	}
}
