// Port of the tests in mj-client/src/transcript.rs.

package client

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"slices"
	"strings"
	"testing"
	"unsafe"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// transcriptPretty is serde_json::to_string_pretty of a value whose JSON
// form is encoded.
func transcriptPretty(t *testing.T, encoded []byte) string {
	t.Helper()
	var out bytes.Buffer
	if err := json.Indent(&out, encoded, "", "  "); err != nil {
		t.Fatal(err)
	}
	return out.String()
}

func transcriptEncode(t *testing.T, v any) []byte {
	t.Helper()
	encoded, err := feedMarshalJSON(v)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func transcriptItem(position uint64, body core.TranscriptBody) *core.TranscriptItem {
	return &core.TranscriptItem{
		StableID:        fmt.Sprintf("item-%d", position),
		Position:        position,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body:            body,
	}
}

// transcriptMakeMut is Rust's Arc::make_mut on a transcript slot: the slot
// gets its own copy, so entries that still point at the old item do not see
// the change.
func transcriptMakeMut(slot **core.TranscriptItem) *core.TranscriptItem {
	copied := **slot
	*slot = &copied
	return &copied
}

func transcriptCall(t *testing.T, text string) core.OpaqueToolCall {
	t.Helper()
	return core.OpaqueToolCallFromValue(json.RawMessage(text))
}

func transcriptUserContent(text string) []json.RawMessage {
	encoded, _ := json.Marshal(map[string]string{"type": "text", "text": text})
	return []json.RawMessage{encoded}
}

func TestGoldenMailboxBrowserTranscript(t *testing.T) {
	// Use the actual delivery projection's golden text as browser input.
	delivery, err := os.ReadFile("../transcript/testdata/tests/golden/mailbox-delivery-transcript.txt")
	if err != nil {
		t.Fatal(err)
	}
	_, after, found := strings.Cut(string(delivery), "20: Mailbox events:\n")
	if !found {
		t.Fatal("no mailbox batch")
	}
	batch, _, found := strings.Cut(after, "\n21: ")
	if !found {
		t.Fatal("no end of the mailbox batch")
	}
	entry := core.ChatEntryPlain(2, core.ChatRoleSystem, fmt.Sprintf(
		"Mailbox events:\n%s\n  - Child task Check parser finished: failed\n    支持 Unicode and <script>literal text</script>.\n    - A report bullet stays in this event.",
		batch))
	browser := BrowserTranscript([]core.ChatEntry{entry}, 2, 0, nil)
	assertRestGolden(t, "mailbox-browser-transcript", transcriptPretty(t, transcriptEncode(t, browser)))
}

func TestGoldenDeliveredMessageBrowserHistory(t *testing.T) {
	session := core.MaterializedSessionEmpty("recipient")
	session.AppliedEventOrdinal = 10
	for _, message := range []struct{ key, sender, text string }{
		{"first", "Reviewer", "One finding.\nThe complete **second line**."},
		{"second", "Builder", "Build passed.\n支持 Unicode."},
	} {
		session.Transcript = append(session.Transcript, &core.TranscriptItem{
			StableID:        core.MessageItemID(message.key),
			Position:        10,
			CreatedAtMs:     1_000,
			LastChangedAtMs: 1_000,
			Body: core.TranscriptBodyMessage{Event: &core.MailboxEvent{
				Key:         message.key,
				Source:      "session_message",
				CreatedAtMs: 900,
				Body: core.MailboxEventBodySessionMessage{
					From: core.Sender{Type: core.SenderSession, ID: message.key + "-sender", Title: message.sender},
					Text: message.text,
				},
			}},
		})
	}
	for _, tool := range []struct {
		position uint64
		id       string
	}{{11, "tool-ask"}, {12, "tool-queued"}} {
		item := transcriptItem(tool.position, core.TranscriptBodyTool{
			Call: transcriptCall(t, fmt.Sprintf(`{"toolCallId":%q, "title":"cargo test", "status":"pending", "kind":"execute"}`, tool.id)),
		})
		item.StableID = transcript.ToolItemID(tool.id)
		session.Transcript = append(session.Transcript, item)
	}
	permission, err := core.ElicitationRequestFromACPParams("tool-permission-0", json.RawMessage(`{
		"mode": "form", "sessionId": "recipient", "message": "Run it?",
		"requestedSchema": {"type":"object", "required":["choice"], "properties":{
			"choice":{"type":"string", "oneOf":[{"const":"approved", "title":"Yes"}]}}}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	askID := "tool-ask"
	permission.ToolPermission = &core.ToolPermissionForm{Skip: true, ToolCallID: &askID}
	session.PendingElicitations = []core.ElicitationRequest{permission}
	session.AppliedEventOrdinal = 12
	projector := &BrowserTranscriptProjector{}
	browser := transcriptAssertProjection(t, projector, &session)
	session.AppliedEventOrdinal = 13
	unchanged := transcriptAssertProjection(t, projector, &session)
	if !transcriptEntriesEqual(unchanged.Entries, browser.Entries) {
		t.Fatal("a batch has independent publication cursors")
	}
	page := core.TranscriptHistoryPage{Items: slices.Clone(session.Transcript), Frontier: session.AppliedEventOrdinal}
	history := HistoryEntries(&page)
	combined := jsonvalue.Object{
		{Key: "live", Value: json.RawMessage(transcriptEncode(t, browser))},
		{Key: "earlier", Value: json.RawMessage(transcriptEncode(t, history))},
	}
	encoded, err := jsonvalue.Marshal(combined)
	if err != nil {
		t.Fatal(err)
	}
	assertRestGolden(t, "delivered-message-browser-history", transcriptPretty(t, encoded))

	session.PendingElicitations = nil
	closed := transcriptAssertProjection(t, projector, &session)
	session.PendingElicitations = append(session.PendingElicitations, permission)
	session.AppliedEventOrdinal = 14
	opened := transcriptAssertProjection(t, projector, &session)
	if opened.Entries[2].ToolStatus == nil || *opened.Entries[2].ToolStatus != ToolAwaitingApproval {
		t.Fatalf("tool status %v", opened.Entries[2].ToolStatus)
	}
	if opened.Entries[2].UpdatedSeq != 14 {
		t.Fatalf("updated seq %d", opened.Entries[2].UpdatedSeq)
	}
	if !transcriptEntryEqual(opened.Entries[3], closed.Entries[3]) {
		t.Fatal("the other tool row changed")
	}

	// A completed call can still have an open request. Warm publication
	// must split and rejoin the completed group without an item revision.
	for i := 2; i < len(session.Transcript); i++ {
		item := transcriptMakeMut(&session.Transcript[i])
		tool := item.Body.(core.TranscriptBodyTool)
		tool.Call.Edit(func(value *any) { settingsSet(value, "completed", "status") })
		item.Body = tool
	}
	session.PendingElicitations = nil
	grouped := transcriptAssertProjection(t, projector, &session)
	session.PendingElicitations = append(session.PendingElicitations, permission)
	opened = transcriptAssertProjection(t, projector, &session)
	if opened.PresentationKey == grouped.PresentationKey {
		t.Fatal("the presentation key did not change")
	}
	if !slices.ContainsFunc(opened.Entries, func(entry *BrowserTranscriptEntry) bool {
		return entry.ToolStatus != nil && *entry.ToolStatus == ToolAwaitingApproval
	}) {
		t.Fatal("no row awaits approval")
	}
	session.PendingElicitations = nil
	closed = transcriptAssertProjection(t, projector, &session)
	if closed.PresentationKey != grouped.PresentationKey {
		t.Fatal("the presentation key did not come back")
	}
	if !transcriptEntriesEqual(closed.Entries, grouped.Entries) {
		t.Fatal("the grouped rows did not come back")
	}

	// Full message content also survives the live browser's ordinary
	// tool-output byte/line budgets.
	item := transcriptMakeMut(&session.Transcript[0])
	message := item.Body.(core.TranscriptBodyMessage)
	event := *message.Event
	large := strings.Repeat("支持 Unicode and a complete line.\n", 2_500)
	event.Body = core.MailboxEventBodyParentMessage{Text: large}
	message.Event = &event
	item.Body = message
	browser = projector.Project(&session)
	if strings.Join(browser.Entries[0].Lines, "\n")+"\n" != large {
		t.Fatal("the full message did not survive")
	}
}

func transcriptEntryEqual(a, b *BrowserTranscriptEntry) bool {
	return sameRendering(a, b) && a.UpdatedSeq == b.UpdatedSeq
}

func transcriptEntriesEqual(a, b []*BrowserTranscriptEntry) bool {
	return slices.EqualFunc(a, b, transcriptEntryEqual)
}

func TestStandaloneTerminalOutputSanitizesAnsiAndOscInErrorAndOutput(t *testing.T) {
	session := core.MaterializedSessionEmpty("terminal-controls")
	session.AppliedEventOrdinal = 1
	exitCode := uint32(101)
	lineCount := uint64(1)
	firstError := "\x1b]0;error title\x07\x1b[31mcompile error\x1b[0m"
	session.Transcript = append(session.Transcript, transcriptItem(1, core.TranscriptBodyTerminalOutput{
		Record: core.TerminalOutputRecord{
			TerminalID: "terminal-controls",
			Output:     core.NewPackedText("\x1b[33mcompiler output\x1b[0m\x1b]0;output title\x1b\\"),
			ExitCode:   &exitCode,
		},
		OutputLineCount: &lineCount,
		FirstErrorLine:  &firstError,
	}))
	entries := MaterializedChatEntries(&session)
	if entries[0].Text != "compile error\ncompiler output\nexited 101" {
		t.Fatalf("text %q", entries[0].Text)
	}
}

func TestAFinishedToolCallEndsAtItsLastChange(t *testing.T) {
	tool := func(status string) BrowserTranscriptEntry {
		item := transcriptItem(2, core.TranscriptBodyTool{
			Call: transcriptCall(t, fmt.Sprintf(`{"toolCallId":"tool-2", "title":"cargo test", "status":%q, "kind":"execute"}`, status)),
		})
		item.CreatedAtMs = 1_000
		item.LastChangedAtMs = 91_000
		entry := MaterializedChatEntry(item, 2)
		return BrowserEntry(&entry)
	}
	for _, status := range []string{"completed", "failed"} {
		entry := tool(status)
		if entry.RecordedAtMs == nil || *entry.RecordedAtMs != 1_000 {
			t.Errorf("%s recorded %v", status, entry.RecordedAtMs)
		}
		if entry.ToolEndedAtMs == nil || *entry.ToolEndedAtMs != 91_000 {
			t.Errorf("%s ended %v", status, entry.ToolEndedAtMs)
		}
	}
	if ended := tool("in_progress").ToolEndedAtMs; ended != nil {
		t.Errorf("a running call ended at %d", *ended)
	}
}

// The viewer's "put back in composer" reads this flag instead of repeating
// the id rules.
// Hard-won: 8b08e8bc1: handback reminders and peer-session messages counted as user authorization
func TestBrowserMarksPromptsMjolnirSubmittedAsGenerated(t *testing.T) {
	user := func(stableID string) BrowserTranscriptEntry {
		item := transcriptItem(2, core.TranscriptBodyUser{Content: transcriptUserContent("Please also update the docs")})
		item.StableID = stableID
		entry := MaterializedChatEntry(item, 2)
		return BrowserEntry(&entry)
	}
	if user("user:prompt-1").Generated {
		t.Error("a typed prompt is generated")
	}
	for _, id := range []string{"user:handback-reminder-5", "user:hold-wake-5", "user:hold-give-up-5", "user:session-message-turn-r1"} {
		if !user(id).Generated {
			t.Errorf("%s is not generated", id)
		}
	}
}

func transcriptAssertProjection(t *testing.T, projector *BrowserTranscriptProjector, session *core.MaterializedSession) WebBrowserTranscript {
	t.Helper()
	cached := projector.Project(session)
	// Everything but the update cursors matches a fresh projection; the
	// cursors are what the projector carries between projections.
	withoutCursors := func(transcript WebBrowserTranscript) any {
		value, err := jsonvalue.Parse(transcriptEncode(t, transcript))
		if err != nil {
			t.Fatal(err)
		}
		entries, _ := settingsArray(settingsGet(value, "entries"))
		for i, entry := range entries {
			object := entry.(jsonvalue.Object)
			object.SwapDelete("updated_seq")
			entries[i] = object
		}
		return value
	}
	fresh := MaterializedBrowserTranscript(session)
	if !settingsEqual(withoutCursors(cached), withoutCursors(fresh)) {
		t.Fatalf("cached projection differs from a fresh one:\n%s\n%s", transcriptEncode(t, cached), transcriptEncode(t, fresh))
	}
	return cached
}

func TestBrowserPublicationSharesLargeRowsWithoutChangingCursors(t *testing.T) {
	session := core.MaterializedSessionEmpty("shared-publication")
	for position := uint64(1); position <= 8_000; position++ {
		session.Transcript = append(session.Transcript, transcriptItem(position, core.TranscriptBodyUser{
			Content: transcriptUserContent(fmt.Sprintf("Message %d: %s", position,
				strings.Repeat("realistic transcript text and Unicode 支持. ", 40))),
		}))
	}
	session.AppliedEventOrdinal = 8_000
	projector := &BrowserTranscriptProjector{}
	first := projector.Project(&session)
	if !bytes.Equal(transcriptEncode(t, first), transcriptEncode(t, MaterializedBrowserTranscript(&session))) {
		t.Fatal("the projector's first publication differs from a fresh one")
	}
	// The conversations map clones the returned transcript. Its rows and the
	// projector's cursor baseline must still have the same allocation.
	conversation := first
	conversation.Entries = slices.Clone(first.Entries)
	for _, entry := range conversation.Entries {
		if entry != projector.published[entry.Identity()] {
			t.Fatal("a published row is not shared")
		}
	}
	lineBytes := 0
	for _, entry := range first.Entries {
		for _, line := range entry.Lines {
			lineBytes += len(line)
		}
	}
	t.Logf("shared publication measurement: %d source items, %d published rows, %d line bytes; old two copies = %d line bytes, shared = %d",
		len(session.Transcript), len(first.Entries), lineBytes, lineBytes*2, lineBytes)
	snapshot := transcriptEncode(t, conversation)

	session.AppliedEventOrdinal++
	last := transcriptMakeMut(&session.Transcript[len(session.Transcript)-1])
	last.Body = core.TranscriptBodyUser{Content: transcriptUserContent("changed newest item")}
	second := projector.Project(&session)
	for _, entry := range second.Entries {
		if entry != projector.published[entry.Identity()] {
			t.Fatal("a published row is not shared")
		}
		if entry.ID == 8_000 {
			if entry.UpdatedSeq != 8_001 {
				t.Fatalf("changed row cursor %d", entry.UpdatedSeq)
			}
			continue
		}
		index := slices.IndexFunc(first.Entries, func(old *BrowserTranscriptEntry) bool { return old.ID == entry.ID })
		if entry.UpdatedSeq != first.Entries[index].UpdatedSeq {
			t.Fatalf("row %d cursor moved", entry.ID)
		}
	}
	// Later publication cannot mutate the previous conversation snapshot.
	if !bytes.Equal(transcriptEncode(t, conversation), snapshot) {
		t.Fatal("a later publication changed an earlier snapshot")
	}
}

// A browser polling with `after_seq` is sent an entry again only when that
// entry changed, for every item kind: here a tool call and a thought that
// did not change stay behind while a new message is sent.
// Hard-won: 8891aa9adda7: browser polls resent unchanged thought, tool, plan, and terminal entries every time
func TestABrowserIsSentOnlyTheEntriesThatChanged(t *testing.T) {
	session := core.MaterializedSessionEmpty("exact-cursors")
	projector := &BrowserTranscriptProjector{}
	session.Transcript = append(session.Transcript,
		transcriptItem(1, core.TranscriptBodyUser{Content: transcriptUserContent("first")}),
		transcriptItem(2, core.TranscriptBodyTool{
			Call: transcriptCall(t, `{"toolCallId":"tool-2", "title":"Read file", "status":"completed", "kind":"read"}`),
		}),
		transcriptItem(3, core.TranscriptBodyThought{
			Chunks: []json.RawMessage{json.RawMessage(`{"content": {"type": "text", "text": "pondering"}}`)},
		}),
	)
	session.AppliedEventOrdinal = 3
	first := transcriptAssertProjection(t, projector, &session)

	session.Transcript = append(session.Transcript,
		transcriptItem(4, core.TranscriptBodyUser{Content: transcriptUserContent("next")}))
	session.AppliedEventOrdinal = 4
	second := transcriptAssertProjection(t, projector, &session)
	sent := func(transcript WebBrowserTranscript, after uint64) []uint64 {
		var ids []uint64
		for _, entry := range transcript.Entries {
			if entry.UpdatedSeq > after {
				ids = append(ids, entry.ID)
			}
		}
		return ids
	}
	if got := sent(second, first.LatestSeq); !slices.Equal(got, []uint64{4}) {
		t.Fatalf("only the new message is sent again: %v", got)
	}

	// The tool call completing again with new output is a change.
	item := transcriptMakeMut(&session.Transcript[1])
	tool := item.Body.(core.TranscriptBodyTool)
	tool.Call.Edit(func(value *any) { settingsSet(value, "failed", "status") })
	item.Body = tool
	session.AppliedEventOrdinal = 5
	third := transcriptAssertProjection(t, projector, &session)
	if got := sent(third, second.LatestSeq); !slices.Equal(got, []uint64{2}) {
		t.Fatalf("the changed tool call is sent again: %v", got)
	}
}

// Hard-won: e13ed5c7c: parked records rebuilt unchanged entries and disappeared across prefix and tail
func TestParkedOutputPreservesEntryReuseAcrossPrefixAndTail(t *testing.T) {
	session := core.MaterializedSessionEmpty("parked-tail")
	session.Transcript = append(session.Transcript, transcriptItem(1, core.TranscriptBodyTool{
		Call:         transcriptCall(t, `{"toolCallId":"owner", "title":"Execute", "status":"completed", "kind":"execute"}`),
		TerminalRefs: []string{"parked"},
	}))
	for position := uint64(2); position <= 300; position++ {
		session.Transcript = append(session.Transcript, transcriptItem(position, core.TranscriptBodyUser{
			Content: transcriptUserContent(fmt.Sprintf("message %d", position)),
		}))
	}
	exitCode := uint32(0)
	session.Transcript = append(session.Transcript, transcriptItem(301, core.TranscriptBodyTerminalOutputOf(core.TerminalOutputRecord{
		TerminalID: "parked",
		Output:     core.NewPackedText("unique parked output"),
		ExitCode:   &exitCode,
	})))
	session.AppliedEventOrdinal = 301
	previous := MaterializedChatEntries(&session)
	pointer := unsafe.StringData(previous[1].Text)
	reused := MaterializedChatEntriesReusing(&session, 0, previous)
	if unsafe.StringData(reused[1].Text) != pointer {
		t.Fatal("an unchanged entry was rebuilt")
	}
	split := len(session.Transcript) - TailSeedItems
	entries := MaterializedPrefixEntries(session.Transcript[:split], 301)
	entries = append(entries, MaterializedChatEntriesReusing(&session, split, nil)...)
	count := 0
	for _, entry := range entries {
		count += strings.Count(entry.Text, "unique parked output")
		for _, content := range entry.ToolContent {
			count += strings.Count(content, "unique parked output")
		}
	}
	if count != 1 {
		t.Fatalf("parked output appears %d times", count)
	}
}

func TestAdvancingTranscriptWindowPreservesComputedToolDiffstats(t *testing.T) {
	session := core.MaterializedSessionEmpty("sliding-tail")
	for position := uint64(1); position <= 3; position++ {
		session.Transcript = append(session.Transcript, transcriptItem(position, core.TranscriptBodyTool{
			Call: transcriptCall(t, fmt.Sprintf(`{"toolCallId": "tool-%d", "title": "Edit file", "status": "completed", "kind": "edit"}`, position)),
		}))
	}
	session.AppliedEventOrdinal = 3
	diffstats := map[string][]string{"item-2": {"file.rs  +7 −2"}}
	previous := MaterializedChatEntriesWithDiffstats(&session, diffstats)
	session.Transcript = session.Transcript[1:]
	session.Transcript = append(session.Transcript, transcriptItem(4, core.TranscriptBodyUser{Content: transcriptUserContent("continue")}))
	session.AppliedEventOrdinal = 4
	reused := MaterializedChatEntriesReusing(&session, 0, previous)
	if !slices.Equal(reused[0].ToolDiffstats, diffstats["item-2"]) {
		t.Fatalf("diffstats %v", reused[0].ToolDiffstats)
	}
	if len(reused) != 3 || reused[2].StartSeq != 4 {
		t.Fatalf("entries %d, last %d", len(reused), reused[len(reused)-1].StartSeq)
	}
}

func TestIncrementalBrowserProjectionPreservesUpdatesCollapseAndHistoryReplacement(t *testing.T) {
	session := core.MaterializedSessionEmpty("cache-test")
	projector := &BrowserTranscriptProjector{}
	session.Transcript = append(session.Transcript,
		transcriptItem(1, core.TranscriptBodyUser{Content: transcriptUserContent("first")}))
	for position := uint64(2); position <= 3; position++ {
		session.Transcript = append(session.Transcript, transcriptItem(position, core.TranscriptBodyTool{
			Call: transcriptCall(t, fmt.Sprintf(`{"toolCallId":"tool-%d", "title":"Read file", "status":"in_progress", "kind":"read", "content":[{"type":"content", "content":{"type":"text", "text":%q}}]}`,
				position, strings.Repeat("x", 80_000))),
		}))
	}
	session.AppliedEventOrdinal = 3
	transcriptAssertProjection(t, projector, &session)
	// Same-millisecond updates still change content and collapse topology.
	for i := 1; i < len(session.Transcript); i++ {
		item := transcriptMakeMut(&session.Transcript[i])
		tool := item.Body.(core.TranscriptBodyTool)
		tool.Call.Edit(func(value *any) { settingsSet(value, "completed", "status") })
		item.Body = tool
	}
	session.AppliedEventOrdinal = 4
	transcriptAssertProjection(t, projector, &session)
	session.Transcript = append(session.Transcript,
		transcriptItem(5, core.TranscriptBodyUser{Content: transcriptUserContent("next")}))
	session.AppliedEventOrdinal = 5
	projected := transcriptAssertProjection(t, projector, &session)
	if !slices.ContainsFunc(projected.Entries, func(entry *BrowserTranscriptEntry) bool {
		return slices.Contains(entry.Lines, "next")
	}) {
		t.Fatal("the new message is missing")
	}
	// Integrity repair can replace content without changing timestamps or
	// IDs.
	transcriptMakeMut(&session.Transcript[0]).Body = core.TranscriptBodyUser{Content: transcriptUserContent("repaired")}
	transcriptAssertProjection(t, projector, &session)
	session.Transcript = session.Transcript[:1]
	session.AppliedEventOrdinal = 1
	transcriptAssertProjection(t, projector, &session)
}
