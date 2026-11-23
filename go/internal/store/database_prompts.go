// Port of mj-store/src/database/prompts.rs.

package store

import (
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RecordUserPrompt records a prompt a person is sending to a session, under
// the project the store holds for that session. Every surface a person types
// into records through here before it submits the prompt, so the record
// exists before the session can start or finish the turn, and nothing is
// left to do once the session accepts it. A prompt the session then refuses
// stays in the history, as an abandoned draft does. Agent messages do not
// record.
//
// commandID is the command that will carry the prompt; a retry with the same
// identity records nothing new.
func RecordUserPrompt(sessionID, commandID, text string) error {
	panic("unported: mj-store/src/database/prompts.rs record_user_prompt")
}

func recordUserPromptTo(path, sessionID, commandID, text string) error {
	panic("unported: mj-store/src/database/prompts.rs record_user_prompt_to")
}

func RecordAbandonedPrompt(sessionID, bundleID, text string) error {
	panic("unported: mj-store/src/database/prompts.rs record_abandoned_prompt")
}

// recordPromptTextTo stores text as one of the session's prompts. A prompt
// sent as a command is stored once per command; an abandoned draft, which
// has no command, is stored unless it equals the session's newest prompt.
func recordPromptTextTo(path, sessionID, bundleID string, commandID *string, text string) error {
	panic("unported: mj-store/src/database/prompts.rs record_prompt_text_to")
}

// SearchPromptPage is one page of prompt history matching query, smallest
// scope first.
//
// after is the Next cursor of the previous page, or nil to start.
func SearchPromptPage(sessionID, bundleID string, scope core.HistoryScope, query string, after *core.HistoryCursor, limit int) (core.PromptHistoryPage, error) {
	return searchPromptPageFrom(DatabasePath(), sessionID, bundleID, scope, query, after, limit)
}

// SessionPrompts is this session's stored prompts, newest first, each text
// once.
func SessionPrompts(sessionID string) ([]core.PromptHistoryEntry, error) {
	return sessionPromptsFrom(DatabasePath(), sessionID)
}

func sessionPromptsFrom(path, sessionID string) ([]core.PromptHistoryEntry, error) {
	panic("unported: mj-store/src/database/prompts.rs session_prompts_from")
}

// historyTier is where prompts come from, smallest scope first. The value is
// the tier's position in core.HistoryCursor's Tier.
type historyTier uint8

const (
	// historyTierSession is the asking session's own prompts.
	historyTierSession historyTier = 0
	// historyTierProject is other sessions in the asking session's project.
	historyTierProject historyTier = 1
	// historyTierElsewhere is sessions in every other project.
	historyTierElsewhere historyTier = 2
)

// historyTierOfScope is the tiers a scope reads, in the order their matches
// are listed (Rust's HistoryTier::of_scope).
func historyTierOfScope(scope core.HistoryScope) []historyTier {
	switch scope {
	case core.HistoryScopeSession:
		return []historyTier{historyTierSession}
	case core.HistoryScopeProject:
		return []historyTier{historyTierSession, historyTierProject}
	default:
		return []historyTier{historyTierSession, historyTierProject, historyTierElsewhere}
	}
}

// page is up to count of the tier's distinct matching prompts, newest first
// by each text's newest row, among texts whose newest row is older than
// position.Before. Only rows up to position.Through, the newest row when the
// search began, take part, so a prompt sent while the search is open cannot
// move an unseen text past the cursor. A text an earlier tier holds is left
// out, so every text appears in one tier and on one page. The full-text
// index drives each query, so the work follows the tier's matches, not its
// rows.
func (t historyTier) page(conn Queryer, phrase, sessionID, bundleID string, position core.HistoryCursor, count int) ([]core.PromptHistoryEntry, error) {
	panic("unported: mj-store/src/database/prompts.rs HistoryTier::page")
}

// fulltextPhrase is query as one full-text phrase, which the trigram index
// matches as a substring ignoring case. false when it is too short for the
// index.
func fulltextPhrase(query string) (string, bool) {
	if utf8.RuneCountInString(query) < core.MinHistoryQueryChars {
		return "", false
	}
	return `"` + strings.ReplaceAll(query, `"`, `""`) + `"`, true
}

// searchPromptPageFrom is the one prompt search behind the terminal composer
// and the web viewer.
//
// Tiers are read in order from the cursor, each with one query for the
// matches it still needs plus one, and the search stops as soon as it holds
// limit matches. Each prompt text is listed once across all pages, at its
// smallest scope.
func searchPromptPageFrom(path, sessionID, bundleID string, scope core.HistoryScope, query string, after *core.HistoryCursor, limit int) (core.PromptHistoryPage, error) {
	panic("unported: mj-store/src/database/prompts.rs search_prompt_page_from")
}

func queryHistoryPage(conn Queryer, query string, parameters ...any) ([]core.PromptHistoryEntry, error) {
	panic("unported: mj-store/src/database/prompts.rs query_history_page")
}
