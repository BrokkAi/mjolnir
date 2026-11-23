// Port of mj-client/src/executable.rs (Windows).

package client

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

	"golang.org/x/sys/windows"
)

// ProcessRunsThisExecutable is whether process pid runs the same executable
// file as this process.
//
// Nil means the question could not be answered: the process is gone, or this
// platform does not expose a process's executable. A file at the process's
// pathname newer than the process's start also means a different build.
func ProcessRunsThisExecutable(pid uint32) (*bool, error) {
	started, path, ok := processExecutableAndStart(pid)
	if !ok || path == "" {
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

// sameExecutableFile compares canonical paths. Windows refuses to overwrite
// or remove a running executable and only lets it be renamed, so a process's
// own pathname still names its file unless a newer file was put there, which
// the caller's time check catches.
func sameExecutableFile(current, other string) (bool, error) {
	left, err := filepath.EvalSymlinks(current)
	if err != nil {
		return false, err
	}
	if left, err = filepath.Abs(left); err != nil {
		return false, err
	}
	right, err := filepath.EvalSymlinks(other)
	if err != nil {
		return false, err
	}
	if right, err = filepath.Abs(right); err != nil {
		return false, err
	}
	return left == right, nil
}

// processExecutableAndStart is process pid's executable pathname and its
// start in Unix seconds, as sysinfo reports them; false when the process
// cannot be opened.
func processExecutableAndStart(pid uint32) (uint64, string, bool) {
	handle, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
	if err != nil {
		return 0, "", false
	}
	defer windows.CloseHandle(handle)
	var creation, exit, kernel, user windows.Filetime
	if err := windows.GetProcessTimes(handle, &creation, &exit, &kernel, &user); err != nil {
		return 0, "", false
	}
	nanos := creation.Nanoseconds()
	var started uint64
	if nanos > 0 {
		started = uint64(nanos / 1_000_000_000)
	}
	buffer := make([]uint16, windows.MAX_LONG_PATH)
	size := uint32(len(buffer))
	if err := windows.QueryFullProcessImageName(handle, 0, &buffer[0], &size); err != nil {
		return started, "", true
	}
	return started, windows.UTF16ToString(buffer[:size]), true
}

// processExecutablePath asks the operating system for the process's
// executable pathname (Rust reads it through sysinfo).
func processExecutablePath(pid uint32) (string, bool) {
	_, path, ok := processExecutableAndStart(pid)
	return path, ok && path != ""
}

func runningExecutablePath() (string, bool) {
	path, err := os.Executable()
	return path, err == nil
}
