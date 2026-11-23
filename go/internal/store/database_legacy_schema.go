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
	"errors"
	"fmt"
	"log/slog"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// legacyExec runs one batch on the connection, as rusqlite's execute_batch.
func legacyExec(connection Queryer, batch string) error {
	_, err := connection.Exec(batch)
	return err
}

// legacyHasRows is rusqlite's Statement::exists: whether the query returns
// any row.
func legacyHasRows(connection Queryer, query string) (bool, error) {
	rows, err := connection.Query(query)
	if err != nil {
		return false, err
	}
	defer rows.Close()
	if rows.Next() {
		return true, nil
	}
	return false, rows.Err()
}

func migrateToBaseline(connection *Connection) error {
	var version int64
	if err := connection.QueryRow("PRAGMA user_version").Scan(&version); err != nil {
		return err
	}
	if version < 2 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN resource_allocation TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 2;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 3 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN last_checkpoint_error TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 3;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 4 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN project_directory BLOB;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 4;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 5 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE session_targets RENAME TO session_targets_v4;
             CREATE TABLE session_targets (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','apple-container','aws-ec2','ssh-bare','ssh-podman')),
                 host TEXT,
                 resource_id TEXT,
                 address TEXT,
                 workspace BLOB,
                 worker_id TEXT,
                 CHECK(
                     (kind = 'local-bare' AND workspace IS NOT NULL
                      AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
                  OR (kind IN ('local-podman','apple-container') AND resource_id IS NOT NULL
                      AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
                      AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
                      AND resource_id IS NULL AND address IS NULL)
                  OR (kind = 'ssh-podman' AND host IS NOT NULL AND resource_id IS NOT NULL
                      AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                 )
             ) STRICT;
             INSERT INTO session_targets
                 SELECT * FROM session_targets_v4;
             DROP TABLE session_targets_v4;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 5;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 6 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE session_checkpoints
                 RENAME COLUMN event_sequence TO event_frontier;
             ALTER TABLE prompt_history
                 RENAME COLUMN event_sequence TO event_ordinal;
             ALTER TABLE sessions ADD COLUMN detached_after_event_ordinal INTEGER NOT NULL
                 DEFAULT 0 CHECK(detached_after_event_ordinal >= 0);
             ALTER TABLE sessions ADD COLUMN managed_worktree TEXT;
             CREATE TABLE materialized_sessions (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 applied_event_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(applied_event_ordinal >= 0),
                 applied_event_digest TEXT NOT NULL
                     DEFAULT '`+core.RelayEventGenesisDigest+`'
                     CHECK(length(applied_event_digest) = 64
                           AND applied_event_digest NOT GLOB '*[^0-9a-f]*'),
                 last_activity_at_ms INTEGER,
                 execution_state TEXT NOT NULL DEFAULT 'idle'
                     CHECK(execution_state IN ('idle','running','closing','closed')),
                 running_started_at_ms INTEGER,
                 session_title TEXT CHECK(session_title IS NULL OR length(trim(session_title)) > 0),
                 configuration_json TEXT NOT NULL DEFAULT '{}',
                 CHECK(
                     (execution_state = 'running' AND running_started_at_ms IS NOT NULL)
                     OR (execution_state != 'running' AND running_started_at_ms IS NULL)
                 )
             ) STRICT;
             CREATE TABLE materialized_transcript_items (
                 session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
                 stable_id TEXT NOT NULL CHECK(length(trim(stable_id)) > 0),
                 position INTEGER NOT NULL CHECK(position > 0),
                 latest_content_event_ordinal INTEGER
                     CHECK(latest_content_event_ordinal IS NULL
                           OR latest_content_event_ordinal >= position),
                 created_at_ms INTEGER NOT NULL,
                 last_changed_at_ms INTEGER NOT NULL CHECK(last_changed_at_ms >= created_at_ms),
                 body_json TEXT NOT NULL,
                 PRIMARY KEY(session_id, stable_id)
             ) STRICT;
             CREATE INDEX materialized_transcript_position
                 ON materialized_transcript_items(session_id, position, stable_id);
             CREATE TABLE materialized_queued_prompts (
                 session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                 command_id TEXT NOT NULL CHECK(length(trim(command_id)) > 0),
                 content_json TEXT NOT NULL,
                 queued_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(session_id, ordinal),
                 UNIQUE(session_id, command_id)
             ) STRICT;
             INSERT INTO materialized_sessions(session_id)
                 SELECT session_id FROM sessions;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 6;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Both development lines used schema version 6: durable relay
	// projection on this branch and managed raw-session worktrees on master.
	// Structural guards make either already-written v6 database converge
	// before the v7 sessions-table rebuild, without inventing a second
	// version-6 ledger row.
	if err := ensureManagedWorktreeColumn(connection); err != nil {
		return err
	}
	if version < 7 {
		if err := ensureRelayProjectionSchema(connection); err != nil {
			return err
		}
		if err := migrateDestroyingSessionState(connection); err != nil {
			return err
		}
	}
	if err := ensureProjectionDigestColumn(connection); err != nil {
		return err
	}
	if err := ensureSessionDraftInputColumn(connection); err != nil {
		return err
	}
	if version < 8 {
		// Queue entries gained a kind so a configuration change can wait in
		// the same queue as prompts. Rows written before that are prompts.
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE materialized_queued_prompts
                 ADD COLUMN kind_json TEXT NOT NULL DEFAULT '"prompt"';
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (8, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 8;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Runs last: it rebuilds `sessions`, so every column the steps above add
	// must already exist to be copied forward.
	if version < 9 {
		if err := migrateGrokHarnessKind(connection); err != nil {
			return err
		}
	}
	// Added after the v9 rebuild so the rebuild never has to copy them.
	if err := ensureSessionContainerOverrideColumns(connection); err != nil {
		return err
	}
	if err := ensureSessionMountReadOnlyColumn(connection); err != nil {
		return err
	}
	if err := ensureMaterializedElicitationColumn(connection); err != nil {
		return err
	}
	if err := legacyExec(connection, `CREATE TABLE IF NOT EXISTS profile_config_cache (
        profile TEXT NOT NULL, model TEXT NOT NULL, fingerprint TEXT NOT NULL,
        observed_at INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(profile, model));
        CREATE TABLE IF NOT EXISTS api_config_results (
        session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
        command_id TEXT NOT NULL, error TEXT, PRIMARY KEY(session_id, command_id));
        CREATE TABLE IF NOT EXISTS session_turn_usage (
        session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
        command_id TEXT NOT NULL, completed_ordinal INTEGER NOT NULL, turn_start_position INTEGER,
        body TEXT NOT NULL, PRIMARY KEY(session_id, command_id));
        CREATE INDEX IF NOT EXISTS session_turn_usage_order ON session_turn_usage(session_id, completed_ordinal);
        CREATE TABLE IF NOT EXISTS session_provider_cost (
        session_id TEXT PRIMARY KEY REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
        body TEXT NOT NULL);`); err != nil {
		return err
	}

	if version < 10 {
		if err := migrateStoppedSessionState(connection); err != nil {
			return err
		}
	}
	if version < 11 {
		if err := migrateDeepseekHarnessKind(connection); err != nil {
			return err
		}
	}
	if version < 12 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions
                 RENAME COLUMN detached_after_event_ordinal TO viewed_through_event_ordinal;
             UPDATE sessions
                 SET target_template_id = 'localhost'
                 WHERE target_template_id = 'raw-localhost';
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (12, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 12;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 13 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             UPDATE sessions
                SET state = 'error'
              WHERE state = 'lost'
                AND EXISTS(
                    SELECT 1
                      FROM session_checkpoints
                     WHERE session_checkpoints.session_id = sessions.session_id
                );
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (13, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 13;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 14 {
		if err := ensureWorkspaceSchema(connection); err != nil {
			return err
		}
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             INSERT OR IGNORE INTO schema_migrations(version, applied_at)
                 VALUES (14, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 14;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 15 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             CREATE TABLE host_container_sizes (
                 host TEXT PRIMARY KEY CHECK(length(trim(host)) > 0),
                 cpus INTEGER NOT NULL CHECK(cpus > 0),
                 memory_bytes INTEGER NOT NULL CHECK(memory_bytes > 0)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (15, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 15;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 16 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             CREATE TABLE second_opinion_defaults (
                 workspace_id TEXT NOT NULL CHECK(length(trim(workspace_id)) > 0),
                 profile_id TEXT NOT NULL CHECK(length(trim(profile_id)) > 0),
                 model TEXT NOT NULL,
                 effort TEXT NOT NULL,
                 PRIMARY KEY (workspace_id, profile_id, model)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (16, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 16;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 17 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             CREATE TABLE second_opinion_reviews (
                 session_id TEXT PRIMARY KEY
                     REFERENCES sessions(session_id) ON DELETE CASCADE,
                 workflow TEXT NOT NULL,
                 generation INTEGER NOT NULL CHECK(generation >= 0),
                 context_baseline INTEGER NOT NULL CHECK(context_baseline >= 0),
                 native_lost INTEGER NOT NULL CHECK(native_lost IN (0, 1))
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (17, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 17;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 18 {
		// The reviewer's conversation lives in its own journal on the
		// target. Losing the target takes that journal with it, so the
		// controller keeps a copy of what it has already read: the
		// conversation stays readable for reference even though it can no
		// longer be continued.
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE second_opinion_reviews
                 ADD COLUMN reviewer_transcript TEXT NOT NULL DEFAULT '[]';
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (18, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 18;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 19 {
		// Turn review is per workspace (is it on, and at which tier) and per
		// session (what has already been reviewed). Neither belongs on
		// `SessionRecord`, which is a compatibility surface with nine
		// construction sites; `second_opinion_reviews` above is the
		// precedent for keeping review state in its own table.
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             CREATE TABLE turn_review_settings (
                 workspace_id TEXT PRIMARY KEY
                     CHECK(length(trim(workspace_id)) > 0),
                 auto_review INTEGER NOT NULL CHECK(auto_review IN (0, 1)),
                 tier TEXT NOT NULL CHECK(tier IN ('quick', 'extended'))
             ) STRICT;
             CREATE TABLE turn_review_state (
                 session_id TEXT PRIMARY KEY
                     REFERENCES sessions(session_id) ON DELETE CASCADE,
                 baselines TEXT NOT NULL,
                 reviewed_through_ordinal INTEGER NOT NULL
                     CHECK(reviewed_through_ordinal >= 0),
                 prior_review TEXT,
                 active TEXT
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (19, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 19;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 20 {
		var targetTableExists bool
		if err := connection.QueryRow(`SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'session_targets'
             )`).Scan(&targetTableExists); err != nil {
			return err
		}
		rebuild, copyRows := "", ""
		if targetTableExists {
			rebuild, copyRows = "ALTER TABLE session_targets RENAME TO session_targets_v19;", `INSERT INTO session_targets SELECT * FROM session_targets_v19;
             DROP TABLE session_targets_v19;`
		}
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             `+rebuild+`
             CREATE TABLE session_targets (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','local-docker','apple-container','aws-ec2','ssh-bare','ssh-podman')),
                 host TEXT,
                 resource_id TEXT,
                 address TEXT,
                 workspace BLOB,
                 worker_id TEXT,
                 CHECK(
                     (kind = 'local-bare' AND workspace IS NOT NULL
                      AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
                  OR (kind IN ('local-podman','local-docker','apple-container') AND resource_id IS NOT NULL
                      AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
                      AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
                      AND resource_id IS NULL AND address IS NULL)
                  OR (kind = 'ssh-podman' AND host IS NOT NULL AND resource_id IS NOT NULL
                      AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                 )
             ) STRICT;
             `+copyRows+`
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (20, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 20;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 21 {
		// Arming review moved into `[review]` in config.toml, which is where
		// the rest of Mjolnir's durable global configuration lives and the
		// only place a phone-only user could ever have set it. No data is
		// migrated: a workspace-to-global mapping has no defensible merge
		// rule, and the release note says to re-arm it in the config file.
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             DROP TABLE IF EXISTS turn_review_settings;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (21, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 21;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 22 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE session_targets ADD COLUMN workspace_storage TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (22, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 22;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 23 {
		present, err := tableHasColumn(connection, "turn_review_state", "pending_forward")
		if err != nil {
			return err
		}
		addPendingForward := "ALTER TABLE turn_review_state ADD COLUMN pending_forward TEXT;"
		if present {
			addPendingForward = ""
		}
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             `+addPendingForward+`
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (23, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 23;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 24 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE session_targets RENAME TO session_targets_v23;
             CREATE TABLE session_targets (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','local-docker','apple-container','aws-ec2','ssh-bare','ssh-podman','ssh-docker')),
                 host TEXT,
                 resource_id TEXT,
                 address TEXT,
                 workspace BLOB,
                 worker_id TEXT,
                 workspace_storage TEXT,
                 CHECK(
                     (kind = 'local-bare' AND workspace IS NOT NULL
                      AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
                  OR (kind IN ('local-podman','local-docker','apple-container') AND resource_id IS NOT NULL
                      AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
                      AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
                      AND resource_id IS NULL AND address IS NULL)
                  OR (kind IN ('ssh-podman','ssh-docker') AND host IS NOT NULL AND resource_id IS NOT NULL
                      AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                 )
             ) STRICT;
             INSERT INTO session_targets SELECT * FROM session_targets_v23;
             DROP TABLE session_targets_v23;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (24, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 24;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 25 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             CREATE TABLE workspace_pane_sizes (
                 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
                 sessions TEXT NOT NULL CHECK(sessions IN ('minimized', 'standard', 'maximized')),
                 targets TEXT NOT NULL CHECK(targets IN ('minimized', 'standard', 'maximized')),
                 quota TEXT NOT NULL CHECK(quota IN ('minimized', 'standard', 'maximized')),
                 CHECK((sessions = 'maximized') + (targets = 'maximized') + (quota = 'maximized') <= 1)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (25, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 25;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 26 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             CREATE TABLE session_moves (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 operation_id TEXT NOT NULL UNIQUE,
                 operation_json TEXT NOT NULL CHECK(json_valid(operation_json))
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (26, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 26;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 27 {
		if err := migrateMuseHarnessKind(connection); err != nil {
			return err
		}
	}
	// The projection gained per-turn identity and outcome so a caller
	// driving a session through the HTTP API can wait for a specific prompt
	// and read how it ended. `api_idempotency` makes session creation
	// retry-safe.
	if version < 28 {
		if err := migrateTurnOutcomeColumns(connection); err != nil {
			return err
		}
	}
	if version < 29 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN create_managed_worktree INTEGER
                 CHECK(create_managed_worktree IN (0, 1));
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (29, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 29;
             COMMIT;`); err != nil {
			return err
		}
	}
	if version < 30 {
		if err := migrateCompatibilityMetadata(connection); err != nil {
			return err
		}
	}
	if version < 31 {
		if err := migrateSubagentSessions(connection); err != nil {
			return err
		}
	}
	if version < 32 {
		if err := migrateZcodeHarnessKind(connection); err != nil {
			return err
		}
	}
	// Compatible: adds one nullable column. Older readers ignore it, and the
	// older writer's session upsert lists columns explicitly, so it
	// preserves the value. An older executable launching such a session
	// falls back to the global `[subagents] enabled` setting, which is a
	// behaviour difference, not data loss. The compatibility floor stays
	// where it is.
	if version < 33 {
		if err := legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN mjolnir_subagents INTEGER
                 CHECK(mjolnir_subagents IN (0, 1));
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (33, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 33;
             COMMIT;`); err != nil {
			return err
		}
	}
	var recorded *int64
	if err := connection.QueryRow("SELECT max(version) FROM schema_migrations").Scan(&recorded); err != nil {
		return err
	}
	if recorded == nil || *recorded != 33 {
		ledger := "None"
		if recorded != nil {
			ledger = fmt.Sprintf("Some(%d)", *recorded)
		}
		return fmt.Errorf("Mjolnir database migration ledger %s does not match schema %d", ledger, 33)
	}
	// A database that already applied migration 14 with a build older than
	// the one that added `client_session_state` never got the table, since
	// that migration only ran `ensureWorkspaceSchema` once, at version 14.
	// Create it unconditionally (IF NOT EXISTS) on every writer open so an
	// already-migrated database converges too.
	if err := ensureClientSessionStateSchema(connection); err != nil {
		return err
	}
	return ensureAPIEventsSchema(connection)
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
	transaction, err := connection.Begin()
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	if _, err := transaction.Exec(`CREATE TABLE IF NOT EXISTS subagent_sessions (
             child_session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
             parent_session_id TEXT NOT NULL REFERENCES sessions(session_id),
             request_key TEXT NOT NULL,
             record_json TEXT NOT NULL CHECK(json_valid(record_json)),
             CHECK(child_session_id <> parent_session_id),
             UNIQUE(parent_session_id, request_key)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS subagent_sessions_parent
             ON subagent_sessions(parent_session_id, child_session_id);
         UPDATE schema_compatibility SET minimum_compatible_version = 31
             WHERE singleton = 1;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (31, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 31;`); err != nil {
		return err
	}
	return transaction.Commit()
}

// legacyWidenHarnessConstraint rebuilds the sessions and
// hidden_native_sessions tables with previous replaced by widened in their
// harness_kind CHECK constraint, keeping their indexes and triggers, inside
// transaction. A table that already admits the widened list is left alone.
func legacyWidenHarnessConstraint(transaction *Transaction, previous, widened string, rebuild func(table, definition string) string) error {
	for _, table := range []string{"sessions", "hidden_native_sessions"} {
		var schema string
		if err := transaction.QueryRow("SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1", table).Scan(&schema); err != nil {
			return err
		}
		_, definition, ok := strings.Cut(schema, "(")
		if !ok {
			return errors.New("missing harness table definition")
		}
		if strings.Contains(definition, widened) {
			continue
		}
		if !strings.Contains(definition, previous) {
			return fmt.Errorf("unexpected %s harness constraint", table)
		}
		definition = strings.ReplaceAll(definition, previous, widened)
		objects, err := committedStrings(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name=?1
                     AND type IN ('index','trigger') AND sql IS NOT NULL`, table)
		if err != nil {
			return err
		}
		if _, err := transaction.Exec(rebuild(table, definition)); err != nil {
			return err
		}
		for _, object := range objects {
			if _, err := transaction.Exec(object); err != nil {
				return err
			}
		}
	}
	return nil
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
	if err := legacyExec(connection, "PRAGMA foreign_keys = OFF;"); err != nil {
		return err
	}
	migration := func() error {
		transaction, err := connection.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		if err := legacyWidenHarnessConstraint(transaction, "'muse')", "'muse','zcode')", func(table, definition string) string {
			return "CREATE TABLE " + table + "_zcode_v32 (" + definition + `;
                 INSERT INTO ` + table + "_zcode_v32 SELECT * FROM " + table + `;
                 DROP TABLE ` + table + `;
                 ALTER TABLE ` + table + "_zcode_v32 RENAME TO " + table + ";"
		}); err != nil {
			return err
		}
		violated, err := legacyHasRows(transaction, "PRAGMA foreign_key_check")
		if err != nil {
			return err
		}
		if violated {
			return errors.New("foreign key violation in ZCode migration")
		}
		if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 32
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (32, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 32;`); err != nil {
			return err
		}
		return transaction.Commit()
	}()
	restored := legacyExec(connection, "PRAGMA foreign_keys = ON;")
	if migration != nil {
		return fmt.Errorf("%s: %w", "migrate ZCode harness constraints", migration)
	}
	if restored != nil {
		return fmt.Errorf("%s: %w", "restore foreign key enforcement after ZCode migration", restored)
	}
	return nil
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
	if err := legacyExec(connection, `CREATE TABLE IF NOT EXISTS workspaces (
             workspace_id TEXT PRIMARY KEY CHECK(length(trim(workspace_id)) > 0),
             name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 64),
             name_key TEXT NOT NULL UNIQUE CHECK(length(trim(name_key)) BETWEEN 1 AND 64),
             created_at TEXT NOT NULL,
             last_opened_at TEXT NOT NULL
         ) STRICT;
         INSERT OR IGNORE INTO workspaces(
             workspace_id, name, name_key, created_at, last_opened_at
         ) VALUES (
             'default', 'default', 'default',
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         );`); err != nil {
		return err
	}
	present, err := tableHasColumn(connection, "session_contexts", "workspace_id")
	if err != nil {
		return err
	}
	if !present {
		if err := legacyExec(connection, `ALTER TABLE session_contexts
                 ADD COLUMN workspace_id TEXT NOT NULL DEFAULT 'default';`); err != nil {
			return err
		}
	}
	if err := legacyExec(connection, `CREATE INDEX IF NOT EXISTS session_contexts_workspace
             ON session_contexts(workspace_id, session_id);
         CREATE TRIGGER IF NOT EXISTS session_contexts_workspace_insert
         BEFORE INSERT ON session_contexts
         WHEN NOT EXISTS(
             SELECT 1 FROM workspaces WHERE workspace_id = NEW.workspace_id
         )
         BEGIN
             SELECT RAISE(ABORT, 'unknown workspace');
         END;
         CREATE TRIGGER IF NOT EXISTS session_contexts_workspace_update
         BEFORE UPDATE OF workspace_id ON session_contexts
         WHEN NOT EXISTS(
             SELECT 1 FROM workspaces WHERE workspace_id = NEW.workspace_id
         )
         BEGIN
             SELECT RAISE(ABORT, 'unknown workspace');
         END;
         CREATE TABLE IF NOT EXISTS client_read_frontiers (
             client_id TEXT NOT NULL CHECK(length(trim(client_id)) > 0),
             workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
             session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
             through_event_ordinal INTEGER NOT NULL DEFAULT 0
                 CHECK(through_event_ordinal >= 0),
             updated_at TEXT NOT NULL,
             PRIMARY KEY(client_id, workspace_id, session_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS detached_drafts (
             draft_id TEXT PRIMARY KEY CHECK(length(trim(draft_id)) > 0),
             workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
             session_id TEXT REFERENCES session_contexts(session_id),
             source TEXT NOT NULL CHECK(length(trim(source)) > 0),
             owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0),
             saved_at TEXT NOT NULL,
             text TEXT NOT NULL CHECK(length(text) > 0),
             recovered_at TEXT
         ) STRICT;
         CREATE INDEX IF NOT EXISTS detached_drafts_workspace_recent
             ON detached_drafts(workspace_id, saved_at DESC);`); err != nil {
		return err
	}
	return ensureClientSessionStateSchema(connection)
}

// ensureClientSessionStateSchema keeps per-client unsent drafts.
//
// This is additive and separate from client_read_frontiers on purpose. A
// frontier is a cursor every client has; a draft is text one viewer typed and
// did not send, and it expires. Keeping them apart means the phone's
// retention policy cannot reach a terminal client's cursor.
func ensureClientSessionStateSchema(connection *Connection) error {
	return legacyExec(connection, `CREATE TABLE IF NOT EXISTS client_session_state (
             client_id TEXT NOT NULL CHECK(length(trim(client_id)) > 0),
             workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
             session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
             draft TEXT NOT NULL DEFAULT '',
             updated_at TEXT NOT NULL,
             PRIMARY KEY(client_id, workspace_id, session_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS client_session_state_age
             ON client_session_state(updated_at);`)
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
			if err := legacyExec(connection, `BEGIN IMMEDIATE;
                 ALTER TABLE sessions ADD COLUMN `+column+` TEXT;
                 COMMIT;`); err != nil {
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
	var statements strings.Builder
	statements.WriteString(`BEGIN IMMEDIATE;
`)
	for _, addition := range []struct{ table, column, statement string }{
		{"materialized_sessions", "active_turn_json", `ALTER TABLE materialized_sessions ADD COLUMN active_turn_json TEXT
                 CHECK(active_turn_json IS NULL OR json_valid(active_turn_json));
`},
		{"materialized_sessions", "last_turn_outcome_json", `ALTER TABLE materialized_sessions ADD COLUMN last_turn_outcome_json TEXT
                 CHECK(last_turn_outcome_json IS NULL OR json_valid(last_turn_outcome_json));
`},
		{"materialized_queued_prompts", "accepted_ordinal", `ALTER TABLE materialized_queued_prompts ADD COLUMN accepted_ordinal INTEGER
                 CHECK(accepted_ordinal IS NULL OR accepted_ordinal > 0);
`},
	} {
		present, err := tableHasColumn(connection, addition.table, addition.column)
		if err != nil {
			return err
		}
		if !present {
			statements.WriteString(addition.statement)
		}
	}
	statements.WriteString(`CREATE TABLE IF NOT EXISTS api_idempotency (
             key TEXT PRIMARY KEY CHECK(length(trim(key)) BETWEEN 1 AND 128),
             session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
             created_at_ms INTEGER NOT NULL
         ) STRICT;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (28, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 28;
         COMMIT;`)
	return legacyExec(connection, statements.String())
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
	return legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE session_mounts ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
             COMMIT;`)
}

func ensureMaterializedElicitationColumn(connection *Connection) error {
	present, err := tableHasColumn(connection, "materialized_sessions", "pending_elicitations_json")
	if err != nil || present {
		return err
	}
	return legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE materialized_sessions
                 ADD COLUMN pending_elicitations_json TEXT NOT NULL DEFAULT '[]';
             COMMIT;`)
}

func ensureManagedWorktreeColumn(connection *Connection) error {
	present, err := tableHasColumn(connection, "sessions", "managed_worktree")
	if err != nil || present {
		return err
	}
	return legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN managed_worktree TEXT;
             COMMIT;`)
}

// ensureRelayProjectionSchema completes the relay half of the colliding v6
// migration for databases first opened by master, whose v6 contained only
// managed_worktree.
func ensureRelayProjectionSchema(connection *Connection) error {
	present, err := tableHasColumn(connection, "sessions", "detached_after_event_ordinal")
	if err != nil || present {
		return err
	}
	return legacyExec(connection, `BEGIN IMMEDIATE;
         ALTER TABLE session_checkpoints
             RENAME COLUMN event_sequence TO event_frontier;
         ALTER TABLE prompt_history
             RENAME COLUMN event_sequence TO event_ordinal;
         ALTER TABLE sessions ADD COLUMN detached_after_event_ordinal INTEGER NOT NULL
             DEFAULT 0 CHECK(detached_after_event_ordinal >= 0);
         CREATE TABLE materialized_sessions (
             session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
             applied_event_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(applied_event_ordinal >= 0),
             applied_event_digest TEXT NOT NULL
                 DEFAULT '`+core.RelayEventGenesisDigest+`'
                 CHECK(length(applied_event_digest) = 64
                       AND applied_event_digest NOT GLOB '*[^0-9a-f]*'),
             last_activity_at_ms INTEGER,
             execution_state TEXT NOT NULL DEFAULT 'idle'
                 CHECK(execution_state IN ('idle','running','closing','closed')),
             running_started_at_ms INTEGER,
             session_title TEXT CHECK(session_title IS NULL OR length(trim(session_title)) > 0),
             configuration_json TEXT NOT NULL DEFAULT '{}',
             CHECK(
                 (execution_state = 'running' AND running_started_at_ms IS NOT NULL)
                 OR (execution_state != 'running' AND running_started_at_ms IS NULL)
             )
         ) STRICT;
         CREATE TABLE materialized_transcript_items (
             session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
             stable_id TEXT NOT NULL CHECK(length(trim(stable_id)) > 0),
             position INTEGER NOT NULL CHECK(position > 0),
             latest_content_event_ordinal INTEGER
                 CHECK(latest_content_event_ordinal IS NULL
                       OR latest_content_event_ordinal >= position),
             created_at_ms INTEGER NOT NULL,
             last_changed_at_ms INTEGER NOT NULL CHECK(last_changed_at_ms >= created_at_ms),
             body_json TEXT NOT NULL,
             PRIMARY KEY(session_id, stable_id)
         ) STRICT;
         CREATE INDEX materialized_transcript_position
             ON materialized_transcript_items(session_id, position, stable_id);
         CREATE TABLE materialized_queued_prompts (
             session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
             command_id TEXT NOT NULL CHECK(length(trim(command_id)) > 0),
             content_json TEXT NOT NULL,
             queued_at_ms INTEGER NOT NULL,
             PRIMARY KEY(session_id, ordinal),
             UNIQUE(session_id, command_id)
         ) STRICT;
         INSERT INTO materialized_sessions(session_id)
             SELECT session_id FROM sessions;
         COMMIT;`)
}

// legacyRebuildSessions runs one standard sessions-table rebuild batch with
// foreign keys disabled around it. SQLite cannot widen or narrow a CHECK
// constraint in place; every child continues to reference the replacement
// table by the same name.
func legacyRebuildSessions(connection *Connection, batch, rollbackWarning, migrationContext, violation string) error {
	if err := legacyExec(connection, "PRAGMA foreign_keys = OFF;"); err != nil {
		return err
	}
	migration := legacyExec(connection, batch)
	if migration != nil {
		if err := legacyExec(connection, "ROLLBACK;"); err != nil {
			slog.Warn(rollbackWarning, "error", err)
		}
	}
	foreignKeys := legacyExec(connection, "PRAGMA foreign_keys = ON;")
	if migration != nil {
		return fmt.Errorf("%s: %w", migrationContext, migration)
	}
	if foreignKeys != nil {
		return fmt.Errorf("restore foreign key enforcement after schema migration: %w", foreignKeys)
	}
	violated, err := legacyHasRows(connection, "PRAGMA foreign_key_check")
	if err != nil {
		return err
	}
	if violated {
		return errors.New(violation)
	}
	return nil
}

func migrateDestroyingSessionState(connection *Connection) error {
	// SQLite cannot widen a CHECK constraint in place. Foreign keys are
	// disabled only around the standard table-rebuild transaction; every
	// child continues to reference the replacement table by the same name.
	return legacyRebuildSessions(connection, `BEGIN IMMEDIATE;
         CREATE TABLE sessions_v7 (
             session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
             title TEXT NOT NULL CHECK(length(trim(title)) > 0),
             harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi')),
             last_profile TEXT NOT NULL,
             target_template_id TEXT NOT NULL,
             state TEXT NOT NULL CHECK(state IN (
                 'provisioning','running','disconnected','checkpointing','closing','destroying',
                 'archived','lost','error','destroyed-with-data-loss'
             )),
             native_session_id TEXT,
             acp_session_title TEXT CHECK(acp_session_title IS NULL OR length(trim(acp_session_title)) > 0),
             session_title_override TEXT CHECK(session_title_override IS NULL OR length(trim(session_title_override)) > 0),
             updated_at TEXT NOT NULL,
             detached_after_event_ordinal INTEGER NOT NULL DEFAULT 0
                 CHECK(detached_after_event_ordinal >= 0),
             last_error TEXT,
             resource_allocation TEXT,
             last_checkpoint_error TEXT,
             project_directory BLOB,
             managed_worktree TEXT
         ) STRICT;
         INSERT INTO sessions_v7(
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             detached_after_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree
         )
         SELECT
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             detached_after_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree
         FROM sessions;
         DROP TABLE sessions;
         ALTER TABLE sessions_v7 RENAME TO sessions;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (7, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 7;
         COMMIT;`, "could not roll back durable-destroying-session migration", "migrate durable destroying session state", "foreign key violation after migrating durable destroying session state")
}

// migrateGrokHarnessKind admits the Grok Build harness. SQLite cannot widen a
// CHECK constraint in place, so this repeats the v7 table rebuild with the
// wider harness list. Foreign keys are disabled only around the rebuild
// transaction; every child continues to reference the replacement table by
// the same name.
func migrateGrokHarnessKind(connection *Connection) error {
	return legacyRebuildSessions(connection, `BEGIN IMMEDIATE;
         CREATE TABLE sessions_v9 (
             session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
             title TEXT NOT NULL CHECK(length(trim(title)) > 0),
             harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok')),
             last_profile TEXT NOT NULL,
             target_template_id TEXT NOT NULL,
             state TEXT NOT NULL CHECK(state IN (
                 'provisioning','running','disconnected','checkpointing','closing','destroying',
                 'archived','lost','error','destroyed-with-data-loss'
             )),
             native_session_id TEXT,
             acp_session_title TEXT CHECK(acp_session_title IS NULL OR length(trim(acp_session_title)) > 0),
             session_title_override TEXT CHECK(session_title_override IS NULL OR length(trim(session_title_override)) > 0),
             updated_at TEXT NOT NULL,
             detached_after_event_ordinal INTEGER NOT NULL DEFAULT 0
                 CHECK(detached_after_event_ordinal >= 0),
             last_error TEXT,
             resource_allocation TEXT,
             last_checkpoint_error TEXT,
             project_directory BLOB,
             managed_worktree TEXT,
             draft_input TEXT NOT NULL DEFAULT ''
         ) STRICT;
         INSERT INTO sessions_v9(
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             detached_after_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree, draft_input
         )
         SELECT
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             detached_after_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree, draft_input
         FROM sessions;
         DROP TABLE sessions;
         ALTER TABLE sessions_v9 RENAME TO sessions;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (9, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 9;
         COMMIT;`, "could not roll back Grok harness migration", "migrate sessions table for the Grok Build harness", "foreign key violation after migrating the sessions harness list")
}

// migrateStoppedSessionState renames the `archived` lifecycle state to
// `stopped` and gives sessions their own display-only `archived` flag, which
// now means "hidden from the resume dialog". SQLite cannot narrow or widen a
// CHECK constraint in place, so this repeats the v9 table rebuild with the
// new state list and the new column. It also adds the hidden set for native
// sessions Mjolnir only reads.
func migrateStoppedSessionState(connection *Connection) error {
	return legacyRebuildSessions(connection, `BEGIN IMMEDIATE;
         CREATE TABLE sessions_v10 (
             session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
             title TEXT NOT NULL CHECK(length(trim(title)) > 0),
             harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok')),
             last_profile TEXT NOT NULL,
             target_template_id TEXT NOT NULL,
             state TEXT NOT NULL CHECK(state IN (
                 'provisioning','running','disconnected','checkpointing','closing','destroying',
                 'stopped','lost','error','destroyed-with-data-loss'
             )),
             native_session_id TEXT,
             acp_session_title TEXT CHECK(acp_session_title IS NULL OR length(trim(acp_session_title)) > 0),
             session_title_override TEXT CHECK(session_title_override IS NULL OR length(trim(session_title_override)) > 0),
             updated_at TEXT NOT NULL,
             detached_after_event_ordinal INTEGER NOT NULL DEFAULT 0
                 CHECK(detached_after_event_ordinal >= 0),
             last_error TEXT,
             resource_allocation TEXT,
             last_checkpoint_error TEXT,
             project_directory BLOB,
             managed_worktree TEXT,
             draft_input TEXT NOT NULL DEFAULT '',
             container_cpus TEXT,
             container_memory TEXT,
             archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1))
         ) STRICT;
         INSERT INTO sessions_v10(
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             detached_after_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree, draft_input,
             container_cpus, container_memory
         )
         SELECT
             session_id, title, harness_kind, last_profile, target_template_id,
             CASE state WHEN 'archived' THEN 'stopped' ELSE state END,
             native_session_id, acp_session_title, session_title_override, updated_at,
             detached_after_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree, draft_input,
             container_cpus, container_memory
         FROM sessions;
         DROP TABLE sessions;
         ALTER TABLE sessions_v10 RENAME TO sessions;
         CREATE TABLE hidden_native_sessions (
             harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok')),
             native_session_id TEXT NOT NULL CHECK(length(trim(native_session_id)) > 0),
             hidden_at TEXT NOT NULL,
             PRIMARY KEY(harness_kind, native_session_id)
         ) STRICT;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (10, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 10;
         COMMIT;`, "could not roll back stopped-session migration", "migrate sessions table for the stopped session state", "foreign key violation after migrating the stopped session state")
}

// migrateMuseHarnessKind preserves existing data and dependent indexes while
// admitting Muse sessions.
//
// The widened constraint this builds still lists 'deepseek', carried over
// from migration 11. That harness, DeepSeek's own DSH, has since been
// removed; the value is retained only so session rows written by earlier
// releases stay readable. The later dsh-acp harness is 'dsh', so such a row
// is never read as one of its sessions. No code accepts it, HarnessKind
// parsing rejects it, and loading state skips such a row with a warning.
// Dropping the value would need another breaking migration that rewrote or
// deleted those rows.
func migrateMuseHarnessKind(connection *Connection) error {
	if err := legacyExec(connection, "PRAGMA foreign_keys = OFF;"); err != nil {
		return err
	}
	migration := func() error {
		transaction, err := connection.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		if err := legacyWidenHarnessConstraint(transaction, "'deepseek')", "'deepseek','muse')", func(table, definition string) string {
			return "CREATE TABLE " + table + "_muse_v27 (" + definition + "; INSERT INTO " + table + "_muse_v27 SELECT * FROM " + table + "; DROP TABLE " + table + "; ALTER TABLE " + table + "_muse_v27 RENAME TO " + table + ";"
		}); err != nil {
			return err
		}
		violated, err := legacyHasRows(transaction, "PRAGMA foreign_key_check")
		if err != nil {
			return err
		}
		if violated {
			return errors.New("foreign key violation in Muse migration")
		}
		if _, err := transaction.Exec("INSERT INTO schema_migrations(version, applied_at) VALUES (27, strftime('%Y-%m-%dT%H:%M:%fZ','now')); PRAGMA user_version = 27;"); err != nil {
			return err
		}
		return transaction.Commit()
	}()
	restored := legacyExec(connection, "PRAGMA foreign_keys = ON;")
	if migration != nil {
		return fmt.Errorf("%s: %w", "migrate Muse harness constraints", migration)
	}
	if restored != nil {
		return fmt.Errorf("%s: %w", "restore foreign key enforcement after Muse migration", restored)
	}
	return nil
}

// migrateDeepseekHarnessKind admits DeepSeek Harness in both stored sessions
// and Mjolnir's native-session hidden set. SQLite requires rebuilding tables
// to widen CHECK constraints.
func migrateDeepseekHarnessKind(connection *Connection) error {
	return legacyRebuildSessions(connection, `BEGIN IMMEDIATE;
         CREATE TABLE sessions_v11 (
             session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
             title TEXT NOT NULL CHECK(length(trim(title)) > 0),
             harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok','deepseek')),
             last_profile TEXT NOT NULL,
             target_template_id TEXT NOT NULL,
             state TEXT NOT NULL CHECK(state IN (
                 'provisioning','running','disconnected','checkpointing','closing','destroying',
                 'stopped','lost','error','destroyed-with-data-loss'
             )),
             native_session_id TEXT,
             acp_session_title TEXT CHECK(acp_session_title IS NULL OR length(trim(acp_session_title)) > 0),
             session_title_override TEXT CHECK(session_title_override IS NULL OR length(trim(session_title_override)) > 0),
             updated_at TEXT NOT NULL,
             detached_after_event_ordinal INTEGER NOT NULL DEFAULT 0
                 CHECK(detached_after_event_ordinal >= 0),
             last_error TEXT,
             resource_allocation TEXT,
             last_checkpoint_error TEXT,
             project_directory BLOB,
             managed_worktree TEXT,
             draft_input TEXT NOT NULL DEFAULT '',
             container_cpus TEXT,
             container_memory TEXT,
             archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1))
         ) STRICT;
         INSERT INTO sessions_v11 SELECT * FROM sessions;
         DROP TABLE sessions;
         ALTER TABLE sessions_v11 RENAME TO sessions;
         ALTER TABLE hidden_native_sessions RENAME TO hidden_native_sessions_v10;
         CREATE TABLE hidden_native_sessions (
             harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok','deepseek')),
             native_session_id TEXT NOT NULL CHECK(length(trim(native_session_id)) > 0),
             hidden_at TEXT NOT NULL,
             PRIMARY KEY(harness_kind, native_session_id)
         ) STRICT;
         INSERT INTO hidden_native_sessions SELECT * FROM hidden_native_sessions_v10;
         DROP TABLE hidden_native_sessions_v10;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (11, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 11;
         COMMIT;`, "could not roll back DeepSeek harness migration", "migrate sessions table for DeepSeek Harness", "foreign key violation after migrating the DeepSeek Harness list")
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
	return legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN draft_input TEXT NOT NULL DEFAULT '';
             COMMIT;`)
}

func ensureProjectionDigestColumn(connection *Connection) error {
	var present bool
	if err := connection.QueryRow(`SELECT EXISTS(
             SELECT 1 FROM pragma_table_info('materialized_sessions')
             WHERE name = 'applied_event_digest'
         )`).Scan(&present); err != nil {
		return err
	}
	if present {
		return nil
	}
	return legacyExec(connection, `BEGIN IMMEDIATE;
             ALTER TABLE materialized_sessions ADD COLUMN applied_event_digest TEXT NOT NULL
                 DEFAULT '`+core.RelayEventGenesisDigest+`'
                 CHECK(length(applied_event_digest) = 64
                       AND applied_event_digest NOT GLOB '*[^0-9a-f]*');
             COMMIT;`)
}

func ensureAPIEventsSchema(connection *Connection) error {
	return legacyExec(connection, `CREATE TABLE IF NOT EXISTS api_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
            recorded_at_ms INTEGER NOT NULL,
            body TEXT NOT NULL CHECK(json_valid(body))
        ) STRICT;
        CREATE INDEX IF NOT EXISTS api_events_session ON api_events(session_id, seq);
        CREATE TABLE IF NOT EXISTS api_session_activity (
            session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
            body TEXT NOT NULL CHECK(json_valid(body))
        ) STRICT;
        CREATE TRIGGER IF NOT EXISTS api_session_error_updated
        AFTER UPDATE OF last_error ON sessions
        WHEN NEW.last_error IS NOT NULL AND NEW.last_error IS NOT OLD.last_error
        BEGIN
            INSERT INTO api_events(session_id, recorded_at_ms, body)
            VALUES (NEW.session_id, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
                json_object('type', 'error', 'data', json_object('message', NEW.last_error, 'command_id', NULL)));
        END;
        CREATE TRIGGER IF NOT EXISTS api_session_error_inserted
        AFTER INSERT ON sessions WHEN NEW.last_error IS NOT NULL
        BEGIN
            INSERT INTO api_events(session_id, recorded_at_ms, body)
            VALUES (NEW.session_id, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
                json_object('type', 'error', 'data', json_object('message', NEW.last_error, 'command_id', NULL)));
        END;`)
}
