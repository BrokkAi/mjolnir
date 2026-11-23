// Port of mj-store/src/database/events.rs (its #[cfg(test)] module).

package store

import (
	"encoding/json"
	"errors"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func eventsTestPage(t testing.TB, path string, observations []core.RelayObservation, fail bool) error {
	t.Helper()
	return eventsTestPageFor(t, path, "session-1", observations, fail)
}

func eventsTestPageFor(t testing.TB, path string, sessionID string, observations []core.RelayObservation, fail bool) error {
	t.Helper()
	current, err := LoadMaterializedSessionFrom(path, sessionID)
	if err != nil {
		return err
	}
	if current == nil {
		t.Fatalf("session %s has no materialized projection", sessionID)
	}
	_, err = applyProjectionPageTo(path, sessionID, func(page *ProjectionPage) (struct{}, error) {
		for _, observation := range observations {
			event := core.RelayEvent{
				Format:         core.RelayEventFormatV1,
				Ordinal:        current.AppliedEventOrdinal + 1,
				PreviousDigest: current.AppliedEventDigest,
				Digest:         "",
				RecordedAtMs:   100,
				CommandID:      nil,
				Observation:    observation,
			}
			digest, err := core.RelayEventDigest(&event)
			if err != nil {
				return struct{}{}, err
			}
			event.Digest = digest
			projected, err := transcript.ProjectRelayEvent(current, &event)
			if err != nil {
				return struct{}{}, err
			}
			mutation := projected.Mutation
			if _, err := page.Apply(event.Ordinal, event.PreviousDigest, event.Digest, &mutation); err != nil {
				return struct{}{}, err
			}
			if err := transcript.ApplyCommittedProjectionEvent(current, &event, mutation); err != nil {
				return struct{}{}, err
			}
		}
		if fail {
			return struct{}{}, errors.New("injected rollback")
		}
		return struct{}{}, nil
	})
	return err
}

func eventsTestTurn() []core.RelayObservation {
	return []core.RelayObservation{
		core.RelayObservationCommandQueued{
			CommandID:     "prompt-1",
			Command:       core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{}},
			CreatedAtMs:   1,
			InsertAtFront: false,
		},
		core.RelayObservationCommandStarted{
			CommandID:   "prompt-1",
			StartedAtMs: 2,
		},
		core.RelayObservationCommandCompleted{
			BarrierCommandID: nil,
			Command:          nil,
			CommandID:        "prompt-1",
			Outcome: core.RelayCommandOutcomePrompt{
				WorkFacts:  nil,
				Diagnostic: nil,
				StopReason: "end_turn",
				Usage:      nil,
			},
		},
	}
}

func eventsTestLoad(t testing.TB, path string, filter core.ApiEventFilter, afterSeq *uint64, limit int) core.ApiEventPage {
	t.Helper()
	page, err := loadAPIEventsFrom(path, &filter, afterSeq, limit)
	if err != nil {
		t.Fatal(err)
	}
	return page
}

func eventsTestSeq(value uint64) *uint64 { return &value }

func eventsTestMaterialized(t testing.TB, path string, sessionID string) *core.MaterializedSession {
	t.Helper()
	current, err := LoadMaterializedSessionFrom(path, sessionID)
	if err != nil {
		t.Fatal(err)
	}
	if current == nil {
		t.Fatalf("session %s has no materialized projection", sessionID)
	}
	return current
}

func eventsTestKinds(page core.ApiEventPage) []string {
	kinds := []string{}
	for _, event := range page.Events {
		kinds = append(kinds, event.Event.Kind())
	}
	return kinds
}

func eventsTestRunning(t testing.TB, path string, ids ...string) {
	t.Helper()
	for _, id := range ids {
		record := session(id, "project")
		record.State = core.SessionStateRunning
		schemaTestSaveSession(t, path, record)
	}
}

// eventsTestOutbox is every mailbox_outbox row as (target, event, wake), by
// event key.
type eventsTestMail struct {
	target string
	event  core.MailboxEvent
	wake   bool
}

func eventsTestOutbox(t testing.TB, connection Queryer) []eventsTestMail {
	t.Helper()
	rows, err := connection.Query(`SELECT target_session_id,event_json,wake FROM mailbox_outbox ORDER BY event_key`)
	schemaTestMust(t, err)
	messages := []eventsTestMail{}
	for rows.Next() {
		var target, body string
		var wake bool
		schemaTestMust(t, rows.Scan(&target, &body, &wake))
		var event core.MailboxEvent
		schemaTestMust(t, json.Unmarshal([]byte(body), &event))
		messages = append(messages, eventsTestMail{target, event, wake})
	}
	schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
	return messages
}

func eventsTestQuestion(t testing.TB, id string, params string) core.ElicitationRequest {
	t.Helper()
	request, err := core.ElicitationRequestFromACPParams(id, json.RawMessage(params))
	schemaTestMust(t, err)
	return request
}

func TestAPIEventsSurviveCoalescingRollbackAndReopen(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	if eventsTestPage(t, path, eventsTestTurn(), true) == nil {
		t.Fatal("the injected rollback succeeded")
	}
	if page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100); len(page.Events) != 0 {
		t.Fatalf("events %v", eventsTestKinds(page))
	}
	schemaTestMust(t, eventsTestPage(t, path, eventsTestTurn(), false))
	first := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 1)
	if kinds := eventsTestKinds(first); !slices.Equal(kinds, []string{"turn_started"}) {
		t.Fatalf("first %v", kinds)
	}
	rest := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(first.NextAfterSeq), 100)
	if kinds := eventsTestKinds(rest); !slices.Equal(kinds, []string{"turn_ended"}) {
		t.Fatalf("rest %v", kinds)
	}
	current := eventsTestMaterialized(t, path, "session-1")
	if current.ActiveTurn != nil {
		t.Fatal("a turn is still active")
	}
	// Re-delivery of the committed frontier must not insert a duplicate.
	_, err := applyProjectionEventTo(path, "session-1", current.AppliedEventOrdinal, "", current.AppliedEventDigest, &core.MaterializedSessionMutation{})
	schemaTestMust(t, err)
	if page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100); len(page.Events) != 2 {
		t.Fatalf("events %v", eventsTestKinds(page))
	}
	if page := eventsTestLoad(t, path, core.ApiEventFilter{}, nil, 100); len(page.Events) != 0 {
		t.Fatalf("events %v", eventsTestKinds(page))
	}
}

func TestAPIEventsFilterAndKeepCursorsAfterForgettingSessions(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	schemaTestMust(t, eventsTestPage(t, path, eventsTestTurn(), false))
	other := "other"
	empty := eventsTestLoad(t, path, core.ApiEventFilter{SessionID: &other}, eventsTestSeq(0), 100)
	if len(empty.Events) != 0 || empty.NextAfterSeq != 2 {
		t.Fatalf("filtered page %#v", empty)
	}
	workspace := "default"
	if page := eventsTestLoad(t, path, core.ApiEventFilter{WorkspaceID: &workspace}, eventsTestSeq(0), 100); len(page.Events) != 2 {
		t.Fatalf("workspace events %v", eventsTestKinds(page))
	}
	schemaTestMust(t, deleteSessionFrom(path, "session-1"))
	deleted := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(2), 100)
	if len(deleted.Events) != 0 || deleted.LatestSeq != 2 {
		t.Fatalf("page after deletion %#v", deleted)
	}
}

func TestOperatorAwayMailIsAtomicAndReturnNoticeSkipsLaterBlockingRequests(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "away.sqlite")
	eventsTestRunning(t, path, "session-1", "session-2", "legacy", "late", "pre-away")
	question := eventsTestQuestion(t, "reused-id", `{
		"mode":"form", "sessionId":"session-1", "message":"Which architecture?",
		"requestedSchema":{"type":"object", "properties":{"choice":{"type":"string", "enum":["thin","thick"]}}}
	}`)
	requested := func() core.RelayObservation {
		return core.RelayObservationElicitationRequested{Request: question}
	}
	reply := "not a user answer"
	resolved := func() core.RelayObservation {
		return core.RelayObservationElicitationResolved{
			ElicitationID: question.ID,
			Action:        core.OperatorAwayAction,
			// Even a historical worker carrying a reply must not manufacture a user answer.
			Reply: &reply,
		}
	}
	// A question already open when AFK starts must be captured into that AFK period
	// when the worker's away resolution arrives.
	schemaTestMust(t, eventsTestPageFor(t, path, "pre-away", []core.RelayObservation{requested()}, false))
	connection, err := open(path)
	schemaTestMust(t, err)
	if changed, err := setWith(connection, true, 0); err != nil || !changed {
		t.Fatalf("away: %v, %v", changed, err)
	}
	if changed, err := setWith(connection, true, 0); err != nil || changed {
		t.Fatalf("away again: %v, %v", changed, err)
	}
	if presence := schemaTestLoadState(t, path).OperatorPresence; presence != (core.OperatorPresence{Away: true, Revision: 1}) {
		t.Fatalf("presence %#v", presence)
	}
	schemaTestExec(t, connection, `CREATE TRIGGER fail_away BEFORE INSERT ON mailbox_outbox BEGIN SELECT RAISE(ABORT,'away rollback'); END;`)
	if eventsTestPage(t, path, []core.RelayObservation{requested(), resolved()}, false) == nil {
		t.Fatal("the away resolution committed despite the failing outbox")
	}
	if count := schemaTestInt(t, connection, "SELECT count(*) FROM api_events"); count != 1 {
		t.Fatalf("the pre-AFK request remains while the failed away resolution rolls back: %d", count)
	}
	schemaTestExec(t, connection, "DROP TRIGGER fail_away")
	schemaTestMust(t, connection.Close())
	for _, id := range []string{"session-1", "session-2", "legacy"} {
		// Capture and resolution cross a transaction/restart boundary.
		schemaTestMust(t, eventsTestPageFor(t, path, id, []core.RelayObservation{requested()}, false))
		schemaTestMust(t, eventsTestPageFor(t, path, id, []core.RelayObservation{resolved()}, false))
	}
	schemaTestMust(t, eventsTestPageFor(t, path, "pre-away", []core.RelayObservation{resolved()}, false))
	connection, err = open(path)
	schemaTestMust(t, err)
	schemaTestExec(t, connection, `INSERT INTO operator_away_workers(session_id,protocol) VALUES('legacy',44)`)
	schemaTestMust(t, connection.Close())
	// A later request with the same runtime-local id waits for the operator.
	schemaTestMust(t, eventsTestPage(t, path, []core.RelayObservation{requested()}, false))
	// A later autoreply resets the blocking cutoff for the other session.
	schemaTestMust(t, eventsTestPageFor(t, path, "session-2", []core.RelayObservation{requested(), resolved()}, false))
	current := eventsTestMaterialized(t, path, "session-2")
	if len(current.PendingElicitations) != 0 {
		t.Fatalf("pending %#v", current.PendingElicitations)
	}
	items := make([]core.TranscriptItem, 0, len(current.Transcript))
	for _, item := range current.Transcript {
		items = append(items, *item)
	}
	outputs := core.TerminalOutputsOf(items)
	texts := []string{}
	for _, item := range current.Transcript {
		texts = append(texts, transcript.TranscriptItemTextWithOutputs(item, outputs))
	}
	text := strings.Join(texts, "\n")
	if !strings.Contains(text, "Not shown because you were away: Which architecture?") {
		t.Fatalf("transcript %s", text)
	}
	if strings.Contains(text, "not a user answer") {
		t.Fatalf("transcript %s", text)
	}
	// This request was refused before AFK-off, but its resolution arrives later.
	schemaTestMust(t, eventsTestPageFor(t, path, "late", []core.RelayObservation{requested()}, false))
	connection, err = open(path)
	schemaTestMust(t, err)
	schemaTestExec(t, connection, "DELETE FROM api_events WHERE session_id='late'")
	if changed, err := setWith(connection, false, 0); err != nil || !changed {
		t.Fatalf("back: %v, %v", changed, err)
	}
	// Repeating the same write and rebuilding a projection emit nothing twice.
	if changed, err := setWith(connection, false, 0); err != nil || changed {
		t.Fatalf("back again: %v, %v", changed, err)
	}
	if presence := schemaTestLoadState(t, path).OperatorPresence; presence != (core.OperatorPresence{Away: false, Revision: 2}) {
		t.Fatalf("presence %#v", presence)
	}
	schemaTestMust(t, connection.Close())
	schemaTestMust(t, eventsTestPageFor(t, path, "late", []core.RelayObservation{resolved()}, false))
	empty := core.MaterializedSessionEmpty("late")
	schemaTestMust(t, SaveMaterializedSessionTo(path, &empty))
	schemaTestMust(t, eventsTestPageFor(t, path, "late", []core.RelayObservation{requested(), resolved()}, false))
	empty = core.MaterializedSessionEmpty("pre-away")
	schemaTestMust(t, SaveMaterializedSessionTo(path, &empty))
	schemaTestMust(t, eventsTestPageFor(t, path, "pre-away", []core.RelayObservation{requested(), resolved()}, false))
	empty = core.MaterializedSessionEmpty("session-2")
	schemaTestMust(t, SaveMaterializedSessionTo(path, &empty))
	schemaTestMust(t, eventsTestPageFor(t, path, "session-2",
		[]core.RelayObservation{requested(), resolved(), requested(), resolved()}, false))
	connection, err = open(path)
	schemaTestMust(t, err)
	defer connection.Close()
	awayCount, backCount := 0, 0
	for _, message := range eventsTestOutbox(t, connection) {
		if !message.wake {
			t.Fatal("operator mail must wake its session")
		}
		switch body := message.event.Body.(type) {
		case core.MailboxEventBodyOperatorAway:
			awayCount++
			if !strings.Contains(body.Request, "Which architecture?") {
				t.Fatalf("away request %q", body.Request)
			}
		case core.MailboxEventBodyOperatorBack:
			backCount++
			if message.target != "session-2" && message.target != "late" && message.target != "pre-away" {
				t.Fatalf("back notice for %s", message.target)
			}
			expected := 1
			if message.target == "session-2" {
				expected = 2
			}
			if len(body.Requests) != expected {
				t.Fatalf("%s back requests %v", message.target, body.Requests)
			}
		default:
			t.Fatalf("unexpected event %#v", body)
		}
	}
	if awayCount != 5 || backCount != 3 {
		t.Fatalf("away %d, back %d", awayCount, backCount)
	}
}

// A child's question reaches its parent under the name the session list
// shows for the child: a typed name first, then the harness's, then the
// title it was created with, cut to one bounded line.
func TestAChildQuestionNamesTheChildAsTheSessionListDoes(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "question-names.sqlite")
	longTitle := "Fix the parser\n\n" + strings.Repeat("The prompt goes on for paragraphs. ", 20)
	eventsTestRunning(t, path, "parent")
	text := func(value string) *string { return &value }
	cases := []struct {
		id            string
		titleOverride *string
		acpTitle      *string
	}{
		{"created-with", nil, nil},
		{"harness", nil, text("Harness name")},
		{"typed", text("Typed name"), text("Harness name")},
	}
	question := eventsTestQuestion(t, "elicitation-1", `{
		"mode":"form", "sessionId":"child", "message":"Which architecture?",
		"requestedSchema":{"type":"object", "properties":{"choice":{"type":"string", "title":"Architecture", "enum":["Thin callers", "Thick callers"]}}}
	}`)
	for _, c := range cases {
		child := session(c.id, "project-1")
		child.State = core.SessionStateRunning
		child.Title = longTitle
		child.SessionTitleOverride = c.titleOverride
		child.ACPSessionTitle = c.acpTitle
		schemaTestSaveSession(t, path, child)
		func() {
			connection, err := open(path)
			schemaTestMust(t, err)
			defer connection.Close()
			schemaTestExec(t, connection, `INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES (?1,'parent',?1,'{}')`, c.id)
		}()
		schemaTestMust(t, eventsTestPageFor(t, path, c.id, []core.RelayObservation{
			core.RelayObservationElicitationRequested{Request: question},
		}, false))
	}
	connection, err := open(path)
	schemaTestMust(t, err)
	defer connection.Close()
	senderTitle := func(child string) string {
		t.Helper()
		body := schemaTestText(t, connection, `SELECT event_json FROM mailbox_outbox WHERE event_key LIKE ?1`, "subagent-question-"+child+"-%")
		var event core.MailboxEvent
		schemaTestMust(t, json.Unmarshal([]byte(body), &event))
		message, ok := event.Body.(core.MailboxEventBodySessionMessage)
		if !ok || message.From.Type != core.SenderSession {
			t.Fatalf("session message expected, got %#v", event.Body)
		}
		if message.From.ID != child {
			t.Fatalf("sender %s", message.From.ID)
		}
		return message.From.Title
	}
	createdWith := senderTitle("created-with")
	if strings.Contains(createdWith, "\n") {
		t.Fatalf("%q", createdWith)
	}
	if !strings.HasPrefix(createdWith, "Fix the parser The prompt goes on") {
		t.Fatalf("%q", createdWith)
	}
	if !strings.HasSuffix(createdWith, "…") {
		t.Fatalf("%q", createdWith)
	}
	if len([]rune(createdWith)) > core.MessageNameChars {
		t.Fatalf("%q is %d characters", createdWith, len([]rune(createdWith)))
	}
	if title := senderTitle("harness"); title != "Harness name" {
		t.Fatalf("harness title %q", title)
	}
	if title := senderTitle("typed"); title != "Typed name" {
		t.Fatalf("typed title %q", title)
	}
}

func TestChildQuestionOutboxSurvivesCancellationReplayAndReusedIdsAndSkipsApprovals(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "questions.sqlite")
	eventsTestRunning(t, path, "session-1", "parent")
	question := eventsTestQuestion(t, "elicitation-1", `{
		"mode":"form", "sessionId":"session-1", "message":"Which architecture?",
		"requestedSchema":{"type":"object", "properties":{"choice":{"type":"string", "title":"Architecture", "enum":["Thin callers", "Thick callers"]}}}
	}`)
	clone := func(request core.ElicitationRequest) core.ElicitationRequest {
		request.Fields = slices.Clone(request.Fields)
		return request
	}
	reused := clone(question)
	reused.Message = "Which database?"
	permission := clone(question)
	permission.ID = "tool-permission-1"
	permission.Message = "May I run this command?"
	permission.ToolPermission = &core.ToolPermissionForm{Skip: true, ToolCallID: nil}
	secret := clone(question)
	secret.Message = "Paste the token"
	secret.Fields[0].Secret = true
	plan := clone(question)
	plan.ID = core.PlanReviewIDPrefix + "1"
	plan.Message = "Approve this plan?"
	observations := func() []core.RelayObservation {
		observations := []core.RelayObservation{}
		// A restarted harness may reuse the same question id. A parent
		// cannot grant approvals, so they stay with the person.
		for _, request := range []core.ElicitationRequest{question, reused, permission, secret, plan} {
			observations = append(observations,
				core.RelayObservationElicitationRequested{Request: clone(request)},
				core.RelayObservationElicitationResolved{
					ElicitationID: request.ID,
					Action:        "cancel",
					Reply:         nil,
				})
		}
		return observations
	}
	// A top-level form remains local.
	schemaTestMust(t, eventsTestPage(t, path, observations(), false))
	connection, err := open(path)
	schemaTestMust(t, err)
	if count := schemaTestInt(t, connection, "SELECT count(*) FROM mailbox_outbox"); count != 0 {
		t.Fatalf("outbox %d", count)
	}
	schemaTestExec(t, connection, `INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES ('session-1','parent','session-1','{}')`)
	schemaTestExec(t, connection, `CREATE TRIGGER fail_question_mail BEFORE INSERT ON mailbox_outbox BEGIN SELECT RAISE(ABORT, 'question mail failed'); END;`)
	if err := eventsTestPage(t, path, observations(), false); err == nil || !strings.Contains(err.Error(), "question mail failed") {
		t.Fatalf("page error %v", err)
	}
	if page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100); len(page.Events) != 10 {
		t.Fatalf("outbox failure rolls back its API events too: %d", len(page.Events))
	}
	schemaTestExec(t, connection, "DROP TRIGGER fail_question_mail")
	schemaTestMust(t, connection.Close())
	schemaTestMust(t, eventsTestPage(t, path, observations(), false))
	current := eventsTestMaterialized(t, path, "session-1")
	if len(current.PendingElicitations) != 0 {
		t.Fatal("every form resolved in this same projection page")
	}
	_, err = applyProjectionEventTo(path, "session-1", current.AppliedEventOrdinal, "", current.AppliedEventDigest, &core.MaterializedSessionMutation{})
	schemaTestMust(t, err)
	connection, err = open(path)
	schemaTestMust(t, err)
	messages := eventsTestOutbox(t, connection)
	if len(messages) != 2 {
		t.Fatalf("messages %d", len(messages))
	}
	texts := []string{}
	for _, message := range messages {
		if message.target != "parent" {
			t.Fatalf("only the direct parent receives the question: %s", message.target)
		}
		if !message.wake {
			t.Fatal("the question must wake the parent")
		}
		body, ok := message.event.Body.(core.MailboxEventBodySessionMessage)
		if !ok || body.From.Type != core.SenderSession {
			t.Fatalf("session message expected, got %#v", message.event.Body)
		}
		if body.From.ID != "session-1" {
			t.Fatalf("sender %s", body.From.ID)
		}
		if !strings.Contains(body.Text, `Architecture
- Thin callers
- Thick callers`) {
			t.Fatalf("text %s", body.Text)
		}
		if !strings.Contains(body.Text, "send_message to session_id session-1") {
			t.Fatalf("text %s", body.Text)
		}
		texts = append(texts, body.Text)
	}
	contains := func(needle string) bool {
		return slices.ContainsFunc(texts, func(text string) bool { return strings.Contains(text, needle) })
	}
	if !contains("Which architecture?") || !contains("Which database?") {
		t.Fatalf("texts %v", texts)
	}
	if contains("May I run this command?") || contains("Paste the token") || contains("Approve this plan?") {
		t.Fatalf("an approval was forwarded: %v", texts)
	}
	if reconciled, err := reconcilePendingSubagentQuestionsWith(connection); err != nil || reconciled != 0 {
		t.Fatalf("reconciled %d, %v", reconciled, err)
	}
	// Projection repair can reset to genesis/checkpoint and replay the old
	// relay observations with new API sequence numbers. Its durable input
	// frontier survives replacement and suppresses historical questions.
	schemaTestMust(t, connection.Close())
	empty := core.MaterializedSessionEmpty("session-1")
	schemaTestMust(t, SaveMaterializedSessionTo(path, &empty))
	schemaTestMust(t, eventsTestPage(t, path, append(observations(), observations()...), false))
	connection, err = open(path)
	schemaTestMust(t, err)
	if count := schemaTestInt(t, connection, `SELECT count(*) FROM api_events WHERE json_extract(body,'$.type')='input_required'`); count != 10 {
		t.Fatalf("input events %d", count)
	}
	if count := schemaTestInt(t, connection, "SELECT count(*) FROM mailbox_outbox"); count != 2 {
		t.Fatalf("outbox %d", count)
	}
	// A legacy worker leaves the next form pending. Simulate an older daemon
	// which stored its input event without creating its outbox message.
	schemaTestMust(t, eventsTestPage(t, path, []core.RelayObservation{
		core.RelayObservationElicitationRequested{Request: clone(question)},
	}, false))
	schemaTestExec(t, connection, `DELETE FROM mailbox_outbox WHERE event_key=(SELECT 'subagent-question-session-1-' || max(seq) FROM api_events)`)
	schemaTestMust(t, connection.Close())
	reopened, err := open(path)
	schemaTestMust(t, err)
	defer reopened.Close()
	if reconciled, err := reconcilePendingSubagentQuestionsWith(reopened); err != nil || reconciled != 1 {
		t.Fatalf("reconciled %d, %v", reconciled, err)
	}
	if reconciled, err := reconcilePendingSubagentQuestionsWith(reopened); err != nil || reconciled != 0 {
		t.Fatalf("reconciled again %d, %v", reconciled, err)
	}
	if count := schemaTestInt(t, reopened, "SELECT count(*) FROM mailbox_outbox"); count != 3 {
		t.Fatalf("outbox %d", count)
	}
}

// Hard-won: #1345: a stopped child's forwarded question invited a
// send_message answer the child could never receive.
func TestAChildThatCannotReceiveWorkGetsNoForwardedQuestion(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "stale-question.sqlite")
	eventsTestRunning(t, path, "parent")
	child := session("session-1", "project-1")
	child.State = core.SessionStateStopped
	schemaTestSaveSession(t, path, child)
	func() {
		connection, err := open(path)
		schemaTestMust(t, err)
		defer connection.Close()
		schemaTestExec(t, connection, `INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES ('session-1','parent','session-1','{}')`)
	}()
	question := eventsTestQuestion(t, "elicitation-1", `{
		"mode":"form", "sessionId":"session-1", "message":"Which architecture?",
		"requestedSchema":{"type":"object", "properties":{"choice":{"type":"string", "enum":["Thin", "Thick"]}}}
	}`)
	schemaTestMust(t, eventsTestPage(t, path, []core.RelayObservation{
		core.RelayObservationElicitationRequested{Request: question},
	}, false))
	count := func(connection Queryer) int64 {
		return schemaTestInt(t, connection, "SELECT count(*) FROM mailbox_outbox")
	}
	func() {
		connection, err := open(path)
		schemaTestMust(t, err)
		defer connection.Close()
		if mail := count(connection); mail != 0 {
			t.Fatalf("outbox %d", mail)
		}
	}()
	// The question stays pending for the stopped child, but reconciliation
	// must not forward it either.
	connection, err := open(path)
	schemaTestMust(t, err)
	defer connection.Close()
	if reconciled, err := reconcilePendingSubagentQuestionsWith(connection); err != nil || reconciled != 0 {
		t.Fatalf("reconciled %d, %v", reconciled, err)
	}
	if mail := count(connection); mail != 0 {
		t.Fatalf("outbox %d", mail)
	}
}

func TestAPIEventsCaptureFreeTextQuestionsResolutionAndErrors(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	record := session("session-1", "project-1")
	schemaTestSaveSession(t, path, record)
	request := eventsTestQuestion(t, "question-1", `{
		"mode": "form", "sessionId": "session-1", "message": "Which directory?",
		"requestedSchema": {"type": "object", "properties": {"directory": {"type": "string"}}, "required": ["directory"]}
	}`)
	observations := eventsTestTurn()
	queued := observations[0].(core.RelayObservationCommandQueued)
	prompt := queued.Command.(core.RelayCommandPrompt)
	prompt.Prompt = append(prompt.Prompt, acpschema.ContentBlock{
		Kind: acpschema.ContentBlockKindText,
		Text: &acpschema.TextContent{Text: "Inspect the chosen directory"},
	})
	queued.Command = prompt
	observations[0] = queued
	reply := request.ReplyText(&core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"directory": core.ElicitationValueString("src")},
	})
	observations = slices.Insert(observations, 2,
		core.RelayObservation(core.RelayObservationElicitationRequested{Request: request}),
		core.RelayObservation(core.RelayObservationElicitationResolved{
			ElicitationID: request.ID,
			Action:        "accept",
			Reply:         &reply,
		}))
	schemaTestMust(t, eventsTestPage(t, path, observations, false))
	failure := "provisioning failed"
	record.LastError = &failure
	schemaTestSaveSession(t, path, record)
	schemaTestSaveSession(t, path, record)
	page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100)
	if kinds := eventsTestKinds(page); !slices.Equal(kinds, []string{
		"turn_started",
		"input_required",
		"input_resolved",
		"turn_ended",
		"session_fault",
	}) {
		t.Fatalf("kinds %v", kinds)
	}
	required, ok := page.Events[1].Event.(core.ApiEventDataInputRequired)
	if !ok {
		t.Fatalf("question event %#v", page.Events[1].Event)
	}
	if required.Request == nil || !reflect.DeepEqual(*required.Request, request) {
		t.Fatalf("request %#v", required.Request)
	}
	if required.TurnID == nil || *required.TurnID != 1 {
		t.Fatalf("turn %v", required.TurnID)
	}
	if resolved, ok := page.Events[2].Event.(core.ApiEventDataInputResolved); !ok || resolved.TurnID == nil || *resolved.TurnID != 1 || resolved.Action != "accept" {
		t.Fatalf("resolution %#v", page.Events[2].Event)
	}
	current := eventsTestMaterialized(t, path, "session-1")
	if len(current.PendingElicitations) != 0 || current.ActiveTurn != nil {
		t.Fatal("the turn is still open")
	}
	var replyItem *core.TranscriptItem
	for _, item := range current.Transcript {
		if strings.HasPrefix(item.StableID, core.ElicitationReplyItemPrefix) {
			replyItem = item
			break
		}
	}
	if replyItem == nil {
		t.Fatal("no reply item")
	}
	if text := transcript.TranscriptItemText(replyItem); text != `Which directory?

directory: src` {
		t.Fatalf("reply %q", text)
	}
	if replyItem.IsTurnStart() {
		t.Fatal("the reply started a turn")
	}
	var turnStart uint64
	found := false
	for _, item := range current.Transcript {
		if item.IsTurnStart() {
			turnStart, found = item.Position, true
			break
		}
	}
	if !found {
		t.Fatal("no turn start")
	}
	if latest, ok := core.LatestCompletedTurnOrdinal(current); !ok || latest != turnStart {
		t.Fatalf("latest completed turn %d, %v", latest, ok)
	}
	connection := schemaTestOpen(t, path)
	if start, err := lastMaterializedTurnStart(connection, "session-1"); err != nil || start == nil || *start != turnStart {
		t.Fatalf("last turn start %v, %v", start, err)
	}
	if position, _, ok, err := lastMaterializedUserMessage(connection, "session-1"); err != nil || !ok || position != turnStart {
		t.Fatalf("last user message %d, %v, %v", position, ok, err)
	}
	if current.LastTurnOutcome == nil || current.LastTurnOutcome.AcceptedOrdinal == nil || *current.LastTurnOutcome.AcceptedOrdinal != 1 {
		t.Fatalf("last turn outcome %#v", current.LastTurnOutcome)
	}
}

// Hard-won: be5abcca: launch failure reasons were absent from the user-visible event stream.
func TestALifecycleSaveThatRecordsALaunchFailureEmitsOneErrorEvent(t *testing.T) {
	// The launch-failure path persists through `save_lifecycle_session`
	// (a plain UPDATE), so prove that path fires the fault transition once so
	// `mj events` shows the reason exactly once, not zero or twice.
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	record := session("session-1", "project-1")
	schemaTestSaveSession(t, path, record)

	record.State = core.SessionStateError
	failure := "worker bootstrap failed: Connection closed by host"
	record.LastError = &failure
	schemaTestMust(t, saveLifecycleSessionTo(path, &record))
	// An unchanged re-save must not add a second event.
	schemaTestMust(t, saveLifecycleSessionTo(path, &record))

	page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100)
	faults := []string{}
	for _, event := range page.Events {
		if fault, ok := event.Event.(core.ApiEventDataSessionFault); ok {
			faults = append(faults, fault.Message)
		}
	}
	if !slices.Equal(faults, []string{failure}) {
		t.Fatalf("a recorded launch failure surfaces as exactly one error event: %v", faults)
	}
}

func TestAPIEventsPreserveCommandIdentityForFailedCompletions(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	observations := eventsTestTurn()
	completed := observations[len(observations)-1].(core.RelayObservationCommandCompleted)
	completed.Outcome = core.RelayCommandOutcomePrompt{
		WorkFacts:  nil,
		Diagnostic: nil,
		StopReason: "provider_error",
		Usage:      nil,
	}
	observations[len(observations)-1] = completed
	schemaTestMust(t, eventsTestPage(t, path, observations, false))
	events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
	if len(events) != 2 {
		t.Fatalf("events %d", len(events))
	}
	ended, ok := events[1].Event.(core.ApiEventDataTurnEnded)
	if !ok || ended.Turn.CommandID != "prompt-1" || ended.Turn.AcceptedOrdinal == nil || *ended.Turn.AcceptedOrdinal != 1 ||
		ended.Turn.Outcome.Kind != core.TurnResultKindFailed ||
		ended.Turn.Outcome.StopReason == nil || *ended.Turn.Outcome.StopReason != "provider_error" {
		t.Fatalf("turn ended %#v", events[1].Event)
	}
}

func TestAPIActivityUpdatesOnlyCurrentStateWithoutRevivingForgottenSessions(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	connection, err := open(path)
	schemaTestMust(t, err)
	idleSince := int64(100)
	idle := core.ApiActivityState{
		State: "running",
		Details: &core.ApiActivityDetails{
			Kind:                  core.ApiActivityKindIdle,
			TurnStartedAtMs:       nil,
			StepStartedAtMs:       nil,
			BackgroundStartedAtMs: nil,
			IdleSinceMs:           &idleSince,
			LastActivityAtMs:      nil,
			Label:                 nil,
		},
		IsIdle:          true,
		WaitingForInput: false,
		CapacityRetry:   false,
	}
	record := func(activity core.ApiActivityState, at int64) {
		t.Helper()
		schemaTestMust(t, recordAPIActivitiesWith(connection, []APIActivityUpdate{{SessionID: "session-1", Activity: activity}}, at))
	}
	record(idle, 100)
	record(idle, 200)
	background := idle
	details := *idle.Details
	background.Details = &details
	background.IsIdle = false
	details.Kind = core.ApiActivityKindBackground
	details.IdleSinceMs = nil
	backgroundStarted := int64(250)
	details.BackgroundStartedAtMs = &backgroundStarted
	record(background, 250)
	background.WaitingForInput = true
	record(background, 300)
	unknown := core.ApiActivityState{
		State:           "disconnected",
		Details:         nil,
		IsIdle:          false,
		WaitingForInput: false,
		CapacityRetry:   false,
	}
	record(unknown, 400)
	schemaTestMust(t, connection.Close())
	if page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100); len(page.Events) != 0 {
		t.Fatalf("activity became events: %v", eventsTestKinds(page))
	}
	connection, err = open(path)
	schemaTestMust(t, err)
	body := schemaTestText(t, connection, `SELECT body FROM api_session_activity WHERE session_id='session-1'`)
	schemaTestMust(t, connection.Close())
	var stored core.ApiActivityState
	schemaTestMust(t, json.Unmarshal([]byte(body), &stored))
	if !reflect.DeepEqual(stored, unknown) {
		t.Fatalf("stored %#v", stored)
	}
	schemaTestMust(t, deleteSessionFrom(path, "session-1"))
	connection, err = open(path)
	schemaTestMust(t, err)
	schemaTestMust(t, recordAPIActivitiesWith(connection, []APIActivityUpdate{{SessionID: "session-1", Activity: idle}}, 500))
	schemaTestMust(t, connection.Close())
	page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100)
	if len(page.Events) != 0 || page.LatestSeq != 0 {
		t.Fatalf("page %#v", page)
	}
}

func TestAPIActivityCleanupIsBoundedRestartableAndKeepsReplayCursors(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	connection, err := open(path)
	schemaTestMust(t, err)
	activity := core.ApiEventDataActivityChanged{
		Activity: core.ApiActivityState{
			State:           "running",
			Details:         nil,
			IsIdle:          false,
			WaitingForInput: false,
			CapacityRetry:   false,
		},
	}
	durable := core.ApiEventDataSessionFault{
		Reason:    core.OutcomeReasonStartupFailed,
		Message:   "activity_changed is only text here",
		CommandID: nil,
	}
	insert := func(at int64, event core.ApiEventData) {
		t.Helper()
		tx, err := connection.Begin()
		schemaTestMust(t, err)
		defer tx.Rollback()
		schemaTestMust(t, insertAPIEvent(tx, "session-1", at, event))
		schemaTestMust(t, tx.Commit())
	}
	prune := func(afterSeq uint64) (uint64, int, bool) {
		t.Helper()
		through, removed, ok, err := pruneAPIActivityEventsWith(connection, afterSeq, 1_000)
		schemaTestMust(t, err)
		return through, removed, ok
	}
	expectPrune := func(afterSeq uint64, through uint64, removed int) {
		t.Helper()
		if gotThrough, gotRemoved, ok := prune(afterSeq); !ok || gotThrough != through || gotRemoved != removed {
			t.Fatalf("prune after %d: (%d, %d, %v), want (%d, %d)", afterSeq, gotThrough, gotRemoved, ok, through, removed)
		}
	}
	tx, err := connection.Begin()
	schemaTestMust(t, err)
	defer tx.Rollback()
	// More than one batch and 64 KiB, with a durable event between two
	// stretches of disposable history and activity at the final frontier.
	for seq := int64(1); seq <= 2_003; seq++ {
		var event core.ApiEventData = activity
		if seq == 1_002 {
			event = durable
		}
		schemaTestMust(t, insertAPIEvent(tx, "session-1", seq, event))
	}
	schemaTestMust(t, tx.Commit())
	before := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 1_000)
	if len(before.Events) != 0 || before.NextAfterSeq != 1_000 {
		t.Fatalf("before cleanup: %d events, next %d", len(before.Events), before.NextAfterSeq)
	}
	expectPrune(0, 1_000, 1_000)
	schemaTestMust(t, connection.Close())

	// Simulate restarting cleanup with only its committed deletes, then
	// append a durable event while later batches are still running.
	connection, err = open(path)
	schemaTestMust(t, err)
	defer connection.Close()
	expectPrune(0, 2_000, 999)
	insert(2_004, durable)
	expectPrune(2_000, 2_004, 3)
	if _, _, ok := prune(2_004); ok {
		t.Fatal("cleanup continued past the frontier")
	}
	history := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100)
	seqs := []uint64{}
	for _, event := range history.Events {
		seqs = append(seqs, event.Seq)
		if !reflect.DeepEqual(event.Event, core.ApiEventData(durable)) {
			t.Fatalf("event %d: %#v", event.Seq, event.Event)
		}
	}
	if !slices.Equal(seqs, []uint64{1_002, 2_004}) {
		t.Fatalf("history %v", seqs)
	}

	// Removing the newest activity must not make an old cursor invalid or
	// let SQLite reuse its ID on the next durable insertion.
	insert(2_005, activity)
	expectPrune(2_004, 2_005, 1)
	tail := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(2_004), 100)
	if len(tail.Events) != 0 || tail.LatestSeq != 2_005 || tail.NextAfterSeq != 2_005 {
		t.Fatalf("tail %#v", tail)
	}
	insert(2_006, durable)
	tail = eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(2_005), 100)
	if len(tail.Events) == 0 || tail.Events[0].Seq != 2_006 {
		t.Fatalf("tail %#v", tail)
	}
}

func TestCheckpointCleanupDoesNotEndATurnOrHideControlFailure(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	schemaTestMust(t, eventsTestPage(t, path, eventsTestTurn()[:2], false))
	for _, c := range []struct {
		id     string
		reason core.OutcomeReason
	}{
		{"arbitrary-one", core.OutcomeReasonControllerDisconnected},
		{"arbitrary-two", core.OutcomeReasonOwnerLostOnRestart},
	} {
		reason := c.reason
		schemaTestMust(t, eventsTestPage(t, path, []core.RelayObservation{
			core.RelayObservationCommandInterrupted{
				ContinuationID: nil,
				CommandID:      c.id,
				Command:        core.RelayCommandKindBeginCheckpoint,
				Reason:         &reason,
				Message:        "diagnostic wording is not a contract",
			},
		}, false))
	}
	failed := core.OutcomeReasonCommandFailed
	schemaTestMust(t, eventsTestPage(t, path, []core.RelayObservation{
		core.RelayObservationCommandRejected{
			CommandID: "arbitrary-three",
			Command:   core.RelayCommandKindCompleteCheckpoint,
			Reason:    &failed,
			Message:   "Cannot save recovery floor",
		},
	}, false))
	current := eventsTestMaterialized(t, path, "session-1")
	if current.ActiveTurn == nil || current.LastTurnOutcome != nil {
		t.Fatal("checkpoint cleanup ended the turn")
	}
	systemText := func(needle string) bool {
		for _, item := range current.Transcript {
			if system, ok := item.Body.(core.TranscriptBodySystem); ok && strings.Contains(system.Text, needle) {
				return true
			}
		}
		return false
	}
	if systemText("diagnostic wording") {
		t.Fatal("a diagnostic reached the transcript")
	}
	if !systemText("Cannot save recovery floor") {
		t.Fatal("the control failure is hidden")
	}
	turn := eventsTestTurn()
	schemaTestMust(t, eventsTestPage(t, path, turn[len(turn)-1:], false))
	events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
	if len(events) != 5 {
		t.Fatalf("events %d", len(events))
	}
	for _, event := range events[1:3] {
		ended, ok := event.Event.(core.ApiEventDataCommandEnded)
		if !ok || ended.Result.Outcome != core.CommandResultKindCancelled || ended.Result.CommandKind != "begin_checkpoint" {
			t.Fatalf("event %#v", event.Event)
		}
	}
	if ended, ok := events[3].Event.(core.ApiEventDataCommandEnded); !ok || ended.Result.Outcome != core.CommandResultKindFailed {
		t.Fatalf("event %#v", events[3].Event)
	}
	if ended, ok := events[4].Event.(core.ApiEventDataTurnEnded); !ok || ended.Turn.Outcome.Kind != core.TurnResultKindCompleted {
		t.Fatalf("event %#v", events[4].Event)
	}
}

func TestTerminalPromptResultsAreSemanticAndNotDuplicateErrorEvents(t *testing.T) {
	prompt := core.RelayCommandKindPrompt
	admission := core.OutcomeReasonAdmissionRejected
	stopped := core.OutcomeReasonRuntimeStopped
	for _, c := range []struct {
		terminal core.RelayObservation
		expected core.TurnResultKind
	}{
		{
			core.RelayObservationCommandCompleted{
				BarrierCommandID: nil,
				CommandID:        "prompt-1",
				Command:          &prompt,
				Outcome: core.RelayCommandOutcomePrompt{
					WorkFacts:  nil,
					StopReason: "Cancelled",
					Usage:      nil,
					Diagnostic: nil,
				},
			},
			core.TurnResultKindCancelled,
		},
		{
			core.RelayObservationCommandCompleted{
				BarrierCommandID: nil,
				CommandID:        "prompt-1",
				Command:          &prompt,
				Outcome: core.RelayCommandOutcomePrompt{
					WorkFacts:  nil,
					StopReason: "QuotaLimit",
					Usage:      nil,
					Diagnostic: nil,
				},
			},
			core.TurnResultKindFailed,
		},
		{
			core.RelayObservationCommandRejected{
				CommandID: "prompt-1",
				Command:   core.RelayCommandKindPrompt,
				Reason:    &admission,
				Message:   "unavailable",
			},
			core.TurnResultKindRejected,
		},
		{
			core.RelayObservationCommandInterrupted{
				ContinuationID: nil,
				CommandID:      "prompt-1",
				Command:        core.RelayCommandKindPrompt,
				Reason:         &stopped,
				Message:        "runtime stopped",
			},
			core.TurnResultKindInterrupted,
		},
	} {
		dir := t.TempDir()
		path := filepath.Join(dir, "events.sqlite")
		schemaTestSaveSession(t, path, session("session-1", "project-1"))
		observations := eventsTestTurn()
		observations[len(observations)-1] = c.terminal
		schemaTestMust(t, eventsTestPage(t, path, observations, false))
		events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
		if len(events) != 2 {
			t.Fatalf("events %d", len(events))
		}
		if ended, ok := events[1].Event.(core.ApiEventDataTurnEnded); !ok || ended.Turn.Outcome.Kind != c.expected || ended.Turn.CommandID != "prompt-1" {
			t.Fatalf("event %#v, want %s", events[1].Event, c.expected)
		}
	}
}

func TestTypedRuntimeFaultDoesNotRewriteACompletedTurn(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	schemaTestMust(t, eventsTestPage(t, path, eventsTestTurn(), false))
	schemaTestMust(t, eventsTestPage(t, path, []core.RelayObservation{
		core.RelayObservationSessionFault{
			Reason:  core.OutcomeReasonRuntimeUnavailable,
			Message: "runtime exited",
		},
	}, false))
	events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
	if len(events) != 3 {
		t.Fatalf("events %d", len(events))
	}
	if ended, ok := events[1].Event.(core.ApiEventDataTurnEnded); !ok || ended.Turn.Outcome.Kind != core.TurnResultKindCompleted {
		t.Fatalf("event %#v", events[1].Event)
	}
	if fault, ok := events[2].Event.(core.ApiEventDataSessionFault); !ok || fault.Reason != core.OutcomeReasonRuntimeUnavailable {
		t.Fatalf("event %#v", events[2].Event)
	}
}

func TestCheckpointRecoveryConsumesAttemptIdentityOnceAndPreservesCorrelation(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	record := session("session-1", "project-1")
	record.State = core.SessionStateRunning
	schemaTestSaveSession(t, path, record)
	record.State = core.SessionStateCheckpointing
	connection, err := open(path)
	schemaTestMust(t, err)
	func() {
		tx, err := connection.Begin()
		schemaTestMust(t, err)
		defer tx.Rollback()
		schemaTestMust(t, beginCheckpointOperationWith(tx, &record, "attempt-1"))
		if beginCheckpointOperationWith(tx, &record, "attempt-2") == nil {
			t.Fatal("a second checkpoint attempt was admitted")
		}
		schemaTestExec(t, tx, `UPDATE checkpoint_operations SET related_command_ids = '["barrier-1"]'`)
		schemaTestMust(t, tx.Commit())
	}()
	schemaTestMust(t, connection.Close())
	if recovered, err := recoverInterruptedCheckpointingSessionsTo(path, "2026-09-27T12:00:00Z"); err != nil || recovered != 1 {
		t.Fatalf("recovered %d, %v", recovered, err)
	}
	if recovered, err := recoverInterruptedCheckpointingSessionsTo(path, "2026-09-27T12:00:01Z"); err != nil || recovered != 0 {
		t.Fatalf("recovered again %d, %v", recovered, err)
	}
	events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
	if len(events) != 1 {
		t.Fatalf("events %d", len(events))
	}
	ended, ok := events[0].Event.(core.ApiEventDataCommandEnded)
	if !ok || ended.Result.CommandID != "attempt-1" || !slices.Equal(ended.Result.RelatedCommandIDs, []string{"barrier-1"}) ||
		ended.Result.Owner != core.CommandOwnerDaemon || ended.Result.Reason == nil ||
		*ended.Result.Reason != core.OutcomeReasonControllerRestarted {
		t.Fatalf("event %#v", events[0].Event)
	}
}

func TestEventMigrationPreservesCursorsAndKeepsUnknownHistoryExplicit(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	schemaTestSaveSession(t, path, session("session-1", "project-1"))
	schemaTestMust(t, eventsTestPage(t, path, eventsTestTurn(), false))
	turn := eventsTestMaterialized(t, path, "session-1").LastTurnOutcome
	if turn == nil {
		t.Fatal("no turn outcome")
	}
	connection := schemaTestOpen(t, path)
	legacyTurn := schemaTestJSON(t, turn)
	schemaTestExec(t, connection, "UPDATE api_events SET body = ?1 WHERE seq = 2",
		`{"type":"turn_ended","data":{"turn":`+legacyTurn+`}}`)
	schemaTestExec(t, connection, "INSERT INTO api_events(seq, session_id, recorded_at_ms, body) VALUES (8, 'session-1', 1234, ?1)", `{"type":"error","data":{"command_id":"arbitrary","message":"old diagnostic"}}`)
	// This fixture predates semantic persisted turns as well as typed events.
	schemaTestExec(t, connection, "UPDATE session_turn_usage SET body=?1", legacyTurn)
	schemaTestExec(t, connection, `UPDATE materialized_sessions SET last_turn_outcome_json=?1 WHERE session_id='session-1'`, legacyTurn)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 56)
	restorePre153TranscriptLayout(t, connection)
	schemaTestExec(t, connection, `DROP TABLE checkpoint_operations; DELETE FROM schema_migrations WHERE version >= 57; UPDATE schema_compatibility SET minimum_compatible_version = 56; DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 56;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)
	page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100)
	if page.LatestSeq != 8 || page.NextAfterSeq != 8 {
		t.Fatalf("page cursors (%d, %d)", page.LatestSeq, page.NextAfterSeq)
	}
	seqs := []uint64{}
	for _, event := range page.Events {
		seqs = append(seqs, event.Seq)
	}
	if !slices.Equal(seqs, []uint64{1, 2, 8}) {
		t.Fatalf("seqs %v", seqs)
	}
	if page.Events[2].RecordedAtMs != 1234 {
		t.Fatalf("recorded at %d", page.Events[2].RecordedAtMs)
	}
	notice, ok := page.Events[2].Event.(core.ApiEventDataLegacyNotice)
	if !ok || notice.OriginalType != "error" || notice.CommandID == nil || *notice.CommandID != "arbitrary" || notice.Message != "old diagnostic" {
		t.Fatalf("notice %#v", page.Events[2].Event)
	}
	if ended, ok := page.Events[1].Event.(core.ApiEventDataTurnEnded); !ok || ended.Turn.Outcome.Kind != core.TurnResultKindCompleted {
		t.Fatalf("turn ended %#v", page.Events[1].Event)
	}
}

func TestRequestedCheckpointResultCommitsWithMetadataAndOnlyForItsOwner(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.sqlite")
	record := session("session-1", "project-1")
	record.State = core.SessionStateRunning
	previousCheckpoint := record.Checkpoint
	schemaTestSaveSession(t, path, record)
	connection, err := open(path)
	schemaTestMust(t, err)
	defer connection.Close()
	record.State = core.SessionStateCheckpointing
	func() {
		tx, err := connection.Begin()
		schemaTestMust(t, err)
		defer tx.Rollback()
		schemaTestMust(t, beginCheckpointOperationWith(tx, &record, "owner"))
		schemaTestMust(t, tx.Commit())
	}()
	record.State = core.SessionStateRunning
	record.Checkpoint = &core.CheckpointMetadata{
		ArchivePath:   filepath.Join(dir, "verified.tar"),
		Sha256:        strings.Repeat("a", 64),
		CreatedAt:     "2026-09-27T12:00:00Z",
		EventFrontier: 0,
	}
	func() {
		tx, err := connection.Begin()
		schemaTestMust(t, err)
		// Rollback undoes both metadata and the terminal event.
		defer tx.Rollback()
		if saveRequestedCheckpointWith(tx, &record, "stale-owner") == nil {
			t.Fatal("a stale owner saved the checkpoint")
		}
		schemaTestMust(t, finishFailedCheckpointWith(tx, &record, "stale-owner", false, "late failure"))
		if count := schemaTestInt(t, tx, "SELECT count(*) FROM api_events"); count != 0 {
			t.Fatalf("events %d", count)
		}
		schemaTestMust(t, saveRequestedCheckpointWith(tx, &record, "owner"))
	}()
	if checkpoint := schemaTestSession(t, schemaTestLoadState(t, path), "session-1").Checkpoint; !reflect.DeepEqual(checkpoint, previousCheckpoint) {
		t.Fatalf("checkpoint %#v", checkpoint)
	}
	if page := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100); len(page.Events) != 0 {
		t.Fatalf("events %v", eventsTestKinds(page))
	}
	func() {
		tx, err := connection.Begin()
		schemaTestMust(t, err)
		defer tx.Rollback()
		schemaTestMust(t, saveRequestedCheckpointWith(tx, &record, "owner"))
		schemaTestMust(t, finishFailedCheckpointWith(tx, &record, "owner", false, "late cleanup failure"))
		schemaTestMust(t, tx.Commit())
	}()
	events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
	if len(events) != 1 {
		t.Fatalf("events %d", len(events))
	}
	if ended, ok := events[0].Event.(core.ApiEventDataCommandEnded); !ok || ended.Result.Outcome != core.CommandResultKindSucceeded || ended.Result.CommandID != "owner" {
		t.Fatalf("event %#v", events[0].Event)
	}
	if checkpoint := schemaTestSession(t, schemaTestLoadState(t, path), "session-1").Checkpoint; !reflect.DeepEqual(checkpoint, record.Checkpoint) {
		t.Fatalf("checkpoint %#v", checkpoint)
	}
}

func TestCheckpointFailureAndDeferralAreDistinctAndPreservePreviousArchive(t *testing.T) {
	for _, deferred := range []bool{false, true} {
		dir := t.TempDir()
		path := filepath.Join(dir, "events.sqlite")
		record := session("session-1", "project-1")
		record.State = core.SessionStateRunning
		record.Checkpoint = &core.CheckpointMetadata{
			ArchivePath:   filepath.Join(dir, "previous.tar"),
			Sha256:        strings.Repeat("a", 64),
			CreatedAt:     "2026-09-27T12:00:00Z",
			EventFrontier: 0,
		}
		schemaTestSaveSession(t, path, record)
		previous := record.Checkpoint
		previousWarning := record.LastCheckpointError
		connection, err := open(path)
		schemaTestMust(t, err)
		tx, err := connection.Begin()
		schemaTestMust(t, err)
		defer tx.Rollback()
		record.State = core.SessionStateCheckpointing
		schemaTestMust(t, beginCheckpointOperationWith(tx, &record, "owner"))
		record.State = core.SessionStateRunning
		if !deferred {
			warning := "archive verification failed"
			record.LastCheckpointError = &warning
		}
		schemaTestMust(t, finishFailedCheckpointWith(tx, &record, "owner", deferred, "diagnostic"))
		schemaTestMust(t, finishFailedCheckpointWith(tx, &record, "owner", deferred, "diagnostic"))
		schemaTestMust(t, tx.Commit())
		schemaTestMust(t, connection.Close())
		loaded := schemaTestSession(t, schemaTestLoadState(t, path), "session-1")
		if !reflect.DeepEqual(loaded.Checkpoint, previous) {
			t.Fatalf("checkpoint %#v", loaded.Checkpoint)
		}
		expectedWarning := previousWarning
		if !deferred {
			warning := "archive verification failed"
			expectedWarning = &warning
		}
		if !reflect.DeepEqual(loaded.LastCheckpointError, expectedWarning) {
			t.Fatalf("warning %v", loaded.LastCheckpointError)
		}
		events := eventsTestLoad(t, path, core.ApiEventFilter{}, eventsTestSeq(0), 100).Events
		if len(events) != 1 {
			t.Fatalf("events %d", len(events))
		}
		expected := core.CommandResultKindFailed
		if deferred {
			expected = core.CommandResultKindRejected
		}
		if ended, ok := events[0].Event.(core.ApiEventDataCommandEnded); !ok || ended.Result.Outcome != expected {
			t.Fatalf("event %#v", events[0].Event)
		}
	}
}
