// Port of the tests of mj-worker/src/cpu_usage.rs.

package worker

import (
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestCpuSamplerHourlyAverageIsNotDilutedAtStart(t *testing.T) {
	var sampler CpuSampler
	at := time.Now()
	for n := range 6 {
		sampler.Observe(at.Add(time.Duration(n*10)*time.Second), time.Duration(n*20)*time.Second, 4)
	}
	usage := sampler.Latest()
	if usage == nil {
		t.Fatal("no sample")
	}
	if usage.HourlyPermille != 500 || usage.HourlyCoveredSecs != 50 || usage.Scope != core.CpuSampleScopeProcessTree {
		t.Fatalf("%+v", *usage)
	}
}

func TestCpuSamplerTreatsAFallingCounterAsIdle(t *testing.T) {
	var sampler CpuSampler
	at := time.Now()
	sampler.Observe(at, 20*time.Second, 1)
	sampler.Observe(at.Add(10*time.Second), 10*time.Second, 1)
	if got := sampler.Latest().RecentPermille; got != 0 {
		t.Fatalf("recent = %d", got)
	}
	sampler.Observe(at.Add(20*time.Second), 15*time.Second, 1)
	if got := sampler.Latest().RecentPermille; got != 500 {
		t.Fatalf("recent = %d", got)
	}
}

func TestCpuSamplerIgnoresShortIntervalsAndCapsCoverage(t *testing.T) {
	var sampler CpuSampler
	at := time.Now()
	sampler.Observe(at, 0, 1)
	sampler.Observe(at.Add(500*time.Millisecond), time.Second, 1)
	if sampler.Latest() != nil {
		t.Fatal("a short interval must not produce a sample")
	}
	sampler.Observe(at.Add(4000*time.Second), 8000*time.Second, 1)
	usage := sampler.Latest()
	if usage.RecentPermille != 1000 || usage.HourlyCoveredSecs != 3600 {
		t.Fatalf("%+v", *usage)
	}
}
