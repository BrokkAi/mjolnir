// Port of mj-store/src/database/session_move.rs.

// Move intent uses the same guarded writer as session lifecycle transitions.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"reflect"
	"runtime"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func RetainedMoveSources(sessionID string) ([]core.RetainedMoveSource, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := environmentTextRows(connection, 4, "SELECT operation_id, source_json, exclusions_json, created_at FROM retained_move_sources WHERE session_id=?1 ORDER BY created_at", sessionID)
	if err != nil {
		return nil, err
	}
	sources := make([]core.RetainedMoveSource, 0, len(rows))
	for _, row := range rows {
		source := core.RetainedMoveSource{OperationID: row[0], SessionID: sessionID, CreatedAt: row[3]}
		if err := json.Unmarshal([]byte(row[1]), &source.Source); err != nil {
			return nil, err
		}
		if err := json.Unmarshal([]byte(row[2]), &source.Exclusions); err != nil {
			return nil, err
		}
		sources = append(sources, source)
	}
	return sources, nil
}

func ForgetRetainedMoveSource(operationID string) error {
	_, err := submitDatabaseWrite("forget_retained_move_source", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM retained_move_sources WHERE operation_id=?1", operationID)
		return struct{}{}, err
	})
	return err
}

// RemoveRevokedMoveGrantedKeys atomically removes revoked ephemeral SSH
// credentials from whichever MoveOperation row is currently persisted for
// sessionID, without overwriting any other field (so background cleanup can
// never clobber a newer Move admitted concurrently for the same session).
func RemoveRevokedMoveGrantedKeys(sessionID string, revoked []core.GrantedMoveKey, updatedAt string) error {
	if len(revoked) == 0 {
		return nil
	}
	revoked = slices.Clone(revoked)
	_, err := submitDatabaseWrite("remove_revoked_move_granted_keys", func(connection *Connection) (struct{}, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		current, err := loadMoveOperationWith(tx, sessionID)
		if err != nil || current == nil {
			return struct{}{}, err
		}
		before := len(current.GrantedKeys)
		current.GrantedKeys = slices.DeleteFunc(slices.Clone(current.GrantedKeys), func(key core.GrantedMoveKey) bool {
			return slices.ContainsFunc(revoked, func(candidate core.GrantedMoveKey) bool {
				return reflect.DeepEqual(candidate, key)
			})
		})
		if len(current.GrantedKeys) != before {
			current.UpdatedAt = updatedAt
			encoded, err := databaseMarshalJSON(current)
			if err != nil {
				return struct{}{}, err
			}
			if _, err := tx.Exec("UPDATE session_moves SET operation_json=?2 WHERE session_id=?1 AND operation_id=?3",
				sessionID, string(encoded), current.OperationID); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func SaveMoveOperation(operation *core.MoveOperation) error {
	_, err := submitDatabaseWrite("save_move_operation", func(connection *Connection) (struct{}, error) {
		return struct{}{}, saveMoveOperationWith(connection, operation)
	})
	return err
}

// AcceptMoveOperation records the person's requests atomically with
// acceptance of the Move intent.
func AcceptMoveOperation(operation *core.MoveOperation) error {
	_, err := submitDatabaseWrite("accept Move", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := updateMoveRequests(tx, &operation.Selection); err != nil {
			return struct{}{}, err
		}
		if err := saveMoveOperationWith(tx, operation); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// AcceptUnchangedMove: an unchanged Move still accepts the person's explicit
// next-creation choices.
func AcceptUnchangedMove(selection *core.MoveSelection) error {
	_, err := submitDatabaseWrite("accept unchanged Move", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := updateMoveRequests(tx, selection); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func updateMoveRequests(tx *sql.Tx, selection *core.MoveSelection) error {
	var allocation *core.SessionResourceAllocation
	switch {
	case selection.ClearResourceAllocation:
		var cleared core.SessionResourceAllocation
		allocation = &cleared
	case selection.ResourceAllocation != nil:
		requested := selection.ResourceAllocation
		allocation = &requested
	}
	return updateContainerRequests(tx, selection.SessionID, allocation, selection.AdditionalMounts)
}

// AdoptMoveDestination publishes destination adoption and ownership
// together, without changing client fields.
func AdoptMoveDestination(operation *core.MoveOperation, session *core.SessionRecord) error {
	_, err := submitDatabaseWrite("adopt_move_destination", func(connection *Connection) (struct{}, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := updateLifecycleFields(tx, session); err != nil {
			return struct{}{}, err
		}
		if err := saveMoveOperationWith(tx, operation); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// SaveMoveOutcome publishes a Move result and its session message as one
// committed outcome. A failed rollback can settle its own Closing record as
// retryable Error. Other session fields retain the values written by their
// own owners.
func SaveMoveOutcome(operation *core.MoveOperation, lastError *string, failClosingSource bool) error {
	_, err := submitDatabaseWrite("save_move_outcome", func(connection *Connection) (struct{}, error) {
		return struct{}{}, saveMoveOutcomeWith(connection, operation, lastError, failClosingSource)
	})
	return err
}

func saveMoveOutcomeWith(connection *Connection, operation *core.MoveOperation, lastError *string, failClosingSource bool) error {
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := saveMoveOperationWith(tx, operation); err != nil {
		return err
	}
	result, err := tx.Exec("UPDATE sessions SET last_error=?2, updated_at=?3 WHERE session_id=?1",
		operation.Selection.SessionID, lastError, operation.UpdatedAt)
	if err != nil {
		return err
	}
	updated, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if updated != 1 {
		return errors.New("Move outcome session is missing")
	}
	if failClosingSource {
		if _, err := tx.Exec("UPDATE sessions SET state='error' WHERE session_id=?1 AND state='closing'",
			operation.Selection.SessionID); err != nil {
			return err
		}
	}
	return tx.Commit()
}

// saveMoveOperationWith writes the intent in the caller's transaction, or in
// its own when connection is a *Connection outside one (Rust checks
// is_autocommit).
func saveMoveOperationWith(connection Queryer, operation *core.MoveOperation) error {
	if outer, ok := connection.(*Connection); ok && outer.isAutocommit() {
		tx, err := outer.Begin()
		if err != nil {
			return err
		}
		defer tx.Rollback()
		if err := saveMoveOperationWith(tx, operation); err != nil {
			return err
		}
		return tx.Commit()
	}
	if err := recordPreparedMoveEnvironment(connection, operation); err != nil {
		return err
	}
	encoded, err := databaseMarshalJSON(operation)
	if err != nil {
		return err
	}
	if _, err := connection.Exec(`INSERT INTO session_moves(session_id, operation_id, operation_json,source_environment_id)
         VALUES (?1, ?2, ?3,(SELECT environment_id FROM environment_bindings WHERE session_id=?1))
         ON CONFLICT(session_id) DO UPDATE SET source_environment_id=CASE WHEN session_moves.operation_id=excluded.operation_id THEN session_moves.source_environment_id ELSE excluded.source_environment_id END,
             operation_id=excluded.operation_id,
             operation_json=CASE WHEN session_moves.operation_id=excluded.operation_id
                 AND json_extract(session_moves.operation_json, '$.cancellation_requested')=1
                 THEN json_set(excluded.operation_json, '$.cancellation_requested', json('true'))
                 ELSE excluded.operation_json END`,
		operation.Selection.SessionID, operation.OperationID, string(encoded)); err != nil {
		return err
	}
	_, err = rejectFailedMoveStartupPromptsWith(connection, operation.Selection.SessionID)
	return err
}

func LoadMoveOperation(sessionID string) (*core.MoveOperation, error) {
	committed, err := WriterCommittedState()
	if err != nil {
		return nil, err
	}
	if committed != nil {
		operation, ok := committed.Moves.Get(sessionID)
		if !ok {
			return nil, nil
		}
		return &operation, nil
	}
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return loadMoveOperationWith(connection, sessionID)
}

// HasActiveInPlaceMove reports whether a durable in-place Move currently owns
// sub-agent admission closed. A worker actor uses this before repairing a
// closed gate on reconnect: an in-flight Move persists its phase before
// closing admission, so the durable row keeps reconnect repair from
// reopening it prematurely.
func HasActiveInPlaceMove(sessionID string) (bool, error) {
	operation, err := LoadMoveOperation(sessionID)
	if err != nil {
		return false, err
	}
	return operation != nil && moveHoldsSubagentGate(operation), nil
}

func moveHoldsSubagentGate(operation *core.MoveOperation) bool {
	return operation.InPlace && operation.IsActive()
}

func loadMoveOperationWith(connection Queryer, sessionID string) (*core.MoveOperation, error) {
	var text string
	err := connection.QueryRow("SELECT operation_json FROM session_moves WHERE session_id=?1", sessionID).Scan(&text)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return decodeMoveOperation(connection, sessionID, text), nil
}

// resolveCancelledMoveForCloseWith releases only the cancelled Move's restore
// authority. Placement ownership has already been settled; do not adopt or
// recreate it while resolving.
func resolveCancelledMoveForCloseWith(connection Queryer, sessionID, operationID, updatedAt string) error {
	operation, err := loadMoveOperationWith(connection, sessionID)
	if err != nil {
		return err
	}
	if operation == nil {
		return errors.New("superseded Move disappeared before close publication")
	}
	if operation.OperationID != operationID || operation.Phase != core.MovePhaseCancelled || operation.QueueAdmissionStarted {
		return errors.New("superseded Move no longer permits close settlement")
	}
	operation.QueueAdmissionFinished = true
	operation.UpdatedAt = updatedAt
	encoded, err := databaseMarshalJSON(operation)
	if err != nil {
		return err
	}
	_, err = connection.Exec("UPDATE session_moves SET operation_json=?2, close_requested_operation_id=NULL WHERE session_id=?1",
		sessionID, string(encoded))
	return err
}

// decodeMoveOperation decodes one stored move intent, or nil when it no
// longer decodes.
//
// A move intent recorded before a harness was removed keeps that harness in
// its recovery snapshot, so it fails to decode under a binary that no longer
// knows the harness. Nothing deletes a completed intent, so such rows linger
// indefinitely (BrokkAi/mjolnir#1026). Every reader asks the same question,
// "is there a move to act on for this session?", and a move whose snapshot
// names a removed harness cannot be acted on, so all readers treat the row as
// absent with a warning rather than failing the operation that asked, whether
// that is daemon startup, a stop, a checkpoint, or a new move.
func decodeMoveOperation(connection Queryer, sessionID, text string) *core.MoveOperation {
	normalized, err := normalizeStoredMove(connection, sessionID, text)
	if err == nil {
		var operation core.MoveOperation
		if err = json.Unmarshal([]byte(normalized), &operation); err == nil {
			return &operation
		}
	}
	slog.Warn("durable move intent no longer decodes; treating it as absent (its harness may have been removed)",
		"session_id", sessionID, "error", err)
	return nil
}

// normalizeStoredMove: older Move intents can hold the opt-in in the sealed
// selection, its preparation receipt, and the rollback snapshot. Translate
// those stored fields at the reader boundary so strict public requests keep
// one policy.
func normalizeStoredMove(connection Queryer, sessionID, text string) (string, error) {
	if !strings.Contains(text, `"supervisors"`) {
		return text, nil
	}
	intent, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		return "", err
	}
	object, ok := intent.(jsonvalue.Object)
	if !ok {
		return text, nil
	}
	if snapshot, ok := object.Get("recovery_session"); ok {
		if err := foldOrchestrators(&snapshot, nil); err != nil {
			return "", err
		}
		object.Set("recovery_session", snapshot)
	}
	var sourcePolicy any
	recovery, _ := object.Get("recovery_session")
	if policy, ok := jsonValueGet(recovery, "subagents"); ok && policy != nil {
		sourcePolicy = policy
	} else {
		stored, err := environmentOptionalString(connection, "SELECT subagents FROM sessions WHERE session_id=?1", sessionID)
		if err != nil {
			return "", err
		}
		if stored != nil {
			if sourcePolicy, err = jsonvalue.Parse([]byte(*stored)); err != nil {
				return "", err
			}
		}
	}
	if selection, ok := object.Get("selection"); ok {
		if err := foldOrchestrators(&selection, sourcePolicy); err != nil {
			return "", err
		}
		object.Set("selection", selection)
	}
	if preparation, ok := object.Get("accepted_preparation"); ok {
		if prepared, ok := preparation.(jsonvalue.Object); ok {
			if selection, ok := prepared.Get("selection"); ok {
				if err := foldOrchestrators(&selection, sourcePolicy); err != nil {
					return "", err
				}
				prepared.Set("selection", selection)
				object.Set("accepted_preparation", prepared)
			}
		}
	}
	normalized, err := jsonvalue.Marshal(object)
	if err != nil {
		return "", err
	}
	return string(normalized), nil
}

// foldOrchestrators is normalize_stored_move's inner fold_orchestrators:
// record and sourcePolicy are jsonvalue-form values, and record is changed
// in place.
func foldOrchestrators(record *any, sourcePolicy any) error {
	object, ok := (*record).(jsonvalue.Object)
	if !ok {
		return nil
	}
	enabledValue, present := object.Get("supervisors")
	object.Delete("supervisors")
	*record = object
	if !present || enabledValue == nil {
		return nil
	}
	enabled, ok := enabledValue.(bool)
	if !ok {
		return fmt.Errorf("invalid type: %s, expected a boolean", jsonValueKind(enabledValue))
	}
	policy, ok := object.Get("subagents")
	if !ok || policy == nil {
		policy = sourcePolicy
	}
	if policy == nil {
		return nil
	}
	encoded, err := jsonvalue.Marshal(policy)
	if err != nil {
		return err
	}
	parsed, err := core.ParseStoredPolicy(string(encoded))
	if err != nil {
		return err
	}
	if parsed.Mode == core.SubagentPolicyMjolnir {
		parsed.Orchestrators = enabled
		encoded, err := json.Marshal(parsed)
		if err != nil {
			return err
		}
		value, err := jsonvalue.Parse(encoded)
		if err != nil {
			return err
		}
		object.Set("subagents", value)
		*record = object
	}
	return nil
}

func LoadMoveOperations() ([]core.MoveOperation, error) {
	committed, err := WriterCommittedState()
	if err != nil {
		return nil, err
	}
	if committed != nil {
		return slices.Collect(committed.Moves.IntoValues()), nil
	}
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return loadMoveOperationsWith(connection)
}

// LegacyMoveOperationRow is one pre-148 session_moves row: the session it
// belongs to and its legacy intent.
type LegacyMoveOperationRow struct {
	SessionID string
	Operation core.LegacyMoveOperation
}

// LoadLegacyMoveOperations loads any pre-148 LegacyMoveOperation rows still
// present in session_moves so startup recovery can roll back source-owned
// moves or finish forward destination-owned moves.
func LoadLegacyMoveOperations() ([]LegacyMoveOperationRow, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := environmentTextRows(connection, 2, "SELECT session_id, operation_json FROM session_moves ORDER BY session_id")
	if err != nil {
		return nil, err
	}
	legacy := []LegacyMoveOperationRow{}
	for _, row := range rows {
		sessionID, text := row[0], row[1]
		normalized, err := normalizeStoredMove(connection, sessionID, text)
		if err != nil {
			continue
		}
		stored, err := core.DecodeStoredMoveOperationJSON(normalized)
		if err == nil && stored.Legacy != nil {
			legacy = append(legacy, LegacyMoveOperationRow{SessionID: sessionID, Operation: *stored.Legacy})
		}
	}
	return legacy, nil
}

// DeleteMoveOperation deletes a settled legacy MoveOperation row after
// startup recovery.
func DeleteMoveOperation(sessionID string) error {
	_, err := submitDatabaseWrite("delete_move_operation", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM session_moves WHERE session_id = ?1", sessionID)
		return struct{}{}, err
	})
	return err
}

// CommitMoveDestinationOwns is the single-transaction commit point for a
// cross-target Move: moves the operation to DestinationOwns and updates the
// session's placement to the destination.
func CommitMoveDestinationOwns(operation *core.MoveOperation, destinationSession *core.SessionRecord) error {
	if operation.Phase != core.MovePhaseDestinationOwns {
		return errors.New("commit_move_destination_owns requires MovePhase::DestinationOwns")
	}
	_, err := submitDatabaseWrite("commit_move_destination_owns", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := updateLifecycleFields(tx, destinationSession); err != nil {
			return struct{}{}, err
		}
		if err := replaceTargets(tx, destinationSession); err != nil {
			return struct{}{}, err
		}
		if err := saveMoveOperationWith(tx, operation); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// CommitInPlaceDispatchReleased is the single-transaction durable transition
// for an in-place same-target Move recording DispatchReleased before telling
// the worker to release its hold.
func CommitInPlaceDispatchReleased(operation *core.MoveOperation) error {
	if operation.Phase != core.MovePhaseDispatchReleased {
		return errors.New("commit_in_place_dispatch_released requires MovePhase::DispatchReleased")
	}
	_, err := submitDatabaseWrite("commit_in_place_dispatch_released", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := saveMoveOperationWith(tx, operation); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// exactMoveCloseIntent reads Move and close intent together; an
// unmatched/stale close ID is inert.
func exactMoveCloseIntent(decodedID, rowID string, closeID *string) bool {
	return decodedID == rowID && closeID != nil && *closeID == rowID
}

// MoveRecoveryOwner is the durable owner of an interrupted Move, shared by
// startup and intent GC.
type MoveRecoveryOwner int

const (
	MoveRecoveryOwnerMove MoveRecoveryOwner = iota
	MoveRecoveryOwnerClose
)

// MoveRecoveryOwnerFor is Rust's move_recovery_owner, renamed because the
// type takes the name. False means no owner.
func MoveRecoveryOwnerFor(state *core.SessionState, exactCloseIntent bool) (MoveRecoveryOwner, bool) {
	if exactCloseIntent {
		return MoveRecoveryOwnerClose, true
	}
	if state != nil && (*state == core.SessionStateClosing || *state == core.SessionStateDestroying) {
		return MoveRecoveryOwnerMove, true
	}
	return 0, false
}

// LoadMoveRecovery is every decodable intent and the operation IDs whose
// close intent exactly matches them.
func LoadMoveRecovery() ([]core.MoveOperation, map[string]struct{}, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, nil, err
	}
	defer connection.Close()
	type moveRow struct {
		sessionID, operationID, text string
		closeID                      *string
	}
	var rows []moveRow
	err = func() error {
		result, err := connection.Query("SELECT session_id, operation_id, operation_json, close_requested_operation_id FROM session_moves ORDER BY session_id")
		if err != nil {
			return err
		}
		defer result.Close()
		for result.Next() {
			var row moveRow
			if err := result.Scan(&row.sessionID, &row.operationID, &row.text, &row.closeID); err != nil {
				return err
			}
			rows = append(rows, row)
		}
		return result.Err()
	}()
	if err != nil {
		return nil, nil, err
	}
	operations := []core.MoveOperation{}
	closes := map[string]struct{}{}
	for _, row := range rows {
		if operation := decodeMoveOperation(connection, row.sessionID, row.text); operation != nil {
			if exactMoveCloseIntent(operation.OperationID, row.operationID, row.closeID) {
				closes[row.operationID] = struct{}{}
			}
			operations = append(operations, *operation)
		}
	}
	return operations, closes, nil
}

func loadMoveOperationsWith(connection Queryer) ([]core.MoveOperation, error) {
	// The daemon loads every intent at startup, so one undecodable row
	// would otherwise stop the daemon from starting; decodeMoveOperation
	// says why such a row is skipped rather than fatal.
	rows, err := environmentTextRows(connection, 2, "SELECT session_id, operation_json FROM session_moves ORDER BY session_id")
	if err != nil {
		return nil, err
	}
	operations := []core.MoveOperation{}
	for _, row := range rows {
		if operation := decodeMoveOperation(connection, row[0], row[1]); operation != nil {
			operations = append(operations, *operation)
		}
	}
	return operations, nil
}

// movePathEqual is PathBuf equality: by components, so a repeated or
// trailing separator and an interior "." do not make two paths differ.
func movePathEqual(a, b string) bool {
	components := func(path string) []string {
		separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
		var parts []string
		if path != "" && separator(rune(path[0])) {
			parts = append(parts, "/")
		}
		for index, part := range strings.FieldsFunc(path, separator) {
			if part == "." && !(index == 0 && parts == nil && strings.HasPrefix(path, ".")) {
				continue
			}
			parts = append(parts, part)
		}
		return parts
	}
	return slices.Equal(components(a), components(b))
}

func MoveCheckpointIsRetained(path string) (bool, error) {
	operations, err := LoadMoveOperations()
	if err != nil {
		return false, err
	}
	for i := range operations {
		for _, checkpoint := range operations[i].RetainedArchives() {
			if movePathEqual(checkpoint.ArchivePath, path) {
				return true, nil
			}
		}
	}
	return false, nil
}

// ReapFinishedMoveIntents deletes the rows of finished moves that can no
// longer act, and reports how many went away.
//
// A finished move keeps its row only for what still reads it: the resume
// dialog's "move needs recovery" offer and the viewer's retry projection,
// both of which restore a destination from the move's own checkpoint
// archive. Once that archive is gone the offer cannot do anything, so the row
// is litter every reader has to skip, and a lingering row is what wedged
// daemon startup once its harness was removed (BrokkAi/mjolnir#1026).
//
// MoveOperation.RetainsCheckpoint already draws the line: it is false only
// for a completed or cancelled move that is not mid queue admission. A move
// that is preparing, closing, resuming, starting a queue, failed, or holding
// a partly admitted queue keeps its row however its archive looks. Deleting
// rows needs no migration.
func ReapFinishedMoveIntents() (int, error) {
	return submitDatabaseWrite("reap_finished_move_intents", func(connection *Connection) (int, error) {
		return reapFinishedMoveIntentsWith(connection)
	})
}

func reapFinishedMoveIntentsWith(connection Queryer) (int, error) {
	operations, err := loadMoveOperationsWith(connection)
	if err != nil {
		return 0, err
	}
	reaped := 0
	for i := range operations {
		operation := &operations[i]
		var state, closeID *string
		if err := connection.QueryRow("SELECT s.state, m.close_requested_operation_id FROM session_moves m LEFT JOIN sessions s ON s.session_id=m.session_id WHERE m.session_id=?1 AND m.operation_id=?2",
			operation.Selection.SessionID, operation.OperationID).Scan(&state, &closeID); err != nil {
			return 0, err
		}
		var stored *core.SessionState
		if state != nil {
			value := storedSessionState(*state)
			stored = &value
		}
		if _, owned := MoveRecoveryOwnerFor(stored, exactMoveCloseIntent(operation.OperationID, operation.OperationID, closeID)); owned {
			continue
		}
		ownsPrepared := operation.Phase != core.MovePhaseCompleted &&
			operation.PreparedDestination != nil && operation.PreparedDestination.OwnsResource()
		archivePresent := false
		if artifact := operation.RestoreArtifact(); artifact != nil {
			_, err := os.Stat(artifact.ArchivePath)
			archivePresent = err == nil
		}
		if ownsPrepared || operation.RetainsCheckpoint() || len(operation.GrantedKeys) > 0 || archivePresent {
			continue
		}
		result, err := connection.Exec("DELETE FROM session_moves WHERE session_id=?1 AND operation_id=?2",
			operation.Selection.SessionID, operation.OperationID)
		if err != nil {
			return 0, err
		}
		removed, err := result.RowsAffected()
		if err != nil {
			return 0, err
		}
		if removed > 0 {
			slog.Debug("reaped the durable row of a finished move whose checkpoint is gone",
				"session_id", operation.Selection.SessionID,
				"operation_id", operation.OperationID,
				"phase", operation.Phase)
			reaped += int(removed)
		}
	}
	return reaped, nil
}

func RequestMoveCancellation(sessionID string) error {
	_, err := submitDatabaseWrite("request_move_cancellation", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec(`UPDATE session_moves SET operation_json=json_set(operation_json, '$.cancellation_requested', json('true'))
             WHERE session_id=?1 AND json_extract(operation_json, '$.phase') IN (
                 'capturing','transferring','restoring','destination_starting',
                 'destination_owns','source_retired','queue_admitted','dispatch_released',
                 'preparing','closing_source','resuming_destination','starting_queue'
             )`, sessionID)
		return struct{}{}, err
	})
	return err
}

func ClearMoveCancellationForRetry(sessionID string) error {
	_, err := submitDatabaseWrite("clear_move_cancellation_for_retry", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE session_moves SET operation_json=json_set(operation_json, '$.cancellation_requested', json('false')) WHERE session_id=?1", sessionID)
		return struct{}{}, err
	})
	return err
}

// MovePendingWork reads the confirmation data without loading the
// conversation history: whether a turn is running, and the queued prompts.
func MovePendingWork(sessionID string) (bool, []core.MaterializedQueuedPrompt, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return false, nil, err
	}
	defer connection.Close()
	running, err := environmentOptionalString(connection, "SELECT execution_state FROM materialized_sessions WHERE session_id=?1", sessionID)
	if err != nil {
		return false, nil, err
	}
	prompts, err := readMaterializedQueuedPrompts(connection, sessionID)
	if err != nil {
		return false, nil, err
	}
	return running != nil && *running == "running", prompts, nil
}

// MoveSourceEnvironment: cleanup uses the accepted source owner, independent
// of the current binding.
func MoveSourceEnvironment(operationID string) (*EnvironmentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	id, err := environmentOptionalString(connection, "SELECT source_environment_id FROM session_moves WHERE operation_id=?1", operationID)
	if err != nil {
		return nil, err
	}
	return environmentByID(connection, id)
}

func RetainedMoveEnvironment(operationID string) (*EnvironmentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	id, err := environmentOptionalString(connection, "SELECT environment_id FROM retained_move_sources WHERE operation_id=?1", operationID)
	if err != nil {
		return nil, err
	}
	return environmentByID(connection, id)
}
