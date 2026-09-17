use super::*;

pub(super) const CGROUP_RESOURCE_USAGE_SCRIPT: &str = r#"
for file in memory.current memory.max memory.swap.current memory.swap.max; do
    path="/sys/fs/cgroup/$file"
    if [ -r "$path" ]; then
        printf "%s=%s\n" "$file" "$(cat "$path")"
    fi
done
if [ -r /sys/fs/cgroup/cpu.stat ]; then
    before=$(awk '/^usage_usec / { print $2 }' /sys/fs/cgroup/cpu.stat)
    sleep 0.25
    after=$(awk '/^usage_usec / { print $2 }' /sys/fs/cgroup/cpu.stat)
    set -- $(cat /sys/fs/cgroup/cpu.max 2>/dev/null || printf 'max 100000')
    if [ "$1" = max ]; then
        cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')
    else
        cores=$(awk -v quota="$1" -v period="$2" 'BEGIN { print quota / period }')
    fi
    awk -v used="$((after - before))" -v cores="$cores" \
        'BEGIN { if (cores > 0) printf "cpu.percent=%.0f\n", used / 250000 / cores * 100 }'
fi
"#;

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

// `du` is run on its own so a path it cannot measure fails the probe instead of
// being silently dropped from the total: a session that reports less disk than
// it uses is worse than one that reports none. Its stderr is deliberately left
// attached, so the caller's failure message names the path that could not be
// read.
pub(super) const AWS_SESSION_DISK_USAGE_SCRIPT: &str = r#"
usage=$(du -sk "$@") || exit 1
printf '%s\n' "$usage" | awk '{ total += $1 * 1024 } END { print total + 0 }'
"#;

pub fn resource_probe(locator: &TargetLocator, session_id: &str) -> Result<SessionResourceProbe> {
    verify_locator(locator, session_id)?;
    let (memory, disk) = match locator {
        TargetLocator::LocalPodman { container_id, .. } => (
            container_exec(
                "podman",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample local Podman container resources"),
            Some(
                CommandSpec::new(
                    "podman",
                    [
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample local Podman container writable disk"),
            ),
        ),
        TargetLocator::LocalDocker { container_id, .. } => (
            container_exec(
                "docker",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample local Docker container resources"),
            Some(
                CommandSpec::new(
                    "docker",
                    [
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample local Docker container writable disk"),
            ),
        ),
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker {
            ssh, container_id, ..
        } => (
            ssh_command(
                ssh,
                [
                    locator.container_engine().expect("remote container"),
                    "exec",
                    container_id,
                    "sh",
                    "-c",
                    CGROUP_RESOURCE_USAGE_SCRIPT,
                ],
            )
            .purpose("sample remote container resources"),
            Some(
                ssh_command(
                    ssh,
                    [
                        locator.container_engine().expect("remote container"),
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample remote container writable disk"),
            ),
        ),
        TargetLocator::AwsEc2 { ssh, workspace, .. } => {
            let worker_root = worker_root(locator, session_id)?;
            let profile_root = format!(".local/share/hel/profiles/{session_id}");
            (
                ssh_command(ssh, ["sh", "-c", HOST_RESOURCE_USAGE_SCRIPT])
                    .purpose("sample EC2 session resources"),
                Some(
                    ssh_command(
                        ssh,
                        [
                            "sh",
                            "-c",
                            AWS_SESSION_DISK_USAGE_SCRIPT,
                            "sh",
                            workspace.as_str(),
                            worker_root.as_str(),
                            profile_root.as_str(),
                        ],
                    )
                    .purpose("sample EC2 session disk"),
                ),
            )
        }
        TargetLocator::AppleContainer { container_id, .. } => (
            container_exec(
                "container",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample Apple container resources"),
            None,
        ),
        TargetLocator::LocalBare { .. } | TargetLocator::SshBare { .. } => {
            bail!("resource sampling is unsupported for this target")
        }
    };
    Ok(SessionResourceProbe { memory, disk })
}

pub fn parse_resource_usage(
    memory_output: &[u8],
    disk_output: Option<&[u8]>,
) -> Result<SessionResourceUsage> {
    let mut values = BTreeMap::new();
    let memory_text = String::from_utf8_lossy(memory_output);
    for line in memory_text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(name, value.trim());
    }

    let memory_current_bytes = parse_cgroup_counter(
        values
            .get("memory.current")
            .context("resource probe did not expose memory.current")?,
    )?
    .context("resource probe reported memory.current as unlimited")?;
    let memory_limit_bytes = values
        .get("memory.max")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let swap_current_bytes = values
        .get("memory.swap.current")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let swap_limit_bytes = values
        .get("memory.swap.max")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let writable_disk_bytes = disk_output.map(parse_disk_usage).transpose()?;
    let cpu_percent = values
        .get("cpu.percent")
        .map(|value| parse_percent(value))
        .transpose()?;

    Ok(SessionResourceUsage {
        cpu_percent,
        memory_current_bytes,
        memory_limit_bytes,
        swap_current_bytes,
        swap_limit_bytes,
        writable_disk_bytes,
    })
}

/// Read the single byte count every writable-disk probe answers with.
///
/// A probe that ran and answered something else measured nothing, which must be
/// reported as a failure rather than silently becoming "disk usage unknown":
/// only a probe that was never run leaves the value unknown.
pub(super) fn parse_disk_usage(output: &[u8]) -> Result<u64> {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    text.parse()
        .with_context(|| format!("disk usage probe answered {text:?} instead of a byte count"))
}

pub fn ssh_host_capacity_command(ssh: &SshTarget) -> CommandSpec {
    ssh_command(ssh, ["sh", "-c", HOST_RESOURCE_USAGE_SCRIPT])
        .purpose("sample deployment host capacity")
}

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
            AWS_ALLOCATED_CAPACITY_SCRIPT.into(),
            "sh".into(),
            workspace.clone(),
        ],
        "sample EC2 allocated capacity",
    )
}

pub fn parse_host_capacity(output: &[u8]) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let total = parse_required_u64(&values, "memory.max")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: Some(parse_percent(required_value(&values, "cpu.percent")?)?),
        memory_used_bytes: parse_required_u64(&values, "memory.current")?,
        memory_total_bytes: total,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: None,
    })
}

pub fn parse_aws_allocated_capacity(output: &[u8]) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let memory_total_bytes = parse_required_u64(&values, "memory.total")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: Some(parse_required_u64(&values, "disk.total")?),
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

pub(super) fn parse_cgroup_counter(value: &str) -> Result<Option<u64>> {
    if value == "max" {
        return Ok(None);
    }
    Ok(Some(value.parse().with_context(|| {
        format!("invalid memory counter {value:?}")
    })?))
}
