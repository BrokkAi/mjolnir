use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream::{self, StreamExt};
use mj_core::config::GithubWatchConfig;
use mj_core::github_item::{
    GithubItem, GithubItemEvidence, GithubItemKind, GithubItemVerdict, SessionContext,
};
use mj_core::mailbox::{
    MAILBOX_COMMENT_BODY_LIMIT, MailboxEvent, MailboxEventBody, MailboxPullRequestChange,
};
use mj_core::repository::RepositoryIdentity;
use mj_core::state::SessionRecord;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::daemon::RuntimeState;

const MAX_ITEMS_PER_POLL: usize = 50;
const ITEMS_PER_PAGE: usize = 100;
const CLASSIFICATION_CONCURRENCY: usize = 4;
const REPOSITORY_CONCURRENCY: usize = 4;
const API_TIMEOUT: Duration = Duration::from_secs(20);
const CONFIG_RECHECK_INTERVAL: Duration = Duration::from_secs(5);
const MIN_INTERVAL: Duration = Duration::from_secs(10);
const MAX_INTERVAL: Duration = Duration::from_secs(60 * 60);

type ClassifyFuture<'a> = Pin<Box<dyn Future<Output = Result<GithubItemVerdict>> + Send + 'a>>;

#[derive(Clone)]
struct PollControl(Arc<dyn Fn() -> bool + Send + Sync>);

#[derive(Clone)]
struct RepositoryPollContext {
    control: PollControl,
    multi_repo_sessions: Arc<BTreeSet<String>>,
}

impl PollControl {
    fn new(enabled: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(enabled))
    }

    #[cfg(test)]
    fn always_enabled() -> Self {
        Self::new(|| true)
    }

    fn check_enabled(&self) -> Result<()> {
        if (self.0)() {
            Ok(())
        } else {
            Err(anyhow::Error::new(MailboxesDisabled))
        }
    }
}

#[derive(Debug)]
struct MailboxesDisabled;

impl std::fmt::Display for MailboxesDisabled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("agent mailboxes disabled during GitHub poll")
    }
}

impl std::error::Error for MailboxesDisabled {}

fn is_mailboxes_disabled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<MailboxesDisabled>().is_some()
}

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
        let mailboxes_enabled = projection.config.agent_mailboxes_enabled();
        let config = projection.config.github.watch;
        let delay = if mailboxes_enabled {
            Duration::from_secs(config.interval_seconds).clamp(MIN_INTERVAL, MAX_INTERVAL)
        } else {
            CONFIG_RECHECK_INTERVAL
        };
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        if stop.is_cancelled() {
            return Ok(());
        }
        let projection = state.controller_projection();
        if !projection.config.agent_mailboxes_enabled() {
            continue;
        }
        let config = projection.config.github.watch;
        let sessions_by_repo = sessions_by_github_repo(&projection.state);
        let multi_repo_sessions = Arc::new(multi_repo_session_ids(&projection.state));
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
                let multi_repo_sessions = multi_repo_sessions.clone();
                let poll_state = state.clone();
                jobs.spawn(async move {
                    let control = PollControl::new(move || {
                        poll_state
                            .controller_projection()
                            .config
                            .agent_mailboxes_enabled()
                    });
                    let result = poll_repository(
                        &owner,
                        &repo,
                        sessions,
                        config,
                        classifier,
                        stop,
                        RepositoryPollContext {
                            control,
                            multi_repo_sessions,
                        },
                    )
                    .await;
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
                        Ok((owner, repo, Err(error))) if is_mailboxes_disabled(&error) => tracing::debug!(%owner, %repo, "GitHub repository poll stopped because agent mailboxes were disabled"),
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

fn multi_repo_session_ids(state: &mj_core::state::State) -> BTreeSet<String> {
    state
        .sessions
        .values()
        .filter(|session| session_github_repo_count(session) > 1)
        .map(|session| session.id.clone())
        .collect()
}

fn session_github_repo_count(session: &SessionRecord) -> usize {
    session
        .project
        .iter()
        .flat_map(|project| project.identities.values())
        .filter_map(|identity| match identity {
            RepositoryIdentity::Github(owner, repo) => {
                Some((owner.to_ascii_lowercase(), repo.to_ascii_lowercase()))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .len()
}

async fn poll_repository(
    owner: &str,
    repo: &str,
    sessions: Vec<SessionRecord>,
    config: GithubWatchConfig,
    classifier: Arc<dyn GithubClassifier>,
    stop: CancellationToken,
    context: RepositoryPollContext,
) -> Result<()> {
    let RepositoryPollContext {
        control,
        multi_repo_sessions,
    } = context;
    if stop.is_cancelled() {
        return Ok(());
    }
    control.check_enabled()?;
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
    let api = GithubApi::new_with_control(&config.api_base, token, control)?;
    poll_repository_with_context(
        owner,
        repo,
        sessions,
        classifier,
        &multi_repo_sessions,
        &api,
        stop,
    )
    .await
}

#[cfg(test)]
async fn poll_repository_with_api(
    owner: &str,
    repo: &str,
    sessions: Vec<SessionRecord>,
    classifier: Arc<dyn GithubClassifier>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    let multi_repo_sessions = sessions
        .iter()
        .filter(|session| session_github_repo_count(session) > 1)
        .map(|session| session.id.clone())
        .collect::<BTreeSet<_>>();
    poll_repository_with_context(
        owner,
        repo,
        sessions,
        classifier,
        &multi_repo_sessions,
        api,
        stop,
    )
    .await
}

async fn poll_repository_with_context(
    owner: &str,
    repo: &str,
    sessions: Vec<SessionRecord>,
    classifier: Arc<dyn GithubClassifier>,
    multi_repo_sessions: &BTreeSet<String>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    api.control.check_enabled()?;
    let watches = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_watches(&owner, &repo)
    })
    .await?;
    if sessions.is_empty() && watches.is_empty() {
        return Ok(());
    }
    let display_repos_by_session = sessions
        .iter()
        .filter(|session| session_github_repo_count(session) > 1)
        .filter_map(|session| {
            session_github_repo_full_name(session, owner, repo)
                .map(|full_name| (session.id.clone(), full_name))
        })
        .collect::<BTreeMap<_, _>>();
    poll_repository_items(owner, repo, &sessions, classifier, api, stop.clone()).await?;
    let watches = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_watches(&owner, &repo)
    })
    .await?;
    poll_repository_comments(
        owner,
        repo,
        watches,
        &display_repos_by_session,
        multi_repo_sessions,
        api,
        stop,
    )
    .await
}

async fn poll_repository_items(
    owner: &str,
    repo: &str,
    sessions: &[SessionRecord],
    classifier: Arc<dyn GithubClassifier>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    api.control.check_enabled()?;
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
    api.control.check_enabled()?;
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
    let mut last_completed: Option<&GithubApiItem> = None;
    for (index, item) in items.iter().enumerate() {
        if stop.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = api.control.check_enabled() {
            if is_mailboxes_disabled(&error) {
                save_item_progress(owner, repo, &cursor, None, last_completed.cloned(), false)
                    .await?;
                return Ok(());
            }
            return Err(error);
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
        let item_owner = owner.to_owned();
        let item_repo = repo.to_owned();
        let item = item.clone();
        let classifier = classifier.clone();
        let control = api.control.clone();
        let classification_results = stream::iter(pending)
            .map(|session| {
                let owner = item_owner.clone();
                let repo = item_repo.clone();
                let item = item.clone();
                let classifier = classifier.clone();
                let control = control.clone();
                async move {
                    classify_for_session(&owner, &repo, item, session, classifier, control).await
                }
            })
            .buffer_unordered(CLASSIFICATION_CONCURRENCY)
            .collect::<Vec<Result<()>>>()
            .await;
        let classification_result = classification_results
            .into_iter()
            .collect::<Result<Vec<_>>>();
        if let Err(error) = classification_result {
            if is_mailboxes_disabled(&error) {
                save_item_progress(owner, repo, &cursor, None, last_completed.cloned(), false)
                    .await?;
                return Ok(());
            }
            return Err(error);
        }
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
    if let Err(error) = api.control.check_enabled() {
        if is_mailboxes_disabled(&error) {
            save_item_progress(owner, repo, &cursor, None, last_completed.cloned(), false).await?;
            return Ok(());
        }
        return Err(error);
    }
    save_item_progress(
        owner,
        repo,
        &cursor,
        response.etag,
        if partial {
            last_completed.cloned()
        } else {
            max.cloned()
        },
        !partial,
    )
    .await?;
    Ok(())
}

async fn classify_for_session(
    owner: &str,
    repo: &str,
    item: GithubApiItem,
    session: SessionRecord,
    classifier: Arc<dyn GithubClassifier>,
    control: PollControl,
) -> Result<()> {
    let repo_label = format!("{owner}/{repo}");
    let kind_label = item.kind_label();
    let display_repo = (session_github_repo_count(&session) > 1).then(|| {
        session_github_repo_full_name(&session, owner, repo)
            .unwrap_or_else(|| item.repo_full_name.clone())
    });
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
    control.check_enabled()?;
    let verdict = classifier.classify(&evidence).await?;
    control.check_enabled()?;
    let event = if verdict.interested && !verdict.created {
        let key = format!(
            "github:{repo_label}#{}:interest:{}",
            item.number, session.id
        );
        let event = MailboxEvent {
            key: key.clone(),
            source: "github".into(),
            wake: false,
            created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
            body: MailboxEventBody::NewGithubItem {
                kind: item.kind,
                number: item.number,
                title: item.title.clone(),
                repo: display_repo,
            },
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

async fn save_item_progress(
    owner: &str,
    repo: &str,
    previous: &crate::database::GithubRepoCursor,
    etag: Option<String>,
    watermark: Option<GithubApiItem>,
    completed_all: bool,
) -> Result<()> {
    let next_cursor = crate::database::GithubRepoCursor {
        items_watermark_at: watermark
            .as_ref()
            .map(|item| item.created_at.clone())
            .or_else(|| previous.items_watermark_at.clone()),
        items_watermark_id: watermark
            .map(|item| item.id.min(i64::MAX as u64) as i64)
            .or(previous.items_watermark_id),
        // Partial progress must fetch again; only a complete feed can reuse its ETag.
        items_etag: completed_all.then_some(etag).flatten(),
        comments_cursor: previous.comments_cursor.clone(),
    };
    blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::save_github_repo_cursor(&owner, &repo, next_cursor)
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
    display_repos_by_session: &BTreeMap<String, String>,
    multi_repo_sessions: &BTreeSet<String>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    if watches.is_empty() || stop.is_cancelled() {
        return Ok(());
    }
    api.control.check_enabled()?;
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
        api.control.check_enabled()?;
        return blocking_db({
            let owner = owner.to_owned();
            let repo = repo.to_owned();
            move || crate::database::save_github_repo_cursor(&owner, &repo, initialized)
        })
        .await;
    };
    let overlapping_since = overlap_timestamp(&since);
    let mut comments = api
        .comment_pages(owner, repo, "issues/comments", &overlapping_since)
        .await?;
    comments.extend(
        api.comment_pages(owner, repo, "pulls/comments", &overlapping_since)
            .await?,
    );
    let credential_login = api.credential_login().await?;
    let mut watches_by_number = BTreeMap::<u64, Vec<crate::database::GithubItemWatch>>::new();
    for watch in watches {
        watches_by_number
            .entry(watch.number as u64)
            .or_default()
            .push(watch);
    }
    let mut turn_windows_by_session = BTreeMap::new();
    let mut outbox = Vec::new();
    let mut newest = None::<String>;
    for comment in comments {
        api.control.check_enabled()?;
        if let Some(timestamp) = comment_timestamp(&comment)
            && newest
                .as_ref()
                .is_none_or(|current| timestamp_precedes(current, timestamp))
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
        let body = truncate_utf8(body, MAILBOX_COMMENT_BODY_LIMIT).to_owned();
        let is_review_comment = comment.get("pull_request_url").is_some();
        let kind = if is_review_comment {
            "review-comment"
        } else {
            "comment"
        };
        let key = format!("github:{owner}/{repo}#{number}:{kind}:{id}");
        for watch in item_watches {
            let suppress_wake = authored_during_session_turn(
                credential_login.as_deref(),
                comment["user"]["login"].as_str(),
                comment_creation_timestamp(&comment),
                &watch.creator_session_id,
                &mut turn_windows_by_session,
            )
            .await;
            let item_kind = item_kind_from_watch(&watch.kind);
            let display_repo =
                display_repo_for_watch(watch, display_repos_by_session, multi_repo_sessions);
            let event_body = if is_review_comment {
                MailboxEventBody::GithubReviewComment {
                    item_kind,
                    number,
                    title: watch.title.clone(),
                    author: login.to_owned(),
                    body: body.clone(),
                    repo: display_repo,
                }
            } else {
                MailboxEventBody::GithubComment {
                    item_kind,
                    number,
                    title: watch.title.clone(),
                    author: login.to_owned(),
                    body: body.clone(),
                    repo: display_repo,
                }
            };
            outbox.push(mailbox_outbox_row(
                key.clone(),
                watch.creator_session_id.clone(),
                event_body,
                !suppress_wake,
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
        api.control.check_enabled()?;
        let response = api
            .pull_request(
                owner,
                repo,
                watch.number as u64,
                watch.pull_request_etag.as_deref(),
            )
            .await?;
        api.control.check_enabled()?;
        let state = if response.not_modified {
            watch
                .pull_request_state
                .as_deref()
                .context("GitHub returned 304 before a pull request state was stored")?
                .to_owned()
        } else {
            let state = response.value["state"]
                .as_str()
                .context("GitHub pull request response has no state")?
                .to_owned();
            ensure!(
                state == "open" || state == "closed",
                "GitHub pull request response has invalid state {state:?}"
            );
            let reopened_actor =
                if watch.pull_request_state.as_deref() == Some("closed") && state == "open" {
                    api.latest_reopened_actor(owner, repo, watch.number as u64)
                        .await?
                } else {
                    None
                };
            let event = pull_request_lifecycle_event(
                owner,
                repo,
                &watch,
                &response.value,
                watch.pull_request_state.as_deref(),
                reopened_actor.as_deref(),
                display_repo_for_watch(&watch, display_repos_by_session, multi_repo_sessions),
            )?;
            blocking_db({
                let watch = watch.clone();
                let state = state.clone();
                let etag = response.etag.clone();
                move || {
                    crate::database::commit_github_pull_request_status(watch, etag, state, event)
                }
            })
            .await?;
            state
        };
        if state != "open" {
            continue;
        }
        for review in api
            .array_pages(
                &format!("repos/{owner}/{repo}/pulls/{}/reviews", watch.number),
                &[],
            )
            .await?
        {
            api.control.check_enabled()?;
            let Some(timestamp) = review["submitted_at"].as_str() else {
                continue;
            };
            if timestamp_precedes(timestamp, &overlapping_since) {
                continue;
            }
            let Some(id) = review["id"].as_u64() else {
                continue;
            };
            let login = review["user"]["login"].as_str().unwrap_or("unknown");
            let body = review["body"].as_str().unwrap_or_default();
            let review_state = review["state"].as_str().unwrap_or("reviewed");
            let body = truncate_utf8(body, MAILBOX_COMMENT_BODY_LIMIT).to_owned();
            let key = format!("github:{owner}/{repo}#{}:review:{id}", watch.number);
            let suppress_wake = authored_during_session_turn(
                credential_login.as_deref(),
                review["user"]["login"].as_str(),
                comment_creation_timestamp(&review),
                &watch.creator_session_id,
                &mut turn_windows_by_session,
            )
            .await;
            let display_repo =
                display_repo_for_watch(&watch, display_repos_by_session, multi_repo_sessions);
            outbox.push(mailbox_outbox_row(
                key,
                watch.creator_session_id.clone(),
                MailboxEventBody::GithubReview {
                    item_kind: GithubItemKind::PullRequest,
                    number: watch.number as u64,
                    title: watch.title.clone(),
                    author: login.to_owned(),
                    body,
                    review_state: Some(review_state.to_owned()),
                    repo: display_repo,
                },
                !suppress_wake,
                comment_timestamp_ms(&review),
            )?);
            if newest
                .as_ref()
                .is_none_or(|current| timestamp_precedes(current, timestamp))
            {
                newest = Some(timestamp.to_owned());
            }
        }
    }
    let comments_cursor = newest
        .filter(|newest| timestamp_precedes(&since, newest))
        .unwrap_or(since);
    api.control.check_enabled()?;
    let owner = owner.to_owned();
    let repo = repo.to_owned();
    blocking_db(move || {
        crate::database::commit_github_comment_events(&owner, &repo, comments_cursor, outbox)
    })
    .await
}

async fn authored_during_session_turn(
    credential_login: Option<&str>,
    author_login: Option<&str>,
    created_at: Option<&str>,
    session_id: &str,
    turn_windows_by_session: &mut BTreeMap<
        String,
        Option<Vec<crate::database::GithubSessionTurnWindow>>,
    >,
) -> bool {
    let Some((credential_login, author_login)) = credential_login.zip(author_login) else {
        return false;
    };
    if !credential_login.eq_ignore_ascii_case(author_login) {
        return false;
    }
    let Some(created_at_ms) = created_at.and_then(timestamp_millis) else {
        return false;
    };
    let created_at_ms = floor_to_second(created_at_ms);
    if !turn_windows_by_session.contains_key(session_id) {
        let owned_session_id = session_id.to_owned();
        let loaded = blocking_db(move || {
            crate::database::load_github_session_turn_windows(&owned_session_id)
        })
        .await;
        let windows = match loaded {
            Ok(windows) => Some(windows),
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    error = %format!("{error:#}"),
                    "could not read transcript turn windows for GitHub comment or review; preserving wake"
                );
                None
            }
        };
        turn_windows_by_session.insert(session_id.to_owned(), windows);
    }
    turn_windows_by_session
        .get(session_id)
        .and_then(Option::as_ref)
        .is_some_and(|windows| {
            windows.iter().any(|window| {
                created_at_ms >= floor_to_second(window.started_at_ms)
                    && window.completed_at_ms.is_none_or(|completed_at_ms| {
                        created_at_ms < ceil_to_second(completed_at_ms)
                    })
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

fn pull_request_lifecycle_event(
    owner: &str,
    repo: &str,
    watch: &crate::database::GithubItemWatch,
    pull: &Value,
    previous_state: Option<&str>,
    reopened_actor: Option<&str>,
    display_repo: Option<String>,
) -> Result<Option<(String, String)>> {
    let repo_label = format!("{owner}/{repo}");
    let number = watch.number;
    let (key, change, actor, timestamp) = if let Some(merged_at) = pull["merged_at"].as_str() {
        let login = pull["merged_by"]["login"].as_str().unwrap_or("unknown");
        (
            format!("github:{repo_label}#{number}:merged"),
            MailboxPullRequestChange::Merged,
            login,
            merged_at,
        )
    } else if pull["state"].as_str() == Some("closed") {
        let closed_at = pull["closed_at"]
            .as_str()
            .context("closed GitHub pull request response has no closing timestamp")?;
        let login = pull["closed_by"]["login"].as_str().unwrap_or("unknown");
        (
            format!("github:{repo_label}#{number}:closed:{closed_at}"),
            MailboxPullRequestChange::ClosedWithoutMerging,
            login,
            closed_at,
        )
    } else if previous_state == Some("closed") && pull["state"].as_str() == Some("open") {
        let reopened_at = pull["updated_at"]
            .as_str()
            .context("reopened GitHub pull request response has no update timestamp")?;
        let login = reopened_actor.unwrap_or("unknown");
        (
            format!("github:{repo_label}#{number}:reopened:{reopened_at}"),
            MailboxPullRequestChange::Reopened,
            login,
            reopened_at,
        )
    } else {
        return Ok(None);
    };
    let event = MailboxEvent {
        key: key.clone(),
        source: "github".into(),
        wake: true,
        created_at_ms: timestamp_millis(timestamp)
            .unwrap_or_else(mj_core::clock::epoch_millis)
            .max(0) as u64,
        body: MailboxEventBody::GithubPullRequestLifecycle {
            change,
            number: number as u64,
            title: watch.title.clone(),
            actor: actor.to_owned(),
            repo: display_repo,
        },
    };
    Ok(Some((key, serde_json::to_string(&event)?)))
}

fn mailbox_outbox_row(
    key: String,
    target_session_id: String,
    body: MailboxEventBody,
    wake: bool,
    created_at_ms: u64,
) -> Result<(String, String, String, bool)> {
    let event = MailboxEvent {
        key: key.clone(),
        source: "github".into(),
        wake,
        created_at_ms,
        body,
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
    repo_full_name: String,
    created_at: String,
}

impl GithubApiItem {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            GithubItemKind::Issue => "issue",
            GithubItemKind::PullRequest => "pull_request",
        }
    }
}

fn parse_github_item(value: &Value) -> Option<GithubApiItem> {
    let url = value["html_url"].as_str()?.to_owned();
    let repo_full_name = value["repository"]["full_name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| github_repo_full_name_from_url(&url))?;
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
        url,
        repo_full_name,
        created_at: value["created_at"].as_str()?.to_owned(),
    })
}

fn github_repo_full_name_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let mut segments = parsed.path_segments()?;
    let owner = segments.next()?;
    let repo = segments.next()?;
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

fn session_github_repo_full_name(
    session: &SessionRecord,
    owner: &str,
    repo: &str,
) -> Option<String> {
    session
        .project
        .iter()
        .flat_map(|project| project.identities.values())
        .find_map(|identity| match identity {
            RepositoryIdentity::Github(identity_owner, identity_repo)
                if identity_owner.eq_ignore_ascii_case(owner)
                    && identity_repo.eq_ignore_ascii_case(repo) =>
            {
                Some(format!("{identity_owner}/{identity_repo}"))
            }
            _ => None,
        })
}

fn display_repo_for_watch(
    watch: &crate::database::GithubItemWatch,
    display_repos_by_session: &BTreeMap<String, String>,
    multi_repo_sessions: &BTreeSet<String>,
) -> Option<String> {
    display_repos_by_session
        .get(&watch.creator_session_id)
        .cloned()
        .or_else(|| {
            multi_repo_sessions
                .contains(&watch.creator_session_id)
                .then(|| github_repo_full_name_from_url(&watch.url))
                .flatten()
        })
}

fn item_kind_from_watch(kind: &str) -> GithubItemKind {
    match kind {
        "pull_request" => GithubItemKind::PullRequest,
        _ => GithubItemKind::Issue,
    }
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

fn comment_creation_timestamp(comment: &Value) -> Option<&str> {
    comment["created_at"]
        .as_str()
        .or_else(|| comment["submitted_at"].as_str())
        .or_else(|| comment["updated_at"].as_str())
}

fn timestamp_millis(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn timestamp_precedes(timestamp: &str, boundary: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(timestamp),
        DateTime::parse_from_rfc3339(boundary),
    ) {
        (Ok(timestamp), Ok(boundary)) => timestamp < boundary,
        _ => timestamp < boundary,
    }
}

fn overlap_timestamp(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|timestamp| {
            (timestamp - chrono::Duration::seconds(5)).to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| timestamp.to_owned())
}

fn comment_timestamp_ms(comment: &Value) -> u64 {
    comment_timestamp(comment)
        .and_then(timestamp_millis)
        .map(|timestamp| timestamp.max(0) as u64)
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

#[derive(Default)]
struct GithubCredentialState {
    rate_limit_until: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    login: tokio::sync::Mutex<GithubLoginCache>,
}

#[derive(Default)]
struct GithubLoginCache {
    success: Option<String>,
    retry_at: Option<tokio::time::Instant>,
    consecutive_failures: u8,
}

fn credential_state(token: Option<&str>) -> Arc<GithubCredentialState> {
    static CREDENTIALS: OnceLock<Mutex<HashMap<[u8; 32], Arc<GithubCredentialState>>>> =
        OnceLock::new();
    let mut digest = Sha256::new();
    match token {
        Some(token) => {
            digest.update(b"token\0");
            digest.update(token.as_bytes());
        }
        None => digest.update(b"anonymous"),
    }
    let key: [u8; 32] = digest.finalize().into();
    let mut credentials = CREDENTIALS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    credentials
        .entry(key)
        .or_insert_with(|| Arc::new(GithubCredentialState::default()))
        .clone()
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

struct GithubApi {
    base: String,
    client: reqwest::Client,
    credential: Arc<GithubCredentialState>,
    control: PollControl,
}

impl GithubApi {
    #[cfg(test)]
    fn new(base: &str, token: Option<String>) -> Result<Self> {
        Self::new_with_control(base, token, PollControl::always_enabled())
    }

    fn new_with_control(base: &str, token: Option<String>, control: PollControl) -> Result<Self> {
        let parsed = url::Url::parse(base).context("parse GitHub API base URL")?;
        let host = parsed
            .host_str()
            .context("GitHub API base URL has no host")?;
        ensure!(
            parsed.scheme() == "https" || (parsed.scheme() == "http" && is_loopback_host(host)),
            "GitHub API base URL must use HTTPS except for loopback HTTP hosts"
        );
        let credential = credential_state(token.as_deref());
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
            credential,
            control,
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

    async fn pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        etag: Option<&str>,
    ) -> Result<GithubResponse> {
        let mut request = self
            .client
            .get(format!("{}/repos/{owner}/{repo}/pulls/{number}", self.base));
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        self.get(request).await
    }

    async fn latest_reopened_actor(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<String>> {
        let events = self
            .array_pages(&format!("repos/{owner}/{repo}/issues/{number}/events"), &[])
            .await?;
        Ok(events
            .iter()
            .filter(|event| event["event"] == "reopened")
            .max_by_key(|event| event["created_at"].as_str())
            .and_then(|event| event["actor"]["login"].as_str())
            .map(str::to_owned))
    }

    async fn credential_login(&self) -> Result<Option<String>> {
        self.control.check_enabled()?;
        let mut cache = self.credential.login.lock().await;
        if let Some(login) = &cache.success {
            return Ok(Some(login.clone()));
        }
        if cache
            .retry_at
            .is_some_and(|retry_at| retry_at > tokio::time::Instant::now())
        {
            return Ok(None);
        }
        let user = self.get_json("user", &[], None).await;
        self.control.check_enabled()?;
        let lookup = user.and_then(|user| {
            user["login"]
                .as_str()
                .filter(|login| !login.is_empty())
                .map(str::to_owned)
                .context("GitHub credential response has no login")
        });
        match lookup {
            Ok(login) => {
                cache.success = Some(login.clone());
                cache.retry_at = None;
                cache.consecutive_failures = 0;
                Ok(Some(login))
            }
            Err(error) if is_mailboxes_disabled(&error) => Err(error),
            Err(error) => {
                cache.consecutive_failures = cache.consecutive_failures.saturating_add(1);
                let exponent = u32::from(cache.consecutive_failures.saturating_sub(1).min(8));
                let retry_delay =
                    Duration::from_secs(1_u64 << exponent).min(Duration::from_secs(5 * 60));
                cache.retry_at = Some(tokio::time::Instant::now() + retry_delay);
                tracing::debug!(
                    error = %format!("{error:#}"),
                    retry_seconds = retry_delay.as_secs(),
                    "GitHub credential login is unknown; lookup will retry after backoff"
                );
                Ok(None)
            }
        }
    }

    async fn get(&self, request: reqwest::RequestBuilder) -> Result<GithubResponse> {
        self.control.check_enabled()?;
        self.wait_for_rate_limit().await?;
        self.control.check_enabled()?;
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
        loop {
            self.control.check_enabled()?;
            let deadline = *self.credential.rate_limit_until.lock().await;
            let Some(deadline) = deadline else {
                return Ok(());
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {},
                _ = tokio::time::sleep(CONFIG_RECHECK_INTERVAL) => continue,
            }
            let now = tokio::time::Instant::now();
            let mut shared_deadline = self.credential.rate_limit_until.lock().await;
            if shared_deadline.is_some_and(|current| current <= now) {
                *shared_deadline = None;
            }
        }
    }

    async fn record_rate_limit(&self, response: &reqwest::Response) {
        let headers = response.headers();
        let now = Utc::now();
        let reset = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
        let retry_after = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .parse::<u64>()
                    .ok()
                    .map(|seconds| {
                        now + chrono::Duration::seconds(seconds.min(i64::MAX as u64) as i64)
                    })
                    .or_else(|| {
                        DateTime::parse_from_rfc2822(value)
                            .ok()
                            .map(|date| date.with_timezone(&Utc))
                    })
            });
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
                .into_iter()
                .chain(reset)
                .max()
                .unwrap_or_else(|| now + chrono::Duration::minutes(1));
            let delay = (retry_at - now).to_std().unwrap_or_default();
            let next = tokio::time::Instant::now() + delay;
            let mut deadline = self.credential.rate_limit_until.lock().await;
            if deadline.is_none_or(|current| current < next) {
                *deadline = Some(next);
            }
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
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const CHILD: &str = "MJ_GITHUB_WATCH_INTEGRATION_CHILD";

    #[derive(Clone)]
    struct RateLimitServer {
        accepted: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct LoginRetryServer {
        requests: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct DisablePollServer {
        enabled: Arc<AtomicBool>,
        requests: Arc<AtomicUsize>,
    }

    #[tokio::test]
    async fn github_login_retries_after_a_transient_failure() {
        let state = LoginRetryServer {
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/user", get(retrying_user))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = GithubApi::new(&base, Some("watch-login-retry-test-token".into())).unwrap();

        assert_eq!(api.credential_login().await.unwrap(), None);
        assert_eq!(api.credential_login().await.unwrap(), None);
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert_eq!(
            api.credential_login().await.unwrap().as_deref(),
            Some("watcher-bot")
        );
        assert_eq!(
            api.credential_login().await.unwrap().as_deref(),
            Some("watcher-bot")
        );
        assert_eq!(state.requests.load(Ordering::SeqCst), 2);

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn watcher_stops_classification_and_requests_when_mailboxes_are_disabled() {
        if !run_isolated_child(
            "watcher_stops_classification_and_requests_when_mailboxes_are_disabled",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut session = crate::database::test_session("disable-poll-session", "project");
        session.state = mj_core::state::SessionState::Running;
        crate::database::save_session(&session).unwrap();
        let old = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        crate::database::save_github_repo_cursor(
            "acme",
            "repo",
            crate::database::GithubRepoCursor {
                items_watermark_at: Some(old.clone()),
                items_watermark_id: Some(1),
                items_etag: None,
                comments_cursor: Some(now_rfc3339()),
            },
        )
        .unwrap();

        let state = DisablePollServer {
            enabled: Arc::new(AtomicBool::new(true)),
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/repos/{owner}/{repo}/issues", get(disable_after_issues))
            .route("/ping", get(count_ping))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let calls = Arc::new(AtomicUsize::new(0));
        let classifier: Arc<dyn GithubClassifier> = Arc::new(FakeClassifier {
            calls: calls.clone(),
        });
        let enabled = state.enabled.clone();
        let api = GithubApi::new_with_control(
            &base,
            Some("watch-disable-poll-test-token".into()),
            PollControl::new(move || enabled.load(Ordering::SeqCst)),
        )
        .unwrap();
        let error = poll_repository_with_api(
            "acme",
            "repo",
            vec![session],
            classifier,
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(is_mailboxes_disabled(&error));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "Jev is not called after opt-out"
        );
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        assert!(is_mailboxes_disabled(
            &api.get_json("ping", &[], None).await.unwrap_err()
        ));
        assert_eq!(
            state.requests.load(Ordering::SeqCst),
            1,
            "no request follows opt-out"
        );
        let cursor = crate::database::load_github_repo_cursor("acme", "repo")
            .unwrap()
            .unwrap();
        assert_eq!(cursor.items_watermark_at.as_deref(), Some(old.as_str()));

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn github_rate_limit_deadline_is_shared_after_failed_requests() {
        let state = RateLimitServer {
            accepted: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/retry-after", get(rate_limited_retry_after))
            .route("/reset", get(rate_limited_reset))
            .route("/ok", get(rate_limit_ok))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        for (path, token) in [
            ("retry-after", "watch-rate-retry-after-test-token"),
            ("reset", "watch-rate-reset-test-token"),
        ] {
            let limited_api = GithubApi::new(&base, Some(token.into())).unwrap();
            assert!(limited_api.get_json(path, &[], None).await.is_err());
            let second_repository_api = GithubApi::new(&base, Some(token.into())).unwrap();
            let blocked = tokio::time::timeout(
                Duration::from_millis(150),
                second_repository_api.get_json("ok", &[], None),
            )
            .await;
            assert!(
                blocked.is_err(),
                "the credential deadline must block requests"
            );
            assert_eq!(state.accepted.load(Ordering::SeqCst), 0);
            tokio::time::timeout(
                Duration::from_secs(3),
                second_repository_api.get_json("ok", &[], None),
            )
            .await
            .expect("rate-limit deadline expires")
            .unwrap();
            assert_eq!(state.accepted.load(Ordering::SeqCst), 1);
            state.accepted.store(0, Ordering::SeqCst);
        }

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn github_api_base_allows_plain_http_only_for_loopback_hosts() {
        assert!(GithubApi::new("http://example.com", Some("secret".into())).is_err());
        assert!(GithubApi::new("http://127.0.0.1:1234", None).is_ok());
        assert!(GithubApi::new("http://localhost:1234", None).is_ok());
        assert!(GithubApi::new("https://api.github.com", None).is_ok());
    }

    #[derive(Clone)]
    struct FakeGithub {
        data: Arc<tokio::sync::Mutex<FakeGithubData>>,
        not_modified: Arc<AtomicUsize>,
        pull_not_modified: Arc<AtomicUsize>,
        user_requests: Arc<AtomicUsize>,
        review_requests: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct FakePull {
        value: Value,
        etag: String,
    }

    #[derive(Clone)]
    struct FakeGithubData {
        items: Vec<Value>,
        comments: Vec<Value>,
        review_comments: Vec<Value>,
        reviews: Vec<Value>,
        etag: String,
        pulls: BTreeMap<u64, FakePull>,
        issue_events: BTreeMap<u64, Vec<Value>>,
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

    // Hard-won: 193f015e: whole-second GitHub timestamps can miss turn-window suppression.
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
        let creator_id = "creator-session";
        let credential = "watcher-lifecycle-integration-token";
        let mut session = crate::database::test_session(creator_id, "project");
        session.state = mj_core::state::SessionState::Running;
        session.project = Some(mj_core::repository::ProjectBundleSnapshot {
            bundle: mj_core::config::ProjectBundle {
                primary_repo: "main".into(),
                repositories: vec![
                    mj_core::config::ProjectRepository {
                        id: "main".into(),
                        github: Some("Acme/Repo".into()),
                        local: None,
                        destination: ".".into(),
                        git_ref: None,
                    },
                    mj_core::config::ProjectRepository {
                        id: "shared".into(),
                        github: Some("Another/Repository".into()),
                        local: None,
                        destination: "shared".into(),
                        git_ref: None,
                    },
                ],
            },
            identities: BTreeMap::from([
                (
                    "main".into(),
                    RepositoryIdentity::Github("Acme".into(), "Repo".into()),
                ),
                (
                    "shared".into(),
                    RepositoryIdentity::Github("Another".into(), "Repository".into()),
                ),
            ]),
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
                review_comments: Vec::new(),
                reviews: Vec::new(),
                etag: "\"v1\"".into(),
                pulls: BTreeMap::new(),
                issue_events: BTreeMap::new(),
            })),
            not_modified: Arc::new(AtomicUsize::new(0)),
            pull_not_modified: Arc::new(AtomicUsize::new(0)),
            user_requests: Arc::new(AtomicUsize::new(0)),
            review_requests: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/repos/{owner}/{repo}/issues", get(fake_issues))
            .route("/repos/{owner}/{repo}/issues/comments", get(fake_comments))
            .route(
                "/repos/{owner}/{repo}/pulls/comments",
                get(fake_review_comments),
            )
            .route("/repos/{owner}/{repo}/pulls/{number}", get(fake_pull))
            .route(
                "/repos/{owner}/{repo}/issues/{number}/events",
                get(fake_issue_events),
            )
            .route("/user", get(fake_user))
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
        let api = GithubApi::new(&base, Some(credential.into())).unwrap();

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
                        51 | 52 => "created pull request",
                        _ => "ordinary issue",
                    };
                    github_item(
                        number,
                        number,
                        title,
                        (now + chrono::Duration::seconds(number as i64)).to_rfc3339(),
                        matches!(number, 51 | 52),
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
        let watches = crate::database::load_github_watches("acme", "repo").unwrap();
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].number, 51);
        let pending = crate::database::pending_mailbox_events(20).unwrap();
        let interest = pending
            .iter()
            .find(|entry| entry.event_key == "github:acme/repo#2:interest:creator-session")
            .expect("interested session receives a non-waking note");
        let interest_event: MailboxEvent = serde_json::from_str(&interest.event_json).unwrap();
        assert!(!interest_event.wake);
        assert!(matches!(
            interest_event.body,
            MailboxEventBody::NewGithubItem {
                kind: GithubItemKind::Issue,
                title,
                repo: Some(repo),
                ..
            } if title == "interesting issue" && repo == "Acme/Repo"
        ));
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
        let restarted_api = GithubApi::new(&base, Some(credential.into())).unwrap();
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
        assert_eq!(watches.len(), 2);
        assert!(watches.iter().any(|watch| watch.number == 51));
        assert!(watches.iter().any(|watch| watch.number == 52));
        assert_eq!(watches[0].creator_session_id, creator_id);
        let comments_cursor = crate::database::load_github_repo_cursor("acme", "repo")
            .unwrap()
            .unwrap()
            .comments_cursor
            .unwrap();
        let cursor_at_ms = timestamp_millis(&comments_cursor).unwrap();
        let cursor_second_ms = floor_to_second(cursor_at_ms);
        crate::database::seed_github_test_turns(
            creator_id,
            cursor_second_ms - 3_000,
            cursor_second_ms - 2_000,
            cursor_second_ms - 500,
        )
        .unwrap();
        let comment_at = |offset_ms: i64| {
            DateTime::<Utc>::from_timestamp_millis(cursor_second_ms + offset_ms)
                .unwrap()
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        };
        {
            let mut data = fake.data.lock().await;
            data.comments = vec![
                json!({
                    "id": 9001,
                    "issue_url": "http://api/repos/acme/repo/issues/52",
                    "user": {"login": "reviewer"},
                    "body": "Please update the changelog.",
                    "created_at": comment_at(0),
                    "updated_at": comment_at(0)
                }),
                json!({
                    "id": 9002,
                    "issue_url": "http://api/repos/acme/repo/issues/52",
                    "user": {"login": "watcher-bot"},
                    "body": "agent reply during completed turn",
                    "created_at": comment_at(-2_500),
                    "updated_at": comment_at(-2_500)
                }),
                json!({
                    "id": 9003,
                    "issue_url": "http://api/repos/acme/repo/issues/52",
                    "user": {"login": "watcher-bot"},
                    "body": "user reply while idle",
                    "created_at": comment_at(-1_500),
                    "updated_at": comment_at(-1_500)
                }),
                json!({
                    "id": 9004,
                    "issue_url": "http://api/repos/acme/repo/issues/52",
                    "user": {"login": "watcher-bot"},
                    "body": "agent reply during active turn",
                    "created_at": comment_at(-500),
                    "updated_at": comment_at(-500)
                }),
                json!({
                    "id": 9007,
                    "issue_url": "http://api/repos/acme/repo/issues/52",
                    "user": {"login": "watcher-bot"},
                    "body": "agent reply exactly when the turn ended",
                    "created_at": comment_at(-2_000),
                    "updated_at": comment_at(-2_000)
                }),
                json!({
                    "id": 9008,
                    "issue_url": "http://api/repos/acme/repo/issues/52",
                    "user": {"login": "watcher-bot"},
                    "body": "agent reply in the first GitHub timestamp second of the turn",
                    "created_at": DateTime::<Utc>::from_timestamp_millis(
                        cursor_second_ms - 1_000
                    )
                    .unwrap()
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                    "updated_at": DateTime::<Utc>::from_timestamp_millis(
                        cursor_second_ms - 1_000
                    )
                    .unwrap()
                    .to_rfc3339_opts(SecondsFormat::Secs, true)
                }),
            ];
            data.review_comments = vec![
                json!({
                    "id": 9005,
                    "pull_request_url": "http://api/repos/acme/repo/pulls/52",
                    "user": {"login": "reviewer"},
                    "body": "inline note at the cursor boundary",
                    "created_at": comment_at(-3_500),
                    "updated_at": comment_at(-3_500)
                }),
                json!({
                    "id": 9006,
                    "pull_request_url": "http://api/repos/acme/repo/pulls/52",
                    "user": {"login": "watcher-bot"},
                    "body": "agent inline reply during active turn",
                    "created_at": comment_at(-500),
                    "updated_at": comment_at(-500)
                }),
            ];
            data.reviews = vec![
                json!({
                    "id": 9101,
                    "user": {"login": "reviewer"},
                    "body": "approved at the cursor boundary",
                    "state": "APPROVED",
                    "submitted_at": comment_at(-3_500)
                }),
                json!({
                    "id": 9102,
                    "user": {"login": "watcher-bot"},
                    "body": "approved during active turn",
                    "state": "APPROVED",
                    "submitted_at": comment_at(-500)
                }),
            ];
        }
        let restarted_api = GithubApi::new(&base, Some(credential.into())).unwrap();
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
        assert_eq!(fake.user_requests.load(Ordering::SeqCst), 1);
        let pending = crate::database::pending_mailbox_events(100).unwrap();
        let event = |key: &str| {
            let row = pending
                .iter()
                .find(|entry| entry.event_key == key)
                .unwrap_or_else(|| panic!("missing mailbox event {key}"));
            assert_eq!(row.target_session_id, creator_id);
            serde_json::from_str::<MailboxEvent>(&row.event_json).unwrap()
        };
        let reviewer_comment = event("github:acme/repo#52:comment:9001");
        assert!(reviewer_comment.wake);
        assert!(matches!(
            reviewer_comment.body,
            MailboxEventBody::GithubComment {
                item_kind: GithubItemKind::PullRequest,
                author,
                body,
                repo: Some(repo),
                ..
            } if author == "reviewer" && body == "Please update the changelog." && repo == "Acme/Repo"
        ));
        assert!(!event("github:acme/repo#52:comment:9002").wake);
        assert!(event("github:acme/repo#52:comment:9003").wake);
        assert!(!event("github:acme/repo#52:comment:9004").wake);
        assert!(event("github:acme/repo#52:comment:9007").wake);
        assert!(!event("github:acme/repo#52:comment:9008").wake);
        let review_comment = event("github:acme/repo#52:review-comment:9005");
        assert!(review_comment.wake);
        assert!(matches!(
            review_comment.body,
            MailboxEventBody::GithubReviewComment {
                author,
                body,
                repo: Some(repo),
                ..
            } if author == "reviewer" && body == "inline note at the cursor boundary" && repo == "Acme/Repo"
        ));
        assert!(!event("github:acme/repo#52:review-comment:9006").wake);
        let review = event("github:acme/repo#52:review:9101");
        assert!(review.wake);
        assert!(matches!(
            review.body,
            MailboxEventBody::GithubReview {
                author,
                body,
                review_state: Some(state),
                repo: Some(repo),
                ..
            } if author == "reviewer" && body == "approved at the cursor boundary" && state == "APPROVED" && repo == "Acme/Repo"
        ));
        assert!(!event("github:acme/repo#52:review:9102").wake);

        let review_requests_before_terminal = fake.review_requests.load(Ordering::SeqCst);
        let closed_at = comment_at(2_000);
        let merged_at = comment_at(3_000);
        {
            let mut data = fake.data.lock().await;
            data.pulls.insert(
                51,
                FakePull {
                    value: json!({
                        "state": "closed",
                        "closed_at": closed_at,
                        "updated_at": closed_at,
                        "closed_by": {"login": "closer"}
                    }),
                    etag: "\"pull-51-v2\"".into(),
                },
            );
            data.pulls.insert(
                52,
                FakePull {
                    value: json!({
                        "state": "closed",
                        "closed_at": merged_at,
                        "merged_at": merged_at,
                        "updated_at": merged_at,
                        "merged_by": {"login": "merger"}
                    }),
                    etag: "\"pull-52-v2\"".into(),
                },
            );
            data.comments.push(json!({
                "id": 9010,
                "issue_url": "http://api/repos/acme/repo/issues/51",
                "user": {"login": "reviewer"},
                "body": "comment after the pull request closed",
                "created_at": comment_at(5_000),
                "updated_at": comment_at(5_000)
            }));
        }
        let api = GithubApi::new(&base, Some(credential.into())).unwrap();
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
            fake.review_requests.load(Ordering::SeqCst),
            review_requests_before_terminal,
            "closed and merged pull requests stop review polling"
        );
        let closed_key = format!("github:acme/repo#51:closed:{closed_at}");
        let merged_key = "github:acme/repo#52:merged";
        let pending = crate::database::pending_mailbox_events(100).unwrap();
        let closed_event = serde_json::from_str::<MailboxEvent>(
            &pending
                .iter()
                .find(|entry| entry.event_key == closed_key)
                .expect("closed pull request creates an outbox event")
                .event_json,
        )
        .unwrap();
        let merged_event = serde_json::from_str::<MailboxEvent>(
            &pending
                .iter()
                .find(|entry| entry.event_key == merged_key)
                .expect("merged pull request creates an outbox event")
                .event_json,
        )
        .unwrap();
        assert!(closed_event.wake);
        assert!(matches!(
            closed_event.body,
            MailboxEventBody::GithubPullRequestLifecycle {
                change: MailboxPullRequestChange::ClosedWithoutMerging,
                actor,
                repo: Some(repo),
                ..
            } if actor == "closer" && repo == "Acme/Repo"
        ));
        assert!(merged_event.wake);
        assert!(matches!(
            merged_event.body,
            MailboxEventBody::GithubPullRequestLifecycle {
                change: MailboxPullRequestChange::Merged,
                actor,
                repo: Some(repo),
                ..
            } if actor == "merger" && repo == "Acme/Repo"
        ));
        assert!(
            pending
                .iter()
                .any(|entry| entry.event_key == "github:acme/repo#51:comment:9010")
        );
        assert_eq!(
            merged_event.created_at_ms,
            timestamp_millis(&merged_at).unwrap().max(0) as u64
        );

        writer.shutdown().unwrap();
        let writer = crate::database::install_isolated_test_writer();
        let pull_304_before_restart_poll = fake.pull_not_modified.load(Ordering::SeqCst);
        let restarted_api = GithubApi::new(&base, Some(credential.into())).unwrap();
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
        assert_eq!(
            fake.pull_not_modified.load(Ordering::SeqCst),
            pull_304_before_restart_poll + 2
        );
        let pending = crate::database::pending_mailbox_events(100).unwrap();
        assert_eq!(
            pending
                .iter()
                .filter(|entry| entry.event_key == closed_key)
                .count(),
            1,
            "restart does not enqueue the closed transition again"
        );
        assert_eq!(
            pending
                .iter()
                .filter(|entry| entry.event_key == merged_key)
                .count(),
            1,
            "restart does not enqueue the merged transition again"
        );
        assert_eq!(
            pending
                .iter()
                .filter(|entry| entry.event_key == "github:acme/repo#51:comment:9010")
                .count(),
            1,
            "comments continue to be delivered after a pull request closes"
        );

        let reopened_at = comment_at(4_000);
        {
            let mut data = fake.data.lock().await;
            data.pulls.insert(
                51,
                FakePull {
                    value: json!({"state": "open", "updated_at": reopened_at}),
                    etag: "\"pull-51-v3\"".into(),
                },
            );
            data.issue_events.insert(
                51,
                vec![json!({
                    "event": "reopened",
                    "actor": {"login": "reopener"},
                    "created_at": reopened_at
                })],
            );
        }
        let review_requests_before_reopen = fake.review_requests.load(Ordering::SeqCst);
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
            fake.review_requests.load(Ordering::SeqCst),
            review_requests_before_reopen + 1,
            "reviews resume only for the reopened pull request"
        );
        let pending = crate::database::pending_mailbox_events(100).unwrap();
        let reopened = pending
            .iter()
            .find(|entry| entry.event_key == format!("github:acme/repo#51:reopened:{reopened_at}"))
            .expect("reopening a watched pull request is reported once");
        let reopened_event = serde_json::from_str::<MailboxEvent>(&reopened.event_json).unwrap();
        assert!(reopened_event.wake);
        assert!(matches!(
            reopened_event.body,
            MailboxEventBody::GithubPullRequestLifecycle {
                change: MailboxPullRequestChange::Reopened,
                actor,
                ..
            } if actor == "reopener"
        ));
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
            "repository": {"full_name": "acme/repo"},
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

    async fn retrying_user(State(state): State<LoginRetryServer>) -> Response {
        if state.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        } else {
            Json(json!({"login": "watcher-bot"})).into_response()
        }
    }

    async fn disable_after_issues(State(state): State<DisablePollServer>) -> Json<Vec<Value>> {
        state.requests.fetch_add(1, Ordering::SeqCst);
        let item = github_item(
            2,
            2,
            "interesting issue after opt-out",
            (Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
            false,
        );
        state.enabled.store(false, Ordering::SeqCst);
        Json(vec![item])
    }

    async fn count_ping(State(state): State<DisablePollServer>) -> Json<Value> {
        state.requests.fetch_add(1, Ordering::SeqCst);
        Json(json!({"ok": true}))
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

    async fn fake_comments(
        State(fake): State<FakeGithub>,
        Query(query): Query<BTreeMap<String, String>>,
    ) -> Json<Vec<Value>> {
        let since = query.get("since").expect("comment requests include since");
        let comments = fake.data.lock().await.comments.clone();
        Json(comments_after(&comments, since))
    }

    async fn fake_review_comments(
        State(fake): State<FakeGithub>,
        Query(query): Query<BTreeMap<String, String>>,
    ) -> Json<Vec<Value>> {
        let since = query
            .get("since")
            .expect("review comment requests include since");
        let comments = fake.data.lock().await.review_comments.clone();
        Json(comments_after(&comments, since))
    }

    fn comments_after(comments: &[Value], since: &str) -> Vec<Value> {
        comments
            .iter()
            .filter(|comment| {
                comment_timestamp(comment)
                    .is_some_and(|timestamp| timestamp_precedes(since, timestamp))
            })
            .cloned()
            .collect()
    }

    async fn fake_pull(
        State(fake): State<FakeGithub>,
        Path((_owner, _repo, number)): Path<(String, String, u64)>,
        headers: HeaderMap,
    ) -> Response {
        let pull = fake
            .data
            .lock()
            .await
            .pulls
            .get(&number)
            .cloned()
            .unwrap_or_else(|| FakePull {
                value: json!({"state": "open"}),
                etag: format!("\"pull-{number}-v1\""),
            });
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            == Some(pull.etag.as_str())
        {
            fake.pull_not_modified.fetch_add(1, Ordering::SeqCst);
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, pull.etag)]).into_response();
        }
        (
            StatusCode::OK,
            [(header::ETAG, pull.etag)],
            Json(pull.value),
        )
            .into_response()
    }

    async fn fake_issue_events(
        State(fake): State<FakeGithub>,
        Path((_owner, _repo, number)): Path<(String, String, u64)>,
    ) -> Json<Vec<Value>> {
        Json(
            fake.data
                .lock()
                .await
                .issue_events
                .get(&number)
                .cloned()
                .unwrap_or_default(),
        )
    }

    async fn fake_reviews(
        State(fake): State<FakeGithub>,
        Path((_owner, _repo, _number)): Path<(String, String, u64)>,
    ) -> Json<Vec<Value>> {
        fake.review_requests.fetch_add(1, Ordering::SeqCst);
        Json(fake.data.lock().await.reviews.clone())
    }

    async fn fake_user(State(fake): State<FakeGithub>) -> Json<Value> {
        fake.user_requests.fetch_add(1, Ordering::SeqCst);
        Json(json!({"login": "watcher-bot"}))
    }

    async fn rate_limited_retry_after() -> Response {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (header::RETRY_AFTER, "1"),
                (
                    header::HeaderName::from_static("x-ratelimit-remaining"),
                    "0",
                ),
            ],
        )
            .into_response()
    }

    async fn rate_limited_reset() -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-ratelimit-reset",
            HeaderValue::from_str(&(Utc::now().timestamp() + 2).to_string()).unwrap(),
        );
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        (StatusCode::FORBIDDEN, headers).into_response()
    }

    async fn rate_limit_ok(State(state): State<RateLimitServer>) -> Json<Value> {
        state.accepted.fetch_add(1, Ordering::SeqCst);
        Json(json!({"ok": true}))
    }
}
