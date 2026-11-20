// Port of the tests in mj-core/src/activity/question_locator.rs.

package core

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func locatorMessage(id, role, text string) EvidenceMessage {
	return EvidenceMessage{ID: id, Role: role, Text: text}
}

func locatorJSON(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("parse %s: %v", text, err)
	}
	return value
}

func TestBuilderUsesStableIDsAfterLastUserAndUTF8Boundaries(t *testing.T) {
	history := DefaultContextHistory()
	history.Messages = []EvidenceMessage{
		locatorMessage("agent:old", "assistant", "old"),
		locatorMessage("user:last", "user", strings.Repeat("🦀", 1200)),
		locatorMessage("agent:one", "assistant", strings.Repeat("é", 2000)),
		locatorMessage("tool:x", "tool", "ignored"),
		locatorMessage("agent:two", "assistant", "choice"),
	}
	request := Build(&history)
	if !reflect.DeepEqual(request.MessageIDs, []string{"agent:one", "agent:two"}) {
		t.Fatalf("message ids = %q", request.MessageIDs)
	}
	if len(request.State.LatestUserMessage) > 4096 {
		t.Fatalf("latest user message is %d bytes", len(request.State.LatestUserMessage))
	}
	if !strings.Contains(request.State.Replies[0].Text, " [...] ") {
		t.Fatal("first reply is not clipped")
	}
	if request.State.Replies[1].Reply != 1 {
		t.Fatalf("reply = %d", request.State.Replies[1].Reply)
	}
}

func TestLiveAndSeededMessageIDsResolveToTheSameTranscriptItem(t *testing.T) {
	live := DefaultContextHistory()
	liveID := "msg_011CgihPmwyh6hJGX2Azu5L5"
	live.Assistant(&liveID, 42, "Choose?")
	if got, ok := TranscriptStableID(live.Messages[0].ID); !ok || got != "agent:msg_011CgihPmwyh6hJGX2Azu5L5" {
		t.Fatalf("live = %q, %v", got, ok)
	}
	seeded := locatorMessage("agent:msg_011CgihPmwyh6hJGX2Azu5L5", "assistant", "Choose?")
	if got, ok := TranscriptStableID(seeded.ID); !ok || got != "agent:msg_011CgihPmwyh6hJGX2Azu5L5" {
		t.Fatalf("seeded = %q, %v", got, ok)
	}
	fallback := DefaultContextHistory()
	fallback.Assistant(nil, 42, "Choose?")
	if fallback.Messages[0].ID != "agent:42" {
		t.Fatalf("fallback id = %q", fallback.Messages[0].ID)
	}
	if got, ok := TranscriptStableID(fallback.Messages[0].ID); ok {
		t.Fatalf("fallback resolved to %q", got)
	}
}

func TestStateShrinksToWireBudgetAndRetainsNewestReplies(t *testing.T) {
	history := DefaultContextHistory()
	history.Messages = append(history.Messages, locatorMessage("user:u", "user", "task"))
	for i := 0; i < 40; i++ {
		history.Messages = append(history.Messages, locatorMessage(fmt.Sprintf("agent:%d", i), "assistant", strings.Repeat("\\", 6000)))
	}
	request := Build(&history)
	if len(request.MessageIDs) != 32 {
		t.Fatalf("message ids = %d", len(request.MessageIDs))
	}
	if request.MessageIDs[0] != "agent:8" {
		t.Fatalf("first id = %q", request.MessageIDs[0])
	}
	encoded, err := json.Marshal(request.State)
	if err != nil {
		t.Fatal(err)
	}
	if len(encoded) > stateBytes {
		t.Fatalf("state is %d bytes", len(encoded))
	}
	if len(request.State.Replies[0].Text) >= 3000 {
		t.Fatalf("first reply is %d bytes", len(request.State.Replies[0].Text))
	}
}

func TestShippedQuestionEqualsTheEvaluatedQuestion(t *testing.T) {
	// The held-out evaluation sent exactly this object for three replies
	// (/mnt/optane/tmp/glide-afk-locator-20261113/locator/glide/FROZEN/).
	data, err := os.ReadFile(filepath.Join("testdata", "activity_question_locator", "question_locator_glide_example_n3.json"))
	if err != nil {
		t.Fatal(err)
	}
	evaluated := locatorJSON(t, string(data))
	shipped := locatorJSON(t, string(QuestionLocatorQuestions(3)))
	if !jsonvalue.Equal(shipped, evaluated) {
		t.Fatalf("questions(3) = %s", QuestionLocatorQuestions(3))
	}
	criteria, _ := agentJSONPointer(locatorJSON(t, string(QuestionLocatorQuestions(32))), "request_reply", "criteria")
	if members, _ := agentJSONMembers(criteria); len(members) != 33 {
		t.Fatalf("criteria = %d", len(members))
	}
}

func TestFrozenRuleAppliesToRecordedGlideProbabilities(t *testing.T) {
	// Real GLiDE answers from the evaluation: the newest reply, `none`, and an older reply.
	cases := []struct {
		probabilities string
		count         int
		index         int
		found         bool
	}{
		{`{"r0":0.000844,"r1":0.000432,"r2":0.000856,"r3":0.908273,"none":0.089595}`, 4, 3, true},
		{`{"r0":0.000108,"r1":0.000107,"r2":0.000113,"r3":0.000131,"r4":0.001922,"none":0.997619}`, 5, 0, false},
		{`{"r0":0.010342,"r1":0.000115,"r2":0.00025,"r3":0.011042,"r4":0.974786,"r5":0.000364,"none":0.003101}`, 6, 4, true},
	}
	for _, c := range cases {
		index, found, err := Parse(locatorJSON(t, c.probabilities), c.count)
		if err != nil || index != c.index || found != c.found {
			t.Fatalf("%s: (%d, %v, %v)", c.probabilities, index, found, err)
		}
	}
}

func TestTiesGoToTheNewestReplyAndNoneNeedsAStrictlyHigherScore(t *testing.T) {
	if index, found, err := Parse(locatorJSON(t, `{"r0":0.4,"r1":0.4,"none":0.2}`), 2); err != nil || !found || index != 1 {
		t.Fatalf("tied replies: (%d, %v, %v)", index, found, err)
	}
	if index, found, err := Parse(locatorJSON(t, `{"r0":0.5,"r1":0.0,"none":0.5}`), 2); err != nil || !found || index != 0 {
		t.Fatalf("tied with none: (%d, %v, %v)", index, found, err)
	}
}

func TestMalformedProbabilityObjectsAreRefused(t *testing.T) {
	for _, c := range []struct {
		probabilities string
		count         int
	}{
		{`{"r0":0.6,"none":0.4}`, 2},
		{`{"r0":0.6,"r1":0.4}`, 1},
		{`{"r0":0.6,"r2":0.1,"none":0.3}`, 2},
		{`{"r0":0.6,"r01":0.1,"none":0.3}`, 2},
		{`{"r0":1.2,"r1":-0.1,"none":-0.1}`, 2},
		{`{"r0":0.6,"r1":0.3,"none":"0.1"}`, 2},
		{`{"r0":0.6,"r1":0.3,"none":0.2}`, 2},
		{`[0.6, 0.4]`, 1},
	} {
		if _, _, err := Parse(locatorJSON(t, c.probabilities), c.count); err == nil {
			t.Fatalf("%s was accepted", c.probabilities)
		}
	}
}

func TestAssessmentFieldRoundTripsAndOldShapeStillReads(t *testing.T) {
	assessment := TurnAssessmentPending("turn", 1, 0, CompletionEvidence{StopReason: "EndTurn"})
	old, err := json.Marshal(assessment)
	if err != nil {
		t.Fatal(err)
	}
	var oldFields map[string]json.RawMessage
	if err := json.Unmarshal(old, &oldFields); err != nil {
		t.Fatal(err)
	}
	if _, present := oldFields["question_message"]; present {
		t.Fatal("question_message is written while unset")
	}
	var restored TurnAssessment
	if err := json.Unmarshal(old, &restored); err != nil {
		t.Fatal(err)
	}
	if restored.QuestionMessage != nil {
		t.Fatalf("question message = %q", *restored.QuestionMessage)
	}
	reply := "agent:reply"
	assessment.QuestionMessage = &reply
	updated, err := json.Marshal(assessment)
	if err != nil {
		t.Fatal(err)
	}
	restored = TurnAssessment{}
	if err := json.Unmarshal(updated, &restored); err != nil {
		t.Fatal(err)
	}
	if restored.QuestionMessage == nil || *restored.QuestionMessage != "agent:reply" {
		t.Fatalf("question message = %v", restored.QuestionMessage)
	}
}
