//go:build !windows

// Port of mj-cli/src/dashboard/surface.rs windows_toast_command off
// Windows, where there is no console window to keep.

package cli

import "os/exec"

// withoutConsoleWindow does nothing off Windows.
func withoutConsoleWindow(*exec.Cmd) {}
