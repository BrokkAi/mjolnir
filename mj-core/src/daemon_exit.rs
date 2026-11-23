//! Exit status captured by the daemon's detached parent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DAEMON_EXIT_SCHEMA: u32 = 1;
pub const STORE_OWNED_EXIT_CODE: i32 = 75;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonExitRecord {
    #[serde(default = "default_daemon_exit_schema")]
    pub schema: u32,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiter_pid: Option<u32>,
    #[serde(default)]
    pub exited_at: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    #[serde(default)]
    pub signal_name: Option<String>,
    #[serde(default)]
    pub core_dumped: Option<bool>,
}

fn default_daemon_exit_schema() -> u32 {
    DAEMON_EXIT_SCHEMA
}

pub fn daemon_exits_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon-exits")
}

pub fn write_daemon_exit_record(exit_dir: &Path, record: &DaemonExitRecord) -> Result<PathBuf> {
    crate::config::create_owner_only_directory(exit_dir)?;
    let path = exit_dir.join(format!("{}.json", record.pid));
    crate::config::atomic_write_existing(&path, &serde_json::to_vec(record)?)?;
    Ok(path)
}

pub fn read_daemon_exit_record(path: &Path) -> Result<DaemonExitRecord> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn format_exit_reason(record: &DaemonExitRecord) -> String {
    if let Some(signal) = record.signal {
        let core = if record.core_dumped == Some(true) {
            ", core dumped"
        } else {
            ""
        };
        return match &record.signal_name {
            Some(name) => format!("terminated by signal {name} ({signal}{core})"),
            None if core.is_empty() => format!("terminated by signal {signal}"),
            None => format!("terminated by signal {signal} (core dumped)"),
        };
    }
    match record.exit_code {
        Some(code) => format!("exited with status {code}"),
        None => "exited with an unknown status".to_owned(),
    }
}

#[cfg(unix)]
pub fn unix_signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGSYS => "SIGSYS",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_exit_fixture_tolerates_future_and_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("42.json");
        std::fs::write(&path, r#"{"pid":42,"future":{"version":2}}"#).unwrap();
        let record = read_daemon_exit_record(&path).unwrap();
        assert_eq!(
            record,
            DaemonExitRecord {
                schema: DAEMON_EXIT_SCHEMA,
                pid: 42,
                waiter_pid: None,
                exited_at: String::new(),
                exit_code: None,
                signal: None,
                signal_name: None,
                core_dumped: None,
            }
        );
    }

    #[test]
    fn daemon_exit_record_replaces_atomically_and_formats_wait_status() {
        let dir = tempfile::tempdir().unwrap();
        let exit_dir = daemon_exits_dir(dir.path());
        let mut record = DaemonExitRecord {
            schema: DAEMON_EXIT_SCHEMA,
            pid: 42,
            waiter_pid: Some(41),
            exited_at: "2026-11-23T12:00:00Z".to_owned(),
            exit_code: None,
            signal: Some(6),
            signal_name: Some("SIGABRT".to_owned()),
            core_dumped: Some(true),
        };
        let path = write_daemon_exit_record(&exit_dir, &record).unwrap();
        assert_eq!(path, exit_dir.join("42.json"));
        assert_eq!(read_daemon_exit_record(&path).unwrap(), record);
        assert_eq!(
            format_exit_reason(&record),
            "terminated by signal SIGABRT (6, core dumped)"
        );
        record.signal = Some(9);
        record.signal_name = Some("SIGKILL".to_owned());
        record.core_dumped = Some(false);
        assert_eq!(
            format_exit_reason(&record),
            "terminated by signal SIGKILL (9)"
        );
        let mut original_file = std::fs::File::open(&path).unwrap();
        let original_record = read_daemon_exit_record(&path).unwrap();
        record.signal = None;
        record.signal_name = None;
        record.core_dumped = None;
        record.exit_code = Some(1);
        assert_eq!(write_daemon_exit_record(&exit_dir, &record).unwrap(), path);
        assert_eq!(read_daemon_exit_record(&path).unwrap(), record);
        // A reader already holding the old file must still see the old
        // record after publication, even though the path now names the new one.
        assert_eq!(
            serde_json::from_reader::<_, DaemonExitRecord>(&mut original_file).unwrap(),
            original_record
        );
        assert_eq!(format_exit_reason(&record), "exited with status 1");
        assert_eq!(std::fs::read_dir(&exit_dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                exit_dir.metadata().unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
