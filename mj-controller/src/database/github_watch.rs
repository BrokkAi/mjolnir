//! Durable cursors, classifications, and creator watches for GitHub polling.

use super::*;
use mj_core::mailbox::{LifecycleRecipient, MailboxEvent, MailboxEventBody};

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
    /// `None` for a watch kept for interested sessions only: a PR another
    /// account opened, or one whose creating session is gone.
    pub creator_session_id: Option<String>,
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
    /// The latest lifecycle event the timeline committed for this item.
    pub lifecycle: Option<RecordedLifecycle>,
}

/// A lifecycle event as recorded on its watch, so a session that is classified
/// after the event was committed can still be told. `body_json` is a serialized
/// `MailboxEventBody` addressed to the creator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedLifecycle {
    pub key: String,
    pub body_json: String,
    pub at: String,
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
        let mut enqueued_lifecycle = false;
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
            return Ok((false, false));
        }
        // The open notice goes first: the outbox delivers in creation order, and
        // a recorded merge must not reach the session before it hears of the PR.
        if let Some((event_key, event_json, wake)) = event {
            enqueue_github_event(
                &tx,
                &event_key,
                &classification.session_id,
                &event_json,
                wake,
            )?;
        }
        if classification.created {
            // A watch kept for interested sessions gains its creator here. What
            // the watch read while it had none was read for interested sessions
            // only, with no review comments, so the creator is owed the whole
            // timeline: the cursors go back to the item's creation. The
            // interested sessions' rows have their own keys, so nothing is
            // delivered twice.
            tx.execute(
                "INSERT INTO github_watch_items(owner, repo, number, creator_session_id,
                        kind, title, url, created_at, timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0, ?8, 0, ?8)
                 ON CONFLICT(owner, repo, number) DO UPDATE
                    SET creator_session_id = excluded.creator_session_id,
                        timeline_at = excluded.timeline_at, timeline_id = 0,
                        review_comments_at = excluded.review_comments_at, review_comments_id = 0,
                        processed_at = excluded.processed_at
                    WHERE github_watch_items.creator_session_id IS NULL",
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
        } else if classification.interested && classification.kind == "pull_request" {
            // Every PR with an interested session is watched, so its merge
            // reaches that session. The watch starts at the PR's creation, so
            // its first poll reads the whole timeline.
            tx.execute(
                "INSERT INTO github_watch_items(owner, repo, number, creator_session_id,
                        kind, title, url, created_at, timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?7, 0, ?7, 0, ?7)
                 ON CONFLICT(owner, repo, number) DO NOTHING",
                params![
                    classification.owner,
                    classification.repo,
                    classification.number,
                    classification.kind,
                    classification.title,
                    classification.url,
                    classification.created_at
                ],
            )?;
            // The timeline commit that recorded the latest lifecycle event may
            // already have run. SQLite orders the two write transactions, so
            // either that commit saw this classification and delivered the
            // event, or this one sees the recorded event and delivers it.
            let recorded = tx
                .query_row(
                    "SELECT lifecycle_event_key, lifecycle_event_json, lifecycle_at
                     FROM github_watch_items WHERE owner=?1 AND repo=?2 AND number=?3",
                    params![
                        classification.owner,
                        classification.repo,
                        classification.number
                    ],
                    |row| Ok(recorded_lifecycle(row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .flatten();
            // No own-activity filter here: sessions share one GitHub account,
            // so an overlapping turn does not show that this session made the
            // change, and a wrong skip would lose the notice for good. A
            // queued echo of its own action costs one line.
            if let Some(recorded) = recorded {
                let body: MailboxEventBody = serde_json::from_str(&recorded.body_json)
                    .context("decode the recorded GitHub lifecycle event")?;
                let key = interested_event_key(&recorded.key, &classification.session_id);
                let json = github_mailbox_event_json(
                    &key,
                    interested_lifecycle_body(body),
                    false,
                    &recorded.at,
                )?;
                enqueued_lifecycle =
                    enqueue_github_event(&tx, &key, &classification.session_id, &json, false)?;
            }
        }
        tx.commit()?;
        Ok((true, enqueued_lifecycle))
    })?;
    let (inserted, enqueued_lifecycle) = inserted;
    if inserted && (has_event || enqueued_lifecycle) {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(inserted)
}

pub(crate) fn load_github_watches(owner: &str, repo: &str) -> Result<Vec<GithubItemWatch>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT w.owner, w.repo, w.number, w.creator_session_id, w.kind, w.title, w.url, w.created_at,
                w.timeline_at, w.timeline_id, w.review_comments_at, w.review_comments_id, w.processed_at, s.updated_at,
                w.lifecycle_event_key, w.lifecycle_event_json, w.lifecycle_at
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
                lifecycle: recorded_lifecycle(row.get(14)?, row.get(15)?, row.get(16)?),
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

/// The four columns are written together; a row with a key but no body or
/// time carries no event.
fn recorded_lifecycle(
    key: Option<String>,
    body_json: Option<String>,
    at: Option<String>,
) -> Option<RecordedLifecycle> {
    Some(RecordedLifecycle {
        key: key?,
        body_json: body_json?,
        at: at?,
    })
}

/// Whether a row was stored. A key already present, and a target that is gone,
/// store nothing.
fn enqueue_github_event(
    tx: &rusqlite::Transaction<'_>,
    key: &str,
    target: &str,
    json: &str,
    wake: bool,
) -> Result<bool> {
    Ok(tx.execute(
        "INSERT INTO mailbox_outbox(event_key, target_session_id, event_json, wake, unpark, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5) ON CONFLICT(event_key) DO NOTHING",
        params![key, target, json, wake, Utc::now().to_rfc3339()],
    )? > 0)
}

/// The outbox key of an interested session's copy of a lifecycle event. It
/// differs from the creator's key, which is the primary key and would
/// otherwise make one of the two inserts a silent no-op.
fn interested_event_key(base_key: &str, session_id: &str) -> String {
    format!("{base_key}:{session_id}")
}

fn github_mailbox_event_json(
    key: &str,
    body: MailboxEventBody,
    wake: bool,
    timestamp: &str,
) -> Result<String> {
    let created_at_ms = chrono::DateTime::parse_from_rfc3339(timestamp)
        .context("invalid GitHub timeline timestamp")?
        .timestamp_millis()
        .max(0) as u64;
    Ok(serde_json::to_string(&MailboxEvent {
        key: key.to_owned(),
        source: "github".into(),
        wake,
        created_at_ms,
        body,
    })?)
}

/// A lifecycle body as an interested session receives it: the same change,
/// worded so it does not claim the PR is theirs. Other bodies pass through.
fn interested_lifecycle_body(body: MailboxEventBody) -> MailboxEventBody {
    match body {
        MailboxEventBody::GithubPullRequestLifecycle {
            change,
            number,
            title,
            actor,
            repo,
            ..
        } => MailboxEventBody::GithubPullRequestLifecycle {
            change,
            number,
            title,
            actor,
            recipient: LifecycleRecipient::Interested,
            repo,
        },
        other => other,
    }
}

/// An event mapped from a timeline read, before recipients are known.
pub(crate) struct MappedGithubEvent {
    /// The creator's outbox key, and the base of every interested session's.
    pub key: String,
    pub body: MailboxEventBody,
    /// The GitHub login that caused it, for the own-activity filter.
    pub author: Option<String>,
    pub timestamp: String,
    /// Merged, closed or reopened: also delivered to interested sessions.
    pub lifecycle: bool,
}

type TurnWindows = BTreeMap<String, Option<Vec<GithubSessionTurnWindow>>>;

/// Whether `session_id` itself caused a GitHub action: the action's author is
/// the shared credential's account, and it happened during one of the
/// session's turns. The session already knows what it did, so delivering it,
/// even without a wake, would echo it into its context. Runs inside the write
/// transaction that decides delivery. A session whose turns cannot be read gets
/// the event.
fn authored_during_session_turn(
    tx: &rusqlite::Transaction<'_>,
    windows_by_session: &mut TurnWindows,
    credential_login: Option<&str>,
    author_login: Option<&str>,
    created_at: Option<&str>,
    session_id: &str,
) -> bool {
    let Some((credential_login, author_login)) = credential_login.zip(author_login) else {
        return false;
    };
    if !credential_login.eq_ignore_ascii_case(author_login) {
        return false;
    }
    let Some(created_at_ms) = created_at
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp_millis())
    else {
        return false;
    };
    let created_at_ms = floor_to_second(created_at_ms);
    let windows = windows_by_session
        .entry(session_id.to_owned())
        .or_insert_with(|| match session_turn_windows(tx, session_id) {
            Ok(windows) => Some(windows),
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    error = %format!("{error:#}"),
                    "could not read transcript turn windows for GitHub activity; delivering it"
                );
                None
            }
        });
    windows.as_ref().is_some_and(|windows| {
        windows.iter().any(|window| {
            created_at_ms >= floor_to_second(window.started_at_ms)
                && window
                    .completed_at_ms
                    .is_none_or(|completed_at_ms| created_at_ms < ceil_to_second(completed_at_ms))
        })
    })
}

fn floor_to_second(timestamp_ms: i64) -> i64 {
    timestamp_ms.div_euclid(1_000) * 1_000
}

fn ceil_to_second(timestamp_ms: i64) -> i64 {
    let floor = floor_to_second(timestamp_ms);
    if floor == timestamp_ms {
        timestamp_ms
    } else {
        floor + 1_000
    }
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
///
/// This transaction owns the decision of who is told. It reads the item's
/// recipients and applies the own-activity filter against the same snapshot
/// that it writes, so a session classified before it runs is a recipient, and
/// one classified after it reads the lifecycle event recorded here (see
/// `commit_github_item_classification`). Neither order loses a notice.
pub(crate) fn commit_github_timeline(
    watch: GithubItemWatch,
    progress: GithubItemProgress,
    events: Vec<MappedGithubEvent>,
    credential_login: Option<String>,
) -> Result<()> {
    let enqueued = submit_database_write("commit_github_timeline", move |connection| {
        let tx = connection.transaction()?;
        let mut enqueued = false;
        // The row, not the caller's snapshot, says who the creator is now.
        let creator = tx
            .query_row(
                "SELECT creator_session_id FROM github_watch_items WHERE owner=?1 AND repo=?2 AND number=?3",
                params![watch.owner, watch.repo, watch.number],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        // No row: the last session that cared about the item is gone.
        let mut advance = false;
        if let Some(creator) = creator {
            // The poll was planned for the creator it saw. If the creator changed
            // since, whether one attached to a creator-less watch (so review
            // comments were not read) or another replaced a destroyed one (so
            // the cursors were rewound), the new creator is owed a full read:
            // leave the progress where it is and let the next poll redo it.
            advance = creator == watch.creator_session_id;
            let interested = {
                let mut statement = tx.prepare(
                    "SELECT session_id FROM github_watch_classifications
                     WHERE owner=?1 AND repo=?2 AND number=?3 AND interested=1 AND created=0
                     ORDER BY session_id",
                )?;
                statement
                    .query_map(params![watch.owner, watch.repo, watch.number], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut windows = TurnWindows::new();
            for event in &events {
                if let Some(creator) = &creator
                    && !authored_during_session_turn(
                        &tx,
                        &mut windows,
                        credential_login.as_deref(),
                        event.author.as_deref(),
                        Some(&event.timestamp),
                        creator,
                    )
                {
                    let json = github_mailbox_event_json(
                        &event.key,
                        event.body.clone(),
                        true,
                        &event.timestamp,
                    )?;
                    enqueued |= enqueue_github_event(&tx, &event.key, creator, &json, true)?;
                }
                if !event.lifecycle {
                    continue;
                }
                for session in interested.iter().filter(|s| Some(*s) != creator.as_ref()) {
                    // No own-activity filter for interested sessions; see
                    // `commit_github_item_classification`.
                    let key = interested_event_key(&event.key, session);
                    let json = github_mailbox_event_json(
                        &key,
                        interested_lifecycle_body(event.body.clone()),
                        false,
                        &event.timestamp,
                    )?;
                    enqueued |= enqueue_github_event(&tx, &key, session, &json, false)?;
                }
            }
            // The latest lifecycle event, delivered or filtered, is what a
            // session classified later is told.
            let latest = events
                .iter()
                .filter(|event| event.lifecycle)
                .filter_map(|event| {
                    chrono::DateTime::parse_from_rfc3339(&event.timestamp)
                        .ok()
                        .map(|at| (at, event))
                })
                .max_by_key(|(at, _)| *at);
            if let Some((_, event)) = latest {
                tx.execute(
                    "UPDATE github_watch_items
                     SET lifecycle_event_key=?4, lifecycle_event_json=?5, lifecycle_at=?6
                     WHERE owner=?1 AND repo=?2 AND number=?3
                       AND (lifecycle_at IS NULL OR julianday(lifecycle_at) <= julianday(?6))",
                    params![
                        watch.owner,
                        watch.repo,
                        watch.number,
                        event.key,
                        serde_json::to_string(&event.body)?,
                        event.timestamp
                    ],
                )?;
            }
        }
        if advance {
            tx.execute(
                "UPDATE github_watch_items SET timeline_at=?4, timeline_id=?5, review_comments_at=?6, review_comments_id=?7, processed_at=?8
                 WHERE owner=?1 AND repo=?2 AND number=?3
                   AND timeline_at=?9 AND timeline_id=?10 AND review_comments_at=?11
                   AND review_comments_id=?12 AND processed_at=?13",
                params![
                    watch.owner,
                    watch.repo,
                    watch.number,
                    progress.timeline_at,
                    progress.timeline_id,
                    progress.review_comments_at,
                    progress.review_comments_id,
                    progress.processed_at,
                    watch.timeline_at,
                    watch.timeline_id,
                    watch.review_comments_at,
                    watch.review_comments_id,
                    watch.processed_at
                ],
            )?;
        }
        tx.commit()?;
        Ok(enqueued)
    })?;
    if enqueued {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn load_github_session_turn_windows_from(
    path: &Path,
    session_id: &str,
) -> Result<Vec<GithubSessionTurnWindow>> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    let windows = session_turn_windows(&tx, session_id)?;
    tx.commit()?;
    Ok(windows)
}

/// Reconstruct turn intervals from the same persisted start and completion
/// facts exposed by the transcript projection. An unmatched start counts only
/// when the materialized projection still names it as the active turn.
/// Turns the harness starts on its own have neither fact; see
/// [`harness_turn_windows`].
fn session_turn_windows(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> Result<Vec<GithubSessionTurnWindow>> {
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
    let mut prompt_turn_starts = BTreeSet::new();
    let mut windows = Vec::new();
    for body in bodies {
        let event: mj_core::storage::ApiEventData = serde_json::from_str(&body)?;
        match event {
            mj_core::storage::ApiEventData::TurnStarted { turn } => {
                prompt_turn_starts.insert(turn.turn_start_position as i64);
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
    let (running, active_turn_json): (bool, Option<String>) = tx
        .query_row(
            "SELECT execution_state='running', active_turn_json
             FROM materialized_sessions WHERE session_id=?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    let active_turn = active_turn_json
        .map(|json| serde_json::from_str::<Option<MaterializedTurn>>(&json))
        .transpose()?
        .flatten();
    let harness_turn_running = running && active_turn.is_none();
    if let Some(active_turn) = active_turn {
        let started_at_ms = starts
            .remove(&active_turn.command_id)
            .unwrap_or(active_turn.started_at_ms);
        windows.push(GithubSessionTurnWindow {
            started_at_ms,
            completed_at_ms: None,
        });
    }
    windows.extend(harness_turn_windows(
        tx,
        session_id,
        &prompt_turn_starts,
        harness_turn_running,
    )?);
    Ok(windows)
}

/// A turn the harness starts on its own, such as work a finished background
/// command woke, records a turn completion but no prompt turn start. Its "Agent continued on its own"
/// marker starts it. The agent's last message, thought or tool call before
/// the next turn or restart ends it. A running session with no active prompt
/// is still in the last such turn.
fn harness_turn_windows(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    prompt_turn_starts: &BTreeSet<i64>,
    harness_turn_running: bool,
) -> Result<Vec<GithubSessionTurnWindow>> {
    let markers = {
        let mut statement = tx.prepare(
            "SELECT position, stable_id GLOB ?2, created_at_ms
             FROM materialized_transcript_items
             WHERE session_id=?1 AND (stable_id GLOB ?2 OR stable_id GLOB ?3)
             ORDER BY position",
        )?;
        statement
            .query_map(
                params![
                    session_id,
                    format!("{}*", mj_core::transcript::HARNESS_TURN_ITEM_PREFIX),
                    format!("{}*", mj_core::transcript::SESSION_RESTART_ITEM_PREFIX),
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?, row.get(2)?)),
            )?
            .collect::<rusqlite::Result<Vec<(i64, bool, i64)>>>()?
    };
    let boundaries = markers
        .iter()
        .map(|(position, ..)| *position)
        .chain(prompt_turn_starts.iter().copied())
        .collect::<BTreeSet<_>>();
    let mut last_work = tx.prepare(
        "SELECT MAX(last_changed_at_ms) FROM materialized_transcript_items
         WHERE session_id=?1 AND position > ?2 AND position < ?3
           AND (stable_id GLOB 'agent:*' OR stable_id GLOB 'thought:*'
                OR stable_id GLOB 'tool:*')",
    )?;
    let mut windows = Vec::new();
    for (position, starts_turn, started_at_ms) in markers {
        // A prompt with no text starts its turn with the same marker.
        if !starts_turn || prompt_turn_starts.contains(&position) {
            continue;
        }
        let next = boundaries.range(position + 1..).next().copied();
        let completed_at_ms = if next.is_none() && harness_turn_running {
            None
        } else {
            let last_changed_at_ms: Option<i64> = last_work.query_row(
                params![session_id, position, next.unwrap_or(i64::MAX)],
                |row| row.get(0),
            )?;
            // A turn that produced nothing could not have posted anything.
            let Some(last_changed_at_ms) = last_changed_at_ms else {
                continue;
            };
            Some(last_changed_at_ms)
        };
        windows.push(GithubSessionTurnWindow {
            started_at_ms,
            completed_at_ms,
        });
    }
    Ok(windows)
}

/// Output of this session's tool calls that were running within a second of
/// `at_ms`; GitHub timestamps have one-second precision. Nothing can print a
/// new item's URL before it exists, so this ignores how the command was
/// composed. Later turn text is not evidence: a session that merely mentions
/// another session's new item afterwards would otherwise claim it.
pub(crate) fn load_github_session_creation_output(
    session_id: &str,
    at_ms: i64,
) -> Result<Vec<String>> {
    load_github_session_creation_output_from(&database_path(), session_id, at_ms)
}

fn load_github_session_creation_output_from(
    path: &std::path::Path,
    session_id: &str,
    at_ms: i64,
) -> Result<Vec<String>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT stable_id, body_json FROM materialized_transcript_items
         WHERE session_id=?1 AND created_at_ms <= ?2 + 1000
           AND (last_changed_at_ms >= ?2 - 1000
                OR json_extract(body_json, '$.call.status') IN ('pending', 'in_progress'))
           AND json_extract(body_json, '$.kind') = 'tool'",
    )?;
    let bodies = statement
        .query_map(params![session_id, at_ms], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = bodies
        .into_iter()
        .map(|(stable_id, body)| {
            Ok(std::sync::Arc::new(mj_core::transcript::TranscriptItem {
                stable_id,
                position: 0,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body: serde_json::from_str(&body)?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    super::materialized::hydrate_terminal_outputs(&connection, session_id, &mut items)?;
    let outputs = mj_core::transcript::TerminalOutputs::of(items.iter().map(AsRef::as_ref));
    let mut output = Vec::new();
    for item in &items {
        let mj_core::transcript::TranscriptBody::Tool { call, .. } = &item.body else {
            continue;
        };
        let call: agent_client_protocol::v1::schema::ToolCall = call.decode()?;
        output.extend(mj_transcript::transcript::tool_content_details(
            &call.content,
            &outputs.for_tool(&item.stable_id),
            call.raw_output.as_ref(),
            mj_transcript::transcript::tool_status(&call.status),
        ));
        // MCP tools can supply structured output without a text block.
        if let Some(raw) = call.raw_output {
            output.push(raw.to_string());
        }
    }
    Ok(output)
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

/// One read-only snapshot shared by all pairs for a session in a poll. Raw JSON
/// preserves persisted tool-call field order for the frozen Python renderer.
#[allow(dead_code)] // Wired by the relevance-policy integration milestone.
pub(crate) fn load_github_history_snapshot(
    session_id: &str,
) -> Result<Vec<mj_transcript::github_history::HistoryRow>> {
    load_github_history_snapshot_from(&database_path(), session_id)
}

fn load_github_history_snapshot_from(
    path: &std::path::Path,
    session_id: &str,
) -> Result<Vec<mj_transcript::github_history::HistoryRow>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare("SELECT stable_id, position, created_at_ms, last_changed_at_ms, body_json FROM materialized_transcript_items WHERE session_id=?1 ORDER BY position, stable_id")?;
    let rows = statement
        .query_map(params![session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(
            |(stable_id, position, created_at_ms, last_changed_at_ms, body)| {
                Ok(mj_transcript::github_history::HistoryRow {
                    stable_id,
                    position,
                    created_at_ms,
                    last_changed_at_ms,
                    body: serde_json::from_str(&body)?,
                })
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields,
    };
    use mj_core::relay::{RelayEvent, RelayObservation, relay_event_digest};
    use mj_transcript::projection::{apply_committed_projection_event, project_relay_event};

    fn agent_message(text: &str, message_id: &str) -> RelayObservation {
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                    .message_id(message_id),
            )),
        }
    }

    #[test]
    fn creation_output_comes_only_from_calls_running_at_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creation.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        let connection = open_reader(&path).unwrap();
        let tool = |id: &str, output: &str| serde_json::json!({"kind":"tool", "call":{"toolCallId":id, "title":"script", "status":"completed", "content":[]}, "terminal_outputs":[{"terminal_id":id, "output":output, "exit_code":0}]});
        // This fixture uses SQL to exercise the persisted transcript format.
        for (position, started, changed, body) in [
            (1, 1000, 2000, tool("before", "early")),
            (
                2,
                3000,
                6000,
                tool("create", "https://github.com/owner/repo/issues/12"),
            ),
            // A later view or mention of the new item in the same turn would
            // let a session that never opened it claim it.
            (3, 7000, 7000, tool("view", "viewed later")),
            (
                4,
                7000,
                7000,
                serde_json::json!({"kind":"agent", "chunks":[{"content":{"type":"text", "text":"mentioned later"}}], "streaming":false}),
            ),
        ] {
            connection.execute("INSERT INTO materialized_transcript_items(session_id, stable_id, position, created_at_ms, last_changed_at_ms, body_json) VALUES ('session-1', ?1, ?2, ?3, ?4, ?5)", params![position.to_string(), position, started, changed, body.to_string()]).unwrap();
        }
        let output = load_github_session_creation_output_from(&path, "session-1", 5000)
            .unwrap()
            .join("\n");
        assert!(output.contains("https://github.com/owner/repo/issues/12"));
        assert!(!output.contains("early"));
        assert!(!output.contains("later"));
    }

    // Hard-won: b4476dc76: comments a session posted in turns the harness started on its own still came back to it.
    #[test]
    fn turns_the_harness_starts_end_with_its_last_work_before_the_next_turn_or_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("harness-turns.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        let observations = [
            (
                1_000,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: 1_000,
                },
            ),
            (
                1_500,
                RelayObservation::SessionUpdate {
                    update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                        "comment",
                        "gh issue comment",
                    ))),
                },
            ),
            (
                4_000,
                RelayObservation::SessionUpdate {
                    update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        "comment",
                        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                    ))),
                },
            ),
            (5_000, agent_message("Posted.", "reply")),
            (
                5_200,
                RelayObservation::HarnessTurnSettled {
                    autonomous: None,
                    origin: None,
                    prompt_in_flight: false,
                },
            ),
            // A notice while idle is not the agent's work.
            (
                60_000,
                RelayObservation::Notice {
                    message: "Delivered 1 mailbox event".into(),
                },
            ),
            (
                100_000,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: 100_000,
                },
            ),
            (100_500, agent_message("Working.", "interrupted")),
            (200_000, RelayObservation::SessionRestarted),
            // A message after the restart does not stretch the interrupted turn.
            (300_000, agent_message("Resumed.", "resume-notice")),
            (
                400_000,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: 400_000,
                },
            ),
            (400_500, agent_message("Still working.", "running")),
        ];
        let mut current = load_materialized_session_from(&path, "session-1")
            .unwrap()
            .unwrap();
        apply_projection_page_to(&path, "session-1", |page| {
            for (recorded_at_ms, observation) in observations {
                let mut event = RelayEvent {
                    format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
                    ordinal: current.applied_event_ordinal + 1,
                    previous_digest: current.applied_event_digest.clone(),
                    digest: String::new(),
                    recorded_at_ms,
                    command_id: None,
                    observation,
                };
                event.digest = relay_event_digest(&event)?;
                let mutation = project_relay_event(&current, &event)?.mutation;
                page.apply(
                    event.ordinal,
                    &event.previous_digest,
                    &event.digest,
                    &mutation,
                )?;
                apply_committed_projection_event(&mut current, &event, mutation)?;
            }
            Ok(())
        })
        .unwrap();
        let window = |started_at_ms, completed_at_ms| GithubSessionTurnWindow {
            started_at_ms,
            completed_at_ms,
        };
        assert_eq!(
            load_github_session_turn_windows_from(&path, "session-1").unwrap(),
            vec![
                window(1_000, Some(5_200)),
                window(100_000, Some(200_000)),
                window(400_000, None),
            ]
        );
    }
}

#[cfg(test)]
mod history_snapshot_tests {
    use super::*;
    // Hard-won: #1505: rows edited after item creation must never leak into historical evidence.
    #[test]
    fn github_history_snapshot_retains_cutoff_metadata_and_raw_call_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        let connection = open_reader(&path).unwrap();
        for (id, position, created, changed, body) in [
            (
                "z",
                1,
                1000,
                1000,
                r#"{"kind":"tool","call":{"title":"first","kind":"execute","toolCallId":"c"}}"#,
            ),
            (
                "a",
                1,
                1000,
                1000,
                r#"{"kind":"user","content":[{"type":"text","text":"before\r\ncutoff"}]}"#,
            ),
            (
                "edited",
                2,
                1000,
                3000,
                r#"{"kind":"agent","chunks":[{"content":{"type":"text","text":"future secret"}}]}"#,
            ),
            ("thought", 3, 1000, 3000, r#"{"kind":"thought"}"#),
            ("later", 4, 3000, 3000, r#"{"kind":"user","content":[]}"#),
            (
                "notice",
                5,
                1000,
                1000,
                r#"{"kind":"message","event":{"body":{"type":"new_github_item","number":42,"repo":"OWNER/REPO"}}}"#,
            ),
        ] {
            connection.execute("INSERT INTO materialized_transcript_items(session_id,stable_id,position,created_at_ms,last_changed_at_ms,body_json) VALUES ('session-1',?1,?2,?3,?4,?5)",params![id,position,created,changed,body]).unwrap();
        }
        let rows = load_github_history_snapshot_from(&path, "session-1").unwrap();
        let rendered =
            mj_transcript::github_history::render("session-1", &rows, 2000, "owner/repo", 42)
                .unwrap();
        assert_eq!(
            rendered,
            "Session transcript as of 1970-01-01T00:00:02.000Z (UTC)\nSession id: session-1\nTranscript rows included: 2; withheld because changed after cutoff: 2; excluded because created later: 1; item-specific GitHub mailbox notices removed: 1.\nRows come only from Mjolnir materialized transcript records; timestamps are row creation times.\n\n[1970-01-01T00:00:01.000Z] USER\nbefore\ncutoff\n\n[1970-01-01T00:00:01.000Z] TOOL EXECUTE\nTITLE: first\nCALL: {\"title\":\"first\",\"kind\":\"execute\",\"toolCallId\":\"c\"}\n"
        );
    }
}
