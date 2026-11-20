// Port of the `cfg(not(unix))` parts of mj-worker/src/terminal.rs.

//go:build !unix

package worker

import (
	"errors"
	"io"
	"math"
	"os"
	"os/exec"
)

func spawnTerminalShell(string, *TerminalSpawn) (*exec.Cmd, io.ReadCloser, io.ReadCloser, error) {
	return nil, nil, nil, errors.New("client terminals need Unix process groups")
}

// setOwnProcessGroup does nothing: process groups are a Unix facility.
func setOwnProcessGroup(*exec.Cmd) {}

func killTerminalProcessGroup(int) {}

// terminalExitFromStatus reports the exit code only; there are no signals.
// Rust reads the code as an i32, so a code past i32's range (a Windows
// NTSTATUS such as 0xC0000005) has no u32 form and is reported as absent.
func terminalExitFromStatus(state *os.ProcessState) TerminalExit {
	code := state.ExitCode()
	if code < 0 || code > math.MaxInt32 {
		return TerminalExit{}
	}
	exitCode := uint32(code)
	return TerminalExit{ExitCode: &exitCode}
}
