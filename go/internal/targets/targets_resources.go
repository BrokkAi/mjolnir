// Port of mj-targets/src/targets/resources.rs.

package targets

import (
	"errors"
	"fmt"
	"math"
	"strconv"
	"strings"

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
	storage := core.StorageProbeScript
	// The storage loop reads the paths from the arguments; the resource
	// scripts read an optional /proc override from `$1`, so clear them first.
	script := fmt.Sprintf("set -eu\n%s\nset --\ncase $(uname -s) in\nDarwin)\n%s\n;;\nLinux)\n%s\n;;\n*) echo 'unsupported host operating system for capacity sampling' >&2; exit 1;;\nesac",
		storage, darwinHostResourceUsageScript, hostResourceUsageScript)
	words := []string{"sh", "-c", script, "mj-capacity"}
	words = append(words, storagePaths...)
	return core.SSHCommand(core.SshTrafficClassControl, ssh, words).WithPurpose("sample deployment host capacity")
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
	command := SSHHostCapacityCommand(ssh, storagePaths)
	engine := core.EngineShellWords(core.MicrovmEngine)
	if len(command.Args) == 0 {
		panic("an SSH command ends with the remote command line")
	}
	last := len(command.Args) - 1
	command.Args[last] = fmt.Sprintf(`%s && if [ -e "$HOME/%s" ]; then info=$(%s info --format json) || exit 1; printf 'microvm.info=%%s\n' "$(printf '%%s' "$info" | tr -d '\n')"; fi`,
		command.Args[last], core.MicrovmEngineWorker, engine)
	return command
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
	if locator.Kind != core.TargetsTargetLocatorAwsEc2 {
		return core.CommandSpec{}, errors.New("AWS allocated-capacity probes require an EC2 locator")
	}
	args := []string{
		"sh", "-c",
		awsAllocatedCapacityScript + "\n" + core.StorageProbeScript,
		"sh", locator.Workspace,
	}
	args = append(args, ec2StoragePaths[:]...)
	return core.CommandOnLocator(core.SshTrafficClassControl, locator, sessionID, args, "sample EC2 allocated capacity")
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
	return measureLocalStorage(paths, executor)
}

// measureUnixStorage runs core.StorageProbeScript over paths through
// executor (Rust's Unix branch of measure_local_storage).
func measureUnixStorage(paths []string, executor core.CommandExecutor) (*string, []core.FilesystemSpace, error) {
	probe := core.NewCommandSpec("sh", []string{"-c", core.StorageProbeScript, "mj-storage"}).
		WithPurpose("measure local free space")
	probe.Args = append(probe.Args, paths...)
	output, err := executor.Execute(&probe)
	if err != nil {
		return nil, nil, err
	}
	if output.Status != 0 {
		return nil, nil, fmt.Errorf("%s failed with status %d: %s", probe.Purpose, output.Status, strings.TrimSpace(lossyText(output.Stderr)))
	}
	home, filesystems := core.ParseStorageLines(output.Stdout)
	return home, filesystems, nil
}

func ParseHostCapacity(output []byte, host string) (core.DeploymentCapacityUsage, error) {
	values := parseKeyValues(output)
	total, err := parseRequiredU64(values, "memory.max")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	used, err := parseRequiredU64(values, "memory.current")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	info, memoryPool := values["microvm.info"]
	var physicalMemory *core.HostMemoryUsage
	if memoryPool {
		physicalMemory = &core.HostMemoryUsage{UsedBytes: used, TotalBytes: total}
		if used, total, err = nitroPoolMemory(info); err != nil {
			return core.DeploymentCapacityUsage{}, err
		}
	}
	var instanceType *string
	if value, ok := values["ec2.instance_type"]; ok && value != "" {
		instanceType = &value
	}
	cpuText, err := requiredValue(values, "cpu.percent")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	cpu, err := parsePercent(cpuText)
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	cores, err := parseRequiredU64(values, "logical.cores")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	return core.DeploymentCapacityUsage{
		EC2InstanceType:  instanceType,
		PhysicalMemory:   physicalMemory,
		MemoryPool:       memoryPool,
		CPUPercent:       &cpu,
		MemoryUsedBytes:  used,
		MemoryTotalBytes: total,
		LogicalCores:     cores,
		Storage:          StorageSamples(output, host),
	}, nil
}

func ParseAWSAllocatedCapacity(output []byte, host string) (core.DeploymentCapacityUsage, error) {
	values := parseKeyValues(output)
	memoryTotal, err := parseRequiredU64(values, "memory.total")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	cores, err := parseRequiredU64(values, "logical.cores")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	disk, err := parseRequiredU64(values, "disk.total")
	if err != nil {
		return core.DeploymentCapacityUsage{}, err
	}
	return core.DeploymentCapacityUsage{
		MemoryTotalBytes: memoryTotal,
		LogicalCores:     cores,
		DiskTotalBytes:   &disk,
		Storage:          StorageSamples(output, host),
	}, nil
}

func parseKeyValues(output []byte) map[string]string {
	values := map[string]string{}
	for _, line := range libLines(lossyText(output)) {
		if key, value, ok := strings.Cut(line, "="); ok {
			values[key] = strings.TrimSpace(value)
		}
	}
	return values
}

func requiredValue(values map[string]string, key string) (string, error) {
	value, ok := values[key]
	if !ok {
		return "", fmt.Errorf("capacity probe did not expose %s", key)
	}
	return value, nil
}

func parseRequiredU64(values map[string]string, key string) (uint64, error) {
	text, err := requiredValue(values, key)
	if err != nil {
		return 0, err
	}
	value, err := libParseU64(text)
	if err != nil {
		return 0, fmt.Errorf("capacity probe reported invalid %s: %w", key, err)
	}
	return value, nil
}

func parsePercent(value string) (uint8, error) {
	parsed, err := strconv.ParseFloat(value, 64)
	if err != nil && !errors.Is(err, strconv.ErrRange) {
		return 0, fmt.Errorf("invalid percentage %s: invalid float literal", core.RustDebugString(value))
	}
	if math.IsInf(parsed, 0) || math.IsNaN(parsed) {
		// Rust formats the parsed float here: inf, -inf or NaN.
		text := "NaN"
		switch {
		case math.IsInf(parsed, 1):
			text = "inf"
		case math.IsInf(parsed, -1):
			text = "-inf"
		}
		return 0, fmt.Errorf("invalid percentage %s", text)
	}
	return uint8(min(max(math.Round(parsed), 0), 100)), nil
}

func ValidateResourceAllocation(targetKind core.ResourceAllocationKind, allocation core.SessionResourceAllocation) error {
	if allocation != nil {
		if err := allocation.Validate(); err != nil {
			return err
		}
	}
	switch allocation.(type) {
	case nil:
		return nil
	case core.SessionResourceAllocationContainer:
		if targetKind == core.ResourceAllocationKindContainer {
			return nil
		}
	case core.SessionResourceAllocationAwsEc2:
		if targetKind == core.ResourceAllocationKindAwsEc2 {
			return nil
		}
	}
	if targetKind == core.ResourceAllocationKindFixed {
		return errors.New(core.BareTargetFixedResources)
	}
	return errors.New("resource allocation does not match the selected target kind")
}
