// Port of the macOS `imp` of mj-worker/src/worker_stop/process_table.rs.

package worker

// processTableLookup is pid's entry; false when no such process exists.
func processTableLookup(pid uint32) (processEntry, bool, error) {
	panic("unported: mj-worker/src/worker_stop/process_table.rs lookup")
}

// processTableList is every process the kernel lists. Processes can start
// while the list is read, so it retries when the list filled its buffer.
func processTableList() ([]processEntry, error) {
	panic("unported: mj-worker/src/worker_stop/process_table.rs table")
}
