// Port of mj-worker/src/cpu_usage.rs.

// Worker-owned CPU measurement and history; survives controller replacement.

package worker

import (
	"context"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProcessTreeCpuTime is the CPU time used by root and every process
// descending from it, including reaped children.
func ProcessTreeCpuTime(root uint32) (time.Duration, error) {
	return processTreeCpuTime(root)
}

// OnlineCpus is the host's online CPU count, independently of cgroup quotas.
func OnlineCpus() (uint32, error) {
	return onlineCpus()
}

// CpuSampler turns cumulative CPU time readings into a recent share and a
// one-hour decaying average. The zero value samples the process tree.
type CpuSampler struct {
	// hasLast is false until the first reading (Rust's last: None).
	hasLast       bool
	lastAt        time.Time
	lastCPU       time.Duration
	weightedShare float64
	weight        float64
	covered       time.Duration
	latest        *core.SessionCpuUsage
	// scope is the physical scope reported in samples; empty is the default,
	// core.CpuSampleScopeProcessTree.
	scope core.CpuSampleScope
}

// Observe records one cumulative CPU reading taken at at. Readings less than
// a second after the previous one are ignored.
func (s *CpuSampler) Observe(at time.Time, cpuTime time.Duration, onlineCpus uint32) {
	panic("unported: mj-worker/src/cpu_usage.rs CpuSampler::observe")
}

// Latest is the newest sample; nil until two readings a second apart.
func (s *CpuSampler) Latest() *core.SessionCpuUsage {
	if s.latest == nil {
		return nil
	}
	latest := *s.latest
	return &latest
}

// cpuRead is one result of the worker's CPU sampler: the latest usage (nil
// until there is one) or why the measurement failed (Rust's
// Result<Option<SessionCpuUsage>, String>).
type cpuRead struct {
	Usage *core.SessionCpuUsage
	Err   error
}

// sampleCpu is the one sampler per serving worker; the serving loop
// supervises it. It publishes a cpuRead to sender every ten seconds until
// ctx ends.
func sampleCpu(ctx context.Context, sender *core.Watch[cpuRead], accounting core.CpuAccountingMode) {
	panic("unported: mj-worker/src/cpu_usage.rs sample_cpu")
}

// cpuCounter is a cumulative CPU time source for one accounting mode.
type cpuCounter interface {
	read() (time.Duration, error)
}

// cpuCounterProcessTree counts the worker's process tree.
type cpuCounterProcessTree struct {
	root uint32
}

func (c cpuCounterProcessTree) read() (time.Duration, error) {
	return ProcessTreeCpuTime(c.root)
}

// discoverCpuCounter selects the counter for accounting; the container
// counter exists only on Linux.
func discoverCpuCounter(accounting core.CpuAccountingMode) (cpuCounter, error) {
	panic("unported: mj-worker/src/cpu_usage.rs CpuCounter::discover")
}
