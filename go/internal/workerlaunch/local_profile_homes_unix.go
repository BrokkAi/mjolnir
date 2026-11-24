//go:build unix

// Port of mj-worker-launch/src/local_profile_homes.rs: the `cfg(unix)` items.

package workerlaunch

import (
	"log/slog"
	"os"
	"strings"

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
	command := core.NewCommandSpec("ps", []string{"-A", "-ww", "-o", "args="}).
		WithPurpose("list running processes before removing files they may be using")
	output, err := executor.Execute(&command)
	if err != nil {
		if arguments, ok := runningProcessArgumentsWithoutPs(); ok {
			return arguments, true
		}
		slog.Warn("could not list running processes: " + err.Error())
		return nil, false
	}
	if output.Status != 0 {
		slog.Warn(
			"could not list running processes: "+strings.TrimSpace(lossyText(output.Stderr)),
			"status", output.Status,
		)
		return nil, false
	}
	arguments = rustLines(lossyText(output.Stdout))
	if arguments == nil {
		arguments = []string{}
	}
	return arguments, true
}
