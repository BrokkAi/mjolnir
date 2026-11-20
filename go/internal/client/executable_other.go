//go:build !linux && !darwin && !windows

// Port of mj-client/src/executable.rs (platforms that do not expose a
// process's executable).

package client

import "os"

// ProcessRunsThisExecutable answers "unknown" on platforms that do not
// expose a process's executable, so every caller has one shape to handle.
func ProcessRunsThisExecutable(pid uint32) (*bool, error) { return nil, nil }

func processExecutablePath(pid uint32) (string, bool) { return "", false }

func runningExecutablePath() (string, bool) {
	path, err := os.Executable()
	return path, err == nil
}
