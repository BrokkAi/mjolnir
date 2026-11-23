// Port of mj-github-relevance/src/github_student/inference.rs: sysinfo's
// System::available_memory on Linux, which parallelism reads: MemAvailable
// from /proc/meminfo, or the older estimate on kernels before 3.14 that
// lack it.

package githubrelevance

import (
	"bufio"
	"os"
	"strconv"
	"strings"
)

func availableMemory() (uint64, error) {
	file, err := os.Open("/proc/meminfo")
	if err != nil {
		// sysinfo leaves the figure at zero when the table is unreadable.
		return 0, nil
	}
	defer file.Close()
	fields := map[string]uint64{}
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		key, value, ok := strings.Cut(scanner.Text(), ":")
		if !ok {
			continue
		}
		number := strings.TrimSpace(strings.TrimSuffix(strings.TrimSpace(value), "kB"))
		kib, err := strconv.ParseUint(number, 10, 64)
		if err != nil {
			continue
		}
		// /proc/meminfo reports KiB, though it says "kB".
		fields[key] = saturatingMul(kib, 1024)
	}
	if available, ok := fields["MemAvailable"]; ok {
		return available, nil
	}
	estimate := saturatingAdd(saturatingAdd(saturatingAdd(fields["MemFree"], fields["Buffers"]), fields["Cached"]), fields["SReclaimable"])
	if estimate < fields["Shmem"] {
		return 0, nil
	}
	return estimate - fields["Shmem"], nil
}

func saturatingAdd(a, b uint64) uint64 {
	if a+b < a {
		return ^uint64(0)
	}
	return a + b
}

func saturatingMul(a, b uint64) uint64 {
	if a != 0 && (a*b)/a != b {
		return ^uint64(0)
	}
	return a * b
}
