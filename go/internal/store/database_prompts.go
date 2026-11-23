// Port of mj-store/src/database/prompts.rs.

package store

import (
	"database/sql"
	"fmt"
	"math"
	"strings"
	"time"
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
	_, err := submitDatabaseWrite("record_user_prompt", func(*Connection) (struct{}, error) {
		return struct{}{}, recordUserPromptTo(DatabasePath(), sessionID, commandID, text)
	})
	return err
}

func recordUserPromptTo(path, sessionID, commandID, text string) error {
	reader, err := openReader(path)
	if err != nil {
		return err
	}
	var bundleID string
	found, err := valuesOptional(reader.QueryRow("SELECT bundle_id FROM session_contexts WHERE session_id = ?1", sessionID).Scan(&bundleID))
	reader.Close()
	if err != nil {
		return err
	}
	// A session with no context has no row to keep history under; the
	// history trigger would ignore the prompt anyway.
	if !found {
		return nil
	}
	return recordPromptTextTo(path, sessionID, bundleID, &commandID, text)
}

func RecordAbandonedPrompt(sessionID, bundleID, text string) error {
	_, err := submitDatabaseWrite("record_abandoned_prompt", func(*Connection) (struct{}, error) {
		return struct{}{}, recordPromptTextTo(DatabasePath(), sessionID, bundleID, nil, text)
	})
	return err
}

// recordPromptTextTo stores text as one of the session's prompts. A prompt
// sent as a command is stored once per command; an abandoned draft, which
// has no command, is stored unless it equals the session's newest prompt.
func recordPromptTextTo(path, sessionID, bundleID string, commandID *string, text string) error {
	if strings.TrimSpace(text) == "" {
		return nil
	}
	submittedAt := core.FormatChronoRFC3339(time.Now())
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	canonicalBundle, err := sessionProjectID(tx, sessionID, bundleID)
	if err != nil {
		return err
	}
	if _, err := tx.Exec(`INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING`, sessionID, canonicalBundle, submittedAt); err != nil {
		return err
	}
	var actualBundle string
	if err := tx.QueryRow("SELECT bundle_id FROM session_contexts WHERE session_id = ?1", sessionID).Scan(&actualBundle); err != nil {
		return err
	}
	if actualBundle != canonicalBundle {
		return fmt.Errorf("session %s belongs to bundle %s, not %s", sessionID, actualBundle, bundleID)
	}
	if commandID != nil {
		if _, err := tx.Exec(`INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text, command_id)
             VALUES (?1, NULL, ?2, ?3, ?4)
             ON CONFLICT(session_id, command_id) WHERE command_id IS NOT NULL DO NOTHING`,
			sessionID, submittedAt, text, *commandID); err != nil {
			return err
		}
	} else {
		var mostRecent string
		found, err := valuesOptional(tx.QueryRow(`SELECT text FROM prompt_history WHERE session_id = ?1
                 ORDER BY history_id DESC LIMIT 1`, sessionID).Scan(&mostRecent))
		if err != nil {
			return err
		}
		if !found || mostRecent != text {
			if _, err := tx.Exec(`INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
                 VALUES (?1, NULL, ?2, ?3)`, sessionID, submittedAt, text); err != nil {
				return err
			}
		}
	}
	return tx.Commit()
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
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return queryHistoryPage(connection, `SELECT max(history_id) AS newest, session_id, text FROM prompt_history
         WHERE session_id = ?1 GROUP BY text ORDER BY newest DESC`, sessionID)
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
	before, through := position.Before, position.Through
	limit := int64(count)
	if uint64(count) > math.MaxInt64 {
		limit = math.MaxInt64
	}
	switch t {
	case historyTierSession:
		// The unary `+` keeps SQLite from driving this query from the
		// session's history index and probing the full-text index once per
		// row, which took 75 ms for a 291-prompt session instead of 6 ms.
		return queryHistoryPage(conn, `SELECT max(p.history_id) AS newest, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid <= ?5 AND +p.session_id = ?2
                 GROUP BY p.text HAVING newest < ?3
                 ORDER BY newest DESC LIMIT ?4`, phrase, sessionID, before, limit, through)
	case historyTierProject:
		return queryHistoryPage(conn, `SELECT max(p.history_id) AS newest, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid <= ?6 AND p.session_id <> ?2
                   AND EXISTS (SELECT 1 FROM session_contexts c
                               WHERE c.session_id = p.session_id AND c.bundle_id = ?3)
                   AND p.text NOT IN (SELECT text FROM prompt_history
                                      WHERE session_id = ?2 AND history_id <= ?6)
                 GROUP BY p.text HAVING newest < ?4
                 ORDER BY newest DESC LIMIT ?5`, phrase, sessionID, bundleID, before, limit, through)
	default:
		return queryHistoryPage(conn, `SELECT max(p.history_id) AS newest, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid <= ?6 AND p.session_id <> ?2
                   AND NOT EXISTS (SELECT 1 FROM session_contexts c
                                   WHERE c.session_id = p.session_id AND c.bundle_id = ?3)
                   AND p.text NOT IN (
                       SELECT h.text
                       FROM prompt_history_text g JOIN prompt_history h ON h.history_id = g.rowid
                       WHERE prompt_history_text MATCH ?1 AND g.rowid <= ?6
                         AND (h.session_id = ?2
                              OR EXISTS (SELECT 1 FROM session_contexts d
                                         WHERE d.session_id = h.session_id
                                           AND d.bundle_id = ?3)))
                 GROUP BY p.text HAVING newest < ?4
                 ORDER BY newest DESC LIMIT ?5`, phrase, sessionID, bundleID, before, limit, through)
	}
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
	phrase, ok := fulltextPhrase(query)
	if !ok {
		return core.PromptHistoryPage{Entries: []core.PromptHistoryEntry{}}, nil
	}
	connection, err := openReader(path)
	if err != nil {
		return core.PromptHistoryPage{}, err
	}
	defer connection.Close()
	canonicalBundle, err := sessionProjectID(connection, sessionID, bundleID)
	if err != nil {
		return core.PromptHistoryPage{}, err
	}
	var start core.HistoryCursor
	if after != nil {
		start = *after
	} else {
		start = core.HistoryCursor{Tier: uint8(historyTierSession), Before: math.MaxInt64}
		if err := connection.QueryRow("SELECT COALESCE(MAX(history_id), 0) FROM prompt_history").Scan(&start.Through); err != nil {
			return core.PromptHistoryPage{}, err
		}
	}
	entries := []core.PromptHistoryEntry{}
	for _, tier := range historyTierOfScope(scope) {
		if uint8(tier) < start.Tier {
			continue
		}
		before := int64(math.MaxInt64)
		if uint8(tier) == start.Tier {
			before = start.Before
		}
		wanted := max(limit-len(entries), 0)
		// One past what is still needed shows whether more exist.
		rows, err := tier.page(connection, phrase, sessionID, canonicalBundle,
			core.HistoryCursor{Tier: uint8(tier), Before: before, Through: start.Through}, wanted+1)
		if err != nil {
			return core.PromptHistoryPage{}, err
		}
		if len(rows) > wanted {
			rows = rows[:wanted]
			if len(rows) > 0 {
				before = rows[len(rows)-1].ID
			}
			entries = append(entries, rows...)
			return core.PromptHistoryPage{
				Entries: entries,
				Next:    &core.HistoryCursor{Tier: uint8(tier), Before: before, Through: start.Through},
			}, nil
		}
		entries = append(entries, rows...)
	}
	return core.PromptHistoryPage{Entries: entries}, nil
}

func queryHistoryPage(conn Queryer, query string, parameters ...any) ([]core.PromptHistoryEntry, error) {
	entries, err := valuesQueryMap(conn, query, parameters, func(rows *sql.Rows) (core.PromptHistoryEntry, error) {
		var entry core.PromptHistoryEntry
		err := rows.Scan(&entry.ID, &entry.SessionID, &entry.Text)
		return entry, err
	})
	if err != nil {
		return nil, err
	}
	if entries == nil {
		entries = []core.PromptHistoryEntry{}
	}
	return entries, nil
}
