// Port of mj-client/src/executable.rs (Linux).

package client

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
)

// ProcessRunsThisExecutable is whether process pid runs the same executable
// file as this process.
//
// Nil means the question could not be answered: the process is gone, or this
// platform does not expose a process's executable.
func ProcessRunsThisExecutable(pid uint32) (*bool, error) {
	current, err := ExecutableFileIdentityOf("/proc/self/exe")
	if err != nil {
		return nil, fmt.Errorf("inspect this process's executable: %w", err)
	}
	processPath := fmt.Sprintf("/proc/%d/exe", pid)
	other, err := ExecutableFileIdentityOf(processPath)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, nil
		}
		return nil, fmt.Errorf("inspect process %d executable via %s: %w", pid, processPath, err)
	}
	same := current == other
	return &same, nil
}

// processExecutablePath reads /proc/<pid>/exe.
func processExecutablePath(pid uint32) (string, bool) {
	path, err := os.Readlink(fmt.Sprintf("/proc/%d/exe", pid))
	return path, err == nil
}

// runningExecutablePath reads /proc/self/exe.
func runningExecutablePath() (string, bool) {
	path, err := os.Readlink("/proc/self/exe")
	return path, err == nil
}
