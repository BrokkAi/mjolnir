// Port of mj-store/src/database/schema.rs (its #[cfg(test)] items).

package store

import (
	"testing"
)

// minimumCompatibleVersion is the expected compatibility floor shared by
// migration regressions. Compatible migrations advance the revision without
// changing this contract. Revision 153 is breaking because it publishes
// references to cold transcript bodies. Older readers cannot resolve them and
// older writers cannot clear them.
const minimumCompatibleVersion int64 = 153

// forgetVerifiedSchema forgets that this process verified a database's
// schema. Only tests need it: they simulate a store written by an older
// build by editing the schema of a database this process has already opened,
// which no controller can do.
func forgetVerifiedSchema(path string) {
	verifiedSchemas.Lock()
	defer verifiedSchemas.Unlock()
	delete(verifiedSchemas.paths, schemaCacheKey(path))
}

func restorePre153TranscriptLayout(t testing.TB, connection *Connection) {
	t.Helper()
	schemaTestExecWhenColumn(t, connection, "materialized_transcript_items", "detail_offset", `DROP INDEX materialized_transcript_update_ordinal;
         DROP INDEX materialized_transcript_unsettled;
         ALTER TABLE materialized_transcript_items DROP COLUMN detail_offset;
         ALTER TABLE materialized_transcript_items DROP COLUMN detail_len;
         ALTER TABLE materialized_transcript_items DROP COLUMN item_kind;
         ALTER TABLE materialized_transcript_items DROP COLUMN settled;
         ALTER TABLE materialized_transcript_items DROP COLUMN update_ordinal;`)
}

func advanceTestSchema(t testing.TB, path string, revision int64, minimumCompatible int64) {
	t.Helper()
	panic("unported: mj-store/src/database/schema.rs advance_test_schema")
}

// dropEnvironmentSchema rewinds environment ownership and removes indexes on
// the columns it added.
func dropEnvironmentSchema(t testing.TB, connection *Connection) {
	t.Helper()
	panic("unported: mj-store/src/database/schema.rs drop_environment_schema")
}

func dropSessionRowTriggers(t testing.TB, connection *Connection) {
	t.Helper()
	dropEnvironmentSchema(t, connection)
	if _, err := connection.Exec(`DROP TRIGGER IF EXISTS subagent_accounting_kind_insert;
             DROP TRIGGER IF EXISTS subagent_accounting_kind_update;
             DROP TRIGGER IF EXISTS session_rows_deleted;
             DROP TRIGGER IF EXISTS session_rows_destroyed;
             DROP TRIGGER IF EXISTS mailbox_outbox_live_target;
             DROP TRIGGER IF EXISTS github_watch_items_live_creator;
             DROP TRIGGER IF EXISTS github_watch_classifications_live_session;
             DROP TRIGGER IF EXISTS github_watch_classification_failures_live_session;`); err != nil {
		t.Fatal(err)
	}
}

func restoreGithubV84Schema(t testing.TB, connection *Connection) {
	t.Helper()
	dropSessionRowTriggers(t, connection)
	schemaTestExecWhenColumn(t, connection, "github_watch_items", "timeline_at", `DROP TABLE IF EXISTS github_watch_classification_failures;
             DROP TABLE IF EXISTS github_seen_items;
             ALTER TABLE github_watch_cursors RENAME TO github_watch_cursors_new;
             CREATE TABLE github_watch_cursors (owner TEXT NOT NULL, repo TEXT NOT NULL,
                items_watermark_at TEXT, items_watermark_id INTEGER, items_etag TEXT, comments_cursor TEXT,
                PRIMARY KEY(owner, repo)) STRICT;
             INSERT INTO github_watch_cursors(owner, repo) SELECT owner, repo FROM github_watch_cursors_new;
             DROP TABLE github_watch_cursors_new;
             ALTER TABLE github_watch_items DROP COLUMN timeline_at;
             ALTER TABLE github_watch_items DROP COLUMN timeline_id;
             ALTER TABLE github_watch_items DROP COLUMN review_comments_at;
             ALTER TABLE github_watch_items DROP COLUMN review_comments_id;
             ALTER TABLE github_watch_items DROP COLUMN processed_at;
             ALTER TABLE github_watch_items ADD COLUMN pull_request_etag TEXT;
             ALTER TABLE github_watch_items ADD COLUMN pull_request_state TEXT;`)
}

// schemaTestExecWhenColumn runs batch on connection when table has column,
// the shape of the rewind helpers below.
func schemaTestExecWhenColumn(t testing.TB, connection *Connection, table string, column string, batch string) {
	t.Helper()
	has, err := tableHasColumn(connection, table, column)
	if err != nil {
		t.Fatal(err)
	}
	if !has {
		return
	}
	if _, err := connection.Exec(batch); err != nil {
		t.Fatal(err)
	}
}

// restorePre145ReviewColumns rewinds revision 145 in a current-schema
// database: the single `active` and `active_group` columns and a single
// `push_findings` object return, and `open_reviews` goes, so a historical
// fixture has the shape older builds wrote. Does nothing to a store that
// already has that shape.
func restorePre145ReviewColumns(t testing.TB, connection *Connection) {
	t.Helper()
	schemaTestExecWhenColumn(t, connection, "turn_review_state", "open_reviews", `ALTER TABLE turn_review_state ADD COLUMN active TEXT;
             ALTER TABLE turn_review_state ADD COLUMN active_group TEXT;
             UPDATE turn_review_state SET
                 active = json_extract(open_reviews, '$[0].active'),
                 active_group = json_extract(open_reviews, '$[0].group');
             UPDATE turn_review_state SET push_findings = json_extract(push_findings, '$[0]')
                 WHERE push_findings IS NOT NULL;
             ALTER TABLE turn_review_state DROP COLUMN open_reviews;`)
}

// restorePre101SubagentPreference rewinds the preference table in a
// current-schema database so an ad-hoc historical fixture has the singleton
// shape that existed before revision 101.
func restorePre101SubagentPreference(t testing.TB, connection *Connection) {
	t.Helper()
	schemaTestExecWhenColumn(t, connection, "subagent_preference", "profile_id", `ALTER TABLE subagent_preference RENAME TO subagent_preference_profiled;
             CREATE TABLE subagent_preference (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), policy TEXT NOT NULL CHECK(json_valid(policy)));
             INSERT INTO subagent_preference(singleton, policy)
                 SELECT 1, policy FROM subagent_preference_profiled WHERE profile_id='';
             DROP TABLE subagent_preference_profiled;`)
}

// restoreReviewStateSchema: historical fixtures must remove review columns
// introduced afterward.
func restoreReviewStateSchema(t testing.TB, connection *Connection, version int64) {
	t.Helper()
	panic("unported: mj-store/src/database/schema.rs restore_review_state_schema")
}

// restorePre116PromptHistory: historical fixtures must remove revision 116's
// prompt-history triggers and index. Revision 77 recreates `prompt_history`,
// and SQLite refuses that rename while triggers on other tables still name
// the dropped table.
func restorePre116PromptHistory(t testing.TB, connection *Connection, version int64) {
	t.Helper()
	panic("unported: mj-store/src/database/schema.rs restore_pre116_prompt_history")
}
