// Port of the tests of mj-worker/src/cpu_usage/linux.rs.

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func mountinfoPath(path string) string {
	return strings.ReplaceAll(strings.ReplaceAll(path, `\`, `\134`), " ", `\040`)
}

func TestCgroupCounterMapsNamespaceMembershipAndReadsEscapedMount(t *testing.T) {
	directory := t.TempDir()
	mount := filepath.Join(directory, "cgroup mount")
	group := filepath.Join(mount, "session")
	if err := os.MkdirAll(group, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(group, "cpu.stat"),
		[]byte("usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	target := mountinfoPath(mount)
	mounts := fmt.Sprintf("40 1 0:40 /unrelated /cgroup-old rw - cgroup2 cgroup rw\n"+
		"41 1 0:41 /tenant %s rw - cgroup2 cgroup rw", target)
	counter, err := resolveCgroupCpu("0::/tenant/session\n", mounts)
	if err != nil {
		t.Fatal(err)
	}
	if usage, err := counter.read(); err != nil || usage != 1_234_567*time.Microsecond {
		t.Fatalf("usage = %s, %v", usage, err)
	}

	// A private cgroup namespace shows its own root as `/`; the mount root
	// must still agree with that view before it is used.
	privateMounts := fmt.Sprintf("42 1 0:42 / %s rw - cgroup2 cgroup rw", target)
	rootCounter, err := resolveCgroupCpu("0::/\n", privateMounts)
	if err != nil {
		t.Fatal(err)
	}
	if rootCounter.path != filepath.Join(mount, "cpu.stat") {
		t.Fatalf("path = %s", rootCounter.path)
	}
}

func TestCgroupV1CounterUsesCpuacctMembershipAndNanoseconds(t *testing.T) {
	directory := t.TempDir()
	if err := os.WriteFile(filepath.Join(directory, "cpuacct.usage"), []byte("7654321000\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	mount := mountinfoPath(directory)
	mounts := fmt.Sprintf("51 1 0:51 /tenant %s rw - cgroup cgroup rw,cpu,cpuacct", mount)
	counter, err := resolveCgroupCpu("4:cpu,cpuacct:/tenant\n", mounts)
	if err != nil {
		t.Fatal(err)
	}
	if usage, err := counter.read(); err != nil || usage != 7_654_321_000*time.Nanosecond {
		t.Fatalf("usage = %s, %v", usage, err)
	}
}

func TestParseProcStatReadsFieldsAfterACommandNameWithParentheses(t *testing.T) {
	stat, err := parseProcStat("123 (a) (b) S 42 0 0 0 0 0 0 0 0 0 10 20 30 40 0")
	if err != nil {
		t.Fatal(err)
	}
	if stat.parent != 42 || stat.ticks != 100 {
		t.Fatalf("%+v", stat)
	}
}
