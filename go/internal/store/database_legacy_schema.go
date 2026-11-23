// Port of mj-store/src/database/legacy_schema.rs.

// Historical migrations retained so upgrades never require an intermediate
// release. These are the original revision 2–33 steps, including their
// structural repairs. New stores use baseline.sql; existing stores keep the
// full forward path.
//
// Every step that runs its own BEGIN IMMEDIATE ... COMMIT batch, or opens a
// transaction, takes the *Connection it must stay on.

package store

import (
	"fmt"
)

func migrateToBaseline(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_to_baseline")
}

// migrateCompatibilityMetadata is the breaking baseline: earlier executables
// reject every newer revision. Later compatible migrations retain the floor;
// breaking ones raise it to their revision in the same transaction as their
// schema and ledger changes.
func migrateCompatibilityMetadata(connection *Connection) error {
	transaction, err := connection.Begin()
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	if _, err := transaction.Exec(`CREATE TABLE schema_compatibility (
             singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
             minimum_compatible_version INTEGER NOT NULL CHECK(minimum_compatible_version >= 30)
         ) STRICT;
         INSERT INTO schema_compatibility(singleton, minimum_compatible_version) VALUES (1, 30);
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (30, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 30;`); err != nil {
		return err
	}
	return transaction.Commit()
}

// migrateSubagentSessions is a breaking migration: older controllers do not
// understand that a child borrows its parent's target and could destroy
// shared resources.
func migrateSubagentSessions(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_subagent_sessions")
}

// migrateZcodeHarnessKind is a breaking migration: older controllers cannot
// deserialize the new harness enum and their table constraints reject ZCode
// rows written by that build.
//
// The ZCode harness has since been removed. The 'zcode' value is retained in
// the harness_kind CHECK constraint only so session rows written by earlier
// releases stay readable; no code accepts it, HarnessKind parsing rejects it,
// and loading state skips such a row with a warning. Removing the value
// would need another breaking migration that rewrote or deleted those rows,
// so it is left in place. Never rewrite this migration; give any later
// schema change a new revision.
func migrateZcodeHarnessKind(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_zcode_harness_kind")
}

func tableHasColumn(connection Queryer, table, column string) (bool, error) {
	var present bool
	err := connection.QueryRow(`SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info(?1)
                 WHERE name = ?2
             )`,
		table, column).Scan(&present)
	return present, err
}

func ensureWorkspaceSchema(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs ensure_workspace_schema")
}

// ensureClientSessionStateSchema keeps per-client unsent drafts.
//
// This is additive and separate from client_read_frontiers on purpose. A
// frontier is a cursor every client has; a draft is text one viewer typed and
// did not send, and it expires. Keeping them apart means the phone's
// retention policy cannot reach a terminal client's cursor.
func ensureClientSessionStateSchema(connection *Connection) error {
	_, err := connection.Exec(`CREATE TABLE IF NOT EXISTS client_session_state (
             client_id TEXT NOT NULL CHECK(length(trim(client_id)) > 0),
             workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
             session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
             draft TEXT NOT NULL DEFAULT '',
             updated_at TEXT NOT NULL,
             PRIMARY KEY(client_id, workspace_id, session_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS client_session_state_age
             ON client_session_state(updated_at);`)
	return err
}

// ensureSessionContainerOverrideColumns adds the per-session container size
// overrides. They are additive columns, so databases written before the
// dashboard could edit them open unchanged.
func ensureSessionContainerOverrideColumns(connection *Connection) error {
	for _, column := range []string{"container_cpus", "container_memory"} {
		present, err := tableHasColumn(connection, "sessions", column)
		if err != nil {
			return err
		}
		if !present {
			if _, err := connection.Exec(fmt.Sprintf(`BEGIN IMMEDIATE;
                 ALTER TABLE sessions ADD COLUMN %s TEXT;
                 COMMIT;`, column)); err != nil {
				return err
			}
		}
	}
	return nil
}

// migrateTurnOutcomeColumns adds the per-turn identity and outcome columns,
// the queue's acceptance ordinal, and the API idempotency ledger.
//
// Each addition is guarded by a structural check rather than by the version
// alone. A database rebuilt by another build's ladder — or by a test that
// rewinds user_version — can already carry some of these, and a bare ALTER
// TABLE would then fail the whole open.
func migrateTurnOutcomeColumns(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_turn_outcome_columns")
}

// ensureSessionMountReadOnlyColumn adds the per-mount read-only flag. It is
// an additive column, so a database written before the mount editors offered
// the option opens unchanged and its mounts keep the copy-on-write overlay
// they were provisioned with.
func ensureSessionMountReadOnlyColumn(connection *Connection) error {
	present, err := tableHasColumn(connection, "session_mounts", "read_only")
	if err != nil || present {
		return err
	}
	_, err = connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_mounts ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
             COMMIT;`)
	return err
}

func ensureMaterializedElicitationColumn(connection *Connection) error {
	present, err := tableHasColumn(connection, "materialized_sessions", "pending_elicitations_json")
	if err != nil || present {
		return err
	}
	_, err = connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE materialized_sessions
                 ADD COLUMN pending_elicitations_json TEXT NOT NULL DEFAULT '[]';
             COMMIT;`)
	return err
}

func ensureManagedWorktreeColumn(connection *Connection) error {
	present, err := tableHasColumn(connection, "sessions", "managed_worktree")
	if err != nil || present {
		return err
	}
	_, err = connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN managed_worktree TEXT;
             COMMIT;`)
	return err
}

// ensureRelayProjectionSchema completes the relay half of the colliding v6
// migration for databases first opened by master, whose v6 contained only
// managed_worktree.
func ensureRelayProjectionSchema(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs ensure_relay_projection_schema")
}

func migrateDestroyingSessionState(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_destroying_session_state")
}

// migrateGrokHarnessKind admits the Grok Build harness. SQLite cannot widen a
// CHECK constraint in place, so this repeats the v7 table rebuild with the
// wider harness list. Foreign keys are disabled only around the rebuild
// transaction; every child continues to reference the replacement table by
// the same name.
func migrateGrokHarnessKind(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_grok_harness_kind")
}

func migrateStoppedSessionState(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_stopped_session_state")
}

func migrateMuseHarnessKind(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_muse_harness_kind")
}

func migrateDeepseekHarnessKind(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs migrate_deepseek_harness_kind")
}

// ensureSessionDraftInputColumn carries unsent chat input across a detach.
// Added as a structural guard rather than a new schema version so databases
// written by either development line converge, matching
// ensureManagedWorktreeColumn.
func ensureSessionDraftInputColumn(connection *Connection) error {
	present, err := tableHasColumn(connection, "sessions", "draft_input")
	if err != nil || present {
		return err
	}
	_, err = connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN draft_input TEXT NOT NULL DEFAULT '';
             COMMIT;`)
	return err
}

func ensureProjectionDigestColumn(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs ensure_projection_digest_column")
}

func ensureAPIEventsSchema(connection *Connection) error {
	panic("unported: mj-store/src/database/legacy_schema.rs ensure_api_events_schema")
}
