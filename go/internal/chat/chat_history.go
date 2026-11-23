// Port of mj-chat/src/chat/history.rs.

// Prompt history: reverse-i-search over the stored prompts and the up/down
// walk through this session's earlier prompts.

package chat

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

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
	if c.historySearch == nil {
		c.composerRuntime.historyTask.drop()
		c.composerRuntime.historyTask = nil
		c.composerRequests = slices.DeleteFunc(c.composerRequests, func(request composerRequest) bool {
			_, search := request.(composerRequestSearch)
			return search
		})
	}
	for len(c.composerRequests) > 0 {
		request := c.composerRequests[0]
		c.composerRequests = c.composerRequests[1:]
		switch request := request.(type) {
		case composerRequestSearch:
			search := historySearchRequest(request)
			search.bundleID = c.bundleID
			generation := search.generation
			updates := c.composerRuntime.tx
			c.composerRuntime.historyTask.drop()
			c.composerRuntime.historyTask = spawnRemoteResultForwarder(func(ctx context.Context) {
				var page core.PromptHistoryPage
				handle, err := session(ctx)
				if err == nil {
					page, err = chatStateResolveHistorySearchRequest(ctx, search, handle)
				}
				if ctx.Err() != nil {
					return
				}
				if sendErr := updates.Send(composerIoUpdateHistorySearchResults{generation: generation, result: page, err: err}); sendErr != nil {
					slog.Debug("history search result dropped because the editor closed", "error", sendErr)
				}
			})
		case composerRequestSessionHistory:
			generation := uint64(request)
			updates := c.composerRuntime.tx
			c.composerRuntime.sessionHistoryTask.drop()
			c.composerRuntime.sessionHistoryTask = spawnRemoteResultForwarder(func(ctx context.Context) {
				var entries []core.PromptHistoryEntry
				handle, err := session(ctx)
				if err == nil {
					entries, err = handle.SessionPrompts(ctx)
				}
				if ctx.Err() != nil {
					return
				}
				if sendErr := updates.Send(composerIoUpdateSessionHistoryListed{generation: generation, result: entries, err: err}); sendErr != nil {
					slog.Debug("session history result dropped because the editor closed", "error", sendErr)
				}
			})
		case composerRequestDaemon:
			// Enumerate daemon commands here too: adding one must require a
			// decision in the common dispatcher, never in a host pump.
			switch request.request.(type) {
			case ChatDaemonRequestRecordAbandonedPrompt, ChatDaemonRequestStartTurnReview:
				if persistence == nil {
					c.SetNotice("This editor cannot reach the Mjolnir daemon; the text remains in Prompt history")
				} else if err := persistence.Send(request.request); err != nil {
					slog.Warn("composer request could not be queued for the daemon", "error", err)
					c.SetNotice("The Mjolnir daemon is not reachable; the text remains in Prompt history")
				}
			default:
				panic(fmt.Sprintf("unhandled composer daemon request %T", request.request))
			}
		}
	}
}

// sessionHistoryRequested reports whether this composer has ever asked for
// its session's history. Generations start at zero and only advance on a
// request.
func (c *ChatState) sessionHistoryRequested() bool {
	return c.sessionHistoryGeneration != 0
}

func (c *ChatState) RequestSessionHistory() {
	c.sessionHistoryGeneration++
	c.composerRequests = slices.DeleteFunc(c.composerRequests, func(request composerRequest) bool {
		_, listing := request.(composerRequestSessionHistory)
		return listing
	})
	c.composerRequests = append(c.composerRequests, composerRequestSessionHistory(c.sessionHistoryGeneration))
}

// setSessionHistory takes this session's stored prompts, newest first, as
// listed for request generation.
func (c *ChatState) setSessionHistory(generation uint64, entries []core.PromptHistoryEntry) {
	if generation != c.sessionHistoryGeneration {
		return
	}
	c.sessionHistoryError = nil
	c.sessionHistory = make([]string, 0, len(entries))
	for index := len(entries) - 1; index >= 0; index-- {
		c.sessionHistory = append(c.sessionHistory, entries[index].Text)
	}
	// Prompts sent from here are stored before they are submitted, so a
	// listing that holds one has it in its true place among prompts sent from
	// elsewhere. Only prompts it does not hold yet stay local.
	stored := make(map[string]struct{}, len(c.sessionHistory))
	for _, text := range c.sessionHistory {
		stored[text] = struct{}{}
	}
	c.promptHistory = slices.DeleteFunc(c.promptHistory, func(text string) bool {
		_, held := stored[text]
		return held
	})
	if c.historyIndex != nil && *c.historyIndex >= len(c.navigationHistory()) {
		c.historyIndex = nil
		c.historyDraft = ""
		c.historyDraftImages = nil
	}
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
	c.autocomplete = nil
	c.historySearch = &historySearch{
		originalInput:  c.input,
		originalCursor: c.inputCursor,
		originalImages: append([]PromptImage(nil), c.inputImages...),
		generation:     c.nextHistorySearchGeneration,
		query:          NewTextInput(),
		scope:          core.HistoryScopeAll,
	}
	c.dropHistorySearchRequests()
}

// dropHistorySearchRequests removes every queued history page request.
func (c *ChatState) dropHistorySearchRequests() {
	c.composerRequests = slices.DeleteFunc(c.composerRequests, func(request composerRequest) bool {
		_, search := request.(composerRequestSearch)
		return search
	})
}

// refreshHistorySearch starts the search again after its query or scope
// changed.
func (c *ChatState) refreshHistorySearch() {
	generation := c.nextHistorySearchGeneration + 1
	search := c.historySearch
	if search == nil {
		return
	}
	c.nextHistorySearchGeneration = generation
	search.generation = generation
	search.matches = nil
	search.selected = nil
	search.next = nil
	search.fetching = false
	c.dropHistorySearchRequests()
	searchable := utf8.RuneCountInString(search.query.Value()) >= core.MinHistoryQueryChars
	original := PromptPayload{Text: search.originalInput, Images: append([]PromptImage(nil), search.originalImages...)}
	originalCursor := search.originalCursor
	c.setInputPayload(original)
	c.inputCursor = originalCursor
	if searchable {
		// The first match is shown, so the first page also fills the queue
		// behind it.
		c.queueHistoryPage(nil, historyLookahead+1)
	}
}

func (c *ChatState) queueHistoryPage(after *core.HistoryCursor, limit int) {
	search := c.historySearch
	if search == nil {
		return
	}
	search.fetching = true
	c.composerRequests = append(c.composerRequests, composerRequestSearch{
		generation: search.generation,
		bundleID:   c.bundleID,
		scope:      search.scope,
		query:      search.query.String(),
		after:      after,
		limit:      limit,
	})
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
	search := c.historySearch
	if search == nil || search.generation != generation {
		return
	}
	search.fetching = false
	if err != nil {
		original := PromptPayload{Text: search.originalInput, Images: append([]PromptImage(nil), search.originalImages...)}
		originalCursor := search.originalCursor
		c.setInputPayload(original)
		c.inputCursor = originalCursor
		c.historySearch = nil
		c.SetNotice(fmt.Sprintf("History unavailable: %v", err))
		c.updateAutocomplete()
		return
	}
	// The store lists each prompt text once across all pages.
	search.matches = append(search.matches, page.Entries...)
	search.next = page.Next
	search.unavailable = nil
	if search.selected == nil && len(search.matches) > 0 {
		text := search.matches[0].Text
		first := 0
		search.selected = &first
		c.setInput(text)
	}
	c.refillHistorySearch()
}

// chatStateResolveHistorySearchRequest is Rust's associated
// `ChatState::resolve_history_search_request`.
func chatStateResolveHistorySearchRequest(
	ctx context.Context,
	request historySearchRequest,
	session client.SessionHandle,
) (core.PromptHistoryPage, error) {
	if request.bundleID == nil {
		return core.PromptHistoryPage{}, errors.New("this chat has no project to search")
	}
	return session.SearchPrompts(ctx, *request.bundleID, request.scope, request.query, request.after, request.limit)
}

func (c *ChatState) stepHistorySearch(direction int) {
	search := c.historySearch
	if search == nil || len(search.matches) == 0 {
		return
	}
	current := 0
	if search.selected != nil {
		current = *search.selected
	}
	next := min(current+1, len(search.matches)-1)
	if direction < 0 {
		next = max(current-1, 0)
	}
	search.selected = &next
	text := search.matches[next].Text
	c.setInput(text)
	c.refillHistorySearch()
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
	search := c.historySearch
	if search == nil {
		return
	}
	c.historySearch = nil
	c.setInputPayload(PromptPayload{Text: search.originalInput, Images: search.originalImages})
	c.inputCursor = search.originalCursor
	c.updateAutocomplete()
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
	if modifiers.Contains(tea.ModAlt) && code == 'r' {
		c.cycleHistoryScope()
		return
	}
	if (modifiers.Contains(tea.ModCtrl) && code == 'r') || code == tea.KeyUp {
		c.stepHistorySearch(1)
		return
	}
	if (modifiers.Contains(tea.ModCtrl) && code == 's') || code == tea.KeyDown {
		c.stepHistorySearch(-1)
		return
	}
	if code == tea.KeyEscape || (modifiers.Contains(tea.ModCtrl) && code == 'c') {
		c.cancelHistorySearch()
		return
	}
	if code == tea.KeyEnter {
		c.acceptHistorySearch()
		return
	}
	changed := c.historySearch != nil &&
		c.historySearch.query.HandleKey(tea.KeyPressMsg{Code: code, Mod: modifiers}).Changed()
	if changed {
		c.refreshHistorySearch()
	}
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
	all := append(append([]string(nil), c.sessionHistory...), c.promptHistory...)
	seen := make(map[string]struct{}, len(all))
	var history []string
	for index := len(all) - 1; index >= 0; index-- {
		text := all[index]
		if _, duplicate := seen[text]; duplicate {
			continue
		}
		seen[text] = struct{}{}
		history = append(history, text)
	}
	slices.Reverse(history)
	return history
}

func (c *ChatState) moveHistory(delta int) {
	history := c.navigationHistory()
	if c.bundleID != nil && c.sessionHistoryError != nil {
		c.SetNotice(fmt.Sprintf("History unavailable: %s", *c.sessionHistoryError))
	}
	if len(history) == 0 {
		return
	}
	var next *int
	switch {
	case c.historyIndex == nil && delta < 0:
		c.historyDraft = c.input
		c.historyDraftImages = append([]PromptImage(nil), c.inputImages...)
		last := len(history) - 1
		next = &last
	case c.historyIndex == nil:
		next = nil
	case delta < 0:
		previous := max(*c.historyIndex-1, 0)
		next = &previous
	case *c.historyIndex+1 < len(history):
		following := *c.historyIndex + 1
		next = &following
	default:
		next = nil
	}
	if next != nil && *next < len(history) {
		// Stored history is text-only. Clear any tracked image ranges so
		// attachments from the temporary draft cannot follow unrelated text
		// into a submission.
		c.setInput(history[*next])
	} else {
		c.setInputPayload(PromptPayload{Text: c.historyDraft, Images: append([]PromptImage(nil), c.historyDraftImages...)})
	}
	c.historyIndex = next
	c.inputCursor = len(c.input)
	c.preferredColumn = nil
	c.updateAutocomplete()
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
	queryChars := utf8.RuneCountInString(search.query.Value())
	switch {
	case search.unavailable != nil:
		return "unavailable: " + *search.unavailable, ""
	case queryChars > 0 && queryChars < core.MinHistoryQueryChars:
		return fmt.Sprintf("type %d+ characters", core.MinHistoryQueryChars), " · Alt-R scope · Esc cancel"
	case queryChars > 0 && len(search.matches) == 0:
		if search.fetching {
			return "searching…", ""
		}
		return "no match", ""
	case search.selected != nil:
		more := ""
		if search.moreMayFollow() {
			more = "+"
		}
		return fmt.Sprintf("%d/%d%s", *search.selected+1, len(search.matches), more),
			" · Enter accept · Esc cancel · Alt-R scope"
	}
	return "type to search", " · Alt-R scope · Esc cancel"
}

func historySearchFooter(search *historySearch) string {
	status, hints := historySearchStatus(search)
	return fmt.Sprintf("reverse-i-search [%s]: %s  %s%s",
		historyScopeName(search.scope), search.query.String(), status, hints)
}

func highlightedInputLines(input string, query string) []Line {
	ranges := caseInsensitiveMatchRanges(input, query)
	var lines []Line
	lineStart := 0
	for _, line := range strings.Split(input, "\n") {
		lineEnd := lineStart + len(line)
		var spans []Span
		cursor := lineStart
		for _, r := range ranges {
			if !(r.Start < lineEnd && r.End > lineStart) {
				continue
			}
			start := max(r.Start, lineStart)
			end := min(r.End, lineEnd)
			if cursor < start {
				spans = append(spans, RawSpan(input[cursor:start]))
			}
			spans = append(spans, StyledSpan(input[start:end], Filled(ThemePalette().Warning).Add(ModifierBold)))
			cursor = end
		}
		if cursor < lineEnd {
			spans = append(spans, RawSpan(input[cursor:lineEnd]))
		}
		lines = append(lines, LineFrom(spans...))
		lineStart = lineEnd + 1
	}
	return lines
}

func caseInsensitiveMatchRanges(text string, query string) []ByteRange {
	if query == "" {
		return nil
	}
	query = rustToLowercase(query)
	type foldedSpan struct {
		folded   ByteRange
		original ByteRange
	}
	var folded strings.Builder
	var spans []foldedSpan
	for start, character := range text {
		original := ByteRange{Start: start, End: start + utf8.RuneLen(character)}
		for _, lower := range rustCharToLowercase(character) {
			foldedStart := folded.Len()
			folded.WriteRune(lower)
			spans = append(spans, foldedSpan{folded: ByteRange{Start: foldedStart, End: folded.Len()}, original: original})
		}
	}
	foldedText := folded.String()
	var ranges []ByteRange
	from := 0
	for from <= len(foldedText) {
		relative := strings.Index(foldedText[from:], query)
		if relative < 0 {
			break
		}
		start := from + relative
		end := start + len(query)
		first, last := -1, -1
		for _, span := range spans {
			if span.folded.End > start && span.folded.Start < end {
				first = span.original.Start
				break
			}
		}
		for index := len(spans) - 1; index >= 0; index-- {
			if span := spans[index]; span.folded.End > start && span.folded.Start < end {
				last = span.original.End
				break
			}
		}
		if first >= 0 && last >= 0 {
			ranges = append(ranges, ByteRange{Start: first, End: last})
		}
		from = end
	}
	return ranges
}

// rustCharToLowercase is Rust's `char::to_lowercase`: the full lowercase
// mapping, which for 'İ' is two characters.
func rustCharToLowercase(character rune) []rune {
	if character == 'İ' {
		return []rune{'i', '̇'}
	}
	return []rune{unicode.ToLower(character)}
}

// rustToLowercase lowercases each character as rustCharToLowercase does.
func rustToLowercase(text string) string {
	var lowered strings.Builder
	for _, character := range text {
		for _, lower := range rustCharToLowercase(character) {
			lowered.WriteRune(lower)
		}
	}
	return lowered.String()
}
