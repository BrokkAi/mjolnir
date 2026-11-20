// Port of the tests in mj-transcript/src/authorization.rs.

package transcript

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func authorizationSession(messages [][2]string) core.MaterializedSession {
	session := core.MaterializedSessionEmpty("isolated-authorization-evidence")
	for i, message := range messages {
		role, text := message[0], message[1]
		ordinal := uint64(i)
		var body core.TranscriptBody
		if role == "user" {
			body = core.TranscriptBodyUser{Content: []json.RawMessage{
				transcriptTestJSON(map[string]any{"type": "text", "text": text}),
			}}
		} else {
			body = core.TranscriptBodyAgent{Chunks: []json.RawMessage{
				transcriptTestJSON(map[string]any{"content": map[string]any{"type": "text", "text": text}}),
			}}
		}
		session.Transcript = append(session.Transcript, &core.TranscriptItem{
			StableID:                  fmt.Sprintf("%s:%d", role, i),
			Position:                  ordinal,
			LatestContentEventOrdinal: &ordinal,
			Body:                      body,
		})
	}
	return session
}

func TestEvidencePreservesAuthorizationAcrossThreeExchanges(t *testing.T) {
	s := authorizationSession([][2]string{
		{"user", "Implement the parser, add tests, and commit."},
		{"assistant", "I will use the existing parser interface."},
		{"user", "Also handle empty files."},
		{"assistant", "That fits the plan."},
		{"user", "Go ahead."},
		{"assistant", "Implemented with empty-file support. Shall I add tests?"},
	})
	e, err := Evidence(&s)
	if err != nil {
		t.Fatal(err)
	}
	if len(e.Messages) != 6 || e.Messages[0].Text != "Implement the parser, add tests, and commit." || e.AssistantHistoryOmitted {
		t.Fatalf("evidence %+v", e)
	}
}

func TestEvidenceKeepsTheNewestUserWindowAndMarksOmittedAssistantContext(t *testing.T) {
	// A pasted document is kept by its head and tail, as the worker keeps it.
	oversized := strings.Repeat("x", core.UserBytes+1)
	s := authorizationSession([][2]string{{"user", oversized}, {"assistant", "Done"}})
	e, err := Evidence(&s)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(e.Messages[0].Text, "bytes omitted from the middle") {
		t.Fatalf("first message %q", e.Messages[0].Text[:min(len(e.Messages[0].Text), 200)])
	}
	// A long session keeps the newest whole user messages that fit.
	var long [][2]string
	for i := range 40 {
		long = append(long, [2]string{"user", fmt.Sprintf("step %d %s", i, strings.Repeat("u", 1024))})
		long = append(long, [2]string{"assistant", fmt.Sprintf("done %d", i)})
	}
	s = authorizationSession(long)
	e, err = Evidence(&s)
	if err != nil {
		t.Fatal(err)
	}
	var users []core.EvidenceMessage
	for _, message := range e.Messages {
		if message.Role == "user" {
			users = append(users, message)
		}
	}
	if len(users) >= 40 || !strings.HasPrefix(users[len(users)-1].Text, "step 39 ") || e.Messages[0].Role != "user" {
		t.Fatalf("%d users, first role %q", len(users), e.Messages[0].Role)
	}
	longReply := strings.Repeat("x", core.AssistantBytes)
	s = authorizationSession([][2]string{
		{"user", "Implement and test"},
		{"assistant", longReply},
		{"user", "Continue"},
		{"assistant", "Implemented; tests remain"},
	})
	e, err = Evidence(&s)
	if err != nil {
		t.Fatal(err)
	}
	if !e.AssistantHistoryOmitted || len(e.Messages) != 3 {
		t.Fatalf("omitted %t, %d messages", e.AssistantHistoryOmitted, len(e.Messages))
	}
}

func TestEvidenceExcludesGeneratedInputAndPriorContext(t *testing.T) {
	s := authorizationSession([][2]string{
		{"user", "Old authorization"},
		{"user", "New request"},
		{"user", "Generated prompt"},
		{"assistant", "Finished"},
	})
	boundary := *s.Transcript[0]
	boundary.StableID = "context-cleared:1"
	s.Transcript[0] = &boundary
	generated := *s.Transcript[2]
	generated.StableID = "user:auto-continue-1-1"
	s.Transcript[2] = &generated
	e, err := Evidence(&s)
	if err != nil {
		t.Fatal(err)
	}
	if len(e.Messages) != 2 || e.Messages[0].Text != "New request" {
		t.Fatalf("evidence %+v", e)
	}
	image := *s.Transcript[1]
	image.Body = core.TranscriptBodyUser{Content: []json.RawMessage{json.RawMessage(`{"type":"image","data":"not-text"}`)}}
	s.Transcript[1] = &image
	if _, err := Evidence(&s); err == nil {
		t.Fatal("non-text context was accepted as authorization")
	}
}
