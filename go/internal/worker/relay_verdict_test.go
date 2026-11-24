// Port of mj-worker/src/relay/verdict.rs (its #[cfg(test)] items and its
// tests and settled_task_tests modules).

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

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

// --- helpers for the verdict tests ---

// verdictSamples is glide_turn::test_samples() as decoded JSON objects.
func verdictSamples(t *testing.T) []map[string]any {
	t.Helper()
	encoded, err := json.Marshal(testSamples())
	if err != nil {
		t.Fatal(err)
	}
	var samples []map[string]any
	if err := json.Unmarshal(encoded, &samples); err != nil {
		t.Fatal(err)
	}
	return samples
}

func verdictSample(t *testing.T, expected string) map[string]any {
	t.Helper()
	for _, sample := range verdictSamples(t) {
		if sample["expected"] == expected {
			return sample
		}
	}
	t.Fatalf("no %s sample", expected)
	return nil
}

// verdictWithBackground is a copy of sample whose background distribution is
// replaced by [needed, unneeded, unclear].
func verdictWithBackground(t *testing.T, sample map[string]any, needed, unneeded, unclear float64) map[string]any {
	t.Helper()
	copied := core.DeepClone(sample)
	answers := copied["answers"].(map[string]any)
	answers["background"].(map[string]any)["probabilities"] = map[string]any{
		"needed": needed, "unneeded": unneeded, "unclear": unclear,
	}
	return copied
}

func verdictParse(t *testing.T, sample map[string]any) core.Probabilities {
	t.Helper()
	probabilities, err := core.ProbabilitiesParse(sample)
	if err != nil {
		t.Fatal(err)
	}
	return probabilities
}

func verdictOpen(t *testing.T, root, sessionID string) *DurableRelay {
	t.Helper()
	return journalTestOpen(t, root, sessionID, "test")
}

// verdictStateOf is the relay's published state, addressable for the
// pointer-receiver queries Rust calls on a temporary.
func verdictStateOf(relay *DurableRelay) *core.RelayOperationalState {
	state := relay.OperationalState()
	return &state
}

func verdictPrepare(t *testing.T, relay *DurableRelay) {
	t.Helper()
	if err := relay.preparePendingAssessment(); err != nil {
		t.Fatal(err)
	}
}

func verdictPending(t *testing.T, relay *DurableRelay) PendingRepliedVerdict {
	t.Helper()
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("no completed-turn verdict is pending")
	}
	return pending
}

func verdictNonePending(t *testing.T, relay *DurableRelay) {
	t.Helper()
	if pending, ok := relay.PendingRepliedVerdict(); ok {
		t.Fatalf("unexpected pending verdict %+v", pending)
	}
}

func verdictApplyGlide(t *testing.T, relay *DurableRelay, revision uint64, glide core.Probabilities) string {
	t.Helper()
	reason, err := relay.applyGlideAssessment(revision, glide)
	if err != nil {
		t.Fatal(err)
	}
	return reason
}

func verdictApplyDecision(t *testing.T, relay *DurableRelay, generation uint64, decision core.Decision, sinceMs int64) string {
	t.Helper()
	reason, err := relay.applyRepliedDecision(generation, decision, sinceMs)
	if err != nil {
		t.Fatal(err)
	}
	return reason
}

func verdictStore(t *testing.T, relay *DurableRelay, assessment core.TurnAssessment) {
	t.Helper()
	if err := relay.storeAssessment(assessment); err != nil {
		t.Fatal(err)
	}
}

// verdictAssessment is a private copy of the current assessment (Rust's
// `.as_deref().cloned()`).
func verdictAssessment(t *testing.T, relay *DurableRelay) core.TurnAssessment {
	t.Helper()
	if relay.snapshot.Assessment == nil {
		t.Fatal("no assessment")
	}
	return core.DeepClone(*relay.snapshot.Assessment)
}

func verdictAction(action core.AssessmentAction) *core.AssessmentAction {
	return &action
}

func verdictSameAction(a, b *core.AssessmentAction) bool {
	return (a == nil) == (b == nil) && (a == nil || *a == *b)
}

func verdictUpdateFromJSON(t *testing.T, encoded string) acpschema.SessionUpdate {
	t.Helper()
	var update acpschema.SessionUpdate
	if err := json.Unmarshal([]byte(encoded), &update); err != nil {
		t.Fatal(err)
	}
	return update
}

func verdictTerminal(t *testing.T, relay *DurableRelay, terminal core.ActiveAgentTerminal) {
	t.Helper()
	if err := relay.AgentTerminalStarted(terminal); err != nil {
		t.Fatal(err)
	}
}

func verdictSetAirTasks(t *testing.T, relay *DurableRelay, tasks []AirTaskFixture) {
	t.Helper()
	if err := relay.setAirTasks(tasks); err != nil {
		t.Fatal(err)
	}
}

func verdictAirTaskChanged(t *testing.T, relay *DurableRelay, update core.AirTaskUpdate) {
	t.Helper()
	if err := relay.AirTaskChanged("root", update); err != nil {
		t.Fatal(err)
	}
}

func verdictSettle(t *testing.T, relay *DurableRelay, origin string) {
	t.Helper()
	if err := relay.settleHarnessTurn(&origin, "end_turn"); err != nil {
		t.Fatal(err)
	}
}

// --- tests ---

func TestGlideCompletedTurnTransportAndReplayPreserveActionWithoutContinue(t *testing.T) {
	for _, sample := range verdictSamples(t) {
		temp := t.TempDir()
		relay := verdictOpen(t, temp, "glide-replay")
		relay.setAcpReady(true)
		setVerdictHarness(t, relay, core.HarnessKindClaude)
		submitRelay(relay, "user-turn", prompt("Complete the task"))
		journalTestClaim(t, relay)
		finishPrompt(relay, "user-turn")
		verdictPrepare(t, relay)
		pending := verdictPending(t, relay)
		revision, evidence := pending.Generation, pending.Evidence
		facts, native := relay.glideRuntime()
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		endpoint := fmt.Sprintf("http://%s/v7/turn-verdict", listener.Addr())
		response, err := json.Marshal(map[string]any{"answers": sample["answers"]})
		if err != nil {
			t.Fatal(err)
		}
		served := make(chan error, 1)
		go func() {
			served <- func() error {
				defer listener.Close()
				socket, err := listener.Accept()
				if err != nil {
					return err
				}
				defer socket.Close()
				reader := bufio.NewReader(socket)
				line, err := reader.ReadString('\n')
				if err != nil {
					return err
				}
				if !strings.HasPrefix(line, "POST /v1/glide-turn-verdict ") {
					return fmt.Errorf("request line %q", line)
				}
				length := 0
				for {
					line, err = reader.ReadString('\n')
					if err != nil {
						return err
					}
					if line == "\r\n" {
						break
					}
					if value, found := strings.CutPrefix(strings.ToLower(line), "content-length:"); found {
						if length, err = strconv.Atoi(strings.TrimSpace(value)); err != nil {
							return err
						}
					}
				}
				body := make([]byte, length)
				if _, err := io.ReadFull(reader, body); err != nil {
					return err
				}
				var request map[string]any
				if err := json.Unmarshal(body, &request); err != nil {
					return err
				}
				if request["latest_user_message"] != "Complete the task" {
					return fmt.Errorf("latest user message %v", request["latest_user_message"])
				}
				if _, found := request["questions"]; found {
					return errors.New("the request carries questions")
				}
				_, err = fmt.Fprintf(socket, "HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", len(response), response)
				return err
			}()
		}()
		client, err := newVerdictClient(VerdictSourceHosted{Endpoint: endpoint})
		if err != nil {
			t.Fatal(err)
		}
		answer, err := client.askGlideTurn(context.Background(), &evidence, core.DeepClone(*evidence.Authorization), &facts, native)
		if err != nil {
			t.Fatal(err)
		}
		if err := <-served; err != nil {
			t.Fatal(err)
		}
		verdictApplyGlide(t, relay, revision, answer)
		assessment := verdictAssessment(t, relay)
		if assessment.Action != nil && *assessment.Action == core.AssessmentActionContinue {
			t.Fatal("the legacy Continue action was produced")
		}
		if assessment.Glide == nil {
			t.Fatal("the GLiDE distributions were not stored")
		}
		var expectedAttention *core.AssessmentAttention
		if assessment.Action == nil || *assessment.Action != core.AssessmentActionAwaitInput {
			important := core.AssessmentAttentionImportant
			expectedAttention = &important
		}
		if !reflect.DeepEqual(assessment.Attention, expectedAttention) {
			t.Fatalf("attention = %v, want %v", assessment.Attention, expectedAttention)
		}
		legacy := snapshotTestDecode[jsonvalue.Object](t, journalTestJSON(t, assessment.Glide))
		legacy.Delete("attention")
		if parsed := snapshotTestDecode[core.Probabilities](t, journalTestJSON(t, legacy)); parsed.Attention != nil {
			t.Fatal("a legacy distribution grew an attention answer")
		}
		replay := verdictOpen(t, temp, "glide-replay")
		replayed := replay.snapshot.Assessment
		if !verdictSameAction(replayed.Action, assessment.Action) {
			t.Fatalf("replayed action %v, want %v", replayed.Action, assessment.Action)
		}
		if !reflect.DeepEqual(replayed.Attention, assessment.Attention) {
			t.Fatalf("replayed attention %v, want %v", replayed.Attention, assessment.Attention)
		}
		if journalTestJSON(t, replayed.Glide) != journalTestJSON(t, assessment.Glide) {
			t.Fatal("replay changed the GLiDE distributions")
		}
		old := snapshotTestDecode[jsonvalue.Object](t, journalTestJSON(t, assessment))
		old.Delete("glide")
		decoded := snapshotTestDecode[core.TurnAssessment](t, journalTestJSON(t, old))
		if !verdictSameAction(decoded.Action, assessment.Action) || decoded.Glide != nil {
			t.Fatalf("an assessment without GLiDE decoded as %+v", decoded)
		}
	}
}

// Hard-won: 2e95f7097: late background inventory reused probabilities for absent processes.
func TestGlideLateBackgroundInventoryRefreshesOnceAfterRestart(t *testing.T) {
	answer := verdictWithBackground(t, verdictSample(t, "finished"), 0.0, 0.0, 1.0)
	for _, restart := range []bool{false, true} {
		temp := t.TempDir()
		relay := verdictOpen(t, temp, "late-background")
		relay.setAcpReady(true)
		setVerdictHarness(t, relay, core.HarnessKindClaude)
		submitRelay(relay, "user-turn", prompt("Complete the task"))
		journalTestClaim(t, relay)
		finishPrompt(relay, "user-turn")
		verdictPrepare(t, relay)
		verdictApplyGlide(t, relay, verdictPending(t, relay).Generation, verdictParse(t, answer))
		action := relay.snapshot.Assessment.Action
		if restart {
			relay = verdictOpen(t, temp, "late-background")
			relay.setAcpReady(true)
			setVerdictHarness(t, relay, core.HarnessKindClaude)
		}
		verdictTerminal(t, relay, core.ActiveAgentTerminal{TerminalID: "late", Command: "server", StartedAtMs: 100})
		if verdictStateOf(relay).IsQuiet() {
			t.Fatal("a live terminal is quiet")
		}
		verdictPrepare(t, relay)
		pending := verdictPending(t, relay)
		if len(pending.Evidence.Background) != 1 {
			t.Fatalf("background evidence = %+v", pending.Evidence.Background)
		}
		verdictPrepare(t, relay)
		verdictNonePending(t, relay)
		refreshed := verdictWithBackground(t, answer, 0.0, 1.0, 0.0)
		verdictApplyGlide(t, relay, pending.Generation, verdictParse(t, refreshed))
		if needed := relay.ActivityFacts().BackgroundNeeded; needed == nil || *needed {
			t.Fatalf("background needed = %v", needed)
		}
		if !verdictStateOf(relay).IsQuiet() {
			t.Fatal("an unneeded leftover process keeps the session busy")
		}
		if !verdictSameAction(relay.snapshot.Assessment.Action, action) {
			t.Fatal("the background refresh changed the turn's action")
		}
		verdictPrepare(t, relay)
		verdictNonePending(t, relay)
	}
}

// Hard-won: 8ad2f6a1d: Wait survived changed and empty background inventories forever.
func TestGlideWaitInventoryChangesReclassifyOnce(t *testing.T) {
	finished := verdictSample(t, "finished")
	waiting := verdictSample(t, "wait")
	for _, remaining := range []bool{false, true} {
		temp := t.TempDir()
		relay := verdictOpen(t, temp, "wait-background")
		relay.setAcpReady(true)
		setVerdictHarness(t, relay, core.HarnessKindClaude)
		submitRelay(relay, "user-turn", prompt("Finish the task"))
		journalTestClaim(t, relay)
		verdictTerminal(t, relay, core.ActiveAgentTerminal{TerminalID: "first", Command: "checks", StartedAtMs: 100})
		finishPrompt(relay, "user-turn")
		verdictPrepare(t, relay)
		verdictApplyGlide(t, relay, verdictPending(t, relay).Generation, verdictParse(t, waiting))
		if action := relay.snapshot.Assessment.Action; action == nil || *action != core.AssessmentActionWait {
			t.Fatalf("action = %v", action)
		}
		if relay.ActivityFacts().ExpectedContinuation == nil {
			t.Fatal("Wait did not expect a continuation")
		}
		if remaining {
			verdictTerminal(t, relay, core.ActiveAgentTerminal{TerminalID: "second", Command: "more checks", StartedAtMs: 200})
		}
		if err := relay.AgentTerminalClosed("first"); err != nil {
			t.Fatal(err)
		}
		verdictPrepare(t, relay)
		pending := verdictPending(t, relay)
		expected := 0
		if remaining {
			expected = 1
		}
		if len(pending.Evidence.Background) != expected {
			t.Fatalf("background evidence = %+v", pending.Evidence.Background)
		}
		verdictPrepare(t, relay)
		verdictNonePending(t, relay)
		verdictApplyGlide(t, relay, pending.Generation, verdictParse(t, finished))
		if action := relay.snapshot.Assessment.Action; action == nil || *action != core.AssessmentActionFinished {
			t.Fatalf("action = %v", action)
		}
		if relay.ActivityFacts().ExpectedContinuation != nil {
			t.Fatal("Finished still expects a continuation")
		}
		verdictPrepare(t, relay)
		verdictNonePending(t, relay)
	}
}

// verdictNormalized decodes a JSON value through encoding/json, so values
// built by different encoders compare by content.
func verdictNormalized(t *testing.T, value any) map[string]any {
	t.Helper()
	var normalized map[string]any
	if err := json.Unmarshal([]byte(journalTestJSON(t, value)), &normalized); err != nil {
		t.Fatal(err)
	}
	return normalized
}

// Hard-won: 2e95f7097: a large current exchange could never fit the GLiDE request.
func TestGlideWorkerProjectionCaptureAndLargeHistoryBound(t *testing.T) {
	temp := t.TempDir()
	relay := verdictOpen(t, temp, "glide-projection-test")
	relay.setAcpReady(true)
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "user-turn", prompt("Run the workspace checks and report failures."))
	journalTestClaim(t, relay)
	journalTestSessionUpdate(t, relay, verdictUpdateFromJSON(t,
		`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"The workspace checks passed. All requested checks are complete."}}`))
	finishPrompt(relay, "user-turn")
	facts := relay.ActivityFacts()
	evidence := relay.turnContext.Evidence(core.HarnessKindClaude, core.TurnPhaseReplied, &facts, 200)
	completion := core.CompletionEvidenceBounded("end_turn", nil)
	evidence.Completion = &completion
	history := core.DeepClone(*relay.snapshot.AuthorizationContext())
	projected, err := project(&evidence, core.DeepClone(history), &facts, relay.nativeAgentCount())
	if err != nil {
		t.Fatal(err)
	}
	body := verdictNormalized(t, projected)
	// The fixture is the jev-proxy's copy of this capture
	// (services/jev-proxy/tests/fixtures/eot-glide-worker.json); Rust
	// refreshes it with MJ_UPDATE_GLIDE_FIXTURE, Go only compares.
	fixture, err := os.ReadFile(filepath.Join("testdata", "relay_verdict", "eot-glide-worker.json"))
	if err != nil {
		t.Fatal(err)
	}
	var captured map[string]any
	if err := json.Unmarshal(fixture, &captured); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(body, captured) {
		t.Fatalf("projection differs from the capture:\n%s", journalTestJSON(t, body))
	}
	current := core.DeepClone(history)
	for index := range 100 {
		current.Messages = slices.Insert(current.Messages, 1, core.EvidenceMessage{
			ID:   fmt.Sprintf("current:%d", index),
			Role: "assistant",
			Text: strings.Repeat("progress ", 500),
		})
	}
	boundedCurrentValue, err := project(&evidence, current, &facts, relay.nativeAgentCount())
	if err != nil {
		t.Fatal(err)
	}
	if size := len(journalTestJSON(t, boundedCurrentValue)); size <= 200*1024 {
		t.Fatalf("current-exchange projection is only %d bytes", size)
	}
	boundedCurrent := verdictNormalized(t, boundedCurrentValue)
	if !reflect.DeepEqual(boundedCurrent["latest_user_message"], body["latest_user_message"]) {
		t.Fatal("the latest user message changed")
	}
	lastReply := func(value map[string]any) any {
		replies := value["current_assistant_replies"].([]any)
		return replies[len(replies)-1]
	}
	if !reflect.DeepEqual(lastReply(boundedCurrent), lastReply(body)) {
		t.Fatal("the final assistant reply changed")
	}
	large := history
	for index := range 500 {
		large.Messages = slices.Insert(large.Messages, 0, core.EvidenceMessage{
			ID:   fmt.Sprintf("old:%d", index),
			Role: "assistant",
			Text: strings.Repeat("historical context ", 100),
		})
	}
	boundedValue, err := project(&evidence, large, &facts, relay.nativeAgentCount())
	if err != nil {
		t.Fatal(err)
	}
	if size := len(journalTestJSON(t, boundedValue)); size <= 800*1024 {
		t.Fatalf("large-history projection is only %d bytes", size)
	}
	bounded := verdictNormalized(t, boundedValue)
	if !reflect.DeepEqual(bounded["latest_user_message"], body["latest_user_message"]) {
		t.Fatal("the latest user message changed")
	}
	if !reflect.DeepEqual(bounded["runtime_observations"], body["runtime_observations"]) {
		t.Fatal("the runtime observations changed")
	}
	if _, found := bounded["questions"]; found {
		t.Fatal("the projection carries questions")
	}
	if strings.Contains(journalTestJSON(t, bounded["runtime_observations"]), "historical context") {
		t.Fatal("history leaked into the runtime observations")
	}
}

func TestManagedChildCompletionAndSilentTurnNeverRequestClassification(t *testing.T) {
	for _, managed := range []bool{false, true} {
		temp := t.TempDir()
		relay := verdictOpen(t, temp, "child-verdict-test")
		relay.setAcpReady(true)
		if err := relay.setManagedSubagent(managed); err != nil {
			t.Fatal(err)
		}
		setVerdictHarness(t, relay, core.HarnessKindClaude)
		submitRelay(relay, "user-turn", prompt("Do the assigned work."))
		journalTestClaim(t, relay)
		verdictPrepare(t, relay)
		verdictNonePending(t, relay)
		// Silence alone never ends an observed open turn.
		if relay.ActivityFacts().PromptStartedAtMs == nil {
			t.Fatal("the open turn has no start")
		}
		finishPrompt(relay, "user-turn")
		verdictPrepare(t, relay)
		if _, pending := relay.PendingRepliedVerdict(); pending != !managed {
			t.Fatalf("managed=%v: pending = %v", managed, pending)
		}
		if pending := relay.OperationalState().RetryAssessmentPending; pending != !managed {
			t.Fatalf("managed=%v: retry assessment pending = %v", managed, pending)
		}
		if relay.ActivityFacts().PromptStartedAtMs != nil {
			t.Fatal("the finished turn still has a start")
		}
		if managed {
			if !verdictStateOf(relay).IsQuiet() {
				t.Fatal("a finished managed child is not quiet")
			}
			if relay.snapshot.Assessment.Current() {
				t.Fatal("a managed child's assessment is current")
			}
		}
		submitRelay(relay, "required-turn", prompt("Choose the next step"))
		journalTestClaim(t, relay)
		// An explicit ACP message ID becomes the transcript's agent: identity.
		messageID := acpschema.MessageId("msg_question")
		journalTestSessionUpdate(t, relay, acpschema.SessionUpdate{
			Kind: acpschema.SessionUpdateKindAgentMessageChunk,
			AgentMessageChunk: &acpschema.ContentChunk{
				Content:   journalTestTextBlock("Keep the old format?"),
				MessageID: &messageID,
			},
		})
		journalTestComplete(t, relay, "required-turn", core.RelayCommandOutcomePrompt{StopReason: core.AwaitingInputStopReason})
		if open := relay.snapshot.OpenRequest(); (open != nil) != !managed {
			t.Fatalf("managed=%v: open request = %+v", managed, open)
		}
		if !managed {
			if question := relay.snapshot.OpenRequest().QuestionMessage; question == nil || *question != "agent:msg_question" {
				t.Fatalf("question message = %v", question)
			}
		}
		relay = verdictOpen(t, temp, "child-verdict-test")
		if relay.snapshot.ManagedSubagent != managed {
			t.Fatal("the managed flag did not survive reopening")
		}
		if open := relay.snapshot.OpenRequest(); (open != nil) != !managed {
			t.Fatalf("managed=%v: open request after reopening = %+v", managed, open)
		}
	}
	// An old worker's durable pending assessment is superseded on launch.
	temp := t.TempDir()
	relay := verdictOpen(t, temp, "legacy-child")
	submitRelay(relay, "old-turn", prompt("Work"))
	journalTestClaim(t, relay)
	finishPrompt(relay, "old-turn")
	relay = verdictOpen(t, temp, "legacy-child")
	if err := relay.setManagedSubagent(true); err != nil {
		t.Fatal(err)
	}
	if relay.OperationalState().RetryAssessmentPending {
		t.Fatal("the legacy assessment is still pending")
	}
	if relay.snapshot.Assessment.Current() {
		t.Fatal("the legacy assessment is current")
	}
}

// Hard-won: 97731d30a: an assessment classified with a peer message as user authorization kept its action.
func TestAnAssessmentRestingOnANowGeneratedPromptIsClassifiedAgain(t *testing.T) {
	temp := t.TempDir()
	relay := verdictOpen(t, temp, "reclassify-test")
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "user-turn", prompt("Fix the build."))
	journalTestClaim(t, relay)
	finishPrompt(relay, "user-turn")
	verdictPrepare(t, relay)
	assessed := func(peer bool) core.TurnAssessment {
		assessment := verdictAssessment(t, relay)
		facts := relay.ActivityFacts()
		evidence := relay.turnContext.Evidence(core.HarnessKindClaude, core.TurnPhaseReplied, &facts, 0)
		messages := []core.EvidenceMessage{{ID: "user:user-turn", Role: "user", Text: "Fix the build."}}
		if peer {
			messages = append(messages, core.EvidenceMessage{ID: "user:session-message-turn-peer-1", Role: "user", Text: "Please also push it."})
		}
		messages = append(messages, core.EvidenceMessage{ID: "agent:reply", Role: "assistant", Text: "Fixed; pushing next."})
		history := core.DefaultContextHistory()
		history.Messages = messages
		evidence.Authorization = &history
		assessment.Evidence = &evidence
		assessment.Action = verdictAction(core.AssessmentActionContinue)
		assessment.Status = core.StatusAssessed
		return assessment
	}
	verdictStore(t, relay, assessed(false))
	verdictPrepare(t, relay)
	if status := relay.snapshot.Assessment.Status; status != core.StatusAssessed {
		t.Fatalf("status = %v", status)
	}

	stale := assessed(true)
	reply := "agent:reply"
	stale.QuestionMessage = &reply
	verdictStore(t, relay, stale)
	verdictPrepare(t, relay)
	a := relay.snapshot.Assessment
	if a.Status != core.StatusPending || a.Action != nil || a.Reason != "authorization_evidence_changed" || a.QuestionMessage != nil {
		t.Fatalf("assessment = %+v", a)
	}
}

func TestQuestionTargetIsStoredOnlyForCurrentInputGateAssessment(t *testing.T) {
	temp := t.TempDir()
	relay := verdictOpen(t, temp, "locator-test")
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "question-turn", prompt("Choose."))
	journalTestClaim(t, relay)
	finishPrompt(relay, "question-turn")
	verdictPrepare(t, relay)
	revision := verdictPending(t, relay).Generation
	assessment := verdictAssessment(t, relay)
	facts := relay.ActivityFacts()
	evidence := relay.turnContext.Evidence(core.HarnessKindClaude, core.TurnPhaseReplied, &facts, 0)
	history := core.DefaultContextHistory()
	history.Messages = []core.EvidenceMessage{
		{ID: "user:1", Role: "user", Text: "task"},
		{ID: "agent:one", Role: "assistant", Text: "Choose?"},
		{ID: "agent:two", Role: "assistant", Text: "Waiting."},
	}
	evidence.Authorization = &history
	assessment.Evidence = &evidence
	assessment.Verdict = &core.Verdict{
		Failure: core.Judgment[core.AssessmentFailure]{Choice: core.AssessmentFailureNone, Confidence: 1.0, Probabilities: map[string]float64{}},
		Input:   core.Judgment[core.AssessmentInput]{Choice: core.AssessmentInputRequired, Confidence: 1.0, Probabilities: map[string]float64{"required": 1.0}},
		Work:    core.Judgment[core.AssessmentWork]{Choice: core.AssessmentWorkWaiting, Confidence: 1.0, Probabilities: map[string]float64{}},
	}
	assessment.Action = verdictAction(core.AssessmentActionAwaitInput)
	assessment.Status = core.StatusAssessed
	verdictStore(t, relay, assessment)
	request := relay.questionLocatorRequest(revision)
	if request == nil || !slices.Equal(request.MessageIDs, []string{"agent:one", "agent:two"}) {
		t.Fatalf("request = %+v", request)
	}
	storeTarget := func(revision uint64, stableID string) bool {
		t.Helper()
		stored, err := relay.storeQuestionTarget(revision, stableID)
		if err != nil {
			t.Fatal(err)
		}
		return stored
	}
	if storeTarget(revision+1, "agent:one") {
		t.Fatal("a target was stored for another revision")
	}
	if !storeTarget(revision, "agent:one") {
		t.Fatal("the target was not stored")
	}
	if question := relay.snapshot.Assessment.QuestionMessage; question == nil || *question != "agent:one" {
		t.Fatalf("question message = %v", question)
	}
	if storeTarget(revision, "agent:two") {
		t.Fatal("a second target was stored")
	}

	oneReply := verdictAssessment(t, relay)
	oneReply.QuestionMessage = nil
	messages := oneReply.Evidence.Authorization.Messages
	oneReply.Evidence.Authorization.Messages = messages[:len(messages)-1]
	verdictStore(t, relay, oneReply)
	if request := relay.questionLocatorRequest(revision); request == nil || !slices.Equal(request.MessageIDs, []string{"agent:one"}) {
		t.Fatalf("request = %+v", request)
	}

	for _, action := range []core.AssessmentAction{core.AssessmentActionFinished, core.AssessmentActionWait} {
		nonInput := verdictAssessment(t, relay)
		nonInput.Action = verdictAction(action)
		verdictStore(t, relay, nonInput)
		if request := relay.questionLocatorRequest(revision); request != nil {
			t.Fatalf("%v: request = %+v", action, request)
		}
	}
	otherFailure := verdictAssessment(t, relay)
	otherFailure.Action = verdictAction(core.AssessmentActionAwaitInput)
	otherFailure.Verdict.Input.Choice = core.AssessmentInputNone
	otherFailure.Verdict.Failure.Choice = core.AssessmentFailureOther
	verdictStore(t, relay, otherFailure)
	if request := relay.questionLocatorRequest(revision); request != nil {
		t.Fatalf("request = %+v", request)
	}
}

func verdictCompleted(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := verdictOpen(t, root, "jev-test")
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	relay.backgroundWork = BackgroundWorkPolicyAirTasks
	relay.turnContext.Reset(nil, "How many tickets fell out of s21?")
	journalTestSessionUpdate(t, relay, verdictUpdateFromJSON(t,
		`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"One ticket so far: #3486."}}`))
	verdictSetAirTasks(t, relay, verdictTasks())
	for _, task := range relay.airTasks {
		if task.state.Kind == AirTaskStateKindBackground {
			task.state.Background.StartedAtMs = 100
		}
	}
	relay.ActivityFacts()
	return relay
}

func verdictTasks() []AirTaskFixture {
	var tasks []AirTaskFixture
	for id := range 4 {
		tasks = append(tasks, AirTaskFixture{TaskID: strconv.Itoa(id), Description: fmt.Sprintf("Old provisioning wait %d", id)})
	}
	return tasks
}

func TestFinishedReplyOverridesOldTasksWithoutLosingInventoryOrSafety(t *testing.T) {
	temp := t.TempDir()
	relay := verdictCompleted(t, temp)
	canStop := true
	verdictAirTaskChanged(t, relay, core.AirTaskUpdateChanged{TaskID: "0", CanStop: &canStop})
	before := relay.OperationalState().BackgroundCommands
	if !slices.ContainsFunc(before, func(task core.BackgroundCommand) bool { return task.ID == "air:4:root:0" && task.CanStop }) {
		t.Fatalf("background commands = %+v", before)
	}
	generation := relay.turnContext.Generation()
	facts := relay.ActivityFacts()
	evidence := relay.turnContext.Evidence(core.HarnessKindClaude, core.TurnPhaseReplied, &facts, 200)
	if evidence.BackgroundCommands != 4 || evidence.AssistantTextTail != "One ticket so far: #3486." {
		t.Fatalf("evidence = %+v", evidence)
	}
	if reason := verdictApplyDecision(t, relay, generation, core.DecisionInferIdle, 200); reason != "applied" {
		t.Fatalf("reason %q", reason)
	}
	state := relay.OperationalState()
	since := int64(200)
	if activity := state.ActivityState(); !reflect.DeepEqual(activity, core.ActivityState{Kind: core.ActivityStateIdle, SinceMs: &since}) {
		t.Fatalf("activity = %+v", activity)
	}
	if !reflect.DeepEqual(state.BackgroundCommands, before) {
		t.Fatalf("background commands changed: %+v", state.BackgroundCommands)
	}
	target, err := relay.BackgroundTaskStopTarget("air:4:root:0")
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(target, core.BackgroundTaskStopTarget(core.BackgroundTaskStopTargetAirAsyncTask{SessionID: "root", TaskID: "0"})) {
		t.Fatalf("stop target = %#v", target)
	}
	if journalTestJSON(t, state.Facts()) != journalTestJSON(t, relay.ActivityFacts()) {
		t.Fatal("the published facts differ from the worker's")
	}
	stateFacts := state.Facts()
	if core.SafeToReplace(&stateFacts, core.HarnessKindClaude) {
		t.Fatal("an inferred-idle session with background tasks is replaceable")
	}
	if state.IsQuiet() {
		t.Fatal("an inferred-idle session with background tasks is quiet")
	}
	verdictNonePending(t, relay)
	reopened := verdictOpen(t, temp, "jev-test")
	if reopened.ActivityFacts().InferredIdleSinceMs != nil {
		t.Fatal("a process-local inference survived reopening")
	}
}

func TestIdenticalInventoryPreservesVerdictButSameCountReplacementInvalidatesIt(t *testing.T) {
	temp := t.TempDir()
	relay := verdictCompleted(t, temp)
	generation := relay.turnContext.Generation()
	verdictApplyDecision(t, relay, generation, core.DecisionInferIdle, 200)
	unchanged := verdictTasks()
	slices.Reverse(unchanged)
	verdictSetAirTasks(t, relay, unchanged)
	if relay.turnContext.Generation() != generation {
		t.Fatal("an identical inventory moved the generation")
	}
	activity := verdictStateOf(relay).ActivityState()
	if !activity.IsIdle() {
		t.Fatalf("activity = %+v", activity)
	}
	verdictNonePending(t, relay)
	// A repeated informational update is not resumed foreground work.
	journalTestSessionUpdate(t, relay, verdictUpdateFromJSON(t, `{"sessionUpdate":"usage_update","used":10,"size":100}`))
	activity = verdictStateOf(relay).ActivityState()
	if !activity.IsIdle() {
		t.Fatalf("activity after usage = %+v", activity)
	}
	replaced := verdictTasks()
	replaced[0].TaskID = "replacement"
	verdictSetAirTasks(t, relay, replaced)
	if kind := verdictStateOf(relay).ActivityState().Kind; kind != core.ActivityStateBackground {
		t.Fatalf("activity kind = %v", kind)
	}
	newGeneration := relay.turnContext.Generation()
	if newGeneration == generation {
		t.Fatal("a replaced inventory kept the generation")
	}
	if reason := verdictApplyDecision(t, relay, generation, core.DecisionInferIdle, 300); reason != "stale_generation" {
		t.Fatalf("reason %q", reason)
	}
	verdictApplyDecision(t, relay, newGeneration, core.DecisionExpectContinuation, 400)
	if kind := verdictStateOf(relay).ActivityState().Kind; kind != core.ActivityStateBackground {
		t.Fatalf("activity kind = %v", kind)
	}
}

func TestHarnessRestartInvalidatesTheCompletedTurnInferenceAndRequest(t *testing.T) {
	temp := t.TempDir()
	relay := verdictCompleted(t, temp)
	generation := relay.turnContext.Generation()
	verdictApplyDecision(t, relay, generation, core.DecisionInferIdle, 200)
	journalTestRecord(t, relay, core.RelayObservationSessionRestarted{})
	if relay.ActivityFacts().InferredIdleSinceMs != nil {
		t.Fatal("the inference survived a harness restart")
	}
	if relay.repliedVerdictIsCurrent(generation) {
		t.Fatal("the old verdict is still current")
	}
	verdictNonePending(t, relay)
	if reason := verdictApplyDecision(t, relay, generation, core.DecisionInferIdle, 300); reason != "stale_generation" {
		t.Fatalf("reason %q", reason)
	}
}

// --- settled_task_tests ---

func settledClaudeRelay(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := verdictOpen(t, root, "settle-test")
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	relay.backgroundWork = BackgroundWorkPolicyAirTasks
	return relay
}

func settledRemoveState(t *testing.T, root string) {
	t.Helper()
	if err := os.Remove(filepath.Join(root, core.RelayStateFile)); err != nil {
		t.Fatal(err)
	}
}

func TestPersonInputFrontierFencesLateAssessmentsAndLocatorsAcrossReplay(t *testing.T) {
	for _, formAnswer := range []bool{true, false} {
		for _, replayBeforeLate := range []bool{false, true} {
			dir := t.TempDir()
			relay := settledClaudeRelay(t, dir)
			relay.setAcpReady(true)
			submitRelay(relay, "original-person", prompt("Choose the format"))
			journalTestClaim(t, relay)
			finishPrompt(relay, "original-person")
			assessment := verdictAssessment(t, relay)
			assessment.Status = core.StatusAssessed
			assessment.Action = verdictAction(core.AssessmentActionAwaitInput)
			revision := assessment.Revision
			if formAnswer {
				verdictStore(t, relay, core.DeepClone(assessment))
				reply := "Old format"
				journalTestRecord(t, relay, core.RelayObservationElicitationResolved{ElicitationID: "human-form", Action: "accept", Reply: &reply})
			} else {
				submitRelay(relay, "new-person-input", prompt("Use the old format"))
			}
			frontier := relay.snapshot.PersonInputThrough
			if frontier <= assessment.CompletedOrdinal {
				t.Fatalf("frontier %d is not past the completion %d", frontier, assessment.CompletedOrdinal)
			}
			if relay.snapshot.OpenRequest() != nil {
				t.Fatal("the answered request is still open")
			}
			if replayBeforeLate {
				settledRemoveState(t, dir)
				relay = settledClaudeRelay(t, dir)
				if relay.snapshot.PersonInputThrough != frontier {
					t.Fatalf("replayed frontier %d, want %d", relay.snapshot.PersonInputThrough, frontier)
				}
			}
			if formAnswer {
				verdictStore(t, relay, assessment)
				stored, err := relay.storeQuestionTarget(revision, "agent:original")
				if err != nil {
					t.Fatal(err)
				}
				if stored {
					t.Fatal("a late locator stored a target")
				}
				if len(relay.snapshot.NotificationStack.Entries) != 0 {
					t.Fatalf("notification entries = %+v", relay.snapshot.NotificationStack.Entries)
				}
			} else if reason := verdictApplyGlide(t, relay, revision, glideAnswer("await_input", nil)); reason != "stale_turn" {
				t.Fatalf("reason %q", reason)
			}
			if relay.snapshot.OpenRequest() != nil || relay.OperationalState().OpenRequest != nil {
				t.Fatal("a late assessment reopened the answered request")
			}
			settledRemoveState(t, dir)
			relay = settledClaudeRelay(t, dir)
			if relay.snapshot.PersonInputThrough != frontier {
				t.Fatalf("replayed frontier %d, want %d", relay.snapshot.PersonInputThrough, frontier)
			}
			if relay.OperationalState().OpenRequest != nil {
				t.Fatal("replay reopened the answered request")
			}
		}
	}
}

func TestOpenRequestSurvivesAgentOnlyCompletionReplayAndOriginalDismissal(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	submitRelay(relay, "person-command", prompt("Choose a target"))
	journalTestClaim(t, relay)
	finishPrompt(relay, "person-command")
	assessment := verdictAssessment(t, relay)
	assessment.Status = core.StatusAssessed
	assessment.Action = verdictAction(core.AssessmentActionAwaitInput)
	verdictStore(t, relay, assessment)
	revision := relay.snapshot.Assessment.Revision
	if stored, err := relay.storeQuestionTarget(revision, "agent:original-question"); err != nil || !stored {
		t.Fatalf("store target: %v %v", stored, err)
	}
	request := core.DeepClone(*relay.snapshot.OpenRequest())
	if request.QuestionMessage == nil || *request.QuestionMessage != "agent:original-question" {
		t.Fatalf("question message = %v", request.QuestionMessage)
	}
	id := core.GeneratedPromptSessionMessageTurn.CommandID("follow-up")
	submitRelay(relay, id, prompt("An agent-only follow-up"))
	journalTestClaim(t, relay)
	finishPrompt(relay, id)
	if journalTestJSON(t, relay.snapshot.OpenRequest()) != journalTestJSON(t, request) {
		t.Fatalf("open request = %+v", relay.snapshot.OpenRequest())
	}
	relay = settledClaudeRelay(t, dir)
	if journalTestJSON(t, relay.snapshot.OpenRequest()) != journalTestJSON(t, request) {
		t.Fatalf("open request after reopening = %+v", relay.snapshot.OpenRequest())
	}
	submitRelay(relay, "dismiss-original", core.RelayCommandDismissQuestion{Question: request.Question})
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("the dismissed request is still open")
	}
	relay = settledClaudeRelay(t, dir)
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("the dismissal did not survive reopening")
	}
	relay.setAcpReady(true)
	submitRelay(relay, "fresh-user-turn", prompt("Ask another question"))
	journalTestClaim(t, relay)
	finishPrompt(relay, "fresh-user-turn")
	assessment = verdictAssessment(t, relay)
	assessment.Status = core.StatusAssessed
	assessment.Action = verdictAction(core.AssessmentActionAwaitInput)
	verdictStore(t, relay, assessment)
	if relay.snapshot.OpenRequest() == nil {
		t.Fatal("a fresh question did not open a request")
	}
	submitRelay(relay, "person-answer", prompt("Keep the old format"))
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("the answer did not close the request")
	}
	if settledClaudeRelay(t, dir).snapshot.OpenRequest() != nil {
		t.Fatal("the answer did not survive reopening")
	}
}

func settledReply(t *testing.T, relay *DurableRelay, text string) {
	t.Helper()
	settledReplyWithID(t, relay, nil, text)
}

func settledReplyWithID(t *testing.T, relay *DurableRelay, id *string, text string) {
	t.Helper()
	chunk := &acpschema.ContentChunk{Content: journalTestTextBlock(text)}
	if id != nil {
		messageID := acpschema.MessageId(*id)
		chunk.MessageID = &messageID
	}
	journalTestSessionUpdate(t, relay, acpschema.SessionUpdate{Kind: acpschema.SessionUpdateKindAgentMessageChunk, AgentMessageChunk: chunk})
}

func settledAnswerElicitation(t *testing.T, relay *DurableRelay) {
	t.Helper()
	reply := "Yes"
	journalTestRecord(t, relay, core.RelayObservationElicitationResolved{ElicitationID: "form", Action: "accept", Reply: &reply})
}

func settledVisibleReplies(relay *DurableRelay) []string {
	var texts []string
	for _, message := range relay.snapshot.AuthorizationContext().Messages {
		runes := []rune(message.Text)
		texts = append(texts, string(runes[:min(len(runes), 24)]))
	}
	return texts
}

func settledReopen(t *testing.T, dir string) *DurableRelay {
	t.Helper()
	settledRemoveState(t, dir)
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	return relay
}

func settledID(id string) *string {
	return &id
}

// Hard-won: 4206e12f7: upgrading lost Important replies with no native message ID.
func TestNotificationStackReplaysAfterMailboxWakesAndFencesStaleClear(t *testing.T) {
	for _, replyID := range []*string{settledID("retained-reply"), nil} {
		checkNotificationStackAfterUpgrade(t, replyID)
	}
}

func checkNotificationStackAfterUpgrade(t *testing.T, replyID *string) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	run := func(relay *DurableRelay, id string, replyID *string) core.TurnAssessment {
		submitRelay(relay, id, prompt("Check results"))
		journalTestClaim(t, relay)
		settledReplyWithID(t, relay, replyID, "Turn result")
		finishPrompt(relay, id)
		assessment := verdictAssessment(t, relay)
		assessment.Status = core.StatusAssessed
		return assessment
	}
	important := run(relay, "first-person", replyID)
	attention := core.AssessmentAttentionImportant
	important.Attention = &attention
	important.Action = verdictAction(core.AssessmentActionFinished)
	verdictStore(t, relay, core.DeepClone(important))
	firstRevision := relay.snapshot.NotificationStack.Revision
	verdictStore(t, relay, core.DeepClone(important))
	if relay.snapshot.NotificationStack.Revision != firstRevision {
		t.Fatal("an identical assessment moved the notification revision")
	}

	ordinary := core.DeepClone(important)
	ordinary.Attention = nil
	verdictStore(t, relay, core.DeepClone(ordinary))
	if len(relay.snapshot.NotificationStack.Entries) != 0 {
		t.Fatalf("entries = %+v", relay.snapshot.NotificationStack.Entries)
	}
	withdrawnRevision := relay.snapshot.NotificationStack.Revision
	if withdrawnRevision != relay.snapshot.LatestOrdinal || withdrawnRevision <= firstRevision {
		t.Fatalf("withdrawn revision %d", withdrawnRevision)
	}
	verdictStore(t, relay, ordinary)
	if relay.snapshot.NotificationStack.Revision != withdrawnRevision {
		t.Fatal("an identical withdrawal moved the notification revision")
	}
	verdictStore(t, relay, core.DeepClone(important))

	legacy := snapshotTestDecode[jsonvalue.Object](t, journalTestJSON(t, relay.snapshot))
	legacy.Set("format_version", json.Number("26"))
	legacy.Delete("notification_stack")
	legacy.Delete("notifications_read_through")
	journalTestWriteFile(t, filepath.Join(dir, core.RelayStateFile), []byte(journalTestJSON(t, legacy)))
	relay = settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	var expected core.EntryMessage = core.EntryMessageNone{}
	if replyID != nil {
		expected = core.EntryMessageFound{Value: "agent:" + *replyID}
	}
	if message := relay.snapshot.NotificationStack.Entries[0].Message; !reflect.DeepEqual(message, expected) {
		t.Fatalf("upgraded entry message = %#v, want %#v", message, expected)
	}
	generated := core.GeneratedPromptSessionMessageTurn
	question := run(relay, generated.CommandID("first-wake"), nil)
	question.Action = verdictAction(core.AssessmentActionAwaitInput)
	verdictStore(t, relay, core.DeepClone(question))
	ordinary = core.DeepClone(question)
	ordinary.Action = verdictAction(core.AssessmentActionFinished)
	verdictStore(t, relay, ordinary)
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("a finished turn left its request open")
	}
	if relay.snapshot.NotificationStack.Revision != relay.snapshot.LatestOrdinal {
		t.Fatal("the stack revision is not the latest ordinal")
	}
	if entries := relay.snapshot.NotificationStack.Entries; len(entries) != 1 || entries[0].CompletedOrdinal != important.CompletedOrdinal {
		t.Fatalf("entries = %+v", entries)
	}
	finish := func(completed uint64) bool {
		t.Helper()
		finished, err := relay.finishQuestionLocator(completed, nil)
		if err != nil {
			t.Fatal(err)
		}
		return finished
	}
	if finish(question.CompletedOrdinal) {
		t.Fatal("a locator finished for a turn without an entry")
	}
	verdictStore(t, relay, core.DeepClone(question))
	if !finish(question.CompletedOrdinal) {
		t.Fatal("the locator did not finish")
	}
	if important.CompletedAtMs <= 0 || question.CompletedAtMs <= 0 {
		t.Fatal("a completion has no time")
	}
	var times []int64
	for _, entry := range relay.snapshot.NotificationStack.Entries {
		times = append(times, entry.CompletedAtMs)
	}
	if !slices.Equal(times, []int64{important.CompletedAtMs, question.CompletedAtMs}) {
		t.Fatalf("entry times = %v", times)
	}
	stack := core.DeepClone(relay.snapshot.NotificationStack)
	if len(stack.Entries) != 2 ||
		stack.Entries[0].Kind != core.NotificationKindImportant || !reflect.DeepEqual(stack.Entries[0].Message, expected) ||
		stack.Entries[1].Kind != core.NotificationKindNeedsInput || !reflect.DeepEqual(stack.Entries[1].Message, core.EntryMessage(core.EntryMessageNone{})) {
		t.Fatalf("stack = %+v", stack.Entries)
	}
	if journalTestJSON(t, settledReopen(t, dir).snapshot.NotificationStack) != journalTestJSON(t, stack) {
		t.Fatal("replay changed the notification stack")
	}
	relay = settledClaudeRelay(t, dir)
	relay.setAcpReady(true)

	// Clear stays available during an automated turn, but a gesture captured
	// before a later entry was appended cannot clear it.
	commandID := generated.CommandID("second-wake")
	submitRelay(relay, commandID, prompt("Continue"))
	journalTestClaim(t, relay)
	result := relay.Handle(relayRequest("stale-clear", core.RelayRequestSubmit{
		CommandID: "stale-clear",
		Command:   core.RelayCommandClearNotifications{Revision: firstRevision},
	}))
	if _, failed := result.Body.(core.RelayResponseBodyError); !failed {
		t.Fatalf("stale clear = %#v", result.Body)
	}
	for _, read := range []struct {
		id      string
		through uint64
	}{{"new-read", question.CompletedOrdinal}, {"old-read", important.CompletedOrdinal}} {
		submitRelay(relay, read.id, core.RelayCommandNotificationsRead{Through: read.through})
	}
	if len(relay.snapshot.NotificationStack.Entries) != 1 {
		t.Fatalf("entries = %+v", relay.snapshot.NotificationStack.Entries)
	}
	if relay.snapshot.NotificationsReadThrough != question.CompletedOrdinal {
		t.Fatalf("read through %d", relay.snapshot.NotificationsReadThrough)
	}
	clear := core.RelayCommandClearNotifications{Revision: relay.snapshot.NotificationStack.Revision}
	accepted := submitRelay(relay, "current-clear", clear)
	if repeated := submitRelay(relay, "current-clear", clear); repeated != accepted {
		t.Fatalf("repeated clear accepted at %d, want %d", repeated, accepted)
	}
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("a request is open after the clear")
	}
	if relay.snapshot.ActivePrompt == nil {
		t.Fatal("the clear ended the automated turn")
	}
	relay = settledReopen(t, dir)
	if len(relay.snapshot.NotificationStack.Entries) != 0 || relay.snapshot.OpenRequest() != nil {
		t.Fatal("the clear did not survive replay")
	}
}

func TestDismissedQuestionsSurviveJournalReplayAndRejectStaleRequests(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	journalTestSessionConfigured(t, relay)
	submitRelay(relay, "first-turn", prompt("Choose a target."))
	journalTestClaim(t, relay)
	settledReply(t, relay, "Which target: A or B?")
	finishPrompt(relay, "first-turn")
	verdictPrepare(t, relay)
	revision := verdictPending(t, relay).Generation
	verdictApplyGlide(t, relay, revision, glideAnswer("await_input", nil))
	question := *relay.snapshot.Assessment.InferredQuestion()
	messages := core.DeepClone(relay.snapshot.AssessmentContext.Messages)
	if count := len(relay.snapshot.AuthorizationContext().Messages); count != 2 {
		t.Fatalf("%d authorization messages", count)
	}
	command := core.RelayCommandDismissQuestion{Question: question}
	accepted := submitRelay(relay, "dismiss-first", command)
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("the dismissed request is still open")
	}
	if repeated := submitRelay(relay, "dismiss-first", command); repeated != accepted {
		t.Fatalf("repeated dismissal accepted at %d, want %d", repeated, accepted)
	}
	if claimed := journalTestClaim(t, relay); len(claimed) != 0 {
		t.Fatalf("dismissal never reaches the harness: %+v", claimed)
	}
	if !reflect.DeepEqual(relay.snapshot.AssessmentContext.Messages, messages) {
		t.Fatal("dismissal grants no new authorization and the stored history stays as written")
	}
	setAside := func(relay *DurableRelay) []string {
		var texts []string
		for _, message := range relay.snapshot.AuthorizationContext().Messages {
			texts = append(texts, message.Text)
		}
		return texts
	}
	if texts := setAside(relay); !slices.Equal(texts, []string{"Choose a target."}) {
		t.Fatalf("authorization texts = %q", texts)
	}
	// Late classification can finish without reopening the answered request.
	verdictApplyGlide(t, relay, revision, glideAnswer("await_input", nil))
	if relay.OperationalState().OpenRequest != nil {
		t.Fatal("a late classification reopened the dismissed request")
	}

	// Force reconstruction from the journal, rather than merely reading the
	// latest snapshot, to exercise serialization and replay together.
	settledRemoveState(t, dir)
	relay = settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("replay reopened the dismissed request")
	}
	verdictPrepare(t, relay)
	verdictNonePending(t, relay)
	if repeated := submitRelay(relay, "dismiss-first", command); repeated != accepted {
		t.Fatalf("replayed dismissal accepted at %d, want %d", repeated, accepted)
	}
	// Replay rebuilt the fence.
	if texts := setAside(relay); !slices.Equal(texts, []string{"Choose a target."}) {
		t.Fatalf("authorization texts after replay = %q", texts)
	}

	// A generated prompt (a mailbox wake) is not the person's message: it
	// leaves the fence in force, and the reply it produces is new.
	submitRelay(relay, "mailbox-wake-1", prompt("[mailbox] wake"))
	journalTestClaim(t, relay)
	settledReply(t, relay, "Background work finished.")
	finishPrompt(relay, "mailbox-wake-1")
	if texts := setAside(relay); !slices.Equal(texts, []string{"Choose a target.", "Background work finished."}) {
		t.Fatalf("authorization texts after the wake = %q", texts)
	}

	submitRelay(relay, "second-turn", prompt("What about the other target?"))
	journalTestClaim(t, relay)
	finishPrompt(relay, "second-turn")
	verdictPrepare(t, relay)
	revision = verdictPending(t, relay).Generation
	verdictApplyGlide(t, relay, revision, glideAnswer("await_input", nil))
	next := *relay.snapshot.Assessment.InferredQuestion()
	if question == next {
		t.Fatal("the new question is the dismissed one")
	}
	if texts := setAside(relay); !slices.Equal(texts, []string{
		"Choose a target.",
		"Which target: A or B?",
		"Background work finished.",
		"What about the other target?",
	}) {
		t.Fatalf("a person message ends the fence's effect: older replies are history again: %q", texts)
	}
	response := relay.Handle(relayRequest("stale-dismissal", core.RelayRequestSubmit{CommandID: "delayed-dismissal", Command: command}))
	if _, failed := response.Body.(core.RelayResponseBodyError); !failed {
		t.Fatalf("%#v", response)
	}
	if current := relay.snapshot.Assessment.InferredQuestion(); current == nil || *current != next {
		t.Fatalf("inferred question = %+v", current)
	}
	journalTestRecord(t, relay, core.RelayObservationElicitationRequested{Request: core.ElicitationRequest{
		ID:      "permission",
		Message: "May I deploy?",
		Fields:  []core.ElicitationField{},
	}})
	response = relay.Handle(relayRequest("form-owns-input", core.RelayRequestSubmit{
		CommandID: "dismiss-form",
		Command:   core.RelayCommandDismissQuestion{Question: next},
	}))
	if _, failed := response.Body.(core.RelayResponseBodyError); !failed {
		t.Fatalf("%#v", response)
	}
	if !slices.Contains(relay.snapshot.AssessmentQuestions, "permission") {
		t.Fatalf("assessment questions = %v", relay.snapshot.AssessmentQuestions)
	}
}

func TestAnAnsweredElicitationSetsAsideEarlierRepliesAcrossReplay(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	submitRelay(relay, "deploy-turn", prompt("Deploy it."))
	journalTestClaim(t, relay)
	// The answer arrives mid-turn, and the chunks carry no message id.
	settledReply(t, relay, "Should I deploy?")
	reply := "Yes"
	journalTestRecord(t, relay, core.RelayObservationElicitationResolved{ElicitationID: "form", Action: "accept", Reply: &reply})
	settledReply(t, relay, "Deployed.")
	finishPrompt(relay, "deploy-turn")
	settledRemoveState(t, dir)
	relay = settledClaudeRelay(t, dir)
	var texts []string
	for _, message := range relay.snapshot.AuthorizationContext().Messages {
		texts = append(texts, message.Text)
	}
	if !slices.Equal(texts, []string{"Deploy it.", "Deployed."}) {
		t.Fatalf("authorization texts = %q", texts)
	}

	// A turn that ends with no text after the answer still names its reply
	// as the question message.
	dir = t.TempDir()
	relay = settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	submitRelay(relay, "silent-turn", prompt("Deploy it."))
	journalTestClaim(t, relay)
	settledReplyWithID(t, relay, settledID("m-ask"), "Should I deploy?")
	settledAnswerElicitation(t, relay)
	finishPrompt(relay, "silent-turn")
	if replies := settledVisibleReplies(relay); !slices.Equal(replies, []string{"Deploy it."}) {
		t.Fatalf("visible replies = %q", replies)
	}
	if message := relay.snapshot.CompletedReplyMessage; message == nil || *message != "agent:m-ask" {
		t.Fatalf("completed reply message = %v", message)
	}

	// A harness that reuses one message id before and after the answer: the
	// later entry shares the id, and a dismissal sets both aside.
	dir = t.TempDir()
	relay = settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	journalTestSessionConfigured(t, relay)
	submitRelay(relay, "reuse-turn", prompt("Deploy it."))
	journalTestClaim(t, relay)
	settledReplyWithID(t, relay, settledID("m1"), "Should I deploy?")
	settledAnswerElicitation(t, relay)
	settledReplyWithID(t, relay, settledID("m1"), "Deployed. Want me to also tag it?")
	if replies := settledVisibleReplies(relay); !slices.Equal(replies, []string{"Deploy it.", "Deployed. Want me to als"}) {
		t.Fatalf("the answer set aside only the question: %q", replies)
	}
	finishPrompt(relay, "reuse-turn")
	verdictPrepare(t, relay)
	verdictApplyGlide(t, relay, verdictPending(t, relay).Generation, glideAnswer("await_input", nil))
	question := *relay.snapshot.Assessment.InferredQuestion()
	submitRelay(relay, "dismiss-reuse", core.RelayCommandDismissQuestion{Question: question})
	if replies := settledVisibleReplies(relay); !slices.Equal(replies, []string{"Deploy it."}) {
		t.Fatalf("visible replies after the dismissal = %q", replies)
	}
	settledReplyWithID(t, relay, settledID("m1"), "Later news.")
	relay = settledReopen(t, dir)
	if replies := settledVisibleReplies(relay); !slices.Equal(replies, []string{"Deploy it.", "Later news."}) {
		t.Fatalf("visible replies after replay = %q", replies)
	}

	// Eviction after the fence is set keeps it on the same entries.
	dir = t.TempDir()
	relay = settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	submitRelay(relay, "evict-turn", prompt("Deploy it."))
	journalTestClaim(t, relay)
	block := func(letter string) string { return strings.Repeat(letter, 5000) }
	settledReplyWithID(t, relay, settledID("a"), block("A"))
	settledReplyWithID(t, relay, settledID("b"), block("B"))
	settledAnswerElicitation(t, relay)
	settledReplyWithID(t, relay, settledID("c"), block("C"))
	settledReplyWithID(t, relay, settledID("d"), block("D"))
	// 20,000 bytes exceed the 16 KiB budget: A was evicted, B stays stored.
	stored := func(relay *DurableRelay) string {
		var letters strings.Builder
		for _, message := range relay.snapshot.AssessmentContext.Messages {
			if message.Role == "assistant" && message.Text != "" {
				letters.WriteString(message.Text[:1])
			}
		}
		return letters.String()
	}
	if got := stored(relay); got != "BCD" {
		t.Fatalf("stored replies %q", got)
	}
	letters := func(relay *DurableRelay) string {
		var letters strings.Builder
		for _, text := range settledVisibleReplies(relay) {
			if !strings.HasPrefix(text, "Deploy") && text != "" {
				letters.WriteString(text[:1])
			}
		}
		return letters.String()
	}
	if got := letters(relay); got != "CD" {
		t.Fatalf("visible replies %q", got)
	}
	settledReplyWithID(t, relay, settledID("e"), block("E"))
	// E's arrival evicts B, the last set-aside entry; the fence reaches 0.
	if got := stored(relay); got != "CDE" {
		t.Fatalf("stored replies %q", got)
	}
	if got := letters(relay); got != "CDE" {
		t.Fatalf("visible replies %q", got)
	}
	relay = settledReopen(t, dir)
	if got := stored(relay); got != "CDE" {
		t.Fatalf("stored replies after replay %q", got)
	}
	if got := letters(relay); got != "CDE" {
		t.Fatalf("visible replies after replay %q", got)
	}
}

func settledTask(id string) AirTaskFixture {
	return AirTaskFixture{TaskID: id, Description: fmt.Sprintf("cargo test (%s)", id)}
}

// Hard-won: 295a3f85: a completed Claude task looked idle during the gap before its task-notification turn.
func TestACompletedTaskKeepsTheSessionBusyUntilItsTurnOpens(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("a"), settledTask("b")})
	if relay.ActivityFacts().TaskSettledAtMs != nil {
		t.Fatal("a settle was recorded before any task ended")
	}

	// The adapter's edge update for a completed task.
	verdictAirTaskChanged(t, relay, core.AirTaskUpdateFinished{TaskID: "a", Settled: true, Failed: false})
	facts := relay.ActivityFacts()
	if facts.TaskSettledAtMs == nil {
		t.Fatal("settle recorded")
	}
	settled := *facts.TaskSettledAtMs
	if !core.TurnImminent(&facts, settled+1_000) {
		t.Fatal("a settled task does not predict a turn")
	}
	if published := relay.OperationalState().TaskSettledAtMs; published == nil || *published != settled {
		t.Fatalf("published settle = %v", published)
	}
	if verdictStateOf(relay).IsQuiet() {
		t.Fatal("a settled task is a turn about to start")
	}

	// The notification turn opens: the settle has done its job, and the turn
	// itself is now the reason the session is busy.
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnStarted{StartedAtMs: settled + 2_000})
	if relay.ActivityFacts().TaskSettledAtMs != nil {
		t.Fatal("the settle survived the turn that answers it")
	}
	if relay.ActivityFacts().HarnessTurnStartedAtMs == nil {
		t.Fatal("the notification turn has no start")
	}
}

// Hard-won: agentclientprotocol/claude-agent-acp#1390: an autonomous turn's task end predicted a notification turn for 60 s.
func TestAnAutonomousTurnTaskDoesNotSettleOrOfferStop(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	relay.setAcpReady(true)
	relay.snapshot.NativeSessionID = settledID("root")

	// claude-agent-acp 0.97.0's autonomousTurnStarted payload canStop=true
	// although this synthetic turn is not background work.
	if err := relay.applyClaudeTaskFixture("autonomous-turn-uuid", true, true, false, false); err != nil {
		t.Fatal(err)
	}
	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("background commands = %+v", commands)
	}
	if _, err := relay.BackgroundTaskStopTarget("air:4:root:autonomous-turn-uuid"); err == nil {
		t.Fatal("an autonomous turn offers a stop")
	}

	// autonomousTurnEnded is an ordinary terminal update without taskType.
	if err := relay.applyClaudeTaskFixture("autonomous-turn-uuid", false, false, true, false); err != nil {
		t.Fatal(err)
	}
	if relay.ActivityFacts().TaskSettledAtMs != nil {
		t.Fatal("an autonomous turn's end predicted a notification turn")
	}
	if !verdictStateOf(relay).IsQuiet() {
		t.Fatal("the session is not quiet after the autonomous turn")
	}
	if _, err := relay.BackgroundTaskStopTarget("air:4:root:autonomous-turn-uuid"); err == nil {
		t.Fatal("an ended autonomous turn offers a stop")
	}
}

func settledFinishedTurnWithServer(t *testing.T, relay *DurableRelay) {
	t.Helper()
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnStarted{StartedAtMs: 1})
	journalTestSessionUpdate(t, relay, verdictUpdateFromJSON(t,
		`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Done; the dev server stays up for you."}}`))
	verdictSettle(t, relay, "test")
}

func TestAConfidentUnneededJudgmentReleasesTheLeftoverTasksUntilTheEvidenceMoves(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	// A configured harness session; otherwise nothing is ever quiet.
	relay.setAcpReady(true)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("srv")})
	settledFinishedTurnWithServer(t, relay)
	verdictPrepare(t, relay)
	pending := verdictPending(t, relay)
	if len(pending.Evidence.Background) != 1 {
		t.Fatalf("the leftover task is named in the evidence: %+v", pending.Evidence.Background)
	}
	if !strings.Contains(pending.Evidence.Background[0].Command, "srv") {
		t.Fatalf("background evidence = %+v", pending.Evidence.Background[0])
	}
	before := relay.ActivityFacts()
	if !core.DriverPresent(&before) {
		t.Fatal("the leftover task does not drive the session")
	}
	if reason := verdictStateOf(relay).Quiet().Reason(); reason != "background work" {
		t.Fatalf("quiet reason %q", reason)
	}

	verdictApplyGlide(t, relay, pending.Generation, glideAnswer("finished", &[3]float64{0.0, 1.0, 0.0}))
	after := relay.ActivityFacts()
	if after.BackgroundNeeded == nil || *after.BackgroundNeeded {
		t.Fatalf("background needed = %v", after.BackgroundNeeded)
	}
	if after.BackgroundCommands != 1 {
		t.Fatalf("the task list itself is untouched: %d", after.BackgroundCommands)
	}
	if core.DriverPresent(&after) {
		t.Fatal("an unneeded task still drives the session")
	}
	quiet := verdictStateOf(relay).Quiet()
	if !quiet.IsYes() {
		t.Fatal(quiet.Reason())
	}
	if needed := relay.OperationalState().BackgroundNeeded; needed == nil || *needed {
		t.Fatalf("published background needed = %v", needed)
	}

	// New foreground work invalidates the judgment with the rest of the
	// inference.
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnStarted{StartedAtMs: 2})
	if relay.ActivityFacts().BackgroundNeeded != nil {
		t.Fatal("the judgment survived new foreground work")
	}
}

// #1202: a legacy (Jev-era) background judgment is process-local, so a
// restarted worker holds none. When the new harness reports leftover
// processes again (Kimi re-reads its task journal on attach), the worker asks
// once more instead of leaving the session busy for good. An unclear answer
// is asked again at the next restart, not within the same process.
// Hard-won: 9cb9d651: after restart, leftover processes kept `mj wait` blocked because Jev was never asked again.
// Hard-won: dc4c34617: a restored unclear GLiDE background answer was never re-asked after restart, so mj wait stayed blocked
func TestARestartedWorkerAsksAgainAboutLeftoverProcessesOnce(t *testing.T) {
	restart := func(root string) *DurableRelay {
		// What the worker records when it starts on an existing root.
		relay := settledClaudeRelay(t, root)
		journalTestRecord(t, relay, core.RelayObservationSessionRestarted{})
		relay.setAcpReady(true)
		return relay
	}
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("srv")})
	settledFinishedTurnWithServer(t, relay)
	verdictPrepare(t, relay)
	verdictApplyGlide(t, relay, verdictPending(t, relay).Generation, glideAnswer("finished", &[3]float64{0.0, 1.0, 0.0}))
	if !verdictStateOf(relay).Quiet().IsYes() {
		t.Fatal("the confident answer did not quiet the session")
	}
	// The answer was applied by this process: nothing more to ask.
	verdictPrepare(t, relay)
	verdictNonePending(t, relay)
	// Store the judgment in the legacy shape an older worker wrote: no GLiDE
	// distribution, so only this process remembers the background answer.
	legacy := verdictAssessment(t, relay)
	legacy.Glide = nil
	legacy.Attention = nil
	verdictStore(t, relay, legacy)

	relay = restart(dir)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("srv")})
	if relay.ActivityFacts().BackgroundNeeded != nil {
		t.Fatal("a restarted worker remembers a process-local judgment")
	}
	verdictPrepare(t, relay)
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("the restarted worker asks about the leftover process")
	}
	if len(pending.Evidence.Background) != 1 || !strings.Contains(pending.Evidence.Background[0].Command, "srv") {
		t.Fatalf("background evidence = %+v", pending.Evidence.Background)
	}
	// One request per restart: an unclear answer is not asked again.
	verdictApplyGlide(t, relay, pending.Generation, glideAnswer("finished", &[3]float64{0.0, 0.0, 1.0}))
	if relay.ActivityFacts().BackgroundNeeded != nil {
		t.Fatal("an unclear answer became a judgment")
	}
	for range 3 {
		verdictPrepare(t, relay)
		verdictNonePending(t, relay)
	}

	// The next restart asks once more, and a confident answer applies.
	relay = restart(dir)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("srv")})
	verdictPrepare(t, relay)
	verdictApplyGlide(t, relay, verdictPending(t, relay).Generation, glideAnswer("finished", &[3]float64{0.0, 1.0, 0.0}))
	if needed := relay.ActivityFacts().BackgroundNeeded; needed == nil || *needed {
		t.Fatalf("background needed = %v", needed)
	}
	quiet := verdictStateOf(relay).Quiet()
	if !quiet.IsYes() {
		t.Fatal(quiet.Reason())
	}
	verdictPrepare(t, relay)
	verdictNonePending(t, relay)
}

// Session 979dc, through the worker: prompt turn 20 ends while a build runs
// in the background and Jev says Wait; the build finishes, Claude Code runs
// and ends a turn of its own, and Jev says Uncertain about it. The assessment
// record holds each verdict for its own turn. The worker publishes no other
// copy of a verdict: daemons older than #1304 still read
// turn_completion.decision, and a Wait left there for turn 20 kept their
// waits open for good.
// Hard-won: #1304: the worker kept turn 20's Wait verdict after the session went idle.
func TestAVerdictStaysWithItsOwnTurnInThePublishedState(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	relay.setAcpReady(true)
	journalTestSessionConfigured(t, relay)
	submitRelay(relay, "api-turn-20", prompt("Build it and report."))
	journalTestClaim(t, relay)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("build")})
	journalTestSessionUpdate(t, relay, journalTestTextChunk("The build is running in the background."))
	journalTestComplete(t, relay, "api-turn-20", journalTestEndTurn())
	verdictPrepare(t, relay)
	if reason := verdictApplyGlide(t, relay, verdictPending(t, relay).Generation, glideAnswer("wait", nil)); reason != "background_work_pending" {
		t.Fatalf("reason %q", reason)
	}

	// The build finishes and Claude Code answers it with its own turn.
	verdictAirTaskChanged(t, relay, core.AirTaskUpdateFinished{TaskID: "build", Settled: true, Failed: false})
	verdictSetAirTasks(t, relay, nil)
	journalTestRecord(t, relay, core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()})
	journalTestSessionUpdate(t, relay, journalTestTextChunk("The build passed."))
	verdictSettle(t, relay, "end_turn")
	completion := core.DeepClone(relay.OperationalState().TurnCompletion)
	verdictPrepare(t, relay)
	pending := verdictPending(t, relay)
	if pending.Turn == nil || !strings.HasPrefix(pending.Turn.TurnID, "harness-turn-") {
		t.Fatalf("Jev is asked about the harness's own turn: %+v", pending.Turn)
	}
	if reason := verdictApplyGlide(t, relay, pending.Generation, glideAnswer("uncertain", nil)); reason != "assessment_uncertain" {
		t.Fatalf("reason %q", reason)
	}

	state := relay.OperationalState()
	activity := state.ActivityState()
	if !activity.IsIdle() {
		t.Fatalf("%+v", activity)
	}
	assessment := state.Assessment
	if !strings.HasPrefix(assessment.TurnID, "harness-turn-") {
		t.Fatalf("assessment turn %s", assessment.TurnID)
	}
	if assessment.Action == nil || *assessment.Action != core.AssessmentActionUncertain {
		t.Fatalf("action = %v", assessment.Action)
	}
	if !reflect.DeepEqual(state.TurnCompletion, completion) {
		t.Fatalf("assessment keeps the harness settlement identity: %+v != %+v", state.TurnCompletion, completion)
	}
	if state.TurnCompletion.CommandID != assessment.TurnID {
		t.Fatalf("completion command %s, assessment turn %s", state.TurnCompletion.CommandID, assessment.TurnID)
	}
	if state.TurnCompletion.Decision != core.DecisionKeepCurrent {
		t.Fatalf("completion decision %v", state.TurnCompletion.Decision)
	}
}

func TestStoppedTasksDoNotAnticipateANotificationTurn(t *testing.T) {
	dir := t.TempDir()
	relay := settledClaudeRelay(t, dir)
	verdictSetAirTasks(t, relay, []AirTaskFixture{settledTask("a"), settledTask("b")})
	// A stop the user asked for is answered with a notice, not a turn.
	verdictAirTaskChanged(t, relay, core.AirTaskUpdateFinished{TaskID: "b", Settled: false, Failed: false})
	if relay.ActivityFacts().TaskSettledAtMs != nil {
		t.Fatal("a stopped task predicted a turn")
	}
	if count := len(relay.OperationalState().BackgroundCommands); count != 1 {
		t.Fatalf("%d background commands", count)
	}
	verdictAirTaskChanged(t, relay, core.AirTaskUpdateFinished{TaskID: "a", Settled: true, Failed: false})
	if relay.ActivityFacts().TaskSettledAtMs == nil {
		t.Fatal("a completed task did not predict a turn")
	}
	// A restart forgets it with the rest of the harness's processes.
	relay.forgetHarnessProcesses()
	if relay.ActivityFacts().TaskSettledAtMs != nil {
		t.Fatal("the settle survived forgetting the harness's processes")
	}
}
