// Port of mj-store/src/database/web_push.rs.

// Durable Web Push reservations and receipts, owned by the dispatch service.

package store

import (
	"database/sql"
	"encoding/json"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

func PushSubscribe(subscription Subscription) error {
	panic("unported: mj-store/src/database/web_push.rs push_subscribe")
}

func pushSubscribeWith(db *Connection, subscription Subscription) error {
	panic("unported: mj-store/src/database/web_push.rs push_subscribe_with")
}

func pushSubscribeIn(db Queryer, subscription Subscription) error {
	panic("unported: mj-store/src/database/web_push.rs push_subscribe_in")
}

func PushRenew(oldEndpoint string, subscription Subscription) error {
	panic("unported: mj-store/src/database/web_push.rs push_renew")
}

func pushRenewWith(db *Connection, oldEndpoint string, subscription Subscription) error {
	panic("unported: mj-store/src/database/web_push.rs push_renew_with")
}

func PushUnsubscribe(endpoint string) error {
	panic("unported: mj-store/src/database/web_push.rs push_unsubscribe")
}

func pushUnsubscribeWith(db Queryer, endpoint string) error {
	_, err := db.Exec("DELETE FROM push_devices WHERE endpoint=?1", endpoint)
	return err
}

// AlertEventsSince reads a consistent durable frontier and only the events
// this client needs. No retention cap: a cursor may resume after any length
// of disconnection.
func AlertEventsSince(after *uint64) (uint64, []client.AlertEvent, error) {
	panic("unported: mj-store/src/database/web_push.rs alert_events_since")
}

func alertEventsSinceWith(db *Connection, after *uint64) (uint64, []client.AlertEvent, error) {
	panic("unported: mj-store/src/database/web_push.rs alert_events_since_with")
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
	panic("unported: mj-store/src/database/web_push.rs notification_reconcile")
}

// PushReady claims the due deliveries, and the next deadline if any.
func PushReady(runtime *client.RuntimeProjection) ([]PushDelivery, *int64, error) {
	panic("unported: mj-store/src/database/web_push.rs push_ready")
}

// PushReconcile reconciles the authoritative card snapshot and admits its
// sends in one transaction. Presence controls delivery and never changes
// entry identity. It also reports whether the alert events changed.
func PushReconcile(runtime *client.RuntimeProjection) ([]PushDelivery, *int64, bool, error) {
	panic("unported: mj-store/src/database/web_push.rs push_reconcile")
}

func pushReconcileWith(db *Connection, runtime *client.RuntimeProjection, now int64, observe bool) ([]PushDelivery, *int64, error) {
	panic("unported: mj-store/src/database/web_push.rs push_reconcile_with")
}

func pushObserveIn(tx *sql.Tx, runtime *client.RuntimeProjection, entries map[string]PushEntry, now int64) (bool, error) {
	panic("unported: mj-store/src/database/web_push.rs push_observe_in")
}

func PushTest(endpoint string) error {
	panic("unported: mj-store/src/database/web_push.rs push_test")
}

func pushTestWith(db *Connection, endpoint string) error {
	panic("unported: mj-store/src/database/web_push.rs push_test_with")
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
	panic("unported: mj-store/src/database/web_push.rs push_claim_in")
}

func pushEntryText(db Queryer, runtime *client.RuntimeProjection, entry *PushEntry) (*string, error) {
	panic("unported: mj-store/src/database/web_push.rs push_entry_text")
}

func PushRecover() error {
	panic("unported: mj-store/src/database/web_push.rs push_recover")
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
	panic("unported: mj-store/src/database/web_push.rs push_result")
}

func applyPushResultWith(db *Connection, subscription Subscription, enrollment, identity string, status *uint16, retryAt int64, failure *string) error {
	panic("unported: mj-store/src/database/web_push.rs apply_push_result_with")
}

func pushResultWith(db Queryer, endpoint, identity string, accepted bool, retryAt int64, failure *string) error {
	_, err := db.Exec("UPDATE push_entry_deliveries SET accepted=?3,flight_session=NULL,retry_at=?4,failures=failures+1,error=?5 WHERE endpoint=?1 AND identity=?2 AND accepted IN (0,3)",
		endpoint, identity, accepted, retryAt, failure)
	return err
}

// PushDeviceStatus is {"subscribed": bool, "error": string or null} for one
// device, members in that order.
func PushDeviceStatus(endpoint string) (json.RawMessage, error) {
	panic("unported: mj-store/src/database/web_push.rs push_device_status")
}
