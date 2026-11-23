// Port of mj-store/src/database/startup.rs.

package store

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"

	acpschema "github.com/BrokkAi/acp-go/schema"
	"modernc.org/sqlite"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type StartupDelivery struct {
	SessionID       string
	CommandID       string
	StepJSON        string
	Phase           string
	GroupID         *string
	AcceptedOrdinal *uint64
	Error           *string
}

// StartupGroupStatus is what status readers need from a session's latest
// startup group, computed once when the group is loaded, so the daemon keeps
// no per-step strings.
type StartupGroupStatus struct {
	GroupID *string
	// Failure is the first failed or rejecting step's error (nil inside when
	// that step recorded none); nil when no step failed.
	Failure **string
	// Pending is whether any step is still pending, delivering or accepted.
	Pending bool
	// LastDone is set when the last step is done: whether it was an API
	// prompt (or why its payload could not be read) and its acceptance
	// ordinal. Parse failures stay visible at the same decision point as
	// before.
	LastDone *StartupLastDone
}

// StartupLastDone is StartupGroupStatus's last finished step (Rust's
// (Result<bool, String>, Option<u64>)).
type StartupLastDone struct {
	// APIPrompt is whether the step was an API prompt; meaningful when
	// ParseError is empty.
	APIPrompt bool
	// ParseError is why the step's payload could not be read; empty when it
	// was read.
	ParseError      string
	AcceptedOrdinal *uint64
}

// StartupGroupStatusOf summarizes a group's steps; nil when the group has no
// steps (Rust's StartupGroupStatus::of).
func StartupGroupStatusOf(steps []StartupDelivery) *StartupGroupStatus {
	if len(steps) == 0 {
		return nil
	}
	last := steps[len(steps)-1]
	status := &StartupGroupStatus{GroupID: startupCloneString(last.GroupID)}
	for _, step := range steps {
		if step.Phase == "failed" || step.Phase == "rejecting" {
			failure := startupCloneString(step.Error)
			status.Failure = &failure
			break
		}
	}
	for _, step := range steps {
		if step.Phase == "pending" || step.Phase == "delivering" || step.Phase == "accepted" {
			status.Pending = true
			break
		}
	}
	if last.Phase == "done" {
		done := &StartupLastDone{}
		if last.AcceptedOrdinal != nil {
			ordinal := *last.AcceptedOrdinal
			done.AcceptedOrdinal = &ordinal
		}
		step, err := UnmarshalStartupStep([]byte(last.StepJSON))
		if err != nil {
			done.ParseError = err.Error()
		} else {
			_, done.APIPrompt = step.(StartupStepApiPrompt)
		}
		status.LastDone = done
	}
	return status
}

// startupCloneString copies an optional string, so a status shares nothing
// with the steps it summarizes.
func startupCloneString(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// startupDeliveryColumns is the column list every StartupDelivery read
// selects, in scanStartupDelivery's order.
const startupDeliveryColumns = "session_id,command_id,step_json,phase,group_id,accepted_ordinal,error"

func scanStartupDelivery(scan func(dest ...any) error) (StartupDelivery, error) {
	var delivery StartupDelivery
	err := scan(&delivery.SessionID, &delivery.CommandID, &delivery.StepJSON, &delivery.Phase,
		&delivery.GroupID, &delivery.AcceptedOrdinal, &delivery.Error)
	return delivery, err
}

func scanStartupDeliveryRows(rows *sql.Rows) (StartupDelivery, error) {
	return scanStartupDelivery(rows.Scan)
}

// startupRowsAffected is rusqlite's execute count.
func startupRowsAffected(result sql.Result, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	return result.RowsAffected()
}

// startupOptionalOrdinal binds an optional u64 ordinal.
func startupOptionalOrdinal(ordinal *uint64) any {
	if ordinal == nil {
		return nil
	}
	return int64(*ordinal)
}

func loadLatestStartupStatusWith(conn Queryer, sessionID string) (*StartupGroupStatus, error) {
	group, err := loadLatestStartupGroupWith(conn, sessionID)
	if err != nil {
		return nil, err
	}
	return StartupGroupStatusOf(group), nil
}

func EnqueueStartupDeliveries(deliveries []StartupDelivery) ([]StartupDelivery, error) {
	return submitDatabaseWrite("enqueue startup delivery", func(connection *Connection) ([]StartupDelivery, error) {
		tx, err := connection.Begin()
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		inserted, err := enqueueStartupDeliveriesWith(tx, deliveries)
		if err != nil {
			return nil, err
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		return inserted, nil
	})
}

func enqueueStartupDeliveriesWith(conn Queryer, deliveries []StartupDelivery) ([]StartupDelivery, error) {
	if len(deliveries) > 0 && deliveries[0].GroupID != nil {
		groupID := *deliveries[0].GroupID
		for _, delivery := range deliveries {
			if delivery.GroupID == nil || *delivery.GroupID != groupID {
				return nil, errors.New("startup batch has mixed group IDs")
			}
		}
		type identity struct{ sessionID, commandID, stepJSON string }
		existing, err := valuesQueryMap(conn, "SELECT session_id,command_id,step_json FROM startup_steps WHERE group_id=?1 ORDER BY sequence",
			[]any{groupID}, func(rows *sql.Rows) (identity, error) {
				var row identity
				err := rows.Scan(&row.sessionID, &row.commandID, &row.stepJSON)
				return row, err
			})
		if err != nil {
			return nil, err
		}
		if len(existing) > 0 {
			requested := make([]identity, 0, len(deliveries))
			for _, delivery := range deliveries {
				requested = append(requested, identity{delivery.SessionID, delivery.CommandID, delivery.StepJSON})
			}
			if !slices.Equal(existing, requested) {
				return nil, errors.New("startup group ID was reused with a different ordered payload")
			}
			return []StartupDelivery{}, nil
		}
	}
	inserted := []StartupDelivery{}
	for _, delivery := range deliveries {
		changed, err := startupRowsAffected(conn.Exec("INSERT INTO startup_steps(session_id,command_id,step_json,phase,group_id) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(command_id) DO NOTHING",
			delivery.SessionID, delivery.CommandID, delivery.StepJSON, delivery.GroupID))
		if err != nil {
			return nil, err
		}
		if changed != 0 {
			inserted = append(inserted, delivery)
			continue
		}
		var sessionID, stepJSON string
		var groupID *string
		if err := conn.QueryRow("SELECT session_id,step_json,group_id FROM startup_steps WHERE command_id=?1", delivery.CommandID).
			Scan(&sessionID, &stepJSON, &groupID); err != nil {
			return nil, err
		}
		sameGroup := (groupID == nil) == (delivery.GroupID == nil) && (groupID == nil || *groupID == *delivery.GroupID)
		if sessionID != delivery.SessionID || stepJSON != delivery.StepJSON || !sameGroup {
			return nil, client.StartupPromptIdentityConflict{}
		}
	}
	return inserted, nil
}

// LoadStartupDelivery reads a step in any phase, including a settled one.
// Callers that only want unfinished work use NextStartupDelivery.
func LoadStartupDelivery(commandID string) (*StartupDelivery, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	delivery, err := scanStartupDelivery(connection.QueryRow(`SELECT `+startupDeliveryColumns+`
             FROM startup_steps WHERE command_id=?1`, commandID).Scan)
	if found, err := valuesOptional(err); err != nil || !found {
		return nil, err
	}
	return &delivery, nil
}

func PrepareStartupDelivery(commandID string, stepJSON string) error {
	_, err := submitDatabaseWrite("prepare startup delivery", func(connection *Connection) (struct{}, error) {
		changed, err := startupRowsAffected(connection.Exec("UPDATE startup_steps SET step_json=?2 WHERE command_id=?1 AND phase='pending'", commandID, stepJSON))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("startup delivery no longer pending")
		}
		return struct{}{}, nil
	})
	return err
}

func LoadStartupDeliveries() ([]StartupDelivery, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	deliveries, err := valuesQueryMap(connection, "SELECT "+startupDeliveryColumns+" FROM startup_steps WHERE phase IN ('pending','delivering','accepted','cancelling','rejecting') ORDER BY sequence",
		nil, scanStartupDeliveryRows)
	if err != nil {
		return nil, err
	}
	if deliveries == nil {
		deliveries = []StartupDelivery{}
	}
	return deliveries, nil
}

func SetStartupDeliveryPhase(commandID, phase string, err *string) error {
	_, failure := submitDatabaseWrite("settle startup delivery", func(connection *Connection) (struct{}, error) {
		changed, err := startupRowsAffected(connection.Exec(`UPDATE startup_steps SET phase=?2,error=?3 WHERE command_id=?1 AND (
                    (?2='delivering' AND phase IN ('pending','delivering')) OR
                    (?2='done' AND phase IN ('accepted','done')) OR
                    (?2='failed' AND phase IN ('pending','delivering','rejecting','failed')) OR
                    (?2='dismissed' AND phase IN ('cancelling','dismissed'))
                )`, commandID, phase, err))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("startup delivery disappeared")
		}
		// Legacy ungrouped work has no status reader. Structured user prompts
		// retain terminal receipts for caller retries and preview
		// reconciliation.
		_, deleteErr := connection.Exec(`DELETE FROM startup_steps WHERE command_id=?1 AND group_id IS NULL
               AND phase IN ('done','failed','dismissed')
               AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'`, commandID)
		return struct{}{}, deleteErr
	})
	return failure
}

// FailRetainedStartupStep records that a peer message queued during startup
// will not be delivered.
//
// SetStartupDeliveryPhase deletes a failed ungrouped step that is not a user
// prompt. This row has to stay: it is the receipt a retry reads, so the same
// request is not sent again after startup has already failed.
func FailRetainedStartupStep(commandID, err string) error {
	_, failure := submitDatabaseWrite("retain failed session message", func(connection *Connection) (struct{}, error) {
		changed, err := startupRowsAffected(connection.Exec(`UPDATE startup_steps SET phase='failed',error=?2
                 WHERE command_id=?1
                   AND phase IN ('pending','delivering','accepted','cancelling','rejecting','failed')`, commandID, err))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("startup delivery disappeared")
		}
		return struct{}{}, nil
	})
	return failure
}

// ClaimStartupDelivery marks a step as delivering, unless it was withdrawn
// since the drain read it. False means another owner took the step; the
// drain reads the queue again and settles it.
func ClaimStartupDelivery(commandID string) (bool, error) {
	return submitDatabaseWrite("claim startup delivery", func(connection *Connection) (bool, error) {
		changed, err := startupRowsAffected(connection.Exec("UPDATE startup_steps SET phase='delivering',error=NULL WHERE command_id=?1 AND phase IN ('pending','delivering')", commandID))
		return changed == 1, err
	})
}

// WithdrawStartupPrompt withdraws exactly one caller-owned prompt before
// worker delivery claims it.
func WithdrawStartupPrompt(sessionID, commandID string) (bool, error) {
	return submitDatabaseWrite("withdraw startup prompt", func(connection *Connection) (bool, error) {
		changed, err := startupRowsAffected(connection.Exec(`UPDATE startup_steps SET phase='cancelling',error=NULL
             WHERE session_id=?1 AND command_id=?2 AND group_id IS NULL AND phase='pending'
             AND json_type(step_json, '$.Prompt')='object'`, sessionID, commandID))
		return changed == 1, err
	})
}

// RejectStartupPrompt keeps a definite refusal durable until its complete
// draft is restored. The writer commits draft recovery and row retirement
// together, so a crash never loses input or restores it twice.
func RejectStartupPrompt(commandID, reason string) error {
	_, err := submitDatabaseWrite("reject startup prompt", func(connection *Connection) (struct{}, error) {
		changed, err := startupRowsAffected(connection.Exec("UPDATE startup_steps SET phase='rejecting',error=?2 WHERE command_id=?1 AND group_id IS NULL AND phase IN ('pending','delivering','rejecting')", commandID, reason))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("startup prompt no longer available for recovery")
		}
		return struct{}{}, nil
	})
	return err
}

func RecoverStartupPrompt(commandID, draft string) error {
	_, err := submitDatabaseWrite("recover startup prompt", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if err := recoverStartupPromptWith(tx, commandID, draft); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func recoverStartupPromptWith(conn Queryer, commandID, draft string) error {
	var sessionID, existing, stepJSON string
	if err := conn.QueryRow("SELECT startup_steps.session_id,sessions.draft_input,startup_steps.step_json FROM startup_steps JOIN sessions USING(session_id) WHERE command_id=?1 AND group_id IS NULL AND phase='rejecting'", commandID).
		Scan(&sessionID, &existing, &stepJSON); err != nil {
		return err
	}
	step, err := containerSizeDecode([]byte(stepJSON))
	if err != nil {
		return err
	}
	inherited, isText := containerSizePointer(step, "/Prompt/inherited_draft").(string)
	preceding := existing
	if isText && inherited == existing {
		preceding = ""
	}
	preceding, err = core.RemoveStartupReceipt(preceding, commandID)
	if err != nil {
		return err
	}
	recovered, err := core.RemoveStartupReceipt(draft, commandID)
	if err != nil {
		return err
	}
	merged, err := core.MergeDrafts(preceding, recovered)
	if err != nil {
		return err
	}
	if _, err := conn.Exec("UPDATE sessions SET draft_input=?2 WHERE session_id=?1", sessionID, merged); err != nil {
		return err
	}
	if _, err := conn.Exec("UPDATE startup_steps SET phase='failed' WHERE command_id=?1 AND phase='rejecting'", commandID); err != nil {
		return err
	}
	_, err = conn.Exec("DELETE FROM startup_steps WHERE command_id=?1 AND phase='failed' AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'", commandID)
	return err
}

// startupIsDatabaseError is whether err came from SQLite or database/sql
// (Rust: downcasting to rusqlite::Error), as opposed to a draft that could
// not be read or merged.
func startupIsDatabaseError(err error) bool {
	var failure *sqlite.Error
	return errors.As(err, &failure) || errors.Is(err, sql.ErrNoRows) ||
		errors.Is(err, sql.ErrTxDone) || errors.Is(err, sql.ErrConnDone)
}

// startupPromptDraft is a startup prompt's draft: its encoded draft, or its
// text for rows from before drafts were encoded.
func startupPromptDraft(prompt any) (string, bool) {
	fields, ok := prompt.(map[string]any)
	if !ok {
		return "", false
	}
	if draft, ok := fields["encoded_draft"].(string); ok {
		return draft, true
	}
	if text, ok := fields["text"].(string); ok {
		return text, true
	}
	return "", false
}

// rejectFailedMoveStartupPromptsWith turns never-claimed Move prompts into
// recoverable drafts. The Move record and pending queue share the writer
// transaction: only work never claimed by a worker is turned into a
// recoverable draft.
func rejectFailedMoveStartupPromptsWith(conn Queryer, sessionID string) (int, error) {
	operation, err := loadMoveOperationWith(conn, sessionID)
	if err != nil || operation == nil {
		return 0, err
	}
	if operation.Phase != core.MovePhaseFailed && operation.Phase != core.MovePhaseCancelled {
		return 0, nil
	}
	outcome := "failed"
	if operation.Phase == core.MovePhaseCancelled {
		outcome = "was cancelled"
	}
	detail := "the destination was not ready"
	if operation.Error != nil {
		detail = *operation.Error
	}
	return rejectPendingMoveStartupPromptsWith(conn, sessionID, operation.OperationID, fmt.Sprintf("Move %s: %s", outcome, detail))
}

func rejectPendingMoveStartupPromptsWith(conn Queryer, sessionID, moveOperationID, reason string) (int, error) {
	changed, err := startupRowsAffected(conn.Exec("UPDATE startup_steps SET phase='rejecting',error=?2 WHERE session_id=?1 AND group_id IS NULL AND phase='pending' AND json_type(step_json, '$.Prompt')='object' AND json_extract(step_json, '$.Prompt.move_operation_id')=?3",
		sessionID, reason, moveOperationID))
	if err != nil {
		return 0, err
	}
	type rejected struct{ id, step string }
	rows, err := valuesQueryMap(conn, "SELECT command_id,step_json FROM startup_steps WHERE session_id=?1 AND group_id IS NULL AND phase='rejecting' AND json_type(step_json, '$.Prompt')='object' AND json_extract(step_json, '$.Prompt.move_operation_id')=?2 ORDER BY sequence",
		[]any{sessionID, moveOperationID}, func(rows *sql.Rows) (rejected, error) {
			var row rejected
			err := rows.Scan(&row.id, &row.step)
			return row, err
		})
	if err != nil {
		return 0, err
	}
	for _, row := range rows {
		step, err := containerSizeDecode([]byte(row.step))
		if err != nil {
			return 0, err
		}
		var prompt any
		if fields, ok := step.(map[string]any); ok {
			prompt = fields["Prompt"]
		}
		draft, ok := startupPromptDraft(prompt)
		if !ok {
			return 0, errors.New("Move prompt is missing draft")
		}
		if err := recoverStartupPromptWith(conn, row.id, draft); err != nil {
			if startupIsDatabaseError(err) {
				return 0, err
			}
			slog.Error("Move draft recovery remains saved", "session_id", sessionID, "command_id", row.id, "error", err)
		}
	}
	return int(changed), nil
}

func RejectPendingMoveStartupPrompts(sessionID, moveOperationID, reason string) error {
	_, err := submitDatabaseWrite("reject pending Move startup prompts", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if _, err := rejectPendingMoveStartupPromptsWith(tx, sessionID, moveOperationID, reason); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// StartupPromptOutcomes reads the terminal outcomes of up to 128 caller-owned
// startup prompts, keyed by command id.
func StartupPromptOutcomes(sessionID string, commandIDs []string) (map[string]client.StartupPromptOutcome, error) {
	if len(commandIDs) > 128 {
		return nil, errors.New("request at most 128 startup prompt outcomes")
	}
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	ids := commandIDs
	if ids == nil {
		ids = []string{}
	}
	encodedIDs, err := databaseMarshalJSON(ids)
	if err != nil {
		return nil, err
	}
	type outcomeRow struct {
		id, phase, step string
		err             *string
	}
	rows, err := valuesQueryMap(connection, "SELECT command_id,phase,step_json,error FROM startup_steps WHERE session_id=?1 AND command_id IN (SELECT value FROM json_each(?2)) AND group_id IS NULL AND json_type(step_json, '$.Prompt')='object'",
		[]any{sessionID, string(encodedIDs)}, func(rows *sql.Rows) (outcomeRow, error) {
			var row outcomeRow
			err := rows.Scan(&row.id, &row.phase, &row.step, &row.err)
			return row, err
		})
	if err != nil {
		return nil, err
	}
	outcomes := map[string]client.StartupPromptOutcome{}
	for _, row := range rows {
		var outcome client.StartupPromptOutcome
		switch row.phase {
		case "accepted", "done":
			outcome = client.StartupPromptOutcomeAccepted{}
		case "cancelling", "dismissed":
			outcome = client.StartupPromptOutcomeWithdrawn{}
		case "failed":
			step, err := containerSizeDecode([]byte(row.step))
			if err != nil {
				return nil, err
			}
			fields, _ := step.(map[string]any)
			prompt, ok := fields["Prompt"]
			if !ok {
				continue
			}
			draft, ok := startupPromptDraft(prompt)
			if !ok {
				return nil, errors.New("refused startup prompt is missing its draft")
			}
			reason := "prompt refused"
			if row.err != nil {
				reason = *row.err
			}
			outcome = client.StartupPromptOutcomeRefused{EncodedDraft: draft, Reason: reason}
		default:
			continue
		}
		outcomes[row.id] = outcome
	}
	return outcomes, nil
}

// LoadLatestStartupGroup is a session's latest startup group, the only one
// the API reports on. The daemon reads it from its published records
// (CommittedState's startup groups); this store read serves a process
// without the writer.
func LoadLatestStartupGroup(sessionID string) ([]StartupDelivery, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return loadLatestStartupGroupWith(connection, sessionID)
}

func LoadStartupGroup(sessionID, groupID string) ([]StartupDelivery, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return loadStartupGroupWith(connection, sessionID, &groupID)
}

func loadLatestStartupGroupWith(conn Queryer, sessionID string) ([]StartupDelivery, error) {
	return loadStartupGroupWith(conn, sessionID, nil)
}

func loadStartupGroupWith(conn Queryer, sessionID string, groupID *string) ([]StartupDelivery, error) {
	deliveries, err := valuesQueryMap(conn, `SELECT `+startupDeliveryColumns+`
         FROM startup_steps WHERE session_id=?1 AND group_id=coalesce(?2, (
            SELECT group_id FROM startup_steps WHERE session_id=?1 AND group_id IS NOT NULL
            ORDER BY sequence DESC LIMIT 1
         )) ORDER BY sequence`, []any{sessionID, groupID}, scanStartupDeliveryRows)
	if err != nil {
		return nil, err
	}
	if deliveries == nil {
		deliveries = []StartupDelivery{}
	}
	return deliveries, nil
}

func SetStartupDeliveryAccepted(commandID string, ordinal *uint64) error {
	_, err := submitDatabaseWrite("record startup acceptance", func(connection *Connection) (struct{}, error) {
		changed, err := startupRowsAffected(connection.Exec("UPDATE startup_steps SET phase='accepted',accepted_ordinal=?2,error=NULL WHERE command_id=?1 AND phase IN ('delivering','accepted')",
			commandID, startupOptionalOrdinal(ordinal)))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("startup delivery disappeared")
		}
		return struct{}{}, nil
	})
	return err
}

func FailStartupGroup(sessionID, groupID, err string) error {
	_, failure := submitDatabaseWrite("record startup rejection", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE startup_steps SET phase=CASE WHEN phase='pending' THEN 'failed' ELSE 'rejecting' END,error=?3 WHERE session_id=?1 AND group_id=?2 AND phase IN ('pending','delivering','accepted')",
			sessionID, groupID, err)
		return struct{}{}, err
	})
	return failure
}

func CancelStartupGroups(sessionID string) error {
	_, err := submitDatabaseWrite("cancel API startup delivery", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE startup_steps SET phase=CASE WHEN phase IN ('pending','delivering','accepted','cancelling','rejecting') THEN 'cancelling' ELSE 'dismissed' END,error=NULL WHERE session_id=?1 AND group_id IS NOT NULL",
			sessionID)
		return struct{}{}, err
	})
	return err
}

func DismissStartupGroup(sessionID, groupID string) error {
	_, err := submitDatabaseWrite("dismiss completed API startup", func(connection *Connection) (struct{}, error) {
		return struct{}{}, dismissStartupGroupWith(connection, sessionID, groupID)
	})
	return err
}

func dismissStartupGroupWith(conn Queryer, sessionID, groupID string) error {
	// A group that is still being rejected settles through the drain; a
	// finished group has been read by its API client and can go.
	if _, err := conn.Exec(`UPDATE startup_steps SET phase='cancelling',error=NULL
         WHERE session_id=?1 AND group_id=?2 AND phase='rejecting'`, sessionID, groupID); err != nil {
		return err
	}
	_, err := conn.Exec(`DELETE FROM startup_steps WHERE session_id=?1 AND group_id=?2
           AND phase IN ('done','failed','dismissed')
           AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'`, sessionID, groupID)
	return err
}

// dismissFailedStartupGroupsWith acknowledges settled launch failures on
// retry. A group with even one unfinished step may still need to reconcile
// with its original worker.
func dismissFailedStartupGroupsWith(conn Queryer, sessionID string) error {
	groups, err := valuesQueryMap(conn, `SELECT group_id FROM startup_steps WHERE session_id=?1 AND group_id IS NOT NULL
             GROUP BY group_id HAVING sum(phase='failed') > 0
                AND sum(phase NOT IN ('done','failed','dismissed')) = 0`, []any{sessionID},
		func(rows *sql.Rows) (string, error) {
			var group string
			err := rows.Scan(&group)
			return group, err
		})
	if err != nil {
		return err
	}
	for _, group := range groups {
		if err := dismissStartupGroupWith(conn, sessionID, group); err != nil {
			return err
		}
	}
	return nil
}

// PruneSettledStartupDeliveries removes rows that nothing will read again:
// dismissed groups, settled legacy steps outside any group, and settled
// groups that a newer group for the same session has superseded (the API
// reads only a session's latest group). Run at daemon start so a store
// upgraded from a build that kept them does not carry them forever. Returns
// how many rows it removed.
func PruneSettledStartupDeliveries() (int, error) {
	return submitDatabaseWrite("prune settled startup deliveries", func(connection *Connection) (int, error) {
		removed, err := startupRowsAffected(connection.Exec(`DELETE FROM startup_steps WHERE (phase='dismissed'
               OR (group_id IS NULL AND phase IN ('done','failed')))
               AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'`))
		if err != nil {
			return 0, err
		}
		superseded, err := startupRowsAffected(connection.Exec(`DELETE FROM startup_steps AS old
              WHERE old.group_id IS NOT NULL AND old.phase IN ('done','failed')
                AND old.group_id <> (
                    SELECT latest.group_id FROM startup_steps AS latest
                     WHERE latest.session_id = old.session_id
                       AND latest.group_id IS NOT NULL
                     ORDER BY latest.sequence DESC LIMIT 1)`))
		if err != nil {
			return 0, err
		}
		return int(removed + superseded), nil
	})
}

// FailUnavailableStartupGroups rejects pending API input of a session that
// is not starting or live. A late readiness failure must not fail a newer,
// already resumed session. Only pending API input can be rejected:
// delivering rows may be accepted.
func FailUnavailableStartupGroups(sessionID, err string) error {
	_, failure := submitDatabaseWrite("reject unavailable API startup", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec(`UPDATE startup_steps SET phase='failed',error=?2
             WHERE session_id=?1 AND group_id IS NOT NULL AND phase='pending'
               AND NOT EXISTS (SELECT 1 FROM sessions WHERE session_id=?1
                 AND state IN ('provisioning','running','disconnected','checkpointing'))`, sessionID, err)
		return struct{}{}, err
	})
	return failure
}

// NextStartupDelivery reads only the first unfinished step using the
// pending-session index.
func NextStartupDelivery(sessionID string) (*StartupDelivery, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	delivery, err := scanStartupDelivery(connection.QueryRow("SELECT "+startupDeliveryColumns+" FROM startup_steps WHERE session_id=?1 AND phase IN ('pending','delivering','accepted','cancelling','rejecting') ORDER BY sequence LIMIT 1", sessionID).Scan)
	if found, err := valuesOptional(err); err != nil || !found {
		return nil, err
	}
	return &delivery, nil
}

// StartupStep is one piece of work that waits for a starting session's
// harness.
//
// Both kinds are ordered against each other on purpose: a restored session's
// hand-off is the hidden context its first prompt reads, so it has to be
// installed before any queued prompt is submitted.
//
// It serializes as serde's externally tagged enum: {"<Variant>": <content>}.
// Decode one with UnmarshalStartupStep.
type StartupStep interface {
	isStartupStep()
}

type StartupStepInstallHandoff struct {
	Value *core.CanonicalSessionSnapshot
}

type StartupStepPreparedHandoff struct {
	Text string `json:"text"`
}

type StartupStepPrompt struct {
	Text string `json:"text"`
	// Prompt is nil only for text rows persisted before structured startup
	// prompts (Rust's None; serialized as null).
	Prompt          []acpschema.ContentBlock `json:"prompt"`
	EncodedDraft    *string                  `json:"encoded_draft"`
	MoveOperationID *string                  `json:"move_operation_id"`
	InheritedDraft  *string                  `json:"inherited_draft"`
}

type StartupStepConfigure struct {
	Key      string `json:"key"`
	Value    string `json:"value"`
	Optional bool   `json:"optional"`
}

type StartupStepApiPrompt struct {
	Text string `json:"text"`
}

// StartupStepSessionMessage is a peer message accepted while this session
// was still starting. Delivered after earlier startup steps: through the
// mailbox when the worker can take one, otherwise as the next turn.
// WaitedOnGroup is the startup group this message queued behind, when one was
// still pending. The request id is the command id's SessionMessageTurn
// suffix.
type StartupStepSessionMessage struct {
	SenderID      string  `json:"sender_id"`
	SenderTitle   string  `json:"sender_title"`
	Text          string  `json:"text"`
	CreatedAtMs   uint64  `json:"created_at_ms"`
	WaitedOnGroup *string `json:"waited_on_group"`
}

func (StartupStepInstallHandoff) isStartupStep()  {}
func (StartupStepPreparedHandoff) isStartupStep() {}
func (StartupStepPrompt) isStartupStep()          {}
func (StartupStepConfigure) isStartupStep()       {}
func (StartupStepApiPrompt) isStartupStep()       {}
func (StartupStepSessionMessage) isStartupStep()  {}

// startupStepJSON writes serde's externally tagged form {"<variant>":content}.
func startupStepJSON(variant string, content any) ([]byte, error) {
	encoded, err := databaseMarshalJSON(content)
	if err != nil {
		return nil, err
	}
	name, err := databaseMarshalJSON(variant)
	if err != nil {
		return nil, err
	}
	out := make([]byte, 0, len(name)+len(encoded)+3)
	out = append(out, '{')
	out = append(out, name...)
	out = append(out, ':')
	out = append(out, encoded...)
	return append(out, '}'), nil
}

func (v StartupStepInstallHandoff) MarshalJSON() ([]byte, error) {
	return startupStepJSON("InstallHandoff", v.Value)
}

func (v StartupStepPreparedHandoff) MarshalJSON() ([]byte, error) {
	type plain StartupStepPreparedHandoff
	return startupStepJSON("PreparedHandoff", plain(v))
}

func (v StartupStepPrompt) MarshalJSON() ([]byte, error) {
	type plain StartupStepPrompt
	return startupStepJSON("Prompt", plain(v))
}

func (v StartupStepConfigure) MarshalJSON() ([]byte, error) {
	type plain StartupStepConfigure
	return startupStepJSON("Configure", plain(v))
}

func (v StartupStepApiPrompt) MarshalJSON() ([]byte, error) {
	type plain StartupStepApiPrompt
	return startupStepJSON("ApiPrompt", plain(v))
}

func (v StartupStepSessionMessage) MarshalJSON() ([]byte, error) {
	type plain StartupStepSessionMessage
	return startupStepJSON("SessionMessage", plain(v))
}

// startupStepFields decodes a struct variant's content as an object and
// checks that every required field is present, as serde does.
func startupStepFields(content json.RawMessage, variant string, required ...string) (map[string]json.RawMessage, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(content, &fields); err != nil {
		return nil, err
	}
	if fields == nil {
		return nil, fmt.Errorf("invalid type: null, expected struct variant StartupStep::%s", variant)
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return nil, fmt.Errorf("missing field `%s`", name)
		}
	}
	return fields, nil
}

// UnmarshalStartupStep decodes serde's externally tagged StartupStep.
// Unknown fields inside a variant are ignored, as serde does without
// deny_unknown_fields.
func UnmarshalStartupStep(data []byte) (StartupStep, error) {
	var outer map[string]json.RawMessage
	if err := json.Unmarshal(data, &outer); err != nil {
		return nil, err
	}
	if len(outer) != 1 {
		return nil, errors.New("invalid type: map, expected map with a single key")
	}
	var variant string
	var content json.RawMessage
	for name, value := range outer {
		variant, content = name, value
	}
	switch variant {
	case "InstallHandoff":
		if bytes.Equal(bytes.TrimSpace(content), []byte("null")) {
			return nil, errors.New("invalid type: null, expected struct CanonicalSessionSnapshot")
		}
		snapshot := new(core.CanonicalSessionSnapshot)
		if err := json.Unmarshal(content, snapshot); err != nil {
			return nil, err
		}
		return StartupStepInstallHandoff{Value: snapshot}, nil
	case "PreparedHandoff":
		if _, err := startupStepFields(content, variant, "text"); err != nil {
			return nil, err
		}
		var step StartupStepPreparedHandoff
		type plain StartupStepPreparedHandoff
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "Prompt":
		if _, err := startupStepFields(content, variant, "text"); err != nil {
			return nil, err
		}
		var step StartupStepPrompt
		type plain StartupStepPrompt
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "Configure":
		if _, err := startupStepFields(content, variant, "key", "value", "optional"); err != nil {
			return nil, err
		}
		var step StartupStepConfigure
		type plain StartupStepConfigure
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "ApiPrompt":
		if _, err := startupStepFields(content, variant, "text"); err != nil {
			return nil, err
		}
		var step StartupStepApiPrompt
		type plain StartupStepApiPrompt
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "SessionMessage":
		if _, err := startupStepFields(content, variant, "sender_id", "sender_title", "text", "created_at_ms"); err != nil {
			return nil, err
		}
		var step StartupStepSessionMessage
		type plain StartupStepSessionMessage
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	default:
		return nil, fmt.Errorf("unknown variant `%s`, expected one of `InstallHandoff`, `PreparedHandoff`, `Prompt`, `Configure`, `ApiPrompt`, `SessionMessage`", variant)
	}
}

// StartFollowup is configuration and a first prompt to apply once a newly
// created session's harness is ready. Served in M2.
type StartFollowup struct {
	Model    *string
	Effort   *string
	Prompt   *string
	FastMode bool
}

// StartupStepID is one startup step's durable command id. The worker relay
// accepts only ASCII alphanumerics, '-' and '_' in a command id, so the step
// key is joined with '-', never ':': a rejected id leaves mj new --model and
// every spawned child waiting forever for a step the worker will never admit.
func StartupStepID(groupID, key string) string {
	return groupID + "-" + key
}

// StartupStepEntry is one of APIFollowupSteps's steps with its command id
// (Rust's (String, StartupStep)).
type StartupStepEntry struct {
	CommandID string
	Step      StartupStep
}

// APIFollowupSteps is the startup steps that apply followup, in order.
func APIFollowupSteps(groupID string, followup StartFollowup) []StartupStepEntry {
	var steps []StartupStepEntry
	for _, setting := range []struct {
		key   string
		value *string
	}{{"model", followup.Model}, {"effort", followup.Effort}} {
		if setting.value != nil {
			steps = append(steps, StartupStepEntry{StartupStepID(groupID, setting.key), StartupStepConfigure{Key: setting.key, Value: *setting.value}})
		}
	}
	if followup.FastMode {
		steps = append(steps, StartupStepEntry{StartupStepID(groupID, "fast-mode"), StartupStepConfigure{Key: "fast-mode", Value: "on", Optional: true}})
	}
	if followup.Prompt != nil {
		steps = append(steps, StartupStepEntry{StartupStepID(groupID, "prompt"), StartupStepApiPrompt{Text: *followup.Prompt}})
	}
	return steps
}
