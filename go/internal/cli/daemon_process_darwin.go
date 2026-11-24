// Port of mj-cli/src/daemon.rs: sysinfo's process table on macOS, read
// through sysctl.

package cli

import (
	"bytes"
	"encoding/binary"

	"golang.org/x/sys/unix"
)

// processCommandLine is a process's arguments, as sysinfo reads them from
// `kern.procargs2`: the argument count, the executable path, padding, then
// the arguments. False when the process cannot be read, as for an exiting
// one.
func processCommandLine(pid uint32) ([]string, bool) {
	raw, err := unix.SysctlRaw("kern.procargs2", int(pid))
	if err != nil || len(raw) < 4 {
		return nil, false
	}
	count := int(binary.LittleEndian.Uint32(raw[:4]))
	rest := raw[4:]
	// The executable path, then the NUL padding before the first argument.
	end := bytes.IndexByte(rest, 0)
	if end < 0 {
		return nil, false
	}
	rest = rest[end:]
	for len(rest) > 0 && rest[0] == 0 {
		rest = rest[1:]
	}
	arguments := make([]string, 0, count)
	for len(arguments) < count && len(rest) > 0 {
		end := bytes.IndexByte(rest, 0)
		if end < 0 {
			end = len(rest)
		}
		arguments = append(arguments, string(rest[:end]))
		if end == len(rest) {
			break
		}
		rest = rest[end+1:]
	}
	return arguments, true
}

// childProcessIDs is every process whose parent is pid.
func childProcessIDs(pid uint32) []uint32 {
	processes, err := unix.SysctlKinfoProcSlice("kern.proc.all")
	if err != nil {
		return nil
	}
	var children []uint32
	for _, process := range processes {
		if process.Eproc.Ppid == int32(pid) && process.Proc.P_pid > 0 {
			children = append(children, uint32(process.Proc.P_pid))
		}
	}
	return children
}
