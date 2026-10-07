//! Durable cursors, classifications, and creator watches for GitHub polling.

use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GithubRepoCursor {
    pub items_watermark_at: Option<String>,
    pub items_watermark_id: Option<i64>,
    pub items_etag: Option<String>,
    pub comments_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubItemClassification {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub url: String,
    pub created_at: String,
    pub interested: bool,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubItemWatch {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub creator_session_id: String,
    pub kind: String,
    pub title: String,
    pub url: String,
    pub created_at: String,
}

pub(crate) fn load_github_repo_cursor(owner: &str, repo: &str) -> Result<Option<GithubRepoCursor>> {
    let connection = open_reader(&database_path())?;
    connection
        .query_row(
            "SELECT items_watermark_at, items_watermark_id, items_etag, comments_cursor
             FROM github_watch_cursors WHERE owner=?1 AND repo=?2",
            params![owner, repo],
            |row| {
                Ok(GithubRepoCursor {
                    items_watermark_at: row.get(0)?,
                    items_watermark_id: row.get(1)?,
                    items_etag: row.get(2)?,
                    comments_cursor: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

pub(crate) fn save_github_repo_cursor(
    owner: &str,
    repo: &str,
    cursor: GithubRepoCursor,
) -> Result<()> {
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    submit_database_write("save_github_repo_cursor", move |connection| {
        connection.execute(
            "INSERT INTO github_watch_cursors(owner, repo, items_watermark_at,
                    items_watermark_id, items_etag, comments_cursor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(owner, repo) DO UPDATE SET
                items_watermark_at=excluded.items_watermark_at,
                items_watermark_id=excluded.items_watermark_id,
                items_etag=excluded.items_etag,
                comments_cursor=excluded.comments_cursor",
            params![
                owner,
                repo,
                cursor.items_watermark_at,
                cursor.items_watermark_id,
                cursor.items_etag,
                cursor.comments_cursor
            ],
        )?;
        Ok(())
    })
}

pub(crate) fn load_github_item_classified_sessions(
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<std::collections::BTreeSet<String>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT session_id FROM github_watch_classifications
         WHERE owner=?1 AND repo=?2 AND number=?3",
    )?;
    statement
        .query_map(params![owner, repo, number], |row| row.get(0))?
        .map(|row| row.map_err(Into::into))
        .collect()
}

/// Classification, creator watch, and resulting interest event share one
/// transaction. A process restart cannot leave one without the others.
pub(crate) fn commit_github_item_classification(
    classification: GithubItemClassification,
    event: Option<(String, String, bool)>,
) -> Result<bool> {
    let has_event = event.is_some();
    let inserted = submit_database_write("commit_github_item_classification", move |connection| {
        let tx = connection.transaction()?;
        let inserted = tx.execute(
            "INSERT INTO github_watch_classifications(owner, repo, number, session_id,
                    kind, title, url, created_at, interested, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(owner, repo, number, session_id) DO NOTHING",
            params![
                classification.owner,
                classification.repo,
                classification.number,
                classification.session_id,
                classification.kind,
                classification.title,
                classification.url,
                classification.created_at,
                classification.interested,
                classification.created
            ],
        )?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(false);
        }
        if classification.created {
            tx.execute(
                "INSERT INTO github_watch_items(owner, repo, number, creator_session_id,
                        kind, title, url, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(owner, repo, number) DO NOTHING",
                params![
                    classification.owner,
                    classification.repo,
                    classification.number,
                    classification.session_id,
                    classification.kind,
                    classification.title,
                    classification.url,
                    classification.created_at
                ],
            )?;
        }
        if let Some((event_key, event_json, wake)) = event {
            enqueue_mailbox_event_with(
                &tx,
                &event_key,
                &classification.session_id,
                &event_json,
                wake,
                false,
            )?;
        }
        tx.commit()?;
        Ok(true)
    })?;
    if inserted && has_event {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(inserted)
}

pub(crate) fn load_github_watches(owner: &str, repo: &str) -> Result<Vec<GithubItemWatch>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT owner, repo, number, creator_session_id, kind, title, url, created_at
         FROM github_watch_items WHERE owner=?1 AND repo=?2
         ORDER BY number, creator_session_id",
    )?;
    statement
        .query_map(params![owner, repo], |row| {
            Ok(GithubItemWatch {
                owner: row.get(0)?,
                repo: row.get(1)?,
                number: row.get(2)?,
                creator_session_id: row.get(3)?,
                kind: row.get(4)?,
                title: row.get(5)?,
                url: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

pub(crate) fn load_github_watched_repositories() -> Result<Vec<(String, String)>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection
        .prepare("SELECT DISTINCT owner, repo FROM github_watch_items ORDER BY owner, repo")?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .map(|row| row.map_err(Into::into))
        .collect()
}

/// Commit newly observed comments and their cursor together. If the daemon
/// stops mid-poll, it either retries the events by stable key or keeps the old
/// cursor; it cannot advance past an event it failed to queue.
pub(crate) fn commit_github_comment_events(
    owner: &str,
    repo: &str,
    comments_cursor: String,
    events: Vec<(String, String, String, bool)>,
) -> Result<()> {
    let has_events = !events.is_empty();
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    submit_database_write("commit_github_comment_events", move |connection| {
        let tx = connection.transaction()?;
        for (event_key, target_session_id, event_json, wake) in events {
            tx.execute(
                "INSERT INTO mailbox_outbox(event_key, target_session_id, event_json, wake, unpark, created_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5) ON CONFLICT(event_key) DO NOTHING",
                params![
                    event_key,
                    target_session_id,
                    event_json,
                    wake,
                    Utc::now().to_rfc3339()
                ],
            )?;
        }
        tx.execute(
            "INSERT INTO github_watch_cursors(owner, repo, comments_cursor)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(owner, repo) DO UPDATE SET comments_cursor=excluded.comments_cursor",
            params![owner, repo, comments_cursor],
        )?;
        tx.commit()?;
        Ok(())
    })?;
    if has_events {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}
