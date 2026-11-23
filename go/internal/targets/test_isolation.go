// Port of mj-targets/src/test_isolation.rs.
//
// Running one test alone in a child of the test binary, for tests that need
// process-global state (the data directory, the database writer, the
// working directory) to themselves.
//
// Rust's libtest selects one test with `--exact <module>::<name>`; a Go test
// binary selects it with `-test.run '^<Name>$'`. The API keeps Rust's shape
// so callers port unchanged: the child runs the Go test named by the last
// `::` segment of the name, so `TestName("targets::tests", "TestFoo")` and
// plain "TestFoo" both select TestFoo.

package targets

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// IncludeIgnoredEnv is set to "1" in a child built with IncludeIgnored. Go
// has no ignored tests; a Go port of a Rust `#[ignore]` case runs when this
// is set and skips otherwise (Rust: libtest's `--ignored`).
const IncludeIgnoredEnv = "MJ_TEST_INCLUDE_IGNORED"

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
	// env holds the variables set (value) or removed (nil) for the child,
	// applied over the inherited environment as Rust's Command does.
	env map[string]*string
}

// isolatedChildStackBytes: a child runs one whole flow on one test thread.
// An in-place Move, for example, polls the checkpoint and provisioning chain
// inline, whose debug-build frames are tens of kilobytes each; at the 8 MiB
// default that sits within a few pages of the limit, so an unrelated change
// can overflow a child that passed before. Give the child room instead of
// leaving every isolated test to the edge of its stack (Rust:
// RUST_MIN_STACK; Go's goroutine stacks grow on their own, so the Go child
// needs no setting).
const isolatedChildStackBytes = "33554432"

func NewIsolatedTest(name string) *IsolatedTest {
	executable, err := os.Executable()
	if err != nil {
		panic(fmt.Sprintf("this test binary: %v", err))
	}
	selected := name
	if index := strings.LastIndex(name, "::"); index >= 0 {
		selected = name[index+len("::"):]
	}
	test := &IsolatedTest{
		name:    name,
		command: exec.Command(executable, "-test.run=^"+selected+"$", "-test.v"),
		env:     map[string]*string{},
	}
	// The instance name selects the default viewer port and the store
	// layout, so a child owns it: an inherited one changes output the
	// child's goldens pin. A test that needs an instance sets it with Env.
	test.EnvRemove("MJ_INSTANCE")
	// Control commands run a staged current worker, so a child that
	// exercises them needs a worker source. A test names its own with Env,
	// which replaces this one.
	if runtime.GOOS != "windows" {
		test.Env("MJ_WORKER_BINARY", core.FakeWorkerDispatcher())
	}
	return test
}

// Env sets a variable for the child. The marker a test uses to tell the
// child apart from the parent goes here too.
func (t *IsolatedTest) Env(key string, value string) *IsolatedTest {
	t.env[key] = &value
	return t
}

// EnvRemove removes a variable from the child, such as the default worker
// source.
func (t *IsolatedTest) EnvRemove(key string) *IsolatedTest {
	t.env[key] = nil
	return t
}

// IsolatedStore gives the child its own configuration and data directories
// under root.
func (t *IsolatedTest) IsolatedStore(root string) *IsolatedTest {
	return t.Env("MJ_DATA_DIR", filepath.Join(root, "data")).
		Env("MJ_CONFIG_DIR", filepath.Join(root, "config"))
}

// environment is the child's environment: this process's, with the
// variables set and removed by Env and EnvRemove.
func (t *IsolatedTest) environment() []string {
	environment := []string{}
	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		if _, changed := t.env[name]; !changed {
			environment = append(environment, entry)
		}
	}
	for name, value := range t.env {
		if value != nil {
			environment = append(environment, name+"="+*value)
		}
	}
	return environment
}

// Run runs the child and fails this test with its output if it did not
// pass.
func (t *IsolatedTest) Run() core.ProcessOutput {
	name := t.name
	output := t.Output()
	if output.Status == nil || !output.Status.Success() {
		panic(fmt.Sprintf("isolated %s failed\nstdout:\n%s\nstderr:\n%s",
			name, lossyText(output.Stdout), lossyText(output.Stderr)))
	}
	return output
}
