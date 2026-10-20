//! Durable cursors, classifications, and creator watches for GitHub polling.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubRepoCursor {
    pub updated_at: String,
    pub watch_started_at: String,
    pub reconciled_through: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubSeenItem {
    pub number: i64,
    pub kind: String,
    pub title: String,
    pub url: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
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
    pub timeline_at: String,
    pub timeline_id: i64,
    pub review_comments_at: String,
    pub review_comments_id: i64,
    pub processed_at: String,
    pub recorded_updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GithubSessionTurnWindow {
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct GithubClassificationFailure {
    pub attempts: i64,
    pub last_attempt_at_ms: i64,
    pub next_attempt_at_ms: i64,
}

pub(crate) fn load_github_classification_failures(
    owner: &str,
    repo: &str,
) -> Result<BTreeMap<(i64, String), GithubClassificationFailure>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare("SELECT number, session_id, attempts, last_attempt_at_ms, next_attempt_at_ms FROM github_watch_classification_failures WHERE owner=?1 AND repo=?2")?;
    statement
        .query_map(params![owner, repo], |row| {
            Ok((
                (row.get(0)?, row.get(1)?),
                GithubClassificationFailure {
                    attempts: row.get(2)?,
                    last_attempt_at_ms: row.get(3)?,
                    next_attempt_at_ms: row.get(4)?,
                },
            ))
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

pub(crate) fn record_github_classification_failure(
    owner: String,
    repo: String,
    number: i64,
    session_id: String,
) -> Result<()> {
    submit_database_write("record_github_classification_failure", move |connection| {
        let tx = connection.transaction()?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM github_watch_classifications WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4)",
            params![owner,repo,number,session_id], |row| row.get::<_,bool>(0))? {
            tx.commit()?; return Ok(());
        }
        let attempts: i64 = tx.query_row("SELECT attempts FROM github_watch_classification_failures WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4",
            params![owner, repo, number, session_id], |row| row.get(0)).optional()?.unwrap_or(0_i64).saturating_add(1);
        let now = mj_core::clock::epoch_millis();
        let delay_ms = (60_000_i64 * (1_i64 << (attempts - 1).min(6))).min(3_600_000);
        tx.execute("INSERT INTO github_watch_classification_failures(owner,repo,number,session_id,attempts,last_attempt_at_ms,next_attempt_at_ms)
            VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(owner,repo,number,session_id) DO UPDATE SET
            attempts=excluded.attempts,last_attempt_at_ms=excluded.last_attempt_at_ms,next_attempt_at_ms=excluded.next_attempt_at_ms",
            params![owner,repo,number,session_id,attempts,now,now.saturating_add(delay_ms)])?;
        tx.commit()?;
        Ok(())
    })
}

/// Initialize the watch boundary once, before the first network request.
pub(crate) fn github_repo_cursor(owner: &str, repo: &str) -> Result<GithubRepoCursor> {
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    submit_database_write("github_repo_cursor", move |connection| {
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO github_watch_cursors(owner, repo, updated_at, watch_started_at, reconciled_through)
             VALUES (?1, ?2, ?3, ?3, ?3) ON CONFLICT(owner, repo) DO NOTHING",
            params![owner, repo, now],
        )?;
        Ok(connection.query_row(
            "SELECT updated_at, watch_started_at, reconciled_through FROM github_watch_cursors WHERE owner=?1 AND repo=?2",
            params![owner, repo],
            |row| Ok(GithubRepoCursor { updated_at: row.get(0)?, watch_started_at: row.get(1)?, reconciled_through: row.get(2)? }),
        )?)
    })
}

pub(crate) enum GithubFeedProgress {
    Main(String),
    Reconciled(String),
}

/// Recording never depends on classification or delivery. Each page is durable.
pub(crate) fn record_github_page(
    owner: String,
    repo: String,
    items: Vec<GithubSeenItem>,
    progress: GithubFeedProgress,
) -> Result<()> {
    submit_database_write("record_github_page", move |connection| {
        let tx = connection.transaction()?;
        for item in items {
            tx.execute(
                "INSERT INTO github_seen_items(owner, repo, number, kind, title, url, author, body, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(owner, repo, number) DO UPDATE SET kind=excluded.kind, title=excluded.title,
                    url=excluded.url, author=excluded.author, body=excluded.body,
                    created_at=excluded.created_at, updated_at=excluded.updated_at
                 WHERE julianday(excluded.updated_at) >= julianday(github_seen_items.updated_at)",
                params![owner, repo, item.number, item.kind, item.title, item.url, item.author, item.body, item.created_at, item.updated_at],
            )?;
        }
        let (sql, through) = match progress {
            GithubFeedProgress::Main(through) => (
                "UPDATE github_watch_cursors SET updated_at=?3 WHERE owner=?1 AND repo=?2
                 AND julianday(updated_at) <= julianday(?3)",
                through,
            ),
            GithubFeedProgress::Reconciled(through) => (
                "UPDATE github_watch_cursors SET reconciled_through=?3 WHERE owner=?1 AND repo=?2
                 AND julianday(reconciled_through) <= julianday(?3)",
                through,
            ),
        };
        tx.execute(sql, params![owner, repo, through])?;
        tx.commit()?;
        Ok(())
    })
}

/// Claim before requesting pages so failures and restarts cannot exceed the hourly rate.
pub(crate) fn claim_github_reconciliation(
    owner: String,
    repo: String,
    now_ms: i64,
) -> Result<bool> {
    submit_database_write("claim_github_reconciliation", move |connection| {
        Ok(connection.execute(
            "UPDATE github_watch_cursors SET last_reconciled_at_ms=?3
             WHERE owner=?1 AND repo=?2 AND (last_reconciled_at_ms IS NULL OR last_reconciled_at_ms <= ?4)",
            params![owner, repo, now_ms, now_ms.saturating_sub(3_600_000)],
        )? != 0)
    })
}

pub(crate) fn load_github_seen_items(owner: &str, repo: &str) -> Result<Vec<GithubSeenItem>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT number, kind, title, url, author, body, created_at, updated_at FROM github_seen_items
         WHERE owner=?1 AND repo=?2 ORDER BY created_at, number")?;
    statement
        .query_map(params![owner, repo], |row| {
            Ok(GithubSeenItem {
                number: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                url: row.get(3)?,
                author: row.get(4)?,
                body: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
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
        tx.execute("DELETE FROM github_watch_classification_failures WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4",
            params![classification.owner, classification.repo, classification.number, classification.session_id])?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(false);
        }
        if classification.created {
            tx.execute(
                "INSERT INTO github_watch_items(owner, repo, number, creator_session_id,
                        kind, title, url, created_at, timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0, ?8, 0, ?8)
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
            enqueue_github_event(
                &tx,
                &event_key,
                &classification.session_id,
                &event_json,
                wake,
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
        "SELECT w.owner, w.repo, w.number, w.creator_session_id, w.kind, w.title, w.url, w.created_at,
                w.timeline_at, w.timeline_id, w.review_comments_at, w.review_comments_id, w.processed_at, s.updated_at
         FROM github_watch_items w LEFT JOIN github_seen_items s
           ON w.owner=s.owner AND w.repo=s.repo AND w.number=s.number
         WHERE w.owner=?1 AND w.repo=?2 ORDER BY w.number",
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
                timeline_at: row.get(8)?,
                timeline_id: row.get(9)?,
                review_comments_at: row.get(10)?,
                review_comments_id: row.get(11)?,
                processed_at: row.get(12)?,
                recorded_updated_at: row.get(13)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

fn enqueue_github_event(
    tx: &rusqlite::Transaction<'_>,
    key: &str,
    target: &str,
    json: &str,
    wake: bool,
) -> Result<()> {
    tx.execute(
        "INSERT INTO mailbox_outbox(event_key, target_session_id, event_json, wake, unpark, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5) ON CONFLICT(event_key) DO NOTHING",
        params![key, target, json, wake, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub(crate) struct GithubItemProgress {
    pub timeline_at: String,
    pub timeline_id: i64,
    pub review_comments_at: String,
    pub review_comments_id: i64,
    pub processed_at: String,
}

/// Only this item's progress advances with its events. A failure cannot roll
/// back a feed page or another item's deliveries.
pub(crate) fn commit_github_timeline(
    watch: GithubItemWatch,
    progress: GithubItemProgress,
    events: Vec<(String, String, String, bool)>,
) -> Result<()> {
    let has_events = !events.is_empty();
    submit_database_write("commit_github_timeline", move |connection| {
        let tx = connection.transaction()?;
        for (key, target, json, wake) in events {
            enqueue_github_event(&tx, &key, &target, &json, wake)?;
        }
        tx.execute(
            "UPDATE github_watch_items SET timeline_at=?4, timeline_id=?5, review_comments_at=?6, review_comments_id=?7, processed_at=?8
             WHERE owner=?1 AND repo=?2 AND number=?3",
            params![
                watch.owner,
                watch.repo,
                watch.number,
                progress.timeline_at,
                progress.timeline_id,
                progress.review_comments_at,
                progress.review_comments_id,
                progress.processed_at
            ],
        )?;
        tx.commit()?;
        Ok(())
    })?;
    if has_events {
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

/// Commands of this session's tool calls that were running within a second
/// of `at_ms`. GitHub timestamps have one-second precision.
pub(crate) fn load_github_session_tool_commands_at(
    session_id: &str,
    at_ms: i64,
) -> Result<Vec<String>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT body_json FROM materialized_transcript_items
         WHERE session_id=?1 AND created_at_ms <= ?2 + 1000
           AND (last_changed_at_ms >= ?2 - 1000
                OR json_extract(body_json, '$.call.status') IN ('pending', 'in_progress'))
           AND json_extract(body_json, '$.kind') = 'tool'",
    )?;
    let bodies = statement
        .query_map(params![session_id, at_ms], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut commands = Vec::new();
    for body in bodies {
        let mj_core::transcript::TranscriptBody::Tool {
            call, presentation, ..
        } = serde_json::from_str(&body)?
        else {
            continue;
        };
        let Ok(call) = serde_json::from_value::<agent_client_protocol::schema::v1::ToolCall>(call)
        else {
            continue;
        };
        commands.push(
            mj_transcript::transcript::materialized_tool_call_presentation(
                presentation.as_deref(),
                &call,
            )
            .source,
        );
    }
    Ok(commands)
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

#[cfg(test)]
pub(crate) fn seed_github_retry_due(
    owner: &str,
    repo: &str,
    number: i64,
    session_id: &str,
) -> Result<()> {
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    let session_id = session_id.to_owned();
    submit_database_write("seed_github_retry_due", move |connection| {
        connection.execute("UPDATE github_watch_classification_failures SET next_attempt_at_ms=0 WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4",
            params![owner,repo,number,session_id])?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn seed_github_repo_start(owner: &str, repo: &str, at: &str) -> Result<()> {
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    let at = at.to_owned();
    submit_database_write("seed_github_repo_start", move |connection| {
        connection.execute(
            "UPDATE github_watch_cursors SET updated_at=?3, watch_started_at=?3,
            reconciled_through=?3, last_reconciled_at_ms=NULL WHERE owner=?1 AND repo=?2",
            params![owner, repo, at],
        )?;
        Ok(())
    })
}
