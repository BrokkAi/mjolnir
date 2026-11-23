use rusqlite::{Connection, OpenFlags};
use sessionwiki::adapters::Adapter;

use super::native::{NativeCandidate, NativeMetadata};
use super::*;

pub(super) fn is_cli_log(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "antigravity-cli")
}

pub(super) fn candidates(
    home: &Path,
    adapter: &dyn Adapter,
    cache: &NativeScanCache,
) -> Result<Vec<NativeCandidate>> {
    let Some(store) = adapter.store() else {
        return Ok(Vec::new());
    };
    ensure!(
        !store.had_error,
        "could not read all Antigravity CLI conversations in {}",
        home.display()
    );
    let root = home.join("antigravity-cli");
    let summaries = read_summaries(&root.join("conversation_summaries.db"))?;
    let mut candidates = Vec::new();
    for (key, _) in store.keys {
        let Some(id) = adapter.native_id(&key) else {
            continue;
        };
        let path = PathBuf::from(key);
        let conversations = root.join("conversations");
        let database = conversations.join(format!("{id}.db"));
        let legacy = conversations.join(format!("{id}.pb"));
        let (file, issue) = if database.is_file() {
            (&database, None)
        } else if legacy.is_file() {
            (&legacy, None)
        } else {
            (&path, Some("Antigravity conversation database is missing"))
        };
        let metadata = if let Some(summary) = summaries.get(&id) {
            summary.clone()
        } else {
            // Older captures may have no summary. Read the opening log
            // record for a workspace without walking the conversation.
            let Some(log) = store.files.iter().find(|log| log.starts_with(&path)) else {
                continue;
            };
            let Some(metadata) = cache.native_metadata(log, || log_prelude(log))? else {
                continue;
            };
            metadata
        };
        if let Some(candidate) =
            NativeCandidate::from_metadata(adapter, path.clone(), file, metadata, issue)?
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn log_prelude(path: &Path) -> Result<Option<NativeMetadata>> {
    let mut first = String::new();
    BufReader::new(fs::File::open(path)?).read_line(&mut first)?;
    let row: Value = serde_json::from_str(&first)?;
    let cwd = ["workspaceUri", "workspace_uri", "cwd", "workspace"]
        .iter()
        .find_map(|field| row[*field].as_str())
        .and_then(recorded_path);
    Ok(Some(NativeMetadata {
        cwd,
        title: None,
        updated_at: None,
        parent_session_id: None,
    }))
}

fn recorded_path(value: &str) -> Option<String> {
    if value.starts_with("file://") {
        return url::Url::parse(value)
            .ok()?
            .to_file_path()
            .ok()?
            .into_os_string()
            .into_string()
            .ok();
    }
    Some(value.to_owned())
}

fn read_summaries(path: &Path) -> Result<BTreeMap<String, NativeMetadata>> {
    if !path.try_exists()? {
        return Ok(BTreeMap::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let columns = conn
        .prepare("PRAGMA table_info(conversation_summaries)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let parent = if columns.contains("parent_conversation_id") {
        "parent_conversation_id"
    } else {
        "NULL"
    };
    let mut rows = conn.prepare(&format!(
        "SELECT conversation_id, title, workspace_uris, {parent} FROM conversation_summaries"
    ))?;
    let entries = rows.query_map([], |row| {
        let uris: Option<String> = row.get(2)?;
        let cwd = uris
            .and_then(|uris| serde_json::from_str::<Vec<String>>(&uris).ok())
            .and_then(|uris| uris.iter().find_map(|uri| recorded_path(uri)));
        Ok((
            row.get(0)?,
            NativeMetadata {
                title: row.get(1)?,
                cwd,
                updated_at: None,
                parent_session_id: row.get(3)?,
            },
        ))
    })?;
    Ok(entries.collect::<rusqlite::Result<_>>()?)
}
