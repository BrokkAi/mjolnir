// Port of mj-store/src/database/review_delivery.rs.

package store

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PartialReviewEvents is the partial-result mailbox events for the reviewers
// of current that settled with findings since previous. Called inside the
// outcome save transaction: durable outcomes own whether a report is new,
// and the outbox commits with them. No delivery flags to recover.
func PartialReviewEvents(owner string, previous *core.ActiveReviewGroup, current *core.ActiveReviewGroup) []core.MailboxEvent {
	panic("unported: mj-store/src/database/review_delivery.rs partial_review_events")
}
