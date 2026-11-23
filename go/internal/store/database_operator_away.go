// Port of mj-store/src/database/operator_away.rs.

// Away notices share the projection transaction and its replay frontier.

package store

import (
	"database/sql"
	"errors"
	"fmt"
	"math"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// operatorAwayRequestLimit is how many characters of a captured request an
// away notice repeats.
const operatorAwayRequestLimit = 4096

func boundedRequest(text string) string {
	if utf8.RuneCountInString(text) <= operatorAwayRequestLimit {
		return text
	}
	return string([]rune(text)[:operatorAwayRequestLimit]) + "\n[request truncated]"
}

// enqueue queues an operator-away mailbox event in the projection
// transaction.
func enqueue(tx *Transaction, session string, key string, atMs int64, body core.MailboxEventBody) error {
	event := core.MailboxEvent{
		Key:         key,
		Source:      "operator_away",
		Wake:        true,
		CreatedAtMs: uint64(max(atMs, 0)),
		Body:        body,
	}
	encoded, err := databaseMarshalJSON(event)
	if err != nil {
		return err
	}
	_, err = EnqueueMailboxEventWith(tx, key, session, string(encoded), true, true)
	return err
}

// projectEvent captures input requests and their away resolutions as the
// projection applies them.
func projectEvent(tx *Transaction, session string, seq uint64, atMs int64, event core.ApiEventData) error {
	var required *core.ElicitationRequest
	var resolved *core.ApiEventDataInputResolved
	switch event := event.(type) {
	case core.ApiEventDataInputRequired:
		if event.Request == nil {
			return nil
		}
		required = event.Request
	case core.ApiEventDataInputResolved:
		resolved = &event
	default:
		return nil
	}
	// A child's questions go to its parent, not operator presence. Its
	// approvals wait for a person, so AFK answers and notices apply to them
	// as to any session's.
	var subagent bool
	if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)", session).Scan(&subagent); err != nil {
		return err
	}
	if subagent && required != nil && required.GoesToParent() {
		return nil
	}
	var away bool
	var period int64
	if err := tx.QueryRow("SELECT away,revision FROM operator_away WHERE singleton=1").Scan(&away, &period); err != nil {
		return err
	}
	if required != nil {
		// Captures survive API-event retention and page boundaries.
		_, err := tx.Exec(`INSERT INTO operator_away_requests(session_id,requested_seq,elicitation_id,request,period)
                VALUES (?1,?2,?3,?4,?5)`,
			session, int64(seq), required.ID, boundedRequest(required.QuestionText()), period)
		return err
	}
	// Match the most recent still-open capture, not a runtime-local id alone.
	var requested uint64
	var request string
	found, err := valuesOptional(tx.QueryRow(`SELECT requested_seq,request FROM operator_away_requests WHERE session_id=?1 AND requested_seq < ?2
                AND elicitation_id=?3 AND resolved_seq IS NULL ORDER BY requested_seq DESC LIMIT 1`,
		session, int64(seq), resolved.ElicitationID).Scan(&requested, &request))
	if err != nil {
		return err
	}
	if !found {
		if resolved.Action == core.OperatorAwayAction && !subagent {
			// A child question has no capture: older workers can still
			// answer it while away (#1444).
			return fmt.Errorf("operator away resolution for %s/%s has no captured request", session, resolved.ElicitationID)
		}
		return nil
	}
	if _, err := tx.Exec("UPDATE operator_away_requests SET resolved_seq=?3,action=?4 WHERE session_id=?1 AND requested_seq=?2",
		session, int64(requested), int64(seq), resolved.Action); err != nil {
		return err
	}
	if resolved.Action == core.OperatorAwayAction {
		// Projection owns the decision: a late observation must not tell the
		// agent the operator is still away.
		var body core.MailboxEventBody
		if away {
			if _, err := tx.Exec("UPDATE operator_away_requests SET period=?3 WHERE session_id=?1 AND requested_seq=?2",
				session, int64(requested), period); err != nil {
				return err
			}
			body = core.MailboxEventBodyOperatorAway{Request: request}
		} else {
			body = core.MailboxEventBodyOperatorBack{Requests: []string{request}}
		}
		if err := enqueue(tx, session, fmt.Sprintf("operator-away-%s-%d", session, seq), atMs, body); err != nil {
			return err
		}
	}
	if !away && resolved.Action != core.OperatorAwayAction {
		if _, err := tx.Exec("DELETE FROM operator_away_requests WHERE session_id=?1 AND requested_seq=?2",
			session, int64(requested)); err != nil {
			return err
		}
	}
	return nil
}

// setWith records operator presence; false when it did not change.
func setWith(conn *Connection, away bool, autoAfkWindow time.Duration) (bool, error) {
	tx, err := conn.BeginWith(TransactionImmediate)
	if err != nil {
		return false, err
	}
	defer tx.Rollback()
	var previous bool
	var period int64
	var changedAtMs *int64
	if err := tx.QueryRow("SELECT away,revision,changed_at_ms FROM operator_away WHERE singleton=1").
		Scan(&previous, &period, &changedAtMs); err != nil {
		return false, err
	}
	if previous == away {
		return false, nil
	}
	now := core.EpochMillis()
	if period == math.MaxInt64 {
		return false, errors.New("operator presence revision exhausted")
	}
	nextRevision := period + 1
	if away {
		if _, err := tx.Exec("UPDATE operator_away SET away=1,revision=?1,changed_at_ms=?2 WHERE singleton=1", nextRevision, now); err != nil {
			return false, err
		}
		if _, err := tx.Exec("DELETE FROM operator_away_requests WHERE resolved_seq IS NOT NULL"); err != nil {
			return false, err
		}
	} else {
		type recipient struct {
			session  string
			lastAway uint64
		}
		recipients, err := valuesQueryMap(tx, `SELECT r.session_id, max(r.resolved_seq) FROM operator_away_requests r JOIN sessions s ON s.session_id=r.session_id
            WHERE r.period=?1 AND r.action=?2 AND s.state IN ('running','disconnected','provisioning','checkpointing','parked')
            GROUP BY r.session_id`, []any{period, core.OperatorAwayAction}, func(rows *sql.Rows) (recipient, error) {
			var row recipient
			err := rows.Scan(&row.session, &row.lastAway)
			return row, err
		})
		if err != nil {
			return false, err
		}
		// A session the operator answered directly within the auto-AFK
		// window before leaving was attended, and one answered during the
		// away period through a path that does not count as activity got its
		// reply already: neither is told the operator is back, as if its
		// requests went unseen.
		var answeredThreshold *int64
		if changedAtMs != nil {
			window := autoAfkWindow.Milliseconds()
			threshold := *changedAtMs - window
			if window > 0 && *changedAtMs < math.MinInt64+window {
				threshold = math.MinInt64
			}
			answeredThreshold = &threshold
		}
		for _, recipient := range recipients {
			var protocol uint32
			known, err := valuesOptional(tx.QueryRow("SELECT protocol FROM operator_away_workers WHERE session_id=?1", recipient.session).Scan(&protocol))
			if err != nil {
				return false, err
			}
			if known && protocol < core.RelayOperatorAwayProtocol {
				continue
			}
			// A captured away resolution itself proves protocol >=45 when no
			// later connection has published a negotiated version yet.
			var blocked bool
			if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM operator_away_requests WHERE session_id=?1 AND period=?2 AND requested_seq > ?3 AND (action IS NULL OR action != ?4))",
				recipient.session, period, int64(recipient.lastAway), core.OperatorAwayAction).Scan(&blocked); err != nil {
				return false, err
			}
			if blocked {
				continue
			}
			if answeredThreshold != nil {
				var answered bool
				if err := tx.QueryRow(`SELECT EXISTS(SELECT 1 FROM api_events WHERE session_id=?1
                        AND json_extract(body,'$.type')='input_resolved'
                        AND json_extract(body,'$.data.action') != ?2
                        AND recorded_at_ms >= ?3)`,
					recipient.session, core.OperatorAwayAction, *answeredThreshold).Scan(&answered); err != nil {
					return false, err
				}
				if answered {
					continue
				}
			}
			requests, err := valuesQueryMap(tx, "SELECT request FROM operator_away_requests WHERE session_id=?1 AND period=?2 AND action=?3 ORDER BY requested_seq LIMIT 8",
				[]any{recipient.session, period, core.OperatorAwayAction}, func(rows *sql.Rows) (string, error) {
					var request string
					err := rows.Scan(&request)
					return request, err
				})
			if err != nil {
				return false, err
			}
			if requests == nil {
				requests = []string{}
			}
			if err := enqueue(tx, recipient.session, fmt.Sprintf("operator-back-%d-%s", period, recipient.session),
				core.EpochMillis(), core.MailboxEventBodyOperatorBack{Requests: requests}); err != nil {
				return false, err
			}
		}
		if _, err := tx.Exec("UPDATE operator_away SET away=0,revision=?1,changed_at_ms=?2 WHERE singleton=1", nextRevision, now); err != nil {
			return false, err
		}
	}
	if err := tx.Commit(); err != nil {
		return false, err
	}
	return true, nil
}
