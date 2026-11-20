// Port of mj-worker/src/relay/verdict.rs (its #[cfg(test)] items).

package worker

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (r *DurableRelay) storeQuestionTarget(revision uint64, stableID string) (bool, error) {
	completed, ok := r.questionLocatorCompletion(revision)
	if !ok {
		return false, nil
	}
	return r.finishQuestionLocator(completed, &stableID)
}

func (r *DurableRelay) resolveRetryAssessment(identity *TurnAssessmentIdentity, retryable bool) (bool, error) {
	if identity == nil {
		return false, nil
	}
	current := r.snapshot.Assessment
	if current == nil || current.TurnID != identity.TurnID ||
		current.CompletedOrdinal != identity.CompletedOrdinal || !current.Current() {
		return false, nil
	}
	a := *current
	a.Status = core.StatusAssessed
	action := core.AssessmentActionUncertain
	if retryable {
		a.Status = core.StatusScheduled
		action = core.AssessmentActionRetryProvider
	}
	a.Action = &action
	if err := r.storeAssessment(a); err != nil {
		return false, err
	}
	return retryable, nil
}

func (r *DurableRelay) retryAssessmentIdentity() *TurnAssessmentIdentity {
	a := r.snapshot.Assessment
	if a == nil || !a.Current() {
		return nil
	}
	return &TurnAssessmentIdentity{TurnID: a.TurnID, CompletedOrdinal: a.CompletedOrdinal}
}

// ExpectContinuation is a test helper: apply an expected-continuation decision
// directly.
func (r *DurableRelay) ExpectContinuation(sinceMs int64, _ string, generation uint64) error {
	_, err := r.applyRepliedDecision(generation, core.DecisionExpectContinuation, sinceMs)
	return err
}
