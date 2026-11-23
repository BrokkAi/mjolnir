// Port of mj-store/src/database/mailbox.rs.

// Durable mailbox event outbox. Delivery is owned by the controller service.

package store

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const MailboxClearedReason = "Discarded by /clear"

// MailboxDeliveryState is one outbox row's durable delivery status.
type MailboxDeliveryState interface {
	isMailboxDeliveryState()
}

type MailboxDeliveryStatePending struct{}

type MailboxDeliveryStateAccepted struct{ Value uint64 }

type MailboxDeliveryStateDiscarded struct{}

type MailboxDeliveryStateFailed struct{ Value string }

func (MailboxDeliveryStatePending) isMailboxDeliveryState()   {}
func (MailboxDeliveryStateAccepted) isMailboxDeliveryState()  {}
func (MailboxDeliveryStateDiscarded) isMailboxDeliveryState() {}
func (MailboxDeliveryStateFailed) isMailboxDeliveryState()    {}

// MailboxDeliveryStateOf reads a delivery's status. Durable status is
// authoritative even for delivery batches fetched earlier (Rust's
// mailbox_delivery_state, named apart from the type).
func MailboxDeliveryStateOf(sessionID, eventKey string) (MailboxDeliveryState, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return mailboxDeliveryStateWith(connection, sessionID, eventKey)
}

func mailboxDeliveryStateWith(conn Queryer, sessionID, eventKey string) (MailboxDeliveryState, error) {
	var accepted bool
	var ordinal *uint64
	var failure *string
	if err := conn.QueryRow(`SELECT accepted_at IS NOT NULL, accepted_ordinal, failure FROM mailbox_outbox
             WHERE event_key=?1 AND target_session_id=?2`, eventKey, sessionID).Scan(&accepted, &ordinal, &failure); err != nil {
		return nil, fmt.Errorf("read mailbox delivery %s for session %s: %w", eventKey, sessionID, err)
	}
	switch {
	case failure != nil && *failure == MailboxClearedReason:
		return MailboxDeliveryStateDiscarded{}, nil
	case failure != nil:
		return MailboxDeliveryStateFailed{Value: *failure}, nil
	case accepted:
		if ordinal == nil {
			return nil, errors.New("accepted mailbox event has no relay ordinal")
		}
		return MailboxDeliveryStateAccepted{Value: *ordinal}, nil
	}
	return MailboxDeliveryStatePending{}, nil
}

// clearMailboxWith records a /clear receipt. The receipt survives projection
// rebuilds. Cleanup and its completion frontier commit together, so
// historical replay cannot discard newer mail.
func clearMailboxWith(conn Queryer, sessionID string, ordinal uint64) error {
	result, err := conn.Exec(`INSERT INTO mailbox_reset_frontiers(session_id, through_ordinal) VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET through_ordinal=excluded.through_ordinal
         WHERE mailbox_reset_frontiers.through_ordinal < excluded.through_ordinal`, sessionID, int64(ordinal))
	if err != nil {
		return err
	}
	changed, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if changed > 0 {
		if _, err := conn.Exec(`UPDATE mailbox_outbox SET accepted_at=?2, failure=?3
             WHERE target_session_id=?1 AND accepted_at IS NULL`,
			sessionID, core.FormatChronoRFC3339(time.Now()), MailboxClearedReason); err != nil {
			return err
		}
	}
	return nil
}

type MailboxOutboxEntry struct {
	EventKey        string
	TargetSessionID string
	EventJSON       string
	Unpark          bool
}

type SubagentMailboxMessage struct {
	EventKey          string
	TargetSessionID   string
	EventJSON         string
	Accepted          bool
	Failure           *string
	AcceptedCommandID *string
	AcceptedOrdinal   *uint64
}

// Enqueued is what an enqueue did. The store ignores an event for a session
// that is gone (schema revision 121), so the caller learns there is no one to
// deliver to and decides whether that is an error for its sender.
type Enqueued interface {
	isEnqueued()
	// Inserted is whether this enqueue inserted the event.
	Inserted() bool
}

type EnqueuedInserted struct{}

type EnqueuedAlreadyQueued struct{}

type EnqueuedTargetGone struct{}

// EnqueuedFailed is recorded in the same transaction, but permanently
// undeliverable.
type EnqueuedFailed struct{ Value string }

func (EnqueuedInserted) isEnqueued()      {}
func (EnqueuedAlreadyQueued) isEnqueued() {}
func (EnqueuedTargetGone) isEnqueued()    {}
func (EnqueuedFailed) isEnqueued()        {}

func (EnqueuedInserted) Inserted() bool      { return true }
func (EnqueuedAlreadyQueued) Inserted() bool { return false }
func (EnqueuedTargetGone) Inserted() bool    { return false }
func (EnqueuedFailed) Inserted() bool        { return false }

// EnqueueMailboxEvent inserts one event once. Reusing a key for a different
// target or payload is a producer error; silently treating it as the
// original event could deliver the wrong message to the wrong session.
func EnqueueMailboxEvent(eventKey, targetSessionID, eventJSON string, wake, unpark bool) (Enqueued, error) {
	inserted, err := submitDatabaseWrite("enqueue_mailbox_event", func(connection *Connection) (Enqueued, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		result, err := EnqueueMailboxEventWith(tx, eventKey, targetSessionID, eventJSON, wake, unpark)
		if err != nil {
			return nil, err
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		return result, nil
	})
	if err != nil {
		return nil, err
	}
	if _, ok := inserted.(EnqueuedInserted); ok {
		NotifyMailboxOutboxChanged()
	}
	// The transaction must commit its failure receipt before returning the
	// known failure to the sender. Returning an error inside it would roll it
	// back.
	if failed, ok := inserted.(EnqueuedFailed); ok {
		return nil, core.RefusalPrecondition(failed.Value)
	}
	return inserted, nil
}

func EnqueueMailboxEventWith(conn Queryer, eventKey, targetSessionID, eventJSON string, wake, unpark bool) (Enqueued, error) {
	var event core.MailboxEvent
	if err := json.Unmarshal([]byte(eventJSON), &event); err != nil {
		return nil, err
	}
	if event.Key != eventKey || event.Wake != wake {
		return nil, errors.New("mailbox event JSON does not match its outbox key and wake value")
	}
	// The producer holds the writer transaction: a sender with an older
	// runtime view cannot enqueue pending work after the durable failure.
	var failure *string
	if _, parentMessage := event.Body.(core.MailboxEventBodyParentMessage); parentMessage {
		var state string
		var lastError *string
		var archived bool
		found, err := valuesOptional(conn.QueryRow("SELECT state, last_error, archived FROM sessions WHERE session_id=?1", targetSessionID).
			Scan(&state, &lastError, &archived))
		if err != nil {
			return nil, err
		}
		if found {
			if state, ok := core.SessionStateFromStored(state); ok {
				failure = terminalParentMessageFailure(state, lastError, archived)
			}
		}
	}
	now := core.FormatChronoRFC3339(time.Now())
	var acceptedAt *string
	if failure != nil {
		acceptedAt = &now
	}
	result, err := conn.Exec(`INSERT INTO mailbox_outbox(event_key, target_session_id, event_json, wake, unpark, created_at, accepted_at, failure)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(event_key) DO NOTHING`,
		eventKey, targetSessionID, eventJSON, wake, unpark, now, acceptedAt, failure)
	if err != nil {
		return nil, err
	}
	changes, err := result.RowsAffected()
	if err != nil {
		return nil, err
	}
	inserted := changes == 1
	var existingTarget, existingJSON string
	var existingWake bool
	var existingFailure *string
	found, err := valuesOptional(conn.QueryRow("SELECT target_session_id, event_json, wake, failure FROM mailbox_outbox WHERE event_key=?1", eventKey).
		Scan(&existingTarget, &existingJSON, &existingWake, &existingFailure))
	if err != nil {
		return nil, err
	}
	if !found {
		return EnqueuedTargetGone{}, nil
	}
	var original core.MailboxEvent
	if err := json.Unmarshal([]byte(existingJSON), &original); err != nil {
		return nil, err
	}
	sameBody, err := mailboxBodiesEqual(original.Body, event.Body)
	if err != nil {
		return nil, err
	}
	if existingTarget != targetSessionID || original.Key != event.Key || !sameBody ||
		original.Wake != event.Wake || existingWake != wake {
		return nil, core.RefusalPrecondition(fmt.Sprintf(
			"mailbox event key %s already names a different session, body, or wake value", core.RustDebugString(eventKey)))
	}
	if existingFailure != nil && *existingFailure != MailboxClearedReason {
		return EnqueuedFailed{Value: *existingFailure}, nil
	}
	// The durable row is authoritative on a retry. Timestamp, source, and
	// unpark metadata from a later request do not rewrite the first event.
	if inserted {
		return EnqueuedInserted{}, nil
	}
	return EnqueuedAlreadyQueued{}, nil
}

// mailboxBodiesEqual is the derived PartialEq of two decoded event bodies,
// compared through their canonical encoding.
func mailboxBodiesEqual(left, right core.MailboxEventBody) (bool, error) {
	leftJSON, err := databaseMarshalJSON(left)
	if err != nil {
		return false, err
	}
	rightJSON, err := databaseMarshalJSON(right)
	if err != nil {
		return false, err
	}
	return bytes.Equal(leftJSON, rightJSON), nil
}

// PendingMailboxEvents selects a bounded batch for each target so blocked
// targets cannot starve others.
func PendingMailboxEvents(limit int) ([]MailboxOutboxEntry, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return valuesQueryMap(connection, `SELECT event_key, target_session_id, event_json, unpark
         FROM (
             SELECT event_key, target_session_id, event_json, unpark, created_at,
                    ROW_NUMBER() OVER (PARTITION BY target_session_id ORDER BY created_at, event_key) AS position
             FROM mailbox_outbox WHERE accepted_at IS NULL
         ) WHERE position <= ?1
         ORDER BY target_session_id, created_at, event_key`, []any{int64(min(uint64(limit), math.MaxInt64))},
		func(rows *sql.Rows) (MailboxOutboxEntry, error) {
			var entry MailboxOutboxEntry
			err := rows.Scan(&entry.EventKey, &entry.TargetSessionID, &entry.EventJSON, &entry.Unpark)
			return entry, err
		})
}

func MailboxEventExists(eventKey string) (bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return false, err
	}
	defer connection.Close()
	var exists bool
	err = connection.QueryRow("SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)", eventKey).Scan(&exists)
	return exists, err
}

// PendingSubagentFinishedChildren is the child session IDs that have an
// undelivered SubagentFinished mailbox row pending for parentSessionID, as a
// set.
func PendingSubagentFinishedChildren(parentSessionID string) (map[string]struct{}, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	events, err := valuesQueryMap(connection, `SELECT event_json
         FROM mailbox_outbox
         WHERE target_session_id = ?1
           AND accepted_at IS NULL
           AND failure IS NULL`, []any{parentSessionID}, func(rows *sql.Rows) (string, error) {
		var event string
		err := rows.Scan(&event)
		return event, err
	})
	if err != nil {
		return nil, err
	}
	pending := map[string]struct{}{}
	for _, encoded := range events {
		var event core.MailboxEvent
		if json.Unmarshal([]byte(encoded), &event) != nil {
			continue
		}
		if finished, ok := event.Body.(core.MailboxEventBodySubagentFinished); ok && finished.Report != nil {
			pending[finished.Report.ChildSessionID] = struct{}{}
		}
	}
	return pending, nil
}

// SubagentMailboxMessages is the recent parent messages addressed to this
// session's children, including whether the child's relay has accepted each
// waking event.
func SubagentMailboxMessages(parentSessionID string) ([]SubagentMailboxMessage, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	messages, err := valuesQueryMap(connection, `SELECT mailbox_outbox.event_key, mailbox_outbox.target_session_id,
                mailbox_outbox.event_json,
                mailbox_outbox.accepted_at IS NOT NULL AND mailbox_outbox.failure IS NULL,
                mailbox_outbox.failure,
                mailbox_outbox.accepted_command_id, mailbox_outbox.accepted_ordinal
         FROM mailbox_outbox
         JOIN subagent_sessions
           ON subagent_sessions.child_session_id = mailbox_outbox.target_session_id
         WHERE subagent_sessions.parent_session_id = ?1
           AND mailbox_outbox.event_key LIKE 'subagent-message-%'
         ORDER BY mailbox_outbox.created_at DESC, mailbox_outbox.event_key DESC
         LIMIT 64`, []any{parentSessionID}, func(rows *sql.Rows) (SubagentMailboxMessage, error) {
		var message SubagentMailboxMessage
		err := rows.Scan(&message.EventKey, &message.TargetSessionID, &message.EventJSON, &message.Accepted,
			&message.Failure, &message.AcceptedCommandID, &message.AcceptedOrdinal)
		return message, err
	})
	if err != nil {
		return nil, err
	}
	if messages == nil {
		messages = []SubagentMailboxMessage{}
	}
	return messages, nil
}

func terminalParentMessageFailure(state core.SessionState, err *string, archived bool) *string {
	if !archived || (state != core.SessionStateError && state != core.SessionStateStartupCleanup) {
		return nil
	}
	var reason string
	if err != nil {
		reason = "child startup failed: " + *err
	} else {
		reason = "child startup failed and was archived; queued parent message was not delivered"
	}
	return &reason
}

// settleFailedParentMessagesWith reads finality from the committed columns in
// this owner's transaction.
func settleFailedParentMessagesWith(conn Queryer, session *core.SessionRecord) error {
	var state string
	var lastError *string
	var archived bool
	if err := conn.QueryRow("SELECT state,last_error,archived FROM sessions WHERE session_id=?1", session.ID).
		Scan(&state, &lastError, &archived); err != nil {
		return err
	}
	stored, ok := core.SessionStateFromStored(state)
	if !ok {
		return nil
	}
	if reason := terminalParentMessageFailure(stored, lastError, archived); reason != nil {
		if _, err := conn.Exec("UPDATE mailbox_outbox SET accepted_at=?2,failure=?3 WHERE target_session_id=?1 AND accepted_at IS NULL AND failure IS NULL AND accepted_command_id IS NULL AND accepted_ordinal IS NULL AND json_extract(event_json,'$.body.type')='parent_message'",
			session.ID, core.FormatChronoRFC3339(time.Now()), *reason); err != nil {
			return err
		}
	}
	return nil
}

// MarkMailboxEventFailed permanently stops retrying a mailbox event that the
// published worker protocol cannot represent. accepted_at also makes
// protocol-75 daemons skip the row; newer readers distinguish this marker
// through failure.
func MarkMailboxEventFailed(eventKey, failure string) error {
	_, err := submitDatabaseWrite("mark_mailbox_event_failed", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec(`UPDATE mailbox_outbox SET accepted_at=?2, failure=?3
             WHERE event_key=?1 AND accepted_at IS NULL AND failure IS NULL`,
			eventKey, core.FormatChronoRFC3339(time.Now()), failure)
		return struct{}{}, err
	})
	return err
}

func MarkMailboxEventAccepted(eventKey, commandID string, ordinal uint64) error {
	_, err := submitDatabaseWrite("mark_mailbox_event_accepted", func(connection *Connection) (struct{}, error) {
		return struct{}{}, markMailboxEventAcceptedWith(connection, eventKey, commandID, ordinal)
	})
	return err
}

func markMailboxEventAcceptedWith(conn Queryer, eventKey, commandID string, ordinal uint64) error {
	// A relay receipt is evidence of delivery; cleanup/protocol failures are
	// only an inference while the acknowledgement is in flight. Both writers
	// serialize on the row's absence of a receipt, so the receipt wins either
	// ordering. An explicit /clear discard remains terminal.
	_, err := conn.Exec(`UPDATE mailbox_outbox SET accepted_at=?2, accepted_command_id=?3,
                    accepted_ordinal=?4, failure=NULL
             WHERE event_key=?1 AND accepted_command_id IS NULL AND accepted_ordinal IS NULL
               AND (accepted_at IS NULL OR (failure IS NOT NULL AND failure<>?5))`,
		eventKey, core.FormatChronoRFC3339(time.Now()), commandID,
		int64(min(ordinal, math.MaxInt64)), MailboxClearedReason)
	return err
}

// PrunePendingMessagesForStoppedChildren removes only unaccepted
// parent-message rows for stopped children, which cannot receive parent
// messages and cannot be resumed by send_message; it keeps accepted delivery
// history and events addressed to stopped primary sessions.
func PrunePendingMessagesForStoppedChildren() (int, error) {
	return submitDatabaseWrite("prune pending messages for stopped children", func(connection *Connection) (int, error) {
		result, err := connection.Exec(`DELETE FROM mailbox_outbox
             WHERE accepted_at IS NULL
               AND unpark = 1
               AND event_key LIKE 'subagent-message-%'
               AND target_session_id IN (
                   SELECT session_id FROM sessions WHERE state = 'stopped'
               )`)
		if err != nil {
			return 0, err
		}
		removed, err := result.RowsAffected()
		return int(removed), err
	})
}

// PruneStaleSubagentQuestions removes forwarded questions whose asking child
// can no longer receive a send_message answer; the pending invitation can
// only fail. Accepted history stays.
func PruneStaleSubagentQuestions() (int, error) {
	return submitDatabaseWrite("prune stale subagent questions", func(connection *Connection) (int, error) {
		return pruneStaleSubagentQuestionsWith(connection)
	})
}

func pruneStaleSubagentQuestionsWith(conn Queryer) (int, error) {
	type question struct {
		key   string
		state *string
	}
	questions, err := valuesQueryMap(conn, `SELECT mailbox_outbox.event_key, sessions.state
                 FROM mailbox_outbox
                 LEFT JOIN sessions
                   ON sessions.session_id = json_extract(mailbox_outbox.event_json, '$.body.from.id')
                 WHERE mailbox_outbox.accepted_at IS NULL
                   AND mailbox_outbox.event_key LIKE 'subagent-question-%'`, nil,
		func(rows *sql.Rows) (question, error) {
			var row question
			err := rows.Scan(&row.key, &row.state)
			return row, err
		})
	if err != nil {
		return 0, err
	}
	removed := 0
	for _, row := range questions {
		// A child row that is gone entirely can receive nothing either.
		receivable := false
		if row.state != nil {
			if state, ok := core.SessionStateFromStored(*row.state); ok {
				receivable = state.CanReceiveChildWork()
			}
		}
		if receivable {
			continue
		}
		result, err := conn.Exec("DELETE FROM mailbox_outbox WHERE event_key=?1 AND accepted_at IS NULL", row.key)
		if err != nil {
			return 0, err
		}
		changed, err := result.RowsAffected()
		if err != nil {
			return 0, err
		}
		removed += int(changed)
	}
	return removed, nil
}
