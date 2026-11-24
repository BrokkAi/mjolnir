//go:build !unix

// Port of mj-worker-launch/src/local_profile_homes.rs: the `cfg(not(unix))`
// items.

package workerlaunch

import (
	"errors"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// unsupportedError is Rust's io::Error of kind Unsupported with a message:
// it displays as the message alone and matches errors.ErrUnsupported.
type unsupportedError string

func (e unsupportedError) Error() string        { return string(e) }
func (e unsupportedError) Is(target error) bool { return target == errors.ErrUnsupported }

func linkDirectory(target string, link string) error {
	return unsupportedError("linking a staged home needs a Unix file system")
}

// RunningProcessArguments is the argument lists of every process running on
// this machine; ok is false when they cannot be listed.
func RunningProcessArguments(executor core.CommandExecutor) (arguments []string, ok bool) {
	return runningProcessArgumentsWithoutPs()
}
