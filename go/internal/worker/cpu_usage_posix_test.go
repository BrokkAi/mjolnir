// Port of the `cfg(any(target_os = "linux", target_os = "macos"))` tests of
// mj-worker/src/cpu_usage.rs.

//go:build linux || darwin

package worker

import (
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestProcessTreeCpuTimeCountsABusyChildAfterItIsReaped(t *testing.T) {
	const child = "MJ_CPU_REAPED_TEST_CHILD"
	const burn = "MJ_CPU_REAPED_TEST_BURN"
	// A loaded runner slows the wall clock, not the child's own CPU use, so
	// waiting for this much CPU keeps the lower bound valid under parallel
	// test load instead of scaling it with the clock.
	const childCPU = 1200 * time.Millisecond
	if _, ok := os.LookupEnv(burn); ok {
		burnOwnCPU(t, childCPU)
		return
	}
	directory := t.TempDir()
	if _, ok := os.LookupEnv(child); !ok {
		// The counter includes every thread and descendant of this process.
		// Run alone so parallel worker tests cannot contribute CPU time.
		output, err := core.RunWithInput(cpuTestCommand(directory, child), nil)
		if err != nil {
			t.Fatal(err)
		}
		if !output.Status.Success() {
			t.Fatalf("%s\n%s", fromUTF8Lossy(output.Stdout), fromUTF8Lossy(output.Stderr))
		}
		return
	}
	before, err := ProcessTreeCpuTime(uint32(os.Getpid()))
	if err != nil {
		t.Fatal(err)
	}
	// A separate process burns the CPU, so this process counts it only
	// through the reaped-child accounting of the process tree.
	output, err := core.RunWithInput(cpuTestCommand(directory, burn), nil)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("%s\n%s", fromUTF8Lossy(output.Stdout), fromUTF8Lossy(output.Stderr))
	}
	after, err := ProcessTreeCpuTime(uint32(os.Getpid()))
	if err != nil {
		t.Fatal(err)
	}
	used := max(after-before, 0)
	if used < time.Second || used >= 5*time.Second {
		t.Fatalf("used = %s", used)
	}
}

// cpuTestCommand runs this test alone in a fresh test binary with variable
// set, in an isolated instance.
func cpuTestCommand(directory string, variable string) *exec.Cmd {
	command := exec.Command(os.Args[0],
		"-test.run=^TestProcessTreeCpuTimeCountsABusyChildAfterItIsReaped$",
		"-test.count=1",
		"-test.v",
	)
	command.Env = append(os.Environ(),
		variable+"=1",
		"MJ_INSTANCE=cpu-counter-test",
		"MJ_DATA_DIR="+filepath.Join(directory, "data"),
		"MJ_CONFIG_DIR="+filepath.Join(directory, "config"),
	)
	return command
}

// burnOwnCPU spends target of this process's own CPU, however slowly the
// scheduler runs it.
func burnOwnCPU(t *testing.T, target time.Duration) {
	start := ownCPUTime(t)
	state := uint64(0)
	for ownCPUTime(t)-start < target {
		for step := range uint64(1_000_000) {
			state = state*6_364_136_223_846_793_005 + step
		}
		runtime.KeepAlive(state)
	}
}

func ownCPUTime(t *testing.T) time.Duration {
	var usage syscall.Rusage
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &usage); err != nil {
		t.Fatalf("read own CPU time: %v", err)
	}
	return time.Duration(usage.Utime.Nano() + usage.Stime.Nano())
}
