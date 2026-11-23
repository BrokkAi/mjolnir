// Port of DurableRelay::prepare_handback_check in mj-worker/src/relay/verdict.rs.
//
// The declarations phase left this function out of relay_verdict.go. The
// relay-core group ported it here, apart from that file, because its
// handback tests (mj-worker/src/relay/handback_tests.rs) call it. If
// relay_verdict.go also gains it, keep one of the two.

package worker

import (
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// PreparedHandbackCheck is the classifier request a sub-agent handback is
// checked with: the session, the journal frontier it was prepared at, and
// the evidence (Rust's (String, u64, TurnEvidence)).
type PreparedHandbackCheck struct {
	SessionID  string
	Generation uint64
	Evidence   core.TurnEvidence
}

// prepareHandbackCheck: the delivery events own assignment identity.
// Reminders and provider retries do not replace it, so all their handbacks
// consume the original assignment. False when the report is not to be
// checked.
func (r *DurableRelay) prepareHandbackCheck(report string) (PreparedHandbackCheck, bool, error) {
	if core.DisabledByEnvironment() {
		return PreparedHandbackCheck{}, false, nil
	}
	if r.verdictHarness == nil {
		return PreparedHandbackCheck{}, false, nil
	}
	harness := *r.verdictHarness
	checks := &r.snapshot.HandbackChecks
	if checks.AssignmentID == nil {
		return PreparedHandbackCheck{}, false, nil
	}
	assignmentID := *checks.AssignmentID
	if relaySortedSetContains(checks.Checked, assignmentID) {
		return PreparedHandbackCheck{}, false, nil
	}
	if checks.Context == nil {
		return PreparedHandbackCheck{}, false, nil
	}
	context := *checks.Context
	context.Messages = slices.Clone(context.Messages)
	if context.SetAsideThrough != nil {
		through := *context.SetAsideThrough
		context.SetAsideThrough = &through
	}
	context.OpenAssistantID = nil
	id := "handback-report"
	context.Assistant(&id, r.snapshot.LatestOrdinal, report)
	facts := r.ActivityFacts()
	evidence := r.turnContext.Evidence(harness, core.TurnPhaseReplied, &facts, core.EpochMillis())
	// The report goes whole into the authorization history above; the tail
	// field keeps the bound every classifier route enforces.
	evidence.AssistantTextTail = transcript.Tail(report, core.AssistantTextBytes)
	evidence.FinalToolCalls = nil
	evidence.ToolsInFlight = nil
	completion := core.CompletionEvidenceBounded("end_turn", nil)
	evidence.Completion = &completion
	// The whole history goes in; only GLiDE's own size refusal shrinks it,
	// and the client refuses to shrink away the parent's instructions.
	// Authorization is historical context. Only the turn owner supplies the
	// current reply; an older question must not become this turn's answer.
	evidence.Authorization = &context
	evidence.TranscriptSummary = ""
	// GLiDE judges the report against the parent's instructions. When they
	// did not arrive whole (a non-text block), there is nothing to judge
	// against: deliver unchecked and leave the check unspent.
	if !context.AuthorizationComplete {
		return PreparedHandbackCheck{}, false, nil
	}
	// Journal before HTTP: a crash or cancelled socket consumes the attempt.
	if _, err := r.RecordObservation(core.RelayObservationHandbackCheckStarted{AssignmentID: assignmentID}); err != nil {
		return PreparedHandbackCheck{}, false, err
	}
	return PreparedHandbackCheck{
		SessionID:  r.snapshot.SessionID,
		Generation: r.snapshot.LatestOrdinal,
		Evidence:   evidence,
	}, true, nil
}
