use super::*;

pub fn record_prompt(
    session_id: &str,
    bundle_id: &str,
    event_ordinal: u64,
    submitted_at: Option<&str>,
    text: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    let submitted_at = submitted_at.map(str::to_owned);
    let text = text.to_owned();
    submit_database_write("record_prompt", move |_| {
        record_prompt_to(
            &database_path(),
            &session_id,
            &bundle_id,
            event_ordinal,
            submitted_at.as_deref(),
            &text,
        )
    })
}

pub(super) fn record_prompt_to(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    event_ordinal: u64,
    submitted_at: Option<&str>,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, bundle_id, submitted_at.unwrap_or("unknown")],
    )?;
    let actual_bundle: String = tx.query_row(
        "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    if actual_bundle != bundle_id {
        bail!("session {session_id} belongs to bundle {actual_bundle}, not {bundle_id}");
    }
    tx.execute(
        "INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id, event_ordinal) DO NOTHING",
        params![
            session_id,
            event_ordinal,
            submitted_at
                .map(str::to_owned)
                .unwrap_or_else(|| Utc::now().to_rfc3339()),
            text,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn search_prompts(
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    search_prompts_from(&database_path(), session_id, bundle_id, scope, query)
}

/// Search prompt history, stopping at `limit` matches.
///
/// `search_prompts` pages through the whole table and stops only when a page
/// comes back short, which is fine for a terminal running it against a local
/// database and is not something an HTTP route may reach.
pub fn search_prompts_bounded(
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    limit: usize,
) -> Result<BoundedPromptHistory> {
    search_prompts_bounded_from(&database_path(), session_id, bundle_id, scope, query, limit)
}

pub(super) fn search_prompts_bounded_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    limit: usize,
) -> Result<BoundedPromptHistory> {
    const PAGE_SIZE: usize = 256;
    /// How many rows the search may read before giving up on finding more.
    /// A query that matches nothing must not walk an unbounded history.
    const MAX_ROWS_SCANNED: usize = 4_096;

    let connection = open_reader(path)?;
    let query = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    let mut before = i64::MAX;
    let mut scanned = 0;
    let mut truncated = false;
    loop {
        let page = match scope {
            HistoryScope::Project => query_history_page(
                &connection,
                "SELECT h.history_id, h.session_id, h.text
                 FROM prompt_history h JOIN session_contexts c USING(session_id)
                 WHERE c.bundle_id = ?1 AND h.history_id < ?2
                 ORDER BY h.history_id DESC LIMIT ?3",
                params![bundle_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::Session => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE session_id = ?1 AND history_id < ?2
                 ORDER BY history_id DESC LIMIT ?3",
                params![session_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::All => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE history_id < ?1 ORDER BY history_id DESC LIMIT ?2",
                params![before, PAGE_SIZE as i64],
            )?,
        };
        let page_len = page.len();
        for entry in page {
            before = entry.id;
            scanned += 1;
            if entry.text.to_lowercase().contains(&query) && seen.insert(entry.text.clone()) {
                if matches.len() == limit {
                    truncated = true;
                    break;
                }
                matches.push(entry);
            }
        }
        if truncated || page_len < PAGE_SIZE {
            break;
        }
        if scanned >= MAX_ROWS_SCANNED {
            truncated = true;
            break;
        }
    }
    Ok(BoundedPromptHistory {
        entries: matches,
        truncated,
    })
}

pub(super) fn search_prompts_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    const PAGE_SIZE: usize = 256;
    let connection = open_reader(path)?;
    let query = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    let mut before = i64::MAX;
    loop {
        let page = match scope {
            HistoryScope::Project => query_history_page(
                &connection,
                "SELECT h.history_id, h.session_id, h.text
                 FROM prompt_history h JOIN session_contexts c USING(session_id)
                 WHERE c.bundle_id = ?1 AND h.history_id < ?2
                 ORDER BY h.history_id DESC LIMIT ?3",
                params![bundle_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::Session => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE session_id = ?1 AND history_id < ?2
                 ORDER BY history_id DESC LIMIT ?3",
                params![session_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::All => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE history_id < ?1 ORDER BY history_id DESC LIMIT ?2",
                params![before, PAGE_SIZE as i64],
            )?,
        };
        let page_len = page.len();
        for entry in page {
            before = entry.id;
            if entry.text.to_lowercase().contains(&query) && seen.insert(entry.text.clone()) {
                matches.push(entry);
            }
        }
        if page_len < PAGE_SIZE {
            break;
        }
    }
    Ok(matches)
}

pub(super) fn query_history_page(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<PromptHistoryEntry>> {
    let mut statement = connection.prepare_cached(sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok(PromptHistoryEntry {
            id: row.get(0)?,
            session_id: row.get(1)?,
            text: row.get(2)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}
