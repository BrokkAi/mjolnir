// Port of mj-store/src/database/subagent_questions.rs.

// Child questions and their parent mailbox event share the projection
// transaction.

package store

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"log/slog"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// forwardSubagentQuestionWith forwards a child's question to its live parent
// as a session message; false when nothing was forwarded.
func forwardSubagentQuestionWith(tx *Transaction, child string, seq uint64, recordedAtMs int64, request *core.ElicitationRequest) (bool, error) {
	// Approvals stay with the person; the child's worker leaves them open.
	if !request.GoesToParent() {
		return false, nil
	}
	var parent, childState, rawTitle string
	var state, titleOverride, acpTitle *string
	found, err := valuesOptional(tx.QueryRow(`SELECT a.parent_session_id, p.state, c.state, c.title, c.session_title_override,
                    c.acp_session_title FROM subagent_sessions a
         JOIN sessions c ON c.session_id=a.child_session_id
         LEFT JOIN sessions p ON p.session_id=a.parent_session_id
         WHERE a.child_session_id=?1`, child).Scan(&parent, &state, &childState, &rawTitle, &titleOverride, &acpTitle))
	if err != nil {
		return false, err
	}
	if !found {
		return false, nil
	}
	// The name the session list shows, bounded to one line.
	title := core.BoundedOneLine(core.ListedSessionTitle(child, rawTitle, titleOverride, acpTitle), core.MessageNameChars)
	// The message invites a send_message answer; a child that can no longer
	// receive work could not receive it. The child's own finish or failure
	// report already tells the parent how it ended.
	childReceivable := false
	if stored, ok := core.SessionStateFromStored(childState); ok {
		childReceivable = stored.CanReceiveChildWork()
	}
	if !childReceivable {
		slog.Info("child question not forwarded; the child can no longer receive its answer",
			"child_session_id", child, "parent_session_id", parent, "child_state", childState,
			"elicitation_id", request.ID)
		return false, nil
	}
	parentLive := false
	if state != nil {
		switch *state {
		case "running", "disconnected", "provisioning", "checkpointing", "parked":
			parentLive = true
		}
	}
	if !parentLive {
		parentState := "None"
		if state != nil {
			parentState = fmt.Sprintf("Some(%s)", core.RustDebugString(*state))
		}
		slog.Warn("child question has no live parent; leaving its routing unchanged",
			"child_session_id", child, "parent_session_id", parent, "parent_state", parentState,
			"elicitation_id", request.ID)
		return false, nil
	}
	key := fmt.Sprintf("subagent-question-%s-%d", child, seq)
	// The first capture owns the name and payload even if the session is
	// renamed.
	var exists bool
	if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)", key).Scan(&exists); err != nil {
		return false, err
	}
	if exists {
		return false, nil
	}
	event := core.MailboxEvent{
		Key:         key,
		Source:      "session_message",
		Wake:        true,
		CreatedAtMs: uint64(max(recordedAtMs, 0)),
		Body: core.MailboxEventBodySessionMessage{
			From: core.Sender{Type: core.SenderSession, ID: child, Title: title},
			Text: fmt.Sprintf("Your sub-agent asked this question:\n\n%s\n\nIts question form is already closed. Answer with send_message to session_id %s.",
				request.QuestionText(), child),
		},
	}
	encoded, err := databaseMarshalJSON(event)
	if err != nil {
		return false, err
	}
	enqueued, err := EnqueueMailboxEventWith(tx, key, parent, string(encoded), true, true)
	if err != nil {
		return false, err
	}
	return enqueued.Inserted(), nil
}

// ReconcilePendingSubagentQuestions recovers the pending questions older
// workers left open, without replaying historical questions that were
// already answered before this build.
func ReconcilePendingSubagentQuestions() (int, error) {
	return submitDatabaseWrite("reconcile pending child questions", reconcilePendingSubagentQuestionsWith)
}

func reconcilePendingSubagentQuestionsWith(conn *Connection) (int, error) {
	tx, err := conn.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	type pendingRow struct{ child, body string }
	pending, err := valuesQueryMap(tx, `SELECT m.session_id, m.pending_elicitations_json
                FROM materialized_sessions m JOIN subagent_sessions a ON a.child_session_id=m.session_id
                WHERE m.pending_elicitations_json != '[]'`, nil, func(rows *sql.Rows) (pendingRow, error) {
		var row pendingRow
		err := rows.Scan(&row.child, &row.body)
		return row, err
	})
	if err != nil {
		return 0, err
	}
	inserted := 0
	for _, row := range pending {
		var requests []core.ElicitationRequest
		if err := json.Unmarshal([]byte(row.body), &requests); err != nil {
			return 0, err
		}
		for _, request := range requests {
			var seq uint64
			var recordedAt int64
			var body string
			found, err := valuesOptional(tx.QueryRow(`SELECT seq, recorded_at_ms, body FROM api_events WHERE session_id=?1
                     AND json_extract(body, '$.type')='input_required'
                     AND json_extract(body, '$.data.request.id')=?2 ORDER BY seq DESC LIMIT 1`, row.child, request.ID).
				Scan(&seq, &recordedAt, &body))
			if err != nil {
				return 0, err
			}
			if !found {
				slog.Warn("pending child question has no captured input event; cannot forward it",
					"child_session_id", row.child, "elicitation_id", request.ID)
				continue
			}
			data, err := core.UnmarshalApiEventData([]byte(body))
			if err != nil {
				return 0, err
			}
			if required, ok := data.(core.ApiEventDataInputRequired); ok && required.Request != nil {
				forwarded, err := forwardSubagentQuestionWith(tx, row.child, seq, recordedAt, required.Request)
				if err != nil {
					return 0, err
				}
				if forwarded {
					inserted++
				}
			}
		}
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return inserted, nil
}
