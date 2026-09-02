//! Normalized controller state and composer history stored in SQLite.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::hel_config::data_dir;
use crate::hel_state::{
    CheckpointMetadata, HelState, HostContainerSize, ManagedWorktree, MaterializedExecutionState,
    MaterializedQueuedPrompt, MaterializedSession, MaterializedSessionSummary, ProjectionWindow,
    SessionRecord, SessionResourceAllocation, SessionState, TargetLocator, TranscriptBody,
    TranscriptItem, validate_relay_event_digest, validate_relay_event_frontier,
};
use crate::hel_targets::AdditionalMount;
use crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST;
use crate::hel_workspace::{
    DEFAULT_WORKSPACE_ID, DetachedDraft, WorkspaceRecord, new_workspace_id,
    normalize_workspace_name,
};

const SCHEMA_VERSION: i64 = 21;

/// A deterministic projection integrity violation. Retrying cannot fix it, so
/// callers must report it separately from transport failures.
#[derive(Debug)]
pub struct ProjectionIntegrityError(pub String);

impl std::fmt::Display for ProjectionIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProjectionIntegrityError {}

/// A store whose schema is not the one this build supports.
///
/// Carried as a typed cause rather than a message so the daemon can tell a
/// store that moved underneath it from a transport failure. It survives every
/// `anyhow` hop to the caller, which finds it with `error.chain()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSchemaMismatch {
    pub found: i64,
    pub supported: i64,
}

impl std::fmt::Display for StoreSchemaMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { found, supported } = self;
        // Direction decides the advice. A store ahead of this build cannot be
        // migrated by starting a daemon of this build -- that is what the old
        // single message told the user to do, for an hour.
        if found > supported {
            write!(
                formatter,
                "Mjolnir database schema {found} is newer than this Mjolnir build supports ({supported}); upgrade Mjolnir"
            )
        } else {
            write!(
                formatter,
                "Mjolnir database schema {found} is not the supported schema {supported}; start the Mjolnir daemon to migrate it"
            )
        }
    }
}

impl std::error::Error for StoreSchemaMismatch {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryScope {
    Project,
    Session,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryEntry {
    pub id: i64,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionApplyOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptMutation {
    Upsert(TranscriptItem),
    Remove { stable_id: String },
}

/// Changes derived from one relay event. `None` leaves a scalar untouched;
/// the nested option on `session_title` permits explicitly clearing it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MaterializedSessionMutation {
    /// Relay receipt time for this event. Persistence and the actor cache both
    /// take a monotonic maximum so removing detail rows cannot move activity
    /// backwards.
    pub last_activity_at_ms: Option<i64>,
    pub execution: Option<MaterializedExecutionState>,
    pub session_title: Option<Option<String>>,
    pub configuration: Option<BTreeMap<String, serde_json::Value>>,
    pub transcript: Vec<TranscriptMutation>,
    pub queued_prompts: Option<Vec<MaterializedQueuedPrompt>>,
    pub pending_elicitations: Option<Vec<crate::hel_elicitation::ElicitationRequest>>,
}

mod schema;

pub use schema::database_path;
#[cfg(test)]
use schema::{forget_verified_schema, table_has_column};
use schema::{open, open_reader};

const DATABASE_WRITE_QUEUE_CAPACITY: usize = 256;

/// A queued write, handed either the writer's connection or the reason it
/// must not be used. The job -- not the lane -- decides what a refusal means
/// to its caller.
type DatabaseWriteJob =
    Box<dyn FnOnce(std::result::Result<&mut Connection, StoreSchemaMismatch>) + Send + 'static>;

enum DatabaseWriterMessage {
    Run {
        label: &'static str,
        job: DatabaseWriteJob,
    },
    Shutdown,
}

/// Cloneable submission handle for the daemon's ordered SQLite write lane.
///
/// Calling [`DatabaseWriter::execute`] is synchronous and may apply bounded
/// backpressure, so async and UI callers must invoke database mutations from
/// their existing supervised blocking tasks.
#[derive(Clone)]
pub struct DatabaseWriter {
    id: u64,
    sender: SyncSender<DatabaseWriterMessage>,
}

impl std::fmt::Debug for DatabaseWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseWriter")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl DatabaseWriter {
    fn execute<T, F>(&self, label: &'static str, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.sender
            .send(DatabaseWriterMessage::Run {
                label,
                job: Box::new(move |connection| {
                    let reply = match connection {
                        Ok(connection) => operation(connection),
                        // The mismatch travels as the operation's own failure,
                        // so a refused write reports why rather than the
                        // writer-stopped message a dropped reply would give.
                        Err(mismatch) => Err(anyhow::Error::new(mismatch)),
                    };
                    let _ = reply_tx.send(reply);
                }),
            })
            .map_err(|_| {
                anyhow::anyhow!("submit database writer operation {label}: writer stopped")
            })?;
        reply_rx
            .recv()
            .with_context(|| format!("database writer stopped during {label}"))?
    }
}

/// Owns the daemon's writer thread and persistent SQLite connection.
///
/// The owner is deliberately not cloneable. Dropping it removes the global
/// submission handle, drains accepted work in FIFO order, and joins the
/// thread before releasing the connection.
pub struct DatabaseWriterOwner {
    writer: DatabaseWriter,
    thread: Option<JoinHandle<()>>,
    stopped: Receiver<Result<()>>,
}

impl std::fmt::Debug for DatabaseWriterOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseWriterOwner")
            .field("writer", &self.writer)
            .finish_non_exhaustive()
    }
}

impl DatabaseWriterOwner {
    pub fn shutdown(mut self) -> Result<()> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<()> {
        if self.thread.is_none() {
            return Ok(());
        }
        clear_database_writer(self.writer.id);
        let send_result = self.writer.sender.send(DatabaseWriterMessage::Shutdown);
        let worker_result = self
            .stopped
            .recv()
            .context("database writer stopped without reporting its result")?;
        let join_result = self
            .thread
            .take()
            .expect("database writer thread checked above")
            .join();
        if let Err(panic) = join_result {
            std::panic::resume_unwind(panic);
        }
        match (send_result, worker_result) {
            (_, Err(error)) => Err(error),
            (Err(_), Ok(())) => bail!("request database writer shutdown: writer stopped"),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

impl Drop for DatabaseWriterOwner {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_inner() {
            tracing::error!(%error, "database writer did not shut down cleanly");
        }
    }
}

fn database_writer_slot() -> &'static Mutex<Option<DatabaseWriter>> {
    static WRITER: OnceLock<Mutex<Option<DatabaseWriter>>> = OnceLock::new();
    WRITER.get_or_init(|| Mutex::new(None))
}

fn clear_database_writer(id: u64) {
    let mut installed = database_writer_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if installed.as_ref().is_some_and(|writer| writer.id == id) {
        *installed = None;
    }
}

/// Install the process-wide writer for a test that owns its data directory.
///
/// Production installs this once, in the daemon, after `ControllerStoreGuard`
/// establishes exclusivity, and the daemon is then the only process that
/// writes. A test may do the same only because it re-execs itself with its own
/// `MJ_DATA_DIR` and is therefore alone in its process — which is exactly why
/// the tests that need this are shaped that way.
///
/// The returned owner has to be held for the rest of the test: dropping it
/// stops the writer, and the next write fails with the message above.
#[cfg(test)]
#[must_use = "the writer stops when this owner is dropped"]
pub(crate) fn install_isolated_test_writer() -> DatabaseWriterOwner {
    start_database_writer().expect("install the writer for an isolated test child")
}

pub(crate) fn start_database_writer() -> Result<DatabaseWriterOwner> {
    start_database_writer_at(&database_path(), true)
}

fn start_database_writer_at(path: &Path, install_globally: bool) -> Result<DatabaseWriterOwner> {
    static NEXT_WRITER_ID: AtomicU64 = AtomicU64::new(1);

    let connection = schema::open_writer(path)?;
    let path = path.to_owned();
    let (sender, receiver) = sync_channel(DATABASE_WRITE_QUEUE_CAPACITY);
    let (stopped_tx, stopped) = sync_channel(1);
    let id = NEXT_WRITER_ID.fetch_add(1, Ordering::Relaxed);
    let writer = DatabaseWriter { id, sender };
    if install_globally {
        let mut installed = database_writer_slot()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(installed.is_none(), "database writer is already running");
        *installed = Some(writer.clone());
    }
    let thread = match thread::Builder::new()
        .name("hel-database-writer".to_owned())
        .spawn(move || {
            let mut connection = connection;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match receiver.recv() {
                        Ok(DatabaseWriterMessage::Run { label, job }) => {
                            tracing::trace!(operation = label, "running database writer operation");
                            // This connection verified the schema once, when it
                            // opened. Another process can migrate the store
                            // afterwards, and this lane would keep writing rows
                            // a foreign ladder no longer expects. Re-read the
                            // recorded version per job: it is one pragma
                            // against an open connection.
                            match writer_schema_state(&path, &connection, label) {
                                Ok(()) => job(Ok(&mut connection)),
                                Err(mismatch) => job(Err(mismatch)),
                            }
                        }
                        Ok(DatabaseWriterMessage::Shutdown) => break Ok(()),
                        Err(error) => {
                            break Err(error).context("database writer queue disconnected");
                        }
                    }
                }
            }))
            .unwrap_or_else(|panic| {
                let detail = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic payload");
                Err(anyhow::anyhow!("database writer thread panicked: {detail}"))
            });
            clear_database_writer(id);
            let _ = stopped_tx.send(result);
        }) {
        Ok(thread) => thread,
        Err(error) => {
            if install_globally {
                clear_database_writer(id);
            }
            return Err(error).context("spawn database writer thread");
        }
    };
    Ok(DatabaseWriterOwner {
        writer,
        thread: Some(thread),
        stopped,
    })
}

/// Whether the writer's own connection still sees the schema it opened.
///
/// A version that reads successfully and differs is divergence in either
/// direction: a store rolled back under this connection is as foreign to it as
/// one migrated forward. A pragma that fails to read is not divergence at all
/// -- the connection is broken, and the job it is about to run reports its own
/// I/O error, which is a truer message than a schema claim this code cannot
/// support.
fn writer_schema_state(
    path: &Path,
    connection: &Connection,
    label: &'static str,
) -> std::result::Result<(), StoreSchemaMismatch> {
    let version: i64 = match connection.query_row("PRAGMA user_version", [], |row| row.get(0)) {
        Ok(version) => version,
        Err(error) => {
            tracing::warn!(
                operation = label,
                path = %path.display(),
                error = %error,
                "could not read the store's schema version before a write; running the operation"
            );
            return Ok(());
        }
    };
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    tracing::error!(
        operation = label,
        path = %path.display(),
        found = version,
        supported = SCHEMA_VERSION,
        "the store's schema moved underneath this writer; refusing the operation"
    );
    Err(StoreSchemaMismatch {
        found: version,
        supported: SCHEMA_VERSION,
    })
}

fn submit_database_write<T, F>(label: &'static str, operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
{
    let writer = database_writer_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Some(writer) = writer {
        writer.execute(label, operation)
    } else {
        // There is one way to write, and this is not it. In production the
        // daemon installs the writer after `ControllerStoreGuard` establishes
        // exclusivity, and it is the only process that writes; a caller
        // reaching here has no exclusivity and would be competing with
        // whatever does. This used to open `database_path()` directly, which
        // meant any process without a writer silently wrote to — and migrated
        // — the real user database as a side effect of doing something else.
        bail!("database writer is not available for operation {label}")
    }
}

pub fn load_state() -> Result<HelState> {
    load_state_from(&database_path())
}

pub fn list_workspaces() -> Result<Vec<WorkspaceRecord>> {
    list_workspaces_from(&database_path())
}

pub fn list_workspaces_from(path: &Path) -> Result<Vec<WorkspaceRecord>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT w.workspace_id, w.name, w.created_at, w.last_opened_at,
                count(c.session_id)
          FROM workspaces w
           LEFT JOIN session_contexts c USING(workspace_id)
          GROUP BY w.workspace_id
         HAVING w.workspace_id != 'default' OR count(c.session_id) > 0
          ORDER BY w.last_opened_at DESC, w.created_at DESC, w.workspace_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(WorkspaceRecord {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            last_opened_at: row.get(3)?,
            session_count: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub fn create_workspace(name: &str) -> Result<WorkspaceRecord> {
    let name = name.to_owned();
    submit_database_write("create_workspace", move |_| {
        create_workspace_at(&database_path(), &name)
    })
}

/// Create the named workspace, or return the concurrently-created winner.
///
/// Interactive setup uses this operation after presenting a snapshot of the
/// workspace list. Several selectors can therefore submit the same normalized
/// name legitimately. Explicit database creation remains strict through
/// [`create_workspace`].
pub fn create_or_get_workspace(name: &str) -> Result<WorkspaceRecord> {
    let name = name.to_owned();
    submit_database_write("create_or_get_workspace", move |_| {
        create_or_get_workspace_at(&database_path(), &name)
    })
}

pub fn create_or_get_workspace_at(path: &Path, name: &str) -> Result<WorkspaceRecord> {
    let (name, name_key) = normalize_workspace_name(name)?;
    let id = new_workspace_id()?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut connection = open(path)?;
    let transaction = connection.transaction()?;
    transaction
        .execute(
            "INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name_key) DO NOTHING",
            params![id, name, name_key, now],
        )
        .with_context(|| format!("create or find workspace {name:?}"))?;
    let workspace = transaction.query_row(
        "SELECT w.workspace_id, w.name, w.created_at, w.last_opened_at,
                count(c.session_id)
           FROM workspaces w
           LEFT JOIN session_contexts c USING(workspace_id)
          WHERE w.name_key = ?1
          GROUP BY w.workspace_id",
        params![name_key],
        |row| {
            Ok(WorkspaceRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                last_opened_at: row.get(3)?,
                session_count: row.get(4)?,
            })
        },
    )?;
    transaction.commit()?;
    Ok(workspace)
}

pub fn create_workspace_at(path: &Path, name: &str) -> Result<WorkspaceRecord> {
    let (name, name_key) = normalize_workspace_name(name)?;
    let id = new_workspace_id()?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let connection = open(path)?;
    connection
        .execute(
            "INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, name, name_key, now],
        )
        .with_context(|| format!("create workspace {name:?}"))?;
    Ok(WorkspaceRecord {
        id,
        name,
        created_at: now.clone(),
        last_opened_at: now,
        session_count: 0,
    })
}

pub fn rename_workspace(workspace_id: &str, name: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    let name = name.to_owned();
    submit_database_write("rename_workspace", move |_| {
        rename_workspace_at(&database_path(), &workspace_id, &name)
    })
}

pub fn rename_workspace_at(path: &Path, workspace_id: &str, name: &str) -> Result<()> {
    let (name, name_key) = normalize_workspace_name(name)?;
    let connection = open(path)?;
    let changed = connection
        .execute(
            "UPDATE workspaces SET name = ?2, name_key = ?3 WHERE workspace_id = ?1",
            params![workspace_id, name, name_key],
        )
        .with_context(|| format!("rename workspace to {name:?}"))?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    Ok(())
}

pub fn touch_workspace(workspace_id: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("touch_workspace", move |_| {
        touch_workspace_at(&database_path(), &workspace_id)
    })
}

pub fn touch_workspace_at(path: &Path, workspace_id: &str) -> Result<()> {
    let connection = open(path)?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = connection.execute(
        "UPDATE workspaces SET last_opened_at = ?2 WHERE workspace_id = ?1",
        params![workspace_id, now],
    )?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    Ok(())
}

pub fn delete_empty_workspace(workspace_id: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("delete_empty_workspace", move |_| {
        delete_empty_workspace_at(&database_path(), &workspace_id)
    })
}

pub fn delete_empty_workspace_at(path: &Path, workspace_id: &str) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let context_count: u64 = tx.query_row(
        "SELECT count(*) FROM session_contexts WHERE workspace_id = ?1",
        [workspace_id],
        |row| row.get(0),
    )?;
    let draft_count: u64 = tx.query_row(
        "SELECT count(*) FROM detached_drafts WHERE workspace_id = ?1",
        [workspace_id],
        |row| row.get(0),
    )?;
    ensure!(
        context_count == 0 && draft_count == 0,
        "workspace is not empty ({context_count} session histories, {draft_count} drafts)"
    );
    let changed = tx.execute(
        "DELETE FROM workspaces WHERE workspace_id = ?1",
        [workspace_id],
    )?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    tx.commit()?;
    Ok(())
}

pub fn workspace_for_session(session_id: &str) -> Result<Option<String>> {
    workspace_for_session_at(&database_path(), session_id)
}

pub fn workspace_for_session_at(path: &Path, session_id: &str) -> Result<Option<String>> {
    open_reader(path)?
        .query_row(
            "SELECT workspace_id FROM session_contexts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

pub fn session_ids_for_workspace(workspace_id: &str) -> Result<Vec<String>> {
    session_ids_for_workspace_at(&database_path(), workspace_id)
}

pub fn session_ids_for_workspace_at(path: &Path, workspace_id: &str) -> Result<Vec<String>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT c.session_id
           FROM session_contexts c
           JOIN sessions s USING(session_id)
          WHERE c.workspace_id = ?1
          ORDER BY c.created_at, c.session_id",
    )?;
    let rows = statement.query_map([workspace_id], |row| row.get(0))?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

/// Assign a newly-created session context to a workspace. Existing contexts
/// are immutable: moving sessions is deliberately outside the v1 model.
pub fn assign_new_session_workspace(session_id: &str, workspace_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    submit_database_write("assign_new_session_workspace", move |_| {
        assign_new_session_workspace_at(&database_path(), &session_id, &workspace_id)
    })
}

pub fn assign_new_session_workspace_at(
    path: &Path,
    session_id: &str,
    workspace_id: &str,
) -> Result<()> {
    let connection = open(path)?;
    let current: String = connection
        .query_row(
            "SELECT workspace_id FROM session_contexts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .with_context(|| format!("find session context {session_id:?}"))?;
    if current == workspace_id {
        return Ok(());
    }
    ensure!(
        current == DEFAULT_WORKSPACE_ID,
        "session {session_id} already belongs to workspace {current}"
    );
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
        [workspace_id],
        |row| row.get(0),
    )?;
    ensure!(exists, "unknown workspace {workspace_id:?}");
    connection.execute(
        "UPDATE session_contexts SET workspace_id = ?2 WHERE session_id = ?1",
        params![session_id, workspace_id],
    )?;
    Ok(())
}

pub fn client_read_frontier(client_id: &str, workspace_id: &str, session_id: &str) -> Result<u64> {
    client_read_frontier_at(&database_path(), client_id, workspace_id, session_id)
}

fn client_read_frontier_at(
    path: &Path,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
) -> Result<u64> {
    let connection = open_reader(path)?;
    let client: Option<u64> = connection
        .query_row(
            "SELECT through_event_ordinal
               FROM client_read_frontiers
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
            params![client_id, workspace_id, session_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(frontier) = client {
        return Ok(frontier);
    }
    connection
        .query_row(
            "SELECT s.viewed_through_event_ordinal
               FROM sessions s JOIN session_contexts c USING(session_id)
              WHERE s.session_id = ?1 AND c.workspace_id = ?2",
            params![session_id, workspace_id],
            |row| row.get(0),
        )
        .with_context(|| format!("find session {session_id:?} in workspace {workspace_id:?}"))
}

pub fn advance_client_read_frontier(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    let client_id = client_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    submit_database_write("advance_client_read_frontier", move |_| {
        advance_client_read_frontier_at(
            &database_path(),
            &client_id,
            &workspace_id,
            &session_id,
            through,
        )
    })
}

/// Atomically advance both the per-client and legacy session read frontiers.
/// Neither value changes when validation or persistence fails.
/// One viewer's stored state for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientSessionState {
    pub draft: String,
    pub through_event_ordinal: u64,
}

/// What this viewer has stored for this session: an unsent draft and how far
/// it has read.
pub fn client_session_state(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
) -> Result<ClientSessionState> {
    let connection = open_reader(&database_path())?;
    let draft = connection
        .query_row(
            "SELECT draft FROM client_session_state
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
            params![client_id, workspace_id, session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default();
    let through_event_ordinal = connection
        .query_row(
            "SELECT through_event_ordinal FROM client_read_frontiers
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
            params![client_id, workspace_id, session_id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .unwrap_or_default();
    Ok(ClientSessionState {
        draft,
        through_event_ordinal,
    })
}

/// Store one viewer's unsent draft.
///
/// An empty draft deletes the row rather than storing emptiness, so a viewer
/// that cleared its composer stops occupying a row and stops being pruned
/// later for something it no longer holds.
pub fn persist_client_draft(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    draft: &str,
) -> Result<()> {
    ensure!(!client_id.trim().is_empty(), "client id is empty");
    let client_id = client_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    let draft = draft.to_owned();
    submit_database_write("persist_client_draft", move |connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if draft.is_empty() {
            transaction.execute(
                "DELETE FROM client_session_state
                  WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
                params![client_id, workspace_id, session_id],
            )?;
            transaction.commit()?;
            return Ok(());
        }
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let changed = transaction.execute(
            "INSERT INTO client_session_state(
                 client_id, workspace_id, session_id, draft, updated_at
             )
             SELECT ?1, ?2, ?3, ?4, ?5
              WHERE EXISTS(
                  SELECT 1 FROM session_contexts
                   WHERE session_id = ?3 AND workspace_id = ?2
              )
             ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
                 draft = excluded.draft,
                 updated_at = excluded.updated_at",
            params![client_id, workspace_id, session_id, draft, now],
        )?;
        ensure!(
            changed == 1,
            "session {session_id:?} is not in workspace {workspace_id:?}"
        );
        transaction.commit()?;
        Ok(())
    })
}

/// Forget web-viewer state that has passed its retention.
///
/// Only rows whose client id names a phone are considered. A terminal client's
/// read frontier is not the phone's to expire, and deleting one would lose a
/// person's place in a conversation they are still reading.
pub fn prune_phone_client_state(older_than: Duration) -> Result<usize> {
    let cutoff = (Utc::now() - chrono::Duration::from_std(older_than).unwrap_or_default())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    submit_database_write("prune_phone_client_state", move |connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let drafts = transaction.execute(
            "DELETE FROM client_session_state
              WHERE client_id LIKE 'phone:%' AND updated_at < ?1",
            params![cutoff],
        )?;
        let frontiers = transaction.execute(
            "DELETE FROM client_read_frontiers
              WHERE client_id LIKE 'phone:%' AND updated_at < ?1",
            params![cutoff],
        )?;
        transaction.commit()?;
        Ok(drafts + frontiers)
    })
}

pub fn persist_read_receipt(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    let client_id = client_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    submit_database_write("persist_read_receipt", move |connection| {
        persist_read_receipt_with(connection, &client_id, &workspace_id, &session_id, through)
    })
}

fn persist_read_receipt_with(
    connection: &mut Connection,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    ensure!(!client_id.trim().is_empty(), "client id is empty");
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let applied = transaction
        .query_row(
            "SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .with_context(|| format!("unknown session {session_id}"))?;
    ensure!(
        through <= applied,
        "cannot acknowledge event ordinal {through} for session {session_id}; projection is at {applied}"
    );
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = transaction.execute(
        "INSERT INTO client_read_frontiers(
             client_id, workspace_id, session_id, through_event_ordinal, updated_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE EXISTS(
              SELECT 1 FROM session_contexts
               WHERE session_id = ?3 AND workspace_id = ?2
          )
         ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
             through_event_ordinal = max(
                 client_read_frontiers.through_event_ordinal,
                 excluded.through_event_ordinal
             ),
             updated_at = excluded.updated_at",
        params![client_id, workspace_id, session_id, through, now],
    )?;
    ensure!(
        changed == 1,
        "session {session_id:?} is not in workspace {workspace_id:?}"
    );
    let changed = transaction.execute(
        "UPDATE sessions
         SET viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?2)
         WHERE session_id = ?1",
        params![session_id, through],
    )?;
    ensure!(changed == 1, "unknown session {session_id}");
    let frontier = transaction.query_row(
        "SELECT through_event_ordinal
           FROM client_read_frontiers
          WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
        params![client_id, workspace_id, session_id],
        |row| row.get(0),
    )?;
    transaction.commit()?;
    Ok(frontier)
}

fn advance_client_read_frontier_at(
    path: &Path,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    ensure!(!client_id.trim().is_empty(), "client id is empty");
    let connection = open(path)?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = connection.execute(
        "INSERT INTO client_read_frontiers(
             client_id, workspace_id, session_id, through_event_ordinal, updated_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE EXISTS(
              SELECT 1 FROM session_contexts
               WHERE session_id = ?3 AND workspace_id = ?2
          )
         ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
             through_event_ordinal = max(
                 client_read_frontiers.through_event_ordinal,
                 excluded.through_event_ordinal
             ),
             updated_at = excluded.updated_at",
        params![client_id, workspace_id, session_id, through, now],
    )?;
    ensure!(
        changed == 1,
        "session {session_id:?} is not in workspace {workspace_id:?}"
    );
    client_read_frontier_at(path, client_id, workspace_id, session_id)
}

pub fn save_detached_draft(
    workspace_id: &str,
    session_id: Option<&str>,
    source: &str,
    owner_pid: Option<u32>,
    text: &str,
) -> Result<Option<String>> {
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.map(str::to_owned);
    let source = source.to_owned();
    let text = text.to_owned();
    submit_database_write("save_detached_draft", move |_| {
        save_detached_draft_at(
            &database_path(),
            &workspace_id,
            session_id.as_deref(),
            &source,
            owner_pid,
            &text,
        )
    })
}

fn save_detached_draft_at(
    path: &Path,
    workspace_id: &str,
    session_id: Option<&str>,
    source: &str,
    owner_pid: Option<u32>,
    text: &str,
) -> Result<Option<String>> {
    if text.is_empty() {
        return Ok(None);
    }
    ensure!(!source.trim().is_empty(), "draft source is empty");
    let id = new_workspace_id()?;
    let saved_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let connection = open(path)?;
    connection.execute(
        "INSERT INTO detached_drafts(
             draft_id, workspace_id, session_id, source, owner_pid, saved_at, text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            workspace_id,
            session_id,
            source,
            owner_pid,
            saved_at,
            text
        ],
    )?;
    Ok(Some(id))
}

pub fn list_detached_drafts(workspace_id: &str) -> Result<Vec<DetachedDraft>> {
    list_detached_drafts_at(&database_path(), workspace_id)
}

fn list_detached_drafts_at(path: &Path, workspace_id: &str) -> Result<Vec<DetachedDraft>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT draft_id, workspace_id, session_id, source, owner_pid, saved_at, text,
                recovered_at
           FROM detached_drafts
          WHERE workspace_id = ?1 AND recovered_at IS NULL
          ORDER BY saved_at DESC, draft_id DESC",
    )?;
    let rows = statement.query_map([workspace_id], |row| {
        Ok(DetachedDraft {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            session_id: row.get(2)?,
            source: row.get(3)?,
            owner_pid: row.get(4)?,
            saved_at: row.get(5)?,
            text: row.get(6)?,
            recovered_at: row.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub fn mark_draft_recovered(draft_id: &str) -> Result<()> {
    let draft_id = draft_id.to_owned();
    submit_database_write("mark_draft_recovered", move |_| {
        mark_draft_recovered_at(&database_path(), &draft_id)
    })
}

fn mark_draft_recovered_at(path: &Path, draft_id: &str) -> Result<()> {
    let connection = open(path)?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = connection.execute(
        "UPDATE detached_drafts SET recovered_at = ?2
          WHERE draft_id = ?1 AND recovered_at IS NULL",
        params![draft_id, now],
    )?;
    ensure!(
        changed == 1,
        "unknown or already recovered draft {draft_id:?}"
    );
    Ok(())
}

/// Explicitly restore a detached draft into its session composer. This is the
/// only operation that merges client-local draft state back into the legacy
/// session field, and the transaction marks the source draft recovered at the
/// same durable boundary.
pub fn recover_detached_draft(draft_id: &str) -> Result<String> {
    let draft_id = draft_id.to_owned();
    submit_database_write("recover_detached_draft", move |_| {
        recover_detached_draft_at(&database_path(), &draft_id)
    })
}

fn recover_detached_draft_at(path: &Path, draft_id: &str) -> Result<String> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (session_id, text): (Option<String>, String) = tx
        .query_row(
            "SELECT session_id, text FROM detached_drafts
              WHERE draft_id = ?1 AND recovered_at IS NULL",
            [draft_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .with_context(|| format!("find recoverable draft {draft_id:?}"))?;
    let session_id = session_id.context("draft is not associated with a session")?;
    let changed = tx.execute(
        "UPDATE sessions SET draft_input = ?2 WHERE session_id = ?1",
        params![session_id, text],
    )?;
    ensure!(changed == 1, "draft session no longer exists");
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    tx.execute(
        "UPDATE detached_drafts SET recovered_at = ?2 WHERE draft_id = ?1",
        params![draft_id, now],
    )?;
    tx.commit()?;
    Ok(session_id)
}

pub fn load_state_from(path: &Path) -> Result<HelState> {
    let connection = open_reader(path)?;
    let mut state = HelState::default();
    let mut statement = connection.prepare(
        "SELECT s.session_id, s.title, s.harness_kind, s.last_profile, c.bundle_id,
                s.target_template_id, s.state, s.native_session_id, s.acp_session_title,
                s.session_title_override, c.created_at, s.updated_at,
                s.viewed_through_event_ordinal, s.last_error, s.resource_allocation,
                s.last_checkpoint_error, s.project_directory, s.managed_worktree,
                s.draft_input, s.container_cpus, s.container_memory, s.archived
                , c.workspace_id
         FROM sessions s JOIN session_contexts c USING(session_id)
         ORDER BY s.session_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SessionRecord {
            workspace_id: row.get(22)?,
            archived: row.get(21)?,
            container_cpus: row.get(19)?,
            container_memory: row.get(20)?,
            id: row.get(0)?,
            title: row.get(1)?,
            harness_kind: row.get::<_, String>(2)?.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::<dyn std::error::Error + Send + Sync>::from(format!("{error:#}")),
                )
            })?,
            last_profile: row.get(3)?,
            bundle_id: row.get(4)?,
            project_directory: row.get_ref(16)?.blob_or_null()?.map(blob_to_path),
            managed_worktree: row
                .get::<_, Option<String>>(17)?
                .map(|json| serde_json::from_str::<ManagedWorktree>(&json))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        17,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            target_template_id: row.get(5)?,
            resource_allocation: row
                .get::<_, Option<String>>(14)?
                .map(|json| serde_json::from_str::<SessionResourceAllocation>(&json))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        14,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            additional_mounts: Vec::new(),
            state: parse_session_state(&row.get::<_, String>(6)?),
            target: None,
            native_session_id: row.get(7)?,
            acp_session_title: row
                .get::<_, Option<String>>(8)?
                .as_deref()
                .and_then(crate::hel_state::normalize_session_title),
            session_title_override: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
            viewed_through_event_ordinal: row.get::<_, u64>(12)?,
            draft_input: row.get(18)?,
            last_error: row.get(13)?,
            last_checkpoint_error: row.get(15)?,
            checkpoint: None,
        })
    })?;
    for row in rows {
        let session = row?;
        state.sessions.insert(session.id.clone(), session);
    }
    load_targets(&connection, &mut state)?;
    load_mounts(&connection, &mut state)?;
    load_checkpoints(&connection, &mut state)?;
    let mut statement =
        connection.prepare("SELECT host, source FROM mount_history ORDER BY host, ordinal")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            blob_to_path(row.get_ref(1)?.as_blob()?),
        ))
    })?;
    for row in rows {
        let (host, source) = row?;
        state.mount_history.entry(host).or_default().push(source);
    }
    let mut statement = connection
        .prepare("SELECT host, cpus, memory_bytes FROM host_container_sizes ORDER BY host")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            HostContainerSize {
                cpus: row.get::<_, i64>(1)? as u64,
                memory_bytes: row.get::<_, i64>(2)? as u64,
            },
        ))
    })?;
    for row in rows {
        let (host, size) = row?;
        state.container_sizes.insert(host, size);
    }
    state.validate()?;
    Ok(state)
}

pub fn save_state(state: &HelState) -> Result<()> {
    let state = state.clone();
    submit_database_write("save_state", move |_| {
        save_state_to(&database_path(), &state)
    })
}

/// Persist one operational session without rewriting unrelated controller
/// state. Dashboard lifecycle jobs use this path so independent jobs can
/// commit concurrently without restoring stale copies of other sessions.
pub fn save_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_session", move |_| {
        save_session_to(&database_path(), &session)
    })
}

/// Persist a session and the container size it most recently launched on its
/// host in one transaction.
pub fn save_session_with_container_size(
    session: &SessionRecord,
    host: &str,
    size: HostContainerSize,
) -> Result<()> {
    let session = session.clone();
    let host = host.to_owned();
    submit_database_write("save_session_with_container_size", move |_| {
        save_session_with_container_size_to(&database_path(), &session, Some((&host, size)))
    })
}

/// Update only the fields a lifecycle transition owns on a session that
/// already exists. Everything else — display titles, checkpoints, container
/// settings, and attached directories — stays with its own writer.
pub fn save_lifecycle_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_lifecycle_session", move |_| {
        save_lifecycle_session_to(&database_path(), &session)
    })
}

/// Install a lifecycle transition together with the checkpoint it just
/// verified and the harness session id that produced it.
pub fn save_checkpointed_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_checkpointed_session", move |_| {
        save_checkpointed_session_to(&database_path(), &session)
    })
}

/// Recover lifecycle rows stranded by a process exit during checkpoint
/// creation. This must be called once by the top-level controller process
/// while it owns the controller-store guard, not by per-operation reloads.
pub fn recover_interrupted_checkpointing_sessions(updated_at: &str) -> Result<usize> {
    let updated_at = updated_at.to_owned();
    submit_database_write("recover_interrupted_checkpointing_sessions", move |_| {
        recover_interrupted_checkpointing_sessions_to(&database_path(), &updated_at)
    })
}

/// Change only the user-owned display name. This avoids writing a stale
/// SessionRecord over independently committed checkpoint or relay metadata.
pub fn set_session_title_override(session_id: &str, title: &str, updated_at: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let title = title.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("set_session_title_override", move |_| {
        set_session_title_override_to(&database_path(), &session_id, &title, &updated_at)
    })
}

/// Rewrite a configured profile id in every persisted session in one SQLite
/// transaction. Configuration is stored separately, so the controller owns
/// coordinating this update with the matching config-map rename.
pub fn rename_profile_references(old_id: &str, new_id: &str) -> Result<usize> {
    rename_session_reference("last_profile", old_id, new_id)
}

/// Rewrite a configured target id in every persisted session in one SQLite
/// transaction.
pub fn rename_target_references(old_id: &str, new_id: &str) -> Result<usize> {
    rename_session_reference("target_template_id", old_id, new_id)
}

fn rename_session_reference(column: &'static str, old_id: &str, new_id: &str) -> Result<usize> {
    ensure!(
        matches!(column, "last_profile" | "target_template_id"),
        "unsupported session reference column"
    );
    let old_id = old_id.to_owned();
    let new_id = new_id.to_owned();
    submit_database_write("rename_session_reference", move |_| {
        rename_session_reference_at(&database_path(), column, &old_id, &new_id)
    })
}

fn rename_session_reference_at(
    path: &Path,
    column: &str,
    old_id: &str,
    new_id: &str,
) -> Result<usize> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        &format!("UPDATE sessions SET {column} = ?2 WHERE {column} = ?1"),
        params![old_id, new_id],
    )?;
    tx.commit()?;
    Ok(changed)
}

/// Change only whether the resume dialog hides this session. Archiving is a
/// display choice, so it has its own writer and never rewrites lifecycle,
/// checkpoint, or title columns another task owns.
pub fn set_session_archived(session_id: &str, archived: bool) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("set_session_archived", move |_| {
        set_session_archived_to(&database_path(), &session_id, archived)
    })
}

/// Record that the managed target of an otherwise live session is definitively
/// gone. A verified checkpoint keeps the session recoverable as an error on the
/// dashboard; without one, the session is lost. The state predicate keeps a
/// late poll result from overwriting a concurrent lifecycle transition.
pub fn mark_session_target_missing(
    session_id: &str,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionState>> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("mark_session_target_missing", move |_| {
        mark_session_target_missing_to(&database_path(), &session_id, &detail, &updated_at)
    })
}

fn mark_session_target_missing_to(
    path: &Path,
    session_id: &str,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionState>> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET state = CASE
                 WHEN EXISTS(
                     SELECT 1 FROM session_checkpoints
                     WHERE session_checkpoints.session_id = sessions.session_id
                 ) THEN 'error'
                 ELSE 'lost'
             END,
             last_error = ?2,
             updated_at = ?3
         WHERE session_id = ?1
           AND state IN ('provisioning', 'running', 'disconnected', 'error')",
        params![session_id, detail, updated_at],
    )?;
    ensure!(changed <= 1, "updated {changed} sessions for {session_id}");
    let state = if changed == 1 {
        let stored: String = tx.query_row(
            "SELECT state FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        Some(parse_session_state(&stored))
    } else {
        None
    };
    tx.commit()?;
    Ok(state)
}

fn set_session_archived_to(path: &Path, session_id: &str, archived: bool) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET archived = ?2 WHERE session_id = ?1",
        params![session_id, archived],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Native sessions the resume dialog hides. Hel never writes into a harness
/// home, so the hidden set lives here instead of in the harness's own store.
pub fn hidden_native_sessions() -> Result<BTreeSet<(crate::hel_config::HarnessKind, String)>> {
    hidden_native_sessions_from(&database_path())
}

fn hidden_native_sessions_from(
    path: &Path,
) -> Result<BTreeSet<(crate::hel_config::HarnessKind, String)>> {
    let connection = open_reader(path)?;
    let mut statement =
        connection.prepare("SELECT harness_kind, native_session_id FROM hidden_native_sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut hidden = BTreeSet::new();
    for row in rows {
        let (harness, native_session_id) = row?;
        let harness = harness
            .parse::<crate::hel_config::HarnessKind>()
            .with_context(|| format!("unknown harness {harness:?} in the hidden session set"))?;
        hidden.insert((harness, native_session_id));
    }
    Ok(hidden)
}

/// Hide or reveal one native session in the resume dialog.
pub fn set_native_session_hidden(
    harness: crate::hel_config::HarnessKind,
    native_session_id: &str,
    hidden: bool,
) -> Result<()> {
    let native_session_id = native_session_id.to_owned();
    submit_database_write("set_native_session_hidden", move |_| {
        set_native_session_hidden_to(&database_path(), harness, &native_session_id, hidden)
    })
}

fn set_native_session_hidden_to(
    path: &Path,
    harness: crate::hel_config::HarnessKind,
    native_session_id: &str,
    hidden: bool,
) -> Result<()> {
    if native_session_id.trim().is_empty() {
        bail!("native session id is empty");
    }
    let connection = open(path)?;
    if hidden {
        connection.execute(
            "INSERT INTO hidden_native_sessions(harness_kind, native_session_id, hidden_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(harness_kind, native_session_id) DO NOTHING",
            params![harness.id(), native_session_id, Utc::now().to_rfc3339()],
        )?;
    } else {
        connection.execute(
            "DELETE FROM hidden_native_sessions
             WHERE harness_kind = ?1 AND native_session_id = ?2",
            params![harness.id(), native_session_id],
        )?;
    }
    Ok(())
}

/// Change only the per-session container provisioning inputs: the size
/// overrides and the attached directories. Everything else the session row
/// owns is left to its own writer.
pub fn set_session_container_settings(
    session_id: &str,
    cpus: Option<&str>,
    memory: Option<&str>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let cpus = cpus.map(str::to_owned);
    let memory = memory.map(str::to_owned);
    let mounts = mounts.to_vec();
    let updated_at = updated_at.to_owned();
    submit_database_write("set_session_container_settings", move |_| {
        set_session_container_settings_to(
            &database_path(),
            &session_id,
            cpus.as_deref(),
            memory.as_deref(),
            &mounts,
            &updated_at,
        )
    })
}

fn set_session_container_settings_to(
    path: &Path,
    session_id: &str,
    cpus: Option<&str>,
    memory: Option<&str>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    if updated_at.trim().is_empty() {
        bail!("session update timestamp is empty");
    }
    crate::hel_targets::validate_additional_mounts(mounts)?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET container_cpus = ?2, container_memory = ?3, updated_at = ?4
         WHERE session_id = ?1",
        params![session_id, cpus, memory, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    tx.execute(
        "DELETE FROM session_mounts WHERE session_id = ?1",
        [session_id],
    )?;
    for (ordinal, mount) in mounts.iter().enumerate() {
        tx.execute(
            "INSERT INTO session_mounts(session_id, ordinal, source, destination, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                ordinal as i64,
                path_to_blob(&mount.source),
                path_to_blob(&mount.destination),
                mount.read_only
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn set_session_title_override_to(
    path: &Path,
    session_id: &str,
    title: &str,
    updated_at: &str,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("session title is empty");
    }
    if updated_at.trim().is_empty() {
        bail!("session update timestamp is empty");
    }
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions
         SET session_title_override = ?2, updated_at = ?3
         WHERE session_id = ?1",
        params![session_id, title, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Persist the latest ACP-provided title without replacing unrelated session
/// fields that may have changed in another supervised controller task.
pub fn set_session_acp_title(session_id: &str, title: Option<&str>) -> Result<()> {
    let session_id = session_id.to_owned();
    let title = title.map(str::to_owned);
    submit_database_write("set_session_acp_title", move |_| {
        set_session_acp_title_to(&database_path(), &session_id, title.as_deref())
    })
}

fn set_session_acp_title_to(path: &Path, session_id: &str, title: Option<&str>) -> Result<()> {
    if title.is_some_and(|title| title.trim().is_empty()) {
        bail!("ACP session title is empty");
    }
    let title = title.and_then(crate::hel_state::normalize_session_title);
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET acp_session_title = ?2 WHERE session_id = ?1",
        params![session_id, title],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Commit the successful handshake for a newly provisioned worker without
/// replacing checkpoint or display metadata owned by other controller tasks.
pub fn mark_session_worker_connected(
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.map(str::to_owned);
    let updated_at = updated_at.to_owned();
    submit_database_write("mark_session_worker_connected", move |_| {
        mark_session_worker_connected_to(
            &database_path(),
            &session_id,
            native_session_id.as_deref(),
            &updated_at,
        )
    })
}

fn mark_session_worker_connected_to(
    path: &Path,
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    if updated_at.trim().is_empty() {
        bail!("worker connection timestamp is empty");
    }
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions
         SET state = 'running',
             native_session_id = coalesce(?2, native_session_id),
             updated_at = ?3,
             last_error = NULL
         WHERE session_id = ?1",
        params![session_id, native_session_id, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

fn recover_interrupted_checkpointing_sessions_to(path: &Path, updated_at: &str) -> Result<usize> {
    if updated_at.trim().is_empty() {
        bail!("checkpoint recovery timestamp is empty");
    }
    let connection = open(path)?;
    connection
        .execute(
            "UPDATE sessions
             SET state = 'running', updated_at = ?1, last_checkpoint_error = ?2
             WHERE state = 'checkpointing'",
            params![
                updated_at,
                "checkpointing was interrupted by a controller restart; the target was left running"
            ],
        )
        .context("recover interrupted checkpointing sessions")
}

fn save_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    save_session_with_container_size_to(path, session, None)
}

fn save_session_with_container_size_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(existing_bundle) = tx
        .query_row(
            "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
            [session.id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && existing_bundle != session.bundle_id
    {
        bail!(
            "session {} was already associated with bundle {}, not {}",
            session.id,
            existing_bundle,
            session.bundle_id
        );
    }
    insert_session(&tx, session)?;
    if let Some((host, size)) = container_size {
        write_host_container_size(&tx, host, size)?;
    }
    tx.commit()?;
    Ok(())
}

fn save_lifecycle_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    update_lifecycle_fields(&tx, session)?;
    tx.commit()?;
    Ok(())
}

fn save_checkpointed_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    update_lifecycle_fields(&tx, session)?;
    tx.execute(
        "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
        params![session.id, session.native_session_id],
    )?;
    replace_checkpoint(&tx, session)?;
    tx.commit()?;
    Ok(())
}

fn validate_session_record(session: &SessionRecord) -> Result<()> {
    let mut validation = HelState::default();
    validation
        .sessions
        .insert(session.id.clone(), session.clone());
    validation.validate()
}

/// Remove one operational session while retaining its relational history
/// context and prompt history.
pub fn delete_session(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("delete_session", move |_| {
        delete_session_from(&database_path(), &session_id)
    })
}

fn delete_session_from(path: &Path, session_id: &str) -> Result<()> {
    let connection = open(path)?;
    connection.execute("DELETE FROM sessions WHERE session_id = ?1", [session_id])?;
    Ok(())
}

/// Load a session's whole projection, transcript and all.
///
/// Crate-private on purpose. The cost of this call is everything that has ever
/// happened in the conversation, and the callers that made that a visible
/// problem — the runtime poll and the resume reply — were both outside this
/// crate. What they wanted was [`load_materialized_projection_tail`]; what
/// they reached for was this, because it was public and its name did not say
/// otherwise. The remaining caller owns a live projection and genuinely needs
/// all of it.
pub(crate) fn load_materialized_session(session_id: &str) -> Result<Option<MaterializedSession>> {
    load_materialized_session_from(&database_path(), session_id)
}

/// Load only the projection fields needed by dashboard session summaries.
/// Transcript bodies for tools, plans, thoughts, and old messages stay in
/// SQLite, which keeps dashboard startup independent of transcript size.
pub fn load_materialized_session_summary(
    session_id: &str,
) -> Result<Option<MaterializedSessionSummary>> {
    load_materialized_session_summary_from(&database_path(), session_id)
}

fn load_materialized_session_summary_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<MaterializedSessionSummary>> {
    let connection = open_reader(path)?;
    let row = connection
        .query_row(
            "SELECT applied_event_ordinal, last_activity_at_ms, execution_state,
                    running_started_at_ms, session_title
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((
        applied_event_ordinal,
        last_activity_at_ms,
        execution,
        running_started_at_ms,
        session_title,
    )) = row
    else {
        return Ok(None);
    };

    let last_user_message = last_materialized_user_message(&connection, session_id)?;
    let last_agent_message = last_materialized_agent_message(&connection, session_id)?;
    let last_agent_message_follows_last_user =
        last_agent_message
            .as_ref()
            .is_some_and(|(agent_position, _)| {
                last_user_message
                    .as_ref()
                    .is_none_or(|(user_position, _)| agent_position > user_position)
            });
    let mut ordinal_statement = connection.prepare(
        "SELECT latest_content_event_ordinal
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND latest_content_event_ordinal IS NOT NULL
           AND EXISTS (
               SELECT 1 FROM json_each(
                   CASE
                       WHEN latest_content_event_ordinal IS NOT NULL
                           AND json_valid(body_json)
                       THEN body_json
                       ELSE '{}'
                   END,
                   '$.chunks'
               ) AS chunk
               WHERE json_extract(chunk.value, '$.content.type') IS NOT NULL
                 AND (
                     json_extract(chunk.value, '$.content.type') <> 'text'
                     OR trim(coalesce(json_extract(chunk.value, '$.content.text'), '')) <> ''
                 )
           )
         ORDER BY position, stable_id",
    )?;
    let agent_message_latest_content_ordinals = ordinal_statement
        .query_map([session_id], |row| row.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let restart_pattern = format!("{}*", crate::hel_transcript::SESSION_RESTART_ITEM_PREFIX);
    let mut restart_statement = connection.prepare(
        "SELECT position
         FROM materialized_transcript_items
         WHERE session_id = ?1 AND stable_id GLOB ?2
         ORDER BY position, stable_id",
    )?;
    let session_restart_event_ordinals = restart_statement
        .query_map((session_id, restart_pattern), |row| row.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Some(MaterializedSessionSummary {
        session_id: session_id.to_owned(),
        applied_event_ordinal,
        last_activity_at_ms,
        execution: parse_materialized_execution(&execution, running_started_at_ms)?,
        session_title,
        last_agent_message: last_agent_message.map(|(_, message)| message),
        last_user_message: last_user_message.map(|(_, message)| message),
        last_agent_message_follows_last_user,
        agent_message_latest_content_ordinals,
        session_restart_event_ordinals,
    }))
}

/// The oldest visible user message, which is where a session's provisional
/// title comes from. It sits at the head of the transcript, so a projection
/// loaded as a tail cannot find it by scanning; this reads it directly.
fn first_materialized_user_message(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    materialized_user_message(connection, session_id, true)
}

fn last_materialized_user_message(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    materialized_user_message(connection, session_id, false)
}

fn materialized_user_message(
    connection: &Connection,
    session_id: &str,
    oldest_first: bool,
) -> Result<Option<(u64, String)>> {
    let mut statement = connection.prepare(if oldest_first {
        "SELECT position, body_json
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND json_extract(
               CASE
                   WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                   THEN body_json
                   ELSE '{}'
               END,
               '$.kind'
           ) = 'user'
         ORDER BY position, stable_id"
    } else {
        "SELECT position, body_json
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND json_extract(
               CASE
                   WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                   THEN body_json
                   ELSE '{}'
               END,
               '$.kind'
           ) = 'user'
         ORDER BY position DESC, stable_id DESC"
    })?;
    let rows = statement.query_map([session_id], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (position, body_json) = row?;
        let body: TranscriptBody = serde_json::from_str(&body_json)
            .with_context(|| format!("parse materialized user message for session {session_id}"))?;
        let TranscriptBody::User { content } = body else {
            continue;
        };
        let text = crate::hel_chat::materialized_content_text(&content);
        if !text.trim().is_empty() {
            return Ok(Some((position, text)));
        }
    }
    Ok(None)
}

/// Where the newest turn began: a user message, or the marker for a turn the
/// harness started on its own. This is the recovery boundary, so it reads a
/// position only and never has to decode a transcript body.
fn last_materialized_turn_start(connection: &Connection, session_id: &str) -> Result<Option<u64>> {
    Ok(connection
        .query_row(
            "SELECT position
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND (
                   stable_id GLOB ?2
                   OR json_extract(
                       CASE
                           WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                           THEN body_json
                           ELSE '{}'
                       END,
                       '$.kind'
                   ) = 'user'
               )
             ORDER BY position DESC, stable_id DESC
             LIMIT 1",
            params![
                session_id,
                format!("{}*", crate::hel_transcript::HARNESS_TURN_ITEM_PREFIX)
            ],
            |row| row.get::<_, u64>(0),
        )
        .optional()?)
}

fn last_materialized_agent_message(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    let row = connection
        .query_row(
            "SELECT position, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND latest_content_event_ordinal IS NOT NULL
               AND EXISTS (
                   SELECT 1 FROM json_each(
                       CASE
                           WHEN latest_content_event_ordinal IS NOT NULL
                               AND json_valid(body_json)
                           THEN body_json
                           ELSE '{}'
                       END,
                       '$.chunks'
                   ) AS chunk
                   WHERE json_extract(chunk.value, '$.content.type') IS NOT NULL
                     AND (
                         json_extract(chunk.value, '$.content.type') <> 'text'
                         OR trim(coalesce(json_extract(chunk.value, '$.content.text'), '')) <> ''
                     )
               )
             ORDER BY position DESC, stable_id DESC
             LIMIT 1",
            [session_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((position, body_json)) = row else {
        return Ok(None);
    };
    let body: TranscriptBody = serde_json::from_str(&body_json)
        .with_context(|| format!("parse materialized agent message for session {session_id}"))?;
    let TranscriptBody::Agent { chunks, .. } = body else {
        return Ok(None);
    };
    let text = crate::hel_chat::materialized_chunks_text(&chunks);
    Ok((!text.trim().is_empty()).then_some((position, text)))
}

/// Read the newest `limit` transcript items for a session, oldest first.
///
/// A conversation view seeds itself from the tail and discards everything
/// before it — `ChatState::from_materialized_tail` keeps `TAIL_SEED_ITEMS`
/// and drops the rest — so reading the whole transcript to show the end of it
/// is work proportional to history for a result that never was. On a real
/// session that meant reading 28,066 rows to render 256.
///
/// The `materialized_transcript_position` index covers the ordering, so this
/// costs the rows it returns rather than the rows that exist.
pub fn load_materialized_transcript_tail(
    session_id: &str,
    limit: usize,
) -> Result<Vec<Arc<TranscriptItem>>> {
    load_materialized_transcript_tail_from(&database_path(), session_id, limit)
}

fn load_materialized_transcript_tail_from(
    path: &Path,
    session_id: &str,
    limit: usize,
) -> Result<Vec<Arc<TranscriptItem>>> {
    read_materialized_transcript(&open_reader(path)?, session_id, Some(limit))
}

/// How many transcript rows one retention pass rewrites.
///
/// The daemon is the single database writer, so a pass that rewrote every row
/// of a long session would stall every other write behind it. A capped pass
/// leaves the rest for the next checkpoint, which is the next time any of it
/// becomes redundant anyway.
const RETENTION_BATCH_ITEMS: usize = 4_096;

/// Rows below this are already small enough that rewriting them would cost
/// more than it reclaims.
const RETENTION_BODY_FLOOR_BYTES: usize = 4 * 1024;

/// What one retention pass reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptRetention {
    pub items: usize,
    pub bytes: usize,
    /// Rows this pass left for the next one, because of
    /// `RETENTION_BATCH_ITEMS`.
    pub remaining: bool,
}

/// Drop tool output that a verified checkpoint already holds.
///
/// The projection only ever grew: the only deletes were a per-item remove, a
/// whole-session wipe, and the `sessions` cascade. One measured session reached
/// 28,066 items and 635 MiB, of which 561 MB was tool-call content.
///
/// A checkpoint archive carries the complete transcript up to its event
/// frontier, and one checkpoint per session is retained, so every item at or
/// below `event_frontier` is durably recorded elsewhere. What stays here is
/// what the transcript still shows: which tool ran, on what, with what result,
/// and each edit's diffstat. See
/// [`crate::hel_transcript::compact_tool_call_for_retention`].
pub fn compact_materialized_transcript_through(
    session_id: &str,
    event_frontier: u64,
) -> Result<TranscriptRetention> {
    let session_id = session_id.to_owned();
    submit_database_write("compact_materialized_transcript", move |_| {
        compact_materialized_transcript_in(&database_path(), &session_id, event_frontier)
    })
}

fn compact_materialized_transcript_in(
    path: &Path,
    session_id: &str,
    event_frontier: u64,
) -> Result<TranscriptRetention> {
    let mut connection = open(path)?;
    let candidates = {
        let mut statement = connection.prepare(
            "SELECT stable_id, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND position <= ?2
               AND length(body_json) > ?3
               AND json_extract(
                   CASE WHEN json_valid(body_json) THEN body_json ELSE '{}' END,
                   '$.kind'
               ) = 'tool'
             ORDER BY position, stable_id
             LIMIT ?4",
        )?;
        statement
            .query_map(
                params![
                    session_id,
                    event_frontier,
                    RETENTION_BODY_FLOOR_BYTES as i64,
                    RETENTION_BATCH_ITEMS as i64 + 1
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let remaining = candidates.len() > RETENTION_BATCH_ITEMS;
    let mut retention = TranscriptRetention {
        remaining,
        ..TranscriptRetention::default()
    };
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (stable_id, body_json) in candidates.into_iter().take(RETENTION_BATCH_ITEMS) {
        let mut body: TranscriptBody = match serde_json::from_str(&body_json) {
            Ok(body) => body,
            // A row this cannot read is a row it must not rewrite.
            Err(error) => {
                tracing::warn!(%session_id, %stable_id, %error, "skipping unreadable transcript body");
                continue;
            }
        };
        if !crate::hel_transcript::compact_tool_call_for_retention(&mut body) {
            continue;
        }
        let compacted = serde_json::to_string(&body)
            .with_context(|| format!("serialize compacted transcript body {stable_id}"))?;
        if compacted.len() >= body_json.len() {
            continue;
        }
        transaction.execute(
            "UPDATE materialized_transcript_items SET body_json = ?3
             WHERE session_id = ?1 AND stable_id = ?2",
            params![session_id, stable_id, compacted],
        )?;
        retention.items += 1;
        retention.bytes += body_json.len() - compacted.len();
    }
    transaction.commit()?;
    Ok(retention)
}

/// How many transcript items a polled projection carries.
///
/// Every viewer of a polled projection is bounded already: the conversation
/// pane keeps `hel_chat::TAIL_SEED_ITEMS` (256) entries, and the browser
/// transcript keeps 1,000 rendered lines. This is set above both, since an
/// entry renders to at least one line, so the window is the whole of what any
/// of them would show.
pub const PROJECTION_TAIL_ITEMS: usize = 1_024;

/// Load a projection carrying only the end of its transcript.
///
/// The steady-state poll reloads a session's projection every time anything
/// about it moves. Loading the whole transcript to do that is work
/// proportional to everything that has ever happened in the conversation —
/// 635 MiB and 28,066 items on one measured session — for a view that shows
/// the last few hundred entries. This reads the window instead, plus the two
/// facts that live outside it, each with one indexed query. See
/// [`ProjectionWindow`].
pub fn load_materialized_projection_tail(
    session_id: &str,
    transcript_limit: usize,
) -> Result<Option<(MaterializedSession, ProjectionWindow)>> {
    load_materialized_projection_tail_from(&database_path(), session_id, transcript_limit)
}

fn load_materialized_projection_tail_from(
    path: &Path,
    session_id: &str,
    transcript_limit: usize,
) -> Result<Option<(MaterializedSession, ProjectionWindow)>> {
    let connection = open_reader(path)?;
    let Some(fields) = read_materialized_session_fields(&connection, session_id)? else {
        return Ok(None);
    };
    let transcript = read_materialized_transcript(&connection, session_id, Some(transcript_limit))?;
    let total_items = connection.query_row(
        "SELECT COUNT(*) FROM materialized_transcript_items WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, usize>(0),
    )?;
    let window = ProjectionWindow {
        omitted_items: total_items.saturating_sub(transcript.len()),
        provisional_title: first_materialized_user_message(&connection, session_id)?
            .and_then(|(_, text)| crate::hel_state::provisional_session_title(&text)),
        latest_turn_start_position: last_materialized_turn_start(&connection, session_id)?,
    };
    let materialized = MaterializedSession {
        session_id: session_id.to_owned(),
        applied_event_ordinal: fields.applied_event_ordinal,
        applied_event_digest: fields.applied_event_digest,
        last_activity_at_ms: fields.last_activity_at_ms,
        execution: fields.execution,
        session_title: fields.session_title,
        configuration: fields.configuration,
        transcript,
        queued_prompts: read_materialized_queued_prompts(&connection, session_id)?,
        pending_elicitations: fields.pending_elicitations,
    };
    materialized.validate()?;
    Ok(Some((materialized, window)))
}

/// Read only the projection's event frontier. Deciding whether a stored
/// projection already matches an archive costs one row this way, instead of
/// deserializing every transcript item to compare two integers.
pub fn materialized_event_frontier(session_id: &str) -> Result<Option<(u64, String)>> {
    materialized_event_frontier_from(&database_path(), session_id)
}

fn materialized_event_frontier_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    Ok(open_reader(path)?
        .query_row(
            "SELECT applied_event_ordinal, applied_event_digest
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?)
}

/// Replace a session's durable prompt queue without touching its transcript or
/// event frontier. Resume uses this when it keeps the stored projection but
/// still has to drop the queue the archive carried.
pub fn replace_materialized_queued_prompts(
    session_id: &str,
    queued_prompts: &[MaterializedQueuedPrompt],
) -> Result<()> {
    let session_id = session_id.to_owned();
    let queued_prompts = queued_prompts.to_vec();
    submit_database_write("replace_materialized_queued_prompts", move |_| {
        replace_materialized_queued_prompts_in(&database_path(), &session_id, &queued_prompts)
    })
}

fn replace_materialized_queued_prompts_in(
    path: &Path,
    session_id: &str,
    queued_prompts: &[MaterializedQueuedPrompt],
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !session_exists(&tx, session_id)? {
        bail!("unknown session {session_id}");
    }
    replace_materialized_queue(&tx, session_id, queued_prompts)?;
    tx.commit()?;
    Ok(())
}

/// Load only the durable prompt queues without deserializing transcript rows.
/// Dashboard startup uses this path so work is proportional to queued prompts,
/// not to the complete retained conversation history.
pub fn load_materialized_queued_prompts() -> Result<BTreeMap<String, Vec<MaterializedQueuedPrompt>>>
{
    load_materialized_queued_prompts_from(&database_path())
}

fn load_materialized_queued_prompts_from(
    path: &Path,
) -> Result<BTreeMap<String, Vec<MaterializedQueuedPrompt>>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT session_id, command_id, kind_json, content_json, queued_at_ms
         FROM materialized_queued_prompts
         ORDER BY session_id, ordinal",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut queues = BTreeMap::<String, Vec<MaterializedQueuedPrompt>>::new();
    for row in rows {
        let (session_id, command_id, kind_json, content_json, queued_at_ms) = row?;
        let content = serde_json::from_str(&content_json).with_context(|| {
            format!("parse materialized queued prompt for session {session_id}")
        })?;
        let kind = serde_json::from_str(&kind_json).with_context(|| {
            format!("parse materialized queue entry kind for session {session_id}")
        })?;
        queues
            .entry(session_id)
            .or_default()
            .push(MaterializedQueuedPrompt {
                command_id,
                kind,
                content,
                queued_at_ms,
            });
    }
    Ok(queues)
}

fn load_materialized_session_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<MaterializedSession>> {
    let connection = open_reader(path)?;
    load_materialized_session_with(&connection, session_id)
}

fn load_materialized_session_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<MaterializedSession>> {
    let Some(fields) = read_materialized_session_fields(connection, session_id)? else {
        return Ok(None);
    };
    let materialized = MaterializedSession {
        session_id: session_id.to_owned(),
        applied_event_ordinal: fields.applied_event_ordinal,
        applied_event_digest: fields.applied_event_digest,
        last_activity_at_ms: fields.last_activity_at_ms,
        execution: fields.execution,
        session_title: fields.session_title,
        configuration: fields.configuration,
        transcript: read_materialized_transcript(connection, session_id, None)?,
        queued_prompts: read_materialized_queued_prompts(connection, session_id)?,
        pending_elicitations: fields.pending_elicitations,
    };
    materialized.validate()?;
    Ok(Some(materialized))
}

/// Everything a projection holds apart from its transcript and its queue.
struct MaterializedSessionFields {
    applied_event_ordinal: u64,
    applied_event_digest: String,
    last_activity_at_ms: Option<i64>,
    execution: MaterializedExecutionState,
    session_title: Option<String>,
    configuration: BTreeMap<String, serde_json::Value>,
    pending_elicitations: Vec<crate::hel_elicitation::ElicitationRequest>,
}

fn read_materialized_session_fields(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<MaterializedSessionFields>> {
    let row = connection
        .query_row(
            "SELECT applied_event_ordinal, applied_event_digest, last_activity_at_ms,
                    execution_state, running_started_at_ms, session_title, configuration_json,
                    pending_elicitations_json
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        applied_event_ordinal,
        applied_event_digest,
        last_activity_at_ms,
        execution,
        running_started_at_ms,
        session_title,
        configuration_json,
        pending_elicitations_json,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(MaterializedSessionFields {
        applied_event_ordinal,
        applied_event_digest,
        last_activity_at_ms,
        execution: parse_materialized_execution(&execution, running_started_at_ms)?,
        session_title,
        configuration: serde_json::from_str(&configuration_json).with_context(|| {
            format!("parse materialized configuration for session {session_id}")
        })?,
        pending_elicitations: serde_json::from_str(&pending_elicitations_json)
            .with_context(|| format!("parse pending elicitations for session {session_id}"))?,
    }))
}

/// Read a session's transcript, oldest first. `limit` reads only that many
/// items from the end, walking the `materialized_transcript_position` index
/// backwards so the read costs the rows it returns.
fn read_materialized_transcript(
    connection: &Connection,
    session_id: &str,
    limit: Option<usize>,
) -> Result<Vec<Arc<TranscriptItem>>> {
    let mut statement = connection.prepare(match limit {
        Some(_) => {
            "SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
             ORDER BY position DESC, stable_id DESC
             LIMIT ?2"
        }
        None => {
            "SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
             ORDER BY position, stable_id"
        }
    })?;
    let read = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, Option<u64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
        ))
    };
    let rows = match limit {
        Some(limit) => statement
            .query_map(params![session_id, limit as i64], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => statement
            .query_map([session_id], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    let mut transcript = rows
        .into_iter()
        .map(
            |(
                stable_id,
                position,
                latest_content_event_ordinal,
                created_at_ms,
                last_changed_at_ms,
                body_json,
            )| {
                Ok(Arc::new(TranscriptItem {
                    stable_id,
                    position,
                    latest_content_event_ordinal,
                    created_at_ms,
                    last_changed_at_ms,
                    body: serde_json::from_str(&body_json).with_context(|| {
                        format!("parse materialized transcript body for session {session_id}")
                    })?,
                }))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    if limit.is_some() {
        // The bounded query walks the index backwards to bound what it reads;
        // every caller wants the transcript in the order it was written.
        transcript.reverse();
    }
    Ok(transcript)
}

fn read_materialized_queued_prompts(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<MaterializedQueuedPrompt>> {
    let mut statement = connection.prepare(
        "SELECT command_id, kind_json, content_json, queued_at_ms
         FROM materialized_queued_prompts
         WHERE session_id = ?1
         ORDER BY ordinal",
    )?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(command_id, kind_json, content_json, queued_at_ms)| {
            Ok(MaterializedQueuedPrompt {
                command_id,
                kind: serde_json::from_str(&kind_json).with_context(|| {
                    format!("parse materialized queue entry kind for session {session_id}")
                })?,
                content: serde_json::from_str(&content_json).with_context(|| {
                    format!("parse materialized queued prompt for session {session_id}")
                })?,
                queued_at_ms,
            })
        })
        .collect()
}

/// Replace a complete projection, primarily when seeding a restored
/// checkpoint. Operational `SessionRecord` metadata and read receipts are not
/// modified.
pub fn save_materialized_session(materialized: &MaterializedSession) -> Result<()> {
    let materialized = materialized.clone();
    submit_database_write("save_materialized_session", move |_| {
        save_materialized_session_to(&database_path(), &materialized)
    })
}

fn save_materialized_session_to(path: &Path, materialized: &MaterializedSession) -> Result<()> {
    materialized.validate()?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !session_exists(&tx, &materialized.session_id)? {
        bail!("unknown session {}", materialized.session_id);
    }
    write_materialized_session(&tx, materialized)?;
    tx.commit()?;
    Ok(())
}

/// One relay page being applied inside a single write transaction. The relay
/// retains everything past the last acknowledgement, so a page that fails
/// part-way rolls back to the previous durable frontier and is simply
/// redelivered. Only a committed page may be acknowledged.
pub struct ProjectionPage<'a> {
    session_id: &'a str,
    transaction: Transaction<'a>,
    applied_ordinal: u64,
    applied_digest: String,
    dirty: bool,
    pending: MaterializedSessionMutation,
    pending_transcript: BTreeMap<String, PendingTranscriptMutation>,
}

struct PendingTranscriptMutation {
    final_mutation: TranscriptMutation,
    remove_before_upsert: bool,
}

impl ProjectionPage<'_> {
    /// Apply the projection effects of the next relay event to the open page.
    /// The event must continue the chain the page has reached so far, which is
    /// the persisted frontier plus every event already applied to this page.
    pub fn apply(
        &mut self,
        event_ordinal: u64,
        previous_event_digest: &str,
        event_digest: &str,
        mutation: &MaterializedSessionMutation,
    ) -> Result<ProjectionApplyOutcome> {
        if event_ordinal == 0 {
            bail!("relay event ordinal must be positive");
        }
        // A v2 event carries no chain link (empty previous digest). Its
        // continuity to the projection frontier is proven by ordinal
        // contiguity plus the attach cursor the controller validated against
        // the worker, not by an in-record back-reference; divergence is caught
        // there, before any event is applied.
        let chained = !previous_event_digest.is_empty();
        if chained {
            validate_relay_event_digest(previous_event_digest, "previous relay event digest")?;
        }
        validate_relay_event_frontier(event_ordinal, event_digest, "relay event frontier")?;
        let session_id = self.session_id;
        let applied = self.applied_ordinal;
        if event_ordinal < applied {
            return Ok(ProjectionApplyOutcome::AlreadyApplied);
        }
        if event_ordinal == applied {
            if event_digest != self.applied_digest {
                bail!(
                    "relay event digest mismatch for session {session_id} at ordinal {event_ordinal}: projection has {}, received {event_digest}",
                    self.applied_digest
                );
            }
            return Ok(ProjectionApplyOutcome::AlreadyApplied);
        }
        let expected = applied
            .checked_add(1)
            .context("materialized event ordinal overflow")?;
        if event_ordinal != expected {
            bail!(
                "relay event gap for session {session_id}: expected ordinal {expected}, received {event_ordinal}"
            );
        }
        if chained && previous_event_digest != self.applied_digest {
            bail!(
                "relay event chain diverged for session {session_id} before ordinal {event_ordinal}: projection has {}, event follows {previous_event_digest}",
                self.applied_digest
            );
        }

        if let Some(activity_at_ms) = mutation.last_activity_at_ms {
            self.pending.last_activity_at_ms = Some(
                self.pending
                    .last_activity_at_ms
                    .map_or(activity_at_ms, |existing| existing.max(activity_at_ms)),
            );
        }
        if let Some(execution) = mutation.execution {
            self.pending.execution = Some(execution);
        }
        if let Some(title) = &mutation.session_title {
            if title.as_ref().is_some_and(|title| title.trim().is_empty()) {
                bail!("materialized session title cannot be empty");
            }
            self.pending.session_title = Some(title.clone());
        }
        if let Some(configuration) = &mutation.configuration {
            self.pending.configuration = Some(configuration.clone());
        }
        for item_mutation in &mutation.transcript {
            match item_mutation {
                TranscriptMutation::Upsert(item) => {
                    item.validate(event_ordinal)?;
                    let stable_id = item.stable_id.clone();
                    let entry = self.pending_transcript.entry(stable_id).or_insert_with(|| {
                        PendingTranscriptMutation {
                            final_mutation: TranscriptMutation::Upsert(item.clone()),
                            remove_before_upsert: false,
                        }
                    });
                    entry.remove_before_upsert |=
                        matches!(&entry.final_mutation, TranscriptMutation::Remove { .. });
                    entry.final_mutation = TranscriptMutation::Upsert(item.clone());
                }
                TranscriptMutation::Remove { stable_id } => {
                    if stable_id.trim().is_empty() {
                        bail!("cannot remove a transcript item with an empty stable id");
                    }
                    let removed = TranscriptMutation::Remove {
                        stable_id: stable_id.clone(),
                    };
                    self.pending_transcript
                        .entry(stable_id.clone())
                        .and_modify(|entry| entry.final_mutation = removed.clone())
                        .or_insert(PendingTranscriptMutation {
                            final_mutation: removed,
                            remove_before_upsert: false,
                        });
                }
            }
        }
        if let Some(queued_prompts) = &mutation.queued_prompts {
            self.pending.queued_prompts = Some(queued_prompts.clone());
        }
        if let Some(pending_elicitations) = &mutation.pending_elicitations {
            self.pending.pending_elicitations = Some(pending_elicitations.clone());
        }
        self.applied_ordinal = event_ordinal;
        event_digest.clone_into(&mut self.applied_digest);
        self.dirty = true;
        Ok(ProjectionApplyOutcome::Applied)
    }

    /// Persist the coalesced final state of this page. Intermediate event
    /// frontiers are useful only for chain validation: a page commits or rolls
    /// back as a unit, so writing them individually adds no recovery value.
    fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let tx = &self.transaction;
        let session_id = self.session_id;
        if let Some(execution) = self.pending.execution {
            let (state, started_at_ms) = materialized_execution_columns(execution);
            tx.execute(
                "UPDATE materialized_sessions
                 SET execution_state = ?2, running_started_at_ms = ?3
                 WHERE session_id = ?1",
                params![session_id, state, started_at_ms],
            )?;
        }
        if let Some(title) = &self.pending.session_title {
            tx.execute(
                "UPDATE materialized_sessions SET session_title = ?2 WHERE session_id = ?1",
                params![session_id, title],
            )?;
        }
        if let Some(configuration) = &self.pending.configuration {
            tx.execute(
                "UPDATE materialized_sessions SET configuration_json = ?2 WHERE session_id = ?1",
                params![session_id, serde_json::to_string(configuration)?],
            )?;
        }
        for pending in self.pending_transcript.values() {
            match &pending.final_mutation {
                TranscriptMutation::Upsert(item) => {
                    // A remove followed by an upsert deliberately starts a new
                    // item identity. Preserve that boundary even though other
                    // repeated updates are coalesced to one write.
                    if pending.remove_before_upsert {
                        tx.execute(
                            "DELETE FROM materialized_transcript_items
                             WHERE session_id = ?1 AND stable_id = ?2",
                            params![session_id, item.stable_id],
                        )?;
                    }
                    upsert_transcript_item(tx, session_id, item)?;
                }
                TranscriptMutation::Remove { stable_id } => {
                    tx.execute(
                        "DELETE FROM materialized_transcript_items
                         WHERE session_id = ?1 AND stable_id = ?2",
                        params![session_id, stable_id],
                    )?;
                }
            }
        }
        if let Some(queued_prompts) = &self.pending.queued_prompts {
            replace_materialized_queue(tx, session_id, queued_prompts)?;
        }
        if let Some(pending_elicitations) = &self.pending.pending_elicitations {
            tx.execute(
                "UPDATE materialized_sessions
                 SET pending_elicitations_json = ?2 WHERE session_id = ?1",
                params![session_id, serde_json::to_string(pending_elicitations)?],
            )?;
        }
        tx.execute(
            "UPDATE materialized_sessions
             SET last_activity_at_ms = CASE
                     WHEN ?2 IS NULL THEN last_activity_at_ms
                     WHEN last_activity_at_ms IS NULL OR last_activity_at_ms < ?2 THEN ?2
                     ELSE last_activity_at_ms
                 END,
                 applied_event_ordinal = ?3,
                 applied_event_digest = ?4
             WHERE session_id = ?1",
            params![
                session_id,
                self.pending.last_activity_at_ms,
                self.applied_ordinal,
                self.applied_digest,
            ],
        )?;
        Ok(())
    }
}

/// Apply one relay page in a single transaction. `fill` feeds the page's
/// events through [`ProjectionPage::apply`]; the projection changes and the
/// event frontier commit together only when `fill` succeeds, so callers may
/// acknowledge the page's last ordinal to the relay after this returns.
pub fn apply_projection_page<T>(
    session_id: &str,
    fill: impl FnOnce(&mut ProjectionPage<'_>) -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let session_id = session_id.to_owned();
    submit_database_write("apply_projection_page", move |connection| {
        apply_projection_page_with(connection, &session_id, fill)
    })
}

#[cfg(test)]
fn apply_projection_page_to<T>(
    path: &Path,
    session_id: &str,
    fill: impl FnOnce(&mut ProjectionPage<'_>) -> Result<T>,
) -> Result<T> {
    let mut connection = open(path)?;
    apply_projection_page_with(&mut connection, session_id, fill)
}

fn apply_projection_page_with<T>(
    connection: &mut Connection,
    session_id: &str,
    fill: impl FnOnce(&mut ProjectionPage<'_>) -> Result<T>,
) -> Result<T> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (applied_ordinal, applied_digest) = transaction
        .query_row(
            "SELECT applied_event_ordinal, applied_event_digest
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .with_context(|| format!("unknown session {session_id}"))?;
    validate_relay_event_frontier(
        applied_ordinal,
        &applied_digest,
        "persisted relay event frontier",
    )?;
    let mut page = ProjectionPage {
        session_id,
        transaction,
        applied_ordinal,
        applied_digest,
        dirty: false,
        pending: MaterializedSessionMutation::default(),
        pending_transcript: BTreeMap::new(),
    };
    // Dropping the page on failure rolls the whole transaction back, leaving
    // the projection at the frontier the relay last saw acknowledged.
    let filled = fill(&mut page)?;
    page.flush()?;
    page.transaction.commit()?;
    Ok(filled)
}

/// Apply exactly one relay event, as a page of one.
pub fn apply_projection_event(
    session_id: &str,
    event_ordinal: u64,
    previous_event_digest: &str,
    event_digest: &str,
    mutation: &MaterializedSessionMutation,
) -> Result<ProjectionApplyOutcome> {
    let session_id = session_id.to_owned();
    let previous_event_digest = previous_event_digest.to_owned();
    let event_digest = event_digest.to_owned();
    let mutation = mutation.clone();
    submit_database_write("apply_projection_event", move |connection| {
        apply_projection_page_with(connection, &session_id, |page| {
            page.apply(
                event_ordinal,
                &previous_event_digest,
                &event_digest,
                &mutation,
            )
        })
    })
}

#[cfg(test)]
fn apply_projection_event_to(
    path: &Path,
    session_id: &str,
    event_ordinal: u64,
    previous_event_digest: &str,
    event_digest: &str,
    mutation: &MaterializedSessionMutation,
) -> Result<ProjectionApplyOutcome> {
    apply_projection_page_to(path, session_id, |page| {
        page.apply(event_ordinal, previous_event_digest, event_digest, mutation)
    })
}

/// Advance the persisted detach/read receipt monotonically. A receipt cannot
/// acknowledge an event the controller projection has not durably applied.
pub fn advance_viewed_through_event_ordinal(session_id: &str, through: u64) -> Result<u64> {
    let session_id = session_id.to_owned();
    submit_database_write("advance_viewed_through_event_ordinal", move |_| {
        advance_viewed_through_event_ordinal_to(&database_path(), &session_id, through)
    })
}

fn advance_viewed_through_event_ordinal_to(
    path: &Path,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let applied = tx
        .query_row(
            "SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .with_context(|| format!("unknown session {session_id}"))?;
    if through > applied {
        bail!(
            "cannot acknowledge event ordinal {through} for session {session_id}; projection is at {applied}"
        );
    }
    tx.execute(
        "UPDATE sessions
         SET viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?2)
         WHERE session_id = ?1",
        params![session_id, through],
    )?;
    let receipt = tx.query_row(
        "SELECT viewed_through_event_ordinal FROM sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, u64>(0),
    )?;
    tx.commit()?;
    Ok(receipt)
}

/// Overwrite the unsent chat input carried across a detach. Unlike the read
/// receipt this is not monotonic: a draft can shrink, and an empty string
/// clears it.
pub fn set_session_draft_input(session_id: &str, draft: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let draft = draft.to_owned();
    submit_database_write("set_session_draft_input", move |_| {
        set_session_draft_input_at(&database_path(), &session_id, &draft)
    })
}

fn set_session_draft_input_at(path: &Path, session_id: &str, draft: &str) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let updated = tx.execute(
        "UPDATE sessions SET draft_input = ?2 WHERE session_id = ?1",
        params![session_id, draft],
    )?;
    ensure!(updated == 1, "unknown session {session_id}");
    tx.commit()?;
    Ok(())
}

/// Atomically apply the controller's MRU policy for newly used mount sources.
pub fn remember_mount_sources(host: &str, mounts: &[AdditionalMount]) -> Result<()> {
    if mounts.is_empty() {
        return Ok(());
    }
    let host = host.to_owned();
    let sources = mounts
        .iter()
        .map(|mount| mount.source.clone())
        .collect::<Vec<_>>();
    submit_database_write("remember_mount_sources", move |_| {
        remember_sources(&database_path(), &host, sources)
    })
}

/// Replace one host's remembered mount sources with exactly this list, so the
/// dashboard can forget a directory the user no longer wants suggested.
/// What each workspace last chose for a second opinion.
///
/// The selection is remembered so a repeat review does not ask again, and it
/// is workspace scoped because a reviewer that suits one project rarely suits
/// the next. Values are validated against what the harness advertises now
/// before they are used, so a retired profile is harmless here.
pub fn reviewer_defaults() -> Result<crate::hel_second_opinion::ReviewerDefaults> {
    reviewer_defaults_in(&database_path())
}

fn reviewer_defaults_in(path: &Path) -> Result<crate::hel_second_opinion::ReviewerDefaults> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT workspace_id, profile_id, model, effort FROM second_opinion_defaults
         ORDER BY workspace_id, profile_id, model",
    )?;
    let mut defaults = crate::hel_second_opinion::ReviewerDefaults::default();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let workspace_id: String = row.get(0)?;
        let profile_id: String = row.get(1)?;
        let model: String = row.get(2)?;
        let effort: String = row.get(3)?;
        defaults.restore(&workspace_id, &profile_id, &model, &effort);
    }
    Ok(defaults)
}

/// Record one confirmed selection.
pub fn remember_reviewer_selection(
    workspace_id: &str,
    selection: &crate::hel_second_opinion::ReviewerSelection,
) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    let selection = selection.clone();
    submit_database_write("remember_reviewer_selection", move |_| {
        remember_reviewer_selection_in(&database_path(), &workspace_id, &selection)
    })
}

fn remember_reviewer_selection_in(
    path: &Path,
    workspace_id: &str,
    selection: &crate::hel_second_opinion::ReviewerSelection,
) -> Result<()> {
    ensure!(
        !workspace_id.trim().is_empty(),
        "second-opinion defaults need a workspace"
    );
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (profile_id, model, effort) = selection.stored_values();
    // One profile is the workspace's reviewer at a time, so the rows for the
    // others stop being the remembered choice rather than accumulating.
    tx.execute(
        "DELETE FROM second_opinion_defaults WHERE workspace_id = ?1 AND profile_id <> ?2",
        params![workspace_id, profile_id],
    )?;
    tx.execute(
        "INSERT INTO second_opinion_defaults(workspace_id, profile_id, model, effort)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id, profile_id, model) DO UPDATE SET effort = excluded.effort",
        params![workspace_id, profile_id, model, effort],
    )?;
    tx.commit()?;
    Ok(())
}

/// A second-opinion review that was still open when the UI last stopped.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredReview {
    pub workflow: crate::hel_second_opinion::ReviewWorkflow,
    /// Reviewer lifetime this review belongs to. It is bumped when native
    /// continuity is lost, so a resumed session starts a new conversation
    /// rather than pretending to reload one that is gone.
    pub generation: u64,
    /// The primary's transcript frontier when the context request went out.
    pub context_baseline: u64,
    /// Whether the reviewer's native session is known to be gone.
    pub native_lost: bool,
    /// What the controller has read of the reviewer's conversation. The
    /// reviewer's own journal is the source, but it dies with the target, so
    /// this copy is what keeps a finished review readable afterwards.
    pub reviewer_transcript: Vec<std::sync::Arc<crate::hel_state::TranscriptItem>>,
}

/// The open review for `session_id`, if the session has one.
pub fn active_review(session_id: &str) -> Result<Option<StoredReview>> {
    active_review_in(&database_path(), session_id)
}

fn active_review_in(path: &Path, session_id: &str) -> Result<Option<StoredReview>> {
    let connection = open_reader(path)?;
    let row = connection
        .query_row(
            "SELECT workflow, generation, context_baseline, native_lost, reviewer_transcript
             FROM second_opinion_reviews WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((workflow, generation, baseline, native_lost, transcript)) = row else {
        return Ok(None);
    };
    Ok(Some(StoredReview {
        workflow: serde_json::from_str(&workflow).context("parse the stored review workflow")?,
        generation: u64::try_from(generation).unwrap_or_default(),
        context_baseline: u64::try_from(baseline).unwrap_or_default(),
        native_lost: native_lost != 0,
        reviewer_transcript: serde_json::from_str(&transcript)
            .context("parse the stored reviewer transcript")?,
    }))
}

/// Records the open review, replacing any earlier one for this session.
pub fn save_active_review(session_id: &str, review: &StoredReview) -> Result<()> {
    let session_id = session_id.to_owned();
    let review = review.clone();
    submit_database_write("save_active_review", move |_| {
        save_active_review_in(&database_path(), &session_id, &review)
    })
}

fn save_active_review_in(path: &Path, session_id: &str, review: &StoredReview) -> Result<()> {
    let connection = open(path)?;
    connection.execute(
        "INSERT INTO second_opinion_reviews(
             session_id, workflow, generation, context_baseline, native_lost,
             reviewer_transcript
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
             workflow = excluded.workflow,
             generation = excluded.generation,
             context_baseline = excluded.context_baseline,
             native_lost = excluded.native_lost,
             reviewer_transcript = excluded.reviewer_transcript",
        params![
            session_id,
            serde_json::to_string(&review.workflow)?,
            i64::try_from(review.generation).unwrap_or(i64::MAX),
            i64::try_from(review.context_baseline).unwrap_or(i64::MAX),
            i64::from(review.native_lost),
            serde_json::to_string(&review.reviewer_transcript)?,
        ],
    )?;
    Ok(())
}

/// Forgets the open review once it has finished.
pub fn clear_active_review(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("clear_active_review", move |_| {
        clear_active_review_in(&database_path(), &session_id)
    })
}

fn clear_active_review_in(path: &Path, session_id: &str) -> Result<()> {
    let connection = open(path)?;
    connection.execute(
        "DELETE FROM second_opinion_reviews WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}

/// How far this session has been reviewed.
///
/// `baselines` are Git tree ids by repository root: the working tree as of the
/// last completed review. They advance only when a review resolves, which is
/// what makes a cancelled review lossless -- the next one covers both turns.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnReviewState {
    pub baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    pub reviewed_through_ordinal: u64,
    /// The last forwarded verdict, which turns the next review into a
    /// verification pass. Cleared once that pass consumes it.
    pub prior_review: Option<crate::hel_review::lanes::PriorReviewContext>,
    /// A review that was running when the daemon stopped. On recovery it is
    /// cleared without advancing the baseline.
    pub active: Option<String>,
}

/// How far `session_id` has been reviewed, or a fresh state when it has never
/// been reviewed.
pub fn turn_review_state(session_id: &str) -> Result<TurnReviewState> {
    turn_review_state_in(&database_path(), session_id)
}

fn turn_review_state_in(path: &Path, session_id: &str) -> Result<TurnReviewState> {
    let connection = open_reader(path)?;
    let row = connection
        .query_row(
            "SELECT baselines, reviewed_through_ordinal, prior_review, active
             FROM turn_review_state WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((baselines, ordinal, prior, active)) = row else {
        return Ok(TurnReviewState::default());
    };
    Ok(TurnReviewState {
        baselines: serde_json::from_str(&baselines).context("parse the stored review baselines")?,
        reviewed_through_ordinal: u64::try_from(ordinal).unwrap_or_default(),
        prior_review: prior
            .map(|prior| serde_json::from_str(&prior))
            .transpose()
            .context("parse the stored prior review")?,
        active,
    })
}

/// Records how far a session has been reviewed.
pub fn save_turn_review_state(session_id: &str, state: &TurnReviewState) -> Result<()> {
    let session_id = session_id.to_owned();
    let state = state.clone();
    submit_database_write("save_turn_review_state", move |_| {
        save_turn_review_state_in(&database_path(), &session_id, &state)
    })
}

fn save_turn_review_state_in(path: &Path, session_id: &str, state: &TurnReviewState) -> Result<()> {
    let connection = open(path)?;
    connection.execute(
        "INSERT INTO turn_review_state(
             session_id, baselines, reviewed_through_ordinal, prior_review, active
         ) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
             baselines = excluded.baselines,
             reviewed_through_ordinal = excluded.reviewed_through_ordinal,
             prior_review = excluded.prior_review,
             active = excluded.active",
        params![
            session_id,
            serde_json::to_string(&state.baselines)?,
            i64::try_from(state.reviewed_through_ordinal).unwrap_or(i64::MAX),
            state
                .prior_review
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            state.active,
        ],
    )?;
    Ok(())
}

/// Clears every session's in-flight review flag.
///
/// A review that was running when the daemon stopped is not resumed: the
/// baseline never advanced, so the next review covers the same change, and half
/// a multi-agent fan-out is not worth rebuilding. Baselines are deliberately
/// left alone, which is what makes the interruption lossless. Returns the
/// sessions whose review was interrupted, so the daemon can say so in each
/// conversation.
pub fn clear_interrupted_turn_reviews() -> Result<Vec<String>> {
    submit_database_write("clear_interrupted_turn_reviews", move |_| {
        clear_interrupted_turn_reviews_in(&database_path())
    })
}

fn clear_interrupted_turn_reviews_in(path: &Path) -> Result<Vec<String>> {
    let connection = open(path)?;
    let interrupted = {
        let mut statement = connection
            .prepare("SELECT session_id FROM turn_review_state WHERE active IS NOT NULL")?;
        let mut rows = statement.query([])?;
        let mut interrupted = Vec::new();
        while let Some(row) = rows.next()? {
            interrupted.push(row.get::<_, String>(0)?);
        }
        interrupted
    };
    connection.execute(
        "UPDATE turn_review_state SET active = NULL WHERE active IS NOT NULL",
        [],
    )?;
    Ok(interrupted)
}

/// Marks this session's reviewer conversation as no longer continuable, and
/// reports the generation a future review must start under.
///
/// Losing the target takes the reviewer's native session with it. The
/// materialized transcript is kept for reference, but the next review is a new
/// conversation, so it runs under a new generation.
pub fn lose_reviewer_continuity(session_id: &str) -> Result<u64> {
    let session_id = session_id.to_owned();
    submit_database_write("lose_reviewer_continuity", move |_| {
        lose_reviewer_continuity_in(&database_path(), &session_id)
    })
}

fn lose_reviewer_continuity_in(path: &Path, session_id: &str) -> Result<u64> {
    let Some(mut review) = active_review_in(path, session_id)? else {
        return Ok(0);
    };
    if review.native_lost {
        return Ok(review.generation);
    }
    review.native_lost = true;
    review.generation = review.generation.saturating_add(1);
    save_active_review_in(path, session_id, &review)?;
    Ok(review.generation)
}

pub fn replace_mount_history(host: &str, sources: &[PathBuf]) -> Result<()> {
    let host = host.to_owned();
    let sources = sources.to_vec();
    submit_database_write("replace_mount_history", move |_| {
        replace_mount_history_in(&database_path(), &host, &sources)
    })
}

fn replace_mount_history_in(path: &Path, host: &str, sources: &[PathBuf]) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    write_mount_history(&tx, host, sources)?;
    tx.commit()?;
    Ok(())
}

fn write_mount_history(tx: &Transaction<'_>, host: &str, sources: &[PathBuf]) -> Result<()> {
    tx.execute("DELETE FROM mount_history WHERE host = ?1", [host])?;
    let mut written = Vec::new();
    for source in sources.iter().take(20) {
        if written.contains(source) {
            continue;
        }
        tx.execute(
            "INSERT INTO mount_history(host, source, ordinal) VALUES (?1, ?2, ?3)",
            params![host, path_to_blob(source), written.len() as i64],
        )?;
        written.push(source.clone());
    }
    Ok(())
}

fn write_host_container_size(
    tx: &Transaction<'_>,
    host: &str,
    size: HostContainerSize,
) -> Result<()> {
    ensure!(!host.trim().is_empty(), "container size host is empty");
    let cpus = i64::try_from(size.cpus).context("container CPU count exceeds SQLite range")?;
    let memory =
        i64::try_from(size.memory_bytes).context("container memory exceeds SQLite range")?;
    ensure!(
        cpus > 0 && memory > 0,
        "container size values must be positive"
    );
    tx.execute(
        "INSERT INTO host_container_sizes(host, cpus, memory_bytes)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(host) DO UPDATE SET cpus = excluded.cpus, memory_bytes = excluded.memory_bytes",
        params![host, cpus, memory],
    )?;
    Ok(())
}

pub fn remember_project_directory(host: &str, directory: &Path) -> Result<()> {
    let host = format!("project:{host}");
    let directory = directory.to_path_buf();
    submit_database_write("remember_project_directory", move |_| {
        remember_sources(&database_path(), &host, std::iter::once(directory))
    })
}

fn remember_sources(
    path: &Path,
    host: &str,
    new_sources: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut sources = {
        let mut statement =
            tx.prepare("SELECT source FROM mount_history WHERE host = ?1 ORDER BY ordinal")?;
        statement
            .query_map([host], |row| Ok(blob_to_path(row.get_ref(0)?.as_blob()?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let additions = new_sources.into_iter().collect::<Vec<_>>();
    for source in additions.iter().rev() {
        sources.retain(|existing| existing != source);
        sources.insert(0, source.clone());
    }
    sources.truncate(20);
    write_mount_history(&tx, host, &sources)?;
    tx.commit()?;
    Ok(())
}

pub fn record_recovery_success(
    session_id: &str,
    native_session_id: &str,
    checkpoint: &CheckpointMetadata,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.to_owned();
    let checkpoint = checkpoint.clone();
    submit_database_write("record_recovery_success", move |_| {
        record_recovery_success_to(
            &database_path(),
            &session_id,
            &native_session_id,
            &checkpoint,
        )
    })
}

fn record_recovery_success_to(
    path: &Path,
    session_id: &str,
    native_session_id: &str,
    checkpoint: &CheckpointMetadata,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET native_session_id = ?2, last_checkpoint_error = NULL
         WHERE session_id = ?1",
        params![session_id, native_session_id],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    tx.execute(
        "INSERT INTO session_checkpoints(
             session_id, archive_path, sha256, created_at, event_frontier
         ) VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(session_id) DO UPDATE SET
             archive_path = excluded.archive_path,
             sha256 = excluded.sha256,
             created_at = excluded.created_at,
             event_frontier = excluded.event_frontier",
        params![
            session_id,
            path_to_blob(&checkpoint.archive_path),
            checkpoint.sha256,
            checkpoint.created_at,
            checkpoint.event_frontier,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn record_recovery_failure(session_id: &str, detail: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    submit_database_write("record_recovery_failure", move |_| {
        record_recovery_failure_to(&database_path(), &session_id, &detail)
    })
}

fn record_recovery_failure_to(path: &Path, session_id: &str, detail: &str) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET last_checkpoint_error = ?2 WHERE session_id = ?1",
        params![session_id, detail],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

pub fn save_state_to(path: &Path, state: &HelState) -> Result<()> {
    state.validate()?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing_contexts = existing_contexts(&tx)?;
    let existing_sessions = {
        let mut statement = tx.prepare("SELECT session_id FROM sessions")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for session_id in existing_sessions {
        if !state.sessions.contains_key(&session_id) {
            tx.execute("DELETE FROM sessions WHERE session_id = ?1", [session_id])?;
        }
    }
    tx.execute("DELETE FROM mount_history", [])?;
    tx.execute("DELETE FROM host_container_sizes", [])?;
    for session in state.sessions.values() {
        if let Some((existing_bundle, existing_workspace)) = existing_contexts.get(&session.id) {
            ensure!(
                existing_bundle == &session.bundle_id,
                "session {} was already associated with bundle {}, not {}",
                session.id,
                existing_bundle,
                session.bundle_id
            );
            ensure!(
                existing_workspace == &session.workspace_id,
                "session {} was already associated with workspace {}, not {}",
                session.id,
                existing_workspace,
                session.workspace_id
            );
        }
        insert_session(&tx, session)?;
    }
    for (host, sources) in &state.mount_history {
        for (ordinal, source) in sources.iter().enumerate() {
            tx.execute(
                "INSERT INTO mount_history(host, source, ordinal) VALUES (?1, ?2, ?3)",
                params![host, path_to_blob(source), ordinal as i64],
            )?;
        }
    }
    for (host, size) in &state.container_sizes {
        write_host_container_size(&tx, host, *size)?;
    }
    tx.commit()?;
    Ok(())
}

fn existing_contexts(tx: &Transaction<'_>) -> Result<BTreeMap<String, (String, String)>> {
    let mut statement =
        tx.prepare("SELECT session_id, bundle_id, workspace_id FROM session_contexts")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

fn session_exists(tx: &Transaction<'_>, session_id: &str) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM sessions WHERE session_id = ?1",
            [session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn write_materialized_session(
    tx: &Transaction<'_>,
    materialized: &MaterializedSession,
) -> Result<()> {
    let (execution, running_started_at_ms) = materialized_execution_columns(materialized.execution);
    tx.execute(
        "INSERT INTO materialized_sessions(
             session_id, applied_event_ordinal, applied_event_digest, execution_state,
             running_started_at_ms, session_title, configuration_json, last_activity_at_ms,
             pending_elicitations_json
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(session_id) DO UPDATE SET
             applied_event_ordinal = excluded.applied_event_ordinal,
             applied_event_digest = excluded.applied_event_digest,
             execution_state = excluded.execution_state,
             running_started_at_ms = excluded.running_started_at_ms,
             session_title = excluded.session_title,
             configuration_json = excluded.configuration_json,
             last_activity_at_ms = excluded.last_activity_at_ms,
             pending_elicitations_json = excluded.pending_elicitations_json",
        params![
            materialized.session_id,
            materialized.applied_event_ordinal,
            materialized.applied_event_digest,
            execution,
            running_started_at_ms,
            materialized.session_title,
            serde_json::to_string(&materialized.configuration)?,
            materialized.last_activity_at_ms,
            serde_json::to_string(&materialized.pending_elicitations)?,
        ],
    )?;
    tx.execute(
        "DELETE FROM materialized_transcript_items WHERE session_id = ?1",
        [materialized.session_id.as_str()],
    )?;
    for item in &materialized.transcript {
        upsert_transcript_item(tx, &materialized.session_id, item)?;
    }
    replace_materialized_queue(tx, &materialized.session_id, &materialized.queued_prompts)?;
    Ok(())
}

fn upsert_transcript_item(
    tx: &Transaction<'_>,
    session_id: &str,
    item: &TranscriptItem,
) -> Result<()> {
    let existing = tx
        .query_row(
            "SELECT position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms
             FROM materialized_transcript_items
             WHERE session_id = ?1 AND stable_id = ?2",
            params![session_id, item.stable_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, Option<u64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms)) =
        existing
    {
        if position != item.position || created_at_ms != item.created_at_ms {
            return Err(ProjectionIntegrityError(format!(
                "transcript item {:?} changed immutable identity fields",
                item.stable_id
            ))
            .into());
        }
        if item.last_changed_at_ms < last_changed_at_ms {
            return Err(ProjectionIntegrityError(format!(
                "transcript item {:?} moved its changed timestamp backwards",
                item.stable_id
            ))
            .into());
        }
        if latest_content_event_ordinal.is_some_and(|existing| {
            item.latest_content_event_ordinal
                .is_none_or(|next| next < existing)
        }) {
            return Err(ProjectionIntegrityError(format!(
                "transcript item {:?} moved its latest content ordinal backwards",
                item.stable_id
            ))
            .into());
        }
        tx.execute(
            "UPDATE materialized_transcript_items
             SET latest_content_event_ordinal = ?3, last_changed_at_ms = ?4, body_json = ?5
             WHERE session_id = ?1 AND stable_id = ?2",
            params![
                session_id,
                item.stable_id,
                item.latest_content_event_ordinal,
                item.last_changed_at_ms,
                serde_json::to_string(&item.body)?,
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO materialized_transcript_items(
                 session_id, stable_id, position, latest_content_event_ordinal,
                 created_at_ms, last_changed_at_ms, body_json
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                session_id,
                item.stable_id,
                item.position,
                item.latest_content_event_ordinal,
                item.created_at_ms,
                item.last_changed_at_ms,
                serde_json::to_string(&item.body)?,
            ],
        )?;
    }
    Ok(())
}

fn replace_materialized_queue(
    tx: &Transaction<'_>,
    session_id: &str,
    queued_prompts: &[MaterializedQueuedPrompt],
) -> Result<()> {
    let mut command_ids = BTreeSet::new();
    for prompt in queued_prompts {
        if prompt.command_id.trim().is_empty() {
            bail!("materialized prompt queue has an empty command id");
        }
        if !command_ids.insert(prompt.command_id.as_str()) {
            bail!(
                "materialized prompt queue contains duplicate command {:?}",
                prompt.command_id
            );
        }
    }
    tx.execute(
        "DELETE FROM materialized_queued_prompts WHERE session_id = ?1",
        [session_id],
    )?;
    for (ordinal, prompt) in queued_prompts.iter().enumerate() {
        tx.execute(
            "INSERT INTO materialized_queued_prompts(
                 session_id, ordinal, command_id, kind_json, content_json, queued_at_ms
             ) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                session_id,
                ordinal as i64,
                prompt.command_id,
                serde_json::to_string(&prompt.kind)?,
                serde_json::to_string(&prompt.content)?,
                prompt.queued_at_ms,
            ],
        )?;
    }
    Ok(())
}

fn materialized_execution_columns(
    execution: MaterializedExecutionState,
) -> (&'static str, Option<i64>) {
    match execution {
        MaterializedExecutionState::Idle => ("idle", None),
        MaterializedExecutionState::Running { started_at_ms } => ("running", Some(started_at_ms)),
        MaterializedExecutionState::Closing => ("closing", None),
        MaterializedExecutionState::Closed => ("closed", None),
    }
}

fn parse_materialized_execution(
    execution: &str,
    running_started_at_ms: Option<i64>,
) -> Result<MaterializedExecutionState> {
    match (execution, running_started_at_ms) {
        ("idle", None) => Ok(MaterializedExecutionState::Idle),
        ("running", Some(started_at_ms)) => {
            Ok(MaterializedExecutionState::Running { started_at_ms })
        }
        ("closing", None) => Ok(MaterializedExecutionState::Closing),
        ("closed", None) => Ok(MaterializedExecutionState::Closed),
        _ => bail!("invalid materialized execution state {execution:?}"),
    }
}

/// Write every field of a session, including the ones other writers own.
/// Only a flow that authors the whole record — creation, import, resume, or
/// orphan adoption — may use this.
fn insert_session(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO session_contexts(session_id, bundle_id, created_at, workspace_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO NOTHING",
        params![
            session.id,
            session.bundle_id,
            session.created_at,
            session.workspace_id
        ],
    )?;
    let (stored_bundle, stored_workspace): (String, String) = tx.query_row(
        "SELECT bundle_id, workspace_id FROM session_contexts WHERE session_id = ?1",
        [session.id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        stored_bundle == session.bundle_id,
        "session {} belongs to bundle {}, not {}",
        session.id,
        stored_bundle,
        session.bundle_id
    );
    ensure!(
        stored_workspace == session.workspace_id,
        "session {} belongs to workspace {}, not {}",
        session.id,
        stored_workspace,
        session.workspace_id
    );
    tx.execute(
        "INSERT INTO sessions(
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             viewed_through_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree,
             container_cpus, container_memory, archived
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(session_id) DO UPDATE SET
             title = excluded.title,
             harness_kind = excluded.harness_kind,
             last_profile = excluded.last_profile,
             target_template_id = excluded.target_template_id,
             state = excluded.state,
             native_session_id = excluded.native_session_id,
             acp_session_title = excluded.acp_session_title,
             session_title_override = excluded.session_title_override,
             updated_at = excluded.updated_at,
             viewed_through_event_ordinal = max(
                 sessions.viewed_through_event_ordinal,
                 excluded.viewed_through_event_ordinal
             ),
             last_error = excluded.last_error,
             resource_allocation = excluded.resource_allocation,
             last_checkpoint_error = excluded.last_checkpoint_error,
             project_directory = excluded.project_directory,
             managed_worktree = excluded.managed_worktree,
             container_cpus = excluded.container_cpus,
             container_memory = excluded.container_memory,
             archived = excluded.archived",
        params![
            session.id,
            session.title,
            session.harness_kind.id(),
            session.last_profile,
            session.target_template_id,
            session_state_name(session.state),
            session.native_session_id,
            session.acp_session_title,
            session.session_title_override,
            session.updated_at,
            session.viewed_through_event_ordinal,
            session.last_error,
            session
                .resource_allocation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            session.last_checkpoint_error,
            session
                .project_directory
                .as_ref()
                .map(|path| path_to_blob(path)),
            session
                .managed_worktree
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            session.container_cpus,
            session.container_memory,
            session.archived,
        ],
    )?;
    tx.execute(
        "INSERT INTO materialized_sessions(session_id) VALUES (?1)
         ON CONFLICT(session_id) DO NOTHING",
        [session.id.as_str()],
    )?;
    replace_targets(tx, session)?;
    tx.execute(
        "DELETE FROM session_mounts WHERE session_id = ?1",
        [session.id.as_str()],
    )?;
    for (ordinal, mount) in session.additional_mounts.iter().enumerate() {
        tx.execute(
            "INSERT INTO session_mounts(session_id, ordinal, source, destination, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.id,
                ordinal as i64,
                path_to_blob(&mount.source),
                path_to_blob(&mount.destination),
                mount.read_only
            ],
        )?;
    }
    replace_checkpoint(tx, session)?;
    Ok(())
}

/// Update the columns a lifecycle transition owns, plus the target locator
/// that provisioning and teardown maintain with them. The row must exist:
/// a transition never resurrects a session another writer deleted.
fn update_lifecycle_fields(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    let changed = tx.execute(
        // The detach ordinal only ever moves forward, so a transition that
        // started before a detach receipt cannot rewind it.
        "UPDATE sessions
         SET title = ?2,
             harness_kind = ?3,
             last_profile = ?4,
             target_template_id = ?5,
             state = ?6,
             updated_at = ?7,
             viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?8),
             last_error = ?9,
             resource_allocation = ?10,
             last_checkpoint_error = ?11,
             project_directory = ?12,
             managed_worktree = ?13
         WHERE session_id = ?1",
        params![
            session.id,
            session.title,
            session.harness_kind.id(),
            session.last_profile,
            session.target_template_id,
            session_state_name(session.state),
            session.updated_at,
            session.viewed_through_event_ordinal,
            session.last_error,
            session
                .resource_allocation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            session.last_checkpoint_error,
            session
                .project_directory
                .as_ref()
                .map(|path| path_to_blob(path)),
            session
                .managed_worktree
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    if changed != 1 {
        bail!("unknown session {}", session.id);
    }
    replace_targets(tx, session)
}

fn replace_targets(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    tx.execute(
        "DELETE FROM session_targets WHERE session_id = ?1",
        [session.id.as_str()],
    )?;
    if let Some(target) = &session.target {
        insert_target(tx, &session.id, target)?;
    }
    Ok(())
}

fn replace_checkpoint(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    tx.execute(
        "DELETE FROM session_checkpoints WHERE session_id = ?1",
        [session.id.as_str()],
    )?;
    if let Some(checkpoint) = &session.checkpoint {
        tx.execute(
            "INSERT INTO session_checkpoints(session_id, archive_path, sha256, created_at, event_frontier)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.id,
                path_to_blob(&checkpoint.archive_path),
                checkpoint.sha256,
                checkpoint.created_at,
                checkpoint.event_frontier,
            ],
        )?;
    }
    Ok(())
}

fn insert_target(tx: &Transaction<'_>, session_id: &str, target: &TargetLocator) -> Result<()> {
    let (kind, host, resource, address, workspace, worker_id) = match target {
        TargetLocator::LocalBare { worker_root } => (
            "local-bare",
            None,
            None,
            None,
            Some(path_to_blob(worker_root)),
            None,
        ),
        TargetLocator::LocalPodman { container_id } => (
            "local-podman",
            None,
            Some(container_id.as_str()),
            None,
            None,
            None,
        ),
        TargetLocator::LocalDocker { container_id } => (
            "local-docker",
            None,
            Some(container_id.as_str()),
            None,
            None,
            None,
        ),
        TargetLocator::AppleContainer { container_id } => (
            "apple-container",
            None,
            Some(container_id.as_str()),
            None,
            None,
            None,
        ),
        TargetLocator::AwsEc2 {
            instance_id,
            address,
        } => (
            "aws-ec2",
            None,
            Some(instance_id.as_str()),
            address.as_deref(),
            None,
            None,
        ),
        TargetLocator::SshBare {
            host,
            workspace,
            worker_id,
        } => (
            "ssh-bare",
            Some(host.as_str()),
            None,
            None,
            Some(path_to_blob(workspace)),
            worker_id.as_deref(),
        ),
        TargetLocator::SshPodman { host, container_id } => (
            "ssh-podman",
            Some(host.as_str()),
            Some(container_id.as_str()),
            None,
            None,
            None,
        ),
    };
    tx.execute(
        "INSERT INTO session_targets(session_id, kind, host, resource_id, address, workspace, worker_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![session_id, kind, host, resource, address, workspace, worker_id],
    )?;
    Ok(())
}

fn load_targets(connection: &Connection, state: &mut HelState) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT session_id, kind, host, resource_id, address, workspace, worker_id
         FROM session_targets",
    )?;
    let rows = statement.query_map([], |row| {
        let session_id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let host: Option<String> = row.get(2)?;
        let resource: Option<String> = row.get(3)?;
        let address: Option<String> = row.get(4)?;
        let workspace = row.get_ref(5)?.blob_or_null()?.map(blob_to_path);
        let worker_id: Option<String> = row.get(6)?;
        let target = match kind.as_str() {
            "local-bare" => TargetLocator::LocalBare {
                worker_root: workspace.unwrap(),
            },
            "local-podman" => TargetLocator::LocalPodman {
                container_id: resource.unwrap(),
            },
            "local-docker" => TargetLocator::LocalDocker {
                container_id: resource.unwrap(),
            },
            "apple-container" => TargetLocator::AppleContainer {
                container_id: resource.unwrap(),
            },
            "aws-ec2" => TargetLocator::AwsEc2 {
                instance_id: resource.unwrap(),
                address,
            },
            "ssh-bare" => TargetLocator::SshBare {
                host: host.unwrap(),
                workspace: workspace.unwrap(),
                worker_id,
            },
            "ssh-podman" => TargetLocator::SshPodman {
                host: host.unwrap(),
                container_id: resource.unwrap(),
            },
            _ => unreachable!("target kind constrained by schema"),
        };
        Ok((session_id, target))
    })?;
    for row in rows {
        let (session_id, target) = row?;
        state.sessions.get_mut(&session_id).unwrap().target = Some(target);
    }
    Ok(())
}

fn load_mounts(connection: &Connection, state: &mut HelState) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT session_id, source, destination, read_only
         FROM session_mounts ORDER BY session_id, ordinal",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            AdditionalMount {
                source: blob_to_path(row.get_ref(1)?.as_blob()?),
                destination: blob_to_path(row.get_ref(2)?.as_blob()?),
                read_only: row.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (session_id, mount) = row?;
        state
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .additional_mounts
            .push(mount);
    }
    Ok(())
}

fn load_checkpoints(connection: &Connection, state: &mut HelState) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT session_id, archive_path, sha256, created_at, event_frontier FROM session_checkpoints",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            CheckpointMetadata {
                archive_path: blob_to_path(row.get_ref(1)?.as_blob()?),
                sha256: row.get(2)?,
                created_at: row.get(3)?,
                event_frontier: row.get(4)?,
            },
        ))
    })?;
    for row in rows {
        let (session_id, checkpoint) = row?;
        state.sessions.get_mut(&session_id).unwrap().checkpoint = Some(checkpoint);
    }
    Ok(())
}

/// Re-associate a session with another project bundle.
///
/// A session's bundle is otherwise fixed, because prompt history is grouped by
/// it. Resume calls this when it converts a session between its raw and bundle
/// representations: the project is the same, so its history follows it, and
/// only the name Hel files it under changes.
pub fn rebind_session_bundle(session_id: &str, bundle_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    submit_database_write("rebind_session_bundle", move |_| {
        rebind_session_bundle_to(&database_path(), &session_id, &bundle_id)
    })
}

fn rebind_session_bundle_to(path: &Path, session_id: &str, bundle_id: &str) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE session_contexts SET bundle_id = ?2 WHERE session_id = ?1",
        params![session_id, bundle_id],
    )?;
    if changed == 0 {
        tx.execute(
            "INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)",
            params![session_id, bundle_id, Utc::now().to_rfc3339()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn record_prompt(
    session_id: &str,
    bundle_id: &str,
    event_ordinal: u64,
    submitted_at: Option<&str>,
    text: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    let submitted_at = submitted_at.map(str::to_owned);
    let text = text.to_owned();
    submit_database_write("record_prompt", move |_| {
        record_prompt_to(
            &database_path(),
            &session_id,
            &bundle_id,
            event_ordinal,
            submitted_at.as_deref(),
            &text,
        )
    })
}

fn record_prompt_to(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    event_ordinal: u64,
    submitted_at: Option<&str>,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, bundle_id, submitted_at.unwrap_or("unknown")],
    )?;
    let actual_bundle: String = tx.query_row(
        "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    if actual_bundle != bundle_id {
        bail!("session {session_id} belongs to bundle {actual_bundle}, not {bundle_id}");
    }
    tx.execute(
        "INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id, event_ordinal) DO NOTHING",
        params![
            session_id,
            event_ordinal,
            submitted_at
                .map(str::to_owned)
                .unwrap_or_else(|| Utc::now().to_rfc3339()),
            text,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn search_prompts(
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    search_prompts_from(&database_path(), session_id, bundle_id, scope, query)
}

/// What a bounded prompt search found, and whether it stopped early.
///
/// The flag is not decoration. Without it a caller cannot tell twenty matches
/// from the first twenty of many, and will present a partial answer as a whole
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedPromptHistory {
    pub entries: Vec<PromptHistoryEntry>,
    pub truncated: bool,
}

/// Search prompt history, stopping at `limit` matches.
///
/// `search_prompts` pages through the whole table and stops only when a page
/// comes back short, which is fine for a terminal running it against a local
/// database and is not something an HTTP route may reach.
pub fn search_prompts_bounded(
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    limit: usize,
) -> Result<BoundedPromptHistory> {
    search_prompts_bounded_from(&database_path(), session_id, bundle_id, scope, query, limit)
}

fn search_prompts_bounded_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
    limit: usize,
) -> Result<BoundedPromptHistory> {
    const PAGE_SIZE: usize = 256;
    /// How many rows the search may read before giving up on finding more.
    /// A query that matches nothing must not walk an unbounded history.
    const MAX_ROWS_SCANNED: usize = 4_096;

    let connection = open_reader(path)?;
    let query = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    let mut before = i64::MAX;
    let mut scanned = 0;
    let mut truncated = false;
    loop {
        let page = match scope {
            HistoryScope::Project => query_history_page(
                &connection,
                "SELECT h.history_id, h.session_id, h.text
                 FROM prompt_history h JOIN session_contexts c USING(session_id)
                 WHERE c.bundle_id = ?1 AND h.history_id < ?2
                 ORDER BY h.history_id DESC LIMIT ?3",
                params![bundle_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::Session => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE session_id = ?1 AND history_id < ?2
                 ORDER BY history_id DESC LIMIT ?3",
                params![session_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::All => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE history_id < ?1 ORDER BY history_id DESC LIMIT ?2",
                params![before, PAGE_SIZE as i64],
            )?,
        };
        let page_len = page.len();
        for entry in page {
            before = entry.id;
            scanned += 1;
            if entry.text.to_lowercase().contains(&query) && seen.insert(entry.text.clone()) {
                if matches.len() == limit {
                    truncated = true;
                    break;
                }
                matches.push(entry);
            }
        }
        if truncated || page_len < PAGE_SIZE {
            break;
        }
        if scanned >= MAX_ROWS_SCANNED {
            truncated = true;
            break;
        }
    }
    Ok(BoundedPromptHistory {
        entries: matches,
        truncated,
    })
}

fn search_prompts_from(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
    scope: HistoryScope,
    query: &str,
) -> Result<Vec<PromptHistoryEntry>> {
    const PAGE_SIZE: usize = 256;
    let connection = open_reader(path)?;
    let query = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    let mut before = i64::MAX;
    loop {
        let page = match scope {
            HistoryScope::Project => query_history_page(
                &connection,
                "SELECT h.history_id, h.session_id, h.text
                 FROM prompt_history h JOIN session_contexts c USING(session_id)
                 WHERE c.bundle_id = ?1 AND h.history_id < ?2
                 ORDER BY h.history_id DESC LIMIT ?3",
                params![bundle_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::Session => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE session_id = ?1 AND history_id < ?2
                 ORDER BY history_id DESC LIMIT ?3",
                params![session_id, before, PAGE_SIZE as i64],
            )?,
            HistoryScope::All => query_history_page(
                &connection,
                "SELECT history_id, session_id, text FROM prompt_history
                 WHERE history_id < ?1 ORDER BY history_id DESC LIMIT ?2",
                params![before, PAGE_SIZE as i64],
            )?,
        };
        let page_len = page.len();
        for entry in page {
            before = entry.id;
            if entry.text.to_lowercase().contains(&query) && seen.insert(entry.text.clone()) {
                matches.push(entry);
            }
        }
        if page_len < PAGE_SIZE {
            break;
        }
    }
    Ok(matches)
}

fn query_history_page(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<PromptHistoryEntry>> {
    let mut statement = connection.prepare_cached(sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok(PromptHistoryEntry {
            id: row.get(0)?,
            session_id: row.get(1)?,
            text: row.get(2)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn migrate_legacy_state() -> Result<()> {
    let legacy = crate::hel_state::state_path();
    let database = database_path();
    migrate_legacy_state_from(&legacy, &database)
}

fn migrate_legacy_state_from(legacy: &Path, database: &Path) -> Result<()> {
    if !legacy.exists() {
        return Ok(());
    }
    // The database may exist after an interrupted migration. The legacy file
    // remains the authority until the import commits and this file is renamed.
    let mut state = HelState::load_json_from(legacy)?;
    // Legacy worker sequence numbers are not relay event ordinals. Carrying
    // them across the new compatibility floor could mark unseen relay events
    // as read.
    for session in state.sessions.values_mut() {
        session.viewed_through_event_ordinal = 0;
    }
    save_state_to(database, &state)?;
    let migrated = legacy.with_file_name("state.json.migrated-v1");
    fs::rename(legacy, &migrated)
        .with_context(|| format!("retain migrated Mjolnir state as {}", migrated.display()))?;
    Ok(())
}

fn session_state_name(value: SessionState) -> &'static str {
    match value {
        SessionState::Provisioning => "provisioning",
        SessionState::Running => "running",
        SessionState::Disconnected => "disconnected",
        SessionState::Checkpointing => "checkpointing",
        SessionState::Closing => "closing",
        SessionState::Destroying => "destroying",
        SessionState::Stopped => "stopped",
        SessionState::Lost => "lost",
        SessionState::Error => "error",
        SessionState::DestroyedWithDataLoss => "destroyed-with-data-loss",
    }
}
fn parse_session_state(value: &str) -> SessionState {
    match value {
        "provisioning" => SessionState::Provisioning,
        "running" => SessionState::Running,
        "disconnected" => SessionState::Disconnected,
        "checkpointing" => SessionState::Checkpointing,
        "closing" => SessionState::Closing,
        "destroying" => SessionState::Destroying,
        // Rows written before the verb was renamed still say "archived".
        "stopped" | "archived" => SessionState::Stopped,
        "lost" => SessionState::Lost,
        "error" => SessionState::Error,
        "destroyed-with-data-loss" => SessionState::DestroyedWithDataLoss,
        _ => unreachable!(),
    }
}

#[cfg(unix)]
fn path_to_blob(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(unix)]
fn blob_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}
#[cfg(windows)]
fn path_to_blob(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
#[cfg(windows)]
fn blob_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let wide = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect::<Vec<_>>();
    PathBuf::from(std::ffi::OsString::from_wide(&wide))
}

trait ValueRefExt<'a> {
    fn blob_or_null(self) -> rusqlite::Result<Option<&'a [u8]>>;
}
impl<'a> ValueRefExt<'a> for rusqlite::types::ValueRef<'a> {
    fn blob_or_null(self) -> rusqlite::Result<Option<&'a [u8]>> {
        match self {
            rusqlite::types::ValueRef::Null => Ok(None),
            value => Ok(Some(value.as_blob()?)),
        }
    }
}

#[cfg(test)]
mod tests;
