// Port of mj-store/src/database/tests.rs, continued (prompt history search
// and concurrent session writes).

package store

import (
	"database/sql"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func dtPromptTexts(entries []core.PromptHistoryEntry) []string {
	texts := []string{}
	for _, entry := range entries {
		texts = append(texts, entry.Text)
	}
	return texts
}

func TestHistorySearchScopesByProjectSessionAndAllProjects(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	liveSessions(t, database, [][2]string{
		{"session-1", "project-1"},
		{"session-2", "project-1"},
		{"session-3", "project-2"},
	})
	for _, prompt := range []struct {
		session, bundle string
		sequence        uint64
		text            string
	}{
		{"session-1", "project-1", 1, "fix parser"},
		{"session-2", "project-1", 1, "fix renderer"},
		{"session-3", "project-2", 1, "fix database"},
		{"session-1", "project-1", 2, "fix parser"},
	} {
		dtOK(t, recordPromptTo(database, prompt.session, prompt.bundle, prompt.sequence, dtPtr("2026-08-12T00:00:00Z"), prompt.text))
	}

	project := dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeProject, "FIX"))
	dtEqual(t, dtPromptTexts(project), []string{"fix parser", "fix renderer"})
	session := dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeSession, "parser"))
	dtEqual(t, len(session), 1, "duplicate prompt text is suppressed")
	all := dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeAll, "database"))
	dtEqual(t, all[0].SessionID, "session-3")
}

func TestWiderHistoryScopesListSmallerScopesFirstAndDedupeAtTheSmallest(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	liveSessions(t, database, [][2]string{
		{"session-1", "project-1"},
		{"session-2", "project-1"},
		{"session-3", "project-2"},
	})
	// Oldest to newest. Date order alone would put "alpha elsewhere" and
	// "alpha project" ahead of this session's older prompt.
	for ordinal, prompt := range [][3]string{
		{"session-1", "project-1", "alpha session old"},
		{"session-2", "project-1", "alpha project"},
		{"session-3", "project-2", "alpha elsewhere"},
		{"session-2", "project-1", "alpha session old"},
		{"session-1", "project-1", "alpha session new"},
	} {
		dtOK(t, recordPromptTo(database, prompt[0], prompt[1], uint64(ordinal), dtPtr("2026-08-12T00:00:00Z"), prompt[2]))
	}
	search := func(scope core.HistoryScope, query string) []string {
		return dtPromptTexts(dtMust(searchPromptsFrom(database, "session-1", "project-1", scope, query)))
	}
	texts := func(scope core.HistoryScope) []string { return search(scope, "alpha") }
	textsFor := func(query string) []string { return search(core.HistoryScopeAll, query) }

	dtEqual(t, texts(core.HistoryScopeAll), []string{
		"alpha session new",
		"alpha session old",
		"alpha project",
		"alpha elsewhere",
	})
	dtEqual(t, texts(core.HistoryScopeProject), []string{"alpha session new", "alpha session old", "alpha project"})
	dtEqual(t, texts(core.HistoryScopeSession), []string{"alpha session new", "alpha session old"})

	// Paging two at a time from each `next` cursor yields the same sequence,
	// with no gaps and no repeats, including across tier boundaries, even
	// when an unseen prompt is sent again while the search is open.
	expected := texts(core.HistoryScopeAll)
	paged := []string{}
	var after *core.HistoryCursor
	resent := false
	for {
		page := dtMust(searchPromptPageFrom(database, "session-1", "project-1", core.HistoryScopeAll, "alpha", after, 2))
		dtTrue(t, len(page.Entries) <= 2)
		paged = append(paged, dtPromptTexts(page.Entries)...)
		if !resent {
			// Hard-won: 8e9b542d5: a text resent between pages moved past the cursor and was never listed
			dtOK(t, recordPromptTo(database, "session-1", "project-1", 50, dtPtr("2026-08-12T00:00:00Z"), "alpha elsewhere"))
			resent = true
		}
		if page.Next == nil {
			break
		}
		after = page.Next
	}
	dtEqual(t, paged, expected)

	dtEqual(t, len(textsFor("al")), 0, "too short for the trigram index")
	dtEqual(t, len(textsFor(`say "alpha"`)), 0)
}

func TestAbandonedPromptsAreSearchableInSessionAndProjectHistory(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	liveSessions(t, database, [][2]string{{"session-1", "project-1"}, {"session-2", "project-1"}})
	dtOK(t, recordPromptTextTo(database, "session-1", "project-1", nil, "unfinished draft"))
	dtOK(t, recordPromptTo(database, "session-2", "project-1", 1, dtPtr("2026-08-12T00:00:00Z"), "another session prompt"))

	connection := dtMust(openReader(database))
	ordinal := dtQuery[sql.NullInt64](t, connection, "SELECT event_ordinal FROM prompt_history WHERE session_id = 'session-1'")
	dtTrue(t, !ordinal.Valid)
	connection.Close()

	session := dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeSession, "unfinished"))
	dtEqual(t, len(session), 1)
	dtEqual(t, session[0].Text, "unfinished draft")

	project := dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeProject, "prompt"))
	dtEqual(t, dtPromptTexts(project), []string{"another session prompt"})
	projectAbandoned := dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeProject, "unfinished"))
	dtEqual(t, projectAbandoned[0].SessionID, "session-1")
}

func TestAbandonedPromptDeduplicationUsesTheLatestRowAndKeepsDeliveredDedupe(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	liveSessions(t, database, [][2]string{
		{"session-1", "project-1"},
		{"session-2", "project-1"},
		{"session-3", "project-1"},
	})
	// Hard-won: 9d9cc05e2: a retried command recorded its prompt again and moved it to the front
	for _, prompt := range [][2]string{{"cmd-1", "first"}, {"cmd-2", "second"}, {"cmd-1", "first"}} {
		dtOK(t, recordUserPromptTo(database, "session-3", prompt[0], prompt[1]))
	}
	dtEqual(t, dtPromptTexts(dtMust(sessionPromptsFrom(database, "session-3"))), []string{"second", "first"},
		"the retry is the same prompt, not a newer one")
	dtOK(t, recordPromptTo(database, "session-1", "project-1", 7, dtPtr("2026-08-12T00:00:00Z"), "submitted prompt"))
	dtOK(t, recordPromptTextTo(database, "session-1", "project-1", nil, "submitted prompt"))
	dtOK(t, recordPromptTextTo(database, "session-1", "project-1", nil, "unfinished draft"))
	dtOK(t, recordPromptTextTo(database, "session-1", "project-1", nil, "unfinished draft"))
	// The delivered prompt's ordinal remains its idempotency key after the
	// nullable-ordinal migration.
	dtOK(t, recordPromptTo(database, "session-1", "project-1", 7, dtPtr("2026-08-12T00:00:00Z"), "submitted prompt"))

	connection := dtOpenReader(t, database)
	type historyRow struct {
		ordinal *int64
		text    string
	}
	rows, err := connection.Query(`SELECT event_ordinal, text FROM prompt_history
             WHERE session_id = 'session-1' ORDER BY history_id`)
	dtOK(t, err)
	var found []historyRow
	for rows.Next() {
		var ordinal sql.NullInt64
		var text string
		dtOK(t, rows.Scan(&ordinal, &text))
		row := historyRow{text: text}
		if ordinal.Valid {
			row.ordinal = &ordinal.Int64
		}
		found = append(found, row)
	}
	dtOK(t, rows.Err())
	dtOK(t, rows.Close())
	dtEqual(t, found, []historyRow{
		{dtPtr(int64(7)), "submitted prompt"},
		{nil, "unfinished draft"},
	})

	dtOK(t, recordPromptTextTo(database, "session-2", "project-1", nil, " \n\t "))
	dtEqual(t, len(dtMust(sessionPromptsFrom(database, "session-2"))), 0)
}

func TestRebindingASessionMovesItsPromptHistoryToTheNewBundle(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	liveSessions(t, database, [][2]string{{"session-1", "project-1"}})
	dtOK(t, recordPromptTo(database, "session-1", "project-1", 1, dtPtr("2026-08-12T00:00:00Z"), "fix parser"))

	dtMust(rebindSessionBundleTo(database, "session-1", "project-2"))

	dtEqual(t, len(dtMust(searchPromptsFrom(database, "other-session", "project-1", core.HistoryScopeProject, "fix"))), 0)
	dtEqual(t, len(dtMust(searchPromptsFrom(database, "session-1", "project-2", core.HistoryScopeProject, "fix"))), 1)
	dtEqual(t, len(dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeProject, "fix"))), 1,
		"the old worker ID follows its own rebound context")
	// Recording under the new bundle now succeeds where it would have been
	// refused as a bundle mismatch.
	dtOK(t, recordPromptTo(database, "session-1", "project-2", 2, dtPtr("2026-08-12T00:01:00Z"), "fix renderer"))
}

func TestIndependentSessionWritesPreserveBothUpdates(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	for _, entry := range [][2]string{{"session-1", "project-1"}, {"session-2", "project-2"}} {
		record := session(entry[0], entry[1])
		dtOK(t, SaveSessionTo(database, &record))
	}

	var wait sync.WaitGroup
	failures := make(chan error, 2)
	for _, entry := range [][3]string{
		{"session-1", "project-1", "first changed"},
		{"session-2", "project-2", "second changed"},
	} {
		wait.Go(func() {
			record := session(entry[0], entry[1])
			record.SessionTitleOverride = dtPtr(entry[2])
			if err := SaveSessionTo(database, &record); err != nil {
				failures <- err
			}
		})
	}
	wait.Wait()
	close(failures)
	for err := range failures {
		dtOK(t, err)
	}

	state := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(state, "session-1").SessionTitleOverride, dtPtr("first changed"))
	dtEqual(t, dtSessionOf(state, "session-2").SessionTitleOverride, dtPtr("second changed"))
}
