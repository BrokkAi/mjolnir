// Port of mj-worker/src/relay/capacity_tests.rs.

package worker

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	acp "github.com/BrokkAi/acp-go"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func capacityOpen(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := journalTestOpen(t, root, session, "test")
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	relay.SetTurnVerdictHarness(core.HarnessKindCodex)
	journalTestSessionConfigured(t, relay)
	return relay
}

func capacityStart(t *testing.T, relay *DurableRelay, id string) {
	t.Helper()
	submitRelay(relay, id, prompt("work"))
	if claimed := journalTestClaim(t, relay); claimed[0].CommandID != id {
		t.Fatalf("claimed %s, want %s", claimed[0].CommandID, id)
	}
}

func capacityFinish(t *testing.T, relay *DurableRelay, id, text, stop string) {
	t.Helper()
	journalTestSessionUpdate(t, relay, journalTestTextChunk(text))
	journalTestComplete(t, relay, id, core.RelayCommandOutcomePrompt{StopReason: stop})
}

func capacityAssess(t *testing.T, relay *DurableRelay, retryable bool) {
	t.Helper()
	identity := relay.retryAssessmentIdentity()
	if identity == nil {
		t.Fatal("no current assessment to resolve")
	}
	resolved, err := relay.resolveRetryAssessment(identity, retryable)
	if err != nil {
		t.Fatal(err)
	}
	if resolved != retryable {
		t.Fatalf("resolved %v, want %v", resolved, retryable)
	}
}

func capacitySubmitDue(t *testing.T, relay *DurableRelay, nowMs int64) bool {
	t.Helper()
	submitted, err := relay.SubmitDueCapacityRetry(nowMs)
	if err != nil {
		t.Fatal(err)
	}
	return submitted
}

func capacityPrepare(t *testing.T, relay *DurableRelay) {
	t.Helper()
	if err := relay.preparePendingAssessment(); err != nil {
		t.Fatal(err)
	}
}

func capacityApplyGlide(t *testing.T, relay *DurableRelay, generation uint64, answer core.Probabilities) string {
	t.Helper()
	reason, err := relay.applyGlideAssessment(generation, answer)
	if err != nil {
		t.Fatal(err)
	}
	return reason
}

func TestServerRetryRequiresAJevVerdictAndRetriesWithBackoff(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	id := "original-prompt"
	for index, minutes := range []int64{1, 2, 4, 8, 16, 16} {
		capacityFinish(t, relay, id, "Provider temporarily unavailable", "error")
		if _, armed := relay.CapacityRetryDeadline(); armed {
			t.Fatal("a retry was armed before Jev judged the turn")
		}
		if !relay.OperationalState().RetryAssessmentPending {
			t.Fatal("no retry assessment is pending")
		}
		capacityAssess(t, relay, true)
		retry := relay.OperationalState().CapacityRetry
		if retry == nil {
			t.Fatal("no retry armed")
		}
		if retry.Attempt != uint32(index+1) {
			t.Fatalf("attempt %d, want %d", retry.Attempt, index+1)
		}
		if delay := retry.RetryAtMs - relay.hotEvents[len(relay.hotEvents)-1].RecordedAtMs; delay != minutes*60_000 {
			t.Fatalf("retry delay %d ms, want %d minutes", delay, minutes)
		}
		if capacitySubmitDue(t, relay, retry.RetryAtMs-1) {
			t.Fatal("the retry was submitted early")
		}
		if !capacitySubmitDue(t, relay, retry.RetryAtMs) {
			t.Fatal("the due retry was not submitted")
		}
		if capacitySubmitDue(t, relay, retry.RetryAtMs) {
			t.Fatal("the retry was submitted twice")
		}
		if user := relay.snapshot.Continuation.UserCommandID; user == nil || *user != "original-prompt" {
			t.Fatalf("an automatic retry must not become new user authorization: %v", user)
		}
		commands := journalTestClaim(t, relay)
		if len(commands) != 1 {
			t.Fatalf("claimed %+v", commands)
		}
		if journalTestJSON(t, commands[0].Command) != journalTestJSON(t, prompt("Continue")) {
			t.Fatalf("retry command = %#v", commands[0].Command)
		}
		id = commands[0].CommandID
	}
	capacityFinish(t, relay, id, "Done", "EndTurn")
	capacityAssess(t, relay, false)
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a retry stayed armed")
	}
	if relay.snapshot.CapacityRetry != nil {
		t.Fatal("the retry sequence survived success")
	}
	// A later autonomous refusal starts a new backoff sequence.
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()})
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnSettled{})
	capacityAssess(t, relay, true)
	if attempt := relay.OperationalState().CapacityRetry.Attempt; attempt != 1 {
		t.Fatalf("attempt %d", attempt)
	}
}

func TestRetryDispatchesForEachHarnessAfterJevApproval(t *testing.T) {
	configurations := []struct {
		harness core.HarnessKind
		policy  BackgroundWorkPolicy
	}{
		{core.HarnessKindCodex, BackgroundWorkPolicyAirTasks},
		{core.HarnessKindClaude, BackgroundWorkPolicyAirTasks},
		{core.HarnessKindKimi, BackgroundWorkPolicyKimiTasks},
		{core.HarnessKindGrok, BackgroundWorkPolicyHostedTerminals},
		{core.HarnessKindMuse, BackgroundWorkPolicyHostedTerminals},
	}
	for _, configuration := range configurations {
		root := t.TempDir()
		relay := capacityOpen(t, root)
		relay.SetTurnVerdictHarness(configuration.harness)
		relay.SetBackgroundWorkPolicy(configuration.policy)
		if configuration.harness == core.HarnessKindKimi {
			if err := relay.KimiBackgroundTasksChanged(nil, map[string]struct{}{}, map[string]struct{}{}); err != nil {
				t.Fatal(err)
			}
		}
		capacityStart(t, relay, "original-prompt")
		capacityFinish(t, relay, "original-prompt", "Service temporarily unavailable", "error")
		capacityPrepare(t, relay)
		if harness := relay.snapshot.Assessment.Evidence.Harness; harness != configuration.harness {
			t.Fatalf("evidence harness %v, want %v", harness, configuration.harness)
		}
		capacityAssess(t, relay, true)
		due, armed := relay.CapacityRetryDeadline()
		if !armed {
			t.Fatalf("%v: no retry armed", configuration.harness)
		}
		if !capacitySubmitDue(t, relay, due) {
			t.Fatalf("%v: the due retry was not submitted", configuration.harness)
		}
		if claimed := journalTestClaim(t, relay); journalTestJSON(t, claimed[0].Command) != journalTestJSON(t, prompt("Continue")) {
			t.Fatalf("%v: retry command = %#v", configuration.harness, claimed[0].Command)
		}
	}
}

func TestTextOnlyCapacityMessageAndStructuredErrorsWaitForJev(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "text-prompt")
	capacityFinish(t, relay, "text-prompt", "Selected model is at capacity. Please try a different model.", "EndTurn")
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("text alone armed a retry")
	}
	capacityPrepare(t, relay)
	pending, ok := relay.PendingRepliedVerdict()
	if !ok || pending.Turn == nil {
		t.Fatalf("pending verdict = %+v, %v", pending, ok)
	}
	if pending.Evidence.Completion.StopReason != "EndTurn" {
		t.Fatalf("stop reason %q", pending.Evidence.Completion.StopReason)
	}
	capacityAssess(t, relay, false)
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a rejected assessment armed a retry")
	}

	capacityStart(t, relay, "legacy-reason-prompt")
	capacityFinish(t, relay, "legacy-reason-prompt", "Provider unavailable", "ModelCapacity")
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a legacy stop reason armed a retry")
	}
	capacityAssess(t, relay, false)
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a rejected assessment armed a retry")
	}

	capacityStart(t, relay, "structured-prompt")
	diagnostic := core.TurnDiagnosticFromACP(&acp.RPCError{
		Code:    -32603,
		Message: "failed",
		Data:    json.RawMessage(`{"codex_error_info":"server_overloaded"}`),
	})
	journalTestComplete(t, relay, "structured-prompt", core.RelayCommandOutcomePrompt{StopReason: "error", Diagnostic: &diagnostic})
	capacityPrepare(t, relay)
	evidence := relay.snapshot.Assessment.Evidence
	if code := evidence.Completion.Diagnostic.Code; code == nil || *code != "server_overloaded" {
		t.Fatalf("diagnostic code = %v", code)
	}
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a structured error armed a retry before Jev")
	}
	capacityAssess(t, relay, true)
	if _, armed := relay.CapacityRetryDeadline(); !armed {
		t.Fatal("an approved retry was not armed")
	}
}

func TestPendingAssessmentAndArmedRetrySurviveWorkerReopen(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "temporary service failure", "error")
	relay = capacityOpen(t, root)
	if !relay.OperationalState().RetryAssessmentPending {
		t.Fatal("the pending assessment did not survive reopening")
	}
	capacityPrepare(t, relay)
	if _, ok := relay.PendingRepliedVerdict(); !ok {
		t.Fatal("no verdict is pending after reopening")
	}
	capacityAssess(t, relay, true)
	retry := *relay.OperationalState().CapacityRetry
	relay = capacityOpen(t, root)
	if armed := relay.OperationalState().CapacityRetry; armed == nil || !reflect.DeepEqual(*armed, retry) {
		t.Fatalf("retry after reopening = %+v, want %+v", armed, retry)
	}
	if !capacitySubmitDue(t, relay, retry.RetryAtMs) {
		t.Fatal("the due retry was not submitted")
	}
	if capacitySubmitDue(t, relay, retry.RetryAtMs) {
		t.Fatal("the retry was submitted twice")
	}
}

func TestNewerUserWorkCancelsPendingAssessment(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "temporary service failure", "error")
	identity := relay.retryAssessmentIdentity()
	submitRelay(relay, "new-prompt", prompt("Do something else"))
	if relay.OperationalState().RetryAssessmentPending {
		t.Fatal("newer user work left the assessment pending")
	}
	if resolved, err := relay.resolveRetryAssessment(identity, true); err != nil || resolved {
		t.Fatalf("a superseded assessment resolved: %v %v", resolved, err)
	}
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a superseded assessment armed a retry")
	}
}

func TestAutonomousProviderFailureArmsOneRetryAndSurvivesRestart(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "Continuing the requested work.", "EndTurn")
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnStarted{StartedAtMs: 1})
	journalTestSessionUpdate(t, relay, journalTestTextChunk("Selected model is at capacity. Please try a different model."))
	origin := "test"
	if err := relay.settleHarnessTurn(&origin, "end_turn"); err != nil {
		t.Fatal(err)
	}
	capacityPrepare(t, relay)
	pending, ok := relay.PendingRepliedVerdict()
	if !ok || pending.Evidence.Completion == nil {
		t.Fatalf("pending verdict = %+v", pending)
	}
	generation := pending.Generation
	if reason := capacityApplyGlide(t, relay, generation, glideAnswer("retry_provider", nil)); reason != "server_retry_armed" {
		t.Fatalf("reason %q", reason)
	}
	retry := *relay.OperationalState().CapacityRetry
	if action := relay.snapshot.Assessment.Action; action == nil || *action != core.AssessmentActionRetryProvider {
		t.Fatalf("action = %v", action)
	}
	relay = capacityOpen(t, root)
	if deadline, armed := relay.CapacityRetryDeadline(); !armed || deadline != retry.RetryAtMs {
		t.Fatalf("deadline after reopening = %d, %v", deadline, armed)
	}
	if !capacitySubmitDue(t, relay, retry.RetryAtMs) {
		t.Fatal("the due retry was not submitted")
	}
	if capacitySubmitDue(t, relay, retry.RetryAtMs) {
		t.Fatal("the retry was submitted twice")
	}
	if claimed := journalTestClaim(t, relay); len(claimed) != 1 {
		t.Fatalf("claimed %+v", claimed)
	}
	id := fmt.Sprintf("assessment-%s-%d", session, generation)
	page, err := core.JevRead(filepath.Join(root, "jev-decisions"), session, &id)
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Decisions) != 1 || page.Decisions[0].Status != "consumed" ||
		page.Decisions[0].Action != "automatic action admitted" {
		t.Fatalf("decisions = %+v", page.Decisions)
	}
}

func TestANewUserCommandSupersedesAnInFlightAutonomousAssessment(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "Selected model is at capacity.", "EndTurn")
	capacityPrepare(t, relay)
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("no verdict is pending")
	}
	submitRelay(relay, "replacement-prompt", prompt("Stop that task and do this instead"))
	if reason := capacityApplyGlide(t, relay, pending.Generation, glideAnswer("retry_provider", nil)); reason != "stale_turn" {
		t.Fatalf("reason %q", reason)
	}
	if _, armed := relay.CapacityRetryDeadline(); armed {
		t.Fatal("a stale assessment armed a retry")
	}
}

func TestUncertainAssessmentIsCachedAndPendingCompletionRecovers(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "Insufficient information.", "EndTurn")
	relay = capacityOpen(t, root)
	capacityPrepare(t, relay)
	pending, ok := relay.PendingRepliedVerdict()
	if !ok || pending.Evidence.Authorization == nil || !pending.Evidence.Authorization.AuthorizationComplete {
		t.Fatalf("pending verdict = %+v", pending)
	}
	capacityApplyGlide(t, relay, pending.Generation, glideAnswer("uncertain", nil))
	if _, again := relay.PendingRepliedVerdict(); again {
		t.Fatal("an uncertain answer was asked again")
	}
	relay = capacityOpen(t, root)
	capacityPrepare(t, relay)
	if _, again := relay.PendingRepliedVerdict(); again {
		t.Fatal("an uncertain answer was asked again after reopening")
	}
}

// Hard-won: 295a3f85c: evidence sized without the questions exceeded the request limit on every retry.
func TestStoredEvidencePreservesHistoryBeforeGlideProjection(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "request-one")
	capacityFinish(t, relay, "request-one", "Done.", "EndTurn")
	capacityPrepare(t, relay)
	// A full user budget and recent replies, stored by an earlier build. The
	// history reaches the projection whole: only the provider's refusal cuts
	// it.
	var messages []core.EvidenceMessage
	for index := range 29 {
		messages = append(messages, core.EvidenceMessage{
			ID:   fmt.Sprintf("user:u%d", index),
			Role: "user",
			Text: strings.Repeat("u", 1100),
		})
	}
	for index := range 8 {
		messages = append(messages, core.EvidenceMessage{
			ID:   fmt.Sprintf("agent:a%d", index),
			Role: "assistant",
			Text: strings.Repeat("a", 2700),
		})
	}
	// Rust's Arc::make_mut: change a private copy of the stored assessment.
	assessment := core.DeepClone(*relay.snapshot.Assessment)
	history := core.DefaultContextHistory()
	history.Messages = messages
	assessment.Evidence.Authorization = &history
	assessment.Evidence.TranscriptSummary = ""
	relay.snapshot.Assessment = &assessment

	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("no verdict is pending")
	}
	sent := pending.Evidence
	facts := relay.ActivityFacts()
	body, err := project(&sent, core.DeepClone(*sent.Authorization), &facts, 0)
	if err != nil {
		t.Fatal(err)
	}
	if size := len(journalTestJSON(t, body)); size > 80*1024 {
		t.Fatalf("projection is %d bytes", size)
	}
	if sent.Authorization.AssistantHistoryOmitted {
		t.Fatal("assistant history was omitted")
	}
	users := 0
	for _, message := range sent.Authorization.Messages {
		if message.Role == "user" {
			users++
		}
	}
	if users != 29 {
		t.Fatalf("%d user messages sent", users)
	}
	if last := sent.Authorization.Messages[len(sent.Authorization.Messages)-1]; last.ID != "agent:a7" {
		t.Fatalf("last message %s", last.ID)
	}
}

// Hard-won: b72adcc6f: a retry stayed failed while a fresh assessment was pending.
func TestClassificationTransportBackoffAndDiagnosticsSurviveRestart(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "request-one")
	capacityFinish(t, relay, "request-one", "Selected model is at capacity", "EndTurn")
	capacityPrepare(t, relay)
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("no verdict is pending")
	}
	revision, evidence := pending.Generation, pending.Evidence
	if err := relay.failTurnAssessment(revision, "transport_failed"); err != nil {
		t.Fatal(err)
	}
	a := core.DeepClone(*relay.snapshot.Assessment)
	if a.NeedsClassification(*a.RetryAtMs - 1) {
		t.Fatal("classification is due before its backoff ends")
	}
	if !a.NeedsClassification(*a.RetryAtMs) {
		t.Fatal("classification is not due when its backoff ends")
	}
	relay = capacityOpen(t, root)
	if journalTestJSON(t, relay.snapshot.Assessment) != journalTestJSON(t, a) {
		t.Fatalf("assessment after reopening = %+v", relay.snapshot.Assessment)
	}
	if _, again := relay.PendingRepliedVerdict(); again {
		t.Fatal("a backing-off assessment is pending")
	}
	if journalTestJSON(t, a.Evidence) != journalTestJSON(t, evidence) {
		t.Fatal("the failure changed the stored evidence")
	}
	id := fmt.Sprintf("assessment-%s-%d", session, revision)
	page, err := core.JevRead(filepath.Join(root, "jev-decisions"), session, &id)
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Decisions) != 1 || page.Decisions[0].Status != "failed" || page.Decisions[0].Action != "transport failed" {
		t.Fatalf("decisions = %+v", page.Decisions)
	}

	capacityPrepare(t, relay)
	if journalTestJSON(t, relay.snapshot.Assessment) != journalTestJSON(t, a) {
		t.Fatal("preparing during backoff changed the assessment")
	}
	due := core.DeepClone(a)
	zero := int64(0)
	due.RetryAtMs = &zero
	journalTestRecord(t, relay, core.RelayObservationTurnAssessmentUpdated{Assessment: due})
	capacityPrepare(t, relay)
	requeued := core.DeepClone(*relay.snapshot.Assessment)
	if requeued.Status != core.StatusPending || requeued.RetryAtMs != nil ||
		requeued.Failures != a.Failures || requeued.Revision != revision {
		t.Fatalf("requeued assessment = %+v", requeued)
	}
	if again, ok := relay.PendingRepliedVerdict(); !ok || journalTestJSON(t, again.Evidence) != journalTestJSON(t, evidence) {
		t.Fatalf("requeued verdict = %+v, %v", again, ok)
	}
	capacityPrepare(t, relay)
	if _, again := relay.PendingRepliedVerdict(); again {
		t.Fatal("the requeued verdict was asked twice")
	}
	relay = capacityOpen(t, root)
	if journalTestJSON(t, relay.snapshot.Assessment) != journalTestJSON(t, requeued) {
		t.Fatal("the requeued assessment did not survive reopening")
	}
	if _, ok := relay.PendingRepliedVerdict(); !ok {
		t.Fatal("a restarted worker does not ask about the requeued assessment")
	}
}

func TestACheckpointSeedPreservesPendingAssessmentAndFullAuthorization(t *testing.T) {
	source := t.TempDir()
	relay := capacityOpen(t, source)
	capacityStart(t, relay, "original-request")
	capacityFinish(t, relay, "original-request", "Selected model is at capacity", "EndTurn")
	capacityPrepare(t, relay)
	state := core.Checkpoint{
		Version:        1,
		Context:        core.DeepClone(relay.snapshot.AssessmentContext),
		Assessment:     core.DeepClone(relay.snapshot.Assessment),
		Continuation:   core.DeepClone(relay.snapshot.Continuation),
		CapacityRetry:  core.DeepClone(relay.snapshot.CapacityRetry),
		TurnCompletion: core.DeepClone(relay.snapshot.TurnCompletion),
	}
	destination := t.TempDir()
	seed := map[string]any{
		"event_frontier":        relay.snapshot.LatestOrdinal,
		"event_frontier_digest": relay.snapshot.LatestDigest,
		"assessment_state":      state,
	}
	journalTestWriteFile(t, core.RestoredRelaySeedPath(destination), []byte(journalTestJSON(t, seed)))
	restored := capacityOpen(t, destination)
	pending, ok := restored.PendingRepliedVerdict()
	if !ok {
		t.Fatal("the restored relay has no pending verdict")
	}
	if journalTestJSON(t, pending.Evidence) != journalTestJSON(t, state.Assessment.Evidence) {
		t.Fatal("the restored evidence differs from the checkpoint")
	}
	if journalTestJSON(t, restored.snapshot.AssessmentContext) != journalTestJSON(t, state.Context) {
		t.Fatal("the restored context differs from the checkpoint")
	}
}

// A routine checkpoint is Mjolnir's housekeeping, not user work, so it keeps
// a pending retry assessment and, once assessed, the armed retry (F24).
// Hard-won: 1a0ed612: routine checkpoints cleared capacity assessments and armed retries.
func TestARoutineCheckpointKeepsThePendingAssessmentAndTheArmedRetry(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "temporary service failure", "error")
	identity := relay.retryAssessmentIdentity()
	if identity == nil {
		t.Fatal("no assessment is current")
	}
	readyCheckpoint(relay, "routine-checkpoint-1")
	if !relay.OperationalState().RetryAssessmentPending {
		t.Fatal("the checkpoint cleared the pending assessment")
	}
	if current := relay.retryAssessmentIdentity(); current == nil || *current != *identity {
		t.Fatalf("assessment identity = %+v, want %+v", current, identity)
	}
	submitRelay(relay, "release-1", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "routine-checkpoint-1"})
	capacityAssess(t, relay, true)
	retry := *relay.OperationalState().CapacityRetry
	readyCheckpoint(relay, "routine-checkpoint-2")
	if armed := relay.OperationalState().CapacityRetry; armed == nil || !reflect.DeepEqual(*armed, retry) {
		t.Fatalf("retry after the checkpoint = %+v, want %+v", armed, retry)
	}
}

// Older journals hold a RetryAssessmentStarted record, the durable form of a
// turn that ended on a capacity refusal. A routine checkpoint must not drop it
// (F24).
// Hard-won: 1a0ed612: routine checkpoints cleared capacity assessments and armed retries.
func TestARoutineCheckpointKeepsAJournaledRetryAssessment(t *testing.T) {
	root := t.TempDir()
	relay := capacityOpen(t, root)
	capacityStart(t, relay, "original-prompt")
	capacityFinish(t, relay, "original-prompt", "temporary failure", "error")
	context := &transcript.TurnContext{}
	context.Reset(nil, "Implement")
	facts := core.ActivityFacts{}
	evidence := context.Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &facts, 0)
	journalTestRecord(t, relay, core.RelayObservationRetryAssessmentStarted{CommandID: "original-prompt", Evidence: evidence})
	pending := journalTestJSON(t, relay.snapshot.RetryAssessment)
	if relay.snapshot.RetryAssessment == nil {
		t.Fatal("no retry assessment was recorded")
	}
	readyCheckpoint(relay, "routine-checkpoint")
	if journalTestJSON(t, relay.snapshot.RetryAssessment) != pending {
		t.Fatal("the routine checkpoint changed the retry assessment")
	}
	// A user prompt is still real work and still supersedes it.
	submitRelay(relay, "release-routine-checkpoint", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "routine-checkpoint"})
	submitRelay(relay, "new-prompt", prompt("Something else"))
	if relay.snapshot.RetryAssessment != nil {
		t.Fatal("a user prompt did not supersede the retry assessment")
	}
}
