// Port of mj-store/src/database/web_push.rs.

// Durable Web Push reservations and receipts, owned by the dispatch service.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"math"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func PushSubscribe(subscription Subscription) error {
	_, err := submitDatabaseWrite("subscribe Web Push device", func(db *Connection) (struct{}, error) {
		return struct{}{}, pushSubscribeWith(db, subscription)
	})
	return err
}

func pushSubscribeWith(db *Connection, subscription Subscription) error {
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := pushSubscribeIn(tx, subscription); err != nil {
		return err
	}
	return tx.Commit()
}

func pushSubscribeIn(db Queryer, subscription Subscription) error {
	encoded, err := databaseMarshalJSON(subscription)
	if err != nil {
		return err
	}
	var old, previousEnrollment string
	err = db.QueryRow("SELECT subscription,enrollment FROM push_devices WHERE endpoint=?1", subscription.Endpoint).
		Scan(&old, &previousEnrollment)
	previous := err == nil
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	var oldSubscription Subscription
	if previous {
		if err := json.Unmarshal([]byte(old), &oldSubscription); err != nil {
			return err
		}
	}
	var enrollment string
	if previous && oldSubscription.Keys == subscription.Keys {
		// Expiry metadata does not change the encryption capability: keep
		// its owner so an already admitted response can still complete
		// normally.
		enrollment = previousEnrollment
	} else {
		// Replacement releases the old owner's reservations in the same
		// transaction. An opaque ID also prevents ABA after
		// delete/recreate.
		if _, err := db.Exec("UPDATE push_entry_deliveries SET accepted=0,flight_session=NULL,retry_at=0,failures=0,error=NULL WHERE endpoint=?1 AND accepted IN (0,3)",
			subscription.Endpoint); err != nil {
			return err
		}
		if enrollment, err = core.NewSessionID(); err != nil {
			return fmt.Errorf("generate Web Push enrollment identifier: %w", err)
		}
	}
	_, err = db.Exec("INSERT INTO push_devices(endpoint,subscription,enrollment) VALUES(?1,?2,?3) ON CONFLICT(endpoint) DO UPDATE SET subscription=excluded.subscription,enrollment=excluded.enrollment",
		subscription.Endpoint, string(encoded), enrollment)
	// Routine device reconciliation must preserve retry deadlines and
	// errors.
	return err
}

func PushRenew(oldEndpoint string, subscription Subscription) error {
	_, err := submitDatabaseWrite("renew Web Push subscription", func(db *Connection) (struct{}, error) {
		return struct{}{}, pushRenewWith(db, oldEndpoint, subscription)
	})
	return err
}

func pushRenewWith(db *Connection, oldEndpoint string, subscription Subscription) error {
	if oldEndpoint == subscription.Endpoint {
		return pushSubscribeWith(db, subscription)
	}
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := pushSubscribeIn(tx, subscription); err != nil {
		return err
	}
	// Preserve known receipts across native endpoint rotation. Old
	// in-flight attempts become uncertain queued work under the new
	// encryption owner.
	if _, err := tx.Exec("UPDATE push_devices SET retry_at=max(retry_at,coalesce((SELECT retry_at FROM push_devices WHERE endpoint=?1),0)) WHERE endpoint=?2",
		oldEndpoint, subscription.Endpoint); err != nil {
		return err
	}
	if _, err := tx.Exec(`INSERT INTO push_entry_deliveries(endpoint,identity,accepted,retry_at,failures,error)
        SELECT ?2,q.identity,CASE WHEN q.accepted=3 THEN 0 ELSE q.accepted END,q.retry_at,q.failures,q.error
        FROM push_entry_deliveries q WHERE q.endpoint=?1
        ON CONFLICT(endpoint,identity) DO UPDATE SET accepted=CASE WHEN push_entry_deliveries.accepted=3 THEN 3 WHEN push_entry_deliveries.accepted=1 OR excluded.accepted=1 THEN 1 ELSE push_entry_deliveries.accepted END`,
		oldEndpoint, subscription.Endpoint); err != nil {
		return err
	}
	if _, err := tx.Exec("DELETE FROM push_devices WHERE endpoint=?1", oldEndpoint); err != nil {
		return err
	}
	return tx.Commit()
}

func PushUnsubscribe(endpoint string) error {
	_, err := submitDatabaseWrite("unsubscribe Web Push device", func(db *Connection) (struct{}, error) {
		return struct{}{}, pushUnsubscribeWith(db, endpoint)
	})
	return err
}

func pushUnsubscribeWith(db Queryer, endpoint string) error {
	_, err := db.Exec("DELETE FROM push_devices WHERE endpoint=?1", endpoint)
	return err
}

// AlertEventsSince reads a consistent durable frontier and only the events
// this client needs. No retention cap: a cursor may resume after any length
// of disconnection.
func AlertEventsSince(after *uint64) (uint64, []client.AlertEvent, error) {
	db, err := openReader(DatabasePath())
	if err != nil {
		return 0, nil, err
	}
	defer db.Close()
	return alertEventsSinceWith(db.Connection, after)
}

func alertEventsSinceWith(db *Connection, after *uint64) (uint64, []client.AlertEvent, error) {
	tx, err := db.Begin()
	if err != nil {
		return 0, nil, err
	}
	defer tx.Rollback()
	var high uint64
	if err := tx.QueryRow("SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name='alert_events'),0)").Scan(&high); err != nil {
		return 0, nil, err
	}
	lower := high
	if after != nil {
		lower = *after
	}
	events := []client.AlertEvent{}
	err = func() error {
		rows, err := tx.Query("SELECT sequence,destination,session_title,text,kind,identity FROM alert_events WHERE sequence>?1 AND sequence<=?2 ORDER BY sequence",
			lower, high)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var event client.AlertEvent
			var kind string
			if err := rows.Scan(&event.Sequence, &event.Destination, &event.SessionTitle, &event.Text, &kind, &event.Identity); err != nil {
				return err
			}
			if err := json.Unmarshal([]byte(kind), &event.Kind); err != nil {
				return err
			}
			events = append(events, event)
		}
		return rows.Err()
	}()
	if err != nil {
		return 0, nil, err
	}
	if err := tx.Commit(); err != nil {
		return 0, nil, err
	}
	return high, events, nil
}

// NotificationReconcile is what one notification reconciliation changed.
// The daemon notification service is the sole observer of published items.
type NotificationReconcile struct {
	AlertsChanged bool
	Complete      bool
}

// ReconcileNotifications is Rust's notification_reconcile, renamed because
// the result type takes the name. Nil when a drain refused the observation.
func ReconcileNotifications(runtime *client.RuntimeProjection) (*NotificationReconcile, error) {
	// The blocking owner holds admission from queueing through commit. A
	// drain refuses this disposable observation; the next daemon rebuilds
	// it.
	work, err := core.UpgradeActivityUnlessDraining("notification reconciliation")
	if err != nil {
		return nil, nil
	}
	defer work.Release()
	return submitDatabaseWrite("observe notification entries", func(db *Connection) (*NotificationReconcile, error) {
		before, _, err := alertEventsSinceWith(db, nil)
		if err != nil {
			return nil, err
		}
		tx, err := db.Begin()
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		complete := true
		if entries, ok := CardEntries(runtime); ok {
			if complete, err = pushObserveIn(tx, runtime, entries, time.Now().Unix()); err != nil {
				return nil, err
			}
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		after, _, err := alertEventsSinceWith(db, nil)
		if err != nil {
			return nil, err
		}
		return &NotificationReconcile{AlertsChanged: after != before, Complete: complete}, nil
	})
}

// PushReady claims the due deliveries, and the next deadline if any.
func PushReady(runtime *client.RuntimeProjection) ([]PushDelivery, *int64, error) {
	type ready struct {
		due  []PushDelivery
		next *int64
	}
	result, err := submitDatabaseWrite("claim Web Push deliveries", func(db *Connection) (ready, error) {
		due, next, err := pushReconcileWith(db, runtime, time.Now().Unix(), false)
		return ready{due, next}, err
	})
	return result.due, result.next, err
}

// PushReconcile reconciles the authoritative card snapshot and admits its
// sends in one transaction. Presence controls delivery and never changes
// entry identity. It also reports whether the alert events changed.
func PushReconcile(runtime *client.RuntimeProjection) ([]PushDelivery, *int64, bool, error) {
	type reconciled struct {
		due     []PushDelivery
		next    *int64
		changed bool
	}
	result, err := submitDatabaseWrite("reconcile Web Push entries", func(db *Connection) (reconciled, error) {
		before, _, err := alertEventsSinceWith(db, nil)
		if err != nil {
			return reconciled{}, err
		}
		due, next, err := pushReconcileWith(db, runtime, time.Now().Unix(), true)
		if err != nil {
			return reconciled{}, err
		}
		after, _, err := alertEventsSinceWith(db, nil)
		if err != nil {
			return reconciled{}, err
		}
		return reconciled{due, next, after != before}, nil
	})
	return result.due, result.next, result.changed, err
}

func pushReconcileWith(db *Connection, runtime *client.RuntimeProjection, now int64, observe bool) ([]PushDelivery, *int64, error) {
	entries, present := CardEntries(runtime)
	automatic := runtime.Metadata.OperatorPresence.Away && present
	tx, err := db.Begin()
	if err != nil {
		return nil, nil, err
	}
	defer tx.Rollback()
	if present && observe {
		if _, err := pushObserveIn(tx, runtime, entries, now); err != nil {
			return nil, nil, err
		}
	}
	if present && entries == nil {
		entries = map[string]PushEntry{}
	}
	due, err := pushClaimIn(tx, runtime, entries, automatic, now)
	if err != nil {
		return nil, nil, err
	}
	// Ready entries absent from this snapshot await the observer's wake,
	// rather than spinning on an already-expired delivery deadline.
	var next *int64
	if err := tx.QueryRow(readyPush+" SELECT min(due) FROM ranked WHERE rank=1 AND due>?2", automatic, now).Scan(&next); err != nil {
		return nil, nil, err
	}
	if err := tx.Commit(); err != nil {
		return nil, nil, err
	}
	return due, next, nil
}

// saturatingMul is i64::saturating_mul.
func saturatingMul(a, b int64) int64 {
	if a == 0 || b == 0 {
		return 0
	}
	product := a * b
	if product/b != a || (a == -1 && b == math.MinInt64) || (b == -1 && a == math.MinInt64) {
		if (a < 0) != (b < 0) {
			return math.MinInt64
		}
		return math.MaxInt64
	}
	return product
}

func pushObserveIn(tx *sql.Tx, runtime *client.RuntimeProjection, entries map[string]PushEntry, now int64) (bool, error) {
	complete := true
	// The cutover baseline applies even to devices enrolled later. An absent
	// card map cannot consume it, and a locator still in flight has no
	// identity.
	result, err := tx.Exec("INSERT OR IGNORE INTO push_entry_cutover VALUES(1)")
	if err != nil {
		return false, err
	}
	inserted, err := result.RowsAffected()
	if err != nil {
		return false, err
	}
	baseline := inserted == 1
	visible, err := committedStrings(tx, "SELECT identity FROM push_entries WHERE visible=1 AND session IS NOT NULL")
	if err != nil {
		return false, err
	}
	for _, id := range visible {
		if _, shown := entries[id]; !shown {
			if _, err := tx.Exec("UPDATE push_entries SET visible=0 WHERE identity=?1", id); err != nil {
				return false, err
			}
		}
	}
	identities := slices.Sorted(maps.Keys(entries))
	for _, identity := range identities {
		entry := entries[identity]
		if _, err := tx.Exec(`INSERT INTO push_entries(identity,session,completed_ordinal,visible,baseline,first_seen_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(identity) DO UPDATE SET session=excluded.session,
             completed_ordinal=excluded.completed_ordinal,visible=excluded.visible
         WHERE session IS NOT excluded.session OR completed_ordinal IS NOT excluded.completed_ordinal
             OR visible IS NOT excluded.visible`,
			identity, entry.Session, entry.CompletedOrdinal, !entry.Paused, baseline, saturatingMul(now, 1_000)); err != nil {
			return false, err
		}
	}
	// The same identity reservation owns both terminal alerts and push
	// delivery. Presence and device enrollment do not participate in this
	// decision.
	for _, identity := range identities {
		entry := entries[identity]
		if entry.Paused {
			continue
		}
		var eligible bool
		if err := tx.QueryRow("SELECT baseline=0 AND NOT EXISTS(SELECT 1 FROM alert_baseline WHERE identity=?1) AND NOT EXISTS(SELECT 1 FROM alert_events WHERE identity=?1) FROM push_entries WHERE identity=?1",
			identity).Scan(&eligible); err != nil {
			return false, err
		}
		if !eligible {
			continue
		}
		message, err := pushEntryText(tx, runtime, &entry)
		if err != nil {
			slog.Warn("could not read notification text; leaving item pending",
				"session", entry.Session, "identity", identity, "error", err.Error())
			complete = false
			continue
		}
		notice := entry.Notice(runtime, identity, message)
		kind, err := databaseMarshalJSON(entry.Kind)
		if err != nil {
			return false, err
		}
		if _, err := tx.Exec("INSERT INTO alert_events(identity,destination,session_title,text,kind) VALUES(?1,?2,?3,?4,?5)",
			identity, entry.Holder, notice.Title, notice.Body, string(kind)); err != nil {
			return false, err
		}
	}
	return complete, nil
}

func PushTest(endpoint string) error {
	_, err := submitDatabaseWrite("reserve Web Push test", func(db *Connection) (struct{}, error) {
		return struct{}{}, pushTestWith(db, endpoint)
	})
	return err
}

func pushTestWith(db *Connection, endpoint string) error {
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	var subscribed bool
	if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM push_devices WHERE endpoint=?1)", endpoint).Scan(&subscribed); err != nil {
		return err
	}
	if !subscribed {
		return errors.New("this device is not subscribed")
	}
	now := time.Now()
	identity := fmt.Sprintf("test:%d", now.UnixNano())
	if _, err := tx.Exec("INSERT OR IGNORE INTO push_entries(identity,visible,baseline,first_seen_at_ms) VALUES(?1,1,0,?2)",
		identity, now.UnixMilli()); err != nil {
		return err
	}
	if _, err := tx.Exec("INSERT OR IGNORE INTO push_entry_deliveries(endpoint,identity) VALUES(?1,?2)", endpoint, identity); err != nil {
		return err
	}
	return tx.Commit()
}

type PushDelivery struct {
	Subscription Subscription
	Enrollment   string
	Identity     string
	Payload      string
	Failures     uint32
}

// readyPush ranks before checking deadlines so an older retry holds its
// device/tag while other tags can proceed. A permanent rejection is retired
// until re-enrollment. The flight keeps the tag it sent even if a live form
// moves to another card.
const readyPush = `WITH ranked AS (
 SELECT d.subscription,d.enrollment,q.identity,q.failures,e.session,
        max(q.retry_at,d.retry_at) AS due,
        row_number() OVER (
            PARTITION BY q.endpoint,e.session
            ORDER BY e.completed_ordinal IS NULL,e.completed_ordinal,e.identity
        ) AS rank
 FROM push_entry_deliveries q JOIN push_devices d USING(endpoint) JOIN push_entries e USING(identity)
 WHERE q.accepted=0 AND q.retry_at<9223372036854775807
   AND e.visible=1 AND (e.session IS NULL OR ?1)
   AND NOT EXISTS(
       SELECT 1 FROM push_entry_deliveries flight
       WHERE flight.endpoint=q.endpoint AND flight.accepted=3
         AND flight.flight_session IS e.session
   )
)`

// pushClaimIn admits the due sends. A nil entries is a snapshot without a
// card map.
func pushClaimIn(tx *sql.Tx, runtime *client.RuntimeProjection, entries map[string]PushEntry, automatic bool, now int64) ([]PushDelivery, error) {
	if automatic {
		if _, err := tx.Exec(`INSERT OR IGNORE INTO push_entry_deliveries(endpoint,identity)
             SELECT d.endpoint,e.identity FROM push_entries e CROSS JOIN push_devices d
             WHERE e.visible=1 AND e.session IS NOT NULL AND e.baseline=0`); err != nil {
			return nil, err
		}
	}
	type dueRow struct {
		subscription, enrollment, identity string
		failures                           uint32
		session                            *string
	}
	var due []dueRow
	err := func() error {
		rows, err := tx.Query(readyPush+` SELECT subscription,enrollment,identity,failures,session
         FROM ranked WHERE rank=1 AND due<=?2`, automatic, now)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var row dueRow
			if err := rows.Scan(&row.subscription, &row.enrollment, &row.identity, &row.failures, &row.session); err != nil {
				return err
			}
			due = append(due, row)
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, err
	}
	admitted := []PushDelivery{}
	notices := map[string]string{}
	for _, row := range due {
		var subscription Subscription
		if err := json.Unmarshal([]byte(row.subscription), &subscription); err != nil {
			return nil, err
		}
		var payload string
		if row.session != nil {
			entry, ok := entries[row.identity]
			if entries == nil || !ok {
				// The delivery snapshot can advance before the notification
				// owner hides its ledger row. Its reconciliation wakes us.
				continue
			}
			if entry.Paused {
				// Availability can change before the observer hides the
				// ledger row. The consumer must not admit that stale
				// decision.
				continue
			}
			if cached, ok := notices[row.identity]; ok {
				payload = cached
			} else {
				message, err := pushEntryText(tx, runtime, &entry)
				if err != nil {
					return nil, err
				}
				encoded, err := databaseMarshalJSON(entry.Notice(runtime, row.identity, message))
				if err != nil {
					return nil, err
				}
				payload = string(encoded)
				notices[row.identity] = payload
			}
		} else {
			encoded, err := databaseMarshalJSON(Notice{
				Title:            "Mjolnir",
				Body:             "Notifications are enabled on this device",
				SessionID:        nil,
				URL:              "/",
				Tag:              "test",
				Episode:          nil,
				ObservedRevision: 0,
			})
			if err != nil {
				return nil, err
			}
			payload = string(encoded)
		}
		if _, err := tx.Exec("UPDATE push_entry_deliveries SET accepted=3,flight_session=?3 WHERE endpoint=?1 AND identity=?2",
			subscription.Endpoint, row.identity, row.session); err != nil {
			return nil, err
		}
		admitted = append(admitted, PushDelivery{
			Subscription: subscription,
			Enrollment:   row.enrollment,
			Identity:     row.identity,
			Payload:      payload,
			Failures:     row.failures,
		})
	}
	return admitted, nil
}

// pushTranscriptItems is the items of a transcript tail in tail order.
func pushTranscriptItems(tail *client.SessionTail) []core.TranscriptItem {
	if tail == nil {
		return nil
	}
	items := make([]core.TranscriptItem, 0, tail.Items.Len())
	for item := range tail.Items.Values() {
		if *item != nil {
			items = append(items, **item)
		}
	}
	return items
}

func pushEntryText(db Queryer, runtime *client.RuntimeProjection, entry *PushEntry) (*string, error) {
	if entry.Text != nil {
		text := *entry.Text
		return &text, nil
	}
	if entry.MessageID != nil {
		id := *entry.MessageID
		if text, ok := core.MaterializedAgentMessageText(pushTranscriptItems(runtime.Transcripts.GetShared(entry.Session)), id); ok {
			return &text, nil
		}
		stored, err := LoadMaterializedTranscriptItem(entry.Session, id)
		if err != nil {
			return nil, err
		}
		var items []core.TranscriptItem
		if stored != nil {
			items = []core.TranscriptItem{*stored}
		}
		if text, ok := core.MaterializedAgentMessageText(items, id); ok {
			return &text, nil
		}
		return nil, nil
	}
	// An entry without a message id reads its own turn only, as the cards
	// do.
	if entry.CompletedOrdinal == nil {
		return nil, nil
	}
	item, err := MaterializedAgentItemThrough(db, entry.Session, *entry.CompletedOrdinal)
	if err != nil || item == nil {
		return nil, err
	}
	if text, ok := core.MaterializedAgentMessageText([]core.TranscriptItem{*item}, item.StableID); ok {
		return &text, nil
	}
	return nil, nil
}

func PushRecover() error {
	_, err := submitDatabaseWrite("recover uncertain Web Push sends", func(db *Connection) (struct{}, error) {
		return struct{}{}, pushRecoverWith(db)
	})
	return err
}

func pushRecoverWith(db Queryer) error {
	// Only service startup, under the daemon's store ownership, recovers
	// flight reservations. Remote acceptance may be unknown; receipts never
	// reset.
	_, err := db.Exec("UPDATE push_entry_deliveries SET accepted=0,flight_session=NULL WHERE accepted=3")
	return err
}

// PushResult records a send's receipt. status is the push service's HTTP
// status, nil when the request failed before a response.
func PushResult(subscription Subscription, enrollment, identity string, status *uint16, retryAt int64, failure *string) error {
	_, err := submitDatabaseWrite("record Web Push receipt", func(db *Connection) (struct{}, error) {
		return struct{}{}, applyPushResultWith(db, subscription, enrollment, identity, status, retryAt, failure)
	})
	return err
}

func applyPushResultWith(db *Connection, subscription Subscription, enrollment, identity string, status *uint16, retryAt int64, failure *string) error {
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	current, err := environmentOptionalString(tx, "SELECT enrollment FROM push_devices WHERE endpoint=?1", subscription.Endpoint)
	if err != nil {
		return err
	}
	if current == nil || *current != enrollment {
		return nil
	}
	if status != nil && (*status == 404 || *status == 410) {
		// A response from an older enrollment cannot delete its
		// replacement.
		if _, err := tx.Exec("DELETE FROM push_devices WHERE endpoint=?1 AND enrollment=?2", subscription.Endpoint, enrollment); err != nil {
			return err
		}
		return tx.Commit()
	}
	var ownsFlight bool
	if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM push_entry_deliveries WHERE endpoint=?1 AND identity=?2 AND accepted=3)",
		subscription.Endpoint, identity).Scan(&ownsFlight); err != nil {
		return err
	}
	if !ownsFlight {
		return nil
	}
	// A permanent payload rejection retires only that intent. Endpoint-wide
	// backpressure is reserved for transient service/transport failures.
	if status == nil || *status == 429 || *status >= 500 {
		// Retry-After constrains follow-up requests to this endpoint,
		// including newly queued entries, not just the rejected intent.
		if _, err := tx.Exec("UPDATE push_devices SET retry_at=max(retry_at,?2) WHERE endpoint=?1", subscription.Endpoint, retryAt); err != nil {
			return err
		}
	}
	accepted := status != nil && *status >= 200 && *status < 300
	if err := pushResultWith(tx, subscription.Endpoint, identity, accepted, retryAt, failure); err != nil {
		return err
	}
	return tx.Commit()
}

func pushResultWith(db Queryer, endpoint, identity string, accepted bool, retryAt int64, failure *string) error {
	_, err := db.Exec("UPDATE push_entry_deliveries SET accepted=?3,flight_session=NULL,retry_at=?4,failures=failures+1,error=?5 WHERE endpoint=?1 AND identity=?2 AND accepted IN (0,3)",
		endpoint, identity, accepted, retryAt, failure)
	return err
}

// PushDeviceStatus is {"subscribed": bool, "error": string or null} for one
// device, members in that order.
func PushDeviceStatus(endpoint string) (json.RawMessage, error) {
	db, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer db.Close()
	var subscribed bool
	if err := db.QueryRow("SELECT EXISTS(SELECT 1 FROM push_devices WHERE endpoint=?1)", endpoint).Scan(&subscribed); err != nil {
		return nil, err
	}
	failure, err := environmentOptionalString(db, "SELECT error FROM push_entry_deliveries WHERE endpoint=?1 AND accepted IN (0,3) AND error IS NOT NULL ORDER BY retry_at DESC LIMIT 1", endpoint)
	if err != nil {
		return nil, err
	}
	return databaseMarshalJSON(struct {
		Subscribed bool    `json:"subscribed"`
		Error      *string `json:"error"`
	}{subscribed, failure})
}
