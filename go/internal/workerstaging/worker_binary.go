// Port of mj-worker-staging/src/worker_binary.rs.

// Worker binary acquisition, profile staging, and worker installation.
//
// The Rust module re-exports its submodules (`pub use staging::*`, ...);
// in Go they are all files of this package.

package workerstaging

import (
	"fmt"
	"os"
	"runtime"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// rustArch is Rust's std::env::consts::ARCH for this build's architecture.
func rustArch() string {
	switch runtime.GOARCH {
	case "amd64":
		return "x86_64"
	case "arm64":
		return "aarch64"
	case "386":
		return "x86"
	case "ppc64le", "ppc64":
		return "powerpc64"
	case "loong64":
		return "loongarch64"
	default:
		return runtime.GOARCH
	}
}

// isFile is Rust's `Path::is_file`: path exists, after following links, as
// a regular file.
func isFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

// lossyText is Rust's String::from_utf8_lossy for command output.
func lossyText(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}

// onDedicatedThread runs a blocking HTTP request on its own goroutine and
// returns its result.
//
// In Rust, a `reqwest::blocking::Client` owns a private Tokio runtime that
// panics when it is dropped on a thread with a Tokio `block_on` context
// entered, which the session-move lifecycle has; a fresh OS thread avoids
// that. Go has no such runtime, but the helper keeps the same contract for
// its callers: the work runs apart from the caller, and a panic in it is
// reported as an error rather than taking the caller down.
func onDedicatedThread[T any](work func() (T, error)) (T, error) {
	type outcome struct {
		value T
		err   error
	}
	done := make(chan outcome, 1)
	go func() {
		defer func() {
			if panicked := recover(); panicked != nil {
				var zero T
				done <- outcome{zero, fmt.Errorf("blocking HTTP thread panicked: %s", core.CommandThreadPanicMessage(panicked))}
			}
		}()
		value, err := work()
		done <- outcome{value, err}
	}()
	result := <-done
	return result.value, result.err
}
