// Port of mj-worker/src/relay/reviewer_admission.rs.
//
// Reviewer work participates in the primary relay's atomic replacement gate.
// Rust compiles this only on Unix, where the reviewer runtime lives; it has
// nothing platform-specific, so Go builds it everywhere.

package worker

import (
	"errors"
	"math"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ReviewerAdmission is a reviewer lease. The primary relay owns admission.
// Holding this lease, rather than sampling a role's status later, excludes
// worker replacement throughout preparation, execution, and cleanup. No
// lease survives its worker process.
type ReviewerAdmission struct {
	relay *RelayCell
	id    uint64
}

// reviewerAdmissionAcquire is ReviewerAdmission::acquire.
func reviewerAdmissionAcquire(relay *RelayCell, label string) (*ReviewerAdmission, error) {
	owner := relay.Lock()
	defer owner.Unlock()
	if owner.checkpointOnly ||
		owner.snapshot.Execution == core.RelayExecutionStateClosed ||
		owner.snapshot.CheckpointBarrier != nil ||
		owner.pendingCheckpointBarrier() {
		return nil, errors.New("worker is reserved for checkpoint or replacement; reviewer work was not admitted")
	}
	if owner.nextReviewerAdmission == math.MaxUint64 {
		return nil, errors.New("reviewer admission IDs exhausted")
	}
	id := owner.nextReviewerAdmission + 1
	owner.nextReviewerAdmission = id
	owner.reviewerAdmissions[id] = label
	return &ReviewerAdmission{relay: relay, id: id}, nil
}

// Release returns the lease (Rust's Drop).
func (a *ReviewerAdmission) Release() {
	owner := a.relay.Lock()
	defer owner.Unlock()
	delete(owner.reviewerAdmissions, a.id)
}
