// Port of the tests in mj-core/src/assessment.rs.

package core

import (
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
)

func assessmentJSON(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("parse %s: %v", text, err)
	}
	return value
}

func TestHandbackGlideDistributionBoundaryAndMalformedAnswers(t *testing.T) {
	parse := func(value map[string]any) (HandbackProbabilities, error) {
		return HandbackProbabilitiesParse(value)
	}
	// Unfinished at exactly 30% of unfinished+finished, stops at exactly .30.
	value := map[string]any{"unfinished": 0.18, "needs_parent_input": 0.1,
		"blocked": 0.1, "permitted_stop": 0.1, "finished": 0.42, "unclear": 0.1}
	probabilities, err := parse(value)
	if err != nil || !HandbackNeedsMoreWork(&probabilities) {
		t.Fatalf("boundary: %+v, %v", probabilities, err)
	}
	value["permitted_stop"] = 0.11
	value["unclear"] = 0.09
	probabilities, err = parse(value)
	if err != nil || HandbackNeedsMoreWork(&probabilities) {
		t.Fatalf("a stop category above the guard must not bounce: %v", err)
	}
	value["permitted_stop"] = 0.1
	value["unfinished"] = 0.17
	value["finished"] = 0.43
	probabilities, err = parse(value)
	if err != nil || HandbackNeedsMoreWork(&probabilities) {
		t.Fatalf("below the share: %v", err)
	}
	value["unfinished"] = -0.1
	if _, err := parse(value); err == nil {
		t.Fatal("a negative probability was accepted")
	}
	delete(value, "unfinished")
	if _, err := parse(value); err == nil {
		t.Fatal("a missing probability was accepted")
	}
}

func assessmentVerdict(failure AssessmentFailure, input AssessmentInput, work AssessmentWork) Verdict {
	return Verdict{
		Failure: Judgment[AssessmentFailure]{Choice: failure, Confidence: 0.99},
		Input:   Judgment[AssessmentInput]{Choice: input, Confidence: 0.99, Probabilities: map[string]float64{string(input): 1.0}},
		Work:    Judgment[AssessmentWork]{Choice: work, Confidence: 0.99, Probabilities: map[string]float64{string(work): 1.0}},
	}
}

func TestOldSavedAssessmentsKeepTheirActionWithoutInventingProbabilities(t *testing.T) {
	assessment := TurnAssessmentPending("old", 1, 0, CompletionEvidence{StopReason: "EndTurn"})
	verdict := assessmentVerdict(AssessmentFailureNone, AssessmentInputRequired, AssessmentWorkFinished)
	assessment.Verdict = &verdict
	action := AssessmentActionAwaitInput
	assessment.Action = &action
	assessment.Status = StatusAssessed
	encoded, err := json.Marshal(assessment)
	if err != nil {
		t.Fatal(err)
	}
	var saved map[string]any
	if err := json.Unmarshal(encoded, &saved); err != nil {
		t.Fatal(err)
	}
	for _, axis := range []string{"failure", "input", "work"} {
		delete(saved["verdict"].(map[string]any)[axis].(map[string]any), "probabilities")
	}
	encoded, err = json.Marshal(saved)
	if err != nil {
		t.Fatal(err)
	}
	var restored TurnAssessment
	if err := json.Unmarshal(encoded, &restored); err != nil {
		t.Fatal(err)
	}
	if restored.Action == nil || *restored.Action != AssessmentActionAwaitInput {
		t.Fatalf("action = %v", restored.Action)
	}
	if restored.NeedsClassification(0) {
		t.Fatal("a saved assessment needs classification")
	}
	if restored.Verdict.RequiresInput() {
		t.Fatal("a verdict without probabilities requires input")
	}
}

func TestAMissingBackgroundAnswerParsesAndABadOneFails(t *testing.T) {
	answers := map[string]any{
		"failure": assessmentJSON(t, `{"type": "choice", "choice": "none", "confidence": 0.99, "probabilities": {"none": 1.0, "transient_provider": 0.0, "quota": 0.0, "other": 0.0, "unclear": 0.0}}`),
		"input":   assessmentJSON(t, `{"type": "choice", "choice": "none", "confidence": 0.99, "probabilities": {"none": 1.0, "redundant_request": 0.0, "required": 0.0, "unclear": 0.0}}`),
		"work":    assessmentJSON(t, `{"type": "choice", "choice": "finished", "confidence": 0.99, "probabilities": {"finished": 1.0, "authorized_unfinished": 0.0, "waiting": 0.0, "unclear": 0.0}}`),
	}
	response := map[string]any{"answers": answers}
	verdict, err := VerdictParse(response)
	if err != nil || verdict.Background != nil {
		t.Fatalf("verdict = %+v, %v", verdict, err)
	}
	answers["background"] = assessmentJSON(t, `{"type": "choice", "choice": "unneeded", "confidence": 0.9, "probabilities": {"needed": 0.0, "unneeded": 1.0, "unclear": 0.0}}`)
	verdict, err = VerdictParse(response)
	if err != nil || verdict.Background == nil || verdict.Background.Choice != AssessmentBackgroundUnneeded {
		t.Fatalf("verdict = %+v, %v", verdict, err)
	}
	answers["background"] = assessmentJSON(t, `{"type": "choice", "choice": "maybe", "confidence": 0.9}`)
	if _, err := VerdictParse(response); err == nil {
		t.Fatal("a bad background answer was accepted")
	}
}

func TestReplyAnswerIsOptionalForFrozenContractsAndValidatedWhenPresent(t *testing.T) {
	answers := map[string]any{
		"failure": assessmentJSON(t, `{"type": "choice", "choice": "none", "confidence": 0.99, "probabilities": {"none": 1.0, "transient_provider": 0.0, "quota": 0.0, "other": 0.0, "unclear": 0.0}}`),
		"input":   assessmentJSON(t, `{"type": "choice", "choice": "none", "confidence": 0.99, "probabilities": {"none": 1.0, "redundant_request": 0.0, "required": 0.0, "unclear": 0.0}}`),
		"work":    assessmentJSON(t, `{"type": "choice", "choice": "authorized_unfinished", "confidence": 0.99, "probabilities": {"finished": 0.0, "authorized_unfinished": 1.0, "waiting": 0.0, "unclear": 0.0}}`),
	}
	response := map[string]any{"answers": answers}
	verdict, err := VerdictParse(response)
	if err != nil || verdict.Reply != nil {
		t.Fatalf("verdict = %+v, %v", verdict, err)
	}

	reply := map[string]any{
		"type": "choice", "choice": "closing", "confidence": 0.99,
		"probabilities": map[string]any{"closing": 1.0, "continuing": 0.0, "unclear": 0.0},
	}
	answers["reply"] = reply
	verdict, err = VerdictParse(response)
	if err != nil || verdict.Reply == nil || verdict.Reply.Choice != AssessmentReplyClosing {
		t.Fatalf("verdict = %+v, %v", verdict, err)
	}
	reply["probabilities"] = map[string]any{"closing": 0.8, "continuing": 0.2}
	if _, err := VerdictParse(response); err == nil {
		t.Fatal("an incomplete reply answer was accepted")
	}
}

func assessmentText(text string) []schema.ContentBlock {
	return []schema.ContentBlock{{Text: &schema.TextContent{Text: text}}}
}

func assessmentIDs(messages []EvidenceMessage) []string {
	ids := make([]string, 0, len(messages))
	for _, m := range messages {
		ids = append(ids, m.ID)
	}
	return ids
}

func TestGeneratedSubagentWaitPromptDoesNotEnterAuthorizationHistory(t *testing.T) {
	context := DefaultContextHistory()
	prompt := assessmentText(ParentWaitPromptText)
	context.User("subagent-wait-123", prompt)
	context.User("api-123", prompt)
	if len(context.Messages) != 0 || !context.AuthorizationComplete {
		t.Fatalf("context = %+v", context)
	}
	// Hard-won: 8b08e8bc1: handback reminders and peer-session messages counted as user authorization
	typed := assessmentText("Please also update the docs.")
	context.User(HandbackReminderCommandID(7), typed)
	context.User(GeneratedPromptSessionMessageTurn.CommandID("peer-1"), typed)
	if len(context.Messages) != 0 {
		t.Fatalf("messages = %q", assessmentIDs(context.Messages))
	}
	handback := DefaultContextHistory()
	for _, id := range []string{"subagent-spawn-first", "subagent-input-next"} {
		context.User(id, typed)
		handback.HandbackUser(id, typed)
	}
	for _, id := range []string{
		"subagent-wait-123",
		"handback-reminder-7",
		"session-message-turn-peer-1",
		"mailbox-wake-8",
		"auto-continue-9",
	} {
		handback.HandbackUser(id, typed)
	}
	if len(context.Messages) != 0 {
		t.Fatal("global authorization must still exclude assignments")
	}
	if got := assessmentIDs(handback.Messages); !reflect.DeepEqual(got, []string{"user:subagent-spawn-first", "user:subagent-input-next"}) {
		t.Fatalf("handback = %q", got)
	}
	// Hard-won: 97731d30a: a history stored before the upgrade kept a peer message as user authorization.
	written := `{"messages":[{"id":"user:prompt-1","role":"user","text":"Fix the build."},{"id":"agent:2","role":"assistant","text":"Fixed."},{"id":"user:session-message-turn-peer-1","role":"user","text":"Please also push it."},{"id":"agent:4","role":"assistant","text":"Pushing now."}],"authorization_complete":true,"assistant_history_omitted":false,"open_assistant_id":null,"final_reply_omitted":false}`
	var stored ContextHistory
	if err := json.Unmarshal([]byte(written), &stored); err != nil {
		t.Fatal(err)
	}
	// Journal digests cover the stored form, so reading it must not change it.
	encoded, err := json.Marshal(stored)
	if err != nil {
		t.Fatal(err)
	}
	if !jsonvalue.Equal(assessmentJSON(t, string(encoded)), assessmentJSON(t, written)) {
		t.Fatalf("stored = %s", encoded)
	}
	if !stored.HasGeneratedPrompts() {
		t.Fatal("the peer message is not recognized")
	}
	read := stored.ReaderView()
	if got := assessmentIDs(read.Messages); !reflect.DeepEqual(got, []string{"user:prompt-1", "agent:2", "agent:4"}) {
		t.Fatalf("reader view = %q", got)
	}
	if read.HasGeneratedPrompts() {
		t.Fatal("the reader view keeps a generated prompt")
	}
}

func TestAuthorizationSurvivesAssistantEvictionWithoutClippingUserConsent(t *testing.T) {
	context := DefaultContextHistory()
	prompt := assessmentText("Implement, test, and commit.")
	context.User("user-command", prompt)
	context.User("server-retry-10", prompt)
	for i := 0; i < 40; i++ {
		id := fmt.Sprintf("message-%d", i)
		context.Assistant(&id, uint64(i), strings.Repeat("x", 1024))
	}
	if !context.AuthorizationComplete || !context.AssistantHistoryOmitted {
		t.Fatalf("context = complete %v omitted %v", context.AuthorizationComplete, context.AssistantHistoryOmitted)
	}
	if context.userCount() != 1 {
		t.Fatalf("user messages = %d", context.userCount())
	}
	// One pasted document is kept by its head and tail, not refused.
	context.User("pasted", assessmentText(strings.Repeat("x", UserBytes+1)))
	if !context.AuthorizationComplete {
		t.Fatal("a pasted document made the history incomplete")
	}
	index := slices.IndexFunc(context.Messages, func(m EvidenceMessage) bool { return m.ID == "user:pasted" })
	if index < 0 {
		t.Fatal("the pasted document was not kept")
	}
	pasted := context.Messages[index]
	if len(pasted.Text) >= MessageHeadBytes+MessageTailBytes+100 || !strings.Contains(pasted.Text, "bytes omitted from the middle") {
		t.Fatalf("pasted is %d bytes", len(pasted.Text))
	}
	// A full user budget gives up the oldest exchanges, never the current
	// request, and the window of whole messages stays complete.
	for i := 0; i < 6; i++ {
		context.User(fmt.Sprintf("paste-%d", i), assessmentText(strings.Repeat("y", UserBytes)))
	}
	if !context.AuthorizationComplete {
		t.Fatal("history is incomplete")
	}
	if context.userBytes() > UserBytes {
		t.Fatalf("user bytes = %d", context.userBytes())
	}
	ids := assessmentIDs(context.Messages)
	if slices.Contains(ids, "user:user-command") || !slices.Contains(ids, "user:paste-5") {
		t.Fatalf("ids = %q", ids)
	}
}

// Hard-won: a893e12: recorded Codex goal fixtures failed after the previous total-entry cap rejected long sessions.
func TestLongSessionsEvictAssistantEntriesBeforeUserMessages(t *testing.T) {
	context := DefaultContextHistory()
	// Two hundred exchanges with ten assistant messages each: far past the
	// old 256-entry refusal, still complete.
	for i := 0; i < 200; i++ {
		context.User(fmt.Sprintf("u%d", i), assessmentText(fmt.Sprintf("step %d", i)))
		for j := 0; j < 10; j++ {
			id := fmt.Sprintf("a%d-%d", i, j)
			context.Assistant(&id, uint64(i*10+j), "ok")
		}
	}
	if !context.AuthorizationComplete {
		t.Fatal("hundreds of exchanges stay complete")
	}
	if len(context.Messages) > MaxMessages || !context.AssistantHistoryOmitted || context.FinalReplyOmitted {
		t.Fatalf("messages %d omitted %v final omitted %v", len(context.Messages), context.AssistantHistoryOmitted, context.FinalReplyOmitted)
	}
	if context.userCount() != 200 {
		t.Fatalf("user count = %d", context.userCount())
	}
	if !slices.Contains(assessmentIDs(context.Messages), "user:u0") {
		t.Fatal("user history within budget is never evicted")
	}
	if context.Messages[len(context.Messages)-1].Role != "assistant" {
		t.Fatal("last entry is not the reply")
	}
	// The 257th distinct user message displaces the oldest one.
	for i := 200; i < MaxMessages; i++ {
		context.User(fmt.Sprintf("u%d", i), assessmentText(fmt.Sprintf("step %d", i)))
	}
	if !context.AuthorizationComplete {
		t.Fatal("history is incomplete")
	}
	context.User("u-one-too-many", assessmentText("step 256"))
	if !context.AuthorizationComplete || context.userCount() != MaxMessages {
		t.Fatalf("complete %v user count %d", context.AuthorizationComplete, context.userCount())
	}
	if slices.Contains(assessmentIDs(context.Messages), "user:u0") {
		t.Fatal("the oldest user message was kept")
	}
	if last := context.Messages[len(context.Messages)-1].ID; last != "user:u-one-too-many" {
		t.Fatalf("last = %q", last)
	}
}

// Hard-won: a893e12: oversized assessment history was dropped instead of evicting old assistant entries to preserve the reply.
func TestShrinkingToAWireLimitDropsOldAssistantEntriesButKeepsTheReply(t *testing.T) {
	context := DefaultContextHistory()
	context.User("u", assessmentText("do the thing"))
	for i := 0; i < 12; i++ {
		id := fmt.Sprintf("a%d", i)
		context.Assistant(&id, uint64(i), strings.Repeat("z", 1000))
	}
	size := func(c *ContextHistory) int {
		encoded, err := json.Marshal(c)
		if err != nil {
			t.Fatal(err)
		}
		return len(encoded)
	}
	limit := 5_000
	if !context.ShrinkUntil(func(c *ContextHistory) bool { return size(c) <= limit }) {
		t.Fatal("shrink gave up")
	}
	if size(&context) > limit || !context.AssistantHistoryOmitted {
		t.Fatalf("size %d omitted %v", size(&context), context.AssistantHistoryOmitted)
	}
	if !slices.Contains(assessmentIDs(context.Messages), "user:u") {
		t.Fatal("the user message was dropped")
	}
	if last := context.Messages[len(context.Messages)-1].ID; last != "a11" {
		t.Fatalf("last = %q", last)
	}
	if context.FinalReplyOmitted {
		t.Fatal("final reply omitted")
	}
	evidence := context.Evidence()
	if err := evidence.Validate(); err != nil {
		t.Fatalf("validate: %v", err)
	}
	// Once only the user message and the final reply remain, it gives up.
	if context.ShrinkUntil(func(c *ContextHistory) bool { return size(c) <= 100 }) {
		t.Fatal("shrink claimed to fit 100 bytes")
	}
}

func TestALongFinalReplyIsTrimmedRatherThanOmitted(t *testing.T) {
	context := DefaultContextHistory()
	context.User("u", assessmentText("write the report"))
	reply := "reply"
	for chunk := 0; chunk < 40; chunk++ {
		context.Assistant(&reply, uint64(chunk), strings.Repeat("r", 1024))
	}
	if context.FinalReplyOmitted || !context.AuthorizationComplete {
		t.Fatalf("final omitted %v complete %v", context.FinalReplyOmitted, context.AuthorizationComplete)
	}
	last := context.Messages[len(context.Messages)-1]
	if last.Role != "assistant" || !strings.Contains(last.Text, "bytes omitted from the middle") {
		t.Fatalf("last = %+v", last)
	}
	evidence := context.Evidence()
	if err := evidence.Validate(); err != nil {
		t.Fatalf("validate: %v", err)
	}
}
