// Port of the non-Unix parts of mj-targets/src/test_isolation.rs.

//go:build !unix

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Output runs the child and returns its output without judging it.
func (t *IsolatedTest) Output() core.ProcessOutput {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::output")
}
