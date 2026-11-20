// Port of mj-worker/src/test_support.rs.

// Fixtures shared by more than one test file in this package.

package worker

import (
	"fmt"
	"os/exec"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// testSupportWaitFor is test_support::wait_for: a hang detector for fixtures
// that perform real filesystem, process, or socket I/O. It runs future on its
// own goroutine and panics when it takes longer than ioTimeout. Tests of
// application deadlines use their own controlled clock.
func testSupportWaitFor[T any](description string, future func() T) T {
	done := make(chan T, 1)
	go func() { done <- future() }()
	select {
	case value := <-done:
		return value
	case <-time.After(ioTimeout):
		panic(fmt.Sprintf("timed out waiting for %s", description))
	}
}

// testSupportGit is test_support::git: run Git in repository and return its
// trimmed standard output. It panics when Git cannot run or fails.
//
// Every test that builds a repository needs this, so it lives here rather
// than once per test file. Callers that only need the side effect discard
// the returned text.
func testSupportGit(repository string, arguments ...string) string {
	command := exec.Command("git", arguments...)
	command.Dir = repository
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		panic(fmt.Sprintf("git %q: %v", arguments, err))
	}
	if !output.Status.Success() {
		panic(fmt.Sprintf("git %q: %s", arguments, fromUTF8Lossy(output.Stderr)))
	}
	return strings.TrimSpace(string(output.Stdout))
}

// scopedLogCaptureSeesEveryCallsite is test_support's tracing workaround.
// tracing-core caches each callsite's interest process-wide, so the Rust
// tests install a global subscriber that keeps every callsite asking the
// scoped one. log/slog has no callsite cache, so there is nothing to do.
func scopedLogCaptureSeesEveryCallsite() {}
