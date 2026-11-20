// Port of mj-worker/src/cpu_usage/linux.rs, plus the Linux branches of
// online_cpus and CpuCounter in mj-worker/src/cpu_usage.rs.

package worker

import (
	"time"
)

type procStat struct {
	parent uint32
	ticks  uint64
}

// parseProcStat reads the parent and the CPU counters (own and waited-for
// children) from one /proc/<pid>/stat text.
func parseProcStat(text string) (procStat, error) {
	panic("unported: mj-worker/src/cpu_usage/linux.rs parse_proc_stat")
}

func processTreeCpuTime(root uint32) (time.Duration, error) {
	panic("unported: mj-worker/src/cpu_usage/linux.rs process_tree_cpu_time")
}

// onlineCpus is sysconf(_SC_NPROCESSORS_ONLN): the host's online CPUs,
// independently of cgroup quotas.
func onlineCpus() (uint32, error) {
	panic("unported: mj-worker/src/cpu_usage.rs online_cpus")
}

// cgroupCpu is a fixed CPU counter selected from this worker's cgroup
// membership.
type cgroupCpu struct {
	path    string
	unified bool
}

var _ cpuCounter = cgroupCpu{}

func discoverCgroupCpu() (cgroupCpu, error) {
	panic("unported: mj-worker/src/cpu_usage/linux.rs CgroupCpu::discover")
}

// resolveCgroupCpu finds the counter for the cgroup named in membership
// (/proc/self/cgroup) under a mount in mounts (/proc/self/mountinfo).
func resolveCgroupCpu(membership string, mounts string) (cgroupCpu, error) {
	panic("unported: mj-worker/src/cpu_usage/linux.rs CgroupCpu::resolve")
}

func (c cgroupCpu) read() (time.Duration, error) {
	panic("unported: mj-worker/src/cpu_usage/linux.rs CgroupCpu::read")
}

// cgroupMountPath decodes one octal-escaped mountinfo path field.
func cgroupMountPath(text string) (string, error) {
	panic("unported: mj-worker/src/cpu_usage/linux.rs mount_path")
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
