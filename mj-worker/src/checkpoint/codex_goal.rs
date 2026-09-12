//! Session-scoped native goal checkpoints. Codex stores accounting outside rollouts.
use anyhow::{Context, Result, ensure};
use mj_core::archive::{NativeArtifact, validate_component};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha384};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DATABASE: &str = "goals_1.sqlite";
const ARTIFACT_ROOT: &str = mj_core::goal::NATIVE_ARTIFACT_ROOT;
// Match the pinned Codex goals database migrations, including their checksums.
const MIGRATIONS: [(&str, &str); 2] = [
    (
        "thread goals",
        r#"CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active',
        'paused',
        'blocked',
        'usage_limited',
        'budget_limited',
        'complete'
    )),
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
"#,
    ),
    (
        "thread goal continuation deferrals",
        r#"CREATE TABLE thread_goal_continuation_deferrals (
    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
);
"#,
    ),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Goal {
    goal_id: String,
    objective: String,
    status: String,
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    thread_id: String,
    goal: Option<Goal>,
    continuation_deferred: bool,
}

fn artifact_path(thread_id: &str) -> PathBuf {
    Path::new(ARTIFACT_ROOT).join(format!("{thread_id}.json"))
}

/// Returns the last supported migration. Unknown schemas fail before mutation.
fn schema_version(connection: &Connection) -> Result<usize> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )?;
        ensure!(tables == 0, "unrecognized Codex goal database schema");
        return Ok(0);
    }
    let mut query = connection
        .prepare("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")?;
    let mut rows = query.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let version: i64 = row.get(0)?;
        let success: bool = row.get(1)?;
        let checksum: Vec<u8> = row.get(2)?;
        ensure!(
            version == count as i64 + 1 && count < MIGRATIONS.len() && success,
            "unsupported Codex goal migration {version}"
        );
        ensure!(
            checksum == Sha384::digest(MIGRATIONS[count].1.as_bytes()).to_vec(),
            "Codex goal migration {version} does not match the supported schema"
        );
        count += 1;
    }
    Ok(count)
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    let version = schema_version(connection)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
        version BIGINT PRIMARY KEY, description TEXT NOT NULL,
        installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL
    )",
    )?;
    for (index, (description, sql)) in MIGRATIONS.iter().enumerate().skip(version) {
        connection.execute_batch(sql)?;
        connection.execute("INSERT INTO _sqlx_migrations(version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)",
            params![index as i64 + 1, description, Sha384::digest(sql.as_bytes()).to_vec()])?;
    }
    Ok(())
}

fn read_goal(connection: &Connection, thread_id: &str) -> Result<Option<Goal>> {
    Ok(connection.query_row("SELECT goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms FROM thread_goals WHERE thread_id = ?",
        [thread_id], |r| Ok(Goal { goal_id: r.get(0)?, objective: r.get(1)?, status: r.get(2)?, token_budget: r.get(3)?, tokens_used: r.get(4)?, time_used_seconds: r.get(5)?, created_at_ms: r.get(6)?, updated_at_ms: r.get(7)? })).optional()?)
}

pub(super) fn collect(home: &Path, thread_id: &str) -> Result<Option<NativeArtifact>> {
    validate_component(thread_id, "native goal thread ID")?;
    let path = home.join(DATABASE);
    if !path.try_exists()? {
        return Ok(None);
    }
    let mut snapshot = Snapshot {
        version: 1,
        thread_id: thread_id.into(),
        goal: None,
        continuation_deferred: false,
    };
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("open native Codex goal state for checkpoint")?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let transaction = connection.transaction()?;
    let version = schema_version(&transaction)?;
    ensure!(version > 0, "Codex goal database is not initialized");
    snapshot.goal = read_goal(&transaction, thread_id)?;
    if version >= 2 {
        snapshot.continuation_deferred = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_goal_continuation_deferrals WHERE thread_id = ?)",
            [thread_id],
            |r| r.get(0),
        )?;
    }
    Ok(Some(NativeArtifact {
        relative_path: artifact_path(thread_id),
        data: serde_json::to_vec(&snapshot)?,
        mode: 0o600,
    }))
}

pub(super) fn is_artifact(path: &Path) -> bool {
    path.starts_with(ARTIFACT_ROOT)
}

pub(super) fn restore(home: &Path, thread_id: &str, path: &Path, data: &[u8]) -> Result<()> {
    validate_component(thread_id, "native goal thread ID")?;
    ensure!(
        path == artifact_path(thread_id),
        "goal artifact does not belong to this native session"
    );
    let snapshot: Snapshot =
        serde_json::from_slice(data).context("decode native goal checkpoint")?;
    ensure!(
        snapshot.version == 1 && snapshot.thread_id == thread_id,
        "native goal checkpoint identity/version mismatch"
    );
    if let Some(goal) = &snapshot.goal {
        ensure!(
            !goal.goal_id.is_empty()
                && !goal.objective.trim().is_empty()
                && goal.tokens_used >= 0
                && goal.time_used_seconds >= 0
                && goal.created_at_ms >= 0
                && goal.updated_at_ms >= 0
                && goal.token_budget.is_none_or(|b| b > 0)
                && matches!(
                    goal.status.as_str(),
                    "active"
                        | "paused"
                        | "blocked"
                        | "usage_limited"
                        | "budget_limited"
                        | "complete"
                ),
            "invalid native goal checkpoint"
        );
    }
    ensure!(
        !snapshot.continuation_deferred || snapshot.goal.is_some(),
        "goal continuation deferral has no goal"
    );
    let database = home.join(DATABASE);
    if snapshot.goal.is_none() && !database.try_exists()? {
        return Ok(());
    }
    fs::create_dir_all(home)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&database) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context("create native Codex goal database"),
    }
    let mut connection = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    initialize_schema(&transaction)?;
    transaction.execute(
        "DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ?",
        [thread_id],
    )?;
    transaction.execute("DELETE FROM thread_goals WHERE thread_id = ?", [thread_id])?;
    if let Some(goal) = snapshot.goal {
        transaction.execute("INSERT INTO thread_goals(thread_id, goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![thread_id, goal.goal_id, goal.objective, goal.status, goal.token_budget, goal.tokens_used, goal.time_used_seconds, goal.created_at_ms, goal.updated_at_ms])?;
        if snapshot.continuation_deferred {
            transaction.execute(
                "INSERT INTO thread_goal_continuation_deferrals(thread_id) VALUES (?)",
                [thread_id],
            )?;
        }
    }
    transaction
        .commit()
        .context("restore native Codex goal state")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(home: &Path, thread_id: &str, objective: &str) -> Goal {
        let mut connection = Connection::open(home.join(DATABASE)).unwrap();
        let transaction = connection.transaction().unwrap();
        initialize_schema(&transaction).unwrap();
        let goal = Goal {
            goal_id: format!("identity-{thread_id}"),
            objective: objective.into(),
            status: "paused".into(),
            token_budget: Some(9000),
            tokens_used: 1234,
            time_used_seconds: 56,
            created_at_ms: 1234567,
            updated_at_ms: 2345678,
        };
        transaction
            .execute(
                "INSERT INTO thread_goals VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    thread_id,
                    goal.goal_id,
                    goal.objective,
                    goal.status,
                    goal.token_budget,
                    goal.tokens_used,
                    goal.time_used_seconds,
                    goal.created_at_ms,
                    goal.updated_at_ms
                ],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO thread_goal_continuation_deferrals VALUES (?)",
                [thread_id],
            )
            .unwrap();
        transaction.commit().unwrap();
        goal
    }

    #[test]
    fn goal_checkpoint_restores_accounting_and_only_the_selected_session() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let goal = fixture(source.path(), "selected", "finish campaign");
        fixture(
            source.path(),
            "private-other",
            "unrelated private objective",
        );
        let other = fixture(target.path(), "existing-other", "keep target session");
        let artifact = collect(source.path(), "selected").unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&artifact.data).contains("unrelated private"));
        restore(
            target.path(),
            "selected",
            &artifact.relative_path,
            &artifact.data,
        )
        .unwrap();
        let restored = Connection::open(target.path().join(DATABASE)).unwrap();
        assert_eq!(
            read_goal(&restored, "selected").unwrap(),
            Some(goal.clone())
        );
        assert_eq!(read_goal(&restored, "existing-other").unwrap(), Some(other));
        assert!(read_goal(&restored, "private-other").unwrap().is_none());
        assert_eq!(restored.query_row("SELECT count(*) FROM thread_goal_continuation_deferrals WHERE thread_id='selected'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let fresh = tempfile::tempdir().unwrap();
        restore(
            fresh.path(),
            "selected",
            &artifact.relative_path,
            &artifact.data,
        )
        .unwrap();
        let fresh_db = Connection::open(fresh.path().join(DATABASE)).unwrap();
        assert_eq!(schema_version(&fresh_db).unwrap(), 2);
        assert_eq!(read_goal(&fresh_db, "selected").unwrap(), Some(goal));
    }

    #[test]
    fn cleared_goal_checkpoint_removes_only_that_goal_and_rejects_foreign_identity() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fixture(source.path(), "other", "other");
        let original = fixture(target.path(), "selected", "finish");
        let artifact = collect(source.path(), "selected").unwrap().unwrap();
        assert!(
            restore(
                target.path(),
                "different",
                &artifact.relative_path,
                &artifact.data
            )
            .is_err()
        );
        let db = Connection::open(target.path().join(DATABASE)).unwrap();
        assert_eq!(read_goal(&db, "selected").unwrap(), Some(original));
        restore(
            target.path(),
            "selected",
            &artifact.relative_path,
            &artifact.data,
        )
        .unwrap();
        assert!(read_goal(&db, "selected").unwrap().is_none());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM thread_goal_continuation_deferrals",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_goal_restore_does_not_follow_a_database_symlink() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fixture(source.path(), "selected", "original");
        let artifact = collect(source.path(), "selected").unwrap().unwrap();
        std::os::unix::fs::symlink(source.path().join(DATABASE), target.path().join(DATABASE))
            .unwrap();
        assert!(collect(target.path(), "selected").is_err());
        assert!(
            restore(
                target.path(),
                "selected",
                &artifact.relative_path,
                &artifact.data
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_native_goal_schema_is_rejected_without_changing_existing_goals() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fixture(source.path(), "selected", "new snapshot");
        let original = fixture(target.path(), "selected", "existing snapshot");
        let artifact = collect(source.path(), "selected").unwrap().unwrap();
        let db = Connection::open(target.path().join(DATABASE)).unwrap();
        db.execute(
            "UPDATE _sqlx_migrations SET checksum = x'00' WHERE version=2",
            [],
        )
        .unwrap();
        assert!(
            restore(
                target.path(),
                "selected",
                &artifact.relative_path,
                &artifact.data
            )
            .is_err()
        );
        assert_eq!(read_goal(&db, "selected").unwrap(), Some(original));
        assert!(collect(target.path(), "selected").is_err());
    }
    #[test]
    fn worker_checkpoint_entrypoints_restore_native_goal_accounting() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, archive_path) = crate::checkpoint_tests::fixture(temp.path());
        let expected = fixture(
            &spec.harness_home,
            &spec.session.native_session_id,
            "keep working",
        );
        let encoded = serde_json::to_vec(&spec).unwrap();
        crate::checkpoint::export_from_spec_reader(&mut encoded.as_slice()).unwrap();
        let target_home = temp.path().join("restored-home");
        let restore_spec = mj_core::checkpoint::CheckpointRestoreSpec {
            archive_path,
            workspace_root: spec.workspace_root,
            relay_root: temp.path().join("restored-relay"),
            harness_home: target_home.clone(),
            restore_repositories: false,
            restore_native: true,
            discard_queued_prompts: false,
            primary_repository_root: None,
        };
        let restore_path = temp.path().join("restore.json");
        fs::write(&restore_path, serde_json::to_vec(&restore_spec).unwrap()).unwrap();
        crate::checkpoint::restore_from_spec_file(&restore_path).unwrap();
        let db = Connection::open(target_home.join(DATABASE)).unwrap();
        assert_eq!(
            read_goal(&db, &spec.session.native_session_id).unwrap(),
            Some(expected)
        );
    }

    #[test]
    #[ignore = "requires disposable CODEX_GOAL_SOURCE_HOME, CODEX_GOAL_TARGET_HOME and CODEX_GOAL_THREAD_ID for a packaged native recovery probe"]
    fn checkpoint_goal_into_fresh_live_home() {
        let source = PathBuf::from(std::env::var_os("CODEX_GOAL_SOURCE_HOME").unwrap());
        let target = PathBuf::from(std::env::var_os("CODEX_GOAL_TARGET_HOME").unwrap());
        let thread_id = std::env::var("CODEX_GOAL_THREAD_ID").unwrap();
        assert_ne!(source, target);
        assert!(!target.join(DATABASE).exists());
        let artifact = collect(&source, &thread_id).unwrap().unwrap();
        restore(&target, &thread_id, &artifact.relative_path, &artifact.data).unwrap();
    }
}
