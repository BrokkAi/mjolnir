// Port of mj-store/src/database/schema.rs (its #[cfg(test)] items).

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	"modernc.org/sqlite"
	sqlite3 "modernc.org/sqlite/lib"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	connection := schemaTestOpen(t, path)
	defer connection.Close()
	transaction, err := connection.Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer transaction.Rollback()
	schemaTestExec(t, transaction, "UPDATE schema_compatibility SET minimum_compatible_version = ?1", minimumCompatible)
	schemaTestExec(t, transaction, "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, 'test')", revision)
	schemaTestExec(t, transaction, fmt.Sprintf("PRAGMA user_version = %d", revision))
	if err := transaction.Commit(); err != nil {
		t.Fatal(err)
	}
	forgetVerifiedSchema(path)
}

// dropEnvironmentSchema rewinds environment ownership and removes indexes on
// the columns it added.
func dropEnvironmentSchema(t testing.TB, connection *Connection) {
	t.Helper()
	// These historical fixtures also predate removal of the size columns.
	if err := ensureSessionContainerOverrideColumns(connection); err != nil {
		t.Fatal(err)
	}
	schemaTestExec(t, connection, `DROP INDEX IF EXISTS session_moves_source_environment;
             DROP INDEX IF EXISTS retained_move_sources_environment;`)
	for _, column := range [][2]string{
		{"session_moves", "source_environment_id"},
		{"retained_move_sources", "environment_id"},
	} {
		table, column := column[0], column[1]
		schemaTestExecWhenColumn(t, connection, table, column, fmt.Sprintf("ALTER TABLE %s DROP COLUMN %s;", table, column))
	}
	schemaTestExec(t, connection, `DROP TRIGGER IF EXISTS environment_report_registered;
        DROP TRIGGER IF EXISTS environment_report_updated;
        DROP TRIGGER IF EXISTS reclaimed_environment_owner_deleted;
        DROP TRIGGER IF EXISTS reclaimed_environment_binding_deleted;
        DROP TABLE IF EXISTS environment_reports;
        DROP TABLE IF EXISTS environment_bindings;
        DROP TABLE IF EXISTS environment_workers;
        DROP TABLE IF EXISTS environments;`)
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
	if version < 145 {
		restorePre145ReviewColumns(t, connection)
	}
	// Rewinding the ledger must also restore the constraint removed by 126,
	// otherwise historical harness-admission migrations see a modern table.
	if version < 126 {
		schemaTestExec(t, connection, `PRAGMA writable_schema=ON;
            UPDATE sqlite_schema SET sql=replace(sql, 'harness_kind TEXT NOT NULL',
                'harness_kind TEXT NOT NULL CHECK(harness_kind IN (''codex'',''claude'',''kimi'',''grok'',''deepseek'',''muse'',''opencode'',''mimo'',''dsh''))')
            WHERE name IN ('sessions','hidden_native_sessions') AND sql NOT LIKE '%CHECK(harness_kind IN (%';
            PRAGMA writable_schema=RESET;`)
	}

	if version < 122 {
		dropEnvironmentSchema(t, connection)
	}
	if version < 121 {
		dropSessionRowTriggers(t, connection)
	}
	if version < 119 {
		schemaTestExecWhenColumn(t, connection, "sessions", "no_github_credentials",
			"ALTER TABLE sessions DROP COLUMN no_github_credentials;")
	}
	restorePre116PromptHistory(t, connection, version)
	if version < 114 {
		schemaTestExecWhenColumn(t, connection, "turn_review_state", "declined_findings",
			"ALTER TABLE turn_review_state DROP COLUMN declined_findings;")
	}
	if version < 109 {
		for _, column := range []string{"pending_followup", "forward_attempts"} {
			schemaTestExecWhenColumn(t, connection, "turn_review_state", column,
				fmt.Sprintf("ALTER TABLE turn_review_state DROP COLUMN %s;", column))
		}
	}
	if version < 106 {
		schemaTestExecWhenColumn(t, connection, "turn_review_state", "active_group",
			"ALTER TABLE turn_review_state DROP COLUMN active_group;")
	}
	if version < 104 {
		schemaTestExecWhenColumn(t, connection, "turn_review_state", "push_findings",
			"ALTER TABLE turn_review_state DROP COLUMN push_findings;")
	}
}

// restorePre116PromptHistory: historical fixtures must remove revision 116's
// prompt-history triggers and index. Revision 77 recreates `prompt_history`,
// and SQLite refuses that rename while triggers on other tables still name
// the dropped table.
func restorePre116PromptHistory(t testing.TB, connection *Connection, version int64) {
	t.Helper()
	if version < 116 {
		schemaTestExec(t, connection, `DROP TRIGGER IF EXISTS prompt_history_top_level_only;
                 DROP TRIGGER IF EXISTS prompt_history_child_registered;
                 DROP TRIGGER IF EXISTS prompt_history_session_destroyed;
                 DROP TRIGGER IF EXISTS prompt_history_text_insert;
                 DROP TRIGGER IF EXISTS prompt_history_text_delete;
                 DROP TRIGGER IF EXISTS prompt_history_text_update;
                 DROP TABLE IF EXISTS prompt_history_text;
                 DROP INDEX IF EXISTS prompt_history_command;`)
		schemaTestExecWhenColumn(t, connection, "prompt_history", "command_id",
			"ALTER TABLE prompt_history DROP COLUMN command_id;")
	}
}

// --- helpers for the reader_tests port ---

// schemaTestOpen is rusqlite's Connection::open for a test.
func schemaTestOpen(t testing.TB, path string) *Connection {
	t.Helper()
	connection, err := OpenConnection(path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = connection.Close() })
	return connection
}

// schemaTestWriter is open_writer(path).unwrap().
func schemaTestWriter(t testing.TB, path string) *Connection {
	t.Helper()
	connection, err := openWriter(path)
	if err != nil {
		t.Fatalf("open writer %s: %v", path, err)
	}
	t.Cleanup(func() { _ = connection.Close() })
	return connection
}

func schemaTestExec(t testing.TB, connection Queryer, query string, args ...any) {
	t.Helper()
	if _, err := connection.Exec(query, args...); err != nil {
		t.Fatalf("%v\n%s", err, query)
	}
}

// schemaTestRow scans the one row query returns into targets.
func schemaTestRow(t testing.TB, connection Queryer, query string, args []any, targets ...any) {
	t.Helper()
	if err := connection.QueryRow(query, args...).Scan(targets...); err != nil {
		t.Fatalf("%v\n%s", err, query)
	}
}

func schemaTestInt(t testing.TB, connection Queryer, query string, args ...any) int64 {
	t.Helper()
	var value int64
	schemaTestRow(t, connection, query, args, &value)
	return value
}

func schemaTestText(t testing.TB, connection Queryer, query string, args ...any) string {
	t.Helper()
	var value string
	schemaTestRow(t, connection, query, args, &value)
	return value
}

func schemaTestOptionalText(t testing.TB, connection Queryer, query string, args ...any) *string {
	t.Helper()
	var value sql.NullString
	schemaTestRow(t, connection, query, args, &value)
	if !value.Valid {
		return nil
	}
	return &value.String
}

func schemaTestBool(t testing.TB, connection Queryer, query string, args ...any) bool {
	t.Helper()
	var value bool
	schemaTestRow(t, connection, query, args, &value)
	return value
}

func schemaTestTexts(t testing.TB, connection Queryer, query string, args ...any) []string {
	t.Helper()
	rows, err := connection.Query(query, args...)
	if err != nil {
		t.Fatalf("%v\n%s", err, query)
	}
	values := []string{}
	for rows.Next() {
		var value string
		if err := rows.Scan(&value); err != nil {
			t.Fatal(err)
		}
		values = append(values, value)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		t.Fatal(err)
	}
	return values
}

func schemaTestExists(t testing.TB, connection Queryer, query string, args ...any) bool {
	t.Helper()
	exists, err := schemaRowExists(connection, query, args...)
	if err != nil {
		t.Fatalf("%v\n%s", err, query)
	}
	return exists
}

func schemaTestHasColumn(t testing.TB, connection Queryer, table string, column string) bool {
	t.Helper()
	has, err := tableHasColumn(connection, table, column)
	if err != nil {
		t.Fatal(err)
	}
	return has
}

func schemaTestState(t testing.TB, connection *Connection) schemaState {
	t.Helper()
	state, err := readSchemaState(connection)
	if err != nil {
		t.Fatal(err)
	}
	return state
}

// schemaTestFloor is a schema state's floor, or -1 when it has none.
func schemaTestFloor(state schemaState) int64 {
	if state.minimumCompatible == nil {
		return -1
	}
	return *state.minimumCompatible
}

// schemaTestExpectState fails unless state is (revision, Some(floor)), or
// (revision, None) for a negative floor.
func schemaTestExpectState(t testing.TB, state schemaState, revision int64, floor int64) {
	t.Helper()
	if state.revision != revision || schemaTestFloor(state) != floor {
		t.Fatalf("schema state (%d, %d), want (%d, %d)", state.revision, schemaTestFloor(state), revision, floor)
	}
}

// schemaTestRollbackIfOpen is `if !connection.is_autocommit() { ROLLBACK }`.
func schemaTestRollbackIfOpen(t testing.TB, connection *Connection) {
	t.Helper()
	if !connection.isAutocommit() {
		schemaTestExec(t, connection, "ROLLBACK")
	}
}

// schemaTestMismatch is `error.downcast_ref::<StoreSchemaMismatch>()`, and
// `error.chain().find_map(...)`: Go's errors.As walks the whole chain.
func schemaTestMismatch(t testing.TB, err error) core.StoreSchemaMismatch {
	t.Helper()
	var mismatch core.StoreSchemaMismatch
	if !errors.As(err, &mismatch) {
		t.Fatalf("expected a StoreSchemaMismatch, got %v", err)
	}
	return mismatch
}

// schemaTestIncompatible fails unless err is a mismatch whose reason is
// Incompatible { minimum_compatible: floor }.
func schemaTestIncompatible(t testing.TB, err error, floor int64) {
	t.Helper()
	if reason := schemaTestMismatch(t, err).Reason; reason != (core.StoreSchemaMismatchReasonIncompatible{MinimumCompatible: floor}) {
		t.Fatalf("mismatch reason %#v, want Incompatible{%d}", reason, floor)
	}
}

func schemaTestMustFail(t testing.TB, err error, what string) {
	t.Helper()
	if err == nil {
		t.Fatalf("%s: expected an error", what)
	}
}

func schemaTestMust(t testing.TB, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func schemaTestSaveSession(t testing.TB, path string, record core.SessionRecord) {
	t.Helper()
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
}

func schemaTestLoadState(t testing.TB, path string) core.StateState {
	t.Helper()
	state, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	return state
}

func schemaTestSession(t testing.TB, state core.StateState, id string) core.SessionRecord {
	t.Helper()
	record, ok := state.Sessions.Get(id)
	if !ok {
		t.Fatalf("session %s is missing", id)
	}
	return record
}

func schemaTestJSON(t testing.TB, value any) string {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func TestSchema152FoldsOnlyMjolnirPoliciesAndFencesRevision151(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "orchestrator-policies.sqlite")
	schemaTestWriter(t, path).Close()
	stampSchemaVersion(t, path, 151)
	connection := schemaTestOpen(t, path)
	mjolnir := `{"mode":"mjolnir","subtle":{"model":"preserved","effort":"high"}}`
	native := `{"mode":"native"}`
	none := `{"mode":"none"}`
	transaction, err := connection.Begin()
	schemaTestMust(t, err)
	defer transaction.Rollback()
	for _, c := range []struct {
		id      string
		policy  *string
		enabled bool
	}{
		{"mjolnir", &mjolnir, true},
		{"disabled", &mjolnir, false},
		{"native", &native, true},
		{"none", &none, true},
		{"absent", nil, true},
	} {
		record := session(c.id, "project")
		schemaTestMust(t, insertSession(transaction, &record))
		schemaTestExec(t, transaction, `UPDATE sessions SET subagents = ?2, supervisors = ?3 WHERE session_id = ?1`, c.id, c.policy, c.enabled)
		schemaTestExec(t, transaction, `INSERT INTO supervisor_preference(profile_id, enabled) VALUES (?1, ?2)`, c.id, c.enabled)
		if c.policy != nil {
			schemaTestExec(t, transaction, `INSERT INTO subagent_preference(profile_id, policy) VALUES (?1, ?2)`, c.id, *c.policy)
		}
	}
	schemaTestExec(t, transaction, `CREATE TRIGGER interrupt_152 BEFORE INSERT ON schema_migrations WHEN NEW.version=152
             BEGIN SELECT RAISE(ABORT, 'fixture interruption'); END;
             CREATE TRIGGER stop_at_152 BEFORE INSERT ON schema_migrations WHEN NEW.version > 152
             BEGIN SELECT RAISE(ABORT, 'fixture revision 152 boundary'); END;`)
	schemaTestMust(t, transaction.Commit())

	schemaTestMustFail(t, migrateSchema(connection), "the interrupted migration")
	before := schemaTestState(t, connection)
	schemaTestExpectState(t, before, 151, 151)
	previous, err := loadStateWith(connection)
	schemaTestMust(t, err)
	if policy := schemaTestSession(t, previous, "mjolnir").Subagents; policy == nil ||
		policy.Mode != core.SubagentPolicyMjolnir || policy.Orchestrators {
		t.Fatalf("revision 151 policy %#v", policy)
	}
	schemaTestExec(t, connection, "DROP TRIGGER interrupt_152")
	err = migrateSchema(connection)
	if err == nil || !strings.Contains(err.Error(), "fixture revision 152 boundary") {
		t.Fatalf("migration to the boundary: %v", err)
	}

	schema := schemaTestState(t, connection)
	schemaTestExpectState(t, schema, 152, 152)
	schemaTestMustFail(t, schema.ensureSupportedBy(151), "a revision-151 build")
	state, err := loadStateWith(connection)
	schemaTestMust(t, err)
	migrated := schemaTestSession(t, state, "mjolnir").Subagents
	if migrated == nil || migrated.Mode != core.SubagentPolicyMjolnir || !migrated.Orchestrators {
		t.Fatalf("migrated policy %#v", migrated)
	}
	if choice := migrated.OverrideChoice(core.SubagentTaskTypeSubtle); choice == nil || choice.Model != "preserved" {
		t.Fatalf("subtle choice %#v", choice)
	}
	if remembered := state.LastSubagentPolicies["mjolnir"]; !reflect.DeepEqual(remembered, *migrated) {
		t.Fatalf("remembered policy %#v, want %#v", remembered, *migrated)
	}
	for _, id := range []string{"native", "none", "disabled", "absent"} {
		if !reflect.DeepEqual(schemaTestSession(t, state, id).Subagents, schemaTestSession(t, previous, id).Subagents) {
			t.Fatalf("session %s policy changed", id)
		}
		now, nowOK := state.LastSubagentPolicies[id]
		was, wasOK := previous.LastSubagentPolicies[id]
		if nowOK != wasOK || !reflect.DeepEqual(now, was) {
			t.Fatalf("remembered policy %s changed", id)
		}
	}
}

// schemaTestRowValues is every column of the one row query returns, as
// rusqlite::types::Value.
func schemaTestRowValues(t testing.TB, connection Queryer, query string) []any {
	t.Helper()
	rows, err := connection.Query(query)
	schemaTestMust(t, err)
	defer rows.Close()
	columns, err := rows.Columns()
	schemaTestMust(t, err)
	if !rows.Next() {
		t.Fatalf("no row: %v", rows.Err())
	}
	values := make([]any, len(columns))
	pointers := make([]any, len(columns))
	for i := range values {
		pointers[i] = &values[i]
	}
	schemaTestMust(t, rows.Scan(pointers...))
	return values
}

// Hard-won: unknown harness rows were erased by full snapshot saves.
func TestUnknownHarnessSurvivesMigrationAndSnapshotSave(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "future.sqlite3")
	connection := schemaTestOpen(t, path)
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestExec(t, connection, `CREATE TRIGGER stop_harness BEFORE INSERT ON schema_migrations WHEN NEW.version > 125 BEGIN SELECT RAISE(ABORT, 'boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestExec(t, connection, `DROP TRIGGER stop_harness;
        INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('kept','project','now');
        INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at) VALUES ('kept','kept','codex','codex','target','stopped','now');
        CREATE INDEX harness_fixture_index ON sessions(title);
        CREATE TRIGGER harness_external_trigger AFTER UPDATE ON session_contexts BEGIN SELECT count(*) FROM sessions; END;
        CREATE TRIGGER harness_fixture_trigger AFTER UPDATE OF title ON sessions BEGIN SELECT 1; END;`)
	schemaTestMust(t, migrateSchema(connection))
	schemaTestExec(t, connection, `UPDATE sessions SET harness_kind='future-harness'; INSERT INTO hidden_native_sessions VALUES ('future-harness','native','now');`)
	snapshot := func() []any {
		return schemaTestRowValues(t, connection, "SELECT * FROM sessions WHERE session_id='kept'")
	}
	before := snapshot()
	state := schemaTestLoadState(t, path)
	if state.Sessions.ContainsKey("kept") {
		t.Fatal("a session with an unknown harness was loaded")
	}
	schemaTestMust(t, SaveStateTo(path, &state))
	if after := snapshot(); !reflect.DeepEqual(before, after) {
		t.Fatalf("snapshot save changed the row:\n%#v\n%#v", before, after)
	}
	if kind := schemaTestText(t, connection, `SELECT harness_kind FROM sessions WHERE session_id='kept'`); kind != "future-harness" {
		t.Fatalf("harness %q", kind)
	}
	if count := schemaTestInt(t, connection, `SELECT count(*) FROM hidden_native_sessions WHERE harness_kind='future-harness'`); count != 1 {
		t.Fatalf("hidden native sessions %d", count)
	}
	if count := schemaTestInt(t, connection, `SELECT count(*) FROM sqlite_schema WHERE name IN ('harness_fixture_index','harness_fixture_trigger')`); count != 2 {
		t.Fatalf("fixture objects %d", count)
	}
	if schemaTestExists(t, connection, "PRAGMA foreign_key_check") {
		t.Fatal("foreign key violation")
	}
}

func TestTerminalDraftMigrationPreservesArchivesAndAdmitsTombstones(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "upgrade.sqlite3")
	connection := schemaTestOpen(t, path)
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestExec(t, connection, `CREATE TRIGGER stop_before_terminal_drafts BEFORE INSERT ON schema_migrations
                 WHEN NEW.version > 122 BEGIN SELECT RAISE(ABORT, 'fixture migration boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, connection)
	if version := schemaTestInt(t, connection, "PRAGMA user_version"); version != 122 {
		t.Fatalf("revision %d", version)
	}
	schemaTestExec(t, connection, `DROP TRIGGER stop_before_terminal_drafts;
                 INSERT INTO workspaces(workspace_id,name,name_key,created_at,last_opened_at)
                     VALUES ('workspace','Workspace','workspace','now','now');
                 INSERT INTO session_contexts(session_id,bundle_id,created_at,workspace_id)
                     VALUES ('session','project','now','workspace');
                 INSERT INTO detached_drafts(
                     draft_id,workspace_id,session_id,source,owner_pid,saved_at,text,recovered_at
                 ) VALUES ('archive','workspace','session','terminal',123,'then','old archive','recovered');`)

	schemaTestMust(t, migrateSchema(connection))

	var text, recoveredAt string
	var ownerPID int64
	var revision sql.NullInt64
	schemaTestRow(t, connection, `SELECT text, owner_pid, recovered_at, revision FROM detached_drafts
                  WHERE draft_id='archive'`, nil, &text, &ownerPID, &recoveredAt, &revision)
	if text != "old archive" || ownerPID != 123 || recoveredAt != "recovered" || revision.Valid {
		t.Fatalf("archive (%q, %d, %q, %v)", text, ownerPID, recoveredAt, revision)
	}
	schemaTestExec(t, connection, `INSERT INTO detached_drafts(
                     draft_id,workspace_id,session_id,source,owner_pid,saved_at,text,revision
                 ) VALUES ('tombstone','workspace','session','terminal',123,'now','',1)`)
	if errors := schemaTestInt(t, connection, "SELECT count(*) FROM pragma_foreign_key_check"); errors != 0 {
		t.Fatalf("foreign key errors %d", errors)
	}
}

func TestNitroTargetKindMigrationKeepsRowsAdmitsTheKindAndRaisesTheFloor(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "upgrade.sqlite3")
	connection := schemaTestOpen(t, path)
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestExec(t, connection, `CREATE TRIGGER stop_before_nitro BEFORE INSERT ON schema_migrations
                 WHEN NEW.version > 123 BEGIN SELECT RAISE(ABORT, 'fixture migration boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, connection)
	released := schemaTestState(t, connection)
	schemaTestExpectState(t, released, 123, 123)
	for _, id := range []string{"apple", "nitro", "no-host"} {
		schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('`+id+`','project','now');
                     INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
                         VALUES ('`+id+`','`+id+`','codex','codex','target','stopped','now');`)
	}
	// The revision-123 table refuses the new kind, which is what keeps an
	// older daemon from storing a row it could not read back.
	insertNitro := `INSERT INTO session_targets(session_id, kind, host, resource_id)
                            VALUES ('nitro', 'ssh-nitro', 'ec2.example.test', 'mj-nitro')`
	if _, err := connection.Exec(insertNitro); err == nil {
		t.Fatal("the revision-123 table admitted ssh-nitro")
	}
	schemaTestExec(t, connection, `DROP TRIGGER stop_before_nitro;
                 INSERT INTO session_targets(session_id, kind, host, resource_id, borrowed_from)
                     VALUES ('apple', 'ssh-apple-container', 'mac.example.test', 'mj-apple', 'parent');
                 INSERT INTO environments(environment_id, owner_session_id, phase, placement_json)
                     VALUES ('environment', 'apple', 'active', '{}');
                 INSERT INTO environment_workers(lease_id, environment_id, worker_session_id, kind, host, resource_id, target_template_id)
                     VALUES ('lease', 'environment', 'apple', 'ssh-apple-container', 'mac.example.test', 'mj-apple', 'target');
                 UPDATE environments SET target_lease_id = 'lease' WHERE environment_id = 'environment';`)

	schemaTestMust(t, migrateSchema(connection))

	var kind, host, resource string
	var borrowed sql.NullString
	schemaTestRow(t, connection, `SELECT kind, host, resource_id, borrowed_from FROM session_targets
                  WHERE session_id='apple'`, nil, &kind, &host, &resource, &borrowed)
	if kind != "ssh-apple-container" || host != "mac.example.test" || resource != "mj-apple" ||
		!borrowed.Valid || borrowed.String != "parent" {
		t.Fatalf("kept target (%q, %q, %q, %v)", kind, host, resource, borrowed)
	}
	schemaTestExec(t, connection, insertNitro)
	// The physical ownership ledger admits the kind too, and rebuilding
	// its table did not clear the lease that points at it (the foreign
	// key is `ON DELETE SET NULL`).
	if lease := schemaTestOptionalText(t, connection, `SELECT target_lease_id FROM environments WHERE environment_id = 'environment'`); lease == nil || *lease != "lease" {
		t.Fatalf("lease %v", lease)
	}
	schemaTestExec(t, connection, `INSERT INTO environment_workers(lease_id, environment_id, worker_session_id, kind, host, resource_id, target_template_id)
                 VALUES ('nitro-lease', 'environment', 'nitro', 'ssh-nitro', 'ec2.example.test', 'mj-nitro', 'target')`)
	// The shape constraint still holds for the new kind: a sandbox names
	// its host and resource, and carries no workspace path.
	if _, err := connection.Exec(`INSERT INTO session_targets(session_id, kind, resource_id)
                     VALUES ('no-host', 'ssh-nitro', 'mj-nitro')`); err == nil {
		t.Fatal("a sandbox without a host was admitted")
	}
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	schemaTestMustFail(t, state.ensureSupportedBy(123), "a revision-123 build")
}

// schemaTestTables is every table's name and stored SQL, by name.
func schemaTestTables(t testing.TB, database Queryer) [][2]string {
	t.Helper()
	rows, err := database.Query(`SELECT name, sql FROM sqlite_schema WHERE type='table' ORDER BY name`)
	schemaTestMust(t, err)
	tables := [][2]string{}
	for rows.Next() {
		var name, sql string
		schemaTestMust(t, rows.Scan(&name, &sql))
		tables = append(tables, [2]string{name, sql})
	}
	schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
	return tables
}

func TestRemovedPlanReviewTablesRaiseTheFloorAndPreserveTurnReviews(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "upgrade.sqlite3")
	connection := schemaTestOpen(t, path)
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestExec(t, connection, `CREATE TRIGGER stop_before_removal BEFORE INSERT ON schema_migrations
             WHEN NEW.version > 94 BEGIN SELECT RAISE(ABORT, 'fixture migration boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, connection)
	released := schemaTestState(t, connection)
	schemaTestExpectState(t, released, 94, 92)
	schemaTestExec(t, connection, `DROP TRIGGER stop_before_removal;
             INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('kept','project','now');
             INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
                 VALUES ('kept','Keep my work','codex','codex','local','error','now');
             INSERT INTO turn_review_state(session_id,baselines,reviewed_through_ordinal)
                 VALUES ('kept','{"/repo":"tree"}',42);`)
	schemaTestMust(t, migrateSchema(connection))
	state := schemaTestState(t, connection)
	schemaTestMustFail(t, state.ensureSupportedBy(minimumCompatibleVersion-1), "the previous floor")
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	revisions := []int64{}
	for _, text := range schemaTestTexts(t, connection, `SELECT version FROM schema_migrations WHERE version >= 93 ORDER BY version`) {
		revision, err := strconv.ParseInt(text, 10, 64)
		schemaTestMust(t, err)
		revisions = append(revisions, revision)
	}
	expected := []int64{}
	for revision := int64(93); revision <= schemaVersion; revision++ {
		expected = append(expected, revision)
	}
	if !slices.Equal(revisions, expected) {
		t.Fatalf("revisions %v", revisions)
	}
	fresh := schemaTestOpen(t, ":memory:")
	schemaTestMust(t, migrateSchema(fresh))
	if upgraded, created := schemaTestTables(t, connection), schemaTestTables(t, fresh); !reflect.DeepEqual(upgraded, created) {
		t.Fatalf("upgraded schema differs from a fresh one:\n%v\n%v", upgraded, created)
	}
	schemaTestMustFail(t, state.ensureSupportedBy(94), "a revision-94 build")
	schemaTestMustFail(t, state.ensureSupportedBy(92), "a revision-92 build")
	// Both legacy tables have been removed, while the independent turn-review state remains.
	for _, name := range schemaTestTexts(t, connection, "SELECT name FROM sqlite_schema WHERE type='table'") {
		if strings.HasPrefix(name, "second_") {
			t.Fatalf("legacy table %s remains", name)
		}
	}
	var baselines string
	var reviewedThrough int64
	schemaTestRow(t, connection, `SELECT baselines,reviewed_through_ordinal FROM turn_review_state WHERE session_id='kept'`, nil, &baselines, &reviewedThrough)
	if baselines != `{"/repo":"tree"}` || reviewedThrough != 42 {
		t.Fatalf("retained (%q, %d)", baselines, reviewedThrough)
	}
}

func TestRestartIntentSchemaMatchesForFreshAndUpgradedStores(t *testing.T) {
	directory := t.TempDir()
	fresh := schemaTestOpen(t, filepath.Join(directory, "fresh.sqlite3"))
	schemaTestMust(t, migrateSchema(fresh))

	upgraded := schemaTestOpen(t, filepath.Join(directory, "upgraded.sqlite3"))
	schemaTestMust(t, createBaselineSchema(upgraded))
	schemaTestExec(t, upgraded, `CREATE TRIGGER stop_before_restart_migration BEFORE INSERT ON schema_migrations
                 WHEN NEW.version > 82
                 BEGIN SELECT RAISE(ABORT, 'fixture migration boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(upgraded), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, upgraded)
	schemaTestExec(t, upgraded, `INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('kept','project','now');
             INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
                 VALUES ('kept','Keep my work','codex','codex','podman','closing','now');
             INSERT INTO session_restart_intents(session_id,operation_id,phase,updated_at,checkpoint_sha256,checkpoint_started)
                 VALUES ('kept','restart-kept','stopping','now','sealed-sha',1);`)
	schemaTestMustFail(t, migrateSchema(upgraded), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, upgraded)
	var operation, sha string
	schemaTestRow(t, upgraded, `SELECT operation_id,checkpoint_sha256 FROM session_restart_intents WHERE session_id='kept'`, nil, &operation, &sha)
	if operation != "restart-kept" || sha != "sealed-sha" {
		t.Fatalf("retained (%q, %q)", operation, sha)
	}
	schemaTestExec(t, upgraded, "DROP TRIGGER stop_before_restart_migration")
	if revision := schemaTestState(t, upgraded).revision; revision != 82 {
		t.Fatalf("revision %d", revision)
	}
	schemaTestMust(t, migrateSchema(upgraded))

	upgradedSchema := schemaTestState(t, upgraded)
	if schemaTestFloor(upgradedSchema) != minimumCompatibleVersion {
		t.Fatalf("floor %d", schemaTestFloor(upgradedSchema))
	}
	schemaTestMustFail(t, upgradedSchema.ensureSupportedBy(82), "a revision-82 build")
	schemaTestExec(t, upgraded, `UPDATE session_restart_intents SET phase='recreating_container' WHERE session_id='kept'`)
	var started bool
	schemaTestRow(t, upgraded, `SELECT operation_id,checkpoint_sha256,checkpoint_started FROM session_restart_intents WHERE session_id='kept'`, nil, &operation, &sha, &started)
	if operation != "restart-kept" || sha != "sealed-sha" || !started {
		t.Fatalf("retained (%q, %q, %v)", operation, sha, started)
	}

	tableSQL := func(connection *Connection) string {
		return schemaTestText(t, connection, `SELECT sql FROM sqlite_schema WHERE type='table' AND name='session_restart_intents'`)
	}
	if tableSQL(fresh) != tableSQL(upgraded) {
		t.Fatalf("restart intent tables differ:\n%s\n%s", tableSQL(fresh), tableSQL(upgraded))
	}
	if revision := schemaTestState(t, fresh).revision; revision != schemaVersion {
		t.Fatalf("fresh revision %d", revision)
	}
	if revision := schemaTestState(t, upgraded).revision; revision != schemaVersion {
		t.Fatalf("upgraded revision %d", revision)
	}
}

// Hard-won: 0d648876: web New saved generated placeholders as manual overrides.
func TestDefaultTitleOverrideRepairRunsOnceAndPreservesManualNames(t *testing.T) {
	connection := schemaTestOpen(t, ":memory:")
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestExec(t, connection, `CREATE TRIGGER stop_repair BEFORE INSERT ON schema_migrations WHEN NEW.version=112 BEGIN SELECT RAISE(ABORT,'fixture boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, connection)
	schemaTestExec(t, connection, "DROP TRIGGER stop_repair;")
	projectDirectory := "/work/repo/.mj/clones/session-id/"
	for _, c := range []struct {
		id, title, manual string
		directory         *string
	}{
		{"project", "repo via fake", "repo via fake", &projectDirectory},
		{"bundle", "bundle via fake", "bundle via fake", nil},
		{"rename", "bundle via fake", "My manual name", nil},
		{"custom", "Fix the parser", "Fix the parser", nil},
		{"moved", "bundle via original", "bundle via original", nil},
		{"words", "a b via c", "a b via c", nil},
		{"repeated", "a via b via c", "a via b via c", nil},
		{"unicode", "a via b c", "a via b c", nil},
		{"empty", "a via ", "a via ", nil},
		{"child", "bundle via fake", "bundle via fake", nil},
	} {
		var directory any
		if c.directory != nil {
			directory = pathToBlob(*c.directory)
		}
		schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES (?1,'bundle','now')`, c.id)
		schemaTestExec(t, connection, `INSERT INTO sessions(session_id,title,session_title_override,harness_kind,last_profile,target_template_id,state,updated_at,project_directory) VALUES (?1,?2,?3,'codex','fake','local','error','now',?4)`, c.id, c.title, c.manual, directory)
	}
	schemaTestExec(t, connection, `INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES ('child','project','task','{}');`)
	schemaTestMust(t, migrateSchema(connection))
	type override struct {
		id     string
		manual *string
	}
	overrides := func() []override {
		rows, err := connection.Query(`SELECT session_id,session_title_override FROM sessions ORDER BY session_id`)
		schemaTestMust(t, err)
		values := []override{}
		for rows.Next() {
			var id string
			var manual sql.NullString
			schemaTestMust(t, rows.Scan(&id, &manual))
			value := override{id: id}
			if manual.Valid {
				value.manual = &manual.String
			}
			values = append(values, value)
		}
		schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
		return values
	}
	text := func(value string) *string { return &value }
	expected := []override{
		{"bundle", nil},
		{"child", text("bundle via fake")},
		{"custom", text("Fix the parser")},
		{"empty", text("a via ")},
		{"moved", nil},
		{"project", nil},
		{"rename", text("My manual name")},
		{"repeated", text("a via b via c")},
		{"unicode", text("a via b c")},
		{"words", text("a b via c")},
	}
	if got := overrides(); !reflect.DeepEqual(got, expected) {
		t.Fatalf("overrides %v", got)
	}
	if touched := schemaTestTexts(t, connection, `SELECT session_id FROM sessions WHERE updated_at <> 'now' ORDER BY session_id`); !slices.Equal(touched, []string{"bundle", "moved", "project"}) {
		t.Fatalf("touched %v", touched)
	}
	schema := schemaTestState(t, connection)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	schemaTestExec(t, connection, `UPDATE sessions SET session_title_override=title WHERE session_id='bundle'`)
	before := overrides()
	schemaTestMust(t, migrateSchema(connection))
	if after := overrides(); !reflect.DeepEqual(after, before) {
		t.Fatalf("a second repair changed overrides: %v", after)
	}
}

func assertDivergentHistoryUpgrades(t *testing.T, revisionNumber int64, projectHistory bool, interrupt bool) {
	t.Helper()
	revision := strconv.FormatInt(revisionNumber, 10)
	directory := t.TempDir()
	path := filepath.Join(directory, "divergent-history.sqlite")
	connection := schemaTestOpen(t, path)
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id,bundle_id,created_at)
                 VALUES ('kept','project','now');
             INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at,project_directory)
                 VALUES ('kept','Keep my work','codex','codex','local','error','now',X'2F7265706F');
             INSERT INTO materialized_sessions(session_id) VALUES ('kept');
             INSERT INTO session_turn_usage VALUES ('kept','turn',1,1,'{"command_id":"turn","completed_ordinal":1,"completed_at_ms":100,"outcome":{"kind":"completed","stop_reason":"EndTurn"}}');
             INSERT INTO session_provider_cost VALUES ('kept','{"amount":1}');
             CREATE TRIGGER stop_at_revision BEFORE INSERT ON schema_migrations
                 WHEN NEW.version > `+revision+`
                 BEGIN SELECT RAISE(ABORT,'fixture migration boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestRollbackIfOpen(t, connection)
	schemaTestExec(t, connection, "DROP TRIGGER stop_at_revision")
	if found := schemaTestState(t, connection).revision; found != revisionNumber {
		t.Fatalf("revision %d", found)
	}
	if projectHistory {
		schemaTestExec(t, connection, projectCatalogV67SQL)
		schemaTestExec(t, connection, `INSERT INTO project_catalog VALUES ('project','key','{}',0);
                 INSERT INTO project_aliases VALUES ('alias','project','{}',1);
                 INSERT INTO project_session_aliases VALUES ('kept','alias');
                 UPDATE sessions SET project_json='{"kept":true}';`)
	}
	if interrupt {
		schemaTestExec(t, connection, `CREATE TRIGGER interrupt_reconciliation BEFORE INSERT ON schema_migrations
                 WHEN NEW.version=69 BEGIN SELECT RAISE(ABORT,'interrupted reconciliation'); END;`)
		schemaTestMustFail(t, migrateSchema(connection), "the interrupted reconciliation")
		schemaTestMust(t, connection.Close())
		connection := schemaTestOpen(t, path)
		if found := schemaTestState(t, connection).revision; found != 68 {
			t.Fatalf("revision %d", found)
		}
		schemaTestExec(t, connection, "DROP TRIGGER interrupt_reconciliation")
		schemaTestMust(t, connection.Close())
	} else {
		schemaTestMust(t, connection.Close())
	}

	writer := schemaTestWriter(t, path)
	state := schemaTestState(t, writer)
	if state.revision != schemaVersion {
		t.Fatalf("revision %d", state.revision)
	}
	schemaTestMustFail(t, state.ensureSupportedBy(68), "a revision-68 build")
	schemaTestMustFail(t, state.ensureSupportedBy(67), "a revision-67 build")
	if projectHistory {
		if snapshot := schemaTestText(t, writer, `SELECT project_json FROM sessions WHERE session_id='kept'`); snapshot != `{"kept":true}` {
			t.Fatalf("snapshot %s", snapshot)
		}
		var canonical string
		var pending int64
		schemaTestRow(t, writer, `SELECT canonical_id,config_pending FROM project_aliases WHERE bundle_id='alias'`, nil, &canonical, &pending)
		if canonical != "project" || pending != 1 {
			t.Fatalf("alias (%q, %d)", canonical, pending)
		}
	}
	// Rebuilding sessions must retain discovery triggers and admit the new state.
	schemaTestExec(t, writer, `UPDATE sessions SET state='startup-cleanup',project_directory=X'2F6E6577' WHERE session_id='kept';
             INSERT INTO session_turn_selections(session_id,command_id,model,effort) VALUES ('kept','turn','model','high');`)
	var changed []byte
	schemaTestRow(t, writer, `SELECT directory FROM project_discovery_changes ORDER BY sequence DESC LIMIT 1`, nil, &changed)
	if string(changed) != "/new" {
		t.Fatalf("changed directory %q", changed)
	}
	schemaTestExec(t, writer, "DELETE FROM sessions WHERE session_id='kept'")
	usage := schemaTestText(t, writer, "SELECT body FROM session_turn_usage")
	turn, err := deserializeTurnOutcome(usage)
	schemaTestMust(t, err)
	if turn.CommandID != "turn" {
		t.Fatalf("turn %q", turn.CommandID)
	}
	if cost := schemaTestText(t, writer, "SELECT body FROM session_provider_cost"); cost != `{"amount":1}` {
		t.Fatalf("cost %s", cost)
	}
	if schemaTestExists(t, writer, "PRAGMA foreign_key_check") {
		t.Fatal("foreign key violation")
	}
	schemaTestMust(t, writer.Close())
	forgetVerifiedSchema(path)
	if found := schemaTestState(t, schemaTestWriter(t, path)).revision; found != schemaVersion {
		t.Fatalf("revision %d", found)
	}
}

func TestDivergentAccountingRevision67PreservesUsageAndAddsProjects(t *testing.T) {
	assertDivergentHistoryUpgrades(t, 67, false, false)
}

func TestDivergentCleanupRevision68PreservesUsageAndAddsProjects(t *testing.T) {
	assertDivergentHistoryUpgrades(t, 68, false, false)
}

func TestDivergentProjectRevision67PreservesAliasesSnapshotsAndUsage(t *testing.T) {
	assertDivergentHistoryUpgrades(t, 66, true, false)
}

func TestDivergentProjectReconciliationResumesAfterInterruption(t *testing.T) {
	assertDivergentHistoryUpgrades(t, 66, true, true)
}

func TestMoveOwnershipUpgradeRetainsSourcesAndRefusesPreviousDaemons(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "move.sqlite")
	connection := schemaTestWriter(t, path)
	restorePre153TranscriptLayout(t, connection)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 63)
	schemaTestExec(t, connection, `DROP TABLE retained_move_sources; DELETE FROM schema_migrations WHERE version>=64; UPDATE schema_compatibility SET minimum_compatible_version=63 WHERE singleton=1; DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version=63;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)
	upgraded := schemaTestWriter(t, path)
	schema := schemaTestState(t, upgraded)
	schemaTestMustFail(t, schema.ensureSupportedBy(63), "a revision-63 build")
	schemaTestExec(t, upgraded, `INSERT INTO retained_move_sources(operation_id,session_id,source_json,exclusions_json,created_at) VALUES ('move-one','session-one','{}','[]','now')`)
	schemaTestMust(t, upgraded.Close())
	reopened := schemaTestWriter(t, path)
	if count := schemaTestInt(t, reopened, "SELECT count(*) FROM retained_move_sources"); count != 1 {
		t.Fatalf("retained sources %d", count)
	}
}

func TestEc2MoveOwnershipUpgradeIsAtomicAndPreservesExistingMoves(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "ec2-move-migration.sqlite3")
	schemaTestSaveSession(t, path, session("source", "project"))
	stampSchemaVersion(t, path, 70)
	old := schemaTestOpen(t, path)
	schemaTestExec(t, old, `UPDATE schema_compatibility SET minimum_compatible_version=69;
             INSERT INTO session_moves(session_id,operation_id,operation_json)
                 VALUES('source','existing-move','{"operation_id":"existing-move"}');
             CREATE TRIGGER stop_ec2_move_migration BEFORE INSERT ON schema_migrations
             WHEN NEW.version=71 BEGIN SELECT RAISE(ABORT,'fixture boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(old), "the migration to the boundary")
	state := schemaTestState(t, old)
	schemaTestExpectState(t, state, 70, 69)
	schemaTestExec(t, old, "DROP TRIGGER stop_ec2_move_migration")
	schemaTestMust(t, old.Close())
	upgraded := schemaTestWriter(t, path)
	state = schemaTestState(t, upgraded)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	schemaTestMustFail(t, state.ensureSupportedBy(70), "a revision-70 build")
	schemaTestMust(t, state.ensureSupportedBy(minimumCompatibleVersion))
	schemaTestMustFail(t, state.ensureSupportedBy(minimumCompatibleVersion-1), "the previous floor")
	if intent := schemaTestOptionalText(t, upgraded, `SELECT close_requested_operation_id FROM session_moves WHERE session_id='source'`); intent != nil {
		t.Fatalf("close intent %v", *intent)
	}
	if preserved := schemaTestText(t, upgraded, `SELECT operation_json FROM session_moves WHERE session_id='source'`); preserved != `{"operation_id":"existing-move"}` {
		t.Fatalf("operation %s", preserved)
	}
	if count := schemaTestInt(t, upgraded, `SELECT count(*) FROM sessions WHERE session_id='source'`); count != 1 {
		t.Fatalf("sessions %d", count)
	}
}

func TestTitleMigrationCapsOldTitlesPreservesShortTitlesAndIsCompatible(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "title-migration.sqlite3")
	long := strings.Repeat("word ", 20_000)
	unicodeTitle := strings.Repeat("界", 257)
	exact := strings.Repeat("界", 256)
	short := "  Keep\nthis title  "
	titles := []struct {
		id    string
		title *string
	}{
		{"long", &long},
		{"unicode", &unicodeTitle},
		{"exact", &exact},
		{"short", &short},
		{"unset", nil},
	}
	for _, c := range titles {
		record := session(c.id, "project")
		record.State = core.SessionStateStopped
		schemaTestSaveSession(t, path, record)
	}
	stampSchemaVersion(t, path, 69)
	old := schemaTestOpen(t, path)
	schemaTestExec(t, old, `UPDATE schema_compatibility SET minimum_compatible_version=69;
             CREATE TRIGGER stop_after_title_migration BEFORE INSERT ON schema_migrations
             WHEN NEW.version=71 BEGIN SELECT RAISE(ABORT,'fixture boundary'); END;`)
	for _, c := range titles {
		schemaTestExec(t, old, `UPDATE sessions SET acp_session_title=?2 WHERE session_id=?1`, c.id, c.title)
	}
	schemaTestMustFail(t, migrateSchema(old), "the migration to the boundary")
	upgraded := old
	state := schemaTestState(t, upgraded)
	schemaTestExpectState(t, state, 70, 69)
	schemaTestMust(t, state.ensureSupportedBy(69))
	for _, c := range titles {
		stored := schemaTestOptionalText(t, upgraded, `SELECT acp_session_title FROM sessions WHERE session_id=?1`, c.id)
		expected := c.title
		switch c.id {
		case "long":
			text := strings.Repeat("word ", 50) + "word…"
			expected = &text
		case "unicode":
			text := strings.Repeat("界", 255) + "…"
			expected = &text
		}
		if !reflect.DeepEqual(stored, expected) {
			t.Fatalf("session %s: %v", c.id, stored)
		}
		if stored != nil && len([]rune(*stored)) > 256 {
			t.Fatalf("session %s title is %d characters", c.id, len([]rune(*stored)))
		}
	}
	schemaTestExec(t, upgraded, "DROP TRIGGER stop_after_title_migration")
	schemaTestMust(t, upgraded.Close())
	forgetVerifiedSchema(path)
	schemaTestMust(t, schemaTestWriter(t, path).Close())
}

func TestInterruptedTitleMigrationRollsBackTitlesAndRevision(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "title-migration-interrupted.sqlite3")
	schemaTestSaveSession(t, path, session("old", "project"))
	stampSchemaVersion(t, path, 69)
	original := strings.Repeat("word ", 20_000)
	old := schemaTestOpen(t, path)
	schemaTestExec(t, old, "UPDATE sessions SET acp_session_title=?1", original)
	schemaTestExec(t, old, `CREATE TRIGGER stop_title_migration BEFORE INSERT ON schema_migrations
             WHEN NEW.version=70 BEGIN SELECT RAISE(ABORT,'fixture boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(old), "the migration to the boundary")
	if revision := schemaTestState(t, old).revision; revision != 69 {
		t.Fatalf("revision %d", revision)
	}
	if stored := schemaTestText(t, old, "SELECT acp_session_title FROM sessions"); stored != original {
		t.Fatal("the interrupted migration changed a title")
	}
	schemaTestExec(t, old, "DROP TRIGGER stop_title_migration")
	schemaTestMust(t, old.Close())
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	title := schemaTestSession(t, schemaTestLoadState(t, path), "old").ACPSessionTitle
	if title == nil || len([]rune(*title)) > 256 {
		t.Fatalf("title %v", title)
	}
}

func TestGithubTimelineMigrationResetsProgressAndKeepsWatches(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "github-migration.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	schemaTestSaveSession(t, path, session("creator", "project"))
	stampSchemaVersion(t, path, 84)
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `INSERT INTO github_watch_cursors(owner,repo,items_watermark_at,items_watermark_id,items_etag,comments_cursor)
                 VALUES ('acme','repo','2020-01-01T00:00:00Z',1,'etag','2020-01-01T00:00:00Z');
             INSERT INTO github_watch_items(owner,repo,number,creator_session_id,kind,title,url,created_at,pull_request_etag,pull_request_state)
                 VALUES ('acme','repo',42,'creator','pull_request','keep title','https://example.com/42','2020-01-01T00:00:00Z','etag','closed');
             INSERT INTO github_watch_classifications(owner,repo,number,session_id,kind,title,url,created_at,interested,created)
                 VALUES ('acme','repo',42,'creator','pull_request','keep title','https://example.com/42','2020-01-01T00:00:00Z',1,1);`)
	schemaTestMust(t, connection.Close())
	before := time.Now().UnixMilli()
	writer := schemaTestWriter(t, path)
	var cursor, start string
	schemaTestRow(t, writer, `SELECT updated_at, watch_started_at FROM github_watch_cursors`, nil, &cursor, &start)
	if cursor != start {
		t.Fatalf("cursor %s, start %s", cursor, start)
	}
	if reconciled := schemaTestText(t, writer, `SELECT reconciled_through FROM github_watch_cursors`); reconciled != start {
		t.Fatalf("reconciled through %s", reconciled)
	}
	var lastReconciled sql.NullInt64
	schemaTestRow(t, writer, `SELECT last_reconciled_at_ms FROM github_watch_cursors`, nil, &lastReconciled)
	if lastReconciled.Valid {
		t.Fatalf("last reconciled %d", lastReconciled.Int64)
	}
	parsed, err := time.Parse(time.RFC3339Nano, cursor)
	schemaTestMust(t, err)
	if parsed.UnixMilli() < before {
		t.Fatalf("cursor %s predates the migration", cursor)
	}
	var title, creator, timeline, processed string
	var id int64
	schemaTestRow(t, writer, `SELECT title,creator_session_id,timeline_at,processed_at,timeline_id FROM github_watch_items`, nil, &title, &creator, &timeline, &processed, &id)
	if title != "keep title" || creator != "creator" {
		t.Fatalf("watch (%q, %q)", title, creator)
	}
	var commentsAt string
	var commentsID int64
	schemaTestRow(t, writer, `SELECT review_comments_at, review_comments_id FROM github_watch_items`, nil, &commentsAt, &commentsID)
	if commentsAt != start || commentsID != 0 {
		t.Fatalf("comments (%q, %d)", commentsAt, commentsID)
	}
	if count := schemaTestInt(t, writer, `SELECT count(*) FROM github_watch_classification_failures`); count != 0 {
		t.Fatalf("classification failures %d", count)
	}
	if timeline != start || processed != start || id != 0 {
		t.Fatalf("timeline (%q, %q, %d)", timeline, processed, id)
	}
	if count := schemaTestInt(t, writer, `SELECT count(*) FROM github_watch_classifications WHERE created=1 AND interested=1`); count != 1 {
		t.Fatalf("classifications %d", count)
	}
	if count := schemaTestInt(t, writer, "SELECT count(*) FROM github_seen_items"); count != 0 {
		t.Fatalf("seen items %d", count)
	}
	state := schemaTestState(t, writer)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	schemaTestMustFail(t, state.ensureSupportedBy(84), "a revision-84 build")
	// Revision 130 rebuilt the table: the watch kept its creator and
	// cursors, and the recorded lifecycle event starts empty.
	var key, eventJSON, at sql.NullString
	schemaTestRow(t, writer, `SELECT lifecycle_event_key, lifecycle_event_json, lifecycle_at
                 FROM github_watch_items`, nil, &key, &eventJSON, &at)
	if key.Valid || eventJSON.Valid || at.Valid {
		t.Fatalf("lifecycle (%v, %v, %v)", key, eventJSON, at)
	}
}

func TestOriginDeltaArchiveMigrationFencesLegacyReadersAndWriters(t *testing.T) {
	connection := schemaTestOpen(t, ":memory:")
	schemaTestExec(t, connection, `CREATE TABLE schema_compatibility (
                singleton INTEGER PRIMARY KEY,
                minimum_compatible_version INTEGER NOT NULL
            );
            INSERT INTO schema_compatibility VALUES (1, 137);
            CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT);
            INSERT INTO schema_migrations VALUES (143, 'previous release');
            PRAGMA user_version = 143;`)
	schemaTestMust(t, schemaTestState(t, connection).ensureSupportedBy(143))

	schemaTestMust(t, migrateOriginDeltaArchives(connection))

	state := schemaTestState(t, connection)
	if schemaTestFloor(state) != 144 {
		t.Fatalf("floor %d", schemaTestFloor(state))
	}
	schemaTestMustFail(t, state.ensureSupportedBy(143), "a revision-143 build")
	schemaTestMust(t, state.ensureSupportedBy(144))
}

// Revision 130 is breaking. Build a store at revision 129 with a watch, its
// triggers and queued mail, migrate it, and check what survives and what an
// older build is told.
func TestWatchFollowersMigrationKeepsRowsAndRefusesOlderBuilds(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "watch-followers.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	for _, id := range []string{"creator", "follower"} {
		schemaTestSaveSession(t, path, session(id, "project"))
	}
	// Rebuild the table as revision 129 had it: a creator is required, and
	// there are no lifecycle columns. Rewinding the ledger makes the next
	// open apply revision 130.
	connection := schemaTestOpen(t, path)
	restorePre153TranscriptLayout(t, connection)
	schemaTestMust(t, ensureSessionContainerOverrideColumns(connection))
	schemaTestExec(t, connection, `DROP TRIGGER session_rows_deleted;
                 DROP TRIGGER session_rows_destroyed;
                 DROP TRIGGER github_watch_items_live_creator;
                 DROP TABLE github_watch_items;
                 CREATE TABLE github_watch_items (
                     owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,
                     creator_session_id TEXT NOT NULL,
                     kind TEXT NOT NULL CHECK(kind IN ('issue', 'pull_request')),
                     title TEXT NOT NULL, url TEXT NOT NULL, created_at TEXT NOT NULL,
                     timeline_at TEXT NOT NULL DEFAULT '', timeline_id INTEGER NOT NULL DEFAULT 0,
                     review_comments_at TEXT NOT NULL DEFAULT '', review_comments_id INTEGER NOT NULL DEFAULT 0,
                     processed_at TEXT NOT NULL DEFAULT '',
                     PRIMARY KEY(owner, repo, number)
                 ) STRICT;
                 CREATE TRIGGER session_rows_deleted AFTER DELETE ON sessions BEGIN
                     DELETE FROM mailbox_outbox WHERE target_session_id = OLD.session_id;
                     DELETE FROM github_watch_items WHERE creator_session_id = OLD.session_id;
                     DELETE FROM github_watch_classifications WHERE session_id = OLD.session_id;
                     DELETE FROM github_watch_classification_failures WHERE session_id = OLD.session_id;
                 END;
                 CREATE TRIGGER session_rows_destroyed
                     AFTER UPDATE OF state ON sessions
                     WHEN NEW.state = 'destroyed-with-data-loss' BEGIN
                     DELETE FROM mailbox_outbox WHERE target_session_id = NEW.session_id;
                     DELETE FROM github_watch_items WHERE creator_session_id = NEW.session_id;
                     DELETE FROM github_watch_classifications WHERE session_id = NEW.session_id;
                     DELETE FROM github_watch_classification_failures WHERE session_id = NEW.session_id;
                 END;
                 CREATE TRIGGER github_watch_items_live_creator
                     BEFORE INSERT ON github_watch_items
                     WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = NEW.creator_session_id AND state <> 'destroyed-with-data-loss')
                     BEGIN SELECT RAISE(IGNORE); END;
                 INSERT INTO github_watch_items(owner,repo,number,creator_session_id,kind,title,url,created_at,
                         timeline_at,timeline_id,review_comments_at,review_comments_id,processed_at)
                     VALUES ('acme','repo',7,'creator','pull_request','kept','https://example.com/7','c',
                         't1',11,'r1',12,'p1');
                 INSERT INTO github_watch_classifications(owner,repo,number,session_id,kind,title,url,created_at,interested,created)
                     VALUES ('acme','repo',7,'follower','pull_request','kept','https://example.com/7','c',1,0);
                 INSERT INTO github_watch_classifications(owner,repo,number,session_id,kind,title,url,created_at,interested,created)
                     VALUES ('acme','repo',9,'follower','pull_request','batch','https://example.com/9','2020-01-01T00:00:00Z',1,0),
                            ('acme','repo',10,'follower','issue','not a PR','https://example.com/10','c',1,0),
                            ('acme','repo',11,'follower','pull_request','not interested','https://example.com/11','c',0,0);
                 INSERT INTO mailbox_outbox(event_key,target_session_id,event_json,wake,created_at)
                     VALUES ('queued','creator','{}',1,'c');
                 DELETE FROM schema_migrations WHERE version >= 130;
                 UPDATE schema_compatibility SET minimum_compatible_version = 126;
                 PRAGMA user_version = 129;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)

	writer := schemaTestWriter(t, path)
	state := schemaTestState(t, writer)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	// A revision-129 build must refuse to read or write the store.
	schemaTestMustFail(t, state.ensureSupportedBy(129), "a revision-129 build")
	var creator, timelineAt, reviewCommentsAt, processedAt string
	var timelineID, reviewCommentsID int64
	var lifecycleKey sql.NullString
	schemaTestRow(t, writer, `SELECT creator_session_id, timeline_at, timeline_id, review_comments_at,
                        review_comments_id, processed_at, lifecycle_event_key
                 FROM github_watch_items WHERE number = 7`, nil, &creator, &timelineAt, &timelineID, &reviewCommentsAt, &reviewCommentsID, &processedAt, &lifecycleKey)
	if creator != "creator" || timelineAt != "t1" || timelineID != 11 || reviewCommentsAt != "r1" ||
		reviewCommentsID != 12 || processedAt != "p1" || lifecycleKey.Valid {
		t.Fatalf("kept watch (%q, %q, %d, %q, %d, %q, %v)", creator, timelineAt, timelineID, reviewCommentsAt, reviewCommentsID, processedAt, lifecycleKey)
	}
	// A PR a session was told about before the upgrade is watched from the
	// migration on, with no creator; an issue or an uninterested pair is not.
	rows, err := writer.Query(`SELECT number, creator_session_id, created_at, timeline_at > '2026'
                 FROM github_watch_items WHERE number <> 7 ORDER BY number`)
	schemaTestMust(t, err)
	type backfill struct {
		number    int64
		creator   sql.NullString
		createdAt string
		recent    bool
	}
	backfilled := []backfill{}
	for rows.Next() {
		var row backfill
		schemaTestMust(t, rows.Scan(&row.number, &row.creator, &row.createdAt, &row.recent))
		backfilled = append(backfilled, row)
	}
	schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
	if !reflect.DeepEqual(backfilled, []backfill{{9, sql.NullString{}, "2020-01-01T00:00:00Z", true}}) {
		t.Fatalf("backfilled %v", backfilled)
	}
	for _, c := range []struct {
		table    string
		expected int64
	}{{"mailbox_outbox", 1}, {"github_watch_classifications", 4}} {
		if count := schemaTestInt(t, writer, fmt.Sprintf("SELECT count(*) FROM %s", c.table)); count != c.expected {
			t.Fatalf("%s: %d", c.table, count)
		}
	}
	// The rebuilt table accepts a watch with no creator, and its triggers
	// work: the follower keeps the watch alive when the creator goes.
	schemaTestExec(t, writer, `INSERT INTO github_watch_items(owner,repo,number,creator_session_id,kind,title,url,created_at)
                 VALUES ('acme','repo',8,NULL,'pull_request','batch','u','c')`)
	schemaTestExec(t, writer, "DELETE FROM sessions WHERE session_id = 'creator'")
	rows, err = writer.Query(`SELECT number, creator_session_id FROM github_watch_items ORDER BY number`)
	schemaTestMust(t, err)
	type watch struct {
		number  int64
		creator sql.NullString
	}
	creators := []watch{}
	for rows.Next() {
		var row watch
		schemaTestMust(t, rows.Scan(&row.number, &row.creator))
		creators = append(creators, row)
	}
	schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
	if !reflect.DeepEqual(creators, []watch{{7, sql.NullString{}}, {8, sql.NullString{}}, {9, sql.NullString{}}}) {
		t.Fatalf("creators %v", creators)
	}
	if count := schemaTestInt(t, writer, "SELECT count(*) FROM mailbox_outbox"); count != 0 {
		t.Fatalf("mail addressed to the deleted creator goes with it: %d", count)
	}
}

func TestRecentRevisionsUpgradeDirectlyAndPreserveUserData(t *testing.T) {
	// Revision 47 was current on 2026-09-23. Keep the exhaustive
	// interruption matrix focused on the last week's migration history.
	const testRevisionFloor int64 = 47
	for revisionNumber := testRevisionFloor; revisionNumber < schemaVersion; revisionNumber++ {
		revision := strconv.FormatInt(revisionNumber, 10)
		directory := t.TempDir()
		path := filepath.Join(directory, "mj.sqlite3")
		connection := schemaTestOpen(t, path)
		// Building the store as it stood at `revision` is fixture work
		// that only this process reads. Flushed, it is one disk flush per
		// historical step for every revision: the slowest test in the
		// suite by far on a slow disk.
		schemaTestExec(t, connection, "PRAGMA synchronous = OFF;")
		schemaTestExec(t, connection, legacyV1SQL)
		schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('old-session', 'project', '2026-01-01T00:00:00Z');
                 INSERT INTO sessions(session_id, title, harness_kind, last_profile,
                     target_template_id, state, updated_at, native_session_id)
                     VALUES ('old-session', 'Keep my work', 'codex', 'codex', 'local', 'error',
                         '2026-01-01T00:00:00Z', 'native-original');
                 INSERT INTO prompt_history(session_id, event_sequence, submitted_at, text)
                     VALUES ('old-session', 1, '2026-01-01T00:00:00Z', 'Keep my prompt');`)
		// Interrupt at each historical transaction boundary. Closing the
		// connection rolls back the interrupted step, exactly as a killed
		// updater would. Reopening must resume without manual cleanup.
		schemaTestExec(t, connection, `CREATE TRIGGER stop_at_revision BEFORE INSERT ON schema_migrations
                 WHEN NEW.version > `+revision+`
                 BEGIN SELECT RAISE(ABORT, 'fixture migration boundary'); END;`)
		schemaTestMust(t, migrateToBaseline(connection))
		schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
		schemaTestMust(t, connection.Close())
		connection = schemaTestOpen(t, path)
		if found := schemaTestInt(t, connection, "PRAGMA user_version"); found != revisionNumber {
			t.Fatalf("revision %d: found %d", revisionNumber, found)
		}
		schemaTestExec(t, connection, "DROP TRIGGER stop_at_revision")
		if revisionNumber >= 59 {
			schemaTestExec(t, connection, `INSERT INTO startup_steps(session_id, command_id, step_json, phase) VALUES('old-session','legacy-startup',?1,'pending')`, `{"Prompt":{"text":"Keep my queued prompt","inherited_draft":null}}`)
		}
		schemaTestMust(t, connection.Close())

		writer, err := openWriter(path)
		if err != nil {
			t.Fatalf("revision %d: %v", revisionNumber, err)
		}
		schema := schemaTestState(t, writer)
		schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
		schemaTestMustFail(t, schema.ensureSupportedBy(minimumCompatibleVersion-1), "the previous floor")
		if integrity := schemaTestText(t, writer, "PRAGMA integrity_check"); integrity != "ok" {
			t.Fatalf("revision %d: integrity %s", revisionNumber, integrity)
		}
		if schemaTestExists(t, writer, "PRAGMA foreign_key_check") {
			t.Fatalf("revision %d: foreign key violation", revisionNumber)
		}
		schemaTestMust(t, writer.Close())
		reader, err := openReaderStrict(path)
		schemaTestMust(t, err)
		if prompt := schemaTestText(t, reader, "SELECT text FROM prompt_history"); prompt != "Keep my prompt" {
			t.Fatalf("revision %d: prompt %q", revisionNumber, prompt)
		}
		if revisionNumber >= 59 {
			pending := schemaTestText(t, reader, `SELECT step_json FROM startup_steps WHERE command_id='legacy-startup'`)
			step, err := UnmarshalStartupStep([]byte(pending))
			schemaTestMust(t, err)
			prompt, ok := step.(StartupStepPrompt)
			if !ok || prompt.Prompt != nil || prompt.EncodedDraft != nil || prompt.Text != "Keep my queued prompt" {
				t.Fatalf("revision %d: startup step %#v", revisionNumber, step)
			}
		}
		state := schemaTestLoadState(t, path)
		record := schemaTestSession(t, state, "old-session")
		if record.Title != "Keep my work" {
			t.Fatalf("revision %d: title %q", revisionNumber, record.Title)
		}
		if record.NativeSessionID == nil || *record.NativeSessionID != "native-original" {
			t.Fatalf("revision %d: native session %v", revisionNumber, record.NativeSessionID)
		}
		reader.Close()
		// A fresh writer process must not apply any migration twice.
		forgetVerifiedSchema(path)
		schemaTestMust(t, schemaTestWriter(t, path).Close())
	}
}

// schemaTestColumnTexts is column index of every row query returns.
func schemaTestColumnTexts(t testing.TB, connection Queryer, query string, index int) []string {
	t.Helper()
	rows, err := connection.Query(query)
	schemaTestMust(t, err)
	columns, err := rows.Columns()
	schemaTestMust(t, err)
	values := []string{}
	for rows.Next() {
		row := make([]any, len(columns))
		pointers := make([]any, len(columns))
		for i := range row {
			pointers[i] = &row[i]
		}
		schemaTestMust(t, rows.Scan(pointers...))
		text, ok := row[index].(string)
		if !ok {
			t.Fatalf("column %d is %T", index, row[index])
		}
		values = append(values, text)
	}
	schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
	return values
}

func TestRevision77MakesPromptOrdinalsNullableAndPreservesHistory(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	connection := schemaTestOpen(t, path)
	schemaTestMust(t, createBaselineSchema(connection))
	schemaTestMust(t, migrateSchema(connection))
	schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id, bundle_id, created_at)
                     VALUES ('old-session', 'project', '2026-01-01T00:00:00Z');
                 INSERT INTO sessions(session_id, title, harness_kind, last_profile,
                                      target_template_id, state, updated_at)
                     VALUES ('old-session', 'Keep history', 'codex', 'codex', 'local',
                             'error', '2026-01-01T00:00:00Z');`)
	schemaTestExec(t, connection, `INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
                 VALUES ('old-session', 11, '2026-01-01T00:00:00Z', 'kept prompt')`)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 76)
	restorePre153TranscriptLayout(t, connection)
	// Restore the previous table definition and migration ledger, retaining
	// a real row so the forward migration proves it copies existing data.
	schemaTestExec(t, connection, `DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away;
                 CREATE TABLE prompt_history_v76 (
                     history_id INTEGER PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                     event_ordinal INTEGER NOT NULL CHECK(event_ordinal >= 0),
                     submitted_at TEXT NOT NULL,
                     text TEXT NOT NULL CHECK(length(trim(text)) > 0),
                     UNIQUE(session_id, event_ordinal)
                 ) STRICT;
                 INSERT INTO prompt_history_v76
                     SELECT history_id, session_id, event_ordinal, submitted_at, text
                     FROM prompt_history;
                 DROP TABLE prompt_history;
                 ALTER TABLE prompt_history_v76 RENAME TO prompt_history;
                 CREATE INDEX prompt_history_session_recent
                     ON prompt_history(session_id, history_id DESC);
                 CREATE INDEX prompt_history_recent ON prompt_history(history_id DESC);
                 ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json;
                 ALTER TABLE session_turn_selections DROP COLUMN profile;
                 DELETE FROM schema_migrations WHERE version >= 77;
                 UPDATE schema_compatibility SET minimum_compatible_version = 74;
                 PRAGMA user_version = 76;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)

	migrated := schemaTestWriter(t, path)
	if revision := schemaTestState(t, migrated).revision; revision != schemaVersion {
		t.Fatalf("revision %d", revision)
	}
	var historyID, ordinal int64
	var text string
	schemaTestRow(t, migrated, `SELECT history_id, event_ordinal, text FROM prompt_history`, nil, &historyID, &ordinal, &text)
	if historyID != 1 || ordinal != 11 || text != "kept prompt" {
		t.Fatalf("preserved (%d, %d, %q)", historyID, ordinal, text)
	}
	if notNull := schemaTestInt(t, migrated, `SELECT "notnull" FROM pragma_table_info('prompt_history')
                 WHERE name = 'event_ordinal'`); notNull != 0 {
		t.Fatalf("event_ordinal notnull %d", notNull)
	}
	indexes := schemaTestColumnTexts(t, migrated, "PRAGMA index_list('prompt_history')", 1)
	if !slices.Contains(indexes, "prompt_history_session_recent") || !slices.Contains(indexes, "prompt_history_recent") {
		t.Fatalf("indexes %v", indexes)
	}
}

func TestAccountingMigrationPreservesUsageAndChangesItsDeletionOwner(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "migration.sqlite")
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, legacyV1SQL)
	schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('old-session','project','2026-01-01T00:00:00Z');
            INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
            VALUES ('old-session','Retain usage','codex','codex','local','error','2026-01-01T00:00:00Z');
            CREATE TRIGGER stop_before_accounting BEFORE INSERT ON schema_migrations WHEN NEW.version=67
            BEGIN SELECT RAISE(ABORT,'fixture boundary'); END;`)
	schemaTestMust(t, migrateToBaseline(connection))
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestExec(t, connection, `ROLLBACK; DROP TRIGGER stop_before_accounting;
            INSERT INTO session_turn_usage VALUES ('old-session','turn',7,1,'{"command_id":"turn","completed_ordinal":7,"completed_at_ms":100,"outcome":{"kind":"completed","stop_reason":"EndTurn"}}');
            INSERT INTO session_provider_cost VALUES ('old-session','{"amount":1}');`)
	schemaTestMust(t, connection.Close())
	writer := schemaTestWriter(t, path)
	schemaTestExec(t, writer, `DELETE FROM sessions WHERE session_id='old-session'`)
	for _, table := range []string{"session_turn_usage", "session_provider_cost"} {
		if count := schemaTestInt(t, writer, fmt.Sprintf("SELECT COUNT(*) FROM %s", table)); count != 1 {
			t.Fatalf("%s: %d", table, count)
		}
	}
	body := schemaTestText(t, writer, "SELECT body FROM session_turn_usage")
	turn, err := deserializeTurnOutcome(body)
	schemaTestMust(t, err)
	if turn.CommandID != "turn" {
		t.Fatalf("turn %q", turn.CommandID)
	}
	if schemaTestExists(t, writer, "PRAGMA foreign_key_check") {
		t.Fatal("foreign key violation")
	}
	if floor := schemaTestFloor(schemaTestState(t, writer)); floor != minimumCompatibleVersion {
		t.Fatalf("floor %d", floor)
	}
}

func TestRevision153BackfillsTranscriptMetadataAndRefusesRevision152(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestSaveSession(t, path, session("details", "project"))
	stampSchemaVersion(t, path, 152)
	previous := schemaTestOpen(t, path)
	before := schemaTestState(t, previous)
	schemaTestExpectState(t, before, 152, 152)
	schemaTestMust(t, before.ensureSupportedBy(152))
	ordinal := func(value int64) *int64 { return &value }
	cases := []struct {
		json    string
		ordinal *int64
		kind    string
		settled bool
	}{
		{`{"kind":"agent","chunks":[],"streaming":true}`, ordinal(10), "agent", false},
		{`{"kind":"agent","chunks":[],"streaming":false}`, ordinal(11), "agent", true},
		{`{"kind":"thought","chunks":[],"streaming":true}`, nil, "thought", false},
		{`{"kind":"tool","call":{"status":"in_progress"}}`, nil, "tool", false},
		{`{"kind":"tool","call":{}}`, nil, "tool", false},
		{`{"kind":"tool","call":{"status":"completed"}}`, nil, "tool", true},
		{`{"kind":"terminal_output","record":{}}`, nil, "terminal_output", true},
	}
	for i, c := range cases {
		position := int64(i + 1)
		schemaTestExec(t, previous, `INSERT INTO materialized_transcript_items(
                     session_id, stable_id, position, latest_content_event_ordinal,
                     created_at_ms, last_changed_at_ms, body_json
                 ) VALUES ('details', ?1, ?2, ?3, 1, 1, ?4)`, strconv.FormatInt(position, 10), position, c.ordinal, c.json)
	}
	schemaTestMust(t, previous.Close())
	upgraded := schemaTestWriter(t, path)
	state := schemaTestState(t, upgraded)
	schemaTestExpectState(t, state, 153, 153)
	schemaTestIncompatible(t, state.ensureSupportedBy(152), 153)
	for i, c := range cases {
		position := int64(i + 1)
		var body, kind string
		var settled bool
		var updateOrdinal int64
		var detailOffset, detailLen sql.NullInt64
		schemaTestRow(t, upgraded, `SELECT body_json, item_kind, settled, update_ordinal, detail_offset, detail_len
                 FROM materialized_transcript_items WHERE session_id = 'details' AND position = ?1`, []any{position}, &body, &kind, &settled, &updateOrdinal, &detailOffset, &detailLen)
		expectedOrdinal := position
		if c.ordinal != nil {
			expectedOrdinal = *c.ordinal
		}
		if body != c.json || kind != c.kind || settled != c.settled || updateOrdinal != expectedOrdinal ||
			detailOffset.Valid || detailLen.Valid {
			t.Fatalf("position %d: (%q, %q, %v, %d, %v, %v)", position, body, kind, settled, updateOrdinal, detailOffset, detailLen)
		}
	}
}

// stampSchemaVersion rewrites a store's recorded schema version the way
// another build's migration ladder would, and forgets that this process
// verified it.
func stampSchemaVersion(t testing.TB, path string, version int64) {
	t.Helper()
	if version > schemaVersion {
		advanceTestSchema(t, path, version, version)
		return
	}
	connection := schemaTestOpen(t, path)
	if version < 153 {
		restorePre153TranscriptLayout(t, connection)
	}
	if version < 146 {
		for _, column := range [][2]string{
			{"session_turn_selections", "category"},
			{"subagent_accounting", "category"},
		} {
			table, column := column[0], column[1]
			schemaTestExecWhenColumn(t, connection, table, column, fmt.Sprintf("ALTER TABLE %s DROP COLUMN %s;", table, column))
		}
	}
	if version < 145 {
		restorePre145ReviewColumns(t, connection)
	}
	if version < 137 {
		schemaTestExec(t, connection, `DROP TRIGGER IF EXISTS review_reports_session_destroyed;
                     DROP TABLE IF EXISTS review_reports;`)
	}
	if version < 135 {
		schemaTestExec(t, connection, `DROP TRIGGER IF EXISTS subagent_accounting_kind_insert; DROP TRIGGER IF EXISTS subagent_accounting_kind_update;`)
		schemaTestExecWhenColumn(t, connection, "subagent_accounting", "kind", "ALTER TABLE subagent_accounting DROP COLUMN kind;")
	}
	if version < 134 {
		schemaTestExec(t, connection, "DROP TABLE IF EXISTS session_create_intents;")
	}
	if version < 133 {
		schemaTestExecWhenColumn(t, connection, "session_moves", "close_requested_operation_id", `DROP TRIGGER IF EXISTS invalidate_move_close_intent; ALTER TABLE session_moves DROP COLUMN close_requested_operation_id;`)
	}
	if version < 132 {
		for _, column := range []string{"container_cpus", "container_memory"} {
			if !schemaTestHasColumn(t, connection, "sessions", column) {
				schemaTestExec(t, connection, fmt.Sprintf("ALTER TABLE sessions ADD COLUMN %s TEXT;", column))
			}
		}
	}
	if version < 131 {
		schemaTestExecWhenColumn(t, connection, "sessions", "dismissed_failure_json", `DROP TRIGGER IF EXISTS clear_dismissed_failure; ALTER TABLE sessions DROP COLUMN dismissed_failure_json; ALTER TABLE sessions DROP COLUMN failure_generation;`)
	}
	if version < 129 {
		schemaTestExecWhenColumn(t, connection, "sessions", "notifications_ignored", `ALTER TABLE sessions DROP COLUMN notifications_ignored;`)
	}
	if version < 122 {
		dropEnvironmentSchema(t, connection)
	}
	if version < 111 {
		schemaTestExec(t, connection, `ALTER TABLE sessions DROP COLUMN oracle_choice;
                ALTER TABLE sessions DROP COLUMN oracle;
                DROP TABLE IF EXISTS oracle_preference;`)
	}
	if version < 86 {
		schemaTestExec(t, connection, "DROP TABLE IF EXISTS mailbox_reset_frontiers;")
	}
	if version < 67 {
		schemaTestExec(t, connection, `DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET;`)
	}
	if version < 85 {
		restoreGithubV84Schema(t, connection)
	}
	if version < 99 {
		schemaTestExec(t, connection, `DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away;`)
		schemaTestExecWhenColumn(t, connection, "materialized_sessions", "operator_away_event_frontier", `ALTER TABLE materialized_sessions DROP COLUMN operator_away_event_frontier;`)
	}
	if version < 100 {
		schemaTestExec(t, connection, `DROP TRIGGER IF EXISTS startup_steps_session_deleted;`)
	}
	if version < 101 {
		restorePre101SubagentPreference(t, connection)
	}
	if version < 102 {
		schemaTestExec(t, connection, `ALTER TABLE session_restart_intents DROP COLUMN handoff_json;`)
	}
	if version < 103 {
		schemaTestExec(t, connection, "DROP TABLE IF EXISTS quota_history;")
	}
	restoreReviewStateSchema(t, connection, version)
	// Historical fixtures must remove columns introduced by later revisions.
	if version < 94 {
		schemaTestExecWhenColumn(t, connection, "materialized_sessions", "input_required_event_frontier", `ALTER TABLE materialized_sessions DROP COLUMN input_required_event_frontier;`)
	}
	if version < 89 {
		schemaTestExecWhenColumn(t, connection, "session_restart_intents", "automatic", `ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json;`)
	}
	if version < 91 {
		schemaTestExecWhenColumn(t, connection, "session_restart_intents", "container_upgrade_json", `ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json;`)
	}
	if version < 84 {
		schemaTestExecWhenColumn(t, connection, "subagent_sessions", "notification_epoch", `ALTER TABLE subagent_sessions DROP COLUMN notification_epoch;`)
	}
	if version < 76 {
		schemaTestExecWhenColumn(t, connection, "mailbox_outbox", "failure", "ALTER TABLE mailbox_outbox DROP COLUMN failure;")
	}
	if version < 93 {
		schemaTestExecWhenColumn(t, connection, "session_turn_selections", "profile", `ALTER TABLE session_turn_selections DROP COLUMN profile;`)
	}
	schemaTestExec(t, connection, fmt.Sprintf("PRAGMA user_version = %d;", version))
	schemaTestExec(t, connection, "DELETE FROM schema_migrations WHERE version > ?1", version)
	if version >= compatibilityMetadataVersion {
		schemaTestExec(t, connection, `UPDATE schema_compatibility SET minimum_compatible_version=min(minimum_compatible_version, ?1)`, version)
	}
	// Rewinding before the brief-writer fence restores the historical
	// floor, including compatible revisions 133 through 135.
	if 132 <= version && version < 136 {
		schemaTestExec(t, connection, `UPDATE schema_compatibility SET minimum_compatible_version=132 WHERE singleton=1`)
	}
	if 137 <= version && version < 142 {
		schemaTestExec(t, connection, `UPDATE schema_compatibility SET minimum_compatible_version=137 WHERE singleton=1`)
	}
	if version == 30 {
		schemaTestExec(t, connection, `UPDATE schema_compatibility SET minimum_compatible_version = 30 WHERE singleton = 1`)
	}
	if version == 69 || version == 70 {
		schemaTestExec(t, connection, `UPDATE schema_compatibility SET minimum_compatible_version=69 WHERE singleton=1`)
	}
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)
}

// Revision 146 adds nullable category columns without moving the floor.
// Rows written at 145 stay, with category NULL. A new registration and
// its turn selection record the category, a session with no accounting
// row is `primary`, and a named insert or update that omits the column
// leaves an existing value alone.
func TestCategoryColumnMigrationPreservesRowsAndRecordsNewCategories(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "category.sqlite3")
	parent := session("parent", "project")
	oldChild := session("old-child", "project")
	newChild := session("new-child", "project")
	top := session("top", "project")
	legacyWriter := session("legacy-writer", "project")
	for _, record := range []core.SessionRecord{parent, oldChild, newChild, top, legacyWriter} {
		schemaTestSaveSession(t, path, record)
	}
	stampSchemaVersion(t, path, 149)

	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `INSERT INTO subagent_accounting(child_session_id, parent_session_id, task_name)
                     VALUES ('old-child', 'parent', 'legacy task');
                 INSERT INTO session_turn_selections(session_id, command_id, model, effort)
                     VALUES ('old-child', 'legacy-turn', 'claude-opus', 'high');
                 INSERT INTO session_turn_selections(session_id, command_id, model, effort)
                     VALUES ('top', 'legacy-top', 'gpt-6', 'medium');`)
	schemaTestMust(t, connection.Close())

	connection, err := open(path)
	schemaTestMust(t, err)
	t.Cleanup(func() { _ = connection.Close() })
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	if migrated := schemaTestInt(t, connection, `SELECT count(*) FROM schema_migrations WHERE version = 150`); migrated != 1 {
		t.Fatalf("revision 150 recorded %d times", migrated)
	}

	category := func(query string) *string {
		return schemaTestOptionalText(t, connection, query)
	}
	expectText := func(value *string, expected *string, what string) {
		t.Helper()
		if !reflect.DeepEqual(value, expected) {
			t.Fatalf("%s: %v, want %v", what, value, expected)
		}
	}
	text := func(value string) *string { return &value }
	expectText(category(`SELECT category FROM subagent_accounting WHERE child_session_id='old-child'`), nil, "a row written before the column exists stays NULL")
	expectText(category(`SELECT task_name FROM subagent_accounting WHERE child_session_id='old-child'`), text("legacy task"), "task name")
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='old-child' AND command_id='legacy-turn'`), nil, "legacy turn category")
	expectText(category(`SELECT model FROM session_turn_selections
                 WHERE session_id='old-child' AND command_id='legacy-turn'`), text("claude-opus"), "legacy turn model")

	relation := core.SubagentRecord{
		ResumeWithParent: false,
		Kind:             core.SubagentKindWorker,
		ReviewerLaunch:   nil,
		ChildSessionID:   newChild.ID,
		ParentSessionID:  parent.ID,
		TaskName:         "subtle probe",
		ProfileID:        "codex",
		Model:            text("claude-opus-5-5"),
		Effort:           text("high"),
		WorkingDirectory: "",
		InitialPrompt:    "look carefully",
		RequestKey:       "subtle-1",
		CreatedAt:        newChild.CreatedAt,
		NoticedTurn:      nil,
		ReportedFinish:   nil,
		HandbackTool:     false,
		Orchestrator:     false,
	}
	schemaTestMust(t, recordChildAccounting(connection, &relation, text("subtle")))
	schemaTestMust(t, recordChildAccounting(connection, &relation, text("oracle")))
	expectText(category(`SELECT category FROM subagent_accounting WHERE child_session_id='new-child'`), text("subtle"), "a replay does not replace the category already stored")
	var configuration core.SessionConfiguration
	schemaTestMust(t, json.Unmarshal([]byte(`{"model": "claude-opus-5-5", "effort": "high"}`), &configuration))
	schemaTestMust(t, recordTurnSelection(connection, "new-child", "turn-1", &configuration))
	schemaTestMust(t, recordTurnSelection(connection, "new-child", "turn-1", &configuration))
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='new-child' AND command_id='turn-1'`), text("subtle"), "child turn category")
	// The accounting row exists and its category is NULL, so the turn
	// copies NULL instead of becoming primary.
	schemaTestMust(t, recordTurnSelection(connection, "old-child", "after-migration", &configuration))
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='old-child' AND command_id='after-migration'`), nil, "turn of a legacy child")
	schemaTestMust(t, recordTurnSelection(connection, "top", "turn-primary", &configuration))
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='top' AND command_id='turn-primary'`), text("primary"), "top-level turn")
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='top' AND command_id='legacy-top'`), nil, "the turn written before the column exists is not backfilled")

	schemaTestExec(t, connection, `UPDATE subagent_accounting SET task_name='renamed' WHERE child_session_id='new-child'`)
	schemaTestExec(t, connection, `UPDATE session_turn_selections SET model='other-model'
                 WHERE session_id='new-child' AND command_id='turn-1'`)
	expectText(category(`SELECT category FROM subagent_accounting WHERE child_session_id='new-child'`), text("subtle"), "a named update keeps the accounting category")
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='new-child' AND command_id='turn-1'`), text("subtle"), "a named update keeps the turn category")
	schemaTestExec(t, connection, `INSERT INTO subagent_accounting(child_session_id, parent_session_id, task_name)
                     VALUES ('legacy-writer', 'parent', 'omitted');
                 INSERT INTO session_turn_selections(session_id, command_id, model, effort)
                     VALUES ('legacy-writer', 'omitted-turn', 'model', 'high');`)
	expectText(category(`SELECT category FROM subagent_accounting WHERE child_session_id='legacy-writer'`), nil, "an older accounting insert")
	expectText(category(`SELECT category FROM session_turn_selections
                 WHERE session_id='legacy-writer' AND command_id='omitted-turn'`), nil, "an older turn insert")
}

func TestOracleMigrationEnablesExistingSessionsAndPersistsOverride(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "oracle.sqlite3")
	record := session("existing", "project")
	schemaTestSaveSession(t, path, record)
	connection := schemaTestOpen(t, path)
	restorePre153TranscriptLayout(t, connection)
	dropEnvironmentSchema(t, connection)
	restorePre145ReviewColumns(t, connection)
	schemaTestExec(t, connection, `ALTER TABLE sessions DROP COLUMN no_github_credentials;
             ALTER TABLE sessions DROP COLUMN oracle_choice;
             ALTER TABLE sessions DROP COLUMN oracle;
             DROP TABLE oracle_preference;
             ALTER TABLE turn_review_state DROP COLUMN declined_findings;
             DELETE FROM schema_migrations WHERE version >= 111;
             UPDATE schema_compatibility SET minimum_compatible_version = 110 WHERE singleton = 1;
             PRAGMA user_version = 110;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)
	connection = schemaTestWriter(t, path)
	schemaTestMustFail(t, schemaTestState(t, connection).ensureSupportedBy(110), "a revision-110 build")
	schemaTestMust(t, connection.Close())
	if !schemaTestSession(t, schemaTestLoadState(t, path), "existing").Oracle {
		t.Fatal("an existing session lost its oracle")
	}
	effort := "high"
	record.Oracle = false
	record.OracleChoice = &core.SubagentChoice{Model: "advisor", Effort: &effort}
	schemaTestSaveSession(t, path, record)
	loaded := schemaTestSession(t, schemaTestLoadState(t, path), "existing")
	if loaded.Oracle {
		t.Fatal("the oracle override was lost")
	}
	if !reflect.DeepEqual(loaded.OracleChoice, record.OracleChoice) {
		t.Fatalf("oracle choice %#v", loaded.OracleChoice)
	}
}

func TestSubagentPolicyMigrationPreservesLegacyChoicesAndRefusesOldWriters(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "subagent-policy.sqlite3")
	for _, id := range []string{"all", "native", "unset"} {
		schemaTestSaveSession(t, path, session(id, "project"))
	}
	connection := schemaTestOpen(t, path)
	restorePre153TranscriptLayout(t, connection)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 55)
	schemaTestExec(t, connection, `UPDATE sessions SET mjolnir_subagents = 1 WHERE session_id = 'all';
             UPDATE sessions SET mjolnir_subagents = 0 WHERE session_id = 'native';
             ALTER TABLE sessions DROP COLUMN subagents;
             DROP TABLE subagent_preference;
             DELETE FROM schema_migrations WHERE version >= 56;
             UPDATE schema_compatibility SET minimum_compatible_version = 55;
             DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 55;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)
	upgraded := schemaTestWriter(t, path)
	schemaTestMustFail(t, schemaTestState(t, upgraded).ensureSupportedBy(55), "a revision-55 build")
	schemaTestMust(t, upgraded.Close())
	state := schemaTestLoadState(t, path)
	if policy := schemaTestSession(t, state, "all").Subagents; !reflect.DeepEqual(policy, &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}) {
		t.Fatalf("all: %#v", policy)
	}
	if policy := schemaTestSession(t, state, "native").Subagents; !reflect.DeepEqual(policy, &core.SubagentPolicy{Mode: core.SubagentPolicyNative}) {
		t.Fatalf("native: %#v", policy)
	}
	if policy := schemaTestSession(t, state, "unset").Subagents; policy != nil {
		t.Fatalf("unset: %#v", policy)
	}
	if policy := state.LastSubagentPolicyFor("codex"); !reflect.DeepEqual(policy, core.SubagentPolicy{Mode: core.SubagentPolicyNative}) {
		t.Fatalf("remembered %#v", policy)
	}
}

func TestQuotaHistoryMigrationIsAtomicAndPreservesCompatibilityWithPreviousWriters(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "quota-history.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	stampSchemaVersion(t, path, 102)
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `INSERT INTO quota_reset_cache VALUES ('account', '{"windows":[]}');
             CREATE TRIGGER fail_quota_history BEFORE INSERT ON schema_migrations WHEN NEW.version=103
                 BEGIN SELECT RAISE(ABORT,'quota history migration'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the interrupted migration")
	schemaTestExec(t, connection, "ROLLBACK; DROP TRIGGER fail_quota_history;")
	if revision := schemaTestState(t, connection).revision; revision != 102 {
		t.Fatalf("revision %d", revision)
	}
	if schemaTestBool(t, connection, `SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='quota_history')`) {
		t.Fatal("the interrupted migration left quota_history")
	}
	schemaTestMust(t, migrateSchema(connection))
	schema := schemaTestState(t, connection)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	schemaTestMust(t, schema.ensureSupportedBy(minimumCompatibleVersion))
	if body := schemaTestText(t, connection, `SELECT body FROM quota_reset_cache WHERE identity='account'`); body != `{"windows":[]}` {
		t.Fatalf("cache %s", body)
	}

	// Revision 102 writes this cache and session state, but never history.
	// Observations have no foreign keys to operational records.
	schemaTestExec(t, connection, `INSERT INTO quota_history(identity,profile_id,harness,sampled_at_epoch_seconds,body)
                 VALUES ('account','codex','codex',123,'{"windows":[]}');
             UPDATE quota_reset_cache SET body='{}' WHERE identity='account';`)
	record := TestSession("kept", "project")
	schemaTestSaveSession(t, path, record)
	record.Title = "Updated by the existing writer"
	schemaTestSaveSession(t, path, record)
	schemaTestExec(t, connection, "DELETE FROM sessions WHERE session_id=?1", record.ID)
	if body := schemaTestText(t, connection, "SELECT body FROM quota_history"); body != `{"windows":[]}` {
		t.Fatalf("history %s", body)
	}
}

func TestRestartHandoffMigrationPreservesLegacyIntentsAndRefusesOldRecovery(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "restart-handoff.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	stampSchemaVersion(t, path, 101)
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('kept','project','now');
             INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
                 VALUES ('kept','Keep my work','codex','codex','podman','closing','now');
             INSERT INTO session_restart_intents(session_id,operation_id,phase,updated_at,checkpoint_sha256,checkpoint_started)
                 VALUES ('kept','restart-kept','stopping','now','sealed-sha',1);
             CREATE TRIGGER fail_restart_handoff BEFORE INSERT ON schema_migrations WHEN NEW.version=102
                 BEGIN SELECT RAISE(ABORT,'restart handoff migration'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the interrupted migration")
	schemaTestExec(t, connection, "ROLLBACK; DROP TRIGGER fail_restart_handoff;")
	schema := schemaTestState(t, connection)
	schemaTestExpectState(t, schema, 101, 101)
	if schemaTestHasColumn(t, connection, "session_restart_intents", "handoff_json") {
		t.Fatal("the interrupted migration added handoff_json")
	}
	schemaTestMust(t, migrateSchema(connection))
	schema = schemaTestState(t, connection)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	schemaTestMustFail(t, schema.ensureSupportedBy(101), "a revision-101 build")
	intent, err := loadSessionRestartWith(connection, "kept")
	schemaTestMust(t, err)
	if intent == nil {
		t.Fatal("the restart intent is gone")
	}
	if intent.OperationID != "restart-kept" || intent.CheckpointSHA256 == nil || *intent.CheckpointSHA256 != "sealed-sha" ||
		intent.Handoff != nil || !intent.CheckpointStarted {
		t.Fatalf("intent %#v", intent)
	}
}

func TestProfileSubagentPreferenceMigrationPreservesFallbackAndRefusesOldWriters(t *testing.T) {
	native := core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	for _, c := range []struct {
		name             string
		remembered       *core.SubagentPolicy
		failFirstAttempt bool
	}{
		{"populated", &native, true},
		{"empty", nil, false},
	} {
		directory := t.TempDir()
		path := filepath.Join(directory, c.name+".sqlite3")
		schemaTestMust(t, schemaTestWriter(t, path).Close())
		stampSchemaVersion(t, path, 100)
		legacy := schemaTestOpen(t, path)
		if c.remembered != nil {
			schemaTestExec(t, legacy, `INSERT INTO subagent_preference(singleton, policy) VALUES(1, ?1)`, schemaTestJSON(t, *c.remembered))
		}
		if c.failFirstAttempt {
			schemaTestExec(t, legacy, `CREATE TRIGGER fail_profile_preference_migration
                     BEFORE INSERT ON schema_migrations WHEN NEW.version=101
                     BEGIN SELECT RAISE(ABORT,'profile preference migration'); END;`)
			schemaTestMustFail(t, migrateSchema(legacy), "the interrupted migration")
			schemaTestExec(t, legacy, `ROLLBACK; DROP TRIGGER fail_profile_preference_migration;`)
			interrupted := schemaTestState(t, legacy)
			schemaTestExpectState(t, interrupted, 100, 100)
			if schemaTestHasColumn(t, legacy, "subagent_preference", "profile_id") {
				t.Fatal("the interrupted migration added profile_id")
			}
		}
		schemaTestMust(t, migrateSchema(legacy))
		schema := schemaTestState(t, legacy)
		schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
		schemaTestMustFail(t, schema.ensureSupportedBy(100), "a revision-100 build")
		if !schemaTestHasColumn(t, legacy, "subagent_preference", "profile_id") {
			t.Fatal("profile_id is missing")
		}
		if schemaTestHasColumn(t, legacy, "subagent_preference", "singleton") {
			t.Fatal("singleton remains")
		}
		schemaTestMust(t, legacy.Close())

		state := schemaTestLoadState(t, path)
		remembered, ok := state.LastSubagentPolicies[""]
		if ok != (c.remembered != nil) || (ok && !reflect.DeepEqual(remembered, *c.remembered)) {
			t.Fatalf("%s: remembered %#v", c.name, remembered)
		}
	}
}

func TestSteeringMigrationRefusesBuildsThatCannotReadReturnedSteers(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "steering-migration.sqlite3")
	record := session("steered-session", "project")
	schemaTestSaveSession(t, path, record)
	connection := schemaTestOpen(t, path)
	restorePre153TranscriptLayout(t, connection)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 47)
	schemaTestExec(t, connection, `DELETE FROM schema_migrations WHERE version >= 48;
             UPDATE schema_compatibility SET minimum_compatible_version = 47;
             DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 47;`)
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)
	upgraded := schemaTestWriter(t, path)
	schema := schemaTestState(t, upgraded)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	schemaTestIncompatible(t, schema.ensureSupportedBy(47), minimumCompatibleVersion)
	schemaTestMust(t, upgraded.Close())
	if loaded := schemaTestSession(t, schemaTestLoadState(t, path), record.ID); !reflect.DeepEqual(loaded, record) {
		t.Fatalf("loaded %#v\nwant %#v", loaded, record)
	}
}

func TestDurableTargetMigrationPreservesSessionsAndRefusesPreviousBuilds(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "target-migration.sqlite3")
	record := session("preserved-session", "project")
	schemaTestSaveSession(t, path, record)
	connection := schemaTestOpen(t, path)
	restorePre153TranscriptLayout(t, connection)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 45)
	schemaTestExec(t, connection, `ALTER TABLE sessions DROP COLUMN target_runtime_json;
             DELETE FROM schema_migrations WHERE version >= 46;
             UPDATE schema_compatibility SET minimum_compatible_version = 44;
             DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 45;`)
	forgetVerifiedSchema(path)
	upgraded := schemaTestWriter(t, path)
	schema := schemaTestState(t, upgraded)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	schemaTestIncompatible(t, schema.ensureSupportedBy(45), minimumCompatibleVersion)
	schemaTestMust(t, upgraded.Close())
	if restored := schemaTestSession(t, schemaTestLoadState(t, path), record.ID); !reflect.DeepEqual(restored, record) {
		t.Fatalf("restored %#v\nwant %#v", restored, record)
	}
}

func TestNativeAgentsAndUnstructuredInputRaiseTheStoreCompatibilityFloor(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	connection := schemaTestWriter(t, path)
	restorePre153TranscriptLayout(t, connection)
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 38)
	schemaTestExec(t, connection, `BEGIN IMMEDIATE;
             DROP TABLE quota_reset_cache;
             DROP TABLE native_agent_transcript;
             DROP TABLE native_agents;
             DROP TABLE native_agent_replay;
             DELETE FROM schema_migrations WHERE version >= 39;
             UPDATE schema_compatibility SET minimum_compatible_version = 32;
             DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_restart_intents DROP COLUMN container_upgrade_json; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 38;
             COMMIT;`)
	schemaTestMust(t, migrateSchema(connection))
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	turnID := uint64(1)
	event := core.ApiEventDataInputRequired{Request: nil, TurnID: &turnID}
	encoded, err := core.MarshalApiEventData(event)
	schemaTestMust(t, err)
	var envelope struct {
		Data json.RawMessage `json:"data"`
	}
	schemaTestMust(t, json.Unmarshal(encoded, &envelope))
	// A legacy reader requires the structured request.
	var legacy struct {
		Request *json.RawMessage `json:"request"`
	}
	schemaTestMust(t, json.Unmarshal(envelope.Data, &legacy))
	var request core.ElicitationRequest
	if legacy.Request != nil && json.Unmarshal(*legacy.Request, &request) == nil {
		t.Fatal("a legacy reader decoded an input event without a request")
	}
}

func TestPermissionCallIdsRaiseTheStoreCompatibilityFloor(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	record := session("approval", "project")
	record.State = core.SessionStateStopped
	schemaTestSaveSession(t, path, record)
	request, err := core.ElicitationRequestFromACPParams("tool-permission-0", json.RawMessage(`{
		"mode": "form", "sessionId": "approval", "message": "Run it?",
		"requestedSchema": {"type":"object", "required":["choice"],"properties":{
			"choice":{"type":"string", "oneOf":[{"const":"approved", "title":"Yes"}]}}}
	}`))
	schemaTestMust(t, err)
	request.ToolPermission = &core.ToolPermissionForm{Skip: true, ToolCallID: nil}
	materialized := MaterializedSession("approval")
	materialized.PendingElicitations = []core.ElicitationRequest{request}
	schemaTestMust(t, SaveMaterializedSessionTo(path, &materialized))

	stampSchemaVersion(t, path, 142)
	previous := schemaTestState(t, schemaTestOpen(t, path))
	if schemaTestFloor(previous) != 142 {
		t.Fatalf("floor %d", schemaTestFloor(previous))
	}
	schemaTestMust(t, previous.ensureSupportedBy(142))
	upgraded := schemaTestWriter(t, path)
	schema := schemaTestState(t, upgraded)
	if schema.revision != schemaVersion {
		t.Fatalf("revision %d", schema.revision)
	}
	// A later breaking revision can raise the floor further.
	schemaTestIncompatible(t, schema.ensureSupportedBy(142), minimumCompatibleVersion)
	schemaTestMust(t, upgraded.Close())

	load := func() *core.MaterializedSession {
		loaded, err := LoadMaterializedSessionFrom(path, "approval")
		schemaTestMust(t, err)
		if loaded == nil {
			t.Fatal("the materialized session is gone")
		}
		return loaded
	}
	if pending := load().PendingElicitations; !reflect.DeepEqual(pending, []core.ElicitationRequest{request}) {
		t.Fatalf("pending %#v", pending)
	}
	callID := "call-1"
	request.ToolPermission.ToolCallID = &callID
	materialized.PendingElicitations = []core.ElicitationRequest{request}
	schemaTestMust(t, SaveMaterializedSessionTo(path, &materialized))
	if pending := load().PendingElicitations; !reflect.DeepEqual(pending, []core.ElicitationRequest{request}) {
		t.Fatalf("pending %#v", pending)
	}
}

func TestNativeUseArchiveMigrationFencesRevision145(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())

	stampSchemaVersion(t, path, 145)
	previous := schemaTestState(t, schemaTestOpen(t, path))
	schemaTestExpectState(t, previous, 145, 145)
	schemaTestMust(t, previous.ensureSupportedBy(145))

	upgraded := schemaTestWriter(t, path)
	schema := schemaTestState(t, upgraded)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	reason := schemaTestMismatch(t, schema.ensureSupportedBy(145)).Reason
	if incompatible, ok := reason.(core.StoreSchemaMismatchReasonIncompatible); !ok || incompatible.MinimumCompatible < 146 {
		t.Fatalf("reason %#v", reason)
	}
}

func TestCompatibleFailureReceiptsAreInvalidatedByFailureWriters(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	record := session("failure", "project")
	record.State = core.SessionStateStopped
	failure := "resume failed"
	record.LastError = &failure
	schemaTestSaveSession(t, path, record)
	stampSchemaVersion(t, path, 130)
	connection := schemaTestWriter(t, path)
	if floor := schemaTestFloor(schemaTestState(t, connection)); floor != minimumCompatibleVersion {
		t.Fatalf("floor %d", floor)
	}
	receipt := schemaTestJSON(t, core.FailureIdentityOf(&record))
	schemaTestExec(t, connection, `UPDATE sessions SET dismissed_failure_json=?1 WHERE session_id='failure'`, receipt)
	// The real upsert names state/error even when only unrelated fields
	// change; it must preserve the receipt for that same failure.
	record.Title = "renamed"
	record.DraftInput = "draft"
	schemaTestSaveSession(t, path, record)
	if dismissed := schemaTestSession(t, schemaTestLoadState(t, path), "failure").DismissedFailure; !reflect.DeepEqual(dismissed, core.FailureIdentityOf(&record)) {
		t.Fatalf("dismissed %#v", dismissed)
	}
	// A retry passes through a different state before failing identically.
	record.State = core.SessionStateProvisioning
	schemaTestSaveSession(t, path, record)
	record.State = core.SessionStateStopped
	schemaTestSaveSession(t, path, record)
	if dismissed := schemaTestSession(t, schemaTestLoadState(t, path), "failure").DismissedFailure; dismissed != nil {
		t.Fatalf("dismissed %#v", dismissed)
	}
}

func TestMoveCloseIntentMigrationPreservesOlderColumnWrites(t *testing.T) {
	const name = "TestMoveCloseIntentMigrationPreservesOlderColumnWrites"
	const child = "MJ_TEST_MOVE_CLOSE_MIGRATION"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName("store::database::schema::reader_tests", name)).
			IsolatedStore(directory).
			Env(child, "1").
			Run()
		return
	}
	path := filepath.Join(core.DataDir(), "move-close-compat.sqlite3")
	for _, id := range []string{"source", "later"} {
		schemaTestSaveSession(t, path, session(id, "project"))
	}
	stampSchemaVersion(t, path, 132)
	// Exact explicit-column UPSERT a revision-132 writer issues. It still
	// preserves the later column mechanically; revision 134 refuses such
	// a writer outright because it cannot record finished review output.
	olderWrite := func(connection *Connection, sessionID string, operation string) {
		t.Helper()
		operationJSON := schemaTestJSON(t, map[string]any{"operation_id": operation, "cancellation_requested": true})
		schemaTestExec(t, connection, `INSERT INTO session_moves(session_id, operation_id, operation_json,source_environment_id)
                VALUES (?1,?2,?3,(SELECT environment_id FROM environment_bindings WHERE session_id=?1))
                ON CONFLICT(session_id) DO UPDATE SET source_environment_id=CASE WHEN session_moves.operation_id=excluded.operation_id THEN session_moves.source_environment_id ELSE excluded.source_environment_id END,
                operation_id=excluded.operation_id,
                operation_json=CASE WHEN session_moves.operation_id=excluded.operation_id AND json_extract(session_moves.operation_json, '$.cancellation_requested')=1
                    THEN json_set(excluded.operation_json, '$.cancellation_requested', json('true')) ELSE excluded.operation_json END`, sessionID, operation, operationJSON)
	}
	old := schemaTestOpen(t, path)
	olderWrite(old, "source", "old-move")
	schemaTestExec(t, old, `CREATE TRIGGER reject_close_migration BEFORE INSERT ON schema_migrations WHEN NEW.version=133 BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;`)
	schemaTestMustFail(t, migrateSchema(old), "the interrupted migration")
	schemaTestExpectState(t, schemaTestState(t, old), 132, 132)
	if schemaTestHasColumn(t, old, "session_moves", "close_requested_operation_id") {
		t.Fatal("the interrupted migration added the close intent")
	}
	schemaTestExec(t, old, "DROP TRIGGER reject_close_migration")
	schemaTestMust(t, old.Close())
	writer := schemaTestWriter(t, path)
	schema := schemaTestState(t, writer)
	if schema.revision != schemaVersion {
		t.Fatalf("revision %d", schema.revision)
	}
	// The later fences refuse older daemons; the column preservation
	// checks below still exercise revision-132 SQL.
	if schemaTestFloor(schema) != minimumCompatibleVersion {
		t.Fatalf("floor %d", schemaTestFloor(schema))
	}
	schemaTestMustFail(t, schema.ensureSupportedBy(132), "a revision-132 build")
	schemaTestMust(t, schema.ensureSupportedBy(minimumCompatibleVersion))
	intent := func(sessionID string) *string {
		return schemaTestOptionalText(t, writer, `SELECT close_requested_operation_id FROM session_moves WHERE session_id=?1`, sessionID)
	}
	expectIntent := func(sessionID string, expected *string) {
		t.Helper()
		if got := intent(sessionID); !reflect.DeepEqual(got, expected) {
			t.Fatalf("%s close intent %v, want %v", sessionID, got, expected)
		}
	}
	oldMove := "old-move"
	expectIntent("source", nil)
	schemaTestExec(t, writer, `UPDATE session_moves SET close_requested_operation_id=operation_id WHERE session_id='source'`)
	olderWrite(writer, "source", "old-move")
	schemaTestExec(t, writer, `UPDATE session_moves SET operation_json=json_set(operation_json,'$.cancellation_requested',json('true')), source_environment_id=source_environment_id WHERE session_id='source'`)
	expectIntent("source", &oldMove)
	olderWrite(writer, "later", "inserted-move")
	expectIntent("later", nil)
	olderWrite(writer, "source", "new-move")
	expectIntent("source", nil)
	schemaTestExec(t, writer, `UPDATE session_moves SET close_requested_operation_id='old-move' WHERE session_id='source'`)
	if schemaTestBool(t, writer, `SELECT close_requested_operation_id=operation_id FROM session_moves WHERE session_id='source'`) {
		t.Fatal("a stale close intent matches the new operation")
	}
	schemaTestExec(t, writer, `UPDATE session_moves SET close_requested_operation_id=operation_id WHERE session_id='source'`)
	// Older retry code reuses the operation ID, but explicitly resetting
	// cancellation invalidates the previous request in the same transaction.
	schemaTestExec(t, writer, `UPDATE session_moves SET operation_json=json_set(operation_json,'$.cancellation_requested',json('false')) WHERE session_id='source'`)
	expectIntent("source", nil)
}

func TestCreateIntentMigrationKeepsOlderWritersWorkingAndSettlesWithTheSession(t *testing.T) {
	const name = "TestCreateIntentMigrationKeepsOlderWritersWorkingAndSettlesWithTheSession"
	const child = "MJ_TEST_CREATE_INTENT_MIGRATION"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName("store::database::schema::reader_tests", name)).
			IsolatedStore(directory).
			Env(child, "1").
			Run()
		return
	}
	path := filepath.Join(core.DataDir(), "create-intent-compat.sqlite3")
	existing := session("existing", "project")
	existing.State = core.SessionStateProvisioning
	schemaTestSaveSession(t, path, existing)
	stampSchemaVersion(t, path, 133)
	// A revision-133 daemon fails the provisioning session and rewrites the row.
	old := schemaTestOpen(t, path)
	schemaTestExec(t, old, `UPDATE sessions SET state='error' WHERE session_id='existing'`)
	schemaTestMust(t, old.Close())
	writer := schemaTestWriter(t, path)
	schemaTestExpectState(t, schemaTestState(t, writer), schemaVersion, minimumCompatibleVersion)
	schemaTestMust(t, writer.Close())
	intents := func(path string) []string {
		connection := schemaTestOpen(t, path)
		defer connection.Close()
		return schemaTestTexts(t, connection, `SELECT session_id FROM session_create_intents ORDER BY session_id`)
	}
	if found := intents(path); len(found) != 0 {
		t.Fatalf("intents %v", found)
	}
	created := session("created", "project")
	created.State = core.SessionStateProvisioning
	schemaTestMust(t, saveNewSessionTo(path, &created, nil, true, false))
	if found := intents(path); !slices.Equal(found, []string{"created"}) {
		t.Fatalf("intents %v", found)
	}
	// Provisioning writes keep it, including the move to Disconnected.
	created.State = core.SessionStateDisconnected
	schemaTestSaveSession(t, path, created)
	schemaTestMust(t, saveLifecycleSessionTo(path, &created))
	if found := intents(path); !slices.Equal(found, []string{"created"}) {
		t.Fatalf("intents %v", found)
	}
	// Committing or failing for good settles it with the same write.
	created.State = core.SessionStateRunning
	schemaTestMust(t, markSessionWorkerConnectedTo(path, "created", nil, "now"))
	if found := intents(path); len(found) != 0 {
		t.Fatalf("intents %v", found)
	}
	failed := session("failed", "project")
	failed.State = core.SessionStateProvisioning
	schemaTestMust(t, saveNewSessionTo(path, &failed, nil, false, false))
	failed.State = core.SessionStateError
	schemaTestMust(t, saveLifecycleSessionTo(path, &failed))
	if found := intents(path); len(found) != 0 {
		t.Fatalf("intents %v", found)
	}
}

func TestOlderReadersAndReopenedWritersPreserveACompatibleFutureSchema(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	connection := schemaTestWriter(t, path)
	schemaTestExec(t, connection, `CREATE TABLE future_feature(value TEXT NOT NULL);
                 INSERT INTO future_feature VALUES ('preserve me');`)
	schemaTestMust(t, connection.Close())
	advanceTestSchema(t, path, schemaVersion+1, schemaVersion)

	reader, err := openReaderStrict(path)
	schemaTestMust(t, err)
	if value := schemaTestText(t, reader, "SELECT value FROM future_feature"); value != "preserve me" {
		t.Fatalf("value %q", value)
	}
	if _, err := reader.Exec("DELETE FROM future_feature"); err == nil {
		t.Fatal("a reader deleted rows")
	}
	reader.Close()

	// A repair would recreate this deliberately removed trigger. A future
	// schema is authoritative even when it differs from our own repairs.
	raw := schemaTestOpen(t, path)
	schemaTestExec(t, raw, "DROP TRIGGER session_contexts_workspace_update;")
	schemaTestMust(t, raw.Close())
	writer := schemaTestWriter(t, path)
	if schemaTestBool(t, writer, `SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'session_contexts_workspace_update')`) {
		t.Fatal("a writer repaired a future schema")
	}
	if value := schemaTestText(t, writer, "SELECT value FROM future_feature"); value != "preserve me" {
		t.Fatalf("value %q", value)
	}
	state := schemaTestState(t, writer)
	schemaTestExpectState(t, state, schemaVersion+1, schemaVersion)
}

// schemaTestValue is a JSON object or value as serde_json::Value, for the
// fixtures below that edit stored JSON the way an older build wrote it.
func schemaTestValue(t testing.TB, value any) map[string]any {
	t.Helper()
	var object map[string]any
	schemaTestMust(t, json.Unmarshal([]byte(schemaTestJSON(t, value)), &object))
	return object
}

func schemaTestClone(t testing.TB, value map[string]any) map[string]any {
	t.Helper()
	return schemaTestValue(t, value)
}

func TestOutcomeFactsMigrationRaisesTheFloor(t *testing.T) {
	// Exercise the removed-column migration against the previous writer's data,
	// including its applied snapshot, before checking older-store refusal.
	text := func(value string) *string { return &value }
	sizes := func(cpus uint64, memory uint64) *[2]uint64 { return &[2]uint64{cpus, memory} }
	cases := []struct {
		id         string
		cpus       *string
		memory     *string
		allocation core.SessionResourceAllocation
		expected   *[2]uint64
	}{
		{"whole", text("4"), text("8g"), nil, sizes(4, 8<<30)},
		{"fraction", text("1.5"), text("700m"), nil, sizes(2, 700<<20)},
		{"partial", text("6"), nil, core.SessionResourceAllocationContainer{Cpus: 2, MemoryBytes: 4 << 30}, sizes(6, 4<<30)},
		{"missing", text("6"), nil, nil, nil},
		{"invalid", nil, text("lots"), nil, nil},
		{"ec2", text("4"), text("8g"), core.SessionResourceAllocationAwsEc2{InstanceType: "m5.large", Vcpus: 2, MemoryBytes: 8 << 30}, nil},
	}
	fixture := t.TempDir()
	oldPath := filepath.Join(fixture, "sizes.sqlite3")
	for _, c := range cases {
		record := session(c.id, "project")
		record.ResourceAllocation = c.allocation
		schemaTestSaveSession(t, oldPath, record)
	}
	stampSchemaVersion(t, oldPath, 130)
	raw := schemaTestOpen(t, oldPath)
	for _, c := range cases {
		schemaTestExec(t, raw, `UPDATE sessions SET container_cpus=?2,container_memory=?3 WHERE session_id=?1`, c.id, c.cpus, c.memory)
	}
	schemaTestExec(t, raw, "UPDATE sessions SET target_runtime_json=?1 WHERE session_id='whole'",
		`{"kind":"local-podman","container_settings":{"additional_mounts":[],"resource_allocation":null,"cpus":"3","memory":"6g"}}`)
	runtimeValue := schemaTestValue(t, core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{}))
	runtimeValue["environment"].(map[string]any)["container_cpus"] = "keep-this-variable"
	runtimeValue["container_settings"] = map[string]any{"additional_mounts": []any{}, "resource_allocation": nil, "cpus": "3", "memory": "6g"}
	recovery := schemaTestValue(t, session("whole", "project"))
	recovery["resource_allocation"] = map[string]any{"kind": "container", "cpus": 2, "memory_bytes": uint64(4) << 30}
	recovery["container_cpus"] = "1.5"
	recovery["container_memory"] = "700m"
	recovery["target_runtime"] = runtimeValue
	transferSource := schemaTestClone(t, recovery)
	transferSource["container_cpus"] = "6"
	delete(transferSource, "container_memory")
	prepared := map[string]any{"launch_args": []any{}, "runtime": runtimeValue, "state": map[string]any{"phase": "launch_pending"}}
	operation := map[string]any{
		"operation_id": "legacy-size-move", "selection": map[string]any{"session_id": "whole"},
		"source_profile_id": "codex", "source_target_template_id": "local",
		"source_target": nil, "source_native_session_id": nil, "source_additional_mounts": []any{},
		"source_resource_allocation": nil, "destination_target": nil,
		"destination_native_session_id": nil, "destination_store_id": nil,
		"configuration_fingerprint": "kept", "checkpoint": nil, "recovery_session": recovery,
		"prepared_destination": prepared, "queue": "start", "phase": "capturing",
		"queue_admission_started": false, "queue_admission_finished": false,
		"cancellation_requested": false, "created_at": "created", "updated_at": "updated", "error": nil,
	}
	schemaTestExec(t, raw, `INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES('whole','legacy-size-move',?1)`, schemaTestJSON(t, operation))
	schemaTestExec(t, raw, `INSERT INTO retained_move_sources(operation_id,session_id,source_json,exclusions_json,created_at) VALUES('legacy-size-move','whole',?1,'[]','created')`, schemaTestJSON(t, transferSource))
	var environmentID, placementJSON string
	schemaTestRow(t, raw, `SELECT e.environment_id,e.placement_json FROM environments e JOIN environment_bindings b USING(environment_id) WHERE b.session_id='whole'`, nil, &environmentID, &placementJSON)
	var placement map[string]any
	schemaTestMust(t, json.Unmarshal([]byte(placementJSON), &placement))
	placement["runtime"] = runtimeValue
	placement["prepared_destination"] = map[string]any{"operation_id": "legacy-size-move", "destination": prepared}
	schemaTestExec(t, raw, `UPDATE environments SET placement_json=?2 WHERE environment_id=?1`, environmentID, schemaTestJSON(t, placement))
	schemaTestExec(t, raw, `UPDATE environment_workers SET runtime_json=?1 WHERE worker_session_id='whole'`, schemaTestJSON(t, runtimeValue))
	preparing := schemaTestClone(t, operation)
	preparing["operation_id"] = "preparing-size-move"
	preparing["selection"].(map[string]any)["session_id"] = "partial"
	preparing["recovery_session"] = nil
	preparing["prepared_destination"] = nil
	preparing["source_resource_allocation"] = map[string]any{"kind": "container", "cpus": 2, "memory_bytes": uint64(4) << 30}
	schemaTestExec(t, raw, `INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES('partial','preparing-size-move',?1)`, schemaTestJSON(t, preparing))
	changes, err := migrate(raw)
	schemaTestMust(t, err)
	if len(changes) != len(cases)+5 {
		t.Fatalf("changes %d", len(changes))
	}
	if schemaTestHasColumn(t, raw, "sessions", "container_cpus") || schemaTestHasColumn(t, raw, "sessions", "container_memory") {
		t.Fatal("the legacy size columns remain")
	}
	for _, c := range cases {
		var allocation core.SessionResourceAllocation
		if stored := schemaTestOptionalText(t, raw, `SELECT resource_allocation FROM sessions WHERE session_id=?1`, c.id); stored != nil {
			allocation, err = core.UnmarshalSessionResourceAllocationJSON([]byte(*stored))
			schemaTestMust(t, err)
		}
		expected := c.allocation
		if c.expected != nil {
			expected = core.SessionResourceAllocationContainer{Cpus: c.expected[0], MemoryBytes: c.expected[1]}
		}
		if allocation != expected {
			t.Fatalf("%s: allocation %#v, want %#v", c.id, allocation, expected)
		}
	}
	var runtime map[string]any
	schemaTestMust(t, json.Unmarshal([]byte(schemaTestText(t, raw, `SELECT target_runtime_json FROM sessions WHERE session_id='whole'`)), &runtime))
	settings := runtime["container_settings"].(map[string]any)
	if _, ok := settings["cpus"]; ok {
		t.Fatal("cpus remains")
	}
	if _, ok := settings["memory"]; ok {
		t.Fatal("memory remains")
	}
	if allocation := schemaTestJSON(t, settings["resource_allocation"]); allocation != `{"cpus":3,"kind":"container","memory_bytes":6442450944}` {
		t.Fatalf("snapshot allocation %s", allocation)
	}
	preparingMove, err := loadMoveOperationWith(raw, "partial")
	schemaTestMust(t, err)
	if preparingMove == nil || preparingMove.SourceResourceAllocation != (core.SessionResourceAllocationContainer{Cpus: 6, MemoryBytes: 4 << 30}) {
		t.Fatalf("preparing move %#v", preparingMove)
	}
	moved, err := loadMoveOperationWith(raw, "whole")
	schemaTestMust(t, err)
	// Hard-won: #1538: a pre-upgrade Move intent with override keys stopped decoding
	if moved == nil {
		t.Fatal("the migrated durable Move must remain recoverable")
	}
	converted := core.SessionResourceAllocationContainer{Cpus: 2, MemoryBytes: 700 << 20}
	if moved.SourceResourceAllocation != converted {
		t.Fatalf("source allocation %#v", moved.SourceResourceAllocation)
	}
	if moved.RecoverySession.ResourceAllocation != converted {
		t.Fatalf("recovery allocation %#v", moved.RecoverySession.ResourceAllocation)
	}
	expectedSnapshot := core.SessionResourceAllocationContainer{Cpus: 3, MemoryBytes: 6 << 30}
	for _, runtime := range []*core.TargetRuntimeSettings{
		moved.RecoverySession.TargetRuntime,
		&moved.PreparedDestination.Runtime,
	} {
		if runtime.Environment["container_cpus"] != "keep-this-variable" {
			t.Fatalf("environment %v", runtime.Environment)
		}
		if runtime.ContainerSettings.ResourceAllocation != expectedSnapshot {
			t.Fatalf("snapshot %#v", runtime.ContainerSettings.ResourceAllocation)
		}
	}
	var retained core.SessionRecord
	schemaTestMust(t, json.Unmarshal([]byte(schemaTestText(t, raw, `SELECT source_json FROM retained_move_sources WHERE operation_id='legacy-size-move'`)), &retained))
	if retained.ResourceAllocation != (core.SessionResourceAllocationContainer{Cpus: 6, MemoryBytes: 4 << 30}) {
		t.Fatalf("retained allocation %#v", retained.ResourceAllocation)
	}
	if retained.TargetRuntime.ContainerSettings.ResourceAllocation != expectedSnapshot {
		t.Fatalf("retained snapshot %#v", retained.TargetRuntime.ContainerSettings.ResourceAllocation)
	}
	environment, err := readEnvironment(raw, environmentID)
	schemaTestMust(t, err)
	if environment == nil {
		t.Fatal("the environment is gone")
	}
	if environment.Placement.Runtime.ContainerSettings.ResourceAllocation != expectedSnapshot {
		t.Fatalf("placement snapshot %#v", environment.Placement.Runtime.ContainerSettings.ResourceAllocation)
	}
	if environment.Placement.PreparedDestination.Destination.Runtime.ContainerSettings.ResourceAllocation != expectedSnapshot {
		t.Fatal("prepared destination snapshot")
	}
	var workerRuntime core.TargetRuntimeSettings
	schemaTestMust(t, json.Unmarshal([]byte(schemaTestText(t, raw, `SELECT runtime_json FROM environment_workers WHERE worker_session_id='whole'`)), &workerRuntime))
	if workerRuntime.ContainerSettings.ResourceAllocation != expectedSnapshot {
		t.Fatalf("worker snapshot %#v", workerRuntime.ContainerSettings.ResourceAllocation)
	}
	schema := schemaTestState(t, raw)
	if schemaTestFloor(schema) != 132 {
		t.Fatalf("floor %d", schemaTestFloor(schema))
	}
	schemaTestIncompatible(t, schema.ensureSupportedBy(131), 132)
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestSaveSession(t, path, session("facts-upgrade", "project"))
	stampSchemaVersion(t, path, 117)
	connection := schemaTestWriter(t, path)
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	// A revision-117 build is refused: its decoder of a stored outcome
	// rejects the field a sub-agent's worker adds.
	schemaTestMustFail(t, state.ensureSupportedBy(117), "a revision-117 build")
	schemaTestMustFail(t, state.ensureSupportedBy(118), "a revision-118 build")
	schemaTestMust(t, state.ensureSupportedBy(schemaVersion))
}

func TestGithubCredentialPolicySurvivesMigrationAndRefusesOlderWriters(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	record := session("credential-upgrade", "project")
	schemaTestSaveSession(t, path, record)
	stampSchemaVersion(t, path, 118)
	connection := schemaTestWriter(t, path)
	schemaTestMustFail(t, schemaTestState(t, connection).ensureSupportedBy(118), "a revision-118 build")
	schemaTestMust(t, connection.Close())
	if schemaTestSession(t, schemaTestLoadState(t, path), record.ID).NoGithubCredentials {
		t.Fatal("the migration withheld credentials")
	}
	record.NoGithubCredentials = true
	schemaTestSaveSession(t, path, record)
	if !schemaTestSession(t, schemaTestLoadState(t, path), record.ID).NoGithubCredentials {
		t.Fatal("the credential policy was lost")
	}
}

func TestMailboxResetMigrationSeedsExistingFrontiersAndPreservesCompatibility(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	record := session("mailbox-upgrade", "project")
	schemaTestSaveSession(t, path, record)
	stampSchemaVersion(t, path, 85)
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `UPDATE materialized_sessions SET applied_event_ordinal=42 WHERE session_id=?1`, record.ID)
	schemaTestMust(t, connection.Close())
	connection = schemaTestWriter(t, path)
	schemaTestMust(t, schemaTestState(t, connection).ensureSupportedBy(minimumCompatibleVersion))
	frontier := schemaTestInt(t, connection, `SELECT through_ordinal FROM mailbox_reset_frontiers WHERE session_id=?1`, record.ID)
	if frontier != 42 {
		t.Fatalf("frontier %d", frontier)
	}
	schemaTestMust(t, connection.Close())
	schemaTestSaveSession(t, path, record)
	connection = schemaTestWriter(t, path)
	if preserved := schemaTestInt(t, connection, `SELECT through_ordinal FROM mailbox_reset_frontiers WHERE session_id=?1`, record.ID); preserved != frontier {
		t.Fatalf("preserved %d", preserved)
	}
	schemaTestExec(t, connection, "DELETE FROM sessions WHERE session_id=?1", record.ID)
	if schemaTestExists(t, connection, `SELECT 1 FROM mailbox_reset_frontiers WHERE session_id=?1`, record.ID) {
		t.Fatal("the frontier outlived its session")
	}
}

func TestPromptHistoryKeepsTopLevelPromptsAndItsIndexFollowsPreviousWriters(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	connection := schemaTestWriter(t, path)
	restorePre153TranscriptLayout(t, connection)
	dropEnvironmentSchema(t, connection)
	// Rewind to revision 115, then store what a revision-115 build kept:
	// a top-level prompt, a child's spawn instructions and a destroyed
	// session's prompt.
	restorePre116PromptHistory(t, connection, 115)
	schemaTestExec(t, connection, `ALTER TABLE sessions DROP COLUMN no_github_credentials;
                 DELETE FROM schema_migrations WHERE version >= 116;
                 UPDATE schema_compatibility SET minimum_compatible_version = 114;
                 PRAGMA user_version = 115;`)
	sessionSQL := func(id string) string {
		return `INSERT INTO session_contexts(session_id, bundle_id, created_at)
                     VALUES ('` + id + `', 'project', 'now');
                 INSERT INTO sessions(session_id, title, harness_kind, last_profile,
                                      target_template_id, state, updated_at)
                     VALUES ('` + id + `', '` + id + `', 'codex', 'codex', 'local', 'error', 'now');`
	}
	prompt := func(id string, ordinal int64, text string) string {
		return fmt.Sprintf(`INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
                     VALUES ('%s', %d, 'now', '%s');`, id, ordinal, text)
	}
	schemaTestExec(t, connection, fmt.Sprintf(`%s%s%s
                 INSERT INTO subagent_sessions(child_session_id, parent_session_id,
                                               request_key, record_json)
                     VALUES ('child', 'top', 'child', '{}');
                 INSERT INTO session_contexts(session_id, bundle_id, created_at)
                     VALUES ('destroyed', 'project', 'now');
                 %s%s%s`,
		sessionSQL("top"),
		sessionSQL("child"),
		sessionSQL("other"),
		prompt("top", 1, "Fix the Parser"),
		prompt("child", 1, "parser instructions for a child"),
		prompt("destroyed", 1, "parser prompt of a destroyed session"),
	))
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)

	connection = schemaTestWriter(t, path)
	state := schemaTestState(t, connection)
	if state.revision != schemaVersion {
		t.Fatalf("revision %d", state.revision)
	}
	if err := state.ensureSupportedBy(minimumCompatibleVersion); err != nil {
		t.Fatalf("the store is readable at the compatibility floor: %v", err)
	}
	matching := func(query string) []string {
		return schemaTestTexts(t, connection, `SELECT p.text FROM prompt_history_text f
                     JOIN prompt_history p ON p.history_id = f.rowid
                     WHERE prompt_history_text MATCH ?1 ORDER BY f.rowid`, query)
	}
	stored := func() int64 {
		return schemaTestInt(t, connection, "SELECT count(*) FROM prompt_history")
	}
	expectMatching := func(query string, expected ...string) {
		t.Helper()
		if found := matching(query); !slices.Equal(found, expected) {
			t.Fatalf("%s matched %v, want %v", query, found, expected)
		}
	}
	expectMatching(`"the parser"`, "Fix the Parser")
	if count := stored(); count != 1 {
		t.Fatalf("child and destroyed-session prompts are deleted: %d", count)
	}

	// The writes a revision-115 build makes, in plain SQL.
	schemaTestExec(t, connection, fmt.Sprintf(`%s%s
                 UPDATE prompt_history SET text = 'renamed prompt'
                     WHERE session_id = 'top' AND event_ordinal = 1;`,
		prompt("child", 2, "parser follow-up for a child"),
		prompt("top", 2, "parser follow-up"),
	))
	expectMatching(`"parser"`, "parser follow-up")
	expectMatching(`"renamed"`, "renamed prompt")
	if count := stored(); count != 2 {
		t.Fatalf("a child's prompt is never stored: %d", count)
	}

	// A prompt stored before its session became a child goes when it does.
	schemaTestExec(t, connection, fmt.Sprintf(`%s%s
                 INSERT INTO subagent_sessions(child_session_id, parent_session_id,
                                               request_key, record_json)
                     VALUES ('late', 'top', 'late', '{}');`,
		sessionSQL("late"),
		prompt("late", 1, "parser instructions stored early"),
	))
	expectMatching(`"parser"`, "parser follow-up")

	// Destroying a session removes its prompts and their index entries.
	schemaTestExec(t, connection, fmt.Sprintf(`%s
                 DELETE FROM sessions WHERE session_id = 'other';`,
		prompt("other", 1, "parser prompt of another session"),
	))
	expectMatching(`"parser"`, "parser follow-up")
	if count := stored(); count != 2 {
		t.Fatalf("stored %d", count)
	}
	// Hard-won: a58ad7eb8: a stale chat's abandoned prompt was stored after its session was destroyed
	schemaTestExec(t, connection, prompt("other", 2, "parser prompt after destroy")+
		prompt("destroyed", 2, "parser prompt with only a context"))
	if count := stored(); count != 2 {
		t.Fatalf("a destroyed session's prompt is not stored: %d", count)
	}
	if _, err := connection.Exec(`INSERT INTO prompt_history_text(prompt_history_text) VALUES ('integrity-check')`); err != nil {
		t.Fatalf("the index agrees with prompt_history: %v", err)
	}
}

func TestStoppedTargetlessSessionsLoseTheirWorkerRestartIntentsAndOthersKeepThem(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	connection := schemaTestWriter(t, path)
	restorePre153TranscriptLayout(t, connection)
	dropEnvironmentSchema(t, connection)
	// Rewind to revision 116 and store what a revision-116 build left:
	// intents of a closed child, of a stopped session that still names a
	// container, of a failed child and of a live session.
	schemaTestExec(t, connection, `ALTER TABLE sessions DROP COLUMN no_github_credentials;
                 DELETE FROM schema_migrations WHERE version >= 117;
                 UPDATE schema_compatibility SET minimum_compatible_version = 114;
                 PRAGMA user_version = 116;`)
	for _, c := range []struct {
		id, state, phase string
		target           bool
	}{
		{"closed-child", "stopped", "swapping", false},
		{"kept-container", "stopped", "awaiting_readiness", true},
		{"failed-child", "error", "awaiting_readiness", false},
		{"live", "running", "swapping", false},
	} {
		id, state, phase := c.id, c.state, c.phase
		schemaTestExec(t, connection, `INSERT INTO session_contexts(session_id, bundle_id, created_at)
                         VALUES ('`+id+`', 'project', 'now');
                     INSERT INTO sessions(session_id, title, harness_kind, last_profile,
                                          target_template_id, state, updated_at)
                         VALUES ('`+id+`', '`+id+`', 'codex', 'codex', 'local', '`+state+`', 'now');
                     INSERT INTO worker_restart_intents(session_id, operation_id, target_json,
                                                        desired_build, phase)
                         VALUES ('`+id+`', 'op-`+id+`', '{}', '', '`+phase+`');`)
		if c.target {
			schemaTestExec(t, connection, `INSERT INTO session_targets(session_id, kind, resource_id)
                         VALUES (?1, 'local-podman', 'container')`, id)
		}
	}
	schemaTestMust(t, connection.Close())
	forgetVerifiedSchema(path)

	connection = schemaTestWriter(t, path)
	state := schemaTestState(t, connection)
	if state.revision != schemaVersion {
		t.Fatalf("revision %d", state.revision)
	}
	if err := state.ensureSupportedBy(minimumCompatibleVersion); err != nil {
		t.Fatalf("a revision-116 build still reads and writes the store: %v", err)
	}
	if remaining := schemaTestTexts(t, connection, `SELECT session_id FROM worker_restart_intents ORDER BY session_id`); !slices.Equal(remaining, []string{"failed-child", "kept-container", "live"}) {
		t.Fatalf("remaining %v", remaining)
	}
}

func TestTurnProfileMigrationBackfillsLiveSessionsAndStaysCompatible(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	for _, c := range [][2]string{{"kept", "codex2"}, {"destroyed", "codex"}} {
		record := session(c[0], "project")
		record.LastProfile = c[1]
		schemaTestSaveSession(t, path, record)
	}
	stampSchemaVersion(t, path, 92)
	old := schemaTestOpen(t, path)
	schemaTestExec(t, old, `INSERT INTO session_turn_selections(session_id,command_id,model,effort)
                 VALUES ('kept','one','sol','high'),('destroyed','one','luna','low');
             DELETE FROM sessions WHERE session_id='destroyed';`)
	schemaTestMust(t, old.Close())
	boundary := schemaTestOpen(t, path)
	schemaTestExec(t, boundary, `CREATE TRIGGER released_boundary BEFORE INSERT ON schema_migrations WHEN NEW.version=95 BEGIN SELECT RAISE(ABORT,'released boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(boundary), "the migration to the boundary")
	schemaTestExec(t, boundary, "ROLLBACK")
	upgraded := boundary
	state := schemaTestState(t, upgraded)
	if state.revision != 94 {
		t.Fatalf("revision %d", state.revision)
	}
	schemaTestMust(t, state.ensureSupportedBy(92))
	// A revision-92 daemon names its columns and leaves the profile unknown.
	schemaTestExec(t, upgraded, `INSERT INTO session_turn_selections(session_id,command_id,model,effort)
                 VALUES ('kept','two','sol','high');`)
	rows, err := upgraded.Query(`SELECT session_id, command_id, profile FROM session_turn_selections ORDER BY 1, 2`)
	schemaTestMust(t, err)
	type selection struct {
		session, command string
		profile          sql.NullString
	}
	profiles := []selection{}
	for rows.Next() {
		var row selection
		schemaTestMust(t, rows.Scan(&row.session, &row.command, &row.profile))
		profiles = append(profiles, row)
	}
	schemaTestMust(t, errors.Join(rows.Err(), rows.Close()))
	expected := []selection{
		{"destroyed", "one", sql.NullString{}},
		{"kept", "one", sql.NullString{String: "codex2", Valid: true}},
		{"kept", "two", sql.NullString{}},
	}
	if !reflect.DeepEqual(profiles, expected) {
		t.Fatalf("profiles %v", profiles)
	}
}

func TestInvalidCompatibilityMetadataRefusesReadersAndWriters(t *testing.T) {
	for _, alteration := range []string{
		"DROP TABLE schema_compatibility",
		"DELETE FROM schema_compatibility",
		`PRAGMA ignore_check_constraints = ON; UPDATE schema_compatibility SET minimum_compatible_version = 0`,
		`UPDATE schema_compatibility SET minimum_compatible_version = 99999`,
		`PRAGMA ignore_check_constraints = ON; UPDATE schema_compatibility SET singleton = 2`,
		`PRAGMA ignore_check_constraints = ON; INSERT INTO schema_compatibility VALUES (2, 30)`,
		`DROP TABLE schema_compatibility; CREATE TABLE schema_compatibility(singleton, minimum_compatible_version); INSERT INTO schema_compatibility VALUES (1, 'invalid')`,
		`DELETE FROM schema_migrations WHERE version = (SELECT max(version) FROM schema_migrations)`,
	} {
		for _, future := range []bool{false, true} {
			directory := t.TempDir()
			path := filepath.Join(directory, "mj.sqlite3")
			schemaTestMust(t, schemaTestWriter(t, path).Close())
			if future {
				advanceTestSchema(t, path, schemaVersion+1, schemaVersion)
			}
			raw := schemaTestOpen(t, path)
			dropEnvironmentSchema(t, raw)
			dropEnvironmentSchema(t, raw)
			schemaTestExec(t, raw, alteration)
			before := schemaTestInt(t, raw, "PRAGMA schema_version")
			// Exercise the cached path as well as a fresh writer open.
			_, readerErr := openReaderStrict(path)
			_, writerErr := openWriter(path)
			for _, err := range []error{readerErr, writerErr} {
				schemaTestMustFail(t, err, alteration)
				if reason := schemaTestMismatch(t, err).Reason; reason != (core.StoreSchemaMismatchReasonInvalidCompatibilityMetadata{}) {
					t.Fatalf("%s: reason %#v", alteration, reason)
				}
			}
			forgetVerifiedSchema(path)
			if writer, err := openWriter(path); err == nil {
				writer.Close()
				t.Fatalf("%s: a writer opened the store", alteration)
			}
			if after := schemaTestInt(t, raw, "PRAGMA schema_version"); before != after {
				t.Fatalf("a rejected open repaired schema: %s", alteration)
			}
		}
	}
}

func TestAFailedBaselineLeavesAnEmptyStoreThatARetryCreates(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	connection := schemaTestOpen(t, path)
	// A table the baseline also creates makes its batch fail part way.
	schemaTestExec(t, connection, "CREATE TABLE workspaces(conflict TEXT)")

	err := migrateSchema(connection)

	if err == nil || !strings.Contains(err.Error(), "create baseline database schema") {
		t.Fatalf("migration error %v", err)
	}
	if !connection.isAutocommit() {
		t.Fatal("the failed baseline left a transaction open")
	}
	if revision := schemaTestState(t, connection).revision; revision != 0 {
		t.Fatalf("revision %d", revision)
	}
	if tables := schemaTestInt(t, connection, `SELECT count(*) FROM sqlite_schema WHERE type = 'table'`); tables != 1 {
		t.Fatalf("only the conflicting table remains: %d", tables)
	}

	schemaTestExec(t, connection, "DROP TABLE workspaces")
	schemaTestMust(t, connection.Close())
	writer := schemaTestWriter(t, path)
	schemaTestExpectState(t, schemaTestState(t, writer), schemaVersion, minimumCompatibleVersion)
}

// A store ahead of this build cannot be fixed by starting a daemon of
// this build, so the reader must not say so. This is the message the
// incident in #24 printed twice a second for an hour.
func TestOperatorAwayMigrationRefusesPreviousReadersAndWriters(t *testing.T) {
	const child = "MJ_OPERATOR_AWAY_MIGRATION_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName("store::database::schema::reader_tests",
			"TestOperatorAwayMigrationRefusesPreviousReadersAndWriters")).
			Env(child, "1").
			IsolatedStore(directory).
			Run()
		return
	}
	root := t.TempDir()
	path := filepath.Join(root, "away-migration.sqlite")
	schemaTestSaveSession(t, path, session("session", "project"))
	stampSchemaVersion(t, path, 98)
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `CREATE TRIGGER fail_away_migration BEFORE INSERT ON schema_migrations WHEN NEW.version=99 BEGIN SELECT RAISE(ABORT,'away migration'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the interrupted migration")
	schemaTestExec(t, connection, "ROLLBACK; DROP TRIGGER fail_away_migration;")
	before := schemaTestState(t, connection)
	schemaTestExpectState(t, before, 98, 98)
	if schemaTestHasColumn(t, connection, "materialized_sessions", "operator_away_event_frontier") {
		t.Fatal("the interrupted migration added the frontier")
	}
	schemaTestExec(t, connection, `CREATE TRIGGER stop_before_profile_preference BEFORE INSERT ON schema_migrations WHEN NEW.version=101 BEGIN SELECT RAISE(ABORT,'profile preference migration'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestExec(t, connection, `ROLLBACK; DROP TRIGGER stop_before_profile_preference;`)
	revision100 := schemaTestState(t, connection)
	schemaTestExpectState(t, revision100, 100, 100)
	if !schemaTestBool(t, connection, `SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='trigger' AND name='startup_steps_session_deleted')`) {
		t.Fatal("the startup step trigger is missing")
	}
	schemaTestExec(t, connection, `CREATE TRIGGER stop_before_stored_answers BEFORE INSERT ON schema_migrations WHEN NEW.version=113 BEGIN SELECT RAISE(ABORT,'stored answers migration'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestExec(t, connection, "ROLLBACK; DROP TRIGGER stop_before_stored_answers;")
	beforeAnswers := schemaTestState(t, connection)
	schemaTestExpectState(t, beforeAnswers, 112, 111)
	schemaTestMust(t, beforeAnswers.ensureSupportedBy(112))
	schemaTestMust(t, migrateSchema(connection))
	schema := schemaTestState(t, connection)
	schemaTestExpectState(t, schema, schemaVersion, minimumCompatibleVersion)
	schemaTestIncompatible(t, schema.ensureSupportedBy(112), minimumCompatibleVersion)
	schemaTestIncompatible(t, schema.ensureSupportedBy(98), minimumCompatibleVersion)
	if schemaTestFloor(schema) != minimumCompatibleVersion {
		t.Fatalf("floor %d", schemaTestFloor(schema))
	}
	schemaTestExec(t, connection, "UPDATE operator_away SET away=1,revision=7")
	schemaTestExec(t, connection, `UPDATE materialized_sessions SET operator_away_event_frontier=20 WHERE session_id='session'`)
	// New writers preserve the durable flag and replay frontier across ordinary session updates.
	schemaTestExec(t, connection, `UPDATE materialized_sessions SET applied_event_ordinal=2,pending_elicitations_json='[]' WHERE session_id='session'`)
	if frontier := schemaTestInt(t, connection, `SELECT operator_away_event_frontier FROM materialized_sessions WHERE session_id='session'`); frontier != 20 {
		t.Fatalf("frontier %d", frontier)
	}
	if !schemaTestLoadState(t, path).OperatorPresence.Away {
		t.Fatal("the operator is not away")
	}
	if revision := schemaTestInt(t, connection, "SELECT revision FROM operator_away"); revision != 7 {
		t.Fatalf("revision %d", revision)
	}
}

func TestQuestionFrontierMigrationIsAtomicAndPreservedByPreviousWriters(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "question-frontier.sqlite")
	schemaTestSaveSession(t, path, session("child", "project"))
	connection := schemaTestOpen(t, path)
	schemaTestExec(t, connection, `UPDATE materialized_sessions SET applied_event_ordinal=7 WHERE session_id='child'`)
	schemaTestMust(t, connection.Close())
	stampSchemaVersion(t, path, 92)
	connection = schemaTestOpen(t, path)
	schemaTestExec(t, connection, `CREATE TRIGGER stop_question_migration BEFORE INSERT ON schema_migrations WHEN NEW.version=94 BEGIN SELECT RAISE(ABORT,'question frontier fixture'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the interrupted migration")
	schemaTestExec(t, connection, "ROLLBACK; DROP TRIGGER stop_question_migration;")
	if schemaTestHasColumn(t, connection, "materialized_sessions", "input_required_event_frontier") {
		t.Fatal("the interrupted migration added the frontier")
	}
	if revision := schemaTestState(t, connection).revision; revision != 93 {
		t.Fatalf("revision %d", revision)
	}
	schemaTestExec(t, connection, `CREATE TRIGGER released_boundary BEFORE INSERT ON schema_migrations WHEN NEW.version=95 BEGIN SELECT RAISE(ABORT,'released boundary'); END;`)
	schemaTestMustFail(t, migrateSchema(connection), "the migration to the boundary")
	schemaTestExec(t, connection, "ROLLBACK")
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, 94, 92)
	schemaTestMust(t, state.ensureSupportedBy(92))
	// The old writer names only the old projection columns. Its update
	// must not erase the new build's already-delivered question frontier.
	schemaTestExec(t, connection, `UPDATE materialized_sessions SET applied_event_ordinal=2, pending_elicitations_json='[]' WHERE session_id='child'`)
	if frontier := schemaTestInt(t, connection, `SELECT input_required_event_frontier FROM materialized_sessions WHERE session_id='child'`); frontier != 7 {
		t.Fatalf("frontier %d", frontier)
	}
}

func TestStrictReaderReportsANewerStoreWithoutBlamingTheDaemon(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	stampSchemaVersion(t, path, schemaVersion+1)

	_, err := openReaderStrict(path)

	schemaTestMustFail(t, err, "a newer store")
	mismatch := schemaTestMismatch(t, err)
	if mismatch.Found != schemaVersion+1 || mismatch.Supported != schemaVersion {
		t.Fatalf("mismatch %#v", mismatch)
	}
	message := mismatch.Error()
	if !strings.Contains(message, "upgrade Mjolnir") {
		t.Fatalf("got %s", message)
	}
	if strings.Contains(message, "start the Mjolnir daemon") {
		t.Fatalf("got %s", message)
	}
}

// A store behind this build keeps the advice that works, verbatim, so
// existing log greps and runbooks keep matching.
func TestStrictReaderKeepsTheMigrateAdviceWhenTheStoreIsBehind(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	raw := schemaTestOpen(t, path)
	schemaTestExec(t, raw, strings.ReplaceAll(`UPDATE schema_compatibility SET minimum_compatible_version = {0};
             DELETE FROM schema_migrations WHERE version > {0};
             INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES ({0}, 'test');
             PRAGMA user_version = {0};`, "{0}", strconv.FormatInt(schemaVersion-1, 10)))
	schemaTestMust(t, raw.Close())

	_, err := openReaderStrict(path)

	schemaTestMustFail(t, err, "an older store")
	expected := fmt.Sprintf("Mjolnir database schema %d is not the supported schema %d; "+
		"start the Mjolnir daemon to migrate it", schemaVersion-1, schemaVersion)
	if message := schemaTestMismatch(t, err).Error(); message != expected {
		t.Fatalf("got %s", message)
	}
}

func TestStrictReaderRejectsMutation(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())

	reader, err := openReaderStrict(path)
	schemaTestMust(t, err)
	defer reader.Close()
	_, err = reader.Exec("CREATE TABLE forbidden(value TEXT)")
	var failure *sqlite.Error
	if !errors.As(err, &failure) || failure.Code()&0xff != sqlite3.SQLITE_READONLY {
		t.Fatalf("unexpected mutation error: %v", err)
	}
}

func hasWorkspace(t testing.TB, reader Queryer, name string) bool {
	t.Helper()
	return schemaTestBool(t, reader, `SELECT EXISTS(SELECT 1 FROM workspaces WHERE name = ?1)`, name)
}

func addWorkspace(t testing.TB, path string, name string) {
	t.Helper()
	writer := schemaTestWriter(t, path)
	defer writer.Close()
	schemaTestExec(t, writer, `INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
                 VALUES (?1, ?1, ?1, 'now', 'now')`, name)
}

// schemaTestReader is open_reader_strict(path).unwrap().
func schemaTestReader(t testing.TB, path string) *reader {
	t.Helper()
	reader, err := openReaderStrict(path)
	schemaTestMust(t, err)
	return reader
}

// Reads reuse an idle connection instead of opening one and parsing the
// schema again, and still see every write committed since.
func TestStrictReadersAreReusedAndSeeLaterCommits(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	before := openedReaders.Load()
	reader := schemaTestReader(t, path)
	if hasWorkspace(t, reader, "later") {
		t.Fatal("the workspace exists before it is added")
	}
	reader.Close()
	addWorkspace(t, path, "later")
	for range 20 {
		reader := schemaTestReader(t, path)
		if !hasWorkspace(t, reader, "later") {
			t.Fatal("a reused reader missed a later commit")
		}
		reader.Close()
	}
	if opened := openedReaders.Load() - before; opened != 1 {
		t.Fatalf("one connection served every read: opened %d", opened)
	}

	// Two readers at once need two connections; both are kept afterwards.
	first := schemaTestReader(t, path)
	second := schemaTestReader(t, path)
	first.Close()
	second.Close()
	if opened := openedReaders.Load() - before; opened != 2 {
		t.Fatalf("opened %d", opened)
	}
	schemaTestReader(t, path).Close()
	schemaTestReader(t, path).Close()
	if opened := openedReaders.Load() - before; opened != 2 {
		t.Fatalf("opened %d", opened)
	}

	// A connection given back inside a transaction is not reused.
	reader = schemaTestReader(t, path)
	schemaTestExec(t, reader, "BEGIN")
	reader.Close()
	first, second = schemaTestReader(t, path), schemaTestReader(t, path)
	if !first.isAutocommit() || !second.isAutocommit() {
		t.Fatal("a reader was handed out inside a transaction")
	}
	first.Close()
	second.Close()
	if opened := openedReaders.Load() - before; opened != 3 {
		t.Fatalf("opened %d", opened)
	}
}

// An idle connection must not outlive the store it was opened on, and
// must not hide a migration another build made while it sat idle.
func TestIdleReadersFollowStoreReplacementAndSchemaChanges(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1299: Windows file locking and directory times")
	}
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	addWorkspace(t, path, "old store")
	schemaTestReader(t, path).Close()

	for _, suffix := range []string{"", "-wal", "-shm"} {
		file := path + suffix
		if _, err := os.Stat(file); err == nil {
			schemaTestMust(t, os.Remove(file))
		}
	}
	forgetVerifiedSchema(path)
	schemaTestMust(t, schemaTestWriter(t, path).Close())
	addWorkspace(t, path, "new store")
	reader := schemaTestReader(t, path)
	if !hasWorkspace(t, reader, "new store") || hasWorkspace(t, reader, "old store") {
		t.Fatal("a reader read the replaced store")
	}
	reader.Close()

	schemaTestReader(t, path).Close()
	stampSchemaVersion(t, path, schemaVersion+1)
	_, err := openReaderStrict(path)
	var mismatch core.StoreSchemaMismatch
	if !errors.As(err, &mismatch) {
		t.Fatalf("a reused connection checks compatibility like a new one: %v", err)
	}
}

func TestClosingConsentMigrationPersistsConsentAndRefusesRevision146(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	func() {
		connection := schemaTestWriter(t, path)
		defer connection.Close()
		restorePre153TranscriptLayout(t, connection)
		schemaTestExec(t, connection, `ALTER TABLE sessions DROP COLUMN closing_consent;
                     UPDATE schema_compatibility SET minimum_compatible_version = 146 WHERE singleton = 1;
                     DELETE FROM schema_migrations WHERE version >= 147;
                     PRAGMA user_version = 146;`)
	}()
	forgetVerifiedSchema(path)
	connection := schemaTestWriter(t, path)
	if !schemaTestHasColumn(t, connection, "sessions", "closing_consent") {
		t.Fatal("migration 147 must add sessions.closing_consent")
	}
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	schemaTestIncompatible(t, state.ensureSupportedBy(146), minimumCompatibleVersion)
}

func TestHostSnapshotCacheMigration148CreatesTable(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	func() {
		connection := schemaTestWriter(t, path)
		defer connection.Close()
		restorePre153TranscriptLayout(t, connection)
		schemaTestExec(t, connection, `DROP TABLE host_snapshot_cache;
                     UPDATE schema_compatibility SET minimum_compatible_version = 147 WHERE singleton = 1;
                     DELETE FROM schema_migrations WHERE version >= 148;
                     PRAGMA user_version = 147;`)
	}()
	forgetVerifiedSchema(path)
	connection := schemaTestWriter(t, path)
	if !schemaTestBool(t, connection, `SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'host_snapshot_cache')`) {
		t.Fatal("migration 148 must create host_snapshot_cache")
	}
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
}

func TestMoveOriginDeltaMigration149AddsGrantsAndRefusesRevision148(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	func() {
		connection := schemaTestWriter(t, path)
		defer connection.Close()
		restorePre153TranscriptLayout(t, connection)
		schemaTestExec(t, connection, `DROP TABLE ec2_move_grants;
                     DROP TABLE daemon_instance_metadata;
                     UPDATE schema_compatibility SET minimum_compatible_version = 147 WHERE singleton = 1;
                     DELETE FROM schema_migrations WHERE version >= 149;
                     PRAGMA user_version = 148;`)
	}()
	forgetVerifiedSchema(path)
	connection := schemaTestWriter(t, path)
	if grants := schemaTestInt(t, connection, `SELECT count(*) FROM sqlite_master WHERE type='table' AND name='ec2_move_grants'`); grants != 1 {
		t.Fatalf("migration 149 must create ec2_move_grants: %d", grants)
	}
	if metadata := schemaTestInt(t, connection, `SELECT count(*) FROM sqlite_master WHERE type='table' AND name='daemon_instance_metadata'`); metadata != 1 {
		t.Fatalf("migration 149 must create daemon_instance_metadata: %d", metadata)
	}
	state := schemaTestState(t, connection)
	schemaTestExpectState(t, state, schemaVersion, minimumCompatibleVersion)
	schemaTestIncompatible(t, state.ensureSupportedBy(148), minimumCompatibleVersion)
}
