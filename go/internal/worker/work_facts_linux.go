// Port of mj-worker/src/work_facts/linux.rs.
//
// /proc reader for the process scan. Built on Linux only (the _linux file
// suffix), as Rust's #[cfg(target_os = "linux")].

package worker

import (
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"
)

// workFactsStat is Rust's linux::Stat (renamed: locals named stat would
// shadow it, and cpu_usage's ProcStat is procStat): the fields of
// /proc/<pid>/stat that follow the command name.
type workFactsStat struct {
	zombie     bool
	ppid       uint32
	startTicks uint64
}

func parseStat(text string) (workFactsStat, bool) {
	cut := strings.LastIndexByte(text, ')')
	if cut < 0 {
		return workFactsStat{}, false
	}
	fields := strings.Fields(text[cut+1:])
	// State is field 3 of the file and so index 0 here; ppid is field 4 and
	// the start time, in clock ticks since boot, is field 22.
	if len(fields) < 20 {
		return workFactsStat{}, false
	}
	ppid, err := strconv.ParseUint(fields[1], 10, 32)
	if err != nil {
		return workFactsStat{}, false
	}
	start, err := strconv.ParseUint(fields[19], 10, 64)
	if err != nil {
		return workFactsStat{}, false
	}
	return workFactsStat{zombie: strings.HasPrefix(fields[0], "Z"), ppid: uint32(ppid), startTicks: start}, true
}

// ticksPerSecond is sysconf(_SC_CLK_TCK).
func ticksPerSecond() (uint64, error) {
	panic("unported: mj-worker/src/work_facts/linux.rs ticks_per_second")
}

// uptimeMs is uptime in milliseconds. It is monotonic, unlike the boot time
// in /proc/stat, which moves with wall-clock corrections.
func uptimeMs() (int64, error) {
	text, err := os.ReadFile("/proc/uptime")
	if err != nil {
		return 0, fmt.Errorf("read /proc/uptime: %w", err)
	}
	fields := strings.Fields(string(text))
	if len(fields) == 0 {
		return 0, errors.New("empty /proc/uptime")
	}
	seconds, err := strconv.ParseFloat(fields[0], 64)
	if err != nil {
		return 0, fmt.Errorf("parse /proc/uptime: %w", err)
	}
	return int64(seconds * 1000.0), nil
}

// sessionMarker is whether pid's environment carries wanted. A refused read
// is not the same as an absent marker: for a process of this user it means
// the marker cannot be checked, which the scan reports instead of treating
// the process as foreign.
func sessionMarker(pid uint32, wanted []byte) WorkFactsMarker {
	panic("unported: mj-worker/src/work_facts/linux.rs session_marker")
}

// readTable is every process the scan may consider, and the reader for the
// survivors.
func readTable(sessionID string) (ProcessTable, error) {
	panic("unported: mj-worker/src/work_facts/linux.rs read_table")
}

func describe(pid uint32, bootID string) Described {
	panic("unported: mj-worker/src/work_facts/linux.rs describe")
}
