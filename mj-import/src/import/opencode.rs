use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sessionwiki::adapters::Adapter;

use super::native::{NativeCandidate, NativeMetadata, optional_entries};
use super::*;

pub(super) use mj_checkpoint::native::opencode::database_key;

pub(super) const LEGACY: &str = "Open this session in OpenCode and let it finish migrating its session database before importing.";

pub(super) fn candidates(
    home: &Path,
    adapter: &dyn Adapter,
    cache: &NativeScanCache,
) -> Result<Vec<NativeCandidate>> {
    let root = HarnessKind::OpenCode
        .nested_data_dir(home)
        .context("OpenCode data directory is unavailable")?;
    let mut databases = Vec::new();
    if let Some(override_path) = std::env::var_os("OPENCODE_DB")
        .filter(|path| !path.is_empty() && HarnessKind::OpenCode.is_own_nested_home(home))
    {
        if override_path == ":memory:" {
            return Ok(Vec::new());
        }
        databases.push(root.join(override_path));
    } else {
        for entry in optional_entries(&root)? {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(|name| {
                name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db"))
            }) && entry.file_type()?.is_file()
            {
                databases.push(entry.path());
            }
        }
    }
    let mut candidates = Vec::new();
    databases.retain(|path| path.is_file());
    let has_database = !databases.is_empty();
    for database in databases {
        scan_database(adapter, &database, &mut candidates)
            .with_context(|| format!("list OpenCode sessions in {}", database.display()))?;
    }
    if !has_database {
        for project in optional_entries(&root.join("storage/session"))? {
            let project = project?;
            if !project.file_type()?.is_dir() {
                continue;
            }
            for entry in optional_entries(&project.path())? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_file() && path.extension().is_some_and(|ext| ext == "json")
                {
                    let Some(metadata) = cache.native_metadata(&path, || legacy_metadata(&path))?
                    else {
                        continue;
                    };

                    if let Some(candidate) = NativeCandidate::from_metadata(
                        adapter,
                        path.clone(),
                        &path,
                        metadata,
                        Some(LEGACY),
                    )? {
                        candidates.push(candidate);
                    }
                }
            }
        }
    }
    Ok(candidates)
}

fn scan_database(
    adapter: &dyn Adapter,
    database: &Path,
    candidates: &mut Vec<NativeCandidate>,
) -> Result<()> {
    let conn = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let tx = conn.unchecked_transaction()?;
    let tables = tx
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let migration: Option<String> = if tables.contains("kv") {
        tx.query_row(
            "SELECT value FROM kv WHERE key = 'migration.v1-v2'",
            [],
            |row| row.get(0),
        )
        .optional()?
    } else {
        None
    };
    let migration: Value = migration
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let cursor = migration["cursor"].as_str();
    let mut seen = BTreeSet::new();
    let database_text = database
        .to_str()
        .context("OpenCode database path is not UTF-8")?;
    ensure!(
        !database_text.contains('\u{1f}'),
        "OpenCode database path contains its key separator"
    );
    for table in ["session_v2", "session"] {
        if !tables.contains(table) || (table == "session" && migration["phase"] == "completed") {
            continue;
        }
        let mut statement = tx.prepare(&format!(
            "SELECT id, title, directory, time_updated, time_created, parent_id FROM {table}"
        ))?;
        let rows = statement.query_map([], |row| {
            let updated: Option<i64> = row.get(3)?;
            let created: Option<i64> = row.get(4)?;
            Ok((
                row.get::<_, String>(0)?,
                NativeMetadata {
                    title: row.get(1)?,
                    cwd: row.get(2)?,
                    updated_at: updated
                        .or(created)
                        .filter(|time| *time > 0)
                        .and_then(chrono::DateTime::from_timestamp_millis),
                    parent_session_id: row.get(5)?,
                },
            ))
        })?;
        for row in rows {
            let (id, metadata) = row?;
            if (table == "session" && cursor.is_some_and(|cursor| id.as_str() >= cursor))
                || !seen.insert(id.clone())
            {
                continue;
            }
            let key = PathBuf::from(format!("{database_text}\u{1f}{id}"));
            if let Some(candidate) = NativeCandidate::from_metadata(
                adapter,
                key,
                database,
                metadata,
                (table == "session").then_some(LEGACY),
            )? {
                candidates.push(candidate);
            }
        }
    }
    Ok(())
}

fn legacy_metadata(path: &Path) -> Result<Option<NativeMetadata>> {
    let row: Value = serde_json::from_reader(BufReader::new(fs::File::open(path)?))?;
    Ok(Some(NativeMetadata {
        title: row["title"].as_str().map(str::to_owned),
        cwd: row["directory"].as_str().map(str::to_owned),
        updated_at: row["time"]["updated"]
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis),
        parent_session_id: row["parentID"].as_str().map(str::to_owned),
    }))
}

/// The pinned release can store an assistant message's parts in separate
/// rows. SessionWiki 0.34.28 reads inline parts. Build its expected shape
/// for projection only, leaving the original database layout for resume.
pub(super) fn parse_database(
    adapter: &dyn Adapter,
    key: &Path,
) -> Result<sessionwiki::model::Session> {
    let (database, id) = database_key(key).context("invalid OpenCode session key")?;
    let database = std::path::absolute(database)?;
    let database = database.as_path();
    let source = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(std::time::Duration::from_secs(5))?;
    let has_parts: bool = source.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'session_part')",
        [],
        |row| row.get(0),
    )?;
    if !has_parts
        || !source.query_row(
            "SELECT EXISTS (SELECT 1 FROM session_part WHERE session_id = ?1)",
            [id],
            |row| row.get::<_, bool>(0),
        )?
    {
        return adapter.parse_key(key.to_str().context("OpenCode key is not UTF-8")?);
    }
    let scratch = tempfile::tempdir()?;
    let scratch_path = scratch.path().join("opencode.db");
    {
        let conn = Connection::open(&scratch_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut uri = url::Url::from_file_path(database)
            .map_err(|()| anyhow::anyhow!("cannot make SQLite URI for {}", database.display()))?;
        uri.query_pairs_mut().append_pair("mode", "ro");
        conn.execute("ATTACH DATABASE ?1 AS native", [uri.as_str()])?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "CREATE TABLE session_v2 AS SELECT * FROM native.session_v2 WHERE id = ?1",
            [id],
        )?;
        tx.execute("CREATE TABLE session_message AS SELECT * FROM native.session_message WHERE session_id = ?1", [id])?;
        // During a format transition the database may contain both a stored
        // prefix and inline trailing parts. Match OpenCode's replay order.
        tx.execute_batch("
            UPDATE session_message AS m SET data = json_set(m.data, '$.content', json((
                SELECT json_group_array(json(content)) FROM (
                    SELECT content FROM (
                        SELECT json_set(p.data, '$.type', p.type) AS content
                        FROM native.session_part AS p
                        WHERE p.message_id = m.id AND p.session_id = m.session_id ORDER BY p.ordinal
                    )
                    UNION ALL
                    SELECT value AS content FROM json_each(m.data, '$.content')
                    WHERE json_type(m.data, '$.content') = 'array'
                )
            ))) WHERE m.type = 'assistant' AND EXISTS (
                SELECT 1 FROM native.session_part AS p WHERE p.message_id = m.id AND p.session_id = m.session_id
            )
        ")?;
        tx.commit()?;
    }
    let key = format!(
        "{}\u{1f}{id}",
        scratch_path.to_str().context("scratch path is not UTF-8")?
    );
    adapter.parse_key(&key)
}
