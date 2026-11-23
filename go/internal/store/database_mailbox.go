// Port of mj-store/src/database/mailbox.rs.

// Durable mailbox event outbox. Delivery is owned by the controller service.

package store

import (
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
	panic("unported: mj-store/src/database/mailbox.rs mailbox_delivery_state")
}

func mailboxDeliveryStateWith(conn Queryer, sessionID, eventKey string) (MailboxDeliveryState, error) {
	panic("unported: mj-store/src/database/mailbox.rs mailbox_delivery_state_with")
}

// clearMailboxWith records a /clear receipt. The receipt survives projection
// rebuilds. Cleanup and its completion frontier commit together, so
// historical replay cannot discard newer mail.
func clearMailboxWith(conn Queryer, sessionID string, ordinal uint64) error {
	panic("unported: mj-store/src/database/mailbox.rs clear_mailbox_with")
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
	panic("unported: mj-store/src/database/mailbox.rs enqueue_mailbox_event")
}

func EnqueueMailboxEventWith(conn Queryer, eventKey, targetSessionID, eventJSON string, wake, unpark bool) (Enqueued, error) {
	panic("unported: mj-store/src/database/mailbox.rs enqueue_mailbox_event_with")
}

// PendingMailboxEvents selects a bounded batch for each target so blocked
// targets cannot starve others.
func PendingMailboxEvents(limit int) ([]MailboxOutboxEntry, error) {
	panic("unported: mj-store/src/database/mailbox.rs pending_mailbox_events")
}

func MailboxEventExists(eventKey string) (bool, error) {
	panic("unported: mj-store/src/database/mailbox.rs mailbox_event_exists")
}

// PendingSubagentFinishedChildren is the child session IDs that have an
// undelivered SubagentFinished mailbox row pending for parentSessionID, as a
// set.
func PendingSubagentFinishedChildren(parentSessionID string) (map[string]struct{}, error) {
	panic("unported: mj-store/src/database/mailbox.rs pending_subagent_finished_children")
}

// SubagentMailboxMessages is the recent parent messages addressed to this
// session's children, including whether the child's relay has accepted each
// waking event.
func SubagentMailboxMessages(parentSessionID string) ([]SubagentMailboxMessage, error) {
	panic("unported: mj-store/src/database/mailbox.rs subagent_mailbox_messages")
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
	panic("unported: mj-store/src/database/mailbox.rs settle_failed_parent_messages_with")
}

// MarkMailboxEventFailed permanently stops retrying a mailbox event that the
// published worker protocol cannot represent. accepted_at also makes
// protocol-75 daemons skip the row; newer readers distinguish this marker
// through failure.
func MarkMailboxEventFailed(eventKey, failure string) error {
	panic("unported: mj-store/src/database/mailbox.rs mark_mailbox_event_failed")
}

func MarkMailboxEventAccepted(eventKey, commandID string, ordinal uint64) error {
	panic("unported: mj-store/src/database/mailbox.rs mark_mailbox_event_accepted")
}

func markMailboxEventAcceptedWith(conn Queryer, eventKey, commandID string, ordinal uint64) error {
	panic("unported: mj-store/src/database/mailbox.rs mark_mailbox_event_accepted_with")
}

// PrunePendingMessagesForStoppedChildren removes only unaccepted
// parent-message rows for stopped children, which cannot receive parent
// messages and cannot be resumed by send_message; it keeps accepted delivery
// history and events addressed to stopped primary sessions.
func PrunePendingMessagesForStoppedChildren() (int, error) {
	panic("unported: mj-store/src/database/mailbox.rs prune_pending_messages_for_stopped_children")
}

// PruneStaleSubagentQuestions removes forwarded questions whose asking child
// can no longer receive a send_message answer; the pending invitation can
// only fail. Accepted history stays.
func PruneStaleSubagentQuestions() (int, error) {
	panic("unported: mj-store/src/database/mailbox.rs prune_stale_subagent_questions")
}

func pruneStaleSubagentQuestionsWith(conn Queryer) (int, error) {
	panic("unported: mj-store/src/database/mailbox.rs prune_stale_subagent_questions_with")
}
