// Port of mj-worker-launch/src/local_profile_homes.rs: sysinfo's process
// command lines on Windows, read from each process as sysinfo does.

package workerlaunch

import (
	"errors"
	"strings"
	"unsafe"

	"golang.org/x/sys/windows"
)

// processCommandLines is the command line of every process on this machine,
// each one's arguments joined with spaces, as sysinfo reads them with
// ProcessRefreshKind::new().with_cmd(UpdateKind::Always). A process whose
// command line cannot be read lists as "".
func processCommandLines() []string {
	snapshot, err := windows.CreateToolhelp32Snapshot(windows.TH32CS_SNAPPROCESS, 0)
	if err != nil {
		return nil
	}
	defer windows.CloseHandle(snapshot)
	var entry windows.ProcessEntry32
	entry.Size = uint32(unsafe.Sizeof(entry))
	var lines []string
	for err := windows.Process32First(snapshot, &entry); err == nil; err = windows.Process32Next(snapshot, &entry) {
		lines = append(lines, windowsCommandLine(entry.ProcessID))
	}
	return lines
}

// windowsCommandLine is sysinfo's get_cmd_line_new: the process's command
// line from NtQueryInformationProcess(ProcessCommandLineInformation), split
// into arguments as CommandLineToArgvW splits them, joined with spaces.
// sysinfo opens the process for query and memory reads, or for limited query
// when it may not.
func windowsCommandLine(pid uint32) string {
	handle, err := windows.OpenProcess(windows.PROCESS_QUERY_INFORMATION|windows.PROCESS_VM_READ, false, pid)
	if err != nil {
		handle, err = windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
		if err != nil {
			return ""
		}
	}
	defer windows.CloseHandle(handle)
	var length uint32
	if err := windows.NtQueryInformationProcess(handle, windows.ProcessCommandLineInformation, nil, 0, &length); err != nil {
		var status windows.NTStatus
		if !errors.As(err, &status) || (status != windows.STATUS_BUFFER_OVERFLOW &&
			status != windows.STATUS_BUFFER_TOO_SMALL && status != windows.STATUS_INFO_LENGTH_MISMATCH) {
			return ""
		}
	}
	if uintptr(length) < unsafe.Sizeof(windows.NTUnicodeString{}) {
		return ""
	}
	// Whole words keep the UNICODE_STRING header at the start aligned.
	word := unsafe.Sizeof(uintptr(0))
	buffer := make([]uintptr, (uintptr(length)+word-1)/word)
	if err := windows.NtQueryInformationProcess(handle, windows.ProcessCommandLineInformation,
		unsafe.Pointer(&buffer[0]), length, &length); err != nil {
		return ""
	}
	text := (*windows.NTUnicodeString)(unsafe.Pointer(&buffer[0])).String()
	arguments, err := windows.DecomposeCommandLine(text)
	if err != nil {
		return ""
	}
	return strings.Join(arguments, " ")
}
