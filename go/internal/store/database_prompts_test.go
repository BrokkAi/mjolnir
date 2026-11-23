// Port of mj-store/src/database/prompts.rs (its #[cfg(test)] items).

package store

import (
	"fmt"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recordPromptTo records a delivered prompt keyed by its event ordinal, for
// fixtures that need several rows of the same text. submittedAt nil records
// the current time.
func recordPromptTo(path, sessionID, bundleID string, eventOrdinal uint64, submittedAt *string, text string) error {
	if strings.TrimSpace(text) == "" {
		return nil
	}
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
	contextCreated := "unknown"
	if submittedAt != nil {
		contextCreated = *submittedAt
	}
	if _, err := tx.Exec(`INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING`, sessionID, canonicalBundle, contextCreated); err != nil {
		return err
	}
	var actualBundle string
	if err := tx.QueryRow("SELECT bundle_id FROM session_contexts WHERE session_id = ?1", sessionID).Scan(&actualBundle); err != nil {
		return err
	}
	if actualBundle != canonicalBundle {
		return fmt.Errorf("session %s belongs to bundle %s, not %s", sessionID, actualBundle, bundleID)
	}
	stamp := core.FormatChronoRFC3339(time.Now())
	if submittedAt != nil {
		stamp = *submittedAt
	}
	if _, err := tx.Exec(`INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id, event_ordinal) DO NOTHING`, sessionID, int64(eventOrdinal), stamp, text); err != nil {
		return err
	}
	return tx.Commit()
}

// searchPromptsFrom is every match in one page, for tests that check what a
// search finds rather than how it pages.
func searchPromptsFrom(path, sessionID, bundleID string, scope core.HistoryScope, query string) ([]core.PromptHistoryEntry, error) {
	page, err := searchPromptPageFrom(path, sessionID, bundleID, scope, query, nil, 10_000)
	if err != nil {
		return nil, err
	}
	if page.Next != nil {
		panic("the test history fits in one page")
	}
	return page.Entries, nil
}
