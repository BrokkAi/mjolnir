use super::*;
use rusqlite::OpenFlags;

pub fn database_path() -> PathBuf {
    data_dir().join("mj.sqlite3")
}

pub(super) fn open_writer(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create Mjolnir data directory {}", parent.display()))?;
    }
    let connection = Connection::open(path)
        .with_context(|| format!("open Mjolnir database {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;",
    )?;
    verify_schema_once(path, &connection)?;
    Ok(connection)
}

pub(super) fn open(path: &Path) -> Result<Connection> {
    open_writer(path)
}

/// Open an existing database without permitting schema or data mutation.
/// Client processes use this path so an accidental write fails locally
/// instead of competing with the daemon's writer.
#[cfg(not(test))]
pub(super) fn open_reader(path: &Path) -> Result<Connection> {
    open_reader_strict(path)
}

#[cfg(test)]
pub(super) fn open_reader(path: &Path) -> Result<Connection> {
    // Path-taking database helpers are migration fixtures in unit tests: they
    // intentionally open old or not-yet-created schemas. Production query
    // entry points compile against the strict reader above.
    open_writer(path)
}

#[cfg_attr(test, allow(dead_code))]
fn open_reader_strict(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open Mjolnir database read-only {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA query_only = ON;",
    )?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreSchemaMismatch {
            found: version,
            supported: SCHEMA_VERSION,
        }
        .into());
    }
    Ok(connection)
}

/// Databases this process has already migrated. A controller owns its store
/// exclusively (`ControllerStoreGuard`), so a schema verified once stays
/// verified and later connections skip the migration probes entirely.
fn verified_schemas() -> &'static Mutex<HashSet<PathBuf>> {
    static VERIFIED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    VERIFIED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Stable cache identity for a database. The file itself may not exist yet, so
/// the canonicalized parent directory carries the identity.
fn schema_cache_key(path: &Path) -> PathBuf {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return path.to_owned();
    };
    match (fs::canonicalize(parent), path.file_name()) {
        (Ok(canonical), Some(name)) => canonical.join(name),
        _ => path.to_owned(),
    }
}

/// Run the migration ladder the first time this process opens a database.
/// Later opens confirm only the recorded schema version, which keeps relay
/// catch-up from paying for the full probe sequence on every connection. A
/// database whose version no longer matches is migrated again, so a recreated
/// file under a reused path still converges.
fn verify_schema_once(path: &Path, connection: &Connection) -> Result<()> {
    let key = schema_cache_key(path);
    let mut verified = verified_schemas()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if verified.contains(&key) {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }
    }
    // Holding the lock across the ladder keeps two first opens of the same
    // database from running the additive migration steps against each other.
    migrate_schema(connection)?;
    verified.insert(key);
    Ok(())
}

/// Forget that this process verified a database's schema. Only tests need it:
/// they simulate a store written by an older build by editing the schema of a
/// database this process has already opened, which no controller can do.
#[cfg(test)]
pub(super) fn forget_verified_schema(path: &Path) {
    verified_schemas()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&schema_cache_key(path));
}

fn migrate_schema(connection: &Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreSchemaMismatch {
            found: version,
            supported: SCHEMA_VERSION,
        }
        .into());
    }
    if version == 0 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE schema_migrations (
                 version INTEGER PRIMARY KEY CHECK(version > 0),
                 applied_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE session_contexts (
                 session_id TEXT PRIMARY KEY,
                 bundle_id TEXT NOT NULL,
                 created_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE sessions (
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
                 last_viewed_event_sequence INTEGER NOT NULL DEFAULT 0 CHECK(last_viewed_event_sequence >= 0),
                 last_error TEXT
             ) STRICT;
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
             CREATE TABLE session_mounts (
                 session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                 source BLOB NOT NULL,
                 destination BLOB NOT NULL,
                 PRIMARY KEY(session_id, ordinal),
                 UNIQUE(session_id, destination)
             ) STRICT;
             CREATE TABLE session_checkpoints (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 archive_path BLOB NOT NULL,
                 sha256 TEXT NOT NULL CHECK(length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
                 created_at TEXT NOT NULL,
                 event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0)
             ) STRICT;
             CREATE TABLE mount_history (
                 host TEXT NOT NULL CHECK(length(trim(host)) > 0),
                 source BLOB NOT NULL,
                 ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                 PRIMARY KEY(host, ordinal),
                 UNIQUE(host, source)
             ) STRICT;
             CREATE TABLE prompt_history (
                 history_id INTEGER PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                 event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0),
                 submitted_at TEXT NOT NULL,
                 text TEXT NOT NULL CHECK(length(trim(text)) > 0),
                 UNIQUE(session_id, event_sequence)
             ) STRICT;
             CREATE INDEX prompt_history_session_recent
                 ON prompt_history(session_id, history_id DESC);
             CREATE INDEX session_contexts_bundle
                 ON session_contexts(bundle_id, session_id);
             CREATE INDEX prompt_history_recent
                 ON prompt_history(history_id DESC);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    if version < 2 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN resource_allocation TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    if version < 3 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN last_checkpoint_error TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }
    if version < 4 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN project_directory BLOB;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
    }
    if version < 5 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 6 {
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
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
                     DEFAULT '{RELAY_EVENT_GENESIS_DIGEST}'
                     CHECK(length(applied_event_digest) = 64
                           AND applied_event_digest NOT GLOB '*[^0-9a-f]*'),
                 last_activity_at_ms INTEGER,
                 execution_state TEXT NOT NULL DEFAULT 'idle'
                     CHECK(execution_state IN ('idle','running','closing','closed')),
                 running_started_at_ms INTEGER,
                 session_title TEXT CHECK(session_title IS NULL OR length(trim(session_title)) > 0),
                 configuration_json TEXT NOT NULL DEFAULT '{{}}',
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
             COMMIT;",
        ))?;
    }
    // Both development lines used schema version 6: durable relay projection
    // on this branch and managed raw-session worktrees on master. Structural
    // guards make either already-written v6 database converge before the v7
    // sessions-table rebuild, without inventing a second version-6 ledger row.
    ensure_managed_worktree_column(connection)?;
    if version < 7 {
        ensure_relay_projection_schema(connection)?;
        migrate_destroying_session_state(connection)?;
    }
    ensure_projection_digest_column(connection)?;
    ensure_session_draft_input_column(connection)?;
    if version < 8 {
        // Queue entries gained a kind so a configuration change can wait in the
        // same queue as prompts. Rows written before that are prompts.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE materialized_queued_prompts
                 ADD COLUMN kind_json TEXT NOT NULL DEFAULT '\"prompt\"';
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (8, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 8;
             COMMIT;",
        )?;
    }
    // Runs last: it rebuilds `sessions`, so every column the steps above add
    // must already exist to be copied forward.
    if version < 9 {
        migrate_grok_harness_kind(connection)?;
    }
    // Added after the v9 rebuild so the rebuild never has to copy them.
    ensure_session_container_override_columns(connection)?;
    ensure_session_mount_read_only_column(connection)?;
    ensure_materialized_elicitation_column(connection)?;
    if version < 10 {
        migrate_stopped_session_state(connection)?;
    }
    if version < 11 {
        migrate_deepseek_harness_kind(connection)?;
    }
    if version < 12 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions
                 RENAME COLUMN detached_after_event_ordinal TO viewed_through_event_ordinal;
             UPDATE sessions
                 SET target_template_id = 'localhost'
                 WHERE target_template_id = 'raw-localhost';
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (12, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 12;
             COMMIT;",
        )?;
    }
    if version < 13 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 14 {
        ensure_workspace_schema(connection)?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             INSERT OR IGNORE INTO schema_migrations(version, applied_at)
                 VALUES (14, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 14;
             COMMIT;",
        )?;
    }
    if version < 15 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE host_container_sizes (
                 host TEXT PRIMARY KEY CHECK(length(trim(host)) > 0),
                 cpus INTEGER NOT NULL CHECK(cpus > 0),
                 memory_bytes INTEGER NOT NULL CHECK(memory_bytes > 0)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (15, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 15;
             COMMIT;",
        )?;
    }
    if version < 16 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 17 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 18 {
        // The reviewer's conversation lives in its own journal on the target.
        // Losing the target takes that journal with it, so the controller
        // keeps a copy of what it has already read: the conversation stays
        // readable for reference even though it can no longer be continued.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE second_opinion_reviews
                 ADD COLUMN reviewer_transcript TEXT NOT NULL DEFAULT '[]';
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (18, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 18;
             COMMIT;",
        )?;
    }
    if version < 19 {
        // Turn review is per workspace (is it on, and at which tier) and per
        // session (what has already been reviewed). Neither belongs on
        // `SessionRecord`, which is a compatibility surface with nine
        // construction sites; `second_opinion_reviews` above is the precedent
        // for keeping review state in its own table.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 20 {
        let target_table_exists: bool = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'session_targets'
             )",
            [],
            |row| row.get(0),
        )?;
        let rebuild = if target_table_exists {
            "ALTER TABLE session_targets RENAME TO session_targets_v19;"
        } else {
            ""
        };
        let copy = if target_table_exists {
            "INSERT INTO session_targets SELECT * FROM session_targets_v19;
             DROP TABLE session_targets_v19;"
        } else {
            ""
        };
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {rebuild}
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
             {copy}
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (20, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 20;
             COMMIT;"
        ))?;
    }
    if version < 21 {
        // Arming review moved into `[review]` in config.toml, which is where
        // the rest of Mjolnir's durable global configuration lives and the only
        // place a phone-only user could ever have set it. No data is
        // migrated: a workspace-to-global mapping has no defensible merge
        // rule, and the release note says to re-arm it in the config file.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             DROP TABLE IF EXISTS turn_review_settings;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (21, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 21;
             COMMIT;",
        )?;
    }
    if version < 22 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE session_targets ADD COLUMN workspace_storage TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (22, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 22;
             COMMIT;",
        )?;
    }
    if version < 23 {
        let add_pending_forward =
            if table_has_column(connection, "turn_review_state", "pending_forward")? {
                ""
            } else {
                "ALTER TABLE turn_review_state ADD COLUMN pending_forward TEXT;"
            };
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {add_pending_forward}
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (23, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 23;
             COMMIT;"
        ))?;
    }
    let recorded: Option<i64> =
        connection.query_row("SELECT max(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    if recorded != Some(SCHEMA_VERSION) {
        bail!(
            "Mjolnir database migration ledger {:?} does not match schema {}",
            recorded,
            SCHEMA_VERSION
        );
    }
    // A database that already applied migration 14 with a build older than the
    // one that added `client_session_state` never got the table, since that
    // migration only ran `ensure_workspace_schema` once, at version 14. Create
    // it unconditionally (IF NOT EXISTS) on every writer open so an
    // already-migrated database converges too.
    ensure_client_session_state_schema(connection)?;
    Ok(())
}

pub(super) fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info(?1)
                 WHERE name = ?2
             )",
            params![table, column],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn ensure_workspace_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspaces (
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
         );",
    )?;
    if !table_has_column(connection, "session_contexts", "workspace_id")? {
        connection.execute_batch(
            "ALTER TABLE session_contexts
                 ADD COLUMN workspace_id TEXT NOT NULL DEFAULT 'default';",
        )?;
    }
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS session_contexts_workspace
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
             ON detached_drafts(workspace_id, saved_at DESC);",
    )?;
    ensure_client_session_state_schema(connection)?;
    Ok(())
}

/// Per-viewer, per-session state a web client keeps between visits.
///
/// This is additive and separate from `client_read_frontiers` on purpose. A
/// frontier is a cursor every client has; a draft is text one viewer typed and
/// did not send, and it expires. Keeping them apart means the phone's
/// retention policy cannot reach a terminal client's cursor.
fn ensure_client_session_state_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_session_state (
             client_id TEXT NOT NULL CHECK(length(trim(client_id)) > 0),
             workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
             session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
             draft TEXT NOT NULL DEFAULT '',
             updated_at TEXT NOT NULL,
             PRIMARY KEY(client_id, workspace_id, session_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS client_session_state_age
             ON client_session_state(updated_at);",
    )?;
    Ok(())
}

/// Per-session container size overrides. They are additive columns, so
/// databases written before the dashboard could edit them open unchanged.
fn ensure_session_container_override_columns(connection: &Connection) -> Result<()> {
    for column in ["container_cpus", "container_memory"] {
        if !table_has_column(connection, "sessions", column)? {
            connection.execute_batch(&format!(
                "BEGIN IMMEDIATE;
                 ALTER TABLE sessions ADD COLUMN {column} TEXT;
                 COMMIT;"
            ))?;
        }
    }
    Ok(())
}

/// Per-mount read-only flag. It is an additive column, so a database written
/// before the mount editors offered the option opens unchanged and its mounts
/// keep the copy-on-write overlay they were provisioned with.
fn ensure_session_mount_read_only_column(connection: &Connection) -> Result<()> {
    if !table_has_column(connection, "session_mounts", "read_only")? {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE session_mounts ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
             COMMIT;",
        )?;
    }
    Ok(())
}

fn ensure_materialized_elicitation_column(connection: &Connection) -> Result<()> {
    if !table_has_column(
        connection,
        "materialized_sessions",
        "pending_elicitations_json",
    )? {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE materialized_sessions
                 ADD COLUMN pending_elicitations_json TEXT NOT NULL DEFAULT '[]';
             COMMIT;",
        )?;
    }
    Ok(())
}

fn ensure_managed_worktree_column(connection: &Connection) -> Result<()> {
    if !table_has_column(connection, "sessions", "managed_worktree")? {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN managed_worktree TEXT;
             COMMIT;",
        )?;
    }
    Ok(())
}

/// Complete the relay half of the colliding v6 migration for databases first
/// opened by master, whose v6 contained only `managed_worktree`.
fn ensure_relay_projection_schema(connection: &Connection) -> Result<()> {
    if table_has_column(connection, "sessions", "detached_after_event_ordinal")? {
        return Ok(());
    }
    connection.execute_batch(&format!(
        "BEGIN IMMEDIATE;
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
                 DEFAULT '{RELAY_EVENT_GENESIS_DIGEST}'
                 CHECK(length(applied_event_digest) = 64
                       AND applied_event_digest NOT GLOB '*[^0-9a-f]*'),
             last_activity_at_ms INTEGER,
             execution_state TEXT NOT NULL DEFAULT 'idle'
                 CHECK(execution_state IN ('idle','running','closing','closed')),
             running_started_at_ms INTEGER,
             session_title TEXT CHECK(session_title IS NULL OR length(trim(session_title)) > 0),
             configuration_json TEXT NOT NULL DEFAULT '{{}}',
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
         COMMIT;",
    ))?;
    Ok(())
}

fn migrate_destroying_session_state(connection: &Connection) -> Result<()> {
    // SQLite cannot widen a CHECK constraint in place. Foreign keys are
    // disabled only around the standard table-rebuild transaction; every
    // child continues to reference the replacement table by the same name.
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = connection.execute_batch(
        "BEGIN IMMEDIATE;
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
         COMMIT;",
    );
    if migration.is_err()
        && let Err(error) = connection.execute_batch("ROLLBACK;")
    {
        tracing::warn!(%error, "could not roll back durable-destroying-session migration");
    }
    let foreign_keys = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration.context("migrate durable destroying session state")?;
    foreign_keys.context("restore foreign key enforcement after schema migration")?;
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.exists([])? {
        bail!("foreign key violation after migrating durable destroying session state");
    }
    Ok(())
}

/// Admit the Grok Build harness. SQLite cannot widen a CHECK constraint in
/// place, so this repeats the v7 table rebuild with the wider harness list.
/// Foreign keys are disabled only around the rebuild transaction; every child
/// continues to reference the replacement table by the same name.
fn migrate_grok_harness_kind(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = connection.execute_batch(
        "BEGIN IMMEDIATE;
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
         COMMIT;",
    );
    if migration.is_err()
        && let Err(error) = connection.execute_batch("ROLLBACK;")
    {
        tracing::warn!(%error, "could not roll back Grok harness migration");
    }
    let foreign_keys = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration.context("migrate sessions table for the Grok Build harness")?;
    foreign_keys.context("restore foreign key enforcement after schema migration")?;
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.exists([])? {
        bail!("foreign key violation after migrating the sessions harness list");
    }
    Ok(())
}

/// Rename the `archived` lifecycle state to `stopped` and give sessions their
/// own display-only `archived` flag, which now means "hidden from the resume
/// dialog". SQLite cannot narrow or widen a CHECK constraint in place, so this
/// repeats the v9 table rebuild with the new state list and the new column.
/// It also adds the hidden set for native sessions Mjolnir only reads.
fn migrate_stopped_session_state(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = connection.execute_batch(
        "BEGIN IMMEDIATE;
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
         COMMIT;",
    );
    if migration.is_err()
        && let Err(error) = connection.execute_batch("ROLLBACK;")
    {
        tracing::warn!(%error, "could not roll back stopped-session migration");
    }
    let foreign_keys = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration.context("migrate sessions table for the stopped session state")?;
    foreign_keys.context("restore foreign key enforcement after schema migration")?;
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.exists([])? {
        bail!("foreign key violation after migrating the stopped session state");
    }
    Ok(())
}

/// Admit DeepSeek Harness in both stored sessions and Mjolnir's native-session
/// hidden set. SQLite requires rebuilding tables to widen CHECK constraints.
fn migrate_deepseek_harness_kind(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = connection.execute_batch(
        "BEGIN IMMEDIATE;
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
         COMMIT;",
    );
    if migration.is_err()
        && let Err(error) = connection.execute_batch("ROLLBACK;")
    {
        tracing::warn!(%error, "could not roll back DeepSeek harness migration");
    }
    let foreign_keys = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration.context("migrate sessions table for DeepSeek Harness")?;
    foreign_keys.context("restore foreign key enforcement after schema migration")?;
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.exists([])? {
        bail!("foreign key violation after migrating the DeepSeek Harness list");
    }
    Ok(())
}

/// Carry unsent chat input across a detach. Added as a structural guard rather
/// than a new schema version so databases written by either development line
/// converge, matching `ensure_managed_worktree_column`.
fn ensure_session_draft_input_column(connection: &Connection) -> Result<()> {
    if !table_has_column(connection, "sessions", "draft_input")? {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN draft_input TEXT NOT NULL DEFAULT '';
             COMMIT;",
        )?;
    }
    Ok(())
}

fn ensure_projection_digest_column(connection: &Connection) -> Result<()> {
    let present = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM pragma_table_info('materialized_sessions')
             WHERE name = 'applied_event_digest'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !present {
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             ALTER TABLE materialized_sessions ADD COLUMN applied_event_digest TEXT NOT NULL
                 DEFAULT '{RELAY_EVENT_GENESIS_DIGEST}'
                 CHECK(length(applied_event_digest) = 64
                       AND applied_event_digest NOT GLOB '*[^0-9a-f]*');
             COMMIT;",
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    /// Rewrites a store's recorded schema version the way another build's
    /// migration ladder would, and forgets that this process verified it.
    fn stamp_schema_version(path: &Path, version: i64) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(&format!("PRAGMA user_version = {version};"))
            .unwrap();
        drop(connection);
        forget_verified_schema(path);
    }

    /// A store ahead of this build cannot be fixed by starting a daemon of
    /// this build, so the reader must not say so. This is the message the
    /// incident in #24 printed twice a second for an hour.
    #[test]
    fn strict_reader_reports_a_newer_store_without_blaming_the_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        drop(open_writer(&path).unwrap());
        stamp_schema_version(&path, SCHEMA_VERSION + 1);

        let error = open_reader_strict(&path).unwrap_err();

        let mismatch = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
            .expect("the reader reports the mismatch as a typed cause");
        assert_eq!(mismatch.found, SCHEMA_VERSION + 1);
        assert_eq!(mismatch.supported, SCHEMA_VERSION);
        let message = mismatch.to_string();
        assert!(message.contains("upgrade Mjolnir"), "got {message}");
        assert!(
            !message.contains("start the Mjolnir daemon"),
            "got {message}"
        );
    }

    /// A store behind this build keeps the advice that works, verbatim, so
    /// existing log greps and runbooks keep matching.
    #[test]
    fn strict_reader_keeps_the_migrate_advice_when_the_store_is_behind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        drop(open_writer(&path).unwrap());
        stamp_schema_version(&path, SCHEMA_VERSION - 1);

        let error = open_reader_strict(&path).unwrap_err();

        let mismatch = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
            .expect("the reader reports the mismatch as a typed cause");
        assert_eq!(
            mismatch.to_string(),
            format!(
                "Mjolnir database schema {} is not the supported schema {SCHEMA_VERSION}; \
                 start the Mjolnir daemon to migrate it",
                SCHEMA_VERSION - 1
            )
        );
    }

    #[test]
    fn strict_reader_rejects_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        drop(open_writer(&path).unwrap());

        let reader = open_reader_strict(&path).unwrap();
        let error = reader
            .execute("CREATE TABLE forbidden(value TEXT)", [])
            .unwrap_err();
        assert!(
            matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ReadOnly)
            ),
            "unexpected mutation error: {error}"
        );
    }
}
