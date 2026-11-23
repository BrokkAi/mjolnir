// Port of mj-store/src/database/review_delivery.rs.

package store

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PartialReviewEvents is the partial-result mailbox events for the reviewers
// of current that settled with findings since previous. Called inside the
// outcome save transaction: durable outcomes own whether a report is new,
// and the outbox commits with them. No delivery flags to recover.
func PartialReviewEvents(owner string, previous *core.ActiveReviewGroup, current *core.ActiveReviewGroup) []core.MailboxEvent {
	snapshot := &current.Driver
	if _, running := snapshot.Phase.(core.TurnReviewPhaseRunning); !running {
		return []core.MailboxEvent{}
	}
	total := len(snapshot.Roles)
	completed := 0
	for _, role := range snapshot.Roles {
		if role.Outcome != nil {
			completed++
		}
	}
	if previous != nil && previous.Driver.ReviewID != snapshot.ReviewID {
		previous = nil
	}
	events := []core.MailboxEvent{}
	for _, role := range snapshot.Roles {
		if previous != nil {
			settledBefore := false
			for _, prior := range previous.Driver.Roles {
				if prior.Role == role.Role && prior.Outcome != nil {
					settledBefore = true
					break
				}
			}
			if settledBefore {
				continue
			}
		}
		findings, ok := role.Outcome.(core.ReviewVerdictFindings)
		if !ok {
			continue
		}
		report := findings.Synthesis
		for _, delta := range snapshot.Deltas {
			head := ""
			if delta.HeadCommit != nil {
				head = *delta.HeadCommit
			}
			var commits string
			if delta.BaseCommit != nil && *delta.BaseCommit != "" {
				commits = fmt.Sprintf("%s..%s", *delta.BaseCommit, head)
			} else {
				commits = fmt.Sprintf("up to %s", head)
			}
			events = append(events, core.MailboxEvent{
				Key:         fmt.Sprintf("push-review-partial:%s:%s:%s:%s", owner, snapshot.ReviewID, role.Role, delta.Root),
				Source:      "review",
				Wake:        true,
				CreatedAtMs: uint64(max(core.EpochMillis(), 0)),
				Body: core.MailboxEventBodyPlainText{
					Text: fmt.Sprintf("Review %d/%d — partial result for commits %s in %s (review %s).\n%s\n%d of %d reviewers are still running. Independent reviewer reports may overlap. Start investigating and fixing findings you agree with now instead of idling until all reviewers finish. Wait for the final result before committing, pushing, calling override_review, or requesting another review.",
						completed, total, commits, delta.Root, snapshot.ReviewID, report, total-completed, total),
				},
			})
		}
	}
	return events
}
