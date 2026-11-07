//! `/proc` reader for the process scan.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use super::{Described, ProcRow, ProcessReader};

/// Fields of `/proc/<pid>/stat` that follow the command name.
struct Stat {
    zombie: bool,
    ppid: u32,
    start_ticks: u64,
}

fn parse_stat(text: &str) -> Option<Stat> {
    let (_, tail) = text.rsplit_once(')')?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    // State is field 3 of the file and so index 0 here; ppid is field 4 and
    // the start time, in clock ticks since boot, is field 22.
    Some(Stat {
        zombie: fields.first()?.starts_with('Z'),
        ppid: fields.get(1)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

fn ticks_per_second() -> Result<u64> {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        bail!("could not read the clock tick frequency");
    }
    Ok(ticks as u64)
}

/// Uptime in milliseconds. It is monotonic, unlike the boot time in
/// `/proc/stat`, which moves with wall-clock corrections.
fn uptime_ms() -> Result<i64> {
    let text = std::fs::read_to_string("/proc/uptime").context("read /proc/uptime")?;
    let seconds: f64 = text
        .split_whitespace()
        .next()
        .context("empty /proc/uptime")?
        .parse()
        .context("parse /proc/uptime")?;
    Ok((seconds * 1000.0) as i64)
}

fn has_session_marker(pid: u32, wanted: &[u8]) -> bool {
    std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
        environ
            .split(|byte| *byte == 0)
            .any(|entry| entry == wanted)
    })
}

/// Every process the scan may consider, and the reader for the survivors.
pub(super) fn read_table(session_id: &str) -> Result<(Vec<ProcRow>, ProcessReader)> {
    let hz = ticks_per_second()?;
    let uptime = uptime_ms()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read the boot identity")?
        .trim()
        .to_owned();
    let wanted = format!("MJ_SESSION_ID={session_id}").into_bytes();
    let mut rows = Vec::new();
    for entry in std::fs::read_dir("/proc").context("list processes")? {
        let Some(pid) = entry
            .ok()
            .and_then(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        else {
            continue;
        };
        // A process may exit between the listing and the read.
        let Some(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| parse_stat(&text))
        else {
            continue;
        };
        let start_ms = i64::try_from(stat.start_ticks.saturating_mul(1000) / hz).unwrap_or(0);
        rows.push(ProcRow {
            pid,
            ppid: stat.ppid,
            age_ms: uptime.saturating_sub(start_ms).max(0),
            zombie: stat.zombie,
            marked: has_session_marker(pid, &wanted),
        });
    }
    // The start tick is read again for survivors only, so the identity matches
    // the one `PROCESS_BIRTH_SCRIPT` prints for the same process.
    let reader = ProcessReader::new(move |row| describe(row.pid, &boot_id));
    Ok((rows, reader))
}

fn describe(pid: u32, boot_id: &str) -> Described {
    let command = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .map(String::from_utf8_lossy)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|command| !command.trim().is_empty())
        .or_else(|| {
            // A process without arguments, such as a kernel thread, has only
            // its name.
            let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let name = text.split_once('(')?.1.rsplit_once(')')?.0.to_owned();
            Some(format!("[{name}]"))
        })
        .unwrap_or_default();
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| {
            // A replaced executable reads back with this suffix.
            let text = path.to_string_lossy();
            PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text))
        });
    let identity = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|text| parse_stat(&text))
        .map(|stat| format!("{boot_id}:{}", stat.start_ticks))
        .unwrap_or_default();
    Described {
        command,
        exe,
        identity,
    }
}
