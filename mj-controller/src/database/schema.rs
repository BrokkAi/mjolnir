use super::*;
use rusqlite::OpenFlags;

const COMPATIBILITY_METADATA_VERSION: i64 = 30;

pub(super) struct SchemaState {
    pub(super) revision: i64,
    minimum_compatible: Option<i64>,
}

impl SchemaState {
    pub(super) fn ensure_supported(&self) -> Result<()> {
        let reason = if self.revision < SCHEMA_VERSION {
            StoreSchemaMismatchReason::NeedsMigration
        } else if let Some(minimum_compatible) = self.minimum_compatible {
            if minimum_compatible <= SCHEMA_VERSION {
                return Ok(());
            }
            StoreSchemaMismatchReason::Incompatible { minimum_compatible }
        } else {
            StoreSchemaMismatchReason::InvalidCompatibilityMetadata
        };
        Err(StoreSchemaMismatch {
            found: self.revision,
            supported: SCHEMA_VERSION,
            reason,
        }
        .into())
    }
}

/// The revision, ledger, and compatibility floor must describe one snapshot.
/// A missing floor is only legitimate before compatibility was introduced.
pub(super) fn read_schema_state(connection: &Connection) -> Result<SchemaState> {
    let snapshot = connection
        .unchecked_transaction()
        .context("start database compatibility snapshot")?;
    let revision: i64 = snapshot
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read database migration revision")?;
    let minimum_compatible = if revision >= COMPATIBILITY_METADATA_VERSION {
        let invalid = || StoreSchemaMismatch {
            found: revision,
            supported: SCHEMA_VERSION,
            reason: StoreSchemaMismatchReason::InvalidCompatibilityMetadata,
        };
        let (count, singleton, floor, recorded): (i64, Option<i64>, Option<i64>, Option<i64>) =
            snapshot
                .query_row(
                    "SELECT count(*), min(singleton), min(minimum_compatible_version),
                    (SELECT max(version) FROM schema_migrations)
             FROM schema_compatibility",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|error| {
                    // Missing tables/columns and invalid field types are
                    // structural. Busy, I/O, and interruption errors are not
                    // evidence of an incompatible migration.
                    let structural = match &error {
                        rusqlite::Error::SqliteFailure(code, _) => {
                            code.code == rusqlite::ErrorCode::Unknown
                        }
                        _ => true,
                    };
                    let error = anyhow::Error::new(error);
                    if structural {
                        error.context(invalid())
                    } else {
                        error.context("read database compatibility metadata")
                    }
                })?;
        if count != 1
            || singleton != Some(1)
            || recorded != Some(revision)
            || !floor
                .is_some_and(|floor| (COMPATIBILITY_METADATA_VERSION..=revision).contains(&floor))
        {
            return Err(invalid().into());
        }
        floor
    } else {
        None
    };
    snapshot
        .commit()
        .context("finish database compatibility snapshot")?;
    Ok(SchemaState {
        revision,
        minimum_compatible,
    })
}

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
    read_schema_state(&connection)?.ensure_supported()?;
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
/// Later opens confirm compatibility without repeating schema repairs. A
/// database behind this build is migrated again, so a recreated file under a
/// reused path still converges. Compatible future stores are never repaired.
fn verify_schema_once(path: &Path, connection: &Connection) -> Result<()> {
    let key = schema_cache_key(path);
    let mut verified = verified_schemas()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let state = read_schema_state(connection)?;
    if state.revision > SCHEMA_VERSION
        || (state.revision == SCHEMA_VERSION && verified.contains(&key))
    {
        // An older build must never run its repairs against a newer schema.
        return state.ensure_supported();
    }
    // Holding the lock across the ladder keeps two first opens of the same
    // database from running the additive migration steps against each other.
    migrate_schema(connection)?;
    read_schema_state(connection)?.ensure_supported()?;
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
    let state = read_schema_state(connection)?;
    let version = state.revision;
    if version > SCHEMA_VERSION {
        return state.ensure_supported();
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
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS profile_config_cache (
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
        body TEXT NOT NULL);",
    )?;

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
    if version < 24 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 25 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
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
             COMMIT;",
        )?;
    }
    if version < 26 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE session_moves (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 operation_id TEXT NOT NULL UNIQUE,
                 operation_json TEXT NOT NULL CHECK(json_valid(operation_json))
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (26, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 26;
             COMMIT;",
        )?;
    }
    if version < 27 {
        migrate_muse_harness_kind(connection)?;
    }
    // The projection gained per-turn identity and outcome so a caller driving a
    // session through the HTTP API can wait for a specific prompt and read how
    // it ended. `api_idempotency` makes session creation retry-safe.
    if version < 28 {
        migrate_turn_outcome_columns(connection)?;
    }
    if version < 29 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN create_managed_worktree INTEGER
                 CHECK(create_managed_worktree IN (0, 1));
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (29, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 29;
             COMMIT;",
        )?;
    }
    if version < 30 {
        migrate_compatibility_metadata(connection)?;
    }
    if version < 31 {
        migrate_subagent_sessions(connection)?;
    }
    if version < 32 {
        migrate_zcode_harness_kind(connection)?;
    }
    // Compatible: adds one nullable column. Older readers ignore it, and the
    // older writer's session upsert lists columns explicitly, so it preserves
    // the value. An older executable launching such a session falls back to the
    // global `[subagents] enabled` setting, which is a behaviour difference,
    // not data loss. The compatibility floor stays where it is.
    if version < 33 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN mjolnir_subagents INTEGER
                 CHECK(mjolnir_subagents IN (0, 1));
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (33, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 33;
             COMMIT;",
        )?;
    }
    // Compatible: adds one table. Older readers ignore it and treat read-write
    // mounts as copy-on-write, a behaviour difference rather than lost data.
    // Older writers rewrite `session_mounts` but never touch this table, so its
    // rows survive their updates; a row only applies while a mount with the same
    // source and destination is still not read-only, so an older build that
    // makes the mount read-only or removes it keeps that choice. The
    // compatibility floor stays where it is.
    if version < 34 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS session_mount_access (
                 session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                 source BLOB NOT NULL,
                 destination BLOB NOT NULL,
                 access TEXT NOT NULL CHECK(access IN ('rw')),
                 PRIMARY KEY(session_id, destination)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (34, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 34;
             COMMIT;",
        )?;
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
    ensure_api_events_schema(connection)?;
    Ok(())
}

/// Breaking baseline: earlier executables reject every newer revision.
/// Later compatible migrations retain the floor; breaking ones raise it to
/// their revision in the same transaction as their schema and ledger changes.
fn migrate_compatibility_metadata(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE TABLE schema_compatibility (
             singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
             minimum_compatible_version INTEGER NOT NULL CHECK(minimum_compatible_version >= 30)
         ) STRICT;
         INSERT INTO schema_compatibility(singleton, minimum_compatible_version) VALUES (1, 30);
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (30, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 30;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Breaking migration: older controllers do not understand that a child
/// borrows its parent's target and could destroy shared resources.
fn migrate_subagent_sessions(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS subagent_sessions (
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
         PRAGMA user_version = 31;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Breaking migration: older controllers cannot deserialize the new harness
/// enum and their table constraints reject ZCode rows written by that build.
///
/// The ZCode harness has since been removed. The `'zcode'` value is retained in
/// the `harness_kind` CHECK constraint only so session rows written by earlier
/// releases stay readable; no code accepts it, `HarnessKind::from_str` rejects
/// it, and `load_state_from` skips such a row with a warning. Removing the
/// value would need another breaking migration that rewrote or deleted those
/// rows, so it is left in place. Never rewrite this migration; give any later
/// schema change a new revision.
fn migrate_zcode_harness_kind(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = (|| -> Result<()> {
        let transaction = connection.unchecked_transaction()?;
        for table in ["sessions", "hidden_native_sessions"] {
            let sql: String = transaction.query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )?;
            let (_, definition) = sql
                .split_once('(')
                .context("missing harness table definition")?;
            if definition.contains("'muse','zcode')") {
                continue;
            }
            ensure!(
                definition.contains("'muse')"),
                "unexpected {table} harness constraint"
            );
            let definition = definition.replace("'muse')", "'muse','zcode')");
            let objects: Vec<String> = transaction
                .prepare(
                    "SELECT sql FROM sqlite_schema WHERE tbl_name=?1
                     AND type IN ('index','trigger') AND sql IS NOT NULL",
                )?
                .query_map([table], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            transaction.execute_batch(&format!(
                "CREATE TABLE {table}_zcode_v32 ({definition};
                 INSERT INTO {table}_zcode_v32 SELECT * FROM {table};
                 DROP TABLE {table};
                 ALTER TABLE {table}_zcode_v32 RENAME TO {table};"
            ))?;
            for object in objects {
                transaction.execute_batch(&object)?;
            }
        }
        ensure!(
            !transaction
                .prepare("PRAGMA foreign_key_check")?
                .exists([])?,
            "foreign key violation in ZCode migration"
        );
        transaction.execute_batch(
            "UPDATE schema_compatibility SET minimum_compatible_version = 32
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (32, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 32;",
        )?;
        transaction.commit()?;
        Ok(())
    })();
    let restored = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration.context("migrate ZCode harness constraints")?;
    restored.context("restore foreign key enforcement after ZCode migration")?;
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
/// Add the per-turn identity and outcome columns, the queue's acceptance
/// ordinal, and the API idempotency ledger.
///
/// Each addition is guarded by a structural check rather than by the version
/// alone. A database rebuilt by another build's ladder — or by a test that
/// rewinds `user_version` — can already carry some of these, and a bare
/// `ALTER TABLE` would then fail the whole open.
fn migrate_turn_outcome_columns(connection: &Connection) -> Result<()> {
    let mut statements = String::from("BEGIN IMMEDIATE;\n");
    if !table_has_column(connection, "materialized_sessions", "active_turn_json")? {
        statements.push_str(
            "ALTER TABLE materialized_sessions ADD COLUMN active_turn_json TEXT
                 CHECK(active_turn_json IS NULL OR json_valid(active_turn_json));\n",
        );
    }
    if !table_has_column(
        connection,
        "materialized_sessions",
        "last_turn_outcome_json",
    )? {
        statements.push_str(
            "ALTER TABLE materialized_sessions ADD COLUMN last_turn_outcome_json TEXT
                 CHECK(last_turn_outcome_json IS NULL OR json_valid(last_turn_outcome_json));\n",
        );
    }
    if !table_has_column(
        connection,
        "materialized_queued_prompts",
        "accepted_ordinal",
    )? {
        statements.push_str(
            "ALTER TABLE materialized_queued_prompts ADD COLUMN accepted_ordinal INTEGER
                 CHECK(accepted_ordinal IS NULL OR accepted_ordinal > 0);\n",
        );
    }
    statements.push_str(
        "CREATE TABLE IF NOT EXISTS api_idempotency (
             key TEXT PRIMARY KEY CHECK(length(trim(key)) BETWEEN 1 AND 128),
             session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
             created_at_ms INTEGER NOT NULL
         ) STRICT;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (28, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 28;
         COMMIT;",
    );
    connection.execute_batch(&statements)?;
    Ok(())
}

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

/// Preserve existing data and dependent indexes while admitting Muse sessions.
///
/// The widened constraint this builds still lists `'deepseek'`, carried over
/// from migration 11. The DSH harness has since been removed; the value is
/// retained only so session rows written by earlier releases stay readable.
/// No code accepts it, `HarnessKind::from_str` rejects it, and `load_state_from`
/// skips such a row with a warning. Dropping the value would need another
/// breaking migration that rewrote or deleted those rows.
fn migrate_muse_harness_kind(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = (|| -> Result<()> {
        let transaction = connection.unchecked_transaction()?;
        for table in ["sessions", "hidden_native_sessions"] {
            let sql: String = transaction.query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )?;
            let (_, definition) = sql
                .split_once('(')
                .context("missing harness table definition")?;
            if definition.contains("'deepseek','muse')") {
                continue;
            }
            ensure!(
                definition.contains("'deepseek')"),
                "unexpected {table} harness constraint"
            );
            let definition = definition.replace("'deepseek')", "'deepseek','muse')");
            let objects: Vec<String> = transaction.prepare("SELECT sql FROM sqlite_schema WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL")?
                .query_map([table], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
            transaction.execute_batch(&format!(
                "CREATE TABLE {table}_muse_v27 ({definition}; INSERT INTO {table}_muse_v27 SELECT * FROM {table}; DROP TABLE {table}; ALTER TABLE {table}_muse_v27 RENAME TO {table};"
            ))?;
            for object in objects {
                transaction.execute_batch(&object)?;
            }
        }
        ensure!(
            !transaction
                .prepare("PRAGMA foreign_key_check")?
                .exists([])?,
            "foreign key violation in Muse migration"
        );
        transaction.execute_batch("INSERT INTO schema_migrations(version, applied_at) VALUES (27, strftime('%Y-%m-%dT%H:%M:%fZ','now')); PRAGMA user_version = 27;")?;
        transaction.commit()?;
        Ok(())
    })();
    let restored = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration.context("migrate Muse harness constraints")?;
    restored.context("restore foreign key enforcement after Muse migration")?;
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

fn ensure_api_events_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_events (
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
        END;"
    )?;
    Ok(())
}

#[cfg(test)]
pub(super) fn advance_test_schema(path: &Path, revision: i64, minimum_compatible: i64) {
    let connection = Connection::open(path).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    transaction
        .execute(
            "UPDATE schema_compatibility SET minimum_compatible_version = ?1",
            [minimum_compatible],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, 'test')",
            [revision],
        )
        .unwrap();
    transaction
        .pragma_update(None, "user_version", revision)
        .unwrap();
    transaction.commit().unwrap();
    forget_verified_schema(path);
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    /// The oldest executable revision that can still read and write a store at
    /// `SCHEMA_VERSION`. Migration 32 (ZCode) was the last breaking one; the
    /// compatible migrations after it leave the floor where it is.
    const MINIMUM_COMPATIBLE_VERSION: i64 = 32;

    /// Rewrites a store's recorded schema version the way another build's
    /// migration ladder would, and forgets that this process verified it.
    fn stamp_schema_version(path: &Path, version: i64) {
        if version > SCHEMA_VERSION {
            advance_test_schema(path, version, version);
            return;
        }
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(&format!("PRAGMA user_version = {version};"))
            .unwrap();
        connection
            .execute(
                "DELETE FROM schema_migrations WHERE version > ?1",
                [version],
            )
            .unwrap();
        if version == 30 {
            connection
                .execute(
                    "UPDATE schema_compatibility SET minimum_compatible_version = 30 WHERE singleton = 1",
                    [],
                )
                .unwrap();
        }
        drop(connection);
        forget_verified_schema(path);
    }

    #[test]
    fn older_readers_and_reopened_writers_preserve_a_compatible_future_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let connection = open_writer(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE future_feature(value TEXT NOT NULL);
                 INSERT INTO future_feature VALUES ('preserve me');",
            )
            .unwrap();
        drop(connection);
        advance_test_schema(&path, SCHEMA_VERSION + 1, SCHEMA_VERSION);

        let reader = open_reader_strict(&path).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM future_feature", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserve me"
        );
        assert!(reader.execute("DELETE FROM future_feature", []).is_err());
        drop(reader);

        // A repair would recreate this deliberately removed trigger. A future
        // schema is authoritative even when it differs from our own repairs.
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch("DROP TRIGGER api_session_error_updated;")
            .unwrap();
        drop(raw);
        let writer = open_writer(&path).unwrap();
        assert!(!writer.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'api_session_error_updated')", [], |row| row.get::<_, bool>(0)).unwrap());
        assert_eq!(
            writer
                .query_row("SELECT value FROM future_feature", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserve me"
        );
        let state = read_schema_state(&writer).unwrap();
        assert_eq!(state.revision, SCHEMA_VERSION + 1);
        assert_eq!(state.minimum_compatible, Some(SCHEMA_VERSION));
    }

    #[test]
    fn invalid_compatibility_metadata_refuses_readers_and_writers() {
        for alteration in [
            "DROP TABLE schema_compatibility",
            "DELETE FROM schema_compatibility",
            "PRAGMA ignore_check_constraints = ON; UPDATE schema_compatibility SET minimum_compatible_version = 0",
            "UPDATE schema_compatibility SET minimum_compatible_version = 99999",
            "PRAGMA ignore_check_constraints = ON; UPDATE schema_compatibility SET singleton = 2",
            "PRAGMA ignore_check_constraints = ON; INSERT INTO schema_compatibility VALUES (2, 30)",
            "DROP TABLE schema_compatibility; CREATE TABLE schema_compatibility(singleton, minimum_compatible_version); INSERT INTO schema_compatibility VALUES (1, 'invalid')",
            "DELETE FROM schema_migrations WHERE version = (SELECT max(version) FROM schema_migrations)",
        ] {
            for future in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("mj.sqlite3");
                drop(open_writer(&path).unwrap());
                if future {
                    advance_test_schema(&path, SCHEMA_VERSION + 1, SCHEMA_VERSION);
                }
                let raw = Connection::open(&path).unwrap();
                raw.execute_batch(alteration).unwrap();
                let before: i64 = raw
                    .query_row("PRAGMA schema_version", [], |row| row.get(0))
                    .unwrap();
                // Exercise the cached path as well as a fresh writer open.
                for error in [
                    open_reader_strict(&path).unwrap_err(),
                    open_writer(&path).unwrap_err(),
                ] {
                    let mismatch = error.downcast_ref::<StoreSchemaMismatch>().unwrap();
                    assert_eq!(
                        mismatch.reason,
                        StoreSchemaMismatchReason::InvalidCompatibilityMetadata,
                        "{alteration}"
                    );
                }
                forget_verified_schema(&path);
                assert!(open_writer(&path).is_err(), "{alteration}");
                let after: i64 = raw
                    .query_row("PRAGMA schema_version", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(
                    before, after,
                    "a rejected open repaired schema: {alteration}"
                );
            }
        }
    }

    #[test]
    fn compatibility_baseline_migration_is_atomic_and_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let connection = open_writer(&path).unwrap();
        let state = read_schema_state(&connection).unwrap();
        assert_eq!(state.minimum_compatible, Some(MINIMUM_COMPATIBLE_VERSION));
        connection
            .execute_batch(
                "DROP TABLE schema_compatibility;
             ALTER TABLE sessions DROP COLUMN mjolnir_subagents;
             DELETE FROM schema_migrations WHERE version >= 30;
             PRAGMA user_version = 29;
             CREATE TRIGGER reject_baseline BEFORE INSERT ON schema_migrations
             WHEN NEW.version = 30 BEGIN SELECT RAISE(ABORT, 'injected migration failure'); END;",
            )
            .unwrap();
        forget_verified_schema(&path);
        let error = migrate_schema(&connection).unwrap_err();
        assert!(error.to_string().contains("injected migration failure"));
        assert!(
            connection.is_autocommit(),
            "the failed migration left a transaction open"
        );
        assert_eq!(read_schema_state(&connection).unwrap().revision, 29);
        assert_eq!(
            connection
                .query_row("SELECT max(version) FROM schema_migrations", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            29
        );
        assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'schema_compatibility')", [], |row| row.get::<_, bool>(0)).unwrap());
        connection
            .execute_batch("DROP TRIGGER reject_baseline")
            .unwrap();
        drop(connection);
        let writer = open_writer(&path).unwrap();
        let state = read_schema_state(&writer).unwrap();
        assert_eq!(state.revision, SCHEMA_VERSION);
        assert_eq!(state.minimum_compatible, Some(MINIMUM_COMPATIBLE_VERSION));
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
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch(&format!(
            "UPDATE schema_compatibility SET minimum_compatible_version = {0};
             DELETE FROM schema_migrations WHERE version > {0};
             PRAGMA user_version = {0};",
            SCHEMA_VERSION - 1
        ))
        .unwrap();
        drop(raw);

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
