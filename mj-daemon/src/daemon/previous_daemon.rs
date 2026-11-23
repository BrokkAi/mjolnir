use super::*;
use mj_core::daemon_exit::{
    DaemonExitRecord, daemon_exits_dir, format_exit_reason, read_daemon_exit_record,
};

/// Why the previous daemon of this instance is gone, judged from what it left
/// behind. A daemon removes `daemon.json` on every exit path it controls (the
/// epilogue and the shutdown watchdog), so a record that is still there once
/// this daemon owns the store means the previous one did not shut down.
const HOST_RESTARTED: &str = "the host restarted after the previous daemon was last seen";
const STOPPED_UNCLEANLY: &str = "the previous daemon stopped without a clean shutdown";

fn conclusion(last_seen: Option<SystemTime>, boot: Option<SystemTime>) -> &'static str {
    match (last_seen, boot) {
        (Some(last_seen), Some(boot)) if boot > last_seen => HOST_RESTARTED,
        _ => STOPPED_UNCLEANLY,
    }
}

/// The host's boot time. Only Linux exposes it cheaply (`btime` in
/// `/proc/stat`); elsewhere the field is left out.
fn host_boot_time() -> Option<SystemTime> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let seconds = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse::<u64>()
            .ok()?;
        Some(UNIX_EPOCH + Duration::from_secs(seconds))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The daemon-kind logs in the log directory, with their parsed names. The
/// file name format is owned by `mj_core::logging`.
fn daemon_logs(data_dir: &Path) -> Vec<(PathBuf, mj_core::logging::DaemonLogName)> {
    fs::read_dir(data_dir.join("logs"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let parsed = mj_core::logging::parse_daemon_log_name(entry.file_name().to_str()?)?;
            Some((entry.path(), parsed))
        })
        .collect()
}

/// When the daemon with `pid` last wrote its tracing log, which the daemon
/// already keeps current: the cheapest record of when it was last alive.
fn last_logged_at(data_dir: &Path, pid: u32) -> Option<SystemTime> {
    daemon_logs(data_dir)
        .into_iter()
        .filter(|(_, name)| name.pid == pid)
        .filter_map(|(path, _)| path.metadata().ok()?.modified().ok())
        .max()
}

/// Rename the previous daemon's tracing log to its crashed name so log
/// retention keeps it apart from ordinary logs. Returns the new paths.
fn keep_crashed_logs(data_dir: &Path, previous_pid: u32) -> Vec<PathBuf> {
    let mut kept = Vec::new();
    for (path, name) in daemon_logs(data_dir) {
        if name.crashed || name.pid != previous_pid {
            continue;
        }
        let Some(crashed) = path
            .file_name()
            .and_then(|file| file.to_str())
            .and_then(mj_core::logging::crashed_log_name)
        else {
            continue;
        };
        let destination = path.with_file_name(crashed);
        match fs::rename(&path, &destination) {
            Ok(()) => kept.push(destination),
            Err(error) => tracing::warn!(
                log = %path.display(),
                %error,
                "could not keep the crashed daemon's log under its crashed name"
            ),
        }
    }
    kept
}

fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Report abnormal exits and preserve their diagnostic logs. Call this only
/// while holding the controller store guard, which excludes a live predecessor.
pub(super) fn report_unclean_previous_daemon() {
    report_unclean_previous_daemon_at(
        &mj_core::config::data_dir(),
        read_metadata_any(),
        host_boot_time,
    );
}

fn report_unclean_previous_daemon_at(
    data_dir: &Path,
    previous: Result<DaemonMetadata>,
    host_boot_time: impl FnOnce() -> Option<SystemTime>,
) {
    let mut exits = read_exit_records(data_dir);
    match previous {
        Ok(previous) if previous.pid > 0 => {
            let boot = host_boot_time();
            if let Some(index) = exits
                .iter()
                .position(|(_, record)| record.pid == previous.pid)
            {
                let (path, record) = exits.remove(index);
                report_exit_record(data_dir, &record, Some((&previous, boot)));
                remove_exit_record(&path);
            } else {
                let last_seen = last_logged_at(data_dir, previous.pid);
                let kept_logs = keep_crashed_logs(data_dir, previous.pid);
                tracing::warn!(
                    previous_pid = previous.pid,
                    previous_started_at = %previous.started_at,
                    last_seen = %timestamp(last_seen),
                    host_boot = %boot_timestamp(boot),
                    kept_logs = ?kept_logs,
                    "{}",
                    conclusion(last_seen, boot)
                );
            }
        }
        Err(error) if daemon_not_running(&error).is_none() => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "the previous daemon left a daemon.json that could not be read; it did not shut down cleanly"
            );
        }
        // A host-side container record has PID 0. It does not identify
        // a process in this namespace.
        _ => {}
    }

    for (path, record) in exits {
        report_exit_record(data_dir, &record, None);
        remove_exit_record(&path);
    }
}

fn timestamp(time: Option<SystemTime>) -> String {
    time.map_or_else(|| "unknown".to_owned(), rfc3339)
}

fn boot_timestamp(time: Option<SystemTime>) -> String {
    time.map_or_else(|| "not available".to_owned(), rfc3339)
}

fn read_exit_records(data_dir: &Path) -> Vec<(PathBuf, DaemonExitRecord)> {
    let directory = daemon_exits_dir(data_dir);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(directory = %directory.display(), %error, "could not list daemon exit records");
            return Vec::new();
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                tracing::warn!(directory = %directory.display(), %error, "could not read a daemon exit directory entry");
                continue;
            }
        };
        if path.extension().is_none_or(|extension| extension != "json")
            || path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
        {
            continue;
        }
        match path.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => paths.push(path),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not inspect daemon exit record");
            }
        }
    }
    paths.sort();

    paths
        .into_iter()
        .filter_map(|path| match read_daemon_exit_record(&path) {
            Ok(record) => Some((path, record)),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "could not read daemon exit record");
                remove_exit_record(&path);
                None
            }
        })
        .collect()
}

fn report_exit_record(
    data_dir: &Path,
    record: &DaemonExitRecord,
    previous: Option<(&DaemonMetadata, Option<SystemTime>)>,
) {
    let last_seen = last_logged_at(data_dir, record.pid);
    let kept_logs = keep_crashed_logs(data_dir, record.pid);
    tracing::warn!(
        previous_pid = record.pid,
        previous_started_at = previous.map(|(metadata, _)| metadata.started_at.as_str()),
        exited_at = %record.exited_at,
        exit_code = record.exit_code,
        signal = record.signal,
        signal_name = record.signal_name.as_deref(),
        core_dumped = record.core_dumped,
        last_seen = %timestamp(last_seen),
        host_boot = previous.map(|(_, boot)| boot_timestamp(boot)).as_deref(),
        kept_logs = ?kept_logs,
        "{}",
        format_exit_reason(record)
    );
}

fn remove_exit_record(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "could not remove daemon exit record");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous(pid: u32) -> DaemonMetadata {
        DaemonMetadata {
            pid,
            protocol_version: PROTOCOL_VERSION,
            address: "127.0.0.1:1".parse().unwrap(),
            token: "fixture".to_owned(),
            started_at: "2026-11-06T16:35:01Z".to_owned(),
            build_version: "fixture".to_owned(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        }
    }

    fn absent(data_dir: &Path) -> Result<DaemonMetadata> {
        Err(DaemonNotRunning {
            metadata_path: data_dir.join("daemon.json"),
        }
        .into())
    }

    fn report(
        data_dir: &Path,
        metadata: Result<DaemonMetadata>,
        boot: Option<SystemTime>,
    ) -> String {
        let trace = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(trace.reopen().unwrap())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            report_unclean_previous_daemon_at(data_dir, metadata, || boot);
        });
        fs::read_to_string(trace.path()).unwrap()
    }

    fn daemon_log(data_dir: &Path, pid: u32) -> PathBuf {
        let logs = data_dir.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join(format!("mj-daemon-20261106T163501.626Z-{pid}.log"));
        fs::write(&path, "log").unwrap();
        path
    }

    fn exit_record(data_dir: &Path, pid: u32) -> PathBuf {
        use mj_core::daemon_exit::{DAEMON_EXIT_SCHEMA, write_daemon_exit_record};

        write_daemon_exit_record(
            &daemon_exits_dir(data_dir),
            &DaemonExitRecord {
                schema: DAEMON_EXIT_SCHEMA,
                pid,
                waiter_pid: Some(pid - 1),
                exited_at: "2026-11-06T16:35:02Z".to_owned(),
                exit_code: None,
                signal: Some(9),
                signal_name: Some("SIGKILL".to_owned()),
                core_dumped: Some(false),
            },
        )
        .unwrap()
    }

    #[test]
    fn previous_daemon_exit_record_and_metadata_are_reported_together() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path();
        let pid = 42;
        let log = daemon_log(data_dir, pid);
        let exit = exit_record(data_dir, pid);
        let boot = Some(UNIX_EPOCH + Duration::from_secs(1_300));

        let warnings = report(data_dir, Ok(previous(pid)), boot);
        assert_eq!(warnings.lines().count(), 1, "{warnings}");
        for field in [
            "WARN",
            "terminated by signal SIGKILL (9)",
            "previous_pid=42",
            "previous_started_at=\"2026-11-06T16:35:01Z\"",
            "exited_at=2026-11-06T16:35:02Z",
            "signal=9",
            "signal_name=\"SIGKILL\"",
            "core_dumped=false",
            "last_seen=",
            "host_boot=\"1970-01-01T00:21:40Z\"",
            "kept_logs=",
        ] {
            assert!(warnings.contains(field), "missing {field}: {warnings}");
        }
        assert!(!log.exists());
        assert!(log.with_extension("crashed.log").exists());
        assert!(!exit.exists());
        // This daemon publishes and later removes its own metadata.
        assert!(report(data_dir, absent(data_dir), boot).is_empty());
    }

    #[test]
    fn previous_daemon_exit_records_survive_absent_container_or_unreadable_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path();
        for (pid, metadata) in [
            (42, absent(data_dir)),
            (43, Ok(previous(0))),
            (44, Err(anyhow!("invalid metadata fixture"))),
        ] {
            let log = daemon_log(data_dir, pid);
            let exit = exit_record(data_dir, pid);
            let warnings = report(data_dir, metadata, None);
            assert!(warnings.contains("WARN"), "{warnings}");
            assert!(
                warnings.contains(&format!("previous_pid={pid}")),
                "{warnings}"
            );
            assert!(
                warnings.contains("terminated by signal SIGKILL (9)"),
                "{warnings}"
            );
            assert!(warnings.contains("signal=9"), "{warnings}");
            assert!(!warnings.contains("previous_started_at"), "{warnings}");
            assert!(!log.exists());
            assert!(log.with_extension("crashed.log").exists());
            assert!(!exit.exists());
        }
        assert!(report(data_dir, absent(data_dir), None).is_empty());
    }

    #[test]
    fn previous_daemon_without_exit_record_still_infers_a_host_restart() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path();
        let pid = 42;
        let log = daemon_log(data_dir, pid);
        let modified = log.metadata().unwrap().modified().unwrap();

        let warnings = report(
            data_dir,
            Ok(previous(pid)),
            Some(modified + Duration::from_secs(60)),
        );
        assert!(warnings.contains(HOST_RESTARTED), "{warnings}");
        assert!(log.with_extension("crashed.log").exists());
    }

    #[test]
    fn previous_daemon_discards_corrupt_exit_records_but_ignores_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path();
        let exit_dir = daemon_exits_dir(data_dir);
        fs::create_dir(&exit_dir).unwrap();
        let corrupt = exit_dir.join("42.json");
        let temporary = exit_dir.join(".tmp.json");
        let subdirectory = exit_dir.join("43.json");
        fs::write(&corrupt, "incomplete").unwrap();
        fs::write(&temporary, "incomplete").unwrap();
        fs::create_dir(&subdirectory).unwrap();

        let warnings = report(data_dir, absent(data_dir), None);
        assert_eq!(warnings.lines().count(), 1, "{warnings}");
        assert!(
            warnings.contains("could not read daemon exit record"),
            "{warnings}"
        );
        assert!(warnings.contains("42.json"), "{warnings}");
        assert!(!corrupt.exists());
        assert!(temporary.is_file());
        assert!(subdirectory.is_dir());
        assert!(report(data_dir, absent(data_dir), None).is_empty());
    }

    #[test]
    fn the_previous_daemon_keeps_its_log_under_a_crashed_name() {
        let data_dir = tempfile::tempdir().unwrap();
        let logs = data_dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        // A dead pid: the previous daemon, another daemon started a few ms
        // later, an unrelated older daemon, and a live process's log started
        // in the same second.
        let dead = u32::MAX - 1;
        let live = std::process::id();
        let names = [
            format!("mj-daemon-20261106T163501.626Z-{dead}.log"),
            format!("mj-daemon-20261106T163501.629Z-{}.log", dead - 1),
            format!("mj-daemon-20261106T120000.000Z-{}.log", dead - 2),
            format!("mj-daemon-20261106T163501.630Z-{live}.log"),
            format!("mj-tui-20261106T163501.198Z-{dead}.log"),
        ];
        for name in &names {
            fs::write(logs.join(name), "log").unwrap();
        }

        let kept = keep_crashed_logs(data_dir.path(), dead);

        assert_eq!(kept.len(), 1);
        let present: std::collections::BTreeSet<String> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            format!("mj-daemon-20261106T163501.626Z-{dead}.crashed.log"),
            names[1].clone(),
            names[2].clone(),
            names[3].clone(),
            names[4].clone(),
        ]
        .into();
        assert_eq!(present, expected);
        // A second report finds nothing left to rename.
        assert!(keep_crashed_logs(data_dir.path(), dead).is_empty());
    }

    #[test]
    fn a_host_boot_after_the_last_sighting_is_reported_as_a_host_restart() {
        let last_seen = UNIX_EPOCH + Duration::from_secs(1_000);
        let after = Some(UNIX_EPOCH + Duration::from_secs(1_300));
        let before = Some(UNIX_EPOCH + Duration::from_secs(500));
        assert_eq!(conclusion(Some(last_seen), after), HOST_RESTARTED);
        assert_eq!(conclusion(Some(last_seen), before), STOPPED_UNCLEANLY);
        // Without both times nothing supports a claim about the host.
        assert_eq!(conclusion(None, after), STOPPED_UNCLEANLY);
        assert_eq!(conclusion(Some(last_seen), None), STOPPED_UNCLEANLY);
    }
}
