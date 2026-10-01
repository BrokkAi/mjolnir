use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Debug)]
struct ProcStat {
    parent: u32,
    ticks: u64,
}

fn parse_proc_stat(text: &str) -> Result<ProcStat> {
    let (_, fields) = text
        .rsplit_once(')')
        .context("missing command name in /proc stat")?;
    // The tail starts at field 3 (state); ppid is 4, CPU counters are 14..17.
    let fields: Vec<_> = fields.split_whitespace().take(15).collect();
    if fields.len() < 15 {
        bail!("short /proc stat");
    }
    let parent = fields[1].parse()?;
    let mut ticks = 0_u64;
    for field in &fields[11..15] {
        // Waited-for child counters are signed, though normally nonnegative.
        ticks = ticks.saturating_add(field.parse::<i64>()?.max(0) as u64);
    }
    Ok(ProcStat { parent, ticks })
}

pub(super) fn process_tree_cpu_time(root: u32) -> Result<Duration> {
    let mut processes = BTreeMap::new();
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for entry in std::fs::read_dir("/proc").context("list processes for CPU sampling")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let result = std::fs::read_to_string(entry.path().join("stat"))
            .map_err(anyhow::Error::from)
            .and_then(|text| parse_proc_stat(&text));
        let stat = match result {
            Ok(stat) => stat,
            Err(error) if pid == root => return Err(error.context("read worker CPU time")),
            Err(_) => continue, // Other processes may exit or be unreadable.
        };
        children.entry(stat.parent).or_default().push(pid);
        processes.insert(pid, stat);
    }
    if !processes.contains_key(&root) {
        bail!("worker process {root} disappeared during CPU sampling");
    }
    let mut pending = vec![root];
    let mut visited = BTreeSet::new();
    let mut ticks = 0_u64;
    while let Some(pid) = pending.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if let Some(stat) = processes.get(&pid) {
            ticks = ticks.saturating_add(stat.ticks);
        }
        if let Some(child_pids) = children.get(&pid) {
            pending.extend(child_pids);
        }
    }
    let frequency = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if frequency <= 0 {
        bail!("could not read CPU clock tick frequency");
    }
    Ok(Duration::from_secs_f64(ticks as f64 / frequency as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_proc_stat_reads_fields_after_a_command_name_with_parentheses() {
        let stat = parse_proc_stat("123 (a) (b) S 42 0 0 0 0 0 0 0 0 0 10 20 30 40 0").unwrap();
        assert_eq!(stat.parent, 42);
        assert_eq!(stat.ticks, 100);
    }
}
