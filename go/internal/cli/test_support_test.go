// Port of mj-cli/src/test_support.rs: shared fixtures for the CLI tests.

package cli

import (
	"os"
	"os/exec"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// rerunInIsolatedChild re-runs one test in a child process with its own
// MJ_DATA_DIR and MJ_CONFIG_DIR.
//
// MJ_DATA_DIR and MJ_CONFIG_DIR are process-global, so a test that needs its
// own store cannot share the test binary with the rest of the suite. The
// parent spawns the child, waits for it and asserts that it passed; the
// child sees childVar set and runs the body.
//
// It returns true in the parent, which should return immediately, and false
// in the child, which should run the test body.
func rerunInIsolatedChild(t *testing.T, childVar string) bool {
	t.Helper()
	return rerunInIsolatedChildWith(t, childVar, nil)
}

func rerunInIsolatedChildWith(t *testing.T, childVar string, configure func(command *exec.Cmd, directory string)) bool {
	t.Helper()
	if _, ok := os.LookupEnv(childVar); ok {
		return false
	}
	// The parent only waits for its child, so isolated tests run beside
	// each other.
	t.Parallel()
	directory := t.TempDir()
	executable, err := os.Executable()
	if err != nil {
		t.Fatalf("locate the running test binary: %v", err)
	}
	command := exec.Command(executable, "-test.run=^"+t.Name()+"$", "-test.v", "-test.count=1")
	command.Env = append(os.Environ(),
		childVar+"=1",
		"MJ_DATA_DIR="+directory,
		"MJ_CONFIG_DIR="+directory,
	)
	if configure != nil {
		configure(command, directory)
	}
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		t.Fatalf("run the isolated child: %v", err)
	}
	if !output.Status.Success() {
		t.Fatalf("isolated run of %s failed\nstdout:\n%s\nstderr:\n%s", t.Name(), output.Stdout, output.Stderr)
	}
	// A pattern that matched nothing would pass without running the body.
	if !strings.Contains(string(output.Stdout), "--- PASS: "+t.Name()+" ") {
		t.Fatalf("isolated run of %s did not run it\nstdout:\n%s\nstderr:\n%s", t.Name(), output.Stdout, output.Stderr)
	}
	return true
}

// check is Rust's unwrap of a unit result.
func check(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

func pointer[T any](value T) *T { return &value }
