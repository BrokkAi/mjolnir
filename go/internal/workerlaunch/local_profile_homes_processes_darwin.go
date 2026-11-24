// Port of mj-worker-launch/src/local_profile_homes.rs: sysinfo's process
// command lines on macOS, read through sysctl (KERN_PROCARGS2).

package workerlaunch

// processCommandLines is the command line of every process on this machine,
// each one's arguments joined with spaces, as sysinfo reads them with
// ProcessRefreshKind::new().with_cmd(UpdateKind::Always). A process whose
// command line cannot be read lists as "".
func processCommandLines() []string {
	panic("unported: mj-worker-launch/src/local_profile_homes.rs running_process_arguments_without_ps (macOS process listing)")
}
