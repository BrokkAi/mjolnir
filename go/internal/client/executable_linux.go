// Port of mj-client/src/executable.rs (Linux).

package client

// ProcessRunsThisExecutable is whether process pid runs the same executable
// file as this process.
//
// Nil means the question could not be answered: the process is gone, or this
// platform does not expose a process's executable.
func ProcessRunsThisExecutable(pid uint32) (*bool, error) {
	panic("unported: mj-client/src/executable.rs process_runs_this_executable")
}

// processExecutablePath reads /proc/<pid>/exe.
func processExecutablePath(pid uint32) (string, bool) {
	panic("unported: mj-client/src/executable.rs process_executable_path")
}

// runningExecutablePath reads /proc/self/exe.
func runningExecutablePath() (string, bool) {
	panic("unported: mj-client/src/executable.rs running_executable_path")
}
