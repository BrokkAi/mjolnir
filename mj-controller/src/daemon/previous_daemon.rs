use super::*;

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

/// How far apart the daemon and the `mj node` process it was started with
/// begin their logs. They start together (the same millisecond in some runs,
/// a few milliseconds apart in others), and the node's pid is not derived
/// from the daemon's.
const SIBLING_LOG_START_WINDOW: chrono::TimeDelta = chrono::TimeDelta::seconds(5);

/// Rename the previous daemon's tracing log, and the log of the `mj node`
/// that served it, to their crashed names so log retention keeps them apart
/// from ordinary logs. A process that is still running keeps its name: it may
/// still be writing. Returns the new paths.
fn keep_crashed_logs(data_dir: &Path, previous_pid: u32) -> Vec<PathBuf> {
    let logs = daemon_logs(data_dir);
    let started = |started: &str| {
        chrono::NaiveDateTime::parse_from_str(started, mj_core::logging::LOG_TIMESTAMP_FORMAT).ok()
    };
    let previous_starts: Vec<_> = logs
        .iter()
        .filter(|(_, name)| name.pid == previous_pid && !name.crashed)
        .filter_map(|(_, name)| started(&name.started))
        .collect();
    let mut kept = Vec::new();
    for (path, name) in logs {
        let is_previous = name.pid == previous_pid;
        let is_sibling = !is_previous
            && started(&name.started).is_some_and(|at| {
                previous_starts
                    .iter()
                    .any(|previous| (at - *previous).abs() <= SIBLING_LOG_START_WINDOW)
            })
            && !mj_client::daemon::process_is_alive(name.pid);
        if name.crashed || !(is_previous || is_sibling) {
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

/// Log one WARN line when the previous daemon left its `daemon.json` behind.
///
/// Call it only while holding the controller store guard: that proves the
/// recorded daemon is not running, so the record is a leftover and not a
/// daemon that is still shutting down.
pub(super) fn report_unclean_previous_daemon() {
    let previous = match read_metadata_any() {
        Ok(previous) => previous,
        // No record is a clean stop.
        Err(error) if daemon_not_running(&error).is_some() => return,
        // A record that is there but unreadable is not a clean stop either;
        // say so, though it names no previous daemon.
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "the previous daemon left a daemon.json that could not be read; it did not shut down cleanly"
            );
            return;
        }
    };
    // A container daemon's record carries PID 0 and its guard is in the
    // container, so holding the host guard says nothing about it.
    if previous.pid == 0 {
        return;
    }
    let data_dir = mj_core::config::data_dir();
    let last_seen = last_logged_at(&data_dir, previous.pid);
    let kept_logs = keep_crashed_logs(&data_dir, previous.pid);
    let boot = host_boot_time();
    let timestamp = |time: Option<SystemTime>| time.map_or_else(|| "unknown".to_owned(), rfc3339);
    tracing::warn!(
        previous_pid = previous.pid,
        previous_started_at = %previous.started_at,
        last_seen = %timestamp(last_seen),
        host_boot = %boot.map_or_else(|| "not available".to_owned(), rfc3339),
        kept_logs = ?kept_logs,
        "{}",
        conclusion(last_seen, boot)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_previous_daemon_and_its_node_keep_their_logs_under_crashed_names() {
        let data_dir = tempfile::tempdir().unwrap();
        let logs = data_dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        // A dead pid: the previous daemon and its node (a few ms apart, as in
        // the 2026-11-06 incident), an unrelated older daemon, and a live
        // process's log started in the same second.
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

        assert_eq!(kept.len(), 2);
        let present: std::collections::BTreeSet<String> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            format!("mj-daemon-20261106T163501.626Z-{dead}.crashed.log"),
            format!("mj-daemon-20261106T163501.629Z-{}.crashed.log", dead - 1),
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
