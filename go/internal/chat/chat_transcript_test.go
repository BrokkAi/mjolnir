// Tests of mj-chat/src/chat/transcript.rs (chat/transcript/tests.rs) and its
// test-only items. The tests are split over chat_transcript_test.go,
// chat_transcript_view_test.go and chat_transcript_golden_test.go in Rust's
// order.

package chat

import (
	"bytes"
	"encoding/json"
	"fmt"
	"image/color"
	"math"
	"reflect"
	"slices"
	"sort"
	"strconv"
	"strings"
	"testing"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	acp "github.com/BrokkAi/acp-go/schema/unstable"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func (s *TranscriptSnapshot) richTail(width int, maximumLines int) []Line {
	lines, _ := s.RichTailScrolled(width, maximumLines, 0, DefaultToolDisplay())
	return lines
}

// transcriptPtr is a pointer to a copy of value, for Rust's Some(value).
func transcriptPtr[T any](value T) *T {
	return &value
}

// transcriptJSONString is a JSON string literal of text as serde_json writes
// it, for splicing into hand-written JSON.
func transcriptJSONString(text string) string {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(text); err != nil {
		panic(err)
	}
	return strings.TrimSuffix(buffer.String(), "\n")
}

// transcriptJSONBytes is serde_json's form of a byte slice: an array of
// numbers.
func transcriptJSONBytes(text string) string {
	numbers := make([]string, 0, len(text))
	for _, value := range []byte(text) {
		numbers = append(numbers, strconv.Itoa(int(value)))
	}
	return "[" + strings.Join(numbers, ",") + "]"
}

func newTranscriptChat() *ChatState {
	worker := snapshot()
	return NewChatState(&worker, nil)
}

// chatEntriesEqual is Vec<ChatEntry> equality: provenance (Source) is not
// part of an entry's value, and an empty list equals an absent one.
func chatEntriesEqual(t *testing.T, left, right []core.ChatEntry) bool {
	t.Helper()
	encodedLeft, err := json.Marshal(left)
	if err != nil {
		t.Fatal(err)
	}
	encodedRight, err := json.Marshal(right)
	if err != nil {
		t.Fatal(err)
	}
	return bytes.Equal(encodedLeft, encodedRight)
}

// cloneMaterializedSession is MaterializedSession::clone: items stay shared,
// the lists do not.
func cloneMaterializedSession(session *core.MaterializedSession) *core.MaterializedSession {
	cloned := *session
	cloned.Transcript = slices.Clone(session.Transcript)
	cloned.QueuedPrompts = slices.Clone(session.QueuedPrompts)
	cloned.PendingElicitations = slices.Clone(session.PendingElicitations)
	return &cloned
}

// makeMutItem is Arc::make_mut on a transcript slot: the slot gets its own
// copy, so entries that still point at the old item do not see the change.
func makeMutItem(slot **core.TranscriptItem) *core.TranscriptItem {
	copied := **slot
	*slot = &copied
	return &copied
}

func completedTool(seq uint64, title string) core.ChatEntry {
	entry := core.ChatEntryTool(seq, title, nil, core.ToolStatusCompleted)
	entry.ToolSummary = transcriptPtr(title)
	return entry
}

func completedExecuteTool(seq uint64, title string) core.ChatEntry {
	kind := acp.ToolKindExecute
	status := acp.ToolCallStatusCompleted
	call := acp.ToolCall{
		ToolCallID: acp.ToolCallId(fmt.Sprintf("call-%d", seq)),
		Title:      title,
		Kind:       &kind,
		Status:     &status,
	}
	presentation := transcript.ToolCallPresentation(&call)
	entry := core.ChatEntryTool(seq, title, nil, core.ToolStatusCompleted)
	entry.ToolSummary = transcriptPtr(presentation.Summary)
	entry.ToolPresentation = &presentation
	return entry
}

func sessionRestart(seq uint64) core.ChatEntry {
	return core.ChatEntryPlain(seq, core.ChatRoleSystem, core.SessionRestartText)
}

// keypadKey is a keypad press: Bubble Tea reports crossterm's
// KeyEventState::KEYPAD keys as their own codes (tea.KeyKpHome).
func keypadKey(code rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: code}
}

// wheel is a wheel event clear of the conversations pane, which is the hitbox
// hover routing checks before it hands the wheel to the transcript.
func wheel(button tea.MouseButton) tea.MouseMsg {
	return mouseIn(tea.MouseWheelMsg{Button: button}, uv.Rect(0, 10, 40, 1))
}

// leftClick is crossterm's Down(Left) at column, row.
func leftClick(column, row int) tea.MouseMsg {
	return tea.MouseClickMsg{X: column, Y: row, Button: tea.MouseLeft}
}

func countContaining(lines []string, needle string) int {
	count := 0
	for _, line := range lines {
		if strings.Contains(line, needle) {
			count++
		}
	}
	return count
}

func entryIDs(entries []*client.BrowserTranscriptEntry) []uint64 {
	ids := make([]uint64, 0, len(entries))
	for _, entry := range entries {
		ids = append(ids, entry.ID)
	}
	return ids
}

func assertStrings(t *testing.T, got []string, want ...string) {
	t.Helper()
	if !slices.Equal(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

// Hard-won: e13ed5c7c: one parked terminal record disabled reuse of every unchanged entry.
func TestBackgroundConversionReusesUnchangedEntriesAndRefreshesChangedText(t *testing.T) {
	session := core.MaterializedSessionEmpty("incremental")
	session.AppliedEventOrdinal = 3
	session.Transcript = []*core.TranscriptItem{
		userTranscriptItem(1, "first"),
		userTranscriptItem(2, "second"),
		// Output parked because its owner had left the live window.
		{
			StableID:        "terminal:parked",
			Position:        3,
			CreatedAtMs:     3,
			LastChangedAtMs: 3,
			Body: core.TranscriptBodyTerminalOutputOf(core.TerminalOutputRecord{
				TerminalID: "parked",
				Output:     core.NewPackedText("done\n"),
				Truncated:  false,
				ExitCode:   transcriptPtr(uint32(0)),
			}),
		},
	}
	first := TranscriptSnapshotFromMaterialized(&session)
	previous := first.ConvertedEntries()
	session.AppliedEventOrdinal = 4
	session.Transcript[1] = userTranscriptItem(2, "edited")
	session.Transcript = append(session.Transcript, userTranscriptItem(4, "third"))
	reused := TranscriptSnapshotFromMaterializedReusing(
		&session,
		map[string][]string{},
		previous,
		map[string][]string{},
	)
	if reused.entries[0].Revision != previous[0].Revision {
		t.Fatalf("revision %d, want %d", reused.entries[0].Revision, previous[0].Revision)
	}
	if reused.entries[1].Text != "edited" {
		t.Fatalf("text %q", reused.entries[1].Text)
	}
	if got, want := reused.BrowserTail(100), TranscriptSnapshotFromMaterialized(&session).BrowserTail(100); !slices.Equal(got, want) {
		t.Fatalf("browser tail %#v, want %#v", got, want)
	}
}

func TestBackgroundConversionRefreshesAddedAndRemovedDiffstats(t *testing.T) {
	session := core.MaterializedSessionEmpty("diffstats")
	session.AppliedEventOrdinal = 1
	session.Transcript = []*core.TranscriptItem{fixtureToolItem(1)}
	empty := map[string][]string{}
	stats := map[string][]string{"tool:1": {"src/file-1.rs +3 -1"}}
	original := TranscriptSnapshotFromMaterialized(&session)
	added := TranscriptSnapshotFromMaterializedReusing(&session, stats, original.entries, empty)
	if !slices.Equal(added.entries[0].ToolDiffstats, stats["tool:1"]) {
		t.Fatalf("diffstats %#v", added.entries[0].ToolDiffstats)
	}
	removed := TranscriptSnapshotFromMaterializedReusing(&session, empty, added.entries, stats)
	if !slices.Equal(removed.entries[0].ToolDiffstats, original.entries[0].ToolDiffstats) {
		t.Fatalf("diffstats %#v, want %#v", removed.entries[0].ToolDiffstats, original.entries[0].ToolDiffstats)
	}
}

func TestAdjacentSessionRestartMarkersKeepOnlyTheLatestPresentation(t *testing.T) {
	chat := newTranscriptChat()
	secondRestart := sessionRestart(2)
	secondRestart.RecordedAtMs = transcriptPtr(int64(2_000))
	chat.entries = []core.ChatEntry{
		sessionRestart(1),
		secondRestart,
		core.ChatEntryPlain(3, core.ChatRoleUser, "continue"),
		sessionRestart(4),
		sessionRestart(5),
	}
	chat.latestSeq = 5

	rich := transcriptText(chat, 80)
	if count := countContaining(rich, core.SessionRestartText); count != 2 {
		t.Fatalf("one marker survives each adjacent run: %#v", rich)
	}
	if len(chat.entries) != 5 {
		t.Fatal("presentation collapse does not rewrite the projected transcript")
	}
	chat.renderMode = client.TranscriptRenderModeRaw
	raw := transcriptText(chat, 80)
	if count := countContaining(raw, core.SessionRestartText); count != 2 {
		t.Fatalf("raw mode coalesces the markers while keeping its visible source rows: %#v", raw)
	}

	rawOnly := core.ChatEntryPlain(10, core.ChatRoleSystem, "captured terminal output")
	rawOnly.RawOnly = true
	rawChat := newTranscriptChat()
	rawChat.entries = []core.ChatEntry{sessionRestart(8), rawOnly, sessionRestart(12)}
	rawChat.renderMode = client.TranscriptRenderModeRaw
	rawWithVisibleDetail := transcriptText(rawChat, 80)
	if count := countContaining(rawWithVisibleDetail, core.SessionRestartText); count != 2 {
		t.Fatalf("a Raw-visible detail separates restart markers: %#v", rawWithVisibleDetail)
	}

	rawChat.renderMode = client.TranscriptRenderModeRich
	richWithOmittedDetail := transcriptText(rawChat, 80)
	if count := countContaining(richWithOmittedDetail, core.SessionRestartText); count != 1 {
		t.Fatalf("a Rich-omitted detail does not separate restart markers: %#v", richWithOmittedDetail)
	}

	browser := chat.TranscriptSnapshot().BrowserTranscript(nil)
	if ids := entryIDs(browser.Entries); !slices.Equal(ids, []uint64{2, 3, 5}) {
		t.Fatalf("the browser receives the newest marker from each run: %v", ids)
	}
	if at := browser.Entries[0].RecordedAtMs; at == nil || *at != 2_000 {
		t.Fatalf("recorded at %v", at)
	}
	if browser.LatestSeq != 5 {
		t.Fatalf("latest seq %d", browser.LatestSeq)
	}
	if browser.WindowStartSeq != 5 {
		t.Fatalf("window start %d", browser.WindowStartSeq)
	}

	// A viewer that had the older marker must be told to replace its feed;
	// otherwise the append-only browser DOM would retain the hidden entry.
	delta := chat.TranscriptSnapshot().BrowserTranscript(transcriptPtr(uint64(1)))
	if !delta.Reset {
		t.Fatal("the delta resets")
	}
	if ids := entryIDs(delta.Entries); !slices.Equal(ids, []uint64{2, 3, 5}) {
		t.Fatalf("ids %v", ids)
	}
}

func TestBrowserRestartCollapseResetsWhenADeliveredMarkerIsReplaced(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = []core.ChatEntry{
		core.ChatEntryPlain(1, core.ChatRoleUser, "before restart"),
		sessionRestart(2),
	}
	chat.latestSeq = 2
	opened := chat.TranscriptSnapshot().BrowserTranscript(nil)
	if ids := entryIDs(opened.Entries); !slices.Equal(ids, []uint64{1, 2}) {
		t.Fatalf("ids %v", ids)
	}
	if opened.WindowStartSeq != 1 {
		t.Fatalf("window start %d", opened.WindowStartSeq)
	}

	chat.entries = append(chat.entries, sessionRestart(3))
	chat.latestSeq = 3
	delta := chat.TranscriptSnapshot().BrowserTranscript(transcriptPtr(uint64(2)))
	if !delta.Reset {
		t.Fatal("the old marker was already delivered")
	}
	if delta.WindowStartSeq != 3 {
		t.Fatalf("window start %d", delta.WindowStartSeq)
	}
	if ids := entryIDs(delta.Entries); !slices.Equal(ids, []uint64{1, 3}) {
		t.Fatalf("ids %v", ids)
	}

	settled := chat.TranscriptSnapshot().BrowserTranscript(transcriptPtr(uint64(3)))
	if settled.Reset {
		t.Fatal("the settled view does not reset")
	}
	if len(settled.Entries) != 0 {
		t.Fatalf("entries %d", len(settled.Entries))
	}
}

// Hard-won: 0333efb9: review-generated prompts rendered as if the user typed them.
func TestAGeneratedReviewPromptRendersAsHelsOwnLine(t *testing.T) {
	item := &core.TranscriptItem{
		StableID: "user:1",
		Position: 1,
		Body: core.TranscriptBodyUser{
			Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"[HARNESS NOTE: wait for the delegated work to finish.]"}`)},
		},
	}
	typed := &core.TranscriptItem{
		StableID: "user:2",
		Position: 2,
		Body: core.TranscriptBodyUser{
			Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"fix the parser"}`)},
		},
	}
	session := core.MaterializedSessionEmpty("session-1")
	session.Transcript = []*core.TranscriptItem{item, typed}
	session.AppliedEventOrdinal = 2

	entries := client.MaterializedChatEntriesReusing(&session, 0, nil)
	if entries[0].Role != core.ChatRoleSystem {
		t.Fatalf("role %v", entries[0].Role)
	}
	if entries[1].Role != core.ChatRoleUser {
		t.Fatalf("role %v", entries[1].Role)
	}

	// Rebuilding reuses the entries rather than flipping their roles.
	again := client.MaterializedChatEntriesReusing(&session, 0, slices.Clone(entries))
	if again[0].Role != core.ChatRoleSystem {
		t.Fatalf("role %v", again[0].Role)
	}
	if again[1].Role != core.ChatRoleUser {
		t.Fatalf("role %v", again[1].Role)
	}
	if again[0].Text != entries[0].Text {
		t.Fatalf("text %q, want %q", again[0].Text, entries[0].Text)
	}
}

func TestAnEmptyConversationIdentifiesInitialRelayLoading(t *testing.T) {
	chat := newTranscriptChat()
	chat.setTranscriptLoading(true)

	assertStrings(t, transcriptText(chat, 80), "Loading…")

	chat.setTranscriptLoading(false)
	assertStrings(t, transcriptText(chat, 80), "No messages yet — send a prompt to begin.")
}

func TestConversationTitleIncludesTheSessionNameAfterTheDashboardSummary(t *testing.T) {
	chat := newTranscriptChat()
	chat.SetHeaderSummary("precision-3260/bifrost-fuzz", "kimi", "Fix the build")
	chat.turnStartedAtEpochSeconds = transcriptPtr(uint64(7_847))
	chat.setCurrentStepStart(transcriptPtr(int64(20_000_000)))

	assertTitle := func(want string) {
		t.Helper()
		if got := transcriptTitle(chat, 20_000).String(); got != want {
			t.Fatalf("title %q, want %q", got, want)
		}
	}
	assertTitle(" precision-3260/bifrost-fuzz  Working 3h22m  kimi  Fix the build ")
	chat.SetDetailedActivityClocks(true)
	assertTitle(" precision-3260/bifrost-fuzz  T 3h22m S 0s  kimi  Fix the build ")
	chat.SetDetailedActivityClocks(false)

	chat.renderMode = client.TranscriptRenderModeRaw
	assertTitle(" precision-3260/bifrost-fuzz  Working 3h22m  kimi  Fix the build · raw source ")

	// An idle session that left a command running names it in the same place
	// the turn clock goes.
	chat.renderMode = client.TranscriptRenderModeRich
	chat.turnStartedAtEpochSeconds = nil
	chat.setSessionActivity(client.SessionActivity{
		DismissQuestionSupported: false,
		PursuingGoal:             false,
		CheckingResponse:         false,
		BackgroundCommands: []core.BackgroundCommand{{
			ID:          "test-background",
			StartedAtMs: 17_384_000,
			Command:     "cargo test",
			CanStop:     false,
		}},
		ActiveUserShells: []core.ActiveUserShell{},
	})
	assertTitle(" precision-3260/bifrost-fuzz  1 task 43m36s  kimi  Fix the build ")
	chat.SetDetailedActivityClocks(true)
	assertTitle(" precision-3260/bifrost-fuzz  BG 43m36s  kimi  Fix the build ")
	chat.SetDetailedActivityClocks(false)

	previousActivity := *chat.SessionActivity()
	previousActivity.PursuingGoal = false
	previousActivity.CheckingResponse = false
	previousActivity.ForegroundToolStartedAtMs = transcriptPtr(int64(19_988_000))
	previousActivity.BackgroundCommands = []core.BackgroundCommand{}
	chat.setSessionActivity(previousActivity)
	assertTitle(" precision-3260/bifrost-fuzz  Working 12s  kimi  Fix the build ")
}

// bufferRowSymbols is one buffer row's cell symbols, untrimmed, as ratatui
// reads them: a cell a wide grapheme hides reads as a space.
func bufferRowSymbols(buffer *uv.Buffer, y int) string {
	var row strings.Builder
	for x := 0; x < buffer.Width(); x++ {
		row.WriteString(bufferSymbol(buffer, x, y))
	}
	return row.String()
}

// bufferSymbol is ratatui's buffer[(x, y)].symbol().
func bufferSymbol(buffer *uv.Buffer, x, y int) string {
	cell := buffer.CellAt(x, y)
	if cell == nil || cell.Content == "" {
		return " "
	}
	return cell.Content
}

// A host that draws its own chips at the right of the title row tells the
// chat how many columns to stay clear of, and the title stops short of them.
func TestALongTitleStopsShortOfTheColumnsTheHostReserved(t *testing.T) {
	chat := newTranscriptChat()
	chat.SetHeaderSummary("podman", "profile", strings.Repeat("t", 200))
	reserve := 4
	buffer := uv.NewBuffer(60, 10)
	frame := NewFrame(buffer)
	renderTranscript(frame, frame.Area(), chat, false, reserve, 0, false)
	header := bufferRowSymbols(buffer, 0)
	index := strings.LastIndex(header, "t")
	if index < 0 {
		t.Fatal("the title is drawn")
	}
	titleEnd := utf8.RuneCountInString(header[:index]) + 1
	if titleEnd > 60-1-reserve {
		t.Fatalf("the title has to stop short of the reserved chip columns: %q", header)
	}
}

func TestLongConversationTitlesUseTheHeaderWidthWhileWorking(t *testing.T) {
	chat := newTranscriptChat()
	chat.SetHeaderSummary("podman", "profile", strings.Repeat("界", 80))
	chat.markPromptSubmitted("continue")
	for _, width := range []int{32, 48, 80} {
		buffer := uv.NewBuffer(width, 10)
		frame := NewFrame(buffer)
		renderTranscript(frame, frame.Area(), chat, false, 0, 0, false)
		found := false
		for x := width - 3; x < width-1; x++ {
			if bufferSymbol(buffer, x, 0) == "…" {
				found = true
			}
		}
		if !found {
			t.Fatalf("width %d: %q", width, bufferRowSymbols(buffer, 0))
		}
	}
}

func TestRealToolClaimSuppressesOnlyTheMatchingTerminalIncarnation(t *testing.T) {
	claimedAtMs := core.EpochMillis()
	session := core.MaterializedSessionEmpty("session-terminal-claim")
	session.AppliedEventOrdinal = 1
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:shell",
		Position:        1,
		CreatedAtMs:     claimedAtMs,
		LastChangedAtMs: claimedAtMs,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(json.RawMessage(
				`{"toolCallId":"shell","title":"Shell","status":"in_progress","content":[{"type":"terminal","terminalId":"term-1"}]}`,
			)),
			TerminalRefs: []string{"term-1"},
		},
	}}
	chat := ChatStateFromMaterialized(&session, nil, nil)
	chat.setActiveAgentTerminals(
		[]core.ActiveAgentTerminal{{
			TerminalID:  "term-1",
			Command:     "hidden fallback command",
			StartedAtMs: claimedAtMs,
		}},
		&session,
	)

	claimed := transcriptText(chat, 80)
	if countContaining(claimed, "hidden fallback command") != 0 {
		t.Fatalf("the ACP tool card owns its live terminal: %#v", claimed)
	}

	chat.setActiveAgentTerminals(
		[]core.ActiveAgentTerminal{{
			TerminalID:  "term-1",
			Command:     "new bridge command",
			StartedAtMs: claimedAtMs + 1,
		}},
		&session,
	)
	reused := transcriptText(chat, 80)
	if countContaining(reused, "new bridge command") == 0 {
		t.Fatalf("an old claim cannot hide a reused id after restart: %#v", reused)
	}
}

func thought(seq uint64, text string) core.ChatEntry {
	return core.ChatEntryPlain(seq, core.ChatRoleThought, text)
}

// numberedChat is a chat with count single-line user messages, each naming
// its index so scroll assertions can name the row they expect to see.
func numberedChat(count int) *ChatState {
	chat := newTranscriptChat()
	chat.entries = make([]core.ChatEntry, 0, count)
	for index := range count {
		chat.entries = append(chat.entries, core.ChatEntryPlain(uint64(index), core.ChatRoleUser, fmt.Sprintf("message %d", index)))
	}
	return chat
}

func userTranscriptItem(position uint64, text string) *core.TranscriptItem {
	content, err := json.Marshal(text)
	if err != nil {
		panic(err)
	}
	return &core.TranscriptItem{
		StableID:        fmt.Sprintf("user:%d", position),
		Position:        position,
		CreatedAtMs:     int64(position) * 10,
		LastChangedAtMs: int64(position) * 10,
		Body: core.TranscriptBodyUser{
			Content: []json.RawMessage{content},
		},
	}
}

// fixtureMs: transcript items for the tail-first tests. Every item carries
// the same timestamps, so entries share a revision and a row cached at one
// position would be served at any other position the cache still believes
// in.
const fixtureMs int64 = 7

func fixtureItem(position uint64, stableID string, body core.TranscriptBody) *core.TranscriptItem {
	item := &core.TranscriptItem{
		StableID:        stableID,
		Position:        position,
		CreatedAtMs:     fixtureMs,
		LastChangedAtMs: fixtureMs,
		Body:            body,
	}
	// The projection requires an agent message to carry the ordinal of its
	// latest content chunk, and carries none for anything else.
	if _, agent := body.(core.TranscriptBodyAgent); agent {
		item.LatestContentEventOrdinal = transcriptPtr(position)
	}
	return item
}

func fixtureUserItem(position uint64) *core.TranscriptItem {
	content, err := json.Marshal(fmt.Sprintf("question %d", position))
	if err != nil {
		panic(err)
	}
	return fixtureItem(
		position,
		fmt.Sprintf("user:%d", position),
		core.TranscriptBodyUser{Content: []json.RawMessage{content}},
	)
}

func fixtureAgentItem(position uint64) *core.TranscriptItem {
	// Multi-kilobyte, so the conversion cost is realistic.
	chunks := make([]json.RawMessage, 0, 8)
	for chunk := range 8 {
		text := strings.Repeat(fmt.Sprintf("answer %d.%d ", position, chunk), 40)
		chunks = append(chunks, json.RawMessage(`{"content":{"type":"text","text":`+transcriptJSONString(text)+`}}`))
	}
	return fixtureItem(
		position,
		fmt.Sprintf("agent:%d", position),
		core.TranscriptBodyAgent{Chunks: chunks, Streaming: false},
	)
}

func fixtureThoughtItem(position uint64) *core.TranscriptItem {
	text := fmt.Sprintf("thinking about %d", position)
	return fixtureItem(
		position,
		fmt.Sprintf("thought:%d", position),
		core.TranscriptBodyThought{
			Chunks:    []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":` + transcriptJSONString(text) + `}}`)},
			Streaming: false,
		},
	)
}

func fixtureToolItem(position uint64) *core.TranscriptItem {
	call := fmt.Sprintf(
		`{"toolCallId":%s,"title":%s,"status":"completed","content":[{"type":"content","content":{"type":"text","text":%s}}],"locations":[{"path":%s,"line":3}]}`,
		transcriptJSONString(strconv.FormatUint(position, 10)),
		transcriptJSONString(fmt.Sprintf("read file-%d", position)),
		transcriptJSONString(strings.Repeat("output ", 600)),
		transcriptJSONString(fmt.Sprintf("src/file-%d.rs", position)),
	)
	return fixtureItem(
		position,
		fmt.Sprintf("tool:%d", position),
		core.TranscriptBodyTool{Call: core.OpaqueToolCallFromValue(json.RawMessage(call))},
	)
}

func fixturePlanItem(position uint64) *core.TranscriptItem {
	plan := fmt.Sprintf(
		`{"entries":[{"content":%s,"priority":"medium","status":"in_progress"}]}`,
		transcriptJSONString(fmt.Sprintf("step %d", position)),
	)
	return fixtureItem(
		position,
		fmt.Sprintf("plan:%d", position),
		core.TranscriptBodyPlan{Plan: json.RawMessage(plan)},
	)
}

func fixtureSystemItem(position uint64) *core.TranscriptItem {
	return fixtureItem(
		position,
		fmt.Sprintf("system:%d", position),
		core.TranscriptBodySystem{Text: fmt.Sprintf("notice %d", position)},
	)
}

// materializedSessionFrom is a conversation with the mix of bodies a real
// session carries, its first item at firstPosition. A compaction rewrite
// replaces the history in place, so it produces the same shape of transcript
// at fresh ordinals.
func materializedSessionFrom(firstPosition uint64, items uint64) *core.MaterializedSession {
	session := core.MaterializedSessionEmpty("session-long")
	session.Transcript = make([]*core.TranscriptItem, 0, items)
	for position := firstPosition; position < firstPosition+items; position++ {
		var item *core.TranscriptItem
		switch position % 6 {
		case 0:
			item = fixtureToolItem(position)
		case 1:
			item = fixtureUserItem(position)
		case 2:
			item = fixtureAgentItem(position)
		case 3:
			item = fixtureThoughtItem(position)
		case 4:
			item = fixturePlanItem(position)
		default:
			item = fixtureSystemItem(position)
		}
		session.Transcript = append(session.Transcript, item)
	}
	session.AppliedEventOrdinal = firstPosition + items
	return &session
}

// longMaterializedSession is a conversation with the mix of bodies a real
// session carries.
func longMaterializedSession(items uint64) *core.MaterializedSession {
	return materializedSessionFrom(1, items)
}

func entryTexts(entries []core.ChatEntry) []string {
	texts := make([]string, 0, len(entries))
	for _, entry := range entries {
		texts = append(texts, entry.Text)
	}
	return texts
}

func convertedPrefix(session *core.MaterializedSession, chat *ChatState) []core.ChatEntry {
	return client.MaterializedPrefixEntries(
		session.Transcript[:chat.unconvertedPrefix],
		session.AppliedEventOrdinal,
	)
}

// Hard-won: 2b229f70: opening a large session blocked the dashboard for seconds.
func TestOpeningALongSessionConvertsOnlyTheTail(t *testing.T) {
	items := uint64(TailSeedItems) + 400
	session := longMaterializedSession(items)

	chat := ChatStateFromMaterializedTail(session, nil, nil)

	if len(chat.entries) != TailSeedItems {
		t.Fatalf("entries %d", len(chat.entries))
	}
	if chat.unconvertedPrefix != 400 {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	eager := client.MaterializedChatEntries(session)
	if !chatEntriesEqual(t, chat.entries, eager[400:]) {
		t.Fatal("the tail matches the eager conversion")
	}
	shortSession := longMaterializedSession(uint64(TailSeedItems))
	shortChat := ChatStateFromMaterializedTail(shortSession, nil, nil)
	if shortChat.unconvertedPrefix != 0 {
		t.Fatalf("unconverted prefix %d", shortChat.unconvertedPrefix)
	}
	if !chatEntriesEqual(t, shortChat.entries, client.MaterializedChatEntries(shortSession)) {
		t.Fatal("the threshold keeps shorter conversations fully available")
	}
}

// Hard-won: 509af374e: grouped completed tools hid open permission requests.
func TestPermissionMarksFollowOpenRequestsAcrossPrefixSpliceAndRenderCache(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 300)
	chat := ChatStateFromMaterializedTail(session, nil, nil)
	tail := strconv.FormatUint((uint64(len(session.Transcript))/6)*6, 10)
	permission := func(id string) core.ElicitationRequest {
		request, err := core.ElicitationRequestFromACPParams(
			"permission-"+id,
			json.RawMessage(`{"mode":"form","sessionId":"session-long","message":"Run it?","requestedSchema":{"type":"object","required":["choice"],"properties":{"choice":{"type":"string","oneOf":[{"const":"approved","title":"Yes"}]}}}}`),
		)
		if err != nil {
			t.Fatal(err)
		}
		request.ToolPermission = &core.ToolPermissionForm{
			Skip:       true,
			ToolCallID: transcriptPtr(id),
		}
		return request
	}
	marks := func(chat *ChatState) []string {
		var marked []string
		for _, entry := range chat.entries {
			if entry.AwaitingApproval && entry.ToolCallID != nil {
				marked = append(marked, *entry.ToolCallID)
			}
		}
		return marked
	}
	prefix := convertedPrefix(session, chat)
	session.PendingElicitations = []core.ElicitationRequest{permission(tail), permission("6")}
	chat.ApplyMaterialized(session, nil, nil)
	assertStrings(t, marks(chat), transcript.ToolItemID(tail))
	if !chat.spliceTranscriptPrefix(prefix) {
		t.Fatal("the prefix splices")
	}
	assertStrings(t, marks(chat), "tool:6", transcript.ToolItemID(tail))
	session.PendingElicitations = nil
	chat.ApplyMaterialized(session, nil, nil)
	if len(marks(chat)) != 0 {
		t.Fatalf("marks %v", marks(chat))
	}

	// Exercise the warmed row cache without ApplyMaterialized's invalidation.
	for _, calls := range [][]*core.TranscriptItem{
		{fixtureToolItem(6)},
		{fixtureToolItem(6), fixtureToolItem(12)},
	} {
		session := core.MaterializedSessionEmpty("session-long")
		session.Transcript = calls
		session.AppliedEventOrdinal = 13
		chat := ChatStateFromMaterialized(&session, nil, nil)
		chat.toolDisplay.Layout = core.ToolOutputGrouped
		closed := drawnTranscript(chat, 100, 24)
		if shows(closed, client.ToolAwaitingApproval) {
			t.Fatalf("%#v", closed)
		}
		if !slices.ContainsFunc(chat.renderCache.entries, func(entry *CachedEntry) bool { return entry != nil }) {
			t.Fatal("the row cache is warm")
		}
		client.ApplyEntryListRules(chat.entries, []core.ElicitationRequest{permission("6")})
		opened := drawnTranscript(chat, 100, 24)
		if !shows(opened, client.ToolAwaitingApproval) {
			t.Fatalf("%#v", opened)
		}
		if chat.renderCache.collapse[0] != (client.EntryCollapse{Kind: client.EntryCollapseNone}) {
			t.Fatalf("collapse %#v", chat.renderCache.collapse[0])
		}
		client.ApplyEntryListRules(chat.entries, nil)
		if got := drawnTranscript(chat, 100, 24); !slices.Equal(got, closed) {
			t.Fatalf("got %#v, want %#v", got, closed)
		}
	}
}

func TestAnUpdateWhileThePrefixIsPendingKeepsTheTailAndStillSplices(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 300)
	chat := ChatStateFromMaterializedTail(session, nil, nil)
	prefix := convertedPrefix(session, chat)
	pending := chat.unconvertedPrefix

	appended := uint64(len(session.Transcript)) + 1
	session.Transcript = append(session.Transcript, fixtureUserItem(appended), fixtureAgentItem(appended+1))
	session.AppliedEventOrdinal = appended + 2
	chat.ApplyMaterialized(session, nil, nil)

	if chat.unconvertedPrefix != pending {
		t.Fatalf("unconverted prefix %d, want %d", chat.unconvertedPrefix, pending)
	}
	if len(chat.entries) != len(session.Transcript)-pending {
		t.Fatalf("entries %d", len(chat.entries))
	}
	assertStrings(t, entryTexts(chat.entries), entryTexts(client.MaterializedChatEntries(session)[pending:])...)

	if !chat.spliceTranscriptPrefix(prefix) {
		t.Fatal("the prefix splices")
	}
	assertStrings(t, entryTexts(chat.entries), entryTexts(client.MaterializedChatEntries(session))...)
	for index := 1; index < len(chat.entries); index++ {
		if chat.entries[index-1].StartSeq >= chat.entries[index].StartSeq {
			t.Fatalf("entries %d and %d are out of order", index-1, index)
		}
	}
}

func TestSplicingThePrefixDropsRenderRowsCachedAtTheOldPositions(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 120)
	chat := ChatStateFromMaterializedTail(session, nil, nil)
	prefix := convertedPrefix(session, chat)
	// Fill the cache while the entries still stand for the tail only.
	chat.anchor = TranscriptAnchorRow{Entry: 0, Row: 0}
	tailTop := drawnTranscript(chat, 60, 24)
	if !shows(tailTop, "question 121") {
		t.Fatalf("%#v", tailTop)
	}

	if !chat.spliceTranscriptPrefix(prefix) {
		t.Fatal("the prefix splices")
	}
	chat.anchor = TranscriptAnchorRow{Entry: 0, Row: 0}
	splicedTop := drawnTranscript(chat, 60, 24)

	eager := ChatStateFromMaterialized(session, nil, nil)
	eager.anchor = TranscriptAnchorRow{Entry: 0, Row: 0}
	if want := drawnTranscript(eager, 60, 24); !slices.Equal(splicedTop, want) {
		t.Fatalf("got %#v, want %#v", splicedTop, want)
	}
	if !shows(splicedTop, "question 1") {
		t.Fatalf("%#v", splicedTop)
	}
	if shows(splicedTop, "question 121") {
		t.Fatalf("%#v", splicedTop)
	}
}

func TestAPrefixThatNoLongerMeetsTheTailIsRefused(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 60)
	chat := ChatStateFromMaterializedTail(session, nil, nil)
	pending := chat.unconvertedPrefix
	// History from a compacted transcript: the right length, but it runs past
	// the first entry the tail holds.
	stale := client.MaterializedPrefixEntries(
		session.Transcript[len(session.Transcript)-pending:],
		session.AppliedEventOrdinal,
	)

	if chat.spliceTranscriptPrefix(stale) {
		t.Fatal("the stale prefix is refused")
	}

	if chat.unconvertedPrefix != pending {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	if len(chat.entries) != TailSeedItems {
		t.Fatalf("entries %d", len(chat.entries))
	}
}

// Hard-won: 93fa5d9a: a same-size compaction rewrite let a stale prefix match current history.
func TestAPrefixFromReplacedHistoryIsRefusedWhenTheRewriteKeepsTheLength(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 60)
	chat := ChatStateFromMaterializedTail(session, nil, nil)
	stale := convertedPrefix(session, chat)
	pending := chat.unconvertedPrefix
	if len(stale) != pending {
		t.Fatalf("stale %d, pending %d", len(stale), pending)
	}

	// Compaction rewrites the whole conversation at fresh ordinals and leaves
	// it exactly as long, so counting alone still lines up.
	rewritten := materializedSessionFrom(1_000, uint64(len(session.Transcript)))
	chat.ApplyMaterialized(rewritten, nil, nil)
	if chat.unconvertedPrefix != pending {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	if stale[len(stale)-1].StartSeq >= chat.entries[0].StartSeq {
		t.Fatal("the replaced history still sorts in front of the rewritten tail")
	}

	if chat.spliceTranscriptPrefix(stale) {
		t.Fatal("the stale prefix is refused")
	}

	if chat.unconvertedPrefix != pending {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	assertStrings(t, entryTexts(chat.entries), entryTexts(client.MaterializedChatEntries(rewritten)[pending:])...)
}

func TestCompactionBelowThePendingPrefixReseatsTheTail(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 500)
	chat := ChatStateFromMaterializedTail(session, nil, nil)
	prefix := convertedPrefix(session, chat)

	// Compaction leaves a transcript shorter than the pending prefix.
	compacted := longMaterializedSession(uint64(TailSeedItems) + 100)
	compacted.AppliedEventOrdinal = session.AppliedEventOrdinal + 1
	chat.ApplyMaterialized(compacted, nil, nil)

	if chat.unconvertedPrefix != 100 {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	if len(chat.entries) != TailSeedItems {
		t.Fatalf("entries %d", len(chat.entries))
	}
	assertStrings(t, entryTexts(chat.entries), entryTexts(client.MaterializedChatEntries(compacted)[100:])...)
	// The history built against the old transcript no longer fits.
	if chat.spliceTranscriptPrefix(prefix) {
		t.Fatal("the old prefix is refused")
	}
}

func shows(rows []string, needle string) bool {
	return slices.ContainsFunc(rows, func(row string) bool { return strings.Contains(row, needle) })
}

// visibleMessages is the message bodies on screen, ignoring the title and
// composer chrome and the scrollbar, whose marks move as the conversation
// grows.
func visibleMessages(rows []string) []string {
	var messages []string
	for _, row := range rows {
		_, body, ok := strings.Cut(row, "│ message ")
		if !ok {
			continue
		}
		word := ""
		if fields := strings.Fields(body); len(fields) > 0 {
			word = fields[0]
		}
		messages = append(messages, "message "+word)
	}
	return messages
}

func browserTailLabel(entry *client.BrowserTranscriptEntry) string {
	return entry.Label + ": " + entry.Lines[0]
}

func TestResetInteractionPreservesProjectedTranscriptAndRenderCache(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleAgent, "cached response"))
	_ = transcriptText(chat, 80)
	chat.setInput("draft")
	chat.promptHistory = append(chat.promptHistory, "previous")
	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-1", "queued"))
	chat.anchor = TranscriptAnchorRow{Entry: 0, Row: 4}
	chat.SetNotice("temporary")
	chat.voiceActive = true

	chat.resetInteraction()

	if len(chat.entries) != 1 {
		t.Fatalf("entries %d", len(chat.entries))
	}
	if chat.renderCache.entries[0] == nil {
		t.Fatal("the cached rows survive")
	}
	if chat.input != "draft" {
		t.Fatalf("input %q", chat.input)
	}
	if chat.inputCursor != len("draft") {
		t.Fatalf("cursor %d", chat.inputCursor)
	}
	if len(chat.promptHistory) != 0 {
		t.Fatalf("history %v", chat.promptHistory)
	}
	if len(chat.queuedPrompts) != 0 {
		t.Fatalf("queued %d", len(chat.queuedPrompts))
	}
	if !anchorIsBottom(chat.anchor) {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("notice %q", notice)
	}
	if chat.voiceActive {
		t.Fatal("voice is off")
	}
}

func TestAnExpandedToolHeaderShowsWhenTheCallRan(t *testing.T) {
	started := int64(1_000_000_000_000)
	header := func(endedAtMs *int64) string {
		entry := completedTool(2, "cargo test").WithRecordedAt(&started)
		entry.ToolEndedAtMs = endedAtMs
		return lineText(renderTranscriptEntryExpanded(&entry, 80))[0]
	}
	start, ok := FormatEventTime(&started)
	if !ok {
		t.Fatal("the start time formats")
	}
	end, ok := FormatEventTime(transcriptPtr(started + 5*60_000))
	if !ok {
		t.Fatal("the end time formats")
	}

	if got := header(transcriptPtr(started + 59_999)); !strings.HasSuffix(got, " · "+start) {
		t.Fatal(got)
	}
	if got := header(nil); !strings.HasSuffix(got, " · "+start) {
		t.Fatal(got)
	}
	if got := header(transcriptPtr(started + 5*60_000)); !strings.HasSuffix(got, " · "+start+"–"+end) {
		t.Fatal(got)
	}

	// A compact summary row stays without a time.
	entry := completedTool(2, "cargo test").WithRecordedAt(&started)
	entry.ToolEndedAtMs = transcriptPtr(started + 5*60_000)
	summary := lineText(renderTranscriptEntry(&entry, 80, client.TranscriptRenderModeRich))
	if strings.Contains(summary[0], start) {
		t.Fatalf("%#v", summary)
	}
}

func TestUserAndAgentHeadersShowFirstEventTimeAsLocalHoursAndMinutes(t *testing.T) {
	expected, ok := FormatEventTime(transcriptPtr(int64(0)))
	if !ok {
		t.Fatal("the epoch formats")
	}
	runtime := func(text string) json.RawMessage {
		payload, err := json.Marshal(core.RuntimeEventSessionUpdate{Update: json.RawMessage(
			`{"sessionUpdate":"agent_message_chunk","messageId":"message-1","content":{"type":"text","text":` + transcriptJSONString(text) + `}}`,
		)})
		if err != nil {
			t.Fatal(err)
		}
		return payload
	}
	events := []core.SequencedEvent{
		{
			Seq:          1,
			RecordedAtMs: transcriptPtr(int64(0)),
			RequestID:    transcriptPtr("p"),
			Event: core.WorkerEventPromptAccepted{
				RequestID:   "p",
				Text:        "work",
				Attachments: []core.Attachment{},
			},
		},
		{
			Seq:          2,
			RecordedAtMs: transcriptPtr(int64(0)),
			Event: core.WorkerEventAdapter{
				Kind:    "session_update",
				Payload: runtime("do"),
			},
		},
		{
			Seq:          3,
			RecordedAtMs: transcriptPtr(int64(60_000)),
			Event: core.WorkerEventAdapter{
				Kind:    "session_update",
				Payload: runtime("ne"),
			},
		},
	}
	initial := snapshot()
	initial.LatestSeq = 3
	chat := NewChatState(&initial, events)
	lines := transcriptText(chat, 80)

	if !slices.Contains(lines, "❯ You · "+expected) {
		t.Fatalf("%#v", lines)
	}
	if !slices.Contains(lines, "● Agent · "+expected) {
		t.Fatalf("%#v", lines)
	}
	if chat.entries[1].Text != "done" {
		t.Fatalf("text %q", chat.entries[1].Text)
	}
	if at := chat.entries[1].RecordedAtMs; at == nil || *at != 0 {
		t.Fatalf("recorded at %v", at)
	}
}

func TestLiveAcpDiffsRenderPathsWithoutCountingLinesOnTheEventLoop(t *testing.T) {
	chat := newTranscriptChat()
	chat.applySessionUpdate(1, json.RawMessage(
		`{"sessionUpdate":"tool_call","toolCallId":"edit-lib","title":"Edit src/lib.rs","status":"in_progress","content":[{"type":"diff","path":"/workspace/src/lib.rs","oldText":"alpha\n","newText":"alpha\nbeta\n"}]}`,
	))

	assertStrings(t, transcriptText(chat, 80), "● Tool · running", "│ Edit", "│ /workspace/src/lib.rs", "")

	chat.applySessionUpdate(2, json.RawMessage(
		`{"sessionUpdate":"tool_call_update","toolCallId":"edit-lib","status":"completed","content":[{"type":"diff","path":"/workspace/src/lib.rs","oldText":"alpha\n","newText":"gamma\n"}]}`,
	))

	assertStrings(t, chat.entries[0].ToolDiffstats, "/workspace/src/lib.rs")
	assertStrings(t, transcriptText(chat, 80), "✓ Tool · done", "│ Edit", "│ /workspace/src/lib.rs", "")
}

func TestCompletedToolRunCollapsesToSingleSummaryCell(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "grep -rn beta src"),
		completedTool(3, "cat notes.md"),
	)

	text := transcriptText(chat, 80)

	assertStrings(t, text,
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src, cat notes.md",
		"",
	)
}

func findClickTarget(t *testing.T, chat *ChatState, startSeq uint64, why string) TranscriptClickTarget {
	t.Helper()
	index := slices.IndexFunc(chat.transcriptClickTargets, func(target TranscriptClickTarget) bool {
		return target.StartSeq == startSeq
	})
	if index < 0 {
		t.Fatal(why)
	}
	return chat.transcriptClickTargets[index]
}

func TestClickingACollapsedMemberExpandsOnlyThatCallAndSplitsTheRun(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "first command"),
		completedTool(2, "second command"),
		completedTool(3, "third command"),
		completedTool(4, "fourth command"),
	)

	drawnTranscript(chat, 80, 24)
	target := findClickTarget(t, chat, 2, "the second summary segment is clickable")
	transcriptInner := uv.Rect(1, 1, 78, 22)
	for _, target := range chat.transcriptClickTargets {
		if !uv.Pos(target.Rect.Min.X, target.Rect.Min.Y).In(transcriptInner) ||
			!uv.Pos(max(target.Rect.Max.X-1, 0), max(target.Rect.Max.Y-1, 0)).In(transcriptInner) {
			t.Fatalf("target %#v leaves the transcript", target)
		}
	}

	chat.HandleMouse(leftClick(target.Rect.Min.X, target.Rect.Min.Y))
	drawnTranscript(chat, 80, 24)

	if _, ok := chat.expandedToolCalls[2]; !ok {
		t.Fatal("the second call is expanded")
	}
	if chat.renderCache.collapse[1].Kind != client.EntryCollapseExpanded {
		t.Fatalf("collapse %#v", chat.renderCache.collapse[1])
	}
	if collapse := chat.renderCache.collapse[2]; collapse.Kind != client.EntryCollapseSummary || collapse.End != 4 {
		t.Fatalf("collapse %#v", collapse)
	}

	expanded := findClickTarget(t, chat, 2, "the expanded call remains clickable")
	chat.HandleMouse(leftClick(expanded.Rect.Min.X, expanded.Rect.Min.Y))
	drawnTranscript(chat, 80, 24)

	if _, ok := chat.expandedToolCalls[2]; ok {
		t.Fatal("the second call is collapsed again")
	}
	if collapse := chat.renderCache.collapse[0]; collapse.Kind != client.EntryCollapseSummary || collapse.End != 4 {
		t.Fatalf("collapse %#v", collapse)
	}
}

// renderedTextPosition is the screen cell where text first appears in rows.
func renderedTextPosition(t *testing.T, rows []string, text string) (int, int) {
	t.Helper()
	for row, line := range rows {
		if offset := strings.Index(line, text); offset >= 0 {
			return displayWidth(line[:offset]), row
		}
	}
	t.Fatalf("missing %q in %#v", text, rows)
	return 0, 0
}

func clickRenderedText(t *testing.T, chat *ChatState, rows []string, text string) {
	t.Helper()
	column, row := renderedTextPosition(t, rows, text)
	chat.HandleMouse(leftClick(column, row))
}

func expandedToolSet(chat *ChatState) []uint64 {
	keys := make([]uint64, 0, len(chat.expandedToolCalls))
	for key := range chat.expandedToolCalls {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	return keys
}

func TestPartiallyScrolledRepeatedToolNamesExpandTheVisibleCall(t *testing.T) {
	chat := newTranscriptChat()
	for seq := uint64(1); seq <= 4; seq++ {
		tool := completedTool(seq, fmt.Sprintf("provider title %d", seq))
		tool.ToolSummary = transcriptPtr("repeat-command")
		tool.ToolContent = []string{fmt.Sprintf("details for call %d", seq)}
		chat.entries = append(chat.entries, tool)
	}
	chat.entries = append(chat.entries, core.ChatEntryPlain(5, core.ChatRoleAgent, strings.Repeat("later response\n", 30)))
	drawnTranscript(chat, 24, 16)
	// The first tool's summary row is above the viewport; the second and
	// third rows have identical text, including their trailing comma.
	chat.anchor = TranscriptAnchorRow{Entry: 0, Row: 2}
	rows := drawnTranscript(chat, 24, 16)
	if !strings.Contains(rows[1], "repeat-command,") {
		t.Fatalf("%#v", rows)
	}
	clickRenderedText(t, chat, rows, "repeat-command")
	if got := expandedToolSet(chat); !slices.Equal(got, []uint64{2}) {
		t.Fatalf("expanded %v", got)
	}
	rendered := transcriptText(chat, 24)
	if !shows(rendered, "provider title 2") {
		t.Fatalf("%#v", rendered)
	}
	if !shows(rendered, "details for call 2") {
		t.Fatalf("%#v", rendered)
	}
	if shows(rendered, "details for call 1") {
		t.Fatalf("%#v", rendered)
	}
}

func TestKimiShellToolRunCollapsesToCommandNames(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedExecuteTool(1, "Running: rg -n project_memory src"),
		completedExecuteTool(2, "Running: cargo test --lib"),
		completedExecuteTool(3, "Starting background: npm run preview"),
		core.ChatEntryPlain(4, core.ChatRoleUser, "continue"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ rg, cargo test, npm run preview",
		"",
		"❯ You",
		"│ continue",
		"",
	)
}

// A parser-derived summary starts at the executable even when a harness
// decorates the command title.
// Hard-won: 585c3402: leading command punctuation made a collapsed command title render incorrectly.
func TestACollapsedToolLabelStartsAtTheCommandName(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedExecuteTool(1, "Running: sed -n 1,10p notes.md"),
		completedExecuteTool(2, "./build.sh --release"),
		completedExecuteTool(3, "Running: ls -la"),
		core.ChatEntryPlain(4, core.ChatRoleUser, "continue"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ sed, ./build.sh, ls",
		"",
		"❯ You",
		"│ continue",
		"",
	)
}

func TestInterleavedToolsAndThoughtsRenderLatestThinkingThenToolCdl(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "sed -n 1,260p .agents/PLANS.md"),
		thought(2, "Planning coverage analysis with cargo llvm-cov"),
		completedTool(3, "cargo llvm-cov nextest --help"),
		thought(4, "Requesting full main help information"),
		completedTool(5, "cargo llvm-cov --help"),
		thought(6, "Planning durable coverage storage"),
		completedTool(7, "cargo llvm-cov report --help"),
		thought(8, "Planning optimized coverage reporting"),
		completedTool(9, "Editing files"),
		thought(10, "Preparing coverage environment cleanup"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"○ Thinking",
		"│ Preparing coverage environment cleanup",
		"",
		"✓ Tool · done",
		"│ sed -n 1,260p .agents/PLANS.md, cargo llvm-cov nextest --help, cargo llvm-cov",
		"│ --help, cargo llvm-cov report --help, Editing files",
		"",
	)
}

func TestThoughtOnlyStreakKeepsOnlyTheMostRecentBlock(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		thought(1, "first approach"),
		thought(2, "second approach"),
		thought(3, "final approach"),
	)

	assertStrings(t, transcriptText(chat, 80), "○ Thinking", "│ final approach", "")
}

func TestVisibleNonmembersBreakToolThoughtStreaks(t *testing.T) {
	separators := []core.ChatEntry{
		core.ChatEntryPlain(3, core.ChatRoleUser, "user boundary"),
		core.ChatEntryPlain(3, core.ChatRoleAgent, "agent boundary"),
		core.ChatEntryPlan(3, []core.PlanLine{}),
		core.ChatEntryPlain(3, core.ChatRoleSystem, "system boundary"),
		core.ChatEntryTool(3, "waiting tool", nil, core.ToolStatusPending),
		core.ChatEntryTool(3, "running tool", nil, core.ToolStatusRunning),
		core.ChatEntryTool(3, "failed tool", nil, core.ToolStatusFailed),
	}

	for _, separator := range separators {
		chat := newTranscriptChat()
		chat.entries = append(chat.entries,
			thought(1, "thought before boundary"),
			completedTool(2, "grep -rn alpha src"),
			separator,
			thought(4, "thought after boundary"),
			completedTool(5, "cat notes.md"),
			core.ChatEntryPlain(6, core.ChatRoleUser, "release trailing tool"),
		)

		rendered := transcriptText(chat, 80)
		for _, want := range []string{
			"│ thought before boundary",
			"│ thought after boundary",
			"│ grep -rn alpha src",
			"│ cat notes.md",
		} {
			if !slices.Contains(rendered, want) {
				t.Fatalf("missing %q in %#v", want, rendered)
			}
		}
		if slices.Contains(rendered, "│ grep, cat") {
			t.Fatalf("%#v", rendered)
		}
	}
}

func TestTrailingToolSummaryDoesNotChangeWhenALaterThoughtAppears(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		thought(2, "checking the first result"),
		completedTool(3, "cat notes.md"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"○ Thinking",
		"│ checking the first result",
		"",
		"✓ Tool · done",
		"│ grep -rn alpha src, cat notes.md",
		"",
	)

	chat.entries = append(chat.entries, thought(4, "checking the combined result"))

	assertStrings(t, transcriptText(chat, 80),
		"○ Thinking",
		"│ checking the combined result",
		"",
		"✓ Tool · done",
		"│ grep -rn alpha src, cat notes.md",
		"",
	)
}

func TestUpdatingTheLatestCollapsedThoughtInvalidatesTheSummaryCache(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		thought(2, "old thought"),
		completedTool(3, "cat notes.md"),
		thought(4, "latest thought"),
	)
	if rendered := transcriptText(chat, 80); !slices.Contains(rendered, "│ latest thought") {
		t.Fatalf("%#v", rendered)
	}

	chat.entries[3].Text = "revised latest thought"
	chat.entries[3].Touch(5)

	rendered := transcriptText(chat, 80)
	if !slices.Contains(rendered, "│ revised latest thought") {
		t.Fatalf("%#v", rendered)
	}
	if slices.Contains(rendered, "│ latest thought") {
		t.Fatalf("%#v", rendered)
	}
}

func TestCompletedToolRunCollapsesFullyOnceANewRequestStarts(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "grep -rn beta src"),
		completedTool(3, "cat notes.md"),
		core.ChatEntryPlain(4, core.ChatRoleUser, "now ship it"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src, cat notes.md",
		"",
		"❯ You",
		"│ now ship it",
		"",
	)
}

func TestNewestCompletedToolJoinsItsPredecessorImmediately(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "cat notes.md"),
	)

	assertStrings(t, transcriptText(chat, 80), "✓ Tool · done", "│ grep -rn alpha src, cat notes.md", "")
}

func TestALaterCompletedToolCollapsesTheEarlierRunEntirely(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "grep -rn beta src"),
		completedTool(3, "cat notes.md"),
		core.ChatEntryPlain(4, core.ChatRoleAgent, "found it"),
		completedTool(5, "rg gamma src"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src, cat notes.md",
		"",
		"● Agent",
		"│ found it",
		"",
		"✓ Tool · done",
		"│ rg gamma src",
		"",
	)
}

func TestAgentMessageBetweenCompletedToolsPreventsCollapsing(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		core.ChatEntryPlain(2, core.ChatRoleAgent, "found it"),
		completedTool(3, "cat notes.md"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src",
		"",
		"● Agent",
		"│ found it",
		"",
		"✓ Tool · done",
		"│ cat notes.md",
		"",
	)
}

func TestFailedToolRendersAloneAndBreaksTheCollapsedRun(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "grep -rn beta src"),
		core.ChatEntryTool(3, "cat missing.md", nil, core.ToolStatusFailed),
		completedTool(4, "rg gamma src"),
		completedTool(5, "rg delta src"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src",
		"",
		"× Tool · failed",
		"│ cat missing.md",
		"",
		"✓ Tool · done",
		"│ rg gamma src, rg delta src",
		"",
	)

	chat.entries = append(chat.entries, core.ChatEntryPlain(6, core.ChatRoleUser, "now ship it"))

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src",
		"",
		"× Tool · failed",
		"│ cat missing.md",
		"",
		"✓ Tool · done",
		"│ rg gamma src, rg delta src",
		"",
		"❯ You",
		"│ now ship it",
		"",
	)
}

func TestRawModeRendersEveryCompletedToolInFull(t *testing.T) {
	chat := newTranscriptChat()
	chat.renderMode = client.TranscriptRenderModeRaw
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "grep -rn beta src"),
		completedTool(3, "cat notes.md"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src",
		"",
		"✓ Tool · done",
		"│ grep -rn beta src",
		"",
		"✓ Tool · done",
		"│ cat notes.md",
		"",
	)
}

func TestRawModePreservesInterleavedToolsAndThoughtsInSourceOrder(t *testing.T) {
	chat := newTranscriptChat()
	chat.renderMode = client.TranscriptRenderModeRaw
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		thought(2, "first thought"),
		completedTool(3, "cat notes.md"),
		thought(4, "latest thought"),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src",
		"",
		"○ Thinking",
		"│ first thought",
		"",
		"✓ Tool · done",
		"│ cat notes.md",
		"",
		"○ Thinking",
		"│ latest thought",
		"",
	)
}

func TestARunningToolBreaksTheEarlierGroupThenJoinsWhenCompleted(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "grep -rn alpha src"),
		completedTool(2, "grep -rn beta src"),
		core.ChatEntryTool(3, "cat notes.md", nil, core.ToolStatusRunning),
	)

	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src",
		"",
		"● Tool · running",
		"│ cat notes.md",
		"",
	)

	chat.entries[2].Touch(4)
	chat.entries[2].ToolStatus = transcriptPtr(core.ToolStatusCompleted)

	// Once completed, the third summary joins the same run immediately.
	assertStrings(t, transcriptText(chat, 80),
		"✓ Tool · done",
		"│ grep -rn alpha src, grep -rn beta src, cat notes.md",
		"",
	)
}

func TestExactDiffstatsAreAvailableOnlyAfterTheToolFinishes(t *testing.T) {
	item := func(status string) core.TranscriptItem {
		return core.TranscriptItem{
			StableID:        "tool:edit",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 2,
			Body: core.TranscriptBodyTool{
				Call: core.OpaqueToolCallFromValue(json.RawMessage(
					`{"toolCallId":"edit","title":"Edit src/lib.rs","status":` + transcriptJSONString(status) +
						`,"content":[{"type":"diff","path":"/workspace/src/lib.rs","oldText":"alpha\n","newText":"alpha\nbeta\n"}]}`,
				)),
			},
		}
	}

	running := item("in_progress")
	if diffstats, ok := transcript.MaterializedToolDiffstats(&running); ok {
		t.Fatalf("diffstats %v", diffstats)
	}
	completed := item("completed")
	diffstats, ok := transcript.MaterializedToolDiffstats(&completed)
	if !ok {
		t.Fatal("a finished tool has diffstats")
	}
	assertStrings(t, diffstats, "/workspace/src/lib.rs  +1 −0")
}

func TestTranscriptBlocksKeepRoleHeadersAndWrappedBodyIndented(t *testing.T) {
	entry := core.ChatEntryPlain(1, core.ChatRoleUser, "alpha beta gamma")
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, entry)
	text := transcriptText(chat, 12)

	assertStrings(t, text, "❯ You", "│ alpha beta", "│ gamma", "")
}

// A preview is the conversation's own rendering minus its rail: the same
// rows, wrapped the same way, without the gutter that only means something
// under a role header.
// Hard-won: 7ab8368b: session previews repeated the transcript gutter after drawing their own prefix.
func TestAgentPreviewTailMatchesTheConversationBodyRowsWithoutTheGutter(t *testing.T) {
	text := "# heading\n\nfirst paragraph with some words to wrap\n\n- alpha\n- beta"
	entry := core.ChatEntryPlain(0, core.ChatRoleAgent, text)
	// The conversation spends two columns on the gutter, so a preview asked
	// for 38 columns of text renders the same rows as a 40-column transcript.
	var body []Line
	for _, line := range renderTranscriptEntry(&entry, 40, client.TranscriptRenderModeRich)[1:] { // header row
		if !lineIsEmpty(line) {
			body = append(body, withoutRoleGutter(line))
		}
	}
	if len(body) == 0 {
		t.Fatal("the message has body rows")
	}
	for _, line := range body {
		if len(line.Spans) > 0 && line.Spans[0].Content == roleGutter() {
			t.Fatal("the comparison rows have no gutter left to match")
		}
	}

	if got := RenderAgentMessageTail(text, 38, math.MaxInt); !reflect.DeepEqual(got, body) {
		t.Fatalf("got %#v, want %#v", got, body)
	}
	if got := RenderAgentMessageTail(text, 38, 2); !reflect.DeepEqual(got, body[len(body)-2:]) {
		t.Fatalf("got %#v, want %#v", got, body[len(body)-2:])
	}
}

// A session-list summary wraps only as far as its rows need. Those rows, and
// the ellipsis that says more was left out, are exactly what wrapping the
// whole message and keeping its first rows gives.
func TestAgentPreviewHeadMatchesTheFirstRowsOfTheWholeMessage(t *testing.T) {
	var findings strings.Builder
	for index := range 40 {
		fmt.Fprintf(&findings,
			"### Finding %d: `src/file_%d.rs:%d`\n\n- %s\n\n",
			index, index, index,
			strings.Repeat("the daemon publishes a revision and every waiter wakes ", 4),
		)
	}
	sources := []string{
		strings.Repeat("word ", 4000),
		strings.ReplaceAll(findings.String(), "\n", " "),
		findings.String(),
		strings.Repeat("漢字かな交じり文 ", 600),
		strings.Repeat("x", 9000),
		"short",
		"\n\n  \n",
		"first line\n**late-corpus diagnostics,**\nthird line",
	}
	for _, source := range sources {
		for _, width := range []int{1, 2, 7, 38, 80} {
			for _, maximum := range []int{1, 2, 3} {
				entry := core.ChatEntryPlain(0, core.ChatRoleAgent, source)
				var whole []Line
				for _, line := range entryBodyRows(&entry, width+roleGutterWidth, client.TranscriptRenderModeRich) {
					if !lineIsEmpty(line) {
						whole = append(whole, withoutRoleGutter(line))
					}
				}
				truncated := len(whole) > maximum
				whole = whole[:min(len(whole), maximum)]
				if truncated && len(whole) > 0 {
					appendTrimmedEllipsis(&whole[len(whole)-1], 0)
				}
				if got := RenderAgentMessageHead(source, width, maximum); !reflect.DeepEqual(got, whole) {
					t.Fatalf("width %d, %d rows, source %q: got %#v, want %#v",
						width, maximum, source[:min(len(source), 40)], got, whole)
				}
			}
		}
	}
}

func TestAgentPreviewHeadRemovesPunctuationBeforeItsEllipsis(t *testing.T) {
	lines := RenderAgentMessageHead(
		"first line\n**late-corpus diagnostics,**\nthird line",
		80,
		2,
	)
	rendered := lineText(lines)

	// No role gutter: the session list draws its own prefix in front of these
	// rows, and a second marker there means nothing.
	assertStrings(t, rendered, "first line", "late-corpus diagnostics…")
	spans := lines[1].Spans
	if !spans[len(spans)-1].Style.AddModifier.Contains(ModifierBold) {
		t.Fatalf("style %#v", spans[len(spans)-1].Style)
	}
}

func TestChangingThemeRecolorsCachedConversationWithoutChangingText(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		core.ChatEntryPlain(1, core.ChatRoleUser, "inspect the renderer"),
		core.ChatEntryPlain(2, core.ChatRoleAgent, "**Done.** Use `cargo test`."),
	)
	var previous []Line
	hasPrevious := false
	for _, theme := range core.UiThemeAll {
		WithTheme(theme, func() {
			lines := transcriptLines(chat, 60)
			accented := false
			for _, line := range lines {
				for _, span := range line.Spans {
					if span.Style.Fg == ThemePalette().Accent {
						accented = true
					}
				}
			}
			if !accented {
				t.Fatalf("%v: no span uses the accent", theme)
			}
			if hasPrevious {
				if !slices.Equal(lineText(lines), lineText(previous)) {
					t.Fatalf("%v: text changed", theme)
				}
				if reflect.DeepEqual(lines, previous) {
					t.Fatalf("%v: existing rows must adopt the new colors", theme)
				}
			}
			previous = lines
			hasPrevious = true
		})
	}
}

func TestTranscriptSnapshotTailCountsOnlyNonemptyRows(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"one\n\ntwo\n\nthree\n\nfour\n\nfive",
	))

	transcriptSnapshot := chat.TranscriptSnapshot()
	tail := lineText(transcriptSnapshot.richTail(80, 4))

	if len(tail) != 4 {
		t.Fatalf("tail %#v", tail)
	}
	for _, line := range tail {
		if strings.TrimSpace(line) == "" {
			t.Fatalf("tail %#v", tail)
		}
	}
	assertStrings(t, tail, "│ two", "│ three", "│ four", "│ five")
}

func TestBrowserTranscriptPreservesRowsIsUtf8SafeAndSupportsDeltas(t *testing.T) {
	chat := newTranscriptChat()
	lines := make([]string, 0, 1_005)
	for line := range 1_005 {
		lines = append(lines, fmt.Sprintf("line %d", line))
	}
	chat.entries = append(chat.entries,
		core.ChatEntryPlain(1, core.ChatRoleAgent, strings.Join(lines, "\n")),
		core.ChatEntryPlain(2, core.ChatRoleThought, strings.Repeat("🦀", client.BrowserLineBytes)),
	)
	chat.latestSeq = 2

	full := chat.TranscriptSnapshot().BrowserTranscript(nil)
	total := 0
	for _, entry := range full.Entries {
		total += len(entry.Lines)
	}
	if total != 1_006 {
		t.Fatalf("lines %d", total)
	}
	last := full.Entries[len(full.Entries)-1]
	if last.Role != "thought" {
		t.Fatalf("role %q", last.Role)
	}
	if full.Entries[0].Lines[0] != "line 0" {
		t.Fatalf("first line %q", full.Entries[0].Lines[0])
	}
	truncated := last.Lines[0]
	if !strings.HasSuffix(truncated, "… [truncated]") {
		t.Fatalf("truncated %q", truncated)
	}
	if len(truncated) > client.BrowserLineBytes {
		t.Fatalf("truncated to %d bytes", len(truncated))
	}
	if full.Reset {
		t.Fatal("the full transcript does not reset")
	}

	delta := chat.TranscriptSnapshot().BrowserTranscript(transcriptPtr(uint64(1)))
	if delta.Reset {
		t.Fatal("the delta does not reset")
	}
	if len(delta.Entries) != 1 {
		t.Fatalf("entries %d", len(delta.Entries))
	}
	if delta.Entries[0].UpdatedSeq != 2 {
		t.Fatalf("updated seq %d", delta.Entries[0].UpdatedSeq)
	}
	if !chat.TranscriptSnapshot().BrowserTranscript(transcriptPtr(uint64(0))).Reset {
		t.Fatal("a cursor before the window resets")
	}
}

func TestBrowserTranscriptExcludesEntriesBeforeProviderCompaction(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		core.ChatEntryPlain(1, core.ChatRoleUser, "old"),
		core.ChatEntryPlain(3, core.ChatRoleAgent, "current"),
	)
	chat.latestSeq = 3
	chat.lastCompactionSeq = 2

	browser := chat.TranscriptSnapshot().BrowserTranscript(nil)
	if len(browser.Entries) != 1 {
		t.Fatalf("entries %d", len(browser.Entries))
	}
	assertStrings(t, browser.Entries[0].Lines, "current")
	if label := browserTailLabel(browser.Entries[0]); label != "Agent: current" {
		t.Fatalf("label %q", label)
	}
}

// A delta has to be proportional to what changed, not to the window. The
// bodies the projection records no change ordinal for still overshoot, so
// this asserts a large reduction rather than a minimal one.
// Hard-won: 93fa5d9a: one transcript update retransmitted the full browser window.
func TestADeltaCostsAFractionOfTheWindowItUpdates(t *testing.T) {
	session := longMaterializedSession(600)
	frontier := session.AppliedEventOrdinal
	size := func(browser client.WebBrowserTranscript) int {
		encoded, err := json.Marshal(browser.Entries)
		if err != nil {
			t.Fatal(err)
		}
		return len(encoded)
	}
	window := size(TranscriptSnapshotFromMaterialized(session).BrowserTranscript(nil))

	appended := frontier + 1
	session.Transcript = append(session.Transcript, fixtureAgentItem(appended))
	session.AppliedEventOrdinal = appended
	delta := TranscriptSnapshotFromMaterialized(session).BrowserTranscript(&frontier)

	t.Logf("window %d bytes, delta %d bytes", window, size(delta))
	if size(delta)*4 >= window {
		t.Fatalf("one appended message resent %d of %d bytes", size(delta), window)
	}
}

// messageSession is the conversation a delta test needs: settled messages
// the projection records an exact update cursor for.
func messageSession(items uint64) *core.MaterializedSession {
	session := core.MaterializedSessionEmpty("session-delta")
	for position := uint64(1); position <= items; position++ {
		if position%2 == 1 {
			session.Transcript = append(session.Transcript, fixtureUserItem(position))
		} else {
			session.Transcript = append(session.Transcript, fixtureAgentItem(position))
		}
	}
	session.AppliedEventOrdinal = items
	return &session
}

func deltaIDs(t *testing.T, session *core.MaterializedSession, afterSeq uint64) []uint64 {
	t.Helper()
	delta := TranscriptSnapshotFromMaterialized(session).BrowserTranscript(&afterSeq)
	if delta.Reset {
		t.Fatal("the window still covers the viewer's cursor")
	}
	return entryIDs(delta.Entries)
}

// Hard-won: 93fa5d9a: one transcript update retransmitted the full browser window.
func TestAppendingOneMessageMarksOnlyThatEntryChanged(t *testing.T) {
	session := messageSession(8)
	opened := TranscriptSnapshotFromMaterialized(session).BrowserTranscript(nil)
	if len(opened.Entries) != 8 {
		t.Fatalf("entries %d", len(opened.Entries))
	}
	cursor := opened.LatestSeq

	session.Transcript = append(session.Transcript, fixtureAgentItem(9))
	session.AppliedEventOrdinal = 9

	if ids := deltaIDs(t, session, cursor); !slices.Equal(ids, []uint64{9}) {
		t.Fatalf("ids %v", ids)
	}
}

// appendAgentChunk appends one ACP content chunk to an agent message body.
func appendAgentChunk(item *core.TranscriptItem, text string) {
	body, ok := item.Body.(core.TranscriptBodyAgent)
	if !ok {
		panic("expected an agent message")
	}
	body.Chunks = append(slices.Clone(body.Chunks), json.RawMessage(`{"content":{"type":"text","text":`+transcriptJSONString(text)+`}}`))
	item.Body = body
}

// Hard-won: 93fa5d9a: one transcript update retransmitted the full browser window.
func TestAGrowingAgentMessageIsTheOnlyEntryItsDeltaCarries(t *testing.T) {
	session := messageSession(6)
	cursor := TranscriptSnapshotFromMaterialized(session).BrowserTranscript(nil).LatestSeq

	streaming := makeMutItem(&session.Transcript[5])
	appendAgentChunk(streaming, " and one more thing")
	streaming.LatestContentEventOrdinal = transcriptPtr(uint64(7))
	streaming.LastChangedAtMs = fixtureMs + 1
	session.AppliedEventOrdinal = 7

	if ids := deltaIDs(t, session, cursor); !slices.Equal(ids, []uint64{6}) {
		t.Fatalf("ids %v", ids)
	}
	delta := TranscriptSnapshotFromMaterialized(session).BrowserTranscript(&cursor)
	if !strings.HasSuffix(delta.Entries[0].Lines[0], " and one more thing") {
		t.Fatalf("line %q", delta.Entries[0].Lines[0])
	}
}

// I1-7: after a resume the pane opened on the revealed reply ("message 7 of
// N") and stayed there when a new reply arrived. The reveal is not a user
// scroll, so new content brings the view back to the tail.
// Hard-won: bf09b3d6: a resumed pane stayed on its opening reveal after a new reply arrived.
func TestNewContentAfterTheOpeningRevealFollowsTheTail(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"response advertised on the dashboard",
	))
	for index := range uint64(8) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(
			index+2,
			core.ChatRoleSystem,
			fmt.Sprintf("terminal failure %d\n%s", index, strings.Repeat("output\n", 12)),
		))
	}
	opened := drawnTranscript(chat, 60, 24)
	if !shows(opened, "End to follow") {
		t.Fatalf("the reveal opens mid-history: %#v", opened)
	}

	chat.entries = append(chat.entries, core.ChatEntryPlain(
		20,
		core.ChatRoleAgent,
		"the reply after resume",
	))
	rows := drawnTranscript(chat, 60, 24)
	if !shows(rows, "the reply after resume") {
		t.Fatalf("%#v", rows)
	}
	if shows(rows, "End to follow") {
		t.Fatalf("%#v", rows)
	}
}

// D-14: a narrow pinned pane opened on the reveal of an earlier reply, the
// dashboard switched away and back (a relaunch did this through the
// workspace it opened first), and the pane came back parked on that reply
// while the new reply landed below it. The saved position was the reveal's
// anchor, which the reopened view took for a reader's own scroll.
// Hard-won: 24c88abc: a reopened pane treated its saved opening reveal as a user scroll.
func TestAViewReopenedFromTheOpeningRevealFollowsNewRows(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries,
		core.ChatEntryPlain(1, core.ChatRoleAgent, "earlier reply"),
		core.ChatEntryPlain(2, core.ChatRoleUser, "detach probe\n"+strings.Repeat("wrapped line\n", 30)),
	)
	opened := drawnTranscript(chat, 31, 12)
	if !shows(opened, "earlier reply") {
		t.Fatalf("the reveal parks on the earlier reply: %#v", opened)
	}
	position := chat.TranscriptPosition()

	unchanged := newTranscriptChat()
	unchanged.entries = slices.Clone(chat.entries)
	unchanged.RestoreTranscriptPosition(position)
	if got := drawnTranscript(unchanged, 31, 12); !slices.Equal(got, opened) {
		t.Fatalf("got %#v, want %#v", got, opened)
	}

	reopened := newTranscriptChat()
	reopened.entries = slices.Clone(chat.entries)
	reopened.entries = append(reopened.entries, core.ChatEntryPlain(3, core.ChatRoleAgent, "reply to the probe"))
	reopened.RestoreTranscriptPosition(position)
	rows := drawnTranscript(reopened, 31, 12)

	if !shows(rows, "reply to the probe") {
		t.Fatalf("%#v", rows)
	}

	// A deliberate reader scroll survives the same save and restore path.
	chat = newTranscriptChat()
	for index := range uint64(40) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(index+1, core.ChatRoleUser, fmt.Sprintf("line %d", index)))
	}
	drawnTranscript(chat, 60, 24)
	chat.HandleKey(key(tea.KeyPgUp))
	drawnTranscript(chat, 60, 24)
	position = chat.TranscriptPosition()

	reopened = newTranscriptChat()
	reopened.entries = slices.Clone(chat.entries)
	reopened.entries = append(reopened.entries, core.ChatEntryPlain(100, core.ChatRoleAgent, "late reply"))
	reopened.RestoreTranscriptPosition(position)
	rows = drawnTranscript(reopened, 60, 24)

	if !shows(rows, "End to follow") {
		t.Fatalf("%#v", rows)
	}
	if shows(rows, "late reply") {
		t.Fatalf("%#v", rows)
	}
}

// ctrlKey is a press of a named key with Ctrl held.
func ctrlKey(code rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: code, Mod: tea.ModCtrl}
}

// Hard-won: 24c88abc: switching back to a tail repeated the opening reveal.
func TestAReopenedTailDoesNotRepeatTheOpeningReveal(t *testing.T) {
	chat := newTranscriptChat()
	var reply strings.Builder
	for line := 1; line <= 60; line++ {
		fmt.Fprintf(&reply, "reply line %d\n", line)
	}
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleAgent, reply.String()))
	drawnTranscript(chat, 60, 24)
	chat.HandleKey(ctrlKey(tea.KeyEnd))
	expected := drawnTranscript(chat, 60, 24)

	for range 3 {
		position := chat.TranscriptPosition()
		reopened := newTranscriptChat()
		reopened.entries = slices.Clone(chat.entries)
		reopened.RestoreTranscriptPosition(position)
		if got := drawnTranscript(reopened, 60, 24); !slices.Equal(got, expected) {
			t.Fatalf("got %#v, want %#v", got, expected)
		}
		chat = reopened
	}
}

func TestQuestionTargetResolvesAfterOlderHistoryLoadAndMissingTargetFallsBack(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 120)
	full := ChatStateFromMaterialized(session, nil, nil)
	full.RestoreTranscriptPosition(TranscriptPositionAtStableID("user:2"))
	expected := drawnTranscript(full, 60, 24)

	reopened := ChatStateFromMaterializedTail(session, nil, nil)
	prefix := convertedPrefix(session, reopened)
	reopened.RestoreTranscriptPosition(TranscriptPositionAtStableID("user:2"))
	if rows := drawnTranscript(reopened, 60, 24); !shows(rows, "Loading") {
		t.Fatalf("%#v", rows)
	}
	if !reopened.spliceTranscriptPrefix(prefix) {
		t.Fatal("the prefix splices")
	}
	if got := drawnTranscript(reopened, 60, 24); !slices.Equal(got, expected) {
		t.Fatalf("got %#v, want %#v", got, expected)
	}

	missing := ChatStateFromMaterializedTail(session, nil, nil)
	prefix = convertedPrefix(session, missing)
	missing.RestoreTranscriptPosition(TranscriptPositionAtStableID("missing"))
	if !missing.spliceTranscriptPrefix(prefix) {
		t.Fatal("the prefix splices")
	}
	drawnTranscript(missing, 60, 24)
	if !anchorIsBottom(missing.anchor) {
		t.Fatalf("anchor %#v", missing.anchor)
	}
}

// Hard-won: 2b229f70: a saved full-history index was restored against the tail.
func TestAReopenedScrollSurvivesTailFirstHistoryLoadingInEitherOrder(t *testing.T) {
	session := longMaterializedSession(uint64(TailSeedItems) + 120)
	for _, entry := range []int{1, 121} {
		chat := ChatStateFromMaterialized(session, nil, nil)
		chat.anchor = TranscriptAnchorRow{Entry: entry, Row: 3}
		expected := drawnTranscript(chat, 60, 24)
		encoded, err := json.Marshal(chat.TranscriptPosition())
		if err != nil {
			t.Fatal(err)
		}
		var position TranscriptPosition
		if err := json.Unmarshal(encoded, &position); err != nil {
			t.Fatal(err)
		}
		// Fixed previous-build handoff shape: legacy indexes refer to the full
		// transcript, even when the new view has only loaded its tail.
		var legacy TranscriptPosition
		if err := json.Unmarshal([]byte(fmt.Sprintf(`{"Row":{"entry":%d,"row":3}}`, entry)), &legacy); err != nil {
			t.Fatal(err)
		}

		for _, saved := range []TranscriptPosition{position, legacy} {
			for _, drawBeforeHistory := range []bool{false, true} {
				reopened := ChatStateFromMaterializedTail(session, nil, nil)
				prefix := convertedPrefix(session, reopened)
				reopened.RestoreTranscriptPosition(saved)
				if drawBeforeHistory {
					rows := drawnTranscript(reopened, 60, 24)
					if entry < reopened.unconvertedPrefix {
						if !shows(rows, "Loading") {
							t.Fatalf("%#v", rows)
						}
						if reopened.FrameSurfaces().Surface(SurfaceIdTranscript{}) != nil {
							t.Fatal("a loading transcript registers no surface")
						}
						if _, ok := reopened.transcriptScrollbar.pointer.Geometry(); ok {
							t.Fatal("a loading transcript has no scrollbar")
						}
					}
					// Switching away again while history loads must keep the
					// original position, rather than save a temporary viewport.
					saved := reopened.TranscriptPosition()
					reopened.RestoreTranscriptPosition(saved)
				}
				if !reopened.spliceTranscriptPrefix(prefix) {
					t.Fatal("the prefix splices")
				}
				if got := drawnTranscript(reopened, 60, 24); !slices.Equal(got, expected) {
					t.Fatalf("entry %d, draw before history %v, saved %#v: got %#v, want %#v",
						entry, drawBeforeHistory, saved, got, expected)
				}
			}
		}
	}

	// Saving from a partial view must use the same entry identity after
	// arriving messages move the next open's tail-loading boundary.
	partial := ChatStateFromMaterializedTail(session, nil, nil)
	partial.anchor = TranscriptAnchorRow{Entry: 1, Row: 3}
	drawnTranscript(partial, 60, 24)
	saved := partial.TranscriptPosition()
	newer := cloneMaterializedSession(session)
	next := uint64(len(newer.Transcript)) + 1
	for position := next; position < next+20; position++ {
		newer.Transcript = append(newer.Transcript, fixtureUserItem(position))
	}
	newer.AppliedEventOrdinal += 20
	expectedChat := ChatStateFromMaterialized(newer, nil, nil)
	expectedChat.anchor = TranscriptAnchorRow{Entry: 121, Row: 3}
	expected := drawnTranscript(expectedChat, 60, 24)

	reopened := ChatStateFromMaterializedTail(newer, nil, nil)
	prefix := convertedPrefix(newer, reopened)
	reopened.RestoreTranscriptPosition(saved)
	if rows := drawnTranscript(reopened, 60, 24); !shows(rows, "Loading") {
		t.Fatalf("%#v", rows)
	}
	if !reopened.spliceTranscriptPrefix(prefix) {
		t.Fatal("the prefix splices")
	}
	if got := drawnTranscript(reopened, 60, 24); !slices.Equal(got, expected) {
		t.Fatalf("got %#v, want %#v", got, expected)
	}
}

func TestMouseWheelReachesTheTailAcrossALargeCollapsedToolRun(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleUser, "before tools"))
	for seq := uint64(2); seq < 102; seq++ {
		chat.entries = append(chat.entries, completedTool(seq, fmt.Sprintf("command %d", seq)))
	}
	for index := range uint64(20) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(102+index, core.ChatRoleUser, fmt.Sprintf("tail message %d", index)))
	}
	wheel := func(button tea.MouseButton) tea.MouseMsg {
		return mouseIn(tea.MouseWheelMsg{Button: button}, uv.Rect(0, 10, 60, 1))
	}
	rows := drawnTranscript(chat, 60, 24)

	upwardSteps := 0
	for !shows(rows, "before tools") && upwardSteps < 40 {
		chat.HandleMouse(wheel(tea.MouseWheelUp))
		rows = drawnTranscript(chat, 60, 24)
		upwardSteps++
	}
	if !shows(rows, "before tools") {
		t.Fatal("wheel up reached old history")
	}

	for range upwardSteps + 1 {
		chat.HandleMouse(wheel(tea.MouseWheelDown))
		rows = drawnTranscript(chat, 60, 24)
	}
	if !shows(rows, "tail message 19") {
		t.Fatalf("wheel down reached the tail: %#v", rows)
	}
	if shows(rows, "End to follow") {
		t.Fatalf("the conversation resumed following after crossing hidden tool entries: %#v", rows)
	}
}

// Hard-won: 1f30f668: scrolling an empty transcript panicked on a missing render-cache entry.
func TestTheWheelOverAnEmptyTranscriptHasNothingToScroll(t *testing.T) {
	chat := newTranscriptChat()
	rows := drawnTranscript(chat, 40, 24)

	chat.HandleMouse(wheel(tea.MouseWheelUp))
	chat.HandleMouse(wheel(tea.MouseWheelDown))

	if got := drawnTranscript(chat, 40, 24); !slices.Equal(got, rows) {
		t.Fatalf("got %#v, want %#v", got, rows)
	}
}

func TestScrolledHistoryStaysPutWhileNewMessagesStreamIn(t *testing.T) {
	chat := numberedChat(40)
	_ = drawnTranscript(chat, 60, 24)
	chat.HandleKey(key(tea.KeyPgUp))
	before := drawnTranscript(chat, 60, 24)
	if !shows(before, "End to follow") {
		t.Fatalf("%#v", before)
	}

	for index := 40; index < 50; index++ {
		chat.entries = append(chat.entries, core.ChatEntryPlain(
			uint64(index),
			core.ChatRoleUser,
			fmt.Sprintf("message %d", index),
		))
	}
	chat.entries = append(chat.entries, core.ChatEntryPlain(100, core.ChatRoleAgent, "late reply"))
	after := drawnTranscript(chat, 60, 24)

	if !slices.Equal(visibleMessages(before), visibleMessages(after)) {
		t.Fatalf("appending messages must not move a scrolled-back viewport: %#v then %#v", before, after)
	}
	if len(visibleMessages(after)) == 0 {
		t.Fatalf("%#v", after)
	}
	if !shows(after, "End to follow") {
		t.Fatal("the scrolled reader remains marked as behind the tail")
	}
	if shows(after, "late reply") {
		t.Fatalf("%#v", after)
	}
}

func TestAdjacentThoughtMessagesCoalesceWithoutAnExtraSeparator(t *testing.T) {
	chat := newTranscriptChat()
	for _, update := range []struct {
		seq  uint64
		id   string
		text string
	}{{1, "one", "first thought"}, {2, "two", "second thought"}} {
		chat.applySessionUpdate(update.seq, json.RawMessage(
			`{"sessionUpdate":"agent_thought_chunk","messageId":`+transcriptJSONString(update.id)+
				`,"content":{"type":"text","text":`+transcriptJSONString(update.text)+`}}`,
		))
	}

	if len(chat.entries) != 1 {
		t.Fatalf("entries %d", len(chat.entries))
	}
	if chat.entries[0].Text != "first thought\nsecond thought" {
		t.Fatalf("text %q", chat.entries[0].Text)
	}
	rendered := transcriptText(chat, 80)
	if countContaining(rendered, "Thinking") != 1 {
		t.Fatalf("%#v", rendered)
	}
	assertStrings(t, rendered, "○ Thinking", "│ first thought", "│ second thought", "")
}

// debugTerminalColor is ratatui's Debug of a drawn cell's color.
func debugTerminalColor(drawn color.Color) string {
	switch value := drawn.(type) {
	case nil:
		return "Reset"
	case ansi.BasicColor:
		return namedColorDebugNames[int(value)+1]
	case ansi.IndexedColor:
		return fmt.Sprintf("Indexed(%d)", uint8(value))
	case color.RGBA:
		return fmt.Sprintf("Rgb(%d, %d, %d)", value.R, value.G, value.B)
	}
	red, green, blue, _ := drawn.RGBA()
	return fmt.Sprintf("Rgb(%d, %d, %d)", red>>8, green>>8, blue>>8)
}

// debugOptionColor is Rust's Debug of an Option<Color> style field.
func debugOptionColor(value Color) string {
	if !value.IsSet() {
		return "None"
	}
	return "Some(" + value.GoString() + ")"
}

// debugAnchor is Rust's Debug of a TranscriptAnchor.
func debugAnchor(anchor TranscriptAnchor) string {
	switch anchor := anchor.(type) {
	case TranscriptAnchorRow:
		return fmt.Sprintf("Row { entry: %d, row: %d }", anchor.Entry, anchor.Row)
	case TranscriptAnchorRestoring:
		return fmt.Sprintf("Restoring(%#v)", anchor.Position)
	default:
		return "Bottom"
	}
}

// debugSet is Rust's Debug of a BTreeSet<u64>.
func debugSet(values []uint64) string {
	sorted := slices.Clone(values)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i] < sorted[j] })
	parts := make([]string, 0, len(sorted))
	for _, value := range sorted {
		parts = append(parts, strconv.FormatUint(value, 10))
	}
	return "{" + strings.Join(parts, ", ") + "}"
}

// debugString is Rust's Debug of a string.
func debugString(text string) string {
	var quoted strings.Builder
	quoted.WriteByte('"')
	for _, character := range text {
		switch character {
		case '"':
			quoted.WriteString(`\"`)
		case '\\':
			quoted.WriteString(`\\`)
		case '\n':
			quoted.WriteString(`\n`)
		case '\r':
			quoted.WriteString(`\r`)
		case '\t':
			quoted.WriteString(`\t`)
		default:
			if unicode.IsControl(character) {
				fmt.Fprintf(&quoted, `\u{%x}`, character)
			} else {
				quoted.WriteRune(character)
			}
		}
	}
	quoted.WriteByte('"')
	return quoted.String()
}

// debugChatAction is Rust's Debug of the ChatAction values these tests see.
func debugChatAction(action ChatAction) string {
	switch action := action.(type) {
	case nil, ChatActionNone:
		return "None"
	case ChatActionOpenLink:
		return "OpenLink(" + debugString(action.URL) + ")"
	case ChatActionCopyLink:
		return "CopyLink(" + debugString(action.URL) + ")"
	}
	return fmt.Sprintf("%#v", action)
}
