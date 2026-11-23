// Port of mj-chat/src/chat/history.rs (test-only items and tests).

package chat

import (
	"fmt"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (c *ChatState) takeSessionHistoryRequest() (uint64, bool) {
	for index, request := range c.composerRequests {
		if generation, ok := request.(composerRequestSessionHistory); ok {
			c.composerRequests = append(c.composerRequests[:index:index], c.composerRequests[index+1:]...)
			return uint64(generation), true
		}
	}
	return 0, false
}

func (c *ChatState) takeHistorySearchRequest() (historySearchRequest, bool) {
	for index, request := range c.composerRequests {
		if search, ok := request.(composerRequestSearch); ok {
			c.composerRequests = append(c.composerRequests[:index:index], c.composerRequests[index+1:]...)
			return historySearchRequest(search), true
		}
	}
	return historySearchRequest{}, false
}

func historyEntry(id int64, text string) core.PromptHistoryEntry {
	return core.PromptHistoryEntry{ID: id, SessionID: "1234567890", Text: text}
}

func typeQuery(chat *ChatState, query string) {
	for _, character := range query {
		chat.HandleKey(key(character))
	}
}

// answerHistoryPage answers the queued page request with texts and next,
// returning the request it answered.
func answerHistoryPage(t *testing.T, chat *ChatState, texts []string, next *core.HistoryCursor) historySearchRequest {
	t.Helper()
	request, ok := chat.takeHistorySearchRequest()
	if !ok {
		t.Fatal("history search request was queued")
	}
	entries := make([]core.PromptHistoryEntry, 0, len(texts))
	for index, text := range texts {
		entries = append(entries, historyEntry(int64(index), text))
	}
	chat.applyHistorySearchResults(request.generation, core.PromptHistoryPage{Entries: entries, Next: next}, nil)
	return request
}

func singleLineFrame(chat *ChatState, width int) (string, uv.Position) {
	buffer := uv.NewBuffer(width, 1)
	frame := NewFrame(buffer)
	chat.DrawSingleLinePrompt(frame, frame.Area(), true)
	surface := SurfaceFrameFixed(SurfaceIdPromptInput{}, uv.Rect(0, 0, width, 1))
	selected := NewSelectionRange(NewContentPos(0, 0), NewContentPos(0, width-1))
	text := ExtractRows(buffer, &surface, &selected)
	cursor, _ := frame.CursorPosition()
	return text, cursor
}

func imageDraft(t *testing.T, chat *ChatState) []PromptImage {
	t.Helper()
	chat.setPromptImagesSupported(true)
	chat.setInput("unfinished")
	chat.handleClipboardContent(ClipboardContentImage{Image: ClipboardImage{
		DataBase64: "synthetic-png",
		MimeType:   "image/png",
	}})
	if len(chat.inputImages) == 0 {
		t.Fatal("the image was not attached")
	}
	return append([]PromptImage(nil), chat.inputImages...)
}

func promptImagesEqual(a, b []PromptImage) bool {
	if len(a) != len(b) {
		return false
	}
	for index := range a {
		if !a[index].Equal(&b[index]) {
			return false
		}
	}
	return true
}

// Hard-won: e557e89: shipped Ctrl-C did nothing, forcing users to retype abandoned prompts
func TestControlCStashesTheTypedPromptIntoHistoryAndClearsTheInput(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	for _, character := range "draft prompt" {
		chat.HandleKey(key(character))
	}

	if action := chat.HandleKey(ctrl('c')); action != (ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.input != "" || chat.inputCursor != 0 {
		t.Fatalf("input = %q, cursor %d", chat.input, chat.inputCursor)
	}

	// Repeated cancellation on the empty composer keeps the draft
	// recoverable.
	if action := chat.HandleKey(ctrl('c')); action != (ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if action := chat.HandleKey(ctrl('c')); action != (ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}

	chat.HandleKey(key(tea.KeyUp))
	if chat.input != "draft prompt" {
		t.Fatalf("recalled input = %q", chat.input)
	}
}

func TestStaleHistorySearchResultsAreDropped(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.SetHistoryContext("bundle")
	chat.setInput("draft")

	chat.HandleKey(ctrl('r'))
	typeQuery(chat, "fix")
	stale, ok := chat.takeHistorySearchRequest()
	if !ok {
		t.Fatal("first search request")
	}
	chat.HandleKey(key(' '))

	chat.applyHistorySearchResults(stale.generation, core.PromptHistoryPage{
		Entries: []core.PromptHistoryEntry{historyEntry(1, "fix parser")},
	}, nil)
	if chat.input != "draft" {
		t.Fatalf("a stale page changed the input to %q", chat.input)
	}

	answerHistoryPage(t, chat, []string{"fix renderer"}, nil)
	if chat.input != "fix renderer" {
		t.Fatalf("input = %q", chat.input)
	}
	shown, _ := singleLineFrame(chat, 100)
	if !(strings.Contains(shown, "fix renderer") && strings.Contains(shown, "fix ") && strings.Contains(shown, "1/1")) {
		t.Fatal(shown)
	}
	chat.HandleSingleLinePromptKey(key('x'))
	answerHistoryPage(t, chat, nil, nil)
	shown, _ = singleLineFrame(chat, 100)
	if !(strings.Contains(shown, "fix x") && strings.Contains(shown, "no match")) {
		t.Fatal(shown)
	}
	if !chat.HandleSingleLinePromptKey(key(tea.KeyEnter)).Consumed {
		t.Fatal("Enter was not consumed")
	}
	// Restoring a refused reply from a button also closes search. Return its
	// original draft so the host can preserve it, not the preview.
	if original := chat.SetSingleLinePromptText("restored\nreply"); original != "draft" {
		t.Fatalf("returned draft = %q", original)
	}
	if chat.historySearch != nil {
		t.Fatal("search stayed open")
	}
	if chat.input != "restored\nreply" {
		t.Fatalf("input = %q", chat.input)
	}
}

func TestHistorySearchKeepsEightMatchesQueuedAndRefillsFromTheCursor(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.SetHistoryContext("bundle")
	chat.HandleKey(ctrl('r'))
	typeQuery(chat, "fi")
	if _, ok := chat.takeHistorySearchRequest(); ok {
		t.Fatal("two characters are too short for the index")
	}
	if footer := historySearchFooter(chat.historySearch); !strings.Contains(footer, "type 3+") {
		t.Fatal(footer)
	}
	chat.HandleKey(key('x'))

	// The first page fills the shown match and the eight behind it.
	firstPage := make([]string, 0, 9)
	for index := range 9 {
		firstPage = append(firstPage, fmt.Sprintf("fix %d", index))
	}
	cursor := core.HistoryCursor{Tier: 1, Before: 40, Through: 60}
	request := answerHistoryPage(t, chat, firstPage, &cursor)
	if request.after != nil || request.limit != 9 {
		t.Fatalf("request after %v, limit %d", request.after, request.limit)
	}
	if chat.input != "fix 0" {
		t.Fatalf("input = %q", chat.input)
	}
	if _, ok := chat.takeHistorySearchRequest(); ok {
		t.Fatal("eight matches are already queued")
	}
	if footer := historySearchFooter(chat.historySearch); !strings.Contains(footer, "1/9+") {
		t.Fatal(footer)
	}

	// Stepping leaves seven ahead, so the next page resumes at the cursor.
	chat.HandleKey(ctrl('r'))
	if chat.input != "fix 1" {
		t.Fatalf("input = %q", chat.input)
	}
	request = answerHistoryPage(t, chat, []string{"fix elsewhere"}, nil)
	if request.after == nil || *request.after != cursor || request.limit != 1 {
		t.Fatalf("request after %v, limit %d", request.after, request.limit)
	}

	for range 20 {
		chat.HandleKey(ctrl('r'))
	}
	if chat.input != "fix elsewhere" {
		t.Fatalf("input = %q", chat.input)
	}
	if _, ok := chat.takeHistorySearchRequest(); ok {
		t.Fatal("the store reported no more matches")
	}
	if footer := historySearchFooter(chat.historySearch); !strings.Contains(footer, "10/10 ") {
		t.Fatal(footer)
	}
}

func TestCancellingReverseSearchRestoresInlineImageRanges(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	images := imageDraft(t, chat)
	draft := chat.input
	chat.SetHistoryContext("bundle")

	chat.HandleKey(ctrl('r'))
	typeQuery(chat, "his")
	answerHistoryPage(t, chat, []string{"old history prompt"}, nil)
	if chat.input != "old history prompt" || len(chat.inputImages) != 0 {
		t.Fatalf("input = %q with %d images", chat.input, len(chat.inputImages))
	}

	chat.HandleKey(key(tea.KeyEscape))
	if chat.input != draft {
		t.Fatalf("input = %q", chat.input)
	}
	if !promptImagesEqual(chat.inputImages, images) {
		t.Fatalf("images = %+v", chat.inputImages)
	}
}

func TestHistoryNavigationRestoresUnsentImageDraftAndClearsItForHistory(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	images := imageDraft(t, chat)
	draft := chat.input
	chat.promptHistory = []string{"unrelated history prompt"}

	chat.moveHistory(-1)
	if chat.input != "unrelated history prompt" || len(chat.inputImages) != 0 {
		t.Fatalf("input = %q with %d images", chat.input, len(chat.inputImages))
	}

	chat.moveHistory(1)
	if chat.input != draft || !promptImagesEqual(chat.inputImages, images) {
		t.Fatalf("input = %q, images %+v", chat.input, chat.inputImages)
	}

	chat.moveHistory(-1)
	if chat.input != "unrelated history prompt" || len(chat.inputImages) != 0 {
		t.Fatalf("input = %q with %d images", chat.input, len(chat.inputImages))
	}
}

// Hard-won: b6b9787: this session's prompts appeared interleaved with other sessions' history
func TestMoveHistoryWalksOnlyThisSessionsPrompts(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.SetHistoryContext("bundle")
	chat.RequestSessionHistory()
	generation, ok := chat.takeSessionHistoryRequest()
	if !ok {
		t.Fatal("session history request")
	}
	chat.setSessionHistory(generation, []core.PromptHistoryEntry{
		{ID: 3, SessionID: "1234567890", Text: "mine newest"},
		{ID: 1, SessionID: "1234567890", Text: "mine oldest"},
	})
	// The detached presentation must expose all of a recalled multiline
	// payload, including Unicode, while retaining literal submission text.
	chat.RecordPromptHistory("first 世界\nsecond\nthird")
	chat.ClearDraft()
	chat.HandleSingleLinePromptKey(ctrl('p'))
	if chat.input != "first 世界\nsecond\nthird" {
		t.Fatalf("input = %q", chat.input)
	}
	shown, cursor := singleLineFrame(chat, 80)
	if !strings.Contains(shown, "first 世界⏎second⏎third") {
		t.Fatal(shown)
	}
	if cursor.X != displayWidth("first 世界⏎second⏎third") {
		t.Fatalf("cursor at %d", cursor.X)
	}
	shown, cursor = singleLineFrame(chat, 8)
	if !(strings.Contains(shown, "third") && cursor.X < 8) {
		t.Fatal(shown)
	}
	chat.HandleSingleLinePromptKey(ctrl('a'))
	_, cursor = singleLineFrame(chat, 80)
	if cursor.X != displayWidth("first 世界⏎second⏎") {
		t.Fatalf("cursor at %d", cursor.X)
	}
	chat.HandleSingleLinePromptKey(ctrl('a'))
	chat.HandleSingleLinePromptKey(ctrl('a'))
	shown, cursor = singleLineFrame(chat, 8)
	if !(strings.HasPrefix(shown, "first ") && cursor.X == 0) {
		t.Fatal(shown)
	}
	chat.HandleSingleLinePromptKey(ctrl('e'))
	_, cursor = singleLineFrame(chat, 80)
	if cursor.X != displayWidth("first 世界") {
		t.Fatalf("cursor at %d", cursor.X)
	}
	chat.promptHistory = nil
	chat.ClearDraft()
	chat.RecordPromptHistory("this visit")

	var recalled []string
	for range 4 {
		chat.HandleKey(key(tea.KeyUp))
		recalled = append(recalled, chat.input)
	}
	if want := []string{"this visit", "mine newest", "mine oldest", "mine oldest"}; strings.Join(recalled, "|") != strings.Join(want, "|") {
		t.Fatalf("recalled = %q", recalled)
	}
	chat.HandleKey(key(tea.KeyEscape))

	// Hard-won: 8e9b542d5: a prompt sent here earlier was recalled before a newer one sent from the web
	chat.RequestSessionHistory()
	generation, ok = chat.takeSessionHistoryRequest()
	if !ok {
		t.Fatal("session history request")
	}
	stored := func(id int64, text string) core.PromptHistoryEntry {
		return core.PromptHistoryEntry{ID: id, SessionID: "1234567890", Text: text}
	}
	chat.setSessionHistory(generation, []core.PromptHistoryEntry{
		stored(5, "sent from the web"),
		stored(4, "this visit"),
		stored(3, "mine newest"),
		stored(1, "mine oldest"),
	})
	chat.historyIndex = nil
	chat.clearInput()
	recalled = nil
	for range 3 {
		chat.HandleKey(key(tea.KeyUp))
		recalled = append(recalled, chat.input)
	}
	if want := []string{"sent from the web", "this visit", "mine newest"}; strings.Join(recalled, "|") != strings.Join(want, "|") {
		t.Fatalf("recalled = %q", recalled)
	}
}
