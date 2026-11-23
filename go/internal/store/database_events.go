// Port of mj-store/src/database/events.rs.

// Durable, ordered events for the native subagent API.

package store

import (
	"database/sql"
	"errors"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func LoadAPIEvents(filter *core.ApiEventFilter, afterSeq *uint64, limit int) (core.ApiEventPage, error) {
	return loadAPIEventsFrom(DatabasePath(), filter, afterSeq, limit)
}

func loadAPIEventsFrom(path string, filter *core.ApiEventFilter, afterSeq *uint64, limit int) (core.ApiEventPage, error) {
	panic("unported: mj-store/src/database/events.rs load_api_events_from")
}

func insertAPIEvent(tx *Transaction, sessionID string, recordedAtMs int64, event core.ApiEventData) error {
	panic("unported: mj-store/src/database/events.rs insert_api_event")
}

// APIActivityUpdate is one session's activity for RecordAPIActivities
// (Rust's `(String, ApiActivityState)` pair).
type APIActivityUpdate struct {
	SessionID string
	Activity  core.ApiActivityState
}

// RecordAPIActivities is called on a blocking task; the shared writer
// serializes this with relay projection.
func RecordAPIActivities(activities []APIActivityUpdate, recordedAtMs int64) error {
	_, err := submitDatabaseWrite("record_api_activities", func(connection *Connection) (struct{}, error) {
		return struct{}{}, recordAPIActivitiesWith(connection, activities, recordedAtMs)
	})
	return err
}

func recordAPIActivitiesWith(connection *Connection, activities []APIActivityUpdate, recordedAtMs int64) error {
	panic("unported: mj-store/src/database/events.rs record_api_activities_with")
}

// PruneAPIActivityEvents removes one batch of activity events after
// afterSeq: the last sequence the batch scanned and how many rows it
// removed, or false when nothing follows afterSeq. Each batch bounds both
// the scan and the deletion, including stores with millions of legacy
// activity rows. Never reset sqlite_sequence: old cursors still name the
// same durable frontier after its last row has been removed.
func PruneAPIActivityEvents(afterSeq uint64) (through uint64, removed int, ok bool, err error) {
	type pruned struct {
		through uint64
		removed int
		ok      bool
	}
	result, err := submitDatabaseWrite("prune API activity history", func(connection *Connection) (pruned, error) {
		through, removed, ok, err := pruneAPIActivityEventsWith(connection, afterSeq, 1_000)
		return pruned{through, removed, ok}, err
	})
	return result.through, result.removed, result.ok, err
}

func pruneAPIActivityEventsWith(connection *Connection, afterSeq uint64, limit int) (through uint64, removed int, ok bool, err error) {
	panic("unported: mj-store/src/database/events.rs prune_api_activity_events_with")
}

func RecordStartupFault(sessionID string, message string) error {
	panic("unported: mj-store/src/database/events.rs record_startup_fault")
}

// migrateEventOutcomes upgrades bodies in place: sequence identity and the
// AUTOINCREMENT frontier stay unchanged.
func migrateEventOutcomes(tx *Transaction) error {
	panic("unported: mj-store/src/database/events.rs migrate_event_outcomes")
}

func previousSessionError(tx *Transaction, sessionID string) (*string, error) {
	var lastError sql.NullString
	err := tx.QueryRow("SELECT last_error FROM sessions WHERE session_id = ?1", sessionID).Scan(&lastError)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if !lastError.Valid {
		return nil, nil
	}
	return &lastError.String, nil
}

// recordSessionFaultTransition: lifecycle writes own this comparison and
// event in the same database transaction.
func recordSessionFaultTransition(tx *Transaction, session *core.SessionRecord, previous *string) error {
	if session.LastError != nil && (previous == nil || *previous != *session.LastError) {
		return insertAPIEvent(tx, session.ID, time.Now().UnixMilli(), core.ApiEventDataSessionFault{
			Reason:    core.OutcomeReasonLifecycleFailed,
			Message:   *session.LastError,
			CommandID: nil,
		})
	}
	return nil
}

// BeginCheckpointOperation reserves one requested checkpoint and saves its
// lifecycle state in one writer decision.
func BeginCheckpointOperation(session *core.SessionRecord, commandID string) error {
	_, err := submitDatabaseWrite("begin_checkpoint_operation", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := beginCheckpointOperationWith(tx, session, commandID); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func beginCheckpointOperationWith(tx *Transaction, session *core.SessionRecord, commandID string) error {
	panic("unported: mj-store/src/database/events.rs begin_checkpoint_operation_with")
}

func SaveRequestedCheckpoint(session *core.SessionRecord, commandID string) error {
	_, err := submitDatabaseWrite("save_requested_checkpoint", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := saveRequestedCheckpointWith(tx, session, commandID); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func saveRequestedCheckpointWith(tx *Transaction, session *core.SessionRecord, commandID string) error {
	panic("unported: mj-store/src/database/events.rs save_requested_checkpoint_with")
}

// finishCheckpointOperation consumes identity and publishes the terminal
// fact atomically; repeated finishes do nothing.
func finishCheckpointOperation(tx *Transaction, sessionID string, outcome core.CommandResultKind, reason *core.OutcomeReason, message *string) error {
	panic("unported: mj-store/src/database/events.rs finish_checkpoint_operation")
}

// CorrelateCheckpointBarrier is called before submission, so even a lost
// acknowledgement retains barrier correlation.
func CorrelateCheckpointBarrier(sessionID string, operationID string, commandID string) error {
	_, err := submitDatabaseWrite("correlate_checkpoint_barrier", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE checkpoint_operations SET related_command_ids = json_insert(related_command_ids, '$[#]', ?2) WHERE session_id = ?1 AND command_id = ?3", sessionID, commandID, operationID)
		return struct{}{}, err
	})
	return err
}

func FinishFailedCheckpoint(session *core.SessionRecord, commandID string, deferred bool, message string) error {
	_, err := submitDatabaseWrite("finish_failed_checkpoint", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := finishFailedCheckpointWith(tx, session, commandID, deferred, message); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func finishFailedCheckpointWith(tx *Transaction, session *core.SessionRecord, commandID string, deferred bool, message string) error {
	panic("unported: mj-store/src/database/events.rs finish_failed_checkpoint_with")
}
