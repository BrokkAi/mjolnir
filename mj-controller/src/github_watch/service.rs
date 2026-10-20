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

const MAX_CLASSIFICATIONS_PER_POLL: usize = 50;
const ITEMS_PER_PAGE: usize = 100;
const MERGE_CLOSE_TOLERANCE_MS: i64 = 5_000;
const CLASSIFICATION_CONCURRENCY: usize = 4;
const REPOSITORY_CONCURRENCY: usize = 4;
const API_TIMEOUT: Duration = Duration::from_secs(20);
const CONFIG_RECHECK_INTERVAL: Duration = Duration::from_secs(5);
const MIN_INTERVAL: Duration = Duration::from_secs(10);
const MAX_INTERVAL: Duration = Duration::from_secs(60 * 60);

type ClassifyFuture<'a> = Pin<Box<dyn Future<Output = Result<GithubItemVerdict>> + Send + 'a>>;

#[derive(Clone)]
struct PollControl(Arc<dyn Fn() -> bool + Send + Sync>);

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
            Err(anyhow::Error::new(GithubEventsDisabled))
        }
    }
}

#[derive(Debug)]
struct GithubEventsDisabled;

impl std::fmt::Display for GithubEventsDisabled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GitHub events disabled during poll")
    }
}

impl std::error::Error for GithubEventsDisabled {}

fn is_github_events_disabled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<GithubEventsDisabled>().is_some()
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
        let mailboxes_enabled = projection.config.github_events_enabled();
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
        if !projection.config.github_events_enabled() {
            continue;
        }
        let config = projection.config.github.watch;
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
                let poll_state = state.clone();
                jobs.spawn(async move {
                    let control = PollControl::new(move || {
                        poll_state
                            .controller_projection()
                            .config
                            .github_events_enabled()
                    });
                    let result =
                        poll_repository(&owner, &repo, sessions, config, classifier, stop, control)
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
                        Ok((owner, repo, Err(error))) if is_github_events_disabled(&error) => tracing::debug!(%owner, %repo, "GitHub repository poll stopped because GitHub events were disabled"),
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
        // A session started with `--no-github-events` is never a delivery target.
        if !session.state.has_live_worker()
            || state.is_subagent_session(&session.id)
            || session.no_github_events
        {
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
    control: PollControl,
) -> Result<()> {
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
    poll_repository_with_context(owner, repo, sessions, classifier, &api, stop).await
}

async fn poll_repository_with_context(
    owner: &str,
    repo: &str,
    sessions: Vec<SessionRecord>,
    classifier: Arc<dyn GithubClassifier>,
    api: &GithubApi,
    stop: CancellationToken,
) -> Result<()> {
    let cursor = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::github_repo_cursor(&owner, &repo)
    })
    .await?;
    record_repository(owner, repo, &cursor, api, &stop).await?;
    if let Err(error) = reconcile_repository(owner, repo, api, &stop, Utc::now()).await {
        if is_github_events_disabled(&error) {
            return Err(error);
        }
        tracing::warn!(%owner, %repo, %error, "GitHub reconciliation failed; retrying from checkpoint next hour");
    }
    // Classification and timeline work have separate durable progress. Neither
    // one's failure prevents the other from running.
    let (classification, timeline) = tokio::join!(
        classify_recorded_items(
            owner,
            repo,
            &sessions,
            classifier,
            &cursor.watch_started_at,
            api,
            &stop
        ),
        poll_repository_timelines(owner, repo, api, &stop),
    );
    classification?;
    timeline
}

async fn record_repository(
    owner: &str,
    repo: &str,
    cursor: &crate::database::GithubRepoCursor,
    api: &GithubApi,
    stop: &CancellationToken,
) -> Result<()> {
    acquire_repository(owner, repo, &cursor.updated_at, None, api, stop).await
}

async fn reconcile_repository(
    owner: &str,
    repo: &str,
    api: &GithubApi,
    stop: &CancellationToken,
    now: DateTime<Utc>,
) -> Result<()> {
    if stop.is_cancelled() {
        return Ok(());
    }
    api.control.check_enabled()?;
    // Read the writer's current coverage after main acquisition, not its old snapshot.
    let cursor = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::github_repo_cursor(&owner, &repo)
    })
    .await?;
    let mut until = (now - chrono::Duration::hours(1)).to_rfc3339();
    if timestamp_precedes(&cursor.updated_at, &until) {
        until = cursor.updated_at;
    }
    // Equality must replay the boundary: a crash may leave its tie unfinished.
    if timestamp_precedes(&until, &cursor.reconciled_through) {
        return Ok(());
    }
    let claimed = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::claim_github_reconciliation(owner, repo, now.timestamp_millis())
    })
    .await?;
    if !claimed {
        return Ok(());
    }
    // Re-read every main-feed range once its item timestamps are an hour old.
    // Page commits retain coverage across pauses, failed requests and crashes.
    // With offset pages, an item can be missed only if moves within its tied
    // second disturb both the main read and the later reconciliation read.
    acquire_repository(
        owner,
        repo,
        &cursor.reconciled_through,
        Some(&until),
        api,
        stop,
    )
    .await
}

async fn acquire_repository(
    owner: &str,
    repo: &str,
    from: &str,
    until: Option<&str>,
    api: &GithubApi,
    stop: &CancellationToken,
) -> Result<()> {
    // GitHub's `since` is inclusive and offset pages are ordered by update
    // time. After a full page, every item older than its last timestamp has
    // been read, so the next read starts at that timestamp. Only a page that
    // holds nothing newer than `since` itself pages forward by offset, so
    // each request either advances `since` or reads deeper into one second.
    let mut newest = from.to_owned();
    let mut since = overlap_timestamp(&newest);
    let mut page = 1;
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        let response = api
            .get_json(
                &format!("repos/{owner}/{repo}/issues"),
                &[
                    ("state", "all".into()),
                    ("sort", "updated".into()),
                    ("direction", "asc".into()),
                    ("since", since.clone()),
                    ("per_page", ITEMS_PER_PAGE.to_string()),
                    ("page", page.to_string()),
                ],
            )
            .await?;
        api.control.check_enabled()?;
        let values = response
            .as_array()
            .context("GitHub issues response is not an array")?;
        let mut reached_end = false;
        let mut last: Option<String> = None;
        let mut items = Vec::new();
        for value in values {
            if let Some(item) = parse_github_item(value) {
                if until.is_some_and(|end| timestamp_precedes(end, &item.updated_at)) {
                    reached_end = true;
                    continue;
                }
                if timestamp_precedes(&newest, &item.updated_at) {
                    newest = item.updated_at.clone();
                }
                if last
                    .as_ref()
                    .is_none_or(|last| timestamp_precedes(last, &item.updated_at))
                {
                    last = Some(item.updated_at.clone());
                }
                items.push(item);
            } else {
                tracing::warn!(%owner, %repo, "GitHub issues feed contains an invalid item; skipping it");
            }
        }
        let short_page = values.len() < ITEMS_PER_PAGE;
        let item_owner = owner.to_owned();
        let item_repo = repo.to_owned();
        // A short page or the first item beyond the bound proves this range
        // complete, including a gap with no items. Otherwise save the newest
        // timestamp; restarting from its overlap rereads its whole second.
        let completed_range = until.filter(|_| reached_end || short_page);
        let through = completed_range.unwrap_or(&newest).to_owned();
        let progress = if until.is_some() {
            crate::database::GithubFeedProgress::Reconciled(through)
        } else {
            crate::database::GithubFeedProgress::Main(through)
        };
        blocking_db(move || {
            crate::database::record_github_page(item_owner, item_repo, items, progress)
        })
        .await?;
        if completed_range.is_some() || short_page {
            break;
        }
        match last {
            Some(last) if timestamp_precedes(&since, &last) => {
                since = last;
                page = 1;
            }
            _ => page += 1,
        }
    }
    Ok(())
}

async fn classify_recorded_items(
    owner: &str,
    repo: &str,
    sessions: &[SessionRecord],
    classifier: Arc<dyn GithubClassifier>,
    watch_started_at: &str,
    api: &GithubApi,
    stop: &CancellationToken,
) -> Result<()> {
    let items = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_seen_items(&owner, &repo)
    })
    .await?;
    let failures = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_classification_failures(&owner, &repo)
    })
    .await?;
    let now = mj_core::clock::epoch_millis();
    let mut pending = Vec::new();
    for item in items {
        if stop.is_cancelled() {
            return Ok(());
        }
        api.control.check_enabled()?;
        if !timestamp_precedes(watch_started_at, &item.created_at) {
            continue;
        }
        let classified = blocking_db({
            let owner = owner.to_owned();
            let repo = repo.to_owned();
            let number = item.number;
            move || crate::database::load_github_item_classified_sessions(&owner, &repo, number)
        })
        .await?;
        for session in sessions {
            if session.no_github_events
                || timestamp_precedes(&item.created_at, &session.created_at)
                || classified.contains(&session.id)
            {
                continue;
            }
            let failure = failures.get(&(item.number, session.id.clone()));
            if failure.is_some_and(|failure| failure.next_attempt_at_ms > now) {
                continue;
            }
            pending.push((
                item.clone(),
                session.clone(),
                failure.map(|failure| failure.last_attempt_at_ms),
                failure.map_or(0, |failure| failure.attempts),
            ));
        }
    }
    // Each pair owns its retry state. Never-attempted work precedes retries;
    // eligible retries run oldest-first and never consume the cap in backoff.
    pending.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| a.0.number.cmp(&b.0.number))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    if pending.is_empty() {
        return Ok(());
    }
    let login = api.credential_login().await?;
    let login = login.as_deref();
    let results = stream::iter(pending.into_iter().take(MAX_CLASSIFICATIONS_PER_POLL)).map(|(item, session, _, attempts)| {
        let classifier = classifier.clone(); let control = api.control.clone();
        async move {
            if stop.is_cancelled() { return Ok(()); }
            let number = item.number; let session_id = session.id.clone();
            match classify_for_session(owner, repo, item, session, login, classifier, control).await {
                Ok(()) => Ok(()),
                Err(error) if is_github_events_disabled(&error) => Err(error),
                Err(error) => {
                    tracing::warn!(%owner, %repo, number, %session_id, attempt=attempts.saturating_add(1), %error, "GitHub classification failed; retrying with backoff");
                    let owner = owner.to_owned(); let repo = repo.to_owned();
                    blocking_db(move || crate::database::record_github_classification_failure(owner, repo, number, session_id)).await
                }
            }
        }
    }).buffer_unordered(CLASSIFICATION_CONCURRENCY).collect::<Vec<Result<()>>>().await;
    for result in results {
        result?;
    }
    Ok(())
}

async fn classify_for_session(
    owner: &str,
    repo: &str,
    item: crate::database::GithubSeenItem,
    session: SessionRecord,
    credential_login: Option<&str>,
    classifier: Arc<dyn GithubClassifier>,
    control: PollControl,
) -> Result<()> {
    let repo_label = format!("{owner}/{repo}");
    let kind = item_kind_from_watch(&item.kind);
    // Sessions share one GitHub account, so the author cannot name a session,
    // but this session did not open an item another account authored.
    let own_account = credential_login.is_none_or(|login| login.eq_ignore_ascii_case(&item.author));
    let recent_turns = recent_turns(session.id.clone()).await?;
    let evidence = GithubItemEvidence {
        item: GithubItem {
            repo: repo_label.clone(),
            kind,
            number: item.number as u64,
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
    let created = own_account
        && (verdict.created
            || opened_by_session_tool_call(&session.id, &item.created_at, &item.title).await?);
    let event = if verdict.interested && !created {
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
                kind,
                number: item.number as u64,
                title: item.title.clone(),
                repo: Some(repo_label.clone()),
            },
        };
        Some((key, serde_json::to_string(&event)?, false))
    } else {
        None
    };
    let classification = crate::database::GithubItemClassification {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: item.number,
        session_id: session.id,
        kind: item.kind,
        title: item.title,
        url: item.url,
        created_at: item.created_at,
        interested: verdict.interested,
        created,
    };
    blocking_db(move || {
        crate::database::commit_github_item_classification(classification, event).map(|_| ())
    })
    .await
}

/// Jev misses items whose opening command has left its evidence or never
/// showed the title. A `gh pr create` or `gh issue create` call that was
/// running when the item appeared and names its title also counts. Requiring
/// the title stops a long command from claiming another session's item.
async fn opened_by_session_tool_call(
    session_id: &str,
    created_at: &str,
    title: &str,
) -> Result<bool> {
    let Some(created_at_ms) = timestamp_millis(created_at) else {
        return Ok(false);
    };
    let session_id = session_id.to_owned();
    let commands = blocking_db(move || {
        crate::database::load_github_session_tool_commands_at(&session_id, created_at_ms)
    })
    .await?;
    Ok(commands
        .iter()
        .any(|command| command_opens_titled_item(command, title)))
}

fn command_opens_titled_item(command: &str, title: &str) -> bool {
    let command = spaced_words(command);
    let title = spaced_words(title);
    !title.trim().is_empty()
        && (command.contains(" gh pr create ") || command.contains(" gh issue create "))
        && command.contains(&title)
}

/// Lowercase words with a space before and after each, so shell quoting and
/// escaping cannot hide a title and a match always covers whole words.
fn spaced_words(text: &str) -> String {
    let mut spaced = String::from(" ");
    for word in text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        spaced.push_str(&word.to_lowercase());
        spaced.push(' ');
    }
    spaced
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

async fn poll_repository_timelines(
    owner: &str,
    repo: &str,
    api: &GithubApi,
    stop: &CancellationToken,
) -> Result<()> {
    let watches = blocking_db({
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        move || crate::database::load_github_watches(&owner, &repo)
    })
    .await?;
    for watch in watches {
        if stop.is_cancelled() {
            break;
        }
        api.control.check_enabled()?;
        let Some(updated_at) = &watch.recorded_updated_at else {
            continue;
        };
        if timestamp_precedes(updated_at, &watch.processed_at) {
            continue;
        }
        let number = watch.number;
        if let Err(error) = poll_item_timeline(watch, api).await {
            if is_github_events_disabled(&error) {
                return Err(error);
            }
            tracing::warn!(%owner, %repo, number, error=%format!("{error:#}"), "GitHub item timeline failed; will retry next poll");
        }
    }
    Ok(())
}

async fn poll_item_timeline(
    watch: crate::database::GithubItemWatch,
    api: &GithubApi,
) -> Result<()> {
    let fetched = async {
        let (entries, processed_at) = api
            .array_pages_observed(
                &format!(
                    "repos/{}/{}/issues/{}/timeline",
                    watch.owner, watch.repo, watch.number
                ),
                &[],
            )
            .await?;
        // A merge that lands commits directly, as Merge Marshall does, leaves
        // only a closed entry in the timeline. Only the pull request itself
        // records that it merged, so read it when such a close appears.
        let merged_at = if watch.kind == "pull_request"
            && has_close_without_merge(&entries, &watch.timeline_at)
        {
            api.get_json(
                &format!(
                    "repos/{}/{}/pulls/{}",
                    watch.owner, watch.repo, watch.number
                ),
                &[],
            )
            .await?["merged_at"]
                .as_str()
                .map(str::to_owned)
        } else {
            None
        };
        let comments = if watch.kind == "pull_request" {
            // Use the stored cursor, before either source advances it.
            api.array_pages(
                &format!(
                    "repos/{}/{}/pulls/{}/comments",
                    watch.owner, watch.repo, watch.number
                ),
                &[("since", overlap_timestamp(&watch.review_comments_at))],
            )
            .await?
        } else {
            Vec::new()
        };
        Ok::<_, anyhow::Error>((entries, comments, merged_at, processed_at))
    }
    .await;
    let (entries, comments, merged_at, processed_at) = match fetched {
        Ok(values) => values,
        Err(error)
            if error
                .downcast_ref::<GithubHttpStatus>()
                .is_some_and(|status| matches!(status.status.as_u16(), 404 | 410)) =>
        {
            (
                Vec::new(),
                Vec::new(),
                None,
                error
                    .downcast_ref::<GithubHttpStatus>()
                    .context("missing GitHub HTTP status")?
                    .observed_at
                    .clone(),
            )
        }
        Err(error) => {
            return Err(error.context(format!(
                "GitHub activity {}/{}#{}",
                watch.owner, watch.repo, watch.number
            )));
        }
    };
    let mapped = map_item_activity(&watch, &entries, &comments, merged_at.as_deref())?;
    let mut outbox = Vec::new();
    let mut turn_windows = BTreeMap::new();
    let mut login = None;
    if mapped.events.iter().any(|event| event.author.is_some()) {
        login = api.credential_login().await?;
    }
    for event in mapped.events {
        api.control.check_enabled()?;
        let suppress = authored_during_session_turn(
            login.as_deref(),
            event.author.as_deref(),
            Some(&event.timestamp),
            &watch.creator_session_id,
            &mut turn_windows,
        )
        .await;
        outbox.push(mailbox_outbox_row(
            event.key,
            watch.creator_session_id.clone(),
            event.body,
            !suppress,
            timestamp_millis(&event.timestamp)
                .context("invalid GitHub timeline timestamp")?
                .max(0) as u64,
        )?);
    }
    api.control.check_enabled()?;
    blocking_db(move || {
        crate::database::commit_github_timeline(
            watch,
            crate::database::GithubItemProgress {
                timeline_at: mapped.cursor_at,
                timeline_id: mapped.cursor_id,
                review_comments_at: mapped.review_comments_at,
                review_comments_id: mapped.review_comments_id,
                processed_at,
            },
            outbox,
        )
    })
    .await
}

struct TimelineEvent {
    key: String,
    timestamp: String,
    author: Option<String>,
    body: MailboxEventBody,
}

struct TimelineMapping {
    events: Vec<TimelineEvent>,
    cursor_at: String,
    cursor_id: i64,
    review_comments_at: String,
    review_comments_id: i64,
}

/// Each source owns its cursor. A later comment read cannot advance the
/// timeline past an event that appeared after the timeline read.
fn map_item_activity(
    watch: &crate::database::GithubItemWatch,
    entries: &[Value],
    comments: &[Value],
    merged_at: Option<&str>,
) -> Result<TimelineMapping> {
    let mut mapped = map_timeline(watch, entries, Some(&watch.timeline_at), merged_at)?;
    // Apply the stored boundary to update time, replaying ties.
    // A newly published comment can have an older creation time.
    let comments_to_map = comments
        .iter()
        .filter(|comment| {
            comment["updated_at"]
                .as_str()
                .or_else(|| comment_creation_timestamp(comment))
                .is_some_and(|at| !timestamp_precedes(at, &watch.review_comments_at))
        })
        .collect::<Vec<_>>();
    let review_entries =
        [serde_json::json!({"event": "line-commented", "comments": comments_to_map})];
    let review_mapping = map_timeline(watch, &review_entries, None, None)?;
    mapped.events.extend(review_mapping.events);
    for comment in comments {
        let Some(timestamp) = comment["updated_at"]
            .as_str()
            .or_else(|| comment_creation_timestamp(comment))
        else {
            continue;
        };
        let Some(id) = comment["id"].as_i64() else {
            continue;
        };
        if timestamp_millis(timestamp).is_none() {
            bail!("invalid GitHub review comment timestamp");
        }
        if compare_watermark(
            timestamp,
            id as u64,
            &mapped.review_comments_at,
            mapped.review_comments_id as u64,
        )
        .is_gt()
        {
            mapped.review_comments_at = timestamp.to_owned();
            mapped.review_comments_id = id;
        }
    }
    Ok(mapped)
}

/// Whether the timeline has a close after the cursor with no merge before it
/// in the same open interval. Only such a close needs the pull request's
/// merge time to tell a merge from a close.
fn has_close_without_merge(entries: &[Value], since: &str) -> bool {
    let mut merged = false;
    for entry in entries {
        match entry["event"].as_str() {
            Some("merged") => merged = true,
            Some("reopened") => merged = false,
            Some("closed")
                if !merged
                    && comment_creation_timestamp(entry)
                        .is_some_and(|at| !timestamp_precedes(at, since)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// GitHub records the merge time and the closing entry within a second or two
/// of each other.
fn same_moment(left: &str, right: &str) -> bool {
    match (timestamp_millis(left), timestamp_millis(right)) {
        (Some(left), Some(right)) => (left - right).abs() <= MERGE_CLOSE_TOLERANCE_MS,
        _ => false,
    }
}

/// Entry payloads supply actors, comment text, review state and event IDs.
/// Watch metadata supplies only the item's identity and display title.
fn map_timeline(
    watch: &crate::database::GithubItemWatch,
    entries: &[Value],
    since: Option<&str>,
    merged_at: Option<&str>,
) -> Result<TimelineMapping> {
    let mut result = TimelineMapping {
        events: Vec::new(),
        cursor_at: watch.timeline_at.clone(),
        cursor_id: watch.timeline_id,
        review_comments_at: watch.review_comments_at.clone(),
        review_comments_id: watch.review_comments_id,
    };
    let mut merged = false;
    let repo = Some(format!("{}/{}", watch.owner, watch.repo));
    let item_kind = item_kind_from_watch(&watch.kind);
    for entry in entries {
        let event_type = entry["event"].as_str().unwrap_or_default();
        // GitHub emits closed after merged, sometimes in a later second.
        // A reopen starts a new lifecycle interval.
        if event_type == "merged" {
            merged = true;
        }
        if event_type == "reopened" {
            merged = false;
        }
        let values = if event_type == "line-commented" {
            entry["comments"]
                .as_array()
                .context("GitHub line-commented entry has no comments")?
                .iter()
                .collect::<Vec<_>>()
        } else {
            vec![entry]
        };
        for value in values {
            let Some(timestamp) = comment_creation_timestamp(value) else {
                continue;
            };
            let Some(id) = value["id"].as_i64() else {
                continue;
            };
            if timestamp_millis(timestamp).is_none() {
                bail!("invalid GitHub timeline timestamp");
            }
            if compare_watermark(
                timestamp,
                id as u64,
                &result.cursor_at,
                result.cursor_id as u64,
            )
            .is_gt()
            {
                result.cursor_at = timestamp.to_owned();
                result.cursor_id = id;
            }
            // IDs belong to different GitHub tables. Replay the boundary second
            // and let the outbox deduplicate, so a lower ID cannot hide an event.
            if since.is_some_and(|boundary| timestamp_precedes(timestamp, boundary)) {
                continue;
            }
            let author = value["user"]["login"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let body = truncate_utf8(
                value["body"].as_str().unwrap_or_default(),
                MAILBOX_COMMENT_BODY_LIMIT,
            )
            .to_owned();
            let (suffix, body, authored) = match event_type {
                "commented" => (
                    "comment",
                    MailboxEventBody::GithubComment {
                        item_kind,
                        number: watch.number as u64,
                        title: watch.title.clone(),
                        author: author.clone(),
                        body,
                        repo: repo.clone(),
                    },
                    true,
                ),
                "reviewed" => (
                    "review",
                    MailboxEventBody::GithubReview {
                        item_kind,
                        number: watch.number as u64,
                        title: watch.title.clone(),
                        author: author.clone(),
                        body,
                        review_state: value["state"].as_str().map(str::to_owned),
                        repo: repo.clone(),
                    },
                    true,
                ),
                "line-commented" => (
                    "review-comment",
                    MailboxEventBody::GithubReviewComment {
                        item_kind,
                        number: watch.number as u64,
                        title: watch.title.clone(),
                        author: author.clone(),
                        body,
                        repo: repo.clone(),
                    },
                    true,
                ),
                "merged" | "closed" | "reopened" if watch.kind == "pull_request" => {
                    if event_type == "closed" && merged {
                        continue;
                    }
                    // Compare times so an earlier close, before a reopen and a
                    // later merge, still reads as a close.
                    let (suffix, change) = match event_type {
                        "merged" => ("merged", MailboxPullRequestChange::Merged),
                        "closed" if merged_at.is_some_and(|at| same_moment(at, timestamp)) => {
                            ("merged", MailboxPullRequestChange::Merged)
                        }
                        "closed" => ("closed", MailboxPullRequestChange::ClosedWithoutMerging),
                        _ => ("reopened", MailboxPullRequestChange::Reopened),
                    };
                    (
                        suffix,
                        MailboxEventBody::GithubPullRequestLifecycle {
                            change,
                            number: watch.number as u64,
                            title: watch.title.clone(),
                            actor: value["actor"]["login"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                            repo: repo.clone(),
                        },
                        false,
                    )
                }
                _ => continue,
            };
            result.events.push(TimelineEvent {
                key: format!(
                    "github:{}/{}#{}:{suffix}:{id}",
                    watch.owner, watch.repo, watch.number
                ),
                timestamp: timestamp.to_owned(),
                author: authored.then_some(author),
                body,
            });
        }
    }
    Ok(result)
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

fn parse_github_item(value: &Value) -> Option<crate::database::GithubSeenItem> {
    Some(crate::database::GithubSeenItem {
        number: value["number"].as_i64()?,
        kind: if value.get("pull_request").is_some() {
            "pull_request"
        } else {
            "issue"
        }
        .to_owned(),
        title: value["title"].as_str().unwrap_or_default().to_owned(),
        body: value["body"].as_str().unwrap_or_default().to_owned(),
        author: value["user"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        url: value["html_url"].as_str().unwrap_or_default().to_owned(),
        created_at: value["created_at"].as_str()?.to_owned(),
        updated_at: value["updated_at"].as_str()?.to_owned(),
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
            (timestamp - chrono::Duration::seconds(1)).to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| timestamp.to_owned())
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

    async fn array_pages(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        Ok(self.array_pages_observed(path, query).await?.0)
    }

    async fn array_pages_observed(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<(Vec<Value>, String)> {
        let mut observed_at = None;
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
            if observed_at.is_none() {
                observed_at = Some(response.observed_at);
            }
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
        Ok((
            values,
            observed_at.context("GitHub paging did not fetch a page")?,
        ))
    }

    async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let request = self
            .client
            .get(format!("{}/{path}", self.base))
            .query(query);
        let response = self.get(request).await?;
        ensure!(
            !response.not_modified,
            "unexpected 304 for GitHub resource {path}"
        );
        Ok(response.value)
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
        let user = self.get_json("user", &[]).await;
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
            Err(error) if is_github_events_disabled(&error) => Err(error),
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
        let observed_at = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|value| value.to_str().ok())
            .and_then(|date| DateTime::parse_from_rfc2822(date).ok())
            .map(|date| date.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        // Date may follow the response snapshot, and local clocks may be ahead.
        let observed_at = (observed_at - chrono::Duration::seconds(30))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        let status = response.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(GithubResponse {
                value: Value::Null,
                not_modified: true,
                observed_at,
            });
        }
        if !status.is_success() {
            return Err(anyhow::Error::new(GithubHttpStatus {
                status,
                observed_at,
            }));
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
            not_modified: false,
            observed_at,
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
        let now = Utc::now();
        if let Some(retry_at) =
            github_rate_limit_retry_at(response.status(), response.headers(), now)
        {
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

/// When a GitHub API response is a rate-limit refusal, the moment it may be
/// retried; `None` when it is not one. Covers the primary limit (remaining
/// requests exhausted), HTTP 429, and secondary limits, which GitHub signals
/// with `Retry-After` while requests remain.
pub(crate) fn github_rate_limit_retry_at(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
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
                .map(|seconds| now + chrono::Duration::seconds(seconds.min(i64::MAX as u64) as i64))
                .or_else(|| {
                    DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(|date| date.with_timezone(&Utc))
                })
        });
    let exhausted = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "0");
    (exhausted || status == reqwest::StatusCode::TOO_MANY_REQUESTS || retry_after.is_some()).then(
        || {
            retry_after
                .into_iter()
                .chain(reset)
                .max()
                .unwrap_or_else(|| now + chrono::Duration::minutes(1))
        },
    )
}

#[derive(Debug)]
struct GithubHttpStatus {
    status: reqwest::StatusCode,
    observed_at: String,
}
impl std::fmt::Display for GithubHttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub REST API returned {}", self.status)
    }
}
impl std::error::Error for GithubHttpStatus {}

struct GithubResponse {
    observed_at: String,
    value: Value,
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
        // The feed returns an item newer than this cursor, so recording it
        // after the opt-out would move the cursor.
        let before = crate::database::github_repo_cursor("acme", "repo")
            .unwrap()
            .updated_at;

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
        let error = poll_repository_with_context(
            "acme",
            "repo",
            vec![session],
            classifier,
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(is_github_events_disabled(&error));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "Jev is not called after opt-out"
        );
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        assert!(is_github_events_disabled(
            &api.get_json("ping", &[]).await.unwrap_err()
        ));
        assert_eq!(
            state.requests.load(Ordering::SeqCst),
            1,
            "no request follows opt-out"
        );
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        assert_eq!(cursor.updated_at, before);

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
            assert!(limited_api.get_json(path, &[]).await.is_err());
            let second_repository_api = GithubApi::new(&base, Some(token.into())).unwrap();
            let blocked = tokio::time::timeout(
                Duration::from_millis(150),
                second_repository_api.get_json("ok", &[]),
            )
            .await;
            assert!(
                blocked.is_err(),
                "the credential deadline must block requests"
            );
            assert_eq!(state.accepted.load(Ordering::SeqCst), 0);
            tokio::time::timeout(
                Duration::from_secs(3),
                second_repository_api.get_json("ok", &[]),
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

    #[derive(Clone, Default)]
    struct FakeGithub {
        data: Arc<tokio::sync::Mutex<FakeGithubData>>,
    }

    #[derive(Default)]
    struct FakeGithubData {
        items: Vec<Value>,
        timelines: BTreeMap<u64, Vec<Value>>,
        review_comments: BTreeMap<u64, Vec<Value>>,
        comment_queries: Vec<(u64, String)>,
        fail: BTreeSet<u64>,
        gone: BTreeSet<u64>,
        feed_queries: Vec<HashMap<String, String>>,
        timeline_requests: Vec<u64>,
        observed_at: Option<String>,
        move_after_feed: Option<(usize, usize, String)>,
        timeline_after_read: Option<(u64, Value)>,
        bulk_moves_after_feed: Vec<(usize, Vec<usize>, String)>,
        fail_feed_request: Option<usize>,
        pulls: BTreeMap<u64, Value>,
        pull_requests: Vec<u64>,
    }

    async fn fake_issues(
        State(fake): State<FakeGithub>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        let mut data = fake.data.lock().await;
        data.feed_queries.push(query.clone());
        if data.fail_feed_request == Some(data.feed_queries.len()) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        assert_eq!(query["state"], "all");
        assert_eq!(query["sort"], "updated");
        assert_eq!(query["direction"], "asc");
        let page: usize = query["page"].parse().unwrap();
        let mut items = data
            .items
            .iter()
            // GitHub's `since` includes items updated at exactly that time.
            .filter(|item| {
                !timestamp_precedes(item["updated_at"].as_str().unwrap(), &query["since"])
            })
            .cloned()
            .collect::<Vec<_>>();
        items.sort_by_key(|item| item["updated_at"].as_str().unwrap().to_owned());
        let values = items
            .into_iter()
            .skip((page - 1) * ITEMS_PER_PAGE)
            .take(ITEMS_PER_PAGE)
            .collect::<Vec<_>>();
        if data
            .move_after_feed
            .as_ref()
            .is_some_and(|(request, _, _)| *request == data.feed_queries.len())
        {
            let (_, index, updated_at) = data.move_after_feed.take().unwrap();
            data.items[index]["updated_at"] = json!(updated_at);
        }
        let request = data.feed_queries.len();
        let moves = std::mem::take(&mut data.bulk_moves_after_feed);
        for (after, indices, updated_at) in moves {
            if after == request {
                for index in indices {
                    data.items[index]["updated_at"] = json!(updated_at);
                }
            } else {
                data.bulk_moves_after_feed
                    .push((after, indices, updated_at));
            }
        }
        Json(values).into_response()
    }

    async fn fake_timeline(
        State(fake): State<FakeGithub>,
        Path((_, _, number)): Path<(String, String, u64)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        let mut data = fake.data.lock().await;
        data.timeline_requests.push(number);
        if data.fail.contains(&number) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if data.gone.contains(&number) {
            return StatusCode::GONE.into_response();
        }
        let page: usize = query["page"].parse().unwrap();
        let values = data
            .timelines
            .get(&number)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .skip((page - 1) * ITEMS_PER_PAGE)
            .take(ITEMS_PER_PAGE)
            .collect::<Vec<_>>();
        let mut response = Json(values).into_response();
        if let Some(at) = &data.observed_at {
            let date = DateTime::parse_from_rfc3339(at)
                .unwrap()
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string();
            response
                .headers_mut()
                .insert(header::DATE, HeaderValue::from_str(&date).unwrap());
        }
        // Make a new event visible only after taking this response snapshot.
        if let Some((target, event)) = data.timeline_after_read.take() {
            data.timelines.entry(target).or_default().push(event);
        }
        response
    }

    async fn fake_pull(
        State(fake): State<FakeGithub>,
        Path((_, _, number)): Path<(String, String, u64)>,
    ) -> Response {
        let mut data = fake.data.lock().await;
        data.pull_requests.push(number);
        match data.pulls.get(&number) {
            Some(pull) => Json(pull.clone()).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn fake_review_comments(
        State(fake): State<FakeGithub>,
        Path((_, _, number)): Path<(String, String, u64)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        let mut data = fake.data.lock().await;
        data.comment_queries.push((number, query["since"].clone()));
        let page: usize = query["page"].parse().unwrap();
        let comments = data
            .review_comments
            .get(&number)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|comment| {
                timestamp_precedes(&query["since"], comment["updated_at"].as_str().unwrap())
            })
            .skip((page - 1) * ITEMS_PER_PAGE)
            .take(ITEMS_PER_PAGE)
            .collect::<Vec<_>>();
        Json(comments).into_response()
    }

    fn watch_classification(
        number: i64,
        created_at: &str,
    ) -> crate::database::GithubItemClassification {
        crate::database::GithubItemClassification {
            owner: "acme".into(),
            repo: "repo".into(),
            number,
            session_id: "creator".into(),
            kind: "pull_request".into(),
            title: "synthetic PR".into(),
            url: "https://github.com/acme/repo/pull/1".into(),
            created_at: created_at.into(),
            interested: true,
            created: true,
        }
    }

    // Hard-won: 0477819ab: a regenerated merge body rolled back all repository progress.
    #[tokio::test]
    async fn watcher_records_pages_and_retries_items_without_blocking_other_work() {
        if !run_isolated_child(
            "watcher_records_pages_and_retries_items_without_blocking_other_work",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let initial = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let parsed = DateTime::parse_from_rfc3339(&initial.watch_started_at).unwrap();
        let start = DateTime::from_timestamp(parsed.timestamp() + 1, 0).unwrap();
        let created =
            (start + chrono::Duration::seconds(2)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let updated =
            (start + chrono::Duration::seconds(5)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let later =
            (start + chrono::Duration::seconds(10)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let comment_updated =
            (start + chrono::Duration::seconds(12)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            // A full page and a partial page share the same update timestamp.
            data.items = (1..=101)
                .map(|n| {
                    let mut item =
                        github_item(n, n, "interesting synthetic item", created.clone(), true);
                    item["updated_at"] = json!(updated);
                    item
                })
                .collect();
            data.observed_at = Some(
                (DateTime::parse_from_rfc3339(&updated).unwrap() + chrono::Duration::seconds(30))
                    .to_rfc3339(),
            );
            data.fail.insert(1);
            data.gone.insert(3);
            // GitHub exposes the review but omits its line comments from the timeline.
            data.review_comments.insert(2, vec![json!({"id":8,"created_at":updated,"updated_at":updated,"user":{"login":"watcher-bot"},"body":"line comment"})]);
            data.timelines.insert(2, vec![
                json!({"event":"merged","id":200,"created_at":updated,"actor":{"login":"merge-bot"}}),
                json!({"event":"closed","id":201,"created_at":later,"actor":{"login":"merge-bot"}}),
                json!({"event":"commented","id":10,"created_at":updated,"user":{"login":"watcher-bot"},"body":"original"}),
                json!({"event":"reviewed","id":9,"submitted_at":updated,"user":{"login":"reviewer"},"state":"approved","body":"review"}),
            ]);
        }
        {
            let mut data = fake.data.lock().await;
            let timeline = data.timelines.get_mut(&2).unwrap();
            let mut first_page = (1000..1100)
                .map(|id| json!({"event":"labeled","id":id,"created_at":created}))
                .collect::<Vec<_>>();
            first_page.append(timeline);
            *timeline = first_page;
        }
        for n in 1..=3 {
            crate::database::commit_github_item_classification(
                watch_classification(n, &created),
                None,
            )
            .unwrap();
        }
        let mut creator = crate::database::test_session("creator", "project");
        creator.created_at = initial.watch_started_at.clone();
        crate::database::save_session(&creator).unwrap();
        crate::database::seed_github_test_turns(
            "creator",
            start.timestamp_millis() - 5000,
            start.timestamp_millis() - 1000,
            start.timestamp_millis(),
        )
        .unwrap();
        let mut new_session = crate::database::test_session("new-session", "project");
        new_session.created_at = later.clone();
        crate::database::save_session(&new_session).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/repos/{owner}/{repo}/issues", get(fake_issues))
            .route(
                "/repos/{owner}/{repo}/issues/{number}/timeline",
                get(fake_timeline),
            )
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/comments",
                get(fake_review_comments),
            )
            .route(
                "/user",
                get(|| async { Json(json!({"login":"watcher-bot"})) }),
            )
            .with_state(fake.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = GithubApi::new(&base, Some("synthetic-watcher-items".into())).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let classifier: Arc<dyn GithubClassifier> = Arc::new(FakeClassifier {
            calls: calls.clone(),
        });
        let stop = CancellationToken::new();
        poll_repository_with_context(
            "acme",
            "repo",
            vec![new_session.clone()],
            classifier.clone(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "session must not classify items older than itself"
        );
        assert_eq!(
            crate::database::load_github_seen_items("acme", "repo")
                .unwrap()
                .len(),
            101
        );
        assert_eq!(
            crate::database::github_repo_cursor("acme", "repo")
                .unwrap()
                .updated_at,
            updated
        );
        let watches = crate::database::load_github_watches("acme", "repo").unwrap();
        assert_eq!(
            watches[0].processed_at, created,
            "failed item keeps its progress"
        );
        assert_eq!(watches[1].processed_at, updated);
        assert_eq!(
            watches[2].recorded_updated_at.as_deref(),
            Some(updated.as_str()),
            "gone item retains its recorded update"
        );
        assert_eq!(
            fake.data.lock().await.comment_queries[0],
            (2, overlap_timestamp(&created)),
            "review comments use the stored cursor even when the timeline advances it"
        );
        let rows = crate::database::pending_mailbox_events(1000).unwrap();
        assert_eq!(rows.len(), 4);
        let events = rows
            .iter()
            .map(|row| serde_json::from_str::<MailboxEvent>(&row.event_json).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.body,
                    MailboxEventBody::GithubPullRequestLifecycle { .. }
                ))
                .count(),
            1
        );
        let merge = events
            .iter()
            .find(|event| event.key.ends_with(":merged:200"))
            .unwrap();
        assert!(
            matches!(&merge.body, MailboxEventBody::GithubPullRequestLifecycle { actor, repo: Some(repo), .. } if actor == "merge-bot" && repo == "acme/repo")
        );
        assert!(
            !events
                .iter()
                .find(|event| event.key.ends_with(":comment:10"))
                .unwrap()
                .wake
        );
        assert!(
            !events
                .iter()
                .find(|event| event.key.ends_with(":review-comment:8"))
                .unwrap()
                .wake
        );
        assert!(
            events
                .iter()
                .find(|event| event.key.ends_with(":review:9"))
                .unwrap()
                .wake
        );
        {
            let mut data = fake.data.lock().await;
            data.fail.clear();
            data.observed_at = Some(
                (DateTime::parse_from_rfc3339(&later).unwrap() + chrono::Duration::seconds(30))
                    .to_rfc3339(),
            );
            // The cursor boundary can acquire another item with a lower ID.
            let mut item = github_item(
                500,
                102,
                "interesting boundary item",
                created.clone(),
                false,
            );
            item["updated_at"] = json!(updated);
            data.items.push(item);
            data.items[1]["updated_at"] = json!(later);
            data.timelines.get_mut(&2).unwrap()[102]["body"] = json!("edited body");
            data.review_comments.get_mut(&2).unwrap()[0]["body"] = json!("edited line comment");
            data.review_comments.get_mut(&2).unwrap()[0]["updated_at"] = json!(comment_updated);
            data.timelines.get_mut(&2).unwrap()[100]["actor"] = json!({"login":"changed-bot"});
        }
        // Explicitly replay the same IDs with changed bodies. The first values win.
        let mut replay = watches[1].clone();
        replay.timeline_at = created.clone();
        replay.timeline_id = 0;
        replay.review_comments_at = created.clone();
        replay.review_comments_id = 0;
        replay.recorded_updated_at = Some(updated.clone());
        poll_item_timeline(replay, &api).await.unwrap();
        poll_repository_with_context(
            "acme",
            "repo",
            vec![new_session],
            classifier.clone(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::database::load_github_seen_items("acme", "repo")
                .unwrap()
                .len(),
            102
        );
        assert_eq!(
            crate::database::pending_mailbox_events(1000).unwrap(),
            rows,
            "repeated keys preserve their original bodies"
        );
        assert_eq!(
            crate::database::load_github_watches("acme", "repo").unwrap()[0].processed_at,
            later
        );
        assert_eq!(
            crate::database::load_github_watches("acme", "repo").unwrap()[1].review_comments_at,
            comment_updated,
            "review comments advance only their own cursor"
        );
        assert_eq!(
            fake.data
                .lock()
                .await
                .comment_queries
                .iter()
                .filter(|(number, _)| *number == 2)
                .count(),
            3,
            "the second repository poll fetches the edited line comment again"
        );
        // Older sessions process the backlog in bounded batches, independent of feed cursor.
        poll_repository_with_context(
            "acme",
            "repo",
            vec![creator.clone()],
            classifier.clone(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), MAX_CLASSIFICATIONS_PER_POLL);
        poll_repository_with_context("acme", "repo", vec![creator], classifier, &api, stop)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 99);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    async fn start_fake_github(fake: FakeGithub) -> (GithubApi, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/repos/{owner}/{repo}/issues", get(fake_issues))
            .route(
                "/repos/{owner}/{repo}/issues/{number}/timeline",
                get(fake_timeline),
            )
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/comments",
                get(fake_review_comments),
            )
            .route("/repos/{owner}/{repo}/pulls/{number}", get(fake_pull))
            .route(
                "/user",
                get(|| async { Json(json!({"login":"watcher-bot"})) }),
            )
            .with_state(fake);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (GithubApi::new(&base, Some(base.clone())).unwrap(), server)
    }

    fn fake_classifier() -> Arc<dyn GithubClassifier> {
        Arc::new(FakeClassifier {
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    // Hard-won: review f2c1683: an item moving between offset pages hid its neighbor.
    #[tokio::test]
    async fn watcher_keyset_recording_keeps_items_moving_between_pages() {
        if !run_isolated_child("watcher_keyset_recording_keeps_items_moving_between_pages") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let start = DateTime::parse_from_rfc3339(&cursor.updated_at).unwrap();
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            data.items = (1..=201)
                .map(|n| {
                    github_item(
                        n,
                        n,
                        "synthetic",
                        (start + chrono::Duration::seconds(n as i64)).to_rfc3339(),
                        false,
                    )
                })
                .collect();
            data.move_after_feed =
                Some((1, 0, (start + chrono::Duration::seconds(300)).to_rfc3339()));
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        record_repository("acme", "repo", &cursor, &api, &CancellationToken::new())
            .await
            .unwrap();
        let seen = crate::database::load_github_seen_items("acme", "repo").unwrap();
        assert_eq!(seen.len(), 201);
        assert!(
            seen.iter().any(|item| item.number == 101),
            "the moving item's neighbor must not be skipped"
        );
        assert!(
            fake.data
                .lock()
                .await
                .feed_queries
                .iter()
                .all(|query| query["page"] == "1")
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: 5f5e74082: an inclusive `since` refetched a full page straddling two seconds and stopped.
    #[tokio::test]
    async fn watcher_pages_past_a_full_page_that_straddles_two_seconds() {
        if !run_isolated_child("watcher_pages_past_a_full_page_that_straddles_two_seconds") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let start = DateTime::from_timestamp(
            DateTime::parse_from_rfc3339(&cursor.updated_at)
                .unwrap()
                .timestamp()
                + 1,
            0,
        )
        .unwrap();
        let at = |seconds| {
            (start + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            data.items = (1..=200)
                .map(|n| {
                    let mut item = github_item(n, n, "synthetic", at(1), false);
                    item["updated_at"] = json!(if n <= 50 { at(1) } else { at(2) });
                    item
                })
                .collect();
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        record_repository("acme", "repo", &cursor, &api, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            crate::database::load_github_seen_items("acme", "repo")
                .unwrap()
                .len(),
            200
        );
        assert_eq!(fake.data.lock().await.feed_queries.len(), 3);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[derive(Clone, Copy)]
    enum ReconciliationRecovery {
        OldBacklog,
        LongPause,
        FailedPass,
    }

    async fn check_reconciliation_checkpoint_recovery(scenario: ReconciliationRecovery) {
        let _writer = crate::database::install_isolated_test_writer();
        let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
        let origin = now - chrono::Duration::hours(4);
        let at = |seconds| {
            (origin + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let (tied_at, moved_at, mut read_at, prefix_count) = match scenario {
            ReconciliationRecovery::OldBacklog => (at(3600), at(14400), now, 0),
            ReconciliationRecovery::LongPause => {
                (at(1), at(2), origin + chrono::Duration::seconds(10802), 0)
            }
            ReconciliationRecovery::FailedPass => {
                (at(5400), at(5401), origin + chrono::Duration::hours(3), 100)
            }
        };
        crate::database::github_repo_cursor("acme", "repo").unwrap();
        crate::database::seed_github_repo_start("acme", "repo", &at(0)).unwrap();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let mut watch = watch_classification(201, &tied_at);
        watch.kind = "issue".into();
        crate::database::commit_github_item_classification(watch, None).unwrap();
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            // Distinct prefix times let the failing pass commit a useful checkpoint
            // before the timestamp tie containing the skipped item.
            data.items = (0..prefix_count)
                .map(|n| {
                    github_item(
                        1000 + n as u64,
                        1000 + n as u64,
                        "prefix",
                        at(3600 + n as i64),
                        false,
                    )
                })
                .collect();
            data.items
                .extend((1..=401).map(|n| github_item(n, n, "synthetic", tied_at.clone(), false)));
            // Moves land after the tie's second and fourth offset pages. Without a
            // prefix, the first request jumps to the tie and the second rereads it.
            let request_offset = if prefix_count == 0 { 1 } else { 2 };
            data.bulk_moves_after_feed = vec![
                (2 + request_offset, vec![prefix_count], moved_at.clone()),
                (
                    4 + request_offset,
                    (prefix_count + 1..prefix_count + 101).collect(),
                    moved_at.clone(),
                ),
            ];
            data.observed_at = Some((read_at + chrono::Duration::seconds(30)).to_rfc3339());
            data.timelines.insert(201, vec![json!({"event":"commented","id":201,"created_at":tied_at,"user":{"login":"other"},"body":"recovered activity"})]);
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        let stop = CancellationToken::new();
        record_repository("acme", "repo", &cursor, &api, &stop)
            .await
            .unwrap();
        let seen = crate::database::load_github_seen_items("acme", "repo").unwrap();
        assert_eq!(seen.len(), prefix_count + 400);
        assert!(!seen.iter().any(|item| item.number == 201));
        poll_repository_timelines("acme", "repo", &api, &stop)
            .await
            .unwrap();
        assert!(fake.data.lock().await.timeline_requests.is_empty());
        let main_cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        assert_eq!(main_cursor.updated_at, moved_at);
        assert_eq!(main_cursor.reconciled_through, at(0));
        if matches!(scenario, ReconciliationRecovery::FailedPass) {
            let requests = fake.data.lock().await.feed_queries.len();
            fake.data.lock().await.fail_feed_request = Some(requests + 2);
            let error = reconcile_repository("acme", "repo", &api, &stop, read_at)
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("503"), "{error:#}");
            let interrupted = crate::database::github_repo_cursor("acme", "repo").unwrap();
            assert_eq!(interrupted.updated_at, moved_at);
            assert_eq!(interrupted.reconciled_through, at(3699));
            assert!(
                !crate::database::load_github_seen_items("acme", "repo")
                    .unwrap()
                    .iter()
                    .any(|item| item.number == 201)
            );
            let requests = fake.data.lock().await.feed_queries.len();
            fake.data.lock().await.fail_feed_request = None;
            reconcile_repository(
                "acme",
                "repo",
                &api,
                &stop,
                read_at + chrono::Duration::seconds(3599),
            )
            .await
            .unwrap();
            assert_eq!(fake.data.lock().await.feed_queries.len(), requests);
            assert_eq!(
                crate::database::github_repo_cursor("acme", "repo")
                    .unwrap()
                    .reconciled_through,
                interrupted.reconciled_through
            );
            read_at += chrono::Duration::hours(1);
            reconcile_repository("acme", "repo", &api, &stop, read_at)
                .await
                .unwrap();
            assert_eq!(
                fake.data.lock().await.feed_queries[requests]["since"],
                overlap_timestamp(&interrupted.reconciled_through)
            );
        } else {
            // A new reader and a much later clock model catch-up or process restart.
            reconcile_repository("acme", "repo", &api, &stop, read_at)
                .await
                .unwrap();
        }
        let seen = crate::database::load_github_seen_items("acme", "repo").unwrap();
        assert_eq!(seen.len(), prefix_count + 401);
        assert!(seen.iter().any(|item| item.number == 201));
        let reconciled = crate::database::github_repo_cursor("acme", "repo").unwrap();
        assert_eq!(reconciled.updated_at, moved_at);
        let expected_through = match scenario {
            ReconciliationRecovery::OldBacklog => at(10800),
            _ => moved_at,
        };
        assert_eq!(
            timestamp_millis(&reconciled.reconciled_through),
            timestamp_millis(&expected_through)
        );
        assert!(!timestamp_precedes(
            &(read_at - chrono::Duration::hours(1)).to_rfc3339(),
            &reconciled.reconciled_through
        ));
        poll_repository_timelines("acme", "repo", &api, &stop)
            .await
            .unwrap();
        let events = crate::database::pending_mailbox_events(100).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].event_key.ends_with("#201:comment:201"));
        if matches!(scenario, ReconciliationRecovery::FailedPass) {
            // A full tied page can commit exactly the upper bound before its
            // remaining offset pages fail. Equality must not abandon that tie.
            let requests = fake.data.lock().await.feed_queries.len();
            fake.data.lock().await.fail_feed_request = Some(requests + 2);
            read_at += chrono::Duration::hours(1);
            reconcile_repository("acme", "repo", &api, &stop, read_at)
                .await
                .unwrap_err();
            let partial_tie = crate::database::github_repo_cursor("acme", "repo").unwrap();
            assert_eq!(partial_tie.reconciled_through, partial_tie.updated_at);
            let requests = fake.data.lock().await.feed_queries.len();
            fake.data.lock().await.fail_feed_request = None;
            read_at += chrono::Duration::hours(1);
            reconcile_repository("acme", "repo", &api, &stop, read_at)
                .await
                .unwrap();
            let data = fake.data.lock().await;
            assert!(data.feed_queries.len() >= requests + 2);
            assert_eq!(
                data.feed_queries[requests]["since"],
                overlap_timestamp(&partial_tie.reconciled_through)
            );
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: review f2c1683: a catch-up tie was already older than the trailing window.
    #[tokio::test]
    async fn watcher_reconciliation_recovers_skips_in_three_hour_old_backlog() {
        if run_isolated_child("watcher_reconciliation_recovers_skips_in_three_hour_old_backlog") {
            check_reconciliation_checkpoint_recovery(ReconciliationRecovery::OldBacklog).await;
        }
    }

    // Hard-won: review f2c1683: a skipped item aged out while the repository was paused.
    #[tokio::test]
    async fn watcher_reconciliation_recovers_skips_after_a_three_hour_pause() {
        if run_isolated_child("watcher_reconciliation_recovers_skips_after_a_three_hour_pause") {
            check_reconciliation_checkpoint_recovery(ReconciliationRecovery::LongPause).await;
        }
    }

    // Hard-won: review f2c1683: a failed pass lost the next hour's recovery opportunity.
    #[tokio::test]
    async fn watcher_reconciliation_resumes_failed_pass_from_durable_checkpoint() {
        if run_isolated_child("watcher_reconciliation_resumes_failed_pass_from_durable_checkpoint")
        {
            check_reconciliation_checkpoint_recovery(ReconciliationRecovery::FailedPass).await;
        }
    }

    // Hard-won: review f2c1683: a later line-comment read skipped unseen timeline events.
    #[tokio::test]
    async fn watcher_source_cursors_do_not_skip_interleaved_timeline_entries() {
        if !run_isolated_child("watcher_source_cursors_do_not_skip_interleaved_timeline_entries") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let start = DateTime::from_timestamp(
            DateTime::parse_from_rfc3339(&cursor.updated_at)
                .unwrap()
                .timestamp()
                + 1,
            0,
        )
        .unwrap();
        let at = |seconds| {
            (start + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        crate::database::commit_github_item_classification(watch_classification(1, &at(1)), None)
            .unwrap();
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            let mut item = github_item(1, 1, "synthetic", at(1), true);
            item["updated_at"] = json!(at(10));
            data.items.push(item);
            data.observed_at = Some(at(10));
            data.timeline_after_read = Some((
                1,
                json!({"event":"commented","id":800,"created_at":at(20),"user":{"login":"other"},"body":"arrived after timeline read"}),
            ));
            data.review_comments.insert(1,vec![json!({"id":900,"created_at":at(30),"updated_at":at(30),"user":{"login":"other"},"body":"later line comment"})]);
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let watch = crate::database::load_github_watches("acme", "repo")
            .unwrap()
            .remove(0);
        assert_eq!(watch.timeline_at, at(1));
        assert_eq!(watch.review_comments_at, at(30));
        {
            let mut data = fake.data.lock().await;
            data.items[0]["updated_at"] = json!(at(30));
            data.observed_at = Some(at(70));
        }
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let rows = crate::database::pending_mailbox_events(100).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.event_key.ends_with(":comment:800"))
        );
        let watch = crate::database::load_github_watches("acme", "repo")
            .unwrap()
            .remove(0);
        assert_eq!(watch.timeline_at, at(20));
        assert_eq!(watch.review_comments_at, at(30));
        assert_eq!(watch.processed_at, at(40));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: b4bbe4016: Merge Marshall merges left only closed entries, and
    // agents were told their merged pull requests closed without merging.
    #[tokio::test]
    async fn watcher_reports_a_close_at_the_merge_time_as_a_merge() {
        if !run_isolated_child("watcher_reports_a_close_at_the_merge_time_as_a_merge") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let start = DateTime::from_timestamp(
            DateTime::parse_from_rfc3339(&cursor.updated_at)
                .unwrap()
                .timestamp()
                + 1,
            0,
        )
        .unwrap();
        let at = |seconds| {
            (start + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            for n in [11, 12, 13, 14] {
                crate::database::commit_github_item_classification(
                    watch_classification(n, &at(1)),
                    None,
                )
                .unwrap();
                let mut item = github_item(n as u64, n as u64, "synthetic", at(1), true);
                item["updated_at"] = json!(at(30));
                data.items.push(item);
            }
            data.observed_at = Some(at(90));
            let closed = |id: i64, seconds: i64| json!({"event":"closed","id":id,"created_at":at(seconds),"actor":{"login":"mergemarshall[bot]"},"commit_id":"abc"});
            // Merged by landing commits: only a closed entry, one second after the merge time.
            data.timelines.insert(11, vec![closed(300, 6)]);
            data.pulls.insert(11, json!({"merged_at": at(5)}));
            // Closed without merging.
            data.timelines.insert(12, vec![closed(301, 6)]);
            data.pulls.insert(12, json!({"merged_at": null}));
            // Closed, reopened, then merged later: the first close stays a close.
            data.timelines.insert(13, vec![
                closed(302, 6),
                json!({"event":"reopened","id":303,"created_at":at(10),"actor":{"login":"author"}}),
                json!({"event":"merged","id":304,"created_at":at(20),"actor":{"login":"merger"}}),
                closed(305, 20),
            ]);
            data.pulls.insert(13, json!({"merged_at": at(20)}));
            // A recorded merge followed by its close needs no pull request read.
            data.timelines.insert(14, vec![
                json!({"event":"merged","id":306,"created_at":at(7),"actor":{"login":"merger"}}),
                closed(307, 7),
            ]);
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let rows = crate::database::pending_mailbox_events(100).unwrap();
        let mut changes = rows
            .iter()
            .map(|row| {
                let event: MailboxEvent = serde_json::from_str(&row.event_json).unwrap();
                let MailboxEventBody::GithubPullRequestLifecycle { change, actor, .. } = event.body
                else {
                    panic!("unexpected event {}", row.event_key);
                };
                (row.event_key.clone(), change, actor)
            })
            .collect::<Vec<_>>();
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            changes,
            vec![
                (
                    "github:acme/repo#11:merged:300".to_owned(),
                    MailboxPullRequestChange::Merged,
                    "mergemarshall[bot]".to_owned(),
                ),
                (
                    "github:acme/repo#12:closed:301".to_owned(),
                    MailboxPullRequestChange::ClosedWithoutMerging,
                    "mergemarshall[bot]".to_owned(),
                ),
                (
                    "github:acme/repo#13:closed:302".to_owned(),
                    MailboxPullRequestChange::ClosedWithoutMerging,
                    "mergemarshall[bot]".to_owned(),
                ),
                (
                    "github:acme/repo#13:merged:304".to_owned(),
                    MailboxPullRequestChange::Merged,
                    "merger".to_owned(),
                ),
                (
                    "github:acme/repo#13:reopened:303".to_owned(),
                    MailboxPullRequestChange::Reopened,
                    "author".to_owned(),
                ),
                (
                    "github:acme/repo#14:merged:306".to_owned(),
                    MailboxPullRequestChange::Merged,
                    "merger".to_owned(),
                ),
            ]
        );
        let mut read = fake.data.lock().await.pull_requests.clone();
        read.sort_unstable();
        assert_eq!(read, vec![11, 12, 13]);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: review f2c1683: equality at the processed second suppressed later activity.
    #[tokio::test]
    async fn watcher_rechecks_same_second_updates_then_converges() {
        if !run_isolated_child("watcher_rechecks_same_second_updates_then_converges") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let start = DateTime::from_timestamp(
            DateTime::parse_from_rfc3339(&cursor.updated_at)
                .unwrap()
                .timestamp()
                + 1,
            0,
        )
        .unwrap();
        let at = |seconds| {
            (start + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let mut classification = watch_classification(1, &at(1));
        classification.kind = "issue".into();
        crate::database::commit_github_item_classification(classification, None).unwrap();
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            let mut item = github_item(1, 1, "synthetic", at(1), false);
            item["updated_at"] = json!(at(10));
            data.items.push(item);
            data.observed_at = Some(at(10));
            data.timelines.insert(1,vec![json!({"event":"commented","id":1,"created_at":at(10),"user":{"login":"other"},"body":"first"})]);
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::database::load_github_watches("acme", "repo").unwrap()[0].processed_at,
            at(-20)
        );
        {
            let mut data = fake.data.lock().await;
            data.timelines.get_mut(&1).unwrap().push(json!({"event":"commented","id":2,"created_at":at(10),"user":{"login":"other"},"body":"same-second update"}));
            data.observed_at = Some(at(41));
        }
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::database::pending_mailbox_events(100).unwrap().len(),
            2
        );
        assert_eq!(
            fake.data.lock().await.timeline_requests.len(),
            2,
            "one extra read catches the update and the later Date ends replay"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: review f2c1683: Date after the snapshot hid an update between them.
    #[tokio::test]
    async fn watcher_processed_margin_recovers_updates_between_snapshot_and_date() {
        if !run_isolated_child(
            "watcher_processed_margin_recovers_updates_between_snapshot_and_date",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let start = DateTime::from_timestamp(
            DateTime::parse_from_rfc3339(&cursor.updated_at)
                .unwrap()
                .timestamp()
                + 1,
            0,
        )
        .unwrap();
        let at = |seconds| {
            (start + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let mut watch = watch_classification(1, &at(1));
        watch.kind = "issue".into();
        crate::database::commit_github_item_classification(watch, None).unwrap();
        let fake = FakeGithub::default();
        {
            let mut data = fake.data.lock().await;
            let mut item = github_item(1, 1, "synthetic", at(1), false);
            item["updated_at"] = json!(at(10));
            data.items.push(item);
            data.observed_at = Some(at(12));
            data.timeline_after_read = Some((
                1,
                json!({"event":"commented","id":11,"created_at":at(11),"user":{"login":"other"},"body":"between snapshot and Date"}),
            ));
        }
        let (api, server) = start_fake_github(fake.clone()).await;
        let stop = CancellationToken::new();
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        assert!(
            crate::database::pending_mailbox_events(100)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            crate::database::load_github_watches("acme", "repo").unwrap()[0].processed_at,
            at(-18)
        );
        {
            let mut data = fake.data.lock().await;
            data.items[0]["updated_at"] = json!(at(11));
            data.observed_at = Some(at(42));
        }
        poll_repository_with_context(
            "acme",
            "repo",
            vec![],
            fake_classifier(),
            &api,
            stop.clone(),
        )
        .await
        .unwrap();
        poll_repository_with_context("acme", "repo", vec![], fake_classifier(), &api, stop)
            .await
            .unwrap();
        let events = crate::database::pending_mailbox_events(100).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].event_key.ends_with(":comment:11"));
        assert_eq!(fake.data.lock().await.timeline_requests.len(), 2);
        assert_eq!(
            crate::database::load_github_watches("acme", "repo").unwrap()[0].processed_at,
            at(12)
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    struct FailingSessionClassifier(Arc<AtomicBool>);
    impl GithubClassifier for FailingSessionClassifier {
        fn classify<'a>(&'a self, evidence: &'a GithubItemEvidence) -> ClassifyFuture<'a> {
            Box::pin(async move {
                if evidence.session.recent_turns.contains("FAIL_PAIR")
                    && self.0.load(Ordering::SeqCst)
                {
                    bail!("permanent session classifier failure");
                }
                Ok(GithubItemVerdict {
                    interested: false,
                    created: false,
                })
            })
        }
    }

    // Hard-won: review f2c1683: repeated failures consumed every poll before item 51.
    #[tokio::test]
    async fn watcher_classification_backoff_does_not_starve_healthy_sessions() {
        if !run_isolated_child("watcher_classification_backoff_does_not_starve_healthy_sessions") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let created = (DateTime::parse_from_rfc3339(&cursor.watch_started_at).unwrap()
            + chrono::Duration::seconds(1))
        .to_rfc3339();
        let items = (1..=51)
            .map(|n| {
                parse_github_item(&github_item(n, n, "synthetic", created.clone(), false)).unwrap()
            })
            .collect();
        crate::database::record_github_page(
            "acme".into(),
            "repo".into(),
            items,
            crate::database::GithubFeedProgress::Main(created),
        )
        .unwrap();
        let mut sessions = Vec::new();
        for id in ["bad", "healthy"] {
            let mut session = crate::database::test_session(id, "project");
            session.created_at = cursor.watch_started_at.clone();
            crate::database::save_session(&session).unwrap();
            sessions.push(session);
        }
        use mj_core::state::{
            MaterializedExecutionState, MaterializedSession, TranscriptBody, TranscriptItem,
        };
        let materialized = MaterializedSession {
            session_id: "bad".into(),
            applied_event_ordinal: 1,
            applied_event_digest: "1".repeat(64),
            last_activity_at_ms: Some(1),
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: Default::default(),
            transcript: vec![Arc::new(TranscriptItem {
                stable_id: "user:1".into(),
                position: 1,
                latest_content_event_ordinal: None,
                created_at_ms: 1,
                last_changed_at_ms: 1,
                body: TranscriptBody::User {
                    content: vec![json!({"type":"text","text":"FAIL_PAIR"})],
                },
            })],
            queued_prompts: vec![],
            pending_elicitations: vec![],
            active_turn: None,
            last_turn_outcome: None,
        };
        crate::database::save_materialized_session(&materialized).unwrap();
        assert!(
            recent_turns("bad".into())
                .await
                .unwrap()
                .contains("FAIL_PAIR")
        );
        let failing = Arc::new(AtomicBool::new(true));
        let classifier: Arc<dyn GithubClassifier> =
            Arc::new(FailingSessionClassifier(failing.clone()));
        let api =
            GithubApi::new("http://127.0.0.1:1", Some("classification-backoff".into())).unwrap();
        for _ in 0..3 {
            classify_recorded_items(
                "acme",
                "repo",
                &sessions,
                classifier.clone(),
                &cursor.watch_started_at,
                &api,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        }
        assert!(
            crate::database::load_github_item_classified_sessions("acme", "repo", 51)
                .unwrap()
                .contains("healthy")
        );
        let failures =
            crate::database::load_github_classification_failures("acme", "repo").unwrap();
        assert_eq!(failures.len(), 51);
        let failure = &failures[&(1, "bad".into())];
        assert_eq!(failure.attempts, 1);
        assert_eq!(
            failure.next_attempt_at_ms - failure.last_attempt_at_ms,
            60_000
        );
        // Retry durable failures beyond the one-hour cap; they never give up.
        for attempt in 2..=8 {
            crate::database::seed_github_retry_due("acme", "repo", 1, "bad").unwrap();
            classify_recorded_items(
                "acme",
                "repo",
                &sessions,
                classifier.clone(),
                &cursor.watch_started_at,
                &api,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            let failures =
                crate::database::load_github_classification_failures("acme", "repo").unwrap();
            let failure = &failures[&(1, "bad".into())];
            assert_eq!(failure.attempts, attempt);
            assert!(failure.next_attempt_at_ms - failure.last_attempt_at_ms <= 3_600_000);
        }
        failing.store(false, Ordering::SeqCst);
        crate::database::seed_github_retry_due("acme", "repo", 1, "bad").unwrap();
        classify_recorded_items(
            "acme",
            "repo",
            &sessions,
            classifier,
            &cursor.watch_started_at,
            &api,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            !crate::database::load_github_classification_failures("acme", "repo")
                .unwrap()
                .contains_key(&(1, "bad".into()))
        );
        assert!(
            crate::database::load_github_item_classified_sessions("acme", "repo", 1)
                .unwrap()
                .contains("bad")
        );
    }

    // Hard-won: d3fd6f01e: Jev denied items a session's own `gh pr create` opened and claimed bot batches.
    #[tokio::test]
    async fn classification_uses_opening_tool_calls_and_the_credential_author() {
        if !run_isolated_child("classification_uses_opening_tool_calls_and_the_credential_author") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let cursor = crate::database::github_repo_cursor("acme", "repo").unwrap();
        let created = DateTime::parse_from_rfc3339(&cursor.watch_started_at).unwrap()
            + chrono::Duration::seconds(1);
        let created_ms = created.timestamp_millis();
        let created = created.to_rfc3339();
        let mut opened = github_item(1, 1, "interesting synthetic change", created.clone(), true);
        opened["user"]["login"] = json!("Watcher-Bot");
        let mut batch = github_item(2, 2, "interesting created batch", created.clone(), true);
        batch["user"]["login"] = json!("merge-bot[bot]");
        crate::database::record_github_page(
            "acme".into(),
            "repo".into(),
            vec![
                parse_github_item(&opened).unwrap(),
                parse_github_item(&batch).unwrap(),
            ],
            crate::database::GithubFeedProgress::Main(created),
        )
        .unwrap();
        let mut sessions = Vec::new();
        for id in ["opener", "other"] {
            let mut session = crate::database::test_session(id, "project");
            session.created_at = cursor.watch_started_at.clone();
            crate::database::save_session(&session).unwrap();
            sessions.push(session);
        }
        use mj_core::state::{
            MaterializedExecutionState, MaterializedSession, TranscriptBody, TranscriptItem,
        };
        crate::database::save_materialized_session(&MaterializedSession {
            session_id: "opener".into(),
            applied_event_ordinal: 1,
            applied_event_digest: "1".repeat(64),
            last_activity_at_ms: Some(1),
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: Default::default(),
            transcript: vec![Arc::new(TranscriptItem {
                stable_id: "tool:1".into(),
                position: 1,
                latest_content_event_ordinal: None,
                created_at_ms: created_ms - 3000,
                last_changed_at_ms: created_ms + 2000,
                body: TranscriptBody::Tool {
                    call: json!({
                        "toolCallId": "call-1",
                        "title": "Open the pull request",
                        "kind": "execute",
                        "status": "completed",
                        "rawInput": {"command": "gh pr create --title 'Interesting synthetic change' --body-file pr.md"}
                    }),
                    terminal_outputs: vec![],
                    terminal_refs: vec![],
                    presentation: None,
                },
            })],
            queued_prompts: vec![],
            pending_elicitations: vec![],
            active_turn: None,
            last_turn_outcome: None,
        })
        .unwrap();
        let (api, server) = start_fake_github(FakeGithub::default()).await;
        classify_recorded_items(
            "acme",
            "repo",
            &sessions,
            fake_classifier(),
            &cursor.watch_started_at,
            &api,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let watches = crate::database::load_github_watches("acme", "repo").unwrap();
        assert_eq!(
            watches
                .iter()
                .map(|watch| (watch.number, watch.creator_session_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "opener")],
            "the opening call claims item 1; another account's batch has no creator"
        );
        let mut interest = crate::database::pending_mailbox_events(1000)
            .unwrap()
            .into_iter()
            .map(|row| row.event_key)
            .collect::<Vec<_>>();
        interest.sort();
        assert_eq!(
            interest,
            vec![
                "github:acme/repo#1:interest:other",
                "github:acme/repo#2:interest:opener",
                "github:acme/repo#2:interest:other",
            ]
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    /// Run with MJ_GITHUB_DRY_RUN pointing to a private JSON file containing
    /// {since, items:[{item:<issues entry>, timeline:[<timeline entries>]}]}.
    /// This entry point never opens a store or sends a request.
    #[test]
    #[ignore = "requires private read-only GitHub capture"]
    fn github_timeline_read_only_dry_run() {
        let path = std::env::var_os("MJ_GITHUB_DRY_RUN").expect("MJ_GITHUB_DRY_RUN");
        let capture: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut counts = BTreeMap::<String, usize>::new();
        let mut examples = Vec::new();
        for captured in capture["items"].as_array().unwrap() {
            let item = parse_github_item(&captured["item"]).unwrap();
            let watch = crate::database::GithubItemWatch {
                owner: "brokkai".into(),
                repo: "bifrost-dev".into(),
                number: item.number,
                creator_session_id: "dry-run".into(),
                kind: item.kind,
                title: item.title,
                url: item.url,
                created_at: item.created_at,
                timeline_at: capture["since"].as_str().unwrap().into(),
                timeline_id: 0,
                review_comments_at: capture["since"].as_str().unwrap().into(),
                review_comments_id: 0,
                processed_at: String::new(),
                recorded_updated_at: Some(item.updated_at),
            };
            let comments = captured["review_comments"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mapped = map_item_activity(
                &watch,
                captured["timeline"].as_array().unwrap(),
                &comments,
                captured["pull"]["merged_at"].as_str(),
            )
            .unwrap();
            for event in mapped.events {
                let body = serde_json::to_value(&event.body).unwrap();
                let kind = body["type"].as_str().unwrap().to_owned();
                *counts.entry(kind.clone()).or_default() += 1;
                if examples.len() < 12 {
                    // No titles, comment text, review text, or user logins leave this test.
                    examples.push(json!({"key":event.key,"type":kind,"at":event.timestamp}));
                }
            }
        }
        println!(
            "DRY_RUN {}",
            json!({"items":capture["items"].as_array().unwrap().len(),"since":capture["since"],"counts":counts,"examples":examples})
        );
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
            "created_at": created_at,
            "updated_at": created_at
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
