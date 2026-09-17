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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use mj_core::state::{SessionRecord, State};
use sessionwiki::adapters::{Adapter, Discovered, Store};
use sessionwiki::model::{Message, Role, Session};

use crate::controller::Controller;
use crate::controller::checkpoint::managed_checkpoint_archive_name;

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

/// What indexing needs from controller state: which sessions exist, and which
/// of them are sub-agent children.
#[derive(Default)]
struct Sessions {
    records: BTreeMap<String, SessionRecord>,
    subagent_ids: BTreeSet<String>,
}

impl Sessions {
    fn of(state: &State) -> Self {
        Self {
            records: state.sessions.clone(),
            subagent_ids: state.subagents.keys().cloned().collect(),
        }
    }
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
        let mut keys = Vec::with_capacity(newest.len());
        let mut files = Vec::with_capacity(newest.len());
        for (session_id, archive) in newest {
            keys.push((self.key_for(&session_id), archive.token));
            files.push(archive.path);
        }
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
        let (newest, _) = self.newest_archives();
        let archive = newest
            .get(session_id)
            .with_context(|| format!("no checkpoint archive for session {session_id}"))?;
        let snapshot = mj_checkpoint::archive::read_archive_verified(&archive.path)
            .with_context(|| format!("read checkpoint {}", archive.path.display()))?
            .canonical_session()
            .with_context(|| format!("read the transcript of session {session_id}"))?;

        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = sessions.records.get(session_id);
        let mut messages = Vec::new();
        for item in &snapshot.transcript {
            let ts = DateTime::from_timestamp_millis(item.created_at_ms);
            let (role, text) = match &item.body {
                mj_core::archive::CanonicalTranscriptBody::User { content } => (
                    Role::User,
                    mj_core::transcript::materialized_content_text(content),
                ),
                mj_core::archive::CanonicalTranscriptBody::Agent { chunks, .. } => (
                    Role::Assistant,
                    mj_core::transcript::materialized_chunks_text(chunks),
                ),
                // The tool's own title, which is what the transcript shows the
                // user. Arguments and output are not worth indexing.
                mj_core::archive::CanonicalTranscriptBody::Tool { call, .. } => (
                    Role::Tool,
                    call.get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
                _ => continue,
            };
            let text = text.trim().to_owned();
            if text.is_empty() {
                continue;
            }
            messages.push(Message { role, text, ts });
        }

        let title = record
            .and_then(|record| record.session_title_override.clone())
            .or_else(|| record.and_then(|record| record.acp_session_title.clone()))
            .or_else(|| snapshot.session.session_title.clone())
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
            touched: Vec::new(),
            edits: Vec::new(),
        })
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
        if is_busy(error) {
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
        let ran = tokio::task::spawn_blocking(move || sync_blocking(since))
            .await
            .context("run the SessionWiki sync")??;
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

/// One synchronous sync pass. Returns false when SessionWiki is switched off,
/// so a disabled daemon never records a success it did not have.
fn sync_blocking(since: Option<i64>) -> Result<bool> {
    let controller =
        Controller::load().context("load controller state for the SessionWiki sync")?;
    if !controller.config.sessionwiki.enabled {
        return Ok(false);
    }
    // Mjolnir's own sessions go first: a cold index walks every other tool's
    // store for many minutes, and a just-closed session should not wait on it.
    let mut adapters: Vec<Box<dyn sessionwiki::adapters::Adapter>> =
        vec![Box::new(MjolnirAdapter::reloading(&controller.state))];
    adapters.extend(sessionwiki::adapters::all());
    let mut connection = sessionwiki::index::open().context("open the SessionWiki index")?;
    sessionwiki::index::sync_with(&mut connection, &adapters, since)
        .context("sync the SessionWiki index")?;
    Ok(true)
}

/// Whether a failure is SQLite reporting another writer, which a later trigger
/// simply retries.
fn is_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

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
                                    "title": "Read config.toml",
                                    "status": "completed"
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
        let record = SessionRecord {
            build_cache: None,
            container_workspace: None,
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
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
        };
        MjolnirAdapter {
            sessions_dir: directory.to_path_buf(),
            sessions: std::sync::Mutex::new(Sessions {
                records: BTreeMap::from([(session_id.to_owned(), record)]),
                subagent_ids: BTreeSet::new(),
            }),
            reload: false,
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
            session
                .messages
                .iter()
                .map(|message| (message.role, message.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (Role::User, "index this session"),
                (Role::Tool, "Read config.toml"),
                (Role::Assistant, "done"),
            ]
        );
    }
}
