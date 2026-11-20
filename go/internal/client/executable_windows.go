// Port of mj-client/src/executable.rs (Windows).

package client

import "os"

// ProcessRunsThisExecutable is whether process pid runs the same executable
// file as this process.
//
// Nil means the question could not be answered: the process is gone, or this
// platform does not expose a process's executable. A file at the process's
// pathname newer than the process's start also means a different build.
func ProcessRunsThisExecutable(pid uint32) (*bool, error) {
	panic("unported: mj-client/src/executable.rs process_runs_this_executable")
}

// sameExecutableFile compares canonical paths. Windows refuses to overwrite
// or remove a running executable and only lets it be renamed, so a process's
// own pathname still names its file unless a newer file was put there, which
// the caller's time check catches.
func sameExecutableFile(current, other string) (bool, error) {
	panic("unported: mj-client/src/executable.rs same_executable_file")
}

// processExecutablePath asks the operating system for the process's
// executable pathname (Rust reads it through sysinfo).
func processExecutablePath(pid uint32) (string, bool) {
	panic("unported: mj-client/src/executable.rs process_executable_path")
}

func runningExecutablePath() (string, bool) {
	path, err := os.Executable()
	return path, err == nil
}
