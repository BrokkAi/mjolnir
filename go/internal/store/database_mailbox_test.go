// Port of the tests in mj-store/src/database/mailbox.rs.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func mailboxTestEventJSON(t *testing.T, event core.MailboxEvent) string {
	t.Helper()
	encoded, err := json.Marshal(event)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func mailboxTestExec(t *testing.T, conn Queryer, query string, args ...any) {
	t.Helper()
	if _, err := conn.Exec(query, args...); err != nil {
		t.Fatal(err)
	}
}

func mailboxTestState(t *testing.T, conn Queryer, sessionID, key string) MailboxDeliveryState {
	t.Helper()
	state, err := mailboxDeliveryStateWith(conn, sessionID, key)
	if err != nil {
		t.Fatal(err)
	}
	return state
}

// Hard-won: b0a5dd7c0: final startup cleanup could mask an already accepted worker message while its receipt write waited.
func TestWorkerReceiptWinsStartupArchiveInEitherTransactionOrder(t *testing.T) {
	for _, archiveFirst := range []bool{true, false} {
		path := filepath.Join(t.TempDir(), "store.sqlite3")
		record := session("child", "project")
		record.State = core.SessionStateProvisioning
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
		connection, err := open(path)
		if err != nil {
			t.Fatal(err)
		}
		event := core.MailboxEvent{
			Key:         "subagent-message-accepted",
			Source:      "parent",
			Wake:        true,
			CreatedAtMs: 1,
			Body:        core.MailboxEventBodyParentMessage{Text: "consumed by worker before cleanup"},
		}
		if _, err := EnqueueMailboxEventWith(connection, event.Key, record.ID, mailboxTestEventJSON(t, event), true, true); err != nil {
			t.Fatal(err)
		}
		// The relay has accepted ordinal 42. Its daemon acknowledgement and
		// the non-retry cleanup transaction can reach the writer in either
		// order.
		if !archiveFirst {
			if err := markMailboxEventAcceptedWith(connection, event.Key, "worker-receipt", 42); err != nil {
				t.Fatal(err)
			}
		}
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			t.Fatal(err)
		}
		mailboxTestExec(t, tx, "UPDATE sessions SET state='error',archived=1,last_error='startup cancelled' WHERE session_id=?1", record.ID)
		if err := settleFailedParentMessagesWith(tx, &record); err != nil {
			t.Fatal(err)
		}
		if err := tx.Commit(); err != nil {
			t.Fatal(err)
		}
		if archiveFirst {
			if _, failed := mailboxTestState(t, connection, record.ID, event.Key).(MailboxDeliveryStateFailed); !failed {
				t.Fatal("cleanup first must fail the pending message")
			}
			if err := markMailboxEventAcceptedWith(connection, event.Key, "worker-receipt", 42); err != nil {
				t.Fatal(err)
			}
		}
		if state := mailboxTestState(t, connection, record.ID, event.Key); state != (MailboxDeliveryStateAccepted{Value: 42}) {
			t.Fatalf("archive first: %v: %#v", archiveFirst, state)
		}
		// Neither repeated cleanup nor a later receipt rewrites a recorded
		// receipt.
		if err := settleFailedParentMessagesWith(connection, &record); err != nil {
			t.Fatal(err)
		}
		if err := markMailboxEventAcceptedWith(connection, event.Key, "later-receipt", 99); err != nil {
			t.Fatal(err)
		}
		var commandID *string
		var ordinal *uint64
		var failure *string
		if err := connection.QueryRow("SELECT accepted_command_id,accepted_ordinal,failure FROM mailbox_outbox WHERE event_key=?1", event.Key).
			Scan(&commandID, &ordinal, &failure); err != nil {
			t.Fatal(err)
		}
		if commandID == nil || *commandID != "worker-receipt" || ordinal == nil || *ordinal != 42 || failure != nil {
			t.Fatalf("receipt = (%v, %v, %v)", commandID, ordinal, failure)
		}
		_ = connection.Close()
	}
}

func mailboxTestEnqueue(t *testing.T, connection Queryer, sessionID, key string) {
	t.Helper()
	event := core.MailboxEvent{
		Key:         key,
		Source:      "test",
		Wake:        false,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyPlainText{Text: key},
	}
	if _, err := EnqueueMailboxEventWith(connection, key, sessionID, mailboxTestEventJSON(t, event), false, false); err != nil {
		t.Fatal(err)
	}
}

func mailboxTestEnqueueQuestion(t *testing.T, connection Queryer, parent, child, key string) {
	t.Helper()
	event := core.MailboxEvent{
		Key:         key,
		Source:      "session_message",
		Wake:        true,
		CreatedAtMs: 1,
		Body: core.MailboxEventBodySessionMessage{
			From: core.Sender{Type: core.SenderSession, ID: child, Title: child},
			Text: fmt.Sprintf("Your sub-agent asked this question: pick one. Answer with send_message to session_id %s.", child),
		},
	}
	if _, err := EnqueueMailboxEventWith(connection, key, parent, mailboxTestEventJSON(t, event), true, true); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: #1345: a forwarded question outlived the child that could
// receive its answer; delivery then invited a send_message that failed.
//
// Pending questions from children that stopped, closed, or vanished are
// pruned; a live child's question and accepted delivery history stay.
func TestStaleChildQuestionsArePrunedButLiveAndAcceptedOnesStay(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	parent := session("parent", "project")
	parent.State = core.SessionStateRunning
	if err := SaveSessionTo(path, &parent); err != nil {
		t.Fatal(err)
	}
	for _, child := range []struct {
		id    string
		state core.SessionState
	}{
		{"running-child", core.SessionStateRunning},
		{"stopped-child", core.SessionStateStopped},
		{"gone-child", core.SessionStateRunning},
	} {
		record := session(child.id, "project")
		record.State = child.state
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	mailboxTestEnqueueQuestion(t, connection, "parent", "running-child", "subagent-question-running-child-1")
	mailboxTestEnqueueQuestion(t, connection, "parent", "stopped-child", "subagent-question-stopped-child-1")
	mailboxTestEnqueueQuestion(t, connection, "parent", "gone-child", "subagent-question-gone-child-1")
	mailboxTestEnqueueQuestion(t, connection, "parent", "stopped-child", "subagent-question-stopped-child-2")
	// Accepted delivery history is not pruned.
	mailboxTestExec(t, connection, "UPDATE mailbox_outbox SET accepted_at='now' WHERE event_key='subagent-question-stopped-child-2'")
	// The child row disappearing entirely is terminal too. The deletion
	// trigger removes rows addressed TO it; questions FROM it remain.
	mailboxTestExec(t, connection, "DELETE FROM sessions WHERE session_id='gone-child'")
	removed, err := pruneStaleSubagentQuestionsWith(connection)
	if err != nil {
		t.Fatal(err)
	}
	if removed != 2 {
		t.Fatalf("removed %d, want 2", removed)
	}
	remaining := func(pattern string) uint64 {
		var count uint64
		if err := connection.QueryRow("SELECT count(*) FROM mailbox_outbox WHERE event_key LIKE ?1", pattern).Scan(&count); err != nil {
			t.Fatal(err)
		}
		return count
	}
	if got := remaining("subagent-question-running-child-%"); got != 1 {
		t.Fatalf("running child questions = %d", got)
	}
	if got := remaining("subagent-question-stopped-child-%"); got != 1 {
		t.Fatalf("stopped child questions = %d", got)
	}
	if got := remaining("subagent-question-gone-child-%"); got != 0 {
		t.Fatalf("gone child questions = %d", got)
	}
}

func mailboxCompleteClear(path string, fail bool) error {
	materialized, err := LoadMaterializedSessionFrom(path, "cleared")
	if err != nil {
		return err
	}
	if materialized == nil {
		return errors.New("cleared session has no projection")
	}
	commandID := fmt.Sprintf("clear-%d", materialized.AppliedEventOrdinal+1)
	command := core.RelayCommandKindClearContext
	event := core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        materialized.AppliedEventOrdinal + 1,
		PreviousDigest: materialized.AppliedEventDigest,
		RecordedAtMs:   10,
		CommandID:      &commandID,
		Observation: core.RelayObservationCommandCompleted{
			CommandID: commandID,
			Command:   &command,
			Outcome:   core.RelayCommandOutcomeContextCleared{NativeSessionID: "fresh"},
		},
	}
	event.Digest, err = core.RelayEventDigest(&event)
	if err != nil {
		return err
	}
	projected, err := transcript.ProjectRelayEvent(materialized, &event)
	if err != nil {
		return err
	}
	_, err = applyProjectionPageTo(path, "cleared", func(page *ProjectionPage) (struct{}, error) {
		if _, err := page.Apply(event.Ordinal, event.PreviousDigest, event.Digest, &projected.Mutation); err != nil {
			return struct{}{}, err
		}
		if fail {
			return struct{}{}, errors.New("injected clear rollback")
		}
		return struct{}{}, nil
	})
	return err
}

// Hard-won: 2b74434f9: queued daemon events survived /clear and historical resets could discard newer mail.
//
// A session's mailbox and GitHub rows go in the transaction that removes the
// session, whether it is deleted or destroyed, and a late write for a session
// that is gone stores nothing. No sweep is left to race writers.
func TestRowsOfAGoneSessionCannotOutliveIt(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	for _, id := range []string{"deleted", "destroyed", "kept"} {
		record := session(id, "project")
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	rows := func(sessionID string) int64 {
		var count int64
		if err := connection.QueryRow(`SELECT (SELECT count(*) FROM mailbox_outbox WHERE target_session_id = ?1)
                          + (SELECT count(*) FROM github_watch_items WHERE creator_session_id = ?1)
                          + (SELECT count(*) FROM github_watch_classifications WHERE session_id = ?1)`, sessionID).Scan(&count); err != nil {
			t.Fatal(err)
		}
		return count
	}
	for number, sessionID := range []string{"deleted", "destroyed", "kept"} {
		mailboxTestEnqueue(t, connection, sessionID, "message-"+sessionID)
		mailboxTestExec(t, connection, `INSERT INTO github_watch_items(owner, repo, number, creator_session_id, kind,
                         title, url, created_at, timeline_at, timeline_id, review_comments_at,
                         review_comments_id, processed_at)
                     VALUES ('acme', 'repo', ?1, ?2, 'issue', 't', 'u', 'c', 'c', 0, 'c', 0, 'c')`, int64(number), sessionID)
		mailboxTestExec(t, connection, `INSERT INTO github_watch_classifications(owner, repo, number, session_id,
                         kind, title, url, created_at, interested, created)
                     VALUES ('acme', 'repo', ?1, ?2, 'issue', 't', 'u', 'c', 1, 1)`, int64(number), sessionID)
		if got := rows(sessionID); got != 3 {
			t.Fatalf("%s: %d rows", sessionID, got)
		}
	}

	mailboxTestExec(t, connection, "DELETE FROM sessions WHERE session_id = 'deleted'")
	mailboxTestExec(t, connection, "UPDATE sessions SET state = 'destroyed-with-data-loss' WHERE session_id = 'destroyed'")
	if got := rows("deleted"); got != 0 {
		t.Fatalf("deleted: %d rows", got)
	}
	if got := rows("destroyed"); got != 0 {
		t.Fatalf("destroyed: %d rows", got)
	}
	if got := rows("kept"); got != 3 {
		t.Fatalf("kept: %d rows", got)
	}

	event := core.MailboxEvent{
		Key:         "late",
		Source:      "test",
		Wake:        false,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyPlainText{Text: "late"},
	}
	for _, gone := range []string{"deleted", "destroyed", "never-existed"} {
		inserted, err := EnqueueMailboxEventWith(connection, "late", gone, mailboxTestEventJSON(t, event), false, false)
		if err != nil {
			t.Fatal(err)
		}
		if inserted != (EnqueuedTargetGone{}) {
			t.Fatalf("%s: %#v", gone, inserted)
		}
		if got := rows(gone); got != 0 {
			t.Fatalf("%s: %d rows", gone, got)
		}
	}
}

// Revision 130: a watch outlives its creator while a live session is
// interested in the item, and goes with its last follower.
func TestAWatchOutlivesItsCreatorWhileALiveSessionIsInterested(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	for _, id := range []string{"creator", "creator-alone", "follower", "second", "idle"} {
		record := session(id, "project")
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	watch := func(number int64, creator *string) {
		mailboxTestExec(t, connection, `INSERT INTO github_watch_items(owner, repo, number, creator_session_id, kind,
                         title, url, created_at, timeline_at, timeline_id, review_comments_at,
                         review_comments_id, processed_at)
                     VALUES ('acme', 'repo', ?1, ?2, 'pull_request', 't', 'u', 'c', 'c', 0, 'c', 0, 'c')`, number, creator)
	}
	classify := func(number int64, sessionID string, created bool) {
		mailboxTestExec(t, connection, `INSERT INTO github_watch_classifications(owner, repo, number, session_id,
                         kind, title, url, created_at, interested, created)
                     VALUES ('acme', 'repo', ?1, ?2, 'pull_request', 't', 'u', 'c', 1, ?3)`, number, sessionID, created)
	}
	// creatorOf is the watch's creator: found reports whether the watch
	// exists, creator is nil for a creator-less one.
	type creatorOf struct {
		found   bool
		creator *string
	}
	creator := func(number int64) creatorOf {
		var result creatorOf
		err := connection.QueryRow("SELECT creator_session_id FROM github_watch_items WHERE number = ?1", number).Scan(&result.creator)
		if errors.Is(err, sql.ErrNoRows) {
			return creatorOf{}
		}
		if err != nil {
			t.Fatal(err)
		}
		result.found = true
		return result
	}
	noCreator := creatorOf{found: true}
	gone := creatorOf{}
	name := func(text string) *string { return &text }
	// 1: a creator and a follower. 2: a creator and nobody else.
	// 3: no creator, two followers. 4: no creator, one follower.
	watch(1, name("creator"))
	classify(1, "creator", true)
	classify(1, "follower", false)
	watch(2, name("creator-alone"))
	classify(2, "creator-alone", true)
	watch(3, nil)
	classify(3, "follower", false)
	classify(3, "second", false)
	watch(4, nil)
	classify(4, "second", false)

	// Deleted: the follower keeps watch 1; watch 2 has no one left.
	mailboxTestExec(t, connection, "DELETE FROM sessions WHERE session_id = 'creator'")
	if got := creator(1); !reflect.DeepEqual(got, noCreator) {
		t.Fatalf("watch 1: %#v", got)
	}
	mailboxTestExec(t, connection, "UPDATE sessions SET state = 'destroyed-with-data-loss' WHERE session_id = 'creator-alone'")
	if got := creator(2); !reflect.DeepEqual(got, gone) {
		t.Fatalf("watch 2: %#v", got)
	}

	// Destroyed: a creator-less watch stays while one follower is left and
	// goes with the last one.
	mailboxTestExec(t, connection, "UPDATE sessions SET state = 'destroyed-with-data-loss' WHERE session_id = 'second'")
	if got := creator(3); !reflect.DeepEqual(got, noCreator) {
		t.Fatalf("the other follower is still live: %#v", got)
	}
	if got := creator(4); !reflect.DeepEqual(got, gone) {
		t.Fatalf("its only follower is gone: %#v", got)
	}
	mailboxTestExec(t, connection, "DELETE FROM sessions WHERE session_id = 'follower'")
	if got := creator(1); !reflect.DeepEqual(got, gone) {
		t.Fatalf("watch 1: %#v", got)
	}
	if got := creator(3); !reflect.DeepEqual(got, gone) {
		t.Fatalf("watch 3: %#v", got)
	}

	// A watch with no creator can still be inserted; one for a creator that
	// is gone cannot.
	watch(5, nil)
	watch(6, name("creator"))
	if got := creator(5); !reflect.DeepEqual(got, noCreator) {
		t.Fatalf("watch 5: %#v", got)
	}
	if got := creator(6); !reflect.DeepEqual(got, gone) {
		t.Fatalf("watch 6: %#v", got)
	}
}

func TestClearMailboxCommitsWithProjectionAndReplayPreservesNewerEvents(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	for _, id := range []string{"cleared", "other"} {
		record := session(id, "project")
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	mailboxTestEnqueue(t, connection, "cleared", "before")
	mailboxTestEnqueue(t, connection, "cleared", "during")
	mailboxTestEnqueue(t, connection, "other", "other-message")
	if err := mailboxCompleteClear(path, true); err == nil {
		t.Fatal("the injected failure must roll the clear back")
	}
	if state := mailboxTestState(t, connection, "cleared", "before"); state != (MailboxDeliveryStatePending{}) {
		t.Fatalf("before: %#v", state)
	}
	if err := mailboxCompleteClear(path, false); err != nil {
		t.Fatal(err)
	}
	for _, key := range []string{"before", "during"} {
		if state := mailboxTestState(t, connection, "cleared", key); state != (MailboxDeliveryStateDiscarded{}) {
			t.Fatalf("%s: %#v", key, state)
		}
	}
	// A late acknowledgement cannot overwrite the terminal discard.
	if err := markMailboxEventAcceptedWith(connection, "before", "late-ack", 99); err != nil {
		t.Fatal(err)
	}
	if state := mailboxTestState(t, connection, "cleared", "before"); state != (MailboxDeliveryStateDiscarded{}) {
		t.Fatalf("before after late ack: %#v", state)
	}
	mailboxTestEnqueue(t, connection, "cleared", "after")
	_ = connection.Close()
	empty := core.MaterializedSessionEmpty("cleared")
	if err := SaveMaterializedSessionTo(path, &empty); err != nil {
		t.Fatal(err)
	}
	if err := mailboxCompleteClear(path, false); err != nil {
		t.Fatal(err)
	}
	connection, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if state := mailboxTestState(t, connection, "cleared", "after"); state != (MailboxDeliveryStatePending{}) {
		t.Fatalf("after: %#v", state)
	}
	if state := mailboxTestState(t, connection, "other", "other-message"); state != (MailboxDeliveryStatePending{}) {
		t.Fatalf("other-message: %#v", state)
	}
	if err := mailboxCompleteClear(path, false); err != nil {
		t.Fatal(err)
	}
	if state := mailboxTestState(t, connection, "cleared", "after"); state != (MailboxDeliveryStateDiscarded{}) {
		t.Fatalf("after second clear: %#v", state)
	}
	mailboxTestEnqueue(t, connection, "cleared", "before")
	if state := mailboxTestState(t, connection, "cleared", "before"); state != (MailboxDeliveryStateDiscarded{}) {
		t.Fatalf("before re-enqueued: %#v", state)
	}
}
