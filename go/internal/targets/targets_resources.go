// Port of mj-targets/src/targets/resources.rs.

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const hostResourceUsageScript = `
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
dmi=/sys/devices/virtual/dmi/id
if vendor=$(cat "$dmi/sys_vendor" 2>/dev/null) &&
   [ "$(printf '%s' "$vendor" | awk '{sub(/^[[:space:]]+/, ""); sub(/[[:space:]]+$/, ""); print}')" = 'Amazon EC2' ] &&
   product=$(cat "$dmi/product_name" 2>/dev/null) && [ -n "$product" ]; then
    printf 'ec2.instance_type=%s\n' "$product"
fi
`

const awsAllocatedCapacityScript = `
awk '/^MemTotal:/ { printf "memory.total=%.0f\n", $2 * 1024 }' /proc/meminfo
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
df -B1 -P -- "$1" | awk 'NR == 2 { print "disk.total=" $2 }'
`

// ec2StoragePaths is where Mjolnir writes on an EC2 instance besides its
// workspace.
var ec2StoragePaths = [5]string{
	core.RemoteWorkersDirectory,
	core.RemoteProfilesDirectory,
	core.RemoteCacheDirectory,
	core.DefaultBuildCacheDirectory,
	core.TemporaryDirectory,
}

// SSHHostCapacityCommand samples a host's CPU and memory, and the free space
// at each of storagePaths (relative paths are under the SSH user's home).
// One SSH command per host and poll: storage rides on the capacity probe
// rather than adding round trips.
func SSHHostCapacityCommand(ssh *core.SshTarget, storagePaths []string) core.CommandSpec {
	panic("unported: mj-targets/src/targets/resources.rs ssh_host_capacity_command")
}

// SSHNitroHostCapacityCommand is SSHHostCapacityCommand for a host that runs
// Nitro sandboxes: the same sample, then the engine's report appended as one
// `microvm.info=<json>` line.
//
// Sandbox memory comes from the pool reserved at boot, not from what the
// host has free, so the pool is the host's total for admission. The engine
// is optional here: before the first session installs it, this reports the
// host's own totals, and the next sample after that corrects them. A probe
// that required the engine would refuse the very launch that installs it.
// Only an absent engine is optional: an installed engine that cannot report
// fails the sample, rather than passing physical memory off as the pool.
func SSHNitroHostCapacityCommand(ssh *core.SshTarget, storagePaths []string) core.CommandSpec {
	panic("unported: mj-targets/src/targets/resources.rs ssh_nitro_host_capacity_command")
}

// vm_stat reports pages, whose size differs between Intel and Apple silicon.
// Count active, wired and compressed pages as used; inactive/speculative
// pages are reclaimable cache. top's second sample measures an interval, not
// uptime.

const darwinHostResourceUsageScript = `
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
`

func AWSAllocatedCapacityCommand(locator *core.TargetsTargetLocator, sessionID string) (core.CommandSpec, error) {
	panic("unported: mj-targets/src/targets/resources.rs aws_allocated_capacity_command")
}

// StorageSamples is the storage lines of a probe's output, as one sample for
// host.
func StorageSamples(output []byte, host string) []core.HostStorageSample {
	home, filesystems := core.ParseStorageLines(output)
	if len(filesystems) == 0 {
		return []core.HostStorageSample{}
	}
	return []core.HostStorageSample{{Host: host, Home: home, Filesystems: filesystems}}
}

// MeasureLocalStorage is the home and the filesystems under paths on this
// machine: what the daemon's capacity service and doctor both report for
// local targets. Unix runs core.StorageProbeScript through executor; Windows
// has no POSIX shell, so it asks its volume APIs.
func MeasureLocalStorage(paths []string, executor core.CommandExecutor) (*string, []core.FilesystemSpace, error) {
	panic("unported: mj-targets/src/targets/resources.rs measure_local_storage")
}

func ParseHostCapacity(output []byte, host string) (core.DeploymentCapacityUsage, error) {
	panic("unported: mj-targets/src/targets/resources.rs parse_host_capacity")
}

func ParseAWSAllocatedCapacity(output []byte, host string) (core.DeploymentCapacityUsage, error) {
	panic("unported: mj-targets/src/targets/resources.rs parse_aws_allocated_capacity")
}

func parseKeyValues(output []byte) map[string]string {
	panic("unported: mj-targets/src/targets/resources.rs parse_key_values")
}

func requiredValue(values map[string]string, key string) (string, error) {
	panic("unported: mj-targets/src/targets/resources.rs required_value")
}

func parseRequiredU64(values map[string]string, key string) (uint64, error) {
	panic("unported: mj-targets/src/targets/resources.rs parse_required_u64")
}

func parsePercent(value string) (uint8, error) {
	panic("unported: mj-targets/src/targets/resources.rs parse_percent")
}

func ValidateResourceAllocation(targetKind core.ResourceAllocationKind, allocation core.SessionResourceAllocation) error {
	panic("unported: mj-targets/src/targets/resources.rs validate_resource_allocation")
}
