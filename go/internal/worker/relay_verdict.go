// Port of mj-worker/src/relay/verdict.rs.
//
// Durable completed-turn assessment and process-local activity inference.

package worker

import (
	"cmp"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// TurnAssessmentIdentity names the turn an assessment judges: its turn id and
// the ordinal its completion was recorded at. Rust carries it as the tuple
// `(String, u64)`.
type TurnAssessmentIdentity struct {
	TurnID           string
	CompletedOrdinal uint64
}

// PendingRepliedVerdict is a completed-turn check ready to send: the
// assessment revision it answers, the evidence captured for it, and the turn
// it judges. Rust: `(u64, TurnEvidence, Option<(String, u64)>)`.
type PendingRepliedVerdict struct {
	Generation uint64
	Evidence   core.TurnEvidence
	Turn       *TurnAssessmentIdentity
}

// repliedVerdictInference is an accepted Jev decision with the evidence
// generation it was made at and the instant it applies from.
type repliedVerdictInference struct {
	generation uint64
	decision   core.Decision
	sinceMs    int64
}

// repliedVerdictBackground is Jev's confident answer to whether the listed
// background commands are still needed, with the evidence generation it was
// judged at.
type repliedVerdictBackground struct {
	generation uint64
	needed     bool
}

// RepliedVerdictState is the process-local half of the completed-turn
// verdict. The zero value is ready (Rust's Default).
//
// Rust keeps inference and background behind their own mutexes because
// activity facts read them through a shared reference. Every access here goes
// through the DurableRelay, which its RelayCell lock already serializes, so
// the Go fields are plain.
type RepliedVerdictState struct {
	lastGeneration *uint64
	// inferred is Rust's `inference` field; Go cannot give a field and a
	// method the same name.
	inferred *repliedVerdictInference
	// background is Jev's confident answer to whether the listed background
	// commands are still needed, with the evidence generation it was judged
	// at.
	background *repliedVerdictBackground
	// askedTurn is the turn (turn_id, completed ordinal) this process last
	// asked Jev about. The background judgment dies with the process, so a
	// restarted worker asks once more about a turn it has not asked about
	// itself.
	askedTurn *TurnAssessmentIdentity
}

// blocked names the foreground fact that keeps a completed-turn decision from
// applying, if any.
func blocked(facts *core.ActivityFacts) (string, bool) {
	switch {
	case facts.Execution == core.RelayExecutionStateClosing || facts.Execution == core.RelayExecutionStateClosed:
		return "session_closed", true
	case facts.PromptStartedAtMs != nil || facts.HarnessTurnStartedAtMs != nil:
		return "foreground_turn", true
	case len(facts.ToolsInFlight) > 0 ||
		(facts.Execution == core.RelayExecutionStateRunning && facts.CurrentStepStartedAtMs != nil):
		return "foreground_tool", true
	case facts.GoalRunning:
		return "running_goal", true
	case facts.QueuedCommands > 0:
		return "queued_prompt", true
	}
	return "", false
}

// inference returns (expected_continuation, inferred_idle_since_ms,
// background_needed) for the current generation, or nothing once the
// evidence has moved on.
func (s *RepliedVerdictState) inference(generation uint64, facts *core.ActivityFacts) (*int64, *int64, *bool) {
	_, stale := blocked(facts)
	if (s.inferred != nil && s.inferred.generation != generation) || stale {
		s.inferred = nil
	}
	if (s.background != nil && s.background.generation != generation) || stale {
		s.background = nil
	}
	var needed *bool
	if s.background != nil {
		value := s.background.needed
		needed = &value
	}
	if s.inferred == nil {
		return nil, nil, needed
	}
	since := s.inferred.sinceMs
	switch s.inferred.decision {
	case core.DecisionInferIdle, core.DecisionAwaitingInput:
		return nil, &since, needed
	case core.DecisionExpectContinuation:
		return &since, nil, needed
	}
	return nil, nil, needed
}

func (s *RepliedVerdictState) judgeBackground(generation uint64, needed *bool) {
	if needed == nil {
		s.background = nil
		return
	}
	s.background = &repliedVerdictBackground{generation: generation, needed: *needed}
}

// glideAttention marks a non-question turn important when GLiDE's attention
// axis says so. Only applyGlideAssessment uses it.
func glideAttention(glide *core.Probabilities, action core.AssessmentAction) *core.AssessmentAttention {
	if action != core.AssessmentActionAwaitInput && glide != nil && glide.Attention != nil &&
		glide.Attention.Probabilities.Important >= core.ImportantAttention {
		important := core.AssessmentAttentionImportant
		return &important
	}
	return nil
}

func (r *DurableRelay) classifiesTurns() bool {
	return !r.snapshot.ManagedSubagent
}

// suppressChildAssessment persists suppression so replaying controllers and
// older assessment readers never wait for a child classification. Keep the
// shared event format.
func (r *DurableRelay) suppressChildAssessment() error {
	if !r.snapshot.ManagedSubagent {
		return nil
	}
	r.repliedVerdict = RepliedVerdictState{}
	if r.snapshot.Assessment == nil || !r.snapshot.Assessment.Current() {
		return nil
	}
	assessment := *r.snapshot.Assessment
	assessment.Status = core.StatusSuperseded
	assessment.Reason = "managed_subagent"
	assessment.RetryAtMs = nil
	return r.storeAssessment(assessment)
}

func (r *DurableRelay) publishAssessmentDiagnostic() {
	if r.snapshot.Assessment == nil {
		return
	}
	if log, ok := r.turnContext.DecisionLog(); ok {
		log.RecordAssessment(r.snapshot.SessionID, r.snapshot.Assessment)
	}
}

// preparePendingAssessment captures immutable semantic evidence before issuing
// a request. Completion itself already installed the pending record, so
// restart can finish this.
//
// The Unix worker is the only production caller; these assessment operations
// stay compiled on Windows so their tests still build there.
func (r *DurableRelay) preparePendingAssessment() error {
	panic("unported: mj-worker/src/relay/verdict.rs prepare_pending_assessment")
}

// requestBackgroundRefresh queues one more completed-turn check for the
// leftover processes. Waiting and uncertain assessments classify again; other
// completed actions keep their authority while the background judgment
// refreshes.
func (r *DurableRelay) requestBackgroundRefresh(a core.TurnAssessment) error {
	panic("unported: mj-worker/src/relay/verdict.rs request_background_refresh")
}

// reaskBackgroundAfterRestart restores a stored GLiDE judgment only for the
// inventory it assessed. Changed inventories refresh through the same durable
// request admission. Legacy Jev judgments remain process-local and are
// reasked after restart, and so is an unclear stored GLiDE judgment, once per
// restart.
func (r *DurableRelay) reaskBackgroundAfterRestart() error {
	panic("unported: mj-worker/src/relay/verdict.rs reask_background_after_restart")
}

// reclassifyAssessmentWithGeneratedAuthorization: an assessment classified by
// an earlier worker can rest on a user message that today's rules attribute
// to Mjolnir, such as another session's message. Its verdict and action are
// not the person's to rely on, so the turn is classified again on the
// corrected history, the same way a seeded history does it.
func (r *DurableRelay) reclassifyAssessmentWithGeneratedAuthorization() error {
	panic("unported: mj-worker/src/relay/verdict.rs reclassify_assessment_with_generated_authorization")
}

func (r *DurableRelay) storeAssessment(assessment core.TurnAssessment) error {
	_, err := r.appendRelayEvent(nil, core.RelayObservationTurnAssessmentUpdated{Assessment: assessment})
	return err
}

func (r *DurableRelay) questionLocatorRequest(revision uint64) *core.Request {
	panic("unported: mj-worker/src/relay/verdict.rs question_locator_request")
}

func (r *DurableRelay) questionLocatorCompletion(revision uint64) (uint64, bool) {
	a := r.snapshot.Assessment
	if a == nil || a.Revision != revision || a.InferredQuestion() == nil {
		return 0, false
	}
	return a.CompletedOrdinal, true
}

func (r *DurableRelay) finishQuestionLocator(completedOrdinal uint64, questionMessage *string) (bool, error) {
	entries := r.snapshot.NotificationStack.Entries
	index, found := slices.BinarySearchFunc(entries, completedOrdinal,
		func(entry core.NotificationStackEntry, target uint64) int {
			return cmp.Compare(entry.CompletedOrdinal, target)
		})
	if !found {
		return false, nil
	}
	if _, locating := entries[index].Message.(core.EntryMessageLocating); !locating {
		return false, nil
	}
	if _, err := r.RecordObservation(core.RelayObservationQuestionLocatorFinished{
		CompletedOrdinal: completedOrdinal,
		QuestionMessage:  questionMessage,
	}); err != nil {
		return false, err
	}
	return true, nil
}

func (r *DurableRelay) glideRuntime() (core.ActivityFacts, int) {
	return r.ActivityFacts(), r.nativeAgentCount()
}

func (r *DurableRelay) applyGlideAssessment(ordinal uint64, glide core.Probabilities) (string, error) {
	panic("unported: mj-worker/src/relay/verdict.rs apply_glide_assessment")
}

func (r *DurableRelay) failTurnAssessment(ordinal uint64, reason string) error {
	panic("unported: mj-worker/src/relay/verdict.rs fail_turn_assessment")
}

// PendingRepliedVerdict returns the completed-turn check to send now, once per
// assessment revision.
func (r *DurableRelay) PendingRepliedVerdict() (PendingRepliedVerdict, bool) {
	if !r.classifiesTurns() {
		return PendingRepliedVerdict{}, false
	}
	a := r.snapshot.Assessment
	if a == nil || !a.NeedsClassification(core.EpochMillis()) ||
		(r.repliedVerdict.lastGeneration != nil && *r.repliedVerdict.lastGeneration == a.Revision) {
		return PendingRepliedVerdict{}, false
	}
	if a.Evidence == nil {
		return PendingRepliedVerdict{}, false
	}
	generation := a.Revision
	r.repliedVerdict.lastGeneration = &generation
	r.repliedVerdict.askedTurn = &TurnAssessmentIdentity{TurnID: a.TurnID, CompletedOrdinal: a.CompletedOrdinal}
	turn := TurnAssessmentIdentity{TurnID: a.TurnID, CompletedOrdinal: a.CompletedOrdinal}
	return PendingRepliedVerdict{Generation: a.Revision, Evidence: *a.Evidence, Turn: &turn}, true
}

func (r *DurableRelay) repliedVerdictIsCurrent(generation uint64) bool {
	a := r.snapshot.Assessment
	return a != nil && a.Current() && a.Revision == generation
}

// applyRepliedDecision sets the process-local inference that the activity
// state reads. Jev's verdict on the turn is its assessment record, or the
// stop reason a classifier handoff ended it with.
func (r *DurableRelay) applyRepliedDecision(generation uint64, decision core.Decision, sinceMs int64) (string, error) {
	panic("unported: mj-worker/src/relay/verdict.rs apply_replied_decision")
}
