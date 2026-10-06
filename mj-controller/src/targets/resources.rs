use super::*;

pub(super) const HOST_RESOURCE_USAGE_SCRIPT: &str = r#"
memory_proc_root=${1:-/proc}
read_cpu() { awk '/^cpu / { total=0; for (i=2; i<=NF; i++) total += $i; print total, $5 + $6 }' /proc/stat; }
set -- $(read_cpu); total_before=$1; idle_before=$2
sleep 0.25
set -- $(read_cpu); total_after=$1; idle_after=$2
awk -v total="$((total_after - total_before))" -v idle="$((idle_after - idle_before))" \
    'BEGIN { if (total > 0) printf "cpu.percent=%.0f\n", (total - idle) * 100 / total }'
arc_size=0
arc_min=0
arcstats="$memory_proc_root/spl/kstat/zfs/arcstats"
if [ -r "$arcstats" ]; then
    set -- $(awk '
        $1 == "c_min" { arc_min = $3 }
        $1 == "size" { arc_size = $3 }
        END { printf "%.0f %.0f\n", arc_size, arc_min }
    ' "$arcstats")
    arc_size=$1
    arc_min=$2
fi
awk -v arc_size="$arc_size" -v arc_min="$arc_min" '
    /^MemTotal:/ { memory_total = $2 }
    /^MemAvailable:/ { memory_available = $2 }
    /^SwapTotal:/ { swap_total = $2 }
    /^SwapFree:/ { swap_free = $2 }
    END {
        memory_total *= 1024
        memory_available *= 1024
        # Like btop, count ARC above its minimum size as reclaimable cache.
        if (arc_size > arc_min) memory_available += arc_size - arc_min
        if (memory_available > memory_total) memory_available = memory_total
        printf "memory.current=%.0f\n", memory_total - memory_available
        printf "memory.max=%.0f\n", memory_total
        printf "memory.swap.current=%.0f\n", (swap_total - swap_free) * 1024
        printf "memory.swap.max=%.0f\n", swap_total * 1024
    }
' "$memory_proc_root/meminfo"
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
"#;

pub(super) const AWS_ALLOCATED_CAPACITY_SCRIPT: &str = r#"
awk '/^MemTotal:/ { printf "memory.total=%.0f\n", $2 * 1024 }' /proc/meminfo
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
df -B1 -P -- "$1" | awk 'NR == 2 { print "disk.total=" $2 }'
"#;

/// Where Mjolnir writes on an EC2 instance besides its workspace.
const EC2_STORAGE_PATHS: [&str; 5] = [
    mj_core::targets::storage::REMOTE_WORKERS_DIRECTORY,
    mj_core::targets::storage::REMOTE_PROFILES_DIRECTORY,
    mj_core::targets::storage::REMOTE_CACHE_DIRECTORY,
    mj_core::targets::storage::DEFAULT_BUILD_CACHE_DIRECTORY,
    mj_core::targets::storage::TEMPORARY_DIRECTORY,
];

/// Sample a host's CPU and memory, and the free space at each of
/// `storage_paths` (relative paths are under the SSH user's home). One SSH
/// command per host and poll: storage rides on the capacity probe rather
/// than adding round trips.
pub fn ssh_host_capacity_command(ssh: &SshTarget, storage_paths: &[String]) -> CommandSpec {
    let storage = mj_core::targets::storage::STORAGE_PROBE_SCRIPT;
    // The storage loop reads the paths from the arguments; the resource
    // scripts read an optional /proc override from `$1`, so clear them first.
    let script = format!(
        "set -eu\n{storage}\nset --\ncase $(uname -s) in\nDarwin)\n{DARWIN_HOST_RESOURCE_USAGE_SCRIPT}\n;;\nLinux)\n{HOST_RESOURCE_USAGE_SCRIPT}\n;;\n*) echo 'unsupported host operating system for capacity sampling' >&2; exit 1;;\nesac"
    );
    let mut words = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script,
        "mj-capacity".to_owned(),
    ];
    words.extend(storage_paths.iter().cloned());
    ssh_command(ssh, words).purpose("sample deployment host capacity")
}

// vm_stat reports pages, whose size differs between Intel and Apple silicon.
// Count active, wired and compressed pages as used; inactive/speculative pages
// are reclaimable cache. top's second sample measures an interval, not uptime.
pub(super) const DARWIN_HOST_RESOURCE_USAGE_SCRIPT: &str = r#"
export LC_ALL=C
memory_total=$(sysctl -n hw.memsize)
cores=$(sysctl -n hw.logicalcpu)
pages=$(vm_stat)
cpu=$(top -l 2 -s 1 -n 0)
printf '%s\n' "$pages" | awk -v total="$memory_total" '
    /page size of/ { page_size = $8 }
    /^Pages active:/ { active = $3 }
    /^Pages wired down:/ { wired = $4 }
    /^Pages occupied by compressor:/ { compressed = $5 }
    END {
        if (page_size <= 0 || total <= 0) exit 1
        used = (active + wired + compressed) * page_size
        if (used > total) used = total
        printf "memory.current=%.0f\nmemory.max=%.0f\n", used, total
    }'
printf '%s\n' "$cpu" | awk '
    /^CPU usage:/ { idle = $7; found = 1 }
    END {
        if (!found) exit 1
        printf "cpu.percent=%.2f\n", 100 - idle
    }'
printf 'logical.cores=%s\n' "$cores"
"#;

pub fn aws_allocated_capacity_command(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandSpec> {
    let TargetLocator::AwsEc2 { workspace, .. } = locator else {
        bail!("AWS allocated-capacity probes require an EC2 locator");
    };
    command_on_locator(
        locator,
        session_id,
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "{AWS_ALLOCATED_CAPACITY_SCRIPT}\n{}",
                mj_core::targets::storage::STORAGE_PROBE_SCRIPT
            ),
            "sh".into(),
            workspace.clone(),
        ]
        .into_iter()
        .chain(EC2_STORAGE_PATHS.iter().map(|path| (*path).to_owned()))
        .collect(),
        "sample EC2 allocated capacity",
    )
}

/// The storage lines of a probe's output, as one sample for `host`.
pub fn storage_samples(
    output: &[u8],
    host: &str,
) -> Vec<mj_core::targets::storage::HostStorageSample> {
    let (home, filesystems) = mj_core::targets::storage::parse_storage_lines(output);
    if filesystems.is_empty() {
        return Vec::new();
    }
    vec![mj_core::targets::storage::HostStorageSample {
        host: host.to_owned(),
        home,
        filesystems,
    }]
}

/// The home and the filesystems under `paths` on this machine: what the
/// daemon's capacity service and doctor both report for local targets.
/// Unix runs [`mj_core::targets::storage::STORAGE_PROBE_SCRIPT`] through
/// `executor`; Windows has no POSIX shell, so it asks its volume APIs.
pub fn measure_local_storage(
    paths: &[String],
    executor: &impl CommandExecutor,
) -> Result<(
    Option<String>,
    Vec<mj_core::targets::storage::FilesystemSpace>,
)> {
    #[cfg(unix)]
    {
        let mut probe = CommandSpec::new(
            "sh",
            [
                "-c",
                mj_core::targets::storage::STORAGE_PROBE_SCRIPT,
                "mj-storage",
            ],
        )
        .purpose("measure local free space");
        probe.args.extend(paths.iter().cloned());
        let output = executor.execute(&probe)?;
        ensure!(
            output.status == 0,
            "{} failed with status {}: {}",
            probe.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(mj_core::targets::storage::parse_storage_lines(
            &output.stdout,
        ))
    }
    #[cfg(windows)]
    {
        let _ = executor;
        Ok((
            None,
            mj_core::targets::storage::measure_windows_filesystems(paths),
        ))
    }
}

pub fn parse_host_capacity(output: &[u8], host: &str) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let total = parse_required_u64(&values, "memory.max")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: Some(parse_percent(required_value(&values, "cpu.percent")?)?),
        memory_used_bytes: parse_required_u64(&values, "memory.current")?,
        memory_total_bytes: total,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: None,
        storage: storage_samples(output, host),
    })
}

pub fn parse_aws_allocated_capacity(output: &[u8], host: &str) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let memory_total_bytes = parse_required_u64(&values, "memory.total")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: Some(parse_required_u64(&values, "disk.total")?),
        storage: storage_samples(output, host),
    })
}

pub(super) fn parse_key_values(output: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.trim().to_owned()))
        .collect()
}

pub(super) fn required_value<'a>(
    values: &'a BTreeMap<String, String>,
    key: &str,
) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("capacity probe did not expose {key}"))
}

pub(super) fn parse_required_u64(values: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    required_value(values, key)?
        .parse()
        .with_context(|| format!("capacity probe reported invalid {key}"))
}

pub(super) fn parse_percent(value: &str) -> Result<u8> {
    let value: f64 = value
        .parse()
        .with_context(|| format!("invalid percentage {value:?}"))?;
    if !value.is_finite() {
        bail!("invalid percentage {value:?}");
    }
    Ok(value.round().clamp(0.0, 100.0) as u8)
}

#[cfg(all(test, unix))]
mod darwin_tests {
    use super::*;

    #[test]
    fn darwin_capacity_uses_reported_page_size_and_last_cpu_sample() {
        for page_size in [4096, 16384] {
            let directory = tempfile::tempdir().unwrap();
            for (name, body) in [
                ("sysctl", "case $2 in hw.memsize) echo 17179869184;; hw.logicalcpu) echo 8;; *) exit 1;; esac".to_owned()),
                ("vm_stat", format!("printf '%s\\n' 'Mach Virtual Memory Statistics: (page size of {page_size} bytes)' 'Pages active: 100.' 'Pages inactive: 900.' 'Pages wired down: 200.' 'Pages occupied by compressor: 300.'")),
                ("top", "printf '%s\\n' 'CPU usage: 10.00% user, 10.00% sys, 80.00% idle' 'CPU usage: 20.00% user, 10.00% sys, 70.00% idle'".to_owned()),
            ] {
                mj_core::test_hooks::install_fake_command(directory.path(), name, &format!("#!/bin/sh\n{body}\n"));
            }
            let script = format!("set -eu\n{DARWIN_HOST_RESOURCE_USAGE_SCRIPT}");
            let mut command = CommandSpec::new("sh", ["-c", &script]);
            command.env.insert(
                "PATH".into(),
                format!("{}:/usr/bin:/bin", directory.path().display()),
            );
            let output = ProcessExecutor.execute(&command).unwrap();
            assert_eq!(
                output.status,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let usage = parse_host_capacity(&output.stdout, "mac").unwrap();
            assert_eq!(usage.memory_used_bytes, 600 * page_size);
            assert_eq!(usage.memory_total_bytes, 17_179_869_184);
            assert_eq!(usage.logical_cores, 8);
            assert_eq!(usage.cpu_percent, Some(30));

            mj_core::test_hooks::install_fake_command(
                directory.path(),
                "vm_stat",
                "#!/bin/sh\nexit 7\n",
            );
            assert_ne!(ProcessExecutor.execute(&command).unwrap().status, 0);
        }
    }
}
