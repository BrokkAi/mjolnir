// Port of mj-tui/src/notify.rs tests.

package tui

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func notifyQuestion(sessionID string) core.ElicitationRequest {
	params, err := json.Marshal(map[string]any{
		"mode":            "form",
		"sessionId":       sessionID,
		"message":         "Which branch should I use?",
		"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"b": map[string]any{"type": "string"}}},
	})
	if err != nil {
		panic(err)
	}
	request, err := core.ElicitationRequestFromACPParams(sessionID+"-question", params)
	if err != nil {
		panic(fmt.Sprintf("valid test question: %v", err))
	}
	return request
}

func notifyDashboard() *DashboardState {
	var sessions core.SnapshotMap[string, core.SessionRecord]
	for _, id := range []string{"asks", "done"} {
		session := runningSession()
		session.ID = id
		sessions.Insert(session.ID, session)
	}
	dashboard := NewDashboardState(config(), core.StateState{
		Version:  core.StateVersion,
		Sessions: sessions,
	}, map[string]client.ProfileQuota{})
	details := *dashboard.sessionDetails.Mut()
	details["asks"].pendingElicitations = []core.ElicitationRequest{notifyQuestion("asks")}
	done := details["done"]
	done.unreadAgentMessages = 1
	lastMessage := "All tests pass now.\nSecond line"
	done.lastAgentMessage = &lastMessage
	var attention core.SnapshotMap[string, core.SessionAttention]
	attention.Insert("asks", core.SessionAttentionNeedsInput)
	attention.Insert("done", core.SessionAttentionUnread)
	dashboard.SetSessionAttention(attention)
	dashboard.SetCurrentSession(nil)
	return dashboard
}

func notifyRequireTitle(t *testing.T, dashboard *DashboardState, want string) {
	t.Helper()
	if title, ok := dashboard.TerminalTitle(); !ok || title != want {
		t.Fatalf("terminal title = %q, %v; want %q", title, ok, want)
	}
}

func notifySessionIDs(notices []Notification) []string {
	ids := make([]string, 0, len(notices))
	for _, notice := range notices {
		ids = append(ids, notice.SessionID)
	}
	return ids
}

func TestUnmutedSubagentNotificationsFollowTheChildAtEitherDepth(t *testing.T) {
	dashboard, root := dashboardWithOneSubagent()
	child := "child-session"
	grandchild := "nested-child"
	state := *dashboard.state.Get()
	session, _ := state.Sessions.Get(child)
	session.ID = grandchild
	state.Sessions.Insert(grandchild, session)
	relation, _ := state.Subagents.Get(child)
	relation.ChildSessionID = grandchild
	relation.ParentSessionID = child
	state.Subagents.Insert(grandchild, relation)
	dashboard.SetState(state)
	publishFixtureVerdicts(dashboard)
	dashboard.config.Mut().Notify.Mode = core.NotifyModeSystem
	dashboard.config.Mut().Notify.DelaySeconds = 1
	dashboard.SetCurrentSession(nil)
	dashboard.SetAlertEvents(0, nil)
	serveAttention(dashboard, []string{root, child, grandchild}, core.SessionAttentionNeedsInput)
	dashboard.state.Mut().Sessions.GetMut(root).NotificationsIgnored = true
	for index, testCase := range []struct{ owner, muted, parent string }{
		{child, grandchild, root},
		{grandchild, child, child},
	} {
		owner, muted, parent := testCase.owner, testCase.muted, testCase.parent
		dashboard.state.Mut().Sessions.GetMut(owner).NotificationsIgnored = false
		dashboard.state.Mut().Sessions.GetMut(muted).NotificationsIgnored = true
		card := func(id string) client.NotificationCard {
			holder := id
			return client.NotificationCard{
				SinceMs: 1,
				Badge:   core.NotificationKindNeedsInput,
				Holder:  &holder,
				Form: &client.CardForm{
					Source:     client.CardFormSourceElicitation,
					RaisedAtMs: 1,
					RequestID:  id + "-approval",
					Message:    "Approve?",
				},
			}
		}
		var published core.SnapshotMap[string, client.NotificationCard]
		for _, id := range []string{owner, muted, root} {
			published.Insert(id, card(id))
		}
		for _, cards := range []*core.SnapshotMap[string, client.NotificationCard]{nil, &published} {
			dashboard.SetNotificationCards(cards)
			queue := dashboard.AttentionQueue()
			if len(queue) != 1 || queue[0].SessionID != owner {
				t.Fatalf("queue = %+v", queue)
			}
			if level, count, ok := dashboard.attentionBadgeSummary(); !ok || level != AttentionLevelWaiting || count != 1 {
				t.Fatalf("badge summary = %v, %d, %v", level, count, ok)
			}
			if level, count, ok := dashboard.workspaceAttentionSummary(queue[0].WorkspaceID); !ok || level != AttentionLevelWaiting || count != 1 {
				t.Fatalf("workspace summary = %v, %d, %v", level, count, ok)
			}
			notifyRequireTitle(t, dashboard, "mj · 1 waiting")
			entries := dashboard.notificationEntries()
			if len(entries) != 1 || entries[0].sessionID != owner {
				t.Fatalf("entries = %+v", entries)
			}
		}
		event := func(sequence uint64, id string) client.AlertEvent {
			current := card(id)
			identity, _ := current.ItemIdentity(id, 0)
			return client.AlertEvent{
				Sequence:     sequence,
				Destination:  id,
				Identity:     identity,
				Kind:         core.NotificationKindNeedsInput,
				SessionTitle: id,
				Text:         "Approve?",
			}
		}
		sequence := 2*uint64(index) + 1
		dashboard.SetAlertEvents(sequence+1, []client.AlertEvent{event(sequence, owner), event(sequence+1, muted)})
		notices := dashboard.notificationEventsAt(time.Now().Add(time.Second))
		if got := notifySessionIDs(notices); !slices.Equal(got, []string{owner}) {
			t.Fatalf("notices = %q", got)
		}

		if action := dashboard.stepAttention(1); action != (DashboardActionOpen{SessionID: owner}) {
			t.Fatalf("step attention = %#v", action)
		}
		if got, ok := dashboard.SubagentParentID(); !ok || got != parent {
			t.Fatalf("sub-agent parent = %q, %v", got, ok)
		}
		if got, ok := dashboard.SelectedSessionID(); !ok || got != owner {
			t.Fatalf("selected = %q, %v", got, ok)
		}
		if action := dashboard.DispatchCommand(CommandIdIgnoreNotifications); action != (DashboardActionSetSessionNotificationsIgnored{SessionID: owner, Ignored: true}) {
			t.Fatalf("ignore = %#v", action)
		}
		dashboard.state.Mut().Sessions.GetMut(owner).NotificationsIgnored = true
		if action := dashboard.DispatchCommand(CommandIdIgnoreNotifications); action != (DashboardActionSetSessionNotificationsIgnored{SessionID: owner, Ignored: false}) {
			t.Fatalf("unignore = %#v", action)
		}
	}
}

func TestTheTitleNamesAnUnreachableWorkerApartFromQuestions(t *testing.T) {
	dashboard := notifyDashboard()
	notifyRequireTitle(t, dashboard, "mj · 1 waiting · 1 unread")
	dashboard.state.Mut().Sessions.GetMut("done").State = core.SessionStateDisconnected
	serveAttention(dashboard, []string{"done"}, core.SessionAttentionUnreachable)

	notifyRequireTitle(t, dashboard, "mj · 1 unreachable · 1 waiting")
	configured := *dashboard.config.Get()
	configured.Notify.Title = false
	dashboard.SetConfig(configured)
	if title, ok := dashboard.TerminalTitle(); ok {
		t.Fatalf("terminal title = %q", title)
	}
}

// Hard-won: 88460860c553: relay stderr leaked into the footer and the unreachable notice stayed after recovery.
func TestAnUnreachableWorkerIsReportedPlainlyAndWithdrawnOnRecovery(t *testing.T) {
	dashboard := notifyDashboard()
	record, _ := dashboard.state.Get().Sessions.Get("done")
	title := record.ListedTitle()
	dashboard.ReportSessionUnreachable("done")
	if notice, _ := dashboard.Notice(); notice != fmt.Sprintf("Session %s cannot be reached. The log has the worker's details.", title) {
		t.Fatalf("notice = %q", notice)
	}
	dashboard.ReportSessionUnreachable("done")
	if notice, _ := dashboard.Notice(); strings.Contains(notice, "stderr") {
		t.Fatalf("notice = %q", notice)
	}
	dashboard.ReportSessionReachable("done")
	if notice, _ := dashboard.Notice(); notice != fmt.Sprintf("Session %s is reachable again.", title) {
		t.Fatalf("notice = %q", notice)
	}
	// A later, unrelated notice is left alone by another recovery.
	dashboard.ReportSessionUnreachable("done")
	dashboard.SetNotice("Profile quotas refreshed.")
	dashboard.ReportSessionReachable("done")
	if notice, _ := dashboard.Notice(); notice != "Profile quotas refreshed." {
		t.Fatalf("notice = %q", notice)
	}
}

// Hard-won: #1444: repeated worker polls restored a dismissed unreachable warning.
func TestUnreachableEpisodeStaysDismissedUntilRecoveryAndNewFailure(t *testing.T) {
	dashboard := notifyDashboard()
	dashboard.ReportSessionUnreachable("done")
	firstDeadline := time.Now().Add(chat.NoticeMinimumDisplay)
	first, _ := dashboard.Notice()
	time.Sleep(time.Millisecond)
	dashboard.ReportSessionUnreachable("done")
	if !dashboard.notices.Dismiss(firstDeadline) {
		t.Fatal("the notice is dismissed after its minimum display")
	}
	dashboard.ReportSessionUnreachable("done")
	if notice, ok := dashboard.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}
	if history := dashboard.notices.History(); len(history) != 1 {
		t.Fatalf("history = %+v", history)
	}

	dashboard.ReportSessionReachable("done")
	if notice, ok := dashboard.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}
	dashboard.ReportSessionUnreachable("done")
	if notice, ok := dashboard.Notice(); !ok || notice != first {
		t.Fatalf("notice = %q, %v", notice, ok)
	}
	dashboard.SetNotice("The daemon found a missing target.")
	dashboard.ReportSessionUnreachable("done")
	if notice, ok := dashboard.Notice(); !ok || notice != "The daemon found a missing target." {
		t.Fatalf("notice = %q, %v", notice, ok)
	}
	withoutSession := *dashboard.state.Get()
	withoutSession.Sessions.Remove("done")
	dashboard.SetState(withoutSession)
	if _, kept := dashboard.unreachableNotices["done"]; kept {
		t.Fatal("the unreachable episode is forgotten with the session")
	}
	if notice, ok := dashboard.Notice(); !ok || notice != "The daemon found a missing target." {
		t.Fatalf("notice = %q, %v", notice, ok)
	}
}

// Hard-won: 751b46ebb: alert consumption ignored the configured delay and item disappearance.
func TestAlertCursorSuppressesHistoryDuplicatesAndVisibleDestinations(t *testing.T) {
	dashboard := notifyDashboard()
	dashboard.config.Mut().Notify.DelaySeconds = 0
	var cards core.SnapshotMap[string, client.NotificationCard]
	for _, id := range []string{"asks", "done"} {
		cards.Insert(id, client.NotificationCard{
			SinceMs: 0,
			Badge:   core.NotificationKindNeedsInput,
			Form: &client.CardForm{
				Source:     client.CardFormSourceElicitation,
				RaisedAtMs: 1,
				RequestID:  "elicitation-1",
				Message:    "Question?",
			},
		})
	}
	dashboard.notificationCards = &cards
	event := func(sequence uint64, destination string) client.AlertEvent {
		identity, err := json.Marshal([]any{"form", destination, "elicitation-1", 1})
		if err != nil {
			t.Fatal(err)
		}
		return client.AlertEvent{
			Sequence:     sequence,
			Identity:     string(identity),
			Destination:  destination,
			SessionTitle: "Title",
			Text:         "Question?",
			Kind:         core.NotificationKindNeedsInput,
		}
	}
	asks, done := "asks", "done"
	dashboard.SetAlertEvents(7, []client.AlertEvent{event(7, asks)})
	if len(dashboard.NotificationEvents()) != 0 {
		t.Fatal("history on attachment is suppressed")
	}
	dashboard.SetAlertEvents(8, []client.AlertEvent{event(8, asks)})
	notices := dashboard.NotificationEvents()
	if len(notices) != 1 || notices[0].SessionTitle != "Title" || notices[0].Body != "Question?" {
		t.Fatalf("notices = %+v", notices)
	}
	dashboard.SetAlertEvents(8, []client.AlertEvent{event(8, asks)})
	if len(dashboard.NotificationEvents()) != 0 {
		t.Fatal("a duplicate is suppressed")
	}
	dashboard.SetCurrentSession(&asks)
	dashboard.SetAlertEvents(9, []client.AlertEvent{event(9, asks)})
	if len(dashboard.NotificationEvents()) != 0 {
		t.Fatal("a visible destination is suppressed")
	}
	dashboard.SetCurrentSession(nil)
	dashboard.SetAlertEvents(9, []client.AlertEvent{event(9, asks)})
	if len(dashboard.NotificationEvents()) != 0 {
		t.Fatal("an already seen event is suppressed")
	}
	dashboard.SetAlertEvents(11, []client.AlertEvent{event(10, asks), event(11, done)})
	if got := len(dashboard.NotificationEvents()); got != 2 {
		t.Fatalf("notices = %d", got)
	}
	dashboard.SetAlertEvents(12, []client.AlertEvent{event(12, asks)})
	dashboard.SetCurrentSession(&asks)
	if len(dashboard.NotificationEvents()) != 0 {
		t.Fatal("opening the destination before emission suppresses the ring")
	}
	dashboard.SetCurrentSession(nil)
	dashboard.SetAlertEvents(13, []client.AlertEvent{event(13, done)})
	dashboard.config.Mut().Notify.Mode = core.NotifyModeOff
	if len(dashboard.NotificationEvents()) != 0 {
		t.Fatal("turning notifications off before emission suppresses the ring")
	}
	dashboard.config.Mut().Notify.Mode = core.NotifyModeTerminal
	dashboard.config.Mut().Notify.DelaySeconds = 5
	dashboard.SetAlertEvents(14, []client.AlertEvent{event(14, asks)})
	received := dashboard.pendingAlerts[0].received
	if len(dashboard.notificationEventsAt(received.Add(4*time.Second))) != 0 {
		t.Fatal("the delay holds the ring")
	}
	if got := len(dashboard.notificationEventsAt(received.Add(5 * time.Second))); got != 1 {
		t.Fatalf("notices after the delay = %d", got)
	}
	if len(dashboard.notificationEventsAt(received.Add(6*time.Second))) != 0 {
		t.Fatal("a ring is emitted once")
	}

	dashboard.SetAlertEvents(15, []client.AlertEvent{event(15, asks)})
	received = dashboard.pendingAlerts[0].received
	// A new form reuses the request ID, but is a different published item.
	asksCard := dashboard.notificationCards.GetMut("asks")
	form := *asksCard.Form
	form.RaisedAtMs = 2
	asksCard.Form = &form
	if len(dashboard.notificationEventsAt(received.Add(5*time.Second))) != 0 {
		t.Fatal("a replaced item does not ring")
	}

	dashboard.SetAlertEvents(16, []client.AlertEvent{event(16, done)})
	received = dashboard.pendingAlerts[0].received
	dashboard.notificationCards.Remove("done")
	if len(dashboard.notificationEventsAt(received.Add(5*time.Second))) != 0 {
		t.Fatal("a withdrawn item does not ring")
	}
}
