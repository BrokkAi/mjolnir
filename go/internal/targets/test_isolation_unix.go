// Port of the Unix parts of mj-targets/src/test_isolation.rs.

//go:build unix

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// IncludeIgnored includes an ignored integration case in the isolated child
// process.
func (t *IsolatedTest) IncludeIgnored() *IsolatedTest {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::include_ignored")
}

// Output runs the child and returns its output without judging it.
func (t *IsolatedTest) Output() core.ProcessOutput {
	return t.outputUnix()
}

func (t *IsolatedTest) outputUnix() core.ProcessOutput {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::output_unix")
}

type isolatedProcessGroupGuard struct {
	// pgid is the child's process group, or 0 once terminated or for none.
	pgid int
}

func newIsolatedProcessGroupGuard(pid int) *isolatedProcessGroupGuard {
	if pid <= 1 {
		pid = 0
	}
	return &isolatedProcessGroupGuard{pgid: pid}
}

func (g *isolatedProcessGroupGuard) terminate() {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedProcessGroupGuard::terminate")
}

// close is Rust's Drop.
func (g *isolatedProcessGroupGuard) close() {
	g.terminate()
}

func waitForExitWithoutReaping(pid int) {
	panic("unported: mj-targets/src/test_isolation.rs wait_for_exit_without_reaping")
}
