// Port of mj-targets/src/test_isolation.rs.
//
// Running one test alone in a child of the test binary, for tests that need
// process-global state (the data directory, the database writer, the
// working directory) to themselves.
//
// Rust's libtest selects one test with `--exact <module>::<name>`; a Go test
// binary selects it with `-test.run '^<Name>$'`. The API keeps Rust's shape
// so callers port unchanged.

package targets

import (
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// TestName is the name that selects one test of this binary, given its
// module path (Rust: the `--exact` name from `module_path!()`).
//
// `module_path!()` carries the crate name, which libtest's filter does not.
// mj-daemon's tests use this too, so any crate name is stripped.
func TestName(modulePath string, test string) string {
	module := modulePath
	if _, rest, ok := strings.Cut(modulePath, "::"); ok {
		module = rest
	}
	return fmt.Sprintf("%s::%s", module, test)
}

// IsolatedTest re-runs one test alone, in a child of this test binary.
//
// The data directory, the installed database writer, the log handler and
// the process working directory are all process-global, so a test that
// needs its own has to be the only test in its process. The child runs the
// named test alone, and Run fails the parent with the child's own output
// when it does not pass.
//
// Build the name with TestName.
type IsolatedTest struct {
	name    string
	command *exec.Cmd
}

// isolatedChildStackBytes: a child runs one whole flow on one test thread.
// An in-place Move, for example, polls the checkpoint and provisioning chain
// inline, whose debug-build frames are tens of kilobytes each; at the 8 MiB
// default that sits within a few pages of the limit, so an unrelated change
// can overflow a child that passed before. Give the child room instead of
// leaving every isolated test to the edge of its stack (Rust:
// RUST_MIN_STACK; Go's goroutine stacks grow on their own).
const isolatedChildStackBytes = "33554432"

func NewIsolatedTest(name string) *IsolatedTest {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::new")
}

// Env sets a variable for the child. The marker a test uses to tell the
// child apart from the parent goes here too.
func (t *IsolatedTest) Env(key string, value string) *IsolatedTest {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::env")
}

// EnvRemove removes a variable from the child, such as the default worker
// source.
func (t *IsolatedTest) EnvRemove(key string) *IsolatedTest {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::env_remove")
}

// IsolatedStore gives the child its own configuration and data directories
// under root.
func (t *IsolatedTest) IsolatedStore(root string) *IsolatedTest {
	return t.Env("MJ_DATA_DIR", filepath.Join(root, "data")).
		Env("MJ_CONFIG_DIR", filepath.Join(root, "config"))
}

// Run runs the child and fails this test with its output if it did not
// pass.
func (t *IsolatedTest) Run() core.ProcessOutput {
	panic("unported: mj-targets/src/test_isolation.rs IsolatedTest::run")
}
