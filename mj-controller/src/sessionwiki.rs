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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use mj_client::daemon::{
    WikiHitBlock, WikiHitTranscript, WikiIndexState, WikiRow, WikiSessionInfo, WikiSessionStatus,
    WikiStatus,
};
use mj_core::config::HarnessKind;
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
    records: BTreeMap<String, SessionRecord>,
    subagent_ids: BTreeSet<String>,
    /// Session id to change token, for sessions indexed from the projection.
    live: BTreeMap<String, i64>,
}

impl Sessions {
    fn of(state: &State) -> Self {
        Self {
            records: state.sessions.clone(),
            subagent_ids: state.subagents.keys().cloned().collect(),
            live: live_tokens(state),
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
            sessions: std::sync::Mutex::new(Sessions::of(state)),
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
                    Sessions::of(&controller.state)
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
        for (session_id, archive) in newest {
            tokens.insert(session_id, archive.token);
            files.push(archive.path);
        }
        // A session that is still running is indexed from the projection, and
        // its own token replaces any checkpoint token it has: the conversation
        // has moved on since that checkpoint was written. Listing it also
        // keeps reconciliation from archiving a running session.
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live = sessions.live.clone();
        tokens.extend(live);
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
            .map(|(session_id, token)| {
                (
                    self.key_for(&session_id),
                    token
                        .saturating_mul(1024)
                        .saturating_add(i64::from(mj_transcript::summary::SUMMARY_VERSION)),
                )
            })
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
            project: record
                .and_then(|record| record.project_directory.as_ref())
                .map(|directory| directory.display().to_string())
                .unwrap_or_default(),
            started: record.and_then(|record| parse_time(&record.created_at)),
            ended: record.and_then(|record| parse_time(&record.updated_at)),
            title,
            subagent: sessions.subagent_ids.contains(session_id),
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
        let inner = Arc::new(Indexer::default());
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
        let work = crate::upgrade::activity("SessionWiki sync")?;
        self.in_flight.store(true, Ordering::Release);
        let ran = tokio::task::spawn_blocking(move || {
            let _work = work;
            sync_blocking(since)
        })
        .await;
        self.in_flight.store(false, Ordering::Release);
        let ran = ran.context("run the SessionWiki sync")??;
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

/// One synchronous sync pass. Returns false when this process must not touch
/// the index, so a refused run never records a success it did not have.
fn sync_blocking(since: Option<i64>) -> Result<bool> {
    if !index_is_writable() {
        return Ok(false);
    }
    let controller =
        Controller::load().context("load controller state for the SessionWiki sync")?;
    // Mjolnir's own sessions go first: a cold index walks every other tool's
    // store for many minutes, and a just-closed session should not wait on it.
    let mjolnir = Arc::new(MjolnirAdapter::reloading(&controller.state));
    let mut adapters: Vec<Box<dyn sessionwiki::adapters::Adapter>> =
        vec![Box::new(SharedMjolnirAdapter(Arc::clone(&mjolnir)))];
    adapters.extend(native_adapters(&controller.config));
    let mut connection = sessionwiki::index::open().context("open the SessionWiki index")?;
    sessionwiki::index::sync_with(&mut connection, &adapters, since)
        .context("sync the SessionWiki index")?;
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
/// profile home takes their place; every other built-in adapter is kept as is.
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
    adapters.extend(
        sessionwiki::adapters::all()
            .into_iter()
            .filter(|adapter| !matches!(adapter.name(), "codex" | "claude-code")),
    );
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
/// Runs SQLite work, so callers on the async runtime wrap it in
/// `spawn_blocking`.
pub fn query_rows(query: &str, limit: usize, live: &BTreeSet<String>) -> Result<Vec<WikiRow>> {
    let limit = limit.clamp(1, MAX_WIKI_LIMIT);
    if !index_is_writable() {
        // Nothing to answer from: either this process has no index of its own
        // or the one on disk is at another version. The status beside the rows
        // says which.
        return Ok(Vec::new());
    }
    let connection = open_readonly()?;
    let query = query.trim();
    if query.is_empty() {
        let rows = sessionwiki::index::recent(&connection, limit, None, None, None, false)
            .context("list recent SessionWiki sessions")?;
        let mut rows: Vec<WikiRow> = rows
            .into_iter()
            .map(|row| wiki_row(row, None, live))
            .collect();
        fill_session_tags(&connection, &mut rows)?;
        return Ok(rows);
    }
    let hits = if query.chars().count() < MIN_FULLTEXT_QUERY {
        sessionwiki::index::search_like(&connection, query, limit, None, None)
    } else {
        sessionwiki::index::search(&connection, query, limit, None, None)
    }
    .context("search the SessionWiki index")?;
    let mut rows: Vec<WikiRow> = hits
        .into_iter()
        .map(|hit| wiki_row(hit.row, Some(hit.snippet), live))
        .collect();
    // SessionWiki searches message text alone, so a session known by a title
    // or a project that is never said out loud would be unfindable. Those
    // matches follow the full-text ones rather than displacing them.
    let found: BTreeSet<String> = rows.iter().map(|row| row.id.clone()).collect();
    for row in named_like(&connection, query)? {
        if rows.len() >= limit {
            break;
        }
        if found.contains(&row.session_id) {
            continue;
        }
        rows.push(wiki_row(row, None, live));
    }
    fill_session_tags(&connection, &mut rows)?;
    Ok(rows)
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
/// whole corpus.
const NAME_SCAN_LIMIT: usize = 2_000;

/// Indexed sessions whose title or project contains the query, ignoring case.
fn named_like(
    connection: &rusqlite::Connection,
    query: &str,
) -> Result<Vec<sessionwiki::index::SessionRow>> {
    let needle = query.to_lowercase();
    let rows = sessionwiki::index::recent(connection, NAME_SCAN_LIMIT, None, None, None, false)
        .context("list recent SessionWiki sessions")?;
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
    let Some(row) = row_by_id(&connection, id)? else {
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
    let Some(row) = row_by_id(&connection, id)? else {
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

/// The matching passages of one loaded session, converted from SessionWiki's
/// own grep. Pure, so the conversion can be tested without an index on disk.
///
/// Matching, redaction and the excerpt window are `sessionwiki::grep`'s, so the
/// `sessionwiki grep` CLI and this preview report the same hits. Tool output
/// never anchors a passage: it is machine chatter the reader did not write,
/// a hit buried in it would open the preview on a wall of command output, and
/// the preview collapses tool runs anyway. Tool messages still appear as
/// context around a real match.
fn hit_transcript(
    session: &Session,
    query: &str,
    context_messages: usize,
    per_message_chars: usize,
) -> WikiHitTranscript {
    let found = sessionwiki::grep::grep_session(
        session,
        query,
        &sessionwiki::grep::GrepOpts {
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
    let Some(row) = row_by_id(&connection, id)? else {
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
    sessions: &BTreeMap<String, SessionRecord>,
    subagents: &BTreeMap<String, mj_core::subagent::SubagentRecord>,
    now: DateTime<Utc>,
    older_than_days: u32,
) -> Vec<String> {
    let cutoff = now - chrono::Duration::days(i64::from(older_than_days));
    let aged = |session_id: &String| {
        sessions.get(session_id).is_some_and(|record| {
            record.state == mj_core::state::SessionState::Stopped
                && parse_time(&record.updated_at).is_some_and(|updated| updated <= cutoff)
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
    subagents: &BTreeMap<String, mj_core::subagent::SubagentRecord>,
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
    Ok(archive_space_over(
        &mj_core::config::sessions_dir(),
        &controller.state.sessions,
        &controller.state.subagents,
        Utc::now(),
        older_than_days,
    ))
}

/// The sizing itself, over given records and a given sessions directory, so it
/// can be tested without the live data directory.
fn archive_space_over(
    sessions_root: &Path,
    sessions: &BTreeMap<String, SessionRecord>,
    subagents: &BTreeMap<String, mj_core::subagent::SubagentRecord>,
    now: DateTime<Utc>,
    older_than_days: Option<u32>,
) -> ArchiveSpacePreview {
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
        let aged = sessions_ready_to_archive(sessions, subagents, now, days);
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
            configuration: BTreeMap::new(),
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
    let Some(row) = row_by_id(&connection, wiki_id)? else {
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
        true => tags::read(&connection, &[row.session_id.as_str()])
            .context("read the indexed session metadata")?
            .remove(&row.session_id)
            .unwrap_or_default(),
        false => tags::MjTags::default(),
    };
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
    }))
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
        fn a_mjolnir_row_with_a_record_is_resumed_and_one_without_is_restored() {
            let path = Path::new("/home/user/.local/share/mj/sessions/session-7");
            assert_eq!(
                wiki_continuation("session-7", "mjolnir", path, true).unwrap(),
                WikiContinuation::Resume {
                    session_id: "session-7".to_owned(),
                }
            );
            assert_eq!(
                wiki_continuation("session-7", "mjolnir", path, false).unwrap(),
                WikiContinuation::Restore {
                    wiki_id: "session-7".to_owned(),
                }
            );
        }

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

        #[test]
        fn an_unknown_tool_is_an_error_that_names_it() {
            let error = wiki_continuation("abc123", "opencode", Path::new("/tmp/s.jsonl"), false)
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("opencode"),
                "the error has to name the tool: {error:#}"
            );
        }
    }

    use mj_checkpoint::archive::{
        ArchiveInput, BundleManifest, CanonicalExecutionState, CanonicalSessionSnapshot,
        CanonicalSessionState, CanonicalTranscriptBody, CanonicalTranscriptItem, SessionManifest,
        TargetManifest, write_archive_atomic,
    };

    use super::*;

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
                    event_frontier: 4,
                    event_frontier_digest: "a".repeat(64),
                    session: CanonicalSessionState {
                        execution: CanonicalExecutionState::Idle,
                        last_activity_at_ms: Some(1_700_000_000_004),
                        session_title: Some("snapshot title".into()),
                        configuration: BTreeMap::new(),
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
            id: session_id.into(),
            ..record_template()
        };
        MjolnirAdapter {
            sessions_dir: directory.to_path_buf(),
            sessions: std::sync::Mutex::new(Sessions {
                records: BTreeMap::from([(session_id.to_owned(), record)]),
                subagent_ids: BTreeSet::new(),
                live,
            }),
            reload: false,
        }
    }

    fn record_template() -> SessionRecord {
        SessionRecord {
            build_cache: None,
            container_workspace: None,
            mjolnir_subagents: None,
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

    fn projection(session_id: &str) -> mj_core::state::MaterializedSession {
        use mj_core::transcript::{TranscriptBody, TranscriptItem};
        let mut projected = mj_core::state::MaterializedSession::empty(session_id);
        let mut push = |position: u64, body: TranscriptBody| {
            let streamed = matches!(body, TranscriptBody::Agent { .. });
            projected
                .transcript
                .push(std::sync::Arc::new(TranscriptItem {
                    stable_id: format!("item-{position}"),
                    position,
                    latest_content_event_ordinal: streamed.then_some(position),
                    created_at_ms: 1_700_000_000_000 + i64::try_from(position).unwrap(),
                    last_changed_at_ms: 1_700_000_000_000 + i64::try_from(position).unwrap(),
                    body,
                }));
        };
        push(
            1,
            TranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": "still talking"})],
            },
        );
        push(
            2,
            TranscriptBody::Thought {
                chunks: vec![serde_json::json!({"content": {"type": "text", "text": "hmm"}})],
                streaming: false,
            },
        );
        push(
            3,
            TranscriptBody::Tool {
                call: serde_json::json!({"toolCallId": "c1", "title": "Read README.md"}),
                terminal_outputs: Vec::new(),
                terminal_refs: Vec::new(),
                presentation: None,
            },
        );
        push(
            4,
            TranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content": {"type": "text", "text": "reading"}})],
                streaming: false,
            },
        );
        projected.session_title = Some("the live title".into());
        projected
    }

    /// A session that has never been checkpointed is indexed from the
    /// daemon's own projection, with the same roles a checkpoint would give.
    #[test]
    fn a_running_session_is_indexed_from_its_stored_transcript() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let messages = projected_messages(&projection(session_id));
        assert_eq!(
            messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Tool, Role::Assistant]
        );
        assert_eq!(messages[0].text, "still talking");
        assert_eq!(messages[2].text, "reading");
        let tool: serde_json::Value = serde_json::from_str(&messages[1].text).unwrap();
        assert_eq!(tool["name"], "Read");
        assert_eq!(tool["call"]["title"], "Read README.md");
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
                (
                    key_of(running),
                    1_900_000_000 * 1024 + i64::from(mj_transcript::summary::SUMMARY_VERSION)
                ),
                (
                    key_of(never_checkpointed),
                    1_900_000_001 * 1024 + i64::from(mj_transcript::summary::SUMMARY_VERSION)
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

    /// A hit is found whatever the case of the query or of the transcript, and
    /// the reported range covers the matched text in the returned block.
    #[test]
    fn transcript_hits_locates_case_insensitive_matches() {
        let session = indexed(vec![
            (Role::User, "Make the Tests green"),
            (Role::Assistant, "the tests are green now"),
        ]);

        let found = hit_transcript(&session, "TESTS", 0, 4_000);

        assert_eq!(found.blocks.len(), 2, "both messages contain the query");
        assert_eq!(found.blocks[0].role, "user");
        let (start, end) = found.blocks[0].hits[0];
        assert_eq!(&found.blocks[0].text[start..end], "Tests");
        let (start, end) = found.blocks[1].hits[0];
        assert_eq!(&found.blocks[1].text[start..end], "tests");
        assert!(!found.blocks[0].truncated);
        assert_eq!(found.omitted_after, 0);
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

    /// A query that only occurs in tool output finds nothing, and a tool
    /// message beside a real match still comes back as context. Tool text is
    /// machine chatter: anchoring a passage on it opens the preview on command
    /// output the reader never wrote, and the preview collapses tool runs, so
    /// the match could not be shown even if it were returned.
    #[test]
    fn transcript_hits_never_anchor_on_tool_output() {
        let session = indexed(vec![
            (Role::User, "make it build"),
            (Role::Tool, "cargo build --needle"),
            (Role::Assistant, "it builds"),
        ]);

        let only_in_a_tool = hit_transcript(&session, "needle", 1, 4_000);
        assert!(
            only_in_a_tool.blocks.is_empty(),
            "tool output must not anchor a passage, got {:?}",
            only_in_a_tool.blocks
        );

        let beside_a_match = hit_transcript(&session, "builds", 1, 4_000);
        let shown: Vec<(&str, bool)> = beside_a_match
            .blocks
            .iter()
            .map(|block| (block.role.as_str(), !block.hits.is_empty()))
            .collect();
        assert_eq!(
            shown,
            vec![("tool", false), ("assistant", true)],
            "a tool message is still context around a real match"
        );
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

    /// The snapshot a restore hands to compaction has to satisfy the same
    /// validator a real checkpoint does, and has to carry every message in
    /// order.
    #[test]
    fn a_restored_snapshot_is_a_valid_transcript_of_the_indexed_session() {
        let snapshot = snapshot_of(&indexed(vec![
            (Role::User, "make the tests green"),
            (Role::Tool, "Read src/lib.rs"),
            (Role::Assistant, "they are green now"),
            (Role::User, "  "),
        ]))
        .unwrap();

        snapshot.validate().expect("the snapshot is well formed");
        assert_eq!(snapshot.event_frontier, 3);
        assert_eq!(
            snapshot.session.session_title.as_deref(),
            Some("the archived session")
        );
        assert!(snapshot.session.last_activity_at_ms.is_some());
        let bodies = snapshot
            .transcript
            .iter()
            .map(|item| match &item.body {
                mj_core::archive::CanonicalTranscriptBody::User { content } => (
                    "user",
                    mj_core::transcript::materialized_content_text(content),
                ),
                mj_core::archive::CanonicalTranscriptBody::Agent { chunks, .. } => (
                    "agent",
                    mj_core::transcript::materialized_chunks_text(chunks),
                ),
                mj_core::archive::CanonicalTranscriptBody::Tool { call, .. } => (
                    "tool",
                    call["title"].as_str().unwrap_or_default().to_owned(),
                ),
                _ => ("other", String::new()),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bodies,
            vec![
                ("user", "make the tests green".to_owned()),
                ("tool", "Read src/lib.rs".to_owned()),
                ("agent", "they are green now".to_owned()),
            ],
            "the blank message is dropped and every other one keeps its role"
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
    }

    fn record(
        session_id: &str,
        state: mj_core::state::SessionState,
        updated_at: &str,
    ) -> SessionRecord {
        SessionRecord {
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
        let sessions: BTreeMap<String, SessionRecord> = [
            sized_session(root, "old-stopped", "2026-09-01T00:00:00Z", 1000, &[10, 20]),
            sized_session(root, "just-stopped", "2026-09-09T00:00:00Z", 500, &[]),
            // A record whose checkpoint file is already gone counts as zero
            // rather than failing the whole estimate.
            SessionRecord {
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

        let all = archive_space_over(root, &sessions, &BTreeMap::new(), now, None);
        assert_eq!(all.sessions, 3);
        assert_eq!(all.bytes, 1530);
        assert_eq!(all.reclaimable_sessions, 0);
        assert_eq!(all.reclaimable_bytes, 0);

        let aged = archive_space_over(root, &sessions, &BTreeMap::new(), now, Some(3));
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

    #[test]
    fn native_adapters_cover_every_enabled_profile_home() {
        use mj_core::config::{Config, HarnessKind, HarnessProfile};

        fn profile(kind: HarnessKind, home: &str, enabled: bool) -> HarnessProfile {
            HarnessProfile {
                enabled,
                kind,
                home: PathBuf::from(home),
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            }
        }

        let mut config = Config::default();
        for (id, built) in [
            (
                "codex",
                profile(HarnessKind::Codex, "/home/dev/.codex3", true),
            ),
            (
                "codex-ds",
                profile(HarnessKind::Codex, "/home/dev/.codex-ds", true),
            ),
            // A second profile on one home must not add a second adapter.
            (
                "codex-alt",
                profile(HarnessKind::Codex, "/home/dev/.codex3", true),
            ),
            (
                "codex-off",
                profile(HarnessKind::Codex, "/home/dev/.codex-off", false),
            ),
            (
                "claude",
                profile(HarnessKind::Claude, "/home/dev/.claude4", true),
            ),
            ("kimi", profile(HarnessKind::Kimi, "/home/dev/.kimi", true)),
            ("grok", profile(HarnessKind::Grok, "/home/dev/.grok", true)),
            ("muse", profile(HarnessKind::Muse, "/home/dev/muse", true)),
            (
                "muse-off",
                profile(HarnessKind::Muse, "/home/dev/muse-off", false),
            ),
        ] {
            config.profiles.insert(id.into(), built);
        }

        let adapters = native_adapters(&config);
        let roots: Vec<(&str, Option<PathBuf>)> = adapters
            .iter()
            .map(|adapter| (adapter.name(), adapter.root()))
            .collect();

        let codex: Vec<&Option<PathBuf>> = roots
            .iter()
            .filter(|(name, _)| *name == "codex")
            .map(|(_, root)| root)
            .collect();
        assert_eq!(
            codex,
            vec![
                &Some(PathBuf::from("/home/dev/.codex3/sessions")),
                &Some(PathBuf::from("/home/dev/.codex-ds/sessions")),
            ],
            "one adapter per enabled Codex home, deduplicated: {roots:?}"
        );

        let claude: Vec<&Option<PathBuf>> = roots
            .iter()
            .filter(|(name, _)| *name == "claude-code")
            .map(|(_, root)| root)
            .collect();
        assert_eq!(
            claude,
            vec![&Some(PathBuf::from("/home/dev/.claude4/projects"))],
            "one adapter for the enabled Claude home: {roots:?}"
        );

        for (_, root) in &roots {
            let Some(root) = root else { continue };
            let text = root.to_string_lossy();
            assert!(
                !text.contains(".codex-off"),
                "a disabled profile must not be indexed: {roots:?}"
            );
            assert!(
                !text.ends_with("/.codex/sessions") && !text.ends_with("/.claude/projects"),
                "the stock homes are not indexed unless a profile names them: {roots:?}"
            );
        }

        // SessionWiki has no adapter for these three, so Mjolnir supplies one
        // per enabled profile home under its own tool name.
        for (name, root) in [
            ("kimi-code", PathBuf::from("/home/dev/.kimi/sessions")),
            ("grok-build", PathBuf::from("/home/dev/.grok/sessions")),
            (
                "muse",
                mj_checkpoint::native::muse_sessions_root(Path::new("/home/dev/muse")).unwrap(),
            ),
        ] {
            let found: Vec<&Option<PathBuf>> = roots
                .iter()
                .filter(|(found, _)| *found == name)
                .map(|(_, root)| root)
                .collect();
            assert_eq!(found, vec![&Some(root)], "one {name} adapter: {roots:?}");
        }

        for (_, root) in &roots {
            let Some(root) = root else { continue };
            assert!(
                !root.to_string_lossy().contains("muse-off"),
                "a disabled profile must not be indexed: {roots:?}"
            );
        }

        assert!(
            roots.iter().any(|(name, _)| *name == "gemini"),
            "the other built-in adapters are kept: {roots:?}"
        );
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

        let rows = query_rows("", 10, &BTreeSet::new()).expect("query the index");
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
}
