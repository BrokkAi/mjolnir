// Port of mj-worker/src/worker_runtime/shell_environment.rs.

// Session shell hooks and safely published executable wrappers.

//go:build unix

package worker

// shellEnvironmentConfigure installs the session's BASH_ENV hook at `path`
// and points BASH_ENV at it through MJ_SESSION_BASH_ENV, keeping a
// user-supplied hook as MJ_ORIGINAL_BASH_ENV.
func shellEnvironmentConfigure(path string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/shell_environment.rs configure")
}

// writeExecutable publishes an executable script at `path` without ever
// holding a writable descriptor to it in this process.
func writeExecutable(path string, contents string) error {
	panic("unported: mj-worker/src/worker_runtime/shell_environment.rs write_executable")
}
