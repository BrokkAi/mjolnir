// Port of the tests in mj-store/src/database/operator_away.rs.

package store

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// operatorAwayEvent is an API event and when it was recorded.
type operatorAwayEvent struct {
	at    int64
	event core.ApiEventData
}

// operatorAwayInsert records events in one transaction, failing the test on
// any error.
func operatorAwayInsert(t *testing.T, connection *Connection, sessionID string, events ...operatorAwayEvent) {
	t.Helper()
	tx, err := connection.Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	for _, event := range events {
		if err := insertAPIEvent(tx, sessionID, event.at, event.event); err != nil {
			t.Fatal(err)
		}
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: #1444: a child operator-away resolution blocked relay projection forever.
func TestChildOperatorAwayResolutionAdvancesAPIEventsWithoutAnAwayCapture(t *testing.T) {
	path := filepath.Join(t.TempDir(), "operator-away.sqlite3")
	for _, id := range []string{"parent", "child"} {
		record := session(id, "project-1")
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := connection.Exec("INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES ('child','parent','child','{}')"); err != nil {
		t.Fatal(err)
	}
	request := core.ElicitationRequest{
		ID:      "elicitation-1",
		Message: "Choose a response",
		Fields:  []core.ElicitationField{},
	}
	required := core.ApiEventDataInputRequired{Request: &request}
	resolved := core.ApiEventDataInputResolved{
		ElicitationID: "elicitation-1",
		Action:        core.OperatorAwayAction,
	}
	operatorAwayInsert(t, connection, "child", operatorAwayEvent{1, required}, operatorAwayEvent{2, resolved})

	var childEvents, captures int64
	if err := connection.QueryRow("SELECT count(*) FROM api_events WHERE session_id='child'").Scan(&childEvents); err != nil {
		t.Fatal(err)
	}
	if err := connection.QueryRow("SELECT count(*) FROM operator_away_requests WHERE session_id='child'").Scan(&captures); err != nil {
		t.Fatal(err)
	}
	if childEvents != 2 || captures != 0 {
		t.Fatalf("child events = %d, captures = %d", childEvents, captures)
	}

	tx, err := connection.Begin()
	if err != nil {
		t.Fatal(err)
	}
	err = insertAPIEvent(tx, "parent", 3, resolved)
	if err == nil || !strings.Contains(err.Error(), "has no captured request") {
		t.Fatalf("parent resolution without a capture: %v", err)
	}
	_ = tx.Rollback()

	// A child's approval waits for a person, so AFK answers it and the child
	// is told, as any session would be.
	changed, err := setWith(connection, true, 0)
	if err != nil {
		t.Fatal(err)
	}
	if !changed {
		t.Fatal("going away must change presence")
	}
	permission := core.ElicitationRequest{
		ID:             "tool-permission-1",
		Message:        "May I run this command?",
		Fields:         []core.ElicitationField{},
		ToolPermission: &core.ToolPermissionForm{Skip: true},
	}
	operatorAwayInsert(t, connection, "child",
		operatorAwayEvent{4, core.ApiEventDataInputRequired{Request: &permission}},
		operatorAwayEvent{5, core.ApiEventDataInputResolved{ElicitationID: "tool-permission-1", Action: core.OperatorAwayAction}})
	var notice string
	if err := connection.QueryRow("SELECT event_json FROM mailbox_outbox WHERE target_session_id='child' AND event_key LIKE 'operator-away-%'").Scan(&notice); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(notice, "May I run this command?") {
		t.Fatal(notice)
	}
}

// The operator answered this session directly within the auto-AFK window
// before leaving: its requests were never unattended, so "the operator is
// back" would only tell the agent to re-ask what was already answered.
func TestOperatorBackSkipsSessionsAnsweredWithinTheAutoAfkWindow(t *testing.T) {
	path := filepath.Join(t.TempDir(), "operator-back.sqlite3")
	for _, id := range []string{"answered", "stale", "plain"} {
		record := session(id, "project-1")
		record.State = core.SessionStateRunning
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	question := func(id, text string) core.ElicitationRequest {
		params, err := json.Marshal(map[string]any{
			"mode": "form", "sessionId": id, "message": text,
			"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"choice": map[string]any{"type": "string", "enum": []string{"A", "B"}}}},
		})
		if err != nil {
			t.Fatal(err)
		}
		request, err := core.ElicitationRequestFromACPParams(id, params)
		if err != nil {
			t.Fatal(err)
		}
		return request
	}
	ask := func(sessionID string, request core.ElicitationRequest) {
		operatorAwayInsert(t, connection, sessionID,
			operatorAwayEvent{core.EpochMillis(), core.ApiEventDataInputRequired{Request: &request}},
			operatorAwayEvent{core.EpochMillis(), core.ApiEventDataInputResolved{ElicitationID: request.ID, Action: core.OperatorAwayAction}})
	}
	// The operator's own answers: one just before leaving, one long before.
	now := core.EpochMillis()
	for _, answer := range []struct {
		session string
		at      int64
	}{{"answered", now}, {"stale", now - 7_200_000}} {
		operatorAwayInsert(t, connection, answer.session, operatorAwayEvent{answer.at, core.ApiEventDataInputResolved{
			ElicitationID: fmt.Sprintf("earlier-%s", answer.session),
			Action:        "accept",
		}})
	}
	window := 3600 * time.Second
	if changed, err := setWith(connection, true, window); err != nil || !changed {
		t.Fatalf("going away: %v, %v", changed, err)
	}
	for _, asked := range []struct{ session, id, text string }{
		{"answered", "q-answered", "Continue with A?"},
		{"stale", "q-stale", "Continue with A?"},
		{"plain", "q-plain", "Continue with A?"},
	} {
		ask(asked.session, question(asked.id, asked.text))
	}
	if changed, err := setWith(connection, false, window); err != nil || !changed {
		t.Fatalf("coming back: %v, %v", changed, err)
	}
	notified, err := valuesQueryMap(connection, "SELECT target_session_id, event_json FROM mailbox_outbox WHERE event_key LIKE 'operator-back-%'", nil,
		func(rows *sql.Rows) (string, error) {
			var target, event string
			err := rows.Scan(&target, &event)
			return target, err
		})
	if err != nil {
		t.Fatal(err)
	}
	slices.Sort(notified)
	if !slices.Equal(notified, []string{"plain", "stale"}) {
		t.Fatalf("notified %v", notified)
	}
}
