// Port of mj-store/src/database/events.rs.

// Durable, ordered events for the native subagent API.

package store

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func LoadAPIEvents(filter *core.ApiEventFilter, afterSeq *uint64, limit int) (core.ApiEventPage, error) {
	return loadAPIEventsFrom(DatabasePath(), filter, afterSeq, limit)
}

func loadAPIEventsFrom(path string, filter *core.ApiEventFilter, afterSeq *uint64, limit int) (core.ApiEventPage, error) {
	connection, err := openReader(path)
	if err != nil {
		return core.ApiEventPage{}, err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return core.ApiEventPage{}, err
	}
	defer tx.Rollback()
	// sqlite_sequence survives deletion of the last event; cursors never move back.
	var latestSeq uint64
	if err := tx.QueryRow(
		"SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'api_events'), 0)",
	).Scan(&latestSeq); err != nil {
		return core.ApiEventPage{}, err
	}
	after := latestSeq
	if afterSeq != nil {
		after = *afterSeq
	}
	rows, err := tx.Query(
		"SELECT e.seq, e.session_id, e.recorded_at_ms, e.body FROM api_events e JOIN session_contexts w ON w.session_id = e.session_id WHERE e.seq > ?1 AND (?2 IS NULL OR e.session_id = ?2) AND (?3 IS NULL OR w.workspace_id = ?3) ORDER BY e.seq LIMIT ?4",
		after, filter.SessionID, filter.WorkspaceID, int64(min(max(limit, 1), 1000)),
	)
	if err != nil {
		return core.ApiEventPage{}, err
	}
	var events []core.ApiEvent
	for rows.Next() {
		var event core.ApiEvent
		var body string
		if err := rows.Scan(&event.Seq, &event.SessionID, &event.RecordedAtMs, &body); err != nil {
			rows.Close()
			return core.ApiEventPage{}, err
		}
		event.Event, err = core.UnmarshalApiEventData([]byte(body))
		if err != nil {
			rows.Close()
			return core.ApiEventPage{}, err
		}
		events = append(events, event)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		return core.ApiEventPage{}, err
	}
	nextAfterSeq := max(latestSeq, after)
	if len(events) > 0 {
		nextAfterSeq = events[len(events)-1].Seq
	}
	// Historical activity is disposable. Advance over it without replaying it,
	// including while the background cleanup is still removing old rows.
	kept := []core.ApiEvent{}
	for _, event := range events {
		switch event.Event.(type) {
		case core.ApiEventDataActivityChanged, *core.ApiEventDataActivityChanged:
			continue
		}
		kept = append(kept, event)
	}
	return core.ApiEventPage{
		Events:       kept,
		NextAfterSeq: nextAfterSeq,
		LatestSeq:    latestSeq,
	}, nil
}

// eventsSerdeText is serde_json::to_string over encoding/json's output:
// the same JSON without Go's HTML escapes (`<`, `>`, `&`, U+2028 and U+2029
// stay as written), so stored bodies match the ones Rust writes.
func eventsSerdeText(data []byte, err error) (string, error) {
	if err != nil {
		return "", err
	}
	if !bytes.Contains(data, []byte(`\u`)) {
		return string(data), nil
	}
	output := make([]byte, 0, len(data))
	inString := false
	for i := 0; i < len(data); i++ {
		c := data[i]
		if !inString {
			if c == '"' {
				inString = true
			}
			output = append(output, c)
			continue
		}
		switch c {
		case '"':
			inString = false
			output = append(output, c)
		case '\\':
			if i+5 < len(data) && data[i+1] == 'u' {
				replacement := ""
				switch string(data[i+2 : i+6]) {
				case "003c":
					replacement = "<"
				case "003e":
					replacement = ">"
				case "0026":
					replacement = "&"
				case "2028":
					replacement = " "
				case "2029":
					replacement = " "
				}
				if replacement != "" {
					output = append(output, replacement...)
					i += 5
					continue
				}
			}
			output = append(output, c, data[i+1])
			i++
		default:
			output = append(output, c)
		}
	}
	return string(output), nil
}

func insertAPIEvent(tx *Transaction, sessionID string, recordedAtMs int64, event core.ApiEventData) error {
	body, err := eventsSerdeText(core.MarshalApiEventData(event))
	if err != nil {
		return err
	}
	result, err := tx.Exec(
		"INSERT INTO api_events(session_id, recorded_at_ms, body) VALUES (?1, ?2, ?3)",
		sessionID, recordedAtMs, body,
	)
	if err != nil {
		return err
	}
	rowID, err := result.LastInsertId()
	if err != nil {
		return err
	}
	seq := uint64(rowID)
	if err := projectEvent(tx, sessionID, seq, recordedAtMs, event); err != nil {
		return err
	}
	var request *core.ElicitationRequest
	switch input := event.(type) {
	case core.ApiEventDataInputRequired:
		request = input.Request
	case *core.ApiEventDataInputRequired:
		request = input.Request
	}
	if request != nil {
		if _, err := forwardSubagentQuestionWith(tx, sessionID, seq, recordedAtMs, request); err != nil {
			return err
		}
	}
	return nil
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
	tx, err := connection.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	for _, update := range activities {
		var exists bool
		if err := tx.QueryRow(
			"SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)", update.SessionID,
		).Scan(&exists); err != nil {
			return err
		}
		if !exists {
			continue
		}
		body, err := eventsSerdeText(json.Marshal(update.Activity))
		if err != nil {
			return err
		}
		var previous string
		err = tx.QueryRow(
			"SELECT body FROM api_session_activity WHERE session_id = ?1", update.SessionID,
		).Scan(&previous)
		switch {
		case errors.Is(err, sql.ErrNoRows):
		case err != nil:
			return err
		case previous == body:
			continue
		}
		if _, err := tx.Exec(
			"INSERT INTO api_session_activity(session_id, body) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET body = excluded.body",
			update.SessionID, body,
		); err != nil {
			return err
		}
	}
	return tx.Commit()
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
	tx, err := connection.Begin()
	if err != nil {
		return 0, 0, false, err
	}
	defer tx.Rollback()
	var last sql.NullInt64
	if err := tx.QueryRow(
		"SELECT max(seq) FROM (SELECT seq FROM api_events WHERE seq > ?1 ORDER BY seq LIMIT ?2)",
		afterSeq, uint64(limit),
	).Scan(&last); err != nil {
		return 0, 0, false, err
	}
	if !last.Valid {
		return 0, 0, false, nil
	}
	through = uint64(last.Int64)
	result, err := tx.Exec(
		`DELETE FROM api_events WHERE seq > ?1 AND seq <= ?2
         AND json_extract(body, '$.type') = 'activity_changed'`,
		afterSeq, through,
	)
	if err != nil {
		return 0, 0, false, err
	}
	deleted, err := result.RowsAffected()
	if err != nil {
		return 0, 0, false, err
	}
	if err := tx.Commit(); err != nil {
		return 0, 0, false, err
	}
	return through, int(deleted), true, nil
}

func RecordStartupFault(sessionID string, message string) error {
	_, err := submitDatabaseWrite("record_startup_fault", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		var exists bool
		if err := tx.QueryRow(
			"SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)", sessionID,
		).Scan(&exists); err != nil {
			return struct{}{}, err
		}
		if exists {
			if err := insertAPIEvent(tx, sessionID, time.Now().UnixMilli(), core.ApiEventDataSessionFault{
				Reason:    core.OutcomeReasonStartupFailed,
				Message:   message,
				CommandID: nil,
			}); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// eventsMember is one member of a JSON object, kept in document order as
// serde_json's preserve_order Value keeps it.
type eventsMember struct {
	key   string
	value json.RawMessage
}

// eventsObject reads a JSON object's members in order. A repeated key keeps
// its first position and its last value, as indexmap's insert does.
func eventsObject(data []byte) ([]eventsMember, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	if delimiter, ok := token.(json.Delim); !ok || delimiter != '{' {
		return nil, fmt.Errorf("expected a JSON object, found %s", data)
	}
	members := []eventsMember{}
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, err
		}
		key, ok := token.(string)
		if !ok {
			return nil, fmt.Errorf("expected an object key, found %v", token)
		}
		var value json.RawMessage
		if err := decoder.Decode(&value); err != nil {
			return nil, err
		}
		members = eventsSet(members, key, value)
	}
	if _, err := decoder.Token(); err != nil {
		return nil, err
	}
	return members, nil
}

// eventsSet is serde_json's `object[key] = value`: an existing member keeps
// its position, a new one goes last.
func eventsSet(members []eventsMember, key string, value json.RawMessage) []eventsMember {
	for i := range members {
		if members[i].key == key {
			members[i].value = value
			return members
		}
	}
	return append(members, eventsMember{key: key, value: value})
}

// eventsGet is serde_json's `object[key]`: the member's value, or null.
func eventsGet(members []eventsMember, key string) json.RawMessage {
	for _, member := range members {
		if member.key == key {
			return member.value
		}
	}
	return json.RawMessage("null")
}

// eventsObjectJSON writes the members as compact JSON, in order.
func eventsObjectJSON(members []eventsMember) ([]byte, error) {
	var output bytes.Buffer
	output.WriteByte('{')
	for i, member := range members {
		if i > 0 {
			output.WriteByte(',')
		}
		key, err := eventsSerdeText(json.Marshal(member.key))
		if err != nil {
			return nil, err
		}
		output.WriteString(key)
		output.WriteByte(':')
		if err := json.Compact(&output, member.value); err != nil {
			return nil, err
		}
	}
	output.WriteByte('}')
	return output.Bytes(), nil
}

// eventsChild is serde_json's IndexMut on `object[key]` for a nested
// object: null (or a missing member) becomes an empty object.
func eventsChild(members []eventsMember, key string) ([]eventsMember, error) {
	value := eventsGet(members, key)
	if bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
		return []eventsMember{}, nil
	}
	return eventsObject(value)
}

// migrateEventOutcomes upgrades bodies in place: sequence identity and the
// AUTOINCREMENT frontier stay unchanged.
func migrateEventOutcomes(tx *Transaction) error {
	rows, err := tx.Query("SELECT seq, body FROM api_events WHERE json_extract(body, '$.type') IN ('error', 'turn_ended')")
	if err != nil {
		return err
	}
	type stored struct {
		seq  uint64
		body string
	}
	var bodies []stored
	for rows.Next() {
		var row stored
		if err := rows.Scan(&row.seq, &row.body); err != nil {
			rows.Close()
			return err
		}
		bodies = append(bodies, row)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		return err
	}
	for _, row := range bodies {
		value, err := eventsObject([]byte(row.body))
		if err != nil {
			return err
		}
		var kind string
		isError := json.Unmarshal(eventsGet(value, "type"), &kind) == nil && kind == "error"
		data, err := eventsChild(value, "data")
		if err != nil {
			return err
		}
		if isError {
			value = eventsSet(value, "type", json.RawMessage(`"legacy_notice"`))
			data = eventsSet(data, "original_type", json.RawMessage(`"error"`))
		} else {
			var turn core.MaterializedTurnOutcome
			if err := json.Unmarshal(eventsGet(data, "turn"), &turn); err != nil {
				return err
			}
			outcome, err := eventsSerdeText(json.Marshal(core.ApiTurnOutcomeFrom(&turn)))
			if err != nil {
				return err
			}
			data = eventsSet(data, "turn", json.RawMessage(outcome))
		}
		encodedData, err := eventsObjectJSON(data)
		if err != nil {
			return err
		}
		value = eventsSet(value, "data", encodedData)
		encoded, err := eventsObjectJSON(value)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("UPDATE api_events SET body = ?2 WHERE seq = ?1", row.seq, string(encoded)); err != nil {
			return err
		}
	}
	return nil
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
	if err := validateSessionRecord(session); err != nil {
		return err
	}
	var state string
	if err := tx.QueryRow("SELECT state FROM sessions WHERE session_id = ?1", session.ID).Scan(&state); err != nil {
		return err
	}
	switch state {
	case "checkpointing", "closing", "destroying":
		return fmt.Errorf("session %s is already in a lifecycle operation", session.ID)
	}
	if _, err := tx.Exec(
		"INSERT INTO checkpoint_operations(session_id, command_id) VALUES (?1, ?2)",
		session.ID, commandID,
	); err != nil {
		return err
	}
	return updateLifecycleFields(tx, session)
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
	if err := validateSessionRecord(session); err != nil {
		return err
	}
	var owns bool
	if err := tx.QueryRow(
		"SELECT EXISTS(SELECT 1 FROM checkpoint_operations WHERE session_id = ?1 AND command_id = ?2)",
		session.ID, commandID,
	).Scan(&owns); err != nil {
		return err
	}
	if !owns {
		return fmt.Errorf("checkpoint operation no longer owns session %s", session.ID)
	}
	if session.Checkpoint == nil {
		return errors.New("successful checkpoint has no archive metadata")
	}
	if err := updateLifecycleFields(tx, session); err != nil {
		return err
	}
	if _, err := tx.Exec(
		"UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
		session.ID, session.NativeSessionID,
	); err != nil {
		return err
	}
	if err := replaceCheckpoint(tx, session); err != nil {
		return err
	}
	return finishCheckpointOperation(tx, session.ID, core.CommandResultKindSucceeded, nil, nil)
}

// finishCheckpointOperation consumes identity and publishes the terminal
// fact atomically; repeated finishes do nothing.
func finishCheckpointOperation(tx *Transaction, sessionID string, outcome core.CommandResultKind, reason *core.OutcomeReason, message *string) error {
	var commandID, related string
	err := tx.QueryRow(
		"SELECT command_id, related_command_ids FROM checkpoint_operations WHERE session_id = ?1",
		sessionID,
	).Scan(&commandID, &related)
	if errors.Is(err, sql.ErrNoRows) {
		return nil
	}
	if err != nil {
		return err
	}
	var relatedCommandIDs []string
	if err := json.Unmarshal([]byte(related), &relatedCommandIDs); err != nil {
		return err
	}
	if err := insertAPIEvent(tx, sessionID, time.Now().UnixMilli(), core.ApiEventDataCommandEnded{
		Result: core.CommandResult{
			Owner:             core.CommandOwnerDaemon,
			CommandID:         commandID,
			CommandKind:       "checkpoint",
			Outcome:           outcome,
			Reason:            reason,
			Message:           message,
			RelatedCommandIDs: relatedCommandIDs,
		},
	}); err != nil {
		return err
	}
	_, err = tx.Exec("DELETE FROM checkpoint_operations WHERE session_id = ?1", sessionID)
	return err
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
	if err := validateSessionRecord(session); err != nil {
		return err
	}
	var owns bool
	if err := tx.QueryRow(
		"SELECT EXISTS(SELECT 1 FROM checkpoint_operations WHERE session_id = ?1 AND command_id = ?2)",
		session.ID, commandID,
	).Scan(&owns); err != nil {
		return err
	}
	if !owns {
		return nil
	}
	if err := updateLifecycleFields(tx, session); err != nil {
		return err
	}
	outcome := core.CommandResultKindFailed
	reason := core.OutcomeReasonCheckpointFailed
	if deferred {
		outcome = core.CommandResultKindRejected
		reason = core.OutcomeReasonCheckpointDeferred
	}
	return finishCheckpointOperation(tx, session.ID, outcome, &reason, &message)
}
