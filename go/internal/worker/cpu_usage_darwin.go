// Port of mj-worker/src/cpu_usage/macos.rs, plus the macOS branches of
// online_cpus and CpuCounter in mj-worker/src/cpu_usage.rs.

package worker

import (
	"errors"
	"time"
)

// darwinChildPids lists pid's children, growing the buffer until the result
// fits, since a process can spawn children between two calls.
func darwinChildPids(pid int32) ([]int32, error) {
	panic("unported: mj-worker/src/cpu_usage/macos.rs child_pids")
}

// processTreeCpuTime sums `rusage_info_v2` CPU time (own and children's),
// converted from Mach absolute-time ticks with the host's time base.
func processTreeCpuTime(root uint32) (time.Duration, error) {
	panic("unported: mj-worker/src/cpu_usage/macos.rs process_tree_cpu_time")
}

// onlineCpus is sysconf(_SC_NPROCESSORS_ONLN): the host's online CPUs.
func onlineCpus() (uint32, error) {
	panic("unported: mj-worker/src/cpu_usage.rs online_cpus")
}

func discoverContainerCpuCounter() (cpuCounter, error) {
	return nil, errors.New("container CPU sampling is not supported on this platform")
}
