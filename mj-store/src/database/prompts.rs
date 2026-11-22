use super::*;

/// Record a prompt a person is sending to a session, under the project the
/// store holds for that session. Every surface a person types into records
/// through here before it submits the prompt, so the record exists before
/// the session can start or finish the turn, and nothing is left to do once
/// the session accepts it. A prompt the session then refuses stays in the
/// history, as an abandoned draft does. Agent messages do not record.
///
/// `command_id` is the command that will carry the prompt; a retry with the
/// same identity records nothing new.
pub fn record_user_prompt(session_id: &str, command_id: &str, text: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let command_id = command_id.to_owned();
    let text = text.to_owned();
    submit_database_write("record_user_prompt", move |_| {
        record_user_prompt_to(&database_path(), &session_id, &command_id, &text)
    })
}

pub(super) fn record_user_prompt_to(
    path: &Path,
    session_id: &str,
    command_id: &str,
    text: &str,
) -> Result<()> {
    let bundle_id: Option<String> = open_reader(path)?
        .query_row(
            "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    // A session with no context has no row to keep history under; the
    // history trigger would ignore the prompt anyway.
    let Some(bundle_id) = bundle_id else {
        return Ok(());
    };
    record_prompt_text_to(path, session_id, &bundle_id, Some(command_id), text)
}

pub fn record_abandoned_prompt(session_id: &str, bundle_id: &str, text: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    let text = text.to_owned();
    submit_database_write("record_abandoned_prompt", move |_| {
        record_prompt_text_to(&database_path(), &session_id, &bundle_id, None, &text)
    })
}

/// Store `text` as one of the session's prompts. A prompt sent as a command
/// is stored once per command; an abandoned draft, which has no command, is
/// stored unless it equals the session's newest prompt.
pub(super) fn record_prompt_text_to(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    command_id: Option<&str>,
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
    if let Some(command_id) = command_id {
        tx.execute(
            "INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text, command_id)
             VALUES (?1, NULL, ?2, ?3, ?4)
             ON CONFLICT(session_id, command_id) WHERE command_id IS NOT NULL DO NOTHING",
            params![session_id, submitted_at, text, command_id],
        )?;
    } else {
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
    }
    tx.commit()?;
    Ok(())
}

/// A delivered prompt keyed by its event ordinal, for fixtures that need
/// several rows of the same text.
#[cfg(test)]
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
    query_history_page(
        &connection,
        "SELECT max(history_id) AS newest, session_id, text FROM prompt_history
         WHERE session_id = ?1 GROUP BY text ORDER BY newest DESC",
        [session_id],
    )
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

    /// Up to `count` of the tier's distinct matching prompts, newest first by
    /// each text's newest row, among texts whose newest row is older than
    /// `position.before`. Only rows up to `position.through`, the newest row
    /// when the search began, take part, so a prompt sent while the search is open cannot
    /// move an unseen text past the cursor. A text an earlier tier holds is left out, so every text
    /// appears in one tier and on one page. The full-text index drives each
    /// query, so the work follows the tier's matches, not its rows.
    fn page(
        self,
        connection: &Connection,
        phrase: &str,
        session_id: &str,
        bundle_id: &str,
        position: HistoryCursor,
        count: usize,
    ) -> Result<Vec<PromptHistoryEntry>> {
        let HistoryCursor {
            before, through, ..
        } = position;
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        match self {
            // The unary `+` keeps SQLite from driving this query from the
            // session's history index and probing the full-text index once per
            // row, which took 75 ms for a 291-prompt session instead of 6 ms.
            HistoryTier::Session => query_history_page(
                connection,
                "SELECT max(p.history_id) AS newest, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid <= ?5 AND +p.session_id = ?2
                 GROUP BY p.text HAVING newest < ?3
                 ORDER BY newest DESC LIMIT ?4",
                params![phrase, session_id, before, count, through],
            ),
            HistoryTier::Project => query_history_page(
                connection,
                "SELECT max(p.history_id) AS newest, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid <= ?6 AND p.session_id <> ?2
                   AND EXISTS (SELECT 1 FROM session_contexts c
                               WHERE c.session_id = p.session_id AND c.bundle_id = ?3)
                   AND p.text NOT IN (SELECT text FROM prompt_history
                                      WHERE session_id = ?2 AND history_id <= ?6)
                 GROUP BY p.text HAVING newest < ?4
                 ORDER BY newest DESC LIMIT ?5",
                params![phrase, session_id, bundle_id, before, count, through],
            ),
            HistoryTier::Elsewhere => query_history_page(
                connection,
                "SELECT max(p.history_id) AS newest, p.session_id, p.text
                 FROM prompt_history_text f JOIN prompt_history p ON p.history_id = f.rowid
                 WHERE prompt_history_text MATCH ?1 AND f.rowid <= ?6 AND p.session_id <> ?2
                   AND NOT EXISTS (SELECT 1 FROM session_contexts c
                                   WHERE c.session_id = p.session_id AND c.bundle_id = ?3)
                   AND p.text NOT IN (
                       SELECT h.text
                       FROM prompt_history_text g JOIN prompt_history h ON h.history_id = g.rowid
                       WHERE prompt_history_text MATCH ?1 AND g.rowid <= ?6
                         AND (h.session_id = ?2
                              OR EXISTS (SELECT 1 FROM session_contexts d
                                         WHERE d.session_id = h.session_id
                                           AND d.bundle_id = ?3)))
                 GROUP BY p.text HAVING newest < ?4
                 ORDER BY newest DESC LIMIT ?5",
                params![phrase, session_id, bundle_id, before, count, through],
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
/// Tiers are read in order from the cursor, each with one query for the
/// matches it still needs plus one, and the search stops as soon as it holds
/// `limit` matches. Each prompt text is listed once across all pages, at its
/// smallest scope.
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
    let start = match after {
        Some(cursor) => cursor,
        None => HistoryCursor {
            tier: HistoryTier::Session as u8,
            before: i64::MAX,
            through: connection.query_row(
                "SELECT COALESCE(MAX(history_id), 0) FROM prompt_history",
                [],
                |row| row.get(0),
            )?,
        },
    };
    let mut entries: Vec<PromptHistoryEntry> = Vec::new();
    for &tier in HistoryTier::of_scope(scope)
        .iter()
        .filter(|tier| **tier as u8 >= start.tier)
    {
        let before = if tier as u8 == start.tier {
            start.before
        } else {
            i64::MAX
        };
        let wanted = limit.saturating_sub(entries.len());
        // One past what is still needed shows whether more exist.
        let mut rows = tier.page(
            &connection,
            &phrase,
            session_id,
            &canonical_bundle,
            HistoryCursor {
                tier: tier as u8,
                before,
                through: start.through,
            },
            wanted.saturating_add(1),
        )?;
        if rows.len() > wanted {
            rows.truncate(wanted);
            let before = rows.last().map_or(before, |entry| entry.id);
            entries.extend(rows);
            return Ok(PromptHistoryPage {
                entries,
                next: Some(HistoryCursor {
                    tier: tier as u8,
                    before,
                    through: start.through,
                }),
            });
        }
        entries.extend(rows);
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
