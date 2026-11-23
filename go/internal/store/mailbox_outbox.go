// Port of mj-store/src/mailbox_outbox.rs.

// The mailbox outbox's wake signal, delivery outcomes and command ids. The
// daemon's delivery owner is daemon::mailbox_delivery.

package store

import (
	"crypto/sha256"
	"encoding/hex"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const trustedParentMessageProtocolError = "This queued parent message reached an older mj worker that cannot receive structured mailbox events; it was not delivered."
const sessionMessageProtocolError = "This queued session message reached an older mj worker that cannot receive its sender information; it was not delivered."

// outboxNotifyChannel is tokio's Notify for the outbox: one stored permit,
// taken by the next waiter.
var outboxNotifyChannel = make(chan struct{}, 1)

// OutboxNotify is the channel the delivery owner waits on; a receive takes
// the permit NotifyMailboxOutboxChanged stored.
func OutboxNotify() <-chan struct{} {
	return outboxNotifyChannel
}

// NotifyMailboxOutboxChanged wakes the delivery owner after a producer
// commits new outbox rows.
func NotifyMailboxOutboxChanged() {
	select {
	case outboxNotifyChannel <- struct{}{}:
	default:
	}
}

// MailboxEventDelivery is how a mailbox event's delivery ended, or what it
// still waits for.
type MailboxEventDelivery interface {
	isMailboxEventDelivery()
}

// MailboxEventDeliveryPending is not delivered yet; the text says what
// delivery waits for.
type MailboxEventDeliveryPending struct{ Value string }

type MailboxEventDeliveryAccepted struct{ Value uint64 }

type MailboxEventDeliveryDiscarded struct{}

type MailboxEventDeliveryFailed struct{ Value string }

func (MailboxEventDeliveryPending) isMailboxEventDelivery()   {}
func (MailboxEventDeliveryAccepted) isMailboxEventDelivery()  {}
func (MailboxEventDeliveryDiscarded) isMailboxEventDelivery() {}
func (MailboxEventDeliveryFailed) isMailboxEventDelivery()    {}

// PublishedWorkerRelayProtocol reads the worker protocol only from the
// protocol version published in the session manager's current view. Request
// admission and outbox delivery use this same fact.
func PublishedWorkerRelayProtocol(view *client.ManagedSessionView) *uint32 {
	if view.Snapshot == nil {
		return nil
	}
	return view.Snapshot.Operational.Get().RelayProtocolVersion
}

// TrustedParentMessageProtocolError is the refusal for a structured parent
// message to a worker older than the structured mailbox protocol; empty when
// the worker can take it or its protocol is unknown.
func TrustedParentMessageProtocolError(protocol *uint32) (string, bool) {
	if protocol != nil && *protocol < core.RelayStructuredMailboxProtocol {
		return trustedParentMessageProtocolError, true
	}
	return "", false
}

// SessionMessageProtocolError is the refusal for a session message to a
// worker older than the session message protocol.
func SessionMessageProtocolError(protocol *uint32) (string, bool) {
	if protocol != nil && *protocol < core.RelaySessionMessageProtocol {
		return sessionMessageProtocolError, true
	}
	return "", false
}

// MailboxCommandID is the relay command id that delivers the outbox event
// eventKey: "mailbox-" and the key's SHA-256 in lowercase hex.
func MailboxCommandID(eventKey string) string {
	digest := sha256.Sum256([]byte(eventKey))
	return "mailbox-" + hex.EncodeToString(digest[:])
}
