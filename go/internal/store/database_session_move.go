// Port of mj-store/src/database/session_move.rs.

// Move intent uses the same guarded writer as session lifecycle transitions.

package store

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"log/slog"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func RetainedMoveSources(sessionID string) ([]core.RetainedMoveSource, error) {
	panic("unported: mj-store/src/database/session_move.rs retained_move_sources")
}

func ForgetRetainedMoveSource(operationID string) error {
	panic("unported: mj-store/src/database/session_move.rs forget_retained_move_source")
}

// RemoveRevokedMoveGrantedKeys atomically removes revoked ephemeral SSH
// credentials from whichever MoveOperation row is currently persisted for
// sessionID, without overwriting any other field (so background cleanup can
// never clobber a newer Move admitted concurrently for the same session).
func RemoveRevokedMoveGrantedKeys(sessionID string, revoked []core.GrantedMoveKey, updatedAt string) error {
	panic("unported: mj-store/src/database/session_move.rs remove_revoked_move_granted_keys")
}

func SaveMoveOperation(operation *core.MoveOperation) error {
	panic("unported: mj-store/src/database/session_move.rs save_move_operation")
}

// AcceptMoveOperation records the person's requests atomically with
// acceptance of the Move intent.
func AcceptMoveOperation(operation *core.MoveOperation) error {
	panic("unported: mj-store/src/database/session_move.rs accept_move_operation")
}

// AcceptUnchangedMove: an unchanged Move still accepts the person's explicit
// next-creation choices.
func AcceptUnchangedMove(selection *core.MoveSelection) error {
	panic("unported: mj-store/src/database/session_move.rs accept_unchanged_move")
}

func updateMoveRequests(tx *sql.Tx, selection *core.MoveSelection) error {
	panic("unported: mj-store/src/database/session_move.rs update_move_requests")
}

// AdoptMoveDestination publishes destination adoption and ownership
// together, without changing client fields.
func AdoptMoveDestination(operation *core.MoveOperation, session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/session_move.rs adopt_move_destination")
}

// SaveMoveOutcome publishes a Move result and its session message as one
// committed outcome. A failed rollback can settle its own Closing record as
// retryable Error. Other session fields retain the values written by their
// own owners.
func SaveMoveOutcome(operation *core.MoveOperation, lastError *string, failClosingSource bool) error {
	panic("unported: mj-store/src/database/session_move.rs save_move_outcome")
}

func saveMoveOutcomeWith(connection *sql.Conn, operation *core.MoveOperation, lastError *string, failClosingSource bool) error {
	panic("unported: mj-store/src/database/session_move.rs save_move_outcome_with")
}

// saveMoveOperationWith writes the intent in the caller's transaction, or in
// its own when connection is a *sql.Conn outside one (Rust checks
// is_autocommit).
func saveMoveOperationWith(connection Connection, operation *core.MoveOperation) error {
	panic("unported: mj-store/src/database/session_move.rs save_move_operation_with")
}

func LoadMoveOperation(sessionID string) (*core.MoveOperation, error) {
	panic("unported: mj-store/src/database/session_move.rs load_move_operation")
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

func loadMoveOperationWith(connection Connection, sessionID string) (*core.MoveOperation, error) {
	var text string
	err := connection.QueryRowContext(context.Background(),
		"SELECT operation_json FROM session_moves WHERE session_id=?1", sessionID).Scan(&text)
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
func resolveCancelledMoveForCloseWith(connection Connection, sessionID, operationID, updatedAt string) error {
	panic("unported: mj-store/src/database/session_move.rs resolve_cancelled_move_for_close_with")
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
func decodeMoveOperation(connection Connection, sessionID, text string) *core.MoveOperation {
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
func normalizeStoredMove(connection Connection, sessionID, text string) (string, error) {
	panic("unported: mj-store/src/database/session_move.rs normalize_stored_move")
}

// foldOrchestrators is normalize_stored_move's inner fold_orchestrators:
// record and sourcePolicy are jsonvalue-form values, and record is changed
// in place.
func foldOrchestrators(record *any, sourcePolicy any) error {
	panic("unported: mj-store/src/database/session_move.rs fold_orchestrators")
}

func LoadMoveOperations() ([]core.MoveOperation, error) {
	panic("unported: mj-store/src/database/session_move.rs load_move_operations")
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
	panic("unported: mj-store/src/database/session_move.rs load_legacy_move_operations")
}

// DeleteMoveOperation deletes a settled legacy MoveOperation row after
// startup recovery.
func DeleteMoveOperation(sessionID string) error {
	panic("unported: mj-store/src/database/session_move.rs delete_move_operation")
}

// CommitMoveDestinationOwns is the single-transaction commit point for a
// cross-target Move: moves the operation to DestinationOwns and updates the
// session's placement to the destination.
func CommitMoveDestinationOwns(operation *core.MoveOperation, destinationSession *core.SessionRecord) error {
	panic("unported: mj-store/src/database/session_move.rs commit_move_destination_owns")
}

// CommitInPlaceDispatchReleased is the single-transaction durable transition
// for an in-place same-target Move recording DispatchReleased before telling
// the worker to release its hold.
func CommitInPlaceDispatchReleased(operation *core.MoveOperation) error {
	panic("unported: mj-store/src/database/session_move.rs commit_in_place_dispatch_released")
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
	panic("unported: mj-store/src/database/session_move.rs load_move_recovery")
}

func loadMoveOperationsWith(connection Connection) ([]core.MoveOperation, error) {
	panic("unported: mj-store/src/database/session_move.rs load_move_operations_with")
}

func MoveCheckpointIsRetained(path string) (bool, error) {
	panic("unported: mj-store/src/database/session_move.rs move_checkpoint_is_retained")
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
	panic("unported: mj-store/src/database/session_move.rs reap_finished_move_intents")
}

func reapFinishedMoveIntentsWith(connection Connection) (int, error) {
	panic("unported: mj-store/src/database/session_move.rs reap_finished_move_intents_with")
}

func RequestMoveCancellation(sessionID string) error {
	panic("unported: mj-store/src/database/session_move.rs request_move_cancellation")
}

func ClearMoveCancellationForRetry(sessionID string) error {
	panic("unported: mj-store/src/database/session_move.rs clear_move_cancellation_for_retry")
}

// MovePendingWork reads the confirmation data without loading the
// conversation history: whether a turn is running, and the queued prompts.
func MovePendingWork(sessionID string) (bool, []core.MaterializedQueuedPrompt, error) {
	panic("unported: mj-store/src/database/session_move.rs move_pending_work")
}

// MoveSourceEnvironment: cleanup uses the accepted source owner, independent
// of the current binding.
func MoveSourceEnvironment(operationID string) (*EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/session_move.rs move_source_environment")
}

func RetainedMoveEnvironment(operationID string) (*EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/session_move.rs retained_move_environment")
}
