//! Durable, non-blocking diagnostics for controller-facing Mjolnir processes.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const RETAINED_LOGS: usize = 10;
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) struct ControllerLog {
    _writer_guard: ReliableWorkerGuard,
}

/// A non-blocking logger with reliable delivery to its writer thread. The
/// `tracing_appender` default is bounded and lossy, which can discard a fatal
/// error when a controller emits a burst of diagnostics. An unbounded standard
/// channel keeps the UI call site free of filesystem I/O while retaining every
/// record until the worker writes it or the process exits.
#[derive(Clone)]
struct ReliableWriter {
    sender: Sender<LogMessage>,
}

enum LogMessage {
    Line(Vec<u8>),
    Flush(SyncSender<std::io::Result<()>>),
}

struct ReliableWorkerGuard {
    sender: Option<Sender<LogMessage>>,
}

impl Write for ReliableWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = bytes.len();
        self.sender
            .send(LogMessage::Line(bytes.to_vec()))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "log worker stopped")
            })?;
        Ok(length)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let (flushed_tx, flushed_rx) = mpsc::sync_channel(1);
        self.sender
            .send(LogMessage::Flush(flushed_tx))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "log worker stopped")
            })?;
        flushed_rx
            .recv_timeout(LOG_FLUSH_TIMEOUT)
            .map_err(|error| {
                let kind = match error {
                    mpsc::RecvTimeoutError::Timeout => std::io::ErrorKind::TimedOut,
                    mpsc::RecvTimeoutError::Disconnected => std::io::ErrorKind::BrokenPipe,
                };
                std::io::Error::new(kind, format!("log worker did not flush: {error}"))
            })?
    }
}

impl<'a> MakeWriter<'a> for ReliableWriter {
    type Writer = ReliableWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Drop for ReliableWorkerGuard {
    fn drop(&mut self) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        let mut writer = ReliableWriter { sender };
        if let Err(error) = writer.flush() {
            eprintln!("Mjolnir log writer failed to drain before exit: {error}");
        }
        // The global tracing subscriber owns another sender for the rest of
        // the process. Keep its worker valid after this flush so detached
        // runtime work cannot turn a late diagnostic into terminal output.
        // The operating system stops the detached writer at process exit.
    }
}

fn reliable_non_blocking(file: File) -> Result<(ReliableWriter, ReliableWorkerGuard)> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("mj-log-writer".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_log_messages(file, receiver);
            }));
            if let Err(error) = result {
                eprintln!("Mjolnir log writer panicked: {error:?}");
            }
        })
        .context("spawn Mjolnir log writer")?;
    let writer = ReliableWriter {
        sender: sender.clone(),
    };
    let guard = ReliableWorkerGuard {
        sender: Some(sender),
    };
    Ok((writer, guard))
}

fn write_log_messages(mut file: File, receiver: mpsc::Receiver<LogMessage>) {
    while let Ok(message) = receiver.recv() {
        match message {
            LogMessage::Line(line) => {
                if let Err(error) = file.write_all(&line) {
                    eprintln!("Mjolnir log writer failed: {error}");
                    break;
                }
            }
            LogMessage::Flush(flushed) => {
                let result = file.flush();
                let failed = result.is_err();
                if flushed.send(result).is_err() {
                    eprintln!("Mjolnir log writer flush completion could not be reported");
                }
                if failed {
                    break;
                }
            }
        }
    }
    if let Err(error) = file.flush() {
        eprintln!("Mjolnir log writer failed to flush: {error}");
    }
}

impl ControllerLog {
    pub(crate) fn start(command: &'static str) -> Result<Self> {
        let directory = hel::hel_config::data_dir().join("logs");
        fs::create_dir_all(&directory)
            .with_context(|| format!("create Mjolnir log directory {}", directory.display()))?;
        prune_logs(&directory, RETAINED_LOGS.saturating_sub(1))?;

        let path = directory.join(log_filename());
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("create Mjolnir log {}", path.display()))?;
        let (writer, writer_guard) = reliable_non_blocking(file)?;
        let (filter, filter_error) = env_filter("info");
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(writer)
            .try_init()
            .map_err(|error| anyhow::anyhow!("install Mjolnir log subscriber: {error}"))?;

        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            process_id = std::process::id(),
            command,
            log = %path.display(),
            "Mjolnir started"
        );
        if let Some(error) = filter_error {
            tracing::warn!(%error, "ignored invalid RUST_LOG filter");
        }
        Ok(Self {
            _writer_guard: writer_guard,
        })
    }
}

pub(crate) fn start_stderr() -> Result<()> {
    let (filter, filter_error) = env_filter("warn");
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("install Mjolnir stderr subscriber: {error}"))?;
    if let Some(error) = filter_error {
        tracing::warn!(%error, "ignored invalid RUST_LOG filter");
    }
    Ok(())
}

fn env_filter(default: &str) -> (EnvFilter, Option<String>) {
    match std::env::var("RUST_LOG") {
        Ok(value) => match EnvFilter::try_new(value) {
            Ok(filter) => (filter, None),
            Err(error) => (EnvFilter::new(default), Some(error.to_string())),
        },
        Err(std::env::VarError::NotPresent) => (EnvFilter::new(default), None),
        Err(error @ std::env::VarError::NotUnicode(_)) => {
            (EnvFilter::new(default), Some(error.to_string()))
        }
    }
}

fn log_filename() -> String {
    format!(
        "mj-{}-{}.log",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        std::process::id()
    )
}

fn prune_logs(directory: &Path, retain: usize) -> Result<()> {
    let mut logs = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read Mjolnir log directory {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", directory.display()))?;
        if let Some(path) = {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            (name.starts_with("mj-") && name.ends_with(".log")).then_some(entry.path())
        } {
            logs.push(path);
        }
    }
    logs.sort_unstable();
    let remove = logs.len().saturating_sub(retain);
    for path in logs.into_iter().take(remove) {
        fs::remove_file(&path)
            .with_context(|| format!("remove expired Mjolnir log {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliable_writer_queues_every_line_without_dropping_a_burst() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("burst.log");
        let file = File::create(&path).unwrap();
        let (mut writer, guard) = reliable_non_blocking(file).unwrap();
        for line in 0..10_000 {
            writer
                .write_all(format!("error {line}\n").as_bytes())
                .unwrap();
        }
        drop(writer);
        drop(guard);

        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents.lines().count(), 10_000);
        assert!(contents.ends_with("error 9999\n"));
    }

    #[test]
    fn reliable_writer_remains_valid_after_guard_flushes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("late.log");
        let file = File::create(&path).unwrap();
        let (mut writer, guard) = reliable_non_blocking(file).unwrap();
        writer.write_all(b"before guard drop\n").unwrap();

        drop(guard);
        writer.write_all(b"after guard drop\n").unwrap();
        writer.flush().unwrap();

        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents, "before guard drop\nafter guard drop\n");
    }

    #[test]
    fn prune_logs_keeps_newest_managed_logs_and_unrelated_files() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "mj-20260824T000000.000Z-1.log",
            "mj-20260825T000000.000Z-2.log",
            "mj-20260826T000000.000Z-3.log",
            "hel-20260823T000000.000Z-4.log",
            "notes.log",
        ] {
            fs::write(directory.path().join(name), name).unwrap();
        }

        prune_logs(directory.path(), 2).unwrap();

        assert!(
            !directory
                .path()
                .join("mj-20260824T000000.000Z-1.log")
                .exists()
        );
        assert!(
            directory
                .path()
                .join("mj-20260825T000000.000Z-2.log")
                .exists()
        );
        assert!(
            directory
                .path()
                .join("mj-20260826T000000.000Z-3.log")
                .exists()
        );
        assert!(
            directory
                .path()
                .join("hel-20260823T000000.000Z-4.log")
                .exists(),
            "legacy Hel logs are ignored rather than treated as Mjolnir state"
        );
        assert!(directory.path().join("notes.log").exists());
    }
}
