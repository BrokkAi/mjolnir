// Port of mj-worker/src/acp/kimi_tasks.rs: its test-only items.

package worker

// FullScan parses the complete main-agent wire stream.
func FullScan(wirePath string) (KimiTaskSnapshot, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs full_scan")
}

// parseComplete parses every complete line of bytes; it returns the tracker
// and the number of lines read.
func parseComplete(bytes []byte, path string) (TaskTracker, uint64, error) {
	panic("unported: mj-worker/src/acp/kimi_tasks.rs parse_complete")
}
