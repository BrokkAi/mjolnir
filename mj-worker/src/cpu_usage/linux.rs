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

/// A fixed CPU counter selected from this worker's cgroup membership.
#[derive(Debug, Clone)]
pub(super) struct CgroupCpu {
    path: std::path::PathBuf,
    unified: bool,
}

impl CgroupCpu {
    pub(super) fn discover() -> Result<Self> {
        Self::resolve(
            &std::fs::read_to_string("/proc/self/cgroup")?,
            &std::fs::read_to_string("/proc/self/mountinfo")?,
        )
    }

    fn resolve(membership: &str, mounts: &str) -> Result<Self> {
        use std::path::{Component, Path};
        for line in membership.lines() {
            let mut fields = line.splitn(3, ':');
            let hierarchy = fields.next().context("missing cgroup hierarchy")?;
            let controllers = fields.next().context("missing cgroup controllers")?;
            let group = fields.next().context("missing cgroup path")?;
            let unified = hierarchy == "0" && controllers.is_empty();
            if !unified && !controllers.split(',').any(|value| value == "cpuacct") {
                continue;
            }
            let group = Path::new(group);
            if !group.is_absolute() || group.components().any(|c| c == Component::ParentDir) {
                bail!("cgroup membership is outside the visible mount");
            }
            for mount in mounts.lines() {
                let Some((left, right)) = mount.split_once(" - ") else {
                    continue;
                };
                let right: Vec<_> = right.split_whitespace().collect();
                if right.len() < 3
                    || right[0] != if unified { "cgroup2" } else { "cgroup" }
                    || (!unified && !right[2].split(',').any(|c| c == "cpuacct"))
                {
                    continue;
                }
                let fields: Vec<_> = left.split_whitespace().collect();
                if fields.len() < 6 {
                    bail!("short cgroup mountinfo record");
                }
                let (Ok(root), Ok(target)) = (mount_path(fields[3]), mount_path(fields[4])) else {
                    continue;
                };
                // The mount root is the base for paths shown in
                // /proc/self/cgroup. A nonmatching root can expose an
                // ancestor mount whose counter cannot identify this cgroup.
                let Ok(relative) = group.strip_prefix(&root) else {
                    continue;
                };
                return Ok(Self {
                    path: target.join(relative).join(if unified {
                        "cpu.stat"
                    } else {
                        "cpuacct.usage"
                    }),
                    unified,
                });
            }
        }
        bail!("no visible CPU accounting mount for worker cgroup")
    }

    pub(super) fn read(&self) -> Result<Duration> {
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read cgroup CPU counter {}", self.path.display()))?;
        if !self.unified {
            return Ok(Duration::from_nanos(text.trim().parse()?));
        }
        let value = text
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next() == Some("usage_usec"))
                    .then(|| fields.next())
                    .flatten()
            })
            .context("cgroup cpu.stat has no usage_usec")?;
        Ok(Duration::from_micros(value.parse()?))
    }
}

fn mount_path(text: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut bytes = Vec::new();
    let mut input = text.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'\\' {
            let digits: Vec<_> = input.by_ref().take(3).collect();
            if digits.len() != 3 || digits.iter().any(|d| !(b'0'..=b'7').contains(d)) {
                bail!("invalid mountinfo path escape");
            }
            let value = digits
                .iter()
                .fold(0_u16, |n, d| n * 8 + u16::from(d - b'0'));
            bytes.push(u8::try_from(value)?);
        } else {
            bytes.push(byte);
        }
    }
    let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
    if !path.is_absolute()
        || path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("invalid cgroup mount path");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mountinfo_path(path: &std::path::Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\134")
            .replace(' ', "\\040")
    }

    #[test]
    fn cgroup_counter_maps_namespace_membership_and_reads_escaped_mount() {
        let directory = tempfile::tempdir().unwrap();
        let mount = directory.path().join("cgroup mount");
        let group = mount.join("session");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(
            group.join("cpu.stat"),
            "usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\n",
        )
        .unwrap();
        let target = mountinfo_path(&mount);
        let mounts = format!(
            "40 1 0:40 /unrelated /cgroup-old rw - cgroup2 cgroup rw\n\
             41 1 0:41 /tenant {target} rw - cgroup2 cgroup rw"
        );
        let counter = CgroupCpu::resolve("0::/tenant/session\n", &mounts).unwrap();
        assert_eq!(counter.read().unwrap(), Duration::from_micros(1_234_567));

        // A private cgroup namespace shows its own root as `/`; the mount
        // root must still agree with that view before it is used.
        let private_mounts = format!("42 1 0:42 / {target} rw - cgroup2 cgroup rw");
        let root_counter = CgroupCpu::resolve("0::/\n", &private_mounts).unwrap();
        assert_eq!(root_counter.path, mount.join("cpu.stat"));
    }

    #[test]
    fn cgroup_v1_counter_uses_cpuacct_membership_and_nanoseconds() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("cpuacct.usage"), "7654321000\n").unwrap();
        let mount = mountinfo_path(directory.path());
        let mounts = format!("51 1 0:51 /tenant {mount} rw - cgroup cgroup rw,cpu,cpuacct");
        let counter = CgroupCpu::resolve("4:cpu,cpuacct:/tenant\n", &mounts).unwrap();
        assert_eq!(counter.read().unwrap(), Duration::from_nanos(7_654_321_000));
    }

    #[test]
    fn parse_proc_stat_reads_fields_after_a_command_name_with_parentheses() {
        let stat = parse_proc_stat("123 (a) (b) S 42 0 0 0 0 0 0 0 0 0 10 20 30 40 0").unwrap();
        assert_eq!(stat.parent, 42);
        assert_eq!(stat.ticks, 100);
    }
}
