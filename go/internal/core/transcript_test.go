// Port of the tests in mj-core/src/transcript.rs.

package core

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

func transcriptTestTextChunk(text string, messageID *string) json.RawMessage {
	encodedText, _ := transcriptMarshal(text)
	if messageID == nil {
		return json.RawMessage(fmt.Sprintf(`{"content":{"type":"text","text":%s}}`, encodedText))
	}
	encodedID, _ := transcriptMarshal(*messageID)
	return json.RawMessage(fmt.Sprintf(`{"content":{"type":"text","text":%s},"messageId":%s}`, encodedText, encodedID))
}

func transcriptTestChunksEqual(t *testing.T, got, want []json.RawMessage) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("got %d chunks, want %d", len(got), len(want))
	}
	for i := range got {
		if !transcriptValueEqual(transcriptParse(got[i]), transcriptParse(want[i])) {
			t.Fatalf("chunk %d: got %s, want %s", i, got[i], want[i])
		}
	}
}

func transcriptTestSourcedUserEntry(stableID, text string) ChatEntry {
	entry := ChatEntryPlain(1, ChatRoleUser, text)
	encoded, _ := transcriptMarshal(text)
	entry.Source = TranscriptSource{Item: &TranscriptItem{
		StableID: stableID,
		Position: 1,
		Body: TranscriptBodyUser{Content: []json.RawMessage{
			json.RawMessage(`{"type":"text","text":` + string(encoded) + `}`),
		}},
	}}
	return entry
}

func TestOnlyMessagesTheUserWroteAreUserAuthored(t *testing.T) {
	for _, c := range [][2]string{
		{"user:prompt-7", "fix the build"},
		{"user:2", "a prompt recovered by position"},
		{"elicitation-reply:9", "Yes"},
	} {
		entry := transcriptTestSourcedUserEntry(c[0], c[1])
		if !entry.IsUserAuthored() {
			t.Fatal(c[0])
		}
	}
	projected := ChatEntryPlain(1, ChatRoleUser, "not yet projected")
	if !projected.IsUserAuthored() {
		t.Fatal("an unprojected prompt")
	}
	for _, c := range [][2]string{
		{"user:auto-continue-1-1", "Continue."},
		{"user:quota-retry-4", "Continue."},
		{"user:server-retry-5", "Continue."},
		{"user:review-forward-2", "Review findings"},
		// Hard-won: 8b08e8bc1: handback reminders and peer-session messages counted as user authorization
		{"user:handback-reminder-36", "Please also update the docs"},
		{"user:subagent-input-3", "A sub-agent finished."},
		{"user:session-message-turn-8", "Message from another session"},
		{"user:prompt-9", "[HARNESS NOTE: summarize the request]"},
		{"user:prompt-10", ParentWaitPromptText},
	} {
		entry := transcriptTestSourcedUserEntry(c[0], c[1])
		if entry.IsUserAuthored() {
			t.Fatal(c[0])
		}
	}
	reply := ChatEntryPlain(1, ChatRoleAgent, "a reply")
	if reply.IsUserAuthored() {
		t.Fatal("an agent reply")
	}
}

// Hard-won: ff088d42: A 15,000-chunk thought made daemon RSS grow from gigabytes of stored JSON into many gigabytes.
func TestPushContentChunkMergesAdjacentTextForTheSameMessageID(t *testing.T) {
	m1 := "m1"
	chunks := []json.RawMessage{transcriptTestTextChunk("x", &m1)}
	for i := 1; i < 15_001; i++ {
		PushContentChunk(&chunks, transcriptTestTextChunk("x", &m1))
	}
	transcriptTestChunksEqual(t, chunks, []json.RawMessage{transcriptTestTextChunk(strings.Repeat("x", 15_001), &m1)})
}

func TestPushContentChunkKeepsChunksFromDifferentMessageIdsApart(t *testing.T) {
	m1, m2 := "m1", "m2"
	image := json.RawMessage(`{"content": {"type": "image", "data": "abc", "mimeType": "image/png"}}`)
	chunks := []json.RawMessage{transcriptTestTextChunk("first", &m1)}
	for _, chunk := range []json.RawMessage{
		transcriptTestTextChunk(" more", &m1),
		transcriptTestTextChunk("second", &m2),
		transcriptTestTextChunk("third", nil),
		transcriptTestTextChunk(" more", nil),
		image,
		image,
		json.RawMessage(`{"content": {"type": "text", "text": "a"}, "meta": {"source": "one"}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "b"}, "meta": {"source": "two"}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "c"}, "meta": {"source": "two"}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "annotated", "annotations": {"audience": ["user"]}}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "plain"}}`),
	} {
		PushContentChunk(&chunks, chunk)
	}
	transcriptTestChunksEqual(t, chunks, []json.RawMessage{
		transcriptTestTextChunk("first more", &m1),
		transcriptTestTextChunk("second", &m2),
		transcriptTestTextChunk("third more", nil),
		image,
		image,
		json.RawMessage(`{"content": {"type": "text", "text": "a"}, "meta": {"source": "one"}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "bc"}, "meta": {"source": "two"}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "annotated", "annotations": {"audience": ["user"]}}}`),
		json.RawMessage(`{"content": {"type": "text", "text": "plain"}}`),
	})
}

// Hard-won: ff088d42: Stored 15,000-chunk transcript rows ballooned daemon RSS after restart.
func TestCoalesceContentChunksCollapsesRunsAndKeepsSegmentBoundaries(t *testing.T) {
	m1, m2 := "m1", "m2"
	image := json.RawMessage(`{"content": {"type": "image", "data": "x", "mimeType": "image/png"}}`)
	chunks := make([]json.RawMessage, 0, 15_004)
	for i := 0; i < 15_001; i++ {
		chunks = append(chunks, transcriptTestTextChunk("x", &m1))
	}
	chunks = append(chunks, transcriptTestTextChunk("next", &m2), transcriptTestTextChunk(" turn", &m2), image)
	CoalesceContentChunks(&chunks)
	transcriptTestChunksEqual(t, chunks, []json.RawMessage{
		transcriptTestTextChunk(strings.Repeat("x", 15_001), &m1),
		transcriptTestTextChunk("next turn", &m2),
		image,
	})
}
