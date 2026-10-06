//! Durable, non-blocking diagnostics for controller-facing Mjolnir processes.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const RETAINED_LOGS: usize = 10;
/// Logs started within this window are kept beyond [`RETAINED_LOGS`], up to
/// [`RECENT_LOG_LIMIT`] of a kind. Scripts and agents start a CLI command every
/// second or so; a newest-ten window then lost the log of a command that ran a
/// minute ago, such as the one whose slow daemon upgrade needed explaining.
const RECENT_LOG_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(1);
const RECENT_LOG_LIMIT: usize = 1000;
/// The timestamp format in log filenames. It sorts as text in time order.
const LOG_TIMESTAMP: &str = "%Y%m%dT%H%M%S%.3fZ";
/// Give a queued burst time to drain under contention without allowing a stuck
/// writer to hold process shutdown indefinitely.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// The kind of process writing a Mjolnir log, recorded in the log filename so
/// retention can keep the newest logs of each kind separately. A short-lived
/// `mj sessions` or `mj wait` invocation must never crowd the long-running
/// daemon's own log out of a single shared newest-N window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProcessKind {
    /// The persistent per-user daemon (`mj daemon-run`).
    Daemon,
    /// An interactive dashboard or other terminal-owning surface (no
    /// subcommand, `go`, `workspaces`, `app`).
    Tui,
    /// A one-shot CLI invocation (everything else).
    Cli,
}

impl ProcessKind {
    fn label(&self) -> &'static str {
        match self {
            ProcessKind::Daemon => "daemon",
            ProcessKind::Tui => "tui",
            ProcessKind::Cli => "cli",
        }
    }
}

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
    pub(crate) fn start(command: &'static str, kind: ProcessKind) -> Result<Self> {
        let data_dir = mj_core::config::data_dir();
        let directory = data_dir.join("logs");
        fs::create_dir_all(&directory)
            .with_context(|| format!("create Mjolnir log directory {}", directory.display()))?;
        prune_logs(
            &directory,
            RETAINED_LOGS.saturating_sub(1),
            current_daemon_pid(&data_dir),
        )?;

        let path = directory.join(log_filename(kind));
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

fn log_filename(kind: ProcessKind) -> String {
    format!(
        "mj-{}-{}-{}.log",
        kind.label(),
        Utc::now().format(LOG_TIMESTAMP),
        std::process::id()
    )
}

/// Reads the PID of the daemon currently recorded in `<data_dir>/daemon.json`,
/// if any. A missing or unparseable file means "no protected PID" rather than
/// an error: absence just means no daemon has started under this data
/// directory yet, or its metadata predates this process.
fn current_daemon_pid(data_dir: &Path) -> Option<u32> {
    let path = data_dir.join("daemon.json");
    let body = fs::read(path).ok()?;
    let metadata: mj_client::daemon::DaemonMetadata = serde_json::from_slice(&body).ok()?;
    Some(metadata.pid)
}

/// Parses a managed Mjolnir log filename (already known to start with `mj-`
/// and end with `.log`) into its process kind and PID. The current format is
/// `mj-<kind>-<timestamp>-<pid>.log`; the pre-existing format
/// `mj-<timestamp>-<pid>.log` (no kind, three `-`-separated parts) is treated
/// as kind `cli` so its retention matches today's short CLI invocations. The
/// PID is `None` when it cannot be parsed, which keeps the file eligible for
/// pruning as before this change.
fn parse_log_filename(name: &str) -> Option<(ProcessKind, Option<u32>)> {
    let stem = name.strip_prefix("mj-")?.strip_suffix(".log")?;
    let parts: Vec<&str> = stem.split('-').collect();
    match parts.as_slice() {
        [kind, _timestamp, pid] => {
            let kind = match *kind {
                "daemon" => ProcessKind::Daemon,
                "tui" => ProcessKind::Tui,
                "cli" => ProcessKind::Cli,
                _ => return None,
            };
            Some((kind, pid.parse::<u32>().ok()))
        }
        [_timestamp, pid] => Some((ProcessKind::Cli, pid.parse::<u32>().ok())),
        _ => None,
    }
}

/// The start timestamp in a managed log filename, in either format.
fn log_timestamp(name: &str) -> Option<&str> {
    let stem = name.strip_prefix("mj-")?.strip_suffix(".log")?;
    let parts: Vec<&str> = stem.split('-').collect();
    match parts.as_slice() {
        [_, timestamp, _] | [timestamp, _] => Some(timestamp),
        _ => None,
    }
}

pub(crate) fn daemon_log_path(data_dir: &Path, pid: u32) -> Result<Option<PathBuf>> {
    let directory = data_dir.join("logs");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("find the launched daemon's diagnostic log"),
    };
    let mut latest = None;
    for entry in entries {
        let entry = entry.context("read daemon diagnostic log entry")?;
        if entry.file_name().to_str().and_then(parse_log_filename)
            == Some((ProcessKind::Daemon, Some(pid)))
        {
            let path = entry.path();
            if latest.as_ref().is_none_or(|previous| path > *previous) {
                latest = Some(path);
            }
        }
    }
    Ok(latest)
}

fn prune_logs(directory: &Path, retain: usize, protected_pid: Option<u32>) -> Result<()> {
    let recent = (Utc::now() - RECENT_LOG_WINDOW)
        .format(LOG_TIMESTAMP)
        .to_string();
    prune_logs_since(directory, retain, protected_pid, &recent)
}

/// Prune each kind to its newest `retain` logs, keeping as well those whose
/// filename timestamp is at or after `recent` (up to [`RECENT_LOG_LIMIT`]) and
/// those of processes still running, which may still be writing.
fn prune_logs_since(
    directory: &Path,
    retain: usize,
    protected_pid: Option<u32>,
    recent: &str,
) -> Result<()> {
    let mut logs_by_kind: HashMap<ProcessKind, Vec<PathBuf>> = HashMap::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read Mjolnir log directory {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", directory.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("mj-") && name.ends_with(".log")) {
            continue;
        }
        // A file whose kind cannot be parsed falls back to `cli`, the same
        // group a malformed-but-managed name would have landed in before
        // filenames carried a kind.
        let (kind, pid) = parse_log_filename(name).unwrap_or((ProcessKind::Cli, None));
        if pid.is_some() && pid == protected_pid {
            // Never prune the current daemon's own log, regardless of kind
            // or age; it may still be writing to it.
            continue;
        }
        if pid.is_some_and(mj_client::daemon::process_is_alive) {
            // A process that is still running may still be writing its log.
            continue;
        }
        logs_by_kind.entry(kind).or_default().push(entry.path());
    }
    for mut logs in logs_by_kind.into_values() {
        logs.sort_unstable();
        let started_recently = logs
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(log_timestamp)
                    .is_some_and(|started| started >= recent)
            })
            .count();
        let keep = retain.max(started_recently.min(RECENT_LOG_LIMIT));
        let remove = logs.len().saturating_sub(keep);
        for path in logs.into_iter().take(remove) {
            remove_expired_log(&path)?;
        }
    }
    Ok(())
}

fn remove_expired_log(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        // Another Mjolnir process may have pruned this file after the scan.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove expired Mjolnir log {}", path.display()))
        }
    }
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

    /// An arbitrary PID used for logs that are never the protected daemon
    /// PID in a given test.
    const OTHER_PID: u32 = 999_999_999;

    // Hard-won: 1d6a27cb6a: A short-lived CLI could prune the older log still being written by the live daemon; the test protects the daemon PID log beyond the normal retention count.
    #[test]
    fn prune_logs_never_removes_the_current_daemons_log() {
        let data_dir = tempfile::tempdir().unwrap();
        let daemon_pid = 4_242_424u32;
        let metadata = serde_json::json!({
            "protocol_version": 1,
            "pid": daemon_pid,
            "address": "127.0.0.1:0",
            "token": "t",
            "started_at": "2026-01-01T00:00:00Z",
            "build_version": "0.0.0",
        });
        fs::write(data_dir.path().join("daemon.json"), metadata.to_string()).unwrap();
        let logs_dir = data_dir.path().join("logs");
        fs::create_dir_all(&logs_dir).unwrap();

        // The current daemon's own log is the oldest by filename, so a naive
        // newest-N prune would delete it first.
        let protected_name = format!("mj-daemon-20260101T000000.000Z-{daemon_pid}.log");
        fs::write(logs_dir.join(&protected_name), "daemon").unwrap();
        let other_names: Vec<String> = (0..12)
            .map(|index| format!("mj-daemon-202602{index:02}T000000.000Z-{OTHER_PID}.log"))
            .collect();
        for name in &other_names {
            fs::write(logs_dir.join(name), "daemon").unwrap();
        }

        let resolved_pid = current_daemon_pid(data_dir.path());
        assert_eq!(resolved_pid, Some(daemon_pid));
        prune_logs(&logs_dir, RETAINED_LOGS - 1, resolved_pid).unwrap();

        assert!(
            logs_dir.join(&protected_name).exists(),
            "the current daemon's own log must survive pruning even when it is the oldest"
        );
        let remaining_others = other_names
            .iter()
            .filter(|name| logs_dir.join(name).exists())
            .count();
        assert_eq!(
            remaining_others,
            RETAINED_LOGS - 1,
            "other daemon-kind logs beyond the retained window are still pruned"
        );
    }

    /// Under a script that runs a command every second, the newest ten logs
    /// cover ten seconds. A command still running, or one that ran within the
    /// hour, keeps its log.
    // Hard-won: 4ce833574f: A live replacement report disappeared from logs within seconds; this checks recent files and active-process logs survive pruning.
    #[test]
    fn prune_logs_keeps_recent_logs_and_logs_of_running_processes() {
        let directory = tempfile::tempdir().unwrap();
        let running = format!("mj-cli-20260101T000000.000Z-{}.log", std::process::id());
        fs::write(directory.path().join(&running), "running").unwrap();
        for index in 0..30 {
            let name = format!("mj-cli-20260105T0000{index:02}.000Z-{OTHER_PID}.log");
            fs::write(directory.path().join(&name), "recent").unwrap();
        }
        let old = format!("mj-cli-20260102T000000.000Z-{OTHER_PID}.log");
        fs::write(directory.path().join(&old), "old").unwrap();

        prune_logs_since(directory.path(), 2, None, "20260105T000000.000Z").unwrap();

        assert!(directory.path().join(&running).exists());
        assert!(!directory.path().join(&old).exists());
        let recent = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.contains("20260105"))
            })
            .count();
        assert_eq!(recent, 30, "every log started in the window is kept");
    }

    // Hard-won: 1d6a27cb: A burst of short-lived CLI logs could prune the older live daemon log.
    #[test]
    fn prune_logs_retains_per_kind() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..12 {
            let name = format!("mj-cli-202601{index:02}T000000.000Z-{OTHER_PID}.log");
            fs::write(directory.path().join(&name), "cli").unwrap();
        }
        let daemon_name = format!("mj-daemon-20260101T000000.000Z-{OTHER_PID}.log");
        fs::write(directory.path().join(&daemon_name), "daemon").unwrap();

        prune_logs(directory.path(), RETAINED_LOGS - 1, None).unwrap();

        assert!(
            directory.path().join(&daemon_name).exists(),
            "the single daemon log must survive even though 12 cli logs are newer by name"
        );
        let remaining_cli = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("mj-cli-"))
            })
            .count();
        assert_eq!(remaining_cli, RETAINED_LOGS - 1);
    }

    // Hard-won: 1d6a27cb: Legacy CLI log names could still crowd out the live daemon log after filename versioning.
    #[test]
    fn prune_logs_treats_legacy_filenames_as_cli() {
        let directory = tempfile::tempdir().unwrap();
        let legacy_names: Vec<String> = (0..12)
            .map(|index| format!("mj-202601{index:02}T000000.000Z-{OTHER_PID}.log"))
            .collect();
        for name in &legacy_names {
            fs::write(directory.path().join(name), "legacy").unwrap();
        }
        let daemon_name = format!("mj-daemon-20260101T000000.000Z-{OTHER_PID}.log");
        fs::write(directory.path().join(&daemon_name), "daemon").unwrap();

        prune_logs(directory.path(), RETAINED_LOGS - 1, None).unwrap();

        assert!(
            directory.path().join(&daemon_name).exists(),
            "legacy cli-shaped logs must not crowd out the daemon's retained window"
        );
        let remaining_legacy = legacy_names
            .iter()
            .filter(|name| directory.path().join(name).exists())
            .count();
        assert_eq!(
            remaining_legacy,
            RETAINED_LOGS - 1,
            "legacy filenames are pruned in the same group as mj-cli-* files"
        );
    }

    #[test]
    fn remove_expired_log_ignores_a_candidate_removed_by_another_process() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj-cli-20260824T000000.000Z-1.log");
        fs::write(&path, "expired").unwrap();

        // This is the state observed when another process wins the race
        // between prune_logs' directory scan and its remove_file call.
        fs::remove_file(&path).unwrap();

        remove_expired_log(&path).unwrap();
    }

    #[test]
    fn remove_expired_log_reports_non_missing_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj-20260824T000000.000Z-1.log");
        fs::create_dir(&path).unwrap();

        let error = remove_expired_log(&path).unwrap_err();

        assert!(error.to_string().contains("remove expired Mjolnir log"));
    }
}
