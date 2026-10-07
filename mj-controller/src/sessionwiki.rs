//! Publishing Mjolnir's own checkpointed sessions into the user's SessionWiki
//! index, and the daemon-side job that keeps that index current.
//!
//! SessionWiki keeps one searchable index of AI coding sessions across every
//! tool a user runs. Mjolnir links it as a library and registers
//! [`MjolnirAdapter`] beside SessionWiki's built-in adapters, so a Mjolnir
//! session is searchable next to a Claude Code or Codex one. The adapter is a
//! "shared store" adapter: checkpoints are not one-file-per-session in a shape
//! SessionWiki can parse, so the indexer enumerates sessions by key and asks
//! this adapter to parse the ones whose checkpoint changed.

mod harness_adapters;
pub(crate) mod history;
mod provenance;
pub mod tags;
mod top_level;
mod transcript_grep;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use mj_client::daemon::{
    SessionTextMatch, SessionTextMatchKind, WikiHitBlock, WikiHitTranscript, WikiIndexState,
    WikiRow, WikiSessionInfo, WikiSessionStatus, WikiStatus,
};
use mj_core::config::{Config, HarnessKind};
use mj_core::state::{SessionRecord, State};
use sessionwiki::adapters::{Adapter, Discovered, Store};
use sessionwiki::model::{Message, Role, Session};

use crate::controller::Controller;
use crate::controller::checkpoint::managed_checkpoint_archive_name;
use harness_adapters::HarnessAdapter;

/// The tool name every Mjolnir instance publishes under. One name means one
/// search partition; reconciliation is scoped per instance instead (see
/// [`Adapter::reconcile_scope`]).
const TOOL: &str = "mjolnir";
const CONVERSATION_ROLES: &[Role] = &[Role::User, Role::Assistant];

/// The low token slot identifies the `Session` projection format. Raising it
/// makes rows indexed by older parse rules stale exactly once.
const SESSION_PARSE_FORMAT_VERSION: i64 = 1;
const CHANGE_TOKEN_SLOT_BITS: u32 = 10;

fn session_change_token(token: i64) -> i64 {
    token
        .saturating_mul(1_i64 << (CHANGE_TOKEN_SLOT_BITS * 2))
        .saturating_add(
            i64::from(mj_transcript::summary::SUMMARY_VERSION)
                .saturating_mul(1_i64 << CHANGE_TOKEN_SLOT_BITS),
        )
        .saturating_add(SESSION_PARSE_FORMAT_VERSION)
}

/// One checkpoint archive on disk, reduced to what indexing needs.
struct ArchiveFile {
    path: PathBuf,
    frontier: u64,
    /// Modification time in epoch seconds, SessionWiki's change token.
    token: i64,
}

/// What indexing needs from controller state: which sessions exist, which of
/// them are sub-agent children, and which are still running with their
/// conversation in the daemon's own database rather than in a checkpoint.
#[derive(Default)]
struct Sessions {
    records: mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    ownership: top_level::Snapshot,
    /// Effective agent working directory derived through `State::checkout`.
    /// Errors are retained so a malformed bundle cannot silently index with
    /// an empty project.
    project_directories: BTreeMap<String, std::result::Result<Option<PathBuf>, String>>,
    /// Session id to change token, for sessions indexed from the projection.
    live: BTreeMap<String, i64>,
}

impl Sessions {
    fn of(state: &State, config: Option<&Config>) -> Self {
        Self {
            records: state.sessions.clone(),
            ownership: top_level::Snapshot::from_state(state),
            project_directories: project_directories_of(state, config),
            live: live_tokens(state),
        }
    }
}

fn project_directories_of(
    state: &State,
    config: Option<&Config>,
) -> BTreeMap<String, std::result::Result<Option<PathBuf>, String>> {
    state
        .sessions
        .iter()
        .map(|(session_id, record)| {
            (
                session_id.clone(),
                indexed_project_directory(state, config, session_id, record)
                    .map_err(|error| format!("{error:#}")),
            )
        })
        .collect()
}

/// The project path SessionWiki should search for this session: the attached
/// or managed raw checkout, or the primary repository in a managed workspace.
fn indexed_project_directory(
    state: &State,
    config: Option<&Config>,
    session_id: &str,
    record: &SessionRecord,
) -> Result<Option<PathBuf>> {
    match state.checkout(session_id)?.effective() {
        mj_core::state::Checkout::Attached { path } => Ok(Some(path.to_path_buf())),
        mj_core::state::Checkout::ManagedWorktree {
            project_directory, ..
        } => Ok(project_directory.map(Path::to_path_buf)),
        mj_core::state::Checkout::ManagedWorkspace => {
            let Some(config) = config else {
                return Ok(None);
            };
            let Some(bundle) = record.project_bundle(config) else {
                return Ok(None);
            };
            let primary = bundle
                .repositories
                .iter()
                .find(|repository| repository.id == bundle.primary_repo)
                .with_context(|| {
                    format!(
                        "session bundle has no primary repository {:?}",
                        bundle.primary_repo
                    )
                })?;
            let workspace_root = match record.target.as_ref() {
                Some(locator) => {
                    let backend = crate::controller::backend_locator(locator, record, config)
                        .context("resolve the session target for SessionWiki")?;
                    crate::controller::workspace_root(
                        &backend,
                        record.container_workspace.as_deref(),
                    )
                }
                None => mj_core::targets::container_workspace_root(
                    record.container_workspace.as_deref(),
                ),
            };
            Ok(Some(PathBuf::from(
                crate::server_runtime::api::agent_working_directory_at(
                    &workspace_root,
                    &primary.destination,
                ),
            )))
        }
        mj_core::state::Checkout::Borrowed { .. } => {
            unreachable!("State::checkout resolves borrowed sessions")
        }
    }
}

/// The change token of every session whose transcript is still only in the
/// daemon's database: its activity watermark in whole seconds.
///
/// A stopped session keeps being indexed from its checkpoint, which never
/// changes again. Everything else is indexed from the projection, so a running
/// session is findable before it has ever been closed.
fn live_tokens(state: &State) -> BTreeMap<String, i64> {
    let activity = match crate::database::load_transcribed_session_activity() {
        Ok(activity) => activity,
        Err(error) => {
            tracing::warn!(%error, "could not read session activity for SessionWiki");
            return BTreeMap::new();
        }
    };
    state
        .sessions
        .iter()
        .filter(|(_, record)| record.state != mj_core::state::SessionState::Stopped)
        .filter_map(|(session_id, _)| {
            let watermark = activity.get(session_id)?;
            Some((session_id.clone(), watermark.unwrap_or_default() / 1000))
        })
        .collect()
}

/// Mjolnir's sessions, as SessionWiki sees them.
pub struct MjolnirAdapter {
    sessions_dir: PathBuf,
    sessions: std::sync::Mutex<Sessions>,
    /// Re-read controller state when the indexer reaches this adapter.
    reload: bool,
}

impl MjolnirAdapter {
    /// A fixed view of the given state, which is what a caller with a state in
    /// hand wants.
    pub fn from_state(state: &State) -> Self {
        Self {
            sessions_dir: mj_core::config::sessions_dir(),
            sessions: std::sync::Mutex::new(Sessions::of(state, None)),
            reload: false,
        }
    }

    /// A fixed view that can resolve bundle-backed session directories from
    /// the configuration snapshot paired with the state.
    pub(crate) fn from_state_with_config(state: &State, config: &Config) -> Self {
        Self {
            sessions_dir: mj_core::config::sessions_dir(),
            sessions: std::sync::Mutex::new(Sessions::of(state, Some(config))),
            reload: false,
        }
    }

    /// The same, but re-reading controller state when the indexer reaches this
    /// adapter.
    ///
    /// One sync pass walks every other tool's store first, which can take
    /// minutes on a large corpus. Without the reload, sessions that closed
    /// during that walk would be indexed with no record: no project, no start
    /// time, and the title guessed from the first prompt. Their checkpoints do
    /// not change afterwards, so nothing would ever correct them.
    pub fn reloading(state: &State) -> Self {
        Self {
            reload: true,
            ..Self::from_state(state)
        }
    }

    /// Mjolnir's own metadata for every session this adapter knows about, to
    /// be stored in the index beside the transcripts.
    ///
    /// Read from the adapter's own snapshot rather than from the controller
    /// state the sync loaded, because [`MjolnirAdapter::reloading`] replaces
    /// that snapshot when the indexer reaches this adapter. A session that
    /// closed during a long first pass is indexed from the reloaded state, so
    /// its metadata has to come from the same state that produced its row.
    pub fn indexed_tags(&self) -> BTreeMap<String, tags::MjTags> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .records
            .iter()
            .filter(|(id, _)| !sessions.ownership.owns_session(id))
            .map(|(session_id, record)| {
                (
                    session_id.clone(),
                    tags::MjTags {
                        target: Some(record.target_template_id.clone()).filter(|id| !id.is_empty()),
                        profile: Some(record.last_profile.clone()).filter(|id| !id.is_empty()),
                        harness: Some(record.harness_kind.id().to_owned()),
                    },
                )
            })
            .collect()
    }

    fn reload(&self) {
        if !self.reload {
            return;
        }
        match Controller::load() {
            Ok(controller) => {
                *self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Sessions::of(&controller.state, Some(&controller.config))
            }
            Err(error) => {
                tracing::warn!(%error, "could not refresh session records for SessionWiki")
            }
        }
    }

    /// The conversation of a stopped session, read from its newest checkpoint,
    /// with the title the checkpoint recorded.
    fn checkpointed_transcript(&self, session_id: &str) -> Result<IndexedTranscript> {
        let (newest, _) = self.newest_archives();
        let archive = newest
            .get(session_id)
            .with_context(|| format!("no checkpoint archive for session {session_id}"))?;
        let snapshot = mj_checkpoint::archive::read_archive_verified(&archive.path)
            .with_context(|| format!("read checkpoint {}", archive.path.display()))?
            .canonical_session()
            .with_context(|| format!("read the transcript of session {session_id}"))?;
        let mut evidence = provenance::Evidence::default();
        for item in &snapshot.transcript {
            if let mj_core::archive::CanonicalTranscriptBody::Tool { call, .. } = &item.body {
                evidence.observe(call, item.created_at_ms);
            }
        }
        let messages = summary_messages(mj_transcript::summary::TranscriptSummary::from_snapshot(
            &snapshot,
        ));
        Ok(IndexedTranscript {
            messages,
            title: snapshot.session.session_title.clone(),
            evidence,
        })
    }

    /// The conversation of a session that has not stopped, read from the
    /// daemon's own projection. It is the same conversation the checkpoint
    /// would hold, minus whatever has not happened yet.
    fn projected_transcript(&self, session_id: &str) -> Result<IndexedTranscript> {
        let projection = crate::database::load_materialized_session(session_id)
            .with_context(|| format!("read the stored transcript of session {session_id}"))?
            .with_context(|| format!("no stored transcript for session {session_id}"))?;
        let mut evidence = provenance::Evidence::default();
        for item in &projection.transcript {
            if let mj_core::state::TranscriptBody::Tool { call, .. } = &item.body {
                evidence.observe(call, item.created_at_ms);
            }
        }
        Ok(IndexedTranscript {
            messages: projected_messages(&projection),
            title: projection.session_title.clone(),
            evidence,
        })
    }

    /// The stable key for one session: its checkpoint directory and id. The
    /// directory is per instance, which is what scopes reconciliation.
    fn key_for(&self, session_id: &str) -> String {
        format!("{}/{session_id}", self.sessions_dir.display())
    }

    /// The newest checkpoint of every session in the directory, by session id.
    ///
    /// `had_error` is true when the directory exists but could not be read in
    /// full; the indexer then skips deletion reconciliation rather than
    /// archiving every Mjolnir session off a partial listing.
    fn newest_archives(&self) -> (BTreeMap<String, ArchiveFile>, bool) {
        let mut newest: BTreeMap<String, ArchiveFile> = BTreeMap::new();
        let mut had_error = false;
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) => {
                if self.sessions_dir.exists() {
                    tracing::debug!(
                        directory = %self.sessions_dir.display(),
                        %error,
                        "could not list the checkpoint directory for SessionWiki"
                    );
                    had_error = true;
                }
                return (newest, had_error);
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                had_error = true;
                continue;
            };
            let Some((session_id, frontier)) = checkpoint_archive_session(&entry.file_name())
            else {
                continue;
            };
            let token = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| age.as_secs() as i64)
                .unwrap_or(0);
            let candidate = ArchiveFile {
                path: entry.path(),
                frontier,
                token,
            };
            match newest.get(&session_id) {
                Some(existing) if existing.frontier >= candidate.frontier => {}
                _ => {
                    newest.insert(session_id, candidate);
                }
            }
        }
        (newest, had_error)
    }
}

struct IndexedTranscript {
    messages: Vec<Message>,
    title: Option<String>,
    evidence: provenance::Evidence,
}

/// The session a checkpoint file name belongs to, with its generation.
///
/// Managed checkpoints carry a frontier and a nonce; an imported archive is
/// named for its session alone and counts as generation zero.
fn checkpoint_archive_session(name: &std::ffi::OsStr) -> Option<(String, u64)> {
    if let Some(parsed) = managed_checkpoint_archive_name(name) {
        return Some((parsed.session_id, parsed.frontier));
    }
    let stem = name
        .to_str()
        .and_then(|name| name.strip_suffix(".hel.zip"))?;
    mj_core::config::validate_id("session", stem)
        .is_ok()
        .then(|| (stem.to_owned(), 0))
}

/// A running session's conversation, as SessionWiki stores it.
fn projected_messages(projection: &mj_core::state::MaterializedSession) -> Vec<Message> {
    summary_messages(mj_transcript::summary::TranscriptSummary::from_materialized(projection))
}

fn summary_messages(summary: mj_transcript::summary::TranscriptSummary) -> Vec<Message> {
    use mj_transcript::summary::SummaryRole;
    summary
        .entries
        .into_iter()
        .filter_map(|entry| {
            let role = match entry.role {
                SummaryRole::User => Role::User,
                SummaryRole::Assistant => Role::Assistant,
                SummaryRole::Tool => Role::Tool,
                SummaryRole::Plan => return None,
            };
            message(role, entry.body(), entry.created_at_ms)
        })
        .collect()
}

/// One indexed message, or nothing when the item carried no text.
fn message(role: Role, text: String, created_at_ms: i64) -> Option<Message> {
    let text = text.trim().to_owned();
    (!text.is_empty()).then(|| Message {
        role,
        text,
        ts: DateTime::from_timestamp_millis(created_at_ms),
    })
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

impl Adapter for MjolnirAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn root(&self) -> Option<PathBuf> {
        Some(self.sessions_dir.clone())
    }

    /// Unused: this is a shared-store adapter, so the indexer enumerates
    /// sessions through [`Adapter::store`] instead of walking files.
    fn discover(&self) -> Discovered {
        Discovered {
            files: Vec::new(),
            had_error: false,
        }
    }

    fn parse(&self, _path: &Path) -> Result<Session> {
        anyhow::bail!("Mjolnir sessions are parsed by key, not by file")
    }

    fn store(&self) -> Option<Store> {
        self.reload();
        let (newest, had_error) = self.newest_archives();
        let mut files = Vec::with_capacity(newest.len());
        let mut tokens: BTreeMap<String, i64> = BTreeMap::new();
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (session_id, archive) in newest {
            if sessions.ownership.owns_session(&session_id) {
                continue;
            }
            tokens.insert(session_id, archive.token);
            files.push(archive.path);
        }
        // A session that is still running is indexed from the projection, and
        // its own token replaces any checkpoint token it has: the conversation
        // has moved on since that checkpoint was written. Listing it also
        // keeps reconciliation from archiving a running session.
        tokens.extend(
            sessions
                .live
                .iter()
                .filter(|(id, _)| !sessions.ownership.owns_session(id))
                .map(|(id, token)| (id.clone(), *token)),
        );
        // A rename changes the record and not the conversation, so the
        // record's own last update is part of the change token. Without it a
        // renamed session would keep its old title in the index for as long as
        // its transcript stood still.
        for (session_id, token) in tokens.iter_mut() {
            let updated = sessions
                .records
                .get(session_id)
                .and_then(|record| parse_time(&record.updated_at))
                .map(|updated| updated.timestamp());
            if let Some(updated) = updated {
                *token = (*token).max(updated);
            }
        }
        let keys = tokens
            .into_iter()
            .map(|(session_id, token)| (self.key_for(&session_id), session_change_token(token)))
            .collect();
        Some(Store {
            keys,
            files,
            had_error,
        })
    }

    /// Every Mjolnir instance publishes under one tool name, so this instance
    /// speaks only for keys under its own checkpoint directory. Without the
    /// scope, two instances would archive each other's rows on every sync.
    fn reconcile_scope(&self) -> Option<String> {
        Some(format!("{}/", self.sessions_dir.display()))
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let session_id = key.rsplit('/').next().unwrap_or_default();
        anyhow::ensure!(!session_id.is_empty(), "no session id in key {key:?}");
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            !sessions.ownership.owns_session(session_id),
            "sub-agent sessions are not indexed"
        );
        let IndexedTranscript {
            messages,
            title: snapshot_title,
            evidence,
        } = if sessions.live.contains_key(session_id) {
            self.projected_transcript(session_id)?
        } else {
            self.checkpointed_transcript(session_id)?
        };
        let record = sessions.records.get(session_id);
        let project = match sessions.project_directories.get(session_id) {
            Some(Ok(Some(directory))) => directory.display().to_string(),
            Some(Ok(None)) => String::new(),
            Some(Err(error)) => {
                anyhow::bail!("resolve the session project directory: {error}")
            }
            None => record
                .and_then(|record| record.checkout().project_directory())
                .map(|directory| directory.display().to_string())
                .unwrap_or_default(),
        };

        let title = record
            .and_then(|record| record.session_title_override.clone())
            .or_else(|| record.and_then(|record| record.acp_session_title.clone()))
            .or_else(|| snapshot_title.clone())
            .unwrap_or_else(|| {
                messages
                    .iter()
                    .find(|message| message.role == Role::User)
                    .map(|message| message.text.chars().take(80).collect())
                    .unwrap_or_default()
            });

        Ok(Session {
            id: session_id.to_owned(),
            tool: TOOL,
            path: PathBuf::from(key),
            project,
            started: record.and_then(|record| parse_time(&record.created_at)),
            ended: record.and_then(|record| parse_time(&record.updated_at)),
            title,
            subagent: sessions.ownership.owns_session(session_id),
            messages,
            touched: evidence.paths.into_iter().collect(),
            edits: evidence.edits,
        })
    }
}

/// The Mjolnir adapter handed to the indexer while the sync keeps its own
/// handle on it.
///
/// The indexer takes `Box<dyn Adapter>` and consumes the list, but the sync has
/// to ask the same adapter for its final session snapshot once the walk is over
/// (see [`MjolnirAdapter::indexed_tags`]). Sharing the adapter is the only way
/// both can hold it.
struct SharedMjolnirAdapter(Arc<MjolnirAdapter>);

impl Adapter for SharedMjolnirAdapter {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn root(&self) -> Option<PathBuf> {
        self.0.root()
    }

    fn discover(&self) -> Discovered {
        self.0.discover()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        self.0.parse(path)
    }

    fn store(&self) -> Option<Store> {
        self.0.store()
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        self.0.parse_key(key)
    }

    fn reconcile_scope(&self) -> Option<String> {
        self.0.reconcile_scope()
    }
}

/// The daemon's SessionWiki sync job.
///
/// Triggers coalesce: a request while a sync is running marks a rerun instead
/// of queueing a second one, so a burst of closing sessions costs one extra
/// pass. Syncs are single-flight because SessionWiki holds a write transaction
/// per adapter batch, and two writers only produce a busy error.
pub struct WikiIndexer {
    inner: Arc<Indexer>,
}

#[derive(Default)]
struct Indexer {
    /// Held for the whole of one run: this is what makes syncs single-flight.
    running: tokio::sync::Mutex<()>,
    notify: tokio::sync::Notify,
    /// A trigger arrived; the worker has not consumed it yet.
    requested: AtomicBool,
    /// At least one waiting trigger asked for a full sync.
    full_requested: AtomicBool,
    /// A sync pass is running now. A surface shows this as "topping up", so a
    /// user knows more results may arrive.
    in_flight: AtomicBool,
    last_success: std::sync::Mutex<Option<Success>>,
    native_scan_cache: crate::import::NativeScanCache,
}

#[derive(Clone, Copy)]
struct Success {
    at: Instant,
    epoch_seconds: i64,
}

impl WikiIndexer {
    /// Start the background sync worker. Without a Tokio runtime (some tests
    /// build a runtime state without one) the indexer stays inert.
    pub fn spawn() -> Self {
        let inner = Arc::new(Indexer {
            native_scan_cache: crate::import::NativeScanCache::shared(),
            ..Indexer::default()
        });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let worker = Arc::clone(&inner);
            handle.spawn(async move { worker.run().await });
        }
        Self { inner }
    }

    /// Ask for a sync. Returns immediately; the work happens in the background.
    pub fn request_sync(&self, full: bool) {
        if full {
            self.inner.full_requested.store(true, Ordering::Release);
        }
        self.inner.requested.store(true, Ordering::Release);
        self.inner.notify.notify_one();
    }

    /// An indexer with no worker, so a test can see a request that nothing
    /// takes and no test runs a sync against the real index.
    #[cfg(test)]
    pub(crate) fn inert() -> Self {
        Self {
            inner: Arc::new(Indexer::default()),
        }
    }

    /// Whether a sync has been requested and not yet taken by the worker.
    #[cfg(test)]
    pub(crate) fn sync_requested(&self) -> bool {
        self.inner.requested.load(Ordering::Acquire)
    }

    /// Run a sync and wait for it, joining a sync already in flight.
    pub async fn sync_now(&self, full: bool) -> Result<()> {
        self.inner.sync(full).await
    }

    /// The state of the index and whether a sync is running, for the surfaces
    /// that say so while the first build is under way.
    pub fn status(&self) -> WikiStatus {
        WikiStatus {
            state: index_state(),
            topping_up: self.inner.in_flight.load(Ordering::Acquire)
                || self.inner.requested.load(Ordering::Acquire),
        }
    }

    /// When the last sync succeeded, for callers that trigger on staleness.
    pub fn last_success(&self) -> Option<Instant> {
        self.inner
            .last_success
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|success| success.at)
    }
}

impl Indexer {
    async fn run(self: Arc<Self>) {
        loop {
            self.notify.notified().await;
            while self.requested.swap(false, Ordering::AcqRel) {
                let full = self.full_requested.swap(false, Ordering::AcqRel);
                if let Err(error) = self.sync(full).await {
                    self.report(&error);
                    // A failure waits for the next trigger rather than
                    // retrying straight away: a busy index stays busy for as
                    // long as the other writer holds it, and a spin would only
                    // add to the contention.
                    break;
                }
            }
        }
    }

    /// Log a failed sync at the level its cause deserves. A busy index is an
    /// expected collision with another writer, not a fault: mark a rerun and
    /// say so only in debug output.
    fn report(&self, error: &anyhow::Error) {
        if crate::database::is_busy_error(error) {
            self.requested.store(true, Ordering::Release);
            tracing::debug!(%error, "the SessionWiki index was busy; retrying on the next trigger");
        } else {
            tracing::warn!(%error, "could not sync sessions into SessionWiki");
        }
    }

    async fn sync(&self, full: bool) -> Result<()> {
        let _guard = self.running.lock().await;
        let since = if full {
            None
        } else {
            self.last_success
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                // A minute of overlap covers checkpoints written while the
                // previous run was reading the directory.
                .map(|success| success.epoch_seconds - 60)
        };
        let started = Instant::now();
        self.in_flight.store(true, Ordering::Release);
        let cache = self.native_scan_cache.clone();
        let ran = run_abandonable(move || sync_blocking(since, &cache)).await;
        self.in_flight.store(false, Ordering::Release);
        let ran = ran?;
        if ran {
            *self
                .last_success
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Success {
                at: started,
                epoch_seconds: Utc::now().timestamp(),
            });
        }
        Ok(())
    }
}

/// Run one sync pass on a thread of its own, outside daemon upgrade admission
/// and outside the runtime's blocking pool.
///
/// A pass walks every native session store and can take minutes. It is safe to
/// stop at any point: SessionWiki writes its index in SQLite transactions,
/// which roll back when the process exits, and every daemon syncs again when
/// it starts. So a daemon handoff must not wait for it. Holding admission made
/// the handoff wait for the whole pass, and the daemon process waits for its
/// blocking pool when it exits, so a pass there would hold up the exit
/// instead. On this thread the exiting process abandons the pass.
async fn run_abandonable<T: Send + 'static>(
    pass: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("sessionwiki-sync".to_owned())
        .spawn(move || {
            // The receiver is gone only when the caller was dropped; the
            // pass has nobody left to report to.
            let _ = sender.send(pass());
        })
        .context("start the SessionWiki sync thread")?;
    receiver
        .await
        .context("the SessionWiki sync thread stopped without an answer")?
}

/// One synchronous sync pass. Returns false when this process must not touch
/// the index, so a refused run never records a success it did not have.
fn sync_blocking(since: Option<i64>, cache: &crate::import::NativeScanCache) -> Result<bool> {
    if !index_is_writable() {
        return Ok(false);
    }
    // Isolated tests park a pass here to stand for one that takes minutes.
    mj_core::test_hooks::reach_test_hook("sessionwiki_sync_pass")?;
    let controller =
        Controller::load().context("load controller state for the SessionWiki sync")?;
    // Mjolnir's own sessions go first: a cold index walks every other tool's
    // store for many minutes, and a just-closed session should not wait on it.
    // Cleanup and enumeration use one ownership snapshot. Native discovery
    // happens afterwards, so it cannot make this snapshot stale before use.
    let mjolnir = Arc::new(MjolnirAdapter::from_state_with_config(
        &controller.state,
        &controller.config,
    ));
    let owned: Vec<Box<dyn Adapter>> = vec![Box::new(SharedMjolnirAdapter(Arc::clone(&mjolnir)))];
    let ownership = mjolnir
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ownership
        .clone();
    let mut connection = sessionwiki::index::open().context("open the SessionWiki index")?;
    top_level::prune(&mut connection, &BTreeSet::new(), &ownership)?;
    sessionwiki::index::sync_with(&mut connection, &owned, since)
        .context("sync Mjolnir sessions into SessionWiki")?;
    let (native, excluded) = top_level::prepare(native_adapters(&controller.config), cache);
    top_level::prune(&mut connection, &excluded, &ownership)?;
    sessionwiki::index::sync_with(&mut connection, &native, since)
        .context("sync native sessions into SessionWiki")?;
    top_level::prune(&mut connection, &excluded, &ownership)?;
    write_session_tags(&mut connection, &mjolnir.indexed_tags())
        .context("store Mjolnir's session metadata in the SessionWiki index")?;
    provenance::backfill(&mut connection, &mjolnir).context("backfill Mjolnir file provenance")?;
    if since.is_none() {
        // A full pass has walked every store, so the index is complete enough
        // for a search to be trusted. The marker is what a later daemon reads
        // instead of walking the corpus again to find out.
        record_first_build();
    }
    Ok(true)
}

/// Store each session's target, profile and harness in the index, in one
/// transaction.
///
/// Every session is written on every sync rather than only the changed ones:
/// the write is a delete and three inserts, which is nothing beside the
/// transcript indexing in the same pass, and it is what makes a Move or a
/// profile switch show up without tracking which records changed. It is also
/// what gives sessions indexed before this existed their metadata, with no
/// migration and no re-index.
fn write_session_tags(
    connection: &mut rusqlite::Connection,
    session_tags: &BTreeMap<String, tags::MjTags>,
) -> Result<()> {
    if session_tags.is_empty() {
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .context("open a transaction for the session metadata")?;
    for (session_id, session) in session_tags {
        if session.is_empty() {
            continue;
        }
        tags::write(&transaction, session_id, session)?;
    }
    transaction
        .commit()
        .context("commit the session metadata")?;
    Ok(())
}

/// The non-Mjolnir adapters this install indexes.
///
/// Mjolnir's configured harness profiles decide which harness homes are
/// indexed, not the stock `~/.codex` and `~/.claude` locations. A user who
/// runs several profile homes expects every session Mjolnir can start to be
/// searchable, and a home no profile names is not Mjolnir's to walk. So the
/// stock Codex and Claude adapters are dropped and one adapter per enabled
/// profile home takes their place; of the rest, only OpenCode is kept, the
/// only other harness Mjolnir can start. Every other built-in adapter (aider,
/// gemini, cline, ...) is dropped, so a tool Mjolnir cannot run can neither
/// trigger a home-directory walk nor add rows Resume cannot act on.
///
/// Kimi Code, Grok Build and Muse have no SessionWiki adapter at all, so
/// Mjolnir supplies one per enabled profile home of its own (see
/// [`harness_adapters`]). Without them those sessions would never appear in
/// the Resume dialog's search.
///
/// Each per-home adapter reports a reconcile scope covering only its own root,
/// so a sync of one install never archives the rows of another.
fn native_adapters(config: &mj_core::config::Config) -> Vec<Box<dyn Adapter>> {
    // Two profiles may share one home, and two harnesses may share one home
    // path without sharing sessions, so the kind is part of the identity.
    let mut seen: BTreeSet<(HarnessKind, &Path)> = BTreeSet::new();
    let mut adapters: Vec<Box<dyn Adapter>> = Vec::new();
    for (_, profile) in config.enabled_profiles() {
        // A second adapter for the same home would only walk it twice.
        if !seen.insert((profile.kind, profile.home.as_path())) {
            continue;
        }
        let adapter: Box<dyn Adapter> = match profile.kind {
            HarnessKind::Codex => {
                Box::new(sessionwiki::adapters::Codex::in_home(profile.home.clone()))
            }
            HarnessKind::Claude => Box::new(sessionwiki::adapters::ClaudeCode::in_home(
                profile.home.clone(),
            )),
            kind => match HarnessAdapter::in_home(kind, profile.home.clone()) {
                Some(adapter) => Box::new(adapter),
                None => continue,
            },
        };
        adapters.push(adapter);
    }
    adapters.extend(sessionwiki::adapters::all().into_iter().filter(|adapter| {
        harness_adapters::harness_for_tool(adapter.name()) == Some(HarnessKind::OpenCode)
    }));
    adapters
}

// ---------------------------------------------------------------------------
// Which index, and whether it may be touched
// ---------------------------------------------------------------------------

/// Whether this process may open the index at all.
///
/// Indexing is always on, so a process that never resolved where its index
/// belongs must not reach for one: it would walk the user's real session
/// stores and write the user's real index. Only Mjolnir's own startup resolves
/// it (see `mj_core::config::apply_instance_flag`), so this refuses every unit
/// test that builds a daemon runtime directly and every other embedder, unless
/// it names an index of its own with `SESSIONWIKI_DATA`.
fn index_is_isolated() -> bool {
    static SAID: AtomicBool = AtomicBool::new(false);
    if mj_core::config::session_index_is_resolved()
        || std::env::var_os(mj_core::config::SESSION_INDEX_ENV).is_some()
    {
        return true;
    }
    if !SAID.swap(true, Ordering::AcqRel) {
        tracing::debug!(
            "this process did not resolve a session index location; SessionWiki is not used"
        );
    }
    false
}

/// Whether the index on disk was written by a SessionWiki at another schema
/// version.
///
/// SessionWiki's own `open` drops and rebuilds its whole cache when the file's
/// `user_version` differs from the version it was built with, which on a large
/// corpus costs tens of minutes. Mjolnir will not do that to a user who also
/// runs the `sessionwiki` command: it reads the version without SessionWiki and
/// stands aside.
fn index_version_mismatch() -> bool {
    static SAID: AtomicBool = AtomicBool::new(false);
    let Ok(path) = sessionwiki::index::db_path() else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    let version = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .and_then(|connection| connection.pragma_query_value(None, "user_version", |row| row.get(0)));
    let version: i64 = match version {
        Ok(version) => version,
        Err(error) => {
            tracing::debug!(%error, "could not read the SessionWiki index schema version");
            return false;
        }
    };
    // Zero is an index SessionWiki has not finished creating; it is not a
    // different version.
    let mismatch = version != 0 && version != sessionwiki::index::SCHEMA_VERSION;
    if mismatch && !SAID.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            found = version,
            expected = sessionwiki::index::SCHEMA_VERSION,
            path = %path.display(),
            "the SessionWiki index was written by another version;              Mjolnir will not open it, because opening it would rebuild it.              Install the matching sessionwiki command"
        );
    }
    mismatch
}

fn index_is_writable() -> bool {
    index_is_isolated() && !index_version_mismatch()
}

/// The file recording that one full sync has completed, holding the schema
/// version it completed at.
fn first_build_marker() -> PathBuf {
    mj_core::config::data_dir().join("sessionwiki-built")
}

fn record_first_build() {
    let path = first_build_marker();
    let version = sessionwiki::index::SCHEMA_VERSION.to_string();
    if std::fs::read_to_string(&path).is_ok_and(|held| held.trim() == version) {
        return;
    }
    if let Err(error) = std::fs::write(&path, &version) {
        tracing::warn!(%error, path = %path.display(), "could not record the first SessionWiki build");
    }
}

/// Whether this index has completed a full build at this schema version.
fn first_build_is_done() -> bool {
    std::fs::read_to_string(first_build_marker())
        .is_ok_and(|held| held.trim() == sessionwiki::index::SCHEMA_VERSION.to_string())
        && sessionwiki::index::db_path().is_ok_and(|path| path.exists())
}

/// What a surface should say about this index right now.
pub fn index_state() -> WikiIndexState {
    if !index_is_isolated() {
        return WikiIndexState::Indexing;
    }
    if index_version_mismatch() {
        return WikiIndexState::VersionMismatch;
    }
    if first_build_is_done() {
        WikiIndexState::Ready
    } else {
        WikiIndexState::Indexing
    }
}

// ---------------------------------------------------------------------------
// Indexing a session before it is destroyed
// ---------------------------------------------------------------------------

/// How long a destroy waits for a sync pass before it indexes the session on
/// its own. A first build can run for many minutes, and a destroy must not
/// wait for it.
pub const DESTROY_SYNC_WAIT: Duration = Duration::from_secs(20);

/// How often rows the index was too busy to take are offered again, and for
/// how long. A first build holds the index while it parses one tool's
/// sessions and frees it between tools.
const DEFERRED_WRITE_RETRY: Duration = Duration::from_secs(5);
const DEFERRED_WRITE_LIMIT: Duration = Duration::from_secs(60 * 60);

/// How a session about to be destroyed, with its sub-agents, reached the
/// index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexedBeforeDestroy {
    /// The index already held every one of them as they are now, or none of
    /// them had a conversation to index.
    Current,
    /// A sync pass that began after the request finished in time.
    Synced,
    /// The sync pass did not finish in time, so their rows were written on
    /// their own.
    WrittenDirectly,
    /// The index was busy. Their rows were read before the destroy and are
    /// written in the background once the index is free.
    Deferred,
    /// This process may not write the index, and why.
    Unavailable(&'static str),
    /// Indexing failed, and why. The destroy goes ahead.
    Failed(String),
}

impl WikiIndexer {
    /// Put a top-level session into the index while its record and stored
    /// conversation still exist. Child cleanup never requires an index copy.
    ///
    /// Sessions enter the index only on a sync pass, and destroy deletes the
    /// record and the conversation, so a session created and destroyed
    /// between two passes was never findable (R2-11). This asks for an
    /// incremental pass and waits `wait` for it. A pass that does not finish
    /// in time, such as a first build, is left running, and the sessions are
    /// indexed on their own from the same rows the pass would write.
    pub async fn index_before_destroy(
        &self,
        session_id: &str,
        wait: Duration,
    ) -> IndexedBeforeDestroy {
        if let Some(reason) = unwritable_reason() {
            return IndexedBeforeDestroy::Unavailable(reason);
        }
        let root = session_id.to_owned();
        let pending = match tokio::task::spawn_blocking(move || unindexed_session(&root)).await {
            Ok(Ok(pending)) => pending,
            Ok(Err(error)) => {
                return IndexedBeforeDestroy::Failed(format!(
                    "could not tell whether the index holds the session: {error:#}"
                ));
            }
            Err(error) => {
                return IndexedBeforeDestroy::Failed(format!(
                    "checking the index for the session stopped: {error}"
                ));
            }
        };
        if pending.is_empty() {
            return IndexedBeforeDestroy::Current;
        }
        let inner = Arc::clone(&self.inner);
        index_before_destroy_with(
            async move { inner.sync(false).await },
            wait,
            move || capture_sessions(&pending),
            DEFERRED_WRITE_RETRY,
        )
        .await
    }
}

/// Why this process may not write the index, if it may not.
fn unwritable_reason() -> Option<&'static str> {
    if !index_is_isolated() {
        return Some("this process did not resolve a SessionWiki index of its own");
    }
    if index_version_mismatch() {
        return Some("the SessionWiki index was written by another SessionWiki version");
    }
    None
}

/// The bounded wait and its fallback, with the sync pass and the reading of
/// the sessions passed in so a test can stand in for either.
async fn index_before_destroy_with<S, C>(
    sync: S,
    wait: Duration,
    capture: C,
    retry: Duration,
) -> IndexedBeforeDestroy
where
    S: std::future::Future<Output = Result<()>> + Send + 'static,
    C: FnOnce() -> Result<Vec<CapturedSession>> + Send + 'static,
{
    // Spawned rather than awaited here, so a wait that runs out drops only
    // the handle and the pass still finishes. Dropping `Indexer::sync`
    // part-way would release the single-flight lock while its blocking half
    // was still writing.
    match tokio::time::timeout(wait, tokio::spawn(sync)).await {
        Ok(Ok(Ok(()))) => return IndexedBeforeDestroy::Synced,
        Ok(Ok(Err(error))) => tracing::warn!(
            error = %format!("{error:#}"),
            "the SessionWiki sync before a destroy failed; indexing the session on its own"
        ),
        Ok(Err(error)) => tracing::warn!(
            %error,
            "the SessionWiki sync before a destroy stopped; indexing the session on its own"
        ),
        Err(_) => tracing::info!(
            wait_seconds = wait.as_secs_f64(),
            "the SessionWiki sync did not finish in time; indexing the session on its own"
        ),
    }
    let captured = match tokio::task::spawn_blocking(capture).await {
        Ok(Ok(captured)) => Arc::new(captured),
        Ok(Err(error)) => {
            return IndexedBeforeDestroy::Failed(format!(
                "could not read the session to index it: {error:#}"
            ));
        }
        Err(error) => {
            return IndexedBeforeDestroy::Failed(format!(
                "reading the session to index it stopped: {error}"
            ));
        }
    };
    if captured.is_empty() {
        return IndexedBeforeDestroy::Current;
    }
    let attempt = Arc::clone(&captured);
    match tokio::task::spawn_blocking(move || write_captured(&attempt)).await {
        Ok(Ok(())) => IndexedBeforeDestroy::WrittenDirectly,
        Ok(Err(error)) if crate::database::is_busy_error(&error) => {
            write_captured_later(captured, retry);
            IndexedBeforeDestroy::Deferred
        }
        Ok(Err(error)) => IndexedBeforeDestroy::Failed(format!(
            "could not write the session into the index: {error:#}"
        )),
        Err(error) => IndexedBeforeDestroy::Failed(format!(
            "writing the session into the index stopped: {error}"
        )),
    }
}

/// A top-level session whose current conversation the index does not hold.
fn unindexed_session(root: &str) -> Result<Vec<String>> {
    let controller =
        Controller::load().context("load controller state to index a destroyed session")?;
    if !controller.state.sessions.contains_key(root) {
        return Ok(Vec::new());
    }
    unindexed(
        &MjolnirAdapter::from_state_with_config(&controller.state, &controller.config),
        &[root.to_owned()],
    )
}

/// Those of `session_ids` that have a conversation the index does not hold
/// as it is now: no row, an archived row, or a row with an older change
/// token than the adapter lists.
fn unindexed(adapter: &MjolnirAdapter, session_ids: &[String]) -> Result<Vec<String>> {
    let tokens: BTreeMap<String, i64> = adapter
        .store()
        .map(|store| store.keys.into_iter().collect())
        .unwrap_or_default();
    // No index yet holds nothing.
    let connection = open_readonly().ok();
    let mut pending = Vec::new();
    for session_id in session_ids {
        let key = adapter.key_for(session_id);
        // Only a session with a stored or checkpointed conversation is listed.
        let Some(&token) = tokens.get(&key) else {
            continue;
        };
        let current = match &connection {
            Some(connection) => indexed_token(connection, &key)? == Some(token),
            None => false,
        };
        if !current {
            pending.push(session_id.clone());
        }
    }
    Ok(pending)
}

/// The change token the index holds for a live row. SessionWiki stores a
/// shared-store token in the `mtime` column.
fn indexed_token(connection: &rusqlite::Connection, key: &str) -> Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    connection
        .query_row(
            "SELECT mtime FROM files WHERE path = ?1 AND archived_at IS NULL",
            [key],
            |row| row.get(0),
        )
        .optional()
        .context("read a session's change token from the SessionWiki index")
}

/// One session's index row and metadata, read while its record and
/// conversation still exist, so they can be written after both are gone.
struct CapturedSession {
    key: String,
    token: i64,
    session: Session,
    tags: tags::MjTags,
}

fn capture_sessions(session_ids: &[String]) -> Result<Vec<CapturedSession>> {
    let controller =
        Controller::load().context("load controller state to index a destroyed session")?;
    capture_sessions_from(
        &MjolnirAdapter::from_state_with_config(&controller.state, &controller.config),
        session_ids,
    )
}

/// The rows a sync pass would write for these sessions, built by the same
/// adapter. A session with no conversation to index is left out.
fn capture_sessions_from(
    adapter: &MjolnirAdapter,
    session_ids: &[String],
) -> Result<Vec<CapturedSession>> {
    let tokens: BTreeMap<String, i64> = adapter
        .store()
        .map(|store| store.keys.into_iter().collect())
        .unwrap_or_default();
    let mut session_tags = adapter.indexed_tags();
    let mut captured = Vec::new();
    for session_id in session_ids {
        let key = adapter.key_for(session_id);
        let Some(&token) = tokens.get(&key) else {
            continue;
        };
        captured.push(CapturedSession {
            session: adapter.parse_key(&key)?,
            tags: session_tags.remove(session_id).unwrap_or_default(),
            key,
            token,
        });
    }
    Ok(captured)
}

/// Write captured rows through SessionWiki's own indexing, one session at a
/// time, and their metadata beside them.
fn write_captured(captured: &Arc<Vec<CapturedSession>>) -> Result<()> {
    anyhow::ensure!(
        index_is_writable(),
        "this process may not write the SessionWiki index"
    );
    let mut connection = sessionwiki::index::open().context("open the SessionWiki index")?;
    for index in 0..captured.len() {
        let adapter: Box<dyn Adapter> = Box::new(CapturedAdapter {
            captured: Arc::clone(captured),
            index,
        });
        sessionwiki::index::sync_with(&mut connection, &[adapter], None)
            .context("index a session before it is destroyed")?;
    }
    let session_tags = captured
        .iter()
        .map(|captured| (captured.session.id.clone(), captured.tags.clone()))
        .collect();
    write_session_tags(&mut connection, &session_tags)
        .context("store Mjolnir's session metadata in the SessionWiki index")
}

/// Offer rows the index was too busy to take until it takes them, or until
/// [`DEFERRED_WRITE_LIMIT`] passes. The rows live only in this task: a
/// daemon that stops before the index is free loses them, and the sessions
/// logged here are then not found by id.
fn write_captured_later(captured: Arc<Vec<CapturedSession>>, retry: Duration) {
    let sessions = captured
        .iter()
        .map(|captured| captured.session.id.clone())
        .collect::<Vec<_>>();
    tracing::info!(
        ?sessions,
        "the SessionWiki index is busy; indexing the destroyed sessions once it is free"
    );
    tokio::spawn(async move {
        let started = Instant::now();
        loop {
            tokio::time::sleep(retry).await;
            let attempt = Arc::clone(&captured);
            let error = match tokio::task::spawn_blocking(move || write_captured(&attempt)).await {
                Ok(Ok(())) => {
                    tracing::info!(?sessions, "indexed the destroyed sessions");
                    return;
                }
                Ok(Err(error)) => error,
                Err(error) => anyhow::Error::new(error),
            };
            if !crate::database::is_busy_error(&error) || started.elapsed() >= DEFERRED_WRITE_LIMIT
            {
                tracing::warn!(
                    ?sessions,
                    error = %format!("{error:#}"),
                    "gave up indexing destroyed sessions in SessionWiki"
                );
                return;
            }
        }
    });
}

/// One captured session, offered to SessionWiki as a shared store that
/// lists only it.
struct CapturedAdapter {
    captured: Arc<Vec<CapturedSession>>,
    index: usize,
}

impl CapturedAdapter {
    fn captured(&self) -> &CapturedSession {
        &self.captured[self.index]
    }
}

impl Adapter for CapturedAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn root(&self) -> Option<PathBuf> {
        Path::new(&self.captured().key)
            .parent()
            .map(Path::to_path_buf)
    }

    fn discover(&self) -> Discovered {
        Discovered {
            files: Vec::new(),
            had_error: false,
        }
    }

    fn parse(&self, _path: &Path) -> Result<Session> {
        anyhow::bail!("Mjolnir sessions are parsed by key, not by file")
    }

    fn store(&self) -> Option<Store> {
        let captured = self.captured();
        Some(Store {
            keys: vec![(captured.key.clone(), captured.token)],
            files: Vec::new(),
            had_error: false,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let captured = self.captured();
        anyhow::ensure!(key == captured.key, "no captured session for key {key:?}");
        Ok(copy_session(&captured.session))
    }

    /// A prefix no key starts with, since keys hold no NUL. This store lists
    /// one session, not every session of this instance, so reconciliation
    /// must not archive the rows it does not list.
    fn reconcile_scope(&self) -> Option<String> {
        Some(format!("{}\0", self.captured().key))
    }
}

/// A copy of an indexed session, for a write that may be retried.
/// SessionWiki's model does not implement `Clone`.
fn copy_session(session: &Session) -> Session {
    Session {
        id: session.id.clone(),
        tool: session.tool,
        path: session.path.clone(),
        project: session.project.clone(),
        started: session.started,
        ended: session.ended,
        title: session.title.clone(),
        subagent: session.subagent,
        messages: session
            .messages
            .iter()
            .map(|message| Message {
                role: message.role,
                text: message.text.clone(),
                ts: message.ts,
            })
            .collect(),
        touched: session.touched.clone(),
        edits: session
            .edits
            .iter()
            .map(|edit| sessionwiki::model::EditEvent {
                path: edit.path.clone(),
                kind: edit.kind,
                snippet: edit.snippet.clone(),
                ts: edit.ts,
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Queries and restore
// ---------------------------------------------------------------------------

/// The largest page a caller may ask a wiki query for.
pub const MAX_WIKI_LIMIT: usize = 200;
/// The page size a caller that names none gets.
pub const DEFAULT_WIKI_LIMIT: usize = 50;
/// SessionWiki's full-text index needs three characters; shorter queries fall
/// back to a substring scan.
const MIN_FULLTEXT_QUERY: usize = 3;
/// How stale the index may be before a query triggers a background sync.
pub const SYNC_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a query should trigger a bounded background sync before it answers.
pub fn sync_is_stale(last_success: Option<Instant>) -> bool {
    last_success.is_none_or(|at| at.elapsed() >= SYNC_STALE_AFTER)
}

/// One page of the index, newest first or best match first.
///
/// `live` is the set of session ids this daemon still holds, which is what
/// decides whether a Mjolnir row names a session the user can simply resume.
/// Only top-level sessions are answered. `include_tool_matches` keeps tool-only
/// hits for agent history; the resume list requires conversational matches.
/// Runs SQLite work, so callers on the async runtime wrap it in
/// `spawn_blocking`.
pub fn query_rows(
    query: &str,
    limit: usize,
    live: &BTreeSet<String>,
    include_tool_matches: bool,
) -> Result<Vec<WikiRow>> {
    let limit = limit.clamp(1, MAX_WIKI_LIMIT);
    if !index_is_writable() {
        // Nothing to answer from: either this process has no index of its own
        // or the one on disk is at another version. The status beside the rows
        // says which.
        return Ok(Vec::new());
    }
    let connection = open_readonly()?;
    let ownership = top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    query_rows_from(
        &connection,
        query,
        limit,
        live,
        include_tool_matches,
        &ownership,
        &cache,
    )
}

fn query_rows_from(
    connection: &rusqlite::Connection,
    query: &str,
    limit: usize,
    live: &BTreeSet<String>,
    include_tool_matches: bool,
    ownership: &top_level::Snapshot,
    cache: &crate::import::NativeScanCache,
) -> Result<Vec<WikiRow>> {
    let limit = limit.clamp(1, MAX_WIKI_LIMIT);
    let query = query.trim();
    if query.is_empty() {
        let rows = sessionwiki::index::recent(connection, MAX_WIKI_LIMIT, None, None, None, true)
            .context("list recent SessionWiki sessions")?;
        let mut visible = Vec::with_capacity(limit);
        for row in rows {
            if visible.len() >= limit {
                break;
            }
            if top_level::is_child(&row, ownership, cache)? {
                continue;
            }
            visible.push(wiki_row(row, None, live));
        }
        let mut rows = visible;
        fill_session_tags(connection, &mut rows)?;
        return Ok(rows);
    }
    // SessionWiki filters conversational roles before its candidate cap and
    // session grouping. Native-child filtering still happens here, so
    // over-fetch to keep children ranked first from shortening a page.
    let search_limit = MAX_WIKI_LIMIT;
    let roles = if include_tool_matches {
        None
    } else {
        Some(CONVERSATION_ROLES)
    };
    let hits =
        sessionwiki::index::search_with_roles(connection, query, search_limit, None, None, roles)
            .context("search the SessionWiki index")?;
    let mut rows = Vec::with_capacity(hits.len().min(limit));
    for hit in hits {
        if rows.len() >= limit {
            break;
        }
        if top_level::is_child(&hit.row, ownership, cache)? {
            continue;
        }
        rows.push(wiki_row(hit.row, Some(hit.snippet), live));
    }
    // SessionWiki searches message text alone, so a session known by a title
    // or a project that is never said out loud would be unfindable. Those
    // matches follow the full-text ones rather than displacing them.
    if rows.len() < limit {
        let mut found: BTreeSet<String> = rows.iter().map(|row| row.id.clone()).collect();
        for row in named_like(connection, query)? {
            if rows.len() >= limit {
                break;
            }
            if found.contains(&row.session_id) || top_level::is_child(&row, ownership, cache)? {
                continue;
            }
            found.insert(row.session_id.clone());
            rows.push(wiki_row(row, None, live));
        }
    }
    fill_session_tags(connection, &mut rows)?;
    Ok(rows)
}

/// How many message rows one text search reads before it stops. A term common
/// enough to pass this can miss sessions whose only match ranks past it; the
/// person narrows the query, as with the resume dialog's own search.
const TEXT_SEARCH_MESSAGE_LIMIT: i64 = 20_000;

/// The live Mjolnir sessions whose user or agent messages contain `query`,
/// ignoring case, and where: a session with a user match reports the user
/// match. Tool messages never match. Runs SQLite work, so callers on the async
/// runtime wrap it in `spawn_blocking`.
pub fn session_text_matches(query: &str, live: &BTreeSet<String>) -> Result<Vec<SessionTextMatch>> {
    let query = query.trim();
    if query.is_empty() || !index_is_writable() {
        return Ok(Vec::new());
    }
    let connection = open_readonly()?;
    let ownership = top_level::current_snapshot()?;
    text_matches_in(&connection, query, live, &ownership)
}

fn text_matches_in(
    connection: &rusqlite::Connection,
    query: &str,
    live: &BTreeSet<String>,
    ownership: &top_level::Snapshot,
) -> Result<Vec<SessionTextMatch>> {
    let mut statement;
    let rows = if query.chars().count() < MIN_FULLTEXT_QUERY {
        // Too short for the trigram index; scan the newest messages instead.
        let pattern = format!(
            "%{}%",
            sessionwiki::util::nfc(query)
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        statement = connection.prepare(
            "SELECT f.session_id, f.path, m.role, f.kind
             FROM messages m JOIN files f ON f.session_id = m.session_id
             WHERE f.tool = ?1 AND m.role IN ('user', 'assistant')
               AND m.text LIKE ?2 ESCAPE '\\'
             ORDER BY m.id DESC LIMIT ?3",
        )?;
        statement
            .query_map(
                rusqlite::params![TOOL, pattern, TEXT_SEARCH_MESSAGE_LIMIT],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
    } else {
        let phrase = format!("\"{}\"", sessionwiki::util::nfc(query).replace('"', "\"\""));
        statement = connection.prepare(
            "SELECT f.session_id, f.path, m.role, f.kind
             FROM (SELECT rowid AS mid FROM msgs WHERE msgs MATCH ?2 LIMIT ?3) x
             JOIN messages m ON m.id = x.mid
             JOIN files f ON f.session_id = m.session_id
             WHERE f.tool = ?1 AND m.role IN ('user', 'assistant')",
        )?;
        statement
            .query_map(
                rusqlite::params![TOOL, phrase, TEXT_SEARCH_MESSAGE_LIMIT],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
    }
    .context("search the indexed messages")?;
    let mut found = BTreeMap::<String, SessionTextMatchKind>::new();
    for (session_id, path, role, kind) in rows {
        if !live.contains(&session_id)
            || ownership.owns_indexed_child(&session_id, &path, TOOL, &kind)
        {
            continue;
        }
        let kind = if role == "user" {
            SessionTextMatchKind::User
        } else {
            SessionTextMatchKind::Agent
        };
        let entry = found.entry(session_id.clone()).or_insert(kind);
        *entry = (*entry).min(kind);
    }
    Ok(found
        .into_iter()
        .map(|(session_id, kind)| SessionTextMatch { session_id, kind })
        .collect())
}

/// Fill in the target, profile and harness of every Mjolnir row on this page
/// from the index's own tags, in one query.
///
/// Only Mjolnir writes those tags, so a row from another tool keeps `None` and
/// is not even asked about.
fn fill_session_tags(connection: &rusqlite::Connection, rows: &mut [WikiRow]) -> Result<()> {
    let ids: Vec<&str> = rows
        .iter()
        .filter(|row| row.tool == TOOL)
        .map(|row| row.id.as_str())
        .collect();
    let found = tags::read(connection, &ids).context("read the indexed session metadata")?;
    for row in rows.iter_mut().filter(|row| row.tool == TOOL) {
        let Some(session) = found.get(&row.id) else {
            continue;
        };
        row.target = session.target.clone();
        row.profile = session.profile.clone();
        row.harness = session.harness.clone();
    }
    Ok(())
}

/// How far back a title or project match looks. Those columns have no index of
/// their own, so this is a scan of the most recent sessions rather than of the
/// whole corpus. Fetch only metadata here: the regular recent-session query
/// also loads a preview, summary and tags for every row, none of which title
/// matching needs.
const NAME_SCAN_LIMIT: usize = 2_000;

/// Indexed sessions whose title or project contains the query, ignoring case.
fn named_like(
    connection: &rusqlite::Connection,
    query: &str,
) -> Result<Vec<sessionwiki::index::SessionRow>> {
    let needle = query.to_lowercase();
    let sql = format!(
        "SELECT session_id, tool, path, project, title, started, msg_count, kind,
                archived_at IS NOT NULL
         FROM files ORDER BY started DESC LIMIT {NAME_SCAN_LIMIT}"
    );
    let mut statement = connection
        .prepare(&sql)
        .context("prepare recent SessionWiki metadata scan")?;
    let rows = statement
        .query_map([], |row| {
            Ok(sessionwiki::index::SessionRow {
                last_active: None,
                session_id: row.get(0)?,
                tool: row.get(1)?,
                path: row.get(2)?,
                project: row.get(3)?,
                title: row.get(4)?,
                started: row.get(5)?,
                msg_count: row.get(6)?,
                kind: row.get(7)?,
                preview: None,
                summary: None,
                tags: None,
                archived: row.get(8)?,
                account: None,
            })
        })
        .context("list recent SessionWiki metadata")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("read recent SessionWiki metadata")?;
    Ok(rows
        .into_iter()
        .filter(|row| {
            row.title.to_lowercase().contains(&needle)
                || row.project.to_lowercase().contains(&needle)
        })
        .collect())
}

/// The briefing for one indexed session, or `None` when the id names none.
pub fn brief(id: &str, max_chars: usize) -> Result<Option<String>> {
    if !index_is_writable() {
        return Ok(None);
    }
    let connection = open_readonly()?;
    let ownership = top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    let Some(row) = visible_row_by_id(&connection, id, &ownership, &cache)? else {
        return Ok(None);
    };
    let session = sessionwiki::index::session_from_index(&connection, &row)
        .context("read an indexed session")?;
    Ok(Some(sessionwiki::commands::brief_markdown(
        &session, max_chars, true,
    )))
}

/// The passages of one indexed session that match `query`, or `None` when the
/// id names no indexed session.
///
/// Every matching message is returned with `context_messages` neighbours on
/// each side; overlapping groups are merged and each group's first block says
/// how many messages were skipped before it. Each block's text is capped at
/// `per_message_chars` characters, keeping the window around its first match.
pub fn transcript_hits(
    id: &str,
    query: &str,
    context_messages: usize,
    per_message_chars: usize,
) -> Result<Option<WikiHitTranscript>> {
    if !index_is_writable() {
        return Ok(None);
    }
    let connection = open_readonly()?;
    let ownership = top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    let Some(row) = visible_row_by_id(&connection, id, &ownership, &cache)? else {
        return Ok(None);
    };
    let session = sessionwiki::index::session_from_index(&connection, &row)
        .context("read an indexed session")?;
    Ok(Some(hit_transcript(
        &session,
        query,
        context_messages,
        per_message_chars,
    )))
}

/// The matching passages of one loaded session, converted to the daemon
/// response shape. Pure, so the conversion can be tested without an index.
///
/// Tool output never anchors a passage: it is machine chatter the reader did
/// not write, a hit buried in it would open the preview on a wall of command
/// output, and the preview collapses tool runs anyway. Tool messages still
/// appear as context around a real match.
fn hit_transcript(
    session: &Session,
    query: &str,
    context_messages: usize,
    per_message_chars: usize,
) -> WikiHitTranscript {
    let found = transcript_grep::grep_session(
        session,
        query,
        &transcript_grep::GrepOpts {
            context_messages,
            chars: per_message_chars,
            max_matches: None,
            anchor_roles: vec![Role::User, Role::Assistant],
        },
    );
    WikiHitTranscript {
        blocks: found
            .hits
            .into_iter()
            .map(|hit| WikiHitBlock {
                role: role_name(hit.role).to_owned(),
                text: hit.text,
                hits: hit.matches,
                omitted_before: hit.omitted_before,
                truncated: hit.truncated,
            })
            .collect(),
        omitted_after: found.omitted_after,
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// What a restore needs from the index: the transcript as a snapshot the
/// compaction pipeline accepts, plus the title and project of the session it
/// came from.
pub struct ArchivedSession {
    pub title: String,
    /// The project directory the session ran in, when the row names one that
    /// still exists.
    pub project_directory: Option<PathBuf>,
    pub snapshot: mj_core::archive::CanonicalSessionSnapshot,
}

/// Load one indexed session for restore, or `None` when the id names none.
pub fn archived_session(id: &str) -> Result<Option<ArchivedSession>> {
    if !index_is_writable() {
        return Ok(None);
    }
    let connection = open_readonly()?;
    let ownership = top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    let Some(row) = visible_row_by_id(&connection, id, &ownership, &cache)? else {
        return Ok(None);
    };
    let session = sessionwiki::index::session_from_index(&connection, &row)
        .context("read an indexed session")?;
    let snapshot = snapshot_of(&session)?;
    Ok(Some(ArchivedSession {
        title: session.title.clone(),
        project_directory: project_directory_of(&session.project),
        snapshot,
    }))
}

// ---------------------------------------------------------------------------
// The archive job
// ---------------------------------------------------------------------------

/// The stopped sessions that `archive_after_days = older_than_days` has caught,
/// children before their parents.
///
/// A session qualifies when its record is `Stopped`, its last update is at
/// least that many days old, and every sub-agent child it still has is being
/// archived in the same pass. The child rule is what keeps the pass from
/// destroying a session it did not choose: archiving a parent tears its
/// children down with it, so a child that is still running, or stopped but not
/// yet old enough, holds its parent back until the next pass.
///
/// Pure over controller state, so the rule can be tested without a daemon.
pub fn sessions_ready_to_archive(
    sessions: &mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    subagents: &mj_core::snapshot_map::SnapshotMap<String, mj_core::subagent::SubagentRecord>,
    now: DateTime<Utc>,
    older_than_days: u32,
) -> Vec<String> {
    let state = mj_core::state::State {
        sessions: sessions.clone(),
        subagents: subagents.clone(),
        ..mj_core::state::State::default()
    };
    sessions_ready_to_archive_from_state(&state, now, older_than_days)
}

pub(crate) fn sessions_ready_to_archive_from_state(
    state: &mj_core::state::State,
    now: DateTime<Utc>,
    older_than_days: u32,
) -> Vec<String> {
    let sessions = &state.sessions;
    let subagents = &state.subagents;
    let cutoff = now - chrono::Duration::days(i64::from(older_than_days));
    let aged = |session_id: &String| {
        sessions.get(session_id).is_some_and(|record| {
            let checkout = state.checkout(session_id).ok();
            let is_managed_worktree = checkout.as_ref().is_some_and(|checkout| {
                matches!(
                    checkout.effective(),
                    mj_core::state::Checkout::ManagedWorktree { worktree, .. }
                        if worktree.kind == mj_core::state::ManagedCheckoutKind::Worktree
                )
            });
            // Preserve the historical child rule: its copied raw path counted
            // as a raw checkout, even when the parent owns a managed clone.
            let has_raw_path = checkout.as_ref().is_some_and(|checkout| {
                checkout.project_directory().is_some()
                    && matches!(
                        checkout,
                        mj_core::state::Checkout::Attached { .. }
                            | mj_core::state::Checkout::Borrowed { .. }
                    )
            });
            record.state == mj_core::state::SessionState::Stopped
                && parse_time(&record.updated_at).is_some_and(|updated| updated <= cutoff)
                && (is_managed_worktree
                    || has_raw_path
                    || record
                        .checkpoint
                        .as_ref()
                        .zip(record.publication.as_ref())
                        .is_some_and(|(checkpoint, publication)| {
                            publication.checkpoint_sha256 == checkpoint.sha256
                                && publication.state == mj_core::state::PublicationState::Published
                                && !publication.dirty
                                && !publication.stashed
                        }))
        })
    };
    let selected: BTreeSet<String> = sessions
        .keys()
        .filter(|session_id| aged(session_id))
        .filter(|session_id| {
            subagents
                .values()
                .filter(|child| &&child.parent_session_id == session_id)
                // A child whose record is already gone holds nothing open.
                .filter(|child| sessions.contains_key(&child.child_session_id))
                .all(|child| aged(&child.child_session_id))
        })
        .cloned()
        .collect();
    let mut ordered: Vec<String> = selected.iter().cloned().collect();
    ordered.sort_by_key(|session_id| std::cmp::Reverse(ancestor_depth(session_id, subagents)));
    ordered
}

/// How many sub-agent parents a session has above it. Deeper sessions are
/// archived first so a parent never tears down a child the pass still has to
/// visit.
fn ancestor_depth(
    session_id: &str,
    subagents: &mj_core::snapshot_map::SnapshotMap<String, mj_core::subagent::SubagentRecord>,
) -> usize {
    let mut depth = 0;
    let mut current = session_id;
    // Bounded by the map: a cycle cannot outlive one pass over every entry.
    while let Some(parent) = subagents
        .get(current)
        .map(|child| child.parent_session_id.as_str())
    {
        depth += 1;
        if depth > subagents.len() {
            break;
        }
        current = parent;
    }
    depth
}

/// How much disk Mjolnir's own copies of sessions use, and how much an
/// `archive_after_days` value would free. "Mjolnir's own copy" is the
/// checkpoint archive plus the session's image attachments; the conversation
/// itself lives in the SessionWiki index and is not counted, because archiving
/// keeps it. The type lives in `mj-core` so the terminal UI can name it too.
pub use mj_core::state::ArchiveSpacePreview;

/// The space every session uses now and, when `older_than_days` is set, the
/// space archiving after that many days would reclaim.
///
/// The reclaim figure uses the archive job's own selection rule but not its
/// "is it indexed yet" gate: that gate depends on how far the hourly index
/// sync has got, so applying it would make the estimate swing between zero and
/// the true value while the first index builds. This answers what the policy
/// would reclaim, not what the next tick happens to reclaim.
///
/// Walks the filesystem, so callers on the async runtime must run it in a
/// blocking task.
pub fn archive_space_preview(older_than_days: Option<u32>) -> Result<ArchiveSpacePreview> {
    let controller =
        Controller::load().context("load the session records to size their storage")?;
    Ok(archive_space_over_state(
        &mj_core::config::sessions_dir(),
        &controller.state,
        Utc::now(),
        older_than_days,
    ))
}

/// Size archives using a State view so borrowed checkout owners remain visible.
fn archive_space_over_state(
    sessions_root: &Path,
    state: &mj_core::state::State,
    now: DateTime<Utc>,
    older_than_days: Option<u32>,
) -> ArchiveSpacePreview {
    let sessions = &state.sessions;
    let mut preview = ArchiveSpacePreview {
        sessions: sessions.len(),
        bytes: sessions
            .iter()
            .map(|(session_id, record)| session_bytes(sessions_root, session_id, record))
            .sum(),
        reclaimable_sessions: 0,
        reclaimable_bytes: 0,
    };
    if let Some(days) = older_than_days {
        let aged = sessions_ready_to_archive_from_state(state, now, days);
        preview.reclaimable_sessions = aged.len();
        preview.reclaimable_bytes = aged
            .iter()
            .filter_map(|session_id| {
                sessions
                    .get(session_id)
                    .map(|record| session_bytes(sessions_root, session_id, record))
            })
            .sum();
    }
    preview
}

/// The sizing itself, over given records and a sessions directory.
#[cfg(test)]
fn archive_space_over(
    sessions_root: &Path,
    sessions: &mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    subagents: &mj_core::snapshot_map::SnapshotMap<String, mj_core::subagent::SubagentRecord>,
    now: DateTime<Utc>,
    older_than_days: Option<u32>,
) -> ArchiveSpacePreview {
    let state = mj_core::state::State {
        sessions: sessions.clone(),
        subagents: subagents.clone(),
        ..mj_core::state::State::default()
    };
    archive_space_over_state(sessions_root, &state, now, older_than_days)
}

/// What archiving one session would free: its checkpoint archive and its
/// attachments. Anything already missing counts as zero.
fn session_bytes(sessions_root: &Path, session_id: &str, record: &SessionRecord) -> u64 {
    let checkpoint = record
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| std::fs::metadata(&checkpoint.archive_path).ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let attachments = sessions_root
        .join(session_id)
        .join(mj_core::attachment::ATTACHMENT_DIR);
    let attachments = crate::import::claude::directory_size(&attachments).unwrap_or(0);
    checkpoint.saturating_add(attachments)
}

/// Which of `session_ids` the index holds under this instance's own key, with
/// at least one message and not already archived.
///
/// This is the gate the archive job will not cross: Mjolnir only deletes its
/// own copy of a conversation SessionWiki has actually stored. Runs SQLite
/// work, so callers on the async runtime wrap it in `spawn_blocking`.
pub fn indexed_with_messages(session_ids: &[String]) -> Result<BTreeSet<String>> {
    if !index_is_writable() {
        // An index this daemon will not open holds nothing it may act on, and
        // the archive job deletes data, so it must find nothing here.
        return Ok(BTreeSet::new());
    }
    let connection = open_readonly()?;
    let sessions_dir = mj_core::config::sessions_dir();
    let mut indexed = BTreeSet::new();
    for session_id in session_ids {
        let key = format!("{}/{session_id}", sessions_dir.display());
        let rows = sessionwiki::index::resolve(&connection, session_id)
            .context("look up a stopped session in the SessionWiki index")?;
        if rows
            .iter()
            .any(|row| row.tool == TOOL && row.path == key && row.msg_count > 0 && !row.archived)
        {
            indexed.insert(session_id.clone());
        }
    }
    Ok(indexed)
}

fn open_readonly() -> Result<rusqlite::Connection> {
    sessionwiki::index::open_readonly().context("open the SessionWiki index")
}

/// The one row an id names exactly. `resolve` matches prefixes, which is right
/// for a person typing and wrong for a client passing an id back.
fn row_by_id(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<Option<sessionwiki::index::SessionRow>> {
    Ok(sessionwiki::index::resolve(connection, id)
        .context("look up an indexed session")?
        .into_iter()
        .find(|row| row.session_id == id))
}

/// Explicit IDs obey the same visibility rule as search results: a child
/// found by standalone SessionWiki sync is hidden as though it were absent.
fn visible_row_by_id(
    connection: &rusqlite::Connection,
    id: &str,
    ownership: &top_level::Snapshot,
    cache: &crate::import::NativeScanCache,
) -> Result<Option<sessionwiki::index::SessionRow>> {
    let Some(row) = row_by_id(connection, id)? else {
        return Ok(None);
    };
    if top_level::is_child(&row, ownership, cache)? {
        Ok(None)
    } else {
        Ok(Some(row))
    }
}

fn wiki_row(
    row: sessionwiki::index::SessionRow,
    snippet: Option<String>,
    live: &BTreeSet<String>,
) -> WikiRow {
    // Only this daemon's own sessions can be live here, and only under the key
    // shape the adapter writes: the checkpoint directory and the session id.
    let hel_session_id = (row.tool == TOOL)
        .then(|| row.path.rsplit('/').next().unwrap_or_default().to_owned())
        .filter(|session_id| live.contains(session_id));
    let native_id = sessionwiki::index::native_id_of(&row.path);
    WikiRow {
        id: row.session_id,
        tool: row.tool,
        project: row.project,
        title: row.title,
        started: row.started,
        msgs: row.msg_count,
        preview: row.preview,
        archived: row.archived,
        native_id,
        snippet,
        hel_session_id,
        // Filled in by `fill_session_tags` from the index's own tags; the row
        // itself does not carry them.
        target: None,
        profile: None,
        harness: None,
    }
}

/// The project a restored session should open.
///
/// A Mjolnir session runs in a managed worktree under the repository it was
/// started from, and that worktree is gone once the session is archived. The
/// repository above it is what the user still has, so a worktree path is
/// reduced to it. Any other path is used as it stands, and a path that no
/// longer exists is left for the caller to replace.
fn project_directory_of(project: &str) -> Option<PathBuf> {
    if project.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(project);
    let repository = path
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".mj"))
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or(path);
    repository.is_dir().then_some(repository)
}

/// Rebuild an indexed transcript as a canonical snapshot.
///
/// The snapshot is only ever read by the compaction pipeline, which wants
/// turns: a user message opens a turn and assistant and tool items attach to
/// it. Messages before the first user message therefore have nowhere to go and
/// are dropped, and a session with no user message at all cannot be restored.
fn snapshot_of(
    session: &sessionwiki::model::Session,
) -> Result<mj_core::archive::CanonicalSessionSnapshot> {
    use mj_core::archive::{
        CanonicalExecutionState, CanonicalSessionSnapshot, CanonicalSessionState,
        CanonicalTranscriptBody, CanonicalTranscriptItem,
    };

    let started_ms = session
        .started
        .map(|time| time.timestamp_millis())
        .unwrap_or_default();
    let mut transcript: Vec<CanonicalTranscriptItem> = Vec::new();
    for message in &session.messages {
        let text = message.text.trim();
        if text.is_empty() {
            continue;
        }
        // Compaction attaches assistant and tool items to the open turn, so an
        // item before the first user message would be dropped anyway.
        if transcript.is_empty() && message.role != Role::User {
            continue;
        }
        let position = transcript.len() as u64 + 1;
        let body = match message.role {
            Role::User => CanonicalTranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": text})],
            },
            Role::Assistant => CanonicalTranscriptBody::Agent {
                chunks: vec![serde_json::json!({
                    "content": {"type": "text", "text": text}
                })],
                streaming: false,
            },
            // New indexes retain the shared projection; legacy rows contain only a title.
            Role::Tool => {
                let (call, terminal_outputs) = mj_transcript::summary::indexed_tool_call(
                    text,
                    &format!("wiki-tool-{position}"),
                );
                CanonicalTranscriptBody::Tool {
                    call,
                    terminal_outputs,
                    terminal_refs: Vec::new(),
                    presentation: None,
                }
            }
        };
        let created_at_ms = message
            .ts
            .map(|time| time.timestamp_millis())
            .unwrap_or(started_ms);
        transcript.push(CanonicalTranscriptItem {
            stable_id: format!("wiki-{position}"),
            position,
            // The validator wants an ordinal on agent messages and on nothing
            // else; one event per item makes the item's own position right.
            latest_content_event_ordinal: matches!(body, CanonicalTranscriptBody::Agent { .. })
                .then_some(position),
            created_at_ms,
            last_changed_at_ms: created_at_ms,
            body,
        });
    }
    anyhow::ensure!(
        !transcript.is_empty(),
        "the archived session has no prompt to restore from"
    );

    let event_frontier = transcript.len() as u64;
    let last_activity_at_ms = transcript.last().map(|item| item.last_changed_at_ms);
    Ok(CanonicalSessionSnapshot {
        command_ledger: None,
        assessment_state: None,
        event_frontier,
        // Not a relay frontier, so there is no recorded digest to carry. It has
        // to be a well-formed non-genesis digest, and deriving it from the
        // session makes two restores of one session agree.
        event_frontier_digest: {
            use sha2::Digest;
            mj_core::hex::lower_hex(sha2::Sha256::digest(
                format!("sessionwiki:{}", session.id).as_bytes(),
            ))
        },
        session: CanonicalSessionState {
            execution: CanonicalExecutionState::Idle,
            last_activity_at_ms,
            session_title: Some(session.title.clone()).filter(|title| !title.trim().is_empty()),
            configuration: Default::default(),
        },
        transcript,
        queued_prompts: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Continuing an indexed session
// ---------------------------------------------------------------------------

/// What continuing one indexed session means.
///
/// An agent that found a session with SessionWiki should not have to know
/// whose session it was, so the branch lives here and `mj resume --wiki` takes
/// it on the agent's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikiContinuation {
    /// A Mjolnir session this daemon still has a record of: resume it.
    Resume { session_id: String },
    /// A Mjolnir session whose record the archive job destroyed: start a new
    /// session seeded with a compacted hand-off.
    Restore { wiki_id: String },
    /// Another tool's session: import it, then resume what the import made.
    Import {
        harness: HarnessKind,
        native_session_id: String,
    },
}

/// How to continue the indexed session a row describes.
///
/// Pure over the row so the branch can be tested without an index:
/// `path` is the row's stored path and `has_record` says whether controller
/// state still holds a session with this id.
pub fn wiki_continuation(
    wiki_id: &str,
    tool: &str,
    path: &Path,
    has_record: bool,
) -> Result<WikiContinuation> {
    if tool == TOOL {
        // `MjolnirAdapter::parse_key` names the session by its own Mjolnir id,
        // so a Mjolnir row's SessionWiki id is the session id.
        return Ok(match has_record {
            true => WikiContinuation::Resume {
                session_id: wiki_id.to_owned(),
            },
            false => WikiContinuation::Restore {
                wiki_id: wiki_id.to_owned(),
            },
        });
    }
    let harness = harness_adapters::harness_for_tool(tool)
        .with_context(|| format!("Mjolnir cannot continue a {tool} session"))?;
    let native_session_id = crate::import::native_session_id_from_path(harness, path)
        .with_context(|| {
            format!(
                "no {tool} session id in the indexed path {}",
                path.display()
            )
        })?;
    Ok(WikiContinuation::Import {
        harness,
        native_session_id,
    })
}

/// What one indexed session is, as far as continuing it is concerned.
///
/// Read through [`wiki_session`]; the daemon serves it for `mj resume --wiki`
/// and for `mj sessions --session` when the id names no Mjolnir session.
pub fn wiki_session(
    wiki_id: &str,
    known_sessions: &BTreeSet<String>,
) -> Result<Option<WikiSessionInfo>> {
    if !index_is_writable() {
        return Ok(None);
    }
    let connection = open_readonly()?;
    let ownership = top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    wiki_session_from(&connection, wiki_id, known_sessions, &ownership, &cache)
}

fn wiki_session_from(
    connection: &rusqlite::Connection,
    wiki_id: &str,
    known_sessions: &BTreeSet<String>,
    ownership: &top_level::Snapshot,
    cache: &crate::import::NativeScanCache,
) -> Result<Option<WikiSessionInfo>> {
    let Some(row) = visible_row_by_id(connection, wiki_id, ownership, cache)? else {
        return Ok(None);
    };
    let is_mjolnir = row.tool == TOOL;
    let mjolnir_session_id = is_mjolnir.then(|| row.session_id.clone());
    let has_record = mjolnir_session_id
        .as_deref()
        .is_some_and(|session_id| known_sessions.contains(session_id));
    let status = match (is_mjolnir, has_record) {
        (false, _) => WikiSessionStatus::Native,
        (true, true) => WikiSessionStatus::Mine,
        (true, false) => WikiSessionStatus::Archived,
    };
    let tags = match is_mjolnir {
        true => tags::read(connection, &[row.session_id.as_str()])
            .context("read the indexed session metadata")?
            .remove(&row.session_id)
            .unwrap_or_default(),
        false => tags::MjTags::default(),
    };
    // Only an archived row is continued by restoring its transcript, and a
    // restore needs a prompt to open the first turn.
    let nothing_to_restore = status == WikiSessionStatus::Archived
        && !has_prompt(
            &sessionwiki::index::session_from_index(connection, &row)
                .context("read an indexed session")?,
        );
    let harness = tags
        .harness
        .as_deref()
        .and_then(|id| id.parse::<HarnessKind>().ok())
        .or_else(|| {
            (!is_mjolnir)
                .then(|| harness_adapters::harness_for_tool(&row.tool))
                .flatten()
        });
    Ok(Some(WikiSessionInfo {
        wiki_id: row.session_id,
        tool: row.tool,
        path: PathBuf::from(row.path),
        status,
        mjolnir_session_id,
        profile_id: tags.profile,
        target_template_id: tags.target,
        harness,
        title: row.title,
        project: row.project,
        nothing_to_restore,
    }))
}

/// Whether an indexed transcript holds a prompt, which is what
/// [`snapshot_of`] needs to open a turn.
fn has_prompt(session: &sessionwiki::model::Session) -> bool {
    session
        .messages
        .iter()
        .any(|message| message.role == Role::User && !message.text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    /// The dispatch `mj resume --wiki` takes, over rows built by hand: the
    /// branch has to be right without an index behind it.
    mod continuation {
        use super::super::{WikiContinuation, wiki_continuation};
        use mj_core::config::HarnessKind;
        use std::path::Path;

        #[test]
        fn a_claude_code_row_is_imported_with_the_uuid_from_its_path() {
            let path = Path::new(
                "/home/user/.claude/projects/-home-user-app/7f3a1c20-0b11-4a55-9e0d-2c8a5d6f1b44.jsonl",
            );
            assert_eq!(
                wiki_continuation("abc123", "claude-code", path, false).unwrap(),
                WikiContinuation::Import {
                    harness: HarnessKind::Claude,
                    native_session_id: "7f3a1c20-0b11-4a55-9e0d-2c8a5d6f1b44".to_owned(),
                }
            );
        }

        /// A Codex rollout's file name is a timestamp and the thread UUID, so
        /// the stem alone is not the id `mj import codex --session` takes.
        #[test]
        fn a_codex_row_is_imported_with_the_uuid_from_its_rollout_name() {
            let path = Path::new(
                "/home/user/.codex/sessions/2026/09/18/rollout-2026-09-18T09-15-00-7f3a1c20-0b11-4a55-9e0d-2c8a5d6f1b44.jsonl",
            );
            assert_eq!(
                wiki_continuation("abc123", "codex", path, false).unwrap(),
                WikiContinuation::Import {
                    harness: HarnessKind::Codex,
                    native_session_id: "7f3a1c20-0b11-4a55-9e0d-2c8a5d6f1b44".to_owned(),
                }
            );
        }
    }

    use mj_checkpoint::archive::{
        ArchiveInput, BundleManifest, CanonicalExecutionState, CanonicalSessionSnapshot,
        CanonicalSessionState, CanonicalTranscriptBody, CanonicalTranscriptItem, SessionManifest,
        TargetManifest, write_archive_atomic,
    };

    use super::*;

    fn wiki_session_for_test(
        wiki_id: &str,
        known_sessions: &BTreeSet<String>,
    ) -> Result<Option<WikiSessionInfo>> {
        let connection = open_readonly()?;
        wiki_session_from(
            &connection,
            wiki_id,
            known_sessions,
            &top_level::Snapshot::default(),
            &crate::import::NativeScanCache::new(),
        )
    }

    fn query_rows_for_test(
        query: &str,
        limit: usize,
        include_tool_matches: bool,
    ) -> Result<Vec<WikiRow>> {
        query_rows_with_snapshot_for_test(
            query,
            limit,
            include_tool_matches,
            &top_level::Snapshot::default(),
        )
    }

    fn query_rows_with_snapshot_for_test(
        query: &str,
        limit: usize,
        include_tool_matches: bool,
        ownership: &top_level::Snapshot,
    ) -> Result<Vec<WikiRow>> {
        let connection = open_readonly()?;
        query_rows_from(
            &connection,
            query,
            limit,
            &BTreeSet::new(),
            include_tool_matches,
            ownership,
            &crate::import::NativeScanCache::new(),
        )
    }

    fn item(position: u64, body: CanonicalTranscriptBody) -> CanonicalTranscriptItem {
        // Only an agent message carries a content ordinal; the snapshot
        // validator rejects one on any other item and demands one here.
        let streamed = matches!(body, CanonicalTranscriptBody::Agent { .. });
        CanonicalTranscriptItem {
            stable_id: format!("item-{position}"),
            position,
            latest_content_event_ordinal: streamed.then_some(position),
            created_at_ms: 1_700_000_000_000 + i64::try_from(position).unwrap(),
            last_changed_at_ms: 1_700_000_000_000 + i64::try_from(position).unwrap(),
            body,
        }
    }

    /// A managed checkpoint with one prompt, one reply, one tool call, and one
    /// thought, which is every transcript shape the adapter decides about.
    fn write_archive(directory: &Path, session_id: &str, frontier: u64) {
        let path = directory.join(format!(
            "{session_id}-{frontier}-archive-{}.hel.zip",
            "0".repeat(32)
        ));
        write_archive_atomic(
            &path,
            &ArchiveInput {
                session: SessionManifest {
                    id: session_id.into(),
                    title: "indexed session".into(),
                    harness_kind: mj_core::config::HarnessKind::Codex,
                    profile_id: "codex".into(),
                    native_session_id: "native-session".into(),
                    created_at: "2026-09-01T00:00:00Z".into(),
                    checkpointed_at: "2026-09-01T01:00:00Z".into(),
                    hel_version: "test".into(),
                    relay_version: "test".into(),
                    adapter_version: "test".into(),
                },
                target: TargetManifest {
                    template_id: "local".into(),
                    target_kind: "local-bare".into(),
                    details: BTreeMap::new(),
                },
                bundle: BundleManifest {
                    id: "project".into(),
                    primary_repository: "project".into(),
                },
                canonical_session: CanonicalSessionSnapshot {
                    command_ledger: None,
                    assessment_state: None,
                    event_frontier: 4,
                    event_frontier_digest: "a".repeat(64),
                    session: CanonicalSessionState {
                        execution: CanonicalExecutionState::Idle,
                        last_activity_at_ms: Some(1_700_000_000_004),
                        session_title: Some("snapshot title".into()),
                        configuration: Default::default(),
                    },
                    transcript: vec![
                        item(
                            1,
                            CanonicalTranscriptBody::User {
                                content: vec![serde_json::json!({
                                    "type": "text",
                                    "text": "index this session"
                                })],
                            },
                        ),
                        item(
                            2,
                            CanonicalTranscriptBody::Thought {
                                chunks: vec![serde_json::json!({
                                    "content": {"type": "text", "text": "pondering"}
                                })],
                                streaming: false,
                            },
                        ),
                        item(
                            3,
                            CanonicalTranscriptBody::Tool {
                                call: serde_json::json!({
                                    "toolCallId": "call-1",
                                    "title": "Edit config.toml",
                                    "kind": "edit",
                                    "status": "completed",
                                    "locations": [{"path": "/old/container/config.toml"}]
                                }),
                                terminal_outputs: Vec::new(),
                                terminal_refs: Vec::new(),
                                presentation: None,
                            },
                        ),
                        item(
                            4,
                            CanonicalTranscriptBody::Agent {
                                chunks: vec![serde_json::json!({
                                    "content": {"type": "text", "text": "done"}
                                })],
                                streaming: false,
                            },
                        ),
                    ],
                    queued_prompts: Vec::new(),
                },
                native_artifacts: Vec::new(),
                repositories: Vec::new(),
            },
        )
        .unwrap();
    }

    fn adapter(directory: &Path, session_id: &str) -> MjolnirAdapter {
        adapter_with_live(directory, session_id, BTreeMap::new())
    }

    fn adapter_with_live(
        directory: &Path,
        session_id: &str,
        live: BTreeMap<String, i64>,
    ) -> MjolnirAdapter {
        let record = SessionRecord {
            project: None,
            id: session_id.into(),
            ..record_template()
        };
        MjolnirAdapter {
            sessions_dir: directory.to_path_buf(),
            sessions: std::sync::Mutex::new(Sessions {
                records: [(session_id.to_owned(), record)].into_iter().collect(),
                ownership: top_level::Snapshot::default(),
                project_directories: [(
                    session_id.to_owned(),
                    Ok(Some(PathBuf::from("/home/dev/project"))),
                )]
                .into_iter()
                .collect(),
                live,
            }),
            reload: false,
        }
    }

    fn adapter_for_state(directory: &Path, state: &State, config: &Config) -> MjolnirAdapter {
        let sessions = Sessions {
            records: state.sessions.clone(),
            ownership: top_level::Snapshot::from_state(state),
            project_directories: project_directories_of(state, Some(config)),
            live: BTreeMap::new(),
        };
        MjolnirAdapter {
            sessions_dir: directory.to_path_buf(),
            sessions: std::sync::Mutex::new(sessions),
            reload: false,
        }
    }

    fn sync_adapter(connection: &mut rusqlite::Connection, source: &Arc<MjolnirAdapter>) {
        let adapter: Box<dyn Adapter> = Box::new(SharedMjolnirAdapter(Arc::clone(source)));
        sessionwiki::index::sync_with(connection, &[adapter], None).unwrap();
    }

    fn indexed_project(connection: &rusqlite::Connection, session_id: &str) -> String {
        connection
            .query_row(
                "SELECT project FROM files WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn indexed_mtime(connection: &rusqlite::Connection, session_id: &str) -> i64 {
        connection
            .query_row(
                "SELECT mtime FROM files WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn bundle_config() -> Config {
        let mut config = Config::default();
        config.bundles.insert(
            "project".into(),
            mj_core::config::ProjectBundle {
                primary_repo: "bifrost".into(),
                repositories: vec![mj_core::config::ProjectRepository {
                    id: "bifrost".into(),
                    github: Some("BrokkAi/bifrost".into()),
                    local: None,
                    destination: PathBuf::from("bifrost"),
                    git_ref: None,
                }],
            },
        );
        config
    }

    fn bundle_record(session_id: &str) -> SessionRecord {
        SessionRecord {
            id: session_id.into(),
            project_directory: None,
            container_workspace: Some(PathBuf::from(format!("/workspace/{session_id}"))),
            target: Some(mj_core::state::TargetLocator::LocalPodman {
                container_id: "test-container".into(),
                workspace_storage: Default::default(),
                borrowed_from: None,
            }),
            ..record_template()
        }
    }

    fn state_with_record(session_id: &str, record: SessionRecord) -> State {
        let mut state = State::default();
        state.sessions.insert(session_id.to_owned(), record);
        state
    }

    fn record_template() -> SessionRecord {
        SessionRecord {
            project: None,
            target_runtime: None,
            launch_base: None,
            launch_branch: None,
            checkout: None,
            publication: None,
            build_cache: None,
            container_workspace: None,
            subagents: None,
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: "0123456789abcdef0123456789abcdef".into(),
            title: "indexed session".into(),
            harness_kind: mj_core::config::HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: Some(PathBuf::from("/home/dev/project")),
            managed_worktree: None,
            review: None,
            target_template_id: "local-bare".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: mj_core::state::SessionState::Stopped,
            target: None,
            native_session_id: Some("native-session".into()),
            acp_session_title: Some("the harness title".into()),
            session_title_override: None,
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: "2026-09-01T01:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    #[test]
    fn children_have_no_store_keys_metadata_or_pre_destroy_work() {
        let _held = tags::testing::lock();
        let (_index, _connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let parent = "0123456789abcdef0123456789abcdef";
        let child = "fedcba9876543210fedcba9876543210";
        write_archive(directory.path(), parent, 1);
        write_archive(directory.path(), child, 1);
        let source = adapter_with_live(
            directory.path(),
            parent,
            BTreeMap::from([
                (parent.to_owned(), 1_900_000_000),
                (child.to_owned(), 1_900_000_001),
            ]),
        );
        {
            let mut sessions = source.sessions.lock().unwrap();
            sessions.ownership = top_level::Snapshot::for_test(BTreeSet::from([child.to_owned()]));
            sessions.records.insert(
                child.to_owned(),
                SessionRecord {
                    id: child.into(),
                    ..record_template()
                },
            );
        }
        let store = source.store().unwrap();
        assert_eq!(store.keys.len(), 1);
        assert_eq!(store.keys[0].0, source.key_for(parent));
        assert_eq!(store.files.len(), 1);
        assert_eq!(
            source.indexed_tags().keys().cloned().collect::<Vec<_>>(),
            [parent]
        );
        assert_eq!(
            unindexed(&source, &[parent.to_owned(), child.to_owned()]).unwrap(),
            [parent]
        );
        assert!(
            source
                .parse_key(&source.key_for(child))
                .unwrap_err()
                .to_string()
                .contains("sub-agent")
        );
        // Stopped children are excluded as well, even when their checkpoint remains.
        source.sessions.lock().unwrap().live.clear();
        assert_eq!(source.store().unwrap().keys.len(), 1);
    }

    #[test]
    fn the_newest_checkpoint_of_each_session_is_one_indexed_key() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        write_archive(directory.path(), session_id, 1);
        write_archive(directory.path(), session_id, 7);
        let adapter = adapter(directory.path(), session_id);

        let store = adapter.store().expect("the adapter is a shared store");
        let key = format!("{}/{session_id}", directory.path().display());
        assert_eq!(
            store
                .keys
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec![key.as_str()]
        );
        assert!(!store.had_error);
        assert_eq!(store.files.len(), 1);
        assert!(
            store.files[0]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("-7-archive-"),
            "the newest checkpoint is the one indexed: {:?}",
            store.files[0]
        );
        assert_eq!(
            adapter.reconcile_scope(),
            Some(format!("{}/", directory.path().display()))
        );

        let session = adapter.parse_key(&key).unwrap();
        assert_eq!(session.id, session_id);
        assert_eq!(session.tool, "mjolnir");
        assert_eq!(session.path, PathBuf::from(&key));
        assert_eq!(session.project, "/home/dev/project");
        assert_eq!(session.title, "the harness title");
        assert!(!session.subagent);
        assert_eq!(
            session.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Tool, Role::Assistant]
        );
        assert_eq!(session.messages[0].text, "index this session");
        let tool: serde_json::Value = serde_json::from_str(&session.messages[1].text).unwrap();
        assert_eq!(tool["name"], "Edit");
        assert_eq!(tool["call"]["title"], "Edit config.toml");
        assert_eq!(session.messages[2].text, "done");
        assert_eq!(session.touched, vec!["/old/container/config.toml"]);
    }

    #[test]
    fn a_bundle_session_is_indexed_and_searchable_by_its_primary_repository() {
        let _held = tags::testing::lock();
        let (_index_dir, mut connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        write_archive(directory.path(), session_id, 1);
        let state = state_with_record(session_id, bundle_record(session_id));
        let source = Arc::new(adapter_for_state(
            directory.path(),
            &state,
            &bundle_config(),
        ));

        sync_adapter(&mut connection, &source);

        assert_eq!(
            indexed_project(&connection, session_id),
            format!("/workspace/{session_id}/bifrost")
        );
        assert!(
            query_rows_for_test("bifrost", 10, false)
                .unwrap()
                .iter()
                .any(|row| row.id == session_id),
            "the indexed bundle session is found by its repository name"
        );
    }

    #[test]
    fn a_raw_local_session_keeps_its_directory_when_indexed() {
        let _held = tags::testing::lock();
        let (_index_dir, mut connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let session_id = "fedcba9876543210fedcba9876543210";
        let project_directory = PathBuf::from("/home/jonathan/Projects/bifrost");
        write_archive(directory.path(), session_id, 1);
        let record = SessionRecord {
            id: session_id.into(),
            project_directory: Some(project_directory.clone()),
            ..record_template()
        };
        let state = state_with_record(session_id, record);
        let source = Arc::new(adapter_for_state(
            directory.path(),
            &state,
            &Config::default(),
        ));

        sync_adapter(&mut connection, &source);

        assert_eq!(
            indexed_project(&connection, session_id),
            project_directory.display().to_string()
        );
    }

    #[test]
    fn a_row_from_the_previous_parse_format_is_reparsed_once() {
        let _held = tags::testing::lock();
        let (_index_dir, mut connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        write_archive(directory.path(), session_id, 1);
        let mut record = bundle_record(session_id);
        record.updated_at = "2099-01-01T00:00:00Z".into();
        let previous_token = parse_time(&record.updated_at)
            .unwrap()
            .timestamp()
            .saturating_mul(1024)
            .saturating_add(i64::from(mj_transcript::summary::SUMMARY_VERSION));
        let state = state_with_record(session_id, record);
        let source = Arc::new(adapter_for_state(
            directory.path(),
            &state,
            &bundle_config(),
        ));
        let key = source.key_for(session_id);
        tags::testing::index_row(&connection, session_id, TOOL);
        connection
            .execute(
                "UPDATE files SET path = ?1, project = '', mtime = ?2 WHERE session_id = ?3",
                rusqlite::params![key, previous_token, session_id],
            )
            .unwrap();

        sync_adapter(&mut connection, &source);

        let parsed_token = indexed_mtime(&connection, session_id);
        assert_eq!(
            indexed_project(&connection, session_id),
            format!("/workspace/{session_id}/bifrost")
        );
        assert_eq!(
            parsed_token,
            source
                .store()
                .unwrap()
                .keys
                .into_iter()
                .find(|(path, _)| path == &key)
                .unwrap()
                .1
        );

        // If the unchanged second sync calls parse_key again, it will fail.
        source
            .sessions
            .lock()
            .unwrap()
            .project_directories
            .insert(session_id.into(), Err("unexpected second parse".into()));
        sync_adapter(&mut connection, &source);

        assert_eq!(indexed_mtime(&connection, session_id), parsed_token);
        assert_eq!(
            indexed_project(&connection, session_id),
            format!("/workspace/{session_id}/bifrost")
        );
    }

    #[test]
    fn provenance_backfill_repairs_an_unchanged_checkpoint_without_rebuilding_the_index() {
        let _held = tags::testing::lock();
        let (_index_dir, mut connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        write_archive(directory.path(), "old-session", 4);
        let source = adapter(directory.path(), "old-session");
        let key = source.key_for("old-session");
        tags::testing::index_row(&connection, "old-session", "mjolnir");
        connection
            .execute(
                "UPDATE files SET path = ?1 WHERE session_id = 'old-session'",
                [&key],
            )
            .unwrap();
        provenance::backfill(&mut connection, &source).unwrap();
        assert_eq!(
            sessionwiki::index::files_for(&connection, "old-session").unwrap(),
            vec!["/old/container/config.toml"]
        );
        provenance::backfill(&mut connection, &source).unwrap();
        assert_eq!(
            sessionwiki::index::sessions_for_file(&connection, "config.toml", 20)
                .unwrap()
                .len(),
            1
        );
    }

    /// A running session is listed under the same key as a stopped one, with
    /// its own change token, so it is searchable before it is ever closed and
    /// reconciliation never archives it. When it stops, the key stays and the
    /// checkpoint becomes its source.
    #[test]
    fn a_running_session_is_listed_with_its_own_change_token() {
        let directory = tempfile::tempdir().unwrap();
        let running = "0123456789abcdef0123456789abcdef";
        let never_checkpointed = "fedcba9876543210fedcba9876543210";
        write_archive(directory.path(), running, 3);
        let live = adapter_with_live(
            directory.path(),
            running,
            BTreeMap::from([
                (running.to_owned(), 1_900_000_000),
                (never_checkpointed.to_owned(), 1_900_000_001),
            ]),
        );

        let store = live.store().expect("the adapter is a shared store");
        let key_of = |session_id: &str| format!("{}/{session_id}", directory.path().display());
        assert_eq!(
            store.keys,
            vec![
                (key_of(running), session_change_token(1_900_000_000)),
                (
                    key_of(never_checkpointed),
                    session_change_token(1_900_000_001)
                ),
            ],
            "a live session's own token replaces the checkpoint's"
        );

        // Once it stops it leaves the live set, and the checkpoint's own
        // modification time is the token again.
        let stopped = adapter(directory.path(), running);
        let keys = stopped.store().expect("a shared store").keys;
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, key_of(running));
        assert_ne!(keys[0].1, 1_900_000_000);
        assert_eq!(
            stopped.parse_key(&key_of(running)).unwrap().title,
            "the harness title",
            "a stopped session is parsed from its checkpoint"
        );
    }

    /// Renaming a session leaves its conversation untouched, so only the
    /// record's own last update can tell the index the title moved.
    #[test]
    fn a_rename_moves_a_session_change_token() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        write_archive(directory.path(), session_id, 1);
        let adapter = adapter(directory.path(), session_id);
        let before = adapter.store().expect("a shared store").keys[0].1;

        {
            let mut sessions = adapter.sessions.lock().unwrap();
            let record = sessions.records.get_mut(session_id).unwrap();
            record.session_title_override = Some("the new name".into());
            record.updated_at = "2099-01-01T00:00:00Z".into();
        }
        let after = adapter.store().expect("a shared store").keys[0].1;
        assert!(
            after > before,
            "a renamed session is re-indexed: {before} then {after}"
        );
        assert_eq!(
            adapter
                .parse_key(&format!("{}/{session_id}", directory.path().display()))
                .unwrap()
                .title,
            "the new name"
        );
    }

    fn indexed(messages: Vec<(Role, &str)>) -> sessionwiki::model::Session {
        Session {
            id: "0123456789abcdef0123456789abcdef".into(),
            tool: "mjolnir",
            path: PathBuf::from("/sessions/0123456789abcdef0123456789abcdef"),
            project: "/home/dev/project".into(),
            started: DateTime::from_timestamp_millis(1_700_000_000_000),
            ended: None,
            title: "the archived session".into(),
            subagent: false,
            messages: messages
                .into_iter()
                .map(|(role, text)| Message {
                    role,
                    text: text.to_owned(),
                    ts: None,
                })
                .collect(),
            touched: Vec::new(),
            edits: Vec::new(),
        }
    }

    #[test]
    fn golden_wiki_resume_preview_search() {
        use std::fmt::Write as _;

        fn response(out: &mut String, label: &str, transcript: &WikiHitTranscript) {
            writeln!(
                out,
                "=== {label} ({} preview blocks) ===",
                transcript.blocks.len()
            )
            .unwrap();
            writeln!(out, "{}", serde_json::to_string_pretty(transcript).unwrap()).unwrap();
        }

        let mut out = String::new();
        let case_insensitive = indexed(vec![
            (Role::User, "Make the Tests green"),
            (Role::Assistant, "the tests are green now"),
        ]);
        response(
            &mut out,
            "case-insensitive query with user and assistant hits",
            &hit_transcript(&case_insensitive, "TESTS", 0, 4_000),
        );

        let tool_only_and_assistant = indexed(vec![
            (Role::User, "make it build"),
            (Role::Tool, "cargo build --needle"),
            (Role::Assistant, "it builds"),
        ]);
        response(
            &mut out,
            "query found only in tool output",
            &hit_transcript(&tool_only_and_assistant, "needle", 1, 4_000),
        );
        response(
            &mut out,
            "tool context beside an assistant hit",
            &hit_transcript(&tool_only_and_assistant, "builds", 1, 4_000),
        );

        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "wiki-resume-preview-search",
            &out,
        );
    }

    /// Context messages come back around each hit, with the gap between two
    /// groups counted rather than silently closed.
    #[test]
    fn transcript_hits_keeps_context_and_marks_omissions() {
        let session = indexed(vec![
            (Role::User, "zero"),
            (Role::Assistant, "one needle one"),
            (Role::Tool, "two"),
            (Role::User, "three"),
            (Role::Assistant, "four"),
            (Role::Tool, "five"),
            (Role::User, "six needle six"),
            (Role::Assistant, "seven"),
            (Role::User, "eight"),
        ]);

        let found = hit_transcript(&session, "needle", 1, 4_000);

        let shown: Vec<(&str, &str, usize)> = found
            .blocks
            .iter()
            .map(|block| {
                (
                    block.role.as_str(),
                    block.text.as_str(),
                    block.omitted_before,
                )
            })
            .collect();
        assert_eq!(
            shown,
            vec![
                ("user", "zero", 0),
                ("assistant", "one needle one", 0),
                ("tool", "two", 0),
                ("tool", "five", 2),
                ("user", "six needle six", 0),
                ("assistant", "seven", 0),
            ]
        );
        assert_eq!(found.omitted_after, 1, "the last message is not shown");
        assert!(found.blocks[0].hits.is_empty(), "context has no hits");
    }

    /// A long message is cut down to the caller's budget around its first hit,
    /// not from the start, so the match is always in what comes back.
    #[test]
    fn transcript_hits_window_keeps_the_first_hit() {
        let filler = "x".repeat(4_000);
        let session = indexed(vec![(Role::User, &format!("{filler} needle {filler}"))]);

        let found = hit_transcript(&session, "needle", 0, 100);

        let block = &found.blocks[0];
        assert!(block.truncated);
        assert_eq!(block.text.chars().count(), 100);
        assert_eq!(block.hits.len(), 1, "the windowed text keeps its hit");
        let (start, end) = block.hits[0];
        assert_eq!(&block.text[start..end], "needle");
        assert!(
            start >= 20,
            "the window keeps lead-in before the hit, got {start}"
        );
    }

    /// Compaction attaches assistant and tool items to the open turn, so an
    /// index that starts mid-conversation must not produce a snapshot whose
    /// first item has no turn to join.
    #[test]
    fn messages_before_the_first_prompt_are_dropped() {
        let snapshot = snapshot_of(&indexed(vec![
            (Role::Assistant, "still working"),
            (Role::User, "carry on"),
        ]))
        .unwrap();
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].position, 1);
        snapshot.validate().unwrap();

        let error = snapshot_of(&indexed(vec![(Role::Assistant, "nobody asked")])).unwrap_err();
        assert!(
            error.to_string().contains("no prompt"),
            "a session with no prompt cannot be restored: {error}"
        );
        // `mj sessions --session` offers a restore by the same rule.
        assert!(!has_prompt(&indexed(vec![(
            Role::Assistant,
            "nobody asked"
        )])));
        assert!(has_prompt(&indexed(vec![(Role::User, "carry on")])));
    }

    fn record(
        session_id: &str,
        state: mj_core::state::SessionState,
        updated_at: &str,
    ) -> SessionRecord {
        SessionRecord {
            project: None,
            id: session_id.into(),
            state,
            updated_at: updated_at.into(),
            ..record_template()
        }
    }

    fn child(child_session_id: &str, parent_session_id: &str) -> mj_core::subagent::SubagentRecord {
        mj_core::subagent::SubagentRecord {
            child_session_id: child_session_id.into(),
            parent_session_id: parent_session_id.into(),
            task_name: "task".into(),
            profile_id: "codex".into(),
            model: None,
            effort: None,
            working_directory: PathBuf::new(),
            initial_prompt: "do the thing".into(),
            request_key: "key".into(),
            created_at: "2026-09-01T00:00:00Z".into(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: false,
        }
    }

    fn ready(
        sessions: Vec<SessionRecord>,
        children: Vec<mj_core::subagent::SubagentRecord>,
    ) -> Vec<String> {
        let now = parse_time("2026-09-10T00:00:00Z").unwrap();
        sessions_ready_to_archive(
            &sessions
                .into_iter()
                .map(|record| (record.id.clone(), record))
                .collect(),
            &children
                .into_iter()
                .map(|child| (child.child_session_id.clone(), child))
                .collect(),
            now,
            3,
        )
    }

    #[test]
    fn aged_clone_requires_clean_published_evidence_for_its_current_checkpoint() {
        let id = "0123456789abcdef0123456789abcdef";
        let root = PathBuf::from(format!("/srv/project/.mj/clones/{id}"));
        let mut session = record(
            id,
            mj_core::state::SessionState::Stopped,
            "2026-09-01T00:00:00Z",
        );
        session.project_directory = Some(root.clone());
        session.managed_worktree = Some(mj_core::state::ManagedWorktree {
            kind: mj_core::state::ManagedCheckoutKind::Clone,
            source_project_directory: "/srv/project".into(),
            source_repository: "/srv/project".into(),
            worktree_root: root,
            branch: "feature".into(),
            target: mj_core::state::ManagedWorktreeTarget::Local,
            base_commit: Some("1".repeat(40)),
        });
        session.checkpoint = Some(mj_core::state::CheckpointMetadata {
            archive_path: "sessions/checkpoint.hel.zip".into(),
            sha256: "a".repeat(64),
            created_at: "2026-09-01T00:00:00Z".into(),
            event_frontier: 0,
        });
        assert!(ready(vec![session.clone()], vec![]).is_empty());
        session.publication = Some(mj_core::state::PublicationAssessment {
            checkpoint_sha256: "a".repeat(64),
            state: mj_core::state::PublicationState::Published,
            dirty: false,
            stashed: false,
            saved_commits: vec!["2".repeat(40)],
            destinations: vec!["https://example.test/repository.git".into()],
            checked_at: "2026-09-01T01:00:00Z".into(),
            reason: Some("feature branch was pushed but not merged".into()),
        });
        assert_eq!(ready(vec![session.clone()], vec![]), vec![id]);
        session.publication.as_mut().unwrap().stashed = true;
        assert!(ready(vec![session.clone()], vec![]).is_empty());
        session.publication.as_mut().unwrap().stashed = false;
        session.publication.as_mut().unwrap().checkpoint_sha256 = "b".repeat(64);
        assert!(ready(vec![session], vec![]).is_empty());
    }

    /// A session whose checkpoint archive and attachments sit under `root`.
    fn sized_session(
        root: &Path,
        session_id: &str,
        updated_at: &str,
        checkpoint_bytes: usize,
        attachment_bytes: &[usize],
    ) -> SessionRecord {
        let archive_path = root.join(format!("{session_id}.hel.zip"));
        std::fs::write(&archive_path, vec![b'c'; checkpoint_bytes]).unwrap();
        if !attachment_bytes.is_empty() {
            let attachments = root
                .join(session_id)
                .join(mj_core::attachment::ATTACHMENT_DIR);
            std::fs::create_dir_all(&attachments).unwrap();
            for (index, size) in attachment_bytes.iter().enumerate() {
                std::fs::write(attachments.join(format!("{index}.png")), vec![b'a'; *size])
                    .unwrap();
            }
        }
        SessionRecord {
            project: None,
            checkpoint: Some(mj_core::state::CheckpointMetadata {
                archive_path,
                sha256: "0".repeat(64),
                created_at: updated_at.into(),
                event_frontier: 1,
            }),
            ..record(
                session_id,
                mj_core::state::SessionState::Stopped,
                updated_at,
            )
        }
    }

    #[test]
    fn the_space_preview_sizes_every_session_and_only_the_aged_ones_as_reclaimable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let sessions: mj_core::snapshot_map::SnapshotMap<String, SessionRecord> = [
            sized_session(root, "old-stopped", "2026-09-01T00:00:00Z", 1000, &[10, 20]),
            sized_session(root, "just-stopped", "2026-09-09T00:00:00Z", 500, &[]),
            // A record whose checkpoint file is already gone counts as zero
            // rather than failing the whole estimate.
            SessionRecord {
                project: None,
                checkpoint: Some(mj_core::state::CheckpointMetadata {
                    archive_path: root.join("missing.hel.zip"),
                    sha256: "0".repeat(64),
                    created_at: "2026-09-01T00:00:00Z".into(),
                    event_frontier: 1,
                }),
                ..record(
                    "lost-checkpoint",
                    mj_core::state::SessionState::Stopped,
                    "2026-09-01T00:00:00Z",
                )
            },
        ]
        .into_iter()
        .map(|record| (record.id.clone(), record))
        .collect();
        let now = parse_time("2026-09-10T00:00:00Z").unwrap();

        let all = archive_space_over(root, &sessions, &Default::default(), now, None);
        assert_eq!(all.sessions, 3);
        assert_eq!(all.bytes, 1530);
        assert_eq!(all.reclaimable_sessions, 0);
        assert_eq!(all.reclaimable_bytes, 0);

        let aged = archive_space_over(root, &sessions, &Default::default(), now, Some(3));
        assert_eq!(aged.bytes, 1530);
        assert_eq!(
            (aged.reclaimable_sessions, aged.reclaimable_bytes),
            (2, 1030),
            "only the sessions the job would archive count, attachments included"
        );
    }

    #[test]
    fn only_stopped_sessions_past_the_cut_off_are_archived() {
        use mj_core::state::SessionState;
        let selected = ready(
            vec![
                record("old-stopped", SessionState::Stopped, "2026-09-01T00:00:00Z"),
                record(
                    "just-stopped",
                    SessionState::Stopped,
                    "2026-09-09T00:00:00Z",
                ),
                record("old-running", SessionState::Running, "2026-09-01T00:00:00Z"),
                record("old-error", SessionState::Error, "2026-09-01T00:00:00Z"),
                record("unparsable", SessionState::Stopped, "not a time"),
                // Exactly the cut-off counts as old enough.
                record("at-the-edge", SessionState::Stopped, "2026-09-07T00:00:00Z"),
            ],
            Vec::new(),
        );
        assert_eq!(selected, vec!["at-the-edge", "old-stopped"]);
    }

    #[test]
    fn a_child_the_pass_is_not_archiving_holds_its_parent_back() {
        use mj_core::state::SessionState;
        let selected = ready(
            vec![
                record("parent", SessionState::Stopped, "2026-09-01T00:00:00Z"),
                record(
                    "running-child",
                    SessionState::Running,
                    "2026-09-01T00:00:00Z",
                ),
            ],
            vec![child("running-child", "parent")],
        );
        assert!(selected.is_empty(), "the parent must wait: {selected:?}");

        let selected = ready(
            vec![
                record("parent", SessionState::Stopped, "2026-09-01T00:00:00Z"),
                record("young-child", SessionState::Stopped, "2026-09-09T00:00:00Z"),
            ],
            vec![child("young-child", "parent")],
        );
        assert!(selected.is_empty(), "the parent must wait: {selected:?}");

        // A child whose record is already gone holds nothing open.
        let selected = ready(
            vec![record(
                "parent",
                SessionState::Stopped,
                "2026-09-01T00:00:00Z",
            )],
            vec![child("departed-child", "parent")],
        );
        assert_eq!(selected, vec!["parent"]);
    }

    #[test]
    fn children_are_archived_before_their_parents() {
        use mj_core::state::SessionState;
        let selected = ready(
            vec![
                record("parent", SessionState::Stopped, "2026-09-01T00:00:00Z"),
                record("child", SessionState::Stopped, "2026-09-01T00:00:00Z"),
                record("grandchild", SessionState::Stopped, "2026-09-01T00:00:00Z"),
            ],
            vec![child("child", "parent"), child("grandchild", "child")],
        );
        assert_eq!(selected, vec!["grandchild", "child", "parent"]);
    }

    /// A Mjolnir row carries the target, profile and harness the sync stored
    /// in the index; a row from another tool carries none, because only
    /// Mjolnir writes those tags.
    #[test]
    fn query_rows_returns_the_indexed_target_profile_and_harness() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        tags::testing::index_row(&connection, "mj-session", TOOL);
        tags::testing::index_row(&connection, "codex-session", "codex");
        tags::write(
            &connection,
            "mj-session",
            &tags::MjTags {
                target: Some("Prod-Box".into()),
                profile: Some("codex-Main".into()),
                harness: Some("codex".into()),
            },
        )
        .expect("write the session metadata");

        let rows = query_rows_for_test("", 10, false).expect("query the index");
        let mjolnir = rows
            .iter()
            .find(|row| row.id == "mj-session")
            .expect("the Mjolnir row is returned");
        assert_eq!(mjolnir.target.as_deref(), Some("Prod-Box"));
        assert_eq!(mjolnir.profile.as_deref(), Some("codex-Main"));
        assert_eq!(mjolnir.harness.as_deref(), Some("codex"));

        let codex = rows
            .iter()
            .find(|row| row.id == "codex-session")
            .expect("the Codex row is returned");
        assert_eq!(codex.target, None);
        assert_eq!(codex.profile, None);
        assert_eq!(codex.harness, None);
    }

    #[test]
    fn standalone_indexed_children_are_hidden_from_every_history_listing() {
        let _held = tags::testing::lock();
        let (_directory, mut connection) = tags::testing::isolated_index();
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("projects/project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = [
            project.join("00000000-0000-4000-8000-000000000001.jsonl"),
            project.join("00000000-0000-4000-8000-000000000002.jsonl"),
            project.join("00000000-0000-4000-8000-000000000003.jsonl"),
            project.join("00000000-0000-4000-8000-000000000004.jsonl"),
        ];
        let transcript = |timestamp: &str, content: &str, sidechain: bool| {
            format!(
                "{{\"type\":\"user\",\"cwd\":\"/src/project\",\"entrypoint\":\"cli\",\"timestamp\":\"{timestamp}\",\"isSidechain\":{sidechain},\"message\":{{\"role\":\"user\",\"content\":\"{content}\"}}}}\n"
            )
        };
        for (index, path) in paths.iter().enumerate() {
            let is_child = index >= 2;
            let content = if is_child {
                "quokka quokka quokka quokka quokka quokka"
            } else {
                "quokka parent conversation"
            };
            let timestamp = match index {
                0 => "2026-10-04T00:00:00Z",
                1 => "2026-10-03T00:00:00Z",
                _ => "2026-10-05T00:00:00Z",
            };
            std::fs::write(path, transcript(timestamp, content, index == 3)).unwrap();
        }

        // This is a standalone SessionWiki sync: it sees no Mjolnir ownership
        // relation and stores both child transcripts as ordinary main rows.
        let adapter = Box::new(sessionwiki::adapters::ClaudeCode::in_home(
            home.path().to_owned(),
        ));
        sessionwiki::index::sync_with(&mut connection, &[adapter], None).unwrap();
        for path in &paths[2..] {
            let kind: String = connection
                .query_row(
                    "SELECT kind FROM files WHERE path = ?1",
                    [path.to_string_lossy()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(kind, "main", "the standalone sync misclassified {path:?}");
        }

        let mj_child_id = "mj-child-session";
        let native_mj_child_id = "00000000-0000-4000-8000-000000000003";
        let mut state = mj_core::state::State::default();
        let mut child_record = crate::database::test_session(mj_child_id, "test-project");
        child_record.native_session_id = Some(native_mj_child_id.to_owned());
        state.sessions.insert(mj_child_id.to_owned(), child_record);
        state.subagents.insert(
            mj_child_id.to_owned(),
            mj_core::subagent::SubagentRecord {
                child_session_id: mj_child_id.to_owned(),
                parent_session_id: "mj-parent-session".to_owned(),
                task_name: "child".to_owned(),
                profile_id: "codex".to_owned(),
                model: None,
                effort: None,
                working_directory: PathBuf::from("/src/project"),
                initial_prompt: "child task".to_owned(),
                request_key: "test-child".to_owned(),
                created_at: "2026-10-05T00:00:00Z".to_owned(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: false,
            },
        );
        let ownership = top_level::Snapshot::from_state(&state);
        let ids_for_paths: Vec<String> = paths
            .iter()
            .map(|path| {
                connection
                    .query_row(
                        "SELECT session_id FROM files WHERE path = ?1",
                        [path.to_string_lossy()],
                        |row| row.get(0),
                    )
                    .unwrap()
            })
            .collect();
        for id in &ids_for_paths {
            connection
                .execute(
                    "INSERT INTO touched(session_id, path) VALUES (?1, '/src/project/src/a.rs')",
                    [id],
                )
                .unwrap();
        }
        connection
            .execute(
                "UPDATE files SET title = 'standalone-only-name' WHERE session_id = ?1",
                [&ids_for_paths[2]],
            )
            .unwrap();

        let child_native_ids = BTreeSet::from([
            "00000000-0000-4000-8000-000000000003".to_owned(),
            "00000000-0000-4000-8000-000000000004".to_owned(),
        ]);
        let raw_hits = sessionwiki::index::search(&connection, "quokka", 10, None, None).unwrap();
        let first_search_ids: BTreeSet<_> = raw_hits
            .iter()
            .take(2)
            .filter_map(|hit| sessionwiki::index::native_id_of(&hit.row.path))
            .collect();
        assert_eq!(first_search_ids, child_native_ids);
        let raw_recent =
            sessionwiki::index::recent(&connection, 2, None, None, None, true).unwrap();
        let first_recent_ids: BTreeSet<_> = raw_recent
            .iter()
            .filter_map(|row| sessionwiki::index::native_id_of(&row.path))
            .collect();
        assert_eq!(first_recent_ids, child_native_ids);
        let resume = query_rows_with_snapshot_for_test("quokka", 2, false, &ownership).unwrap();
        let mut resume_ids: Vec<_> = resume.into_iter().map(|row| row.id).collect();
        resume_ids.sort();
        let mut parent_ids = ids_for_paths[..2].to_vec();
        parent_ids.sort();
        assert_eq!(resume_ids, parent_ids);

        let recent = query_rows_with_snapshot_for_test("", 2, false, &ownership).unwrap();
        let mut recent_ids: Vec<_> = recent.into_iter().map(|row| row.id).collect();
        recent_ids.sort();
        assert_eq!(recent_ids, parent_ids);
        assert!(
            query_rows_with_snapshot_for_test("standalone-only-name", 10, false, &ownership,)
                .unwrap()
                .is_empty()
        );

        let search_sessions = mj_core::history::HistoryRequest {
            request_id: "test-search".into(),
            query: mj_core::history::HistoryQuery::SearchSessions {
                query: "quokka".into(),
                limit: 2,
            },
            blame: None,
        };
        let history = history::query_in_with(
            &connection,
            &search_sessions,
            &ownership,
            &crate::import::NativeScanCache::new(),
        )
        .unwrap();
        let mut history_ids: Vec<String> = history["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect();
        history_ids.sort();
        assert_eq!(history_ids, parent_ids);

        let trace = history::query_in_with(
            &connection,
            &mj_core::history::HistoryRequest {
                request_id: "test-trace".into(),
                query: mj_core::history::HistoryQuery::TraceFile {
                    path: "src/a.rs".into(),
                    limit: 2,
                },
                blame: None,
            },
            &ownership,
            &crate::import::NativeScanCache::new(),
        )
        .unwrap();
        let mut trace_ids: Vec<String> = trace["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["session"]["id"].as_str().unwrap().to_owned())
            .collect();
        trace_ids.sort();
        assert_eq!(trace_ids, parent_ids);

        let blame_time = chrono::DateTime::parse_from_rfc3339("2026-10-05T00:00:00Z")
            .unwrap()
            .timestamp();
        let blame = history::query_in_with(
            &connection,
            &mj_core::history::HistoryRequest {
                request_id: "test-blame".into(),
                query: mj_core::history::HistoryQuery::BlameFile {
                    path: PathBuf::from("src/a.rs"),
                    start_line: 1,
                    end_line: 1,
                },
                blame: Some(mj_core::history::BlameEvidence {
                    repository: PathBuf::from("/src/project"),
                    relative_path: PathBuf::from("src/a.rs"),
                    porcelain: format!(
                        "{} 1 1 1\nauthor-time {blame_time}\n\tline\n",
                        "a".repeat(40),
                    ),
                }),
            },
            &ownership,
            &crate::import::NativeScanCache::new(),
        )
        .unwrap();
        assert_eq!(blame["runs"][0]["status"], "confident");
        assert_eq!(
            blame["runs"][0]["sessions"][0]["session_id"],
            ids_for_paths[0]
        );

        let child_brief = history::query_in_with(
            &connection,
            &mj_core::history::HistoryRequest {
                request_id: "test-child-brief".into(),
                query: mj_core::history::HistoryQuery::GetSessionBrief {
                    session_id: ids_for_paths[2].clone(),
                    max_chars: 200,
                },
                blame: None,
            },
            &ownership,
            &crate::import::NativeScanCache::new(),
        );
        assert!(child_brief.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn every_query_path_excludes_sub_agents_including_agent_history() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        for (session_id, kind) in [("main-session", "main"), ("sub-session", "sub")] {
            tags::testing::index_row(&connection, session_id, "claude");
            connection
                .execute(
                    "UPDATE files SET kind = ?2 WHERE session_id = ?1",
                    rusqlite::params![session_id, kind],
                )
                .expect("set the session kind");
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text)
                     VALUES (?1, 'user', 'fix the bridge derivation zq')",
                    [session_id],
                )
                .expect("insert a message");
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, 'fix the bridge derivation zq')",
                    [connection.last_insert_rowid()],
                )
                .expect("index the message");
        }
        let ids = |query: &str, include_tool_matches: bool| {
            let mut ids: Vec<String> = query_rows_for_test(query, 10, include_tool_matches)
                .expect("query the index")
                .into_iter()
                .map(|row| row.id)
                .collect();
            ids.sort();
            ids
        };

        // The recent list, full-text search, short-query scan, and title match.
        for query in ["", "bridge derivation", "zq", "an indexed session"] {
            assert_eq!(ids(query, false), ["main-session"], "query {query:?}");
            assert_eq!(ids(query, true), ["main-session"], "query {query:?}");
        }
    }

    /// I1-5: a phrase only a sub-agent wrote reaches its parent's index as
    /// tool text (Claude Code records the Task prompt and the sub-agent's
    /// answer as the parent's tool call and tool result). The resume search
    /// matched the parent on it while the preview, which never anchors on
    /// tool output, said "no hits". A match counts only where the preview can
    /// show it.
    // Hard-won: e53bc221: A child-only transcript phrase must not make the parent match when its preview has no hits.
    #[test]
    fn a_phrase_only_in_a_sub_agents_transcript_does_not_match_its_parent() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        let message = |session_id: &str, role: &str, text: &str| {
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
                    rusqlite::params![session_id, role, text],
                )
                .expect("insert a message");
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
                    rusqlite::params![connection.last_insert_rowid(), text],
                )
                .expect("index the message");
        };
        for (session_id, kind) in [("parent", "main"), ("child", "sub")] {
            tags::testing::index_row(&connection, session_id, "claude");
            connection
                .execute(
                    "UPDATE files SET kind = ?2 WHERE session_id = ?1",
                    rusqlite::params![session_id, kind],
                )
                .expect("set the session kind");
        }
        message("parent", "user", "look into the relay journal");
        message("parent", "tool", "Task {\"prompt\":\"read the journal\"}");
        message("parent", "tool", "the journal uses a quokka checksum");
        message(
            "parent",
            "assistant",
            "The journal is fine; the parent zebra ends here.",
        );
        message("child", "user", "read the journal");
        message("child", "assistant", "the journal uses a quokka checksum");

        let ids = |query: &str, include_tool_matches: bool| {
            let mut ids: Vec<String> = query_rows_for_test(query, 10, include_tool_matches)
                .expect("query the index")
                .into_iter()
                .map(|row| row.id)
                .collect();
            ids.sort();
            ids
        };
        assert!(
            ids("quokka", false).is_empty(),
            "{:?}",
            ids("quokka", false)
        );
        // Agent history keeps the parent's tool text, but never child rows.
        assert_eq!(ids("quokka", true), ["parent"]);
        assert_eq!(ids("parent zebra", false), ["parent"]);

        let history = history::query_in_with(
            &connection,
            &mj_core::history::HistoryRequest {
                request_id: "test-tool-search".into(),
                query: mj_core::history::HistoryQuery::SearchSessions {
                    query: "quokka".into(),
                    limit: 10,
                },
                blame: None,
            },
            &top_level::Snapshot::default(),
            &crate::import::NativeScanCache::new(),
        )
        .expect("search indexed history");
        assert_eq!(history["sessions"][0]["id"], "parent");
    }

    // Hard-won: e53bc221: Short-query fallback must not restore the tool-only false match fixed in trigram search.
    #[test]
    fn short_query_scan_also_ignores_tool_only_matches() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        tags::testing::index_row(&connection, "parent", "claude");
        connection
            .execute(
                "INSERT INTO messages(session_id, role, text) VALUES ('parent', 'tool', 'qx')",
                [],
            )
            .expect("insert a message");
        assert!(
            query_rows_for_test("qx", 10, false)
                .expect("query the index")
                .is_empty()
        );
    }

    #[test]
    fn resume_search_uses_conversational_hits_and_ands_terms_in_one_message() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        let insert_message = |session_id: &str, role: &str, text: &str| {
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
                    rusqlite::params![session_id, role, text],
                )
                .expect("insert a message");
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
                    rusqlite::params![connection.last_insert_rowid(), text],
                )
                .expect("index a message");
        };

        tags::testing::index_row(&connection, "mixed", "claude");
        insert_message("mixed", "tool", &"amber manta tool output ".repeat(20));
        insert_message(
            "mixed",
            "user",
            "amber and manta appear in this conversation",
        );
        tags::testing::index_row(&connection, "same-message", "claude");
        insert_message("same-message", "assistant", "amber appears before manta");
        tags::testing::index_row(&connection, "split-messages", "claude");
        insert_message("split-messages", "user", "amber appears here");
        insert_message("split-messages", "assistant", "manta appears there");

        let rows =
            query_rows_for_test("amber manta", 10, false).expect("search conversational messages");
        let ids: BTreeSet<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, BTreeSet::from(["mixed", "same-message"]));
        let mixed = rows.iter().find(|row| row.id == "mixed").unwrap();
        let snippet = mixed.snippet.as_deref().expect("role-filtered snippet");
        assert!(
            snippet.contains(" and ") && snippet.contains("appe"),
            "{snippet:?}"
        );
        assert!(!snippet.contains("tool output"), "{snippet:?}");
    }

    #[test]
    fn resume_search_preserves_short_nfc_and_cjk_queries() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        let insert_message = |session_id: &str, role: &str, text: &str| {
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
                    rusqlite::params![session_id, role, text],
                )
                .expect("insert a message");
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
                    rusqlite::params![connection.last_insert_rowid(), text],
                )
                .expect("index a message");
        };
        for (id, text) in [
            ("short-query", "The short token qx appears here"),
            ("nfc-query", "The café is open"),
            ("cjk-query", "東京の会話を検索する"),
        ] {
            tags::testing::index_row(&connection, id, "codex");
            insert_message(id, "assistant", text);
        }

        for (query, expected) in [
            ("qx", "short-query"),
            ("cafe\u{301}", "nfc-query"),
            ("東京", "cjk-query"),
        ] {
            let rows = query_rows_for_test(query, 10, false)
                .unwrap_or_else(|error| panic!("search {query:?}: {error:#}"));
            assert_eq!(
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                [expected],
                "query {query:?}"
            );
        }
    }

    #[test]
    fn role_filter_keeps_a_full_page_after_more_than_200_tool_only_hits() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        let insert_message = |session_id: &str, role: &str, text: &str| {
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
                    rusqlite::params![session_id, role, text],
                )
                .expect("insert a message");
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
                    rusqlite::params![connection.last_insert_rowid(), text],
                )
                .expect("index a message");
        };

        let query = "quokka kalimba";
        let tool_text = format!("{} tool output", "quokka kalimba ".repeat(16));
        for index in 0..250 {
            let id = format!("tool-only-{index:03}");
            tags::testing::index_row(&connection, &id, "claude");
            insert_message(&id, "tool", &tool_text);
        }
        for index in 0..200 {
            let id = format!("conversation-{index:03}");
            tags::testing::index_row(&connection, &id, "claude");
            insert_message(
                &id,
                "user",
                "quokka kalimba appears once in this conversation",
            );
        }

        let unfiltered = sessionwiki::index::search(&connection, query, 200, None, None)
            .expect("search all indexed roles");
        assert_eq!(unfiltered.len(), 200);
        assert!(
            unfiltered
                .iter()
                .all(|hit| hit.row.session_id.starts_with("tool-only-")),
            "tool-only hits must outrank conversational hits in the fixture"
        );

        let rows =
            query_rows_for_test(query, 200, false).expect("search a full conversational page");
        assert_eq!(rows.len(), 200);
        assert!(rows.iter().all(|row| row.id.starts_with("conversation-")));
    }

    #[test]
    fn full_text_search_fills_its_result_limit_after_skipping_sub_agents() {
        let _held = tags::testing::lock();
        let (_directory, connection) = tags::testing::isolated_index();
        for (id, kind, text) in [
            ("sub", "sub", "restic restic restic restic"),
            ("main", "main", "restic cleanup"),
        ] {
            tags::testing::index_row(&connection, id, "codex");
            connection
                .execute(
                    "UPDATE files SET kind = ?2 WHERE session_id = ?1",
                    rusqlite::params![id, kind],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, 'user', ?2)",
                    rusqlite::params![id, text],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
                    rusqlite::params![connection.last_insert_rowid(), text],
                )
                .unwrap();
        }

        let rows = query_rows_for_test("restic", 1, false).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["main"]
        );
    }

    /// A runtime of the test's own, so a test can hold the index lock without
    /// holding it across an await.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// R2-11's fallback. A destroy that outwaits a running sync pass, such as
    /// a first build, indexes the session on its own from the rows the pass
    /// would write, and does not wait for the pass. The session is then found
    /// by its id with no record left, as after the destroy.
    // Hard-won: ab7d7f49: A session destroyed between sync passes must remain searchable by its ID.
    #[test]
    fn a_destroy_indexes_the_session_itself_when_the_sync_outlasts_the_wait() {
        let _held = tags::testing::lock();
        let (_index_dir, _connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        write_archive(directory.path(), session_id, 1);
        let source = adapter(directory.path(), session_id);

        let started = Instant::now();
        let outcome = block_on(index_before_destroy_with(
            std::future::pending::<Result<()>>(),
            Duration::from_millis(200),
            move || capture_sessions_from(&source, &[session_id.to_owned()]),
            Duration::from_millis(50),
        ));

        assert_eq!(outcome, IndexedBeforeDestroy::WrittenDirectly);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the destroy must not wait for the pass: {:?}",
            started.elapsed()
        );
        let found = wiki_session_for_test(session_id, &BTreeSet::new())
            .unwrap()
            .expect("the session is found by its id");
        assert_eq!(found.status, WikiSessionStatus::Archived);
        assert_eq!(found.tool, TOOL);
        assert_eq!(
            found.path,
            PathBuf::from(format!("{}/{session_id}", directory.path().display()))
        );
        assert_eq!(found.title, "the harness title");
        assert_eq!(
            found.harness,
            Some(HarnessKind::Codex),
            "the session's metadata is written beside its row"
        );
        assert!(!found.nothing_to_restore);
    }

    /// While another writer holds the index, as a first build does while it
    /// parses one tool's sessions, the destroy goes ahead and the rows it read
    /// are written once the index is free.
    // Hard-won: ab7d7f49: A busy SessionWiki writer must not make a destroyed session disappear from search.
    #[test]
    fn a_busy_index_takes_the_destroyed_session_once_it_is_free() {
        let _held = tags::testing::lock();
        let (_index_dir, writer) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        write_archive(directory.path(), session_id, 1);
        let source = adapter(directory.path(), session_id);

        block_on(async {
            writer.execute_batch("BEGIN IMMEDIATE").unwrap();
            let outcome = index_before_destroy_with(
                std::future::pending::<Result<()>>(),
                Duration::from_millis(50),
                move || capture_sessions_from(&source, &[session_id.to_owned()]),
                Duration::from_millis(50),
            )
            .await;
            assert_eq!(outcome, IndexedBeforeDestroy::Deferred);
            assert!(
                wiki_session_for_test(session_id, &BTreeSet::new())
                    .unwrap()
                    .is_none(),
                "nothing is written while the other writer holds the index"
            );

            writer.execute_batch("COMMIT").unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while wiki_session_for_test(session_id, &BTreeSet::new())
                .unwrap()
                .is_none()
            {
                assert!(
                    Instant::now() < deadline,
                    "the deferred row never reached the index"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }

    /// A destroy of a session the index already holds as it is now runs no
    /// sync pass, so destroying a workspace or a parent with sub-agents costs
    /// one pass at most. The token compared is the one SessionWiki stored.
    #[test]
    fn a_session_the_index_holds_as_it_is_now_needs_no_indexing() {
        let _held = tags::testing::lock();
        let (_index_dir, _connection) = tags::testing::isolated_index();
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let never_prompted = "fedcba9876543210fedcba9876543210";
        write_archive(directory.path(), session_id, 1);
        let source = adapter(directory.path(), session_id);
        let ids = [session_id.to_owned(), never_prompted.to_owned()];

        assert_eq!(
            unindexed(&source, &ids).unwrap(),
            [session_id],
            "a session with no conversation has nothing to index"
        );
        let captured = Arc::new(capture_sessions_from(&source, &ids).unwrap());
        write_captured(&captured).unwrap();
        assert!(unindexed(&source, &ids).unwrap().is_empty());

        // A rename moves the change token, so the row is stale again.
        source
            .sessions
            .lock()
            .unwrap()
            .records
            .get_mut(session_id)
            .unwrap()
            .updated_at = "2099-01-01T00:00:00Z".into();
        assert_eq!(unindexed(&source, &ids).unwrap(), [session_id]);
    }

    // Hard-won: #1259: the stock Aider adapter walks $HOME at startup looking for history files.
    #[test]
    fn adapters_cover_every_supported_harness_and_no_unsupported_tool() {
        use mj_core::config::HarnessProfile;
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        for (index, kind) in HarnessKind::ALL.into_iter().enumerate() {
            let home = directory.path().join(format!("home-{index}"));
            std::fs::create_dir_all(&home).unwrap();
            config.profiles.insert(
                format!("profile-{index}"),
                HarnessProfile {
                    enabled: true,
                    kind,
                    home,
                    environment: Default::default(),
                    context_window_bytes: None,
                    subagents: Default::default(),
                    guardian_review_model: None,
                },
            );
        }
        let mut names: Vec<&str> = native_adapters(&config)
            .iter()
            .map(|adapter| adapter.name())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "claude-code",
                "codex",
                "grok-build",
                "kimi-code",
                "muse",
                "opencode"
            ],
            "one adapter per supported harness, none for tools Mjolnir cannot run"
        );
    }

    /// The text search over an in-memory index shaped like SessionWiki's.
    mod text_search {
        use super::super::{SessionTextMatch, SessionTextMatchKind, text_matches_in};
        use std::collections::BTreeSet;

        fn index(sessions: &[(&str, &[(&str, &str)])]) -> rusqlite::Connection {
            let connection = rusqlite::Connection::open_in_memory().unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE files(path TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                                        tool TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'main');
                     CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                                           role TEXT NOT NULL, text TEXT NOT NULL);
                     CREATE VIRTUAL TABLE msgs USING fts5(
                         text, content='messages', content_rowid='id', tokenize='trigram');",
                )
                .unwrap();
            for (id, messages) in sessions {
                connection
                    .execute(
                        "INSERT INTO files(path, session_id, tool) VALUES (?1, ?2, 'mjolnir')",
                        rusqlite::params![format!("/checkpoints/{id}"), id],
                    )
                    .unwrap();
                for (role, text) in *messages {
                    connection
                        .execute(
                            "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
                            rusqlite::params![id, role, text],
                        )
                        .unwrap();
                    let rowid = connection.last_insert_rowid();
                    connection
                        .execute(
                            "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
                            rusqlite::params![rowid, text],
                        )
                        .unwrap();
                }
            }
            connection
        }

        fn live(ids: &[&str]) -> BTreeSet<String> {
            ids.iter().map(|id| (*id).to_owned()).collect()
        }

        fn matches(kinds: &[(&str, SessionTextMatchKind)]) -> Vec<SessionTextMatch> {
            kinds
                .iter()
                .map(|(id, kind)| SessionTextMatch {
                    session_id: (*id).to_owned(),
                    kind: *kind,
                })
                .collect()
        }

        #[test]
        fn golden_sessions_filter_search() {
            let connection = index(&[
                ("said-by-user", &[("user", "please fix the Zebra crossing")]),
                ("said-by-agent", &[("assistant", "the zebra is fixed")]),
                (
                    "only-in-tool",
                    &[("tool", "zebra stack trace"), ("user", "hello")],
                ),
                (
                    "both",
                    &[("assistant", "a ZEBRA appears"), ("user", "a zebra please")],
                ),
                ("gone", &[("user", "zebra")]),
            ]);
            let live = live(&["said-by-user", "said-by-agent", "only-in-tool", "both"]);
            let found = text_matches_in(
                &connection,
                "zebra",
                &live,
                &super::super::top_level::Snapshot::default(),
            )
            .unwrap();
            assert_eq!(
                found,
                matches(&[
                    ("both", SessionTextMatchKind::User),
                    ("said-by-agent", SessionTextMatchKind::Agent),
                    ("said-by-user", SessionTextMatchKind::User),
                ])
            );
            let rendered = format!(
                "=== controller text search response ({} matches) ===\n{}\n",
                found.len(),
                serde_json::to_string_pretty(&found).unwrap()
            );
            mj_core::golden::assert_golden(
                env!("CARGO_MANIFEST_DIR"),
                "sessions-filter-search",
                &rendered,
            );
        }

        #[test]
        fn a_query_too_short_for_the_trigram_index_still_matches() {
            let connection = index(&[
                ("a", &[("user", "go to the zoo")]),
                ("b", &[("assistant", "zoo")]),
                ("c", &[("tool", "zoo")]),
            ]);
            assert_eq!(
                text_matches_in(
                    &connection,
                    "zo",
                    &live(&["a", "b", "c"]),
                    &super::super::top_level::Snapshot::default(),
                )
                .unwrap(),
                matches(&[
                    ("a", SessionTextMatchKind::User),
                    ("b", SessionTextMatchKind::Agent),
                ])
            );
            // A percent sign is text, not a wildcard.
            assert_eq!(
                text_matches_in(
                    &connection,
                    "%z",
                    &live(&["a"]),
                    &super::super::top_level::Snapshot::default(),
                )
                .unwrap(),
                Vec::new()
            );
        }
    }
}
