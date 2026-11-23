// Port of mj-worker/src/cpu_usage.rs.

// Worker-owned CPU measurement and history; survives controller replacement.

package worker

import (
	"context"
	"errors"
	"fmt"
	"math"
	"os"
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
	if onlineCpus == 0 {
		return
	}
	if !s.hasLast {
		s.hasLast, s.lastAt, s.lastCPU = true, at, cpuTime
		return
	}
	dt := max(at.Sub(s.lastAt), 0)
	if dt < time.Second {
		return
	}
	lastCPU := s.lastCPU
	s.lastAt, s.lastCPU = at, cpuTime
	seconds := dt.Seconds()
	share := min(max(max(cpuTime-lastCPU, 0).Seconds()/(seconds*float64(onlineCpus)), 0), 1)
	decay := math.Exp(-seconds / 3600)
	s.weightedShare = s.weightedShare*decay + share*seconds
	s.weight = s.weight*decay + seconds
	s.covered = min(s.covered+dt, time.Hour)
	scope := s.scope
	if scope == "" {
		scope = core.CpuSampleScopeProcessTree
	}
	s.latest = &core.SessionCpuUsage{
		Scope:             scope,
		RecentPermille:    saturatingUint16(math.Round(share * 1000)),
		HourlyPermille:    saturatingUint16(math.Round(s.weightedShare / s.weight * 1000)),
		HourlyCoveredSecs: uint32(s.covered / time.Second),
		OnlineCpus:        onlineCpus,
	}
}

// saturatingUint16 is Rust's `as u16` from a float: it saturates at the
// bounds, and NaN becomes zero.
func saturatingUint16(value float64) uint16 {
	switch {
	case math.IsNaN(value) || value <= 0:
		return 0
	case value >= math.MaxUint16:
		return math.MaxUint16
	}
	return uint16(value)
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
	scope := core.CpuSampleScopeProcessTree
	if !accounting.IsProcessTree() {
		scope = core.CpuSampleScopeContainer
	}
	sampler := CpuSampler{scope: scope}
	// Discovery and every reading are blocking file reads; a panic in one is
	// reported as the measurement's failure rather than lost.
	counter, err := core.SpawnBlocking(func() cpuCounterDiscovery {
		counter, err := discoverCpuCounter(accounting)
		return cpuCounterDiscovery{counter: counter, err: err}
	}).Join(ctx)
	if ctx.Err() != nil {
		return
	}
	var counterErr error
	switch {
	case err != nil:
		counterErr = fmt.Errorf("CPU counter discovery task failed: %w", err)
	case counter.err != nil:
		counterErr = errors.New(counter.err.Error())
	}
	ticks := time.NewTicker(10 * time.Second)
	defer ticks.Stop()
	// tokio's interval ticks at once; later ticks a slow measurement missed
	// are skipped, as Go's ticker drops them.
	for first := true; ; first = false {
		if !first {
			select {
			case <-ticks.C:
			case <-ctx.Done():
				return
			}
		}
		reading, err := core.SpawnBlocking(func() cpuReading {
			if counterErr != nil {
				return cpuReading{err: counterErr}
			}
			at := time.Now()
			cpu, err := counter.counter.read()
			if err != nil {
				return cpuReading{err: err}
			}
			count, err := OnlineCpus()
			if err != nil {
				return cpuReading{err: err}
			}
			return cpuReading{at: at, cpu: cpu, onlineCpus: count}
		}).Join(ctx)
		if ctx.Err() != nil {
			return
		}
		var value cpuRead
		switch {
		case err != nil:
			value.Err = fmt.Errorf("CPU measurement task failed: %w", err)
		case reading.err != nil:
			value.Err = reading.err
		default:
			sampler.Observe(reading.at, reading.cpu, reading.onlineCpus)
			value.Usage = sampler.Latest()
		}
		sender.SendReplace(value)
	}
}

// cpuCounterDiscovery is the outcome of discovering the CPU counter.
type cpuCounterDiscovery struct {
	counter cpuCounter
	err     error
}

// cpuReading is one cumulative CPU reading, or why it failed.
type cpuReading struct {
	at         time.Time
	cpu        time.Duration
	onlineCpus uint32
	err        error
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
	if accounting.IsProcessTree() {
		return cpuCounterProcessTree{root: uint32(os.Getpid())}, nil
	}
	return discoverContainerCpuCounter()
}
