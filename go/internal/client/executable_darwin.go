// Port of mj-client/src/executable.rs (macOS).

package client

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"

	"golang.org/x/sys/unix"
)

// ProcessRunsThisExecutable is whether process pid runs the same executable
// file as this process.
//
// Nil means the question could not be answered: the process is gone, or this
// platform does not expose a process's executable. macOS reports a pathname,
// not Linux's reference to the running inode, so a file at that pathname
// newer than the process's start also means a different build.
func ProcessRunsThisExecutable(pid uint32) (*bool, error) {
	started, ok := processStartTime(pid)
	if !ok {
		return nil, nil
	}
	path, ok := processExecutablePath(pid)
	if !ok {
		return nil, nil
	}
	current, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("find this process's executable: %w", err)
	}
	info, err := os.Stat(path)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			different := false
			return &different, nil
		}
		return nil, fmt.Errorf("inspect process executable: %w", err)
	}
	modified := info.ModTime().Unix()
	if modified < 0 {
		return nil, fmt.Errorf("inspect process executable: modification time %v is before the Unix epoch", info.ModTime())
	}
	same, err := sameExecutableFile(current, path)
	if err != nil {
		return nil, err
	}
	answer := same && uint64(modified) <= started
	return &answer, nil
}

// sameExecutableFile compares the two files by device and inode.
func sameExecutableFile(current, other string) (bool, error) {
	left, err := ExecutableFileIdentityOf(current)
	if err != nil {
		return false, err
	}
	right, err := ExecutableFileIdentityOf(other)
	if err != nil {
		return false, err
	}
	return left == right, nil
}

// processStartTime is when process pid started, in Unix seconds, as
// sysinfo's Process::start_time reports it; false when it is gone.
func processStartTime(pid uint32) (uint64, bool) {
	info, err := unix.SysctlKinfoProc("kern.proc.pid", int(pid))
	if err != nil || info.Proc.P_pid != int32(pid) {
		return 0, false
	}
	seconds := info.Proc.P_starttime.Sec
	if seconds < 0 {
		return 0, false
	}
	return uint64(seconds), true
}

// processExecutablePath asks the operating system for the process's
// executable pathname (Rust reads it through sysinfo). The kernel's argument
// area starts with argc and then the path the process was executed from.
func processExecutablePath(pid uint32) (string, bool) {
	arguments, err := unix.SysctlRaw("kern.procargs2", int(pid))
	if err != nil || len(arguments) < 4 {
		return "", false
	}
	path := arguments[4:]
	if end := bytes.IndexByte(path, 0); end >= 0 {
		path = path[:end]
	}
	if len(path) == 0 {
		return "", false
	}
	return string(path), true
}

func runningExecutablePath() (string, bool) {
	path, err := os.Executable()
	return path, err == nil
}
