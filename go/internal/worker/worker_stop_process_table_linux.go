// Port of the Linux `imp` of mj-worker/src/worker_stop/process_table.rs.

package worker

// processTableLookup is pid's entry; false when no such process exists. A
// process can vanish between listing and reading it.
func processTableLookup(pid uint32) (processEntry, bool, error) {
	panic("unported: mj-worker/src/worker_stop/process_table.rs lookup")
}

// processTableList is every process the kernel shows in /proc.
func processTableList() ([]processEntry, error) {
	panic("unported: mj-worker/src/worker_stop/process_table.rs table")
}

// processTableParse reads one /proc/<pid>/stat text. The command name may
// hold spaces and parentheses, so fields are counted from the last `)`. The
// tail starts at field 3 (state): ppid is field 4, pgrp 5 and starttime 22.
func processTableParse(pid uint32, text string) (processEntry, error) {
	panic("unported: mj-worker/src/worker_stop/process_table.rs parse")
}
