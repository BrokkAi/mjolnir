// Port of mj-worker/src/work_facts/macos.rs: the scan itself, which Rust
// builds on macOS only (#[cfg(target_os = "macos")]; here the _darwin file
// suffix).

package worker

// readTable is every process the scan may consider, from two ps listings,
// and the reader for the survivors.
func readTable(sessionID string) (ProcessTable, error) {
	panic("unported: mj-worker/src/work_facts/macos.rs read_table")
}
