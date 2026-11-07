//! Diagnostic output must never turn an unavailable disk into a process failure.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

const RECENT_LOG_BYTES: usize = 12 * 1024;

/// One interval of unavailable diagnostic output, owned by its writer.
#[derive(Debug, Clone)]
pub struct DroppedLines {
    pub count: u64,
    pub first_at: String,
    pub last_at: String,
    pub kind: io::ErrorKind,
    pub error: String,
}

struct State<W> {
    writer: W,
    dropped: Option<DroppedLines>,
    recent: VecDeque<String>,
    recent_bytes: usize,
}

/// Serializes lines, failure accounting and a bounded tail at one decision point.
/// Write errors consume/drop the line and return success to tracing, preventing
/// tracing-subscriber's own stderr error path. A later line tries output again.
pub struct ResilientWriter<W> {
    state: Arc<Mutex<State<W>>>,
}

impl<W> Clone for ResilientWriter<W> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<W: Write> ResilientWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                writer,
                dropped: None,
                recent: VecDeque::new(),
                recent_bytes: 0,
            })),
        }
    }

    pub fn recent_log(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recent
            .iter()
            .cloned()
            .collect()
    }

    pub fn dropped_lines(&self) -> Option<DroppedLines> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dropped
            .clone()
    }
}

impl<W> State<W> {
    fn remember(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        for line in text.split_inclusive('\n') {
            let start = line.ceil_char_boundary(line.len().saturating_sub(RECENT_LOG_BYTES));
            let line = line[start..].to_owned();
            self.recent_bytes += line.len();
            self.recent.push_back(line);
            while self.recent_bytes > RECENT_LOG_BYTES {
                if let Some(old) = self.recent.pop_front() {
                    self.recent_bytes -= old.len();
                }
            }
        }
    }

    fn failed(&mut self, error: io::Error, count: u64) {
        let now = chrono::Utc::now().to_rfc3339();
        match &mut self.dropped {
            Some(dropped) => {
                dropped.count = dropped.count.saturating_add(count);
                dropped.last_at = now;
                dropped.kind = error.kind();
                dropped.error = error.to_string();
            }
            None => {
                self.dropped = Some(DroppedLines {
                    count,
                    first_at: now.clone(),
                    last_at: now,
                    kind: error.kind(),
                    error: error.to_string(),
                })
            }
        }
    }
}

impl<W: Write> Write for ResilientWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lines = bytes.iter().filter(|byte| **byte == b'\n').count().max(1) as u64;
        if let Some(dropped) = &state.dropped {
            let summary = format!(
                "dropped {} log lines between {} and {}: {} ({:?})\n",
                dropped.count, dropped.first_at, dropped.last_at, dropped.error, dropped.kind,
            );
            if let Err(error) = state.writer.write_all(summary.as_bytes()) {
                state.remember(bytes);
                state.failed(error, lines);
                return Ok(bytes.len());
            }
            state.remember(summary.as_bytes());
            state.dropped = None;
        }
        state.remember(bytes);
        if let Err(error) = state.writer.write_all(bytes) {
            state.failed(error, lines);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = state.writer.flush() {
            state.failed(error, 0);
        }
        Ok(())
    }
}

/// Preserve explicitly enabled backtraces without invoking stderr's default hook.
pub fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let mut message = info.to_string();
    let backtrace = std::backtrace::Backtrace::capture();
    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        message.push('\n');
        message.push_str(&backtrace.to_string());
    }
    message
}

/// Worker tracing and direct diagnostics share this sink and its recent tail.
pub fn stderr_writer() -> ResilientWriter<io::Stderr> {
    static WRITER: OnceLock<ResilientWriter<io::Stderr>> = OnceLock::new();
    WRITER
        .get_or_init(|| ResilientWriter::new(io::stderr()))
        .clone()
}

pub fn stderr_line(args: std::fmt::Arguments<'_>) {
    let mut line = args.to_string();
    line.push('\n');
    let _ = stderr_writer().write_all(line.as_bytes());
}

pub fn stdout_line(args: std::fmt::Arguments<'_>) {
    static WRITER: OnceLock<ResilientWriter<io::Stdout>> = OnceLock::new();
    let mut line = args.to_string();
    line.push('\n');
    let _ = WRITER
        .get_or_init(|| ResilientWriter::new(io::stdout()))
        .clone()
        .write_all(line.as_bytes());
}

/// The timestamp format in log file names. It sorts as text in time order.
pub const LOG_TIMESTAMP_FORMAT: &str = "%Y%m%dT%H%M%S%.3fZ";

/// Marks a daemon's log that was kept because the daemon did not shut down
/// cleanly: `mj-daemon-<time>-<pid>.log` becomes
/// `mj-daemon-<time>-<pid>.crashed.log`. Retention treats such logs as a kind
/// of their own, so ordinary logs cannot push them out.
const CRASHED_LOG_MARK: &str = ".crashed";

/// The name `mj-daemon-<time>-<pid>.log` is kept under after a crash, or
/// `None` when `name` is not an unmarked managed log name.
pub fn crashed_log_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".log")?;
    (name.starts_with("mj-") && !stem.ends_with(CRASHED_LOG_MARK))
        .then(|| format!("{stem}{CRASHED_LOG_MARK}.log"))
}

/// `name` without its crashed mark, and whether it had one. Everything that
/// parses managed log names reads them through this, so a crashed log parses
/// as the log it was.
pub fn without_crashed_mark(name: &str) -> (std::borrow::Cow<'_, str>, bool) {
    match name
        .strip_suffix(".log")
        .and_then(|stem| stem.strip_suffix(CRASHED_LOG_MARK))
    {
        Some(stem) => (std::borrow::Cow::Owned(format!("{stem}.log")), true),
        None => (std::borrow::Cow::Borrowed(name), false),
    }
}

/// The start time and process of a daemon-kind log, read from its file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonLogName {
    pub started: String,
    pub pid: u32,
    pub crashed: bool,
}

/// Parse `mj-daemon-<time>-<pid>.log`, with or without the crashed mark. The
/// `mj node` process that serves a daemon writes the same kind of log.
pub fn parse_daemon_log_name(name: &str) -> Option<DaemonLogName> {
    let (name, crashed) = without_crashed_mark(name);
    let stem = name.strip_prefix("mj-daemon-")?.strip_suffix(".log")?;
    let (started, pid) = stem.split_once('-')?;
    Some(DaemonLogName {
        started: started.to_owned(),
        pid: pid.parse().ok()?,
        crashed,
    })
}

#[macro_export]
macro_rules! diagnostic_stderr {
    ($($arg:tt)*) => { $crate::logging::stderr_line(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! diagnostic_stdout {
    ($($arg:tt)*) => { $crate::logging::stdout_line(format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FullDisk {
        full: Arc<AtomicBool>,
        output: Arc<Mutex<Vec<u8>>>,
    }
    impl Write for FullDisk {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.full.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.full.load(Ordering::SeqCst) {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            } else {
                Ok(())
            }
        }
    }

    // Hard-won: d7ca109e4: diagnostic ENOSPC panicked a worker and permanently stopped daemon logging.
    #[test]
    fn full_disk_drops_lines_retains_bounded_tail_and_recovers_once() {
        let full = Arc::new(AtomicBool::new(true));
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut writer = ResilientWriter::new(FullDisk {
            full: full.clone(),
            output: output.clone(),
        });
        for index in 0..1000 {
            writer
                .write_all(format!("{index}: {}\n", "é".repeat(100)).as_bytes())
                .unwrap();
        }
        writer.flush().unwrap();
        let failure = writer.dropped_lines().unwrap();
        assert_eq!(failure.count, 1000);
        assert_eq!(
            failure.kind,
            io::Error::from_raw_os_error(libc::ENOSPC).kind()
        );
        assert!(failure.first_at <= failure.last_at);
        let tail = writer.recent_log();
        assert!(tail.iter().map(String::len).sum::<usize>() <= RECENT_LOG_BYTES);
        assert!(tail.last().unwrap().starts_with("999:"));
        assert!(output.lock().unwrap().is_empty());
        full.store(false, Ordering::SeqCst);
        writer.write_all(b"recovered\n").unwrap();
        writer.write_all(b"next\n").unwrap();
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(text.matches("dropped 1000 log lines between").count(), 1);
        assert!(text.contains(&failure.error));
        assert!(text.ends_with("recovered\nnext\n"));
        assert!(writer.dropped_lines().is_none());
        let recent = writer.recent_log().concat();
        assert_eq!(recent.matches("dropped 1000 log lines between").count(), 1);
        assert!(recent.ends_with("recovered\nnext\n"));
    }

    // Hard-won: d7ca109e4: oversized and escaped diagnostic tails must stay bounded on disk.
    #[test]
    fn oversized_unicode_line_keeps_the_latest_bytes_without_panicking() {
        let mut writer = ResilientWriter::new(io::sink());
        writer
            .write_all(format!("{}last\n", "é".repeat(100_000)).as_bytes())
            .unwrap();
        let tail = writer.recent_log();
        assert!(tail.iter().map(String::len).sum::<usize>() <= RECENT_LOG_BYTES);
        assert!(tail.last().unwrap().ends_with("last\n"));
    }
}
