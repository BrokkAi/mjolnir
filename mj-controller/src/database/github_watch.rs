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
    pub pull_request_etag: Option<String>,
    pub pull_request_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GithubSessionTurnWindow {
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
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
        "SELECT owner, repo, number, creator_session_id, kind, title, url, created_at,
                pull_request_etag, pull_request_state
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
                pull_request_etag: row.get(8)?,
                pull_request_state: row.get(9)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

pub(crate) fn commit_github_pull_request_status(
    watch: GithubItemWatch,
    etag: Option<String>,
    state: String,
    event: Option<(String, String)>,
) -> Result<()> {
    let has_event = event.is_some();
    submit_database_write("commit_github_pull_request_status", move |connection| {
        let tx = connection.transaction()?;
        let updated = tx.execute(
            "UPDATE github_watch_items
             SET pull_request_etag=?4, pull_request_state=?5
             WHERE owner=?1 AND repo=?2 AND number=?3 AND kind='pull_request'",
            params![watch.owner, watch.repo, watch.number, etag, state],
        )?;
        ensure!(
            updated == 1,
            "GitHub pull request watch disappeared during polling"
        );
        if let Some((event_key, event_json)) = event {
            enqueue_mailbox_event_with(
                &tx,
                &event_key,
                &watch.creator_session_id,
                &event_json,
                true,
                false,
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    if has_event {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}

/// Reconstruct turn intervals from the same persisted start and completion
/// facts exposed by the transcript projection. An unmatched start counts only
/// when the materialized projection still names it as the active turn.
pub(crate) fn load_github_session_turn_windows(
    session_id: &str,
) -> Result<Vec<GithubSessionTurnWindow>> {
    let mut connection = open_reader(&database_path())?;
    let tx = connection.transaction()?;
    let bodies = {
        let mut statement = tx.prepare(
            "SELECT body FROM api_events
             WHERE session_id=?1
               AND json_extract(body, '$.type') IN ('turn_started', 'turn_ended')
             ORDER BY seq",
        )?;
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut starts = BTreeMap::<String, i64>::new();
    let mut windows = Vec::new();
    for body in bodies {
        let event: mj_core::storage::ApiEventData = serde_json::from_str(&body)?;
        match event {
            mj_core::storage::ApiEventData::TurnStarted { turn } => {
                starts.insert(turn.command_id, turn.started_at_ms);
            }
            mj_core::storage::ApiEventData::TurnEnded { turn } => {
                if let Some(started_at_ms) = starts.remove(&turn.command_id) {
                    windows.push(GithubSessionTurnWindow {
                        started_at_ms,
                        completed_at_ms: Some(turn.completed_at_ms),
                    });
                }
            }
            _ => {}
        }
    }
    let active_turn_json: Option<String> = tx
        .query_row(
            "SELECT active_turn_json FROM materialized_sessions WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if let Some(active_turn) = active_turn_json
        .map(|json| serde_json::from_str::<Option<MaterializedTurn>>(&json))
        .transpose()?
        .flatten()
    {
        let started_at_ms = starts
            .remove(&active_turn.command_id)
            .unwrap_or(active_turn.started_at_ms);
        windows.push(GithubSessionTurnWindow {
            started_at_ms,
            completed_at_ms: None,
        });
    }
    tx.commit()?;
    Ok(windows)
}

#[cfg(test)]
pub(crate) fn seed_github_test_turns(
    session_id: &str,
    completed_start_ms: i64,
    completed_at_ms: i64,
    active_start_ms: i64,
) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("seed_github_test_turns", move |connection| {
        let completed_start = serde_json::json!({
            "type": "turn_started",
            "data": {"turn": {
                "command_id": "completed-turn",
                "turn_start_position": 1,
                "started_at_ms": completed_start_ms
            }}
        });
        let completed_end = serde_json::json!({
            "type": "turn_ended",
            "data": {"turn": {
                "command_id": "completed-turn",
                "completed_ordinal": 1,
                "completed_at_ms": completed_at_ms,
                "outcome": {"kind": "completed"}
            }}
        });
        let active_start = serde_json::json!({
            "type": "turn_started",
            "data": {"turn": {
                "command_id": "active-turn",
                "turn_start_position": 2,
                "started_at_ms": active_start_ms
            }}
        });
        let active_turn = serde_json::json!({
            "command_id": "active-turn",
            "turn_start_position": 2,
            "started_at_ms": active_start_ms
        });
        let tx = connection.transaction()?;
        for (timestamp, event) in [
            (completed_start_ms, completed_start),
            (completed_at_ms, completed_end),
            (active_start_ms, active_start),
        ] {
            tx.execute(
                "INSERT INTO api_events(session_id, recorded_at_ms, body) VALUES (?1, ?2, ?3)",
                params![session_id, timestamp, event.to_string()],
            )?;
        }
        tx.execute(
            "INSERT INTO materialized_sessions(session_id, execution_state,
                    running_started_at_ms, active_turn_json)
             VALUES (?1, 'running', ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET execution_state='running',
                    running_started_at_ms=excluded.running_started_at_ms,
                    active_turn_json=excluded.active_turn_json",
            params![session_id, active_start_ms, active_turn.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    })
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
