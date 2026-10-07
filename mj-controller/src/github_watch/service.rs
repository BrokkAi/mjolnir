use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream::{self, StreamExt};
use mj_core::config::GithubWatchConfig;
use mj_core::github_item::{
    GithubItem, GithubItemEvidence, GithubItemKind, GithubItemVerdict, SessionContext,
};
use mj_core::mailbox::MailboxEvent;
use mj_core::repository::RepositoryIdentity;
use mj_core::state::SessionRecord;
use serde_json::Value;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::daemon::RuntimeState;

const MAX_ITEMS_PER_POLL: usize = 50;
const ITEMS_PER_PAGE: usize = 100;
const CLASSIFICATION_CONCURRENCY: usize = 4;
const REPOSITORY_CONCURRENCY: usize = 4;
const COMMENT_BODY_LIMIT: usize = 8 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_INTERVAL: Duration = Duration::from_secs(10);
const MAX_INTERVAL: Duration = Duration::from_secs(60 * 60);

type ClassifyFuture<'a> = Pin<Box<dyn Future<Output = Result<GithubItemVerdict>> + Send + 'a>>;

trait GithubClassifier: Send + Sync {
    fn classify<'a>(&'a self, evidence: &'a GithubItemEvidence) -> ClassifyFuture<'a>;
}

struct JevGithubClassifier;

impl GithubClassifier for JevGithubClassifier {
    fn classify<'a>(&'a self, evidence: &'a GithubItemEvidence) -> ClassifyFuture<'a> {
        Box::pin(crate::github_item_verdict::classify(evidence))
    }
}

/// Cancellable periodic poller. Poll progress lives only in the database; a
/// replacement daemon can resume from the same repo cursors and event keys.
pub(crate) async fn run(state: Arc<RuntimeState>, stop: CancellationToken) -> Result<()> {
    run_with_classifier(state, stop, Arc::new(JevGithubClassifier)).await
}

async fn run_with_classifier(
    state: Arc<RuntimeState>,
    stop: CancellationToken,
    classifier: Arc<dyn GithubClassifier>,
) -> Result<()> {
    loop {
        let projection = state.controller_projection();
        let config = projection.config.github.watch;
        let delay = Duration::from_secs(config.interval_seconds).clamp(MIN_INTERVAL, MAX_INTERVAL);
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        if stop.is_cancelled() {
            return Ok(());
        }
        let projection = state.controller_projection();
        let config = projection.config.github.watch;
        if !config.enabled {
            continue;
        }
        let sessions_by_repo = sessions_by_github_repo(&projection.state);
        let watched_repositories =
            tokio::task::spawn_blocking(crate::database::load_github_watched_repositories)
                .await
                .context("load watched GitHub repositories")??;
        let mut repositories = sessions_by_repo;
        for key in watched_repositories {
            repositories.entry(key).or_default();
        }
        let mut jobs = JoinSet::new();
        let mut repositories = repositories.into_iter();
        loop {
            while jobs.len() < REPOSITORY_CONCURRENCY {
                let Some(((owner, repo), sessions)) = repositories.next() else {
                    break;
                };
                let config = config.clone();
                let classifier = classifier.clone();
                let stop = stop.clone();
                jobs.spawn(async move {
                    let result =
                        poll_repository(&owner, &repo, sessions, config, classifier, stop).await;
                    (owner, repo, result)
                });
            }
            if jobs.is_empty() {
                break;
            }
            tokio::select! {
                _ = stop.cancelled() => {
                    jobs.abort_all();
                    while let Some(result) = jobs.join_next().await {
                        if let Err(error) = result && !error.is_cancelled() {
                            tracing::warn!(%error, "GitHub watch task failed during shutdown");
                        }
                    }
                    return Ok(());
                }
                result = jobs.join_next() => {
                    match result.context("GitHub watch task disappeared")? {
                        Ok((owner, repo, Ok(()))) => tracing::debug!(%owner, %repo, "GitHub repository poll completed"),
                        Ok((owner, repo, Err(error))) => tracing::warn!(%owner, %repo, error = %format!("{error:#}"), "GitHub repository poll failed; durable cursors remain available for retry"),
                        Err(error) => tracing::warn!(%error, "GitHub repository poll task panicked"),
                    }
                }
            }
        }
    }
}

fn sessions_by_github_repo(
    state: &mj_core::state::State,
) -> BTreeMap<(String, String), Vec<SessionRecord>> {
    let mut sessions_by_repo = BTreeMap::<(String, String), Vec<SessionRecord>>::new();
    for session in state.sessions.values() {
        if !session.state.has_live_worker() || state.is_subagent_session(&session.id) {
            continue;
        }
        let Some(project) = &session.project else {
            continue;
        };
        let mut repositories = BTreeSet::new();
        for identity in project.identities.values() {
            if let RepositoryIdentity::Github(owner, repo) = identity {
                repositories.insert((owner.to_ascii_lowercase(), repo.to_ascii_lowercase()));
            }
        }
        for repository in repositories {
            sessions_by_repo
                .entry(repository)
                .or_default()
                .push(session.clone());
        }
    }
    sessions_by_repo
}

async fn poll_repository(
    owner: &str,
    repo: &str,
    sessions: Vec<SessionRecord>,
    config: GithubWatchConfig,
    classifier: Arc<dyn GithubClassifier>,
    stop: CancellationToken,
) -> Result<()> {
    if stop.is_cancelled() {
        return Ok(());
    }
    let watches = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_watches(&owner, &repo)
    })
    .await?;
    if sessions.is_empty() && watches.is_empty() {
        return Ok(());
    }
    let token_session = sessions
        .first()
        .map(|session| session.id.as_str())
        .or_else(|| {
            watches
                .first()
                .map(|watch| watch.creator_session_id.as_str())
        })
        .context("GitHub repository has neither a live session nor a creator watch")?;
    let controller = tokio::task::spawn_blocking(crate::controller::Controller::load)
        .await
        .context("load controller for GitHub watch credentials")??;
    let token = controller
        .github_token_for_session(token_session)
        .await
        .context("resolve GitHub watch credential")?;
    let api = GithubApi::new(&config.api_base, token)?;
    poll_repository_with_api(owner, repo, sessions, classifier, &api, stop).await
}

async fn poll_repository_with_api(
    owner: &str,
    repo: &str,
    sessions: Vec<SessionRecord>,
    classifier: Arc<dyn GithubClassifier>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    let watches = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_watches(&owner, &repo)
    })
    .await?;
    if sessions.is_empty() && watches.is_empty() {
        return Ok(());
    }
    poll_repository_items(owner, repo, &sessions, classifier, api, stop.clone()).await?;
    let watches = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_watches(&owner, &repo)
    })
    .await?;
    poll_repository_comments(owner, repo, watches, api, stop).await
}

async fn poll_repository_items(
    owner: &str,
    repo: &str,
    sessions: &[SessionRecord],
    classifier: Arc<dyn GithubClassifier>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    let old_cursor = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_repo_cursor(&owner, &repo)
    })
    .await?;
    let first_sight = old_cursor
        .as_ref()
        .is_none_or(|cursor| cursor.items_watermark_at.is_none());
    let cursor = old_cursor.unwrap_or_default();
    let watermark_id = cursor.items_watermark_id.map(|id| id.max(0) as u64);
    let response = api
        .items(
            owner,
            repo,
            cursor.items_watermark_at.as_deref(),
            watermark_id,
            cursor.items_etag.as_deref(),
        )
        .await?;
    if first_sight {
        let max = response
            .items
            .iter()
            .filter_map(parse_github_item)
            .max_by(|a, b| compare_watermark(&a.created_at, a.id, &b.created_at, b.id));
        let watermark_at = max
            .as_ref()
            .map(|item| item.created_at.clone())
            .unwrap_or_else(now_rfc3339);
        let watermark_id = max.map(|item| item.id.min(i64::MAX as u64) as i64);
        let new_cursor = crate::database::GithubRepoCursor {
            items_watermark_at: Some(watermark_at),
            items_watermark_id: watermark_id,
            items_etag: response.etag,
            comments_cursor: Some(now_rfc3339()),
        };
        blocking_db({
            let owner = owner.to_owned();
            let repo = repo.to_owned();
            move || crate::database::save_github_repo_cursor(&owner, &repo, new_cursor)
        })
        .await?;
        return Ok(());
    }
    if response.not_modified {
        return Ok(());
    }

    let watermark_at = cursor.items_watermark_at.as_deref();
    let watermark_id = cursor.items_watermark_id.unwrap_or_default().max(0) as u64;
    let mut items = response
        .items
        .iter()
        .filter_map(parse_github_item)
        .filter(|item| {
            watermark_at.is_none_or(|at| {
                compare_watermark(&item.created_at, item.id, at, watermark_id)
                    == std::cmp::Ordering::Greater
            })
        })
        .collect::<Vec<_>>();
    // The feed is newest-first. Process oldest-first so a bounded poll can
    // advance only through a contiguous, fully classified prefix.
    items.sort_by(|a, b| compare_watermark(&a.created_at, a.id, &b.created_at, b.id));
    let mut classified_items = 0usize;
    let mut skipped_items = 0usize;
    let mut last_completed = None;
    for (index, item) in items.iter().enumerate() {
        if stop.is_cancelled() {
            return Ok(());
        }
        let classified = blocking_db({
            let owner = owner.to_owned();
            let repo = repo.to_owned();
            let number = item.number as i64;
            move || crate::database::load_github_item_classified_sessions(&owner, &repo, number)
        })
        .await?;
        let pending = sessions
            .iter()
            .filter(|session| !classified.contains(&session.id))
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            last_completed = Some(item);
            continue;
        }
        if classified_items >= MAX_ITEMS_PER_POLL {
            skipped_items = items.len() - index;
            break;
        }
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        let item = item.clone();
        let classifier = classifier.clone();
        stream::iter(pending)
            .map(|session| {
                let owner = owner.clone();
                let repo = repo.clone();
                let item = item.clone();
                let classifier = classifier.clone();
                async move { classify_for_session(&owner, &repo, item, session, classifier).await }
            })
            .buffer_unordered(CLASSIFICATION_CONCURRENCY)
            .collect::<Vec<Result<()>>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        classified_items += 1;
        last_completed = Some(&items[index]);
    }
    let partial = skipped_items > 0;
    if partial {
        tracing::warn!(%owner, %repo, cap = MAX_ITEMS_PER_POLL, skipped = skipped_items, "GitHub item poll reached its per-repository classification cap; remaining items will be retried");
    }
    let max = items
        .iter()
        .max_by(|a, b| compare_watermark(&a.created_at, a.id, &b.created_at, b.id));
    let next_cursor = crate::database::GithubRepoCursor {
        items_watermark_at: if partial {
            last_completed
                .map(|item| item.created_at.clone())
                .or(cursor.items_watermark_at.clone())
        } else {
            max.map(|item| item.created_at.clone())
                .or(cursor.items_watermark_at.clone())
        },
        items_watermark_id: if partial {
            last_completed
                .map(|item| item.id.min(i64::MAX as u64) as i64)
                .or(cursor.items_watermark_id)
        } else {
            max.map(|item| item.id.min(i64::MAX as u64) as i64)
                .or(cursor.items_watermark_id)
        },
        // A capped batch cannot reuse its ETag: fetch the remaining items
        // again after advancing through this fully classified prefix.
        items_etag: if partial { None } else { response.etag },
        comments_cursor: cursor.comments_cursor,
    };
    blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::save_github_repo_cursor(&owner, &repo, next_cursor)
    })
    .await?;
    Ok(())
}

async fn classify_for_session(
    owner: &str,
    repo: &str,
    item: GithubApiItem,
    session: SessionRecord,
    classifier: Arc<dyn GithubClassifier>,
) -> Result<()> {
    let repo_label = format!("{owner}/{repo}");
    let item_label = item.as_str();
    let kind_label = item.kind_label();
    let recent_turns = recent_turns(session.id.clone()).await?;
    let evidence = GithubItemEvidence {
        item: GithubItem {
            repo: repo_label.clone(),
            kind: item.kind,
            number: item.number,
            title: item.title.clone(),
            body: item.body,
            author: item.author,
            url: item.url.clone(),
        },
        session: SessionContext { recent_turns },
    };
    let verdict = classifier.classify(&evidence).await?;
    let event = if verdict.interested && !verdict.created {
        let key = format!(
            "github:{repo_label}#{}:interest:{}",
            item.number, session.id
        );
        let text = format!(
            "{repo_label} {item_label} #{}: {} ({})",
            item.number, item.title, item.url
        );
        let event = MailboxEvent {
            key: key.clone(),
            source: "github".into(),
            wake: false,
            text,
            created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
        };
        Some((key, serde_json::to_string(&event)?, false))
    } else {
        None
    };
    let classification = crate::database::GithubItemClassification {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: item.number as i64,
        session_id: session.id,
        kind: kind_label.to_owned(),
        title: item.title,
        url: item.url,
        created_at: item.created_at,
        interested: verdict.interested,
        created: verdict.created,
    };
    blocking_db(move || {
        crate::database::commit_github_item_classification(classification, event).map(|_| ())
    })
    .await
}

async fn recent_turns(session_id: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let Some(materialized) = crate::database::load_materialized_session(&session_id)? else {
            return Ok(String::new());
        };
        let canonical =
            mj_transcript::projection::canonical_session_from_materialized(&materialized)?;
        Ok(crate::compaction::render_recent_turns(&canonical, 3))
    })
    .await
    .context("load session turns for GitHub classification")?
}

async fn poll_repository_comments(
    owner: &str,
    repo: &str,
    watches: Vec<crate::database::GithubItemWatch>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    if watches.is_empty() || stop.is_cancelled() {
        return Ok(());
    }
    let cursor = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_repo_cursor(&owner, &repo)
    })
    .await?
    .unwrap_or_default();
    let Some(since) = cursor.comments_cursor else {
        let mut initialized = cursor;
        initialized.comments_cursor = Some(now_rfc3339());
        return blocking_db({
            let owner = owner.to_owned();
            let repo = repo.to_owned();
            move || crate::database::save_github_repo_cursor(&owner, &repo, initialized)
        })
        .await;
    };
    let mut comments = api
        .comment_pages(owner, repo, "issues/comments", &since)
        .await?;
    comments.extend(
        api.comment_pages(owner, repo, "pulls/comments", &since)
            .await?,
    );
    let mut watches_by_number = BTreeMap::<u64, Vec<crate::database::GithubItemWatch>>::new();
    for watch in watches {
        watches_by_number
            .entry(watch.number as u64)
            .or_default()
            .push(watch);
    }
    let mut outbox = Vec::new();
    let mut newest = None::<String>;
    for comment in comments {
        if let Some(timestamp) = comment_timestamp(&comment)
            && newest
                .as_ref()
                .is_none_or(|current| timestamp > current.as_str())
        {
            newest = Some(timestamp.to_owned());
        }
        let Some(number) = comment_item_number(&comment) else {
            continue;
        };
        let Some(item_watches) = watches_by_number.get(&number) else {
            continue;
        };
        let Some(id) = comment["id"].as_u64() else {
            tracing::warn!(%owner, %repo, number, "ignoring GitHub comment with no numeric id");
            continue;
        };
        let login = comment["user"]["login"].as_str().unwrap_or("unknown");
        let body = comment["body"].as_str().unwrap_or_default();
        let body = truncate_utf8(body, COMMENT_BODY_LIMIT);
        let kind = if comment.get("pull_request_url").is_some() {
            "review-comment"
        } else {
            "comment"
        };
        let key = format!("github:{owner}/{repo}#{number}:{kind}:{id}");
        for watch in item_watches {
            let text = format!(
                "GitHub comment by {login} on #{number} {} ({}):\n{}",
                watch.title, watch.url, body
            );
            outbox.push(mailbox_outbox_row(
                key.clone(),
                watch.creator_session_id.clone(),
                text,
                true,
                comment_timestamp_ms(&comment),
            )?);
        }
    }

    let watched_pull_requests = watches_by_number
        .values()
        .flat_map(|watches| watches.iter())
        .filter(|watch| watch.kind == "pull_request")
        .cloned()
        .collect::<Vec<_>>();
    for watch in watched_pull_requests {
        if stop.is_cancelled() {
            return Ok(());
        }
        let pull: Value = api
            .get_json(
                &format!("repos/{owner}/{repo}/pulls/{}", watch.number),
                &[],
                None,
            )
            .await?;
        if pull["state"].as_str() != Some("open") {
            continue;
        }
        for review in api
            .array_pages(
                &format!("repos/{owner}/{repo}/pulls/{}/reviews", watch.number),
                &[],
            )
            .await?
        {
            let Some(timestamp) = review["submitted_at"].as_str() else {
                continue;
            };
            if timestamp < since.as_str() {
                continue;
            }
            let Some(id) = review["id"].as_u64() else {
                continue;
            };
            let login = review["user"]["login"].as_str().unwrap_or("unknown");
            let body = review["body"].as_str().unwrap_or_default();
            let state = review["state"].as_str().unwrap_or("reviewed");
            let summary = if body.trim().is_empty() {
                format!("review state: {state}")
            } else {
                format!("{state}: {}", truncate_utf8(body, COMMENT_BODY_LIMIT))
            };
            let key = format!("github:{owner}/{repo}#{}:review:{id}", watch.number);
            let text = format!(
                "GitHub review by {login} on #{} {} ({}):\n{}",
                watch.number, watch.title, watch.url, summary
            );
            outbox.push(mailbox_outbox_row(
                key,
                watch.creator_session_id.clone(),
                text,
                true,
                comment_timestamp_ms(&review),
            )?);
            if newest
                .as_ref()
                .is_none_or(|current| timestamp > current.as_str())
            {
                newest = Some(timestamp.to_owned());
            }
        }
    }
    let comments_cursor = newest.unwrap_or(since);
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    blocking_db(move || {
        crate::database::commit_github_comment_events(&owner, &repo, comments_cursor, outbox)
    })
    .await
}

fn mailbox_outbox_row(
    key: String,
    target_session_id: String,
    text: String,
    wake: bool,
    created_at_ms: u64,
) -> Result<(String, String, String, bool)> {
    let event = MailboxEvent {
        key: key.clone(),
        source: "github".into(),
        wake,
        text,
        created_at_ms,
    };
    Ok((key, target_session_id, serde_json::to_string(&event)?, wake))
}

#[derive(Debug, Clone)]
struct GithubApiItem {
    id: u64,
    number: u64,
    kind: GithubItemKind,
    title: String,
    body: String,
    author: String,
    url: String,
    created_at: String,
}

impl GithubApiItem {
    fn as_str(&self) -> &'static str {
        match self.kind {
            GithubItemKind::Issue => "issue",
            GithubItemKind::PullRequest => "pull request",
        }
    }

    fn kind_label(&self) -> &'static str {
        match self.kind {
            GithubItemKind::Issue => "issue",
            GithubItemKind::PullRequest => "pull_request",
        }
    }
}

fn parse_github_item(value: &Value) -> Option<GithubApiItem> {
    Some(GithubApiItem {
        id: value["id"].as_u64()?,
        number: value["number"].as_u64()?,
        kind: if value.get("pull_request").is_some() {
            GithubItemKind::PullRequest
        } else {
            GithubItemKind::Issue
        },
        title: value["title"].as_str()?.to_owned(),
        body: value["body"].as_str().unwrap_or_default().to_owned(),
        author: value["user"]["login"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
        url: value["html_url"].as_str()?.to_owned(),
        created_at: value["created_at"].as_str()?.to_owned(),
    })
}

fn compare_watermark(a_at: &str, a_id: u64, b_at: &str, b_id: u64) -> std::cmp::Ordering {
    match (
        DateTime::parse_from_rfc3339(a_at),
        DateTime::parse_from_rfc3339(b_at),
    ) {
        (Ok(a), Ok(b)) => a.cmp(&b).then_with(|| a_id.cmp(&b_id)),
        _ => a_at.cmp(b_at).then_with(|| a_id.cmp(&b_id)),
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn comment_timestamp(comment: &Value) -> Option<&str> {
    comment["updated_at"]
        .as_str()
        .or_else(|| comment["submitted_at"].as_str())
        .or_else(|| comment["created_at"].as_str())
}

fn comment_timestamp_ms(comment: &Value) -> u64 {
    comment_timestamp(comment)
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp_millis().max(0) as u64)
        .unwrap_or_else(|| mj_core::clock::epoch_millis().max(0) as u64)
}

fn comment_item_number(comment: &Value) -> Option<u64> {
    let url = comment["issue_url"]
        .as_str()
        .or_else(|| comment["pull_request_url"].as_str())?;
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

async fn blocking_db<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("GitHub watch database task failed")?
}

struct GithubPage {
    items: Vec<Value>,
    etag: Option<String>,
    not_modified: bool,
}

struct GithubApi {
    base: String,
    client: reqwest::Client,
    rate_limit_until: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl GithubApi {
    fn new(base: &str, token: Option<String>) -> Result<Self> {
        let parsed = url::Url::parse(base).context("parse GitHub API base URL")?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
            "GitHub API base URL must be an HTTP(S) URL"
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(API_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("mjolnir/", env!("CARGO_PKG_VERSION")));
        if let Some(token) = token {
            builder = builder.default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
                );
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                        .context("invalid GitHub credential header")?,
                );
                headers
            });
        }
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            client: builder.build()?,
            rate_limit_until: tokio::sync::Mutex::new(None),
        })
    }

    async fn items(
        &self,
        owner: &str,
        repo: &str,
        watermark_at: Option<&str>,
        watermark_id: Option<u64>,
        etag: Option<&str>,
    ) -> Result<GithubPage> {
        let mut items = Vec::new();
        let mut next_etag = None;
        // Keep the URL stable so its ETag remains a valid validator while the
        // client applies the creation watermark below.
        for page in 1.. {
            let query = vec![
                ("state", "all".to_owned()),
                ("sort", "created".to_owned()),
                ("direction", "desc".to_owned()),
                ("per_page", ITEMS_PER_PAGE.to_string()),
                ("page", page.to_string()),
            ];
            let mut request = self
                .client
                .get(format!("{}/repos/{owner}/{repo}/issues", self.base))
                .query(&query);
            if page == 1
                && let Some(etag) = etag
            {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            let response = self.get(request).await?;
            if response.not_modified {
                return Ok(GithubPage {
                    items: Vec::new(),
                    etag: etag.map(str::to_owned),
                    not_modified: true,
                });
            }
            if page == 1 {
                next_etag = response.etag;
            }
            let page_items = response
                .value
                .as_array()
                .context("GitHub issues response is not an array")?;
            let count = page_items.len();
            let crossed_watermark = match (watermark_at, watermark_id) {
                (Some(watermark_at), Some(watermark_id)) => {
                    page_items.iter().filter_map(parse_github_item).all(|item| {
                        compare_watermark(&item.created_at, item.id, watermark_at, watermark_id)
                            != std::cmp::Ordering::Greater
                    })
                }
                _ => false,
            };
            items.extend(page_items.iter().cloned());
            if watermark_at.is_none() || crossed_watermark || count < ITEMS_PER_PAGE {
                return Ok(GithubPage {
                    items,
                    etag: next_etag,
                    not_modified: false,
                });
            }
        }
        unreachable!("GitHub issue pagination always returns or fails")
    }

    async fn comment_pages(
        &self,
        owner: &str,
        repo: &str,
        endpoint: &str,
        since: &str,
    ) -> Result<Vec<Value>> {
        self.array_pages(
            &format!("repos/{owner}/{repo}/{endpoint}"),
            &[("since", since.to_owned())],
        )
        .await
    }

    async fn array_pages(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        for page in 1.. {
            let mut page_query = query.to_vec();
            page_query.push(("per_page", ITEMS_PER_PAGE.to_string()));
            page_query.push(("page", page.to_string()));
            let response = self
                .get(
                    self.client
                        .get(format!("{}/{path}", self.base))
                        .query(&page_query),
                )
                .await?;
            ensure!(
                !response.not_modified,
                "unexpected 304 response for paged GitHub request"
            );
            let page_values = response
                .value
                .as_array()
                .context("GitHub response is not an array")?;
            let count = page_values.len();
            values.extend(page_values.iter().cloned());
            if count < ITEMS_PER_PAGE {
                break;
            }
        }
        Ok(values)
    }

    async fn get_json(
        &self,
        path: &str,
        query: &[(&str, String)],
        etag: Option<&str>,
    ) -> Result<Value> {
        let mut request = self
            .client
            .get(format!("{}/{path}", self.base))
            .query(query);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = self.get(request).await?;
        ensure!(
            !response.not_modified,
            "unexpected 304 for GitHub resource {path}"
        );
        Ok(response.value)
    }

    async fn get(&self, request: reqwest::RequestBuilder) -> Result<GithubResponse> {
        self.wait_for_rate_limit().await?;
        let response = request.send().await.context("request GitHub REST API")?;
        self.record_rate_limit(&response).await;
        let status = response.status();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(GithubResponse {
                value: Value::Null,
                etag,
                not_modified: true,
            });
        }
        if !status.is_success() {
            bail!("GitHub REST API returned {status}");
        }
        let response = response
            .error_for_status()
            .context("GitHub REST API request failed")?;
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= 8 * 1024 * 1024),
            "GitHub REST API response is larger than 8 MiB"
        );
        let bytes = response
            .bytes()
            .await
            .context("read GitHub REST API response")?;
        ensure!(
            bytes.len() <= 8 * 1024 * 1024,
            "GitHub REST API response is larger than 8 MiB"
        );
        Ok(GithubResponse {
            value: serde_json::from_slice(&bytes).context("decode GitHub REST API response")?,
            etag,
            not_modified: false,
        })
    }

    async fn wait_for_rate_limit(&self) -> Result<()> {
        let deadline = *self.rate_limit_until.lock().await;
        if let Some(deadline) = deadline {
            tokio::time::sleep_until(deadline).await;
            *self.rate_limit_until.lock().await = None;
        }
        Ok(())
    }

    async fn record_rate_limit(&self, response: &reqwest::Response) {
        let headers = response.headers();
        let reset = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
        let retry_after = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| Utc::now() + chrono::Duration::seconds(seconds as i64));
        let has_retry_after = retry_after.is_some();
        let exhausted = headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "0");
        if exhausted
            || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || has_retry_after
        {
            let retry_at = retry_after
                .or(reset)
                .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(1));
            let delay = (retry_at - Utc::now()).to_std().unwrap_or_default();
            let next = tokio::time::Instant::now() + delay;
            *self.rate_limit_until.lock().await = Some(next);
            tracing::warn!(
                retry_seconds = delay.as_secs(),
                "GitHub API rate limit reached; polling will back off"
            );
        }
    }
}

struct GithubResponse {
    value: Value,
    etag: Option<String>,
    not_modified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHILD: &str = "MJ_GITHUB_WATCH_INTEGRATION_CHILD";

    #[derive(Clone)]
    struct FakeGithub {
        data: Arc<tokio::sync::Mutex<FakeGithubData>>,
        not_modified: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct FakeGithubData {
        items: Vec<Value>,
        comments: Vec<Value>,
        etag: String,
    }

    struct FakeClassifier {
        calls: Arc<AtomicUsize>,
    }

    impl GithubClassifier for FakeClassifier {
        fn classify<'a>(&'a self, evidence: &'a GithubItemEvidence) -> ClassifyFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let title = evidence.item.title.to_ascii_lowercase();
                Ok(GithubItemVerdict {
                    interested: title.contains("interesting"),
                    created: title.contains("created"),
                })
            })
        }
    }

    fn run_isolated_child(test: &str) -> bool {
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let root = tempfile::tempdir().unwrap();
        crate::controller::test_support::IsolatedTest::new(
            crate::controller::test_support::test_name(module_path!(), test),
        )
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        false
    }

    #[tokio::test]
    async fn watcher_persists_interest_creator_watch_comments_and_restart_cursor() {
        if !run_isolated_child(
            "watcher_persists_interest_creator_watch_comments_and_restart_cursor",
        ) {
            return;
        }
        let writer = crate::database::install_isolated_test_writer();
        let now = Utc::now();
        let old = now - chrono::Duration::minutes(5);
        let later = now + chrono::Duration::seconds(52);
        let creator_id = "creator-session";
        let mut session = crate::database::test_session(creator_id, "project");
        session.state = mj_core::state::SessionState::Running;
        session.project = Some(mj_core::repository::ProjectBundleSnapshot {
            bundle: mj_core::config::ProjectBundle {
                primary_repo: "main".into(),
                repositories: vec![mj_core::config::ProjectRepository {
                    id: "main".into(),
                    github: Some("acme/repo".into()),
                    local: None,
                    destination: ".".into(),
                    git_ref: None,
                }],
            },
            identities: BTreeMap::from([(
                "main".into(),
                RepositoryIdentity::Github("acme".into(), "repo".into()),
            )]),
            network_sources: BTreeMap::new(),
        });
        crate::database::save_session(&session).unwrap();
        let mut projection = mj_core::state::State::default();
        projection
            .sessions
            .insert(session.id.clone(), session.clone());
        assert!(sessions_by_github_repo(&projection).contains_key(&("acme".into(), "repo".into())));

        let fake = FakeGithub {
            data: Arc::new(tokio::sync::Mutex::new(FakeGithubData {
                items: vec![github_item(
                    1,
                    1,
                    "old existing issue",
                    old.to_rfc3339(),
                    false,
                )],
                comments: Vec::new(),
                etag: "\"v1\"".into(),
            })),
            not_modified: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/repos/{owner}/{repo}/issues", get(fake_issues))
            .route("/repos/{owner}/{repo}/issues/comments", get(fake_comments))
            .route("/repos/{owner}/{repo}/pulls/comments", get(fake_comments))
            .route("/repos/{owner}/{repo}/pulls/{number}", get(fake_pull))
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/reviews",
                get(fake_reviews),
            )
            .with_state(fake.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let calls = Arc::new(AtomicUsize::new(0));
        let classifier: Arc<dyn GithubClassifier> = Arc::new(FakeClassifier {
            calls: calls.clone(),
        });
        let stop = CancellationToken::new();
        let api = GithubApi::new(&base, None).unwrap();

        poll_repository_with_api(
            "acme",
            "repo",
            vec![session.clone()],
            classifier.clone(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the first poll only establishes the watermark"
        );
        assert!(
            crate::database::pending_mailbox_events(20)
                .unwrap()
                .is_empty()
        );
        let cursor = crate::database::load_github_repo_cursor("acme", "repo")
            .unwrap()
            .unwrap();
        assert_eq!(
            cursor.items_watermark_at.as_deref(),
            Some(old.to_rfc3339().as_str())
        );

        {
            let mut data = fake.data.lock().await;
            data.items = (2..=52)
                .map(|number| {
                    let title = match number {
                        2 => "interesting issue",
                        52 => "created pull request",
                        _ => "ordinary issue",
                    };
                    github_item(
                        number,
                        number,
                        title,
                        (now + chrono::Duration::seconds(number as i64)).to_rfc3339(),
                        number == 52,
                    )
                })
                .chain(std::iter::once(github_item(
                    1,
                    1,
                    "old existing issue",
                    old.to_rfc3339(),
                    false,
                )))
                .collect();
            data.etag = "\"v2\"".into();
        }
        poll_repository_with_api(
            "acme",
            "repo",
            vec![session.clone()],
            classifier.clone(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 50);
        assert!(
            crate::database::load_github_watches("acme", "repo")
                .unwrap()
                .is_empty()
        );
        let pending = crate::database::pending_mailbox_events(20).unwrap();
        let interest = pending
            .iter()
            .find(|entry| entry.event_key == "github:acme/repo#2:interest:creator-session")
            .expect("interested session receives a non-waking note");
        let interest_event: MailboxEvent = serde_json::from_str(&interest.event_json).unwrap();
        assert!(!interest_event.wake);
        assert!(interest_event.text.contains("interesting issue"));
        let retry_command_id = crate::mailbox_outbox::mailbox_command_id(&interest.event_key);
        assert!(
            !crate::database::enqueue_mailbox_event(
                &interest.event_key,
                &interest.target_session_id,
                &interest.event_json,
                false,
                false,
            )
            .unwrap()
        );

        writer.shutdown().unwrap();
        let writer = crate::database::install_isolated_test_writer();
        assert_eq!(
            retry_command_id,
            crate::mailbox_outbox::mailbox_command_id(&interest.event_key),
            "an unacknowledged row retains its stable relay identity after restart"
        );
        let restarted_api = GithubApi::new(&base, None).unwrap();
        poll_repository_with_api(
            "acme",
            "repo",
            vec![session.clone()],
            classifier.clone(),
            &restarted_api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 51);
        let watches = crate::database::load_github_watches("acme", "repo").unwrap();
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].number, 52);
        assert_eq!(watches[0].creator_session_id, creator_id);
        {
            let mut data = fake.data.lock().await;
            data.comments = vec![json!({
                "id": 9001,
                "issue_url": "http://api/repos/acme/repo/issues/52",
                "user": {"login": "reviewer"},
                "body": "Please update the changelog.",
                "created_at": (later + chrono::Duration::seconds(1)).to_rfc3339(),
                "updated_at": (later + chrono::Duration::seconds(1)).to_rfc3339()
            })];
        }
        let restarted_api = GithubApi::new(&base, None).unwrap();
        poll_repository_with_api(
            "acme",
            "repo",
            vec![session],
            classifier,
            &restarted_api,
            stop,
        )
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            51,
            "persisted classifications are not repeated"
        );
        assert_eq!(
            fake.not_modified.load(Ordering::SeqCst),
            1,
            "the stored ETag handles 304"
        );
        let pending = crate::database::pending_mailbox_events(20).unwrap();
        let comment = pending
            .iter()
            .find(|entry| entry.event_key == "github:acme/repo#52:comment:9001")
            .expect("watched creator receives later comments");
        let comment_event: MailboxEvent = serde_json::from_str(&comment.event_json).unwrap();
        assert_eq!(comment.target_session_id, creator_id);
        assert!(comment_event.wake);
        assert!(comment_event.text.contains("reviewer"));
        assert!(comment_event.text.contains("Please update the changelog."));
        writer.shutdown().unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    fn github_item(
        id: u64,
        number: u64,
        title: &str,
        created_at: String,
        pull_request: bool,
    ) -> Value {
        let mut item = json!({
            "id": id,
            "number": number,
            "title": title,
            "body": "item body",
            "user": {"login": "author"},
            "html_url": format!("https://github.com/acme/repo/issues/{number}"),
            "created_at": created_at
        });
        if pull_request {
            item["pull_request"] = json!({"url": "https://api.github.com/repos/acme/repo/pulls/3"});
            item["html_url"] = json!(format!("https://github.com/acme/repo/pull/{number}"));
        }
        item
    }

    async fn fake_issues(
        State(fake): State<FakeGithub>,
        headers: HeaderMap,
        Query(query): Query<BTreeMap<String, String>>,
    ) -> Response {
        assert_eq!(query.get("state").map(String::as_str), Some("all"));
        assert_eq!(query.get("sort").map(String::as_str), Some("created"));
        assert_eq!(query.get("direction").map(String::as_str), Some("desc"));
        assert!(!query.contains_key("since"));
        let data = fake.data.lock().await;
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            == Some(data.etag.as_str())
        {
            fake.not_modified.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::NOT_MODIFIED,
                [(header::ETAG, data.etag.clone())],
            )
                .into_response();
        }
        (
            StatusCode::OK,
            [(header::ETAG, data.etag.clone())],
            Json(data.items.clone()),
        )
            .into_response()
    }

    async fn fake_comments(State(fake): State<FakeGithub>) -> Json<Vec<Value>> {
        Json(fake.data.lock().await.comments.clone())
    }

    async fn fake_pull() -> Json<Value> {
        Json(json!({"state": "open"}))
    }

    async fn fake_reviews() -> Json<Vec<Value>> {
        Json(Vec::new())
    }
}
