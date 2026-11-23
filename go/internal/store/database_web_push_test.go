// Port of the tests in mj-store/src/database/web_push.rs.

package store

import (
	"encoding/json"
	"fmt"
	"math"
	"path/filepath"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func pushTestSubscription() Subscription {
	return Subscription{
		Endpoint: "https://push.example.com/device",
		Keys:     SubscriptionKeys{P256dh: "key", Auth: "auth"},
	}
}

func pushTestRuntime(away bool) client.RuntimeProjection {
	var runtime client.RuntimeProjection
	runtime.Metadata.OperatorPresence.Away = away
	cards := core.NewSnapshotMap[string, client.NotificationCard]()
	runtime.Metadata.NotificationCards = &cards
	runtime.Revision = 40
	return runtime
}

func pushTestShow(runtime *client.RuntimeProjection, sessionID string, entries []core.NotificationStackEntry) {
	runtime.Metadata.NotificationCards.Insert(sessionID, client.NotificationCard{
		SinceMs: 0,
		Badge:   core.NotificationKindNeedsInput,
		Entries: entries,
	})
}

func pushTestCompletion(ordinal uint64, message core.EntryMessage) core.NotificationStackEntry {
	return core.NotificationStackEntry{
		CompletedAtMs:    0,
		CompletedOrdinal: ordinal,
		Kind:             core.NotificationKindNeedsInput,
		Message:          message,
	}
}

func pushTestTurnOnly(runtime *client.RuntimeProjection, ids ...string) {
	runtime.Metadata.NotificationCards.Clear()
	for _, id := range ids {
		pushTestShow(runtime, id, []core.NotificationStackEntry{pushTestCompletion(1, core.EntryMessageNone{})})
	}
}

func pushTestResult(t *testing.T, db *Connection, delivery *PushDelivery, status *uint16, retryAt int64) {
	t.Helper()
	if err := applyPushResultWith(db, delivery.Subscription, delivery.Enrollment, delivery.Identity, status, retryAt, nil); err != nil {
		t.Fatal(err)
	}
}

func pushTestStatus(status uint16) *uint16 { return &status }

func pushTestClaim(t *testing.T, db *Connection, runtime *client.RuntimeProjection, now int64) []PushDelivery {
	t.Helper()
	due, _, err := pushReconcileWith(db, runtime, now, true)
	if err != nil {
		t.Fatal(err)
	}
	return due
}

func pushTestAddMessage(transcript *core.MaterializedSession, id string, ordinal uint64, text string) {
	chunk, _ := json.Marshal(map[string]any{"content": map[string]any{"type": "text", "text": text}})
	latest := ordinal
	transcript.Transcript = append(transcript.Transcript, &core.TranscriptItem{
		StableID:                  id,
		Position:                  ordinal,
		LatestContentEventOrdinal: &latest,
		CreatedAtMs:               int64(ordinal),
		LastChangedAtMs:           int64(ordinal),
		Body: core.TranscriptBodyAgent{
			Chunks:    []json.RawMessage{chunk},
			Streaming: false,
		},
	})
}

func pushTestFinishedTurn(start, completed uint64) core.MaterializedTurnOutcome {
	accepted, position := start, start
	return core.MaterializedTurnOutcome{
		CommandID:         fmt.Sprintf("turn-%d", completed),
		AcceptedOrdinal:   &accepted,
		TurnStartPosition: &position,
		CompletedOrdinal:  completed,
		CompletedAtMs:     int64(completed),
		Outcome:           core.TurnOutcomeKindCompleted{StopReason: "EndTurn"},
	}
}

func pushTestSaveTurnHistory(t *testing.T, db *Connection, sessionID string, turn *core.MaterializedTurnOutcome) {
	t.Helper()
	body, err := serializeTurnOutcome(turn)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,turn_start_position,body)
             VALUES(?1,?2,?3,?4,?5)`, sessionID, turn.CommandID, turn.CompletedOrdinal, turn.TurnStartPosition, body); err != nil {
		t.Fatal(err)
	}
}

func pushTestOpen(t *testing.T, path string) *Connection {
	t.Helper()
	db, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { db.Close() })
	return db
}

func pushTestNotice(t *testing.T, delivery *PushDelivery) Notice {
	t.Helper()
	var notice Notice
	if err := json.Unmarshal([]byte(delivery.Payload), &notice); err != nil {
		t.Fatal(err)
	}
	return notice
}

func pushTestAlerts(t *testing.T, db *Connection, after *uint64) (uint64, []client.AlertEvent) {
	t.Helper()
	high, events, err := alertEventsSinceWith(db, after)
	if err != nil {
		t.Fatal(err)
	}
	return high, events
}

func pushTestZero() *uint64 {
	zero := uint64(0)
	return &zero
}

func pushTestSaveSession(t *testing.T, path, id string) {
	t.Helper()
	record := session(id, "project")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
}

func pushTestSaveMaterialized(t *testing.T, path string, transcript *core.MaterializedSession) {
	t.Helper()
	if err := SaveMaterializedSessionTo(path, transcript); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: integrated review of a1f2e5f67: push read an id-less entry's text from an earlier turn.
func TestPushIDLessEntryReadsOnlyItsOwnTurn(t *testing.T) {
	path := filepath.Join(t.TempDir(), "push-test.sqlite3")
	db := pushTestOpen(t, path)
	if err := pushSubscribeWith(db, pushTestSubscription()); err != nil {
		t.Fatal(err)
	}
	pushTestSaveSession(t, path, "s")

	transcript := core.MaterializedSessionEmpty("s")
	transcript.AppliedEventOrdinal = 9
	transcript.AppliedEventDigest = fmt.Sprintf("%064x", 9)
	pushTestAddMessage(&transcript, "agent:previous", 4, "Earlier turn reply")
	finished := pushTestFinishedTurn(8, 9)
	transcript.LastTurnOutcome = &finished
	pushTestSaveMaterialized(t, path, &transcript)
	earlier := pushTestFinishedTurn(3, 5)
	pushTestSaveTurnHistory(t, db, "s", &earlier)

	runtime := pushTestRuntime(true)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 { // Cutover.
		t.Fatalf("cutover admitted %d", len(due))
	}
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{
		pushTestCompletion(5, core.EntryMessageNone{}),
		pushTestCompletion(9, core.EntryMessageNone{}),
	})
	withText := pushTestClaim(t, db, &runtime, 0)
	if len(withText) != 1 {
		t.Fatalf("due = %d", len(withText))
	}
	if body := pushTestNotice(t, &withText[0]).Body; body != "Earlier turn reply" {
		t.Fatalf("body = %q", body)
	}
	pushTestResult(t, db, &withText[0], pushTestStatus(201), 0)

	withoutText := pushTestClaim(t, db, &runtime, 0)
	if len(withoutText) != 1 {
		t.Fatalf("due = %d", len(withoutText))
	}
	if body := pushTestNotice(t, &withoutText[0]).Body; body != "Needs your input" {
		t.Fatalf("body = %q", body)
	}
}

func TestPushLocatorSettlesOnceAcrossWorkingRestartAndReclassification(t *testing.T) {
	path := filepath.Join(t.TempDir(), "push-test.sqlite3")
	db, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := pushSubscribeWith(db, pushTestSubscription()); err != nil {
		t.Fatal(err)
	}
	runtime := pushTestRuntime(true)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 { // Cutover.
		t.Fatalf("cutover admitted %d", len(due))
	}
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{pushTestCompletion(10, core.EntryMessageLocating{})})
	runtime.Metadata.SessionAttention.Insert("child", core.SessionAttentionNeedsInput)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("a locator in flight admitted %d", len(due))
	}

	transcript := core.MaterializedSessionEmpty("s")
	pushTestAddMessage(&transcript, "agent:original", 9, "Keep the old format?")
	pushTestAddMessage(&transcript, "agent:latest", 20, "New unrelated reply")
	window := core.ProjectionWindowOf(&transcript)
	runtime.Transcripts.Insert("s", client.SessionTailOf(&transcript, &window, 10))
	entry := pushTestCompletion(10, core.EntryMessageFound{Value: "agent:original"})
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{entry})
	due := pushTestClaim(t, db, &runtime, 0)
	if len(due) != 1 {
		t.Fatalf("due = %d", len(due))
	}
	notice := pushTestNotice(t, &due[0])
	if notice.Body != "Keep the old format?" {
		t.Fatalf("body = %q", notice.Body)
	}
	high, events := pushTestAlerts(t, db, pushTestZero())
	if len(events) != 1 || events[0].Destination != "s" || events[0].Text != notice.Body || events[0].Sequence != high {
		t.Fatalf("alerts = %+v (high %d)", events, high)
	}
	if notice.Episode == nil || *notice.Episode != due[0].Identity {
		t.Fatalf("episode = %v", notice.Episode)
	}
	if notice.ObservedRevision != 40 {
		t.Fatalf("observed revision = %d", notice.ObservedRevision)
	}
	pushTestResult(t, db, &due[0], pushTestStatus(201), 0)

	runtime.Metadata.SessionAttention.Insert("s", core.SessionAttentionWorking)
	pushTestTurnOnly(&runtime)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("working admitted %d", len(due))
	}
	runtime.Metadata.SessionAttention.Insert("s", core.SessionAttentionNeedsInput)
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{entry})
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("a settled locator admitted %d again", len(due))
	}

	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db = pushTestOpen(t, path)
	if err := pushRecoverWith(db); err != nil {
		t.Fatal(err)
	}
	entry.Kind = core.NotificationKindImportant
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{entry})
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("reclassification admitted %d", len(due))
	}
	if _, events := pushTestAlerts(t, db, pushTestZero()); len(events) != 1 {
		t.Fatalf("alerts = %d", len(events))
	}
	if _, events := pushTestAlerts(t, db, nil); len(events) != 0 {
		t.Fatalf("alerts after the frontier = %d", len(events))
	}
	if _, events := pushTestAlerts(t, db, &high); len(events) != 0 {
		t.Fatalf("alerts after %d = %d", high, len(events))
	}
}

func TestPromotedQuestionPushUsesTheCardOwnerMutePreference(t *testing.T) {
	for _, c := range []struct {
		parentMuted, childMuted bool
		owner                   string
		expected                *string
	}{
		{false, true, "parent", pushTestPointer("parent")},
		{true, true, "parent", nil},
		{false, false, "child", pushTestPointer("child")},
	} {
		db := pushTestOpen(t, filepath.Join(t.TempDir(), "push.sqlite3"))
		if err := pushSubscribeWith(db, pushTestSubscription()); err != nil {
			t.Fatal(err)
		}
		runtime := pushTestRuntime(true)
		if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
			t.Fatalf("cutover admitted %d", len(due))
		}
		for _, member := range []struct {
			id    string
			muted bool
		}{{"parent", c.parentMuted}, {"child", c.childMuted}} {
			record := session(member.id, "project")
			record.NotificationsIgnored = member.muted
			runtime.Records.Insert(member.id, record)
		}
		pushTestShow(&runtime, c.owner, nil)
		card := runtime.Metadata.NotificationCards.GetMut(c.owner)
		holder := "child"
		card.Holder = &holder
		card.Form = &client.CardForm{
			Source:     client.CardFormSourceElicitation,
			RaisedAtMs: 1,
			RequestID:  "approval",
			Message:    "Approve?",
		}
		due := pushTestClaim(t, db, &runtime, 1)
		expectedDue := 0
		if c.expected != nil {
			expectedDue = 1
		}
		if len(due) != expectedDue {
			t.Fatalf("%+v: due = %d", c, len(due))
		}
		_, events := pushTestAlerts(t, db, pushTestZero())
		if len(events) != len(due) {
			t.Fatalf("%+v: alerts = %d", c, len(events))
		}
		if c.expected != nil {
			notice := pushTestNotice(t, &due[0])
			if notice.SessionID == nil || *notice.SessionID != *c.expected {
				t.Fatalf("%+v: notice session = %v", c, notice.SessionID)
			}
			if events[0].Destination != "child" {
				t.Fatalf("%+v: alert destination = %s", c, events[0].Destination)
			}
			if notice.URL != "/#conversation/child" {
				t.Fatalf("%+v: url = %s", c, notice.URL)
			}
		}
	}
}

func pushTestPointer[T any](value T) *T { return &value }

func TestAlertDecisionIsIndependentOfPresenceDevicesAndReusedFormIDs(t *testing.T) {
	path := filepath.Join(t.TempDir(), "alerts.sqlite3")
	db, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	runtime := pushTestRuntime(false)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("cutover admitted %d", len(due))
	}
	parent := session("parent", "project")
	title := "Branch review"
	parent.SessionTitleOverride = &title
	runtime.Records.Insert("parent", parent)
	pushTestShow(&runtime, "parent", nil)
	card := runtime.Metadata.NotificationCards.GetMut("parent")
	holderID := "child"
	card.Holder = &holderID
	card.Form = &client.CardForm{
		Source:     client.CardFormSourceElicitation,
		RaisedAtMs: 1,
		RequestID:  "elicitation-1",
		Message:    "Which branch?",
	}
	holder := session("child", "project")
	holder.NotificationsIgnored = true
	runtime.Records.Insert("child", holder)
	if due := pushTestClaim(t, db, &runtime, 1); len(due) != 0 {
		t.Fatalf("no device admitted %d", len(due))
	}
	high, events := pushTestAlerts(t, db, pushTestZero())
	if len(events) != 1 || events[0].Destination != "child" || events[0].Text != "Which branch?" || events[0].SessionTitle != "Branch review" {
		t.Fatalf("alerts = %+v", events)
	}
	removed, ok := runtime.Metadata.NotificationCards.Remove("parent")
	if !ok {
		t.Fatal("the parent card is missing")
	}
	pushTestClaim(t, db, &runtime, 2)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db = pushTestOpen(t, path)
	runtime.Metadata.NotificationCards.Insert("parent", removed)
	pushTestClaim(t, db, &runtime, 3)
	if again, _ := pushTestAlerts(t, db, pushTestZero()); again != high {
		t.Fatalf("one live form remains one identity across a restart: %d != %d", again, high)
	}
	runtime.Metadata.NotificationCards.GetMut("parent").Form.RaisedAtMs = 4
	pushTestClaim(t, db, &runtime, 4)
	pushTestClaim(t, db, &runtime, 5)
	newHigh, events := pushTestAlerts(t, db, &high)
	if newHigh <= high {
		t.Fatalf("frontier did not advance: %d", newHigh)
	}
	if len(events) != 1 {
		t.Fatalf("a new incarnation of elicitation-1 alerts exactly once: %d", len(events))
	}
	pushTestSaveSession(t, path, "parent")
	if _, err := db.Exec("DELETE FROM sessions WHERE session_id='parent'"); err != nil {
		t.Fatal(err)
	}
	afterDestroy, remaining := pushTestAlerts(t, db, pushTestZero())
	if afterDestroy != newHigh {
		t.Fatalf("sqlite_sequence preserves the durable frontier after deleting the last event: %d", afterDestroy)
	}
	if len(remaining) != 0 {
		t.Fatal("destroyed session titles and text cannot replay")
	}
}

// Hard-won: c21e3b62b: delivery admitted a paused holder before observer reconciliation.
func TestPushOrdersOneDeviceTagAcrossRetryAndWorkerUnavailability(t *testing.T) {
	path := filepath.Join(t.TempDir(), "push-test.sqlite3")
	db := pushTestOpen(t, path)
	if err := pushSubscribeWith(db, pushTestSubscription()); err != nil {
		t.Fatal(err)
	}
	pushTestSaveSession(t, path, "s")
	transcript := core.MaterializedSessionEmpty("s")
	transcript.AppliedEventOrdinal = 20
	transcript.AppliedEventDigest = fmt.Sprintf("%064x", 20)
	pushTestAddMessage(&transcript, "agent:first", 2, "First reply")
	pushTestAddMessage(&transcript, "agent:second", 9, "Second reply")
	pushTestAddMessage(&transcript, "agent:newer", 19, "Too new for both cards")
	pushTestSaveMaterialized(t, path, &transcript)
	firstTurn, secondTurn := pushTestFinishedTurn(1, 3), pushTestFinishedTurn(8, 10)
	pushTestSaveTurnHistory(t, db, "s", &firstTurn)
	pushTestSaveTurnHistory(t, db, "s", &secondTurn)
	runtime := pushTestRuntime(false)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 { // Cutover.
		t.Fatalf("cutover admitted %d", len(due))
	}
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{
		pushTestCompletion(3, core.EntryMessageNone{}),
		pushTestCompletion(10, core.EntryMessageNone{}),
	})
	card := runtime.Metadata.NotificationCards.GetMut("s")
	holder := "child"
	card.Holder = &holder
	card.Form = &client.CardForm{
		Source:     client.CardFormSourceElicitation,
		RaisedAtMs: 1,
		RequestID:  "form:1",
		Message:    "Accept changes?",
	}
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 { // Presence.
		t.Fatalf("present operator admitted %d", len(due))
	}
	runtime.Metadata.OperatorPresence.Away = true
	ahead := runtime
	aheadCards := *runtime.Metadata.NotificationCards
	aheadCards.Clear()
	ahead.Metadata.NotificationCards = &aheadCards
	deferred, next, err := pushReconcileWith(db, &ahead, 0, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(deferred) != 0 {
		t.Fatalf("deferred = %d", len(deferred))
	}
	if next != nil {
		t.Fatalf("a delivery snapshot ahead of the observer waits for reconciliation, not an expired timer: %d", *next)
	}
	first := pushTestClaim(t, db, &runtime, 0)
	if len(first) != 1 || first[0].Identity != `["completion","s",3]` {
		t.Fatalf("first = %+v", first)
	}
	if body := pushTestNotice(t, &first[0]).Body; body != "First reply" {
		t.Fatalf("first body = %q", body)
	}
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("an in-flight tag admitted %d", len(due))
	}
	pushTestResult(t, db, &first[0], pushTestStatus(429), 10)
	runtime.Metadata.SessionAttention.Insert("child", core.SessionAttentionUnreachable)
	retry := pushTestClaim(t, db, &runtime, 10)
	if len(retry) == 0 || retry[0].Identity != first[0].Identity {
		t.Fatalf("stack entry delivery is independent of the form holder: %+v", retry)
	}
	runtime.Metadata.SessionAttention.Remove("child")
	if due := pushTestClaim(t, db, &runtime, 9); len(due) != 0 {
		t.Fatalf("before the deadline admitted %d", len(due))
	}
	pushTestResult(t, db, &retry[0], pushTestStatus(201), 10)

	second := pushTestClaim(t, db, &runtime, 10)
	if len(second) != 1 || second[0].Identity != `["completion","s",10]` {
		t.Fatalf("second = %+v", second)
	}
	if body := pushTestNotice(t, &second[0]).Body; body != "Second reply" {
		t.Fatalf("second body = %q", body)
	}
	pushTestResult(t, db, &second[0], pushTestStatus(413), math.MaxInt64) // Permanently retires only this intent.
	runtime.Metadata.SessionAttention.Insert("child", core.SessionAttentionUnreachable)
	paused, next, err := pushReconcileWith(db, &runtime, 10, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(paused) != 0 {
		t.Fatal("a paused holder cannot consume a still-visible ledger entry")
	}
	if next != nil {
		t.Fatal("availability recovery wakes deferred delivery")
	}
	var visible bool
	if err := db.QueryRow("SELECT visible FROM push_entries WHERE identity=?1", `["form","child","form:1",1]`).Scan(&visible); err != nil {
		t.Fatal(err)
	}
	if !visible {
		t.Fatal("the regression must exercise visibility ahead of reconciliation")
	}
	runtime.Metadata.SessionAttention.Remove("child")
	form := pushTestClaim(t, db, &runtime, 10)
	if len(form) != 1 || form[0].Identity != `["form","child","form:1",1]` {
		t.Fatalf("form = %+v", form)
	}
	notice := pushTestNotice(t, &form[0])
	if notice.Body != "Accept changes?" || notice.URL != "/#conversation/child" || notice.Tag != "s" {
		t.Fatalf("form notice = %+v", notice)
	}
	moved, _ := runtime.Metadata.NotificationCards.Remove("s")
	moved.Entries = nil
	runtime.Metadata.NotificationCards.Insert("other", moved)
	pushTestShow(&runtime, "s", []core.NotificationStackEntry{pushTestCompletion(20, core.EntryMessageNone{})})
	if due := pushTestClaim(t, db, &runtime, 10); len(due) != 0 {
		t.Fatalf("the moved form still holds its tag: %d", len(due))
	}
	pushTestResult(t, db, &form[0], pushTestStatus(201), 10)
	afterForm := pushTestClaim(t, db, &runtime, 10)
	if len(afterForm) != 1 || afterForm[0].Identity != `["completion","s",20]` {
		t.Fatalf("after form = %+v", afterForm)
	}
}

func TestPushCompatibleMigrationBaselinesFirstAuthoritativeMap(t *testing.T) {
	path := filepath.Join(t.TempDir(), "push-test.sqlite3")
	db, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	subscription := pushTestSubscription()
	if err := pushSubscribeWith(db, subscription); err != nil {
		t.Fatal(err)
	}
	restorePre153TranscriptLayout(t, db)
	if _, err := db.Exec(`DROP TABLE push_entry_deliveries;
             DROP TABLE push_entries;
             DROP TABLE push_entry_cutover;
             DROP TABLE alert_events;
             DELETE FROM schema_migrations WHERE version>=138;
             UPDATE schema_compatibility SET minimum_compatible_version=137 WHERE singleton=1;
             PRAGMA user_version=137;
             INSERT INTO push_episodes VALUES('s','legacy','{}','[]',1);
             CREATE TRIGGER stop_before_alert_ledger BEFORE INSERT ON schema_migrations
                 WHEN NEW.version=139 BEGIN SELECT RAISE(ABORT,'fixture boundary after push ledger'); END;
             CREATE TRIGGER stop_before_mail_fence BEFORE INSERT ON schema_migrations
                 WHEN NEW.version=142 BEGIN SELECT RAISE(ABORT,'fixture boundary before mail fence'); END;`); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec("INSERT INTO push_deliveries(endpoint,episode,payload,accepted) VALUES(?1,'legacy','{}',0)", subscription.Endpoint); err != nil {
		t.Fatal(err)
	}
	err = migrateSchema(db)
	if err == nil || !strings.Contains(err.Error(), "fixture boundary after push ledger") {
		t.Fatalf("migration = %v", err)
	}
	if _, err := db.Exec("ROLLBACK; DROP TRIGGER stop_before_alert_ledger;"); err != nil {
		t.Fatal(err)
	}
	pushed, err := readSchemaState(db)
	if err != nil {
		t.Fatal(err)
	}
	if pushed.revision != 138 {
		t.Fatalf("revision = %d", pushed.revision)
	}
	if err := pushed.ensureSupportedBy(137); err != nil {
		t.Fatal(err)
	}

	// Test the remaining compatible revisions before the separate fence for
	// checkpoint mail.
	err = migrateSchema(db)
	if err == nil || !strings.Contains(err.Error(), "fixture boundary before mail fence") {
		t.Fatalf("migration = %v", err)
	}
	if _, err := db.Exec("ROLLBACK; DROP TRIGGER stop_before_mail_fence;"); err != nil {
		t.Fatal(err)
	}
	beforeMail, err := readSchemaState(db)
	if err != nil {
		t.Fatal(err)
	}
	if beforeMail.revision != 141 {
		t.Fatalf("revision = %d", beforeMail.revision)
	}
	if err := beforeMail.ensureSupportedBy(137); err != nil {
		t.Fatal(err)
	}
	if err := migrateSchema(db); err != nil {
		t.Fatal(err)
	}

	runtime := pushTestRuntime(true)
	runtime.Metadata.NotificationCards = nil
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("an absent card map admitted %d", len(due))
	}
	cards := core.NewSnapshotMap[string, client.NotificationCard]()
	runtime.Metadata.NotificationCards = &cards
	pushTestTurnOnly(&runtime, "s")
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("the first authoritative map admitted %d", len(due))
	}
	var baseline bool
	if err := db.QueryRow("SELECT baseline FROM push_entries WHERE session='s'").Scan(&baseline); err != nil {
		t.Fatal(err)
	}
	if !baseline {
		t.Fatal("the first authoritative map is the baseline")
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db = pushTestOpen(t, path)
	pushTestTurnOnly(&runtime)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("an empty map admitted %d", len(due))
	}
	pushTestTurnOnly(&runtime, "s", "new")
	due := pushTestClaim(t, db, &runtime, 0)
	if len(due) != 1 || due[0].Identity != `["completion","new",1]` {
		t.Fatalf("due = %+v", due)
	}
	var accepted uint32
	if err := db.QueryRow("SELECT q.accepted FROM push_deliveries q JOIN push_episodes e ON e.identity=q.episode WHERE q.episode='legacy'").Scan(&accepted); err != nil {
		t.Fatal(err)
	}
	if accepted != 0 {
		t.Fatalf("legacy delivery accepted = %d", accepted)
	}
}

// Hard-won: c146a8a89: worker recovery must retain the pending question's identity.
func TestPushRestartReceiptsEnrollmentAndExpiryAreDurable(t *testing.T) {
	path := filepath.Join(t.TempDir(), "push-test.sqlite3")
	db, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	subscription := pushTestSubscription()
	if err := pushSubscribeWith(db, subscription); err != nil {
		t.Fatal(err)
	}
	runtime := pushTestRuntime(true)
	if due := pushTestClaim(t, db, &runtime, 0); len(due) != 0 {
		t.Fatalf("cutover admitted %d", len(due))
	}
	pushTestTurnOnly(&runtime, "a", "b")
	due := pushTestClaim(t, db, &runtime, 0)
	if len(due) != 2 {
		t.Fatalf("due = %d", len(due))
	}
	if again := pushTestClaim(t, db, &runtime, 0); len(again) != 0 {
		t.Fatalf("in-flight sends admitted %d again", len(again))
	}
	pushTestResult(t, db, &due[0], pushTestStatus(201), 0)
	// Only the uncertain request replays across restart.
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db = pushTestOpen(t, path)
	if err := pushRecoverWith(db); err != nil {
		t.Fatal(err)
	}
	retry := pushTestClaim(t, db, &runtime, 0)
	if len(retry) != 1 || retry[0].Identity != due[1].Identity {
		t.Fatalf("retry = %+v", retry)
	}
	pushTestResult(t, db, &retry[0], pushTestStatus(201), 0)

	// Backpressure carries across a routine browser refresh and applies to
	// new intents too.
	pushTestTurnOnly(&runtime, "c")
	pending := pushTestClaim(t, db, &runtime, 0)[0]
	pushTestResult(t, db, &pending, pushTestStatus(429), 100)
	if err := pushSubscribeWith(db, subscription); err != nil {
		t.Fatal(err)
	}
	pushTestTurnOnly(&runtime, "c", "d")
	if early := pushTestClaim(t, db, &runtime, 99); len(early) != 0 {
		t.Fatalf("before Retry-After admitted %d", len(early))
	}
	ready := pushTestClaim(t, db, &runtime, 100)
	if len(ready) != 2 {
		t.Fatalf("ready = %d", len(ready))
	}
	for i := range ready {
		pushTestResult(t, db, &ready[i], pushTestStatus(413), math.MaxInt64)
	}
	if err := pushTestWith(db, subscription.Endpoint); err != nil {
		t.Fatal(err)
	}
	var defaultRuntime client.RuntimeProjection
	test := pushTestClaim(t, db, &defaultRuntime, 100)[0]
	pushTestResult(t, db, &test, pushTestStatus(201), 0)

	// Expiry refresh preserves the flight, encryption replacement and ABA
	// invalidate old results, and endpoint rotation recovers uncertainty.
	pushTestTurnOnly(&runtime, "e")
	old := pushTestClaim(t, db, &runtime, 100)[0]
	refreshed := subscription
	expiration := 123.0
	refreshed.ExpirationTime = &expiration
	if err := pushSubscribeWith(db, refreshed); err != nil {
		t.Fatal(err)
	}
	if again := pushTestClaim(t, db, &runtime, 100); len(again) != 0 {
		t.Fatalf("an expiry refresh admitted %d", len(again))
	}
	replaced := subscription
	replaced.Keys.Auth = "replacement"
	if err := pushSubscribeWith(db, replaced); err != nil {
		t.Fatal(err)
	}
	replacement := pushTestClaim(t, db, &runtime, 100)[0]
	pushTestResult(t, db, &old, pushTestStatus(410), 0)
	if err := pushSubscribeWith(db, subscription); err != nil {
		t.Fatal(err)
	}
	aba := pushTestClaim(t, db, &runtime, 100)[0]
	if aba.Enrollment == old.Enrollment {
		t.Fatal("a recreated enrollment must not reuse the old identifier")
	}
	pushTestResult(t, db, &replacement, pushTestStatus(410), 0)
	pushTestResult(t, db, &old, pushTestStatus(201), 0)
	pushTestResult(t, db, &aba, pushTestStatus(201), 0)

	pushTestTurnOnly(&runtime, "e", "f")
	uncertain := pushTestClaim(t, db, &runtime, 100)[0]
	rotated := subscription
	rotated.Endpoint = "https://push.example.com/rotated"
	if err := pushRenewWith(db, subscription.Endpoint, rotated); err != nil {
		t.Fatal(err)
	}
	if err := pushRenewWith(db, subscription.Endpoint, rotated); err != nil {
		t.Fatal(err)
	}
	pushTestResult(t, db, &uncertain, pushTestStatus(410), 0)
	rotatedDue := pushTestClaim(t, db, &runtime, 100)
	if len(rotatedDue) != 1 || rotatedDue[0].Identity != uncertain.Identity {
		t.Fatalf("rotated due = %+v", rotatedDue)
	}
	pushTestResult(t, db, &rotatedDue[0], pushTestStatus(410), 0)
	if last := pushTestClaim(t, db, &runtime, 100); len(last) != 0 {
		t.Fatalf("after rotation = %d", len(last))
	}
}
