use rusqlite::{Connection, OpenFlags};
use sessionwiki::adapters::Adapter;

use super::native::{NativeCandidate, NativeMetadata, absolute_native_cwd};
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
        let metadata = match summaries.get(&id) {
            Some(summary)
                if summary
                    .parent_session_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty()) =>
            {
                continue;
            }
            Some(summary) if summary.cwd.clone().and_then(absolute_native_cwd).is_some() => {
                summary.clone()
            }
            summary => {
                let Some(log) = store.files.iter().find(|log| log.starts_with(&path)) else {
                    continue;
                };
                let Some(log_metadata) = cache.native_metadata(log, || log_workspace(log))? else {
                    continue;
                };
                match summary {
                    Some(summary) => NativeMetadata {
                        cwd: log_metadata.cwd,
                        ..summary.clone()
                    },
                    None => log_metadata,
                }
            }
        };
        if let Some(candidate) =
            NativeCandidate::from_metadata(adapter, path.clone(), file, metadata, issue)?
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn log_workspace(path: &Path) -> Result<Option<NativeMetadata>> {
    let mut cwd = None;
    sessionwiki::util::for_each_line(path, |line| {
        if let Ok(row) = serde_json::from_str(line)
            && let Some(workspace) = workspace_cwd(&row)
        {
            cwd = Some(workspace);
        }
    })?;
    Ok(Some(NativeMetadata {
        cwd,
        title: None,
        updated_at: None,
        parent_session_id: None,
    }))
}

fn workspace_cwd(row: &Value) -> Option<String> {
    ["workspaceUri", "workspace_uri", "cwd", "workspace"]
        .into_iter()
        .map(|field| &row[field])
        .chain(
            [
                "/workspace/uri",
                "/workspace/cwd",
                "/metadata/cwd",
                "/metadata/workspaceUri",
            ]
            .into_iter()
            .filter_map(|pointer| row.pointer(pointer)),
        )
        .filter_map(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .find_map(recorded_path)
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
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let columns = conn
        .prepare("PRAGMA table_info(conversation_summaries)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    if !["conversation_id", "title", "workspace_uris"]
        .iter()
        .all(|col| columns.contains(*col))
    {
        return Ok(BTreeMap::new());
    }
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
            .and_then(|uris| {
                uris.iter()
                    .map(|uri| uri.trim())
                    .filter(|uri| !uri.is_empty())
                    .find_map(recorded_path)
            });
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
