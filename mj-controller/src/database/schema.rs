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

/// A writer-capable connection whose transactions take the WAL write lock at
/// `BEGIN`, where the busy handler applies. A DEFERRED transaction that has
/// already read cannot wait: SQLite only calls the busy handler when the
/// connection holds no transaction, so the upgrade to a write returns
/// `SQLITE_BUSY` at once (issue 1117).
pub(super) fn open_writer(path: &Path) -> Result<Connection> {
    let mut connection = open_writable(path)?;
    connection.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
    Ok(connection)
}

fn open_writable(path: &Path) -> Result<Connection> {
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
    // entry points compile against the strict reader above. The connection is
    // writable but keeps SQLite's DEFERRED default, so a fixture read does not
    // take the write lock.
    open_writable(path)
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

/// The oldest revision this build upgrades in place: the revision Mjolnir 2.7.2
/// shipped. A new store is created directly at this revision from
/// `baseline.sql`; stores written by older builds are refused.
const BASELINE_SCHEMA_VERSION: i64 = 33;

/// The compatibility floor a baseline store records. Migration 32 (ZCode) was
/// the last breaking change before the baseline.
const BASELINE_MINIMUM_COMPATIBLE_VERSION: i64 = 32;

fn migrate_schema(connection: &Connection) -> Result<()> {
    let state = read_schema_state(connection)?;
    let version = state.revision;
    if version > SCHEMA_VERSION {
        return state.ensure_supported();
    }
    if version == 0 {
        create_baseline_schema(connection)?;
    } else if version < BASELINE_SCHEMA_VERSION {
        // Every release from 2.7.2 through 2.9.x still carries the migration
        // chain below the baseline; every later release refuses, as this one
        // does. That range is closed, so the advice never goes stale.
        bail!(
            "Mjolnir database schema {version} was written by a Mjolnir release older than 2.7.2, \
             which this build cannot upgrade; upgrade through any Mjolnir release from 2.7.2 \
             through 2.9.x first, or start with a fresh data directory (--instance NAME or \
             MJ_DATA_DIR)"
        );
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
    // Compatible: adds one nullable column. Older readers ignore it, and the
    // older writer's session upsert lists columns explicitly, so it preserves
    // the value. An older executable launching such a session uses the shared
    // `/workspace` instead of the recorded per-session path, which is a
    // behaviour difference, not data loss. The compatibility floor stays where
    // it is.
    if version < 35 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN container_workspace TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (35, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 35;
             COMMIT;",
        )?;
    }
    // Compatible: adds one nullable column. Older readers ignore it, and the
    // older writer's session upsert lists columns explicitly, so it preserves
    // the value. An older executable launching such a session runs it without
    // the mbx build cache, which is a behaviour difference, not data loss. The
    // compatibility floor stays where it is.
    if version < 36 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN build_cache_json TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (36, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 36;
             COMMIT;",
        )?;
    }
    // Compatible: adds one nullable column to `session_targets`. Only a
    // container sub-agent child row ever carries a value, and older builds
    // could never start such a child, so an older update that rewrites the row
    // without the column loses nothing usable. Older readers ignore it. The
    // compatibility floor stays where it is.
    if version < 37 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE session_targets ADD COLUMN borrowed_from TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (37, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 37;
             COMMIT;",
        )?;
    }
    // Compatible: adds one table holding the dashboard's conversation pane
    // arrangement per workspace. Older readers never select from it and older
    // writers never touch it, so their updates leave its rows intact; a
    // workspace deleted by an older build still removes them through the
    // foreign key. Losing the table only means the conversation area opens as
    // a single pane. The compatibility floor stays where it is.
    if version < 38 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS workspace_layouts (
                 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
                 layout TEXT NOT NULL
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (38, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 38;
             COMMIT;",
        )?;
    }
    // Breaking: persisted input_required API events can now omit the structured
    // request. Older readers require it and fail to deserialize the event log;
    // the shared read/write compatibility floor must advance with the revision.
    if version < 39 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 39 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (39, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 39;
             COMMIT;",
        )?;
    }
    // Breaking: native child projections and relay observations must be preserved
    // by every reader/writer; older builds cannot interpret their lifecycle.
    if version < 40 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE native_agents (
                 owner TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                 child TEXT NOT NULL,
                 staging INTEGER NOT NULL CHECK(staging IN (0,1)),
                 body TEXT NOT NULL CHECK(json_valid(body)),
                 PRIMARY KEY(owner, child, staging)
             ) STRICT;
             CREATE TABLE native_agent_transcript (
                 owner TEXT NOT NULL,
                 child TEXT NOT NULL,
                 staging INTEGER NOT NULL,
                 stable_id TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 body TEXT NOT NULL CHECK(json_valid(body)),
                 PRIMARY KEY(owner, child, staging, stable_id),
                 FOREIGN KEY(owner, child, staging) REFERENCES native_agents(owner, child, staging)
                     ON DELETE CASCADE ON UPDATE CASCADE
             ) STRICT;
             CREATE INDEX native_agent_transcript_position ON native_agent_transcript(owner, child, staging, position);
             CREATE TABLE native_agent_replay (
                 owner TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE
             ) STRICT;
             UPDATE schema_compatibility SET minimum_compatible_version = 40 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (40, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 40;
             COMMIT;",
        )?;
    }
    // Breaking: clear-context relay commands/outcomes are persisted in event
    // JSON. Older readers cannot decode them or honor the context boundary.
    if version < 41 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 41 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (41, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 41;
             COMMIT;",
        )?;
    }

    // Breaking: older layout writers discard Browse identity and pin badges.
    if version < 42 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 42 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (42, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 42;
             COMMIT;",
        )?;
    }

    // Breaking: durable steering commands/observations and native availability
    // cannot be interpreted or preserved by older readers and writers.
    if version < 43 {
        connection.execute_batch("BEGIN IMMEDIATE;
            UPDATE schema_compatibility SET minimum_compatible_version = 43 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (43, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 43;
            COMMIT;")?;
    }

    // Breaking: new quota recovery commands in stored relay JSON cannot be
    // read or preserved by older binaries, even though the cache is additive.
    if version < 44 {
        connection.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE quota_reset_cache (identity TEXT PRIMARY KEY, body TEXT NOT NULL);
            UPDATE schema_compatibility SET minimum_compatible_version = 44 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (44, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 44;
            COMMIT;")?;
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
    Ok(())
}

/// Create an empty store at the baseline revision in one immediate transaction.
/// The revision is read again under the write lock, so a second process that
/// raced to create the same store finds it already created.
fn create_baseline_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("BEGIN IMMEDIATE;")?;
    let created = (|| -> Result<()> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != 0 {
            return Ok(());
        }
        connection.execute_batch(include_str!("baseline.sql"))?;
        connection.execute(
            "INSERT INTO schema_compatibility(singleton, minimum_compatible_version) VALUES (1, ?1)",
            [BASELINE_MINIMUM_COMPATIBLE_VERSION],
        )?;
        connection.execute(
            "INSERT INTO schema_migrations(version, applied_at)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            [BASELINE_SCHEMA_VERSION],
        )?;
        connection.pragma_update(None, "user_version", BASELINE_SCHEMA_VERSION)?;
        Ok(())
    })();
    match created {
        Ok(()) => connection
            .execute_batch("COMMIT;")
            .context("commit baseline database schema"),
        Err(error) => {
            if let Err(rollback) = connection.execute_batch("ROLLBACK;") {
                tracing::warn!(%rollback, "could not roll back a failed baseline schema");
            }
            Err(error.context("create baseline database schema"))
        }
    }
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
    /// `SCHEMA_VERSION`. Migration 44 adds durable quota recovery commands.
    const MINIMUM_COMPATIBLE_VERSION: i64 = 44;

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
    fn native_agents_and_unstructured_input_raise_the_store_compatibility_floor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let connection = open_writer(&path).unwrap();
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
             DROP TABLE quota_reset_cache;
             DROP TABLE native_agent_transcript;
             DROP TABLE native_agents;
             DROP TABLE native_agent_replay;
             DELETE FROM schema_migrations WHERE version >= 39;
             UPDATE schema_compatibility SET minimum_compatible_version = 32;
             PRAGMA user_version = 38;
             COMMIT;",
            )
            .unwrap();
        migrate_schema(&connection).unwrap();
        let state = read_schema_state(&connection).unwrap();
        assert_eq!(state.revision, SCHEMA_VERSION);
        assert_eq!(state.minimum_compatible, Some(MINIMUM_COMPATIBLE_VERSION));
        let event = ApiEventData::InputRequired {
            request: None,
            turn_id: Some(1),
        };
        #[derive(serde::Deserialize)]
        struct LegacyInputEvent {
            #[serde(rename = "request")]
            _request: mj_core::elicitation::ElicitationRequest,
        }
        let encoded = serde_json::to_value(&event).unwrap();
        assert!(serde_json::from_value::<LegacyInputEvent>(encoded["data"].clone()).is_err());
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
    fn a_failed_baseline_leaves_an_empty_store_that_a_retry_creates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let connection = Connection::open(&path).unwrap();
        // A table the baseline also creates makes its batch fail part way.
        connection
            .execute_batch("CREATE TABLE workspaces(conflict TEXT)")
            .unwrap();

        let error = migrate_schema(&connection).unwrap_err();

        assert!(format!("{error:#}").contains("create baseline database schema"));
        assert!(
            connection.is_autocommit(),
            "the failed baseline left a transaction open"
        );
        assert_eq!(read_schema_state(&connection).unwrap().revision, 0);
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "only the conflicting table remains");

        connection.execute_batch("DROP TABLE workspaces").unwrap();
        drop(connection);
        let writer = open_writer(&path).unwrap();
        let state = read_schema_state(&writer).unwrap();
        assert_eq!(state.revision, SCHEMA_VERSION);
        assert_eq!(state.minimum_compatible, Some(MINIMUM_COMPATIBLE_VERSION));
    }

    #[test]
    fn a_store_from_before_the_baseline_is_refused_with_upgrade_advice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA user_version = {};",
                COMPATIBILITY_METADATA_VERSION - 1
            ))
            .unwrap();
        drop(connection);

        let error = open_writer(&path).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("older than 2.7.2"), "{message}");
        assert!(
            message.contains("from 2.7.2 through 2.9.x"),
            "advice must name the closed range of releases that can migrate: {message}"
        );
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
             INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES ({0}, 'test');
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
