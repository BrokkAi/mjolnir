use super::*;

pub(super) const DATABASE_WRITE_QUEUE_CAPACITY: usize = 256;

/// Writes that take at least this long are logged with their label.
const SLOW_WRITE: std::time::Duration = std::time::Duration::from_millis(250);

/// A queued write, handed either the writer's connection or the reason it
/// must not be used. The job -- not the lane -- decides what a refusal means
/// to its caller.
pub(super) type DatabaseWriteJob = Box<dyn FnOnce(Result<&mut Connection>) + Send + 'static>;

pub(super) enum DatabaseWriterMessage {
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
    pub(super) id: u64,
    pub(super) sender: SyncSender<DatabaseWriterMessage>,
    space: Arc<tokio::sync::Notify>,
    path: Arc<PathBuf>,
    committed: tokio::sync::watch::Sender<std::result::Result<Arc<CommittedState>, Arc<str>>>,
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
    pub fn committed_state(&self) -> Result<Arc<CommittedState>> {
        self.committed
            .borrow()
            .clone()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    pub fn committed_changes(
        &self,
    ) -> tokio::sync::watch::Receiver<std::result::Result<Arc<CommittedState>, Arc<str>>> {
        self.committed.subscribe()
    }

    fn run<T, F>(
        &self,
        label: &'static str,
        connection: Result<&mut Connection>,
        operation: F,
    ) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let connection = connection?;
        let previous = self.committed_state()?;
        committed::begin_operation(&self.path);
        let result = operation(connection);
        match committed::finish_operation(
            connection,
            &previous,
            &super::state_io::sessions_root_for(&self.path),
        ) {
            Ok(Some(next)) => {
                drop(self.committed.send_replace(Ok(Arc::new(next))));
            }
            Ok(None) => {}
            Err(error) => {
                let detail: Arc<str> = format!("database publication failed after {label}; a commit may have occurred, do not replay this mutation: {error:#}").into();
                drop(self.committed.send_replace(Err(detail.clone())));
                bail!("{detail}");
            }
        }
        result
    }

    pub fn execute<T, F>(&self, label: &'static str, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply_tx, reply_rx) = sync_channel(1);
        let publication = self.clone();
        self.sender
            .send(DatabaseWriterMessage::Run {
                label,
                job: Box::new(move |connection| {
                    let _ = reply_tx.send(publication.run(label, connection, operation));
                }),
            })
            .map_err(|_| {
                anyhow::anyhow!("submit database writer operation {label}: writer stopped")
            })?;
        reply_rx
            .recv()
            .with_context(|| format!("database writer stopped during {label}"))?
    }

    /// Dropping this future before enqueue accepts nothing. After it returns,
    /// the caller owns a durable obligation to finish publication and its reply.
    pub async fn enqueue<T, G, F>(
        &self,
        label: &'static str,
        guard: G,
        operation: F,
    ) -> Result<AcceptedDatabaseWrite<T, G>>
    where
        T: Send + 'static,
        G: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let publication = self.clone();
        let mut message = DatabaseWriterMessage::Run {
            label,
            job: Box::new(move |connection| {
                let result = publication.run(label, connection, operation);
                let _ = reply_tx.send((result, guard));
            }),
        };
        loop {
            // Register before trying: dequeue may happen between Full and await.
            let notified = self.space.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.sender.try_send(message) {
                Ok(()) => return Ok(AcceptedDatabaseWrite { reply: reply_rx }),
                Err(std::sync::mpsc::TrySendError::Full(returned)) => message = returned,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    bail!("submit database writer operation {label}: writer stopped")
                }
            }
            notified.await;
        }
    }
}

pub struct AcceptedDatabaseWrite<T, G> {
    reply: tokio::sync::oneshot::Receiver<(Result<T>, G)>,
}

impl<T, G> AcceptedDatabaseWrite<T, G> {
    /// Once accepted this wait must not be cancelled: the guard covers the
    /// committed publication and the originating connection's reply attempt.
    pub async fn finish(self) -> Result<(Result<T>, G)> {
        self.reply
            .await
            .context("database writer stopped during accepted write")
    }
}

pub(super) async fn enqueue_database_write<T, G, F>(
    label: &'static str,
    guard: G,
    operation: F,
) -> Result<AcceptedDatabaseWrite<T, G>>
where
    T: Send + 'static,
    G: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
{
    let writer = database_writer_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .context("database writer is not available")?;
    writer.enqueue(label, guard, operation).await
}

/// Owns the daemon's writer thread and persistent SQLite connection.
///
/// The owner is deliberately not cloneable. Dropping it removes the global
/// submission handle, drains accepted work in FIFO order, and joins the
/// thread before releasing the connection.
pub struct DatabaseWriterOwner {
    pub(super) writer: DatabaseWriter,
    pub(super) thread: Option<JoinHandle<()>>,
    pub(super) stopped: Receiver<Result<()>>,
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

    pub(super) fn shutdown_inner(&mut self) -> Result<()> {
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

pub(super) fn database_writer_slot() -> &'static Mutex<Option<DatabaseWriter>> {
    static WRITER: OnceLock<Mutex<Option<DatabaseWriter>>> = OnceLock::new();
    WRITER.get_or_init(|| Mutex::new(None))
}

/// Whether this process holds the database writer. Only the daemon does, so
/// work that merely keeps a daemon-owned record current, and that other
/// processes also run, asks this instead of failing the write there.
pub fn database_writer_installed() -> bool {
    database_writer_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}

/// The daemon's current durable records. Client/offline readers have no writer
/// and use explicit store reads; a failed daemon projection remains an error.
pub fn committed_state() -> Result<Option<Arc<CommittedState>>> {
    let writer = database_writer_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    writer.map(|writer| writer.committed_state()).transpose()
}

pub fn subscribe_committed_state()
-> Result<tokio::sync::watch::Receiver<std::result::Result<Arc<CommittedState>, Arc<str>>>> {
    let writer = database_writer_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .context("database writer is not installed")?;
    Ok(writer.committed_changes())
}

pub(super) fn clear_database_writer(id: u64) {
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
///
/// This fixture is compiled unconditionally and hidden from the documentation
/// because the controller crate's tests need it and a `#[cfg(test)]` item is
/// invisible to another crate. It is a thin wrapper over
/// [`start_database_writer`], so nothing test-only leaks into the library.
#[doc(hidden)]
#[must_use = "the writer stops when this owner is dropped"]
pub fn install_isolated_test_writer() -> DatabaseWriterOwner {
    start_database_writer().expect("install the writer for an isolated test child")
}

pub fn start_database_writer() -> Result<DatabaseWriterOwner> {
    start_database_writer_at(&database_path(), true)
}

pub(super) fn start_database_writer_at(
    path: &Path,
    install_globally: bool,
) -> Result<DatabaseWriterOwner> {
    static NEXT_WRITER_ID: AtomicU64 = AtomicU64::new(1);

    let mut connection = schema::open_writer(path)?;
    // 0x10002 is SQLite's recommendation for long-lived connections. Bit 0x10
    // applies a temporary analysis_limit, keeping the first run bounded on a
    // large store that has never been analyzed. Refresh statistics once at
    // every daemon start by design; there is no periodic run.
    let optimize_started = std::time::Instant::now();
    match connection.execute_batch("PRAGMA optimize=0x10012") {
        Ok(()) => tracing::info!(
            elapsed_ms = optimize_started.elapsed().as_millis() as u64,
            "optimized SQLite query planner statistics"
        ),
        Err(error) => tracing::warn!(
            %error,
            elapsed_ms = optimize_started.elapsed().as_millis() as u64,
            "failed to optimize SQLite query planner statistics; continuing startup"
        ),
    }
    if let Err(error) = super::tool_details::sweep_orphaned_tool_details(&connection, path) {
        tracing::warn!(error = %format!("{error:#}"), "could not finish orphaned tool detail sweep");
    }
    let mut observed_revision = schema::read_schema_state(&connection)?.revision;
    let initial =
        CommittedState::bootstrap(&mut connection, &super::state_io::sessions_root_for(path))?;
    let (committed, _) = tokio::sync::watch::channel(Ok(Arc::new(initial)));
    let path = path.to_owned();
    let (sender, receiver) = sync_channel(DATABASE_WRITE_QUEUE_CAPACITY);
    let (stopped_tx, stopped) = sync_channel(1);
    let id = NEXT_WRITER_ID.fetch_add(1, Ordering::Relaxed);
    let writer = DatabaseWriter {
        id,
        sender,
        space: Arc::new(tokio::sync::Notify::new()),
        path: Arc::new(path.clone()),
        committed,
    };
    if install_globally {
        let mut installed = database_writer_slot()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(installed.is_none(), "database writer is already running");
        *installed = Some(writer.clone());
    }
    let publication = writer.clone();
    let thread = match thread::Builder::new()
        .name("hel-database-writer".to_owned())
        .spawn(move || {
            let mut connection = connection;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match receiver.recv() {
                        Ok(DatabaseWriterMessage::Run { label, job }) => {
                            publication.space.notify_waiters();
                            tracing::trace!(operation = label, "running database writer operation");
                            let job_started = std::time::Instant::now();
                            // Recheck compatibility even after startup, and
                            // remember forward progress to detect rollback.
                            match writer_schema_state(
                                &path,
                                &connection,
                                label,
                                &mut observed_revision,
                            ) {
                                Ok(()) => job(Ok(&mut connection)),
                                Err(error) => job(Err(error)),
                            }
                            // Every write waits behind the one before it, so a
                            // slow one delays all of them, the daemon's exit
                            // included.
                            let took = job_started.elapsed();
                            if took >= SLOW_WRITE {
                                tracing::info!(
                                    operation = label,
                                    duration_ms = took.as_millis(),
                                    "slow database write"
                                );
                            }
                            if let Err(error) = publication.committed_state() {
                                break Err(error);
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
            drop(receiver);
            publication.space.notify_waiters();
            if let Err(error) = &result {
                drop(
                    publication
                        .committed
                        .send_replace(Err(format!("database writer stopped: {error:#}").into())),
                );
            }
            if result.is_ok() {
                clear_database_writer(id);
            }
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

/// Refuse incompatible stores, rollback, and unreadable metadata before a job.
pub(super) fn writer_schema_state(
    path: &Path,
    connection: &Connection,
    label: &'static str,
    observed_revision: &mut i64,
) -> Result<()> {
    let result: Result<()> = (|| {
        let state = schema::read_schema_state(connection)?;
        if state.revision < *observed_revision {
            return Err(StoreSchemaMismatch {
                found: state.revision,
                supported: SCHEMA_VERSION,
                reason: StoreSchemaMismatchReason::Rollback {
                    previous: *observed_revision,
                },
            }
            .into());
        }
        *observed_revision = state.revision;
        state.ensure_supported()
    })();
    if let Err(error) = &result {
        tracing::error!(
            operation = label,
            path = %path.display(),
            error = %error,
            "could not establish store compatibility; refusing the operation"
        );
    }
    result.with_context(|| {
        format!(
            "check database compatibility before {label} at {}",
            path.display()
        )
    })
}

pub(super) fn submit_database_write<T, F>(label: &'static str, operation: F) -> Result<T>
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
