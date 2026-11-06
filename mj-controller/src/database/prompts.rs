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

/// One page of prompt history matching `query`, smallest scope first.
///
/// `after` is the `next` cursor of the previous page, or `None` to start.
pub fn search_prompt_page(
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    after: Option<HistoryCursor>,
    limit: usize,
) -> Result<PromptHistoryPage> {
    search_prompt_page_from(
        &database_path(),
        session_id,
        bundle_id,
        scope,
        query,
        after,
        limit,
    )
}

/// This session's stored prompts, newest first, each text once.
pub fn session_prompts(session_id: &str) -> Result<Vec<PromptHistoryEntry>> {
    session_prompts_from(&database_path(), session_id)
}

pub(super) fn session_prompts_from(
    path: &Path,
    session_id: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    let connection = open_reader(path)?;
    let mut seen = std::collections::HashSet::new();
    Ok(query_history_page(
        &connection,
        "SELECT history_id, session_id, text FROM prompt_history
         WHERE session_id = ?1 ORDER BY history_id DESC",
        [session_id],
    )?
    .into_iter()
    .filter(|entry| seen.insert(entry.text.clone()))
    .collect())
}

/// Where prompts come from, smallest scope first. The discriminant is the
/// tier's position in [`HistoryCursor::tier`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HistoryTier {
    /// The asking session's own prompts.
    Session = 0,
    /// Other sessions in the asking session's project.
    Project = 1,
    /// Sessions in every other project.
    Elsewhere = 2,
}

impl HistoryTier {
    /// The tiers a scope reads, in the order their matches are listed.
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

    /// Up to `count` of the tier's matches older than `before`, newest first.
    /// The full-text index drives each query, so it reads matches only.
    fn page(
        self,
        connection: &Connection,
        phrase: &str,
        session_id: &str,
        bundle_id: &str,
        before: i64,
        count: usize,
    ) -> Result<Vec<PromptHistoryEntry>> {
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        match self {
            // The unary `+` keeps SQLite from driving this query from the
            // session's history index and probing the full-text index once per
            // row, which took 75 ms for a 291-prompt session instead of 6 ms.
            HistoryTier::Session => query_history_page(
                connection,
                "SELECT p.history_id, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid < ?2 AND +p.session_id = ?3
                 ORDER BY f.rowid DESC LIMIT ?4",
                params![phrase, before, session_id, count],
            ),
            HistoryTier::Project => query_history_page(
                connection,
                "SELECT p.history_id, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid < ?2 AND p.session_id <> ?3
                   AND EXISTS (SELECT 1 FROM session_contexts c
                               WHERE c.session_id = p.session_id AND c.bundle_id = ?4)
                 ORDER BY f.rowid DESC LIMIT ?5",
                params![phrase, before, session_id, bundle_id, count],
            ),
            HistoryTier::Elsewhere => query_history_page(
                connection,
                "SELECT p.history_id, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid < ?2
                   AND NOT EXISTS (SELECT 1 FROM session_contexts c
                                   WHERE c.session_id = p.session_id AND c.bundle_id = ?3)
                 ORDER BY f.rowid DESC LIMIT ?4",
                params![phrase, before, bundle_id, count],
            ),
        }
    }
}

/// `query` as one full-text phrase, which the trigram index matches as a
/// substring ignoring case. `None` when it is too short for the index.
fn fulltext_phrase(query: &str) -> Option<String> {
    (query.chars().count() >= MIN_HISTORY_QUERY_CHARS)
        .then(|| format!("\"{}\"", query.replace('"', "\"\"")))
}

/// The one prompt search behind the terminal composer and the web viewer.
///
/// Tiers are read in order from the cursor, and the search stops as soon as
/// it holds `limit` matches, so its work follows the matches returned rather
/// than the size of the history. Repeated text appears once within a page;
/// a caller that keeps several pages drops repeats across them.
pub(super) fn search_prompt_page_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    after: Option<HistoryCursor>,
    limit: usize,
) -> Result<PromptHistoryPage> {
    let Some(phrase) = fulltext_phrase(query) else {
        return Ok(PromptHistoryPage::default());
    };
    let connection = open_reader(path)?;
    let canonical_bundle = super::projects::session_project_id(&connection, session_id, bundle_id)?;
    let start = after.unwrap_or(HistoryCursor {
        tier: HistoryTier::Session as u8,
        before: i64::MAX,
    });
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for &tier in HistoryTier::of_scope(scope)
        .iter()
        .filter(|tier| **tier as u8 >= start.tier)
    {
        let mut before = if tier as u8 == start.tier {
            start.before
        } else {
            i64::MAX
        };
        loop {
            let wanted = limit.saturating_sub(entries.len());
            // One row past what is still needed shows whether more exist.
            let rows = tier.page(
                &connection,
                &phrase,
                session_id,
                &canonical_bundle,
                before,
                wanted.saturating_add(1),
            )?;
            let exhausted = rows.len() <= wanted;
            for entry in rows {
                if entries.len() == limit {
                    return Ok(PromptHistoryPage {
                        entries,
                        next: Some(HistoryCursor {
                            tier: tier as u8,
                            before,
                        }),
                    });
                }
                before = entry.id;
                if seen.insert(entry.text.clone()) {
                    entries.push(entry);
                }
            }
            if exhausted {
                break;
            }
        }
    }
    Ok(PromptHistoryPage {
        entries,
        next: None,
    })
}

/// Every match in one page, for tests that check what a search finds rather
/// than how it pages.
#[cfg(test)]
pub(super) fn search_prompts_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    let page = search_prompt_page_from(path, session_id, bundle_id, scope, query, None, 10_000)?;
    assert!(page.next.is_none(), "the test history fits in one page");
    Ok(page.entries)
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
