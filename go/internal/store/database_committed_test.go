// Port of the tests in mj-store/src/database/committed.rs.

package store

import (
	"encoding/json"
	"errors"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// committedTestWriter starts a writer on path for one test, without
// installing it for the process.
func committedTestWriter(t *testing.T, path string) *DatabaseWriterOwner {
	t.Helper()
	owner, err := startDatabaseWriterAt(path, false)
	if err != nil {
		t.Fatal(err)
	}
	return owner
}

// committedExecute runs operation as one writer job.
func committedExecute(writer *DatabaseWriter, label string, operation func(connection *Connection) error) error {
	_, err := DatabaseWriterExecute(writer, label, func(connection *Connection) (struct{}, error) {
		return struct{}{}, operation(connection)
	})
	return err
}

func committedSnapshot(t *testing.T, writer *DatabaseWriter) *CommittedState {
	t.Helper()
	state, err := writer.CommittedState()
	if err != nil {
		t.Fatal(err)
	}
	return state
}

func committedSaveSessions(t *testing.T, path string, ids ...string) {
	t.Helper()
	for _, id := range ids {
		record := session(id, "project")
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
}

func TestBindingChangesPublishIndependentlyOfSessionAndRelationRows(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "owner-a", "owner-b", "member")
	owner := committedTestWriter(t, path)
	before := committedSnapshot(t, owner.writer)
	memberRecord := before.State.Sessions.GetShared("member")
	ownerA := before.State.EnvironmentOwners.GetShared("owner-a")
	if memberRecord == nil || ownerA == nil {
		t.Fatal("bootstrap lost the stored sessions")
	}

	if err := committedExecute(owner.writer, "remove binding", func(connection *Connection) error {
		_, err := connection.Exec("DELETE FROM environment_bindings WHERE session_id='member'")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	unbound := committedSnapshot(t, owner.writer)
	if _, known := unbound.State.OwnsEnvironment("member"); known {
		t.Fatal("an unbound session must have no known environment ownership")
	}
	if err := committedExecute(owner.writer, "insert binding", func(connection *Connection) error {
		_, err := connection.Exec(`INSERT INTO environment_bindings(session_id, environment_id)
                 SELECT 'member', environment_id FROM environment_bindings WHERE session_id='owner-a'`)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	withA := committedSnapshot(t, owner.writer)
	if id, ok := withA.State.EnvironmentOwnerID("member"); !ok || id != "owner-a" {
		t.Fatalf("member owner = %q, %v", id, ok)
	}
	rebind := func(connection Queryer) error {
		_, err := connection.Exec(`UPDATE environment_bindings SET environment_id=(
                     SELECT environment_id FROM environment_bindings WHERE session_id='owner-b'
                 ) WHERE session_id='member'`)
		return err
	}
	rolledBack := committedExecute(owner.writer, "roll back binding", func(connection *Connection) error {
		transaction, err := connection.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		if err := rebind(transaction); err != nil {
			return err
		}
		return errors.New("fixture rollback")
	})
	if rolledBack == nil {
		t.Fatal("the rolled back job must fail")
	}
	if committedSnapshot(t, owner.writer) != withA {
		t.Fatal("a rolled back write must not publish")
	}
	if err := committedExecute(owner.writer, "update binding", func(connection *Connection) error {
		return rebind(connection)
	}); err != nil {
		t.Fatal(err)
	}
	withB := committedSnapshot(t, owner.writer)
	if id, ok := withB.State.EnvironmentOwnerID("member"); !ok || id != "owner-b" {
		t.Fatalf("member owner = %q, %v", id, ok)
	}
	if withB.State.Sessions.GetShared("member") != memberRecord {
		t.Fatal("a binding change must not republish the session record")
	}
	if withB.State.EnvironmentOwners.GetShared("owner-a") != ownerA {
		t.Fatal("an untouched owner must stay shared")
	}
	loaded, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(withB.State.EnvironmentOwners, loaded.EnvironmentOwners) {
		t.Fatal("published owners differ from the store")
	}
	if err := owner.Shutdown(); err != nil {
		t.Fatal(err)
	}
}

func TestCompactTurnPublicationsAreCommittedAndSessionSpecific(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "first", "second")
	owner := committedTestWriter(t, path)
	before := committedSnapshot(t, owner.writer)
	second := before.Turns.GetShared("second")
	if err := committedExecute(owner.writer, "transcript frontier only", func(connection *Connection) error {
		_, err := connection.Exec("UPDATE materialized_sessions SET applied_event_ordinal=99 WHERE session_id='first'")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if committedSnapshot(t, owner.writer) != before {
		t.Fatal("transcript updates do not change compact wait facts")
	}
	rollback := committedExecute(owner.writer, "rollback turn", func(connection *Connection) error {
		transaction, err := connection.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		if _, err := transaction.Exec("UPDATE materialized_sessions SET execution_state='running',running_started_at_ms=7 WHERE session_id='first'"); err != nil {
			return err
		}
		return errors.New("rolled back")
	})
	if rollback == nil {
		t.Fatal("the rolled back job must fail")
	}
	if committedSnapshot(t, owner.writer) != before {
		t.Fatal("a rolled back turn must not publish")
	}
	if err := committedExecute(owner.writer, "start first turn", func(connection *Connection) error {
		_, err := connection.Exec("UPDATE materialized_sessions SET execution_state='running',running_started_at_ms=7 WHERE session_id='first'")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	running := committedSnapshot(t, owner.writer)
	previousFirst, _ := before.WaitRevisions.Get("first")
	if *running.WaitRevisions.Index("first") == previousFirst {
		t.Fatal("the started session's wait revision must advance")
	}
	if !reflect.DeepEqual(running.WaitRevisions.GetShared("second"), before.WaitRevisions.GetShared("second")) {
		t.Fatal("the other session's wait revision must not move")
	}
	if running.Turns.GetShared("second") != second {
		t.Fatal("the other session's turn must stay shared")
	}
	if err := owner.Shutdown(); err != nil {
		t.Fatal(err)
	}
	owner = committedTestWriter(t, path)
	if !reflect.DeepEqual(committedSnapshot(t, owner.writer).Turns, running.Turns) {
		t.Fatal("a restarted writer must publish the same turns")
	}
	if err := committedExecute(owner.writer, "delete projection", func(connection *Connection) error {
		_, err := connection.Exec("DELETE FROM materialized_sessions WHERE session_id='first'")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if committedSnapshot(t, owner.writer).Turns.ContainsKey("first") {
		t.Fatal("a deleted projection must not stay published")
	}
	if !running.Turns.ContainsKey("first") {
		t.Fatal("a held snapshot must keep its turn")
	}
}

func TestCommittedFormTimeUsesTheRecordedRaiseAcrossUpdatesAndRestart(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "session")
	owner := committedTestWriter(t, path)
	for _, step := range []struct{ raisedAt, expected int64 }{{11, 11}, {99, 11}, {111, 111}} {
		raisedAt := step.raisedAt
		if err := committedExecute(owner.writer, "record request and activity", func(connection *Connection) error {
			tx, err := connection.Begin()
			if err != nil {
				return err
			}
			defer tx.Rollback()
			request := core.ElicitationRequest{
				ID:      "form",
				Message: strconv.FormatInt(min(raisedAt, 99), 10),
				Fields:  []core.ElicitationField{},
			}
			if raisedAt == 111 {
				if err := insertAPIEvent(tx, "session", 110, core.ApiEventDataInputResolved{
					ElicitationID: request.ID,
					Action:        "submit",
				}); err != nil {
					return err
				}
			}
			encoded, err := json.Marshal([]*core.ElicitationRequest{&request})
			if err != nil {
				return err
			}
			if _, err := tx.Exec(`UPDATE materialized_sessions SET pending_elicitations_json = ?1,
                             last_activity_at_ms = ?2 WHERE session_id = 'session'`, string(encoded), raisedAt); err != nil {
				return err
			}
			if err := insertAPIEvent(tx, "session", raisedAt, core.ApiEventDataInputRequired{Request: &request}); err != nil {
				return err
			}
			return tx.Commit()
		}); err != nil {
			t.Fatal(err)
		}
		state := committedSnapshot(t, owner.writer)
		if got := state.Turns.Index("session").PendingFormTimes["form"]; got != step.expected {
			t.Fatalf("form time after raise at %d = %d, want %d", raisedAt, got, step.expected)
		}
	}
	if err := owner.Shutdown(); err != nil {
		t.Fatal(err)
	}
	owner = committedTestWriter(t, path)
	if got := committedSnapshot(t, owner.writer).Turns.Index("session").PendingFormTimes["form"]; got != 111 {
		t.Fatalf("restarted form time = %d", got)
	}
}

func TestPublicationFailureStopsMutationsWithoutReplayingTheCommittedWrite(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "selected")
	owner := committedTestWriter(t, path)
	err := committedExecute(owner.writer, "invalid committed record", func(connection *Connection) error {
		_, err := connection.Exec("UPDATE sessions SET resource_allocation='[]'")
		return err
	})
	if err == nil || !strings.Contains(err.Error(), "do not replay") {
		t.Fatalf("publication failure = %v", err)
	}
	if _, err := owner.writer.CommittedState(); err == nil {
		t.Fatal("a failed publication must stay failed")
	}
	if err := committedExecute(owner.writer, "must not execute", func(*Connection) error {
		panic("a failed publication must close mutation service")
	}); err == nil {
		t.Fatal("a failed publication must refuse further writes")
	}
	connection, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	var stored string
	if err := connection.QueryRow("SELECT resource_allocation FROM sessions WHERE session_id='selected'").Scan(&stored); err != nil {
		t.Fatal(err)
	}
	connection.Close()
	if stored != "[]" {
		t.Fatalf("publication failure cannot undo or replay a commit: %q", stored)
	}
	if owner.Shutdown() == nil {
		t.Fatal("shutting down a failed writer must report its failure")
	}
}

func TestSecondaryConnectionsPublishCommittedRecordsBeforeTheWriteReply(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	owner := committedTestWriter(t, path)
	before := committedSnapshot(t, owner.writer)
	record := session("created", "project")
	saved := record
	if err := committedExecute(owner.writer, "create on secondary connection", func(*Connection) error {
		return SaveSessionTo(path, &saved)
	}); err != nil {
		t.Fatal(err)
	}
	after := committedSnapshot(t, owner.writer)
	if !before.State.Sessions.IsEmpty() {
		t.Fatal("the store started with sessions")
	}
	if !reflect.DeepEqual(*after.State.Sessions.Index("created"), record) {
		t.Fatalf("published record = %+v", *after.State.Sessions.Index("created"))
	}
	if after.Sequence != before.Sequence+1 {
		t.Fatalf("sequence = %d after %d", after.Sequence, before.Sequence)
	}
}

// A wait reads a session's startup status and a child's report from the
// published records, so every write to them must publish, keyed to the
// session it changed and equal to what the store holds.
func TestStartupGroupsAndSubagentReportsArePublishedPerSession(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	owner := committedTestWriter(t, path)
	before := committedSnapshot(t, owner.writer)
	if !before.StartupGroups.IsEmpty() || !before.SubagentReports.IsEmpty() {
		t.Fatal("a new store has no startup groups or reports")
	}

	if err := committedExecute(owner.writer, "queue startup", func(connection *Connection) error {
		_, err := connection.Exec(`INSERT INTO startup_steps(session_id,group_id,command_id,step_json,phase)
                     VALUES ('first','group-1','first:prompt','{}','pending'),
                            ('second','group-2','second:prompt','{}','pending')`)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	queued := committedSnapshot(t, owner.writer)
	if queued.Sequence != before.Sequence+1 {
		t.Fatalf("sequence = %d after %d", queued.Sequence, before.Sequence)
	}
	reader, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, sessionID := range []string{"first", "second"} {
		stored, err := loadLatestStartupStatusWith(reader, sessionID)
		if err != nil || stored == nil {
			t.Fatalf("stored group for %s: %v, %v", sessionID, stored, err)
		}
		if !reflect.DeepEqual(*queued.StartupGroups.Index(sessionID), *stored) {
			t.Fatalf("published group for %s differs from the store", sessionID)
		}
	}
	reader.Close()

	handback := core.SubagentHandback{CommandID: "task", Message: "the report", RecordedAtMs: 7}
	recorded := handback
	if err := committedExecute(owner.writer, "finish one session", func(connection *Connection) error {
		if _, err := connection.Exec(`UPDATE startup_steps SET phase='failed',error='refused'
                     WHERE session_id='first'`); err != nil {
			return err
		}
		_, err := recordSubagentHandbackTo(path, "first", &recorded)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	after := committedSnapshot(t, owner.writer)
	failure := after.StartupGroups.Index("first").Failure
	if failure == nil || *failure == nil || **failure != "refused" {
		t.Fatalf("first group failure = %v", failure)
	}
	if got := after.SubagentReports.Index("first").Handback; got == nil || *got != handback {
		t.Fatalf("first report handback = %v", got)
	}
	if !reflect.DeepEqual(*after.StartupGroups.Index("second"), *queued.StartupGroups.Index("second")) {
		t.Fatal("the other session's group is untouched")
	}
	if after.SubagentReports.ContainsKey("second") {
		t.Fatal("the other session has no report")
	}

	// A writer that starts over the same store publishes the same records.
	if err := owner.Shutdown(); err != nil {
		t.Fatal(err)
	}
	owner = committedTestWriter(t, path)
	bootstrapped := committedSnapshot(t, owner.writer)
	if !reflect.DeepEqual(bootstrapped.StartupGroups, after.StartupGroups) {
		t.Fatal("bootstrapped startup groups differ")
	}
	if !reflect.DeepEqual(bootstrapped.SubagentReports, after.SubagentReports) {
		t.Fatal("bootstrapped reports differ")
	}

	if err := committedExecute(owner.writer, "drop startup", func(connection *Connection) error {
		if _, err := connection.Exec("DELETE FROM startup_steps WHERE session_id='first'"); err != nil {
			return err
		}
		_, err := connection.Exec("DELETE FROM subagent_handbacks")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	cleared := committedSnapshot(t, owner.writer)
	if cleared.StartupGroups.ContainsKey("first") || !cleared.StartupGroups.ContainsKey("second") {
		t.Fatal("only the dropped session's group goes away")
	}
	if !cleared.SubagentReports.IsEmpty() {
		t.Fatal("deleted reports must not stay published")
	}
}

func TestRollbackAndNoOpUpdatesDoNotPublishChanges(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "selected")
	owner := committedTestWriter(t, path)
	before := committedSnapshot(t, owner.writer)
	result := committedExecute(owner.writer, "rollback", func(connection *Connection) error {
		transaction, err := connection.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		if _, err := transaction.Exec("UPDATE sessions SET title='rolled back'"); err != nil {
			return err
		}
		return errors.New("operation failed before commit")
	})
	if result == nil {
		t.Fatal("the rolled back job must fail")
	}
	if err := committedExecute(owner.writer, "no-op update", func(connection *Connection) error {
		_, err := connection.Exec("UPDATE sessions SET title=title")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	after := committedSnapshot(t, owner.writer)
	if after.Sequence != before.Sequence {
		t.Fatalf("sequence moved from %d to %d", before.Sequence, after.Sequence)
	}
	if after != before {
		t.Fatal("a rollback or no-op must keep the published snapshot")
	}
	if !reflect.DeepEqual(after.State, before.State) {
		t.Fatal("state changed")
	}
}

func TestACommittedWriteIsPublishedEvenWhenLaterWorkInTheOperationFails(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "selected")
	owner := committedTestWriter(t, path)
	result := committedExecute(owner.writer, "failure after commit", func(connection *Connection) error {
		if _, err := connection.Exec("UPDATE sessions SET title='committed'"); err != nil {
			return err
		}
		return errors.New("later work failed")
	})
	if result == nil || !strings.Contains(result.Error(), "later work failed") {
		t.Fatalf("job result = %v", result)
	}
	if title := committedSnapshot(t, owner.writer).State.Sessions.Index("selected").Title; title != "committed" {
		t.Fatalf("published title = %q", title)
	}
}

func TestDeletingASessionPublishesItsAbsenceAndKeepsAHeldSnapshot(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	committedSaveSessions(t, path, "selected")
	owner := committedTestWriter(t, path)
	before := committedSnapshot(t, owner.writer)
	if err := committedExecute(owner.writer, "delete with cascading related rows", func(connection *Connection) error {
		_, err := connection.Exec("DELETE FROM sessions WHERE session_id='selected'")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if !committedSnapshot(t, owner.writer).State.Sessions.IsEmpty() {
		t.Fatal("a deleted session must not stay published")
	}
	if !before.State.Sessions.ContainsKey("selected") {
		t.Fatal("a held snapshot must keep the session")
	}
}
