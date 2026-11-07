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

/// When the daemon with `pid` last wrote its tracing log, which the daemon
/// already keeps current: the cheapest record of when it was last alive. The
/// file name format is owned by mj-cli's logging (`mj-daemon-<time>-<pid>.log`).
fn last_logged_at(data_dir: &Path, pid: u32) -> Option<SystemTime> {
    let suffix = format!("-{pid}.log");
    fs::read_dir(data_dir.join("logs"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("mj-daemon-") && name.ends_with(&suffix))
        })
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()
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
    let Ok(previous) = read_metadata_any() else {
        // No record is a clean stop; an unreadable one tells nothing we can name.
        return;
    };
    // A container daemon's record carries PID 0 and its guard is in the
    // container, so holding the host guard says nothing about it.
    if previous.pid == 0 {
        return;
    }
    let data_dir = mj_core::config::data_dir();
    let last_seen = last_logged_at(&data_dir, previous.pid);
    let boot = host_boot_time();
    let timestamp = |time: Option<SystemTime>| time.map_or_else(|| "unknown".to_owned(), rfc3339);
    tracing::warn!(
        previous_pid = previous.pid,
        previous_started_at = %previous.started_at,
        last_seen = %timestamp(last_seen),
        host_boot = %boot.map_or_else(|| "not available".to_owned(), rfc3339),
        "{}",
        conclusion(last_seen, boot)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
