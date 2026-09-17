use super::*;

pub(super) const DATABASE_WRITE_QUEUE_CAPACITY: usize = 256;

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
    pub(super) fn execute<T, F>(&self, label: &'static str, operation: F) -> Result<T>
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
                        Err(error) => Err(error),
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

    let connection = schema::open_writer(path)?;
    let mut observed_revision = schema::read_schema_state(&connection)?.revision;
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
