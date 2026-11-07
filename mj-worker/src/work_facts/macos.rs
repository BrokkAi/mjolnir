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

const LISTING: &[&str] = &["-axww", "-o", "pid=,ppid=,etime=,stat=,command="];

pub(super) fn read_table(session_id: &str) -> Result<(Vec<ProcRow>, ProcessReader)> {
    let wanted = format!("MJ_SESSION_ID={session_id}");
    let marked: std::collections::BTreeSet<u32> = ps(&["-axwwE", "-o", "pid=,command="])?
        .lines()
        .filter(|line| line.split_whitespace().any(|word| word == wanted))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect();
    let listing = ps(LISTING)?;
    let mut rows = Vec::new();
    let mut commands = std::collections::BTreeMap::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(etime), Some(stat)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Some(elapsed)) = (
            pid.parse::<u32>(),
            ppid.parse::<u32>(),
            parse_elapsed_secs(etime),
        ) else {
            continue;
        };
        let command = fields.collect::<Vec<_>>().join(" ");
        if ppid == std::process::id() && command == own_ps_command(LISTING) {
            continue;
        }
        commands.insert(pid, command);
        rows.push(ProcRow {
            pid,
            ppid,
            age_ms: elapsed.saturating_mul(1000),
            zombie: stat.starts_with('Z'),
            marker: if marked.contains(&pid) {
                Marker::Present
            } else {
                Marker::Absent
            },
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
