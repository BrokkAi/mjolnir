//! Automatic, non-blocking publication of Mjolnir sessions to Brokk Observe.
//!
//! A revision-driven task converts the daemon's canonical transcript and
//! usage projections into stable Observe envelopes, writes them to the durable
//! local spool, and flushes ordered batches. Observe being unavailable never
//! blocks a session: envelopes stay on disk and the delivery loop backs off.

use super::*;
use crate::observe::{
    ObserveDeliveryState, ObservePaths, ObserveSession, ObserveSpool, envelopes_for_transcript,
    transcript_response_from_materialized, usage_envelopes,
};
use mj_core::observe::{ObserveCommit, ObserveDeliveryConfig, ObserveGitMeta};
use mj_core::state::MaterializedExecutionState;
use serde_json::Value;
use std::time::Instant;
use tokio::task::JoinHandle;

const INITIAL_DELIVERY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_DELIVERY_BACKOFF: Duration = Duration::from_secs(60);

/// Start Observe publication with the daemon. The returned task exits on the
/// daemon cancellation token; pending spool files remain for the next start.
pub(super) fn spawn(
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move { run(state, cancellation).await })
}

async fn run(state: Arc<RuntimeState>, cancellation: CancellationToken) -> Result<()> {
    let paths = ObservePaths::current();
    fs::create_dir_all(&paths.root).with_context(|| {
        format!(
            "create Brokk Observe data directory {}",
            paths.root.display()
        )
    })?;
    let config = match ObserveDeliveryConfig::load() {
        Ok(Some(config)) => config,
        Ok(None) => return Ok(()),
        Err(error) => {
            record_failure(&paths, &error)?;
            tracing::warn!(%error, "automatic Brokk Observe delivery is misconfigured");
            return Ok(());
        }
    };
    let client = reqwest::Client::builder()
        .build()
        .context("build Brokk Observe delivery client")?;
    let mut wiki_changes = state.wiki().changes();
    state.wiki().request_sync(false);
    let mut revisions = state.revisions.subscribe();
    let mut immediate = true;
    let mut backoff = INITIAL_DELIVERY_BACKOFF;

    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if immediate {
            let before = revisions.borrow_and_update().to_owned();
            let export = publish_changed_sessions(&state, &paths, &cancellation).await;
            if let Err(error) = &export {
                tracing::warn!(error = %format!("{error:#}"), "automatic Brokk Observe export stopped early");
            }
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let flush = flush_spool(&paths, &config, &client, &cancellation).await;
            match &flush {
                Ok(delivered) => {
                    if *delivered > 0 {
                        tracing::info!(
                            delivered,
                            "delivered automatically observed Mjolnir events"
                        );
                    }
                    backoff = INITIAL_DELIVERY_BACKOFF;
                }
                Err(error) => {
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        retry_after_ms = backoff.as_millis(),
                        "could not deliver Mjolnir events to Brokk Observe; retained the spool"
                    );
                    backoff = (backoff * 2).min(MAX_DELIVERY_BACKOFF);
                }
            }
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let failed = match (&export, &flush) {
                (Ok(()), Ok(_)) => {
                    record_success(&paths)?;
                    false
                }
                (export, flush) => {
                    let error = match (export, flush) {
                        (Err(export), Err(flush)) => {
                            format!("export: {export:#}; delivery: {flush:#}")
                        }
                        (Err(export), _) => format!("export: {export:#}"),
                        (_, Err(flush)) => format!("delivery: {flush:#}"),
                        (Ok(()), Ok(_)) => unreachable!(),
                    };
                    record_failure(&paths, &anyhow!(error))?;
                    true
                }
            };
            immediate = *revisions.borrow() != before;
            if failed {
                tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(backoff) => immediate = true,
                    changed = revisions.changed() => {
                        if changed.is_err() { return Ok(()); }
                        immediate = true;
                    }
                    changed = wiki_changes.changed() => {
                        if changed.is_err() { return Ok(()); }
                        immediate = true;
                    }
                }
            }
            continue;
        }

        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            changed = revisions.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                immediate = true;
            }
            changed = wiki_changes.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                immediate = true;
            }
        }
    }
}

async fn publish_changed_sessions(
    state: &RuntimeState,
    paths: &ObservePaths,
    cancellation: &CancellationToken,
) -> Result<()> {
    let (runtime_state, config) = {
        let owner = state.owner();
        let controller = owner.controller();
        (controller.state.clone(), controller.config.clone())
    };
    let paths = paths.clone();
    let cancellation = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let now = mj_core::clock::epoch_millis();
        let mut delivery = ObserveDeliveryState::load(&paths.state, now)?;
        let spool = ObserveSpool::open(&paths.spool)?;
        let mut ids = runtime_state
            .sessions
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        match crate::sessionwiki::mjolnir_session_ids() {
            Ok(indexed) => ids.extend(indexed),
            Err(error) => tracing::warn!(
                %error,
                "could not use SessionWiki for Mjolnir Observe discovery"
            ),
        }

        let mut failures = Vec::new();
        let mut emitted = 0_usize;
        for session_id in ids {
            if cancellation.is_cancelled() {
                break;
            }
            let record = match runtime_state.sessions.get(&session_id).cloned() {
                Some(record) => Some(record),
                None => match crate::database::load_session_record(&session_id) {
                    Ok(record) => record,
                    Err(error) => {
                        failures.push(format!("{error:#}"));
                        continue;
                    }
                },
            };
            let wiki_only = record.is_none();
            let published = if let Some(record) = record {
                publish_one(&record, &runtime_state, &config, &spool, &mut delivery).map(Some)
            } else if delivery.sessions.get(&session_id).is_some_and(|state| {
                state.historical_imported_at_ms.is_some()
                    || state.full_source_unavailable_at_ms.is_some()
            }) {
                Ok(None)
            } else {
                publish_checkpoint(&session_id, &spool, &mut delivery)
            };
            match published {
                Ok(Some(count)) => emitted += count,
                Ok(None) => {
                    if wiki_only {
                        delivery
                            .sessions
                            .entry(session_id.clone())
                            .or_default()
                            .full_source_unavailable_at_ms = Some(mj_core::clock::epoch_millis());
                    }
                }
                Err(error) => {
                    failures.push(format!("session {session_id}: {error:#}"));
                    tracing::warn!(
                        session_id,
                        error = %format!("{error:#}"),
                        "could not enqueue one Mjolnir session for Brokk Observe"
                    );
                }
            }
        }
        delivery.save(&paths.state)?;
        if emitted > 0 {
            tracing::debug!(emitted, "enqueued Mjolnir events for Brokk Observe");
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "{} Mjolnir session(s) could not be prepared for Observe: {}",
                failures.len(),
                failures.join("; ")
            )
        }
    })
    .await
    .context("Observe export task panicked")?
}

fn publish_one(
    record: &SessionRecord,
    state: &mj_core::state::State,
    config: &Config,
    spool: &ObserveSpool,
    delivery: &mut ObserveDeliveryState,
) -> Result<usize> {
    let Some(summary) = crate::database::load_materialized_session_summary(&record.id)? else {
        return publish_checkpoint(&record.id, spool, delivery).map(|count| count.unwrap_or(0));
    };
    if delivery.sessions.get(&record.id).is_some_and(|cursor| {
        cursor.record_updated_at == record.updated_at
            && cursor.materialized_event_ordinal == summary.applied_event_ordinal
            && cursor.execution == Some(summary.execution)
    }) {
        return Ok(0);
    }
    let Some(materialized) = crate::database::load_materialized_session(&record.id)? else {
        return publish_checkpoint(&record.id, spool, delivery).map(|count| count.unwrap_or(0));
    };
    let model = materialized
        .configuration
        .values
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cwd =
        crate::sessionwiki::indexed_project_directory(state, Some(config), &record.id, record)
            .ok()
            .flatten()
            .map(|path| path.to_string_lossy().into_owned());
    let parent_id = state
        .subagents
        .get(&record.id)
        .map(|relation| relation.parent_session_id.clone());
    let cursor = delivery.sessions.get(&record.id);
    let initialized_at = delivery.initialized_at_ms;
    let started_at_ms = chrono::DateTime::parse_from_rfc3339(&record.created_at)
        .ok()
        .map_or(0, |time| time.timestamp_millis());
    let historical = cursor.is_none() && started_at_ms < initialized_at;
    let (runtime, worker_id, hostname) = target_observation(record);
    let (git, commits) = git_observation(record);
    let session = ObserveSession {
        id: record.id.clone(),
        parent_id,
        native_session_id: record.native_session_id.clone(),
        worker_id,
        harness: record.harness_kind.id().to_owned(),
        model: model.clone(),
        cwd,
        hostname,
        os: None,
        started_at_ms,
        workspace_id: Some(record.workspace_id.clone()),
        profile_id: Some(record.last_profile.clone()),
        target_id: Some(record.target_template_id.clone()),
        runtime,
        git,
        commits,
        historical,
    };
    let transcript = transcript_response_from_materialized(&record.id, &materialized)?;
    let mut envelopes = envelopes_for_transcript(&session, &transcript)?;
    let mut usage_after = 0_u64;
    loop {
        let Some(page) = crate::database::load_session_usage(&record.id, usage_after, 500)? else {
            break;
        };
        envelopes.extend(usage_envelopes(&session, &page)?);
        if page.next_after_seq <= usage_after || page.next_after_seq >= page.latest_seq {
            break;
        }
        usage_after = page.next_after_seq;
    }

    enqueue_envelopes(
        &record.id,
        envelopes,
        ObserveBatch {
            revision: &record.updated_at,
            materialized_event_ordinal: materialized.applied_event_ordinal,
            execution: materialized.execution,
            historical,
        },
        spool,
        delivery,
    )
}

fn publish_checkpoint(
    session_id: &str,
    spool: &ObserveSpool,
    delivery: &mut ObserveDeliveryState,
) -> Result<Option<usize>> {
    let Some(path) = crate::sessionwiki::newest_checkpoint_archive(session_id)? else {
        tracing::debug!(
            session_id,
            "SessionWiki knows a Mjolnir session whose full checkpoint was already archived"
        );
        return Ok(None);
    };
    let archive = mj_checkpoint::archive::verify_archive_streaming(&path)
        .with_context(|| format!("read Mjolnir checkpoint {}", path.display()))?;
    ensure!(
        archive.manifest.session.id == session_id,
        "checkpoint {} names session {}, not {session_id}",
        path.display(),
        archive.manifest.session.id
    );
    let mut materialized = mj_transcript::projection::materialized_session_from_canonical(
        session_id,
        &archive.canonical_session,
    )
    .with_context(|| format!("materialize checkpoint {}", path.display()))?;
    materialized.execution = MaterializedExecutionState::Closed;
    let model = materialized
        .configuration
        .values
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let primary = &archive.manifest.bundle.primary_repository;
    let repository = archive
        .manifest
        .repositories
        .iter()
        .find(|repository| &repository.metadata.id == primary);
    let git = repository
        .map(|repository| ObserveGitMeta {
            remote: nonempty_string(&repository.metadata.origin),
            branch: repository.metadata.branch.clone(),
            head: nonempty_string(&repository.metadata.head_commit),
        })
        .filter(|git| git.remote.is_some() || git.branch.is_some() || git.head.is_some());
    let native_session_id = nonempty_string(&archive.manifest.session.native_session_id);
    let started_at_ms = chrono::DateTime::parse_from_rfc3339(&archive.manifest.session.created_at)
        .ok()
        .map_or(0, |time| time.timestamp_millis());
    let session = ObserveSession {
        id: session_id.to_owned(),
        parent_id: None,
        native_session_id,
        worker_id: archive.manifest.target.details.get("worker_id").cloned(),
        harness: archive.manifest.session.harness_kind.id().to_owned(),
        model,
        cwd: archive
            .manifest
            .target
            .details
            .get("cwd")
            .or_else(|| archive.manifest.target.details.get("workspace"))
            .cloned(),
        hostname: archive.manifest.target.details.get("hostname").cloned(),
        os: None,
        started_at_ms,
        workspace_id: archive.manifest.target.details.get("workspace_id").cloned(),
        profile_id: nonempty_string(&archive.manifest.session.profile_id),
        target_id: nonempty_string(&archive.manifest.target.template_id),
        runtime: nonempty_string(&archive.manifest.target.target_kind),
        git,
        commits: Vec::new(),
        historical: true,
    };
    let transcript = transcript_response_from_materialized(session_id, &materialized)?;
    let envelopes = envelopes_for_transcript(&session, &transcript)?;
    enqueue_envelopes(
        session_id,
        envelopes,
        ObserveBatch {
            revision: &archive.manifest.session.checkpointed_at,
            materialized_event_ordinal: archive.canonical_session.event_frontier,
            execution: MaterializedExecutionState::Closed,
            historical: true,
        },
        spool,
        delivery,
    )
    .map(Some)
}

fn enqueue_envelopes(
    session_id: &str,
    envelopes: Vec<mj_core::observe::ObserveEnvelope>,
    batch: ObserveBatch<'_>,
    spool: &ObserveSpool,
    delivery: &mut ObserveDeliveryState,
) -> Result<usize> {
    let cursor = delivery.sessions.entry(session_id.to_owned()).or_default();
    let mut count = 0_usize;
    for envelope in envelopes {
        if cursor.emitted_event_ids.contains(&envelope.meta.event_id) {
            continue;
        }
        spool.enqueue(&envelope)?;
        cursor
            .emitted_event_ids
            .insert(envelope.meta.event_id.clone());
        count += 1;
    }
    cursor.record_updated_at = batch.revision.to_owned();
    cursor.materialized_event_ordinal = batch.materialized_event_ordinal;
    cursor.execution = Some(batch.execution);
    if batch.historical && cursor.historical_imported_at_ms.is_none() {
        cursor.historical_imported_at_ms = Some(mj_core::clock::epoch_millis());
    }
    Ok(count)
}

struct ObserveBatch<'a> {
    revision: &'a str,
    materialized_event_ordinal: u64,
    execution: MaterializedExecutionState,
    historical: bool,
}

fn nonempty_string(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn target_observation(record: &SessionRecord) -> (Option<String>, Option<String>, Option<String>) {
    use mj_core::state::TargetLocator;

    let Some(target) = record.target.as_deref() else {
        return (None, None, None);
    };
    match target {
        TargetLocator::LocalBare { .. } => (Some("local".into()), Some(record.id.clone()), None),
        TargetLocator::LocalPodman { container_id, .. }
        | TargetLocator::LocalDocker { container_id, .. }
        | TargetLocator::AppleContainer { container_id, .. } => {
            (Some("container".into()), Some(container_id.clone()), None)
        }
        TargetLocator::AwsEc2 {
            instance_id,
            address,
        } => (
            Some("cloud".into()),
            Some(instance_id.clone()),
            address.clone(),
        ),
        TargetLocator::SshBare {
            host, worker_id, ..
        } => (
            Some("remote".into()),
            worker_id.clone().or_else(|| Some(record.id.clone())),
            Some(host.clone()),
        ),
        TargetLocator::SshPodman {
            host, container_id, ..
        }
        | TargetLocator::SshDocker {
            host, container_id, ..
        }
        | TargetLocator::SshAppleContainer {
            host, container_id, ..
        }
        | TargetLocator::SshNitro {
            host, container_id, ..
        } => (
            Some("remote-container".into()),
            Some(container_id.clone()),
            Some(host.clone()),
        ),
    }
}

fn git_observation(record: &SessionRecord) -> (Option<ObserveGitMeta>, Vec<ObserveCommit>) {
    use mj_core::repository::RepositoryIdentity;
    use mj_core::state::Checkout;

    let branch = match record.checkout().effective() {
        Checkout::ManagedWorktree { worktree, .. } => Some(worktree.branch.clone()),
        Checkout::Attached { .. } | Checkout::ManagedWorkspace | Checkout::Borrowed { .. } => {
            record.start_selection().branch
        }
    };
    let head = match record.checkout().effective() {
        Checkout::ManagedWorktree { worktree, .. } => worktree.base_commit.clone(),
        Checkout::Attached { .. } | Checkout::ManagedWorkspace | Checkout::Borrowed { .. } => {
            record.start_selection().at
        }
    };
    let remote = record.project.as_ref().and_then(|project| {
        project
            .identities
            .get(&project.bundle.primary_repo)
            .and_then(|identity| match identity {
                RepositoryIdentity::Github(owner, repository) => {
                    Some(format!("https://github.com/{owner}/{repository}"))
                }
                RepositoryIdentity::Network(url) => Some(url.clone()),
                RepositoryIdentity::RemoteDirectory { host, root } => {
                    Some(format!("{host}:{}", root.display()))
                }
                RepositoryIdentity::Local(_) => None,
            })
    });
    let commits = record
        .publication
        .as_ref()
        .into_iter()
        .flat_map(|publication| publication.saved_commits.iter())
        .map(|sha| ObserveCommit {
            sha: sha.clone(),
            ts: 0,
            ..Default::default()
        })
        .collect();
    let git = (remote.is_some() || branch.is_some() || head.is_some()).then_some(ObserveGitMeta {
        remote,
        branch,
        head,
    });
    (git, commits)
}

async fn flush_spool(
    paths: &ObservePaths,
    config: &ObserveDeliveryConfig,
    client: &reqwest::Client,
    cancellation: &CancellationToken,
) -> Result<usize> {
    let spool = ObserveSpool::open(&paths.spool)?;
    let pending = spool.pending()?;
    if pending.is_empty() {
        return Ok(0);
    }
    let ping_exchange = async {
        let response = client
            .get(format!("{}/api/v1/ingest/ping", config.url))
            .bearer_auth(&config.token)
            .send()
            .await
            .context("ask Brokk Observe for its ingest request limit")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Ok::<_, anyhow::Error>((status, body))
    };
    let (status, ping_body) = tokio::select! {
        _ = cancellation.cancelled() => return Ok(0),
        response = ping_exchange => response?,
    };
    if !status.is_success() {
        bail!(
            "Brokk Observe ping failed with HTTP {status}: {}",
            ping_body.chars().take(300).collect::<String>()
        );
    }
    let ping: Value =
        serde_json::from_str(&ping_body).context("decode Brokk Observe ingest limits")?;
    let limit = ping
        .get("max_body_bytes")
        .and_then(Value::as_u64)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes > 0)
        .context("Brokk Observe ping did not provide max_body_bytes")?;

    let started = Instant::now();
    let mut delivered = 0_usize;
    let mut index = 0_usize;
    while index < pending.len() {
        let (body, end) = batch_from(&pending, index, limit)?;
        let exchange = async {
            let response = client
                .post(format!("{}/api/v1/ingest/batch", config.url))
                .bearer_auth(&config.token)
                .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                .body(body)
                .send()
                .await
                .context("deliver a Brokk Observe batch")?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Ok::<_, anyhow::Error>((status, body))
        };
        let (status, response_body) = tokio::select! {
            _ = cancellation.cancelled() => return Ok(0),
            response = exchange => response?,
        };
        if !status.is_success() {
            bail!(
                "Brokk Observe batch failed with HTTP {status}: {}",
                response_body.chars().take(300).collect::<String>()
            );
        }
        for path in &pending[index..end] {
            spool.remove(path)?;
        }
        delivered += end - index;
        index = end;
    }
    tracing::debug!(
        delivered,
        elapsed_ms = started.elapsed().as_millis(),
        "delivered a Brokk Observe spool"
    );
    Ok(delivered)
}

fn batch_from(paths: &[PathBuf], start: usize, limit: usize) -> Result<(Vec<u8>, usize)> {
    let mut body = Vec::new();
    let mut end = start;
    while end < paths.len() {
        let line = fs::read(&paths[end])
            .with_context(|| format!("read Observe spool entry {}", paths[end].display()))?;
        if line.len() > limit {
            bail!(
                "Observe spool entry {} is {} bytes, over the server's {}-byte request limit",
                paths[end].display(),
                line.len(),
                limit
            );
        }
        if !body.is_empty() && body.len() + line.len() > limit {
            break;
        }
        body.extend_from_slice(&line);
        end += 1;
    }
    if end == start {
        bail!("no Observe spool entry fits the server request limit");
    }
    Ok((body, end))
}

fn record_success(paths: &ObservePaths) -> Result<()> {
    let now = mj_core::clock::epoch_millis();
    let mut state = ObserveDeliveryState::load(&paths.state, now)?;
    state.mark_success(now);
    state.save(&paths.state)
}

fn record_failure(paths: &ObservePaths, error: &anyhow::Error) -> Result<()> {
    let now = mj_core::clock::epoch_millis();
    let mut state = ObserveDeliveryState::load(&paths.state, now)?;
    state.mark_failure(now, format!("{error:#}"));
    state.save(&paths.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{ContentBlock, ContentChunk, TextContent};
    use mj_core::state::{
        MaterializedExecutionState, MaterializedSession, SessionConfiguration, TranscriptBody,
        TranscriptItem,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn batches_fill_the_server_limit_without_splitting_an_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("1.json");
        let second = directory.path().join("2.json");
        std::fs::write(&first, b"1234\n").unwrap();
        std::fs::write(&second, b"5678\n").unwrap();
        let paths = vec![first, second];
        assert_eq!(batch_from(&paths, 0, 10).unwrap().0, b"1234\n5678\n");
        assert_eq!(batch_from(&paths, 0, 9).unwrap().1, 1);
        assert!(batch_from(&paths, 0, 4).is_err());
    }

    #[test]
    fn a_session_wiki_only_checkpoint_is_backfilled_from_its_canonical_archive() {
        const CHILD: &str = "MJOLNIR_OBSERVE_CHECKPOINT_BACKFILL_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "a_session_wiki_only_checkpoint_is_backfilled_from_its_canonical_archive",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }

        let sessions = mj_core::config::sessions_dir();
        std::fs::create_dir_all(&sessions).unwrap();
        crate::controller::test_support::write_checkpoint_gate_archive(
            &sessions,
            "observe-archived-session",
            1,
        );
        let root = tempfile::tempdir().unwrap();
        let paths = ObservePaths {
            root: root.path().join("observe"),
            spool: root.path().join("observe/spool"),
            state: root.path().join("observe/delivery.json"),
        };
        std::fs::create_dir_all(&paths.root).unwrap();
        let spool = ObserveSpool::open(&paths.spool).unwrap();
        let mut delivery = ObserveDeliveryState::load(&paths.state, 1_800_000_000_000).unwrap();
        let emitted = publish_checkpoint("observe-archived-session", &spool, &mut delivery)
            .unwrap()
            .unwrap();
        assert!(emitted >= 2);
        let uploaded = spool
            .pending()
            .unwrap()
            .into_iter()
            .map(std::fs::read_to_string)
            .collect::<std::io::Result<String>>()
            .unwrap();
        assert!(uploaded.contains("\"hook_event_name\":\"SessionStart\""));
        assert!(uploaded.contains("\"hook_event_name\":\"SessionEnd\""));
        assert!(uploaded.contains("\"source\":\"mjolnir-backfill\""));
        assert!(uploaded.contains("\"harness\":\"codex\""));
    }

    #[tokio::test]
    async fn a_session_projectes_to_the_spool_and_uploads_as_an_ordered_batch() {
        const CHILD: &str = "MJOLNIR_OBSERVE_DELIVERY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "a_session_projectes_to_the_spool_and_uploads_as_an_ordered_batch",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }

        let _writer = crate::database::start_database_writer().unwrap();
        let mut record =
            crate::controller::test_support::checkpoint_test_session("observe-delivery-session");
        record.native_session_id = Some("observe-native-session".into());
        record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: PathBuf::from("/tmp/observe-worker/observe-delivery-session"),
        }));
        crate::database::save_session(&record).unwrap();
        let item = TranscriptItem {
            stable_id: "user-1".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1_700_000_000_100,
            last_changed_at_ms: 1_700_000_000_100,
            body: TranscriptBody::User {
                content: vec![
                    serde_json::to_value(ContentChunk::new(ContentBlock::Text(TextContent::new(
                        "audit this session",
                    ))))
                    .unwrap(),
                ],
            },
        };
        crate::database::save_materialized_session(&MaterializedSession {
            session_id: record.id.clone(),
            applied_event_ordinal: 1,
            applied_event_digest: format!("{:064x}", 1),
            last_activity_at_ms: Some(item.last_changed_at_ms),
            execution: MaterializedExecutionState::Idle,
            session_title: Some("Observe delivery".into()),
            configuration: SessionConfiguration::default(),
            transcript: vec![Arc::new(item)],
            queued_prompts: Vec::new(),
            pending_elicitations: Vec::new(),
            active_turn: None,
            last_turn_outcome: None,
        })
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        let paths = ObservePaths {
            root: root.path().join("observe"),
            spool: root.path().join("observe/spool"),
            state: root.path().join("observe/delivery.json"),
        };
        std::fs::create_dir_all(&paths.root).unwrap();
        let mut delivery = ObserveDeliveryState::load(&paths.state, 1_800_000_000_000).unwrap();
        delivery.initialized_at_ms = 1_800_000_000_000;
        delivery.save(&paths.state).unwrap();
        let spool = ObserveSpool::open(&paths.spool).unwrap();
        let state = mj_core::state::State {
            sessions: [(record.id.clone(), record.clone())].into_iter().collect(),
            ..Default::default()
        };
        let emitted =
            publish_one(&record, &state, &Config::default(), &spool, &mut delivery).unwrap();
        assert!(emitted >= 2, "session start and prompt must be emitted");
        assert_eq!(
            publish_one(&record, &state, &Config::default(), &spool, &mut delivery).unwrap(),
            0,
            "an unchanged projection must not be rebuilt"
        );
        delivery.save(&paths.state).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut uploaded = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..read]);
                    let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() < end + 4 + length {
                        continue;
                    }
                    if index == 0 {
                        assert!(headers.starts_with("GET /api/v1/ingest/ping"));
                    } else {
                        assert!(headers.starts_with("POST /api/v1/ingest/batch"));
                        uploaded.extend_from_slice(&request[end + 4..end + 4 + length]);
                    }
                    let response = if index == 0 {
                        r#"{"max_body_bytes":1048576}"#
                    } else {
                        r#"{"accepted":4,"duplicate":0,"rejected":0}"#
                    };
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                                response.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    break;
                }
            }
            uploaded
        });
        let config = ObserveDeliveryConfig {
            url: format!("http://{address}"),
            token: "test-token".into(),
            timeout_ms: 3_000,
        };
        let client = reqwest::Client::new();
        assert!(
            flush_spool(&paths, &config, &client, &CancellationToken::new())
                .await
                .unwrap()
                >= 2
        );
        assert!(spool.pending().unwrap().is_empty());
        let uploaded = String::from_utf8(server.await.unwrap()).unwrap();
        assert!(uploaded.contains("\"hook_event_name\":\"SessionStart\""));
        assert!(uploaded.contains("\"hook_event_name\":\"UserPromptSubmit\""));
        assert!(uploaded.contains("\"native_session_id\":\"observe-native-session\""));
        assert!(uploaded.contains("\"worker_id\":\"observe-delivery-session\""));
        assert!(uploaded.contains("\"runtime\":\"local\""));
        assert!(uploaded.contains("\"source\":\"mjolnir-backfill\""));
    }
}
