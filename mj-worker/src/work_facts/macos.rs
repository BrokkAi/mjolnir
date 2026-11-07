//! `ps` reader for the process scan. macOS has no `/proc`; `ps -E` appends a
//! process's environment to its command line, which is how the session marker
//! is found.

use anyhow::{Context, Result, bail};
use std::process::Command;

use super::{Described, Marker, ProcRow, ProcessReader};

/// `[[dd-]hh:]mm:ss` as `ps -o etime` prints it.
pub(super) fn parse_elapsed_secs(text: &str) -> Option<i64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<i64>().ok()?, clock),
        None => (0, text),
    };
    let mut seconds = 0_i64;
    for part in clock.split(':') {
        seconds = seconds
            .checked_mul(60)?
            .checked_add(part.parse::<i64>().ok()?)?;
    }
    days.checked_mul(86_400)?.checked_add(seconds)
}

/// The command line `ps` shows for itself when run with `args`. The scan's own
/// `ps` is a child of the worker that started after the harness was
/// configured, so it would be reported as the agent's work without this.
fn own_ps_command(args: &[&str]) -> String {
    format!("ps {}", args.join(" "))
}

fn ps(args: &[&str]) -> Result<String> {
    let mut command = Command::new("ps");
    command.args(args).env("LC_ALL", "C");
    let output = mj_core::subprocess::run_with_input(&mut command, &[]).context("run ps")?;
    if !output.status.success() {
        bail!("ps failed with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

const LISTING: &[&str] = &["-axww", "-o", "pid=,ppid=,uid=,etime=,stat=,command="];

/// Whether a process's environment carries the session marker, from the line
/// `ps -E` printed for it. `ps -E` appends a process's environment to its
/// command; for a process of the same user whose environment it cannot show it
/// prints the command alone, which is the case reported as unreadable rather
/// than as an absent marker (the Linux reader does the same for a refused
/// read). A process of another user is not this session's and stays absent.
pub(super) fn classify_marker(
    environment_line: Option<&str>,
    command: &str,
    same_user: bool,
    wanted: &str,
) -> Marker {
    let Some(line) = environment_line else {
        return Marker::Absent;
    };
    // The line is `pid command... ENV=value ...`.
    let words: Vec<&str> = line.split_whitespace().skip(1).collect();
    if words.contains(&wanted) {
        Marker::Present
    } else if same_user && words.join(" ") == command {
        Marker::Unreadable
    } else {
        Marker::Absent
    }
}

pub(super) fn read_table(session_id: &str) -> Result<(Vec<ProcRow>, ProcessReader)> {
    let wanted = format!("MJ_SESSION_ID={session_id}");
    let environment_lines: std::collections::BTreeMap<u32, String> =
        ps(&["-axwwE", "-o", "pid=,command="])?
            .lines()
            .filter_map(|line| {
                Some((
                    line.split_whitespace().next()?.parse().ok()?,
                    line.to_owned(),
                ))
            })
            .collect();
    let me = unsafe { libc::geteuid() };
    let listing = ps(LISTING)?;
    let mut rows = Vec::new();
    let mut commands = std::collections::BTreeMap::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(uid), Some(etime), Some(stat)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(uid), Some(elapsed)) = (
            pid.parse::<u32>(),
            ppid.parse::<u32>(),
            uid.parse::<u32>(),
            parse_elapsed_secs(etime),
        ) else {
            continue;
        };
        let command = fields.collect::<Vec<_>>().join(" ");
        if ppid == std::process::id() && command == own_ps_command(LISTING) {
            continue;
        }
        let marker = classify_marker(
            environment_lines.get(&pid).map(String::as_str),
            &command,
            uid == me,
            &wanted,
        );
        commands.insert(pid, command);
        rows.push(ProcRow {
            pid,
            ppid,
            age_ms: elapsed.saturating_mul(1000),
            // `etime` is whole seconds.
            age_granularity_ms: 1000,
            zombie: stat.starts_with('Z'),
            marker,
        });
    }
    let reader = ProcessReader::new(move |row| {
        let command = commands.get(&row.pid).cloned().unwrap_or_default();
        let exe = command
            .split(' ')
            .next()
            .filter(|program| program.starts_with('/'))
            .map(std::path::PathBuf::from);
        Described {
            command,
            exe,
            identity: mj_core::subprocess::process_birth_identity(row.pid).unwrap_or_default(),
        }
    });
    Ok((rows, reader))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_same_user_process_whose_environment_ps_cannot_show_is_unreadable() {
        let wanted = "MJ_SESSION_ID=abc";
        let command = "/usr/bin/tool --flag";
        assert_eq!(
            classify_marker(
                Some("  41 /usr/bin/tool --flag HOME=/h MJ_SESSION_ID=abc"),
                command,
                true,
                wanted
            ),
            Marker::Present
        );
        assert_eq!(
            classify_marker(
                Some("  41 /usr/bin/tool --flag HOME=/h"),
                command,
                true,
                wanted
            ),
            Marker::Absent
        );
        // No environment appended to a same-user process: it could not be read.
        assert_eq!(
            classify_marker(Some("  41 /usr/bin/tool --flag"), command, true, wanted),
            Marker::Unreadable
        );
        // Another user's process is not this session's.
        assert_eq!(
            classify_marker(Some("  41 /usr/bin/tool --flag"), command, false, wanted),
            Marker::Absent
        );
        assert_eq!(classify_marker(None, command, true, wanted), Marker::Absent);
    }
}
