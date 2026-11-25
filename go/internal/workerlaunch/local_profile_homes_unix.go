//go:build unix

// Port of mj-worker-launch/src/local_profile_homes.rs: the `cfg(unix)` items.

package workerlaunch

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func linkDirectory(target string, link string) error {
	return os.Symlink(target, link)
}

// RunningProcessArguments is the argument lists of every process running on
// this machine; ok is false when they cannot be listed. The same `ps`
// options work on Linux and macOS, and `-ww` keeps long arguments whole. A
// host without `ps` is read through the platform's process table instead.
//
// Minimal Linux images need not install procps, so a `ps` that cannot run
// falls back to runningProcessArgumentsWithoutPs. A `ps` that runs but
// fails stays a failed listing: the sweep must not remove files it cannot
// prove are idle.
func RunningProcessArguments(executor core.CommandExecutor) (arguments []string, ok bool) {
	panic("unported: mj-worker-launch/src/local_profile_homes.rs running_process_arguments")
}
