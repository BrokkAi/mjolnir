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

pub fn record_abandoned_prompt(session_id: &str, bundle_id: &str, text: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    let text = text.to_owned();
    submit_database_write("record_abandoned_prompt", move |_| {
        record_abandoned_prompt_to(&database_path(), &session_id, &bundle_id, &text)
    })
}

pub(super) fn record_abandoned_prompt_to(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let submitted_at = Utc::now().to_rfc3339();
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let canonical_bundle = super::projects::session_project_id(&tx, session_id, bundle_id)?;
    tx.execute(
        "INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, canonical_bundle, submitted_at],
    )?;
    let actual_bundle: String = tx.query_row(
        "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    if actual_bundle != canonical_bundle {
        bail!("session {session_id} belongs to bundle {actual_bundle}, not {bundle_id}");
    }
    let most_recent: Option<String> = tx
        .query_row(
            "SELECT text FROM prompt_history WHERE session_id = ?1
             ORDER BY history_id DESC LIMIT 1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    if most_recent.as_deref() != Some(text) {
        tx.execute(
            "INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
             VALUES (?1, NULL, ?2, ?3)",
            params![session_id, submitted_at, text],
        )?;
    }
    tx.commit()?;
    Ok(())
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
    let canonical_bundle = super::projects::session_project_id(&tx, session_id, bundle_id)?;
    tx.execute(
        "INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![
            session_id,
            canonical_bundle,
            submitted_at.unwrap_or("unknown")
        ],
    )?;
    let actual_bundle: String = tx.query_row(
        "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    if actual_bundle != canonical_bundle {
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
    search_prompt_tiers(
        path,
        session_id,
        bundle_id,
        scope,
        query,
        Some(SearchBounds {
            limit,
            // A query that matches nothing must not walk an unbounded history.
            max_rows_scanned: 4_096,
        }),
    )
}

pub(super) fn search_prompts_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    Ok(search_prompt_tiers(path, session_id, bundle_id, scope, query, None)?.entries)
}

/// Where prompts come from, smallest scope first.
#[derive(Clone, Copy)]
enum HistoryTier {
    /// The asking session's own prompts.
    Session,
    /// Other sessions in the asking session's project.
    Project,
    /// Sessions in every other project.
    Elsewhere,
}

impl HistoryTier {
    /// The tiers a scope reads, in the order their matches are listed. Within
    /// a tier, matches run newest first.
    fn of_scope(scope: HistoryScope) -> &'static [HistoryTier] {
        match scope {
            HistoryScope::Session => &[HistoryTier::Session],
            HistoryScope::Project => &[HistoryTier::Session, HistoryTier::Project],
            HistoryScope::All => &[
                HistoryTier::Session,
                HistoryTier::Project,
                HistoryTier::Elsewhere,
            ],
        }
    }

    /// One newest-first page of the tier's rows older than `before`.
    fn page(
        self,
        connection: &Connection,
        session_id: &str,
        bundle_id: &str,
        before: i64,
        page_size: usize,
    ) -> Result<Vec<PromptHistoryEntry>> {
        let page_size = page_size as i64;
        match self {
            HistoryTier::Session => query_history_page(
                connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE session_id = ?1 AND history_id < ?2
                 ORDER BY history_id DESC LIMIT ?3",
                params![session_id, before, page_size],
            ),
            HistoryTier::Project => query_history_page(
                connection,
                "SELECT h.history_id, h.session_id, h.text
                 FROM prompt_history h JOIN session_contexts c USING(session_id)
                 WHERE c.bundle_id = ?1 AND h.session_id <> ?2 AND h.history_id < ?3
                 ORDER BY h.history_id DESC LIMIT ?4",
                params![bundle_id, session_id, before, page_size],
            ),
            HistoryTier::Elsewhere => query_history_page(
                connection,
                "SELECT h.history_id, h.session_id, h.text FROM prompt_history h
                 WHERE h.history_id < ?1 AND NOT EXISTS (
                     SELECT 1 FROM session_contexts c
                     WHERE c.session_id = h.session_id AND c.bundle_id = ?2)
                 ORDER BY h.history_id DESC LIMIT ?3",
                params![before, bundle_id, page_size],
            ),
        }
    }
}

struct SearchBounds {
    /// Matches to return before reporting the result as truncated.
    limit: usize,
    /// Rows the search may read, across all tiers, before it gives up.
    max_rows_scanned: usize,
}

/// The one prompt search behind the terminal composer and the web viewer.
///
/// Tiers are read one after another, each as indexed newest-first pages, so a
/// scan cap spends its rows on the smaller scopes first. A prompt that
/// appears in several tiers is listed once, at its smallest scope.
fn search_prompt_tiers(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    bounds: Option<SearchBounds>,
) -> Result<BoundedPromptHistory> {
    const PAGE_SIZE: usize = 256;

    let connection = open_reader(path)?;
    let canonical_bundle = super::projects::session_project_id(&connection, session_id, bundle_id)?;
    let query = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    let mut scanned = 0;
    for tier in HistoryTier::of_scope(scope) {
        let mut before = i64::MAX;
        loop {
            let page = tier.page(
                &connection,
                session_id,
                &canonical_bundle,
                before,
                PAGE_SIZE,
            )?;
            let page_len = page.len();
            for entry in page {
                before = entry.id;
                scanned += 1;
                if entry.text.to_lowercase().contains(&query) && seen.insert(entry.text.clone()) {
                    if bounds
                        .as_ref()
                        .is_some_and(|bounds| matches.len() == bounds.limit)
                    {
                        return Ok(BoundedPromptHistory {
                            entries: matches,
                            truncated: true,
                        });
                    }
                    matches.push(entry);
                }
            }
            if page_len < PAGE_SIZE {
                break;
            }
            if bounds
                .as_ref()
                .is_some_and(|bounds| scanned >= bounds.max_rows_scanned)
            {
                return Ok(BoundedPromptHistory {
                    entries: matches,
                    truncated: true,
                });
            }
        }
    }
    Ok(BoundedPromptHistory {
        entries: matches,
        truncated: false,
    })
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
