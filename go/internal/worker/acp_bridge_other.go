//go:build !unix

// Part of the port of mj-worker/src/acp.rs: outside Unix a process does not
// end on a signal, so Rust's ExitStatus Display names none.

package worker

import "syscall"

func bridgeSignalName(syscall.Signal) string {
	return ""
}
