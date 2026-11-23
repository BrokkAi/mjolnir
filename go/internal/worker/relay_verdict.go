// Port of mj-worker/src/relay/verdict.rs.
//
// Durable completed-turn assessment and process-local activity inference.

package worker

import (
	"cmp"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
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
	if !r.classifiesTurns() {
		return r.suppressChildAssessment()
	}
	if err := r.reclassifyAssessmentWithGeneratedAuthorization(); err != nil {
		return err
	}
	if current := r.snapshot.Assessment; current != nil && current.Current() && current.Action != nil &&
		(*current.Action == core.AssessmentActionWait || *current.Action == core.AssessmentActionUncertain) {
		a := *current
		facts := r.ActivityFacts()
		if e := a.Evidence; e != nil &&
			((a.Glide == nil && e.BackgroundCommands != facts.BackgroundCommands) ||
				e.QueuedCommands != facts.QueuedCommands) {
			a.ClassifyAgain(r.snapshot.LatestOrdinal + 1)
			if err := r.storeAssessment(a); err != nil {
				return err
			}
		}
	}
	if err := r.reaskBackgroundAfterRestart(); err != nil {
		return err
	}
	if current := r.snapshot.Assessment; current != nil && current.Status == core.StatusFailed &&
		current.NeedsClassification(core.EpochMillis()) {
		// A failed request stays failed during backoff. Requeueing it is the
		// durable transition that makes the next check pending again.
		assessment := *current
		assessment.Status = core.StatusPending
		assessment.Reason = "awaiting_classification"
		assessment.RetryAtMs = nil
		if err := r.storeAssessment(assessment); err != nil {
			return err
		}
	}
	current := r.snapshot.Assessment
	if current == nil || !current.NeedsClassification(core.EpochMillis()) || current.Evidence != nil {
		return nil
	}
	a := *current
	if r.verdictHarness == nil {
		return nil
	}
	facts := r.ActivityFacts()
	evidence := r.turnContext.Evidence(*r.verdictHarness, core.TurnPhaseReplied, &facts, core.EpochMillis())
	completion := a.Completion
	evidence.Completion = &completion
	now := core.EpochMillis()
	var background []core.BackgroundEvidence
	for _, command := range r.backgroundCommands() {
		if len(background) == core.InFlightTools {
			break
		}
		background = append(background, core.BackgroundEvidence{
			Command:     core.ToolTitle(command.Command),
			ID:          command.ID,
			StartedSAgo: uint64(max(verdictSaturatingSubI64(now, command.StartedAtMs), 0)) / 1000,
		})
	}
	evidence.Background = background
	evidence.Authorization = r.snapshot.AuthorizationContext()
	a.Evidence = &evidence
	return r.storeAssessment(a)
}

// verdictSaturatingSubI64 is Rust's i64::saturating_sub.
func verdictSaturatingSubI64(a, b int64) int64 {
	difference := a - b
	if (b > 0 && difference > a) || (b < 0 && difference < a) {
		if b > 0 {
			return math.MinInt64
		}
		return math.MaxInt64
	}
	return difference
}

// verdictSaturatingAddI64 is Rust's i64::saturating_add.
func verdictSaturatingAddI64(a, b int64) int64 {
	sum := a + b
	if (b > 0 && sum < a) || (b < 0 && sum > a) {
		if b > 0 {
			return math.MaxInt64
		}
		return math.MinInt64
	}
	return sum
}

// verdictGlideAnswers is serde_json::to_value of the typed probabilities: the
// decoded JSON value the frozen GLiDE rules read.
func verdictGlideAnswers(glide *core.Probabilities) (any, error) {
	encoded, err := workerRuntimeMarshalJSON(glide)
	if err != nil {
		return nil, err
	}
	var answers any
	if err := json.Unmarshal(encoded, &answers); err != nil {
		return nil, err
	}
	return answers, nil
}

// requestBackgroundRefresh queues one more completed-turn check for the
// leftover processes. Waiting and uncertain assessments classify again; other
// completed actions keep their authority while the background judgment
// refreshes.
func (r *DurableRelay) requestBackgroundRefresh(a core.TurnAssessment) error {
	if a.Action != nil && (*a.Action == core.AssessmentActionWait || *a.Action == core.AssessmentActionUncertain) {
		a.ClassifyAgain(r.snapshot.LatestOrdinal + 1)
	} else {
		a.Revision = r.snapshot.LatestOrdinal + 1
		a.Status = core.StatusPending
		a.Evidence = nil
	}
	return r.storeAssessment(a)
}

// reaskBackgroundAfterRestart restores a stored GLiDE judgment only for the
// inventory it assessed. Changed inventories refresh through the same durable
// request admission. Legacy Jev judgments remain process-local and are
// reasked after restart, and so is an unclear stored GLiDE judgment, once per
// restart.
func (r *DurableRelay) reaskBackgroundAfterRestart() error {
	if r.verdictHarness == nil {
		return nil
	}
	current := r.snapshot.Assessment
	if current == nil || !current.Current() || current.Status != core.StatusAssessed {
		return nil
	}
	turn := TurnAssessmentIdentity{TurnID: current.TurnID, CompletedOrdinal: current.CompletedOrdinal}
	askedThisTurn := r.repliedVerdict.askedTurn != nil && *r.repliedVerdict.askedTurn == turn
	if current.Glide == nil && askedThisTurn {
		return nil
	}
	a := *current
	if a.Glide != nil {
		inventory := r.backgroundCommands()
		listedInventory := inventory[:min(len(inventory), core.InFlightTools)]
		sameInventory := false
		if e := a.Evidence; e != nil && e.BackgroundCommands == r.ActivityFacts().BackgroundCommands &&
			len(e.Background) == len(listedInventory) {
			sameInventory = true
			for _, command := range listedInventory {
				if !slices.ContainsFunc(e.Background, func(seen core.BackgroundEvidence) bool {
					return seen.ID == command.ID && seen.Command == core.ToolTitle(command.Command)
				}) {
					sameInventory = false
					break
				}
			}
		}
		if !sameInventory {
			facts := r.ActivityFacts()
			if _, stale := blocked(&facts); stale {
				return nil
			}
			return r.requestBackgroundRefresh(a)
		}
		answers, err := verdictGlideAnswers(a.Glide)
		if err != nil {
			return err
		}
		listed := len(r.backgroundCommands()) > 0
		needed := core.BackgroundNeeded(answers, listed)
		// 9cb9d651: a worker asks once per restart about leftover processes.
		// An unclear stored answer is not a judgment, so the first call after
		// a restart asks again; once this process has asked, it does not.
		if needed == nil && listed && !askedThisTurn {
			facts := r.ActivityFacts()
			if _, stale := blocked(&facts); !stale {
				return r.requestBackgroundRefresh(a)
			}
		}
		r.repliedVerdict.judgeBackground(r.turnContext.Generation(), needed)
		r.repliedVerdict.askedTurn = &turn
		return nil
	}
	facts := r.ActivityFacts()
	if _, stale := blocked(&facts); len(r.backgroundCommands()) == 0 || stale || facts.BackgroundNeeded != nil {
		return nil
	}
	slog.Info("asking Jev again about leftover processes after a worker restart",
		"target", "mj_jev", "session", r.snapshot.SessionID, "revision", a.Revision, "phase", "replied")
	a.ClassifyAgain(r.snapshot.LatestOrdinal + 1)
	return r.storeAssessment(a)
}

// reclassifyAssessmentWithGeneratedAuthorization: an assessment classified by
// an earlier worker can rest on a user message that today's rules attribute
// to Mjolnir, such as another session's message. Its verdict and action are
// not the person's to rely on, so the turn is classified again on the
// corrected history, the same way a seeded history does it.
func (r *DurableRelay) reclassifyAssessmentWithGeneratedAuthorization() error {
	current := r.snapshot.Assessment
	if current == nil || !current.Current() ||
		(current.Action != nil && (*current.Action == core.AssessmentActionRetryProvider ||
			*current.Action == core.AssessmentActionRecoverQuota)) ||
		current.Evidence == nil || current.Evidence.Authorization == nil ||
		!current.Evidence.Authorization.HasGeneratedPrompts() {
		return nil
	}
	a := *current
	a.ClassifyAgain(r.snapshot.LatestOrdinal + 1)
	a.Failures = 0
	a.RetryAtMs = nil
	a.Reason = "authorization_evidence_changed"
	return r.storeAssessment(a)
}

func (r *DurableRelay) storeAssessment(assessment core.TurnAssessment) error {
	_, err := r.appendRelayEvent(nil, core.RelayObservationTurnAssessmentUpdated{Assessment: assessment})
	return err
}

func (r *DurableRelay) questionLocatorRequest(revision uint64) *core.Request {
	assessment := r.snapshot.Assessment
	if assessment == nil {
		return nil
	}
	requiresInput := (assessment.Glide != nil && assessment.Glide.Input.Probabilities.Required >= core.RequiredInput) ||
		(assessment.Verdict != nil && assessment.Verdict.RequiresInput())
	if assessment.Revision != revision || !assessment.Current() || assessment.Action == nil ||
		*assessment.Action != core.AssessmentActionAwaitInput || !requiresInput {
		return nil
	}
	if assessment.Evidence == nil || assessment.Evidence.Authorization == nil {
		return nil
	}
	request := core.Build(assessment.Evidence.Authorization)
	if len(request.MessageIDs) == 0 {
		return nil
	}
	return &request
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
	current := r.snapshot.Assessment
	if current == nil || current.Revision != ordinal || !current.Current() {
		return "stale_turn", nil
	}
	a := *current
	answers, err := verdictGlideAnswers(&glide)
	if err != nil {
		panic(fmt.Sprintf("typed probabilities serialize: %v", err))
	}
	action := core.Action(answers)
	// The quiet judgment is separate from the action: it only ever says
	// whether the listed leftover processes still matter, and only when the
	// request listed some.
	listed := a.Evidence != nil && len(a.Evidence.Background) > 0
	needed := core.BackgroundNeeded(answers, listed)
	r.repliedVerdict.judgeBackground(r.turnContext.Generation(), needed)
	if a.Glide != nil && a.Action != nil {
		a.Glide = &glide
		a.Status = core.StatusAssessed
		a.RetryAtMs = nil
		if err := r.storeAssessment(a); err != nil {
			return "", err
		}
		return "background_judgment_refreshed", nil
	}
	a.Verdict = nil
	a.Glide = &glide
	a.Attention = glideAttention(a.Glide, action)
	a.Action = &action
	a.Status = core.StatusAssessed
	a.RetryAtMs = nil
	suppressed := len(r.snapshot.AssessmentQuestions) > 0 ||
		r.snapshot.Goal.BudgetLimited() ||
		(r.snapshot.Goal.Snapshot != nil && r.snapshot.Goal.Snapshot.Status == "paused") ||
		len(r.snapshot.QueuedPrompts) > 0
	var reason string
	if suppressed {
		a.Status = core.StatusSuperseded
		reason = "automatic_action_suppressed"
	} else {
		switch action {
		case core.AssessmentActionRetryProvider:
			a.Status = core.StatusScheduled
			reason = "server_retry_armed"
		case core.AssessmentActionRecoverQuota:
			a.Status = core.StatusDeferred
			reason = "quota_resolution_required"
		case core.AssessmentActionAwaitInput:
			reason = "user_input_required"
		case core.AssessmentActionFinished:
			reason = "work_finished"
		case core.AssessmentActionWait:
			reason = "background_work_pending"
		default:
			// Continue is legacy: stored assessments may carry it, but
			// nothing acts on it, so it reads as uncertain.
			reason = "assessment_uncertain"
		}
	}
	a.Reason = reason
	if err := r.storeAssessment(a); err != nil {
		return "", err
	}
	if !suppressed {
		decision := core.DecisionKeepCurrent
		switch action {
		case core.AssessmentActionAwaitInput:
			decision = core.DecisionAwaitingInput
		case core.AssessmentActionFinished:
			decision = core.DecisionInferIdle
		case core.AssessmentActionWait:
			decision = core.DecisionExpectContinuation
		}
		// Semantic completion survives; runtime inference only applies to
		// current quiet facts, and never grants resource-replacement safety.
		if _, err := r.applyRepliedDecision(r.turnContext.Generation(), decision, core.EpochMillis()); err != nil {
			return "", err
		}
	}
	return reason, nil
}

func (r *DurableRelay) failTurnAssessment(ordinal uint64, reason string) error {
	current := r.snapshot.Assessment
	if current == nil || current.Revision != ordinal || !current.Current() {
		return nil
	}
	a := *current
	if reason == "classifier_unavailable" {
		a.Status = core.StatusAssessed
	} else {
		a.Status = core.StatusFailed
	}
	if a.Failures < math.MaxUint32 {
		a.Failures++
	}
	backoff := min(int64(60_000)*(int64(1)<<min(max(a.Failures, 1)-1, 3)), 300_000)
	retryAt := verdictSaturatingAddI64(core.EpochMillis(), backoff)
	a.RetryAtMs = &retryAt
	a.Reason = reason
	r.repliedVerdict.lastGeneration = nil
	return r.storeAssessment(a)
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
	// A private copy, as Rust's clone is: the caller may change it.
	return PendingRepliedVerdict{Generation: a.Revision, Evidence: core.DeepClone(*a.Evidence), Turn: &turn}, true
}

func (r *DurableRelay) repliedVerdictIsCurrent(generation uint64) bool {
	a := r.snapshot.Assessment
	return a != nil && a.Current() && a.Revision == generation
}

// applyRepliedDecision sets the process-local inference that the activity
// state reads. Jev's verdict on the turn is its assessment record, or the
// stop reason a classifier handoff ended it with.
func (r *DurableRelay) applyRepliedDecision(generation uint64, decision core.Decision, sinceMs int64) (string, error) {
	facts := r.ActivityFacts()
	if generation != r.turnContext.Generation() {
		return "stale_generation", nil
	}
	if reason, stale := blocked(&facts); stale {
		return reason, nil
	}
	if decision == core.DecisionKeepCurrent {
		return "keep_current", nil
	}
	// Jev's verdict on the turn is its assessment record, or the stop reason
	// a classifier handoff ended it with. This sets only the process-local
	// inference that the activity state reads.
	r.repliedVerdict.inferred = &repliedVerdictInference{generation: generation, decision: decision, sinceMs: sinceMs}
	if err := r.persistActivityTransition(); err != nil {
		return "", err
	}
	after := r.ActivityFacts()
	slog.Info("Jev activity inference applied",
		"target", "mj_jev", "session", r.snapshot.SessionID, "generation", generation,
		"phase", "replied", "decision", decision,
		"previous_activity", fmt.Sprintf("%+v", core.Classify(&facts)),
		"activity", fmt.Sprintf("%+v", core.Classify(&after)))
	return "applied", nil
}

// PreparedHandbackCheck is a sub-agent handback check ready to send: the
// session and generation it is logged under and the evidence it judges.
// Rust: `(String, u64, TurnEvidence)`.
type PreparedHandbackCheck struct {
	SessionID  string
	Generation uint64
	Evidence   core.TurnEvidence
}

// prepareHandbackCheck: the delivery events own assignment identity.
// Reminders and provider retries do not replace it, so all their handbacks
// consume the original assignment.
func (r *DurableRelay) prepareHandbackCheck(report string) (*PreparedHandbackCheck, error) {
	if core.DisabledByEnvironment() {
		return nil, nil
	}
	if r.verdictHarness == nil {
		return nil, nil
	}
	checks := &r.snapshot.HandbackChecks
	if checks.AssignmentID == nil {
		return nil, nil
	}
	assignmentID := *checks.AssignmentID
	if slices.Contains(checks.Checked, assignmentID) {
		return nil, nil
	}
	if checks.Context == nil {
		return nil, nil
	}
	context := core.DeepClone(*checks.Context)
	context.OpenAssistantID = nil
	reportID := "handback-report"
	context.Assistant(&reportID, r.snapshot.LatestOrdinal, report)
	facts := r.ActivityFacts()
	evidence := r.turnContext.Evidence(*r.verdictHarness, core.TurnPhaseReplied, &facts, core.EpochMillis())
	// The report goes whole into the authorization history above; the tail
	// field keeps the bound every classifier route enforces.
	evidence.AssistantTextTail = transcript.Tail(report, core.AssistantTextBytes)
	evidence.FinalToolCalls = nil
	evidence.ToolsInFlight = []core.ToolEvidence{}
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
	if !evidence.Authorization.AuthorizationComplete {
		return nil, nil
	}
	// Journal before HTTP: a crash or cancelled socket consumes the attempt.
	if _, err := r.RecordObservation(core.RelayObservationHandbackCheckStarted{AssignmentID: assignmentID}); err != nil {
		return nil, err
	}
	return &PreparedHandbackCheck{
		SessionID:  r.snapshot.SessionID,
		Generation: r.snapshot.LatestOrdinal,
		Evidence:   evidence,
	}, nil
}
