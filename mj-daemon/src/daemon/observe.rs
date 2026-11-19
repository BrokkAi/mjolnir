//! Automatic, non-blocking publication of Mjolnir sessions to Brokk Observe.
//!
//! A revision-driven task converts the daemon's canonical transcript and
//! usage projections into stable Observe envelopes, writes them to the durable
//! local spool, and flushes ordered batches. Observe being unavailable never
//! blocks a session: envelopes stay on disk and the delivery loop backs off.

use super::*;
use crate::observe::{
    ObserveCheckpoint, ObserveDeliveryState, ObservePaths, ObserveSession, ObserveSpool,
    checkpoint_envelopes, envelopes_for_transcript, transcript_response_from_materialized,
    usage_envelopes,
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
    let config = ObserveDeliveryConfig::load();
    if matches!(config, Ok(None)) {
        return Ok(());
    }
    let delivery = {
        let path = paths.state.clone();
        let loading_cancellation = cancellation.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            ObserveDeliveryState::load_cancellable(
                &path,
                mj_core::clock::epoch_millis(),
                &loading_cancellation,
            )
        })
        .await
        .context("Observe state load task panicked")?;
        if cancellation.is_cancelled() {
            return Ok(());
        }
        Arc::new(std::sync::Mutex::new(loaded?))
    };
    let config = match config {
        Ok(Some(config)) => config,
        Ok(None) => return Ok(()),
        Err(error) => {
            record_failure(&paths, &delivery, &error).await?;
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
            save_delivery(&paths, &delivery, None).await?;
            return Ok(());
        }
        if immediate {
            let before = revisions.borrow_and_update().to_owned();
            let export = publish_changed_sessions(&state, &paths, &delivery, &cancellation).await;
            if let Err(error) = &export {
                tracing::warn!(error = %format!("{error:#}"), "automatic Brokk Observe export stopped early");
            }
            if cancellation.is_cancelled() {
                save_delivery(&paths, &delivery, None).await?;
                return Ok(());
            }
            // Each batch repairs its own id log before upload. An enqueue
            // failure must not prevent already-durable entries freeing space.
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
                save_delivery(&paths, &delivery, None).await?;
                return Ok(());
            }
            let failed = match (&export, &flush) {
                (Ok(()), Ok(_)) => {
                    record_success(&paths, &delivery).await?;
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
                    record_failure(&paths, &delivery, &anyhow!(error)).await?;
                    true
                }
            };
            immediate = *revisions.borrow() != before;
            if failed {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        save_delivery(&paths, &delivery, None).await?;
                        return Ok(());
                    },
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
            _ = cancellation.cancelled() => {
                save_delivery(&paths, &delivery, None).await?;
                return Ok(());
            },
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
    delivery: &Arc<std::sync::Mutex<ObserveDeliveryState>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let (runtime_state, config) = {
        let owner = state.owner();
        let controller = owner.controller();
        (controller.state.clone(), controller.config.clone())
    };
    let paths = paths.clone();
    let delivery = Arc::clone(delivery);
    let cancellation = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let mut delivery = delivery
            .lock()
            .map_err(|_| anyhow!("Observe state lock poisoned"))?;
        let spool = ObserveSpool::open_for_delivery(&paths, &cancellation)?;
        let mut ids = runtime_state
            .sessions
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        match mj_controller::sessionwiki::mjolnir_session_ids() {
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
                None => match mj_controller::database::load_session_record(&session_id) {
                    Ok(record) => record,
                    Err(error) => {
                        failures.push(error);
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
                    tracing::warn!(
                        session_id,
                        error = %format!("{error:#}"),
                        "could not enqueue one Mjolnir session for Brokk Observe"
                    );
                    failures.push(error.context(format!("session {session_id}")));
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
            let error = anyhow!(
                "{} Mjolnir session(s) could not be prepared for Observe: {}",
                failures.len(),
                failures
                    .iter()
                    .map(|error| format!("{error:#}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            Err(error)
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
    let Some(summary) = mj_controller::database::load_materialized_session_summary(&record.id)?
    else {
        return publish_checkpoint(&record.id, spool, delivery).map(|count| count.unwrap_or(0));
    };
    let checkpoint = record.checkpoint.as_deref();
    if delivery.sessions.get(&record.id).is_some_and(|cursor| {
        cursor.record_updated_at == record.updated_at
            && cursor.materialized_event_ordinal == summary.applied_event_ordinal
            && cursor.execution == Some(summary.execution)
            && cursor.checkpoint_sha256.as_deref()
                == checkpoint.map(|checkpoint| checkpoint.sha256.as_str())
    }) {
        return Ok(0);
    }
    let Some(materialized) = mj_controller::database::load_materialized_session(&record.id)? else {
        return publish_checkpoint(&record.id, spool, delivery).map(|count| count.unwrap_or(0));
    };
    let model = materialized
        .configuration
        .values
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cwd = mj_controller::sessionwiki::indexed_project_directory(
        state,
        Some(config),
        &record.id,
        record,
    )
    .ok()
    .flatten()
    .map(|path| path.to_string_lossy().into_owned());
    let parent_id = state
        .subagents
        .get(&record.id)
        .map(|relation| relation.parent_session_id.clone());
    let emitted = spool.emitted_ids().load(&record.id)?;
    let cursor = delivery.sessions.get(&record.id);
    let initialized_at = delivery.initialized_at_ms;
    let started_at_ms = chrono::DateTime::parse_from_rfc3339(&record.created_at)
        .ok()
        .map_or(0, |time| time.timestamp_millis());
    let historical = cursor.is_none() && started_at_ms < initialized_at;
    let (runtime, worker_id, hostname) = target_observation(record);
    let (mut git, mut commits) = git_observation(record);
    // The record knows where a session was asked to start. Only a checkpoint
    // knows where a checkout on the target is now, or which commit a managed
    // workspace resolved its launch to.
    let found = match checkpoint.map(read_checkpoint_git).transpose() {
        Ok(found) => found.flatten(),
        Err(error) => {
            tracing::warn!(
                session_id = record.id,
                error = %format!("{error:#}"),
                "could not read the checkpoint's Git position for Brokk Observe"
            );
            None
        }
    };
    let mut position = None;
    if let Some(found) = found {
        let known = git.get_or_insert_with(ObserveGitMeta::default);
        known.remote = known.remote.take().or(found.remote);
        known.branch = found.position.branch.clone().or(known.branch.take());
        let start_already_sent = known.head.is_none()
            && cursor.is_none_or(|cursor| cursor.reported_git.is_none())
            && emitted.contains(&mj_core::observe::event_id(
                &record.id,
                0,
                "SessionStart",
                0,
            ));
        if known.head.is_none() {
            known.head = found.launch.clone();
        }
        let reported = reported_git(&found.position);
        if cursor.and_then(|cursor| cursor.reported_git.as_ref()) != Some(&reported) {
            position = Some(ObserveCheckpoint {
                launch: found.launch.filter(|_| start_already_sent),
                ..found.position
            });
        }
    }
    // A saved ref that still names the start commit is not this session's work.
    let start = git.as_ref().and_then(|git| git.head.as_deref());
    commits.retain(|commit| Some(commit.sha.as_str()) != start);
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
    if let Some(position) = &position {
        let mut found = checkpoint_envelopes(&session, position)?;
        // A checkpoint from before delivery began is reported after the fact,
        // like the session it belongs to. Observe then keeps its time instead
        // of counting an old session as active now.
        if position.created_at_ms < initialized_at {
            for envelope in &mut found {
                envelope.meta.historical = true;
            }
        }
        insert_before_session_end(&mut envelopes, found);
    }
    let mut usage_after = 0_u64;
    loop {
        let Some(page) = mj_controller::database::load_session_usage(&record.id, usage_after, 500)?
        else {
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
            checkpoint_sha256: checkpoint.map(|checkpoint| checkpoint.sha256.as_str()),
            reported_git: position.as_ref().map(reported_git),
        },
        spool,
        delivery,
        emitted,
    )
}

fn publish_checkpoint(
    session_id: &str,
    spool: &ObserveSpool,
    delivery: &mut ObserveDeliveryState,
) -> Result<Option<usize>> {
    let Some(path) = mj_controller::sessionwiki::newest_checkpoint_archive(session_id)? else {
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
    if delivery.sessions.get(session_id).is_some_and(|cursor| {
        cursor.checkpoint_sha256.as_deref() == Some(archive.archive_sha256.as_str())
            && cursor.historical_imported_at_ms.is_some()
    }) {
        return Ok(Some(0));
    }
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
    let found = checkpoint_git(
        &archive.manifest,
        &archive.archive_sha256,
        &archive.manifest.session.checkpointed_at,
        archive.canonical_session.event_frontier,
    )?;
    let git = found.as_ref().map(|found| ObserveGitMeta {
        remote: found.remote.clone(),
        branch: found.position.branch.clone(),
        head: found.launch.clone(),
    });
    let position = found.map(|found| found.position);
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
    let mut envelopes = envelopes_for_transcript(&session, &transcript)?;
    if let Some(position) = &position {
        insert_before_session_end(&mut envelopes, checkpoint_envelopes(&session, position)?);
    }
    let emitted = spool.emitted_ids().load(session_id)?;
    enqueue_envelopes(
        session_id,
        envelopes,
        ObserveBatch {
            revision: &archive.manifest.session.checkpointed_at,
            materialized_event_ordinal: archive.canonical_session.event_frontier,
            execution: MaterializedExecutionState::Closed,
            historical: true,
            checkpoint_sha256: Some(&archive.archive_sha256),
            reported_git: position.as_ref().map(reported_git),
        },
        spool,
        delivery,
        emitted,
    )
    .map(Some)
}

fn enqueue_envelopes(
    session_id: &str,
    envelopes: Vec<mj_core::observe::ObserveEnvelope>,
    batch: ObserveBatch<'_>,
    spool: &ObserveSpool,
    delivery: &mut ObserveDeliveryState,
    mut emitted: BTreeSet<String>,
) -> Result<usize> {
    let ids = spool.emitted_ids();
    let mut appended = Vec::new();
    let mut count = 0_usize;
    for envelope in envelopes {
        if emitted.contains(&envelope.meta.event_id) {
            continue;
        }
        if let Err(error) = spool.enqueue(&envelope) {
            if !appended.is_empty() {
                ids.append(session_id, &appended)?;
            }
            // Batch delivery recovers any current id left behind by a failure
            // after publication, independently of whether projection can retry.
            return Err(error);
        }
        emitted.insert(envelope.meta.event_id.clone());
        appended.push(envelope.meta.event_id.clone());
        count += 1;
    }
    ids.append(session_id, &appended)?;
    let cursor = delivery.sessions.entry(session_id.to_owned()).or_default();
    cursor.record_updated_at = batch.revision.to_owned();
    cursor.materialized_event_ordinal = batch.materialized_event_ordinal;
    cursor.execution = Some(batch.execution);
    cursor.checkpoint_sha256 = batch.checkpoint_sha256.map(str::to_owned);
    if let Some(reported) = batch.reported_git {
        cursor.reported_git = Some(reported);
    }
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
    /// The checkpoint examined for this batch, if the session has one.
    checkpoint_sha256: Option<&'a str>,
    /// The checkpoint position this batch publishes, if it publishes one.
    reported_git: Option<ObserveGitMeta>,
}

/// What a checkpoint recorded about the session's primary repository.
struct CheckpointGit {
    remote: Option<String>,
    /// The commit the session launched from. Only a managed network workspace
    /// records one: any other archive's base is where that capture started.
    launch: Option<String>,
    position: ObserveCheckpoint,
}

fn read_checkpoint_git(
    checkpoint: &mj_core::state::CheckpointMetadata,
) -> Result<Option<CheckpointGit>> {
    let manifest = mj_checkpoint::archive::read_checkpoint_manifest(&checkpoint.archive_path)
        .with_context(|| format!("read checkpoint {}", checkpoint.archive_path.display()))?;
    checkpoint_git(
        &manifest,
        &checkpoint.sha256,
        &checkpoint.created_at,
        checkpoint.event_frontier,
    )
}

/// `None` is a checkpoint with no Git position to report: no primary
/// repository, a plain directory, or a repository without a commit.
fn checkpoint_git(
    manifest: &mj_core::archive::ArchiveManifest,
    sha256: &str,
    created_at: &str,
    event_frontier: u64,
) -> Result<Option<CheckpointGit>> {
    let Some(repository) = manifest
        .repositories
        .iter()
        .map(|repository| &repository.metadata)
        .find(|repository| repository.id == manifest.bundle.primary_repository)
    else {
        return Ok(None);
    };
    let Some(head) = nonempty_string(&repository.head_commit) else {
        return Ok(None);
    };
    let created_at_ms = chrono::DateTime::parse_from_rfc3339(created_at)
        .with_context(|| format!("parse checkpoint time {created_at:?}"))?
        .timestamp_millis();
    Ok(Some(CheckpointGit {
        remote: nonempty_string(&repository.origin),
        launch: repository
            .remote_workspace
            .then(|| nonempty_string(&repository.base_commit))
            .flatten(),
        position: ObserveCheckpoint {
            sha256: sha256.to_owned(),
            created_at_ms,
            event_frontier,
            branch: repository.branch.clone(),
            head,
            launch: None,
        },
    }))
}

/// The part of a checkpoint position that decides whether it is news.
fn reported_git(position: &ObserveCheckpoint) -> ObserveGitMeta {
    ObserveGitMeta {
        remote: None,
        branch: position.branch.clone(),
        head: Some(position.head.clone()),
    }
}

/// A session's end is its last event, so a checkpoint taken while it stopped
/// is placed ahead of it.
fn insert_before_session_end(
    envelopes: &mut Vec<mj_core::observe::ObserveEnvelope>,
    checkpoint: Vec<mj_core::observe::ObserveEnvelope>,
) {
    let end = envelopes
        .iter()
        .rposition(|envelope| envelope.event["hook_event_name"] == "SessionEnd")
        .unwrap_or(envelopes.len());
    envelopes.splice(end..end, checkpoint);
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
    let delivery_paths = paths.clone();
    let preparing_cancellation = cancellation.clone();
    let (spool, pending) = tokio::task::spawn_blocking(move || {
        let spool = ObserveSpool::open_for_delivery(&delivery_paths, &preparing_cancellation)?;
        let pending = spool.pending_cancellable(&preparing_cancellation)?;
        Ok::<_, anyhow::Error>((spool, Arc::new(pending)))
    })
    .await
    .context("Observe spool preparation task panicked")??;
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
        let batch_paths = Arc::clone(&pending);
        let preparing_spool = spool.clone();
        let preparing_cancellation = cancellation.clone();
        let (body, end) = tokio::task::spawn_blocking(move || {
            let (body, end) = batch_from(&batch_paths, index, limit)?;
            // Recover only this server-sized batch, using bytes already read.
            preparing_spool.prepare_delivery(&body, &preparing_cancellation)?;
            Ok::<_, anyhow::Error>((body, end))
        })
        .await
        .context("Observe batch preparation task panicked")??;
        if cancellation.is_cancelled() {
            return Ok(0);
        }
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
        let removing_spool = spool.clone();
        let removing_paths = Arc::clone(&pending);
        tokio::task::spawn_blocking(move || {
            for path in &removing_paths[index..end] {
                removing_spool.remove(path)?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Observe spool removal task panicked")??;
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

async fn save_delivery(
    paths: &ObservePaths,
    delivery: &Arc<std::sync::Mutex<ObserveDeliveryState>>,
    outcome: Option<std::result::Result<(), String>>,
) -> Result<()> {
    let path = paths.state.clone();
    let delivery = Arc::clone(delivery);
    tokio::task::spawn_blocking(move || {
        let mut state = delivery
            .lock()
            .map_err(|_| anyhow!("Observe state lock poisoned"))?;
        match outcome {
            Some(Ok(())) => state.mark_success(mj_core::clock::epoch_millis()),
            Some(Err(error)) => state.mark_failure(mj_core::clock::epoch_millis(), error),
            None => {}
        }
        state.save(&path)
    })
    .await
    .context("Observe state save task panicked")?
}

async fn record_success(
    paths: &ObservePaths,
    delivery: &Arc<std::sync::Mutex<ObserveDeliveryState>>,
) -> Result<()> {
    save_delivery(paths, delivery, Some(Ok(()))).await
}

async fn record_failure(
    paths: &ObservePaths,
    delivery: &Arc<std::sync::Mutex<ObserveDeliveryState>>,
    error: &anyhow::Error,
) -> Result<()> {
    save_delivery(paths, delivery, Some(Err(format!("{error:#}")))).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::ObserveEmittedIds;
    use agent_client_protocol::v1::schema::{ContentBlock, ContentChunk, TextContent};
    use mj_core::state::{
        MaterializedExecutionState, MaterializedSession, SessionConfiguration, TranscriptBody,
        TranscriptItem,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn projected_stop(session_id: &str, text: &str) -> mj_core::observe::ObserveEnvelope {
        let session = ObserveSession {
            id: session_id.into(),
            harness: "codex".into(),
            ..Default::default()
        };
        let transcript = crate::server::api::TranscriptResponse {
            session_id: session_id.into(),
            latest_seq: 1,
            next_after_seq: 1,
            execution: MaterializedExecutionState::Idle,
            items: vec![crate::server::api::TranscriptItemView {
                stable_id: "agent-1".into(),
                position: 1,
                seq: 1,
                role: "assistant".into(),
                text: text.into(),
                created_at_ms: 10,
                last_changed_at_ms: 10,
                body: TranscriptBody::Agent {
                    chunks: vec![],
                    streaming: false,
                },
            }],
        };
        envelopes_for_transcript(&session, &transcript)
            .unwrap()
            .pop()
            .unwrap()
    }

    fn replay_stop(
        spool: &ObserveSpool,
        delivery: &mut ObserveDeliveryState,
        envelope: mj_core::observe::ObserveEnvelope,
    ) -> usize {
        let session_id = envelope.event["session_id"].as_str().unwrap().to_owned();
        let ids = spool.emitted_ids();
        enqueue_envelopes(
            &session_id,
            vec![envelope],
            ObserveBatch {
                revision: "after-replay",
                materialized_event_ordinal: 7,
                execution: MaterializedExecutionState::Idle,
                historical: false,
                checkpoint_sha256: None,
                reported_git: None,
            },
            spool,
            delivery,
            ids.load(&session_id).unwrap(),
        )
        .unwrap()
    }

    // Hard-won: Observe spool replay after a crash rejected a grown Stop forever.
    #[test]
    fn observe_crash_after_spooling_keeps_first_version_and_advances_cursor() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("delivery.json");
        let spool = ObserveSpool::open(root.path().join("spool")).unwrap();
        let original = projected_stop("recover-native-opus", "partial answer");
        let first = spool.enqueue(&original).unwrap();
        drop(spool);
        // Restart without recording an id, then project a longer message.
        let spool = ObserveSpool::open(root.path().join("spool")).unwrap();
        let mut changed = projected_stop("recover-native-opus", "complete answer");
        changed.meta.ts += 1; // Content revisions may also change ordering metadata.
        let mut delivery = ObserveDeliveryState::load(&path, 1).unwrap();
        assert_eq!(replay_stop(&spool, &mut delivery, changed.clone()), 1);
        assert_eq!(
            delivery.sessions["recover-native-opus"].record_updated_at,
            "after-replay"
        );
        assert_eq!(spool.pending().unwrap(), vec![first.clone()]);
        assert_eq!(
            std::fs::read_to_string(first).unwrap(),
            original.to_ndjson().unwrap()
        );
        spool.remove(&spool.pending().unwrap()[0]).unwrap();
        // A lost cursor after delivery is filtered by the durable id log.
        delivery = ObserveDeliveryState::load(&path, 1).unwrap();
        assert_eq!(replay_stop(&spool, &mut delivery, changed), 0);
        assert!(spool.pending().unwrap().is_empty());
        // Another crash-left session cannot be projected at all. Delivery
        // recovers its id from the queued envelope before removing that file.
        let unprojectable = projected_stop("unprojectable-session", "queued before crash");
        spool.enqueue(&unprojectable).unwrap();
        assert!(
            spool
                .emitted_ids()
                .load("unprojectable-session")
                .unwrap()
                .is_empty()
        );
        let later = spool.root().join("zz-later.json");
        std::fs::write(&later, b"not a valid envelope").unwrap();
        let pending = spool.pending().unwrap();
        let (body, end) =
            batch_from(&pending, 0, unprojectable.to_ndjson().unwrap().len()).unwrap();
        assert_eq!(end, 1);
        // Hard-won: d91380c89: preparation must not read the rest of the backlog.
        spool
            .prepare_delivery(&body, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            spool.emitted_ids().load("unprojectable-session").unwrap(),
            BTreeSet::from([unprojectable.meta.event_id.clone()])
        );
        spool.remove(&pending[0]).unwrap();
        spool.remove(&later).unwrap();
        assert_eq!(replay_stop(&spool, &mut delivery, unprojectable), 0);
    }

    // Hard-won: 66409e7c5: a post-link sync failure let delivery outrun its id log.
    #[cfg(unix)]
    #[test]
    fn observe_post_link_failure_retains_spool_until_ids_are_durable() {
        use std::os::unix::fs::PermissionsExt;
        // Root bypasses the permission error this filesystem regression needs.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("requires an unprivileged Unix user to exercise directory fsync denial");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("delivery.json");
        let spool = ObserveSpool::open(root.path().join("spool")).unwrap();
        let session_id = "post-link-sync-failure";
        let envelope = projected_stop(session_id, "first answer");
        let mut delivery = ObserveDeliveryState::load(&path, 1).unwrap();
        // Prime the lazy identity index before denying directory reads.
        let earlier = spool
            .enqueue(&projected_stop("earlier-session", "earlier answer"))
            .unwrap();
        spool.remove(&earlier).unwrap();
        let permissions = std::fs::metadata(spool.root()).unwrap().permissions();
        // Creating/removing directory entries still works, but opening this
        // directory for its fsync fails after the final spool link is visible.
        std::fs::set_permissions(spool.root(), std::fs::Permissions::from_mode(0o300)).unwrap();
        let result = enqueue_envelopes(
            session_id,
            vec![envelope.clone()],
            ObserveBatch {
                revision: "before-sync",
                materialized_event_ordinal: 1,
                execution: MaterializedExecutionState::Idle,
                historical: false,
                checkpoint_sha256: None,
                reported_git: None,
            },
            &spool,
            &mut delivery,
            BTreeSet::new(),
        );
        std::fs::set_permissions(spool.root(), permissions).unwrap();
        assert!(result.is_err());
        assert!(!delivery.sessions.contains_key(session_id));
        assert!(spool.emitted_ids().load(session_id).unwrap().is_empty());
        let queued = spool.pending().unwrap();
        assert_eq!(queued.len(), 1);
        let mut replay = projected_stop(session_id, "longer answer");
        replay.meta.ts += 1;
        assert_eq!(replay_stop(&spool, &mut delivery, replay), 1);
        assert_eq!(spool.pending().unwrap(), queued);
        assert_eq!(
            std::fs::read_to_string(&queued[0]).unwrap(),
            envelope.to_ndjson().unwrap()
        );
        assert_eq!(
            spool.emitted_ids().load(session_id).unwrap(),
            BTreeSet::from([envelope.meta.event_id])
        );
    }

    #[test]
    fn observe_v1_migration_preserves_ids_across_reprojection() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("delivery.json");
        let session_id = "../imported/native-opus-recovery";
        let envelope = projected_stop(session_id, "already sent");
        let legacy = serde_json::json!({
            "version": 1, "initialized_at_ms": 10, "last_failure": "offline", "last_failure_ms": 20,
            "sessions": { session_id: { "record_updated_at": "before", "emitted_event_ids": [envelope.meta.event_id] } }
        });
        let body = serde_json::to_vec(&legacy).unwrap();
        std::fs::write(&path, &body).unwrap();
        assert_eq!(
            ObserveDeliveryState::diagnostics(&path, 30)
                .unwrap()
                .last_failure
                .as_deref(),
            Some("offline")
        );
        assert_eq!(std::fs::read(&path).unwrap(), body); // diagnostics cannot migrate.
        let ids = ObserveEmittedIds::new(&path);
        ids.append(session_id, &["partial-migration".into()])
            .unwrap();
        let mut interrupted = legacy.clone();
        interrupted["sessions"]["zz-invalid-session"] =
            serde_json::json!({"record_updated_at": 7, "emitted_event_ids": []});
        let interrupted_body = serde_json::to_vec(&interrupted).unwrap();
        std::fs::write(&path, &interrupted_body).unwrap();
        assert!(ObserveDeliveryState::load(&path, 30).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), interrupted_body);
        std::fs::write(&path, &body).unwrap();
        let delivery = ObserveDeliveryState::load(&path, 30).unwrap();
        assert_eq!(delivery.version, 2);
        assert_eq!(delivery.initialized_at_ms, 10);
        assert_eq!(
            ids.load(session_id).unwrap(),
            BTreeSet::from([envelope.meta.event_id.clone()])
        );
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            persisted["sessions"][session_id]
                .get("emitted_event_ids")
                .is_none()
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("emitted"))
                .unwrap()
                .count(),
            1
        );
        let mut reloaded = ObserveDeliveryState::load(&path, 40).unwrap();
        let paths = ObservePaths {
            root: root.path().to_owned(),
            state: path.clone(),
            spool: root.path().join("spool"),
        };
        #[cfg(unix)]
        let relocated_spool = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(relocated_spool.path(), &paths.spool).unwrap();
        // Hard-won: d91380c89: a relocated spool must use the migrated id log.
        let spool = ObserveSpool::open_for_delivery(&paths, &CancellationToken::new()).unwrap();

        assert_eq!(replay_stop(&spool, &mut reloaded, envelope), 0);
        assert!(spool.pending().unwrap().is_empty());
    }

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
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
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
        mj_controller::controller::test_support::write_checkpoint_gate_archive(
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
        assert_eq!(
            publish_checkpoint("observe-archived-session", &spool, &mut delivery).unwrap(),
            Some(0)
        );
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

    /// A checkpoint of `session_id` whose primary repository is a managed
    /// network workspace launched at `base` and now at `head`.
    fn checkpoint_at(
        directory: &Path,
        session_id: &str,
        event_frontier: u64,
        base: &str,
        head: &str,
    ) -> mj_core::state::CheckpointMetadata {
        use mj_checkpoint::archive::{RepositoryMetadata, RepositorySnapshot};
        let input = mj_controller::controller::test_support::checkpoint_archive_input(
            session_id,
            event_frontier,
            vec![RepositorySnapshot {
                metadata: RepositoryMetadata {
                    review_baseline_ref: None,
                    review_refs_metadata_only: false,
                    saved_refs: Default::default(),
                    stash_stack: Vec::new(),
                    git_operation: None,
                    id: "project".into(),
                    relative_destination: "project".into(),
                    checkout_subdirectory: None,
                    origin: "https://github.com/example/project.git".into(),
                    push_urls: Vec::new(),
                    remote_workspace: true,
                    plain_directory: false,
                    base_commit: base.into(),
                    head_commit: head.into(),
                    branch: Some("fix/observed".into()),
                },
                committed_bundle: Vec::new(),
                staged_patch: Vec::new(),
                unstaged_patch: Vec::new(),
                untracked_tar: Vec::new(),
                operation_payloads: None,
                linked_worktrees: Vec::new(),
            }],
            Vec::new(),
        );
        mj_controller::controller::test_support::write_checkpoint_archive_input(
            directory, session_id, &input,
        )
    }

    #[test]
    fn a_checkpoint_position_is_published_once_and_never_moves_the_start() {
        const CHILD: &str = "MJOLNIR_OBSERVE_CHECKPOINT_POSITION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "a_checkpoint_position_is_published_once_and_never_moves_the_start",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }

        let _writer = mj_controller::database::start_database_writer().unwrap();
        let mut record = mj_controller::controller::test_support::checkpoint_test_session(
            "observe-position-session",
        );
        mj_controller::database::save_session(&record).unwrap();
        let item = TranscriptItem {
            stable_id: "user-1".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1_700_000_000_100,
            last_changed_at_ms: 1_700_000_000_100,
            body: TranscriptBody::User {
                content: vec![
                    serde_json::to_value(ContentChunk::new(ContentBlock::Text(TextContent::new(
                        "fix the bug",
                    ))))
                    .unwrap(),
                ],
            },
        };
        mj_controller::database::save_materialized_session(&MaterializedSession {
            session_id: record.id.clone(),
            applied_event_ordinal: 1,
            applied_event_digest: format!("{:064x}", 1),
            last_activity_at_ms: Some(item.last_changed_at_ms),
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: SessionConfiguration::default(),
            transcript: vec![Arc::new(item)],
            queued_prompts: Vec::new(),
            pending_elicitations: Vec::new(),
            active_turn: None,
            last_turn_outcome: None,
        })
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        let spool = ObserveSpool::open(root.path().join("spool")).unwrap();
        let mut delivery =
            ObserveDeliveryState::load(&root.path().join("delivery.json"), 1_800_000_000_000)
                .unwrap();
        let state = mj_core::state::State::default();
        let mut publish = |record: &SessionRecord| {
            let count =
                publish_one(record, &state, &Config::default(), &spool, &mut delivery).unwrap();
            if record.checkpoint.is_none() {
                // Hard-won: d91380c89: partial export can persist SessionStart without a cursor.
                delivery.sessions.remove(&record.id);
            }
            count
        };
        // Every head in the spool, in delivery order, with the event carrying it.
        let heads = || {
            spool
                .pending()
                .unwrap()
                .into_iter()
                .map(|path| serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap())
                .filter_map(|envelope| {
                    let git = &envelope["meta"]["git"];
                    assert_eq!(git["remote"], "https://github.com/example/project.git");
                    assert_eq!(git["branch"], "fix/observed");
                    git["head"].as_str().map(|head| {
                        (
                            envelope["event"]["hook_event_name"]
                                .as_str()
                                .unwrap()
                                .to_owned(),
                            head.to_owned(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        let checkpoint = |head: &str| ("Checkpoint".to_owned(), head.to_owned());
        let (launch, first, second) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let archives = tempfile::tempdir().unwrap();
        let checkpointed =
            |record: &mut SessionRecord, frontier: u64, head: &str, revision: &str| {
                record.checkpoint = Some(Box::new(checkpoint_at(
                    archives.path(),
                    "observe-position-session",
                    frontier,
                    &launch,
                    head,
                )));
                record.updated_at = revision.into();
            };

        // The session starts before anything says where its checkout is.
        assert!(publish(&record) >= 2);
        for path in spool.pending().unwrap() {
            spool.remove(&path).unwrap();
        }

        // Its first checkpoint reports the launch commit ahead of the head, so
        // Observe's first head is the start and its newest is the end.
        checkpointed(&mut record, 1, &first, "2026-08-14T12:00:01Z");
        assert_eq!(publish(&record), 2);
        assert_eq!(heads(), [checkpoint(&launch), checkpoint(&first)]);
        assert_eq!(publish(&record), 0);

        // A later checkpoint that finds the session where it was is not news.
        checkpointed(&mut record, 2, &first, "2026-08-14T12:00:02Z");
        assert_eq!(publish(&record), 0);

        // One that finds it elsewhere is, and does not repeat the launch.
        checkpointed(&mut record, 3, &second, "2026-08-14T12:00:03Z");
        assert_eq!(publish(&record), 1);
        assert_eq!(
            heads(),
            [checkpoint(&launch), checkpoint(&first), checkpoint(&second)]
        );
    }

    #[tokio::test]
    async fn a_session_projectes_to_the_spool_and_uploads_as_an_ordered_batch() {
        const CHILD: &str = "MJOLNIR_OBSERVE_DELIVERY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "a_session_projectes_to_the_spool_and_uploads_as_an_ordered_batch",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }

        let _writer = mj_controller::database::start_database_writer().unwrap();
        let mut record = mj_controller::controller::test_support::checkpoint_test_session(
            "observe-delivery-session",
        );
        record.native_session_id = Some("observe-native-session".into());
        record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: PathBuf::from("/tmp/observe-worker/observe-delivery-session"),
        }));
        mj_controller::database::save_session(&record).unwrap();
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
        mj_controller::database::save_materialized_session(&MaterializedSession {
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

        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::metadata(spool.root()).unwrap().permissions();
            std::fs::set_permissions(spool.root(), std::fs::Permissions::from_mode(0o500)).unwrap();
            let enqueue = spool.enqueue(&projected_stop(
                "cannot-enqueue",
                "no free writable storage",
            ));
            std::fs::set_permissions(spool.root(), permissions).unwrap();
            // Hard-won: d91380c89: failed enqueue must still let durable entries drain.
            assert!(enqueue.is_err());
        }

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
