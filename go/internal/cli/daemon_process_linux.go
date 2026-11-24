// Port of mj-cli/src/daemon.rs: sysinfo's process table on Linux, read
// from /proc.

package cli

import (
	"os"
	"strconv"
	"strings"
)

// processCommandLine is a process's arguments, as sysinfo's `cmd()` reads
// them from /proc/<pid>/cmdline; false when the process cannot be read.
func processCommandLine(pid uint32) ([]string, bool) {
	body, err := os.ReadFile("/proc/" + strconv.FormatUint(uint64(pid), 10) + "/cmdline")
	if err != nil {
		return nil, false
	}
	text := strings.TrimSuffix(string(body), "\x00")
	if text == "" {
		return []string{}, true
	}
	return strings.Split(text, "\x00"), true
}

// childProcessIDs is every process whose parent is pid.
func childProcessIDs(pid uint32) []uint32 {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil
	}
	var children []uint32
	for _, entry := range entries {
		child, err := strconv.ParseUint(entry.Name(), 10, 32)
		if err != nil {
			continue
		}
		stat, err := os.ReadFile("/proc/" + entry.Name() + "/stat")
		if err != nil {
			continue
		}
		// The command name may hold spaces and parentheses; the fields
		// after the last `)` are state, then the parent PID.
		closing := strings.LastIndexByte(string(stat), ')')
		if closing < 0 {
			continue
		}
		fields := strings.Fields(string(stat[closing+1:]))
		if len(fields) < 2 {
			continue
		}
		if parent, err := strconv.ParseUint(fields[1], 10, 32); err == nil && uint32(parent) == pid {
			children = append(children, uint32(child))
		}
	}
	return children
}
