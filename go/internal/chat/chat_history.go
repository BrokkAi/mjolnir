// Port of mj-chat/src/chat/history.rs.

// Prompt history: reverse-i-search over the stored prompts and the up/down
// walk through this session's earlier prompts.

package chat

import (
	"context"
	"fmt"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// historyLookahead is how many matches the search keeps fetched beyond the
// selected one, so stepping to an older match never waits on the store.
const historyLookahead = 8

type historySearch struct {
	originalInput  string
	originalCursor int
	originalImages []PromptImage
	generation     uint64
	query          TextInput
	scope          core.HistoryScope
	// matches is every match fetched for this query, in the order the store
	// listed them, each text once.
	matches  []core.PromptHistoryEntry
	selected *int
	// next is where the next page starts. Nil before the first page arrives
	// and once the store has no more matches; fetching tells the two apart.
	next *core.HistoryCursor
	// fetching is set while a page request is queued or in flight for this
	// generation.
	fetching    bool
	unavailable *string
}

// ahead is the fetched matches not yet stepped to.
func (s *historySearch) ahead() int {
	if s.selected == nil {
		return len(s.matches)
	}
	return max(len(s.matches)-(*s.selected+1), 0)
}

// moreMayFollow reports whether the store may hold matches the search has
// not fetched.
func (s *historySearch) moreMayFollow() bool {
	return s.fetching || s.next != nil
}

type historySearchRequest struct {
	generation uint64
	bundleID   *string
	scope      core.HistoryScope
	query      string
	after      *core.HistoryCursor
	limit      int
}

// composerRequest is every background request produced by a Prompt editor.
// Both attached chats and detached hosts pump the exhaustive dispatcher.
type composerRequest interface {
	isComposerRequest()
}

type composerRequestSearch historySearchRequest

// composerRequestSessionHistory carries the listing's generation.
type composerRequestSessionHistory uint64

type composerRequestDaemon struct {
	request ChatDaemonRequest
}

func (composerRequestSearch) isComposerRequest()         {}
func (composerRequestSessionHistory) isComposerRequest() {}
func (composerRequestDaemon) isComposerRequest()         {}

func (c *ChatState) SetPromptHistoryProject(bundleID *string) {
	c.bundleID = bundleID
}

// DispatchComposerRequests drains the queued composer requests. session
// opens the session a request runs against, inside the request's task;
// persistence is the daemon channel, nil when the editor cannot reach it.
func (c *ChatState) DispatchComposerRequests(
	session func(ctx context.Context) (client.SessionHandle, error),
	persistence *core.Unbounded[ChatDaemonRequest],
) {
	panic("unported: mj-chat/src/chat/history.rs DispatchComposerRequests")
}

// sessionHistoryRequested reports whether this composer has ever asked for
// its session's history. Generations start at zero and only advance on a
// request.
func (c *ChatState) sessionHistoryRequested() bool {
	return c.sessionHistoryGeneration != 0
}

func (c *ChatState) RequestSessionHistory() {
	panic("unported: mj-chat/src/chat/history.rs RequestSessionHistory")
}

// setSessionHistory takes this session's stored prompts, newest first, as
// listed for request generation.
func (c *ChatState) setSessionHistory(generation uint64, entries []core.PromptHistoryEntry) {
	panic("unported: mj-chat/src/chat/history.rs setSessionHistory")
}

func (c *ChatState) setSessionHistoryUnavailable(generation uint64, err string) {
	if generation != c.sessionHistoryGeneration {
		return
	}
	if c.sessionHistoryError == nil || *c.sessionHistoryError != err {
		c.sessionHistoryError = &err
	}
}

func (c *ChatState) beginHistorySearch() {
	panic("unported: mj-chat/src/chat/history.rs beginHistorySearch")
}

// refreshHistorySearch starts the search again after its query or scope
// changed.
func (c *ChatState) refreshHistorySearch() {
	panic("unported: mj-chat/src/chat/history.rs refreshHistorySearch")
}

func (c *ChatState) queueHistoryPage(after *core.HistoryCursor, limit int) {
	panic("unported: mj-chat/src/chat/history.rs queueHistoryPage")
}

// refillHistorySearch fetches the next page when fewer than historyLookahead
// matches lie ahead of the selected one and the store may have more.
func (c *ChatState) refillHistorySearch() {
	search := c.historySearch
	if search == nil {
		return
	}
	ahead := search.ahead()
	if search.fetching || ahead >= historyLookahead {
		return
	}
	if search.next != nil {
		after := *search.next
		c.queueHistoryPage(&after, historyLookahead-ahead)
	}
}

// applyHistorySearchResults takes one page of results for request
// generation, or the error that ended the search.
func (c *ChatState) applyHistorySearchResults(generation uint64, page core.PromptHistoryPage, err error) {
	panic("unported: mj-chat/src/chat/history.rs applyHistorySearchResults")
}

// chatStateResolveHistorySearchRequest is Rust's associated
// `ChatState::resolve_history_search_request`.
func chatStateResolveHistorySearchRequest(
	ctx context.Context,
	request historySearchRequest,
	session client.SessionHandle,
) (core.PromptHistoryPage, error) {
	panic("unported: mj-chat/src/chat/history.rs chatStateResolveHistorySearchRequest")
}

func (c *ChatState) stepHistorySearch(direction int) {
	panic("unported: mj-chat/src/chat/history.rs stepHistorySearch")
}

func (c *ChatState) cycleHistoryScope() {
	search := c.historySearch
	if search == nil {
		return
	}
	// Each press narrows the search; the last wraps back to the widest.
	switch search.scope {
	case core.HistoryScopeAll:
		search.scope = core.HistoryScopeProject
	case core.HistoryScopeProject:
		search.scope = core.HistoryScopeSession
	case core.HistoryScopeSession:
		search.scope = core.HistoryScopeAll
	}
	c.refreshHistorySearch()
}

func (c *ChatState) cancelHistorySearch() {
	panic("unported: mj-chat/src/chat/history.rs cancelHistorySearch")
}

func (c *ChatState) acceptHistorySearch() {
	if c.historySearch != nil && c.historySearch.selected != nil {
		c.historySearch = nil
		c.historyIndex = nil
		c.updateAutocomplete()
	}
}

// handleHistorySearchKey handles keys while reverse-i-search is open. The
// search owns every key, so nothing typed here reaches the composer.
func (c *ChatState) handleHistorySearchKey(code rune, modifiers tea.KeyMod) {
	panic("unported: mj-chat/src/chat/history.rs handleHistorySearchKey")
}

func (c *ChatState) RecordPromptHistory(prompt string) {
	if len(c.promptHistory) == 0 || c.promptHistory[len(c.promptHistory)-1] != prompt {
		c.promptHistory = append(c.promptHistory, prompt)
	}
	c.historyIndex = nil
	c.historyDraft = ""
	c.historyDraftImages = nil
}

// navigationHistory is stored prompts, then this visit's, oldest first. A
// prompt in both, or sent again, is listed once at its newest place.
func (c *ChatState) navigationHistory() []string {
	panic("unported: mj-chat/src/chat/history.rs navigationHistory")
}

func (c *ChatState) moveHistory(delta int) {
	panic("unported: mj-chat/src/chat/history.rs moveHistory")
}

func historyScopeName(scope core.HistoryScope) string {
	switch scope {
	case core.HistoryScopeSession:
		return "session"
	case core.HistoryScopeProject:
		return "project"
	default:
		return "all projects"
	}
}

// historySearchStatus is the shared status text for the conversation footer
// and a one-row Prompt host, and its hints. Hints stay separate so the
// compact field does not reimplement search modes.
func historySearchStatus(search *historySearch) (string, string) {
	panic("unported: mj-chat/src/chat/history.rs historySearchStatus")
}

func historySearchFooter(search *historySearch) string {
	status, hints := historySearchStatus(search)
	return fmt.Sprintf("reverse-i-search [%s]: %s  %s%s",
		historyScopeName(search.scope), search.query.String(), status, hints)
}

func highlightedInputLines(input string, query string) []Line {
	panic("unported: mj-chat/src/chat/history.rs highlightedInputLines")
}

func caseInsensitiveMatchRanges(text string, query string) []ByteRange {
	panic("unported: mj-chat/src/chat/history.rs caseInsensitiveMatchRanges")
}
