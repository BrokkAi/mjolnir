// Port of mj-cli/src/daemon.rs: sysinfo's process table and the daemon
// termination on Windows.

package cli

import (
	"fmt"
	"unsafe"

	"golang.org/x/sys/windows"
)

// processCommandLine is a process's arguments, from the command line the
// process was started with (ProcessCommandLineInformation), split as the
// C runtime splits it. False when the process cannot be read.
func processCommandLine(pid uint32) ([]string, bool) {
	handle, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
	if err != nil {
		return nil, false
	}
	defer windows.CloseHandle(handle)
	var needed uint32
	buffer := make([]byte, 4096)
	for {
		err := windows.NtQueryInformationProcess(handle, windows.ProcessCommandLineInformation,
			unsafe.Pointer(&buffer[0]), uint32(len(buffer)), &needed)
		if err == nil {
			break
		}
		if needed <= uint32(len(buffer)) {
			return nil, false
		}
		buffer = make([]byte, needed)
	}
	text := (*windows.NTUnicodeString)(unsafe.Pointer(&buffer[0])).String()
	if text == "" {
		return []string{}, true
	}
	arguments, err := windows.DecomposeCommandLine(text)
	if err != nil {
		return nil, false
	}
	return arguments, true
}

// childProcessIDs is every process whose parent is pid.
func childProcessIDs(pid uint32) []uint32 {
	snapshot, err := windows.CreateToolhelp32Snapshot(windows.TH32CS_SNAPPROCESS, 0)
	if err != nil {
		return nil
	}
	defer windows.CloseHandle(snapshot)
	var entry windows.ProcessEntry32
	entry.Size = uint32(unsafe.Sizeof(entry))
	var children []uint32
	for err = windows.Process32First(snapshot, &entry); err == nil; err = windows.Process32Next(snapshot, &entry) {
		if entry.ParentProcessID == pid && entry.ProcessID != pid {
			children = append(children, entry.ProcessID)
		}
	}
	return children
}

// signalHostDaemon terminates the daemon at pid: Windows has no SIGTERM, and
// the daemon has no console to receive Ctrl-Break. Windows does not reuse a
// PID while a handle to its process is open, so the process checked is the
// one terminated.
func signalHostDaemon(pid uint32) error {
	handle, err := windows.OpenProcess(windows.PROCESS_TERMINATE, false, pid)
	if err != nil {
		daemon, checkErr := hostPIDIsDaemon(pid)
		if checkErr != nil {
			return checkErr
		}
		if daemon {
			return fmt.Errorf("open superseded Mjolnir daemon: %w", err)
		}
		return nil
	}
	defer windows.CloseHandle(handle)
	daemon, err := hostPIDIsDaemon(pid)
	if err != nil {
		return err
	}
	if daemon {
		if err := windows.TerminateProcess(handle, 1); err != nil {
			return fmt.Errorf("stop superseded Mjolnir daemon: %w", err)
		}
	}
	return nil
}
