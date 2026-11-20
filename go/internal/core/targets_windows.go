//go:build windows

// Port of the Windows parts of mj-core/src/targets.rs.

package core

import (
	"errors"
	"log/slog"
	"os"
	"os/exec"
)

// targetsSetProcessGroup does nothing on Windows: process groups are a Unix
// feature, and Rust's build sets none there either.
func targetsSetProcessGroup(*exec.Cmd) {}

// targetsKillChild kills a cancelled command's process.
func targetsKillChild(child *targetChild, _ bool) {
	if err := child.process.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
		slog.Warn("could not terminate cancelled command", "pid", child.pid(), "error", err)
	}
}
