// Port of the tests in mj-targets/src/targets/resources.rs.

package targets

import (
	"fmt"
	"path/filepath"
	"runtime"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestDarwinCapacityUsesReportedPageSizeAndLastCPUSample(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only, as in Rust")
	}
	for _, pageSize := range []uint64{4096, 16384} {
		directory := t.TempDir()
		for _, fake := range []struct{ name, body string }{
			{"sysctl", "case $2 in hw.memsize) echo 17179869184;; hw.logicalcpu) echo 8;; *) exit 1;; esac"},
			{"vm_stat", fmt.Sprintf("printf '%%s\\n' 'Mach Virtual Memory Statistics: (page size of %d bytes)' 'Pages active: 100.' 'Pages inactive: 900.' 'Pages wired down: 200.' 'Pages occupied by compressor: 300.'", pageSize)},
			{"top", "printf '%s\\n' 'CPU usage: 10.00% user, 10.00% sys, 80.00% idle' 'CPU usage: 20.00% user, 10.00% sys, 70.00% idle'"},
		} {
			core.InstallFakeCommand(directory, fake.name, "#!/bin/sh\n"+fake.body+"\n")
		}
		script := "set -eu\n" + darwinHostResourceUsageScript
		command := core.NewCommandSpec("sh", []string{"-c", script})
		command.Env["PATH"] = filepath.ToSlash(directory) + ":/usr/bin:/bin"
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if output.Status != 0 {
			t.Fatalf("status %d: %s", output.Status, output.Stderr)
		}
		usage, err := ParseHostCapacity(output.Stdout, "mac")
		if err != nil {
			t.Fatal(err)
		}
		if usage.MemoryUsedBytes != 600*pageSize {
			t.Fatalf("memory used = %d, want %d", usage.MemoryUsedBytes, 600*pageSize)
		}
		if usage.MemoryTotalBytes != 17_179_869_184 {
			t.Fatalf("memory total = %d", usage.MemoryTotalBytes)
		}
		if usage.LogicalCores != 8 {
			t.Fatalf("logical cores = %d", usage.LogicalCores)
		}
		if usage.CPUPercent == nil || *usage.CPUPercent != 30 {
			t.Fatalf("cpu percent = %v", usage.CPUPercent)
		}

		core.InstallFakeCommand(directory, "vm_stat", "#!/bin/sh\nexit 7\n")
		output, err = core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if output.Status == 0 {
			t.Fatal("a failing vm_stat still produced a sample")
		}
	}
}

func TestHostCapacityAcceptsOptionalEc2InstanceType(t *testing.T) {
	sample := "memory.max=68719476736\nmemory.current=8589934592\ncpu.percent=20\nlogical.cores=16\n"
	legacy, err := ParseHostCapacity([]byte(sample), "remote")
	if err != nil {
		t.Fatal(err)
	}
	if legacy.EC2InstanceType != nil {
		t.Fatalf("instance type = %q", *legacy.EC2InstanceType)
	}
	output := sample + "ec2.instance_type= c8id.4xlarge \n"
	sampled, err := ParseHostCapacity([]byte(output), "remote")
	if err != nil {
		t.Fatal(err)
	}
	if sampled.EC2InstanceType == nil || *sampled.EC2InstanceType != "c8id.4xlarge" {
		t.Fatalf("instance type = %v", sampled.EC2InstanceType)
	}
	if sampled.HostMemory() != legacy.HostMemory() {
		t.Fatalf("host memory %+v != %+v", sampled.HostMemory(), legacy.HostMemory())
	}
}
