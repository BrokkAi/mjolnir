// Port of the tests in mj-tui/src/operator_away.rs.

package tui

import (
	"errors"
	"reflect"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: AFK review finding 3: coalesced off/on feeds stranded Returning forever.
func TestOperatorAwayFeedAndDismissalWaitForConfirmation(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	unicode := core.SymbolSetUnicode
	dashboard.config.Mut().Advanced.Symbols = &unicode
	dashboard.beginNoticeLog()
	firstAway := core.OperatorPresence{Away: true, Revision: 1}
	dashboard.SetOperatorPresence(firstAway)
	if dashboard.awayDialog() == nil {
		t.Fatal("the AFK dialog did not open")
	}
	if text := strings.Join(drawn(dashboard, 100, 32), "\n"); !strings.Contains(text, "Close this window") {
		t.Fatalf("the AFK dialog is not drawn:\n%s", text)
	}
	returnAction := DashboardActionSetOperatorAway{Away: false}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, returnAction) {
		t.Fatalf("Esc = %#v, want %#v", action, returnAction)
	}
	dashboard.SetOperatorPresence(firstAway)
	if dashboard.awayDialog() != nil {
		t.Fatal("the closed revision stays suppressed")
	}
	if action := dashboard.DispatchCommand(CommandIdGoAfk); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("GoAfk = %#v, want None", action)
	}
	dashboard.OperatorAwayRequestFinished(false, 1, nil)
	dashboard.SetOperatorPresence(firstAway)
	if dashboard.awayDialog() != nil {
		t.Fatal("acknowledgement cannot replay a stale feed")
	}

	// The off at revision 2 is coalesced away before this client sees it.
	nextAway := core.OperatorPresence{Away: true, Revision: 3}
	dashboard.SetOperatorPresence(nextAway)
	if dashboard.awayDialog() == nil {
		t.Fatal("a new AFK period reopens without an intervening off snapshot")
	}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, returnAction) {
		t.Fatalf("Esc = %#v, want %#v", action, returnAction)
	}
	dashboard.OperatorAwayRequestFinished(false, 1, errors.New("old request failed late"))
	if dashboard.awayDialog() != nil {
		t.Fatal("an earlier period's failure cannot reopen this returning period")
	}
	dashboard.SetOperatorPresence(firstAway)
	if revision := dashboard.OperatorPresenceRevision(); revision != 3 {
		t.Fatalf("revision = %d: older feed revisions cannot roll back presence", revision)
	}
	dashboard.OperatorAwayRequestFinished(false, 3, errors.New("connection lost"))
	if dashboard.awayDialog() == nil {
		t.Fatal("a failed off in the current revision reopens AFK")
	}

	lines := drawn(dashboard, 100, 32)
	at := point(lines, "×")
	for _, kind := range []tea.MouseMsg{
		tea.MouseClickMsg{Button: tea.MouseLeft},
		tea.MouseReleaseMsg{Button: tea.MouseLeft},
	} {
		action := dashboard.HandleMouse(mouseAt(kind, at))
		if _, release := kind.(tea.MouseReleaseMsg); release && !reflect.DeepEqual(action, returnAction) {
			t.Fatalf("release = %#v, want %#v", action, returnAction)
		}
	}
	dashboard.SetOperatorPresence(nextAway)
	if dashboard.awayDialog() != nil {
		t.Fatal("the returning revision reopened AFK")
	}
	dashboard.SetOperatorPresence(core.OperatorPresence{Away: false, Revision: 4})
	dashboard.OperatorAwayRequestFinished(false, 3, errors.New("failure after confirmation"))
	if dashboard.awayDialog() != nil {
		t.Fatal("an off feed confirms return despite a late failure")
	}
	if _, ok := dashboard.mode.(ModeNoticeLog); !ok {
		t.Fatalf("mode = %#v: preserve the underlying dialog", dashboard.mode)
	}
}
