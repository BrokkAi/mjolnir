// Port of the tests in mj-transcript/src/github_item.rs.

package transcript

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func githubItemUserBody(text string) core.TranscriptBody {
	return core.TranscriptBodyUser{Content: []json.RawMessage{
		transcriptTestJSON(map[string]any{"type": "text", "text": text}),
	}}
}

func TestMentionsSurviveReferenceAndConversationTruncationWithUtf8(t *testing.T) {
	session := core.MaterializedSessionEmpty("mention-index")
	var pending []string
	for n := 2; n < 50; n++ {
		pending = append(pending, fmt.Sprintf("pending #%d", n))
	}
	text := fmt.Sprintf("pending #1\n%s\n%s", strings.Repeat("é", core.MessageBytes), strings.Join(pending, "\n"))
	session.Transcript = append(session.Transcript, &core.TranscriptItem{
		StableID: "user:1",
		Position: 1,
		Body:     githubItemUserBody(text),
	})
	digest := SessionDigest(&session)
	if slices.Contains(digest.References, "#1") {
		t.Fatalf("references %v", digest.References)
	}
	item := core.GithubItem{
		Repo:   "owner/repo",
		Kind:   core.GithubItemKindIssue,
		Number: 49,
		Title:  "Fix #1",
		URL:    "https://github.com/owner/repo/issues/49",
	}
	if mentions := digest.ForItem(&item, true).Mentions; !slices.Equal(mentions, []string{"pending #49", "pending #1"}) {
		t.Fatalf("mentions %v", mentions)
	}
	long := fmt.Sprintf("#123 first %s reviewing #12 next %s", strings.Repeat("é", 200), strings.Repeat("x", 400))
	at := -1
	for _, reference := range core.GithubReferences(long) {
		if reference.Number == 12 {
			at = reference.Start
			break
		}
	}
	window := mentionWindow(long, at, len("#12"))
	if len(window) > core.MentionBytes || !strings.Contains(window, "reviewing #12 next") {
		t.Fatalf("window %q", window)
	}
	// GitHub allows 39-byte owners and 100-byte repository names.
	url := fmt.Sprintf("https://github.com/%s/%s/pull/123456", strings.Repeat("o", 39), strings.Repeat("r", 100))
	long = fmt.Sprintf("%s %s %s", strings.Repeat("x", 300), url, strings.Repeat("y", 300))
	window = mentionWindow(long, strings.Index(long, url), len(url))
	if len(window) > core.MentionBytes || !strings.Contains(window, url) ||
		!strings.HasPrefix(window, mentionElision) || !strings.HasSuffix(window, mentionElision) {
		t.Fatalf("window %q", window)
	}
}

func TestDigestPreservesRecencyAndUtf8AcrossTurnBoundaries(t *testing.T) {
	session := core.MaterializedSessionEmpty("digest-test")
	bodies := []core.TranscriptBody{
		githubItemUserBody("old #1"),
		githubItemUserBody("first #2 owner/repo#3"),
		githubItemUserBody("second #2"),
		core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(json.RawMessage(`{"toolCallId":"t","title":"Read https://github.com/owner/repo/pull/4", "status":"completed","locations":[{"path":"src/lib.rs"}]}`)),
		},
		githubItemUserBody(strings.Repeat("x", core.MessageBytes-len(messageTruncationMarker)-1) + "🦀" + strings.Repeat("tail", 20)),
	}
	for i, body := range bodies {
		session.Transcript = append(session.Transcript, &core.TranscriptItem{
			StableID: fmt.Sprintf("item:%d", i),
			Position: uint64(i) + 1,
			Body:     body,
		})
	}
	digest := SessionDigest(&session)
	want := fmt.Sprintf("User: first #2 owner/repo#3\nUser: second #2\nUser: %s%s\n",
		strings.Repeat("x", core.MessageBytes-len(messageTruncationMarker)-1), messageTruncationMarker)
	if digest.Conversation != want {
		t.Fatalf("conversation %q", digest.Conversation)
	}
	if !slices.Equal(digest.Files, []string{"src/lib.rs"}) {
		t.Fatalf("files %v", digest.Files)
	}
	if !slices.Equal(digest.References, []string{"https://github.com/owner/repo/pull/4", "#2", "owner/repo#3"}) {
		t.Fatalf("references %v", digest.References)
	}
	last := *session.Transcript[len(session.Transcript)-1]
	last.StableID = core.ContextBoundaryPrefix + "clear"
	session.Transcript[len(session.Transcript)-1] = &last
	cleared := SessionDigest(&session)
	if cleared.Conversation != "" || len(cleared.Files) != 0 || len(cleared.References) != 0 {
		t.Fatalf("cleared digest %+v", cleared)
	}
}

func TestDigestKeepsPeerRelationshipsAndNewestMessagesWithinBudget(t *testing.T) {
	session := core.MaterializedSessionEmpty("digest-budget")
	add := func(stableID string, body core.TranscriptBody) {
		session.Transcript = append(session.Transcript, &core.TranscriptItem{
			StableID: stableID,
			Position: uint64(len(session.Transcript)) + 1,
			Body:     body,
		})
	}
	add("user:old", githubItemUserBody("discard before clear"))
	add("context-cleared:1", githubItemUserBody("/clear"))
	add("user:task", githubItemUserBody("Review the pending fix"))
	add("user:continuation", githubItemUserBody(core.Prompt))
	add("user:compact", githubItemUserBody(" /compact retain pending reviews"))
	// Enough assistant messages to exhaust even the largest replay budget.
	for i := range core.ConversationBytes/core.MessageBytes + 2 {
		add(fmt.Sprintf("agent:%d", i), core.TranscriptBodyAgent{Chunks: []json.RawMessage{
			transcriptTestJSON(map[string]any{"content": map[string]any{
				"type": "text", "text": fmt.Sprintf("reply %d: %s", i, strings.Repeat("é", core.MessageBytes)),
			}}),
		}})
	}
	add("user:peer", githubItemUserBody("Message from session reviewer: awaiting #42 final head"))
	peer := &core.MailboxEvent{
		Key:    "peer-review",
		Source: "peer",
		Body:   core.MailboxEventBodyParentMessage{Text: "The review for issue 42 is pending"},
	}
	peerText := core.RenderMailboxEvent(peer)
	add("message:peer-review", core.TranscriptBodyMessage{Event: peer})
	digest := SessionDigest(&session)
	if len(digest.Conversation) > core.ConversationBytes {
		t.Fatalf("conversation of %d bytes", len(digest.Conversation))
	}
	for _, excluded := range []string{"discard before clear", "Review the pending fix", core.Prompt, "/compact", "reply 0:"} {
		if strings.Contains(digest.Conversation, excluded) {
			t.Fatalf("conversation contains %q", excluded)
		}
	}
	if !strings.HasSuffix(digest.Conversation, "User: "+peerText+"\n") ||
		!strings.Contains(digest.Conversation, "User: Message from session reviewer: awaiting #42 final head\n") ||
		!strings.Contains(digest.Conversation, messageTruncationMarker) {
		t.Fatalf("conversation %q", digest.Conversation)
	}
	if !slices.Equal(digest.References, []string{"#42"}) {
		t.Fatalf("references %v", digest.References)
	}
}
