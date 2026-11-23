// Port of mj-store/src/database/session_create.rs.

// Durable record that a new session's creation is unfinished.
//
// The session record cannot say this by itself: `Disconnected` also
// describes every ordinary session whose worker connection dropped. One row
// per session, written in the transaction that registers the session and
// removed by every write that moves the session out of the creating states,
// is the single owner of "creation unfinished". A daemon that stops while a
// create runs leaves the row; the next daemon finishes the create from it.

package store

import (
	"database/sql"
	"errors"
	"fmt"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type SessionCreateIntent struct {
	SessionID   string
	OperationID string
	// RememberLaunchChoices is whether the person launched interactively;
	// the post-success step that remembers the project directory needs it
	// after a restart.
	RememberLaunchChoices bool
}

// HoldsCreateIntent is the states a session's create intent can exist in:
// the storage rule that registration and every session write keep. It says
// nothing about whether a given session's creation is unfinished; the intent
// row is the owner of that.
func HoldsCreateIntent(state core.SessionState) bool {
	return state == core.SessionStateProvisioning || state == core.SessionStateDisconnected
}

// CreateIntentIsStale is whether a create intent outlived its creation: its
// session was archived, or finished or failed by something that did not
// settle the intent (an older daemon).
func CreateIntentIsStale(session *core.SessionRecord) bool {
	return session.Archived || !HoldsCreateIntent(session.State)
}

// CreateEnd is how a session's creation stands, read from durable state
// alone: the intent row says whether it is unfinished, the record says how it
// ended. The row is read first. It is removed in the transaction that writes
// the final state, so a missing row means the record already shows the end.
type CreateEnd interface {
	isCreateEnd()
}

type CreateEndUnfinished struct{}

type CreateEndFinished struct{}

// CreateEndFailed carries the failure the record shows.
type CreateEndFailed struct {
	Message string
}

func (CreateEndUnfinished) isCreateEnd() {}
func (CreateEndFinished) isCreateEnd()   {}
func (CreateEndFailed) isCreateEnd()     {}

func SessionCreateEnd(sessionID string) (CreateEnd, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	var unfinished bool
	err = connection.QueryRow(
		"SELECT EXISTS(SELECT 1 FROM session_create_intents WHERE session_id = ?1)",
		sessionID,
	).Scan(&unfinished)
	connection.Close()
	if err != nil {
		return nil, err
	}
	if unfinished {
		return CreateEndUnfinished{}, nil
	}
	record, err := LoadSessionRecord(sessionID)
	if err != nil {
		return nil, err
	}
	if record == nil {
		return CreateEndFailed{Message: fmt.Sprintf("session %s no longer exists", sessionID)}, nil
	}
	// A failed create passes through StartupCleanup (intent settled) on its
	// way to Error; that is already its failure.
	switch record.State {
	case core.SessionStateError, core.SessionStateStartupCleanup:
		message := "session creation failed"
		if record.LastError != nil {
			message = *record.LastError
		}
		return CreateEndFailed{Message: message}, nil
	}
	return CreateEndFinished{}, nil
}

// insertCreateIntent writes session's create intent and returns its
// operation ID; nil when the session is past creation or already has one.
func insertCreateIntent(tx *Transaction, session *core.SessionRecord, rememberLaunchChoices bool) (*string, error) {
	// Sessions registered already past creation (fixtures, imports) own none.
	if !HoldsCreateIntent(session.State) {
		return nil, nil
	}
	operationID, err := client.NewCommandID("create")
	if err != nil {
		return nil, err
	}
	var inserted string
	err = tx.QueryRow(`INSERT INTO session_create_intents(session_id, operation_id, remember_launch_choices, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO NOTHING RETURNING operation_id`,
		session.ID, operationID, rememberLaunchChoices, core.FormatChronoRFC3339(time.Now()),
	).Scan(&inserted)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &inserted, nil
}

// settleCreateIntent: every session write calls this with the state it
// writes: a state outside the creating ones means the create committed or
// failed for good.
func settleCreateIntent(tx *Transaction, session *core.SessionRecord) error {
	return settleCreateIntentForState(tx, session.ID, session.State)
}

func settleCreateIntentForState(tx *Transaction, sessionID string, state core.SessionState) error {
	if !HoldsCreateIntent(state) {
		if _, err := tx.Exec("DELETE FROM session_create_intents WHERE session_id = ?1", sessionID); err != nil {
			return err
		}
	}
	return nil
}

func LoadSessionCreateIntents() ([]SessionCreateIntent, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT session_id, operation_id, remember_launch_choices
         FROM session_create_intents ORDER BY created_at, session_id`)
	if err != nil {
		return nil, err
	}
	intents := []SessionCreateIntent{}
	for rows.Next() {
		var intent SessionCreateIntent
		if err := rows.Scan(&intent.SessionID, &intent.OperationID, &intent.RememberLaunchChoices); err != nil {
			rows.Close()
			return nil, err
		}
		intents = append(intents, intent)
	}
	return intents, errors.Join(rows.Err(), rows.Close())
}

// DiscardSessionCreateIntent drops an intent whose session already finished
// or failed elsewhere.
func DiscardSessionCreateIntent(sessionID string, operationID string) error {
	_, err := submitDatabaseWrite("discard_session_create_intent", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec(
			"DELETE FROM session_create_intents WHERE session_id = ?1 AND operation_id = ?2",
			sessionID, operationID,
		)
		return struct{}{}, err
	})
	return err
}

// restoreCreateIntentForRetry: a sub-agent's failed worker start that is
// retried stays the same create: the cleanup between the attempts moves the
// record out of the creating states and settles its intent, so the retry
// puts the intent back.
func restoreCreateIntentForRetry(tx *Transaction, session *core.SessionRecord) error {
	_, err := insertCreateIntent(tx, session, false)
	return err
}

// FailedFirstLaunch is the only classification of a failed create that
// Resume may launch again.
func FailedFirstLaunch(session *core.SessionRecord, isSubagent bool, movement *core.MoveOperation) bool {
	return !isSubagent &&
		!core.IsViewID(session.ID) &&
		!session.Archived &&
		session.Checkpoint == nil &&
		session.NativeSessionID == nil &&
		(session.State == core.SessionStateStartupCleanup ||
			(session.State == core.SessionStateError && session.Target == nil)) &&
		!(movement != nil && (movement.IsActive() ||
			movement.HoldsSourceEnvironment() ||
			(movement.QueueAdmissionStarted && !movement.QueueAdmissionFinished)))
}

func failedLaunchRecordWith(connection Queryer, id string) (*core.SessionRecord, error) {
	record, err := loadSessionWith(connection, id)
	if err != nil || record == nil {
		return nil, err
	}
	var isSubagent bool
	if err := connection.QueryRow(
		"SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1)", id,
	).Scan(&isSubagent); err != nil {
		return nil, err
	}
	movement, err := loadMoveOperationWith(connection, id)
	if err != nil {
		return nil, err
	}
	if !FailedFirstLaunch(record, isSubagent, movement) {
		return nil, nil
	}
	return record, nil
}

func ReadFailedLaunchRecord(id string) (*core.SessionRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	snapshot, err := connection.Begin()
	if err != nil {
		return nil, err
	}
	defer snapshot.Rollback()
	return failedLaunchRecordWith(snapshot, id)
}

// BeginFailedLaunchRetry accepts the retry and publishes a recoverable
// create in the same transaction. It returns the create's operation ID, or
// nil when the session is no longer the failed launch expected describes.
func BeginFailedLaunchRetry(expected core.SessionRecord) (*string, error) {
	return submitDatabaseWrite("begin failed launch retry", func(connection *Connection) (*string, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		record, err := failedLaunchRecordWith(tx, expected.ID)
		if err != nil || record == nil {
			return nil, err
		}
		if record.WorkspaceID != expected.WorkspaceID ||
			record.LastProfile != expected.LastProfile ||
			record.TargetTemplateID != expected.TargetTemplateID ||
			record.ResourceAllocation != expected.ResourceAllocation ||
			!slices.Equal(record.AdditionalMounts, expected.AdditionalMounts) {
			return nil, nil
		}
		record.State = core.SessionStateProvisioning
		record.LastError = nil
		record.UpdatedAt = core.FormatChronoRFC3339(time.Now())
		if err := updateLifecycleFields(tx, record); err != nil {
			return nil, err
		}
		operationID, err := insertCreateIntent(tx, record, false)
		if err != nil {
			return nil, err
		}
		if operationID == nil {
			return nil, errors.New("failed session still has a create intent")
		}
		if err := dismissFailedStartupGroupsWith(tx, record.ID); err != nil {
			return nil, err
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		return operationID, nil
	})
}

// ResetSessionForCreateRetry resets a session to its registered form so its
// create can run again: the failed attempt's environment is retired by the
// caller first, so nothing the worker restart intent describes exists any
// more.
func ResetSessionForCreateRetry(session *core.SessionRecord) error {
	_, err := submitDatabaseWrite("reset_session_for_create_retry", func(connection *Connection) (struct{}, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if _, err := tx.Exec("DELETE FROM worker_restart_intents WHERE session_id = ?1", session.ID); err != nil {
			return struct{}{}, err
		}
		if err := updateLifecycleFields(tx, session); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}
