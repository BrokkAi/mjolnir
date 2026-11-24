// Port of mj-worker-launch/src/local_profile_homes.rs: sysinfo's process
// command lines on macOS, read through sysctl (KERN_PROCARGS2).

package workerlaunch

import (
	"bytes"
	"encoding/binary"
	"strings"

	"golang.org/x/sys/unix"
)

// processCommandLines is the command line of every process on this machine,
// each one's arguments joined with spaces, as sysinfo reads them with
// ProcessRefreshKind::new().with_cmd(UpdateKind::Always). A process whose
// command line cannot be read lists as "".
func processCommandLines() []string {
	processes, err := unix.SysctlKinfoProcSlice("kern.proc.all")
	if err != nil {
		return nil
	}
	lines := make([]string, 0, len(processes))
	for index := range processes {
		lines = append(lines, darwinCommandLine(int(processes[index].Proc.P_pid)))
	}
	return lines
}

// darwinCommandLine reads one process's arguments as sysinfo's
// get_process_infos and get_arguments do: KERN_PROCARGS2 holds argc, the
// executable path, NUL padding, then the arguments, each NUL-terminated.
// Empty arguments are dropped.
func darwinCommandLine(pid int) string {
	data, err := unix.SysctlRaw("kern.procargs2", pid)
	if err != nil || len(data) < 4 {
		return ""
	}
	count := int32(binary.NativeEndian.Uint32(data[:4]))
	data = data[4:]
	// Skip the executable path.
	if end := bytes.IndexByte(data, 0); end >= 0 {
		data = data[end:]
	} else {
		data = nil
	}
	if count < 1 {
		return ""
	}
	data = bytes.TrimLeft(data, "\x00")
	var arguments []string
	for ; count > 0 && len(data) != 0; count-- {
		end := bytes.IndexByte(data, 0)
		if end < 0 {
			end = len(data)
		}
		if end != 0 {
			arguments = append(arguments, lossyText(data[:end]))
		}
		data = bytes.TrimLeft(data[end:], "\x00")
	}
	return strings.Join(arguments, " ")
}
