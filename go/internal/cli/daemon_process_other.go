//go:build !linux && !darwin && !windows

// Port of mj-cli/src/daemon.rs: sysinfo's process table where Mjolnir has
// no reader for it. Every command line reads as unreadable and no process
// as a child, which the callers treat as inconclusive.

package cli

func processCommandLine(uint32) ([]string, bool) { return nil, false }

func childProcessIDs(uint32) []uint32 { return nil }
