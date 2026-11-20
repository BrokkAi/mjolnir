// Port of mj-worker/src/cpu_usage/linux.rs, plus the Linux branches of
// online_cpus and CpuCounter in mj-worker/src/cpu_usage.rs.

package worker

import (
	"errors"
	"fmt"
	"math/bits"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"
)

type procStat struct {
	parent uint32
	ticks  uint64
}

// parseProcStat reads the parent and the CPU counters (own and waited-for
// children) from one /proc/<pid>/stat text.
func parseProcStat(text string) (procStat, error) {
	end := strings.LastIndexByte(text, ')')
	if end < 0 {
		return procStat{}, errors.New("missing command name in /proc stat")
	}
	// The tail starts at field 3 (state); ppid is 4, CPU counters are 14..17.
	fields := strings.Fields(text[end+1:])
	if len(fields) < 15 {
		return procStat{}, errors.New("short /proc stat")
	}
	parent, err := strconv.ParseUint(fields[1], 10, 32)
	if err != nil {
		return procStat{}, err
	}
	var ticks uint64
	for _, field := range fields[11:15] {
		// Waited-for child counters are signed, though normally nonnegative.
		value, err := strconv.ParseInt(field, 10, 64)
		if err != nil {
			return procStat{}, err
		}
		sum, carry := bits.Add64(ticks, uint64(max(value, 0)), 0)
		if carry != 0 {
			sum = ^uint64(0)
		}
		ticks = sum
	}
	return procStat{parent: uint32(parent), ticks: ticks}, nil
}

func processTreeCpuTime(root uint32) (time.Duration, error) {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return 0, fmt.Errorf("list processes for CPU sampling: %w", err)
	}
	processes := map[uint32]procStat{}
	children := map[uint32][]uint32{}
	for _, entry := range entries {
		parsed, err := strconv.ParseUint(entry.Name(), 10, 32)
		if err != nil {
			continue
		}
		pid := uint32(parsed)
		text, err := os.ReadFile(filepath.Join("/proc", entry.Name(), "stat"))
		var stat procStat
		if err == nil {
			stat, err = parseProcStat(string(text))
		}
		if err != nil {
			if pid == root {
				return 0, fmt.Errorf("read worker CPU time: %w", err)
			}
			continue // Other processes may exit or be unreadable.
		}
		children[stat.parent] = append(children[stat.parent], pid)
		processes[pid] = stat
	}
	if _, ok := processes[root]; !ok {
		return 0, fmt.Errorf("worker process %d disappeared during CPU sampling", root)
	}
	pending := []uint32{root}
	visited := map[uint32]bool{}
	var ticks uint64
	for len(pending) > 0 {
		pid := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if visited[pid] {
			continue
		}
		visited[pid] = true
		if stat, ok := processes[pid]; ok {
			sum, carry := bits.Add64(ticks, stat.ticks, 0)
			if carry != 0 {
				sum = ^uint64(0)
			}
			ticks = sum
		}
		pending = append(pending, children[pid]...)
	}
	frequency := clockTicksPerSecond()
	if frequency <= 0 {
		return 0, errors.New("could not read CPU clock tick frequency")
	}
	return time.Duration(float64(ticks) / float64(frequency) * float64(time.Second)), nil
}

// clockTicksPerSecond is sysconf(_SC_CLK_TCK): the kernel's AT_CLKTCK
// auxiliary vector entry, or 100 when the kernel gave none, as glibc reads
// it (musl always answers 100).
func clockTicksPerSecond() int64 {
	const atClkTck = 17
	if vector, err := unix.Auxv(); err == nil {
		for _, entry := range vector {
			if entry[0] == atClkTck && entry[1] != 0 {
				return int64(entry[1])
			}
		}
	}
	return 100
}

// onlineCpus is sysconf(_SC_NPROCESSORS_ONLN): the host's online CPUs,
// independently of cgroup quotas. It reads what glibc reads, in its order:
// the kernel's online CPU list, then the CPU lines of /proc/stat, then the
// scheduler affinity.
func onlineCpus() (uint32, error) {
	count := linuxOnlineCpuList()
	if count <= 0 {
		count = linuxProcStatCpus()
	}
	if count <= 0 {
		var affinity unix.CPUSet
		if unix.SchedGetaffinity(0, &affinity) == nil {
			count = affinity.Count()
		}
	}
	if count <= 0 {
		return 0, errors.New("could not read the machine's online CPU count")
	}
	return uint32(count), nil
}

// linuxOnlineCpuList counts the CPUs in /sys/devices/system/cpu/online, a
// list of ranges such as "0-3,5"; zero when it cannot be read.
func linuxOnlineCpuList() int {
	text, err := os.ReadFile("/sys/devices/system/cpu/online")
	if err != nil {
		return 0
	}
	count := 0
	for _, item := range strings.Split(strings.TrimSpace(string(text)), ",") {
		low, high, isRange := strings.Cut(item, "-")
		first, err := strconv.Atoi(low)
		if err != nil {
			return 0
		}
		last := first
		if isRange {
			if last, err = strconv.Atoi(high); err != nil || last < first {
				return 0
			}
		}
		count += last - first + 1
	}
	return count
}

// linuxProcStatCpus counts the per-CPU lines of /proc/stat; zero when it
// cannot be read.
func linuxProcStatCpus() int {
	text, err := os.ReadFile("/proc/stat")
	if err != nil {
		return 0
	}
	count := 0
	for _, line := range strings.Split(string(text), "\n") {
		if rest, ok := strings.CutPrefix(line, "cpu"); ok && rest != "" && rest[0] >= '0' && rest[0] <= '9' {
			count++
		}
	}
	return count
}

// cgroupCpu is a fixed CPU counter selected from this worker's cgroup
// membership.
type cgroupCpu struct {
	path    string
	unified bool
}

var _ cpuCounter = cgroupCpu{}

func discoverCgroupCpu() (cgroupCpu, error) {
	membership, err := os.ReadFile("/proc/self/cgroup")
	if err != nil {
		return cgroupCpu{}, err
	}
	mounts, err := os.ReadFile("/proc/self/mountinfo")
	if err != nil {
		return cgroupCpu{}, err
	}
	return resolveCgroupCpu(string(membership), string(mounts))
}

// resolveCgroupCpu finds the counter for the cgroup named in membership
// (/proc/self/cgroup) under a mount in mounts (/proc/self/mountinfo).
func resolveCgroupCpu(membership string, mounts string) (cgroupCpu, error) {
	for _, line := range rustLines(membership) {
		fields := strings.SplitN(line, ":", 3)
		if len(fields) < 2 {
			return cgroupCpu{}, errors.New("missing cgroup controllers")
		}
		if len(fields) < 3 {
			return cgroupCpu{}, errors.New("missing cgroup path")
		}
		hierarchy, controllers, group := fields[0], fields[1], fields[2]
		unified := hierarchy == "0" && controllers == ""
		if !unified && !slices.Contains(strings.Split(controllers, ","), "cpuacct") {
			continue
		}
		if !strings.HasPrefix(group, "/") || slices.Contains(strings.Split(group, "/"), "..") {
			return cgroupCpu{}, errors.New("cgroup membership is outside the visible mount")
		}
		filesystem := "cgroup"
		counter := "cpuacct.usage"
		if unified {
			filesystem, counter = "cgroup2", "cpu.stat"
		}
		for _, mount := range rustLines(mounts) {
			left, right, ok := strings.Cut(mount, " - ")
			if !ok {
				continue
			}
			described := strings.Fields(right)
			if len(described) < 3 || described[0] != filesystem ||
				(!unified && !slices.Contains(strings.Split(described[2], ","), "cpuacct")) {
				continue
			}
			mountFields := strings.Fields(left)
			if len(mountFields) < 6 {
				return cgroupCpu{}, errors.New("short cgroup mountinfo record")
			}
			root, rootErr := cgroupMountPath(mountFields[3])
			target, targetErr := cgroupMountPath(mountFields[4])
			if rootErr != nil || targetErr != nil {
				continue
			}
			// The mount root is the base for paths shown in /proc/self/cgroup.
			// A nonmatching root can expose an ancestor mount whose counter
			// cannot identify this cgroup.
			relative, ok := cgroupStripPrefix(group, root)
			if !ok {
				continue
			}
			return cgroupCpu{path: filepath.Join(target, relative, counter), unified: unified}, nil
		}
	}
	return cgroupCpu{}, errors.New("no visible CPU accounting mount for worker cgroup")
}

// rustLines is str::lines: lines split at "\n", each without a trailing
// "\r", and no final empty line after a trailing newline.
func rustLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// cgroupStripPrefix is Path::strip_prefix for absolute Unix paths: the rest
// of path after base, compared by component.
func cgroupStripPrefix(path, base string) (string, bool) {
	components := func(text string) []string {
		var parts []string
		for _, part := range strings.Split(text, "/") {
			if part != "" && part != "." {
				parts = append(parts, part)
			}
		}
		return parts
	}
	pathParts, baseParts := components(path), components(base)
	if len(baseParts) > len(pathParts) || !slices.Equal(pathParts[:len(baseParts)], baseParts) {
		return "", false
	}
	return strings.Join(pathParts[len(baseParts):], "/"), true
}

func (c cgroupCpu) read() (time.Duration, error) {
	text, err := os.ReadFile(c.path)
	if err != nil {
		return 0, fmt.Errorf("read cgroup CPU counter %s: %w", c.path, err)
	}
	if !c.unified {
		nanos, err := strconv.ParseUint(strings.TrimSpace(string(text)), 10, 64)
		if err != nil {
			return 0, err
		}
		return time.Duration(nanos), nil
	}
	for _, line := range rustLines(string(text)) {
		fields := strings.Fields(line)
		if len(fields) >= 2 && fields[0] == "usage_usec" {
			micros, err := strconv.ParseUint(fields[1], 10, 64)
			if err != nil {
				return 0, err
			}
			return time.Duration(micros) * time.Microsecond, nil
		}
	}
	return 0, errors.New("cgroup cpu.stat has no usage_usec")
}

// cgroupMountPath decodes one octal-escaped mountinfo path field.
func cgroupMountPath(text string) (string, error) {
	var path []byte
	for i := 0; i < len(text); i++ {
		if text[i] != '\\' {
			path = append(path, text[i])
			continue
		}
		digits := text[i+1 : min(i+4, len(text))]
		i += len(digits)
		if len(digits) != 3 || strings.IndexFunc(digits, func(r rune) bool { return r < '0' || r > '7' }) >= 0 {
			return "", errors.New("invalid mountinfo path escape")
		}
		value := 0
		for _, digit := range digits {
			value = value*8 + int(digit-'0')
		}
		if value > 0xFF {
			return "", fmt.Errorf("out of range integral type conversion attempted")
		}
		path = append(path, byte(value))
	}
	decoded := string(path)
	if !strings.HasPrefix(decoded, "/") || slices.Contains(strings.Split(decoded, "/"), "..") {
		return "", errors.New("invalid cgroup mount path")
	}
	return decoded, nil
}

// discoverContainerCpuCounter is the cgroup counter of this worker's
// container.
func discoverContainerCpuCounter() (cpuCounter, error) {
	counter, err := discoverCgroupCpu()
	if err != nil {
		return nil, err
	}
	return counter, nil
}
